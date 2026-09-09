mod defaults;
mod io;
mod paths;
mod persona_paths;
mod platform;
mod platform_ops;
mod platform_plugins;
mod pool_ref;
mod provider;
pub(crate) use provider::append_resolved_api_keys;
mod provider_ops;
pub(crate) use provider_ops::detect_provider_renames;
mod tool_plugins;
pub(crate) use defaults::*;
pub(crate) use paths::*;
pub(crate) use platform::*;
pub(crate) use platform_plugins::*;
pub(crate) use pool_ref::*;
pub(crate) use provider::*;
pub(crate) use tool_plugins::*;

use crate::default_models::{
    OPENCODE_DEFAULT_CHAT_MODEL, OPENCODE_DEFAULT_VISION_MODEL, OPENCODE_PROVIDER_ID,
    OPENCODE_ZEN_BASE_URL,
};
use crate::paths::MiyuPaths;
use crate::prompts::default_system_prompt;
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Deserializer, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};

pub const MAX_COMMAND_OUTPUT_LINES: usize = 1_000;

/// Dev 模式提示词文件名(config 目录下,可编辑;清空=回退内置默认)。
pub const DEV_PROMPT_FILE: &str = "dev-prompt.md";
/// Dev 模式内置默认提示词。dsh 极简变体同款措辞——贴近编码 RL 训练分布
/// 是它强的主因(08-15 与用户讨论定稿,修正了社区传言的拼写错误)。
pub const DEFAULT_DEV_SYSTEM_PROMPT: &str = "You are a helpful software engineer assistant.";
/// Replay redraws whole turns, so a large value floods the screen on startup.
pub const MAX_REPL_REPLAY_TURNS: usize = 20;
pub const CURRENT_CONFIG_VERSION: u32 = 2;
const LEGACY_DEFAULT_TEMPERATURE: f32 = 0.7;
/// 上下文窗口那个数是哪来的。
///
/// `Known` = 用户在配置里写死的，或 models.dev / 供应商 `/models` 报的。
/// `Assumed` = 谁都没给，用的是 `context.default_context_window` 那个通用常数
/// ——它跟具体模型没有任何关系，只是让溢出判定有个数可用。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ContextWindowSource {
    Known,
    Assumed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    #[serde(default)]
    pub config_version: u32,
    pub active_provider: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_provider_models: Option<Vec<ActiveProviderModelConfig>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_multimodal_provider_models: Option<Vec<ActiveProviderModelConfig>>,
    pub providers: Vec<ProviderConfig>,
    #[serde(default, skip_serializing_if = "EmbeddingConfig::is_default")]
    pub embedding: EmbeddingConfig,
    #[serde(default)]
    pub context: ContextConfig,
    #[serde(default)]
    pub tools: ToolsConfig,
    #[serde(default, skip_serializing_if = "CacheConfig::is_default")]
    pub cache: CacheConfig,
    #[serde(default)]
    pub mcp: McpConfig,
    #[serde(default)]
    pub skills: SkillsConfig,
    #[serde(default)]
    pub display: DisplayConfig,
    #[serde(default)]
    pub notifications: NotificationsConfig,
    #[serde(default)]
    pub prompt: PromptConfig,
    #[serde(default)]
    pub plugins: PluginsConfig,
    #[serde(default, skip_serializing)]
    pub memory: MemoryConfig,
    #[serde(default)]
    pub system_prompt_file: Option<String>,
    /// 裸 `miyu` 的默认模式:"normal" | "dev";空(默认)=打印带模式说明的
    /// 帮助,逼一次显式选择。`miyu normal` / `miyu dev` 子命令始终可用。
    #[serde(default)]
    pub default_mode: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_prompt: Option<String>,
    /// Tiered model pools. The pre-09-05 key `subagent_tiers` stays readable.
    #[serde(
        default,
        alias = "subagent_tiers",
        skip_serializing_if = "ModelTiersConfig::is_empty"
    )]
    pub model_tiers: ModelTiersConfig,
    #[serde(default, skip_serializing_if = "PlatformsConfig::is_empty")]
    pub platforms: PlatformsConfig,
    /// 语音前端(`miyu-voice` 进程):唤醒词、本地识别、听写、提示音。
    #[serde(default)]
    pub voice: VoiceConfig,
}

/// 语音功能。整套只在 `voice.enabled` 时由 daemon 拉起独立的 `miyu-voice`
/// 进程,关着时 daemon 零占用;主程序不含任何识别模型代码。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VoiceConfig {
    /// 语音唤醒开关:麦克风常开、唤醒词、听写。与 `tts.enabled` 独立。
    #[serde(default)]
    pub enabled: bool,
    /// 中文唤醒词,任意汉字,内部转拼音送 KWS 模型,不用重训。
    /// 唤醒词,可多个,任一命中即唤醒。配置里写数组或逗号分隔的字符串都行,
    /// 旧键名 `wake_keyword` 照样读。
    #[serde(
        default = "default_wake_keywords",
        alias = "wake_keyword",
        deserialize_with = "deserialize_wake_keywords"
    )]
    pub wake_keywords: Vec<String>,
    /// 唤醒判定阈值(0~1,越低越灵敏;sherpa 默认 0.25)。
    #[serde(default = "default_wake_threshold")]
    pub wake_threshold: f32,
    /// 唤醒词路径加分(越大越灵敏;sherpa 默认 1.0)。
    #[serde(default = "default_wake_boost")]
    pub wake_boost: f32,
    /// 麦克风设备名(`miyu-voice devices` 可列),null = 系统默认。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub microphone: Option<String>,
    /// "local"(SenseVoice,本地)| "cloud"(OpenAI 兼容 transcriptions)。
    /// 本地识别线程数。
    #[serde(default = "default_stt_threads")]
    pub stt_threads: usize,
    /// 本地识别语言:auto | zh | en | ja | ko | yue。固定 zh 可避免噪声被
    /// 认成日文碎片。
    #[serde(default = "default_stt_language")]
    pub stt_language: String,
    /// 本地识别模型闲置多少秒后卸载(0 = 常驻)。
    #[serde(default = "default_stt_unload_seconds")]
    pub stt_unload_seconds: u64,
    /// 免唤醒追问窗口(秒):从她回复完(播报播完)起算,这段时间内说话不用
    /// 再喊唤醒词;每次回复都重新起算。0 = 每句都要唤醒词。
    #[serde(default = "default_follow_up_seconds")]
    pub follow_up_seconds: u64,
    /// 识别文本少于这么多有效字视为噪声丢弃。
    #[serde(default = "default_min_utterance_chars")]
    pub min_utterance_chars: usize,
    /// 提示音总开关。
    #[serde(default = "default_true")]
    pub sounds: bool,
    #[serde(default = "default_sound_volume")]
    pub sound_volume: f32,
    /// 回合完成通知里带的回复摘要字数。
    #[serde(default = "default_notify_reply_chars")]
    pub notify_reply_chars: usize,
    /// REPL 听写:识别一句就直接提交(true)还是先填进编辑框等回车(false)。
    #[serde(default)]
    pub dictation_auto_submit: bool,
    /// 回复播报(语音合成)。
    #[serde(default)]
    pub tts: VoiceTtsConfig,
}

/// 回复播报(语音合成)。供应商各自独立配置(不共用 providers 里的 LLM
/// 供应商,免得混),`active` 指向激活的那一个;空 = 不播报,只弹通知和提示音。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct VoiceTtsConfig {
    /// 文本转语音开关:开了才播报回复、才注册 `speak` 工具。与语音唤醒独立,
    /// 任一开启都会拉起 miyu-voice(唤醒关闭时它只管播放,不开麦克风)。
    #[serde(default)]
    pub enabled: bool,
    /// 播报供应商:`minimax` | `mimo`(小米 MiMo);None / 空 = 默认 MiniMax。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active: Option<String>,
    /// 播报文本上限(字),超出截断。
    #[serde(default = "default_tts_max_chars")]
    pub max_chars: usize,
    /// 试听用的句子。
    #[serde(default = "default_tts_preview_text")]
    pub preview_text: String,
    #[serde(default)]
    pub minimax: MiniMaxTtsConfig,
    #[serde(default)]
    pub mimo: MimoTtsConfig,
}

/// 小米 MiMo 语音合成(`mimo-v2.5-tts` 系列,OpenAI 兼容的 `chat/completions`:
/// 待合成文本放 assistant 消息,风格描述放 user 消息,音频以 base64 回来)。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MimoTtsConfig {
    /// API key(platform.xiaomimimo.com),支持 `$env:VAR` 引用。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    /// `https://api.xiaomimimo.com/v1`。
    #[serde(default = "default_mimo_base_url")]
    pub base_url: String,
    /// `mimo-v2.5-tts`(预置音色)| `mimo-v2.5-tts-voicedesign`(按描述造音色)|
    /// `mimo-v2.5-tts-voiceclone`(按样本克隆)。
    #[serde(default = "default_mimo_model")]
    pub model: String,
    /// 预置音色:mimo_default / 冰糖 / 茉莉 / 苏打 / 白桦 / Mia / Chloe / Milo / Dean。
    /// voicedesign / voiceclone 模型不用它。
    #[serde(default = "default_mimo_voice")]
    pub voice: String,
    /// 风格标签(写在文本开头的 `(温柔)` 那种):空 = 不加。多个用空格隔开,
    /// 如 `温柔 慵懒`。
    #[serde(default)]
    pub style: String,
    /// 提示词(user 消息):语速/语气/角色用自然语言写,如「语速稍快,像在跟朋友
    /// 聊天」(MiMo 没有数值语速,只认这种说法);voicedesign 模型下是音色描述
    /// (必填)。空 = 不发 user 消息。旧键名 `instruction` 照样读。
    #[serde(default, alias = "instruction")]
    pub prompt: String,
    /// voiceclone 模型的参考音频路径(wav / mp3,base64 后 ≤ 10MB)。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sample_audio: Option<String>,
}

fn default_mimo_base_url() -> String {
    "https://api.xiaomimimo.com/v1".to_string()
}
fn default_mimo_model() -> String {
    "mimo-v2.5-tts".to_string()
}
fn default_mimo_voice() -> String {
    "mimo_default".to_string()
}

impl Default for MimoTtsConfig {
    fn default() -> Self {
        Self {
            api_key: None,
            base_url: default_mimo_base_url(),
            model: default_mimo_model(),
            voice: default_mimo_voice(),
            style: String::new(),
            prompt: String::new(),
            sample_audio: None,
        }
    }
}

impl MimoTtsConfig {
    pub fn has_key(&self) -> bool {
        self.api_key
            .as_deref()
            .is_some_and(|key| !key.trim().is_empty())
    }
}

/// 播报供应商 id 与显示名(TUI/WebUI 列表顺序)。
pub const TTS_PROVIDERS: &[(&str, &str)] = &[("minimax", "MiniMax"), ("mimo", "Xiaomi MiMo")];

/// MiniMax `t2a_v2` 播报配置。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MiniMaxTtsConfig {
    /// API key,支持 `$env:VAR` 引用。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    /// 国内 `https://api.minimaxi.com/v1`,国际 `https://api.minimax.io/v1`。
    #[serde(default = "default_minimax_base_url")]
    pub base_url: String,
    #[serde(default = "default_tts_model")]
    pub model: String,
    /// 音色 id(系统音色名或克隆音色 id)。
    #[serde(default = "default_tts_voice")]
    pub voice_id: String,
    /// 语速 0.5~2.0。
    #[serde(default = "default_unit")]
    pub speed: f32,
    /// 音量 0.1~10。
    #[serde(default = "default_unit")]
    pub vol: f32,
    /// 音调:半音偏移 -12~12,0 原声。
    #[serde(default)]
    pub pitch: i32,
    /// 情绪:空=模型自定;happy | sad | angry | fearful | disgusted | surprised | calm | fluent | whisper
    #[serde(default)]
    pub emotion: String,
    /// 语种增强:auto 或语种名(Chinese / English / Japanese …)。
    #[serde(default = "default_language_boost")]
    pub language_boost: String,
}

fn default_minimax_base_url() -> String {
    "https://api.minimaxi.com/v1".to_string()
}
fn default_tts_model() -> String {
    "speech-2.6-turbo".to_string()
}
fn default_tts_voice() -> String {
    "Chinese_sweet_girl_nv1".to_string()
}
fn default_unit() -> f32 {
    1.0
}
fn default_language_boost() -> String {
    "auto".to_string()
}
fn default_tts_max_chars() -> usize {
    300
}
fn default_tts_preview_text() -> String {
    "今天也是充满希望的一天".to_string()
}

impl Default for VoiceTtsConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            active: None,
            max_chars: default_tts_max_chars(),
            preview_text: default_tts_preview_text(),
            minimax: MiniMaxTtsConfig::default(),
            mimo: MimoTtsConfig::default(),
        }
    }
}

impl Default for MiniMaxTtsConfig {
    fn default() -> Self {
        Self {
            api_key: None,
            base_url: default_minimax_base_url(),
            model: default_tts_model(),
            voice_id: default_tts_voice(),
            speed: 1.0,
            vol: 1.0,
            pitch: 0,
            emotion: String::new(),
            language_boost: default_language_boost(),
        }
    }
}

impl VoiceTtsConfig {
    /// 播报可用:开关开着,且激活的供应商配好了(`active` 缺省当 MiniMax,
    /// 填了 key 就算配好——装上、填 key、开开关三步即可,不用再点"激活")。
    pub fn is_active(&self) -> bool {
        self.enabled
            && self
                .provider()
                .is_some_and(|provider| self.provider_has_key(provider))
    }

    /// 生效的供应商名:`active` 为空或空串时默认 MiniMax。
    pub fn provider(&self) -> Option<&str> {
        match self.active.as_deref().map(str::trim) {
            None | Some("") => Some("minimax"),
            Some(other) => Some(other),
        }
    }

    /// 某个供应商是否填了 key(未知供应商名 = 没有)。
    pub fn provider_has_key(&self, provider: &str) -> bool {
        match provider {
            "minimax" => self.minimax.has_key(),
            "mimo" => self.mimo.has_key(),
            _ => false,
        }
    }
}

impl MiniMaxTtsConfig {
    pub fn has_key(&self) -> bool {
        self.api_key
            .as_deref()
            .is_some_and(|key| !key.trim().is_empty())
    }
}

fn default_wake_keywords() -> Vec<String> {
    ["未有未有", "密友密友", "miyumiyu", "みゆみゆ"]
        .into_iter()
        .map(str::to_string)
        .collect()
}

/// 把 "未有未有, 小未" 这样的文本拆成唤醒词列表(逗号/顿号/分号/换行分隔,去重)。
pub fn split_wake_keywords(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for part in text.split(|ch: char| matches!(ch, ',' | '，' | '、' | ';' | '；' | '\n')) {
        let part = part.trim();
        if !part.is_empty() && !out.iter().any(|seen| seen == part) {
            out.push(part.to_string());
        }
    }
    out
}

fn deserialize_wake_keywords<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Vec<String>, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum OneOrMany {
        One(String),
        Many(Vec<String>),
    }
    let mut out = Vec::new();
    match OneOrMany::deserialize(deserializer)? {
        OneOrMany::One(text) => out = split_wake_keywords(&text),
        OneOrMany::Many(items) => {
            for item in items {
                for keyword in split_wake_keywords(&item) {
                    if !out.contains(&keyword) {
                        out.push(keyword);
                    }
                }
            }
        }
    }
    if out.is_empty() {
        out = default_wake_keywords();
    }
    Ok(out)
}
fn default_wake_threshold() -> f32 {
    0.25
}
fn default_wake_boost() -> f32 {
    1.0
}
fn default_stt_threads() -> usize {
    2
}
fn default_stt_language() -> String {
    // SenseVoice 自动判语种会把普通话片段判成日语吐假名,默认锁中文。
    "zh".to_string()
}
fn default_stt_unload_seconds() -> u64 {
    60
}
fn default_follow_up_seconds() -> u64 {
    30
}
fn default_min_utterance_chars() -> usize {
    2
}
fn default_sound_volume() -> f32 {
    0.6
}
fn default_notify_reply_chars() -> usize {
    120
}

impl Default for VoiceConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            wake_keywords: default_wake_keywords(),
            wake_threshold: default_wake_threshold(),
            wake_boost: default_wake_boost(),
            microphone: None,
            stt_threads: default_stt_threads(),
            stt_language: default_stt_language(),
            stt_unload_seconds: default_stt_unload_seconds(),
            follow_up_seconds: default_follow_up_seconds(),
            min_utterance_chars: default_min_utterance_chars(),
            sounds: true,
            sound_volume: default_sound_volume(),
            notify_reply_chars: default_notify_reply_chars(),
            dictation_auto_submit: false,
            tts: VoiceTtsConfig::default(),
        }
    }
}

/// Provider prompt-cache tuning (v7, DeepSeek 高命中策略实测产物). The
/// tuning knobs default to off — they trade a little latency or a few cheap
/// requests for prefix-cache hits on best-effort provider caches. The
/// accounting log defaults to on (numbers only, ~0.2 KB per request).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct CacheConfig {
    /// Idle keepalive: while the agent waits for the next user turn, re-send
    /// the exact prompt prefix of the last request every N seconds as a
    /// non-streaming max_tokens=1 completion so hot-tier prefix caches
    /// (DeepSeek-style) keep the deep prefix alive across turn gaps. The ping
    /// is billed at the provider's cache-hit input price. 0 disables (the
    /// default — enable only after measuring your provider: on per-REQUEST
    /// billed endpoints every ping burns quota for nothing).
    /// Only effective in long-lived processes (daemon/REPL); one-shot `ask`
    /// exits before any ping fires.
    pub keepalive_seconds: u64,
    /// Stop pinging after this many keepalives per turn (bounds idle cost).
    pub keepalive_max_pings: u32,
    /// Provider cache writes are asynchronous (measured: a follow-up within
    /// ~2s can miss the prefix the previous request just computed). When >0,
    /// consecutive tool-loop requests wait until at least this many
    /// milliseconds have passed since the previous round completed.
    pub write_grace_ms: u64,
    /// Per-request cache accounting log: one JSONL line of absolute token
    /// numbers (prompt/cache_read/completion/…) per LLM request under
    /// cache/logs/cache-usage.<date>.jsonl. Numbers only — never prompt text.
    /// Roughly 0.2 KB per request; daily files, pruned by retention below.
    pub request_log: bool,
    /// Days of cache-usage JSONL files to keep (older files are deleted when
    /// a new line is written).
    pub request_log_retention_days: u64,
}

impl Default for CacheConfig {
    fn default() -> Self {
        Self {
            keepalive_seconds: 0,
            keepalive_max_pings: 20,
            write_grace_ms: 0,
            request_log: true,
            request_log_retention_days: 14,
        }
    }
}

impl CacheConfig {
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct DisplayConfig {
    #[serde(default = "default_display_language")]
    pub language: String,
    #[serde(default = "default_reasoning_display")]
    pub reasoning: String,
    #[serde(default = "default_tool_call_display")]
    pub tool_calls: String,
    #[serde(default = "default_true")]
    pub readable_tool_names: bool,
    #[serde(default)]
    pub show_token_usage: bool,
    #[serde(default = "default_mixed_model_endpoint_display")]
    pub mixed_model_endpoint_display: String,
    #[serde(default = "default_command_output_lines")]
    pub command_output_lines: usize,
    /// How many finished turns a reopened REPL redraws; 0 disables replay.
    #[serde(default = "default_repl_replay_turns")]
    pub repl_replay_turns: usize,
}

/// Desktop notifications. Both kinds are suppressed while the REPL window has
/// focus — if you are looking at the terminal, a popup is only noise.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NotificationsConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Notify when a reply finishes and Miyu is waiting on you again.
    #[serde(default = "default_true")]
    pub on_turn_complete: bool,
    /// shellhook/单次 CLI 触发的后台任务完成后,把跟进回复写回触发它的那个
    /// 终端。仅在该 shell 仍活着、停在同一 tty 的前台提示符时才写;写不了退化
    /// 为桌面通知。
    #[serde(default = "default_true")]
    pub job_writeback_to_terminal: bool,
}

impl Default for NotificationsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            on_turn_complete: true,
            job_writeback_to_terminal: true,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
struct RawDisplayConfig {
    #[serde(default)]
    language: Option<String>,
    #[serde(default)]
    reasoning: Option<String>,
    #[serde(default)]
    tool_calls: Option<String>,
    #[serde(default)]
    show_reasoning: Option<bool>,
    #[serde(default)]
    reasoning_mode: Option<String>,
    #[serde(default)]
    show_tool_details: Option<bool>,
    #[serde(default)]
    readable_tool_names: Option<bool>,
    #[serde(default)]
    show_token_usage: Option<bool>,
    #[serde(default)]
    show_mixed_model_endpoint: Option<bool>,
    #[serde(default)]
    mixed_model_endpoint_display: Option<String>,
    #[serde(default)]
    command_output_lines: Option<usize>,
    #[serde(default)]
    repl_replay_turns: Option<usize>,
}

impl<'de> Deserialize<'de> for DisplayConfig {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let raw = RawDisplayConfig::deserialize(deserializer)?;
        let reasoning = raw.reasoning.unwrap_or_else(|| {
            if raw.show_reasoning == Some(false) {
                "hidden".to_string()
            } else {
                raw.reasoning_mode.unwrap_or_else(default_reasoning_display)
            }
        });
        let tool_calls = raw.tool_calls.unwrap_or_else(|| {
            if raw.show_tool_details == Some(true) {
                "full".to_string()
            } else {
                default_tool_call_display()
            }
        });
        Ok(Self {
            language: raw.language.unwrap_or_else(default_display_language),
            reasoning,
            tool_calls,
            readable_tool_names: raw.readable_tool_names.unwrap_or_else(default_true),
            show_token_usage: raw.show_token_usage.unwrap_or(false),
            mixed_model_endpoint_display: raw.mixed_model_endpoint_display.unwrap_or_else(|| {
                match raw.show_mixed_model_endpoint {
                    Some(true) => "all".to_string(),
                    Some(false) => "off".to_string(),
                    None => default_mixed_model_endpoint_display(),
                }
            }),
            command_output_lines: raw
                .command_output_lines
                .unwrap_or_else(default_command_output_lines),
            repl_replay_turns: raw
                .repl_replay_turns
                .unwrap_or_else(default_repl_replay_turns),
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PromptConfig {
    #[serde(default = "default_prompts_dir")]
    pub prompts_dir: String,
    #[serde(default = "default_identities_dir")]
    pub identities_dir: String,
    #[serde(default = "default_user_identity_file")]
    pub user_identity_file: String,
    #[serde(default)]
    pub active_persona: String,
    #[serde(default)]
    pub active_identity: String,
    /// 防失忆提醒(自动蒸馏,见 persona_hint 模块)。08-16 起改为
    /// 化石注入:每隔 `persona_reminder_interval` 轮进一次历史,纯追加
    /// 不再掰前缀缓存。A/B 实证干净体制下预设对话已足够→默认禁用。
    #[serde(default)]
    pub persona_reminder: bool,
    /// 相邻两次防失忆提醒之间至少间隔的轮数(>=1)。
    #[serde(default = "default_persona_reminder_interval")]
    pub persona_reminder_interval: u32,
}

/// Identifies who a model prompt is acting for. Only trusted local operator
/// turns may receive the configured user identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptAudience {
    Owner,
    External,
    Internal,
}

impl PromptAudience {
    fn includes_user_identity(self) -> bool {
        matches!(self, Self::Owner)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextConfig {
    /// 工具输出的模型侧内联上限(UTF-8 字节)。超限的纯文本输出全文外溢到
    /// 会话级 spill 文件,模型只看头尾预览+取回提示(read_file/rg 按需读回)。
    /// 0 = 关闭外溢。照抄 dsh 默认 50KB。
    #[serde(default = "default_tool_output_spill_bytes")]
    pub tool_output_spill_bytes: usize,
    #[serde(default = "default_trim_at_ratio")]
    pub trim_at_ratio: f32,
    #[serde(default = "default_trim_batch_ratio")]
    pub trim_batch_ratio: f32,
    #[serde(default = "default_on_overflow")]
    pub on_overflow: String,
    #[serde(default = "default_context_window")]
    pub default_context_window: usize,
    /// Watermark that forces a compaction even when the fold-economics gate
    /// would skip it. Must be >= trim_at_ratio.
    #[serde(default = "default_compact_force_ratio")]
    pub compact_force_ratio: f32,
    /// Verbatim tail budget kept outside the summary, in tokens. None derives
    /// min(16384, window/4) for task modes and 8192 for chat mode; the value
    /// is always capped at window/2 so a small window still lands below the
    /// trigger after compaction (re-compaction loop guard).
    #[serde(default)]
    pub compact_tail_tokens: Option<usize>,
    /// 历史工具结果分级剪枝（字符）：落库时超过 chars 的输出改写成
    /// 「头 head + 省略标记 + 尾 tail」。0 = 关闭。默认值抄 dsh 的
    /// compaction-tool-result-pruner（8192 / 4096 / 1024）。
    #[serde(default = "default_tool_result_prune_chars")]
    pub tool_result_prune_chars: usize,
    #[serde(default = "default_tool_result_prune_head_chars")]
    pub tool_result_prune_head_chars: usize,
    #[serde(default = "default_tool_result_prune_tail_chars")]
    pub tool_result_prune_tail_chars: usize,
    /// Summarization requests fork the live conversation (same byte prefix,
    /// same tools + one appended instruction) so the provider prefix cache
    /// pays for re-reading the history — roughly a 10x input-cost saving on
    /// prefix-cached providers (DeepSeek/OpenAI-compatible/Anthropic). Turn
    /// OFF on per-request-billed gateways where cache hits save nothing: the
    /// isolated fallback path sends the history as plain text instead.
    #[serde(default = "default_true")]
    pub compact_cache_reuse: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolsConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub max_rounds: usize,
    #[serde(default = "default_tools_loading_mode")]
    pub loading_mode: String,
    #[serde(default = "default_true")]
    pub persist_loaded_tools: bool,
    /// How many `task` subagents from one tool batch may run concurrently.
    #[serde(default = "default_subagent_concurrency")]
    pub subagent_concurrency: usize,
    /// 工具执行兜底超时（秒），0=关闭。防没有自管超时的工具（MCP/web/生图
    /// 等）把回合无限挂死；run_command/task/deep_research 等自管或长跑工具
    /// 在 descriptions JSON 里以 timeout_seconds=0 豁免。
    #[serde(default = "default_tools_timeout_secs")]
    pub default_timeout_secs: u64,
    /// run_command 命令拒绝子串。命中即拒（guard 层，回给模型 tool error）。
    /// 防提示注入与模型手滑；默认只收录几乎不可能误伤的毁灭性模式。
    #[serde(default = "default_command_deny")]
    pub command_deny: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub servers: Vec<McpServerConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpServerConfig {
    pub id: String,
    #[serde(default)]
    pub display_name: String,
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: HashMap<String, String>,
    #[serde(default = "default_mcp_timeout")]
    pub timeout_seconds: u64,
    #[serde(default = "default_true")]
    pub enabled: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SkillsConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub allow_command_execution: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MemoryConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_true")]
    pub evicted_context_enabled: bool,
    #[serde(default = "default_true")]
    pub association_enabled: bool,
    #[serde(default = "default_true")]
    pub auto_diary_enabled: bool,
    #[serde(default = "default_true")]
    pub auto_fact_enabled: bool,
    #[serde(default = "default_memory_diary_batch_size")]
    pub diary_batch_size: usize,
    #[serde(default = "default_memory_short_diary_retention_days")]
    pub short_diary_retention_days: u64,
    #[serde(default = "default_memory_diary_promotion_recalls")]
    pub diary_promotion_recalls: u64,
    #[serde(default = "default_memory_organizer_timeout_seconds")]
    pub organizer_timeout_seconds: u64,
    #[serde(default)]
    pub auto_skill_enabled: bool,
    #[serde(default = "default_memory_association_facts")]
    pub association_facts: usize,
    #[serde(default = "default_memory_association_episodes")]
    pub association_episodes: usize,
    #[serde(default = "default_memory_association_max_chars")]
    pub association_max_chars: usize,
    /// 单条联想记忆的正文上限（字符）。日记常把当时那条完整回复整段存进
    /// 去，实测一条 400+ 字符；截断后带 id，模型可用 recall_memories(id=)
    /// 取全文。0 = 不截断。
    #[serde(default = "default_memory_association_entry_chars")]
    pub association_entry_chars: usize,
    /// 同一条记忆若已在本会话早前回合注入过（化石仍在可见上下文中逐字回放），
    /// 本回合不再重复注入。内容或日期变化的记忆视为新条目照常注入。
    #[serde(default = "default_true")]
    pub association_dedup: bool,
    #[serde(default = "default_memory_snippet_chars")]
    pub snippet_chars: usize,
    #[serde(default = "default_memory_forget_after_days")]
    pub forget_after_days: u64,
    #[serde(default = "default_true")]
    pub forgetting_enabled: bool,
    #[serde(default = "default_memory_half_life_days")]
    pub forgetting_half_life_days: f64,
    #[serde(default = "default_memory_min_strength")]
    pub forgetting_min_strength: f64,
    #[serde(default = "default_memory_review_boost")]
    pub forgetting_review_boost: f64,
    #[serde(default = "default_memory_min_task_chars")]
    pub learning_min_task_chars: usize,
    #[serde(default = "default_memory_min_method_chars")]
    pub learning_min_method_chars: usize,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            config_version: CURRENT_CONFIG_VERSION,
            active_provider: OPENCODE_PROVIDER_ID.to_string(),
            active_provider_models: None,
            active_multimodal_provider_models: None,
            providers: ProviderConfig::default_templates(),
            embedding: EmbeddingConfig::default(),
            context: ContextConfig::default(),
            tools: ToolsConfig::default(),
            cache: CacheConfig::default(),
            mcp: McpConfig::default(),
            skills: SkillsConfig::default(),
            display: DisplayConfig::default(),
            notifications: NotificationsConfig::default(),
            prompt: PromptConfig::default(),
            plugins: PluginsConfig::default(),
            memory: MemoryConfig::default(),
            system_prompt_file: Some("system-prompt.md".to_string()),
            default_mode: String::new(),
            system_prompt: None,
            model_tiers: ModelTiersConfig::default(),
            platforms: PlatformsConfig::default(),
            voice: VoiceConfig::default(),
        }
    }
}

impl Default for PromptConfig {
    fn default() -> Self {
        Self {
            prompts_dir: default_prompts_dir(),
            identities_dir: default_identities_dir(),
            user_identity_file: default_user_identity_file(),
            active_persona: String::new(),
            active_identity: String::new(),
            persona_reminder: false,
            persona_reminder_interval: default_persona_reminder_interval(),
        }
    }
}

impl Default for DisplayConfig {
    fn default() -> Self {
        Self {
            language: default_display_language(),
            reasoning: default_reasoning_display(),
            tool_calls: default_tool_call_display(),
            readable_tool_names: default_true(),
            show_token_usage: false,
            mixed_model_endpoint_display: default_mixed_model_endpoint_display(),
            command_output_lines: default_command_output_lines(),
            repl_replay_turns: default_repl_replay_turns(),
        }
    }
}

impl Default for McpConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            servers: Vec::new(),
        }
    }
}

impl Default for ToolsConfig {
    fn default() -> Self {
        Self {
            enabled: default_true(),
            max_rounds: 0,
            loading_mode: default_tools_loading_mode(),
            persist_loaded_tools: default_true(),
            subagent_concurrency: default_subagent_concurrency(),
            default_timeout_secs: default_tools_timeout_secs(),
            command_deny: default_command_deny(),
        }
    }
}

impl Default for SkillsConfig {
    fn default() -> Self {
        Self {
            enabled: default_true(),
            allow_command_execution: default_true(),
        }
    }
}

impl Default for MemoryConfig {
    fn default() -> Self {
        Self {
            enabled: default_true(),
            evicted_context_enabled: default_true(),
            association_enabled: default_true(),
            auto_diary_enabled: default_true(),
            auto_fact_enabled: default_true(),
            diary_batch_size: default_memory_diary_batch_size(),
            short_diary_retention_days: default_memory_short_diary_retention_days(),
            diary_promotion_recalls: default_memory_diary_promotion_recalls(),
            organizer_timeout_seconds: default_memory_organizer_timeout_seconds(),
            auto_skill_enabled: false,
            association_facts: default_memory_association_facts(),
            association_episodes: default_memory_association_episodes(),
            association_max_chars: default_memory_association_max_chars(),
            association_entry_chars: default_memory_association_entry_chars(),
            association_dedup: default_true(),
            snippet_chars: default_memory_snippet_chars(),
            forget_after_days: default_memory_forget_after_days(),
            forgetting_enabled: default_true(),
            forgetting_half_life_days: default_memory_half_life_days(),
            forgetting_min_strength: default_memory_min_strength(),
            forgetting_review_boost: default_memory_review_boost(),
            learning_min_task_chars: default_memory_min_task_chars(),
            learning_min_method_chars: default_memory_min_method_chars(),
        }
    }
}

impl Default for ContextConfig {
    fn default() -> Self {
        Self {
            tool_output_spill_bytes: default_tool_output_spill_bytes(),
            trim_at_ratio: default_trim_at_ratio(),
            trim_batch_ratio: default_trim_batch_ratio(),
            on_overflow: default_on_overflow(),
            default_context_window: default_context_window(),
            compact_force_ratio: default_compact_force_ratio(),
            compact_tail_tokens: None,
            tool_result_prune_chars: default_tool_result_prune_chars(),
            tool_result_prune_head_chars: default_tool_result_prune_head_chars(),
            tool_result_prune_tail_chars: default_tool_result_prune_tail_chars(),
            compact_cache_reuse: true,
        }
    }
}

impl AppConfig {}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod scaling_probe {
    use super::*;
    use std::time::Instant;

    /// 量尺：`cargo test --lib config::scaling_probe -- --ignored --nocapture`
    ///
    /// `handle_message_with_activity` 第一件事就是深拷贝整份 AppConfig，
    /// 在 `qq.enabled` 检查之前、在准入判定之前——每条被丢弃的群消息也照付。
    /// 这里量的是「一条消息的入场费」。
    #[test]
    #[ignore]
    fn app_config_clone_cost() {
        // 照着实际配置的规模造:22 个供应商、每家 3 个模型、默认敏感词表
        let mut config = AppConfig::default();
        let template = config.providers[0].clone();
        config.providers.clear();
        for index in 0..22 {
            let mut provider = template.clone();
            provider.id = format!("provider{index}");
            provider.models = (0..3).map(|m| format!("model-{index}-{m}")).collect();
            for model in &provider.models {
                provider
                    .model_modalities
                    .insert(model.clone(), vec!["text".to_string(), "image".to_string()]);
                provider.model_context_window.insert(model.clone(), 128_000);
            }
            config.providers.push(provider);
        }

        let json = serde_json::to_string(&config).unwrap();
        println!("\n  序列化后 {} KB", json.len() / 1024);

        // 预热
        for _ in 0..100 {
            std::hint::black_box(config.clone());
        }
        let rounds = 10_000;
        let start = Instant::now();
        for _ in 0..rounds {
            std::hint::black_box(config.clone());
        }
        let each_us = start.elapsed().as_secs_f64() * 1e6 / rounds as f64;
        println!("  单次 clone   {each_us:>8.1} µs");
        println!("  一条消息按 3 次算 {:>6.1} µs", each_us * 3.0);
        println!(
            "  1000 条/分钟的群 每分钟 {:>6.1} ms",
            each_us * 3.0 * 1000.0 / 1000.0
        );
    }
}
