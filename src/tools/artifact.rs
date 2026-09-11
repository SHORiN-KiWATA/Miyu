use super::{ToolProgress, ToolRegistry, ToolSpec};
use crate::paths::MiyuPaths;
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
#[cfg(test)]
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::path::{Component, Path};

pub(super) const MAX_ARTIFACT_BYTES: usize = 20 * 1024 * 1024;

// 08-21 二次裁定:Artifact 写入独立成 `artifact` 补丁工具(域名即广告),
// 读取走 read 的 artifact: 前缀;发布仍是 present_artifact。
pub fn register_webui(registry: &mut ToolRegistry, paths: &MiyuPaths, session_id: &str) {
    register_present(registry);
    super::apply_patch::register_artifact(registry, paths.artifacts_dir(), session_id);
}

pub fn managed_manifest(paths: &MiyuPaths, session_id: &str) -> Result<String> {
    let root = paths.artifacts_dir();
    validate_session_id(session_id)?;
    let session_dir = root.join(session_id);
    let mut entries = Vec::new();
    let Ok(read_dir) = std::fs::read_dir(&session_dir) else {
        return Ok("(no managed artifacts yet)".to_string());
    };
    for entry in read_dir.flatten() {
        let metadata = match entry.metadata() {
            Ok(metadata) if metadata.is_file() => metadata,
            _ => continue,
        };
        let name = entry.file_name().to_string_lossy().to_string();
        if name.chars().any(char::is_control) {
            continue;
        }
        entries.push((name, metadata.len()));
    }
    entries.sort_by(|left, right| left.0.cmp(&right.0));
    if entries.is_empty() {
        return Ok("(no managed artifacts yet)".to_string());
    }
    let mut output = String::from("Managed artifact files in this session:");
    for (name, size) in entries.into_iter().take(100) {
        output.push_str(&format!("\n- {name} ({size} bytes)"));
    }
    Ok(output)
}

fn register_present(registry: &mut ToolRegistry) {
    registry.register(ToolSpec::new_with_progress(
        "present_artifact",
        "Publish a completed file to the WebUI preview workspace. Use this only for files the user should inspect as a deliverable, not routine source edits.",
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "File path to publish."
                },
                "title": {
                    "type": "string",
                    "description": "Optional display title."
                }
            },
            "required": ["path"],
            "additionalProperties": false
        }),
        |args, progress| async move { present_artifact(args, progress) },
    ).presentation());
}

fn present_artifact(args: Value, progress: ToolProgress) -> Result<String> {
    let raw_path = args
        .get("path")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim();
    if raw_path.is_empty() {
        bail!("path is required");
    }
    let path = expand_path(raw_path);
    crate::tools::sandbox::guard_read(&path)?;
    let metadata = std::fs::metadata(&path)?;
    if !metadata.is_file() {
        bail!("artifact path is not a file: {}", path.display());
    }
    let title = args
        .get("title")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim()
        .to_string();
    progress.report_artifact(path.clone(), title.clone());
    let filename = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("artifact")
        .to_string();
    Ok(serde_json::to_string_pretty(&json!({
        "ok": true,
        "path": path,
        "filename": filename,
        "title": title,
        "published": true
    }))?)
}

// Edit/Read 统一后生产路径走 edit 的 artifact: 命名空间;本函数仅测试
// 保留(路径逃逸/权限语义的回归靠它,底层 managed_file_path 与生产共享)。
#[cfg(test)]
fn create_artifact(
    args: Value,
    progress: ToolProgress,
    root: &Path,
    session_id: &str,
) -> Result<String> {
    let filename = args
        .get("filename")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim();
    validate_file_name(filename)?;
    validate_session_id(session_id)?;
    let content = args
        .get("content")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("content is required"))?;
    if content.is_empty() || content.len() > MAX_ARTIFACT_BYTES {
        bail!("artifact content must be between 1 byte and 20 MiB");
    }
    let title = args
        .get("title")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim();

    ensure_private_dir(root)?;
    let session_dir = root.join(session_id);
    ensure_private_dir(&session_dir)?;
    let path = session_dir.join(filename);
    if let Ok(metadata) = std::fs::symlink_metadata(&path) {
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            bail!(
                "artifact destination is not a regular file: {}",
                path.display()
            );
        }
    }
    let mut temp = tempfile::NamedTempFile::new_in(&session_dir)?;
    temp.as_file_mut()
        .set_permissions(std::fs::Permissions::from_mode(0o600))?;
    temp.write_all(content.as_bytes())?;
    temp.as_file_mut().sync_all()?;
    temp.persist(&path)?;
    progress.report_artifact(path.clone(), title);
    Ok(serde_json::to_string_pretty(&json!({
        "ok": true,
        "path": path,
        "filename": filename,
        "title": title,
        "published": true
    }))?)
}

#[cfg(test)]
fn read_artifact(args: Value, root: &Path, session_id: &str) -> Result<String> {
    let filename = required_filename(&args)?;
    let path = managed_file_path(root, session_id, filename)?;
    let mut scoped = args;
    scoped["path"] = Value::String(path.to_string_lossy().to_string());
    super::default_tools::read_file(scoped)
}

#[cfg(test)]
fn required_filename(args: &Value) -> Result<&str> {
    let filename = args
        .get("filename")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim();
    validate_file_name(filename)?;
    Ok(filename)
}

pub(in crate::tools) fn managed_file_path(
    root: &Path,
    session_id: &str,
    filename: &str,
) -> Result<PathBuf> {
    validate_session_id(session_id)?;
    let session_dir = root.join(session_id);
    let session_canonical = session_dir.canonicalize().with_context(|| {
        format!(
            "Artifact workspace does not exist: {}",
            session_dir.display()
        )
    })?;
    let path = session_dir.join(filename);
    let metadata = std::fs::symlink_metadata(&path)
        .with_context(|| format!("Artifact does not exist: {filename}"))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        bail!("Artifact is not a regular file: {filename}");
    }
    let canonical = path.canonicalize()?;
    if canonical.parent() != Some(session_canonical.as_path()) {
        bail!("Artifact path escaped its managed workspace");
    }
    Ok(canonical)
}

fn validate_file_name(filename: &str) -> Result<()> {
    let path = Path::new(filename);
    let mut components = path.components();
    let valid_component =
        matches!(components.next(), Some(Component::Normal(_))) && components.next().is_none();
    if !valid_component || filename.chars().count() > 180 || filename.chars().any(char::is_control)
    {
        bail!("filename must be a single safe file name");
    }
    Ok(())
}

fn validate_session_id(session_id: &str) -> Result<()> {
    let mut components = Path::new(session_id).components();
    if !matches!(components.next(), Some(Component::Normal(_))) || components.next().is_some() {
        bail!("invalid session id for Artifact workspace");
    }
    Ok(())
}

fn ensure_private_dir(path: &Path) -> Result<()> {
    std::fs::create_dir_all(path)?;
    let metadata = std::fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        bail!(
            "Artifact workspace path is not a directory: {}",
            path.display()
        );
    }
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
    Ok(())
}

fn expand_path(value: &str) -> PathBuf {
    if let Some(rest) = value.strip_prefix("~/") {
        if let Some(home) = directories::BaseDirs::new().map(|dirs| dirs.home_dir().to_path_buf()) {
            return home.join(rest);
        }
    }
    let path = std::path::Path::new(value);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        super::workspace::effective_workdir().join(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn managed_artifact_is_private_and_published() {
        let temp = tempfile::tempdir().unwrap();
        let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
        let output = create_artifact(
            json!({"filename":"report.md", "content":"# Report\n", "title":"Report"}),
            ToolProgress::new(sender),
            temp.path(),
            "sess_test",
        )
        .unwrap();
        let payload: Value = serde_json::from_str(&output).unwrap();
        assert_eq!(payload["published"], true);
        let path = temp.path().join("sess_test/report.md");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "# Report\n");
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert!(matches!(
            receiver.try_recv().unwrap(),
            super::super::ToolProgressEvent::Artifact { path: event_path, .. } if event_path == path
        ));
    }

    #[test]
    fn managed_artifact_rejects_path_escape() {
        let temp = tempfile::tempdir().unwrap();
        for filename in ["../report.md", "/tmp/report.md", "nested/report.md", ""] {
            assert!(create_artifact(
                json!({"filename":filename, "content":"content"}),
                ToolProgress::default(),
                temp.path(),
                "sess_test",
            )
            .is_err());
        }
    }

    #[test]
    fn managed_artifact_can_be_read() {
        let temp = tempfile::tempdir().unwrap();
        create_artifact(
            json!({
                "filename":"deployment-plan.md",
                "content":"# Plan\n\n## Rollback\nRestore the previous release.\n"
            }),
            ToolProgress::default(),
            temp.path(),
            "sess_test",
        )
        .unwrap();

        let read = read_artifact(
            json!({"filename":"deployment-plan.md"}),
            temp.path(),
            "sess_test",
        )
        .unwrap();
        assert!(read.contains("3: ## Rollback"));
    }

    #[test]
    fn artifact_manifest_lists_names_without_file_contents() {
        let temp = tempfile::tempdir().unwrap();
        let paths = MiyuPaths {
            root_dir: temp.path().to_path_buf(),
            config_dir: temp.path().join("config"),
            config_file: temp.path().join("config/config.jsonc"),
            skills_dir: temp.path().join("config/skills"),
            data_dir: temp.path().join("data"),
            cache_dir: temp.path().join("cache"),
            state_dir: temp.path().join("state"),
            pictures_dir: temp.path().join("pictures"),
            fish_hook_file: temp.path().join("fish"),
            bash_hook_file: temp.path().join("bash"),
            zsh_hook_file: temp.path().join("zsh"),
            scripts_dir: temp.path().join("scripts"),
            system_scripts_dir: temp.path().join("system-scripts"),
        };
        create_artifact(
            json!({"filename":"secret-report.md", "content":"private body"}),
            ToolProgress::default(),
            &paths.artifacts_dir(),
            "sess_test",
        )
        .unwrap();
        let manifest = managed_manifest(&paths, "sess_test").unwrap();
        assert!(manifest.contains("secret-report.md"));
        assert!(!manifest.contains("private body"));
    }
}
