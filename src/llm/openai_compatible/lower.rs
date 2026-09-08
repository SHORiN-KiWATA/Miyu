//! 把内部消息「降」到各协议的线上格式。
//!
//! 内部只有一种 `ChatMessage`，出门要变成三种形状。这一层是纯函数，所以也是
//! **字节纯度**的一部分：同样的消息必须降出同样的字节，否则前缀缓存全废。
//!
//! `openai_tool_input_schema` 那几个函数在改工具的 JSON Schema：部分供应商不认
//! 顶层 `anyOf`、不认 `type: null`，得先摊平。这类改写只动结构不动语义，且必
//! 须是确定性的——工具目录逐字节稳定是缓存的前提。

use crate::llm::openai_compatible::*;

pub(in crate::llm::openai_compatible) fn prepare_chat_messages_for_provider(
    provider: &ProviderConfig,
    mut messages: Vec<ChatMessage>,
) -> Vec<ChatMessage> {
    if !provider_accepts_reasoning_content(provider) {
        for message in &mut messages {
            message.reasoning_content = None;
        }
    }
    messages
}

pub(in crate::llm::openai_compatible) fn taotoken_glm_chat_template_kwargs(
    provider: &ProviderConfig,
) -> Option<ChatTemplateKwargs> {
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

pub(in crate::llm::openai_compatible) fn lower_responses_messages(
    messages: Vec<ChatMessage>,
) -> Vec<Value> {
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

pub(in crate::llm::openai_compatible) fn lower_responses_assistant_message(
    message: ChatMessage,
) -> Vec<Value> {
    let mut items = Vec::new();
    let text = chat_content_text(message.content);
    if !text.trim().is_empty() {
        items.push(json!({"role": "assistant", "content": text}));
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

pub(in crate::llm::openai_compatible) fn lower_responses_user_content(
    content: Option<crate::llm::ChatContent>,
) -> Vec<Value> {
    match content {
        Some(crate::llm::ChatContent::Parts(parts)) => parts
            .into_iter()
            .map(|part| match part {
                crate::llm::ChatContentPart::Text { text } => {
                    json!({"type": "input_text", "text": text})
                }
                crate::llm::ChatContentPart::ImageUrl { image_url } => {
                    json!({"type": "input_image", "image_url": image_url.url})
                }
                crate::llm::ChatContentPart::VideoUrl { .. } => {
                    json!({"type": "input_text", "text": "[video input omitted: this provider protocol has no video support]"})
                }
                // Responses 的 PDF 块:`input_file` + `filename` + `file_data`
                // (data URL)。跟 chat completions 是两个字段名,不能照搬。
                crate::llm::ChatContentPart::File { file } => {
                    json!({"type": "input_file", "filename": file.filename, "file_data": file.file_data})
                }
            })
            .collect(),
        Some(crate::llm::ChatContent::Text(text)) => vec![json!({"type": "input_text", "text": text})],
        None => vec![json!({"type": "input_text", "text": ""})],
    }
}

pub(in crate::llm::openai_compatible) fn chat_content_text(
    content: Option<crate::llm::ChatContent>,
) -> String {
    match content {
        Some(crate::llm::ChatContent::Text(text)) => text,
        Some(crate::llm::ChatContent::Parts(parts)) => parts
            .into_iter()
            .filter_map(|part| match part {
                crate::llm::ChatContentPart::Text { text } => Some(text),
                crate::llm::ChatContentPart::ImageUrl { .. }
                | crate::llm::ChatContentPart::VideoUrl { .. }
                | crate::llm::ChatContentPart::File { .. } => None,
            })
            .collect::<Vec<_>>()
            .join(""),
        None => String::new(),
    }
}

pub(in crate::llm::openai_compatible) fn lower_responses_tools(
    tools: Vec<ToolDefinition>,
) -> Vec<Value> {
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

pub(in crate::llm::openai_compatible) fn lower_anthropic_system(
    messages: &[ChatMessage],
) -> Option<String> {
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

pub(in crate::llm::openai_compatible) fn lower_anthropic_messages(
    messages: Vec<ChatMessage>,
) -> Vec<AnthropicMessage> {
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

pub(in crate::llm::openai_compatible) fn lower_anthropic_user_content(
    content: Option<crate::llm::ChatContent>,
) -> Vec<AnthropicContentBlock> {
    match content {
        Some(crate::llm::ChatContent::Parts(parts)) => {
            let mut blocks: Vec<AnthropicContentBlock> = parts
                .into_iter()
                .filter_map(|part| match part {
                    crate::llm::ChatContentPart::Text { text } => {
                        Some(AnthropicContentBlock::Text { text })
                    }
                    crate::llm::ChatContentPart::ImageUrl { image_url } => {
                        lower_anthropic_image_url(&image_url.url)
                    }
                    crate::llm::ChatContentPart::VideoUrl { .. } => {
                        Some(AnthropicContentBlock::Text {
                            text:
                                "[video input omitted: this provider protocol has no video support]"
                                    .to_string(),
                        })
                    }
                    crate::llm::ChatContentPart::File { file } => {
                        lower_anthropic_document(&file.file_data)
                    }
                })
                .collect();
            // Anthropic 规定 document 块摆在文本块**之前**,而 Miyu 组装 parts
            // 时正文在最前(图/视频/PDF 依次追加)。稳定分区把 document 提上来,
            // 同类块之间的原有次序不动——这一层是字节纯度的一部分,排序不确定
            // 就等于每轮换一份前缀。
            blocks.sort_by_key(|block| !matches!(block, AnthropicContentBlock::Document { .. }));
            blocks
        }
        Some(crate::llm::ChatContent::Text(text)) => vec![AnthropicContentBlock::Text { text }],
        None => vec![AnthropicContentBlock::Text {
            text: String::new(),
        }],
    }
}

pub(in crate::llm::openai_compatible) fn lower_anthropic_image_url(
    url: &str,
) -> Option<AnthropicContentBlock> {
    if url.starts_with("http://") || url.starts_with("https://") {
        return Some(AnthropicContentBlock::Image {
            source: AnthropicImageSource::Url {
                url: url.to_string(),
            },
        });
    }
    let data = url.strip_prefix("data:")?;
    let (media_type, base64) = data.split_once(";base64,")?;
    Some(AnthropicContentBlock::Image {
        source: AnthropicImageSource::Base64 {
            media_type: media_type.to_string(),
            data: base64.to_string(),
        },
    })
}

/// PDF → Anthropic `document` 块。`source` 与 image 同构,所以复用同一个
/// 拆解:`data:<media_type>;base64,<data>` 或直接 URL。拆不开就整块丢掉——
/// 发一个残缺的 source 只会换来 400。
pub(in crate::llm::openai_compatible) fn lower_anthropic_document(
    url: &str,
) -> Option<AnthropicContentBlock> {
    if url.starts_with("http://") || url.starts_with("https://") {
        return Some(AnthropicContentBlock::Document {
            source: AnthropicImageSource::Url {
                url: url.to_string(),
            },
        });
    }
    let data = url.strip_prefix("data:")?;
    let (media_type, base64) = data.split_once(";base64,")?;
    Some(AnthropicContentBlock::Document {
        source: AnthropicImageSource::Base64 {
            media_type: media_type.to_string(),
            data: base64.to_string(),
        },
    })
}

pub(in crate::llm::openai_compatible) fn lower_anthropic_assistant_content(
    message: ChatMessage,
) -> Vec<AnthropicContentBlock> {
    let mut content = Vec::new();
    let has_tool_calls = message
        .tool_calls
        .as_ref()
        .is_some_and(|calls| !calls.is_empty());
    if has_tool_calls {
        if let (Some(signature), Some(thinking)) = (
            message.thinking_signature.as_ref(),
            message.reasoning_content.as_ref(),
        ) {
            if !thinking.trim().is_empty() && !signature.trim().is_empty() {
                content.push(AnthropicContentBlock::Thinking {
                    thinking: thinking.clone(),
                    signature: signature.clone(),
                });
            }
        }
    }
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

pub(in crate::llm::openai_compatible) fn lower_anthropic_tools(
    tools: Vec<ToolDefinition>,
) -> Vec<AnthropicTool> {
    tools
        .into_iter()
        .map(|tool| AnthropicTool {
            name: tool.function.name,
            description: tool.function.description,
            input_schema: tool.function.parameters,
        })
        .collect()
}

pub(in crate::llm::openai_compatible) fn wrap_system_update(text: String) -> String {
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

pub(in crate::llm::openai_compatible) fn chat_content_text_ref(
    content: Option<&crate::llm::ChatContent>,
) -> String {
    match content {
        Some(crate::llm::ChatContent::Text(text)) => text.clone(),
        Some(crate::llm::ChatContent::Parts(parts)) => parts
            .iter()
            .filter_map(|part| match part {
                crate::llm::ChatContentPart::Text { text } => Some(text.clone()),
                crate::llm::ChatContentPart::ImageUrl { .. }
                | crate::llm::ChatContentPart::VideoUrl { .. }
                | crate::llm::ChatContentPart::File { .. } => None,
            })
            .collect::<Vec<_>>()
            .join(""),
        None => String::new(),
    }
}

pub(in crate::llm::openai_compatible) fn openai_tool_input_schema(schema: Value) -> Value {
    let flattened = flatten_top_level_any_of(schema);
    let normalized = remove_null_any_of(flattened);
    if normalized.is_object() {
        normalized
    } else {
        json!({"type": "object"})
    }
}

pub(in crate::llm::openai_compatible) fn flatten_top_level_any_of(schema: Value) -> Value {
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

pub(in crate::llm::openai_compatible) fn remove_null_any_of(value: Value) -> Value {
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

/// Anthropic stop_reason → OpenAI 风格 finish_reason(消费方按后者判断)。
pub(in crate::llm::openai_compatible) fn map_anthropic_stop_reason(
    stop_reason: Option<String>,
) -> Option<String> {
    stop_reason.map(|reason| {
        match reason.as_str() {
            "max_tokens" => "length",
            "tool_use" => "tool_calls",
            "end_turn" | "stop_sequence" => "stop",
            other => other,
        }
        .to_string()
    })
}
