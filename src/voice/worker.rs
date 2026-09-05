//! `miyu-voice` 进程主体:麦克风、唤醒、识别、提示音全部住在这里。
//!
//! 它不懂对话——识别出什么就以 IPC 信令告诉 daemon,daemon 侧的
//! `voice_bridge` 负责起回合、发通知、取消、听写中继。因此这里只有
//! 一条持久 `VoiceAttach` 连接,双向裸交换 Event 帧:
//!
//! | 方向 | kind | data |
//! |---|---|---|
//! | w→d | voice.ready | {device} |
//! | w→d | voice.wake / voice.speech_start / voice.timeout / voice.window_closed | {} |
//! | w→d | voice.command / voice.dictation | {text} |
//! | w→d | voice.transcribed | {request_id, text} |
//! | w→d | voice.error | {message} |
//! | d→w | voice.hold | {on} |
//! | d→w | voice.close_window | {} |
//! | d→w | voice.dictation | {on, source: "mic" \| "stream"} |
//! | d→w | voice.audio | {pcm16: base64 的 16kHz 单声道 PCM16 LE}(stream 听写期间) |
//! | d→w | voice.transcribe | {request_id, wav_path} |
//! | d→w | voice.cue | {name} |
//!
//! daemon 消失(attach 断开)即退出,由 daemon 侧负责重启。

use super::cues::{Cue, Player};
use super::{models, Control, SttChoice, VoiceEvent, VoiceRuntimeConfig, VoiceService};
use crate::config::{AppConfig, VoiceConfig};
use crate::i18n::text as t;
use crate::ipc;
use crate::paths::MiyuPaths;
use anyhow::{Context, Result};
use serde_json::{json, Value};
use std::sync::mpsc;
use std::time::Duration;

/// 控制循环消费的归并事件。
enum WorkerEvent {
    Voice(VoiceEvent),
    Signal(String, Value),
    /// 信令连接断开:daemon 没了,worker 退出。
    AttachLost,
}

/// 从配置拼出管线参数。云端 STT 的供应商按 id 在 providers 里找。
pub fn runtime_config(config: &AppConfig, paths: &MiyuPaths) -> Result<VoiceRuntimeConfig> {
    let voice = &config.voice;
    let stt = if voice.stt_engine.eq_ignore_ascii_case("cloud") {
        let provider_id = voice
            .stt_cloud_provider
            .as_deref()
            .filter(|id| !id.trim().is_empty())
            .context("stt_engine 为 cloud 但未指定 stt_cloud_provider")?;
        let provider = config
            .providers
            .iter()
            .find(|provider| provider.id == provider_id)
            .with_context(|| format!("找不到供应商「{provider_id}」"))?;
        SttChoice::Cloud {
            base_url: provider.base_url.clone(),
            api_key: provider.api_key.clone(),
            model: voice.stt_cloud_model.clone(),
        }
    } else {
        SttChoice::Local {
            threads: voice.stt_threads.max(1),
            language: voice.stt_language.clone(),
        }
    };
    Ok(VoiceRuntimeConfig {
        models_dir: models_dir(paths),
        wake_keyword: voice.wake_keyword.clone(),
        wake_threshold: voice.wake_threshold,
        wake_boost: voice.wake_boost,
        // WebUI 的文本框留空写回的是 "",与 null 同义。
        microphone: voice
            .microphone
            .clone()
            .filter(|name| !name.trim().is_empty()),
        stt,
        stt_unload_after: Duration::from_secs(voice.stt_unload_seconds),
        follow_up: Duration::from_secs(voice.follow_up_seconds),
        min_utterance_chars: voice.min_utterance_chars,
    })
}

pub fn models_dir(paths: &MiyuPaths) -> std::path::PathBuf {
    std::env::var_os("MIYU_VOICE_MODELS_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| paths.state_dir.join("models"))
}

/// 模型缺失时现场下载;下载前后各发一条桌面通知,让人知道在忙什么。
fn ensure_models_with_notice(dir: &std::path::Path, needs_local_stt: bool) -> Result<()> {
    if models::models_ready(dir) {
        return Ok(());
    }
    let _ = needs_local_stt;
    crate::notify::notify(
        t("Miyu voice", "未有语音"),
        &format!(
            "{} ({})",
            t("downloading speech models…", "正在下载语音模型…"),
            models::DOWNLOAD_SIZE_HINT
        ),
    );
    models::ensure_models(dir, &mut |stage| tracing::info!("{stage}"))?;
    crate::notify::notify(
        t("Miyu voice", "未有语音"),
        t("speech models ready", "语音模型已就位"),
    );
    Ok(())
}

/// worker 形态:连回 daemon,常驻监听。
pub fn run_worker() -> Result<()> {
    let paths = MiyuPaths::new()?;
    let config = AppConfig::load_or_default(&paths)?;
    let runtime = runtime_config(&config, &paths)?;
    ensure_models_with_notice(
        &runtime.models_dir,
        matches!(runtime.stt, SttChoice::Local { .. }),
    )?;
    let voice_config = config.voice.clone();

    let (event_tx, event_rx) = mpsc::channel::<WorkerEvent>();
    let (outbound_tx, outbound_rx) = tokio::sync::mpsc::unbounded_channel::<(String, Value)>();

    // 信令连接线程:读 daemon 信令,写识别事件。
    {
        let event_tx = event_tx.clone();
        let socket = paths.ipc_socket();
        std::thread::Builder::new()
            .name("miyu-voice-attach".into())
            .spawn(move || {
                if let Err(error) = attach_loop(&socket, event_tx.clone(), outbound_rx) {
                    tracing::error!("信令连接失败: {error:#}");
                }
                let _ = event_tx.send(WorkerEvent::AttachLost);
            })?;
    }

    let (service, voice_events) = VoiceService::start(runtime)?;
    // 语音事件桥接线程。
    {
        let event_tx = event_tx.clone();
        std::thread::Builder::new()
            .name("miyu-voice-events".into())
            .spawn(move || {
                for event in voice_events {
                    if event_tx.send(WorkerEvent::Voice(event)).is_err() {
                        return;
                    }
                }
            })?;
    }
    let player = Player::start()?;
    let cue = |name: Cue| {
        if voice_config.sounds {
            player.play(name, voice_config.sound_volume);
        }
    };
    let send = |kind: &str, data: Value| {
        let _ = outbound_tx.send((kind.to_string(), data));
    };

    tracing::info!("语音前端就绪:采集 {}", service.device_description);
    send(
        "voice.ready",
        json!({ "device": service.device_description }),
    );

    for event in event_rx {
        match event {
            WorkerEvent::Voice(event) => match event {
                VoiceEvent::Wake => {
                    cue(Cue::Wake);
                    send("voice.wake", json!({}));
                }
                VoiceEvent::Command(text) => {
                    cue(Cue::Heard);
                    send("voice.command", json!({ "text": text }));
                }
                VoiceEvent::Dictation(text) => {
                    send("voice.dictation", json!({ "text": text }));
                }
                VoiceEvent::SpeechStart => send("voice.speech_start", json!({})),
                VoiceEvent::ListeningTimeout => send("voice.timeout", json!({})),
                VoiceEvent::WindowClosed => send("voice.window_closed", json!({})),
                VoiceEvent::Transcribed { request_id, text } => send(
                    "voice.transcribed",
                    json!({ "request_id": request_id, "text": text }),
                ),
                VoiceEvent::HeardSpeech(seconds) => {
                    tracing::debug!("听到 {seconds:.1}s 语音,未命中唤醒词")
                }
                VoiceEvent::Timing {
                    stage,
                    audio_secs,
                    millis,
                } => tracing::debug!("[timing] {stage} audio={audio_secs:.2}s took={millis}ms"),
                VoiceEvent::Fatal(message) => {
                    send("voice.error", json!({ "message": message }));
                    // 给信令线程一点时间把错误送出去。
                    std::thread::sleep(Duration::from_millis(200));
                    anyhow::bail!("语音服务致命错误: {message}");
                }
            },
            WorkerEvent::Signal(kind, data) => match kind.as_str() {
                "voice.hold" => service.control(Control::Hold(
                    data.get("on").and_then(Value::as_bool).unwrap_or(false),
                )),
                "voice.close_window" => service.control(Control::CloseWindow),
                "voice.dictation" => {
                    if data.get("on").and_then(Value::as_bool).unwrap_or(false) {
                        let external = data
                            .get("source")
                            .and_then(Value::as_str)
                            .is_some_and(|source| source == "stream");
                        service.control(Control::StartDictation { external });
                    } else {
                        service.control(Control::StopDictation);
                    }
                }
                "voice.audio" => {
                    let encoded = data
                        .get("pcm16")
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    match decode_pcm16(encoded) {
                        Ok(samples) if !samples.is_empty() => {
                            service.control(Control::Audio(samples))
                        }
                        Ok(_) => {}
                        Err(error) => tracing::warn!("外部音频帧解码失败: {error:#}"),
                    }
                }
                "voice.transcribe" => {
                    let request_id = data
                        .get("request_id")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                    let path = data
                        .get("wav_path")
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    match load_wav_16k(path) {
                        Ok(samples) => service.control(Control::Transcribe {
                            request_id,
                            samples,
                        }),
                        Err(error) => {
                            tracing::warn!("读取待转写音频失败: {error:#}");
                            send(
                                "voice.transcribed",
                                json!({ "request_id": request_id, "text": "", "error": format!("{error:#}") }),
                            );
                        }
                    }
                }
                "voice.cue" => {
                    if let Some(name) = data
                        .get("name")
                        .and_then(Value::as_str)
                        .and_then(Cue::parse)
                    {
                        cue(name);
                    }
                }
                other => tracing::debug!("忽略未知信令 {other}"),
            },
            WorkerEvent::AttachLost => {
                tracing::info!("daemon 信令连接断开,worker 退出");
                break;
            }
        }
    }
    Ok(())
}

/// base64 的 PCM16 LE(16kHz 单声道)→ f32 采样。
fn decode_pcm16(encoded: &str) -> Result<Vec<f32>> {
    use base64::Engine;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(encoded.trim())
        .context("base64")?;
    Ok(bytes
        .chunks_exact(2)
        .map(|pair| i16::from_le_bytes([pair[0], pair[1]]) as f32 / 32768.0)
        .collect())
}

/// 读 WAV 并重采样到 16kHz 单声道。
fn load_wav_16k(path: &str) -> Result<Vec<f32>> {
    let bytes = std::fs::read(path).with_context(|| format!("读取 {path}"))?;
    let (rate, samples) = super::stt::decode_wav(&bytes)?;
    if rate == super::pipeline::SAMPLE_RATE {
        return Ok(samples);
    }
    let mut resampler = super::mic::LinearResampler::new(rate, super::pipeline::SAMPLE_RATE);
    Ok(resampler.process(&samples))
}

/// 信令连接:注册 VoiceAttach,读信令写事件,直到断开。
fn attach_loop(
    socket: &std::path::Path,
    event_tx: mpsc::Sender<WorkerEvent>,
    mut outbound: tokio::sync::mpsc::UnboundedReceiver<(String, Value)>,
) -> Result<()> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async move {
        let mut stream = ipc::connect(socket).await?;
        ipc::send(&mut stream, &ipc::Request::new(ipc::Command::VoiceAttach)).await?;
        let first = ipc::receive::<ipc::Frame>(&mut stream)
            .await?
            .context("daemon 关闭了信令连接")?;
        anyhow::ensure!(
            matches!(first, ipc::Frame::Ack),
            "VoiceAttach 被拒绝: {first:?}"
        );
        loop {
            tokio::select! {
                outgoing = outbound.recv() => {
                    let Some((kind, data)) = outgoing else { break };
                    ipc::send(&mut stream, &ipc::Frame::Event { id: 0, kind, data }).await?;
                }
                frame = ipc::receive::<ipc::Frame>(&mut stream) => {
                    let Some(frame) = frame? else { break };
                    if let ipc::Frame::Event { kind, data, .. } = frame {
                        if event_tx.send(WorkerEvent::Signal(kind, data)).is_err() {
                            break;
                        }
                    }
                }
            }
        }
        Ok(())
    })
}

/// 测试形态:不连 daemon,打开麦克风把事件逐行打印。排查"没反应"先跑它:
/// 一行"听到语音"都没有 = 音频没进来;有但不命中 = 唤醒词层。
pub fn run_test(keyword: Option<String>, device: Option<String>, timings: bool) -> Result<()> {
    let paths = MiyuPaths::new()?;
    let config = AppConfig::load_or_default(&paths)?;
    let mut runtime = runtime_config(&config, &paths)?;
    ensure_models_with_notice(&runtime.models_dir, true)?;
    if let Some(keyword) = keyword {
        runtime.wake_keyword = keyword;
    }
    if device.is_some() {
        runtime.microphone = device;
    }
    println!(
        "{}",
        if crate::i18n::is_zh() {
            format!(
                "语音测试:唤醒词「{}」,对着麦克风说话,Ctrl+C 退出",
                runtime.wake_keyword
            )
        } else {
            format!(
                "voice test: keyword \"{}\", speak into the mic, Ctrl+C to quit",
                runtime.wake_keyword
            )
        }
    );
    let devices = super::mic::list_input_devices();
    if !devices.is_empty() {
        println!("{}:", t("available input devices", "可用输入设备"));
        for name in &devices {
            println!("  - {name}");
        }
    }
    let player = Player::start()?;
    let voice_config: VoiceConfig = config.voice.clone();
    let (service, events) = VoiceService::start(runtime)?;
    println!(
        "{}: {}",
        t("capturing from", "正在采集"),
        service.device_description
    );
    for event in events {
        match event {
            VoiceEvent::SpeechStart => {
                println!("· {}", t("speech onset in window", "窗口内检测到开口"))
            }
            VoiceEvent::HeardSpeech(seconds) => println!(
                "· {}",
                if crate::i18n::is_zh() {
                    format!("听到 {seconds:.1}s 语音,未命中唤醒词")
                } else {
                    format!("heard {seconds:.1}s of speech, wake word not matched")
                }
            ),
            VoiceEvent::Wake => {
                if voice_config.sounds {
                    player.play(Cue::Wake, voice_config.sound_volume);
                }
                println!("✔ {}", t("wake word hit, listening…", "唤醒命中,等待指令…"))
            }
            VoiceEvent::Command(text) => {
                if voice_config.sounds {
                    player.play(Cue::Heard, voice_config.sound_volume);
                }
                println!("» {text}")
            }
            VoiceEvent::Dictation(text) => println!("» [{}] {text}", t("dictation", "听写")),
            VoiceEvent::ListeningTimeout => {
                println!("… {}", t("timed out, back to wake word", "超时,回到待唤醒"))
            }
            VoiceEvent::WindowClosed => println!(
                "… {}",
                t(
                    "window closed, wake word required again",
                    "窗口关闭,重新需要唤醒词"
                )
            ),
            VoiceEvent::Transcribed { text, .. } => println!("» {text}"),
            VoiceEvent::Timing {
                stage,
                audio_secs,
                millis,
            } => {
                if timings {
                    println!("  [timing] {stage} audio={audio_secs:.2}s took={millis}ms");
                }
            }
            VoiceEvent::Fatal(message) => {
                anyhow::bail!("{message}");
            }
        }
    }
    Ok(())
}

/// 试听提示音。
pub fn play_cue(name: &str, volume: f32) -> Result<()> {
    let cue = Cue::parse(name).with_context(|| format!("未知提示音「{name}」"))?;
    let player = Player::start()?;
    player.play(cue, volume);
    // Player 线程是异步排队的,等它播完。
    std::thread::sleep(Duration::from_millis(900));
    Ok(())
}
