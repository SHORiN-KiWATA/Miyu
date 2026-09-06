//! 回合流水的落盘。
//!
//! 模型输出是流式的，但落盘不能每个 chunk 一次 IO。攒够 `JOURNAL_FLUSH_BYTES`
//! 或到了 `JOURNAL_FLUSH_INTERVAL` 才刷一次。
//!
//! 顺序是硬要求：**流水必须先落盘再显示**。反过来的话，进程在显示后落盘前挂
//! 掉，用户看见过的内容就再也找不回来了。

use crate::agent::*;

pub(in crate::agent) const JOURNAL_FLUSH_BYTES: usize = 16 * 1024;

pub(in crate::agent) const JOURNAL_FLUSH_INTERVAL: Duration = Duration::from_millis(80);

pub(in crate::agent) struct PendingJournalChunk {
    pub(in crate::agent) kind: ChatStreamKind,
    pub(in crate::agent) text: String,
}

/// Persists semantic stream events before forwarding them to a transport.
/// Small adjacent deltas are coalesced so a long answer does not turn into a
/// SQLite transaction per provider token.
pub(in crate::agent) struct TurnJournalSink {
    pub(in crate::agent) state: StateStore,
    pub(in crate::agent) turn_id: String,
    pub(in crate::agent) revision: i64,
    pub(in crate::agent) segment_index: i64,
    pub(in crate::agent) pending: Option<PendingJournalChunk>,
    pub(in crate::agent) pending_reasoning_display: String,
    pub(in crate::agent) last_flush: Instant,
}

impl TurnJournalSink {
    pub(in crate::agent) fn new(state: StateStore, turn_id: String, revision: i64) -> Self {
        Self {
            state,
            turn_id,
            revision,
            segment_index: 0,
            pending: None,
            pending_reasoning_display: String::new(),
            last_flush: Instant::now(),
        }
    }

    pub(in crate::agent) fn emit<F>(&mut self, event: AgentEvent, on_event: &mut F) -> Result<()>
    where
        F: FnMut(AgentEvent) -> Result<()>,
    {
        match event {
            AgentEvent::Chunk(chunk)
                if matches!(
                    chunk.kind,
                    ChatStreamKind::Content | ChatStreamKind::ToolCall
                ) =>
            {
                self.push_chunk(chunk, on_event)
            }
            AgentEvent::Chunk(chunk) if chunk.kind == ChatStreamKind::Reasoning => {
                self.pending_reasoning_display.push_str(&chunk.text);
                Ok(())
            }
            AgentEvent::RawReasoning(chunk) => {
                if chunk.kind == ChatStreamKind::Reasoning && !chunk.text.is_empty() {
                    self.push_chunk(chunk, on_event)
                } else {
                    Ok(())
                }
            }
            AgentEvent::FlushJournal => self.flush(on_event),
            AgentEvent::SpinnerTick => {
                self.flush(on_event)?;
                on_event(AgentEvent::SpinnerTick)
            }
            // 瞬态计量快照,只给 UI,不入回放日志。
            event @ AgentEvent::RoundUsage { .. } => on_event(event),
            AgentEvent::ReasoningStart { received_at } => {
                self.flush(on_event)?;
                self.append("reasoning_start", None, None, None, None, None)?;
                on_event(AgentEvent::ReasoningStart { received_at })
            }
            AgentEvent::ReasoningReset { received_at } => {
                self.flush(on_event)?;
                self.append("reasoning_reset", None, None, None, None, None)?;
                on_event(AgentEvent::ReasoningReset { received_at })
            }
            AgentEvent::ReasoningPartStart { received_at } => {
                self.flush(on_event)?;
                self.append("reasoning_part_start", None, None, None, None, None)?;
                on_event(AgentEvent::ReasoningPartStart { received_at })
            }
            AgentEvent::ReasoningPartEnd { received_at } => {
                self.flush(on_event)?;
                self.append("reasoning_part_end", None, None, None, None, None)?;
                on_event(AgentEvent::ReasoningPartEnd { received_at })
            }
            AgentEvent::ReasoningTitle(title) => {
                self.flush(on_event)?;
                self.append("reasoning_title", None, None, Some(&title), None, None)?;
                on_event(AgentEvent::ReasoningTitle(title))
            }
            AgentEvent::ToolCall {
                call_id,
                name,
                arguments,
            } => {
                self.flush(on_event)?;
                self.append(
                    "tool_call",
                    Some(&call_id),
                    Some(&name),
                    Some(&arguments),
                    None,
                    None,
                )?;
                on_event(AgentEvent::ToolCall {
                    call_id,
                    name,
                    arguments,
                })
            }
            AgentEvent::ToolPreparing { name, batch } => {
                self.flush(on_event)?;
                self.append("tool_preparing", None, Some(&name), Some(&name), None, None)?;
                on_event(AgentEvent::ToolPreparing { name, batch })
            }
            AgentEvent::ToolResult {
                call_id,
                name,
                ok,
                output,
            } => {
                self.flush(on_event)?;
                self.append(
                    "tool_result",
                    Some(&call_id),
                    Some(&name),
                    Some(&output),
                    None,
                    Some(ok),
                )?;
                on_event(AgentEvent::ToolResult {
                    call_id,
                    name,
                    ok,
                    output,
                })
            }
            AgentEvent::ToolProgress {
                call_id,
                name,
                message,
            } => {
                self.flush(on_event)?;
                self.append(
                    "tool_progress",
                    Some(&call_id),
                    Some(&name),
                    Some(&message),
                    None,
                    None,
                )?;
                on_event(AgentEvent::ToolProgress {
                    call_id,
                    name,
                    message,
                })
            }
            AgentEvent::CommandOutput {
                call_id,
                name,
                stream,
                chunk,
            } => {
                self.flush(on_event)?;
                let kind = match stream {
                    tools::CommandOutputStream::Stdout => "command_stdout",
                    tools::CommandOutputStream::Stderr => "command_stderr",
                };
                self.append(kind, Some(&call_id), Some(&name), None, Some(&chunk), None)?;
                on_event(AgentEvent::CommandOutput {
                    call_id,
                    name,
                    stream,
                    chunk,
                })
            }
            AgentEvent::Image {
                call_id,
                name,
                path,
                alt,
                size,
            } => {
                self.flush(on_event)?;
                // size 刻意不落库:它是这一次的展示偏好,回放时终端可能已经
                // 换了尺寸,按当时的配置百分比重算才对。
                let payload = serde_json::json!({
                    "path": path.display().to_string(),
                    "alt": alt,
                });
                let payload = serde_json::to_string(&payload)?;
                self.append(
                    "image",
                    Some(&call_id),
                    Some(&name),
                    Some(&payload),
                    None,
                    None,
                )?;
                on_event(AgentEvent::Image {
                    call_id,
                    name,
                    path,
                    alt,
                    size,
                })
            }
            AgentEvent::Artifact {
                call_id,
                name,
                path,
                title,
            } => {
                self.flush(on_event)?;
                let payload = serde_json::json!({
                    "path": path.display().to_string(),
                    "title": title,
                });
                let payload = serde_json::to_string(&payload)?;
                self.append(
                    "artifact",
                    Some(&call_id),
                    Some(&name),
                    Some(&payload),
                    None,
                    None,
                )?;
                on_event(AgentEvent::Artifact {
                    call_id,
                    name,
                    path,
                    title,
                })
            }
            AgentEvent::AskQuestion {
                call_id,
                request,
                responder,
            } => {
                self.flush(on_event)?;
                let payload = serde_json::to_string(&request)?;
                self.append(
                    "question",
                    Some(&call_id),
                    Some("ask_question"),
                    Some(&payload),
                    None,
                    None,
                )?;
                on_event(AgentEvent::AskQuestion {
                    call_id,
                    request,
                    responder,
                })
            }
            AgentEvent::GenerationSuperseded { prompt_ids } => {
                self.flush(on_event)?;
                self.state.supersede_turn_journal_segment(
                    &self.turn_id,
                    self.revision,
                    self.segment_index,
                )?;
                on_event(AgentEvent::GenerationSuperseded { prompt_ids })
            }
            AgentEvent::QueuedPromptsConsumed {
                prompt_ids,
                mode,
                provider_id,
                model,
            } => {
                self.flush(on_event)?;
                self.segment_index = self.segment_index.saturating_add(1);
                on_event(AgentEvent::QueuedPromptsConsumed {
                    prompt_ids,
                    mode,
                    provider_id,
                    model,
                })
            }
            AgentEvent::CompactStart
            | AgentEvent::CompactChunk(_)
            | AgentEvent::CompactEnd
            | AgentEvent::PopStart
            | AgentEvent::PopEnd
            | AgentEvent::Notice { .. }
            | AgentEvent::TurnStarted { .. }
            | AgentEvent::PrepareForExternalOutput { .. } => on_event(event),
            AgentEvent::Chunk(chunk) => on_event(AgentEvent::Chunk(chunk)),
        }
    }

    pub(in crate::agent) fn push_chunk<F>(
        &mut self,
        chunk: ChatStreamChunk,
        on_event: &mut F,
    ) -> Result<()>
    where
        F: FnMut(AgentEvent) -> Result<()>,
    {
        if self.pending.is_none() && !self.pending_reasoning_display.is_empty() {
            self.flush(on_event)?;
        }
        let should_flush = self.pending.as_ref().is_some_and(|pending| {
            pending.kind != chunk.kind
                || pending.text.len().saturating_add(chunk.text.len()) >= JOURNAL_FLUSH_BYTES
                || self.last_flush.elapsed() >= JOURNAL_FLUSH_INTERVAL
        });
        if should_flush {
            self.flush(on_event)?;
        }
        if let Some(pending) = self.pending.as_mut() {
            pending.text.push_str(&chunk.text);
        } else {
            self.pending = Some(PendingJournalChunk {
                kind: chunk.kind,
                text: chunk.text,
            });
        }
        if self
            .pending
            .as_ref()
            .is_some_and(|pending| pending.text.len() >= JOURNAL_FLUSH_BYTES)
        {
            self.flush(on_event)?;
        }
        Ok(())
    }

    pub(in crate::agent) fn flush<F>(&mut self, on_event: &mut F) -> Result<()>
    where
        F: FnMut(AgentEvent) -> Result<()>,
    {
        let Some(pending) = self.pending.take() else {
            if self.pending_reasoning_display.is_empty() {
                return Ok(());
            }
            let text = std::mem::take(&mut self.pending_reasoning_display);
            on_event(AgentEvent::Chunk(ChatStreamChunk {
                kind: ChatStreamKind::Reasoning,
                text,
            }))?;
            self.last_flush = Instant::now();
            return Ok(());
        };
        let kind = match pending.kind {
            ChatStreamKind::Content => "assistant_content",
            ChatStreamKind::Reasoning => "assistant_reasoning",
            ChatStreamKind::ToolCall => "tool_call_delta",
            ChatStreamKind::ReasoningReset
            | ChatStreamKind::ReasoningPartStart
            | ChatStreamKind::ReasoningPartEnd
            // 中转侧工具活动在事件层已成卡片,journal 的流缓冲不收。
            | ChatStreamKind::RemoteToolPreparing
            | ChatStreamKind::RemoteToolStarted
            | ChatStreamKind::RemoteToolFinished => return Ok(()),
        };
        self.append(kind, None, None, Some(&pending.text), None, None)?;
        self.last_flush = Instant::now();
        if pending.kind == ChatStreamKind::Reasoning {
            let text = std::mem::take(&mut self.pending_reasoning_display);
            if text.is_empty() {
                return Ok(());
            }
            return on_event(AgentEvent::Chunk(ChatStreamChunk {
                kind: ChatStreamKind::Reasoning,
                text,
            }));
        }
        on_event(AgentEvent::Chunk(ChatStreamChunk {
            kind: pending.kind,
            text: pending.text,
        }))
    }

    pub(in crate::agent) fn finish<F>(&mut self, on_event: &mut F) -> Result<()>
    where
        F: FnMut(AgentEvent) -> Result<()>,
    {
        self.flush(on_event)
    }

    pub(in crate::agent) fn append(
        &self,
        kind: &str,
        call_id: Option<&str>,
        name: Option<&str>,
        text_payload: Option<&str>,
        blob_payload: Option<&[u8]>,
        ok: Option<bool>,
    ) -> Result<()> {
        self.state.append_turn_journal_event(
            &self.turn_id,
            self.revision,
            self.segment_index,
            kind,
            call_id,
            name,
            text_payload,
            blob_payload,
            ok,
        )
    }
}
