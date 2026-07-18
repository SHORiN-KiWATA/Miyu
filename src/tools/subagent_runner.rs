use super::{readable_tool_name, ToolProgress, ToolRegistry};
use crate::i18n::is_zh;
use crate::llm::{
    ChatMessage, ChatResult, ChatStreamChunk, ChatStreamKind, OpenAiCompatibleClient, Usage,
};
use anyhow::Result;
use serde_json::{json, Value};
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Eq, PartialEq)]
pub enum ProgressMode {
    Hidden,
    Summary,
    Full,
}

impl ProgressMode {
    pub fn from_config(config: &crate::config::AppConfig) -> Self {
        Self::from_value(&config.display.tool_calls)
    }

    fn from_value(value: &str) -> Self {
        match value.trim().to_ascii_lowercase().as_str() {
            "hidden" => Self::Hidden,
            "full" => Self::Full,
            _ => Self::Summary,
        }
    }
}

#[derive(Clone)]
pub struct SubagentProgress {
    progress: ToolProgress,
    tool_mode: ProgressMode,
    enabled: bool,
}

impl SubagentProgress {
    pub fn new(progress: ToolProgress, tool_mode: ProgressMode, enabled: bool) -> Self {
        Self {
            progress,
            tool_mode,
            enabled,
        }
    }

    pub fn clone_inner(&self) -> ToolProgress {
        self.progress.clone()
    }

    pub fn phase(&self, message: impl Into<String>) {
        if self.enabled && self.tool_mode != ProgressMode::Hidden {
            self.progress.report(message.into());
        }
    }

    pub fn reasoning(&self, text: &str) {
        if self.enabled && self.tool_mode == ProgressMode::Full {
            self.progress
                .report(format!("__subagent_reasoning__{}", text));
        }
    }

    pub fn tool_start(&self, step: usize, name: &str, args: &str) {
        if !self.enabled || self.tool_mode == ProgressMode::Hidden {
            return;
        }
        if self.tool_mode == ProgressMode::Summary {
            let subject = crate::render::tool_subject(name, args)
                .map(|subject| format!(" · {subject}"))
                .unwrap_or_default();
            self.progress.report(if is_zh() {
                format!("工具 #{step}：{}{subject} 运行中", readable_tool_name(name))
            } else {
                format!("tool #{step}: {name}{subject} running")
            });
        }
    }

    pub fn tool_call_detail(&self, name: &str, args: &str) {
        if self.enabled && self.tool_mode == ProgressMode::Full && name != "run_command" {
            self.progress.report(format!(
                "__subtool_call__{}",
                json!({ "name": name, "args": args })
            ));
        }
    }

    pub fn tool_end(&self, step: usize, name: &str, args: &str, ok: bool, output: &str) {
        if !self.enabled || self.tool_mode == ProgressMode::Hidden {
            return;
        }
        if self.tool_mode == ProgressMode::Summary {
            let subject = crate::render::tool_subject(name, args)
                .map(|subject| format!(" · {subject}"))
                .unwrap_or_default();
            let status = if ok { "ok" } else { "err" };
            self.progress.report(if is_zh() {
                format!(
                    "工具 #{step}：{}{subject} {status}",
                    readable_tool_name(name)
                )
            } else {
                format!("tool #{step}: {name}{subject} {status}")
            });
        }
        if self.tool_mode == ProgressMode::Full {
            self.progress.report(format!(
                "__subtool_result__{}",
                json!({ "name": name, "args": args, "ok": ok, "output": output })
            ));
        }
    }
}

#[derive(Default)]
pub struct SubagentStats {
    pub tool_calls: usize,
    pub tool_ok: usize,
    pub tool_errors: usize,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub total_tokens: u64,
    pub token_estimate: u64,
    pub token_estimate_method: TokenEstimateMethod,
    pub budget_reached: bool,
}

#[derive(Clone, Copy, Default, Eq, PartialEq)]
pub enum TokenEstimateMethod {
    #[default]
    None,
    ProviderUsage,
    ProviderUsagePlusEstimate,
    RoughCharEstimate,
}

impl SubagentStats {
    pub fn add_usage_or_estimate(&mut self, usage: Option<&Usage>, texts: &[&str]) {
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

    pub fn public(&self) -> Value {
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

pub fn token_estimate_method_label(method: TokenEstimateMethod) -> &'static str {
    match method {
        TokenEstimateMethod::ProviderUsage => "provider_usage",
        TokenEstimateMethod::ProviderUsagePlusEstimate => "provider_usage_plus_estimate",
        TokenEstimateMethod::RoughCharEstimate | TokenEstimateMethod::None => "rough_char_estimate",
    }
}

pub fn estimate_tokens(texts: &[&str]) -> u64 {
    let combined: String = texts.iter().copied().collect();
    if combined.is_empty() {
        0
    } else {
        crate::agent::overflow::estimate_tokens(&combined) as u64
    }
}

pub fn format_token_count(tokens: u64, estimated: bool) -> String {
    let prefix = if estimated { "≈" } else { "" };
    if tokens >= 1_000_000 {
        format!("{prefix}{:.2}M", tokens as f64 / 1_000_000.0)
    } else if tokens >= 1_000 {
        format!("{prefix}{:.1}K", tokens as f64 / 1_000.0)
    } else {
        format!("{prefix}{tokens}")
    }
}

pub fn clip_inline(value: &str, max_chars: usize) -> String {
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

pub fn finalization_prompt() -> &'static str {
    "<tool_budget_reached>工具预算已用尽。不要再请求工具。请只基于上面的任务描述和已执行工具结果输出最终结果；缺少信息的地方明确说明。</tool_budget_reached>"
}

pub struct SubagentRunner {
    client: OpenAiCompatibleClient,
    system_prompt: String,
    tools: ToolRegistry,
    excluded_tools: Vec<String>,
    max_steps: usize,
    timeout_seconds: u64,
    progress: SubagentProgress,
}

impl SubagentRunner {
    pub fn new(
        client: OpenAiCompatibleClient,
        system_prompt: impl Into<String>,
        tools: ToolRegistry,
        progress: SubagentProgress,
    ) -> Self {
        Self {
            client,
            system_prompt: system_prompt.into(),
            tools,
            excluded_tools: Vec::new(),
            max_steps: 0,
            timeout_seconds: 60,
            progress,
        }
    }

    pub fn max_steps(mut self, n: usize) -> Self {
        self.max_steps = n;
        self
    }

    pub fn timeout_seconds(mut self, s: u64) -> Self {
        self.timeout_seconds = s;
        self
    }

    pub fn excluded_tools(mut self, names: &[&str]) -> Self {
        self.excluded_tools = names.iter().map(|s| s.to_string()).collect();
        self
    }

    pub async fn run(&self, prompt: &str) -> Result<(ChatResult, SubagentStats)> {
        let mut stats = SubagentStats::default();
        let messages = vec![
            ChatMessage::system(self.system_prompt.clone()),
            ChatMessage::plain("user", prompt.to_string()),
        ];

        let result = self
            .chat_with_tools(messages, &mut stats, Instant::now())
            .await?;

        stats.add_usage_or_estimate(
            result.usage.as_ref(),
            &[&self.system_prompt, prompt, &result.content],
        );

        self.report_stats(&stats);

        Ok((result, stats))
    }

    fn report_stats(&self, stats: &SubagentStats) {
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
        self.progress.phase(format!("__subagent_stats__{text}"));
    }

    async fn chat_with_tools(
        &self,
        mut messages: Vec<ChatMessage>,
        stats: &mut SubagentStats,
        _start: Instant,
    ) -> Result<ChatResult> {
        let excluded: Vec<&str> = self.excluded_tools.iter().map(String::as_str).collect();
        let definitions = self.tools.definitions_except(&excluded);
        let mut steps = 0usize;

        loop {
            if self.max_steps > 0 && steps >= self.max_steps {
                stats.budget_reached = true;
                messages.push(ChatMessage::plain("user", finalization_prompt()));
                let result = self
                    .client
                    .chat_stream(messages, Vec::new(), |chunk: ChatStreamChunk| {
                        if chunk.kind == ChatStreamKind::Reasoning {
                            self.progress.reasoning(&chunk.text);
                        }
                        Ok(())
                    })
                    .await?;
                stats.add_usage_or_estimate(result.usage.as_ref(), &[&result.content]);
                return Ok(result);
            }

            let result = self
                .client
                .chat_stream(
                    messages.clone(),
                    definitions.clone(),
                    |chunk: ChatStreamChunk| {
                        if chunk.kind == ChatStreamKind::Reasoning {
                            self.progress.reasoning(&chunk.text);
                        }
                        Ok(())
                    },
                )
                .await?;
            stats.add_usage_or_estimate(result.usage.as_ref(), &[]);

            if result.tool_calls.is_empty() {
                return Ok(result);
            }

            messages.push(ChatMessage::assistant(
                result.content.clone(),
                Some(result.tool_calls.clone()),
            ));

            for call in result.tool_calls {
                if self.max_steps > 0 && steps >= self.max_steps {
                    messages.push(ChatMessage::tool(
                        call.id,
                        "tool budget reached for this subagent session",
                    ));
                    continue;
                }
                steps += 1;
                stats.tool_calls += 1;

                self.progress
                    .tool_start(steps, &call.function.name, &call.function.arguments);
                self.progress
                    .tool_call_detail(&call.function.name, &call.function.arguments);

                let (output, ok) = match tokio::time::timeout(
                    Duration::from_secs(self.timeout_seconds.max(5)),
                    self.tools
                        .call(&call.function.name, &call.function.arguments),
                )
                .await
                {
                    Ok(Ok(output)) => (output, true),
                    Ok(Err(err)) => (format!("tool error: {err}"), false),
                    Err(_) => (
                        format!(
                            "tool error: {} timed out after {}s",
                            call.function.name, self.timeout_seconds
                        ),
                        false,
                    ),
                };

                if ok {
                    stats.tool_ok += 1;
                } else {
                    stats.tool_errors += 1;
                }

                self.progress.tool_end(
                    steps,
                    &call.function.name,
                    &call.function.arguments,
                    ok,
                    &output,
                );
                messages.push(ChatMessage::tool(call.id, output));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::ToolProgressEvent;
    use tokio::sync::mpsc;

    #[test]
    fn tool_summary_suppresses_raw_subagent_reasoning() {
        let (sender, mut receiver) = mpsc::unbounded_channel();
        let progress =
            SubagentProgress::new(ToolProgress::new(sender), ProgressMode::Summary, true);

        progress.reasoning("detailed reasoning");
        assert!(receiver.try_recv().is_err());
    }

    #[test]
    fn tool_full_emits_subagent_reasoning_and_tool_details() {
        let (sender, mut receiver) = mpsc::unbounded_channel();
        let progress = SubagentProgress::new(ToolProgress::new(sender), ProgressMode::Full, true);

        progress.reasoning("detailed reasoning");
        progress.tool_call_detail("grep", r#"{"pattern":"x"}"#);

        let ToolProgressEvent::Message(message) = receiver.try_recv().unwrap() else {
            panic!("expected reasoning progress message");
        };
        assert_eq!(message, "__subagent_reasoning__detailed reasoning");
        let ToolProgressEvent::Message(message) = receiver.try_recv().unwrap() else {
            panic!("expected tool detail progress message");
        };
        assert!(message.starts_with("__subtool_call__"));
        assert!(receiver.try_recv().is_err());
    }
}
