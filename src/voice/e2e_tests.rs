//! 语音管线的真机 e2e:用模型自带 test_wavs 走完整 VAD→KWS→STT 链路。
//! 需要模型已就位(~/.miyu/state/models 或 MIYU_VOICE_MODELS_DIR),
//! 缺模型时标记为跳过而不是失败,因此全部 #[ignore],显式
//! `cargo test --features voice -- --ignored` 运行。

use super::pipeline::Pipeline;
use super::stt::{LocalSenseVoice, SttEngine};
use super::{models, SttChoice, VoiceEvent, VoiceRuntimeConfig};
use std::path::PathBuf;

fn models_dir() -> Option<PathBuf> {
    let dir = std::env::var_os("MIYU_VOICE_MODELS_DIR")
        .map(PathBuf::from)
        .or_else(|| {
            directories::BaseDirs::new().map(|base| base.home_dir().join(".miyu/state/models"))
        })?;
    models::models_ready(&dir).then_some(dir)
}

fn config(dir: &std::path::Path, keyword: &str) -> VoiceRuntimeConfig {
    VoiceRuntimeConfig {
        models_dir: dir.to_path_buf(),
        wake_keyword: keyword.to_string(),
        wake_threshold: 0.25,
        wake_boost: 1.0,
        microphone: None,
        stt: SttChoice::Local {
            threads: 2,
            language: "auto".to_string(),
        },
        stt_unload_after: std::time::Duration::ZERO,
        follow_up: std::time::Duration::from_secs(15),
        min_utterance_chars: 2,
    }
}

fn feed_wav(pipeline: &mut Pipeline, path: &std::path::Path) -> Vec<VoiceEvent> {
    let wave = sherpa_onnx::Wave::read(path.to_str().unwrap()).expect("read wav");
    assert_eq!(wave.sample_rate(), 16_000, "test wav must be 16k");
    let mut events = Vec::new();
    for chunk in wave.samples().chunks(1600) {
        events.extend(pipeline.feed(chunk));
    }
    events.extend(pipeline.flush());
    events
}

fn kws_test_wavs(dir: &std::path::Path) -> Vec<PathBuf> {
    let mut wavs: Vec<PathBuf> = std::fs::read_dir(dir.join(models::KWS_DIR).join("test_wavs"))
        .expect("kws test_wavs")
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| path.extension().is_some_and(|ext| ext == "wav"))
        .collect();
    wavs.sort();
    wavs
}

#[test]
#[ignore = "需要本地语音模型"]
fn wake_keyword_fires_on_real_speech_and_not_on_others() {
    let Some(dir) = models_dir() else {
        eprintln!("跳过:语音模型未就位");
        return;
    };
    // 真实关键词:test_wavs 中至少一条音频包含「周望军」。
    let mut pipeline = Pipeline::new(&config(&dir, "周望军")).expect("pipeline");
    let mut fired = 0usize;
    for wav in kws_test_wavs(&dir) {
        let events = feed_wav(&mut pipeline, &wav);
        if events
            .iter()
            .any(|event| matches!(event, VoiceEvent::Wake | VoiceEvent::Command(_)))
        {
            fired += 1;
            eprintln!("命中: {} -> {:?}", wav.display(), events);
        }
    }
    assert!(fired >= 1, "真实关键词在测试音频上一次都没命中");

    // 无关关键词:同一批音频必须零命中(误唤醒检查)。
    let mut pipeline = Pipeline::new(&config(&dir, "肯德基星期四")).expect("pipeline");
    for wav in kws_test_wavs(&dir) {
        let events = feed_wav(&mut pipeline, &wav);
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, VoiceEvent::Wake | VoiceEvent::Command(_))),
            "无关关键词误唤醒: {} -> {events:?}",
            wav.display()
        );
    }
}

#[test]
#[ignore = "需要本地语音模型"]
fn sense_voice_transcribes_chinese() {
    let Some(dir) = models_dir() else {
        eprintln!("跳过:语音模型未就位");
        return;
    };
    let wav_path = dir.join(models::SENSE_VOICE_DIR).join("test_wavs/zh.wav");
    let wave = sherpa_onnx::Wave::read(wav_path.to_str().unwrap()).expect("read zh.wav");
    let mut stt = LocalSenseVoice::new(&dir, 2, "auto").expect("stt");
    let started = std::time::Instant::now();
    let text = stt
        .transcribe(wave.sample_rate() as u32, wave.samples())
        .expect("transcribe");
    let elapsed = started.elapsed();
    eprintln!(
        "zh.wav ({:.1}s 音频) 转写耗时 {:?}: {text}",
        wave.samples().len() as f32 / wave.sample_rate() as f32,
        elapsed
    );
    assert!(
        text.chars()
            .any(|ch| ('\u{4e00}'..='\u{9fff}').contains(&ch)),
        "转写结果没有中文: {text}"
    );
}

#[test]
#[ignore = "需要本地语音模型"]
fn follow_up_window_skips_wake_word_then_expires() {
    let Some(dir) = models_dir() else {
        eprintln!("跳过:语音模型未就位");
        return;
    };
    let zh_wav = dir.join(models::SENSE_VOICE_DIR).join("test_wavs/zh.wav");

    // 窗口开着:先用「周望军」在 5.wav 上出一条指令,紧接着喂不含
    // 唤醒词的 zh.wav,应免唤醒直接转写。
    let mut pipeline = Pipeline::new(&config(&dir, "周望军")).expect("pipeline");
    let mut woke = false;
    for wav in kws_test_wavs(&dir) {
        let events = feed_wav(&mut pipeline, &wav);
        if events
            .iter()
            .any(|event| matches!(event, VoiceEvent::Command(_)))
        {
            woke = true;
            break;
        }
    }
    assert!(woke, "唤醒指令没触发,无法测追问窗口");
    let events = feed_wav(&mut pipeline, &zh_wav);
    let follow_up = events.iter().find_map(|event| match event {
        VoiceEvent::Command(text) => Some(text.clone()),
        _ => None,
    });
    eprintln!("追问窗口内免唤醒转写: {follow_up:?}");
    assert!(follow_up.is_some_and(|text| !text.is_empty()));

    // 喂 16 秒静音把窗口耗尽,再喂 zh.wav 必须不产出指令。
    let mut ended = false;
    for _ in 0..160 {
        for event in pipeline.feed(&vec![0.0f32; 1600]) {
            if matches!(event, VoiceEvent::WindowClosed) {
                ended = true;
            }
        }
    }
    assert!(ended, "追问窗口静默 16 秒后没有关闭");
    let events = feed_wav(&mut pipeline, &zh_wav);
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, VoiceEvent::Command(_) | VoiceEvent::Wake)),
        "窗口关闭后无唤醒词仍触发了: {events:?}"
    );
}

#[test]
#[ignore = "需要本地语音模型"]
fn awaiting_phase_transcribes_next_segment_as_command() {
    let Some(dir) = models_dir() else {
        eprintln!("跳过:语音模型未就位");
        return;
    };
    // 用「女儿」当唤醒词(某条测试音频包含它),命中后紧接着喂
    // SenseVoice 的 zh.wav 当作指令段,应产出 Utterance。
    let mut pipeline = Pipeline::new(&config(&dir, "女儿")).expect("pipeline");
    let mut woke = false;
    for wav in kws_test_wavs(&dir) {
        let events = feed_wav(&mut pipeline, &wav);
        if events.iter().any(|event| matches!(event, VoiceEvent::Wake)) {
            let command_events = feed_wav(
                &mut pipeline,
                &dir.join(models::SENSE_VOICE_DIR).join("test_wavs/zh.wav"),
            );
            let utterance = command_events.iter().find_map(|event| match event {
                VoiceEvent::Command(text) => Some(text.clone()),
                _ => None,
            });
            eprintln!("唤醒后指令转写: {utterance:?}");
            assert!(utterance.is_some_and(|text| !text.is_empty()));
            return;
        }
        if events
            .iter()
            .any(|event| matches!(event, VoiceEvent::Command(_)))
        {
            // 关键词在句中,同段直接出了 Utterance,也算链路通了。
            woke = true;
        }
    }
    assert!(woke, "「女儿」在测试音频上未触发任何事件");
}
