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

const THINKER_SYSTEM_PROMPT: &str = r#"你是深度研究系统中的“沉思者”。
你的任务是理解用户命题，主动调用可用工具查证，形成可发送给用户的 Markdown 草稿。

工作原则：
1. 优先基于题面和本地资料；需要时使用 web_search 和 web_fetch 联网查证。
2. 关键事实、技术判断、推荐理由和核心观点应有来源或依据。
3. 需要引用资料时，先调用 register_deep_research_reference 注册参考资料，再在正文中使用返回的 [R数字]/[K数字]/[W数字]。
4. 第一轮必须调用 register_deep_research_topic_title 注册 4-40 字短标题。
5. 不编造来源；资料冲突时说明冲突和取舍；无法查证的点写入“不确定点”。
6. 输出可直接发送给用户的 Markdown 正文，不输出内部 JSON，不输出“参考资料”章节。
7. 不使用 emoji 或装饰性图标。
"#;

const REVIEWER_SYSTEM_PROMPT: &str = r#"你是深度研究系统中的“审视者”。
你只审查沉思者草稿，不替用户回答。请严格输出 JSON。

审查重点：
1. 是否覆盖用户问题的关键对象、维度、限制和输出要求。
2. 关键事实和观点是否有已注册 R/K/W 引用支撑。
3. 是否存在严重逻辑错误、前后矛盾、结论超出证据。
4. 是否存在影响结论的数据缺口，却没有说明查证失败或列入不确定点。

输出格式：
{
  "accepted": true/false,
  "challenge": "主要质疑或通过理由",
  "revision_instructions": ["需要修正的事项"]
}
"#;

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

#[derive(Default)]
struct ResearchStats {
    tool_calls: usize,
    tool_ok: usize,
    tool_errors: usize,
    prompt_tokens: u64,
    completion_tokens: u64,
    total_tokens: u64,
    token_estimate: u64,
    token_estimate_method: TokenEstimateMethod,
}

#[derive(Clone, Copy, Default, Eq, PartialEq)]
enum TokenEstimateMethod {
    #[default]
    None,
    ProviderUsage,
    ProviderUsagePlusEstimate,
    RoughCharEstimate,
}

impl ResearchStats {
    fn add_usage_or_estimate(&mut self, usage: Option<&Usage>, texts: &[&str]) {
        if let Some(usage) = usage {
            let total_tokens = usage.effective_total_tokens();
            if total_tokens > 0 {
                self.prompt_tokens += usage.prompt_tokens;
                self.completion_tokens += usage.completion_tokens;
                self.total_tokens += total_tokens;
                self.token_estimate += total_tokens;
                self.token_estimate_method = match self.token_estimate_method {
                    TokenEstimateMethod::None | TokenEstimateMethod::ProviderUsage => {
                        TokenEstimateMethod::ProviderUsage
                    }
                    _ => TokenEstimateMethod::ProviderUsagePlusEstimate,
                };
                return;
            }
        }
        let estimate = estimate_tokens(texts);
        self.token_estimate += estimate;
        self.token_estimate_method = match self.token_estimate_method {
            TokenEstimateMethod::None | TokenEstimateMethod::RoughCharEstimate => {
                TokenEstimateMethod::RoughCharEstimate
            }
            _ => TokenEstimateMethod::ProviderUsagePlusEstimate,
        };
    }
}

#[derive(Default)]
struct ReferenceCounters {
    record: usize,
    knowledge: usize,
    web: usize,
}

#[derive(Clone)]
struct Reference {
    marker: String,
    kind: String,
    title: String,
    url: String,
    path: String,
    snippet: String,
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
    let client = OpenAiCompatibleClient::from_config(&context.config, &context.paths)?
        .for_subagent_output(sa_mode == ProgressMode::Full);
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
        "ok": true,
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

fn register_reference_tools(registry: &mut ToolRegistry, state: Arc<Mutex<ResearchState>>) {
    let title_state = Arc::clone(&state);
    registry.register(ToolSpec::new(
        "register_deep_research_topic_title",
        "Register a concise title for this deep research task.",
        json!({"type":"object","properties":{"topic_title":{"type":"string"},"reason":{"type":"string"}},"required":["topic_title"],"additionalProperties":false}),
        move |args| {
            let title_state = Arc::clone(&title_state);
            async move {
                let title = args.get("topic_title").and_then(Value::as_str).unwrap_or_default();
                let title = sanitize_title(title, 40);
                let mut state = title_state.lock().expect("deep research state lock");
                state.topic_title = title.clone();
                Ok(json!({"ok": true, "topic_title": title}).to_string())
            }
        },
    ));
    let ref_state = Arc::clone(&state);
    registry.register(ToolSpec::new(
        "register_deep_research_reference",
        "Register a source and receive a stable citation marker such as [W1].",
        json!({"type":"object","properties":{"reference_type":{"type":"string","enum":["R","K","W","record","knowledge","web"]},"title":{"type":"string"},"url":{"type":"string"},"path":{"type":"string"},"snippet":{"type":"string"}},"required":["reference_type","title"],"additionalProperties":false}),
        move |args| {
            let ref_state = Arc::clone(&ref_state);
            async move {
                let kind = normalized_reference_kind(args.get("reference_type").and_then(Value::as_str).unwrap_or("W"));
                let title = args.get("title").and_then(Value::as_str).unwrap_or("Untitled").trim().to_string();
                let url = args.get("url").and_then(Value::as_str).unwrap_or_default().trim().to_string();
                let path = args.get("path").and_then(Value::as_str).unwrap_or_default().trim().to_string();
                let snippet = args.get("snippet").and_then(Value::as_str).unwrap_or_default().trim().to_string();
                let mut state = ref_state.lock().expect("deep research state lock");
                let number = match kind.as_str() {
                    "R" => { state.counters.record += 1; state.counters.record }
                    "K" => { state.counters.knowledge += 1; state.counters.knowledge }
                    _ => { state.counters.web += 1; state.counters.web }
                };
                let marker = format!("{kind}{number}");
                state.references.push(Reference { marker: marker.clone(), kind, title, url, path, snippet });
                Ok(json!({"ok": true, "ref": marker, "citation": format!("[{marker}]")}).to_string())
            }
        },
    ));
    registry.register(ToolSpec::new(
        "remove_deep_research_reference",
        "Remove a registered source by marker.",
        json!({"type":"object","properties":{"ref":{"type":"string"},"reason":{"type":"string"}},"required":["ref"],"additionalProperties":false}),
        move |args| {
            let state = Arc::clone(&state);
            async move {
                let marker = args.get("ref").and_then(Value::as_str).unwrap_or_default().trim().trim_matches(&['[', ']'][..]).to_string();
                let mut state = state.lock().expect("deep research state lock");
                let old_len = state.references.len();
                state.references.retain(|item| item.marker != marker);
                Ok(json!({"ok": old_len != state.references.len(), "ref": marker}).to_string())
            }
        },
    ));
}

fn merge_stats(
    state: &Arc<Mutex<ResearchState>>,
    sa_stats: &super::subagent_runner::SubagentStats,
) {
    use super::subagent_runner::TokenEstimateMethod as SaTEM;
    let mut state = state.lock().expect("deep research state lock");
    state.stats.tool_calls += sa_stats.tool_calls;
    state.stats.tool_ok += sa_stats.tool_ok;
    state.stats.tool_errors += sa_stats.tool_errors;
    state.stats.prompt_tokens += sa_stats.prompt_tokens;
    state.stats.completion_tokens += sa_stats.completion_tokens;
    state.stats.total_tokens += sa_stats.total_tokens;
    state.stats.token_estimate += sa_stats.token_estimate;
    let has_provider = state.stats.token_estimate_method == TokenEstimateMethod::ProviderUsage
        || sa_stats.token_estimate_method == SaTEM::ProviderUsage;
    let has_estimate = state.stats.token_estimate_method == TokenEstimateMethod::RoughCharEstimate
        || state.stats.token_estimate_method == TokenEstimateMethod::None
        || sa_stats.token_estimate_method == SaTEM::RoughCharEstimate
        || sa_stats.token_estimate_method == SaTEM::None;
    state.stats.token_estimate_method = if has_provider && !has_estimate {
        TokenEstimateMethod::ProviderUsage
    } else if has_provider {
        TokenEstimateMethod::ProviderUsagePlusEstimate
    } else {
        TokenEstimateMethod::RoughCharEstimate
    };
}

fn thinker_prompt(
    topic: &str,
    iteration: usize,
    draft: &str,
    review: &Value,
    state: &Arc<Mutex<ResearchState>>,
) -> Result<String> {
    Ok(format!(
        "请完成第 {iteration} 轮深度研究。\n\n用户命题：\n{topic}\n\n上一轮草稿：\n{}\n\n上一轮审视意见：\n{}\n\n当前参考资料注册表：\n{}\n\n要求：结论先行，必要时调用工具查证；需要引用时先注册参考资料，并在正文中使用 [R1]/[K1]/[W1] 标注。不要输出参考资料章节。",
        if draft.trim().is_empty() { "（无）" } else { draft },
        serde_json::to_string_pretty(review)?,
        reference_registry_json(state)?,
    ))
}

fn reviewer_prompt(
    topic: &str,
    iteration: usize,
    draft: &str,
    state: &Arc<Mutex<ResearchState>>,
) -> Result<String> {
    Ok(format!(
        "请审查第 {iteration} 轮草案。\n\n用户命题：\n{topic}\n\n草案：\n{draft}\n\n参考资料注册表：\n{}\n\n若可以发送，accepted=true；否则列出具体 revision_instructions。",
        reference_registry_json(state)?,
    ))
}

fn reference_registry_json(state: &Arc<Mutex<ResearchState>>) -> Result<String> {
    let state = state.lock().expect("deep research state lock");
    let refs = state.references.iter().map(|item| json!({"ref": item.marker, "type": item.kind, "title": item.title, "url": item.url, "path": item.path, "snippet": item.snippet})).collect::<Vec<_>>();
    Ok(serde_json::to_string_pretty(&refs)?)
}

fn parse_review(content: &str) -> Value {
    parse_json_object(content).unwrap_or_else(|| {
        json!({"accepted": true, "challenge": "reviewer returned non-JSON feedback; accept current draft to avoid repeated research", "revision_instructions": [], "review_text": content.trim()})
    })
}

fn parse_json_object(content: &str) -> Option<Value> {
    let trimmed = content.trim();
    serde_json::from_str(trimmed)
        .ok()
        .or_else(|| extract_json_object(trimmed).and_then(|json| serde_json::from_str(json).ok()))
}

fn extract_json_object(content: &str) -> Option<&str> {
    let start = content.find('{')?;
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    for (offset, ch) in content[start..].char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        if ch == '\\' && in_string {
            escaped = true;
            continue;
        }
        if ch == '"' {
            in_string = !in_string;
            continue;
        }
        if in_string {
            continue;
        }
        match ch {
            '{' => depth += 1,
            '}' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    let end = start + offset + ch.len_utf8();
                    return Some(&content[start..end]);
                }
            }
            _ => {}
        }
    }
    None
}

fn normalize_final_answer(draft: &str, state: &Arc<Mutex<ResearchState>>) -> Result<String> {
    let diagnostics = reference_diagnostics(draft, state);
    let mut answer = strip_reference_section(draft).trim().to_string();
    if !diagnostics.is_empty() {
        answer.push_str("\n\n## 引用校验提示\n");
        for item in diagnostics {
            answer.push_str(&format!("- {item}\n"));
        }
    }
    answer.push_str("\n\n## 参考资料\n");
    let state = state.lock().expect("deep research state lock");
    if state.references.is_empty() {
        answer.push_str("- 本次研究没有注册外部参考资料。\n");
    } else {
        for item in &state.references {
            let source = if !item.url.is_empty() {
                format!("[{}]({})", item.title, item.url)
            } else if !item.path.is_empty() {
                format!("{} ({})", item.title, item.path)
            } else {
                item.title.clone()
            };
            answer.push_str(&format!("- [{}] {}\n", item.marker, source));
        }
    }
    Ok(answer)
}

fn reference_diagnostics(draft: &str, state: &Arc<Mutex<ResearchState>>) -> Vec<String> {
    let state = state.lock().expect("deep research state lock");
    let known = state
        .references
        .iter()
        .map(|item| item.marker.as_str())
        .collect::<Vec<_>>();
    let mut diagnostics = Vec::new();
    for marker in extract_markers(draft) {
        if !known.iter().any(|item| *item == marker) {
            diagnostics.push(format!("正文引用了未注册来源 [{marker}]。"));
        }
    }
    if draft.contains("http://") || draft.contains("https://") {
        diagnostics.push("正文中存在裸 URL；建议注册为 W 类型参考资料后使用编号引用。".to_string());
    }
    diagnostics
}

fn extract_markers(value: &str) -> Vec<String> {
    let mut out = Vec::new();
    for part in value.split('[').skip(1) {
        let Some(end) = part.find(']') else { continue };
        let marker = &part[..end];
        if marker.len() >= 2
            && matches!(marker.as_bytes()[0], b'R' | b'K' | b'W')
            && marker[1..].chars().all(|ch| ch.is_ascii_digit())
        {
            out.push(marker.to_string());
        }
    }
    out
}

fn strip_reference_section(value: &str) -> String {
    for heading in ["\n## 参考资料", "\n# 参考资料"] {
        if let Some(index) = value.find(heading) {
            return value[..index].to_string();
        }
    }
    value.to_string()
}

fn write_report(
    plugin: &DeepResearchPluginConfig,
    paths: &MiyuPaths,
    topic: &str,
    final_answer: &str,
    state: &Arc<Mutex<ResearchState>>,
    stop_reason: &str,
    iterations: usize,
    state_for_stats: &Arc<Mutex<ResearchState>>,
) -> Result<PathBuf> {
    let output_dir = expand_output_dir(&plugin.output_dir, paths);
    std::fs::create_dir_all(&output_dir)?;
    let title = topic_title(state, topic);
    let filename = unique_report_filename(&output_dir, &title);
    let path = output_dir.join(filename);
    let stats = public_stats(state_for_stats);
    let report = format!(
        "---\ntopic: {}\ntopic_title: {}\ncreated_at: {}\nstop_reason: {}\niterations_used: {}\ntool_calls: {}\ntool_ok: {}\ntool_errors: {}\ntoken_estimate: {}\ntoken_estimate_method: {}\ntoken_estimate_is_actual: {}\n---\n\n{}\n",
        topic,
        title,
        Local::now().to_rfc3339(),
        stop_reason,
        iterations,
        stats["tool_calls"].as_u64().unwrap_or(0),
        stats["tool_ok"].as_u64().unwrap_or(0),
        stats["tool_errors"].as_u64().unwrap_or(0),
        stats["token_estimate"].as_u64().unwrap_or(0),
        stats["token_estimate_method"].as_str().unwrap_or("rough_char_estimate"),
        stats["token_estimate_is_actual"].as_bool().unwrap_or(false),
        final_answer.trim_end()
    );
    std::fs::write(&path, report)?;
    Ok(path)
}

fn public_sources(state: &Arc<Mutex<ResearchState>>) -> Vec<Value> {
    let state = state.lock().expect("deep research state lock");
    state.references.iter().map(|item| json!({"ref": item.marker, "type": item.kind, "title": item.title, "url": item.url, "path": item.path})).collect()
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
