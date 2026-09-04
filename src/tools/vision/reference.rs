//! 解析「哪张图」。
//!
//! 引用可以是本地路径、URL、历史里的图片 ID、或者「刚才那张」。四条路都要落到
//! 同一份字节上。
//!
//! 作用域内有配额（`MAX_SCOPED_CONTEXT_FETCHES` / `_TOTAL_BYTES` / `_VISION_CALLS`）：
//! 引用由模型给，不设上限就是让它能把整个群的图都拉一遍。

use crate::tools::vision::*;

pub(crate) const MAX_IMAGE_BYTES: usize = 10 * 1024 * 1024;

pub(crate) const MAX_SCOPED_CONTEXT_FETCHES: usize = 4;

pub(crate) const MAX_SCOPED_TOTAL_BYTES: usize = 20 * 1024 * 1024;

pub(crate) const MAX_SCOPED_VISION_CALLS: usize = 6;

#[derive(Clone, Debug)]
pub(crate) struct ResolvedContextImage {
    pub(crate) image: PlatformImageData,
    pub(crate) digest: String,
    pub(crate) cache_path: PathBuf,
}

pub(crate) struct ScopedVisionState {
    pub(crate) allowed_paths: Vec<PathBuf>,
    pub(crate) context_images: HashMap<String, PlatformContextImageRef>,
    /// 历史/当前消息里的文件与视频引用(`file_<msg>_<n>`),按需懒下载到
    /// `platform_files` 缓存后再看(09-04)。
    pub(crate) context_files: HashMap<String, PlatformContextFileRef>,
    pub(crate) platform_context: Option<Arc<dyn PlatformToolContext>>,
    pub(crate) allow_general_access: bool,
    pub(crate) resolve_lock: tokio::sync::Mutex<()>,
    pub(crate) resolved: Mutex<HashMap<String, ResolvedContextImage>>,
    pub(crate) resolved_files: Mutex<HashMap<String, PlatformFileDownload>>,
    pub(crate) content_images: Mutex<HashMap<String, ResolvedContextImage>>,
    pub(crate) analyses: Mutex<HashMap<(String, String), String>>,
    pub(crate) calls: AtomicUsize,
    pub(crate) fetches: AtomicUsize,
    pub(crate) total_bytes: AtomicUsize,
}

/// 生图参考图的引用解析器:把一个引用(本轮图片路径 / context_image_N /
/// 可信头像 URL / 普通本地路径或 URL)解析成图片字节与 MIME。
///
/// 作用域直接沿用 `vision_analyze` 的那一套:平台面只认本轮附带的图片、群
/// 聊记录里明确列出的 context_image_N、以及群查询工具返回的可信头像 URL;
/// 终端面(state=None)照旧接受任意本地路径与 http(s) URL。生图会把图片原样
/// 发到第三方 API,信任面必须和看图工具一致(08-17 决定)。
pub(crate) type ReferenceImage = (Vec<u8>, String);

pub(crate) struct ReferenceResolver {
    pub(crate) config: AppConfig,
    pub(crate) paths: MiyuPaths,
    pub(crate) state: Option<Arc<ScopedVisionState>>,
}

impl ReferenceResolver {
    pub(crate) fn unscoped(config: AppConfig, paths: MiyuPaths) -> Self {
        Self {
            config,
            paths,
            state: None,
        }
    }

    pub(crate) async fn resolve(&self, reference: &str) -> Result<ReferenceImage> {
        let reference = reference.trim();
        if reference.is_empty() {
            bail!("reference image must not be empty")
        }
        if let Some(state) = &self.state {
            if state.context_images.contains_key(reference) {
                let resolved = resolve_context_image(&self.paths, state, reference).await?;
                return Ok((resolved.image.data.to_vec(), resolved.image.mime.clone()));
            }
            if !state.allow_general_access {
                if reference.starts_with("http://") || reference.starts_with("https://") {
                    if crate::platform_types::is_trusted_avatar_url(reference) {
                        return download_reference_image(reference).await;
                    }
                    bail!("only images attached to the current platform turn can be used as a reference")
                }
                let path = expand_path(reference)
                    .canonicalize()
                    .context("failed to resolve the requested reference image")?;
                if !state.allowed_paths.iter().any(|allowed| allowed == &path) {
                    bail!("reference image is not attached to the current platform turn")
                }
                return read_reference_file(&path);
            }
        }
        let _ = &self.config;
        if reference.starts_with("http://") || reference.starts_with("https://") {
            return download_reference_image(reference).await;
        }
        read_reference_file(&expand_path(reference))
    }
}

pub(crate) fn read_reference_file(path: &Path) -> Result<ReferenceImage> {
    let metadata = std::fs::metadata(path)
        .with_context(|| format!("failed to stat reference image {}", path.display()))?;
    if !metadata.is_file() {
        bail!("reference image is not a file: {}", path.display())
    }
    if metadata.len() as usize > MAX_IMAGE_BYTES {
        bail!("reference image too large: {} bytes", metadata.len())
    }
    let mime = mime_from_path(path)?;
    let data = std::fs::read(path)
        .with_context(|| format!("failed to read reference image {}", path.display()))?;
    Ok((data, mime.to_string()))
}

pub(crate) async fn download_reference_image(url: &str) -> Result<ReferenceImage> {
    let response = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()?
        .get(url)
        .send()
        .await?;
    let status = response.status();
    if !status.is_success() {
        bail!("failed to download reference image ({status})")
    }
    let mime = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(|value| value.split(';').next().unwrap_or(value).trim().to_string())
        .filter(|value| value.starts_with("image/"))
        .unwrap_or_else(|| "image/png".to_string());
    // 边读边卡上限，而不是「整读进内存再看 len()」——后者对一个声称自己有
    // 2 GB 的响应会先老老实实缓冲完 2 GB 才判超限，护栏形同虚设。上面本地
    // 文件那条路一直是对的（先 `metadata.len()` 再读），HTTP 这条补齐。
    let data = crate::tools::http_response::read_bytes(response, MAX_IMAGE_BYTES)
        .await
        .context("reference image too large")?;
    Ok((data, mime))
}

pub(crate) async fn resolve_context_image(
    paths: &MiyuPaths,
    state: &ScopedVisionState,
    image_id: &str,
) -> Result<ResolvedContextImage> {
    if let Some(resolved) = state.resolved.lock().unwrap().get(image_id).cloned() {
        return Ok(resolved);
    }
    let _resolve_guard = state.resolve_lock.lock().await;
    if let Some(resolved) = state.resolved.lock().unwrap().get(image_id).cloned() {
        return Ok(resolved);
    }
    let source = state
        .context_images
        .get(image_id)
        .context("context image ID is not available in the current platform turn")?
        .clone();
    let context = state
        .platform_context
        .as_ref()
        .context("platform image lookup is unavailable")?;
    if state
        .fetches
        .try_update(Ordering::AcqRel, Ordering::Acquire, |count| {
            (count < MAX_SCOPED_CONTEXT_FETCHES).then_some(count + 1)
        })
        .is_err()
    {
        bail!("context image fetch limit reached for the current platform turn")
    }
    let images = match context.message_images_task(source.message_id.clone()).await {
        Ok(images) => images,
        Err(error) => {
            state.fetches.fetch_sub(1, Ordering::AcqRel);
            return Err(error).context("failed to retrieve the context image message");
        }
    };
    let image = match images.into_iter().nth(source.image_index.saturating_sub(1)) {
        Some(image) => image,
        None => {
            state.fetches.fetch_sub(1, Ordering::AcqRel);
            bail!("the requested context image is no longer available")
        }
    };
    if image.data.len() > MAX_IMAGE_BYTES {
        state.fetches.fetch_sub(1, Ordering::AcqRel);
        bail!("context image is too large: {} bytes", image.data.len())
    }
    let digest = hex::encode(Sha256::digest(&image.data));
    if let Some(existing) = state.content_images.lock().unwrap().get(&digest).cloned() {
        state
            .resolved
            .lock()
            .unwrap()
            .insert(image_id.to_string(), existing.clone());
        return Ok(existing);
    }
    let previous = state
        .total_bytes
        .fetch_add(image.data.len(), Ordering::AcqRel);
    if previous.saturating_add(image.data.len()) > MAX_SCOPED_TOTAL_BYTES {
        state
            .total_bytes
            .fetch_sub(image.data.len(), Ordering::AcqRel);
        state.fetches.fetch_sub(1, Ordering::AcqRel);
        bail!("context image byte limit reached for the current platform turn")
    }
    let cache_path = match write_image_cache_file(
        &paths.cache_dir,
        Path::new("platform_images/qq"),
        &image.mime,
        &image.data,
    ) {
        Ok(path) => path,
        Err(error) => {
            state
                .total_bytes
                .fetch_sub(image.data.len(), Ordering::AcqRel);
            state.fetches.fetch_sub(1, Ordering::AcqRel);
            return Err(error).context("failed to cache the context image");
        }
    };
    let resolved = ResolvedContextImage {
        image,
        digest,
        cache_path,
    };
    tracing::info!(
        target: "miyu::qq",
        image_id,
        message_id = %source.message_id,
        image_index = source.image_index,
        bytes = resolved.image.data.len(),
        cache_file = resolved
            .cache_path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("image"),
        "{}",
        crate::i18n::text(
            "OneBot context image prepared on demand",
            "已按需准备 OneBot 上下文图片",
        )
    );
    state
        .resolved
        .lock()
        .unwrap()
        .insert(image_id.to_string(), resolved.clone());
    state
        .content_images
        .lock()
        .unwrap()
        .insert(resolved.digest.clone(), resolved.clone());
    Ok(resolved)
}

/// 把 `file_<msg>_<n>` 引用懒下载到本地缓存,返回缓存里的路径。占用与图片
/// 同一份取件配额(`fetches`);同一 id 本回合只下一次。
pub(crate) async fn resolve_context_file(
    paths: &MiyuPaths,
    state: &ScopedVisionState,
    file_id: &str,
) -> Result<PlatformFileDownload> {
    if let Some(resolved) = state.resolved_files.lock().unwrap().get(file_id).cloned() {
        return Ok(resolved);
    }
    let _resolve_guard = state.resolve_lock.lock().await;
    if let Some(resolved) = state.resolved_files.lock().unwrap().get(file_id).cloned() {
        return Ok(resolved);
    }
    let source = state
        .context_files
        .get(file_id)
        .context("file ID is not available in the current platform turn")?
        .clone();
    let context = state
        .platform_context
        .as_ref()
        .context("platform file lookup is unavailable")?;
    if state
        .fetches
        .try_update(Ordering::AcqRel, Ordering::Acquire, |count| {
            (count < MAX_SCOPED_CONTEXT_FETCHES).then_some(count + 1)
        })
        .is_err()
    {
        bail!("context media fetch limit reached for the current platform turn")
    }
    let download = match context.fetch_platform_file_task(source.clone()).await {
        Ok(download) => download,
        Err(error) => {
            state.fetches.fetch_sub(1, Ordering::AcqRel);
            return Err(error).context("failed to download the platform file");
        }
    };
    if !is_platform_cache_path(&paths.cache_dir, &download.path) {
        state.fetches.fetch_sub(1, Ordering::AcqRel);
        bail!("the downloaded platform file landed outside the platform file cache")
    }
    tracing::info!(
        target: "miyu::qq",
        file_id,
        message_id = %source.message_id,
        bytes = download.size,
        name = %download.name,
        "{}",
        crate::i18n::text(
            "OneBot context file prepared on demand for vision",
            "已按需为看图/看视频准备 OneBot 上下文文件",
        )
    );
    state
        .resolved_files
        .lock()
        .unwrap()
        .insert(file_id.to_string(), download.clone());
    Ok(download)
}

/// 路径是否落在 `<cache>/platform_files/` 之下(canonicalize 之后比较,
/// 挡住 `..` 与符号链接)。
pub(crate) fn is_platform_cache_path(cache_dir: &Path, path: &Path) -> bool {
    let Ok(root) = cache_dir.join("platform_files").canonicalize() else {
        return false;
    };
    path.canonicalize()
        .map(|path| path.starts_with(&root))
        .unwrap_or(false)
}

pub(crate) fn image_data_url(mime: &str, data: &[u8]) -> String {
    let encoded = base64::engine::general_purpose::STANDARD.encode(data);
    format!("data:{mime};base64,{encoded}")
}

#[cfg(test)]
mod download_limit_tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    /// 一个只会往外倒数据、永远不停的「图片」服务器。
    ///
    /// 不发 `Content-Length`——正是这种不声明长度的响应最能说明问题：靠读完
    /// 再看 `len()` 的写法在这里没有任何刹车点。返回它实际吐出去了多少字节。
    async fn serve_endless_image(cap: usize) -> (String, Arc<AtomicUsize>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let sent = Arc::new(AtomicUsize::new(0));
        let counter = sent.clone();
        tokio::spawn(async move {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let mut scratch = [0u8; 1024];
            let _ = socket.read(&mut scratch).await;
            let _ = socket
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Type: image/png\r\n\r\n")
                .await;
            let block = vec![0u8; 64 * 1024];
            while counter.load(Ordering::Relaxed) < cap {
                if socket.write_all(&block).await.is_err() {
                    break;
                }
                counter.fetch_add(block.len(), Ordering::Relaxed);
            }
        });
        (format!("http://127.0.0.1:{port}/endless.png"), sent)
    }

    /// 上限要在**读的过程中**生效。服务器最多准备吐 64 MiB（上限的 6.4 倍），
    /// 我们要在远早于此处就断开。
    #[tokio::test]
    async fn download_stops_before_buffering_everything() {
        let cap = 64 * 1024 * 1024;
        let (url, sent) = serve_endless_image(cap).await;
        let result = download_reference_image(&url).await;
        assert!(result.is_err(), "无限流不该成功返回");

        let pushed = sent.load(Ordering::Relaxed);
        println!(
            "  上限 {} MiB：服务器准备吐 {} MiB，实际推出 {:.1} MiB 就被切断",
            MAX_IMAGE_BYTES / 1048576,
            cap / 1048576,
            pushed as f64 / 1048576.0
        );
        assert!(
            pushed < cap,
            "服务器把准备的 {cap} 字节全吐完了，说明没有中途刹车"
        );
        // 留一倍余量给 TCP 缓冲和分块边界：卡在 10 MiB 上限的读取端，
        // 不该让对面推出去 20 MiB 以上。
        assert!(
            pushed <= MAX_IMAGE_BYTES * 2,
            "服务器吐了 {pushed} 字节，远超上限 {MAX_IMAGE_BYTES} 的合理范围"
        );
    }
}
