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
