use crate::config::{AppConfig, KnowledgeBasePluginConfig, MemoryConfig};
use crate::paths::MiyuPaths;
use anyhow::Result;
use chrono::{DateTime, Utc};
use rusqlite::{params, Connection};
use serde_json::{json, Value};
use std::collections::HashSet;
use std::path::PathBuf;

#[derive(Clone)]
pub struct MemoryStore {
    config: MemoryConfig,
    kb_config: KnowledgeBasePluginConfig,
    data_db: PathBuf,
    state_db: PathBuf,
    skills_dir: PathBuf,
}

#[derive(Debug, Clone)]
pub struct EvictedTurn {
    pub timestamp: String,
    pub role: String,
    pub content: String,
}

#[derive(Debug, Clone)]
pub struct AssociationContext {
    pub facts: Vec<MemoryHit>,
    pub episodes: Vec<MemoryHit>,
}

#[derive(Debug, Clone)]
pub struct MemoryHit {
    pub id: i64,
    pub content: String,
    pub score: f32,
    pub timestamp: String,
    pub source: String,
}

impl MemoryStore {
    pub fn new(config: &AppConfig, paths: &MiyuPaths) -> Self {
        let data_dir = config.active_persona_memory_data_dir(paths).join("memory");
        let state_dir = config.active_persona_memory_state_dir(paths).join("memory");
        Self {
            config: config.memory_config().clone(),
            kb_config: config.plugins.knowledge_base.clone(),
            data_db: data_dir.join("memory.db"),
            state_db: state_dir.join("evicted_context.db"),
            skills_dir: config.active_persona_skills_dir(paths),
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

    pub fn remember_evicted_turns(&self, turns: &[EvictedTurn]) -> Result<()> {
        if !self.config.enabled || !self.config.evicted_context_enabled || turns.is_empty() {
            return Ok(());
        }
        self.init()?;
        let mut conn = self.state_conn()?;
        let tx = conn.transaction()?;
        for turn in turns {
            tx.execute(
                "INSERT INTO evicted_turns (timestamp, role, content, created_at) VALUES (?1, ?2, ?3, ?4)",
                params![turn.timestamp, turn.role, turn.content, now()],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn clear_evicted_context(&self) -> Result<()> {
        self.init()?;
        self.state_conn()?
            .execute("DELETE FROM evicted_turns", [])?;
        Ok(())
    }

    pub fn clear_pending_events(&self) -> Result<()> {
        self.init()?;
        let data = self.data_conn()?;
        data.execute("DELETE FROM pending_events", [])?;
        data.execute(
            "DELETE FROM sqlite_sequence WHERE name = 'pending_events'",
            [],
        )?;
        Ok(())
    }

    #[allow(dead_code)]
    pub fn search_evicted_context(&self, query: &str, limit: usize) -> Result<Value> {
        self.init()?;
        self.search_evicted_context_existing(query, limit)
    }

    pub fn search_evicted_context_readonly(&self, query: &str, limit: usize) -> Result<Value> {
        if !self.state_db.is_file() {
            return Ok(json!({ "ok": true, "query": query, "results": [] }));
        }
        self.search_evicted_context_existing(query, limit)
    }

    fn search_evicted_context_existing(&self, query: &str, limit: usize) -> Result<Value> {
        let tokens = query_tokens(query);
        let conn = self.state_conn()?;
        let mut stmt = conn.prepare(
            "SELECT id, timestamp, role, content FROM evicted_turns ORDER BY id DESC LIMIT 1000",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
            ))
        })?;
        let mut hits = Vec::new();
        for row in rows {
            let (id, timestamp, role, content) = row?;
            let score = score_text(&content, &tokens);
            if score <= 0.0 {
                continue;
            }
            hits.push(json!({
                "id": id,
                "timestamp": timestamp,
                "role": role,
                "score": score,
                "snippet": snippet(&content, &tokens, self.kb_config.snippet_context_chars),
            }));
        }
        sort_json_hits(&mut hits);
        hits.truncate(limit.clamp(1, 50));
        Ok(json!({ "ok": true, "query": query, "results": hits }))
    }

    pub fn remember_fact(&self, content: &str, source: &str) -> Result<i64> {
        if !self.config.enabled || content.trim().is_empty() {
            return Ok(0);
        }
        self.init()?;
        let conn = self.data_conn()?;
        conn.execute(
            "INSERT INTO facts (content, source, status, confidence, recall_count, created_at, updated_at) VALUES (?1, ?2, 'active', 1.0, 0, ?3, ?3)",
            params![content.trim(), source.trim(), now()],
        )?;
        Ok(conn.last_insert_rowid())
    }

    pub fn remember_pending_event(
        &self,
        user_message: &str,
        assistant_message: &str,
    ) -> Result<()> {
        if !self.config.enabled || !self.config.auto_diary_enabled {
            return Ok(());
        }
        self.init()?;
        self.data_conn()?.execute(
            "INSERT INTO pending_events (user_message, assistant_message, created_at) VALUES (?1, ?2, ?3)",
            params![user_message.trim(), assistant_message.trim(), now()],
        )?;
        Ok(())
    }

    pub fn process_after_turn(&self, user_message: &str, assistant_message: &str) -> Result<()> {
        self.remember_pending_event(user_message, assistant_message)?;
        self.flush_pending_events()?;
        Ok(())
    }

    pub fn stats(&self) -> Result<Value> {
        self.init()?;
        self.prune_missing_skill_records()?;
        let data = self.data_conn()?;
        let state = self.state_conn()?;
        Ok(json!({
            "ok": true,
            "data_db": self.data_db.display().to_string(),
            "state_db": self.state_db.display().to_string(),
            "skills_dir": self.skills_dir.display().to_string(),
            "facts": count_rows(&data, "facts")?,
            "episodes": count_rows(&data, "episodes")?,
            "unprocessed_pending_events": count_where(&data, "pending_events", "processed_at IS NULL")?,
            "total_pending_events": count_rows(&data, "pending_events")?,
            "skill_records": count_rows(&data, "skill_records")?,
            "skill_dirs": count_skill_dirs(&self.skills_dir)?,
            "evicted_turns": count_rows(&state, "evicted_turns")?,
        }))
    }

    pub fn reset_all(&self, include_skills: bool) -> Result<()> {
        self.init()?;
        let data = self.data_conn()?;
        data.execute("DELETE FROM facts", [])?;
        data.execute("DELETE FROM episodes", [])?;
        data.execute("DELETE FROM pending_events", [])?;
        data.execute("DELETE FROM skill_records", [])?;
        data.execute(
            "DELETE FROM sqlite_sequence WHERE name IN ('facts', 'episodes', 'pending_events', 'skill_records')",
            [],
        )?;
        self.clear_evicted_context()?;
        if include_skills {
            self.remove_auto_skills()?;
        }
        Ok(())
    }

    fn remove_auto_skills(&self) -> Result<()> {
        if !self.skills_dir.exists() {
            return Ok(());
        }
        for entry in std::fs::read_dir(&self.skills_dir)? {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            let skill_file = entry.path().join("SKILL.md");
            let raw = std::fs::read_to_string(&skill_file).unwrap_or_default();
            if raw.contains("Auto-learned method from assistant conversation")
                || raw.contains("Auto-learned method from Miyu conversation")
                || raw.contains("generated_by: miyu")
            {
                std::fs::remove_dir_all(entry.path())?;
            }
        }
        Ok(())
    }

    fn flush_pending_events(&self) -> Result<()> {
        if !self.config.enabled || !self.config.auto_diary_enabled {
            return Ok(());
        }
        self.init()?;
        let conn = self.data_conn()?;
        let mut stmt = conn.prepare(
            "SELECT id, user_message, assistant_message, created_at FROM pending_events WHERE processed_at IS NULL ORDER BY id LIMIT 20",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
            ))
        })?;
        for row in rows {
            let (id, user, assistant, created_at) = row?;
            let content = format!(
                "{}，我被要求：{}；结果：{}",
                created_at,
                truncate_chars(&compact_line(&user), 260),
                truncate_chars(&compact_line(&assistant), 520)
            );
            conn.execute(
                "INSERT INTO episodes (content, source, status, recall_count, created_at, updated_at) VALUES (?1, 'episode', 'active', 0, ?2, ?2)",
                params![content, created_at],
            )?;
            conn.execute(
                "UPDATE pending_events SET processed_at=?1 WHERE id=?2",
                params![now(), id],
            )?;
        }
        Ok(())
    }

    fn prune_missing_skill_records(&self) -> Result<()> {
        let conn = self.data_conn()?;
        let mut stmt = conn.prepare("SELECT id, path FROM skill_records")?;
        let rows = stmt.query_map([], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
        })?;
        let mut missing = Vec::new();
        for row in rows {
            let (id, path) = row?;
            if !PathBuf::from(path).exists() {
                missing.push(id);
            }
        }
        drop(stmt);
        for id in missing {
            conn.execute("DELETE FROM skill_records WHERE id=?1", params![id])?;
        }
        Ok(())
    }

    pub fn recall_memories(
        &self,
        query: &str,
        limit: usize,
        include_forgotten: bool,
    ) -> Result<Value> {
        self.init()?;
        self.recall_memories_existing(query, limit, include_forgotten)
    }

    pub fn recall_memories_readonly(
        &self,
        query: &str,
        limit: usize,
        include_forgotten: bool,
    ) -> Result<Value> {
        if !self.data_db.is_file() {
            return Ok(json!({ "ok": true, "query": query, "facts": [], "episodes": [] }));
        }
        self.recall_memories_existing(query, limit, include_forgotten)
    }

    fn recall_memories_existing(
        &self,
        query: &str,
        limit: usize,
        include_forgotten: bool,
    ) -> Result<Value> {
        let facts = self.search_facts(query, limit, include_forgotten)?;
        let episodes = self.search_episodes(query, limit, include_forgotten)?;
        Ok(json!({
            "ok": true,
            "query": query,
            "facts": facts.iter().map(memory_hit_json).collect::<Vec<_>>(),
            "episodes": episodes.iter().map(memory_hit_json).collect::<Vec<_>>(),
        }))
    }

    #[allow(dead_code)]
    pub fn recall_past_events(&self, query: &str, limit: usize) -> Result<Value> {
        self.init()?;
        self.recall_past_events_existing(query, limit)
    }

    pub fn recall_past_events_readonly(&self, query: &str, limit: usize) -> Result<Value> {
        if !self.data_db.is_file() {
            return Ok(json!({ "ok": true, "query": query, "episodes": [] }));
        }
        self.recall_past_events_existing(query, limit)
    }

    fn recall_past_events_existing(&self, query: &str, limit: usize) -> Result<Value> {
        let episodes = self.search_episodes(query, limit, true)?;
        Ok(json!({
            "ok": true,
            "query": query,
            "episodes": episodes.iter().map(memory_hit_json).collect::<Vec<_>>(),
        }))
    }

    pub fn association(&self, query: &str) -> Result<Option<AssociationContext>> {
        if !self.config.enabled || !self.config.association_enabled {
            return Ok(None);
        }
        self.init()?;
        let facts = self.search_facts(query, self.config.association_facts, false)?;
        let episodes = self.search_episodes(query, self.config.association_episodes, false)?;
        for hit in facts.iter().chain(episodes.iter()) {
            self.reinforce(hit.id, &hit.source)?;
        }
        if facts.is_empty() && episodes.is_empty() {
            return Ok(None);
        }
        Ok(Some(AssociationContext { facts, episodes }))
    }

    pub fn format_association(&self, association: &AssociationContext) -> String {
        let mut output = String::new();
        output.push_str("<associative-memory>\n");
        output.push_str("以下是根据当前用户输入联想到的旧记忆，可能相关也可能不相关；必要时使用，不要强行引用。\n");
        if !association.facts.is_empty() {
            output.push_str("\n曾经记住的相关知识点：\n");
            for hit in &association.facts {
                output.push_str("- ");
                output.push_str(&compact_line(&hit.content));
                output.push('\n');
            }
        }
        if !association.episodes.is_empty() {
            output.push_str("\n曾经发生的事情：\n");
            for hit in &association.episodes {
                output.push_str("- ");
                output.push_str(&compact_line(&hit.content));
                output.push('\n');
            }
        }
        output.push_str("</associative-memory>");
        truncate_chars(&output, self.config.association_max_chars)
    }

    fn search_facts(
        &self,
        query: &str,
        limit: usize,
        include_forgotten: bool,
    ) -> Result<Vec<MemoryHit>> {
        self.search_table("facts", query, limit, include_forgotten)
    }

    fn search_episodes(
        &self,
        query: &str,
        limit: usize,
        include_forgotten: bool,
    ) -> Result<Vec<MemoryHit>> {
        self.search_table("episodes", query, limit, include_forgotten)
    }

    fn search_table(
        &self,
        table: &str,
        query: &str,
        limit: usize,
        include_forgotten: bool,
    ) -> Result<Vec<MemoryHit>> {
        let tokens = query_tokens(query);
        let sql = format!(
            "SELECT id, content, source, status, created_at FROM {table} ORDER BY updated_at DESC LIMIT 1000"
        );
        let conn = self.data_conn()?;
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
            ))
        })?;
        let mut hits = Vec::new();
        for row in rows {
            let (id, content, source, status, timestamp) = row?;
            if !include_forgotten && status == "forgotten" {
                continue;
            }
            let score = score_text(&content, &tokens);
            if score <= 0.0 {
                continue;
            }
            hits.push(MemoryHit {
                id,
                content,
                score,
                timestamp,
                source,
            });
        }
        hits.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        hits.truncate(limit.clamp(1, 50));
        Ok(hits)
    }

    fn reinforce(&self, id: i64, source: &str) -> Result<()> {
        let table = if source == "episode" {
            "episodes"
        } else {
            "facts"
        };
        let sql = format!(
            "UPDATE {table} SET recall_count=recall_count+1, strength=MIN(1.0, strength+?1), last_recalled_at=?2, updated_at=?2, status='active' WHERE id=?3"
        );
        self.data_conn()?.execute(
            &sql,
            params![self.config.forgetting_review_boost, now(), id],
        )?;
        Ok(())
    }

    fn decay_memories(&self) -> Result<()> {
        if !self.config.enabled || !self.config.forgetting_enabled {
            return Ok(());
        }
        let conn = self.data_conn()?;
        decay_table(&conn, "facts", &self.config)?;
        decay_table(&conn, "episodes", &self.config)?;
        Ok(())
    }

    fn data_conn(&self) -> Result<Connection> {
        Ok(Connection::open(&self.data_db)?)
    }

    fn state_conn(&self) -> Result<Connection> {
        Ok(Connection::open(&self.state_db)?)
    }
}

fn init_data_db(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS facts (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            content TEXT NOT NULL,
            source TEXT NOT NULL DEFAULT '',
            status TEXT NOT NULL DEFAULT 'active',
            confidence REAL NOT NULL DEFAULT 1.0,
            strength REAL NOT NULL DEFAULT 1.0,
            recall_count INTEGER NOT NULL DEFAULT 0,
            last_recalled_at TEXT,
            last_decay_at TEXT,
            created_at TEXT NOT NULL,
            updated_at TEXT NOT NULL
        );
        CREATE TABLE IF NOT EXISTS episodes (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            content TEXT NOT NULL,
            source TEXT NOT NULL DEFAULT 'episode',
            status TEXT NOT NULL DEFAULT 'active',
            strength REAL NOT NULL DEFAULT 1.0,
            recall_count INTEGER NOT NULL DEFAULT 0,
            last_recalled_at TEXT,
            last_decay_at TEXT,
            created_at TEXT NOT NULL,
            updated_at TEXT NOT NULL
        );
        CREATE TABLE IF NOT EXISTS pending_events (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            user_message TEXT NOT NULL,
            assistant_message TEXT NOT NULL,
            created_at TEXT NOT NULL,
            processed_at TEXT
        );
        CREATE TABLE IF NOT EXISTS skill_records (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            name TEXT NOT NULL UNIQUE,
            path TEXT NOT NULL,
            summary TEXT NOT NULL DEFAULT '',
            created_at TEXT NOT NULL,
            updated_at TEXT NOT NULL
        );",
    )?;
    add_column_if_missing(conn, "facts", "strength", "REAL NOT NULL DEFAULT 1.0")?;
    add_column_if_missing(conn, "facts", "last_decay_at", "TEXT")?;
    add_column_if_missing(conn, "episodes", "strength", "REAL NOT NULL DEFAULT 1.0")?;
    add_column_if_missing(conn, "episodes", "last_decay_at", "TEXT")?;
    Ok(())
}

fn init_state_db(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS evicted_turns (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            timestamp TEXT NOT NULL,
            role TEXT NOT NULL,
            content TEXT NOT NULL,
            created_at TEXT NOT NULL
        );",
    )?;
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

fn decay_table(conn: &Connection, table: &str, config: &MemoryConfig) -> Result<()> {
    let now = Utc::now();
    let mut stmt = conn.prepare(&format!(
        "SELECT id, strength, COALESCE(last_recalled_at, updated_at, created_at), last_decay_at FROM {table} WHERE status='active'"
    ))?;
    let rows = stmt.query_map([], |row| {
        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, f64>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, Option<String>>(3)?,
        ))
    })?;
    let mut updates = Vec::new();
    for row in rows {
        let (id, strength, recalled_at, last_decay_at) = row?;
        let anchor = last_decay_at.as_deref().unwrap_or(&recalled_at);
        let Ok(anchor) = DateTime::parse_from_rfc3339(anchor) else {
            continue;
        };
        let days = (now - anchor.with_timezone(&Utc)).num_seconds().max(0) as f64 / 86_400.0;
        if days < 0.25 {
            continue;
        }
        let half_life = config.forgetting_half_life_days.max(0.1);
        let new_strength = strength * 2f64.powf(-days / half_life);
        let status = if new_strength < config.forgetting_min_strength {
            "forgotten"
        } else {
            "active"
        };
        updates.push((id, new_strength, status.to_string()));
    }
    drop(stmt);
    for (id, strength, status) in updates {
        conn.execute(
            &format!("UPDATE {table} SET strength=?1, status=?2, last_decay_at=?3 WHERE id=?4"),
            params![strength, status, now.to_rfc3339(), id],
        )?;
    }
    Ok(())
}

fn memory_hit_json(hit: &MemoryHit) -> Value {
    json!({
        "id": hit.id,
        "timestamp": hit.timestamp,
        "score": hit.score,
        "source": hit.source,
        "content": hit.content,
    })
}

fn sort_json_hits(hits: &mut [Value]) {
    hits.sort_by(|a, b| {
        b.get("score")
            .and_then(Value::as_f64)
            .unwrap_or_default()
            .partial_cmp(&a.get("score").and_then(Value::as_f64).unwrap_or_default())
            .unwrap_or(std::cmp::Ordering::Equal)
    });
}

fn score_text(text: &str, tokens: &[String]) -> f32 {
    if tokens.is_empty() {
        return 0.0;
    }
    let lower = text.to_ascii_lowercase();
    let mut score = 0.0;
    let mut matched = HashSet::new();
    for token in tokens {
        if lower.contains(token) {
            score += 10.0;
            matched.insert(token);
        }
    }
    score + matched.len() as f32 / tokens.len() as f32 * 20.0
}

fn query_tokens(query: &str) -> Vec<String> {
    query
        .split(|ch: char| ch.is_whitespace() || ch.is_ascii_punctuation())
        .map(str::trim)
        .filter(|token| token.chars().count() >= 2)
        .map(|token| token.to_ascii_lowercase())
        .collect()
}

fn snippet(text: &str, tokens: &[String], max_chars: usize) -> String {
    let lower = text.to_ascii_lowercase();
    let start = tokens
        .iter()
        .filter_map(|token| lower.find(token))
        .min()
        .unwrap_or(0);
    let start = text[..start.min(text.len())]
        .char_indices()
        .rev()
        .nth(max_chars / 4)
        .map(|(index, _)| index)
        .unwrap_or(0);
    truncate_chars(&text[start..], max_chars)
}

fn compact_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn truncate_chars(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    format!(
        "{}...",
        text.chars()
            .take(max_chars.saturating_sub(3))
            .collect::<String>()
    )
}

fn count_rows(conn: &Connection, table: &str) -> Result<i64> {
    let sql = format!("SELECT COUNT(*) FROM {table}");
    Ok(conn.query_row(&sql, [], |row| row.get(0))?)
}

fn count_where(conn: &Connection, table: &str, condition: &str) -> Result<i64> {
    let sql = format!("SELECT COUNT(*) FROM {table} WHERE {condition}");
    Ok(conn.query_row(&sql, [], |row| row.get(0))?)
}

fn count_skill_dirs(skills_dir: &PathBuf) -> Result<usize> {
    if !skills_dir.exists() {
        return Ok(0);
    }
    let mut count = 0usize;
    for entry in std::fs::read_dir(skills_dir)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() && entry.path().join("SKILL.md").is_file() {
            count += 1;
        }
    }
    Ok(count)
}

fn now() -> String {
    Utc::now().to_rfc3339()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AppConfig;
    use crate::paths::MiyuPaths;

    fn test_paths(temp: &tempfile::TempDir) -> MiyuPaths {
        MiyuPaths {
            config_dir: temp.path().join("config"),
            config_file: temp.path().join("config/config.jsonc"),
            secrets_file: temp.path().join("config/secrets.jsonc"),
            skills_dir: temp.path().join("config/skills"),
            data_dir: temp.path().join("data"),
            cache_dir: temp.path().join("cache"),
            state_dir: temp.path().join("state"),
            pictures_dir: temp.path().join("pictures"),
            fish_hook_file: temp.path().join("fish/miyu.fish"),
            bash_hook_file: temp.path().join("shell/bash-hook.sh"),
            zsh_hook_file: temp.path().join("shell/zsh-hook.zsh"),
            scripts_dir: temp.path().join("config/scripts"),
            system_scripts_dir: PathBuf::new(),
        }
    }

    #[test]
    fn remembers_and_recalls_fact() {
        let temp = tempfile::tempdir().unwrap();
        let config = AppConfig::default();
        let paths = test_paths(&temp);
        let store = MemoryStore::new(&config, &paths);
        store
            .remember_fact("Niri 输入法需要 XMODIFIERS", "test")
            .unwrap();
        let result = store.recall_memories("Niri XMODIFIERS", 5, false).unwrap();
        assert!(result.to_string().contains("XMODIFIERS"));
    }

    #[test]
    fn reset_all_clears_facts_and_episodes() {
        let temp = tempfile::tempdir().unwrap();
        let config = AppConfig::default();
        let paths = test_paths(&temp);
        let store = MemoryStore::new(&config, &paths);
        store
            .remember_fact("Niri 输入法需要 XMODIFIERS", "test")
            .unwrap();
        store.remember_pending_event("你好", "在呢").unwrap();
        store.flush_pending_events().unwrap();

        let before = store.recall_memories("你好 XMODIFIERS", 5, false).unwrap();
        assert!(!before["facts"].as_array().unwrap().is_empty());
        assert!(!before["episodes"].as_array().unwrap().is_empty());

        store.reset_all(false).unwrap();

        let after = store.recall_memories("你好 XMODIFIERS", 5, false).unwrap();
        assert!(after["facts"].as_array().unwrap().is_empty());
        assert!(after["episodes"].as_array().unwrap().is_empty());
    }

    #[test]
    fn evicted_context_can_be_cleared() {
        let temp = tempfile::tempdir().unwrap();
        let config = AppConfig::default();
        let paths = test_paths(&temp);
        let store = MemoryStore::new(&config, &paths);
        store
            .remember_evicted_turns(&[EvictedTurn {
                timestamp: "now".to_string(),
                role: "user".to_string(),
                content: "旧上下文 输入法".to_string(),
            }])
            .unwrap();
        assert!(store
            .search_evicted_context("输入法", 5)
            .unwrap()
            .to_string()
            .contains("旧上下文"));
        store.clear_evicted_context().unwrap();
        assert!(!store
            .search_evicted_context("输入法", 5)
            .unwrap()
            .to_string()
            .contains("旧上下文"));
    }
}
