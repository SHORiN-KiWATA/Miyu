//! SSE 流的解析与累积。
//!
//! 三条 `handle_*_sse_line` 各解析一套协议的事件，共同的难点是**工具调用是分片
//! 到达的**：函数名先来，参数一个字符一个字符地流。累积器（`*ToolAccumulator`）
//! 负责拼回完整调用，并在拼的过程中就能报出「模型要调这个工具了」。
//!
//! `SseDataBuffer` 处理字节层的坑：一个 UTF-8 字符可能被切在两个 TCP 包之间，
//! 直接 `from_utf8_lossy` 会把它变成替换字符——中文流式输出会随机出现「�」。
//!
//! `MAX_STREAM_*` 那组上限是防御性的：流的内容由对端决定，没有上限就等于把内
//! 存交给供应商管。

mod accumulator;
mod anthropic;
mod responses;
pub(in crate::llm::openai_compatible) use accumulator::*;
pub(in crate::llm::openai_compatible) use anthropic::*;
pub(in crate::llm::openai_compatible) use responses::*;

use crate::llm::openai_compatible::*;

pub(in crate::llm::openai_compatible) fn clean_response_content(
    content: String,
) -> (String, Option<String>) {
    split_tagged_reasoning(clean_plain_text(content))
}

pub(in crate::llm::openai_compatible) fn split_tagged_reasoning(
    content: String,
) -> (String, Option<String>) {
    match split_tag_pair(content, "think").or_else(|content| split_tag_pair(content, "thinking")) {
        Ok(result) => result,
        Err(content) => (content, None),
    }
}

pub(in crate::llm::openai_compatible) fn split_tag_pair(
    content: String,
    tag: &str,
) -> std::result::Result<(String, Option<String>), String> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let Some(start) = content.find(&open) else {
        return Err(content);
    };
    let reasoning_start = start + open.len();
    let Some(relative_end) = content[reasoning_start..].find(&close) else {
        return Ok((content, None));
    };
    let end = reasoning_start + relative_end;
    let reasoning = content[reasoning_start..end].trim().to_string();
    let mut visible = String::new();
    visible.push_str(content[..start].trim_end());
    visible.push_str(content[end + close.len()..].trim_start());
    Ok((
        visible.trim().to_string(),
        (!reasoning.is_empty()).then_some(reasoning),
    ))
}

pub(in crate::llm::openai_compatible) fn handle_sse_line<F>(
    line: &str,
    content: &mut String,
    content_emitted: &mut usize,
    reasoning: &mut String,
    reasoning_emitted: &mut usize,
    reasoning_part_active: &mut bool,
    finish_reason: &mut Option<String>,
    usage: &mut Option<Usage>,
    tool_calls: &mut ToolCallAccumulator,
    on_chunk: &mut F,
) -> Result<Option<bool>>
where
    F: FnMut(ChatStreamChunk) -> Result<()>,
{
    let Some(data) = line.strip_prefix("data:").map(str::trim) else {
        return Ok(None);
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
        tracing::debug!(
            finish_reason = finish_reason.as_deref(),
            content_chars = content.chars().count(),
            reasoning_chars = reasoning.chars().count(),
            tool_call_count = tool_calls.calls.len(),
            "{}",
            t(
                "Chat completions stream received DONE",
                "聊天补全流已收到 DONE"
            )
        );
        return Ok(Some(true));
    }
    let response: ChatStreamResponse = serde_json::from_str(data).with_context(|| {
        format!(
            "{}: {}",
            t(
                "invalid chat completions stream response",
                "无效的聊天流式响应",
            ),
            clean_plain_text(data.to_string())
        )
    })?;
    // An empty `error` is not one: some gateways send `{"error":""}` alongside
    // the terminal usage event, and failing the turn over it would turn a
    // normal completion into a spurious error.
    if let Some(error) = response.error.filter(|error| !is_empty_error(error)) {
        bail!(
            "{}: {}",
            t(
                "chat completions stream returned an error",
                "聊天流式响应返回错误"
            ),
            provider_error_text(&error)
        );
    }
    if let Some(next_usage) = response.usage {
        *usage = Some(next_usage);
    }
    for choice in response.choices {
        // An empty string is "absent", not an end signal: some gateways send
        // `"finish_reason": ""` on ordinary chunks.
        if let Some(next_finish_reason) = choice.finish_reason.filter(|reason| !reason.is_empty()) {
            tracing::debug!(
                finish_reason = %next_finish_reason,
                "{}",
                t(
                    "Chat completions stream finish reason received",
                    "已收到聊天补全流结束原因"
                )
            );
            *finish_reason = Some(next_finish_reason);
        }
        let delta = choice.delta;
        // 空串是「没有」,不是一段推理:DeepSeek 等网关会在每个正文 delta 上
        // 附带 `reasoning_content: ""`,若照收就会给每个正文分片包一对
        // ReasoningPartStart/End,终端把每个分片单独截成一段(09-10 截图)。
        if let Some(text) = delta_reasoning_text(&delta).filter(|text| !text.is_empty()) {
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
        if let Some(text) = delta.content {
            if !text.is_empty() {
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
                push_buffered_chunk(
                    content,
                    content_emitted,
                    ChatStreamKind::Content,
                    text,
                    on_chunk,
                )?;
            }
        }
        for tool_call in delta.tool_calls {
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
            if let Some(name) = tool_calls.push(tool_call) {
                on_chunk(ChatStreamChunk {
                    kind: ChatStreamKind::ToolCall,
                    text: name,
                })?;
            }
        }
    }
    Ok(Some(false))
}

pub(in crate::llm::openai_compatible) fn delta_reasoning_text(
    delta: &ChatChoiceMessage,
) -> Option<String> {
    delta
        .reasoning_content
        .clone()
        .or_else(|| delta.reasoning.clone())
        .or_else(|| delta.thinking.clone())
        .or_else(|| delta.thinking_content.clone())
        .or_else(|| delta.reasoning_text.clone())
        .or_else(|| reasoning_details_text(delta.reasoning_details.as_ref()))
}

pub(in crate::llm::openai_compatible) fn reasoning_details_text(
    value: Option<&serde_json::Value>,
) -> Option<String> {
    let value = value?;
    if let Some(text) = value.as_str() {
        return Some(text.to_string());
    }
    if let Some(array) = value.as_array() {
        let text = array
            .iter()
            .filter_map(|item| {
                item.get("text")
                    .or_else(|| item.get("content"))
                    .and_then(serde_json::Value::as_str)
            })
            .collect::<Vec<_>>()
            .join("");
        return (!text.is_empty()).then_some(text);
    }
    value
        .get("text")
        .or_else(|| value.get("content"))
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
}

pub(in crate::llm::openai_compatible) fn push_buffered_chunk<F>(
    target: &mut String,
    emitted: &mut usize,
    kind: ChatStreamKind,
    text: String,
    on_chunk: &mut F,
) -> Result<()>
where
    F: FnMut(ChatStreamChunk) -> Result<()>,
{
    if text.is_empty() {
        return Ok(());
    }
    target.push_str(&text);
    flush_buffer(target, emitted, kind, on_chunk, false)
}

pub(in crate::llm::openai_compatible) fn flush_buffer<F>(
    target: &str,
    emitted: &mut usize,
    kind: ChatStreamKind,
    on_chunk: &mut F,
    final_flush: bool,
) -> Result<()>
where
    F: FnMut(ChatStreamChunk) -> Result<()>,
{
    while *emitted < target.len() {
        let remaining = &target[*emitted..];
        if starts_hidden_prefix(remaining) {
            if let Some(end) = hidden_end_after(target, *emitted) {
                *emitted = end;
                continue;
            }
            if final_flush {
                *emitted = target.len();
            }
            return Ok(());
        }
        let hidden_start = hidden_start_after(target, *emitted);
        let mut safe_end = hidden_start.unwrap_or(target.len());
        if hidden_start.is_none() && !final_flush {
            safe_end =
                safe_end.saturating_sub(partial_hidden_suffix_len(&target[*emitted..safe_end]));
        }
        if safe_end <= *emitted {
            return Ok(());
        }
        let text = target[*emitted..safe_end].to_string();
        *emitted = safe_end;
        if !text.is_empty() {
            on_chunk(ChatStreamChunk { kind, text })?;
        }
    }
    Ok(())
}

pub(in crate::llm::openai_compatible) fn finalize_stream_result(
    content: String,
    reasoning: String,
    usage: Option<Usage>,
    tool_calls: Vec<ToolCall>,
    dsml_enabled: bool,
) -> Result<ChatResult> {
    let usage = usage.map(|mut usage| {
        usage.normalize_cache_fields();
        if usage.cache_reported {
            // v7 Release 1 observability: one absolute-value line per request,
            // à la Reasonix ("in N (M cached / K new)"). Percentages mislead
            // when a turn adds lots of fresh content, so none are shown.
            tracing::info!(
                prompt_tokens = usage.prompt_tokens,
                cache_read = usage.cache_read_tokens,
                cache_write = usage.cache_write_tokens,
                fresh = usage.uncached_prompt_tokens(),
                "prompt cache accounting"
            );
        }
        usage
    });
    let content = clean_plain_text(content);
    let (content, mut dsml_tool_calls) = if dsml_enabled {
        extract_dsml_tool_calls(content)
    } else {
        (content, Vec::new())
    };
    let content = if dsml_enabled {
        strip_orphaned_dsml_tags(content)
    } else {
        content
    };
    let reasoning = clean_plain_text(reasoning);
    let (reasoning, reasoning_dsml_tool_calls) = if dsml_enabled {
        extract_dsml_tool_calls(reasoning)
    } else {
        (reasoning, Vec::new())
    };
    let reasoning = if dsml_enabled {
        strip_orphaned_dsml_tags(reasoning)
    } else {
        reasoning
    };
    dsml_tool_calls.extend(reasoning_dsml_tool_calls);
    let (content, tag_reasoning) = clean_response_content(content);
    let reasoning = if reasoning.trim().is_empty() {
        tag_reasoning
    } else {
        Some(reasoning)
    };
    let tool_calls = if dsml_tool_calls.is_empty() {
        tool_calls
    } else {
        dsml_tool_calls
    };
    if content.trim().is_empty()
        && !reasoning
            .as_ref()
            .is_some_and(|text| !text.trim().is_empty())
        && tool_calls.is_empty()
    {
        bail!(
            "{}",
            t(
                "chat completions stream response was empty",
                "聊天流式响应为空",
            )
        );
    }
    Ok(ChatResult {
        content,
        reasoning: reasoning.filter(|text| !text.trim().is_empty()),
        usage,
        usage_estimated: false,
        tool_calls,
        provider_id: None,
        model: None,
        finish_reason: None,
        thinking_signature: None,
        last_request_usage: None,
        responses_continuation: None,
    })
}
