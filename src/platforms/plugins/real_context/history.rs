//! 群历史的取回与格式化。
//!
//! 格式化出来的东西会进提示词，所以**顺序与措辞必须稳定**——这段在前缀里，换
//! 一个字就是一次全量重算。
//!
//! 图片与文件只放引用不放内容（`MAX_CONTEXT_IMAGE_REFS` / `_FILE_REFS`），并且
//! 只回看有限条消息：群历史可以无限长，上下文不能。

use crate::platforms::plugins::real_context::*;

pub(in crate::platforms::plugins::real_context) const MAX_CONTEXT_IMAGE_REFS: usize = 8;

pub(in crate::platforms::plugins::real_context) const MAX_CONTEXT_FILE_REFS: usize = 8;

/// How far back `vision_analyze` can still reach. Deliberately independent of
/// what this turn rendered: the log is incremental now, so a block usually
/// holds a handful of messages, and tying the resolvable set to it would leave
/// the model unable to open a picture it can plainly see in the replayed
/// history. Ids derive from the message they came from, so the wider sweep
/// mints exactly the ids already written down in earlier turns.
pub(in crate::platforms::plugins::real_context) const CONTEXT_IMAGE_LOOKBACK_MESSAGES: usize = 200;

pub(in crate::platforms::plugins::real_context) const MAX_CONTEXT_IMAGES_PER_MESSAGE: usize = 4;

pub(in crate::platforms::plugins::real_context) const MAX_CONTEXT_FILES_PER_MESSAGE: usize = 4;

pub(in crate::platforms::plugins::real_context) fn history_query_limit(configured: usize) -> usize {
    configured.saturating_add(1).min(200)
}

pub(in crate::platforms::plugins::real_context) fn prepare_history(
    messages: &mut Vec<HistoryMessage>,
    message_id: &str,
    maximum: usize,
) {
    if !message_id.is_empty() {
        messages.retain(|message| message.message_id != message_id);
    }
    if messages.len() > maximum {
        messages.drain(..messages.len() - maximum);
    }
}

pub(in crate::platforms::plugins::real_context) fn restraint_adjustments(
    enabled: bool,
    strength: &str,
    heat: f64,
) -> (f64, f64) {
    if !enabled {
        return (0.0, 0.0);
    }
    let (penalty_per_heat, max_penalty, threshold_per_heat, max_threshold) = match strength {
        "light" => (0.01, 0.13, 0.015, 0.05),
        "strong" => (0.11, 0.40, 0.04, 0.12),
        _ => (0.05, 0.25, 0.025, 0.08),
    };
    (
        (heat * penalty_per_heat).min(max_penalty),
        (heat * threshold_per_heat).min(max_threshold),
    )
}

pub(in crate::platforms::plugins::real_context) fn short_message_boost(
    event: &PlatformInboundEvent,
    continuation_boost: f64,
    system_boost: f64,
    strength: &str,
) -> f64 {
    if continuation_boost > 0.0 || system_boost > 0.0 || !event.media.is_empty() {
        return 0.0;
    }
    let (maximum, boost) = match strength {
        "light" => (6, 0.03),
        "strong" => (8, 0.08),
        _ => (6, 0.05),
    };
    let length = event
        .text
        .chars()
        .filter(|character| !character.is_whitespace())
        .count();
    if length > 0 && length <= maximum {
        boost
    } else {
        0.0
    }
}

pub(super) fn format_history(
    messages: &[HistoryMessage],
    maximum_bytes: usize,
    show_user_ids: bool,
) -> String {
    format_history_internal(messages, maximum_bytes, show_user_ids, 0, 0, false).text
}

pub(in crate::platforms::plugins::real_context) struct FormattedHistory {
    pub(in crate::platforms::plugins::real_context) text: String,
    pub(in crate::platforms::plugins::real_context) images:
        Vec<crate::platforms::PlatformContextImageRef>,
    pub(in crate::platforms::plugins::real_context) files: Vec<PlatformContextFileRef>,
    pub(in crate::platforms::plugins::real_context) message_count: usize,
}

pub(in crate::platforms::plugins::real_context) fn format_history_for_turn(
    messages: &[HistoryMessage],
    maximum_bytes: usize,
    show_user_ids: bool,
    maximum_images: usize,
    maximum_files: usize,
) -> FormattedHistory {
    format_history_internal(
        messages,
        maximum_bytes,
        show_user_ids,
        maximum_images,
        maximum_files,
        false,
    )
}

/// 只收图片引用的入口:与 `format_history_for_turn(...).images` 逐项一致
/// (含预算截断语义),但图片收满即提前停止,不再渲染剩余文本。
pub(in crate::platforms::plugins::real_context) fn context_image_refs(
    messages: &[HistoryMessage],
    maximum_bytes: usize,
    show_user_ids: bool,
    maximum_images: usize,
) -> Vec<crate::platforms::PlatformContextImageRef> {
    format_history_internal(
        messages,
        maximum_bytes,
        show_user_ids,
        maximum_images,
        0,
        true,
    )
    .images
}

/// 私聊用:历史里的图片引用和文件/视频引用一起收(09-04)。此前私聊只收图,
/// 上一轮看过的视频在下一轮就解析不到——模型从会话上下文里拿到旧 id 却
/// 被告知"文件已过期"。不早停:收满图片后文件可能还没收齐。
pub(in crate::platforms::plugins::real_context) fn context_media_refs(
    messages: &[HistoryMessage],
    maximum_bytes: usize,
    show_user_ids: bool,
    maximum_images: usize,
    maximum_files: usize,
) -> (
    Vec<crate::platforms::PlatformContextImageRef>,
    Vec<crate::platforms::PlatformContextFileRef>,
) {
    let formatted = format_history_internal(
        messages,
        maximum_bytes,
        show_user_ids,
        maximum_images,
        maximum_files,
        false,
    );
    (formatted.images, formatted.files)
}

#[allow(clippy::too_many_arguments)]
pub(in crate::platforms::plugins::real_context) fn format_history_internal(
    messages: &[HistoryMessage],
    maximum_bytes: usize,
    show_user_ids: bool,
    maximum_images: usize,
    maximum_files: usize,
    stop_when_images_full: bool,
) -> FormattedHistory {
    let mut lines = Vec::with_capacity(messages.len());
    let mut used_bytes = 0_usize;
    let mut images = Vec::new();
    let mut files = Vec::new();
    let mut source_ids = HashMap::<(String, usize), String>::new();
    for message in messages.iter().rev() {
        // 预算触底即 break(:超限检查),唯一需要撤销的就是触底那一条:记下
        // 本条新增,失败时截回去——替代原先每条消息克隆两份集合的 O(n²)。
        let images_before = images.len();
        let files_before = files.len();
        let mut added_sources = Vec::new();
        let mut image_index = 0_usize;
        let mut file_index = 0_usize;
        let media = message
            .content
            .media
            .iter()
            .map(|media| {
                let image_id = if media.kind == MediaKind::Image {
                    image_index += 1;
                    if image_index > MAX_CONTEXT_IMAGES_PER_MESSAGE {
                        return format_history_media(media, None, None);
                    }
                    let source = (message.message_id.clone(), image_index);
                    source_ids.get(&source).cloned().or_else(|| {
                        if images.len() >= maximum_images {
                            return None;
                        }
                        // Derived from the message it came from, not from its
                        // position in the rendered window. The old
                        // `context_image_{n}` counted backwards from the newest
                        // image, so every new picture shifted every id: a
                        // reference written down in one turn pointed at a
                        // different photo in the next, and `vision_analyze`
                        // resolved it without complaint. A stale id now simply
                        // fails to resolve, which the model can act on.
                        let id = format!(
                            "img_{}_{}",
                            safe_prompt_field(&message.message_id),
                            image_index
                        );
                        source_ids.insert(source.clone(), id.clone());
                        added_sources.push(source);
                        images.push(crate::platforms::PlatformContextImageRef {
                            id: id.clone(),
                            message_id: message.message_id.clone(),
                            image_index,
                        });
                        Some(id)
                    })
                } else {
                    None
                };
                let file_id = if matches!(media.kind, MediaKind::File | MediaKind::Video) {
                    file_index += 1;
                    if file_index > MAX_CONTEXT_FILES_PER_MESSAGE {
                        return format_history_media(media, image_id.as_deref(), None);
                    }
                    media.media_id.as_deref().and_then(|provider_id| {
                        (files.len() < maximum_files).then(|| {
                            let id = format!(
                                "file_{}_{}",
                                safe_prompt_field(&message.message_id),
                                file_index
                            );
                            files.push(PlatformContextFileRef {
                                id: id.clone(),
                                message_id: message.message_id.clone(),
                                file_index,
                                file_id: provider_id.to_string(),
                                file_name: media
                                    .label
                                    .clone()
                                    .unwrap_or_else(|| provider_id.to_string()),
                                url: None,
                            });
                            id
                        })
                    })
                } else {
                    None
                };
                format_history_media(media, image_id.as_deref(), file_id.as_deref())
            })
            .collect::<Vec<_>>();
        let sender = if message.is_bot {
            "[you]".to_string()
        } else if show_user_ids {
            format!(
                "{}(QQ:{})",
                safe_prompt_field(&message.sender_name),
                safe_prompt_field(&message.sender_id)
            )
        } else {
            safe_prompt_field(&message.sender_name)
        };
        let mut content = truncate_utf8(message.content.text.trim(), 4_096).to_string();
        if !media.is_empty() {
            if !content.is_empty() {
                content.push(' ');
            }
            content.push_str(&media.join(" "));
        }
        if content.is_empty() {
            content.push_str("[no text content]");
        }
        let mut line = format!(
            "[{}] {} [msg={}]: {}",
            format_history_time(message.sent_at),
            sender,
            safe_prompt_field(&message.message_id),
            safe_prompt_field(&content)
        );
        if let Some(reply_to) = message.reply_to_message_id.as_ref() {
            line.push_str(&format!(
                "\n  reply-to: msg={}",
                safe_prompt_field(reply_to)
            ));
        }
        // 历史块暂不标 [you]:自己发的消息本来就有 [you] 前缀,这里再用同
        // 一个记号表示"被提到"会撞义,而且要把本机账号一路穿进来。先只在
        // 当前消息块上做。
        if let Some(mentions) = format_mentioned_users(
            &message.content.mentioned_users,
            &message.content.mentioned_user_ids,
            show_user_ids,
            None,
        ) {
            line.push_str(&format!("\n  @mentions: {mentions}"));
        }
        line.push('\n');
        if used_bytes.saturating_add(line.len()) > maximum_bytes {
            images.truncate(images_before);
            files.truncate(files_before);
            for source in added_sources {
                source_ids.remove(&source);
            }
            break;
        }
        used_bytes += line.len();
        lines.push(line);
        if stop_when_images_full && images.len() >= maximum_images {
            // 图片集合已定格:上限过滤保证之后的消息不可能再改动 images,
            // 只收图的调用者无需继续陪跑剩余文本渲染。预算先触底、收满先
            // 发生、两者都不发生三种情形的 .images 输出均与全量渲染一致
            // (有对拍测试)。
            break;
        }
    }
    let message_count = lines.len();
    lines.reverse();
    FormattedHistory {
        text: lines.concat().trim_end().to_string(),
        images,
        files,
        message_count,
    }
}

pub(in crate::platforms::plugins::real_context) fn format_history_media(
    media: &MediaPlaceholder,
    image_id: Option<&str>,
    file_id: Option<&str>,
) -> String {
    let id = image_id.or(file_id);
    match (id, media.label.as_deref()) {
        (Some(id), Some(label)) => format!(
            "[{} id={}, label={}]",
            media_label(media.kind),
            id,
            safe_prompt_field(label)
        ),
        (Some(id), None) => format!("[{} id={}]", media_label(media.kind), id),
        (None, Some(label)) => format!(
            "[{}: {}]",
            media_label(media.kind),
            safe_prompt_field(label)
        ),
        (None, None) => format!("[{}]", media_label(media.kind)),
    }
}

pub(in crate::platforms::plugins::real_context) fn format_history_time(timestamp: i64) -> String {
    chrono::DateTime::<chrono::Utc>::from_timestamp(timestamp, 0)
        .map(|time| {
            time.with_timezone(&chrono::Local)
                .format("%H:%M")
                .to_string()
        })
        .unwrap_or_else(|| timestamp.to_string())
}

pub(in crate::platforms::plugins::real_context) fn media_label(kind: MediaKind) -> &'static str {
    match kind {
        MediaKind::Image => "image",
        MediaKind::Sticker => "sticker",
        MediaKind::File => "file",
        MediaKind::Audio => "audio",
        MediaKind::Video => "video",
        MediaKind::Other => "media",
    }
}

pub(in crate::platforms::plugins::real_context) fn outbound_text(
    message: &OutboundMessage,
) -> String {
    let mut parts = Vec::new();
    match &message.body {
        OutboundBody::Segments(segments) => append_segment_text(&mut parts, segments),
        OutboundBody::Forward(nodes) => {
            for node in nodes {
                append_segment_text(&mut parts, &node.segments);
            }
        }
    }
    parts.join("\n").trim().to_string()
}

pub(in crate::platforms::plugins::real_context) fn append_segment_text(
    parts: &mut Vec<String>,
    segments: &[OutboundSegment],
) {
    for segment in segments {
        match segment {
            OutboundSegment::Markdown(text) | OutboundSegment::Text(text) => {
                if !text.trim().is_empty() {
                    parts.push(text.clone());
                }
            }
            OutboundSegment::Mention(user_id) => parts.push(format!("@{user_id}")),
            _ => {}
        }
    }
}

pub(in crate::platforms::plugins::real_context) fn truncate_utf8(
    value: &str,
    maximum_bytes: usize,
) -> &str {
    if value.len() <= maximum_bytes {
        return value;
    }
    let mut end = maximum_bytes;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    &value[..end]
}

pub(in crate::platforms::plugins::real_context) fn truncate_utf8_tail(
    value: &str,
    maximum_bytes: usize,
) -> &str {
    if value.len() <= maximum_bytes {
        return value;
    }
    let mut start = value.len().saturating_sub(maximum_bytes);
    while !value.is_char_boundary(start) {
        start += 1;
    }
    &value[start..]
}

pub(in crate::platforms::plugins::real_context) fn normalized_timestamp(timestamp: i64) -> i64 {
    if timestamp > 0 {
        timestamp
    } else {
        now_unix()
    }
}
