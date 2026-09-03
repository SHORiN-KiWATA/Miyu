//! Miyu 消息增量 → claude stream-json user 消息的翻译。
//!
//! claude 的 stream-json 输入只接受 user 消息,不能注入 assistant 历史。
//! 所以增量按「历史段 + 活跃尾巴」二分:活跃尾巴(结尾连续的 user 消息,
//! 即本轮输入与 runtime/记忆等 turn_context 块)逐条变成独立 text/image
//! 块;历史段(全量重放时才有)渲染成一个 `<conversation-history>` 转写块。

use crate::llm::openai_compatible::*;
use crate::llm::{ChatContent, ChatContentPart};

/// 抽出开头的 system 消息拼成 `--system-prompt`,其余原顺序保留。
pub(in crate::llm::openai_compatible) fn split_system(messages: Vec<ChatMessage>) -> (String, Vec<ChatMessage>) {
    let mut system_parts = Vec::new();
    let mut conversation = Vec::with_capacity(messages.len());
    for message in messages {
        if message.role == "system" && conversation.is_empty() {
            if let Some(text) = text_of(&message) {
                system_parts.push(text);
            }
            continue;
        }
        conversation.push(message);
    }
    (system_parts.join("\n\n"), conversation)
}

fn text_of(message: &ChatMessage) -> Option<String> {
    match &message.content {
        Some(ChatContent::Text(text)) => Some(text.clone()),
        Some(ChatContent::Parts(parts)) => {
            let text = parts
                .iter()
                .filter_map(|part| match part {
                    ChatContentPart::Text { text } => Some(text.as_str()),
                    ChatContentPart::ImageUrl { .. } | ChatContentPart::VideoUrl { .. } => None,
                })
                .collect::<Vec<_>>()
                .join("\n");
            (!text.is_empty()).then_some(text)
        }
        None => None,
    }
}

/// stdin 的单行载荷:一条 stream-json user 消息(含尾部换行)。
pub(super) fn render_user_payload(delta: &[ChatMessage]) -> String {
    let blocks = render_user_blocks(delta);
    let mut line = json!({
        "type": "user",
        "message": { "role": "user", "content": blocks }
    })
    .to_string();
    line.push('\n');
    line
}

/// 增量消息 → 内容块(Anthropic 口径:text/image)。历史段转写成一个
/// `<conversation-history>` 文本块,活跃尾巴逐块给。antigravity 线共用这份
/// 翻译,只把它认不了的 image 块再降级。
pub(in crate::llm::openai_compatible) fn render_user_blocks(delta: &[ChatMessage]) -> Vec<Value> {
    let mut blocks: Vec<Value> = Vec::new();
    // 活跃尾巴 = 结尾连续的 user 消息;之前的一切都是要转写的历史。
    let tail_start = delta
        .iter()
        .rposition(|message| message.role != "user")
        .map(|index| index + 1)
        .unwrap_or(0);
    let (history, tail) = delta.split_at(tail_start);
    if !history.is_empty() {
        let mut transcript = String::from(
            "The <conversation-history> block replays this conversation's earlier turns \
             (the relay layer had to restart the session). Treat it as prior context, \
             not as new input.\n<conversation-history>\n",
        );
        for message in history {
            render_history_line(message, &mut transcript);
        }
        transcript.push_str("</conversation-history>");
        blocks.push(json!({ "type": "text", "text": transcript }));
    }
    for message in tail {
        match &message.content {
            Some(ChatContent::Text(text)) => {
                if !text.is_empty() {
                    blocks.push(json!({ "type": "text", "text": text }));
                }
            }
            Some(ChatContent::Parts(parts)) => {
                for part in parts {
                    match part {
                        ChatContentPart::Text { text } => {
                            if !text.is_empty() {
                                blocks.push(json!({ "type": "text", "text": text }));
                            }
                        }
                        ChatContentPart::ImageUrl { image_url } => {
                            blocks.push(image_block(&image_url.url));
                        }
                        ChatContentPart::VideoUrl { .. } => {
                            blocks.push(json!({
                                "type": "text",
                                "text": "[video input omitted: the claude-code relay has no video support]"
                            }));
                        }
                    }
                }
            }
            None => {}
        }
    }
    if blocks.is_empty() {
        blocks.push(json!({ "type": "text", "text": "(continue)" }));
    }
    blocks
}

fn render_history_line(message: &ChatMessage, transcript: &mut String) {
    match message.role.as_str() {
        "user" => {
            transcript.push_str("User:\n");
            transcript.push_str(&text_of(message).unwrap_or_default());
            if matches!(&message.content, Some(ChatContent::Parts(parts))
                if parts.iter().any(|part| matches!(part, ChatContentPart::ImageUrl { .. })))
            {
                transcript.push_str("\n[image omitted in replayed history]");
            }
        }
        "assistant" => {
            transcript.push_str("Assistant:\n");
            transcript.push_str(&text_of(message).unwrap_or_default());
            if let Some(calls) = &message.tool_calls {
                for call in calls {
                    transcript.push_str(&format!(
                        "\n[called tool {} with {}]",
                        call.function.name, call.function.arguments
                    ));
                }
            }
        }
        "tool" => {
            transcript.push_str("[tool result]\n");
            transcript.push_str(&text_of(message).unwrap_or_default());
        }
        other => {
            transcript.push_str(other);
            transcript.push_str(":\n");
            transcript.push_str(&text_of(message).unwrap_or_default());
        }
    }
    transcript.push_str("\n\n");
}

/// data:/http(s) 图片 → Anthropic 内容块;认不出的形态退化为占位文本。
fn image_block(url: &str) -> Value {
    if let Some(rest) = url.strip_prefix("data:") {
        if let Some((meta, data)) = rest.split_once(',') {
            if let Some(media_type) = meta.strip_suffix(";base64") {
                return json!({
                    "type": "image",
                    "source": { "type": "base64", "media_type": media_type, "data": data }
                });
            }
        }
    } else if url.starts_with("http://") || url.starts_with("https://") {
        return json!({
            "type": "image",
            "source": { "type": "url", "url": url }
        });
    }
    json!({ "type": "text", "text": "[image unavailable]" })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn system_prompt_splits_off_and_tail_stays_verbatim() {
        let messages = vec![
            ChatMessage::system("persona prompt"),
            ChatMessage::plain("user", "hello"),
            ChatMessage::turn_context("<runtime now=\"x\"/>"),
        ];
        let (system, conversation) = split_system(messages);
        assert_eq!(system, "persona prompt");
        assert_eq!(conversation.len(), 2);

        let payload = render_user_payload(&conversation);
        let value: Value = serde_json::from_str(payload.trim()).unwrap();
        let blocks = value["message"]["content"].as_array().unwrap();
        // 全是 user 消息 ⇒ 没有历史转写块,逐条独立 text 块。
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0]["text"], "hello");
        assert_eq!(blocks[1]["text"], "<runtime now=\"x\"/>");
    }

    #[test]
    fn full_replay_wraps_history_and_keeps_live_tail_separate() {
        let conversation = vec![
            ChatMessage::plain("user", "q1"),
            ChatMessage::assistant("a1", None),
            ChatMessage::plain("user", "q2"),
        ];
        let payload = render_user_payload(&conversation);
        let value: Value = serde_json::from_str(payload.trim()).unwrap();
        let blocks = value["message"]["content"].as_array().unwrap();
        assert_eq!(blocks.len(), 2);
        let transcript = blocks[0]["text"].as_str().unwrap();
        assert!(transcript.contains("<conversation-history>"));
        assert!(transcript.contains("User:\nq1"));
        assert!(transcript.contains("Assistant:\na1"));
        assert_eq!(blocks[1]["text"], "q2");
    }

    #[test]
    fn data_url_images_become_base64_blocks() {
        let block = image_block("data:image/png;base64,QUJD");
        assert_eq!(block["type"], "image");
        assert_eq!(block["source"]["media_type"], "image/png");
        assert_eq!(block["source"]["data"], "QUJD");
        let fallback = image_block("file:///tmp/x.png");
        assert_eq!(fallback["type"], "text");
    }
}
