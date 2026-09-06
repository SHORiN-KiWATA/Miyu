//! 识别管线状态机:能量门 → VAD 切句 → 唤醒词命中 → 转写。
//!
//! 输入是 16kHz 单声道 f32 帧(来源不限,麦克风或 wav 测具都行——
//! 这层与音频采集完全解耦,e2e 测试直接喂文件)。
//!
//! 取舍:
//! - KWS 不做逐帧流式,而是等 VAD 切出完整语音段后整段过一遍。唤醒和
//!   指令说在同一口气里("未有未有帮我查X")时反正要等说完才能转写,
//!   提前半句知道命中没有意义;
//! - 安静时 VAD 也不跑:帧能量低于自适应噪声底就直接跳过(能量门),
//!   只在 VAD 已在句中或刚结束时才连续喂,保证切句时序不受影响;
//! - STT 模型按闲置时间卸载,唤醒态常驻的只有 VAD + KWS。

use super::stt::SttEngine;
use super::{keywords, models, Control, SttChoice, VoiceEvent, VoiceRuntimeConfig};
use anyhow::{Context, Result};
use sherpa_onnx::{
    KeywordSpotter, KeywordSpotterConfig, OnlineTransducerModelConfig, SileroVadModelConfig,
    VadModelConfig, VoiceActivityDetector,
};
use std::collections::VecDeque;
use std::time::{Duration, Instant};

pub const SAMPLE_RATE: u32 = 16_000;
/// 语音段前补回的原始音频长度。VAD 判定"开始说话"总比真实起音晚一点,
/// 能量门又会把起音前的弱帧整个跳过,m/n/b 这类弱起音的唤醒词(密友密友)
/// 头一个音节常被切掉,裸模型能命中而管线不命中(09-05 实测)。这里从
/// 原始环形缓冲把段前这一截补回来再送 KWS/STT。
const PRE_ROLL: Duration = Duration::from_millis(500);
/// 原始环形缓冲容量:PRE_ROLL + VAD 最长语音段(20s)+ 裕量。
const RAW_RING: Duration = Duration::from_secs(23);
/// 唤醒后等待指令的静默超时。
const AWAIT_TIMEOUT: Duration = Duration::from_secs(8);
/// 听写窗口:静默 10 秒自动结束("问一句就走"的形态,不适用唤醒对话的
/// 长保持)。
const DICTATION_WINDOW: Duration = Duration::from_secs(10);
/// 起势判定阈值:窗口内持续 0.3s 人声即认为用户在说话。
const SPEECH_START: Duration = Duration::from_millis(300);
/// 短于这个时长的语音段不进 STT(咳嗽、敲击)。
const MIN_SEGMENT: Duration = Duration::from_millis(500);
/// 唤醒词最后一个 token 的起点之后再留这么久,当作唤醒词的尾巴一起切掉。
const KEYWORD_TAIL: Duration = Duration::from_millis(300);
/// VAD 的静默切句阈值;能量门在句尾之后至少再连续喂这么久,保证段被切出。
const VAD_MIN_SILENCE: f32 = 0.6;
const GATE_TRAIL: Duration = Duration::from_millis(1500);

fn samples(duration: Duration) -> usize {
    (duration.as_secs_f64() * SAMPLE_RATE as f64) as usize
}

enum Phase {
    /// 待唤醒:语音段只过 KWS。
    Idle,
    /// 已唤醒:下一个语音段整段转写为指令。
    Awaiting { silent: usize },
    /// 免唤醒窗口:刚出过指令(或听写开启),窗口长度随相位携带。
    Window {
        silent: usize,
        window: usize,
        dictation: bool,
    },
}

/// 能量门:自适应噪声底。安静帧不进 VAD。
struct EnergyGate {
    /// 噪声底(RMS)的慢速跟踪值。
    floor: f32,
    /// 最近一次"疑似有声"帧之后已连续放行的采样数(句尾拖尾)。
    trail: usize,
}

impl EnergyGate {
    /// 绝对下限:低于此 RMS 一律视为安静(约 -54 dBFS)。
    const ABS_FLOOR: f32 = 0.002;
    /// 高于噪声底多少倍算"可能有声"。
    const RATIO: f32 = 2.5;

    fn new() -> Self {
        Self {
            floor: 0.01,
            trail: usize::MAX,
        }
    }

    /// 返回这一帧是否需要进 VAD。
    fn admit(&mut self, frame: &[f32], vad_busy: bool) -> bool {
        let rms = (frame.iter().map(|s| s * s).sum::<f32>() / frame.len().max(1) as f32).sqrt();
        let threshold = (self.floor * Self::RATIO).max(Self::ABS_FLOOR);
        let loud = rms > threshold;
        if loud {
            // 有声时噪声底只允许极慢上浮,防止长时间说话把底抬高。
            self.floor += (rms - self.floor) * 0.002;
            self.trail = 0;
            return true;
        }
        // 安静帧:快速跟踪噪声底(仍夹在合理区间)。
        self.floor += (rms - self.floor) * 0.05;
        self.floor = self.floor.clamp(Self::ABS_FLOOR / Self::RATIO, 0.2);
        self.trail = self.trail.saturating_add(frame.len());
        // VAD 句中或刚结束:继续喂,保证 min_silence 计时正确、段能被切出。
        vad_busy || self.trail < samples(GATE_TRAIL)
    }
}

pub struct Pipeline {
    vad: VoiceActivityDetector,
    kws: KeywordSpotter,
    stt: Box<dyn SttEngine>,
    gate: EnergyGate,
    wake_keywords: Vec<String>,
    follow_up: usize,
    min_utterance_chars: usize,
    stt_unload_after: Duration,
    stt_last_used: Instant,
    phase: Phase,
    /// 窗口计时冻结:回合运行期间由宿主置位,静默不消耗窗口。
    hold: bool,
    /// 窗口内连续人声采样数,用于起势(SpeechStart)判定。
    speech_run: usize,
    speech_start_emitted: bool,
    /// 原始音频环形缓冲(含被能量门跳过的帧),补段前起音用。
    raw_ring: VecDeque<f32>,
    /// 环形缓冲第一个采样在原始流里的下标。
    raw_ring_base: usize,
    /// 已喂入的原始采样总数 / 已进 VAD 的采样总数。
    raw_total: usize,
    admitted_total: usize,
    /// VAD 流下标 → 原始流下标的分段映射:每次能量门跳帧后重新进 VAD
    /// 时记一条 (admitted_offset, raw_offset)。
    admitted_map: VecDeque<(usize, usize)>,
    last_frame_dropped: bool,
}

impl Pipeline {
    pub fn new(config: &VoiceRuntimeConfig) -> Result<Self> {
        let stt: Box<dyn SttEngine> =
            match &config.stt {
                SttChoice::Local { threads, language } => Box::new(
                    super::stt::LocalSenseVoice::new(&config.models_dir, *threads, language)?,
                ),
            };
        Self::with_stt(config, stt)
    }

    pub fn with_stt(config: &VoiceRuntimeConfig, stt: Box<dyn SttEngine>) -> Result<Self> {
        let vad_config = VadModelConfig {
            silero_vad: SileroVadModelConfig {
                model: Some(
                    models::vad_path(&config.models_dir)
                        .to_string_lossy()
                        .into_owned(),
                ),
                threshold: 0.5,
                min_silence_duration: VAD_MIN_SILENCE,
                min_speech_duration: 0.25,
                window_size: 512,
                max_speech_duration: 20.0,
            },
            sample_rate: SAMPLE_RATE as i32,
            num_threads: 1,
            ..Default::default()
        };
        let vad =
            VoiceActivityDetector::create(&vad_config, 60.0).context("加载 Silero VAD 模型失败")?;

        let kws_paths = models::kws_paths(&config.models_dir);
        // 编不出来的唤醒词只记日志跳过(用户配置里混进一个不支持的写法不该
        // 让整个语音前端起不来);全部编不出来才报错。
        let mut keyword_lines: Vec<String> = Vec::new();
        let mut first_error: Option<anyhow::Error> = None;
        for keyword in &config.wake_keywords {
            match keywords::encode_keyword(keyword, &kws_paths.tokens) {
                Ok(lines) => keyword_lines.extend(lines),
                Err(error) => {
                    tracing::warn!("唤醒词「{keyword}」无法编码,跳过: {error:#}");
                    first_error.get_or_insert(error);
                }
            }
        }
        if keyword_lines.is_empty() {
            return Err(first_error.unwrap_or_else(|| anyhow::anyhow!("至少要有一个唤醒词")));
        }
        let keyword_line = keyword_lines.join("\n");
        let mut kws_config = KeywordSpotterConfig::default();
        kws_config.model_config.transducer = OnlineTransducerModelConfig {
            encoder: Some(kws_paths.encoder.to_string_lossy().into_owned()),
            decoder: Some(kws_paths.decoder.to_string_lossy().into_owned()),
            joiner: Some(kws_paths.joiner.to_string_lossy().into_owned()),
        };
        kws_config.model_config.tokens = Some(kws_paths.tokens.to_string_lossy().into_owned());
        kws_config.model_config.num_threads = 1;
        kws_config.keywords_threshold = config.wake_threshold.clamp(0.01, 1.0);
        kws_config.keywords_score = config.wake_boost.clamp(0.0, 10.0);
        kws_config.keywords_buf = Some(keyword_line);
        let kws = KeywordSpotter::create(&kws_config).context("加载唤醒词模型失败")?;

        Ok(Self {
            vad,
            kws,
            stt,
            gate: EnergyGate::new(),
            wake_keywords: config.wake_keywords.clone(),
            follow_up: samples(config.follow_up),
            min_utterance_chars: config.min_utterance_chars,
            stt_unload_after: config.stt_unload_after,
            stt_last_used: Instant::now(),
            phase: Phase::Idle,
            hold: false,
            speech_run: 0,
            speech_start_emitted: false,
            raw_ring: VecDeque::with_capacity(samples(RAW_RING)),
            raw_ring_base: 0,
            raw_total: 0,
            admitted_total: 0,
            admitted_map: VecDeque::from([(0usize, 0usize)]),
            last_frame_dropped: false,
        })
    }

    /// 宿主控制命令。
    pub fn control(&mut self, command: Control) -> Vec<VoiceEvent> {
        match command {
            Control::Hold(hold) => {
                self.hold = hold;
                Vec::new()
            }
            Control::CloseWindow => self.close_window(),
            // 快捷键呼叫是开关:已经在听(等指令 / 追问窗口内)就关掉,否则进入等待指令。
            Control::Listen => match self.phase {
                Phase::Window {
                    dictation: true, ..
                } => Vec::new(),
                Phase::Window { .. } | Phase::Awaiting { .. } => {
                    self.phase = Phase::Idle;
                    vec![VoiceEvent::ListenOff]
                }
                Phase::Idle => {
                    self.phase = Phase::Awaiting { silent: 0 };
                    self.speech_run = 0;
                    self.speech_start_emitted = false;
                    let _ = self.stt.preload();
                    vec![VoiceEvent::Wake]
                }
            },
            // 音频来源(麦克风/外部)由 VoiceService 分流,管线不区分。
            Control::StartDictation { .. } => {
                self.phase = Phase::Window {
                    silent: 0,
                    window: samples(DICTATION_WINDOW),
                    dictation: true,
                };
                // 听写多半马上要转写,提前把模型拉起来。
                let _ = self.stt.preload();
                Vec::new()
            }
            Control::StopDictation => match self.phase {
                Phase::Window {
                    dictation: true, ..
                } => {
                    self.phase = Phase::Idle;
                    Vec::new()
                }
                _ => Vec::new(),
            },
            // 外部帧由 VoiceService 决定是否喂进来,走的是 feed(),这里不该收到。
            Control::Audio(_) | Control::Playback(_) => Vec::new(),
            Control::Transcribe {
                request_id,
                samples,
            } => {
                let text = match self.transcribe(&samples) {
                    Ok(text) => text,
                    Err(error) => {
                        tracing::warn!("外部音频转写失败: {error:#}");
                        String::new()
                    }
                };
                vec![VoiceEvent::Transcribed { request_id, text }]
            }
            Control::Stop => Vec::new(),
        }
    }

    /// 没有音频帧时的周期检查(STT 闲置卸载)。
    pub fn tick(&mut self) -> Vec<VoiceEvent> {
        self.maybe_unload_stt();
        Vec::new()
    }

    fn maybe_unload_stt(&mut self) {
        if self.stt_unload_after.is_zero() || !self.stt.is_loaded() {
            return;
        }
        if matches!(self.phase, Phase::Idle)
            && self.stt_last_used.elapsed() >= self.stt_unload_after
        {
            self.stt.release();
            tracing::info!("STT 模型闲置卸载");
        }
    }

    /// 喂一帧 16kHz 单声道音频,返回本帧产生的事件。
    pub fn feed(&mut self, frame: &[f32]) -> Vec<VoiceEvent> {
        let mut events = Vec::new();
        let vad_busy = self.vad.detected() || !self.vad.is_empty();
        let admitted = self.gate.admit(frame, vad_busy);
        self.record_raw(frame, admitted);
        if admitted {
            self.vad.accept_waveform(frame);
        }
        let speaking = admitted && self.vad.detected();

        // 唤醒/追问的静默计时:说话中不计时,宿主 hold(回合中)也不计时;
        // 静默累计超限回到待唤醒。被能量门跳过的帧照样计时。
        match &mut self.phase {
            Phase::Awaiting { silent } => {
                if speaking || self.hold {
                    *silent = 0;
                } else {
                    *silent += frame.len();
                    if *silent > samples(AWAIT_TIMEOUT) {
                        self.phase = Phase::Idle;
                        events.push(VoiceEvent::ListeningTimeout);
                    }
                }
            }
            Phase::Window { silent, window, .. } => {
                if speaking || self.hold {
                    *silent = 0;
                } else {
                    *silent += frame.len();
                    if *silent > *window {
                        self.phase = Phase::Idle;
                        events.push(VoiceEvent::WindowClosed);
                    }
                }
            }
            Phase::Idle => {}
        }

        // 起势判定:窗口内持续人声 0.3s 立即上报,宿主借此即时打断回合。
        if matches!(self.phase, Phase::Window { .. } | Phase::Awaiting { .. }) && speaking {
            self.speech_run += frame.len();
            if !self.speech_start_emitted && self.speech_run >= samples(SPEECH_START) {
                self.speech_start_emitted = true;
                events.push(VoiceEvent::SpeechStart);
            }
        } else {
            self.speech_run = 0;
            self.speech_start_emitted = false;
        }

        self.drain_segments(&mut events);
        self.maybe_unload_stt();
        events
    }

    /// 处理完整语音段前先冲掉 VAD 内部缓冲,给 wav 测具收尾用。
    /// 麦克风连续流不需要调它。
    pub fn flush(&mut self) -> Vec<VoiceEvent> {
        let mut events = Vec::new();
        self.vad.flush();
        self.drain_segments(&mut events);
        // flush 后内部窗口游标可能残留半窗数据,直接续喂会让 C 层
        // circular-buffer 报 Invalid n;喂流前复位一次。VAD 流下标随之归零,
        // 映射表同步重建。
        self.vad.reset();
        self.admitted_total = 0;
        self.admitted_map.clear();
        self.admitted_map.push_back((0, self.raw_total));
        self.last_frame_dropped = false;
        events
    }

    /// 原始帧进环形缓冲;能量门跳帧后第一帧重新进 VAD 时记一条映射。
    fn record_raw(&mut self, frame: &[f32], admitted: bool) {
        self.raw_ring.extend(frame.iter().copied());
        let capacity = samples(RAW_RING);
        if self.raw_ring.len() > capacity {
            let drop = self.raw_ring.len() - capacity;
            self.raw_ring.drain(..drop);
            self.raw_ring_base += drop;
        }
        if admitted {
            if self.last_frame_dropped {
                self.admitted_map
                    .push_back((self.admitted_total, self.raw_total));
            }
            self.admitted_total += frame.len();
        }
        self.last_frame_dropped = !admitted;
        self.raw_total += frame.len();
        // 映射只需覆盖环形缓冲还留着的范围。
        while self.admitted_map.len() > 1 && self.admitted_map[1].1 <= self.raw_ring_base {
            self.admitted_map.pop_front();
        }
    }

    /// VAD 段(流下标 start、长度 len)→ 带前置起音的原始音频,以及补回的
    /// 前置长度。环形缓冲里已没有对应数据时退回 VAD 段本身。
    fn padded_segment(&self, start: usize, segment: &[f32]) -> (Vec<f32>, usize) {
        let mapped = self
            .admitted_map
            .iter()
            .rev()
            .find(|(admitted, _)| *admitted <= start)
            .map(|(admitted, raw)| raw + (start - admitted));
        let Some(raw_start) = mapped else {
            return (segment.to_vec(), 0);
        };
        let raw_end = raw_start + segment.len();
        if raw_start < self.raw_ring_base || raw_end > self.raw_ring_base + self.raw_ring.len() {
            return (segment.to_vec(), 0);
        }
        let pre = samples(PRE_ROLL).min(raw_start - self.raw_ring_base);
        let from = raw_start - pre - self.raw_ring_base;
        let to = raw_end - self.raw_ring_base;
        (self.raw_ring.range(from..to).copied().collect(), pre)
    }

    fn drain_segments(&mut self, events: &mut Vec<VoiceEvent>) {
        while !self.vad.is_empty() {
            let (start, segment) = match self.vad.front() {
                Some(segment) => (segment.start().max(0) as usize, segment.samples().to_vec()),
                None => break,
            };
            self.vad.pop();
            let (padded, pre) = self.padded_segment(start, &segment);
            self.handle_segment(&padded, pre, events);
        }
    }

    /// `samples` 含 `pre` 个前置起音采样;长度判定按 VAD 段本身算。
    fn handle_segment(&mut self, samples: &[f32], pre: usize, events: &mut Vec<VoiceEvent>) {
        let seconds = (samples.len() - pre) as f32 / SAMPLE_RATE as f32;
        let segment_len = samples.len() - pre;
        match self.phase {
            Phase::Idle => {
                let Some(keyword_end) = self.keyword_hit(samples, events) else {
                    events.push(VoiceEvent::HeardSpeech(seconds));
                    return;
                };
                // 命中即刻上报 Wake(提示音/通知不等 STT 加载的两秒)。
                events.push(VoiceEvent::Wake);
                // 唤醒词按 KWS 的时间戳从音频上切掉,只把后半段送识别——
                // 唤醒词本身永远不进 STT,不再依赖识别文本和唤醒词字面对得上。
                let remainder = &samples[keyword_end.min(samples.len())..];
                if remainder.len() < super::pipeline::samples(MIN_SEGMENT) {
                    self.phase = Phase::Awaiting { silent: 0 };
                    return;
                }
                match self.transcribe_timed(remainder, events) {
                    Ok(text) => {
                        // 时间戳缺失时 remainder 是整段,退回字面剥离兜底。
                        let command = if keyword_end == 0 {
                            strip_any_wake_keyword(&text, &self.wake_keywords)
                        } else {
                            text.trim().to_string()
                        };
                        if self.effective_chars(&command) < self.min_utterance_chars.max(1) {
                            self.phase = Phase::Awaiting { silent: 0 };
                        } else {
                            self.emit_command(command, events);
                        }
                    }
                    Err(error) => events.push(VoiceEvent::Fatal(format!("识别失败: {error:#}"))),
                }
            }
            Phase::Awaiting { .. } => {
                if segment_len < super::pipeline::samples(MIN_SEGMENT) {
                    // 太短(咳嗽/敲击):继续等,不消耗唤醒。
                    return;
                }
                self.phase = Phase::Idle;
                match self.transcribe_timed(samples, events) {
                    Ok(text) if self.effective_chars(&text) < self.min_utterance_chars => {
                        events.push(VoiceEvent::ListeningTimeout);
                    }
                    Ok(text) => self.emit_command(text.trim().to_string(), events),
                    Err(error) => events.push(VoiceEvent::Fatal(format!("识别失败: {error:#}"))),
                }
            }
            // 会话/听写窗口:免唤醒词,整段直接转写。噪声不打扰也不关窗。
            Phase::Window { dictation, .. } => {
                if segment_len < super::pipeline::samples(MIN_SEGMENT) {
                    return;
                }
                match self.transcribe_timed(samples, events) {
                    Ok(text) if self.effective_chars(&text) < self.min_utterance_chars => {}
                    Ok(text) => {
                        let text = text.trim().to_string();
                        if dictation {
                            // 听写:每句续期窗口。
                            if let Phase::Window { silent, .. } = &mut self.phase {
                                *silent = 0;
                            }
                            events.push(VoiceEvent::Dictation(text));
                        } else {
                            self.emit_command(text, events);
                        }
                    }
                    Err(error) => events.push(VoiceEvent::Fatal(format!("识别失败: {error:#}"))),
                }
            }
        }
    }

    /// 立即关闭会话/听写窗口。在窗口内返回 WindowClosed 事件,不在窗口内为空。
    fn close_window(&mut self) -> Vec<VoiceEvent> {
        match self.phase {
            Phase::Window { .. } | Phase::Awaiting { .. } => {
                self.phase = Phase::Idle;
                vec![VoiceEvent::WindowClosed]
            }
            Phase::Idle => Vec::new(),
        }
    }

    /// 吐出识别指令,并进入/续期免唤醒追问窗口。
    fn emit_command(&mut self, text: String, events: &mut Vec<VoiceEvent>) {
        let window = match self.phase {
            Phase::Window { window, .. } => window,
            _ => self.follow_up,
        };
        self.phase = if window > 0 {
            Phase::Window {
                silent: 0,
                window,
                dictation: false,
            }
        } else {
            Phase::Idle
        };
        events.push(VoiceEvent::Command(text));
    }

    fn transcribe_timed(
        &mut self,
        samples: &[f32],
        events: &mut Vec<VoiceEvent>,
    ) -> Result<String> {
        let audio_secs = samples.len() as f32 / SAMPLE_RATE as f32;
        if !self.stt.is_loaded() {
            let start = Instant::now();
            self.stt.preload()?;
            events.push(VoiceEvent::Timing {
                stage: "stt_load",
                audio_secs,
                millis: start.elapsed().as_millis(),
            });
        }
        let start = Instant::now();
        let text = self.transcribe(samples)?;
        events.push(VoiceEvent::Timing {
            stage: "stt",
            audio_secs,
            millis: start.elapsed().as_millis(),
        });
        Ok(text)
    }

    fn transcribe(&mut self, samples: &[f32]) -> Result<String> {
        self.stt_last_used = Instant::now();
        let text = self.stt.transcribe(SAMPLE_RATE, samples)?;
        self.stt_last_used = Instant::now();
        Ok(text)
    }

    fn effective_chars(&self, text: &str) -> usize {
        text.chars().filter(|ch| ch.is_alphanumeric()).count()
    }

    /// 整段过一遍唤醒词模型。命中返回唤醒词在这段音频里的结束位置
    /// (采样下标;时间戳拿不到时为 0,表示"命中了但不知道在哪"),未命中
    /// 返回 None。
    fn keyword_hit(&self, samples: &[f32], events: &mut Vec<VoiceEvent>) -> Option<usize> {
        let start = Instant::now();
        let stream = self.kws.create_stream();
        stream.accept_waveform(SAMPLE_RATE as i32, samples);
        // 流式模型需要尾部静音把最后几帧推出解码窗口。
        stream.accept_waveform(SAMPLE_RATE as i32, &vec![0.0; (SAMPLE_RATE / 2) as usize]);
        stream.input_finished();
        let mut hit: Option<usize> = None;
        while self.kws.is_ready(&stream) {
            self.kws.decode(&stream);
            if let Some(result) = self.kws.get_result(&stream) {
                if !result.keyword.is_empty() {
                    // timestamps 是唤醒词各 token 的起点(秒,从流开头计);
                    // 取最后一个再加尾巴,就是唤醒词说完的位置。
                    let last_token = result
                        .timestamps
                        .iter()
                        .copied()
                        .fold(f32::NEG_INFINITY, f32::max);
                    let end = if last_token.is_finite() && last_token >= 0.0 {
                        (last_token as f64 * SAMPLE_RATE as f64) as usize
                            + super::pipeline::samples(KEYWORD_TAIL)
                    } else {
                        0
                    };
                    hit = Some(hit.map_or(end, |previous| previous.max(end)));
                    self.kws.reset(&stream);
                }
            }
        }
        events.push(VoiceEvent::Timing {
            stage: "kws",
            audio_secs: samples.len() as f32 / SAMPLE_RATE as f32,
            millis: start.elapsed().as_millis(),
        });
        hit
    }
}

/// 多唤醒词版:哪个能从开头剥掉就用哪个,都对不上原样返回整句。
pub fn strip_any_wake_keyword(text: &str, keywords: &[String]) -> String {
    let whole = text.trim().to_string();
    for keyword in keywords {
        let stripped = strip_wake_keyword(text, keyword);
        if stripped != whole {
            return stripped;
        }
    }
    whole
}

/// 从识别文本里剥掉开头的唤醒词(容忍标点/空白夹杂)。只在 KWS 没给出
/// 时间戳、唤醒词没能从音频上切掉时兜底使用。识别文本与唤醒词对不上时
/// 保守地原样返回整句,宁可多带前缀也不吞指令。
pub fn strip_wake_keyword(text: &str, keyword: &str) -> String {
    let keyword_chars: Vec<char> = keyword.chars().filter(|ch| !ch.is_whitespace()).collect();
    let mut matched = 0usize;
    let mut remainder_start = None;
    for (offset, ch) in text.char_indices() {
        if matched == keyword_chars.len() {
            remainder_start = Some(offset);
            break;
        }
        if ch == keyword_chars[matched] {
            matched += 1;
        } else if ch.is_whitespace() || ch.is_ascii_punctuation() || is_cjk_punctuation(ch) {
            // 标点夹在唤醒词里(如"未有,未有")照样往下匹配。
        } else {
            // 前缀对不上唤醒词,整句当指令。
            return text.trim().to_string();
        }
    }
    if matched < keyword_chars.len() {
        // 整句只有半个唤醒词,当作纯唤醒。
        return String::new();
    }
    let remainder = remainder_start.map_or("", |start| &text[start..]);
    remainder
        .trim_start_matches(|ch: char| {
            ch.is_whitespace() || ch.is_ascii_punctuation() || is_cjk_punctuation(ch)
        })
        .trim()
        .to_string()
}

fn is_cjk_punctuation(ch: char) -> bool {
    matches!(ch,
        '\u{3000}'..='\u{303F}' | '\u{FF00}'..='\u{FF0F}' | '\u{FF1A}'..='\u{FF20}'
        | '\u{FF3B}'..='\u{FF40}' | '\u{FF5B}'..='\u{FF65}')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_keyword_with_punctuation() {
        assert_eq!(
            strip_wake_keyword("未有未有,帮我查天气。", "未有未有"),
            "帮我查天气。"
        );
        assert_eq!(
            strip_wake_keyword("未有,未有 帮我查天气", "未有未有"),
            "帮我查天气"
        );
    }

    #[test]
    fn bare_keyword_becomes_empty() {
        assert_eq!(strip_wake_keyword("未有未有。", "未有未有"), "");
        assert_eq!(strip_wake_keyword("未有未", "未有未有"), "");
    }

    #[test]
    fn any_keyword_strips_first_match() {
        let keywords = vec!["未有未有".to_string(), "小未".to_string()];
        assert_eq!(strip_any_wake_keyword("小未,开灯", &keywords), "开灯");
        assert_eq!(strip_any_wake_keyword("未有未有。", &keywords), "");
        assert_eq!(strip_any_wake_keyword("开灯", &keywords), "开灯");
    }

    #[test]
    fn mismatched_prefix_keeps_whole_text() {
        assert_eq!(strip_wake_keyword("帮我查天气", "未有未有"), "帮我查天气");
    }

    #[test]
    fn energy_gate_skips_silence_but_trails_speech() {
        let mut gate = EnergyGate::new();
        let quiet = vec![0.0005f32; 512];
        let loud: Vec<f32> = (0..512).map(|i| (i as f32 * 0.3).sin() * 0.3).collect();
        // 冷启动:噪声底还高,先放行一小段拖尾,随后安静帧被拦。
        for _ in 0..200 {
            gate.admit(&quiet, false);
        }
        assert!(!gate.admit(&quiet, false));
        assert!(gate.admit(&loud, false));
        // 说话结束后 1.5s 内继续放行,之后再拦。
        let frames_in_trail = samples(GATE_TRAIL) / 512;
        for _ in 0..frames_in_trail {
            assert!(gate.admit(&quiet, false));
        }
        for _ in 0..40 {
            gate.admit(&quiet, false);
        }
        assert!(!gate.admit(&quiet, false));
        // VAD 忙时永远放行。
        assert!(gate.admit(&quiet, true));
    }
}
