//! claude 子进程的生命周期与 stream-json 事件泵。
//!
//! stdout 每行一个 JSON 事件;`stream_event` 里包的就是原生 Anthropic SSE
//! 事件,直接复用 [`AnthropicStreamEvent`] 与缓冲发射件。与 HTTP 线的两个关
//! 键差异:①一个 claude 回合可能含多次模型调用(MCP 工具循环),content 跨
//! 消息累积,权威用量以最终 `result` 帧为准;②tool_use 不回吐给 Miyu 执行
//! (桥在 claude 侧闭环),只作为思考通道的一行进度展示。
//!
//! 超时/出错路径必须显式杀进程组:drop future 只是弃 promise,不杀子进程。

use crate::llm::openai_compatible::claude_code::ClaudeCodeRuntime;
use crate::llm::openai_compatible::*;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

pub(super) struct TurnOutcome {
    pub(super) result: ChatResult,
    pub(super) claude_session: Option<String>,
}

/// 中转侧工具活动里**不**翻成卡片事件的工具。桥问答(`ask_question`)有自己
/// 的事件流(question.requested/answered,由 bridge_question 直发 EventHub),
/// 再发一份 tool.started 只会捣乱:CLI 的工具步 ACTIVE 与桥的 question.requested
/// 并发到达,ACTIVE 晚到时终端的「准备问题」黏性态在面板关掉之后才被置上,
/// 此后每个工具前都挂着「准备问题」(09-03 用户实录)。
pub(in crate::llm::openai_compatible) fn hidden_remote_tool(name: &str) -> bool {
    name == "ask_question"
}

/// `--resume` 目标在 claude 侧已不存在(过期/被清理)的签名。
pub(super) fn resume_session_lost(error: &anyhow::Error) -> bool {
    format!("{error:#}")
        .to_ascii_lowercase()
        .contains("no conversation found")
}

pub(in crate::llm::openai_compatible) fn kill_process_group(pid: u32) {
    unsafe {
        libc::kill(-(pid as i32), libc::SIGKILL);
    }
}

/// 把订阅侧的失败翻译成端点调度认识的分类:限流给 429(600s 冷却,池里有
/// 别的供应商就转移过去),登录失效给 401。措辞是嫌疑人,只做窄匹配。
fn classify_claude_failure(text: &str) -> Option<HttpStatusFailure> {
    let lower = text.to_ascii_lowercase();
    const RATE_LIMIT: &[&str] = &[
        "usage limit",
        "rate limit",
        "limit reached",
        "out of extra usage",
        "hit your limit",
    ];
    const AUTH: &[&str] = &[
        "please run /login",
        "not logged in",
        "authentication",
        "oauth token",
        "invalid api key",
        "credit balance",
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

pub(super) async fn run_claude_turn<F>(
    runtime: &ClaudeCodeRuntime,
    workdir: &std::path::Path,
    args: &[String],
    stdin_payload: &str,
    request_id: &str,
    on_chunk: &mut F,
) -> Result<TurnOutcome>
where
    F: FnMut(ChatStreamChunk) -> Result<()>,
{
    let mut command = tokio::process::Command::new(&runtime.binary);
    command
        .args(args)
        .current_dir(workdir)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .process_group(0)
        .kill_on_drop(true);
    // MCP 工具调用的客户端超时:ask_question 这类交互工具要等人回答,
    // claude 默认的 MCP 超时等不起,放宽到 30 分钟。
    command.env("MCP_TOOL_TIMEOUT", "1800000");
    if runtime.prefer_subscription {
        // 环境里的按量 API key 会抢走订阅登录态,中转的意义就没了。
        command.env_remove("ANTHROPIC_API_KEY");
        command.env_remove("ANTHROPIC_AUTH_TOKEN");
    }
    let mut child = command.spawn().map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            anyhow::anyhow!(
                "{}: {}",
                t(
                    "Claude Code CLI not found; install it or set plugins.claude_code.binary",
                    "找不到 Claude Code CLI;请安装它或配置 plugins.claude_code.binary"
                ),
                runtime.binary.display()
            )
        } else {
            anyhow::Error::from(error).context("failed to spawn the Claude Code CLI")
        }
    })?;
    let pid = child.id().unwrap_or_default();
    let mut stdin = child
        .stdin
        .take()
        .context("claude-code stdin unavailable")?;
    let stdout = child
        .stdout
        .take()
        .context("claude-code stdout unavailable")?;
    let stderr = child
        .stderr
        .take()
        .context("claude-code stderr unavailable")?;
    // stderr 尾巴单独收,失败时并进报错(限流/登录错误常常只在 stderr)。
    let stderr_tail = Arc::new(Mutex::new(String::new()));
    let stderr_task = {
        let tail = stderr_tail.clone();
        tokio::spawn(async move {
            let mut lines = tokio::io::BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                if let Ok(mut tail) = tail.lock() {
                    tail.push_str(&line);
                    tail.push('\n');
                    let overflow = tail.len().saturating_sub(8192);
                    if overflow > 0 {
                        let cut = tail
                            .char_indices()
                            .map(|(index, _)| index)
                            .find(|index| *index >= overflow)
                            .unwrap_or(0);
                        tail.drain(..cut);
                    }
                }
            }
        })
    };
    stdin
        .write_all(stdin_payload.as_bytes())
        .await
        .context("failed to write the claude-code stdin payload")?;
    drop(stdin);

    let mut lines = tokio::io::BufReader::new(stdout).lines();
    let mut state = AnthropicStreamState::default();
    // claude 侧工具调用 id → 名称,tool_result 帧只带 id,收口事件靠它配名。
    let mut remote_tools: HashMap<String, String> = HashMap::new();
    let mut session_id: Option<String> = None;
    let mut final_frame: Option<Value> = None;
    loop {
        let line = match tokio::time::timeout(runtime.idle_timeout, lines.next_line()).await {
            Err(_) => {
                kill_process_group(pid);
                return Err(anyhow::anyhow!(
                    "claude-code produced no output for {}s; the process was killed",
                    runtime.idle_timeout.as_secs()
                )
                .context(TransportFailure {
                    stage: "claude-code.stream",
                    kind: TransportFailureKind::Timeout,
                }));
            }
            Ok(read) => read.context("failed to read claude-code stdout")?,
        };
        let Some(line) = line else {
            break;
        };
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let value: Value = match serde_json::from_str(trimmed) {
            Ok(value) => value,
            Err(error) => {
                tracing::warn!(request_id, %error, "claude-code emitted a non-JSON stdout line");
                continue;
            }
        };
        match value.get("type").and_then(Value::as_str) {
            Some("system") => {
                if value.get("subtype").and_then(Value::as_str) == Some("init") {
                    if let Some(id) = value.get("session_id").and_then(Value::as_str) {
                        session_id = Some(id.to_string());
                    }
                }
            }
            // 完整 assistant 帧带 tool_use 的完整入参 → 标准 tool.started
            // 卡片(claude 侧闭环执行,started/finished 都由流侧翻译)。
            Some("assistant") => {
                emit_remote_tool_started(&value, &mut remote_tools, on_chunk)?;
            }
            // user 帧是 CLI 回填的工具结果 → tool.finished 收口。
            Some("user") => {
                emit_remote_tool_finished(&value, &remote_tools, on_chunk)?;
            }
            Some("stream_event") => {
                if let Some(event) = value.get("event") {
                    match serde_json::from_value::<AnthropicStreamEvent>(event.clone()) {
                        Ok(event) => handle_claude_stream_event(event, &mut state, on_chunk)?,
                        Err(error) => tracing::debug!(
                            request_id,
                            %error,
                            "claude-code stream event did not parse; skipped"
                        ),
                    }
                }
            }
            Some("result") => {
                final_frame = Some(value);
                break;
            }
            _ => {}
        }
    }
    // 结果帧到手后进程应当自然退出;不给它耗着的机会。
    let exit = match tokio::time::timeout(Duration::from_secs(10), child.wait()).await {
        Ok(status) => status.ok(),
        Err(_) => {
            kill_process_group(pid);
            None
        }
    };
    stderr_task.abort();
    let stderr_text = stderr_tail
        .lock()
        .map(|tail| tail.clone())
        .unwrap_or_default();

    let Some(final_frame) = final_frame else {
        kill_process_group(pid);
        let code = exit
            .and_then(|status| status.code())
            .map(|code| code.to_string())
            .unwrap_or_else(|| "unknown".to_string());
        let mut error = anyhow::anyhow!(
            "claude-code exited (code {code}) without a result frame: {}",
            stderr_text.trim()
        );
        if let Some(failure) = classify_claude_failure(&stderr_text) {
            error = error.context(failure);
        }
        return Err(error);
    };
    if let Some(id) = final_frame.get("session_id").and_then(Value::as_str) {
        session_id = Some(id.to_string());
    }
    let result_text = final_frame
        .get("result")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let subtype = final_frame
        .get("subtype")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let is_error = final_frame
        .get("is_error")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if is_error || subtype.starts_with("error") {
        let detail = if result_text.trim().is_empty() {
            stderr_text.trim().to_string()
        } else {
            result_text.clone()
        };
        let mut error = anyhow::anyhow!("claude-code turn failed ({subtype}): {}", detail.trim());
        if let Some(failure) = classify_claude_failure(&format!("{detail}\n{stderr_text}")) {
            error = error.context(failure);
        }
        return Err(error);
    }

    // 收尾:清空缓冲并闭合思考段(镜像 consume_anthropic_stream 的结尾)。
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
    if state.reasoning_part_active {
        on_chunk(ChatStreamChunk {
            kind: ChatStreamKind::ReasoningPartEnd,
            text: String::new(),
        })?;
        state.reasoning_part_active = false;
    }
    let mut content = state.content;
    if content.trim().is_empty() {
        // 极端情形(部分事件缺失)以结果帧的最终文本兜底,并补发给调用方。
        if !result_text.trim().is_empty() {
            on_chunk(ChatStreamChunk {
                kind: ChatStreamKind::Content,
                text: result_text.clone(),
            })?;
        }
        content = result_text;
    }
    // 结果帧的 usage 是整轮累计(多次工具迭代求和),记 Σ 用它;上下文表
    // 读数要的是"最后一次模型调用的真实占用",那在流内最后一个
    // message_start/delta 里(state.usage 恰好保存的就是最新一次)。
    let per_request_usage = state.usage.take().map(|mut usage| {
        usage.normalize_cache_fields();
        usage
    });
    let usage = usage_from_result_frame(&final_frame).or_else(|| per_request_usage.clone());
    let stop_reason = final_frame
        .get("stop_reason")
        .and_then(Value::as_str)
        .map(str::to_string)
        .or(state.stop_reason.take());
    let mut result = finalize_stream_result(content, state.reasoning, usage, Vec::new(), false)?;
    result.finish_reason = map_anthropic_stop_reason(stop_reason);
    result.last_request_usage = per_request_usage;
    Ok(TurnOutcome {
        result,
        claude_session: session_id,
    })
}

/// 折叠空白并按字符截断,给思考通道的一行摘要用。
pub(in crate::llm::openai_compatible) fn compact_line(text: &str, limit: usize) -> String {
    let mut collapsed = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.chars().count() > limit {
        collapsed = collapsed.chars().take(limit).collect::<String>() + "…";
    }
    collapsed
}

/// 按字符截断但保留换行结构:Bash 的输出走命令输出块渲染,折叠空白会把
/// 多行日志压成一行。
pub(in crate::llm::openai_compatible) fn truncate_block(text: &str, limit: usize) -> String {
    let trimmed = text.trim_end();
    if trimmed.chars().count() > limit {
        trimmed.chars().take(limit).collect::<String>() + "…"
    } else {
        trimmed.to_string()
    }
}

fn emit_remote_tool_started<F>(
    frame: &Value,
    remote_tools: &mut HashMap<String, String>,
    on_chunk: &mut F,
) -> Result<()>
where
    F: FnMut(ChatStreamChunk) -> Result<()>,
{
    let Some(blocks) = frame.pointer("/message/content").and_then(Value::as_array) else {
        return Ok(());
    };
    for block in blocks {
        if block.get("type").and_then(Value::as_str) != Some("tool_use") {
            continue;
        }
        let id = block
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let raw_name = block.get("name").and_then(Value::as_str).unwrap_or("tool");
        // MCP 前缀剥掉:Miyu 工具按本名显示(readable_tool_name 才认识),
        // claude 原生工具保持原名。
        let name = raw_name.strip_prefix("mcp__miyu__").unwrap_or(raw_name);
        if hidden_remote_tool(name) {
            continue;
        }
        remote_tools.insert(id.clone(), name.to_string());
        let input = block.get("input").cloned().unwrap_or(json!({}));
        on_chunk(ChatStreamChunk {
            kind: ChatStreamKind::RemoteToolStarted,
            text: json!({ "id": id, "name": name, "input": input }).to_string(),
        })?;
    }
    Ok(())
}

fn emit_remote_tool_finished<F>(
    frame: &Value,
    remote_tools: &HashMap<String, String>,
    on_chunk: &mut F,
) -> Result<()>
where
    F: FnMut(ChatStreamChunk) -> Result<()>,
{
    let Some(blocks) = frame.pointer("/message/content").and_then(Value::as_array) else {
        return Ok(());
    };
    for block in blocks {
        if block.get("type").and_then(Value::as_str) != Some("tool_result") {
            continue;
        }
        let id = block
            .get("tool_use_id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        // started 时被隐藏的工具(桥问答)没有登记,收口同样不发。
        let Some(name) = remote_tools.get(&id).cloned() else {
            continue;
        };
        let ok = !block
            .get("is_error")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let output = match block.get("content") {
            Some(Value::String(content)) => content.clone(),
            Some(Value::Array(parts)) => parts
                .iter()
                .filter_map(|part| part.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("\n"),
            _ => String::new(),
        };
        on_chunk(ChatStreamChunk {
            kind: ChatStreamKind::RemoteToolFinished,
            text: json!({
                "id": id,
                "name": name,
                "ok": ok,
                "output": if name == "Bash" {
                    truncate_block(&output, 4000)
                } else {
                    compact_line(&output, 4000)
                },
            })
            .to_string(),
        })?;
    }
    Ok(())
}

/// stream_event 内层事件 → 内容/思考缓冲。与 [`handle_anthropic_sse_data`]
/// 的差异:tool_use 只在思考通道展示一行,不进工具累加器;跨消息(message_
/// start 再来一次)时 content 用空行接续。
fn handle_claude_stream_event<F>(
    event: AnthropicStreamEvent,
    state: &mut AnthropicStreamState,
    on_chunk: &mut F,
) -> Result<()>
where
    F: FnMut(ChatStreamChunk) -> Result<()>,
{
    let close_reasoning_part = |state: &mut AnthropicStreamState, on_chunk: &mut F| -> Result<()> {
        if state.reasoning_part_active {
            flush_buffer(
                &state.reasoning,
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
        Ok(())
    };
    let open_reasoning_part = |state: &mut AnthropicStreamState, on_chunk: &mut F| -> Result<()> {
        if !state.reasoning_part_active {
            if !state.reasoning.is_empty() && !state.reasoning.ends_with("\n\n") {
                state.reasoning.push_str("\n\n");
            }
            on_chunk(ChatStreamChunk {
                kind: ChatStreamKind::ReasoningPartStart,
                text: String::new(),
            })?;
            state.reasoning_part_active = true;
        }
        Ok(())
    };
    match event.kind.as_str() {
        "message_start" => {
            if let Some(usage) = event.message.and_then(|message| message.usage) {
                merge_anthropic_usage(&mut state.usage, usage);
            }
            if !state.content.is_empty() && !state.content.ends_with("\n\n") {
                push_buffered_chunk(
                    &mut state.content,
                    &mut state.content_emitted,
                    ChatStreamKind::Content,
                    "\n\n".to_string(),
                    on_chunk,
                )?;
            }
        }
        "content_block_start" => {
            if let Some(block) = event.content_block {
                match block.kind.as_str() {
                    "text" => {
                        close_reasoning_part(state, on_chunk)?;
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
                        close_reasoning_part(state, on_chunk)?;
                        open_reasoning_part(state, on_chunk)?;
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
                    // tool_use 的展示交给完整 assistant 帧翻译出的
                    // tool.started 卡片,这里不再往思考通道塞行。
                    "tool_use" | "server_tool_use" | "mcp_tool_use" => {}
                    _ => {}
                }
            }
        }
        "content_block_delta" => {
            if let Some(delta) = event.delta {
                match delta.kind.as_deref() {
                    Some("text_delta") => {
                        close_reasoning_part(state, on_chunk)?;
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
                        if let Some(text) = delta.thinking {
                            open_reasoning_part(state, on_chunk)?;
                            push_buffered_chunk(
                                &mut state.reasoning,
                                &mut state.reasoning_emitted,
                                ChatStreamKind::Reasoning,
                                text,
                                on_chunk,
                            )?;
                        }
                    }
                    // 工具参数与思考签名在 claude 侧闭环,中转层不消费。
                    _ => {}
                }
            }
        }
        "content_block_stop" => {
            close_reasoning_part(state, on_chunk)?;
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
        }
        "error" => {
            let message = event
                .error
                .map(|error| match (error.kind, error.message) {
                    (Some(kind), Some(message)) => format!("{kind}: {message}"),
                    (Some(kind), None) => kind,
                    (None, Some(message)) => message,
                    (None, None) => "claude-code stream error".to_string(),
                })
                .unwrap_or_else(|| "claude-code stream error".to_string());
            bail!("{message}");
        }
        _ => {}
    }
    Ok(())
}

/// 结果帧的聚合用量(Anthropic 口径):prompt = input + cache_read +
/// cache_write,与 [`merge_anthropic_usage`] 的归一不变量一致。
fn usage_from_result_frame(frame: &Value) -> Option<Usage> {
    let usage = frame.get("usage")?;
    let read = usage
        .get("cache_read_input_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let write = usage
        .get("cache_creation_input_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let input = usage.get("input_tokens").and_then(Value::as_u64)?;
    let output = usage
        .get("output_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let reasoning = usage
        .pointer("/output_tokens_details/thinking_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let prompt = input.saturating_add(read).saturating_add(write);
    Some(Usage {
        prompt_tokens: prompt,
        completion_tokens: output,
        total_tokens: prompt.saturating_add(output),
        cache_read_tokens: read,
        cache_write_tokens: write,
        reasoning_tokens: reasoning,
        cache_reported: true,
        ..Usage::default()
    })
}

#[cfg(test)]
mod output_shaping_tests {
    /// Bash 走命令输出块,换行必须保住;其余工具仍折叠成一行摘要。
    #[test]
    fn bash_output_keeps_line_structure() {
        assert_eq!(super::truncate_block("a\nb\n", 4000), "a\nb");
        assert_eq!(super::compact_line("a\nb", 4000), "a b");
        let long = "x".repeat(4001);
        let cut = super::truncate_block(&long, 4000);
        assert!(cut.ends_with('…'));
        assert_eq!(cut.chars().count(), 4001);
    }
}
