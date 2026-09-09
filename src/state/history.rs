//! 历史读取、压缩与重做。
//!
//! 压缩是**可撤销**的：`replace_visible_with_summary` 只是把旧回合标记为不可
//! 见，`undo_last_turn` 能一层层退回去。所以这里从不真删历史，除非用户明确
//! 要求。
//!
//! `reset_if_prompt_changed` 是启动时的检查：提示词变了意味着上下文的前提变
//! 了，但**指纹变化绝不能删历史**（`prompt_fingerprint_changes_never_delete_history`
//! 守着这条）——它只影响要不要复用旧上下文。

use crate::state::*;

impl StateStore {
    pub fn background_report_replies_after(
        &self,
        session_id: &str,
        after_seq: i64,
    ) -> Result<Vec<(i64, String, String, String)>> {
        self.conv_db
            .background_report_replies_after(session_id, after_seq)
    }

    pub fn oldest_visible_turn_timestamp(&self, excluding_turn_id: &str) -> Result<Option<String>> {
        self.conv_db
            .oldest_visible_turn_timestamp(&self.session(), excluding_turn_id)
    }

    pub fn reset_if_prompt_changed(&self, system_prompt: &str) -> Result<()> {
        self.reset_if_prompt_changed_with_compatible(system_prompt, None)
    }

    pub(crate) fn reset_if_prompt_changed_with_compatible(
        &self,
        system_prompt: &str,
        // Kept for call-site compatibility; since the v7 no-delete semantics
        // every previous prompt is effectively compatible.
        _compatible_previous_prompt: Option<&str>,
    ) -> Result<()> {
        self.init_files()?;
        let fingerprint = prompt_fingerprint(system_prompt);
        let file = self.prompt_fingerprint_file();
        if let Some(parent) = file.parent() {
            std::fs::create_dir_all(parent)?;
        }
        if !file.exists() && self.state_dir.join("prompt.sha256").exists() {
            std::fs::write(file, format!("{fingerprint}\n"))?;
            return Ok(());
        }
        let previous = std::fs::read_to_string(&file).unwrap_or_default();
        if previous.trim() != fingerprint {
            // v7 Release 3: a persona prompt text change is a planned cache
            // cold start, not a reason to destroy data. Earlier versions
            // physically deleted every turn and the session's artifacts here,
            // which meant "upgrade the binary → conversations silently
            // vanish". History and artifacts are kept; only the fingerprint
            // advances. Users who want a clean slate still have /clear.
            tracing::info!(
                "persona prompt fingerprint changed; keeping session history (cache cold start)"
            );
            self.clear_last_usage()?;
            std::fs::write(file, format!("{fingerprint}\n"))?;
        }
        Ok(())
    }

    pub fn load_session_loaded_tools(&self) -> Result<BTreeSet<String>> {
        self.conv_db
            .load_session_loaded_items(&self.session(), "tool")
    }

    pub fn load_session_loaded_tools_with_sources(&self) -> Result<Vec<(String, Option<String>)>> {
        self.conv_db
            .load_session_loaded_items_with_sources(&self.session(), "tool")
    }

    pub fn add_session_loaded_tools(
        &self,
        names: &[String],
        source_turn_id: Option<&str>,
    ) -> Result<()> {
        self.conv_db
            .add_session_loaded_items(&self.session(), "tool", names, source_turn_id)?;
        Ok(())
    }

    pub fn add_session_loaded_targets(
        &self,
        names: &[String],
        source_turn_id: Option<&str>,
    ) -> Result<()> {
        self.conv_db
            .add_session_loaded_items(&self.session(), "target", names, source_turn_id)?;
        Ok(())
    }

    pub fn history(&self, limit: usize) -> Result<Vec<StoredConversationEntry>> {
        let turns = self
            .conv_db
            .load_turns(&self.session())?
            .into_iter()
            .filter(|turn| !turn.is_summary)
            .collect();
        let mut entries = turns_to_entries(turns);
        let start = entries.len().saturating_sub(limit);
        Ok(entries.split_off(start))
    }

    pub fn load_conversation(&self) -> Result<Vec<StoredConversationEntry>> {
        let turns = self
            .conv_db
            .load_turns(&self.session())?
            .into_iter()
            .filter(|turn| !turn.is_summary)
            .collect();
        Ok(turns_to_entries(turns))
    }

    #[allow(dead_code)]
    pub fn load_turns(&self) -> Result<Vec<Turn>> {
        self.conv_db.load_turns(&self.session())
    }

    #[allow(dead_code)]
    pub fn load_turns_excluding(&self, exclude_turn_id: &str) -> Result<Vec<Turn>> {
        self.conv_db
            .load_turns_excluding(&self.session(), exclude_turn_id)
    }

    pub fn load_visible_turns(&self) -> Result<Vec<Turn>> {
        self.conv_db.load_visible_turns(&self.session())
    }

    /// Display transcripts of this session's last `limit` turns, for redrawing
    /// a reopened REPL.
    pub fn session_replay(&self, limit: usize) -> Result<Vec<conversation_db::TurnReplay>> {
        self.conv_db.session_replay(&self.session(), limit)
    }

    pub fn load_visible_turns_excluding(&self, exclude_turn_id: &str) -> Result<Vec<Turn>> {
        self.conv_db
            .load_visible_turns_excluding(&self.session(), exclude_turn_id)
    }

    #[allow(dead_code)]
    pub fn hide_turns_before_seq(&self, seq: i64) -> Result<usize> {
        self.conv_db.hide_turns_before_seq(&self.session(), seq)
    }

    #[allow(dead_code)]
    pub fn insert_summary_turn(
        &self,
        summary: &str,
        tokens: TurnTokens,
        token_usage_estimated: bool,
    ) -> Result<()> {
        self.conv_db
            .insert_summary_turn(&self.session(), summary, tokens, token_usage_estimated)
    }

    pub fn load_last_summary(&self) -> Result<Option<Turn>> {
        self.conv_db.load_last_summary(&self.session())
    }

    pub fn replace_visible_with_summary(
        &self,
        fold_turn_ids: &[String],
        visible_turn_ids: &[String],
        summary: &str,
        tokens: TurnTokens,
        token_usage_estimated: bool,
        footprint_json: Option<&str>,
        extras_json: Option<&str>,
    ) -> Result<()> {
        self.conv_db.replace_visible_with_summary(
            &self.session(),
            fold_turn_ids,
            visible_turn_ids,
            summary,
            tokens,
            token_usage_estimated,
            footprint_json,
            extras_json,
        )
    }

    pub fn load_summary_extras_json(&self, turn_id: &str) -> Result<Option<String>> {
        self.conv_db
            .load_summary_extras_json(&self.session(), turn_id)
    }

    pub fn oldest_evictable_visible_turns(&self, count: usize) -> Result<Vec<Turn>> {
        self.conv_db
            .oldest_evictable_visible_turns(&self.session(), count)
    }

    pub fn delete_visible_turns(&self, turn_ids: &[String]) -> Result<usize> {
        self.conv_db.delete_visible_turns(&self.session(), turn_ids)
    }

    pub fn delete_visible_turns_checked(
        &self,
        turn_ids: &[String],
        expected_loaded_tools: Option<&[(String, Option<String>)]>,
    ) -> Result<usize> {
        self.conv_db
            .delete_visible_turns_checked(&self.session(), turn_ids, expected_loaded_tools)
    }

    pub fn archive_and_delete_visible_turns(
        &self,
        archive_db: &Path,
        turns: &[EvictedTurn],
        turn_ids: &[String],
        expected_loaded_tools: Option<&[(String, Option<String>)]>,
    ) -> Result<usize> {
        self.conv_db.archive_and_delete_visible_turns(
            &self.session(),
            archive_db,
            turns,
            turn_ids,
            expected_loaded_tools,
        )
    }

    // ---- 赞助记账 ----
    //
    // 纯转发。真正的 SQL 在 `conversation_db/sponsors.rs`；这里只是让平台工具
    // 和 dashboard 都从 StateStore 这一个门进去。

    pub fn add_sponsor_record(
        &self,
        new: &crate::state::NewSponsorRecord,
    ) -> Result<crate::state::SponsorRecord> {
        self.conv_db.add_sponsor_record(new)
    }

    pub fn sponsor_record(&self, record_id: i64) -> Result<Option<crate::state::SponsorRecord>> {
        self.conv_db.sponsor_record(record_id)
    }

    pub fn sponsor_records(
        &self,
        limit: usize,
        offset: usize,
    ) -> Result<Vec<crate::state::SponsorRecord>> {
        self.conv_db.sponsor_records(limit, offset)
    }

    pub fn sponsor_records_for(
        &self,
        sponsor_id: &str,
        limit: usize,
    ) -> Result<Vec<crate::state::SponsorRecord>> {
        self.conv_db.sponsor_records_for(sponsor_id, limit)
    }

    pub fn update_sponsor_record(
        &self,
        record_id: i64,
        note: Option<&str>,
        sponsor_name: Option<&str>,
    ) -> Result<Option<crate::state::SponsorRecord>> {
        self.conv_db
            .update_sponsor_record(record_id, note, sponsor_name)
    }

    pub fn delete_sponsor_record(&self, record_id: i64) -> Result<bool> {
        self.conv_db.delete_sponsor_record(record_id)
    }

    pub fn sponsor_totals(
        &self,
        order: crate::state::SponsorOrder,
        limit: usize,
    ) -> Result<Vec<crate::state::SponsorTotal>> {
        self.conv_db.sponsor_totals(order, limit)
    }

    pub fn sponsor_summary(&self) -> Result<crate::state::SponsorSummary> {
        self.conv_db.sponsor_summary()
    }

    // ---- 会话目标（goal）----
    //
    // 一律显式传 session_id 而不是用 `self.session()`：续轮驱动器在 daemon 的
    // 后台任务里跑，它推的会话未必是当前会话。

    pub fn goal(&self, session_id: &str) -> Result<Option<crate::state::GoalRecord>> {
        self.conv_db.goal(session_id)
    }

    pub fn create_goal(
        &self,
        session_id: &str,
        objective: &str,
        max_rounds: Option<i64>,
    ) -> Result<crate::state::GoalRecord> {
        self.conv_db.create_goal(session_id, objective, max_rounds)
    }

    pub fn edit_goal(
        &self,
        session_id: &str,
        goal_id: &str,
        revision: i64,
        objective: Option<&str>,
        max_rounds: Option<i64>,
    ) -> Result<crate::state::GoalRecord> {
        self.conv_db
            .edit_goal(session_id, goal_id, revision, objective, max_rounds)
    }

    pub fn pause_goal(
        &self,
        session_id: &str,
        goal_id: &str,
        revision: i64,
    ) -> Result<crate::state::GoalRecord> {
        self.conv_db.pause_goal(session_id, goal_id, revision)
    }

    pub fn resume_goal(
        &self,
        session_id: &str,
        goal_id: &str,
        revision: i64,
    ) -> Result<crate::state::GoalRecord> {
        self.conv_db.resume_goal(session_id, goal_id, revision)
    }

    pub fn complete_goal(
        &self,
        session_id: &str,
        goal_id: &str,
        revision: i64,
    ) -> Result<crate::state::GoalRecord> {
        self.conv_db.complete_goal(session_id, goal_id, revision)
    }

    pub fn block_goal(
        &self,
        session_id: &str,
        goal_id: &str,
        revision: i64,
        code: &str,
        message: &str,
    ) -> Result<crate::state::GoalRecord> {
        self.conv_db
            .block_goal(session_id, goal_id, revision, code, message)
    }

    pub fn clear_goal(&self, session_id: &str, goal_id: &str, revision: i64) -> Result<()> {
        self.conv_db.clear_goal(session_id, goal_id, revision)
    }

    pub fn begin_goal_round(
        &self,
        session_id: &str,
        goal_id: &str,
        revision: i64,
        expected_round: i64,
    ) -> Result<crate::state::GoalRecord> {
        self.conv_db
            .begin_goal_round(session_id, goal_id, revision, expected_round)
    }

    pub fn reset_conversation(&self) -> Result<()> {
        self.clear_session_content()?;
        usage::reset_conversation(&self.usage_file())
    }

    pub fn reset_persona_contexts(&self, persona: &str, platform: &str) -> Result<Vec<String>> {
        let session_ids = self.conv_db.reset_persona_contexts(persona, platform)?;
        self.remove_artifact_session_dirs(&session_ids)?;
        Ok(session_ids)
    }

    /// Clears only the pinned session's conversation state. Platform commands
    /// use this instead of `reset_conversation` so they cannot reset the
    /// daemon-wide usage counters or another client's current session.
    pub fn clear_session_content(&self) -> Result<()> {
        let session_id = self.session();
        self.conv_db.reset(&session_id)?;
        self.remove_artifact_session_dir(&session_id)
    }

    pub fn undo_last_turn(&self) -> Result<(usize, Option<String>)> {
        self.conv_db.undo_last_turn(&self.session())
    }

    pub fn redo_candidate(&self) -> Result<Option<RedoCandidate>> {
        self.conv_db.redo_candidate(&self.session())
    }

    pub fn load_redo_batch_prompts(
        &self,
        turn_id: &str,
        prompt_ids: &[String],
    ) -> Result<Vec<QueuedPrompt>> {
        self.conv_db
            .load_redo_batch_prompts(&self.session(), turn_id, prompt_ids)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn begin_redo(
        &self,
        turn_id: &str,
        input_id: &str,
        input_kind: RedoInputKind,
        expected_revision: i64,
        content: &str,
        display_content: &str,
        owner_pid: u32,
    ) -> Result<RedoStart> {
        self.conv_db.begin_redo(
            &self.session(),
            turn_id,
            input_id,
            input_kind,
            expected_revision,
            content,
            display_content,
            owner_pid,
            &self.queue_session_id,
        )
    }
}
