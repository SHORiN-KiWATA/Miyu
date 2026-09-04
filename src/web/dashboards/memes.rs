//! 表情包面板:库清单 / 全量列表 / 图片直出 / 上传 / 编辑 / 启停 / 删除 /
//! 视觉重分类 / 平台使用计数。写操作走 memes::dashboard 复用 crud 的锁与落盘。

use crate::tools::memes::{
    dashboard_add, dashboard_classify, dashboard_delete, dashboard_image, dashboard_libraries,
    dashboard_list, dashboard_update, DashboardPatch, DashboardUpload,
};
use crate::web::*;

#[derive(Deserialize)]
pub(in crate::web) struct LibraryQuery {
    #[serde(default)]
    library: String,
}

#[derive(Deserialize)]
pub(in crate::web) struct ImageQuery {
    #[serde(default)]
    library: String,
    id: String,
}

#[derive(Deserialize)]
pub(in crate::web) struct UploadQuery {
    #[serde(default)]
    library: String,
    #[serde(default = "default_mode")]
    mode: String,
    #[serde(default)]
    name_zh: String,
    #[serde(default)]
    name_en: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    usage: String,
    #[serde(default)]
    tags: String,
}

#[derive(Deserialize)]
pub(in crate::web) struct DeleteQuery {
    #[serde(default)]
    library: String,
    #[serde(default)]
    hard: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::web) struct PatchBody {
    #[serde(default)]
    name_zh: Option<String>,
    #[serde(default)]
    name_en: Option<String>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    usage: Option<String>,
    #[serde(default)]
    tags: Option<Vec<String>>,
    #[serde(default)]
    enabled: Option<bool>,
}

fn default_mode() -> String {
    "ai".into()
}

/// 单张上传硬顶;配置里的 max_image_mb 在 add_meme 里还会再查一次。
pub(in crate::web) const MEME_UPLOAD_LIMIT: usize = 24 * 1024 * 1024;

fn library_name(state: &DaemonState, requested: &str) -> String {
    let requested = requested.trim();
    if requested.is_empty() {
        let config = state.manager.lock().unwrap().config.clone();
        crate::tools::memes::current_persona_library(&config)
    } else {
        crate::tools::memes::sanitize_library(requested)
    }
}

fn valid_id(id: &str) -> std::result::Result<String, ApiError> {
    let id = id.trim();
    let hash = id.strip_prefix("sha256:").unwrap_or(id);
    if hash.len() < 7 || hash.len() > 64 || !hash.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(ApiError::new(StatusCode::BAD_REQUEST, "invalid meme id"));
    }
    Ok(id.to_string())
}

fn user_error(error: anyhow::Error) -> ApiError {
    ApiError::new(StatusCode::BAD_REQUEST, safe_error_message(&error))
}

pub(in crate::web) async fn dash_memes_libraries(
    State(state): State<DaemonState>,
    headers: HeaderMap,
) -> std::result::Result<Json<Value>, ApiError> {
    require_auth(&headers, &state)?;
    let config = state.manager.lock().unwrap().config.clone();
    let paths = state.paths.clone();
    let result = tokio::task::spawn_blocking(move || dashboard_libraries(&config, &paths))
        .await
        .map_err(ApiError::internal)?
        .map_err(|error| ApiError::internal(safe_error_message(&error)))?;
    Ok(Json(result))
}

pub(in crate::web) async fn dash_memes_items(
    State(state): State<DaemonState>,
    headers: HeaderMap,
    Query(query): Query<LibraryQuery>,
) -> std::result::Result<Json<Value>, ApiError> {
    require_auth(&headers, &state)?;
    let library = library_name(&state, &query.library);
    let paths = state.paths.clone();
    let store = state.state_store.clone();
    let result = tokio::task::spawn_blocking(move || -> anyhow::Result<Value> {
        let mut listing = dashboard_list(&paths, &library)?;
        let counts = store.platform_meme_ref_counts(&library)?;
        listing["refs"] = json!(counts);
        Ok(listing)
    })
    .await
    .map_err(ApiError::internal)?
    .map_err(|error| ApiError::internal(safe_error_message(&error)))?;
    Ok(Json(result))
}

/// 图片直出:按 (库, id) 解析路径,绝不接受调用方传路径。
pub(in crate::web) async fn dash_memes_image(
    State(state): State<DaemonState>,
    headers: HeaderMap,
    Query(query): Query<ImageQuery>,
) -> std::result::Result<Response, ApiError> {
    require_auth(&headers, &state)?;
    let library = library_name(&state, &query.library);
    let id = valid_id(&query.id)?;
    let paths = state.paths.clone();
    let resolved = tokio::task::spawn_blocking(move || dashboard_image(&paths, &library, &id))
        .await
        .map_err(ApiError::internal)?
        .map_err(user_error)?;
    let Some((path, mime)) = resolved else {
        return Err(ApiError::new(StatusCode::NOT_FOUND, "meme not found"));
    };
    let mime: &'static str = match mime.as_str() {
        "image/png" => "image/png",
        "image/gif" => "image/gif",
        "image/webp" => "image/webp",
        _ => "image/jpeg",
    };
    stream_file_response(&path, mime, &headers, None, "private, max-age=86400").await
}

pub(in crate::web) async fn dash_memes_upload(
    State(state): State<DaemonState>,
    headers: HeaderMap,
    Query(query): Query<UploadQuery>,
    body: Bytes,
) -> std::result::Result<Json<Value>, ApiError> {
    require_mutation(&headers, &state)?;
    if body.is_empty() {
        return Err(ApiError::new(StatusCode::BAD_REQUEST, "empty image"));
    }
    let manual = query.mode == "manual";
    if manual
        && (query.name_zh.trim().is_empty()
            || query.description.trim().is_empty()
            || query.usage.trim().is_empty())
    {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "manual mode needs name_zh, description and usage",
        ));
    }
    let upload = DashboardUpload {
        library: library_name(&state, &query.library),
        name_zh: query.name_zh.trim().to_string(),
        name_en: query.name_en.trim().to_string(),
        description: query.description.trim().to_string(),
        usage: query.usage.trim().to_string(),
        tags: query
            .tags
            .split(|c: char| c == ',' || c == '，' || c == '、')
            .map(str::trim)
            .filter(|tag| !tag.is_empty())
            .map(str::to_string)
            .collect(),
        manual,
    };
    let config = state.manager.lock().unwrap().config.clone();
    let paths = state.paths.clone();
    // add_meme 内部同步读写文件,但视觉分类是网络调用,整段留在 async 里。
    let result = dashboard_add(&config, &paths, upload, &body)
        .await
        .map_err(user_error)?;
    Ok(Json(result))
}

pub(in crate::web) async fn dash_memes_patch(
    State(state): State<DaemonState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Query(query): Query<LibraryQuery>,
    Json(body): Json<PatchBody>,
) -> std::result::Result<Json<Value>, ApiError> {
    require_mutation(&headers, &state)?;
    let id = valid_id(&id)?;
    let library = library_name(&state, &query.library);
    let config = state.manager.lock().unwrap().config.clone();
    let patch = DashboardPatch {
        name_zh: body.name_zh.map(|v| v.trim().to_string()),
        name_en: body.name_en.map(|v| v.trim().to_string()),
        description: body.description.map(|v| v.trim().to_string()),
        usage: body.usage.map(|v| v.trim().to_string()),
        tags: body.tags.map(|tags| {
            tags.into_iter()
                .map(|t| t.trim().to_string())
                .filter(|t| !t.is_empty())
                .collect()
        }),
        enabled: body.enabled,
    };
    let result = dashboard_update(&config, &state.paths, &library, &id, patch)
        .await
        .map_err(user_error)?;
    Ok(Json(result))
}

pub(in crate::web) async fn dash_memes_delete(
    State(state): State<DaemonState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Query(query): Query<DeleteQuery>,
) -> std::result::Result<Json<Value>, ApiError> {
    require_mutation(&headers, &state)?;
    let id = valid_id(&id)?;
    let library = library_name(&state, &query.library);
    let config = state.manager.lock().unwrap().config.clone();
    let result = dashboard_delete(&config, &state.paths, &library, &id, query.hard)
        .await
        .map_err(user_error)?;
    Ok(Json(result))
}

pub(in crate::web) async fn dash_memes_classify(
    State(state): State<DaemonState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Query(query): Query<LibraryQuery>,
) -> std::result::Result<Json<Value>, ApiError> {
    require_mutation(&headers, &state)?;
    let id = valid_id(&id)?;
    let library = library_name(&state, &query.library);
    let config = state.manager.lock().unwrap().config.clone();
    let result = dashboard_classify(&config, &state.paths, &library, &id)
        .await
        .map_err(user_error)?;
    Ok(Json(result))
}
