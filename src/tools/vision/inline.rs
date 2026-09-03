//! 让当前多模态模型**直接看**:工具不发旁路请求。
//!
//! `vision_analyze` 判定活跃文本池自己就认图/视频时,把媒体寄存在这里,
//! 只给模型回一条短 JSON(`mode: inline` + `ref`);回合循环在推进 tool
//! 消息之后凭 `ref` 取走媒体,作为图片/视频块追加成一条用户消息,并落库
//! (`turn_inline_media`)供历史重放逐字节回灌。旁路请求"另一个没有上下文的
//! 模型看完转述文字"的信息损失由此消除(09-03 用户裁定)。
//!
//! 寄存表是进程级的,键是每次调用的随机 `ref`,取走即删;工具结果落库的
//! 是 JSON 文本,不含媒体本体,所以对话库里不会多出一份 base64。

use crate::state::TurnInlineMedia;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};

pub const INLINE_MODE: &str = "inline";

static PENDING: LazyLock<Mutex<HashMap<String, Vec<TurnInlineMedia>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// 寄存一批媒体,返回工具结果文本。`items` 的 `call_id`/`seq` 由取走方补。
pub fn deposit(items: Vec<TurnInlineMedia>) -> String {
    let reference = crate::runtime::random_id("vis", 12);
    let media: Vec<Value> = items
        .iter()
        .map(|item| {
            json!({
                "source": item.source,
                "kind": item.kind,
                "mime": item.mime,
            })
        })
        .collect();
    PENDING.lock().unwrap().insert(reference.clone(), items);
    json!({
        "ok": true,
        "mode": INLINE_MODE,
        "ref": reference,
        "media": media,
    })
    .to_string()
}

/// 从工具结果里认出 inline 标记并取走寄存的媒体。不是 inline 结果、或
/// 已被取走(进程重启后重放不会再来这里)都返回空。
pub fn take_from_output(output: &str) -> Vec<TurnInlineMedia> {
    let Some(reference) = inline_reference(output) else {
        return Vec::new();
    };
    PENDING
        .lock()
        .unwrap()
        .remove(&reference)
        .unwrap_or_default()
}

pub fn inline_reference(output: &str) -> Option<String> {
    let value = serde_json::from_str::<Value>(output).ok()?;
    if value.get("mode").and_then(Value::as_str) != Some(INLINE_MODE) {
        return None;
    }
    value.get("ref").and_then(Value::as_str).map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deposit_and_take_round_trip_once() {
        let items = vec![TurnInlineMedia {
            call_id: String::new(),
            seq: 0,
            kind: "image".to_string(),
            mime: "image/png".to_string(),
            source: "/tmp/a.png".to_string(),
            data: Some(vec![1, 2, 3]),
        }];
        let output = deposit(items.clone());
        assert!(inline_reference(&output).is_some());
        assert_eq!(take_from_output(&output), items);
        assert!(take_from_output(&output).is_empty());
        assert!(take_from_output("{\"ok\":true}").is_empty());
        assert!(take_from_output("plain text").is_empty());
    }
}

/// 真机探针(09-03):当前活跃供应商接不接受 tool 消息里带 image_url。
/// 跑法:`cargo test --lib live_probe_tool_message_media -- --ignored --nocapture`
#[cfg(test)]
mod live_probe {
    use crate::config::AppConfig;
    use crate::llm::{
        ChatContent, ChatContentPart, ChatMessage, FunctionDefinition, ImageUrlContent, ToolCall,
        ToolCallFunction, ToolDefinition,
    };
    use base64::Engine;

    #[tokio::test]
    #[ignore]
    async fn live_probe_tool_message_media() {
        let paths = crate::paths::MiyuPaths::new().unwrap();
        let config = AppConfig::load(&paths).unwrap();
        let wanted = std::env::var("MIYU_PROBE_PROVIDER").ok();
        let provider = config.provider(wanted.as_deref()).unwrap();
        eprintln!("provider={} model={}", provider.id, provider.default_model);
        let client = crate::llm::OpenAiCompatibleClient::new(provider, &config, &paths).unwrap();
        let mut png = Vec::new();
        image::RgbaImage::from_pixel(48, 48, image::Rgba([220, 30, 30, 255]))
            .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
            .unwrap();
        let data_url = format!(
            "data:image/png;base64,{}",
            base64::engine::general_purpose::STANDARD.encode(&png)
        );
        let tool_message = ChatMessage {
            content: Some(ChatContent::Parts(vec![
                ChatContentPart::Text {
                    text: "{\"ok\":true,\"attached\":1}".to_string(),
                },
                ChatContentPart::ImageUrl {
                    image_url: ImageUrlContent { url: data_url },
                },
            ])),
            tool_call_id: Some("call_probe".to_string()),
            ..ChatMessage::plain("tool", "")
        };
        let messages = vec![
            ChatMessage::system("Tools may return images. Answer briefly."),
            ChatMessage::plain(
                "user",
                "Look at /tmp/probe.png with vision_analyze and tell me the color of the square.",
            ),
            ChatMessage::assistant(
                "",
                Some(vec![ToolCall {
                    id: "call_probe".to_string(),
                    kind: "function".to_string(),
                    function: ToolCallFunction {
                        name: "vision_analyze".to_string(),
                        arguments: "{\"image\":\"/tmp/probe.png\"}".to_string(),
                    },
                }]),
            ),
            tool_message,
        ];
        let tools = vec![ToolDefinition {
            kind: "function",
            function: FunctionDefinition {
                name: "vision_analyze".to_string(),
                description: "Look at an image.".to_string(),
                parameters: serde_json::json!({"type":"object","properties":{"image":{"type":"string"}}}),
            },
        }];
        match client.chat_stream(messages, tools, |_| Ok(())).await {
            Ok(result) => eprintln!("PROBE OK: {}", result.content.trim()),
            Err(error) => eprintln!("PROBE ERR: {error:#}"),
        }
    }
}
