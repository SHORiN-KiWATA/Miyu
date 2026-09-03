//! REPL 的会话选择与切换。
//!
//! 一次性会话（`create_ephemeral_session`）用完就删，`EphemeralSessionGuard` 的
//! `Drop` 保证 Ctrl-C 退出时也删得掉——否则每次中断都在库里留一个空会话。
//!
//! 远端回合（`RemoteTurn*`）有三种结局：正常、被取消、被分离到后台。分离不是
//! 错误，所以 `is_remote_turn_detached` 单独判——当成错误会让用户以为出事了。

use crate::cli::*;

/// Which session a one-shot CLI turn lands in.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::cli) enum TurnSession {
    /// The terminal session — what shell-hook and `miyu new`/`session` drive.
    Current,
    /// An explicit `--session` target, resolved to a session id.
    Explicit(String),
    /// A throwaway session created for this turn and deleted right after, so a
    /// quick question never lands in a conversation the user cares about.
    Ephemeral,
}

/// Picks the session for `miyu ask` / a bare `miyu '<message>'`. Both default
/// to a throwaway session; `--session` and `--continue` opt back into a real
/// one (clap already rejects passing both).
pub(in crate::cli) async fn one_shot_session(
    paths: &MiyuPaths,
    session_arg: Option<&str>,
    continue_session: bool,
) -> Result<TurnSession> {
    if let Some(arg) = session_arg {
        return Ok(TurnSession::Explicit(
            resolve_session_id_for_turn(paths, arg).await?,
        ));
    }
    if continue_session {
        return Ok(TurnSession::Current);
    }
    Ok(TurnSession::Ephemeral)
}

/// Named rather than left blank on purpose: a row that leaks past the sweep is
/// recognisable, and a non-empty name also skips the daemon's auto-title LLM
/// call (`maybe_auto_name_session`) for a session about to be deleted.
pub(in crate::cli) fn ephemeral_session_name() -> String {
    t("One-shot", "一次性对话").to_string()
}

pub(in crate::cli) async fn create_ephemeral_session(paths: &MiyuPaths) -> Result<String> {
    let (_, data) = session_admin(
        paths,
        IpcCommand::CreateSession {
            name: Some(ephemeral_session_name()),
            switch: false,
            kind: Some(crate::state::ASK_SESSION_KIND.to_string()),
            mode: None,
        },
    )
    .await?;
    data.get("session")
        .and_then(|session| session.get("session_id"))
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| anyhow::anyhow!("Miyu core returned an invalid response"))
}

/// Tears a throwaway session down. Background jobs go first so nothing is left
/// pointing at a session that is about to disappear. Best effort: a daemon
/// that has gone away leaves a row the startup sweep collects.
pub(in crate::cli) async fn discard_ephemeral_session(paths: &MiyuPaths, session_id: &str) {
    // CLI 中转(claude-code/antigravity)的联动:直连形态没有 daemon,DeleteSession
    // 那条路上的 forget 不会跑到,这里自己收——续传映射与 CLI 侧转录都在本进程。
    crate::llm::forget_relay_sessions(session_id);
    let _ = send_ipc_admin(
        paths,
        IpcCommand::StopSessionJobs {
            session_id: session_id.to_string(),
        },
    )
    .await;
    let _ = send_ipc_admin(
        paths,
        IpcCommand::DeleteSession {
            target: crate::ipc::SessionRef::Id {
                id: session_id.to_string(),
            },
        },
    )
    .await;
}

/// Deletes the throwaway session however the direct-mode turn unwinds — error,
/// cancelled question, or early return.
pub(in crate::cli) struct EphemeralSessionGuard {
    pub(in crate::cli) state: StateStore,
    pub(in crate::cli) session_id: String,
}

impl Drop for EphemeralSessionGuard {
    fn drop(&mut self) {
        crate::llm::forget_relay_sessions(&self.session_id);
        let _ = self.state.delete_session(&self.session_id);
    }
}

pub(in crate::cli) struct RemoteTurnSummary {
    pub(in crate::cli) result: ChatResult,
    pub(in crate::cli) context_tokens: u64,
    pub(in crate::cli) context_window: Option<usize>,
    pub(in crate::cli) cumulative_tokens: TurnTokens,
}

/// Marker error for a remote turn interrupted by the user (Ctrl+C) or a
/// cancel from another client. The REPL catches it and returns to the prompt
/// instead of exiting; one-shot mode surfaces it as a normal error message.
#[derive(Debug)]
pub(in crate::cli) struct RemoteTurnCancelled;

impl std::fmt::Display for RemoteTurnCancelled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(t("cancelled", "已取消"))
    }
}

impl std::error::Error for RemoteTurnCancelled {}

/// 前端退出但回合继续:daemon 拥有回合,REPL 只是观众离席(验收:
/// dsh 语义,前端退出任务照跑)。
#[derive(Debug)]
pub(in crate::cli) struct RemoteTurnDetached;

impl std::fmt::Display for RemoteTurnDetached {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(t("detached", "已脱离"))
    }
}

impl std::error::Error for RemoteTurnDetached {}

pub(in crate::cli) fn is_remote_turn_detached(error: &anyhow::Error) -> bool {
    error.downcast_ref::<RemoteTurnDetached>().is_some()
}

pub(in crate::cli) fn is_remote_turn_cancelled(error: &anyhow::Error) -> bool {
    error.downcast_ref::<RemoteTurnCancelled>().is_some()
}

#[allow(clippy::too_many_arguments)]
/// 触发终端指纹。shellhook/单次 CLI 的 stdin 常被管道占用(--stdin 喂正文),
/// 所以按 stderr→stdout→stdin 找第一个 tty;父进程就是触发它的 shell。后台任务
/// 完成后 daemon 凭这份指纹校验「shell 还活着、仍在这个 tty、空闲在提示符」,
/// 才把跟进回复写回终端。检测不到(纯管道/重定向/cron)就不带。
pub(in crate::cli) fn detect_origin_tty() -> Option<crate::ipc::OriginTty> {
    let fd = [2, 1, 0]
        .into_iter()
        .find(|&fd| unsafe { libc::isatty(fd) } == 1)?;
    let path = std::fs::read_link(format!("/proc/self/fd/{fd}")).ok()?;
    if !path.starts_with("/dev/") {
        return None;
    }
    Some(crate::ipc::OriginTty {
        path,
        shell_pid: std::os::unix::process::parent_id(),
    })
}

pub(in crate::cli) async fn send_ipc_command(paths: &MiyuPaths, command: IpcCommand) -> Result<()> {
    let mut stream = ipc::connect(&paths.ipc_socket()).await?;
    ipc::send(&mut stream, &IpcRequest::new(command)).await?;
    validate_ipc_command_response(ipc::receive::<IpcFrame>(&mut stream).await?)
}

pub(in crate::cli) fn validate_ipc_command_response(frame: Option<IpcFrame>) -> Result<()> {
    match frame {
        Some(IpcFrame::Ack) | Some(IpcFrame::Ready { .. }) | Some(IpcFrame::AdminResult { .. }) => {
            Ok(())
        }
        Some(IpcFrame::Error { message, .. }) => bail!("{message}"),
        Some(other) => bail!("Miyu core returned an unexpected response: {other:?}"),
        None => bail!("Miyu core closed the connection without a response"),
    }
}

/// Refreshes REPL-local state after the daemon switched to another session:
/// input history, queue tray, and the footer's token accounting.
/// Writes one line of REPL feedback through the live tail so the output
/// cursor stays in sync; never use bare `println!` inside the remote REPL.
pub(in crate::cli) fn repl_note(live: &mut LiveReplTail, text: &str) -> Result<()> {
    live.apply_output_frame(format!("{text}\n").as_bytes())
}

/// Client-side display fallback for sessions the server has not named yet.
pub(in crate::cli) fn display_session_name(name: &str) -> &str {
    if name.trim().is_empty() {
        t("New session", "新会话")
    } else {
        name
    }
}

pub(in crate::cli) async fn apply_repl_session_switch(
    paths: &MiyuPaths,
    config: &AppConfig,
    state: &ipc::SessionState,
    active_session_id: &mut String,
    history: &mut Vec<String>,
    live_repl: &mut LiveReplTail,
    footer: &mut ReplFooterStatus,
    cumulative_tokens: &mut TurnTokens,
) -> Result<()> {
    if state.session_id.is_empty() {
        bail!("{}", t("session state has no id", "会话状态缺少 ID"));
    }
    let store = StateStore::new(paths)?.pinned(&state.session_id);
    active_session_id.clone_from(&state.session_id);
    *history = load_repl_input_history(&store, paths)?;
    live_repl.editor.history = history.clone();
    live_repl.editor.history_index = live_repl.editor.history.len();
    live_repl.editor.history_clean_index = None;
    live_repl.editor.input.clear();
    live_repl.editor.cursor = 0;
    repl_note(
        live_repl,
        &format!(
            "\x1b[2m{}: {}\x1b[0m\n",
            t("switched to session", "已切换到会话"),
            display_session_name(&state.session_name)
        ),
    )?;
    synchronized_terminal_update(CursorAfterUpdate::Shown, || live_repl.reload_queue(&store))?;
    // Rebuild rather than reset: the target session may pin its own model
    // pool, so provider/model/thinking have to be re-derived alongside the
    // token numbers. `refresh_footer` repaints straight away — merely storing
    // the footer left the previous session's numbers on screen until the next
    // turn finished.
    *cumulative_tokens = state_cumulative(&state);
    let session_config = footer_config_for_session(paths, config, &state.session_id);
    *footer =
        ReplFooterStatus::from_config(&session_config, state.context_tokens, *cumulative_tokens);
    let client = OpenAiCompatibleClient::from_config(&session_config, paths)?;
    footer.update_thinking_variant(client.thinking_variant_summary().as_deref());
    footer.update_context_window(state.context_window, state.context_window_assumed);
    live_repl.refresh_footer(footer.clone())?;
    // Every REPL session change funnels through here, so this is the one place
    // the REPL lane needs to be remembered. Best effort: losing the write only
    // means the next REPL starts on the terminal session.
    let _ = send_ipc_admin(
        paths,
        IpcCommand::SetReplSession {
            target: crate::ipc::SessionRef::Id {
                id: state.session_id.clone(),
            },
        },
    )
    .await;
    Ok(())
}

/// One row of the daemon's session list, parsed from `ListSessions` JSON.
pub(in crate::cli) struct SessionListEntry {
    pub(in crate::cli) id: String,
    pub(in crate::cli) name: String,
    pub(in crate::cli) is_current: bool,
    pub(in crate::cli) turns: u64,
    pub(in crate::cli) snippet: String,
    pub(in crate::cli) workspace: Option<String>,
    /// "dev" | "normal",由 daemon 按会话人格推导。
    pub(in crate::cli) mode: String,
}

pub(in crate::cli) fn session_list_entries(data: &serde_json::Value) -> Vec<SessionListEntry> {
    data.get("sessions")
        .and_then(serde_json::Value::as_array)
        .map(|sessions| sessions.iter().map(session_list_entry).collect())
        .unwrap_or_default()
}

pub(in crate::cli) fn session_list_entry(session: &serde_json::Value) -> SessionListEntry {
    let text = |key: &str| {
        session
            .get(key)
            .and_then(serde_json::Value::as_str)
            .map(str::to_string)
    };
    SessionListEntry {
        id: text("session_id").unwrap_or_default(),
        name: text("name").unwrap_or_default(),
        is_current: session
            .get("is_current")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false),
        turns: session
            .get("turn_count")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0),
        snippet: text("last_user_content")
            .map(|content| {
                let cleaned = content.trim().replace(['\n', '\r'], " ");
                let truncated: String = cleaned.chars().take(24).collect();
                if cleaned.chars().count() > 24 {
                    format!("{truncated}…")
                } else {
                    truncated
                }
            })
            .unwrap_or_default(),
        workspace: text("workspace"),
        mode: text("mode").unwrap_or_else(|| "normal".to_string()),
    }
}

/// Maps a user-facing 1-based session number to a session id ref.
pub(in crate::cli) fn session_ref_from_index(
    entries: &[SessionListEntry],
    index: usize,
) -> Option<crate::ipc::SessionRef> {
    index
        .checked_sub(1)
        .and_then(|index| entries.get(index))
        .map(|entry| crate::ipc::SessionRef::Id {
            id: entry.id.clone(),
        })
}

pub(in crate::cli) fn session_entry_is_active(
    entry: &SessionListEntry,
    active_session_id: Option<&str>,
) -> bool {
    active_session_id.map_or(entry.is_current, |session_id| entry.id == session_id)
}

pub(in crate::cli) fn session_select_line(
    entry: &SessionListEntry,
    active_session_id: Option<&str>,
) -> String {
    let marker = if session_entry_is_active(entry, active_session_id) {
        "* "
    } else {
        "  "
    };
    // 验收三轮定版:「模式：名称 · 摘要」,轮数删掉。
    let mut line = format!(
        "{marker}{}：{}",
        session_mode_label(&entry.mode),
        display_session_name(&entry.name),
    );
    if !entry.snippet.is_empty() {
        line.push_str(" · ");
        line.push_str(&entry.snippet);
    }
    if let Some(workspace) = &entry.workspace {
        line.push_str(&format!("  [{workspace}]"));
    }
    line
}

pub(in crate::cli) fn session_select_search(entry: &SessionListEntry) -> String {
    format!(
        "{} {} {} {}",
        display_session_name(&entry.name),
        session_mode_label(&entry.mode),
        entry.snippet,
        entry.workspace.as_deref().unwrap_or_default()
    )
}

/// 会话类型标(验收:列表看不出普通/开发)。
pub(in crate::cli) fn session_mode_label(mode: &str) -> &'static str {
    if mode == "dev" {
        t("dev", "开发")
    } else {
        t("normal", "普通")
    }
}

pub(in crate::cli) fn session_initial_selection(
    entries: &[SessionListEntry],
    active_session_id: Option<&str>,
) -> usize {
    entries
        .iter()
        .position(|entry| session_entry_is_active(entry, active_session_id))
        .unwrap_or(0)
}

/// What the interactive session picker came back with.
pub(in crate::cli) enum SessionPick {
    Cancelled,
    Switch(crate::ipc::SessionRef),
    /// Deletion confirmed inside the picker. `index` is where the cursor sat,
    /// so the caller can reopen the refreshed list at the same spot.
    Delete {
        session_id: String,
        index: usize,
    },
}

pub(in crate::cli) fn select_session_target(
    entries: &[SessionListEntry],
    active_session_id: Option<&str>,
    cursor: Option<usize>,
) -> Result<SessionPick> {
    let lines = entries
        .iter()
        .map(|entry| session_select_line(entry, active_session_id))
        .collect::<Vec<_>>();
    let search = entries
        .iter()
        .map(session_select_search)
        .collect::<Vec<_>>();
    let labels = entries
        .iter()
        .map(|entry| display_session_name(&entry.name).to_string())
        .collect::<Vec<_>>();
    let initial = cursor
        .map(|index| index.min(entries.len().saturating_sub(1)))
        .unwrap_or_else(|| session_initial_selection(entries, active_session_id));
    Ok(
        match inline_single_select_deletable(
            t("Select session", "选择会话"),
            &lines,
            &search,
            initial,
            Some(&labels),
        )? {
            InlineSelectOutcome::Cancelled => SessionPick::Cancelled,
            InlineSelectOutcome::Chosen(index) => SessionPick::Switch(crate::ipc::SessionRef::Id {
                id: entries[index].id.clone(),
            }),
            InlineSelectOutcome::Deleted(index) => SessionPick::Delete {
                session_id: entries[index].id.clone(),
                index,
            },
        },
    )
}

/// Resolves a user-typed `/session` / `/delete` argument into a session ref:
/// a number picks from the visible session list, anything else is a name.
/// REPL 会话列表的作用域:dev REPL 只看/只解析 dev 人格名下的会话。
pub(in crate::cli) fn repl_list_mode(mode: AgentMode) -> Option<String> {
    (mode == AgentMode::Dev).then(|| "dev".to_string())
}

pub(in crate::cli) async fn resolve_repl_session_target(
    paths: &MiyuPaths,
    live: &mut LiveReplTail,
    mode: AgentMode,
    arg: &str,
) -> Result<Option<crate::ipc::SessionRef>> {
    let index = arg.parse::<usize>().ok();
    // 名字寻址在 daemon 侧按"当前人格"检索,够不着 dev 会话;dev REPL
    // 统一走列表在客户端配对,再降成不可猜的 id 显式寻址。
    if index.is_some() || mode == AgentMode::Dev {
        let Some((_, data)) = repl_ipc_admin(
            paths,
            live,
            IpcCommand::ListSessions {
                mode: repl_list_mode(mode),
            },
        )
        .await?
        else {
            return Ok(None);
        };
        let entries = session_list_entries(&data);
        let target = match index {
            Some(index) => session_ref_from_index(&entries, index),
            None => entries.iter().find(|entry| entry.name == arg).map(|entry| {
                crate::ipc::SessionRef::Id {
                    id: entry.id.clone(),
                }
            }),
        };
        let Some(target) = target else {
            repl_note(
                live,
                &format!(
                    "\x1b[2m{}: {arg}\x1b[0m\n",
                    t("no such session", "没有这个会话")
                ),
            )?;
            return Ok(None);
        };
        Ok(Some(target))
    } else {
        Ok(Some(crate::ipc::SessionRef::Name {
            name: arg.to_string(),
        }))
    }
}

pub(in crate::cli) fn reload_repl_queue(
    live: &mut LiveReplTail,
    paths: &MiyuPaths,
    session_id: &str,
) -> Result<()> {
    let store = StateStore::new(paths)?.pinned(session_id);
    synchronized_terminal_update(CursorAfterUpdate::Shown, || live.reload_queue(&store))
}

pub(in crate::cli) fn confirm_inline(live: &mut LiveReplTail, prompt: &str) -> Result<bool> {
    live.apply_output_frame(format!("{prompt} [y/N] ").as_bytes())?;
    let mut answer = String::new();
    io::stdin().read_line(&mut answer)?;
    Ok(matches!(answer.trim(), "y" | "Y" | "yes" | "YES"))
}

pub(in crate::cli) fn confirm_stdin(prompt: &str) -> Result<bool> {
    print!("{prompt} [y/N] ");
    io::stdout().flush()?;
    let mut answer = String::new();
    io::stdin().read_line(&mut answer)?;
    Ok(matches!(answer.trim(), "y" | "Y" | "yes" | "YES"))
}

/// Sends an admin command from inside the REPL loop, printing failures (core
/// busy, core restarting, …) through the live tail instead of propagating
/// them so the REPL survives.
pub(in crate::cli) async fn repl_ipc_admin(
    paths: &MiyuPaths,
    live: &mut LiveReplTail,
    command: IpcCommand,
) -> Result<Option<(ipc::SessionState, serde_json::Value)>> {
    match send_ipc_admin(paths, command).await {
        Ok(result) => Ok(Some(result)),
        Err(err) => {
            repl_note(
                live,
                &format!("\x1b[31m{}: {err}\x1b[0m\n", t("error", "错误")),
            )?;
            Ok(None)
        }
    }
}

pub(in crate::cli) async fn repl_get_session_state(
    paths: &MiyuPaths,
    live: &mut LiveReplTail,
    target: crate::ipc::SessionRef,
) -> Result<Option<ipc::SessionState>> {
    Ok(
        repl_ipc_admin(paths, live, IpcCommand::GetSessionState { target })
            .await?
            .map(|(state, _)| state),
    )
}

pub(in crate::cli) async fn repl_fallback_session_state(
    paths: &MiyuPaths,
    live: &mut LiveReplTail,
    mode: AgentMode,
) -> Result<Option<ipc::SessionState>> {
    // dev 无普通人格的"终端会话"可退:GetReplSession 会治愈指针并在
    // 没有 dev 会话时就地自举一个,绝不落回普通人格的会话。
    if mode == AgentMode::Dev {
        return Ok(repl_ipc_admin(
            paths,
            live,
            IpcCommand::GetReplSession {
                mode: Some("dev".to_string()),
            },
        )
        .await?
        .map(|(state, _)| state));
    }
    let Some((_, data)) =
        repl_ipc_admin(paths, live, IpcCommand::ListSessions { mode: None }).await?
    else {
        return Ok(None);
    };
    let entries = session_list_entries(&data);
    let Some(entry) = entries
        .iter()
        .find(|entry| entry.is_current)
        .or_else(|| entries.first())
    else {
        return Ok(None);
    };
    repl_get_session_state(
        paths,
        live,
        crate::ipc::SessionRef::Id {
            id: entry.id.clone(),
        },
    )
    .await
}

/// Runs the interactive session picker inside the REPL, servicing Ctrl+D
/// deletions in place. Returns the session state to switch to — a fallback
/// session when the REPL's own session was one of the ones deleted, so backing
/// out never strands the REPL on a session that no longer exists.
pub(in crate::cli) async fn repl_pick_session(
    paths: &MiyuPaths,
    live: &mut LiveReplTail,
    mode: AgentMode,
    active_session_id: &str,
) -> Result<Option<ipc::SessionState>> {
    let mut cursor = None;
    let mut lost_active = false;
    loop {
        let Some((_, data)) = repl_ipc_admin(
            paths,
            live,
            IpcCommand::ListSessions {
                mode: repl_list_mode(mode),
            },
        )
        .await?
        else {
            return Ok(None);
        };
        let entries = session_list_entries(&data);
        if entries.is_empty() {
            repl_note(
                live,
                &format!("\x1b[2m{}\x1b[0m\n", t("no sessions", "没有会话")),
            )?;
            // Deleting the last session leaves the daemon to mint a fresh one.
            return if lost_active {
                repl_fallback_session_state(paths, live, mode).await
            } else {
                Ok(None)
            };
        }
        match select_session_target(&entries, Some(active_session_id), cursor)? {
            SessionPick::Cancelled => {
                return if lost_active {
                    repl_fallback_session_state(paths, live, mode).await
                } else {
                    Ok(None)
                };
            }
            SessionPick::Switch(target) => {
                return repl_get_session_state(paths, live, target).await;
            }
            SessionPick::Delete { session_id, index } => {
                let was_active = session_id == active_session_id;
                let deleted = repl_ipc_admin(
                    paths,
                    live,
                    IpcCommand::DeleteSession {
                        target: crate::ipc::SessionRef::Id { id: session_id },
                    },
                )
                .await?;
                if deleted.is_none() {
                    return if lost_active {
                        repl_fallback_session_state(paths, live, mode).await
                    } else {
                        Ok(None)
                    };
                }
                lost_active |= was_active;
                // The rows below shift up, so holding the index parks the
                // cursor on the next session instead of jumping to the top.
                cursor = Some(index);
            }
        }
    }
}

pub(in crate::cli) async fn repl_active_or_default_state(
    paths: &MiyuPaths,
    active_session_id: &str,
) -> Result<(ipc::SessionState, bool)> {
    match send_ipc_admin(
        paths,
        IpcCommand::GetSessionState {
            target: crate::ipc::SessionRef::Id {
                id: active_session_id.to_string(),
            },
        },
    )
    .await
    {
        Ok((state, _)) => Ok((state, false)),
        Err(_) => {
            let (state, _) = send_ipc_admin(paths, IpcCommand::GetStatus).await?;
            let changed = state.session_id != active_session_id;
            Ok((state, changed))
        }
    }
}

/// Ensures the daemon is running, then sends one admin command; used by the
/// one-shot session subcommands (`miyu new/session/rename/...`).
pub(in crate::cli) async fn session_admin(
    paths: &MiyuPaths,
    command: IpcCommand,
) -> Result<(ipc::SessionState, serde_json::Value)> {
    ipc::ensure_daemon(paths, None).await?;
    let refreshed = MiyuPaths::new()?;
    send_ipc_admin(&refreshed, command).await
}

/// Resolves a `miyu session/delete` target argument outside the REPL:
/// numbers index into the visible session list, anything else is a name.
/// Resolves a `--session` argument (name or list index) to a concrete
/// session id, without moving the global current pointer.
pub(in crate::cli) async fn resolve_session_id_for_turn(
    paths: &MiyuPaths,
    arg: &str,
) -> Result<String> {
    let (_, data) = session_admin(paths, IpcCommand::ListSessions { mode: None }).await?;
    let entries = session_list_entries(&data);
    if let Ok(index) = arg.parse::<usize>() {
        if let Some(entry) = index.checked_sub(1).and_then(|index| entries.get(index)) {
            return Ok(entry.id.clone());
        }
        bail!(
            "{}: {index}",
            t("no session with this number", "没有这个编号的会话")
        );
    }
    entries
        .into_iter()
        .find(|entry| entry.name.eq_ignore_ascii_case(arg) || entry.id == arg)
        .map(|entry| entry.id)
        .ok_or_else(|| anyhow::anyhow!("{}: {arg}", t("session not found", "找不到该会话")))
}

/// `/goal edit`（无参数）的编辑器内变身：把「/goal edit <当前目标>」放进
/// 输入行，改几个字就能回车——终端里的「可编辑文本框」。
///
/// 必须在提交**之前**拦：提交会把原文回显成一条消息块，用户看到的是
/// 「/goal edit 被当作消息发出去了」。返回 true 表示已变身（调用方跳过这次
/// 提交并重绘输入行）；没有目标时返回 false，走正常提交让命令层去报错。
pub(in crate::cli) fn prefill_goal_edit_input(
    paths: &MiyuPaths,
    session_id: Option<&str>,
    live: &mut LiveReplTail,
) -> bool {
    let Some(session) = session_id else {
        return false;
    };
    let Some(objective) = StateStore::new(paths)
        .ok()
        .and_then(|store| store.goal(session).ok().flatten())
        .map(|goal| goal.objective)
    else {
        return false;
    };
    live.editor.input = format!("/goal edit {objective}");
    live.editor.cursor = live.editor.input.chars().count();
    live.editor.history_clean_index = None;
    true
}

pub(in crate::cli) async fn send_ipc_admin(
    paths: &MiyuPaths,
    command: IpcCommand,
) -> Result<(ipc::SessionState, serde_json::Value)> {
    let mut stream = ipc::connect(&paths.ipc_socket()).await?;
    ipc::send(&mut stream, &IpcRequest::new(command)).await?;
    match ipc::receive::<IpcFrame>(&mut stream).await? {
        Some(IpcFrame::AdminResult { state, data }) => Ok((state, data)),
        Some(IpcFrame::Error { message, .. }) => bail!("{message}"),
        _ => bail!("Miyu core returned an invalid admin response"),
    }
}

pub(in crate::cli) fn ipc_text<'a>(data: &'a serde_json::Value, key: &str) -> &'a str {
    data.get(key)
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
}

pub(in crate::cli) fn ipc_u64(data: &serde_json::Value, key: &str) -> u64 {
    data.get(key)
        .and_then(serde_json::Value::as_u64)
        .unwrap_or_default()
}

pub(in crate::cli) fn ipc_mode_name(mode: AgentMode) -> &'static str {
    match mode {
        AgentMode::Normal => "normal",
        AgentMode::Dev => "dev",
    }
}

pub(in crate::cli) fn ipc_images(
    images: &[Option<crate::clipboard::PastedImage>],
) -> Vec<Option<crate::ipc::ImageAttachment>> {
    images
        .iter()
        .map(|image| {
            image.as_ref().map(|image| match image {
                crate::clipboard::PastedImage::Binary(image) => {
                    crate::ipc::ImageAttachment::Binary {
                        mime: image.mime.clone(),
                        data: image.data.clone(),
                    }
                }
                crate::clipboard::PastedImage::Path(path) => {
                    crate::ipc::ImageAttachment::Path { path: path.clone() }
                }
            })
        })
        .collect()
}
