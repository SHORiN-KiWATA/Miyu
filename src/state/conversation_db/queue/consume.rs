//! 出队与清理。
//!
//! 消费队列必须和回合状态在同一个事务里改，否则会出现「消息出队了但回合没接
//! 住」——那条消息就永远丢了。
//!
//! `discard_stale_queued_prompts` 清的是「进程没了但队列还在」的残留，判据是
//! 会话的活跃标记，不是时间。

use crate::state::conversation_db::*;

impl ConversationDb {
    pub fn consume_queued_prompts(
        &self,
        session_id: &str,
        turn_id: &str,
        prompts: &[(String, String)],
        preceding_assistant_content: Option<&str>,
        preceding_assistant_reasoning: Option<&str>,
        preceding_assistant_provider_id: Option<&str>,
        preceding_assistant_model: Option<&str>,
        queue_session_id: &str,
    ) -> Result<()> {
        let prompts = prompts
            .iter()
            .map(|(prompt_id, content)| (prompt_id.clone(), content.clone(), "[]".to_string()))
            .collect::<Vec<_>>();
        self.consume_queued_prompts_with_checkpoint(
            session_id,
            turn_id,
            &prompts,
            preceding_assistant_content,
            preceding_assistant_reasoning,
            preceding_assistant_provider_id,
            preceding_assistant_model,
            queue_session_id,
            None,
        )
    }

    /// `prompts` = (prompt_id, 发出去的正文, 随发的瞬态尾巴 JSON)。尾巴要和
    /// 正文一起落库:回放少了它,活体与化石字节不同,缓存前缀与 CLI 续传链
    /// 都在 followup 处掰断(09-04 codex 线实证)。
    #[allow(clippy::too_many_arguments)]
    pub fn consume_queued_prompts_with_checkpoint(
        &self,
        session_id: &str,
        turn_id: &str,
        prompts: &[(String, String, String)],
        preceding_assistant_content: Option<&str>,
        preceding_assistant_reasoning: Option<&str>,
        preceding_assistant_provider_id: Option<&str>,
        preceding_assistant_model: Option<&str>,
        queue_session_id: &str,
        mut checkpoint: Option<TurnRedoCheckpointPayload>,
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
        if let Some(checkpoint) = checkpoint.as_mut() {
            checkpoint.prefix_question_count = tx.query_row(
                "SELECT COUNT(*) FROM question_exchanges WHERE turn_id = ?1",
                params![turn_id],
                |row| row.get::<_, i64>(0),
            )? as usize;
            checkpoint.prefix_image_asset_ids = {
                let mut stmt = tx.prepare(
                    "SELECT asset_id FROM image_assets WHERE turn_id = ?1 ORDER BY created_at, asset_id",
                )?;
                let rows = stmt
                    .query_map(params![turn_id], |row| row.get::<_, String>(0))?
                    .collect::<std::result::Result<Vec<_>, _>>()?;
                rows
            };
            checkpoint.prefix_artifact_asset_ids = {
                let mut stmt = tx.prepare(
                    "SELECT asset_id FROM artifact_assets
                     WHERE turn_id = ?1 ORDER BY updated_at, asset_id",
                )?;
                let rows = stmt
                    .query_map(params![turn_id], |row| row.get::<_, String>(0))?
                    .collect::<std::result::Result<Vec<_>, _>>()?;
                rows
            };
            checkpoint.loaded_items = {
                let mut stmt = tx.prepare(
                    "SELECT kind, name, source_turn_id FROM session_loaded_items
                     WHERE session_id = ?1 ORDER BY kind, name",
                )?;
                let rows = stmt
                    .query_map(params![session_id], |row| {
                        Ok((row.get(0)?, row.get(1)?, row.get(2)?))
                    })?
                    .collect::<std::result::Result<Vec<_>, _>>()?;
                rows
            };
        }
        let consumed_at = Utc::now().to_rfc3339();
        for (index, (prompt_id, context_content, context_messages)) in prompts.iter().enumerate() {
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
                      preceding_assistant_model = ?7,
                      context_messages = ?11
                   WHERE prompt_id = ?8 AND status = 'queued' AND session_id = ?9
                     AND queue_session_id = ?10",
                params![
                    consumed_at,
                    turn_id,
                    context_content,
                    preceding_content,
                    preceding_reasoning,
                    preceding_assistant_provider_id,
                    preceding_assistant_model,
                    prompt_id,
                    session_id,
                    queue_session_id,
                    context_messages
                ],
            )?;
            if affected != 1 {
                bail!("queued prompt changed before it could be consumed: {prompt_id}");
            }
        }
        let batch_prompt_ids = prompts
            .iter()
            .map(|(prompt_id, _, _)| prompt_id.as_str())
            .collect::<Vec<_>>();
        let batch_prompt_ids = serde_json::to_string(&batch_prompt_ids)?;
        let (payload, unavailable_reason) = match checkpoint {
            Some(checkpoint) => {
                let payload = serde_json::to_vec(&checkpoint)?;
                if payload.len() <= MAX_REDO_CHECKPOINT_BYTES {
                    (Some(payload), None)
                } else {
                    (
                        None,
                        Some(format!(
                            "replay checkpoint exceeds the {} byte limit",
                            MAX_REDO_CHECKPOINT_BYTES
                        )),
                    )
                }
            }
            None => (None, Some("replay checkpoint was not captured".to_string())),
        };
        tx.execute(
            "INSERT INTO turn_redo_checkpoints
                (turn_id, version, batch_prompt_ids, payload, unavailable_reason, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(turn_id) DO UPDATE SET
                version = excluded.version,
                batch_prompt_ids = excluded.batch_prompt_ids,
                payload = excluded.payload,
                unavailable_reason = excluded.unavailable_reason,
                created_at = excluded.created_at",
            params![
                turn_id,
                REDO_CHECKPOINT_VERSION,
                batch_prompt_ids,
                payload,
                unavailable_reason,
                consumed_at
            ],
        )?;
        let revision: i64 = tx.query_row(
            "SELECT revision FROM turns WHERE turn_id = ?1 AND status = 'running'",
            params![turn_id],
            |row| row.get(0),
        )?;
        tx.execute(
            "INSERT OR IGNORE INTO turn_journal_segments
                (turn_id, revision, segment_index, status, started_at)
             VALUES (?1, ?2, 0, 'running', ?3)",
            params![turn_id, revision, consumed_at],
        )?;
        let (segment_index, segment_status): (i64, String) = tx.query_row(
            "SELECT segment_index, status FROM turn_journal_segments
             WHERE turn_id = ?1 AND revision = ?2
             ORDER BY segment_index DESC LIMIT 1",
            params![turn_id, revision],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        let next_segment = segment_index.saturating_add(1);
        let prompt_payload =
            serde_json::to_string(&prompts.iter().map(|(id, _, _)| id).collect::<Vec<_>>())?;
        if segment_status == "superseded" {
            tx.execute(
                "INSERT INTO turn_journal_segments
                    (turn_id, revision, segment_index, status, started_at)
                 VALUES (?1, ?2, ?3, 'running', ?4)",
                params![turn_id, revision, next_segment, consumed_at],
            )?;
            tx.execute(
                "INSERT INTO turn_journal_events
                    (turn_id, revision, segment_index, kind, text_payload, created_at)
                 VALUES (?1, ?2, ?3, 'queued_prompts_consumed', ?4, ?5)",
                params![turn_id, revision, next_segment, prompt_payload, consumed_at],
            )?;
        } else {
            tx.execute(
                "INSERT INTO turn_journal_events
                    (turn_id, revision, segment_index, kind, text_payload, created_at)
                 VALUES (?1, ?2, ?3, 'queued_prompts_consumed', ?4, ?5)",
                params![
                    turn_id,
                    revision,
                    segment_index,
                    prompt_payload,
                    consumed_at
                ],
            )?;
        }
        if segment_status == "running" {
            tx.execute(
                "UPDATE turn_journal_segments
                 SET status = 'completed', finished_at = ?1
                 WHERE turn_id = ?2 AND revision = ?3 AND segment_index = ?4",
                params![consumed_at, turn_id, revision, segment_index],
            )?;
        }
        if segment_status != "superseded" {
            tx.execute(
                "INSERT INTO turn_journal_segments
                    (turn_id, revision, segment_index, status, started_at)
                 VALUES (?1, ?2, ?3, 'running', ?4)",
                params![turn_id, revision, next_segment, consumed_at],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn discard_queued_prompts(
        &self,
        session_id: &str,
        queue_session_id: &str,
    ) -> Result<usize> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let turn = tx
            .query_row(
                "SELECT turn_id, status, revision, assistant_content, assistant_reasoning
                 FROM turns
                 WHERE session_id = ?1 AND queue_session_id = ?2
                 ORDER BY seq DESC LIMIT 1",
                params![session_id, queue_session_id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, i64>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, Option<String>>(4)?,
                    ))
                },
            )
            .optional()?;
        let Some((turn_id, status, revision, assistant_content, assistant_reasoning)) = turn else {
            let deleted = tx.execute(
                "DELETE FROM queued_prompts
                 WHERE status = 'queued' AND session_id = ?1 AND queue_session_id = ?2",
                params![session_id, queue_session_id],
            )?;
            tx.commit()?;
            return Ok(deleted);
        };
        if status == "running" {
            let deleted = tx.execute(
                "DELETE FROM queued_prompts
                 WHERE status = 'queued' AND session_id = ?1 AND queue_session_id = ?2",
                params![session_id, queue_session_id],
            )?;
            tx.commit()?;
            return Ok(deleted);
        }

        let now = Utc::now().to_rfc3339();
        let preceding_content = if status == "interrupted" {
            interrupted_prefix(&assistant_content)
        } else {
            assistant_content
        };
        let preceding_content = (!preceding_content.trim().is_empty()).then_some(preceding_content);
        let mut stmt = tx.prepare(
            "SELECT prompt_id FROM queued_prompts
             WHERE status = 'queued' AND session_id = ?1 AND queue_session_id = ?2
             ORDER BY seq",
        )?;
        let prompt_ids = stmt
            .query_map(params![session_id, queue_session_id], |row| {
                row.get::<_, String>(0)
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        drop(stmt);
        for (index, prompt_id) in prompt_ids.iter().enumerate() {
            tx.execute(
                "UPDATE queued_prompts
                 SET status = 'consumed', consumed_at = ?1, turn_id = ?2,
                     context_content = content,
                     preceding_assistant_content = ?3,
                     preceding_assistant_reasoning = ?4
                 WHERE prompt_id = ?5 AND status = 'queued'",
                params![
                    now,
                    turn_id,
                    (index == 0)
                        .then_some(preceding_content.as_deref())
                        .flatten(),
                    (index == 0)
                        .then_some(assistant_reasoning.as_deref())
                        .flatten(),
                    prompt_id,
                ],
            )?;
        }
        if status == "interrupted" && !prompt_ids.is_empty() {
            let next_segment: i64 = tx.query_row(
                "SELECT COALESCE(MAX(segment_index), -1) + 1
                 FROM turn_journal_segments WHERE turn_id = ?1 AND revision = ?2",
                params![turn_id, revision],
                |row| row.get(0),
            )?;
            tx.execute(
                "INSERT INTO turn_journal_segments
                    (turn_id, revision, segment_index, status, started_at, finished_at)
                 VALUES (?1, ?2, ?3, 'interrupted', ?4, ?4)",
                params![turn_id, revision, next_segment, now],
            )?;
            tx.execute(
                "INSERT INTO turn_journal_events
                    (turn_id, revision, segment_index, kind, text_payload, created_at)
                 VALUES (?1, ?2, ?3, 'queued_prompts_consumed', ?4, ?5)",
                params![
                    turn_id,
                    revision,
                    next_segment,
                    serde_json::to_string(&prompt_ids)?,
                    now
                ],
            )?;
        }
        tx.commit()?;
        Ok(prompt_ids.len())
    }

    pub fn remove_queued_prompt(
        &self,
        session_id: &str,
        prompt_id: &str,
        queue_session_id: &str,
    ) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        Ok(conn.execute(
            "DELETE FROM queued_prompts
             WHERE prompt_id = ?1 AND status = 'queued' AND session_id = ?2
               AND queue_session_id = ?3",
            params![prompt_id, session_id, queue_session_id],
        )? == 1)
    }

    /// Hard-drop every still-queued prompt of a queue session and return
    /// their ids. Unlike `discard_queued_prompts` this never folds prompts
    /// into the conversation: it backs an explicit user cancel, where the
    /// queued follow-ups are withdrawn rather than preserved as context.
    pub fn delete_queued_prompts(
        &self,
        session_id: &str,
        queue_session_id: &str,
    ) -> Result<Vec<String>> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let prompt_ids = {
            let mut stmt = tx.prepare(
                "SELECT prompt_id FROM queued_prompts
                 WHERE status = 'queued' AND session_id = ?1 AND queue_session_id = ?2
                 ORDER BY seq",
            )?;
            let prompt_ids = stmt
                .query_map(params![session_id, queue_session_id], |row| {
                    row.get::<_, String>(0)
                })?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            prompt_ids
        };
        if !prompt_ids.is_empty() {
            tx.execute(
                "DELETE FROM queued_prompts
                 WHERE status = 'queued' AND session_id = ?1 AND queue_session_id = ?2",
                params![session_id, queue_session_id],
            )?;
        }
        tx.commit()?;
        Ok(prompt_ids)
    }

    pub fn discard_stale_queued_prompts(
        &self,
        current_session_id: &str,
        _current_pid: u32,
    ) -> Result<usize> {
        let mut conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT q.prompt_id, q.queue_session_id, q.owner_pid,
                    EXISTS(
                        SELECT 1 FROM turns t
                        WHERE t.status = 'running'
                          AND t.queue_session_id = q.queue_session_id
                    )
             FROM queued_prompts q WHERE q.status = 'queued'",
        )?;
        let queued_prompts = stmt
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, Option<i64>>(2)?,
                    row.get::<_, bool>(3)?,
                ))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        let stale_prompt_ids = queued_prompts
            .into_iter()
            .filter_map(|row| {
                let (prompt_id, session_id, owner_pid, belongs_to_running_turn) = row;
                if session_id.as_deref() == Some(current_session_id) {
                    return None;
                }
                if belongs_to_running_turn {
                    return None;
                }
                let owner_pid = owner_pid.and_then(|pid| u32::try_from(pid).ok());
                // Multiple stores in the daemon share a PID. A different
                // queue identity owned by this live process may belong to an
                // active parent turn, so only dead owners are stale here.
                let stale =
                    session_id.is_none() || !owner_pid.is_some_and(crate::alarm::process_exists);
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
}
