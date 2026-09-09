use crate::agent::compact_analysis::{
    compact_system_prompt, strip_analysis_block, AnalysisChunkFilter,
};
use crate::agent::compact_extras::{build_compact_extras, CompactExtras, CompactExtrasPolicy};
use crate::agent::tool_report::replay_rounds;
use crate::llm::{
    ChatMessage, ChatResult, ChatStreamChunk, OpenAiCompatibleClient, ToolDefinition, Usage,
};
use crate::prompts::COMPACT_SYSTEM_PROMPT;
use crate::state::{StateStore, Turn};
use anyhow::{bail, Result};

use super::overflow::estimate_tokens;

const COMPACT_PROMPT_OVERHEAD: usize = 2000;
const MAX_MERGE_ROUNDS: usize = 5;
/// The newest turns are always kept verbatim, budget notwithstanding, so a
/// single oversized turn cannot force a zero-tail compaction.
const MIN_TAIL_TURNS: usize = 2;
/// Fold-economics gate: a non-forced compaction that would free less than
/// this is skipped silently — the cache reset would cost more than it buys.
const MIN_FOLD_TOKENS: usize = 400;
/// Per-item cap for tool reports and reasoning fed to the summarizer. Long
/// tool payloads are the largest summary-input cost and a prompt-injection
/// vector when echoed back into the summary (sub-agent prompts re-entering
/// the session as history).
const SUMMARY_ITEM_MAX_CHARS: usize = 2000;
/// A stalled summarizer stream fails loudly instead of wedging compaction.
/// 固定 90s 是 09-09 的实况事故:输出帽提到 16384 之后,opus 在 148k 上下文
/// 的会话上生成完整摘要要好几分钟,90s 必然砍在半路——而且砍完还要再试,
/// 整个 actor 被拖住四分半。超时必须跟着输出预算走。
const SUMMARY_TIMEOUT_BASE: std::time::Duration = std::time::Duration::from_secs(90);
/// 慢供应商的保守吞吐估计(claude-code 中转的 opus 实测远低于直连)。
const SUMMARY_TOKENS_PER_SEC: u32 = 40;
/// 上下界:再快也至少给 90s(首字节可能就要几十秒),再慢也不超过 5 分钟。
/// 满帽(8192)算出来是 90 + 8192/40 = 294s,基本就是 5 分钟——压缩不再阻塞
/// 其他会话之后(见 web/actor 的 spawn),放宽超时的代价只剩「失败得晚一点」。
const SUMMARY_TIMEOUT_MIN: u64 = 90;
const SUMMARY_TIMEOUT_MAX: u64 = 300;

/// 一次摘要请求的墙钟预算 = 基准 + 输出帽 / 吞吐。
pub(in crate::agent) fn summary_timeout(summary_cap: u32) -> std::time::Duration {
    let generation = u64::from(summary_cap / SUMMARY_TOKENS_PER_SEC);
    let seconds = SUMMARY_TIMEOUT_BASE
        .as_secs()
        .saturating_add(generation)
        .clamp(SUMMARY_TIMEOUT_MIN, SUMMARY_TIMEOUT_MAX);
    std::time::Duration::from_secs(seconds)
}

/// (byte-identical conversation prefix over the fold region, live tools).
/// Built by the agent because only it owns the request rendering; consumed by
/// the fork summarization path, which re-reads the history at cached price.
pub type CompactForkParts = (Vec<ChatMessage>, Vec<ToolDefinition>);
pub type CompactForkBuilder<'a> = &'a dyn Fn(&[String]) -> Result<CompactForkParts>;

pub struct Compactor {
    client: OpenAiCompatibleClient,
    state: StateStore,
    context_window: usize,
    reserved_tokens: usize,
    /// Verbatim tail kept outside the summary. A fixed token count rather
    /// than a window fraction: the trigger grows with the window while the
    /// tail stays constant, which is what stops the re-compaction loop.
    tail_budget_tokens: usize,
    /// 折叠前缀开头的预设对话对数(begin_dialogs)。它们为对齐实况请求的
    /// 缓存字节而留在前缀里,但不是真实会话——摘要指令按这个数量明确
    /// 排除,防止样板对话被总结成伪造的会话事实。
    preset_dialog_pairs: usize,
    /// 压后重建策略(回灌 + 转录)。None = 不产出 extras。
    extras_policy: Option<CompactExtrasPolicy>,
    /// 摘要系统提示词,构造时按输出帽决定开不开分析段并冻结。
    system_prompt: String,
    /// 摘要输出帽。超时按它缩放——生成一万 token 和生成一千 token 不该
    /// 共用一个墙钟预算。
    summary_cap: u32,
}

pub struct CompactResult {
    pub usage: Usage,
    pub usage_estimated: bool,
    pub folded_turns: usize,
    pub kept_turns: usize,
    /// 压缩用的 provider,供用量历史记账(模型按请求轮换,不在此追溯)。
    pub provider_id: Option<String>,
    /// 正文真正回灌进 checkpoint 的文件数(超限只留路径的不算)。
    pub restored_files: usize,
    /// 本次折叠的原文转录路径。
    pub transcript: Option<String>,
}

struct CompactTextResult {
    text: String,
    usage: Usage,
    usage_estimated: bool,
}

impl Compactor {
    pub fn new(
        client: OpenAiCompatibleClient,
        state: StateStore,
        context_window: usize,
        reserved_tokens: usize,
        tail_budget_tokens: usize,
        preset_dialog_pairs: usize,
    ) -> Self {
        // Safety cap: on a small window the tail itself must stay well under
        // the trigger watermark or compaction can never win.
        let tail_budget_tokens = tail_budget_tokens.min(context_window / 2).max(1);
        // Hard cap on the summary completion (pi: 0.8×reserve, opencode: 4k
        // flat): a runaway summary must not eat the reserved output space.
        // 09-09 退回 8192:曾提到 16384(理由是九节结构 + 分析段更长),但
        // 实测四个变体的摘要输出只有 3394-6083 tok,从没接近 8192——帽子提高
        // 一点收益没有,只是把「模型可以一直写」的空间放开了一倍,在长会话 +
        // 慢模型上直接把墙钟推过超时线。
        let summary_cap = ((reserved_tokens as f32 * 0.8) as u32).clamp(2048, 8192);
        let system_prompt = compact_system_prompt(COMPACT_SYSTEM_PROMPT, summary_cap);
        let client = client
            .with_max_tokens(summary_cap)
            .with_request_scope("compact");
        Self {
            client,
            state,
            context_window,
            reserved_tokens,
            tail_budget_tokens,
            preset_dialog_pairs,
            extras_policy: None,
            system_prompt,
            summary_cap,
        }
    }

    /// 压后重建材料的产出策略。预算在这里按窗口缩放:窗口是唯一只有
    /// Compactor 知道的量,而在 168k 窗口合适的 24k 回灌,搬到 32k 小窗上
    /// 就等于压完立刻再超。
    pub fn with_extras(mut self, policy: CompactExtrasPolicy) -> Self {
        let mut policy = policy;
        let window_cap = self.context_window / 8;
        policy.restore_total_tokens = policy.restore_total_tokens.min(window_cap);
        policy.restore_file_tokens = policy.restore_file_tokens.min(policy.restore_total_tokens);
        self.extras_policy = Some(policy);
        self
    }

    fn system_prompt(&self) -> &str {
        &self.system_prompt
    }

    /// Fork summarization: the live conversation prefix (same bytes, same
    /// tools the provider already cached) plus one appended user instruction.
    /// The previous summary is already rendered inside the prefix as the
    /// <conversation-checkpoint> block, so anchoring is implicit. Tool calls
    /// in the response invalidate the attempt (prompt-level deny is not a
    /// guarantee), triggering the isolated fallback.
    async fn summarize_via_fork<F>(
        &self,
        prefix: Vec<ChatMessage>,
        tools: Vec<ToolDefinition>,
        has_previous_summary: bool,
        compact_usage: &mut Usage,
        usage_estimated: &mut bool,
        on_chunk: &mut F,
    ) -> Result<String>
    where
        F: FnMut(ChatStreamChunk) -> Result<()>,
    {
        let mut messages = prefix;
        let anchor_note = if has_previous_summary {
            "A <conversation-checkpoint> block near the top of this conversation holds the previous anchored summary: PRESERVE its still-true content item by item (the standing facts / user agreements section must be carried over verbatim) and merge in the new facts from the conversation. "
        } else {
            ""
        };
        let preset_note = if self.preset_dialog_pairs > 0 {
            format!(
                "The first {} user/assistant exchange(s) at the very top of the conversation are scripted persona example dialogs, NOT part of the real session — exclude them entirely from the summary. ",
                self.preset_dialog_pairs,
            )
        } else {
            String::new()
        };
        messages.push(ChatMessage::plain(
            "user",
            format!(
                "IMPORTANT: The conversation stops here. Do NOT reply to the messages above and do NOT call any tools — a tool call fails this task. You are now acting under the summarization instructions below.\n\n{}\n\n{}{}Summarize the entire conversation above now, following the output structure exactly. Start with the analysis block if the instructions ask for one.",
                self.system_prompt(),
                preset_note,
                anchor_note,
            ),
        ));
        let mut filter = AnalysisChunkFilter::new();
        let result = self
            .client
            .chat_stream(messages.clone(), tools, &mut |chunk| {
                filter.push(chunk, on_chunk)
            })
            .await?;
        filter.finish(on_chunk)?;
        if !result.tool_calls.is_empty() {
            bail!("fork summarization attempted a tool call");
        }
        let outcome = compact_text_result(result, &messages);
        add_usage(compact_usage, &outcome.usage);
        *usage_estimated |= outcome.usage_estimated;
        if outcome.text.trim().is_empty() {
            bail!("fork summarization returned empty output");
        }
        Ok(outcome.text)
    }

    async fn summarize_fold<F>(
        &self,
        fold: &[&Turn],
        prev_text: Option<&str>,
        compact_usage: &mut Usage,
        usage_estimated: &mut bool,
        on_chunk: &mut F,
    ) -> Result<String>
    where
        F: FnMut(ChatStreamChunk) -> Result<()>,
    {
        let usable = self
            .context_window
            .saturating_sub(self.reserved_tokens)
            .saturating_sub(COMPACT_PROMPT_OVERHEAD);
        let fold_text = turns_to_text(fold);
        let fold_tokens = estimate_tokens(&fold_text);

        if fold_tokens <= usable {
            let result = compact_single_pass(
                &self.client,
                self.system_prompt(),
                &fold_text,
                prev_text,
                on_chunk,
            )
            .await?;
            add_usage(compact_usage, &result.usage);
            *usage_estimated |= result.usage_estimated;
            return Ok(result.text);
        }

        let segments = split_into_segments(fold, usable);
        let mut summaries = Vec::new();
        for segment in &segments {
            let segment_text = turns_to_text(segment);
            let result = compact_single_pass(
                &self.client,
                self.system_prompt(),
                &segment_text,
                None,
                &mut |_| Ok(()),
            )
            .await?;
            add_usage(compact_usage, &result.usage);
            *usage_estimated |= result.usage_estimated;
            summaries.push(result.text);
        }
        let result = merge_summaries_tree(
            &self.client,
            self.system_prompt(),
            &summaries,
            prev_text,
            usable,
            on_chunk,
        )
        .await?;
        add_usage(compact_usage, &result.usage);
        *usage_estimated |= result.usage_estimated;
        Ok(result.text)
    }

    /// `mechanical_fallback`: automatic compactions must always free space —
    /// aborting on a summarizer failure leaves the context full and re-fires
    /// the same failing compaction every turn. When set, a failed summary
    /// degrades to a deterministic placeholder (the folded turns stay
    /// soft-deleted in SQLite, so nothing is lost). Manual /compact keeps
    /// erroring so the user sees the real failure.
    /// `fork_builder`: when set (proactive compaction with cache reuse on),
    /// the summary request is a fork of the live conversation — same bytes,
    /// same tools, plus one appended instruction — so the provider prefix
    /// cache pays for reading the history instead of us (Claude Code
    /// strategy). Must be None on overflow recovery: the fork would overflow
    /// exactly like the request it is trying to rescue. A fork failure falls
    /// back to the isolated serialized path.
    pub async fn perform_compact<F>(
        &self,
        force: bool,
        mechanical_fallback: bool,
        fork_builder: Option<CompactForkBuilder<'_>>,
        on_chunk: &mut F,
    ) -> Result<Option<CompactResult>>
    where
        F: FnMut(ChatStreamChunk) -> Result<()>,
    {
        let turns = self.state.load_visible_turns()?;
        if turns.is_empty() {
            return Ok(None);
        }
        if turns
            .iter()
            .any(|turn| turn.status == crate::state::TurnStatus::Running)
        {
            bail!("cannot compact while another conversation turn is running");
        }

        let head: Vec<&Turn> = turns.iter().filter(|turn| !turn.is_summary).collect();
        if head.is_empty() {
            return Ok(None);
        }

        // Cut point: walk newest → oldest accumulating the verbatim tail;
        // everything before the cut folds into the summary. Turn granularity
        // means the cut can never split a tool call/result pair.
        let cut = find_cut_index(&head, self.tail_budget_tokens);
        if cut == 0 {
            // Everything fits in the tail budget — nothing to fold. This is
            // the "kept region is still within budget" guard against
            // re-compacting a freshly compacted session.
            return Ok(None);
        }
        let fold = &head[..cut];
        if !force {
            let fold_tokens: usize = fold
                .iter()
                .map(|turn| estimate_tokens(&turn_to_text(turn)))
                .sum();
            if fold_tokens < MIN_FOLD_TOKENS {
                return Ok(None);
            }
        }

        let previous_summary = self.state.load_last_summary()?;
        // The footprint sections are code-owned: strip them from the anchor
        // so the LLM cannot garble them, then re-append the merged sets.
        let prev_text = previous_summary
            .as_ref()
            .map(|t| strip_footprint_sections(&t.assistant_content).to_string());

        let mut compact_usage = Usage::default();
        let mut usage_estimated = false;

        let fold_turn_ids = fold
            .iter()
            .map(|turn| turn.turn_id.clone())
            .collect::<Vec<_>>();

        // Deterministic footprint: merged from the folded turns plus the
        // previous summary row (which carries everything it already folded).
        let mut footprint = self.state.load_merged_footprint(&fold_turn_ids)?;
        if let Some(prev) = previous_summary.as_ref() {
            footprint.merge(
                self.state
                    .load_merged_footprint(std::slice::from_ref(&prev.turn_id))?,
            );
        }

        let budget = summary_timeout(self.summary_cap);
        // fork 超时后不再走隔离路径:隔离路径向同一个模型要同样长的输出,
        // 还没有前缀缓存可吃,只会更慢。再等一个完整预算换来的多半是第二次
        // 超时,代价却是把调用方(以及整个压缩)多拖住几分钟。
        let mut fork_timed_out = false;
        let mut fork_summary = None;
        if let Some(builder) = fork_builder {
            match builder(&fold_turn_ids) {
                Ok((prefix, tools)) => {
                    match tokio::time::timeout(
                        budget,
                        self.summarize_via_fork(
                            prefix,
                            tools,
                            prev_text.is_some(),
                            &mut compact_usage,
                            &mut usage_estimated,
                            on_chunk,
                        ),
                    )
                    .await
                    {
                        Ok(Ok(text)) => fork_summary = Some(text),
                        Ok(Err(error)) => tracing::warn!(
                            error = %error,
                            "fork summarization failed; falling back to the isolated path"
                        ),
                        Err(_) => {
                            fork_timed_out = true;
                            tracing::warn!(
                                budget_secs = budget.as_secs(),
                                summary_cap = self.summary_cap,
                                "fork summarization timed out; not retrying on the isolated path"
                            );
                        }
                    }
                }
                Err(error) => tracing::warn!(
                    error = %error,
                    "fork prefix build failed; falling back to the isolated path"
                ),
            }
        }

        // Timeout keeps a stalled summarizer stream from wedging the
        // compaction placeholder forever; one retry absorbs transient
        // provider failures but a timeout is not retried (the caller should
        // not wait another full budget on a provider that just proved slow).
        let summary_outcome = if let Some(text) = fork_summary {
            Ok(text)
        } else if fork_timed_out {
            Err(anyhow::anyhow!(
                "compaction summary timed out after {}s",
                budget.as_secs()
            ))
        } else {
            let first_attempt = tokio::time::timeout(
                budget,
                self.summarize_fold(
                    fold,
                    prev_text.as_deref(),
                    &mut compact_usage,
                    &mut usage_estimated,
                    on_chunk,
                ),
            )
            .await;
            match first_attempt {
                Ok(Ok(text)) => Ok(text),
                Ok(Err(first_error)) => {
                    match tokio::time::timeout(
                        budget,
                        self.summarize_fold(
                            fold,
                            prev_text.as_deref(),
                            &mut compact_usage,
                            &mut usage_estimated,
                            on_chunk,
                        ),
                    )
                    .await
                    {
                        Ok(Ok(text)) => Ok(text),
                        Ok(Err(retry_error)) => Err(retry_error.context(first_error)),
                        Err(_) => Err(anyhow::anyhow!(
                            "compaction summary timed out after {}s (retry)",
                            budget.as_secs()
                        )),
                    }
                }
                Err(_) => Err(anyhow::anyhow!(
                    "compaction summary timed out after {}s",
                    budget.as_secs()
                )),
            }
        };
        let summary = match summary_outcome {
            Ok(text) => text,
            Err(error) if mechanical_fallback => {
                tracing::warn!(
                    error = %error,
                    folded = fold.len(),
                    "compaction summary unavailable; folding mechanically"
                );
                mechanical_fold_digest(fold.len())
            }
            Err(error) => return Err(error),
        };
        let summary = append_footprint_sections(summary, &footprint);
        let footprint_json = if footprint.is_empty() {
            None
        } else {
            Some(serde_json::to_string(&footprint)?)
        };

        // 压后重建材料:回灌最近碰过的文件正文 + 折叠原文转录。尾巴逐字
        // 保留,它读过的东西不重复回灌。
        let tail = &head[cut..];
        let previous_extras = match previous_summary.as_ref() {
            Some(previous) => self
                .state
                .load_summary_extras_json(&previous.turn_id)?
                .and_then(|json| serde_json::from_str::<CompactExtras>(&json).ok()),
            None => None,
        };
        let extras = self
            .extras_policy
            .as_ref()
            .map(|policy| {
                build_compact_extras(
                    policy,
                    &self.state.session_id(),
                    fold,
                    tail,
                    previous_extras.as_ref(),
                    prev_text.as_deref(),
                )
            })
            .filter(|extras| !extras.is_empty());
        let extras_json = extras.as_ref().map(serde_json::to_string).transpose()?;
        let restored_files = extras
            .as_ref()
            .map(CompactExtras::included_files)
            .unwrap_or(0);
        let transcript = extras
            .as_ref()
            .and_then(|extras| extras.transcripts.first().cloned());

        let visible_turn_ids = turns
            .iter()
            .map(|turn| turn.turn_id.clone())
            .collect::<Vec<_>>();
        self.state.replace_visible_with_summary(
            &fold_turn_ids,
            &visible_turn_ids,
            &summary,
            crate::llm::TurnTokens::from_usage(Some(&compact_usage)),
            usage_estimated,
            footprint_json.as_deref(),
            extras_json.as_deref(),
        )?;
        tracing::info!(
            folded_turns = fold.len(),
            kept_turns = head.len() - cut,
            summary_chars = summary.len(),
            restored_files,
            transcript = transcript.as_deref().unwrap_or(""),
            "context_rewrite reason=compact"
        );
        Ok(Some(CompactResult {
            usage: compact_usage,
            usage_estimated,
            folded_turns: fold.len(),
            kept_turns: head.len() - cut,
            provider_id: Some(self.client.provider_id().to_string()),
            restored_files,
            transcript,
        }))
    }
}

/// Returns the number of oldest turns to fold (index of the first kept
/// turn). 0 means everything fits in the tail budget. The newest
/// MIN_TAIL_TURNS turns are kept unconditionally.
fn find_cut_index(head: &[&Turn], tail_budget_tokens: usize) -> usize {
    let mut acc = 0usize;
    for i in (0..head.len()).rev() {
        let kept_count = head.len() - i;
        let turn_tokens = estimate_tokens(&turn_to_text(head[i]));
        if kept_count <= MIN_TAIL_TURNS {
            acc = acc.saturating_add(turn_tokens);
            continue;
        }
        if acc.saturating_add(turn_tokens) > tail_budget_tokens {
            return i + 1;
        }
        acc = acc.saturating_add(turn_tokens);
    }
    0
}

/// Deterministic stand-in when the summarizer is unavailable. The folded
/// turns remain soft-deleted in SQLite (undo can restore them), so degrading
/// beats aborting: an aborted auto-compaction leaves the context full and
/// re-fires the same failing call every turn.
fn mechanical_fold_digest(folded_turns: usize) -> String {
    format!(
        "{folded_turns} earlier conversation turn(s) were folded here to free context, but the automatic summary was unavailable. The original turns are still archived; ask the user if details from before this point are needed."
    )
}

const FOOTPRINT_MARKER: &str = "\n\n<read-files>";
const FOOTPRINT_MARKER_ALT: &str = "\n\n<modified-files>";
const FOOTPRINT_MARKER_MEM: &str = "\n\n<saved-memories>";

/// Removes the code-appended footprint block from a stored summary so the
/// anchor sent to the LLM contains only prose it is allowed to rewrite.
fn strip_footprint_sections(summary: &str) -> &str {
    let cut = [FOOTPRINT_MARKER, FOOTPRINT_MARKER_ALT, FOOTPRINT_MARKER_MEM]
        .iter()
        .filter_map(|marker| summary.find(marker))
        .min();
    match cut {
        Some(index) => summary[..index].trim_end(),
        None => summary,
    }
}

/// Appends the deterministic footprint after the LLM summary (pi's pattern:
/// enumerable facts never pass through the summarizer, so they cannot be
/// dropped or hallucinated). BTreeSet iteration keeps the bytes stable.
fn append_footprint_sections(summary: String, footprint: &crate::state::ToolFootprint) -> String {
    if footprint.is_empty() {
        return summary;
    }
    let mut output = summary.trim_end().to_string();
    let mut push_section = |tag: &str, items: &std::collections::BTreeSet<String>| {
        if items.is_empty() {
            return;
        }
        output.push_str("\n\n<");
        output.push_str(tag);
        output.push('>');
        for item in items {
            output.push('\n');
            output.push_str(item);
        }
        output.push_str("\n</");
        output.push_str(tag);
        output.push('>');
    };
    push_section("read-files", &footprint.read);
    push_section("modified-files", &footprint.modified);
    push_section("saved-memories", &footprint.memories);
    output
}

/// Head-truncate a summarizer input item at a char boundary, marking the cut.
fn truncate_for_summary(text: &str) -> std::borrow::Cow<'_, str> {
    if text.len() <= SUMMARY_ITEM_MAX_CHARS {
        return std::borrow::Cow::Borrowed(text);
    }
    let mut end = SUMMARY_ITEM_MAX_CHARS;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    std::borrow::Cow::Owned(format!(
        "{}\n[... {} more bytes truncated ...]",
        &text[..end],
        text.len() - end
    ))
}

fn add_usage(total: &mut Usage, usage: &Usage) {
    total.prompt_tokens = total.prompt_tokens.saturating_add(usage.prompt_tokens);
    total.completion_tokens = total
        .completion_tokens
        .saturating_add(usage.completion_tokens);
    total.total_tokens = total
        .total_tokens
        .saturating_add(usage.effective_total_tokens());
    // 缓存字段曾被丢弃:fork 式折叠明明大量命中,summary 轮与用量史却
    // 记 0,Σ 命中率随折叠次数被系统性低估(deepseek 报告 P1 实证)。
    //
    // 09-09 补:上一轮只补了供应商原始字段,漏了归一化后的
    // `cache_read_tokens` —— 而 `TurnTokens::from_usage` 读的正是它,于是
    // 摘要行的 token_cache_read 照旧记 0。实测 fork 摘要 26911 prompt 里
    // 命中 25984(96.6%),落库仍是 0。
    total.cache_read_tokens = total
        .cache_read_tokens
        .saturating_add(usage.cache_read_tokens);
    total.cache_write_tokens = total
        .cache_write_tokens
        .saturating_add(usage.cache_write_tokens);
    total.cache_reported |= usage.cache_reported;
    if let Some(hit) = usage.prompt_cache_hit_tokens {
        total.prompt_cache_hit_tokens = Some(
            total
                .prompt_cache_hit_tokens
                .unwrap_or(0)
                .saturating_add(hit),
        );
    }
    if let Some(miss) = usage.prompt_cache_miss_tokens {
        total.prompt_cache_miss_tokens = Some(
            total
                .prompt_cache_miss_tokens
                .unwrap_or(0)
                .saturating_add(miss),
        );
    }
    if let Some(details) = usage.prompt_tokens_details.as_ref() {
        if let Some(cached) = details.cached_tokens {
            let slot = total
                .prompt_tokens_details
                .get_or_insert_with(Default::default);
            slot.cached_tokens = Some(slot.cached_tokens.unwrap_or(0).saturating_add(cached));
        }
    }
}

fn turns_to_text(turns: &[&Turn]) -> String {
    let mut output = String::new();
    for (i, turn) in turns.iter().enumerate() {
        if turn.is_summary {
            continue;
        }
        output.push_str(&format!("--- Turn {} ---\n", i + 1));
        output.push_str("User: ");
        output.push_str(&turn.user_content);
        for exchange in &turn.question_exchanges {
            output.push_str("\nAssistant clarification: ");
            output.push_str(&crate::question::assistant_exchange_text(exchange));
            output.push_str("\nUser clarification: ");
            output.push_str(&crate::question::user_exchange_text(exchange));
        }
        for followup in &turn.followups {
            if let Some(content) = &followup.preceding_assistant_content {
                if !content.trim().is_empty() {
                    output.push_str("\nAssistant: ");
                    output.push_str(content);
                }
            }
            if let Some(reasoning) = &followup.preceding_assistant_reasoning {
                if !reasoning.trim().is_empty() {
                    output.push_str("\n[Reasoning: ");
                    output.push_str(reasoning);
                    output.push(']');
                }
            }
            output.push_str("\nUser: ");
            output.push_str(&followup.content);
        }
        output.push_str("\nAssistant: ");
        output.push_str(&turn.assistant_content);
        if let Some(reasoning) = &turn.assistant_reasoning {
            if !reasoning.trim().is_empty() {
                output.push_str("\n[Reasoning: ");
                output.push_str(&truncate_for_summary(reasoning));
                output.push(']');
            }
        }
        for report in &turn.tool_reports {
            output.push_str("\n[Tool Report: ");
            output.push_str(&truncate_for_summary(report));
            output.push(']');
        }
        output.push('\n');
    }
    output
}

fn build_compact_prompt(history: &str, previous_summary: Option<&str>) -> String {
    match previous_summary {
        Some(prev) => format!(
            "Update the anchored summary in <previous-summary> using the new conversation \
             history in <conversation>.\n\
             Rules:\n\
             - PRESERVE all still-true information from the previous summary; keep exact \
             file paths, names, identifiers, and user-stated facts verbatim.\n\
             - The standing facts / user agreements section, if present, must be carried \
             over item by item; never reworded away.\n\
             - ADD new facts, decisions, and progress from the new history.\n\
             - UPDATE status: move finished in-progress items to done; drop resolved \
             blockers; rewrite next steps to match the current state.\n\
             - Remove a detail only when the new history explicitly made it stale.\n\
             - Keep the User Requests list bounded as the structure describes: the \
             newest 20 verbatim, everything older compressed into at most 5 \
             grouped lines.\n\n\
             <previous-summary>\n{prev}\n</previous-summary>\n\n\
             <conversation>\n{history}\n</conversation>"
        ),
        None => format!(
            "Create a new anchored summary from the conversation history in <conversation>.\n\n\
             <conversation>\n{history}\n</conversation>"
        ),
    }
}

async fn compact_single_pass<F>(
    client: &OpenAiCompatibleClient,
    system_prompt: &str,
    history: &str,
    previous_summary: Option<&str>,
    on_chunk: &mut F,
) -> Result<CompactTextResult>
where
    F: FnMut(ChatStreamChunk) -> Result<()>,
{
    let prompt = build_compact_prompt(history, previous_summary);
    let messages = vec![
        ChatMessage::system(system_prompt.to_string()),
        ChatMessage::plain("user", &prompt),
    ];
    let mut filter = AnalysisChunkFilter::new();
    let result = client
        .chat_stream(messages.clone(), vec![], &mut |chunk| {
            filter.push(chunk, on_chunk)
        })
        .await?;
    filter.finish(on_chunk)?;
    Ok(compact_text_result(result, &messages))
}

async fn compact_single_pass_text<F>(
    client: &OpenAiCompatibleClient,
    system_prompt: &str,
    text: &str,
    previous_summary: Option<&str>,
    on_chunk: &mut F,
) -> Result<CompactTextResult>
where
    F: FnMut(ChatStreamChunk) -> Result<()>,
{
    let prompt = match previous_summary {
        Some(prev) => format!(
            "Update the anchored summary below using the segment summaries above.\n\
             Preserve still-true details, remove stale details, and merge in the new facts.\n\
             <previous-summary>\n{prev}\n</previous-summary>\n\n\
             <segment-summaries>\n{text}\n</segment-summaries>"
        ),
        None => format!(
            "Merge the following segment summaries into a single coherent summary.\n\n\
             <segment-summaries>\n{text}\n</segment-summaries>"
        ),
    };
    let messages = vec![
        ChatMessage::system(system_prompt.to_string()),
        ChatMessage::plain("user", &prompt),
    ];
    let mut filter = AnalysisChunkFilter::new();
    let result = client
        .chat_stream(messages.clone(), vec![], &mut |chunk| {
            filter.push(chunk, on_chunk)
        })
        .await?;
    filter.finish(on_chunk)?;
    Ok(compact_text_result(result, &messages))
}

fn compact_text_result(result: ChatResult, messages: &[ChatMessage]) -> CompactTextResult {
    // 分析段是草稿,不落库。三条摘要路径(fork / 单趟 / 树状合并)的输出都
    // 经过这里,剥一次即可全覆盖。
    let content = strip_analysis_block(&result.content);
    if let Some(usage) = result.usage {
        return CompactTextResult {
            text: content,
            usage,
            usage_estimated: result.usage_estimated,
        };
    }

    let prompt_tokens = super::overflow::estimate_messages_tokens(messages) as u64;
    let completion_tokens = estimate_tokens(&content) as u64;
    CompactTextResult {
        text: content,
        usage: Usage {
            prompt_tokens,
            completion_tokens,
            total_tokens: prompt_tokens.saturating_add(completion_tokens),
            ..Usage::default()
        },
        usage_estimated: true,
    }
}

fn split_into_segments<'a>(turns: &[&'a Turn], budget_tokens: usize) -> Vec<Vec<&'a Turn>> {
    let mut segments = Vec::new();
    let mut current = Vec::new();
    let mut current_tokens = 0usize;

    for turn in turns {
        let turn_tokens = estimate_tokens(&turn_to_text(turn));
        if current_tokens + turn_tokens > budget_tokens && !current.is_empty() {
            segments.push(std::mem::take(&mut current));
            current_tokens = 0;
        }
        current.push(*turn);
        current_tokens += turn_tokens;
    }
    if !current.is_empty() {
        segments.push(current);
    }
    segments
}

fn turn_to_text(turn: &Turn) -> String {
    let mut output = String::new();
    output.push_str(&turn.user_content);
    for exchange in &turn.question_exchanges {
        output.push_str(&crate::question::assistant_exchange_text(exchange));
        output.push_str(&crate::question::user_exchange_text(exchange));
    }
    for followup in &turn.followups {
        if let Some(content) = &followup.preceding_assistant_content {
            output.push_str(content);
        }
        if let Some(reasoning) = &followup.preceding_assistant_reasoning {
            output.push_str(reasoning);
        }
        output.push_str(&followup.content);
    }
    output.push_str(&turn.assistant_content);
    if let Some(reasoning) = &turn.assistant_reasoning {
        output.push_str(reasoning);
    }
    // v20+ 工具密集回合的主体在 tool_flow 里(reports 多为空):漏计它,
    // 压缩预算会把"40 轮"当成保尾额度塞进 16K,压后必然仍超。
    for round in replay_rounds(&turn.tool_flow) {
        output.push_str(&round.assistant_content);
        if let Some(reasoning) = &round.assistant_reasoning {
            output.push_str(reasoning);
        }
        for call in &round.calls {
            output.push_str(&call.name);
            output.push_str(&call.arguments);
            output.push_str(&call.output);
        }
    }
    for report in &turn.tool_reports {
        output.push_str(report);
    }
    output
}

async fn merge_summaries_tree<F>(
    client: &OpenAiCompatibleClient,
    system_prompt: &str,
    summaries: &[String],
    previous_summary: Option<&str>,
    usable_tokens: usize,
    on_chunk: &mut F,
) -> Result<CompactTextResult>
where
    F: FnMut(ChatStreamChunk) -> Result<()>,
{
    if summaries.len() == 1 {
        return Ok(CompactTextResult {
            text: summaries[0].clone(),
            usage: Usage::default(),
            usage_estimated: false,
        });
    }

    let mut current: Vec<String> = summaries.to_vec();
    let mut total_usage = Usage::default();
    let mut usage_estimated = false;

    for _round in 0..MAX_MERGE_ROUNDS {
        let combined = current.join("\n\n---\n\n");
        let combined_tokens = estimate_tokens(&combined);

        if combined_tokens <= usable_tokens {
            let result = compact_single_pass_text(
                client,
                system_prompt,
                &combined,
                previous_summary,
                on_chunk,
            )
            .await?;
            add_usage(&mut total_usage, &result.usage);
            usage_estimated |= result.usage_estimated;
            return Ok(CompactTextResult {
                text: result.text,
                usage: total_usage,
                usage_estimated,
            });
        }

        let mut next = Vec::new();
        let mut batch = Vec::new();
        let mut batch_tokens = 0usize;

        for s in &current {
            let s_tokens = estimate_tokens(s);
            if batch_tokens + s_tokens > usable_tokens && !batch.is_empty() {
                let batch_text = batch.join("\n\n---\n\n");
                let merged =
                    compact_single_pass_text(client, system_prompt, &batch_text, None, &mut |_| {
                        Ok(())
                    })
                    .await?;
                add_usage(&mut total_usage, &merged.usage);
                usage_estimated |= merged.usage_estimated;
                next.push(merged.text);
                batch.clear();
                batch_tokens = 0;
            }
            batch.push(s.clone());
            batch_tokens += s_tokens;
        }
        if !batch.is_empty() {
            let batch_text = batch.join("\n\n---\n\n");
            let merged =
                compact_single_pass_text(client, system_prompt, &batch_text, None, &mut |_| Ok(()))
                    .await?;
            add_usage(&mut total_usage, &merged.usage);
            usage_estimated |= merged.usage_estimated;
            next.push(merged.text);
        }

        if next.len() >= current.len() {
            let combined = current.join("\n\n---\n\n");
            let result = compact_single_pass_text(
                client,
                system_prompt,
                &combined,
                previous_summary,
                on_chunk,
            )
            .await?;
            add_usage(&mut total_usage, &result.usage);
            usage_estimated |= result.usage_estimated;
            return Ok(CompactTextResult {
                text: result.text,
                usage: total_usage,
                usage_estimated,
            });
        }
        current = next;
    }

    let combined = current.join("\n\n---\n\n");
    let result =
        compact_single_pass_text(client, system_prompt, &combined, previous_summary, on_chunk)
            .await?;
    add_usage(&mut total_usage, &result.usage);
    usage_estimated |= result.usage_estimated;
    Ok(CompactTextResult {
        text: result.text,
        usage: total_usage,
        usage_estimated,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::TurnStatus;

    fn turn(id: &str, seq: i64, content: &str) -> Turn {
        Turn {
            turn_id: id.to_string(),
            seq,
            user_content: content.to_string(),
            display_content: content.to_string(),
            user_timestamp: "now".to_string(),
            assistant_content: String::new(),
            assistant_reasoning: None,
            assistant_provider_id: None,
            assistant_model: None,
            assistant_timestamp: None,
            status: TurnStatus::Completed,
            tool_reports: Vec::new(),
            tool_flow: Vec::new(),
            question_exchanges: Vec::new(),
            followups: Vec::new(),
            attachments: Vec::new(),
            hidden: false,
            is_summary: false,
            owner_pid: None,
            token_total: 0,
            token_prompt: 0,
            token_cache_read: 0,
            token_usage_estimated: false,
            revision: 0,
            journal_events: Vec::new(),
            context_messages: Vec::new(),
        }
    }

    #[test]
    fn cut_index_keeps_everything_when_history_fits_the_tail() {
        let turns: Vec<Turn> = (0..4).map(|i| turn(&format!("t{i}"), i, "hi")).collect();
        let refs: Vec<&Turn> = turns.iter().collect();
        assert_eq!(find_cut_index(&refs, 1_000_000), 0);
    }

    #[test]
    fn cut_index_folds_oldest_turns_beyond_the_budget() {
        let body = "lorem ipsum dolor sit amet ".repeat(40);
        let turns: Vec<Turn> = (0..5).map(|i| turn(&format!("t{i}"), i, &body)).collect();
        let refs: Vec<&Turn> = turns.iter().collect();
        let per_turn = estimate_tokens(&turn_to_text(refs[0]));
        // Budget covers the 2-turn floor plus half a turn: the floor is
        // unconditional, the third-newest turn no longer fits, so the two
        // oldest turns fold.
        let budget = per_turn * 2 + per_turn / 2;
        assert_eq!(find_cut_index(&refs, budget), 3);
    }

    #[test]
    fn cut_index_keeps_the_two_newest_turns_even_when_oversized() {
        let body = "lorem ipsum dolor sit amet ".repeat(400);
        let turns: Vec<Turn> = (0..2).map(|i| turn(&format!("t{i}"), i, &body)).collect();
        let refs: Vec<&Turn> = turns.iter().collect();
        assert_eq!(find_cut_index(&refs, 1), 0);
        let turns: Vec<Turn> = (0..3).map(|i| turn(&format!("t{i}"), i, &body)).collect();
        let refs: Vec<&Turn> = turns.iter().collect();
        assert_eq!(find_cut_index(&refs, 1), 1);
    }

    #[test]
    fn footprint_sections_round_trip() {
        let mut fp = crate::state::ToolFootprint::default();
        fp.read.insert("src/a.rs".to_string());
        fp.modified.insert("src/b.rs".to_string());
        fp.memories.insert("用户喜欢橘猫".to_string());
        let summary = append_footprint_sections("## Goal\nstuff".to_string(), &fp);
        assert!(summary.contains("<read-files>\nsrc/a.rs\n</read-files>"));
        assert!(summary.contains("<modified-files>\nsrc/b.rs\n</modified-files>"));
        assert!(summary.contains("<saved-memories>\n用户喜欢橘猫\n</saved-memories>"));
        // Anchor sent back to the LLM must not contain the code-owned block.
        assert_eq!(strip_footprint_sections(&summary), "## Goal\nstuff");
        let empty = crate::state::ToolFootprint::default();
        assert_eq!(append_footprint_sections("x".to_string(), &empty), "x");
    }

    #[test]
    fn summary_truncation_respects_multibyte_boundaries() {
        let text = "汉".repeat(SUMMARY_ITEM_MAX_CHARS);
        let truncated = truncate_for_summary(&text);
        assert!(truncated.len() < text.len());
        assert!(truncated.contains("more bytes truncated"));
        assert!(std::str::from_utf8(truncated.as_bytes()).is_ok());
        let short = "short";
        assert_eq!(truncate_for_summary(short), short);
    }

    /// fork 摘要大量命中缓存,而摘要行落库记的是 `TurnTokens::from_usage`
    /// 读的那个归一化字段。累加时漏掉它 → 命中率永远显示 0(09-09 实测:
    /// 26911 prompt 命中 25984,落库仍是 0)。退回修复前这条报红。
    #[test]
    fn add_usage_keeps_the_normalized_cache_counters() {
        let round = Usage {
            prompt_tokens: 26911,
            completion_tokens: 4031,
            total_tokens: 30942,
            cache_read_tokens: 25984,
            cache_reported: true,
            prompt_cache_hit_tokens: Some(25984),
            ..Usage::default()
        };
        let mut total = Usage::default();
        add_usage(&mut total, &round);
        add_usage(&mut total, &round);

        assert_eq!(total.cache_read_tokens, 51968);
        assert!(total.cache_reported);
        let tokens = crate::llm::TurnTokens::from_usage(Some(&total));
        assert_eq!(tokens.cache_read, 51968, "落库口径必须带上命中数");
        assert_eq!(tokens.prompt, 53822);
    }
}
