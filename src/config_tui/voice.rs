//! 语音功能表单:唤醒词、麦克风、识别引擎、提示音。
//!
//! 保存时校验唤醒词能编码成 KWS 音节这件事在 `miyu-voice` 进程里做(本进程
//! 不链接语音栈),这里只做形式校验(非空、含中文)。

use crate::config_tui::*;

pub(in crate::config_tui) fn edit_voice(
    stdout: &mut io::Stdout,
    config: &mut AppConfig,
) -> Result<()> {
    let voice = &config.voice;
    let mut fields = vec![
        Field::boolean(t("Enable voice", "启用语音"), voice.enabled),
        Field::new(t("Wake keyword", "唤醒词"), voice.wake_keyword.clone()),
        Field::new(
            t("Microphone (empty = default)", "麦克风(空=系统默认)"),
            voice.microphone.clone().unwrap_or_default(),
        ),
        Field::new(
            t("Wake threshold (0.01-1)", "唤醒阈值(0.01-1,越低越灵敏)"),
            format!("{}", voice.wake_threshold),
        ),
        Field::new(
            t("Wake boost (0-10)", "唤醒词加分(0-10,越大越灵敏)"),
            format!("{}", voice.wake_boost),
        ),
        Field::new(t("STT engine", "识别引擎"), voice.stt_engine.clone())
            .choices(&["local", "cloud"]),
        Field::new(
            t("Local STT threads", "本地识别线程数"),
            voice.stt_threads.to_string(),
        ),
        Field::new(
            t(
                "Unload STT after idle seconds (0 = keep)",
                "识别模型闲置卸载秒数(0=常驻)",
            ),
            voice.stt_unload_seconds.to_string(),
        ),
        Field::new(
            t("Cloud STT provider id", "云端识别供应商 id"),
            voice.stt_cloud_provider.clone().unwrap_or_default(),
        ),
        Field::new(
            t("Cloud STT model", "云端识别模型"),
            voice.stt_cloud_model.clone(),
        ),
        Field::new(
            t(
                "Follow-up window seconds (0 = off)",
                "免唤醒追问窗口秒数(0=关)",
            ),
            voice.follow_up_seconds.to_string(),
        ),
        Field::new(
            t("Minimum utterance chars", "最少有效字数"),
            voice.min_utterance_chars.to_string(),
        ),
        Field::boolean(t("Sound cues", "提示音"), voice.sounds),
        Field::new(
            t("Sound volume (0-1)", "提示音音量(0-1)"),
            format!("{}", voice.sound_volume),
        ),
        Field::new(
            t("Reply digest chars in notification", "完成通知摘要字数"),
            voice.notify_reply_chars.to_string(),
        ),
        Field::boolean(
            t("REPL dictation submits directly", "REPL 听写直接发送"),
            voice.dictation_auto_submit,
        ),
        // 追加在末尾:下面的回读按下标取值。
        Field::new(
            t("Local STT language", "本地识别语言"),
            voice.stt_language.clone(),
        )
        .choices(&["auto", "zh", "en", "ja", "ko", "yue"]),
    ];
    debug_assert_eq!(
        fields.len(),
        17,
        "voice fields changed: update the read-back"
    );
    run_form_without_buttons(stdout, t(" VOICE ", " 语音功能 "), &mut fields)?;

    let enabled = parse_bool_field(&fields[0].value)?;
    let keyword = fields[1].value.trim().to_string();
    if enabled {
        if keyword.is_empty() {
            bail!(t("wake keyword is empty", "唤醒词为空"));
        }
        if !keyword
            .chars()
            .any(|ch| ('\u{4e00}'..='\u{9fff}').contains(&ch))
        {
            bail!(t(
                "wake keyword must contain Chinese characters (the KWS model is Mandarin)",
                "唤醒词必须包含汉字(唤醒模型是普通话模型)"
            ));
        }
    }
    let voice = &mut config.voice;
    voice.enabled = enabled;
    voice.wake_keyword = keyword;
    voice.microphone = Some(fields[2].value.trim().to_string()).filter(|s| !s.is_empty());
    voice.wake_threshold = fields[3].value.trim().parse::<f32>()?.clamp(0.01, 1.0);
    voice.wake_boost = fields[4].value.trim().parse::<f32>()?.clamp(0.0, 10.0);
    voice.stt_engine = match fields[5].value.trim() {
        "cloud" => "cloud".to_string(),
        _ => "local".to_string(),
    };
    voice.stt_threads = fields[6].value.trim().parse::<usize>()?.clamp(1, 8);
    voice.stt_unload_seconds = fields[7].value.trim().parse::<u64>()?;
    voice.stt_cloud_provider = Some(fields[8].value.trim().to_string()).filter(|s| !s.is_empty());
    voice.stt_cloud_model = fields[9].value.trim().to_string();
    voice.follow_up_seconds = fields[10].value.trim().parse::<u64>()?;
    voice.min_utterance_chars = fields[11].value.trim().parse::<usize>()?;
    voice.sounds = parse_bool_field(&fields[12].value)?;
    voice.sound_volume = fields[13].value.trim().parse::<f32>()?.clamp(0.0, 1.0);
    voice.notify_reply_chars = fields[14].value.trim().parse::<usize>()?;
    voice.dictation_auto_submit = parse_bool_field(&fields[15].value)?;
    voice.stt_language = match fields[16].value.trim() {
        lang @ ("auto" | "zh" | "en" | "ja" | "ko" | "yue") => lang.to_string(),
        _ => "zh".to_string(),
    };
    if voice.stt_engine == "cloud" && voice.stt_cloud_provider.is_none() {
        bail!(t(
            "cloud STT needs a provider id",
            "云端识别需要填供应商 id"
        ));
    }
    Ok(())
}
