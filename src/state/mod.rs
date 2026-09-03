mod assets;
mod history;
mod shared_files;
pub use conversation_db::SharedFile;
mod conversation_db;
mod migrations;
mod queue;
mod sessions;
mod turns;
mod usage_ops;
pub use migrations::DEFAULT_SESSION_ID;
pub(crate) mod usage;

/// Newest `conversation.db` schema this build can open — the gate an import
/// checks before restoring a database written by a newer Miyu.
pub fn latest_schema_version() -> i64 {
    migrations::LATEST_VERSION
}

use crate::llm::{TurnTokens, Usage};
use crate::memory_types::EvictedTurn;
use crate::paths::MiyuPaths;
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeSet, HashMap, HashSet};
use std::io::{Cursor, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, RwLock, Weak};

#[allow(unused_imports)]
pub use conversation_db::{
    interrupted_text, pending_placeholder, ArtifactAsset, ArtifactAssetData, ConversationDb,
    GoalDenied, GoalPhase, GoalRecord, ImageAsset, ImageAssetData, PlatformAccessActor,
    PlatformAccessGrant, PlatformAccessGrantKey, PlatformMemeRefRecord, PlatformPluginScopeKey,
    PlatformSessionBinding, PlatformSessionBindingKey, PruneStats, QueuedPrompt,
    QueuedPromptAttachment, RedoCandidate, RedoInputKind, RedoStart, ReplayEntry, SessionOverview,
    SessionRecord, ToolFlowCall, ToolFlowRound, ToolFootprint, Turn, TurnFollowup, TurnInlineMedia,
    TurnJournalEvent, TurnRedoCheckpointPayload, TurnReplay, TurnStatus, UserAttachment,
    UserAttachmentData, DEFAULT_MAX_GOAL_ROUNDS, GLOBAL_PLATFORM_ACCOUNT_SCOPE,
    INLINE_MEDIA_KIND_IMAGE, INLINE_MEDIA_KIND_TEXT, INLINE_MEDIA_KIND_VIDEO,
    USER_ATTACHMENT_KIND_FILE, USER_ATTACHMENT_KIND_IMAGE, USER_ATTACHMENT_KIND_TEXT,
};
pub use usage::{
    UsageMeta, UsageRange, UsageSnapshot, UsageStats, USAGE_KIND_AFFECTION, USAGE_KIND_GROUP_JOIN,
    USAGE_KIND_JUDGE,
};

/// The only session kind users can list, name, switch to, or bind a platform
/// to. Everything else is infrastructure and stays out of the session list.
pub const USER_SESSION_KIND: &str = "user";
/// Build/Dev 模式的保留人格 scope:dev 会话全部挂在它名下,借现有
/// 按人格隔离机制白拿会话/记忆/REPL 指针的分家;模式由会话的
/// persona==DEV_PERSONA 推导,无需迁移。
pub const DEV_PERSONA: &str = "dev";
/// Backs a one-shot `miyu ask` / `miyu '<message>'` turn: created just before
/// the turn, deleted right after, and invisible to every listing in between.
pub const ASK_SESSION_KIND: &str = "ask";

type PlatformAccessSubjects = HashSet<String>;
type PlatformAccessKinds = HashMap<String, PlatformAccessSubjects>;
type PlatformAccessPermissions = HashMap<String, PlatformAccessKinds>;
type PlatformAccessScopes = HashMap<String, PlatformAccessPermissions>;

#[derive(Debug)]
struct SharedPlatformAccess {
    index: RwLock<PlatformAccessIndex>,
    mutations: Mutex<()>,
}

static PLATFORM_ACCESS_INDEXES: OnceLock<Mutex<HashMap<PathBuf, Weak<SharedPlatformAccess>>>> =
    OnceLock::new();

#[derive(Debug, Default)]
struct PlatformAccessIndex {
    platforms: HashMap<String, PlatformAccessScopes>,
}

impl PlatformAccessIndex {
    fn from_grants(grants: impl IntoIterator<Item = PlatformAccessGrant>) -> Self {
        let mut index = Self::default();
        for grant in grants {
            index.insert(&grant.key);
        }
        index
    }

    fn contains(
        &self,
        platform: &str,
        account_scope: &str,
        permission: &str,
        subject_kind: &str,
        subject_id: &str,
    ) -> bool {
        self.platforms
            .get(platform)
            .and_then(|scopes| scopes.get(account_scope))
            .and_then(|permissions| permissions.get(permission))
            .and_then(|kinds| kinds.get(subject_kind))
            .is_some_and(|subjects| subjects.contains(subject_id))
    }

    fn insert(&mut self, key: &PlatformAccessGrantKey) {
        self.platforms
            .entry(key.platform.clone())
            .or_default()
            .entry(key.account_scope.clone())
            .or_default()
            .entry(key.permission.clone())
            .or_default()
            .entry(key.subject_kind.clone())
            .or_default()
            .insert(key.subject_id.clone());
    }

    fn remove(&mut self, key: &PlatformAccessGrantKey) -> bool {
        if let Some(subjects) = self
            .platforms
            .get_mut(&key.platform)
            .and_then(|scopes| scopes.get_mut(&key.account_scope))
            .and_then(|permissions| permissions.get_mut(&key.permission))
            .and_then(|kinds| kinds.get_mut(&key.subject_kind))
        {
            return subjects.remove(&key.subject_id);
        }
        false
    }
}

fn shared_platform_access_index(
    state_dir: &Path,
    conv_db: &ConversationDb,
) -> Result<Arc<SharedPlatformAccess>> {
    let key = state_dir
        .canonicalize()
        .unwrap_or_else(|_| state_dir.to_path_buf());
    let indexes = PLATFORM_ACCESS_INDEXES.get_or_init(|| Mutex::new(HashMap::new()));
    let mut indexes = indexes.lock().unwrap();
    if let Some(index) = indexes.get(&key).and_then(Weak::upgrade) {
        return Ok(index);
    }
    indexes.retain(|_, index| index.strong_count() > 0);
    let index = Arc::new(SharedPlatformAccess {
        index: RwLock::new(PlatformAccessIndex::from_grants(
            conv_db.platform_access_grants(None)?,
        )),
        mutations: Mutex::new(()),
    });
    indexes.insert(key, Arc::downgrade(&index));
    Ok(index)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunningTurnQueueTarget {
    pub turn_id: String,
    pub queue_session_id: Option<String>,
    pub owner_pid: Option<u32>,
}

#[derive(Clone, Debug)]
pub(crate) struct PlatformAccessAuthorization {
    pub(crate) statically_authorized: bool,
    pub(crate) dynamic_key: PlatformAccessGrantKey,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PlatformAccessMutation {
    Grant,
    Revoke,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PlatformAccessMutationResult {
    Unauthorized,
    Unchanged,
    Changed,
}

#[derive(Debug, Clone)]
pub struct StateStore {
    state_dir: PathBuf,
    artifacts_dir: PathBuf,
    shared_files_dir: PathBuf,
    conv_db: Arc<ConversationDb>,
    platform_access: Arc<SharedPlatformAccess>,
    /// Active session. Shared across clones and swappable at runtime so a
    /// long-lived daemon switches every holder atomically.
    session_id: Arc<std::sync::RwLock<Arc<str>>>,
    queue_session_id: Arc<str>,
    queue_owner_pid: u32,
}

impl StateStore {
    pub fn new(paths: &MiyuPaths) -> Result<Self> {
        let state_dir = paths.state_dir.clone();
        let conv_db = Arc::new(ConversationDb::open(&state_dir)?);
        let platform_access = shared_platform_access_index(&state_dir, &conv_db)?;
        let session_id = Arc::new(std::sync::RwLock::new(Arc::<str>::from(
            conv_db.resolve_current_session()?,
        )));
        let queue_owner_pid = std::process::id();
        let queue_session_id: Arc<str> = format!(
            "queue_{}_{}_{}",
            queue_owner_pid,
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|duration| duration.as_nanos())
                .unwrap_or(0),
            rand::random::<u64>()
        )
        .into();
        conv_db.discard_stale_queued_prompts(&queue_session_id, queue_owner_pid)?;
        Ok(Self {
            state_dir,
            artifacts_dir: paths.data_dir.join("artifacts"),
            shared_files_dir: paths.data_dir.join("shared"),
            conv_db,
            platform_access,
            session_id,
            queue_session_id,
            queue_owner_pid,
        })
    }

    pub fn session_id(&self) -> Arc<str> {
        self.session_id.read().unwrap().clone()
    }

    pub(crate) fn state_dir(&self) -> &Path {
        &self.state_dir
    }

    pub fn init_files(&self) -> Result<()> {
        std::fs::create_dir_all(&self.state_dir)?;
        if !self.usage_file().exists() {
            std::fs::write(self.usage_file(), "{\n  \"requests\": 0,\n  \"prompt_tokens\": 0,\n  \"completion_tokens\": 0,\n  \"total_tokens\": 0,\n  \"conversation_tokens\": 0\n}\n")?;
        }
        if !self.profile_file().exists() {
            std::fs::write(self.profile_file(), "# Miyu Profile\n\n")?;
        }
        Ok(())
    }

    #[allow(dead_code)]
    pub fn conv_db(&self) -> &ConversationDb {
        &self.conv_db
    }

    #[allow(dead_code)]
    pub fn migrate_from_jsonl(&self) -> Result<usize> {
        let jsonl_path = self.conversation_file();
        self.conv_db
            .migrate_from_jsonl(&self.session(), &jsonl_path)
    }

    fn conversation_file(&self) -> PathBuf {
        self.state_dir.join("conversation.jsonl")
    }

    fn usage_file(&self) -> PathBuf {
        self.state_dir.join("usage.json")
    }

    fn profile_file(&self) -> PathBuf {
        self.state_dir.join("profile.md")
    }

    fn prompt_fingerprint_file(&self) -> PathBuf {
        let key = blake3::hash(self.session().as_bytes()).to_hex();
        self.state_dir
            .join("prompt-fingerprints")
            .join(format!("{key}.sha256"))
    }
}

fn artifact_media_type(path: &Path) -> (&'static str, &'static str) {
    match path
        .extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase()
        .as_str()
    {
        "md" | "markdown" => ("text/markdown; charset=utf-8", "markdown"),
        "html" | "htm" => ("text/html; charset=utf-8", "html"),
        "pdf" => ("application/pdf", "pdf"),
        "json" | "jsonl" => ("application/json; charset=utf-8", "json"),
        "txt" | "log" | "csv" | "tsv" => ("text/plain; charset=utf-8", "text"),
        "css" => ("text/css; charset=utf-8", "code"),
        "js" | "mjs" | "cjs" => ("text/javascript; charset=utf-8", "code"),
        "xml" => ("application/xml; charset=utf-8", "code"),
        "rs" | "jsx" | "ts" | "tsx" | "py" | "go" | "java" | "c" | "cc" | "cpp" | "h" | "hpp"
        | "cs" | "rb" | "php" | "swift" | "kt" | "kts" | "sh" | "bash" | "zsh" | "fish"
        | "toml" | "yaml" | "yml" | "scss" | "sql" => ("text/plain; charset=utf-8", "code"),
        _ => ("application/octet-stream", "file"),
    }
}

fn prompt_fingerprint(system_prompt: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(system_prompt.as_bytes());
    format!("{:x}", hasher.finalize())
}

#[allow(dead_code)]
fn turn_chars(turn: &Turn) -> usize {
    turn.user_content.chars().count()
        + turn.assistant_content.chars().count()
        + turn
            .assistant_reasoning
            .as_deref()
            .map(str::chars)
            .map(Iterator::count)
            .unwrap_or(0)
        + turn
            .tool_reports
            .iter()
            .map(|r| r.chars().count())
            .sum::<usize>()
        + turn
            .question_exchanges
            .iter()
            .filter_map(|exchange| serde_json::to_string(exchange).ok())
            .map(|exchange| exchange.chars().count())
            .sum::<usize>()
        + turn
            .followups
            .iter()
            .map(|followup| {
                followup.content.chars().count()
                    + followup
                        .preceding_assistant_content
                        .as_deref()
                        .map(str::chars)
                        .map(Iterator::count)
                        .unwrap_or(0)
                    + followup
                        .preceding_assistant_reasoning
                        .as_deref()
                        .map(str::chars)
                        .map(Iterator::count)
                        .unwrap_or(0)
            })
            .sum::<usize>()
}

fn turns_to_entries(turns: Vec<Turn>) -> Vec<StoredConversationEntry> {
    let mut entries = Vec::with_capacity(turns.len() * 3);
    for turn in turns {
        let ts = turn.assistant_timestamp.clone().unwrap_or_default();
        entries.push(StoredConversationEntry {
            timestamp: turn.user_timestamp,
            role: "user".to_string(),
            content: turn.user_content,
            reasoning: None,
        });
        for exchange in &turn.question_exchanges {
            entries.push(StoredConversationEntry {
                timestamp: exchange.answered_at.clone(),
                role: "assistant_clarification".to_string(),
                content: crate::question::assistant_exchange_text(exchange),
                reasoning: None,
            });
            entries.push(StoredConversationEntry {
                timestamp: exchange.answered_at.clone(),
                role: "user_clarification".to_string(),
                content: crate::question::user_exchange_text(exchange),
                reasoning: None,
            });
        }
        for followup in turn.followups {
            if followup
                .preceding_assistant_content
                .as_deref()
                .is_some_and(|content| !content.trim().is_empty())
                || followup
                    .preceding_assistant_reasoning
                    .as_deref()
                    .is_some_and(|reasoning| !reasoning.trim().is_empty())
            {
                entries.push(StoredConversationEntry {
                    timestamp: followup.submitted_at.clone(),
                    role: "assistant".to_string(),
                    content: followup.preceding_assistant_content.unwrap_or_default(),
                    reasoning: followup.preceding_assistant_reasoning,
                });
            }
            entries.push(StoredConversationEntry {
                timestamp: followup.submitted_at,
                role: "user".to_string(),
                content: followup.content,
                reasoning: None,
            });
        }
        entries.push(StoredConversationEntry {
            timestamp: ts.clone(),
            role: "assistant".to_string(),
            content: turn.assistant_content,
            reasoning: turn.assistant_reasoning,
        });
        for report in turn.tool_reports {
            entries.push(StoredConversationEntry {
                timestamp: ts.clone(),
                role: "assistant".to_string(),
                content: report,
                reasoning: None,
            });
        }
    }
    entries
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct StoredConversationEntry {
    pub timestamp: String,
    pub role: String,
    pub content: String,
    #[serde(default)]
    pub reasoning: Option<String>,
}

#[cfg(test)]
mod tests;
