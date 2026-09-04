//! 一批工具调用的并发执行与排队消息的消费。
//!
//! 输出必须**按请求顺序**映射回去，不能按完成顺序——模型看到的结果和它发出的
//! 调用对不上，后面全乱。
//!
//! 排队消息在工具轮次之间消费：这个时机是刻意的，回合中途插入用户消息只能落在
//! 一个完整的工具轮次边界上，否则会把工具调用和结果拆散。

use crate::agent::*;

impl Agent {
    /// Runs a batch's `task` tool calls concurrently, in waves bounded by
    /// `tools.subagent_concurrency`. Subagents are independent by design, so
    /// fanning them out preserves semantics while collapsing wall-clock time.
    /// Batches with fewer than two task calls return an empty map and take the
    /// serial path.
    pub(in crate::agent) async fn execute_parallel_task_calls<F>(
        &self,
        calls: &[crate::llm::ToolCall],
        on_event: &mut F,
    ) -> Result<std::collections::HashMap<usize, GroupTaskOutput>>
    where
        F: FnMut(AgentEvent) -> Result<()>,
    {
        let mut outputs = std::collections::HashMap::new();
        let eligible: Vec<usize> = calls
            .iter()
            .enumerate()
            .filter(|(_, call)| call.function.name == "task")
            .map(|(index, _)| index)
            .collect();
        if eligible.len() < 2 {
            return Ok(outputs);
        }

        struct Slot {
            call_index: usize,
            call_id: String,
            event_name: String,
            future: Option<tools::ToolFuture>,
            progress: mpsc::UnboundedReceiver<tools::ToolProgressEvent>,
        }
        enum WaveEvent {
            Done(usize, Result<String>),
            Progress(usize, tools::ToolProgressEvent),
            Spinner,
        }

        let limit = self.config.tools.subagent_concurrency.max(1);
        for wave in eligible.chunks(limit) {
            let mut slots: Vec<Slot> = Vec::new();
            {
                let tools = self.tools.lock().unwrap();
                for &call_index in wave {
                    let call = &calls[call_index];
                    let event_name = tool_event_name(&call.function.name, &call.function.arguments);
                    on_event(AgentEvent::ToolCall {
                        call_id: call.id.clone(),
                        name: event_name.clone(),
                        arguments: call.function.arguments.clone(),
                    })?;
                    let (progress_tx, progress_rx) = mpsc::unbounded_channel();
                    match tools.call_with_progress_future(
                        &call.function.name,
                        &call.function.arguments,
                        progress_tx,
                        &crate::tools::GuardCtx::default(),
                    ) {
                        Ok(future) => slots.push(Slot {
                            call_index,
                            call_id: call.id.clone(),
                            event_name,
                            future: Some(future),
                            progress: progress_rx,
                        }),
                        Err(err) => {
                            let output = format!("tool error: {err}");
                            on_event(AgentEvent::ToolResult {
                                call_id: call.id.clone(),
                                name: event_name,
                                ok: false,
                                output: output.clone(),
                            })?;
                            outputs.insert(
                                call_index,
                                GroupTaskOutput {
                                    output,
                                    report: None,
                                },
                            );
                        }
                    }
                }
            }
            let mut remaining = slots.iter().filter(|slot| slot.future.is_some()).count();
            let mut spinner_interval = tokio::time::interval(self.spinner_interval);
            spinner_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            spinner_interval.tick().await;
            while remaining > 0 {
                let event = {
                    let poll_slots = std::future::poll_fn(|context| {
                        for (position, slot) in slots.iter_mut().enumerate() {
                            if let std::task::Poll::Ready(Some(progress)) =
                                slot.progress.poll_recv(context)
                            {
                                return std::task::Poll::Ready(WaveEvent::Progress(
                                    position, progress,
                                ));
                            }
                            if let Some(future) = slot.future.as_mut() {
                                if let std::task::Poll::Ready(result) =
                                    future.as_mut().poll(context)
                                {
                                    slot.future = None;
                                    return std::task::Poll::Ready(WaveEvent::Done(
                                        position, result,
                                    ));
                                }
                            }
                        }
                        std::task::Poll::Pending
                    });
                    tokio::select! {
                        event = poll_slots => event,
                        _ = spinner_interval.tick() => WaveEvent::Spinner,
                    }
                };
                match event {
                    WaveEvent::Spinner => on_event(AgentEvent::SpinnerTick)?,
                    WaveEvent::Progress(position, progress) => {
                        emit_tool_progress(
                            on_event,
                            &slots[position].call_id,
                            &slots[position].event_name,
                            progress,
                        )?;
                    }
                    WaveEvent::Done(position, result) => {
                        remaining -= 1;
                        while let Ok(progress) = slots[position].progress.try_recv() {
                            emit_tool_progress(
                                on_event,
                                &slots[position].call_id,
                                &slots[position].event_name,
                                progress,
                            )?;
                        }
                        let call_index = slots[position].call_index;
                        let call_id = slots[position].call_id.clone();
                        let event_name = slots[position].event_name.clone();
                        match result {
                            Ok(output) => {
                                on_event(AgentEvent::ToolResult {
                                    call_id,
                                    name: event_name,
                                    ok: true,
                                    output: output.clone(),
                                })?;
                                let report = extract_persistable_tool_report("task", &output);
                                outputs.insert(call_index, GroupTaskOutput { output, report });
                            }
                            Err(err) => {
                                let output = format!("tool error: {err}");
                                on_event(AgentEvent::ToolResult {
                                    call_id,
                                    name: event_name,
                                    ok: false,
                                    output: output.clone(),
                                })?;
                                outputs.insert(
                                    call_index,
                                    GroupTaskOutput {
                                        output,
                                        report: None,
                                    },
                                );
                            }
                        }
                    }
                }
            }
        }
        Ok(outputs)
    }

    pub(in crate::agent) async fn consume_queued_prompts<F>(
        &mut self,
        current_turn_id: &str,
        messages: &mut Vec<ChatMessage>,
        queued: Vec<QueuedPrompt>,
        preceding_assistant: (Option<&str>, Option<&str>, Option<&str>, Option<&str>),
        checkpoint: TurnRedoCheckpointPayload,
        control: &AgentTurnControl,
        on_event: &mut F,
    ) -> Result<()>
    where
        F: FnMut(AgentEvent) -> Result<()>,
    {
        on_event(AgentEvent::FlushJournal)?;
        // 排队消息=用户更新了请求,平台生图配额随之重置(非平台回合 no-op)。
        crate::tools::workspace::reset_image_gen_limit();
        let mut prepared = Vec::with_capacity(queued.len());
        for prompt in queued {
            let images = self.queued_prompt_images(&prompt)?;
            let input = self.prepare_user_input(&prompt.content, &images).await?;
            prepared.push((prompt, input));
        }

        let mode = control.mode();
        if self.mode != mode {
            self.switch_mode(mode, control.tools(mode));
            self.refresh_system_prompt()?;
        }
        replace_request_mode_context(
            messages,
            &self.system_prompt,
            mode,
            self.platform_context.is_some(),
        );

        // 瞬态尾巴挂在 followup 正文之后,并与正文一起化石化(v31):runtime
        // 按首轮同一口径"变了才追加"(只挂第一条),图片路径/context-images
        // 提示原样跟随。live 推进 messages 的就是这同一份 tail,活体与化石
        // 逐字节一致——少了这一步,下一轮回放在 followup 处比活体短一截,
        // 缓存前缀与 CLI 续传链都在这里掰断(09-04 codex 线实证)。
        let runtime = runtime_context(mode, self.platform_context.is_some());
        let runtime_block = (last_fossil_with_prefix(messages, "<runtime ")
            != Some(runtime.as_str()))
        .then(|| ChatMessage::turn_context(runtime));
        let mut consumed = Vec::with_capacity(prepared.len());
        let mut tails: Vec<Vec<ChatMessage>> = Vec::with_capacity(prepared.len());
        for (index, (prompt, input)) in prepared.iter().enumerate() {
            let mut tail = Vec::new();
            if index == 0 {
                tail.extend(runtime_block.clone());
            }
            tail.extend(fossil_context_messages(&input.hints));
            consumed.push((
                prompt.prompt_id.clone(),
                input.content.clone(),
                serde_json::to_string(&tail)?,
            ));
            tails.push(tail);
        }
        self.state.consume_queued_prompts_with_checkpoint(
            current_turn_id,
            &consumed,
            preceding_assistant
                .0
                .filter(|content| !content.trim().is_empty()),
            preceding_assistant
                .1
                .filter(|reasoning| !reasoning.trim().is_empty()),
            preceding_assistant
                .2
                .filter(|provider_id| !provider_id.trim().is_empty()),
            preceding_assistant
                .3
                .filter(|model| !model.trim().is_empty()),
            checkpoint,
        )?;
        for (prompt, _) in &prepared {
            if let Some(context) = self.platform_context.clone() {
                let files = context.take_queued_files(&prompt.prompt_id);
                if !files.is_empty() {
                    self.context_files.extend(files);
                    self.set_platform_context_files(context, self.context_files.clone());
                }
            }
        }
        on_event(AgentEvent::QueuedPromptsConsumed {
            prompt_ids: consumed.iter().map(|(id, _, _)| id.clone()).collect(),
            mode,
            provider_id: preceding_assistant.2.map(str::to_string),
            model: preceding_assistant.3.map(str::to_string),
        })?;

        for ((_, input), tail) in prepared.into_iter().zip(tails) {
            messages.push(input.message);
            messages.extend(tail);
        }
        Ok(())
    }

    /// 浮动尾部人格提醒,所有会话形态(终端/WebUI/平台)一致生效:命中
    /// 缓存时只是一次小文件读,缓存未建时对同一 client 蒸馏一次(每份
    /// 人格内容一生只发生一次)。蒸馏失败降级为无提醒,绝不阻断回合。
    pub(in crate::agent) async fn resolve_persona_reminder(&self) -> Option<String> {
        // Dev 无人格,自然无防失忆提醒。
        if self.mode == AgentMode::Dev {
            return None;
        }
        if !self.config.prompt.persona_reminder {
            return None;
        }
        match persona_hint::resolve(&self.config, &self.paths, &self.client).await {
            Ok(reminder) => reminder,
            Err(error) => {
                tracing::warn!(error = %error, "persona reminder distillation failed");
                None
            }
        }
    }
}
