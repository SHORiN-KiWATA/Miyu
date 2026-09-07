//! `miyu session …`:程序驱动的会话管理面。
//!
//! 每个子命令都是对 daemon 一条会话 IPC 的薄壳;`--json` 直出 daemon 的
//! 数据形状,不二次加工——宿主和 WebUI 看到的是同一份。目标参数认编号
//! (`session list` 的序号)、名字或 id。

use crate::cli::args::{ModelsArgs, SessionCommand};
use crate::cli::exit_code::usage_error;
use crate::cli::model_cmds::{run_models_for_session, session_model_override_snapshot};
use crate::cli::pop_cmds::{print_pop_outcome, PopOutcome};
use crate::cli::repl::session::{session_admin, SessionListEntry};
use crate::cli::turn_request::{
    create_named_session, list_managed_sessions, resolve_managed_session,
};
use crate::i18n::text as t;
use crate::ipc::{Command as IpcCommand, SessionRef, SessionState};
use crate::paths::MiyuPaths;
use crate::state::StateStore;
use anyhow::Result;
use serde_json::{json, Value};
use std::io::{self, IsTerminal, Write};

fn print_json(value: &Value) -> Result<()> {
    let mut out = io::stdout().lock();
    writeln!(out, "{}", serde_json::to_string_pretty(value)?)?;
    Ok(())
}

fn session_ref(entry: &SessionListEntry) -> SessionRef {
    SessionRef::Id {
        id: entry.id.clone(),
    }
}

fn print_session_table(entries: &[SessionListEntry]) {
    if entries.is_empty() {
        println!("{}", t("no sessions", "没有会话"));
        return;
    }
    for (index, entry) in entries.iter().enumerate() {
        let current = if entry.is_current { "*" } else { " " };
        let workspace = entry
            .workspace
            .as_deref()
            .map(|path| format!("  @{path}"))
            .unwrap_or_default();
        println!(
            "{current}{:>3}  {:<6} {:>4}  {}{}  {}",
            index + 1,
            entry.mode,
            entry.turns,
            entry.name,
            workspace,
            format_args!("\x1b[2m{}\x1b[0m", entry.snippet),
        );
    }
}

/// 详情 = 列表行 + `GetSessionState` 的上下文计量 + 模型覆盖快照。
async fn session_detail(paths: &MiyuPaths, entry: &SessionListEntry) -> Result<Value> {
    let (state, _) = session_admin(
        paths,
        IpcCommand::GetSessionState {
            target: session_ref(entry),
        },
    )
    .await?;
    let SessionState {
        context_tokens,
        context_window,
        context_window_assumed,
        cumulative_tokens,
        cumulative_prompt_tokens,
        cumulative_cache_read_tokens,
        workspace,
        ..
    } = state;
    let models = session_model_override_snapshot(paths, Some(&entry.id))?;
    Ok(json!({
        "session_id": entry.id,
        "name": entry.name,
        "mode": entry.mode,
        "is_current": entry.is_current,
        "turn_count": entry.turns,
        "last_user_content": entry.snippet,
        "workspace": workspace.or_else(|| entry.workspace.clone()),
        "context_tokens": context_tokens,
        "context_window": context_window,
        "context_window_assumed": context_window_assumed,
        "cumulative_tokens": cumulative_tokens,
        "cumulative_prompt_tokens": cumulative_prompt_tokens,
        "cumulative_cache_read_tokens": cumulative_cache_read_tokens,
        "model_override": models,
    }))
}

fn print_session_detail(detail: &Value) {
    let field = |key: &str| {
        detail
            .get(key)
            .map(|value| match value {
                Value::String(text) => text.clone(),
                Value::Null => "-".to_string(),
                other => other.to_string(),
            })
            .unwrap_or_else(|| "-".to_string())
    };
    let rows = [
        (t("session", "会话"), field("name")),
        ("id", field("session_id")),
        (t("mode", "模式"), field("mode")),
        (t("turns", "轮数"), field("turn_count")),
        (t("workspace", "工作区"), field("workspace")),
        (
            t("context", "上下文"),
            format!("{} / {}", field("context_tokens"), field("context_window")),
        ),
        (
            t("cumulative tokens", "累计 token"),
            field("cumulative_tokens"),
        ),
        (
            t("model override", "模型覆盖"),
            match detail.get("model_override") {
                Some(Value::Array(models)) if !models.is_empty() => models
                    .iter()
                    .map(|model| {
                        format!(
                            "{}/{}",
                            model["provider_id"].as_str().unwrap_or_default(),
                            model["model"].as_str().unwrap_or_default()
                        )
                    })
                    .collect::<Vec<_>>()
                    .join(", "),
                _ => t("(inherits global pool)", "(继承全局模型池)").to_string(),
            },
        ),
    ];
    for (label, value) in rows {
        println!("{label:<12} {value}");
    }
}

/// `session pop` 的候选在客户端按会话挑(只读),写走 IPC,daemon 仍是唯一
/// 写者——与 `miyu pop` 同款分工。
async fn pop_session(paths: &MiyuPaths, entry: &SessionListEntry, count: usize) -> Result<()> {
    let state = StateStore::new(paths)?.pinned(&entry.id);
    let turn_ids: Vec<String> = state
        .oldest_evictable_visible_turns(count)?
        .into_iter()
        .map(|turn| turn.turn_id)
        .collect();
    if turn_ids.is_empty() {
        crate::cli::pop_cmds::print_nothing_to_pop();
        return Ok(());
    }
    let (_, data) = session_admin(
        paths,
        IpcCommand::Pop {
            target: session_ref(entry),
            turn_ids,
        },
    )
    .await?;
    print_pop_outcome(PopOutcome {
        turns: data["turns"].as_u64().unwrap_or_default() as usize,
        archived: data["archived"].as_bool().unwrap_or(false),
    });
    Ok(())
}

pub(in crate::cli) async fn run_session_command(
    paths: &MiyuPaths,
    command: SessionCommand,
) -> Result<()> {
    match command {
        SessionCommand::List { json } => {
            if json {
                let (_, data) = session_admin(
                    paths,
                    IpcCommand::ListSessions {
                        mode: Some("all".to_string()),
                    },
                )
                .await?;
                return print_json(&data);
            }
            print_session_table(&list_managed_sessions(paths).await?);
            Ok(())
        }
        SessionCommand::New { name, mode, json } => {
            let name = name.trim().to_string();
            if name.is_empty() || name.parse::<usize>().is_ok() {
                return Err(usage_error(t(
                    "session name must be non-empty and not a number",
                    "会话名不能为空,也不能是纯数字",
                )));
            }
            let session = create_named_session(paths, &name, mode.as_deref()).await?;
            if json {
                return print_json(&session);
            }
            println!(
                "{}: {} ({})",
                t("created session", "已新建会话"),
                session["name"].as_str().unwrap_or(&name),
                session["session_id"].as_str().unwrap_or_default()
            );
            Ok(())
        }
        SessionCommand::Show { target, json } => {
            let entry = resolve_managed_session(paths, &target).await?;
            let detail = session_detail(paths, &entry).await?;
            if json {
                return print_json(&detail);
            }
            print_session_detail(&detail);
            Ok(())
        }
        SessionCommand::Delete { target, yes } => {
            let entry = resolve_managed_session(paths, &target).await?;
            if !yes {
                if !io::stdin().is_terminal() {
                    return Err(usage_error(t(
                        "delete needs a terminal to confirm; pass --yes to run it unattended",
                        "删除需要终端确认;非交互场景请加 --yes",
                    )));
                }
                let prompt = format!("{} {}?", t("delete session", "删除会话"), entry.name);
                if !crate::cli::repl::session::confirm_stdin(&prompt)? {
                    println!("{}", t("cancelled", "已取消"));
                    return Ok(());
                }
            }
            let _ = session_admin(
                paths,
                IpcCommand::StopSessionJobs {
                    session_id: entry.id.clone(),
                },
            )
            .await;
            session_admin(
                paths,
                IpcCommand::DeleteSession {
                    target: session_ref(&entry),
                },
            )
            .await?;
            println!("{}: {}", t("deleted session", "已删除会话"), entry.name);
            Ok(())
        }
        SessionCommand::Rename { target, name } => {
            let entry = resolve_managed_session(paths, &target).await?;
            let name = name.trim().to_string();
            if name.is_empty() {
                return Err(usage_error(t("name must not be empty", "名字不能为空")));
            }
            session_admin(
                paths,
                IpcCommand::RenameSession {
                    target: session_ref(&entry),
                    name: name.clone(),
                },
            )
            .await?;
            println!("{}: {} → {}", t("renamed", "已重命名"), entry.name, name);
            Ok(())
        }
        SessionCommand::Clear { target } => {
            let entry = resolve_managed_session(paths, &target).await?;
            session_admin(
                paths,
                IpcCommand::ResetConversation {
                    target: session_ref(&entry),
                },
            )
            .await?;
            println!(
                "{}: {}",
                t("cleared session context", "已清空会话上下文"),
                entry.name
            );
            Ok(())
        }
        SessionCommand::Pop { target, count } => {
            let entry = resolve_managed_session(paths, &target).await?;
            pop_session(paths, &entry, count).await
        }
        SessionCommand::Compact { target } => {
            let entry = resolve_managed_session(paths, &target).await?;
            compact_session(paths, session_ref(&entry), Some(&entry.name)).await
        }
        SessionCommand::Models { target, model } => {
            let entry = resolve_managed_session(paths, &target).await?;
            run_models_for_session(
                paths,
                ModelsArgs {
                    target: model,
                    global: false,
                },
                Some(&entry.id),
            )
            .await
        }
        SessionCommand::Workspace { target, dir, clear } => {
            let entry = resolve_managed_session(paths, &target).await?;
            if !clear && dir.is_none() {
                println!(
                    "{}",
                    entry
                        .workspace
                        .as_deref()
                        .unwrap_or(t("(no workspace bound)", "(未绑定工作区)"))
                );
                return Ok(());
            }
            let path = match dir {
                Some(dir) => Some(std::fs::canonicalize(&dir).map_err(|error| {
                    usage_error(format!(
                        "{}: {} ({error})",
                        t("directory not found", "找不到目录"),
                        dir.display()
                    ))
                })?),
                None => None,
            };
            session_admin(
                paths,
                IpcCommand::SetWorkspace {
                    target: session_ref(&entry),
                    path: path.clone(),
                },
            )
            .await?;
            match path {
                Some(path) => println!(
                    "{}: {} → {}",
                    t("workspace bound", "已绑定工作区"),
                    entry.name,
                    path.display()
                ),
                None => println!("{}: {}", t("workspace cleared", "已解绑工作区"), entry.name),
            }
            Ok(())
        }
    }
}

/// 压缩一个会话的上下文。`miyu compact`(缺省当前会话)与
/// `miyu session compact <目标>` 共用这一条;`name` 只用于回显。
///
/// 走 `session_admin` 而不是裸 IPC:daemon 没起就先拉起来——压缩要过 actor,
/// 没有进程内直连的等价路径。目标会话有回合在跑时 daemon 会拒(admin busy),
/// 那是设计:压缩重写消息数组,在跑的回合手里那份会成悬空引用。
pub(in crate::cli) async fn compact_session(
    paths: &MiyuPaths,
    target: SessionRef,
    name: Option<&str>,
) -> Result<()> {
    let (_, data) = session_admin(paths, IpcCommand::Compact { target }).await?;
    if data["compacted"].as_bool().unwrap_or(false) {
        match name {
            Some(name) => println!("{}: {}", t("compacted", "已压缩"), name),
            None => println!("{}", t("compacted", "已压缩")),
        }
    } else {
        println!("{}", t("nothing to compact", "没有可压缩的内容"));
    }
    Ok(())
}

/// stdio 模式的会话操作:同一套 IPC,结果以 JSON 返回给分发器,不打印。
/// `op` 与 `miyu session` 子命令同名;`args` 是宿主传的对象。
pub(in crate::cli) async fn session_op_json(
    paths: &MiyuPaths,
    op: &str,
    args: &Value,
) -> Result<Value> {
    let text = |key: &str| args.get(key).and_then(Value::as_str).map(str::trim);
    let target = || {
        text("target")
            .or_else(|| text("name"))
            .filter(|target| !target.is_empty())
            .ok_or_else(|| usage_error(t("missing target", "缺少 target")))
    };
    match op {
        "list" => {
            let (_, data) = session_admin(
                paths,
                IpcCommand::ListSessions {
                    mode: Some("all".to_string()),
                },
            )
            .await?;
            Ok(data)
        }
        "new" => {
            let name = text("name")
                .filter(|name| !name.is_empty() && name.parse::<usize>().is_err())
                .ok_or_else(|| {
                    usage_error(t(
                        "session name must be non-empty and not a number",
                        "会话名不能为空,也不能是纯数字",
                    ))
                })?;
            create_named_session(paths, name, text("mode")).await
        }
        "show" => {
            let entry = resolve_managed_session(paths, target()?).await?;
            session_detail(paths, &entry).await
        }
        "delete" => {
            let entry = resolve_managed_session(paths, target()?).await?;
            let _ = session_admin(
                paths,
                IpcCommand::StopSessionJobs {
                    session_id: entry.id.clone(),
                },
            )
            .await;
            session_admin(
                paths,
                IpcCommand::DeleteSession {
                    target: session_ref(&entry),
                },
            )
            .await?;
            Ok(json!({ "session_id": entry.id }))
        }
        "rename" => {
            let entry = resolve_managed_session(paths, target()?).await?;
            let name = text("new_name")
                .filter(|name| !name.is_empty())
                .ok_or_else(|| usage_error(t("missing new_name", "缺少 new_name")))?;
            session_admin(
                paths,
                IpcCommand::RenameSession {
                    target: session_ref(&entry),
                    name: name.to_string(),
                },
            )
            .await?;
            Ok(json!({ "session_id": entry.id, "name": name }))
        }
        "clear" => {
            let entry = resolve_managed_session(paths, target()?).await?;
            session_admin(
                paths,
                IpcCommand::ResetConversation {
                    target: session_ref(&entry),
                },
            )
            .await?;
            Ok(json!({ "session_id": entry.id }))
        }
        "pop" => {
            let entry = resolve_managed_session(paths, target()?).await?;
            let count = args
                .get("count")
                .and_then(Value::as_u64)
                .filter(|count| *count > 0)
                .ok_or_else(|| usage_error(t("count must be positive", "count 必须是正整数")))?;
            let state = StateStore::new(paths)?.pinned(&entry.id);
            let turn_ids: Vec<String> = state
                .oldest_evictable_visible_turns(count as usize)?
                .into_iter()
                .map(|turn| turn.turn_id)
                .collect();
            if turn_ids.is_empty() {
                return Ok(json!({ "session_id": entry.id, "turns": 0, "archived": false }));
            }
            let (_, mut data) = session_admin(
                paths,
                IpcCommand::Pop {
                    target: session_ref(&entry),
                    turn_ids,
                },
            )
            .await?;
            data["session_id"] = json!(entry.id);
            Ok(data)
        }
        "compact" => {
            let entry = resolve_managed_session(paths, target()?).await?;
            let (_, mut data) = session_admin(
                paths,
                IpcCommand::Compact {
                    target: session_ref(&entry),
                },
            )
            .await?;
            data["session_id"] = json!(entry.id);
            Ok(data)
        }
        other => Err(usage_error(format!(
            "{}: {other}",
            t("unknown session op", "未知的会话操作")
        ))),
    }
}
