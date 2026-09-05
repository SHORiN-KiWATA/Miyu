//! 不经 AI 回合的主动直发通道。
//!
//! 定时消息插件到点后把固定文本直接投递到会话——没有入站事件、没有 agent、
//! 没有回合调度，只借用现成的 `OneBotAdapter` 分帧与发送逻辑。

use crate::platforms::onebot::*;

/// 把一段纯文本直接发到指定 QQ 会话。
///
/// `account` 为空时用当前已连接的第一个账号；`conversation_kind` 只接受
/// `group` / `private`。任何一步失败都返回错误，由调用方决定日志与重试策略。
pub(crate) async fn send_direct_text(
    state: &DaemonState,
    account: Option<i64>,
    conversation_kind: &str,
    conversation_id: &str,
    text: &str,
) -> Result<()> {
    let text = text.trim();
    if text.is_empty() {
        bail!("scheduled message text is empty");
    }
    send_direct(
        state,
        account,
        conversation_kind,
        conversation_id,
        OutboundMessage::text(OutboundOrigin::Plugin, text),
    )
    .await
}

/// 同 [`send_direct_text`],但可以带任意出站消息(语音段等)。
pub(crate) async fn send_direct(
    state: &DaemonState,
    account: Option<i64>,
    conversation_kind: &str,
    conversation_id: &str,
    message: OutboundMessage,
) -> Result<()> {
    let registry = state.platforms.onebot.clone();
    let (self_id, conn) = {
        let locked = registry.lock().unwrap();
        let self_id = match account {
            Some(id) => id,
            None => *locked
                .connected_accounts()
                .first()
                .context("no QQ account is connected")?,
        };
        let conn = locked
            .handle(self_id)
            .context("the QQ account is not connected")?;
        (self_id, conn)
    };
    let target_id: i64 = conversation_id
        .parse()
        .context("invalid QQ conversation id for a scheduled message")?;
    let target = match conversation_kind {
        "group" => Target::Group {
            group_id: target_id,
        },
        "private" => Target::Private { user_id: target_id },
        other => bail!("unsupported QQ conversation kind: {other}"),
    };
    let max_reply_chars = state
        .manager
        .lock()
        .unwrap()
        .config
        .platforms
        .qq
        .max_reply_chars;
    let adapter = OneBotAdapter {
        conn,
        registry,
        http: state.platforms.http_client()?,
        self_id,
        target,
        max_reply_chars,
        file_store_lock: state.platforms.file_store_lock.clone(),
    };
    adapter.send_message(message).await?;
    Ok(())
}
