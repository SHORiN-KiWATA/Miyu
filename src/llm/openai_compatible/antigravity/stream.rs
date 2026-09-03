//! agy 子进程的生命周期与 stream-json 事件泵。
//!
//! stdout 每行一个事件:`init`(首行,带会话 id 与代理名)/ `step_update`
//! (正文增量、工具步、错误步)/ `result`(终态)。与 claude 线的差异:
//! ①正文分片在 `agent_response` 步的 `text_delta` 里,同轮多次模型调用是多个
//! 步,步间补空行;②工具步 ACTIVE/DONE/ERROR 三态,DONE 的 `tool_info.output`
//! 只有命令与 MCP 结果有正文;③result 的 usage 是整轮累计,单次调用的用量在
//! 最后一个 `agent_response DONE` 里;④两种静默失败——续传目标丢失时静默
//! 新开会话、代理没挂上时静默退回默认提示词——都只能靠 init 首行判定,判定
//! 失败立刻杀进程(init 先于模型调用,不花额度)。
//!
//! 超时/出错路径必须显式杀进程组:drop future 只是弃 promise,不杀子进程。

use super::{AntigravityRuntime, ResumeTargetLost, AGENT_NAME, MCP_SERVER_NAME};
use crate::llm::openai_compatible::claude_code::stream::{
    compact_line, hidden_remote_tool, kill_process_group, truncate_block,
};
use crate::llm::openai_compatible::*;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

pub(super) struct TurnOutcome {
    pub(super) result: ChatResult,
    pub(super) conversation_id: Option<String>,
}

/// 把登录态/额度类失败翻译成端点调度认识的分类。只看 result 级的错误文本:
/// stderr 每次启动都打一行 "not logged into Antigravity" 再静默鉴权成功,
/// 拿它判 401 会误杀。
fn classify_agy_failure(text: &str) -> Option<HttpStatusFailure> {
    let lower = text.to_ascii_lowercase();
    const RATE_LIMIT: &[&str] = &[
        "quota",
        "rate limit",
        "too many requests",
        "resource_exhausted",
        "resource exhausted",
        "usage limit",
        "out of credits",
        "credits exhausted",
    ];
    const AUTH: &[&str] = &[
        "not logged in",
        "not logged into",
        "sign in",
        "unauthenticated",
        "authentication",
        "login required",
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
    /// 当前正文步的 step_index:换步时补空行。
    response_step: Option<u64>,
    /// 最后一次模型调用的用量(上下文表读它)。
    per_call_usage: Option<Usage>,
    /// 已发过 started 的工具步。
    started_tools: HashSet<u64>,
    /// `error_message` 步的正文,静默失败时并进报错。
    error_text: String,
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn run_agy_turn<F>(
    runtime: &AntigravityRuntime,
    workdir: &std::path::Path,
    args: &[String],
    env: &[(String, Option<String>)],
    stdin_payload: &str,
    expected_conversation: Option<&str>,
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
    for (key, value) in env {
        match value {
            Some(value) => {
                command.env(key, value);
            }
            None => {
                command.env_remove(key);
            }
        }
    }
    let mut child = command.spawn().map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            anyhow::anyhow!(
                "{}: {}",
                t(
                    "Antigravity CLI (agy) not found; install it or set plugins.antigravity.binary",
                    "找不到 Antigravity CLI(agy);请安装它或配置 plugins.antigravity.binary"
                ),
                runtime.binary.display()
            )
        } else {
            anyhow::Error::from(error).context("failed to spawn the Antigravity CLI")
        }
    })?;
    let pid = child.id().unwrap_or_default();
    let mut stdin = child.stdin.take().context("agy stdin unavailable")?;
    let stdout = child.stdout.take().context("agy stdout unavailable")?;
    let stderr = child.stderr.take().context("agy stderr unavailable")?;
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
        .context("failed to write the agy stdin payload")?;
    // 关写端 = 本轮输入结束;agy 跑完这一轮就退出。
    drop(stdin);

    let read_stderr = |tail: &Arc<Mutex<String>>| {
        tail.lock()
            .map(|tail| tail.trim().to_string())
            .unwrap_or_default()
    };
    let mut lines = tokio::io::BufReader::new(stdout).lines();
    let mut state = StreamState::default();
    let mut conversation_id: Option<String> = None;
    let mut final_frame: Option<Value> = None;
    loop {
        let line = match tokio::time::timeout(runtime.idle_timeout, lines.next_line()).await {
            Err(_) => {
                kill_process_group(pid);
                return Err(anyhow::anyhow!(
                    "agy produced no output for {}s; the process was killed",
                    runtime.idle_timeout.as_secs()
                )
                .context(TransportFailure {
                    stage: "antigravity.stream",
                    kind: TransportFailureKind::Timeout,
                }));
            }
            Ok(read) => read.context("failed to read agy stdout")?,
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
                tracing::warn!(request_id, %error, "agy emitted a non-JSON stdout line");
                continue;
            }
        };
        match value.get("event").and_then(Value::as_str) {
            Some("init") => {
                let actual = value
                    .get("conversation_id")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                let agent = value.pointer("/init/agent").and_then(Value::as_str);
                if agent != Some(AGENT_NAME) {
                    // 代理没挂上 = 静默跑在 agy 自己的默认提示词上,人格全丢。
                    kill_process_group(pid);
                    bail!(
                        "agy did not load the `{AGENT_NAME}` persona agent (init.agent = {agent:?}); {}",
                        read_stderr(&stderr_tail)
                    );
                }
                if let Some(expected) = expected_conversation {
                    if actual != expected {
                        kill_process_group(pid);
                        return Err(anyhow::Error::new(ResumeTargetLost {
                            requested: expected.to_string(),
                            actual,
                        }));
                    }
                }
                conversation_id = Some(actual);
            }
            Some("step_update") => {
                if let Some(step) = value.get("step_update") {
                    handle_step(step, &mut state, on_chunk)?;
                }
            }
            Some("result") => {
                final_frame = Some(value);
                break;
            }
            _ => {}
        }
    }
    let exit = match tokio::time::timeout(Duration::from_secs(10), child.wait()).await {
        Ok(status) => status.ok(),
        Err(_) => {
            kill_process_group(pid);
            None
        }
    };
    stderr_task.abort();
    let stderr_text = read_stderr(&stderr_tail);

    let Some(final_frame) = final_frame else {
        kill_process_group(pid);
        let code = exit
            .and_then(|status| status.code())
            .map(|code| code.to_string())
            .unwrap_or_else(|| "unknown".to_string());
        bail!(
            "agy exited (code {code}) without a result event: {} {}",
            state.error_text.trim(),
            stderr_text
        );
    };
    let result_frame = final_frame.get("result").cloned().unwrap_or(Value::Null);
    if conversation_id.is_none() {
        conversation_id = result_frame
            .get("conversation_id")
            .and_then(Value::as_str)
            .map(str::to_string);
    }
    let status = result_frame
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let response = result_frame
        .get("response")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let error_field = match result_frame.get("error") {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Null) | None => String::new(),
        Some(other) => other.to_string(),
    };
    if status != "SUCCESS" {
        let detail = [
            error_field.as_str(),
            state.error_text.trim(),
            response.trim(),
        ]
        .into_iter()
        .find(|text| !text.is_empty())
        .unwrap_or(stderr_text.as_str())
        .to_string();
        let mut error = anyhow::anyhow!("agy turn failed ({status}): {}", detail.trim());
        if let Some(failure) = classify_agy_failure(&format!("{error_field}\n{detail}")) {
            error = error.context(failure);
        }
        return Err(error);
    }

    flush_buffer(
        &state.content,
        &mut state.content_emitted,
        ChatStreamKind::Content,
        &mut *on_chunk,
        true,
    )?;
    let mut content = state.content;
    if content.trim().is_empty() && !response.trim().is_empty() {
        on_chunk(ChatStreamChunk {
            kind: ChatStreamKind::Content,
            text: response.clone(),
        })?;
        content = response;
    }
    let total_usage = result_frame.get("usage").and_then(usage_from_agy);
    let per_call_usage = state.per_call_usage.clone();
    if content.trim().is_empty()
        && per_call_usage.is_none()
        && total_usage
            .as_ref()
            .map(|usage| usage.effective_total_tokens() == 0)
            .unwrap_or(true)
    {
        // 「SUCCESS 但什么都没发生」:代理配置坏了、权限被拒、或模型一个字
        // 没吐。agy 对这几种都只在 stderr/error_message 步里留痕。
        bail!(
            "agy produced no output (status SUCCESS, zero usage): {} {}",
            state.error_text.trim(),
            stderr_text
        );
    }
    let usage = total_usage.or_else(|| per_call_usage.clone());
    let mut result = finalize_stream_result(content, String::new(), usage, Vec::new(), false)?;
    result.finish_reason = Some("stop".to_string());
    result.last_request_usage = per_call_usage;
    Ok(TurnOutcome {
        result,
        conversation_id,
    })
}

fn handle_step<F>(step: &Value, state: &mut StreamState, on_chunk: &mut F) -> Result<()>
where
    F: FnMut(ChatStreamChunk) -> Result<()>,
{
    let index = step.get("step_index").and_then(Value::as_u64).unwrap_or(0);
    let step_state = step.get("state").and_then(Value::as_str).unwrap_or("");
    match step.get("step_type").and_then(Value::as_str) {
        Some("agent_response") => {
            if state.response_step != Some(index) {
                if state.response_step.is_some()
                    && !state.content.is_empty()
                    && !state.content.ends_with("\n\n")
                {
                    push_buffered_chunk(
                        &mut state.content,
                        &mut state.content_emitted,
                        ChatStreamKind::Content,
                        "\n\n".to_string(),
                        on_chunk,
                    )?;
                }
                state.response_step = Some(index);
            }
            if let Some(text) = step.get("text_delta").and_then(Value::as_str) {
                push_buffered_chunk(
                    &mut state.content,
                    &mut state.content_emitted,
                    ChatStreamKind::Content,
                    text.to_string(),
                    on_chunk,
                )?;
            }
            if step_state == "DONE" {
                if let Some(usage) = step.get("usage").and_then(usage_from_agy) {
                    state.per_call_usage = Some(usage);
                }
            }
        }
        Some("tool") => {
            let info = step.get("tool_info").cloned().unwrap_or(Value::Null);
            let raw_name = step
                .get("tool_name")
                .and_then(Value::as_str)
                .or_else(|| info.get("name").and_then(Value::as_str))
                .unwrap_or("tool");
            let (name, input) = translate_tool(raw_name, info.get("parameters").cloned());
            if hidden_remote_tool(&name) {
                return Ok(());
            }
            let id = format!("agy-{index}");
            if state.started_tools.insert(index) {
                on_chunk(ChatStreamChunk {
                    kind: ChatStreamKind::RemoteToolStarted,
                    text: json!({ "id": id, "name": name, "input": input }).to_string(),
                })?;
            }
            if step_state == "DONE" || step_state == "ERROR" {
                let error = info
                    .pointer("/error/message")
                    .and_then(Value::as_str)
                    .map(str::to_string);
                let ok = step_state == "DONE" && error.is_none();
                let raw_output = match info.get("output") {
                    Some(Value::String(text)) => text.clone(),
                    Some(Value::Null) | None => String::new(),
                    Some(other) => other.to_string(),
                };
                let output = error.unwrap_or(raw_output).replace('\r', "");
                let output = if crate::render::is_command_tool(&name) {
                    truncate_block(&output, 4000)
                } else {
                    compact_line(&output, 4000)
                };
                on_chunk(ChatStreamChunk {
                    kind: ChatStreamKind::RemoteToolFinished,
                    text: json!({ "id": id, "name": name, "ok": ok, "output": output }).to_string(),
                })?;
            }
        }
        Some("error_message") => {
            if let Some(text) = step.get("text_delta").and_then(Value::as_str) {
                state.error_text.push_str(text);
                state.error_text.push('\n');
            }
        }
        _ => {}
    }
    Ok(())
}

/// agy 的工具名/入参 → Miyu 卡片认识的名字与键。
/// - eager 注册的桥工具叫 `mcp_miyu_<name>`,剥前缀就是 Miyu 本名;
/// - 懒加载时是 `call_mcp_tool{ServerName,ToolName,Arguments}`,拆出来;
/// - 原生工具的入参键是 CamelCase(CommandLine/AbsolutePath…),归一成 Miyu 的
///   `command`/`path`/…,终端 `↳` 主题、WebUI 命令卡、平台日志三端都不用改。
fn translate_tool(raw_name: &str, parameters: Option<Value>) -> (String, Value) {
    let bridge_prefix = format!("mcp_{MCP_SERVER_NAME}_");
    if let Some(inner) = raw_name.strip_prefix(&bridge_prefix) {
        return (inner.to_string(), parameters.unwrap_or(json!({})));
    }
    if raw_name == "call_mcp_tool" {
        if let Some(params) = parameters.as_ref() {
            if params.get("ServerName").and_then(Value::as_str) == Some(MCP_SERVER_NAME) {
                let name = params
                    .get("ToolName")
                    .and_then(Value::as_str)
                    .unwrap_or("tool")
                    .to_string();
                let input = params.get("Arguments").cloned().unwrap_or(json!({}));
                return (name, input);
            }
        }
    }
    (raw_name.to_string(), normalize_native_arguments(parameters))
}

fn normalize_native_arguments(parameters: Option<Value>) -> Value {
    let Some(Value::Object(map)) = parameters else {
        return parameters.unwrap_or(json!({}));
    };
    let mut out = Map::new();
    for (key, value) in map {
        let mapped = match key.as_str() {
            "CommandLine" => "command",
            "Cwd" => "cwd",
            "AbsolutePath" | "TargetFile" | "DirectoryPath" | "SearchDirectory" | "SearchPath" => {
                "path"
            }
            "Pattern" => "pattern",
            "Query" => "query",
            "Url" => "url",
            // agy 给 UI 用的摘要字段,不是工具入参。
            "toolAction" | "toolSummary" => continue,
            other => other,
        };
        out.insert(mapped.to_string(), value);
    }
    Value::Object(out)
}

/// agy 的 usage 对象 → Miyu 口径。`input_tokens` 已含缓存命中部分(cache_read
/// 是它的子集)。**agy 的 `cache_read_tokens` 不可全信**:09-03 真机六轮里五轮
/// 它等于整个 input(9355/9355、45486/45486…),而本轮新输入的用户消息不可能
/// 已在缓存里——整段命中在物理上不成立。按 usage.rs 的规矩(没有真实依据的
/// 比率不能渲染成确定数字),声称整段命中的按「未报告缓存」处理;部分命中
/// (24324/33736 这种)照实上报。
fn usage_from_agy(value: &Value) -> Option<Usage> {
    let input = value.get("input_tokens").and_then(Value::as_u64)?;
    let output = value
        .get("output_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let thinking = value
        .get("thinking_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let cache_read = value
        .get("cache_read_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let total = value
        .get("total_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(input.saturating_add(output));
    let cache_reported = cache_read > 0 && cache_read < input;
    Some(Usage {
        prompt_tokens: input,
        completion_tokens: output,
        total_tokens: total,
        cache_read_tokens: if cache_reported { cache_read } else { 0 },
        reasoning_tokens: thinking,
        cache_reported,
        ..Usage::default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_arguments_are_normalized_to_miyu_keys() {
        let (name, input) = translate_tool(
            "run_command",
            Some(json!({ "CommandLine": "pwd", "Cwd": "/w", "toolAction": "x" })),
        );
        assert_eq!(name, "run_command");
        assert_eq!(input, json!({ "command": "pwd", "cwd": "/w" }));
        let (name, input) =
            translate_tool("view_file", Some(json!({ "AbsolutePath": "/tmp/a.txt" })));
        assert_eq!(name, "view_file");
        assert_eq!(input["path"], "/tmp/a.txt");
    }

    #[test]
    fn bridge_tools_are_unwrapped_in_both_shapes() {
        let (name, input) = translate_tool("mcp_miyu_use_meme", Some(json!({ "action": "show" })));
        assert_eq!(name, "use_meme");
        assert_eq!(input["action"], "show");
        let (name, input) = translate_tool(
            "call_mcp_tool",
            Some(
                json!({ "ServerName": "miyu", "ToolName": "alarm", "Arguments": { "at": "9:00" } }),
            ),
        );
        assert_eq!(name, "alarm");
        assert_eq!(input["at"], "9:00");
        // 别家 MCP 服务器不剥、不拆。
        let (name, _) = translate_tool("mcp_other_thing", None);
        assert_eq!(name, "mcp_other_thing");
    }

    #[test]
    fn failure_classification_reads_quota_and_auth_phrases() {
        assert_eq!(
            classify_agy_failure("Quota exceeded for model").map(|f| f.status),
            Some(429)
        );
        assert_eq!(
            classify_agy_failure("You are not logged into Antigravity").map(|f| f.status),
            Some(401)
        );
        assert!(classify_agy_failure("model output error").is_none());
    }

    #[test]
    fn usage_maps_agy_fields() {
        let usage = usage_from_agy(&json!({
            "input_tokens": 100, "output_tokens": 7, "thinking_tokens": 3,
            "cache_read_tokens": 40, "total_tokens": 110
        }))
        .unwrap();
        assert_eq!(usage.prompt_tokens, 100);
        assert_eq!(usage.completion_tokens, 7);
        assert_eq!(usage.reasoning_tokens, 3);
        assert_eq!(usage.cache_read_tokens, 40);
        assert_eq!(usage.total_tokens, 110);
        assert!(usage.cache_reported);
        // 声称整段命中 = 不可信,按未报告处理;零命中同样不算"报告了缓存"。
        let bogus = usage_from_agy(&json!({ "input_tokens": 100, "cache_read_tokens": 100 })).unwrap();
        assert_eq!(bogus.cache_read_tokens, 0);
        assert!(!bogus.cache_reported);
        let none = usage_from_agy(&json!({ "input_tokens": 100, "cache_read_tokens": 0 })).unwrap();
        assert!(!none.cache_reported);
    }
}
