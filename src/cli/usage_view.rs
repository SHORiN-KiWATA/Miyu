//! 用量与压缩水位的文字概览。
//!
//! `compact_watermark_text` 把「离压缩还有多远」画成一行；`usage_overview_text`
//! 汇总一段时间的花费。查不到价格时显示「—」而不是猜一个数字。

use crate::cli::*;

/// The daemon reports Σ as three flat numbers; regroup them for the meters.
pub(in crate::cli) fn state_cumulative(state: &ipc::SessionState) -> TurnTokens {
    TurnTokens {
        total: state.cumulative_tokens,
        prompt: state.cumulative_prompt_tokens,
        cache_read: state.cumulative_cache_read_tokens,
    }
}

/// One line per context watermark: absolute tokens left before each tier
/// fires (compaction / forced compaction). Absolute values, not percentages
/// — same reasoning as the cache accounting log line.
pub(in crate::cli) fn compact_watermark_text(
    context_tokens: usize,
    window: usize,
    context: &crate::config::ContextConfig,
) -> String {
    let tier = |label: &str, ratio: f32| -> String {
        let threshold = (window as f32 * ratio).max(1.0) as usize;
        if context_tokens >= threshold {
            format!("{label} {}✓", t("reached", "已达"))
        } else {
            format!("{label} -{}", threshold - context_tokens)
        }
    };
    format!(
        "{}: {} / {} · {}",
        t("Context watermarks", "上下文水位"),
        context_tokens,
        window,
        [
            tier(t("compact", "压缩"), context.trim_at_ratio),
            tier(t("force", "强制"), context.compact_force_ratio),
        ]
        .join(" · ")
    )
}

pub(in crate::cli) fn usage_overview_text(
    snapshot: &crate::state::UsageSnapshot,
    context: Option<(u64, Option<usize>)>,
) -> String {
    let compact = render::format_compact_count;
    let mut lines = Vec::new();
    lines.push(format!(
        "\x1b[1m{}\x1b[0m \x1b[2m{}\x1b[0m",
        t("Token usage", "Token 用量"),
        t(
            "(global totals: all sessions + background calls)",
            "（全局累计：含所有会话与后台调用）"
        )
    ));
    lines.push(format!(
        "  {:<10} {}",
        t("requests", "请求次数"),
        compact(snapshot.requests)
    ));
    let cached = snapshot.cache_read_tokens;
    let fresh = snapshot.prompt_tokens.saturating_sub(cached);
    let mut input_line = format!(
        "  {:<10} {}",
        t("input", "输入"),
        compact(snapshot.prompt_tokens)
    );
    if cached > 0 {
        input_line.push_str(&format!(
            "\x1b[2m（{} {} · {} {}",
            t("cache hits", "缓存命中"),
            compact(cached),
            t("billed new", "计费新输入"),
            compact(fresh)
        ));
        if snapshot.cache_write_tokens > 0 {
            input_line.push_str(&format!(
                " · {} {}",
                t("cache writes", "缓存写入"),
                compact(snapshot.cache_write_tokens)
            ));
        }
        input_line.push_str("）\x1b[0m");
    }
    lines.push(input_line);
    let mut output_line = format!(
        "  {:<10} {}",
        t("output", "输出"),
        compact(snapshot.completion_tokens)
    );
    if snapshot.reasoning_tokens > 0 {
        output_line.push_str(&format!(
            "\x1b[2m（{} {}）\x1b[0m",
            t("reasoning", "其中思考"),
            compact(snapshot.reasoning_tokens)
        ));
    }
    lines.push(output_line);
    lines.push(format!(
        "  {:<10} {} \x1b[2m· {} Σ{}\x1b[0m",
        t("total", "总计"),
        compact(snapshot.total_tokens),
        t("conversation", "对话口径"),
        compact(snapshot.conversation_tokens)
    ));
    if let Some(last) = snapshot
        .last_conversation_usage
        .as_ref()
        .or(snapshot.last_usage.as_ref())
    {
        let mut line = format!(
            "  {:<10} in {}",
            t("last turn", "最近一轮"),
            compact(last.prompt_tokens)
        );
        if last.cache_read_tokens > 0 {
            line.push_str(&format!("(C{})", compact(last.cache_read_tokens)));
        }
        line.push_str(&format!(" · out {}", compact(last.completion_tokens)));
        lines.push(line);
    }
    if let Some((tokens, window)) = context {
        let window = window
            .map(|value| render::format_compact_count(value as u64))
            .unwrap_or_else(|| "?".to_string());
        lines.push(format!(
            "  {:<10} {} / {window}",
            t("context", "会话上下文"),
            compact(tokens)
        ));
    }
    lines.join("\n")
}

pub(in crate::cli) fn readable_bytes(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut size = bytes as f64;
    let mut unit = 0;
    while size >= 1024.0 && unit + 1 < UNITS.len() {
        size /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} {}", UNITS[0])
    } else {
        format!("{size:.1} {}", UNITS[unit])
    }
}

/// `t` for messages built at runtime — the static version cannot take a `format!`.
pub(in crate::cli) fn owned(en: String, zh: String) -> String {
    if crate::i18n::is_zh() {
        zh
    } else {
        en
    }
}

pub(in crate::cli) fn print_chat_token_usage(
    result: &crate::llm::ChatResult,
    enabled: bool,
    session_token_total: u64,
    context_window: Option<usize>,
    cumulative: TurnTokens,
) -> Result<()> {
    if enabled && result.usage.is_some() {
        let meter = turn_meter(
            TurnTokens::from_usage(result.usage.as_ref()),
            GenerationSpeed::from_usage(result.usage.as_ref()),
            session_token_total,
            context_window,
            cumulative,
        );
        render::print_token_usage(&meter, result.usage_estimated)?;
    }
    Ok(())
}

pub(in crate::cli) fn result_context_window(
    config: &AppConfig,
    result: &crate::llm::ChatResult,
) -> Option<usize> {
    if config.active_provider_model_choices().len() > 1 {
        return None;
    }
    let provider = result.provider_id.as_deref()?;
    let model = result.model.as_deref()?;
    config
        .context_window_for_provider_model(provider, model)
        .ok()
        .flatten()
}

pub(in crate::cli) async fn handle_post_turn_overflow(
    agent: &Agent,
    renderer: &mut render::StreamRenderer,
    context_tokens: u64,
    show_token_usage: bool,
    cumulative_tokens: Option<&mut TurnTokens>,
) -> Result<Option<crate::llm::ChatResult>> {
    let compact_result = agent
        .handle_overflow_after_turn(context_tokens, |event| handle_agent_event(renderer, event))
        .await?;
    renderer.finish()?;
    if let Some(compact_result) = compact_result {
        let mut cumulative_display = TurnTokens::default();
        if let Some(total) = cumulative_tokens {
            if let Some(usage) = compact_result.usage.as_ref() {
                total.add(TurnTokens::from_usage(Some(usage)));
                cumulative_display = *total;
            }
        }
        print_chat_token_usage(
            &compact_result,
            show_token_usage,
            agent.effective_context_tokens()?,
            agent.context_window(),
            cumulative_display,
        )?;
        return Ok(Some(compact_result));
    }
    Ok(None)
}
