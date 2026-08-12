use super::{ToolProgress, ToolRegistry, ToolSpec};
use crate::i18n::agent_text as t;
use crate::paths::MiyuPaths;
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::io::Write;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::path::{Component, Path};

pub(super) const MAX_ARTIFACT_BYTES: usize = 20 * 1024 * 1024;

pub fn register_webui(registry: &mut ToolRegistry, paths: &MiyuPaths, session_id: &str) {
    let root = paths.data_dir.join("artifacts");
    register_create(registry, root.clone(), session_id);
    register_present(registry);
    register_read(registry, root.clone(), session_id);
    super::apply_patch::register_artifact(registry, root, session_id);
}

pub fn managed_manifest(paths: &MiyuPaths, session_id: &str) -> Result<String> {
    let root = paths.data_dir.join("artifacts");
    validate_session_id(session_id)?;
    let session_dir = root.join(session_id);
    let mut entries = Vec::new();
    let Ok(read_dir) = std::fs::read_dir(&session_dir) else {
        return Ok("（暂无托管 Artifact）".to_string());
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
        return Ok("（暂无托管 Artifact）".to_string());
    }
    let mut output = String::from("当前会话的托管 Artifact 文件：");
    for (name, size) in entries.into_iter().take(100) {
        output.push_str(&format!("\n- {name} ({size} bytes)"));
    }
    Ok(output)
}

fn register_present(registry: &mut ToolRegistry) {
    registry.register(ToolSpec::new_with_progress(
        "present_artifact",
        t(
            "Publish a completed file to the WebUI preview workspace. Use this only for files the user should inspect as a deliverable, not routine source edits.",
            "将已完成的文件发布到 WebUI 预览工作区。仅用于需要用户查看的交付物，不要用于普通源码编辑。",
        ),
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": t("File path to publish.", "要发布的文件路径。")
                },
                "title": {
                    "type": "string",
                    "description": t("Optional display title.", "可选的显示标题。")
                }
            },
            "required": ["path"],
            "additionalProperties": false
        }),
        |args, progress| async move { present_artifact(args, progress) },
    ).presentation());
}

fn register_create(registry: &mut ToolRegistry, root: PathBuf, session_id: &str) {
    let session_id = session_id.to_string();
    registry.register(ToolSpec::new_with_progress(
        "create_artifact",
        t(
            "Create or update a completed deliverable in Miyu's managed Artifact workspace and display it in the WebUI. Use this for reports, documents, standalone code, HTML, JSON, CSV, and other files the user should inspect. Do not use it for routine project source edits.",
            "在 Miyu 托管的 Artifact 工作区创建或更新已完成的交付物，并展示到 WebUI。适用于报告、文档、独立代码、HTML、JSON、CSV 等需要用户查看的文件；不要用于普通项目源码修改。",
        ),
        json!({
            "type": "object",
            "properties": {
                "filename": {
                    "type": "string",
                    "description": t("File name including an appropriate extension. Directory components are not allowed.", "包含正确扩展名的文件名，不允许包含目录。")
                },
                "content": {
                    "type": "string",
                    "description": t("Complete file content.", "完整文件内容。")
                },
                "title": {
                    "type": "string",
                    "description": t("Optional display title.", "可选的显示标题。")
                }
            },
            "required": ["filename", "content"],
            "additionalProperties": false
        }),
        move |args, progress| {
            let root = root.clone();
            let session_id = session_id.clone();
            async move { create_artifact(args, progress, &root, &session_id) }
        },
    ).presentation());
}

fn register_read(registry: &mut ToolRegistry, root: PathBuf, session_id: &str) {
    let read_session = session_id.to_string();
    registry.register(ToolSpec::new(
        "read_artifact",
        t(
            "Read an existing file from the current WebUI Artifact workspace. Use the filename from the Artifact list; do not search the filesystem with glob.",
            "读取当前 WebUI Artifact 工作区中的已有文件。使用 Artifact 清单中的文件名，不要用 glob 搜索文件系统。",
        ),
        json!({
            "type": "object",
            "properties": {
                "filename": {"type": "string", "description": t("Managed Artifact file name.", "托管 Artifact 文件名。")},
                "offset": {"type": "integer", "description": t("1-based line offset.", "从 1 开始的行偏移。")},
                "limit": {"type": "integer", "description": t("Maximum lines to read.", "最多读取行数。")}
            },
            "required": ["filename"],
            "additionalProperties": false
        }),
        move |args| {
            let root = root.clone();
            let session_id = read_session.clone();
            async move { read_artifact(args, &root, &session_id) }
        },
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
    crate::sys::set_secure_file_permissions(temp.path())?;
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

fn read_artifact(args: Value, root: &Path, session_id: &str) -> Result<String> {
    let filename = required_filename(&args)?;
    let path = managed_file_path(root, session_id, filename)?;
    let mut scoped = args;
    scoped["path"] = Value::String(path.to_string_lossy().to_string());
    super::default_tools::read_file(scoped)
}

fn required_filename(args: &Value) -> Result<&str> {
    let filename = args
        .get("filename")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim();
    validate_file_name(filename)?;
    Ok(filename)
}

fn managed_file_path(root: &Path, session_id: &str, filename: &str) -> Result<PathBuf> {
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
    crate::sys::set_secure_dir_permissions(path)?;
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
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
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
            &paths.data_dir.join("artifacts"),
            "sess_test",
        )
        .unwrap();
        let manifest = managed_manifest(&paths, "sess_test").unwrap();
        assert!(manifest.contains("secret-report.md"));
        assert!(!manifest.contains("private body"));
    }
}
