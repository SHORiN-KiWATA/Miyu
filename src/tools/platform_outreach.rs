//! `send_qq_message`:本地会话(REPL / WebUI / shellhook)里让模型把消息发到
//! 用户的 QQ。只在 `platforms.terminal_outreach` 打开时注册,平台会话不注册
//! (那边有 `send_message_to_user`)。收件人只能是 `qq.admin_users` 里的号码:
//! `to` 的可选项按 `qq.admin_aliases` 的别名列出(没别名显示号码),不传发给
//! 第一个(主管理员)。`voice: true` 走文本转语音发语音消息。

use super::{ToolRegistry, ToolSpec};
use crate::config::AppConfig;
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};

pub const TOOL_NAME: &str = "send_qq_message";

/// NapCat 的反向 WebSocket 是否已连上(至少一个账号在线)。工具只在连上时
/// 注册:掉线时模型看不到它,不会对着断线的通道尝试。
pub fn qq_connected() -> bool {
    crate::web::voice_bridge::daemon_state().is_some_and(|state| {
        !state
            .platforms
            .onebot
            .lock()
            .unwrap()
            .connected_accounts()
            .is_empty()
    })
}

/// (QQ 号, 显示名)按配置顺序;第一个是主管理员。
fn recipients(config: &AppConfig) -> Vec<(i64, String)> {
    let qq = &config.platforms.qq;
    qq.admin_users
        .iter()
        .map(|id| {
            let label = qq
                .admin_aliases
                .get(&id.to_string())
                .map(|alias| alias.trim())
                .filter(|alias| !alias.is_empty())
                .map(str::to_string)
                .unwrap_or_else(|| id.to_string());
            (*id, label)
        })
        .collect()
}

pub fn register(registry: &mut ToolRegistry, config: &AppConfig) {
    let list = recipients(config);
    let labels: Vec<String> = list.iter().map(|(_, label)| label.clone()).collect();
    let primary = labels.first().cloned().unwrap_or_default();
    let mut to_schema = json!({
        "type": "string",
        "description": format!("Recipient. Omit to reach the primary administrator ({primary})."),
    });
    if !labels.is_empty() {
        to_schema["enum"] = Value::Array(labels.into_iter().map(Value::String).collect());
    }
    registry.register(
        ToolSpec::new(
            TOOL_NAME,
            "Send a message to the user's QQ, as text or as a spoken voice message.",
            json!({
                "type": "object",
                "properties": {
                    "text": { "type": "string", "description": "Message text (spoken text when voice is true)." },
                    "voice": { "type": "boolean", "description": "Send as a voice message instead of text." },
                    "to": to_schema
                },
                "required": ["text"],
                "additionalProperties": false
            }),
            move |arguments| async move { send(arguments).await },
        )
        .writes(),
    );
}

async fn send(arguments: Value) -> Result<String> {
    let text = arguments
        .get("text")
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or_default()
        .to_string();
    if text.is_empty() {
        bail!("text is required");
    }
    let voice = arguments
        .get("voice")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let state = crate::web::voice_bridge::daemon_state()
        .context("send_qq_message only works inside the daemon")?;
    let (allowed, list) = {
        let manager = state.manager.lock().unwrap();
        (
            manager.config.platforms.terminal_outreach,
            recipients(&manager.config),
        )
    };
    if !allowed {
        bail!("sending to messaging platforms from the terminal is disabled in settings");
    }
    let Some((primary, _)) = list.first() else {
        bail!("no administrator QQ id is configured (接入通讯平台 → 允许使用终端的管理员 QQ 号)");
    };
    let target = match arguments.get("to").and_then(Value::as_str).map(str::trim) {
        Some(to) if !to.is_empty() => list
            .iter()
            .find(|(id, label)| label == to || id.to_string() == to)
            .map(|(id, _)| *id)
            .with_context(|| {
                let allowed: Vec<&str> = list.iter().map(|(_, label)| label.as_str()).collect();
                format!("unknown recipient {to}; allowed: {allowed:?}")
            })?,
        _ => *primary,
    };
    let kind = if voice {
        let path = crate::web::voice_bridge::synthesize_for_platform(&text).await?;
        let outcome = crate::platforms::onebot::proactive::send_direct(
            state,
            None,
            "private",
            &target.to_string(),
            crate::platform_types::OutboundMessage::segments(
                crate::platform_types::OutboundOrigin::Tool,
                vec![crate::platform_types::OutboundSegment::AudioPath {
                    path: path.clone(),
                    transcript: text.clone(),
                }],
            ),
        )
        .await;
        let _ = std::fs::remove_file(&path);
        outcome?;
        "voice"
    } else {
        crate::platforms::onebot::proactive::send_direct_text(
            state,
            None,
            "private",
            &target.to_string(),
            &text,
        )
        .await?;
        "text"
    };
    Ok(json!({ "ok": true, "kind": kind, "to": target }).to_string())
}
