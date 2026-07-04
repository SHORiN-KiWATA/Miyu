use super::{ToolRegistry, ToolSpec};
use crate::i18n::text as t;
use anyhow::{bail, Result};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::process::Command;

const MAX_READ_BYTES: u64 = 50 * 1024;
const MAX_READ_LINES: usize = 2_000;
const MAX_LINE_CHARS: usize = 2_000;
const MAX_COMMAND_OUTPUT_CHARS: usize = 20_000;
const SEARCH_TIMEOUT_SECONDS: u64 = 30;

pub fn register(registry: &mut ToolRegistry, allow_command_execution: bool) {
    register_readonly(registry);
    registry.register(ToolSpec::new(
        "run_command",
        t("Run a shell command in the workspace when skills.allow_command_execution is enabled.", "当 skills.allow_command_execution 启用时，在工作区运行 shell 命令。"),
        json!({"type":"object","properties":{"command":{"type":"string","description": t("Command to run.", "要运行的命令。")},"timeout_seconds":{"type":"integer","description": t("Optional timeout in seconds.", "可选超时时间，单位秒。")}},"required":["command"],"additionalProperties":false}),
        move |args| async move { run_command(args, allow_command_execution).await },
    ).writes());
    registry.register(ToolSpec::new(
        "edit_file",
        t("Edit an existing UTF-8 file by replacing an inclusive 1-based line range. Use after read_file identifies exact line numbers.", "按 1 起始的闭区间行号替换现有 UTF-8 文件内容。应先用 read_file 确认准确行号。"),
        json!({"type":"object","properties":{"path":{"type":"string","description": t("Workspace-relative or absolute file path.", "工作区相对路径或绝对文件路径。")},"start_line":{"type":"integer","description": t("1-based first line to replace.", "要替换的第一行，1 起始。")},"end_line":{"type":"integer","description": t("1-based last line to replace, inclusive.", "要替换的最后一行，闭区间。")},"replacement":{"type":"string","description": t("Replacement text. May contain multiple lines. Empty text deletes the line range.", "替换文本，可包含多行；空文本会删除指定行范围。")}},"required":["path","start_line","end_line","replacement"],"additionalProperties":false}),
        |args| async move { edit_file(args) },
    ).writes());
    registry.register(ToolSpec::new(
        "trash_path",
        t("Move a file, directory, or symlink to the system Trash instead of permanently deleting it. Use this when the user asks to delete/remove/clean up a local path; do not use rm unless explicitly requested. Only claim success when exists_after is false.", "把文件、目录或符号链接移入系统回收站，而不是永久删除。用户要求删除/移除/清理本地路径时优先使用它；除非用户明确要求，不要使用 rm。只有 exists_after 为 false 时才说明已处理。"),
        json!({"type":"object","properties":{"path":{"type":"string","description": t("File, directory, or symlink path to move to Trash. Supports absolute paths, workspace-relative paths, and ~/ paths.", "要移入回收站的文件、目录或符号链接路径。支持绝对路径、工作区相对路径和 ~/ 路径。")}},"required":["path"],"additionalProperties":false}),
        |args| async move { trash_path(args) },
    ).writes());
}

pub fn register_readonly(registry: &mut ToolRegistry) {
    registry.register(ToolSpec::new(
        "run_command",
        t("Run an explicitly read-only shell command for inspection. Mutating commands are blocked in plan mode.", "运行明确只读的 shell 命令用于检查。计划模式会阻止修改性命令。"),
        json!({"type":"object","properties":{"command":{"type":"string","description": t("Read-only command to run.", "要运行的只读命令。")},"timeout_seconds":{"type":"integer","description": t("Optional timeout in seconds.", "可选超时时间，单位秒。")}},"required":["command"],"additionalProperties":false}),
        |args| async move { run_readonly_command(args).await },
    ));
    registry.register(ToolSpec::new(
        "check_os_info",
        t("Check basic read-only OS, shell, desktop session, kernel, host, and package-manager context. For concrete Linux input method issues, prefer linux_input_method_diagnose.", "查看只读基础系统信息，包括 OS、shell、桌面会话、内核、主机和包管理器上下文。排查具体 Linux 输入法问题时优先使用 linux_input_method_diagnose。"),
        json!({"type":"object","properties":{},"additionalProperties":false}),
        |_| async move { check_os_info() },
    ));
    registry.register(ToolSpec::new(
        "read_file",
        t("Read a UTF-8 text file by 1-based line offset, or list a directory page. Use absolute paths, workspace-relative paths, or ~/ paths. Large files are paged and binary files are refused.", "按 1 起始行号分页读取 UTF-8 文本文件，或分页列出目录。支持绝对路径、工作区相对路径和 ~/ 路径。大文件会分页，二进制文件会被拒绝。"),
        json!({"type":"object","properties":{"path":{"type":"string","description": t("File or directory path.", "文件或目录路径。")},"offset":{"type":"integer","description": t("Starting line, 1-based.", "起始行，1 起始。")},"limit":{"type":"integer","description": t("Maximum lines to read.", "最多读取行数。")}},"required":["path"],"additionalProperties":false}),
        |args| async move { read_file(args) },
    ));
    registry.register(ToolSpec::new(
        "glob",
        t("Find files by case-insensitive glob pattern under a directory. Defaults to workspace; use ~ or /home for user files, or / for protected global search.", "在目录下按大小写不敏感 glob 模式查找文件。默认工作区；查用户文件用 ~ 或 /home，受保护的全局搜索可用 /。"),
        json!({"type":"object","properties":{"path":{"type":"string","description": t("Directory to search. Defaults to workspace; use ~ or /home for user files, or / for protected global search.", "搜索目录，默认工作区；查用户文件用 ~ 或 /home，受保护的全局搜索可用 /。")},"pattern":{"type":"string","description": t("Case-insensitive glob pattern, for example *ai*test*.", "大小写不敏感 Glob 模式，例如 *ai*测试*。")},"max_results":{"type":"integer","description": t("Maximum results.", "最多结果数。")}},"required":["pattern"],"additionalProperties":false}),
        |args| async move { glob_files(args).await },
    ));
    registry.register(ToolSpec::new(
        "find_files",
        t("Find files by case-insensitive filename glob. Defaults to workspace; use ~ or /home for user files, or / for protected global search.", "按大小写不敏感文件名 glob 查找文件。默认工作区；查用户文件用 ~ 或 /home，受保护的全局搜索可用 /。"),
        json!({"type":"object","properties":{"path":{"type":"string","description": t("Directory to search. Defaults to workspace; use ~ or /home for user files, or / for protected global search.", "搜索目录，默认工作区；查用户文件用 ~ 或 /home，受保护的全局搜索可用 /。")},"pattern":{"type":"string","description": t("Case-insensitive glob pattern.", "大小写不敏感 Glob 模式。")},"max_results":{"type":"integer","description": t("Maximum results.", "最多结果数。")}},"required":["pattern"],"additionalProperties":false}),
        |args| async move { glob_files(args).await },
    ));
    registry.register(ToolSpec::new(
        "grep",
        t("Search file contents using ripgrep under a directory or single file. Defaults to workspace; use ~ or /home for user files, or / for protected global search. No matches are returned as an empty ok result.", "在目录或单个文件中用 ripgrep 搜索内容。默认工作区；查用户文件用 ~ 或 /home，受保护的全局搜索可用 /。无匹配会作为成功的空结果返回。"),
        json!({"type":"object","properties":{"path":{"type":"string","description": t("Directory or file to search. Defaults to workspace; use ~ or /home for user files, or / for protected global search.", "要搜索的目录或文件，默认工作区；查用户文件用 ~ 或 /home，受保护的全局搜索可用 /。")},"pattern":{"type":"string","description": t("Regex pattern.", "正则模式。")},"include":{"type":"string","description": t("Optional case-insensitive file glob filter.", "可选大小写不敏感文件 glob 过滤。")},"max_results":{"type":"integer","description": t("Maximum matches.", "最多匹配数。")}},"required":["pattern"],"additionalProperties":false}),
        |args| async move { grep_text(args).await },
    ));
    registry.register(ToolSpec::new(
        "search_text",
        t("Search text in files using ripgrep. Defaults to workspace; use ~ or /home for user files, or / for protected global search. No matches are returned as an empty ok result.", "用 ripgrep 搜索文件内容。默认工作区；查用户文件用 ~ 或 /home，受保护的全局搜索可用 /。无匹配会作为成功的空结果返回。"),
        json!({"type":"object","properties":{"path":{"type":"string","description": t("Directory to search. Defaults to workspace; use ~ or /home for user files, or / for protected global search.", "搜索目录，默认工作区；查用户文件用 ~ 或 /home，受保护的全局搜索可用 /。")},"pattern":{"type":"string","description": t("Regex or text pattern.", "正则或文本模式。")},"include":{"type":"string","description": t("Optional case-insensitive file glob filter.", "可选大小写不敏感文件 glob 过滤。")},"max_results":{"type":"integer","description": t("Maximum results.", "最多结果数。")}},"required":["pattern"],"additionalProperties":false}),
        |args| async move { grep_text(args).await },
    ));
}

fn check_os_info() -> Result<String> {
    let mut env = BTreeMap::new();
    for key in [
        "SHELL",
        "TERM",
        "LANG",
        "PATH",
        "XDG_CURRENT_DESKTOP",
        "XDG_SESSION_TYPE",
        "DESKTOP_SESSION",
        "WAYLAND_DISPLAY",
        "DISPLAY",
    ] {
        if let Ok(value) = std::env::var(key) {
            if !value.trim().is_empty() {
                env.insert(key, value);
            }
        }
    }
    let os_release = read_small_file("/etc/os-release");
    let arch_release = read_small_file("/etc/arch-release").is_some();
    let debian_version = read_small_file("/etc/debian_version");
    let fedora_release = read_small_file("/etc/fedora-release");
    let proc_version = read_small_file("/proc/version");
    let proc_cmdline = read_small_file("/proc/cmdline");
    let macos_system_version = read_small_file("/System/Library/CoreServices/SystemVersion.plist");
    let macos = parse_macos_system_version(macos_system_version.as_deref());
    let package_manager_guess = package_manager_guess(
        &os_release,
        arch_release,
        debian_version.is_some(),
        fedora_release.is_some(),
        macos_system_version.is_some(),
    );
    Ok(serde_json::to_string_pretty(&json!({
        "ok": true,
        "platform": std::env::consts::OS,
        "os_release": os_release,
        "arch_release": arch_release,
        "debian_version": debian_version,
        "fedora_release": fedora_release,
        "macos": macos,
        "kernel_version": proc_version,
        "kernel_cmdline": proc_cmdline,
        "arch": std::env::consts::ARCH,
        "os": std::env::consts::OS,
        "family": std::env::consts::FAMILY,
        "username": std::env::var("USER").ok().or_else(|| std::env::var("USERNAME").ok()),
        "hostname": read_small_file("/etc/hostname").map(|value| value.trim().to_string()),
        "env": env,
        "package_manager_guess": package_manager_guess,
        "notes": [
            "This tool is read-only and does not execute shell commands.",
            "This only reports basic OS context. For concrete Linux input method issues, use linux_input_method_diagnose."
        ],
    }))?)
}

fn read_small_file(path: &str) -> Option<String> {
    let metadata = std::fs::metadata(path).ok()?;
    if !metadata.is_file() || metadata.len() > 64 * 1024 {
        return None;
    }
    std::fs::read_to_string(path)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn package_manager_guess(
    os_release: &Option<String>,
    arch_release: bool,
    debian_version: bool,
    fedora_release: bool,
    macos: bool,
) -> Vec<&'static str> {
    let lower = os_release
        .as_deref()
        .unwrap_or_default()
        .to_ascii_lowercase();
    let mut managers = Vec::new();
    if arch_release || lower.contains("id=arch") || lower.contains("id_like=arch") {
        managers.push("pacman");
    }
    if debian_version
        || lower.contains("id=debian")
        || lower.contains("id=ubuntu")
        || lower.contains("id_like=debian")
    {
        managers.push("apt");
    }
    if fedora_release || lower.contains("id=fedora") || lower.contains("id_like=fedora") {
        managers.push("dnf");
    }
    if macos || std::env::consts::OS == "macos" {
        if Path::new("/opt/homebrew").exists() || Path::new("/usr/local/Homebrew").exists() {
            managers.push("brew");
        }
        if Path::new("/opt/local").exists() {
            managers.push("port");
        }
        if !managers
            .iter()
            .any(|manager| matches!(*manager, "brew" | "port"))
        {
            managers.push("brew");
        }
    }
    if managers.is_empty() {
        managers.push("unknown");
    }
    managers
}

fn parse_macos_system_version(raw: Option<&str>) -> Value {
    let Some(raw) = raw else {
        return Value::Null;
    };
    json!({
        "product_name": plist_value(raw, "ProductName"),
        "product_version": plist_value(raw, "ProductVersion"),
        "product_build_version": plist_value(raw, "ProductBuildVersion"),
    })
}

fn plist_value(raw: &str, key: &str) -> Option<String> {
    let marker = format!("<key>{key}</key>");
    let after_key = raw.split(&marker).nth(1)?;
    let after_string = after_key.split("<string>").nth(1)?;
    after_string
        .split("</string>")
        .next()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToString::to_string)
}

fn read_file(args: Value) -> Result<String> {
    let path = path_arg(&args, "path")?;
    let offset = args
        .get("offset")
        .and_then(Value::as_u64)
        .unwrap_or(1)
        .max(1) as usize;
    let limit = args
        .get("limit")
        .and_then(Value::as_u64)
        .unwrap_or(MAX_READ_LINES as u64)
        .clamp(1, MAX_READ_LINES as u64) as usize;
    if path.is_dir() {
        let mut entries = Vec::new();
        for entry in std::fs::read_dir(&path)? {
            let entry = entry?;
            let suffix = if entry.file_type()?.is_dir() { "/" } else { "" };
            entries.push(format!("{}{}", entry.file_name().to_string_lossy(), suffix));
        }
        entries.sort();
        let start = offset.saturating_sub(1);
        let selected = entries
            .iter()
            .skip(start)
            .take(limit)
            .cloned()
            .collect::<Vec<_>>();
        let next = (start + selected.len() < entries.len()).then_some(offset + selected.len());
        return Ok(serde_json::to_string_pretty(&json!({
            "type": "directory-page",
            "path": path.display().to_string(),
            "offset": offset,
            "limit": limit,
            "entries": selected,
            "truncated": next.is_some(),
            "next": next,
        }))?);
    }
    let metadata = std::fs::metadata(&path)?;
    if !metadata.is_file() {
        bail!("not a regular file or directory: {}", path.display())
    }
    ensure_not_binary_file(&path)?;
    let file = std::fs::File::open(&path)?;
    let reader = BufReader::new(file);
    let mut lines = Vec::new();
    let mut bytes = 0usize;
    let mut next = None;
    for (index, line) in reader.lines().enumerate() {
        let line_number = index + 1;
        if line_number < offset {
            continue;
        }
        if lines.len() >= limit || bytes >= MAX_READ_BYTES as usize {
            next = Some(line_number);
            break;
        }
        let mut line = line?;
        if line.chars().count() > MAX_LINE_CHARS {
            line = format!(
                "{}... (line truncated to {MAX_LINE_CHARS} chars)",
                line.chars().take(MAX_LINE_CHARS).collect::<String>()
            );
        }
        let rendered = format!("{line_number}: {line}");
        bytes += rendered.len() + 1;
        if bytes > MAX_READ_BYTES as usize {
            next = Some(line_number);
            break;
        }
        lines.push(rendered);
    }
    if lines.is_empty() && offset != 1 {
        bail!("offset {offset} is out of range")
    }
    Ok(serde_json::to_string_pretty(&json!({
        "type": "text-page",
        "path": path.display().to_string(),
        "offset": offset,
        "limit": limit,
        "content": lines.join("\n"),
        "truncated": next.is_some(),
        "next": next,
    }))?)
}

fn edit_file(args: Value) -> Result<String> {
    let path = path_arg(&args, "path")?;
    ensure_editable_file_path(&path)?;
    let start_line = args
        .get("start_line")
        .and_then(Value::as_u64)
        .ok_or_else(|| anyhow::anyhow!("start_line is required"))? as usize;
    let end_line = args
        .get("end_line")
        .and_then(Value::as_u64)
        .ok_or_else(|| anyhow::anyhow!("end_line is required"))? as usize;
    let replacement = args
        .get("replacement")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("replacement is required"))?;
    if start_line == 0 || end_line == 0 {
        bail!("line numbers must be 1-based")
    }
    if start_line > end_line {
        bail!("start_line must be less than or equal to end_line")
    }
    let original = std::fs::read_to_string(&path)?;
    let had_trailing_newline = original.ends_with('\n');
    let mut lines = original.lines().map(str::to_string).collect::<Vec<_>>();
    let old_line_count = lines.len();
    if start_line > old_line_count || end_line > old_line_count {
        bail!("line range {start_line}-{end_line} out of range: {old_line_count} lines")
    }
    let replacement = replacement.replace("\r\n", "\n").replace('\r', "\n");
    let replacement_lines = if replacement.is_empty() {
        Vec::new()
    } else {
        replacement.lines().map(str::to_string).collect::<Vec<_>>()
    };
    lines.splice(start_line - 1..end_line, replacement_lines);
    let mut updated = lines.join("\n");
    if had_trailing_newline && !updated.is_empty() {
        updated.push('\n');
    }
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let temp = tempfile::NamedTempFile::new_in(parent)?;
    std::fs::write(temp.path(), updated.as_bytes())?;
    temp.persist(&path)?;
    Ok(serde_json::to_string_pretty(&json!({
        "ok": true,
        "path": path.display().to_string(),
        "old_line_count": old_line_count,
        "new_line_count": lines.len()
    }))?)
}

fn trash_path(args: Value) -> Result<String> {
    let input = path_arg(&args, "path")?;
    let resolved = resolve_existing_path_without_following_leaf(&input)?;
    ensure_safe_trash_target(&resolved)?;
    let metadata = std::fs::symlink_metadata(&resolved)?;
    let kind = path_kind(&metadata);
    let existed_before = true;
    let original_path = resolved.display().to_string();
    let timestamp_before = current_unix_seconds();

    trash::delete(&resolved).map_err(|err| anyhow::anyhow!("failed to move to trash: {err}"))?;

    let exists_after = std::fs::symlink_metadata(&resolved).is_ok();
    let trash_item_id = find_recent_trash_item(&resolved, timestamp_before);
    Ok(serde_json::to_string_pretty(&json!({
        "ok": !exists_after,
        "kind": kind,
        "original_path": original_path,
        "existed_before": existed_before,
        "exists_after": exists_after,
        "trash_item_id": trash_item_id,
        "restore_hint": t("Open the system Trash and restore the item if needed.", "如需恢复，请打开系统回收站并还原该项目。"),
        "note": t("The path was moved to Trash, not permanently deleted.", "该路径已移入回收站，并未永久删除。")
    }))?)
}

async fn glob_files(args: Value) -> Result<String> {
    let path = optional_path(&args).unwrap_or(std::env::current_dir()?);
    let search_path = prepare_search_path(&path)?;
    let pattern = required(&args, "pattern")?;
    let max_results = max_results(&args);
    let output = tokio::time::timeout(
        std::time::Duration::from_secs(SEARCH_TIMEOUT_SECONDS),
        Command::new("rg")
            .arg("--no-config")
            .arg("--files")
            .arg("--no-messages")
            .arg("--hidden")
            .arg(format!("--iglob={pattern}"))
            .args(search_exclude_args(&search_path))
            .arg(".")
            .current_dir(&search_path)
            .stdin(Stdio::null())
            .output(),
    )
    .await??;
    search_output_limited(output, max_results)
}

async fn grep_text(args: Value) -> Result<String> {
    let path = optional_path(&args).unwrap_or(std::env::current_dir()?);
    let is_file = path.is_file();
    let search_root = if is_file {
        path.parent()
            .unwrap_or_else(|| Path::new("."))
            .to_path_buf()
    } else {
        path.clone()
    };
    let search_root = prepare_search_path(&search_root)?;
    let pattern = required(&args, "pattern")?;
    let max_results = max_results(&args);
    let mut command = Command::new("rg");
    command
        .arg("--no-config")
        .arg("--line-number")
        .arg("--no-messages")
        .arg("--hidden")
        .args(search_exclude_args(&search_root))
        .arg(pattern);
    if let Some(include) = args
        .get("include")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
    {
        command.arg("--iglob").arg(include.trim());
    }
    if is_file {
        if let Some(name) = path.file_name() {
            command.arg(name);
        }
    } else {
        command.arg(".");
    }
    let output = tokio::time::timeout(
        std::time::Duration::from_secs(SEARCH_TIMEOUT_SECONDS),
        command
            .current_dir(search_root)
            .stdin(Stdio::null())
            .output(),
    )
    .await??;
    search_output_limited(output, max_results)
}

async fn run_command(args: Value, allowed: bool) -> Result<String> {
    if !allowed {
        bail!("{}", t("command execution is disabled; set skills.allow_command_execution=true in config.jsonc to enable run_command", "命令执行已禁用；请在 config.jsonc 中设置 skills.allow_command_execution=true 以启用 run_command"));
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

async fn run_readonly_command(args: Value) -> Result<String> {
    let command = required(&args, "command")?;
    ensure_readonly_command(&command)?;
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

fn ensure_readonly_command(command: &str) -> Result<()> {
    let lower = command.to_ascii_lowercase();
    let forbidden = [
        ">",
        ">>",
        " 2>",
        "tee ",
        "tee -",
        "rm ",
        "mv ",
        "cp ",
        "mkdir ",
        "rmdir ",
        "touch ",
        "chmod ",
        "chown ",
        "chgrp ",
        "ln ",
        "truncate ",
        "dd ",
        "mkfs",
        "mount ",
        "umount ",
        "systemctl ",
        "service ",
        "kill ",
        "pkill ",
        "reboot",
        "shutdown",
        "poweroff",
        "pacman -s",
        "pacman -r",
        "pacman -u",
        "paru -s",
        "yay -s",
        "apt install",
        "apt remove",
        "apt update",
        "dnf install",
        "brew install",
        "sed -i",
        "git add",
        "git commit",
        "git push",
        "git reset",
        "git checkout",
        "cargo build",
        "cargo test",
        "make ",
        "npm install",
        "pnpm install",
        "yarn install",
    ];
    if forbidden.iter().any(|needle| lower.contains(needle)) {
        bail!(
            "{}",
            t(
                "Plan mode only allows read-only inspection commands",
                "计划模式只允许只读检查命令"
            )
        );
    }
    Ok(())
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

fn search_output_limited(output: std::process::Output, max_lines: usize) -> Result<String> {
    if output.status.code() == Some(1) && output.stdout.is_empty() {
        return Ok(serde_json::to_string_pretty(&json!({
            "success": true,
            "exit_code": 0,
            "stdout": "",
            "stderr": clip_output(&String::from_utf8_lossy(&output.stderr)),
            "truncated": false,
            "max_results": max_lines,
            "matches": 0,
            "note": "no matches"
        }))?);
    }
    command_output_limited(output, max_lines)
}

fn prepare_search_path(path: &Path) -> Result<PathBuf> {
    let path = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    if path == Path::new("/usr") || path == Path::new("/var") || path == Path::new("/etc") {
        bail!(
            "refusing broad system search path: {}; use / for protected global search or choose a specific subdirectory",
            path.display()
        );
    }
    Ok(path)
}

fn search_exclude_args(search_root: &Path) -> Vec<String> {
    let mut args = vec!["--glob=!**/.git/**".to_string()];
    if search_root == Path::new("/") {
        args.extend(
            [
                "--glob=!dev/**",
                "--glob=!proc/**",
                "--glob=!sys/**",
                "--glob=!run/**",
                "--glob=!tmp/**",
                "--glob=!var/cache/**",
                "--glob=!var/lib/**",
                "--glob=!var/log/**",
                "--glob=!usr/**",
                "--glob=!nix/**",
                "--glob=!snap/**",
                "--glob=!flatpak/**",
            ]
            .into_iter()
            .map(ToString::to_string),
        );
    }
    args
}

fn ensure_not_binary_file(path: &Path) -> Result<()> {
    let mut file = std::fs::File::open(path)?;
    let mut buffer = [0u8; 8192];
    let read = file.read(&mut buffer)?;
    let sample = &buffer[..read];
    if sample.contains(&0) {
        bail!("cannot read binary file: {}", path.display())
    }
    let non_printable = sample
        .iter()
        .filter(|byte| **byte < 9 || (**byte > 13 && **byte < 32))
        .count();
    if !sample.is_empty() && non_printable * 10 > sample.len() * 3 {
        bail!("cannot read binary file: {}", path.display())
    }
    Ok(())
}

fn ensure_editable_file_path(path: &Path) -> Result<()> {
    let canonical = path.canonicalize()?;
    if !canonical.is_file() {
        bail!("not a regular file: {}", path.display())
    }
    Ok(())
}

fn resolve_existing_path_without_following_leaf(path: &Path) -> Result<PathBuf> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let filename = path
        .file_name()
        .ok_or_else(|| anyhow::anyhow!("refusing to trash a root path: {}", path.display()))?;
    let parent = parent.canonicalize()?;
    let resolved = parent.join(filename);
    std::fs::symlink_metadata(&resolved)?;
    Ok(resolved)
}

fn ensure_safe_trash_target(path: &Path) -> Result<()> {
    let cwd = std::env::current_dir()?.canonicalize()?;
    let home = directories::BaseDirs::new().map(|dirs| dirs.home_dir().to_path_buf());
    let dangerous = [
        Path::new("/"),
        Path::new("/bin"),
        Path::new("/boot"),
        Path::new("/dev"),
        Path::new("/etc"),
        Path::new("/home"),
        Path::new("/opt"),
        Path::new("/proc"),
        Path::new("/root"),
        Path::new("/run"),
        Path::new("/sbin"),
        Path::new("/sys"),
        Path::new("/tmp"),
        Path::new("/usr"),
        Path::new("/var"),
    ];
    if dangerous.iter().any(|item| path == *item) {
        bail!(
            "refusing to trash dangerous system path: {}",
            path.display()
        )
    }
    if path == cwd {
        bail!(
            "refusing to trash current workspace root: {}",
            path.display()
        )
    }
    if let Some(home) = home {
        if path == home {
            bail!("refusing to trash home directory: {}", path.display())
        }
        let trash_dir = home.join(".local/share/Trash");
        if path == trash_dir || path.starts_with(&trash_dir) {
            bail!(
                "refusing to trash the Trash directory itself: {}",
                path.display()
            )
        }
    }
    Ok(())
}

fn path_kind(metadata: &std::fs::Metadata) -> &'static str {
    let file_type = metadata.file_type();
    if file_type.is_symlink() {
        "symlink"
    } else if file_type.is_dir() {
        "directory"
    } else if file_type.is_file() {
        "file"
    } else {
        "other"
    }
}

fn current_unix_seconds() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0)
}

fn find_recent_trash_item(original_path: &Path, timestamp_before: i64) -> Option<String> {
    #[cfg(any(
        target_os = "windows",
        all(
            unix,
            not(target_os = "macos"),
            not(target_os = "ios"),
            not(target_os = "android")
        )
    ))]
    {
        let items = trash::os_limited::list().ok()?;
        items
            .into_iter()
            .filter(|item| item.original_path() == original_path)
            .filter(|item| {
                item.time_deleted < 0 || item.time_deleted >= timestamp_before.saturating_sub(2)
            })
            .max_by_key(|item| item.time_deleted)
            .map(|item| item.id.to_string_lossy().to_string())
    }
    #[cfg(not(any(
        target_os = "windows",
        all(
            unix,
            not(target_os = "macos"),
            not(target_os = "ios"),
            not(target_os = "android")
        )
    )))]
    {
        let _ = original_path;
        let _ = timestamp_before;
        None
    }
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
            "{}\n...[{} {MAX_COMMAND_OUTPUT_CHARS} {}]",
            value
                .chars()
                .take(MAX_COMMAND_OUTPUT_CHARS)
                .collect::<String>(),
            t("truncated to", "已截断到"),
            t("chars", "字符")
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
        bail!("{}: {key}", t("required argument missing", "缺少必需参数"))
    } else {
        Ok(value.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn readonly_command_allows_inspection() {
        assert!(ensure_readonly_command("git status --short").is_ok());
        assert!(ensure_readonly_command("pacman -Q miyu").is_ok());
    }

    #[test]
    fn readonly_command_blocks_mutation() {
        assert!(ensure_readonly_command("rm file").is_err());
        assert!(ensure_readonly_command("sed -i 's/a/b/' file").is_err());
        assert!(ensure_readonly_command("cargo test").is_err());
    }

    #[test]
    fn edit_file_replaces_lines() {
        let cwd = std::env::current_dir().unwrap();
        let temp = tempfile::tempdir_in(cwd).unwrap();
        let path = temp.path().join("sample.txt");
        std::fs::write(&path, "one\ntwo\nthree\n").unwrap();
        let result = edit_file(json!({
            "path": path.display().to_string(),
            "start_line": 2,
            "end_line": 2,
            "replacement": "TWO\nTWO-B"
        }));
        result.unwrap();
        assert_eq!(
            std::fs::read_to_string(path).unwrap(),
            "one\nTWO\nTWO-B\nthree\n"
        );
    }

    #[test]
    fn edit_file_allows_existing_files_outside_workspace() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("sample.txt");
        std::fs::write(&path, "one\ntwo\n").unwrap();
        edit_file(json!({
            "path": path.display().to_string(),
            "start_line": 1,
            "end_line": 2,
            "replacement": "table"
        }))
        .unwrap();
        assert_eq!(std::fs::read_to_string(path).unwrap(), "table\n");
    }

    #[test]
    fn read_file_paginates_text() {
        let cwd = std::env::current_dir().unwrap();
        let temp = tempfile::tempdir_in(cwd).unwrap();
        let path = temp.path().join("sample.txt");
        std::fs::write(&path, "one\ntwo\nthree\n").unwrap();
        let result = read_file(json!({
            "path": path.display().to_string(),
            "offset": 2,
            "limit": 1,
        }))
        .unwrap();
        let data: Value = serde_json::from_str(&result).unwrap();
        assert_eq!(data["type"], "text-page");
        assert_eq!(data["content"], "2: two");
        assert_eq!(data["truncated"], true);
        assert_eq!(data["next"], 3);
    }

    #[test]
    fn read_file_rejects_binary() {
        let cwd = std::env::current_dir().unwrap();
        let temp = tempfile::tempdir_in(cwd).unwrap();
        let path = temp.path().join("sample.bin");
        std::fs::write(&path, [0, 1, 2, 3]).unwrap();
        assert!(read_file(json!({"path": path.display().to_string()})).is_err());
    }

    #[tokio::test]
    async fn glob_files_matches_filename_case_insensitively() {
        let cwd = std::env::current_dir().unwrap();
        let temp = tempfile::tempdir_in(cwd).unwrap();
        let path = temp.path().join("ai测试题.txt");
        std::fs::write(&path, "content").unwrap();
        let result = glob_files(json!({
            "path": temp.path().display().to_string(),
            "pattern": "*Ai*测试*",
        }))
        .await
        .unwrap();
        let data: Value = serde_json::from_str(&result).unwrap();
        assert_eq!(data["success"], true);
        assert!(data["stdout"].as_str().unwrap().contains("ai测试题.txt"));
    }

    #[tokio::test]
    async fn grep_no_matches_is_successful_empty_result() {
        let cwd = std::env::current_dir().unwrap();
        let temp = tempfile::tempdir_in(cwd).unwrap();
        std::fs::write(temp.path().join("sample.txt"), "hello").unwrap();
        let result = grep_text(json!({
            "path": temp.path().display().to_string(),
            "pattern": "definitely-not-present",
        }))
        .await
        .unwrap();
        let data: Value = serde_json::from_str(&result).unwrap();
        assert_eq!(data["success"], true);
        assert_eq!(data["exit_code"], 0);
        assert_eq!(data["stdout"], "");
        assert_eq!(data["note"], "no matches");
    }

    #[test]
    fn root_search_uses_protective_excludes() {
        let root = Path::new("/");
        assert!(prepare_search_path(root).is_ok());
        let args = search_exclude_args(root).join(" ");
        assert!(args.contains("--glob=!proc/**"));
        assert!(args.contains("--glob=!usr/**"));
    }

    #[test]
    fn trash_path_rejects_workspace_root() {
        let cwd = std::env::current_dir().unwrap().canonicalize().unwrap();
        assert!(ensure_safe_trash_target(&cwd).is_err());
    }

    #[test]
    fn trash_path_moves_file_to_trash() {
        let cwd = std::env::current_dir().unwrap();
        let temp = tempfile::tempdir_in(cwd).unwrap();
        let path = temp.path().join("trash-me.txt");
        std::fs::write(&path, "bye").unwrap();
        let result = trash_path(json!({"path": path.display().to_string()})).unwrap();
        let data: Value = serde_json::from_str(&result).unwrap();
        assert_eq!(data["ok"], true);
        assert_eq!(data["kind"], "file");
        assert_eq!(data["exists_after"], false);
        assert!(!path.exists());
    }

    #[test]
    fn trash_path_moves_directory_to_trash() {
        let cwd = std::env::current_dir().unwrap();
        let temp = tempfile::tempdir_in(cwd).unwrap();
        let path = temp.path().join("trash-dir");
        std::fs::create_dir(&path).unwrap();
        std::fs::write(path.join("child.txt"), "bye").unwrap();
        let result = trash_path(json!({"path": path.display().to_string()})).unwrap();
        let data: Value = serde_json::from_str(&result).unwrap();
        assert_eq!(data["ok"], true);
        assert_eq!(data["kind"], "directory");
        assert_eq!(data["exists_after"], false);
        assert!(!path.exists());
    }
}
