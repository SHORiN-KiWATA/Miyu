//! 摘要前的分析段：先在 `<analysis>` 里过一遍对话，再写摘要。
//!
//! 对标 Claude Code：让摘要器先按时间顺序把「用户要什么、做了什么、错在哪、
//! 被纠正过什么」列一遍，再落笔写结构化摘要。分析段本身是草稿，既不落库也不
//! 给用户看——这里负责把它从落库文本里剥掉、从流里滤掉。
//!
//! 剥离与过滤必须同一套判定：一个未闭合的 `<analysis>` 在落库侧返回空串（触发
//! 现有的空摘要重试/回退），在流里同样整段丢弃，两边不能各说各话。

use crate::llm::{ChatStreamChunk, ChatStreamKind};
use anyhow::Result;

const OPEN_TAG: &str = "<analysis>";
const CLOSE_TAG: &str = "</analysis>";

/// 摘要输出帽低于这个数就不开分析段：分析要占输出预算，小窗口下开了等于把
/// 摘要本身挤没。
pub(in crate::agent) const ANALYSIS_MIN_CAP: u32 = 6000;

pub(in crate::agent) const ANALYSIS_STEP: &str = "Work in two phases. First, inside an <analysis> block, walk the conversation in order and note for each part what the user asked, what was done, the decisions, the errors and fixes, and every user correction; check the notes for gaps before moving on. Second, after the block, write the summary. The analysis block is discarded and never shown, so the summary must stand on its own.";

pub(in crate::agent) const ANALYSIS_SKIP: &str =
    "Write the summary directly, without an analysis block.";

/// 组装摘要系统提示词。`MIYU_COMPACT_PROMPT_FILE` 换整份底稿、
/// `MIYU_COMPACT_ANALYSIS=0` 强制关分析段，两个都是测具用的实验开关。
pub(in crate::agent) fn compact_system_prompt(base: &str, summary_cap: u32) -> String {
    let base = std::env::var("MIYU_COMPACT_PROMPT_FILE")
        .ok()
        .and_then(|path| std::fs::read_to_string(path).ok())
        .unwrap_or_else(|| base.to_string());
    let forced_off = std::env::var("MIYU_COMPACT_ANALYSIS")
        .map(|value| value.trim() == "0")
        .unwrap_or(false);
    let step = if summary_cap >= ANALYSIS_MIN_CAP && !forced_off {
        ANALYSIS_STEP
    } else {
        ANALYSIS_SKIP
    };
    base.replace("{{ANALYSIS_STEP}}", step)
}

/// 从落库文本里剥掉分析段。
///
/// 只有闭合的段才算草稿；以 `<analysis>` 开头却没闭合，说明模型把输出预算全
/// 花在分析上、摘要根本没写出来——返回空串让上层走重试/机械兜底，比落库一份
/// 只有草稿的「摘要」强。
pub(in crate::agent) fn strip_analysis_block(text: &str) -> String {
    let Some(open) = text.find(OPEN_TAG) else {
        return text.to_string();
    };
    match text[open..].find(CLOSE_TAG) {
        Some(offset) => {
            let close = open + offset + CLOSE_TAG.len();
            format!("{}{}", &text[..open], &text[close..])
                .trim()
                .to_string()
        }
        None if text.trim_start().starts_with(OPEN_TAG) => String::new(),
        None => text.to_string(),
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Mode {
    /// 还没看清开头是不是 `<analysis>`：标签会被切在两个 chunk 中间。
    Undecided,
    Suppressing,
    Passing,
}

/// 流式过滤：分析段不进用户眼里的压缩进度。
pub(in crate::agent) struct AnalysisChunkFilter {
    buffer: String,
    mode: Mode,
}

impl AnalysisChunkFilter {
    pub(in crate::agent) fn new() -> Self {
        Self {
            buffer: String::new(),
            mode: Mode::Undecided,
        }
    }

    pub(in crate::agent) fn push<F>(
        &mut self,
        chunk: ChatStreamChunk,
        on_chunk: &mut F,
    ) -> Result<()>
    where
        F: FnMut(ChatStreamChunk) -> Result<()>,
    {
        if chunk.kind != ChatStreamKind::Content {
            return on_chunk(chunk);
        }
        match self.mode {
            Mode::Passing => on_chunk(chunk),
            Mode::Undecided => {
                self.buffer.push_str(&chunk.text);
                let trimmed = self.buffer.trim_start();
                if trimmed.is_empty() {
                    return Ok(());
                }
                if trimmed.starts_with(OPEN_TAG) {
                    self.mode = Mode::Suppressing;
                    self.buffer = trimmed.to_string();
                    return self.drain_suppressed(on_chunk);
                }
                if OPEN_TAG.starts_with(trimmed) {
                    // 还可能是被切开的开标签，再等一个 chunk。
                    return Ok(());
                }
                self.mode = Mode::Passing;
                let text = std::mem::take(&mut self.buffer);
                on_chunk(ChatStreamChunk {
                    kind: ChatStreamKind::Content,
                    text,
                })
            }
            Mode::Suppressing => {
                self.buffer.push_str(&chunk.text);
                self.drain_suppressed(on_chunk)
            }
        }
    }

    fn drain_suppressed<F>(&mut self, on_chunk: &mut F) -> Result<()>
    where
        F: FnMut(ChatStreamChunk) -> Result<()>,
    {
        let Some(offset) = self.buffer.find(CLOSE_TAG) else {
            return Ok(());
        };
        let rest = self.buffer[offset + CLOSE_TAG.len()..]
            .trim_start()
            .to_string();
        self.buffer.clear();
        self.mode = Mode::Passing;
        if rest.is_empty() {
            return Ok(());
        }
        on_chunk(ChatStreamChunk {
            kind: ChatStreamKind::Content,
            text: rest,
        })
    }

    /// 流结束。仍未决的残留照发（它不是分析段）；未闭合的分析段整段丢弃，
    /// 与 `strip_analysis_block` 的判定一致。
    pub(in crate::agent) fn finish<F>(&mut self, on_chunk: &mut F) -> Result<()>
    where
        F: FnMut(ChatStreamChunk) -> Result<()>,
    {
        if self.mode == Mode::Undecided && !self.buffer.is_empty() {
            let text = std::mem::take(&mut self.buffer);
            self.mode = Mode::Passing;
            return on_chunk(ChatStreamChunk {
                kind: ChatStreamKind::Content,
                text,
            });
        }
        self.buffer.clear();
        Ok(())
    }
}
