use super::registry::UnregisteredScript;
use super::{ToolRegistry, ToolSpec};
use crate::i18n::{agent_is_zh, agent_text as t, is_zh};
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

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct ScriptIndex {
    #[serde(default)]
    scripts: Vec<ScriptEntry>,
    #[serde(default)]
    disabled: Vec<DisabledScript>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct DisabledScript {
    id: String,
    path: String,
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
    #[serde(default)]
    always_loaded: Option<bool>,
    #[serde(default)]
    load_policy: LoadPolicy,
    #[serde(default)]
    groups: Vec<String>,
}

#[derive(Debug, Clone, Default)]
struct ScriptScanResult {
    entries: Vec<ScriptEntry>,
    unregistered: Vec<UnregisteredScript>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct ScriptDescriptions {
    zh: Option<String>,
    en: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct ScriptMetadata {
    descriptions: ScriptDescriptions,
    display_names: ScriptDisplayNames,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct ScriptDisplayNames {
    zh: Option<String>,
    en: Option<String>,
}

pub fn register(registry: &mut ToolRegistry, paths: &MiyuPaths) {
    let dirs = [
        paths.system_scripts_dir.as_path(),
        paths.scripts_dir.as_path(),
    ];
    if let Ok(scan) = scan_scripts(&dirs) {
        let specs = script_specs(&scan.entries, &paths.scripts_dir);
        let _ = registry.replace_script_tools(specs, scan.unregistered);
    }
    register_script_tools(registry, paths.scripts_dir.clone());
}

pub fn rescan_scripts(registry: &mut ToolRegistry, paths: &MiyuPaths) {
    let dirs = [
        paths.system_scripts_dir.as_path(),
        paths.scripts_dir.as_path(),
    ];
    let scan = match scan_scripts(&dirs) {
        Ok(scan) => scan,
        Err(_) => return,
    };
    let specs = script_specs(&scan.entries, &paths.scripts_dir);
    let _ = registry.replace_script_tools(specs, scan.unregistered);
}

fn script_specs(entries: &[ScriptEntry], scripts_dir: &Path) -> Vec<ToolSpec> {
    entries
        .iter()
        .filter_map(|entry| entry_to_spec(entry, scripts_dir).ok())
        .collect()
}

fn scan_scripts(dirs: &[&Path]) -> Result<ScriptScanResult> {
    let mut entries = BTreeMap::<String, ScriptEntry>::new();
    let mut unregistered = BTreeMap::<String, UnregisteredScript>::new();
    let mut seen_paths = BTreeSet::new();

    for scripts_dir in dirs {
        if !scripts_dir.is_dir() {
            continue;
        }

        let index_path = scripts_dir.join("index.json");
        let index = read_script_index_for_scan(&index_path)?;

        let mut disabled_ids = BTreeSet::new();
        let mut disabled_paths = BTreeSet::new();
        for disabled in &index.disabled {
            if !disabled.id.trim().is_empty() {
                disabled_ids.insert(disabled.id.clone());
                entries.remove(&disabled.id);
                unregistered.remove(&disabled.id);
            }
            if !disabled.path.trim().is_empty() {
                disabled_paths.insert(canonicalize_key(&resolve_script_path(
                    &disabled.path,
                    scripts_dir,
                )));
            }
        }

        for indexed_entry in index.scripts {
            if !is_valid_registered_script_id(&indexed_entry.id)
                || disabled_ids.contains(&indexed_entry.id)
                || is_reserved_script_id(&indexed_entry.id)
            {
                continue;
            }
            let unresolved_path = resolve_script_path(&indexed_entry.path, scripts_dir);
            if !unresolved_path.is_file() {
                continue;
            }
            let path = match ensure_path_within_root(&unresolved_path, scripts_dir) {
                Ok(path) => path,
                Err(_) => continue,
            };
            let canon = canonicalize_key(&path);
            if disabled_paths.contains(&canon) {
                continue;
            }
            seen_paths.insert(canon);

            let mut entry = indexed_entry;
            entry.path = path.to_string_lossy().to_string();
            if entry.description.trim().is_empty() {
                entry.description = description_from_script(&path).unwrap_or_default();
            }
            if entry.description.trim().is_empty() {
                entries.remove(&entry.id);
                unregistered.insert(
                    entry.id.clone(),
                    UnregisteredScript {
                        name: entry.id,
                        path: path.to_string_lossy().to_string(),
                    },
                );
            } else {
                unregistered.remove(&entry.id);
                entries.insert(entry.id.clone(), entry);
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
            let Some(detected) = inspect_script(&path) else {
                continue;
            };
            if is_reserved_script_id(&detected.id) {
                continue;
            }
            let canon = canonicalize_key(&path);
            if disabled_ids.contains(&detected.id)
                || disabled_paths.contains(&canon)
                || !seen_paths.insert(canon)
            {
                continue;
            }

            if let Some(description) = detected.description {
                let entry = ScriptEntry {
                    id: detected.id.clone(),
                    display_name: detected.display_name,
                    description,
                    path: path.to_string_lossy().to_string(),
                    parameters: Value::Null,
                    timeout_seconds: None,
                    always_loaded: Some(true),
                    load_policy: LoadPolicy::Summary,
                    groups: Vec::new(),
                };
                unregistered.remove(&detected.id);
                entries.insert(detected.id, entry);
            } else {
                entries.remove(&detected.id);
                unregistered.insert(
                    detected.id.clone(),
                    UnregisteredScript {
                        name: detected.id,
                        path: path.to_string_lossy().to_string(),
                    },
                );
            }
        }
    }

    Ok(ScriptScanResult {
        entries: entries.into_values().collect(),
        unregistered: unregistered.into_values().collect(),
    })
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

fn ensure_path_within_root(path: &Path, scripts_dir: &Path) -> Result<PathBuf> {
    let root = scripts_dir.canonicalize().with_context(|| {
        format!(
            "failed to resolve scripts directory {}",
            scripts_dir.display()
        )
    })?;
    let path = path
        .canonicalize()
        .with_context(|| format!("failed to resolve script path {}", path.display()))?;
    if !path.starts_with(&root) {
        bail!(
            "script path must stay within the scripts directory: {}",
            path.display()
        );
    }
    Ok(path)
}

fn relative_script_path(path: &Path, scripts_dir: &Path) -> String {
    let root = scripts_dir
        .canonicalize()
        .unwrap_or_else(|_| scripts_dir.to_path_buf());
    let path = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    path.strip_prefix(&root)
        .unwrap_or(&path)
        .to_string_lossy()
        .to_string()
}

fn is_reserved_script_id(id: &str) -> bool {
    id == "load_tools" || super::tool_descriptions::get(id).is_some()
}

fn is_valid_registered_script_id(id: &str) -> bool {
    id.chars()
        .next()
        .map(|character| character.is_ascii_alphabetic())
        .unwrap_or(false)
        && id
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || character == '_')
}

#[derive(Debug, Clone)]
struct DetectedScript {
    id: String,
    display_name: String,
    description: Option<String>,
}

fn inspect_script(path: &Path) -> Option<DetectedScript> {
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
    let metadata = extract_metadata(&raw);
    let display_name =
        select_script_display_name(&metadata.display_names).unwrap_or_else(|| id.clone());
    let description = select_script_description(&metadata.descriptions);
    Some(DetectedScript {
        id,
        display_name,
        description,
    })
}

#[cfg(test)]
fn auto_detect_script(path: &Path) -> Option<ScriptEntry> {
    let detected = inspect_script(path)?;
    let description = detected.description?;
    Some(ScriptEntry {
        id: detected.id,
        display_name: detected.display_name,
        description,
        path: path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("")
            .to_string(),
        parameters: Value::Null,
        timeout_seconds: None,
        always_loaded: Some(true),
        load_policy: LoadPolicy::Summary,
        groups: Vec::new(),
    })
}

fn extract_description(raw: &str) -> Option<String> {
    select_script_description(&extract_metadata(raw).descriptions)
}

fn description_from_script(path: &Path) -> Option<String> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|raw| extract_description(&raw))
}

fn select_script_description(descriptions: &ScriptDescriptions) -> Option<String> {
    let preferred = if agent_is_zh() {
        descriptions.zh.as_ref().or(descriptions.en.as_ref())
    } else {
        descriptions.en.as_ref().or(descriptions.zh.as_ref())
    }?;
    Some(preferred.clone())
}

fn select_script_display_name(display_names: &ScriptDisplayNames) -> Option<String> {
    let preferred = if is_zh() {
        display_names.zh.as_ref().or(display_names.en.as_ref())
    } else {
        display_names.en.as_ref().or(display_names.zh.as_ref())
    }?;
    Some(preferred.clone())
}

fn extract_metadata(raw: &str) -> ScriptMetadata {
    let mut metadata = ScriptMetadata::default();
    for line in raw.lines().skip(1) {
        let trimmed = line.trim_start_matches('#').trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Some((key, desc)) = split_description_line(trimmed) {
            match key {
                DescriptionKey::Chinese => metadata.descriptions.zh = Some(desc.to_string()),
                DescriptionKey::English => metadata.descriptions.en = Some(desc.to_string()),
            }
            continue;
        }
        if let Some((key, display_name)) = split_display_name_line(trimmed) {
            match key {
                DisplayNameKey::Chinese => {
                    metadata.display_names.zh = Some(display_name.to_string())
                }
                DisplayNameKey::English => {
                    metadata.display_names.en = Some(display_name.to_string())
                }
            }
            continue;
        }
        if !trimmed.starts_with("#!") {
            break;
        }
    }
    metadata
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum DescriptionKey {
    Chinese,
    English,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum DisplayNameKey {
    Chinese,
    English,
}

fn split_description_line(line: &str) -> Option<(DescriptionKey, &str)> {
    let (raw_key, raw_value) = line.split_once(':').or_else(|| line.split_once('：'))?;
    let key = raw_key.trim();
    let value = raw_value.trim();
    if value.is_empty() {
        return None;
    }
    if key == "描述" || key == "功能介绍" {
        return Some((DescriptionKey::Chinese, value));
    }
    if key.eq_ignore_ascii_case("description") {
        return Some((DescriptionKey::English, value));
    }
    None
}

fn split_display_name_line(line: &str) -> Option<(DisplayNameKey, &str)> {
    let (raw_key, raw_value) = line.split_once(':').or_else(|| line.split_once('：'))?;
    let key = raw_key.trim();
    let value = raw_value.trim();
    if value.is_empty() {
        return None;
    }
    if key == "显示名称" || key == "工具名称" {
        return Some((DisplayNameKey::Chinese, value));
    }
    if key.eq_ignore_ascii_case("display_name") || key.eq_ignore_ascii_case("display name") {
        return Some((DisplayNameKey::English, value));
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
    if entry.description.trim().is_empty() {
        bail!("registered script is missing a description: {id}");
    }
    let description = entry.description.clone();
    let always_loaded = entry
        .always_loaded
        .unwrap_or_else(|| entry.parameters.is_null());
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
    let timeout = entry
        .timeout_seconds
        .unwrap_or(SCRIPT_TIMEOUT_SECS)
        .min(300);
    let path_str = entry.path.clone();
    let scripts_dir = scripts_dir.to_path_buf();

    let spec = ToolSpec::new(id, description, parameters, move |args| {
        let path_str = path_str.clone();
        let scripts_dir = scripts_dir.clone();
        async move { run_script(&path_str, &scripts_dir, &args, timeout).await }
    })
    .writes()
    .with_display_name(display_name)
    .with_always_loaded(always_loaded)
    .with_load_policy(entry.load_policy)
    .with_groups(entry.groups.clone())
    .script();
    Ok(spec)
}

fn parse_load_policy(value: &str) -> Result<LoadPolicy> {
    match value.trim() {
        "" | "summary" | "lazy" => Ok(LoadPolicy::Summary),
        "group" => Ok(LoadPolicy::Group),
        "hidden" => Ok(LoadPolicy::Hidden),
        other => bail!("invalid load_policy: {other}"),
    }
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

    let mut command = script_command(&script_path);
    command.stdin(Stdio::piped());
    command.stdout(Stdio::piped());
    command.stderr(Stdio::piped());
    command.kill_on_drop(true);

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

fn script_command(script_path: &Path) -> Command {
    #[cfg(windows)]
    {
        let extension = script_path
            .extension()
            .and_then(|extension| extension.to_str())
            .unwrap_or_default()
            .to_ascii_lowercase();
        match extension.as_str() {
            "ps1" => {
                let mut command = Command::new("powershell.exe");
                command.args([
                    "-NoLogo",
                    "-NoProfile",
                    "-NonInteractive",
                    "-ExecutionPolicy",
                    "Bypass",
                    "-Command",
                ]);
                let path = script_path.display().to_string().replace('\'', "''");
                command.arg(format!(
                    "$OutputEncoding = [Console]::OutputEncoding = [Text.UTF8Encoding]::new($false); & '{path}'"
                ));
                return command;
            }
            "cmd" | "bat" => {
                let mut command = Command::new("cmd.exe");
                command.args(["/D", "/S", "/C"]).arg(script_path);
                return command;
            }
            "py" => {
                let mut command = Command::new("python.exe");
                command.arg(script_path);
                command.env("PYTHONUTF8", "1");
                command.env("PYTHONIOENCODING", "utf-8");
                return command;
            }
            _ => return Command::new(script_path),
        }
    }
    #[cfg(not(windows))]
    Command::new(script_path)
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
            t("truncated to", "已截断到"),
            t("chars", "字符")
        )
    }
}

fn make_executable(_path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(_path)?.permissions();
        perms.set_mode(perms.mode() | 0o111);
        std::fs::set_permissions(_path, perms)?;
    }
    Ok(())
}

fn register_script_tools(registry: &mut ToolRegistry, scripts_dir: PathBuf) {
    let scripts_dir_2 = scripts_dir.clone();
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
                    "description": t("Optional tool description override. If omitted, Miyu reads the script header lines `Description:`/`description:` or `描述：` and sends only one localized description to the AI.", "可选的工具描述覆盖。省略时 Miyu 会读取脚本头部的 `Description:`/`description:` 或 `描述：`，并只向 AI 提供一条本地化描述。")
                },
                "path": {
                    "type": "string",
                    "description": t("Script file name or path within the user scripts directory.", "用户 scripts 目录内的脚本文件名或路径。")
                },
                "parameters": {
                    "type": "object",
                    "description": t("JSON schema for tool parameters. If omitted, a generic schema with stdin is used.", "工具参数的 JSON schema。省略时使用带 stdin 的通用 schema。")
                },
                "timeout_seconds": {
                    "type": "integer",
                    "description": t("Optional timeout in seconds, max 300.", "可选超时时间，单位秒，最大 300。")
                },
                "always_loaded": {
                    "type": "boolean",
                    "description": t("Optional loading override. By default scripts with a custom schema are loaded on demand, while scripts using generic stdin are always visible.", "可选加载策略覆盖。默认有自定义 schema 的脚本按需加载，使用通用 stdin 的脚本始终可见。")
                },
                "load_policy": {
                    "type": "string",
                    "enum": ["summary", "group", "hidden"],
                    "description": t("Hybrid catalog policy. summary shows this script as a single load target, group exposes it through group:<name>, hidden keeps it out of the catalog.", "Hybrid 工具目录策略。summary 将脚本作为单独加载目标展示；group 通过 group:<name> 展示；hidden 不展示在目录中。")
                },
                "groups": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": t("Optional hybrid catalog groups, e.g. gaming or systeminfo.", "可选 Hybrid 目录分组，例如 gaming 或 systeminfo。")
                }
            },
            "required": ["id", "path"],
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
    let groups = args
        .get("groups")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_string)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
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

fn read_script_index_value(index_path: &Path) -> Result<Value> {
    if !index_path.is_file() {
        return Ok(json!({"scripts": [], "disabled": []}));
    }
    let raw = std::fs::read_to_string(index_path)?;
    let value: Value = serde_json::from_str(&raw)
        .with_context(|| format!("failed to parse {}", index_path.display()))?;
    if !value.is_object() {
        bail!(
            "script index root must be an object: {}",
            index_path.display()
        );
    }
    Ok(value)
}

fn read_script_index_for_scan(index_path: &Path) -> Result<ScriptIndex> {
    if !index_path.is_file() {
        return Ok(ScriptIndex::default());
    }
    let raw = std::fs::read_to_string(index_path)?;
    let value: Value = serde_json::from_str(&raw)
        .with_context(|| format!("failed to parse {}", index_path.display()))?;
    let scripts = value
        .get("scripts")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|entry| serde_json::from_value(entry.clone()).ok())
        .collect();
    let disabled = value
        .get("disabled")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|entry| serde_json::from_value(entry.clone()).ok())
        .collect();
    Ok(ScriptIndex { scripts, disabled })
}

fn index_array_mut<'a>(index: &'a mut Value, key: &str) -> Result<&'a mut Vec<Value>> {
    let object = index
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("script index root must be an object"))?;
    let value = object.entry(key.to_string()).or_insert_with(|| json!([]));
    if !value.is_array() {
        *value = json!([]);
    }
    Ok(value.as_array_mut().expect("array was just initialized"))
}

fn raw_entry_field<'a>(entry: &'a Value, field: &str) -> Option<&'a str> {
    entry.get(field).and_then(Value::as_str)
}

fn write_script_index_value(index_path: &Path, index: &Value) -> Result<()> {
    let file_name = index_path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("index.json");
    let temp_path = index_path.with_file_name(format!(".{file_name}.{}.tmp", std::process::id()));
    std::fs::write(&temp_path, serde_json::to_string_pretty(index)?)
        .with_context(|| format!("failed to write {}", temp_path.display()))?;
    if let Err(error) = std::fs::rename(&temp_path, index_path) {
        let _ = std::fs::remove_file(&temp_path);
        return Err(error).with_context(|| format!("failed to replace {}", index_path.display()));
    }
    Ok(())
}

fn find_auto_detected_path(scripts_dir: &Path, id: &str) -> Result<Option<String>> {
    if !scripts_dir.is_dir() {
        return Ok(None);
    }
    for file_entry in std::fs::read_dir(scripts_dir)? {
        let file_entry = file_entry?;
        let path = file_entry.path();
        if !path.is_file() {
            continue;
        }
        let Some(detected) = inspect_script(&path) else {
            continue;
        };
        if detected.id == id {
            return Ok(Some(relative_script_path(&path, scripts_dir)));
        }
    }
    Ok(None)
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
        assert_eq!(extract_description(raw), Some("检查系统状态".to_string()));
    }

    #[test]
    fn extracts_bilingual_script_descriptions() {
        let raw = "#!/bin/bash\n# 描述： Pacman/AUR安装软件的TUI\n# Description: Pacman/AUR pkg installation TUI\n\necho ok";
        assert_eq!(
            extract_metadata(raw).descriptions,
            ScriptDescriptions {
                zh: Some("Pacman/AUR安装软件的TUI".to_string()),
                en: Some("Pacman/AUR pkg installation TUI".to_string()),
            }
        );
    }

    #[test]
    fn extracts_lowercase_english_description() {
        let raw = "#!/bin/bash\n# description: Pacman/AUR pkg installation TUI\n\necho ok";
        assert_eq!(
            extract_metadata(raw).descriptions,
            ScriptDescriptions {
                zh: None,
                en: Some("Pacman/AUR pkg installation TUI".to_string()),
            }
        );
    }

    #[test]
    fn script_description_falls_back_when_locale_description_missing() {
        let english_only = ScriptDescriptions {
            zh: None,
            en: Some("English only".to_string()),
        };
        assert_eq!(
            select_script_description(&english_only),
            Some("English only".to_string())
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
    fn extracts_script_display_name_metadata() {
        let raw = "#!/bin/bash\n# 显示名称：电池护理\n# 描述：管理电池充电阈值\n\necho ok";
        let metadata = extract_metadata(raw);
        assert_eq!(metadata.display_names.zh, Some("电池护理".to_string()));
        assert_eq!(
            metadata.descriptions.zh,
            Some("管理电池充电阈值".to_string())
        );
    }

    #[test]
    fn auto_detect_uses_script_display_name() {
        let temp = tempfile::tempdir().unwrap();
        let script_path = temp.path().join("battery-care.sh");
        std::fs::write(
            &script_path,
            "#!/bin/bash\n# 显示名称：电池护理\n# 描述：管理电池充电阈值\n\necho ok",
        )
        .unwrap();
        let entry = auto_detect_script(&script_path).unwrap();
        assert_eq!(entry.id, "battery-care");
        assert_eq!(entry.display_name, "电池护理");
        assert_eq!(entry.description, "管理电池充电阈值");
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
        let scan = scan_scripts(&[scripts_dir]).unwrap();
        assert_eq!(scan.entries.len(), 1);
        assert_eq!(scan.entries[0].id, "greet");
        assert!(scan.unregistered.is_empty());
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
        std::fs::write(scripts_dir.join("custom.sh"), "#!/bin/bash\necho custom").unwrap();
        std::fs::write(
            scripts_dir.join("auto.sh"),
            "#!/bin/bash\ndescription: Auto detected\n\necho auto",
        )
        .unwrap();
        let scan = scan_scripts(&[scripts_dir]).unwrap();
        assert_eq!(scan.entries.len(), 2);
        let ids: Vec<&str> = scan.entries.iter().map(|e| e.id.as_str()).collect();
        assert!(ids.contains(&"custom"));
        assert!(ids.contains(&"auto"));
    }

    #[test]
    fn scan_fills_empty_index_description_from_script_header() {
        let temp = tempfile::tempdir().unwrap();
        let scripts_dir = temp.path();
        std::fs::write(
            scripts_dir.join("index.json"),
            r#"{"scripts":[{"id":"custom","display_name":"自定义","description":"","path":"custom.sh"}]}"#,
        )
        .unwrap();
        std::fs::write(
            scripts_dir.join("custom.sh"),
            "#!/bin/bash\n# Description: Custom header description\n\necho custom",
        )
        .unwrap();
        let scan = scan_scripts(&[scripts_dir]).unwrap();
        assert_eq!(scan.entries.len(), 1);
        assert_eq!(scan.entries[0].description, "Custom header description");
    }

    #[tokio::test]
    async fn register_script_uses_header_description_when_omitted() {
        let temp = tempfile::tempdir().unwrap();
        let scripts_dir = temp.path();
        std::fs::write(
            scripts_dir.join("pkg.sh"),
            "#!/bin/bash\n# Description: Pacman/AUR pkg installation TUI\n\necho ok",
        )
        .unwrap();

        register_script_handler(
            json!({
                "id": "pkg_install",
                "path": "pkg.sh"
            }),
            scripts_dir,
        )
        .await
        .unwrap();

        let raw = std::fs::read_to_string(scripts_dir.join("index.json")).unwrap();
        let index: ScriptIndex = serde_json::from_str(&raw).unwrap();
        assert_eq!(index.scripts.len(), 1);
        assert_eq!(
            index.scripts[0].description,
            "Pacman/AUR pkg installation TUI"
        );
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
        let scan = scan_scripts(&[scripts_dir]).unwrap();
        assert_eq!(scan.entries.len(), 1);
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
        let scan = scan_scripts(&[sys_temp.path(), user_temp.path()]).unwrap();
        assert_eq!(scan.entries.len(), 1);
        assert_eq!(scan.entries[0].description, "User version");
    }

    #[test]
    fn scan_lists_scripts_without_descriptions_as_unregistered() {
        let temp = tempfile::tempdir().unwrap();
        let scripts_dir = temp.path();
        std::fs::write(scripts_dir.join("unknown.sh"), "#!/bin/bash\necho unknown").unwrap();

        let scan = scan_scripts(&[scripts_dir]).unwrap();
        assert!(scan.entries.is_empty());
        assert_eq!(scan.unregistered.len(), 1);
        assert_eq!(scan.unregistered[0].name, "unknown");
        assert_eq!(
            scan.unregistered[0].path,
            scripts_dir.join("unknown.sh").to_string_lossy()
        );
    }

    #[test]
    fn explicit_schema_defaults_to_lazy_loading() {
        let entry = ScriptEntry {
            id: "search_game".to_string(),
            display_name: "Search game".to_string(),
            description: "Search game status".to_string(),
            path: "search-game".to_string(),
            parameters: json!({"type":"object","properties":{"query":{"type":"string"}}}),
            timeout_seconds: None,
            always_loaded: None,
            load_policy: LoadPolicy::Summary,
            groups: Vec::new(),
        };
        let spec = entry_to_spec(&entry, Path::new(".")).unwrap();
        assert!(!spec.always_loaded);
        assert!(spec.is_script);
    }

    #[test]
    fn scan_drives_top_level_and_available_script_visibility() {
        let temp = tempfile::tempdir().unwrap();
        let scripts_dir = temp.path();
        std::fs::write(
            scripts_dir.join("generic.sh"),
            "#!/bin/bash\n# Description: Generic script\n\necho generic",
        )
        .unwrap();
        std::fs::write(
            scripts_dir.join("lazy.sh"),
            "#!/bin/bash\n# Description: Lazy script\n\necho lazy",
        )
        .unwrap();
        std::fs::write(
            scripts_dir.join("index.json"),
            serde_json::to_string(&json!({
                "scripts": [
                    {
                        "id": "generic_script",
                        "display_name": "Generic",
                        "description": "Generic script",
                        "path": "generic.sh"
                    },
                    {
                        "id": "lazy_script",
                        "display_name": "Lazy",
                        "description": "Lazy script",
                        "path": "lazy.sh",
                        "parameters": {
                            "type": "object",
                            "properties": {"query": {"type": "string"}}
                        }
                    }
                ]
            }))
            .unwrap(),
        )
        .unwrap();

        let scan = scan_scripts(&[scripts_dir]).unwrap();
        let specs = script_specs(&scan.entries, scripts_dir);
        let mut registry = ToolRegistry::new();
        super::super::load_tools::register(&mut registry);
        registry
            .replace_script_tools(specs, scan.unregistered)
            .unwrap();

        let definitions = registry.lazy_definitions(&BTreeSet::new());
        let names = definitions
            .iter()
            .map(|definition| definition.function.name.as_str())
            .collect::<BTreeSet<_>>();
        assert!(names.contains("generic_script"));
        assert!(!names.contains("lazy_script"));
        let load_tools = definitions
            .iter()
            .find(|definition| definition.function.name == "load_tools")
            .unwrap();
        assert!(load_tools
            .function
            .description
            .contains("<available_load_targets>"));
        assert!(load_tools.function.description.contains("lazy_script"));
    }

    #[tokio::test]
    async fn register_rejects_reserved_tool_names_before_writing_index() {
        let temp = tempfile::tempdir().unwrap();
        let scripts_dir = temp.path();
        std::fs::write(
            scripts_dir.join("weather.sh"),
            "#!/bin/bash\n# Description: Fake weather\n\necho fake",
        )
        .unwrap();

        let error =
            register_script_handler(json!({"id":"get_weather","path":"weather.sh"}), scripts_dir)
                .await
                .unwrap_err();
        assert!(error.to_string().contains("reserved tool name"));
        assert!(!scripts_dir.join("index.json").exists());
    }

    #[test]
    fn invalid_external_index_entry_does_not_hide_valid_local_scripts() {
        let scripts_temp = tempfile::tempdir().unwrap();
        let external_temp = tempfile::tempdir().unwrap();
        let scripts_dir = scripts_temp.path();
        let external_script = external_temp.path().join("external.sh");
        std::fs::write(
            &external_script,
            "#!/bin/bash\n# Description: External\n\necho external",
        )
        .unwrap();
        std::fs::write(
            scripts_dir.join("local.sh"),
            "#!/bin/bash\n# Description: Local\n\necho local",
        )
        .unwrap();
        std::fs::write(
            scripts_dir.join("index.json"),
            serde_json::to_string(&json!({
                "scripts": [{
                    "id": "external_script",
                    "display_name": "External",
                    "description": "External",
                    "path": external_script
                }]
            }))
            .unwrap(),
        )
        .unwrap();

        let scan = scan_scripts(&[scripts_dir]).unwrap();
        assert_eq!(scan.entries.len(), 1);
        assert_eq!(scan.entries[0].id, "local");
    }

    #[test]
    fn malformed_index_entries_do_not_hide_valid_scripts() {
        let temp = tempfile::tempdir().unwrap();
        let scripts_dir = temp.path();
        std::fs::write(
            scripts_dir.join("valid.sh"),
            "#!/bin/bash\n# Description: Valid\n\necho valid",
        )
        .unwrap();
        std::fs::write(scripts_dir.join("invalid.sh"), "not a script").unwrap();
        std::fs::write(
            scripts_dir.join("index.json"),
            serde_json::to_string(&json!({
                "scripts": [
                    "broken entry",
                    {
                        "id": "",
                        "display_name": "Invalid",
                        "description": "Invalid",
                        "path": "invalid.sh"
                    },
                    {
                        "id": "valid_script",
                        "display_name": "Valid",
                        "description": "Valid",
                        "path": "valid.sh"
                    }
                ]
            }))
            .unwrap(),
        )
        .unwrap();

        let scan = scan_scripts(&[scripts_dir]).unwrap();
        assert_eq!(scan.entries.len(), 1);
        assert_eq!(scan.entries[0].id, "valid_script");
    }

    #[tokio::test]
    async fn lifecycle_mutations_preserve_malformed_sibling_entries() {
        let temp = tempfile::tempdir().unwrap();
        let scripts_dir = temp.path();
        std::fs::write(
            scripts_dir.join("existing.sh"),
            "#!/bin/bash\n# Description: Existing\n\necho existing",
        )
        .unwrap();
        std::fs::write(
            scripts_dir.join("new.sh"),
            "#!/bin/bash\n# Description: New\n\necho new",
        )
        .unwrap();
        std::fs::write(
            scripts_dir.join("index.json"),
            serde_json::to_string(&json!({
                "scripts": [
                    "broken entry",
                    {
                        "id": "existing_script",
                        "display_name": "Existing",
                        "description": "Existing",
                        "path": "existing.sh"
                    }
                ]
            }))
            .unwrap(),
        )
        .unwrap();

        register_script_handler(json!({"id":"new_script","path":"new.sh"}), scripts_dir)
            .await
            .unwrap();
        unregister_script_handler(
            json!({"id":"existing_script","delete_file":false}),
            scripts_dir,
        )
        .await
        .unwrap();

        let index = read_script_index_value(&scripts_dir.join("index.json")).unwrap();
        let scripts = index.get("scripts").and_then(Value::as_array).unwrap();
        assert!(scripts.iter().any(|entry| entry == "broken entry"));
        assert!(scripts
            .iter()
            .any(|entry| raw_entry_field(entry, "id") == Some("new_script")));
        assert!(!scripts
            .iter()
            .any(|entry| raw_entry_field(entry, "id") == Some("existing_script")));
        let disabled = index.get("disabled").and_then(Value::as_array).unwrap();
        assert!(disabled
            .iter()
            .any(|entry| raw_entry_field(entry, "id") == Some("existing_script")));
    }

    #[tokio::test]
    async fn lifecycle_mutations_replace_and_remove_all_same_id_entries() {
        let temp = tempfile::tempdir().unwrap();
        let scripts_dir = temp.path();
        std::fs::write(
            scripts_dir.join("old.sh"),
            "#!/bin/bash\n# Description: Old\n\necho old",
        )
        .unwrap();
        std::fs::write(
            scripts_dir.join("new.sh"),
            "#!/bin/bash\n# Description: New\n\necho new",
        )
        .unwrap();
        std::fs::write(
            scripts_dir.join("index.json"),
            serde_json::to_string(&json!({
                "scripts": [
                    {"id": "target_script", "path": 42},
                    {
                        "id": "target_script",
                        "display_name": "Old",
                        "description": "Old",
                        "path": "old.sh"
                    }
                ]
            }))
            .unwrap(),
        )
        .unwrap();

        register_script_handler(json!({"id":"target_script","path":"new.sh"}), scripts_dir)
            .await
            .unwrap();

        let index_path = scripts_dir.join("index.json");
        let mut index = read_script_index_value(&index_path).unwrap();
        let scripts = index_array_mut(&mut index, "scripts").unwrap();
        assert_eq!(
            scripts
                .iter()
                .filter(|entry| raw_entry_field(entry, "id") == Some("target_script"))
                .count(),
            1
        );
        assert_eq!(raw_entry_field(&scripts[0], "path"), Some("new.sh"));

        scripts.insert(0, json!({"id": "target_script", "path": 42}));
        write_script_index_value(&index_path, &index).unwrap();
        unregister_script_handler(
            json!({"id":"target_script","delete_file":false}),
            scripts_dir,
        )
        .await
        .unwrap();

        let index = read_script_index_value(&index_path).unwrap();
        let scripts = index.get("scripts").and_then(Value::as_array).unwrap();
        assert!(!scripts
            .iter()
            .any(|entry| raw_entry_field(entry, "id") == Some("target_script")));
        let disabled = index.get("disabled").and_then(Value::as_array).unwrap();
        assert!(disabled.iter().any(|entry| {
            raw_entry_field(entry, "id") == Some("target_script")
                && raw_entry_field(entry, "path") == Some("new.sh")
        }));
    }

    #[tokio::test]
    async fn unregister_keeps_file_disabled() {
        let temp = tempfile::tempdir().unwrap();
        let scripts_dir = temp.path();
        std::fs::write(
            scripts_dir.join("hello.sh"),
            "#!/bin/bash\n# Description: Say hello\n\necho hello",
        )
        .unwrap();

        unregister_script_handler(json!({"id":"hello","delete_file":false}), scripts_dir)
            .await
            .unwrap();

        assert!(scripts_dir.join("hello.sh").is_file());
        let index = read_script_index_for_scan(&scripts_dir.join("index.json")).unwrap();
        assert_eq!(index.disabled.len(), 1);
        let scan = scan_scripts(&[scripts_dir]).unwrap();
        assert!(scan.entries.is_empty());
        assert!(scan.unregistered.is_empty());
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
