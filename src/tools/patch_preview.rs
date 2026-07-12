use super::ToolProgress;
use anyhow::Result;
use serde_json::{json, Map, Value};
use std::path::{Path, PathBuf};

pub(crate) fn write_with_patch_preview(
    path: &Path,
    before: &str,
    after: &str,
    progress: &ToolProgress,
    mut result: Map<String, Value>,
) -> Result<String> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent)?;
    let temp = tempfile::NamedTempFile::new_in(parent)?;
    std::fs::write(temp.path(), after.as_bytes())?;
    temp.persist(path)?;
    report_patch_preview(progress, path, &patch_result_json(path, before, after));
    result.insert("ok".to_string(), Value::Bool(true));
    result.insert("path".to_string(), Value::String(display_path(path)));
    Ok(serde_json::to_string_pretty(&Value::Object(result))?)
}

pub(crate) fn patch_result_json(path: &Path, before: &str, after: &str) -> String {
    unified_diff(&display_path(path), before, after)
}

fn report_patch_preview(progress: &ToolProgress, path: &Path, diff: &str) {
    let Ok(payload) = serde_json::to_string(&json!({
        "path": display_path(path),
        "diff": diff,
    })) else {
        return;
    };
    progress.report(format!("__patch_preview__{payload}"));
}

pub(crate) fn display_path(path: &Path) -> String {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(path)
    };
    if let Ok(cwd) = std::env::current_dir() {
        if let Ok(stripped) = absolute.strip_prefix(cwd) {
            return stripped.display().to_string();
        }
    }
    if let Ok(home) = std::env::var("HOME") {
        let home = PathBuf::from(home);
        if let Ok(stripped) = absolute.strip_prefix(home) {
            return format!("~/{}", stripped.display());
        }
    }
    path.display().to_string()
}

fn unified_diff(path: &str, before: &str, after: &str) -> String {
    let before_lines = split_lines(before);
    let after_lines = split_lines(after);
    let edits = diff_lines(&before_lines, &after_lines);
    let mut output = String::new();
    output.push_str(&format!("--- a/{path}\n"));
    output.push_str(&format!("+++ b/{path}\n"));
    for hunk in diff_hunks(&edits) {
        output.push_str(&format!(
            "@@ -{},{} +{},{} @@\n",
            hunk.old_start, hunk.old_count, hunk.new_start, hunk.new_count
        ));
        for edit in &edits[hunk.start..hunk.end] {
            output.push(edit.marker());
            output.push_str(edit.line());
            output.push('\n');
        }
    }
    output
}

#[derive(Debug, Eq, PartialEq)]
struct DiffHunk {
    start: usize,
    end: usize,
    old_start: usize,
    old_count: usize,
    new_start: usize,
    new_count: usize,
}

fn diff_hunks(edits: &[EditLine<'_>]) -> Vec<DiffHunk> {
    const CONTEXT: usize = 3;
    let mut changed = Vec::new();
    for (index, edit) in edits.iter().enumerate() {
        match edit {
            EditLine::Context(_) => {}
            EditLine::Delete(_) | EditLine::Insert(_) => changed.push(index),
        }
    }
    if changed.is_empty() {
        return Vec::new();
    }

    let mut ranges = Vec::<(usize, usize)>::new();
    for index in changed {
        let start = index.saturating_sub(CONTEXT);
        let end = (index + CONTEXT + 1).min(edits.len());
        if let Some((_, last_end)) = ranges.last_mut() {
            if start <= *last_end {
                *last_end = (*last_end).max(end);
                continue;
            }
        }
        ranges.push((start, end));
    }

    ranges
        .into_iter()
        .map(|(start, end)| {
            let (old_start, new_start) = line_numbers_at(edits, start);
            let (old_count, new_count) = line_counts(&edits[start..end]);
            DiffHunk {
                start,
                end,
                old_start,
                old_count,
                new_start,
                new_count,
            }
        })
        .collect()
}

fn line_numbers_at(edits: &[EditLine<'_>], index: usize) -> (usize, usize) {
    let mut old_line = 1usize;
    let mut new_line = 1usize;
    for edit in &edits[..index] {
        match edit {
            EditLine::Context(_) => {
                old_line += 1;
                new_line += 1;
            }
            EditLine::Delete(_) => old_line += 1,
            EditLine::Insert(_) => new_line += 1,
        }
    }
    (old_line, new_line)
}

fn line_counts(edits: &[EditLine<'_>]) -> (usize, usize) {
    let mut old_count = 0usize;
    let mut new_count = 0usize;
    for edit in edits {
        match edit {
            EditLine::Context(_) => {
                old_count += 1;
                new_count += 1;
            }
            EditLine::Delete(_) => old_count += 1,
            EditLine::Insert(_) => new_count += 1,
        }
    }
    (old_count, new_count)
}

fn split_lines(value: &str) -> Vec<String> {
    if value.is_empty() {
        return Vec::new();
    }
    value
        .lines()
        .map(str::to_string)
        .chain(if value.ends_with('\n') {
            Vec::new()
        } else {
            vec!["\\ No newline at end of file".to_string()]
        })
        .collect()
}

#[derive(Debug, Eq, PartialEq)]
enum EditLine<'a> {
    Context(&'a str),
    Delete(&'a str),
    Insert(&'a str),
}

impl<'a> EditLine<'a> {
    fn marker(&self) -> char {
        match self {
            Self::Context(_) => ' ',
            Self::Delete(_) => '-',
            Self::Insert(_) => '+',
        }
    }

    fn line(&self) -> &'a str {
        match self {
            Self::Context(line) | Self::Delete(line) | Self::Insert(line) => line,
        }
    }
}

fn diff_lines<'a>(before: &'a [String], after: &'a [String]) -> Vec<EditLine<'a>> {
    if before.len().saturating_mul(after.len()) > 250_000 {
        return before
            .iter()
            .map(|line| EditLine::Delete(line.as_str()))
            .chain(after.iter().map(|line| EditLine::Insert(line.as_str())))
            .collect();
    }

    let rows = before.len() + 1;
    let cols = after.len() + 1;
    let mut lcs = vec![0usize; rows * cols];
    for i in (0..before.len()).rev() {
        for j in (0..after.len()).rev() {
            let index = i * cols + j;
            lcs[index] = if before[i] == after[j] {
                lcs[(i + 1) * cols + j + 1] + 1
            } else {
                lcs[(i + 1) * cols + j].max(lcs[i * cols + j + 1])
            };
        }
    }

    let mut edits = Vec::new();
    let mut i = 0;
    let mut j = 0;
    while i < before.len() && j < after.len() {
        if before[i] == after[j] {
            edits.push(EditLine::Context(before[i].as_str()));
            i += 1;
            j += 1;
        } else if lcs[(i + 1) * cols + j] >= lcs[i * cols + j + 1] {
            edits.push(EditLine::Delete(before[i].as_str()));
            i += 1;
        } else {
            edits.push(EditLine::Insert(after[j].as_str()));
            j += 1;
        }
    }
    while i < before.len() {
        edits.push(EditLine::Delete(before[i].as_str()));
        i += 1;
    }
    while j < after.len() {
        edits.push(EditLine::Insert(after[j].as_str()));
        j += 1;
    }
    edits
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn creates_unified_diff() {
        let diff = unified_diff("demo.txt", "one\ntwo\n", "one\nTWO\nthree\n");
        assert!(diff.contains("--- a/demo.txt"));
        assert!(diff.contains("+++ b/demo.txt"));
        assert!(diff.contains("-two"));
        assert!(diff.contains("+TWO"));
        assert!(diff.contains("+three"));
    }

    #[test]
    fn new_file_diff_contains_insertions() {
        let diff = unified_diff("new.txt", "", "alpha\nbeta\n");
        assert!(diff.contains("@@ -1,0 +1,2 @@"));
        assert!(diff.contains("+alpha"));
        assert!(diff.contains("+beta"));
        assert!(!diff.contains("-alpha"));
    }

    #[test]
    fn emptied_file_diff_contains_deletions() {
        let diff = unified_diff("old.txt", "alpha\nbeta\n", "");
        assert!(diff.contains("@@ -1,2 +1,0 @@"));
        assert!(diff.contains("-alpha"));
        assert!(diff.contains("-beta"));
        assert!(!diff.contains("+alpha"));
    }
}
