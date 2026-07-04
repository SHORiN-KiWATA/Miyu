use super::{ToolRegistry, ToolSpec};
use crate::i18n::text as t;
use crate::paths::MiyuPaths;
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use tokio::process::Command;

const SCRIPT_TIMEOUT_SECS: u64 = 120;
const MAX_SCRIPT_OUTPUT_CHARS: usize = 20_000;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct ScriptIndex {
    #[serde(default)]
    scripts: Vec<ScriptEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ScriptEntry {
    id: String,
    #[serde(default)]
    display_name: String,
    description: String,
    #[serde(default)]
    path: String,
    #[serde(default)]
    parameters: Value,
    #[serde(default)]
    timeout_seconds: Option<u64>,
}

pub fn register(registry: &mut ToolRegistry, paths: &MiyuPaths) {
    let dirs = [paths.system_scripts_dir.as_path(), paths.scripts_dir.as_path()];
    let entries = match scan_scripts(&dirs) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries {
        if let Ok(spec) = entry_to_spec(&entry, &paths.scripts_dir) {
            registry.register(spec);
        }
    }
    register_script_tools(
        registry,
        paths.scripts_dir.clone(),
        paths.system_scripts_dir.clone(),
    );
}

pub fn rescan_scripts(registry: &mut ToolRegistry, paths: &MiyuPaths) {
    let dirs = [paths.system_scripts_dir.as_path(), paths.scripts_dir.as_path()];
    let entries = match scan_scripts(&dirs) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries {
        if registry.get(&entry.id).is_none() {
            if let Ok(spec) = entry_to_spec(&entry, &paths.scripts_dir) {
                registry.register(spec);
            }
        }
    }
}

fn scan_scripts(dirs: &[&Path]) -> Result<Vec<ScriptEntry>> {
    let mut entries: Vec<ScriptEntry> = Vec::new();
    let mut id_index: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    let mut seen_paths = std::collections::BTreeSet::new();

    for scripts_dir in dirs {
        if !scripts_dir.is_dir() {
            continue;
        }

        let index_path = scripts_dir.join("index.json");
        let indexed: Vec<ScriptEntry> = if index_path.is_file() {
            let raw = std::fs::read_to_string(&index_path)?;
            let index: ScriptIndex = serde_json::from_str(&raw).unwrap_or_default();
            index.scripts
        } else {
            Vec::new()
        };

        for entry in &indexed {
            let path = resolve_script_path(&entry.path, scripts_dir);
            if !path.is_file() {
                continue;
            }
            let canon = canonicalize_key(&path);
            seen_paths.insert(canon);
            let mut entry = entry.clone();
            entry.path = path.to_string_lossy().to_string();
            if let Some(&idx) = id_index.get(&entry.id) {
                entries[idx] = entry;
            } else {
                id_index.insert(entry.id.clone(), entries.len());
                entries.push(entry);
            }
        }

        for file_entry in std::fs::read_dir(scripts_dir)? {
            let file_entry = file_entry?;
            let path = file_entry.path();
            if !path.is_file() {
                continue;
            }
            let fname = file_entry.file_name().to_string_lossy().to_string();
            if fname == "index.json" || fname.starts_with('.') {
                continue;
            }
            if let Some(mut entry) = auto_detect_script(&path) {
                let canon = canonicalize_key(&path);
                if !seen_paths.insert(canon) {
                    continue;
                }
                entry.path = path.to_string_lossy().to_string();
                if let Some(&idx) = id_index.get(&entry.id) {
                    entries[idx] = entry;
                } else {
                    id_index.insert(entry.id.clone(), entries.len());
                    entries.push(entry);
                }
            }
        }
    }

    Ok(entries)
}

fn canonicalize_key(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

fn resolve_script_path(path_str: &str, scripts_dir: &Path) -> PathBuf {
    let p = Path::new(path_str);
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        scripts_dir.join(p)
    }
}

fn auto_detect_script(path: &Path) -> Option<ScriptEntry> {
    let raw = std::fs::read_to_string(path).ok()?;
    let first_line = raw.lines().next()?;
    if !first_line.starts_with("#!") {
        return None;
    }
    let id = path
        .file_stem()
        .and_then(|n| n.to_str())
        .unwrap_or("script")
        .to_string();
    let display_name = id.clone();
    let description = extract_description(&raw)
        .unwrap_or_else(|| format!("User script: {id}"));
    let parameters = json!({
        "type": "object",
        "properties": {
            "stdin": {
                "type": "string",
                "description": t("Optional raw stdin input. If omitted, all arguments are sent as JSON via stdin.", "可选的原始 stdin 输入。省略时所有参数以 JSON 形式通过 stdin 传入。")
            }
        },
        "additionalProperties": true
    });
    Some(ScriptEntry {
        id,
        display_name,
        description,
        path: path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("")
            .to_string(),
        parameters,
        timeout_seconds: None,
    })
}

fn extract_description(raw: &str) -> Option<String> {
    for line in raw.lines().skip(1) {
        let trimmed = line.trim_start_matches('#').trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Some(desc) = trimmed.strip_prefix("description:") {
            return Some(desc.trim().to_string());
        }
        if let Some(desc) = trimmed.strip_prefix("功能介绍:") {
            return Some(desc.trim().to_string());
        }
        if !trimmed.starts_with("#!") {
            break;
        }
    }
    None
}

fn entry_to_spec(entry: &ScriptEntry, scripts_dir: &Path) -> Result<ToolSpec> {
    let id = entry.id.clone();
    if id.is_empty() {
        bail!("script id is empty");
    }
    let display_name = if entry.display_name.is_empty() {
        id.clone()
    } else {
        entry.display_name.clone()
    };
    let description = if entry.description.is_empty() {
        format!("User script: {id}")
    } else {
        entry.description.clone()
    };
    let parameters = if entry.parameters.is_null() {
        json!({
            "type": "object",
            "properties": {
                "stdin": {
                    "type": "string",
                    "description": t("Optional raw stdin input. If omitted, all arguments are sent as JSON via stdin.", "可选的原始 stdin 输入。省略时所有参数以 JSON 形式通过 stdin 传入。")
                }
            },
            "additionalProperties": true
        })
    } else {
        entry.parameters.clone()
    };
    let timeout = entry.timeout_seconds.unwrap_or(SCRIPT_TIMEOUT_SECS).min(300);
    let path_str = entry.path.clone();
    let scripts_dir = scripts_dir.to_path_buf();

    let spec = ToolSpec::new(
        id,
        description,
        parameters,
        move |args| {
            let path_str = path_str.clone();
            let scripts_dir = scripts_dir.clone();
            async move { run_script(&path_str, &scripts_dir, &args, timeout).await }
        },
    )
    .writes()
    .with_display_name(display_name);
    Ok(spec)
}

async fn run_script(
    path_str: &str,
    scripts_dir: &Path,
    args: &Value,
    timeout_secs: u64,
) -> Result<String> {
    let script_path = resolve_script_path(path_str, scripts_dir);

    if !script_path.is_file() {
        bail!("script not found: {}", script_path.display());
    }

    let stdin_input = if let Some(text) = args.get("stdin").and_then(Value::as_str) {
        if !text.is_empty() {
            text.to_string()
        } else {
            serde_json::to_string(args).unwrap_or_default()
        }
    } else {
        serde_json::to_string(args).unwrap_or_default()
    };

    let mut command = Command::new(&script_path);
    command.stdin(Stdio::piped());
    command.stdout(Stdio::piped());
    command.stderr(Stdio::piped());

    let mut child = command.spawn()?;
    if !stdin_input.is_empty() {
        if let Some(mut stdin) = child.stdin.take() {
            use tokio::io::AsyncWriteExt;
            let _ = stdin.write_all(stdin_input.as_bytes()).await;
        }
    }

    let output = tokio::time::timeout(
        std::time::Duration::from_secs(timeout_secs),
        child.wait_with_output(),
    )
    .await
    .map_err(|_| anyhow::anyhow!("script timed out after {timeout_secs}s"))??;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = clip_output(stdout.trim());
    let stderr = clip_output(stderr.trim());

    Ok(serde_json::to_string_pretty(&json!({
        "success": output.status.success(),
        "exit_code": output.status.code(),
        "stdout": stdout,
        "stderr": stderr,
    }))?)
}

fn clip_output(value: &str) -> String {
    if value.chars().count() <= MAX_SCRIPT_OUTPUT_CHARS {
        value.to_string()
    } else {
        format!(
            "{}\n...[{} {MAX_SCRIPT_OUTPUT_CHARS} {}]",
            value.chars().take(MAX_SCRIPT_OUTPUT_CHARS).collect::<String>(),
            t("truncated to", "已截断到"),
            t("chars", "字符")
        )
    }
}

fn make_executable(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(path)?.permissions();
        perms.set_mode(perms.mode() | 0o111);
        std::fs::set_permissions(path, perms)?;
    }
    Ok(())
}

fn register_script_tools(registry: &mut ToolRegistry, scripts_dir: PathBuf, system_scripts_dir: PathBuf) {
    let scripts_dir_2 = scripts_dir.clone();
    let scripts_dir_3 = scripts_dir.clone();
    let system_scripts_dir_2 = system_scripts_dir.clone();
    registry.register(ToolSpec::new(
        "register_script",
        t(
            "Register or update a user script as a tool. The script must exist in the scripts directory. This updates index.json, sets executable permission, and makes the script immediately available as a tool in subsequent tool rounds.",
            "注册或更新用户脚本为工具。脚本必须存在于 scripts 目录中。此操作更新 index.json、设置可执行权限，并使脚本在后续工具调用轮次中立即可用。"
        ),
        json!({
            "type": "object",
            "properties": {
                "id": {
                    "type": "string",
                    "pattern": "^[a-zA-Z][a-zA-Z0-9_]*$",
                    "description": t("Unique tool identifier (ASCII, starts with a letter). This is the function name the AI calls.", "唯一工具标识符（ASCII，字母开头）。这是 AI 调用的函数名。")
                },
                "display_name": {
                    "type": "string",
                    "description": t("Human-readable display name, may contain Chinese characters.", "可读显示名称，可包含中文。")
                },
                "description": {
                    "type": "string",
                    "description": t("Tool description shown to the AI.", "展示给 AI 的工具描述。")
                },
                "path": {
                    "type": "string",
                    "description": t("Script file name or relative/absolute path.", "脚本文件名或相对/绝对路径。")
                },
                "parameters": {
                    "type": "object",
                    "description": t("JSON schema for tool parameters. If omitted, a generic schema with stdin is used.", "工具参数的 JSON schema。省略时使用带 stdin 的通用 schema。")
                },
                "timeout_seconds": {
                    "type": "integer",
                    "description": t("Optional timeout in seconds, max 300.", "可选超时时间，单位秒，最大 300。")
                }
            },
            "required": ["id", "display_name", "description", "path"],
            "additionalProperties": false
        }),
        move |args| {
            let scripts_dir = scripts_dir.clone();
            async move { register_script_handler(args, &scripts_dir).await }
        },
    ).writes());

    registry.register(ToolSpec::new(
        "unregister_script",
        t(
            "Remove a registered script from the tool index. Optionally delete the script file if it resides within the scripts directory.",
            "从工具索引中移除已注册的脚本。如果脚本文件位于 scripts 目录内，可选删除文件。"
        ),
        json!({
            "type": "object",
            "properties": {
                "id": {
                    "type": "string",
                    "description": t("The script id to unregister.", "要注销的脚本 id。")
                },
                "delete_file": {
                    "type": "boolean",
                    "description": t("If true, delete the script file from disk. Only affects files within the scripts directory.", "若为 true，同时从磁盘删除脚本文件。仅影响 scripts 目录内的文件。")
                }
            },
            "required": ["id"],
            "additionalProperties": false
        }),
        move |args| {
            let scripts_dir = scripts_dir_2.clone();
            async move { unregister_script_handler(args, &scripts_dir).await }
        },
    ).writes());

    registry.register(ToolSpec::new(
        "list_scripts",
        t(
            "List all registered script tools, including both system and user scripts. Returns id, display name, description, path, and source for each script.",
            "列出所有已注册的脚本工具，包括系统脚本和用户脚本。返回每个脚本的 id、显示名称、描述、路径和来源。"
        ),
        json!({
            "type": "object",
            "properties": {},
            "additionalProperties": false
        }),
        move |_args| {
            let scripts_dir = scripts_dir_3.clone();
            let system_scripts_dir = system_scripts_dir_2.clone();
            async move { list_scripts_handler(&scripts_dir, &system_scripts_dir).await }
        },
    ));
}

async fn register_script_handler(args: Value, scripts_dir: &Path) -> Result<String> {
    let id = args
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim()
        .to_string();
    if id.is_empty() {
        bail!("id is required");
    }
    if !id.chars().next().map(|c| c.is_ascii_alphabetic()).unwrap_or(false)
        || !id.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
    {
        bail!("id must start with an ASCII letter and contain only ASCII alphanumeric and underscore");
    }
    let display_name = args
        .get("display_name")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim()
        .to_string();
    let description = args
        .get("description")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let path = args
        .get("path")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim()
        .to_string();
    if path.is_empty() {
        bail!("path is required");
    }
    let script_path = resolve_script_path(&path, scripts_dir);
    if !script_path.is_file() {
        bail!("script file not found: {}", script_path.display());
    }
    make_executable(&script_path)?;

    let parameters = args.get("parameters").cloned().unwrap_or(Value::Null);
    let timeout_seconds = args
        .get("timeout_seconds")
        .and_then(Value::as_u64)
        .map(|v| v.min(300));

    let entry = ScriptEntry {
        id: id.clone(),
        display_name: if display_name.is_empty() {
            id.clone()
        } else {
            display_name
        },
        description,
        path,
        parameters,
        timeout_seconds,
    };

    let index_path = scripts_dir.join("index.json");
    let mut index: ScriptIndex = if index_path.is_file() {
        let raw = std::fs::read_to_string(&index_path)?;
        serde_json::from_str(&raw).unwrap_or_default()
    } else {
        ScriptIndex::default()
    };

    if let Some(existing) = index.scripts.iter_mut().find(|s| s.id == id) {
        *existing = entry.clone();
    } else {
        index.scripts.push(entry.clone());
    }

    std::fs::write(&index_path, serde_json::to_string_pretty(&index)?)
        .with_context(|| format!("failed to write {}", index_path.display()))?;

    Ok(format!(
        "Script '{id}' registered successfully. It will be available as a tool in the next tool call round. The script path is: {}",
        script_path.display()
    ))
}

async fn unregister_script_handler(args: Value, scripts_dir: &Path) -> Result<String> {
    let id = args
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim()
        .to_string();
    if id.is_empty() {
        bail!("id is required");
    }
    let delete_file = args
        .get("delete_file")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    let index_path = scripts_dir.join("index.json");
    if !index_path.is_file() {
        bail!("no scripts registered (index.json not found)");
    }

    let raw = std::fs::read_to_string(&index_path)?;
    let mut index: ScriptIndex = serde_json::from_str(&raw).unwrap_or_default();

    let entry = index.scripts.iter().find(|s| s.id == id).cloned();
    let Some(entry) = entry else {
        bail!("script id '{id}' not found in index");
    };

    index.scripts.retain(|s| s.id != id);
    std::fs::write(&index_path, serde_json::to_string_pretty(&index)?)?;

    let mut deleted_file = false;
    if delete_file {
        let script_path = resolve_script_path(&entry.path, scripts_dir);
        let canon_script = script_path
            .canonicalize()
            .unwrap_or_else(|_| script_path.clone());
        let canon_dir = scripts_dir
            .canonicalize()
            .unwrap_or_else(|_| scripts_dir.to_path_buf());
        if canon_script.starts_with(&canon_dir) && canon_script.is_file() {
            std::fs::remove_file(&canon_script)?;
            deleted_file = true;
        }
    }

    Ok(format!(
        "Script '{}' unregistered successfully{}.",
        id,
        if deleted_file { " and file deleted" } else { "" }
    ))
}

async fn list_scripts_handler(scripts_dir: &Path, system_scripts_dir: &Path) -> Result<String> {
    let dirs = [system_scripts_dir, scripts_dir];
    let entries = scan_scripts(&dirs)?;

    let canon_sys = system_scripts_dir
        .canonicalize()
        .unwrap_or_else(|_| system_scripts_dir.to_path_buf());

    let scripts: Vec<Value> = entries
        .iter()
        .map(|entry| {
            let path = Path::new(&entry.path);
            let canon_path = path
                .canonicalize()
                .unwrap_or_else(|_| path.to_path_buf());
            let source = if canon_path.starts_with(&canon_sys) {
                "system"
            } else {
                "user"
            };
            json!({
                "id": entry.id,
                "display_name": entry.display_name,
                "description": entry.description,
                "path": entry.path,
                "source": source,
            })
        })
        .collect();

    Ok(serde_json::to_string_pretty(&json!({
        "scripts": scripts,
        "total": entries.len(),
    }))?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_description_from_shebang_script() {
        let raw = "#!/bin/bash\ndescription: Check system status\n\necho ok";
        assert_eq!(
            extract_description(raw),
            Some("Check system status".to_string())
        );
    }

    #[test]
    fn extracts_chinese_description() {
        let raw = "#!/usr/bin/env python3\n功能介绍: 检查系统状态\n\nprint('ok')";
        assert_eq!(
            extract_description(raw),
            Some("检查系统状态".to_string())
        );
    }

    #[test]
    fn returns_none_when_no_description() {
        let raw = "#!/bin/bash\necho hello";
        assert_eq!(extract_description(raw), None);
    }

    #[test]
    fn auto_detects_executable_script() {
        let temp = tempfile::tempdir().unwrap();
        let script_path = temp.path().join("hello.sh");
        std::fs::write(
            &script_path,
            "#!/bin/bash\ndescription: Say hello\n\necho hello",
        )
        .unwrap();
        let entry = auto_detect_script(&script_path).unwrap();
        assert_eq!(entry.id, "hello");
        assert_eq!(entry.display_name, "hello");
        assert_eq!(entry.description, "Say hello");
        assert_eq!(entry.path, "hello.sh");
    }

    #[test]
    fn scan_finds_auto_detected_scripts() {
        let temp = tempfile::tempdir().unwrap();
        let scripts_dir = temp.path();
        std::fs::write(
            scripts_dir.join("greet.sh"),
            "#!/bin/bash\ndescription: Greet user\n\necho hi",
        )
        .unwrap();
        let entries = scan_scripts(&[scripts_dir]).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].id, "greet");
    }

    #[test]
    fn scan_merges_index_and_auto_detected() {
        let temp = tempfile::tempdir().unwrap();
        let scripts_dir = temp.path();
        std::fs::write(
            scripts_dir.join("index.json"),
            r#"{"scripts":[{"id":"custom","display_name":"自定义","description":"Custom tool","path":"custom.sh"}]}"#,
        )
        .unwrap();
        std::fs::write(
            scripts_dir.join("custom.sh"),
            "#!/bin/bash\necho custom",
        )
        .unwrap();
        std::fs::write(
            scripts_dir.join("auto.sh"),
            "#!/bin/bash\ndescription: Auto detected\n\necho auto",
        )
        .unwrap();
        let entries = scan_scripts(&[scripts_dir]).unwrap();
        assert_eq!(entries.len(), 2);
        let ids: Vec<&str> = entries.iter().map(|e| e.id.as_str()).collect();
        assert!(ids.contains(&"custom"));
        assert!(ids.contains(&"auto"));
    }

    #[test]
    fn scan_deduplicates_by_path() {
        let temp = tempfile::tempdir().unwrap();
        let scripts_dir = temp.path();
        let script = scripts_dir.join("dup.sh");
        std::fs::write(&script, "#!/bin/bash\ndescription: Dup\n\necho dup").unwrap();
        std::fs::write(
            scripts_dir.join("index.json"),
            r#"{"scripts":[{"id":"alias1","display_name":"A1","description":"alias","path":"dup.sh"}]}"#,
        )
        .unwrap();
        let entries = scan_scripts(&[scripts_dir]).unwrap();
        assert_eq!(entries.len(), 1);
    }

    #[test]
    fn scan_user_dir_overrides_system_dir() {
        let sys_temp = tempfile::tempdir().unwrap();
        let user_temp = tempfile::tempdir().unwrap();
        std::fs::write(
            sys_temp.path().join("tool.sh"),
            "#!/bin/bash\ndescription: System version\n\necho sys",
        )
        .unwrap();
        std::fs::write(
            user_temp.path().join("tool.sh"),
            "#!/bin/bash\ndescription: User version\n\necho user",
        )
        .unwrap();
        let entries = scan_scripts(&[sys_temp.path(), user_temp.path()]).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].description, "User version");
    }

    #[cfg(unix)]
    #[test]
    fn make_executable_sets_x_bit() {
        let temp = tempfile::tempdir().unwrap();
        let script = temp.path().join("test.sh");
        std::fs::write(&script, "#!/bin/bash\necho hi").unwrap();
        use std::os::unix::fs::PermissionsExt;
        let perms = std::fs::metadata(&script).unwrap().permissions();
        assert_eq!(perms.mode() & 0o111, 0);
        make_executable(&script).unwrap();
        let perms = std::fs::metadata(&script).unwrap().permissions();
        assert_ne!(perms.mode() & 0o111, 0);
    }
}
