mod conversation;

use crate::config::AppConfig;
use crate::llm::{
    ChatMessage, ChatResult, ChatStreamChunk, ChatStreamKind, OpenAiCompatibleClient,
};
use crate::paths::MiyuPaths;
use crate::state::StateStore;
use crate::tools::ToolRegistry;
use anyhow::Result;
use chrono::Local;

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
}

impl Agent {
    pub fn new(
        config: AppConfig,
        paths: &MiyuPaths,
        state: StateStore,
        client: OpenAiCompatibleClient,
        tools: ToolRegistry,
    ) -> Result<Self> {
        let base_system_prompt = config.system_prompt(paths)?;
        state.reset_if_prompt_changed(&base_system_prompt)?;
        let system_prompt = with_current_time(base_system_prompt);
        let context_chars = config.active_context_chars()?;
        let tools_enabled = config.tools.enabled;
        let max_tool_rounds = config.tools.max_rounds;
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
        })
    }

    pub async fn chat_stream<F>(&mut self, input: &str, on_event: F) -> Result<ChatResult>
    where
        F: FnMut(AgentEvent) -> Result<()>,
    {
        self.state.trim_conversation_to_budget(
            self.context_chars,
            self.trim_at_ratio,
            self.trim_batch_ratio,
        )?;
        self.state.append_message("user", input)?;
        let mut messages = self.chat_messages()?;
        let result = self.chat_with_tools(&mut messages, on_event).await?;
        self.state
            .append_assistant_message(&result.content, result.reasoning.as_deref())?;
        if let Some(usage) = &result.usage {
            self.state.add_usage(usage)?;
        }
        Ok(result)
    }

    async fn chat_with_tools<F>(
        &self,
        messages: &mut Vec<ChatMessage>,
        mut on_event: F,
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
                on_event(AgentEvent::ToolCall {
                    name: call.function.name.clone(),
                    arguments: call.function.arguments.clone(),
                })?;
                let output = match self
                    .tools
                    .call(&call.function.name, &call.function.arguments)
                    .await
                {
                    Ok(output) => {
                        on_event(AgentEvent::ToolResult {
                            name: call.function.name.clone(),
                            ok: true,
                            output: output.clone(),
                        })?;
                        output
                    }
                    Err(err) => {
                        on_event(AgentEvent::ToolResult {
                            name: call.function.name.clone(),
                            ok: false,
                            output: format!("tool error: {err}"),
                        })?;
                        format!("tool error: {err}")
                    }
                };
                messages.push(ChatMessage::tool(call.id, output));
            }
        }
    }

    fn chat_messages(&self) -> Result<Vec<ChatMessage>> {
        let mut messages = vec![ChatMessage::system(self.system_prompt.clone())];
        for entry in self.state.load_conversation()? {
            if entry.role == "user" || entry.role == "assistant" {
                messages.push(ChatMessage::plain(entry.role, entry.content));
            }
        }
        Ok(messages)
    }
}

fn with_current_time(system_prompt: String) -> String {
    format!(
        "{system_prompt}\n\n<system-reminder>\n当前系统时间：{}\n</system-reminder>",
        Local::now().format("%Y年%m月%d日 %H时")
    )
}
