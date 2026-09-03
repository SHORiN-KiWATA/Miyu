//! 会话库的记录类型。
//!
//! 纯数据 + serde，不含 SQL。分出来是因为这些类型全 crate 都在传，而库的实现
//! 细节只有 [`super`] 关心。
//!
//! `MAX_*_BYTES` 那组上限限制的是**写进库的单条记录**：流水事件、重做检查点都
//! 可能被模型的输出撑大，没有上限就是把库的大小交给模型决定。

use crate::state::conversation_db::*;

pub(crate) const PENDING_PLACEHOLDER: &str = "<system-reminder>上一轮prompt正在由另一轮回复处理中，你只需要回应用户当前的prompt，不要处理上一轮的prompt</system-reminder>";

pub(crate) const INTERRUPTED_TEXT: &str =
    "<system-reminder>上一轮prompt已被中断，除非用户重新要求否则不要处理上一轮的prompt</system-reminder>";

/// Budget for a finished turn's display transcript. Generous enough for a
/// normal turn's prose plus a handful of tool blocks, small enough that a
/// session's worth of them stays cheap to load.
pub(crate) const REPLAY_JOURNAL_MAX_CHARS: usize = 8 * 1024;

/// Per-entry clamp so one runaway tool result cannot eat the whole budget.
pub(crate) const REPLAY_ENTRY_MAX_CHARS: usize = 2 * 1024;

/// One entry of a finished turn's display transcript, in stream order.
///
/// Reconstructed from the live journal just before it is dropped, so the
/// interleaving of prose and tool blocks survives — which is the whole point,
/// since `assistant_content` alone would flatten a turn into one paragraph.
/// Command output tails are deliberately absent: they are the bulky part and
/// the settled block reads fine without them.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ReplayEntry {
    Text {
        text: String,
    },
    ToolCall {
        name: String,
        #[serde(default)]
        arguments: String,
    },
    ToolResult {
        name: String,
        ok: bool,
        #[serde(default)]
        output: String,
    },
}

/// `app_state` key prefixes for the two persona-scoped session pointers. The
/// terminal lane (shell-hook, `miyu new`/`session`) and the REPL lane move
/// independently; one-shot `ask` turns use neither.
pub(crate) const CURRENT_SESSION_POINTER: &str = "current_session_persona";

pub(crate) const REPL_SESSION_POINTER: &str = "repl_session_persona";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnStatus {
    Running,
    Completed,
    Interrupted,
}

#[allow(dead_code)]
impl TurnStatus {
    pub(crate) fn as_str(&self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Completed => "completed",
            Self::Interrupted => "interrupted",
        }
    }

    pub(crate) fn from_str(s: &str) -> Self {
        match s {
            "completed" => Self::Completed,
            "interrupted" => Self::Interrupted,
            _ => Self::Running,
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct PruneStats {
    pub turns: usize,
    pub saved_chars: usize,
}

/// Deterministic per-turn tool footprint. BTreeSet: sorted, deduplicated,
/// byte-deterministic serialization (cache-purity requirement for anything
/// that ends up in a rendered summary).
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct ToolFootprint {
    #[serde(default, skip_serializing_if = "std::collections::BTreeSet::is_empty")]
    pub read: std::collections::BTreeSet<String>,
    #[serde(default, skip_serializing_if = "std::collections::BTreeSet::is_empty")]
    pub modified: std::collections::BTreeSet<String>,
    #[serde(default, skip_serializing_if = "std::collections::BTreeSet::is_empty")]
    pub memories: std::collections::BTreeSet<String>,
}

impl ToolFootprint {
    pub fn is_empty(&self) -> bool {
        self.read.is_empty() && self.modified.is_empty() && self.memories.is_empty()
    }

    pub fn merge(&mut self, other: ToolFootprint) {
        self.read.extend(other.read);
        self.modified.extend(other.modified);
        self.memories.extend(other.memories);
    }
}

/// 一轮工具调用:assistant(可带思考)发起若干 call,随后各自的结果。
/// `output` 与该轮模型实际看到的字节一致(超限时是 spill 预览),回放即重现。
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ToolFlowRound {
    /// 中转(claude-code)侧闭环执行的工具活动:仅供 UI 重绘,**不参与历史
    /// 回放**(claude 会话自带这些上下文;回放会污染前缀并打断续传)。
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub remote: bool,
    #[serde(default)]
    pub assistant_content: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assistant_reasoning: Option<String>,
    pub calls: Vec<ToolFlowCall>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ToolFlowCall {
    pub id: String,
    pub name: String,
    /// 模型原样产出的 JSON 字符串,不解析不重排(dsh:字节保真)。
    pub arguments: String,
    pub output: String,
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct Turn {
    pub turn_id: String,
    pub seq: i64,
    pub user_content: String,
    pub display_content: String,
    pub user_timestamp: String,
    pub assistant_content: String,
    pub assistant_reasoning: Option<String>,
    pub assistant_provider_id: Option<String>,
    pub assistant_model: Option<String>,
    pub assistant_timestamp: Option<String>,
    pub status: TurnStatus,
    pub tool_reports: Vec<String>,
    /// 结构化工具流(v20+):非空时历史回放走原生 tool_calls/role:"tool" 形态,
    /// tool_reports 只服务 UI 与旧回合兜底。
    pub tool_flow: Vec<ToolFlowRound>,
    pub question_exchanges: Vec<QuestionExchange>,
    pub followups: Vec<TurnFollowup>,
    pub attachments: Vec<UserAttachment>,
    pub hidden: bool,
    pub is_summary: bool,
    pub owner_pid: Option<i64>,
    pub token_total: u64,
    /// Prompt half of the turn's usage and how much of it the provider served
    /// from cache. A hit rate needs the prompt as its denominator, not the
    /// total: output tokens only enter the prompt on the *next* turn.
    pub token_prompt: u64,
    pub token_cache_read: u64,
    pub token_usage_estimated: bool,
    pub revision: i64,
    /// Semantic events for a non-completed generation. Completed turns keep
    /// this empty so normal history loading does not materialize large logs.
    pub journal_events: Vec<TurnJournalEvent>,
    /// Fossilized transient tail (v7 append-only): the system messages that
    /// followed the user message in the live request, replayed verbatim so the
    /// provider prefix cache sees a pure extension instead of a divergence.
    pub context_messages: Vec<ChatMessage>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnJournalEvent {
    pub event_id: i64,
    pub revision: i64,
    pub segment_index: i64,
    pub kind: String,
    pub call_id: Option<String>,
    pub name: Option<String>,
    pub text_payload: Option<String>,
    pub blob_payload: Option<Vec<u8>>,
    pub ok: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TurnRedoCheckpointPayload {
    pub replay_messages: Vec<ChatMessage>,
    pub prefix_tool_reports: Vec<String>,
    pub tool_rounds: usize,
    pub question_rounds: usize,
    pub loaded_items: Vec<(String, String, Option<String>)>,
    pub prefix_question_count: usize,
    pub prefix_image_asset_ids: Vec<String>,
    #[serde(default)]
    pub prefix_artifact_asset_ids: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct TurnRedoCheckpoint {
    pub batch_prompt_ids: Vec<String>,
    pub payload: Option<TurnRedoCheckpointPayload>,
    pub unavailable_reason: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RedoInputKind {
    Initial,
    Followup,
}

#[derive(Debug, Clone)]
pub struct RedoCandidate {
    pub turn_id: String,
    pub revision: i64,
    pub input_id: String,
    pub input_kind: RedoInputKind,
    pub display_content: String,
    pub batch_prompt_ids: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct RedoStart {
    pub revision: i64,
    pub checkpoint: Option<TurnRedoCheckpointPayload>,
}

#[derive(Debug, Clone)]
pub struct StaleTurnRecovery {
    pub turn_id: String,
    pub session_id: String,
    pub restored_redo: bool,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct TurnRedoBackup {
    pub(crate) status: String,
    pub(crate) user_content: String,
    pub(crate) display_content: String,
    pub(crate) followup_content: Option<String>,
    pub(crate) followup_display_content: Option<String>,
    pub(crate) followup_context_content: Option<String>,
    pub(crate) assistant_content: String,
    pub(crate) assistant_reasoning: Option<String>,
    pub(crate) assistant_provider_id: Option<String>,
    pub(crate) assistant_model: Option<String>,
    pub(crate) assistant_timestamp: Option<String>,
    pub(crate) tool_reports: String,
    pub(crate) owner_pid: Option<i64>,
    pub(crate) queue_session_id: Option<String>,
    pub(crate) token_total: i64,
    #[serde(default)]
    pub(crate) token_prompt: i64,
    #[serde(default)]
    pub(crate) token_cache_read: i64,
    pub(crate) token_usage_estimated: i64,
    pub(crate) loaded_items: Vec<(String, String, Option<String>, String, String)>,
    pub(crate) consumed_prompt_ids: Vec<String>,
    pub(crate) checkpoint: Option<RedoCheckpointBackup>,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct RedoCheckpointBackup {
    pub(crate) version: i64,
    pub(crate) batch_prompt_ids: String,
    pub(crate) payload: Option<Vec<u8>>,
    pub(crate) unavailable_reason: Option<String>,
    pub(crate) created_at: String,
}

pub(crate) const REDO_CHECKPOINT_VERSION: i64 = 1;

pub(crate) const MAX_REDO_CHECKPOINT_BYTES: usize = 2 * 1024 * 1024;

pub(crate) const MAX_JOURNAL_TEXT_EVENT_BYTES: usize = 64 * 1024 * 1024;

pub(crate) const MAX_JOURNAL_BLOB_EVENT_BYTES: usize = 8 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum QueuedPromptAttachment {
    Binary { mime: String, data_base64: String },
    Path { path: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueuedPrompt {
    pub prompt_id: String,
    pub seq: i64,
    pub content: String,
    pub display_content: String,
    pub attachments: Vec<QueuedPromptAttachment>,
    pub uploaded_attachments: Vec<UserAttachment>,
    pub submitted_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnFollowup {
    pub prompt_id: String,
    pub content: String,
    pub display_content: String,
    pub attachments: Vec<QueuedPromptAttachment>,
    pub uploaded_attachments: Vec<UserAttachment>,
    pub submitted_at: String,
    pub preceding_assistant_content: Option<String>,
    pub preceding_assistant_reasoning: Option<String>,
    pub preceding_assistant_provider_id: Option<String>,
    pub preceding_assistant_model: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserAttachment {
    pub attachment_id: String,
    pub file_name: String,
    pub mime: String,
    pub kind: String,
    pub size_bytes: u64,
    pub width: u32,
    pub height: u32,
    pub created_at: String,
}

/// 附件本体。旧行把内容存在 `data` 列里,`path` 为 `None`;v29 起新上传一律
/// 落盘,`path` 指向磁盘文件。`file` 类附件不读内容进内存(可能是几 GB 的
/// 视频),`bytes` 为空,消费方只拿路径。
#[derive(Debug, Clone)]
pub struct UserAttachmentData {
    pub attachment: UserAttachment,
    pub bytes: Vec<u8>,
    pub path: Option<std::path::PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageAsset {
    pub asset_id: String,
    pub turn_id: String,
    pub tool_id: Option<String>,
    pub mime: String,
    pub width: u32,
    pub height: u32,
    pub alt: String,
    pub created_at: String,
}

#[derive(Debug, Clone)]
pub struct ImageAssetData {
    pub asset: ImageAsset,
    pub bytes: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtifactAsset {
    pub asset_id: String,
    pub turn_id: String,
    pub tool_id: Option<String>,
    pub source_key: String,
    pub file_name: String,
    pub mime: String,
    pub kind: String,
    pub size_bytes: u64,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone)]
pub struct ArtifactAssetData {
    pub asset: ArtifactAsset,
    pub bytes: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionRecord {
    pub session_id: String,
    pub persona: String,
    pub name: String,
    pub kind: String,
    pub parent_session_id: Option<String>,
    pub workspace: Option<String>,
    pub archived: bool,
    pub created_at: String,
    pub updated_at: String,
    /// 侧栏手动排序键,越小越靠前(v28)。
    pub sort_key: i64,
}

#[derive(Debug, Clone)]
pub struct SessionOverview {
    pub record: SessionRecord,
    pub turn_count: i64,
    pub last_user_content: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PlatformSessionBindingKey {
    pub platform: String,
    pub account_id: String,
    pub conversation_kind: String,
    pub conversation_id: String,
    pub participant_id: Option<String>,
    pub persona: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlatformSessionBinding {
    pub key: PlatformSessionBindingKey,
    pub session_id: String,
}

impl PlatformSessionBindingKey {
    pub(crate) fn normalized_participant_id(&self) -> &str {
        self.participant_id.as_deref().unwrap_or("")
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PlatformPluginScopeKey {
    pub plugin_id: String,
    pub platform: String,
    pub account_id: String,
    pub conversation_kind: String,
    pub conversation_id: String,
}

/// Account scope shared by every account on one platform.
pub const GLOBAL_PLATFORM_ACCOUNT_SCOPE: &str = "*";

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PlatformAccessGrantKey {
    pub platform: String,
    pub account_scope: String,
    pub permission: String,
    pub subject_kind: String,
    pub subject_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlatformAccessActor {
    pub platform: String,
    pub account_id: String,
    pub user_id: String,
    pub conversation_kind: String,
    pub conversation_id: String,
    pub message_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlatformAccessGrant {
    pub key: PlatformAccessGrantKey,
    pub granted_by: PlatformAccessActor,
    pub created_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlatformMemeRefRecord {
    pub platform: String,
    pub account_id: String,
    pub conversation_kind: String,
    pub conversation_id: String,
    pub message_id: String,
    pub library: String,
    pub meme_id: String,
    pub direction: String,
    pub created_at: String,
}

/// One replayable turn: the prompt echo plus either its ordered transcript or,
/// for turns predating the transcript column, just the final reply.
#[derive(Clone, Debug, Default)]
pub struct TurnReplay {
    /// What the user saw as the prompt — or, for a wake turn, the
    /// `[后台任务完成] …` headline.
    pub display_content: String,
    pub assistant_content: String,
    pub entries: Vec<ReplayEntry>,
    /// daemon 自己合成的轮，不是任何人敲的：后台任务唤醒、目标续轮。
    /// 回放时画成一条居中提示，而不是用户气泡。
    pub is_synthetic: bool,
}
