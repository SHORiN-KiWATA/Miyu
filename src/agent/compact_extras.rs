//! 压缩后的重建材料：回灌最近读过的文件正文 + 折叠原文的转录路径。
//!
//! 摘要再好也是有损的。Claude Code 压完不失忆靠的不是更聪明的摘要，而是压完
//! 立刻把最近读过的几个文件从盘上重读一遍塞回上下文，并且把折叠掉的原始对话
//! 写成一份磁盘转录、告诉模型路径——摘要没写到的细节，它自己 read 回来。
//!
//! 这里产出的东西存进摘要行的 `compact_extras` 列，每次请求跟在
//! `<conversation-checkpoint>` 后面重新渲染。渲染模板是代码常量，同一份
//! extras 渲染多少次都是同样的字节（缓存契约 AGENTS §1.1）。
//!
//! 转录文件与 spill 一样只写不清：删会话不删它，备份不含它。磁盘而已。

use crate::agent::overflow::estimate_tokens;
use crate::agent::tool_report::{replay_rounds, tool_call_paths, PathAccess};
use crate::state::Turn;
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// 单条转录里每段正文的截断长度（工具输出与推理最容易失控）。
const TRANSCRIPT_ITEM_MAX_CHARS: usize = 4000;
/// 转录链最多回溯几份。再往前的折叠已被摘要吸收过两轮以上。
const MAX_TRANSCRIPT_CHAIN: usize = 5;
/// 预判上限：文件字节数超过 `cap_tokens × 此值` 直接判超限，不读全文。
const BYTES_PER_TOKEN_GUESS: usize = 8;
/// 单行截断，与 read 工具一致，免得一行 minified JS 顶掉整个预算。
const MAX_LINE_CHARS: usize = 2000;

#[derive(Debug, Clone)]
pub(in crate::agent) struct CompactExtrasPolicy {
    /// 回灌几个文件。0 = 关。
    pub restore_files: usize,
    /// 单文件 token 上限，超了只留路径。
    pub restore_file_tokens: usize,
    /// 本次回灌的总 token 预算。
    pub restore_total_tokens: usize,
    pub export_transcript: bool,
    /// 决定提示语要不要提 read 工具：没这工具时提了就是空头支票。
    pub read_tool_available: bool,
    /// 转录落盘目录：`state_dir/compact/<session>`。
    pub transcript_dir: PathBuf,
    /// MIYU 根目录。人格、配置、记忆库不回灌——它们已由别的通路进上下文。
    pub exclude_root: Option<PathBuf>,
    /// 相对路径的解析基准。
    pub workdir: PathBuf,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub(in crate::agent) struct CompactExtras {
    /// 折叠转录路径，最新在前，最多 `MAX_TRANSCRIPT_CHAIN` 条。
    #[serde(default)]
    pub transcripts: Vec<String>,
    #[serde(default)]
    pub restored: Vec<RestoredFile>,
    /// 压缩时冻结：之后工具集变了也不改提示语，否则这块每请求重渲染会
    /// 逐字节漂移，把 checkpoint 之后的缓存全打掉。
    #[serde(default)]
    pub read_hint: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(in crate::agent) struct RestoredFile {
    pub path: String,
    pub status: RestoreStatus,
    #[serde(default)]
    pub lines: usize,
    /// `"N: line"` 格式，与 read 工具输出同形，模型读起来无需换脑子。
    #[serde(default)]
    pub content: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(in crate::agent) enum RestoreStatus {
    Included,
    TooLarge,
    OverBudget,
}

/// 路径片段消毒：只留 `[A-Za-z0-9._-]`，取前 48 位。会话 id 与 turn id 都由
/// 别处生成，进文件名前一律过这里。
pub(in crate::agent) fn safe_path_segment(raw: &str) -> String {
    raw.chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-') {
                ch
            } else {
                '_'
            }
        })
        .take(48)
        .collect()
}

impl CompactExtras {
    pub(in crate::agent) fn is_empty(&self) -> bool {
        self.transcripts.is_empty() && self.restored.is_empty()
    }

    pub(in crate::agent) fn included_files(&self) -> usize {
        self.restored
            .iter()
            .filter(|file| file.status == RestoreStatus::Included)
            .count()
    }

    /// 模型可见文本：英文短句，全部由常量拼成，同一份 extras 渲染即同字节。
    pub(in crate::agent) fn render(&self) -> String {
        let mut blocks: Vec<String> = Vec::new();
        if !self.restored.is_empty() {
            blocks.push(self.render_restored());
        }
        if !self.transcripts.is_empty() {
            blocks.push(self.render_transcripts());
        }
        blocks.join("\n")
    }

    fn render_restored(&self) -> String {
        let mut out = String::from("<restored-files>\n");
        out.push_str("Current on-disk contents of files touched before the checkpoint, re-read after compaction.");
        if self
            .restored
            .iter()
            .any(|file| file.status != RestoreStatus::Included)
        {
            out.push_str(" Entries without content were too large or over budget");
            if self.read_hint {
                out.push_str("; the read tool returns them on demand.");
            } else {
                out.push('.');
            }
        }
        out.push('\n');
        for file in &self.restored {
            let path = escape_attribute(&file.path);
            match file.status {
                RestoreStatus::Included => {
                    out.push_str(&format!(
                        "<file path=\"{path}\" lines=\"{}\">\n",
                        file.lines
                    ));
                    out.push_str(&file.content);
                    if !file.content.ends_with('\n') {
                        out.push('\n');
                    }
                    out.push_str("</file>\n");
                }
                RestoreStatus::TooLarge => {
                    out.push_str(&format!("<file path=\"{path}\" status=\"too large\"/>\n"));
                }
                RestoreStatus::OverBudget => {
                    out.push_str(&format!("<file path=\"{path}\" status=\"over budget\"/>\n"));
                }
            }
        }
        out.push_str("</restored-files>\n");
        out
    }

    fn render_transcripts(&self) -> String {
        let mut out = String::from("<compact-transcript>\n");
        out.push_str(&format!(
            "The folded turns are saved verbatim at {}.",
            self.transcripts[0]
        ));
        if self.transcripts.len() > 1 {
            out.push_str(&format!(
                " Earlier folds: {}.",
                self.transcripts[1..].join(", ")
            ));
        }
        if self.read_hint {
            out.push_str(" The read tool returns them when the summary lacks a detail.");
        }
        out.push('\n');
        out.push_str("</compact-transcript>\n");
        out
    }
}

fn escape_attribute(value: &str) -> String {
    value.replace('&', "&amp;").replace('"', "&quot;")
}

/// `~/` 展开、绝对路径原样、相对路径挂 workdir。不 canonicalize：符号链接下
/// 解析出的真身路径和模型看到的路径对不上，反而更难认。
fn resolve_path(raw: &str, workdir: &Path) -> PathBuf {
    let raw = raw.trim();
    if let Some(rest) = raw.strip_prefix("~/") {
        if let Some(home) = directories::BaseDirs::new().map(|dirs| dirs.home_dir().to_path_buf()) {
            return home.join(rest);
        }
    }
    let path = Path::new(raw);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        workdir.join(path)
    }
}

/// 这些回合里碰过的文件，**最近碰过的排在前面**，按解析后的路径去重。
/// `reads_only` 只认读类工具（尾巴的跳过集用：写过不等于正文在模型手里）。
pub(in crate::agent) fn touched_files(
    turns: &[&Turn],
    workdir: &Path,
    reads_only: bool,
) -> Vec<PathBuf> {
    let mut seen: HashSet<PathBuf> = HashSet::new();
    let mut ordered: Vec<PathBuf> = Vec::new();
    for turn in turns.iter().rev() {
        let rounds = replay_rounds(&turn.tool_flow);
        for round in rounds.iter().rev() {
            for call in round.calls.iter().rev() {
                for (access, raw) in tool_call_paths(&call.name, &call.arguments) {
                    if reads_only && access != PathAccess::Read {
                        continue;
                    }
                    let path = resolve_path(&raw, workdir);
                    if seen.insert(path.clone()) {
                        ordered.push(path);
                    }
                }
            }
        }
    }
    ordered
}

enum FileRead {
    TooLarge,
    Text {
        content: String,
        lines: usize,
        tokens: usize,
    },
}

/// 存在、是普通文件、不是二进制。三条都过才占一个回灌名额——一个已被删掉的
/// 路径不该顶掉后面还在的文件。
fn is_restorable_file(path: &Path) -> bool {
    let Ok(metadata) = std::fs::metadata(path) else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    let Ok(mut file) = std::fs::File::open(path) else {
        return false;
    };
    use std::io::Read;
    let mut buffer = [0u8; 8192];
    let Ok(read) = file.read(&mut buffer) else {
        return false;
    };
    !buffer[..read].contains(&0)
}

/// 读文件并渲染成 `"N: line"`。先按字节数预判，避免为了发现「太大」而把一个
/// 几百兆的文件读进内存。
fn read_for_restore(path: &Path, cap_tokens: usize) -> Option<FileRead> {
    let metadata = std::fs::metadata(path).ok()?;
    if metadata.len() as usize > cap_tokens.saturating_mul(BYTES_PER_TOKEN_GUESS) {
        return Some(FileRead::TooLarge);
    }
    let raw = std::fs::read_to_string(path).ok()?;
    let mut rendered = String::new();
    let mut lines = 0usize;
    for (index, line) in raw.lines().enumerate() {
        lines = index + 1;
        let line = if line.chars().count() > MAX_LINE_CHARS {
            format!(
                "{}... (line truncated to {MAX_LINE_CHARS} chars)",
                line.chars().take(MAX_LINE_CHARS).collect::<String>()
            )
        } else {
            line.to_string()
        };
        rendered.push_str(&format!("{lines}: {line}\n"));
    }
    let tokens = estimate_tokens(&rendered);
    if tokens > cap_tokens {
        return Some(FileRead::TooLarge);
    }
    Some(FileRead::Text {
        content: rendered,
        lines,
        tokens,
    })
}

/// 按候选顺序回灌，直到名额或预算用完。超限项只留路径但**照样占名额**——
/// 模型该知道这个文件曾在工作集里，哪怕正文没跟来。
fn restore_files(
    candidates: &[PathBuf],
    skip: &HashSet<PathBuf>,
    policy: &CompactExtrasPolicy,
) -> Vec<RestoredFile> {
    let mut restored = Vec::new();
    let mut total = 0usize;
    for path in candidates {
        if restored.len() >= policy.restore_files {
            break;
        }
        if skip.contains(path) {
            continue;
        }
        if let Some(root) = policy.exclude_root.as_ref() {
            if path.starts_with(root) {
                continue;
            }
        }
        if !is_restorable_file(path) {
            continue;
        }
        let display = path.display().to_string();
        match read_for_restore(path, policy.restore_file_tokens) {
            None => continue,
            Some(FileRead::TooLarge) => restored.push(RestoredFile {
                path: display,
                status: RestoreStatus::TooLarge,
                lines: 0,
                content: String::new(),
            }),
            Some(FileRead::Text {
                content,
                lines,
                tokens,
            }) => {
                if total.saturating_add(tokens) > policy.restore_total_tokens {
                    restored.push(RestoredFile {
                        path: display,
                        status: RestoreStatus::OverBudget,
                        lines,
                        content: String::new(),
                    });
                } else {
                    total = total.saturating_add(tokens);
                    restored.push(RestoredFile {
                        path: display,
                        status: RestoreStatus::Included,
                        lines,
                        content,
                    });
                }
            }
        }
    }
    restored
}

fn truncate_item(text: &str) -> std::borrow::Cow<'_, str> {
    if text.len() <= TRANSCRIPT_ITEM_MAX_CHARS {
        return std::borrow::Cow::Borrowed(text);
    }
    let mut end = TRANSCRIPT_ITEM_MAX_CHARS;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    std::borrow::Cow::Owned(format!(
        "{}\n[... {} more bytes truncated ...]",
        &text[..end],
        text.len() - end
    ))
}

/// 折叠掉的对话原文，Markdown。被本次压缩取代的上一份摘要也写进去：它是这
/// 段历史的唯一其他载体，丢了就真没了。
fn render_transcript(session_id: &str, fold: &[&Turn], previous_summary: Option<&str>) -> String {
    let mut out = String::from("# Folded conversation transcript\n\n");
    out.push_str(&format!("Session: {session_id}\n"));
    out.push_str(&format!("Folded turns: {}\n", fold.len()));
    out.push_str(&format!("Written: {}\n", chrono::Utc::now().to_rfc3339()));
    if let Some(previous) = previous_summary.map(str::trim).filter(|s| !s.is_empty()) {
        out.push_str("\n## Previous checkpoint summary\n\n");
        out.push_str(previous);
        out.push('\n');
    }
    for turn in fold {
        out.push_str(&format!(
            "\n## Turn {} · {}\n\n### User\n{}\n",
            turn.seq, turn.user_timestamp, turn.user_content
        ));
        for exchange in &turn.question_exchanges {
            out.push_str(&format!(
                "\n#### Clarification\nAssistant: {}\nUser: {}\n",
                crate::question::assistant_exchange_text(exchange),
                crate::question::user_exchange_text(exchange),
            ));
        }
        for followup in &turn.followups {
            if let Some(content) = followup
                .preceding_assistant_content
                .as_deref()
                .map(str::trim)
                .filter(|text| !text.is_empty())
            {
                out.push_str(&format!("\n#### Assistant (interim)\n{content}\n"));
            }
            out.push_str(&format!("\n#### User (follow-up)\n{}\n", followup.content));
        }
        out.push_str(&format!("\n### Assistant\n{}\n", turn.assistant_content));
        if let Some(reasoning) = turn
            .assistant_reasoning
            .as_deref()
            .map(str::trim)
            .filter(|text| !text.is_empty())
        {
            out.push_str(&format!("\n### Reasoning\n{}\n", truncate_item(reasoning)));
        }
        let rounds = replay_rounds(&turn.tool_flow);
        if !rounds.is_empty() {
            out.push_str("\n### Tool calls\n");
            for round in rounds {
                if !round.assistant_content.trim().is_empty() {
                    out.push_str(&format!("\n{}\n", round.assistant_content));
                }
                for call in &round.calls {
                    out.push_str(&format!("\n#### {} {}\n", call.name, call.arguments));
                    out.push_str("```text\n");
                    out.push_str(&truncate_item(&call.output));
                    out.push_str("\n```\n");
                }
            }
        }
        if !turn.tool_reports.is_empty() {
            out.push_str("\n### Tool reports\n");
            for report in &turn.tool_reports {
                out.push_str(&format!("\n{}\n", truncate_item(report)));
            }
        }
    }
    out
}

/// 写 `transcript_dir/fold-<unix_ms>.md`；同毫秒撞名就加 `-2`、`-3`。
fn export_transcript(
    policy: &CompactExtrasPolicy,
    session_id: &str,
    fold: &[&Turn],
    previous_summary: Option<&str>,
) -> Result<PathBuf> {
    std::fs::create_dir_all(&policy.transcript_dir)?;
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or(0);
    let mut path = policy.transcript_dir.join(format!("fold-{stamp}.md"));
    let mut suffix = 2;
    while path.exists() && suffix < 100 {
        path = policy
            .transcript_dir
            .join(format!("fold-{stamp}-{suffix}.md"));
        suffix += 1;
    }
    std::fs::write(&path, render_transcript(session_id, fold, previous_summary))?;
    Ok(path)
}

/// 折叠区与尾巴各自落库的 footprint(`turns.tool_footprint` 合并值)。
///
/// 回放视图 `replay_rounds` 按契约过滤 remote 轮,中转线(claude-code /
/// codex / agy)碰过的文件只在 footprint 里有记录;直连线的路径两边都有,
/// 按解析后路径去重。
#[derive(Default)]
pub(in crate::agent) struct FoldFootprints {
    pub fold: crate::state::ToolFootprint,
    pub tail: crate::state::ToolFootprint,
}

pub(in crate::agent) fn build_compact_extras(
    policy: &CompactExtrasPolicy,
    session_id: &str,
    fold: &[&Turn],
    tail: &[&Turn],
    footprints: &FoldFootprints,
    previous: Option<&CompactExtras>,
    previous_summary: Option<&str>,
) -> CompactExtras {
    let mut extras = CompactExtras {
        read_hint: policy.read_tool_available,
        ..Default::default()
    };

    if policy.restore_files > 0 && policy.restore_file_tokens > 0 && policy.restore_total_tokens > 0
    {
        // 回放里的路径带近因顺序,排前面;footprint 只是集合,补在后面。
        let mut candidates = touched_files(fold, &policy.workdir, false);
        let mut seen: HashSet<PathBuf> = candidates.iter().cloned().collect();
        for raw in footprints.fold.modified.iter().chain(&footprints.fold.read) {
            let path = resolve_path(raw, &policy.workdir);
            if seen.insert(path.clone()) {
                candidates.push(path);
            }
        }
        // 尾巴逐字保留，它读过的文件正文已经在模型手里，再灌一遍是纯浪费。
        let mut skip: HashSet<PathBuf> = touched_files(tail, &policy.workdir, true)
            .into_iter()
            .collect();
        skip.extend(
            footprints
                .tail
                .read
                .iter()
                .map(|raw| resolve_path(raw, &policy.workdir)),
        );
        extras.restored = restore_files(&candidates, &skip, policy);
    }

    // 导出失败不能让压缩失败：转录是加分项，摘要才是主线。
    extras.transcripts = previous
        .map(|previous| previous.transcripts.clone())
        .unwrap_or_default();
    if policy.export_transcript {
        match export_transcript(policy, session_id, fold, previous_summary) {
            Ok(path) => extras.transcripts.insert(0, path.display().to_string()),
            Err(error) => tracing::warn!(
                %error,
                dir = %policy.transcript_dir.display(),
                "folded transcript export failed; compaction continues without it"
            ),
        }
    }
    extras.transcripts.truncate(MAX_TRANSCRIPT_CHAIN);
    extras
}
