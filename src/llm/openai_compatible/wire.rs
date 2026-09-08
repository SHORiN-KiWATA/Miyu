//! 三套协议的线上数据结构。
//!
//! 请求体、响应体、流式事件，纯 serde 定义，不含逻辑。分开放是因为它们**同名
//! 概念在三套协议里字段完全不同**（`ChatStreamChoice` / `ResponsesStreamItem` /
//! `AnthropicStreamBlock` 说的都是「模型这一步产出了什么」），凑在业务代码里
//! 看会一直串。
//!
//! `null_as_default` 到处在用：不少供应商会把可选字段发成 `null` 而不是省略，
//! 严格反序列化会直接失败。

use crate::llm::openai_compatible::*;

pub(in crate::llm::openai_compatible) const CHAT_RESERVED_BODY_KEYS: &[&str] = &[
    "model",
    "messages",
    "temperature",
    "stream",
    "stream_options",
    "tools",
    "chat_template_kwargs",
];

pub(in crate::llm::openai_compatible) const RESPONSES_RESERVED_BODY_KEYS: &[&str] = &[
    "model",
    "input",
    "instructions",
    "previous_response_id",
    "stream",
    "tools",
    "reasoning",
    "temperature",
];

pub(in crate::llm::openai_compatible) const ANTHROPIC_RESERVED_BODY_KEYS: &[&str] = &[
    "model",
    "system",
    "messages",
    "tools",
    "stream",
    "max_tokens",
    "temperature",
    "thinking",
];

pub(in crate::llm::openai_compatible) fn sanitize_extra_body(
    extra: Option<Map<String, Value>>,
    reserved_keys: &[&str],
) -> Option<Map<String, Value>> {
    let mut extra = extra?;
    for key in reserved_keys {
        extra.remove(*key);
    }
    (!extra.is_empty()).then_some(extra)
}

pub(in crate::llm::openai_compatible) fn merge_extra_body(
    base: Option<Map<String, Value>>,
    overlay: Option<Map<String, Value>>,
) -> Option<Map<String, Value>> {
    let mut base = base.unwrap_or_default();
    for (key, value) in overlay.unwrap_or_default() {
        match base.get_mut(&key) {
            Some(existing) => merge_json_value(existing, value),
            None => {
                base.insert(key, value);
            }
        }
    }
    (!base.is_empty()).then_some(base)
}

pub(in crate::llm::openai_compatible) fn merge_json_value(base: &mut Value, overlay: Value) {
    if let (Some(base), Some(overlay)) = (base.as_object_mut(), overlay.as_object()) {
        for (key, value) in overlay {
            match base.get_mut(key) {
                Some(existing) => merge_json_value(existing, value.clone()),
                None => {
                    base.insert(key.clone(), value.clone());
                }
            }
        }
    } else {
        *base = overlay;
    }
}

#[derive(Debug, Clone, Serialize)]
pub(in crate::llm::openai_compatible) struct ChatRequest {
    pub(in crate::llm::openai_compatible) model: String,
    pub(in crate::llm::openai_compatible) messages: Vec<ChatMessage>,
    pub(in crate::llm::openai_compatible) temperature: f32,
    pub(in crate::llm::openai_compatible) stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(in crate::llm::openai_compatible) stream_options: Option<ChatStreamOptions>,
    /// Only set by cache-keepalive pings; normal chat leaves the provider
    /// default in place.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(in crate::llm::openai_compatible) max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(in crate::llm::openai_compatible) tools: Option<Vec<ToolDefinition>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(in crate::llm::openai_compatible) chat_template_kwargs: Option<ChatTemplateKwargs>,
    #[serde(flatten)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(in crate::llm::openai_compatible) extra_body: Option<Map<String, Value>>,
}

#[derive(Debug, Clone, Serialize)]
pub(in crate::llm::openai_compatible) struct ChatStreamOptions {
    pub(in crate::llm::openai_compatible) include_usage: bool,
}

#[derive(Debug, Serialize)]
pub(in crate::llm::openai_compatible) struct ResponsesRequest {
    pub(in crate::llm::openai_compatible) model: String,
    pub(in crate::llm::openai_compatible) input: Vec<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(in crate::llm::openai_compatible) instructions: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(in crate::llm::openai_compatible) previous_response_id: Option<String>,
    pub(in crate::llm::openai_compatible) stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(in crate::llm::openai_compatible) tools: Option<Vec<Value>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(in crate::llm::openai_compatible) reasoning: Option<ResponsesReasoning>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(in crate::llm::openai_compatible) temperature: Option<f32>,
    #[serde(flatten)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(in crate::llm::openai_compatible) extra_body: Option<Map<String, Value>>,
}

#[derive(Debug, Serialize)]
pub(in crate::llm::openai_compatible) struct ResponsesReasoning {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(in crate::llm::openai_compatible) effort: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(in crate::llm::openai_compatible) summary: Option<String>,
}

pub(in crate::llm::openai_compatible) fn default_responses_reasoning(
    summary: &str,
) -> ResponsesReasoning {
    ResponsesReasoning {
        effort: Some("medium".to_string()),
        summary: Some(summary.to_string()),
    }
}

#[derive(Debug, Serialize)]
pub(in crate::llm::openai_compatible) struct AnthropicRequest {
    pub(in crate::llm::openai_compatible) model: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(in crate::llm::openai_compatible) system: Option<String>,
    pub(in crate::llm::openai_compatible) messages: Vec<AnthropicMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(in crate::llm::openai_compatible) tools: Option<Vec<AnthropicTool>>,
    pub(in crate::llm::openai_compatible) stream: bool,
    pub(in crate::llm::openai_compatible) max_tokens: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(in crate::llm::openai_compatible) temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(in crate::llm::openai_compatible) thinking: Option<Value>,
    #[serde(flatten)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(in crate::llm::openai_compatible) extra_body: Option<Map<String, Value>>,
}

#[derive(Debug, Serialize)]
pub(in crate::llm::openai_compatible) struct AnthropicMessage {
    pub(in crate::llm::openai_compatible) role: String,
    pub(in crate::llm::openai_compatible) content: Vec<AnthropicContentBlock>,
}

#[derive(Debug, Serialize)]
#[serde(tag = "type")]
pub(in crate::llm::openai_compatible) enum AnthropicContentBlock {
    #[serde(rename = "text")]
    Text { text: String },
    #[serde(rename = "image")]
    Image { source: AnthropicImageSource },
    /// PDF 输入。官方规格:`source` 与 image 同构(base64 / url),块要摆在
    /// 文本块**之前**,base64 里不能有换行;整个请求 ≤32MB、≤600 页
    /// (200k 窗口的模型 100 页)。
    #[serde(rename = "document")]
    Document { source: AnthropicImageSource },
    #[serde(rename = "tool_use")]
    ToolUse {
        id: String,
        name: String,
        input: Value,
    },
    #[serde(rename = "tool_result")]
    ToolResult {
        tool_use_id: String,
        content: String,
    },
    /// Extended-thinking block replayed on assistant tool_use turns. Anthropic
    /// 400s a thinking-enabled tool loop whose assistant turns omit the block.
    #[serde(rename = "thinking")]
    Thinking { thinking: String, signature: String },
}

#[derive(Debug, Serialize)]
#[serde(tag = "type")]
pub(in crate::llm::openai_compatible) enum AnthropicImageSource {
    #[serde(rename = "base64")]
    Base64 { media_type: String, data: String },
    #[serde(rename = "url")]
    Url { url: String },
}

#[derive(Debug, Serialize)]
pub(in crate::llm::openai_compatible) struct AnthropicTool {
    pub(in crate::llm::openai_compatible) name: String,
    pub(in crate::llm::openai_compatible) description: String,
    pub(in crate::llm::openai_compatible) input_schema: Value,
}

#[derive(Debug, Clone, Serialize)]
pub(in crate::llm::openai_compatible) struct ChatTemplateKwargs {
    pub(in crate::llm::openai_compatible) enable_thinking: bool,
}

#[derive(Debug, Deserialize)]
pub(in crate::llm::openai_compatible) struct ChatStreamResponse {
    #[serde(default, deserialize_with = "null_as_default")]
    pub(in crate::llm::openai_compatible) choices: Vec<ChatStreamChoice>,
    #[serde(default, deserialize_with = "null_as_default")]
    pub(in crate::llm::openai_compatible) usage: Option<Usage>,
    #[serde(default, deserialize_with = "null_as_default")]
    pub(in crate::llm::openai_compatible) error: Option<Value>,
}

#[derive(Debug, Deserialize)]
pub(in crate::llm::openai_compatible) struct ChatCompletionResponse {
    #[serde(default, deserialize_with = "null_as_default")]
    pub(in crate::llm::openai_compatible) choices: Vec<ChatCompletionChoice>,
    #[serde(default, deserialize_with = "null_as_default")]
    pub(in crate::llm::openai_compatible) usage: Option<Usage>,
    #[serde(default, deserialize_with = "null_as_default")]
    pub(in crate::llm::openai_compatible) error: Option<Value>,
}

#[derive(Debug, Deserialize)]
pub(in crate::llm::openai_compatible) struct ChatCompletionChoice {
    #[serde(default, deserialize_with = "null_as_default")]
    pub(in crate::llm::openai_compatible) finish_reason: Option<String>,
    #[serde(default)]
    pub(in crate::llm::openai_compatible) message: ChatChoiceMessage,
}

#[derive(Debug, Deserialize)]
pub(in crate::llm::openai_compatible) struct ChatStreamChoice {
    #[serde(default, deserialize_with = "null_as_default")]
    pub(in crate::llm::openai_compatible) finish_reason: Option<String>,
    #[serde(default)]
    pub(in crate::llm::openai_compatible) delta: ChatChoiceMessage,
}

#[derive(Debug, Default, Deserialize)]
pub(in crate::llm::openai_compatible) struct ChatChoiceMessage {
    #[serde(default, deserialize_with = "null_as_default")]
    pub(in crate::llm::openai_compatible) content: Option<String>,
    #[serde(default, deserialize_with = "null_as_default")]
    pub(in crate::llm::openai_compatible) reasoning_content: Option<String>,
    #[serde(default, deserialize_with = "null_as_default")]
    pub(in crate::llm::openai_compatible) reasoning: Option<String>,
    #[serde(default, deserialize_with = "null_as_default")]
    pub(in crate::llm::openai_compatible) thinking: Option<String>,
    #[serde(default, deserialize_with = "null_as_default")]
    pub(in crate::llm::openai_compatible) thinking_content: Option<String>,
    #[serde(default, deserialize_with = "null_as_default")]
    pub(in crate::llm::openai_compatible) reasoning_text: Option<String>,
    #[serde(default, deserialize_with = "null_as_default")]
    pub(in crate::llm::openai_compatible) reasoning_details: Option<serde_json::Value>,
    #[serde(default, deserialize_with = "null_as_default")]
    pub(in crate::llm::openai_compatible) tool_calls: Vec<ToolCallDelta>,
}

pub(in crate::llm::openai_compatible) fn null_as_default<'de, D, T>(
    deserializer: D,
) -> std::result::Result<T, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Default + Deserialize<'de>,
{
    Ok(Option::<T>::deserialize(deserializer)?.unwrap_or_default())
}

#[derive(Debug, Default, Deserialize)]
pub(in crate::llm::openai_compatible) struct ToolCallDelta {
    #[serde(default)]
    pub(in crate::llm::openai_compatible) index: usize,
    #[serde(default, deserialize_with = "null_as_default")]
    pub(in crate::llm::openai_compatible) id: Option<String>,
    #[serde(rename = "type", default, deserialize_with = "null_as_default")]
    pub(in crate::llm::openai_compatible) kind: Option<String>,
    #[serde(default)]
    pub(in crate::llm::openai_compatible) function: ToolCallFunctionDelta,
}

#[derive(Debug, Default, Deserialize)]
pub(in crate::llm::openai_compatible) struct ToolCallFunctionDelta {
    #[serde(default, deserialize_with = "null_as_default")]
    pub(in crate::llm::openai_compatible) name: Option<String>,
    #[serde(default, deserialize_with = "null_as_default")]
    pub(in crate::llm::openai_compatible) arguments: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(in crate::llm::openai_compatible) struct ResponsesStreamEvent {
    #[serde(rename = "type")]
    pub(in crate::llm::openai_compatible) kind: String,
    #[serde(default, deserialize_with = "null_as_default")]
    pub(in crate::llm::openai_compatible) delta: Option<String>,
    #[serde(default, deserialize_with = "null_as_default")]
    pub(in crate::llm::openai_compatible) arguments: Option<String>,
    #[serde(default, deserialize_with = "null_as_default")]
    pub(in crate::llm::openai_compatible) name: Option<String>,
    #[serde(default, deserialize_with = "null_as_default")]
    pub(in crate::llm::openai_compatible) refusal: Option<String>,
    #[serde(default, deserialize_with = "null_as_default")]
    pub(in crate::llm::openai_compatible) text: Option<String>,
    #[serde(default, deserialize_with = "null_as_default")]
    pub(in crate::llm::openai_compatible) content_index: Option<usize>,
    #[serde(default, deserialize_with = "null_as_default")]
    pub(in crate::llm::openai_compatible) item_id: Option<String>,
    #[serde(default, deserialize_with = "null_as_default")]
    pub(in crate::llm::openai_compatible) item: Option<ResponsesStreamItem>,
    #[serde(default, deserialize_with = "null_as_default")]
    pub(in crate::llm::openai_compatible) response: Option<ResponsesStreamResponse>,
}

#[derive(Debug, Deserialize)]
pub(in crate::llm::openai_compatible) struct ResponsesStreamItem {
    #[serde(rename = "type")]
    pub(in crate::llm::openai_compatible) kind: String,
    #[serde(default, deserialize_with = "null_as_default")]
    pub(in crate::llm::openai_compatible) id: Option<String>,
    #[serde(default, deserialize_with = "null_as_default")]
    pub(in crate::llm::openai_compatible) call_id: Option<String>,
    #[serde(default, deserialize_with = "null_as_default")]
    pub(in crate::llm::openai_compatible) name: Option<String>,
    #[serde(default, deserialize_with = "null_as_default")]
    pub(in crate::llm::openai_compatible) arguments: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(in crate::llm::openai_compatible) struct ResponsesStreamResponse {
    #[serde(default, deserialize_with = "null_as_default")]
    pub(in crate::llm::openai_compatible) id: Option<String>,
    #[serde(default, deserialize_with = "null_as_default")]
    pub(in crate::llm::openai_compatible) usage: Option<ResponsesUsage>,
    #[serde(default, deserialize_with = "null_as_default")]
    pub(in crate::llm::openai_compatible) incomplete_details: Option<ResponsesIncompleteDetails>,
}

#[derive(Debug, Deserialize)]
pub(in crate::llm::openai_compatible) struct ResponsesIncompleteDetails {
    #[serde(default, deserialize_with = "null_as_default")]
    pub(in crate::llm::openai_compatible) reason: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(in crate::llm::openai_compatible) struct ResponsesUsage {
    #[serde(default)]
    pub(in crate::llm::openai_compatible) input_tokens: u64,
    #[serde(default)]
    pub(in crate::llm::openai_compatible) output_tokens: u64,
    #[serde(default)]
    pub(in crate::llm::openai_compatible) total_tokens: u64,
    #[serde(default)]
    pub(in crate::llm::openai_compatible) input_tokens_details: Option<ResponsesInputTokenDetails>,
    #[serde(default)]
    pub(in crate::llm::openai_compatible) output_tokens_details:
        Option<ResponsesOutputTokenDetails>,
}

#[derive(Debug, Deserialize)]
pub(in crate::llm::openai_compatible) struct ResponsesInputTokenDetails {
    #[serde(default)]
    pub(in crate::llm::openai_compatible) cached_tokens: Option<u64>,
    #[serde(default)]
    pub(in crate::llm::openai_compatible) cache_write_tokens: Option<u64>,
}

#[derive(Debug, Deserialize)]
pub(in crate::llm::openai_compatible) struct ResponsesOutputTokenDetails {
    #[serde(default)]
    pub(in crate::llm::openai_compatible) reasoning_tokens: Option<u64>,
}

#[derive(Debug, Deserialize)]
pub(in crate::llm::openai_compatible) struct AnthropicStreamEvent {
    #[serde(rename = "type")]
    pub(in crate::llm::openai_compatible) kind: String,
    #[serde(default, deserialize_with = "null_as_default")]
    pub(in crate::llm::openai_compatible) index: Option<usize>,
    #[serde(default, deserialize_with = "null_as_default")]
    pub(in crate::llm::openai_compatible) message: Option<AnthropicStreamMessage>,
    #[serde(default, deserialize_with = "null_as_default")]
    pub(in crate::llm::openai_compatible) content_block: Option<AnthropicStreamBlock>,
    #[serde(default, deserialize_with = "null_as_default")]
    pub(in crate::llm::openai_compatible) delta: Option<AnthropicStreamDelta>,
    #[serde(default, deserialize_with = "null_as_default")]
    pub(in crate::llm::openai_compatible) usage: Option<AnthropicUsage>,
    #[serde(default, deserialize_with = "null_as_default")]
    pub(in crate::llm::openai_compatible) error: Option<AnthropicStreamError>,
}

#[derive(Debug, Deserialize)]
pub(in crate::llm::openai_compatible) struct AnthropicStreamMessage {
    #[serde(default, deserialize_with = "null_as_default")]
    pub(in crate::llm::openai_compatible) usage: Option<AnthropicUsage>,
}

#[derive(Debug, Deserialize)]
pub(in crate::llm::openai_compatible) struct AnthropicStreamBlock {
    #[serde(rename = "type")]
    pub(in crate::llm::openai_compatible) kind: String,
    #[serde(default, deserialize_with = "null_as_default")]
    pub(in crate::llm::openai_compatible) id: Option<String>,
    #[serde(default, deserialize_with = "null_as_default")]
    pub(in crate::llm::openai_compatible) name: Option<String>,
    #[serde(default, deserialize_with = "null_as_default")]
    pub(in crate::llm::openai_compatible) text: Option<String>,
    #[serde(default, deserialize_with = "null_as_default")]
    pub(in crate::llm::openai_compatible) thinking: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(in crate::llm::openai_compatible) struct AnthropicStreamDelta {
    #[serde(rename = "type", default, deserialize_with = "null_as_default")]
    pub(in crate::llm::openai_compatible) kind: Option<String>,
    #[serde(default, deserialize_with = "null_as_default")]
    pub(in crate::llm::openai_compatible) text: Option<String>,
    #[serde(default, deserialize_with = "null_as_default")]
    pub(in crate::llm::openai_compatible) thinking: Option<String>,
    #[serde(default, deserialize_with = "null_as_default")]
    pub(in crate::llm::openai_compatible) partial_json: Option<String>,
    #[serde(default, deserialize_with = "null_as_default")]
    pub(in crate::llm::openai_compatible) signature: Option<String>,
    /// `message_delta` 携带终止原因;丢掉它会让 Anthropic 路径的
    /// finish_reason 恒为 None,max_tokens 截断的工具参数照常执行。
    #[serde(default, deserialize_with = "null_as_default")]
    pub(in crate::llm::openai_compatible) stop_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(in crate::llm::openai_compatible) struct AnthropicUsage {
    #[serde(default)]
    pub(in crate::llm::openai_compatible) input_tokens: u64,
    #[serde(default)]
    pub(in crate::llm::openai_compatible) output_tokens: u64,
    #[serde(default)]
    pub(in crate::llm::openai_compatible) cache_read_input_tokens: Option<u64>,
    #[serde(default)]
    pub(in crate::llm::openai_compatible) cache_creation_input_tokens: Option<u64>,
}

#[derive(Debug, Deserialize)]
pub(in crate::llm::openai_compatible) struct AnthropicStreamError {
    #[serde(rename = "type", default, deserialize_with = "null_as_default")]
    pub(in crate::llm::openai_compatible) kind: Option<String>,
    #[serde(default, deserialize_with = "null_as_default")]
    pub(in crate::llm::openai_compatible) message: Option<String>,
}
