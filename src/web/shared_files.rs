//! 文件分享的 HTTP 面：列表、上传、下载（Range 流式）、删除。
//!
//! 与 artifact 路由（SQLite blob 全量响应）不同，这里的字节始终来自磁盘并
//! 流式返回——大视频既不进内存也不进库。reference 模式在**每次**下载前校验
//! size/mtime 指纹：原文件被移动/修改后返回 410，绝不流出与分享时不一致的
//! 内容。凭证与 WebUI 完全一致（`require_auth`），能打开 WebUI 就能下载。

use crate::web::*;

/// 只有这三类允许内联预览；其余一律 attachment，杜绝 HTML/SVG 之类的
/// 活性内容在 WebUI 域下渲染。
fn inline_allowed(kind: &str) -> bool {
    matches!(kind, "video" | "audio" | "image")
}

fn valid_share_id(share_id: &str) -> bool {
    !share_id.is_empty()
        && share_id.len() <= 96
        && share_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
}

fn share_json(record: &crate::state::SharedFile) -> serde_json::Value {
    json!({
        "share_id": record.share_id,
        "file_name": record.file_name,
        "title": record.title,
        "mode": record.mode,
        "kind": record.kind,
        "mime": record.mime,
        "size_bytes": record.size_bytes,
        "created_at": record.created_at,
    })
}

pub(in crate::web) async fn shared_files_list(
    State(state): State<DaemonState>,
    headers: HeaderMap,
) -> std::result::Result<Response, ApiError> {
    require_auth(&headers, &state)?;
    let shares = state
        .state_store
        .list_shared_files()
        .map_err(ApiError::internal)?;
    Ok(axum::Json(json!({
        "shares": shares.iter().map(share_json).collect::<Vec<_>>()
    }))
    .into_response())
}

pub(in crate::web) async fn shared_file_delete(
    State(state): State<DaemonState>,
    headers: HeaderMap,
    Path(share_id): Path<String>,
) -> std::result::Result<Response, ApiError> {
    require_auth(&headers, &state)?;
    if !valid_share_id(&share_id) {
        return Err(ApiError::new(StatusCode::NOT_FOUND, "share not found"));
    }
    let deleted = state
        .state_store
        .delete_shared_file(&share_id)
        .map_err(ApiError::internal)?;
    if !deleted {
        return Err(ApiError::new(StatusCode::NOT_FOUND, "share not found"));
    }
    Ok(axum::Json(json!({ "ok": true })).into_response())
}

/// WebUI 主动上传:请求体流到落地区,再以 snapshot 模式登记——这就是
/// 「局域网传文件」的入口,不经过模型。文件名走附件同一套消毒,大小只受
/// `plugins.file_sharing.max_shared_file_bytes`(默认 0=不限)约束。
pub(in crate::web) async fn shared_file_upload(
    State(state): State<DaemonState>,
    headers: HeaderMap,
    body: axum::body::Body,
) -> std::result::Result<Response, ApiError> {
    require_mutation(&headers, &state)?;
    let sharing = state
        .manager
        .lock()
        .unwrap()
        .config
        .plugins
        .file_sharing
        .clone();
    if !sharing.enabled {
        return Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "file sharing is disabled in plugins.file_sharing",
        ));
    }
    let file_name = header_file_name(&headers, "x-miyu-filename")?
        .ok_or_else(|| ApiError::new(StatusCode::BAD_REQUEST, "attachment filename is required"))?;
    let file_name = sanitize_attachment_file_name(&file_name)?;
    let title = header_file_name(&headers, "x-miyu-title")?.unwrap_or_default();

    let store = &state.state_store;
    let staging_dir = store.shared_files_incoming_dir().join(random_id("up", 16));
    tokio::fs::create_dir_all(&staging_dir)
        .await
        .map_err(ApiError::internal)?;
    let staged = staging_dir.join(&file_name);
    let outcome = spool_body_to_file(body, &staged).await;
    let size = match outcome {
        Ok(size) if size > 0 => size,
        Ok(_) => {
            let _ = tokio::fs::remove_dir_all(&staging_dir).await;
            return Err(ApiError::new(
                StatusCode::BAD_REQUEST,
                "uploaded file must not be empty",
            ));
        }
        Err(error) => {
            let _ = tokio::fs::remove_dir_all(&staging_dir).await;
            return Err(error);
        }
    };
    if sharing.max_shared_file_bytes > 0 && size > sharing.max_shared_file_bytes {
        let _ = tokio::fs::remove_dir_all(&staging_dir).await;
        return Err(ApiError::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            format!(
                "uploaded file is {size} bytes which exceeds max_shared_file_bytes ({})",
                sharing.max_shared_file_bytes
            ),
        ));
    }
    let registered = {
        let store = store.clone();
        let staged = staged.clone();
        let file_name = file_name.clone();
        tokio::task::spawn_blocking(move || store.share_uploaded_file(&staged, &file_name, &title))
            .await
            .map_err(ApiError::internal)?
    };
    let _ = tokio::fs::remove_dir_all(&staging_dir).await;
    let record = registered.map_err(ApiError::internal)?;
    Ok(axum::Json(share_json(&record)).into_response())
}

/// 读一个 URL 编码的文本头;缺失返回 `None`,编码坏了报 400。
fn header_file_name(
    headers: &HeaderMap,
    name: &str,
) -> std::result::Result<Option<String>, ApiError> {
    let Some(raw) = headers.get(name).and_then(|value| value.to_str().ok()) else {
        return Ok(None);
    };
    urlencoding::decode(raw)
        .map(|value| Some(value.into_owned()))
        .map_err(|_| ApiError::new(StatusCode::BAD_REQUEST, format!("{name} header is invalid")))
}

#[derive(Deserialize)]
pub(in crate::web) struct SharedDownloadQuery {
    #[serde(default)]
    download: Option<String>,
}

pub(in crate::web) async fn shared_file_download(
    State(state): State<DaemonState>,
    headers: HeaderMap,
    Path(share_id): Path<String>,
    Query(query): Query<SharedDownloadQuery>,
) -> std::result::Result<Response, ApiError> {
    require_auth(&headers, &state)?;
    if !valid_share_id(&share_id) {
        return Err(ApiError::new(StatusCode::NOT_FOUND, "share not found"));
    }
    let Some(record) = state
        .state_store
        .load_shared_file(&share_id)
        .map_err(ApiError::internal)?
    else {
        return Err(ApiError::new(StatusCode::NOT_FOUND, "share not found"));
    };
    let path = std::path::PathBuf::from(&record.stored_path);
    let metadata = tokio::fs::metadata(&path)
        .await
        .map_err(|_| ApiError::new(StatusCode::GONE, "the shared file no longer exists on disk"))?;
    if !metadata.is_file() {
        return Err(ApiError::new(
            StatusCode::GONE,
            "the shared file no longer exists on disk",
        ));
    }
    // reference 模式:分享的是「当时那份内容」,指纹变了就拒绝——宁可 410,
    // 不流出与分享时不一致的字节。快照模式的副本只属于托管区,不校验。
    if record.mode == "reference" {
        let mtime = metadata
            .modified()
            .ok()
            .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|duration| duration.as_secs() as i64)
            .unwrap_or(0);
        if metadata.len() != record.size_bytes || mtime != record.mtime_unix {
            return Err(ApiError::new(
                StatusCode::GONE,
                "the shared file has changed or been removed since it was shared; ask for it to be shared again",
            ));
        }
    }
    let force_download = query.download.as_deref().is_some_and(|value| value == "1");
    let inline = !force_download && inline_allowed(&record.kind);
    let disposition = format!(
        "{}; filename*=UTF-8''{}",
        if inline { "inline" } else { "attachment" },
        urlencoding::encode(&record.file_name)
    );
    let disposition = HeaderValue::from_str(&disposition).map_err(ApiError::internal)?;
    stream_file_response(
        &path,
        &record.mime,
        &headers,
        Some(disposition),
        "private, no-cache",
    )
    .await
}
