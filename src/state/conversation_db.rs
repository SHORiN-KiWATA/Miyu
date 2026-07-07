use anyhow::{Context, Result};
use chrono::Utc;
use rusqlite::{params, Connection, OptionalExtension};
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

    pub fn load_turns(&self) -> Result<Vec<Turn>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT turn_id, seq, user_content, user_timestamp, assistant_content,
                    assistant_reasoning, assistant_timestamp, status, tool_reports, hidden, is_summary, owner_pid,
                    token_total, token_usage_estimated
             FROM turns ORDER BY seq ASC",
        )?;
        let turns = stmt
            .query_map([], map_turn_row)?
            .collect::<std::result::Result<Vec<_>, _>>()?;
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
        let turns = stmt
            .query_map(params![exclude_turn_id], map_turn_row)?
            .collect::<std::result::Result<Vec<_>, _>>()?;
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
        let turns = stmt
            .query_map([], map_turn_row)?
            .collect::<std::result::Result<Vec<_>, _>>()?;
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
        let turns = stmt
            .query_map(params![exclude_turn_id], map_turn_row)?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(turns)
    }

    pub fn hide_turns_before_seq(&self, seq: i64) -> Result<usize> {
        let conn = self.conn.lock().unwrap();
        let affected = conn.execute(
            "UPDATE turns SET hidden = 1 WHERE seq <= ?1",
            params![seq],
        )?;
        Ok(affected)
    }

    pub fn insert_summary_turn(&self, summary: &str) -> Result<()> {
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
        conn.execute(
            "INSERT INTO turns (turn_id, seq, user_content, user_timestamp, assistant_content, assistant_timestamp, status, tool_reports, hidden, is_summary)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'completed', '[]', 0, 1)",
            params![turn_id, seq, "[conversation summary]", now, summary, now],
        )?;
        Ok(())
    }

    pub fn load_last_summary(&self) -> Result<Option<Turn>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT turn_id, seq, user_content, user_timestamp, assistant_content,
                    assistant_reasoning, assistant_timestamp, status, tool_reports, hidden, is_summary, owner_pid,
                    token_total, token_usage_estimated
             FROM turns WHERE is_summary = 1 ORDER BY seq DESC LIMIT 1",
        )?;
        let turn = stmt
            .query_map([], map_turn_row)?
            .next()
            .transpose()?;
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
        let to_remove: Vec<Turn> = stmt
            .query_map(params![count as i64], map_turn_row)?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        drop(stmt);
        for turn in &to_remove {
            conn.execute(
                "DELETE FROM turns WHERE turn_id = ?1",
                params![turn.turn_id],
            )?;
        }
        Ok(to_remove)
    }

    pub fn trim_oldest_visible_turns(&self, count: usize) -> Result<Vec<Turn>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT turn_id, seq, user_content, user_timestamp, assistant_content,
                    assistant_reasoning, assistant_timestamp, status, tool_reports, hidden, is_summary, owner_pid,
                    token_total, token_usage_estimated
             FROM turns WHERE hidden = 0 AND is_summary = 0 ORDER BY seq ASC LIMIT ?1",
        )?;
        let to_remove: Vec<Turn> = stmt
            .query_map(params![count as i64], map_turn_row)?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        drop(stmt);
        for turn in &to_remove {
            conn.execute(
                "DELETE FROM turns WHERE turn_id = ?1",
                params![turn.turn_id],
            )?;
        }
        Ok(to_remove)
    }

    pub fn reset(&self) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute("DELETE FROM turns", [])?;
        Ok(())
    }

    pub fn token_total(&self) -> Result<u64> {
        let conn = self.conn.lock().unwrap();
        let total: i64 = conn.query_row(
            "SELECT COALESCE(SUM(token_total), 0) FROM turns WHERE status = 'completed' AND is_summary = 0",
            [],
            |row| row.get(0),
        )?;
        Ok(total.max(0) as u64)
    }

    pub fn undo_last_turn(&self) -> Result<(usize, Option<String>)> {
        let conn = self.conn.lock().unwrap();
        let last: Option<(String, String)> = conn
            .query_row(
                "SELECT turn_id, user_content FROM turns ORDER BY seq DESC LIMIT 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        match last {
            Some((turn_id, user_content)) => {
                conn.execute(
                    "DELETE FROM turns WHERE turn_id = ?1",
                    params![turn_id],
                )?;
                Ok((1, Some(user_content)))
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
        let mut stmt = conn.prepare(
            "SELECT user_content FROM turns WHERE status = 'running' ORDER BY seq ASC",
        )?;
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
        let mut stmt = conn.prepare(
            "SELECT turn_id, owner_pid FROM turns WHERE status = 'running'",
        )?;
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
                if alive { None } else { Some(turn_id) }
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
        let max_seq: i64 = conn
            .query_row("SELECT COALESCE(MAX(seq), 0) FROM turns", [], |row| {
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
            let content = entry
                .get("content")
                .and_then(|v| v.as_str())
                .unwrap_or("");
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
    let tool_reports: Vec<String> =
        serde_json::from_str(&tool_reports_json).unwrap_or_default();
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
        hidden: hidden != 0,
        is_summary: is_summary != 0,
        owner_pid: row.get(11)?,
        token_total: row.get::<_, i64>(12)?.max(0) as u64,
        token_usage_estimated: row.get::<_, i64>(13)? != 0,
    })
}

fn add_column_if_missing(conn: &Connection, table: &str, column: &str, definition: &str) -> Result<()> {
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
