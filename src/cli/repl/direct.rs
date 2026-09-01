//! 进程内直连的回合驱动。
//!
//! `MIYU_DIRECT=1` 或 daemon 起不来时走这条：agent 直接在本进程跑，事件不过
//! IPC。功能与远端那条等价，但少一层进程边界——调试和排查时用它能把「是不是
//! IPC 丢了东西」这个变量排除掉。

use crate::cli::repl::editor::*;
use crate::cli::repl::input::*;
use crate::cli::repl::remote::*;
use crate::cli::repl::tail::*;
use crate::cli::*;

pub(in crate::cli) async fn run_chat_with_images(
    paths: &MiyuPaths,
    message: String,
    pasted_images: Vec<Option<crate::clipboard::PastedImage>>,
) -> Result<()> {
    if !direct_mode_requested() {
        match try_run_remote_chat(
            paths,
            None,
            &message,
            None,
            false,
            AgentMode::Normal,
            &pasted_images,
            None,
            None,
        )
        .await
        {
            Ok(Some(_)) => return Ok(()),
            Ok(None) => {}
            Err(err) => return Err(err),
        }
    }
    let _core_lease = ipc::acquire_direct_core(paths)?;
    initialize_models_cache(paths);
    AppConfig::init_files(paths)?;
    let config = AppConfig::load_or_default(paths)?;
    let state = StateStore::new(paths)?;
    state.init_files()?;
    let memory_organizer = MemoryOrganizer::spawn()?;
    let memory_organizer_handle = memory_organizer.handle();
    memory_organizer_handle.wake(config.clone(), paths.clone(), state.clone());
    let client = OpenAiCompatibleClient::from_config(&config, paths)?;
    let registry = build_tool_registry(
        &config,
        paths,
        AgentMode::Normal,
        crate::question_tui::available(false),
    )?;
    let reasoning_mode = render::ReasoningDisplayMode::from_config(&config.display.reasoning);
    let tool_call_mode = render::ToolCallDisplayMode::from_config(&config.display.tool_calls);
    let readable_tool_names = config.display.readable_tool_names;
    let command_output_lines = config.display.command_output_lines;
    let show_token_usage = config.display.show_token_usage;
    let show_mixed_model_endpoint = show_mixed_model_endpoint(&config, false);
    let display_config = config.clone();
    let mut agent = Agent::new(
        config,
        paths,
        state.clone(),
        client,
        registry,
        AgentMode::Normal,
    )?;
    agent.set_memory_organizer(memory_organizer_handle);
    agent.prepare_for_turn()?;
    let mut renderer = render::StreamRenderer::new(
        reasoning_mode,
        tool_call_mode,
        false,
        readable_tool_names,
        command_output_lines,
    );
    renderer.start_waiting()?;
    let result = agent
        .chat_stream_with_images(&message, &pasted_images, |event| {
            handle_agent_event(&mut renderer, event)
        })
        .await;
    renderer.finish()?;
    let result = match result {
        Ok(result) => result,
        Err(err) if crate::question::is_question_cancelled(&err) => return Ok(()),
        Err(err) => return Err(err),
    };
    print_mixed_model_endpoint(show_mixed_model_endpoint, &result, None);
    let mut cumulative_tokens = TurnTokens::from_usage(result.usage.as_ref());
    let context_tokens = agent.effective_context_tokens()?;
    print_chat_token_usage(
        &result,
        show_token_usage,
        context_tokens,
        result_context_window(&display_config, &result).or(agent.context_window()),
        cumulative_tokens,
    )?;
    let overflow_result = handle_post_turn_overflow(
        &agent,
        &mut renderer,
        context_tokens,
        show_token_usage,
        Some(&mut cumulative_tokens),
    )
    .await?;
    let updated_context_tokens = agent.effective_context_tokens()?;
    if overflow_result.is_none() && updated_context_tokens != context_tokens {
        print_chat_token_usage(
            &result,
            show_token_usage,
            updated_context_tokens,
            result_context_window(&display_config, &result).or(agent.context_window()),
            cumulative_tokens,
        )?;
    }
    Ok(())
}

pub(in crate::cli) async fn run_chat_with_options(
    paths: &MiyuPaths,
    message: String,
    show_reasoning: Option<bool>,
    plain: bool,
    mode: AgentMode,
    session: TurnSession,
) -> Result<()> {
    let message = append_stdin_if_piped(message).await;
    if message.is_empty() {
        return run_repl(paths, mode).await;
    }
    if !direct_mode_requested() {
        let session_override = match &session {
            TurnSession::Current => None,
            TurnSession::Explicit(session_id) => Some(session_id.clone()),
            TurnSession::Ephemeral => Some(create_ephemeral_session(paths).await?),
        };
        // Not `?`-through: the throwaway session has to be torn down on the
        // failure path too, otherwise a cancelled turn leaves it behind.
        let outcome = try_run_remote_chat(
            paths,
            None,
            &message,
            show_reasoning,
            plain,
            mode,
            &[],
            session_override.clone(),
            None,
        )
        .await;
        if session == TurnSession::Ephemeral {
            if let Some(session_id) = &session_override {
                discard_ephemeral_session(paths, session_id).await;
            }
        }
        match outcome {
            Ok(Some(_)) => return Ok(()),
            Ok(None) => {}
            Err(err) => return Err(err),
        }
    }
    let _core_lease = ipc::acquire_direct_core(paths)?;
    initialize_models_cache(paths);
    AppConfig::init_files(paths)?;
    let config = AppConfig::load_or_default(paths)?;
    let state = StateStore::new(paths)?;
    state.init_files()?;
    // Direct mode has no daemon to mint the throwaway session, so it makes its
    // own and pins the turn to it.
    let (state, _ephemeral_guard) = if session == TurnSession::Ephemeral {
        let record = state.create_session(
            &config.active_persona_scope(),
            &ephemeral_session_name(),
            crate::state::ASK_SESSION_KIND,
            None,
        )?;
        let guard = EphemeralSessionGuard {
            state: state.clone(),
            session_id: record.session_id.clone(),
        };
        (state.pinned(&record.session_id), Some(guard))
    } else {
        (state, None)
    };
    let memory_organizer = MemoryOrganizer::spawn()?;
    let memory_organizer_handle = memory_organizer.handle();
    memory_organizer_handle.wake(config.clone(), paths.clone(), state.clone());
    let client = OpenAiCompatibleClient::from_config(&config, paths)?;
    let registry =
        build_tool_registry(&config, paths, mode, crate::question_tui::available(plain))?;
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
    let readable_tool_names = config.display.readable_tool_names;
    let command_output_lines = config.display.command_output_lines;
    let show_token_usage = config.display.show_token_usage && !plain;
    let show_mixed_model_endpoint = show_mixed_model_endpoint(&config, false);
    let display_config = config.clone();
    let mut agent = Agent::new(config, paths, state.clone(), client, registry, mode)?;
    agent.set_memory_organizer(memory_organizer_handle);
    agent.prepare_for_turn()?;
    let mut renderer = render::StreamRenderer::new(
        reasoning_mode,
        tool_call_mode,
        plain,
        readable_tool_names,
        command_output_lines,
    );
    renderer.start_waiting()?;
    let result = agent
        .chat_stream(&message, |event| handle_agent_event(&mut renderer, event))
        .await;
    renderer.finish()?;
    let result = match result {
        Ok(result) => result,
        Err(err) if crate::question::is_question_cancelled(&err) => return Ok(()),
        Err(err) => return Err(err),
    };
    print_mixed_model_endpoint(show_mixed_model_endpoint, &result, None);
    let mut cumulative_tokens = TurnTokens::from_usage(result.usage.as_ref());
    let context_tokens = agent.effective_context_tokens()?;
    print_chat_token_usage(
        &result,
        show_token_usage,
        context_tokens,
        result_context_window(&display_config, &result).or(agent.context_window()),
        cumulative_tokens,
    )?;
    let overflow_result = handle_post_turn_overflow(
        &agent,
        &mut renderer,
        context_tokens,
        show_token_usage,
        Some(&mut cumulative_tokens),
    )
    .await?;
    let updated_context_tokens = agent.effective_context_tokens()?;
    if overflow_result.is_none() && updated_context_tokens != context_tokens {
        print_chat_token_usage(
            &result,
            show_token_usage,
            updated_context_tokens,
            result_context_window(&display_config, &result).or(agent.context_window()),
            cumulative_tokens,
        )?;
    }
    Ok(())
}

pub(in crate::cli) async fn run_direct_repl(
    paths: &MiyuPaths,
    initial_mode: AgentMode,
) -> Result<()> {
    let _core_lease = ipc::acquire_direct_core(paths)?;
    initialize_models_cache(paths);
    let _cursor_restore = ReplCursorRestore;
    AppConfig::init_files(paths)?;
    let mut config = AppConfig::load_or_default(paths)?;
    tools::jobs::init(paths);
    let state = StateStore::new(paths)?;
    state.init_files()?;
    // Same lane as the remote REPL: resume where the last REPL was, not where
    // shell-hook happens to be pointing.
    let persona = if initial_mode == AgentMode::Dev {
        crate::state::DEV_PERSONA.to_string()
    } else {
        config.active_persona_scope()
    };
    // 与远端 `GetReplSession` 同一条语义（见 `ensure_repl_session`）：指针缺失
    // 就自举本车道的会话，绝不退到终端集成那条。
    let repl_session_id = state.ensure_repl_session(&persona)?;
    state.adopt_session(&repl_session_id);
    apply_session_model_override(&state, &mut config);
    let memory_organizer = MemoryOrganizer::spawn()?;
    let memory_organizer_handle = memory_organizer.handle();
    memory_organizer_handle.wake(config.clone(), paths.clone(), state.clone());
    let mut client = OpenAiCompatibleClient::from_config(&config, paths)?;
    let mut mode = initial_mode;
    let mut input_history = load_repl_input_history(&state, paths)?;
    let mut prefill = None::<String>;
    let mut live_repl = None::<LiveReplTail>;

    crate::default_kb::check_update_if_due(paths).await.ok();
    if let Ok(Some(message)) = crate::default_kb::notice_if_update_available(paths) {
        println!("\x1b[2m{message}\x1b[0m");
    }
    let mut cumulative_tokens = state.session_cumulative_token_totals().unwrap_or_default();
    let mut show_shortcut_hint = true;
    let initial_registry =
        build_tool_registry(&config, paths, mode, crate::question_tui::available(false))?;
    let mut agent = Agent::new(
        config.clone(),
        paths,
        state.clone(),
        client.clone(),
        initial_registry,
        mode,
    )?;
    agent.set_memory_organizer(memory_organizer_handle);
    agent.prepare_for_turn()?;
    let mut footer = ReplFooterStatus::from_config(
        &config,
        agent.effective_context_tokens()?,
        TurnTokens::default(),
    );
    let thinking_summary = client.thinking_variant_summary();
    footer.update_thinking_variant(thinking_summary.as_deref());
    footer.update_context_window(agent.context_window(), agent.context_window_assumed());
    loop {
        let thinking_summary = client.thinking_variant_summary();
        footer.update_thinking_variant(thinking_summary.as_deref());
        let next_input = if let Some(live) = live_repl.as_mut() {
            live.set_footer(footer.clone());
            let jobs_feed = JobsFeed::Local(Some(state.session_id().to_string()));
            let input = match read_live_repl_input(live, paths, &jobs_feed, None)? {
                LiveReplOutcome::Exit | LiveReplOutcome::FollowWake { .. } => None,
                // Direct mode owns its jobs in-process, so stop them here
                // rather than through the daemon.
                LiveReplOutcome::StopJobs => {
                    for job in crate::tools::jobs::overview() {
                        if job.running {
                            let _ = crate::tools::jobs::stop_job(&job.job_id).await;
                        }
                    }
                    continue;
                }
                LiveReplOutcome::Submit(next_mode, input, images) => {
                    Some((next_mode, input, images))
                }
            };
            // The user moved on: finished background commands count as
            // reported in direct mode (no daemon wake exists here).
            for job in crate::tools::jobs::overview() {
                if !job.running {
                    crate::tools::jobs::acknowledge(&job.job_id);
                }
            }
            input
        } else {
            read_repl_input(
                paths,
                mode,
                prefill.take(),
                &input_history,
                &footer,
                show_shortcut_hint,
            )?
        };
        let (input, pasted_images) = match next_input {
            Some((new_mode, input, pasted_images)) => {
                mode = new_mode;
                (input, pasted_images)
            }
            None => break,
        };
        let input = input.trim();
        // 只按完整命令名比对:前缀展开已从执行路径撤走(见 `parse_repl_input`),
        // 否则 `/d 3` 会静默删掉 3 号会话。
        let (command, command_args) = split_repl_command(input);
        let command_args_empty = command_args.trim().is_empty();
        if input.eq_ignore_ascii_case("exit")
            || input.eq_ignore_ascii_case("quit")
            || (command.eq_ignore_ascii_case("/exit") && command_args_empty)
        {
            break;
        }
        if command.eq_ignore_ascii_case("/help") && command_args_empty {
            print_repl_help();
            continue;
        }
        if command.eq_ignore_ascii_case("/usage") && command_args_empty {
            let snapshot = state.usage_snapshot()?;
            let context_tokens = agent.effective_context_tokens()?;
            let context = Some((context_tokens, agent.context_window()));
            println!("{}", usage_overview_text(&snapshot, context));
            if let Some(window) = agent.context_window() {
                println!(
                    "{}",
                    compact_watermark_text(context_tokens as usize, window, &config.context)
                );
            }
            println!();
            continue;
        }
        if command.eq_ignore_ascii_case("/persona") {
            match run_persona_picker(paths, command_args) {
                Ok(true) => {
                    reload_repl_config(paths, &state, &mut config, &mut client)?;
                    // 人格是会话的命名空间维度:切人格后必须重绑到新人格的
                    // 会话(与启动时 ensure_repl_session 同一条语义),否则 agent
                    // 还挂在旧人格的会话上,人格提示词与历史命名空间错位。
                    let persona = if mode == AgentMode::Dev {
                        crate::state::DEV_PERSONA.to_string()
                    } else {
                        config.active_persona_scope()
                    };
                    let repl_session_id = state.ensure_repl_session(&persona)?;
                    state.adopt_session(&repl_session_id);
                    apply_session_model_override(&state, &mut config);
                    client = OpenAiCompatibleClient::from_config(&config, paths)?;
                    input_history = load_repl_input_history(&state, paths)?;
                    cumulative_tokens =
                        state.session_cumulative_token_totals().unwrap_or_default();
                    footer = ReplFooterStatus::from_config(
                        &config,
                        agent.effective_context_tokens()?,
                        cumulative_tokens,
                    );
                    let thinking_summary = client.thinking_variant_summary();
                    footer.update_thinking_variant(thinking_summary.as_deref());
                    let registry = build_tool_registry(
                        &config,
                        paths,
                        mode,
                        crate::question_tui::available(false),
                    )?;
                    agent.reload_config(config.clone(), client.clone())?;
                    agent.switch_mode(mode, registry);
                    footer.update_context_window(
                        agent.context_window(),
                        agent.context_window_assumed(),
                    );
                    println!("{}", t("configuration reloaded", "配置已重新加载"));
                }
                Ok(false) => {}
                Err(error) => println!("\x1b[31m{error:#}\x1b[0m"),
            }
            println!();
            continue;
        }
        if command.eq_ignore_ascii_case("/models") {
            let argument = command_args.trim();
            let repl_session_id = state.session_id();
            run_models_for_session(
                paths,
                parse_models_argument(argument),
                Some(&repl_session_id),
            )
            .await?;
            reload_repl_config(paths, &state, &mut config, &mut client)?;
            footer = ReplFooterStatus::from_config(
                &config,
                agent.effective_context_tokens()?,
                cumulative_tokens,
            );
            let thinking_summary = client.thinking_variant_summary();
            footer.update_thinking_variant(thinking_summary.as_deref());
            let registry =
                build_tool_registry(&config, paths, mode, crate::question_tui::available(false))?;
            agent.reload_config(config.clone(), client.clone())?;
            agent.switch_mode(mode, registry);
            footer.update_context_window(agent.context_window(), agent.context_window_assumed());
            if let Some(live) = live_repl.as_mut() {
                live.set_footer(footer.clone());
            }
            println!("{}", t("configuration reloaded", "配置已重新加载"));
            println!();
            continue;
        }
        if command.eq_ignore_ascii_case("/config") && command_args_empty {
            crate::config_tui::run(paths)?;
            reload_repl_config(paths, &state, &mut config, &mut client)?;
            footer = ReplFooterStatus::from_config(
                &config,
                agent.effective_context_tokens()?,
                cumulative_tokens,
            );
            let thinking_summary = client.thinking_variant_summary();
            footer.update_thinking_variant(thinking_summary.as_deref());
            let registry =
                build_tool_registry(&config, paths, mode, crate::question_tui::available(false))?;
            agent.reload_config(config.clone(), client.clone())?;
            agent.switch_mode(mode, registry);
            footer.update_context_window(agent.context_window(), agent.context_window_assumed());
            if let Some(live) = live_repl.as_mut() {
                live.set_footer(footer.clone());
            }
            println!("{}", t("configuration reloaded", "配置已重新加载"));
            println!();
            continue;
        }
        if command.eq_ignore_ascii_case("/variant") {
            if !crate::models_cache::is_loaded() {
                println!(
                    "{}\n",
                    t(
                        "model metadata is still loading; try /variant again shortly",
                        "模型元数据仍在加载，请稍后重试 /variant"
                    )
                );
                continue;
            }
            let selected = command_args.trim();
            match execute_variant(
                paths,
                &mut client,
                (!selected.is_empty()).then_some(selected),
                "/variant",
            )? {
                VariantOutcome::Updated => {
                    let thinking_summary = client.thinking_variant_summary();
                    footer.update_thinking_variant(thinking_summary.as_deref());
                    agent.replace_client(client.clone());
                    print_variant_updated();
                }
                VariantOutcome::Cancelled => {}
                VariantOutcome::Rejected(message) => {
                    eprintln!("\x1b[31m{message}\x1b[0m");
                }
            }
            continue;
        }
        if command.eq_ignore_ascii_case("/undo") && command_args_empty {
            let (removed, prompt) = state.undo_last_turn()?;
            footer.update_session_tokens(agent.effective_context_tokens()?);
            if removed > 0 && prompt.is_none() {
                println!("{}", t("context compaction undone", "已撤销上下文压缩"));
            } else {
                println!("{}: {removed}", t("undone messages", "已撤销消息数"));
            }
            if let Some(prompt) = prompt {
                if let Some(live) = live_repl.as_mut() {
                    live.editor.input = prompt;
                    live.editor.cursor = live.editor.input.chars().count();
                    live.editor.history_clean_index = None;
                } else {
                    prefill = Some(prompt);
                }
            }
            continue;
        }
        if command.eq_ignore_ascii_case("/pop") {
            let count = match parse_repl_pop_count(command_args) {
                Ok(count) => count,
                Err(err) => {
                    eprintln!("\x1b[31m{}: {err}\x1b[0m", t("error", "错误"));
                    continue;
                }
            };
            state.recover_stale_turns()?;
            match execute_pop(paths, &config, &state, count) {
                Ok(Some(outcome)) => {
                    print_pop_outcome(outcome);
                    footer.update_session_tokens(agent.effective_context_tokens()?);
                }
                Ok(None) => {}
                Err(err) => {
                    eprintln!("\x1b[31m{}: {err}\x1b[0m", t("error", "错误"));
                }
            }
            continue;
        }
        if command.eq_ignore_ascii_case("/compact") && command_args_empty {
            let reasoning_mode =
                render::ReasoningDisplayMode::from_config(&config.display.reasoning);
            let tool_call_mode =
                render::ToolCallDisplayMode::from_config(&config.display.tool_calls);
            let mut renderer = render::StreamRenderer::new(
                reasoning_mode,
                tool_call_mode,
                false,
                config.display.readable_tool_names,
                config.display.command_output_lines,
            );
            match agent
                .compact_now(|event| handle_agent_event(&mut renderer, event))
                .await
            {
                Ok(Some(result)) => {
                    renderer.finish()?;
                    if let Some(usage) = result.usage.as_ref() {
                        cumulative_tokens.add(TurnTokens::from_usage(Some(usage)));
                    }
                    footer.update_token_usage(
                        &result,
                        agent.effective_context_tokens()?,
                        agent.context_window(),
                        cumulative_tokens,
                    );
                    if config.display.show_token_usage {
                        print_chat_token_usage(
                            &result,
                            true,
                            agent.effective_context_tokens()?,
                            agent.context_window(),
                            cumulative_tokens,
                        )?;
                    }
                }
                Ok(None) => {
                    renderer.finish()?;
                    println!(
                        "\x1b[2m{}\x1b[0m",
                        t("nothing to compact", "没有可压缩的上下文")
                    );
                    footer.update_session_tokens(agent.effective_context_tokens()?);
                }
                Err(err) => {
                    renderer.finish()?;
                    eprintln!("\x1b[31m{}: {err}\x1b[0m", t("error", "错误"));
                }
            }
            continue;
        }
        if command.eq_ignore_ascii_case("/reset-memory") {
            // 不二次确认:只清长期记忆,会话历史/技能/知识库都不动。
            agent.wipe_memory()?;
            println!("{}", t("long-term memory erased", "长期记忆已清空"));
            continue;
        }
        if command.eq_ignore_ascii_case("/reset") && command_args.trim().is_empty() {
            run_reset(paths).await?;
            cumulative_tokens = TurnTokens::default();
            footer.reset_token_usage(agent.effective_context_tokens()?, agent.context_window());
            // 直连道同病同修(验收问题四):不重绘,Σ 旧数一直挂在屏上。
            if let Some(live) = live_repl.as_mut() {
                live.queued.clear();
                live.refresh_footer(footer.clone())?;
            }
            continue;
        }
        if command.eq_ignore_ascii_case("/wipe") {
            println!("{}", wipe_summary());
            if !confirm_stdin(t("wipe everything?", "确认全部抹掉？"))? {
                println!("{}", t("cancelled", "已取消"));
                continue;
            }
            run_wipe(paths, true).await?;
            agent.reset_memory()?;
            cumulative_tokens = TurnTokens::default();
            footer.reset_token_usage(agent.effective_context_tokens()?, agent.context_window());
            if let Some(live) = live_repl.as_mut() {
                live.queued.clear();
                live.refresh_footer(footer.clone())?;
            }
            continue;
        }
        // 命令泄漏守门(任务#14):直连道的 if 链只实现了命令表的子集,
        // 落到这里的表内命令(如 /new /session)以前会原文发给模型当聊天
        // ——人格实验冒烟时实锤过。现在一律拦下提示,绝不进对话;完整的
        // 双 dispatch 后端归一记为技术债,此守门先消灭整个 bug 类。
        //
        // 只拦**表里有**的:不在表里的 `/xxx` 不是命令,是普通消息
        // (`/home/shorin/x 这是什么`),照常发给模型。
        if is_repl_command(command) {
            println!(
                "{}",
                t(
                    "this command needs the full (daemon) REPL; start without MIYU_DIRECT to use it",
                    "该命令需要完整(daemon)REPL;不带 MIYU_DIRECT 启动即可使用"
                )
            );
            continue;
        }
        if input.is_empty() {
            continue;
        }
        push_history_capped(&mut input_history, input);
        persist_repl_history_entry(paths, &state.session_id(), input);
        if let Some(live) = live_repl.as_mut() {
            live.editor.record_history(input);
        }
        if agent.mode() != mode {
            let registry =
                build_tool_registry(&config, paths, mode, crate::question_tui::available(false))?;
            agent.switch_mode(mode, registry);
        }
        agent.prepare_for_turn()?;
        let reasoning_mode = render::ReasoningDisplayMode::from_config(&config.display.reasoning);
        let tool_call_mode = render::ToolCallDisplayMode::from_config(&config.display.tool_calls);
        let mut renderer = render::StreamRenderer::new(
            reasoning_mode,
            tool_call_mode,
            false,
            config.display.readable_tool_names,
            config.display.command_output_lines,
        );
        let control = AgentTurnControl::new(
            mode,
            build_tool_registry(
                &config,
                paths,
                AgentMode::Normal,
                crate::question_tui::available(false),
            )?,
            build_tool_registry(
                &config,
                paths,
                AgentMode::Dev,
                crate::question_tui::available(false),
            )?,
        );
        if live_repl.is_none() {
            live_repl = Some(LiveReplTail::new(
                mode,
                input_history.clone(),
                state.load_queued_prompts()?,
                footer.clone(),
            )?);
        }
        let live = live_repl.as_mut().expect("live REPL was initialized");
        let chat_result = run_live_agent_turn(
            live,
            paths,
            &state,
            &mut agent,
            LiveAgentInput {
                content: input,
                images: &pasted_images,
            },
            &control,
            &mut renderer,
        )
        .await;
        mode = live.mode();
        match chat_result {
            Ok(Some(result)) => {
                let context_window =
                    result_context_window(&config, &result).or(agent.context_window());
                let mut turn_tokens = TurnTokens::from_usage(result.usage.as_ref());
                if let Some(usage) = result.usage.as_ref() {
                    cumulative_tokens.add(TurnTokens::from_usage(Some(usage)));
                }
                let context_tokens = agent.effective_context_tokens()?;
                footer.update_token_usage(
                    &result,
                    context_tokens,
                    context_window,
                    cumulative_tokens,
                );
                let endpoint_variant = result.provider_id.as_deref().and_then(|provider_id| {
                    result
                        .model
                        .as_deref()
                        .and_then(|model| client.thinking_variant_for(provider_id, model))
                });
                if show_mixed_model_endpoint(&config, true) {
                    let provider = result.provider_id.as_deref().unwrap_or("-");
                    let model = result.model.as_deref().unwrap_or("-");
                    let frame = format!(
                        "\x1b[2m{}\x1b[0m\n",
                        mixed_model_endpoint_label(provider, model, endpoint_variant.as_deref())
                    );
                    live.apply_output_frame(frame.as_bytes())?;
                }
                match handle_live_post_turn_overflow(
                    live,
                    &agent,
                    &mut renderer,
                    context_tokens,
                    config.display.show_token_usage,
                    Some(&mut cumulative_tokens),
                )
                .await
                {
                    Ok(Some(compact_result)) => {
                        if let Some(usage) = compact_result.usage.as_ref() {
                            turn_tokens.add(TurnTokens::from_usage(Some(usage)));
                        }
                        footer.set_token_usage_with_cache(
                            turn_tokens,
                            agent.effective_context_tokens()?,
                            agent.context_window(),
                            cumulative_tokens,
                        );
                    }
                    Ok(None) => {
                        footer.update_session_tokens(agent.effective_context_tokens()?);
                    }
                    Err(err) => {
                        let frame = format!("\x1b[31m{}: {err}\x1b[0m\n", t("error", "错误"));
                        live.apply_output_frame(frame.as_bytes())?;
                        continue;
                    }
                }
                live.refresh_footer(footer.clone())?;
                show_shortcut_hint = false;
            }
            Ok(None) => {
                // An explicit cancel also withdraws the queued follow-ups;
                // reloading afterwards clears their bubbles.
                let _ = state.delete_queued_prompts();
                if let Some(live) = live_repl.as_mut() {
                    synchronized_terminal_update(CursorAfterUpdate::Shown, || {
                        live.reload_queue(&state)
                    })?;
                }
                // An interrupted turn is persisted and will be replayed into
                // the next request, so the context meter must reflect it.
                cumulative_tokens = state
                    .session_cumulative_token_totals()
                    .unwrap_or(cumulative_tokens);
                footer.update_session_tokens(agent.effective_context_tokens()?);
                footer.update_cumulative_tokens(cumulative_tokens);
            }
            Err(err) if crate::question::is_question_cancelled(&err) => {
                let _ = state.delete_queued_prompts();
                if let Some(live) = live_repl.as_mut() {
                    synchronized_terminal_update(CursorAfterUpdate::Shown, || {
                        live.reload_queue(&state)
                    })?;
                }
                cumulative_tokens = state
                    .session_cumulative_token_totals()
                    .unwrap_or(cumulative_tokens);
                footer.update_session_tokens(agent.effective_context_tokens()?);
                footer.update_cumulative_tokens(cumulative_tokens);
                continue;
            }
            Err(err) => {
                if let Some(live) = live_repl.as_mut() {
                    let frame = format!("\x1b[31m{}: {err}\x1b[0m\n", t("error", "错误"));
                    live.apply_output_frame(frame.as_bytes())?;
                    synchronized_terminal_update(CursorAfterUpdate::Shown, || {
                        live.reload_queue(&state)
                    })?;
                }
                continue;
            }
        }
    }
    state.discard_queued_prompts()?;
    // Background jobs are children of this REPL process; never leave them
    // running once the host is gone.
    tools::jobs::shutdown_all();
    Ok(())
}
