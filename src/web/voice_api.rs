//! 语音功能的 HTTP 面(WebUI 设置页与麦克风按钮的后端):
//! - `GET /api/voice/status`:二进制是否在、前端是否在跑、采集设备;
//! - `GET /api/voice/devices`:麦克风列表(问 `miyu-voice devices`);
//! - `GET /api/voice/stream`(WebSocket):流式听写。浏览器把 16kHz 单声道
//!   PCM16 LE 以二进制帧持续推上来,daemon 转给 `miyu-voice` 做 VAD/分句/
//!   识别,识别出一句就回一条文本帧 `{"type":"dictation","text"}`;静默
//!   超窗回 `{"type":"ended"}`;浏览器发文本帧 `stop` 或直接断开即结束;
//! - `POST /api/voice/transcribe`:整段 16k 单声道 PCM WAV → 文本(外部脚本用)。

use crate::web::*;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use voice_bridge::DictationRelay;

/// 浏览器一段听写录音的上限:16kHz 16-bit 单声道 60 秒 ≈ 1.9MB,留余量。
pub(in crate::web) const VOICE_UPLOAD_LIMIT: usize = 4 * 1024 * 1024;

pub(in crate::web) async fn voice_status(
    State(state): State<DaemonState>,
    headers: HeaderMap,
) -> std::result::Result<Json<Value>, ApiError> {
    require_auth(&headers, &state)?;
    Ok(Json(voice_bridge::status(&state)))
}

pub(in crate::web) async fn voice_devices(
    State(state): State<DaemonState>,
    headers: HeaderMap,
) -> std::result::Result<Json<Value>, ApiError> {
    require_auth(&headers, &state)?;
    let Some(binary) = voice_bridge::locate_binary() else {
        return Ok(json_devices(Vec::new(), Some("miyu-voice not installed")));
    };
    let output = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        tokio::process::Command::new(binary)
            .arg("devices")
            .stdin(std::process::Stdio::null())
            .output(),
    )
    .await;
    match output {
        Ok(Ok(output)) if output.status.success() => {
            // 每行 `源名<TAB>描述`;老版本只有一列时描述取源名。
            let devices = String::from_utf8_lossy(&output.stdout)
                .lines()
                .map(str::trim)
                .filter(|line| !line.is_empty())
                .map(|line| {
                    let (name, label) = line.split_once('\t').unwrap_or((line, line));
                    json!({ "value": name.trim(), "label": label.trim() })
                })
                .collect();
            Ok(json_devices(devices, None))
        }
        Ok(Ok(output)) => Ok(json_devices(
            Vec::new(),
            Some(&String::from_utf8_lossy(&output.stderr)),
        )),
        Ok(Err(error)) => Ok(json_devices(Vec::new(), Some(&error.to_string()))),
        Err(_) => Ok(json_devices(Vec::new(), Some("timed out listing devices"))),
    }
}

/// `GET /api/voice/tts/voices`:按已保存的 MiniMax 播报配置拉音色列表。
pub(in crate::web) async fn voice_tts_voices(
    State(state): State<DaemonState>,
    headers: HeaderMap,
) -> std::result::Result<Json<Value>, ApiError> {
    require_auth(&headers, &state)?;
    let cfg = state.manager.lock().unwrap().config.voice.tts.minimax.clone();
    match voice_tts::list_minimax_voices(&cfg).await {
        Ok(voices) => Ok(Json(json!({ "voices": voices, "error": null }))),
        Err(error) => Ok(Json(json!({ "voices": [], "error": format!("{error:#}") }))),
    }
}

#[derive(serde::Deserialize)]
pub(in crate::web) struct TtsPreviewRequest {
    #[serde(default)]
    text: String,
}

/// `POST /api/voice/tts/preview`:用当前(已保存的)TTS 配置合成并播一句。
pub(in crate::web) async fn voice_tts_preview(
    State(state): State<DaemonState>,
    headers: HeaderMap,
    Json(request): Json<TtsPreviewRequest>,
) -> std::result::Result<Json<Value>, ApiError> {
    require_mutation(&headers, &state)?;
    let text = if request.text.trim().is_empty() {
        let preview = state.manager.lock().unwrap().config.voice.tts.preview_text.clone();
        if preview.trim().is_empty() {
            "今天也是充满希望的一天".to_string()
        } else {
            preview
        }
    } else {
        request.text
    };
    voice_bridge::wait_attached_public(&state)
        .await
        .map_err(|message| ApiError::new(StatusCode::SERVICE_UNAVAILABLE, message))?;
    voice_bridge::speak(&state, &text)
        .await
        .map_err(|error| ApiError::new(StatusCode::BAD_GATEWAY, format!("{error:#}")))?;
    Ok(Json(json!({ "ok": true })))
}

pub(in crate::web) async fn voice_stream(
    State(state): State<DaemonState>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Response {
    if let Err(error) = require_auth(&headers, &state) {
        return error.into_response();
    }
    ws.on_upgrade(move |socket| stream_dictation(state, socket))
}

fn ws_json(value: Value) -> Message {
    Message::Text(value.to_string().into())
}

/// 一条浏览器听写连接的生命周期:认领 → 音频上行/文本下行 → 释放。
async fn stream_dictation(state: DaemonState, mut socket: WebSocket) {
    let mut relay = match voice_bridge::claim_dictation(&state, true).await {
        Ok(relay) => relay,
        Err(message) => {
            let _ = socket
                .send(ws_json(json!({ "type": "error", "message": message })))
                .await;
            let _ = socket.send(Message::Close(None)).await;
            return;
        }
    };
    if socket.send(ws_json(json!({ "type": "ready" }))).await.is_err() {
        voice_bridge::release_dictation();
        return;
    }
    loop {
        tokio::select! {
            event = relay.recv() => match event {
                Some(DictationRelay::Utterance(text)) => {
                    if socket
                        .send(ws_json(json!({ "type": "dictation", "text": text })))
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
                Some(DictationRelay::Ended) | None => {
                    let _ = socket.send(ws_json(json!({ "type": "ended" }))).await;
                    break;
                }
            },
            message = socket.recv() => match message {
                Some(Ok(Message::Binary(bytes))) => voice_bridge::push_audio(&bytes),
                Some(Ok(Message::Text(text))) if text.as_str().trim() == "stop" => break,
                Some(Ok(Message::Text(_) | Message::Ping(_) | Message::Pong(_))) => {}
                Some(Ok(Message::Close(_))) | Some(Err(_)) | None => break,
            },
        }
    }
    voice_bridge::release_dictation();
    let _ = socket.send(Message::Close(None)).await;
}

fn json_devices(devices: Vec<Value>, error: Option<&str>) -> Json<Value> {
    Json(json!({ "devices": devices, "error": error.map(|text| text.trim().to_string()) }))
}

pub(in crate::web) async fn voice_transcribe(
    State(state): State<DaemonState>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> std::result::Result<Json<Value>, ApiError> {
    require_auth(&headers, &state)?;
    if body.len() < 44 {
        return Err(ApiError::new(StatusCode::BAD_REQUEST, "empty audio"));
    }
    match voice_bridge::transcribe_wav(&state, &body).await {
        Ok(text) => Ok(Json(json!({ "text": text }))),
        Err(error) => Err(ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            format!("{error:#}"),
        )),
    }
}
