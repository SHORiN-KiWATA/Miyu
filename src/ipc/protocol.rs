//! 帧格式与请求/响应类型。
//!
//! `MAX_FRAME_BYTES` 是必须的：帧长度是从对端读来的数字，照着它分配就等于把内
//! 存交给对端——即便对端是自己人，一个字段写错也够崩。
//!
//! `PROTOCOL_VERSION` 让新旧客户端能互相识别：版本不匹配时明确报出来，比字段
//! 缺失导致的怪行为好排查得多。

use crate::ipc::*;

pub const PROTOCOL_VERSION: u16 = 3;

pub(crate) const MAX_FRAME_BYTES: usize = 24 * 1024 * 1024;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SessionState {
    pub context_tokens: u64,
    pub context_window: Option<usize>,
    /// `context_window` 是不是猜出来的。默认 false——跟老 daemon 说话时按
    /// 「有出处」处理，显示跟以前一模一样，不会平白多出一堆波浪号。
    #[serde(default)]
    pub context_window_assumed: bool,
    pub cumulative_tokens: u64,
    /// Prompt and cache-read halves behind Σ's cache rate. Defaulted so a REPL
    /// talking to an older daemon degrades to "no cache reported" instead of
    /// failing to parse the state frame.
    #[serde(default)]
    pub cumulative_prompt_tokens: u64,
    #[serde(default)]
    pub cumulative_cache_read_tokens: u64,
    #[serde(default)]
    pub session_id: String,
    #[serde(default)]
    pub session_name: String,
    #[serde(default)]
    pub workspace: Option<String>,
}

/// Reference to a chat session in IPC commands.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SessionRef {
    /// The daemon's current session.
    Current,
    /// A session by exact id.
    Id { id: String },
    /// A user session of the active persona by (case-insensitive) name.
    Name { name: String },
}

/// 「仅本回合生效」的覆盖集。每一项都是 `None`/空 = 不覆盖。
///
/// 设计约束:这些值**不写 config、不写会话覆盖表**,回合结束即消失。取值
/// 无界的项(窗口、提示词)走 Agent 字段而不是改 config,免得把
/// `TurnResourceCache`(键=整份 config)冲刷掉。
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct TurnOverrides {
    /// 本回合模型池;空 = 沿用会话/全局池。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub models: Vec<crate::config::ActiveProviderModelConfig>,
    /// 本回合上下文窗口(token)。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window: Option<usize>,
    /// 整体替换人格/模式提示词(掉缓存,宿主自担)。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_prompt: Option<String>,
    /// 追加在系统提示词末尾的宿主指令(进 system 侧,不化石)。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub append_system_prompt: Option<String>,
    /// `Some(false)` = 本回合不写长期记忆/日记/经历,也不给 remember_fact。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_writes: Option<bool>,
    /// 工具白名单;`Some(vec![])` = 一个工具都不给。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_allowlist: Option<Vec<String>>,
}

impl TurnOverrides {
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Request {
    pub version: u16,
    #[serde(flatten)]
    pub command: Command,
}

impl Request {
    pub fn new(command: Command) -> Self {
        Self {
            version: PROTOCOL_VERSION,
            command,
        }
    }
}

/// 触发回合的终端身份:tty 设备路径 + 拉起 miyu 的 shell 进程。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OriginTty {
    pub path: std::path::PathBuf,
    pub shell_pid: u32,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "command", rename_all = "snake_case")]
pub enum Command {
    Ping,
    Shutdown,
    ReloadConfig,
    GetStatus,
    /// Lightweight poll for the REPL background-command status strip.
    JobsOverview,
    /// Attach to a running daemon-initiated turn (background-command wake)
    /// and stream its event frames until it finishes.
    FollowRun {
        run_id: String,
    },
    /// Stop all running background commands of a session (REPL exit).
    StopSessionJobs {
        session_id: String,
    },
    GetSessionState {
        target: SessionRef,
    },
    /// Re-initializes one conversation: history, queue, per-session usage and
    /// the recall caches that belong to it. Only ever sent by the first-party
    /// frontends (CLI, REPL, WebUI) — platform sessions are rejected upstream
    /// and clear themselves through `ClearSessionContent`.
    ResetConversation {
        target: SessionRef,
    },
    /// 只清长期记忆(事实/日记/经历/待处理事件与外溢上下文存档),会话
    /// 历史与技能不动。`mode: "dev"` 清开发模式的独立记忆命名空间,
    /// 缺省清当前人格。不可逆,前端须先确认。
    ResetMemory {
        #[serde(default)]
        mode: Option<String>,
    },
    /// 出网请求录制开关(进程级,重启即关)。开着时每个 LLM 请求的完整
    /// 序列化体追加到 logs/requests-<日期>.jsonl,供审计注入内容。
    SetRequestLogging {
        enabled: bool,
    },
    /// Erases everything the persona accumulated: memory, every session's
    /// contents, group-chat contexts and auto-generated skills. Configuration
    /// is untouched. Irreversible; every frontend confirms before sending it.
    WipePersona,
    Undo {
        target: SessionRef,
    },
    Pop {
        target: SessionRef,
        turn_ids: Vec<String>,
    },
    Compact {
        target: SessionRef,
    },
    /// `/goal ...`：目标的读写都在 daemon 里做。
    ///
    /// 不在客户端直连库，是因为「是否自动续跑」（armed）驻在 daemon 内存——
    /// REPL 进程自己设那个标记，续轮驱动器根本看不见。
    Goal {
        target: SessionRef,
        input: String,
    },
    StartTurn {
        content: String,
        mode: String,
        #[serde(default)]
        images: Vec<Option<ImageAttachment>>,
        /// 触发本回合的终端身份(shellhook/单次 CLI)。后台任务完成后的
        /// 跟进回复据此写回原终端;缺省(REPL/WebUI/平台)不回写。
        #[serde(default)]
        origin_tty: Option<OriginTty>,
        /// Client working directory; used as the turn workspace when the
        /// target session has none bound.
        #[serde(default)]
        cwd: Option<std::path::PathBuf>,
        /// Target session id. Defaults to the global current session; when
        /// set, the turn runs there without moving the current pointer.
        #[serde(default)]
        session_id: Option<String>,
        /// 仅本回合生效的覆盖(模型/窗口/提示词/记忆/工具面),不落盘。
        /// 程序驱动的 CLI(`miyu ask --model …`、`miyu stdio`)用;REPL/WebUI
        /// 不传。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        overrides: Option<TurnOverrides>,
    },
    QueueTurnUpdate {
        run_id: String,
        turn_id: String,
        content: String,
        display_content: String,
        #[serde(default)]
        images: Vec<Option<ImageAttachment>>,
        #[serde(default)]
        supersede: bool,
    },
    Cancel {
        run_id: String,
    },
    AnswerQuestion {
        question_id: String,
        answers: QuestionAnswers,
    },
    /// Resolve a question without an answer, when the client cannot present it
    /// at all. Distinct from `Cancel`: the turn keeps going and the tool that
    /// asked simply learns nobody answered.
    CloseQuestion {
        question_id: String,
    },
    ListSessions {
        /// `dev` 列开发模式会话(保留人格 "dev" 名下),`all` 普通+dev
        /// 合并(管理面);缺省=当前人格。
        #[serde(default)]
        mode: Option<String>,
    },
    CreateSession {
        #[serde(default)]
        name: Option<String>,
        #[serde(default)]
        switch: bool,
        /// `user` (default) or `ask`; anything else is rejected. `ask` sessions
        /// back one-shot turns and stay out of every listing.
        #[serde(default)]
        kind: Option<String>,
        /// `"dev"` 建到保留人格 dev 名下(Build 模式会话);缺省=当前人格。
        #[serde(default)]
        mode: Option<String>,
    },
    /// 会话列表手动排序:按给定顺序重写展示序(WebUI 侧栏拖拽)。
    ReorderSessions {
        session_ids: Vec<String>,
    },
    /// Session the REPL was last on. Falls back to the current session and
    /// heals the stored pointer when it has gone stale.
    GetReplSession {
        /// `"dev"` 取 dev 人格的 REPL 指针(无则自举一个 dev 会话)。
        #[serde(default)]
        mode: Option<String>,
    },
    SetReplSession {
        target: SessionRef,
    },
    /// 工具桥(任务#12):`miyu tool-call` 打回 daemon,以指定会话的身份与
    /// 回合来源执行结构化工具——内层调用照走 guard/超时管线。bash 就是
    /// 编排层:中间数据在脚本里流动,不经模型上下文往返。
    ToolCall {
        #[serde(default)]
        session: Option<String>,
        name: String,
        #[serde(default)]
        arguments: String,
        /// 序列化的 TurnOrigin(来自 run_command 注入的 MIYU_TURN_ORIGIN)。
        #[serde(default)]
        origin: Option<String>,
        /// 递归深度(护栏,daemon 侧校验)。
        #[serde(default)]
        depth: u32,
    },
    /// 工具桥目录:`--list/--describe` 与 ToolCall 同一条解析链(会话→
    /// 模式→registry),杜绝"--list 列全量、调用却 unknown tool"的错位
    /// (dev 会话实测踩坑)。name=None 列全表,Some(name) 查单个合同。
    ToolCatalog {
        #[serde(default)]
        session: Option<String>,
        #[serde(default)]
        name: Option<String>,
        /// true 时列表项带完整合同(description+parameters):MCP 桥一次
        /// tools/list 拿全量,免去逐工具 describe 的 N 次往返。
        #[serde(default)]
        full: bool,
    },
    RenameSession {
        target: SessionRef,
        name: String,
    },
    DeleteSession {
        target: SessionRef,
    },
    SetWorkspace {
        target: SessionRef,
        #[serde(default)]
        path: Option<std::path::PathBuf>,
    },
    /// Pins the target session to its own model pool. An empty list clears
    /// the override so the session follows the global active pool again.
    SetSessionModels {
        target: SessionRef,
        #[serde(default)]
        models: Vec<crate::config::ActiveProviderModelConfig>,
    },
    /// `miyu-voice` 进程注册的持久信令连接。应答 Ack 后双向裸交换 Event
    /// 帧(见 `voice::worker` 模块文档的信令表)。
    VoiceAttach,
    /// 客户端(REPL `/stt`、`miyu stt`)认领一条听写流:daemon 让语音前端
    /// 开听写窗,识别文本以 Event 帧 `voice.dictation {text}` 流回,窗口
    /// 结束发 `voice.dictation_ended`;连接断开即释放。
    StartDictation,
    /// 语音前端状态(二进制是否存在、是否在跑、设备名等)。应答
    /// Event `voice.status`。
    VoiceStatus,
    /// 让语音前端不用唤醒词直接进入等待指令状态(`miyu listen`,桌面
    /// 快捷键呼叫)。应答 Ack;语音未启用/前端未就绪/听写中为 Error。
    VoiceListen,
    /// 合成并播出一段文本(`miyu voice say`、设置页试听)。`tts` 为 Some 时用
    /// 这份配置(TUI 里试听尚未保存的音色/语速),否则用 daemon 当前配置。
    /// 应答 Ack(已交给前端播)或 Error。
    VoiceSpeak {
        text: String,
        #[serde(default)]
        tts: Option<crate::config::VoiceTtsConfig>,
    },
    /// 删除唤醒对话的专属会话,下次唤醒重建(`miyu voice reset`)。应答 Ack。
    VoiceReset,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ImageAttachment {
    Binary { mime: String, data: Vec<u8> },
    Path { path: String },
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Frame {
    Ready {
        pid: u32,
        #[serde(default)]
        web_port: u16,
        #[serde(default)]
        web_public: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        web_bind: Option<std::net::IpAddr>,
        #[serde(default)]
        build_id: String,
    },
    Accepted {
        run_id: String,
        /// Present when attaching to an already-running turn (FollowRun).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        turn_id: Option<String>,
    },
    TurnUpdateAccepted {
        run_id: String,
        turn_id: String,
        prompt_id: String,
        seq: i64,
        submitted_at: String,
    },
    Event {
        id: u64,
        kind: String,
        data: Value,
    },
    Ack,
    AdminResult {
        state: SessionState,
        data: Value,
    },
    Error {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        code: Option<ErrorCode>,
        message: String,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    Busy,
    #[serde(other)]
    Unknown,
}

impl Frame {
    pub fn error(message: impl Into<String>) -> Self {
        Self::Error {
            code: None,
            message: message.into(),
        }
    }

    pub fn coded_error(code: ErrorCode, message: impl Into<String>) -> Self {
        Self::Error {
            code: Some(code),
            message: message.into(),
        }
    }
}

pub(crate) fn expected_protocol_version(message: &str) -> Option<u16> {
    message
        .strip_prefix("unsupported IPC protocol version ")?
        .rsplit_once("; expected ")?
        .1
        .parse()
        .ok()
}

pub async fn send<T: Serialize>(stream: &mut UnixStream, value: &T) -> Result<()> {
    let bytes = serde_json::to_vec(value)?;
    if bytes.len() > MAX_FRAME_BYTES {
        bail!("IPC frame exceeds the 24 MiB limit");
    }
    stream.write_u32(bytes.len() as u32).await?;
    stream.write_all(&bytes).await?;
    stream.flush().await?;
    Ok(())
}

pub async fn receive<T: DeserializeOwned>(stream: &mut UnixStream) -> Result<Option<T>> {
    let length = match stream.read_u32().await {
        Ok(length) => length as usize,
        Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    if length == 0 || length > MAX_FRAME_BYTES {
        bail!("invalid IPC frame length: {length}");
    }
    let mut bytes = vec![0; length];
    stream.read_exact(&mut bytes).await?;
    Ok(Some(serde_json::from_slice(&bytes)?))
}
