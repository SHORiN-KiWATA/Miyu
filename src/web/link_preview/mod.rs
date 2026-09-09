//! 链接卡片：`/api/link-preview` 与它的缩略图出口。
//!
//! 只在 WebUI 里用。模型正文里单独占一行的链接，前端拿这里的元数据升级成一
//! 张带图卡片；这个接口失败、超时、或者页面没有可用元数据时，前端原样保留那
//! 条链接——卡片是增益，不是必需品，任何一环出问题都不该让正文变样。
//!
//! 三件事分三个文件：本文件是路由与缓存，`fetch` 是出站抓取，`html` 是元数据
//! 解析（纯函数、好测）。

mod fetch;
mod html;

use crate::web::*;
use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

/// 抓成功的结果留这么久。页面的 OG 元数据基本不动，重复请求没有意义。
const POSITIVE_TTL: Duration = Duration::from_secs(6 * 60 * 60);
/// 「这个地址就是做不出卡片」——不是 HTML、没有标题。这类结论不会自己变，
/// 记久一点。
const NO_PREVIEW_TTL: Duration = Duration::from_secs(15 * 60);
/// 网络抖动、超时、对面临时 5xx。这类**会**自己变，钉太久等于把一条本来能出
/// 卡的链接按死一刻钟（09-09 用户那条 bilibili 就是这么变成纯文本的）。
const TRANSIENT_TTL: Duration = Duration::from_secs(45);
/// 内存里最多记这么多条，满了整体清空。做 LRU 不值得——这是一张几百字节的
/// 元数据表，清空的代价就是重抓一次。
const CACHE_CAPACITY: usize = 512;

/// 抓不出卡片的两种原因，TTL 不同。
#[derive(Clone, Copy, PartialEq, Eq)]
enum Miss {
    /// 页面本身没有可用元数据。
    NoPreview,
    /// 网络层面没拿到东西，下次可能就好了。
    Transient,
}

struct CacheEntry {
    value: Result<fetch::Preview, Miss>,
    at: Instant,
}

static CACHE: LazyLock<Mutex<HashMap<String, CacheEntry>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn cached(key: &str) -> Option<Result<fetch::Preview, Miss>> {
    let cache = CACHE.lock().ok()?;
    let entry = cache.get(key)?;
    let ttl = match entry.value {
        Ok(_) => POSITIVE_TTL,
        Err(Miss::NoPreview) => NO_PREVIEW_TTL,
        Err(Miss::Transient) => TRANSIENT_TTL,
    };
    (entry.at.elapsed() < ttl).then(|| entry.value.clone())
}

fn remember(key: String, value: Result<fetch::Preview, Miss>) {
    let Ok(mut cache) = CACHE.lock() else {
        return;
    };
    if cache.len() >= CACHE_CAPACITY {
        cache.clear();
    }
    cache.insert(
        key,
        CacheEntry {
            value,
            at: Instant::now(),
        },
    );
}

#[derive(Deserialize)]
pub(in crate::web) struct LinkPreviewQuery {
    url: String,
}

/// 元数据出口。**永远返回 200**：拿不到卡片是正常结果之一，不是错误——让前端
/// 去分辨 `ok:false` 比让它 catch 一串 4xx/5xx 简单，也不会在控制台刷红。
pub(in crate::web) async fn link_preview(
    State(state): State<DaemonState>,
    headers: HeaderMap,
    Query(query): Query<LinkPreviewQuery>,
) -> std::result::Result<Response, ApiError> {
    require_auth(&headers, &state)?;
    let enabled = {
        let manager = state.manager.lock().unwrap();
        manager.config.plugins.web.enabled
    };
    if !enabled {
        return Ok(refused("link previews are disabled"));
    }
    let Ok(url) = reqwest::Url::parse(query.url.trim()) else {
        return Ok(refused("not a URL"));
    };
    if !matches!(url.scheme(), "http" | "https") {
        return Ok(refused("unsupported scheme"));
    }
    let key = url.as_str().to_string();
    if let Some(hit) = cached(&key) {
        return Ok(match hit {
            Ok(preview) => found(&preview),
            Err(_) => refused("no preview available"),
        });
    }
    let outcome = match fetch::fetch(&state.paths.cache_dir, &url).await {
        Ok(preview) if preview.is_useful() => Ok(preview),
        // 页面拿到了、但里面没有能做卡片的东西：这是个稳定结论。
        Ok(_) => Err(Miss::NoPreview),
        Err(error) if fetch::failure_is_stable(&error) => Err(Miss::NoPreview),
        Err(_) => Err(Miss::Transient),
    };
    remember(key, outcome.clone());
    Ok(match outcome {
        Ok(preview) => found(&preview),
        Err(_) => refused("no preview available"),
    })
}

fn found(preview: &fetch::Preview) -> Response {
    let mut response = Json(json!({ "ok": true, "preview": preview })).into_response();
    response.headers_mut().insert(
        CACHE_CONTROL,
        HeaderValue::from_static("private, max-age=1800"),
    );
    response
}

fn refused(reason: &str) -> Response {
    Json(json!({ "ok": false, "reason": reason })).into_response()
}

/// 缩略图出口。图片是抓回来落在本机缓存里的，浏览器只跟本机说话——CSP 的
/// `img-src 'self'` 因此一个字都不用放宽。
pub(in crate::web) async fn link_preview_image(
    State(state): State<DaemonState>,
    headers: HeaderMap,
    Path(asset_id): Path<String>,
) -> std::result::Result<Response, ApiError> {
    require_auth(&headers, &state)?;
    if !fetch::is_valid_asset_id(&asset_id) {
        return Err(ApiError::new(StatusCode::NOT_FOUND, "image not found"));
    }
    let path = fetch::image_dir(&state.paths.cache_dir).join(&asset_id);
    let Ok(bytes) = std::fs::read(&path) else {
        return Err(ApiError::new(StatusCode::NOT_FOUND, "image not found"));
    };
    let mime = fetch::image_mime(&asset_id).unwrap_or("application/octet-stream");
    let mut response = bytes.into_response();
    response
        .headers_mut()
        .insert(CONTENT_TYPE, HeaderValue::from_static(mime));
    // 内容寻址,可以放心长缓存。
    response.headers_mut().insert(
        CACHE_CONTROL,
        HeaderValue::from_static("private, max-age=604800, immutable"),
    );
    response
        .headers_mut()
        .insert(X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    Ok(response)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_hiccup_is_not_pinned_as_long_as_a_real_answer() {
        // 网络抖一下就把链接按死一刻钟，正是 09-09 那条 bilibili 变纯文本的原因。
        assert!(TRANSIENT_TTL < NO_PREVIEW_TTL);
        assert!(NO_PREVIEW_TTL < POSITIVE_TTL);
    }

    #[test]
    fn a_full_cache_is_cleared_rather_than_grown() {
        CACHE.lock().unwrap().clear();
        for index in 0..CACHE_CAPACITY + 1 {
            remember(format!("https://example.com/{index}"), Err(Miss::NoPreview));
        }
        assert!(CACHE.lock().unwrap().len() <= CACHE_CAPACITY);
        CACHE.lock().unwrap().clear();
    }
}
