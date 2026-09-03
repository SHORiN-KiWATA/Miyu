//! 推理流里的标题识别与过滤。
//!
//! 模型的推理段常以一个小标题开头（`**分析问题**`、`## 检查依赖`），前端要把
//! 它当标题显示而不是正文。难点在**流式**：标题可能被切在任意字符边界上，甚至
//! 中途才发现「这不是标题，是正文的粗体」。
//!
//! 所以过滤器是攒够才判、判不出就原样吐——`..._matches_unsplit_input_at_every_character_boundary`
//! 这条测试用每一个可能的切点跑一遍，守的就是切法不影响结果。

use crate::agent::*;

#[derive(Default)]
pub(in crate::agent) struct ReasoningTitleFilter {
    pub(in crate::agent) pending: String,
    pub(in crate::agent) decided: bool,
    pub(in crate::agent) trim_body_prefix: bool,
}

impl ReasoningTitleFilter {
    pub(in crate::agent) fn push(&mut self, text: &str) -> (Option<String>, Option<String>) {
        if self.decided {
            let text = if self.trim_body_prefix {
                let text = text.trim_start_matches(['\r', '\n']);
                if text.is_empty() {
                    return (None, None);
                }
                self.trim_body_prefix = false;
                text
            } else {
                text
            };
            return (None, (!text.is_empty()).then(|| text.to_string()));
        }
        self.pending.push_str(text);
        let trimmed = self.pending.trim_start();
        if "**".starts_with(trimmed) {
            return (None, None);
        }
        if let Some(body) = trimmed.strip_prefix("**") {
            let Some(close) = body.find("**") else {
                if trimmed.chars().count() <= 160 {
                    return (None, None);
                }
                return self.release_without_title();
            };
            let title = clean_reasoning_title(&body[..close]);
            let suffix = &body[close + 2..];
            if only_line_breaks(suffix) {
                return self.finish_decision(title, String::new());
            }
            if !suffix.starts_with("\n\n") && !suffix.starts_with("\r\n\r\n") {
                return self.release_without_title();
            }
            let rest = suffix.trim_start_matches(['\r', '\n']).to_string();
            return self.finish_decision(title, rest);
        }
        if possible_markdown_heading_prefix(trimmed) {
            return (None, None);
        }
        if let Some(title_start) = markdown_heading_content_start(trimmed) {
            let Some(end) = trimmed.find('\n') else {
                if trimmed.chars().count() <= 160 {
                    return (None, None);
                }
                return self.release_without_title();
            };
            let suffix = &trimmed[end + 1..];
            if only_line_breaks(suffix) {
                return (None, None);
            }
            let title = clean_reasoning_title(&trimmed[title_start..end]);
            let rest = suffix.trim_start_matches(['\r', '\n']).to_string();
            return self.finish_decision(title, rest);
        }
        self.release_without_title()
    }

    pub(in crate::agent) fn finish_decision(
        &mut self,
        title: String,
        rest: String,
    ) -> (Option<String>, Option<String>) {
        self.pending.clear();
        self.decided = true;
        self.trim_body_prefix = rest.is_empty();
        (
            (!title.is_empty()).then_some(title),
            (!rest.is_empty()).then_some(rest),
        )
    }

    pub(in crate::agent) fn release_without_title(&mut self) -> (Option<String>, Option<String>) {
        self.decided = true;
        (None, Some(std::mem::take(&mut self.pending)))
    }

    pub(in crate::agent) fn finish(&mut self) -> (Option<String>, Option<String>) {
        if self.pending.is_empty() {
            return (None, None);
        }
        self.decided = true;
        let pending = std::mem::take(&mut self.pending);
        let trimmed = pending.trim_start();
        if let Some(body) = trimmed.strip_prefix("**") {
            if let Some(close) = body.find("**") {
                let suffix = &body[close + 2..];
                if suffix.is_empty()
                    || ((suffix.starts_with("\n\n") || suffix.starts_with("\r\n\r\n"))
                        && only_line_breaks(suffix))
                {
                    let title = clean_reasoning_title(&body[..close]);
                    return ((!title.is_empty()).then_some(title), None);
                }
            }
        }
        if let Some(title_start) = markdown_heading_content_start(trimmed) {
            let title = clean_reasoning_title(&trimmed[title_start..]);
            return ((!title.is_empty()).then_some(title), None);
        }
        (None, Some(trimmed.to_string()))
    }
}

pub(in crate::agent) fn possible_markdown_heading_prefix(text: &str) -> bool {
    !text.is_empty() && text.len() <= 6 && text.bytes().all(|byte| byte == b'#')
}

pub(in crate::agent) fn only_line_breaks(text: &str) -> bool {
    text.bytes().all(|byte| matches!(byte, b'\r' | b'\n'))
}

pub(in crate::agent) fn markdown_heading_content_start(text: &str) -> Option<usize> {
    let hashes = text.bytes().take_while(|byte| *byte == b'#').count();
    if !(1..=6).contains(&hashes) {
        return None;
    }
    let rest = text.get(hashes..)?;
    let whitespace = rest
        .bytes()
        .take_while(|byte| matches!(*byte, b' ' | b'\t'))
        .count();
    (whitespace > 0).then_some(hashes + whitespace)
}

pub(in crate::agent) fn clean_reasoning_title(value: &str) -> String {
    let value = compact_one_line(value);
    let value = value.trim_matches(['*', '#', ' ', '\t', '.', '。', '!', '！', '?', '？']);
    truncate_chars(value, 80)
}

pub(in crate::agent) fn emit_filtered_chunk_at<F>(
    chunk: ChatStreamChunk,
    received_at: Instant,
    filter: &mut ReasoningTitleFilter,
    tool_calls_seen: &mut usize,
    on_event: &mut F,
) -> Result<()>
where
    F: FnMut(AgentEvent) -> Result<()>,
{
    match chunk.kind {
        ChatStreamKind::ReasoningPartStart => {
            *filter = ReasoningTitleFilter::default();
            on_event(AgentEvent::ReasoningPartStart { received_at })?;
        }
        ChatStreamKind::ReasoningReset => {
            *filter = ReasoningTitleFilter::default();
            on_event(AgentEvent::ReasoningReset { received_at })?;
        }
        ChatStreamKind::ReasoningPartEnd => {
            let (title, text) = filter.finish();
            if let Some(title) = title {
                on_event(AgentEvent::ReasoningTitle(title))?;
            }
            if let Some(text) = text {
                on_event(AgentEvent::Chunk(ChatStreamChunk {
                    kind: ChatStreamKind::Reasoning,
                    text,
                }))?;
            }
            on_event(AgentEvent::ReasoningPartEnd { received_at })?;
        }
        ChatStreamKind::ToolCall => {
            // The chunk carries only the tool name, emitted the moment it is
            // decoded — the arguments are still streaming behind it. That is
            // exactly the window a long patch or file write spends looking
            // frozen, so the hint goes up here rather than at ToolCall.
            *tool_calls_seen += 1;
            // 第二个调用起就是批量:每个工具单看都不够慢,但参数是接连流
            // 完的,合起来的静默窗口和一次大 patch 一样长。
            let batch = *tool_calls_seen > 1;
            if batch || crate::tools::preparing_phase(&chunk.text).is_some() {
                on_event(AgentEvent::ToolPreparing {
                    name: chunk.text.clone(),
                    batch,
                })?;
            }
            on_event(AgentEvent::Chunk(chunk))?;
        }
        // 中转侧闭环执行的工具活动:翻成标准卡片事件。执行不在 Miyu 的
        // 回合循环里,started/finished 都由流侧给,不产生本地执行。
        ChatStreamKind::RemoteToolStarted => {
            if let Ok(value) = serde_json::from_str::<serde_json::Value>(&chunk.text) {
                let text_of = |key: &str| {
                    value
                        .get(key)
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or_default()
                        .to_string()
                };
                on_event(AgentEvent::ToolCall {
                    call_id: text_of("id"),
                    name: text_of("name"),
                    arguments: value
                        .get("input")
                        .map(|input| input.to_string())
                        .unwrap_or_default(),
                })?;
            }
        }
        ChatStreamKind::RemoteToolFinished => {
            if let Ok(value) = serde_json::from_str::<serde_json::Value>(&chunk.text) {
                let text_of = |key: &str| {
                    value
                        .get(key)
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or_default()
                        .to_string()
                };
                let name = text_of("name");
                let output = text_of("output");
                // 中转侧的命令家族(claude 的 Bash / agy 的 run_command)输出走
                // 命令输出块(与本地 run_command 同一渲染路),否则 REPL 摘要
                // 只有一行 ok,命令打了什么全看不见。
                if crate::render::is_command_tool(&name) && !output.is_empty() {
                    on_event(AgentEvent::CommandOutput {
                        call_id: text_of("id"),
                        name: name.clone(),
                        stream: crate::tools::CommandOutputStream::Stdout,
                        chunk: output.clone().into_bytes(),
                    })?;
                }
                on_event(AgentEvent::ToolResult {
                    call_id: text_of("id"),
                    name,
                    ok: value
                        .get("ok")
                        .and_then(serde_json::Value::as_bool)
                        .unwrap_or(true),
                    output,
                })?;
            }
        }
        ChatStreamKind::Reasoning => {
            let (title, text) = filter.push(&chunk.text);
            if let Some(title) = title {
                on_event(AgentEvent::ReasoningTitle(title))?;
            }
            if let Some(text) = text {
                on_event(AgentEvent::Chunk(ChatStreamChunk {
                    kind: ChatStreamKind::Reasoning,
                    text,
                }))?;
            }
        }
        _ => on_event(AgentEvent::Chunk(chunk))?,
    }
    Ok(())
}

pub(in crate::agent) fn emit_model_chunk_at<F>(
    chunk: ChatStreamChunk,
    received_at: Instant,
    filter: &mut ReasoningTitleFilter,
    tool_calls_seen: &mut usize,
    on_event: &mut F,
) -> Result<()>
where
    F: FnMut(AgentEvent) -> Result<()>,
{
    if chunk.kind == ChatStreamKind::Reasoning {
        on_event(AgentEvent::RawReasoning(chunk.clone()))?;
    }
    emit_filtered_chunk_at(chunk, received_at, filter, tool_calls_seen, on_event)
}

/// 测试助手：不关心批量提示的用例用这个，计数器每次从零开始。
#[cfg(test)]
pub(in crate::agent) fn emit_filtered_chunk<F>(
    chunk: ChatStreamChunk,
    filter: &mut ReasoningTitleFilter,
    on_event: &mut F,
) -> Result<()>
where
    F: FnMut(AgentEvent) -> Result<()>,
{
    emit_filtered_chunk_at(chunk, Instant::now(), filter, &mut 0, on_event)
}

#[cfg(test)]
pub(in crate::agent) fn parse_reasoning_title(reasoning: &str) -> (Option<String>, String) {
    parse_reasoning_title_chunks([reasoning])
}

#[cfg(test)]
pub(in crate::agent) fn parse_reasoning_title_chunks<'a>(
    chunks: impl IntoIterator<Item = &'a str>,
) -> (Option<String>, String) {
    let mut filter = ReasoningTitleFilter::default();
    let mut title = None;
    let mut output = String::new();
    for chunk in chunks {
        let (chunk_title, text) = filter.push(chunk);
        title = title.or(chunk_title);
        if let Some(text) = text {
            output.push_str(&text);
        }
    }
    let (finished_title, pending) = filter.finish();
    let title = title.or(finished_title);
    if let Some(pending) = pending {
        output.push_str(&pending);
    }
    (title, output)
}
