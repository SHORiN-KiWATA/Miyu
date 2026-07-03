mod conversation;

use crate::config::AppConfig;
use crate::llm::{
    ChatMessage, ChatResult, ChatStreamChunk, ChatStreamKind, OpenAiCompatibleClient,
};
use crate::memory::{EvictedTurn, MemoryStore};
use crate::paths::MiyuPaths;
use crate::state::StateStore;
use crate::tools::{self, memes, ToolPermission, ToolRegistry};
use anyhow::{bail, Result};
use chrono::Local;
use std::io::IsTerminal;
use tokio::sync::mpsc;

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum AgentMode {
    Yolo,
    Plan,
}

impl AgentMode {
    pub fn label(self) -> &'static str {
        match self {
            Self::Yolo => "YOLO",
            Self::Plan => "PLAN",
        }
    }

    fn reminder(self) -> &'static str {
        match self {
            Self::Yolo => crate::prompts::YOLO_REMINDER,
            Self::Plan => crate::prompts::PLAN_REMINDER,
        }
    }
}

#[derive(Debug, Clone)]
pub enum AgentEvent {
    Chunk(ChatStreamChunk),
    ToolCall {
        name: String,
        arguments: String,
    },
    ToolResult {
        name: String,
        ok: bool,
        output: String,
    },
    ToolProgress {
        name: String,
        message: String,
    },
    ExternalOutput,
}

pub struct Agent {
    state: StateStore,
    client: OpenAiCompatibleClient,
    system_prompt: String,
    context_chars: usize,
    trim_at_ratio: f32,
    trim_batch_ratio: f32,
    tools_enabled: bool,
    max_tool_rounds: usize,
    tools: ToolRegistry,
    memory: MemoryStore,
    mode: AgentMode,
    config: AppConfig,
    paths: MiyuPaths,
}

impl Agent {
    pub fn new(
        config: AppConfig,
        paths: &MiyuPaths,
        state: StateStore,
        client: OpenAiCompatibleClient,
        tools: ToolRegistry,
        mode: AgentMode,
    ) -> Result<Self> {
        let mut base_system_prompt = config.system_prompt(paths)?;
        if config.skills.enabled {
            let prompt = tools::skills_prompt(&config, paths)?;
            if !prompt.trim().is_empty() {
                base_system_prompt.push_str("\n\n");
                base_system_prompt.push_str(&prompt);
            }
        }
        if mode == AgentMode::Yolo {
            state.reset_if_prompt_changed(&base_system_prompt)?;
        }
        let system_prompt = with_current_time(base_system_prompt, mode);
        let context_chars = config.active_context_chars()?;
        let tools_enabled = config.tools.enabled;
        let max_tool_rounds = config.tools.max_rounds;
        let memory = MemoryStore::new(&config, paths);
        memory.init()?;
        Ok(Self {
            state,
            client,
            system_prompt,
            context_chars,
            trim_at_ratio: config.context.trim_at_ratio,
            trim_batch_ratio: config.context.trim_batch_ratio,
            tools_enabled,
            max_tool_rounds,
            tools,
            memory,
            mode,
            config,
            paths: paths.clone(),
        })
    }

    pub async fn chat_stream<F>(&mut self, input: &str, on_event: F) -> Result<ChatResult>
    where
        F: FnMut(AgentEvent) -> Result<()>,
    {
        self.state.mark_interrupted_turn_if_needed()?;
        let evicted = self.state.trim_conversation_to_budget(
            self.context_chars,
            self.trim_at_ratio,
            self.trim_batch_ratio,
        )?;
        let evicted = evicted
            .into_iter()
            .map(|entry| EvictedTurn {
                timestamp: entry.timestamp,
                role: entry.role,
                content: entry.content,
            })
            .collect::<Vec<_>>();
        self.memory.remember_evicted_turns(&evicted)?;
        let input = clean_user_visible_text(input);
        self.state.append_message("user", &input)?;
        let mut messages = self.chat_messages()?;
        if let Some(association) = self.memory.association(&input)? {
            messages.insert(
                1,
                ChatMessage::system(self.memory.format_association(&association)),
            );
        }
        let auto_meme_plan =
            memes::plan_auto_meme_before_reply(&self.config, &self.paths, &self.client, &input)
                .await?;
        if let Some(plan) = &auto_meme_plan {
            messages.push(ChatMessage::system(plan.reminder.clone()));
        }
        let mut on_event = on_event;
        let mut used_tools = Vec::new();
        let mut persisted_tool_reports = Vec::new();
        let result = self
            .chat_with_tools(
                &mut messages,
                &mut used_tools,
                &mut persisted_tool_reports,
                &mut on_event,
            )
            .await?;
        if let Some(plan) = auto_meme_plan {
            on_event(AgentEvent::ExternalOutput)?;
            memes::render_auto_meme(&self.config, &self.paths, &plan.event).await?;
            memes::record_auto_meme_event(&self.config, &self.paths, &plan.event)?;
        }
        self.state
            .append_assistant_message(&result.content, result.reasoning.as_deref())?;
        for (tool_name, report) in persisted_tool_reports {
            self.state.append_tool_report_context(&tool_name, &report)?;
        }
        self.memory.process_after_turn(&input, &result.content)?;
        if let Some(usage) = &result.usage {
            self.state.add_usage(usage)?;
        }
        Ok(result)
    }

    async fn chat_with_tools<F>(
        &self,
        messages: &mut Vec<ChatMessage>,
        used_tools: &mut Vec<String>,
        persisted_tool_reports: &mut Vec<(String, String)>,
        on_event: &mut F,
    ) -> Result<ChatResult>
    where
        F: FnMut(AgentEvent) -> Result<()>,
    {
        let definitions = if self.tools_enabled {
            self.tools.definitions()
        } else {
            Vec::new()
        };
        let mut tool_round = 0usize;
        loop {
            if self.max_tool_rounds > 0 && tool_round >= self.max_tool_rounds {
                let content = format!(
                    "工具调用已达到上限 {} 轮，已停止继续调用。可将 `tools.max_rounds` 设为 0 以允许无限工具调用。",
                    self.max_tool_rounds
                );
                on_event(AgentEvent::Chunk(ChatStreamChunk {
                    kind: ChatStreamKind::Content,
                    text: content.clone(),
                }))?;
                return Ok(ChatResult {
                    content,
                    reasoning: None,
                    usage: None,
                    tool_calls: Vec::new(),
                });
            }
            tool_round += 1;
            let result = self
                .client
                .chat_stream(messages.clone(), definitions.clone(), |chunk| {
                    on_event(AgentEvent::Chunk(chunk))
                })
                .await?;
            if result.tool_calls.is_empty() || !self.tools_enabled {
                return Ok(result);
            }
            messages.push(ChatMessage::assistant(
                result.content.clone(),
                Some(result.tool_calls.clone()),
            ));
            for call in result.tool_calls {
                used_tools.push(call.function.name.clone());
                on_event(AgentEvent::ToolCall {
                    name: call.function.name.clone(),
                    arguments: call.function.arguments.clone(),
                })?;
                if self.mode == AgentMode::Plan
                    && self.tools.permission(&call.function.name)? != ToolPermission::ReadOnly
                {
                    bail!(
                        "Plan mode blocked non-read-only tool: {}",
                        call.function.name
                    );
                }
                if call.function.name == "install_aur_package"
                    && used_tools.iter().any(|name| name == "review_aur_package")
                {
                    let output = "tool error: install_aur_package cannot run in the same turn as review_aur_package; ask the user to confirm installation first".to_string();
                    on_event(AgentEvent::ToolResult {
                        name: call.function.name.clone(),
                        ok: false,
                        output: output.clone(),
                    })?;
                    messages.push(ChatMessage::tool(call.id, output));
                    continue;
                }
                let (progress_tx, mut progress_rx) = mpsc::unbounded_channel();
                let tool_future = self.tools.call_with_progress(
                    &call.function.name,
                    &call.function.arguments,
                    progress_tx,
                );
                tokio::pin!(tool_future);
                let output = loop {
                    tokio::select! {
                        result = &mut tool_future => {
                            break match result {
                                Ok(output) => {
                                    while let Ok(message) = progress_rx.try_recv() {
                                        on_event(AgentEvent::ToolProgress {
                                            name: call.function.name.clone(),
                                            message,
                                        })?;
                                    }
                                    on_event(AgentEvent::ToolResult {
                                        name: call.function.name.clone(),
                                        ok: true,
                                        output: output.clone(),
                                    })?;
                                    if let Some(report) = extract_persistable_tool_report(
                                        &call.function.name,
                                        &output,
                                    ) {
                                        persisted_tool_reports
                                            .push((call.function.name.clone(), report));
                                    }
                                    output
                                }
                                Err(err) => {
                                    while let Ok(message) = progress_rx.try_recv() {
                                        on_event(AgentEvent::ToolProgress {
                                            name: call.function.name.clone(),
                                            message,
                                        })?;
                                    }
                                    on_event(AgentEvent::ToolResult {
                                        name: call.function.name.clone(),
                                        ok: false,
                                        output: format!("tool error: {err}"),
                                    })?;
                                    format!("tool error: {err}")
                                }
                            };
                        }
                        Some(message) = progress_rx.recv() => {
                            on_event(AgentEvent::ToolProgress {
                                name: call.function.name.clone(),
                                message,
                            })?;
                        }
                    }
                };
                messages.push(ChatMessage::tool(call.id, output));
            }
        }
    }

    fn chat_messages(&self) -> Result<Vec<ChatMessage>> {
        let mut messages = vec![ChatMessage::system(self.system_prompt.clone())];
        if let Some(summary) = memes::last_auto_meme_reminder(&self.config, &self.paths)? {
            messages.push(ChatMessage::system(summary));
        }
        for entry in self.state.load_conversation()? {
            if entry.role == "user" || entry.role == "assistant" {
                messages.push(ChatMessage::plain(entry.role, entry.content));
            }
        }
        Ok(messages)
    }
}

fn extract_persistable_tool_report(tool_name: &str, output: &str) -> Option<String> {
    let field = match tool_name {
        "linux_game_compatibility" => "final_report",
        "linux_input_method_diagnose" | "deep_diagnose" | "deep_research" => "final_answer",
        _ => return None,
    };
    serde_json::from_str::<serde_json::Value>(output)
        .ok()
        .and_then(|value| {
            value
                .get(field)
                .and_then(serde_json::Value::as_str)
                .map(str::trim)
                .map(str::to_string)
        })
        .filter(|report| !report.is_empty())
}

fn with_current_time(system_prompt: String, mode: AgentMode) -> String {
    let cwd = std::env::current_dir()
        .map(|path| path.display().to_string())
        .unwrap_or_else(|_| "unknown".to_string());
    let runtime = terminal_runtime_context();
    format!(
        "{system_prompt}\n\n<system-reminder>\n当前系统时间：{}。用户询问当前时间时，优先使用这里的时间，不需要调用命令查询。\n当前工作目录：{cwd}。涉及相对路径、当前项目、文件操作时优先以此为准。\n{runtime}\n</system-reminder>\n\n{}",
        Local::now().format("%Y年%m月%d日 %H:%M"),
        mode.reminder()
    )
}

fn terminal_runtime_context() -> String {
    let stdin_tty = std::io::stdin().is_terminal();
    let stdout_tty = std::io::stdout().is_terminal();
    let stderr_tty = std::io::stderr().is_terminal();
    let environment = if stdin_tty || stdout_tty || stderr_tty {
        if crate::i18n::is_zh() {
            "终端会话"
        } else {
            "terminal session"
        }
    } else if crate::i18n::is_zh() {
        "非交互或管道环境"
    } else {
        "non-interactive or piped environment"
    };
    let shell = std::env::var("SHELL")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| "unknown".to_string());
    let mut terminal_parts = Vec::new();
    for key in ["TERM_PROGRAM", "TERM", "COLORTERM"] {
        if let Ok(value) = std::env::var(key) {
            if !value.trim().is_empty() {
                terminal_parts.push(format!("{key}={value}"));
            }
        }
    }
    let terminal = if terminal_parts.is_empty() {
        "unknown".to_string()
    } else {
        terminal_parts.join(", ")
    };
    if crate::i18n::is_zh() {
        format!("当前运行环境：{environment}。当前 shell：{shell}。当前终端标识：{terminal}。")
    } else {
        format!("Current runtime environment: {environment}. Current shell: {shell}. Terminal identifiers: {terminal}.")
    }
}

fn clean_user_visible_text(input: &str) -> String {
    let mut output = input.to_string();
    for tag in ["system-reminder", "system_reminder"] {
        output = strip_tagged_sections(output, tag);
    }
    output
}

fn strip_tagged_sections(mut text: String, tag: &str) -> String {
    let open = format!("<{tag}");
    let close = format!("</{tag}>");
    while let Some(start) = text.find(&open) {
        let Some(relative_end) = text[start..].find(&close) else {
            text.replace_range(start.., "");
            break;
        };
        let end = start + relative_end + close.len();
        text.replace_range(start..end, "");
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_pasted_system_reminder_from_user_input() {
        let input = "继续<system-reminder>hidden</system-reminder> ok";

        assert_eq!(clean_user_visible_text(input), "继续 ok");
    }

    #[test]
    fn strips_unclosed_system_reminder_from_user_input() {
        let input = "继续<system_reminder>hidden";

        assert_eq!(clean_user_visible_text(input), "继续");
    }
}
