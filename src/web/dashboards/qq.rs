//! QQ 侧两个面板的 HTTP 面:群聊(消息历史 / 统计 / 撤回 / 删除 / 上下文边界)
//! 与群管(事件流 / 违规者 / 踢人 / 清空)。会话与群号都是数字 id,进 SQL 绑定
//! 参数前先过 `valid_qq_id`。

use crate::platforms::plugins::group_management::dashboard as groups;
use crate::platforms::plugins::message_history::dashboard as history;
use crate::web::*;

#[derive(Deserialize)]
pub(in crate::web) struct AccountQuery {
    #[serde(default)]
    account: String,
}

#[derive(Deserialize)]
pub(in crate::web) struct ConversationQuery {
    account: String,
    #[serde(default = "default_kind")]
    kind: String,
    id: String,
}

#[derive(Deserialize)]
pub(in crate::web) struct MessagesParams {
    account: String,
    #[serde(default = "default_kind")]
    kind: String,
    id: String,
    #[serde(default)]
    q: String,
    #[serde(default)]
    sender: String,
    #[serde(default)]
    since: Option<i64>,
    #[serde(default)]
    until: Option<i64>,
    #[serde(default)]
    recalled: bool,
    #[serde(default)]
    media: bool,
    #[serde(default)]
    before_sent: Option<i64>,
    #[serde(default)]
    before_row: Option<i64>,
    #[serde(default = "default_page")]
    limit: usize,
}

#[derive(Deserialize)]
pub(in crate::web) struct StatsParams {
    account: String,
    #[serde(default = "default_kind")]
    kind: String,
    id: String,
    /// 0 = 全部。
    #[serde(default = "default_days")]
    days: i64,
    #[serde(default = "default_rank_limit")]
    limit: usize,
}

#[derive(Deserialize)]
pub(in crate::web) struct RecallsParams {
    account: String,
    #[serde(default = "default_kind")]
    kind: String,
    id: String,
    #[serde(default = "default_page")]
    limit: usize,
    #[serde(default)]
    offset: usize,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::web) struct DeleteBody {
    account: String,
    /// 空 = 该账号全部群聊。
    #[serde(default)]
    kind: String,
    #[serde(default)]
    id: String,
    #[serde(default)]
    keep_days: Option<u32>,
    #[serde(default)]
    sender: String,
    #[serde(default)]
    since: Option<i64>,
    #[serde(default)]
    until: Option<i64>,
    /// 必须等于会话 id(或整账号删除时等于账号)。
    confirm: String,
}

#[derive(Deserialize)]
pub(in crate::web) struct BoundaryQuery {
    account: String,
    #[serde(default = "default_kind")]
    kind: String,
    id: String,
    #[serde(default)]
    persona: String,
}

#[derive(Deserialize)]
pub(in crate::web) struct GroupQuery {
    account: String,
    group: String,
}

fn default_kind() -> String {
    "group".into()
}
fn default_page() -> usize {
    50
}
fn default_days() -> i64 {
    30
}
fn default_rank_limit() -> usize {
    20
}

fn conversation_key(
    account: &str,
    kind: &str,
    id: &str,
) -> std::result::Result<history::DashConversationKey, ApiError> {
    if !valid_qq_id(account) || !valid_qq_id(id) {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "account and conversation id must be numeric QQ ids",
        ));
    }
    history::conversation_key(account, kind, id)
        .map_err(|error| ApiError::new(StatusCode::BAD_REQUEST, safe_error_message(&error)))
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

fn connected_accounts(state: &DaemonState) -> Vec<String> {
    state
        .platforms
        .onebot
        .lock()
        .unwrap()
        .connected_accounts()
        .into_iter()
        .map(|account| account.to_string())
        .collect()
}

fn group_name(account: &str, group: &str) -> Option<String> {
    let account = account.parse::<i64>().ok()?;
    let group = group.parse::<i64>().ok()?;
    crate::platforms::onebot::cached_group_name(account, group)
}

/// 账号 = 在线账号 ∪ 历史库里出现过的 ∪ 群管记录里出现过的。
pub(in crate::web) async fn dash_qq_accounts(
    State(state): State<DaemonState>,
    headers: HeaderMap,
) -> std::result::Result<Json<Value>, ApiError> {
    require_auth(&headers, &state)?;
    let connected = connected_accounts(&state);
    let paths = state.paths.clone();
    let store = state.state_store.clone();
    let (history_accounts, kv_accounts) = blocking(move || {
        let conversations = history::dashboard_conversations(&paths, None)?;
        let history_accounts: Vec<String> = conversations["accounts"]
            .as_array()
            .map(|items| {
                items
                    .iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        let kv_accounts: Vec<String> = store
            .plugin_scopes(QQ_GROUP_MANAGEMENT_PLUGIN_ID, Some("group"))?
            .into_iter()
            .map(|scope| scope.account_id)
            .collect();
        Ok((history_accounts, kv_accounts))
    })
    .await?;
    let mut all: Vec<String> = connected.iter().cloned().collect();
    all.extend(history_accounts);
    all.extend(kv_accounts);
    all.sort();
    all.dedup();
    Ok(Json(
        json!({ "ok": true, "connected": connected, "accounts": all }),
    ))
}

pub(in crate::web) async fn dash_qq_conversations(
    State(state): State<DaemonState>,
    headers: HeaderMap,
    Query(query): Query<AccountQuery>,
) -> std::result::Result<Json<Value>, ApiError> {
    require_auth(&headers, &state)?;
    let account = query.account.trim().to_string();
    if !account.is_empty() && !valid_qq_id(&account) {
        return Err(ApiError::new(StatusCode::BAD_REQUEST, "invalid account"));
    }
    let paths = state.paths.clone();
    let mut result = blocking(move || {
        history::dashboard_conversations(&paths, (!account.is_empty()).then_some(account.as_str()))
    })
    .await?;
    if let Some(items) = result["conversations"].as_array_mut() {
        for item in items {
            if item["kind"] == "group" {
                let name = group_name(
                    item["account_id"].as_str().unwrap_or(""),
                    item["id"].as_str().unwrap_or(""),
                );
                item["name"] = json!(name);
            }
        }
    }
    Ok(Json(result))
}

pub(in crate::web) async fn dash_qq_messages(
    State(state): State<DaemonState>,
    headers: HeaderMap,
    Query(params): Query<MessagesParams>,
) -> std::result::Result<Json<Value>, ApiError> {
    require_auth(&headers, &state)?;
    let key = conversation_key(&params.account, &params.kind, &params.id)?;
    let before = match (params.before_sent, params.before_row) {
        (Some(sent_at), Some(row_id)) => Some(history::DashHistoryCursor { sent_at, row_id }),
        _ => None,
    };
    let query = history::MessagesQuery {
        key,
        text: params.q,
        sender_id: params.sender,
        since: params.since,
        until: params.until,
        only_recalled: params.recalled,
        only_media: params.media,
        before,
        limit: params.limit,
    };
    let paths = state.paths.clone();
    let result = blocking(move || history::dashboard_messages(&paths, query)).await?;
    Ok(Json(result))
}

pub(in crate::web) async fn dash_qq_stats(
    State(state): State<DaemonState>,
    headers: HeaderMap,
    Query(params): Query<StatsParams>,
) -> std::result::Result<Json<Value>, ApiError> {
    require_auth(&headers, &state)?;
    let key = conversation_key(&params.account, &params.kind, &params.id)?;
    let until = chrono::Utc::now().timestamp();
    let since = if params.days <= 0 {
        0
    } else {
        until - params.days.clamp(1, 3650) * 86_400
    };
    let paths = state.paths.clone();
    let result =
        blocking(move || history::dashboard_stats(&paths, key, since, until, params.limit)).await?;
    Ok(Json(result))
}

pub(in crate::web) async fn dash_qq_recalls(
    State(state): State<DaemonState>,
    headers: HeaderMap,
    Query(params): Query<RecallsParams>,
) -> std::result::Result<Json<Value>, ApiError> {
    require_auth(&headers, &state)?;
    let key = conversation_key(&params.account, &params.kind, &params.id)?;
    let paths = state.paths.clone();
    let result =
        blocking(move || history::dashboard_recalls(&paths, key, params.limit, params.offset))
            .await?;
    Ok(Json(result))
}

/// 删除历史。工具侧那道"必须有在场管理员消息"的门在网页上无法复现,
/// 用 require_mutation + 手输 id 确认代替。
pub(in crate::web) async fn dash_qq_delete(
    State(state): State<DaemonState>,
    headers: HeaderMap,
    Json(body): Json<DeleteBody>,
) -> std::result::Result<Json<Value>, ApiError> {
    require_mutation(&headers, &state)?;
    if !valid_qq_id(&body.account) {
        return Err(ApiError::new(StatusCode::BAD_REQUEST, "invalid account"));
    }
    let key = if body.id.trim().is_empty() {
        if body.confirm.trim() != body.account {
            return Err(ApiError::new(
                StatusCode::BAD_REQUEST,
                "confirm must equal the account id",
            ));
        }
        None
    } else {
        if body.confirm.trim() != body.id.trim() {
            return Err(ApiError::new(
                StatusCode::BAD_REQUEST,
                "confirm must equal the conversation id",
            ));
        }
        Some(conversation_key(&body.account, &body.kind, &body.id)?)
    };
    if body.keep_days == Some(0) {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "keep_days must be positive",
        ));
    }
    let spec = history::DeleteSpec {
        key,
        account_id: body.account,
        keep_days: body.keep_days,
        sender_id: body.sender,
        since: body.since,
        until: body.until,
    };
    let result = history::dashboard_delete(&state.paths, spec)
        .await
        .map_err(|error| ApiError::new(StatusCode::BAD_REQUEST, safe_error_message(&error)))?;
    Ok(Json(result))
}

fn persona_scope(state: &DaemonState, requested: &str) -> String {
    if requested.trim().is_empty() {
        let config = state.manager.lock().unwrap().config.clone();
        config.active_persona_scope()
    } else {
        crate::config::persona_scope_name(requested)
    }
}

pub(in crate::web) async fn dash_qq_boundary(
    State(state): State<DaemonState>,
    headers: HeaderMap,
    Query(query): Query<BoundaryQuery>,
) -> std::result::Result<Json<Value>, ApiError> {
    require_auth(&headers, &state)?;
    let key = conversation_key(&query.account, &query.kind, &query.id)?;
    let scope = persona_scope(&state, &query.persona);
    let mut result = history::dashboard_boundary(&state.paths, key, scope.clone())
        .await
        .map_err(|error| ApiError::internal(safe_error_message(&error)))?;
    result["persona"] = json!(scope);
    Ok(Json(result))
}

pub(in crate::web) async fn dash_qq_reset_context(
    State(state): State<DaemonState>,
    headers: HeaderMap,
    Query(query): Query<BoundaryQuery>,
) -> std::result::Result<Json<Value>, ApiError> {
    require_mutation(&headers, &state)?;
    let key = conversation_key(&query.account, &query.kind, &query.id)?;
    let scope = persona_scope(&state, &query.persona);
    let result = history::dashboard_reset_context(&state.paths, key, scope)
        .await
        .map_err(|error| ApiError::internal(safe_error_message(&error)))?;
    Ok(Json(result))
}

/* ── 群管 ─────────────────────────────────────────────── */

/// 有群管记录的 (账号, 群) 清单,附群名缓存与事件数。
pub(in crate::web) async fn dash_qq_groups(
    State(state): State<DaemonState>,
    headers: HeaderMap,
    Query(query): Query<AccountQuery>,
) -> std::result::Result<Json<Value>, ApiError> {
    require_auth(&headers, &state)?;
    let account = query.account.trim().to_string();
    let store = state.state_store.clone();
    let scopes =
        blocking(move || store.plugin_scopes(QQ_GROUP_MANAGEMENT_PLUGIN_ID, Some("group"))).await?;
    let groups: Vec<Value> = scopes
        .into_iter()
        .filter(|scope| account.is_empty() || scope.account_id == account)
        .map(|scope| {
            json!({
                "account_id": scope.account_id,
                "group_id": scope.conversation_id,
                "name": group_name(&scope.account_id, &scope.conversation_id),
            })
        })
        .collect();
    Ok(Json(
        json!({ "ok": true, "groups": groups, "connected": connected_accounts(&state) }),
    ))
}

pub(in crate::web) async fn dash_qq_management(
    State(state): State<DaemonState>,
    headers: HeaderMap,
    Query(query): Query<GroupQuery>,
) -> std::result::Result<Json<Value>, ApiError> {
    require_auth(&headers, &state)?;
    let scope = qq_group_scope(&query.account, &query.group)?;
    let store = state.state_store.clone();
    let mut result = blocking(move || groups::dashboard_management(&store, &scope)).await?;
    result["group_name"] = json!(group_name(&query.account, &query.group));
    Ok(Json(result))
}

pub(in crate::web) async fn dash_qq_management_clear_events(
    State(state): State<DaemonState>,
    headers: HeaderMap,
    Query(query): Query<GroupQuery>,
) -> std::result::Result<Json<Value>, ApiError> {
    require_mutation(&headers, &state)?;
    let scope = qq_group_scope(&query.account, &query.group)?;
    let store = state.state_store.clone();
    let cleared = blocking(move || groups::dashboard_clear_events(&store, &scope)).await?;
    Ok(Json(json!({ "ok": true, "cleared": cleared })))
}
