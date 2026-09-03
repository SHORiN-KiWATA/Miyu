use crate::config::{AppConfig, KnowledgeBasePluginConfig, MemoryConfig};
use crate::paths::MiyuPaths;
// 只要主体身份这一个纯数据类型，不需要整个平台运行时。
use crate::platform_types::PlatformPrincipal;
use anyhow::{bail, Context, Result};
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use rusqlite::{params, Connection, OpenFlags, TransactionBehavior};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::PathBuf;
use std::sync::LazyLock;

mod association;
pub mod browse;
mod evicted;
mod recall;
mod schema;
mod search;
mod validate;
mod write;
pub(crate) use association::*;
use schema::*;
use search::*;
use validate::*;
mod organizer;

pub(crate) use organizer::{MemoryOrganizer, MemoryOrganizerHandle};

const SHORT_TERM: &str = "short_term";
const LONG_TERM: &str = "long_term";
const VISIBILITY_PUBLIC: &str = "public";
const VISIBILITY_PRINCIPAL: &str = "principal";
const VISIBILITY_PRIVILEGED: &str = "privileged";
static JIEBA: LazyLock<CompactJieba> = LazyLock::new(|| {
    CompactJieba::new().expect("the build-generated compact Jieba index must be valid")
});

#[derive(Clone)]
pub struct MemoryStore {
    config: MemoryConfig,
    kb_config: KnowledgeBasePluginConfig,
    /// Kept whole because the embedding call needs provider lookup and the
    /// knowledge base's timeout setting.
    app_config: AppConfig,
    writes_enabled: bool,
    access: MemoryAccess,
    writer_principal: Option<String>,
    writer_display_name: String,
    data_db: PathBuf,
    state_db: PathBuf,
    skills_dir: PathBuf,
}

/// Read authorization for one agent run. Storage remains persona-global; this
/// value only controls which rows may enter the model context or memory tools.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum MemoryAccess {
    Privileged,
    Principal(String),
}

impl MemoryAccess {
    pub(crate) fn principal(key: impl Into<String>) -> Self {
        Self::Principal(key.into())
    }

    fn principal_key(&self) -> Option<&str> {
        match self {
            Self::Privileged => None,
            Self::Principal(key) => Some(key),
        }
    }
}

#[derive(Clone, Debug)]
struct MemoryOwnership {
    visibility: &'static str,
    owner_principal: String,
    owner_display_name: String,
}

impl MemoryOwnership {
    fn public() -> Self {
        Self {
            visibility: VISIBILITY_PUBLIC,
            owner_principal: String::new(),
            owner_display_name: String::new(),
        }
    }

    fn privileged() -> Self {
        Self {
            visibility: VISIBILITY_PRIVILEGED,
            owner_principal: String::new(),
            owner_display_name: String::new(),
        }
    }

    fn principal(key: impl Into<String>, display_name: impl Into<String>) -> Self {
        let display_name = display_name.into();
        Self {
            visibility: VISIBILITY_PRINCIPAL,
            owner_principal: key.into(),
            owner_display_name: truncate_chars(&compact_line(&display_name), 128),
        }
    }
}

// EvictedTurn 下沉到 crate::memory_types：state 落库也要它，留在这里会和
// state 形成循环。原样再导出，`memory::EvictedTurn` 的写法不变。
pub use crate::memory_types::EvictedTurn;

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum MemoryKind {
    Fact,
    Diary,
}

#[derive(Debug, Clone, Default, Serialize)]
pub(crate) struct MemoryOrigin {
    pub(crate) kind: String,
    pub(crate) platform: String,
    pub(crate) account_id: String,
    pub(crate) conversation_kind: String,
    pub(crate) conversation_id: String,
    pub(crate) sender_id: String,
    pub(crate) sender_display_name: String,
    pub(crate) session_id: String,
    pub(crate) message_id: String,
}

impl MemoryOrigin {
    pub(crate) fn local(session_id: impl Into<String>) -> Self {
        Self {
            kind: "local".to_string(),
            session_id: session_id.into(),
            ..Self::default()
        }
    }

    fn principal_ownership(&self) -> Option<MemoryOwnership> {
        if self.kind != "platform"
            || self.platform.trim().is_empty()
            || self.account_id.trim().is_empty()
            || self.sender_id.trim().is_empty()
        {
            return None;
        }
        Some(MemoryOwnership::principal(
            PlatformPrincipal {
                platform: self.platform.clone(),
                account_id: self.account_id.clone(),
                user_id: self.sender_id.clone(),
            }
            .stable_key(),
            self.sender_display_name.trim(),
        ))
    }
}

#[derive(Debug, Clone)]
pub struct MemoryHit {
    pub id: i64,
    pub kind: MemoryKind,
    pub content: String,
    pub score: f32,
    pub timestamp: String,
    pub source: String,
    pub retention: Option<String>,
    visibility: String,
    owner_principal: String,
    owner_display_name: String,
    subjects: String,
    source_episode_ids: Vec<i64>,
    origin_session_id: String,
}

#[derive(Debug, Clone, Eq, Ord, PartialEq, PartialOrd, Deserialize, Serialize)]
pub(crate) struct MemorySubject {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) principal: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) name: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct ShortDiaryRecord {
    pub(crate) id: i64,
    pub(crate) created_at: String,
    pub(crate) user_message: String,
    pub(crate) assistant_message: String,
    pub(crate) force_long_term: bool,
    pub(crate) owner_principal: Option<String>,
    pub(crate) origin: MemoryOrigin,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct ExistingMemoryRecord {
    pub(crate) id: i64,
    pub(crate) kind: String,
    pub(crate) content: String,
    pub(crate) truth_status: String,
    pub(crate) visibility: String,
    pub(crate) owner_principal: String,
    pub(crate) owner_display_name: String,
}

#[derive(Debug, Clone)]
pub(crate) struct OrganizationBatch {
    pub(crate) database_id: String,
    pub(crate) generation: i64,
    pub(crate) diaries: Vec<ShortDiaryRecord>,
    pub(crate) existing: Vec<ExistingMemoryRecord>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct OrganizedOutput {
    #[serde(default)]
    pub(crate) knowledge: Vec<KnowledgeAction>,
    #[serde(default)]
    pub(crate) long_diaries: Vec<LongDiaryDraft>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct KnowledgeAction {
    pub(crate) operation: String,
    #[serde(default)]
    pub(crate) target_id: Option<i64>,
    pub(crate) memory_type: String,
    pub(crate) content: String,
    pub(crate) truth_status: String,
    pub(crate) importance: i64,
    pub(crate) confidence: f64,
    #[serde(default)]
    pub(crate) visibility: String,
    #[serde(default)]
    pub(crate) subjects: Vec<MemorySubject>,
    #[serde(default)]
    pub(crate) tags: Vec<String>,
    #[serde(default)]
    pub(crate) diary_ids: Vec<i64>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct LongDiaryDraft {
    pub(crate) content: String,
    pub(crate) importance: i64,
    pub(crate) confidence: f64,
    #[serde(default)]
    pub(crate) visibility: String,
    #[serde(default)]
    pub(crate) subjects: Vec<MemorySubject>,
    #[serde(default)]
    pub(crate) tags: Vec<String>,
    #[serde(default)]
    pub(crate) diary_ids: Vec<i64>,
}

impl MemoryStore {
    pub fn new(config: &AppConfig, paths: &MiyuPaths) -> Self {
        let data_dir = config.active_persona_memory_data_dir(paths).join("memory");
        let state_dir = config.active_persona_memory_state_dir(paths).join("memory");
        Self {
            config: config.memory_config().clone(),
            kb_config: config.plugins.knowledge_base.clone(),
            app_config: config.clone(),
            writes_enabled: true,
            access: MemoryAccess::Privileged,
            writer_principal: None,
            writer_display_name: String::new(),
            data_db: data_dir.join("memory.db"),
            state_db: state_dir.join("evicted_context.db"),
            skills_dir: config.active_persona_skills_dir(paths),
        }
    }

    pub(crate) fn set_writes_enabled(&mut self, enabled: bool) {
        self.writes_enabled = enabled;
    }

    pub(crate) fn set_request_context(
        &mut self,
        access: MemoryAccess,
        writer_principal: Option<String>,
        writer_display_name: impl Into<String>,
    ) {
        self.access = access;
        self.writer_principal = writer_principal.filter(|value| !value.trim().is_empty());
        self.writer_display_name = writer_display_name.into().trim().to_string();
    }

    pub(crate) fn request_context(&self) -> (MemoryAccess, Option<String>, String) {
        (
            self.access.clone(),
            self.writer_principal.clone(),
            self.writer_display_name.clone(),
        )
    }

    pub(crate) fn with_request_context(
        mut self,
        access: MemoryAccess,
        writer_principal: Option<String>,
        writer_display_name: impl Into<String>,
    ) -> Self {
        self.set_request_context(access, writer_principal, writer_display_name);
        self
    }

    fn automatic_ownership(&self, origin: &MemoryOrigin) -> MemoryOwnership {
        origin
            .principal_ownership()
            .unwrap_or_else(MemoryOwnership::privileged)
    }

    fn writer_ownership(&self) -> MemoryOwnership {
        self.writer_principal
            .as_ref()
            .map(|principal| {
                MemoryOwnership::principal(principal.clone(), self.writer_display_name.clone())
            })
            .unwrap_or_else(MemoryOwnership::privileged)
    }

    pub(crate) fn apply_evicted_ownership(&self, turns: &mut [EvictedTurn]) {
        let ownership = self.writer_ownership();
        for turn in turns {
            turn.visibility = ownership.visibility.to_string();
            turn.owner_principal.clone_from(&ownership.owner_principal);
            turn.owner_display_name
                .clone_from(&ownership.owner_display_name);
        }
    }

    fn manual_fact_ownership(&self) -> MemoryOwnership {
        match self.writer_principal.as_ref() {
            Some(principal) => {
                MemoryOwnership::principal(principal.clone(), self.writer_display_name.clone())
            }
            None => MemoryOwnership::privileged(),
        }
    }

    pub fn init(&self) -> Result<()> {
        if let Some(parent) = self.data_db.parent() {
            std::fs::create_dir_all(parent)?;
        }
        if let Some(parent) = self.state_db.parent() {
            std::fs::create_dir_all(parent)?;
        }
        init_data_db(&self.data_conn()?)?;
        init_state_db(&self.state_conn()?)?;
        self.decay_memories()?;
        Ok(())
    }

    pub(crate) fn identity(&self) -> Result<(String, i64)> {
        if !self.data_db.is_file() {
            self.init()?;
        }
        Ok(self.data_conn_existing()?.query_row(
            "SELECT database_id, generation FROM memory_meta WHERE id=1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?)
    }

    fn init_existing(&self) -> Result<()> {
        let conn = self.data_conn_existing()?;
        init_data_db(&conn)?;
        self.decay_memories_with_conn(&conn)
    }

    fn data_conn(&self) -> Result<Connection> {
        let conn = Connection::open(&self.data_db)?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;")?;
        Ok(conn)
    }

    fn data_conn_existing(&self) -> Result<Connection> {
        let conn = Connection::open_with_flags(
            &self.data_db,
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;")?;
        Ok(conn)
    }

    fn state_conn(&self) -> Result<Connection> {
        let conn = Connection::open(&self.state_db)?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;")?;
        Ok(conn)
    }
}

#[cfg(test)]
mod tests;
