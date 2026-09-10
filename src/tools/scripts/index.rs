//! 脚本索引的扫描与落盘。
//!
//! 真相源顺序(09-05):`index.json` 条目里显式写了的字段 > 脚本头部
//! (header.rs)> 默认值。index 退为覆盖层——描述/参数/超时/分组都能写在脚本
//! 开头的注释里,一个文件就是一个完整的工具;index 只放手写覆盖和 disabled
//! 名单。内置 8 条 index 条目原样保留,它们仍然压在头部之上,行为零变化。
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
    #[serde(default)]
    pub(crate) path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ScriptEntry {
    pub(crate) id: String,
    #[serde(default)]
    pub(crate) display_name: String,
    #[serde(default)]
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
    #[serde(default, skip_serializing_if = "ArgvMode::is_off")]
    pub(crate) argv: ArgvMode,
    /// 场所信任位;缺省 Owner。见 ToolSpec::trust。
    #[serde(default, skip_serializing_if = "is_owner_trust")]
    pub(crate) trust: ToolTrust,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) permission: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) stub_example: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) hints: Vec<(String, String)>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) requires: Vec<String>,
}

fn is_owner_trust(trust: &ToolTrust) -> bool {
    *trust == ToolTrust::Owner
}

impl ScriptEntry {
    /// 只有 id 与路径的空条目:其余字段留空,扫描时由脚本头部补齐。
    pub(crate) fn overlay(id: String, path: String) -> Self {
        Self {
            id,
            display_name: String::new(),
            description: String::new(),
            path,
            parameters: Value::Null,
            timeout_seconds: None,
            always_loaded: None,
            load_policy: LoadPolicy::Summary,
            groups: Vec::new(),
            argv: ArgvMode::Off,
            trust: ToolTrust::Owner,
            permission: None,
            stub_example: None,
            hints: Vec::new(),
            requires: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Default)]
pub(crate) struct ScriptScanResult {
    pub(crate) entries: Vec<ScriptEntry>,
    pub(crate) unregistered: Vec<UnregisteredScript>,
}

/// 扫描根,覆盖链低→高。每个物理层(内置 system、全局 data)都是
/// 「顶层(平台共享) + personas/<当前人格>(人格专属)」两级,四个根按此顺序扫,
/// scan_scripts 后者同名覆盖前者。
///
/// 内置脚本装在 `<system>/personas/default/` 下:自定义人格扫
/// `<system>/personas/<那个人格>`——目录不存在,天然拿不到内置。人格门因此是
/// **隐式**的,不再需要 is_default_persona 特判,四层一套逻辑贯穿,与 data 层的
/// personas/ 约定对齐(09-01)。顶层留给将来的平台级内置脚本(当前为空)。
pub(crate) fn script_scan_roots(
    config: &crate::config::AppConfig,
    paths: &MiyuPaths,
) -> Vec<PathBuf> {
    vec![
        paths.system_scripts_dir.clone(),
        config.active_persona_system_scripts_dir(paths),
        paths.scripts_dir.clone(),
        config.active_persona_scripts_dir(paths),
    ]
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

/// index 条目没写的字段从脚本头部补:显示名、描述、参数 schema、超时、分组、
/// argv 模式。index 写了的一律不动——它是覆盖层。
pub(crate) fn merge_header_defaults(entry: &mut ScriptEntry, metadata: &ScriptMetadata) {
    if entry.display_name.trim().is_empty() {
        if let Some(display_name) = select_script_display_name(&metadata.display_names) {
            entry.display_name = display_name;
        }
    }
    if entry.description.trim().is_empty() {
        if let Some(description) = select_script_description(&metadata.descriptions) {
            entry.description = description;
        }
    }
    if entry.parameters.is_null() {
        if let Some(parameters) = &metadata.parameters {
            entry.parameters = parameters.clone();
        }
    }
    if entry.timeout_seconds.is_none() {
        entry.timeout_seconds = metadata.timeout_seconds;
    }
    // 头部给了分组就顺带走 group 目录;index 里自己写的 groups+load_policy
    // 组合原样保留(用户现有条目有 groups 配 summary 的,不替它改语义)。
    if entry.groups.is_empty() && !metadata.groups.is_empty() {
        entry.groups = metadata.groups.clone();
        if matches!(entry.load_policy, LoadPolicy::Summary) {
            entry.load_policy = LoadPolicy::Group;
        }
    }
    if entry.argv.is_off() {
        if let Some(argv) = metadata.argv {
            entry.argv = argv;
        }
    }
    if entry.trust == ToolTrust::Owner {
        if let Some(trust) = metadata.trust {
            entry.trust = trust;
        }
    }
    if entry.permission.is_none() {
        entry.permission = metadata.permission.map(|permission| {
            match permission {
                ToolPermission::ReadOnly => "read-only",
                ToolPermission::Presentation => "presentation",
                ToolPermission::Writes => "writes",
            }
            .to_string()
        });
    }
    if entry.stub_example.is_none() {
        entry.stub_example = metadata.stub_example.clone();
    }
    if entry.hints.is_empty() {
        entry.hints = metadata.hints.clone();
    }
    if entry.requires.is_empty() {
        entry.requires = metadata.requires.clone();
    }
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
        // 本层 index 已登记的 id:同目录里同名 stem 的其它文件(gpustoggle.bak
        // 之类)不得再以自动检测的身份把它顶掉或拖进未注册清单——用户机器上
        // 一个没有描述头的 .bak 就把正主从工具面上抹掉了(09-05 实查)。
        let mut indexed_ids = BTreeSet::new();
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
            indexed_ids.insert(entry.id.clone());
            entry.path = path.to_string_lossy().to_string();
            merge_header_defaults(&mut entry, &metadata_from_script(&path));
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
            if fname == "index.json" || fname.starts_with('.') || is_backup_file_name(&fname) {
                continue;
            }
            let Some(detected) = inspect_script(&path) else {
                continue;
            };
            if detected
                .id
                .as_deref()
                .is_some_and(|id| indexed_ids.contains(id))
            {
                continue;
            }
            let canon = canonicalize_key(&path);
            let path_string = path.to_string_lossy().to_string();
            // 文件名折不出合法工具名(纯中文文件名):列进未注册清单,模型能
            // 看见它、用 manage_script 给个 id 注册。
            let Some(id) = detected.id.clone() else {
                if disabled_paths.contains(&canon) || !seen_paths.insert(canon) {
                    continue;
                }
                unregistered.insert(
                    detected.stem.clone(),
                    UnregisteredScript {
                        name: detected.stem,
                        path: path_string,
                    },
                );
                continue;
            };
            if is_reserved_script_id(&id) {
                continue;
            }
            if disabled_ids.contains(&id)
                || disabled_paths.contains(&canon)
                || !seen_paths.insert(canon)
            {
                continue;
            }

            let entry = entry_from_detected(&detected, id.clone(), path_string.clone());
            if entry.description.trim().is_empty() {
                entries.remove(&id);
                unregistered.insert(
                    id.clone(),
                    UnregisteredScript {
                        name: id,
                        path: path_string,
                    },
                );
            } else {
                unregistered.remove(&id);
                entries.insert(id, entry);
            }
        }
    }

    Ok(ScriptScanResult {
        entries: entries.into_values().collect(),
        unregistered: unregistered.into_values().collect(),
    })
}

/// 编辑器/手工备份副本不算脚本:`foo.bak` 的 stem 仍是 `foo`,会撞正主的 id。
pub(crate) fn is_backup_file_name(name: &str) -> bool {
    name.ends_with('~')
        || [".bak", ".orig", ".tmp", ".swp", ".old", ".rej"]
            .iter()
            .any(|suffix| name.to_ascii_lowercase().ends_with(suffix))
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
    /// 工具名:头部 `Id:`(合法时)> 文件名 stem 归一化;折不出来为 None。
    pub(crate) id: Option<String>,
    pub(crate) stem: String,
    pub(crate) display_name: String,
    pub(crate) metadata: ScriptMetadata,
}

pub(crate) fn inspect_script(path: &Path) -> Option<DetectedScript> {
    let raw = read_header(path)?;
    if !raw.starts_with("#!") {
        return None;
    }
    let stem = path.file_stem()?.to_str()?.to_string();
    let metadata = extract_metadata(&raw);
    let id = metadata
        .id
        .as_deref()
        .map(str::trim)
        .filter(|id| is_valid_registered_script_id(id))
        .map(str::to_string)
        .or_else(|| normalize_script_id(&stem));
    let display_name = select_script_display_name(&metadata.display_names)
        .unwrap_or_else(|| id.clone().unwrap_or_else(|| stem.clone()));
    Some(DetectedScript {
        id,
        stem,
        display_name,
        metadata,
    })
}

pub(crate) fn entry_from_detected(
    detected: &DetectedScript,
    id: String,
    path: String,
) -> ScriptEntry {
    let mut entry = ScriptEntry::overlay(id, path);
    entry.display_name = detected.display_name.clone();
    merge_header_defaults(&mut entry, &detected.metadata);
    entry
}

#[cfg(test)]
pub(crate) fn auto_detect_script(path: &Path) -> Option<ScriptEntry> {
    let detected = inspect_script(path)?;
    let id = detected.id.clone()?;
    let file_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("")
        .to_string();
    let entry = entry_from_detected(&detected, id, file_name);
    (!entry.description.is_empty()).then_some(entry)
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
    // 默认懒加载(09-05)。此前「没写参数就常驻」:自动检测出来的脚本全都带着
    // 泛化 schema 永久占 tools 数组;stub 模式下常驻工具发的还是完整定义
    // (registry::stub_definitions),白花字节。index 里显式 always_loaded:true
    // 仍然放行。
    let always_loaded = entry.always_loaded.unwrap_or(false);
    let load_policy = entry.load_policy;
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
    let argv = entry.argv;
    let path_str = entry.path.clone();
    let scripts_dir = scripts_dir.to_path_buf();
    let cache_dir = cache_dir.to_path_buf();

    // 缺省 writes:脚本会跑命令。头部/index 明确写了 read-only 的才降。
    let permission = entry
        .permission
        .as_deref()
        .and_then(ToolPermission::parse)
        .unwrap_or(ToolPermission::Writes);
    let mut spec =
        ToolSpec::new_with_progress(id, description, parameters, move |args, progress| {
            let path_str = path_str.clone();
            let scripts_dir = scripts_dir.clone();
            let cache_dir = cache_dir.clone();
            async move {
                run_script(
                    &path_str,
                    &scripts_dir,
                    &cache_dir,
                    &args,
                    timeout,
                    argv,
                    &progress,
                )
                .await
            }
        })
        .with_permission(permission)
        .with_display_name(display_name)
        .with_always_loaded(always_loaded)
        .with_load_policy(load_policy)
        .with_groups(entry.groups.clone())
        .with_trust(entry.trust)
        .with_cross_hints(entry.hints.clone())
        .with_requires_prior(entry.requires.clone())
        .script();
    if let Some(example) = entry
        .stub_example
        .as_deref()
        .map(str::trim)
        .filter(|e| !e.is_empty())
    {
        spec = spec.with_stub_example(example);
    }
    Ok(spec)
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
    if let Some(parent) = index_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
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
        if detected.id.as_deref() == Some(id) {
            return Ok(Some(relative_script_path(&path, scripts_dir)));
        }
    }
    Ok(None)
}
