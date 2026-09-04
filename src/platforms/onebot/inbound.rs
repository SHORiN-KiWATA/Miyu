//! 入站消息的解析与限额。
//!
//! OneBot 的消息体有两种形态：CQ 码字符串和段数组。两条路都要走到同一个
//! `InboundMessage`，所以解析函数是成对的（`parse_cq_string` / `parse_message`、
//! `append_cq_image_sources` / `append_message_image_sources`）。
//!
//! 这里几乎每个 `push_*` 都带上限检查，因为**入站数据完全由对端控制**：段数、
//! 字段数、@ 数、文本字符数、图片张数与总字节数、ID 长度。少一条就是一个能把
//! 内存打爆的入口。上限值集中在 `MAX_INBOUND_*`。
//!
//! 图片要保序（`ordered_image_source`）：模型看到的图片顺序必须和用户发的一
//! 致，否则「第二张图里的字」这类指代会错位。

use crate::platforms::onebot::*;

pub(in crate::platforms::onebot) const MAX_INBOUND_IMAGE_BYTES: usize = 10 * 1024 * 1024;

pub(in crate::platforms::onebot) const MAX_INBOUND_IMAGE_TOTAL_BYTES: usize = 20 * 1024 * 1024;

pub(in crate::platforms::onebot) const MAX_INBOUND_IMAGES: usize = 4;

pub(in crate::platforms::onebot) const MAX_INBOUND_FILES: usize = 4;

pub(in crate::platforms::onebot) const MAX_INBOUND_MEDIA_RECORDS: usize = 32;

pub(in crate::platforms::onebot) const MAX_INBOUND_MESSAGE_BYTES: usize = 16 * 1024 * 1024;

pub(in crate::platforms::onebot) const MAX_INBOUND_TEXT_CHARS: usize = 20_000;

pub(in crate::platforms::onebot) const MAX_INBOUND_SEGMENTS: usize = 256;

pub(in crate::platforms::onebot) const MAX_INBOUND_MENTIONS: usize = 32;

pub(in crate::platforms::onebot) const MAX_CQ_FIELDS: usize = 32;

pub(in crate::platforms::onebot) const MAX_ONEBOT_ID_BYTES: usize = 128;

pub(in crate::platforms::onebot) const MAX_INBOUND_FILE_NAME_CHARS: usize = 512;

/// 一条消息里最多跟进几个合并转发。转发套转发的深度另有上限(见 `forward`)。
pub(in crate::platforms::onebot) const MAX_INBOUND_FORWARDS: usize = 4;

#[derive(Default)]
pub(in crate::platforms::onebot) struct InboundMessage {
    pub(in crate::platforms::onebot) text: String,
    pub(in crate::platforms::onebot) text_chars: usize,
    pub(in crate::platforms::onebot) rejected_reason: Option<&'static str>,
    pub(in crate::platforms::onebot) images: Vec<MediaRef>,
    pub(in crate::platforms::onebot) unresolved_image_files: Vec<String>,
    pub(in crate::platforms::onebot) files: Vec<FileRef>,
    pub(in crate::platforms::onebot) at_self: bool,
    pub(in crate::platforms::onebot) reply_to_message_id: Option<String>,
    pub(in crate::platforms::onebot) quoted_message_data: Option<Value>,
    pub(in crate::platforms::onebot) mentioned_user_ids: Vec<String>,
    pub(in crate::platforms::onebot) media: Vec<PlatformInboundMedia>,
    /// 合并转发的资源 id。段里只有这个 id,内容要另外调 `get_forward_msg` 取
    /// (见 `forward`)——解析是同步的,拿不到连接,所以只记 id,展开留给
    /// dispatch。
    pub(in crate::platforms::onebot) forward_ids: Vec<String>,
}

#[derive(Debug)]
pub(in crate::platforms::onebot) enum MediaRef {
    Url(String),
    Bytes(Vec<u8>),
}

pub(in crate::platforms::onebot) enum OrderedMessageImageSource {
    Media(MediaRef),
    File(String),
}

impl MediaRef {
    pub(in crate::platforms::onebot) fn inline_bytes(&self) -> usize {
        match self {
            Self::Url(_) => 0,
            Self::Bytes(bytes) => bytes.len(),
        }
    }

    pub(in crate::platforms::onebot) fn same_source(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Url(left), Self::Url(right)) => left == right,
            (Self::Bytes(left), Self::Bytes(right)) => left == right,
            _ => false,
        }
    }
}

pub(in crate::platforms::onebot) struct FileRef {
    pub(in crate::platforms::onebot) file_id: Option<String>,
    pub(in crate::platforms::onebot) name: String,
    pub(in crate::platforms::onebot) url: Option<String>,
}

pub(in crate::platforms::onebot) fn inbound_file_placeholders(
    message_id: &str,
    files: &[FileRef],
) -> (String, Vec<PlatformContextFileRef>) {
    let mut text = String::new();
    let mut refs = Vec::new();
    for (index, file) in files.iter().enumerate() {
        let provider_id = file
            .file_id
            .clone()
            .or_else(|| file.url.clone())
            .unwrap_or_default();
        if !text.is_empty() {
            text.push('\n');
        }
        if provider_id.is_empty() {
            text.push_str(&format!(
                "[{}: {}]",
                t("file", "文件"),
                crate::platforms::plugins::real_context::safe_prompt_field(&file.name)
            ));
            continue;
        }
        let file_index = index + 1;
        let id = format!("file_{}_{}", message_id, file_index);
        let label = if crate::tools::vision::video_mime(&file.name).is_some() {
            t("video", "视频")
        } else {
            t("file", "文件")
        };
        text.push_str(&format!(
            "[{} id={}, label={}]",
            label,
            id,
            crate::platforms::plugins::real_context::safe_prompt_field(&file.name)
        ));
        refs.push(PlatformContextFileRef {
            id,
            message_id: message_id.to_string(),
            file_index,
            file_id: provider_id,
            file_name: file.name.clone(),
            url: file.url.clone(),
        });
    }
    (text, refs)
}

/// 视频段的文件名:优先 name / file_name,再取 file(NapCat 放的是文件名,
/// 有时是 md5 裸串)。没有视频扩展名就补 `.mp4`,下游全靠扩展名认视频
/// (`vision::video_mime`),缺了它视频会被当普通二进制文件拒掉。
pub(in crate::platforms::onebot) fn video_file_name(data: &Value) -> String {
    let raw = data
        .get("name")
        .and_then(Value::as_str)
        .or_else(|| data.get("file_name").and_then(Value::as_str))
        .or_else(|| data.get("file").and_then(Value::as_str))
        .map(str::trim)
        .filter(|name| !name.is_empty() && !name.starts_with("base64://"))
        .unwrap_or("video");
    let name = bounded_chars(raw, MAX_INBOUND_FILE_NAME_CHARS);
    if crate::tools::vision::video_mime(&name).is_some() {
        name
    } else {
        format!("{name}.mp4")
    }
}

/// Group wake check. `Some(text)` = triggered, with any wake prefix
/// already stripped; `None` = stay silent.
pub(in crate::platforms::onebot) fn group_trigger_text(
    config: &OneBotConfig,
    parsed: &InboundMessage,
    replied_message: Option<&PlatformMessageInfo>,
    self_id: i64,
) -> Option<String> {
    if parsed.at_self
        || replied_message
            .is_some_and(|message| message.sender_id.parse::<i64>().ok() == Some(self_id))
    {
        return Some(parsed.text.clone());
    }
    let text = parsed.text.trim_start();
    // 唤醒词留在正文里。这里曾把它连同后面的分隔符一起剥掉——那是按名字型
    // 唤醒词想的(`miyu 你好` → `你好`),但关键词表接受任何词:用户把「为什么」
    // 设成唤醒词,「为什么不查知识库」被剥成「不查知识库」,疑问句变祈使句,
    // 她照着"别查"去做(08-29 实测)。
    //
    // 剥离还只影响人格模型那一份:`observe_inbound`(入历史库)与主动回复判官
    // 都在 `parsed.text = trigger.content`(dispatch.rs)之前跑,读到的是完整
    // 原文。剥离制造的是"同一条消息两个模型读出两个意思",不是干净。
    // @ 唤醒与引用唤醒本来也不剥,上面那两条分支直接返回全文。
    config
        .group_chats
        .trigger_keywords
        .iter()
        .any(|keyword| text.starts_with(keyword.as_str()))
        .then(|| parsed.text.clone())
}

pub(in crate::platforms::onebot) fn decode_cq_text(text: &str) -> String {
    text.replace("&#91;", "[")
        .replace("&#93;", "]")
        .replace("&#44;", ",")
        .replace("&amp;", "&")
}

pub(in crate::platforms::onebot) fn push_inbound_text(parsed: &mut InboundMessage, text: &str) {
    if parsed.rejected_reason.is_some() {
        return;
    }
    let remaining = MAX_INBOUND_TEXT_CHARS.saturating_sub(parsed.text_chars);
    let mut chars = text.chars();
    let before = parsed.text.len();
    parsed.text.extend(chars.by_ref().take(remaining));
    parsed.text_chars += parsed.text[before..].chars().count();
    if chars.next().is_some() {
        parsed.rejected_reason = Some("message text exceeds the 20,000 character limit");
    }
}

pub(in crate::platforms::onebot) fn push_cq_text(parsed: &mut InboundMessage, text: &str) {
    if parsed.rejected_reason.is_some() {
        return;
    }
    let remaining = MAX_INBOUND_TEXT_CHARS.saturating_sub(parsed.text_chars);
    // The longest supported CQ entity is five characters for one decoded
    // character. Bound the temporary decode even when a raw frame is large.
    let raw_limit = remaining.saturating_mul(5).saturating_add(1);
    let bounded = text.chars().take(raw_limit).collect::<String>();
    push_inbound_text(parsed, &decode_cq_text(&bounded));
    if bounded.chars().count() == raw_limit && text.chars().nth(raw_limit).is_some() {
        parsed.rejected_reason = Some("message text exceeds the 20,000 character limit");
    }
}

pub(in crate::platforms::onebot) fn bounded_onebot_id(value: impl Into<String>) -> Option<String> {
    let value = value.into();
    (!value.is_empty() && value.len() <= MAX_ONEBOT_ID_BYTES).then_some(value)
}

pub(in crate::platforms::onebot) fn push_mention(parsed: &mut InboundMessage, qq: String) {
    if parsed.mentioned_user_ids.len() >= MAX_INBOUND_MENTIONS
        || qq.len() > MAX_ONEBOT_ID_BYTES
        || !qq.bytes().all(|byte| byte.is_ascii_digit())
        || qq == "0"
        || parsed.mentioned_user_ids.contains(&qq)
    {
        return;
    }
    parsed.mentioned_user_ids.push(qq);
}

pub(in crate::platforms::onebot) fn bounded_chars(value: &str, maximum: usize) -> String {
    value.chars().take(maximum).collect()
}

pub(in crate::platforms::onebot) fn push_image_ref_with_limits(
    images: &mut Vec<MediaRef>,
    candidate: MediaRef,
    maximum_images: usize,
    maximum_inline_bytes: usize,
) -> bool {
    if images
        .iter()
        .any(|existing| existing.same_source(&candidate))
    {
        return false;
    }
    if images.len() >= maximum_images {
        return false;
    }
    let candidate_bytes = candidate.inline_bytes();
    if candidate_bytes > MAX_INBOUND_IMAGE_BYTES
        || images
            .iter()
            .map(MediaRef::inline_bytes)
            .sum::<usize>()
            .saturating_add(candidate_bytes)
            > maximum_inline_bytes
    {
        return false;
    }
    images.push(candidate);
    true
}

pub(in crate::platforms::onebot) fn push_inbound_base64(
    parsed: &mut InboundMessage,
    encoded: &str,
) -> bool {
    // Refuse before decoding once the shared count budget is full.
    if parsed.images.len() >= MAX_INBOUND_IMAGES {
        return false;
    }
    let encoded = encoded.strip_prefix("base64://").unwrap_or(encoded);
    let remaining = MAX_INBOUND_IMAGE_TOTAL_BYTES.saturating_sub(
        parsed
            .images
            .iter()
            .map(MediaRef::inline_bytes)
            .sum::<usize>(),
    );
    let maximum_decoded = MAX_INBOUND_IMAGE_BYTES.min(remaining);
    if maximum_decoded == 0 {
        return false;
    }
    let maximum_encoded = maximum_decoded
        .saturating_add(2)
        .div_ceil(3)
        .saturating_mul(4);
    if encoded.len() > maximum_encoded {
        return false;
    }
    let Ok(bytes) = BASE64.decode(encoded) else {
        return false;
    };
    if bytes.len() > maximum_decoded {
        return false;
    }
    push_image_ref_with_limits(
        &mut parsed.images,
        MediaRef::Bytes(bytes),
        MAX_INBOUND_IMAGES,
        MAX_INBOUND_IMAGE_TOTAL_BYTES,
    )
}

pub(in crate::platforms::onebot) fn http_image_source<'a>(
    file: &'a str,
    url: Option<&'a str>,
) -> Option<&'a str> {
    url.filter(|url| {
        (url.starts_with("http://") || url.starts_with("https://")) && url.len() <= 4096
    })
    .or_else(|| {
        Some(file).filter(|file| {
            (file.starts_with("http://") || file.starts_with("https://")) && file.len() <= 4096
        })
    })
}

pub(in crate::platforms::onebot) fn push_inbound_image_source(
    parsed: &mut InboundMessage,
    file: &str,
    url: Option<&str>,
) -> bool {
    if let Some(encoded) = file.strip_prefix("base64://") {
        return push_inbound_base64(parsed, encoded);
    }

    http_image_source(file, url).is_some_and(|source| {
        push_image_ref_with_limits(
            &mut parsed.images,
            MediaRef::Url(source.to_string()),
            MAX_INBOUND_IMAGES,
            MAX_INBOUND_IMAGE_TOTAL_BYTES,
        )
    })
}

pub(in crate::platforms::onebot) fn push_unresolved_image_file(
    resolved_images: usize,
    unresolved: &mut Vec<String>,
    file: Option<String>,
) {
    if resolved_images.saturating_add(unresolved.len()) >= MAX_INBOUND_IMAGES {
        return;
    }
    let Some(file) = file else { return };
    let file = file.trim();
    if file.is_empty()
        || file.len() > 4096
        || file.starts_with("base64://")
        || file.starts_with("http://")
        || file.starts_with("https://")
        || unresolved.iter().any(|existing| existing == file)
    {
        return;
    }
    unresolved.push(file.to_string());
}

pub(in crate::platforms::onebot) fn append_cq_image_sources(
    parsed: &mut InboundMessage,
    raw: &str,
    unresolved: &mut Vec<String>,
) {
    let mut remaining = raw;
    for _ in 0..MAX_INBOUND_SEGMENTS {
        let Some(start) = remaining.find("[CQ:") else {
            return;
        };
        let segment = &remaining[start + 4..];
        let Some(end) = segment.find(']') else {
            return;
        };
        let body = &segment[..end];
        let mut fields = body.split(',');
        if fields.next() == Some("image") {
            let parameters = fields
                .take(MAX_CQ_FIELDS)
                .filter_map(|field| field.split_once('='))
                .collect::<HashMap<_, _>>();
            let file = parameters
                .get("file")
                .map(|value| decode_cq_text(value))
                .unwrap_or_default();
            let url = parameters.get("url").map(|value| decode_cq_text(value));
            if http_image_source(&file, url.as_deref()).is_some() || file.starts_with("base64://") {
                push_inbound_image_source(parsed, &file, url.as_deref());
            } else {
                let file_id = parameters.get("file_id").map(|value| decode_cq_text(value));
                push_unresolved_image_file(
                    parsed.images.len(),
                    unresolved,
                    (!file.is_empty()).then_some(file).or(file_id),
                );
            }
        }
        if parsed.images.len().saturating_add(unresolved.len()) >= MAX_INBOUND_IMAGES {
            return;
        }
        remaining = &segment[end + 1..];
    }
}

pub(in crate::platforms::onebot) fn append_message_image_sources(
    parsed: &mut InboundMessage,
    message: Option<&Value>,
    raw_message: Option<&Value>,
) -> Vec<String> {
    let mut unresolved = Vec::new();
    if let Some(Value::Array(segments)) = message {
        for segment in segments.iter().take(MAX_INBOUND_SEGMENTS) {
            if segment.get("type").and_then(Value::as_str) != Some("image") {
                continue;
            }
            let data = segment.get("data").unwrap_or(&Value::Null);
            let file = data.get("file").and_then(Value::as_str).unwrap_or("");
            let url = data.get("url").and_then(Value::as_str);
            if http_image_source(file, url).is_some() || file.starts_with("base64://") {
                push_inbound_image_source(parsed, file, url);
            } else {
                let file_id = data.get("file_id").and_then(value_id_string);
                push_unresolved_image_file(
                    parsed.images.len(),
                    &mut unresolved,
                    (!file.is_empty()).then(|| file.to_string()).or(file_id),
                );
            }
            if parsed.images.len().saturating_add(unresolved.len()) >= MAX_INBOUND_IMAGES {
                break;
            }
        }
        return unresolved;
    }
    if let Some(raw) = message
        .and_then(Value::as_str)
        .or_else(|| raw_message.and_then(Value::as_str))
    {
        append_cq_image_sources(parsed, raw, &mut unresolved);
    }
    unresolved
}

pub(in crate::platforms::onebot) fn ordered_image_source(
    file: &str,
    url: Option<&str>,
) -> Option<OrderedMessageImageSource> {
    if let Some(encoded) = file.strip_prefix("base64://") {
        let maximum_encoded = MAX_INBOUND_IMAGE_BYTES
            .saturating_add(2)
            .div_ceil(3)
            .saturating_mul(4);
        if encoded.len() > maximum_encoded {
            return None;
        }
        let bytes = BASE64.decode(encoded).ok()?;
        return (bytes.len() <= MAX_INBOUND_IMAGE_BYTES)
            .then_some(OrderedMessageImageSource::Media(MediaRef::Bytes(bytes)));
    }
    if let Some(source) = http_image_source(file, url) {
        return Some(OrderedMessageImageSource::Media(MediaRef::Url(
            source.to_string(),
        )));
    }
    let file = file.trim();
    (!file.is_empty() && file.len() <= 4096)
        .then(|| OrderedMessageImageSource::File(file.to_string()))
}

pub(in crate::platforms::onebot) fn ordered_message_image_sources(
    message: Option<&Value>,
    raw_message: Option<&Value>,
) -> Vec<OrderedMessageImageSource> {
    let mut sources = Vec::new();
    if let Some(Value::Array(segments)) = message {
        for segment in segments.iter().take(MAX_INBOUND_SEGMENTS) {
            if sources.len() >= MAX_INBOUND_IMAGES
                || segment.get("type").and_then(Value::as_str) != Some("image")
            {
                continue;
            }
            let data = segment.get("data").unwrap_or(&Value::Null);
            let file = data.get("file").and_then(Value::as_str).unwrap_or_default();
            let file_id = data.get("file_id").and_then(value_id_string);
            if let Some(source) = ordered_image_source(
                if file.is_empty() {
                    file_id.as_deref().unwrap_or_default()
                } else {
                    file
                },
                data.get("url").and_then(Value::as_str),
            ) {
                sources.push(source);
            }
        }
        return sources;
    }

    let Some(raw) = message
        .and_then(Value::as_str)
        .or_else(|| raw_message.and_then(Value::as_str))
    else {
        return sources;
    };
    let mut remaining = raw;
    for _ in 0..MAX_INBOUND_SEGMENTS {
        let Some(start) = remaining.find("[CQ:") else {
            break;
        };
        let segment = &remaining[start + 4..];
        let Some(end) = segment.find(']') else {
            break;
        };
        let body = &segment[..end];
        let mut fields = body.split(',');
        if fields.next() == Some("image") && sources.len() < MAX_INBOUND_IMAGES {
            let parameters = fields
                .take(MAX_CQ_FIELDS)
                .filter_map(|field| field.split_once('='))
                .collect::<HashMap<_, _>>();
            let file = parameters
                .get("file")
                .or_else(|| parameters.get("file_id"))
                .map(|value| decode_cq_text(value))
                .unwrap_or_default();
            let url = parameters.get("url").map(|value| decode_cq_text(value));
            if let Some(source) = ordered_image_source(&file, url.as_deref()) {
                sources.push(source);
            }
        }
        remaining = &segment[end + 1..];
    }
    sources
}

pub(in crate::platforms::onebot) fn parse_cq_string(raw: &str, self_id: i64) -> InboundMessage {
    let mut parsed = InboundMessage::default();
    let mut remaining = raw;
    let mut segment_count = 0usize;
    while let Some(start) = remaining.find("[CQ:") {
        push_cq_text(&mut parsed, &remaining[..start]);
        if parsed.rejected_reason.is_some() {
            return parsed;
        }
        segment_count += 1;
        if segment_count > MAX_INBOUND_SEGMENTS {
            parsed.rejected_reason = Some("message has too many OneBot segments");
            return parsed;
        }
        let segment = &remaining[start + 4..];
        let Some(end) = segment.find(']') else {
            push_cq_text(&mut parsed, &remaining[start..]);
            return parsed;
        };
        let body = &segment[..end];
        let mut fields = body.split(',');
        let kind = fields.next().unwrap_or_default();
        let parameters = fields
            .take(MAX_CQ_FIELDS)
            .filter_map(|field| field.split_once('='))
            .collect::<HashMap<_, _>>();
        match kind {
            "at" => {
                if let Some(qq) = parameters.get("qq").map(|value| decode_cq_text(value)) {
                    parsed.at_self |= qq == self_id.to_string();
                    push_mention(&mut parsed, qq);
                }
            }
            "reply" => {
                parsed.reply_to_message_id = parameters
                    .get("id")
                    .map(|value| decode_cq_text(value))
                    .and_then(bounded_onebot_id);
            }
            "forward" if parsed.forward_ids.len() < MAX_INBOUND_FORWARDS => {
                if let Some(id) = parameters
                    .get("id")
                    .or_else(|| parameters.get("res_id"))
                    .map(|value| decode_cq_text(value))
                    .and_then(bounded_onebot_id)
                {
                    parsed.forward_ids.push(id);
                }
            }
            "image" | "file" | "record" | "video" | "face"
                if parsed.media.len() < MAX_INBOUND_MEDIA_RECORDS =>
            {
                let media_kind = match kind {
                    "image" => PlatformMediaKind::Image,
                    "file" => PlatformMediaKind::File,
                    "record" => PlatformMediaKind::Audio,
                    "video" => PlatformMediaKind::Video,
                    "face" => PlatformMediaKind::Emoji,
                    _ => PlatformMediaKind::Other,
                };
                parsed.media.push(PlatformInboundMedia {
                    kind: media_kind,
                    id: parameters
                        .get("id")
                        .or_else(|| parameters.get("file_id"))
                        .map(|value| decode_cq_text(value))
                        .and_then(bounded_onebot_id),
                    name: parameters
                        .get("name")
                        .or_else(|| parameters.get("file_name"))
                        .map(|value| {
                            bounded_chars(&decode_cq_text(value), MAX_INBOUND_FILE_NAME_CHARS)
                        }),
                    url: parameters
                        .get("url")
                        .map(|value| decode_cq_text(value))
                        .filter(|url| url.starts_with("http") && url.len() <= 4096),
                });
            }
            _ => {}
        }
        if kind == "image" {
            let file = parameters
                .get("file")
                .map(|value| decode_cq_text(value))
                .unwrap_or_default();
            let url = parameters.get("url").map(|value| decode_cq_text(value));
            if !push_inbound_image_source(&mut parsed, &file, url.as_deref()) {
                push_unresolved_image_file(
                    parsed.images.len(),
                    &mut parsed.unresolved_image_files,
                    (!file.is_empty()).then_some(file),
                );
            }
        }
        remaining = &segment[end + 1..];
    }
    push_cq_text(&mut parsed, remaining);
    parsed
}

/// Parses the OneBot `message` field (segment array, or raw string as a
/// fallback when NapCat isn't configured for array format).
pub(in crate::platforms::onebot) fn parse_message(
    message: Option<&Value>,
    raw_message: Option<&Value>,
    self_id: i64,
) -> InboundMessage {
    let mut parsed = InboundMessage::default();
    let Some(Value::Array(segments)) = message else {
        if let Some(raw) = message
            .and_then(Value::as_str)
            .or_else(|| raw_message.and_then(Value::as_str))
        {
            return parse_cq_string(raw, self_id);
        }
        return parsed;
    };
    if segments.len() > MAX_INBOUND_SEGMENTS {
        parsed.rejected_reason = Some("message has too many OneBot segments");
        return parsed;
    }
    for segment in segments.iter().take(MAX_INBOUND_SEGMENTS) {
        let kind = segment.get("type").and_then(Value::as_str).unwrap_or("");
        let data = segment.get("data").unwrap_or(&Value::Null);
        match kind {
            "text" => {
                if let Some(text) = data.get("text").and_then(Value::as_str) {
                    push_inbound_text(&mut parsed, text);
                    if parsed.rejected_reason.is_some() {
                        return parsed;
                    }
                }
            }
            "image" => {
                if parsed.media.len() < MAX_INBOUND_MEDIA_RECORDS {
                    let file = data.get("file").and_then(Value::as_str).unwrap_or("");
                    parsed.media.push(PlatformInboundMedia {
                        kind: PlatformMediaKind::Image,
                        id: data
                            .get("file_id")
                            .and_then(value_id_string)
                            .and_then(bounded_onebot_id)
                            .or_else(|| {
                                (!file.is_empty() && !file.starts_with("base64://"))
                                    .then(|| file.to_string())
                                    .and_then(bounded_onebot_id)
                            }),
                        name: None,
                        url: data
                            .get("url")
                            .and_then(Value::as_str)
                            .filter(|url| url.starts_with("http") && url.len() <= 4096)
                            .map(str::to_string),
                    });
                }
                let file = data.get("file").and_then(Value::as_str).unwrap_or("");
                if !push_inbound_image_source(
                    &mut parsed,
                    file,
                    data.get("url").and_then(Value::as_str),
                ) {
                    let file_id = data.get("file_id").and_then(value_id_string);
                    push_unresolved_image_file(
                        parsed.images.len(),
                        &mut parsed.unresolved_image_files,
                        (!file.is_empty()).then(|| file.to_string()).or(file_id),
                    );
                }
            }
            "at" => {
                let qq = data.get("qq").and_then(|qq| match qq {
                    Value::String(qq) => Some(qq.clone()),
                    Value::Number(qq) => Some(qq.to_string()),
                    _ => None,
                });
                if qq.as_deref() == Some(self_id.to_string().as_str()) {
                    parsed.at_self = true;
                }
                if let Some(qq) = qq {
                    push_mention(&mut parsed, qq);
                }
            }
            "reply" => {
                parsed.reply_to_message_id = data
                    .get("id")
                    .and_then(value_id_string)
                    .and_then(bounded_onebot_id);
            }
            "forward" if parsed.forward_ids.len() < MAX_INBOUND_FORWARDS => {
                // 实现之间字段名不统一:NapCat 用 id,部分实现用 res_id。
                if let Some(id) = data
                    .get("id")
                    .or_else(|| data.get("res_id"))
                    .and_then(value_id_string)
                    .and_then(bounded_onebot_id)
                {
                    parsed.forward_ids.push(id);
                }
            }
            "file" => {
                if parsed.media.len() < MAX_INBOUND_MEDIA_RECORDS {
                    parsed.media.push(PlatformInboundMedia {
                        kind: PlatformMediaKind::File,
                        id: data
                            .get("file_id")
                            .and_then(value_id_string)
                            .or_else(|| data.get("file").and_then(value_id_string))
                            .and_then(bounded_onebot_id),
                        name: data
                            .get("file_name")
                            .and_then(Value::as_str)
                            .or_else(|| data.get("name").and_then(Value::as_str))
                            .map(|name| bounded_chars(name, MAX_INBOUND_FILE_NAME_CHARS)),
                        url: data
                            .get("url")
                            .and_then(Value::as_str)
                            .filter(|url| url.starts_with("http") && url.len() <= 4096)
                            .map(str::to_string),
                    });
                }
                if parsed.files.len() >= MAX_INBOUND_FILES {
                    continue;
                }
                let name = bounded_chars(
                    data.get("file_name")
                        .and_then(Value::as_str)
                        .or_else(|| data.get("name").and_then(Value::as_str))
                        .or_else(|| data.get("file").and_then(Value::as_str))
                        .unwrap_or("file"),
                    MAX_INBOUND_FILE_NAME_CHARS,
                );
                parsed.files.push(FileRef {
                    file_id: data
                        .get("file_id")
                        .and_then(Value::as_str)
                        .and_then(|id| bounded_onebot_id(id.to_string())),
                    name,
                    url: data
                        .get("url")
                        .and_then(Value::as_str)
                        .filter(|url| url.starts_with("http") && url.len() <= 4096)
                        .map(str::to_string),
                });
            }
            "video" => {
                // 视频走文件那条懒下载链路(09-04):此前只进 media,当轮正文里
                // 没有任何 id,模型连"有段视频可以看"都不知道。NapCat 视频段带
                // file(文件名)/url/file_id/file_size;file_id 不是群文件 id,
                // 下载时由 fetch_platform_file 用 url 或 get_file 兜底。
                let name = video_file_name(data);
                let file_id = data
                    .get("file_id")
                    .and_then(value_id_string)
                    .or_else(|| data.get("id").and_then(value_id_string))
                    .and_then(bounded_onebot_id);
                let url = data
                    .get("url")
                    .and_then(Value::as_str)
                    .filter(|url| url.starts_with("http") && url.len() <= 4096)
                    .map(str::to_string);
                if parsed.media.len() < MAX_INBOUND_MEDIA_RECORDS {
                    parsed.media.push(PlatformInboundMedia {
                        kind: PlatformMediaKind::Video,
                        id: file_id.clone(),
                        name: Some(name.clone()),
                        url: url.clone(),
                    });
                }
                if parsed.files.len() < MAX_INBOUND_FILES && (file_id.is_some() || url.is_some()) {
                    parsed.files.push(FileRef { file_id, name, url });
                }
            }
            "face" | "record" if parsed.media.len() < MAX_INBOUND_MEDIA_RECORDS => {
                parsed.media.push(PlatformInboundMedia {
                    kind: match kind {
                        "face" => PlatformMediaKind::Emoji,
                        "record" => PlatformMediaKind::Audio,
                        _ => PlatformMediaKind::Other,
                    },
                    id: data
                        .get("id")
                        .and_then(value_id_string)
                        .or_else(|| data.get("file_id").and_then(value_id_string))
                        .and_then(bounded_onebot_id),
                    name: data
                        .get("name")
                        .and_then(Value::as_str)
                        .map(|name| bounded_chars(name, MAX_INBOUND_FILE_NAME_CHARS)),
                    url: data
                        .get("url")
                        .and_then(Value::as_str)
                        .filter(|url| url.starts_with("http") && url.len() <= 4096)
                        .map(str::to_string),
                });
            }
            // Other OneBot segments carry no turn input.
            _ => {}
        }
    }
    parsed
}

pub(in crate::platforms::onebot) fn onebot_id_value(value: &str) -> Value {
    value
        .trim()
        .parse::<i64>()
        .map(Value::from)
        .unwrap_or_else(|_| Value::String(value.trim().to_string()))
}

pub(in crate::platforms::onebot) fn parse_message_info(
    data: &Value,
    self_id: i64,
) -> Option<PlatformMessageInfo> {
    let message_id = data.get("message_id").and_then(value_id_string)?;
    let parsed = parse_message(data.get("message"), data.get("raw_message"), self_id);
    let sender = data.get("sender");
    let sender_id = sender
        .and_then(|sender| sender.get("user_id"))
        .and_then(value_id_string)
        .or_else(|| data.get("user_id").and_then(value_id_string))
        .unwrap_or_default();
    let sender_display_name = sender
        .and_then(|sender| sender.get("card"))
        .and_then(Value::as_str)
        .filter(|name| !name.trim().is_empty())
        .or_else(|| {
            sender
                .and_then(|sender| sender.get("nickname"))
                .and_then(Value::as_str)
        })
        .unwrap_or("?")
        .to_string();
    let conversation_kind = match data.get("message_type").and_then(Value::as_str) {
        Some("group") => Some(ConversationKind::Group),
        Some("private") => Some(ConversationKind::Private),
        _ => None,
    };
    let conversation_id = data
        .get("group_id")
        .and_then(value_id_string)
        // 私聊的会话 id 是**对方**,不是自己。`target_id`/`peer_id` 在
        // user→bot 的消息里指向机器人,原来不过滤就直接采信,于是
        // `get_msg` 回来的会话 id 成了自己的号,与 `Target::Private` 期望的
        // 对方号对不上——`adapter.rs` 的归属校验判成"属于另一个会话",
        // vision_analyze 取历史图必失败(08-30 测具实测,报错原文
        // "the requested image message belongs to another conversation")。
        //
        // 三个来源统一按"不是我"过滤:bot 发出的消息里 target_id 是对方
        // (保留),user_id 是自己(跳过);用户发来的消息反过来。两个方向都
        // 落到同一个人身上。
        .or_else(|| {
            data.get("target_id")
                .and_then(value_id_string)
                .filter(|id| id != &self_id.to_string())
        })
        .or_else(|| {
            data.get("peer_id")
                .and_then(value_id_string)
                .filter(|id| id != &self_id.to_string())
        })
        .or_else(|| {
            data.get("user_id")
                .and_then(value_id_string)
                .filter(|id| id != &self_id.to_string())
        })
        .or_else(|| {
            (conversation_kind == Some(ConversationKind::Private)
                && sender_id != self_id.to_string())
            .then(|| sender_id.clone())
        });
    Some(PlatformMessageInfo {
        message_id,
        sender_id,
        sender_display_name,
        timestamp: data.get("time").and_then(Value::as_i64).unwrap_or(0),
        text: parsed.text,
        reply_to_message_id: parsed.reply_to_message_id,
        mentioned_user_ids: parsed.mentioned_user_ids,
        mentioned_users: Vec::new(),
        media: parsed.media,
        conversation_kind,
        conversation_id,
    })
}

pub(in crate::platforms::onebot) fn parse_group_member(
    data: &Value,
    fallback_group_id: i64,
) -> Option<PlatformGroupMember> {
    Some(PlatformGroupMember {
        group_id: data
            .get("group_id")
            .and_then(value_id_string)
            .unwrap_or_else(|| fallback_group_id.to_string()),
        user_id: data.get("user_id").and_then(value_id_string)?,
        nickname: data
            .get("nickname")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        card: data
            .get("card")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        role: data
            .get("role")
            .and_then(Value::as_str)
            .unwrap_or("member")
            .to_string(),
        title: data
            .get("title")
            .and_then(Value::as_str)
            .or_else(|| data.get("special_title").and_then(Value::as_str))
            .unwrap_or_default()
            .to_string(),
        joined_at: data.get("join_time").and_then(Value::as_i64).unwrap_or(0),
        last_active_at: data
            .get("last_sent_time")
            .and_then(Value::as_i64)
            .unwrap_or(0),
    })
}

pub(in crate::platforms::onebot) fn group_member_mute_until(data: &Value) -> Option<i64> {
    data.get("shut_up_timestamp").and_then(|value| {
        value
            .as_i64()
            .or_else(|| value.as_u64().and_then(|value| i64::try_from(value).ok()))
            .or_else(|| value.as_str().and_then(|value| value.trim().parse().ok()))
    })
}
