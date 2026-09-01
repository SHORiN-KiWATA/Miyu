mod index;
pub(crate) use index::*;

use super::registry::UnregisteredScript;
use super::{ToolRegistry, ToolSpec};
use crate::i18n::is_zh;
use crate::paths::MiyuPaths;
use crate::tools::tool_descriptions::LoadPolicy;
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use tokio::process::Command;

const SCRIPT_TIMEOUT_SECS: u64 = 120;
const MAX_SCRIPT_OUTPUT_CHARS: usize = 20_000;

pub fn register(registry: &mut ToolRegistry, config: &crate::config::AppConfig, paths: &MiyuPaths) {
    // 内置脚本装在 <system>/personas/default/ 下,自定义人格天然扫不到——
    // 别人换上自定义人格拿到纯净状态(09-01)。覆盖链与四层细节见
    // script_scan_roots。
    let roots = script_scan_roots(config, paths);
    let dirs: Vec<&Path> = roots.iter().map(std::path::PathBuf::as_path).collect();
    match scan_scripts(&dirs) {
        Ok(scan) => {
            let specs = script_specs(&scan.entries, &paths.scripts_dir, &paths.cache_dir);
            if let Err(error) = registry.replace_script_tools(specs, scan.unregistered) {
                tracing::warn!(error = %error, "failed to register Miyu script tools");
            }
        }
        Err(error) => {
            tracing::warn!(error = %error, "failed to scan Miyu script directories during tool registration");
        }
    }
    register_script_tools(registry, paths.scripts_dir.clone());
}

async fn run_script(
    path_str: &str,
    scripts_dir: &Path,
    cache_dir: &Path,
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
    // 脚本的中间产物(登录 profile、会话快照、查询票据、二维码图)统一收进
    // Miyu 自己的缓存目录,`miyu wipe` 清 ~/.miyu 时一并带走,不在用户的
    // ~/.cache 下散落一堆。脚本单独拿到终端跑时这个变量不存在,退回 XDG
    // 默认——两种用法各自有各自的登录态,互不覆盖。
    command.env("MIYU_SCRIPT_CACHE_DIR", cache_dir);
    command.stdin(Stdio::piped());
    command.stdout(Stdio::piped());
    command.stderr(Stdio::piped());
    command.kill_on_drop(true);

    let mut child = command.spawn()?;
    let stdin_pipe = child.stdin.take();

    // Collect with a hard per-stream cap: wait_with_output() buffers
    // without bounds, so a runaway script could exhaust memory.
    // stdin 写入必须在 timeout 之内且与读取并发:脚本不读 stdin 且输入
    // 超过管道缓冲时 write_all 永久 pending,放在超时外整个 future 就
    // 永远不返回,kill_on_drop 也无从生效。
    let stdout_pipe = child.stdout.take();
    let stderr_pipe = child.stderr.take();
    let (status, stdout_bytes, stderr_bytes) =
        tokio::time::timeout(std::time::Duration::from_secs(timeout_secs), async {
            let write_stdin = async {
                if let Some(mut stdin) = stdin_pipe {
                    if !stdin_input.is_empty() {
                        use tokio::io::AsyncWriteExt;
                        let _ = stdin.write_all(stdin_input.as_bytes()).await;
                    }
                    // drop 关闭写端,脚本读 stdin 时拿到 EOF。
                }
            };
            let (_, stdout_bytes, stderr_bytes, status) = tokio::join!(
                write_stdin,
                read_capped_stream(stdout_pipe),
                read_capped_stream(stderr_pipe),
                child.wait(),
            );
            status.map(|status| (status, stdout_bytes, stderr_bytes))
        })
        .await
        .map_err(|_| anyhow::anyhow!("script timed out after {timeout_secs}s"))??;

    let stdout = String::from_utf8_lossy(&stdout_bytes);
    let stderr = String::from_utf8_lossy(&stderr_bytes);
    let stdout = clip_output(stdout.trim());
    let stderr = clip_output(stderr.trim());

    Ok(serde_json::to_string_pretty(&json!({
        "success": status.success(),
        "exit_code": status.code(),
        "stdout": stdout,
        "stderr": stderr,
    }))?)
}

/// Drains a child stream, keeping at most 8MB in memory.
async fn read_capped_stream(reader: Option<impl tokio::io::AsyncRead + Unpin>) -> Vec<u8> {
    use tokio::io::AsyncReadExt;
    const CAP: usize = 8 * 1024 * 1024;
    let Some(mut reader) = reader else {
        return Vec::new();
    };
    let mut output = Vec::new();
    let mut truncated = false;
    let mut buffer = [0u8; 8192];
    loop {
        match reader.read(&mut buffer).await {
            Ok(0) | Err(_) => break,
            Ok(read) => {
                let remaining = CAP.saturating_sub(output.len());
                if remaining == 0 {
                    truncated = true;
                    continue;
                }
                let take = read.min(remaining);
                if take < read {
                    truncated = true;
                }
                output.extend_from_slice(&buffer[..take]);
            }
        }
    }
    if truncated {
        output.extend_from_slice(b"\n[truncated at 8MB]");
    }
    output
}

fn clip_output(value: &str) -> String {
    if value.chars().count() <= MAX_SCRIPT_OUTPUT_CHARS {
        value.to_string()
    } else {
        format!(
            "{}\n...[{} {MAX_SCRIPT_OUTPUT_CHARS} {}]",
            value
                .chars()
                .take(MAX_SCRIPT_OUTPUT_CHARS)
                .collect::<String>(),
            "truncated to",
            "chars"
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

/// 注册与注销合并成 `manage_script`(08-17):同一份脚本索引的两种写操作。
fn register_script_tools(registry: &mut ToolRegistry, scripts_dir: PathBuf) {
    registry.register(ToolSpec::new(
        "manage_script",
        "Manage user scripts as tools. action=register adds or updates one (the script must already exist in the scripts directory; this updates index.json, sets the executable bit, and the script becomes callable in later tool rounds). action=unregister removes it from the index, optionally deleting the file.",
        json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["register", "unregister"],
                    "description": "register adds or updates, unregister removes."
                },
                "id": {
                    "type": "string",
                    "pattern": "^[a-zA-Z][a-zA-Z0-9_]*$",
                    "description": "Unique tool identifier (ASCII, starts with a letter). This is the function name the AI calls."
                },
                "display_name": {
                    "type": "string",
                    "description": "Human-readable display name, may contain Chinese characters."
                },
                "description": {
                    "type": "string",
                    "description": "Optional tool description override. If omitted, Miyu reads the script header lines `Description:`/`description:` or `描述：` and sends only one localized description to the AI."
                },
                "path": {
                    "type": "string",
                    "description": "register only: script file name or path within the user scripts directory."
                },
                "parameters": {
                    "type": "object",
                    "description": "JSON schema for tool parameters. If omitted, a generic schema with stdin is used."
                },
                "timeout_seconds": {
                    "type": "integer",
                    "description": "Optional timeout in seconds, max 300."
                },
                "always_loaded": {
                    "type": "boolean",
                    "description": "Optional loading override. By default scripts with a custom schema are loaded on demand, while scripts using generic stdin are always visible."
                },
                "load_policy": {
                    "type": "string",
                    "enum": ["summary", "group", "hidden"],
                    "description": "Hybrid catalog policy. summary shows this script as a single load target, group exposes it through group:<name>, hidden keeps it out of the catalog."
                },
                "groups": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Optional hybrid catalog groups, e.g. gaming or systeminfo."
                },
                "delete_file": {
                    "type": "boolean",
                    "description": "unregister only: also delete the script file from disk. Only affects files within the scripts directory."
                }
            },
            "required": ["action", "id"],
            "additionalProperties": false
        }),
        move |args| {
            let scripts_dir = scripts_dir.clone();
            async move {
                match args.get("action").and_then(Value::as_str).unwrap_or_default() {
                    "register" => register_script_handler(args, &scripts_dir).await,
                    "unregister" => unregister_script_handler(args, &scripts_dir).await,
                    other => bail!("unknown action: {other}; expected register or unregister"),
                }
            }
        },
    ).writes());
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
    if !is_valid_registered_script_id(&id) {
        bail!(
            "id must start with an ASCII letter and contain only ASCII alphanumeric and underscore"
        );
    }
    if is_reserved_script_id(&id) {
        bail!("script id conflicts with a reserved tool name: {id}");
    }
    let display_name = args
        .get("display_name")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim()
        .to_string();
    let description_override = args
        .get("description")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim()
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
    let unresolved_path = resolve_script_path(&path, scripts_dir);
    if !unresolved_path.is_file() {
        bail!("script file not found: {}", unresolved_path.display());
    }
    let script_path = ensure_path_within_root(&unresolved_path, scripts_dir)?;
    make_executable(&script_path)?;

    let description = if description_override.is_empty() {
        description_from_script(&script_path).unwrap_or_default()
    } else {
        description_override
    };
    if description.is_empty() {
        bail!("description is required when the script header has no Description/描述 metadata");
    }

    let parameters = args.get("parameters").cloned().unwrap_or(Value::Null);
    let timeout_seconds = args
        .get("timeout_seconds")
        .and_then(Value::as_u64)
        .map(|v| v.min(300));
    let always_loaded = args.get("always_loaded").and_then(Value::as_bool);
    let load_policy = args
        .get("load_policy")
        .and_then(Value::as_str)
        .map(parse_load_policy)
        .transpose()?
        .unwrap_or_default();
    let groups = super::string_list(args.get("groups"));
    let stored_path = relative_script_path(&script_path, scripts_dir);

    let entry = ScriptEntry {
        id: id.clone(),
        display_name: if display_name.is_empty() {
            id.clone()
        } else {
            display_name
        },
        description,
        path: stored_path.clone(),
        parameters,
        timeout_seconds,
        always_loaded,
        load_policy,
        groups,
    };

    let index_path = scripts_dir.join("index.json");
    let mut index = read_script_index_value(&index_path)?;
    {
        let scripts = index_array_mut(&mut index, "scripts")?;
        let entry = serde_json::to_value(&entry)?;
        scripts.retain(|script| raw_entry_field(script, "id") != Some(id.as_str()));
        scripts.push(entry);
    }
    let script_key = canonicalize_key(&script_path);
    index_array_mut(&mut index, "disabled")?.retain(|disabled| {
        raw_entry_field(disabled, "id") != Some(id.as_str())
            && raw_entry_field(disabled, "path")
                .map(|path| canonicalize_key(&resolve_script_path(path, scripts_dir)) != script_key)
                .unwrap_or(true)
    });

    write_script_index_value(&index_path, &index)?;

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
    let mut index = read_script_index_value(&index_path)?;

    let indexed_path = index
        .get("scripts")
        .and_then(Value::as_array)
        .and_then(|scripts| {
            scripts
                .iter()
                .filter(|script| raw_entry_field(script, "id") == Some(id.as_str()))
                .find_map(|script| raw_entry_field(script, "path"))
        })
        .map(str::to_string);
    let path = if let Some(path) = indexed_path {
        path
    } else {
        find_auto_detected_path(scripts_dir, &id)?
            .ok_or_else(|| anyhow::anyhow!("script id '{id}' not found"))?
    };

    index_array_mut(&mut index, "scripts")?
        .retain(|script| raw_entry_field(script, "id") != Some(id.as_str()));

    let mut deleted_file = false;
    let unresolved_path = resolve_script_path(&path, scripts_dir);
    if delete_file {
        if unresolved_path.is_file() {
            let script_path = ensure_path_within_root(&unresolved_path, scripts_dir)?;
            std::fs::remove_file(&script_path)?;
            deleted_file = true;
        }
        index_array_mut(&mut index, "disabled")?.retain(|disabled| {
            raw_entry_field(disabled, "id") != Some(id.as_str())
                && raw_entry_field(disabled, "path") != Some(path.as_str())
        });
    } else {
        let disabled = index_array_mut(&mut index, "disabled")?;
        disabled.retain(|entry| {
            raw_entry_field(entry, "id") != Some(id.as_str())
                && raw_entry_field(entry, "path") != Some(path.as_str())
        });
        disabled.push(json!({"id": id, "path": path}));
    }

    write_script_index_value(&index_path, &index)?;

    Ok(format!(
        "Script '{}' unregistered successfully{}.",
        id,
        if deleted_file {
            " and file deleted"
        } else {
            " and file disabled"
        }
    ))
}

#[cfg(test)]
mod tests;
