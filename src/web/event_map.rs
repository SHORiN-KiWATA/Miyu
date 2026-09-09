//! 把 agent 事件翻成前端事件。
//!
//! agent 发的是「思考开始了」「工具在流参数」「这一轮用了多少 token」这类内部
//! 事件，前端要的是能直接画的东西。中间这层还负责把图片、artifact 之类的产物
//! 落库成资产再交出 ID——事件流本身不搬字节。

use crate::web::*;

pub(in crate::web) struct RunEventMapper {
    pub(in crate::web) run_id: String,
    pub(in crate::web) events: EventHub,
    pub(in crate::web) questions: QuestionBroker,
    pub(in crate::web) state_store: StateStore,
    pub(in crate::web) manager: Arc<Mutex<ManagerState>>,
    pub(in crate::web) turn_id: Option<String>,
    pub(in crate::web) active_tools: Vec<ActiveTool>,
    pub(in crate::web) queue_ingress: Option<Arc<crate::agent::QueueIngressBarrier>>,
    pub(in crate::web) operation: &'static str,
    pub(in crate::web) redo_input_id: Option<String>,
    pub(in crate::web) redo_display_content: Option<String>,
    pub(in crate::web) command_output_lines: usize,
    /// 最近一次模型请求的 (provider, model),盖到工具事件上供日志标注。
    pub(in crate::web) round_endpoint: Option<(String, String)>,
}

pub(in crate::web) struct ActiveTool {
    pub(in crate::web) id: String,
    pub(in crate::web) name: String,
    pub(in crate::web) display_name: String,
    pub(in crate::web) command_output: Option<crate::render::CommandOutputTail>,
}

impl RunEventMapper {
    pub(in crate::web) fn new(
        run_id: String,
        events: EventHub,
        questions: QuestionBroker,
        state_store: StateStore,
        manager: Arc<Mutex<ManagerState>>,
        queue_ingress: Option<Arc<crate::agent::QueueIngressBarrier>>,
        operation: &'static str,
        redo_input_id: Option<String>,
        redo_display_content: Option<String>,
        command_output_lines: usize,
    ) -> Self {
        Self {
            run_id,
            events,
            questions,
            state_store,
            manager,
            turn_id: None,
            active_tools: Vec::new(),
            queue_ingress,
            operation,
            redo_input_id,
            redo_display_content,
            command_output_lines,
            round_endpoint: None,
        }
    }

    pub(in crate::web) fn publish(&self, kind: &str, data: Value) {
        self.events.publish(kind, data);
    }

    pub(in crate::web) fn next_tool(&self, call_id: String, event_name: String) -> ActiveTool {
        let name = real_tool_name(&event_name).to_string();
        let display_name = tools::readable_tool_name(&event_name);
        ActiveTool {
            id: call_id,
            command_output: crate::render::is_command_tool(&name)
                .then(|| crate::render::CommandOutputTail::new(self.command_output_lines)),
            name,
            display_name,
        }
    }

    pub(in crate::web) fn handle(&mut self, event: AgentEvent) {
        match event {
            AgentEvent::TurnStarted { turn_id } => {
                self.turn_id = Some(turn_id.clone());
                if let Some(run) = self
                    .manager
                    .lock()
                    .unwrap()
                    .active_runs
                    .get_mut(&self.run_id)
                {
                    run.turn_id = Some(turn_id.clone());
                    run.queue_target = Some(self.state_store.queue_target(turn_id.clone()));
                }
                self.publish(
                    "turn.started",
                    json!({
                        "run_id": self.run_id,
                        "turn_id": turn_id,
                        "operation": self.operation,
                        "input_id": self.redo_input_id,
                        "display_content": self.redo_display_content,
                    }),
                );
            }
            AgentEvent::RawReasoning(_) => {}
            AgentEvent::FlushJournal => {}
            AgentEvent::Chunk(chunk) => match chunk.kind {
                ChatStreamKind::Content => self.publish(
                    "assistant.delta",
                    json!({ "run_id": self.run_id, "delta": chunk.text }),
                ),
                ChatStreamKind::Reasoning => self.publish(
                    "reasoning.delta",
                    json!({ "run_id": self.run_id, "delta": chunk.text }),
                ),
                _ => {}
            },
            AgentEvent::ReasoningStart { .. } => {
                self.publish("reasoning.start", json!({ "run_id": self.run_id }))
            }
            AgentEvent::ReasoningReset { .. } => {
                self.publish("reasoning.reset", json!({ "run_id": self.run_id }))
            }
            AgentEvent::ReasoningPartStart { .. } => {
                self.publish("reasoning.part_start", json!({ "run_id": self.run_id }))
            }
            AgentEvent::ReasoningPartEnd { .. } => {
                self.publish("reasoning.part_end", json!({ "run_id": self.run_id }))
            }
            AgentEvent::ReasoningTitle(title) => self.publish(
                "reasoning.title",
                json!({ "run_id": self.run_id, "title": title }),
            ),
            AgentEvent::ToolCall {
                call_id,
                name,
                arguments,
            } => {
                if let Some(queue_ingress) = self.queue_ingress.as_ref() {
                    queue_ingress.tool_started(&call_id);
                }
                let tool = self.next_tool(call_id, name);
                self.publish(
                    "tool.started",
                    json!({
                        "run_id": self.run_id,
                        "tool_id": tool.id,
                        "name": tool.name,
                        "display_name": tool.display_name,
                        "arguments": arguments,
                        "provider_id": self.round_endpoint.as_ref().map(|(provider, _)| provider),
                        "model": self.round_endpoint.as_ref().map(|(_, model)| model),
                    }),
                );
                self.active_tools.push(tool);
            }
            // `name` is the raw tool name, matching `tool.started` — it used to
            // be the readable one here alone, which is an easy way to wire a
            // consumer to the wrong field. `tool_name` stays as an alias for
            // browsers still running a cached asset.
            AgentEvent::ToolPreparing { name, batch } => self.publish(
                "tool.preparing",
                json!({
                    "run_id": self.run_id,
                    "name": &name,
                    "tool_name": &name,
                    // 同一条消息里的第 2+ 个工具调用。终端在自己那侧解析
                    // 提示词（i18n 归渲染层），所以标志位也要过 IPC。
                    "batch": batch,
                    "display_name": tools::readable_tool_name(&name),
                    // Sent so the WebUI label tracks the backend list instead
                    // of keeping its own copy in sync.
                    "phase": tools::preparing_phase(&name)
                        .or_else(|| batch.then(tools::batch_preparing_phase)),
                }),
            ),
            AgentEvent::ToolProgress {
                call_id,
                name,
                message,
            } => {
                let (tool_id, tool_name) = self.tool_identity(&call_id, &name);
                self.publish(
                    "tool.progress",
                    json!({
                        "run_id": self.run_id,
                        "tool_id": tool_id,
                        "name": tool_name,
                        "message": message,
                    }),
                );
            }
            AgentEvent::CommandOutput {
                call_id,
                name,
                stream,
                chunk,
            } => {
                let stream_name = match stream {
                    CommandOutputStream::Stdout => "stdout",
                    CommandOutputStream::Stderr => "stderr",
                };
                let (tool_id, tool_name, preview) = if let Some(tool) =
                    self.active_tools.iter_mut().find(|tool| tool.id == call_id)
                {
                    let preview = tool.command_output.as_mut().map(|output| {
                        output.push(stream, &chunk);
                        output.preview()
                    });
                    (tool.id.clone(), tool.name.clone(), preview)
                } else {
                    (call_id.clone(), real_tool_name(&name).to_string(), None)
                };
                self.publish(
                    "tool.output",
                    json!({
                        "run_id": self.run_id,
                        "tool_id": tool_id,
                        "name": tool_name,
                        "stream": stream_name,
                        "output": String::from_utf8_lossy(&chunk),
                        "preview": preview,
                    }),
                );
            }
            AgentEvent::ToolResult {
                call_id,
                name,
                ok,
                output,
            } => {
                if let Some(queue_ingress) = self.queue_ingress.as_ref() {
                    queue_ingress.tool_finished(&call_id);
                }
                let mut tool = self
                    .active_tools
                    .iter()
                    .position(|tool| tool.id == call_id)
                    .map(|index| self.active_tools.remove(index))
                    .unwrap_or_else(|| self.next_tool(call_id, name));
                let preview = tool.command_output.as_mut().map(|output| {
                    output.finalize();
                    output.preview()
                });
                self.publish(
                    "tool.finished",
                    json!({
                        "run_id": self.run_id,
                        "tool_id": tool.id,
                        "name": tool.name,
                        "display_name": tool.display_name,
                        "ok": ok,
                        "output": output,
                        "preview": preview,
                        "provider_id": self.round_endpoint.as_ref().map(|(provider, _)| provider),
                        "model": self.round_endpoint.as_ref().map(|(_, model)| model),
                    }),
                );
            }
            AgentEvent::PrepareForExternalOutput { ready } => {
                let _ = ready.send(false);
            }
            AgentEvent::Image {
                call_id,
                name,
                path,
                alt,
                size,
            } => {
                let (tool_id, tool_name) = self.tool_identity(&call_id, &name);
                // 从原始事件名判:tool_name 已被 real_tool_name 剥成
                // "use_meme",分不出是 show 还是 search
                let hide_caption = name == "use_meme:show";
                let Some(turn_id) = self.turn_id.as_deref() else {
                    // 这条分支此前完全无日志:图既不落库也不进平台投递,
                    // 表现为「生图 ok 但群里没图」却查无痕迹(08-20 生图三连丢
                    // 的排查盲区)。
                    tracing::warn!(
                        run_id = %self.run_id,
                        tool = %tool_name,
                        "a tool image arrived before the turn id was known and was dropped"
                    );
                    self.publish(
                        "tool.image",
                        json!({
                            "run_id": self.run_id,
                            "tool_id": tool_id,
                            "name": tool_name,
                            "error": "image could not be associated with the current turn",
                        }),
                    );
                    return;
                };
                match self
                    .state_store
                    .save_image_asset(turn_id, Some(&tool_id), &path, &alt)
                {
                    Ok(asset) => self.publish(
                        "tool.image",
                        json!({
                            "run_id": self.run_id,
                            "tool_id": tool_id,
                            "name": tool_name,
                            // 模型显式要的尺寸。缺省交给终端按配置百分比
                            // 自己算——daemon 量到的不是用户的终端。
                            "size": size,
                            "asset": SafeImageAsset::from_asset(asset, hide_caption),
                        }),
                    ),
                    Err(error) => {
                        // warn 在默认 ERROR 级别下不可见——图片静默丢失(08-21)。
                        tracing::error!(
                            run_id = %self.run_id,
                            tool = %tool_name,
                            error = %error,
                            "{}",
                            t("failed to persist a WebUI image", "WebUI 图像保存失败")
                        );
                        self.publish(
                            "tool.image",
                            json!({
                                "run_id": self.run_id,
                                "tool_id": tool_id,
                                "name": tool_name,
                                "error": "image could not be added to the WebUI",
                            }),
                        );
                    }
                }
            }
            AgentEvent::Artifact {
                call_id,
                name,
                path,
                title,
            } => {
                let (tool_id, tool_name) = self.tool_identity(&call_id, &name);
                let Some(turn_id) = self.turn_id.as_deref() else {
                    self.publish(
                        "tool.artifact",
                        json!({
                            "run_id": self.run_id,
                            "tool_id": tool_id,
                            "name": tool_name,
                            "error": "artifact could not be associated with the current turn",
                        }),
                    );
                    return;
                };
                match self
                    .state_store
                    .save_artifact_asset(turn_id, Some(&tool_id), &path, &title)
                {
                    Ok(asset) => self.publish(
                        "tool.artifact",
                        json!({
                            "run_id": self.run_id,
                            "tool_id": tool_id,
                            "name": tool_name,
                            "artifact": SafeArtifactAsset::from(asset),
                        }),
                    ),
                    Err(error) => {
                        tracing::warn!(run_id = %self.run_id, tool = %tool_name, error = %error, "failed to persist a WebUI artifact");
                        self.publish(
                            "tool.artifact",
                            json!({
                                "run_id": self.run_id,
                                "tool_id": tool_id,
                                "name": tool_name,
                                "error": "file could not be added to the WebUI preview",
                            }),
                        );
                    }
                }
            }
            AgentEvent::AskQuestion {
                call_id,
                request,
                responder,
            } => {
                let question_id = self
                    .questions
                    .insert(&self.run_id, request.clone(), responder);
                let (tool_id, tool_name) = self.tool_identity(&call_id, "ask_question");
                self.publish(
                    "question.requested",
                    json!({
                        "run_id": self.run_id,
                        "question_id": question_id,
                        "tool_id": tool_id,
                        "name": tool_name,
                        "questions": request.questions,
                    }),
                );
            }
            AgentEvent::QueuedPromptsConsumed {
                prompt_ids,
                mode,
                provider_id,
                model,
            } => self.publish(
                "queue.consumed",
                json!({
                    "run_id": self.run_id,
                    "prompt_ids": prompt_ids,
                    "mode": mode_name(mode),
                    "provider_id": provider_id,
                    "model": model,
                }),
            ),
            AgentEvent::GenerationSuperseded { prompt_ids } => self.publish(
                "generation.superseded",
                json!({
                    "run_id": self.run_id,
                    "turn_id": self.turn_id,
                    "prompt_ids": prompt_ids,
                }),
            ),
            AgentEvent::SpinnerTick => {}
            // 逐请求计量快照:round 为刚结束请求的用量(prompt+completion
            // ≈ 当前上下文占用),turn 为回合累计。前端据此在回合中途刷新
            // 计量条,不必等 chat.done。
            AgentEvent::RoundUsage {
                round,
                turn,
                speed,
                estimated,
                provider_id,
                model,
            } => {
                self.round_endpoint = match (provider_id.as_deref(), model.as_deref()) {
                    (None, None) => None,
                    (provider, model) => Some((
                        provider.unwrap_or_default().to_string(),
                        model.unwrap_or_default().to_string(),
                    )),
                };
                self.publish(
                    "chat.round_usage",
                    json!({
                        "run_id": self.run_id,
                        "turn_id": self.turn_id,
                        "usage": *round,
                        "turn_total": turn.total,
                        "turn_prompt": turn.prompt,
                        "turn_cache_read": turn.cache_read,
                        "turn_generation_tokens": speed.tokens,
                        "turn_generation_ms": speed.millis,
                        "estimated": estimated,
                        "provider_id": provider_id,
                        "model": model,
                    }),
                );
            }
            AgentEvent::CompactStart => {
                self.publish("context.compact_start", json!({ "run_id": self.run_id }))
            }
            AgentEvent::CompactChunk(chunk) => self.publish(
                "context.compact_delta",
                json!({ "run_id": self.run_id, "delta": chunk.text }),
            ),
            AgentEvent::CompactEnd => {
                self.publish("context.compact_end", json!({ "run_id": self.run_id }))
            }
            AgentEvent::PopStart => {
                self.publish("context.pop_start", json!({ "run_id": self.run_id }))
            }
            AgentEvent::PopEnd => self.publish("context.pop_end", json!({ "run_id": self.run_id })),
            AgentEvent::Notice { text } => self.publish(
                "context.notice",
                json!({ "run_id": self.run_id, "text": text }),
            ),
        }
    }

    pub(in crate::web) fn tool_identity(&self, call_id: &str, fallback: &str) -> (String, String) {
        self.active_tools
            .iter()
            .find(|tool| tool.id == call_id)
            .map(|tool| (tool.id.clone(), tool.name.clone()))
            .unwrap_or_else(|| (call_id.to_string(), real_tool_name(fallback).to_string()))
    }
}

pub(in crate::web) struct SseStreamState {
    pub(in crate::web) pending: VecDeque<EventRecord>,
    pub(in crate::web) receiver: broadcast::Receiver<EventRecord>,
    pub(in crate::web) events: EventHub,
    pub(in crate::web) last_id: u64,
}

pub(in crate::web) fn record_to_sse(record: EventRecord) -> Event {
    Event::default()
        .id(record.id.to_string())
        .event(record.kind)
        .data(record.data)
}
