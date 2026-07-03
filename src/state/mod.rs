mod usage;

use crate::llm::Usage;
use crate::paths::MiyuPaths;
use anyhow::Result;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs::OpenOptions;
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct StateStore {
    state_dir: PathBuf,
}

impl StateStore {
    pub fn new(paths: &MiyuPaths) -> Result<Self> {
        Ok(Self {
            state_dir: paths.state_dir.clone(),
        })
    }

    pub fn init_files(&self) -> Result<()> {
        std::fs::create_dir_all(&self.state_dir)?;
        touch(self.conversation_file())?;
        if !self.usage_file().exists() {
            std::fs::write(self.usage_file(), "{\n  \"requests\": 0,\n  \"prompt_tokens\": 0,\n  \"completion_tokens\": 0,\n  \"total_tokens\": 0\n}\n")?;
        }
        touch(self.log_file())?;
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
            std::fs::write(self.conversation_file(), "")?;
            std::fs::write(file, format!("{fingerprint}\n"))?;
        }
        Ok(())
    }

    pub fn append_message(&self, role: &str, content: &str) -> Result<()> {
        self.append_entry(role, content, None)
    }

    pub fn append_assistant_message(&self, content: &str, reasoning: Option<&str>) -> Result<()> {
        self.append_entry("assistant", content, reasoning)
    }

    pub fn append_tool_report_context(&self, tool_name: &str, report: &str) -> Result<()> {
        self.append_assistant_message(
            &format!(
                "<previous_tool_report name=\"{tool_name}\">\n{}\n</previous_tool_report>",
                report.trim()
            ),
            None,
        )
    }

    pub fn mark_interrupted_turn_if_needed(&self) -> Result<bool> {
        let entries = self.load_conversation()?;
        if !matches!(entries.last(), Some(entry) if entry.role == "user") {
            return Ok(false);
        }
        self.append_assistant_message(
            "上一轮响应已中断，未完成。不要继续执行上一轮任务，除非用户重新要求。",
            None,
        )?;
        Ok(true)
    }

    fn append_entry(&self, role: &str, content: &str, reasoning: Option<&str>) -> Result<()> {
        self.init_files()?;
        let entry = ConversationEntry {
            timestamp: Utc::now().to_rfc3339(),
            role,
            content,
            reasoning,
        };
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.conversation_file())?;
        writeln!(file, "{}", serde_json::to_string(&entry)?)?;
        Ok(())
    }

    pub fn history(&self, limit: usize) -> Result<Vec<StoredConversationEntry>> {
        let mut entries = self.load_conversation()?;
        let start = entries.len().saturating_sub(limit);
        Ok(entries.split_off(start))
    }

    pub fn load_conversation(&self) -> Result<Vec<StoredConversationEntry>> {
        self.init_files()?;
        let file = OpenOptions::new()
            .read(true)
            .open(self.conversation_file())?;
        let mut entries = Vec::new();
        for line in BufReader::new(file).lines() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            entries.push(serde_json::from_str(&line)?);
        }
        Ok(entries)
    }

    pub fn trim_conversation_to_budget(
        &self,
        max_chars: usize,
        trim_at_ratio: f32,
        trim_batch_ratio: f32,
    ) -> Result<Vec<StoredConversationEntry>> {
        let entries = self.load_conversation()?;
        let trigger = (max_chars as f32 * trim_at_ratio).max(1.0) as usize;
        let mut total = conversation_chars(&entries);
        if total <= trigger {
            return Ok(Vec::new());
        }
        let target =
            max_chars.saturating_sub((max_chars as f32 * trim_batch_ratio).max(1.0) as usize);
        let mut start = 0usize;
        while start < entries.len() && total > target {
            total = total.saturating_sub(entry_chars(&entries[start]));
            start += 1;
        }
        let evicted = entries[..start].to_vec();
        self.rewrite_conversation(&entries[start..])?;
        Ok(evicted)
    }

    pub fn reset_conversation(&self) -> Result<()> {
        self.init_files()?;
        std::fs::write(self.conversation_file(), "")?;
        Ok(())
    }

    pub fn undo_last_turn(&self) -> Result<(usize, Option<String>)> {
        let mut entries = self.load_conversation()?;
        let original_len = entries.len();
        let mut prompt = None;
        while matches!(entries.last(), Some(entry) if entry.role != "assistant") {
            entries.pop();
        }
        if matches!(entries.last(), Some(entry) if entry.role == "assistant") {
            entries.pop();
        }
        if matches!(entries.last(), Some(entry) if entry.role == "user") {
            prompt = entries.last().map(|entry| entry.content.clone());
            entries.pop();
        }
        let removed = original_len.saturating_sub(entries.len());
        if removed > 0 {
            self.rewrite_conversation(&entries)?;
        }
        Ok((removed, prompt))
    }

    pub fn add_usage(&self, usage: &Usage) -> Result<()> {
        self.init_files()?;
        usage::add_usage(&self.usage_file(), usage)
    }

    fn conversation_file(&self) -> PathBuf {
        self.state_dir.join("conversation.jsonl")
    }

    fn rewrite_conversation(&self, entries: &[StoredConversationEntry]) -> Result<()> {
        self.init_files()?;
        let mut file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(self.conversation_file())?;
        for entry in entries {
            writeln!(file, "{}", serde_json::to_string(entry)?)?;
        }
        Ok(())
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

fn conversation_chars(entries: &[StoredConversationEntry]) -> usize {
    entries.iter().map(entry_chars).sum()
}

fn entry_chars(entry: &StoredConversationEntry) -> usize {
    entry.role.chars().count()
        + entry.content.chars().count()
        + entry
            .reasoning
            .as_deref()
            .map(str::chars)
            .map(Iterator::count)
            .unwrap_or(0)
}

#[derive(Serialize)]
struct ConversationEntry<'a> {
    timestamp: String,
    role: &'a str,
    content: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning: Option<&'a str>,
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
    fn marks_orphan_user_turn_as_interrupted() {
        let temp = tempfile::tempdir().unwrap();
        let store = StateStore {
            state_dir: temp.path().to_path_buf(),
        };
        store.append_message("user", "old task").unwrap();
        assert!(store.mark_interrupted_turn_if_needed().unwrap());
        let entries = store.load_conversation().unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[1].role, "assistant");
        assert!(entries[1].content.contains("已中断"));
        assert!(!store.mark_interrupted_turn_if_needed().unwrap());
    }
}
