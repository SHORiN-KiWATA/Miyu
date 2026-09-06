//! 语音功能菜单:总开关 → 播报供应商(MiniMax:连接/模型、音色浏览、参数、
//! 试听;小米 MiMo:连接/模型/音色、风格与指令、试听)→ 识别与唤醒设置。
//!
//! 唤醒词能否编码成 KWS 音节这件事在 `miyu-voice` 进程里做(本进程不链接
//! 语音栈),这里只做形式校验(非空、含中文)。麦克风列表同样问
//! `miyu-voice devices` 要,二进制不在就退化成手填。
//!
//! TUI 跑在 `miyu config` 的 tokio 运行时线程上,不能在这里 `block_on`
//! (运行时套运行时会 panic,整个 TUI 崩出):网络请求与 IPC 统统丢到独立
//! 线程里,那个线程自己起一个 current_thread 运行时。

use crate::config::{MiniMaxTtsConfig, VoiceTtsConfig, TTS_PROVIDERS};
use crate::config_tui::*;
use crate::web::voice_tts::{MIMO_MODELS, MIMO_VOICES};
use anyhow::Context as _;

/// MiMo 风格标签(文档 2026-09):情绪 / 语气 / 音色定位 / 角色 / 方言,多选。
const MIMO_STYLES: &[&str] = &[
    "温柔",
    "开心",
    "活泼",
    "俏皮",
    "慵懒",
    "平静",
    "严肃",
    "深情",
    "撒娇",
    "深沉",
    "干练",
    "高冷",
    "悲伤",
    "兴奋",
    "委屈",
    "无奈",
    "欣慰",
    "甜美",
    "清亮",
    "磁性",
    "醇厚",
    "空灵",
    "稚嫩",
    "沙哑",
    "夹子音",
    "御姐音",
    "正太音",
    "大叔音",
    "东北话",
    "四川话",
    "河南话",
    "粤语",
    "台湾腔",
    "唱歌",
];

/// MiniMax 可选模型(t2a_v2 文档,2026-09)。
const MINIMAX_TTS_MODELS: &[&str] = &[
    "speech-2.8-hd",
    "speech-2.8-turbo",
    "speech-2.6-hd",
    "speech-2.6-turbo",
    "speech-02-hd",
    "speech-02-turbo",
    "speech-01-hd",
    "speech-01-turbo",
];
const MINIMAX_BASE_URLS: &[&str] = &["https://api.minimaxi.com/v1", "https://api.minimax.io/v1"];
const TTS_EMOTIONS: &[&str] = &[
    "",
    "happy",
    "sad",
    "angry",
    "fearful",
    "disgusted",
    "surprised",
    "calm",
    "fluent",
    "whisper",
];
const TTS_LANGUAGE_BOOSTS: &[&str] = &[
    "auto",
    "Chinese",
    "Chinese,Yue",
    "English",
    "Japanese",
    "Korean",
    "Arabic",
    "Spanish",
    "French",
    "Portuguese",
    "German",
    "Russian",
];

// ---------------------------------------------------------------------------
// 基础设施
// ---------------------------------------------------------------------------

fn block_on_thread<T, F, Fut>(build: F) -> Result<T>
where
    T: Send + 'static,
    F: FnOnce() -> Fut + Send + 'static,
    Fut: std::future::Future<Output = Result<T>>,
{
    std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        runtime.block_on(build())
    })
    .join()
    .map_err(|_| anyhow::anyhow!("network thread panicked"))?
}

/// 问 `miyu-voice devices` 要输入源列表(源名, 描述);拿不到返回空。
fn list_microphones() -> Vec<(String, String)> {
    let Some(binary) = crate::web::voice_bridge::locate_binary() else {
        return Vec::new();
    };
    let Ok(output) = std::process::Command::new(binary)
        .arg("devices")
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
    else {
        return Vec::new();
    };
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(|line| {
            let (name, label) = line.split_once('\t').unwrap_or((line, line));
            (name.trim().to_string(), label.trim().to_string())
        })
        .collect()
}

/// MiniMax 音色列表 (value, label)。
fn fetch_minimax_voice_list(cfg: &MiniMaxTtsConfig) -> Result<Vec<(String, String)>> {
    let cfg = cfg.clone();
    let voices =
        block_on_thread(
            move || async move { crate::web::voice_tts::list_minimax_voices(&cfg).await },
        )?;
    Ok(voices
        .iter()
        .filter_map(|voice| {
            Some((
                voice.get("value")?.as_str()?.to_string(),
                voice.get("label")?.as_str()?.to_string(),
            ))
        })
        .collect())
}

/// 经 daemon 试听:把(可能尚未保存的)tts 配置整份带过去,按 `provider` 合成。
fn preview_tts(
    paths: &MiyuPaths,
    tts: &VoiceTtsConfig,
    provider: &str,
    text: Option<&str>,
) -> Result<()> {
    let text = text
        .map(str::to_string)
        .unwrap_or_else(|| tts.preview_text.trim().to_string());
    let text = if text.is_empty() {
        "今天也是充满希望的一天".to_string()
    } else {
        text
    };
    let mut tts = tts.clone();
    // 试听不要求先打开开关或把它设为当前供应商。
    tts.enabled = true;
    tts.active = Some(provider.to_string());
    let socket = paths.ipc_socket();
    block_on_thread(move || async move {
        let mut stream = crate::ipc::connect(&socket).await.context(t(
            "Miyu daemon is not running (preview needs it)",
            "Miyu daemon 未运行(试听要 daemon 在跑)",
        ))?;
        crate::ipc::send(
            &mut stream,
            &crate::ipc::Request::new(crate::ipc::Command::VoiceSpeak {
                text,
                tts: Some(tts),
            }),
        )
        .await?;
        match crate::ipc::receive::<crate::ipc::Frame>(&mut stream).await? {
            Some(crate::ipc::Frame::Ack) => Ok(()),
            Some(crate::ipc::Frame::Error { message, .. }) => bail!("{message}"),
            other => bail!("unexpected reply to VoiceSpeak: {other:?}"),
        }
    })
}

fn on_off(value: bool) -> &'static str {
    if value {
        t("on", "开")
    } else {
        t("off", "关")
    }
}

fn provider_display_name(provider: &str) -> &'static str {
    TTS_PROVIDERS
        .iter()
        .find(|(id, _)| *id == provider)
        .map(|(_, name)| *name)
        .unwrap_or("?")
}

/// 某供应商的一行摘要:模型 · 音色,没填 key 就说没填。
fn provider_summary(tts: &VoiceTtsConfig, provider: &str) -> String {
    if !tts.provider_has_key(provider) {
        return t("no api key yet", "未填 key").to_string();
    }
    match provider {
        "mimo" => {
            let cfg = &tts.mimo;
            let voice = if cfg.model.ends_with("voicedesign") {
                t("designed voice", "描述音色").to_string()
            } else if cfg.model.ends_with("voiceclone") {
                t("cloned voice", "克隆音色").to_string()
            } else {
                cfg.voice.clone()
            };
            format!("{} · {voice}", cfg.model)
        }
        _ => format!("{} · {}", tts.minimax.model, tts.minimax.voice_id),
    }
}

fn tts_label(tts: &VoiceTtsConfig) -> String {
    let provider = tts.provider().unwrap_or("minimax");
    format!(
        "{} · {}",
        provider_display_name(provider),
        provider_summary(tts, provider)
    )
}

/// 编辑完某供应商的连接后:当前生效的供应商没 key 而这个有,就顺手切过来
/// (填了 key 就该响,不用再找"设为当前")。
fn adopt_provider_if_current_unusable(tts: &mut VoiceTtsConfig, provider: &str) {
    let current = tts.provider().unwrap_or("minimax");
    if current != provider && !tts.provider_has_key(current) && tts.provider_has_key(provider) {
        tts.active = Some(provider.to_string());
    }
}

// ---------------------------------------------------------------------------
// 入口菜单
// ---------------------------------------------------------------------------

/// 语音功能入口菜单:语音唤醒开关 / 文本转语音开关 / 播报供应商 / 识别与唤醒设置。
pub(in crate::config_tui) fn edit_voice(
    stdout: &mut io::Stdout,
    paths: &MiyuPaths,
    config: &mut AppConfig,
) -> Result<()> {
    let mut selected = 0usize;
    loop {
        let options = [
            format!(
                "{}: {}",
                t("Wake word listening", "语音唤醒"),
                on_off(config.voice.enabled)
            ),
            format!(
                "{}: {}",
                t("Text to speech", "文本转语音"),
                on_off(config.voice.tts.enabled)
            ),
            format!(
                "{} ({}: {})",
                t("Configure TTS providers", "配置播报供应商"),
                t("Current", "当前"),
                tts_label(&config.voice.tts)
            ),
            t("Recognition and wake settings", "识别与唤醒设置").to_string(),
        ];
        draw_menu(
            stdout,
            t(" VOICE ", " 语音功能 "),
            &options,
            selected,
            t("[Enter]toggle/open [q]back", "[Enter]切换/进入 [q]返回"),
        )?;
        match read_key()? {
            KeyCode::Esc | KeyCode::Char('q') => return Ok(()),
            KeyCode::Up | KeyCode::Char('k') => selected = selected.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => selected = (selected + 1).min(options.len() - 1),
            KeyCode::Enter | KeyCode::Tab => {
                let outcome = match selected {
                    0 => {
                        config.voice.enabled = !config.voice.enabled;
                        Ok(())
                    }
                    1 => {
                        // 没填 key 也让开:缺东西静默(09-05 用户裁定),填上
                        // key 的那一刻播报自然生效,不在这里拦。
                        config.voice.tts.enabled = !config.voice.tts.enabled;
                        Ok(())
                    }
                    2 => edit_tts_providers(stdout, paths, config),
                    _ => edit_voice_form(stdout, config),
                };
                if let Err(error) = outcome {
                    show_tui_error(stdout, &error)?;
                }
            }
            _ => {}
        }
    }
}

// ---------------------------------------------------------------------------
// 播报供应商
// ---------------------------------------------------------------------------

/// 预置的播报供应商列表:`[*]` 是当前生效的那个;[Enter] 配置,[Tab] 设为当前。
fn edit_tts_providers(
    stdout: &mut io::Stdout,
    paths: &MiyuPaths,
    config: &mut AppConfig,
) -> Result<()> {
    let mut selected = 0usize;
    loop {
        let tts = &config.voice.tts;
        let current = tts.provider().unwrap_or("minimax");
        let options: Vec<String> = TTS_PROVIDERS
            .iter()
            .map(|(id, name)| {
                let marker = if *id == current { "[*] " } else { "[ ] " };
                format!("{marker}{name}  ({})", provider_summary(tts, id))
            })
            .collect();
        draw_menu(
            stdout,
            t(" TTS PROVIDERS ", " 播报供应商 "),
            &options,
            selected,
            t(
                "[Enter]configure [Tab]set current [q]back",
                "[Enter]配置 [Tab]设为当前 [q]返回",
            ),
        )?;
        let provider = TTS_PROVIDERS[selected.min(TTS_PROVIDERS.len() - 1)].0;
        match read_key()? {
            KeyCode::Esc | KeyCode::Char('q') => return Ok(()),
            KeyCode::Up | KeyCode::Char('k') => selected = selected.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => selected = (selected + 1).min(options.len() - 1),
            KeyCode::Tab | KeyCode::Char(' ') => {
                config.voice.tts.active = Some(provider.to_string());
            }
            KeyCode::Enter => {
                let outcome = match provider {
                    "mimo" => edit_mimo(stdout, paths, config),
                    _ => edit_minimax(stdout, paths, config),
                };
                if let Err(error) = outcome {
                    show_tui_error(stdout, &error)?;
                }
            }
            _ => {}
        }
    }
}

// ---------------------------------------------------------------------------
// 小米 MiMo
// ---------------------------------------------------------------------------

/// MiMo 配置菜单:连接与模型 / 风格与指令 / 试听。
fn edit_mimo(stdout: &mut io::Stdout, paths: &MiyuPaths, config: &mut AppConfig) -> Result<()> {
    let mut selected = 0usize;
    loop {
        let cfg = &config.voice.tts.mimo;
        let voice = if cfg.model.ends_with("voicedesign") {
            t("by description", "按描述").to_string()
        } else if cfg.model.ends_with("voiceclone") {
            cfg.sample_audio
                .clone()
                .filter(|path| !path.trim().is_empty())
                .unwrap_or_else(|| t("(no sample audio)", "(未填参考音频)").to_string())
        } else {
            cfg.voice.clone()
        };
        let options = [
            format!(
                "{} ({} · {voice})",
                t("Connection, model and voice", "连接、模型与音色"),
                cfg.model
            ),
            format!(
                "{} ({}{})",
                t("Style and instruction", "风格与指令"),
                if cfg.style.trim().is_empty() {
                    t("no style tag", "无标签").to_string()
                } else {
                    format!("({})", cfg.style.trim())
                },
                if cfg.instruction.trim().is_empty() {
                    String::new()
                } else {
                    format!(
                        " · {}",
                        crate::web::voice_bridge::clip(cfg.instruction.trim(), 24)
                    )
                }
            ),
            t("Preview", "试听").to_string(),
        ];
        draw_menu(
            stdout,
            " Xiaomi MiMo ",
            &options,
            selected,
            t("[Enter]open [q]back", "[Enter]进入 [q]返回"),
        )?;
        match read_key()? {
            KeyCode::Esc | KeyCode::Char('q') => return Ok(()),
            KeyCode::Up | KeyCode::Char('k') => selected = selected.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => selected = (selected + 1).min(options.len() - 1),
            KeyCode::Enter => {
                let outcome = match selected {
                    0 => edit_mimo_connection(stdout, config),
                    1 => edit_mimo_style(stdout, config),
                    _ => preview_tts(paths, &config.voice.tts, "mimo", None),
                };
                if let Err(error) = outcome {
                    show_tui_error(stdout, &error)?;
                }
            }
            _ => {}
        }
    }
}

fn edit_mimo_connection(stdout: &mut io::Stdout, config: &mut AppConfig) -> Result<()> {
    let cfg = config.voice.tts.mimo.clone();
    // 音色下拉显示「冰糖 · 中文女声」,写回的是 id。
    let voice_labels: Vec<String> = MIMO_VOICES
        .iter()
        .map(|(_, label)| label.to_string())
        .collect();
    let current_voice_label = MIMO_VOICES
        .iter()
        .find(|(id, _)| *id == cfg.voice.trim())
        .map(|(_, label)| label.to_string())
        .unwrap_or_else(|| cfg.voice.clone());
    let mut fields = vec![
        Field::new(
            t("API key", "API key"),
            cfg.api_key.clone().unwrap_or_default(),
        )
        .sensitive(),
        Field::new(t("Base URL", "接口地址"), cfg.base_url.clone()),
        Field::new(t("Model", "模型"), cfg.model.clone()).choices(MIMO_MODELS),
        Field::new(
            t(
                "Voice (mimo-v2.5-tts only)",
                "音色(只对 mimo-v2.5-tts 生效)",
            ),
            current_voice_label,
        )
        .choices_owned(voice_labels),
        Field::new(
            t(
                "Sample audio for voiceclone (wav/mp3 path)",
                "voiceclone 参考音频(wav/mp3 路径)",
            ),
            cfg.sample_audio.clone().unwrap_or_default(),
        ),
        Field::new(
            t("Max spoken chars", "播报字数上限"),
            config.voice.tts.max_chars.to_string(),
        ),
    ];
    run_form_without_buttons(
        stdout,
        t(" MiMo CONNECTION ", " MiMo 连接、模型与音色 "),
        &mut fields,
    )?;
    let tts = &mut config.voice.tts;
    tts.mimo.api_key = Some(fields[0].value.trim().to_string()).filter(|k| !k.is_empty());
    tts.mimo.base_url = fields[1].value.trim().to_string();
    tts.mimo.model = fields[2].value.trim().to_string();
    let picked = fields[3].value.trim().to_string();
    tts.mimo.voice = MIMO_VOICES
        .iter()
        .find(|(_, label)| *label == picked)
        .map(|(id, _)| id.to_string())
        .unwrap_or(picked);
    tts.mimo.sample_audio = Some(fields[4].value.trim().to_string()).filter(|p| !p.is_empty());
    tts.max_chars = fields[5].value.trim().parse::<usize>()?.max(20);
    adopt_provider_if_current_unusable(tts, "mimo");
    Ok(())
}

fn edit_mimo_style(stdout: &mut io::Stdout, config: &mut AppConfig) -> Result<()> {
    let cfg = config.voice.tts.mimo.clone();
    let mut fields = vec![
        // 多选菜单(Enter 进入,Tab 勾选),值是逗号分隔;发请求时转成 MiMo 要的空格。
        Field::new(
            t(
                "Style tags (Enter to pick, several allowed; empty = none)",
                "风格标签(回车进菜单勾选,可多个;空=不加)",
            ),
            cfg.style.clone(),
        )
        .multi_choices(MIMO_STYLES),
        // MiMo 没有数值语速,档位会拼成「语速稍快」进指令。
        Field::new(
            t(
                "Speed (MiMo has no numeric speed; this becomes an instruction)",
                "语速(MiMo 没有数值语速,档位会拼成「语速稍快」放进指令)",
            ),
            cfg.speed.clone(),
        )
        .choices(&["很慢", "稍慢", "稍快", "很快"])
        .empty_choice_label(t("normal", "常速")),
        Field::new(
            t(
                "Instruction (Enter to type: tone/role; voice description for voicedesign)",
                "指令(回车输入:语气/角色;voicedesign 模型下写音色描述,必填)",
            ),
            cfg.instruction.clone(),
        ),
        Field::new(
            t("Preview sentence", "试听语句"),
            config.voice.tts.preview_text.clone(),
        ),
    ];
    run_form_without_buttons(stdout, t(" MiMo STYLE ", " MiMo 风格与指令 "), &mut fields)?;
    let tts = &mut config.voice.tts;
    tts.mimo.style = fields[0].value.trim().to_string();
    tts.mimo.speed = fields[1].value.trim().to_string();
    tts.mimo.instruction = fields[2].value.trim().to_string();
    let preview = fields[3].value.trim().to_string();
    tts.preview_text = if preview.is_empty() {
        "今天也是充满希望的一天".to_string()
    } else {
        preview
    };
    Ok(())
}

// ---------------------------------------------------------------------------
// MiniMax
// ---------------------------------------------------------------------------

/// MiniMax 配置菜单。
fn edit_minimax(stdout: &mut io::Stdout, paths: &MiyuPaths, config: &mut AppConfig) -> Result<()> {
    let mut selected = 0usize;
    loop {
        let cfg = &config.voice.tts.minimax;
        let options = [
            format!(
                "{} ({} · {})",
                t("Connection and model", "连接与模型"),
                if cfg.base_url.contains("minimax.io") {
                    t("international", "国际站")
                } else {
                    t("China", "国内站")
                },
                cfg.model
            ),
            format!(
                "{} ({}: {})",
                t("Pick a voice", "选择音色"),
                t("Current", "当前"),
                cfg.voice_id
            ),
            format!(
                "{} ({} {} · {} {} · {} {}{})",
                t("Speech parameters", "播报参数"),
                t("speed", "语速"),
                cfg.speed,
                t("vol", "音量"),
                cfg.vol,
                t("pitch", "音调"),
                cfg.pitch,
                if cfg.emotion.is_empty() {
                    String::new()
                } else {
                    format!(" · {}", cfg.emotion)
                }
            ),
            t("Preview", "试听").to_string(),
        ];
        draw_menu(
            stdout,
            " MiniMax ",
            &options,
            selected,
            t("[Enter]open [q]back", "[Enter]进入 [q]返回"),
        )?;
        match read_key()? {
            KeyCode::Esc | KeyCode::Char('q') => return Ok(()),
            KeyCode::Up | KeyCode::Char('k') => selected = selected.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => selected = (selected + 1).min(options.len() - 1),
            KeyCode::Enter => {
                let outcome = match selected {
                    0 => edit_minimax_connection(stdout, config),
                    1 => browse_minimax_voices(stdout, paths, config),
                    2 => edit_minimax_params(stdout, config),
                    _ => preview_tts(paths, &config.voice.tts, "minimax", None),
                };
                if let Err(error) = outcome {
                    show_tui_error(stdout, &error)?;
                }
            }
            _ => {}
        }
    }
}

fn edit_minimax_connection(stdout: &mut io::Stdout, config: &mut AppConfig) -> Result<()> {
    let cfg = config.voice.tts.minimax.clone();
    let mut fields = vec![
        Field::new(
            t("API key", "API key"),
            cfg.api_key.clone().unwrap_or_default(),
        )
        .sensitive(),
        Field::new(t("Base URL", "接口地址"), cfg.base_url.clone()).choices(MINIMAX_BASE_URLS),
        Field::new(t("Model", "模型"), cfg.model.clone()).choices(MINIMAX_TTS_MODELS),
        Field::new(t("Language boost", "语种增强"), cfg.language_boost.clone())
            .choices(TTS_LANGUAGE_BOOSTS),
        Field::new(
            t("Max spoken chars", "播报字数上限"),
            config.voice.tts.max_chars.to_string(),
        ),
    ];
    run_form_without_buttons(
        stdout,
        t(" MiniMax CONNECTION ", " MiniMax 连接与模型 "),
        &mut fields,
    )?;
    let tts = &mut config.voice.tts;
    tts.minimax.api_key = Some(fields[0].value.trim().to_string()).filter(|k| !k.is_empty());
    tts.minimax.base_url = fields[1].value.trim().to_string();
    tts.minimax.model = fields[2].value.trim().to_string();
    tts.minimax.language_boost = fields[3].value.trim().to_string();
    tts.max_chars = fields[4].value.trim().parse::<usize>()?.max(20);
    adopt_provider_if_current_unusable(tts, "minimax");
    Ok(())
}

fn edit_minimax_params(stdout: &mut io::Stdout, config: &mut AppConfig) -> Result<()> {
    let cfg = config.voice.tts.minimax.clone();
    let mut emotion_field =
        Field::new(t("Emotion", "情绪"), cfg.emotion.clone()).choices(TTS_EMOTIONS);
    emotion_field.empty_choice_label = t("(model decides)", "(模型自定)");
    let mut fields = vec![
        Field::new(
            t("Speed (0.5-2, 1 = normal)", "语速(0.5~2,1 为常速)"),
            format!("{}", cfg.speed),
        ),
        Field::new(
            t("Volume (0.1-10, 1 = normal)", "音量(0.1~10,1 为原音量)"),
            format!("{}", cfg.vol),
        ),
        Field::new(
            t(
                "Pitch (-12..12 semitones, 0 = original)",
                "音调(-12~12 半音,0 原声;正数更高更细,负数更低更沉,±12 = 一个八度)",
            ),
            cfg.pitch.to_string(),
        ),
        emotion_field,
        Field::new(
            t("Preview sentence", "试听语句"),
            config.voice.tts.preview_text.clone(),
        ),
    ];
    run_form_without_buttons(stdout, t(" SPEECH PARAMETERS ", " 播报参数 "), &mut fields)?;
    let tts = &mut config.voice.tts;
    tts.minimax.speed = fields[0].value.trim().parse::<f32>()?.clamp(0.5, 2.0);
    tts.minimax.vol = fields[1].value.trim().parse::<f32>()?.clamp(0.1, 10.0);
    tts.minimax.pitch = fields[2].value.trim().parse::<i32>()?.clamp(-12, 12);
    tts.minimax.emotion = fields[3].value.trim().to_string();
    let preview = fields[4].value.trim().to_string();
    tts.preview_text = if preview.is_empty() {
        "今天也是充满希望的一天".to_string()
    } else {
        preview
    };
    Ok(())
}

// ---------------------------------------------------------------------------
// 音色浏览
// ---------------------------------------------------------------------------

/// 可筛选的标签(按 id/描述推出来的,MiniMax 接口本身不给结构化标签)。
const VOICE_TAGS: &[&str] = &[
    "全部",
    "中文",
    "粤语",
    "英文",
    "日语",
    "韩语",
    "其他语种",
    "女声",
    "男声",
    "克隆",
];

fn voice_tags(id: &str, label: &str) -> Vec<&'static str> {
    let mut tags = Vec::new();
    let lower = id.to_ascii_lowercase();
    let language = if id.starts_with("Chinese (Mandarin)")
        || lower.starts_with("female-")
        || lower.starts_with("male-")
        || lower.starts_with("chinese_")
    {
        "中文"
    } else if id.starts_with("Cantonese") {
        "粤语"
    } else if id.starts_with("English") {
        "英文"
    } else if id.starts_with("Japanese") {
        "日语"
    } else if id.starts_with("Korean") {
        "韩语"
    } else if [
        "Spanish",
        "Portuguese",
        "French",
        "German",
        "Russian",
        "Italian",
        "Arabic",
        "Indonesian",
        "Vietnamese",
        "Dutch",
        "Turkish",
        "Ukrainian",
        "Thai",
        "Polish",
        "Romanian",
        "Greek",
        "Czech",
        "Finnish",
        "Hindi",
    ]
    .iter()
    .any(|prefix| id.starts_with(prefix))
    {
        "其他语种"
    } else {
        // 没有语种前缀的通用音色(Sweet_Girl / lovely_girl / moss_audio_*)多半能说中文。
        "中文"
    };
    tags.push(language);
    let text = format!("{lower} {}", label.to_ascii_lowercase());
    let female = [
        "female", "girl", "lady", "woman", "women", "wife", "mother", "grandma", "女", "姐", "妹",
        "奶奶", "大婶", "闺蜜", "少女", "空姐",
    ]
    .iter()
    .any(|needle| text.contains(needle));
    let male = [
        "male",
        "boy",
        "gentleman",
        "guy",
        "husband",
        "father",
        "grandpa",
        "男",
        "哥",
        "爷",
        "弟",
        "竹马",
        "少年",
    ]
    .iter()
    .any(|needle| {
        text.contains(needle)
            && !(*needle == "male" && text.contains("female") && !text.contains(" male"))
    });
    if female {
        tags.push("女声");
    } else if male {
        tags.push("男声");
    }
    if label.starts_with("克隆:") || label.starts_with("生成:") {
        tags.push("克隆");
    }
    tags
}

/// 标签筛选语义:语种标签之间取"或",女声/男声、克隆各自成组,组间取"且"。
/// 例如勾「中文 + 日语 + 女声」= 中文或日语的女声;什么都不勾 = 全部。
fn voice_matches_tags(tags: &[&str], picked: &[&'static str]) -> bool {
    let languages: Vec<&str> = picked
        .iter()
        .copied()
        .filter(|tag| !matches!(*tag, "女声" | "男声" | "克隆"))
        .collect();
    let genders: Vec<&str> = picked
        .iter()
        .copied()
        .filter(|tag| matches!(*tag, "女声" | "男声"))
        .collect();
    let clone = picked.contains(&"克隆");
    (languages.is_empty() || languages.iter().any(|tag| tags.contains(tag)))
        && (genders.is_empty() || genders.iter().any(|tag| tags.contains(tag)))
        && (!clone || tags.contains(&"克隆"))
}

/// 音色浏览:列表来自 `get_voice`(名字 + 描述),`/` 搜索,`t` 按标签筛选
/// (多选,Tab 勾选),`p` 试听当前行,`Enter` 选用。
fn browse_minimax_voices(
    stdout: &mut io::Stdout,
    paths: &MiyuPaths,
    config: &mut AppConfig,
) -> Result<()> {
    let all = fetch_minimax_voice_list(&config.voice.tts.minimax)?;
    if all.is_empty() {
        bail!(t("no voices returned", "MiniMax 没有返回音色"));
    }
    let tagged: Vec<(String, String, Vec<&'static str>)> = all
        .into_iter()
        .map(|(id, label)| {
            let tags = voice_tags(&id, &label);
            (id, label, tags)
        })
        .collect();
    let mut search = String::new();
    // `/` 进入过滤输入:每敲一个字列表立刻变,Esc 清空退出,Enter 保留过滤退出。
    let mut searching = false;
    let mut picked: Vec<&'static str> = vec!["中文"];
    let mut selected = 0usize;
    loop {
        let needle = search.to_lowercase();
        let shown: Vec<&(String, String, Vec<&'static str>)> = tagged
            .iter()
            .filter(|(id, label, tags)| {
                voice_matches_tags(tags, &picked)
                    && (needle.is_empty()
                        || id.to_lowercase().contains(&needle)
                        || label.to_lowercase().contains(&needle))
            })
            .collect();
        // 行宽按终端显示宽度控制:名字/描述优先,id 放得下才带;否则右边被
        // 框截成「…」什么都看不见(09-06 用户反馈)。
        let row_width = terminal::size()
            .map(|(cols, _)| cols as usize)
            .unwrap_or(80)
            .saturating_sub(10)
            .max(40);
        let mut options: Vec<String> = shown
            .iter()
            .map(|(id, label, tags)| {
                let marker = if *id == config.voice.tts.minimax.voice_id {
                    "[*] "
                } else {
                    "[ ] "
                };
                let gender = tags
                    .iter()
                    .find(|tag| **tag == "女声" || **tag == "男声")
                    .map(|tag| format!("[{}]", &tag[..3]))
                    .unwrap_or_default();
                let head = format!("{marker}{gender}");
                let body = truncate(label, row_width.saturating_sub(display_width(&head)));
                let row = format!("{head}{body}");
                let suffix = format!("  ({id})");
                if label.starts_with(id.as_str())
                    || display_width(&row) + display_width(&suffix) > row_width
                {
                    row
                } else {
                    format!("{row}{suffix}")
                }
            })
            .collect();
        if options.is_empty() {
            options.push(t("(no match)", "(没有匹配的音色)").to_string());
        }
        selected = selected.min(options.len() - 1);
        let tag_summary = if picked.is_empty() {
            "全部".to_string()
        } else {
            picked.join("+")
        };
        let title = format!(
            " MiniMax {} · {tag_summary}{} ({}/{}) ",
            t("VOICES", "音色"),
            if searching {
                format!(" · /{search}_")
            } else if search.is_empty() {
                String::new()
            } else {
                format!(" · /{search}")
            },
            shown.len(),
            tagged.len()
        );
        draw_menu(
            stdout,
            &title,
            &options,
            selected,
            if searching {
                t(
                    "type to filter · [Esc]clear [Enter]done",
                    "输入即过滤 · [Esc]清空 [Enter]完成",
                )
            } else {
                t(
                    "[Enter]pick [p]preview [/]search [t]tags [q]back",
                    "[Enter]选用 [p]试听 [/]搜索 [t]标签(多选) [q]返回",
                )
            },
        )?;
        if searching {
            match read_key()? {
                KeyCode::Esc => {
                    search.clear();
                    searching = false;
                }
                KeyCode::Enter => searching = false,
                KeyCode::Backspace => {
                    search.pop();
                }
                KeyCode::Up => selected = selected.saturating_sub(1),
                KeyCode::Down => selected = (selected + 1).min(options.len() - 1),
                KeyCode::Char(ch) => {
                    search.push(ch);
                    selected = 0;
                }
                _ => {}
            }
            continue;
        }
        match read_key()? {
            KeyCode::Esc | KeyCode::Char('q') => return Ok(()),
            KeyCode::Up | KeyCode::Char('k') => selected = selected.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => selected = (selected + 1).min(options.len() - 1),
            KeyCode::Char('/') => {
                searching = true;
                selected = 0;
            }
            KeyCode::Char('t') => {
                // 多选:Tab / 空格勾选,Enter 或 q 应用。「全部」= 清空勾选。
                let tags: Vec<&'static str> = VOICE_TAGS
                    .iter()
                    .copied()
                    .filter(|tag| *tag != "全部")
                    .collect();
                let mut pick = 0usize;
                loop {
                    let options: Vec<String> = std::iter::once(format!(
                        "{} {}",
                        if picked.is_empty() { "[*]" } else { "[ ]" },
                        "全部"
                    ))
                    .chain(tags.iter().map(|tag| {
                        format!("{} {tag}", if picked.contains(tag) { "[*]" } else { "[ ]" })
                    }))
                    .collect();
                    draw_menu(
                        stdout,
                        t(" TAGS ", " 标签 "),
                        &options,
                        pick,
                        t(
                            "[Tab/Space]toggle [Enter/q]apply",
                            "[Tab/空格]勾选 [Enter/q]应用",
                        ),
                    )?;
                    match read_key()? {
                        KeyCode::Up | KeyCode::Char('k') => pick = pick.saturating_sub(1),
                        KeyCode::Down | KeyCode::Char('j') => {
                            pick = (pick + 1).min(options.len() - 1)
                        }
                        KeyCode::Tab | KeyCode::Char(' ') => {
                            if pick == 0 {
                                picked.clear();
                            } else {
                                let tag = tags[pick - 1];
                                if let Some(index) = picked.iter().position(|item| *item == tag) {
                                    picked.remove(index);
                                } else {
                                    picked.push(tag);
                                }
                            }
                        }
                        KeyCode::Enter | KeyCode::Esc | KeyCode::Char('q') => break,
                        _ => {}
                    }
                }
                selected = 0;
            }
            KeyCode::Char('p') if !shown.is_empty() => {
                let mut tts = config.voice.tts.clone();
                tts.minimax.voice_id = shown[selected].0.clone();
                if let Err(error) = preview_tts(paths, &tts, "minimax", None) {
                    show_tui_error(stdout, &error)?;
                }
            }
            KeyCode::Enter if !shown.is_empty() => {
                config.voice.tts.minimax.voice_id = shown[selected].0.clone();
                return Ok(());
            }
            _ => {}
        }
    }
}

// ---------------------------------------------------------------------------
// 识别与唤醒设置
// ---------------------------------------------------------------------------

fn edit_voice_form(stdout: &mut io::Stdout, config: &mut AppConfig) -> Result<()> {
    let voice = &config.voice;
    let default_mic = t("(system default)", "(系统默认)");
    let current_mic = voice
        .microphone
        .clone()
        .filter(|name| !name.trim().is_empty());
    // 列表里显示描述(「fifine Microphone」),写回配置的是源名。
    let mut devices = list_microphones();
    if let Some(name) = &current_mic {
        if !devices.iter().any(|(candidate, _)| candidate == name) {
            // 当前配置的设备现在没插着,也留在列表里,别一保存就丢。
            devices.push((name.clone(), name.clone()));
        }
    }
    let mic_field = if devices.is_empty() {
        Field::new(
            t(
                "Microphone (empty = default)",
                "麦克风(空=系统默认;装了 miyu-voice 才能列设备)",
            ),
            current_mic.clone().unwrap_or_default(),
        )
    } else {
        let mut choices = vec![default_mic.to_string()];
        choices.extend(devices.iter().map(|(_, label)| label.clone()));
        let current_label = current_mic
            .as_ref()
            .and_then(|name| devices.iter().find(|(candidate, _)| candidate == name))
            .map(|(_, label)| label.clone())
            .unwrap_or_else(|| default_mic.to_string());
        Field::new(t("Microphone", "麦克风"), current_label).choices_owned(choices)
    };
    let mut fields = vec![
        Field::string_list(
            t("Wake keywords (Enter to edit list)", "唤醒词(回车进列表)"),
            voice.wake_keywords.join(", "),
        ),
        mic_field,
        Field::new(
            t("Wake threshold (0.01-1)", "唤醒阈值(0.01-1,越低越灵敏)"),
            format!("{}", voice.wake_threshold),
        ),
        Field::new(
            t("Wake boost (0-10)", "唤醒词加分(0-10,越大越灵敏)"),
            format!("{}", voice.wake_boost),
        ),
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
        Field::new(
            t("Local STT language", "本地识别语言"),
            voice.stt_language.clone(),
        )
        .choices(&["auto", "zh", "en", "ja", "ko", "yue"]),
    ];
    debug_assert_eq!(
        fields.len(),
        13,
        "voice fields changed: update the read-back"
    );
    run_form_without_buttons(stdout, t(" RECOGNITION ", " 识别与唤醒设置 "), &mut fields)?;

    let keywords = crate::config::split_wake_keywords(&fields[0].value);
    if keywords.is_empty() {
        bail!(t("wake keyword is empty", "唤醒词为空"));
    }
    for keyword in &keywords {
        // 汉字 / 假名 / 拉丁字母 / 显式拼音(mi3 yu2)都行,真正能不能编码由
        // miyu-voice 按模型词表判定,编不出来的它会记日志跳过。
        let acceptable = keyword.chars().all(|ch| {
            ch.is_whitespace()
                || ch.is_ascii_alphanumeric()
                || ('\u{4e00}'..='\u{9fff}').contains(&ch)
                || ('\u{3041}'..='\u{30ff}').contains(&ch)
                || matches!(ch, '-' | '\'' | '・')
        });
        if !acceptable {
            bail!(t(
                "wake keyword may only contain Chinese characters, kana, or Latin letters",
                "唤醒词只能是汉字、假名或拉丁字母(如 未有未有 / みゆみゆ / miyumiyu)"
            ));
        }
    }
    let voice = &mut config.voice;
    voice.wake_keywords = keywords;
    let picked = fields[1].value.trim().to_string();
    voice.microphone = if picked.is_empty() || picked == default_mic {
        None
    } else {
        // 选的是描述就映射回源名;手填(没有列表时)原样保存。
        Some(
            devices
                .iter()
                .find(|(_, label)| *label == picked)
                .map(|(name, _)| name.clone())
                .unwrap_or(picked),
        )
    };
    voice.wake_threshold = fields[2].value.trim().parse::<f32>()?.clamp(0.01, 1.0);
    voice.wake_boost = fields[3].value.trim().parse::<f32>()?.clamp(0.0, 10.0);
    voice.stt_threads = fields[4].value.trim().parse::<usize>()?.clamp(1, 8);
    voice.stt_unload_seconds = fields[5].value.trim().parse::<u64>()?;
    voice.follow_up_seconds = fields[6].value.trim().parse::<u64>()?;
    voice.min_utterance_chars = fields[7].value.trim().parse::<usize>()?;
    voice.sounds = parse_bool_field(&fields[8].value)?;
    voice.sound_volume = fields[9].value.trim().parse::<f32>()?.clamp(0.0, 1.0);
    voice.notify_reply_chars = fields[10].value.trim().parse::<usize>()?;
    voice.dictation_auto_submit = parse_bool_field(&fields[11].value)?;
    voice.stt_language = match fields[12].value.trim() {
        lang @ ("auto" | "zh" | "en" | "ja" | "ko" | "yue") => lang.to_string(),
        _ => "zh".to_string(),
    };
    Ok(())
}
