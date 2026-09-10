//! 静态资源与用户素材。
//!
//! 两类东西走同一条出口但信任面完全不同：前端资源（HTML/CSS/JS/KaTeX 字体）
//! 是编译进二进制的，人格素材（头像、背景图）是用户上传的。后者要校验哈希、
//! 限制命名空间、拒绝路径穿越——`resolve_persona_asset_path` 那一串检查每一
//! 条都对应一种能拿到任意文件的写法。

use crate::web::*;

#[derive(Serialize)]
pub(in crate::web) struct SafeImageAsset {
    pub(in crate::web) id: String,
    pub(in crate::web) url: String,
    pub(in crate::web) mime: String,
    pub(in crate::web) width: u32,
    pub(in crate::web) height: u32,
    pub(in crate::web) alt: String,
    pub(in crate::web) hide_caption: bool,
}

#[derive(Clone, Serialize)]
pub(in crate::web) struct SafeArtifactAsset {
    pub(in crate::web) id: String,
    pub(in crate::web) url: String,
    pub(in crate::web) name: String,
    pub(in crate::web) mime: String,
    pub(in crate::web) kind: String,
    pub(in crate::web) type_label: String,
    pub(in crate::web) size: u64,
    pub(in crate::web) updated_at: String,
}

pub(in crate::web) fn embedded_asset(
    headers: &HeaderMap,
    content: &'static [u8],
    content_type: &'static str,
) -> Response {
    if headers
        .get(axum::http::header::IF_NONE_MATCH)
        .is_some_and(|value| value == build_etag())
    {
        let mut response = StatusCode::NOT_MODIFIED.into_response();
        response
            .headers_mut()
            .insert(axum::http::header::ETAG, build_etag().clone());
        return response;
    }
    let mut response = finish_asset_response(content.into_response(), content_type);
    response
        .headers_mut()
        .insert(axum::http::header::ETAG, build_etag().clone());
    response
}

/// artifact 的 iframe 拿的是**不透明源**,浏览器把它当成一个公网页面;它去读本机
/// 的 `/vendor/` 就成了「公网访问内网」,被 Private Network Access 拦下,报
/// `Permission was denied for this request to access the loopback address space`。
/// CSP 里把来源写得再对也没用——这道拦截在 CSP 之前。放行要两样:这组响应头,
/// 以及能应付浏览器为此强制发起的 OPTIONS 预检(见 `vendor_preflight`)。
///
/// **这组头只配挂在 /vendor/ 这类公开第三方库上**(Apache/MIT 的 JS、字体,不含
/// 任何用户数据)。`Allow-Origin: *` 是对全网开放读取,任何带数据的接口都不能用。
fn allow_sandboxed_frames(headers: &mut HeaderMap) {
    headers.insert(ACCESS_CONTROL_ALLOW_ORIGIN, HeaderValue::from_static("*"));
    headers.insert(
        HeaderName::from_static("access-control-allow-private-network"),
        HeaderValue::from_static("true"),
    );
    headers.insert(
        ACCESS_CONTROL_ALLOW_METHODS,
        HeaderValue::from_static("GET, OPTIONS"),
    );
    headers.insert(ACCESS_CONTROL_MAX_AGE, HeaderValue::from_static("600"));
}

pub(in crate::web) async fn vendor_preflight() -> Response {
    let mut response = StatusCode::NO_CONTENT.into_response();
    allow_sandboxed_frames(response.headers_mut());
    response
}

/// `/vendor/` 下的静态库。与 `embedded_asset` 的差别只有那组放行头。
pub(in crate::web) fn vendor_asset(
    headers: &HeaderMap,
    content: &'static [u8],
    content_type: &'static str,
) -> Response {
    let mut response = embedded_asset(headers, content, content_type);
    allow_sandboxed_frames(response.headers_mut());
    response
}

/// 仓库里存的就是 gzip 后的字节,直接原样发出去、让浏览器自己解——服务端不碰
/// 压缩解压,省下的是二进制体积(ECharts 1096KB → 359KB)。
///
/// 万一对方不收 gzip(现实中几乎不存在,但 `Accept-Encoding` 是可以不带的),
/// 现场解一次再发,总好过甩给它一坨解不开的字节。
pub(in crate::web) fn vendor_gzip_asset(
    headers: &HeaderMap,
    gzipped: &'static [u8],
    content_type: &'static str,
) -> Response {
    if !accepts_gzip(headers) {
        if let Some(plain) = inflate(gzipped) {
            let mut response = finish_asset_response(plain.into_response(), content_type);
            response
                .headers_mut()
                .insert(axum::http::header::ETAG, build_etag().clone());
            allow_sandboxed_frames(response.headers_mut());
            mark_encoding_varies(response.headers_mut());
            return response;
        }
    }
    let mut response = vendor_asset(headers, gzipped, content_type);
    if response.status() == StatusCode::OK {
        response
            .headers_mut()
            .insert(CONTENT_ENCODING, HeaderValue::from_static("gzip"));
    }
    mark_encoding_varies(response.headers_mut());
    response
}

/// 同一个 URL 会按 `Accept-Encoding` 发出两种字节(压缩的和现解的),而这里所有
/// 资源共用一个按构建号算的 ETag——不声明 Vary,中间的缓存就可能把压缩版回给
/// 一个不收压缩的客户端。现实中浏览器全都收 gzip,这条是给代理和 curl 兜底的。
fn mark_encoding_varies(headers: &mut HeaderMap) {
    headers.insert(
        axum::http::header::VARY,
        HeaderValue::from_static("accept-encoding"),
    );
}

fn accepts_gzip(headers: &HeaderMap) -> bool {
    headers
        .get(ACCEPT_ENCODING)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| {
            value.split(',').any(|part| {
                part.split(';')
                    .next()
                    .unwrap_or_default()
                    .trim()
                    .eq_ignore_ascii_case("gzip")
            })
        })
}

fn inflate(gzipped: &[u8]) -> Option<Vec<u8>> {
    use std::io::Read;
    let mut out = Vec::new();
    flate2::read::GzDecoder::new(gzipped)
        .read_to_end(&mut out)
        .ok()?;
    Some(out)
}

pub(in crate::web) async fn index_asset(headers: HeaderMap) -> Response {
    // Version the asset references so browsers and intermediaries can never
    // serve a stale app.js/styles.css after an upgrade.
    static VERSIONED_INDEX: std::sync::LazyLock<String> = std::sync::LazyLock::new(|| {
        let mut html = INDEX_HTML
            .replace(
                "href=\"/styles.css\"",
                concat!("href=\"/styles.css?v=", env!("MIYU_BUILD_ID"), "\""),
            )
            .replace(
                "src=\"/app.js\"",
                concat!("src=\"/app.js?v=", env!("MIYU_BUILD_ID"), "\""),
            )
            .replace(
                "src=\"/commands.js\"",
                concat!("src=\"/commands.js?v=", env!("MIYU_BUILD_ID"), "\""),
            )
            .replace(
                "src=\"/lightbox.js\"",
                concat!("src=\"/lightbox.js?v=", env!("MIYU_BUILD_ID"), "\""),
            )
            .replace(
                "src=\"/preview.js\"",
                concat!("src=\"/preview.js?v=", env!("MIYU_BUILD_ID"), "\""),
            )
            .replace(
                "src=\"/linkcards.js\"",
                concat!("src=\"/linkcards.js?v=", env!("MIYU_BUILD_ID"), "\""),
            )
            .replace(
                "src=\"/todos.js\"",
                concat!("src=\"/todos.js?v=", env!("MIYU_BUILD_ID"), "\""),
            )
            .replace(
                "src=\"/shared.js\"",
                concat!("src=\"/shared.js?v=", env!("MIYU_BUILD_ID"), "\""),
            )
            .replace(
                "src=\"/highlight.js\"",
                concat!("src=\"/highlight.js?v=", env!("MIYU_BUILD_ID"), "\""),
            )
            .replace(
                "href=\"/vendor/katex/katex.min.css\"",
                concat!(
                    "href=\"/vendor/katex/katex.min.css?v=",
                    env!("MIYU_BUILD_ID"),
                    "\""
                ),
            )
            .replace(
                "src=\"/vendor/katex/katex.min.js\"",
                concat!(
                    "src=\"/vendor/katex/katex.min.js?v=",
                    env!("MIYU_BUILD_ID"),
                    "\""
                ),
            )
            .replace(
                "src=\"/vendor/prism/prism.min.js\"",
                concat!(
                    "src=\"/vendor/prism/prism.min.js?v=",
                    env!("MIYU_BUILD_ID"),
                    "\""
                ),
            );
        for (name, _) in DASH_SCRIPTS {
            html = html.replace(
                &format!("src=\"/dash/{name}\""),
                &format!("src=\"/dash/{name}?v={}\"", env!("MIYU_BUILD_ID")),
            );
        }
        html
    });
    embedded_asset(
        &headers,
        VERSIONED_INDEX.as_bytes(),
        "text/html; charset=utf-8",
    )
}

pub(in crate::web) async fn styles_asset(headers: HeaderMap) -> Response {
    embedded_asset(&headers, STYLES_CSS.as_bytes(), "text/css; charset=utf-8")
}

pub(in crate::web) async fn app_asset(headers: HeaderMap) -> Response {
    embedded_asset(
        &headers,
        APP_JS.as_bytes(),
        "application/javascript; charset=utf-8",
    )
}

pub(in crate::web) async fn commands_js_asset(headers: HeaderMap) -> Response {
    embedded_asset(
        &headers,
        COMMANDS_JS.as_bytes(),
        "application/javascript; charset=utf-8",
    )
}

pub(in crate::web) async fn lightbox_js_asset(headers: HeaderMap) -> Response {
    embedded_asset(
        &headers,
        LIGHTBOX_JS.as_bytes(),
        "application/javascript; charset=utf-8",
    )
}

pub(in crate::web) async fn preview_js_asset(headers: HeaderMap) -> Response {
    embedded_asset(
        &headers,
        PREVIEW_JS.as_bytes(),
        "application/javascript; charset=utf-8",
    )
}

pub(in crate::web) async fn linkcards_js_asset(headers: HeaderMap) -> Response {
    embedded_asset(
        &headers,
        LINKCARDS_JS.as_bytes(),
        "application/javascript; charset=utf-8",
    )
}

pub(in crate::web) async fn todos_js_asset(headers: HeaderMap) -> Response {
    embedded_asset(
        &headers,
        TODOS_JS.as_bytes(),
        "application/javascript; charset=utf-8",
    )
}

pub(in crate::web) async fn shared_js_asset(headers: HeaderMap) -> Response {
    embedded_asset(
        &headers,
        SHARED_JS.as_bytes(),
        "application/javascript; charset=utf-8",
    )
}

/// 插件 dashboard 脚本表:`/dash/<name>` 一张表一个 handler。新增面板只在这里
/// 加一行(外加 index.html 的 script 标签),不再逐个写 handler + route。
/// `dashboards.js` 是共享层,得排在各面板之前加载,顺序由 index.html 决定。
static DASH_SCRIPTS: &[(&str, &str)] = &[
    ("dashboards.js", include_str!("../../web/dashboards.js")),
    ("dash-memory.js", include_str!("../../web/dash-memory.js")),
    ("dash-kb.js", include_str!("../../web/dash-kb.js")),
    ("dash-memes.js", include_str!("../../web/dash-memes.js")),
    ("dash-qq.js", include_str!("../../web/dash-qq.js")),
    ("dash-groups.js", include_str!("../../web/dash-groups.js")),
    (
        "dash-affection.js",
        include_str!("../../web/dash-affection.js"),
    ),
    ("dash-scripts.js", include_str!("../../web/dash-scripts.js")),
    ("dash-ledger.js", include_str!("../../web/dash-ledger.js")),
    (
        "dash-ledger-manage.js",
        include_str!("../../web/dash-ledger-manage.js"),
    ),
    ("dash-sponsor.js", include_str!("../../web/dash-sponsor.js")),
    // 设置页(09-04 重做):字段模式表 + 渲染层,走同一条静态表。
    (
        "settings-schema.js",
        include_str!("../../web/settings-schema.js"),
    ),
    ("settings.js", include_str!("../../web/settings.js")),
];

pub(in crate::web) async fn dash_script_asset(
    headers: HeaderMap,
    Path(name): Path<String>,
) -> Response {
    match DASH_SCRIPTS.iter().find(|(entry, _)| *entry == name) {
        Some((_, content)) => embedded_asset(
            &headers,
            content.as_bytes(),
            "application/javascript; charset=utf-8",
        ),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

pub(in crate::web) async fn logo_asset(headers: HeaderMap) -> Response {
    embedded_asset(&headers, MIYU_LOGO, "image/png")
}

pub(in crate::web) async fn highlight_js_asset(headers: HeaderMap) -> Response {
    embedded_asset(
        &headers,
        HIGHLIGHT_JS.as_bytes(),
        "application/javascript; charset=utf-8",
    )
}

// 这几条都走 `vendor_asset`:除了主界面自己在用,artifact 的沙箱 iframe 也要取得到
// (见 `allow_sandboxed_frames`)。
pub(in crate::web) async fn prism_js_asset(headers: HeaderMap) -> Response {
    vendor_asset(
        &headers,
        PRISM_JS.as_bytes(),
        "text/javascript; charset=utf-8",
    )
}

pub(in crate::web) async fn katex_js_asset(headers: HeaderMap) -> Response {
    vendor_asset(
        &headers,
        KATEX_JS.as_bytes(),
        "text/javascript; charset=utf-8",
    )
}

pub(in crate::web) async fn katex_css_asset(headers: HeaderMap) -> Response {
    vendor_asset(&headers, KATEX_CSS.as_bytes(), "text/css; charset=utf-8")
}

pub(in crate::web) async fn katex_font_asset(
    headers: HeaderMap,
    Path(font): Path<String>,
) -> Response {
    match KATEX_FONTS.iter().find(|(name, _)| *name == font) {
        Some((_, bytes)) => vendor_asset(&headers, bytes, "font/woff2"),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

pub(in crate::web) async fn echarts_js_asset(headers: HeaderMap) -> Response {
    vendor_gzip_asset(&headers, ECHARTS_JS_GZ, "text/javascript; charset=utf-8")
}

pub(in crate::web) async fn wallpaper_asset(headers: HeaderMap) -> Response {
    embedded_asset(&headers, MIYU_WALLPAPER, "image/png")
}

pub(in crate::web) async fn upload_persona_asset(
    State(state): State<DaemonState>,
    headers: HeaderMap,
    body: Bytes,
) -> std::result::Result<Json<Value>, ApiError> {
    require_mutation(&headers, &state)?;
    if body.is_empty() {
        return Err(ApiError::new(StatusCode::BAD_REQUEST, "image is empty"));
    }
    if body.len() > PERSONA_ASSET_LIMIT {
        return Err(ApiError::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            "persona image is too large",
        ));
    }
    let format = image::guess_format(&body)
        .map_err(|_| ApiError::new(StatusCode::BAD_REQUEST, "unsupported image format"))?;
    let extension = match format {
        image::ImageFormat::Png => "png",
        image::ImageFormat::Jpeg => "jpg",
        image::ImageFormat::Gif => "gif",
        image::ImageFormat::WebP => "webp",
        image::ImageFormat::Bmp => "bmp",
        _ => {
            return Err(ApiError::new(
                StatusCode::BAD_REQUEST,
                "unsupported image format",
            ))
        }
    };
    let hash = format!("{:x}", Sha256::digest(&body));
    let relative = format!("persona-avatars/{hash}.{extension}");
    let directory = state.paths.persona_avatars_dir();
    let destination = directory.join(format!("{hash}.{extension}"));
    tokio::fs::create_dir_all(&directory)
        .await
        .map_err(ApiError::internal)?;
    let directory_metadata = tokio::fs::symlink_metadata(&directory)
        .await
        .map_err(ApiError::internal)?;
    if directory_metadata.file_type().is_symlink() || !directory_metadata.is_dir() {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "persona asset directory is unsafe",
        ));
    }
    store_persona_asset(&directory, &destination, &hash, &body).await?;
    let config = state.manager.lock().unwrap().config.clone();
    if let Ok(prompts) = read_prompt_documents(&config, &state.paths) {
        cleanup_persona_assets(&state.paths, &prompts, &prompts);
    }
    Ok(Json(json!({
        "path": relative,
        "preview_url": format!("/api/persona/avatar?path={relative}"),
    })))
}

pub(in crate::web) async fn store_persona_asset(
    directory: &FilePath,
    destination: &FilePath,
    expected_hash: &str,
    body: &[u8],
) -> std::result::Result<(), ApiError> {
    let replace_corrupt = match tokio::fs::symlink_metadata(destination).await {
        Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => {
            match verify_persona_asset_hash(destination, expected_hash).await {
                Ok(()) => return Ok(()),
                Err(error) if error.status == StatusCode::CONFLICT => true,
                Err(error) => return Err(error),
            }
        }
        Ok(_) => {
            return Err(ApiError::new(
                StatusCode::CONFLICT,
                "persona asset destination is unsafe",
            ));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(error) => return Err(ApiError::internal(error)),
    };

    let temporary = directory.join(format!(
        ".upload-{}-{:016x}",
        std::process::id(),
        rand::random::<u64>()
    ));
    let mut file = tokio::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&temporary)
        .await
        .map_err(ApiError::internal)?;
    let write_result = async {
        file.write_all(body).await?;
        file.sync_all().await?;
        if replace_corrupt {
            tokio::fs::rename(&temporary, destination).await
        } else {
            tokio::fs::hard_link(&temporary, destination).await
        }
    }
    .await;
    match write_result {
        Ok(()) => {
            let _ = tokio::fs::remove_file(&temporary).await;
            let directory = tokio::fs::File::open(directory)
                .await
                .map_err(ApiError::internal)?;
            directory.sync_all().await.map_err(ApiError::internal)?;
            Ok(())
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            let _ = tokio::fs::remove_file(&temporary).await;
            verify_persona_asset_hash(destination, expected_hash).await
        }
        Err(error) => {
            let _ = tokio::fs::remove_file(&temporary).await;
            Err(ApiError::internal(error))
        }
    }
}

pub(in crate::web) async fn verify_persona_asset_hash(
    path: &FilePath,
    expected_hash: &str,
) -> std::result::Result<(), ApiError> {
    let bytes = tokio::fs::read(path).await.map_err(ApiError::internal)?;
    if bytes.len() > PERSONA_ASSET_LIMIT || format!("{:x}", Sha256::digest(&bytes)) != expected_hash
    {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "persona asset cache entry is corrupted",
        ));
    }
    Ok(())
}

pub(in crate::web) fn text_asset(content: &'static str, content_type: &'static str) -> Response {
    asset_response(content.as_bytes(), content_type)
}

pub(in crate::web) fn binary_asset(content: &'static [u8], content_type: &'static str) -> Response {
    asset_response(content, content_type)
}

pub(in crate::web) fn asset_response(
    content: &'static [u8],
    content_type: &'static str,
) -> Response {
    finish_asset_response(content.into_response(), content_type)
}

pub(in crate::web) fn finish_asset_response(
    mut response: Response,
    content_type: &'static str,
) -> Response {
    response
        .headers_mut()
        .insert(CONTENT_TYPE, HeaderValue::from_static(content_type));
    response
        .headers_mut()
        .insert(CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    response
        .headers_mut()
        .insert(X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    response.headers_mut().insert(
        CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(
            "default-src 'self'; img-src 'self'; media-src 'self' https: http:; style-src 'self'; script-src 'self'; connect-src 'self'; base-uri 'none'; frame-ancestors 'none'; form-action 'self'",
        ),
    );
    response
        .headers_mut()
        .insert(REFERRER_POLICY, HeaderValue::from_static("no-referrer"));
    response
}

pub(in crate::web) fn cleanup_persona_assets(
    paths: &MiyuPaths,
    previous: &PromptDocuments,
    current: &PromptDocuments,
) {
    let directory = paths.persona_avatars_dir();
    let Ok(entries) = std::fs::read_dir(&directory) else {
        return;
    };
    let referenced = |prompts: &PromptDocuments| {
        prompts
            .personas
            .iter()
            .flat_map(|document| {
                [
                    document.avatar_path.as_deref(),
                    document.board_image_path.as_deref(),
                ]
            })
            .flatten()
            .filter_map(|path| resolve_persona_asset_path(paths, path))
            .filter_map(|path| {
                path.strip_prefix(&directory)
                    .ok()
                    .map(|relative| relative.to_string_lossy().to_string())
            })
            .collect::<HashSet<_>>()
    };
    let previous = referenced(previous);
    let current = referenced(current);
    let now = std::time::SystemTime::now();
    for entry in entries.flatten() {
        let file_type = match entry.file_type() {
            Ok(file_type) => file_type,
            Err(_) => continue,
        };
        if !file_type.is_file() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        let stale = entry
            .metadata()
            .and_then(|metadata| metadata.modified())
            .ok()
            .and_then(|modified| now.duration_since(modified).ok())
            .is_some_and(|age| age >= std::time::Duration::from_secs(24 * 60 * 60));
        if name.starts_with(".upload-") {
            if stale {
                let _ = std::fs::remove_file(entry.path());
            }
            continue;
        }
        let bytes = name.as_bytes();
        let managed_name = bytes.len() >= 68
            && bytes[64] == b'.'
            && bytes[..64].iter().all(u8::is_ascii_hexdigit)
            && matches!(&bytes[65..], b"png" | b"jpg" | b"gif" | b"webp" | b"bmp");
        if !managed_name || current.contains(&name) {
            continue;
        }
        let old_reference = previous.contains(&name);
        if old_reference || stale {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

pub(in crate::web) async fn image_asset(
    State(state): State<DaemonState>,
    headers: HeaderMap,
    Path(asset_id): Path<String>,
) -> std::result::Result<Response, ApiError> {
    require_auth(&headers, &state)?;
    if asset_id.len() > 96
        || asset_id.is_empty()
        || !asset_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
    {
        return Err(ApiError::new(
            StatusCode::NOT_FOUND,
            "image asset not found",
        ));
    }
    let Some(asset) = state
        .state_store
        .load_image_asset(&asset_id)
        .map_err(ApiError::internal)?
    else {
        return Err(ApiError::new(
            StatusCode::NOT_FOUND,
            "image asset not found",
        ));
    };
    let mut response = asset.bytes.into_response();
    response.headers_mut().insert(
        CONTENT_TYPE,
        HeaderValue::from_str(&asset.asset.mime).map_err(ApiError::internal)?,
    );
    response.headers_mut().insert(
        CACHE_CONTROL,
        HeaderValue::from_static("private, max-age=86400"),
    );
    response
        .headers_mut()
        .insert(X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    Ok(response)
}

#[derive(Deserialize)]
pub(in crate::web) struct ArtifactQuery {
    #[serde(default)]
    download: Option<String>,
}

pub(in crate::web) async fn artifact_asset(
    State(state): State<DaemonState>,
    headers: HeaderMap,
    Path(asset_id): Path<String>,
    Query(query): Query<ArtifactQuery>,
) -> std::result::Result<Response, ApiError> {
    require_auth(&headers, &state)?;
    if asset_id.len() > 96
        || asset_id.is_empty()
        || !asset_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
    {
        return Err(ApiError::new(StatusCode::NOT_FOUND, "artifact not found"));
    }
    let Some(artifact) = state
        .state_store
        .load_artifact_asset(&asset_id)
        .map_err(ApiError::internal)?
    else {
        return Err(ApiError::new(StatusCode::NOT_FOUND, "artifact not found"));
    };
    // `?download=1` 强制 attachment:预览按钮走 inline,下载按钮拿到的必须
    // 是真下载,不能又弹一个预览页。
    //
    // **svg 刻意不在这个名单里。** 它进得了预览面板(那边是 `<img>`,浏览器强制
    // 禁掉 SVG 里的脚本和外链),但直接导航过去就是在 WebUI 自己的域下渲染一份
    // 模型写的活性文档——那是 XSS。同一个判断在 shared_files.rs 里也写着。
    let force_download = query.download.as_deref() == Some("1");
    let inline = !force_download
        && matches!(
            artifact.asset.kind.as_str(),
            "markdown" | "text" | "code" | "json" | "csv" | "pdf" | "html"
        );
    let disposition = format!(
        "{}; filename*=UTF-8''{}",
        if inline { "inline" } else { "attachment" },
        urlencoding::encode(&artifact.asset.file_name)
    );
    let mut response = artifact.bytes.into_response();
    response.headers_mut().insert(
        CONTENT_TYPE,
        HeaderValue::from_str(&artifact.asset.mime).map_err(ApiError::internal)?,
    );
    response.headers_mut().insert(
        CONTENT_DISPOSITION,
        HeaderValue::from_str(&disposition).map_err(ApiError::internal)?,
    );
    response
        .headers_mut()
        .insert(CACHE_CONTROL, HeaderValue::from_static("private, no-cache"));
    response
        .headers_mut()
        .insert(X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    if let Some(policy) = artifact_csp(artifact.asset.kind.as_str(), &headers) {
        response.headers_mut().insert(
            CONTENT_SECURITY_POLICY,
            HeaderValue::from_str(&policy).map_err(ApiError::internal)?,
        );
    }
    Ok(response)
}

/// 活性内容(html / svg)的出站策略。**这是外泄的唯一一道闸**——iframe 的
/// `sandbox` 属性只管权限(脚本能不能跑、能不能提交表单),完全不限制网络请求。
/// 两道各管一半,缺一不可。
///
/// html 放开脚本(不放开的话图表、按钮、切换全是死的),但出站三条路全掐:
/// `connect-src 'none'` 掐 fetch/XHR/WebSocket,`img-src` 不含外域掐像素外带,
/// `script-src` 不含外域掐 CDN。
///
/// **WebRTC 那条封不住,而且这里刻意不写 `webrtc 'block'`。** Chromium 151 和
/// Firefox 153 都不认这个指令(实测 `RTCPeerConnection` 照样构造得出来),写上去
/// 唯一的效果是每开一份 HTML artifact 就往控制台丢一条
/// `Unrecognized Content-Security-Policy directive 'webrtc'`——拿一条常驻报错
/// 换一个不生效的防护不划算。等浏览器认了再加回来。
/// (Claude 的生产 CSP 里有这条,同样是不生效的。)
///
/// svg 只走 `<img>`,压根不需要脚本,所以维持最严:一行 JS 都不给。
fn artifact_csp(kind: &str, headers: &HeaderMap) -> Option<String> {
    const STRICT: &str =
        "sandbox; default-src 'none'; style-src 'unsafe-inline'; img-src data: blob:";
    match kind {
        "svg" => Some(STRICT.to_string()),
        "html" => Some(match request_origin(headers) {
            // 不透明源下 `'self'` 匹配不上任何东西(它匹配文档自己的源,而不透明源
            // 与谁都不相等),所以本机来源必须逐字写进去,页面才加载得到 /vendor/ 的库。
            Some(origin) => format!(
                "sandbox allow-scripts allow-modals; \
                 default-src 'none'; \
                 script-src 'unsafe-inline' 'unsafe-eval' {origin}; \
                 style-src 'unsafe-inline' {origin}; \
                 font-src data: {origin}; \
                 img-src data: blob: {origin}; \
                 media-src data: blob: {origin}; \
                 connect-src 'none'; form-action 'none'; frame-src 'none'; \
                 object-src 'none'; base-uri 'none'"
            ),
            // 拿不到可信的 Host 就退回最严,宁可页面画不出来也不放开一个拼错的来源。
            None => STRICT.to_string(),
        }),
        _ => None,
    }
}

/// 从 `Host` 头还原本次请求的来源。**Host 是请求方可控的**,要逐字节拼进 CSP,
/// 所以字符集按白名单卡死——放进一个分号就等于让调用方改写整条策略。
fn request_origin(headers: &HeaderMap) -> Option<String> {
    let host = headers.get(HOST)?.to_str().ok()?;
    let shaped = host.len() <= 260
        && !host.is_empty()
        && host.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b':' | b'[' | b']')
        });
    if !shaped {
        return None;
    }
    // 反代在前面时 daemon 自己仍是 http,scheme 只能问转发头。
    let scheme = headers
        .get("x-forwarded-proto")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(',').next())
        .map(str::trim)
        .filter(|value| *value == "https")
        .unwrap_or("http");
    Some(format!("{scheme}://{host}"))
}

pub(in crate::web) fn resolve_persona_asset_path(
    paths: &MiyuPaths,
    value: &str,
) -> Option<PathBuf> {
    let value = value.trim();
    if persona_asset_uses_managed_namespace(value) {
        return managed_persona_asset_path(paths, value);
    }
    let path = PathBuf::from(value);
    if let Some(path) = paths.migrated_resource_path(&path) {
        return Some(path);
    }
    Some(if path.is_absolute() {
        path
    } else {
        paths.config_dir.join(path)
    })
}

pub(in crate::web) fn managed_persona_asset_path(
    paths: &MiyuPaths,
    value: &str,
) -> Option<PathBuf> {
    let value = value.trim();
    if value.contains('\\') || value.chars().any(char::is_control) {
        return None;
    }
    let mut components = std::path::Path::new(value).components();
    while matches!(
        components.clone().next(),
        Some(std::path::Component::CurDir)
    ) {
        components.next();
    }
    if !matches!(components.next(), Some(std::path::Component::Normal(name)) if name == "persona-avatars")
    {
        return None;
    }
    let mut normalized = PathBuf::new();
    for component in components {
        match component {
            std::path::Component::Normal(component) => normalized.push(component),
            _ => return None,
        }
    }
    if normalized.as_os_str().is_empty() {
        return None;
    }
    Some(paths.persona_avatars_dir().join(normalized))
}

pub(in crate::web) fn persona_asset_uses_managed_namespace(value: &str) -> bool {
    std::path::Path::new(value)
        .components()
        .find(|component| !matches!(component, std::path::Component::CurDir))
        .is_some_and(|component| {
            matches!(component, std::path::Component::Normal(name) if name == "persona-avatars")
        })
}

pub(in crate::web) fn validate_managed_persona_asset_file(
    paths: &MiyuPaths,
    path: &FilePath,
) -> Result<()> {
    let root_path = paths.persona_avatars_dir();
    let root_metadata = std::fs::symlink_metadata(&root_path)?;
    if root_metadata.file_type().is_symlink() || !root_metadata.is_dir() {
        bail!("managed persona asset directory is unsafe");
    }
    let root = std::fs::canonicalize(root_path)?;
    let canonical = std::fs::canonicalize(path)?;
    if !canonical.starts_with(&root) || !std::fs::metadata(&canonical)?.is_file() {
        bail!("managed persona asset escapes its resource directory");
    }
    Ok(())
}

impl From<ArtifactAsset> for SafeArtifactAsset {
    fn from(asset: ArtifactAsset) -> Self {
        Self {
            url: format!("/api/artifacts/{}", asset.asset_id),
            id: asset.asset_id,
            name: asset.file_name,
            mime: asset.mime,
            kind: asset.kind,
            type_label: artifact_type_label(&asset.source_key),
            size: asset.size_bytes,
            updated_at: asset.updated_at,
        }
    }
}

impl SafeImageAsset {
    pub(in crate::web) fn from_asset(asset: ImageAsset, hide_caption: bool) -> Self {
        Self {
            url: format!("/api/assets/{}", asset.asset_id),
            id: asset.asset_id,
            mime: asset.mime,
            width: asset.width,
            height: asset.height,
            alt: asset.alt,
            hide_caption,
        }
    }
}

impl From<ImageAsset> for SafeImageAsset {
    fn from(asset: ImageAsset) -> Self {
        Self::from_asset(asset, false)
    }
}
