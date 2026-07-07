mod conversation_db;
mod usage;

use crate::llm::Usage;
use crate::paths::MiyuPaths;
use anyhow::Result;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs::OpenOptions;
use std::path::PathBuf;
use std::sync::Arc;

#[allow(unused_imports)]
pub use conversation_db::{
    interrupted_text, pending_placeholder, ConversationDb, Turn, TurnStatus,
};

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
        if !self.log_file().exists() {
            touch(self.log_file())?;
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

    pub fn append_tool_report_context(&self, turn_id: &str, tool_name: &str, report: &str) -> Result<()> {
        self.conv_db.append_tool_report(
            turn_id,
            &format!(
                "<previous_tool_report name=\"{tool_name}\">\n{}\n</previous_tool_report>",
                report.trim()
            ),
        )
    }

    pub fn mark_interrupted_turn_if_needed(&self) -> Result<bool> {
        Ok(false)
    }

    pub fn recover_stale_turns(&self) -> Result<usize> {
        self.conv_db.recover_stale_running_turns()
    }

    pub fn history(&self, limit: usize) -> Result<Vec<StoredConversationEntry>> {
        let turns = self.conv_db.load_turns()?;
        let mut entries = turns_to_entries(turns);
        let start = entries.len().saturating_sub(limit);
        Ok(entries.split_off(start))
    }

    pub fn load_conversation(&self) -> Result<Vec<StoredConversationEntry>> {
        let turns = self.conv_db.load_turns()?;
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

    pub fn hide_turns_before_seq(&self, seq: i64) -> Result<usize> {
        self.conv_db.hide_turns_before_seq(seq)
    }

    pub fn insert_summary_turn(&self, summary: &str) -> Result<()> {
        self.conv_db.insert_summary_turn(summary)
    }

    pub fn load_last_summary(&self) -> Result<Option<Turn>> {
        self.conv_db.load_last_summary()
    }

    pub fn trim_visible_to_token_budget(
        &self,
        context_window: usize,
        trim_at_ratio: f32,
        trim_batch_ratio: f32,
    ) -> Result<Vec<StoredConversationEntry>> {
        let turns = self.conv_db.load_visible_turns()?;
        let trigger = (context_window as f32 * trim_at_ratio).max(1.0) as usize;
        let mut total: usize = turns.iter().map(|t| turn_estimated_tokens(t)).sum();
        if total <= trigger {
            return Ok(Vec::new());
        }
        let target =
            (context_window as f32 * (1.0 - trim_batch_ratio)).max(1.0) as usize;
        let mut start = 0usize;
        while start < turns.len() && total > target {
            total = total.saturating_sub(turn_estimated_tokens(&turns[start]));
            start += 1;
        }
        if start == 0 {
            return Ok(Vec::new());
        }
        let evicted = turns_to_entries(self.conv_db.trim_oldest_visible_turns(start)?);
        Ok(evicted)
    }

    pub fn reset_conversation(&self) -> Result<()> {
        self.conv_db.reset()
    }

    pub fn undo_last_turn(&self) -> Result<(usize, Option<String>)> {
        self.conv_db.undo_last_turn()
    }

    pub fn add_usage(&self, usage: &Usage) -> Result<()> {
        self.init_files()?;
        usage::add_usage(&self.usage_file(), usage)
    }

    pub fn token_total(&self) -> Result<u64> {
        self.conv_db.token_total()
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
    pub fn running_turn_summaries_excluding(
        &self,
        exclude_turn_id: &str,
    ) -> Result<Vec<String>> {
        self.conv_db.running_turn_summaries_excluding(exclude_turn_id)
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

    fn log_file(&self) -> PathBuf {
        self.state_dir.join("miyu.log")
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

fn turn_estimated_tokens(turn: &Turn) -> usize {
    (turn_chars(turn) / 4).max(1)
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

fn touch(path: PathBuf) -> Result<()> {
    OpenOptions::new().create(true).append(true).open(path)?;
    Ok(())
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
        })
        .unwrap();

        store.start_turn("turn_1", "hello", 999999).unwrap();
        let turns = store.load_turns().unwrap();
        assert_eq!(turns.len(), 1);
        assert_eq!(turns[0].status, TurnStatus::Running);
        assert_eq!(turns[0].assistant_content, pending_placeholder());

        store
            .complete_turn("turn_1", "hi there", None)
            .unwrap();
        let turns = store.load_turns().unwrap();
        assert_eq!(turns[0].status, TurnStatus::Completed);
        assert_eq!(turns[0].assistant_content, "hi there");
    }

    #[test]
    fn interrupt_turn() {
        let temp = tempfile::tempdir().unwrap();
        let store = StateStore::new(&MiyuPaths {
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
        store.start_turn("turn_1", "终端1的prompt", current_pid).unwrap();
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
        })
        .unwrap();
        (temp, store)
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

        store.insert_summary_turn("## Task Goal\nDo stuff").unwrap();

        let summary = store.load_last_summary().unwrap();
        assert!(summary.is_some());
        let summary = summary.unwrap();
        assert!(summary.is_summary);
        assert!(!summary.hidden);
        assert_eq!(summary.assistant_content, "## Task Goal\nDo stuff");

        let visible = store.load_visible_turns().unwrap();
        assert_eq!(visible.len(), 2);
        assert!(visible.iter().any(|t| t.is_summary));
        assert!(visible.iter().any(|t| !t.is_summary));
    }

    #[test]
    fn hide_before_seq_hides_old_summary_too() {
        let (_temp, store) = test_store();
        store.start_turn("t1", "old", 999999).unwrap();
        store.complete_turn("t1", "old reply", None).unwrap();
        store.insert_summary_turn("summary of old").unwrap();
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
    fn token_trim_pops_oldest() {
        let (_temp, store) = test_store();
        for i in 0..10 {
            let id = format!("t{i}");
            let content = "x".repeat(1000);
            store.start_turn(&id, &content, 999999).unwrap();
            store.complete_turn(&id, &content, None).unwrap();
        }

        let evicted = store
            .trim_visible_to_token_budget(2000, 0.9, 0.15)
            .unwrap();
        assert!(!evicted.is_empty(), "should have evicted some turns");

        let visible = store.load_visible_turns().unwrap();
        assert!(visible.len() < 10, "should have fewer turns after trim");
    }

    #[test]
    fn token_trim_noop_when_under_threshold() {
        let (_temp, store) = test_store();
        store.start_turn("t1", "short", 999999).unwrap();
        store.complete_turn("t1", "reply", None).unwrap();

        let evicted = store
            .trim_visible_to_token_budget(100_000, 0.9, 0.15)
            .unwrap();
        assert!(evicted.is_empty());

        let visible = store.load_visible_turns().unwrap();
        assert_eq!(visible.len(), 1);
    }
}
