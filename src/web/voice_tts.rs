//! 回复播报:把回合的回复变成能读出来的文本,再合成语音。
//!
//! 三层:模型按 `<voice-protocol>`(见 `agent::prompt::VOICE_PROTOCOL`)在回复
//! 末尾给 `<speak>` 口语版 → 没给就把正文清洗一遍(去代码块/行内代码/链接/
//! 路径/Markdown 记号)兜底 → 截到 `max_chars`。合成按 `tts.active` 分派:
//! MiniMax `t2a_v2`(返回 hex 编码的 wav)或小米 MiMo `chat/completions`
//! (`mimo-v2.5-tts` 系列,返回 base64 的 wav);播放在 miyu-voice 里。

use crate::config::{MimoTtsConfig, MiniMaxTtsConfig, VoiceTtsConfig};
use anyhow::{Context, Result};
use base64::Engine as _;
use serde_json::{json, Value};

const SPEAK_OPEN: &str = "<speak>";
const SPEAK_CLOSE: &str = "</speak>";

/// 取 `<speak>` 块内容(多块拼接)。没有返回 None。
pub(crate) fn extract_speak(reply: &str) -> Option<String> {
    let mut rest = reply;
    let mut parts: Vec<&str> = Vec::new();
    while let Some(start) = rest.find(SPEAK_OPEN) {
        let after = &rest[start + SPEAK_OPEN.len()..];
        let Some(end) = after.find(SPEAK_CLOSE) else {
            // 没闭合(被截断):剩余全算
            parts.push(after.trim());
            break;
        };
        parts.push(after[..end].trim());
        rest = &after[end + SPEAK_CLOSE.len()..];
    }
    let joined = parts
        .into_iter()
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    (!joined.is_empty()).then_some(joined)
}

/// 正文去掉 `<speak>` 块(给实录/通知用)。
pub(crate) fn strip_speak(reply: &str) -> String {
    crate::agent::prompt_strip_tagged(reply.to_string(), "speak")
        .trim()
        .to_string()
}

/// 把 Markdown 正文清洗成勉强能读的文本:代码块整块丢弃,行内代码去反引号,
/// 链接/路径换成「链接」「路径」,标题/列表/加粗记号剥掉,表格行丢弃。
pub(crate) fn sanitize_for_speech(text: &str) -> String {
    let mut out: Vec<String> = Vec::new();
    let mut in_fence = false;
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            in_fence = !in_fence;
            continue;
        }
        if in_fence || trimmed.is_empty() {
            continue;
        }
        if trimmed.starts_with('|') {
            continue;
        }
        let mut cleaned = trimmed
            .trim_start_matches(|ch: char| {
                ch == '#' || ch == '>' || ch == '-' || ch == '*' || ch == '+'
            })
            .trim()
            .to_string();
        // 有序列表 "1. "
        if let Some(rest) = cleaned
            .split_once(". ")
            .filter(|(head, _)| head.chars().all(|ch| ch.is_ascii_digit()) && !head.is_empty())
            .map(|(_, rest)| rest.to_string())
        {
            cleaned = rest;
        }
        cleaned = cleaned.replace("**", "").replace("__", "").replace('`', "");
        cleaned = replace_tokens(&cleaned);
        if !cleaned.trim().is_empty() {
            out.push(cleaned.trim().to_string());
        }
    }
    out.join(" ")
}

/// 逐词替换:URL → 「链接」,像路径的 → 「路径」。
fn replace_tokens(line: &str) -> String {
    line.split_whitespace()
        .map(|word| {
            let bare = word.trim_matches(|ch: char| ",。,;;:()()[]【】\"'<>".contains(ch));
            if bare.starts_with("http://")
                || bare.starts_with("https://")
                || bare.starts_with("www.")
            {
                "链接".to_string()
            } else if looks_like_path(bare) {
                "路径".to_string()
            } else {
                word.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn looks_like_path(word: &str) -> bool {
    let slashes = word.matches('/').count();
    (word.starts_with('/') || word.starts_with("~/") || word.starts_with("./"))
        && slashes >= 1
        && word.len() > 2
        || (slashes >= 2 && !word.contains("://"))
}

/// 截到 `max` 字,尽量落在句末。
fn clip_spoken(text: &str, max: usize) -> String {
    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= max {
        return text.to_string();
    }
    let head = &chars[..max];
    match head
        .iter()
        .rposition(|ch| "。!?!?;;".contains(*ch))
        .filter(|&pos| pos >= max / 5)
    {
        Some(pos) => head[..=pos].iter().collect(),
        None => head.iter().collect(),
    }
}

/// 播报文本:优先 `<speak>`,否则清洗正文;空回复给一句「办好了」。
pub(crate) fn spoken_text(reply: &str, tts: &VoiceTtsConfig) -> String {
    let text = extract_speak(reply)
        .map(|speak| sanitize_for_speech(&speak))
        .filter(|speak| !speak.trim().is_empty())
        .unwrap_or_else(|| sanitize_for_speech(&strip_speak(reply)));
    let text = text.trim();
    if text.is_empty() {
        return crate::i18n::text("done", "办好了").to_string();
    }
    clip_spoken(text, tts.max_chars.max(20))
}

/// 按 `tts.active` 分派合成:文本 → wav 字节。
pub(crate) async fn synthesize(tts: &VoiceTtsConfig, text: &str) -> Result<Vec<u8>> {
    match tts.provider() {
        Some("mimo") => synthesize_mimo(&tts.mimo, text).await,
        Some("minimax") | None => synthesize_minimax(&tts.minimax, text).await,
        Some(other) => anyhow::bail!("未知的播报供应商 {other}"),
    }
}

/// 某供应商的音色列表 `[{ "value", "label" }]`(设置页下拉 / TUI 浏览)。
pub(crate) async fn list_voices(tts: &VoiceTtsConfig, provider: &str) -> Result<Vec<Value>> {
    match provider {
        "mimo" => Ok(list_mimo_voices()),
        "minimax" => list_minimax_voices(&tts.minimax).await,
        other => anyhow::bail!("未知的播报供应商 {other}"),
    }
}

// ---------------------------------------------------------------------------
// 小米 MiMo
// ---------------------------------------------------------------------------

/// MiMo 预置音色(文档 2026-09):(id, 显示名)。id 本身就是中文名。
pub(crate) const MIMO_VOICES: &[(&str, &str)] = &[
    ("mimo_default", "MiMo 默认"),
    ("冰糖", "冰糖 · 中文女声"),
    ("茉莉", "茉莉 · 中文女声"),
    ("苏打", "苏打 · 中文男声"),
    ("白桦", "白桦 · 中文男声"),
    ("Mia", "Mia · 英文女声"),
    ("Chloe", "Chloe · 英文女声"),
    ("Milo", "Milo · 英文男声"),
    ("Dean", "Dean · 英文男声"),
];

/// MiMo 模型:预置音色 / 按描述造音色 / 按样本克隆。
pub(crate) const MIMO_MODELS: &[&str] = &[
    "mimo-v2.5-tts",
    "mimo-v2.5-tts-voicedesign",
    "mimo-v2.5-tts-voiceclone",
];

pub(crate) fn list_mimo_voices() -> Vec<Value> {
    MIMO_VOICES
        .iter()
        .map(|(id, label)| json!({ "value": id, "label": label }))
        .collect()
}

fn mimo_base(cfg: &MimoTtsConfig) -> String {
    let base = cfg.base_url.trim().trim_end_matches('/');
    let base = if base.is_empty() {
        "https://api.xiaomimimo.com/v1"
    } else {
        base
    };
    if base.ends_with("/v1") {
        base.to_string()
    } else {
        format!("{base}/v1")
    }
}

/// MiMo 的 key(支持 `$env:VAR` 引用)。
pub(crate) fn mimo_api_key(cfg: &MimoTtsConfig) -> Result<String> {
    let raw = cfg
        .api_key
        .as_deref()
        .map(str::trim)
        .filter(|key| !key.is_empty())
        .context("MiMo 播报没有配置 api_key")?;
    let mut keys = Vec::new();
    crate::config::append_resolved_api_keys(&mut keys, raw)?;
    keys.into_iter()
        .map(|key| key.value)
        .find(|key| !key.trim().is_empty())
        .context("MiMo 播报的 api_key 解析为空")
}

/// 参考音频 → `data:audio/wav;base64,…`(voiceclone 模型的 `voice` 字段)。
fn mimo_clone_voice(cfg: &MimoTtsConfig) -> Result<String> {
    let path = cfg
        .sample_audio
        .as_deref()
        .map(str::trim)
        .filter(|path| !path.is_empty())
        .context("MiMo voiceclone 模型需要参考音频(sample_audio)")?;
    let path = match path.strip_prefix("~/") {
        Some(rest) => std::env::var_os("HOME")
            .map(|home| std::path::PathBuf::from(home).join(rest))
            .unwrap_or_else(|| std::path::PathBuf::from(path)),
        None => std::path::PathBuf::from(path),
    };
    let bytes = std::fs::read(&path).with_context(|| format!("读取参考音频 {}", path.display()))?;
    let mime = match path
        .extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| ext.to_ascii_lowercase())
        .as_deref()
    {
        Some("mp3") => "audio/mpeg",
        Some("wav") => "audio/wav",
        other => anyhow::bail!("参考音频只支持 wav / mp3,不是 {other:?}"),
    };
    let encoded = base64::engine::general_purpose::STANDARD.encode(&bytes);
    anyhow::ensure!(
        encoded.len() <= 10 * 1024 * 1024,
        "参考音频 base64 后超过 10MB"
    );
    Ok(format!("data:{mime};base64,{encoded}"))
}

/// 组 `chat/completions` 请求体:待合成文本在 assistant 消息(风格标签作前缀),
/// 指令/音色描述在 user 消息;`audio.voice` 按模型:预置 id 或克隆样本 data URI。
pub(crate) fn mimo_request_body(cfg: &MimoTtsConfig, text: &str) -> Result<Value> {
    let model = cfg.model.trim();
    let model = if model.is_empty() {
        "mimo-v2.5-tts"
    } else {
        model
    };
    let mut messages = Vec::new();
    let prompt = cfg.prompt.trim();
    if model.ends_with("voicedesign") {
        anyhow::ensure!(
            !prompt.is_empty(),
            "MiMo voicedesign 模型需要在提示词里写一句音色描述"
        );
    }
    if !prompt.is_empty() {
        messages.push(json!({ "role": "user", "content": prompt }));
    }
    // 标签之间 MiMo 要空格;配置里(TUI 多选)存的是逗号分隔,顿号/中文逗号也认。
    let style = cfg
        .style
        .split(|ch: char| ch == ',' || ch == '，' || ch == '、' || ch.is_whitespace())
        .map(str::trim)
        .filter(|tag| !tag.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    let content = if style.is_empty() {
        text.to_string()
    } else {
        format!("({style}){text}")
    };
    messages.push(json!({ "role": "assistant", "content": content }));
    let mut audio = json!({ "format": "wav" });
    if model.ends_with("voiceclone") {
        audio["voice"] = Value::String(mimo_clone_voice(cfg)?);
    } else if !model.ends_with("voicedesign") {
        let voice = cfg.voice.trim();
        audio["voice"] = Value::String(
            if voice.is_empty() {
                "mimo_default"
            } else {
                voice
            }
            .to_string(),
        );
    }
    Ok(json!({
        "model": model,
        "messages": messages,
        "audio": audio,
        "stream": false,
    }))
}

/// 从 `chat/completions` 应答里取音频:`choices[0].message.audio.data`(base64 wav)。
/// 错误按 OpenAI 风格 `error.message` 报。
pub(crate) fn mimo_extract_audio(status: u16, payload: &Value) -> Result<Vec<u8>> {
    let error = payload
        .pointer("/error/message")
        .or_else(|| payload.pointer("/error"))
        .or_else(|| payload.pointer("/message"))
        .and_then(|value| match value {
            Value::String(text) => Some(text.clone()),
            other if !other.is_null() => Some(other.to_string()),
            _ => None,
        });
    if !(200..300).contains(&status) {
        anyhow::bail!(
            "MiMo 合成失败(HTTP {status}):{}",
            error.unwrap_or_else(|| "unknown".to_string())
        );
    }
    let encoded = payload
        .pointer("/choices/0/message/audio/data")
        .and_then(Value::as_str)
        .or_else(|| payload.pointer("/data").and_then(Value::as_str))
        .with_context(|| match error {
            Some(message) => format!("MiMo 合成失败:{message}"),
            None => "MiMo 应答没有 choices[0].message.audio.data".to_string(),
        })?;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(encoded.trim())
        .context("MiMo 音频 base64 解码")?;
    anyhow::ensure!(bytes.len() > 44, "MiMo 返回的音频为空");
    Ok(bytes)
}

/// MiMo `chat/completions`:文本 → wav 字节(24kHz 单声道)。
pub(crate) async fn synthesize_mimo(cfg: &MimoTtsConfig, text: &str) -> Result<Vec<u8>> {
    let key = mimo_api_key(cfg)?;
    let body = mimo_request_body(cfg, text)?;
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(90))
        .build()?;
    // 文档里 curl 例子用 `api-key` 头,SDK 例子走 Bearer;两个都带。
    let response = client
        .post(format!("{}/chat/completions", mimo_base(cfg)))
        .header("Authorization", format!("Bearer {key}"))
        .header("api-key", &key)
        .json(&body)
        .send()
        .await
        .context("请求 MiMo chat/completions")?;
    let status = response.status().as_u16();
    let raw = response.text().await.context("读取 MiMo 应答")?;
    let payload: Value = serde_json::from_str(&raw).unwrap_or_else(
        |_| json!({ "error": { "message": crate::web::voice_bridge::clip(raw.trim(), 200) } }),
    );
    mimo_extract_audio(status, &payload)
}

// ---------------------------------------------------------------------------
// MiniMax
// ---------------------------------------------------------------------------

fn minimax_base(cfg: &MiniMaxTtsConfig) -> String {
    let base = cfg.base_url.trim().trim_end_matches('/');
    let base = if base.is_empty() {
        "https://api.minimaxi.com/v1"
    } else {
        base
    };
    if base.ends_with("/v1") {
        base.to_string()
    } else {
        format!("{base}/v1")
    }
}

/// MiniMax 的 key(支持 `$env:VAR` 引用)。
pub(crate) fn minimax_api_key(cfg: &MiniMaxTtsConfig) -> Result<String> {
    let raw = cfg
        .api_key
        .as_deref()
        .map(str::trim)
        .filter(|key| !key.is_empty())
        .context("MiniMax 播报没有配置 api_key")?;
    let mut keys = Vec::new();
    crate::config::append_resolved_api_keys(&mut keys, raw)?;
    keys.into_iter()
        .map(|key| key.value)
        .find(|key| !key.trim().is_empty())
        .context("MiniMax 播报的 api_key 解析为空")
}

fn minimax_client(cfg: &MiniMaxTtsConfig) -> Result<(reqwest::Client, String)> {
    let key = minimax_api_key(cfg)?;
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(60))
        .build()?;
    Ok((client, format!("Bearer {key}")))
}

/// MiniMax `t2a_v2`:文本 → wav 字节(24kHz 单声道)。
pub(crate) async fn synthesize_minimax(tts: &MiniMaxTtsConfig, text: &str) -> Result<Vec<u8>> {
    let (client, auth) = minimax_client(tts)?;
    let mut voice_setting = json!({
        "voice_id": tts.voice_id,
        "speed": tts.speed.clamp(0.5, 2.0),
        "vol": tts.vol.clamp(0.1, 10.0),
        "pitch": tts.pitch.clamp(-12, 12),
    });
    if !tts.emotion.trim().is_empty() {
        voice_setting["emotion"] = Value::String(tts.emotion.trim().to_string());
    }
    let body = json!({
        "model": tts.model,
        "text": text,
        "stream": false,
        "output_format": "hex",
        "language_boost": if tts.language_boost.trim().is_empty() { "auto" } else { tts.language_boost.trim() },
        "voice_setting": voice_setting,
        "audio_setting": { "sample_rate": 24000, "format": "wav", "channel": 1 },
    });
    let response = client
        .post(format!("{}/t2a_v2", minimax_base(tts)))
        .header("Authorization", &auth)
        .json(&body)
        .send()
        .await
        .context("请求 MiniMax t2a_v2")?;
    let status = response.status();
    let payload: Value = response.json().await.context("解析 MiniMax 应答")?;
    let code = payload
        .pointer("/base_resp/status_code")
        .and_then(Value::as_i64)
        .unwrap_or(-1);
    if !status.is_success() || code != 0 {
        let message = payload
            .pointer("/base_resp/status_msg")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        anyhow::bail!("MiniMax 合成失败(HTTP {status},status_code {code}):{message}");
    }
    let audio_hex = payload
        .pointer("/data/audio")
        .and_then(Value::as_str)
        .context("MiniMax 应答没有 data.audio")?;
    let bytes = hex::decode(audio_hex.trim()).context("MiniMax 音频 hex 解码")?;
    anyhow::ensure!(bytes.len() > 44, "MiniMax 返回的音频为空");
    Ok(bytes)
}

/// MiniMax `get_voice`:系统音色 + 克隆音色,给设置页下拉用。
/// 返回 `[{ "value": voice_id, "label": 名称 }]`。
pub(crate) async fn list_minimax_voices(tts: &MiniMaxTtsConfig) -> Result<Vec<Value>> {
    let (client, auth) = minimax_client(tts)?;
    let response = client
        .post(format!("{}/get_voice", minimax_base(tts)))
        .header("Authorization", &auth)
        .json(&json!({ "voice_type": "all" }))
        .send()
        .await
        .context("请求 MiniMax get_voice")?;
    let payload: Value = response.json().await.context("解析 MiniMax 音色列表")?;
    let code = payload
        .pointer("/base_resp/status_code")
        .and_then(Value::as_i64)
        .unwrap_or(-1);
    if code != 0 {
        let message = payload
            .pointer("/base_resp/status_msg")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        anyhow::bail!("MiniMax get_voice 失败(status_code {code}):{message}");
    }
    let mut out = Vec::new();
    for (group, prefix) in [
        ("system_voice", ""),
        ("voice_cloning", "克隆:"),
        ("voice_generation", "生成:"),
    ] {
        if let Some(items) = payload.get(group).and_then(Value::as_array) {
            for item in items {
                let Some(id) = item.get("voice_id").and_then(Value::as_str) else {
                    continue;
                };
                let name = item
                    .get("voice_name")
                    .and_then(Value::as_str)
                    .filter(|name| !name.trim().is_empty())
                    .unwrap_or(id);
                let description = item
                    .get("description")
                    .and_then(Value::as_array)
                    .map(|list| {
                        list.iter()
                            .filter_map(Value::as_str)
                            .collect::<Vec<_>>()
                            .join(" ")
                    })
                    .unwrap_or_default();
                let label = if description.is_empty() || description == name {
                    format!("{prefix}{name}")
                } else {
                    format!(
                        "{prefix}{name} — {}",
                        crate::web::voice_bridge::clip(&description, 40)
                    )
                };
                out.push(json!({ "value": id, "label": label }));
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mimo_cfg() -> MimoTtsConfig {
        MimoTtsConfig {
            api_key: Some("sk-test".to_string()),
            ..Default::default()
        }
    }

    /// 44 字节 WAV 头 + 若干采样,够 `mimo_extract_audio` 的"非空"判定。
    fn fake_wav() -> Vec<u8> {
        let mut wav = b"RIFF\0\0\0\0WAVEfmt ".to_vec();
        wav.extend(std::iter::repeat_n(0u8, 24));
        wav.extend(b"data\0\0\0\0");
        wav.extend(std::iter::repeat_n(7u8, 480));
        wav
    }

    #[test]
    fn mimo_request_body_follows_docs() {
        // 预置音色:文本在 assistant,风格标签作前缀,指令在 user。
        let mut cfg = mimo_cfg();
        cfg.voice = "冰糖".to_string();
        cfg.style = "温柔,慵懒".to_string();
        cfg.prompt = "语速稍快,像在跟朋友聊天".to_string();
        let body = mimo_request_body(&cfg, "今天也是充满希望的一天").unwrap();
        assert_eq!(body["model"], "mimo-v2.5-tts");
        assert_eq!(body["stream"], false);
        assert_eq!(body["audio"]["format"], "wav");
        assert_eq!(body["audio"]["voice"], "冰糖");
        let messages = body["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0]["role"], "user");
        assert_eq!(messages[0]["content"], "语速稍快,像在跟朋友聊天");
        // 旧键名 instruction 照样读进 prompt。
        let legacy: MimoTtsConfig = serde_json::from_str(r#"{"instruction":"温柔一点"}"#).unwrap();
        assert_eq!(legacy.prompt, "温柔一点");
        assert_eq!(messages[1]["role"], "assistant");
        assert_eq!(messages[1]["content"], "(温柔 慵懒)今天也是充满希望的一天");

        // 无指令无标签:只有 assistant 一条,文本原样。
        let body = mimo_request_body(&mimo_cfg(), "你好").unwrap();
        let messages = body["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0]["content"], "你好");
        assert_eq!(body["audio"]["voice"], "mimo_default");

        // voicedesign:必须有描述,且不带 voice。
        let mut cfg = mimo_cfg();
        cfg.model = "mimo-v2.5-tts-voicedesign".to_string();
        assert!(mimo_request_body(&cfg, "你好").is_err());
        cfg.prompt = "二十岁女声,清亮".to_string();
        let body = mimo_request_body(&cfg, "你好").unwrap();
        assert!(body["audio"].get("voice").is_none());
        assert_eq!(body["messages"][0]["content"], "二十岁女声,清亮");

        // voiceclone:参考音频变 data URI。
        let dir = std::env::temp_dir().join(format!("miyu-mimo-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let sample = dir.join("ref.wav");
        std::fs::write(&sample, fake_wav()).unwrap();
        let mut cfg = mimo_cfg();
        cfg.model = "mimo-v2.5-tts-voiceclone".to_string();
        assert!(mimo_request_body(&cfg, "你好").is_err(), "no sample yet");
        cfg.sample_audio = Some(sample.display().to_string());
        let body = mimo_request_body(&cfg, "你好").unwrap();
        let voice = body["audio"]["voice"].as_str().unwrap();
        assert!(voice.starts_with("data:audio/wav;base64,"), "{voice}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn mimo_extract_audio_reads_choices_and_errors() {
        let wav = fake_wav();
        let encoded = base64::engine::general_purpose::STANDARD.encode(&wav);
        let payload = json!({
            "choices": [{ "message": { "audio": { "data": encoded, "format": "wav" } } }]
        });
        assert_eq!(mimo_extract_audio(200, &payload).unwrap(), wav);

        let error = mimo_extract_audio(
            401,
            &json!({ "error": { "message": "invalid api key", "type": "auth" } }),
        )
        .unwrap_err();
        assert!(error.to_string().contains("401"), "{error}");
        assert!(error.to_string().contains("invalid api key"), "{error}");

        // 200 但没音频(比如文本模型回了文字):报缺字段。
        let error = mimo_extract_audio(
            200,
            &json!({ "choices": [{ "message": { "content": "hi" } }] }),
        )
        .unwrap_err();
        assert!(error.to_string().contains("audio.data"), "{error}");
    }

    /// 起一个本机假 MiMo 服务,走真实 HTTP 链路:校验路径、鉴权头、请求体,
    /// 回文档格式的应答,确认 `synthesize_mimo` 拿到 wav 字节。
    #[tokio::test]
    async fn synthesize_mimo_end_to_end_against_mock_server() {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let wav = fake_wav();
        let reply_wav = wav.clone();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buffer = Vec::new();
            let mut chunk = [0u8; 4096];
            let (head, body_start) = loop {
                let read = stream.read(&mut chunk).unwrap();
                buffer.extend_from_slice(&chunk[..read]);
                if let Some(pos) = buffer.windows(4).position(|w| w == b"\r\n\r\n") {
                    break (String::from_utf8_lossy(&buffer[..pos]).to_string(), pos + 4);
                }
            };
            let length: usize = head
                .lines()
                .find_map(|line| {
                    line.to_ascii_lowercase()
                        .strip_prefix("content-length:")
                        .map(|v| v.trim().parse().unwrap())
                })
                .unwrap();
            while buffer.len() < body_start + length {
                let read = stream.read(&mut chunk).unwrap();
                buffer.extend_from_slice(&chunk[..read]);
            }
            let body: Value =
                serde_json::from_slice(&buffer[body_start..body_start + length]).unwrap();
            let encoded = base64::engine::general_purpose::STANDARD.encode(&reply_wav);
            let response = json!({
                "id": "chatcmpl-x",
                "choices": [{ "index": 0, "message": { "role": "assistant", "audio": { "data": encoded, "format": "wav", "voice": "mimo_default" } } }]
            })
            .to_string();
            let _ = stream.write_all(
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}",
                    response.len()
                )
                .as_bytes(),
            );
            (head, body)
        });
        let mut cfg = mimo_cfg();
        cfg.base_url = format!("http://127.0.0.1:{port}/v1");
        cfg.voice = "茉莉".to_string();
        let audio = synthesize_mimo(&cfg, "测试一句").await.unwrap();
        assert_eq!(audio, wav);
        let (head, body) = server.join().unwrap();
        assert!(head.starts_with("POST /v1/chat/completions "), "{head}");
        assert!(
            head.to_ascii_lowercase()
                .contains("authorization: bearer sk-test"),
            "{head}"
        );
        assert!(
            head.to_ascii_lowercase().contains("api-key: sk-test"),
            "{head}"
        );
        assert_eq!(body["model"], "mimo-v2.5-tts");
        assert_eq!(body["audio"]["voice"], "茉莉");
        assert_eq!(body["messages"][0]["content"], "测试一句");
    }

    /// 真实 MiMo 接口(要 `MIMO_API_KEY`):合成一句写到 `MIYU_MIMO_OUT`(缺省
    /// 当前目录 mimo-test.wav),人工听。
    #[tokio::test]
    #[ignore = "需要 MIMO_API_KEY"]
    async fn synthesize_mimo_real_api() {
        let Ok(key) = std::env::var("MIMO_API_KEY") else {
            eprintln!("跳过:MIMO_API_KEY 未设置");
            return;
        };
        let mut cfg = MimoTtsConfig {
            api_key: Some(key),
            ..Default::default()
        };
        if let Ok(voice) = std::env::var("MIYU_MIMO_VOICE") {
            cfg.voice = voice;
        }
        let started = std::time::Instant::now();
        let audio = synthesize_mimo(&cfg, "今天也是充满希望的一天")
            .await
            .unwrap();
        let out = std::env::var("MIYU_MIMO_OUT").unwrap_or_else(|_| "mimo-test.wav".to_string());
        std::fs::write(&out, &audio).unwrap();
        eprintln!(
            "MiMo 合成 {} 字节,耗时 {:?},写到 {out}",
            audio.len(),
            started.elapsed()
        );
        assert!(audio.starts_with(b"RIFF"), "not a wav");
    }

    #[test]
    fn synthesize_dispatches_on_provider() {
        use futures_util::FutureExt as _;
        let mut tts = VoiceTtsConfig::default();
        tts.active = Some("mimo".to_string());
        assert!(matches!(
            list_voices(&tts, "mimo").now_or_never().map(|r| r.unwrap().len()),
            Some(n) if n == MIMO_VOICES.len()
        ));
        tts.active = Some("nope".to_string());
        assert!(synthesize(&tts, "x").now_or_never().unwrap().is_err());
    }

    #[test]
    fn extracts_and_strips_speak_block() {
        let reply =
            "详细步骤见下。\n\n```sh\nls /tmp\n```\n<speak>我已经把文件列出来了,一共三个。</speak>";
        assert_eq!(
            extract_speak(reply).as_deref(),
            Some("我已经把文件列出来了,一共三个。")
        );
        assert_eq!(strip_speak(reply), "详细步骤见下。\n\n```sh\nls /tmp\n```");
        assert_eq!(extract_speak("没有块"), None);
    }

    #[test]
    fn sanitizes_markdown_for_speech() {
        let text = "# 结果\n\n- 已保存到 `/home/u/a.txt`\n- 参考 https://example.com/x\n\n```\ncode\n```\n| a | b |\n**完成**了";
        assert_eq!(
            sanitize_for_speech(text),
            "结果 已保存到 路径 参考 链接 完成了"
        );
    }

    #[test]
    fn spoken_text_prefers_speak_and_clips() {
        let tts = VoiceTtsConfig {
            max_chars: 20,
            ..Default::default()
        };
        let reply = format!(
            "{}<speak>第一句话。第二句话很长很长很长很长很长很长很长很长很长很长。</speak>",
            "x".repeat(50)
        );
        assert_eq!(spoken_text(&reply, &tts), "第一句话。");
        // 兜底词跟随 UI 语言(测试环境 LANG 不定)。
        assert!(matches!(spoken_text("", &tts).as_str(), "办好了" | "done"));
    }
}
