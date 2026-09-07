//! 后台任务唤醒的事件泵。
//!
//! 子代理跑完、闹钟到点这类事件会「唤醒」一个回合，daemon 把它的事件流推给
//! 终端。它和普通回合走的是两条独立的泵，各自维护分发表——08-17 那次「回合
//! 中途 footer 不刷新」就是因为只有一个泵接了 `chat.round_usage`，另一个漏了。

use crate::cli::repl::editor::*;
use crate::cli::repl::tail::*;
use crate::cli::*;

/// Attach to a daemon-initiated wake turn and render it live: streaming
/// content, reasoning, and tool activity, exactly like a user-started turn.
/// ESC detaches (the turn keeps running; the DB report is suppressed because
/// the turn was already rendered here). Typed submissions queue into the
/// wake turn as follow-ups.
pub(in crate::cli) async fn follow_wake_run(
    paths: &MiyuPaths,
    live: &mut LiveReplTail,
    run_id: &str,
    label: &str,
    // 附着期间静默执行的 `/goal` 要落在**这个 REPL 的会话**上，不能拿
    // daemon 的当前会话指针顶替——普通模式的 REPL 早就有自己的会话了。
    session_id: &str,
    jobs_feed: &JobsFeed,
    jobs_shared: &std::sync::Arc<SharedJobsFeed>,
) -> Result<()> {
    let config = AppConfig::load_or_default(paths)?;
    let mut stream = ipc::connect(&paths.ipc_socket()).await?;
    ipc::send(
        &mut stream,
        &IpcRequest::new(IpcCommand::FollowRun {
            run_id: run_id.to_string(),
        }),
    )
    .await?;
    let mut turn_id: Option<String> = match ipc::receive::<IpcFrame>(&mut stream).await? {
        Some(IpcFrame::Accepted { turn_id, .. }) => turn_id,
        // Run already finished — the DB report path will print it instead.
        _ => return Ok(()),
    };

    let mut renderer = render::StreamRenderer::new(
        render::ReasoningDisplayMode::from_config(&config.display.reasoning),
        render::ToolCallDisplayMode::from_config(&config.display.tool_calls),
        false,
        config.display.readable_tool_names,
        config.display.command_output_lines,
    );
    renderer.use_external_cursor_control();
    renderer.use_buffered_output();
    live.external_output_active = false;
    // Print the header straight into the scrollback (not the live frame):
    // it must survive the streaming render that follows.
    {
        live.suspend()?;
        let mut stdout = io::stdout();
        // 目标续轮不打表头：一个长任务会连着跑几十轮，每轮顶一行「第 N 轮」
        // 只会把真正的输出挤散。轮次已经在 footer 上（那是它常驻的位置）。
        let header = if label == crate::tools::goal::GOAL_ROUND_LABEL {
            String::new()
        } else if label.is_empty() {
            crate::i18n::text("⚙ background task finished", "⚙ 后台任务完成").to_string()
        } else {
            format!("⚙ {label}")
        };
        if !header.is_empty() {
            queue!(stdout, Print(format!("\x1b[2m{header}\x1b[0m\r\n\r\n")))?;
        }
        stdout.flush()?;
        live.output_cursor = cursor_position_or(live.output_cursor);
        let output_cursor = live.output_cursor;
        live.resume_at(output_cursor)?;
    }
    renderer.start_waiting()?;
    live.apply_renderer_frame(&mut renderer)?;
    let mut raw = LiveRawMode::start()?;

    let mut spinner_tick = tokio::time::interval(Duration::from_millis(33));
    spinner_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    spinner_tick.tick().await;
    let mut input_tick = tokio::time::interval(Duration::from_millis(16));
    input_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    input_tick.tick().await;
    let mut follow_strip_tick: u32 = 0;

    'outer: loop {
        let recv = ipc::receive::<IpcFrame>(&mut stream);
        tokio::pin!(recv);
        let frame = loop {
            tokio::select! {
                biased;
                _ = input_tick.tick() => {
                    if terminal_hangup() {
                        let _ = send_ipc_command(paths, IpcCommand::Cancel { run_id: run_id.to_string() }).await;
                        std::process::exit(0);
                    }
                    if !event::poll(Duration::ZERO)? {
                        continue;
                    }
                    let event = event::read()?;
                    // 斜杠命令在**编辑器处理回车之前**拦：编辑器一旦处理
                    // Enter 就会清空缓冲区，「输入原样留着」就成了空话——
                    // 显示滞留旧文本，下一次按键才暴露缓冲区其实已经空了。
                    if matches!(
                        &event,
                        crossterm::event::Event::Key(crossterm::event::KeyEvent {
                            code: crossterm::event::KeyCode::Enter,
                            kind,
                            ..
                        }) if *kind != crossterm::event::KeyEventKind::Release
                    ) {
                        let line = live.editor.input.trim_start().to_string();
                        match crate::cli::repl::editor::parse_repl_input(&line) {
                            crate::slash_commands::ReplInput::Slash(
                                crate::slash_commands::ReplSlashCommand::Goal,
                                args,
                            ) => {
                                let args = args.trim().to_string();
                                if args == "edit" {
                                    // 原地变身成「/goal edit <当前目标>」，
                                    // 零输出；没有目标就静默吞掉这次回车。
                                    if crate::cli::repl::session::prefill_goal_edit_input(
                                        paths,
                                        Some(session_id),
                                        live,
                                    ) && !live.external_output_active
                                    {
                                        synchronized_terminal_update(
                                            CursorAfterUpdate::Preserve,
                                            || live.redraw(),
                                        )?;
                                    }
                                    continue;
                                }
                                // 静默执行：edit/pause/clear 会让 daemon 掐掉
                                // 当前续轮（edit 随后按新目标重开一轮），流的
                                // 中断与重启本身就是反馈。
                                let _ = crate::cli::repl::session::send_ipc_admin(
                                    paths,
                                    IpcCommand::Goal {
                                        target: crate::ipc::SessionRef::Id {
                                            id: session_id.to_string(),
                                        },
                                        input: args,
                                    },
                                )
                                .await;
                                live.editor.clear();
                                if !live.external_output_active {
                                    synchronized_terminal_update(
                                        CursorAfterUpdate::Preserve,
                                        || live.redraw(),
                                    )?;
                                }
                                continue;
                            }
                            // 其他命令运行中不可用，也不打提示（流中间的系统
                            // 消息会写坏渲染）：吞掉回车，输入原样留在输入框。
                            crate::slash_commands::ReplInput::Slash(..) => continue,
                            crate::slash_commands::ReplInput::Chat => {}
                        }
                    }
                    match live.editor.handle_event(event, paths, true)? {
                        LiveEditorAction::None => {}
                        LiveEditorAction::Redraw if !live.external_output_active => {
                            synchronized_terminal_update(CursorAfterUpdate::Preserve, || {
                                live.redraw()
                            })?
                        }
                        LiveEditorAction::Redraw | LiveEditorAction::ClearScreen => {}
                        LiveEditorAction::EmptySubmit => {}
                        LiveEditorAction::Submit(submission) => {
                            // 斜杠命令到不了这里：上面的回车闸在编辑器处理
                            // 之前就拦下了。这里只剩普通消息，照常排队。
                            let Some(target_turn) = turn_id.as_deref() else {
                                continue;
                            };
                            if let Ok(prompt) = persist_remote_queued_submission(
                                paths,
                                run_id,
                                target_turn,
                                &submission,
                            )
                            .await
                            {
                                live.editor.record_history(&submission.content);
                                synchronized_terminal_update(
                                    CursorAfterUpdate::Preserve,
                                    || live.enqueue(prompt),
                                )?;
                            }
                        }
                        LiveEditorAction::Interrupt => {
                            // 目标续轮上 Ctrl+C 的意图是「停」，走 WebUI 停止
                            // 按钮同一条取消路径（取消即解除武装）。仅脱离的话，
                            // 续轮在 daemon 里继续跑：用户面对的是一个看起来
                            // 停了、`/goal` 却说「进行中」、还在烧额度的幽灵轮。
                            // 其他后台唤醒保持仅脱离——那些回合不是它发起的。
                            if label == crate::tools::goal::GOAL_ROUND_LABEL {
                                let _ = send_ipc_command(
                                    paths,
                                    IpcCommand::Cancel {
                                        run_id: run_id.to_string(),
                                    },
                                )
                                .await;
                            }
                            break 'outer;
                        }
                        LiveEditorAction::Exit => {
                            // Detach only: the wake turn keeps running.
                            break 'outer;
                        }
                    }
                }
                frame = &mut recv => break frame?,
                _ = spinner_tick.tick() => {
                    // SpinnerTick 经 live 路径冲刷 chunk 缓冲，流式输出靠它。
                    handle_live_agent_event(live, &mut renderer, AgentEvent::SpinnerTick)?;
                    // 状态条是 live tail 的一部分，附着期间同样要持续刷新。
                    follow_strip_tick = follow_strip_tick.wrapping_add(1);
                    if follow_strip_tick % 8 == 0 && !live.external_output_active {
                        if live.set_jobs(jobs_feed.current()) {
                            synchronized_terminal_update(CursorAfterUpdate::Preserve, || {
                                live.redraw()
                            })?;
                        } else {
                            live.tick_job_strip()?;
                        }
                    }
                }
            }
        };
        let Some(IpcFrame::Event { kind, data, .. }) = frame else {
            break;
        };
        match kind.as_str() {
            "turn.started" => {
                turn_id = Some(ipc_text(&data, "turn_id").to_string());
            }
            "assistant.delta" => handle_live_agent_event(
                live,
                &mut renderer,
                AgentEvent::Chunk(ChatStreamChunk {
                    kind: crate::llm::ChatStreamKind::Content,
                    text: ipc_text(&data, "delta").to_string(),
                }),
            )?,
            "reasoning.delta" => handle_live_agent_event(
                live,
                &mut renderer,
                AgentEvent::Chunk(ChatStreamChunk {
                    kind: crate::llm::ChatStreamKind::Reasoning,
                    text: ipc_text(&data, "delta").to_string(),
                }),
            )?,
            "reasoning.start" => handle_live_agent_event(
                live,
                &mut renderer,
                AgentEvent::ReasoningStart {
                    received_at: Instant::now(),
                },
            )?,
            "reasoning.reset" => handle_live_agent_event(
                live,
                &mut renderer,
                AgentEvent::ReasoningReset {
                    received_at: Instant::now(),
                },
            )?,
            "reasoning.part_start" => handle_live_agent_event(
                live,
                &mut renderer,
                AgentEvent::ReasoningPartStart {
                    received_at: Instant::now(),
                },
            )?,
            "reasoning.part_end" => handle_live_agent_event(
                live,
                &mut renderer,
                AgentEvent::ReasoningPartEnd {
                    received_at: Instant::now(),
                },
            )?,
            "reasoning.title" => handle_live_agent_event(
                live,
                &mut renderer,
                AgentEvent::ReasoningTitle(ipc_text(&data, "title").to_string()),
            )?,
            "tool.preparing" => handle_live_agent_event(
                live,
                &mut renderer,
                AgentEvent::ToolPreparing {
                    name: ipc_text(&data, "name").to_string(),
                    batch: data
                        .get("batch")
                        .and_then(serde_json::Value::as_bool)
                        .unwrap_or(false),
                },
            )?,
            "tool.started" => handle_live_agent_event(
                live,
                &mut renderer,
                AgentEvent::ToolCall {
                    call_id: ipc_text(&data, "tool_id").to_string(),
                    name: ipc_text(&data, "name").to_string(),
                    arguments: ipc_text(&data, "arguments").to_string(),
                },
            )?,
            "tool.progress" => handle_live_agent_event(
                live,
                &mut renderer,
                AgentEvent::ToolProgress {
                    call_id: ipc_text(&data, "tool_id").to_string(),
                    name: ipc_text(&data, "name").to_string(),
                    message: ipc_text(&data, "message").to_string(),
                },
            )?,
            "tool.output" => handle_live_agent_event(
                live,
                &mut renderer,
                AgentEvent::CommandOutput {
                    call_id: ipc_text(&data, "tool_id").to_string(),
                    name: ipc_text(&data, "name").to_string(),
                    stream: if ipc_text(&data, "stream") == "stderr" {
                        tools::CommandOutputStream::Stderr
                    } else {
                        tools::CommandOutputStream::Stdout
                    },
                    chunk: ipc_text(&data, "output").as_bytes().to_vec(),
                },
            )?,
            "tool.finished" => handle_live_agent_event(
                live,
                &mut renderer,
                AgentEvent::ToolResult {
                    call_id: ipc_text(&data, "tool_id").to_string(),
                    name: ipc_text(&data, "name").to_string(),
                    ok: data
                        .get("ok")
                        .and_then(serde_json::Value::as_bool)
                        .unwrap_or(false),
                    output: ipc_text(&data, "output").to_string(),
                },
            )?,
            // shellhook/唤醒形态此前没有这个分支,工具图片(表情包/生图/
            // print_image)在客户端被静默丢弃——REPL 形态(one_shot 事件循环)
            // 一直有,唯独这条流漏了。
            "tool.image" => {
                renderer.prepare_for_external_output()?;
                live.apply_renderer_frame(&mut renderer)?;
                synchronized_terminal_update(CursorAfterUpdate::Hidden, || live.suspend())?;
                live.external_output_active = true;
                let state = StateStore::new(paths)?;
                let size = remote_tool_image_size(
                    ipc_text(&data, "name"),
                    ipc_text(&data, "size"),
                    &config,
                );
                if let Err(error) = render_remote_tool_image(&state, &data, size).await {
                    renderer.write_system_message(&format!(
                        "{}: {error}",
                        t("Could not display tool image", "工具图片显示失败")
                    ))?;
                }
                live.external_output_active = false;
                live.output_cursor = cursor_position_or(live.output_cursor);
                live.resume_at(live.output_cursor)?;
                live.apply_renderer_frame(&mut renderer)?;
            }
            "queue.consumed" => {
                let prompt_ids: Vec<String> = data
                    .get("prompt_ids")
                    .and_then(serde_json::Value::as_array)
                    .map(|values| {
                        values
                            .iter()
                            .filter_map(|value| value.as_str().map(str::to_string))
                            .collect()
                    })
                    .unwrap_or_default();
                let consumed_mode = match ipc_text(&data, "mode") {
                    "dev" => AgentMode::Dev,
                    _ => AgentMode::Normal,
                };
                renderer.prepare_for_external_output()?;
                live.apply_renderer_frame(&mut renderer)?;
                synchronized_terminal_update(CursorAfterUpdate::Preserve, || {
                    live.suspend()?;
                    live.consume_queued(&prompt_ids, consumed_mode)
                })?;
            }
            // daemon 一直在发这个事件,可这里没有对应分支,于是逐请求的
            // 计量在 IPC 这一段就掉地上了——WebUI 有(它自己解 SSE),终端
            // 直连模式也有(走本地事件),唯独日常的「终端连 daemon」要等整
            // 个回合结束才动。
            "chat.round_usage" => {
                let usage = data.get("usage").cloned().unwrap_or_default();
                // prompt+completion 即该请求结束时的上下文实际占用,与
                // 本地事件那条路取同一个口径。
                let context_tokens = ipc_u64(&usage, "prompt_tokens")
                    .saturating_add(ipc_u64(&usage, "completion_tokens"));
                live.refresh_round_usage(
                    context_tokens,
                    TurnTokens {
                        total: ipc_u64(&data, "turn_total"),
                        prompt: ipc_u64(&data, "turn_prompt"),
                        cache_read: ipc_u64(&data, "turn_cache_read"),
                    },
                )?;
            }
            "generation.superseded" => handle_live_agent_event(
                live,
                &mut renderer,
                AgentEvent::ReasoningReset {
                    received_at: Instant::now(),
                },
            )?,
            "run.completed" | "run.failed" | "run.cancelled" => {
                break;
            }
            _ => {}
        }
    }

    // Flush chunks still buffered when the terminal frame arrived — the
    // final content burst lands right before run.completed.
    live.flush_pending_chunks(&mut renderer)?;
    renderer.finish()?;
    live.apply_renderer_frame(&mut renderer)?;
    raw.handoff();
    live.raw_mode_handoff = true;
    // Suppress the duplicate DB report for a turn that was rendered live.
    if let Some(turn_id) = turn_id {
        let mut rendered = jobs_shared.rendered_turns.lock().unwrap();
        if rendered.len() >= JOBS_FEED_MARK_LIMIT {
            rendered.clear();
        }
        rendered.insert(turn_id);
    }
    Ok(())
}
