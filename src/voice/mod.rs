//! 语音前端:本地唤醒词 + VAD + 语音识别。
//!
//! 只在 `voice` feature 下编译,而且只被 `miyu-voice` 二进制使用——主程序
//! `miyu` 永远不链接 sherpa-onnx。模块对外两个面:`models::ensure_models`
//! (模型就位)与 [`VoiceService::start`](启动监听,事件经 channel 吐出,
//! 控制经 [`VoiceService`] 的方法下发)。调用方不感知 VAD/KWS/STT 实现,
//! STT 是 trait,本地 SenseVoice 与云端 OpenAI 兼容端点各一个实现。

pub mod cues;
pub mod keywords;
pub mod mic;
pub mod models;
pub mod pipeline;
pub mod speaker;
pub mod stt;
pub mod worker;

#[cfg(test)]
mod e2e_tests;

use anyhow::Result;
use std::path::PathBuf;
use std::sync::mpsc;
use std::time::Duration;

/// 语音管线对外吐出的事件。
#[derive(Debug, Clone)]
pub enum VoiceEvent {
    /// 待唤醒状态下听到了一段语音但没命中唤醒词(时长秒)。诊断用。
    HeardSpeech(f32),
    /// 会话窗口内检测到用户开始说话(持续 0.3s 人声)。宿主用它做
    /// 即时打断:立刻取消进行中的回合,不等整句转写完。
    SpeechStart,
    /// 唤醒词命中(唤醒词后无内容,等待用户继续说)。
    Wake,
    /// 唤醒/追问窗口内识别出的一句指令(已剥离唤醒词本身)。
    Command(String),
    /// 听写窗口内识别出的一句文本。
    Dictation(String),
    /// 唤醒后静默超时,回到待唤醒状态。
    ListeningTimeout,
    /// 会话/听写窗口关闭(静默超窗或被要求关闭),重新需要唤醒词。
    WindowClosed,
    /// 快捷键呼叫在"已在听"时再按一次:窗口已关,宿主应掐掉进行中的回合、
    /// 丢弃尚未播出的播报,并给用户一个"不听了"的反馈。
    ListenOff,
    /// 外部音频(浏览器录音)的转写结果。
    Transcribed { request_id: String, text: String },
    /// 阶段耗时(诊断/量尺用):stage = kws | stt | stt_load。
    Timing {
        stage: &'static str,
        audio_secs: f32,
        millis: u128,
    },
    /// 不可恢复错误(设备丢失、模型加载失败等),服务已停止。
    Fatal(String),
}

/// 宿主下发给管线线程的控制命令。
pub enum Control {
    /// 冻结/解冻窗口静默计时(回合运行期间置 true)。
    Hold(bool),
    /// 立即关闭会话/听写窗口。
    CloseWindow,
    /// 快捷键呼叫(开关):空闲时不用唤醒词直接进入等待指令状态,行为与
    /// 唤醒命中后无指令一致(吐 [`VoiceEvent::Wake`],等下一句当指令);
    /// 已在等指令或追问窗口内则关窗(吐 [`VoiceEvent::ListenOff`])。听写中忽略。
    Listen,
    /// 进入听写:免唤醒短窗,识别结果以 [`VoiceEvent::Dictation`] 吐出。
    /// `external` 为 true 时音频不来自麦克风,而是宿主经 [`Control::Audio`]
    /// 推入(浏览器麦克风走 WebSocket 到 daemon 再转到这里);期间本机
    /// 麦克风的帧一律丢弃。
    StartDictation {
        external: bool,
    },
    /// 退出听写(回到待唤醒)。外部音频听写会先冲掉 VAD 里的半句。
    StopDictation,
    /// 一段外部 16kHz 单声道音频,只在外部听写期间被消费。
    Audio(Vec<f32>),
    /// 播报中(true)丢弃麦克风帧:没有回声消除,不这样它会听见自己。
    /// 窗口静默计时随之停走,播完再继续。
    Playback(bool),
    /// 转写一段外部 16kHz 单声道音频。
    Transcribe {
        request_id: String,
        samples: Vec<f32>,
    },
    Stop,
}

/// STT 引擎选择(目前只有本地 SenseVoice)。
#[derive(Debug, Clone)]
pub enum SttChoice {
    /// 本地 SenseVoice(sherpa-onnx)。language: auto | zh | en | ja | ko | yue。
    Local { threads: usize, language: String },
}

/// 启动语音服务所需的全部参数,与 AppConfig 解耦以保持模块独立。
#[derive(Debug, Clone)]
pub struct VoiceRuntimeConfig {
    /// 模型根目录(state_dir/models)。
    pub models_dir: PathBuf,
    /// 中文唤醒词,可多个(任一命中即唤醒),如 ["未有未有", "小未"]。
    pub wake_keywords: Vec<String>,
    /// 唤醒词判定阈值(0~1,越低越灵敏,sherpa 默认 0.25)。
    pub wake_threshold: f32,
    /// 唤醒词路径加分(sherpa 默认 1.0,越大越灵敏)。
    pub wake_boost: f32,
    /// 麦克风设备名,None = 系统默认。
    pub microphone: Option<String>,
    pub stt: SttChoice,
    /// STT 模型闲置多久后卸载(0 = 常驻)。
    pub stt_unload_after: Duration,
    /// 免唤醒追问窗口:识别出指令后这段时间内说话不需要唤醒词(0 = 关)。
    pub follow_up: Duration,
    /// 识别文本少于这么多有效字(去标点)视为噪声丢弃。
    pub min_utterance_chars: usize,
}

/// 运行中的语音服务句柄,drop 即停止。
pub struct VoiceService {
    /// 实际打开的麦克风设备与格式,诊断输出用。
    pub device_description: String,
    control: mpsc::Sender<Control>,
    worker: Option<std::thread::JoinHandle<()>>,
}

impl VoiceService {
    pub fn control(&self, command: Control) {
        let _ = self.control.send(command);
    }

    /// 启动麦克风采集与识别管线。模型必须已经就位
    /// (先调用 [`models::ensure_models`])。
    pub fn start(config: VoiceRuntimeConfig) -> Result<(Self, mpsc::Receiver<VoiceEvent>)> {
        let (event_tx, event_rx) = mpsc::channel::<VoiceEvent>();
        let (control_tx, control_rx) = mpsc::channel::<Control>();

        // 麦克风 → 帧 channel。采集失败要在 start 阶段就报出去,
        // 而不是留给后台线程默默死掉。
        let (frame_rx, capture) = mic::start_capture(config.microphone.as_deref())?;
        let device_description = capture.description.clone();

        let worker = std::thread::Builder::new()
            .name("miyu-voice-pipeline".into())
            .spawn(move || {
                // capture 的生命周期挂在管线线程上:线程退出即停止采集。
                let _capture = capture;
                let mut pipeline = match pipeline::Pipeline::new(&config) {
                    Ok(pipeline) => pipeline,
                    Err(error) => {
                        let _ = event_tx.send(VoiceEvent::Fatal(format!("{error:#}")));
                        return;
                    }
                };
                // 外部音频听写中:管线吃宿主推来的帧,麦克风帧丢弃。
                let mut external = false;
                // 播报中:麦克风帧丢弃(半双工)。
                let mut playback = false;
                loop {
                    // 控制命令优先(非阻塞排空)。
                    loop {
                        let events = match control_rx.try_recv() {
                            Ok(Control::Stop) => return,
                            Ok(Control::StartDictation { external: source }) => {
                                external = source;
                                pipeline.control(Control::StartDictation { external: source })
                            }
                            Ok(Control::StopDictation) => {
                                let mut events = Vec::new();
                                if external {
                                    // 浏览器那头停了,VAD 里可能还压着半句。
                                    events.extend(pipeline.flush());
                                    external = false;
                                }
                                events.extend(pipeline.control(Control::StopDictation));
                                events
                            }
                            Ok(Control::Audio(samples)) => {
                                if external {
                                    pipeline.feed(&samples)
                                } else {
                                    Vec::new()
                                }
                            }
                            Ok(Control::Playback(on)) => {
                                playback = on;
                                Vec::new()
                            }
                            Ok(command) => pipeline.control(command),
                            Err(mpsc::TryRecvError::Empty) => break,
                            Err(mpsc::TryRecvError::Disconnected) => return,
                        };
                        for event in events {
                            if event_tx.send(event).is_err() {
                                return;
                            }
                        }
                    }
                    // 100ms 超时轮询控制通道,平时阻塞在 recv 上不空转。
                    match frame_rx.recv_timeout(Duration::from_millis(100)) {
                        Ok(_) if external || playback => {
                            for event in pipeline.tick() {
                                if event_tx.send(event).is_err() {
                                    return;
                                }
                            }
                        }
                        Ok(frame) => {
                            for event in pipeline.feed(&frame) {
                                if event_tx.send(event).is_err() {
                                    return;
                                }
                            }
                        }
                        Err(mpsc::RecvTimeoutError::Timeout) => {
                            for event in pipeline.tick() {
                                if event_tx.send(event).is_err() {
                                    return;
                                }
                            }
                        }
                        Err(mpsc::RecvTimeoutError::Disconnected) => {
                            let _ =
                                event_tx.send(VoiceEvent::Fatal("麦克风采集流断开".to_string()));
                            return;
                        }
                    }
                }
            })?;

        Ok((
            Self {
                device_description,
                control: control_tx,
                worker: Some(worker),
            },
            event_rx,
        ))
    }
}

impl Drop for VoiceService {
    fn drop(&mut self) {
        let _ = self.control.send(Control::Stop);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}
