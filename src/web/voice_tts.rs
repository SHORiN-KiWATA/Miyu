//! 回复播报:把回合的回复变成能读出来的文本,再合成语音。
//!
//! 三层:模型按 `<voice-protocol>`(见 `agent::prompt::VOICE_PROTOCOL`)在回复
//! 末尾给 `<speak>` 口语版 → 没给就把正文清洗一遍(去代码块/行内代码/链接/
//! 路径/Markdown 记号)兜底 → 截到 `max_chars`。合成走 MiniMax `t2a_v2`
//! (播报供应商自己的 base_url + key,返回 wav);播放在 miyu-voice 里。

use crate::config::{MiniMaxTtsConfig, VoiceTtsConfig};
use anyhow::{Context, Result};
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
