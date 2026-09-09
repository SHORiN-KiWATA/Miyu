//! 下载、校验与落盘。
//!
//! 图片 URL 来自搜索引擎，等于来自任意站点，所以 SSRF 防护是硬要求：
//! `is_safe_remote_url` + `resolve_public_remote_target` 两层（实现在
//! `crate::tools::net_guard`，链接卡片那条出站线共用同一份），
//! **解析之后再查 IP**——只看域名挡不住指向内网的 DNS 记录。
//!
//! 解码前先读头部拿尺寸（`detect_image_dimensions`）：一张声称 60000×60000 的
//! PNG 解出来就是几十 GB，等解码器报错已经晚了。

use crate::tools::web_images::*;

pub(in crate::tools::web_images) const MAX_IMAGE_PIXELS: u64 = 16_000_000;

pub(in crate::tools::web_images) const IMAGE_DECODER_MAX_ALLOC: u64 = 64 * 1024 * 1024;

pub(in crate::tools::web_images) const MAX_DOWNLOAD_BYTES: usize = 50 * 1024 * 1024;

pub(in crate::tools::web_images) struct StoredImage {
    pub(in crate::tools::web_images) candidate: ImageCandidate,
    pub(in crate::tools::web_images) local_path: PathBuf,
    pub(in crate::tools::web_images) mime_type: String,
    pub(in crate::tools::web_images) size_bytes: usize,
    pub(in crate::tools::web_images) sha256: String,
    pub(in crate::tools::web_images) used_thumbnail: bool,
    pub(in crate::tools::web_images) vision: VisionScreening,
}

pub(in crate::tools::web_images) struct CallTempDir {
    pub(in crate::tools::web_images) inner: Option<tempfile::TempDir>,
}

impl CallTempDir {
    pub(in crate::tools::web_images) fn new(cache_dir: &Path) -> Result<Self> {
        Ok(Self {
            inner: Some(
                tempfile::Builder::new()
                    .prefix(".webimg-call-")
                    .tempdir_in(cache_dir)
                    .with_context(|| {
                        format!("failed to create image temp dir in {}", cache_dir.display())
                    })?,
            ),
        })
    }

    pub(in crate::tools::web_images) fn path(&self) -> &Path {
        self.inner.as_ref().expect("temp dir is available").path()
    }
}

impl Drop for CallTempDir {
    fn drop(&mut self) {
        if let Some(dir) = self.inner.take() {
            let path = dir.path().to_path_buf();
            if let Err(error) = dir.close() {
                tracing::warn!(
                    error = %error,
                    path = %path.display(),
                    "failed to clean web image call temp directory"
                );
            }
        }
    }
}

pub(in crate::tools::web_images) fn configured_max_download_bytes(max_download_mb: f64) -> usize {
    let max_download_mb = if max_download_mb.is_nan() {
        0.1
    } else {
        max_download_mb.clamp(0.1, 50.0)
    };
    (max_download_mb * 1024.0 * 1024.0) as usize
}

pub(in crate::tools::web_images) struct DownloadResult {
    pub(in crate::tools::web_images) images: Vec<StoredImage>,
    pub(in crate::tools::web_images) rejected_by_vision: usize,
}

pub(in crate::tools::web_images) async fn download_and_store_images(
    config: &AppConfig,
    paths: &MiyuPaths,
    cache_dir: &Path,
    query: &str,
    candidates: Vec<ImageCandidate>,
    count: usize,
    max_bytes: usize,
    progress: ToolProgress,
) -> Result<DownloadResult> {
    tokio::fs::create_dir_all(cache_dir)
        .await
        .with_context(|| format!("failed to create {}", cache_dir.display()))?;
    let call_temp_dir = CallTempDir::new(cache_dir)?;
    let mut completed = Vec::new();
    let mut download_error = None;
    let probe_limit = image_download_probe_limit(count);
    let download_timeout = Duration::from_secs(config.plugins.web_images.timeout_seconds.max(5));
    let downloads =
        candidates
            .into_iter()
            .take(probe_limit)
            .enumerate()
            .map(|(index, candidate)| {
                let temp_dir = call_temp_dir.path().to_path_buf();
                async move {
                    (
                        index,
                        download_candidate(
                            &temp_dir,
                            index,
                            candidate,
                            max_bytes,
                            download_timeout,
                        )
                        .await,
                    )
                }
            });
    let mut downloads =
        futures_util::stream::iter(downloads).buffer_unordered(probe_limit.clamp(1, 4));
    while let Some((index, result)) = downloads.next().await {
        progress.report(format!(
            "{} {}/{}",
            t("downloading images", "正在下载图片"),
            completed.len() + 1,
            probe_limit
        ));
        let result = match result {
            Ok(result) => result,
            Err(err) => {
                download_error.get_or_insert(err);
                continue;
            }
        };
        let Some(mut item) = result else {
            continue;
        };
        item.vision = VisionScreening::not_requested();
        completed.push((index, item));
    }
    if let Some(err) = download_error {
        return Err(err);
    }
    let mut downloaded = dedupe_downloaded(completed);
    if downloaded.is_empty() {
        bail!("image search found candidates, but no image could be downloaded")
    }
    if vision_screening_available(config) {
        progress.report(t("reviewing images", "正在批量审核图片"));
        screen_images_with_vision(config, paths, query, &mut downloaded).await;
    }
    let (mut stored, rejected_by_vision) = select_images(query, downloaded, count);
    if stored.is_empty() {
        bail!("image search candidates were unavailable or rejected by safety review")
    }
    for item in &mut stored {
        publish_image(cache_dir, item).await?;
    }
    progress.report(format!(
        "{} {}/{}",
        t("accepted images", "已通过图片"),
        stored.len(),
        count
    ));
    Ok(DownloadResult {
        images: stored,
        rejected_by_vision,
    })
}

pub(in crate::tools::web_images) fn dedupe_downloaded(
    mut completed: Vec<(usize, StoredImage)>,
) -> Vec<StoredImage> {
    completed.sort_by_key(|(index, _)| *index);
    let mut seen_hashes = HashSet::new();
    completed
        .into_iter()
        .filter_map(|(_, item)| seen_hashes.insert(item.sha256.clone()).then_some(item))
        .collect()
}

pub(in crate::tools::web_images) fn select_images(
    query: &str,
    downloaded: Vec<StoredImage>,
    count: usize,
) -> (Vec<StoredImage>, usize) {
    let before_filter = downloaded.len();
    let mut stored = Vec::new();
    for item in downloaded {
        if item.vision.accepted && item.vision.safe {
            stored.push(item);
        }
    }
    let rejected_by_vision = before_filter.saturating_sub(stored.len());
    stored.sort_by(|left, right| {
        right
            .vision
            .relevance
            .cmp(&left.vision.relevance)
            .then_with(|| right.vision.quality.cmp(&left.vision.quality))
            .then_with(|| {
                score_candidate(query, &right.candidate)
                    .partial_cmp(&score_candidate(query, &left.candidate))
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
    });
    stored.truncate(count);
    (stored, rejected_by_vision)
}

pub(in crate::tools::web_images) async fn download_candidate(
    temp_dir: &Path,
    candidate_index: usize,
    mut candidate: ImageCandidate,
    max_bytes: usize,
    timeout: Duration,
) -> Result<Option<StoredImage>> {
    let urls =
        if candidate.thumbnail_url.is_empty() || candidate.thumbnail_url == candidate.image_url {
            vec![(candidate.image_url.clone(), false)]
        } else {
            vec![
                (candidate.image_url.clone(), false),
                (candidate.thumbnail_url.clone(), true),
            ]
        };
    for (url, used_thumbnail) in urls {
        let deadline = Instant::now() + timeout;
        let Ok((bytes, final_url, content_type)) =
            download_image_bytes(&url, &candidate.page_url, max_bytes, deadline).await
        else {
            continue;
        };
        let decode_permit = IMAGE_DECODE_PERMITS
            .clone()
            .acquire_owned()
            .await
            .context("web image decode limiter closed")?;
        let validated = match tokio::task::spawn_blocking(move || {
            let _decode_permit = decode_permit;
            validate_downloaded_image(bytes, content_type, final_url)
        })
        .await
        {
            Ok(Some(validated)) => validated,
            Ok(None) => continue,
            Err(error) => {
                tracing::warn!(error = %error, "web image decoder task failed");
                continue;
            }
        };
        let ValidatedImage {
            bytes,
            mime_type,
            width,
            height,
            sha256,
        } = validated;
        let ext = extension_for_mime(&mime_type);
        let local_path = temp_dir.join(format!("candidate-{candidate_index}-{sha256}{ext}"));
        if let Err(error) = write_temp_file(&local_path, &bytes).await {
            tracing::warn!(
                error = %error,
                path = %local_path.display(),
                "failed to stage web image candidate"
            );
            continue;
        }
        if width > 0 && height > 0 {
            candidate.width = width;
            candidate.height = height;
        }
        return Ok(Some(StoredImage {
            candidate,
            local_path,
            mime_type,
            size_bytes: bytes.len(),
            sha256,
            used_thumbnail,
            vision: VisionScreening::not_requested(),
        }));
    }
    Ok(None)
}

pub(in crate::tools::web_images) struct ValidatedImage {
    pub(in crate::tools::web_images) bytes: Vec<u8>,
    pub(in crate::tools::web_images) mime_type: String,
    pub(in crate::tools::web_images) width: u32,
    pub(in crate::tools::web_images) height: u32,
    pub(in crate::tools::web_images) sha256: String,
}

pub(in crate::tools::web_images) fn validate_downloaded_image(
    bytes: Vec<u8>,
    content_type: String,
    final_url: String,
) -> Option<ValidatedImage> {
    let mime_type = detect_image_mime(&bytes, &content_type, &final_url)?;
    let (width, height) = detect_image_dimensions(&bytes, &mime_type);
    if !image_dimensions_allowed(width, height) {
        return None;
    }
    let mut reader = image::ImageReader::new(Cursor::new(&bytes))
        .with_guessed_format()
        .ok()?;
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(width);
    limits.max_image_height = Some(height);
    limits.max_alloc = Some(IMAGE_DECODER_MAX_ALLOC);
    reader.limits(limits);
    let decoded = reader.decode().ok()?;
    if decoded.dimensions() != (width, height) {
        return None;
    }
    let sha256 = hex::encode(Sha256::digest(&bytes));
    Some(ValidatedImage {
        bytes,
        mime_type,
        width,
        height,
        sha256,
    })
}

pub(in crate::tools::web_images) fn image_dimensions_allowed(width: u32, height: u32) -> bool {
    width > 0
        && height > 0
        && u64::from(width).saturating_mul(u64::from(height)) <= MAX_IMAGE_PIXELS
}

pub(in crate::tools::web_images) async fn write_temp_file(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut file = match tokio::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .await
    {
        Ok(file) => file,
        Err(err) => {
            return Err(err).with_context(|| format!("failed to create {}", path.display()))
        }
    };
    let write_result = async {
        file.write_all(bytes).await?;
        file.flush().await?;
        file.sync_all().await
    }
    .await;
    if let Err(err) = write_result {
        drop(file);
        if let Err(cleanup_error) = tokio::fs::remove_file(path).await {
            if cleanup_error.kind() != std::io::ErrorKind::NotFound {
                tracing::warn!(
                    error = %cleanup_error,
                    path = %path.display(),
                    "failed to remove incomplete web image temp file"
                );
            }
        }
        return Err(err).with_context(|| format!("failed to write {}", path.display()));
    }
    Ok(())
}

pub(in crate::tools::web_images) async fn publish_image(
    cache_dir: &Path,
    item: &mut StoredImage,
) -> Result<()> {
    let final_path = cache_dir.join(format!(
        "webimg-{}{}",
        item.sha256,
        extension_for_mime(&item.mime_type)
    ));
    let _publish_guard = CACHE_PUBLISH_LOCK.lock().await;
    let source = item.local_path.clone();
    let expected_hash = item.sha256.clone();
    let expected_size = item.size_bytes;
    let cache_dir = cache_dir.to_path_buf();
    let publish_path = final_path.clone();
    tokio::task::spawn_blocking(move || {
        publish_cache_file(
            &source,
            &publish_path,
            &cache_dir,
            &expected_hash,
            expected_size,
        )
    })
    .await
    .context("web image cache publish task failed")??;
    item.local_path = final_path;
    Ok(())
}

pub(in crate::tools::web_images) fn publish_cache_file(
    source: &Path,
    final_path: &Path,
    cache_dir: &Path,
    expected_hash: &str,
    expected_size: usize,
) -> Result<()> {
    for _ in 0..8 {
        match std::fs::hard_link(source, final_path) {
            Ok(()) => {
                // The hard link is already committed and cannot be rolled back safely. A failed
                // directory sync is therefore reported, but must not remove the shared cache.
                if let Err(error) = std::fs::File::open(cache_dir).and_then(|file| file.sync_all())
                {
                    tracing::warn!(
                        error = %error,
                        path = %cache_dir.display(),
                        "web image cache published but directory sync failed"
                    );
                }
                return Ok(());
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                let metadata = match std::fs::symlink_metadata(final_path) {
                    Ok(metadata) => metadata,
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                    Err(error) => {
                        return Err(error).with_context(|| {
                            format!("failed to inspect existing {}", final_path.display())
                        })
                    }
                };
                if metadata.file_type().is_dir() {
                    bail!(
                        "web image cache path is a directory: {}",
                        final_path.display()
                    )
                }
                if metadata.file_type().is_symlink() {
                    remove_invalid_cache_entry(final_path)?;
                    continue;
                }
                if !metadata.file_type().is_file() {
                    bail!(
                        "web image cache path is not a regular file: {}",
                        final_path.display()
                    )
                }
                if valid_cached_file(final_path, expected_hash, expected_size)? {
                    return Ok(());
                }
                remove_invalid_cache_entry(final_path)?;
            }
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("failed to publish {}", final_path.display()))
            }
        }
    }
    bail!("could not publish web image without replacing a concurrent cache entry")
}

pub(in crate::tools::web_images) fn remove_invalid_cache_entry(path: &Path) -> Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => {
            Err(error).with_context(|| format!("failed to remove invalid {}", path.display()))
        }
    }
}

pub(in crate::tools::web_images) fn valid_cached_file(
    path: &Path,
    expected_hash: &str,
    expected_size: usize,
) -> Result<bool> {
    if expected_size == 0 || expected_size > MAX_DOWNLOAD_BYTES {
        return Ok(false);
    }
    let metadata = std::fs::metadata(path)?;
    if metadata.len() != expected_size as u64 {
        return Ok(false);
    }
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hex::encode(hasher.finalize()) == expected_hash)
}

pub(in crate::tools::web_images) async fn download_image_bytes(
    url: &str,
    referer: &str,
    max_bytes: usize,
    deadline: Instant,
) -> Result<(Vec<u8>, String, String)> {
    let mut current = Url::parse(url).context("invalid image URL")?;
    for _ in 0..=8 {
        let remaining = remaining_timeout(deadline)?;
        let resolution = resolve_public_remote_target(&current, remaining).await?;
        let mut builder = Client::builder()
            .timeout(remaining_timeout(deadline)?)
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy();
        if let Some((host, addresses)) = &resolution {
            builder = builder.resolve_to_addrs(host, addresses);
        }
        let client = builder.build()?;
        let response = client
            .get(current.clone())
            .headers(image_headers(referer))
            .send()
            .await?;
        if response.status().is_redirection() {
            let location = response
                .headers()
                .get(reqwest::header::LOCATION)
                .and_then(|value| value.to_str().ok())
                .context("image redirect has no valid location")?;
            current = current
                .join(location)
                .context("invalid image redirect URL")?;
            continue;
        }
        let response = response.error_for_status()?;
        if response.content_length().unwrap_or(0) > max_bytes as u64 {
            bail!("image exceeds size limit")
        }
        let final_url = response.url().to_string();
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_string();
        let mut bytes = Vec::with_capacity(
            response
                .content_length()
                .unwrap_or(64 * 1024)
                .min(max_bytes as u64) as usize,
        );
        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk?;
            if bytes.len().saturating_add(chunk.len()) > max_bytes {
                bail!("image exceeds size limit")
            }
            bytes.extend_from_slice(&chunk);
        }
        if bytes.is_empty() {
            bail!("image is empty")
        }
        return Ok((bytes, final_url, content_type));
    }
    bail!("too many image redirects")
}

pub(in crate::tools::web_images) fn remaining_timeout(deadline: Instant) -> Result<Duration> {
    deadline
        .checked_duration_since(Instant::now())
        .filter(|duration| !duration.is_zero())
        .context("image download timed out")
}

pub(in crate::tools::web_images) fn image_headers(referer: &str) -> reqwest::header::HeaderMap {
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert(reqwest::header::USER_AGENT, USER_AGENT.parse().unwrap());
    headers.insert(
        reqwest::header::ACCEPT,
        "text/html,application/json,text/javascript,image/avif,image/webp,image/apng,image/*,*/*;q=0.8"
            .parse()
            .unwrap(),
    );
    headers.insert(
        reqwest::header::ACCEPT_LANGUAGE,
        "zh-CN,zh;q=0.9,en;q=0.8".parse().unwrap(),
    );
    if !referer.is_empty() {
        if let Ok(value) = referer.parse() {
            headers.insert(reqwest::header::REFERER, value);
        }
    }
    headers
}

pub(in crate::tools::web_images) fn detect_image_mime(
    bytes: &[u8],
    _content_type: &str,
    _url: &str,
) -> Option<String> {
    if bytes.starts_with(b"\xff\xd8\xff") {
        return Some("image/jpeg".to_string());
    }
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        return Some("image/png".to_string());
    }
    if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        return Some("image/gif".to_string());
    }
    if bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP") {
        return Some("image/webp".to_string());
    }
    if bytes.starts_with(b"BM") {
        return Some("image/bmp".to_string());
    }
    None
}

pub(in crate::tools::web_images) fn detect_image_dimensions(
    bytes: &[u8],
    mime_type: &str,
) -> (u32, u32) {
    match mime_type {
        "image/png" if bytes.len() >= 24 && bytes.starts_with(b"\x89PNG\r\n\x1a\n") => (
            u32::from_be_bytes(bytes[16..20].try_into().unwrap()),
            u32::from_be_bytes(bytes[20..24].try_into().unwrap()),
        ),
        "image/gif"
            if bytes.len() >= 10
                && (bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a")) =>
        {
            (
                u16::from_le_bytes(bytes[6..8].try_into().unwrap()) as u32,
                u16::from_le_bytes(bytes[8..10].try_into().unwrap()) as u32,
            )
        }
        "image/bmp" if bytes.len() >= 26 && bytes.starts_with(b"BM") => (
            i32::from_le_bytes(bytes[18..22].try_into().unwrap()).unsigned_abs(),
            i32::from_le_bytes(bytes[22..26].try_into().unwrap()).unsigned_abs(),
        ),
        "image/webp"
            if bytes.len() >= 30
                && bytes.starts_with(b"RIFF")
                && bytes.get(8..12) == Some(b"WEBP") =>
        {
            detect_webp_dimensions(bytes)
        }
        "image/jpeg" | "image/jpg" if bytes.starts_with(b"\xff\xd8") => {
            detect_jpeg_dimensions(bytes)
        }
        _ => (0, 0),
    }
}

pub(in crate::tools::web_images) fn detect_webp_dimensions(bytes: &[u8]) -> (u32, u32) {
    match bytes.get(12..16) {
        Some(b"VP8X") if bytes.len() >= 30 => {
            let width = 1 + u32::from_le_bytes([bytes[24], bytes[25], bytes[26], 0]);
            let height = 1 + u32::from_le_bytes([bytes[27], bytes[28], bytes[29], 0]);
            (width, height)
        }
        Some(b"VP8 ") if bytes.len() >= 30 => {
            let width = u16::from_le_bytes([bytes[26], bytes[27]]) as u32 & 0x3fff;
            let height = u16::from_le_bytes([bytes[28], bytes[29]]) as u32 & 0x3fff;
            (width, height)
        }
        Some(b"VP8L") if bytes.len() >= 25 => {
            let width = 1 + (((bytes[22] as u32 & 0x3f) << 8) | bytes[21] as u32);
            let height = 1
                + (((bytes[24] as u32 & 0x0f) << 10)
                    | ((bytes[23] as u32) << 2)
                    | ((bytes[22] as u32 & 0xc0) >> 6));
            (width, height)
        }
        _ => (0, 0),
    }
}

pub(in crate::tools::web_images) fn detect_jpeg_dimensions(bytes: &[u8]) -> (u32, u32) {
    let mut index = 2;
    while index + 9 < bytes.len() {
        if bytes[index] != 0xff {
            index += 1;
            continue;
        }
        while index < bytes.len() && bytes[index] == 0xff {
            index += 1;
        }
        if index >= bytes.len() {
            break;
        }
        let marker = bytes[index];
        index += 1;
        if matches!(marker, 0xd8 | 0xd9 | 0x01) || (0xd0..=0xd7).contains(&marker) {
            continue;
        }
        if marker == 0xda || index + 2 > bytes.len() {
            break;
        }
        let length = u16::from_be_bytes([bytes[index], bytes[index + 1]]) as usize;
        if length < 2 || index + length > bytes.len() {
            break;
        }
        if matches!(
            marker,
            0xc0 | 0xc1
                | 0xc2
                | 0xc3
                | 0xc5
                | 0xc6
                | 0xc7
                | 0xc9
                | 0xca
                | 0xcb
                | 0xcd
                | 0xce
                | 0xcf
        ) && index + 7 <= bytes.len()
        {
            let height = u16::from_be_bytes([bytes[index + 3], bytes[index + 4]]) as u32;
            let width = u16::from_be_bytes([bytes[index + 5], bytes[index + 6]]) as u32;
            return (width, height);
        }
        index += length;
    }
    (0, 0)
}

pub(in crate::tools::web_images) fn extension_for_mime(mime_type: &str) -> &'static str {
    match mime_type {
        "image/png" => ".png",
        "image/gif" => ".gif",
        "image/webp" => ".webp",
        "image/bmp" => ".bmp",
        _ => ".jpg",
    }
}

pub(in crate::tools::web_images) fn format_bytes(size: usize) -> String {
    let mut value = size as f64;
    for unit in ["B", "KB", "MB", "GB"] {
        if value < 1024.0 || unit == "GB" {
            return if unit == "B" {
                format!("{size} B")
            } else {
                format!("{value:.1} {unit}")
            };
        }
        value /= 1024.0;
    }
    format!("{value:.1} GB")
}
