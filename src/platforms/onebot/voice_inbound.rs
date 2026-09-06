//! 入站 QQ 语音消息 → 文字。
//!
//! 语音段本身只有一个文件名,模型看不见内容。这里在建 inbound_event 之前
//! 把它转成文字接进正文:NapCat `get_record`(要 wav)→ daemon 的语音前端
//! (`miyu-voice`,SenseVoice)转写 → `[语音] 文本`。语音前端没开、没装、
//! 取不到文件、转写失败——一律静默退化成 `[语音消息]` 占位,不打扰任何人
//! (09-05 用户裁定:缺东西应该静默)。转过的语音段从 media 里摘掉,免得
//! 历史库里 `[语音] 文本` 后面再挂一个 `[audio id=…]`。

use crate::platforms::onebot::*;

/// 一条消息里最多转写几段语音(QQ 一条消息通常只有一段)。
const MAX_VOICE_SEGMENTS: usize = 2;
/// 语音文件上限:QQ 语音最长 60s,wav 也就几 MB。
const MAX_RECORD_BYTES: usize = 20 * 1024 * 1024;

pub(in crate::platforms::onebot) async fn attach_voice_transcripts(
    state: &DaemonState,
    conn: &ConnectionHandle,
    parsed: &mut InboundMessage,
) {
    if !parsed
        .media
        .iter()
        .any(|media| media.kind == PlatformMediaKind::Audio)
    {
        return;
    }
    let voices: Vec<PlatformInboundMedia> = parsed
        .media
        .iter()
        .filter(|media| media.kind == PlatformMediaKind::Audio)
        .cloned()
        .collect();
    parsed
        .media
        .retain(|media| media.kind != PlatformMediaKind::Audio);
    let stt_ready = crate::web::voice_bridge::stt_available(state);
    let mut lines = Vec::new();
    for media in voices.iter().take(MAX_VOICE_SEGMENTS) {
        let transcript = if stt_ready {
            match transcribe_record(state, conn, media).await {
                Ok(text) => text,
                Err(error) => {
                    tracing::debug!(
                        target: "miyu::qq",
                        error = %error,
                        file = ?media.id,
                        "{}",
                        t("QQ voice message could not be transcribed", "QQ 语音消息转写失败")
                    );
                    String::new()
                }
            }
        } else {
            String::new()
        };
        lines.push(if transcript.trim().is_empty() {
            "[语音消息]".to_string()
        } else {
            crate::platform_types::voice_history_text(&transcript)
        });
    }
    let addition = lines.join("\n");
    if parsed.text.trim().is_empty() {
        parsed.text = addition;
    } else {
        parsed.text = format!("{}\n{addition}", parsed.text.trim_end());
    }
    parsed.text_chars = parsed.text.chars().count();
}

/// `get_record` 要 wav → 三种来源(base64 / 桥本机路径 / 直链)按顺序试 →
/// 交给语音前端转写。
async fn transcribe_record(
    state: &DaemonState,
    conn: &ConnectionHandle,
    media: &PlatformInboundMedia,
) -> Result<String> {
    let file = media
        .id
        .clone()
        .or_else(|| media.url.clone())
        .context("voice segment carries no file reference")?;
    let data = conn
        .call_api_with_timeout(
            "get_record",
            json!({ "file": file, "out_format": "wav" }),
            FILE_DOWNLOAD_TIMEOUT,
        )
        .await
        .context("get_record")?;
    let sources = parse_platform_file_sources(&data);
    anyhow::ensure!(!sources.is_empty(), "get_record returned no usable source");
    let mut last_error = None;
    for source in sources {
        let bytes = match source {
            PlatformFileSource::Bytes(bytes) => Ok(bytes),
            PlatformFileSource::LocalPath(path) => match tokio::fs::metadata(&path).await {
                Ok(meta) if meta.len() as usize <= MAX_RECORD_BYTES => {
                    tokio::fs::read(&path).await.map_err(anyhow::Error::from)
                }
                Ok(_) => Err(anyhow::anyhow!("record file exceeds the size cap")),
                Err(error) => Err(anyhow::Error::from(error)),
            },
            PlatformFileSource::Url(url) => fetch_url_capped(state, &url).await,
        };
        match bytes {
            Ok(bytes) if !bytes.is_empty() => {
                return crate::web::voice_bridge::transcribe_wav(state, &bytes).await;
            }
            Ok(_) => last_error = Some(anyhow::anyhow!("record file is empty")),
            Err(error) => last_error = Some(error),
        }
    }
    Err(last_error.unwrap_or_else(|| anyhow::anyhow!("no reachable record source")))
}

async fn fetch_url_capped(state: &DaemonState, url: &str) -> Result<Vec<u8>> {
    let client = state.platforms.http_client()?;
    let response = client
        .get(url)
        .timeout(FILE_DOWNLOAD_TIMEOUT)
        .send()
        .await?
        .error_for_status()?;
    let bytes = response.bytes().await?;
    anyhow::ensure!(
        bytes.len() <= MAX_RECORD_BYTES,
        "record download exceeds the size cap"
    );
    Ok(bytes.to_vec())
}
