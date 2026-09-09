//! 单次远端回合（`miyu "问题"` 这种用法）。
//!
//! 和 [`super::interactive`] 共用同一套 IPC 事件流，但生命周期完全不同：跑完
//! 就退，不进 REPL 循环，也就不需要活动区与输入编辑那一整套。

use crate::cli::repl::editor::*;
use crate::cli::repl::tail::*;
use crate::cli::*;

pub(in crate::cli) async fn try_run_remote_chat(
    paths: &MiyuPaths,
    mut live: Option<&mut LiveReplTail>,
    message: &str,
    show_reasoning: Option<bool>,
    plain: bool,
    mode: AgentMode,
    images: &[Option<crate::clipboard::PastedImage>],
    session_override: Option<String>,
    jobs_feed: Option<&JobsFeed>,
    overrides: Option<crate::ipc::TurnOverrides>,
) -> Result<Option<RemoteTurnSummary>> {
    let refreshed_paths = if direct_mode_requested() {
        None
    } else {
        // ensure_daemon also restarts a daemon left over from an older build.
        // Re-resolve paths because that shutdown may complete legacy layout migration.
        ipc::ensure_daemon(paths, None).await?;
        Some(MiyuPaths::new()?)
    };
    let paths = refreshed_paths.as_ref().unwrap_or(paths);
    let mut stream = if direct_mode_requested() {
        match ipc::connect(&paths.ipc_socket()).await {
            Ok(stream) => stream,
            Err(_) => return Ok(None),
        }
    } else {
        ipc::connect(&paths.ipc_socket()).await?
    };
    // Turns run in parallel daemon-side: a running turn in this session does
    // not block a new one (the old multi-process placeholder semantics).
    let state_probe = StateStore::new(paths)?;
    // 这一轮跑在哪个会话上：流式渲染中途静默执行 `/goal` 需要它。
    let turn_session_id = session_override
        .clone()
        .unwrap_or_else(|| state_probe.session_id().to_string());
    let state_probe = session_override
        .as_deref()
        .map(|session_id| state_probe.pinned(session_id))
        .unwrap_or(state_probe);
    ipc::send(
        &mut stream,
        &IpcRequest::new(IpcCommand::StartTurn {
            content: message.to_string(),
            mode: ipc_mode_name(mode).to_string(),
            images: ipc_images(images),
            cwd: std::env::current_dir().ok(),
            session_id: session_override,
            // REPL 常驻连接,后台任务有自己的 FollowWake 通道;只有阅后即焚的
            // 单次/shellhook 触发才需要记下终端供 daemon 回写。
            origin_tty: if live.is_none() {
                detect_origin_tty()
            } else {
                None
            },
            overrides,
        }),
    )
    .await?;
    let Some(first) = ipc::receive::<IpcFrame>(&mut stream).await? else {
        bail!("Miyu core closed the connection before accepting the turn");
    };
    let run_id = match first {
        IpcFrame::Accepted { run_id, .. } => run_id,
        IpcFrame::Error { message, .. } => bail!("{message}"),
        _ => bail!("Miyu core returned an invalid response"),
    };
    let mut turn_id: Option<String> = None;

    let config = AppConfig::load_or_default(paths)?;
    let reasoning_mode = if show_reasoning == Some(false) {
        render::ReasoningDisplayMode::Hidden
    } else {
        render::ReasoningDisplayMode::from_config(&config.display.reasoning)
    };
    let tool_call_mode = if plain {
        render::ToolCallDisplayMode::Hidden
    } else {
        render::ToolCallDisplayMode::from_config(&config.display.tool_calls)
    };
    let mut renderer = render::StreamRenderer::new(
        reasoning_mode,
        tool_call_mode,
        plain,
        config.display.readable_tool_names,
        config.display.command_output_lines,
    );
    let queue_state = Some(state_probe);
    if let Some(live) = live.as_deref_mut() {
        renderer.use_external_cursor_control();
        renderer.use_buffered_output();
        live.external_output_active = false;
        if !live.rendered {
            live.resume_at(live.output_cursor)?;
        }
    }
    // Keep the terminal in raw mode during the turn so the editor stays
    // interactive: typed input is queued for the running turn, mirroring the
    // direct REPL's input pump.
    let mut raw = match live.as_deref_mut() {
        Some(live) => Some(if std::mem::take(&mut live.raw_mode_handoff) {
            LiveRawMode::adopt()
        } else {
            LiveRawMode::start()?
        }),
        None => None,
    };
    renderer.start_waiting()?;
    if let Some(live) = live.as_deref_mut() {
        live.apply_renderer_frame(&mut renderer)?;
    }
    let mut content = String::new();
    let mut reasoning = String::new();
    let mut spinner_tick = tokio::time::interval(Duration::from_millis(33));
    let mut job_strip_tick: u32 = 0;
    spinner_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    spinner_tick.tick().await;
    let mut input_tick = tokio::time::interval(Duration::from_millis(16));
    input_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    input_tick.tick().await;
    let completion = loop {
        // The receive future must survive across select iterations: dropping
        // it after it consumed the 4-byte length prefix (but before the
        // payload arrived) would desynchronize the frame stream.
        let recv = ipc::receive::<IpcFrame>(&mut stream);
        tokio::pin!(recv);
        let frame = loop {
            tokio::select! {
                biased;
                _ = input_tick.tick(), if live.is_some() => {
                    if terminal_hangup() {
                        // 终端没了但回合是 daemon 的:观众离席,戏照演。
                        std::process::exit(0);
                    }
                    if !event::poll(Duration::ZERO)? {
                        continue;
                    }
                    let event = event::read()?;
                    let Some(live_tail) = live.as_deref_mut() else {
                        continue;
                    };
                    if matches!(
                        &event,
                        Event::Key(KeyEvent {
                            code: KeyCode::Enter,
                            kind,
                            ..
                        }) if *kind != KeyEventKind::Release
                    ) {
                        // 斜杠命令在编辑器处理回车之前拦（编辑器一处理就会
                        // 清空缓冲区）。流式渲染中间不打任何本地输出。
                        let line = live_tail.editor.input.trim_start().to_string();
                        match parse_repl_input(&line) {
                            ReplInput::Slash(
                                crate::slash_commands::ReplSlashCommand::Goal,
                                args,
                            ) => {
                                let args = args.trim().to_string();
                                if args == "edit" {
                                    // 原地变身成「/goal edit <当前目标>」；
                                    // 没有目标就静默吞掉这次回车。
                                    if super::super::session::prefill_goal_edit_input(
                                        paths,
                                        Some(turn_session_id.as_str()),
                                        live_tail,
                                    ) && !live_tail.external_output_active
                                    {
                                        synchronized_terminal_update(
                                            CursorAfterUpdate::Preserve,
                                            || live_tail.redraw(),
                                        )?;
                                    }
                                    continue;
                                }
                                // 静默执行；后果由 daemon 体现（edit/pause
                                // 会掐掉正在跑的续轮）。
                                let _ = super::super::session::send_ipc_admin(
                                    paths,
                                    IpcCommand::Goal {
                                        target: crate::ipc::SessionRef::Id {
                                            id: turn_session_id.clone(),
                                        },
                                        input: args,
                                    },
                                )
                                .await;
                                live_tail.editor.clear();
                                if !live_tail.external_output_active {
                                    synchronized_terminal_update(
                                        CursorAfterUpdate::Preserve,
                                        || live_tail.redraw(),
                                    )?;
                                }
                                continue;
                            }
                            // 其余命令静默吞掉回车，输入原样留着，这一轮
                            // 结束后再回车即可。
                            ReplInput::Slash(..) => continue,
                            ReplInput::Chat => {}
                        }
                    }
                    match live_tail.editor.handle_event(event, paths, true)? {
                        LiveEditorAction::None => {}
                        LiveEditorAction::Redraw if !live_tail.external_output_active => {
                            synchronized_terminal_update(CursorAfterUpdate::Preserve, || {
                                live_tail.redraw()
                            })?
                        }
                        LiveEditorAction::ClearScreen if !live_tail.external_output_active => {
                            synchronized_terminal_update(CursorAfterUpdate::Preserve, || {
                                live_tail.clear_screen()
                            })?
                        }
                        LiveEditorAction::Redraw | LiveEditorAction::ClearScreen => {}
                        LiveEditorAction::EmptySubmit => {}
                        LiveEditorAction::Submit(submission) => {
                            // 斜杠命令到不了这里：回车闸在编辑器处理之前就
                            // 拦下了。这里只剩普通消息，照常排队。
                            let Some(target_turn_id) = turn_id.as_deref() else {
                                live_tail.editor.input = submission.display_content.clone();
                                live_tail.editor.cursor = live_tail.editor.input.chars().count();
                                renderer.write_system_message(t(
                                    "the reply is still starting; try sending the follow-up again",
                                    "当前回复仍在启动，请稍后重新发送追加消息",
                                ))?;
                                live_tail.apply_renderer_frame(&mut renderer)?;
                                continue;
                            };
                            match persist_remote_queued_submission(
                                paths,
                                &run_id,
                                target_turn_id,
                                &submission,
                            ).await {
                                Ok(prompt) => {
                                    live_tail.editor.record_history(ReplHistoryEntry::from_submission(&submission));
                                    if live_tail.external_output_active {
                                        live_tail.append_queued(prompt);
                                    } else {
                                        synchronized_terminal_update(
                                            CursorAfterUpdate::Preserve,
                                            || live_tail.enqueue(prompt),
                                        )?;
                                    }
                                }
                                Err(_) => {
                                    live_tail.editor.input =
                                        submission.display_content.clone();
                                    live_tail.editor.cursor =
                                        live_tail.editor.input.chars().count();
                                    renderer.write_system_message(t(
                                        "could not queue the message; the reply may have just finished",
                                        "无法排队消息；当前回复可能刚刚结束",
                                    ))?;
                                    live_tail.apply_renderer_frame(&mut renderer)?;
                                }
                            }
                        }
                        LiveEditorAction::Interrupt => {
                            let _ = send_ipc_command(
                                paths,
                                IpcCommand::Cancel { run_id: run_id.clone() },
                            )
                            .await;
                        }
                        LiveEditorAction::Exit => {
                            renderer.finish()?;
                            if let Some(live) = live.as_deref_mut() {
                                live.stop_footer_spinner()?;
                                live.apply_renderer_frame(&mut renderer)?;
                            }
                            return Err(anyhow::Error::new(RemoteTurnDetached));
                        }
                    }
                },
                frame = &mut recv => break frame?,
                _ = spinner_tick.tick() => {
                    // 外部输出期间活动区是挂起的(rendered=false),这时
                    // apply_output_frame 会把帧直接写在光标当下的位置——
                    // 也就是工具刚打完的图片中间。文本只盖住左边一截,右
                    // 边残留的 Kitty 占位字符继续渲染对应那行的图片切片,
                    // 屏幕上就是一条条横带。
                    //
                    // 进程内那条路(handle_live_agent_event)早就把外部输
                    // 出期间的 SpinnerTick 整个丢掉了,远端这条漏了。丢掉
                    // 不会少画:tool.finished 会先 resume_at 再统一出帧。
                    if live
                        .as_deref()
                        .is_some_and(|live| live.external_output_active)
                    {
                        continue;
                    }
                    renderer.tick_spinner()?;
                    if let Some(live) = live.as_deref_mut() {
                        live.apply_renderer_frame(&mut renderer)?;
                        // footer 里的运行转轮与等待动画同源推进。
                        live.tick_footer_spinner()?;
                        // The job strip is part of the live tail, so it keeps
                        // rendering during streaming; throttle to ~every 8th
                        // spinner frame.
                        if let Some(feed) = jobs_feed {
                            job_strip_tick = job_strip_tick.wrapping_add(1);
                            if job_strip_tick % 8 == 0 && !live.external_output_active {
                                if live.set_jobs(feed.current()) {
                                    synchronized_terminal_update(
                                        CursorAfterUpdate::Preserve,
                                        || live.redraw(),
                                    )?;
                                } else {
                                    live.tick_job_strip()?;
                                }
                            }
                        }
                    }
                }
                _ = tokio::signal::ctrl_c() => {
                    let _ = send_ipc_command(
                        paths,
                        IpcCommand::Cancel { run_id: run_id.clone() },
                    ).await;
                    renderer.finish()?;
                    if let Some(live) = live.as_deref_mut() {
                        live.apply_renderer_frame(&mut renderer)?;
                    }
                    return Err(anyhow::Error::new(RemoteTurnCancelled));
                }
            }
        };
        let Some(frame) = frame else {
            renderer.finish()?;
            if let Some(live) = live.as_deref_mut() {
                live.apply_renderer_frame(&mut renderer)?;
            }
            bail!("Miyu core disconnected during the turn");
        };
        let IpcFrame::Event { kind, data, .. } = frame else {
            if let IpcFrame::Error { message, .. } = frame {
                renderer.finish()?;
                if let Some(live) = live.as_deref_mut() {
                    live.apply_renderer_frame(&mut renderer)?;
                }
                bail!("{message}");
            }
            continue;
        };
        match kind.as_str() {
            "turn.started" => {
                let id = ipc_text(&data, "turn_id");
                if !id.is_empty() {
                    turn_id = Some(id.to_string());
                }
            }
            "assistant.delta" => {
                let delta = ipc_text(&data, "delta");
                content.push_str(delta);
                handle_agent_event(
                    &mut renderer,
                    AgentEvent::Chunk(ChatStreamChunk {
                        kind: crate::llm::ChatStreamKind::Content,
                        text: delta.to_string(),
                    }),
                )?;
            }
            "reasoning.delta" => {
                let delta = ipc_text(&data, "delta");
                reasoning.push_str(delta);
                handle_agent_event(
                    &mut renderer,
                    AgentEvent::Chunk(ChatStreamChunk {
                        kind: crate::llm::ChatStreamKind::Reasoning,
                        text: delta.to_string(),
                    }),
                )?;
            }
            "reasoning.start" => handle_agent_event(
                &mut renderer,
                AgentEvent::ReasoningStart {
                    received_at: Instant::now(),
                },
            )?,
            "reasoning.reset" => {
                reasoning.clear();
                handle_agent_event(
                    &mut renderer,
                    AgentEvent::ReasoningReset {
                        received_at: Instant::now(),
                    },
                )?;
            }
            "reasoning.part_start" => handle_agent_event(
                &mut renderer,
                AgentEvent::ReasoningPartStart {
                    received_at: Instant::now(),
                },
            )?,
            "reasoning.part_end" => handle_agent_event(
                &mut renderer,
                AgentEvent::ReasoningPartEnd {
                    received_at: Instant::now(),
                },
            )?,
            "reasoning.title" => handle_agent_event(
                &mut renderer,
                AgentEvent::ReasoningTitle(ipc_text(&data, "title").to_string()),
            )?,
            "tool.preparing" => handle_agent_event(
                &mut renderer,
                AgentEvent::ToolPreparing {
                    name: ipc_text(&data, "name").to_string(),
                    batch: data
                        .get("batch")
                        .and_then(serde_json::Value::as_bool)
                        .unwrap_or(false),
                },
            )?,
            "tool.started" => handle_agent_event(
                &mut renderer,
                AgentEvent::ToolCall {
                    call_id: ipc_text(&data, "tool_id").to_string(),
                    name: ipc_text(&data, "name").to_string(),
                    arguments: ipc_text(&data, "arguments").to_string(),
                },
            )?,
            "tool.progress" => handle_agent_event(
                &mut renderer,
                AgentEvent::ToolProgress {
                    call_id: ipc_text(&data, "tool_id").to_string(),
                    name: ipc_text(&data, "name").to_string(),
                    message: ipc_text(&data, "message").to_string(),
                },
            )?,
            "tool.output" => handle_agent_event(
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
            "tool.finished" => {
                handle_agent_event(
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
                )?;
                if let Some(live) = live.as_deref_mut() {
                    if live.external_output_active {
                        live.external_output_active = false;
                        live.output_cursor = cursor_position_or(live.output_cursor);
                        live.resume_at(live.output_cursor)?;
                        live.apply_renderer_frame(&mut renderer)?;
                    }
                }
            }
            "tool.image" => {
                renderer.prepare_for_external_output()?;
                if let Some(live) = live.as_deref_mut() {
                    live.apply_renderer_frame(&mut renderer)?;
                    synchronized_terminal_update(CursorAfterUpdate::Hidden, || live.suspend())?;
                    live.external_output_active = true;
                }
                let state = queue_state
                    .as_ref()
                    .expect("queue state exists for a remote turn");
                let size = remote_tool_image_size(
                    ipc_text(&data, "name"),
                    ipc_text(&data, "size"),
                    &config,
                );
                if let Err(error) = render_remote_tool_image(state, &data, size).await {
                    renderer.write_system_message(&format!(
                        "{}: {error}",
                        t("Could not display tool image", "工具图片显示失败")
                    ))?;
                }
                // 图片打完不用再单独「抬进页内」:残影的根因不在图片的位置,而在
                // 受限区滚动本身(见 tail/frame.rs 的 queue_lifted_frame),此后的
                // 帧都会改走整屏滚,活动区 resume 时自己会把光标下方的溢出滚掉。
            }
            "question.requested" => {
                renderer.prepare_for_external_output()?;
                if let Some(live) = live.as_deref_mut() {
                    live.apply_renderer_frame(&mut renderer)?;
                    synchronized_terminal_update(CursorAfterUpdate::Hidden, || live.suspend())?;
                }
                let request = crate::question::QuestionRequest {
                    questions: serde_json::from_value(
                        data.get("questions").cloned().unwrap_or_default(),
                    )?,
                };
                notify_if_unfocused(
                    &config,
                    live.as_deref().map(|live| live.editor.focused),
                    t("Miyu is waiting on you", "Miyu 在等你回答"),
                    request
                        .questions
                        .first()
                        .map(|prompt| prompt.question.as_str())
                        .unwrap_or_default(),
                );
                // A panel that cannot be shown is not a reason to abort the
                // turn: fall through to the same path a closed panel takes, so
                // the daemon gets an answer instead of the run dying on an
                // error the user cannot act on. The direct-mode handler has
                // always done this; this branch used to propagate instead.
                let asked = crate::question_tui::ask(&request).unwrap_or_else(|err| {
                    crate::question::QuestionResponse::Unavailable(err.to_string())
                });
                match asked {
                    crate::question::QuestionResponse::Answered(answers) => {
                        send_ipc_command(
                            paths,
                            IpcCommand::AnswerQuestion {
                                question_id: ipc_text(&data, "question_id").to_string(),
                                answers,
                            },
                        )
                        .await?;
                        renderer.start_waiting()?;
                    }
                    // Nobody could be shown the panel — no tty, or it failed to
                    // open. That is not the user calling the turn off, so the
                    // question is resolved and the turn carries on; the tool
                    // that asked finds out that nobody answered and can say so.
                    crate::question::QuestionResponse::Unavailable(_) => {
                        let _ = send_ipc_command(
                            paths,
                            IpcCommand::CloseQuestion {
                                question_id: ipc_text(&data, "question_id").to_string(),
                            },
                        )
                        .await;
                    }
                    // The terminal question UI maps its close gestures to
                    // Cancelled; that one really is "stop this turn".
                    crate::question::QuestionResponse::Closed
                    | crate::question::QuestionResponse::Cancelled => {
                        let _ = send_ipc_command(
                            paths,
                            IpcCommand::Cancel {
                                run_id: run_id.clone(),
                            },
                        )
                        .await;
                    }
                }
                if let Some(live) = live.as_deref_mut() {
                    live.external_output_active = false;
                    live.output_cursor = cursor_position_or(live.output_cursor);
                    live.resume_at(live.output_cursor)?;
                }
            }
            "queue.consumed" => {
                if let Some(live) = live.as_deref_mut() {
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
            }
            "queue.removed" => {
                if let Some(live) = live.as_deref_mut() {
                    if let Some(prompt_id) =
                        data.get("prompt_id").and_then(serde_json::Value::as_str)
                    {
                        synchronized_terminal_update(CursorAfterUpdate::Preserve, || {
                            live.drop_queued(&[prompt_id.to_string()])
                        })?;
                    }
                }
            }
            "generation.superseded" => {
                content.clear();
                reasoning.clear();
                handle_agent_event(
                    &mut renderer,
                    AgentEvent::ReasoningReset {
                        received_at: Instant::now(),
                    },
                )?;
            }
            "context.compact_start" => handle_agent_event(&mut renderer, AgentEvent::CompactStart)?,
            "context.compact_delta" => handle_agent_event(
                &mut renderer,
                AgentEvent::CompactChunk(ChatStreamChunk {
                    kind: crate::llm::ChatStreamKind::Content,
                    text: ipc_text(&data, "delta").to_string(),
                }),
            )?,
            "context.compact_end" => handle_agent_event(&mut renderer, AgentEvent::CompactEnd)?,
            "context.pop_start" => handle_agent_event(&mut renderer, AgentEvent::PopStart)?,
            "context.pop_end" => handle_agent_event(&mut renderer, AgentEvent::PopEnd)?,
            "context.notice" => handle_agent_event(
                &mut renderer,
                AgentEvent::Notice {
                    text: ipc_text(&data, "text").to_string(),
                },
            )?,
            // daemon 每完成一次模型请求就发这个,可这里没有对应分支,于是
            // 逐请求的计量在 IPC 这一段掉地上——回合跑在 daemon 里,CLI 拿
            // 不到就只能等 run.completed 的权威数字,footer 因此整轮不动。
            // WebUI 没这问题:它自己解 SSE。
            "chat.round_usage" => {
                if let Some(live) = live.as_deref_mut() {
                    let usage = data.get("usage").cloned().unwrap_or_default();
                    // prompt+completion 即该请求结束时的上下文实际占用,
                    // 与进程内那条路同一个口径。
                    let context_tokens = ipc_u64(&usage, "prompt_tokens")
                        .saturating_add(ipc_u64(&usage, "completion_tokens"));
                    live.refresh_round_usage(
                        context_tokens,
                        TurnTokens {
                            total: ipc_u64(&data, "turn_total"),
                            prompt: ipc_u64(&data, "turn_prompt"),
                            cache_read: ipc_u64(&data, "turn_cache_read"),
                        },
                        GenerationSpeed {
                            tokens: ipc_u64(&data, "turn_generation_tokens"),
                            millis: ipc_u64(&data, "turn_generation_ms"),
                        },
                    )?;
                }
            }
            "run.completed" => break data,
            "run.failed" => {
                renderer.finish()?;
                if let Some(live) = live.as_deref_mut() {
                    live.apply_renderer_frame(&mut renderer)?;
                }
                bail!("{}", ipc_text(&data, "message"));
            }
            "run.cancelled" => {
                renderer.finish()?;
                if let Some(live) = live.as_deref_mut() {
                    // 提前 return 的取消路径也要熄波浪:漏掉它,输入框贴着
                    // 终端底部取消时最后一帧波浪就留在屏上(08-20 实测)。
                    live.stop_footer_spinner()?;
                    live.apply_renderer_frame(&mut renderer)?;
                }
                return Err(anyhow::Error::new(RemoteTurnCancelled));
            }
            _ => {}
        }
        if let Some(live) = live.as_deref_mut() {
            live.apply_renderer_frame(&mut renderer)?;
        }
    };
    renderer.finish()?;
    let focused = live.as_deref().map(|live| live.editor.focused);
    if let Some(live) = live {
        live.stop_footer_spinner()?;
        live.apply_renderer_frame(&mut renderer)?;
        if let Some(raw) = raw.as_mut() {
            raw.handoff();
            live.raw_mode_handoff = true;
        }
    }

    let result = ChatResult {
        content,
        reasoning: (!reasoning.is_empty()).then_some(reasoning),
        usage: completion
            .get("usage")
            .cloned()
            .filter(|value| !value.is_null())
            .map(serde_json::from_value::<Usage>)
            .transpose()?,
        usage_estimated: completion
            .get("usage_estimated")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false),
        tool_calls: Vec::new(),
        provider_id: completion
            .get("provider_id")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string),
        model: completion
            .get("model")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string),
        finish_reason: None,
        thinking_signature: None,
        last_request_usage: None,
        responses_continuation: None,
    };
    if config.notifications.on_turn_complete {
        notify_if_unfocused(
            &config,
            focused,
            t("Miyu finished replying", "Miyu 回复完成"),
            &result.content,
        );
    }
    print_mixed_model_endpoint(show_mixed_model_endpoint(&config, false), &result, None);
    let context_tokens = completion
        .get("context_tokens")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or_default();
    let context_window = completion
        .get("context_window")
        .and_then(serde_json::Value::as_u64)
        .and_then(|value| usize::try_from(value).ok());
    let completion_u64 = |key: &str| {
        completion
            .get(key)
            .and_then(serde_json::Value::as_u64)
            .unwrap_or_default()
    };
    let cumulative_tokens = TurnTokens {
        total: completion_u64("cumulative_tokens"),
        prompt: completion_u64("cumulative_prompt_tokens"),
        cache_read: completion_u64("cumulative_cache_read_tokens"),
    };
    print_chat_token_usage(
        &result,
        config.display.show_token_usage && !plain,
        context_tokens,
        context_window,
        TurnTokens::from_usage(result.usage.as_ref()),
    )?;
    Ok(Some(RemoteTurnSummary {
        result,
        context_tokens,
        context_window,
        cumulative_tokens,
    }))
}
