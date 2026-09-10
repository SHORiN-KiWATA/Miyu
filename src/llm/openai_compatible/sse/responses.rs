//! OpenAI Responses 协议的事件解析。
//!
//! 比 Chat 复杂在于它是**分项目的**：每个 output item 有自己的 id 和生命周期，
//! 推理摘要还可能分成多段。所以要按 item id 配对，不能靠顺序。

use crate::llm::openai_compatible::sse::*;
use crate::llm::openai_compatible::*;

pub(in crate::llm::openai_compatible) fn handle_responses_sse_line<F>(
    line: &str,
    content: &mut String,
    content_emitted: &mut usize,
    reasoning: &mut String,
    reasoning_emitted: &mut usize,
    reasoning_part_active: &mut bool,
    usage: &mut Option<Usage>,
    content_started: &mut bool,
    output_text_delta_parts: &mut HashSet<(String, usize)>,
    refusal_delta_parts: &mut HashSet<(String, usize)>,
    response_id: &mut Option<String>,
    tool_calls: &mut ResponsesToolAccumulator,
    on_chunk: &mut F,
) -> Result<bool>
where
    F: FnMut(ChatStreamChunk) -> Result<()>,
{
    let Some(data) = line.strip_prefix("data:").map(str::trim) else {
        return Ok(false);
    };
    if data == "[DONE]" {
        flush_buffer(
            reasoning,
            reasoning_emitted,
            ChatStreamKind::Reasoning,
            on_chunk,
            true,
        )?;
        if *reasoning_part_active {
            on_chunk(ChatStreamChunk {
                kind: ChatStreamKind::ReasoningPartEnd,
                text: String::new(),
            })?;
            *reasoning_part_active = false;
        }
        flush_buffer(
            content,
            content_emitted,
            ChatStreamKind::Content,
            on_chunk,
            true,
        )?;
        return Ok(true);
    }
    let event: ResponsesStreamEvent = serde_json::from_str(data).with_context(|| {
        format!(
            "{}: {}",
            t(
                "invalid responses stream event",
                "无效的 Responses 流式事件"
            ),
            clean_plain_text(data.to_string())
        )
    })?;
    if let Some(id) = event
        .response
        .as_ref()
        .and_then(|response| response.id.as_deref())
        .filter(|id| !id.trim().is_empty())
    {
        *response_id = Some(id.to_string());
    }
    if event.kind.starts_with("response.reasoning")
        || matches!(
            event.kind.as_str(),
            "response.output_item.added" | "response.completed" | "response.incomplete"
        )
    {
        let item_kind = event.item.as_ref().map(|item| item.kind.as_str());
        let delta_chars = event.delta.as_deref().map(|delta| delta.chars().count());
        let reasoning_tokens = event
            .response
            .as_ref()
            .and_then(|response| response.usage.as_ref())
            .and_then(|usage| usage.output_tokens_details.as_ref())
            .and_then(|details| details.reasoning_tokens);
        tracing::debug!(
            event_type = %event.kind,
            item_kind = ?item_kind,
            delta_chars = ?delta_chars,
            reasoning_tokens = ?reasoning_tokens,
            "{}",
            t("Responses stream milestone", "Responses 流关键节点")
        );
    }
    let content_part_key = (
        event.item_id.clone().unwrap_or_default(),
        event.content_index.unwrap_or_default(),
    );
    match event.kind.as_str() {
        "response.output_text.delta"
        | "response.output_text.done"
        | "response.refusal.delta"
        | "response.refusal.done" => {
            let text = match event.kind.as_str() {
                "response.output_text.delta" => {
                    let text = event.delta.unwrap_or_default();
                    if !text.is_empty() {
                        output_text_delta_parts.insert(content_part_key.clone());
                    }
                    text
                }
                "response.output_text.done"
                    if !output_text_delta_parts.contains(&content_part_key) =>
                {
                    event.text.unwrap_or_default()
                }
                "response.output_text.done" => String::new(),
                "response.refusal.delta" => {
                    let text = event.delta.unwrap_or_default();
                    if !text.is_empty() {
                        refusal_delta_parts.insert(content_part_key.clone());
                    }
                    text
                }
                "response.refusal.done" if !refusal_delta_parts.contains(&content_part_key) => {
                    event.refusal.unwrap_or_default()
                }
                "response.refusal.done" => String::new(),
                _ => String::new(),
            };
            if text.is_empty() {
                return Ok(false);
            }
            if *reasoning_part_active {
                flush_buffer(
                    reasoning,
                    reasoning_emitted,
                    ChatStreamKind::Reasoning,
                    on_chunk,
                    true,
                )?;
                on_chunk(ChatStreamChunk {
                    kind: ChatStreamKind::ReasoningPartEnd,
                    text: String::new(),
                })?;
                *reasoning_part_active = false;
            }
            *content_started = true;
            push_buffered_chunk(
                content,
                content_emitted,
                ChatStreamKind::Content,
                text,
                on_chunk,
            )?;
        }
        "response.reasoning_text.delta"
        | "response.reasoning_summary.delta"
        | "response.reasoning_summary_text.delta" => {
            // 空 delta 不开新分段(同 chat 路径的空 reasoning_content)。
            if let Some(text) = event.delta.filter(|text| !text.is_empty()) {
                if !*reasoning_part_active {
                    if !reasoning.is_empty() && !reasoning.ends_with("\n\n") {
                        reasoning.push_str("\n\n");
                    }
                    on_chunk(ChatStreamChunk {
                        kind: ChatStreamKind::ReasoningPartStart,
                        text: String::new(),
                    })?;
                    *reasoning_part_active = true;
                }
                push_buffered_chunk(
                    reasoning,
                    reasoning_emitted,
                    ChatStreamKind::Reasoning,
                    text,
                    on_chunk,
                )?;
            }
        }
        "response.reasoning_text.done"
        | "response.reasoning_summary.done"
        | "response.reasoning_summary_text.done" => {
            flush_buffer(
                reasoning,
                reasoning_emitted,
                ChatStreamKind::Reasoning,
                on_chunk,
                true,
            )?;
            if *reasoning_part_active {
                on_chunk(ChatStreamChunk {
                    kind: ChatStreamKind::ReasoningPartEnd,
                    text: String::new(),
                })?;
                *reasoning_part_active = false;
            }
            if !*content_started && !reasoning.trim().is_empty() {
                *content_started = true;
                on_chunk(ChatStreamChunk {
                    kind: ChatStreamKind::Content,
                    text: String::new(),
                })?;
            }
        }
        "response.output_item.added" => {
            if *reasoning_part_active {
                flush_buffer(
                    reasoning,
                    reasoning_emitted,
                    ChatStreamKind::Reasoning,
                    on_chunk,
                    true,
                )?;
                on_chunk(ChatStreamChunk {
                    kind: ChatStreamKind::ReasoningPartEnd,
                    text: String::new(),
                })?;
                *reasoning_part_active = false;
            }
            if let Some(item) = event.item {
                if let Some(name) = tool_calls.start(item) {
                    on_chunk(ChatStreamChunk {
                        kind: ChatStreamKind::ToolCall,
                        text: name,
                    })?;
                }
            }
        }
        "response.reasoning_summary_part.added" => {
            if *reasoning_part_active {
                flush_buffer(
                    reasoning,
                    reasoning_emitted,
                    ChatStreamKind::Reasoning,
                    on_chunk,
                    true,
                )?;
                on_chunk(ChatStreamChunk {
                    kind: ChatStreamKind::ReasoningPartEnd,
                    text: String::new(),
                })?;
            }
            if !reasoning.is_empty() && !reasoning.ends_with("\n\n") {
                reasoning.push_str("\n\n");
            }
            on_chunk(ChatStreamChunk {
                kind: ChatStreamKind::ReasoningPartStart,
                text: String::new(),
            })?;
            *reasoning_part_active = true;
        }
        "response.reasoning_summary_part.done" => {
            flush_buffer(
                reasoning,
                reasoning_emitted,
                ChatStreamKind::Reasoning,
                on_chunk,
                true,
            )?;
            if *reasoning_part_active {
                on_chunk(ChatStreamChunk {
                    kind: ChatStreamKind::ReasoningPartEnd,
                    text: String::new(),
                })?;
                *reasoning_part_active = false;
            }
        }
        "response.function_call_arguments.delta" => {
            if let Some(delta) = event.delta {
                tool_calls.append_arguments(event.item_id, delta);
            }
        }
        "response.function_call_arguments.done" => {
            tool_calls.finish_arguments(event.item_id, event.name, event.arguments);
        }
        "response.output_item.done" => {
            if let Some(item) = event.item {
                tool_calls.finish_item(item);
            }
        }
        "response.completed" => {
            if let Some(next_usage) = event.response.and_then(|response| response.usage) {
                let total_tokens = if next_usage.total_tokens > 0 {
                    next_usage.total_tokens
                } else {
                    next_usage
                        .input_tokens
                        .saturating_add(next_usage.output_tokens)
                };
                let input_details = next_usage.input_tokens_details.as_ref();
                let cache_read = input_details.and_then(|details| details.cached_tokens);
                let cache_write = input_details.and_then(|details| details.cache_write_tokens);
                let reasoning_tokens = next_usage
                    .output_tokens_details
                    .as_ref()
                    .and_then(|details| details.reasoning_tokens)
                    .unwrap_or(0);
                *usage = Some(Usage {
                    prompt_tokens: next_usage.input_tokens,
                    completion_tokens: next_usage.output_tokens,
                    total_tokens,
                    cache_read_tokens: cache_read.unwrap_or(0),
                    cache_write_tokens: cache_write.unwrap_or(0),
                    reasoning_tokens,
                    cache_reported: cache_read.is_some() || cache_write.is_some(),
                    ..Usage::default()
                });
            }
            flush_buffer(
                reasoning,
                reasoning_emitted,
                ChatStreamKind::Reasoning,
                on_chunk,
                true,
            )?;
            if *reasoning_part_active {
                on_chunk(ChatStreamChunk {
                    kind: ChatStreamKind::ReasoningPartEnd,
                    text: String::new(),
                })?;
                *reasoning_part_active = false;
            }
            flush_buffer(
                content,
                content_emitted,
                ChatStreamKind::Content,
                on_chunk,
                true,
            )?;
            return Ok(true);
        }
        "response.incomplete" => {
            let reason = event
                .response
                .as_ref()
                .and_then(|response| response.incomplete_details.as_ref())
                .and_then(|details| details.reason.as_deref())
                .unwrap_or("unknown");
            bail!("OpenAI Responses response was incomplete: {reason}");
        }
        "error" | "response.failed" => {
            bail!(
                "OpenAI Responses stream failed: {}",
                clean_plain_text(data.to_string())
            );
        }
        _ => {}
    }
    Ok(false)
}

pub(in crate::llm::openai_compatible) fn finalize_responses_stream_result(
    content: String,
    reasoning: String,
    usage: Option<Usage>,
    tool_calls: Vec<ToolCall>,
    dsml_enabled: bool,
    response_id: Option<String>,
    store_disabled: bool,
    continuation_unsupported: bool,
) -> Result<ChatResult> {
    let mut result = finalize_stream_result(content, reasoning, usage, tool_calls, dsml_enabled)?;
    if result.tool_calls.is_empty() {
        return Ok(result);
    }
    // 该端点续传已被记为不可用(能力记录/自愈置位):不设 continuation,
    // 工具轮走无状态全量回放(lower_responses_messages 重放
    // function_call/function_call_output 是完整配对的)。
    if continuation_unsupported {
        return Ok(result);
    }
    if store_disabled {
        bail!(
            "OpenAI Responses returned tool calls, but store=false prevents stateful continuation"
        );
    }
    let response_id = response_id
        .filter(|id| !id.trim().is_empty())
        .context("OpenAI Responses returned tool calls without a response ID")?;
    result.responses_continuation = Some(Box::new(ResponsesContinuation {
        response_id,
        endpoint_id: String::new(),
    }));
    Ok(result)
}
