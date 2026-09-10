//! 插件 dashboard 的 HTTP 面(09-03 demo:先落记忆浏览器一块)。
//!
//! 每个领域一个文件、一组 `/api/dash/<domain>/...` 路由;只给管理员(阶段 5):读用 `require_admin`,
//! 写用 `require_admin_mutation`。不走插件 trait 钩子——这些面板看的多是工具域数据,
//! 与 `qq_history.rs` 一样直接挂在路由表上。

pub(in crate::web) mod affection;
pub(in crate::web) mod kb;
pub(in crate::web) mod ledger;
pub(in crate::web) mod memes;
pub(in crate::web) mod memory;
pub(in crate::web) mod qq;
pub(in crate::web) mod scripts;
pub(in crate::web) mod sponsor;

use crate::config::AppConfig;
use crate::web::*;

/// 按登录者取面板配置(阶段 8):管理员照旧按人格作用域;成员拿到的是带自己
/// 家目录的配置——知识库、账本落在 `home/<用户>`,记忆面板只能看自己的私有人格
/// (`persona` 给 scope 或留空 = 当前用的;共享 Miyu 的记忆不给成员看)。
pub(in crate::web) fn dash_config_for(
    state: &DaemonState,
    identity: &WebIdentity,
    persona: &str,
) -> std::result::Result<AppConfig, ApiError> {
    if identity.admin {
        return persona_scoped_config(state, persona);
    }
    let mut config = state.manager.lock().unwrap().config.clone();
    config.accounts.home_dir = Some(
        state
            .paths
            .user_home_dir(&identity.username)
            .display()
            .to_string(),
    );
    let persona = persona.trim();
    let private = if persona.is_empty() {
        member_persona::active_persona(&state.paths, &identity.username)
    } else {
        member_persona::persona_for_scope(&state.paths, &identity.username, persona)
    };
    if let Some(private) = private {
        member_persona::apply_to_config(&mut config, &private);
    } else if !persona.is_empty() {
        return Err(ApiError::new(StatusCode::NOT_FOUND, "persona not found"));
    }
    Ok(config)
}

/// 记忆面板要求成员有私有人格:共享 Miyu 的记忆库不是成员的。
pub(in crate::web) fn dash_memory_config_for(
    state: &DaemonState,
    identity: &WebIdentity,
    persona: &str,
) -> std::result::Result<AppConfig, ApiError> {
    let config = dash_config_for(state, identity, persona)?;
    if !identity.admin && config.private_persona_dir().is_none() {
        return Err(ApiError::new(
            StatusCode::NOT_FOUND,
            "create a persona of your own first; the shared persona's memory is not yours to browse",
        ));
    }
    Ok(config)
}

/// 按人格作用域取配置:人格名进路径,只认平面名字。记忆 / 脚本等按人格分层的
/// 面板共用——空名或与当前人格同一作用域时原样用当前配置(空名的作用域是
/// "default")。
pub(in crate::web) fn persona_scoped_config(
    state: &DaemonState,
    persona: &str,
) -> std::result::Result<AppConfig, ApiError> {
    let mut config = state.manager.lock().unwrap().config.clone();
    let persona = persona.trim();
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
