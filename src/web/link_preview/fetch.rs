//! 抓一页的 OpenGraph 元数据，并把缩略图落到本地缓存。
//!
//! **为什么必须走服务端。** WebUI 的 CSP 是 `img-src 'self'` / `connect-src
//! 'self'`：浏览器既取不到别人家的页面，也显示不了别人家的图。放开这两条等于
//! 让模型输出里的任意链接在用户浏览器上留下一次带 IP 的请求——远程像素追踪
//! 就是这么做的。所以抓取与图片都由 daemon 代劳，浏览器只跟本机说话。
//!
//! 出站请求一律过 [`crate::tools::net_guard`]：这是 Web 服务器里第一条对外
//! 发起的请求，地址由模型输出决定，等于把 SSRF 的靶子摆在这儿。DNS 解析结果
//! 逐个校验并钉死，重定向自己走、每跳重新校验。

use crate::tools::net_guard::resolve_public_remote_target;
use anyhow::{bail, Context, Result};
use reqwest::{Client, Url};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::time::Duration;

use super::html;

/// 抓页面的预算。卡片是锦上添花，慢了就不要，别让面板等着。
pub(super) const FETCH_TIMEOUT: Duration = Duration::from_secs(12);
/// 抓缩略图/favicon 的预算，短一些：它们失败不影响卡片成立。
const IMAGE_TIMEOUT: Duration = Duration::from_secs(8);
/// 页面读到 `</head>` 就停（见 `read_html_head`），这是兜底硬顶。
///
/// 曾经这里是 256 KB 的死上限，结果 YouTube 抓不出卡片——它的 og 标签在第
/// 705 883 字节（`<head>` 里塞满内联 JS），根本没读到就截断了（09-09 实测）。
const MAX_HTML_BYTES: usize = 2 * 1024 * 1024;
/// 缩略图上限。OG 图正常几十到几百 KB。
const MAX_IMAGE_BYTES: usize = 3 * 1024 * 1024;
const MAX_REDIRECTS: usize = 5;
const MAX_TEXT_CHARS: usize = 300;

const USER_AGENT: &str = "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/143.0.0.0 Safari/537.36";

#[derive(Debug, Clone, Default, serde::Serialize)]
pub(super) struct Preview {
    pub(super) url: String,
    pub(super) title: String,
    pub(super) description: String,
    pub(super) site: String,
    /// 本机缓存里的缩略图 id；没抓到图就是空的，前端画无图小卡。
    pub(super) image: String,
    pub(super) icon: String,
}

impl Preview {
    /// 一张卡至少要有个标题才有存在意义，否则不如留着原来的链接。
    pub(super) fn is_useful(&self) -> bool {
        !self.title.trim().is_empty()
    }
}

/// 这些失败不会自己变好：地址本身不合规、对面不是网页。其余（超时、连不上、
/// 对面临时 5xx）都当抖动，只钉很短一会儿。
const STABLE_FAILURES: &[&str] = &[
    "not an HTML page",
    "not a safe public URL",
    "non-public address",
    "too many redirects",
    "URL has no host",
    "URL has no port",
];

/// 调用方据此决定负缓存钉多久。
pub(super) fn failure_is_stable(error: &anyhow::Error) -> bool {
    let text = format!("{error:#}");
    STABLE_FAILURES.iter().any(|marker| text.contains(marker))
}

fn clip(text: &str, limit: usize) -> String {
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if text.chars().count() <= limit {
        return text;
    }
    let mut out: String = text.chars().take(limit).collect();
    out.push('…');
    out
}

/// 每一跳都重新过闸并把 DNS 结果钉死，重定向不交给 reqwest 自动跟——自动跟
/// 意味着中途某一跳可以指向内网而没人再看一眼。
async fn get(url: &Url, accept: &str, budget: Duration) -> Result<(reqwest::Response, Url)> {
    let mut current = url.clone();
    for _ in 0..=MAX_REDIRECTS {
        let resolution = resolve_public_remote_target(&current, budget).await?;
        let mut builder = Client::builder()
            .timeout(budget)
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy();
        if let Some((host, addresses)) = &resolution {
            builder = builder.resolve_to_addrs(host, addresses);
        }
        let response = builder
            .build()?
            .get(current.clone())
            .header(reqwest::header::USER_AGENT, USER_AGENT)
            .header(reqwest::header::ACCEPT, accept)
            .header(reqwest::header::ACCEPT_LANGUAGE, "zh-CN,zh;q=0.9,en;q=0.8")
            .send()
            .await?;
        if response.status().is_redirection() {
            let location = response
                .headers()
                .get(reqwest::header::LOCATION)
                .and_then(|value| value.to_str().ok())
                .context("redirect without a location")?;
            current = current.join(location).context("invalid redirect target")?;
            continue;
        }
        let response = response.error_for_status()?;
        let final_url = response.url().clone();
        return Ok((response, final_url));
    }
    bail!("too many redirects")
}

async fn read_prefix(mut response: reqwest::Response, max_bytes: usize) -> Result<Vec<u8>> {
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        let room = max_bytes.saturating_sub(body.len());
        if chunk.len() >= room {
            body.extend_from_slice(&chunk[..room]);
            break;
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

/// 读到 `</head>` 或 `<body` 就停手。
///
/// 要的东西全在 `<head>` 里，而正文可以是几 MB。固定字节数的上限两头不讨好：
/// 定小了 YouTube 那种把内联 JS 全塞进 head 的页面读不到 og 标签，定大了每张
/// 卡都要白读几 MB。按结构停才两边都对。
async fn read_html_head(mut response: reqwest::Response, max_bytes: usize) -> Result<Vec<u8>> {
    let mut body: Vec<u8> = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        // 标记可能跨在两个 chunk 的接缝上，所以从上一段的尾巴开始找。
        let scan_from = body.len().saturating_sub(HEAD_MARKER_OVERLAP);
        let room = max_bytes.saturating_sub(body.len());
        if chunk.len() >= room {
            body.extend_from_slice(&chunk[..room]);
            break;
        }
        body.extend_from_slice(&chunk);
        if let Some(end) = find_head_end(&body[scan_from..]) {
            body.truncate(scan_from + end);
            break;
        }
    }
    Ok(body)
}

/// `</head>` 7 字节、`<body` 5 字节，留 8 字节的重叠足够。
const HEAD_MARKER_OVERLAP: usize = 8;

fn find_head_end(haystack: &[u8]) -> Option<usize> {
    let lower = haystack.to_ascii_lowercase();
    let head = lower
        .windows(7)
        .position(|window| window == b"</head>")
        .map(|at| at + 7);
    let body = lower.windows(5).position(|window| window == b"<body");
    match (head, body) {
        (Some(head), Some(body)) => Some(head.min(body)),
        (Some(head), None) => Some(head),
        (None, body) => body,
    }
}

/// 只认这五种。识别不出来的一律不落盘——分不清是什么就别当图片发出去。
///
/// **不收 SVG**：它能带脚本，而这些字节最后是从本机同源发出去的。favicon 收
/// ICO 是因为半数站点的 `rel=icon` 还是 .ico，不收就只能画首字母占位。
fn sniff_image(bytes: &[u8]) -> Option<(&'static str, &'static str)> {
    if bytes.starts_with(b"\x00\x00\x01\x00") {
        return Some(("image/x-icon", "ico"));
    }
    if bytes.starts_with(b"\xff\xd8\xff") {
        return Some(("image/jpeg", "jpg"));
    }
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        return Some(("image/png", "png"));
    }
    if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        return Some(("image/gif", "gif"));
    }
    if bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP") {
        return Some(("image/webp", "webp"));
    }
    None
}

pub(super) fn image_dir(cache_dir: &Path) -> PathBuf {
    cache_dir.join("link-preview")
}

/// 缓存里的文件名同时是对外的 id：内容哈希 + 真实扩展名，天然去重，也不带
/// 任何来源信息。
pub(super) fn image_mime(asset_id: &str) -> Option<&'static str> {
    match asset_id.rsplit('.').next()? {
        "jpg" => Some("image/jpeg"),
        "png" => Some("image/png"),
        "gif" => Some("image/gif"),
        "webp" => Some("image/webp"),
        "ico" => Some("image/x-icon"),
        _ => None,
    }
}

pub(super) fn is_valid_asset_id(asset_id: &str) -> bool {
    let Some((hash, extension)) = asset_id.rsplit_once('.') else {
        return false;
    };
    hash.len() == 64
        && hash.bytes().all(|byte| byte.is_ascii_hexdigit())
        && matches!(extension, "jpg" | "png" | "gif" | "webp" | "ico")
}

async fn cache_remote_image(cache_dir: &Path, url: &Url) -> Option<String> {
    let (response, _) = get(
        url,
        "image/avif,image/webp,image/png,image/*,*/*;q=0.8",
        IMAGE_TIMEOUT,
    )
    .await
    .ok()?;
    if response
        .content_length()
        .is_some_and(|length| length > MAX_IMAGE_BYTES as u64)
    {
        return None;
    }
    let bytes = read_prefix(response, MAX_IMAGE_BYTES).await.ok()?;
    let (_, extension) = sniff_image(&bytes)?;
    let asset_id = format!("{:x}.{extension}", Sha256::digest(&bytes));
    let dir = image_dir(cache_dir);
    std::fs::create_dir_all(&dir).ok()?;
    let target = dir.join(&asset_id);
    if !target.exists() {
        // 先写临时文件再改名：半截文件被当成完整缓存读到，比没有缓存更糟。
        let temp = tempfile::NamedTempFile::new_in(&dir).ok()?;
        std::fs::write(temp.path(), &bytes).ok()?;
        temp.persist(&target).ok()?;
    }
    Some(asset_id)
}

pub(super) async fn fetch(cache_dir: &Path, url: &Url) -> Result<Preview> {
    let (response, final_url) = get(url, "text/html,application/xhtml+xml", FETCH_TIMEOUT).await?;
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_ascii_lowercase();
    if !content_type.contains("html") {
        bail!("not an HTML page")
    }
    let body = read_html_head(response, MAX_HTML_BYTES).await?;
    let meta = html::extract(&String::from_utf8_lossy(&body));

    let site = if meta.site_name.trim().is_empty() {
        final_url
            .host_str()
            .unwrap_or_default()
            .trim_start_matches("www.")
            .to_string()
    } else {
        meta.site_name.clone()
    };

    let image = match final_url.join(&meta.image) {
        Ok(image_url) if !meta.image.trim().is_empty() => {
            cache_remote_image(cache_dir, &image_url).await
        }
        _ => None,
    };
    // favicon 没写就试默认位置；试不到就没有，前端画首字母占位。
    let icon_candidate = if meta.icon.trim().is_empty() {
        final_url.join("/favicon.ico").ok()
    } else {
        final_url.join(&meta.icon).ok()
    };
    let icon = match icon_candidate {
        Some(icon_url) => cache_remote_image(cache_dir, &icon_url).await,
        None => None,
    };

    Ok(Preview {
        url: final_url.to_string(),
        title: clip(&meta.title, 120),
        description: clip(&meta.description, MAX_TEXT_CHARS),
        site: clip(&site, 60),
        image: image.unwrap_or_default(),
        icon: icon.unwrap_or_default(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn asset_ids_are_content_hashes_with_a_known_extension() {
        assert!(is_valid_asset_id(&format!("{}.png", "a".repeat(64))));
        assert!(!is_valid_asset_id(&format!("{}.svg", "a".repeat(64))));
        assert!(!is_valid_asset_id("../../etc/passwd"));
        assert!(!is_valid_asset_id("short.png"));
        assert!(!is_valid_asset_id(&format!("{}.png", "z".repeat(64))));
        assert_eq!(
            image_mime(&format!("{}.webp", "0".repeat(64))),
            Some("image/webp")
        );
    }

    #[test]
    fn only_real_image_bytes_are_cached() {
        assert!(sniff_image(b"<html><body>gotcha</body>").is_none());
        // SVG 能带脚本,而这些字节最后从本机同源发出去,永远不收。
        assert!(sniff_image(b"<svg xmlns=").is_none());
        assert_eq!(
            sniff_image(b"\x00\x00\x01\x00\x01\x00").map(|(mime, _)| mime),
            Some("image/x-icon")
        );
        assert_eq!(
            sniff_image(b"\x89PNG\r\n\x1a\n\x00").map(|(mime, _)| mime),
            Some("image/png")
        );
    }

    #[test]
    fn only_settled_failures_get_the_long_negative_ttl() {
        assert!(failure_is_stable(&anyhow::anyhow!("not an HTML page")));
        assert!(failure_is_stable(&anyhow::anyhow!(
            "host resolves to a non-public address"
        )));
        // 超时和连不上是抖动:钉久了等于把一条本来能出卡的链接按死。
        assert!(!failure_is_stable(&anyhow::anyhow!("operation timed out")));
        assert!(!failure_is_stable(&anyhow::anyhow!(
            "error sending request"
        )));
    }

    #[test]
    fn a_head_that_ends_is_cut_at_the_marker() {
        let html = b"<html><head><title>x</title></head><body>aaaaaaaa</body></html>";
        let end = find_head_end(html).unwrap();
        assert_eq!(&html[..end], b"<html><head><title>x</title></head>");
        // 没有 </head> 就退到 <body。
        let no_close = b"<html><head><title>x</title><body>tail";
        let end = find_head_end(no_close).unwrap();
        assert_eq!(&no_close[..end], b"<html><head><title>x</title>");
        assert!(find_head_end(b"<html><head><title>x</title>").is_none());
    }

    #[test]
    fn text_is_collapsed_and_clipped() {
        assert_eq!(clip("  a\n  b  ", 10), "a b");
        assert_eq!(clip(&"字".repeat(10), 4), "字字字字…");
    }
}
