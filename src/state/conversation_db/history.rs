//! 历史的读取、裁剪与归档。
//!
//! 「可见」是历史的核心概念：被压缩进摘要的回合仍在库里，只是不再进上下文。
//! 所以到处成对出现 `load_turns` / `load_visible_turns`——前者用于回放和取证，
//! 后者用于组装请求。
//!
//! 归档删除（`archive_and_delete_visible_turns`）先复制再删，两步在同一事务
//! 里：删了没存下来的历史是找不回来的。

use crate::state::conversation_db::*;

impl ConversationDb {
    pub fn load_session_loaded_items(
        &self,
        session_id: &str,
        kind: &str,
    ) -> Result<std::collections::BTreeSet<String>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT name FROM session_loaded_items
             WHERE session_id = ?1 AND kind = ?2 ORDER BY name ASC",
        )?;
        let items = stmt
            .query_map(params![session_id, kind], |row| row.get::<_, String>(0))?
            .collect::<std::result::Result<std::collections::BTreeSet<_>, _>>()?;
        Ok(items)
    }

    pub fn load_session_loaded_items_with_sources(
        &self,
        session_id: &str,
        kind: &str,
    ) -> Result<Vec<(String, Option<String>)>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT name, source_turn_id FROM session_loaded_items
             WHERE session_id = ?1 AND kind = ?2 ORDER BY name ASC",
        )?;
        let items = stmt
            .query_map(params![session_id, kind], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(items)
    }

    pub fn add_session_loaded_items(
        &self,
        session_id: &str,
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
                "INSERT INTO session_loaded_items (session_id, kind, name, source_turn_id, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?5)
                 ON CONFLICT(session_id, kind, name) DO UPDATE SET
                    source_turn_id = COALESCE(excluded.source_turn_id, session_loaded_items.source_turn_id),
                    updated_at = excluded.updated_at",
                params![session_id, kind, name, source_turn_id, now],
            )?;
        }
        Ok(affected)
    }

    pub fn load_turns(&self, session_id: &str) -> Result<Vec<Turn>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT turn_id, seq, user_content, display_content, user_timestamp, assistant_content,
                    assistant_reasoning, assistant_provider_id, assistant_model, assistant_timestamp, status, tool_reports, hidden, is_summary, owner_pid,
                    token_total, token_usage_estimated, revision, context_messages, token_prompt, token_cache_read, tool_flow
             FROM turns WHERE session_id = ?1 ORDER BY seq ASC",
        )?;
        let mut turns = stmt
            .query_map(params![session_id], map_turn_row)?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        attach_turn_children_locked(&conn, &mut turns)?;
        Ok(turns)
    }

    #[allow(dead_code)]
    pub fn load_turns_excluding(
        &self,
        session_id: &str,
        exclude_turn_id: &str,
    ) -> Result<Vec<Turn>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT turn_id, seq, user_content, display_content, user_timestamp, assistant_content,
                    assistant_reasoning, assistant_provider_id, assistant_model, assistant_timestamp, status, tool_reports, hidden, is_summary, owner_pid,
                    token_total, token_usage_estimated, revision, context_messages, token_prompt, token_cache_read, tool_flow
             FROM turns WHERE session_id = ?1 AND turn_id != ?2 ORDER BY seq ASC",
        )?;
        let mut turns = stmt
            .query_map(params![session_id, exclude_turn_id], map_turn_row)?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        attach_turn_children_locked(&conn, &mut turns)?;
        Ok(turns)
    }

    #[allow(dead_code)]
    pub fn load_turns_for_context(&self, session_id: &str) -> Result<Vec<Turn>> {
        self.load_turns(session_id)
    }

    pub fn load_visible_turns(&self, session_id: &str) -> Result<Vec<Turn>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT turn_id, seq, user_content, display_content, user_timestamp, assistant_content,
                    assistant_reasoning, assistant_provider_id, assistant_model, assistant_timestamp, status, tool_reports, hidden, is_summary, owner_pid,
                    token_total, token_usage_estimated, revision, context_messages, token_prompt, token_cache_read, tool_flow
             FROM turns WHERE session_id = ?1 AND hidden = 0 ORDER BY seq ASC",
        )?;
        let mut turns = stmt
            .query_map(params![session_id], map_turn_row)?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        attach_turn_children_locked(&conn, &mut turns)?;
        Ok(turns)
    }

    pub fn load_visible_turns_excluding(
        &self,
        session_id: &str,
        exclude_turn_id: &str,
    ) -> Result<Vec<Turn>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT turn_id, seq, user_content, display_content, user_timestamp, assistant_content,
                    assistant_reasoning, assistant_provider_id, assistant_model, assistant_timestamp, status, tool_reports, hidden, is_summary, owner_pid,
                    token_total, token_usage_estimated, revision, context_messages, token_prompt, token_cache_read, tool_flow
             FROM turns WHERE session_id = ?1 AND hidden = 0 AND turn_id != ?2 ORDER BY seq ASC",
        )?;
        let mut turns = stmt
            .query_map(params![session_id, exclude_turn_id], map_turn_row)?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        attach_turn_children_locked(&conn, &mut turns)?;
        Ok(turns)
    }

    #[allow(dead_code)]
    pub fn hide_turns_before_seq(&self, session_id: &str, seq: i64) -> Result<usize> {
        let conn = self.conn.lock().unwrap();
        let affected = conn.execute(
            "UPDATE turns SET hidden = 1 WHERE session_id = ?1 AND seq <= ?2",
            params![session_id, seq],
        )?;
        Ok(affected)
    }

    #[allow(dead_code)]
    pub fn insert_summary_turn(
        &self,
        session_id: &str,
        summary: &str,
        tokens: TurnTokens,
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
        let seq = self.next_seq_locked(&conn, session_id)?;
        let now = Utc::now().to_rfc3339();
        let token_usage_estimated = i64::from(token_usage_estimated);
        conn.execute(
            "INSERT INTO turns (turn_id, session_id, seq, user_content, user_timestamp, assistant_content, assistant_timestamp, status, tool_reports, hidden, is_summary, token_total, token_usage_estimated, token_prompt, token_cache_read)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'completed', '[]', 0, 1, ?8, ?9, ?10, ?11)",
            params![turn_id, session_id, seq, "[conversation summary]", now, summary, now, tokens.total as i64, token_usage_estimated, tokens.prompt as i64, tokens.cache_read as i64],
        )?;
        Ok(())
    }

    pub fn load_last_summary(&self, session_id: &str) -> Result<Option<Turn>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT turn_id, seq, user_content, display_content, user_timestamp, assistant_content,
                    assistant_reasoning, assistant_provider_id, assistant_model, assistant_timestamp, status, tool_reports, hidden, is_summary, owner_pid,
                    token_total, token_usage_estimated, revision, context_messages, token_prompt, token_cache_read, tool_flow
             FROM turns WHERE session_id = ?1 AND is_summary = 1 AND hidden = 0 ORDER BY seq DESC LIMIT 1",
        )?;
        let turn = stmt
            .query_map(params![session_id], map_turn_row)?
            .next()
            .transpose()?;
        Ok(turn)
    }

    #[allow(dead_code)]
    pub fn count_turns(&self, session_id: &str) -> Result<i64> {
        let conn = self.conn.lock().unwrap();
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM turns WHERE session_id = ?1",
            params![session_id],
            |row| row.get(0),
        )?;
        Ok(count)
    }

    #[allow(dead_code)]
    pub fn total_chars(&self, session_id: &str) -> Result<usize> {
        let turns = self.load_turns(session_id)?;
        Ok(turns.iter().map(|t| turn_chars(t)).sum())
    }

    #[allow(dead_code)]
    pub fn trim_oldest_turns(&self, session_id: &str, count: usize) -> Result<Vec<Turn>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT turn_id, seq, user_content, display_content, user_timestamp, assistant_content,
                    assistant_reasoning, assistant_provider_id, assistant_model, assistant_timestamp, status, tool_reports, hidden, is_summary, owner_pid,
                    token_total, token_usage_estimated, revision, context_messages, token_prompt, token_cache_read, tool_flow
             FROM turns WHERE session_id = ?1 AND is_summary = 0 ORDER BY seq ASC LIMIT ?2",
        )?;
        let mut to_remove: Vec<Turn> = stmt
            .query_map(params![session_id, count as i64], map_turn_row)?
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

    pub fn oldest_evictable_visible_turns(
        &self,
        session_id: &str,
        count: usize,
    ) -> Result<Vec<Turn>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT turn_id, seq, user_content, display_content, user_timestamp, assistant_content,
                    assistant_reasoning, assistant_provider_id, assistant_model, assistant_timestamp, status, tool_reports, hidden, is_summary, owner_pid,
                    token_total, token_usage_estimated, revision, context_messages, token_prompt, token_cache_read, tool_flow
             FROM turns
             WHERE session_id = ?1 AND hidden = 0 AND is_summary = 0 AND status != 'running'
             ORDER BY seq ASC LIMIT ?2",
        )?;
        let count = i64::try_from(count).unwrap_or(i64::MAX);
        let mut turns = stmt
            .query_map(params![session_id, count], map_turn_row)?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        attach_turn_children_locked(&conn, &mut turns)?;
        Ok(turns)
    }

    pub fn delete_visible_turns(&self, session_id: &str, turn_ids: &[String]) -> Result<usize> {
        self.delete_visible_turns_checked(session_id, turn_ids, None)
    }

    pub fn delete_visible_turns_checked(
        &self,
        session_id: &str,
        turn_ids: &[String],
        expected_loaded_tools: Option<&[(String, Option<String>)]>,
    ) -> Result<usize> {
        if turn_ids.is_empty() {
            return Ok(0);
        }
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        verify_loaded_tool_sources(&tx, session_id, expected_loaded_tools)?;
        let affected = delete_visible_turns_in_transaction(&tx, session_id, turn_ids)?;
        tx.commit()?;
        Ok(affected)
    }

    pub fn archive_and_delete_visible_turns(
        &self,
        session_id: &str,
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
             (source_id, timestamp, role, content, created_at,
              visibility, owner_principal, owner_display_name)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)"
        );
        let operation = (|| -> Result<usize> {
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            verify_loaded_tool_sources(&tx, session_id, expected_loaded_tools)?;
            let created_at = Utc::now().to_rfc3339();
            for turn in turns {
                tx.execute(
                    &insert_sql,
                    params![
                        turn.source_id,
                        turn.timestamp,
                        turn.role,
                        turn.content,
                        created_at,
                        turn.visibility,
                        turn.owner_principal,
                        turn.owner_display_name,
                    ],
                )?;
            }
            let affected = delete_visible_turns_in_transaction(&tx, session_id, turn_ids)?;
            tx.commit()?;
            Ok(affected)
        })();
        let detach = conn.execute_batch(&format!("DETACH DATABASE {archive_alias}"));
        if let Err(detach_err) = detach {
            tracing::warn!(
                error = %detach_err,
                archive_alias,
                "{}",
                crate::i18n::text(
                    "failed to detach evicted-context database",
                    "分离已移出上下文的数据库失败",
                )
            );
        }
        operation
    }

    pub fn replace_visible_with_summary(
        &self,
        session_id: &str,
        fold_turn_ids: &[String],
        visible_turn_ids: &[String],
        summary: &str,
        tokens: TurnTokens,
        token_usage_estimated: bool,
        footprint_json: Option<&str>,
    ) -> Result<()> {
        if summary.trim().is_empty() {
            bail!("compact returned an empty summary");
        }
        if fold_turn_ids.is_empty() {
            bail!("compact selected no turns to fold");
        }

        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        let current_turn_ids = {
            let mut stmt = tx.prepare(
                "SELECT turn_id FROM turns
                 WHERE session_id = ?1 AND hidden = 0 ORDER BY seq ASC",
            )?;
            let turn_ids = stmt
                .query_map(params![session_id], |row| row.get::<_, String>(0))?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            turn_ids
        };
        if current_turn_ids != visible_turn_ids {
            bail!("conversation changed while compact was running");
        }
        // The previous summary (if any) is superseded by the merged one and
        // folds together with the selected turns. Tail turns keep lower seqs
        // than the old summary row, so membership is by explicit id, not by a
        // seq watermark.
        let prior_summary_ids = {
            let mut stmt = tx.prepare(
                "SELECT turn_id FROM turns
                 WHERE session_id = ?1 AND hidden = 0 AND is_summary = 1",
            )?;
            let ids = stmt
                .query_map(params![session_id], |row| row.get::<_, String>(0))?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            ids
        };
        let parent_summary_seq: Option<i64> = tx.query_row(
            "SELECT MAX(seq) FROM turns
                 WHERE session_id = ?1 AND hidden = 0 AND is_summary = 1",
            params![session_id],
            |row| row.get(0),
        )?;
        let mut hidden_ids: Vec<String> = fold_turn_ids.to_vec();
        for id in prior_summary_ids {
            if !hidden_ids.contains(&id) {
                hidden_ids.push(id);
            }
        }
        let mut hidden = 0usize;
        {
            let mut stmt = tx.prepare(
                "UPDATE turns SET hidden = 1
                 WHERE session_id = ?1 AND hidden = 0 AND turn_id = ?2",
            )?;
            for id in &hidden_ids {
                hidden += stmt.execute(params![session_id, id])?;
            }
        }
        if hidden == 0 {
            bail!("conversation changed before compact could be saved");
        }
        let hidden_json = serde_json::to_string(&hidden_ids)?;

        let turn_id = format!(
            "summary_{}_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis())
                .unwrap_or(0),
            rand::random::<u16>()
        );
        let seq: i64 = tx.query_row(
            "SELECT COALESCE(MAX(seq), 0) + 1 FROM turns WHERE session_id = ?1",
            params![session_id],
            |row| row.get(0),
        )?;
        let now = Utc::now().to_rfc3339();
        let token_total = tokens.total as i64;
        let token_usage_estimated = i64::from(token_usage_estimated);
        tx.execute(
            "INSERT INTO turns (turn_id, session_id, seq, user_content, user_timestamp, assistant_content, assistant_timestamp, status, tool_reports, hidden, is_summary, token_total, token_usage_estimated, token_prompt, token_cache_read, compact_reversible, compact_parent_summary_seq, compact_hidden_json, tool_footprint)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'completed', '[]', 0, 1, ?8, ?9, ?13, ?14, 1, ?10, ?11, ?12)",
            params![turn_id, session_id, seq, "[conversation summary]", now, summary, now, token_total, token_usage_estimated, parent_summary_seq, hidden_json, footprint_json, tokens.prompt as i64, tokens.cache_read as i64],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn reset(&self, session_id: &str) -> Result<()> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        tx.execute(
            "DELETE FROM queued_prompts WHERE session_id = ?1",
            params![session_id],
        )?;
        tx.execute(
            "DELETE FROM turns WHERE session_id = ?1",
            params![session_id],
        )?;
        tx.execute(
            "DELETE FROM session_loaded_items WHERE session_id = ?1",
            params![session_id],
        )?;
        // Subagent audit sessions now count toward this session's Σ, so a
        // reset that left them behind would zero the history and still report
        // a running total. They are records of a conversation that no longer
        // exists; they go with it.
        tx.execute(
            "DELETE FROM sessions WHERE parent_session_id = ?1 AND kind = 'subagent'",
            params![session_id],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn reset_persona_contexts(&self, persona: &str, platform: &str) -> Result<Vec<String>> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let target_sql = "WITH RECURSIVE targets(session_id) AS (
                 SELECT sessions.session_id
                   FROM sessions
                  WHERE sessions.persona = ?1
                    AND sessions.kind = 'user'
                    AND (
                        (NOT EXISTS (
                            SELECT 1 FROM platform_session_bindings
                             WHERE platform_session_bindings.session_id = sessions.session_id
                        ))
                        OR EXISTS (
                            SELECT 1 FROM platform_session_bindings
                             WHERE platform_session_bindings.session_id = sessions.session_id
                               AND platform_session_bindings.platform = ?2
                        )
                    )
                 UNION
                 SELECT child.session_id
                   FROM sessions child
                   JOIN targets parent ON child.parent_session_id = parent.session_id
                  WHERE child.persona = ?1
             )";
        let session_ids = {
            let mut stmt = tx.prepare(&format!(
                "{target_sql} SELECT session_id FROM targets ORDER BY session_id"
            ))?;
            let rows = stmt.query_map(params![persona, platform], |row| row.get(0))?;
            rows.collect::<std::result::Result<Vec<_>, _>>()?
        };
        for table in ["queued_prompts", "turns", "session_loaded_items"] {
            tx.execute(
                &format!(
                    "{target_sql} DELETE FROM {table} WHERE session_id IN (SELECT session_id FROM targets)"
                ),
                params![persona, platform],
            )?;
        }
        // Subagent runs bill to the session that launched them, and their usage
        // lives on the session row rather than in `turns` — deleting the turns
        // alone would leave every Σ still carrying the subagent totals of a
        // conversation that no longer exists.
        tx.execute(
            &format!(
                "{target_sql} DELETE FROM sessions
                  WHERE kind = 'subagent' AND session_id IN (SELECT session_id FROM targets)"
            ),
            params![persona, platform],
        )?;
        tx.commit()?;
        Ok(session_ids)
    }

    pub fn reset_history(&self, session_id: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "DELETE FROM turns WHERE session_id = ?1",
            params![session_id],
        )?;
        conn.execute(
            "DELETE FROM session_loaded_items WHERE session_id = ?1",
            params![session_id],
        )?;
        Ok(())
    }

    pub fn undo_last_turn(&self, session_id: &str) -> Result<(usize, Option<String>)> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        let running: i64 = tx.query_row(
            "SELECT COUNT(*) FROM turns WHERE session_id = ?1 AND hidden = 0 AND status = 'running'",
            params![session_id],
            |row| row.get(0),
        )?;
        if running > 0 {
            tx.rollback()?;
            return Ok((0, None));
        }
        let last: Option<(String, i64, String, bool, bool, Option<i64>, Option<String>)> = tx
            .query_row(
                "SELECT turn_id, seq, user_content, is_summary,
                        compact_reversible, compact_parent_summary_seq, compact_hidden_json
                 FROM turns WHERE session_id = ?1 AND hidden = 0 ORDER BY seq DESC LIMIT 1",
                params![session_id],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get::<_, i64>(3)? != 0,
                        row.get::<_, i64>(4)? != 0,
                        row.get(5)?,
                        row.get(6)?,
                    ))
                },
            )
            .optional()?;
        match last {
            Some((turn_id, _, user_content, false, _, _, _)) => {
                tx.execute("DELETE FROM turns WHERE turn_id = ?1", params![turn_id])?;
                tx.commit()?;
                Ok((1, Some(user_content)))
            }
            Some((_, _, _, true, false, _, _)) => {
                tx.rollback()?;
                Ok((0, None))
            }
            Some((turn_id, _summary_seq, _, true, true, _, Some(hidden_json))) => {
                // Tail-retention era summary: restore exactly the set this
                // compaction hid (folded turns + the superseded summary row).
                let hidden_ids: Vec<String> =
                    serde_json::from_str(&hidden_json).unwrap_or_default();
                if hidden_ids.is_empty() {
                    tx.rollback()?;
                    return Ok((0, None));
                }
                let mut restored = 0usize;
                {
                    let mut stmt = tx.prepare(
                        "UPDATE turns SET hidden = 0
                         WHERE session_id = ?1 AND hidden = 1 AND turn_id = ?2",
                    )?;
                    for id in &hidden_ids {
                        restored += stmt.execute(params![session_id, id])?;
                    }
                }
                if restored == 0 {
                    tx.rollback()?;
                    return Ok((0, None));
                }
                tx.execute("DELETE FROM turns WHERE turn_id = ?1", params![turn_id])?;
                tx.commit()?;
                Ok((1, None))
            }
            Some((turn_id, summary_seq, _, true, true, parent_summary_seq, None)) => {
                let restorable: i64 = match parent_summary_seq {
                    Some(previous_seq) => tx.query_row(
                        "SELECT COUNT(*) FROM turns
                         WHERE session_id = ?1 AND hidden = 1 AND seq < ?2
                           AND (seq = ?3 OR (is_summary = 0 AND seq > ?3))",
                        params![session_id, summary_seq, previous_seq],
                        |row| row.get(0),
                    )?,
                    None => tx.query_row(
                        "SELECT COUNT(*) FROM turns
                         WHERE session_id = ?1 AND hidden = 1 AND is_summary = 0 AND seq < ?2",
                        params![session_id, summary_seq],
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
                             WHERE session_id = ?1 AND hidden = 1 AND seq < ?2
                               AND (seq = ?3 OR (is_summary = 0 AND seq > ?3))",
                            params![session_id, summary_seq, previous_seq],
                        )?;
                    }
                    None => {
                        tx.execute(
                            "UPDATE turns SET hidden = 0
                             WHERE session_id = ?1 AND hidden = 1 AND is_summary = 0 AND seq < ?2",
                            params![session_id, summary_seq],
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
    /// Completed background-command wake turns after `after_seq`, oldest
    /// first: (seq, user display content, assistant reply).
    pub fn background_report_replies_after(
        &self,
        session_id: &str,
        after_seq: i64,
    ) -> Result<Vec<(i64, String, String, String)>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT seq, turn_id, display_content,
                    CASE WHEN status = 'completed' THEN assistant_content
                         WHEN length(trim(assistant_content)) > 0 THEN assistant_content
                         ELSE '（自动跟进未能完成：模型请求失败或被中断，可用 job 工具查看任务输出）'
                    END
             FROM turns
             WHERE session_id = ?1 AND seq > ?2 AND status IN ('completed', 'failed', 'interrupted')
               AND user_content LIKE '<background-job-report>%'
             ORDER BY seq ASC LIMIT 8",
        )?;
        let rows = stmt
            .query_map(params![session_id, after_seq], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                ))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    #[allow(dead_code)]
    pub fn migrate_from_jsonl(&self, session_id: &str, jsonl_path: &Path) -> Result<usize> {
        if !jsonl_path.exists() {
            return Ok(0);
        }
        let turns = self.load_turns(session_id)?;
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
                    let seq = self.next_seq_locked(&conn, session_id)?;
                    conn.execute(
                        "INSERT INTO turns (turn_id, session_id, seq, user_content, user_timestamp, assistant_content, status)
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'completed')",
                        params![turn_id, session_id, seq, prev_content, prev_ts, "(migrated without reply)"],
                    )?;
                    drop(conn);
                    migrated += 1;
                }
                pending_user = Some((timestamp, content.to_string()));
            } else if role == "assistant" {
                if let Some((user_ts, user_content)) = pending_user.take() {
                    let turn_id = format!("migrated_{}", migrated);
                    let conn = self.conn.lock().unwrap();
                    let seq = self.next_seq_locked(&conn, session_id)?;
                    let now = Utc::now().to_rfc3339();
                    conn.execute(
                        "INSERT INTO turns (turn_id, session_id, seq, user_content, user_timestamp,
                         assistant_content, assistant_reasoning, assistant_timestamp, status)
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 'completed')",
                        params![
                            turn_id,
                            session_id,
                            seq,
                            user_content,
                            user_ts,
                            content,
                            reasoning,
                            now
                        ],
                    )?;
                    drop(conn);
                    migrated += 1;
                }
            }
        }
        if let Some((user_ts, user_content)) = pending_user {
            let turn_id = format!("migrated_{}", migrated);
            let conn = self.conn.lock().unwrap();
            let seq = self.next_seq_locked(&conn, session_id)?;
            conn.execute(
                "INSERT INTO turns (turn_id, session_id, seq, user_content, user_timestamp, assistant_content, status)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'interrupted')",
                params![
                    turn_id,
                    session_id,
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
