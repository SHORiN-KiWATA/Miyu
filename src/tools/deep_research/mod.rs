mod prompts;
mod report;
use prompts::*;
use report::*;

use super::subagent_runner::{
    clip_inline, format_token_count, ProgressMode, SubagentProgress, SubagentRunner,
};
use super::{ToolProgress, ToolRegistry, ToolSpec};
use crate::config::{AppConfig, DeepResearchPluginConfig};
use crate::i18n::{is_zh, text as t};
use crate::llm::{ChatMessage, ChatStreamChunk, ChatStreamKind, OpenAiCompatibleClient, Usage};
use crate::paths::MiyuPaths;
use anyhow::{bail, Result};
use chrono::Local;
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

#[derive(Clone)]
struct DeepResearchContext {
    config: AppConfig,
    paths: MiyuPaths,
    tools: ToolRegistry,
}

#[derive(Default)]
struct ResearchState {
    topic_title: String,
    references: Vec<Reference>,
    counters: ReferenceCounters,
    stats: ResearchStats,
}

pub fn register(
    registry: &mut ToolRegistry,
    config: AppConfig,
    paths: MiyuPaths,
    tools: ToolRegistry,
) {
    let context = DeepResearchContext {
        config,
        paths,
        tools,
    };
    registry.register(ToolSpec::new_with_progress(
        "deep_research",
        "Run a dual-role deep research task and write the final Markdown report to the configured output directory.",
        json!({
            "type": "object",
            "properties": {
                "topic": { "type": "string", "description": "Research question or topic." },
                "thinking_depth": { "type": "string", "enum": ["minimal", "low", "medium", "high", "xhigh"], "description": "Optional depth override." }
            },
            "required": ["topic"],
            "additionalProperties": false
        }),
        move |args, progress| {
            let context = context.clone();
            async move { run_deep_research(args, context, progress).await }
        },
    ));
}

async fn run_deep_research(
    args: Value,
    context: DeepResearchContext,
    progress: ToolProgress,
) -> Result<String> {
    if !context.config.plugins.deep_research.enabled {
        bail!("deep_research plugin is disabled")
    }
    let topic = args
        .get("topic")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim()
        .to_string();
    if topic.is_empty() {
        bail!("topic is required")
    }
    let plugin = &context.config.plugins.deep_research;
    let sa_mode = ProgressMode::from_config(&context.config);
    let sa_enabled = context.config.plugins.deep_research.show_progress;
    let sa_progress = SubagentProgress::new(progress, sa_mode, sa_enabled);
    let depth = args
        .get("thinking_depth")
        .and_then(Value::as_str)
        .unwrap_or(&plugin.thinking_depth)
        .to_string();
    let max_revisions = if plugin.max_review_revisions == 0 {
        depth_default_revisions(&depth)
    } else {
        plugin.max_review_revisions
    };
    let max_tool_steps = if plugin.max_tool_steps_per_round == 0 {
        depth_default_tool_steps(&depth)
    } else {
        plugin.max_tool_steps_per_round
    };
    // 走 model_tiers.roles.deep_research 指定的档位池(未配置=主池)。
    let client = OpenAiCompatibleClient::from_aux_role(
        &context.config,
        &context.paths,
        crate::config::AuxRole::DeepResearch,
    )?
    .for_subagent_output(sa_mode == ProgressMode::Full)
    .with_request_scope("deep-research");
    let state = Arc::new(Mutex::new(ResearchState::default()));
    let mut draft = String::new();
    let mut review =
        json!({"accepted": false, "challenge": "首轮暂无审视意见", "revision_instructions": []});
    let mut iterations = 0usize;
    let mut stop_reason = "max_review_revisions_reached".to_string();
    sa_progress.phase(format!(
        "{}=\"{}\"",
        t("topic", "主题"),
        topic_title(&state, &topic)
    ));

    loop {
        let iteration = iterations + 1;
        if max_revisions != usize::MAX && iteration > max_revisions.saturating_add(1) {
            break;
        }
        iterations = iteration;
        sa_progress.phase(if is_zh() {
            format!("第 {iteration} 轮：沉思者起草")
        } else {
            format!("round {iteration}: thinker drafting")
        });
        let tools = research_tool_registry(&context, Arc::clone(&state));
        let prompt = thinker_prompt(&topic, iteration, &draft, &review, &state)?;
        let runner = SubagentRunner::new(
            client.clone(),
            THINKER_SYSTEM_PROMPT,
            tools,
            SubagentProgress::new(sa_progress.clone_inner(), sa_mode, sa_enabled),
        )
        .max_steps(max_tool_steps)
        .timeout_seconds(plugin.tool_call_timeout_seconds)
        .excluded_tools(&["deep_research", "task", "task_agent"]);
        let (thinker, sa_stats) = runner.run(&prompt).await?;
        merge_stats(&state, &sa_stats);
        if !thinker.content.trim().is_empty() {
            draft = thinker.content.trim().to_string();
        }
        if draft.is_empty() {
            stop_reason = "thinker_failed".to_string();
            sa_progress.phase(if is_zh() {
                "沉思者未能生成草稿"
            } else {
                "thinker failed to produce a draft"
            });
            break;
        }
        sa_progress.phase(if is_zh() {
            format!(
                "第 {iteration} 轮：草稿就绪，{chars} 字",
                chars = draft.chars().count()
            )
        } else {
            format!(
                "round {iteration}: draft ready chars={}",
                draft.chars().count()
            )
        });
        let review_prompt = reviewer_prompt(&topic, iteration, &draft, &state)?;
        sa_progress.phase(if is_zh() {
            format!("第 {iteration} 轮：审视者审查中")
        } else {
            format!("round {iteration}: reviewer checking")
        });
        let reviewer_system = REVIEWER_SYSTEM_PROMPT;
        let review_result = client
            .chat_stream(
                vec![
                    ChatMessage::system(reviewer_system),
                    ChatMessage::plain("user", review_prompt.clone()),
                ],
                Vec::new(),
                |chunk: ChatStreamChunk| {
                    if chunk.kind == ChatStreamKind::Reasoning {
                        sa_progress.reasoning(&chunk.text);
                    }
                    Ok(())
                },
            )
            .await?;
        state
            .lock()
            .expect("deep research state lock")
            .stats
            .add_usage_or_estimate(
                review_result.usage.as_ref(),
                &[reviewer_system, &review_prompt, &review_result.content],
            );
        review = parse_review(&review_result.content);
        if review
            .get("accepted")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            stop_reason = "accepted".to_string();
            sa_progress.phase(if is_zh() {
                format!("第 {iteration} 轮：通过")
            } else {
                format!("round {iteration}: accepted")
            });
            break;
        }
        sa_progress.phase(if is_zh() {
            format!(
                "第 {iteration} 轮：需修订 — {}",
                clip_inline(
                    review
                        .get("challenge")
                        .and_then(Value::as_str)
                        .unwrap_or("审视者要求修改"),
                    100
                )
            )
        } else {
            format!(
                "round {iteration}: revision requested - {}",
                clip_inline(
                    review
                        .get("challenge")
                        .and_then(Value::as_str)
                        .unwrap_or("reviewer requested changes"),
                    100
                )
            )
        });
    }

    sa_progress.phase(if is_zh() {
        "生成最终报告"
    } else {
        "finalizing report"
    });
    let mut final_answer = normalize_final_answer(&draft, &state)?;
    if plugin.max_final_answer_chars > 0
        && final_answer.chars().count() > plugin.max_final_answer_chars
    {
        final_answer = format!(
            "{}\n\n...[truncated to {} chars]",
            final_answer
                .chars()
                .take(plugin.max_final_answer_chars)
                .collect::<String>(),
            plugin.max_final_answer_chars
        );
    }
    let path = write_report(
        plugin,
        &context.paths,
        &topic,
        &final_answer,
        &state,
        &stop_reason,
        iterations,
        &state,
    )?;
    record_research_audit(&context, &topic, &final_answer, &state);
    let stats = public_stats(&state);
    sa_progress.phase(format!(
        "{} {} {} {} {}\n{} {}",
        t("tool calls", "工具调用"),
        stats["tool_calls"].as_u64().unwrap_or(0),
        t("times", "次"),
        t("token cost", "消耗词元"),
        format_token_count(
            stats["token_estimate"].as_u64().unwrap_or(0),
            !stats["token_estimate_is_actual"].as_bool().unwrap_or(false)
        ),
        t("result file", "结果文件"),
        path.display()
    ));
    Ok(serde_json::to_string_pretty(&json!({
        // thinker 空产出时报告体也是空的:标成 ok 会让模型把空报告当成
        // 研究结论直接交付用户。
        "ok": stop_reason != "thinker_failed",
        "kind": "deep_research",
        "topic": topic,
        "topic_title": topic_title(&state, &topic),
        "iterations_used": iterations,
        "stop_reason": stop_reason,
        "archive_path": path.display().to_string(),
        "final_answer": final_answer,
        "stats": stats,
        "sources": public_sources(&state)
    }))?)
}

fn research_tool_registry(
    context: &DeepResearchContext,
    state: Arc<Mutex<ResearchState>>,
) -> ToolRegistry {
    let mut registry = context.tools.clone();
    register_reference_tools(&mut registry, state);
    registry
}

/// Persists one aggregate audit session per research run, the same shape
/// `task` uses: a hidden `kind='subagent'` session hanging off the launching
/// session, carrying the run's merged usage. Without it a research run —
/// easily the most expensive thing a turn can do — spends its tokens entirely
/// outside the session's Σ. Provider/model are left unset because a run fans
/// out over several tiers. Best-effort; never fails the research itself.
///
/// Recorded at completion only: a run that dies mid-flight still loses its
/// accounting.
fn record_research_audit(
    context: &DeepResearchContext,
    topic: &str,
    final_answer: &str,
    state: &Arc<Mutex<ResearchState>>,
) {
    let (prompt_tokens, completion_tokens, total_tokens, cache_read_tokens) = {
        let state = state.lock().expect("deep research state lock");
        (
            state.stats.prompt_tokens as i64,
            state.stats.completion_tokens as i64,
            state.stats.total_tokens.max(state.stats.token_estimate) as i64,
            state.stats.cache_read_tokens as i64,
        )
    };
    if total_tokens == 0 {
        return;
    }
    let outcome = (|| -> anyhow::Result<()> {
        let store = crate::state::StateStore::new(&context.paths)?;
        let parent = crate::tools::workspace::try_session().map(|session| session.to_string());
        let persona = context.config.active_persona_scope();
        let name: String = topic.chars().take(40).collect();
        let record = store.create_session(&persona, &name, "subagent", parent.as_deref())?;
        let pinned = store.pinned(&record.session_id);
        let turn_id = format!(
            "dra_{}_{:08x}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|duration| duration.as_millis())
                .unwrap_or(0),
            rand::random::<u32>()
        );
        pinned.start_turn(&turn_id, topic, std::process::id())?;
        pinned.complete_turn(&turn_id, final_answer, None)?;
        store.record_subagent_usage(
            &record.session_id,
            None,
            None,
            None,
            prompt_tokens,
            completion_tokens,
            total_tokens,
            cache_read_tokens,
        )
    })();
    if let Err(error) = outcome {
        tracing::warn!(
            error = %error,
            "{}",
            crate::i18n::text(
                "failed to record deep research audit session",
                "记录深度研究审计会话失败"
            )
        );
    }
}

fn public_stats(state: &Arc<Mutex<ResearchState>>) -> Value {
    let state = state.lock().expect("deep research state lock");
    json!({
        "tool_calls": state.stats.tool_calls,
        "tool_ok": state.stats.tool_ok,
        "tool_errors": state.stats.tool_errors,
        "prompt_tokens": state.stats.prompt_tokens,
        "completion_tokens": state.stats.completion_tokens,
        "total_tokens": state.stats.total_tokens,
        "cache_read_tokens": state.stats.cache_read_tokens,
        "token_estimate": state.stats.token_estimate,
        "token_estimate_method": token_estimate_method_label(state.stats.token_estimate_method),
        "token_estimate_is_actual": state.stats.token_estimate_method == TokenEstimateMethod::ProviderUsage,
        "references": state.references.len(),
    })
}

fn token_estimate_method_label(method: TokenEstimateMethod) -> &'static str {
    match method {
        TokenEstimateMethod::ProviderUsage => "provider_usage",
        TokenEstimateMethod::ProviderUsagePlusEstimate => "provider_usage_plus_estimate",
        TokenEstimateMethod::RoughCharEstimate | TokenEstimateMethod::None => "rough_char_estimate",
    }
}

fn topic_title(state: &Arc<Mutex<ResearchState>>, topic: &str) -> String {
    let state = state.lock().expect("deep research state lock");
    if state.topic_title.trim().is_empty() {
        sanitize_title(topic, 40)
    } else {
        state.topic_title.clone()
    }
}

fn normalized_reference_kind(value: &str) -> String {
    match value.trim().to_ascii_lowercase().as_str() {
        "r" | "record" | "deep_record" => "R".to_string(),
        "k" | "knowledge" => "K".to_string(),
        _ => "W".to_string(),
    }
}

fn depth_default_revisions(depth: &str) -> usize {
    match depth {
        "minimal" => 1,
        "low" => 2,
        "medium" => 3,
        "xhigh" => usize::MAX,
        _ => 3,
    }
}

fn depth_default_tool_steps(depth: &str) -> usize {
    match depth {
        "minimal" => 8,
        "low" => 14,
        "medium" => 24,
        "xhigh" => 0,
        _ => 40,
    }
}

fn estimate_tokens(texts: &[&str]) -> u64 {
    let combined: String = texts.iter().copied().collect();
    if combined.is_empty() {
        0
    } else {
        crate::agent::overflow::estimate_tokens(&combined) as u64
    }
}

fn sanitize_title(value: &str, max_chars: usize) -> String {
    let title = value.split_whitespace().collect::<Vec<_>>().join(" ");
    let title = title
        .trim_matches(|ch: char| ch == '#' || ch == '*' || ch == '`')
        .trim();
    let clipped = title.chars().take(max_chars).collect::<String>();
    if clipped.trim().is_empty() {
        "深度研究".to_string()
    } else {
        clipped
    }
}

fn sanitize_filename(value: &str) -> String {
    let mut out = String::new();
    for ch in value.chars() {
        if ch.is_ascii_alphanumeric()
            || matches!(ch, '-' | '_')
            || ('\u{4e00}'..='\u{9fff}').contains(&ch)
        {
            out.push(ch);
        } else if ch.is_whitespace() {
            out.push('-');
        }
    }
    if out.is_empty() {
        "deep-research".to_string()
    } else {
        out.chars().take(80).collect()
    }
}

fn unique_report_filename(output_dir: &PathBuf, title: &str) -> String {
    let stem = sanitize_filename(&strip_title_date_prefix(title));
    let suffix = format!(
        "{}_{}",
        report_date_suffix(title).unwrap_or_else(|| Local::now().format("%Y%m%d").to_string()),
        Local::now().format("%H%M")
    );
    let filename = format!("{stem}_{suffix}.md");
    if !output_dir.join(&filename).exists() {
        return filename;
    }
    let seconds = Local::now().format("%S").to_string();
    format!("{stem}_{suffix}{seconds}.md")
}

fn report_date_suffix(value: &str) -> Option<String> {
    chinese_date_suffix(value).or_else(|| ascii_date_suffix(value))
}

fn chinese_date_suffix(value: &str) -> Option<String> {
    let chars = value.chars().collect::<Vec<_>>();
    let year_index = chars.iter().position(|ch| *ch == '年')?;
    let month_rel = chars[year_index + 1..].iter().position(|ch| *ch == '月')?;
    let month_index = year_index + 1 + month_rel;
    let day_rel = chars[month_index + 1..]
        .iter()
        .position(|ch| *ch == '日' || *ch == '号')?;
    let day_index = month_index + 1 + day_rel;
    if year_index != 4 || !chars[..year_index].iter().all(|ch| ch.is_ascii_digit()) {
        return None;
    }
    let year = chars[..year_index].iter().collect::<String>();
    let month = chars[year_index + 1..month_index]
        .iter()
        .collect::<String>();
    let day = chars[month_index + 1..day_index].iter().collect::<String>();
    if month.is_empty()
        || day.is_empty()
        || !month.chars().all(|ch| ch.is_ascii_digit())
        || !day.chars().all(|ch| ch.is_ascii_digit())
    {
        return None;
    }
    Some(format!("{year}{:0>2}{:0>2}", month, day))
}

fn ascii_date_suffix(value: &str) -> Option<String> {
    let chars = value.chars().collect::<Vec<_>>();
    for start in 0..chars.len().saturating_sub(9) {
        if chars[start..start + 4].iter().all(|ch| ch.is_ascii_digit())
            && matches!(chars[start + 4], '-' | '/' | '.')
            && chars[start + 5..start + 7]
                .iter()
                .all(|ch| ch.is_ascii_digit())
            && matches!(chars[start + 7], '-' | '/' | '.')
            && chars[start + 8..start + 10]
                .iter()
                .all(|ch| ch.is_ascii_digit())
        {
            let year = chars[start..start + 4].iter().collect::<String>();
            let month = chars[start + 5..start + 7].iter().collect::<String>();
            let day = chars[start + 8..start + 10].iter().collect::<String>();
            return Some(format!("{year}{month}{day}"));
        }
    }
    None
}

fn strip_title_date_prefix(value: &str) -> String {
    let mut title = value.trim().to_string();
    title = strip_leading_ascii_date(&title);
    title = strip_leading_chinese_date(&title);
    title = strip_leading_weekday(&title);
    let title = title.trim_matches(|ch: char| {
        ch.is_whitespace() || matches!(ch, '-' | '_' | '，' | ',' | '：' | ':' | '|' | '｜')
    });
    if title.is_empty() {
        value.trim().to_string()
    } else {
        title.to_string()
    }
}

fn strip_leading_ascii_date(value: &str) -> String {
    let chars = value.chars().collect::<Vec<_>>();
    if chars.len() >= 10
        && chars[0..4].iter().all(|ch| ch.is_ascii_digit())
        && matches!(chars[4], '-' | '/' | '.')
        && chars[5..7].iter().all(|ch| ch.is_ascii_digit())
        && matches!(chars[7], '-' | '/' | '.')
        && chars[8..10].iter().all(|ch| ch.is_ascii_digit())
    {
        chars[10..].iter().collect()
    } else {
        value.to_string()
    }
}

fn strip_leading_chinese_date(value: &str) -> String {
    let chars = value.chars().collect::<Vec<_>>();
    let Some(year_index) = chars.iter().position(|ch| *ch == '年') else {
        return value.to_string();
    };
    let Some(month_rel) = chars[year_index + 1..].iter().position(|ch| *ch == '月') else {
        return value.to_string();
    };
    let month_index = year_index + 1 + month_rel;
    let Some(day_rel) = chars[month_index + 1..]
        .iter()
        .position(|ch| *ch == '日' || *ch == '号')
    else {
        return value.to_string();
    };
    let day_index = month_index + 1 + day_rel;
    if year_index == 4
        && chars[..year_index].iter().all(|ch| ch.is_ascii_digit())
        && chars[year_index + 1..month_index]
            .iter()
            .all(|ch| ch.is_ascii_digit())
        && chars[month_index + 1..day_index]
            .iter()
            .all(|ch| ch.is_ascii_digit())
    {
        chars[day_index + 1..].iter().collect()
    } else {
        value.to_string()
    }
}

fn strip_leading_weekday(value: &str) -> String {
    let weekdays = [
        "星期一",
        "星期二",
        "星期三",
        "星期四",
        "星期五",
        "星期六",
        "星期日",
        "星期天",
        "周一",
        "周二",
        "周三",
        "周四",
        "周五",
        "周六",
        "周日",
        "周天",
    ];
    let mut title = value.trim_start();
    loop {
        let Some(weekday) = weekdays.iter().find(|weekday| title.starts_with(**weekday)) else {
            break;
        };
        title = title[weekday.len()..].trim_start();
    }
    title.to_string()
}

fn expand_output_dir(value: &str, paths: &MiyuPaths) -> PathBuf {
    let value = value.trim();
    if let Some(rest) = value.strip_prefix("~/") {
        if let Some(home) = directories::BaseDirs::new().map(|dirs| dirs.home_dir().to_path_buf()) {
            return home.join(rest);
        }
    }
    if value.is_empty() {
        return paths.config_dir.join("deep-research");
    }
    PathBuf::from(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_leading_chinese_date_and_weekday_from_title() {
        assert_eq!(
            strip_title_date_prefix("2026年6月29日周一夏季早餐推荐"),
            "夏季早餐推荐"
        );
        assert_eq!(
            strip_title_date_prefix("2026年06月29日 星期一：夏季早餐推荐"),
            "夏季早餐推荐"
        );
    }

    #[test]
    fn extracts_report_date_suffix_from_title() {
        assert_eq!(
            report_date_suffix("2026年6月29日周一夏季早餐推荐").as_deref(),
            Some("20260629")
        );
        assert_eq!(
            report_date_suffix("夏季早餐推荐 2026-06-29").as_deref(),
            Some("20260629")
        );
    }
}
