//! Anthropic Messages 协议的事件解析。
//!
//! 事件是「块」粒度的：`content_block_start/delta/stop` 围出一个块，思考块还带
//! 签名。`AnthropicStreamState` 记住当前在哪个块里，`flush_anthropic_state` 在
//! 块结束时把攒的内容交出去。
//!
//! 用量分两次到达（开始时有 input、结束时有 output），`merge_anthropic_usage`
//! 负责合并。

use crate::llm::openai_compatible::sse::*;
use crate::llm::openai_compatible::*;

#[derive(Default)]
pub(in crate::llm::openai_compatible) struct AnthropicStreamState {
    pub(in crate::llm::openai_compatible) content: String,
    pub(in crate::llm::openai_compatible) content_emitted: usize,
    pub(in crate::llm::openai_compatible) reasoning: String,
    pub(in crate::llm::openai_compatible) reasoning_emitted: usize,
    pub(in crate::llm::openai_compatible) reasoning_part_active: bool,
    pub(in crate::llm::openai_compatible) thinking_signature: Option<String>,
    pub(in crate::llm::openai_compatible) usage: Option<Usage>,
    pub(in crate::llm::openai_compatible) tool_calls: AnthropicToolAccumulator,
    pub(in crate::llm::openai_compatible) stop_reason: Option<String>,
}

pub(in crate::llm::openai_compatible) fn handle_anthropic_sse_data<F>(
    data: &str,
    state: &mut AnthropicStreamState,
    on_chunk: &mut F,
) -> Result<bool>
where
    F: FnMut(ChatStreamChunk) -> Result<()>,
{
    if data == "[DONE]" {
        flush_anthropic_state(state, on_chunk)?;
        return Ok(true);
    }
    let event: AnthropicStreamEvent = serde_json::from_str(data).with_context(|| {
        format!(
            "{}: {}",
            t(
                "invalid anthropic messages stream event",
                "无效的 Anthropic Messages 流式事件"
            ),
            clean_plain_text(data.to_string())
        )
    })?;
    match event.kind.as_str() {
        "message_start" => {
            if let Some(usage) = event.message.and_then(|message| message.usage) {
                merge_anthropic_usage(&mut state.usage, usage);
            }
        }
        "content_block_start" => {
            if let Some(block) = event.content_block {
                match block.kind.as_str() {
                    "tool_use" | "server_tool_use" => {
                        if let Some(index) = event.index {
                            if let Some(name) = state.tool_calls.start(index, block) {
                                on_chunk(ChatStreamChunk {
                                    kind: ChatStreamKind::ToolCall,
                                    text: name,
                                })?;
                            }
                        }
                    }
                    "text" => {
                        if state.reasoning_part_active {
                            flush_buffer(
                                &mut state.reasoning,
                                &mut state.reasoning_emitted,
                                ChatStreamKind::Reasoning,
                                on_chunk,
                                true,
                            )?;
                            on_chunk(ChatStreamChunk {
                                kind: ChatStreamKind::ReasoningPartEnd,
                                text: String::new(),
                            })?;
                            state.reasoning_part_active = false;
                        }
                        if let Some(text) = block.text {
                            push_buffered_chunk(
                                &mut state.content,
                                &mut state.content_emitted,
                                ChatStreamKind::Content,
                                text,
                                on_chunk,
                            )?;
                        }
                    }
                    "thinking" => {
                        if state.reasoning_part_active {
                            flush_buffer(
                                &mut state.reasoning,
                                &mut state.reasoning_emitted,
                                ChatStreamKind::Reasoning,
                                on_chunk,
                                true,
                            )?;
                            on_chunk(ChatStreamChunk {
                                kind: ChatStreamKind::ReasoningPartEnd,
                                text: String::new(),
                            })?;
                        }
                        if !state.reasoning.is_empty() && !state.reasoning.ends_with("\n\n") {
                            state.reasoning.push_str("\n\n");
                        }
                        on_chunk(ChatStreamChunk {
                            kind: ChatStreamKind::ReasoningPartStart,
                            text: String::new(),
                        })?;
                        state.reasoning_part_active = true;
                        if let Some(text) = block.thinking {
                            push_buffered_chunk(
                                &mut state.reasoning,
                                &mut state.reasoning_emitted,
                                ChatStreamKind::Reasoning,
                                text,
                                on_chunk,
                            )?;
                        }
                    }
                    _ => {}
                }
            }
        }
        "content_block_delta" => {
            if let Some(delta) = event.delta {
                match delta.kind.as_deref() {
                    Some("text_delta") => {
                        if state.reasoning_part_active {
                            flush_buffer(
                                &mut state.reasoning,
                                &mut state.reasoning_emitted,
                                ChatStreamKind::Reasoning,
                                on_chunk,
                                true,
                            )?;
                            on_chunk(ChatStreamChunk {
                                kind: ChatStreamKind::ReasoningPartEnd,
                                text: String::new(),
                            })?;
                            state.reasoning_part_active = false;
                        }
                        if let Some(text) = delta.text {
                            push_buffered_chunk(
                                &mut state.content,
                                &mut state.content_emitted,
                                ChatStreamKind::Content,
                                text,
                                on_chunk,
                            )?;
                        }
                    }
                    Some("thinking_delta") => {
                        // 空 thinking delta 不开新分段(同 chat 路径的空 reasoning_content)。
                        if let Some(text) = delta.thinking.filter(|text| !text.is_empty()) {
                            if !state.reasoning_part_active {
                                if !state.reasoning.is_empty() && !state.reasoning.ends_with("\n\n")
                                {
                                    state.reasoning.push_str("\n\n");
                                }
                                on_chunk(ChatStreamChunk {
                                    kind: ChatStreamKind::ReasoningPartStart,
                                    text: String::new(),
                                })?;
                                state.reasoning_part_active = true;
                            }
                            push_buffered_chunk(
                                &mut state.reasoning,
                                &mut state.reasoning_emitted,
                                ChatStreamKind::Reasoning,
                                text,
                                on_chunk,
                            )?;
                        }
                    }
                    Some("input_json_delta") => {
                        if let (Some(index), Some(text)) = (event.index, delta.partial_json) {
                            state.tool_calls.append_arguments(index, text);
                        }
                    }
                    Some("signature_delta") => {
                        state.thinking_signature = delta.signature;
                    }
                    _ => {}
                }
            }
        }
        "content_block_stop" => {
            if state.reasoning_part_active {
                flush_buffer(
                    &mut state.reasoning,
                    &mut state.reasoning_emitted,
                    ChatStreamKind::Reasoning,
                    on_chunk,
                    true,
                )?;
                on_chunk(ChatStreamChunk {
                    kind: ChatStreamKind::ReasoningPartEnd,
                    text: String::new(),
                })?;
                state.reasoning_part_active = false;
            }
        }
        "message_delta" => {
            if let Some(usage) = event.usage {
                merge_anthropic_usage(&mut state.usage, usage);
            }
            if let Some(stop_reason) = event
                .delta
                .as_ref()
                .and_then(|delta| delta.stop_reason.clone())
            {
                state.stop_reason = Some(stop_reason);
            }
            flush_anthropic_state(state, on_chunk)?;
        }
        "message_stop" => {
            flush_anthropic_state(state, on_chunk)?;
            return Ok(true);
        }
        "error" => {
            let message = event
                .error
                .map(|error| match (error.kind, error.message) {
                    (Some(kind), Some(message)) => format!("{kind}: {message}"),
                    (Some(kind), None) => kind,
                    (None, Some(message)) => message,
                    (None, None) => "Anthropic Messages stream error".to_string(),
                })
                .unwrap_or_else(|| "Anthropic Messages stream error".to_string());
            bail!("{message}");
        }
        _ => {}
    }
    Ok(false)
}

pub(in crate::llm::openai_compatible) fn flush_anthropic_state<F>(
    state: &mut AnthropicStreamState,
    on_chunk: &mut F,
) -> Result<()>
where
    F: FnMut(ChatStreamChunk) -> Result<()>,
{
    flush_buffer(
        &state.reasoning,
        &mut state.reasoning_emitted,
        ChatStreamKind::Reasoning,
        on_chunk,
        true,
    )?;
    if state.reasoning_part_active {
        on_chunk(ChatStreamChunk {
            kind: ChatStreamKind::ReasoningPartEnd,
            text: String::new(),
        })?;
        state.reasoning_part_active = false;
    }
    flush_buffer(
        &state.content,
        &mut state.content_emitted,
        ChatStreamKind::Content,
        on_chunk,
        true,
    )
}

pub(in crate::llm::openai_compatible) fn merge_anthropic_usage(
    current: &mut Option<Usage>,
    usage: AnthropicUsage,
) {
    let previous = current.take().unwrap_or_default();
    let cache_read = usage
        .cache_read_input_tokens
        .unwrap_or(previous.cache_read_tokens);
    let cache_write = usage
        .cache_creation_input_tokens
        .unwrap_or(previous.cache_write_tokens);
    // Anthropic's `input_tokens` excludes both cache reads and cache writes;
    // normalize to the cross-provider invariant `prompt = uncached + read + write`
    // so context accounting does not collapse once cache_control is in play.
    let prompt_tokens = if usage.input_tokens > 0 || cache_read > 0 || cache_write > 0 {
        usage
            .input_tokens
            .saturating_add(cache_read)
            .saturating_add(cache_write)
    } else {
        previous.prompt_tokens
    };
    let completion_tokens = if usage.output_tokens > 0 {
        usage.output_tokens
    } else {
        previous.completion_tokens
    };
    *current = Some(Usage {
        prompt_tokens,
        completion_tokens,
        total_tokens: prompt_tokens.saturating_add(completion_tokens),
        cache_read_tokens: cache_read,
        cache_write_tokens: cache_write,
        cache_reported: previous.cache_reported
            || usage.cache_read_input_tokens.is_some()
            || usage.cache_creation_input_tokens.is_some(),
        reasoning_tokens: previous.reasoning_tokens,
        ..Usage::default()
    });
}
