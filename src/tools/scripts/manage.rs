//! `manage_script`:脚本工具的注册/注销/清单,同一份索引的三种操作。
//!
//! 09-05 重做,针对的是「AI 写完脚本还得用户手工 cp 进目录」这条链:
//! - `path` 给库外绝对路径时**复制**进目标层(不搬:跨文件系统 rename 会失败,
//!   把用户工作区里的文件搬走也很突兀)。
//! - 加 scope,默认 persona——与 manage_skill 一致(09-01),在某人格对话里
//!   写的脚本归那个人格;文件已经躺在某个用户层里时就地注册,不复制。
//! - index 只存显式给的覆盖字段,描述/参数/超时/分组优先从脚本头部读
//!   (header.rs);register 是合并更新,改一个超时不必重传整条。
//! - 注册前校验 shebang 与 schema 形状,报错指向 script-creator 技能。
//! - 新增 list:已注册/未注册/disabled 连同路径全列出来,模型不再盲写。

use crate::config::AppConfig;
use crate::tools::scripts::*;

const MAX_COPY_BYTES: u64 = 8 * 1024 * 1024;
const CONTRACT_HINT: &str = "Load the script-creator skill for the script contract (shebang line, `# Description:` header, JSON arguments on stdin).";

#[derive(Debug, Clone)]
pub(crate) struct ScriptLayer {
    pub(crate) scope: &'static str,
    pub(crate) dir: PathBuf,
}

/// 用户可写的两层,persona 在前(扫描时它压在 global 之上)。
pub(crate) fn user_layers(config: &AppConfig, paths: &MiyuPaths) -> [ScriptLayer; 2] {
    [
        ScriptLayer {
            scope: "persona",
            dir: config.active_persona_scripts_dir(paths),
        },
        ScriptLayer {
            scope: "global",
            dir: paths.scripts_dir.clone(),
        },
    ]
}

fn layer_for_scope(scope: Option<&str>, layers: &[ScriptLayer; 2]) -> Result<ScriptLayer> {
    match scope.map(str::trim).unwrap_or("") {
        "" | "persona" => Ok(layers[0].clone()),
        "global" => Ok(layers[1].clone()),
        other => bail!("invalid scope: {other}; expected persona or global"),
    }
}

/// 文件直接躺在哪个用户层的顶层目录里。扫描只看顶层,子目录不算。
pub(crate) fn layer_containing(path: &Path, layers: &[ScriptLayer; 2]) -> Option<ScriptLayer> {
    let parent = path.parent()?.canonicalize().ok()?;
    layers
        .iter()
        .find(|layer| layer.dir.canonicalize().ok().as_deref() == Some(parent.as_path()))
        .cloned()
}

fn string_arg(args: &Value, key: &str) -> String {
    args.get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim()
        .to_string()
}

pub(crate) fn register_script_tools(
    registry: &mut ToolRegistry,
    config: AppConfig,
    paths: MiyuPaths,
) {
    // 描述与 schema 的真相源是 descriptions/manage_script.json,这里只是占位。
    registry.register(
        ToolSpec::new(
            "manage_script",
            "Register, unregister or list user scripts as tools.",
            json!({
                "type": "object",
                "properties": { "action": { "type": "string" } },
                "required": ["action"]
            }),
            move |args| {
                let config = config.clone();
                let paths = paths.clone();
                async move {
                    match args
                        .get("action")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                    {
                        "register" => register_script_handler(args, &config, &paths).await,
                        "unregister" => unregister_script_handler(args, &config, &paths).await,
                        "list" => list_scripts_handler(&config, &paths),
                        other => {
                            bail!("unknown action: {other}; expected register, unregister or list")
                        }
                    }
                }
            },
        )
        .writes(),
    );
}

fn copy_into_layer(source: &Path, layer: &ScriptLayer, overwrite: bool) -> Result<PathBuf> {
    let metadata = std::fs::metadata(source)
        .with_context(|| format!("failed to read {}", source.display()))?;
    if !metadata.is_file() {
        bail!("{} is not a regular file", source.display());
    }
    if metadata.len() > MAX_COPY_BYTES {
        bail!(
            "{} is larger than {} bytes; scripts that big do not belong in the tool directory",
            source.display(),
            MAX_COPY_BYTES
        );
    }
    let file_name = source
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| anyhow::anyhow!("script file name is not valid UTF-8"))?;
    if file_name == "index.json" || file_name.starts_with('.') {
        bail!("'{file_name}' is not a usable script file name in the scripts directory");
    }
    std::fs::create_dir_all(&layer.dir)
        .with_context(|| format!("failed to create {}", layer.dir.display()))?;
    let destination = layer.dir.join(file_name);
    if destination.exists() && !overwrite {
        bail!(
            "{} already exists in the {} scripts directory; pass overwrite=true to replace it, or register that file by name",
            destination.display(),
            layer.scope
        );
    }
    std::fs::copy(source, &destination).with_context(|| {
        format!(
            "failed to copy {} to {}",
            source.display(),
            destination.display()
        )
    })?;
    Ok(destination)
}

/// 按 id 在用户层里找已有脚本:先 index 条目,再自动检测的文件。
fn locate_by_id(id: &str, layers: &[ScriptLayer; 2]) -> Result<Option<(ScriptLayer, PathBuf)>> {
    for layer in layers {
        let index = read_script_index_for_scan(&layer.dir.join("index.json"))?;
        if let Some(entry) = index.scripts.iter().find(|entry| entry.id == id) {
            let path = resolve_script_path(&entry.path, &layer.dir);
            if path.is_file() {
                return Ok(Some((layer.clone(), path)));
            }
        }
    }
    for layer in layers {
        if let Some(relative) = find_auto_detected_path(&layer.dir, id)? {
            return Ok(Some((layer.clone(), layer.dir.join(relative))));
        }
    }
    Ok(None)
}

fn validate_parameters(parameters: &Value) -> Result<()> {
    let Some(object) = parameters.as_object() else {
        bail!("parameters must be a JSON Schema object such as {{\"type\":\"object\",\"properties\":{{...}}}}");
    };
    if let Some(kind) = object.get("type") {
        if kind.as_str() != Some("object") {
            bail!("parameters.type must be \"object\": tool arguments are always a JSON object");
        }
    }
    if let Some(properties) = object.get("properties") {
        if !properties.is_object() {
            bail!("parameters.properties must be an object mapping argument names to schemas");
        }
    }
    Ok(())
}

/// 清掉某层 disabled 名单里的一个 id;返回是否真的删了记录。
pub(crate) fn clear_disabled_id(index_path: &Path, id: &str) -> Result<bool> {
    if !index_path.is_file() {
        return Ok(false);
    }
    let mut index = read_script_index_value(index_path)?;
    let disabled = index_array_mut(&mut index, "disabled")?;
    let before = disabled.len();
    disabled.retain(|entry| raw_entry_field(entry, "id") != Some(id));
    let changed = disabled.len() != before;
    if changed {
        write_script_index_value(index_path, &index)?;
    }
    Ok(changed)
}

pub(crate) async fn register_script_handler(
    args: Value,
    config: &AppConfig,
    paths: &MiyuPaths,
) -> Result<String> {
    register_script(args, config, paths)
}

/// 同步核心:工具闭包与 dashboard(spawn_blocking)共用。
pub(crate) fn register_script(
    args: Value,
    config: &AppConfig,
    paths: &MiyuPaths,
) -> Result<String> {
    let layers = user_layers(config, paths);
    let requested_id = string_arg(&args, "id");
    if !requested_id.is_empty() && !is_valid_registered_script_id(&requested_id) {
        bail!(
            "id must start with an ASCII letter and contain only ASCII alphanumeric and underscore"
        );
    }
    let path_arg = string_arg(&args, "path");
    let scope_arg = args.get("scope").and_then(Value::as_str);
    let overwrite = args
        .get("overwrite")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    let mut copied = false;
    let (layer, script_path) = if path_arg.is_empty() {
        if requested_id.is_empty() {
            bail!("path is required: pass the script's absolute path (it is copied into the scripts directory) or its file name inside one");
        }
        match locate_by_id(&requested_id, &layers)? {
            Some(located) => located,
            // 被 unregister 屏蔽过的内置脚本:用户层没有文件,只需把 disabled
            // 记录清掉就恢复了,不然屏蔽一次就再也回不来。
            None if builtin_has_script(config, paths, &requested_id)? => {
                for layer in &layers {
                    clear_disabled_id(&layer.dir.join("index.json"), &requested_id)?;
                }
                return Ok(serde_json::to_string_pretty(&json!({
                    "ok": true,
                    "id": requested_id,
                    "scope": "builtin",
                    "state": "enabled",
                    "next": "The built-in script is visible again from the next tool round."
                }))?);
            }
            None => bail!("script '{requested_id}' not found in any scripts directory; pass path"),
        }
    } else {
        let candidate = Path::new(&path_arg);
        if candidate.is_absolute() {
            if !candidate.is_file() {
                bail!("script file not found: {}", candidate.display());
            }
            match layer_containing(candidate, &layers) {
                Some(layer) => (layer, candidate.to_path_buf()),
                None => {
                    let layer = layer_for_scope(scope_arg, &layers)?;
                    let destination = copy_into_layer(candidate, &layer, overwrite)?;
                    copied = true;
                    (layer, destination)
                }
            }
        } else {
            let preferred = layer_for_scope(scope_arg, &layers)?;
            let local = preferred.dir.join(candidate);
            if local.is_file() {
                (preferred, local)
            } else if let Some(other) = layers
                .iter()
                .find(|layer| scope_arg.is_none() && layer.dir.join(candidate).is_file())
            {
                (other.clone(), other.dir.join(candidate))
            } else {
                bail!(
                    "script file not found: {path_arg} (looked in {}). Pass an absolute path to copy the file into the scripts directory.",
                    preferred.dir.display()
                );
            }
        }
    };
    let script_path = ensure_path_within_root(&script_path, &layer.dir)?;

    let header = read_header(&script_path).unwrap_or_default();
    if !header.starts_with("#!") {
        bail!("script has no shebang: the first line must be an interpreter line such as #!/usr/bin/env python3. {CONTRACT_HINT}");
    }
    make_executable(&script_path)?;
    let metadata = extract_metadata(&header);

    let id = if !requested_id.is_empty() {
        requested_id
    } else {
        let stem = script_path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .unwrap_or_default();
        metadata
            .id
            .as_deref()
            .map(str::trim)
            .filter(|id| is_valid_registered_script_id(id))
            .map(str::to_string)
            .or_else(|| normalize_script_id(stem))
            .ok_or_else(|| {
                anyhow::anyhow!("cannot derive a tool name from the file name '{stem}'; pass id")
            })?
    };
    if is_reserved_script_id(&id) {
        bail!("script id conflicts with a reserved tool name: {id}");
    }

    let index_path = layer.dir.join("index.json");
    let mut index = read_script_index_value(&index_path)?;
    let existing = index
        .get("scripts")
        .and_then(Value::as_array)
        .and_then(|scripts| {
            scripts
                .iter()
                .find(|entry| raw_entry_field(entry, "id") == Some(id.as_str()))
        })
        .and_then(|entry| serde_json::from_value::<ScriptEntry>(entry.clone()).ok());
    let mut entry = existing.unwrap_or_else(|| ScriptEntry::overlay(id.clone(), String::new()));
    entry.id = id.clone();
    entry.path = relative_script_path(&script_path, &layer.dir);
    let display_name = string_arg(&args, "display_name");
    if !display_name.is_empty() {
        entry.display_name = display_name;
    }
    let description = string_arg(&args, "description");
    if !description.is_empty() {
        entry.description = description;
    }
    if let Some(parameters) = args.get("parameters").filter(|value| !value.is_null()) {
        validate_parameters(parameters)?;
        entry.parameters = parameters.clone();
    }
    if let Some(timeout) = args.get("timeout_seconds").and_then(Value::as_u64) {
        entry.timeout_seconds = Some(timeout.clamp(1, 300));
    }

    // 有效值 = index 覆盖 + 头部:描述必须凑得出来,头部的 schema 也得是能用的形状。
    let mut effective = entry.clone();
    merge_header_defaults(&mut effective, &metadata);
    if effective.description.trim().is_empty() {
        bail!("description is required: pass description, or add a `# Description:` line to the script header. {CONTRACT_HINT}");
    }
    if !effective.parameters.is_null() {
        validate_parameters(&effective.parameters)
            .context("the script header Parameters block is not a usable JSON Schema")?;
    }

    {
        let scripts = index_array_mut(&mut index, "scripts")?;
        scripts.retain(|script| raw_entry_field(script, "id") != Some(id.as_str()));
        scripts.push(serde_json::to_value(&entry)?);
    }
    let script_key = canonicalize_key(&script_path);
    index_array_mut(&mut index, "disabled")?.retain(|disabled| {
        raw_entry_field(disabled, "id") != Some(id.as_str())
            && raw_entry_field(disabled, "path")
                .map(|path| canonicalize_key(&resolve_script_path(path, &layer.dir)) != script_key)
                .unwrap_or(true)
    });
    write_script_index_value(&index_path, &index)?;
    // 另一层的 disabled 名单里同 id 也清掉:persona 层的 disable 会压掉 global
    // 层同名条目,留着等于注册了也看不见。
    for other in layers.iter().filter(|other| other.scope != layer.scope) {
        clear_disabled_id(&other.dir.join("index.json"), &id)?;
    }

    Ok(serde_json::to_string_pretty(&json!({
        "ok": true,
        "id": id,
        "scope": layer.scope,
        "path": script_path.display().to_string(),
        "copied": copied,
        "description_source": if entry.description.is_empty() { "header" } else { "index" },
        "parameters_source": if !entry.parameters.is_null() {
            "index"
        } else if metadata.parameters.is_some() {
            "header"
        } else {
            "generic stdin"
        },
        "argv": effective.argv.as_str(),
        "next": "Callable from the next tool round. Call it once with real arguments to verify the output."
    }))?)
}

fn builtin_has_script(config: &AppConfig, paths: &MiyuPaths, id: &str) -> Result<bool> {
    let roots = script_scan_roots(config, paths);
    let dirs: Vec<&Path> = roots.iter().take(2).map(PathBuf::as_path).collect();
    let scan = scan_scripts(&dirs)?;
    Ok(scan.entries.iter().any(|entry| entry.id == id))
}

pub(crate) async fn unregister_script_handler(
    args: Value,
    config: &AppConfig,
    paths: &MiyuPaths,
) -> Result<String> {
    unregister_script(args, config, paths)
}

pub(crate) fn unregister_script(
    args: Value,
    config: &AppConfig,
    paths: &MiyuPaths,
) -> Result<String> {
    let id = string_arg(&args, "id");
    if id.is_empty() {
        bail!("id is required");
    }
    let delete_file = args
        .get("delete_file")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let layers = user_layers(config, paths);

    // 先找用户层的 index 条目,再找用户层自动检测的文件;都没有再看是不是
    // 内置脚本——内置的只能按 id 屏蔽(记在 persona 层),文件不动。
    let mut located: Option<(ScriptLayer, String)> = None;
    for layer in &layers {
        let index = read_script_index_for_scan(&layer.dir.join("index.json"))?;
        if let Some(entry) = index.scripts.iter().find(|entry| entry.id == id) {
            located = Some((layer.clone(), entry.path.clone()));
            break;
        }
    }
    if located.is_none() {
        for layer in &layers {
            if let Some(relative) = find_auto_detected_path(&layer.dir, &id)? {
                located = Some((layer.clone(), relative));
                break;
            }
        }
    }
    let builtin = located.is_none() && builtin_has_script(config, paths, &id)?;
    if located.is_none() && !builtin {
        bail!("script id '{id}' not found. Use action=list to see registered, unregistered and disabled scripts.");
    }
    if builtin && delete_file {
        bail!("built-in scripts cannot be deleted; unregister without delete_file hides '{id}' for this persona");
    }
    let (layer, path) = located.unwrap_or_else(|| (layers[0].clone(), String::new()));

    let index_path = layer.dir.join("index.json");
    let mut index = read_script_index_value(&index_path)?;
    index_array_mut(&mut index, "scripts")?
        .retain(|script| raw_entry_field(script, "id") != Some(id.as_str()));

    let mut deleted_file = false;
    if delete_file && !path.is_empty() {
        let unresolved_path = resolve_script_path(&path, &layer.dir);
        if unresolved_path.is_file() {
            let script_path = ensure_path_within_root(&unresolved_path, &layer.dir)?;
            std::fs::remove_file(&script_path)?;
            deleted_file = true;
        }
    }
    let disabled = index_array_mut(&mut index, "disabled")?;
    disabled.retain(|entry| {
        raw_entry_field(entry, "id") != Some(id.as_str())
            && (path.is_empty() || raw_entry_field(entry, "path") != Some(path.as_str()))
    });
    if !deleted_file {
        disabled.push(json!({"id": id, "path": path}));
    }
    write_script_index_value(&index_path, &index)?;

    Ok(serde_json::to_string_pretty(&json!({
        "ok": true,
        "id": id,
        "scope": layer.scope,
        "state": if deleted_file { "deleted" } else { "disabled" },
        "path": path,
        "next": "Gone from the next tool round. Register it again with manage_script to re-enable."
    }))?)
}

pub(crate) fn list_scripts_handler(config: &AppConfig, paths: &MiyuPaths) -> Result<String> {
    let roots = script_scan_roots(config, paths);
    let dirs: Vec<&Path> = roots.iter().map(PathBuf::as_path).collect();
    let scan = scan_scripts(&dirs)?;
    let layers = user_layers(config, paths);

    let canonical_roots: Vec<Option<PathBuf>> =
        roots.iter().map(|root| root.canonicalize().ok()).collect();
    let labels = ["builtin", "builtin-persona", "global", "persona"];
    let layer_of = |path: &str| -> &'static str {
        let parent = Path::new(path)
            .parent()
            .and_then(|parent| parent.canonicalize().ok());
        canonical_roots
            .iter()
            .enumerate()
            .rev()
            .find(|(_, root)| root.is_some() && **root == parent)
            .map(|(index, _)| labels[index])
            .unwrap_or("unknown")
    };

    let registered: Vec<Value> = scan
        .entries
        .iter()
        .map(|entry| {
            json!({
                "id": entry.id,
                "display_name": entry.display_name,
                "layer": layer_of(&entry.path),
                "path": entry.path,
                "parameters": if entry.parameters.is_null() { "generic stdin" } else { "schema" },
                "argv": entry.argv.as_str(),
                "always_loaded": entry.always_loaded.unwrap_or(false),
            })
        })
        .collect();
    let unregistered: Vec<Value> = scan
        .unregistered
        .iter()
        .map(|script| {
            json!({
                "name": script.name,
                "path": script.path,
                "fix": "add a `# Description:` line to the script header, or register it with manage_script passing description",
            })
        })
        .collect();
    let mut disabled = Vec::new();
    for layer in &layers {
        let index = read_script_index_for_scan(&layer.dir.join("index.json"))?;
        for entry in index.disabled {
            disabled.push(json!({
                "id": entry.id,
                "path": entry.path,
                "scope": layer.scope,
            }));
        }
    }

    Ok(serde_json::to_string_pretty(&json!({
        "ok": true,
        "directories": {
            "persona": layers[0].dir.display().to_string(),
            "global": layers[1].dir.display().to_string(),
        },
        "registered": registered,
        "unregistered": unregistered,
        "disabled": disabled,
    }))?)
}
