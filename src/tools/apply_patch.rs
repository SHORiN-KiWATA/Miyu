use super::{ToolProgress, ToolRegistry, ToolSpec};
use crate::i18n::text as t;
use crate::tools::patch_preview::write_with_patch_preview;
use anyhow::{bail, Result};
use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::os::unix::fs::PermissionsExt;
use std::path::{Component, Path, PathBuf};

/// 三个存储域各一件补丁工具(08-21 二次裁定):`edit`=文件系统、`artifact`=
/// Artifact 库、`kb`=知识库。第一版把三域折进 edit 的路径前缀,实测模型对
/// "存进知识库"想不起 kb: 前缀,反被 remember_fact 的名字劫走——工具名才是
/// 最强的能力广告,域内聚合、域间分名。
pub fn register(registry: &mut ToolRegistry) {
    registry.register(ToolSpec::new_with_progress(
        "edit",
        "Apply a batch patch to files. Prefer this for complex edits and multiple changes in the same file.",
        patch_parameters(),
        move |args, progress| async move {
            progress.report(format!(
                "__tool_phase__~ {}",
                t("prepare patch", "准备修改")
            ));
            tokio::task::yield_now().await;
            edit_filesystem(args, progress)
        },
    ).writes());
}

/// 知识库补丁工具。写入全部路由 KnowledgeBase(元数据/语义索引不绕过)。
pub fn register_kb(
    registry: &mut ToolRegistry,
    config: crate::config::AppConfig,
    paths: crate::paths::MiyuPaths,
) {
    registry.register(ToolSpec::new_with_progress(
        "kb",
        "Write knowledge-base files: create, update, or delete via patch. Paths are knowledge-base relative. The knowledge base holds persistent reference documents, not Miyu's memories (those go through remember_fact). Read entries with read using kb: paths; search with search_knowledge_base.",
        patch_parameters(),
        move |args, progress| {
            let config = config.clone();
            let paths = paths.clone();
            async move {
                progress.report(format!(
                    "__tool_phase__~ {}",
                    t("prepare patch", "准备修改")
                ));
                tokio::task::yield_now().await;
                apply_kb_patch(args, progress, &config, &paths)
            }
        },
    ).writes());
}

/// Artifact 库补丁工具(WebUI 会话注册)。
pub fn register_artifact(registry: &mut ToolRegistry, root: PathBuf, session_id: &str) {
    let session_id = session_id.to_string();
    registry.register(ToolSpec::new_with_progress(
        "artifact",
        "Create or edit deliverable files in the WebUI Artifact workspace via patch. Paths are plain artifact file names. Use for reports, documents, and standalone files the user should inspect; publish an existing local file with present_artifact; read entries with read using artifact: paths.",
        patch_parameters(),
        move |args, progress| {
            let root = root.clone();
            let session_id = session_id.clone();
            async move {
                progress.report(format!(
                    "__tool_phase__~ {}",
                    t("prepare patch", "准备修改")
                ));
                tokio::task::yield_now().await;
                apply_artifact_patch(args, progress, &root, &session_id)
            }
        },
    ).presentation());
}

fn patch_parameters() -> Value {
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
    })
}

fn apply_patch(args: Value, progress: ToolProgress) -> Result<String> {
    apply_patch_with(args, progress, path_arg, false)
}

fn apply_artifact_patch(
    args: Value,
    progress: ToolProgress,
    root: &Path,
    session_id: &str,
) -> Result<String> {
    let session_dir = ensure_artifact_session_dir(root, session_id)?;
    apply_patch_with(
        args,
        progress,
        |value| {
            // artifact: 前缀可写可不写:工具名已选域,前缀只是容错。
            let name = value.strip_prefix("artifact:").unwrap_or(value);
            artifact_patch_path(&session_dir, name)
        },
        true,
    )
}

#[derive(PartialEq)]
enum PatchNamespace {
    Filesystem,
    Artifact,
    KnowledgeBase,
}

/// 扫描补丁头分类命名空间;混用直接拒绝,错误比静默猜测便宜。
fn detect_patch_namespace(patch_text: &str) -> Result<PatchNamespace> {
    let mut namespace: Option<PatchNamespace> = None;
    for line in patch_text.lines() {
        let value = header_value(line, "*** Add File: ")
            .or_else(|| header_value(line, "*** Update File: "))
            .or_else(|| header_value(line, "*** Delete File: "));
        let Some(value) = value else { continue };
        let current = if value.starts_with("artifact:") {
            PatchNamespace::Artifact
        } else if value.starts_with("kb:") {
            PatchNamespace::KnowledgeBase
        } else {
            PatchNamespace::Filesystem
        };
        match &namespace {
            None => namespace = Some(current),
            Some(existing) if *existing == current => {}
            Some(_) => bail!(
                "patch rejected: one patch must target a single namespace (filesystem, artifact:, or kb:)"
            ),
        }
    }
    Ok(namespace.unwrap_or(PatchNamespace::Filesystem))
}

/// edit 只管文件系统;带前缀的补丁给出指路错误而不是静默走错域。
fn edit_filesystem(args: Value, progress: ToolProgress) -> Result<String> {
    let patch_text = args
        .get("patchText")
        .or_else(|| args.get("patch_text"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    match detect_patch_namespace(&patch_text)? {
        PatchNamespace::Filesystem => apply_patch(args, progress),
        PatchNamespace::Artifact => {
            bail!("artifact files are edited with the `artifact` tool; edit only touches the filesystem")
        }
        PatchNamespace::KnowledgeBase => {
            bail!("knowledge-base files are edited with the `kb` tool; edit only touches the filesystem")
        }
    }
}

/// kb: 命名空间:解析与预检复用补丁引擎(读的是知识库里的真实文件),写入
/// 全部路由回 KnowledgeBase——直接 fs 写会绕过元数据库与语义索引。
fn apply_kb_patch(
    args: Value,
    progress: ToolProgress,
    config: &crate::config::AppConfig,
    paths: &crate::paths::MiyuPaths,
) -> Result<String> {
    if !config.plugins.knowledge_base.enabled {
        bail!("knowledge base plugin is disabled");
    }
    let kb = crate::tools::knowledge_base::KnowledgeBase::new(config.clone(), paths.clone())?;
    kb.init()?;
    let patch_text = args
        .get("patchText")
        .or_else(|| args.get("patch_text"))
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("patchText is required"))?;
    let rel_by_path = std::cell::RefCell::new(HashMap::<PathBuf, String>::new());
    let operations = parse_patch_with(patch_text, &|value: &str| {
        let rel = value.strip_prefix("kb:").unwrap_or(value);
        let path = kb.safe_file_path(rel)?;
        rel_by_path
            .borrow_mut()
            .insert(path.clone(), rel.to_string());
        Ok(path)
    })?;
    if operations.is_empty() {
        bail!("patch rejected: empty patch")
    }
    let changes = preflight_operations(operations)?;
    let rel_by_path = rel_by_path.into_inner();
    let mut files = Vec::new();
    for change in changes {
        let rel = rel_by_path
            .get(&change.path)
            .cloned()
            .unwrap_or_else(|| change.path.display().to_string());
        match change.kind {
            ChangeKind::Delete => {
                kb.remove(&rel)?;
                report_delete_preview(&progress, &change.path, &change.before)?;
            }
            ChangeKind::Add | ChangeKind::Update => {
                // 旧 upload 工具的内容守卫(技能/人格/记忆类内容不进知识库)
                // 原样保留,别因换了入口就放开。
                crate::tools::knowledge_base::reject_non_kb_upload(&change.after, "", &rel)?;
                let temp = tempfile::NamedTempFile::new()?;
                std::fs::write(temp.path(), change.after.as_bytes())?;
                kb.import_file(temp.path(), &rel)?;
                let diff = crate::tools::patch_preview::patch_result_json(
                    &change.path,
                    &change.before,
                    &change.after,
                );
                let payload = serde_json::to_string(&json!({
                    "path": format!("kb:{rel}"),
                    "diff": diff,
                }))?;
                progress.report(format!("__patch_preview__{payload}"));
            }
        }
        files.push(json!({
            "path": format!("kb:{rel}"),
            "operation": change.kind.as_str(),
        }));
    }
    kb.spawn_embedding_reindex()?;
    Ok(serde_json::to_string_pretty(&json!({
        "ok": true,
        "operation": "kb",
        "files_changed": files.len(),
        "files": files,
    }))?)
}

fn apply_patch_with<F>(
    args: Value,
    progress: ToolProgress,
    resolve_path: F,
    managed_artifact: bool,
) -> Result<String>
where
    F: Fn(&str) -> Result<PathBuf>,
{
    let patch_text = args
        .get("patchText")
        .or_else(|| args.get("patch_text"))
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("patchText is required"))?;
    let operations = parse_patch_with(patch_text, &resolve_path)?;
    if operations.is_empty() {
        bail!("patch rejected: empty patch")
    }
    let changes = preflight_operations(operations)?;
    if managed_artifact {
        for change in &changes {
            if change.after.len() > super::artifact::MAX_ARTIFACT_BYTES {
                bail!(
                    "Artifact exceeds the {} byte limit: {}",
                    super::artifact::MAX_ARTIFACT_BYTES,
                    change.path.display()
                )
            }
        }
    }

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
                if managed_artifact {
                    std::fs::set_permissions(&change.path, std::fs::Permissions::from_mode(0o600))?;
                    progress.report_artifact(change.path.clone(), String::new());
                }
            }
        }
        let reported_path = if managed_artifact {
            change
                .path
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or_default()
                .to_string()
        } else {
            display_path_for_progress(&change.path)
        };
        files.push(json!({
            "path": reported_path,
            "operation": change.kind.as_str(),
        }));
    }

    Ok(serde_json::to_string_pretty(&json!({
        "ok": true,
        "operation": if managed_artifact { "apply_artifact_patch" } else { "apply_patch" },
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
    parse_patch_with(raw, &path_arg)
}

fn parse_patch_with<F>(raw: &str, resolve_path: &F) -> Result<Vec<Operation>>
where
    F: Fn(&str) -> Result<PathBuf>,
{
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
                path: resolve_path(path)?,
                lines: content,
            });
        } else if let Some(path) = header_value(line, "*** Delete File:") {
            operations.push(Operation::Delete {
                path: resolve_path(path)?,
            });
            index += 1;
        } else if let Some(path) = header_value(line, "*** Update File:") {
            index += 1;
            let mut move_to = None;
            if index < end {
                if let Some(target) = header_value(lines[index], "*** Move to:") {
                    move_to = Some(resolve_path(target)?);
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
                    // hunk 内容样的行(以 ' '/'-'/'+' 开头)出现在 @@ 之外,
                    // 多半是漏写了 @@ 头:静默跳过等于把整块修改丢掉。
                    let stray = lines[index];
                    if stray.starts_with(' ') || stray.starts_with('-') || stray.starts_with('+') {
                        bail!(
                            "apply_patch verification failed: hunk line outside a @@ hunk (missing @@ header?): {stray}"
                        )
                    }
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
                path: resolve_path(path)?,
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
                // CRLF 文件的匹配靠 TrimEnd 模式成功,但替换进来的新行是
                // LF:统一按 LF 应用,写回前还原原文件的行尾风格,避免混合。
                let crlf = before.contains("\r\n");
                let mut after = if crlf {
                    before.replace("\r\n", "\n")
                } else {
                    before.clone()
                };
                for hunk in hunks {
                    after = apply_hunk(&path, &after, &hunk)?;
                }
                if crlf {
                    after = after.replace('\n', "\r\n");
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
    // 无 @@ 锚点的 hunk 在文件中多处匹配时,首命中会落到错误位置还返回
    // ok:与 fallback 子串路径的 count>1 拒绝对齐。带锚点/EOF 的视为已消歧。
    let ambiguous = |found: usize, pattern: &[String]| {
        hunk.context.is_none()
            && !hunk.end_of_file
            && seek_sequence(&current_lines, pattern, found + 1, false).is_some()
    };
    if let Some(found) = seek_sequence(&current_lines, &pattern, start_index, hunk.end_of_file) {
        if ambiguous(found, &pattern) {
            bail!(
                "apply_patch verification failed: hunk matches multiple locations in {}; add a @@ context anchor",
                path.display()
            )
        }
        let mut result = current_lines.clone();
        result.splice(found..found + pattern.len(), new_lines);
        return Ok(join_lines(result));
    }
    if pattern.last().is_some_and(|line| line.is_empty()) {
        pattern.pop();
        if let Some(found) = seek_sequence(&current_lines, &pattern, start_index, hunk.end_of_file)
        {
            if ambiguous(found, &pattern) {
                bail!(
                    "apply_patch verification failed: hunk matches multiple locations in {}; add a @@ context anchor",
                    path.display()
                )
            }
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
    let path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        super::workspace::effective_workdir().join(path)
    };
    super::sandbox::guard_write(&path)?;
    Ok(path)
}

fn ensure_artifact_session_dir(root: &Path, session_id: &str) -> Result<PathBuf> {
    validate_single_component(session_id, "session id")?;
    ensure_private_directory(root)?;
    let session_dir = root.join(session_id);
    ensure_private_directory(&session_dir)?;
    Ok(session_dir)
}

fn ensure_private_directory(path: &Path) -> Result<()> {
    if let Ok(metadata) = std::fs::symlink_metadata(path) {
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            bail!(
                "Artifact workspace path is not a directory: {}",
                path.display()
            );
        }
    } else {
        std::fs::create_dir_all(path)?;
    }
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
    Ok(())
}

fn artifact_patch_path(session_dir: &Path, value: &str) -> Result<PathBuf> {
    let value = value.trim();
    validate_single_component(value, "Artifact file name")?;
    if value.chars().count() > 180 || value.chars().any(char::is_control) {
        bail!("Artifact file name is invalid")
    }
    let path = session_dir.join(value);
    if let Ok(metadata) = std::fs::symlink_metadata(&path) {
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            bail!("Artifact patch target is not a regular file: {value}")
        }
    }
    Ok(path)
}

fn validate_single_component(value: &str, label: &str) -> Result<()> {
    let mut components = Path::new(value).components();
    if !matches!(components.next(), Some(Component::Normal(_))) || components.next().is_some() {
        bail!("{label} must be a single safe path component")
    }
    Ok(())
}

fn display_path_for_progress(path: &Path) -> String {
    crate::tools::patch_preview::display_path(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// kb 补丁三操作全链路:写入/更新/删除都必须落到 KnowledgeBase
    /// (kb_meta.db 有行、文件在 kb 根下),而不是裸 fs 写。
    #[tokio::test]
    async fn kb_patch_routes_writes_through_the_knowledge_base() {
        let temp = tempfile::tempdir().unwrap();
        let paths = crate::tools::tests::test_paths(temp.path());
        let config = crate::config::AppConfig::default();
        let kb = crate::tools::knowledge_base::KnowledgeBase::new(config.clone(), paths.clone())
            .unwrap();
        kb.init().unwrap();

        let add = "*** Begin Patch\n*** Add File: notes/demo.md\n+# demo\n+hello kb\n*** End Patch";
        let output = apply_kb_patch(
            json!({ "patchText": add }),
            ToolProgress::default(),
            &config,
            &paths,
        )
        .unwrap();
        assert!(output.contains("kb:notes/demo.md"), "{output}");
        let stored = kb.safe_file_path("notes/demo.md").unwrap();
        assert_eq!(
            std::fs::read_to_string(&stored).unwrap(),
            "# demo\nhello kb\n"
        );

        let update = "*** Begin Patch\n*** Update File: notes/demo.md\n@@ # demo\n-hello kb\n+hello again\n*** End Patch";
        apply_kb_patch(
            json!({ "patchText": update }),
            ToolProgress::default(),
            &config,
            &paths,
        )
        .unwrap();
        assert!(std::fs::read_to_string(&stored)
            .unwrap()
            .contains("hello again"));

        let delete = "*** Begin Patch\n*** Delete File: notes/demo.md\n*** End Patch";
        apply_kb_patch(
            json!({ "patchText": delete }),
            ToolProgress::default(),
            &config,
            &paths,
        )
        .unwrap();
        assert!(!stored.exists());
    }

    /// edit 收到带域前缀的补丁必须指路而不是走错域。
    #[test]
    fn edit_rejects_prefixed_patches_with_a_pointer() {
        let patch = "*** Begin Patch\n*** Add File: kb:notes/x.md\n+x\n*** End Patch";
        let error = edit_filesystem(json!({ "patchText": patch }), ToolProgress::default())
            .unwrap_err()
            .to_string();
        assert!(error.contains("`kb` tool"), "{error}");
        let patch = "*** Begin Patch\n*** Add File: artifact:r.md\n+x\n*** End Patch";
        let error = edit_filesystem(json!({ "patchText": patch }), ToolProgress::default())
            .unwrap_err()
            .to_string();
        assert!(error.contains("`artifact` tool"), "{error}");
    }

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

    #[test]
    fn artifact_patch_adds_and_updates_managed_files() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("artifacts");
        let session_dir = root.join("sess_test");
        std::fs::create_dir_all(&session_dir).unwrap();
        let report = session_dir.join("report.md");
        std::fs::write(&report, "# Report\n\nOld text.\n").unwrap();
        let patch = "*** Begin Patch\n*** Update File: report.md\n@@ report\n # Report\n \n-Old text.\n+Updated text.\n*** Add File: notes.txt\n+Follow up.\n*** End Patch";
        let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();

        let output = apply_artifact_patch(
            json!({"patchText": patch}),
            ToolProgress::new(sender),
            &root,
            "sess_test",
        )
        .unwrap();
        let payload: Value = serde_json::from_str(&output).unwrap();

        assert_eq!(payload["operation"], "apply_artifact_patch");
        assert_eq!(payload["files_changed"], 2);
        assert_eq!(payload["files"][0]["path"], "report.md");
        assert!(!output.contains(temp.path().to_string_lossy().as_ref()));
        assert_eq!(
            std::fs::read_to_string(&report).unwrap(),
            "# Report\n\nUpdated text.\n"
        );
        let notes = session_dir.join("notes.txt");
        assert_eq!(std::fs::read_to_string(&notes).unwrap(), "Follow up.\n");
        for path in [&report, &notes] {
            assert_eq!(
                std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        let artifacts = std::iter::from_fn(|| receiver.try_recv().ok())
            .filter_map(|event| match event {
                super::super::ToolProgressEvent::Artifact { path, .. } => Some(path),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(artifacts, [report, notes]);
    }

    #[test]
    fn artifact_patch_rejects_unsafe_paths_and_symlinks_but_allows_delete() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("artifacts");
        let session_dir = root.join("sess_test");
        std::fs::create_dir_all(&session_dir).unwrap();
        let report = session_dir.join("report.md");
        std::fs::write(&report, "original\n").unwrap();
        let outside = temp.path().join("outside.md");
        std::fs::write(&outside, "outside\n").unwrap();
        symlink(&outside, session_dir.join("link.md")).unwrap();

        for patch in [
            "*** Begin Patch\n*** Add File: ../escape.md\n+bad\n*** End Patch",
            "*** Begin Patch\n*** Add File: nested/file.md\n+bad\n*** End Patch",
            "*** Begin Patch\n*** Update File: link.md\n@@ patch\n-outside\n+changed\n*** End Patch",
            "*** Begin Patch\n*** Delete File: link.md\n*** End Patch",
        ] {
            assert!(apply_artifact_patch(
                json!({"patchText": patch}),
                ToolProgress::default(),
                &root,
                "sess_test",
            )
            .is_err());
        }
        assert_eq!(std::fs::read_to_string(&report).unwrap(), "original\n");
        assert_eq!(std::fs::read_to_string(outside).unwrap(), "outside\n");
        assert!(!temp.path().join("escape.md").exists());

        // 验收四轮:Artifact 与本地补丁同语义,普通文件的 Delete File 放行。
        let deleted = apply_artifact_patch(
            json!({"patchText": "*** Begin Patch\n*** Delete File: report.md\n*** End Patch"}),
            ToolProgress::default(),
            &root,
            "sess_test",
        );
        assert!(deleted.is_ok(), "{deleted:?}");
        assert!(!report.exists());
    }
}
