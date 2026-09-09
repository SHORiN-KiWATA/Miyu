//! 写入：记事实、记事件、消化成日记。
//!
//! `process_after_turn` 是回合结束后的入口，它只是**排队**：真正的组织交给后台
//! 的 organizer，因为那要调模型，不能挡住回合返回。
//!
//! `next_organization_batch` / `apply_organized_batch` 成对：批次在应用前会校验
//! 会话还在不在——`reset_all` 之后飞在半路的批次必须作废，否则刚清空的记忆会被
//! 旧批次重新写回来。

use crate::memory::*;

impl MemoryStore {
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

    pub fn remember_fact(&self, content: &str, source: &str) -> Result<i64> {
        if !self.config.enabled || !self.writes_enabled || content.trim().is_empty() {
            return Ok(0);
        }
        self.init()?;
        let ownership = self.manual_fact_ownership();
        let subjects = ownership_subjects_json(&ownership);
        let conn = self.data_conn()?;
        conn.execute(
            "INSERT INTO facts (
                content, source, status, confidence, recall_count, created_at, updated_at,
                visibility, owner_principal, owner_display_name, subjects, origin_session_id
             ) VALUES (?1, ?2, 'active', 1.0, 0, ?3, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                content.trim(),
                source.trim(),
                now(),
                ownership.visibility,
                ownership.owner_principal,
                ownership.owner_display_name,
                subjects,
                self.write_session_id(),
            ],
        )?;
        Ok(conn.last_insert_rowid())
    }

    pub fn remember_pending_event(
        &self,
        user_message: &str,
        assistant_message: &str,
    ) -> Result<()> {
        if !self.config.enabled || !self.writes_enabled || !self.config.auto_diary_enabled {
            return Ok(());
        }
        self.init()?;
        self.data_conn()?.execute(
            "INSERT INTO pending_events (
                user_message, assistant_message, created_at, origin_session_id
             ) VALUES (?1, ?2, ?3, ?4)",
            params![
                user_message.trim(),
                assistant_message.trim(),
                now(),
                self.write_session_id(),
            ],
        )?;
        Ok(())
    }

    pub fn process_after_turn(
        &self,
        user_message: &str,
        assistant_message: &str,
        origin: &MemoryOrigin,
        expected_database_id: &str,
        expected_generation: i64,
    ) -> Result<bool> {
        if !self.writes_enabled || !self.config.enabled || !self.config.auto_diary_enabled {
            return Ok(false);
        }
        if !self.data_db.is_file() {
            self.init()?;
        }
        let created_at = now();
        let expires_at = (Utc::now()
            + ChronoDuration::days(self.config.short_diary_retention_days as i64))
        .to_rfc3339();
        let content = diary_content(&created_at, user_message, assistant_message);
        let ownership = self.automatic_ownership(origin);
        let subjects = ownership_subjects_json(&ownership);
        let mut conn = self.data_conn_existing()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (current_database_id, current_generation) = tx.query_row(
            "SELECT database_id, generation FROM memory_meta WHERE id=1",
            [],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
        )?;
        if current_database_id != expected_database_id || current_generation != expected_generation
        {
            return Ok(false);
        }
        tx.execute(
            "INSERT INTO episodes (
                content, source, status, strength, recall_count, created_at, updated_at,
                retention, user_message, assistant_message, expires_at,
                origin_kind, origin_platform, origin_account_id, origin_conversation_kind,
                origin_conversation_id, origin_sender_id, origin_sender_display_name,
                origin_session_id, origin_message_id,
                visibility, owner_principal, owner_display_name, subjects
             ) VALUES (?1, 'episode', 'active', 1.0, 0, ?2, ?2, ?3, ?4, ?5, ?6,
                       ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19)",
            params![
                content,
                created_at,
                SHORT_TERM,
                user_message.trim(),
                assistant_message.trim(),
                expires_at,
                origin.kind,
                origin.platform,
                origin.account_id,
                origin.conversation_kind,
                origin.conversation_id,
                origin.sender_id,
                origin.sender_display_name,
                origin.session_id,
                origin.message_id,
                ownership.visibility,
                ownership.owner_principal,
                ownership.owner_display_name,
                subjects,
            ],
        )?;
        tx.commit()?;
        self.cleanup_expired_short_diaries()?;
        Ok(true)
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
            "short_diaries": count_where(&data, "episodes", "retention='short_term'")?,
            "long_diaries": count_where(&data, "episodes", "retention='long_term'")?,
            "unconsolidated_diaries": count_where(&data, "episodes", "retention='short_term' AND consolidated_at IS NULL")?,
            "unprocessed_pending_events": count_where(&data, "pending_events", "processed_at IS NULL")?,
            "total_pending_events": count_rows(&data, "pending_events")?,
            "skill_records": count_rows(&data, "skill_records")?,
            "skill_dirs": count_skill_dirs(&self.skills_dir)?,
            "evicted_turns": count_rows(&state, "evicted_turns")?,
        }))
    }

    pub fn reset_all(&self, include_skills: bool) -> Result<()> {
        self.init()?;
        let mut data = self.data_conn()?;
        let tx = data.transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute(
            "UPDATE memory_meta SET generation=generation+1 WHERE id=1",
            [],
        )?;
        tx.execute("DELETE FROM facts", [])?;
        tx.execute("DELETE FROM episodes", [])?;
        tx.execute("DELETE FROM pending_events", [])?;
        tx.execute("DELETE FROM skill_records", [])?;
        tx.execute("DELETE FROM memory_revisions", [])?;
        tx.execute(
            "DELETE FROM sqlite_sequence WHERE name IN ('facts', 'episodes', 'pending_events', 'skill_records', 'memory_revisions')",
            [],
        )?;
        tx.commit()?;
        self.clear_evicted_context()?;
        if include_skills {
            self.remove_auto_skills()?;
        }
        Ok(())
    }

    /// 只清掉本会话产生的记忆。改动之前存下的旧行没有会话标记
    /// (`origin_session_id` 为空串),不会被这里删掉——那些只能走 `reset_all`。
    ///
    /// 空 id 直接报错而不是当成"清空标记的行":那批正是全部历史遗留数据,
    /// 静默清掉它们等于把 `reset_all` 伪装成会话级重置。
    pub fn reset_session(&self, session_id: &str) -> Result<MemoryResetSummary> {
        let session_id = session_id.trim();
        if session_id.is_empty() {
            bail!("session-scoped memory reset needs a session id");
        }
        self.init()?;
        let mut data = self.data_conn()?;
        let tx = data.transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute(
            "UPDATE memory_meta SET generation=generation+1 WHERE id=1",
            [],
        )?;
        // 向量按 (kind, id) 挂在行上,没有触发器跟着删。行走了它还在,而 id
        // 是自增的,迟早被新行撞上并读回一段别人的语义。kind 字面量与
        // `semantic::kind_name` 同源。
        tx.execute(
            "DELETE FROM memory_embeddings
              WHERE (kind='fact' AND id IN (SELECT id FROM facts WHERE origin_session_id=?1))
                 OR (kind='episode' AND id IN (SELECT id FROM episodes WHERE origin_session_id=?1))",
            params![session_id],
        )?;
        let facts = tx.execute(
            "DELETE FROM facts WHERE origin_session_id=?1",
            params![session_id],
        )?;
        let episodes = tx.execute(
            "DELETE FROM episodes WHERE origin_session_id=?1",
            params![session_id],
        )?;
        let pending_events = tx.execute(
            "DELETE FROM pending_events WHERE origin_session_id=?1",
            params![session_id],
        )?;
        tx.commit()?;
        let evicted_turns = self.clear_session_evicted_context(session_id)?;
        Ok(MemoryResetSummary {
            facts,
            episodes,
            pending_events,
            evicted_turns,
        })
    }

    pub(crate) fn remove_auto_skills(&self) -> Result<()> {
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
            if crate::skills::is_generated_skill(&raw) {
                std::fs::remove_dir_all(entry.path())?;
            }
        }
        Ok(())
    }

    pub(crate) fn flush_pending_events(&self) -> Result<()> {
        if !self.config.enabled || !self.config.auto_diary_enabled {
            return Ok(());
        }
        self.init()?;
        let conn = self.data_conn()?;
        let mut stmt = conn.prepare(
            "SELECT id, user_message, assistant_message, created_at, origin_session_id
               FROM pending_events WHERE processed_at IS NULL ORDER BY id LIMIT 20",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
            ))
        })?;
        for row in rows {
            let (id, user, assistant, created_at, origin_session_id) = row?;
            let content = diary_content(&created_at, &user, &assistant);
            let expires_at = (Utc::now()
                + ChronoDuration::days(self.config.short_diary_retention_days as i64))
            .to_rfc3339();
            // 会话标记跟着待处理事件走,不取当前环境:消化是后台批处理,
            // 此刻的"当前会话"跟这条事件的来源没有关系。
            conn.execute(
                "INSERT INTO episodes (
                    content, source, status, recall_count, created_at, updated_at,
                    retention, user_message, assistant_message, expires_at, origin_session_id
                 ) VALUES (?1, 'episode', 'active', 0, ?2, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    content,
                    created_at,
                    SHORT_TERM,
                    user,
                    assistant,
                    expires_at,
                    origin_session_id,
                ],
            )?;
            conn.execute(
                "UPDATE pending_events SET processed_at=?1 WHERE id=?2",
                params![now(), id],
            )?;
        }
        Ok(())
    }

    pub(crate) fn next_organization_batch(&self) -> Result<Option<OrganizationBatch>> {
        if !self.config.enabled || !self.config.auto_diary_enabled {
            return Ok(None);
        }
        if !self.data_db.is_file() {
            return Ok(None);
        }
        self.init_existing()?;
        self.cleanup_expired_short_diaries()?;
        let conn = self.data_conn_existing()?;
        let (database_id, generation) = conn.query_row(
            "SELECT database_id, generation FROM memory_meta WHERE id=1",
            [],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
        )?;
        // 已被遗忘(主动 forget 或衰减)的日记不参与整理:否则会被组织
        // 批次"复活"为长期记忆。
        let forced = count_where(
            &conn,
            "episodes",
            "retention='short_term' AND promotion_pending=1 AND status != 'forgotten'",
        )?;
        let unconsolidated = count_where(
            &conn,
            "episodes",
            "retention='short_term' AND consolidated_at IS NULL AND status != 'forgotten'",
        )?;
        if forced == 0 && unconsolidated < self.config.diary_batch_size as i64 {
            return Ok(None);
        }

        let (sql, limit) = if forced > 0 {
            (
                "SELECT id, created_at, user_message, assistant_message, 1,
                        origin_kind, origin_platform, origin_account_id,
                        origin_conversation_kind, origin_conversation_id, origin_sender_id,
                        origin_sender_display_name, origin_session_id, origin_message_id
                 FROM episodes
                 WHERE retention='short_term' AND promotion_pending=1
                   AND status != 'forgotten'
                 ORDER BY id LIMIT ?1",
                self.config.diary_batch_size.max(1),
            )
        } else {
            (
                "SELECT id, created_at, user_message, assistant_message, 0,
                        origin_kind, origin_platform, origin_account_id,
                        origin_conversation_kind, origin_conversation_id, origin_sender_id,
                        origin_sender_display_name, origin_session_id, origin_message_id
                 FROM episodes
                 WHERE retention='short_term' AND consolidated_at IS NULL
                   AND status != 'forgotten'
                 ORDER BY id LIMIT ?1",
                self.config.diary_batch_size,
            )
        };
        let mut stmt = conn.prepare(sql)?;
        let diaries = stmt
            .query_map([limit as i64], |row| {
                let origin = MemoryOrigin {
                    kind: row.get(5)?,
                    platform: row.get(6)?,
                    account_id: row.get(7)?,
                    conversation_kind: row.get(8)?,
                    conversation_id: row.get(9)?,
                    sender_id: row.get(10)?,
                    sender_display_name: row.get(11)?,
                    session_id: row.get(12)?,
                    message_id: row.get(13)?,
                };
                Ok(ShortDiaryRecord {
                    id: row.get(0)?,
                    created_at: row.get(1)?,
                    user_message: row.get(2)?,
                    assistant_message: row.get(3)?,
                    force_long_term: row.get::<_, i64>(4)? != 0,
                    owner_principal: origin
                        .principal_ownership()
                        .map(|ownership| ownership.owner_principal),
                    origin,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        if diaries.is_empty() {
            return Ok(None);
        }
        let existing = load_existing_memory_candidates(&conn, &diaries)?;
        Ok(Some(OrganizationBatch {
            database_id,
            generation,
            diaries,
            existing,
        }))
    }

    pub(crate) fn apply_organized_batch(
        &self,
        batch: &OrganizationBatch,
        output: OrganizedOutput,
    ) -> Result<()> {
        if !self.data_db.is_file() {
            bail!("memory database moved or removed while organization was running");
        }
        if output.knowledge.len() + output.long_diaries.len() > MAX_ORGANIZED_ITEMS {
            bail!("memory organizer returned too many items");
        }
        let diary_ids = batch
            .diaries
            .iter()
            .map(|diary| diary.id)
            .collect::<BTreeSet<_>>();
        let forced_ids = batch
            .diaries
            .iter()
            .filter(|diary| diary.force_long_term)
            .map(|diary| diary.id)
            .collect::<BTreeSet<_>>();
        let candidate_fact_ids = batch
            .existing
            .iter()
            .filter(|memory| memory.kind == "knowledge")
            .map(|memory| memory.id)
            .collect::<BTreeSet<_>>();
        let candidate_facts = batch
            .existing
            .iter()
            .filter(|memory| memory.kind == "knowledge")
            .map(|memory| (memory.id, memory))
            .collect::<BTreeMap<_, _>>();
        for action in &output.knowledge {
            validate_knowledge_action(action, &diary_ids, &candidate_fact_ids)?;
            validate_knowledge_visibility(batch, action)?;
            validate_knowledge_update_scope(batch, action, &candidate_facts)?;
        }
        let mut promoted_ids = BTreeSet::new();
        for diary in &output.long_diaries {
            validate_long_diary(batch, diary, &diary_ids)?;
            promoted_ids.extend(diary.diary_ids.iter().copied());
        }
        if !forced_ids.is_subset(&promoted_ids) {
            bail!("memory organizer did not promote every required diary");
        }

        let mut conn = self.data_conn_existing()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (current_database_id, current_generation) = tx.query_row(
            "SELECT database_id, generation FROM memory_meta WHERE id=1",
            [],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
        )?;
        if current_database_id != batch.database_id || current_generation != batch.generation {
            bail!("memory database was moved, replaced, or reset while organization was running");
        }
        let timestamp = now();
        if self.config.auto_fact_enabled {
            for action in output.knowledge {
                let source_ids = normalized_ids_json(&action.diary_ids);
                let tags = normalized_tags_json(&action.tags);
                let ownership = knowledge_ownership(batch, &action);
                let subjects =
                    organized_subjects_json(batch, &action.diary_ids, &action.subjects, &ownership);
                match action.operation.as_str() {
                    "create" => {
                        tx.execute(
                            "INSERT INTO facts (
                                content, source, status, confidence, strength, recall_count,
                                created_at, updated_at, memory_type, truth_status, importance,
                                tags, source_episode_ids,
                                visibility, owner_principal, owner_display_name, subjects,
                                origin_session_id
                             ) SELECT ?1, 'diary-organizer', 'active', ?2, 1.0, 0,
                                      ?3, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13
                               WHERE NOT EXISTS (
                                    SELECT 1 FROM facts
                                     WHERE content=?1 AND truth_status!='rejected'
                                       AND visibility=?9 AND owner_principal=?10
                                )",
                            params![
                                action.content.trim(),
                                action.confidence,
                                timestamp,
                                action.memory_type,
                                action.truth_status,
                                action.importance,
                                tags,
                                source_ids,
                                ownership.visibility,
                                ownership.owner_principal,
                                ownership.owner_display_name,
                                subjects,
                                organized_session_id(batch, &action.diary_ids),
                            ],
                        )?;
                    }
                    "update" => {
                        let target = action
                            .target_id
                            .context("missing knowledge update target")?;
                        let old_content = tx.query_row(
                            "SELECT content FROM facts WHERE id=?1",
                            [target],
                            |row| row.get::<_, String>(0),
                        )?;
                        tx.execute(
                            "INSERT INTO memory_revisions (
                                memory_id, old_content, new_content, source_episode_ids, created_at
                             ) VALUES (?1, ?2, ?3, ?4, ?5)",
                            params![
                                target,
                                old_content,
                                action.content.trim(),
                                source_ids,
                                timestamp
                            ],
                        )?;
                        tx.execute(
                            "UPDATE facts SET content=?1, source='diary-organizer', status='active',
                                confidence=?2, strength=1.0, updated_at=?3, memory_type=?4,
                                truth_status=?5, importance=?6, tags=?7, source_episode_ids=?8,
                                visibility=?9, owner_principal=?10, owner_display_name=?11,
                                subjects=?12
                              WHERE id=?13",
                            params![
                                action.content.trim(),
                                action.confidence,
                                timestamp,
                                action.memory_type,
                                action.truth_status,
                                action.importance,
                                tags,
                                source_ids,
                                ownership.visibility,
                                ownership.owner_principal,
                                ownership.owner_display_name,
                                subjects,
                                target,
                            ],
                        )?;
                    }
                    _ => unreachable!("validated operation"),
                }
            }
        }

        for diary in output.long_diaries {
            let source_ids = normalized_ids_json(&diary.diary_ids);
            let tags = normalized_tags_json(&diary.tags);
            let source_key = format!(
                "{}:{}",
                diary
                    .diary_ids
                    .iter()
                    .copied()
                    .collect::<BTreeSet<_>>()
                    .into_iter()
                    .map(|id| id.to_string())
                    .collect::<Vec<_>>()
                    .join(","),
                blake3::hash(diary.content.trim().as_bytes()).to_hex()
            );
            let ownership = diary_ownership(batch, &diary.diary_ids);
            let subjects =
                organized_subjects_json(batch, &diary.diary_ids, &diary.subjects, &ownership);
            tx.execute(
                "INSERT OR IGNORE INTO episodes (
                    content, source, status, strength, recall_count, created_at, updated_at,
                    retention, consolidated_at, importance, confidence, tags,
                    source_episode_ids, source_key,
                    visibility, owner_principal, owner_display_name, subjects,
                    origin_session_id
                 ) VALUES (?1, 'diary-organizer', 'active', 1.0, 0, ?2, ?2,
                           ?3, ?2, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
                params![
                    diary.content.trim(),
                    timestamp,
                    LONG_TERM,
                    diary.importance,
                    diary.confidence,
                    tags,
                    source_ids,
                    source_key,
                    ownership.visibility,
                    ownership.owner_principal,
                    ownership.owner_display_name,
                    subjects,
                    organized_session_id(batch, &diary.diary_ids),
                ],
            )?;
        }

        for diary in &batch.diaries {
            tx.execute(
                "UPDATE episodes SET consolidated_at=COALESCE(consolidated_at, ?1),
                    promotion_pending=CASE WHEN ?2 THEN 0 ELSE promotion_pending END,
                    promoted_at=CASE WHEN ?2 THEN COALESCE(promoted_at, ?1) ELSE promoted_at END
                 WHERE id=?3 AND retention='short_term'",
                params![timestamp, promoted_ids.contains(&diary.id), diary.id],
            )?;
        }
        tx.commit()?;
        self.cleanup_expired_short_diaries()?;
        Ok(())
    }

    pub(crate) fn cleanup_expired_short_diaries(&self) -> Result<usize> {
        if !self.data_db.is_file() {
            return Ok(0);
        }
        let conn = self.data_conn_existing()?;
        conn.execute(
            "UPDATE episodes SET status='forgotten'
             WHERE retention='short_term'
               AND consolidated_at IS NULL
               AND promotion_pending=0
               AND expires_at IS NOT NULL
               AND unixepoch(expires_at) IS NOT NULL
               AND unixepoch(expires_at) <= unixepoch('now')",
            [],
        )?;
        Ok(conn.execute(
            "DELETE FROM episodes
             WHERE retention='short_term'
               AND consolidated_at IS NOT NULL
               AND promotion_pending=0
               AND expires_at IS NOT NULL
               AND unixepoch(expires_at) IS NOT NULL
               AND unixepoch(expires_at) <= unixepoch('now')",
            [],
        )?)
    }

    pub(crate) fn prune_missing_skill_records(&self) -> Result<()> {
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
}
