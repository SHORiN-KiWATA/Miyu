//! 网页上查看与清理 QQ 群历史。
//!
//! `valid_qq_id` 挡的是路径/查询注入：群号来自 URL，会直接进 SQL 的绑定参数与
//! 目录名。

use crate::web::*;

pub(in crate::web) const QQ_GROUP_MANAGEMENT_PLUGIN_ID: &str = "qq_group_management";

pub(in crate::web) const QQ_GROUP_MANAGEMENT_PLATFORM: &str = "onebot";

#[derive(Deserialize)]
pub(in crate::web) struct QqGroupHistoryQuery {
    pub(in crate::web) account_id: String,
    pub(in crate::web) group_id: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::web) struct QqGroupHistoryClearRequest {
    pub(in crate::web) account_id: String,
    pub(in crate::web) group_id: String,
    pub(in crate::web) kind: String,
}

pub(in crate::web) fn qq_group_scope(
    account_id: &str,
    group_id: &str,
) -> std::result::Result<PlatformPluginScopeKey, ApiError> {
    if !valid_qq_id(account_id) || !valid_qq_id(group_id) {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "account_id and group_id must be numeric QQ ids",
        ));
    }
    Ok(PlatformPluginScopeKey {
        plugin_id: QQ_GROUP_MANAGEMENT_PLUGIN_ID.to_string(),
        platform: QQ_GROUP_MANAGEMENT_PLATFORM.to_string(),
        account_id: account_id.to_string(),
        conversation_kind: "group".to_string(),
        conversation_id: group_id.to_string(),
    })
}

pub(in crate::web) fn valid_qq_id(value: &str) -> bool {
    (5..=12).contains(&value.len()) && value.bytes().all(|byte| byte.is_ascii_digit())
}

pub(in crate::web) async fn qq_group_history_http(
    State(state): State<DaemonState>,
    headers: HeaderMap,
    Query(query): Query<QqGroupHistoryQuery>,
) -> std::result::Result<Response, ApiError> {
    require_admin(&headers, &state)?;
    let scope = qq_group_scope(&query.account_id, &query.group_id)?;
    let offenders = state
        .state_store
        .plugin_get_json::<Value>(&scope, "offender_history")
        .map_err(ApiError::internal)?
        .unwrap_or_else(|| json!({}));
    let kicks = state
        .state_store
        .plugin_get_json::<Value>(&scope, "kick_history")
        .map_err(ApiError::internal)?
        .unwrap_or_else(|| json!([]));
    let connected_accounts = state
        .platforms
        .onebot
        .lock()
        .unwrap()
        .connected_accounts()
        .into_iter()
        .map(|account| account.to_string())
        .collect::<Vec<_>>();
    Ok(Json(json!({
        "account_id": query.account_id,
        "group_id": query.group_id,
        "offenders": offenders.clone(),
        "kicks": kicks.clone(),
        "offender_history": offenders,
        "kick_history": kicks,
        "connected_accounts": connected_accounts,
    }))
    .into_response())
}

pub(in crate::web) async fn qq_group_history_clear_http(
    State(state): State<DaemonState>,
    headers: HeaderMap,
    Json(request): Json<QqGroupHistoryClearRequest>,
) -> std::result::Result<Response, ApiError> {
    require_admin_mutation(&headers, &state)?;
    let scope = qq_group_scope(&request.account_id, &request.group_id)?;
    let key = match request.kind.as_str() {
        "offenders" => "offender_history",
        "kicks" => "kick_history",
        _ => {
            return Err(ApiError::new(
                StatusCode::BAD_REQUEST,
                "kind must be offenders or kicks",
            ))
        }
    };
    state
        .state_store
        .plugin_delete_key(&scope, key)
        .map_err(ApiError::internal)?;
    Ok(Json(json!({ "ok": true })).into_response())
}

pub(in crate::web) async fn qq_group_offender_delete_http(
    State(state): State<DaemonState>,
    headers: HeaderMap,
    Path(user_id): Path<String>,
    Query(query): Query<QqGroupHistoryQuery>,
) -> std::result::Result<Response, ApiError> {
    require_admin_mutation(&headers, &state)?;
    if !valid_qq_id(&user_id) {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "user_id must be a numeric QQ id",
        ));
    }
    let scope = qq_group_scope(&query.account_id, &query.group_id)?;
    state
        .state_store
        .plugin_update_json::<HashMap<String, Value>, _>(&scope, "offender_history", |current| {
            let mut records = current.unwrap_or_default();
            records.remove(&user_id);
            Ok(if records.is_empty() {
                None
            } else {
                Some(records)
            })
        })
        .map_err(ApiError::internal)?;
    Ok(Json(json!({ "ok": true })).into_response())
}
