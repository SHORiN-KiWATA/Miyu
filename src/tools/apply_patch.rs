use super::{ToolProgress, ToolRegistry, ToolSpec};
use crate::i18n::text as t;
use crate::tools::patch_preview::write_with_patch_preview;
use anyhow::{bail, Result};
use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

pub fn register(registry: &mut ToolRegistry) {
    registry.register(ToolSpec::new_with_progress(
        "apply_patch",
        t(
            "Apply a batch patch to files. Prefer this for complex edits and multiple changes in the same file.",
            "批量应用文件补丁。复杂编辑和同一文件多处修改优先使用。",
        ),
        json!({
            "type": "object",
            "properties": {
                "patchText": {
                    "type": "string",
                    "description": "Full patch text wrapped in *** Begin Patch / *** End Patch."
                }
            },
            "required": ["patchText"],
            "additionalProperties": false
        }),
        |args, progress| async move {
            progress.report(format!(
                "__tool_phase__~ {}",
                t("prepare patch", "准备修改")
            ));
            tokio::task::yield_now().await;
            apply_patch(args, progress)
        },
    ).writes());
}

fn apply_patch(args: Value, progress: ToolProgress) -> Result<String> {
    let patch_text = args
        .get("patchText")
        .or_else(|| args.get("patch_text"))
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("patchText is required"))?;
    let operations = parse_patch(patch_text)?;
    if operations.is_empty() {
        bail!("patch rejected: empty patch")
    }

    let changes = preflight_operations(operations)?;

    for change in &changes {
        progress.report(format!(
            "__tool_phase__~ {} {}",
            t("prepare patch", "准备修改"),
            display_path_for_progress(&change.path)
        ));
    }

    let mut files = Vec::new();
    for change in changes {
        match change.kind {
            ChangeKind::Delete => {
                std::fs::remove_file(&change.path)?;
                report_delete_preview(&progress, &change.path, &change.before)?;
            }
            ChangeKind::Add | ChangeKind::Update => {
                write_with_patch_preview(
                    &change.path,
                    &change.before,
                    &change.after,
                    &progress,
                    Map::new(),
                )?;
            }
        }
        files.push(json!({
            "path": display_path_for_progress(&change.path),
            "operation": change.kind.as_str(),
        }));
    }

    Ok(serde_json::to_string_pretty(&json!({
        "ok": true,
        "operation": "apply_patch",
        "files_changed": files.len(),
        "files": files,
    }))?)
}

fn report_delete_preview(progress: &ToolProgress, path: &Path, before: &str) -> Result<()> {
    let diff = crate::tools::patch_preview::patch_result_json(path, before, "");
    let payload = serde_json::to_string(&json!({
        "path": display_path_for_progress(path),
        "diff": diff,
    }))?;
    progress.report(format!("__patch_preview__{payload}"));
    Ok(())
}

#[derive(Debug, Clone)]
enum Operation {
    Add {
        path: PathBuf,
        lines: Vec<String>,
    },
    Delete {
        path: PathBuf,
    },
    Update {
        path: PathBuf,
        move_to: Option<PathBuf>,
        hunks: Vec<Hunk>,
    },
}

#[derive(Debug, Clone)]
struct Hunk {
    context: Option<String>,
    end_of_file: bool,
    lines: Vec<HunkLine>,
}

#[derive(Debug, Clone, Eq, PartialEq)]
enum HunkLine {
    Context(String),
    Delete(String),
    Insert(String),
}

#[derive(Debug, Clone, Copy)]
enum ChangeKind {
    Add,
    Update,
    Delete,
}

impl ChangeKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Add => "add",
            Self::Update => "update",
            Self::Delete => "delete",
        }
    }
}

struct FileChange {
    path: PathBuf,
    before: String,
    after: String,
    kind: ChangeKind,
}

fn parse_patch(raw: &str) -> Result<Vec<Operation>> {
    let normalized = strip_wrappers(raw)
        .replace("\r\n", "\n")
        .replace('\r', "\n");
    let lines = normalized.lines().collect::<Vec<_>>();
    let begin = lines
        .iter()
        .position(|line| line.trim() == "*** Begin Patch")
        .ok_or_else(|| {
            anyhow::anyhow!("apply_patch verification failed: missing *** Begin Patch")
        })?;
    let end = lines
        .iter()
        .rposition(|line| line.trim() == "*** End Patch")
        .ok_or_else(|| anyhow::anyhow!("apply_patch verification failed: missing *** End Patch"))?;
    if begin >= end {
        bail!("apply_patch verification failed: missing *** Begin Patch")
    }

    let mut operations = Vec::new();
    let mut index = begin + 1;
    while index < end {
        let line = lines[index];
        if line.trim().is_empty() {
            index += 1;
            continue;
        }
        if let Some(path) = header_value(line, "*** Add File:") {
            index += 1;
            let mut content = Vec::new();
            while index < end && !is_patch_header(lines[index]) {
                let Some(rest) = lines[index].strip_prefix('+') else {
                    bail!("apply_patch verification failed: Add File lines must start with +")
                };
                content.push(rest.to_string());
                index += 1;
            }
            operations.push(Operation::Add {
                path: path_arg(path)?,
                lines: content,
            });
        } else if let Some(path) = header_value(line, "*** Delete File:") {
            operations.push(Operation::Delete {
                path: path_arg(path)?,
            });
            index += 1;
        } else if let Some(path) = header_value(line, "*** Update File:") {
            index += 1;
            let mut move_to = None;
            if index < end {
                if let Some(target) = header_value(lines[index], "*** Move to:") {
                    move_to = Some(path_arg(target)?);
                    index += 1;
                }
            }
            let mut hunks = Vec::new();
            while index < end && !is_patch_header(lines[index]) {
                if lines[index].starts_with("--- ") || lines[index].starts_with("+++ ") {
                    index += 1;
                    continue;
                }
                if !lines[index].starts_with("@@") {
                    index += 1;
                    continue;
                }
                let context = lines[index]
                    .strip_prefix("@@")
                    .unwrap_or_default()
                    .trim()
                    .trim_matches('@')
                    .trim();
                let context =
                    (!context.is_empty() && !context.starts_with('-') && !context.starts_with('+'))
                        .then(|| context.to_string());
                index += 1;
                let mut hunk_lines = Vec::new();
                let mut end_of_file = false;
                while index < end
                    && !lines[index].starts_with("@@")
                    && !is_patch_header(lines[index])
                {
                    let line = lines[index];
                    if let Some(rest) = line.strip_prefix(' ') {
                        hunk_lines.push(HunkLine::Context(rest.to_string()));
                    } else if let Some(rest) = line.strip_prefix('-') {
                        hunk_lines.push(HunkLine::Delete(rest.to_string()));
                    } else if let Some(rest) = line.strip_prefix('+') {
                        hunk_lines.push(HunkLine::Insert(rest.to_string()));
                    } else if line == "\\ No newline at end of file" {
                    } else if line == "*** End of File" {
                        end_of_file = true;
                        index += 1;
                        break;
                    } else {
                        bail!("apply_patch verification failed: invalid hunk line: {line}")
                    }
                    index += 1;
                }
                if hunk_lines.is_empty() {
                    bail!("apply_patch verification failed: empty hunk")
                }
                hunks.push(Hunk {
                    context,
                    end_of_file,
                    lines: hunk_lines,
                });
            }
            if hunks.is_empty() {
                bail!("apply_patch verification failed: Update File requires at least one hunk")
            }
            operations.push(Operation::Update {
                path: path_arg(path)?,
                move_to,
                hunks,
            });
        } else {
            bail!("apply_patch verification failed: unknown patch header: {line}")
        }
    }
    Ok(operations)
}

fn strip_wrappers(raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.starts_with("```") && trimmed.ends_with("```") {
        let mut lines = trimmed.lines().collect::<Vec<_>>();
        lines.remove(0);
        lines.pop();
        return lines.join("\n");
    }
    if let Some(captures) = strip_simple_heredoc(trimmed) {
        return captures;
    }
    trimmed.to_string()
}

fn strip_simple_heredoc(raw: &str) -> Option<String> {
    let first = raw.lines().next()?.trim();
    let marker = first
        .strip_prefix("cat <<")
        .or_else(|| first.strip_prefix("<<"))?
        .trim()
        .trim_matches('\'')
        .trim_matches('"');
    if marker.is_empty() {
        return None;
    }
    let mut body = raw.lines().skip(1).collect::<Vec<_>>();
    if body.last().map(|line| line.trim()) == Some(marker) {
        body.pop();
        return Some(body.join("\n"));
    }
    None
}

fn header_value<'a>(line: &'a str, prefix: &str) -> Option<&'a str> {
    line.strip_prefix(prefix)
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

fn is_patch_header(line: &str) -> bool {
    line.starts_with("*** Add File:")
        || line.starts_with("*** Delete File:")
        || line.starts_with("*** Update File:")
        || line.starts_with("*** End Patch")
}

fn preflight_operations(operations: Vec<Operation>) -> Result<Vec<FileChange>> {
    let mut staged: HashMap<PathBuf, Option<String>> = HashMap::new();
    let mut changes = Vec::new();

    for operation in operations {
        match operation {
            Operation::Add { path, lines } => {
                if staged
                    .get(&path)
                    .and_then(|content| content.as_ref())
                    .is_some()
                    || (!staged.contains_key(&path) && path.exists())
                {
                    bail!(
                        "apply_patch verification failed: file already exists: {}",
                        path.display()
                    )
                }
                let after = ensure_trailing_newline(lines.join("\n"));
                staged.insert(path.clone(), Some(after.clone()));
                changes.push(FileChange {
                    path,
                    before: String::new(),
                    after,
                    kind: ChangeKind::Add,
                });
            }
            Operation::Delete { path } => {
                let before = staged_content(&path, &staged)?;
                staged.insert(path.clone(), None);
                changes.push(FileChange {
                    path,
                    before,
                    after: String::new(),
                    kind: ChangeKind::Delete,
                });
            }
            Operation::Update {
                path,
                move_to,
                hunks,
            } => {
                if move_to.is_some() {
                    bail!("apply_patch verification failed: Move to is not supported yet")
                }
                let before = staged_content(&path, &staged)?;
                let mut after = before.clone();
                for hunk in hunks {
                    after = apply_hunk(&path, &after, &hunk)?;
                }
                staged.insert(path.clone(), Some(after.clone()));
                changes.push(FileChange {
                    path,
                    before,
                    after,
                    kind: ChangeKind::Update,
                });
            }
        }
    }

    Ok(changes)
}

fn staged_content(path: &Path, staged: &HashMap<PathBuf, Option<String>>) -> Result<String> {
    if let Some(content) = staged.get(path) {
        return content.clone().ok_or_else(|| {
            anyhow::anyhow!(
                "apply_patch verification failed: file was deleted earlier in patch: {}",
                path.display()
            )
        });
    }
    std::fs::read_to_string(path).map_err(|err| {
        anyhow::anyhow!(
            "apply_patch verification failed: failed to read file to update {}: {err}",
            path.display()
        )
    })
}

fn apply_hunk(path: &Path, content: &str, hunk: &Hunk) -> Result<String> {
    let old = hunk_text(&hunk.lines, false);
    let new = hunk_text(&hunk.lines, true);
    if old.is_empty() {
        return apply_insertion_hunk(path, content, hunk, &new);
    }
    let current_lines = content_lines(content);
    let mut pattern = content_lines(&old);
    let new_lines = content_lines(&new);
    let start_index = hunk
        .context
        .as_ref()
        .and_then(|context| seek_sequence(&current_lines, &[context.to_string()], 0, false))
        .map(|index| index + 1)
        .unwrap_or(0);
    if let Some(found) = seek_sequence(&current_lines, &pattern, start_index, hunk.end_of_file) {
        let mut result = current_lines.clone();
        result.splice(found..found + pattern.len(), new_lines);
        return Ok(join_lines(result));
    }
    if pattern.last().is_some_and(|line| line.is_empty()) {
        pattern.pop();
        if let Some(found) = seek_sequence(&current_lines, &pattern, start_index, hunk.end_of_file)
        {
            let mut result = current_lines.clone();
            result.splice(found..found + pattern.len(), content_lines(&new));
            return Ok(join_lines(result));
        }
    }

    // Fallback for legacy simple hunks: keep exact substring replacement.
    if old.is_empty() {
        bail!(
            "apply_patch verification failed: empty update context for {}",
            path.display()
        )
    }
    let count = count_occurrences(content, &old);
    if count == 0 {
        bail!(
            "apply_patch verification failed: hunk does not match {}",
            path.display()
        )
    }
    if count > 1 {
        bail!(
            "apply_patch verification failed: hunk matches multiple locations in {}",
            path.display()
        )
    }
    Ok(content.replacen(&old, &new, 1))
}

fn apply_insertion_hunk(path: &Path, content: &str, hunk: &Hunk, new: &str) -> Result<String> {
    let mut lines = content_lines(content);
    let insert = content_lines(new);
    let index = if hunk.end_of_file {
        lines.len()
    } else if let Some(context) = &hunk.context {
        seek_sequence(&lines, &[context.to_string()], 0, false)
            .map(|index| index + 1)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "apply_patch verification failed: failed to find context '{}' in {}",
                    context,
                    path.display()
                )
            })?
    } else {
        lines.len()
    };
    lines.splice(index..index, insert);
    Ok(join_lines(lines))
}

fn hunk_text(lines: &[HunkLine], new_side: bool) -> String {
    let selected = lines
        .iter()
        .filter_map(|line| match (new_side, line) {
            (_, HunkLine::Context(text)) => Some(text.as_str()),
            (false, HunkLine::Delete(text)) => Some(text.as_str()),
            (true, HunkLine::Insert(text)) => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>();
    ensure_trailing_newline(selected.join("\n"))
}

fn ensure_trailing_newline(mut value: String) -> String {
    if !value.is_empty() && !value.ends_with('\n') {
        value.push('\n');
    }
    value
}

fn content_lines(content: &str) -> Vec<String> {
    let mut lines = content.split('\n').map(str::to_string).collect::<Vec<_>>();
    if lines.last().is_some_and(|line| line.is_empty()) {
        lines.pop();
    }
    lines
}

fn join_lines(mut lines: Vec<String>) -> String {
    if lines.is_empty() {
        return String::new();
    }
    lines.push(String::new());
    lines.join("\n")
}

fn seek_sequence(
    lines: &[String],
    pattern: &[String],
    start_index: usize,
    eof: bool,
) -> Option<usize> {
    if pattern.is_empty() {
        return None;
    }
    if eof {
        let from_end = lines.len().checked_sub(pattern.len())?;
        if from_end >= start_index && lines_match_at(lines, pattern, from_end, CompareMode::Exact) {
            return Some(from_end);
        }
    }
    for mode in [
        CompareMode::Exact,
        CompareMode::TrimEnd,
        CompareMode::Trim,
        CompareMode::Normalize,
    ] {
        for index in start_index..=lines.len().saturating_sub(pattern.len()) {
            if lines_match_at(lines, pattern, index, mode) {
                return Some(index);
            }
        }
    }
    None
}

#[derive(Clone, Copy)]
enum CompareMode {
    Exact,
    TrimEnd,
    Trim,
    Normalize,
}

fn lines_match_at(lines: &[String], pattern: &[String], index: usize, mode: CompareMode) -> bool {
    pattern.iter().enumerate().all(|(offset, expected)| {
        let Some(actual) = lines.get(index + offset) else {
            return false;
        };
        match mode {
            CompareMode::Exact => actual == expected,
            CompareMode::TrimEnd => actual.trim_end() == expected.trim_end(),
            CompareMode::Trim => actual.trim() == expected.trim(),
            CompareMode::Normalize => normalize_match(actual) == normalize_match(expected),
        }
    })
}

fn normalize_match(value: &str) -> String {
    value
        .trim()
        .replace(['‘', '’', '‚', '‛'], "'")
        .replace(['“', '”', '„', '‟'], "\"")
        .replace(['‐', '‑', '‒', '–', '—', '―'], "-")
        .replace('…', "...")
        .replace('\u{00a0}', " ")
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

fn path_arg(value: &str) -> Result<PathBuf> {
    let value = value.trim();
    if value.is_empty() {
        bail!("path is required")
    }
    if let Some(rest) = value.strip_prefix("~/") {
        if let Some(home) = directories::BaseDirs::new().map(|dirs| dirs.home_dir().to_path_buf()) {
            return Ok(home.join(rest));
        }
    }
    let path = Path::new(value);
    Ok(if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(path)
    })
}

fn display_path_for_progress(path: &Path) -> String {
    crate::tools::patch_preview::display_path(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_add_update_delete_patch() {
        let patch = "*** Begin Patch\n*** Add File: a.txt\n+hello\n*** Update File: b.txt\n@@ marker\n-old\n+new\n*** Delete File: c.txt\n*** End Patch";
        let operations = parse_patch(patch).unwrap();
        assert_eq!(operations.len(), 3);
    }

    #[test]
    fn update_hunk_replaces_exact_text() {
        let path = PathBuf::from("demo.txt");
        let hunk = Hunk {
            context: None,
            end_of_file: false,
            lines: vec![
                HunkLine::Context("one".to_string()),
                HunkLine::Delete("two".to_string()),
                HunkLine::Insert("TWO".to_string()),
                HunkLine::Context("three".to_string()),
            ],
        };
        let result = apply_hunk(&path, "one\ntwo\nthree\n", &hunk).unwrap();
        assert_eq!(result, "one\nTWO\nthree\n");
    }

    #[test]
    fn update_hunk_fails_when_stale() {
        let path = PathBuf::from("demo.txt");
        let hunk = Hunk {
            context: None,
            end_of_file: false,
            lines: vec![
                HunkLine::Delete("missing".to_string()),
                HunkLine::Insert("new".to_string()),
            ],
        };
        assert!(apply_hunk(&path, "current\n", &hunk).is_err());
    }

    #[test]
    fn parses_fenced_patch_with_no_space_after_header_colon() {
        let patch = "```\n*** Begin Patch\n*** Add File:a.txt\n+hello\n*** End Patch\n```";
        let operations = parse_patch(patch).unwrap();
        assert_eq!(operations.len(), 1);
    }

    #[test]
    fn insertion_hunk_uses_context_header() {
        let path = PathBuf::from("demo.txt");
        let hunk = Hunk {
            context: Some("one".to_string()),
            end_of_file: false,
            lines: vec![HunkLine::Insert("inserted".to_string())],
        };
        let result = apply_hunk(&path, "one\ntwo\n", &hunk).unwrap();
        assert_eq!(result, "one\ninserted\ntwo\n");
    }

    #[test]
    fn apply_patch_adds_updates_and_deletes_files() {
        let temp = tempfile::tempdir().unwrap();
        let keep = temp.path().join("keep.txt");
        let remove = temp.path().join("remove.txt");
        std::fs::write(&keep, "one\ntwo\nthree\n").unwrap();
        std::fs::write(&remove, "delete me\n").unwrap();

        let patch = format!(
            "*** Begin Patch\n*** Add File: {}\n+new file\n*** Update File: {}\n@@ patch\n one\n-two\n+TWO\n three\n*** Delete File: {}\n*** End Patch",
            temp.path().join("new.txt").display(),
            keep.display(),
            remove.display()
        );
        let result = apply_patch(json!({ "patchText": patch }), ToolProgress::default()).unwrap();
        let data: Value = serde_json::from_str(&result).unwrap();

        assert_eq!(data["ok"], true);
        assert_eq!(data["files_changed"], 3);
        assert_eq!(
            std::fs::read_to_string(temp.path().join("new.txt")).unwrap(),
            "new file\n"
        );
        assert_eq!(std::fs::read_to_string(&keep).unwrap(), "one\nTWO\nthree\n");
        assert!(!remove.exists());
    }

    #[test]
    fn apply_patch_repeated_update_sections_use_staged_content() {
        let temp = tempfile::tempdir().unwrap();
        let file = temp.path().join("repeated.txt");
        std::fs::write(&file, "one\ntwo\nthree\n").unwrap();

        let patch = format!(
            "*** Begin Patch\n*** Update File: {}\n@@ first\n-one\n+ONE\n*** Update File: {}\n@@ second\n ONE\n-two\n+TWO\n three\n*** End Patch",
            file.display(),
            file.display()
        );
        let result = apply_patch(json!({ "patchText": patch }), ToolProgress::default()).unwrap();
        let data: Value = serde_json::from_str(&result).unwrap();

        assert_eq!(data["ok"], true);
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "ONE\nTWO\nthree\n");
    }

    #[test]
    fn apply_patch_rejects_move_to_until_supported() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source.txt");
        let target = temp.path().join("target.txt");
        std::fs::write(&source, "old\n").unwrap();
        let patch = format!(
            "*** Begin Patch\n*** Update File: {}\n*** Move to: {}\n@@ patch\n-old\n+new\n*** End Patch",
            source.display(),
            target.display()
        );

        assert!(apply_patch(json!({ "patchText": patch }), ToolProgress::default()).is_err());
        assert!(source.exists());
        assert!(!target.exists());
    }
}
