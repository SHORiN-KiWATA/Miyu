//! 记忆浏览器:按人格分库,列表 / 搜索 / 统计 / 单条删除。

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
    #[serde(default = "default_limit")]
    limit: usize,
    #[serde(default)]
    offset: usize,
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
    let stats = tokio::task::spawn_blocking(move || store.stats())
        .await
        .map_err(ApiError::internal)?
        .map_err(|error| ApiError::internal(safe_error_message(&error)))?;
    Ok(Json(stats))
}

pub(in crate::web) async fn dash_memory_items(
    State(state): State<DaemonState>,
    headers: HeaderMap,
    Query(params): Query<BrowseParams>,
) -> std::result::Result<Json<Value>, ApiError> {
    require_auth(&headers, &state)?;
    let Some(table) = crate::memory::browse::BrowseTable::parse(&params.table) else {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "table must be facts or episodes",
        ));
    };
    let store = memory_store(&state, &params.persona)?;
    let query = crate::memory::browse::BrowseQuery {
        text: params.q,
        status: params.status,
        limit: params.limit,
        offset: params.offset,
    };
    let page = tokio::task::spawn_blocking(move || store.browse(table, &query))
        .await
        .map_err(ApiError::internal)?
        .map_err(|error| ApiError::internal(safe_error_message(&error)))?;
    Ok(Json(
        json!({ "ok": true, "items": page.items, "total": page.total }),
    ))
}

pub(in crate::web) async fn dash_memory_delete(
    State(state): State<DaemonState>,
    headers: HeaderMap,
    Path((table, id)): Path<(String, i64)>,
    Query(query): Query<PersonaQuery>,
) -> std::result::Result<Json<Value>, ApiError> {
    require_mutation(&headers, &state)?;
    let Some(table) = crate::memory::browse::BrowseTable::parse(&table) else {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "table must be facts or episodes",
        ));
    };
    let store = memory_store(&state, &query.persona)?;
    let deleted = tokio::task::spawn_blocking(move || store.delete_item(table, id))
        .await
        .map_err(ApiError::internal)?
        .map_err(|error| ApiError::internal(safe_error_message(&error)))?;
    if !deleted {
        return Err(ApiError::new(
            StatusCode::NOT_FOUND,
            "memory item not found",
        ));
    }
    Ok(Json(json!({ "ok": true })))
}
