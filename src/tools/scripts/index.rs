//! 脚本索引的扫描、解析与落盘。
//!
//! 脚本 ID 会变成工具名，所以 `is_valid_registered_script_id` 与
//! `is_reserved_script_id` 挡的是「注册出一个和内建工具重名的工具」。
//!
//! `ensure_path_within_root` 是路径边界：索引里的路径可能被手工编辑过，指到库
//! 外就等于任意文件执行。

use crate::tools::scripts::*;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(crate) struct ScriptIndex {
    #[serde(default)]
    pub(crate) scripts: Vec<ScriptEntry>,
    #[serde(default)]
    pub(crate) disabled: Vec<DisabledScript>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(crate) struct DisabledScript {
    pub(crate) id: String,
    pub(crate) path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ScriptEntry {
    pub(crate) id: String,
    #[serde(default)]
    pub(crate) display_name: String,
    pub(crate) description: String,
    #[serde(default)]
    pub(crate) path: String,
    #[serde(default)]
    pub(crate) parameters: Value,
    #[serde(default)]
    pub(crate) timeout_seconds: Option<u64>,
    #[serde(default)]
    pub(crate) always_loaded: Option<bool>,
    #[serde(default)]
    pub(crate) load_policy: LoadPolicy,
    #[serde(default)]
    pub(crate) groups: Vec<String>,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct ScriptScanResult {
    pub(crate) entries: Vec<ScriptEntry>,
    pub(crate) unregistered: Vec<UnregisteredScript>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ScriptDescriptions {
    pub(crate) zh: Option<String>,
    pub(crate) en: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ScriptMetadata {
    pub(crate) descriptions: ScriptDescriptions,
    pub(crate) display_names: ScriptDisplayNames,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ScriptDisplayNames {
    pub(crate) zh: Option<String>,
    pub(crate) en: Option<String>,
}

pub fn rescan_scripts(registry: &mut ToolRegistry, paths: &MiyuPaths) {
    let dirs = [
        paths.system_scripts_dir.as_path(),
        paths.scripts_dir.as_path(),
    ];
    let scan = match scan_scripts(&dirs) {
        Ok(scan) => scan,
        Err(error) => {
            tracing::warn!(error = %error, "failed to rescan Miyu script directories");
            return;
        }
    };
    let specs = script_specs(&scan.entries, &paths.scripts_dir, &paths.cache_dir);
    if let Err(error) = registry.replace_script_tools(specs, scan.unregistered) {
        tracing::warn!(error = %error, "failed to replace Miyu script tools");
    }
}

pub(crate) fn script_specs(
    entries: &[ScriptEntry],
    scripts_dir: &Path,
    cache_dir: &Path,
) -> Vec<ToolSpec> {
    entries
        .iter()
        .filter_map(|entry| entry_to_spec(entry, scripts_dir, cache_dir).ok())
        .collect()
}

pub(crate) fn scan_scripts(dirs: &[&Path]) -> Result<ScriptScanResult> {
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

pub(crate) fn canonicalize_key(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

pub(crate) fn resolve_script_path(path_str: &str, scripts_dir: &Path) -> PathBuf {
    let p = Path::new(path_str);
    if p.is_absolute() {
        if p.starts_with(scripts_dir) {
            return p.to_path_buf();
        }
        if let Some(root) = scripts_dir
            .parent()
            .filter(|parent| parent.file_name().and_then(|name| name.to_str()) == Some("data"))
            .and_then(Path::parent)
        {
            let legacy = root.join("config/scripts");
            if let Ok(relative) = p.strip_prefix(&legacy) {
                return scripts_dir.join(relative);
            }
        }
        if let Some(base) = directories::BaseDirs::new() {
            let legacy = base.config_dir().join("miyu/scripts");
            if let Ok(relative) = p.strip_prefix(&legacy) {
                return scripts_dir.join(relative);
            }
        }
        p.to_path_buf()
    } else {
        scripts_dir.join(p)
    }
}

pub(crate) fn ensure_path_within_root(path: &Path, scripts_dir: &Path) -> Result<PathBuf> {
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

pub(crate) fn relative_script_path(path: &Path, scripts_dir: &Path) -> String {
    let root = scripts_dir
        .canonicalize()
        .unwrap_or_else(|_| scripts_dir.to_path_buf());
    let path = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    path.strip_prefix(&root)
        .unwrap_or(&path)
        .to_string_lossy()
        .to_string()
}

pub(crate) fn is_reserved_script_id(id: &str) -> bool {
    id == "load_tools" || crate::tools::tool_descriptions::get(id).is_some()
}

pub(crate) fn is_valid_registered_script_id(id: &str) -> bool {
    id.chars()
        .next()
        .map(|character| character.is_ascii_alphabetic())
        .unwrap_or(false)
        && id
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || character == '_')
}

#[derive(Debug, Clone)]
pub(crate) struct DetectedScript {
    pub(crate) id: String,
    pub(crate) display_name: String,
    pub(crate) description: Option<String>,
}

pub(crate) fn inspect_script(path: &Path) -> Option<DetectedScript> {
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
pub(crate) fn auto_detect_script(path: &Path) -> Option<ScriptEntry> {
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

pub(crate) fn extract_description(raw: &str) -> Option<String> {
    select_script_description(&extract_metadata(raw).descriptions)
}

pub(crate) fn description_from_script(path: &Path) -> Option<String> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|raw| extract_description(&raw))
}

pub(crate) fn select_script_description(descriptions: &ScriptDescriptions) -> Option<String> {
    // 模型面恒英文:英文描述优先;用户脚本只写了中文时原样保留(用户内容)。
    let preferred = descriptions.en.as_ref().or(descriptions.zh.as_ref())?;
    Some(preferred.clone())
}

pub(crate) fn select_script_display_name(display_names: &ScriptDisplayNames) -> Option<String> {
    let preferred = if is_zh() {
        display_names.zh.as_ref().or(display_names.en.as_ref())
    } else {
        display_names.en.as_ref().or(display_names.zh.as_ref())
    }?;
    Some(preferred.clone())
}

pub(crate) fn extract_metadata(raw: &str) -> ScriptMetadata {
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
pub(crate) enum DescriptionKey {
    Chinese,
    English,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub(crate) enum DisplayNameKey {
    Chinese,
    English,
}

pub(crate) fn split_description_line(line: &str) -> Option<(DescriptionKey, &str)> {
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

pub(crate) fn split_display_name_line(line: &str) -> Option<(DisplayNameKey, &str)> {
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

pub(crate) fn entry_to_spec(
    entry: &ScriptEntry,
    scripts_dir: &Path,
    cache_dir: &Path,
) -> Result<ToolSpec> {
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
                    "description": "Optional raw stdin input. If omitted, all arguments are sent as JSON via stdin."
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
    let cache_dir = cache_dir.to_path_buf();

    let spec = ToolSpec::new(id, description, parameters, move |args| {
        let path_str = path_str.clone();
        let scripts_dir = scripts_dir.clone();
        let cache_dir = cache_dir.clone();
        async move { run_script(&path_str, &scripts_dir, &cache_dir, &args, timeout).await }
    })
    .writes()
    .with_display_name(display_name)
    .with_always_loaded(always_loaded)
    .with_load_policy(entry.load_policy)
    .with_groups(entry.groups.clone())
    .script();
    Ok(spec)
}

pub(crate) fn parse_load_policy(value: &str) -> Result<LoadPolicy> {
    match value.trim() {
        "" | "summary" | "lazy" => Ok(LoadPolicy::Summary),
        "group" => Ok(LoadPolicy::Group),
        "hidden" => Ok(LoadPolicy::Hidden),
        other => bail!("invalid load_policy: {other}"),
    }
}

pub(crate) fn read_script_index_value(index_path: &Path) -> Result<Value> {
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

pub(crate) fn read_script_index_for_scan(index_path: &Path) -> Result<ScriptIndex> {
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

pub(crate) fn index_array_mut<'a>(index: &'a mut Value, key: &str) -> Result<&'a mut Vec<Value>> {
    let object = index
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("script index root must be an object"))?;
    let value = object.entry(key.to_string()).or_insert_with(|| json!([]));
    if !value.is_array() {
        *value = json!([]);
    }
    Ok(value.as_array_mut().expect("array was just initialized"))
}

pub(crate) fn raw_entry_field<'a>(entry: &'a Value, field: &str) -> Option<&'a str> {
    entry.get(field).and_then(Value::as_str)
}

pub(crate) fn write_script_index_value(index_path: &Path, index: &Value) -> Result<()> {
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

pub(crate) fn find_auto_detected_path(scripts_dir: &Path, id: &str) -> Result<Option<String>> {
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
