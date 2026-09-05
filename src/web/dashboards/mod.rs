//! 插件 dashboard 的 HTTP 面(09-03 demo:先落记忆浏览器一块)。
//!
//! 每个领域一个文件、一组 `/api/dash/<domain>/...` 路由;读用 `require_auth`,
//! 写用 `require_mutation`。不走插件 trait 钩子——这些面板看的多是工具域数据,
//! 与 `qq_history.rs` 一样直接挂在路由表上。

pub(in crate::web) mod affection;
pub(in crate::web) mod kb;
pub(in crate::web) mod memes;
pub(in crate::web) mod memory;
pub(in crate::web) mod qq;
pub(in crate::web) mod scripts;

use crate::config::AppConfig;
use crate::web::*;

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
