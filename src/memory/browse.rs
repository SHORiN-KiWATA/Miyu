//! 记忆浏览(WebUI dashboard):分页列出、过滤、详情、修订历史、编辑、删除,
//! 以及零副作用的统计与逐出归档浏览。
//!
//! 只读查询走 `data_conn_existing`——库不存在就是"这个人格还没有记忆",
//! 不该因为有人打开面板就建一个空库出来;也不跑 `init()`,那会顺带做一次
//! 遗忘衰减,浏览不该改数据。

use super::*;
use rusqlite::OptionalExtension;

/// 可浏览的两张表。列名只在这里白名单化,外面传来的表名不拼 SQL。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BrowseTable {
    Facts,
    Episodes,
}

impl BrowseTable {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "facts" => Some(Self::Facts),
            "episodes" => Some(Self::Episodes),
            _ => None,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Facts => "facts",
            Self::Episodes => "episodes",
        }
    }

    /// 两张表各自的完整列清单;`row_to_json` 按同一顺序读。
    fn columns(self) -> &'static str {
        match self {
            Self::Facts => {
                "id, content, source, status, strength, recall_count, last_recalled_at,
                 created_at, updated_at, visibility, owner_display_name, subjects,
                 confidence, importance, tags, memory_type, truth_status, source_episode_ids,
                 '' AS retention, NULL AS expires_at, NULL AS consolidated_at,
                 0 AS promotion_pending, NULL AS promoted_at, '' AS origin_kind,
                 '' AS origin_platform, '' AS origin_conversation_kind,
                 '' AS origin_conversation_id, '' AS origin_sender_display_name,
                 '' AS user_message, '' AS assistant_message"
            }
            Self::Episodes => {
                "id, content, source, status, strength, recall_count, last_recalled_at,
                 created_at, updated_at, visibility, owner_display_name, subjects,
                 confidence, importance, tags, '' AS memory_type, '' AS truth_status,
                 source_episode_ids, COALESCE(retention, '') AS retention, expires_at,
                 consolidated_at, promotion_pending, promoted_at, origin_kind,
                 origin_platform, origin_conversation_kind, origin_conversation_id,
                 origin_sender_display_name, user_message, assistant_message"
            }
        }
    }
}

/// 经历的整理阶段过滤;事实表忽略。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EpisodeStage {
    Unconsolidated,
    Consolidated,
    PromotionPending,
    Promoted,
}

impl EpisodeStage {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "unconsolidated" => Some(Self::Unconsolidated),
            "consolidated" => Some(Self::Consolidated),
            "promotion_pending" => Some(Self::PromotionPending),
            "promoted" => Some(Self::Promoted),
            _ => None,
        }
    }

    fn clause(self) -> &'static str {
        match self {
            Self::Unconsolidated => "retention='short_term' AND consolidated_at IS NULL",
            Self::Consolidated => "consolidated_at IS NOT NULL",
            Self::PromotionPending => "promotion_pending=1 AND promoted_at IS NULL",
            Self::Promoted => "promoted_at IS NOT NULL",
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct BrowseQuery {
    pub text: String,
    pub status: String,
    pub memory_type: String,
    pub truth_status: String,
    pub visibility: String,
    pub retention: String,
    pub stage: Option<EpisodeStage>,
    pub origin_kind: String,
    pub tag: String,
    pub limit: usize,
    pub offset: usize,
}

pub struct BrowsePage {
    pub items: Vec<Value>,
    pub total: i64,
}

/// 编辑一条记忆:全部字段可选,只改给了的。内容变化时事实表写一条修订。
#[derive(Debug, Clone, Default)]
pub struct BrowsePatch {
    pub content: Option<String>,
    pub status: Option<String>,
    pub importance: Option<i64>,
    pub memory_type: Option<String>,
    pub truth_status: Option<String>,
    pub tags: Option<Vec<String>>,
}

#[derive(Debug, Clone, Default)]
pub struct EvictedQuery {
    pub text: String,
    pub role: String,
    pub start: String,
    pub end: String,
    pub limit: usize,
    pub offset: usize,
}

fn row_to_json(row: &rusqlite::Row<'_>) -> rusqlite::Result<Value> {
    let subjects: String = row.get(11)?;
    let tags: String = row.get(14)?;
    let source_episode_ids: String = row.get(17)?;
    let json_or =
        |text: &str, fallback: Value| serde_json::from_str::<Value>(text).unwrap_or(fallback);
    Ok(json!({
        "id": row.get::<_, i64>(0)?,
        "content": row.get::<_, String>(1)?,
        "source": row.get::<_, String>(2)?,
        "status": row.get::<_, String>(3)?,
        "strength": row.get::<_, f64>(4)?,
        "recall_count": row.get::<_, i64>(5)?,
        "last_recalled_at": row.get::<_, Option<String>>(6)?,
        "created_at": row.get::<_, String>(7)?,
        "updated_at": row.get::<_, String>(8)?,
        "visibility": row.get::<_, String>(9)?,
        "owner": row.get::<_, String>(10)?,
        "subjects": json_or(&subjects, Value::Array(Vec::new())),
        "confidence": row.get::<_, f64>(12)?,
        "importance": row.get::<_, i64>(13)?,
        "tags": json_or(&tags, Value::Array(Vec::new())),
        "memory_type": row.get::<_, String>(15)?,
        "truth_status": row.get::<_, String>(16)?,
        "source_episode_ids": json_or(&source_episode_ids, Value::Array(Vec::new())),
        "retention": row.get::<_, String>(18)?,
        "expires_at": row.get::<_, Option<String>>(19)?,
        "consolidated_at": row.get::<_, Option<String>>(20)?,
        "promotion_pending": row.get::<_, i64>(21)? != 0,
        "promoted_at": row.get::<_, Option<String>>(22)?,
        "origin_kind": row.get::<_, String>(23)?,
        "origin_platform": row.get::<_, String>(24)?,
        "origin_conversation_kind": row.get::<_, String>(25)?,
        "origin_conversation_id": row.get::<_, String>(26)?,
        "origin_sender_display_name": row.get::<_, String>(27)?,
        "user_message": row.get::<_, String>(28)?,
        "assistant_message": row.get::<_, String>(29)?,
    }))
}

fn push_eq(
    clauses: &mut Vec<String>,
    params: &mut Vec<Box<dyn rusqlite::ToSql>>,
    column: &str,
    value: &str,
) {
    let value = value.trim();
    if value.is_empty() || value == "all" {
        return;
    }
    clauses.push(format!("{column} = ?"));
    params.push(Box::new(value.to_string()));
}

impl MemoryStore {
    /// 状态库(逐出归档)只在存在时打开,不建库。
    fn state_conn_existing(&self) -> Result<Option<Connection>> {
        if !self.state_db.exists() {
            return Ok(None);
        }
        let conn = Connection::open_with_flags(
            &self.state_db,
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        Ok(Some(conn))
    }

    pub fn browse(&self, table: BrowseTable, query: &BrowseQuery) -> Result<BrowsePage> {
        if !self.data_db.exists() {
            return Ok(BrowsePage {
                items: Vec::new(),
                total: 0,
            });
        }
        let conn = self.data_conn_existing()?;
        let mut clauses: Vec<String> = Vec::new();
        let mut params: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
        let text = query.text.trim();
        if !text.is_empty() {
            clauses.push("content LIKE ? ESCAPE '\\'".to_string());
            params.push(Box::new(format!("%{}%", escape_like(text))));
        }
        push_eq(&mut clauses, &mut params, "status", &query.status);
        push_eq(&mut clauses, &mut params, "visibility", &query.visibility);
        if table == BrowseTable::Facts {
            push_eq(&mut clauses, &mut params, "memory_type", &query.memory_type);
            push_eq(
                &mut clauses,
                &mut params,
                "truth_status",
                &query.truth_status,
            );
        } else {
            push_eq(&mut clauses, &mut params, "retention", &query.retention);
            push_eq(&mut clauses, &mut params, "origin_kind", &query.origin_kind);
            if let Some(stage) = query.stage {
                clauses.push(stage.clause().to_string());
            }
        }
        let tag = query.tag.trim();
        if !tag.is_empty() {
            // tags 是 JSON 数组文本;按带引号的成员匹配,避免 "ai" 命中 "email"。
            clauses.push("tags LIKE ? ESCAPE '\\'".to_string());
            params.push(Box::new(format!("%\"{}\"%", escape_like(tag))));
        }
        let where_sql = if clauses.is_empty() {
            String::new()
        } else {
            format!(" WHERE {}", clauses.join(" AND "))
        };
        let param_refs: Vec<&dyn rusqlite::ToSql> = params.iter().map(|p| p.as_ref()).collect();
        let total: i64 = conn.query_row(
            &format!("SELECT COUNT(*) FROM {}{where_sql}", table.name()),
            param_refs.as_slice(),
            |row| row.get(0),
        )?;
        let limit = query.limit.clamp(1, 500) as i64;
        let mut stmt = conn.prepare(&format!(
            "SELECT {} FROM {}{where_sql}
             ORDER BY updated_at DESC, id DESC
             LIMIT {limit} OFFSET {}",
            table.columns(),
            table.name(),
            query.offset as i64
        ))?;
        let items = stmt
            .query_map(param_refs.as_slice(), row_to_json)?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(BrowsePage { items, total })
    }

    pub fn browse_item(&self, table: BrowseTable, id: i64) -> Result<Option<Value>> {
        if !self.data_db.exists() {
            return Ok(None);
        }
        let conn = self.data_conn_existing()?;
        let item = conn
            .query_row(
                &format!(
                    "SELECT {} FROM {} WHERE id = ?1",
                    table.columns(),
                    table.name()
                ),
                params![id],
                row_to_json,
            )
            .optional()?;
        Ok(item)
    }

    /// 一条事实的修订历史(整理器 update 时写入),新在前。
    pub fn browse_revisions(&self, memory_id: i64) -> Result<Vec<Value>> {
        if !self.data_db.exists() {
            return Ok(Vec::new());
        }
        let conn = self.data_conn_existing()?;
        let mut stmt = conn.prepare(
            "SELECT id, old_content, new_content, source_episode_ids, created_at
               FROM memory_revisions WHERE memory_id = ?1 ORDER BY id DESC",
        )?;
        let rows = stmt
            .query_map(params![memory_id], |row| {
                let ids: String = row.get(3)?;
                Ok(json!({
                    "id": row.get::<_, i64>(0)?,
                    "old_content": row.get::<_, String>(1)?,
                    "new_content": row.get::<_, String>(2)?,
                    "source_episode_ids": serde_json::from_str::<Value>(&ids).unwrap_or(Value::Array(Vec::new())),
                    "created_at": row.get::<_, String>(4)?,
                }))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// 按 id 批量取经历(事实抽屉里展示"来源经历")。
    pub fn browse_episodes_by_ids(&self, ids: &[i64]) -> Result<Vec<Value>> {
        if ids.is_empty() || !self.data_db.exists() {
            return Ok(Vec::new());
        }
        let conn = self.data_conn_existing()?;
        let ids: Vec<i64> = ids.iter().copied().take(50).collect();
        let placeholders = vec!["?"; ids.len()].join(",");
        let mut stmt = conn.prepare(&format!(
            "SELECT {} FROM episodes WHERE id IN ({placeholders}) ORDER BY id DESC",
            BrowseTable::Episodes.columns()
        ))?;
        let rows = stmt
            .query_map(rusqlite::params_from_iter(ids.iter()), row_to_json)?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn update_item(&self, table: BrowseTable, id: i64, patch: &BrowsePatch) -> Result<bool> {
        if !self.data_db.exists() {
            return Ok(false);
        }
        let mut conn = self.data_conn_existing()?;
        let tx = conn.transaction()?;
        let current: Option<String> = tx
            .query_row(
                &format!("SELECT content FROM {} WHERE id = ?1", table.name()),
                params![id],
                |row| row.get(0),
            )
            .optional()?;
        let Some(current) = current else {
            return Ok(false);
        };
        let timestamp = now();
        let mut sets: Vec<String> = Vec::new();
        let mut values: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
        if let Some(content) = patch.content.as_deref().map(str::trim) {
            if content.is_empty() {
                bail!("content must not be empty");
            }
            if content != current {
                if table == BrowseTable::Facts {
                    tx.execute(
                        "INSERT INTO memory_revisions (
                            memory_id, old_content, new_content, source_episode_ids, created_at
                         ) VALUES (?1, ?2, ?3, '[]', ?4)",
                        params![id, current, content, timestamp],
                    )?;
                }
                sets.push("content = ?".into());
                values.push(Box::new(content.to_string()));
            }
        }
        if let Some(status) = patch.status.as_deref().map(str::trim) {
            if !matches!(status, "active" | "forgotten") {
                bail!("status must be active or forgotten");
            }
            sets.push("status = ?".into());
            values.push(Box::new(status.to_string()));
            if status == "active" {
                // 手工救回一条被遗忘的记忆:强度回满,否则下次衰减又忘掉。
                sets.push("strength = 1.0".into());
            }
        }
        if let Some(importance) = patch.importance {
            if !(1..=5).contains(&importance) {
                bail!("importance must be 1..=5");
            }
            sets.push("importance = ?".into());
            values.push(Box::new(importance));
        }
        if let Some(tags) = patch.tags.as_ref() {
            let cleaned: Vec<String> = tags
                .iter()
                .map(|tag| tag.trim().to_string())
                .filter(|tag| !tag.is_empty())
                .take(32)
                .collect();
            sets.push("tags = ?".into());
            values.push(Box::new(serde_json::to_string(&cleaned)?));
        }
        if table == BrowseTable::Facts {
            if let Some(memory_type) = patch.memory_type.as_deref().map(str::trim) {
                if !matches!(
                    memory_type,
                    "fact" | "preference" | "relationship" | "task" | "self" | "other"
                ) {
                    bail!("invalid memory_type");
                }
                sets.push("memory_type = ?".into());
                values.push(Box::new(memory_type.to_string()));
            }
            if let Some(truth_status) = patch.truth_status.as_deref().map(str::trim) {
                if !matches!(
                    truth_status,
                    "accepted" | "reported" | "uncertain" | "fictional" | "rejected"
                ) {
                    bail!("invalid truth_status");
                }
                sets.push("truth_status = ?".into());
                values.push(Box::new(truth_status.to_string()));
            }
        }
        if sets.is_empty() {
            return Ok(true);
        }
        sets.push("updated_at = ?".into());
        values.push(Box::new(timestamp));
        values.push(Box::new(id));
        let value_refs: Vec<&dyn rusqlite::ToSql> = values.iter().map(|v| v.as_ref()).collect();
        tx.execute(
            &format!(
                "UPDATE {} SET {} WHERE id = ?",
                table.name(),
                sets.join(", ")
            ),
            value_refs.as_slice(),
        )?;
        tx.commit()?;
        Ok(true)
    }

    pub fn delete_item(&self, table: BrowseTable, id: i64) -> Result<bool> {
        if !self.data_db.exists() {
            return Ok(false);
        }
        let conn = self.data_conn_existing()?;
        let affected = conn.execute(
            &format!("DELETE FROM {} WHERE id = ?1", table.name()),
            rusqlite::params![id],
        )?;
        Ok(affected == 1)
    }

    /// 零副作用统计:库不存在全零,不建库、不衰减、不清理。
    pub fn stats_readonly(&self) -> Result<Value> {
        let mut stats = json!({
            "ok": true,
            "exists": self.data_db.exists(),
            "data_db": self.data_db.display().to_string(),
            "state_db": self.state_db.display().to_string(),
            "facts": 0, "facts_forgotten": 0,
            "episodes": 0, "episodes_forgotten": 0,
            "short_diaries": 0, "long_diaries": 0,
            "unconsolidated_diaries": 0, "promotion_pending": 0,
            "unprocessed_pending_events": 0, "revisions": 0,
            "evicted_turns": 0, "evicted_embeddings": 0,
            "evicted_first": Value::Null, "evicted_last": Value::Null,
            "half_life_days": self.config.forgetting_half_life_days,
            "min_strength": self.config.forgetting_min_strength,
        });
        if self.data_db.exists() {
            let data = self.data_conn_existing()?;
            stats["facts"] = json!(count_where(&data, "facts", "status='active'")?);
            stats["facts_forgotten"] = json!(count_where(&data, "facts", "status='forgotten'")?);
            stats["episodes"] = json!(count_where(&data, "episodes", "status='active'")?);
            stats["episodes_forgotten"] =
                json!(count_where(&data, "episodes", "status='forgotten'")?);
            stats["short_diaries"] =
                json!(count_where(&data, "episodes", "retention='short_term'")?);
            stats["long_diaries"] = json!(count_where(&data, "episodes", "retention='long_term'")?);
            stats["unconsolidated_diaries"] = json!(count_where(
                &data,
                "episodes",
                "retention='short_term' AND consolidated_at IS NULL"
            )?);
            stats["promotion_pending"] = json!(count_where(
                &data,
                "episodes",
                "promotion_pending=1 AND promoted_at IS NULL"
            )?);
            stats["unprocessed_pending_events"] = json!(count_where(
                &data,
                "pending_events",
                "processed_at IS NULL"
            )?);
            stats["revisions"] = json!(count_rows(&data, "memory_revisions")?);
        }
        if let Some(state) = self.state_conn_existing()? {
            stats["evicted_turns"] = json!(count_rows(&state, "evicted_turns")?);
            stats["evicted_embeddings"] = json!(count_rows(&state, "evicted_embeddings")?);
            let (first, last): (Option<String>, Option<String>) = state.query_row(
                "SELECT MIN(timestamp), MAX(timestamp) FROM evicted_turns",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )?;
            stats["evicted_first"] = json!(first);
            stats["evicted_last"] = json!(last);
        }
        Ok(stats)
    }

    /// 逐出归档:无关键词时按时间倒序分页;有关键词走 trigram 搜索(取前 50)。
    pub fn browse_evicted(&self, query: &EvictedQuery) -> Result<BrowsePage> {
        let Some(conn) = self.state_conn_existing()? else {
            return Ok(BrowsePage {
                items: Vec::new(),
                total: 0,
            });
        };
        let text = query.text.trim();
        let start = query.start.trim();
        let end = query.end.trim();
        if !text.is_empty() {
            let result = self.search_evicted_context_filtered(
                text,
                50,
                (!start.is_empty()).then_some(start),
                (!end.is_empty()).then_some(end),
            )?;
            let mut items = result["results"].as_array().cloned().unwrap_or_default();
            let role = query.role.trim();
            if !role.is_empty() && role != "all" {
                items.retain(|item| item["role"].as_str() == Some(role));
            }
            let total = items.len() as i64;
            return Ok(BrowsePage { items, total });
        }
        let mut clauses: Vec<String> = Vec::new();
        let mut params: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
        push_eq(&mut clauses, &mut params, "role", &query.role);
        if !start.is_empty() {
            clauses.push("timestamp >= ?".into());
            params.push(Box::new(start.to_string()));
        }
        if !end.is_empty() {
            clauses.push("timestamp <= ?".into());
            params.push(Box::new(end.to_string()));
        }
        let where_sql = if clauses.is_empty() {
            String::new()
        } else {
            format!(" WHERE {}", clauses.join(" AND "))
        };
        let param_refs: Vec<&dyn rusqlite::ToSql> = params.iter().map(|p| p.as_ref()).collect();
        let total: i64 = conn.query_row(
            &format!("SELECT COUNT(*) FROM evicted_turns{where_sql}"),
            param_refs.as_slice(),
            |row| row.get(0),
        )?;
        let limit = query.limit.clamp(1, 500) as i64;
        let mut stmt = conn.prepare(&format!(
            "SELECT t.id, t.timestamp, t.role, t.content, t.visibility, t.owner_display_name,
                    EXISTS(SELECT 1 FROM evicted_embeddings e WHERE e.id = t.id) AS embedded
               FROM evicted_turns t{where_sql}
              ORDER BY t.id DESC LIMIT {limit} OFFSET {}",
            query.offset as i64
        ))?;
        let items = stmt
            .query_map(param_refs.as_slice(), |row| {
                let content: String = row.get(3)?;
                Ok(json!({
                    "id": row.get::<_, i64>(0)?,
                    "timestamp": row.get::<_, String>(1)?,
                    "role": row.get::<_, String>(2)?,
                    "snippet": truncate_chars(&compact_line(&content), 400),
                    "visibility": row.get::<_, String>(4)?,
                    "owner_display_name": row.get::<_, String>(5)?,
                    "embedded": row.get::<_, i64>(6)? != 0,
                }))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(BrowsePage { items, total })
    }

    pub fn browse_evicted_item(&self, id: i64) -> Result<Option<Value>> {
        let Some(conn) = self.state_conn_existing()? else {
            return Ok(None);
        };
        let item = conn
            .query_row(
                "SELECT id, timestamp, role, content, visibility, owner_display_name
                   FROM evicted_turns WHERE id = ?1",
                params![id],
                |row| {
                    Ok(json!({
                        "id": row.get::<_, i64>(0)?,
                        "timestamp": row.get::<_, String>(1)?,
                        "role": row.get::<_, String>(2)?,
                        "content": row.get::<_, String>(3)?,
                        "visibility": row.get::<_, String>(4)?,
                        "owner_display_name": row.get::<_, String>(5)?,
                    }))
                },
            )
            .optional()?;
        Ok(item)
    }

    pub fn delete_evicted_item(&self, id: i64) -> Result<bool> {
        let Some(conn) = self.state_conn_existing()? else {
            return Ok(false);
        };
        let affected = conn.execute("DELETE FROM evicted_turns WHERE id = ?1", params![id])?;
        conn.execute("DELETE FROM evicted_embeddings WHERE id = ?1", params![id])?;
        Ok(affected == 1)
    }
}

fn escape_like(text: &str) -> String {
    text.replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}
