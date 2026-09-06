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

/// `miyu listen`:让前端直接进入等待指令状态(等价于唤醒词命中),
/// 绑到合成器快捷键上就是"按键呼叫"。成功时不输出,便于静默绑定。
pub(in crate::cli) async fn run_listen(paths: &MiyuPaths) -> Result<()> {
    let mut stream = ipc::connect(&paths.ipc_socket())
        .await
        .context(t("Miyu daemon is not running", "Miyu daemon 未运行"))?;
    ipc::send(&mut stream, &IpcRequest::new(IpcCommand::VoiceListen)).await?;
    match ipc::receive::<IpcFrame>(&mut stream).await? {
        Some(IpcFrame::Ack) => Ok(()),
        Some(IpcFrame::Error { message, .. }) => bail!("{message}"),
        other => bail!("unexpected reply to VoiceListen: {other:?}"),
    }
}

/// `miyu voice …`:播报试听、语音会话清空/回看、前端状态。
pub(in crate::cli) async fn run_voice_command(
    paths: &MiyuPaths,
    command: crate::cli::args::VoiceCommand,
) -> Result<()> {
    use crate::cli::args::VoiceCommand;
    match command {
        VoiceCommand::Say { text } => {
            let mut stream = connect_daemon(paths).await?;
            ipc::send(
                &mut stream,
                &IpcRequest::new(IpcCommand::VoiceSpeak { text, tts: None }),
            )
            .await?;
            expect_ack(&mut stream, "VoiceSpeak").await
        }
        VoiceCommand::Reset => {
            let mut stream = connect_daemon(paths).await?;
            ipc::send(&mut stream, &IpcRequest::new(IpcCommand::VoiceReset)).await?;
            expect_ack(&mut stream, "VoiceReset").await?;
            eprintln!(
                "\x1b[2m{}\x1b[0m",
                t("voice session cleared", "语音会话已清空")
            );
            Ok(())
        }
        VoiceCommand::Status => {
            let mut stream = connect_daemon(paths).await?;
            ipc::send(&mut stream, &IpcRequest::new(IpcCommand::VoiceStatus)).await?;
            match ipc::receive::<IpcFrame>(&mut stream).await? {
                Some(IpcFrame::Event { data, .. }) => {
                    println!("{}", serde_json::to_string_pretty(&data)?);
                    Ok(())
                }
                Some(IpcFrame::Error { message, .. }) => bail!("{message}"),
                other => bail!("unexpected reply to VoiceStatus: {other:?}"),
            }
        }
        VoiceCommand::History { limit } => {
            let marker = paths.state_dir.join("voice-session-id");
            let Ok(session_id) = std::fs::read_to_string(&marker) else {
                eprintln!(
                    "\x1b[2m{}\x1b[0m",
                    t("no voice session yet", "还没有语音会话")
                );
                return Ok(());
            };
            let session_id = session_id.trim().to_string();
            let state = StateStore::new(paths)?.pinned(&session_id);
            let turns = state.load_visible_turns()?;
            let start = turns.len().saturating_sub(limit);
            for turn in &turns[start..] {
                let raw = turn.display_content.trim();
                let user = raw
                    .strip_prefix("<voice_input>")
                    .and_then(|rest| rest.strip_suffix("</voice_input>"))
                    .unwrap_or(raw);
                println!(
                    "\x1b[1m{} user\x1b[0m\n{}\n",
                    turn.user_timestamp,
                    user.trim()
                );
                let assistant =
                    crate::agent::prompt_strip_tagged(turn.assistant_content.clone(), "speak");
                println!(
                    "\x1b[1m{} assistant\x1b[0m\n{}\n",
                    turn.assistant_timestamp.as_deref().unwrap_or("-"),
                    assistant.trim()
                );
            }
            Ok(())
        }
    }
}

async fn connect_daemon(paths: &MiyuPaths) -> Result<tokio::net::UnixStream> {
    ipc::connect(&paths.ipc_socket())
        .await
        .context(t("Miyu daemon is not running", "Miyu daemon 未运行"))
}

async fn expect_ack(stream: &mut tokio::net::UnixStream, what: &str) -> Result<()> {
    match ipc::receive::<IpcFrame>(stream).await? {
        Some(IpcFrame::Ack) => Ok(()),
        Some(IpcFrame::Error { message, .. }) => bail!("{message}"),
        other => bail!("unexpected reply to {what}: {other:?}"),
    }
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
