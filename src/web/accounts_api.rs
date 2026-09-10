//! 账号与邀请码接口(09-10 分层架构阶段 5,多用户)。
//!
//! 「防君子不防小人」:账号只解决会话别混在一起与按人统计。管理台
//! (`/api/admin/*`)只有管理员进得去;`/api/account` 是每个人改自己的。

use crate::web::*;

fn account_json(account: &crate::state::Account) -> Value {
    json!({
        "id": account.id,
        "username": account.username,
        "display_name": account.display_name,
        "role": account.role,
        "admin": account.is_admin(),
        "disabled": account.disabled,
        "created_at": account.created_at,
        "last_login_at": account.last_login_at,
    })
}

pub(in crate::web) fn identity_json(identity: &WebIdentity) -> Value {
    json!({
        "account_id": identity.account_id,
        "username": identity.username,
        "display_name": identity.display_name,
        "admin": identity.admin,
    })
}

/// 某个登录者的档案文件:管理员(含只填口令的机器级管理员)是属主档案,
/// 成员是自己家目录里的 profile.md。
pub(in crate::web) fn profile_file_for(paths: &MiyuPaths, identity: &WebIdentity) -> PathBuf {
    if identity.admin {
        paths.profile_file()
    } else {
        paths.user_profile_file(&identity.username)
    }
}

pub(in crate::web) const MAX_PROFILE_CHARS: usize = 20_000;

/// 自己是谁 + 档案内容。
pub(in crate::web) async fn account_me(
    State(state): State<DaemonState>,
    headers: HeaderMap,
) -> std::result::Result<Response, ApiError> {
    let identity = require_identity(&headers, &state)?;
    let profile =
        std::fs::read_to_string(profile_file_for(&state.paths, &identity)).unwrap_or_default();
    Ok(Json(json!({
        "account": identity_json(&identity),
        "multi_user": state.auth.required(),
        "profile": profile,
    }))
    .into_response())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::web) struct UpdateAccountRequest {
    #[serde(default)]
    pub(in crate::web) display_name: Option<String>,
    #[serde(default)]
    pub(in crate::web) current_password: Option<String>,
    #[serde(default)]
    pub(in crate::web) password: Option<String>,
    /// 「希望 AI 如何认知你」——写进自己的 profile.md;只在 WebUI/终端等属主类
    /// 入口注入提示词,通讯平台不看。
    #[serde(default)]
    pub(in crate::web) profile: Option<String>,
}

/// 改自己的显示名/密码/档案。拿 `-p` 口令登录的机器级管理员没有账号行,
/// 密码是命令行给的,这里改不了;档案照改(那是属主档案)。
pub(in crate::web) async fn account_update(
    State(state): State<DaemonState>,
    headers: HeaderMap,
    Json(request): Json<UpdateAccountRequest>,
) -> std::result::Result<Response, ApiError> {
    require_mutation(&headers, &state)?;
    let identity = require_identity(&headers, &state)?;
    if let Some(profile) = request.profile.as_deref() {
        if profile.chars().count() > MAX_PROFILE_CHARS {
            return Err(ApiError::new(
                StatusCode::BAD_REQUEST,
                "profile is too long",
            ));
        }
        let path = profile_file_for(&state.paths, &identity);
        if let Some(parent) = path.parent() {
            crate::paths::ensure_private_dir(parent).map_err(ApiError::internal)?;
        }
        std::fs::write(&path, profile.trim_end().to_string() + "\n").map_err(ApiError::internal)?;
    }
    if identity.account_id.is_empty() {
        if request.display_name.is_none() && request.password.is_none() {
            return Ok(Json(json!({ "account": identity_json(&identity) })).into_response());
        }
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "this login has no account row; sign in with a username to edit it",
        ));
    }
    if let Some(display_name) = request.display_name.as_deref() {
        let display_name = display_name.trim();
        if display_name.is_empty() || display_name.chars().count() > 64 {
            return Err(ApiError::new(
                StatusCode::BAD_REQUEST,
                "display name must be 1 to 64 characters",
            ));
        }
        state
            .state_store
            .set_account_display_name(&identity.account_id, display_name)
            .map_err(ApiError::internal)?;
    }
    if let Some(password) = request.password.as_deref() {
        let account = state
            .state_store
            .account_by_id(&identity.account_id)
            .map_err(ApiError::internal)?
            .ok_or_else(|| ApiError::new(StatusCode::NOT_FOUND, "account not found"))?;
        let current = request.current_password.as_deref().unwrap_or_default();
        if !crate::state::verify_password(&account.password_hash, current) {
            return Err(ApiError::new(
                StatusCode::FORBIDDEN,
                "current password is wrong",
            ));
        }
        state
            .state_store
            .set_account_password(&identity.account_id, password)
            .map_err(|error| ApiError::new(StatusCode::BAD_REQUEST, error.to_string()))?;
    }
    let account = state
        .state_store
        .account_by_id(&identity.account_id)
        .map_err(ApiError::internal)?
        .ok_or_else(|| ApiError::new(StatusCode::NOT_FOUND, "account not found"))?;
    Ok(Json(json!({ "account": account_json(&account) })).into_response())
}

pub(in crate::web) async fn admin_list_accounts(
    State(state): State<DaemonState>,
    headers: HeaderMap,
) -> std::result::Result<Response, ApiError> {
    require_admin(&headers, &state)?;
    let accounts = state
        .state_store
        .list_accounts()
        .map_err(ApiError::internal)?
        .iter()
        .map(account_json)
        .collect::<Vec<_>>();
    Ok(Json(json!({ "accounts": accounts })).into_response())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::web) struct AdminUpdateAccountRequest {
    #[serde(default)]
    pub(in crate::web) disabled: Option<bool>,
    #[serde(default)]
    pub(in crate::web) display_name: Option<String>,
    #[serde(default)]
    pub(in crate::web) password: Option<String>,
}

/// 管理员停用/恢复成员、重设密码、改显示名。不能停用自己,也不能停掉
/// 最后一个管理员——否则没人能再进管理台。
pub(in crate::web) async fn admin_update_account(
    State(state): State<DaemonState>,
    headers: HeaderMap,
    Path(account_id): Path<String>,
    Json(request): Json<AdminUpdateAccountRequest>,
) -> std::result::Result<Response, ApiError> {
    let identity = require_admin_mutation(&headers, &state)?;
    let account = state
        .state_store
        .account_by_id(&account_id)
        .map_err(ApiError::internal)?
        .ok_or_else(|| ApiError::new(StatusCode::NOT_FOUND, "account not found"))?;
    if let Some(disabled) = request.disabled {
        if disabled && account.id == identity.account_id {
            return Err(ApiError::new(
                StatusCode::BAD_REQUEST,
                "you cannot disable yourself",
            ));
        }
        if disabled && account.is_admin() {
            let admins_left = state
                .state_store
                .list_accounts()
                .map_err(ApiError::internal)?
                .iter()
                .filter(|item| item.is_admin() && !item.disabled && item.id != account.id)
                .count();
            if admins_left == 0 {
                return Err(ApiError::new(
                    StatusCode::BAD_REQUEST,
                    "cannot disable the last admin",
                ));
            }
        }
        state
            .state_store
            .set_account_disabled(&account.id, disabled)
            .map_err(ApiError::internal)?;
    }
    if let Some(display_name) = request.display_name.as_deref() {
        let display_name = display_name.trim();
        if display_name.is_empty() || display_name.chars().count() > 64 {
            return Err(ApiError::new(
                StatusCode::BAD_REQUEST,
                "display name must be 1 to 64 characters",
            ));
        }
        state
            .state_store
            .set_account_display_name(&account.id, display_name)
            .map_err(ApiError::internal)?;
    }
    if let Some(password) = request.password.as_deref() {
        state
            .state_store
            .set_account_password(&account.id, password)
            .map_err(|error| ApiError::new(StatusCode::BAD_REQUEST, error.to_string()))?;
    }
    let account = state
        .state_store
        .account_by_id(&account.id)
        .map_err(ApiError::internal)?
        .ok_or_else(|| ApiError::new(StatusCode::NOT_FOUND, "account not found"))?;
    Ok(Json(json!({ "account": account_json(&account) })).into_response())
}

fn invite_json(invite: &crate::state::Invite, now: &str) -> Value {
    let status = if invite.used_by.is_some() {
        "used"
    } else if invite.expires_at.as_str() <= now {
        "expired"
    } else {
        "open"
    };
    json!({
        "id": invite.code_hash,
        "created_by": invite.created_by,
        "created_at": invite.created_at,
        "expires_at": invite.expires_at,
        "used_by": invite.used_by,
        "used_at": invite.used_at,
        "role": invite.role,
        "status": status,
    })
}

pub(in crate::web) async fn admin_list_invites(
    State(state): State<DaemonState>,
    headers: HeaderMap,
) -> std::result::Result<Response, ApiError> {
    require_admin(&headers, &state)?;
    let now = chrono::Utc::now().to_rfc3339();
    let invites = state
        .state_store
        .list_invites()
        .map_err(ApiError::internal)?
        .iter()
        .map(|invite| invite_json(invite, &now))
        .collect::<Vec<_>>();
    Ok(Json(json!({ "invites": invites })).into_response())
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub(in crate::web) struct CreateInviteRequest {
    #[serde(default)]
    pub(in crate::web) days: Option<i64>,
}

/// 生成一次性邀请码:明文只在这一次响应里出现。
pub(in crate::web) async fn admin_create_invite(
    State(state): State<DaemonState>,
    headers: HeaderMap,
    request: Option<Json<CreateInviteRequest>>,
) -> std::result::Result<Response, ApiError> {
    let identity = require_admin_mutation(&headers, &state)?;
    let request = request.map(|Json(request)| request).unwrap_or_default();
    let created_by = if identity.account_id.is_empty() {
        crate::state::BOOTSTRAP_ADMIN_USERNAME.to_string()
    } else {
        identity.account_id.clone()
    };
    let (code, invite) = state
        .state_store
        .create_invite(&created_by, request.days, crate::state::ROLE_MEMBER)
        .map_err(ApiError::internal)?;
    let now = chrono::Utc::now().to_rfc3339();
    Ok((
        StatusCode::CREATED,
        Json(json!({ "code": code, "invite": invite_json(&invite, &now) })),
    )
        .into_response())
}

pub(in crate::web) async fn admin_delete_invite(
    State(state): State<DaemonState>,
    headers: HeaderMap,
    Path(invite_id): Path<String>,
) -> std::result::Result<Response, ApiError> {
    require_admin_mutation(&headers, &state)?;
    let deleted = state
        .state_store
        .delete_invite(&invite_id)
        .map_err(ApiError::internal)?;
    if deleted {
        Ok(StatusCode::NO_CONTENT.into_response())
    } else {
        Err(ApiError::new(StatusCode::NOT_FOUND, "invite not found"))
    }
}

/// 总表按人拆(管理台「数据统计」):每个账号在范围内的汇总,空串 = 管理员/
/// 终端/平台等没有账号的来源。
pub(in crate::web) async fn admin_usage_accounts(
    State(state): State<DaemonState>,
    headers: HeaderMap,
    Query(query): Query<UsageStatsQuery>,
) -> std::result::Result<Response, ApiError> {
    require_admin(&headers, &state)?;
    let range = crate::state::UsageRange::parse(query.range.as_deref().unwrap_or("1d"));
    let config = state.manager.lock().unwrap().config.clone();
    crate::models_cache::ensure_active_metadata(&state.paths, &config);
    let store = state.state_store.clone();
    let stats = tokio::task::spawn_blocking(move || {
        store.usage_stats_for_account(range, Some(&config), None)
    })
    .await
    .map_err(ApiError::internal)?
    .map_err(ApiError::internal)?;
    let names: HashMap<String, (String, String)> = state
        .state_store
        .list_accounts()
        .map_err(ApiError::internal)?
        .into_iter()
        .map(|account| (account.id, (account.username, account.display_name)))
        .collect();
    let accounts = stats
        .accounts
        .iter()
        .map(|entry| {
            let (username, display_name) = names.get(&entry.acct).cloned().unwrap_or_default();
            let mut value = serde_json::to_value(entry).unwrap_or_else(|_| json!({}));
            value["username"] = json!(username);
            value["display_name"] = json!(display_name);
            value
        })
        .collect::<Vec<_>>();
    Ok(Json(json!({
        "ok": true,
        "range": stats.range,
        "totals": stats.totals,
        "accounts": accounts,
    }))
    .into_response())
}
