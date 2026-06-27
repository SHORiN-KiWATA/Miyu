use crate::llm::Usage;
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Default, Serialize, Deserialize)]
struct UsageState {
    requests: u64,
    prompt_tokens: u64,
    completion_tokens: u64,
    total_tokens: u64,
}

pub fn add_usage(path: &Path, usage: &Usage) -> Result<()> {
    let mut state = if path.exists() {
        let raw = std::fs::read_to_string(path)?;
        serde_json::from_str(&raw).unwrap_or_default()
    } else {
        UsageState::default()
    };
    state.requests += 1;
    state.prompt_tokens += usage.prompt_tokens;
    state.completion_tokens += usage.completion_tokens;
    state.total_tokens += usage.total_tokens;
    std::fs::write(path, format!("{}\n", serde_json::to_string_pretty(&state)?))?;
    Ok(())
}
