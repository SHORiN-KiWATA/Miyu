//! 回合的写入与终结。
//!
//! 一个回合有四种终结方式，各有各的落库形态：正常完成、带用量完成、修订版完
//! 成、被打断。分成独立方法而不是一个带 flag 的函数，是因为它们写的列不同，
//! 混在一起很容易漏写某一列。
//!
//! `merge_turn_footprint` 是增量的：工具在回合中途不断产出足迹（读过哪些文件、
//! 记住了什么），要能一次次并进去而不是最后一次性写。

use crate::state::conversation_db::*;

impl ConversationDb {
    pub fn start_turn(
        &self,
        session_id: &str,
        turn_id: &str,
        user_content: &str,
        display_content: &str,
        owner_pid: u32,
        queue_session_id: &str,
        workspace: Option<&str>,
        attachment_run_id: Option<&str>,
    ) -> Result<()> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let seq = self.next_seq_locked(&tx, session_id)?;
        let now = Utc::now().to_rfc3339();
        tx.execute(
            "INSERT INTO turns (turn_id, session_id, seq, user_content, display_content, user_timestamp, assistant_content, status, owner_pid, queue_session_id, workspace)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'running', ?8, ?9, ?10)",
            params![
                turn_id,
                session_id,
                seq,
                user_content,
                display_content,
                now,
                PENDING_PLACEHOLDER,
                owner_pid as i64,
                queue_session_id,
                workspace
            ],
        )?;
        tx.execute(
            "INSERT INTO turn_journal_segments
                (turn_id, revision, segment_index, status, started_at)
             VALUES (?1, 0, 0, 'running', ?2)",
            params![turn_id, now],
        )?;
        if let Some(run_id) = attachment_run_id {
            tx.execute(
                "UPDATE user_attachments SET run_id = NULL, turn_id = ?1
                 WHERE session_id = ?2 AND run_id = ?3",
                params![turn_id, session_id, run_id],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    pub fn append_turn_journal_event(
        &self,
        turn_id: &str,
        revision: i64,
        segment_index: i64,
        kind: &str,
        call_id: Option<&str>,
        name: Option<&str>,
        text_payload: Option<&str>,
        blob_payload: Option<&[u8]>,
        ok: Option<bool>,
    ) -> Result<()> {
        if text_payload.is_some_and(|payload| payload.len() > MAX_JOURNAL_TEXT_EVENT_BYTES) {
            bail!("turn journal text event exceeds the 64 MiB limit");
        }
        if blob_payload.is_some_and(|payload| payload.len() > MAX_JOURNAL_BLOB_EVENT_BYTES) {
            bail!("turn journal binary event exceeds the 8 MiB limit");
        }
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let valid: bool = tx.query_row(
            "SELECT EXISTS(
                 SELECT 1 FROM turns t
                 INNER JOIN turn_journal_segments s
                   ON s.turn_id = t.turn_id AND s.revision = t.revision
                  AND s.segment_index = ?3
                 WHERE t.turn_id = ?1 AND t.revision = ?2
                   AND t.status = 'running' AND s.status != 'superseded'
             )",
            params![turn_id, revision, segment_index],
            |row| row.get(0),
        )?;
        if !valid {
            bail!("turn journal generation is no longer active");
        }
        tx.execute(
            "INSERT INTO turn_journal_events
                (turn_id, revision, segment_index, kind, call_id, name,
                 text_payload, blob_payload, ok, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                turn_id,
                revision,
                segment_index,
                kind,
                call_id,
                name,
                text_payload,
                blob_payload,
                ok.map(i64::from),
                Utc::now().to_rfc3339(),
            ],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn supersede_turn_journal_segment(
        &self,
        turn_id: &str,
        revision: i64,
        segment_index: i64,
    ) -> Result<()> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let affected = tx.execute(
            "UPDATE turn_journal_segments
             SET status = 'superseded', finished_at = ?1
             WHERE turn_id = ?2 AND revision = ?3 AND segment_index = ?4
               AND status = 'running'",
            params![Utc::now().to_rfc3339(), turn_id, revision, segment_index],
        )?;
        if affected != 1 {
            bail!("turn journal segment changed before supersession");
        }
        tx.execute(
            "INSERT INTO turn_journal_events
                (turn_id, revision, segment_index, kind, created_at)
             VALUES (?1, ?2, ?3, 'generation_superseded', ?4)",
            params![turn_id, revision, segment_index, Utc::now().to_rfc3339()],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// 批量追加。见 [`Self::append_tool_report`]——同样一条读都不做。
    pub fn append_tool_reports(&self, turn_id: &str, reports: &[String]) -> Result<()> {
        if reports.is_empty() {
            return Ok(());
        }
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        {
            let mut stmt =
                tx.prepare("INSERT INTO turn_tool_reports (turn_id, report) VALUES (?1, ?2)")?;
            for report in reports {
                stmt.execute(params![turn_id, report])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Stores the fossilized transient tail for a turn (v7 append-only).
    pub fn set_turn_context_messages(&self, turn_id: &str, messages: &[ChatMessage]) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE turns SET context_messages = ?1 WHERE turn_id = ?2",
            params![serde_json::to_string(messages)?, turn_id],
        )?;
        Ok(())
    }

    /// 完成后落一次结构化工具流。独立 UPDATE 而非扩 complete 签名:调用点多,
    /// 且流为空(无工具回合)时根本不写。
    pub fn set_turn_tool_flow(&self, turn_id: &str, flow: &[ToolFlowRound]) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE turns SET tool_flow = ?1 WHERE turn_id = ?2",
            params![serde_json::to_string(flow)?, turn_id],
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
        self.complete_turn_with_usage(
            turn_id,
            content,
            reasoning,
            None,
            None,
            TurnTokens::default(),
            false,
        )
    }

    pub fn complete_turn_with_usage(
        &self,
        turn_id: &str,
        content: &str,
        reasoning: Option<&str>,
        provider_id: Option<&str>,
        model: Option<&str>,
        tokens: TurnTokens,
        token_usage_estimated: bool,
    ) -> Result<()> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let now = Utc::now().to_rfc3339();
        let token_usage_estimated = i64::from(token_usage_estimated);
        let affected = tx.execute(
            "UPDATE turns SET assistant_content = ?1, assistant_reasoning = ?2,
                    assistant_provider_id = ?3, assistant_model = ?4, assistant_timestamp = ?5,
                    status = 'completed', token_total = ?6, token_usage_estimated = ?7,
                    token_prompt = ?9, token_cache_read = ?10
              WHERE turn_id = ?8 AND status = 'running'",
            params![
                content,
                reasoning,
                provider_id,
                model,
                now,
                tokens.total as i64,
                token_usage_estimated,
                turn_id,
                tokens.prompt as i64,
                tokens.cache_read as i64
            ],
        )?;
        if affected != 1 {
            bail!("turn changed before it could be completed");
        }
        bump_completion_seq_locked(&tx, turn_id)?;
        // Snapshot the display transcript before the journal goes: the tables
        // below are load-bearing for in-flight turn recovery, so they keep
        // being wiped on completion exactly as before.
        store_replay_journal(&tx, turn_id)?;
        tx.execute(
            "DELETE FROM turn_journal_segments WHERE turn_id = ?1",
            params![turn_id],
        )?;
        touch_session_last_request(&tx, turn_id)?;
        tx.commit()?;
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    pub fn complete_turn_revision_with_usage(
        &self,
        turn_id: &str,
        revision: i64,
        content: &str,
        reasoning: Option<&str>,
        provider_id: Option<&str>,
        model: Option<&str>,
        tokens: TurnTokens,
        token_usage_estimated: bool,
    ) -> Result<()> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let now = Utc::now().to_rfc3339();
        let affected = tx.execute(
            "UPDATE turns SET assistant_content = ?1, assistant_reasoning = ?2,
                    assistant_provider_id = ?3, assistant_model = ?4, assistant_timestamp = ?5,
                    status = 'completed', token_total = ?6, token_usage_estimated = ?7,
                    token_prompt = ?10, token_cache_read = ?11
             WHERE turn_id = ?8 AND revision = ?9 AND status = 'running'",
            params![
                content,
                reasoning,
                provider_id,
                model,
                now,
                tokens.total as i64,
                i64::from(token_usage_estimated),
                turn_id,
                revision,
                tokens.prompt as i64,
                tokens.cache_read as i64
            ],
        )?;
        if affected != 1 {
            bail!("redo generation changed before it could be completed");
        }
        tx.execute(
            "DELETE FROM turn_redo_backups WHERE turn_id = ?1 AND revision = ?2",
            params![turn_id, revision],
        )?;
        // redo 重写了整个回合:先清掉旧修订的重放转写再按新修订快照,
        // 否则重开 REPL 仍显示被弃用的旧回复(空 journal 时也必须清)。
        tx.execute(
            "UPDATE turns SET replay_journal = NULL WHERE turn_id = ?1",
            params![turn_id],
        )?;
        store_replay_journal(&tx, turn_id)?;
        tx.execute(
            "DELETE FROM turn_journal_segments WHERE turn_id = ?1",
            params![turn_id],
        )?;
        touch_session_last_request(&tx, turn_id)?;
        tx.commit()?;
        Ok(())
    }

    pub fn interrupt_turn(&self, turn_id: &str) -> Result<()> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let revision: Option<i64> = tx
            .query_row(
                "SELECT revision FROM turns WHERE turn_id = ?1 AND status = 'running'",
                params![turn_id],
                |row| row.get(0),
            )
            .optional()?;
        let Some(revision) = revision else {
            tx.commit()?;
            return Ok(());
        };
        let now = Utc::now().to_rfc3339();
        let (content, reasoning) = interrupted_projection_locked(&tx, turn_id, revision)?;
        tx.execute(
            "UPDATE turns SET assistant_content = ?1, assistant_reasoning = ?2,
                    assistant_timestamp = ?3, status = 'interrupted'
             WHERE turn_id = ?4 AND revision = ?5 AND status = 'running'",
            params![content, reasoning, now, turn_id, revision],
        )?;
        bump_completion_seq_locked(&tx, turn_id)?;
        tx.execute(
            "UPDATE turn_journal_segments
             SET status = 'interrupted', finished_at = ?1
             WHERE turn_id = ?2 AND revision = ?3 AND status = 'running'",
            params![now, turn_id, revision],
        )?;
        touch_session_last_request(&tx, turn_id)?;
        tx.commit()?;
        Ok(())
    }

    pub fn interrupt_turn_revision(&self, turn_id: &str, revision: i64) -> Result<bool> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let restored = restore_redo_backup_locked(&tx, turn_id, revision)?;
        if !restored {
            let (content, reasoning) = interrupted_projection_locked(&tx, turn_id, revision)?;
            let now = Utc::now().to_rfc3339();
            tx.execute(
                "UPDATE turns SET assistant_content = ?1, assistant_reasoning = ?2,
                        assistant_timestamp = ?3, status = 'interrupted'
                 WHERE turn_id = ?4 AND revision = ?5 AND status = 'running'",
                params![content, reasoning, now, turn_id, revision],
            )?;
            tx.execute(
                "UPDATE turn_journal_segments
                 SET status = 'interrupted', finished_at = ?1
                 WHERE turn_id = ?2 AND revision = ?3 AND status = 'running'",
                params![now, turn_id, revision],
            )?;
        }
        tx.commit()?;
        Ok(restored)
    }

    /// Unions `delta` into the turn's stored footprint. Read-modify-write is
    /// safe here: the turn is running and owned by exactly one process.
    pub fn merge_turn_footprint(&self, turn_id: &str, delta: &ToolFootprint) -> Result<()> {
        if delta.is_empty() {
            return Ok(());
        }
        let conn = self.conn.lock().unwrap();
        let existing: Option<Option<String>> = conn
            .query_row(
                "SELECT tool_footprint FROM turns WHERE turn_id = ?1",
                params![turn_id],
                |row| row.get(0),
            )
            .optional()?;
        let Some(existing) = existing else {
            return Ok(());
        };
        let mut footprint = existing
            .as_deref()
            .and_then(|json| serde_json::from_str::<ToolFootprint>(json).ok())
            .unwrap_or_default();
        footprint.merge(delta.clone());
        conn.execute(
            "UPDATE turns SET tool_footprint = ?1 WHERE turn_id = ?2",
            params![serde_json::to_string(&footprint)?, turn_id],
        )?;
        Ok(())
    }

    /// Merged footprint across the given turns (summary rows included — they
    /// carry the accumulated footprint of everything they folded).
    pub fn load_merged_footprint(
        &self,
        session_id: &str,
        turn_ids: &[String],
    ) -> Result<ToolFootprint> {
        let conn = self.conn.lock().unwrap();
        let mut merged = ToolFootprint::default();
        let mut stmt = conn
            .prepare("SELECT tool_footprint FROM turns WHERE session_id = ?1 AND turn_id = ?2")?;
        for turn_id in turn_ids {
            let value: Option<Option<String>> = stmt
                .query_row(params![session_id, turn_id], |row| row.get(0))
                .optional()?;
            if let Some(Some(json)) = value {
                if let Ok(footprint) = serde_json::from_str::<ToolFootprint>(&json) {
                    merged.merge(footprint);
                }
            }
        }
        Ok(merged)
    }

    /// 追加一条工具报告（v25 起走 `turn_tool_reports` 子表）。
    ///
    /// 一次 INSERT，**没有读**。原来是「读整列 → 解析 → push → 整个序列化 →
    /// 写回」，第 k 次追加要写回当前全部 k 条，总写入 O(N²)。
    ///
    /// 顺序由 `report_id` 自增保证，所以连 `MAX(seq)` 都不用查。
    pub fn append_tool_report(&self, turn_id: &str, report: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO turn_tool_reports (turn_id, report) VALUES (?1, ?2)",
            params![turn_id, report],
        )?;
        Ok(())
    }

    pub fn turn_session_id(&self, turn_id: &str) -> Result<Option<String>> {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT session_id FROM turns WHERE turn_id = ?1",
            params![turn_id],
            |row| row.get(0),
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

    /// Largest turn seq in a session (0 when empty).
    pub fn latest_turn_seq(&self, session_id: &str) -> Result<i64> {
        let conn = self.conn.lock().unwrap();
        Ok(conn.query_row(
            "SELECT COALESCE(MAX(seq), 0) FROM turns WHERE session_id = ?1",
            params![session_id],
            |row| row.get(0),
        )?)
    }

    pub fn has_running_turns(&self, session_id: &str) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM turns WHERE session_id = ?1 AND status = 'running'",
            params![session_id],
            |row| row.get(0),
        )?;
        Ok(count > 0)
    }

    pub fn has_any_running_turns(&self) -> Result<bool> {
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
        session_id: &str,
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
              WHERE turns.session_id = ?1 AND turns.status = 'running'
              ORDER BY turns.seq DESC
              LIMIT 1",
            params![session_id],
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
    pub fn running_turn_summaries(&self, session_id: &str) -> Result<Vec<String>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT user_content FROM turns
             WHERE session_id = ?1 AND status = 'running' ORDER BY seq ASC",
        )?;
        let summaries = stmt
            .query_map(params![session_id], |row| row.get::<_, String>(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(summaries)
    }

    pub fn running_turn_summaries_excluding(
        &self,
        session_id: &str,
        exclude_turn_id: &str,
    ) -> Result<Vec<String>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT user_content FROM turns
             WHERE session_id = ?1 AND status = 'running' AND turn_id != ?2 ORDER BY seq ASC",
        )?;
        let summaries = stmt
            .query_map(params![session_id, exclude_turn_id], |row| {
                row.get::<_, String>(0)
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(summaries)
    }

    pub fn recover_stale_running_turns(&self) -> Result<Vec<StaleTurnRecovery>> {
        let mut conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT turn_id, session_id, owner_pid, revision, queue_session_id
             FROM turns WHERE status = 'running'",
        )?;
        let stale_turn_ids: Vec<(String, String, i64, Option<String>)> = stmt
            .query_map([], |row| {
                let turn_id: String = row.get(0)?;
                let session_id: String = row.get(1)?;
                let owner_pid: Option<i64> = row.get(2)?;
                let revision: i64 = row.get(3)?;
                let queue_session_id: Option<String> = row.get(4)?;
                Ok((turn_id, session_id, owner_pid, revision, queue_session_id))
            })?
            .filter_map(|row| {
                let (turn_id, session_id, owner_pid, revision, queue_session_id) = row.ok()?;
                let alive = owner_pid
                    .map(|pid| crate::alarm::process_exists(pid as u32))
                    .unwrap_or(false);
                if alive {
                    None
                } else {
                    Some((turn_id, session_id, revision, queue_session_id))
                }
            })
            .collect();
        drop(stmt);
        if stale_turn_ids.is_empty() {
            return Ok(Vec::new());
        }
        let tx = conn.transaction()?;
        let now = Utc::now().to_rfc3339();
        let mut recoveries = Vec::with_capacity(stale_turn_ids.len());
        for (turn_id, session_id, revision, queue_session_id) in &stale_turn_ids {
            if restore_redo_backup_locked(&tx, turn_id, *revision)? {
                recoveries.push(StaleTurnRecovery {
                    turn_id: turn_id.clone(),
                    session_id: session_id.clone(),
                    restored_redo: true,
                });
                continue;
            }
            consume_stale_queued_prompts_locked(
                &tx,
                turn_id,
                *revision,
                queue_session_id.as_deref(),
                &now,
            )?;
            let (content, reasoning) = interrupted_projection_locked(&tx, turn_id, *revision)?;
            let turn_affected = tx.execute(
                "UPDATE turns SET assistant_content = ?1, assistant_reasoning = ?2,
                        assistant_timestamp = ?3, status = 'interrupted'
                 WHERE turn_id = ?4 AND revision = ?5 AND status = 'running'",
                params![content, reasoning, now, turn_id, revision],
            )?;
            if turn_affected == 1 {
                bump_completion_seq_locked(&tx, turn_id)?;
                tx.execute(
                    "UPDATE turn_journal_segments
                     SET status = 'interrupted', finished_at = ?1
                     WHERE turn_id = ?2 AND revision = ?3 AND status = 'running'",
                    params![now, turn_id, revision],
                )?;
                recoveries.push(StaleTurnRecovery {
                    turn_id: turn_id.clone(),
                    session_id: session_id.clone(),
                    restored_redo: false,
                });
            }
        }
        tx.commit()?;
        Ok(recoveries)
    }

    /// 记下该回合最后一次请求的上下文占用(供应商真实计数)。None = 未知,
    /// 此时上下文表继续用本地估算。
    pub fn set_turn_context_end(&self, turn_id: &str, tokens: Option<u64>) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE turns SET token_context_end = ?1 WHERE turn_id = ?2",
            params![tokens.map(|value| value as i64), turn_id],
        )?;
        Ok(())
    }

    /// 最新一条可见回合的上下文锚点,且仅当它是「已完成的普通回合 + 真实
    /// (非估算)用量」时才算数。摘要行在尾(刚压完)、被打断的回合、估算用量
    /// 一律返回 None —— 那些位置的数字不代表下一次请求的前缀大小。
    pub fn load_context_anchor(&self, session_id: &str) -> Result<Option<ContextAnchor>> {
        let conn = self.conn.lock().unwrap();
        let row = conn
            .query_row(
                "SELECT turn_id, assistant_provider_id, assistant_model, token_context_end,
                        token_usage_estimated, is_summary, status
                   FROM turns
                  WHERE session_id = ?1 AND hidden = 0
                  ORDER BY seq DESC LIMIT 1",
                params![session_id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, Option<String>>(1)?,
                        row.get::<_, Option<String>>(2)?,
                        row.get::<_, Option<i64>>(3)?,
                        row.get::<_, i64>(4)?,
                        row.get::<_, i64>(5)?,
                        row.get::<_, String>(6)?,
                    ))
                },
            )
            .optional()?;
        let Some((turn_id, provider_id, model, tokens, estimated, is_summary, status)) = row else {
            return Ok(None);
        };
        if estimated != 0 || is_summary != 0 || status != "completed" {
            return Ok(None);
        }
        let tokens = match tokens {
            Some(value) if value > 0 => value as u64,
            _ => return Ok(None),
        };
        Ok(Some(ContextAnchor {
            turn_id,
            provider_id,
            model,
            tokens,
        }))
    }
}
