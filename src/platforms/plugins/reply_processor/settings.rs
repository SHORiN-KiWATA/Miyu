//! 插件设置与会话级覆盖。
//!
//! 覆盖是**按会话**存的：同一个插件在不同群可以是不同模式。管理员命令改的是
//! 当前会话的覆盖，不是全局默认——改全局需要走配置。

use crate::platforms::plugins::reply_processor::*;

pub(crate) const OVERRIDES_KEY: &str = "session_overrides";

pub(crate) const IMAGE_NOTICES_KEY: &str = "image_notices";

pub(crate) const IMAGE_METADATA_KEY: &str = "reply_processor.image_notice";

pub(crate) const MAX_THRESHOLD: usize = 100_000;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ReplyMode {
    Image,
    Forward,
}

impl ReplyMode {
    pub(crate) fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "图片" | "图" | "转图片" | "文转图" | "image" | "img" => Some(Self::Image),
            "转发" | "合并转发" | "forward" => Some(Self::Forward),
            _ => None,
        }
    }

    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Image => "长文转图片",
            Self::Forward => "合并转发",
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct ReplyProcessorConfig {
    pub(crate) default_enabled: bool,
    pub(crate) threshold: usize,
    pub(crate) mode: ReplyMode,
    pub(crate) followup_mention: bool,
    pub(crate) strip_period: bool,
    pub(crate) theme: String,
    pub(crate) max_height: u32,
    pub(crate) font_size: u32,
    pub(crate) code_font_size: u32,
    pub(crate) padding: u32,
    pub(crate) context_notice: bool,
    pub(crate) ttl_hours: u64,
    pub(crate) max_records: usize,
    pub(crate) send_tool_intercept: bool,
    pub(crate) font: String,
    pub(crate) title_font: String,
    pub(crate) code_font: String,
    pub(crate) emoji_font: String,
}

impl Default for ReplyProcessorConfig {
    fn default() -> Self {
        Self {
            default_enabled: true,
            threshold: 300,
            mode: ReplyMode::Image,
            followup_mention: true,
            strip_period: true,
            theme: "paper".to_string(),
            max_height: 2600,
            font_size: 36,
            code_font_size: 30,
            padding: 64,
            context_notice: true,
            ttl_hours: 24,
            max_records: 3,
            send_tool_intercept: true,
            font: String::new(),
            title_font: String::new(),
            code_font: String::new(),
            emoji_font: String::new(),
        }
    }
}

impl ReplyProcessorConfig {
    pub(crate) fn from_context(context: &PlatformTurnContext) -> Self {
        let mut config = Self::default();
        let Some(instance) = context.config.platforms.qq.plugins.get(PLUGIN_ID) else {
            return config;
        };
        let settings = &instance.settings;
        config.default_enabled = bool_setting(settings, "default_enabled", config.default_enabled);
        config.threshold = usize_setting(settings, "threshold", config.threshold, 1, MAX_THRESHOLD);
        config.mode = string_setting(settings, "mode")
            .as_deref()
            .and_then(ReplyMode::parse)
            .unwrap_or(config.mode);
        config.followup_mention =
            bool_setting(settings, "followup_mention", config.followup_mention);
        config.strip_period = bool_setting(settings, "strip_period", config.strip_period);
        config.theme = match string_setting(settings, "theme").as_deref() {
            Some("light") => "light",
            Some("dark") => "dark",
            _ => "paper",
        }
        .to_string();
        config.max_height = usize_setting(
            settings,
            "max_height",
            config.max_height as usize,
            1000,
            5000,
        ) as u32;
        config.font_size =
            usize_setting(settings, "font_size", config.font_size as usize, 24, 56) as u32;
        config.code_font_size = usize_setting(
            settings,
            "code_font_size",
            config.code_font_size as usize,
            20,
            46,
        ) as u32;
        config.padding =
            usize_setting(settings, "padding", config.padding as usize, 36, 120) as u32;
        config.context_notice = bool_setting(settings, "context_notice", config.context_notice);
        config.ttl_hours =
            usize_setting(settings, "ttl_hours", config.ttl_hours as usize, 1, 168) as u64;
        config.max_records = usize_setting(settings, "max_records", config.max_records, 1, 10);
        config.send_tool_intercept =
            bool_setting(settings, "send_tool_intercept", config.send_tool_intercept);
        config.font = string_setting(settings, "font").unwrap_or_default();
        config.title_font = string_setting(settings, "title_font").unwrap_or_default();
        config.code_font = string_setting(settings, "code_font").unwrap_or_default();
        config.emoji_font = string_setting(settings, "emoji_font").unwrap_or_default();
        config
    }

    pub(crate) fn render_config(&self) -> RenderConfig {
        RenderConfig {
            theme: self.theme.clone(),
            max_height: self.max_height,
            font_size: self.font_size,
            code_font_size: self.code_font_size,
            padding: self.padding,
            font: self.font.clone(),
            title_font: self.title_font.clone(),
            code_font: self.code_font.clone(),
            emoji_font: self.emoji_font.clone(),
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct EffectiveSettings {
    pub(crate) enabled: bool,
    pub(crate) threshold: usize,
    pub(crate) mode: ReplyMode,
    pub(crate) custom: bool,
    pub(crate) config: ReplyProcessorConfig,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub(crate) struct SessionOverrides {
    pub(crate) enabled: Option<bool>,
    pub(crate) threshold: Option<usize>,
    pub(crate) mode: Option<ReplyMode>,
}

impl SessionOverrides {
    pub(crate) fn is_empty(&self) -> bool {
        self.enabled.is_none() && self.threshold.is_none() && self.mode.is_none()
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub(crate) struct ImageNotice {
    pub(crate) timestamp: i64,
    pub(crate) char_count: usize,
    pub(crate) image_count: usize,
    #[serde(default, rename = "preview", skip_serializing_if = "Option::is_none")]
    pub(crate) legacy_preview: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) message_ids: Vec<String>,
}

pub(crate) fn normalize_notices(
    notices: Vec<ImageNotice>,
    config: &ReplyProcessorConfig,
) -> Vec<ImageNotice> {
    let cutoff = unix_timestamp().saturating_sub((config.ttl_hours * 60 * 60) as i64);
    let mut recent = notices
        .into_iter()
        .filter(|notice| notice.timestamp >= cutoff)
        .map(|mut notice| {
            notice.legacy_preview = None;
            notice
        })
        .collect::<Vec<_>>();
    recent.sort_by_key(|notice| notice.timestamp);
    if recent.len() > config.max_records {
        recent.drain(..recent.len() - config.max_records);
    }
    recent
}

pub(crate) fn reply_command(text: &str) -> Option<&str> {
    let text = text.trim();
    let command = text
        .strip_prefix("/回复处理")
        .or_else(|| text.strip_prefix("回复处理"))?;
    if !command.is_empty() && !command.starts_with(char::is_whitespace) {
        return None;
    }
    Some(command.trim())
}

pub(crate) fn bool_setting(
    settings: &serde_json::Map<String, Value>,
    key: &str,
    default: bool,
) -> bool {
    settings
        .get(key)
        .and_then(Value::as_bool)
        .unwrap_or(default)
}

pub(crate) fn usize_setting(
    settings: &serde_json::Map<String, Value>,
    key: &str,
    default: usize,
    min: usize,
    max: usize,
) -> usize {
    settings
        .get(key)
        .and_then(Value::as_u64)
        .and_then(|value| usize::try_from(value).ok())
        .filter(|value| (min..=max).contains(value))
        .unwrap_or(default)
}

pub(crate) fn string_setting(
    settings: &serde_json::Map<String, Value>,
    key: &str,
) -> Option<String> {
    settings
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

pub(crate) fn message_text(message: &OutboundMessage) -> String {
    let OutboundBody::Segments(segments) = &message.body else {
        return String::new();
    };
    segments
        .iter()
        .filter_map(|segment| match segment {
            OutboundSegment::Markdown(text) | OutboundSegment::Text(text) => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

pub(crate) fn message_contains_file(message: &OutboundMessage) -> bool {
    matches!(
        &message.body,
        OutboundBody::Segments(segments)
            if segments
                .iter()
                .any(|segment| matches!(segment, OutboundSegment::FilePath { .. }))
    )
}

pub(crate) fn replace_text_segments(
    mut message: OutboundMessage,
    mut replacement: Vec<OutboundSegment>,
) -> OutboundMessage {
    let OutboundBody::Segments(segments) = &mut message.body else {
        return message;
    };
    let mut output = Vec::with_capacity(segments.len() + replacement.len());
    let mut inserted = false;
    for segment in std::mem::take(segments) {
        if matches!(
            segment,
            OutboundSegment::Markdown(_) | OutboundSegment::Text(_)
        ) {
            if !inserted {
                output.append(&mut replacement);
                inserted = true;
            }
        } else {
            output.push(segment);
        }
    }
    *segments = output;
    message
}

pub(crate) fn strip_trailing_chinese_period(message: &mut OutboundMessage) {
    let OutboundBody::Segments(segments) = &mut message.body else {
        return;
    };
    for segment in segments.iter_mut().rev() {
        let text = match segment {
            OutboundSegment::Markdown(text) | OutboundSegment::Text(text) => text,
            OutboundSegment::Mention(_)
            | OutboundSegment::ImageBytes { .. }
            | OutboundSegment::ImagePath { .. }
            | OutboundSegment::FilePath { .. }
            | OutboundSegment::AudioPath { .. } => continue,
        };
        let trimmed_len = text.trim_end().len();
        if trimmed_len == 0 {
            continue;
        }
        if text[..trimmed_len].ends_with('。') {
            let period_start = trimmed_len - '。'.len_utf8();
            text.replace_range(period_start..trimmed_len, "");
        }
        break;
    }
}

pub(crate) fn unix_timestamp() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0)
}
