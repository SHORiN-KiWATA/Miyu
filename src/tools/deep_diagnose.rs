use super::subagent_runner::format_token_count;
use super::{readable_tool_name, ToolProgress, ToolRegistry, ToolSpec};
use crate::config::AppConfig;
use crate::i18n::{is_zh, text as t};
use crate::llm::{
    ChatMessage, ChatResult, ChatStreamChunk, ChatStreamKind, OpenAiCompatibleClient, Usage,
};
use crate::paths::MiyuPaths;
use anyhow::{bail, Result};
use serde_json::{json, Value};
use std::time::Duration;

const INPUT_METHOD_DIAGNOSIS_PROMPT: &str = crate::prompts::INPUT_METHOD_DIAGNOSIS_PROMPT;

#[derive(Clone)]
struct DiagnosisContext {
    config: AppConfig,
    paths: MiyuPaths,
    tools: ToolRegistry,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum ProgressMode {
    Hidden,
    Summary,
    Full,
}

#[derive(Clone)]
struct DiagnosisProgress {
    progress: ToolProgress,
    mode: ProgressMode,
}

impl DiagnosisProgress {
    fn new(config: &AppConfig, progress: ToolProgress) -> Self {
        let mode = match config
            .display
            .tool_calls
            .trim()
            .to_ascii_lowercase()
            .as_str()
        {
            "hidden" => ProgressMode::Hidden,
            "full" => ProgressMode::Full,
            _ => ProgressMode::Summary,
        };
        Self { progress, mode }
    }

    fn report(&self, message: impl Into<String>) {
        self.progress.report(message);
    }

    fn subtool(&self, message: impl Into<String>) {
        if self.mode == ProgressMode::Full {
            self.progress.report(message);
        }
    }

    fn reasoning(&self, text: &str) {
        if self.mode == ProgressMode::Full {
            self.progress
                .report(format!("__subagent_reasoning__{}", text));
        }
    }

    fn report_stats(&self, stats: &DiagnosisStats) {
        let text = if is_zh() {
            format!(
                "工具调用 {} 次　消耗词元 {}",
                stats.tool_calls,
                format_token_count(stats.token_estimate, false)
            )
        } else {
            format!(
                "tool calls: {}　token cost: {}",
                stats.tool_calls,
                format_token_count(stats.token_estimate, false)
            )
        };
        self.progress.report(format!("__subagent_stats__{text}"));
    }
}

#[derive(Default)]
struct DiagnosisStats {
    tool_calls: usize,
    tool_ok: usize,
    tool_errors: usize,
    prompt_tokens: u64,
    completion_tokens: u64,
    total_tokens: u64,
    token_estimate: u64,
    token_estimate_method: TokenEstimateMethod,
}

#[derive(Clone, Copy, Default, Eq, PartialEq)]
enum TokenEstimateMethod {
    #[default]
    None,
    ProviderUsage,
    ProviderUsagePlusEstimate,
    RoughCharEstimate,
}

impl DiagnosisStats {
    fn add_usage_or_estimate(&mut self, usage: Option<&Usage>, texts: &[&str]) {
        if let Some(usage) = usage {
            let total_tokens = usage.effective_total_tokens();
            if total_tokens > 0 {
                self.prompt_tokens += usage.prompt_tokens;
                self.completion_tokens += usage.completion_tokens;
                self.total_tokens += total_tokens;
                self.token_estimate += total_tokens;
                self.token_estimate_method = match self.token_estimate_method {
                    TokenEstimateMethod::None | TokenEstimateMethod::ProviderUsage => {
                        TokenEstimateMethod::ProviderUsage
                    }
                    _ => TokenEstimateMethod::ProviderUsagePlusEstimate,
                };
                return;
            }
        }
        let estimate = estimate_tokens(texts);
        self.token_estimate += estimate;
        self.token_estimate_method = match self.token_estimate_method {
            TokenEstimateMethod::None | TokenEstimateMethod::RoughCharEstimate => {
                TokenEstimateMethod::RoughCharEstimate
            }
            _ => TokenEstimateMethod::ProviderUsagePlusEstimate,
        };
    }

    fn public(&self) -> Value {
        json!({
            "tool_calls": self.tool_calls,
            "tool_ok": self.tool_ok,
            "tool_errors": self.tool_errors,
            "prompt_tokens": self.prompt_tokens,
            "completion_tokens": self.completion_tokens,
            "total_tokens": self.total_tokens,
            "token_estimate": self.token_estimate,
            "token_estimate_method": token_estimate_method_label(self.token_estimate_method),
            "token_estimate_is_actual": self.token_estimate_method == TokenEstimateMethod::ProviderUsage,
        })
    }
}

fn token_estimate_method_label(method: TokenEstimateMethod) -> &'static str {
    match method {
        TokenEstimateMethod::ProviderUsage => "provider_usage",
        TokenEstimateMethod::ProviderUsagePlusEstimate => "provider_usage_plus_estimate",
        TokenEstimateMethod::RoughCharEstimate | TokenEstimateMethod::None => "rough_char_estimate",
    }
}

fn estimate_tokens(texts: &[&str]) -> u64 {
    let combined: String = texts.iter().copied().collect();
    if combined.is_empty() {
        0
    } else {
        crate::agent::overflow::estimate_tokens(&combined) as u64
    }
}

pub fn register(
    registry: &mut ToolRegistry,
    config: AppConfig,
    paths: MiyuPaths,
    tools: ToolRegistry,
) {
    let context = DiagnosisContext {
        config,
        paths,
        tools,
    };
    registry.register(ToolSpec::new_with_progress(
        "linux_input_method_diagnose",
        "运行 Linux 输入法诊断子代理，基于运行时证据、框架识别、显示模式和输入法路径分析输出诊断报告。",
        json!({
            "type": "object",
            "properties": {
                "issue": { "type": "string", "description": "输入法问题或现象。" },
                "target": { "type": "string", "description": "可选目标应用或进程名，例如 steam 或 qq。" }
            },
            "required": ["issue"],
            "additionalProperties": false
        }),
        move |args, progress| {
            let context = context.clone();
            async move { run_linux_input_method_diagnose(args, context, progress).await }
        },
    ));
}

async fn run_linux_input_method_diagnose(
    args: Value,
    context: DiagnosisContext,
    progress: ToolProgress,
) -> Result<String> {
    if !context.config.plugins.deep_diagnose.enabled {
        bail!("linux input method diagnose plugin is disabled");
    }
    let issue = required(&args, "issue")?;
    let target = args
        .get("target")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string);
    let progress = DiagnosisProgress::new(&context.config, progress);
    progress.report(format!(
        "{}=\"{}\"",
        t("issue", "问题"),
        clip_inline(&issue, 80)
    ));
    let client = OpenAiCompatibleClient::from_config(&context.config, &context.paths)?
        .for_subagent_output(progress.mode == ProgressMode::Full);
    let prompt = input_method_prompt(&issue, target.as_deref());
    let system_prompt = INPUT_METHOD_DIAGNOSIS_PROMPT;
    let mut stats = DiagnosisStats::default();
    let result = chat_with_tools(
        &client,
        vec![
            ChatMessage::system(system_prompt),
            ChatMessage::plain("user", prompt.clone()),
        ],
        context.tools,
        context
            .config
            .plugins
            .deep_diagnose
            .tool_call_timeout_seconds,
        context.config.plugins.deep_diagnose.max_tool_steps,
        &progress,
        &mut stats,
    )
    .await?;
    stats.add_usage_or_estimate(
        result.usage.as_ref(),
        &[system_prompt, &prompt, &result.content],
    );
    progress.report_stats(&stats);
    let final_answer = strip_report_preamble(&result.content);
    Ok(serde_json::to_string_pretty(&json!({
        "ok": true,
        "kind": "linux_input_method_diagnosis",
        "issue": issue,
        "target": target,
        "final_answer": final_answer,
        "stats": stats.public(),
        "output_instruction": "这是 Linux 输入法诊断子代理返回的最终诊断报告。请把 final_answer 当作主要依据回复用户；如果用户追问完整报告，直接复述 final_answer。"
    }))?)
}

async fn chat_with_tools(
    client: &OpenAiCompatibleClient,
    mut messages: Vec<ChatMessage>,
    tools: ToolRegistry,
    timeout_seconds: u64,
    max_tool_steps: usize,
    progress: &DiagnosisProgress,
    stats: &mut DiagnosisStats,
) -> Result<ChatResult> {
    let definitions = tools.definitions_except(&[
        "linux_input_method_diagnose",
        "deep_research",
        "deep_research_linux_game_compatibility",
    ]);
    let mut steps = 0usize;
    loop {
        if max_tool_steps > 0 && steps >= max_tool_steps {
            messages.push(ChatMessage::plain("user", finalization_prompt()));
            let result = client
                .chat_stream(messages, Vec::new(), |chunk| {
                    if chunk.kind == ChatStreamKind::Reasoning {
                        progress.reasoning(&chunk.text);
                    }
                    Ok(())
                })
                .await?;
            stats.add_usage_or_estimate(result.usage.as_ref(), &[&result.content]);
            return Ok(result);
        }
        let result = client
            .chat_stream(
                messages.clone(),
                definitions.clone(),
                |chunk: ChatStreamChunk| {
                    if chunk.kind == ChatStreamKind::Reasoning {
                        progress.reasoning(&chunk.text);
                    }
                    Ok(())
                },
            )
            .await?;
        stats.add_usage_or_estimate(result.usage.as_ref(), &[]);
        if result.tool_calls.is_empty() {
            return Ok(result);
        }
        if !result.content.trim().is_empty() {
            messages.push(ChatMessage::assistant(result.content.clone(), None));
        }
        let mut transcript = Vec::new();
        for call in result.tool_calls {
            if max_tool_steps > 0 && steps >= max_tool_steps {
                transcript.push(render_internal_tool_result(
                    &call.function.name,
                    &call.function.arguments,
                    false,
                    "tool skipped: input method diagnosis tool budget reached",
                ));
                continue;
            }
            steps += 1;
            stats.tool_calls += 1;
            if progress.mode == ProgressMode::Summary {
                let subject =
                    crate::render::tool_subject(&call.function.name, &call.function.arguments)
                        .map(|subject| format!(" · {subject}"))
                        .unwrap_or_default();
                progress.report(if is_zh() {
                    format!(
                        "工具 #{steps}：{}{subject} 运行中",
                        readable_tool_name(&call.function.name)
                    )
                } else {
                    format!("tool #{steps}: {}{subject} running", call.function.name)
                });
            } else if progress.mode == ProgressMode::Full && call.function.name != "run_command" {
                progress.subtool(format!(
                    "__subtool_call__{}",
                    json!({
                        "name": call.function.name,
                        "args": call.function.arguments,
                    })
                ));
            }
            let (output, ok) = match tokio::time::timeout(
                Duration::from_secs(timeout_seconds.max(5)),
                tools.call(&call.function.name, &call.function.arguments),
            )
            .await
            {
                Ok(Ok(output)) => (output, true),
                Ok(Err(err)) => (format!("tool error: {err}"), false),
                Err(_) => (
                    format!(
                        "tool error: {} timed out after {timeout_seconds}s",
                        call.function.name
                    ),
                    false,
                ),
            };
            if ok {
                stats.tool_ok += 1;
            } else {
                stats.tool_errors += 1;
            }
            if progress.mode == ProgressMode::Summary {
                let subject =
                    crate::render::tool_subject(&call.function.name, &call.function.arguments)
                        .map(|subject| format!(" · {subject}"))
                        .unwrap_or_default();
                let status = if ok { "ok" } else { "err" };
                progress.report(if is_zh() {
                    format!(
                        "工具 #{steps}：{}{subject} {status}",
                        readable_tool_name(&call.function.name)
                    )
                } else {
                    format!("tool #{steps}: {}{subject} {status}", call.function.name)
                });
            } else if progress.mode == ProgressMode::Full {
                progress.subtool(format!(
                    "__subtool_result__{}",
                    json!({
                        "name": call.function.name,
                        "args": call.function.arguments,
                        "ok": ok,
                        "output": output,
                    })
                ));
            }
            transcript.push(render_internal_tool_result(
                &call.function.name,
                &call.function.arguments,
                ok,
                &output,
            ));
        }
        if !transcript.is_empty() {
            messages.push(ChatMessage::plain(
                "user",
                render_internal_tool_transcript(&transcript, steps, max_tool_steps),
            ));
        }
    }
}

fn render_internal_tool_transcript(results: &[String], steps: usize, max_steps: usize) -> String {
    format!(
        "<subagent_tool_transcript>\n说明：以下是宿主已经执行完成的内部工具调用结果，不是新的用户请求。请基于这些观察继续诊断；如证据已经足够，请输出最终报告。\ntool_budget: {steps}/{max_steps}\n{}\n</subagent_tool_transcript>",
        results.join("\n")
    )
}

fn render_internal_tool_result(name: &str, arguments: &str, ok: bool, output: &str) -> String {
    format!(
        "<tool_result name=\"{}\" ok=\"{}\">\narguments_json:\n```json\n{}\n```\noutput:\n```text\n{}\n```\n</tool_result>",
        name,
        ok,
        arguments.trim(),
        clip_inline(output, 6000)
    )
}

fn finalization_prompt() -> &'static str {
    "<tool_budget_reached>工具预算已用尽。不要再请求工具。请只基于上面的用户问题、系统要求和已执行工具结果输出最终诊断报告；缺少证据的地方明确写“不确定”或“缺证据”。</tool_budget_reached>"
}

fn input_method_prompt(issue: &str, target: Option<&str>) -> String {
    format!(
        "用户输入法问题：\n{issue}\n\n目标软件：{}\n\n请按照系统提示词中的流程完成诊断。优先调用 check_issue 收集输入法证据；如果目标软件框架或特殊行为不清楚，可以使用 fcitx5_input_method_wiki_qurey、知识库和网络搜索。最终只输出诊断报告。",
        target.unwrap_or("未明确，需从问题中推断")
    )
}

fn strip_report_preamble(content: &str) -> String {
    let trimmed = content.trim();
    for heading in ["## 问题分析", "# 问题分析"] {
        if let Some(index) = trimmed.find(heading) {
            return trimmed[index..].trim().to_string();
        }
    }
    trimmed
        .lines()
        .skip_while(|line| {
            let line = line.trim();
            line.is_empty()
                || line == "---"
                || line.contains("以下是")
                || line.contains("诊断报告") && line.len() < 20
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn required(args: &Value, key: &str) -> Result<String> {
    let value = args
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim();
    if value.is_empty() {
        bail!("missing required argument: {key}")
    }
    Ok(value.to_string())
}

fn clip_inline(value: &str, max_chars: usize) -> String {
    let value = value.split_whitespace().collect::<Vec<_>>().join(" ");
    if value.chars().count() <= max_chars {
        value
    } else {
        format!(
            "{}...",
            value
                .chars()
                .take(max_chars.saturating_sub(3))
                .collect::<String>()
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_report_preamble() {
        assert_eq!(
            strip_report_preamble("以下是诊断报告\n\n## 问题分析\nSteam 输入法不可用"),
            "## 问题分析\nSteam 输入法不可用"
        );
    }
}
