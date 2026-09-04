//! 记忆浏览器:按人格分库,列表 / 过滤 / 详情(含修订历史)/ 编辑 / 删除 /
//! 手工新增 / 逐出归档浏览 / 清理与重置。读走零副作用查询,不建库不衰减。

use crate::memory::browse::{BrowsePatch, BrowseQuery, BrowseTable, EpisodeStage, EvictedQuery};
use crate::web::*;

#[derive(Deserialize)]
pub(in crate::web) struct PersonaQuery {
    #[serde(default)]
    persona: String,
}

#[derive(Deserialize)]
pub(in crate::web) struct BrowseParams {
    #[serde(default)]
    persona: String,
    #[serde(default = "default_table")]
    table: String,
    #[serde(default)]
    q: String,
    #[serde(default)]
    status: String,
    #[serde(default)]
    memory_type: String,
    #[serde(default)]
    truth_status: String,
    #[serde(default)]
    visibility: String,
    #[serde(default)]
    retention: String,
    #[serde(default)]
    stage: String,
    #[serde(default)]
    origin_kind: String,
    #[serde(default)]
    tag: String,
    #[serde(default = "default_limit")]
    limit: usize,
    #[serde(default)]
    offset: usize,
}

#[derive(Deserialize)]
pub(in crate::web) struct EvictedParams {
    #[serde(default)]
    persona: String,
    #[serde(default)]
    q: String,
    #[serde(default)]
    role: String,
    #[serde(default)]
    start: String,
    #[serde(default)]
    end: String,
    #[serde(default = "default_limit")]
    limit: usize,
    #[serde(default)]
    offset: usize,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::web) struct PatchBody {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    importance: Option<i64>,
    #[serde(default)]
    memory_type: Option<String>,
    #[serde(default)]
    truth_status: Option<String>,
    #[serde(default)]
    tags: Option<Vec<String>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::web) struct AddFactBody {
    content: String,
    #[serde(default)]
    source: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::web) struct ResetBody {
    /// 必须等于人格作用域名,防止误点。
    confirm: String,
}

fn default_table() -> String {
    "facts".to_string()
}

fn default_limit() -> usize {
    50
}

/// 人格名进路径,只认平面名字。
fn persona_scoped_config(
    state: &DaemonState,
    persona: &str,
) -> std::result::Result<AppConfig, ApiError> {
    let mut config = state.manager.lock().unwrap().config.clone();
    let persona = persona.trim();
    // 空名或与当前人格同一作用域:原样用当前配置(空名的作用域是 "default")。
    if persona.is_empty()
        || persona == crate::config::persona_scope_name(&config.prompt.active_persona)
    {
        return Ok(config);
    }
    if persona.len() > 64
        || persona.contains(['/', '\\', '\0'])
        || persona == "."
        || persona == ".."
    {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "invalid persona name",
        ));
    }
    config.prompt.active_persona = persona.to_string();
    Ok(config)
}

fn memory_store(
    state: &DaemonState,
    persona: &str,
) -> std::result::Result<crate::memory::MemoryStore, ApiError> {
    let config = persona_scoped_config(state, persona)?;
    Ok(crate::memory::MemoryStore::new(&config, &state.paths))
}

fn parse_table(value: &str) -> std::result::Result<BrowseTable, ApiError> {
    BrowseTable::parse(value)
        .ok_or_else(|| ApiError::new(StatusCode::BAD_REQUEST, "table must be facts or episodes"))
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

/// 有记忆库的人格清单 + 当前活跃人格。
pub(in crate::web) async fn dash_memory_personas(
    State(state): State<DaemonState>,
    headers: HeaderMap,
) -> std::result::Result<Json<Value>, ApiError> {
    require_auth(&headers, &state)?;
    let config = state.manager.lock().unwrap().config.clone();
    let active = crate::config::persona_scope_name(&config.prompt.active_persona);
    let mut names = std::collections::BTreeSet::new();
    names.insert(active.clone());
    if let Ok(entries) = std::fs::read_dir(state.paths.data_dir.join("personas")) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.join("memory").join("memory.db").is_file() {
                if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                    names.insert(name.to_string());
                }
            }
        }
    }
    Ok(Json(json!({ "active": active, "personas": names })))
}

pub(in crate::web) async fn dash_memory_stats(
    State(state): State<DaemonState>,
    headers: HeaderMap,
    Query(query): Query<PersonaQuery>,
) -> std::result::Result<Json<Value>, ApiError> {
    require_auth(&headers, &state)?;
    let store = memory_store(&state, &query.persona)?;
    let stats = blocking(move || store.stats_readonly()).await?;
    Ok(Json(stats))
}

pub(in crate::web) async fn dash_memory_items(
    State(state): State<DaemonState>,
    headers: HeaderMap,
    Query(params): Query<BrowseParams>,
) -> std::result::Result<Json<Value>, ApiError> {
    require_auth(&headers, &state)?;
    let table = parse_table(&params.table)?;
    let stage = match params.stage.trim() {
        "" | "all" => None,
        value => Some(
            EpisodeStage::parse(value)
                .ok_or_else(|| ApiError::new(StatusCode::BAD_REQUEST, "invalid stage"))?,
        ),
    };
    let store = memory_store(&state, &params.persona)?;
    let query = BrowseQuery {
        text: params.q,
        status: params.status,
        memory_type: params.memory_type,
        truth_status: params.truth_status,
        visibility: params.visibility,
        retention: params.retention,
        stage,
        origin_kind: params.origin_kind,
        tag: params.tag,
        limit: params.limit,
        offset: params.offset,
    };
    let page = blocking(move || store.browse(table, &query)).await?;
    Ok(Json(
        json!({ "ok": true, "items": page.items, "total": page.total }),
    ))
}

/// 单条详情:本体 + 修订历史 + 来源经历。
pub(in crate::web) async fn dash_memory_item(
    State(state): State<DaemonState>,
    headers: HeaderMap,
    Path((table, id)): Path<(String, i64)>,
    Query(query): Query<PersonaQuery>,
) -> std::result::Result<Json<Value>, ApiError> {
    require_auth(&headers, &state)?;
    let table = parse_table(&table)?;
    let store = memory_store(&state, &query.persona)?;
    let detail = blocking(move || {
        let Some(item) = store.browse_item(table, id)? else {
            return Ok(None);
        };
        let revisions = if table == BrowseTable::Facts {
            store.browse_revisions(id)?
        } else {
            Vec::new()
        };
        let source_ids: Vec<i64> = item["source_episode_ids"]
            .as_array()
            .map(|ids| ids.iter().filter_map(Value::as_i64).collect())
            .unwrap_or_default();
        let source_episodes = store.browse_episodes_by_ids(&source_ids)?;
        Ok(Some(json!({
            "ok": true,
            "item": item,
            "revisions": revisions,
            "source_episodes": source_episodes,
        })))
    })
    .await?;
    detail
        .map(Json)
        .ok_or_else(|| ApiError::new(StatusCode::NOT_FOUND, "memory item not found"))
}

pub(in crate::web) async fn dash_memory_patch(
    State(state): State<DaemonState>,
    headers: HeaderMap,
    Path((table, id)): Path<(String, i64)>,
    Query(query): Query<PersonaQuery>,
    Json(body): Json<PatchBody>,
) -> std::result::Result<Json<Value>, ApiError> {
    require_mutation(&headers, &state)?;
    let table = parse_table(&table)?;
    let store = memory_store(&state, &query.persona)?;
    let patch = BrowsePatch {
        content: body.content,
        status: body.status,
        importance: body.importance,
        memory_type: body.memory_type,
        truth_status: body.truth_status,
        tags: body.tags,
    };
    // 校验错误(空内容、非法枚举)要原样回给前端,不能吞成 500。
    let result = tokio::task::spawn_blocking(move || store.update_item(table, id, &patch))
        .await
        .map_err(ApiError::internal)?;
    match result {
        Ok(true) => Ok(Json(json!({ "ok": true }))),
        Ok(false) => Err(ApiError::new(
            StatusCode::NOT_FOUND,
            "memory item not found",
        )),
        Err(error) => Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            safe_error_message(&error),
        )),
    }
}

pub(in crate::web) async fn dash_memory_delete(
    State(state): State<DaemonState>,
    headers: HeaderMap,
    Path((table, id)): Path<(String, i64)>,
    Query(query): Query<PersonaQuery>,
) -> std::result::Result<Json<Value>, ApiError> {
    require_mutation(&headers, &state)?;
    let table = parse_table(&table)?;
    let store = memory_store(&state, &query.persona)?;
    let deleted = blocking(move || store.delete_item(table, id)).await?;
    if !deleted {
        return Err(ApiError::new(
            StatusCode::NOT_FOUND,
            "memory item not found",
        ));
    }
    Ok(Json(json!({ "ok": true })))
}

/// 手工新增一条事实(特权可见性,来源默认 "dashboard")。
pub(in crate::web) async fn dash_memory_add_fact(
    State(state): State<DaemonState>,
    headers: HeaderMap,
    Query(query): Query<PersonaQuery>,
    Json(body): Json<AddFactBody>,
) -> std::result::Result<Json<Value>, ApiError> {
    require_mutation(&headers, &state)?;
    let content = body.content.trim().to_string();
    if content.is_empty() || content.chars().count() > 4000 {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "content must be 1..=4000 characters",
        ));
    }
    let source = body
        .source
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "dashboard".to_string());
    let store = memory_store(&state, &query.persona)?;
    let id = blocking(move || store.remember_fact(&content, &source)).await?;
    if id == 0 {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "memory is disabled in config",
        ));
    }
    Ok(Json(json!({ "ok": true, "id": id })))
}

pub(in crate::web) async fn dash_memory_evicted(
    State(state): State<DaemonState>,
    headers: HeaderMap,
    Query(params): Query<EvictedParams>,
) -> std::result::Result<Json<Value>, ApiError> {
    require_auth(&headers, &state)?;
    let store = memory_store(&state, &params.persona)?;
    let query = EvictedQuery {
        text: params.q,
        role: params.role,
        start: params.start,
        end: params.end,
        limit: params.limit,
        offset: params.offset,
    };
    let page = blocking(move || store.browse_evicted(&query)).await?;
    Ok(Json(
        json!({ "ok": true, "items": page.items, "total": page.total }),
    ))
}

pub(in crate::web) async fn dash_memory_evicted_item(
    State(state): State<DaemonState>,
    headers: HeaderMap,
    Path(id): Path<i64>,
    Query(query): Query<PersonaQuery>,
) -> std::result::Result<Json<Value>, ApiError> {
    require_auth(&headers, &state)?;
    let store = memory_store(&state, &query.persona)?;
    let item = blocking(move || store.browse_evicted_item(id)).await?;
    item.map(|item| Json(json!({ "ok": true, "item": item })))
        .ok_or_else(|| ApiError::new(StatusCode::NOT_FOUND, "evicted turn not found"))
}

pub(in crate::web) async fn dash_memory_evicted_delete(
    State(state): State<DaemonState>,
    headers: HeaderMap,
    Path(id): Path<i64>,
    Query(query): Query<PersonaQuery>,
) -> std::result::Result<Json<Value>, ApiError> {
    require_mutation(&headers, &state)?;
    let store = memory_store(&state, &query.persona)?;
    let deleted = blocking(move || store.delete_evicted_item(id)).await?;
    if !deleted {
        return Err(ApiError::new(
            StatusCode::NOT_FOUND,
            "evicted turn not found",
        ));
    }
    Ok(Json(json!({ "ok": true })))
}

pub(in crate::web) async fn dash_memory_evicted_clear(
    State(state): State<DaemonState>,
    headers: HeaderMap,
    Query(query): Query<PersonaQuery>,
) -> std::result::Result<Json<Value>, ApiError> {
    require_mutation(&headers, &state)?;
    let store = memory_store(&state, &query.persona)?;
    blocking(move || store.clear_evicted_context()).await?;
    Ok(Json(json!({ "ok": true })))
}

pub(in crate::web) async fn dash_memory_pending_clear(
    State(state): State<DaemonState>,
    headers: HeaderMap,
    Query(query): Query<PersonaQuery>,
) -> std::result::Result<Json<Value>, ApiError> {
    require_mutation(&headers, &state)?;
    let store = memory_store(&state, &query.persona)?;
    blocking(move || store.clear_pending_events()).await?;
    Ok(Json(json!({ "ok": true })))
}

/// 重置整个人格的记忆(事实/经历/待处理/修订/逐出归档),技能目录不动。
pub(in crate::web) async fn dash_memory_reset(
    State(state): State<DaemonState>,
    headers: HeaderMap,
    Query(query): Query<PersonaQuery>,
    Json(body): Json<ResetBody>,
) -> std::result::Result<Json<Value>, ApiError> {
    require_mutation(&headers, &state)?;
    let config = persona_scoped_config(&state, &query.persona)?;
    let scope = crate::config::persona_scope_name(&config.prompt.active_persona);
    if body.confirm.trim() != scope {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "confirm must equal the persona scope name",
        ));
    }
    let store = crate::memory::MemoryStore::new(&config, &state.paths);
    blocking(move || store.reset_all(false)).await?;
    Ok(Json(json!({ "ok": true })))
}
