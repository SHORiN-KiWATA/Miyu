//! 认证、来源校验与密钥脱敏。
//!
//! 网页端的口令走 cookie，但**本机请求另有一条路**（`is_local_webui_request`）
//! ——用户自己在本机开的界面不该被口令挡住。放宽的前提是同时校验来源
//! （`origin_is_allowed`），否则任意网页都能借浏览器打本机接口。
//!
//! 脱敏这组函数成对出现：发给前端时抹成掩码，写回时按掩码还原。改任何一边都
//! 要同时改另一边，否则用户改个无关选项就会把真密钥覆盖成星号。

use crate::web::*;

pub(in crate::web) const MAX_SECRET_CHARS: usize = 100_000;

pub(in crate::web) const AUTH_COOKIE: &str = "miyu_session";

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::web) struct LoginRequest {
    /// 缺省/空 = 拿 `-p` 口令登录的机器级管理员;填了走账号表。
    #[serde(default)]
    pub(in crate::web) username: Option<String>,
    pub(in crate::web) password: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::web) struct RegisterRequest {
    pub(in crate::web) invite: String,
    pub(in crate::web) username: String,
    #[serde(default)]
    pub(in crate::web) display_name: String,
    pub(in crate::web) password: String,
}

#[derive(Deserialize)]
#[serde(tag = "action", content = "value", rename_all = "snake_case")]
pub(in crate::web) enum SecretMutation {
    Set(String),
    Clear,
}

pub(in crate::web) async fn auth_login(
    State(state): State<DaemonState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(request): Json<LoginRequest>,
) -> std::result::Result<Response, ApiError> {
    if !origin_is_allowed(&headers) {
        return Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "request origin is not allowed",
        ));
    }
    if !state.auth.required() {
        return Ok(StatusCode::NO_CONTENT.into_response());
    }
    if request.password.chars().count() > 1_024 {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "password is too long",
        ));
    }
    let username = request
        .username
        .as_deref()
        .map(str::trim)
        .filter(|username| !username.is_empty());
    let attempt = match username {
        None => state.auth.login(peer.ip(), &request.password),
        Some(username) => {
            state
                .auth
                .login_account(peer.ip(), username, &request.password, &state.state_store)
        }
    };
    let session = match attempt {
        Ok(session) => session,
        Err(LoginFailure::Invalid) => {
            return Err(ApiError::new(
                StatusCode::UNAUTHORIZED,
                if username.is_some() {
                    "invalid username or password"
                } else {
                    "invalid password"
                },
            ));
        }
        Err(LoginFailure::RateLimited) => return Ok(rate_limited_response()),
    };
    session_cookie_response(&session)
}

fn rate_limited_response() -> Response {
    let mut response = ApiError::new(
        StatusCode::TOO_MANY_REQUESTS,
        "too many login attempts; try again shortly",
    )
    .into_response();
    response
        .headers_mut()
        .insert(RETRY_AFTER, HeaderValue::from_static("60"));
    response
}

fn session_cookie_response(session: &str) -> std::result::Result<Response, ApiError> {
    let cookie =
        format!("{AUTH_COOKIE}={session}; HttpOnly; SameSite=Strict; Path=/; Max-Age=86400");
    let mut response = StatusCode::NO_CONTENT.into_response();
    response.headers_mut().insert(
        SET_COOKIE,
        HeaderValue::from_str(&cookie).map_err(ApiError::internal)?,
    );
    response
        .headers_mut()
        .insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    Ok(response)
}

/// 退出登录:作废令牌并清 cookie。
pub(in crate::web) async fn auth_logout(
    State(state): State<DaemonState>,
    headers: HeaderMap,
) -> std::result::Result<Response, ApiError> {
    if !origin_is_allowed(&headers) {
        return Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "request origin is not allowed",
        ));
    }
    if let Some(token) = cookie_value(&headers, AUTH_COOKIE) {
        state.auth.logout(token);
    }
    let cookie = format!("{AUTH_COOKIE}=; HttpOnly; SameSite=Strict; Path=/; Max-Age=0");
    let mut response = StatusCode::NO_CONTENT.into_response();
    response.headers_mut().insert(
        SET_COOKIE,
        HeaderValue::from_str(&cookie).map_err(ApiError::internal)?,
    );
    response
        .headers_mut()
        .insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    Ok(response)
}

/// 凭邀请码注册(阶段 5):建号成功即登录。邀请码错误一律同一句话,
/// 用户名/密码格式问题原样返回(那不是秘密)。
pub(in crate::web) async fn auth_register(
    State(state): State<DaemonState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(request): Json<RegisterRequest>,
) -> std::result::Result<Response, ApiError> {
    if !origin_is_allowed(&headers) {
        return Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "request origin is not allowed",
        ));
    }
    if !state.auth.required() {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "registration needs a password-protected WebUI (start the daemon with -p)",
        ));
    }
    if request.password.chars().count() > 1_024 || request.invite.chars().count() > 64 {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "request is too long",
        ));
    }
    if state.auth.throttle(peer.ip()).is_err() {
        return Ok(rate_limited_response());
    }
    let store = state.state_store.clone();
    let (invite, username, display_name, password) = (
        request.invite,
        request.username,
        request.display_name,
        request.password,
    );
    let account = tokio::task::spawn_blocking(move || {
        store.register_with_invite(&invite, &username, &display_name, &password)
    })
    .await
    .map_err(ApiError::internal)?;
    let account = match account {
        Ok(account) => account,
        Err(error) => {
            state.auth.note_failure(peer.ip());
            return Err(ApiError::new(StatusCode::BAD_REQUEST, error.to_string()));
        }
    };
    let session = state.auth.issue(WebIdentity::from_account(&account));
    session_cookie_response(&session)
}

pub(in crate::web) fn resolve_web_password(args: &WebArgs) -> Result<Option<String>> {
    let password = if let Some(path) = &args.password_file {
        let contents = std::fs::read_to_string(path)
            .with_context(|| format!("reading WebUI password file: {}", path.display()))?;
        Some(contents.trim_end_matches(['\r', '\n']).to_string())
    } else {
        match &args.password {
            Some(password) if !password.is_empty() => Some(password.clone()),
            Some(_) if io::stdin().is_terminal() => {
                Some(rpassword::prompt_password("WebUI password: ")?)
            }
            Some(_) => {
                anyhow::bail!("-p requires an interactive terminal or an explicit password value")
            }
            None => None,
        }
    };
    if let Some(password) = &password {
        if password.is_empty() {
            anyhow::bail!("WebUI password cannot be empty");
        }
        if password.chars().count() > 1_024 {
            anyhow::bail!("WebUI password cannot exceed 1,024 characters");
        }
    }
    Ok(password)
}

pub(in crate::web) fn redact_secret_list(
    states: &mut HashMap<String, bool>,
    key: &str,
    values: &mut Vec<String>,
) {
    states.insert(
        key.to_string(),
        values.iter().any(|value| !value.trim().is_empty()),
    );
    values.clear();
}

pub(in crate::web) fn redact_api_quota_provider(
    states: &mut HashMap<String, bool>,
    prefix: &str,
    provider: &mut crate::config::ApiQuotaProviderConfig,
) {
    if provider.accounts.is_empty() {
        provider
            .accounts
            .push(crate::config::ApiQuotaAccountConfig {
                id: "account-1".to_string(),
                name: "默认账号".to_string(),
                api_key: provider.api_key.clone(),
            });
    } else if !provider.api_key.trim().is_empty() && provider.accounts[0].api_key.trim().is_empty()
    {
        provider.accounts[0].api_key = provider.api_key.clone();
    }
    provider.api_key.clear();
    let mut used_ids = HashSet::with_capacity(provider.accounts.len());
    for (index, account) in provider.accounts.iter_mut().enumerate() {
        if account.id.trim().is_empty() || !used_ids.insert(account.id.clone()) {
            let mut number = index + 1;
            loop {
                let candidate = format!("account-{number}");
                if used_ids.insert(candidate.clone()) {
                    account.id = candidate;
                    break;
                }
                number += 1;
            }
        }
    }
    for (index, account) in provider.accounts.iter_mut().enumerate() {
        let key = format!("{prefix}.accounts.{index}.api_key");
        states.insert(key, !account.api_key.trim().is_empty());
        account.api_key.clear();
    }
}

pub(in crate::web) fn restore_api_quota_provider(
    candidate: &mut crate::config::ApiQuotaProviderConfig,
    current: &crate::config::ApiQuotaProviderConfig,
    mutations: &HashMap<String, SecretMutation>,
    recognized: &mut HashSet<String>,
    prefix: &str,
) -> std::result::Result<(), ApiError> {
    for (index, account) in candidate.accounts.iter_mut().enumerate() {
        let key = format!("{prefix}.accounts.{index}.api_key");
        recognized.insert(key.clone());
        let mut existing = current
            .accounts
            .iter()
            .find(|item| !account.id.is_empty() && item.id == account.id)
            .or_else(|| {
                current
                    .accounts
                    .iter()
                    .find(|item| item.id.is_empty() && item.name == account.name)
            })
            .map(|item| item.api_key.clone())
            .or_else(|| {
                (index == 0 && current.accounts.is_empty()).then(|| current.api_key.clone())
            })
            .unwrap_or_default();
        if existing.is_empty() && index == 0 && !current.api_key.trim().is_empty() {
            existing = current.api_key.clone();
        }
        account.api_key = match mutations.get(&key) {
            Some(SecretMutation::Set(value)) => {
                normalize_single_secret(value, &key)?.unwrap_or_default()
            }
            Some(SecretMutation::Clear) => String::new(),
            None => existing,
        };
    }
    candidate.api_key.clear();
    Ok(())
}

pub(in crate::web) fn restore_secret_list<Mut, Ref>(
    candidate: &mut AppConfig,
    current: &AppConfig,
    mutations: &HashMap<String, SecretMutation>,
    recognized: &mut HashSet<String>,
    key: &str,
    candidate_values: Mut,
    current_values: Ref,
) -> std::result::Result<(), ApiError>
where
    Mut: FnOnce(&mut AppConfig) -> &mut Vec<String>,
    Ref: FnOnce(&AppConfig) -> &Vec<String>,
{
    recognized.insert(key.to_string());
    *candidate_values(candidate) = match mutations.get(key) {
        Some(SecretMutation::Set(value)) => parse_secret_list(value, key)?,
        Some(SecretMutation::Clear) => Vec::new(),
        None => current_values(current).clone(),
    };
    Ok(())
}

pub(in crate::web) fn normalize_single_secret(
    value: &str,
    field: &str,
) -> std::result::Result<Option<String>, ApiError> {
    validate_secret_text(value, field)?;
    Ok(Some(value.trim().to_string()).filter(|value| !value.is_empty()))
}

pub(in crate::web) fn parse_secret_list(
    value: &str,
    field: &str,
) -> std::result::Result<Vec<String>, ApiError> {
    validate_secret_text(value, field)?;
    Ok(value
        .split(|character| matches!(character, ',' | '\n' | '\r'))
        .map(str::trim)
        .filter(|item| !item.is_empty())
        .map(str::to_string)
        .collect())
}

pub(in crate::web) fn validate_secret_text(
    value: &str,
    field: &str,
) -> std::result::Result<(), ApiError> {
    if value.chars().count() > MAX_SECRET_CHARS
        || value
            .chars()
            .any(|character| character.is_control() && !matches!(character, '\n' | '\r' | '\t'))
    {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            format!("{field} is invalid"),
        ));
    }
    Ok(())
}

pub(in crate::web) fn is_local_webui_request(
    audience: PromptAudience,
    has_turn_profile: bool,
) -> bool {
    audience == PromptAudience::External && !has_turn_profile
}

pub(in crate::web) fn require_auth(
    headers: &HeaderMap,
    state: &DaemonState,
) -> std::result::Result<(), ApiError> {
    require_identity(headers, state).map(|_| ())
}

/// 登录者身份;没开口令时是本机管理员。
pub(in crate::web) fn require_identity(
    headers: &HeaderMap,
    state: &DaemonState,
) -> std::result::Result<WebIdentity, ApiError> {
    state
        .auth
        .identity(cookie_value(headers, AUTH_COOKIE))
        .ok_or_else(|| ApiError::new(StatusCode::UNAUTHORIZED, "authentication required"))
}

/// 管理台专用:供应商与密钥、共享人格、脚本/技能、QQ、账号与邀请码。
pub(in crate::web) fn require_admin(
    headers: &HeaderMap,
    state: &DaemonState,
) -> std::result::Result<WebIdentity, ApiError> {
    let identity = require_identity(headers, state)?;
    if identity.admin {
        Ok(identity)
    } else {
        Err(ApiError::new(StatusCode::FORBIDDEN, "admin only"))
    }
}

/// 管理员改配置/写共享资源的路径:身份 + 来源校验。
pub(in crate::web) fn require_admin_mutation(
    headers: &HeaderMap,
    state: &DaemonState,
) -> std::result::Result<WebIdentity, ApiError> {
    let identity = require_admin(headers, state)?;
    if origin_is_allowed(headers) {
        Ok(identity)
    } else {
        Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "request origin is not allowed",
        ))
    }
}

pub(in crate::web) fn require_mutation(
    headers: &HeaderMap,
    state: &DaemonState,
) -> std::result::Result<(), ApiError> {
    require_auth(headers, state)?;
    if origin_is_allowed(headers) {
        Ok(())
    } else {
        Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "request origin is not allowed",
        ))
    }
}

pub(in crate::web) fn cookie_value<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    for header in headers.get_all(COOKIE) {
        let Ok(header) = header.to_str() else {
            continue;
        };
        for pair in header.split(';') {
            let Some((key, value)) = pair.trim().split_once('=') else {
                continue;
            };
            if key.trim() == name {
                return Some(value.trim());
            }
        }
    }
    None
}

pub(in crate::web) fn origin_is_allowed(headers: &HeaderMap) -> bool {
    let mut origins = headers.get_all(ORIGIN).iter();
    let Some(origin) = origins.next() else {
        return true;
    };
    if origins.next().is_some() {
        return false;
    }
    let Some(host) = headers.get(HOST).and_then(|host| host.to_str().ok()) else {
        return false;
    };
    let expected = format!("http://{host}");
    origin.to_str().is_ok_and(|origin| origin == expected)
}
