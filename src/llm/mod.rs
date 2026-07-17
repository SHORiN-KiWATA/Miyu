mod openai_compatible;

pub use openai_compatible::{OpenAiCompatibleClient, ThinkingVariantOptions};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<ChatContent>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCall>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ChatContent {
    Text(String),
    Parts(Vec<ChatContentPart>),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ChatContentPart {
    #[serde(rename = "text")]
    Text { text: String },
    #[serde(rename = "image_url")]
    ImageUrl { image_url: ImageUrlContent },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImageUrlContent {
    pub url: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub function: ToolCallFunction,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCallFunction {
    pub name: String,
    pub arguments: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ToolDefinition {
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub function: FunctionDefinition,
}

#[derive(Debug, Clone, Serialize)]
pub struct FunctionDefinition {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
}

impl ChatMessage {
    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: "system".to_string(),
            content: Some(ChatContent::Text(content.into())),
            tool_call_id: None,
            tool_calls: None,
        }
    }

    pub fn assistant(content: impl Into<String>, tool_calls: Option<Vec<ToolCall>>) -> Self {
        let text = content.into();
        let has_tool_calls = tool_calls.as_ref().map(|c| !c.is_empty()).unwrap_or(false);
        let content = if text.trim().is_empty() && has_tool_calls {
            None
        } else {
            Some(ChatContent::Text(text))
        };
        Self {
            role: "assistant".to_string(),
            content,
            tool_call_id: None,
            tool_calls,
        }
    }

    pub fn tool(tool_call_id: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            role: "tool".to_string(),
            content: Some(ChatContent::Text(content.into())),
            tool_call_id: Some(tool_call_id.into()),
            tool_calls: None,
        }
    }

    pub fn plain(role: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            role: role.into(),
            content: Some(ChatContent::Text(content.into())),
            tool_call_id: None,
            tool_calls: None,
        }
    }

    pub fn user_with_image(text: impl Into<String>, image_url: impl Into<String>) -> Self {
        Self {
            role: "user".to_string(),
            content: Some(ChatContent::Parts(vec![
                ChatContentPart::Text { text: text.into() },
                ChatContentPart::ImageUrl {
                    image_url: ImageUrlContent {
                        url: image_url.into(),
                    },
                },
            ])),
            tool_call_id: None,
            tool_calls: None,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Usage {
    #[serde(default)]
    pub prompt_tokens: u64,
    #[serde(default)]
    pub completion_tokens: u64,
    #[serde(default)]
    pub total_tokens: u64,
}

impl Usage {
    pub fn effective_total_tokens(&self) -> u64 {
        if self.total_tokens > 0 {
            self.total_tokens
        } else {
            self.prompt_tokens.saturating_add(self.completion_tokens)
        }
    }
}

#[derive(Debug, Clone)]
pub struct ChatResult {
    pub content: String,
    pub reasoning: Option<String>,
    pub usage: Option<Usage>,
    pub usage_estimated: bool,
    pub tool_calls: Vec<ToolCall>,
    pub provider_id: Option<String>,
    pub model: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChatStreamKind {
    Content,
    Reasoning,
    ToolCall,
}

#[derive(Debug, Clone)]
pub struct ChatStreamChunk {
    pub kind: ChatStreamKind,
    pub text: String,
}
