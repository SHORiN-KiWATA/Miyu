//! `miyu stt`:终端里说一句,识别成文字后当作一条消息发出去,回复照常
//! 流式打印——shellhook/一次性 CLI 的语音形态。
//!
//! 音频来自本机麦克风(daemon 让 `miyu-voice` 开听写窗),这里只拿文字。
//! 识别出第一句就关掉听写并提交;静默 10 秒没说话则空手退出。

use crate::cli::*;

pub(in crate::cli) async fn run_stt_once(
    paths: &MiyuPaths,
    plain: bool,
    mode: AgentMode,
    session: TurnSession,
) -> Result<()> {
    let text = dictate_one_sentence(paths).await?;
    let Some(text) = text else {
        eprintln!(
            "\x1b[2m{}\x1b[0m",
            t("nothing heard, giving up", "没听到内容,退出")
        );
        return Ok(());
    };
    eprintln!("\x1b[2m» {text}\x1b[0m");
    run_chat_with_options(paths, text, None, plain, mode, session).await
}

/// 认领一条听写流,拿到第一句非空文本就返回;窗口结束返回 None。
pub(in crate::cli) async fn dictate_one_sentence(paths: &MiyuPaths) -> Result<Option<String>> {
    let mut stream = ipc::connect(&paths.ipc_socket())
        .await
        .context(t("Miyu daemon is not running", "Miyu daemon 未运行"))?;
    ipc::send(&mut stream, &IpcRequest::new(IpcCommand::StartDictation)).await?;
    match ipc::receive::<IpcFrame>(&mut stream).await? {
        Some(IpcFrame::Ack) => {}
        Some(IpcFrame::Error { message, .. }) => bail!("{message}"),
        other => bail!("unexpected reply to StartDictation: {other:?}"),
    }
    eprintln!(
        "\x1b[2m{}\x1b[0m",
        t(
            "listening… speak now (Ctrl+C to cancel)",
            "在听…请讲(Ctrl+C 取消)"
        )
    );
    loop {
        let frame = tokio::select! {
            frame = ipc::receive::<IpcFrame>(&mut stream) => frame?,
            _ = tokio::signal::ctrl_c() => return Ok(None),
        };
        match frame {
            Some(IpcFrame::Event { kind, data, .. }) => match kind.as_str() {
                "voice.dictation" => {
                    let text = data
                        .get("text")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or_default()
                        .trim()
                        .to_string();
                    if !text.is_empty() {
                        // 连接一断,daemon 就释放听写、恢复唤醒。
                        return Ok(Some(text));
                    }
                }
                "voice.dictation_ended" => return Ok(None),
                _ => {}
            },
            Some(IpcFrame::Error { message, .. }) => bail!("{message}"),
            Some(_) => {}
            None => return Ok(None),
        }
    }
}
