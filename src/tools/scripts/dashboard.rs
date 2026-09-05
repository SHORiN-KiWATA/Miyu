//! 控制台「脚本」面板的数据面(09-05)。
//!
//! 把四层目录扫出来的脚本工具连同头部元数据、index 覆盖字段、文件信息一起交给
//! WebUI;禁用 / 启用 / 删除 / 补描述注册四个写操作全部复用 manage.rs 的同一套
//! 逻辑,面板与 `manage_script` 看到的永远是同一份状态。读接口零副作用:不建目录、
//! 不写 index。

use crate::config::AppConfig;
use crate::tools::scripts::*;

const SOURCE_LINE_CAP: usize = 400;
const SOURCE_BYTE_CAP: usize = 64 * 1024;
const LAYER_LABELS: [&str; 4] = ["builtin", "builtin-persona", "global", "persona"];

fn canonical_roots(roots: &[PathBuf]) -> Vec<Option<PathBuf>> {
    roots.iter().map(|root| root.canonicalize().ok()).collect()
}

/// 文件直接躺在哪个扫描根里。后扫的层优先:persona 目录在 global 目录之下。
fn layer_of(canonical: &[Option<PathBuf>], path: &Path) -> &'static str {
    let parent = path.parent().and_then(|parent| parent.canonicalize().ok());
    canonical
        .iter()
        .enumerate()
        .rev()
        .find(|(_, root)| root.is_some() && **root == parent)
        .map(|(index, _)| LAYER_LABELS[index])
        .unwrap_or("unknown")
}

fn load_policy_label(policy: LoadPolicy) -> &'static str {
    match policy {
        LoadPolicy::Summary => "summary",
        LoadPolicy::Group => "group",
        LoadPolicy::Hidden => "hidden",
    }
}

fn modified_rfc3339(metadata: &std::fs::Metadata) -> Option<String> {
    metadata
        .modified()
        .ok()
        .map(|time| chrono::DateTime::<chrono::Utc>::from(time).to_rfc3339())
}

fn is_executable(metadata: &std::fs::Metadata) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        let _ = metadata;
        true
    }
}

/// 用户层 index 里对每个 id 显式写了哪些字段——面板据此标出「这一项来自 index
/// 而不是头部」。
fn index_overrides(
    layers: &[ScriptLayer; 2],
) -> Result<BTreeMap<String, (String, Vec<&'static str>)>> {
    let mut overrides = BTreeMap::new();
    for layer in layers {
        let index = read_script_index_for_scan(&layer.dir.join("index.json"))?;
        for entry in index.scripts {
            let mut fields = Vec::new();
            if !entry.display_name.trim().is_empty() {
                fields.push("display_name");
            }
            if !entry.description.trim().is_empty() {
                fields.push("description");
            }
            if !entry.parameters.is_null() {
                fields.push("parameters");
            }
            if entry.timeout_seconds.is_some() {
                fields.push("timeout_seconds");
            }
            if entry.always_loaded.is_some() {
                fields.push("always_loaded");
            }
            if !entry.groups.is_empty() {
                fields.push("groups");
            }
            if !entry.argv.is_off() {
                fields.push("argv");
            }
            overrides.insert(entry.id, (layer.scope.to_string(), fields));
        }
    }
    Ok(overrides)
}

pub(crate) fn scripts_dashboard_overview(config: &AppConfig, paths: &MiyuPaths) -> Result<Value> {
    let roots = script_scan_roots(config, paths);
    let dirs: Vec<&Path> = roots.iter().map(PathBuf::as_path).collect();
    let scan = scan_scripts(&dirs)?;
    let layers = user_layers(config, paths);
    let canonical = canonical_roots(&roots);
    let overrides = index_overrides(&layers)?;

    let mut scripts = Vec::new();
    for entry in &scan.entries {
        let path = Path::new(&entry.path);
        let metadata = std::fs::metadata(path).ok();
        let header = metadata_from_script(path);
        let layer = layer_of(&canonical, path);
        let parameter_names: Vec<String> = entry
            .parameters
            .get("properties")
            .and_then(Value::as_object)
            .map(|properties| properties.keys().cloned().collect())
            .unwrap_or_default();
        let (override_scope, override_fields) = overrides
            .get(&entry.id)
            .map(|(scope, fields)| (Some(scope.clone()), fields.clone()))
            .unwrap_or((None, Vec::new()));
        scripts.push(json!({
            "id": entry.id,
            "display_name": entry.display_name,
            "description": entry.description,
            "layer": layer,
            "builtin": layer.starts_with("builtin"),
            "path": entry.path,
            "file_name": path.file_name().and_then(|name| name.to_str()).unwrap_or(""),
            "size_bytes": metadata.as_ref().map(|meta| meta.len()),
            "modified": metadata.as_ref().and_then(modified_rfc3339),
            "executable": metadata.as_ref().map(is_executable).unwrap_or(false),
            "parameters": entry.parameters,
            "parameter_names": parameter_names,
            "timeout_seconds": entry.timeout_seconds.unwrap_or(SCRIPT_TIMEOUT_SECS),
            "timeout_default": entry.timeout_seconds.is_none(),
            "groups": entry.groups,
            "load_policy": load_policy_label(entry.load_policy),
            "always_loaded": entry.always_loaded.unwrap_or(false),
            "argv": entry.argv.as_str(),
            "header": {
                "id": header.id,
                "has_description": header.descriptions.en.is_some() || header.descriptions.zh.is_some(),
                "has_parameters": header.parameters.is_some(),
                "timeout_seconds": header.timeout_seconds,
                "groups": header.groups,
                "argv": header.argv.map(|argv| argv.as_str()),
            },
            "override_scope": override_scope,
            "overrides": override_fields,
        }));
    }

    let unregistered: Vec<Value> = scan
        .unregistered
        .iter()
        .map(|script| {
            let path = Path::new(&script.path);
            json!({
                "name": script.name,
                "path": script.path,
                "file_name": path.file_name().and_then(|name| name.to_str()).unwrap_or(""),
                "layer": layer_of(&canonical, path),
                "reason": "no description",
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
                "builtin": entry.path.trim().is_empty(),
            }));
        }
    }

    let builtin_count = scripts
        .iter()
        .filter(|script| script["builtin"].as_bool() == Some(true))
        .count();
    let always_loaded = scripts
        .iter()
        .filter(|script| script["always_loaded"].as_bool() == Some(true))
        .count();
    Ok(json!({
        "ok": true,
        "persona": config.active_persona_scope(),
        "directories": {
            "builtin": roots[0].display().to_string(),
            "builtin_persona": roots[1].display().to_string(),
            "global": roots[2].display().to_string(),
            "persona": roots[3].display().to_string(),
        },
        "counts": {
            "registered": scripts.len(),
            "builtin": builtin_count,
            "user": scripts.len() - builtin_count,
            "unregistered": unregistered.len(),
            "disabled": disabled.len(),
            "always_loaded": always_loaded,
        },
        "scripts": scripts,
        "unregistered": unregistered,
        "disabled": disabled,
    }))
}

/// 源码预览只放行扫描根顶层里的文件:面板传回来的路径来自 overview,但仍按
/// 「(目录, 文件名)」重新解析,不接受任意路径。
fn resolve_previewable(config: &AppConfig, paths: &MiyuPaths, requested: &str) -> Result<PathBuf> {
    let roots = script_scan_roots(config, paths);
    let canonical = canonical_roots(&roots);
    let path = Path::new(requested);
    if !path.is_file() {
        bail!("script file not found: {requested}");
    }
    if layer_of(&canonical, path) == "unknown" {
        bail!("path is outside the script directories: {requested}");
    }
    Ok(path.to_path_buf())
}

pub(crate) fn scripts_dashboard_source(
    config: &AppConfig,
    paths: &MiyuPaths,
    id: &str,
    requested_path: &str,
    max_lines: usize,
) -> Result<Value> {
    let path = if !id.trim().is_empty() {
        let roots = script_scan_roots(config, paths);
        let dirs: Vec<&Path> = roots.iter().map(PathBuf::as_path).collect();
        let scan = scan_scripts(&dirs)?;
        let entry = scan
            .entries
            .iter()
            .find(|entry| entry.id == id.trim())
            .ok_or_else(|| anyhow::anyhow!("script id '{id}' is not registered"))?;
        PathBuf::from(&entry.path)
    } else {
        resolve_previewable(config, paths, requested_path.trim())?
    };
    use std::io::Read;
    let mut buffer = Vec::new();
    std::fs::File::open(&path)?
        .take(SOURCE_BYTE_CAP as u64)
        .read_to_end(&mut buffer)?;
    let text = String::from_utf8_lossy(&buffer);
    let max_lines = max_lines.clamp(1, SOURCE_LINE_CAP);
    let all: Vec<&str> = text.lines().collect();
    let lines: Vec<&str> = all.iter().take(max_lines).copied().collect();
    let size = std::fs::metadata(&path).map(|meta| meta.len()).unwrap_or(0);
    Ok(json!({
        "ok": true,
        "path": path.display().to_string(),
        "lines": lines,
        "shown": lines.len(),
        "truncated": all.len() > max_lines || (size as usize) > SOURCE_BYTE_CAP,
        "size_bytes": size,
    }))
}

fn parse_result(output: String) -> Result<Value> {
    Ok(serde_json::from_str(&output)?)
}

pub(crate) fn scripts_dashboard_disable(
    config: &AppConfig,
    paths: &MiyuPaths,
    id: &str,
) -> Result<Value> {
    parse_result(unregister_script(
        json!({ "action": "unregister", "id": id }),
        config,
        paths,
    )?)
}

pub(crate) fn scripts_dashboard_delete(
    config: &AppConfig,
    paths: &MiyuPaths,
    id: &str,
) -> Result<Value> {
    parse_result(unregister_script(
        json!({ "action": "unregister", "id": id, "delete_file": true }),
        config,
        paths,
    )?)
}

/// 启用 = 把用户两层 disabled 名单里的这个 id 清掉。用户脚本与内置脚本同一条
/// 路径:文件本来就在,消失只因为 disabled 记录。
pub(crate) fn scripts_dashboard_enable(
    config: &AppConfig,
    paths: &MiyuPaths,
    id: &str,
) -> Result<Value> {
    let id = id.trim();
    if id.is_empty() {
        bail!("id is required");
    }
    let mut cleared = false;
    for layer in &user_layers(config, paths) {
        cleared |= clear_disabled_id(&layer.dir.join("index.json"), id)?;
    }
    Ok(json!({ "ok": true, "id": id, "enabled": cleared }))
}

/// 未注册文件补描述注册:路径必须已在某个用户层里(overview 给的),就地登记。
pub(crate) fn scripts_dashboard_register(
    config: &AppConfig,
    paths: &MiyuPaths,
    requested_path: &str,
    description: &str,
    id: &str,
) -> Result<Value> {
    let path = resolve_previewable(config, paths, requested_path.trim())?;
    let layers = user_layers(config, paths);
    if layer_containing(&path, &layers).is_none() {
        bail!("only scripts inside the user script directories can be registered here");
    }
    let mut args = json!({
        "action": "register",
        "path": path.display().to_string(),
    });
    if !description.trim().is_empty() {
        args["description"] = json!(description.trim());
    }
    if !id.trim().is_empty() {
        args["id"] = json!(id.trim());
    }
    parse_result(register_script(args, config, paths)?)
}
