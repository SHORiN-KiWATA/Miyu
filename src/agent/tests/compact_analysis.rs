//! 分析段：落库剥离与流式过滤必须是同一套判定。

use crate::agent::compact_analysis::*;
use crate::llm::{ChatStreamChunk, ChatStreamKind};
use crate::prompts::COMPACT_SYSTEM_PROMPT;

fn content(text: &str) -> ChatStreamChunk {
    ChatStreamChunk {
        kind: ChatStreamKind::Content,
        text: text.to_string(),
    }
}

fn run(chunks: &[&str]) -> String {
    let mut filter = AnalysisChunkFilter::new();
    let mut seen = String::new();
    {
        let mut sink = |chunk: ChatStreamChunk| {
            seen.push_str(&chunk.text);
            Ok(())
        };
        for chunk in chunks {
            filter.push(content(chunk), &mut sink).unwrap();
        }
        filter.finish(&mut sink).unwrap();
    }
    seen
}

#[test]
fn strip_analysis_block_removes_a_closed_block() {
    let text = "<analysis>\nnotes about the conversation\n</analysis>\n\n## Standing Facts\n- a";
    assert_eq!(
        strip_analysis_block(text),
        "## Standing Facts\n- a",
        "the draft never reaches storage"
    );
}

#[test]
fn strip_analysis_block_returns_empty_for_an_unclosed_leading_block() {
    let text = "<analysis>\nthe model spent the whole budget here";
    assert_eq!(
        strip_analysis_block(text),
        "",
        "an unfinished draft is not a summary; the empty result triggers the retry"
    );
}

#[test]
fn strip_analysis_block_leaves_text_without_tags() {
    let text = "## Standing Facts\n- port 7043";
    assert_eq!(strip_analysis_block(text), text);
}

#[test]
fn strip_analysis_block_keeps_a_summary_that_merely_mentions_the_tag() {
    let text = "## Notes\nthe prompt asks for an <analysis> block first";
    assert_eq!(strip_analysis_block(text), text);
}

#[test]
fn analysis_filter_hides_the_block_even_when_tags_split_across_chunks() {
    assert_eq!(
        run(&["<ana", "lysis>think…", "…</analy", "sis>\n## Standing"]),
        "## Standing"
    );
}

#[test]
fn analysis_filter_passes_untagged_output_through() {
    assert_eq!(
        run(&["## Standing", " Facts\n- a"]),
        "## Standing Facts\n- a"
    );
}

#[test]
fn analysis_filter_drops_an_unclosed_block_at_finish() {
    assert_eq!(run(&["<analysis>", "still thinking"]), "");
}

#[test]
fn analysis_filter_flushes_an_undecided_remainder_at_finish() {
    // "<" 是开标签的合法前缀，流到此为止就得原样发出去。
    assert_eq!(run(&["<"]), "<");
}

#[test]
fn analysis_filter_forwards_non_content_kinds_untouched() {
    let mut filter = AnalysisChunkFilter::new();
    let mut kinds = Vec::new();
    let mut sink = |chunk: ChatStreamChunk| {
        kinds.push(chunk.kind);
        Ok(())
    };
    filter
        .push(
            ChatStreamChunk {
                kind: ChatStreamKind::Reasoning,
                text: "<analysis>".to_string(),
            },
            &mut sink,
        )
        .unwrap();
    assert_eq!(kinds, vec![ChatStreamKind::Reasoning]);
}

/// 分析段默认关(09-09 实况事故后改的):它让模型先写一份被流式过滤器整段
/// 吞掉的草稿,输出量翻倍、屏幕上几分钟一片空白,而实测召回没提升。
/// 环境变量是进程级的,这条用例不改它——只断言默认行为。
#[test]
fn analysis_is_off_by_default_regardless_of_cap() {
    for cap in [3000, 8000, 16384] {
        let prompt = compact_system_prompt(COMPACT_SYSTEM_PROMPT, cap);
        assert!(
            prompt.contains("without an analysis block"),
            "cap {cap} 默认就该关分析段: {prompt}"
        );
        assert!(!prompt.contains("Work in two phases"), "cap {cap}");
    }
}

#[test]
fn compact_system_prompt_keeps_its_contract() {
    let wide = compact_system_prompt(COMPACT_SYSTEM_PROMPT, 8000);
    let narrow = compact_system_prompt(COMPACT_SYSTEM_PROMPT, 3000);
    for prompt in [&wide, &narrow] {
        assert!(
            prompt.contains("context summarization assistant"),
            "the summary request is identified by this line",
        );
        assert!(!prompt.contains("{{ANALYSIS_STEP}}"), "placeholder left in");
        assert!(prompt.contains("## User Requests"));
        assert!(prompt.contains("## Current Work"));
        assert!(prompt.contains("## Errors & Fixes"));
        // 逐条列用户消息必须有硬上限:没有上限时,首次压缩一个几百轮的会话
        // 会让模型写几百行,输出量直接顶到帽子、把墙钟拖过超时线(09-09)。
        assert!(
            prompt.contains("newest 20 user messages"),
            "User Requests 必须带硬上限"
        );
    }
}

/// 摘要墙钟预算跟着输出帽走。固定 90s 的年代:帽子提到 16384 之后 opus 在
/// 长会话上必然超时,砍完还要再试——一次压缩把 actor 拖住四分半(09-09)。
#[test]
fn summary_timeout_scales_with_the_output_cap() {
    use crate::agent::compact::summary_timeout;
    // 小窗口的 2048 帽:基准就够,落在下限。
    assert_eq!(summary_timeout(2048).as_secs(), 90 + 2048 / 40);
    // 满帽 8192:90 + 204 ≈ 5 分钟。
    assert_eq!(summary_timeout(8192).as_secs(), 294);
    // 再大也不超过 5 分钟。
    assert_eq!(summary_timeout(u32::MAX).as_secs(), 300);
    assert!(summary_timeout(1).as_secs() >= 90, "下限兜底");
}
