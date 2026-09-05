//! 语音会话控制工具:模型判断用户想结束语音对话时调用,关闭免唤醒窗口。
//! 经 voice_bridge 向语音前端发关窗信令;前端不在(语音关闭/非 daemon
//! 进程)时是无操作,安全。只在 `voice.enabled` 时注册。

use super::{ToolRegistry, ToolSpec};
use serde_json::json;

pub const TOOL_NAME: &str = "end_voice_chat";

pub fn register(registry: &mut ToolRegistry) {
    registry.register(ToolSpec::new(
        TOOL_NAME,
        "Call when the user signals the hands-free voice conversation is over (e.g. \"没事了\", \"就这样\", \"去忙吧\", goodbye). Closes the voice window so the wake word is required again. Only meaningful during a voice conversation; harmless otherwise.",
        json!({
            "type": "object",
            "properties": {},
            "additionalProperties": false
        }),
        move |_arguments| async move {
            crate::web::voice_bridge::end_voice_chat_hook();
            Ok("Voice window closed; back to wake-word standby.".to_string())
        },
    ));
}
