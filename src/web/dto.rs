//! 发给前端的数据形状。
//!
//! 前缀 `Safe` 的类型是**脱敏过的投影**：内部结构里有工作目录、原始工具参数、
//! 内部助手文本这些不该出网页的东西。`From` 实现就是那道过滤
//! （`redact_internal_assistant_text` 等），所以新增字段时默认不会漏出去——得
//! 显式加进来才会。

use crate::web::*;

pub(in crate::web) const DEFAULT_BOARD_TITLE: &str = "今天想聊些什么？";

pub(in crate::web) const DEFAULT_BOARD_SUBTITLE: &str = "从一个问题、计划或此刻的想法开始。";

/// 输入框为空时的提示。跟着人格名走,所以是函数不是常量:此前这句写死在
/// index.html 里,人格改了名输入框还留着旧名字。
pub(in crate::web) fn default_composer_placeholder(persona_name: &str) -> String {
    format!("给 {persona_name} 发消息")
}

pub(in crate::web) const DEFAULT_STARTER_PROMPTS: [&str; 4] = [
    "查询今天的天气",
    "分析一个问题",
    "发表情包打个招呼吧",
    "搜索一张图片",
];

pub(in crate::web) const MAX_THINKING_VARIANT_UPDATES: usize = 64;

#[derive(Default, Deserialize)]
pub(in crate::web) struct EventsQuery {
    #[serde(default)]
    pub(in crate::web) after: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::web) struct SetModelsRequest {
    pub(in crate::web) models: Vec<ActiveProviderModelConfig>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::web) struct SetThinkingVariantsRequest {
    pub(in crate::web) updates: Vec<ThinkingVariantUpdate>,
}

#[derive(Serialize)]
pub(in crate::web) struct BootstrapResponse {
    pub(in crate::web) version: &'static str,
    pub(in crate::web) boot_id: String,
    pub(in crate::web) latest_event_id: u64,
    pub(in crate::web) active_run_id: Option<String>,
    pub(in crate::web) running_turn_id: Option<String>,
    pub(in crate::web) external_queue_available: bool,
    pub(in crate::web) turns: Vec<SafeTurn>,
    pub(in crate::web) queued_prompts: Vec<SafeQueuedPrompt>,
    pub(in crate::web) models: Vec<SafeModel>,
    pub(in crate::web) display: WebDisplayConfig,
    pub(in crate::web) context: ContextSnapshot,
    pub(in crate::web) usage: SafeUsageSnapshot,
    pub(in crate::web) capabilities: Capabilities,
    pub(in crate::web) sessions: Vec<Value>,
    pub(in crate::web) current_session_id: String,
    /// Every turn currently running, across all sessions.
    pub(in crate::web) runs: Vec<Value>,
    pub(in crate::web) persona: PersonaIdentity,
    pub(in crate::web) redo_candidate: Option<SafeRedoCandidate>,
}

#[derive(Serialize)]
pub(in crate::web) struct Capabilities {
    pub(in crate::web) multi_conversation: bool,
    pub(in crate::web) attachments: bool,
    pub(in crate::web) queue: bool,
    pub(in crate::web) redo: bool,
}

#[derive(Serialize)]
pub(in crate::web) struct SafeModel {
    pub(in crate::web) provider_id: String,
    pub(in crate::web) provider_name: String,
    pub(in crate::web) model: String,
    pub(in crate::web) active: bool,
}

/// 一次工具调用，给 WebUI 看的形态。
///
/// 比库里的 `ToolFlowCall` 多一个 `display_name`：友好名是注册表算出来的
/// （`tools::readable_tool_name`），不落库——落了就得跟着人格/语言变，而且
/// 同一条记录换个语言看就不对了。事件流那侧本来也是现算的
/// （`event_map.rs:164`），这里保持一致。
#[derive(Serialize)]
pub(in crate::web) struct SafeToolCall {
    pub(in crate::web) id: String,
    pub(in crate::web) name: String,
    pub(in crate::web) display_name: String,
    pub(in crate::web) arguments: String,
    pub(in crate::web) output: String,
    /// 这次调用成没成。判定放在这里、不放前端：规则有两条，抄到 JS 里就成了
    /// 第二份真相，改一条忘另一条，同一次调用在实时和回看里会显示成不同颜色。
    pub(in crate::web) ok: bool,
}

/// 从落库的输出反推成败。
///
/// 成败没有单独落库（`ToolFlowCall` 只有 id/name/arguments/output），但信号全
/// 在输出里，分两层：
///
/// 1. **硬失败**——工具压根没跑起来或被拦下（未加载、ask_question 越限、执行
///    报错）。这些路径产出的文本一律以 `tool error:` 开头，仓库里 14 处都是。
/// 2. **业务失败**——工具跑了但结果是失败，靠 `tool_output_succeeded` 那条：
///    输出是 JSON 且 `success`/`ok` 为 false。
///
/// 只做第 2 层的话，「工具未加载」这种会被判成成功——刷新一下红的变绿的。
fn tool_call_succeeded(output: &str) -> bool {
    !output.trim_start().starts_with("tool error:") && crate::agent::tool_output_succeeded(output)
}

#[derive(Serialize)]
pub(in crate::web) struct SafeToolRound {
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub(in crate::web) remote: bool,
    pub(in crate::web) assistant_content: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(in crate::web) assistant_reasoning: Option<String>,
    pub(in crate::web) calls: Vec<SafeToolCall>,
}

impl From<crate::state::ToolFlowRound> for SafeToolRound {
    fn from(round: crate::state::ToolFlowRound) -> Self {
        Self {
            remote: round.remote,
            assistant_content: round.assistant_content,
            assistant_reasoning: round.assistant_reasoning,
            calls: round
                .calls
                .into_iter()
                .map(|call| SafeToolCall {
                    display_name: crate::tools::readable_tool_name(&call.name),
                    ok: tool_call_succeeded(&call.output),
                    id: call.id,
                    name: call.name,
                    arguments: call.arguments,
                    output: call.output,
                })
                .collect(),
        }
    }
}

#[derive(Serialize)]
pub(in crate::web) struct SafeTurn {
    pub(in crate::web) id: String,
    pub(in crate::web) seq: i64,
    pub(in crate::web) status: &'static str,
    pub(in crate::web) active_context: bool,
    pub(in crate::web) user_content: String,
    pub(in crate::web) assistant_content: String,
    pub(in crate::web) assistant_reasoning: Option<String>,
    pub(in crate::web) provider_id: Option<String>,
    pub(in crate::web) model: Option<String>,
    pub(in crate::web) user_timestamp: String,
    pub(in crate::web) assistant_timestamp: Option<String>,
    pub(in crate::web) token_total: u64,
    pub(in crate::web) token_prompt: u64,
    pub(in crate::web) token_cache_read: u64,
    pub(in crate::web) token_usage_estimated: bool,
    pub(in crate::web) question_exchanges: Vec<crate::question::QuestionExchange>,
    /// 这一轮调过哪些工具、拿到什么结果。
    ///
    /// 以前不发：WebUI 的工具信息只在实时事件流里存在过，切走再回来就没了
    /// ——而库里一直有。空数组的回合（没调工具）跳过序列化，别给每个回合都
    /// 塞一个 `"tool_flow": []`。
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(in crate::web) tool_flow: Vec<SafeToolRound>,
    pub(in crate::web) followups: Vec<SafeFollowup>,
    pub(in crate::web) assets: Vec<SafeImageAsset>,
    pub(in crate::web) artifacts: Vec<SafeArtifactAsset>,
    pub(in crate::web) attachments: Vec<SafeUserAttachment>,
    pub(in crate::web) revision: i64,
}

#[derive(Serialize)]
pub(in crate::web) struct SafeRedoCandidate {
    pub(in crate::web) turn_id: String,
    pub(in crate::web) revision: i64,
    pub(in crate::web) input_id: String,
    pub(in crate::web) input_kind: &'static str,
    pub(in crate::web) content: String,
}

impl From<crate::state::RedoCandidate> for SafeRedoCandidate {
    fn from(candidate: crate::state::RedoCandidate) -> Self {
        Self {
            turn_id: candidate.turn_id,
            revision: candidate.revision,
            input_id: candidate.input_id,
            input_kind: match candidate.input_kind {
                crate::state::RedoInputKind::Initial => "initial",
                crate::state::RedoInputKind::Followup => "followup",
            },
            content: candidate.display_content,
        }
    }
}

#[derive(Serialize)]
pub(in crate::web) struct SafeFollowup {
    pub(in crate::web) id: String,
    pub(in crate::web) content: String,
    pub(in crate::web) submitted_at: String,
    pub(in crate::web) preceding_assistant_content: Option<String>,
    pub(in crate::web) preceding_assistant_reasoning: Option<String>,
    pub(in crate::web) provider_id: Option<String>,
    pub(in crate::web) model: Option<String>,
    pub(in crate::web) attachments: Vec<SafeUserAttachment>,
}

#[derive(Serialize)]
pub(in crate::web) struct SafeUsageSnapshot {
    pub(in crate::web) requests: u64,
    pub(in crate::web) prompt_tokens: u64,
    pub(in crate::web) completion_tokens: u64,
    pub(in crate::web) total_tokens: u64,
    pub(in crate::web) conversation_tokens: u64,
    pub(in crate::web) cache_read_tokens: u64,
    pub(in crate::web) cache_write_tokens: u64,
    pub(in crate::web) reasoning_tokens: u64,
    pub(in crate::web) last_usage: Option<Usage>,
    pub(in crate::web) last_conversation_usage: Option<Usage>,
}

#[derive(Serialize)]
pub(in crate::web) struct ModelResponse {
    pub(in crate::web) models: Vec<SafeModel>,
    pub(in crate::web) display: WebDisplayConfig,
    pub(in crate::web) context: ContextSnapshot,
}

#[derive(Serialize)]
pub(in crate::web) struct ThinkingVariantsResponse {
    pub(in crate::web) options: Vec<ThinkingVariantOptions>,
}

#[derive(Deserialize)]
pub(in crate::web) struct UsageStatsQuery {
    #[serde(default)]
    pub(in crate::web) range: Option<String>,
}

#[derive(Deserialize)]
pub(in crate::web) struct UsageDetailsQuery {
    #[serde(default)]
    pub(in crate::web) limit: Option<usize>,
    #[serde(default)]
    pub(in crate::web) src: Option<String>,
    #[serde(default)]
    pub(in crate::web) model: Option<String>,
}

#[derive(Deserialize)]
pub(in crate::web) struct SetSessionModelsRequest {
    /// Empty clears the override so the session follows the global pool.
    #[serde(default)]
    pub(in crate::web) models: Vec<ActiveProviderModelConfig>,
}

#[derive(Serialize)]
pub(in crate::web) struct SessionModelsResponse {
    pub(in crate::web) model_override: Option<Vec<ActiveProviderModelConfig>>,
}

pub(in crate::web) fn safe_models(config: &AppConfig) -> Vec<SafeModel> {
    config
        .provider_model_choices()
        .into_iter()
        .map(|choice| SafeModel {
            active: config.is_active_provider_model(&choice.provider_id, &choice.model),
            provider_id: choice.provider_id,
            provider_name: choice.provider_name,
            model: choice.model,
        })
        .collect()
}

pub(in crate::web) fn safe_multimodal_models(config: &AppConfig) -> Vec<SafeModel> {
    config
        .multimodal_provider_model_choices()
        .into_iter()
        .map(|choice| SafeModel {
            active: config.is_active_multimodal_provider_model(&choice.provider_id, &choice.model),
            provider_id: choice.provider_id,
            provider_name: choice.provider_name,
            model: choice.model,
        })
        .collect()
}

impl SafeTurn {
    pub(in crate::web) fn from_turn(
        turn: Turn,
        assets: Vec<ImageAsset>,
        artifacts: Vec<ArtifactAsset>,
    ) -> Self {
        let assets = assets
            .into_iter()
            .map(|asset| {
                let hide_caption = meme_asset_caption_hidden(&asset, &turn.tool_reports);
                SafeImageAsset::from_asset(asset, hide_caption)
            })
            .collect();
        Self {
            id: turn.turn_id,
            seq: turn.seq,
            status: match turn.status {
                TurnStatus::Running => "running",
                TurnStatus::Completed => "completed",
                TurnStatus::Interrupted => "interrupted",
            },
            active_context: !turn.hidden,
            user_content: turn.display_content,
            assistant_content: redact_internal_assistant_text(&turn.assistant_content),
            assistant_reasoning: turn
                .assistant_reasoning
                .map(|reasoning| redact_internal_assistant_text(&reasoning)),
            provider_id: turn.assistant_provider_id,
            model: turn.assistant_model,
            user_timestamp: turn.user_timestamp,
            assistant_timestamp: turn.assistant_timestamp,
            token_total: turn.token_total,
            token_prompt: turn.token_prompt,
            token_cache_read: turn.token_cache_read,
            token_usage_estimated: turn.token_usage_estimated,
            question_exchanges: turn.question_exchanges,
            tool_flow: turn
                .tool_flow
                .into_iter()
                .map(SafeToolRound::from)
                .collect(),
            followups: turn.followups.into_iter().map(SafeFollowup::from).collect(),
            assets,
            artifacts: artifacts.into_iter().map(SafeArtifactAsset::from).collect(),
            attachments: turn
                .attachments
                .into_iter()
                .map(SafeUserAttachment::from)
                .collect(),
            revision: turn.revision,
        }
    }
}

pub(in crate::web) fn artifact_type_label(source_key: &str) -> String {
    let extension = FilePath::new(source_key)
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or_default()
        .to_ascii_uppercase();
    match extension.as_str() {
        "MARKDOWN" => "MD".to_string(),
        "HTML" | "HTM" => "HTML".to_string(),
        "JSONL" => "JSONL".to_string(),
        "JSON" => "JSON".to_string(),
        "PDF" => "PDF".to_string(),
        value if value.len() <= 6 && !value.is_empty() => value.to_string(),
        _ => "FILE".to_string(),
    }
}

pub(in crate::web) fn meme_asset_caption_hidden(asset: &ImageAsset, reports: &[String]) -> bool {
    const MAX_DESCRIPTION_CHARS: usize = 120;

    let description = asset.alt.split_whitespace().collect::<Vec<_>>().join(" ");
    if description.is_empty() {
        return false;
    }
    let mut characters = description.chars();
    let mut compact = characters
        .by_ref()
        .take(MAX_DESCRIPTION_CHARS)
        .collect::<String>();
    if characters.next().is_some() {
        compact.push('…');
    }
    let escaped = compact
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;");
    let marker = format!("description={escaped}</sent_meme>");
    reports
        .iter()
        .any(|report| report.starts_with("<sent_meme>") && report.contains(&marker))
}

impl From<TurnFollowup> for SafeFollowup {
    fn from(followup: TurnFollowup) -> Self {
        Self {
            id: followup.prompt_id,
            content: followup.display_content,
            submitted_at: followup.submitted_at,
            preceding_assistant_content: followup
                .preceding_assistant_content
                .map(|content| redact_internal_assistant_text(&content)),
            preceding_assistant_reasoning: followup
                .preceding_assistant_reasoning
                .map(|reasoning| redact_internal_assistant_text(&reasoning)),
            provider_id: followup.preceding_assistant_provider_id,
            model: followup.preceding_assistant_model,
            attachments: followup
                .uploaded_attachments
                .into_iter()
                .map(SafeUserAttachment::from)
                .collect(),
        }
    }
}

impl From<UsageSnapshot> for SafeUsageSnapshot {
    fn from(usage: UsageSnapshot) -> Self {
        Self {
            requests: usage.requests,
            prompt_tokens: usage.prompt_tokens,
            completion_tokens: usage.completion_tokens,
            total_tokens: usage.total_tokens,
            conversation_tokens: usage.conversation_tokens,
            cache_read_tokens: usage.cache_read_tokens,
            cache_write_tokens: usage.cache_write_tokens,
            reasoning_tokens: usage.reasoning_tokens,
            last_usage: usage.last_usage,
            last_conversation_usage: usage.last_conversation_usage,
        }
    }
}

pub(in crate::web) fn redact_internal_assistant_text(value: &str) -> String {
    value
        .replace(crate::state::pending_placeholder(), "")
        .replace(crate::state::interrupted_text(), "")
}

pub(in crate::web) fn validate_short_field(
    value: String,
    field: &str,
    max_chars: usize,
) -> std::result::Result<String, ApiError> {
    let value = value.trim().to_string();
    if value.is_empty() {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            format!("{field} cannot be empty"),
        ));
    }
    if value.chars().count() > max_chars || value.chars().any(char::is_control) {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            format!("{field} is invalid"),
        ));
    }
    Ok(value)
}

pub(in crate::web) fn validate_model_selection(
    models: Vec<ActiveProviderModelConfig>,
) -> std::result::Result<Vec<ActiveProviderModelConfig>, ApiError> {
    if models.is_empty() {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "at least one model endpoint must remain active",
        ));
    }
    if models.len() > 64 {
        return Err(ApiError::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            "at most 64 model endpoints can be active",
        ));
    }
    let mut seen = HashSet::with_capacity(models.len());
    let mut validated = Vec::with_capacity(models.len());
    for model in models {
        let provider_id = validate_short_field(model.provider_id, "provider_id", 200)?;
        let model = validate_short_field(model.model, "model", 500)?;
        if !seen.insert((provider_id.clone(), model.clone())) {
            return Err(ApiError::new(
                StatusCode::BAD_REQUEST,
                "duplicate provider/model selection",
            ));
        }
        validated.push(ActiveProviderModelConfig { provider_id, model });
    }
    Ok(validated)
}

pub(in crate::web) fn validate_thinking_variant_updates(
    updates: Vec<ThinkingVariantUpdate>,
) -> std::result::Result<Vec<ThinkingVariantUpdate>, ApiError> {
    if updates.is_empty() {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "at least one thinking variant update is required",
        ));
    }
    if updates.len() > MAX_THINKING_VARIANT_UPDATES {
        return Err(ApiError::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            format!("at most {MAX_THINKING_VARIANT_UPDATES} thinking variants can be updated"),
        ));
    }

    let mut seen = HashSet::with_capacity(updates.len());
    let mut validated = Vec::with_capacity(updates.len());
    for update in updates {
        let provider_id = validate_short_field(update.provider_id, "provider_id", 200)?;
        let model = validate_short_field(update.model, "model", 500)?;
        let selected = update
            .selected
            .map(|selected| validate_short_field(selected, "selected", 200))
            .transpose()?;
        if !seen.insert((provider_id.clone(), model.clone())) {
            return Err(ApiError::new(
                StatusCode::BAD_REQUEST,
                "duplicate provider/model thinking variant update",
            ));
        }
        validated.push(ThinkingVariantUpdate {
            provider_id,
            model,
            selected,
        });
    }
    Ok(validated)
}

pub(in crate::web) fn parse_mode(mode: &str) -> std::result::Result<AgentMode, ApiError> {
    match mode {
        "normal" => Ok(AgentMode::Normal),
        // 历史会话可能存过 plan：模式已移除，回落到普通模式而不是让会话打不开。
        "plan" => Ok(AgentMode::Normal),
        // 闲聊模式已删除:历史会话存过 "chat" 的回落普通模式,老会话照常打开。
        "chat" => Ok(AgentMode::Normal),
        "dev" => Ok(AgentMode::Dev),
        _ => Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "mode must be normal or dev",
        )),
    }
}

pub(in crate::web) fn mode_name(mode: AgentMode) -> &'static str {
    match mode {
        AgentMode::Normal => "normal",
        AgentMode::Dev => "dev",
    }
}

pub(in crate::web) fn real_tool_name(event_name: &str) -> &str {
    if event_name.starts_with("use_meme:") {
        "use_meme"
    } else if event_name.starts_with("load_skill:") {
        "load_skill"
    } else if event_name.starts_with("load_tools:") {
        "load_tools"
    } else {
        event_name
    }
}
