//! `TurnOptions`(命令行)→ 回合请求:解析模型、读 @文件、定会话。
//!
//! 一次性 `ask` 与 stdio 的 `message` 共用这里的解析,所以「模型不存在」
//! 「@文件读不到」「对已有会话传 --mode」这些用法错误在两条路上报的是同一
//! 句话、同一个退出码。

use crate::cli::args::TurnOptions;
use crate::cli::exit_code::{session_not_found, usage_error};
use crate::cli::repl::session::{
    create_ephemeral_session, session_admin, session_list_entries, SessionListEntry,
};
use crate::config::{ActiveProviderModelConfig, AppConfig};
use crate::i18n::text as t;
use crate::ipc::{Command as IpcCommand, TurnOverrides};
use crate::paths::MiyuPaths;
use anyhow::Result;

/// 本回合落在哪个会话。`ephemeral` 为真时调用方用完要拆。
pub struct ResolvedSession {
    /// None = daemon 当前会话。
    pub session_id: Option<String>,
    pub ephemeral: bool,
}

/// 文本参数支持 `@路径` 读文件;其余原样。
pub fn read_text_argument(argument: &str) -> Result<String> {
    if let Some(path) = argument.strip_prefix('@') {
        let path = crate::cli::stdin_input::expand_tilde(path);
        return std::fs::read_to_string(&path).map_err(|error| {
            usage_error(format!(
                "{}: {} ({error})",
                t("cannot read prompt file", "读不到提示词文件"),
                path.display()
            ))
        });
    }
    Ok(argument.to_string())
}

/// 模型参数 → 池条目。走与 `miyu models` 同一套解析(序号/provider/model/
/// 裸名),所以两处认的写法一致。
pub fn resolve_model_argument(
    config: &AppConfig,
    argument: &str,
) -> Result<ActiveProviderModelConfig> {
    let choices = config.text_provider_model_choices();
    let choice =
        crate::config::resolve_provider_model_argument(&choices, argument).map_err(usage_error)?;
    Ok(ActiveProviderModelConfig {
        provider_id: choice.provider_id.clone(),
        model: choice.model.clone(),
    })
}

/// 覆盖类参数 → `TurnOverrides`;全空返回 None(协议里不带字段)。
pub fn build_overrides(paths: &MiyuPaths, options: &TurnOptions) -> Result<Option<TurnOverrides>> {
    let mut overrides = TurnOverrides::default();
    if let Some(model) = options.model.as_deref() {
        let config = AppConfig::load(paths)?;
        overrides.models = vec![resolve_model_argument(&config, model)?];
    }
    if let Some(window) = options.context_window {
        if window == 0 {
            return Err(usage_error(t(
                "--context-window must be positive",
                "--context-window 必须是正整数",
            )));
        }
        overrides.context_window = Some(window);
    }
    if let Some(prompt) = options.system_prompt.as_deref() {
        overrides.system_prompt = Some(read_text_argument(prompt)?);
    }
    if let Some(prompt) = options.append_system_prompt.as_deref() {
        overrides.append_system_prompt = Some(read_text_argument(prompt)?);
    }
    if options.no_memory {
        overrides.memory_writes = Some(false);
    }
    if options.no_tools {
        overrides.tool_allowlist = Some(Vec::new());
    } else if let Some(tools) = &options.tools {
        overrides.tool_allowlist = Some(
            tools
                .iter()
                .map(|name| name.trim().to_string())
                .filter(|name| !name.is_empty())
                .collect(),
        );
    }
    Ok((!overrides.is_empty()).then_some(overrides))
}

/// 管理面看到的会话列表:当前人格的普通+开发模式会话,不含阅后即焚。
/// `miyu session list` 的编号与 `--session N` 用的是同一份。
pub async fn list_managed_sessions(paths: &MiyuPaths) -> Result<Vec<SessionListEntry>> {
    let (_, data) = session_admin(
        paths,
        IpcCommand::ListSessions {
            mode: Some("all".to_string()),
        },
    )
    .await?;
    Ok(session_list_entries(&data))
}

pub fn find_session<'a>(
    entries: &'a [SessionListEntry],
    target: &str,
) -> Option<&'a SessionListEntry> {
    let target = target.trim();
    if let Ok(index) = target.parse::<usize>() {
        if let Some(entry) = index.checked_sub(1).and_then(|index| entries.get(index)) {
            return Some(entry);
        }
    }
    entries.iter().find(|entry| entry.id == target).or_else(|| {
        entries
            .iter()
            .find(|entry| entry.name.eq_ignore_ascii_case(target))
    })
}

/// 管理面的目标解析:编号/名字/id → 会话 id;找不到退出码 3。
pub async fn resolve_managed_session(paths: &MiyuPaths, target: &str) -> Result<SessionListEntry> {
    let entries = list_managed_sessions(paths).await?;
    find_session(&entries, target)
        .cloned()
        .ok_or_else(|| session_not_found(target))
}

pub async fn create_named_session(
    paths: &MiyuPaths,
    name: &str,
    mode: Option<&str>,
) -> Result<serde_json::Value> {
    let (_, data) = session_admin(
        paths,
        IpcCommand::CreateSession {
            name: Some(name.to_string()),
            switch: false,
            kind: None,
            mode: mode.map(str::to_string),
        },
    )
    .await?;
    data.get("session")
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("Miyu core returned an invalid response"))
}

/// `--session/--create/--continue/--mode` → 会话。规则:
/// - `--session X`:存在就用;不存在且 `--create` 就建(带 `--mode`),否则
///   退出码 3;存在且传了不同的 `--mode` 报用法错误(模式在建会话时定死)。
/// - `--continue`:daemon 当前会话;传 `--mode` 报用法错误。
/// - 都没传:阅后即焚会话,`--mode` 决定它的模式。
pub async fn resolve_turn_session(
    paths: &MiyuPaths,
    options: &TurnOptions,
) -> Result<ResolvedSession> {
    let mode = options.mode.as_deref();
    if let Some(target) = options.session.as_deref() {
        let entries = list_managed_sessions(paths).await?;
        if let Some(entry) = find_session(&entries, target) {
            if let Some(mode) = mode {
                if !entry.mode.eq_ignore_ascii_case(mode) {
                    return Err(usage_error(format!(
                        "{}: {} ({})",
                        t(
                            "--mode only applies when creating a session; this session already is",
                            "--mode 只在新建会话时生效;该会话已是"
                        ),
                        entry.name,
                        entry.mode
                    )));
                }
            }
            return Ok(ResolvedSession {
                session_id: Some(entry.id.clone()),
                ephemeral: false,
            });
        }
        if !options.create {
            return Err(session_not_found(target));
        }
        if target.trim().parse::<usize>().is_ok() {
            return Err(usage_error(t(
                "--create needs a session name, not a number",
                "--create 需要会话名,不能是编号",
            )));
        }
        let session = create_named_session(paths, target.trim(), mode).await?;
        let session_id = session
            .get("session_id")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| anyhow::anyhow!("Miyu core returned an invalid response"))?;
        return Ok(ResolvedSession {
            session_id: Some(session_id),
            ephemeral: false,
        });
    }
    if options.continue_session {
        if mode.is_some() {
            return Err(usage_error(t(
                "--mode cannot change the current session's mode; use --session NAME --create",
                "--mode 改不了当前会话的模式;请用 --session 名字 --create",
            )));
        }
        return Ok(ResolvedSession {
            session_id: None,
            ephemeral: false,
        });
    }
    Ok(ResolvedSession {
        session_id: Some(create_ephemeral_session(paths, mode).await?),
        ephemeral: true,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: &str, name: &str) -> SessionListEntry {
        SessionListEntry {
            id: id.to_string(),
            name: name.to_string(),
            is_current: false,
            turns: 0,
            snippet: String::new(),
            workspace: None,
            mode: "normal".to_string(),
        }
    }

    #[test]
    fn finds_by_index_then_id_then_name() {
        let entries = vec![entry("s-1", "alpha"), entry("s-2", "Beta")];
        assert_eq!(find_session(&entries, "2").unwrap().id, "s-2");
        assert_eq!(find_session(&entries, "s-1").unwrap().name, "alpha");
        assert_eq!(find_session(&entries, "beta").unwrap().id, "s-2");
        assert!(find_session(&entries, "9").is_none());
        assert!(find_session(&entries, "gamma").is_none());
    }

    #[test]
    fn at_file_argument_reads_file_and_reports_missing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("p.md");
        std::fs::write(&path, "hello").unwrap();
        assert_eq!(
            read_text_argument(&format!("@{}", path.display())).unwrap(),
            "hello"
        );
        assert_eq!(read_text_argument("literal").unwrap(), "literal");
        let error = read_text_argument("@/nonexistent/x.md").unwrap_err();
        assert_eq!(
            crate::cli::exit_code::exit_code_for(&error),
            crate::cli::exit_code::EXIT_USAGE
        );
    }
}
