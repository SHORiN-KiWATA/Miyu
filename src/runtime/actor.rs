//! actor 的指令集与管理操作的失败类型。
// 兄弟模块的类型互相引用（DaemonState 持有 EventHub、run 记录引用
// ManagerState 等），统一从 mod.rs 的再导出取，免得每个文件维护一份
// 交叉导入清单。
use super::*;
use crate::agent::AgentMode;
use crate::config::{ActiveProviderModelConfig, AppConfig, PromptAudience};
use crate::ipc::ImageAttachment;
use serde_json::Value;
use std::sync::Arc;
use tokio::sync::oneshot;

// ── ActorCommand 与几种管理操作失败 ──
pub(crate) enum ActorCommand {
    StartTurn {
        run_id: String,
        session_id: Arc<str>,
        content: String,
        display_content: String,
        attachment_run_id: Option<String>,
        mode: AgentMode,
        images: Vec<Option<ImageAttachment>>,
        cwd: Option<std::path::PathBuf>,
        /// 触发回合的终端(shellhook/单次 CLI);后台任务完成回写用。
        /// 装箱:PathBuf+pid 内联 32B,队列项 512B 护栏本就顶格(见 turn_origin 注)。
        origin_tty: Option<Box<crate::ipc::OriginTty>>,
        audience: PromptAudience,
        /// Platform-only per-turn overrides. CLI/WebUI turns leave this empty.
        profile: Option<crate::platforms::TurnProfile>,
        /// 程序驱动 CLI 的「仅本回合」覆盖;其余来源为 None。装箱同 turn_origin。
        overrides: Option<Box<crate::ipc::TurnOverrides>>,
        cancel: tokio::sync::watch::Receiver<bool>,
        /// 回合发起来源(缺省 Human;goal 驱动器与 job 唤醒如实声明)。
        /// 装箱:GoalRound 变体带 String,内联会顶爆 ActorCommand 的
        /// 512B 队列项护栏。
        turn_origin: Box<crate::tools::workspace::TurnOrigin>,
    },
    RedoTurn {
        run_id: String,
        session_id: Arc<str>,
        candidate: crate::state::RedoCandidate,
        prompts: Vec<RedoWebPrompt>,
        mode: AgentMode,
        cancel: tokio::sync::watch::Receiver<bool>,
    },
    SetModels {
        models: Vec<ActiveProviderModelConfig>,
        reply: oneshot::Sender<std::result::Result<(), AdminFailure>>,
    },
    SetThinkingVariants {
        updates: Vec<ThinkingVariantUpdate>,
        reply: oneshot::Sender<std::result::Result<(), AdminFailure>>,
    },
    ApplyConfig {
        config: Box<AppConfig>,
        prompts: PromptDocuments,
        reset_conversation: bool,
        reply: oneshot::Sender<std::result::Result<(), AdminFailure>>,
    },
    ResetConversation {
        session_id: Arc<str>,
        reply: oneshot::Sender<std::result::Result<(), AdminFailure>>,
    },
    ResetPersonaState {
        config: Box<AppConfig>,
        reply: oneshot::Sender<std::result::Result<(), AdminFailure>>,
    },
    ClearSessionContent {
        session_id: Arc<str>,
        reply: oneshot::Sender<std::result::Result<(), AdminFailure>>,
    },
    SwitchSession {
        session_id: String,
        release_reservation: bool,
        reply: oneshot::Sender<std::result::Result<(), AdminFailure>>,
    },
    Undo {
        session_id: Arc<str>,
        reply: oneshot::Sender<std::result::Result<Value, AdminFailure>>,
    },
    Pop {
        session_id: Arc<str>,
        turn_ids: Vec<String>,
        reply: oneshot::Sender<std::result::Result<Value, AdminFailure>>,
    },
    Compact {
        session_id: Arc<str>,
        /// 摘要生成的实时事件出口(`context.compact_start|delta|end` 的
        /// kind + data)。手动压缩要跑一次完整的模型调用,几十秒里一个字都不
        /// 出的话用户只能看着光标发呆——自动压缩早就是流式的(回合的事件
        /// 映射器管着),这条通道让手动路也能同样喂给终端。
        /// `None` = 不需要流(WebUI 现在走这条:它的压缩进度条挂在回合气泡
        /// 上,手动压缩没有气泡可挂)。发送端满/断开一律忽略,绝不能让
        /// 渲染问题打断已经在跑的压缩。
        events: Option<tokio::sync::mpsc::UnboundedSender<(String, Value)>>,
        reply: oneshot::Sender<std::result::Result<Value, AdminFailure>>,
    },
    Shutdown,
}

#[derive(Debug)]
pub(crate) enum AdminFailure {
    Invalid(String),
    Internal(String),
}

#[derive(Debug)]
pub(crate) enum PlatformSessionResetError {
    Busy,
    Unavailable,
    Internal(String),
}

#[derive(Debug)]
pub(crate) enum PlatformPersonaResetError {
    Busy,
    Unavailable,
    Internal(String),
}
