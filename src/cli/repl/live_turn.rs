//! 直接在终端里驱动的回合（不经过 daemon）。
//!
//! `run_live_agent_turn` 与 `handle_live_agent_event` 是本地事件泵。和
//! [`super::remote`] 的远端泵是两条独立的分发表——08-17 那次「回合中途 footer
//! 不刷新」正是因为只有一边接了 `chat.round_usage`，加事件时两边都要过一遍。

use crate::cli::repl::editor::*;
use crate::cli::repl::tail::*;
use crate::cli::*;

pub(in crate::cli) async fn handle_live_post_turn_overflow(
    live: &mut LiveReplTail,
    agent: &Agent,
    renderer: &mut render::StreamRenderer,
    context_tokens: u64,
    show_token_usage: bool,
    cumulative_tokens: Option<&mut TurnTokens>,
) -> Result<Option<crate::llm::ChatResult>> {
    let compact_result = agent
        .handle_overflow_after_turn(context_tokens, |event| {
            handle_live_agent_event(live, renderer, event)
        })
        .await?;
    renderer.finish()?;
    live.apply_renderer_frame(renderer)?;
    if let Some(compact_result) = compact_result {
        let mut cumulative_display = TurnTokens::default();
        if let Some(total) = cumulative_tokens {
            if let Some(usage) = compact_result.usage.as_ref() {
                total.add(TurnTokens::from_usage(Some(usage)));
                cumulative_display = *total;
            }
        }
        if show_token_usage {
            if let Some(usage) = compact_result.usage.as_ref() {
                let frame = render::token_usage_output(
                    &turn_meter(
                        TurnTokens::from_usage(Some(usage)),
                        GenerationSpeed::default(),
                        agent.effective_context_tokens()?,
                        agent.context_window(),
                        cumulative_display,
                    ),
                    compact_result.usage_estimated,
                );
                live.apply_output_frame(frame.strip_suffix('\n').unwrap_or(&frame).as_bytes())?;
            }
        }
        return Ok(Some(compact_result));
    }
    Ok(None)
}

pub(in crate::cli) fn handle_live_agent_event(
    live: &mut LiveReplTail,
    renderer: &mut render::StreamRenderer,
    event: AgentEvent,
) -> Result<()> {
    let event = match event {
        AgentEvent::Chunk(chunk) => {
            live.queue_stream_chunk(chunk);
            return Ok(());
        }
        AgentEvent::RoundUsage {
            round, turn, speed, ..
        } => {
            // 一次模型请求刚结束:立即刷新 footer 计量,不等整个回合。
            // prompt+completion 即该请求结束时的上下文实际占用。
            let context_tokens = round.prompt_tokens.saturating_add(round.completion_tokens);
            return live.refresh_round_usage(context_tokens, turn, speed);
        }
        event => event,
    };
    if live.external_output_active && matches!(&event, AgentEvent::SpinnerTick) {
        return Ok(());
    }
    if matches!(&event, AgentEvent::SpinnerTick) {
        return live.tick_spinner(renderer);
    }
    match event {
        AgentEvent::PrepareForExternalOutput { ready } => {
            let result = (|| {
                live.flush_pending_chunks(renderer)?;
                renderer.prepare_for_external_output()?;
                live.apply_renderer_frame(renderer)?;
                synchronized_terminal_update(CursorAfterUpdate::Hidden, || live.suspend())?;
                live.external_output_active = true;
                Ok(())
            })();
            if result.is_ok() {
                let _ = ready.send(true);
            }
            result
        }
        AgentEvent::QueuedPromptsConsumed {
            prompt_ids, mode, ..
        } => {
            live.flush_pending_chunks(renderer)?;
            renderer.prepare_for_external_output()?;
            live.apply_renderer_frame(renderer)?;
            synchronized_terminal_update(CursorAfterUpdate::Preserve, || {
                live.suspend()?;
                live.consume_queued(&prompt_ids, mode)
            })
        }
        event => {
            let finishes_external_output =
                live.external_output_active && matches!(&event, AgentEvent::ToolResult { .. });
            if live.external_output_active && !finishes_external_output {
                handle_agent_event(renderer, event)?;
                return live.apply_renderer_frame(renderer);
            }
            let question = matches!(&event, AgentEvent::AskQuestion { .. });
            if question {
                live.flush_pending_chunks(renderer)?;
                renderer.prepare_for_external_output()?;
                live.apply_renderer_frame(renderer)?;
                synchronized_terminal_update(CursorAfterUpdate::Hidden, || live.suspend())?;
                handle_agent_event(renderer, event)?;
                // 问题面板只关闭 raw mode 与括号粘贴，键盘增强仍由外层 LiveRawMode 持有
                enable_live_raw_mode()?;
                execute!(io::stdout(), EnableBracketedPaste)?;
                synchronized_terminal_update(CursorAfterUpdate::Shown, || live.resume())?;
                return live.apply_renderer_frame(renderer);
            }
            live.flush_pending_chunks(renderer)?;
            handle_agent_event(renderer, event)?;
            live.apply_renderer_frame(renderer)?;
            if finishes_external_output {
                live.external_output_active = false;
                synchronized_terminal_update(CursorAfterUpdate::Shown, || live.resume())?;
            }
            Ok(())
        }
    }
}

pub(in crate::cli) async fn run_live_agent_turn(
    live: &mut LiveReplTail,
    paths: &MiyuPaths,
    state: &StateStore,
    agent: &mut Agent,
    input: LiveAgentInput<'_>,
    control: &AgentTurnControl,
    renderer: &mut render::StreamRenderer,
) -> Result<Option<crate::llm::ChatResult>> {
    renderer.use_external_cursor_control();
    renderer.use_buffered_output();
    let mut raw = if std::mem::take(&mut live.raw_mode_handoff) {
        LiveRawMode::adopt()
    } else {
        LiveRawMode::start()?
    };
    live.external_output_active = false;
    if !live.rendered {
        live.resume_at(live.output_cursor)?;
    }
    renderer.start_waiting()?;
    live.apply_renderer_frame(renderer)?;

    let result = {
        let live_cell = std::cell::RefCell::new(&mut *live);
        let renderer_cell = std::cell::RefCell::new(&mut *renderer);
        let chat = agent.chat_stream_with_control(input.content, input.images, control, |event| {
            handle_live_agent_event(
                &mut live_cell.borrow_mut(),
                &mut renderer_cell.borrow_mut(),
                event,
            )
        });
        tokio::pin!(chat);
        let mut input_tick = tokio::time::interval(Duration::from_millis(16));
        input_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        input_tick.tick().await;
        loop {
            tokio::select! {
                biased;
                _ = input_tick.tick() => {
                    if terminal_hangup() {
                        std::process::exit(0);
                    }
                    if !event::poll(Duration::ZERO)? {
                        continue;
                    }
                    let event = event::read()?;
                    let mut live = live_cell.borrow_mut();
                    if matches!(
                        &event,
                        Event::Key(KeyEvent {
                            code: KeyCode::Enter,
                            kind,
                            ..
                        }) if *kind != KeyEventKind::Release
                    ) && matches!(
                        // 只拦真命令。以前是 `starts_with('/')`,于是
                        // 「/home/x 这是什么」在回合中途按回车既不发送也不排队,
                        // 文本卡在输入框里——用户看到的是「回车没反应」。
                        parse_repl_input(live.editor.input.trim_start()),
                        ReplInput::Slash(..)
                    ) {
                        // 静默吞掉这次回车：流式渲染中间插系统消息会写坏终端
                        // 帧。输入原样留着，这一轮结束后再回车即可。直连模式
                        // 没有续轮驱动器，`/goal` 也没有运行中执行的意义。
                        continue;
                    }
                    let mode_before = live.mode();
                    match live.editor.handle_event(event, paths, true)? {
                        LiveEditorAction::None => {}
                        LiveEditorAction::Redraw if !live.external_output_active => {
                            synchronized_terminal_update(CursorAfterUpdate::Preserve, || {
                                live.redraw()
                            })?
                        }
                        LiveEditorAction::ClearScreen if !live.external_output_active => {
                            synchronized_terminal_update(CursorAfterUpdate::Preserve, || {
                                live.clear_screen()
                            })?
                        }
                        LiveEditorAction::Redraw | LiveEditorAction::ClearScreen => {}
                        LiveEditorAction::EmptySubmit => {}
                        LiveEditorAction::Submit(submission) => {
                            let prompt = persist_queued_submission(state, &submission)?;
                            live.editor.record_history(ReplHistoryEntry::from_submission(&submission));
                            if live.external_output_active {
                                live.append_queued(prompt);
                            } else {
                                synchronized_terminal_update(CursorAfterUpdate::Preserve, || {
                                    live.enqueue(prompt)
                                })?;
                            }
                        }
                        LiveEditorAction::Interrupt | LiveEditorAction::Exit => break Ok(None),
                    }
                    if live.mode() != mode_before {
                        control.set_mode(live.mode());
                    }
                },
                result = &mut chat => break result.map(Some),
            }
        }
    };

    if matches!(&result, Ok(None)) {
        live.discard_pending_chunks();
    }
    live.external_output_active = false;
    live.flush_pending_chunks(renderer)?;
    renderer.finish()?;
    live.apply_renderer_frame(renderer)?;
    raw.handoff();
    live.raw_mode_handoff = true;
    result
}
