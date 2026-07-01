use super::{readable_tool_name, ToolProgress, ToolRegistry, ToolSpec};
use crate::config::AppConfig;
use crate::i18n::{is_zh, text as t};
use crate::llm::{ChatMessage, ChatResult, OpenAiCompatibleClient};
use crate::paths::MiyuPaths;
use anyhow::{bail, Result};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const INVESTIGATOR_SYSTEM_PROMPT: &str = r#"你是 Miyu Linux 游戏兼容性调查系统中的“深思者”。
You are the investigator in Miyu's Linux game compatibility investigation system.
你的任务是调查用户询问的游戏在 Linux / Proton / Steam Deck 下是否可玩，并输出可交给 Miyu 回复用户的报告。
Your task is to investigate whether the requested game is playable on Linux / Proton / Steam Deck, then produce a report Miyu can use to answer the user.

工作原则：
1. 第一轮必须先调用 gather_linux_game_compatibility_signals 收集 Steam、ProtonDB、Can I Play on Linux、AreWeAntiCheatYet 的基础信号。
2. 关键判断必须注册证据，使用 register_linux_game_evidence 获得 [S1]/[P1]/[C1]/[A1]/[W1] 等标记，并在报告正文中引用。
3. 红绿灯是灵魂，最终必须给出且只能给出：🟢 可玩、🟡 需要继续确认、🟡 不一定能玩、🔴 不可玩。
4. 区分单机可玩、多人/反作弊可玩、Steam Deck 验证、性能表现、崩溃/Mod 等不同维度。
5. 不把 Can I Play on Linux 的历史 recommended Proton 误称为当前最新推荐。优先建议 Steam 当前默认/最新稳定 Proton；出问题再试 Proton Experimental 或来源列出的历史版本。
6. 证据不足、来源冲突、用户问到性能/崩溃/Mod/反作弊细节时，继续调用 web_search / web_fetch 补查，直到报告可信。
7. 不编造来源；资料冲突时说明冲突和取舍；没有证据的点明确说不确定。
8. 最终报告用中文，且必须包含 ## 调查结果、## 依据、## 怎么玩、## 注意事项。只有存在明确 FPS、性能、Steam Deck 体验或 Windows 对比数据时，才额外输出 ## 性能表现。
9. ## 怎么玩 必须给出可执行路线：例如 Steam Proton 版本选择、启动参数、第三方启动器、发行版/Flatpak/AUR 安装方式、反作弊限制和风险。若没有可靠玩法，也必须写“暂无可靠玩法”。
"#;

const REVIEWER_SYSTEM_PROMPT: &str = r#"你是 Miyu Linux 游戏兼容性调查系统中的“审视者”。
You are the reviewer in Miyu's Linux game compatibility investigation system.
你只审查深思者报告，不替用户回答。请严格输出 JSON。
Only review the investigator's report; do not answer the user. Output strict JSON.

审查重点：
1. 红绿灯结论是否有证据支撑，是否保留了 🟢/🟡/🔴 设计。
2. 是否至少检查了 Steam/ProtonDB/Can I Play on Linux/AreWeAntiCheatYet 中和问题相关的来源；缺失时是否补查或说明原因。
3. 是否区分单机、多人、反作弊、Steam Deck、性能、崩溃、Mod 等不同维度。
4. 是否存在来源冲突、过期信息、低置信度信号，却被写成确定结论。
5. 是否混淆历史 recommended Proton 与当前推荐 Proton。
6. 正文引用的 [S/P/C/A/W数字] 是否都在证据注册表中。
7. 是否有足够时效性：近期状态、官方/社区最近资料、网页来源时间或无法确认时间的说明。
8. 是否包含必需章节：## 调查结果、## 依据、## 怎么玩、## 注意事项；缺少任何一个都不接受。
9. ## 怎么玩 是否给出了可执行路线，而不是只有结论；没有可靠玩法时是否明确写出“暂无可靠玩法”。

输出格式：
{
  "accepted": true/false,
  "challenge": "主要质疑或通过理由",
  "revision_instructions": ["需要补查或修正的事项"],
  "missing_evidence": ["仍缺的关键证据"],
  "risk_flags": ["过度确定/来源冲突/时效性不足/反作弊遗漏等"]
}
"#;

const MAX_REVIEW_ROUNDS: usize = 12;
const MAX_TOOL_STEPS_PER_ROUND: usize = 40;
const TOOL_TIMEOUT_SECONDS: u64 = 45;

#[derive(Clone)]
struct GameCompatibilityContext {
    config: AppConfig,
    paths: MiyuPaths,
    tools: ToolRegistry,
}

#[derive(Default)]
struct GameCompatibilityState {
    evidence: Vec<GameEvidence>,
    counters: GameEvidenceCounters,
}

#[derive(Default)]
struct GameEvidenceCounters {
    steam: usize,
    protondb: usize,
    can_i_play: usize,
    anticheat: usize,
    web: usize,
}

#[derive(Clone)]
struct GameEvidence {
    marker: String,
    kind: String,
    title: String,
    source: String,
    snippet: String,
    freshness: String,
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
        t("Run a reviewed Linux game compatibility investigation with evidence and traffic-light verdict. Use for Linux gaming compatibility, Proton, Steam Deck, performance, crash, mods, and anti-cheat questions.", "进行带证据审视的 Linux 游戏兼容性调查，输出红绿灯结论。适用于 Linux 游戏兼容性、Proton、Steam Deck、性能、崩溃、Mod、反作弊等问题。"),
        json!({"type":"object","properties":{"game":{"type":"string","description":"Game title."},"issue":{"type":"string","description":"Optional issue such as crash, multiplayer, anti-cheat, performance, mods."}},"required":["game"],"additionalProperties":false}),
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
    let client = OpenAiCompatibleClient::from_config(&context.config, &context.paths)?;
    let state = Arc::new(Mutex::new(GameCompatibilityState::default()));
    let mut draft = String::new();
    let mut review =
        json!({"accepted": false, "challenge": "首轮暂无审视意见", "revision_instructions": []});
    let mut iterations = 0usize;
    let mut stop_reason = "max_review_rounds_reached".to_string();
    progress.report(format!(
        "{}: {}",
        t("Linux game compatibility", "Linux 游戏兼容性"),
        game
    ));

    for iteration in 1..=MAX_REVIEW_ROUNDS {
        iterations = iteration;
        progress.report(if is_zh() {
            format!("第 {iteration} 轮：兼容性调查中")
        } else {
            format!("round {iteration}: investigating compatibility")
        });
        let tools = compatibility_tool_registry(&context, Arc::clone(&state));
        let prompt = investigator_prompt(&game, &issue, iteration, &draft, &review, &state)?;
        let result = chat_with_tools(
            &client,
            vec![
                ChatMessage::system(INVESTIGATOR_SYSTEM_PROMPT),
                ChatMessage::plain("user", prompt),
            ],
            tools,
            &progress,
        )
        .await?;
        if !result.content.trim().is_empty() {
            draft = result.content.trim().to_string();
        }
        if draft.is_empty() {
            stop_reason = "investigator_failed".to_string();
            break;
        }
        progress.report(if is_zh() {
            format!("第 {iteration} 轮：审视中")
        } else {
            format!("round {iteration}: reviewer checking")
        });
        let review_prompt = reviewer_prompt(&game, &issue, iteration, &draft, &state)?;
        let review_result = client
            .chat_stream(
                vec![
                    ChatMessage::system(REVIEWER_SYSTEM_PROMPT),
                    ChatMessage::plain("user", review_prompt),
                ],
                Vec::new(),
                |_| Ok(()),
            )
            .await?;
        review = parse_review(&review_result.content);
        if review
            .get("accepted")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            stop_reason = "accepted".to_string();
            progress.report(if is_zh() {
                format!("第 {iteration} 轮：通过")
            } else {
                format!("round {iteration}: accepted")
            });
            break;
        }
        progress.report(format!(
            "{}: {}",
            t("revision requested", "需要修订"),
            clip_inline(
                review
                    .get("challenge")
                    .and_then(Value::as_str)
                    .unwrap_or("reviewer requested more evidence"),
                100
            )
        ));
    }

    let final_report = normalize_final_report(&draft, &state);
    Ok(serde_json::to_string_pretty(&json!({
        "ok": true,
        "kind": "linux_game_compatibility",
        "game_query": game,
        "issue": issue,
        "iterations_used": iterations,
        "stop_reason": stop_reason,
        "final_report": final_report,
        "review": review,
        "evidence": public_evidence(&state),
    }))?)
}

fn compatibility_tool_registry(
    context: &GameCompatibilityContext,
    state: Arc<Mutex<GameCompatibilityState>>,
) -> ToolRegistry {
    let mut registry = context.tools.clone();
    registry.register(ToolSpec::new(
        "gather_linux_game_compatibility_signals",
        t("Gather Steam, ProtonDB, Can I Play on Linux, and AreWeAntiCheatYet compatibility signals for one game.", "收集单个游戏在 Steam、ProtonDB、Can I Play on Linux、AreWeAntiCheatYet 上的兼容性信号。"),
        json!({"type":"object","properties":{"game":{"type":"string","description":"Game title."},"issue":{"type":"string","description":"Optional issue such as crash, multiplayer, anti-cheat, performance, mods."}},"required":["game"],"additionalProperties":false}),
        |args| async move { gather_linux_game_compatibility_signals(args).await },
    ));
    register_game_evidence_tool(&mut registry, state);
    registry
}

fn register_game_evidence_tool(
    registry: &mut ToolRegistry,
    state: Arc<Mutex<GameCompatibilityState>>,
) {
    registry.register(ToolSpec::new(
        "register_linux_game_evidence",
        t("Register Linux game compatibility evidence and receive a stable marker such as [S1], [P1], [C1], [A1], or [W1].", "登记 Linux 游戏兼容性证据，并返回 [S1]、[P1]、[C1]、[A1] 或 [W1] 这类稳定标记。"),
        json!({"type":"object","properties":{"evidence_type":{"type":"string","enum":["S","P","C","A","W","steam","protondb","can_i_play_on_linux","anti_cheat","web"]},"title":{"type":"string"},"source":{"type":"string"},"snippet":{"type":"string"},"freshness":{"type":"string"}},"required":["evidence_type","title"],"additionalProperties":false}),
        move |args| {
            let state = Arc::clone(&state);
            async move {
                let kind = normalized_evidence_kind(args.get("evidence_type").and_then(Value::as_str).unwrap_or("W"));
                let title = args.get("title").and_then(Value::as_str).unwrap_or("Untitled").trim().to_string();
                let source = args.get("source").and_then(Value::as_str).unwrap_or_default().trim().to_string();
                let snippet = args.get("snippet").and_then(Value::as_str).unwrap_or_default().trim().to_string();
                let freshness = args.get("freshness").and_then(Value::as_str).unwrap_or_default().trim().to_string();
                let mut state = state.lock().expect("linux game state lock");
                let number = match kind.as_str() {
                    "S" => { state.counters.steam += 1; state.counters.steam }
                    "P" => { state.counters.protondb += 1; state.counters.protondb }
                    "C" => { state.counters.can_i_play += 1; state.counters.can_i_play }
                    "A" => { state.counters.anticheat += 1; state.counters.anticheat }
                    _ => { state.counters.web += 1; state.counters.web }
                };
                let marker = format!("{kind}{number}");
                state.evidence.push(GameEvidence { marker: marker.clone(), kind, title, source, snippet, freshness });
                Ok(json!({"ok": true, "evidence": marker, "marker": format!("[{marker}]")}).to_string())
            }
        },
    ));
}

async fn chat_with_tools(
    client: &OpenAiCompatibleClient,
    mut messages: Vec<ChatMessage>,
    tools: ToolRegistry,
    progress: &ToolProgress,
) -> Result<ChatResult> {
    let definitions =
        tools.definitions_except(&["linux_game_compatibility", "deep_research", "deep_diagnose"]);
    let mut steps = 0usize;
    loop {
        let result = client
            .chat_stream(messages.clone(), definitions.clone(), |_| Ok(()))
            .await?;
        if result.tool_calls.is_empty() {
            return Ok(result);
        }
        messages.push(ChatMessage::assistant(
            result.content.clone(),
            Some(result.tool_calls.clone()),
        ));
        for call in result.tool_calls {
            if steps >= MAX_TOOL_STEPS_PER_ROUND {
                messages.push(ChatMessage::tool(
                    call.id,
                    "tool budget reached for this compatibility round",
                ));
                continue;
            }
            steps += 1;
            progress.report(if is_zh() {
                format!(
                    "工具 #{steps}：{} 运行中",
                    readable_tool_name(&call.function.name)
                )
            } else {
                format!("tool #{steps}: {} running", call.function.name)
            });
            let output = match tokio::time::timeout(
                Duration::from_secs(TOOL_TIMEOUT_SECONDS),
                tools.call(&call.function.name, &call.function.arguments),
            )
            .await
            {
                Ok(Ok(output)) => output,
                Ok(Err(err)) => format!("tool error: {err}"),
                Err(_) => format!(
                    "tool error: {} timed out after {TOOL_TIMEOUT_SECONDS}s",
                    call.function.name
                ),
            };
            progress.report(if is_zh() {
                format!(
                    "工具 #{steps}：{} 完成",
                    readable_tool_name(&call.function.name)
                )
            } else {
                format!("tool #{steps}: {} done", call.function.name)
            });
            messages.push(ChatMessage::tool(call.id, output));
        }
    }
}

fn investigator_prompt(
    game: &str,
    issue: &str,
    iteration: usize,
    draft: &str,
    review: &Value,
    state: &Arc<Mutex<GameCompatibilityState>>,
) -> Result<String> {
    let draft_display = if draft.trim().is_empty() {
        "（无）"
    } else {
        draft
    };
    Ok(if is_zh() {
        format!(
            "这是第 {iteration} 轮 Linux 游戏兼容性调查。\n\n游戏：{game}\n关注点：{}\n\n上一轮报告：\n{draft_display}\n\n上一轮审视意见：\n{}\n\n当前证据注册表：\n{}\n\n要求：第一轮先调用 gather_linux_game_compatibility_signals。必要时继续搜索和读取网页，直到红绿灯结论、反作弊、Proton 建议和时效性都有证据支撑。输出可直接交给 Miyu 回复用户的中文报告。",
            if issue.trim().is_empty() { "（无）" } else { issue },
            serde_json::to_string_pretty(review)?,
            evidence_registry_json(state)?,
        )
    } else {
        format!(
            "This is Linux game compatibility investigation round {iteration}.\n\nGame: {game}\nFocus: {}\n\nPrevious report:\n{draft_display}\n\nPrevious review:\n{}\n\nCurrent evidence registry:\n{}\n\nRequirements: in the first round, call gather_linux_game_compatibility_signals first. Continue searching and fetching pages when needed until the traffic-light verdict, anti-cheat status, Proton recommendation, and freshness are evidence-backed. Output a report Miyu can use directly to answer the user.",
            if issue.trim().is_empty() { "none" } else { issue },
            serde_json::to_string_pretty(review)?,
            evidence_registry_json(state)?,
        )
    })
}

fn reviewer_prompt(
    game: &str,
    issue: &str,
    iteration: usize,
    draft: &str,
    state: &Arc<Mutex<GameCompatibilityState>>,
) -> Result<String> {
    Ok(if is_zh() {
        format!(
            "请审查第 {iteration} 轮 Linux 游戏兼容性报告。\n\n游戏：{game}\n关注点：{}\n\n报告：\n{draft}\n\n证据注册表：\n{}\n\n若报告可信且可交给 Miyu 回复用户，accepted=true；否则列出需要继续补查或修正的事项。",
            if issue.trim().is_empty() { "（无）" } else { issue },
            evidence_registry_json(state)?,
        )
    } else {
        format!(
            "Review Linux game compatibility report round {iteration}.\n\nGame: {game}\nFocus: {}\n\nReport:\n{draft}\n\nEvidence registry:\n{}\n\nIf the report is trustworthy and ready for Miyu to answer the user, set accepted=true; otherwise list concrete evidence gaps or revisions.",
            if issue.trim().is_empty() { "none" } else { issue },
            evidence_registry_json(state)?,
        )
    })
}

fn evidence_registry_json(state: &Arc<Mutex<GameCompatibilityState>>) -> Result<String> {
    let state = state.lock().expect("linux game state lock");
    Ok(serde_json::to_string_pretty(&state.evidence.iter().map(|item| json!({"marker": item.marker, "type": item.kind, "title": item.title, "source": item.source, "snippet": item.snippet, "freshness": item.freshness})).collect::<Vec<_>>())?)
}

fn parse_review(content: &str) -> Value {
    serde_json::from_str(content.trim()).unwrap_or_else(|_| json!({"accepted": false, "challenge": "reviewer returned non-JSON feedback", "revision_instructions": [content.trim()]}))
}

fn normalize_final_report(draft: &str, state: &Arc<Mutex<GameCompatibilityState>>) -> String {
    let mut report = draft.trim().to_string();
    let warnings = evidence_warnings(&report, state);
    if !warnings.is_empty() {
        report.push_str("\n\n## 证据校验提示\n");
        for warning in warnings {
            report.push_str(&format!("- {warning}\n"));
        }
    }
    report
}

fn evidence_warnings(draft: &str, state: &Arc<Mutex<GameCompatibilityState>>) -> Vec<String> {
    let state = state.lock().expect("linux game state lock");
    let known = state
        .evidence
        .iter()
        .map(|item| item.marker.as_str())
        .collect::<Vec<_>>();
    let mut warnings = Vec::new();
    for marker in extract_markers(draft) {
        if !known.iter().any(|item| *item == marker) {
            warnings.push(format!("正文引用了未注册证据 [{marker}]。"));
        }
    }
    warnings
}

fn extract_markers(value: &str) -> Vec<String> {
    let mut out = Vec::new();
    for part in value.split('[').skip(1) {
        let Some(end) = part.find(']') else { continue };
        let marker = &part[..end];
        if marker.len() >= 2
            && matches!(marker.as_bytes()[0], b'S' | b'P' | b'C' | b'A' | b'W')
            && marker[1..].chars().all(|ch| ch.is_ascii_digit())
        {
            out.push(marker.to_string());
        }
    }
    out
}

fn normalized_evidence_kind(value: &str) -> String {
    match value.trim().to_ascii_lowercase().as_str() {
        "s" | "steam" => "S".to_string(),
        "p" | "protondb" => "P".to_string(),
        "c" | "can_i_play_on_linux" | "can-i-play-on-linux" => "C".to_string(),
        "a" | "anti_cheat" | "anticheat" | "areweanticheatyet" => "A".to_string(),
        _ => "W".to_string(),
    }
}

fn public_evidence(state: &Arc<Mutex<GameCompatibilityState>>) -> Vec<Value> {
    let state = state.lock().expect("linux game state lock");
    state.evidence.iter().map(|item| json!({"marker": item.marker, "type": item.kind, "title": item.title, "source": item.source, "freshness": item.freshness})).collect()
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
            "protondb": appid.map(|id| format!("https://www.protondb.com/app/{id}")),
            "can_i_play_on_linux": can_i_play_result.url,
            "are_we_anticheat_yet": anticheat_result.url,
        },
        "output_instruction": "These are collected source signals for the investigator. Register evidence before citing them, continue web_search/web_fetch when sources conflict or lack freshness, and keep the final traffic-light verdict evidence-backed."
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
    json!({"traffic_light":"🟡", "label":"需要继续确认", "reason":"insufficient compatibility data"})
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
    fn output_instruction_keeps_sections_flexible() {
        assert!(INVESTIGATOR_SYSTEM_PROMPT.contains("红绿灯"));
        assert!(INVESTIGATOR_SYSTEM_PROMPT.contains("web_search"));
        assert!(INVESTIGATOR_SYSTEM_PROMPT.contains("## 怎么玩 必须给出可执行路线"));
        assert!(INVESTIGATOR_SYSTEM_PROMPT.contains("暂无可靠玩法"));
        assert!(REVIEWER_SYSTEM_PROMPT.contains("时效性"));
        assert!(REVIEWER_SYSTEM_PROMPT.contains("缺少任何一个都不接受"));
    }

    #[test]
    fn insufficient_data_requires_followup() {
        let result = verdict(&None, None, None, "");
        assert_eq!(result["label"], "需要继续确认");
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
