//! 图片输入的解析与视觉模型的选路。
//!
//! 用户发的图片可能来自剪贴板、平台消息、或历史里的占位符。三条路都要落到同一
//! 组路径上，再决定交给谁：`should_use_active_text_pool_for_images` 判断当前文
//! 本模型池能不能直接吃图，不能就走独立的视觉工具。
//!
//! 判定要求池里**每个**模型都支持视觉（`active_text_pool_supports_vision`），
//! 因为池内是随机选的——只要有一个不支持，就会随机地失败。

use crate::agent::*;

pub(in crate::agent) fn queued_prompt_images(
    prompt: &QueuedPrompt,
) -> Result<Vec<Option<PastedImage>>> {
    prompt
        .attachments
        .iter()
        .map(|attachment| match attachment {
            QueuedPromptAttachment::Binary { mime, data_base64 } => {
                let data = base64::engine::general_purpose::STANDARD
                    .decode(data_base64)
                    .map_err(|error| anyhow::anyhow!("invalid queued image data: {error}"))?;
                Ok(Some(PastedImage::Binary(ClipboardImage::new(
                    mime.clone(),
                    data,
                ))))
            }
            QueuedPromptAttachment::Path { path } => Ok(Some(PastedImage::Path(path.clone()))),
        })
        .collect()
}

pub(in crate::agent) fn clipboard_binary_image_from_tool_result(
    tool_name: &str,
    output: &str,
) -> Option<ClipboardImage> {
    if tool_name != "read_clipboard" {
        return None;
    }
    let value = serde_json::from_str::<Value>(output).ok()?;
    if value.get("ok").and_then(Value::as_bool) != Some(true) {
        return None;
    }
    if value.get("kind").and_then(Value::as_str) != Some("clipboard") {
        return None;
    }
    if value.get("content_type").and_then(Value::as_str) != Some("image") {
        return None;
    }
    if value.get("source").and_then(Value::as_str) != Some("clipboard_binary") {
        return None;
    }
    let path = value.get("path").and_then(Value::as_str)?;
    let mime = value
        .get("mime")
        .and_then(Value::as_str)
        .unwrap_or("image/png")
        .to_string();
    let data = std::fs::read(path).ok()?;
    Some(ClipboardImage::new(mime, data))
}

/// 会在工具结果之后追加媒体块的工具。历史重放只对这些回合查一次
/// `turn_inline_media`,别的回合不多付一次查询。
pub(in crate::agent) const INLINE_MEDIA_TOOLS: &[&str] = &["vision_analyze", "read_clipboard"];

/// 某次工具调用之后要追加给模型的媒体:剪贴板图片(旧路)与
/// `vision_analyze` 的 inline 寄存(09-03)走同一条出口。
pub(in crate::agent) fn inline_media_from_tool_result(
    tool_name: &str,
    output: &str,
) -> Vec<crate::state::TurnInlineMedia> {
    if let Some(image) = clipboard_binary_image_from_tool_result(tool_name, output) {
        return vec![crate::state::TurnInlineMedia {
            call_id: String::new(),
            seq: 0,
            kind: crate::state::INLINE_MEDIA_KIND_IMAGE.to_string(),
            mime: image.mime.clone(),
            source: "clipboard".to_string(),
            data: Some(image.data),
        }];
    }
    crate::tools::vision::inline::take_from_output(output)
}

/// 把媒体块组成紧跟 tool 消息的那条用户消息。活体与重放共用这一个函数,
/// 字节才会一致。纯文本(视觉旁路的描述)用 plain 形态;含图片/视频用
/// parts 形态。本地视频按需从文件读,文件没了就跳过这一块。
pub(in crate::agent) fn inline_media_message(
    items: &[crate::state::TurnInlineMedia],
) -> Option<ChatMessage> {
    use crate::state::{INLINE_MEDIA_KIND_IMAGE, INLINE_MEDIA_KIND_TEXT, INLINE_MEDIA_KIND_VIDEO};
    if items.is_empty() {
        return None;
    }
    if items.iter().all(|item| item.kind == INLINE_MEDIA_KIND_TEXT) {
        let text = items
            .iter()
            .filter_map(|item| item.data.as_deref())
            .map(|data| String::from_utf8_lossy(data).into_owned())
            .collect::<Vec<_>>()
            .join("\n\n");
        return (!text.trim().is_empty()).then(|| ChatMessage::plain("user", text));
    }
    let mut parts = Vec::with_capacity(items.len());
    for item in items {
        match item.kind.as_str() {
            INLINE_MEDIA_KIND_TEXT => {
                if let Some(data) = item.data.as_deref() {
                    parts.push(ChatContentPart::Text {
                        text: String::from_utf8_lossy(data).into_owned(),
                    });
                }
            }
            INLINE_MEDIA_KIND_IMAGE => {
                if let Some(url) = inline_media_url(item) {
                    parts.push(ChatContentPart::ImageUrl {
                        image_url: ImageUrlContent { url },
                    });
                }
            }
            INLINE_MEDIA_KIND_VIDEO => {
                if let Some(url) = inline_media_url(item) {
                    parts.push(ChatContentPart::VideoUrl {
                        video_url: crate::llm::VideoUrlContent { url },
                    });
                }
            }
            _ => {}
        }
    }
    (!parts.is_empty()).then(|| ChatMessage::user_parts(parts))
}

/// 媒体块的内容 parts(不含文本块的包装);供"图进工具结果"形态使用。
fn inline_media_parts(items: &[crate::state::TurnInlineMedia]) -> Vec<ChatContentPart> {
    use crate::state::{INLINE_MEDIA_KIND_IMAGE, INLINE_MEDIA_KIND_TEXT, INLINE_MEDIA_KIND_VIDEO};
    let mut parts = Vec::with_capacity(items.len());
    for item in items {
        match item.kind.as_str() {
            INLINE_MEDIA_KIND_TEXT => {
                if let Some(data) = item.data.as_deref() {
                    parts.push(ChatContentPart::Text {
                        text: String::from_utf8_lossy(data).into_owned(),
                    });
                }
            }
            INLINE_MEDIA_KIND_IMAGE => {
                if let Some(url) = inline_media_url(item) {
                    parts.push(ChatContentPart::ImageUrl {
                        image_url: ImageUrlContent { url },
                    });
                }
            }
            INLINE_MEDIA_KIND_VIDEO => {
                if let Some(url) = inline_media_url(item) {
                    parts.push(ChatContentPart::VideoUrl {
                        video_url: crate::llm::VideoUrlContent { url },
                    });
                }
            }
            _ => {}
        }
    }
    parts
}

/// 把工具结果连同它的媒体块推进对话。两种形态,活体与重放共用,字节一致:
/// - `tool_form`:媒体块直接进 tool 消息的内容 parts(文本在前)——模型看到
///   的就是"工具返回了图",与 Claude Code 的 Read 同构(09-03 用户裁定)。
/// - 否则:tool 消息照旧是文本,紧跟一条带媒体块的用户消息(供应商不认
///   tool 消息里的图时的退路)。
pub(in crate::agent) fn push_tool_result_with_media(
    messages: &mut Vec<ChatMessage>,
    mut tool_message: ChatMessage,
    items: &[crate::state::TurnInlineMedia],
    tool_form: bool,
) {
    if items.is_empty() {
        messages.push(tool_message);
        return;
    }
    if tool_form {
        let parts = inline_media_parts(items);
        if !parts.is_empty() {
            let text = match tool_message.content.take() {
                Some(crate::llm::ChatContent::Text(text)) => text,
                Some(crate::llm::ChatContent::Parts(existing)) => existing
                    .into_iter()
                    .filter_map(|part| match part {
                        ChatContentPart::Text { text } => Some(text),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n"),
                None => String::new(),
            };
            let mut content = vec![ChatContentPart::Text { text }];
            content.extend(parts);
            tool_message.content = Some(crate::llm::ChatContent::Parts(content));
        }
        messages.push(tool_message);
        return;
    }
    messages.push(tool_message);
    if let Some(message) = inline_media_message(items) {
        messages.push(message);
    }
}

fn inline_media_url(item: &crate::state::TurnInlineMedia) -> Option<String> {
    use base64::Engine;
    if let Some(data) = item.data.as_deref() {
        let encoded = base64::engine::general_purpose::STANDARD.encode(data);
        return Some(format!("data:{};base64,{encoded}", item.mime));
    }
    if item.source.starts_with("http://") || item.source.starts_with("https://") {
        return Some(item.source.clone());
    }
    match std::fs::read(&item.source) {
        Ok(bytes) => {
            let encoded = base64::engine::general_purpose::STANDARD.encode(bytes);
            Some(format!("data:{};base64,{encoded}", item.mime))
        }
        Err(error) => {
            tracing::warn!(source = %item.source, %error, "inline media file is gone; skipping it on replay");
            None
        }
    }
}

pub(in crate::agent) fn resolve_pasted_image_paths(
    images: &[Option<PastedImage>],
    paths: &MiyuPaths,
    image_platform: Option<&str>,
) -> Vec<Option<String>> {
    images
        .iter()
        .enumerate()
        .map(|(i, image)| match image {
            Some(PastedImage::Binary(img)) => image_platform
                .map(|platform| {
                    img.write_cache_file(
                        &paths.cache_dir,
                        &PathBuf::from("platform_images").join(platform),
                    )
                })
                .unwrap_or_else(|| img.write_temp_file(&paths.cache_dir, i + 1))
                .ok()
                .map(|path| path.display().to_string()),
            Some(PastedImage::Path(path)) => Some(path.clone()),
            None => None,
        })
        .collect()
}

pub(in crate::agent) fn rewrite_image_placeholders_with_paths(
    input: &str,
    paths: &[Option<String>],
) -> String {
    let mut output = String::new();
    let mut rest = input;
    // 视频占位符走同一套改写:漏掉 `[Video N]` 的话,没能内联的视频就连路径都
    // 传不到模型面前,`vision_analyze` 也就无从下手(08-28)。
    while let Some(start) = crate::clipboard::MEDIA_PLACEHOLDER_PREFIXES
        .iter()
        .filter_map(|prefix| rest.find(prefix))
        .min()
    {
        output.push_str(&rest[..start]);
        let after_start = &rest[start..];
        let Some(end) = after_start.find(']') else {
            output.push_str(after_start);
            return output;
        };
        let placeholder = &after_start[..=end];
        if let Some(index) = image_placeholder_index(placeholder) {
            if let Some(Some(path)) = paths.get(index - 1) {
                // 标签原样保留,别把视频改写成图片。
                let label = crate::clipboard::media_placeholder_prefix(placeholder)
                    .unwrap_or("[Image ")
                    .trim_start_matches('[')
                    .trim_end();
                output.push_str(&format!("[{label} {index}: {path}]"));
            } else {
                output.push_str(placeholder);
            }
        } else {
            output.push_str(placeholder);
        }
        rest = &after_start[end + 1..];
    }
    output.push_str(rest);
    output
}

pub(in crate::agent) fn image_placeholder_index(placeholder: &str) -> Option<usize> {
    let inner = placeholder
        .strip_prefix(crate::clipboard::media_placeholder_prefix(placeholder)?)?
        .strip_suffix(']')?
        .trim_start();
    let num: String = inner.chars().take_while(|c| c.is_ascii_digit()).collect();
    let index = num.parse::<usize>().ok()?;
    (index > 0).then_some(index)
}

pub(in crate::agent) fn vision_analysis_progress(tick: usize) -> String {
    let dots = match tick % 3 {
        1 => ".",
        2 => "..",
        _ => "...",
    };
    if crate::i18n::is_zh() {
        format!("视觉分析{dots}")
    } else {
        format!("Vision analysis{dots}")
    }
}

pub(in crate::agent) fn active_text_pool_supports_vision(config: &AppConfig) -> bool {
    let choices = config.active_provider_model_choices();
    !choices.is_empty()
        && choices.iter().all(|choice| {
            config.model_supports_any_input(&choice.provider_id, &choice.model, &["image"])
        })
}

/// 活跃文本池能不能直接吃视频。
///
/// 与 `active_text_pool_supports_vision` 同构:要**池里每个模型**都支持,否则
/// 轮到不支持的那个就会带着一段它读不懂的内容块发过去。
pub(in crate::agent) fn active_text_pool_supports_video(config: &AppConfig) -> bool {
    let choices = config.active_provider_model_choices();
    !choices.is_empty()
        && choices.iter().all(|choice| {
            config.model_supports_any_input(&choice.provider_id, &choice.model, &["video"])
        })
}

pub(in crate::agent) fn should_use_active_text_pool_for_images(config: &AppConfig) -> bool {
    config.plugins.vision.prefer_current_multimodal_model
        && active_text_pool_supports_vision(config)
}
