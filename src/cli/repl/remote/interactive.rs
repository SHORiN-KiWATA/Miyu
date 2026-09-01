//! 交互式远端 REPL。
//!
//! 主循环：读输入 → 发 IPC → 收事件流 → 刷活动区，中间还要处理后台任务唤醒、
//! 排队消息、窗口变化。这是终端的日常路径。
//!
//! 事件分发表在这里维护，和 [`crate::cli::repl::live_turn`] 的本地泵是两份——
//! 加新事件时两边都要过一遍（08-17「footer 不刷新」就是漏了一边）。

use crate::cli::repl::editor::*;
use crate::cli::repl::input::*;
use crate::cli::repl::tail::*;
use crate::cli::*;

pub(in crate::cli) async fn run_remote_repl(paths: &MiyuPaths, mut mode: AgentMode) -> Result<()> {
    let _cursor_restore = ReplCursorRestore;
    ipc::ensure_daemon(paths, None).await?;
    let refreshed = MiyuPaths::new()?;
    let paths = &refreshed;
    initialize_models_cache(paths);
    let mut config = AppConfig::load_or_default(paths)?;
    // The REPL resumes its own lane rather than the terminal session, so
    // reopening a REPL lands back where the last one left off while shell-hook
    // keeps talking to whatever session it was on.
    let (daemon_state, _) = send_ipc_admin(
        paths,
        IpcCommand::GetReplSession {
            mode: (mode == AgentMode::Dev).then(|| "dev".to_string()),
        },
    )
    .await?;
    let mut active_session_id = daemon_state.session_id.clone();
    let history_state = StateStore::new(paths)?.pinned(&active_session_id);
    let mut history = load_repl_input_history(&history_state, paths)?;
    drop(history_state);
    let mut cumulative_tokens = state_cumulative(&daemon_state);
    // footer 的模型标签与思考程度必须同源:都从会话作用域配置推导。
    // 曾经 client 用全局配置,标签显示会话覆盖模型、·max 却算的全局
    // 模型,两边各说各话(验收#23)。
    let session_config = footer_config_for_session(paths, &config, &active_session_id);
    let mut footer = ReplFooterStatus::from_config(
        &session_config,
        daemon_state.context_tokens,
        cumulative_tokens,
    );
    let client = OpenAiCompatibleClient::from_config(&session_config, paths)?;
    let thinking_summary = client.thinking_variant_summary();
    footer.update_thinking_variant(thinking_summary.as_deref());
    footer.update_context_window(
        daemon_state.context_window,
        daemon_state.context_window_assumed,
    );
    let mut live_repl = LiveReplTail::new(mode, history.clone(), Vec::new(), footer.clone())?;
    let jobs_shared = spawn_jobs_poll_thread(paths.clone());
    let jobs_feed = JobsFeed::Shared(jobs_shared.clone());

    // Terminal closed (SIGHUP) or process killed (SIGTERM): the graceful
    // exit path at the bottom never runs, so stop this session's background
    // jobs from a signal task before dying. SIGKILL still leaks them — the
    // daemon keeps those running and their completion wakes queue up.
    {
        let paths = paths.clone();
        let feed = jobs_shared.clone();
        tokio::spawn(async move {
            use tokio::signal::unix::{signal, SignalKind};
            let (Ok(mut hangup), Ok(mut terminate)) = (
                signal(SignalKind::hangup()),
                signal(SignalKind::terminate()),
            ) else {
                return;
            };
            tokio::select! {
                _ = hangup.recv() => {}
                _ = terminate.recv() => {}
            }
            // 后台任务归 daemon 管:前端死了任务照跑,完成后有唤醒
            // (验收:dsh 语义,前端退出不拖死会话任务)。
            let _ = (&paths, &feed);
            // SIGTERM 时终端往往还活着:process::exit 绕过 Drop,先尽力
            // 恢复 raw mode,否则用户的 shell 停在原始模式里。
            let _ = crossterm::terminal::disable_raw_mode();
            std::process::exit(0);
        });
    }

    // Redraw the tail of the session we just resumed. The tail is not on
    // screen yet (`rendered == false`), so `apply_output_frame` writes the
    // frame raw and re-reads the cursor — no layout budget applies and the
    // frame can be arbitrarily long.
    if config.display.repl_replay_turns > 0 {
        let replay_store = StateStore::new(paths)?.pinned(&active_session_id);
        match replay_store.session_replay(config.display.repl_replay_turns) {
            Ok(replays) if !replays.is_empty() => {
                let (cols, _) = terminal::size().unwrap_or((80, 24));
                let frame =
                    session_replay_frame(&replays, mode, &config, usize::from(cols.max(1)))?;
                live_repl.apply_output_frame(&frame)?;
            }
            Ok(_) => {}
            Err(error) => tracing::debug!(error = %error, "session replay unavailable"),
        }
    }

    loop {
        // Keep the poll thread's session filter in step with /new & /session.
        *jobs_shared.repl_session.lock().unwrap() = Some(active_session_id.clone());
        live_repl.set_footer(footer.clone());
        let (next_mode, input, images) = match read_live_repl_input(
            &mut live_repl,
            paths,
            &jobs_feed,
            Some(&active_session_id),
        )? {
            LiveReplOutcome::Exit => break,
            LiveReplOutcome::StopJobs => {
                let stopped = match repl_ipc_admin(
                    paths,
                    &mut live_repl,
                    IpcCommand::StopSessionJobs {
                        session_id: active_session_id.clone(),
                    },
                )
                .await?
                {
                    Some((_, data)) => data
                        .get("stopped")
                        .and_then(serde_json::Value::as_u64)
                        .unwrap_or(0),
                    None => 0,
                };
                // Drop the strip now instead of waiting out the ~1s jobs poll:
                // every job of this session was just stopped, so an empty strip
                // is the truth.
                live_repl.set_jobs(Vec::new());
                repl_note(
                    &mut live_repl,
                    &format!(
                        "\x1b[2m{}\x1b[0m\n",
                        if is_zh() {
                            format!("已停止 {stopped} 个后台任务")
                        } else {
                            format!("stopped {stopped} background task(s)")
                        }
                    ),
                )?;
                continue;
            }
            LiveReplOutcome::FollowWake { run_id, label } => {
                if let Err(error) = follow_wake_run(
                    paths,
                    &mut live_repl,
                    &run_id,
                    &label,
                    &active_session_id,
                    &jobs_feed,
                    &jobs_shared,
                )
                .await
                {
                    tracing::debug!(error = %error, "wake follow detached with an error");
                }
                continue;
            }
            LiveReplOutcome::Submit(next_mode, input, images) => (next_mode, input, images),
        };
        mode = next_mode;
        let input = input.trim();
        if input.eq_ignore_ascii_case("exit") || input.eq_ignore_ascii_case("quit") {
            break;
        }
        let (slash_command, command_args) = match parse_repl_input(input) {
            ReplInput::Chat => (None, ""),
            ReplInput::Slash(command, args) => (Some(command), args),
        };
        if let Some(command) = slash_command {
            // 命令也进上方向键历史：`/goal 长长的目标` 打错一个字重敲一遍，
            // 和重敲一条消息一样冤。落盘历史仍只收消息（命令是操作不是对话）。
            push_history_capped(&mut history, input);
            live_repl.editor.record_history(input);
            let spec = repl_command_spec(command);
            if spec.arg_hint.is_empty() && !command_args.trim().is_empty() {
                repl_note(
                    &mut live_repl,
                    &format!(
                        "\x1b[2m{}: {}\x1b[0m\n",
                        t("this command takes no arguments", "该命令不接受参数"),
                        spec.name
                    ),
                )?;
                continue;
            }
            match command {
                ReplSlashCommand::Exit => break,
                ReplSlashCommand::Help => print_repl_help(),
                ReplSlashCommand::History => {
                    let state = StateStore::new(paths)?.pinned(&active_session_id);
                    run_history_with_state(
                        &state,
                        HistoryArgs {
                            limit: 20,
                            raw: false,
                            no_thinking: false,
                        },
                    )?
                }
                ReplSlashCommand::Clear => {
                    synchronized_terminal_update(CursorAfterUpdate::Preserve, || {
                        live_repl.clear_screen()
                    })?;
                }
                ReplSlashCommand::New => {
                    let name = command_args.trim();
                    let Some((_, data)) = repl_ipc_admin(
                        paths,
                        &mut live_repl,
                        IpcCommand::CreateSession {
                            name: (!name.is_empty()).then(|| name.to_string()),
                            switch: false,
                            kind: None,
                            mode: (mode == AgentMode::Dev).then(|| "dev".to_string()),
                        },
                    )
                    .await?
                    else {
                        continue;
                    };
                    let Some(session_id) = data
                        .get("session")
                        .and_then(|session| session.get("session_id"))
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_string)
                    else {
                        repl_note(
                            &mut live_repl,
                            &format!(
                                "\x1b[31m{}\x1b[0m\n",
                                t("created session has no id", "新会话缺少 ID")
                            ),
                        )?;
                        continue;
                    };
                    let Some((state, _)) = repl_ipc_admin(
                        paths,
                        &mut live_repl,
                        IpcCommand::GetSessionState {
                            target: crate::ipc::SessionRef::Id { id: session_id },
                        },
                    )
                    .await?
                    else {
                        continue;
                    };
                    apply_repl_session_switch(
                        paths,
                        &config,
                        &state,
                        &mut active_session_id,
                        &mut history,
                        &mut live_repl,
                        &mut footer,
                        &mut cumulative_tokens,
                    )
                    .await?;
                }
                ReplSlashCommand::Session => {
                    let arg = command_args.trim();
                    let state = if arg.is_empty() {
                        match repl_pick_session(paths, &mut live_repl, mode, &active_session_id)
                            .await?
                        {
                            Some(state) => state,
                            None => continue,
                        }
                    } else {
                        let target =
                            match resolve_repl_session_target(paths, &mut live_repl, mode, arg)
                                .await?
                            {
                                Some(target) => target,
                                None => continue,
                            };
                        match repl_get_session_state(paths, &mut live_repl, target).await? {
                            Some(state) => state,
                            None => continue,
                        }
                    };
                    apply_repl_session_switch(
                        paths,
                        &config,
                        &state,
                        &mut active_session_id,
                        &mut history,
                        &mut live_repl,
                        &mut footer,
                        &mut cumulative_tokens,
                    )
                    .await?;
                }
                ReplSlashCommand::Rename => {
                    let name = command_args.trim().to_string();
                    if name.is_empty() {
                        repl_note(
                            &mut live_repl,
                            &format!(
                                "\x1b[2m{}\x1b[0m\n",
                                t("usage: /rename <name>", "用法：/rename <新名称>")
                            ),
                        )?;
                        continue;
                    }
                    if repl_ipc_admin(
                        paths,
                        &mut live_repl,
                        IpcCommand::RenameSession {
                            target: crate::ipc::SessionRef::Id {
                                id: active_session_id.clone(),
                            },
                            name: name.clone(),
                        },
                    )
                    .await?
                    .is_some()
                    {
                        repl_note(
                            &mut live_repl,
                            &format!(
                                "\x1b[2m{}: {name}\x1b[0m\n",
                                t("session renamed", "会话已重命名")
                            ),
                        )?;
                    }
                }
                ReplSlashCommand::Delete => {
                    let arg = command_args.trim();
                    let target = if arg.is_empty() {
                        crate::ipc::SessionRef::Id {
                            id: active_session_id.clone(),
                        }
                    } else {
                        match resolve_repl_session_target(paths, &mut live_repl, mode, arg).await? {
                            Some(target) => target,
                            None => continue,
                        }
                    };
                    let Some(target_state) =
                        repl_get_session_state(paths, &mut live_repl, target).await?
                    else {
                        continue;
                    };
                    let deleted_active = target_state.session_id == active_session_id;
                    if !confirm_inline(
                        &mut live_repl,
                        t(
                            "delete this session and all of its history?",
                            "确认删除该会话及其全部历史？",
                        ),
                    )? {
                        repl_note(
                            &mut live_repl,
                            &format!("\x1b[2m{}\x1b[0m\n", t("cancelled", "已取消")),
                        )?;
                        continue;
                    }
                    let Some((_, _)) = repl_ipc_admin(
                        paths,
                        &mut live_repl,
                        IpcCommand::DeleteSession {
                            target: crate::ipc::SessionRef::Id {
                                id: target_state.session_id,
                            },
                        },
                    )
                    .await?
                    else {
                        continue;
                    };
                    repl_note(
                        &mut live_repl,
                        &format!("\x1b[2m{}\x1b[0m", t("session deleted", "会话已删除")),
                    )?;
                    if deleted_active {
                        let Some(state) =
                            repl_fallback_session_state(paths, &mut live_repl, mode).await?
                        else {
                            continue;
                        };
                        apply_repl_session_switch(
                            paths,
                            &config,
                            &state,
                            &mut active_session_id,
                            &mut history,
                            &mut live_repl,
                            &mut footer,
                            &mut cumulative_tokens,
                        )
                        .await?;
                    }
                }
                ReplSlashCommand::Workspace => {
                    let arg = command_args.trim();
                    if arg.is_empty() {
                        let Some(state) = repl_get_session_state(
                            paths,
                            &mut live_repl,
                            crate::ipc::SessionRef::Id {
                                id: active_session_id.clone(),
                            },
                        )
                        .await?
                        else {
                            continue;
                        };
                        let note = match state.workspace {
                            Some(workspace) => format!(
                                "\x1b[2m{}: {workspace}\x1b[0m\n",
                                t("session workspace", "会话工作目录")
                            ),
                            None => format!(
                                "\x1b[2m{}\x1b[0m\n",
                                t(
                                    "no workspace bound; using the client working directory",
                                    "未绑定工作目录；使用客户端当前目录"
                                )
                            ),
                        };
                        repl_note(&mut live_repl, &note)?;
                        continue;
                    }
                    if arg.eq_ignore_ascii_case("clear") {
                        if repl_ipc_admin(
                            paths,
                            &mut live_repl,
                            IpcCommand::SetWorkspace {
                                target: crate::ipc::SessionRef::Id {
                                    id: active_session_id.clone(),
                                },
                                path: None,
                            },
                        )
                        .await?
                        .is_some()
                        {
                            repl_note(
                                &mut live_repl,
                                &format!(
                                    "\x1b[2m{}\x1b[0m\n",
                                    t("workspace unbound", "已解绑工作目录")
                                ),
                            )?;
                        }
                        continue;
                    }
                    let path = match std::fs::canonicalize(expand_tilde(arg)) {
                        Ok(path) => path,
                        Err(error) => {
                            repl_note(
                                &mut live_repl,
                                &format!(
                                    "\x1b[31m{}: {arg} ({error})\x1b[0m\n",
                                    t("invalid workspace path", "无效的工作目录路径")
                                ),
                            )?;
                            continue;
                        }
                    };
                    if repl_ipc_admin(
                        paths,
                        &mut live_repl,
                        IpcCommand::SetWorkspace {
                            target: crate::ipc::SessionRef::Id {
                                id: active_session_id.clone(),
                            },
                            path: Some(path.clone()),
                        },
                    )
                    .await?
                    .is_some()
                    {
                        repl_note(
                            &mut live_repl,
                            &format!(
                                "\x1b[2m{}: {}\x1b[0m\n",
                                t("workspace bound", "已绑定工作目录"),
                                path.display()
                            ),
                        )?;
                    }
                }
                ReplSlashCommand::Goal => {
                    // 走 IPC 而不是直连库：目标本身在库里，但「是否自动续跑」
                    // 驻在 daemon 内存，REPL 进程自己设那个标记，续轮驱动器
                    // 根本看不见。
                    let Some((_, data)) = repl_ipc_admin(
                        paths,
                        &mut live_repl,
                        IpcCommand::Goal {
                            target: crate::ipc::SessionRef::Id {
                                id: active_session_id.clone(),
                            },
                            input: command_args.to_string(),
                        },
                    )
                    .await?
                    else {
                        continue;
                    };
                    // 终端里没有 WebUI 那条常驻状态行，所以这里必须回一句
                    // ——设完目标到第一轮真正开跑之间有一段静默（驱动器要等
                    // 会话空下来），一个字都不说的话，用户只会以为命令没生效。
                    // 但也就一句：状态、轮次这些留给 `/goal` 自己去查。
                    let text = data
                        .get("text")
                        .and_then(|value| value.as_str())
                        .unwrap_or_default();
                    // 光敲 `/goal edit` 到不了这里：输入泵在提交前就原地变身
                    // 成「/goal edit <当前目标>」（`prefill_goal_edit_input`），
                    // 只有没目标时才落进来打提示。
                    let summary = if command_args.trim().is_empty() {
                        text.to_string()
                    } else {
                        // 多行的详情压成一句：命令回执不该占半屏。
                        text.lines().next().unwrap_or_default().to_string()
                    };
                    // 暗色 + 图标：这是系统回执，不是模型正文，得和邻居们
                    // （工作目录绑定、后台任务表头）长得一族。单个 \n 收尾，
                    // 和它们一致——多一个就空两行。
                    repl_note(&mut live_repl, &format!("\x1b[2m◎ {summary}\x1b[0m\n"))?;
                }
                ReplSlashCommand::Usage => {
                    let snapshot = StateStore::new(paths)?.usage_snapshot()?;
                    let usage = footer.token_usage;
                    let context = Some((usage.session_tokens, usage.context_window));
                    repl_note(
                        &mut live_repl,
                        &format!("{}\n\n", usage_overview_text(&snapshot, context)),
                    )?;
                }
                ReplSlashCommand::Persona => match run_persona_picker(paths, command_args) {
                    Ok(true) => {
                        let _ =
                            repl_ipc_admin(paths, &mut live_repl, IpcCommand::ReloadConfig).await;
                        config = AppConfig::load(paths)?;
                        // 人格是会话的命名空间维度:切人格后 daemon 的当前会话
                        // 指针已经指向新人格的会话,前端必须重取并把
                        // active_session_id / history / footer 一起换过去。
                        // 漏了这步(09-01 前的实现)会让前端还挂在旧人格的
                        // 会话上等事件流,而 daemon 在新人格语境里跑,消息发出
                        // 去永远等不到回执——UI 卡死在「加载中 0ms」。
                        let (daemon_state, _) = send_ipc_admin(
                            paths,
                            IpcCommand::GetReplSession {
                                mode: (mode == AgentMode::Dev).then(|| "dev".to_string()),
                            },
                        )
                        .await?;
                        apply_repl_session_switch(
                            paths,
                            &config,
                            &daemon_state,
                            &mut active_session_id,
                            &mut history,
                            &mut live_repl,
                            &mut footer,
                            &mut cumulative_tokens,
                        )
                        .await?;
                        repl_note(
                            &mut live_repl,
                            &format!("{}\n", t("configuration reloaded", "配置已重新加载")),
                        )?;
                    }
                    Ok(false) => {}
                    Err(error) => {
                        repl_note(&mut live_repl, &format!("\x1b[31m{error:#}\x1b[0m\n"))?
                    }
                },
                ReplSlashCommand::Models => {
                    // Switches this session's pinned model; the change takes
                    // effect from the next turn without a daemon reload.
                    let argument = command_args.trim();
                    let result = run_models_for_session(
                        paths,
                        parse_models_argument(argument),
                        Some(&active_session_id),
                    )
                    .await;
                    if let Err(error) = result {
                        repl_note(&mut live_repl, &format!("\x1b[31m{error:#}\x1b[0m\n"))?;
                        continue;
                    }
                    let session_config =
                        footer_config_for_session(paths, &config, &active_session_id);
                    let (state, _) =
                        repl_active_or_default_state(paths, &active_session_id).await?;
                    cumulative_tokens = state_cumulative(&state);
                    footer = ReplFooterStatus::from_config(
                        &session_config,
                        state.context_tokens,
                        cumulative_tokens,
                    );
                    let client = OpenAiCompatibleClient::from_config(&session_config, paths)?;
                    let thinking_summary = client.thinking_variant_summary();
                    footer.update_thinking_variant(thinking_summary.as_deref());
                    footer
                        .update_context_window(state.context_window, state.context_window_assumed);
                    // Push the rebuilt footer into the live editor now; without
                    // this the on-screen model label stays stale until the next
                    // input event redraws the editor.
                    live_repl.set_footer(footer.clone());
                    repl_note(
                        &mut live_repl,
                        &format!(
                            "\x1b[2m{}\x1b[0m\n",
                            t(
                                "session model updated; takes effect from the next turn",
                                "会话模型已更新，下一轮生效"
                            )
                        ),
                    )?;
                }
                ReplSlashCommand::Config => {
                    crate::config_tui::run(paths)?;
                    let Some((_, _)) =
                        repl_ipc_admin(paths, &mut live_repl, IpcCommand::ReloadConfig).await?
                    else {
                        continue;
                    };
                    let refreshed = AppConfig::load(paths)?;
                    config = refreshed;
                    let (state, changed) =
                        repl_active_or_default_state(paths, &active_session_id).await?;
                    if changed {
                        apply_repl_session_switch(
                            paths,
                            &config,
                            &state,
                            &mut active_session_id,
                            &mut history,
                            &mut live_repl,
                            &mut footer,
                            &mut cumulative_tokens,
                        )
                        .await?;
                    }
                    cumulative_tokens = state_cumulative(&state);
                    // 同源约束(验收#23):标签与思考程度都取会话作用域配置。
                    let session_config =
                        footer_config_for_session(paths, &config, &active_session_id);
                    footer = ReplFooterStatus::from_config(
                        &session_config,
                        state.context_tokens,
                        cumulative_tokens,
                    );
                    let client = OpenAiCompatibleClient::from_config(&session_config, paths)?;
                    let thinking_summary = client.thinking_variant_summary();
                    footer.update_thinking_variant(thinking_summary.as_deref());
                    footer
                        .update_context_window(state.context_window, state.context_window_assumed);
                    live_repl.set_footer(footer.clone());
                    repl_note(
                        &mut live_repl,
                        &format!("{}\n", t("configuration reloaded", "配置已重新加载")),
                    )?;
                }
                ReplSlashCommand::Variant => {
                    if !crate::models_cache::is_loaded() {
                        repl_note(
                            &mut live_repl,
                            &format!(
                                "{}\n",
                                t(
                                    "model metadata is still loading; try /variant again shortly",
                                    "模型元数据仍在加载，请稍后重试 /variant"
                                )
                            ),
                        )?;
                        continue;
                    }
                    let selected = command_args.trim();
                    let mut client = OpenAiCompatibleClient::from_config(&config, paths)?;
                    match execute_variant(
                        paths,
                        &mut client,
                        (!selected.is_empty()).then_some(selected),
                        "/variant",
                    )? {
                        VariantOutcome::Updated => {
                            let Some((_, _)) =
                                repl_ipc_admin(paths, &mut live_repl, IpcCommand::ReloadConfig)
                                    .await?
                            else {
                                continue;
                            };
                            config = AppConfig::load(paths)?;
                            let (state, changed) =
                                repl_active_or_default_state(paths, &active_session_id).await?;
                            if changed {
                                apply_repl_session_switch(
                                    paths,
                                    &config,
                                    &state,
                                    &mut active_session_id,
                                    &mut history,
                                    &mut live_repl,
                                    &mut footer,
                                    &mut cumulative_tokens,
                                )
                                .await?;
                            }
                            cumulative_tokens = state_cumulative(&state);
                            footer = ReplFooterStatus::from_config(
                                &footer_config_for_session(paths, &config, &active_session_id),
                                state.context_tokens,
                                cumulative_tokens,
                            );
                            let thinking_summary = client.thinking_variant_summary();
                            footer.update_thinking_variant(thinking_summary.as_deref());
                            footer.update_context_window(
                                state.context_window,
                                state.context_window_assumed,
                            );
                            live_repl.set_footer(footer.clone());
                            repl_note(
                                &mut live_repl,
                                &format!("{}\n", t("thinking variants updated", "已更新思考档位")),
                            )?;
                        }
                        VariantOutcome::Cancelled => {}
                        VariantOutcome::Rejected(message) => {
                            repl_note(&mut live_repl, &format!("\x1b[31m{message}\x1b[0m"))?;
                        }
                    }
                }
                ReplSlashCommand::Undo => {
                    let Some((state, data)) = repl_ipc_admin(
                        paths,
                        &mut live_repl,
                        IpcCommand::Undo {
                            target: crate::ipc::SessionRef::Id {
                                id: active_session_id.clone(),
                            },
                        },
                    )
                    .await?
                    else {
                        continue;
                    };
                    let removed = data
                        .get("removed")
                        .and_then(serde_json::Value::as_u64)
                        .unwrap_or(0);
                    repl_note(
                        &mut live_repl,
                        &format!("{}: {removed}\n", t("undone messages", "已撤销消息数")),
                    )?;
                    if let Some(prompt) = data.get("prompt").and_then(serde_json::Value::as_str) {
                        live_repl.editor.input = prompt.to_string();
                        live_repl.editor.cursor = live_repl.editor.input.chars().count();
                        live_repl.editor.history_clean_index = None;
                    }
                    cumulative_tokens = state_cumulative(&state);
                    footer.update_session_tokens(state.context_tokens);
                    footer
                        .update_context_window(state.context_window, state.context_window_assumed);
                    footer.update_cumulative_tokens(cumulative_tokens);
                }
                ReplSlashCommand::Pop => {
                    let count = match parse_repl_pop_count(command_args) {
                        Ok(count) => count,
                        Err(err) => {
                            repl_note(
                                &mut live_repl,
                                &format!("\x1b[31m{}: {err}\x1b[0m", t("error", "错误")),
                            )?;
                            continue;
                        }
                    };
                    let state_store = StateStore::new(paths)?.pinned(&active_session_id);
                    state_store.recover_stale_turns()?;
                    let candidates =
                        state_store.oldest_evictable_visible_turns(count.unwrap_or(usize::MAX))?;
                    let turn_ids = if count.is_some() {
                        candidates.into_iter().map(|turn| turn.turn_id).collect()
                    } else {
                        let all = state_store.oldest_evictable_visible_turns(usize::MAX)?;
                        if all.is_empty() {
                            repl_note(&mut live_repl, &repl_nothing_to_pop_text())?;
                            continue;
                        }
                        let Some(selected) = inline_pop_select(&all)? else {
                            continue;
                        };
                        all.into_iter()
                            .zip(selected)
                            .filter_map(|(turn, selected)| selected.then_some(turn.turn_id))
                            .collect()
                    };
                    let Some((state, data)) = repl_ipc_admin(
                        paths,
                        &mut live_repl,
                        IpcCommand::Pop {
                            target: crate::ipc::SessionRef::Id {
                                id: active_session_id.clone(),
                            },
                            turn_ids,
                        },
                    )
                    .await?
                    else {
                        continue;
                    };
                    let turns = data
                        .get("turns")
                        .and_then(serde_json::Value::as_u64)
                        .unwrap_or(0);
                    if turns > 0 {
                        let outcome = PopOutcome {
                            turns: turns as usize,
                            archived: data
                                .get("archived")
                                .and_then(serde_json::Value::as_bool)
                                .unwrap_or(false),
                        };
                        repl_note(&mut live_repl, &repl_pop_outcome_text(outcome))?;
                    } else {
                        repl_note(&mut live_repl, &repl_nothing_to_pop_text())?;
                    }
                    cumulative_tokens = state_cumulative(&state);
                    footer.update_session_tokens(state.context_tokens);
                    footer
                        .update_context_window(state.context_window, state.context_window_assumed);
                    footer.update_cumulative_tokens(cumulative_tokens);
                }
                ReplSlashCommand::Compact => {
                    repl_note(
                        &mut live_repl,
                        &format!(
                            "\x1b[2m{}\x1b[0m",
                            t("compacting context…", "正在压缩上下文…")
                        ),
                    )?;
                    let Some((state, data)) = repl_ipc_admin(
                        paths,
                        &mut live_repl,
                        IpcCommand::Compact {
                            target: crate::ipc::SessionRef::Id {
                                id: active_session_id.clone(),
                            },
                        },
                    )
                    .await?
                    else {
                        continue;
                    };
                    if let Some(usage) = data
                        .get("usage")
                        .cloned()
                        .filter(|value| !value.is_null())
                        .map(serde_json::from_value::<Usage>)
                        .transpose()?
                    {
                        repl_note(
                            &mut live_repl,
                            &format!("\x1b[2m{}\x1b[0m\n", t("context compacted", "上下文已压缩")),
                        )?;
                        let result = ChatResult {
                            content: String::new(),
                            reasoning: None,
                            usage: Some(usage),
                            usage_estimated: data
                                .get("usage_estimated")
                                .and_then(serde_json::Value::as_bool)
                                .unwrap_or(false),
                            tool_calls: Vec::new(),
                            provider_id: None,
                            model: None,
                            finish_reason: None,
                            thinking_signature: None,
                            last_request_usage: None,
                            responses_continuation: None,
                        };
                        print_chat_token_usage(
                            &result,
                            config.display.show_token_usage,
                            state.context_tokens,
                            state.context_window,
                            state_cumulative(&state),
                        )?;
                    } else {
                        repl_note(
                            &mut live_repl,
                            &format!(
                                "\x1b[2m{}\x1b[0m\n",
                                t("nothing to compact", "没有可压缩的上下文")
                            ),
                        )?;
                    }
                    cumulative_tokens = state_cumulative(&state);
                    footer.update_session_tokens(state.context_tokens);
                    footer
                        .update_context_window(state.context_window, state.context_window_assumed);
                    footer.update_cumulative_tokens(cumulative_tokens);
                }
                ReplSlashCommand::ResetMemory => {
                    // 不二次确认:只清长期记忆,会话历史/技能/知识库都不动。
                    let Some((_, _)) = repl_ipc_admin(
                        paths,
                        &mut live_repl,
                        IpcCommand::ResetMemory {
                            mode: (mode == AgentMode::Dev).then(|| "dev".to_string()),
                        },
                    )
                    .await?
                    else {
                        continue;
                    };
                    repl_note(
                        &mut live_repl,
                        &format!(
                            "\x1b[2m{}\x1b[0m\n",
                            t("long-term memory erased", "长期记忆已清空")
                        ),
                    )?;
                }
                ReplSlashCommand::Reset => {
                    let Some((state, _)) = repl_ipc_admin(
                        paths,
                        &mut live_repl,
                        IpcCommand::ResetConversation {
                            target: crate::ipc::SessionRef::Id {
                                id: active_session_id.clone(),
                            },
                        },
                    )
                    .await?
                    else {
                        continue;
                    };
                    live_repl.editor.input.clear();
                    live_repl.editor.cursor = 0;
                    // The footer numbers are only half of it: the loop's own Σ
                    // accumulator has to go too, or the next config reload
                    // rebuilds the footer from the pre-reset total. The queue
                    // rows were deleted in the store, so the strip has to be
                    // reloaded rather than left showing them.
                    cumulative_tokens = TurnTokens::default();
                    footer.reset_token_usage(state.context_tokens, state.context_window);
                    // 存下新数字还不够:footer 不重绘,屏幕上的 Σ 就一直
                    // 挂着重置前的累计(验收问题四)。
                    live_repl.refresh_footer(footer.clone())?;
                    reload_repl_queue(&mut live_repl, paths, &active_session_id)?;
                    repl_note(
                        &mut live_repl,
                        &format!(
                            "\x1b[2m{}\x1b[0m\n",
                            t("cleared current conversation history", "已清空当前会话历史")
                        ),
                    )?;
                }
                ReplSlashCommand::Wipe => {
                    repl_note(
                        &mut live_repl,
                        &format!("\x1b[2m{}\x1b[0m\n", wipe_summary()),
                    )?;
                    if !confirm_inline(&mut live_repl, t("wipe everything?", "确认全部抹掉？"))?
                    {
                        repl_note(
                            &mut live_repl,
                            &format!("\x1b[2m{}\x1b[0m\n", t("cancelled", "已取消")),
                        )?;
                        continue;
                    }
                    let Some((state, _)) =
                        repl_ipc_admin(paths, &mut live_repl, IpcCommand::WipePersona).await?
                    else {
                        continue;
                    };
                    live_repl.editor.input.clear();
                    live_repl.editor.cursor = 0;
                    cumulative_tokens = TurnTokens::default();
                    footer.reset_token_usage(state.context_tokens, state.context_window);
                    live_repl.refresh_footer(footer.clone())?;
                    reload_repl_queue(&mut live_repl, paths, &active_session_id)?;
                    repl_note(
                        &mut live_repl,
                        &format!("\x1b[2m{}\x1b[0m\n", print_wipe_message()),
                    )?;
                }
            }
            continue;
        }
        if input.is_empty() {
            continue;
        }
        push_history_capped(&mut history, input);
        live_repl.editor.record_history(input);
        persist_repl_history_entry(paths, &active_session_id, input);
        match try_run_remote_chat(
            paths,
            Some(&mut live_repl),
            input,
            None,
            false,
            mode,
            &images,
            Some(active_session_id.clone()),
            Some(&jobs_feed),
        )
        .await
        {
            Ok(Some(summary)) => {
                cumulative_tokens = summary.cumulative_tokens;
                footer.update_token_usage(
                    &summary.result,
                    summary.context_tokens,
                    summary.context_window,
                    cumulative_tokens,
                );
                // Refresh the job strip right away — a background command
                // spawned this turn must show up without waiting a poll.
                if let Ok((mut jobs, _, wake_runs)) = fetch_jobs_overview(paths).await {
                    retain_session_jobs(
                        &mut jobs,
                        jobs_shared.repl_session.lock().unwrap().as_deref(),
                    );
                    *jobs_shared.jobs.lock().unwrap() = jobs.clone();
                    *jobs_shared.wake_runs.lock().unwrap() = wake_runs;
                    live_repl.set_jobs(jobs);
                }
                live_repl.refresh_footer(footer.clone())?;
            }
            Ok(None) => bail!(
                "{}",
                t(
                    "the Miyu Web core stopped; start the REPL again to use direct mode",
                    "Miyu Web 核心已停止；请重新启动 REPL 以使用直连模式"
                )
            ),
            Err(err) if is_remote_turn_detached(&err) => {
                let frame = format!(
                    "\x1b[2m{}\x1b[0m\n",
                    t(
                        "exited; the reply keeps running in the daemon",
                        "已退出；回复在 daemon 里继续运行"
                    )
                );
                live_repl.apply_output_frame(frame.as_bytes())?;
                break;
            }
            Err(err)
                if is_remote_turn_cancelled(&err)
                    || crate::question::is_question_cancelled(&err) =>
            {
                let frame = format!("\x1b[2m{}\x1b[0m\n\n", t("cancelled", "已取消"));
                live_repl.apply_output_frame(frame.as_bytes())?;
                // The interrupted turn still entered the context; refresh the
                // footer from the daemon's post-cancel state.
                if let Ok((state, _)) =
                    repl_active_or_default_state(paths, &active_session_id).await
                {
                    cumulative_tokens = state_cumulative(&state);
                    footer.update_session_tokens(state.context_tokens);
                    footer.update_cumulative_tokens(state_cumulative(&state));
                    footer
                        .update_context_window(state.context_window, state.context_window_assumed);
                }
            }
            Err(err) => {
                let frame = format!("\x1b[31m{}: {err}\x1b[0m\n\n", t("error", "错误"));
                live_repl.apply_output_frame(frame.as_bytes())?;
                if let Ok((state, true)) =
                    repl_active_or_default_state(paths, &active_session_id).await
                {
                    apply_repl_session_switch(
                        paths,
                        &config,
                        &state,
                        &mut active_session_id,
                        &mut history,
                        &mut live_repl,
                        &mut footer,
                        &mut cumulative_tokens,
                    )
                    .await?;
                }
            }
        }
    }
    Ok(())
}
