mod conversation_db;
mod usage;

use crate::llm::Usage;
use crate::memory::EvictedTurn;
use crate::paths::MiyuPaths;
use anyhow::Result;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

#[allow(unused_imports)]
pub use conversation_db::{
    interrupted_text, pending_placeholder, ConversationDb, Turn, TurnStatus,
};
pub use usage::UsageSnapshot;

#[derive(Debug, Clone)]
pub struct StateStore {
    state_dir: PathBuf,
    conv_db: Arc<ConversationDb>,
}

impl StateStore {
    pub fn new(paths: &MiyuPaths) -> Result<Self> {
        let state_dir = paths.state_dir.clone();
        let conv_db = Arc::new(ConversationDb::open(&state_dir)?);
        Ok(Self { state_dir, conv_db })
    }

    pub fn init_files(&self) -> Result<()> {
        std::fs::create_dir_all(&self.state_dir)?;
        if !self.usage_file().exists() {
            std::fs::write(self.usage_file(), "{\n  \"requests\": 0,\n  \"prompt_tokens\": 0,\n  \"completion_tokens\": 0,\n  \"total_tokens\": 0\n}\n")?;
        }
        if !self.profile_file().exists() {
            std::fs::write(self.profile_file(), "# Miyu Profile\n\n")?;
        }
        Ok(())
    }

    pub fn reset_if_prompt_changed(&self, system_prompt: &str) -> Result<()> {
        self.init_files()?;
        let fingerprint = prompt_fingerprint(system_prompt);
        let file = self.prompt_fingerprint_file();
        let previous = std::fs::read_to_string(&file).unwrap_or_default();
        if previous.trim() != fingerprint {
            self.conv_db.reset()?;
            self.clear_last_usage()?;
            std::fs::write(file, format!("{fingerprint}\n"))?;
        }
        Ok(())
    }

    #[allow(dead_code)]
    pub fn conv_db(&self) -> &ConversationDb {
        &self.conv_db
    }

    pub fn start_turn(&self, turn_id: &str, user_content: &str, owner_pid: u32) -> Result<()> {
        self.conv_db.start_turn(turn_id, user_content, owner_pid)
    }

    #[allow(dead_code)]
    pub fn complete_turn(
        &self,
        turn_id: &str,
        content: &str,
        reasoning: Option<&str>,
    ) -> Result<()> {
        self.conv_db.complete_turn(turn_id, content, reasoning)
    }

    pub fn interrupt_turn(&self, turn_id: &str) -> Result<()> {
        self.conv_db.interrupt_turn(turn_id)
    }

    pub fn complete_turn_with_usage(
        &self,
        turn_id: &str,
        content: &str,
        reasoning: Option<&str>,
        token_total: Option<u64>,
        token_usage_estimated: bool,
    ) -> Result<()> {
        self.conv_db.complete_turn_with_usage(
            turn_id,
            content,
            reasoning,
            token_total,
            token_usage_estimated,
        )
    }

    pub fn append_persisted_context(&self, turn_id: &str, report: &str) -> Result<()> {
        self.conv_db.append_tool_report(turn_id, report.trim())
    }

    pub fn append_question_exchange(
        &self,
        turn_id: &str,
        exchange: &crate::question::QuestionExchange,
    ) -> Result<()> {
        self.conv_db.append_question_exchange(turn_id, exchange)
    }

    pub fn load_session_loaded_tools(&self) -> Result<BTreeSet<String>> {
        self.conv_db.load_session_loaded_items("tool")
    }

    pub fn load_session_loaded_tools_with_sources(&self) -> Result<Vec<(String, Option<String>)>> {
        self.conv_db.load_session_loaded_items_with_sources("tool")
    }

    pub fn add_session_loaded_tools(
        &self,
        names: &[String],
        source_turn_id: Option<&str>,
    ) -> Result<()> {
        self.conv_db
            .add_session_loaded_items("tool", names, source_turn_id)?;
        Ok(())
    }

    pub fn add_session_loaded_targets(
        &self,
        names: &[String],
        source_turn_id: Option<&str>,
    ) -> Result<()> {
        self.conv_db
            .add_session_loaded_items("target", names, source_turn_id)?;
        Ok(())
    }

    pub fn recover_stale_turns(&self) -> Result<usize> {
        self.conv_db.recover_stale_running_turns()
    }

    pub fn history(&self, limit: usize) -> Result<Vec<StoredConversationEntry>> {
        let turns = self
            .conv_db
            .load_turns()?
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
            .load_turns()?
            .into_iter()
            .filter(|turn| !turn.is_summary)
            .collect();
        Ok(turns_to_entries(turns))
    }

    #[allow(dead_code)]
    pub fn load_turns(&self) -> Result<Vec<Turn>> {
        self.conv_db.load_turns()
    }

    #[allow(dead_code)]
    pub fn load_turns_excluding(&self, exclude_turn_id: &str) -> Result<Vec<Turn>> {
        self.conv_db.load_turns_excluding(exclude_turn_id)
    }

    pub fn load_visible_turns(&self) -> Result<Vec<Turn>> {
        self.conv_db.load_visible_turns()
    }

    pub fn load_visible_turns_excluding(&self, exclude_turn_id: &str) -> Result<Vec<Turn>> {
        self.conv_db.load_visible_turns_excluding(exclude_turn_id)
    }

    #[allow(dead_code)]
    pub fn hide_turns_before_seq(&self, seq: i64) -> Result<usize> {
        self.conv_db.hide_turns_before_seq(seq)
    }

    #[allow(dead_code)]
    pub fn insert_summary_turn(
        &self,
        summary: &str,
        token_total: Option<u64>,
        token_usage_estimated: bool,
    ) -> Result<()> {
        self.conv_db
            .insert_summary_turn(summary, token_total, token_usage_estimated)
    }

    pub fn load_last_summary(&self) -> Result<Option<Turn>> {
        self.conv_db.load_last_summary()
    }

    pub fn replace_visible_with_summary(
        &self,
        last_seq: i64,
        visible_turn_ids: &[String],
        summary: &str,
        token_total: Option<u64>,
        token_usage_estimated: bool,
    ) -> Result<()> {
        self.conv_db.replace_visible_with_summary(
            last_seq,
            visible_turn_ids,
            summary,
            token_total,
            token_usage_estimated,
        )
    }

    pub fn oldest_evictable_visible_turns(&self, count: usize) -> Result<Vec<Turn>> {
        self.conv_db.oldest_evictable_visible_turns(count)
    }

    pub fn delete_visible_turns(&self, turn_ids: &[String]) -> Result<usize> {
        self.conv_db.delete_visible_turns(turn_ids)
    }

    pub fn delete_visible_turns_checked(
        &self,
        turn_ids: &[String],
        expected_loaded_tools: Option<&[(String, Option<String>)]>,
    ) -> Result<usize> {
        self.conv_db
            .delete_visible_turns_checked(turn_ids, expected_loaded_tools)
    }

    pub fn archive_and_delete_visible_turns(
        &self,
        archive_db: &Path,
        turns: &[EvictedTurn],
        turn_ids: &[String],
        expected_loaded_tools: Option<&[(String, Option<String>)]>,
    ) -> Result<usize> {
        self.conv_db.archive_and_delete_visible_turns(
            archive_db,
            turns,
            turn_ids,
            expected_loaded_tools,
        )
    }

    pub fn reset_conversation(&self) -> Result<()> {
        self.conv_db.reset()?;
        self.clear_last_usage()
    }

    pub fn undo_last_turn(&self) -> Result<(usize, Option<String>)> {
        self.conv_db.undo_last_turn()
    }

    pub fn add_usage(&self, usage: &Usage) -> Result<()> {
        self.init_files()?;
        usage::add_usage(&self.usage_file(), usage)
    }

    pub fn add_auxiliary_usage(&self, usage: &Usage) -> Result<()> {
        self.init_files()?;
        usage::add_auxiliary_usage(&self.usage_file(), usage)
    }

    #[allow(dead_code)]
    pub fn usage_snapshot(&self) -> Result<UsageSnapshot> {
        usage::snapshot(&self.usage_file())
    }

    pub fn clear_last_usage(&self) -> Result<()> {
        usage::clear_last_usage(&self.usage_file())
    }

    #[allow(dead_code)]
    pub fn has_running_turns(&self) -> Result<bool> {
        self.conv_db.has_running_turns()
    }

    #[allow(dead_code)]
    pub fn running_turn_summaries(&self) -> Result<Vec<String>> {
        self.conv_db.running_turn_summaries()
    }

    #[allow(dead_code)]
    pub fn running_turn_summaries_excluding(&self, exclude_turn_id: &str) -> Result<Vec<String>> {
        self.conv_db
            .running_turn_summaries_excluding(exclude_turn_id)
    }

    #[allow(dead_code)]
    pub fn migrate_from_jsonl(&self) -> Result<usize> {
        let jsonl_path = self.conversation_file();
        self.conv_db.migrate_from_jsonl(&jsonl_path)
    }

    fn conversation_file(&self) -> PathBuf {
        self.state_dir.join("conversation.jsonl")
    }

    fn usage_file(&self) -> PathBuf {
        self.state_dir.join("usage.json")
    }

    fn profile_file(&self) -> PathBuf {
        self.state_dir.join("profile.md")
    }

    fn prompt_fingerprint_file(&self) -> PathBuf {
        self.state_dir.join("prompt.sha256")
    }
}

fn prompt_fingerprint(system_prompt: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(system_prompt.as_bytes());
    format!("{:x}", hasher.finalize())
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

fn turns_to_entries(turns: Vec<Turn>) -> Vec<StoredConversationEntry> {
    let mut entries = Vec::with_capacity(turns.len() * 3);
    for turn in turns {
        let ts = turn.assistant_timestamp.clone().unwrap_or_default();
        entries.push(StoredConversationEntry {
            timestamp: turn.user_timestamp,
            role: "user".to_string(),
            content: turn.user_content,
            reasoning: None,
        });
        for exchange in &turn.question_exchanges {
            entries.push(StoredConversationEntry {
                timestamp: exchange.answered_at.clone(),
                role: "assistant_clarification".to_string(),
                content: crate::question::assistant_exchange_text(exchange),
                reasoning: None,
            });
            entries.push(StoredConversationEntry {
                timestamp: exchange.answered_at.clone(),
                role: "user_clarification".to_string(),
                content: crate::question::user_exchange_text(exchange),
                reasoning: None,
            });
        }
        entries.push(StoredConversationEntry {
            timestamp: ts.clone(),
            role: "assistant".to_string(),
            content: turn.assistant_content,
            reasoning: turn.assistant_reasoning,
        });
        for report in turn.tool_reports {
            entries.push(StoredConversationEntry {
                timestamp: ts.clone(),
                role: "assistant".to_string(),
                content: report,
                reasoning: None,
            });
        }
    }
    entries
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct StoredConversationEntry {
    pub timestamp: String,
    pub role: String,
    pub content: String,
    #[serde(default)]
    pub reasoning: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn turn_lifecycle() {
        let temp = tempfile::tempdir().unwrap();
        let store = StateStore::new(&MiyuPaths {
            config_dir: temp.path().join("config"),
            config_file: temp.path().join("config/config.jsonc"),
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
        })
        .unwrap();

        store.init_files().unwrap();
        assert!(!temp.path().join("state/miyu.log").exists());

        store.start_turn("turn_1", "hello", 999999).unwrap();
        let turns = store.load_turns().unwrap();
        assert_eq!(turns.len(), 1);
        assert_eq!(turns[0].status, TurnStatus::Running);
        assert_eq!(turns[0].assistant_content, pending_placeholder());

        store.complete_turn("turn_1", "hi there", None).unwrap();
        let turns = store.load_turns().unwrap();
        assert_eq!(turns[0].status, TurnStatus::Completed);
        assert_eq!(turns[0].assistant_content, "hi there");
    }

    #[test]
    fn question_exchange_persists_with_user_role_history() {
        let temp = tempfile::tempdir().unwrap();
        let store = StateStore::new(&MiyuPaths {
            config_dir: temp.path().join("config"),
            config_file: temp.path().join("config/config.jsonc"),
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
        })
        .unwrap();
        store.start_turn("turn_1", "配置它", 999999).unwrap();
        let request = crate::question::QuestionRequest {
            questions: vec![crate::question::QuestionPrompt {
                header: "范围".to_string(),
                question: "修改哪些部分？".to_string(),
                options: vec![crate::question::QuestionOption {
                    label: "全部".to_string(),
                    description: String::new(),
                }],
                multiple: false,
                custom: true,
            }],
        };
        let exchange =
            crate::question::QuestionExchange::new(request, vec![vec!["全部".to_string()]])
                .unwrap();
        store.append_question_exchange("turn_1", &exchange).unwrap();
        store.complete_turn("turn_1", "已经配置。", None).unwrap();

        let turns = store.load_turns().unwrap();
        assert_eq!(turns[0].question_exchanges, vec![exchange]);
        let history = store.load_conversation().unwrap();
        assert_eq!(history[1].role, "assistant_clarification");
        assert_eq!(history[2].role, "user_clarification");
        assert!(history[2].content.contains("全部"));
    }

    #[test]
    fn interrupt_turn() {
        let temp = tempfile::tempdir().unwrap();
        let store = StateStore::new(&MiyuPaths {
            config_dir: temp.path().join("config"),
            config_file: temp.path().join("config/config.jsonc"),
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
        })
        .unwrap();

        store.start_turn("turn_1", "do something", 999999).unwrap();
        store.interrupt_turn("turn_1").unwrap();
        let turns = store.load_turns().unwrap();
        assert_eq!(turns[0].status, TurnStatus::Interrupted);
        assert_eq!(turns[0].assistant_content, interrupted_text());
    }

    #[test]
    fn recover_stale_running() {
        let temp = tempfile::tempdir().unwrap();
        let store = StateStore::new(&MiyuPaths {
            config_dir: temp.path().join("config"),
            config_file: temp.path().join("config/config.jsonc"),
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
        })
        .unwrap();

        store.start_turn("turn_1", "task a", 999999).unwrap();
        store.start_turn("turn_2", "task b", 999999).unwrap();
        assert!(store.has_running_turns().unwrap());

        let recovered = store.recover_stale_turns().unwrap();
        assert_eq!(recovered, 2);

        let turns = store.load_turns().unwrap();
        assert_eq!(turns.len(), 2);
        assert!(turns.iter().all(|t| t.status == TurnStatus::Interrupted));
    }

    #[test]
    fn recover_stale_skips_alive_owner() {
        let (_temp, store) = test_store();

        let current_pid = std::process::id();
        store
            .start_turn("turn_1", "终端1的prompt", current_pid)
            .unwrap();
        store.start_turn("turn_dead", "孤儿turn", 999999).unwrap();

        let recovered = store.recover_stale_turns().unwrap();
        assert_eq!(recovered, 1);

        let turns = store.load_turns().unwrap();
        let turn1 = turns.iter().find(|t| t.turn_id == "turn_1").unwrap();
        assert_eq!(turn1.status, TurnStatus::Running);
        assert_eq!(turn1.assistant_content, pending_placeholder());

        let dead = turns.iter().find(|t| t.turn_id == "turn_dead").unwrap();
        assert_eq!(dead.status, TurnStatus::Interrupted);
    }

    #[test]
    fn undo_removes_last_turn() {
        let temp = tempfile::tempdir().unwrap();
        let store = StateStore::new(&MiyuPaths {
            config_dir: temp.path().join("config"),
            config_file: temp.path().join("config/config.jsonc"),
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
        })
        .unwrap();

        store.start_turn("turn_1", "hello", 999999).unwrap();
        store.complete_turn("turn_1", "hi", None).unwrap();
        store.start_turn("turn_2", "bye", 999999).unwrap();
        store.complete_turn("turn_2", "goodbye", None).unwrap();

        let (removed, prompt) = store.undo_last_turn().unwrap();
        assert_eq!(removed, 1);
        assert_eq!(prompt.as_deref(), Some("bye"));

        let turns = store.load_turns().unwrap();
        assert_eq!(turns.len(), 1);
        assert_eq!(turns[0].turn_id, "turn_1");
    }

    fn test_store() -> (tempfile::TempDir, StateStore) {
        let temp = tempfile::tempdir().unwrap();
        let store = StateStore::new(&MiyuPaths {
            config_dir: temp.path().join("config"),
            config_file: temp.path().join("config/config.jsonc"),
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
        })
        .unwrap();
        (temp, store)
    }

    fn visible_snapshot(store: &StateStore) -> (i64, Vec<String>) {
        let turns = store.load_visible_turns().unwrap();
        let last_seq = turns.last().unwrap().seq;
        let turn_ids = turns.into_iter().map(|turn| turn.turn_id).collect();
        (last_seq, turn_ids)
    }

    #[test]
    fn hidden_turns_excluded_from_visible() {
        let (_temp, store) = test_store();
        store.start_turn("t1", "first", 999999).unwrap();
        store.complete_turn("t1", "reply1", None).unwrap();
        store.start_turn("t2", "second", 999999).unwrap();
        store.complete_turn("t2", "reply2", None).unwrap();

        let visible = store.load_visible_turns().unwrap();
        assert_eq!(visible.len(), 2);

        let hidden_count = store.hide_turns_before_seq(visible[0].seq).unwrap();
        assert_eq!(hidden_count, 1);

        let visible_after = store.load_visible_turns().unwrap();
        assert_eq!(visible_after.len(), 1);
        assert_eq!(visible_after[0].turn_id, "t2");

        let all = store.load_turns().unwrap();
        assert_eq!(all.len(), 2);
        assert!(all[0].hidden);
        assert!(!all[1].hidden);
    }

    #[test]
    fn summary_turn_insert_and_load() {
        let (_temp, store) = test_store();
        store.start_turn("t1", "hello", 999999).unwrap();
        store.complete_turn("t1", "hi", None).unwrap();

        store
            .insert_summary_turn("## Task Goal\nDo stuff", Some(12), true)
            .unwrap();

        let summary = store.load_last_summary().unwrap();
        assert!(summary.is_some());
        let summary = summary.unwrap();
        assert!(summary.is_summary);
        assert!(!summary.hidden);
        assert_eq!(summary.assistant_content, "## Task Goal\nDo stuff");
        assert_eq!(summary.token_total, 12);
        assert!(summary.token_usage_estimated);

        let visible = store.load_visible_turns().unwrap();
        assert_eq!(visible.len(), 2);
        assert!(visible.iter().any(|t| t.is_summary));
        assert!(visible.iter().any(|t| !t.is_summary));
    }

    #[test]
    fn session_loaded_tools_persist_until_reset() {
        let (_temp, store) = test_store();
        store
            .add_session_loaded_tools(&["web_search".to_string()], Some("t1"))
            .unwrap();
        store
            .add_session_loaded_targets(&["group:gaming".to_string()], Some("t1"))
            .unwrap();

        let loaded = store.load_session_loaded_tools().unwrap();
        assert!(loaded.contains("web_search"));

        store.reset_conversation().unwrap();
        assert!(store.load_session_loaded_tools().unwrap().is_empty());
    }

    #[test]
    fn hide_before_seq_hides_old_summary_too() {
        let (_temp, store) = test_store();
        store.start_turn("t1", "old", 999999).unwrap();
        store.complete_turn("t1", "old reply", None).unwrap();
        store
            .insert_summary_turn("summary of old", Some(8), true)
            .unwrap();
        store.start_turn("t2", "new", 999999).unwrap();
        store.complete_turn("t2", "new reply", None).unwrap();

        let visible = store.load_visible_turns().unwrap();
        assert_eq!(visible.len(), 3);

        let t2_seq = visible.last().unwrap().seq;
        let hidden = store.hide_turns_before_seq(t2_seq).unwrap();
        assert_eq!(hidden, 3);

        let visible_after = store.load_visible_turns().unwrap();
        assert!(visible_after.is_empty());
    }

    #[test]
    fn evictable_turns_are_deleted_only_after_explicit_commit() {
        let (_temp, store) = test_store();
        for i in 0..10 {
            let id = format!("t{i}");
            let content = "x".repeat(1000);
            store.start_turn(&id, &content, 999999).unwrap();
            store.complete_turn(&id, &content, None).unwrap();
        }

        let evicted = store.oldest_evictable_visible_turns(3).unwrap();
        assert_eq!(evicted.len(), 3);
        assert_eq!(store.load_visible_turns().unwrap().len(), 10);

        let ids = evicted
            .iter()
            .map(|turn| turn.turn_id.clone())
            .collect::<Vec<_>>();
        store.delete_visible_turns(&ids).unwrap();

        let visible = store.load_visible_turns().unwrap();
        assert_eq!(visible.len(), 7);
    }

    #[test]
    fn deleting_no_visible_turns_is_a_noop() {
        let (_temp, store) = test_store();
        store.start_turn("t1", "short", 999999).unwrap();
        store.complete_turn("t1", "reply", None).unwrap();

        assert_eq!(store.delete_visible_turns(&[]).unwrap(), 0);

        let visible = store.load_visible_turns().unwrap();
        assert_eq!(visible.len(), 1);
    }

    #[test]
    fn deleting_visible_turns_rolls_back_when_any_id_changed() {
        let (_temp, store) = test_store();
        for id in ["t1", "t2"] {
            store.start_turn(id, id, 999999).unwrap();
            store.complete_turn(id, "reply", None).unwrap();
        }
        store
            .add_session_loaded_tools(&["from_t1".to_string()], Some("t1"))
            .unwrap();
        store
            .add_session_loaded_tools(&["from_t2".to_string()], Some("t2"))
            .unwrap();

        assert!(store
            .delete_visible_turns(&["t1".to_string(), "missing".to_string()])
            .is_err());
        assert_eq!(store.load_visible_turns().unwrap().len(), 2);
        assert_eq!(
            store.load_session_loaded_tools().unwrap(),
            BTreeSet::from(["from_t1".to_string(), "from_t2".to_string()])
        );
    }

    #[test]
    fn checked_pop_rolls_back_when_loaded_tool_sources_change() {
        let (_temp, store) = test_store();
        for id in ["t1", "t2"] {
            store.start_turn(id, id, 999999).unwrap();
            store.complete_turn(id, "reply", None).unwrap();
        }
        store
            .add_session_loaded_tools(&["dynamic_tool".to_string()], Some("t1"))
            .unwrap();
        let expected = store.load_session_loaded_tools_with_sources().unwrap();
        store
            .add_session_loaded_tools(&["dynamic_tool".to_string()], Some("t2"))
            .unwrap();

        assert!(store
            .delete_visible_turns_checked(&["t1".to_string()], Some(&expected))
            .is_err());

        assert_eq!(store.load_visible_turns().unwrap().len(), 2);
        assert_eq!(
            store.load_session_loaded_tools_with_sources().unwrap(),
            vec![("dynamic_tool".to_string(), Some("t2".to_string()))]
        );
    }

    #[test]
    fn deleting_visible_turns_unloads_only_items_sourced_from_deleted_turns() {
        let (_temp, store) = test_store();
        for id in ["t1", "t2"] {
            store.start_turn(id, id, 999999).unwrap();
            store.complete_turn(id, "reply", None).unwrap();
        }
        store
            .add_session_loaded_tools(&["popped_tool".to_string()], Some("t1"))
            .unwrap();
        store
            .add_session_loaded_tools(&["kept_tool".to_string()], Some("t2"))
            .unwrap();
        store
            .add_session_loaded_tools(&["global_tool".to_string()], None)
            .unwrap();
        store
            .add_session_loaded_targets(&["popped_target".to_string()], Some("t1"))
            .unwrap();
        store
            .add_session_loaded_targets(&["kept_target".to_string()], Some("t2"))
            .unwrap();

        assert_eq!(store.delete_visible_turns(&["t1".to_string()]).unwrap(), 1);

        assert_eq!(
            store.load_session_loaded_tools().unwrap(),
            BTreeSet::from(["global_tool".to_string(), "kept_tool".to_string()])
        );
        assert_eq!(
            store.conv_db.load_session_loaded_items("target").unwrap(),
            BTreeSet::from(["kept_target".to_string()])
        );
    }

    #[test]
    fn interrupted_turn_is_evictable_but_summary_and_running_turn_are_not() {
        let (_temp, store) = test_store();
        store
            .insert_summary_turn("summary", Some(1), false)
            .unwrap();
        store.start_turn("completed", "completed", 999999).unwrap();
        store.complete_turn("completed", "reply", None).unwrap();
        store
            .start_turn("interrupted", "interrupted", 999999)
            .unwrap();
        store.interrupt_turn("interrupted").unwrap();
        store
            .start_turn("running", "pending", std::process::id())
            .unwrap();

        let evicted = store.oldest_evictable_visible_turns(10).unwrap();
        assert_eq!(
            evicted
                .iter()
                .map(|turn| turn.turn_id.as_str())
                .collect::<Vec<_>>(),
            vec!["completed", "interrupted"]
        );
        assert_eq!(evicted[1].status, TurnStatus::Interrupted);
    }

    #[test]
    fn compact_is_reversible_with_undo() {
        let (_temp, store) = test_store();
        for id in ["t1", "t2"] {
            store.start_turn(id, id, 999999).unwrap();
            store.complete_turn(id, "reply", None).unwrap();
        }
        let (last_seq, turn_ids) = visible_snapshot(&store);

        store
            .replace_visible_with_summary(last_seq, &turn_ids, "summary", Some(10), true)
            .unwrap();

        let all = store.load_turns().unwrap();
        assert_eq!(all.len(), 3);
        assert!(all[0].hidden && all[1].hidden);
        assert_eq!(store.load_visible_turns().unwrap().len(), 1);
        assert_eq!(
            store
                .load_conversation()
                .unwrap()
                .into_iter()
                .filter(|entry| entry.role == "user")
                .map(|entry| entry.content)
                .collect::<Vec<_>>(),
            vec!["t1", "t2"]
        );

        let (removed, prompt) = store.undo_last_turn().unwrap();
        assert_eq!(removed, 1);
        assert!(prompt.is_none());
        let visible = store.load_visible_turns().unwrap();
        assert_eq!(
            visible
                .iter()
                .map(|turn| turn.turn_id.as_str())
                .collect::<Vec<_>>(),
            vec!["t1", "t2"]
        );
    }

    #[test]
    fn nested_compact_undo_restores_one_layer_at_a_time() {
        let (_temp, store) = test_store();
        for id in ["t1", "t2"] {
            store.start_turn(id, id, 999999).unwrap();
            store.complete_turn(id, "reply", None).unwrap();
        }
        let (last_seq, turn_ids) = visible_snapshot(&store);
        store
            .replace_visible_with_summary(last_seq, &turn_ids, "summary one", None, false)
            .unwrap();
        store.start_turn("t3", "third", 999999).unwrap();
        store.complete_turn("t3", "reply", None).unwrap();
        let (last_seq, turn_ids) = visible_snapshot(&store);
        store
            .replace_visible_with_summary(last_seq, &turn_ids, "summary two", None, false)
            .unwrap();

        assert_eq!(
            store
                .load_last_summary()
                .unwrap()
                .unwrap()
                .assistant_content,
            "summary two"
        );
        assert_eq!(store.undo_last_turn().unwrap(), (1, None));
        let visible = store.load_visible_turns().unwrap();
        assert_eq!(visible.len(), 2);
        assert_eq!(visible[0].assistant_content, "summary one");
        assert_eq!(visible[1].turn_id, "t3");

        assert_eq!(store.undo_last_turn().unwrap().1.as_deref(), Some("third"));
        assert_eq!(store.undo_last_turn().unwrap(), (1, None));
        let visible = store.load_visible_turns().unwrap();
        assert_eq!(
            visible
                .iter()
                .map(|turn| turn.turn_id.as_str())
                .collect::<Vec<_>>(),
            vec!["t1", "t2"]
        );
    }

    #[test]
    fn empty_summary_leaves_visible_turns_unchanged() {
        let (_temp, store) = test_store();
        store.start_turn("t1", "hello", 999999).unwrap();
        store.complete_turn("t1", "reply", None).unwrap();
        let (last_seq, turn_ids) = visible_snapshot(&store);

        assert!(store
            .replace_visible_with_summary(last_seq, &turn_ids, "  ", None, false)
            .is_err());

        let visible = store.load_visible_turns().unwrap();
        assert_eq!(visible.len(), 1);
        assert_eq!(visible[0].turn_id, "t1");
    }

    #[test]
    fn compact_insert_failure_rolls_back_hidden_turns() {
        let (temp, store) = test_store();
        store.start_turn("t1", "hello", 999999).unwrap();
        store.complete_turn("t1", "reply", None).unwrap();
        let (last_seq, turn_ids) = visible_snapshot(&store);
        let conn = rusqlite::Connection::open(temp.path().join("state/conversation.db")).unwrap();
        conn.execute_batch(
            "CREATE TRIGGER fail_summary_insert
             BEFORE INSERT ON turns WHEN NEW.is_summary = 1
             BEGIN SELECT RAISE(ABORT, 'injected summary failure'); END;",
        )
        .unwrap();

        assert!(store
            .replace_visible_with_summary(last_seq, &turn_ids, "summary", None, false)
            .is_err());
        let visible = store.load_visible_turns().unwrap();
        assert_eq!(visible.len(), 1);
        assert_eq!(visible[0].turn_id, "t1");
        assert!(!visible[0].hidden);
    }

    #[test]
    fn irreversible_legacy_summary_is_not_deleted_by_undo() {
        let (_temp, store) = test_store();
        store
            .insert_summary_turn("legacy summary", None, false)
            .unwrap();

        assert_eq!(store.undo_last_turn().unwrap(), (0, None));
        assert_eq!(
            store
                .load_last_summary()
                .unwrap()
                .unwrap()
                .assistant_content,
            "legacy summary"
        );
    }

    #[test]
    fn irreversible_nested_legacy_summary_is_not_downgraded_by_undo() {
        let (_temp, store) = test_store();
        store
            .insert_summary_turn("legacy summary one", None, false)
            .unwrap();
        let first_seq = store.load_visible_turns().unwrap()[0].seq;
        store.hide_turns_before_seq(first_seq).unwrap();
        store
            .insert_summary_turn("legacy summary two", None, false)
            .unwrap();

        assert_eq!(store.undo_last_turn().unwrap(), (0, None));
        assert_eq!(
            store
                .load_last_summary()
                .unwrap()
                .unwrap()
                .assistant_content,
            "legacy summary two"
        );
    }

    #[test]
    fn undo_does_not_remove_a_running_turn() {
        let (_temp, store) = test_store();
        store.start_turn("t1", "completed", 999999).unwrap();
        store.complete_turn("t1", "reply", None).unwrap();
        store
            .start_turn("running", "active", std::process::id())
            .unwrap();

        assert_eq!(store.undo_last_turn().unwrap(), (0, None));
        assert_eq!(store.load_visible_turns().unwrap().len(), 2);
    }

    #[test]
    fn compact_rejects_a_changed_snapshot() {
        let (_temp, store) = test_store();
        store.start_turn("t1", "first", 999999).unwrap();
        store.complete_turn("t1", "reply", None).unwrap();
        let (last_seq, turn_ids) = visible_snapshot(&store);
        store.undo_last_turn().unwrap();

        assert!(store
            .replace_visible_with_summary(last_seq, &turn_ids, "stale", None, false)
            .is_err());
        assert!(store.load_visible_turns().unwrap().is_empty());
    }

    #[test]
    fn compact_rejects_a_new_turn_after_snapshot() {
        let (_temp, store) = test_store();
        store.start_turn("t1", "first", 999999).unwrap();
        store.complete_turn("t1", "reply", None).unwrap();
        let (last_seq, turn_ids) = visible_snapshot(&store);
        store.start_turn("t2", "second", 999999).unwrap();
        store.complete_turn("t2", "reply", None).unwrap();

        assert!(store
            .replace_visible_with_summary(last_seq, &turn_ids, "stale", None, false)
            .is_err());
        assert_eq!(store.load_visible_turns().unwrap().len(), 2);
    }
}
