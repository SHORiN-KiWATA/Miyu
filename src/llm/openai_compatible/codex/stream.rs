//! codex 子进程的 `--json` JSONL 事件泵。
//!
//! 事件(exec_events.rs 口径):`thread.started{thread_id}` / `turn.started` /
//! `item.started|updated|completed{item}` / `turn.completed{usage}` /
//! `turn.failed{error}` / `error{message}`。item 按 `type` 分:agent_message
//! (整段正文,不流式)、reasoning(思考摘要)、command_execution、file_change、
//! mcp_tool_call、web_search、todo_list、error。与另两条线的差异:正文没有增量
//! 事件,一条 agent_message 一次到齐;思考摘要有,进思考通道。

use crate::llm::openai_compatible::cli_relay::{
    hidden_remote_tool, process::RelayProcess, shape_remote_output, RelayOutcome,
};
use crate::llm::openai_compatible::codex::CodexRuntime;
use crate::llm::openai_compatible::*;

/// `resume` 目标在 codex 侧已不存在的签名(先于鉴权就报,实测措辞)。
pub(super) fn resume_lost(error: &anyhow::Error) -> bool {
    format!("{error:#}")
        .to_ascii_lowercase()
        .contains("no rollout found")
}

fn classify_codex_failure(text: &str) -> Option<HttpStatusFailure> {
    let lower = text.to_ascii_lowercase();
    const RATE_LIMIT: &[&str] = &[
        "429",
        "rate limit",
        "too many requests",
        "usage limit",
        "quota",
        "insufficient_quota",
    ];
    const AUTH: &[&str] = &[
        "401",
        "unauthorized",
        "not logged in",
        "please log in",
        "codex login",
        "authentication",
    ];
    if RATE_LIMIT.iter().any(|needle| lower.contains(needle)) {
        return Some(HttpStatusFailure {
            status: 429,
            kind: HttpFailureKind::RateLimit,
        });
    }
    if AUTH.iter().any(|needle| lower.contains(needle)) {
        return Some(HttpStatusFailure {
            status: 401,
            kind: HttpFailureKind::Authentication,
        });
    }
    None
}

#[derive(Default)]
struct StreamState {
    content: String,
    content_emitted: usize,
    reasoning: String,
    reasoning_emitted: usize,
    /// 已发过 started 的工具 item id → 展示名。
    started_tools: HashMap<String, String>,
    /// error 事件/error item 的正文,失败时并进报错。
    error_text: String,
    usage: Option<Usage>,
    failed: Option<String>,
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn run_codex_turn<F>(
    runtime: &CodexRuntime,
    workdir: &std::path::Path,
    args: &[String],
    stdin_payload: &str,
    expected_thread: Option<&str>,
    request_id: &str,
    on_chunk: &mut F,
) -> Result<RelayOutcome>
where
    F: FnMut(ChatStreamChunk) -> Result<()>,
{
    let mut process = RelayProcess::spawn(
        &runtime.binary,
        args,
        workdir,
        &[],
        stdin_payload,
        runtime.idle_timeout,
        "codex.stream",
        "codex",
        || {
            t(
                "Codex CLI not found; install it or set plugins.codex.binary",
                "找不到 Codex CLI;请安装它或配置 plugins.codex.binary",
            )
            .to_string()
        },
    )
    .await?;

    let mut state = StreamState::default();
    let mut thread_id: Option<String> = None;
    let mut completed = false;
    while let Some(line) = process.next_line().await? {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let value: Value = match serde_json::from_str(trimmed) {
            Ok(value) => value,
            Err(_) => {
                // codex 在 JSONL 之外也会打人话(警告/提示),留作报错线索。
                tracing::debug!(
                    request_id,
                    line = trimmed,
                    "codex emitted a non-JSON stdout line"
                );
                state.error_text.push_str(trimmed);
                state.error_text.push('\n');
                continue;
            }
        };
        match value.get("type").and_then(Value::as_str) {
            Some("thread.started") => {
                let actual = value
                    .get("thread_id")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                if let Some(expected) = expected_thread {
                    if actual != expected {
                        process.kill();
                        bail!(
                            "codex did not resume thread {expected} (it started {actual} instead); no rollout found"
                        );
                    }
                }
                thread_id = Some(actual);
            }
            Some("item.started") | Some("item.updated") | Some("item.completed") => {
                let completed_item =
                    value.get("type").and_then(Value::as_str) == Some("item.completed");
                if let Some(item) = value.get("item") {
                    handle_item(item, completed_item, &mut state, on_chunk)?;
                }
            }
            Some("turn.completed") => {
                state.usage = value.get("usage").and_then(usage_from_codex);
                completed = true;
                break;
            }
            Some("turn.failed") => {
                let message = value
                    .pointer("/error/message")
                    .and_then(Value::as_str)
                    .unwrap_or("turn failed")
                    .to_string();
                state.failed = Some(message);
                break;
            }
            Some("error") => {
                if let Some(message) = value.get("message").and_then(Value::as_str) {
                    state.error_text.push_str(message);
                    state.error_text.push('\n');
                }
            }
            _ => {}
        }
    }
    let (exit_code, stderr_text) = process.finish().await;

    if let Some(message) = state.failed {
        let mut error = anyhow::anyhow!("codex turn failed: {}", message.trim());
        if let Some(failure) = classify_codex_failure(&format!("{message}\n{}", state.error_text)) {
            error = error.context(failure);
        }
        return Err(error);
    }
    if !completed {
        let detail = format!("{}\n{}", state.error_text.trim(), stderr_text.trim());
        let mut error = anyhow::anyhow!(
            "codex exited (code {exit_code}) without turn.completed: {}",
            detail.trim()
        );
        if let Some(failure) = classify_codex_failure(&detail) {
            error = error.context(failure);
        }
        return Err(error);
    }

    flush_buffer(
        &state.reasoning,
        &mut state.reasoning_emitted,
        ChatStreamKind::Reasoning,
        &mut *on_chunk,
        true,
    )?;
    flush_buffer(
        &state.content,
        &mut state.content_emitted,
        ChatStreamKind::Content,
        &mut *on_chunk,
        true,
    )?;
    if state.content.trim().is_empty()
        && state
            .usage
            .as_ref()
            .map(|usage| usage.effective_total_tokens() == 0)
            .unwrap_or(true)
    {
        bail!(
            "codex produced no output (turn completed with zero usage): {} {}",
            state.error_text.trim(),
            stderr_text.trim()
        );
    }
    let usage = state.usage.clone();
    let mut result = finalize_stream_result(
        state.content,
        state.reasoning,
        usage.clone(),
        Vec::new(),
        false,
    )?;
    result.finish_reason = Some("stop".to_string());
    // codex 只给整轮用量,单次调用口径取不到;上下文表读同一份。
    result.last_request_usage = usage;
    Ok(RelayOutcome {
        result,
        session_id: thread_id,
    })
}

fn handle_item<F>(
    item: &Value,
    completed: bool,
    state: &mut StreamState,
    on_chunk: &mut F,
) -> Result<()>
where
    F: FnMut(ChatStreamChunk) -> Result<()>,
{
    let id = item
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    match item.get("type").and_then(Value::as_str) {
        Some("agent_message") if completed => {
            let text = item.get("text").and_then(Value::as_str).unwrap_or_default();
            if !state.content.is_empty() && !state.content.ends_with("\n\n") {
                push_buffered_chunk(
                    &mut state.content,
                    &mut state.content_emitted,
                    ChatStreamKind::Content,
                    "\n\n".to_string(),
                    on_chunk,
                )?;
            }
            push_buffered_chunk(
                &mut state.content,
                &mut state.content_emitted,
                ChatStreamKind::Content,
                text.to_string(),
                on_chunk,
            )?;
        }
        Some("reasoning") if completed => {
            let text = item.get("text").and_then(Value::as_str).unwrap_or_default();
            if text.trim().is_empty() {
                return Ok(());
            }
            if !state.reasoning.is_empty() && !state.reasoning.ends_with("\n\n") {
                state.reasoning.push_str("\n\n");
            }
            on_chunk(ChatStreamChunk {
                kind: ChatStreamKind::ReasoningPartStart,
                text: String::new(),
            })?;
            push_buffered_chunk(
                &mut state.reasoning,
                &mut state.reasoning_emitted,
                ChatStreamKind::Reasoning,
                text.to_string(),
                on_chunk,
            )?;
            flush_buffer(
                &state.reasoning,
                &mut state.reasoning_emitted,
                ChatStreamKind::Reasoning,
                &mut *on_chunk,
                true,
            )?;
            on_chunk(ChatStreamChunk {
                kind: ChatStreamKind::ReasoningPartEnd,
                text: String::new(),
            })?;
        }
        Some("command_execution") => {
            let command = item
                .get("command")
                .and_then(Value::as_str)
                .unwrap_or_default();
            emit_started(
                state,
                &id,
                "run_command",
                json!({ "command": command }),
                on_chunk,
            )?;
            if completed {
                let output = item
                    .get("aggregated_output")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let exit = item.get("exit_code").and_then(Value::as_i64);
                let ok = item.get("status").and_then(Value::as_str) == Some("completed")
                    && exit.unwrap_or(0) == 0;
                emit_finished(state, &id, ok, output, on_chunk)?;
            }
        }
        Some("mcp_tool_call") => {
            let server = item
                .get("server")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let tool = item.get("tool").and_then(Value::as_str).unwrap_or("tool");
            let name = if server == "miyu" || server.is_empty() {
                tool.to_string()
            } else {
                format!("{server}:{tool}")
            };
            if hidden_remote_tool(&name) {
                return Ok(());
            }
            let input = item.get("arguments").cloned().unwrap_or(json!({}));
            emit_started(state, &id, &name, input, on_chunk)?;
            if completed {
                let error = item.pointer("/error/message").and_then(Value::as_str);
                let ok = error.is_none()
                    && item.get("status").and_then(Value::as_str) == Some("completed");
                let output = match error {
                    Some(message) => message.to_string(),
                    None => item
                        .pointer("/result/content")
                        .and_then(Value::as_array)
                        .map(|parts| {
                            parts
                                .iter()
                                .filter_map(|part| part.get("text").and_then(Value::as_str))
                                .collect::<Vec<_>>()
                                .join("\n")
                        })
                        .unwrap_or_default(),
                };
                emit_finished(state, &id, ok, &output, on_chunk)?;
            }
        }
        Some("file_change") if completed => {
            let changes = item
                .get("changes")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            let paths: Vec<String> = changes
                .iter()
                .filter_map(|change| change.get("path").and_then(Value::as_str))
                .map(str::to_string)
                .collect();
            let input =
                json!({ "path": paths.first().cloned().unwrap_or_default(), "paths": paths });
            emit_started(state, &id, "edit", input, on_chunk)?;
            let ok = item.get("status").and_then(Value::as_str) == Some("completed");
            let summary = changes
                .iter()
                .map(|change| {
                    format!(
                        "{} {}",
                        change.get("kind").and_then(Value::as_str).unwrap_or("?"),
                        change.get("path").and_then(Value::as_str).unwrap_or("?")
                    )
                })
                .collect::<Vec<_>>()
                .join("\n");
            emit_finished(state, &id, ok, &summary, on_chunk)?;
        }
        Some("web_search") => {
            let query = item
                .get("query")
                .and_then(Value::as_str)
                .unwrap_or_default();
            emit_started(
                state,
                &id,
                "web_search",
                json!({ "query": query }),
                on_chunk,
            )?;
            if completed {
                emit_finished(state, &id, true, "", on_chunk)?;
            }
        }
        Some("error") => {
            if let Some(message) = item.get("message").and_then(Value::as_str) {
                state.error_text.push_str(message);
                state.error_text.push('\n');
            }
        }
        _ => {}
    }
    Ok(())
}

fn emit_started<F>(
    state: &mut StreamState,
    id: &str,
    name: &str,
    input: Value,
    on_chunk: &mut F,
) -> Result<()>
where
    F: FnMut(ChatStreamChunk) -> Result<()>,
{
    if state.started_tools.contains_key(id) {
        return Ok(());
    }
    state.started_tools.insert(id.to_string(), name.to_string());
    on_chunk(ChatStreamChunk {
        kind: ChatStreamKind::RemoteToolStarted,
        text: json!({ "id": id, "name": name, "input": input }).to_string(),
    })
}

fn emit_finished<F>(
    state: &mut StreamState,
    id: &str,
    ok: bool,
    output: &str,
    on_chunk: &mut F,
) -> Result<()>
where
    F: FnMut(ChatStreamChunk) -> Result<()>,
{
    let Some(name) = state.started_tools.get(id).cloned() else {
        return Ok(());
    };
    on_chunk(ChatStreamChunk {
        kind: ChatStreamKind::RemoteToolFinished,
        text: json!({
            "id": id,
            "name": name,
            "ok": ok,
            "output": shape_remote_output(&name, output),
        })
        .to_string(),
    })
}

/// turn.completed 的 usage(Responses 口径):input 含缓存命中,cached 是子集。
fn usage_from_codex(value: &Value) -> Option<Usage> {
    let input = value.get("input_tokens").and_then(Value::as_u64)?;
    let cached = value
        .get("cached_input_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0)
        .min(input);
    let write = value
        .get("cache_write_input_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let output = value
        .get("output_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let reasoning = value
        .get("reasoning_output_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    Some(Usage {
        prompt_tokens: input,
        completion_tokens: output,
        total_tokens: input.saturating_add(output),
        cache_read_tokens: cached,
        cache_write_tokens: write,
        reasoning_tokens: reasoning,
        cache_reported: true,
        ..Usage::default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usage_maps_responses_fields() {
        let usage = usage_from_codex(&json!({
            "input_tokens": 24763, "cached_input_tokens": 24448, "output_tokens": 122, "reasoning_output_tokens": 7
        }))
        .unwrap();
        assert_eq!(usage.prompt_tokens, 24763);
        assert_eq!(usage.cache_read_tokens, 24448);
        assert_eq!(usage.completion_tokens, 122);
        assert_eq!(usage.reasoning_tokens, 7);
        assert!(usage.cache_reported);
    }

    #[test]
    fn failure_classification() {
        assert_eq!(
            classify_codex_failure("unexpected status 401 Unauthorized").map(|f| f.status),
            Some(401)
        );
        assert_eq!(
            classify_codex_failure("Rate limit reached").map(|f| f.status),
            Some(429)
        );
        assert!(classify_codex_failure("something else").is_none());
        assert!(resume_lost(&anyhow::anyhow!(
            "thread/resume failed: no rollout found for thread id x"
        )));
    }
}
