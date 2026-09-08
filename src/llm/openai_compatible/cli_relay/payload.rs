//! Miyu 消息增量 → CLI 中转 user 载荷的翻译(claude stream-json 口径,别的线复用块列表)。
//!
//! claude 的 stream-json 输入只接受 user 消息,不能注入 assistant 历史。
//! 所以增量按「历史段 + 活跃尾巴」二分:活跃尾巴(结尾连续的 user 消息,
//! 即本轮输入与 runtime/记忆等 turn_context 块)逐条变成独立 text/image
//! 块;历史段(全量重放时才有)渲染成一个 `<conversation-history>` 转写块。

use crate::llm::openai_compatible::*;
use crate::llm::{ChatContent, ChatContentPart};

/// 抽出开头的 system 消息拼成 `--system-prompt`,其余原顺序保留。
pub(in crate::llm::openai_compatible) fn split_system(
    messages: Vec<ChatMessage>,
) -> (String, Vec<ChatMessage>) {
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
                    ChatContentPart::ImageUrl { .. }
                    | ChatContentPart::VideoUrl { .. }
                    | ChatContentPart::File { .. } => None,
                })
                .collect::<Vec<_>>()
                .join("\n");
            (!text.is_empty()).then_some(text)
        }
        None => None,
    }
}

/// stdin 的单行载荷:一条 stream-json user 消息(含尾部换行)。
pub(in crate::llm::openai_compatible) fn render_user_payload(
    delta: &[ChatMessage],
    byte_budget: Option<usize>,
) -> String {
    let blocks = render_user_blocks(delta, byte_budget);
    let mut line = json!({
        "type": "user",
        "message": { "role": "user", "content": blocks }
    })
    .to_string();
    line.push('\n');
    line
}

/// 历史块被裁掉时打在开头的标记(常量字节,预算里预留)。
const OMITTED_MARK: &str =
    "[earlier turns omitted: the relay input budget could not hold them]\n\n";
const HISTORY_HEAD: &str =
    "The <conversation-history> block replays this conversation's earlier turns \
     (the relay layer had to restart the session). Treat it as prior context, \
     not as new input.\n<conversation-history>\n";
const HISTORY_FOOT: &str = "</conversation-history>";

/// 增量消息 → 内容块(Anthropic 口径:text/image)。历史段转写成一个
/// `<conversation-history>` 文本块,活跃尾巴逐块给。antigravity 线共用这份
/// 翻译,只把它认不了的 image 块再降级。
///
/// `byte_budget` 是这条线对单条输入的字节上限(None=不设)。**活跃尾巴永远
/// 整条保留**,预算只砍历史,从最老的回合丢起并标一句 omitted:agy 超线时从
/// **尾部**静默截断、照样 SUCCESS,先死的正是排在末尾的本轮真实消息——模型
/// 拿旧残片当新问题答(09-04 群 130515298 案卷缺陷 A)。最坏情况只是少看几轮
/// 旧历史,本轮问题永远在。尾巴自己就超预算时不动它(砍尾巴等于砍问题),
/// 只留痕。
pub(in crate::llm::openai_compatible) fn render_user_blocks(
    delta: &[ChatMessage],
    byte_budget: Option<usize>,
) -> Vec<Value> {
    // 活跃尾巴 = 结尾连续的 user 消息;之前的一切都是要转写的历史。
    let tail_start = delta
        .iter()
        .rposition(|message| message.role != "user")
        .map(|index| index + 1)
        .unwrap_or(0);
    let (history, tail) = delta.split_at(tail_start);

    let mut tail_blocks: Vec<Value> = Vec::new();
    for message in tail {
        match &message.content {
            Some(ChatContent::Text(text)) => {
                if !text.is_empty() {
                    tail_blocks.push(json!({ "type": "text", "text": text }));
                }
            }
            Some(ChatContent::Parts(parts)) => {
                for part in parts {
                    match part {
                        ChatContentPart::Text { text } => {
                            if !text.is_empty() {
                                tail_blocks.push(json!({ "type": "text", "text": text }));
                            }
                        }
                        ChatContentPart::ImageUrl { image_url } => {
                            tail_blocks.push(image_block(&image_url.url));
                        }
                        ChatContentPart::VideoUrl { .. } => {
                            tail_blocks.push(json!({
                                "type": "text",
                                "text": "[video input omitted: the claude-code relay has no video support]"
                            }));
                        }
                        // 中转线的 message_input_modalities 恒为纯文本,PDF 本来
                        // 就不会内联到这里(`agent::input` 那侧已经把路径留给了
                        // 原生文件工具)。留这一臂是防御:真漏进来也只说明"有个
                        // PDF 没送到",而不是把 base64 当正文发出去。
                        ChatContentPart::File { file } => {
                            tail_blocks.push(json!({
                                "type": "text",
                                "text": format!(
                                    "[pdf input omitted: the relay takes text only; open {} with your file tool]",
                                    file.filename
                                )
                            }));
                        }
                    }
                }
            }
            None => {}
        }
    }

    let mut blocks: Vec<Value> = Vec::new();
    if !history.is_empty() {
        let lines: Vec<String> = history
            .iter()
            .map(|message| {
                let mut line = String::new();
                render_history_line(message, &mut line);
                line
            })
            .collect();
        let history_bytes: usize = lines.iter().map(String::len).sum();
        let tail_bytes: usize = tail_blocks.iter().map(block_text_bytes).sum();
        let fixed = HISTORY_HEAD.len() + HISTORY_FOOT.len();
        let keep_from = match byte_budget {
            Some(budget) => {
                let room = budget.saturating_sub(tail_bytes + fixed + OMITTED_MARK.len());
                oldest_kept_index(&lines, history, room)
            }
            None => 0,
        };
        if keep_from > 0 {
            let kept_bytes: usize = lines[keep_from..].iter().map(String::len).sum();
            tracing::warn!(
                budget = byte_budget.unwrap_or(0),
                tail_bytes,
                history_bytes,
                kept_bytes,
                dropped_messages = keep_from,
                kept_messages = lines.len() - keep_from,
                "relay full replay exceeds the CLI input budget; oldest history dropped"
            );
        } else {
            tracing::info!(
                budget = byte_budget.unwrap_or(0),
                tail_bytes,
                history_bytes,
                messages = lines.len(),
                "relay full replay payload"
            );
        }
        if byte_budget.is_some_and(|budget| tail_bytes + fixed > budget) {
            tracing::warn!(
                budget = byte_budget.unwrap_or(0),
                tail_bytes,
                "relay live tail alone exceeds the CLI input budget; sending it whole anyway"
            );
        }
        let mut transcript = String::from(HISTORY_HEAD);
        if keep_from > 0 {
            transcript.push_str(OMITTED_MARK);
        }
        for line in &lines[keep_from..] {
            transcript.push_str(line);
        }
        transcript.push_str(HISTORY_FOOT);
        blocks.push(json!({ "type": "text", "text": transcript }));
    }
    blocks.extend(tail_blocks);
    if blocks.is_empty() {
        blocks.push(json!({ "type": "text", "text": "(continue)" }));
    }
    blocks
}

/// 历史里从哪一条起保留:从最新往回累加,装不下就停;再往后跳到下一条
/// user 消息,让转写从一轮的开头起(截在 assistant/tool 中间会让模型把半截
/// 旧回复当上文)。
fn oldest_kept_index(lines: &[String], history: &[ChatMessage], room: usize) -> usize {
    let mut used = 0usize;
    let mut keep_from = lines.len();
    for (index, line) in lines.iter().enumerate().rev() {
        if used + line.len() > room {
            break;
        }
        used += line.len();
        keep_from = index;
    }
    if keep_from == 0 {
        return 0;
    }
    history[keep_from..]
        .iter()
        .position(|message| message.role == "user")
        .map(|offset| keep_from + offset)
        .unwrap_or(lines.len())
}

/// 一个内容块占 CLI 输入的字节:text 按正文算;image 在 agy 线降级成一句占位、
/// 在 claude 线走独立的 image 块不进文本上限——都按占位文本的量级记。
fn block_text_bytes(block: &Value) -> usize {
    match block.get("type").and_then(Value::as_str) {
        Some("text") => block
            .get("text")
            .and_then(Value::as_str)
            .map(str::len)
            .unwrap_or(0),
        _ => 64,
    }
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

/// base64 data URL → (media_type, 字节)。codex 只能从文件收图(`-i <FILE>`),
/// 活跃尾巴里的 data: 图片要先落成临时文件再挂上去。
pub(in crate::llm::openai_compatible) fn data_url_bytes(url: &str) -> Option<(String, Vec<u8>)> {
    use base64::Engine as _;
    let rest = url.strip_prefix("data:")?;
    let (meta, data) = rest.split_once(',')?;
    let media_type = meta.strip_suffix(";base64")?;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(data.trim())
        .ok()?;
    Some((media_type.to_string(), bytes))
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

        let payload = render_user_payload(&conversation, None);
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
        let payload = render_user_payload(&conversation, None);
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

    /// 09-04 案卷缺陷 A 的回归:渲染后超过预算的历史 + 一条本轮消息,载荷必须
    /// ≤ 预算、本轮消息与全部活跃尾巴块完整在场、被裁的是**最老**的历史且带
    /// omitted 标记。修复前第 2 步直接超出。
    #[test]
    fn byte_budget_drops_oldest_history_and_never_the_live_tail() {
        let mut conversation = Vec::new();
        for index in 0..40 {
            conversation.push(ChatMessage::plain(
                "user",
                format!("q{index} {}", "问".repeat(300)),
            ));
            conversation.push(ChatMessage::assistant(
                format!("a{index} {}", "答".repeat(300)),
                None,
            ));
        }
        conversation.push(ChatMessage::plain("user", "本轮真正的问题"));
        conversation.push(ChatMessage::turn_context("<runtime now=\"x\"/>"));
        conversation.push(ChatMessage::turn_context(
            "<associative-memory>m</associative-memory>",
        ));

        let unbounded = render_user_blocks(&conversation, None);
        let unbounded_bytes: usize = unbounded.iter().map(block_text_bytes).sum();
        let budget = 20_000;
        assert!(
            unbounded_bytes > budget,
            "夹具要先超预算: {unbounded_bytes}"
        );

        let blocks = render_user_blocks(&conversation, Some(budget));
        let total: usize = blocks.iter().map(block_text_bytes).sum();
        assert!(total <= budget, "载荷 {total} 超过预算 {budget}");

        // 活跃尾巴三块原样在场,且在历史块之后。
        assert_eq!(blocks.len(), 4, "{blocks:?}");
        assert_eq!(blocks[1]["text"], "本轮真正的问题");
        assert_eq!(blocks[2]["text"], "<runtime now=\"x\"/>");
        assert_eq!(
            blocks[3]["text"],
            "<associative-memory>m</associative-memory>"
        );

        let transcript = blocks[0]["text"].as_str().unwrap();
        assert!(transcript.starts_with(HISTORY_HEAD));
        assert!(transcript.contains(OMITTED_MARK));
        assert!(transcript.ends_with(HISTORY_FOOT));
        // 最老的没了,最新的在;保留段从一条 user 消息起。
        assert!(!transcript.contains("User:\nq0 "));
        assert!(transcript.contains("User:\nq39 "));
        assert!(transcript.contains("Assistant:\na39 "));
        let body = transcript
            .trim_start_matches(HISTORY_HEAD)
            .trim_start_matches(OMITTED_MARK);
        assert!(body.starts_with("User:\n"), "{body:.80}");

        // 预算宽裕时一条都不丢、不打标记。
        let roomy = render_user_blocks(&conversation, Some(unbounded_bytes + 1024));
        assert!(!roomy[0]["text"].as_str().unwrap().contains(OMITTED_MARK));
        assert!(roomy[0]["text"].as_str().unwrap().contains("User:\nq0 "));

        // 尾巴自己就超预算:整条照发,历史全丢只留标记。
        let tight = render_user_blocks(&conversation, Some(10));
        assert_eq!(tight[1]["text"], "本轮真正的问题");
        assert!(!tight[0]["text"].as_str().unwrap().contains("q39"));
    }
}
