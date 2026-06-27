use super::{ToolRegistry, ToolSpec};
use anyhow::{bail, Result};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use tokio::process::Command;

const MAX_READ_BYTES: u64 = 512 * 1024;
const MAX_COMMAND_OUTPUT_CHARS: usize = 20_000;

pub fn register(registry: &mut ToolRegistry, allow_command_execution: bool) {
    registry.register(ToolSpec::new(
        "read_file",
        "Read a UTF-8 text file or list a directory in the local workspace. Use absolute paths or workspace-relative paths.",
        json!({"type":"object","properties":{"path":{"type":"string"},"offset":{"type":"integer"},"limit":{"type":"integer"}},"required":["path"],"additionalProperties":false}),
        |args| async move { read_file(args) },
    ));
    registry.register(ToolSpec::new(
        "find_files",
        "Find files by filename pattern under a workspace directory. Similar to glob. Avoid broad system paths such as /.",
        json!({"type":"object","properties":{"path":{"type":"string"},"pattern":{"type":"string"},"max_results":{"type":"integer"}},"required":["pattern"],"additionalProperties":false}),
        |args| async move { find_files(args).await },
    ));
    registry.register(ToolSpec::new(
        "search_text",
        "Search text in files using ripgrep under a workspace directory. Avoid broad system paths such as /.",
        json!({"type":"object","properties":{"path":{"type":"string"},"pattern":{"type":"string"},"include":{"type":"string"},"max_results":{"type":"integer"}},"required":["pattern"],"additionalProperties":false}),
        |args| async move { search_text(args).await },
    ));
    registry.register(ToolSpec::new(
        "run_command",
        "Run a shell command in the workspace. Disabled unless skills.allow_command_execution is true.",
        json!({"type":"object","properties":{"command":{"type":"string"},"timeout_seconds":{"type":"integer"}},"required":["command"],"additionalProperties":false}),
        move |args| async move { run_command(args, allow_command_execution).await },
    ));
    registry.register(ToolSpec::new(
        "task_agent",
        "Create a focused subtask plan for a complex task. Current implementation returns a structured handoff prompt for the main agent.",
        json!({"type":"object","properties":{"description":{"type":"string"},"prompt":{"type":"string"}},"required":["prompt"],"additionalProperties":false}),
        |args| async move { task_agent(args) },
    ));
}

fn read_file(args: Value) -> Result<String> {
    let path = path_arg(&args, "path")?;
    if path.is_dir() {
        let mut entries = Vec::new();
        for entry in std::fs::read_dir(path)?.take(500) {
            let entry = entry?;
            let suffix = if entry.file_type()?.is_dir() { "/" } else { "" };
            entries.push(format!("{}{}", entry.file_name().to_string_lossy(), suffix));
        }
        entries.sort();
        return Ok(entries.join("\n"));
    }
    let metadata = std::fs::metadata(&path)?;
    if metadata.len() > MAX_READ_BYTES {
        bail!("file too large to read directly: {} bytes", metadata.len());
    }
    let text = std::fs::read_to_string(&path)?;
    let offset = args
        .get("offset")
        .and_then(Value::as_u64)
        .unwrap_or(1)
        .max(1) as usize;
    let limit = args
        .get("limit")
        .and_then(Value::as_u64)
        .unwrap_or(2000)
        .max(1) as usize;
    let lines = text
        .lines()
        .enumerate()
        .skip(offset.saturating_sub(1))
        .take(limit)
        .map(|(index, line)| format!("{}: {}", index + 1, line))
        .collect::<Vec<_>>();
    Ok(lines.join("\n"))
}

async fn find_files(args: Value) -> Result<String> {
    let path = optional_path(&args).unwrap_or(std::env::current_dir()?);
    ensure_safe_search_path(&path)?;
    let pattern = required(&args, "pattern")?;
    let max_results = max_results(&args);
    let output = Command::new("rg")
        .arg("--files")
        .arg("-g")
        .arg(pattern)
        .current_dir(path)
        .stdin(Stdio::null())
        .output()
        .await?;
    command_output_limited(output, max_results)
}

async fn search_text(args: Value) -> Result<String> {
    let path = optional_path(&args).unwrap_or(std::env::current_dir()?);
    ensure_safe_search_path(&path)?;
    let pattern = required(&args, "pattern")?;
    let max_results = max_results(&args);
    let mut command = Command::new("rg");
    command
        .arg("--line-number")
        .arg("--max-count")
        .arg(max_results.to_string())
        .arg(pattern);
    if let Some(include) = args
        .get("include")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
    {
        command.arg("-g").arg(include.trim());
    }
    let output = command
        .current_dir(path)
        .stdin(Stdio::null())
        .output()
        .await?;
    command_output_limited(output, max_results)
}

async fn run_command(args: Value, allowed: bool) -> Result<String> {
    if !allowed {
        bail!("command execution is disabled; set skills.allow_command_execution=true in config.jsonc to enable run_command");
    }
    let command = required(&args, "command")?;
    let timeout = args
        .get("timeout_seconds")
        .and_then(Value::as_u64)
        .unwrap_or(30)
        .min(120);
    let output = tokio::time::timeout(
        std::time::Duration::from_secs(timeout),
        Command::new("sh")
            .arg("-lc")
            .arg(command)
            .stdin(Stdio::null())
            .output(),
    )
    .await??;
    command_output(output)
}

fn task_agent(args: Value) -> Result<String> {
    let prompt = required(&args, "prompt")?;
    let description = args
        .get("description")
        .and_then(Value::as_str)
        .unwrap_or("subtask");
    Ok(serde_json::to_string_pretty(
        &json!({"description": description, "prompt": prompt, "note": "Subagent execution is not implemented yet; use this as a structured handoff."}),
    )?)
}

fn command_output(output: std::process::Output) -> Result<String> {
    let stdout = clip_output(&String::from_utf8_lossy(&output.stdout));
    let stderr = clip_output(&String::from_utf8_lossy(&output.stderr));
    Ok(serde_json::to_string_pretty(
        &json!({"success": output.status.success(), "exit_code": output.status.code(), "stdout": stdout, "stderr": stderr}),
    )?)
}

fn command_output_limited(output: std::process::Output, max_lines: usize) -> Result<String> {
    let stdout_raw = String::from_utf8_lossy(&output.stdout);
    let stdout = stdout_raw
        .lines()
        .take(max_lines)
        .collect::<Vec<_>>()
        .join("\n");
    let stderr = clip_output(&String::from_utf8_lossy(&output.stderr));
    let truncated = stdout_raw.lines().nth(max_lines).is_some();
    Ok(serde_json::to_string_pretty(&json!({
        "success": output.status.success(),
        "exit_code": output.status.code(),
        "stdout": clip_output(&stdout),
        "stderr": stderr,
        "truncated": truncated,
        "max_results": max_lines
    }))?)
}

fn ensure_safe_search_path(path: &Path) -> Result<()> {
    let path = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    if path == Path::new("/") || path == Path::new("/home") || path == Path::new("/usr") {
        bail!(
            "refusing broad system search path: {}; use a specific workspace or subdirectory",
            path.display()
        );
    }
    Ok(())
}

fn max_results(args: &Value) -> usize {
    args.get("max_results")
        .and_then(Value::as_u64)
        .unwrap_or(100)
        .clamp(1, 500) as usize
}

fn clip_output(value: &str) -> String {
    let value = value.trim();
    if value.chars().count() <= MAX_COMMAND_OUTPUT_CHARS {
        value.to_string()
    } else {
        format!(
            "{}\n...[truncated to {MAX_COMMAND_OUTPUT_CHARS} chars]",
            value
                .chars()
                .take(MAX_COMMAND_OUTPUT_CHARS)
                .collect::<String>()
        )
    }
}

fn path_arg(args: &Value, key: &str) -> Result<PathBuf> {
    let value = required(args, key)?;
    Ok(expand_path(&value))
}

fn optional_path(args: &Value) -> Option<PathBuf> {
    args.get("path")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .map(expand_path)
}

fn expand_path(value: &str) -> PathBuf {
    let value = value.trim();
    if let Some(rest) = value.strip_prefix("~/") {
        if let Some(home) = directories::BaseDirs::new().map(|dirs| dirs.home_dir().to_path_buf()) {
            return home.join(rest);
        }
    }
    let path = Path::new(value);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(path)
    }
}

fn required(args: &Value, key: &str) -> Result<String> {
    let value = args
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim();
    if value.is_empty() {
        bail!("{key} is required")
    } else {
        Ok(value.to_string())
    }
}
