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

/// bootstrap 里的账号块:身份 + 当前人格 + 引导是否待做。
pub(in crate::web) fn account_bootstrap_json(state: &DaemonState, identity: &WebIdentity) -> Value {
    let mut value = identity_json(identity);
    if !identity.admin && !identity.username.is_empty() {
        let settings = member_persona::load_settings(&state.paths, &identity.username);
        let active = member_persona::active_persona(&state.paths, &identity.username);
        value["oobe_pending"] = json!(!settings.oobe_done);
        value["persona"] = match active {
            Some(persona) => {
                json!({ "slug": persona.slug, "name": persona.meta.name, "private": true })
            }
            None => json!({ "slug": null, "name": "Miyu", "private": false }),
        };
    } else {
        value["oobe_pending"] = json!(false);
        value["persona"] = json!({ "slug": null, "name": "Miyu", "private": false });
    }
    value
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

// ── 成员的私有人格(阶段 8,OOBE) ──

fn member_username(identity: &WebIdentity) -> std::result::Result<String, ApiError> {
    if identity.admin || identity.username.is_empty() {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "private personas are for member accounts; the admin edits the shared persona in settings",
        ));
    }
    Ok(identity.username.clone())
}

fn plugin_label(id: &str) -> (&'static str, &'static str) {
    match id {
        "files" => ("文件", "读写工作区文件"),
        "usage_query" => ("用量查询", "对话里问用了多少 token"),
        "alarm" => ("闹钟", "定时提醒"),
        "exchange_rate" => ("汇率", "货币换算"),
        "archlinux" => ("Arch Linux", "AUR 查询、Arch 新闻"),
        "api_quota" => ("API 额度", "查供应商余额"),
        "print_image" => ("视觉分析", "看图片和截图"),
        "memes" => ("表情包", "用表情包回复"),
        "platform_outreach" => ("外发", "从对话里给通讯平台发消息"),
        "web_images" => ("搜图", "网络找图"),
        "deep_research" => ("深度研究", "多轮检索写报告"),
        "image_generation" => ("生图", "AI 画图"),
        "knowledge_base" => ("知识库", "自己的资料库,对话里能查"),
        "package_advisor" => ("软件推荐", "推荐与安装软件"),
        "diagnostics" => ("系统诊断", "看系统信息"),
        "ledger" => ("记账", "记账本"),
        "scripts" => ("脚本工具", "逐个勾选"),
        _ => ("", ""),
    }
}

fn persona_json(persona: &member_persona::PrivatePersona) -> Value {
    let scope = persona.scope();
    json!({
        "slug": persona.slug,
        "name": persona.meta.name,
        "description": persona.meta.description,
        "board_title": persona.meta.board_title,
        "board_subtitle": persona.meta.board_subtitle,
        "created_at": persona.meta.created_at,
        "memory": persona.manifest.subsystems.memory,
        "plugins": persona.manifest.plugins.enabled.clone().unwrap_or_default(),
        "scripts": persona.manifest.plugins.scripts.clone(),
        "avatar_url": persona.avatar_path().map(|_| format!("/api/persona/avatar?scope={scope}")),
        "board_image_url": persona.board_path().map(|_| format!("/api/persona/avatar?scope={scope}&board=1")),
        "scope": scope,
    })
}

/// 成员的人格列表 + 可勾的插件 + 当前用的。
pub(in crate::web) async fn account_personas(
    State(state): State<DaemonState>,
    headers: HeaderMap,
) -> std::result::Result<Response, ApiError> {
    let identity = require_identity(&headers, &state)?;
    let config = state.manager.lock().unwrap().config.clone();
    let options = member_persona::member_selectable_plugins(&config)
        .iter()
        .map(|id| {
            let (label, hint) = plugin_label(id);
            json!({ "id": id, "label": if label.is_empty() { id.as_str() } else { label }, "hint": hint })
        })
        .collect::<Vec<_>>();
    let scripts = crate::tools::list_global_scripts(&state.paths)
        .into_iter()
        .map(
            |(id, display, description)| json!({ "id": id, "label": display, "hint": description }),
        )
        .collect::<Vec<_>>();
    if identity.admin || identity.username.is_empty() {
        return Ok(Json(json!({
            "personas": [],
            "active": null,
            "member_personas": false,
            "plugins": options,
            "scripts": scripts,
        }))
        .into_response());
    }
    let personas = member_persona::list_personas(&state.paths, &identity.username)
        .map_err(ApiError::internal)?;
    let settings = member_persona::load_settings(&state.paths, &identity.username);
    Ok(Json(json!({
        "personas": personas.iter().map(persona_json).collect::<Vec<_>>(),
        "active": settings.active_persona,
        "member_personas": config.accounts.member_personas,
        "plugins": options,
        "scripts": scripts,
        "prompt": std::fs::read_to_string(profile_file_for(&state.paths, &identity)).unwrap_or_default(),
    }))
    .into_response())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::web) struct PersonaRequest {
    #[serde(default)]
    pub(in crate::web) slug: Option<String>,
    pub(in crate::web) name: String,
    #[serde(default)]
    pub(in crate::web) description: String,
    pub(in crate::web) prompt: String,
    #[serde(default)]
    pub(in crate::web) board_title: String,
    #[serde(default)]
    pub(in crate::web) board_subtitle: String,
    #[serde(default = "default_true_flag")]
    pub(in crate::web) memory: bool,
    #[serde(default)]
    pub(in crate::web) plugins: Option<Vec<String>>,
    /// 勾了哪些脚本;None = 全部。
    #[serde(default)]
    pub(in crate::web) scripts: Option<Vec<String>>,
    /// 建完就切成当前人格(引导里默认 true)。
    #[serde(default = "default_true_flag")]
    pub(in crate::web) activate: bool,
}

fn default_true_flag() -> bool {
    true
}

fn random_slug() -> String {
    format!("p{}", hex::encode(rand::random::<[u8; 3]>()))
}

pub(in crate::web) async fn account_persona_create(
    State(state): State<DaemonState>,
    headers: HeaderMap,
    Json(request): Json<PersonaRequest>,
) -> std::result::Result<Response, ApiError> {
    require_mutation(&headers, &state)?;
    let identity = require_identity(&headers, &state)?;
    let username = member_username(&identity)?;
    let config = state.manager.lock().unwrap().config.clone();
    if !config.accounts.member_personas {
        return Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "the admin has turned off member personas",
        ));
    }
    let slug = request
        .slug
        .as_deref()
        .map(str::trim)
        .filter(|slug| !slug.is_empty())
        .map(str::to_string)
        .unwrap_or_else(random_slug);
    if member_persona::load_persona(&state.paths, &username, &slug)
        .map_err(|error| ApiError::new(StatusCode::BAD_REQUEST, error.to_string()))?
        .is_some()
    {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "a persona with this id already exists",
        ));
    }
    let plugins = request
        .plugins
        .clone()
        .unwrap_or_else(|| config.accounts.allowed_member_plugins());
    let draft = member_persona::PersonaDraft {
        name: &request.name,
        description: &request.description,
        prompt: &request.prompt,
        board_title: &request.board_title,
        board_subtitle: &request.board_subtitle,
        memory: request.memory,
        plugins,
        scripts: request.scripts.clone(),
    };
    let persona =
        member_persona::create_or_update_persona(&config, &state.paths, &username, &slug, &draft)
            .map_err(|error| ApiError::new(StatusCode::BAD_REQUEST, error.to_string()))?;
    if request.activate {
        retarget_member_sessions(&state, &identity, &persona.scope());
        let mut settings = member_persona::load_settings(&state.paths, &username);
        settings.active_persona = Some(slug.clone());
        // 自己建了人格就不用再引导了
        settings.oobe_done = true;
        member_persona::save_settings(&state.paths, &username, &settings)
            .map_err(ApiError::internal)?;
    }
    Ok((
        StatusCode::CREATED,
        Json(json!({ "persona": persona_json(&persona) })),
    )
        .into_response())
}

pub(in crate::web) async fn account_persona_update(
    State(state): State<DaemonState>,
    headers: HeaderMap,
    Path(slug): Path<String>,
    Json(request): Json<PersonaRequest>,
) -> std::result::Result<Response, ApiError> {
    require_mutation(&headers, &state)?;
    let identity = require_identity(&headers, &state)?;
    let username = member_username(&identity)?;
    let config = state.manager.lock().unwrap().config.clone();
    let existing = member_persona::load_persona(&state.paths, &username, &slug)
        .map_err(|error| ApiError::new(StatusCode::BAD_REQUEST, error.to_string()))?
        .ok_or_else(|| ApiError::new(StatusCode::NOT_FOUND, "persona not found"))?;
    let plugins = request.plugins.clone().unwrap_or_else(|| {
        existing
            .manifest
            .plugins
            .enabled
            .clone()
            .unwrap_or_default()
    });
    let draft = member_persona::PersonaDraft {
        name: &request.name,
        description: &request.description,
        prompt: &request.prompt,
        board_title: &request.board_title,
        board_subtitle: &request.board_subtitle,
        memory: request.memory,
        plugins,
        scripts: request
            .scripts
            .clone()
            .or_else(|| existing.manifest.plugins.scripts.clone()),
    };
    let persona =
        member_persona::create_or_update_persona(&config, &state.paths, &username, &slug, &draft)
            .map_err(|error| ApiError::new(StatusCode::BAD_REQUEST, error.to_string()))?;
    Ok(Json(json!({ "persona": persona_json(&persona) })).into_response())
}

/// 某个人格的提示词全文(编辑用;列表里不带,免得每次都拉几十 KB)。
pub(in crate::web) async fn account_persona_prompt(
    State(state): State<DaemonState>,
    headers: HeaderMap,
    Path(slug): Path<String>,
) -> std::result::Result<Response, ApiError> {
    let identity = require_identity(&headers, &state)?;
    let username = member_username(&identity)?;
    let persona = member_persona::load_persona(&state.paths, &username, &slug)
        .map_err(|error| ApiError::new(StatusCode::BAD_REQUEST, error.to_string()))?
        .ok_or_else(|| ApiError::new(StatusCode::NOT_FOUND, "persona not found"))?;
    let prompt = persona.prompt().map_err(ApiError::internal)?;
    Ok(Json(json!({ "prompt": prompt })).into_response())
}

pub(in crate::web) async fn account_persona_delete(
    State(state): State<DaemonState>,
    headers: HeaderMap,
    Path(slug): Path<String>,
) -> std::result::Result<Response, ApiError> {
    require_mutation(&headers, &state)?;
    let identity = require_identity(&headers, &state)?;
    let username = member_username(&identity)?;
    member_persona::delete_persona(&state.paths, &username, &slug)
        .map_err(|error| ApiError::new(StatusCode::BAD_REQUEST, error.to_string()))?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

/// 头像 / 看板图:请求体就是图片字节(`?board=1` 是看板);DELETE 去掉。
pub(in crate::web) async fn account_persona_image(
    State(state): State<DaemonState>,
    headers: HeaderMap,
    Path(slug): Path<String>,
    Query(query): Query<HashMap<String, String>>,
    body: Bytes,
) -> std::result::Result<Response, ApiError> {
    require_mutation(&headers, &state)?;
    let identity = require_identity(&headers, &state)?;
    let username = member_username(&identity)?;
    let persona = member_persona::load_persona(&state.paths, &username, &slug)
        .map_err(|error| ApiError::new(StatusCode::BAD_REQUEST, error.to_string()))?
        .ok_or_else(|| ApiError::new(StatusCode::NOT_FOUND, "persona not found"))?;
    let stem = if query.contains_key("board") {
        "board"
    } else {
        "avatar"
    };
    member_persona::store_image(&persona.dir, stem, &body)
        .map_err(|error| ApiError::new(StatusCode::BAD_REQUEST, error.to_string()))?;
    let persona = member_persona::load_persona(&state.paths, &username, &slug)
        .map_err(ApiError::internal)?
        .ok_or_else(|| ApiError::new(StatusCode::NOT_FOUND, "persona not found"))?;
    Ok(Json(json!({ "persona": persona_json(&persona) })).into_response())
}

pub(in crate::web) async fn account_persona_image_delete(
    State(state): State<DaemonState>,
    headers: HeaderMap,
    Path(slug): Path<String>,
    Query(query): Query<HashMap<String, String>>,
) -> std::result::Result<Response, ApiError> {
    require_mutation(&headers, &state)?;
    let identity = require_identity(&headers, &state)?;
    let username = member_username(&identity)?;
    let persona = member_persona::load_persona(&state.paths, &username, &slug)
        .map_err(|error| ApiError::new(StatusCode::BAD_REQUEST, error.to_string()))?
        .ok_or_else(|| ApiError::new(StatusCode::NOT_FOUND, "persona not found"))?;
    member_persona::remove_image(
        &persona.dir,
        if query.contains_key("board") {
            "board"
        } else {
            "avatar"
        },
    );
    Ok(StatusCode::NO_CONTENT.into_response())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::web) struct ActivePersonaRequest {
    /// None / null = 共享 Miyu。
    #[serde(default)]
    pub(in crate::web) slug: Option<String>,
    /// 顺手把引导标成做完。
    #[serde(default)]
    pub(in crate::web) oobe_done: Option<bool>,
}

pub(in crate::web) async fn account_active_persona(
    State(state): State<DaemonState>,
    headers: HeaderMap,
    Json(request): Json<ActivePersonaRequest>,
) -> std::result::Result<Response, ApiError> {
    require_mutation(&headers, &state)?;
    let identity = require_identity(&headers, &state)?;
    let username = member_username(&identity)?;
    if let Some(slug) = request.slug.as_deref() {
        if member_persona::load_persona(&state.paths, &username, slug)
            .map_err(|error| ApiError::new(StatusCode::BAD_REQUEST, error.to_string()))?
            .is_none()
        {
            return Err(ApiError::new(StatusCode::NOT_FOUND, "persona not found"));
        }
    }
    let mut settings = member_persona::load_settings(&state.paths, &username);
    settings.active_persona = request.slug.clone();
    if let Some(done) = request.oobe_done {
        settings.oobe_done = done;
    }
    member_persona::save_settings(&state.paths, &username, &settings)
        .map_err(ApiError::internal)?;
    let scope = match request.slug.as_deref() {
        Some(slug) => member_persona::load_persona(&state.paths, &username, slug)
            .ok()
            .flatten()
            .map(|persona| persona.scope())
            .unwrap_or_else(|| active_persona_scope(&state)),
        None => active_persona_scope(&state),
    };
    retarget_member_sessions(&state, &identity, &scope);
    Ok(
        Json(json!({ "active": settings.active_persona, "oobe_done": settings.oobe_done }))
            .into_response(),
    )
}

/// 切了人格,成员那些还没聊过的空会话跟着换人格;一个空的都没有就新建一条,
/// 这样「新建了 Eris,回到聊天还是 Miyu」不会再发生(09-10 反馈)。有历史的
/// 会话保持原人格——它们的对话是按那个人格聊出来的。
fn retarget_member_sessions(state: &DaemonState, identity: &WebIdentity, scope: &str) {
    let owner = identity.owner_key();
    if owner.is_empty() {
        return;
    }
    let Ok(store) = state.stores.for_owner(owner) else {
        return;
    };
    let Ok(sessions) = store.list_owner_sessions(owner) else {
        return;
    };
    let mut has_empty_on_scope = false;
    for overview in sessions.iter().filter(|overview| overview.turn_count == 0) {
        if overview.record.persona == scope {
            has_empty_on_scope = true;
            continue;
        }
        if store
            .set_session_persona(&overview.record.session_id, scope)
            .is_ok()
        {
            has_empty_on_scope = true;
            state.events.publish(
                "session.updated",
                json!({ "session_id": overview.record.session_id, "persona": scope }),
            );
        }
    }
    if has_empty_on_scope {
        return;
    }
    if let Ok(record) =
        store.create_session_for_owner(scope, "", crate::state::USER_SESSION_KIND, None, owner)
    {
        state.stores.note_session_owner(&record.session_id, owner);
        publish_session_created(state, &record);
    }
}
