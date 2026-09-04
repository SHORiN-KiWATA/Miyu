//! 知识库面板:文件树 / 预览 / 搜索 / 上传 / 删除 / 语义重建 / 内置库更新。

use crate::tools::knowledge_base::KnowledgeBase;
use crate::web::*;
use std::sync::Mutex;

#[derive(Deserialize)]
pub(in crate::web) struct FileQuery {
    name: String,
    #[serde(default = "default_start")]
    start: usize,
    #[serde(default = "default_lines")]
    lines: usize,
}

#[derive(Deserialize)]
pub(in crate::web) struct NameQuery {
    name: String,
}

#[derive(Deserialize)]
pub(in crate::web) struct SearchQuery {
    #[serde(default)]
    q: String,
    #[serde(default = "default_by")]
    by: String,
    #[serde(default = "default_search_limit")]
    limit: usize,
}

fn default_start() -> usize {
    1
}
fn default_lines() -> usize {
    400
}
fn default_by() -> String {
    "content".into()
}
fn default_search_limit() -> usize {
    20
}

/// 单文件上传上限:配置里 max_file_size_kb 默认 1 MB,这里给个硬顶。
pub(in crate::web) const KB_UPLOAD_LIMIT: usize = 8 * 1024 * 1024;

fn kb(state: &DaemonState) -> std::result::Result<KnowledgeBase, ApiError> {
    let config = state.manager.lock().unwrap().config.clone();
    KnowledgeBase::new(config, state.paths.clone())
        .map_err(|error| ApiError::internal(safe_error_message(&error)))
}

async fn blocking<T, F>(work: F) -> std::result::Result<T, ApiError>
where
    T: Send + 'static,
    F: FnOnce() -> anyhow::Result<T> + Send + 'static,
{
    tokio::task::spawn_blocking(work)
        .await
        .map_err(ApiError::internal)?
        .map_err(|error| ApiError::internal(safe_error_message(&error)))
}

/// 用户输入引起的错误(路径、类型、守卫)原样回 400,不吞成 500。
async fn blocking_user<T, F>(work: F) -> std::result::Result<T, ApiError>
where
    T: Send + 'static,
    F: FnOnce() -> anyhow::Result<T> + Send + 'static,
{
    tokio::task::spawn_blocking(work)
        .await
        .map_err(ApiError::internal)?
        .map_err(|error| ApiError::new(StatusCode::BAD_REQUEST, safe_error_message(&error)))
}

pub(in crate::web) async fn dash_kb_overview(
    State(state): State<DaemonState>,
    headers: HeaderMap,
) -> std::result::Result<Json<Value>, ApiError> {
    require_auth(&headers, &state)?;
    let kb = kb(&state)?;
    let overview = blocking(move || kb.dashboard_overview()).await?;
    Ok(Json(overview))
}

pub(in crate::web) async fn dash_kb_file(
    State(state): State<DaemonState>,
    headers: HeaderMap,
    Query(query): Query<FileQuery>,
) -> std::result::Result<Json<Value>, ApiError> {
    require_auth(&headers, &state)?;
    let kb = kb(&state)?;
    let page =
        blocking_user(move || kb.dashboard_read(&query.name, query.start, query.lines)).await?;
    Ok(Json(page))
}

pub(in crate::web) async fn dash_kb_search(
    State(state): State<DaemonState>,
    headers: HeaderMap,
    Query(query): Query<SearchQuery>,
) -> std::result::Result<Json<Value>, ApiError> {
    require_auth(&headers, &state)?;
    let text = query.q.trim().to_string();
    if text.is_empty() {
        return Ok(Json(
            json!({ "ok": true, "query": "", "total_matches": 0, "results": [] }),
        ));
    }
    let kb = kb(&state)?;
    let limit = Some(query.limit.clamp(1, 50));
    let result = match query.by.as_str() {
        "name" => blocking(move || kb.find_by_name_readonly(&text, limit)).await?,
        _ => kb
            .search_readonly(&text, limit)
            .await
            .map_err(|error| ApiError::internal(safe_error_message(&error)))?,
    };
    Ok(Json(result))
}

/// 上传:`?name=<相对路径>` + 原始字节体。一次一个文件,前端循环。
pub(in crate::web) async fn dash_kb_upload(
    State(state): State<DaemonState>,
    headers: HeaderMap,
    Query(query): Query<NameQuery>,
    body: Bytes,
) -> std::result::Result<Json<Value>, ApiError> {
    require_mutation(&headers, &state)?;
    if body.is_empty() {
        return Err(ApiError::new(StatusCode::BAD_REQUEST, "empty file"));
    }
    let kb = kb(&state)?;
    let name = query.name;
    let stored = blocking_user(move || kb.dashboard_import(&name, &body)).await?;
    Ok(Json(json!({ "ok": true, "name": stored })))
}

pub(in crate::web) async fn dash_kb_delete(
    State(state): State<DaemonState>,
    headers: HeaderMap,
    Query(query): Query<NameQuery>,
) -> std::result::Result<Json<Value>, ApiError> {
    require_mutation(&headers, &state)?;
    let kb = kb(&state)?;
    blocking_user(move || kb.dashboard_remove(&query.name)).await?;
    Ok(Json(json!({ "ok": true })))
}

pub(in crate::web) async fn dash_kb_reindex_start(
    State(state): State<DaemonState>,
    headers: HeaderMap,
) -> std::result::Result<Json<Value>, ApiError> {
    require_mutation(&headers, &state)?;
    let kb = kb(&state)?;
    let result = blocking_user(move || kb.dashboard_reindex()).await?;
    Ok(Json(result))
}

pub(in crate::web) async fn dash_kb_reindex_status(
    State(state): State<DaemonState>,
    headers: HeaderMap,
) -> std::result::Result<Json<Value>, ApiError> {
    require_auth(&headers, &state)?;
    let kb = kb(&state)?;
    let status = blocking(move || {
        let mut status = kb.dashboard_reindex_status()?;
        let overview = kb.dashboard_overview()?;
        status["semantic_chunks"] = overview["semantic_chunks"].clone();
        status["stale_files"] = overview["stale_files"].clone();
        status["unindexed_files"] = overview["unindexed_files"].clone();
        Ok(status)
    })
    .await?;
    Ok(Json(status))
}

pub(in crate::web) async fn dash_kb_reindex_unlock(
    State(state): State<DaemonState>,
    headers: HeaderMap,
) -> std::result::Result<Json<Value>, ApiError> {
    require_mutation(&headers, &state)?;
    let kb = kb(&state)?;
    let cleared = blocking(move || kb.dashboard_clear_stale_lock()).await?;
    Ok(Json(json!({ "ok": true, "cleared": cleared })))
}

/// 内置库更新是一段几十秒的 git 操作:进程内单例任务,阶段进度放静态槽里
/// 让 GET 轮询;不进 jobs 注册表(那是会话绑定的)。
#[derive(Default)]
struct DefaultKbTask {
    running: bool,
    stage: String,
    error: String,
    finished_at: String,
}

static DEFAULT_KB_TASK: Mutex<Option<DefaultKbTask>> = Mutex::new(None);

fn default_kb_task_snapshot() -> Value {
    let guard = DEFAULT_KB_TASK.lock().unwrap();
    match guard.as_ref() {
        Some(task) => json!({
            "running": task.running,
            "stage": task.stage,
            "error": task.error,
            "finished_at": task.finished_at,
        }),
        None => json!({ "running": false, "stage": "", "error": "", "finished_at": "" }),
    }
}

pub(in crate::web) async fn dash_kb_default(
    State(state): State<DaemonState>,
    headers: HeaderMap,
) -> std::result::Result<Json<Value>, ApiError> {
    require_auth(&headers, &state)?;
    let paths = state.paths.clone();
    let kb_state = blocking(move || crate::default_kb::state(&paths)).await?;
    Ok(Json(json!({
        "ok": true,
        "bundled": crate::default_kb::bundled_available(),
        "state": kb_state,
        "task": default_kb_task_snapshot(),
    })))
}

pub(in crate::web) async fn dash_kb_default_update(
    State(state): State<DaemonState>,
    headers: HeaderMap,
) -> std::result::Result<Json<Value>, ApiError> {
    require_mutation(&headers, &state)?;
    {
        let mut guard = DEFAULT_KB_TASK.lock().unwrap();
        if guard.as_ref().is_some_and(|task| task.running) {
            return Ok(Json(
                json!({ "ok": true, "started": false, "reason": "running" }),
            ));
        }
        *guard = Some(DefaultKbTask {
            running: true,
            ..Default::default()
        });
    }
    let paths = state.paths.clone();
    let config = state.manager.lock().unwrap().config.clone();
    tokio::task::spawn_blocking(move || {
        let result = crate::default_kb::update(&paths, &config, |stage| {
            if let Some(task) = DEFAULT_KB_TASK.lock().unwrap().as_mut() {
                task.stage = stage.message().to_string();
            }
        });
        let mut guard = DEFAULT_KB_TASK.lock().unwrap();
        if let Some(task) = guard.as_mut() {
            task.running = false;
            task.finished_at = chrono::Utc::now().to_rfc3339();
            match result {
                Ok(_) => task.stage = t("done", "完成").to_string(),
                Err(error) => task.error = safe_error_message(&error),
            }
        }
    });
    Ok(Json(json!({ "ok": true, "started": true })))
}
