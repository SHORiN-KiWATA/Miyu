//! 回合主循环：模型请求 ↔ 工具调用的往返。
//!
//! `chat_with_tools` 是整个 agent 的心脏——发请求、收流、认出工具调用、执行、
//! 把结果拼回去、再发一轮，直到模型不再要工具。
//!
//! 这里同时要处理三种打断：用户排队了新消息（`consume_queued_prompts`）、回合
//! 被更新的同一回合超越、上下文溢出。三者都可能落在任意一次 await 上，所以状
//! 态推进都写成「先落库再改内存」，中途挂掉能从库里接着走。
//!
//! `execute_parallel_task_calls` 负责一批工具的并发执行：**输出必须按请求顺序
//! 映射回去**，不能按完成顺序——否则模型看到的结果和它发的调用对不上。

mod parallel;
mod redo;
mod repeat_gate;
mod stream;

use repeat_gate::{ToolRepeatGate, REPEAT_FUSE_THRESHOLD, REPEAT_SKIP_THRESHOLD};

use crate::agent::*;

impl Agent {
    pub(in crate::agent) async fn chat_with_tools<F>(
        &mut self,
        current_turn_id: &str,
        messages: &mut Vec<ChatMessage>,
        used_tools: &mut Vec<String>,
        persisted_tool_reports: &mut Vec<(String, String)>,
        replay_start: usize,
        base_tool_reports: &[String],
        initial_tool_rounds: usize,
        initial_question_rounds: usize,
        control: Option<&AgentTurnControl>,
        on_event: &mut F,
    ) -> Result<ChatResult>
    where
        F: FnMut(AgentEvent) -> Result<()>,
    {
        let mut tool_round = initial_tool_rounds;
        let mut question_rounds = initial_question_rounds;
        let mut replay_start = replay_start;
        // Passive overflow recovery is a one-shot barrier per turn: the
        // post-compaction retry must not recover another overflow (pi /
        // opencode / Claude Code all converge on exactly one attempt).
        let mut overflow_recovery_attempted = false;
        let mut loaded_tools = self.initial_loaded_tools(messages)?;
        // 已经给过契约提示的桩工具:同一工具反复失败不必每次都重发一遍 schema。
        let mut contract_hinted = std::collections::BTreeSet::<String>::new();
        self.pending_remote_tool_calls.lock().unwrap().clear();
        let mut usage_accumulator = UsageAccumulator::default();
        // v7 cache write-grace: provider prefix-cache writes are async, so a
        // follow-up fired within ~2s can miss the prefix the previous round
        // just computed (measured on DeepSeek). Track round completion time.
        let mut last_round_completed_at: Option<Instant> = None;
        let mut responses_continuation = None;
        let mut continuation_input_start = messages.len();
        let mut continuation_context: Option<(usize, Vec<ChatMessage>)> = None;
        let artifact_auto_publish = self.mode == AgentMode::Normal
            && self.prompt_audience == PromptAudience::External
            && artifact_delivery_requested(messages)
            && self
                .tools
                .lock()
                .unwrap()
                .tool_names()
                .iter()
                .any(|name| name == "create_artifact");
        let mut artifact_candidates = Vec::<AutoArtifactCandidate>::new();
        let mut artifact_published = false;
        let mut repeat_gate = ToolRepeatGate::new();
        let mut repeat_fused = false;
        loop {
            let tool_limit_reached =
                (self.max_tool_rounds > 0 && tool_round >= self.max_tool_rounds) || repeat_fused;

            if self.config.skills.enabled {
                if self.mode == AgentMode::Normal {
                    let mut registry = self.tools.lock().unwrap();
                    tools::rescan_scripts(&mut registry, &self.paths);
                    tools::register_script_display_names(&registry);
                }
                let current_fingerprint = {
                    let registry = self.tools.lock().unwrap();
                    registry
                        .contains("load_skill")
                        .then(|| registry.skill_catalog_fingerprint())
                };
                if let Some(current_fingerprint) = current_fingerprint {
                    let config = self.config.clone();
                    let paths = self.paths.clone();
                    let refresh = tokio::task::spawn_blocking(move || {
                        tools::prepare_skill_refresh(current_fingerprint, &config, &paths)
                            .map(|snapshot| (snapshot, config, paths))
                    })
                    .await;
                    match refresh {
                        Ok(Ok((Some(snapshot), config, paths))) => {
                            let mut registry = self.tools.lock().unwrap();
                            tools::apply_skill_refresh(&mut registry, &config, &paths, snapshot);
                        }
                        Ok(Ok((None, _, _))) => {}
                        Ok(Err(error)) => {
                            tracing::warn!(error = %error, "failed to refresh Miyu skill catalog")
                        }
                        Err(error) => {
                            tracing::warn!(error = %error, "Miyu skill catalog worker stopped")
                        }
                    }
                }
            }

            let definitions = if self.tools_enabled && !tool_limit_reached {
                let tools = self.tools.lock().unwrap();
                // 有效模式按候选模型池解析(模型级覆盖,任一成员要 full 则整池
                // full)——约束解码型模型吃不下空壳 stub(09-01)。
                if tools::is_stub_loading_mode(&tools::effective_tools_loading_mode(&self.config)) {
                    tools.stub_definitions()
                } else {
                    tools.definitions()
                }
            } else {
                Vec::new()
            };

            on_event(AgentEvent::ReasoningStart {
                received_at: Instant::now(),
            })?;
            let (chunk_tx, mut chunk_rx) =
                tokio::sync::mpsc::unbounded_channel::<(ChatStreamChunk, Instant)>();
            let mut request_messages = if responses_continuation.is_some() {
                messages
                    .get(continuation_input_start..)
                    .context("Responses continuation input cursor is out of bounds")?
                    .to_vec()
            } else {
                messages.clone()
            };
            if let Some((context_index, context_messages)) = continuation_context.as_ref() {
                let offset = context_index
                    .checked_sub(continuation_input_start)
                    .context("Responses continuation context cursor is out of bounds")?;
                if offset > request_messages.len() {
                    bail!("Responses continuation context cursor is out of bounds");
                }
                request_messages.splice(offset..offset, context_messages.clone());
            }
            let mut reasoning_filter = ReasoningTitleFilter::default();
            // 与 reasoning_filter 同生命周期:一轮模型调用 = 一条 assistant
            // 消息,批量提示要的正是"这条消息里的第几个工具调用"。
            let mut tool_calls_seen = 0usize;
            if self.config.cache.write_grace_ms > 0 {
                if let Some(previous) = last_round_completed_at {
                    let grace = std::time::Duration::from_millis(self.config.cache.write_grace_ms);
                    let elapsed = previous.elapsed();
                    if elapsed < grace {
                        tokio::time::sleep(grace - elapsed).await;
                    }
                }
            }
            if self.config.cache.keepalive_seconds > 0 && responses_continuation.is_none() {
                self.last_request_snapshot = Some((request_messages.clone(), definitions.clone()));
            }
            let round_streamed = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let round = {
                let streamed_flag = round_streamed.clone();
                let llm_future = self.client.chat_stream_with_continuation(
                    request_messages.clone(),
                    definitions,
                    responses_continuation.as_deref(),
                    move |chunk| {
                        streamed_flag.store(true, Ordering::Relaxed);
                        let _ = chunk_tx.send((chunk, Instant::now()));
                        Ok(())
                    },
                );
                tokio::pin!(llm_future);
                let mut spinner_interval = tokio::time::interval(self.spinner_interval);
                spinner_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                spinner_interval.tick().await;
                let supersede = control.and_then(|control| control.supersede.as_deref());
                let supersede_generation = control.and_then(|control| {
                    supersede.map(|_| control.supersede_seen.load(Ordering::Acquire))
                });
                loop {
                    tokio::select! {
                        biased;
                        _ = async {
                            match (supersede, supersede_generation) {
                                (Some(signal), Some(generation)) => signal.wait_after(generation).await,
                                _ => std::future::pending::<()>().await,
                            }
                        } => {
                            break None;
                        }
                        result = &mut llm_future => {
                            break Some(result);
                        }
                        Some((chunk, received_at)) = chunk_rx.recv() => {
                            record_remote_tool_chunk(
                                &chunk,
                                &self.pending_remote_tool_calls,
                            );
                            emit_model_chunk_at(
                                chunk,
                                received_at,
                                &mut reasoning_filter,
                                &mut tool_calls_seen,
                                on_event,
                            )?;
                        }
                        _ = spinner_interval.tick() => {
                            on_event(AgentEvent::SpinnerTick)?;
                        }
                    }
                }
            };
            let round = match round {
                Some(Err(error)) => {
                    // Responses 续传自愈(任务#16):上游不支持
                    // previous_response_id 时,工具轮第二步只发增量会撞
                    // "No tool call found for tool output" 类 400。此时清
                    // 续传重发全量(messages 里工具结果已齐,无状态回放
                    // 完整),并让客户端持久记该供应商不可续传——本会话
                    // 与后续会话都不再发增量。
                    if responses_continuation.is_some()
                        && crate::llm::is_responses_continuation_unsupported_error(&error)
                    {
                        tracing::warn!(
                            error = %error,
                            "responses continuation rejected; retrying this round with full stateless input"
                        );
                        self.client.mark_responses_continuation_unsupported();
                        responses_continuation = None;
                        continue;
                    }
                    // Passive overflow trigger (compact-and-retry). Only at
                    // the turn's initial request, before any assistant output
                    // was streamed: mid-loop the live tool exchange is not
                    // rebuildable from the DB, and a partially shown answer
                    // must not be silently retried (opencode's
                    // hasAssistantStarted guard).
                    let initial_request = tool_round == initial_tool_rounds
                        && question_rounds == initial_question_rounds
                        && responses_continuation.is_none()
                        && !round_streamed.load(Ordering::Relaxed);
                    let window = self.context_window();
                    if initial_request
                        && !overflow_recovery_attempted
                        && window.is_some()
                        && crate::llm::is_context_overflow_error(&error)
                    {
                        overflow_recovery_attempted = true;
                        let window = window.unwrap();
                        let check =
                            overflow::OverflowCheck::new(Some(window), self.trim_at_ratio, None);
                        on_event(AgentEvent::CompactStart)?;
                        let compactor = compact::Compactor::new(
                            self.client.clone(),
                            self.state.clone(),
                            window,
                            check.reserved_tokens,
                            self.compact_tail_budget(window),
                            self.preset_dialogs.len(),
                        );
                        let mut on_compact_chunk =
                            |chunk: ChatStreamChunk| on_event(AgentEvent::CompactChunk(chunk));
                        // No fork here: a fork of an overflowing conversation
                        // overflows identically — recovery must use the
                        // isolated serialized path.
                        let compacted = compactor
                            .perform_compact(true, true, None, &mut on_compact_chunk)
                            .await;
                        on_event(AgentEvent::CompactEnd)?;
                        if let Ok(Some(compact_result)) = compacted {
                            self.state.add_auxiliary_usage(
                                &compact_result.usage,
                                crate::state::UsageMeta {
                                    source: self.usage_source(),
                                    provider: compact_result.provider_id.as_deref(),
                                    model: None,
                                    kind: None,
                                },
                            )?;
                            // Splice the rebuilt (compacted) history prefix in
                            // front of the current turn's user message; the
                            // live tail (user input, runtime stamp, hints)
                            // is preserved byte-for-byte.
                            let user_index = live_user_index(messages, replay_start)
                                .unwrap_or_else(|| replay_start.min(messages.len()));
                            let (rebuilt, rebuilt_user_index) =
                                self.chat_messages(current_turn_id, "")?;
                            let tail = messages.split_off(user_index);
                            messages.clear();
                            messages.extend(rebuilt.into_iter().take(rebuilt_user_index));
                            messages.extend(tail);
                            // 活跃轮边界随尾巴整体平移:新前缀长 + 尾内偏移。
                            replay_start = rebuilt_user_index + (replay_start - user_index);
                            continuation_input_start = messages.len();
                            tracing::info!(
                                folded = compact_result.folded_turns,
                                kept = compact_result.kept_turns,
                                "context overflow recovered by compact-and-retry"
                            );
                            continue;
                        }
                        if let Err(compact_error) = compacted {
                            tracing::warn!(
                                error = %compact_error,
                                "compact-and-retry failed; surfacing the original overflow"
                            );
                        }
                    }
                    return Err(error);
                }
                Some(Ok(result)) => Some(result),
                None => None,
            };
            let Some(result) = round else {
                if let Some(control) = control {
                    if let Some(generation) = control.pending_supersede_generation() {
                        control.mark_supersede_seen(generation);
                    }
                }
                let queued = self.state.load_queued_prompts()?;
                if queued.is_empty() {
                    continue;
                }
                let prompt_ids = queued
                    .iter()
                    .map(|prompt| prompt.prompt_id.clone())
                    .collect::<Vec<_>>();
                on_event(AgentEvent::GenerationSuperseded { prompt_ids })?;
                let checkpoint = redo_checkpoint_payload(
                    messages,
                    replay_start,
                    base_tool_reports,
                    persisted_tool_reports,
                    tool_round,
                    question_rounds,
                );
                let continuation_context_index = responses_continuation.as_ref().map(|_| {
                    continuation_context
                        .as_ref()
                        .map(|(index, _)| *index)
                        .unwrap_or(messages.len())
                });
                self.consume_queued_prompts(
                    current_turn_id,
                    messages,
                    queued,
                    (None, None, None, None),
                    checkpoint,
                    control.expect("supersede requires turn control"),
                    on_event,
                )
                .await?;
                if let Some(index) = continuation_context_index {
                    continuation_context = Some((
                        index,
                        vec![
                            ChatMessage::turn_context(continuation_system_prompt(
                                &self.system_prompt,
                                self.mode,
                            )),
                            ChatMessage::turn_context(runtime_context(
                                self.mode,
                                self.platform_context.is_some(),
                            )),
                        ],
                    ));
                }
                continue;
            };
            while let Ok((chunk, received_at)) = chunk_rx.try_recv() {
                record_remote_tool_chunk(&chunk, &self.pending_remote_tool_calls);
                emit_model_chunk_at(
                    chunk,
                    received_at,
                    &mut reasoning_filter,
                    &mut tool_calls_seen,
                    on_event,
                )?;
            }
            let (title, text) = reasoning_filter.finish();
            if let Some(title) = title {
                on_event(AgentEvent::ReasoningTitle(title))?;
            }
            if let Some(text) = text {
                on_event(AgentEvent::Chunk(ChatStreamChunk {
                    kind: ChatStreamKind::Reasoning,
                    text,
                }))?;
            }
            usage_accumulator.add_result(&result, messages);
            if let Some(turn_usage) = usage_accumulator.usage() {
                // 上下文表读数优先取供应商标注的"最后一次请求"口径。
                let round = result
                    .last_request_usage
                    .clone()
                    .or_else(|| result.usage.clone())
                    .unwrap_or_else(|| {
                        let prompt = overflow::estimate_messages_tokens(&request_messages) as u64;
                        let completion = estimate_result_tokens(&result) as u64;
                        Usage {
                            prompt_tokens: prompt,
                            completion_tokens: completion,
                            total_tokens: prompt.saturating_add(completion),
                            ..Usage::default()
                        }
                    });
                on_event(AgentEvent::RoundUsage {
                    round: Box::new(round),
                    turn: TurnTokens::from_usage(Some(&turn_usage)),
                    estimated: usage_accumulator.estimated,
                    provider_id: result.provider_id.clone(),
                    model: result.model.clone(),
                })?;
            }
            last_round_completed_at = Some(Instant::now());
            if result.tool_calls.is_empty() || !self.tools_enabled {
                responses_continuation = None;
                continuation_input_start = messages.len();
                continuation_context = None;
                if let Some(control) = control {
                    let queued = self.state.load_queued_prompts()?;
                    if !queued.is_empty() {
                        if let Some(generation) = control.pending_supersede_generation() {
                            let prompt_ids = queued
                                .iter()
                                .map(|prompt| prompt.prompt_id.clone())
                                .collect();
                            on_event(AgentEvent::GenerationSuperseded { prompt_ids })?;
                            let checkpoint = redo_checkpoint_payload(
                                messages,
                                replay_start,
                                base_tool_reports,
                                persisted_tool_reports,
                                tool_round,
                                question_rounds,
                            );
                            self.consume_queued_prompts(
                                current_turn_id,
                                messages,
                                queued,
                                (None, None, None, None),
                                checkpoint,
                                control,
                                on_event,
                            )
                            .await?;
                            control.mark_supersede_seen(generation);
                            continue;
                        }
                        push_assistant_context_messages(
                            messages,
                            &result.content,
                            result.reasoning.as_deref(),
                            true,
                        );
                        let checkpoint = redo_checkpoint_payload(
                            messages,
                            replay_start,
                            base_tool_reports,
                            persisted_tool_reports,
                            tool_round,
                            question_rounds,
                        );
                        self.consume_queued_prompts(
                            current_turn_id,
                            messages,
                            queued,
                            (
                                Some(&result.content),
                                result.reasoning.as_deref(),
                                result.provider_id.as_deref(),
                                result.model.as_deref(),
                            ),
                            checkpoint,
                            control,
                            on_event,
                        )
                        .await?;
                        continue;
                    }
                }
                let mut result = result;
                if artifact_auto_publish && !artifact_published {
                    publish_auto_artifact_candidates(&artifact_candidates, on_event)?;
                }
                if let Some(usage) = usage_accumulator.usage() {
                    // 供应商已给出"最后一次请求"的口径(claude-code 中转:
                    // 结果帧是整轮累计,真实上下文在流内最后一次调用里)时
                    // 尊重之,不再用轮用量覆盖。
                    let round_usage = result.usage.take();
                    if result.last_request_usage.is_none() {
                        result.last_request_usage = round_usage;
                    }
                    result.usage = Some(usage);
                    result.usage_estimated = usage_accumulator.estimated;
                }
                return Ok(result);
            }
            if tool_limit_reached {
                let mut result = result;
                // 复读保险丝收束:不产任何警告文本(08-24 用户裁定),模型在
                // 无工具轮已有机会正常成文,这里只收尾。真 max_rounds 上限
                // 保留原提示,但只给所有者受众;平台正文=群消息,拼进去就是
                // 把系统文本发到群里(08-24 实录)。
                if repeat_fused {
                    tracing::warn!("tool repeat fuse: loop closed without a text answer");
                } else if self.prompt_audience == PromptAudience::External {
                    tracing::warn!(
                        "tool calls reached the round limit of {}",
                        self.max_tool_rounds
                    );
                } else {
                    let warning = format!(
                        "Tool calls reached the limit of {} rounds; the remaining tool calls were not executed. Set `tools.max_rounds` to 0 to allow unlimited tool rounds.",
                        self.max_tool_rounds
                    );
                    let warning_chunk = if result.content.trim().is_empty() {
                        warning.clone()
                    } else {
                        format!("\n\n{warning}")
                    };
                    result.content.push_str(&warning_chunk);
                    on_event(AgentEvent::Chunk(ChatStreamChunk {
                        kind: ChatStreamKind::Content,
                        text: warning_chunk,
                    }))?;
                }
                result.tool_calls.clear();
                if let Some(usage) = usage_accumulator.usage() {
                    let round_usage = result.usage.take();
                    if result.last_request_usage.is_none() {
                        result.last_request_usage = round_usage;
                    }
                    result.usage = Some(usage);
                    result.usage_estimated = usage_accumulator.estimated;
                }
                return Ok(result);
            }
            // 同参复读闸(见 repeat_gate.rs):连续相同轮先跳过执行回灌错误,
            // 到保险丝阈值置 repeat_fused——下一轮请求不再带工具,逼模型用
            // 已有结果正常成文(硬截断+英文警告拼正文会把机器文本漏到 QQ,
            // 08-24 线上翻车实录)。
            let round_repeats = repeat_gate.observe(&result.tool_calls);
            if round_repeats >= REPEAT_FUSE_THRESHOLD && !repeat_fused {
                repeat_fused = true;
                tracing::warn!(
                    repeats = round_repeats,
                    "tool repeat fuse blown; withholding tools so the model answers with existing results"
                );
            }
            let repeat_skip = round_repeats >= REPEAT_SKIP_THRESHOLD;
            tool_round += 1;
            let next_responses_continuation = result.responses_continuation.clone();
            push_assistant_message_with_reasoning(
                messages,
                result.content.clone(),
                result.reasoning.as_deref(),
                result.thinking_signature.as_deref(),
                // 参数的合法性由 ChatMessage::assistant 统一收口(见那里的
                // 注释);执行侧仍拿原始参数,好让工具把 `EOF while parsing`
                // 这类解析错误如实回给模型。
                Some(result.tool_calls.clone()),
                true,
            );
            if result
                .finish_reason
                .as_deref()
                .is_some_and(|reason| reason.eq_ignore_ascii_case("length"))
                && !result.tool_calls.is_empty()
            {
                // 续传簿记与正常路径同步:跳过它会让下一轮带着上一轮的旧
                // response id 续传,服务端 400 后再走自愈,白费一次请求。
                // start 必须在 push tool 错误之前设定(续传输入=工具输出段)。
                if next_responses_continuation.is_some() {
                    continuation_input_start = messages.len();
                }
                responses_continuation = next_responses_continuation;
                continuation_context = None;
                // A "length" stop means the output hit the token limit, so every
                // tool call in this message may carry silently truncated
                // arguments. Refuse to execute any of them and let the model
                // re-issue the calls with complete arguments.
                for call in &result.tool_calls {
                    messages.push(ChatMessage::tool(
                        call.id.clone(),
                        "error: this reply was truncated by the output token limit, so the tool call arguments may be incomplete. Re-issue this tool call with complete arguments.",
                    ));
                }
                continue;
            }
            if next_responses_continuation.is_some() {
                continuation_input_start = messages.len();
            }
            responses_continuation = next_responses_continuation;
            continuation_context = None;
            let ask_question_enabled = self
                .tools
                .lock()
                .unwrap()
                .tool_names()
                .iter()
                .any(|name| name == "ask_question");
            let question_call_count = result
                .tool_calls
                .iter()
                .filter(|call| ask_question_enabled && call.function.name == "ask_question")
                .count();
            if question_call_count == 1 {
                question_rounds += 1;
            }
            let question_round_allowed =
                question_call_count == 1 && question_rounds <= MAX_QUESTION_ROUNDS_PER_TURN;
            let defer_sibling_tools = question_call_count == 1 && result.tool_calls.len() > 1;
            // Multiple `task` calls in one batch run concurrently (subagents
            // are independent by design); everything else stays serial.
            let mut parallel_task_outputs = if defer_sibling_tools || repeat_skip {
                std::collections::HashMap::new()
            } else {
                self.execute_parallel_task_calls(&result.tool_calls, on_event)
                    .await?
            };
            for (call_index, call) in result.tool_calls.into_iter().enumerate() {
                if let Some(group_output) = parallel_task_outputs.remove(&call_index) {
                    // Executed in the parallel group; events already emitted.
                    used_tools.push(call.function.name.clone());
                    if let Some(report) = group_output.report {
                        persisted_tool_reports.push((call.function.name.clone(), report));
                    }
                    let model_output = self
                        .spill_tool_output(
                            current_turn_id,
                            &call.id,
                            &call.function.name,
                            &group_output.output,
                        )
                        .unwrap_or(group_output.output);
                    messages.push(ChatMessage::tool(call.id, model_output));
                    continue;
                }
                let call_id = call.id.clone();
                let event_name = tool_event_name(&call.function.name, &call.function.arguments);
                on_event(AgentEvent::ToolCall {
                    call_id: call_id.clone(),
                    name: event_name.clone(),
                    arguments: call.function.arguments.clone(),
                })?;
                if repeat_skip {
                    // 同参复读:不再真执行,回灌上一轮的真实结果字节。不注入
                    // 指令文本——故障态模型看不见输入增量,提示无用(08-24)。
                    let output =
                        repeat_gate.cached_output(&call.function.name, &call.function.arguments);
                    on_event(AgentEvent::ToolResult {
                        call_id: call_id.clone(),
                        name: event_name.clone(),
                        ok: tool_output_succeeded(&output),
                        output: output.clone(),
                    })?;
                    messages.push(ChatMessage::tool(call.id, output));
                    continue;
                }
                if question_call_count > 1 {
                    let output = "tool error: only one ask_question call is allowed per tool batch; combine all questions into one call".to_string();
                    on_event(AgentEvent::ToolResult {
                        call_id: call_id.clone(),
                        name: event_name.clone(),
                        ok: false,
                        output: output.clone(),
                    })?;
                    messages.push(ChatMessage::tool(call.id, output));
                    continue;
                }
                if defer_sibling_tools && call.function.name != "ask_question" {
                    let output = "tool error: deferred until the user answers ask_question; reissue this tool call after receiving the answer".to_string();
                    on_event(AgentEvent::ToolResult {
                        call_id: call_id.clone(),
                        name: event_name.clone(),
                        ok: false,
                        output: output.clone(),
                    })?;
                    messages.push(ChatMessage::tool(call.id, output));
                    continue;
                }
                if ask_question_enabled && call.function.name == "ask_question" {
                    if !question_round_allowed {
                        let output = format!(
                            "tool error: ask_question exceeded the per-turn limit of {MAX_QUESTION_ROUNDS_PER_TURN}"
                        );
                        on_event(AgentEvent::ToolResult {
                            call_id: call_id.clone(),
                            name: event_name.clone(),
                            ok: false,
                            output: output.clone(),
                        })?;
                        messages.push(ChatMessage::tool(call.id, output));
                        continue;
                    }
                    let request = match QuestionRequest::parse(&call.function.arguments) {
                        Ok(request) => request,
                        Err(err) => {
                            // 报错要说自己真正知道的:实测模型看到裸的 serde 消息
                            // （"invalid type: string, expected a sequence"）之后
                            // 反复重试同样的形状,最后判定成「接口不支持」放弃。
                            // 补一句期望形状,它才知道该改什么。
                            let output = format!(
                                "tool error: invalid ask_question request: {err}\n\
                                 expected {{\"questions\": [{{\"header\": ..., \"question\": ..., \
                                 \"options\": [{{\"label\": ..., \"description\": ...}}]}}]}} \
                                 — questions and options must be real JSON arrays, not strings"
                            );
                            on_event(AgentEvent::ToolResult {
                                call_id: call_id.clone(),
                                name: event_name.clone(),
                                ok: false,
                                output: output.clone(),
                            })?;
                            messages.push(ChatMessage::tool(call.id, output));
                            continue;
                        }
                    };
                    let (response_tx, response_rx) = oneshot::channel();
                    on_event(AgentEvent::AskQuestion {
                        call_id: call_id.clone(),
                        request: request.clone(),
                        responder: response_tx,
                    })?;
                    let response = response_rx.await.unwrap_or(QuestionResponse::Cancelled);
                    let output = match response {
                        QuestionResponse::Answered(answers) => {
                            let exchange = QuestionExchange::new(request, answers)?;
                            self.state
                                .append_question_exchange(current_turn_id, &exchange)?;
                            answered_tool_output(&exchange)
                        }
                        QuestionResponse::Closed => closed_tool_output(),
                        QuestionResponse::Cancelled => return Err(QuestionCancelled.into()),
                        QuestionResponse::Unavailable(reason) => unavailable_tool_output(&reason),
                    };
                    messages.push(ChatMessage::tool(call.id, output.clone()));
                    on_event(AgentEvent::ToolResult {
                        call_id: call_id.clone(),
                        name: event_name,
                        ok: true,
                        output,
                    })?;
                    continue;
                }
                used_tools.push(call.function.name.clone());
                // 模式级 ReadOnly 权限门随闲聊模式一并删除:拒绝层现在是
                // registry 的单调 guard(软失败),不可用工具靠 registry 组合
                // 不注册(平台 restricted 同理),未知工具在分发处软失败。
                let (progress_tx, mut progress_rx) = mpsc::unbounded_channel();
                let tool_future = {
                    let tools = self.tools.lock().unwrap();
                    // AUR 互斥等回合级规则已迁入 guard 层,凭 used_tools 上下文判定。
                    tools.call_with_progress_future(
                        &call.function.name,
                        &call.function.arguments,
                        progress_tx,
                        &crate::tools::GuardCtx {
                            used_tools: &used_tools,
                        },
                    )
                };
                // 桩工具失败时把真契约补进返回体(每个工具每回合只补一次)。
                let mut attach_contract = |message: String| -> String {
                    if !tools::is_stub_loading_mode(&self.config.tools.loading_mode) {
                        return message;
                    }
                    if !contract_hinted.insert(call.function.name.clone()) {
                        return message;
                    }
                    let tools = self.tools.lock().unwrap();
                    if !tools.is_stub_presented(&call.function.name) {
                        return message;
                    }
                    match tools.contract_text(&call.function.name) {
                        Some(contract) => format!(
                            "{message}\n\nThis tool was declared with an empty parameter shell, so its real schema follows. Call it again with these arguments at the top level.{contract}"
                        ),
                        None => message,
                    }
                };
                let tool_future = match tool_future {
                    Ok(f) => f,
                    Err(err) => {
                        let output = attach_contract(format!("tool error: {err}"));
                        on_event(AgentEvent::ToolResult {
                            call_id: call_id.clone(),
                            name: event_name.clone(),
                            ok: false,
                            output: output.clone(),
                        })?;
                        messages.push(ChatMessage::tool(call.id, output));
                        continue;
                    }
                };
                tokio::pin!(tool_future);
                let mut spinner_interval = tokio::time::interval(self.spinner_interval);
                spinner_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                spinner_interval.tick().await;
                let (output, tool_succeeded) = loop {
                    tokio::select! {
                        result = &mut tool_future => {
                            break match result {
                                Ok(output) => {
                                    while let Ok(progress) = progress_rx.try_recv() {
                                        emit_tool_progress(on_event, &call_id, &event_name, progress)?;
                                    }
                                    (output, true)
                                }
                                Err(err) => {
                                    while let Ok(progress) = progress_rx.try_recv() {
                                        emit_tool_progress(on_event, &call_id, &event_name, progress)?;
                                    }
                                    let output = attach_contract(format!("tool error: {err}"));
                                    on_event(AgentEvent::ToolResult {
                                        call_id: call_id.clone(),
                                        name: event_name.clone(),
                                        ok: false,
                                        output: output.clone(),
                                    })?;
                                    (output, false)
                                }
                            };
                        }
                        Some(progress) = progress_rx.recv() => {
                            emit_tool_progress(on_event, &call_id, &event_name, progress)?;
                        }
                        _ = spinner_interval.tick() => {
                            on_event(AgentEvent::SpinnerTick)?;
                        }
                    }
                };
                let clipboard_image = if tool_succeeded {
                    clipboard_binary_image_from_tool_result(&call.function.name, &output)
                } else {
                    None
                };
                let model_output = self
                    .spill_tool_output(current_turn_id, &call.id, &call.function.name, &output)
                    .unwrap_or_else(|| output.clone());
                // 复读闸记账:下一轮同参跳过时按键回灌这份字节。(dsh 式
                // advisory 重复提醒于 08-24 整体退役:222 连发与 08-23/24
                // 两次故障实录证明提示文本对故障态模型无效,防线全部交给
                // 结构化的 repeat_gate。)
                repeat_gate.record_output(
                    &call.function.name,
                    &call.function.arguments,
                    &model_output,
                );
                messages.push(ChatMessage::tool(call.id.clone(), model_output));
                if tool_succeeded && call.function.name == "load_tools" {
                    let loaded = loaded_items_from_output(&output);
                    for name in &loaded.tools {
                        loaded_tools.insert(name.clone());
                    }
                    if self.config.tools.persist_loaded_tools {
                        self.state
                            .add_session_loaded_tools(&loaded.tools, Some(current_turn_id))?;
                        self.state
                            .add_session_loaded_targets(&loaded.targets, Some(current_turn_id))?;
                    }
                }
                if let Some(img) = clipboard_image {
                    let supports_vision = self.current_model_supports_vision();
                    let uses_vision_fallback =
                        !supports_vision && self.config.plugins.vision.enabled;
                    if !supports_vision {
                        let message = if self.config.plugins.vision.enabled {
                            if crate::i18n::is_zh() {
                                "视觉分析."
                            } else {
                                "Vision analysis."
                            }
                        } else if crate::i18n::is_zh() {
                            "当前模型不支持图片，且未启用视觉模型，无法分析剪贴板图片。"
                        } else {
                            "The current model does not support images and the vision plugin is disabled, so the clipboard image cannot be analyzed."
                        };
                        on_event(AgentEvent::ToolProgress {
                            call_id: call_id.clone(),
                            name: event_name.clone(),
                            message: message.to_string(),
                        })?;
                    }
                    let image_message = if uses_vision_fallback {
                        let image_future = self.clipboard_image_message(img);
                        tokio::pin!(image_future);
                        let mut spinner_interval = tokio::time::interval(self.spinner_interval);
                        spinner_interval
                            .set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                        spinner_interval.tick().await;
                        let mut progress_interval =
                            tokio::time::interval(Duration::from_millis(900));
                        progress_interval
                            .set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                        progress_interval.tick().await;
                        let mut progress_tick = 0usize;
                        loop {
                            tokio::select! {
                                result = &mut image_future => {
                                    break result?;
                                }
                                _ = progress_interval.tick() => {
                                    progress_tick = progress_tick.wrapping_add(1);
                                    on_event(AgentEvent::ToolProgress {
                                        call_id: call_id.clone(),
                                        name: event_name.clone(),
                                        message: vision_analysis_progress(progress_tick),
                                    })?;
                                }
                                _ = spinner_interval.tick() => {
                                    on_event(AgentEvent::SpinnerTick)?;
                                }
                            }
                        }
                    } else {
                        self.clipboard_image_message(img).await?
                    };
                    if let Some(message) = image_message {
                        messages.push(message);
                    }
                }
                if tool_succeeded {
                    let result_ok = tool_output_succeeded(&output);
                    if result_ok {
                        if let Some(delta) =
                            tool_call_footprint(&call.function.name, &call.function.arguments)
                        {
                            self.state.merge_turn_footprint(current_turn_id, &delta)?;
                        }
                        if matches!(
                            call.function.name.as_str(),
                            "create_artifact" | "apply_artifact_patch" | "present_artifact"
                        ) {
                            artifact_published = true;
                        } else if artifact_auto_publish {
                            for path in artifact_candidate_paths(&call.function.name, &output) {
                                artifact_candidates.push(AutoArtifactCandidate {
                                    call_id: call_id.clone(),
                                    tool_name: event_name.clone(),
                                    path,
                                });
                            }
                        }
                    }
                    on_event(AgentEvent::ToolResult {
                        call_id,
                        name: event_name.clone(),
                        ok: result_ok,
                        output: output.clone(),
                    })?;
                    if let Some(report) =
                        extract_persistable_tool_report(&call.function.name, &output)
                    {
                        persisted_tool_reports.push((call.function.name.clone(), report));
                    }
                }
            }
            // 本轮工具结果已经全部进了 `messages`,趁这里把 tool_flow 落一次盘。
            //
            // 崩溃恢复时正文和工具报告都能从流水物化出来(`interrupted_projection`
            // 与追加型的 `turn_tool_reports`),唯独 tool_flow 不能——它以前只在整
            // 个回合跑完后写一次(`stream.rs` 的 `set_turn_tool_flow`),进程中途死
            // 掉这份就从没存在过。而 tool_flow 正是 `history.rs` 回放给模型的那份
            // 「调过哪些工具、拿到什么结果」,丢了它模型下一轮只看到半截文字,会把
            // 已经跑过的命令、读过的文件原样再来一遍。
            self.checkpoint_tool_flow(current_turn_id, messages, replay_start);
            // goal 侧挂起的步间指令在这里取走注入:自主轮报了完成/受阻之后的
            // 收尾指令(不注入的话,工具返回了 JSON,模型没有理由再说什么,
            // 一个跑了十几轮的目标就无声停住);以及人在续轮中途 `/goal edit`
            // 之后的目标变更通知(不注入的话,模型整轮都在推进旧目标)。
            if let Some(session) = crate::tools::workspace::try_session() {
                if let Some(wrapup) = crate::tools::goal::take_turn_notices(&session) {
                    // `turn_context` 而不是 `system`：中途插一条 system 会把
                    // 提供方模板里的 system 前置块整体挪位，前缀缓存全废
                    // （`ChatMessage::turn_context` 的注释里有实测数据）。
                    messages.push(ChatMessage::turn_context(wrapup));
                }
            }
            if question_round_allowed {
                tool_round = tool_round.saturating_sub(1);
            }
            if let Some(control) = control {
                if let Some(queue_ingress) = control.queue_ingress.as_ref() {
                    queue_ingress.wait_for_reserved_ingress().await;
                }
                let queued = self.state.load_queued_prompts()?;
                if !queued.is_empty() {
                    let supersede_generation = control.pending_supersede_generation();
                    if supersede_generation.is_some() {
                        let prompt_ids = queued
                            .iter()
                            .map(|prompt| prompt.prompt_id.clone())
                            .collect();
                        on_event(AgentEvent::GenerationSuperseded { prompt_ids })?;
                    }
                    let checkpoint = redo_checkpoint_payload(
                        messages,
                        replay_start,
                        base_tool_reports,
                        persisted_tool_reports,
                        tool_round,
                        question_rounds,
                    );
                    let preceding_assistant = if supersede_generation.is_some() {
                        (None, None, None, None)
                    } else {
                        (
                            Some(result.content.as_str()),
                            result.reasoning.as_deref(),
                            result.provider_id.as_deref(),
                            result.model.as_deref(),
                        )
                    };
                    let continuation_context_index = responses_continuation.as_ref().map(|_| {
                        continuation_context
                            .as_ref()
                            .map(|(index, _)| *index)
                            .unwrap_or(messages.len())
                    });
                    self.consume_queued_prompts(
                        current_turn_id,
                        messages,
                        queued,
                        preceding_assistant,
                        checkpoint,
                        control,
                        on_event,
                    )
                    .await?;
                    if let Some(index) = continuation_context_index {
                        continuation_context = Some((
                            index,
                            vec![
                                ChatMessage::turn_context(continuation_system_prompt(
                                    &self.system_prompt,
                                    self.mode,
                                )),
                                ChatMessage::turn_context(runtime_context(
                                    self.mode,
                                    self.platform_context.is_some(),
                                )),
                            ],
                        ));
                    }
                    if let Some(generation) = supersede_generation {
                        control.mark_supersede_seen(generation);
                    }
                }
            }
        }
    }

    /// 把「到目前为止调过哪些工具、拿到什么结果」落一次盘。
    ///
    /// 与回合结束时那次写入(`stream.rs`)同一套派生与裁剪,`set_turn_tool_flow`
    /// 是 UPDATE,后写覆盖先写,重复调用幂等。
    ///
    /// **失败只告警不中断回合**:这是一次耐久性检查点,不是回合的产出。为了它
    /// 把一个正在跑的回合掐掉,比丢掉这份检查点糟得多——回合结束时那次写入仍然
    /// 是 `?`,真有持久化问题跑不掉。
    fn checkpoint_tool_flow(&self, turn_id: &str, messages: &[ChatMessage], replay_start: usize) {
        let mut tool_flow = derive_tool_flow(messages, replay_start);
        prune_tool_flow(&mut tool_flow, &self.config.context);
        self.append_remote_tool_flow(&mut tool_flow);
        if tool_flow.is_empty() {
            return;
        }
        if let Err(error) = self.state.set_turn_tool_flow(turn_id, &tool_flow) {
            tracing::warn!(
                turn_id,
                error = %error,
                "tool flow checkpoint failed; a crash here would lose what the turn already did"
            );
        }
    }
}

impl Agent {
    /// 把收集到的中转侧工具活动折成一条 remote 轮,附到 tool_flow 尾部。
    /// 检查点与最终写入共用;drain 语义幂等(检查点后新活动继续累积)。
    fn append_remote_tool_flow(&self, tool_flow: &mut Vec<crate::state::ToolFlowRound>) {
        let calls = self.pending_remote_tool_calls.lock().unwrap().clone();
        if calls.is_empty() {
            return;
        }
        tool_flow.push(crate::state::ToolFlowRound {
            remote: true,
            assistant_content: String::new(),
            assistant_reasoning: None,
            calls,
        });
    }
}

/// 中转侧工具活动的收集:RemoteToolStarted/Finished 的 JSON 载荷折成
/// ToolFlowCall,失败结果加 "tool error: " 前缀让 SafeToolCall 的 ok 判定
/// 复用既有规则。
fn record_remote_tool_chunk(
    chunk: &ChatStreamChunk,
    pending: &std::sync::Mutex<Vec<crate::state::ToolFlowCall>>,
) {
    let parse = |text: &str| serde_json::from_str::<serde_json::Value>(text).ok();
    match chunk.kind {
        ChatStreamKind::RemoteToolStarted => {
            let Some(value) = parse(&chunk.text) else {
                return;
            };
            let field = |key: &str| {
                value
                    .get(key)
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default()
                    .to_string()
            };
            pending.lock().unwrap().push(crate::state::ToolFlowCall {
                id: field("id"),
                name: field("name"),
                arguments: value
                    .get("input")
                    .map(|input| input.to_string())
                    .unwrap_or_default(),
                output: String::new(),
            });
        }
        ChatStreamKind::RemoteToolFinished => {
            let Some(value) = parse(&chunk.text) else {
                return;
            };
            let id = value
                .get("id")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default();
            let ok = value
                .get("ok")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(true);
            let output = value
                .get("output")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default();
            let mut pending = pending.lock().unwrap();
            if let Some(call) = pending.iter_mut().rev().find(|call| call.id == id) {
                call.output = if ok {
                    output.to_string()
                } else {
                    format!("tool error: {output}")
                };
            }
        }
        _ => {}
    }
}
