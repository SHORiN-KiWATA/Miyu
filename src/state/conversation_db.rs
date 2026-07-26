use crate::i18n::text as t;
use crate::memory::EvictedTurn;
use crate::question::QuestionExchange;
use anyhow::{bail, Context, Result};
use chrono::Utc;
use rusqlite::{params, Connection, OptionalExtension, Transaction, TransactionBehavior};
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::sync::Mutex;

const PENDING_PLACEHOLDER: &str = "<system-reminder>上一轮prompt正在被另一个进程处理，你只需要回应用户当前的prompt，不要处理上一轮的prompt</system-reminder>";
const INTERRUPTED_TEXT: &str =
    "<system-reminder>上一轮prompt已被中断，除非用户重新要求否则不要处理上一轮的prompt</system-reminder>";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnStatus {
    Running,
    Completed,
    Interrupted,
}

#[allow(dead_code)]
impl TurnStatus {
    fn as_str(&self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Completed => "completed",
            Self::Interrupted => "interrupted",
        }
    }

    fn from_str(s: &str) -> Self {
        match s {
            "completed" => Self::Completed,
            "interrupted" => Self::Interrupted,
            _ => Self::Running,
        }
    }
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct Turn {
    pub turn_id: String,
    pub seq: i64,
    pub user_content: String,
    pub user_timestamp: String,
    pub assistant_content: String,
    pub assistant_reasoning: Option<String>,
    pub assistant_provider_id: Option<String>,
    pub assistant_model: Option<String>,
    pub assistant_timestamp: Option<String>,
    pub status: TurnStatus,
    pub tool_reports: Vec<String>,
    pub question_exchanges: Vec<QuestionExchange>,
    pub followups: Vec<TurnFollowup>,
    pub hidden: bool,
    pub is_summary: bool,
    pub owner_pid: Option<i64>,
    pub token_total: u64,
    pub token_usage_estimated: bool,
}

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
    pub submitted_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnFollowup {
    pub prompt_id: String,
    pub content: String,
    pub display_content: String,
    pub attachments: Vec<QueuedPromptAttachment>,
    pub submitted_at: String,
    pub preceding_assistant_content: Option<String>,
    pub preceding_assistant_reasoning: Option<String>,
    pub preceding_assistant_provider_id: Option<String>,
    pub preceding_assistant_model: Option<String>,
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

pub struct ConversationDb {
    conn: Mutex<Connection>,
}

impl std::fmt::Debug for ConversationDb {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConversationDb").finish_non_exhaustive()
    }
}

impl ConversationDb {
    pub fn open(state_dir: &Path) -> Result<Self> {
        std::fs::create_dir_all(state_dir)?;
        let db_path = state_dir.join("conversation.db");
        let conn = Connection::open(&db_path)
            .with_context(|| format!("failed to open conversation db: {}", db_path.display()))?;
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA synchronous = NORMAL;
             PRAGMA busy_timeout = 5000;
             PRAGMA foreign_keys = ON;",
        )?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS question_exchanges (
                turn_id         TEXT NOT NULL,
                exchange_index  INTEGER NOT NULL,
                payload         TEXT NOT NULL,
                PRIMARY KEY (turn_id, exchange_index),
                FOREIGN KEY (turn_id) REFERENCES turns(turn_id) ON DELETE CASCADE
            );
            CREATE INDEX IF NOT EXISTS idx_question_exchanges_turn
                ON question_exchanges(turn_id, exchange_index);",
        )?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS turns (
                turn_id          TEXT PRIMARY KEY,
                seq              INTEGER NOT NULL UNIQUE,
                user_content     TEXT NOT NULL,
                user_timestamp   TEXT NOT NULL,
                assistant_content TEXT NOT NULL,
                assistant_reasoning TEXT,
                assistant_timestamp TEXT,
                status           TEXT NOT NULL DEFAULT 'running',
                tool_reports     TEXT NOT NULL DEFAULT '[]'
            );
            CREATE INDEX IF NOT EXISTS idx_turns_seq ON turns(seq);
             CREATE INDEX IF NOT EXISTS idx_turns_status ON turns(status);",
        )?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS image_assets (
                asset_id    TEXT PRIMARY KEY,
                turn_id     TEXT NOT NULL,
                tool_id     TEXT,
                mime        TEXT NOT NULL,
                width       INTEGER NOT NULL DEFAULT 0,
                height      INTEGER NOT NULL DEFAULT 0,
                alt         TEXT NOT NULL DEFAULT '',
                data        BLOB NOT NULL,
                created_at  TEXT NOT NULL,
                FOREIGN KEY (turn_id) REFERENCES turns(turn_id) ON DELETE CASCADE
            );
            CREATE INDEX IF NOT EXISTS idx_image_assets_turn
                ON image_assets(turn_id, created_at, asset_id);",
        )?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS queued_prompts (
                seq                         INTEGER PRIMARY KEY AUTOINCREMENT,
                prompt_id                   TEXT NOT NULL UNIQUE,
                content                     TEXT NOT NULL,
                display_content             TEXT NOT NULL,
                attachments                 TEXT NOT NULL DEFAULT '[]',
                status                      TEXT NOT NULL DEFAULT 'queued',
                submitted_at                TEXT NOT NULL,
                queue_session_id             TEXT,
                owner_pid                    INTEGER,
                consumed_at                 TEXT,
                turn_id                     TEXT,
                context_content              TEXT,
                preceding_assistant_content  TEXT,
                preceding_assistant_reasoning TEXT,
                FOREIGN KEY (turn_id) REFERENCES turns(turn_id) ON DELETE CASCADE
            );
            CREATE INDEX IF NOT EXISTS idx_queued_prompts_status_seq
                ON queued_prompts(status, seq);
            CREATE INDEX IF NOT EXISTS idx_queued_prompts_turn_seq
                ON queued_prompts(turn_id, seq);",
        )?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS session_loaded_items (
                kind            TEXT NOT NULL,
                name            TEXT NOT NULL,
                source_turn_id  TEXT,
                created_at      TEXT NOT NULL,
                updated_at      TEXT NOT NULL,
                PRIMARY KEY (kind, name)
            );
            CREATE INDEX IF NOT EXISTS idx_session_loaded_items_source_turn
                ON session_loaded_items(source_turn_id);",
        )?;
        add_column_if_missing(&conn, "turns", "hidden", "INTEGER NOT NULL DEFAULT 0")?;
        add_column_if_missing(&conn, "turns", "is_summary", "INTEGER NOT NULL DEFAULT 0")?;
        add_column_if_missing(&conn, "turns", "owner_pid", "INTEGER")?;
        add_column_if_missing(&conn, "turns", "queue_session_id", "TEXT")?;
        add_column_if_missing(&conn, "turns", "token_total", "INTEGER NOT NULL DEFAULT 0")?;
        add_column_if_missing(
            &conn,
            "turns",
            "token_usage_estimated",
            "INTEGER NOT NULL DEFAULT 0",
        )?;
        add_column_if_missing(
            &conn,
            "turns",
            "compact_reversible",
            "INTEGER NOT NULL DEFAULT 0",
        )?;
        add_column_if_missing(&conn, "turns", "compact_parent_summary_seq", "INTEGER")?;
        add_column_if_missing(&conn, "turns", "assistant_provider_id", "TEXT")?;
        add_column_if_missing(&conn, "turns", "assistant_model", "TEXT")?;
        add_column_if_missing(&conn, "queued_prompts", "queue_session_id", "TEXT")?;
        add_column_if_missing(&conn, "queued_prompts", "owner_pid", "INTEGER")?;
        add_column_if_missing(
            &conn,
            "queued_prompts",
            "preceding_assistant_provider_id",
            "TEXT",
        )?;
        add_column_if_missing(&conn, "queued_prompts", "preceding_assistant_model", "TEXT")?;
        conn.execute_batch(
            "CREATE INDEX IF NOT EXISTS idx_turns_visible_seq ON turns(hidden, seq);
             CREATE INDEX IF NOT EXISTS idx_turns_visible_summary_seq
                 ON turns(is_summary, hidden, seq);
             CREATE INDEX IF NOT EXISTS idx_queued_prompts_session_status_seq
                 ON queued_prompts(queue_session_id, status, seq);",
        )?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    pub fn start_turn(
        &self,
        turn_id: &str,
        user_content: &str,
        owner_pid: u32,
        queue_session_id: &str,
    ) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        let seq = self.next_seq_locked(&conn)?;
        let now = Utc::now().to_rfc3339();
        conn.execute(
            "INSERT INTO turns (turn_id, seq, user_content, user_timestamp, assistant_content, status, owner_pid, queue_session_id)
             VALUES (?1, ?2, ?3, ?4, ?5, 'running', ?6, ?7)",
            params![
                turn_id,
                seq,
                user_content,
                now,
                PENDING_PLACEHOLDER,
                owner_pid as i64,
                queue_session_id
            ],
        )?;
        Ok(())
    }

    #[allow(dead_code)]
    pub fn complete_turn(
        &self,
        turn_id: &str,
        content: &str,
        reasoning: Option<&str>,
    ) -> Result<()> {
        self.complete_turn_with_usage(turn_id, content, reasoning, None, None, None, false)
    }

    pub fn complete_turn_with_usage(
        &self,
        turn_id: &str,
        content: &str,
        reasoning: Option<&str>,
        provider_id: Option<&str>,
        model: Option<&str>,
        token_total: Option<u64>,
        token_usage_estimated: bool,
    ) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        let now = Utc::now().to_rfc3339();
        let token_total = token_total.unwrap_or(0) as i64;
        let token_usage_estimated = i64::from(token_usage_estimated);
        conn.execute(
            "UPDATE turns SET assistant_content = ?1, assistant_reasoning = ?2,
                    assistant_provider_id = ?3, assistant_model = ?4, assistant_timestamp = ?5,
                    status = 'completed', token_total = ?6, token_usage_estimated = ?7
             WHERE turn_id = ?8",
            params![
                content,
                reasoning,
                provider_id,
                model,
                now,
                token_total,
                token_usage_estimated,
                turn_id
            ],
        )?;
        Ok(())
    }

    pub fn interrupt_turn(&self, turn_id: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        let now = Utc::now().to_rfc3339();
        conn.execute(
            "UPDATE turns SET assistant_content = ?1, assistant_timestamp = ?2, status = 'interrupted'
             WHERE turn_id = ?3 AND status = 'running'",
            params![INTERRUPTED_TEXT, now, turn_id],
        )?;
        Ok(())
    }

    pub fn append_tool_report(&self, turn_id: &str, report: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        let existing: Option<String> = conn
            .query_row(
                "SELECT tool_reports FROM turns WHERE turn_id = ?1",
                params![turn_id],
                |row| row.get(0),
            )
            .optional()?;
        let mut reports: Vec<String> = existing
            .as_deref()
            .and_then(|s| serde_json::from_str(s).ok())
            .unwrap_or_default();
        reports.push(report.to_string());
        let encoded = serde_json::to_string(&reports)?;
        conn.execute(
            "UPDATE turns SET tool_reports = ?1 WHERE turn_id = ?2",
            params![encoded, turn_id],
        )?;
        Ok(())
    }

    pub fn insert_image_asset(&self, asset: &ImageAsset, data: &[u8]) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO image_assets
                (asset_id, turn_id, tool_id, mime, width, height, alt, data, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                asset.asset_id,
                asset.turn_id,
                asset.tool_id,
                asset.mime,
                i64::from(asset.width),
                i64::from(asset.height),
                asset.alt,
                data,
                asset.created_at,
            ],
        )?;
        Ok(())
    }

    pub fn load_image_assets(&self) -> Result<Vec<ImageAsset>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT asset_id, turn_id, tool_id, mime, width, height, alt, created_at
             FROM image_assets ORDER BY turn_id ASC, created_at ASC, asset_id ASC",
        )?;
        let assets = stmt
            .query_map([], map_image_asset_row)?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(assets)
    }

    pub fn load_image_asset(&self, asset_id: &str) -> Result<Option<ImageAssetData>> {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT asset_id, turn_id, tool_id, mime, width, height, alt, created_at, data
             FROM image_assets WHERE asset_id = ?1",
            params![asset_id],
            |row| {
                Ok(ImageAssetData {
                    asset: map_image_asset_row(row)?,
                    bytes: row.get(8)?,
                })
            },
        )
        .optional()
        .map_err(Into::into)
    }

    pub fn append_question_exchange(
        &self,
        turn_id: &str,
        exchange: &QuestionExchange,
    ) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        let next_index: i64 = conn.query_row(
            "SELECT COALESCE(MAX(exchange_index), -1) + 1
             FROM question_exchanges WHERE turn_id = ?1",
            params![turn_id],
            |row| row.get(0),
        )?;
        conn.execute(
            "INSERT INTO question_exchanges (turn_id, exchange_index, payload)
             VALUES (?1, ?2, ?3)",
            params![turn_id, next_index, serde_json::to_string(exchange)?],
        )?;
        Ok(())
    }

    pub fn enqueue_prompt(
        &self,
        prompt_id: &str,
        content: &str,
        display_content: &str,
        attachments: &[QueuedPromptAttachment],
        queue_session_id: &str,
        owner_pid: u32,
    ) -> Result<QueuedPrompt> {
        let conn = self.conn.lock().unwrap();
        let submitted_at = Utc::now().to_rfc3339();
        let attachments_json = serde_json::to_string(attachments)?;
        conn.execute(
            "INSERT INTO queued_prompts
                (prompt_id, content, display_content, attachments, status, submitted_at,
                 queue_session_id, owner_pid)
             VALUES (?1, ?2, ?3, ?4, 'queued', ?5, ?6, ?7)",
            params![
                prompt_id,
                content,
                display_content,
                attachments_json,
                submitted_at,
                queue_session_id,
                owner_pid as i64
            ],
        )?;
        let seq = conn.last_insert_rowid();
        Ok(QueuedPrompt {
            prompt_id: prompt_id.to_string(),
            seq,
            content: content.to_string(),
            display_content: display_content.to_string(),
            attachments: attachments.to_vec(),
            submitted_at,
        })
    }

    pub fn load_queued_prompts(&self, queue_session_id: &str) -> Result<Vec<QueuedPrompt>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT prompt_id, seq, content, display_content, attachments, submitted_at
             FROM queued_prompts
             WHERE status = 'queued' AND queue_session_id = ?1
             ORDER BY seq ASC",
        )?;
        let rows = stmt
            .query_map(params![queue_session_id], |row| {
                let attachments_json: String = row.get(4)?;
                let attachments = serde_json::from_str(&attachments_json).unwrap_or_default();
                Ok(QueuedPrompt {
                    prompt_id: row.get(0)?,
                    seq: row.get(1)?,
                    content: row.get(2)?,
                    display_content: row.get(3)?,
                    attachments,
                    submitted_at: row.get(5)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn consume_queued_prompts(
        &self,
        turn_id: &str,
        prompts: &[(String, String)],
        preceding_assistant_content: Option<&str>,
        preceding_assistant_reasoning: Option<&str>,
        preceding_assistant_provider_id: Option<&str>,
        preceding_assistant_model: Option<&str>,
        queue_session_id: &str,
    ) -> Result<()> {
        if prompts.is_empty() {
            return Ok(());
        }
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let running: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM turns WHERE turn_id = ?1 AND status = 'running')",
            params![turn_id],
            |row| row.get(0),
        )?;
        if !running {
            bail!("cannot consume queued prompts into a non-running turn");
        }
        let consumed_at = Utc::now().to_rfc3339();
        for (index, (prompt_id, context_content)) in prompts.iter().enumerate() {
            let preceding_content = (index == 0)
                .then_some(preceding_assistant_content)
                .flatten();
            let preceding_reasoning = (index == 0)
                .then_some(preceding_assistant_reasoning)
                .flatten();
            let affected = tx.execute(
                "UPDATE queued_prompts
                  SET status = 'consumed', consumed_at = ?1, turn_id = ?2,
                      context_content = ?3, preceding_assistant_content = ?4,
                      preceding_assistant_reasoning = ?5,
                      preceding_assistant_provider_id = ?6,
                      preceding_assistant_model = ?7
                   WHERE prompt_id = ?8 AND status = 'queued' AND queue_session_id = ?9",
                params![
                    consumed_at,
                    turn_id,
                    context_content,
                    preceding_content,
                    preceding_reasoning,
                    preceding_assistant_provider_id,
                    preceding_assistant_model,
                    prompt_id,
                    queue_session_id
                ],
            )?;
            if affected != 1 {
                bail!("queued prompt changed before it could be consumed: {prompt_id}");
            }
        }
        tx.commit()?;
        Ok(())
    }

    pub fn discard_queued_prompts(&self, queue_session_id: &str) -> Result<usize> {
        let conn = self.conn.lock().unwrap();
        Ok(conn.execute(
            "DELETE FROM queued_prompts
             WHERE status = 'queued' AND queue_session_id = ?1",
            params![queue_session_id],
        )?)
    }

    pub fn remove_queued_prompt(&self, prompt_id: &str, queue_session_id: &str) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        Ok(conn.execute(
            "DELETE FROM queued_prompts
             WHERE prompt_id = ?1 AND status = 'queued' AND queue_session_id = ?2",
            params![prompt_id, queue_session_id],
        )? == 1)
    }

    pub fn discard_stale_queued_prompts(
        &self,
        current_session_id: &str,
        current_pid: u32,
    ) -> Result<usize> {
        let mut conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT prompt_id, queue_session_id, owner_pid
             FROM queued_prompts WHERE status = 'queued'",
        )?;
        let queued_prompts = stmt
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, Option<i64>>(2)?,
                ))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        let stale_prompt_ids = queued_prompts
            .into_iter()
            .filter_map(|row| {
                let (prompt_id, session_id, owner_pid) = row;
                if session_id.as_deref() == Some(current_session_id) {
                    return None;
                }
                let owner_pid = owner_pid.and_then(|pid| u32::try_from(pid).ok());
                let stale = session_id.is_none()
                    || owner_pid == Some(current_pid)
                    || !owner_pid.is_some_and(crate::alarm::process_exists);
                stale.then_some(prompt_id)
            })
            .collect::<Vec<_>>();
        drop(stmt);
        if stale_prompt_ids.is_empty() {
            return Ok(0);
        }
        let tx = conn.transaction()?;
        let mut discarded = 0usize;
        for prompt_id in stale_prompt_ids {
            discarded += tx.execute(
                "DELETE FROM queued_prompts WHERE prompt_id = ?1 AND status = 'queued'",
                params![prompt_id],
            )?;
        }
        tx.commit()?;
        Ok(discarded)
    }

    pub fn load_session_loaded_items(
        &self,
        kind: &str,
    ) -> Result<std::collections::BTreeSet<String>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn
            .prepare("SELECT name FROM session_loaded_items WHERE kind = ?1 ORDER BY name ASC")?;
        let items = stmt
            .query_map(params![kind], |row| row.get::<_, String>(0))?
            .collect::<std::result::Result<std::collections::BTreeSet<_>, _>>()?;
        Ok(items)
    }

    pub fn load_session_loaded_items_with_sources(
        &self,
        kind: &str,
    ) -> Result<Vec<(String, Option<String>)>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT name, source_turn_id FROM session_loaded_items WHERE kind = ?1 ORDER BY name ASC",
        )?;
        let items = stmt
            .query_map(params![kind], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(items)
    }

    pub fn add_session_loaded_items(
        &self,
        kind: &str,
        names: &[String],
        source_turn_id: Option<&str>,
    ) -> Result<usize> {
        let conn = self.conn.lock().unwrap();
        let now = Utc::now().to_rfc3339();
        let mut affected = 0usize;
        for name in names
            .iter()
            .map(|name| name.trim())
            .filter(|name| !name.is_empty())
        {
            affected += conn.execute(
                "INSERT INTO session_loaded_items (kind, name, source_turn_id, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?4)
                 ON CONFLICT(kind, name) DO UPDATE SET
                    source_turn_id = COALESCE(excluded.source_turn_id, session_loaded_items.source_turn_id),
                    updated_at = excluded.updated_at",
                params![kind, name, source_turn_id, now],
            )?;
        }
        Ok(affected)
    }

    pub fn load_turns(&self) -> Result<Vec<Turn>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT turn_id, seq, user_content, user_timestamp, assistant_content,
                    assistant_reasoning, assistant_provider_id, assistant_model, assistant_timestamp, status, tool_reports, hidden, is_summary, owner_pid,
                    token_total, token_usage_estimated
             FROM turns ORDER BY seq ASC",
        )?;
        let mut turns = stmt
            .query_map([], map_turn_row)?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        attach_turn_children_locked(&conn, &mut turns)?;
        Ok(turns)
    }

    #[allow(dead_code)]
    pub fn load_turns_excluding(&self, exclude_turn_id: &str) -> Result<Vec<Turn>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT turn_id, seq, user_content, user_timestamp, assistant_content,
                    assistant_reasoning, assistant_provider_id, assistant_model, assistant_timestamp, status, tool_reports, hidden, is_summary, owner_pid,
                    token_total, token_usage_estimated
             FROM turns WHERE turn_id != ?1 ORDER BY seq ASC",
        )?;
        let mut turns = stmt
            .query_map(params![exclude_turn_id], map_turn_row)?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        attach_turn_children_locked(&conn, &mut turns)?;
        Ok(turns)
    }

    #[allow(dead_code)]
    pub fn load_turns_for_context(&self) -> Result<Vec<Turn>> {
        self.load_turns()
    }

    pub fn load_visible_turns(&self) -> Result<Vec<Turn>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT turn_id, seq, user_content, user_timestamp, assistant_content,
                    assistant_reasoning, assistant_provider_id, assistant_model, assistant_timestamp, status, tool_reports, hidden, is_summary, owner_pid,
                    token_total, token_usage_estimated
             FROM turns WHERE hidden = 0 ORDER BY seq ASC",
        )?;
        let mut turns = stmt
            .query_map([], map_turn_row)?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        attach_turn_children_locked(&conn, &mut turns)?;
        Ok(turns)
    }

    pub fn load_visible_turns_excluding(&self, exclude_turn_id: &str) -> Result<Vec<Turn>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT turn_id, seq, user_content, user_timestamp, assistant_content,
                    assistant_reasoning, assistant_provider_id, assistant_model, assistant_timestamp, status, tool_reports, hidden, is_summary, owner_pid,
                    token_total, token_usage_estimated
             FROM turns WHERE hidden = 0 AND turn_id != ?1 ORDER BY seq ASC",
        )?;
        let mut turns = stmt
            .query_map(params![exclude_turn_id], map_turn_row)?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        attach_turn_children_locked(&conn, &mut turns)?;
        Ok(turns)
    }

    #[allow(dead_code)]
    pub fn hide_turns_before_seq(&self, seq: i64) -> Result<usize> {
        let conn = self.conn.lock().unwrap();
        let affected = conn.execute("UPDATE turns SET hidden = 1 WHERE seq <= ?1", params![seq])?;
        Ok(affected)
    }

    #[allow(dead_code)]
    pub fn insert_summary_turn(
        &self,
        summary: &str,
        token_total: Option<u64>,
        token_usage_estimated: bool,
    ) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        let turn_id = format!(
            "summary_{}_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis())
                .unwrap_or(0),
            rand::random::<u16>()
        );
        let seq = self.next_seq_locked(&conn)?;
        let now = Utc::now().to_rfc3339();
        let token_total = token_total.unwrap_or(0) as i64;
        let token_usage_estimated = i64::from(token_usage_estimated);
        conn.execute(
            "INSERT INTO turns (turn_id, seq, user_content, user_timestamp, assistant_content, assistant_timestamp, status, tool_reports, hidden, is_summary, token_total, token_usage_estimated)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'completed', '[]', 0, 1, ?7, ?8)",
            params![turn_id, seq, "[conversation summary]", now, summary, now, token_total, token_usage_estimated],
        )?;
        Ok(())
    }

    pub fn load_last_summary(&self) -> Result<Option<Turn>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT turn_id, seq, user_content, user_timestamp, assistant_content,
                    assistant_reasoning, assistant_provider_id, assistant_model, assistant_timestamp, status, tool_reports, hidden, is_summary, owner_pid,
                    token_total, token_usage_estimated
             FROM turns WHERE is_summary = 1 AND hidden = 0 ORDER BY seq DESC LIMIT 1",
        )?;
        let turn = stmt.query_map([], map_turn_row)?.next().transpose()?;
        Ok(turn)
    }

    #[allow(dead_code)]
    pub fn count_turns(&self) -> Result<i64> {
        let conn = self.conn.lock().unwrap();
        let count: i64 = conn.query_row("SELECT COUNT(*) FROM turns", [], |row| row.get(0))?;
        Ok(count)
    }

    #[allow(dead_code)]
    pub fn total_chars(&self) -> Result<usize> {
        let turns = self.load_turns()?;
        Ok(turns.iter().map(|t| turn_chars(t)).sum())
    }

    #[allow(dead_code)]
    pub fn trim_oldest_turns(&self, count: usize) -> Result<Vec<Turn>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT turn_id, seq, user_content, user_timestamp, assistant_content,
                    assistant_reasoning, assistant_provider_id, assistant_model, assistant_timestamp, status, tool_reports, hidden, is_summary, owner_pid,
                    token_total, token_usage_estimated
             FROM turns WHERE is_summary = 0 ORDER BY seq ASC LIMIT ?1",
        )?;
        let mut to_remove: Vec<Turn> = stmt
            .query_map(params![count as i64], map_turn_row)?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        drop(stmt);
        attach_turn_children_locked(&conn, &mut to_remove)?;
        for turn in &to_remove {
            conn.execute(
                "DELETE FROM turns WHERE turn_id = ?1",
                params![turn.turn_id],
            )?;
        }
        Ok(to_remove)
    }

    pub fn oldest_evictable_visible_turns(&self, count: usize) -> Result<Vec<Turn>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT turn_id, seq, user_content, user_timestamp, assistant_content,
                    assistant_reasoning, assistant_provider_id, assistant_model, assistant_timestamp, status, tool_reports, hidden, is_summary, owner_pid,
                    token_total, token_usage_estimated
             FROM turns
             WHERE hidden = 0 AND is_summary = 0 AND status != 'running'
             ORDER BY seq ASC LIMIT ?1",
        )?;
        let count = i64::try_from(count).unwrap_or(i64::MAX);
        let mut turns = stmt
            .query_map(params![count], map_turn_row)?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        attach_turn_children_locked(&conn, &mut turns)?;
        Ok(turns)
    }

    pub fn delete_visible_turns(&self, turn_ids: &[String]) -> Result<usize> {
        self.delete_visible_turns_checked(turn_ids, None)
    }

    pub fn delete_visible_turns_checked(
        &self,
        turn_ids: &[String],
        expected_loaded_tools: Option<&[(String, Option<String>)]>,
    ) -> Result<usize> {
        if turn_ids.is_empty() {
            return Ok(0);
        }
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        verify_loaded_tool_sources(&tx, expected_loaded_tools)?;
        let affected = delete_visible_turns_in_transaction(&tx, turn_ids)?;
        tx.commit()?;
        Ok(affected)
    }

    pub fn archive_and_delete_visible_turns(
        &self,
        archive_db: &Path,
        turns: &[EvictedTurn],
        turn_ids: &[String],
        expected_loaded_tools: Option<&[(String, Option<String>)]>,
    ) -> Result<usize> {
        if turn_ids.is_empty() {
            return Ok(0);
        }
        let mut conn = self.conn.lock().unwrap();
        let archive_db = archive_db.to_string_lossy().into_owned();
        let archive_alias = format!("evicted_context_{}", rand::random::<u32>());
        conn.execute(
            &format!("ATTACH DATABASE ?1 AS {archive_alias}"),
            params![archive_db],
        )?;
        let insert_sql = format!(
            "INSERT OR IGNORE INTO {archive_alias}.evicted_turns
             (source_id, timestamp, role, content, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5)"
        );
        let operation = (|| -> Result<usize> {
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            verify_loaded_tool_sources(&tx, expected_loaded_tools)?;
            let created_at = Utc::now().to_rfc3339();
            for turn in turns {
                tx.execute(
                    &insert_sql,
                    params![
                        turn.source_id,
                        turn.timestamp,
                        turn.role,
                        turn.content,
                        created_at
                    ],
                )?;
            }
            let affected = delete_visible_turns_in_transaction(&tx, turn_ids)?;
            tx.commit()?;
            Ok(affected)
        })();
        let detach = conn.execute_batch(&format!("DETACH DATABASE {archive_alias}"));
        if let Err(detach_err) = detach {
            tracing::warn!(
                error = %detach_err,
                archive_alias,
                "failed to detach evicted-context database"
            );
        }
        operation
    }

    pub fn replace_visible_with_summary(
        &self,
        last_seq: i64,
        visible_turn_ids: &[String],
        summary: &str,
        token_total: Option<u64>,
        token_usage_estimated: bool,
    ) -> Result<()> {
        if summary.trim().is_empty() {
            bail!("compact returned an empty summary");
        }

        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        let current_turn_ids = {
            let mut stmt = tx.prepare(
                "SELECT turn_id FROM turns
                 WHERE hidden = 0 ORDER BY seq ASC",
            )?;
            let turn_ids = stmt
                .query_map([], |row| row.get::<_, String>(0))?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            turn_ids
        };
        if current_turn_ids != visible_turn_ids {
            bail!("conversation changed while compact was running");
        }
        let parent_summary_seq: Option<i64> = tx.query_row(
            "SELECT MAX(seq) FROM turns
                 WHERE hidden = 0 AND is_summary = 1 AND seq <= ?1",
            params![last_seq],
            |row| row.get(0),
        )?;
        let hidden = tx.execute(
            "UPDATE turns SET hidden = 1 WHERE hidden = 0 AND seq <= ?1",
            params![last_seq],
        )?;
        if hidden == 0 {
            bail!("conversation changed before compact could be saved");
        }

        let turn_id = format!(
            "summary_{}_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis())
                .unwrap_or(0),
            rand::random::<u16>()
        );
        let seq: i64 = tx.query_row("SELECT COALESCE(MAX(seq), 0) + 1 FROM turns", [], |row| {
            row.get(0)
        })?;
        let now = Utc::now().to_rfc3339();
        let token_total = token_total.unwrap_or(0) as i64;
        let token_usage_estimated = i64::from(token_usage_estimated);
        tx.execute(
            "INSERT INTO turns (turn_id, seq, user_content, user_timestamp, assistant_content, assistant_timestamp, status, tool_reports, hidden, is_summary, token_total, token_usage_estimated, compact_reversible, compact_parent_summary_seq)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'completed', '[]', 0, 1, ?7, ?8, 1, ?9)",
            params![turn_id, seq, "[conversation summary]", now, summary, now, token_total, token_usage_estimated, parent_summary_seq],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn reset(&self) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute("DELETE FROM queued_prompts", [])?;
        conn.execute("DELETE FROM turns", [])?;
        conn.execute("DELETE FROM session_loaded_items", [])?;
        Ok(())
    }

    pub fn reset_history(&self) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute("DELETE FROM turns", [])?;
        conn.execute("DELETE FROM session_loaded_items", [])?;
        Ok(())
    }

    pub fn undo_last_turn(&self) -> Result<(usize, Option<String>)> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        let running: i64 = tx.query_row(
            "SELECT COUNT(*) FROM turns WHERE hidden = 0 AND status = 'running'",
            [],
            |row| row.get(0),
        )?;
        if running > 0 {
            tx.rollback()?;
            return Ok((0, None));
        }
        let last: Option<(String, i64, String, bool, bool, Option<i64>)> = tx
            .query_row(
                "SELECT turn_id, seq, user_content, is_summary,
                        compact_reversible, compact_parent_summary_seq
                 FROM turns WHERE hidden = 0 ORDER BY seq DESC LIMIT 1",
                [],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get::<_, i64>(3)? != 0,
                        row.get::<_, i64>(4)? != 0,
                        row.get(5)?,
                    ))
                },
            )
            .optional()?;
        match last {
            Some((turn_id, _, user_content, false, _, _)) => {
                tx.execute("DELETE FROM turns WHERE turn_id = ?1", params![turn_id])?;
                tx.commit()?;
                Ok((1, Some(user_content)))
            }
            Some((_, _, _, true, false, _)) => {
                tx.rollback()?;
                Ok((0, None))
            }
            Some((turn_id, summary_seq, _, true, true, parent_summary_seq)) => {
                let restorable: i64 = match parent_summary_seq {
                    Some(previous_seq) => tx.query_row(
                        "SELECT COUNT(*) FROM turns
                         WHERE hidden = 1 AND seq < ?1
                           AND (seq = ?2 OR (is_summary = 0 AND seq > ?2))",
                        params![summary_seq, previous_seq],
                        |row| row.get(0),
                    )?,
                    None => tx.query_row(
                        "SELECT COUNT(*) FROM turns
                         WHERE hidden = 1 AND is_summary = 0 AND seq < ?1",
                        params![summary_seq],
                        |row| row.get(0),
                    )?,
                };
                if restorable == 0 {
                    tx.rollback()?;
                    return Ok((0, None));
                }

                tx.execute("DELETE FROM turns WHERE turn_id = ?1", params![turn_id])?;
                match parent_summary_seq {
                    Some(previous_seq) => {
                        tx.execute(
                            "UPDATE turns SET hidden = 0
                             WHERE hidden = 1 AND seq < ?1
                               AND (seq = ?2 OR (is_summary = 0 AND seq > ?2))",
                            params![summary_seq, previous_seq],
                        )?;
                    }
                    None => {
                        tx.execute(
                            "UPDATE turns SET hidden = 0
                             WHERE hidden = 1 AND is_summary = 0 AND seq < ?1",
                            params![summary_seq],
                        )?;
                    }
                }
                tx.commit()?;
                Ok((1, None))
            }
            None => Ok((0, None)),
        }
    }

    #[allow(dead_code)]
    pub fn has_running_turns(&self) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM turns WHERE status = 'running'",
            [],
            |row| row.get(0),
        )?;
        Ok(count > 0)
    }

    pub fn running_turn_queue_target(
        &self,
    ) -> Result<Option<(String, Option<String>, Option<u32>)>> {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT turns.turn_id,
                    COALESCE(
                        turns.queue_session_id,
                        (SELECT queued_prompts.queue_session_id
                           FROM queued_prompts
                          WHERE queued_prompts.owner_pid = turns.owner_pid
                            AND queued_prompts.queue_session_id IS NOT NULL
                          ORDER BY queued_prompts.seq DESC
                          LIMIT 1)
                    ),
                    turns.owner_pid
               FROM turns
              WHERE turns.status = 'running'
              ORDER BY turns.seq DESC
              LIMIT 1",
            [],
            |row| {
                let owner_pid = row
                    .get::<_, Option<i64>>(2)?
                    .and_then(|pid| u32::try_from(pid).ok());
                Ok((row.get(0)?, row.get(1)?, owner_pid))
            },
        )
        .optional()
        .map_err(Into::into)
    }

    #[allow(dead_code)]
    pub fn running_turn_summaries(&self) -> Result<Vec<String>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn
            .prepare("SELECT user_content FROM turns WHERE status = 'running' ORDER BY seq ASC")?;
        let summaries = stmt
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(summaries)
    }

    pub fn running_turn_summaries_excluding(&self, exclude_turn_id: &str) -> Result<Vec<String>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT user_content FROM turns WHERE status = 'running' AND turn_id != ?1 ORDER BY seq ASC",
        )?;
        let summaries = stmt
            .query_map(params![exclude_turn_id], |row| row.get::<_, String>(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(summaries)
    }

    pub fn recover_stale_running_turns(&self) -> Result<usize> {
        let mut conn = self.conn.lock().unwrap();
        let mut stmt =
            conn.prepare("SELECT turn_id, owner_pid FROM turns WHERE status = 'running'")?;
        let stale_turn_ids: Vec<String> = stmt
            .query_map([], |row| {
                let turn_id: String = row.get(0)?;
                let owner_pid: Option<i64> = row.get(1)?;
                Ok((turn_id, owner_pid))
            })?
            .filter_map(|row| {
                let (turn_id, owner_pid) = row.ok()?;
                let alive = owner_pid
                    .map(|pid| crate::alarm::process_exists(pid as u32))
                    .unwrap_or(false);
                if alive {
                    None
                } else {
                    Some(turn_id)
                }
            })
            .collect();
        drop(stmt);
        if stale_turn_ids.is_empty() {
            return Ok(0);
        }
        let tx = conn.transaction()?;
        let now = Utc::now().to_rfc3339();
        let mut affected = 0usize;
        for turn_id in &stale_turn_ids {
            let turn_affected = tx.execute(
                "UPDATE turns SET assistant_content = ?1, assistant_timestamp = ?2, status = 'interrupted'
                 WHERE turn_id = ?3 AND status = 'running'",
                params![INTERRUPTED_TEXT, now, turn_id],
            )?;
            if turn_affected == 1 {
                affected += 1;
            }
        }
        tx.commit()?;
        Ok(affected)
    }

    fn next_seq_locked(&self, conn: &Connection) -> Result<i64> {
        let max_seq: i64 =
            conn.query_row("SELECT COALESCE(MAX(seq), 0) FROM turns", [], |row| {
                row.get(0)
            })?;
        Ok(max_seq + 1)
    }

    #[allow(dead_code)]
    pub fn migrate_from_jsonl(&self, jsonl_path: &Path) -> Result<usize> {
        if !jsonl_path.exists() {
            return Ok(0);
        }
        let turns = self.load_turns()?;
        if !turns.is_empty() {
            return Ok(0);
        }
        let file = std::fs::File::open(jsonl_path)?;
        use std::io::{BufRead, BufReader};
        let mut migrated = 0usize;
        let mut pending_user: Option<(String, String)> = None;
        for line in BufReader::new(file).lines() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            let entry: serde_json::Value = match serde_json::from_str(&line) {
                Ok(v) => v,
                Err(_) => continue,
            };
            let role = entry.get("role").and_then(|v| v.as_str()).unwrap_or("");
            let content = entry.get("content").and_then(|v| v.as_str()).unwrap_or("");
            let timestamp = entry
                .get("timestamp")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let reasoning = entry
                .get("reasoning")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            if role == "user" {
                if let Some((prev_ts, prev_content)) = pending_user.take() {
                    let turn_id = format!("migrated_{}", migrated);
                    let conn = self.conn.lock().unwrap();
                    let seq = self.next_seq_locked(&conn)?;
                    conn.execute(
                        "INSERT INTO turns (turn_id, seq, user_content, user_timestamp, assistant_content, status)
                         VALUES (?1, ?2, ?3, ?4, ?5, 'completed')",
                        params![turn_id, seq, prev_content, prev_ts, "(migrated without reply)"],
                    )?;
                    drop(conn);
                    migrated += 1;
                }
                pending_user = Some((timestamp, content.to_string()));
            } else if role == "assistant" {
                if let Some((user_ts, user_content)) = pending_user.take() {
                    let turn_id = format!("migrated_{}", migrated);
                    let conn = self.conn.lock().unwrap();
                    let seq = self.next_seq_locked(&conn)?;
                    let now = Utc::now().to_rfc3339();
                    conn.execute(
                        "INSERT INTO turns (turn_id, seq, user_content, user_timestamp,
                         assistant_content, assistant_reasoning, assistant_timestamp, status)
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'completed')",
                        params![turn_id, seq, user_content, user_ts, content, reasoning, now],
                    )?;
                    drop(conn);
                    migrated += 1;
                }
            }
        }
        if let Some((user_ts, user_content)) = pending_user {
            let turn_id = format!("migrated_{}", migrated);
            let conn = self.conn.lock().unwrap();
            let seq = self.next_seq_locked(&conn)?;
            conn.execute(
                "INSERT INTO turns (turn_id, seq, user_content, user_timestamp, assistant_content, status)
                 VALUES (?1, ?2, ?3, ?4, ?5, 'interrupted')",
                params![
                    turn_id,
                    seq,
                    user_content,
                    user_ts,
                    "上一轮响应已中断，未完成。不要继续执行上一轮任务，除非用户重新要求。"
                ],
            )?;
            drop(conn);
            migrated += 1;
        }
        Ok(migrated)
    }
}

fn delete_visible_turns_in_transaction(tx: &Transaction<'_>, turn_ids: &[String]) -> Result<usize> {
    let mut affected = 0usize;
    for turn_id in turn_ids {
        let deleted = tx.execute(
            "DELETE FROM turns
             WHERE turn_id = ?1 AND hidden = 0 AND is_summary = 0 AND status != 'running'",
            params![turn_id],
        )?;
        if deleted != 1 {
            bail!(
                "{}",
                t(
                    "conversation changed before popped turns could be deleted",
                    "删除弹出轮次前会话已发生变化"
                )
            );
        }
        tx.execute(
            "DELETE FROM session_loaded_items WHERE source_turn_id = ?1",
            params![turn_id],
        )?;
        affected += deleted;
    }
    Ok(affected)
}

fn verify_loaded_tool_sources(
    tx: &Transaction<'_>,
    expected: Option<&[(String, Option<String>)]>,
) -> Result<()> {
    let Some(expected) = expected else {
        return Ok(());
    };
    let current = {
        let mut stmt = tx.prepare(
            "SELECT name, source_turn_id FROM session_loaded_items
             WHERE kind = 'tool' ORDER BY name ASC",
        )?;
        let rows = stmt
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<std::result::Result<Vec<(String, Option<String>)>, _>>()?;
        rows
    };
    if current != expected {
        bail!(
            "{}",
            t(
                "dynamic tool state changed while popping context",
                "弹出上下文时动态工具状态已发生变化"
            )
        );
    }
    Ok(())
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

#[allow(dead_code)]
pub fn pending_placeholder() -> &'static str {
    PENDING_PLACEHOLDER
}

#[allow(dead_code)]
pub fn interrupted_text() -> &'static str {
    INTERRUPTED_TEXT
}

fn map_turn_row(row: &rusqlite::Row) -> rusqlite::Result<Turn> {
    let tool_reports_json: String = row.get(10)?;
    let tool_reports: Vec<String> = serde_json::from_str(&tool_reports_json).unwrap_or_default();
    Ok(Turn {
        turn_id: row.get(0)?,
        seq: row.get(1)?,
        user_content: row.get(2)?,
        user_timestamp: row.get(3)?,
        assistant_content: row.get(4)?,
        assistant_reasoning: row.get(5)?,
        assistant_provider_id: row.get(6)?,
        assistant_model: row.get(7)?,
        assistant_timestamp: row.get(8)?,
        status: TurnStatus::from_str(row.get::<_, String>(9)?.as_str()),
        tool_reports,
        question_exchanges: Vec::new(),
        followups: Vec::new(),
        hidden: row.get::<_, i64>(11)? != 0,
        is_summary: row.get::<_, i64>(12)? != 0,
        owner_pid: row.get(13)?,
        token_total: row.get::<_, i64>(14)?.max(0) as u64,
        token_usage_estimated: row.get::<_, i64>(15)? != 0,
    })
}

fn map_image_asset_row(row: &rusqlite::Row) -> rusqlite::Result<ImageAsset> {
    Ok(ImageAsset {
        asset_id: row.get(0)?,
        turn_id: row.get(1)?,
        tool_id: row.get(2)?,
        mime: row.get(3)?,
        width: row.get::<_, i64>(4)?.max(0) as u32,
        height: row.get::<_, i64>(5)?.max(0) as u32,
        alt: row.get(6)?,
        created_at: row.get(7)?,
    })
}

fn attach_turn_children_locked(conn: &Connection, turns: &mut [Turn]) -> Result<()> {
    attach_question_exchanges_locked(conn, turns)?;
    attach_followups_locked(conn, turns)
}

fn attach_question_exchanges_locked(conn: &Connection, turns: &mut [Turn]) -> Result<()> {
    if turns.is_empty() {
        return Ok(());
    }
    let indexes = turns
        .iter()
        .enumerate()
        .map(|(index, turn)| (turn.turn_id.clone(), index))
        .collect::<std::collections::HashMap<_, _>>();
    let turn_ids = indexes.keys().collect::<Vec<_>>();
    for chunk in turn_ids.chunks(900) {
        let placeholders = std::iter::repeat_n("?", chunk.len())
            .collect::<Vec<_>>()
            .join(", ");
        let sql = format!(
            "SELECT turn_id, payload FROM question_exchanges
             WHERE turn_id IN ({placeholders}) ORDER BY turn_id, exchange_index"
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(rusqlite::params_from_iter(chunk.iter()), |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        for row in rows {
            let (turn_id, payload) = row?;
            let Some(index) = indexes.get(&turn_id).copied() else {
                continue;
            };
            let exchange = serde_json::from_str::<QuestionExchange>(&payload)
                .with_context(|| format!("invalid question exchange for turn {turn_id}"))?;
            turns[index].question_exchanges.push(exchange);
        }
    }
    Ok(())
}

fn attach_followups_locked(conn: &Connection, turns: &mut [Turn]) -> Result<()> {
    if turns.is_empty() {
        return Ok(());
    }
    let indexes = turns
        .iter()
        .enumerate()
        .map(|(index, turn)| (turn.turn_id.clone(), index))
        .collect::<std::collections::HashMap<_, _>>();
    let turn_ids = indexes.keys().collect::<Vec<_>>();
    for chunk in turn_ids.chunks(900) {
        let placeholders = std::iter::repeat_n("?", chunk.len())
            .collect::<Vec<_>>()
            .join(", ");
        let sql = format!(
            "SELECT prompt_id, turn_id, COALESCE(context_content, content), display_content,
                    attachments, submitted_at, preceding_assistant_content,
                    preceding_assistant_reasoning, preceding_assistant_provider_id,
                    preceding_assistant_model
             FROM queued_prompts
             WHERE status = 'consumed' AND turn_id IN ({placeholders})
             ORDER BY seq ASC"
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(rusqlite::params_from_iter(chunk.iter()), |row| {
            Ok((
                row.get::<_, String>(1)?,
                TurnFollowup {
                    prompt_id: row.get(0)?,
                    content: row.get(2)?,
                    display_content: row.get(3)?,
                    attachments: serde_json::from_str(&row.get::<_, String>(4)?)
                        .unwrap_or_default(),
                    submitted_at: row.get(5)?,
                    preceding_assistant_content: row.get(6)?,
                    preceding_assistant_reasoning: row.get(7)?,
                    preceding_assistant_provider_id: row.get(8)?,
                    preceding_assistant_model: row.get(9)?,
                },
            ))
        })?;
        for row in rows {
            let (turn_id, followup) = row?;
            let Some(index) = indexes.get(&turn_id).copied() else {
                continue;
            };
            turns[index].followups.push(followup);
        }
    }
    Ok(())
}

fn add_column_if_missing(
    conn: &Connection,
    table: &str,
    column: &str,
    definition: &str,
) -> Result<()> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let rows = stmt.query_map([], |row| row.get::<_, String>(1))?;
    for row in rows {
        if row? == column {
            return Ok(());
        }
    }
    conn.execute(
        &format!("ALTER TABLE {table} ADD COLUMN {column} {definition}"),
        [],
    )?;
    Ok(())
}
