use super::{ToolProgress, ToolRegistry, ToolSpec};
use crate::i18n::text as t;
use crate::tools::patch_preview::write_with_patch_preview;
use anyhow::{bail, Result};
use serde_json::{json, Value};
use std::path::PathBuf;

pub fn register(registry: &mut ToolRegistry) {
    registry.register(ToolSpec::new_with_progress(
        "edit_string",
        t(
            "Edit a file by replacing an exact string match. Fails if oldString is not found or found multiple times (unless replaceAll). Must differ from newString.",
            "通过精确字符串匹配替换编辑文件。oldString 未找到或找到多个匹配时报错（除非使用 replaceAll）。oldString 必须与 newString 不同。",
        ),
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": t("File path. Supports absolute, workspace-relative, and ~/ paths.", "文件路径。支持绝对路径、工作区相对路径和 ~/ 路径。")
                },
                "old_string": {
                    "type": "string",
                    "description": t("Exact text to replace. Must match exactly including whitespace and indentation.", "要替换的精确文本。必须完全匹配包括空白和缩进。")
                },
                "new_string": {
                    "type": "string",
                    "description": t("Replacement text. Must differ from old_string.", "替换文本。必须与 old_string 不同。")
                },
                "replace_all": {
                    "type": "boolean",
                    "description": t("Replace all occurrences of old_string (default false).", "替换所有匹配项（默认 false）。"),
                    "default": false
                }
            },
            "required": ["path", "old_string", "new_string"],
            "additionalProperties": false
        }),
        |args, progress| async move { edit_string(args, progress) },
    ).writes());
}

fn edit_string(args: Value, progress: ToolProgress) -> Result<String> {
    let path = path_arg(&args, "path")?;
    let old_string = args
        .get("old_string")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("old_string is required"))?;
    let new_string = args
        .get("new_string")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("new_string is required"))?;
    let replace_all = args
        .get("replace_all")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    if old_string == new_string {
        bail!("old_string and new_string must differ");
    }
    if old_string.is_empty() {
        bail!("old_string must not be empty; use write_file to create or overwrite a file");
    }

    let original = std::fs::read_to_string(&path)?;
    let count = count_occurrences(&original, old_string);

    if count == 0 {
        bail!(
            "Could not find old_string in the file. It must match exactly, including whitespace and indentation."
        );
    }
    if count > 1 && !replace_all {
        bail!(
            "Found {} exact matches for old_string. Provide more surrounding context or set replace_all to true.",
            count
        );
    }

    let updated = if replace_all {
        original.replace(old_string, new_string)
    } else {
        original.replacen(old_string, new_string, 1)
    };

    write_with_patch_preview(
        &path,
        &original,
        &updated,
        &progress,
        serde_json::Map::from_iter([(
            "replacements".to_string(),
            json!(if replace_all { count } else { 1 }),
        )]),
    )
}

fn count_occurrences(content: &str, search: &str) -> usize {
    if search.is_empty() {
        return 0;
    }
    let mut count = 0;
    let mut offset = 0;
    while let Some(pos) = content[offset..].find(search) {
        count += 1;
        offset += pos + search.len();
    }
    count
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
    fn replace_single_match() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("test.txt");
        std::fs::write(&path, "foo bar baz\n").unwrap();
        let result = edit_string(
            json!({
                "path": path.display().to_string(),
                "old_string": "bar",
                "new_string": "BAR"
            }),
            ToolProgress::default(),
        )
        .unwrap();
        let data: Value = serde_json::from_str(&result).unwrap();
        assert_eq!(data["replacements"], 1);
        assert!(data.get("diff").is_none());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "foo BAR baz\n");
    }

    #[test]
    fn replace_all_matches() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("test.txt");
        std::fs::write(&path, "foo foo foo\n").unwrap();
        let result = edit_string(
            json!({
                "path": path.display().to_string(),
                "old_string": "foo",
                "new_string": "qux",
                "replace_all": true
            }),
            ToolProgress::default(),
        )
        .unwrap();
        let data: Value = serde_json::from_str(&result).unwrap();
        assert_eq!(data["replacements"], 3);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "qux qux qux\n");
    }

    #[test]
    fn not_found_errors() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("test.txt");
        std::fs::write(&path, "hello world\n").unwrap();
        let result = edit_string(
            json!({
                "path": path.display().to_string(),
                "old_string": "nonexistent",
                "new_string": "found"
            }),
            ToolProgress::default(),
        );
        assert!(result.is_err());
    }

    #[test]
    fn multiple_matches_without_replace_all_errors() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("test.txt");
        std::fs::write(&path, "a a a\n").unwrap();
        let result = edit_string(
            json!({
                "path": path.display().to_string(),
                "old_string": "a",
                "new_string": "b"
            }),
            ToolProgress::default(),
        );
        assert!(result.is_err());
    }

    #[test]
    fn identical_strings_error() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("test.txt");
        std::fs::write(&path, "same\n").unwrap();
        let result = edit_string(
            json!({
                "path": path.display().to_string(),
                "old_string": "same",
                "new_string": "same"
            }),
            ToolProgress::default(),
        );
        assert!(result.is_err());
    }

    #[test]
    fn multiline_replacement() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("test.txt");
        std::fs::write(&path, "line1\nline2\nline3\n").unwrap();
        let _result = edit_string(
            json!({
                "path": path.display().to_string(),
                "old_string": "line2",
                "new_string": "LINE TWO\nEXTRA"
            }),
            ToolProgress::default(),
        )
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "line1\nLINE TWO\nEXTRA\nline3\n"
        );
    }
}
