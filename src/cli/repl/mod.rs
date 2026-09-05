//! 交互式 REPL。
//!
//! 按职责分文件：宽度计算、输入编辑、活动区渲染、远端与直连两条回合驱动。
pub(in crate::cli) mod commands;
pub(in crate::cli) mod dictation;
pub(in crate::cli) mod jobs;
pub(in crate::cli) mod layout;
pub(in crate::cli) mod placeholder;
pub(in crate::cli) mod session;

pub(super) mod direct;
pub(super) mod editor;
pub(super) mod input;
pub(in crate::cli) mod live_turn;
pub(super) mod remote;
pub(super) mod tail;
pub(super) mod wake;
pub(super) mod width;
