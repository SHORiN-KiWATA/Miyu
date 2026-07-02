use super::{vision, ToolProgress, ToolRegistry, ToolSpec};
use crate::config::{AppConfig, ProviderConfig, VisionPluginConfig};
use crate::default_models::{OPENCODE_DEFAULT_VISION_MODEL, OPENCODE_PROVIDER_ID};
use crate::i18n::text as t;
use crate::llm::{ChatMessage, OpenAiCompatibleClient};
use crate::paths::MiyuPaths;
use anyhow::{bail, Context, Result};
use reqwest::Client;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

const USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/143.0.0.0 Safari/537.36";

#[derive(Debug, Clone)]
struct ImageCandidate {
    title: String,
    page_url: String,
    image_url: String,
    thumbnail_url: String,
    source: String,
    width: u32,
    height: u32,
    search_description: String,
}

struct StoredImage {
    candidate: ImageCandidate,
    local_path: PathBuf,
    mime_type: String,
    size_bytes: usize,
    sha256: String,
    used_thumbnail: bool,
    vision: VisionScreening,
}

#[derive(Debug, Clone)]
struct VisionScreening {
    status: String,
    accepted: bool,
    description: String,
    reason: String,
    provider_id: String,
    model: String,
    error: String,
}

impl VisionScreening {
    fn not_requested() -> Self {
        Self {
            status: "not_requested".to_string(),
            accepted: true,
            description: String::new(),
            reason: String::new(),
            provider_id: String::new(),
            model: String::new(),
            error: String::new(),
        }
    }

    fn failed(error: impl Into<String>, provider: Option<&ProviderConfig>) -> Self {
        Self {
            status: "failed".to_string(),
            accepted: true,
            description: String::new(),
            reason: String::new(),
            provider_id: provider.map(|item| item.id.clone()).unwrap_or_default(),
            model: provider
                .map(|item| item.default_model.clone())
                .unwrap_or_default(),
            error: error.into(),
        }
    }
}

pub fn register(
    registry: &mut ToolRegistry,
    config: AppConfig,
    paths: MiyuPaths,
    allow_download: bool,
) {
    registry.register(ToolSpec::new_with_progress(
        "search_web_images",
        t(
            "Search web images with DuckDuckGo and Bing fallback. In normal mode it can download selected images to the local cache and optionally preview them with chafa. In read-only mode it only returns remote image metadata.",
            "搜索网络图片，使用 DuckDuckGo，失败或不足时回退 Bing。普通模式可下载选中图片到本地缓存并可用 chafa 预览；只读模式只返回远程图片元数据。",
        ),
        json!({
            "type": "object",
            "properties": {
                "query": { "type": "string", "description": t("Image search query.", "图片搜索关键词。") },
                "count": { "type": "integer", "description": t("Required. Exact number of images to return. Match the user's requested quantity: one/a/an/一张/一幅 means 1; a few/几张 means 3; several/多张 means 5 unless the user gives another number. Do not use the configured maximum as the default.", "必填。最终返回图片的精确数量。必须匹配用户要求的数量：一张/一幅/one/a/an 填 1；几张填 3；多张填 5，除非用户给了其他数字。不要把配置上限当默认值。") },
                "preview": { "type": "boolean", "description": t("Download and preview images with chafa when terminal image printing is enabled.", "在终端图片打印启用时，下载并用 chafa 预览图片。") },
                "preview_count": { "type": "integer", "description": t("Maximum images to preview with chafa.", "最多用 chafa 预览几张图片。") },
                "safe_search": { "type": "boolean", "description": t("Enable safe image search. Defaults to plugin config.", "启用安全搜图。默认使用插件配置。") }
            },
            "required": ["query", "count"],
            "additionalProperties": false
        }),
        move |args, progress| {
            let config = config.clone();
            let paths = paths.clone();
            async move { search_web_images(args, config, paths, allow_download, progress).await }
        },
    ));
}

async fn search_web_images(
    args: Value,
    config: AppConfig,
    paths: MiyuPaths,
    allow_download: bool,
    progress: ToolProgress,
) -> Result<String> {
    let plugin = &config.plugins.web_images;
    if !plugin.enabled {
        bail!("web image search plugin is disabled")
    }
    let query = args
        .get("query")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim();
    if query.is_empty() {
        bail!("query is required")
    }
    let Some(count) = args.get("count").and_then(Value::as_u64) else {
        bail!("count is required; choose the number of images from the user's request")
    };
    let count = count.clamp(1, plugin.max_results.clamp(1, 10) as u64) as usize;
    let safe_search = args
        .get("safe_search")
        .and_then(Value::as_bool)
        .unwrap_or(plugin.safe_search);
    let preview = allow_download
        && args
            .get("preview")
            .and_then(Value::as_bool)
            .unwrap_or(plugin.auto_preview);
    let preview_count = args
        .get("preview_count")
        .and_then(Value::as_u64)
        .unwrap_or(count as u64)
        .clamp(0, count.min(5) as u64) as usize;
    let client = Client::builder()
        .timeout(Duration::from_secs(plugin.timeout_seconds.max(5)))
        .redirect(reqwest::redirect::Policy::limited(8))
        .build()?;
    progress.report(t("searching image candidates", "正在搜索图片候选"));
    let candidates = search_images(&client, query, count, safe_search).await?;
    if !allow_download {
        return Ok(json!({
            "success": !candidates.is_empty(),
            "query": query,
            "count": candidates.len().min(count),
            "mode": "metadata_only",
            "images": candidates.into_iter().take(count).map(candidate_json).collect::<Vec<_>>(),
        })
        .to_string());
    }
    let cache_dir = paths.pictures_dir.join("web-images");
    let download_result = download_and_store_images(
        &config,
        &paths,
        &client,
        &cache_dir,
        query,
        candidates,
        count,
        (plugin.max_download_mb.max(0.1) * 1024.0 * 1024.0) as usize,
        progress.clone(),
    )
    .await?;
    let stored = download_result.images;
    let mut print_errors = Vec::new();
    let should_print = preview && config.plugins.print_image.enabled && preview_count > 0;
    if should_print {
        progress.report("__external_output__");
        for item in stored.iter().take(preview_count) {
            if let Err(err) = vision::print_image_file(
                &item.local_path,
                vision::configured_print_size(&config.plugins.print_image),
            )
            .await
            {
                print_errors.push(format!("{}: {err}", item.local_path.display()));
            }
        }
    }
    Ok(json!({
        "success": !stored.is_empty(),
        "query": query,
        "count": stored.len(),
        "result_role": "downloaded_image_candidates",
        "vision_screening": if vision_screening_available(&config) { "enabled" } else { "unavailable" },
        "description_policy": "vision.description is produced by the configured vision model after download; search_description is only search-engine metadata. Prefer vision.description when explaining whether an image matches the request.",
        "rejected_by_vision": download_result.rejected_by_vision,
        "cache_dir": cache_dir,
        "printed": should_print && print_errors.is_empty() && !stored.is_empty(),
        "print_errors": print_errors,
        "images": stored.into_iter().map(stored_json).collect::<Vec<_>>(),
        "assistant_instruction": if should_print {
            "The searched images have been downloaded and previewed in the terminal when possible. In your final response, include the local_path values for reusable images. Do not call print_image again for already printed images unless the user asks."
        } else {
            "The searched images have been downloaded to local_path. In your final response, include useful local_path and page_url values. Call print_image only if the user explicitly asks to render or preview them."
        }
    })
    .to_string())
}

struct DownloadResult {
    images: Vec<StoredImage>,
    rejected_by_vision: usize,
}

async fn search_images(
    client: &Client,
    query: &str,
    count: usize,
    safe_search: bool,
) -> Result<Vec<ImageCandidate>> {
    let limit = image_candidate_pool_limit(count);
    let mut candidates = search_ddg_images(client, query, limit, safe_search)
        .await
        .unwrap_or_default();
    if candidates.len() < count {
        let fallback = search_bing_images(client, query, limit, safe_search)
            .await
            .unwrap_or_default();
        candidates.extend(fallback);
    }
    let mut candidates = dedupe_candidates(candidates);
    rank_candidates(query, &mut candidates);
    if candidates.is_empty() {
        bail!("image search returned no results")
    }
    Ok(candidates.into_iter().take(limit).collect())
}

async fn search_ddg_images(
    client: &Client,
    query: &str,
    limit: usize,
    safe_search: bool,
) -> Result<Vec<ImageCandidate>> {
    let page_url = format!(
        "https://duckduckgo.com/?q={}&iax=images&ia=images",
        urlencoding::encode(query)
    );
    let html = client
        .get("https://duckduckgo.com/")
        .query(&[("q", query), ("iax", "images"), ("ia", "images")])
        .headers(image_headers(""))
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    let vqd = extract_ddg_vqd(&html).context("DuckDuckGo image page did not return vqd")?;
    let response = client
        .get("https://duckduckgo.com/i.js")
        .query(&[
            ("q", query),
            ("o", "json"),
            ("p", if safe_search { "1" } else { "-1" }),
            ("s", "0"),
            ("u", "bing"),
            ("f", ",,,"),
            ("l", "us-en"),
            ("vqd", vqd.as_str()),
        ])
        .headers(image_headers(&page_url))
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    parse_ddg_results(&response, limit)
}

fn extract_ddg_vqd(html: &str) -> Option<String> {
    for marker in ["vqd=\"", "vqd='", "vqd:\"", "vqd: '"] {
        if let Some(start) = html.find(marker) {
            let rest = &html[start + marker.len()..];
            let value: String = rest
                .chars()
                .take_while(|ch| ch.is_ascii_digit() || *ch == '-')
                .collect();
            if !value.is_empty() {
                return Some(value);
            }
        }
    }
    if let Some(start) = html.find("\"vqd\":\"") {
        let rest = &html[start + "\"vqd\":\"".len()..];
        let value: String = rest
            .chars()
            .take_while(|ch| ch.is_ascii_digit() || *ch == '-')
            .collect();
        if !value.is_empty() {
            return Some(value);
        }
    }
    None
}

fn parse_ddg_results(text: &str, limit: usize) -> Result<Vec<ImageCandidate>> {
    let data: Value = serde_json::from_str(text)?;
    let results = data
        .get("results")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut candidates = Vec::new();
    for item in results.into_iter().take(limit) {
        if let Some(candidate) = build_candidate(
            item.get("title")
                .and_then(Value::as_str)
                .unwrap_or_default(),
            item.get("url").and_then(Value::as_str).unwrap_or_default(),
            item.get("image")
                .and_then(Value::as_str)
                .unwrap_or_default(),
            item.get("thumbnail")
                .and_then(Value::as_str)
                .unwrap_or_default(),
            "DuckDuckGo Images",
            item.get("width").and_then(Value::as_u64).unwrap_or(0),
            item.get("height").and_then(Value::as_u64).unwrap_or(0),
            "",
        ) {
            candidates.push(candidate);
        }
    }
    Ok(candidates)
}

async fn search_bing_images(
    client: &Client,
    query: &str,
    limit: usize,
    safe_search: bool,
) -> Result<Vec<ImageCandidate>> {
    let mut request = client
        .get("https://www.bing.com/images/search")
        .query(&[("q", query), ("first", "1")])
        .headers(image_headers(""));
    if safe_search {
        request = request.query(&[("safeSearch", "Strict")]);
    }
    let html = request.send().await?.error_for_status()?.text().await?;
    Ok(parse_bing_results(&html, limit))
}

fn parse_bing_results(html: &str, limit: usize) -> Vec<ImageCandidate> {
    let mut candidates = Vec::new();
    let mut rest = html;
    while let Some(pos) = rest.find("<a") {
        rest = &rest[pos..];
        let Some(iusc_pos) = rest.find("class=\"iusc\"") else {
            if rest.len() <= 2 {
                break;
            }
            rest = &rest[2..];
            continue;
        };
        rest = &rest[iusc_pos..];
        let Some(m_pos) = rest.find("m=\"") else {
            rest = &rest[1..];
            continue;
        };
        let start = m_pos + 3;
        let Some(end) = rest[start..].find('"') else {
            break;
        };
        let raw = html_unescape(&rest[start..start + end]);
        if let Ok(data) = serde_json::from_str::<Value>(&raw) {
            if let Some(candidate) = build_candidate(
                data.get("t")
                    .or_else(|| data.get("desc"))
                    .and_then(Value::as_str)
                    .unwrap_or_default(),
                data.get("purl").and_then(Value::as_str).unwrap_or_default(),
                data.get("murl").and_then(Value::as_str).unwrap_or_default(),
                data.get("turl").and_then(Value::as_str).unwrap_or_default(),
                "Bing Images",
                data.get("w")
                    .or_else(|| data.get("expw"))
                    .and_then(Value::as_u64)
                    .unwrap_or(0),
                data.get("h")
                    .or_else(|| data.get("exph"))
                    .and_then(Value::as_u64)
                    .unwrap_or(0),
                data.get("desc").and_then(Value::as_str).unwrap_or_default(),
            ) {
                candidates.push(candidate);
            }
        }
        if candidates.len() >= limit {
            break;
        }
        rest = &rest[start + end..];
    }
    candidates
}

#[allow(clippy::too_many_arguments)]
fn build_candidate(
    title: &str,
    page_url: &str,
    image_url: &str,
    thumbnail_url: &str,
    source: &str,
    width: u64,
    height: u64,
    extra_description: &str,
) -> Option<ImageCandidate> {
    let image_url = clean_url(image_url);
    if !image_url.starts_with("http://") && !image_url.starts_with("https://") {
        return None;
    }
    let title = clean_text(title, 180);
    let page_url = clean_url(page_url);
    let thumbnail_url = clean_url(thumbnail_url);
    let mut description_parts = vec![title.clone(), clean_text(extra_description, 180)];
    if let Some(host) = host_from_url(&page_url) {
        description_parts.push(format!("来源页面: {host}"));
    }
    let search_description = clean_text(
        &description_parts
            .into_iter()
            .filter(|part| !part.is_empty())
            .collect::<Vec<_>>()
            .join("；"),
        420,
    );
    Some(ImageCandidate {
        title,
        page_url,
        image_url,
        thumbnail_url,
        source: source.to_string(),
        width: width.min(u32::MAX as u64) as u32,
        height: height.min(u32::MAX as u64) as u32,
        search_description,
    })
}

#[allow(clippy::too_many_arguments)]
async fn download_and_store_images(
    config: &AppConfig,
    paths: &MiyuPaths,
    client: &Client,
    cache_dir: &Path,
    query: &str,
    candidates: Vec<ImageCandidate>,
    count: usize,
    max_bytes: usize,
    progress: ToolProgress,
) -> Result<DownloadResult> {
    std::fs::create_dir_all(cache_dir)
        .with_context(|| format!("failed to create {}", cache_dir.display()))?;
    let mut stored = Vec::new();
    let mut seen_hashes = HashSet::new();
    let mut rejected_by_vision = 0;
    for candidate in candidates
        .into_iter()
        .take(image_download_probe_limit(count))
    {
        if stored.len() >= count {
            break;
        }
        progress.report(format!(
            "{} {}/{}",
            t("downloading images", "正在下载图片"),
            stored.len() + 1,
            count
        ));
        let Some(mut item) = download_candidate(client, cache_dir, candidate, max_bytes).await?
        else {
            continue;
        };
        if !seen_hashes.insert(item.sha256.clone()) {
            continue;
        }
        if vision_screening_available(config) {
            progress.report(format!(
                "{} {}/{}",
                t("reviewing images", "正在审核图片"),
                stored.len() + 1,
                count
            ));
        }
        item.vision = screen_image_with_vision(config, paths, query, &item).await;
        if item.vision.status == "success" && !item.vision.accepted {
            rejected_by_vision += 1;
            progress.report(format!(
                "{} {}",
                t("image rejected by review", "图片审核已拒绝"),
                rejected_by_vision
            ));
            continue;
        }
        stored.push(item);
        progress.report(format!(
            "{} {}/{}",
            t("accepted images", "已通过图片"),
            stored.len(),
            count
        ));
    }
    if stored.is_empty() {
        bail!("image search found candidates, but no image could be downloaded")
    }
    Ok(DownloadResult {
        images: stored,
        rejected_by_vision,
    })
}

async fn download_candidate(
    client: &Client,
    cache_dir: &Path,
    mut candidate: ImageCandidate,
    max_bytes: usize,
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
        let Ok((bytes, final_url, content_type)) =
            download_image_bytes(client, &url, max_bytes).await
        else {
            continue;
        };
        let Some(mime_type) = detect_image_mime(&bytes, &content_type, &final_url) else {
            continue;
        };
        let (width, height) = detect_image_dimensions(&bytes, &mime_type);
        if width > 0 && height > 0 {
            candidate.width = width;
            candidate.height = height;
        }
        let sha256 = hex::encode(Sha256::digest(&bytes));
        let ext = extension_for_mime(&mime_type);
        let local_path = cache_dir.join(format!("webimg-{sha256}{ext}"));
        if !local_path.exists() {
            std::fs::write(&local_path, &bytes)
                .with_context(|| format!("failed to write {}", local_path.display()))?;
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

async fn download_image_bytes(
    client: &Client,
    url: &str,
    max_bytes: usize,
) -> Result<(Vec<u8>, String, String)> {
    let response = client
        .get(url)
        .headers(image_headers(""))
        .send()
        .await?
        .error_for_status()?;
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
    let bytes = response.bytes().await?.to_vec();
    if bytes.is_empty() {
        bail!("image is empty")
    }
    if bytes.len() > max_bytes {
        bail!("image exceeds size limit")
    }
    Ok((bytes, final_url, content_type))
}

fn image_headers(referer: &str) -> reqwest::header::HeaderMap {
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
        headers.insert(reqwest::header::REFERER, referer.parse().unwrap());
    }
    headers
}

fn detect_image_mime(bytes: &[u8], content_type: &str, url: &str) -> Option<String> {
    let header = content_type
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    if matches!(
        header.as_str(),
        "image/jpeg" | "image/jpg" | "image/png" | "image/gif" | "image/webp" | "image/bmp"
    ) {
        return Some(header);
    }
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
    let path = url.to_ascii_lowercase();
    if path.ends_with(".jpg") || path.ends_with(".jpeg") {
        return Some("image/jpeg".to_string());
    }
    if path.ends_with(".png") {
        return Some("image/png".to_string());
    }
    if path.ends_with(".gif") {
        return Some("image/gif".to_string());
    }
    if path.ends_with(".webp") {
        return Some("image/webp".to_string());
    }
    if path.ends_with(".bmp") {
        return Some("image/bmp".to_string());
    }
    None
}

fn detect_image_dimensions(bytes: &[u8], mime_type: &str) -> (u32, u32) {
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

fn detect_webp_dimensions(bytes: &[u8]) -> (u32, u32) {
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

fn detect_jpeg_dimensions(bytes: &[u8]) -> (u32, u32) {
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

fn rank_candidates(query: &str, candidates: &mut [ImageCandidate]) {
    candidates.sort_by(|left, right| {
        score_candidate(query, right)
            .partial_cmp(&score_candidate(query, left))
            .unwrap_or(std::cmp::Ordering::Equal)
    });
}

fn score_candidate(query: &str, candidate: &ImageCandidate) -> f32 {
    let metadata = format!(
        "{} {} {}",
        candidate.title, candidate.page_url, candidate.image_url
    )
    .to_ascii_lowercase();
    let mut score = 0.0;
    for term in image_query_terms(query) {
        if candidate.title.to_ascii_lowercase().contains(&term) {
            score += 24.0;
        } else if metadata.contains(&term) {
            score += 10.0;
        }
    }
    let short = candidate.width.min(candidate.height);
    let area = candidate.width.saturating_mul(candidate.height);
    score += if short >= 900 {
        28.0
    } else if short >= 600 {
        24.0
    } else if short >= 300 {
        18.0
    } else if short >= 100 {
        4.0
    } else {
        -8.0
    };
    if area >= 1_000_000 {
        score += 7.0;
    }
    let noisy = [
        "thumb",
        "thumbnail",
        "sprite",
        "placeholder",
        "banner",
        "advert",
        "favicon",
    ];
    if noisy.iter().any(|term| metadata.contains(term)) {
        score -= 8.0;
    }
    if metadata.contains("avatar")
        && !query.contains("头像")
        && !query.to_ascii_lowercase().contains("avatar")
    {
        score -= 8.0;
    }
    score
}

fn image_query_terms(query: &str) -> Vec<String> {
    let generic = [
        "图片",
        "照片",
        "高清",
        "壁纸",
        "photo",
        "image",
        "images",
        "picture",
        "wallpaper",
        "hd",
        "4k",
    ];
    query
        .split(|ch: char| ch.is_whitespace() || ch.is_ascii_punctuation())
        .map(|term| term.trim().to_ascii_lowercase())
        .filter(|term| term.len() >= 2 && !generic.contains(&term.as_str()))
        .collect()
}

fn dedupe_candidates(candidates: Vec<ImageCandidate>) -> Vec<ImageCandidate> {
    let mut seen = HashSet::new();
    let mut deduped = Vec::new();
    for candidate in candidates {
        let key = candidate
            .image_url
            .split('?')
            .next()
            .unwrap_or(&candidate.image_url)
            .to_ascii_lowercase();
        if seen.insert(key) {
            deduped.push(candidate);
        }
    }
    deduped
}

fn image_candidate_pool_limit(count: usize) -> usize {
    count.max((count * 4).max(count + 8).min(30))
}

fn image_download_probe_limit(count: usize) -> usize {
    count.max((count * 4).max(count + 6).min(16))
}

fn candidate_json(candidate: ImageCandidate) -> Value {
    json!({
        "title": candidate.title,
        "page_url": candidate.page_url,
        "image_url": candidate.image_url,
        "thumbnail_url": candidate.thumbnail_url,
        "source": candidate.source,
        "width": candidate.width,
        "height": candidate.height,
        "search_description": candidate.search_description,
    })
}

fn stored_json(item: StoredImage) -> Value {
    json!({
        "title": item.candidate.title,
        "page_url": item.candidate.page_url,
        "image_url": item.candidate.image_url,
        "thumbnail_url": item.candidate.thumbnail_url,
        "source": item.candidate.source,
        "local_path": item.local_path,
        "mime_type": item.mime_type,
        "width": item.candidate.width,
        "height": item.candidate.height,
        "size_bytes": item.size_bytes,
        "size_human": format_bytes(item.size_bytes),
        "sha256": item.sha256,
        "used_thumbnail": item.used_thumbnail,
        "search_description": item.candidate.search_description,
        "vision": {
            "status": item.vision.status,
            "accepted": item.vision.accepted,
            "description": item.vision.description,
            "reason": item.vision.reason,
            "provider_id": item.vision.provider_id,
            "model": item.vision.model,
            "error": item.vision.error,
        },
    })
}

async fn screen_image_with_vision(
    config: &AppConfig,
    paths: &MiyuPaths,
    query: &str,
    item: &StoredImage,
) -> VisionScreening {
    if !vision_screening_available(config) {
        return VisionScreening::not_requested();
    }
    let provider = match vision_provider(config, &config.plugins.vision) {
        Ok(provider) => provider,
        Err(err) => return VisionScreening::failed(err.to_string(), None),
    };
    let client = match OpenAiCompatibleClient::new(&provider, config, paths) {
        Ok(client) => client,
        Err(err) => return VisionScreening::failed(err.to_string(), Some(&provider)),
    };
    let image_url = match local_image_data_url(&item.local_path, item.size_bytes) {
        Ok(value) => value,
        Err(err) => return VisionScreening::failed(err.to_string(), Some(&provider)),
    };
    let prompt = image_screening_prompt(query, &item.candidate);
    let result = client
        .chat_stream(
            vec![
                ChatMessage::system(
                    "你是图片搜索结果筛选器。只根据图片实际内容判断是否匹配用户想看的图片。",
                ),
                ChatMessage::user_with_image(prompt, image_url),
            ],
            Vec::new(),
            |_| Ok(()),
        )
        .await;
    match result {
        Ok(result) => parse_vision_screening(&result.content, &provider),
        Err(err) => VisionScreening::failed(err.to_string(), Some(&provider)),
    }
}

fn vision_screening_available(config: &AppConfig) -> bool {
    config.plugins.web_images.vision_screening_enabled && config.plugins.vision.enabled
}

fn vision_provider(config: &AppConfig, vision: &VisionPluginConfig) -> Result<ProviderConfig> {
    let provider_id = vision.vision_provider_id.trim();
    let model = vision.vision_model.trim();
    let mut provider = if !provider_id.is_empty() {
        config.provider(Some(provider_id))?.clone()
    } else {
        config.provider(Some(OPENCODE_PROVIDER_ID))?.clone()
    };
    provider.default_model = if !model.is_empty() {
        model.to_string()
    } else if provider_id.is_empty() {
        OPENCODE_DEFAULT_VISION_MODEL.to_string()
    } else {
        provider.default_model.clone()
    };
    if provider.default_model.trim().is_empty() {
        bail!("vision provider has no active model")
    }
    if !provider
        .models
        .iter()
        .any(|item| item == &provider.default_model)
    {
        provider.models.push(provider.default_model.clone());
    }
    Ok(provider)
}

fn image_screening_prompt(query: &str, candidate: &ImageCandidate) -> String {
    format!(
        "用户想看的图片：{query}\n搜索结果标题：{}\n搜索结果来源：{}\n搜索结果描述：{}\n\n请判断这张已下载图片是否适合作为用户要看的图片。只输出 JSON，不要 Markdown，不要解释到 JSON 外面。格式：{{\"accepted\": true, \"description\": \"用中文客观描述图片内容\", \"reason\": \"接受或拒绝原因\"}}",
        candidate.title, candidate.page_url, candidate.search_description
    )
}

fn parse_vision_screening(text: &str, provider: &ProviderConfig) -> VisionScreening {
    let raw = text.trim();
    let json_text = raw
        .find('{')
        .and_then(|start| raw.rfind('}').map(|end| &raw[start..=end]));
    if let Some(json_text) = json_text {
        if let Ok(data) = serde_json::from_str::<Value>(json_text) {
            if data.is_object() {
                return VisionScreening {
                    status: "success".to_string(),
                    accepted: parse_boolish(data.get("accepted")).unwrap_or(true),
                    description: data
                        .get("description")
                        .or_else(|| data.get("caption"))
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .trim()
                        .to_string(),
                    reason: data
                        .get("reason")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .trim()
                        .to_string(),
                    provider_id: provider.id.clone(),
                    model: provider.default_model.clone(),
                    error: String::new(),
                };
            }
        }
    }
    VisionScreening {
        status: "success".to_string(),
        accepted: true,
        description: clean_text(raw, 1600),
        reason: "vision model did not return JSON; kept image".to_string(),
        provider_id: provider.id.clone(),
        model: provider.default_model.clone(),
        error: String::new(),
    }
}

fn parse_boolish(value: Option<&Value>) -> Option<bool> {
    match value? {
        Value::Bool(value) => Some(*value),
        Value::String(value) => {
            let lower = value.trim().to_ascii_lowercase();
            Some(!matches!(
                lower.as_str(),
                "false" | "0" | "no" | "reject" | "rejected" | "不" | "否" | "拒绝"
            ))
        }
        Value::Number(value) => Some(value.as_i64().unwrap_or(1) != 0),
        _ => None,
    }
}

fn local_image_data_url(path: &Path, size_bytes: usize) -> Result<String> {
    if size_bytes > 10 * 1024 * 1024 {
        bail!("image too large for vision screening: {size_bytes} bytes")
    }
    let bytes =
        std::fs::read(path).with_context(|| format!("failed to read {}", path.display()))?;
    let mime = detect_image_mime(&bytes, "", &path.display().to_string())
        .context("failed to detect image mime for vision screening")?;
    let encoded = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, bytes);
    Ok(format!("data:{mime};base64,{encoded}"))
}

fn clean_url(value: &str) -> String {
    html_unescape(value.trim())
}

fn clean_text(value: &str, max_chars: usize) -> String {
    let text = html_unescape(value)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if text.chars().count() <= max_chars {
        text
    } else {
        format!("{}...", text.chars().take(max_chars).collect::<String>())
    }
}

fn html_unescape(value: &str) -> String {
    value
        .replace("&amp;", "&")
        .replace("&quot;", "\"")
        .replace("&#x27;", "'")
        .replace("&#39;", "'")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
}

fn host_from_url(url: &str) -> Option<String> {
    let rest = url.split_once("://")?.1;
    Some(rest.split('/').next()?.to_ascii_lowercase())
}

fn extension_for_mime(mime_type: &str) -> &'static str {
    match mime_type {
        "image/png" => ".png",
        "image/gif" => ".gif",
        "image/webp" => ".webp",
        "image/bmp" => ".bmp",
        _ => ".jpg",
    }
}

fn format_bytes(size: usize) -> String {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_ddg_vqd() {
        assert_eq!(
            extract_ddg_vqd("foo vqd=\"123-456\" bar"),
            Some("123-456".to_string())
        );
        assert_eq!(extract_ddg_vqd("foo"), None);
    }

    #[test]
    fn detects_png_dimensions() {
        let mut bytes = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR".to_vec();
        bytes.extend_from_slice(&32u32.to_be_bytes());
        bytes.extend_from_slice(&16u32.to_be_bytes());
        assert_eq!(detect_image_dimensions(&bytes, "image/png"), (32, 16));
    }
}
