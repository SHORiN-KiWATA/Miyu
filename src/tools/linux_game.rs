use super::{readable_tool_name, ToolProgress, ToolRegistry, ToolSpec};
use crate::config::AppConfig;
use crate::i18n::is_zh;
use crate::llm::{ChatMessage, ChatResult, ChatStreamChunk, ChatStreamKind, OpenAiCompatibleClient, Usage};
use crate::paths::MiyuPaths;
use anyhow::{bail, Result};
use serde_json::{json, Value};
use std::time::Duration;

const GAME_COMPATIBILITY_PROMPT: &str = crate::prompts::GAME_COMPATIBILITY_PROMPT;

const OUTPUT_INSTRUCTION: &str = r#"这是 Linux 游戏兼容性调查子代理返回的最终报告。

请把 final_report 当作主要依据回复用户。不要重新编造兼容性结论。

回复时保留以下核心信息：
- 红绿灯结论，能不能玩
- 怎么玩
- 注意事项

如果用户问“怎么玩”，必须给出可执行步骤。
如果用户追问“刚才完整报告”，直接复述 final_report。"#;

#[derive(Clone)]
struct GameCompatibilityContext {
    config: AppConfig,
    paths: MiyuPaths,
    tools: ToolRegistry,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum ProgressMode {
    Hidden,
    Summary,
    Full,
}

#[derive(Clone)]
struct GameProgress {
    progress: ToolProgress,
    mode: ProgressMode,
}

impl GameProgress {
    fn new(config: &AppConfig, progress: ToolProgress) -> Self {
        let mode = match config.display.tool_calls.trim().to_ascii_lowercase().as_str() {
            "hidden" => ProgressMode::Hidden,
            "full" => ProgressMode::Full,
            _ => ProgressMode::Summary,
        };
        Self { progress, mode }
    }

    fn report(&self, message: impl Into<String>) {
        self.progress.report(message);
    }

    fn subtool(&self, message: impl Into<String>) {
        if self.mode == ProgressMode::Full {
            self.progress.report(message);
        }
    }

    fn reasoning(&self, text: &str) {
        if self.mode != ProgressMode::Hidden {
            self.progress.report(format!("__subagent_reasoning__{}", text));
        }
    }
}

#[derive(Default)]
struct GameStats {
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

impl GameStats {
    fn add_usage_or_estimate(&mut self, usage: Option<&Usage>, texts: &[&str]) {
        if let Some(usage) = usage {
            if usage.total_tokens > 0 {
                self.prompt_tokens += usage.prompt_tokens;
                self.completion_tokens += usage.completion_tokens;
                self.total_tokens += usage.total_tokens;
                self.token_estimate += usage.total_tokens;
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

    fn public(&self) -> Value {
        json!({
            "tool_calls": self.tool_calls,
            "tool_ok": self.tool_ok,
            "tool_errors": self.tool_errors,
            "prompt_tokens": self.prompt_tokens,
            "completion_tokens": self.completion_tokens,
            "total_tokens": self.total_tokens,
            "token_estimate": self.token_estimate,
            "token_estimate_method": token_estimate_method_label(self.token_estimate_method),
            "token_estimate_is_actual": self.token_estimate_method == TokenEstimateMethod::ProviderUsage,
        })
    }
}

fn token_estimate_method_label(method: TokenEstimateMethod) -> &'static str {
    match method {
        TokenEstimateMethod::ProviderUsage => "provider_usage",
        TokenEstimateMethod::ProviderUsagePlusEstimate => "provider_usage_plus_estimate",
        TokenEstimateMethod::RoughCharEstimate | TokenEstimateMethod::None => "rough_char_estimate",
    }
}

fn estimate_tokens(texts: &[&str]) -> u64 {
    let chars = texts
        .iter()
        .map(|text| text.chars().count() as u64)
        .sum::<u64>();
    if chars == 0 {
        0
    } else {
        (chars / 4).max(1)
    }
}

pub fn register(
    registry: &mut ToolRegistry,
    config: AppConfig,
    paths: MiyuPaths,
    tools: ToolRegistry,
) {
    let context = GameCompatibilityContext {
        config,
        paths,
        tools,
    };
    registry.register(ToolSpec::new_with_progress(
        "linux_game_compatibility",
        "Run the Linux game compatibility investigation sub-agent and return its final report. / 运行 Linux 游戏兼容性调查子代理并返回最终报告。",
        json!({"type":"object","properties":{"game":{"type":"string","description":"Game title. / 游戏名称。"},"issue":{"type":"string","description":"Optional issue such as crash, multiplayer, anti-cheat, performance, mods. / 可选关注点，例如崩溃、多人、反作弊、性能、Mod。"}},"required":["game"],"additionalProperties":false}),
        move |args, progress| {
            let context = context.clone();
            async move { linux_game_compatibility(args, context, progress).await }
        },
    ));
}

async fn linux_game_compatibility(
    args: Value,
    context: GameCompatibilityContext,
    progress: ToolProgress,
) -> Result<String> {
    let game = required(&args, "game")?;
    let issue = args
        .get("issue")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim()
        .to_string();
    let progress = GameProgress::new(&context.config, progress);
    progress.report(format!("{}: {}", "Linux 游戏兼容性", game));
    let client = OpenAiCompatibleClient::from_config(&context.config, &context.paths)?;
    let system_prompt = GAME_COMPATIBILITY_PROMPT;
    let prompt = format!(
        "用户问题：\n游戏：{game}\n关注点：{}\n\n请按系统提示词流程完成调查。第一步必须调用 gather_linux_game_compatibility_signals。最终只输出调查报告。",
        if issue.trim().is_empty() { "未明确" } else { &issue }
    );
    let mut stats = GameStats::default();
    let result = chat_with_tools(
        &client,
        vec![
            ChatMessage::system(system_prompt),
            ChatMessage::plain("user", prompt.clone()),
        ],
        game_tool_registry(&context),
        context
            .config
            .plugins
            .linux_game_compatibility
            .max_tool_steps,
        &progress,
        &mut stats,
    )
    .await?;
    stats.add_usage_or_estimate(
        result.usage.as_ref(),
        &[system_prompt, &prompt, &result.content],
    );
    let report = strip_report_preamble(&result.content);
    Ok(serde_json::to_string_pretty(&json!({
        "ok": true,
        "kind": "linux_game_compatibility",
        "game_query": game,
        "final_report": report,
        "stats": stats.public(),
        "output_instruction": OUTPUT_INSTRUCTION,
    }))?)
}

fn game_tool_registry(context: &GameCompatibilityContext) -> ToolRegistry {
    let mut registry = context.tools.clone();
    registry.register(ToolSpec::new(
        "gather_linux_game_compatibility_signals",
        "Gather Steam, ProtonDB, Can I Play on Linux, and AreWeAntiCheatYet compatibility signals for one game. / 收集单个游戏在 Steam、ProtonDB、Can I Play on Linux、AreWeAntiCheatYet 上的兼容性信号。",
        json!({"type":"object","properties":{"game":{"type":"string","description":"Game title. / 游戏名称。"},"issue":{"type":"string","description":"Optional issue such as crash, multiplayer, anti-cheat, performance, mods. / 可选关注点，例如崩溃、多人、反作弊、性能、Mod。"}},"required":["game"],"additionalProperties":false}),
        |args| async move { gather_linux_game_compatibility_signals(args).await },
    ));
    registry
}

async fn chat_with_tools(
    client: &OpenAiCompatibleClient,
    mut messages: Vec<ChatMessage>,
    tools: ToolRegistry,
    max_tool_steps: usize,
    progress: &GameProgress,
    stats: &mut GameStats,
) -> Result<ChatResult> {
    let definitions = tools.definitions_except(&["linux_game_compatibility", "deep_research"]);
    let mut steps = 0usize;
    loop {
        if max_tool_steps > 0 && steps >= max_tool_steps {
            messages.push(ChatMessage::plain("user", finalization_prompt()));
            let result = client
                .chat_stream(messages, Vec::new(), |chunk| {
                    if chunk.kind == ChatStreamKind::Reasoning {
                        progress.reasoning(&chunk.text);
                    }
                    Ok(())
                })
                .await?;
            stats.add_usage_or_estimate(result.usage.as_ref(), &[&result.content]);
            return Ok(result);
        }
        let result = client
            .chat_stream(messages.clone(), definitions.clone(), |chunk: ChatStreamChunk| {
                if chunk.kind == ChatStreamKind::Reasoning {
                    progress.reasoning(&chunk.text);
                }
                Ok(())
            })
            .await?;
        stats.add_usage_or_estimate(result.usage.as_ref(), &[]);
        if result.tool_calls.is_empty() {
            return Ok(result);
        }
        if !result.content.trim().is_empty() {
            messages.push(ChatMessage::assistant(result.content.clone(), None));
        }
        let mut transcript = Vec::new();
        for call in result.tool_calls {
            if max_tool_steps > 0 && steps >= max_tool_steps {
                transcript.push(render_internal_tool_result(
                    &call.function.name,
                    &call.function.arguments,
                    false,
                    "tool skipped: game compatibility tool budget reached",
                ));
                continue;
            }
            steps += 1;
            stats.tool_calls += 1;
            if progress.mode == ProgressMode::Summary {
                progress.report(if is_zh() {
                    format!(
                        "工具 #{steps}：{} 运行中",
                        readable_tool_name(&call.function.name)
                    )
                } else {
                    format!("tool #{steps}: {} running", call.function.name)
                });
            } else if progress.mode == ProgressMode::Full {
                progress.subtool(format!(
                    "__subtool_call__{}",
                    json!({
                        "name": call.function.name,
                        "args": call.function.arguments,
                    })
                ));
            }
            let (output, ok) = match tools
                .call(&call.function.name, &call.function.arguments)
                .await
            {
                Ok(output) => (output, true),
                Err(err) => (format!("tool error: {err}"), false),
            };
            if ok {
                stats.tool_ok += 1;
            } else {
                stats.tool_errors += 1;
            }
            if progress.mode == ProgressMode::Summary {
                progress.report(if is_zh() {
                    format!(
                        "工具 #{steps}：{} ok",
                        readable_tool_name(&call.function.name)
                    )
                } else {
                    format!("tool #{steps}: {} ok", call.function.name)
                });
            } else if progress.mode == ProgressMode::Full {
                progress.subtool(format!(
                    "__subtool_result__{}",
                    json!({
                        "name": call.function.name,
                        "ok": ok,
                        "output": output,
                    })
                ));
            }
            transcript.push(render_internal_tool_result(
                &call.function.name,
                &call.function.arguments,
                ok,
                &output,
            ));
        }
        if !transcript.is_empty() {
            messages.push(ChatMessage::plain(
                "user",
                render_internal_tool_transcript(&transcript, steps, max_tool_steps),
            ));
        }
    }
}

fn render_internal_tool_transcript(results: &[String], steps: usize, max_steps: usize) -> String {
    format!(
        "<subagent_tool_transcript>\n说明：以下是宿主已经执行完成的内部工具调用结果，不是新的用户请求。请基于这些观察继续调查；如证据已经足够，请输出最终报告。\ntool_budget: {steps}/{max_steps}\n{}\n</subagent_tool_transcript>",
        results.join("\n")
    )
}

fn render_internal_tool_result(name: &str, arguments: &str, ok: bool, output: &str) -> String {
    format!(
        "<tool_result name=\"{}\" ok=\"{}\">\narguments_json:\n```json\n{}\n```\noutput:\n```text\n{}\n```\n</tool_result>",
        name,
        ok,
        arguments.trim(),
        clip_inline(output, 6000)
    )
}

fn finalization_prompt() -> &'static str {
    "<tool_budget_reached>工具预算已用尽。不要再请求工具。请只基于上面的用户问题、系统要求和已执行工具结果输出最终调查报告；缺少证据的地方明确写“不确定”或“缺证据”。</tool_budget_reached>"
}

fn strip_report_preamble(content: &str) -> String {
    let trimmed = content.trim();
    for heading in ["## 调查结果", "# 调查结果"] {
        if let Some(index) = trimmed.find(heading) {
            return trimmed[index..].trim().to_string();
        }
    }
    trimmed
        .lines()
        .skip_while(|line| {
            let line = line.trim();
            line.is_empty()
                || line == "---"
                || line.contains("以下是")
                || line.contains("最终报告") && line.len() < 30
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn clip_inline(value: &str, max_chars: usize) -> String {
    let value = value.split_whitespace().collect::<Vec<_>>().join(" ");
    if value.chars().count() <= max_chars {
        value
    } else {
        format!(
            "{}...",
            value
                .chars()
                .take(max_chars.saturating_sub(3))
                .collect::<String>()
        )
    }
}

async fn gather_linux_game_compatibility_signals(args: Value) -> Result<String> {
    let game = required(&args, "game")?;
    let candidates = game_candidates(&game);
    let search_game = candidates.first().cloned().unwrap_or_else(|| game.clone());
    let issue = args
        .get("issue")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim()
        .to_string();
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .user_agent("miyu-linux-game-compatibility/0.1")
        .build()?;
    let (steam, steam_attempts) = steam_search_candidates(&client, &candidates).await;
    let appid = steam["appid"].as_u64();
    let steam_name = steam["name"].as_str().unwrap_or(&game).to_string();
    let mut slug_candidates = slug_candidates(&candidates);
    if appid.is_some() {
        slug_candidates.insert(0, slugify(&steam_name));
    }
    slug_candidates.sort();
    slug_candidates.dedup();
    let protondb = if let Some(appid) = appid {
        fetch_json(
            &client,
            &format!("https://www.protondb.com/api/v1/reports/summaries/{appid}.json"),
        )
        .await
        .ok()
    } else {
        None
    };
    let can_i_play_result = fetch_first_text(&client, &slug_candidates, |slug| {
        format!("https://caniplayonlinux.com/games/{slug}/")
    })
    .await;
    let anticheat_result = fetch_first_text(&client, &slug_candidates, |slug| {
        format!("https://areweanticheatyet.com/game/{slug}")
    })
    .await;
    let can_i_play = can_i_play_result.text.as_deref();
    let anticheat = anticheat_result.text.as_deref();
    let verdict = verdict(&protondb, can_i_play, anticheat, &issue);
    let confidence = compatibility_confidence(appid, &protondb, can_i_play, anticheat, &verdict);
    let needs_followup = confidence["needs_followup"].as_bool().unwrap_or(true);
    Ok(serde_json::to_string_pretty(&json!({
        "ok": true,
        "game_query": game,
        "search_query": search_game,
        "query_candidates": candidates,
        "matched_name": steam_name,
        "steam": steam,
        "source_attempts": {
            "steam": steam_attempts,
            "can_i_play_on_linux": can_i_play_result.attempts,
            "are_we_anticheat_yet": anticheat_result.attempts,
        },
        "verdict": verdict,
        "confidence": confidence,
        "needs_followup": needs_followup,
        "protondb": protondb,
        "can_i_play_on_linux": can_i_play.map(extract_can_i_play_summary),
        "are_we_anticheat_yet": anticheat.map(extract_anticheat_summary),
        "sources": {
            "steam": appid.map(|id| format!("https://store.steampowered.com/app/{id}/")),
            "protondb": appid.map(|id| format!("https://www.protondb.com/app/{id}")),
            "can_i_play_on_linux": can_i_play_result.url,
            "are_we_anticheat_yet": anticheat_result.url,
        },
        "methodology": "If ProtonDB exists, use ProtonDB reports/comments as the primary practical playability signal. If ProtonDB is missing or insufficient, continue with web_search/web_fetch outside this tool. Keep final answer concise and include 调查结果, 依据, 怎么玩, 注意事项.",
    }))?)
}

#[derive(Default)]
struct TextFetchResult {
    text: Option<String>,
    url: Option<String>,
    attempts: Vec<Value>,
}

fn game_candidates(game: &str) -> Vec<String> {
    let normalized = normalize_game_query(game);
    let mut candidates = vec![normalized];
    candidates.retain(|candidate| !candidate.trim().is_empty());
    candidates.sort();
    candidates.dedup();
    candidates
}

fn slug_candidates(candidates: &[String]) -> Vec<String> {
    let mut slugs = candidates
        .iter()
        .map(|candidate| slugify(candidate))
        .filter(|slug| !slug.is_empty())
        .collect::<Vec<_>>();
    slugs.sort();
    slugs.dedup();
    slugs
}

fn normalize_game_query(game: &str) -> String {
    let compact = game
        .chars()
        .filter(|ch| !ch.is_whitespace())
        .collect::<String>()
        .to_ascii_lowercase();
    if compact.contains("赛博朋克2077")
        || compact.contains("电驭叛客2077")
        || compact.contains("cyberpunk2077")
    {
        return "Cyberpunk 2077".to_string();
    }
    if compact.contains("原神") || compact.contains("genshinimpact") {
        return "Genshin Impact".to_string();
    }
    game.trim().to_string()
}

async fn steam_search_candidates(
    client: &reqwest::Client,
    candidates: &[String],
) -> (Value, Vec<Value>) {
    let mut attempts = Vec::new();
    for candidate in candidates {
        match steam_search(client, candidate).await {
            Ok(value) => {
                attempts.push(json!({"query": candidate, "ok": true, "appid": value["appid"], "name": value["name"]}));
                return (value, attempts);
            }
            Err(err) => {
                attempts.push(json!({"query": candidate, "ok": false, "error": err.to_string()}))
            }
        }
    }
    (Value::Null, attempts)
}

async fn fetch_first_text<F>(
    client: &reqwest::Client,
    slugs: &[String],
    url_for_slug: F,
) -> TextFetchResult
where
    F: Fn(&str) -> String,
{
    let mut result = TextFetchResult::default();
    for slug in slugs {
        let url = url_for_slug(slug);
        match fetch_text(client, &url).await {
            Ok(text) => {
                result
                    .attempts
                    .push(json!({"slug": slug, "url": url, "ok": true}));
                result.url = Some(url);
                result.text = Some(text);
                return result;
            }
            Err(err) => result
                .attempts
                .push(json!({"slug": slug, "url": url, "ok": false, "error": err.to_string()})),
        }
    }
    result
}

async fn steam_search(client: &reqwest::Client, game: &str) -> Result<Value> {
    let value: Value = client
        .get("https://store.steampowered.com/api/storesearch/")
        .query(&[("term", game), ("l", "english"), ("cc", "US")])
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let item = value["items"]
        .as_array()
        .and_then(|items| items.first())
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("Steam app not found for {game}"))?;
    Ok(json!({"appid": item["id"], "name": item["name"], "url": item["tiny_image"]}))
}

async fn fetch_json(client: &reqwest::Client, url: &str) -> Result<Value> {
    Ok(client
        .get(url)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?)
}

async fn fetch_text(client: &reqwest::Client, url: &str) -> Result<String> {
    Ok(client
        .get(url)
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?)
}

fn verdict(
    protondb: &Option<Value>,
    can_i_play: Option<&str>,
    anticheat: Option<&str>,
    issue: &str,
) -> Value {
    let issue_lower = issue.to_ascii_lowercase();
    let multiplayer_sensitive = issue_lower.contains("multi")
        || issue_lower.contains("online")
        || issue.contains("联机")
        || issue.contains("多人")
        || issue.contains("反作弊");
    let anticheat_denied = anticheat
        .map(|text| text.contains("Denied") || text.contains("Broken"))
        .unwrap_or(false);
    if multiplayer_sensitive && anticheat_denied {
        return json!({"traffic_light":"🔴", "label":"不可玩", "reason":"anti-cheat denied or broken for multiplayer/online use"});
    }
    if can_i_play
        .map(|text| text.contains("Broken"))
        .unwrap_or(false)
    {
        return json!({"traffic_light":"🔴", "label":"不可玩", "reason":"Can I Play on Linux marks it broken"});
    }
    let tier = protondb
        .as_ref()
        .and_then(|value| value["tier"].as_str())
        .unwrap_or_default();
    if matches!(tier, "platinum" | "gold")
        || can_i_play
            .map(|text| text.contains("Works"))
            .unwrap_or(false)
    {
        return json!({"traffic_light":"🟢", "label":"可玩", "reason":"ProtonDB/Can I Play on Linux indicate it works"});
    }
    if matches!(tier, "silver" | "bronze")
        || can_i_play
            .map(|text| text.contains("Partial"))
            .unwrap_or(false)
    {
        return json!({"traffic_light":"🟡", "label":"不一定能玩", "reason":"partial or lower confidence compatibility"});
    }
    json!({"traffic_light":"🟡", "label":"不一定能玩", "reason":"insufficient compatibility data"})
}

fn compatibility_confidence(
    appid: Option<u64>,
    protondb: &Option<Value>,
    can_i_play: Option<&str>,
    anticheat: Option<&str>,
    verdict: &Value,
) -> Value {
    let tier = protondb
        .as_ref()
        .and_then(|value| value["tier"].as_str())
        .unwrap_or_default();
    let has_protondb = protondb.is_some();
    let has_can_i_play = can_i_play.is_some();
    let has_anticheat = anticheat.is_some();
    let can_i_play_works = can_i_play
        .map(|text| text.contains("Works"))
        .unwrap_or(false);
    let can_i_play_partial = can_i_play
        .map(|text| text.contains("Partial"))
        .unwrap_or(false);
    let reason = verdict["reason"].as_str().unwrap_or_default();
    let mut reasons = Vec::new();
    if appid.is_none() {
        reasons.push("Steam app id was not found");
    }
    if !has_protondb {
        reasons.push("ProtonDB data is missing");
    }
    if !has_can_i_play {
        reasons.push("Can I Play on Linux data is missing");
    }
    if !has_anticheat {
        reasons.push("AreWeAntiCheatYet data is missing");
    }
    if reason.contains("insufficient") {
        reasons.push("compatibility data is insufficient");
    }

    let confidence = if appid.is_some()
        && matches!(tier, "platinum" | "gold")
        && can_i_play_works
        && has_anticheat
    {
        "high"
    } else if matches!(tier, "platinum" | "gold" | "silver" | "bronze")
        || can_i_play_partial
        || can_i_play_works
    {
        "medium"
    } else {
        "low"
    };
    let needs_followup =
        confidence == "low" || reason.contains("insufficient") || !reasons.is_empty();
    json!({
        "level": confidence,
        "needs_followup": needs_followup,
        "followup_reason": if reasons.is_empty() { Value::Null } else { json!(reasons.join("; ")) },
        "source_coverage": {
            "steam_appid": appid.is_some(),
            "protondb": has_protondb,
            "can_i_play_on_linux": has_can_i_play,
            "are_we_anticheat_yet": has_anticheat
        },
        "suggested_followup_queries": [
            "ProtonDB game compatibility latest reports",
            "PCGamingWiki Linux Proton known issues",
            "Steam Community Linux Proton performance issues"
        ]
    })
}

fn extract_can_i_play_summary(html: &str) -> Value {
    let text = html2text::from_read(html.as_bytes(), 120);
    json!({
        "works": text.contains("Works"),
        "partial": text.contains("Partial"),
        "broken": text.contains("Broken"),
        "source_recommended_proton": value_after_label(&text, "Recommended Proton"),
        "steam_deck_verified": text.contains("Steam Deck Verified"),
        "known_issues": section_excerpt(&text, "Known issues", "Fixes", 1200),
        "fixes": section_excerpt(&text, "Fixes", "Verdict", 1200),
        "text_excerpt": excerpt(&text, 2000),
    })
}

fn extract_anticheat_summary(html: &str) -> Value {
    let text = html2text::from_read(html.as_bytes(), 120);
    let status = ["Supported", "Running", "Planned", "Broken", "Denied"]
        .into_iter()
        .find(|status| text.contains(status));
    json!({
        "status": status,
        "mentions_eac": text.contains("Easy Anti-Cheat"),
        "mentions_battleye": text.contains("BattlEye"),
        "text_excerpt": excerpt(&text, 1600),
    })
}

fn value_after_label(text: &str, label: &str) -> Option<String> {
    let mut lines = text.lines().map(str::trim).filter(|line| !line.is_empty());
    while let Some(line) = lines.next() {
        if line == label {
            return lines.next().map(|value| value.chars().take(120).collect());
        }
    }
    None
}

fn section_excerpt(text: &str, start: &str, end: &str, max_chars: usize) -> Option<String> {
    let after = text.split(start).nth(1)?;
    let section = after.split(end).next().unwrap_or(after);
    Some(excerpt(section, max_chars))
}

fn excerpt(text: &str, max_chars: usize) -> String {
    text.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(max_chars)
        .collect()
}

fn slugify(value: &str) -> String {
    let mut out = String::new();
    let mut last_dash = false;
    for ch in value.to_ascii_lowercase().chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch);
            last_dash = false;
        } else if !last_dash {
            out.push('-');
            last_dash = true;
        }
    }
    out.trim_matches('-').to_string()
}

fn required(args: &Value, key: &str) -> Result<String> {
    let value = args
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim();
    if value.is_empty() {
        bail!("missing required argument: {key}")
    }
    Ok(value.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugifies_game_names() {
        assert_eq!(slugify("Cyberpunk 2077"), "cyberpunk-2077");
        assert_eq!(
            slugify("Tom Clancy's Rainbow Six® Siege"),
            "tom-clancy-s-rainbow-six-siege"
        );
    }

    #[test]
    fn normalizes_chinese_cyberpunk_query() {
        assert_eq!(normalize_game_query("赛博朋克2077"), "Cyberpunk 2077");
        assert_eq!(
            normalize_game_query("Linux能玩赛博朋克2077吗"),
            "Cyberpunk 2077"
        );
    }

    #[test]
    fn normalizes_chinese_genshin_query() {
        assert_eq!(normalize_game_query("原神"), "Genshin Impact");
        assert!(game_candidates("linux能玩原神吗")
            .iter()
            .any(|candidate| candidate == "Genshin Impact"));
        assert_eq!(slugify("Genshin Impact"), "genshin-impact");
        assert_eq!(
            slug_candidates(&game_candidates("linux能玩原神吗")),
            vec!["genshin-impact"]
        );
    }

    #[test]
    fn output_instruction_mentions_final_report() {
        assert!(OUTPUT_INSTRUCTION.contains("final_report"));
        assert!(OUTPUT_INSTRUCTION.contains("红绿灯"));
        assert!(OUTPUT_INSTRUCTION.contains("怎么"));
    }

    #[test]
    fn insufficient_data_requires_followup() {
        let result = verdict(&None, None, None, "");
        assert_eq!(result["label"], "不一定能玩");
        let confidence = compatibility_confidence(None, &None, None, None, &result);
        assert_eq!(confidence["level"], "low");
        assert_eq!(confidence["needs_followup"], true);
    }

    #[test]
    fn strong_cross_source_signal_is_high_confidence() {
        let protondb = Some(json!({"tier":"gold"}));
        let result = verdict(&protondb, Some("Works"), None, "");
        let confidence = compatibility_confidence(
            Some(1091500),
            &protondb,
            Some("Works"),
            Some("Running"),
            &result,
        );
        assert_eq!(result["label"], "可玩");
        assert_eq!(confidence["level"], "high");
        assert_eq!(confidence["needs_followup"], false);
    }

    #[test]
    fn genshin_can_i_play_and_anticheat_indicate_playable() {
        let result = verdict(
            &None,
            Some("Genshin Impact Works Yes — runs via Proton"),
            Some("Genshin Impact Running AntiCheat"),
            "",
        );
        assert_eq!(result["label"], "可玩");
        let confidence =
            compatibility_confidence(None, &None, Some("Works"), Some("Running"), &result);
        assert_eq!(confidence["level"], "medium");
        assert_eq!(confidence["needs_followup"], true);
    }

    #[test]
    fn single_source_signal_still_suggests_followup() {
        let protondb = Some(json!({"tier":"gold"}));
        let result = verdict(&protondb, None, None, "");
        let confidence = compatibility_confidence(Some(1091500), &protondb, None, None, &result);
        assert_eq!(confidence["level"], "medium");
        assert_eq!(confidence["needs_followup"], true);
    }

    #[test]
    fn anticheat_denied_blocks_multiplayer_verdict() {
        let result = verdict(
            &None,
            None,
            Some("Apex Legends Denied Easy Anti-Cheat"),
            "多人",
        );
        assert_eq!(result["traffic_light"], "🔴");
    }

    #[test]
    fn gold_protondb_is_playable() {
        let result = verdict(&Some(json!({"tier":"gold"})), None, None, "");
        assert_eq!(result["traffic_light"], "🟢");
    }

    #[test]
    fn can_i_play_marks_recommended_proton_as_source_value() {
        let summary = extract_can_i_play_summary(
            "<p>Works</p><p>Recommended Proton</p><p>Proton 9.0-3</p><p>Steam Deck Verified</p>",
        );
        assert_eq!(summary["source_recommended_proton"], "Proton 9.0-3");
        assert!(summary.get("recommended_proton").is_none());
    }
}
