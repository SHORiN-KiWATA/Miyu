//! `speak` 工具:让模型主动开口——把一段文本经播报供应商合成后从扬声器播出。
//! 只在 `voice.enabled` 时注册;播报供应商未激活时调用返回错误说明,不会崩。
//! 语音对话的回复本身会按 `<speak>` 协议自动朗读,工具是给打字会话和
//! 主动提醒用的。

use super::{ToolRegistry, ToolSpec};
use serde_json::json;

pub const TOOL_NAME: &str = "speak";

pub fn register(registry: &mut ToolRegistry) {
    registry.register(ToolSpec::new(
        TOOL_NAME,
        "Speak: say the given text out loud through the speaker with text-to-speech.",
        json!({
            "type": "object",
            "properties": {
                "text": {
                    "type": "string",
                    "description": "What to say, in natural spoken language (a few sentences at most)."
                }
            },
            "required": ["text"],
            "additionalProperties": false
        }),
        move |arguments| async move {
            let text = arguments
                .get("text")
                .and_then(serde_json::Value::as_str)
                .map(str::trim)
                .unwrap_or_default()
                .to_string();
            if text.is_empty() {
                anyhow::bail!("text is empty");
            }
            crate::web::voice_bridge::speak_from_tool(&text).await?;
            Ok("Spoken.".to_string())
        },
    ));
}
