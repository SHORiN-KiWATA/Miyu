//! 记忆库的建表与版本迁移。
//!
//! 两个库：数据库（记忆本身，跟人格走）和状态库（会话相关的记忆状态）。分开是
//! 因为换人格要换记忆，但会话状态不该跟着走。
//!
//! 迁移都是**幂等且保守**的：`migrate_memory_access_v2` 给老记录补可见性时一律
//! 按最严的填——猜错方向会把私密内容暴露给别人，反过来只是少召回几条。

use crate::memory::*;

pub(crate) fn init_data_db(conn: &Connection) -> Result<()> {
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
            updated_at TEXT NOT NULL,
            visibility TEXT NOT NULL DEFAULT 'privileged',
            owner_principal TEXT NOT NULL DEFAULT '',
            owner_display_name TEXT NOT NULL DEFAULT '',
            subjects TEXT NOT NULL DEFAULT '[]'
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
            updated_at TEXT NOT NULL,
            visibility TEXT NOT NULL DEFAULT 'privileged',
            owner_principal TEXT NOT NULL DEFAULT '',
            owner_display_name TEXT NOT NULL DEFAULT '',
            subjects TEXT NOT NULL DEFAULT '[]'
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
        );
        CREATE TABLE IF NOT EXISTS memory_revisions (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            memory_id INTEGER NOT NULL,
            old_content TEXT NOT NULL,
            new_content TEXT NOT NULL,
            source_episode_ids TEXT NOT NULL DEFAULT '[]',
            created_at TEXT NOT NULL
        );
        CREATE TABLE IF NOT EXISTS memory_meta (
            id INTEGER PRIMARY KEY CHECK(id=1),
            generation INTEGER NOT NULL DEFAULT 0,
            database_id TEXT NOT NULL DEFAULT '',
            access_schema_version INTEGER NOT NULL DEFAULT 2
        );",
    )?;
    add_column_if_missing(
        conn,
        "memory_meta",
        "database_id",
        "TEXT NOT NULL DEFAULT ''",
    )?;
    add_column_if_missing(
        conn,
        "memory_meta",
        "access_schema_version",
        "INTEGER NOT NULL DEFAULT 0",
    )?;
    conn.execute(
        "INSERT OR IGNORE INTO memory_meta (
            id, generation, database_id, access_schema_version
         ) VALUES (1, 0, '', 2)",
        [],
    )?;
    let database_id = conn.query_row(
        "SELECT database_id FROM memory_meta WHERE id=1",
        [],
        |row| row.get::<_, String>(0),
    )?;
    if database_id.is_empty() {
        conn.execute(
            "UPDATE memory_meta SET database_id=?1 WHERE id=1 AND database_id=''",
            [format!("mem-{:032x}", rand::random::<u128>())],
        )?;
    }
    add_column_if_missing(conn, "facts", "strength", "REAL NOT NULL DEFAULT 1.0")?;
    add_column_if_missing(conn, "facts", "last_decay_at", "TEXT")?;
    add_column_if_missing(conn, "facts", "memory_type", "TEXT NOT NULL DEFAULT 'fact'")?;
    add_column_if_missing(
        conn,
        "facts",
        "truth_status",
        "TEXT NOT NULL DEFAULT 'reported'",
    )?;
    add_column_if_missing(conn, "facts", "importance", "INTEGER NOT NULL DEFAULT 3")?;
    add_column_if_missing(conn, "facts", "tags", "TEXT NOT NULL DEFAULT '[]'")?;
    add_column_if_missing(
        conn,
        "facts",
        "source_episode_ids",
        "TEXT NOT NULL DEFAULT '[]'",
    )?;
    for table in ["facts", "episodes"] {
        add_column_if_missing(
            conn,
            table,
            "visibility",
            "TEXT NOT NULL DEFAULT 'privileged'",
        )?;
        add_column_if_missing(conn, table, "owner_principal", "TEXT NOT NULL DEFAULT ''")?;
        add_column_if_missing(
            conn,
            table,
            "owner_display_name",
            "TEXT NOT NULL DEFAULT ''",
        )?;
        add_column_if_missing(conn, table, "subjects", "TEXT NOT NULL DEFAULT '[]'")?;
    }
    add_column_if_missing(conn, "episodes", "strength", "REAL NOT NULL DEFAULT 1.0")?;
    add_column_if_missing(conn, "episodes", "last_decay_at", "TEXT")?;
    // Existing episodes predate the short/long split and must remain durable.
    add_column_if_missing(
        conn,
        "episodes",
        "retention",
        "TEXT NOT NULL DEFAULT 'long_term'",
    )?;
    add_column_if_missing(conn, "episodes", "user_message", "TEXT NOT NULL DEFAULT ''")?;
    add_column_if_missing(
        conn,
        "episodes",
        "assistant_message",
        "TEXT NOT NULL DEFAULT ''",
    )?;
    add_column_if_missing(conn, "episodes", "expires_at", "TEXT")?;
    add_column_if_missing(conn, "episodes", "consolidated_at", "TEXT")?;
    add_column_if_missing(
        conn,
        "episodes",
        "promotion_pending",
        "INTEGER NOT NULL DEFAULT 0",
    )?;
    add_column_if_missing(conn, "episodes", "promoted_at", "TEXT")?;
    add_column_if_missing(conn, "episodes", "importance", "INTEGER NOT NULL DEFAULT 3")?;
    add_column_if_missing(conn, "episodes", "confidence", "REAL NOT NULL DEFAULT 1.0")?;
    add_column_if_missing(conn, "episodes", "tags", "TEXT NOT NULL DEFAULT '[]'")?;
    add_column_if_missing(
        conn,
        "episodes",
        "source_episode_ids",
        "TEXT NOT NULL DEFAULT '[]'",
    )?;
    add_column_if_missing(conn, "episodes", "source_key", "TEXT NOT NULL DEFAULT ''")?;
    add_column_if_missing(conn, "episodes", "origin_kind", "TEXT NOT NULL DEFAULT ''")?;
    add_column_if_missing(
        conn,
        "episodes",
        "origin_platform",
        "TEXT NOT NULL DEFAULT ''",
    )?;
    add_column_if_missing(
        conn,
        "episodes",
        "origin_account_id",
        "TEXT NOT NULL DEFAULT ''",
    )?;
    add_column_if_missing(
        conn,
        "episodes",
        "origin_conversation_kind",
        "TEXT NOT NULL DEFAULT ''",
    )?;
    add_column_if_missing(
        conn,
        "episodes",
        "origin_conversation_id",
        "TEXT NOT NULL DEFAULT ''",
    )?;
    add_column_if_missing(
        conn,
        "episodes",
        "origin_sender_id",
        "TEXT NOT NULL DEFAULT ''",
    )?;
    add_column_if_missing(
        conn,
        "episodes",
        "origin_sender_display_name",
        "TEXT NOT NULL DEFAULT ''",
    )?;
    add_column_if_missing(
        conn,
        "episodes",
        "origin_session_id",
        "TEXT NOT NULL DEFAULT ''",
    )?;
    add_column_if_missing(
        conn,
        "episodes",
        "origin_message_id",
        "TEXT NOT NULL DEFAULT ''",
    )?;
    migrate_memory_access_v1(conn)?;
    migrate_memory_subjects_v2(conn)?;
    conn.execute_batch(
        "CREATE INDEX IF NOT EXISTS idx_episodes_retention_created
             ON episodes(retention, created_at);
         CREATE INDEX IF NOT EXISTS idx_episodes_organization
             ON episodes(retention, promotion_pending, consolidated_at, id);
         CREATE INDEX IF NOT EXISTS idx_memory_revisions_memory
             ON memory_revisions(memory_id, id);
         CREATE UNIQUE INDEX IF NOT EXISTS idx_long_diary_source_key
             ON episodes(source_key) WHERE retention='long_term' AND source_key!='';
         CREATE INDEX IF NOT EXISTS idx_facts_access_updated
             ON facts(visibility, owner_principal, updated_at DESC);
         CREATE INDEX IF NOT EXISTS idx_episodes_access_updated
             ON episodes(visibility, owner_principal, updated_at DESC);
         CREATE TABLE IF NOT EXISTS memory_embeddings (
            kind TEXT NOT NULL,
            id INTEGER NOT NULL,
            model TEXT NOT NULL,
            content_sha256 TEXT NOT NULL,
            embedding BLOB NOT NULL,
            created_at TEXT NOT NULL,
            PRIMARY KEY (kind, id)
         );",
    )?;
    Ok(())
}

pub(crate) fn init_state_db(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS evicted_turns (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            source_id TEXT,
            timestamp TEXT NOT NULL,
            role TEXT NOT NULL,
            content TEXT NOT NULL,
            created_at TEXT NOT NULL,
            visibility TEXT NOT NULL DEFAULT 'privileged',
            owner_principal TEXT NOT NULL DEFAULT '',
            owner_display_name TEXT NOT NULL DEFAULT ''
        );
        CREATE TABLE IF NOT EXISTS evicted_embeddings (
            id INTEGER PRIMARY KEY,
            model TEXT NOT NULL,
            embedding_json TEXT NOT NULL,
            created_at TEXT NOT NULL
        );
        CREATE VIRTUAL TABLE IF NOT EXISTS evicted_turns_fts USING fts5(
            content,
            content='evicted_turns',
            content_rowid='id',
            tokenize='trigram'
        );
        CREATE TRIGGER IF NOT EXISTS evicted_turns_fts_insert AFTER INSERT ON evicted_turns BEGIN
            INSERT INTO evicted_turns_fts(rowid, content) VALUES (new.id, new.content);
        END;
        CREATE TRIGGER IF NOT EXISTS evicted_turns_fts_delete AFTER DELETE ON evicted_turns BEGIN
            INSERT INTO evicted_turns_fts(evicted_turns_fts, rowid, content)
            VALUES ('delete', old.id, old.content);
        END;
        CREATE TRIGGER IF NOT EXISTS evicted_turns_fts_update AFTER UPDATE OF content ON evicted_turns BEGIN
            INSERT INTO evicted_turns_fts(evicted_turns_fts, rowid, content)
            VALUES ('delete', old.id, old.content);
            INSERT INTO evicted_turns_fts(rowid, content) VALUES (new.id, new.content);
        END;",
    )?;
    add_column_if_missing(conn, "evicted_turns", "source_id", "TEXT")?;
    // 09-05: vectors as f32 BLOBs; legacy JSON rows are simply re-embedded.
    add_column_if_missing(conn, "evicted_embeddings", "embedding", "BLOB")?;
    add_column_if_missing(
        conn,
        "evicted_turns",
        "visibility",
        "TEXT NOT NULL DEFAULT 'privileged'",
    )?;
    add_column_if_missing(
        conn,
        "evicted_turns",
        "owner_principal",
        "TEXT NOT NULL DEFAULT ''",
    )?;
    add_column_if_missing(
        conn,
        "evicted_turns",
        "owner_display_name",
        "TEXT NOT NULL DEFAULT ''",
    )?;
    conn.execute(
        "CREATE UNIQUE INDEX IF NOT EXISTS idx_evicted_turns_source_id
         ON evicted_turns(source_id) WHERE source_id IS NOT NULL",
        [],
    )?;
    conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_evicted_turns_access
         ON evicted_turns(visibility, owner_principal, id DESC)",
        [],
    )?;
    Ok(())
}

pub(crate) fn migrate_memory_access_v1(conn: &Connection) -> Result<()> {
    let version = conn.query_row(
        "SELECT access_schema_version FROM memory_meta WHERE id=1",
        [],
        |row| row.get::<_, i64>(0),
    )?;
    if version >= 1 {
        return Ok(());
    }

    #[derive(Clone)]
    struct LegacyEpisode {
        id: i64,
        source_episode_ids: String,
        origin: MemoryOrigin,
    }

    let tx = conn.unchecked_transaction()?;
    let episodes = {
        let mut stmt = tx.prepare(
            "SELECT id, source_episode_ids,
                    origin_kind, origin_platform, origin_account_id,
                    origin_conversation_kind, origin_conversation_id, origin_sender_id,
                    origin_sender_display_name, origin_session_id, origin_message_id
               FROM episodes ORDER BY id",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(LegacyEpisode {
                id: row.get(0)?,
                source_episode_ids: row.get(1)?,
                origin: MemoryOrigin {
                    kind: row.get(2)?,
                    platform: row.get(3)?,
                    account_id: row.get(4)?,
                    conversation_kind: row.get(5)?,
                    conversation_id: row.get(6)?,
                    sender_id: row.get(7)?,
                    sender_display_name: row.get(8)?,
                    session_id: row.get(9)?,
                    message_id: row.get(10)?,
                },
            })
        })?;
        rows.collect::<std::result::Result<Vec<_>, _>>()?
    };
    let mut ownerships = BTreeMap::<i64, MemoryOwnership>::new();
    for episode in &episodes {
        if let Some(ownership) = episode.origin.principal_ownership() {
            ownerships.insert(episode.id, ownership);
        } else if episode.origin.kind == "local" {
            ownerships.insert(episode.id, MemoryOwnership::privileged());
        }
    }
    for episode in &episodes {
        if ownerships.contains_key(&episode.id) {
            continue;
        }
        if let Some(ownership) = ownership_from_source_ids(&episode.source_episode_ids, &ownerships)
        {
            ownerships.insert(episode.id, ownership);
        }
    }
    for episode in &episodes {
        let ownership = ownerships
            .get(&episode.id)
            .cloned()
            .unwrap_or_else(MemoryOwnership::privileged);
        let subjects = ownership_subjects_json(&ownership);
        tx.execute(
            "UPDATE episodes SET visibility=?1, owner_principal=?2, owner_display_name=?3,
                                 subjects=?4
              WHERE id=?5",
            params![
                ownership.visibility,
                ownership.owner_principal,
                ownership.owner_display_name,
                subjects,
                episode.id,
            ],
        )?;
    }

    let facts = {
        let mut stmt = tx.prepare("SELECT id, source_episode_ids FROM facts ORDER BY id")?;
        let rows = stmt.query_map([], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
        })?;
        rows.collect::<std::result::Result<Vec<_>, _>>()?
    };
    for (id, source_ids) in facts {
        let ownership = ownership_from_source_ids(&source_ids, &ownerships)
            .unwrap_or_else(MemoryOwnership::privileged);
        let subjects = ownership_subjects_json(&ownership);
        tx.execute(
            "UPDATE facts SET visibility=?1, owner_principal=?2, owner_display_name=?3,
                              subjects=?4
              WHERE id=?5",
            params![
                ownership.visibility,
                ownership.owner_principal,
                ownership.owner_display_name,
                subjects,
                id,
            ],
        )?;
    }
    tx.execute(
        "UPDATE memory_meta SET access_schema_version=1 WHERE id=1",
        [],
    )?;
    tx.commit()?;
    Ok(())
}

pub(crate) fn migrate_memory_subjects_v2(conn: &Connection) -> Result<()> {
    let version = conn.query_row(
        "SELECT access_schema_version FROM memory_meta WHERE id=1",
        [],
        |row| row.get::<_, i64>(0),
    )?;
    if version >= 2 {
        return Ok(());
    }
    let tx = conn.unchecked_transaction()?;
    for table in ["facts", "episodes"] {
        let sql = format!(
            "SELECT id, visibility, owner_principal, owner_display_name
               FROM {table} WHERE subjects='[]' OR subjects=''"
        );
        let rows = {
            let mut stmt = tx.prepare(&sql)?;
            let mapped = stmt.query_map([], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                ))
            })?;
            mapped.collect::<std::result::Result<Vec<_>, _>>()?
        };
        let update = format!("UPDATE {table} SET subjects=?1 WHERE id=?2");
        for (id, visibility, owner_principal, owner_display_name) in rows {
            let ownership = MemoryOwnership {
                visibility: match visibility.as_str() {
                    VISIBILITY_PUBLIC => VISIBILITY_PUBLIC,
                    VISIBILITY_PRINCIPAL => VISIBILITY_PRINCIPAL,
                    _ => VISIBILITY_PRIVILEGED,
                },
                owner_principal,
                owner_display_name,
            };
            tx.execute(&update, params![ownership_subjects_json(&ownership), id])?;
        }
    }
    tx.execute(
        "UPDATE memory_meta SET access_schema_version=2 WHERE id=1",
        [],
    )?;
    tx.commit()?;
    Ok(())
}

pub(crate) fn ownership_from_source_ids(
    encoded: &str,
    ownerships: &BTreeMap<i64, MemoryOwnership>,
) -> Option<MemoryOwnership> {
    let ids = serde_json::from_str::<Vec<i64>>(encoded).ok()?;
    if ids.is_empty() {
        return None;
    }
    let mut principal: Option<MemoryOwnership> = None;
    for id in ids {
        let ownership = ownerships.get(&id)?;
        if ownership.visibility != VISIBILITY_PRINCIPAL {
            return Some(MemoryOwnership::privileged());
        }
        if principal
            .as_ref()
            .is_some_and(|existing| existing.owner_principal != ownership.owner_principal)
        {
            return Some(MemoryOwnership::privileged());
        }
        principal.get_or_insert_with(|| ownership.clone());
    }
    principal
}

pub(crate) fn add_column_if_missing(
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

pub(crate) fn decay_table(conn: &Connection, table: &str, config: &MemoryConfig) -> Result<()> {
    let now = Utc::now();
    let mut stmt = conn.prepare(&format!(
        "SELECT id, strength, COALESCE(last_recalled_at, updated_at, created_at), last_decay_at FROM {table} WHERE status='active'{}",
        if table == "episodes" { " AND retention='long_term'" } else { "" }
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

pub(crate) fn count_rows(conn: &Connection, table: &str) -> Result<i64> {
    let sql = format!("SELECT COUNT(*) FROM {table}");
    Ok(conn.query_row(&sql, [], |row| row.get(0))?)
}

pub(crate) fn count_where(conn: &Connection, table: &str, condition: &str) -> Result<i64> {
    let sql = format!("SELECT COUNT(*) FROM {table} WHERE {condition}");
    Ok(conn.query_row(&sql, [], |row| row.get(0))?)
}

pub(crate) fn count_skill_dirs(skills_dir: &PathBuf) -> Result<usize> {
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
