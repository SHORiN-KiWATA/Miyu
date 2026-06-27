use super::{ToolRegistry, ToolSpec};
use crate::paths::MiyuPaths;
use anyhow::{bail, Result};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::process::Stdio;
use tokio::process::Command;

pub fn register_skills(
    registry: &mut ToolRegistry,
    paths: &MiyuPaths,
    allow_command_execution: bool,
) -> Result<()> {
    if !paths.skills_dir.exists() {
        return Ok(());
    }
    for entry in std::fs::read_dir(&paths.skills_dir)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let skill_dir = entry.path();
        let skill_file = skill_dir.join("SKILL.md");
        if !skill_file.is_file() {
            continue;
        }
        let raw = std::fs::read_to_string(&skill_file)?;
        if frontmatter_value(&raw, "name").as_deref() == Some("web-search") {
            register_web_search(registry, skill_dir, allow_command_execution);
        }
    }
    Ok(())
}

fn register_web_search(
    registry: &mut ToolRegistry,
    skill_dir: PathBuf,
    allow_command_execution: bool,
) {
    let script = skill_dir.join("scripts/web-search.py");
    registry.register(ToolSpec::new(
        "web_search",
        "Search the web for current or real-time information. Use this when the answer needs online lookup, recent facts, news, or verification. Return search results with URLs for verification when needed.",
        json!({
            "type": "object",
            "properties": {
                "query": { "type": "string", "description": "Search query." },
                "max_results": { "type": "integer", "description": "Maximum results to return.", "minimum": 1, "maximum": 10 },
                "provider": { "type": "string", "enum": ["auto", "tavily", "firecrawl", "anysearch", "searxng"], "description": "Search provider." }
            },
            "required": ["query"],
            "additionalProperties": false
        }),
        move |args| {
            let script = script.clone();
            async move { run_web_search(script, allow_command_execution, args).await }
        },
    ));
}

async fn run_web_search(
    script: PathBuf,
    allow_command_execution: bool,
    args: Value,
) -> Result<String> {
    if !allow_command_execution {
        bail!("skill command execution is disabled; set skills.allow_command_execution=true in config.jsonc to enable this tool");
    }
    if !script.is_file() {
        bail!("web-search skill script not found: {}", script.display());
    }
    let query = args
        .get("query")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim();
    if query.is_empty() {
        bail!("web_search requires a non-empty query");
    }
    let max_results = args
        .get("max_results")
        .and_then(Value::as_u64)
        .unwrap_or(5)
        .clamp(1, 10)
        .to_string();
    let provider = args
        .get("provider")
        .and_then(Value::as_str)
        .unwrap_or("auto");
    let output = Command::new("python3")
        .arg(script)
        .arg(query)
        .arg("-n")
        .arg(max_results)
        .arg("-p")
        .arg(provider)
        .stdin(Stdio::null())
        .output()
        .await?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("web_search failed: {}", stderr.trim());
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn frontmatter_value(raw: &str, key: &str) -> Option<String> {
    let mut lines = raw.lines();
    if lines.next()? != "---" {
        return None;
    }
    for line in lines {
        if line == "---" {
            break;
        }
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        if name.trim() == key {
            return Some(value.trim().trim_matches('"').to_string());
        }
    }
    None
}
