//! 群文件的下载与本地暂存。
//!
//! 三重上限缺一不可：单文件字节数（`download_platform_file_capped` 边下边数，
//! 不信任 Content-Length）、目录总量、条目数。少了任何一重，一个群里刷文件就
//! 能把磁盘填满。超量时按时间淘汰，见 `ensure_platform_file_capacity`。

use crate::platforms::onebot::*;

pub(in crate::platforms::onebot) const MAX_INBOUND_FILE_BYTES: usize = 50 * 1024 * 1024;

/// 视频比普通文件宽:视觉路由那边的上限是 200MB(对齐 GLM 规格),下载链路
/// 卡在 50MB 的话,大半手机直出的视频还没到模型就被拒了(09-04)。
pub(in crate::platforms::onebot) const MAX_INBOUND_VIDEO_BYTES: usize = 200 * 1024 * 1024;

/// 按文件名扩展名给下载上限:视频 200MB,其余 50MB。
pub(in crate::platforms::onebot) fn platform_file_byte_limit(name: &str) -> usize {
    if crate::tools::vision::video_mime(name).is_some() {
        MAX_INBOUND_VIDEO_BYTES
    } else {
        MAX_INBOUND_FILE_BYTES
    }
}

/// `get_file` 一类接口给出的文件来源:三种形态都见过——http 直链、桥所在
/// 机器上的本地路径(同机部署时最省)、内嵌 base64。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::platforms::onebot) enum PlatformFileSource {
    Url(String),
    LocalPath(PathBuf),
    Bytes(Vec<u8>),
}

/// 解析 OneBot 文件接口(`get_file` / `get_*_file_url`)的返回。优先级:本地
/// 路径 > http 直链 > base64;`file://` 形式的 url 当本地路径。本地路径存不
/// 存在这里不查(同步函数),由调用方在使用前核实并退回下一形态。
pub(in crate::platforms::onebot) fn parse_platform_file_sources(
    data: &Value,
) -> Vec<PlatformFileSource> {
    let mut sources = Vec::new();
    let mut push_path = |raw: &str| {
        let path = std::path::Path::new(raw.trim());
        if path.is_absolute()
            && !sources.contains(&PlatformFileSource::LocalPath(path.to_path_buf()))
        {
            sources.push(PlatformFileSource::LocalPath(path.to_path_buf()));
        }
    };
    for key in ["file", "path"] {
        if let Some(raw) = data.get(key).and_then(Value::as_str) {
            if let Some(local) = raw.strip_prefix("file://") {
                push_path(local);
            } else {
                push_path(raw);
            }
        }
    }
    if let Some(url) = data.get("url").and_then(Value::as_str).map(str::trim) {
        if url.starts_with("http://") || url.starts_with("https://") {
            sources.push(PlatformFileSource::Url(url.to_string()));
        } else if let Some(local) = url.strip_prefix("file://") {
            push_path(local);
        }
    }
    if let Some(encoded) = data
        .get("base64")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        let encoded = encoded.strip_prefix("base64://").unwrap_or(encoded);
        if let Ok(bytes) = BASE64.decode(encoded) {
            if !bytes.is_empty() {
                sources.push(PlatformFileSource::Bytes(bytes));
            }
        }
    }
    sources
}

/// 把桥所在机器上的文件拷进缓存目录,边拷边计数,和下载一样不信任 metadata。
pub(in crate::platforms::onebot) async fn copy_platform_file_capped(
    source: &std::path::Path,
    data_dir: &std::path::Path,
    name: &str,
    max_bytes: usize,
) -> Result<PathBuf> {
    let metadata = tokio::fs::metadata(source)
        .await
        .with_context(|| format!("reading {}", source.display()))?;
    if !metadata.is_file() {
        bail!("{} is not a regular file", source.display());
    }
    if metadata.len() > max_bytes as u64 {
        bail!(
            "the file is larger than the {}MB limit",
            max_bytes / 1024 / 1024
        );
    }
    let mut input = tokio::fs::File::open(source)
        .await
        .with_context(|| format!("opening {}", source.display()))?;
    let (path, mut output) = create_platform_file(data_dir, name).await?;
    let result = async {
        let mut total = 0usize;
        let mut buffer = vec![0u8; 64 * 1024];
        loop {
            let read = input.read(&mut buffer).await?;
            if read == 0 {
                break;
            }
            total = total
                .checked_add(read)
                .context("platform file size overflow")?;
            if total > max_bytes {
                bail!(
                    "the file is larger than the {}MB limit",
                    max_bytes / 1024 / 1024
                );
            }
            output.write_all(&buffer[..read]).await?;
        }
        output.flush().await?;
        Ok::<(), anyhow::Error>(())
    }
    .await;
    if let Err(error) = result {
        drop(output);
        let _ = tokio::fs::remove_file(&path).await;
        return Err(error);
    }
    Ok(path)
}

pub(in crate::platforms::onebot) const FILE_DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(60);

pub(in crate::platforms::onebot) const PLATFORM_FILE_STORAGE_BYTES: u64 = 1024 * 1024 * 1024;

pub(in crate::platforms::onebot) const PLATFORM_FILE_STORAGE_ENTRIES: usize = 4096;

pub(in crate::platforms::onebot) const PLATFORM_FILE_TTL: Duration =
    Duration::from_secs(7 * 24 * 60 * 60);

/// QQ files are cached under `<cache>/platform_files/qq/`, never under the
/// durable data tree. Downloads are lazy: only `read_platform_file` asks for
/// them, so merely receiving a file costs no disk growth.
pub(in crate::platforms::onebot) fn platform_file_storage_root(
    base_dir: &std::path::Path,
) -> PathBuf {
    base_dir.join("platform_files").join("qq")
}

/// One-time best-effort move of the old eager-download cache from
/// `<data>/platform_files/` to `<cache>/platform_files/qq/`.
pub(in crate::platforms::onebot) async fn migrate_legacy_platform_file_cache(
    paths: &crate::paths::MiyuPaths,
) {
    let legacy = paths.data_dir.join("platform_files");
    if !legacy.exists() {
        return;
    }
    let target = platform_file_storage_root(&paths.cache_dir);
    let result = async {
        tokio::fs::create_dir_all(&target).await?;
        let mut entries = tokio::fs::read_dir(&legacy).await?;
        while let Some(entry) = entries.next_entry().await? {
            if !entry.file_type().await?.is_file() {
                continue;
            }
            let destination = target.join(entry.file_name());
            if destination.exists() {
                continue;
            }
            tokio::fs::rename(entry.path(), destination).await?;
        }
        let _ = tokio::fs::remove_dir(&legacy).await;
        Ok::<(), anyhow::Error>(())
    }
    .await;
    if let Err(error) = result {
        tracing::warn!(error = %error, legacy = %legacy.display(), "legacy platform file cache migration incomplete");
    }
}

/// 扫描配额目录:顺带清理过期文件,返回 (存量字节, 存量条数)。
pub(in crate::platforms::onebot) async fn scan_platform_file_storage(
    data_dir: &std::path::Path,
    ttl: Duration,
) -> Result<(u64, usize)> {
    let dir = platform_file_storage_root(data_dir);
    tokio::fs::create_dir_all(&dir).await?;
    let mut entries = tokio::fs::read_dir(&dir).await?;
    let mut bytes = 0_u64;
    let mut count = 0usize;
    while let Some(entry) = entries.next_entry().await? {
        let metadata = match entry.metadata().await {
            Ok(metadata) if metadata.is_file() => metadata,
            _ => continue,
        };
        let expired = metadata
            .modified()
            .ok()
            .and_then(|modified| modified.elapsed().ok())
            .is_some_and(|age| age > ttl);
        if expired {
            let _ = tokio::fs::remove_file(entry.path()).await;
            continue;
        }
        bytes = bytes
            .checked_add(metadata.len())
            .context("platform file storage size overflow")?;
        count = count.saturating_add(1);
    }
    Ok((bytes, count))
}

pub(in crate::platforms::onebot) async fn ensure_platform_file_capacity(
    data_dir: &std::path::Path,
    reserve: u64,
    max_bytes: u64,
    max_entries: usize,
    ttl: Duration,
) -> Result<()> {
    let (bytes, count) = scan_platform_file_storage(data_dir, ttl).await?;
    if count >= max_entries || bytes.saturating_add(reserve) > max_bytes {
        bail!("platform file storage quota is full");
    }
    Ok(())
}

pub(in crate::platforms::onebot) async fn download_platform_file_capped(
    client: &reqwest::Client,
    url: &str,
    data_dir: &std::path::Path,
    name: &str,
    max_bytes: usize,
    timeout: Duration,
) -> Result<PathBuf> {
    let response = client
        .get(url)
        .timeout(timeout)
        .send()
        .await
        .with_context(|| format!("requesting {url}"))?
        .error_for_status()
        .with_context(|| format!("downloading {url}"))?;
    if response
        .content_length()
        .is_some_and(|length| length > max_bytes as u64)
    {
        bail!(
            "the file is larger than the {}MB limit",
            max_bytes / 1024 / 1024
        );
    }
    let (path, mut output) = create_platform_file(data_dir, name).await?;
    let result = async {
        let mut total = 0usize;
        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.with_context(|| format!("reading {url}"))?;
            total = total
                .checked_add(chunk.len())
                .context("platform file size overflow")?;
            if total > max_bytes {
                bail!(
                    "the file is larger than the {}MB limit",
                    max_bytes / 1024 / 1024
                );
            }
            output.write_all(&chunk).await?;
        }
        output.flush().await?;
        Ok::<(), anyhow::Error>(())
    }
    .await;
    if let Err(error) = result {
        drop(output);
        let _ = tokio::fs::remove_file(&path).await;
        return Err(error);
    }
    Ok(path)
}

/// Saves inbound bytes under `<cache>/platform_files/qq/`, keeping only
/// the basename (no path traversal) and suffixing on collision.
///
/// **返回前必须 flush。** `tokio::fs::File` 的 `write_all` 只把数据拷进内部
/// 缓冲、把真正的写 `spawn_mandatory_blocking` 扔给阻塞线程池，然后立刻返回
/// Ok（tokio `fs/file.rs` 的 `poll_write`：copy_from → spawn → `Poll::Ready`）。
/// drop 不等它完成——tokio 自己的文档写着「要保证 drop 时文件立即关闭，必须先
/// 调 flush」。
///
/// 不 flush 的后果不是「慢一点」，是**调用方拿到路径时文件可能还是空的**：
/// 入站文件的路径会直接交给模型去读。线程池空闲时看不出来，繁忙时就丢数据。
/// `download_platform_file` 那条路一直有 flush，这条漏了。
pub(in crate::platforms::onebot) async fn save_platform_file(
    data_dir: &std::path::Path,
    name: &str,
    bytes: &[u8],
) -> Result<PathBuf> {
    let (path, mut output) = create_platform_file(data_dir, name).await?;
    let written = async {
        output.write_all(bytes).await?;
        output.flush().await
    }
    .await;
    if let Err(error) = written {
        drop(output);
        let _ = tokio::fs::remove_file(&path).await;
        return Err(error).context("writing the inbound platform file");
    }
    Ok(path)
}

pub(in crate::platforms::onebot) async fn create_platform_file(
    data_dir: &std::path::Path,
    name: &str,
) -> Result<(PathBuf, tokio::fs::File)> {
    let dir = platform_file_storage_root(data_dir);
    tokio::fs::create_dir_all(&dir).await?;
    let safe = sanitize_file_name(name);
    for counter in 0..=1000 {
        let path = std::path::Path::new(&safe);
        let stem = path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .unwrap_or("file");
        let file_name = match (counter, path.extension().and_then(|ext| ext.to_str())) {
            (0, _) => safe.clone(),
            (_, Some(ext)) => format!("{stem}-{counter}.{ext}"),
            (_, None) => format!("{stem}-{counter}"),
        };
        let candidate = dir.join(file_name);
        let output = match tokio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&candidate)
            .await
        {
            Ok(output) => output,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error).context("creating the inbound platform file"),
        };
        return Ok((candidate, output));
    }
    bail!("too many files with the same name")
}

pub(in crate::platforms::onebot) fn sanitize_file_name(name: &str) -> String {
    let base = name
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or("file")
        .replace(['\0', '\n', '\r'], "");
    let trimmed = base.trim();
    if trimmed.is_empty() || trimmed == "." || trimmed == ".." {
        return "file".to_string();
    }
    trimmed.chars().take(120).collect()
}
