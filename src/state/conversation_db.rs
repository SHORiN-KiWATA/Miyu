use crate::i18n::text as t;
use crate::memory::EvictedTurn;
use crate::question::QuestionExchange;
use anyhow::{bail, Context, Result};
use chrono::Utc;
use rusqlite::{params, Connection, OptionalExtension, Transaction, TransactionBehavior};
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
    pub assistant_timestamp: Option<String>,
    pub status: TurnStatus,
    pub tool_reports: Vec<String>,
    pub question_exchanges: Vec<QuestionExchange>,
    pub hidden: bool,
    pub is_summary: bool,
    pub owner_pid: Option<i64>,
    pub token_total: u64,
    pub token_usage_estimated: bool,
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
        conn.execute_batch(
            "CREATE INDEX IF NOT EXISTS idx_turns_visible_seq ON turns(hidden, seq);
             CREATE INDEX IF NOT EXISTS idx_turns_visible_summary_seq
                 ON turns(is_summary, hidden, seq);",
        )?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    pub fn start_turn(&self, turn_id: &str, user_content: &str, owner_pid: u32) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        let seq = self.next_seq_locked(&conn)?;
        let now = Utc::now().to_rfc3339();
        conn.execute(
            "INSERT INTO turns (turn_id, seq, user_content, user_timestamp, assistant_content, status, owner_pid)
             VALUES (?1, ?2, ?3, ?4, ?5, 'running', ?6)",
            params![turn_id, seq, user_content, now, PENDING_PLACEHOLDER, owner_pid as i64],
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
        self.complete_turn_with_usage(turn_id, content, reasoning, None, false)
    }

    pub fn complete_turn_with_usage(
        &self,
        turn_id: &str,
        content: &str,
        reasoning: Option<&str>,
        token_total: Option<u64>,
        token_usage_estimated: bool,
    ) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        let now = Utc::now().to_rfc3339();
        let token_total = token_total.unwrap_or(0) as i64;
        let token_usage_estimated = i64::from(token_usage_estimated);
        conn.execute(
            "UPDATE turns SET assistant_content = ?1, assistant_reasoning = ?2, assistant_timestamp = ?3,
                    status = 'completed', token_total = ?4, token_usage_estimated = ?5
             WHERE turn_id = ?6",
            params![content, reasoning, now, token_total, token_usage_estimated, turn_id],
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
                    assistant_reasoning, assistant_timestamp, status, tool_reports, hidden, is_summary, owner_pid,
                    token_total, token_usage_estimated
             FROM turns ORDER BY seq ASC",
        )?;
        let mut turns = stmt
            .query_map([], map_turn_row)?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        attach_question_exchanges_locked(&conn, &mut turns)?;
        Ok(turns)
    }

    #[allow(dead_code)]
    pub fn load_turns_excluding(&self, exclude_turn_id: &str) -> Result<Vec<Turn>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT turn_id, seq, user_content, user_timestamp, assistant_content,
                    assistant_reasoning, assistant_timestamp, status, tool_reports, hidden, is_summary, owner_pid,
                    token_total, token_usage_estimated
             FROM turns WHERE turn_id != ?1 ORDER BY seq ASC",
        )?;
        let mut turns = stmt
            .query_map(params![exclude_turn_id], map_turn_row)?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        attach_question_exchanges_locked(&conn, &mut turns)?;
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
                    assistant_reasoning, assistant_timestamp, status, tool_reports, hidden, is_summary, owner_pid,
                    token_total, token_usage_estimated
             FROM turns WHERE hidden = 0 ORDER BY seq ASC",
        )?;
        let mut turns = stmt
            .query_map([], map_turn_row)?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        attach_question_exchanges_locked(&conn, &mut turns)?;
        Ok(turns)
    }

    pub fn load_visible_turns_excluding(&self, exclude_turn_id: &str) -> Result<Vec<Turn>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT turn_id, seq, user_content, user_timestamp, assistant_content,
                    assistant_reasoning, assistant_timestamp, status, tool_reports, hidden, is_summary, owner_pid,
                    token_total, token_usage_estimated
             FROM turns WHERE hidden = 0 AND turn_id != ?1 ORDER BY seq ASC",
        )?;
        let mut turns = stmt
            .query_map(params![exclude_turn_id], map_turn_row)?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        attach_question_exchanges_locked(&conn, &mut turns)?;
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
                    assistant_reasoning, assistant_timestamp, status, tool_reports, hidden, is_summary, owner_pid,
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
                    assistant_reasoning, assistant_timestamp, status, tool_reports, hidden, is_summary, owner_pid,
                    token_total, token_usage_estimated
             FROM turns WHERE is_summary = 0 ORDER BY seq ASC LIMIT ?1",
        )?;
        let mut to_remove: Vec<Turn> = stmt
            .query_map(params![count as i64], map_turn_row)?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        drop(stmt);
        attach_question_exchanges_locked(&conn, &mut to_remove)?;
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
                    assistant_reasoning, assistant_timestamp, status, tool_reports, hidden, is_summary, owner_pid,
                    token_total, token_usage_estimated
             FROM turns
             WHERE hidden = 0 AND is_summary = 0 AND status != 'running'
             ORDER BY seq ASC LIMIT ?1",
        )?;
        let count = i64::try_from(count).unwrap_or(i64::MAX);
        let mut turns = stmt
            .query_map(params![count], map_turn_row)?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        attach_question_exchanges_locked(&conn, &mut turns)?;
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
        let conn = self.conn.lock().unwrap();
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
        let now = Utc::now().to_rfc3339();
        let mut affected = 0usize;
        for turn_id in &stale_turn_ids {
            affected += conn.execute(
                "UPDATE turns SET assistant_content = ?1, assistant_timestamp = ?2, status = 'interrupted'
                 WHERE turn_id = ?3 AND status = 'running'",
                params![INTERRUPTED_TEXT, now, turn_id],
            )?;
        }
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
    let tool_reports_json: String = row.get(8)?;
    let tool_reports: Vec<String> = serde_json::from_str(&tool_reports_json).unwrap_or_default();
    let hidden: i64 = row.get(9)?;
    let is_summary: i64 = row.get(10)?;
    Ok(Turn {
        turn_id: row.get(0)?,
        seq: row.get(1)?,
        user_content: row.get(2)?,
        user_timestamp: row.get(3)?,
        assistant_content: row.get(4)?,
        assistant_reasoning: row.get(5)?,
        assistant_timestamp: row.get(6)?,
        status: TurnStatus::from_str(row.get::<_, String>(7)?.as_str()),
        tool_reports,
        question_exchanges: Vec::new(),
        hidden: hidden != 0,
        is_summary: is_summary != 0,
        owner_pid: row.get(11)?,
        token_total: row.get::<_, i64>(12)?.max(0) as u64,
        token_usage_estimated: row.get::<_, i64>(13)? != 0,
    })
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
