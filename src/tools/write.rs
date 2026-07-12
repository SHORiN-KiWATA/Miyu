use super::{ToolProgress, ToolRegistry, ToolSpec};
use crate::i18n::text as t;
use crate::tools::patch_preview::write_with_patch_preview;
use anyhow::Result;
use serde_json::{json, Value};
use std::path::PathBuf;

pub fn register(registry: &mut ToolRegistry) {
    registry.register(ToolSpec::new_with_progress(
        "write_file",
        t(
            "Write content to a file, creating it if it does not exist or overwriting if it does. Supports absolute, workspace-relative, and ~/ paths.",
            "写入文件内容。文件不存在时创建，存在时覆盖。支持绝对路径、工作区相对路径和 ~/ 路径。",
        ),
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": t("File path. Supports absolute, workspace-relative, and ~/ paths.", "文件路径。支持绝对路径、工作区相对路径和 ~/ 路径。")
                },
                "content": {
                    "type": "string",
                    "description": t("Full file content to write.", "要写入的完整文件内容。")
                }
            },
            "required": ["path", "content"],
            "additionalProperties": false
        }),
        |args, progress| async move { write_file(args, progress) },
    ).writes());
}

fn write_file(args: Value, progress: ToolProgress) -> Result<String> {
    let path = path_arg(&args, "path")?;
    let content = args
        .get("content")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("content is required"))?;
    let content = content.to_string();

    let existed = path.exists();
    let original = if existed {
        std::fs::read_to_string(&path).unwrap_or_default()
    } else {
        String::new()
    };
    write_with_patch_preview(
        &path,
        &original,
        &content,
        &progress,
        serde_json::Map::from_iter([
            ("created".to_string(), json!(!existed)),
            ("overwritten".to_string(), json!(existed)),
        ]),
    )
}

fn path_arg(args: &Value, key: &str) -> Result<PathBuf> {
    let value = args
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim();
    if value.is_empty() {
        anyhow::bail!("{} is required", key);
    }
    Ok(expand_path(value))
}

fn expand_path(value: &str) -> PathBuf {
    let value = value.trim();
    if let Some(rest) = value.strip_prefix("~/") {
        if let Some(home) = directories::BaseDirs::new().map(|dirs| dirs.home_dir().to_path_buf()) {
            return home.join(rest);
        }
    }
    let path = std::path::Path::new(value);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_creates_new_file() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("new.txt");
        let result = write_file(
            json!({
                "path": path.display().to_string(),
                "content": "hello world\n"
            }),
            ToolProgress::default(),
        )
        .unwrap();
        let data: Value = serde_json::from_str(&result).unwrap();
        assert_eq!(data["ok"], true);
        assert_eq!(data["created"], true);
        assert!(data.get("diff").is_none());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "hello world\n");
    }

    #[test]
    fn write_overwrites_existing_file() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("existing.txt");
        std::fs::write(&path, "old content\n").unwrap();
        let result = write_file(
            json!({
                "path": path.display().to_string(),
                "content": "new content\n"
            }),
            ToolProgress::default(),
        )
        .unwrap();
        let data: Value = serde_json::from_str(&result).unwrap();
        assert_eq!(data["ok"], true);
        assert_eq!(data["overwritten"], true);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "new content\n");
    }

    #[test]
    fn write_creates_parent_dirs() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("a/b/c/file.txt");
        let result = write_file(
            json!({
                "path": path.display().to_string(),
                "content": "nested\n"
            }),
            ToolProgress::default(),
        )
        .unwrap();
        let data: Value = serde_json::from_str(&result).unwrap();
        assert_eq!(data["ok"], true);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "nested\n");
    }
}
