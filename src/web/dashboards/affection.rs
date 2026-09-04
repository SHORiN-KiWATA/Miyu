//! 好感·情绪面板的 HTTP 面。好感度档案按 (账号, 人格) 分键,读走 kv 整批筛,
//! 写走 plugin_update_json 读改写;情绪状态接口待情绪功能落地后补。

use crate::platforms::plugins::real_context::affection::dashboard as affection;
use crate::platforms::plugins::real_context::emotion;
use crate::web::*;

#[derive(Deserialize)]
pub(in crate::web) struct ListQuery {
    #[serde(default)]
    account: String,
    #[serde(default)]
    persona: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::web) struct PatchBody {
    #[serde(default)]
    score: Option<f64>,
    #[serde(default)]
    note: Option<String>,
    #[serde(default)]
    tags: Option<Vec<String>>,
    #[serde(default)]
    auto_update_enabled: Option<bool>,
    #[serde(default)]
    clear_events: bool,
    #[serde(default)]
    reason: String,
}

fn persona_scope(state: &DaemonState, requested: &str) -> String {
    if requested.trim().is_empty() {
        state.manager.lock().unwrap().config.active_persona_scope()
    } else {
        crate::config::persona_scope_name(requested)
    }
}

fn settings(
    state: &DaemonState,
) -> std::result::Result<crate::config::RealContextPluginSettings, ApiError> {
    let config = state.manager.lock().unwrap().config.clone();
    affection::settings_from_config(&config)
        .map_err(|error| ApiError::internal(safe_error_message(&error)))
}

fn check_ids(account: &str, user: Option<&str>) -> std::result::Result<(), ApiError> {
    if !valid_qq_id(account) || user.is_some_and(|value| !valid_qq_id(value)) {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "account and user must be numeric QQ ids",
        ));
    }
    Ok(())
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

pub(in crate::web) async fn dash_affection_scopes(
    State(state): State<DaemonState>,
    headers: HeaderMap,
) -> std::result::Result<Json<Value>, ApiError> {
    require_auth(&headers, &state)?;
    let store = state.state_store.clone();
    let mut result = blocking(move || affection::dashboard_scopes(&store)).await?;
    result["active_persona"] = json!(persona_scope(&state, ""));
    result["connected"] = json!(state
        .platforms
        .onebot
        .lock()
        .unwrap()
        .connected_accounts()
        .into_iter()
        .map(|account| account.to_string())
        .collect::<Vec<_>>());
    Ok(Json(result))
}

pub(in crate::web) async fn dash_affection_items(
    State(state): State<DaemonState>,
    headers: HeaderMap,
    Query(query): Query<ListQuery>,
) -> std::result::Result<Json<Value>, ApiError> {
    require_auth(&headers, &state)?;
    check_ids(&query.account, None)?;
    let scope = persona_scope(&state, &query.persona);
    let settings = settings(&state)?;
    let store = state.state_store.clone();
    let account = query.account.clone();
    let result =
        blocking(move || affection::dashboard_list(&store, &settings, &account, &scope)).await?;
    Ok(Json(result))
}

pub(in crate::web) async fn dash_affection_item(
    State(state): State<DaemonState>,
    headers: HeaderMap,
    Path(user): Path<String>,
    Query(query): Query<ListQuery>,
) -> std::result::Result<Json<Value>, ApiError> {
    require_auth(&headers, &state)?;
    check_ids(&query.account, Some(&user))?;
    let scope = persona_scope(&state, &query.persona);
    let settings = settings(&state)?;
    let store = state.state_store.clone();
    let account = query.account.clone();
    let profile =
        blocking(move || affection::dashboard_profile(&store, &settings, &account, &scope, &user))
            .await?;
    profile
        .map(|profile| Json(json!({ "ok": true, "profile": profile })))
        .ok_or_else(|| ApiError::new(StatusCode::NOT_FOUND, "profile not found"))
}

pub(in crate::web) async fn dash_affection_patch(
    State(state): State<DaemonState>,
    headers: HeaderMap,
    Path(user): Path<String>,
    Query(query): Query<ListQuery>,
    Json(body): Json<PatchBody>,
) -> std::result::Result<Json<Value>, ApiError> {
    require_mutation(&headers, &state)?;
    check_ids(&query.account, Some(&user))?;
    if body.score.is_some_and(|value| !value.is_finite()) {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "score must be finite",
        ));
    }
    let scope = persona_scope(&state, &query.persona);
    let settings = settings(&state)?;
    let store = state.state_store.clone();
    let account = query.account.clone();
    let patch = affection::DashboardPatch {
        score: body.score,
        note: body.note,
        tags: body.tags,
        auto_update_enabled: body.auto_update_enabled,
        clear_events: body.clear_events,
        reason: body.reason,
    };
    let profile = blocking(move || {
        affection::dashboard_update(&store, &settings, &account, &scope, &user, patch)
    })
    .await?;
    profile
        .map(|profile| Json(json!({ "ok": true, "profile": profile })))
        .ok_or_else(|| ApiError::new(StatusCode::NOT_FOUND, "profile not found"))
}

pub(in crate::web) async fn dash_affection_delete(
    State(state): State<DaemonState>,
    headers: HeaderMap,
    Path(user): Path<String>,
    Query(query): Query<ListQuery>,
) -> std::result::Result<Json<Value>, ApiError> {
    require_mutation(&headers, &state)?;
    check_ids(&query.account, Some(&user))?;
    let scope = persona_scope(&state, &query.persona);
    let store = state.state_store.clone();
    let account = query.account.clone();
    let deleted =
        blocking(move || affection::dashboard_delete(&store, &account, &scope, &user)).await?;
    if !deleted {
        return Err(ApiError::new(StatusCode::NOT_FOUND, "profile not found"));
    }
    Ok(Json(json!({ "ok": true })))
}

/* ── 情绪 ─────────────────────────────────────────────── */

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::web) struct EmotionSetBody {
    valence: f64,
    arousal: f64,
    #[serde(default)]
    reason: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::web) struct EmotionResetBody {
    #[serde(default)]
    clear_events: bool,
}

pub(in crate::web) async fn dash_emotion_state(
    State(state): State<DaemonState>,
    headers: HeaderMap,
    Query(query): Query<ListQuery>,
) -> std::result::Result<Json<Value>, ApiError> {
    require_auth(&headers, &state)?;
    check_ids(&query.account, None)?;
    let scope = persona_scope(&state, &query.persona);
    let settings = settings(&state)?;
    let store = state.state_store.clone();
    let paths = state.paths.clone();
    let account = query.account.clone();
    let result =
        blocking(move || emotion::dashboard_state(&store, &paths, &settings, &account, &scope))
            .await?;
    Ok(Json(result))
}

pub(in crate::web) async fn dash_emotion_set(
    State(state): State<DaemonState>,
    headers: HeaderMap,
    Query(query): Query<ListQuery>,
    Json(body): Json<EmotionSetBody>,
) -> std::result::Result<Json<Value>, ApiError> {
    require_mutation(&headers, &state)?;
    check_ids(&query.account, None)?;
    if !body.valence.is_finite() || !body.arousal.is_finite() {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "valence and arousal must be finite",
        ));
    }
    let scope = persona_scope(&state, &query.persona);
    let settings = settings(&state)?;
    let store = state.state_store.clone();
    let account = query.account.clone();
    let updated = blocking(move || {
        emotion::dashboard_set(
            &store,
            &settings,
            &account,
            &scope,
            body.valence,
            body.arousal,
            &body.reason,
        )
    })
    .await?;
    Ok(Json(
        json!({ "ok": true, "valence": updated.valence, "arousal": updated.arousal, "label": emotion::label_for(updated.valence, updated.arousal) }),
    ))
}

pub(in crate::web) async fn dash_emotion_reset(
    State(state): State<DaemonState>,
    headers: HeaderMap,
    Query(query): Query<ListQuery>,
    Json(body): Json<EmotionResetBody>,
) -> std::result::Result<Json<Value>, ApiError> {
    require_mutation(&headers, &state)?;
    check_ids(&query.account, None)?;
    let scope = persona_scope(&state, &query.persona);
    let store = state.state_store.clone();
    let account = query.account.clone();
    blocking(move || emotion::dashboard_reset(&store, &account, &scope, body.clear_events)).await?;
    Ok(Json(json!({ "ok": true })))
}
