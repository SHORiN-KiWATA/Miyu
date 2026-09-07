//! 程序驱动 CLI 的退出码约定。
//!
//! 宿主软件靠退出码分流,所以「什么错」得能从码上读出来,不能全是 1:
//!
//! | 码  | 含义 |
//! |-----|------|
//! | 0   | 成功 |
//! | 1   | 回合失败(模型/工具/daemon 错误) |
//! | 2   | 用法或参数错误(模型不存在、会话模式冲突、@文件读不到) |
//! | 3   | 会话不存在 |
//! | 124 | 超时被取消(`--timeout`) |
//! | 130 | 被用户/宿主取消(Ctrl+C、stdio cancel) |
//!
//! 带码的错误用 [`CliExit`] 包一层塞进 anyhow 链;`main.rs` 用
//! [`crate::exit_code_for`] 取码。没包的错误一律 1。

use std::fmt;

pub const EXIT_FAILURE: i32 = 1;
pub const EXIT_USAGE: i32 = 2;
pub const EXIT_SESSION_NOT_FOUND: i32 = 3;
pub const EXIT_TIMEOUT: i32 = 124;
pub const EXIT_CANCELLED: i32 = 130;

/// 带退出码的错误。`message` 是给人看的正文;`main` 打印时不再加码。
#[derive(Debug)]
pub struct CliExit {
    pub code: i32,
    pub message: String,
}

impl fmt::Display for CliExit {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for CliExit {}

pub fn exit_with(code: i32, message: impl Into<String>) -> anyhow::Error {
    anyhow::Error::new(CliExit {
        code,
        message: message.into(),
    })
}

pub fn usage_error(message: impl Into<String>) -> anyhow::Error {
    exit_with(EXIT_USAGE, message)
}

pub fn session_not_found(target: &str) -> anyhow::Error {
    exit_with(
        EXIT_SESSION_NOT_FOUND,
        format!(
            "{}: {target}",
            crate::i18n::text("session not found", "找不到该会话")
        ),
    )
}

/// 从 anyhow 链里取退出码:`CliExit` 带的码优先,REPL 的取消标记算 130,
/// 其余 1。
pub fn exit_code_for(error: &anyhow::Error) -> i32 {
    if let Some(exit) = error.downcast_ref::<CliExit>() {
        return exit.code;
    }
    if error
        .downcast_ref::<crate::cli::repl::session::RemoteTurnCancelled>()
        .is_some()
    {
        return EXIT_CANCELLED;
    }
    EXIT_FAILURE
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn coded_errors_keep_their_code_through_anyhow_context() {
        let error = usage_error("bad flag").context("while parsing");
        assert_eq!(exit_code_for(&error), EXIT_USAGE);
        let error = session_not_found("x");
        assert_eq!(exit_code_for(&error), EXIT_SESSION_NOT_FOUND);
        let error = exit_with(EXIT_TIMEOUT, "slow");
        assert_eq!(exit_code_for(&error), EXIT_TIMEOUT);
    }

    #[test]
    fn plain_errors_map_to_one() {
        assert_eq!(exit_code_for(&anyhow::anyhow!("boom")), EXIT_FAILURE);
    }

    #[test]
    fn remote_cancel_maps_to_130() {
        let error = anyhow::Error::new(crate::cli::repl::session::RemoteTurnCancelled);
        assert_eq!(exit_code_for(&error), EXIT_CANCELLED);
    }
}
