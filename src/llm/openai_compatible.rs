use super::{
    ChatMessage, ChatResult, ChatStreamChunk, ChatStreamKind, ToolCall, ToolCallFunction,
    ToolDefinition, Usage,
};
use crate::config::{AppConfig, ProviderConfig};
use crate::i18n::text as t;
use crate::paths::MiyuPaths;
use anyhow::{bail, Context, Result};
use futures_util::StreamExt;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProviderProtocol {
    Auto,
    OpenAiChat,
    OpenAiResponses,
    Anthropic,
}

impl ProviderProtocol {
    fn from_provider(provider: &ProviderConfig) -> Result<Self> {
        match provider.protocol.trim() {
            "" | "auto" => Ok(Self::Auto),
            "openai-chat" => Ok(Self::OpenAiChat),
            "openai-responses" => Ok(Self::OpenAiResponses),
            "anthropic" => Ok(Self::Anthropic),
            protocol => bail!("unsupported provider protocol: {protocol}"),
        }
    }
}

#[derive(Clone)]
pub struct OpenAiCompatibleClient {
    client: Client,
    provider: ProviderConfig,
    api_key: String,
}

impl OpenAiCompatibleClient {
    pub fn from_config(config: &AppConfig, paths: &MiyuPaths) -> Result<Self> {
        let provider = config.provider(None)?;
        Self::new(provider, config, paths)
    }

    pub fn new(provider: &ProviderConfig, _config: &AppConfig, paths: &MiyuPaths) -> Result<Self> {
        if provider.default_model.trim().is_empty() {
            bail!(
                "{}: {}",
                t(
                    "provider has no active model; select a model before chatting",
                    "provider 没有当前模型；请先选择模型再聊天",
                ),
                provider.id
            );
        }
        let client = Client::builder()
            .timeout(Duration::from_secs(provider.timeout_seconds))
            .build()?;
        let api_key = provider.resolved_api_key(paths)?;
        Ok(Self {
            client,
            provider: provider.clone(),
            api_key,
        })
    }

    pub async fn chat_stream<F>(
        &self,
        messages: Vec<ChatMessage>,
        tools: Vec<ToolDefinition>,
        mut on_chunk: F,
    ) -> Result<ChatResult>
    where
        F: FnMut(ChatStreamChunk) -> Result<()>,
    {
        let protocol = ProviderProtocol::from_provider(&self.provider)?;
        if protocol == ProviderProtocol::Anthropic {
            return self
                .chat_anthropic_stream(messages, tools, &mut on_chunk)
                .await;
        }
        if protocol == ProviderProtocol::OpenAiResponses
            || (protocol == ProviderProtocol::Auto && self.uses_openai_responses())
        {
            if let Some(result) = self
                .chat_responses_stream(messages.clone(), tools.clone(), &mut on_chunk)
                .await?
            {
                return Ok(result);
            }
            if protocol == ProviderProtocol::OpenAiResponses {
                bail!("OpenAI Responses protocol is not supported by this provider");
            }
        }
        let request = ChatRequest {
            model: self.provider.default_model.clone(),
            messages,
            temperature: self.provider.temperature,
            stream: true,
            tools: (!tools.is_empty()).then_some(tools),
            chat_template_kwargs: taotoken_glm_chat_template_kwargs(&self.provider),
        };
        let url = format!(
            "{}/chat/completions",
            self.provider.base_url.trim_end_matches('/')
        );
        let response = self
            .client
            .post(url)
            .bearer_auth(&self.api_key)
            .json(&request)
            .send()
            .await?;
        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            bail!(
                "{} ({status}): {body}",
                t("chat completions stream request failed", "聊天流式请求失败",)
            );
        }

        let mut buffer = String::new();
        let mut content = String::new();
        let mut content_emitted = 0usize;
        let mut reasoning = String::new();
        let mut reasoning_emitted = 0usize;
        let mut usage = None;
        let mut tool_calls = ToolCallAccumulator::default();
        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk?;
            buffer.push_str(&String::from_utf8_lossy(&chunk));
            while let Some(index) = buffer.find('\n') {
                let line = buffer[..index].trim_end_matches('\r').to_string();
                buffer.drain(..=index);
                if let Some(done) = handle_sse_line(
                    &line,
                    &mut content,
                    &mut content_emitted,
                    &mut reasoning,
                    &mut reasoning_emitted,
                    &mut usage,
                    &mut tool_calls,
                    &mut on_chunk,
                )? {
                    if done {
                        return finalize_stream_result(
                            content,
                            reasoning,
                            usage,
                            tool_calls.finish(),
                        );
                    }
                }
            }
        }
        if !buffer.trim().is_empty() {
            let _ = handle_sse_line(
                buffer.trim_end_matches('\r'),
                &mut content,
                &mut content_emitted,
                &mut reasoning,
                &mut reasoning_emitted,
                &mut usage,
                &mut tool_calls,
                &mut on_chunk,
            )?;
        }
        finalize_stream_result(content, reasoning, usage, tool_calls.finish())
    }

    async fn chat_anthropic_stream<F>(
        &self,
        messages: Vec<ChatMessage>,
        tools: Vec<ToolDefinition>,
        on_chunk: &mut F,
    ) -> Result<ChatResult>
    where
        F: FnMut(ChatStreamChunk) -> Result<()>,
    {
        let request = AnthropicRequest {
            model: self.provider.default_model.clone(),
            system: lower_anthropic_system(&messages),
            messages: lower_anthropic_messages(messages),
            tools: (!tools.is_empty()).then(|| lower_anthropic_tools(tools)),
            stream: true,
            max_tokens: 4096,
            temperature: Some(self.provider.temperature),
        };
        let url = format!("{}/messages", self.provider.base_url.trim_end_matches('/'));
        let response = self
            .client
            .post(url)
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", "2023-06-01")
            .json(&request)
            .send()
            .await?;
        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            bail!(
                "{} ({status}): {body}",
                t(
                    "anthropic messages stream request failed",
                    "Anthropic Messages 流式请求失败"
                )
            );
        }

        let mut state = AnthropicStreamState::default();
        let mut buffer = SseBuffer::default();
        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk?;
            for data in buffer.push(&String::from_utf8_lossy(&chunk))? {
                if handle_anthropic_sse_data(&data, &mut state, &mut *on_chunk)? {
                    return finalize_stream_result(
                        state.content,
                        state.reasoning,
                        state.usage,
                        state.tool_calls.finish(),
                    );
                }
            }
        }
        for data in buffer.finish()? {
            let _ = handle_anthropic_sse_data(&data, &mut state, &mut *on_chunk)?;
        }
        finalize_stream_result(
            state.content,
            state.reasoning,
            state.usage,
            state.tool_calls.finish(),
        )
    }

    async fn chat_responses_stream<F>(
        &self,
        messages: Vec<ChatMessage>,
        tools: Vec<ToolDefinition>,
        on_chunk: &mut F,
    ) -> Result<Option<ChatResult>>
    where
        F: FnMut(ChatStreamChunk) -> Result<()>,
    {
        let request = ResponsesRequest {
            model: self.provider.default_model.clone(),
            input: lower_responses_messages(messages),
            instructions: None,
            stream: true,
            tools: (!tools.is_empty()).then(|| lower_responses_tools(tools)),
            reasoning: Some(ResponsesReasoning {
                effort: Some("medium"),
                summary: Some("concise"),
            }),
            temperature: Some(self.provider.temperature),
        };
        let url = format!("{}/responses", self.provider.base_url.trim_end_matches('/'));
        let response = self
            .client
            .post(url)
            .bearer_auth(&self.api_key)
            .json(&request)
            .send()
            .await?;
        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            if responses_unsupported(status.as_u16(), &body) {
                return Ok(None);
            }
            bail!(
                "{} ({status}): {body}",
                t("responses stream request failed", "Responses 流式请求失败")
            );
        }

        let mut buffer = String::new();
        let mut content = String::new();
        let mut content_emitted = 0usize;
        let mut reasoning = String::new();
        let mut reasoning_emitted = 0usize;
        let mut usage = None;
        let mut content_started = false;
        let mut tool_calls = ResponsesToolAccumulator::default();
        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk?;
            buffer.push_str(&String::from_utf8_lossy(&chunk));
            while let Some(index) = buffer.find('\n') {
                let line = buffer[..index].trim_end_matches('\r').to_string();
                buffer.drain(..=index);
                if handle_responses_sse_line(
                    &line,
                    &mut content,
                    &mut content_emitted,
                    &mut reasoning,
                    &mut reasoning_emitted,
                    &mut usage,
                    &mut content_started,
                    &mut tool_calls,
                    &mut *on_chunk,
                )? {
                    return finalize_stream_result(content, reasoning, usage, tool_calls.finish())
                        .map(Some);
                }
            }
        }
        finalize_stream_result(content, reasoning, usage, tool_calls.finish()).map(Some)
    }

    fn uses_openai_responses(&self) -> bool {
        let model = self.provider.default_model.to_ascii_lowercase();
        model.starts_with("gpt-5")
            || model.starts_with("o1")
            || model.starts_with("o3")
            || model.starts_with("o4")
    }
}

fn responses_unsupported(status: u16, body: &str) -> bool {
    if status == 404 || status == 405 {
        return true;
    }
    if status != 400 {
        return false;
    }
    let body = body.to_ascii_lowercase();
    body.contains("unsupported")
        || body.contains("not supported")
        || body.contains("unknown parameter")
        || body.contains("invalid endpoint")
        || body.contains("not found")
}

#[derive(Debug, Serialize)]
struct ChatRequest {
    model: String,
    messages: Vec<ChatMessage>,
    temperature: f32,
    stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<ToolDefinition>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    chat_template_kwargs: Option<ChatTemplateKwargs>,
}

#[derive(Debug, Serialize)]
struct ResponsesRequest {
    model: String,
    input: Vec<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    instructions: Option<String>,
    stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<Value>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning: Option<ResponsesReasoning>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
}

#[derive(Debug, Serialize)]
struct ResponsesReasoning {
    #[serde(skip_serializing_if = "Option::is_none")]
    effort: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    summary: Option<&'static str>,
}

#[derive(Debug, Serialize)]
struct AnthropicRequest {
    model: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    system: Option<String>,
    messages: Vec<AnthropicMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<AnthropicTool>>,
    stream: bool,
    max_tokens: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
}

#[derive(Debug, Serialize)]
struct AnthropicMessage {
    role: String,
    content: Vec<AnthropicContentBlock>,
}

#[derive(Debug, Serialize)]
#[serde(tag = "type")]
enum AnthropicContentBlock {
    #[serde(rename = "text")]
    Text { text: String },
    #[serde(rename = "image")]
    Image { source: AnthropicImageSource },
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
}

#[derive(Debug, Serialize)]
struct AnthropicImageSource {
    #[serde(rename = "type")]
    kind: &'static str,
    media_type: String,
    data: String,
}

#[derive(Debug, Serialize)]
struct AnthropicTool {
    name: String,
    description: String,
    input_schema: Value,
}

#[derive(Debug, Serialize)]
struct ChatTemplateKwargs {
    enable_thinking: bool,
}

fn taotoken_glm_chat_template_kwargs(provider: &ProviderConfig) -> Option<ChatTemplateKwargs> {
    let base_url = provider.base_url.to_ascii_lowercase();
    let model = provider.default_model.to_ascii_lowercase();
    if base_url.contains("taotoken.net") && model.starts_with("glm") {
        Some(ChatTemplateKwargs {
            enable_thinking: true,
        })
    } else {
        None
    }
}

fn lower_responses_messages(messages: Vec<ChatMessage>) -> Vec<Value> {
    messages
        .into_iter()
        .flat_map(|message| match message.role.as_str() {
            "system" => vec![json!({"role": "system", "content": chat_content_text(message.content)})],
            "user" => vec![json!({"role": "user", "content": lower_responses_user_content(message.content)})],
            "assistant" => lower_responses_assistant_message(message),
            "tool" => vec![json!({"type": "function_call_output", "call_id": message.tool_call_id.unwrap_or_default(), "output": chat_content_text(message.content)})],
            role => vec![json!({"role": role, "content": chat_content_text(message.content)})],
        })
        .collect()
}

fn lower_responses_assistant_message(message: ChatMessage) -> Vec<Value> {
    let mut items = Vec::new();
    let text = chat_content_text(message.content);
    if !text.trim().is_empty() {
        items
            .push(json!({"role": "assistant", "content": [{"type": "output_text", "text": text}]}));
    }
    if let Some(tool_calls) = message.tool_calls {
        items.extend(tool_calls.into_iter().map(|call| {
            json!({
                "type": "function_call",
                "call_id": call.id,
                "name": call.function.name,
                "arguments": call.function.arguments,
            })
        }));
    }
    items
}

fn lower_responses_user_content(content: Option<super::ChatContent>) -> Vec<Value> {
    match content {
        Some(super::ChatContent::Parts(parts)) => parts
            .into_iter()
            .map(|part| match part {
                super::ChatContentPart::Text { text } => {
                    json!({"type": "input_text", "text": text})
                }
                super::ChatContentPart::ImageUrl { image_url } => {
                    json!({"type": "input_image", "image_url": image_url.url})
                }
            })
            .collect(),
        Some(super::ChatContent::Text(text)) => vec![json!({"type": "input_text", "text": text})],
        None => vec![json!({"type": "input_text", "text": ""})],
    }
}

fn chat_content_text(content: Option<super::ChatContent>) -> String {
    match content {
        Some(super::ChatContent::Text(text)) => text,
        Some(super::ChatContent::Parts(parts)) => parts
            .into_iter()
            .filter_map(|part| match part {
                super::ChatContentPart::Text { text } => Some(text),
                super::ChatContentPart::ImageUrl { .. } => None,
            })
            .collect::<Vec<_>>()
            .join(""),
        None => String::new(),
    }
}

fn lower_responses_tools(tools: Vec<ToolDefinition>) -> Vec<Value> {
    tools
        .into_iter()
        .map(|tool| {
            json!({
                "type": "function",
                "name": tool.function.name,
                "description": tool.function.description,
                "parameters": openai_tool_input_schema(tool.function.parameters),
                "strict": false,
            })
        })
        .collect()
}

fn lower_anthropic_system(messages: &[ChatMessage]) -> Option<String> {
    messages
        .iter()
        .take_while(|message| message.role == "system")
        .map(|message| chat_content_text_ref(message.content.as_ref()))
        .filter(|text| !text.trim().is_empty())
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_string()
        .into_non_empty()
}

fn lower_anthropic_messages(messages: Vec<ChatMessage>) -> Vec<AnthropicMessage> {
    let mut output = Vec::new();
    let mut skipped_initial_system = true;
    for message in messages {
        if skipped_initial_system && message.role == "system" {
            continue;
        }
        skipped_initial_system = false;
        match message.role.as_str() {
            "user" => output.push(AnthropicMessage {
                role: "user".to_string(),
                content: lower_anthropic_user_content(message.content),
            }),
            "assistant" => output.push(AnthropicMessage {
                role: "assistant".to_string(),
                content: lower_anthropic_assistant_content(message),
            }),
            "tool" => output.push(AnthropicMessage {
                role: "user".to_string(),
                content: vec![AnthropicContentBlock::ToolResult {
                    tool_use_id: message.tool_call_id.unwrap_or_default(),
                    content: chat_content_text(message.content),
                }],
            }),
            "system" => output.push(AnthropicMessage {
                role: "user".to_string(),
                content: vec![AnthropicContentBlock::Text {
                    text: wrap_system_update(chat_content_text(message.content)),
                }],
            }),
            _ => output.push(AnthropicMessage {
                role: "user".to_string(),
                content: vec![AnthropicContentBlock::Text {
                    text: chat_content_text(message.content),
                }],
            }),
        }
    }
    output
}

fn lower_anthropic_user_content(content: Option<super::ChatContent>) -> Vec<AnthropicContentBlock> {
    match content {
        Some(super::ChatContent::Parts(parts)) => parts
            .into_iter()
            .filter_map(|part| match part {
                super::ChatContentPart::Text { text } => Some(AnthropicContentBlock::Text { text }),
                super::ChatContentPart::ImageUrl { image_url } => {
                    lower_anthropic_image_url(&image_url.url)
                }
            })
            .collect(),
        Some(super::ChatContent::Text(text)) => vec![AnthropicContentBlock::Text { text }],
        None => vec![AnthropicContentBlock::Text {
            text: String::new(),
        }],
    }
}

fn lower_anthropic_image_url(url: &str) -> Option<AnthropicContentBlock> {
    let data = url.strip_prefix("data:")?;
    let (media_type, base64) = data.split_once(";base64,")?;
    Some(AnthropicContentBlock::Image {
        source: AnthropicImageSource {
            kind: "base64",
            media_type: media_type.to_string(),
            data: base64.to_string(),
        },
    })
}

fn lower_anthropic_assistant_content(message: ChatMessage) -> Vec<AnthropicContentBlock> {
    let mut content = Vec::new();
    let text = chat_content_text(message.content);
    if !text.trim().is_empty() {
        content.push(AnthropicContentBlock::Text { text });
    }
    if let Some(tool_calls) = message.tool_calls {
        content.extend(
            tool_calls
                .into_iter()
                .map(|call| AnthropicContentBlock::ToolUse {
                    id: call.id,
                    name: call.function.name,
                    input: serde_json::from_str(&call.function.arguments)
                        .unwrap_or_else(|_| json!({})),
                }),
        );
    }
    if content.is_empty() {
        content.push(AnthropicContentBlock::Text {
            text: String::new(),
        });
    }
    content
}

fn lower_anthropic_tools(tools: Vec<ToolDefinition>) -> Vec<AnthropicTool> {
    tools
        .into_iter()
        .map(|tool| AnthropicTool {
            name: tool.function.name,
            description: tool.function.description,
            input_schema: tool.function.parameters,
        })
        .collect()
}

fn wrap_system_update(text: String) -> String {
    format!(
        "<system-update>\n{}\n</system-update>",
        text.replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
    )
}

trait IntoNonEmpty {
    fn into_non_empty(self) -> Option<String>;
}

impl IntoNonEmpty for String {
    fn into_non_empty(self) -> Option<String> {
        (!self.trim().is_empty()).then_some(self)
    }
}

fn chat_content_text_ref(content: Option<&super::ChatContent>) -> String {
    match content {
        Some(super::ChatContent::Text(text)) => text.clone(),
        Some(super::ChatContent::Parts(parts)) => parts
            .iter()
            .filter_map(|part| match part {
                super::ChatContentPart::Text { text } => Some(text.clone()),
                super::ChatContentPart::ImageUrl { .. } => None,
            })
            .collect::<Vec<_>>()
            .join(""),
        None => String::new(),
    }
}

fn openai_tool_input_schema(schema: Value) -> Value {
    let flattened = flatten_top_level_any_of(schema);
    let normalized = remove_null_any_of(flattened);
    if normalized.is_object() {
        normalized
    } else {
        json!({"type": "object"})
    }
}

fn flatten_top_level_any_of(schema: Value) -> Value {
    let Some(object) = schema.as_object() else {
        return json!({"type": "object"});
    };
    let Some(variants) = object.get("anyOf").and_then(Value::as_array) else {
        let mut cloned = object.clone();
        cloned.insert("type".to_string(), Value::String("object".to_string()));
        return Value::Object(cloned);
    };
    let mut properties = serde_json::Map::new();
    for variant in variants.iter().filter_map(Value::as_object) {
        if let Some(variant_properties) = variant.get("properties").and_then(Value::as_object) {
            for (key, value) in variant_properties {
                properties.insert(key.clone(), value.clone());
            }
        }
    }
    let mut flattened = object
        .iter()
        .filter(|(key, _)| key.as_str() != "anyOf")
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect::<serde_json::Map<_, _>>();
    flattened.insert("type".to_string(), Value::String("object".to_string()));
    flattened.insert("properties".to_string(), Value::Object(properties));
    flattened.insert("additionalProperties".to_string(), Value::Bool(false));
    Value::Object(flattened)
}

fn remove_null_any_of(value: Value) -> Value {
    match value {
        Value::Array(items) => Value::Array(items.into_iter().map(remove_null_any_of).collect()),
        Value::Object(mut object) => {
            let any_of = object.remove("anyOf");
            let mut object = object
                .into_iter()
                .map(|(key, value)| (key, remove_null_any_of(value)))
                .collect::<serde_json::Map<_, _>>();
            let Some(Value::Array(variants)) = any_of else {
                return Value::Object(object);
            };
            let variants = variants
                .into_iter()
                .filter(|variant| variant.get("type").and_then(Value::as_str) != Some("null"))
                .map(remove_null_any_of)
                .collect::<Vec<_>>();
            if variants.len() == 1 {
                if let Some(variant_object) =
                    variants.first().and_then(|item| item.as_object().cloned())
                {
                    object.extend(variant_object);
                    return Value::Object(object);
                }
            }
            object.insert("anyOf".to_string(), Value::Array(variants));
            Value::Object(object)
        }
        value => value,
    }
}

#[derive(Debug, Deserialize)]
struct ChatStreamResponse {
    #[serde(default, deserialize_with = "null_as_default")]
    choices: Vec<ChatStreamChoice>,
    #[serde(default, deserialize_with = "null_as_default")]
    usage: Option<Usage>,
}

#[derive(Debug, Deserialize)]
struct ChatStreamChoice {
    #[serde(default)]
    delta: ChatChoiceMessage,
}

#[derive(Debug, Default, Deserialize)]
struct ChatChoiceMessage {
    #[serde(default, deserialize_with = "null_as_default")]
    content: Option<String>,
    #[serde(default, deserialize_with = "null_as_default")]
    reasoning_content: Option<String>,
    #[serde(default, deserialize_with = "null_as_default")]
    reasoning: Option<String>,
    #[serde(default, deserialize_with = "null_as_default")]
    thinking: Option<String>,
    #[serde(default, deserialize_with = "null_as_default")]
    thinking_content: Option<String>,
    #[serde(default, deserialize_with = "null_as_default")]
    reasoning_text: Option<String>,
    #[serde(default, deserialize_with = "null_as_default")]
    reasoning_details: Option<serde_json::Value>,
    #[serde(default, deserialize_with = "null_as_default")]
    tool_calls: Vec<ToolCallDelta>,
}

fn null_as_default<'de, D, T>(deserializer: D) -> std::result::Result<T, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Default + Deserialize<'de>,
{
    Ok(Option::<T>::deserialize(deserializer)?.unwrap_or_default())
}

#[derive(Debug, Default, Deserialize)]
struct ToolCallDelta {
    index: usize,
    #[serde(default, deserialize_with = "null_as_default")]
    id: Option<String>,
    #[serde(rename = "type", default, deserialize_with = "null_as_default")]
    kind: Option<String>,
    #[serde(default)]
    function: ToolCallFunctionDelta,
}

#[derive(Debug, Default, Deserialize)]
struct ToolCallFunctionDelta {
    #[serde(default, deserialize_with = "null_as_default")]
    name: Option<String>,
    #[serde(default, deserialize_with = "null_as_default")]
    arguments: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ResponsesStreamEvent {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default, deserialize_with = "null_as_default")]
    delta: Option<String>,
    #[serde(default, deserialize_with = "null_as_default")]
    item_id: Option<String>,
    #[serde(default, deserialize_with = "null_as_default")]
    item: Option<ResponsesStreamItem>,
    #[serde(default, deserialize_with = "null_as_default")]
    response: Option<ResponsesStreamResponse>,
}

#[derive(Debug, Deserialize)]
struct ResponsesStreamItem {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default, deserialize_with = "null_as_default")]
    id: Option<String>,
    #[serde(default, deserialize_with = "null_as_default")]
    call_id: Option<String>,
    #[serde(default, deserialize_with = "null_as_default")]
    name: Option<String>,
    #[serde(default, deserialize_with = "null_as_default")]
    arguments: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ResponsesStreamResponse {
    #[serde(default, deserialize_with = "null_as_default")]
    usage: Option<ResponsesUsage>,
}

#[derive(Debug, Deserialize)]
struct ResponsesUsage {
    #[serde(default)]
    input_tokens: u64,
    #[serde(default)]
    output_tokens: u64,
    #[serde(default)]
    total_tokens: u64,
}

#[derive(Debug, Deserialize)]
struct AnthropicStreamEvent {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default, deserialize_with = "null_as_default")]
    index: Option<usize>,
    #[serde(default, deserialize_with = "null_as_default")]
    message: Option<AnthropicStreamMessage>,
    #[serde(default, deserialize_with = "null_as_default")]
    content_block: Option<AnthropicStreamBlock>,
    #[serde(default, deserialize_with = "null_as_default")]
    delta: Option<AnthropicStreamDelta>,
    #[serde(default, deserialize_with = "null_as_default")]
    usage: Option<AnthropicUsage>,
    #[serde(default, deserialize_with = "null_as_default")]
    error: Option<AnthropicStreamError>,
}

#[derive(Debug, Deserialize)]
struct AnthropicStreamMessage {
    #[serde(default, deserialize_with = "null_as_default")]
    usage: Option<AnthropicUsage>,
}

#[derive(Debug, Deserialize)]
struct AnthropicStreamBlock {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default, deserialize_with = "null_as_default")]
    id: Option<String>,
    #[serde(default, deserialize_with = "null_as_default")]
    name: Option<String>,
    #[serde(default, deserialize_with = "null_as_default")]
    text: Option<String>,
    #[serde(default, deserialize_with = "null_as_default")]
    thinking: Option<String>,
}

#[derive(Debug, Deserialize)]
struct AnthropicStreamDelta {
    #[serde(rename = "type", default, deserialize_with = "null_as_default")]
    kind: Option<String>,
    #[serde(default, deserialize_with = "null_as_default")]
    text: Option<String>,
    #[serde(default, deserialize_with = "null_as_default")]
    thinking: Option<String>,
    #[serde(default, deserialize_with = "null_as_default")]
    partial_json: Option<String>,
}

#[derive(Debug, Deserialize)]
struct AnthropicUsage {
    #[serde(default)]
    input_tokens: u64,
    #[serde(default)]
    output_tokens: u64,
}

#[derive(Debug, Deserialize)]
struct AnthropicStreamError {
    #[serde(rename = "type", default, deserialize_with = "null_as_default")]
    kind: Option<String>,
    #[serde(default, deserialize_with = "null_as_default")]
    message: Option<String>,
}

#[derive(Default)]
struct AnthropicStreamState {
    content: String,
    content_emitted: usize,
    reasoning: String,
    reasoning_emitted: usize,
    usage: Option<Usage>,
    tool_calls: AnthropicToolAccumulator,
}

#[derive(Debug, Default)]
struct AnthropicToolAccumulator {
    calls: Vec<PartialToolCall>,
}

impl AnthropicToolAccumulator {
    fn start(&mut self, index: usize, block: AnthropicStreamBlock) {
        while self.calls.len() <= index {
            self.calls.push(PartialToolCall::default());
        }
        let call = &mut self.calls[index];
        call.id = block.id.unwrap_or_else(|| format!("tool-{index}"));
        call.kind = "function".to_string();
        call.name = block.name.unwrap_or_default();
    }

    fn append_arguments(&mut self, index: usize, text: String) {
        while self.calls.len() <= index {
            self.calls.push(PartialToolCall::default());
        }
        self.calls[index].arguments.push_str(&text);
    }

    fn finish(self) -> Vec<ToolCall> {
        self.calls
            .into_iter()
            .filter(|call| !call.name.trim().is_empty())
            .map(|call| ToolCall {
                id: call.id,
                kind: if call.kind.is_empty() {
                    "function".to_string()
                } else {
                    call.kind
                },
                function: ToolCallFunction {
                    name: call.name,
                    arguments: call.arguments,
                },
            })
            .collect()
    }
}

#[derive(Debug, Default)]
struct ResponsesToolAccumulator {
    calls: Vec<PartialToolCall>,
}

impl ResponsesToolAccumulator {
    fn start(&mut self, item: ResponsesStreamItem) {
        if item.kind != "function_call" {
            return;
        }
        self.calls.push(PartialToolCall {
            id: item.call_id.or(item.id).unwrap_or_default(),
            kind: "function".to_string(),
            name: item.name.unwrap_or_default(),
            arguments: item.arguments.unwrap_or_default(),
        });
    }

    fn append_arguments(&mut self, item_id: Option<String>, delta: String) {
        if let Some(item_id) = item_id {
            if let Some(call) = self
                .calls
                .iter_mut()
                .find(|call| call.id == item_id || call.id.is_empty())
            {
                call.arguments.push_str(&delta);
                return;
            }
        }
        if let Some(call) = self.calls.last_mut() {
            call.arguments.push_str(&delta);
        }
    }

    fn finish_item(&mut self, item: ResponsesStreamItem) {
        if item.kind != "function_call" {
            return;
        }
        let id = item.call_id.or(item.id).unwrap_or_default();
        if let Some(call) = self.calls.iter_mut().find(|call| call.id == id) {
            if let Some(name) = item.name {
                call.name = name;
            }
            if let Some(arguments) = item.arguments {
                call.arguments = arguments;
            }
        } else {
            self.start(ResponsesStreamItem {
                kind: "function_call".to_string(),
                id: None,
                call_id: Some(id),
                name: item.name,
                arguments: item.arguments,
            });
        }
    }

    fn finish(self) -> Vec<ToolCall> {
        self.calls
            .into_iter()
            .filter(|call| !call.name.trim().is_empty())
            .map(|call| ToolCall {
                id: call.id,
                kind: call.kind,
                function: ToolCallFunction {
                    name: call.name,
                    arguments: call.arguments,
                },
            })
            .collect()
    }
}

#[derive(Debug, Default)]
struct ToolCallAccumulator {
    calls: Vec<PartialToolCall>,
}

#[derive(Debug, Default)]
struct PartialToolCall {
    id: String,
    kind: String,
    name: String,
    arguments: String,
}

impl ToolCallAccumulator {
    fn push(&mut self, delta: ToolCallDelta) {
        while self.calls.len() <= delta.index {
            self.calls.push(PartialToolCall::default());
        }
        let call = &mut self.calls[delta.index];
        if let Some(id) = delta.id {
            call.id = id;
        }
        if let Some(kind) = delta.kind {
            call.kind = kind;
        }
        if let Some(name) = delta.function.name {
            call.name.push_str(&name);
        }
        if let Some(arguments) = delta.function.arguments {
            call.arguments.push_str(&arguments);
        }
    }

    fn finish(self) -> Vec<ToolCall> {
        self.calls
            .into_iter()
            .filter(|call| !call.name.trim().is_empty())
            .map(|call| ToolCall {
                id: call.id,
                kind: if call.kind.is_empty() {
                    "function".to_string()
                } else {
                    call.kind
                },
                function: ToolCallFunction {
                    name: call.name,
                    arguments: call.arguments,
                },
            })
            .collect()
    }
}

#[derive(Default)]
struct SseBuffer {
    buffer: String,
    data_lines: Vec<String>,
}

impl SseBuffer {
    fn push(&mut self, text: &str) -> Result<Vec<String>> {
        self.buffer.push_str(text);
        let mut events = Vec::new();
        while let Some(index) = self.buffer.find('\n') {
            let line = self.buffer[..index].trim_end_matches('\r').to_string();
            self.buffer.drain(..=index);
            if let Some(event) = self.push_line(&line) {
                events.push(event);
            }
        }
        Ok(events)
    }

    fn finish(mut self) -> Result<Vec<String>> {
        let mut events = Vec::new();
        if !self.buffer.trim().is_empty() {
            let line = self.buffer.trim_end_matches('\r').to_string();
            if let Some(event) = self.push_line(&line) {
                events.push(event);
            }
        }
        if !self.data_lines.is_empty() {
            events.push(self.data_lines.join("\n"));
        }
        Ok(events)
    }

    fn push_line(&mut self, line: &str) -> Option<String> {
        if line.is_empty() {
            if self.data_lines.is_empty() {
                return None;
            }
            return Some(std::mem::take(&mut self.data_lines).join("\n"));
        }
        if let Some(data) = line.strip_prefix("data:") {
            self.data_lines.push(data.trim_start().to_string());
        }
        None
    }
}

fn clean_response_content(content: String) -> (String, Option<String>) {
    split_tagged_reasoning(clean_plain_text(content))
}

fn split_tagged_reasoning(content: String) -> (String, Option<String>) {
    match split_tag_pair(content, "think").or_else(|content| split_tag_pair(content, "thinking")) {
        Ok(result) => result,
        Err(content) => (content, None),
    }
}

fn split_tag_pair(
    content: String,
    tag: &str,
) -> std::result::Result<(String, Option<String>), String> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let Some(start) = content.find(&open) else {
        return Err(content);
    };
    let reasoning_start = start + open.len();
    let Some(relative_end) = content[reasoning_start..].find(&close) else {
        return Ok((content, None));
    };
    let end = reasoning_start + relative_end;
    let reasoning = content[reasoning_start..end].trim().to_string();
    let mut visible = String::new();
    visible.push_str(content[..start].trim_end());
    visible.push_str(content[end + close.len()..].trim_start());
    Ok((
        visible.trim().to_string(),
        (!reasoning.is_empty()).then_some(reasoning),
    ))
}

#[allow(clippy::too_many_arguments)]
fn handle_sse_line<F>(
    line: &str,
    content: &mut String,
    content_emitted: &mut usize,
    reasoning: &mut String,
    reasoning_emitted: &mut usize,
    usage: &mut Option<Usage>,
    tool_calls: &mut ToolCallAccumulator,
    on_chunk: &mut F,
) -> Result<Option<bool>>
where
    F: FnMut(ChatStreamChunk) -> Result<()>,
{
    let Some(data) = line.strip_prefix("data:").map(str::trim) else {
        return Ok(None);
    };
    if data == "[DONE]" {
        flush_buffer(
            content,
            content_emitted,
            ChatStreamKind::Content,
            on_chunk,
            true,
        )?;
        flush_buffer(
            reasoning,
            reasoning_emitted,
            ChatStreamKind::Reasoning,
            on_chunk,
            true,
        )?;
        return Ok(Some(true));
    }
    let response: ChatStreamResponse = serde_json::from_str(data).with_context(|| {
        format!(
            "{}: {}",
            t(
                "invalid chat completions stream response",
                "无效的聊天流式响应",
            ),
            clean_plain_text(data.to_string())
        )
    })?;
    if let Some(next_usage) = response.usage {
        *usage = Some(next_usage);
    }
    for choice in response.choices {
        let delta = choice.delta;
        if let Some(text) = delta_reasoning_text(&delta) {
            push_buffered_chunk(
                reasoning,
                reasoning_emitted,
                ChatStreamKind::Reasoning,
                text,
                on_chunk,
            )?;
        }
        if let Some(text) = delta.content {
            push_buffered_chunk(
                content,
                content_emitted,
                ChatStreamKind::Content,
                text,
                on_chunk,
            )?;
        }
        for tool_call in delta.tool_calls {
            tool_calls.push(tool_call);
        }
    }
    Ok(Some(false))
}

#[allow(clippy::too_many_arguments)]
fn handle_responses_sse_line<F>(
    line: &str,
    content: &mut String,
    content_emitted: &mut usize,
    reasoning: &mut String,
    reasoning_emitted: &mut usize,
    usage: &mut Option<Usage>,
    content_started: &mut bool,
    tool_calls: &mut ResponsesToolAccumulator,
    on_chunk: &mut F,
) -> Result<bool>
where
    F: FnMut(ChatStreamChunk) -> Result<()>,
{
    let Some(data) = line.strip_prefix("data:").map(str::trim) else {
        return Ok(false);
    };
    if data == "[DONE]" {
        flush_buffer(
            content,
            content_emitted,
            ChatStreamKind::Content,
            on_chunk,
            true,
        )?;
        flush_buffer(
            reasoning,
            reasoning_emitted,
            ChatStreamKind::Reasoning,
            on_chunk,
            true,
        )?;
        return Ok(true);
    }
    let event: ResponsesStreamEvent = serde_json::from_str(data).with_context(|| {
        format!(
            "{}: {}",
            t(
                "invalid responses stream event",
                "无效的 Responses 流式事件"
            ),
            clean_plain_text(data.to_string())
        )
    })?;
    match event.kind.as_str() {
        "response.output_text.delta" => {
            if let Some(text) = event.delta {
                *content_started = true;
                push_buffered_chunk(
                    content,
                    content_emitted,
                    ChatStreamKind::Content,
                    text,
                    on_chunk,
                )?;
            }
        }
        "response.reasoning_text.delta"
        | "response.reasoning_summary.delta"
        | "response.reasoning_summary_text.delta" => {
            if let Some(text) = event.delta {
                push_buffered_chunk(
                    reasoning,
                    reasoning_emitted,
                    ChatStreamKind::Reasoning,
                    text,
                    on_chunk,
                )?;
            }
        }
        "response.reasoning_text.done"
        | "response.reasoning_summary.done"
        | "response.reasoning_summary_text.done"
            if !*content_started && !reasoning.trim().is_empty() =>
        {
            flush_buffer(
                reasoning,
                reasoning_emitted,
                ChatStreamKind::Reasoning,
                on_chunk,
                true,
            )?;
            *content_started = true;
            on_chunk(ChatStreamChunk {
                kind: ChatStreamKind::Content,
                text: String::new(),
            })?;
        }
        "response.output_item.added" => {
            if let Some(item) = event.item {
                tool_calls.start(item);
            }
        }
        "response.function_call_arguments.delta" => {
            if let Some(delta) = event.delta {
                tool_calls.append_arguments(event.item_id, delta);
            }
        }
        "response.output_item.done" => {
            if let Some(item) = event.item {
                tool_calls.finish_item(item);
            }
        }
        "response.completed" | "response.incomplete" => {
            if let Some(next_usage) = event.response.and_then(|response| response.usage) {
                *usage = Some(Usage {
                    prompt_tokens: next_usage.input_tokens,
                    completion_tokens: next_usage.output_tokens,
                    total_tokens: next_usage.total_tokens,
                });
            }
            flush_buffer(
                content,
                content_emitted,
                ChatStreamKind::Content,
                on_chunk,
                true,
            )?;
            flush_buffer(
                reasoning,
                reasoning_emitted,
                ChatStreamKind::Reasoning,
                on_chunk,
                true,
            )?;
            return Ok(true);
        }
        "error" | "response.failed" => {
            bail!(
                "OpenAI Responses stream failed: {}",
                clean_plain_text(data.to_string())
            );
        }
        _ => {}
    }
    Ok(false)
}

fn handle_anthropic_sse_data<F>(
    data: &str,
    state: &mut AnthropicStreamState,
    on_chunk: &mut F,
) -> Result<bool>
where
    F: FnMut(ChatStreamChunk) -> Result<()>,
{
    if data == "[DONE]" {
        flush_anthropic_state(state, on_chunk)?;
        return Ok(true);
    }
    let event: AnthropicStreamEvent = serde_json::from_str(data).with_context(|| {
        format!(
            "{}: {}",
            t(
                "invalid anthropic messages stream event",
                "无效的 Anthropic Messages 流式事件"
            ),
            clean_plain_text(data.to_string())
        )
    })?;
    match event.kind.as_str() {
        "message_start" => {
            if let Some(usage) = event.message.and_then(|message| message.usage) {
                state.usage = Some(map_anthropic_usage(usage));
            }
        }
        "content_block_start" => {
            if let Some(block) = event.content_block {
                match block.kind.as_str() {
                    "tool_use" | "server_tool_use" => {
                        if let Some(index) = event.index {
                            state.tool_calls.start(index, block);
                        }
                    }
                    "text" => {
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
                    _ => {}
                }
            }
        }
        "content_block_delta" => {
            if let Some(delta) = event.delta {
                match delta.kind.as_deref() {
                    Some("text_delta") => {
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
                            push_buffered_chunk(
                                &mut state.reasoning,
                                &mut state.reasoning_emitted,
                                ChatStreamKind::Reasoning,
                                text,
                                on_chunk,
                            )?;
                        }
                    }
                    Some("input_json_delta") => {
                        if let (Some(index), Some(text)) = (event.index, delta.partial_json) {
                            state.tool_calls.append_arguments(index, text);
                        }
                    }
                    _ => {}
                }
            }
        }
        "message_delta" => {
            if let Some(usage) = event.usage {
                state.usage = Some(map_anthropic_usage(usage));
            }
            flush_anthropic_state(state, on_chunk)?;
            return Ok(true);
        }
        "error" => {
            let message = event
                .error
                .map(|error| match (error.kind, error.message) {
                    (Some(kind), Some(message)) => format!("{kind}: {message}"),
                    (Some(kind), None) => kind,
                    (None, Some(message)) => message,
                    (None, None) => "Anthropic Messages stream error".to_string(),
                })
                .unwrap_or_else(|| "Anthropic Messages stream error".to_string());
            bail!("{message}");
        }
        _ => {}
    }
    Ok(false)
}

fn flush_anthropic_state<F>(state: &mut AnthropicStreamState, on_chunk: &mut F) -> Result<()>
where
    F: FnMut(ChatStreamChunk) -> Result<()>,
{
    flush_buffer(
        &state.content,
        &mut state.content_emitted,
        ChatStreamKind::Content,
        on_chunk,
        true,
    )?;
    flush_buffer(
        &state.reasoning,
        &mut state.reasoning_emitted,
        ChatStreamKind::Reasoning,
        on_chunk,
        true,
    )
}

fn map_anthropic_usage(usage: AnthropicUsage) -> Usage {
    Usage {
        prompt_tokens: usage.input_tokens,
        completion_tokens: usage.output_tokens,
        total_tokens: usage.input_tokens + usage.output_tokens,
    }
}

fn delta_reasoning_text(delta: &ChatChoiceMessage) -> Option<String> {
    delta
        .reasoning_content
        .clone()
        .or_else(|| delta.reasoning.clone())
        .or_else(|| delta.thinking.clone())
        .or_else(|| delta.thinking_content.clone())
        .or_else(|| delta.reasoning_text.clone())
        .or_else(|| reasoning_details_text(delta.reasoning_details.as_ref()))
}

fn reasoning_details_text(value: Option<&serde_json::Value>) -> Option<String> {
    let value = value?;
    if let Some(text) = value.as_str() {
        return Some(text.to_string());
    }
    if let Some(array) = value.as_array() {
        let text = array
            .iter()
            .filter_map(|item| {
                item.get("text")
                    .or_else(|| item.get("content"))
                    .and_then(serde_json::Value::as_str)
            })
            .collect::<Vec<_>>()
            .join("");
        return (!text.is_empty()).then_some(text);
    }
    value
        .get("text")
        .or_else(|| value.get("content"))
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
}

fn push_buffered_chunk<F>(
    target: &mut String,
    emitted: &mut usize,
    kind: ChatStreamKind,
    text: String,
    on_chunk: &mut F,
) -> Result<()>
where
    F: FnMut(ChatStreamChunk) -> Result<()>,
{
    if text.is_empty() {
        return Ok(());
    }
    target.push_str(&text);
    flush_buffer(target, emitted, kind, on_chunk, false)
}

fn flush_buffer<F>(
    target: &str,
    emitted: &mut usize,
    kind: ChatStreamKind,
    on_chunk: &mut F,
    final_flush: bool,
) -> Result<()>
where
    F: FnMut(ChatStreamChunk) -> Result<()>,
{
    while *emitted < target.len() {
        let remaining = &target[*emitted..];
        if starts_hidden_prefix(remaining) {
            if let Some(end) = hidden_end_after(target, *emitted) {
                *emitted = end;
                continue;
            }
            if final_flush {
                *emitted = target.len();
            }
            return Ok(());
        }
        let hidden_start = hidden_start_after(target, *emitted);
        let mut safe_end = hidden_start.unwrap_or(target.len());
        if hidden_start.is_none() && !final_flush {
            safe_end =
                safe_end.saturating_sub(partial_hidden_suffix_len(&target[*emitted..safe_end]));
        }
        if safe_end <= *emitted {
            return Ok(());
        }
        let text = target[*emitted..safe_end].to_string();
        *emitted = safe_end;
        if !text.is_empty() {
            on_chunk(ChatStreamChunk { kind, text })?;
        }
    }
    Ok(())
}

fn finalize_stream_result(
    content: String,
    reasoning: String,
    usage: Option<Usage>,
    tool_calls: Vec<ToolCall>,
) -> Result<ChatResult> {
    let content = clean_plain_text(content);
    let (content, mut dsml_tool_calls) = extract_dsml_tool_calls(content);
    let reasoning = clean_plain_text(reasoning);
    let (reasoning, reasoning_dsml_tool_calls) = extract_dsml_tool_calls(reasoning);
    dsml_tool_calls.extend(reasoning_dsml_tool_calls);
    let (content, tag_reasoning) = clean_response_content(content);
    let reasoning = if reasoning.trim().is_empty() {
        tag_reasoning
    } else {
        Some(reasoning)
    };
    let tool_calls = if dsml_tool_calls.is_empty() {
        tool_calls
    } else {
        dsml_tool_calls
    };
    if content.trim().is_empty() && tool_calls.is_empty() {
        bail!(
            "{}",
            t(
                "chat completions stream response was empty",
                "聊天流式响应为空",
            )
        );
    }
    Ok(ChatResult {
        content,
        reasoning: reasoning.filter(|text| !text.trim().is_empty()),
        usage,
        tool_calls,
    })
}

const DSML_ANY_PREFIX: &str = "<｜｜DSML";
const DSML_PREFIX: &str = "<｜｜DSML｜｜tool_calls";
const DSML_END: &str = "</｜｜DSML｜｜tool_calls>";
const SYSTEM_REMINDER_PREFIX: &str = "<system-reminder";
const SYSTEM_REMINDER_UNDERSCORE_PREFIX: &str = "<system_reminder";

fn hidden_start_after(target: &str, offset: usize) -> Option<usize> {
    [
        target[offset..].find(DSML_ANY_PREFIX),
        target[offset..].find(SYSTEM_REMINDER_PREFIX),
        target[offset..].find(SYSTEM_REMINDER_UNDERSCORE_PREFIX),
    ]
    .into_iter()
    .flatten()
    .map(|index| offset + index)
    .min()
}

fn starts_hidden_prefix(value: &str) -> bool {
    DSML_ANY_PREFIX.starts_with(value)
        || SYSTEM_REMINDER_PREFIX.starts_with(value)
        || SYSTEM_REMINDER_UNDERSCORE_PREFIX.starts_with(value)
        || value.starts_with(DSML_ANY_PREFIX)
        || value.starts_with(SYSTEM_REMINDER_PREFIX)
        || value.starts_with(SYSTEM_REMINDER_UNDERSCORE_PREFIX)
}

fn partial_hidden_suffix_len(value: &str) -> usize {
    let max_len = value.len().min(
        DSML_ANY_PREFIX
            .len()
            .max(SYSTEM_REMINDER_PREFIX.len())
            .max(SYSTEM_REMINDER_UNDERSCORE_PREFIX.len()),
    );
    for len in (1..=max_len).rev() {
        if !value.is_char_boundary(value.len() - len) {
            continue;
        }
        let suffix = &value[value.len() - len..];
        if DSML_ANY_PREFIX.starts_with(suffix)
            || SYSTEM_REMINDER_PREFIX.starts_with(suffix)
            || SYSTEM_REMINDER_UNDERSCORE_PREFIX.starts_with(suffix)
        {
            return len;
        }
    }
    0
}

fn hidden_end_after(target: &str, offset: usize) -> Option<usize> {
    let remaining = &target[offset..];
    if remaining.starts_with(DSML_ANY_PREFIX) {
        return remaining
            .find(DSML_END)
            .map(|index| offset + index + DSML_END.len());
    }
    for tag in ["system-reminder", "system_reminder"] {
        let open_prefix = format!("<{tag}");
        if remaining.starts_with(&open_prefix) {
            let close = format!("</{tag}>");
            return remaining
                .find(&close)
                .map(|index| offset + index + close.len());
        }
    }
    None
}

fn extract_dsml_tool_calls(mut content: String) -> (String, Vec<ToolCall>) {
    let mut calls = Vec::new();
    let mut index = 0usize;
    while let Some(start) = content.find(DSML_PREFIX) {
        let tag_end = content[start..]
            .find('>')
            .map(|offset| start + offset + 1)
            .unwrap_or(start + DSML_PREFIX.len());
        let body_start = tag_end;
        let Some(relative_end) = content[body_start..].find(DSML_END) else {
            content.replace_range(start.., "");
            break;
        };
        let end = body_start + relative_end;
        let block = content[body_start..end].to_string();
        calls.extend(parse_dsml_block(&block, &mut index));
        content.replace_range(start..end + DSML_END.len(), "");
    }
    (content.trim().to_string(), calls)
}

fn parse_dsml_block(block: &str, index: &mut usize) -> Vec<ToolCall> {
    let mut calls = Vec::new();
    let mut rest = block;
    while let Some(start) = rest.find("<｜｜DSML｜｜invoke") {
        rest = &rest[start..];
        let Some(tag_end) = rest.find('>') else {
            break;
        };
        let tag = &rest[..tag_end];
        let Some(name) = attr_value(tag, "name") else {
            rest = &rest[tag_end..];
            continue;
        };
        let body_start = tag_end + 1;
        let Some(relative_end) = rest[body_start..].find("</｜｜DSML｜｜invoke>") else {
            break;
        };
        let body = &rest[body_start..body_start + relative_end];
        let arguments = parse_dsml_arguments(body);
        *index += 1;
        calls.push(ToolCall {
            id: format!("dsml-tool-call-{index}"),
            kind: "function".to_string(),
            function: ToolCallFunction {
                name,
                arguments: arguments.to_string(),
            },
        });
        rest = &rest[body_start + relative_end + "</｜｜DSML｜｜invoke>".len()..];
    }
    calls
}

fn parse_dsml_arguments(body: &str) -> serde_json::Value {
    let mut map = serde_json::Map::new();
    let mut rest = body;
    while let Some(start) = rest.find("<｜｜DSML｜｜parameter") {
        rest = &rest[start..];
        let Some(tag_end) = rest.find('>') else {
            break;
        };
        let tag = &rest[..tag_end];
        let Some(name) = attr_value(tag, "name") else {
            rest = &rest[tag_end..];
            continue;
        };
        let value_start = tag_end + 1;
        let Some(relative_end) = rest[value_start..].find("</｜｜DSML｜｜parameter>") else {
            break;
        };
        let raw_value = rest[value_start..value_start + relative_end].trim();
        map.insert(name, parse_dsml_value(raw_value));
        rest = &rest[value_start + relative_end + "</｜｜DSML｜｜parameter>".len()..];
    }
    serde_json::Value::Object(map)
}

fn parse_dsml_value(value: &str) -> serde_json::Value {
    let trimmed = value.trim();
    if let Ok(value) = serde_json::from_str(trimmed) {
        return value;
    }
    if let Ok(value) = trimmed.parse::<i64>() {
        return serde_json::Value::Number(value.into());
    }
    serde_json::Value::String(trimmed.trim_matches('"').to_string())
}

fn attr_value(tag: &str, name: &str) -> Option<String> {
    let pattern = format!("{name}=\"");
    let start = tag.find(&pattern)? + pattern.len();
    let end = tag[start..].find('"')?;
    Some(tag[start..start + end].to_string())
}

fn clean_plain_text(mut text: String) -> String {
    for tag in ["system-reminder", "system_reminder"] {
        text = strip_tagged_sections(text, tag);
    }
    text = text.replace("<system-reminder>", "");
    text = text.replace("</system-reminder>", "");
    text = text.replace("<system_reminder>", "");
    text = text.replace("</system_reminder>", "");
    text
}

#[cfg(test)]
#[allow(clippy::items_after_test_module)]
mod tests {
    use super::*;

    #[test]
    fn stream_chunk_accepts_null_tool_calls() {
        let raw = r#"{"choices":[{"delta":{"content":"在","tool_calls":null}}]}"#;
        let parsed: ChatStreamResponse = serde_json::from_str(raw).unwrap();

        assert_eq!(parsed.choices.len(), 1);
        assert_eq!(parsed.choices[0].delta.content.as_deref(), Some("在"));
        assert!(parsed.choices[0].delta.tool_calls.is_empty());
    }

    #[test]
    fn stream_chunk_accepts_taotoken_glm_nulls() {
        let raw = r#"{"created":1782742568,"usage":null,"model":"glm_for_coding","id":"9981f6121a31494387131c61bd2ad7a2","choices":[{"finish_reason":null,"matched_stop":null,"delta":{"role":null,"tool_calls":null,"content":"在","reasoning_content":null},"index":0,"logprobs":null}],"object":"chat.completion.chunk"}"#;
        let parsed: ChatStreamResponse = serde_json::from_str(raw).unwrap();

        assert!(parsed.usage.is_none());
        assert_eq!(parsed.choices.len(), 1);
        assert_eq!(parsed.choices[0].delta.content.as_deref(), Some("在"));
        assert!(parsed.choices[0].delta.reasoning_content.is_none());
        assert!(parsed.choices[0].delta.tool_calls.is_empty());
    }

    #[test]
    fn stream_chunk_emits_glm_reasoning_content() {
        let mut content = String::new();
        let mut content_emitted = 0usize;
        let mut reasoning = String::new();
        let mut reasoning_emitted = 0usize;
        let mut usage = None;
        let mut tool_calls = ToolCallAccumulator::default();
        let mut chunks = Vec::new();
        let mut on_chunk = |chunk| {
            chunks.push(chunk);
            Ok(())
        };

        handle_sse_line(
            r#"data: {"choices":[{"delta":{"reasoning_content":"先想一下","content":null,"tool_calls":null}}]}"#,
            &mut content,
            &mut content_emitted,
            &mut reasoning,
            &mut reasoning_emitted,
            &mut usage,
            &mut tool_calls,
            &mut on_chunk,
        )
        .unwrap();

        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].kind, ChatStreamKind::Reasoning);
        assert_eq!(chunks[0].text, "先想一下");
    }

    #[test]
    fn taotoken_glm_request_enables_thinking() {
        let mut provider = test_provider("taotoken", "https://taotoken.net/api/v1");
        provider.default_model = "glm_for_coding".to_string();

        assert!(taotoken_glm_chat_template_kwargs(&provider)
            .is_some_and(|kwargs| kwargs.enable_thinking));
    }

    #[test]
    fn non_taotoken_glm_request_keeps_default_body() {
        let mut provider = test_provider("local", "http://localhost:11434/v1");
        provider.default_model = "glm-5".to_string();

        assert!(taotoken_glm_chat_template_kwargs(&provider).is_none());
    }

    #[test]
    fn openai_gpt5_uses_responses_api() {
        let mut provider = test_provider("openai", "https://api.openai.com/v1");
        provider.default_model = "gpt-5.5".to_string();
        let client = OpenAiCompatibleClient {
            client: reqwest::Client::new(),
            provider,
            api_key: "test".to_string(),
        };

        assert!(client.uses_openai_responses());
    }

    #[test]
    fn openai_compatible_gpt5_tries_responses_api() {
        let mut provider = test_provider("taotoken", "https://taotoken.net/api/v1");
        provider.default_model = "gpt-5.5".to_string();
        let client = OpenAiCompatibleClient {
            client: reqwest::Client::new(),
            provider,
            api_key: "test".to_string(),
        };

        assert!(client.uses_openai_responses());
    }

    #[test]
    fn responses_unsupported_allows_chat_fallback() {
        assert!(responses_unsupported(404, "not found"));
        assert!(responses_unsupported(400, "unsupported endpoint"));
        assert!(!responses_unsupported(401, "invalid api key"));
    }

    #[test]
    fn openai_tool_schema_flattens_top_level_any_of() {
        let schema = json!({
            "anyOf": [
                {"type":"object","properties":{"path":{"type":"string"}},"required":["path"]},
                {"type":"object","properties":{"resource":{"anyOf":[{"type":"string"},{"type":"null"}]}},"required":["resource"]}
            ]
        });

        let normalized = openai_tool_input_schema(schema);

        assert_eq!(normalized["type"], "object");
        assert_eq!(normalized["additionalProperties"], false);
        assert_eq!(normalized["properties"]["path"]["type"], "string");
        assert_eq!(normalized["properties"]["resource"]["type"], "string");
        assert!(normalized.get("anyOf").is_none());
    }

    #[test]
    fn responses_stream_emits_reasoning_and_content() {
        let mut content = String::new();
        let mut content_emitted = 0usize;
        let mut reasoning = String::new();
        let mut reasoning_emitted = 0usize;
        let mut usage = None;
        let mut content_started = false;
        let mut tool_calls = ResponsesToolAccumulator::default();
        let mut chunks = Vec::new();
        let mut on_chunk = |chunk| {
            chunks.push(chunk);
            Ok(())
        };

        handle_responses_sse_line(
            r#"data: {"type":"response.reasoning_summary_text.delta","item_id":"rs_1","delta":"思考"}"#,
            &mut content,
            &mut content_emitted,
            &mut reasoning,
            &mut reasoning_emitted,
            &mut usage,
            &mut content_started,
            &mut tool_calls,
            &mut on_chunk,
        )
        .unwrap();
        handle_responses_sse_line(
            r#"data: {"type":"response.output_text.delta","item_id":"msg_1","delta":"答案"}"#,
            &mut content,
            &mut content_emitted,
            &mut reasoning,
            &mut reasoning_emitted,
            &mut usage,
            &mut content_started,
            &mut tool_calls,
            &mut on_chunk,
        )
        .unwrap();

        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].kind, ChatStreamKind::Reasoning);
        assert_eq!(chunks[0].text, "思考");
        assert_eq!(chunks[1].kind, ChatStreamKind::Content);
        assert_eq!(chunks[1].text, "答案");
    }

    #[test]
    fn responses_reasoning_done_emits_content_boundary() {
        let mut content = String::new();
        let mut content_emitted = 0usize;
        let mut reasoning = String::new();
        let mut reasoning_emitted = 0usize;
        let mut usage = None;
        let mut content_started = false;
        let mut tool_calls = ResponsesToolAccumulator::default();
        let mut chunks = Vec::new();
        let mut on_chunk = |chunk| {
            chunks.push(chunk);
            Ok(())
        };

        for line in [
            r#"data: {"type":"response.reasoning_summary_text.delta","item_id":"rs_1","delta":"思考"}"#,
            r#"data: {"type":"response.reasoning_summary_text.done","item_id":"rs_1"}"#,
            r#"data: {"type":"response.output_text.delta","item_id":"msg_1","delta":"答案"}"#,
            r#"data: {"type":"response.reasoning_summary_text.delta","item_id":"rs_1","delta":"晚到"}"#,
        ] {
            handle_responses_sse_line(
                line,
                &mut content,
                &mut content_emitted,
                &mut reasoning,
                &mut reasoning_emitted,
                &mut usage,
                &mut content_started,
                &mut tool_calls,
                &mut on_chunk,
            )
            .unwrap();
        }

        assert_eq!(chunks.len(), 4);
        assert_eq!(chunks[0].kind, ChatStreamKind::Reasoning);
        assert_eq!(chunks[0].text, "思考");
        assert_eq!(chunks[1].kind, ChatStreamKind::Content);
        assert!(chunks[1].text.is_empty());
        assert_eq!(chunks[2].kind, ChatStreamKind::Content);
        assert_eq!(chunks[2].text, "答案");
        assert_eq!(chunks[3].kind, ChatStreamKind::Reasoning);
        assert_eq!(chunks[3].text, "晚到");
        assert_eq!(reasoning, "思考晚到");
    }

    #[test]
    fn stream_filter_skips_split_system_reminder() {
        let mut content = String::new();
        let mut emitted = 0usize;
        let mut chunks = Vec::new();
        let mut on_chunk = |chunk| {
            chunks.push(chunk);
            Ok(())
        };

        push_buffered_chunk(
            &mut content,
            &mut emitted,
            ChatStreamKind::Content,
            "hello <system-rem".to_string(),
            &mut on_chunk,
        )
        .unwrap();
        push_buffered_chunk(
            &mut content,
            &mut emitted,
            ChatStreamKind::Content,
            "inder>hidden</system-reminder> world".to_string(),
            &mut on_chunk,
        )
        .unwrap();

        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].text, "hello ");
        assert_eq!(chunks[1].text, " world");
    }

    #[test]
    fn stream_filter_skips_underscore_system_reminder() {
        let mut content = String::new();
        let mut emitted = 0usize;
        let mut chunks = Vec::new();
        let mut on_chunk = |chunk| {
            chunks.push(chunk);
            Ok(())
        };

        push_buffered_chunk(
            &mut content,
            &mut emitted,
            ChatStreamKind::Content,
            "a<system_reminder>hidden</system_reminder>b".to_string(),
            &mut on_chunk,
        )
        .unwrap();

        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].text, "a");
        assert_eq!(chunks[1].text, "b");
    }

    #[test]
    fn responses_stream_collects_tool_calls() {
        let mut content = String::new();
        let mut content_emitted = 0usize;
        let mut reasoning = String::new();
        let mut reasoning_emitted = 0usize;
        let mut usage = None;
        let mut content_started = false;
        let mut tool_calls = ResponsesToolAccumulator::default();
        let mut on_chunk = |_| Ok(());

        for line in [
            r#"data: {"type":"response.output_item.added","item":{"type":"function_call","id":"item_1","call_id":"call_1","name":"calc","arguments":""}}"#,
            r#"data: {"type":"response.function_call_arguments.delta","item_id":"call_1","delta":"{\"x\":"}"#,
            r#"data: {"type":"response.function_call_arguments.delta","item_id":"call_1","delta":"1}"}"#,
            r#"data: {"type":"response.output_item.done","item":{"type":"function_call","id":"item_1","call_id":"call_1","name":"calc","arguments":"{\"x\":1}"}}"#,
        ] {
            handle_responses_sse_line(
                line,
                &mut content,
                &mut content_emitted,
                &mut reasoning,
                &mut reasoning_emitted,
                &mut usage,
                &mut content_started,
                &mut tool_calls,
                &mut on_chunk,
            )
            .unwrap();
        }

        let calls = tool_calls.finish();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].id, "call_1");
        assert_eq!(calls[0].function.name, "calc");
        assert_eq!(calls[0].function.arguments, r#"{"x":1}"#);
    }

    #[test]
    fn protocol_config_accepts_explicit_anthropic() {
        let mut provider = test_provider("anthropic", "https://api.anthropic.com/v1");
        provider.protocol = "anthropic".to_string();

        assert_eq!(
            ProviderProtocol::from_provider(&provider).unwrap(),
            ProviderProtocol::Anthropic
        );
    }

    #[test]
    fn anthropic_stream_emits_reasoning_content_and_usage() {
        let mut state = AnthropicStreamState::default();
        let mut chunks = Vec::new();
        let mut on_chunk = |chunk| {
            chunks.push(chunk);
            Ok(())
        };

        for data in [
            r#"{"type":"message_start","message":{"usage":{"input_tokens":3,"output_tokens":0}}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"想"}}"#,
            r#"{"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"答"}}"#,
            r#"{"type":"message_delta","usage":{"input_tokens":3,"output_tokens":2},"delta":{"stop_reason":"end_turn"}}"#,
        ] {
            let done = handle_anthropic_sse_data(data, &mut state, &mut on_chunk).unwrap();
            if data.contains("message_delta") {
                assert!(done);
            }
        }

        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].kind, ChatStreamKind::Reasoning);
        assert_eq!(chunks[0].text, "想");
        assert_eq!(chunks[1].kind, ChatStreamKind::Content);
        assert_eq!(chunks[1].text, "答");
        let usage = state.usage.unwrap();
        assert_eq!(usage.prompt_tokens, 3);
        assert_eq!(usage.completion_tokens, 2);
        assert_eq!(usage.total_tokens, 5);
    }

    #[test]
    fn anthropic_stream_collects_tool_calls() {
        let mut state = AnthropicStreamState::default();
        let mut on_chunk = |_| Ok(());

        for data in [
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_1","name":"calc","input":{}}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"x\":"}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"1}"}}"#,
        ] {
            handle_anthropic_sse_data(data, &mut state, &mut on_chunk).unwrap();
        }

        let calls = state.tool_calls.finish();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].id, "toolu_1");
        assert_eq!(calls[0].function.name, "calc");
        assert_eq!(calls[0].function.arguments, r#"{"x":1}"#);
    }

    fn test_provider(id: &str, base_url: &str) -> ProviderConfig {
        ProviderConfig {
            id: id.to_string(),
            display_name: id.to_string(),
            base_url: base_url.to_string(),
            protocol: "auto".to_string(),
            api_key: None,
            models: Vec::new(),
            model_context_chars: std::collections::HashMap::new(),
            default_model: String::new(),
            timeout_seconds: 60,
            temperature: 0.7,
        }
    }
}

fn strip_tagged_sections(mut text: String, tag: &str) -> String {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let open_prefix = format!("<{tag}");
    while let Some(start) = text.find(&open_prefix) {
        let content_start = text[start..]
            .find('>')
            .map(|offset| start + offset + 1)
            .unwrap_or(start + open.len());
        let Some(relative_end) = text[content_start..].find(&close) else {
            text.replace_range(start.., "");
            break;
        };
        let end = content_start + relative_end + close.len();
        text.replace_range(start..end, "");
    }
    text
}
