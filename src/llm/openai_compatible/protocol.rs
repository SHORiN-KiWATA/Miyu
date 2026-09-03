//! 线路协议的判定与思考变体。
//!
//! 同一个「模型」可能要走三种线：Chat Completions、OpenAI Responses、Anthropic
//! Messages。`effective_protocol` 按配置和供应商特征选一条，`auto` 时靠
//! `provider_looks_anthropic` 一类的启发式猜。
//!
//! 思考变体（reasoning effort / thinking budget）在三套协议里的字段名和取值范
//! 围全都不同，`reasoning_variant_supported_for_protocol` 挡住不支持的组合——
//! 发过去只会被静默忽略，用户以为开了其实没开。
//!
//! 变体偏好按「供应商 + 模型」存盘，供应商改名时要跟着改
//! （`rename_thinking_variant_entries`），否则设置会凭空消失。

use crate::llm::openai_compatible::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::llm::openai_compatible) enum ProviderProtocol {
    Auto,
    OpenAiChat,
    OpenAiResponses,
    Anthropic,
    /// 本机 Claude Code CLI 中转:传输层是子进程 stream-json,不是 HTTP。
    /// 只能显式配置,`auto` 永远不会猜到这条线。
    ClaudeCode,
    /// 本机 Antigravity CLI(`agy`)中转:同样是子进程 stream-json。
    Antigravity,
    /// 本机 OpenAI Codex CLI 中转:`codex exec --json` 的 JSONL。
    Codex,
}

impl ProviderProtocol {
    pub(in crate::llm::openai_compatible) fn from_provider(
        provider: &ProviderConfig,
    ) -> Result<Self> {
        match provider.protocol.trim().to_ascii_lowercase().as_str() {
            "" | "auto" => Ok(Self::Auto),
            "openai-chat" => Ok(Self::OpenAiChat),
            "openai-responses" => Ok(Self::OpenAiResponses),
            "anthropic" | "anthropic-messages" | "claude" | "claude-messages" => {
                Ok(Self::Anthropic)
            }
            "claude-code" | "claude-code-cli" => Ok(Self::ClaudeCode),
            "antigravity" | "antigravity-cli" | "agy" => Ok(Self::Antigravity),
            "codex" | "codex-cli" => Ok(Self::Codex),
            protocol => bail!("unsupported provider protocol: {protocol}"),
        }
    }
}

/// 端点池装配与 keepalive 分流用:该 provider 是否走 Claude Code CLI 中转。
/// CLI 用订阅登录态,没有 API key 概念,装配时要豁免 key 解析。
pub(in crate::llm::openai_compatible) fn provider_uses_claude_code(
    provider: &ProviderConfig,
) -> bool {
    matches!(
        ProviderProtocol::from_provider(provider),
        Ok(ProviderProtocol::ClaudeCode)
    )
}

/// 该 provider 是否走 Antigravity CLI 中转。
pub(in crate::llm::openai_compatible) fn provider_uses_antigravity(
    provider: &ProviderConfig,
) -> bool {
    matches!(
        ProviderProtocol::from_provider(provider),
        Ok(ProviderProtocol::Antigravity)
    )
}

/// 该 provider 是否走 Codex CLI 中转。
pub(in crate::llm::openai_compatible) fn provider_uses_codex(provider: &ProviderConfig) -> bool {
    matches!(
        ProviderProtocol::from_provider(provider),
        Ok(ProviderProtocol::Codex)
    )
}

/// 三条本机 CLI 中转线的合称:端点装配(无 API key)、keepalive 分流按它豁免。
pub(in crate::llm::openai_compatible) fn provider_uses_cli_relay(
    provider: &ProviderConfig,
) -> bool {
    provider_uses_claude_code(provider)
        || provider_uses_antigravity(provider)
        || provider_uses_codex(provider)
}

/// Codex 的思考档:config 的 `model_reasoning_effort` 五档,所有模型通用。
pub(in crate::llm::openai_compatible) fn codex_reasoning_variants(_model: &str) -> Vec<ReasoningVariant> {
    ["minimal", "low", "medium", "high", "xhigh"]
        .into_iter()
        .map(|effort| ReasoningVariant {
            id: effort.to_string(),
            setting: ReasoningSetting::Effort(effort.to_string()),
        })
        .collect()
}

/// Antigravity 的思考档:CLI 的 `--effort` 三档。gemini/gpt-oss 模型名自带
/// 档位后缀(-high/-low),只给 claude-* 模型暴露 effort。
pub(in crate::llm::openai_compatible) fn antigravity_reasoning_variants(
    model: &str,
) -> Vec<ReasoningVariant> {
    if !model.starts_with("claude-") {
        return Vec::new();
    }
    ["low", "medium", "high"]
        .into_iter()
        .map(|effort| ReasoningVariant {
            id: effort.to_string(),
            setting: ReasoningSetting::Effort(effort.to_string()),
        })
        .collect()
}

pub(in crate::llm::openai_compatible) fn effective_protocol(
    provider: &ProviderConfig,
    model: &str,
) -> Result<ProviderProtocol> {
    match ProviderProtocol::from_provider(provider)? {
        ProviderProtocol::Auto if provider_looks_anthropic(provider) => {
            Ok(ProviderProtocol::Anthropic)
        }
        ProviderProtocol::Auto if uses_openai_responses(model) => {
            Ok(ProviderProtocol::OpenAiResponses)
        }
        ProviderProtocol::Auto => Ok(ProviderProtocol::OpenAiChat),
        protocol => Ok(protocol),
    }
}

pub(in crate::llm::openai_compatible) fn uses_openai_responses(model: &str) -> bool {
    let model = model.to_ascii_lowercase();
    model.starts_with("gpt-5")
        || model.starts_with("o1")
        || model.starts_with("o3")
        || model.starts_with("o4")
}

pub(in crate::llm::openai_compatible) fn is_openrouter_provider(provider: &ProviderConfig) -> bool {
    provider.id.eq_ignore_ascii_case("openrouter")
        || provider
            .base_url
            .to_ascii_lowercase()
            .contains("openrouter.ai")
}

pub(in crate::llm::openai_compatible) fn uses_enable_thinking(
    provider: &ProviderConfig,
    info: &ModelReasoningInfo,
) -> bool {
    info.provider_npm.as_deref() == Some("@ai-sdk/alibaba")
        || provider.id.to_ascii_lowercase().contains("alibaba")
        || provider
            .base_url
            .to_ascii_lowercase()
            .contains("dashscope.aliyuncs.com")
}

pub(in crate::llm::openai_compatible) fn anthropic_reasoning_budget(
    max_tokens: u32,
    requested: u64,
) -> Option<u64> {
    (max_tokens > 1024 && requested < u64::from(max_tokens)).then_some(requested)
}

/// Claude Code 的思考档:CLI 的 `--effort` 五档,不走 models.dev 目录
/// (那里没有这个"供应商")。haiku 不支持调整思考力度,不给档
/// (用户 08-20 裁定)。
pub(in crate::llm::openai_compatible) fn claude_code_reasoning_variants(
    model: &str,
) -> Vec<ReasoningVariant> {
    if model == "haiku" {
        return Vec::new();
    }
    ["low", "medium", "high", "xhigh", "max"]
        .into_iter()
        .map(|effort| ReasoningVariant {
            id: effort.to_string(),
            setting: ReasoningSetting::Effort(effort.to_string()),
        })
        .collect()
}

pub(in crate::llm::openai_compatible) fn supported_reasoning_variants(
    provider: &ProviderConfig,
    model: &str,
) -> Vec<ReasoningVariant> {
    if provider_uses_claude_code(provider) {
        return claude_code_reasoning_variants(model);
    }
    if provider_uses_antigravity(provider) {
        return antigravity_reasoning_variants(model);
    }
    if provider_uses_codex(provider) {
        return codex_reasoning_variants(model);
    }
    let Some(info) = models_cache::reasoning_info(&provider.id, model) else {
        return Vec::new();
    };
    info.variants
        .iter()
        .filter(|variant| reasoning_variant_supported(provider, model, &info, variant))
        .cloned()
        .collect()
}

pub(in crate::llm::openai_compatible) fn reasoning_variant_supported(
    provider: &ProviderConfig,
    model: &str,
    info: &ModelReasoningInfo,
    variant: &ReasoningVariant,
) -> bool {
    let Ok(protocol) = effective_protocol(provider, model) else {
        return false;
    };
    reasoning_variant_supported_for_protocol(provider, info, variant, protocol)
}

pub(in crate::llm::openai_compatible) fn reasoning_variant_supported_for_protocol(
    provider: &ProviderConfig,
    info: &ModelReasoningInfo,
    variant: &ReasoningVariant,
    protocol: ProviderProtocol,
) -> bool {
    match protocol {
        // Claude Code 只认 `--effort` 的档位语义。
        ProviderProtocol::ClaudeCode | ProviderProtocol::Antigravity | ProviderProtocol::Codex => {
            matches!(variant.setting, ReasoningSetting::Effort(_))
        }
        ProviderProtocol::OpenAiResponses => matches!(
            variant.setting,
            ReasoningSetting::Effort(_) | ReasoningSetting::Toggle(_) | ReasoningSetting::Disabled
        ),
        ProviderProtocol::Anthropic => match variant.setting {
            ReasoningSetting::BudgetTokens(budget) => {
                anthropic_reasoning_budget(provider.anthropic_max_tokens, budget).is_some()
            }
            _ => true,
        },
        ProviderProtocol::OpenAiChat | ProviderProtocol::Auto => {
            let npm = info.provider_npm.as_deref().unwrap_or_default();
            if is_openrouter_provider(provider) || npm == "@openrouter/ai-sdk-provider" {
                matches!(
                    variant.setting,
                    ReasoningSetting::Effort(_) | ReasoningSetting::BudgetTokens(_)
                )
            } else if matches!(variant.setting, ReasoningSetting::Effort(_)) {
                true
            } else if uses_enable_thinking(provider, info) {
                matches!(variant.setting, ReasoningSetting::Toggle(_))
            } else {
                false
            }
        }
    }
}

pub(in crate::llm::openai_compatible) fn thinking_variant_key(
    provider_id: &str,
    model: &str,
) -> String {
    format!("{provider_id}\t{model}")
}

pub(in crate::llm::openai_compatible) fn rename_thinking_variant_entries<T>(
    entries: &mut HashMap<String, T>,
    old_id: &str,
    new_id: &str,
) {
    let prefix = format!("{old_id}\t");
    let renamed = entries
        .keys()
        .filter_map(|key| {
            key.strip_prefix(&prefix)
                .map(|model| (key.clone(), thinking_variant_key(new_id, model)))
        })
        .collect::<Vec<_>>();
    for (old_key, new_key) in renamed {
        if let Some(value) = entries.remove(&old_key) {
            entries.insert(new_key, value);
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(crate) struct ThinkingVariantPreferences {
    #[serde(default)]
    pub(in crate::llm::openai_compatible) selected: HashMap<String, String>,
    #[serde(skip)]
    pub(in crate::llm::openai_compatible) changes: HashMap<String, Option<String>>,
    #[serde(skip)]
    pub(in crate::llm::openai_compatible) provider_renames: Vec<(String, String)>,
}

pub(in crate::llm::openai_compatible) fn thinking_variant_preferences_file(
    paths: &MiyuPaths,
) -> PathBuf {
    paths.state_dir.join("thinking-variants.json")
}

pub(in crate::llm::openai_compatible) fn lock_thinking_variant_preferences(
    paths: &MiyuPaths,
) -> Result<File> {
    let lock_path = paths.state_dir.join("thinking-variants.lock");
    let lock = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .open(&lock_path)
        .with_context(|| {
            format!(
                "failed to open thinking variant lock: {}",
                lock_path.display()
            )
        })?;
    let result = unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) };
    if result != 0 {
        return Err(std::io::Error::last_os_error()).with_context(|| {
            format!(
                "failed to lock thinking variant state: {}",
                lock_path.display()
            )
        });
    }
    Ok(lock)
}

pub(in crate::llm::openai_compatible) fn load_thinking_variant_preferences(
    paths: &MiyuPaths,
) -> ThinkingVariantPreferences {
    ThinkingVariantPreferences::load(paths)
}

impl ThinkingVariantPreferences {
    pub(crate) fn load(paths: &MiyuPaths) -> Self {
        Self::load_for_update(paths).unwrap_or_default()
    }

    pub(in crate::llm::openai_compatible) fn load_for_update(paths: &MiyuPaths) -> Result<Self> {
        let path = thinking_variant_preferences_file(paths);
        match std::fs::read_to_string(&path) {
            Ok(text) => serde_json::from_str(&text).with_context(|| {
                format!("failed to parse thinking variant state: {}", path.display())
            }),
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(Self::default()),
            Err(error) => Err(error).with_context(|| {
                format!("failed to read thinking variant state: {}", path.display())
            }),
        }
    }

    pub(crate) fn selected(&self, provider_id: &str, model: &str) -> Option<&str> {
        self.selected
            .get(&thinking_variant_key(provider_id, model))
            .map(String::as_str)
    }

    pub(crate) fn set(&mut self, provider_id: &str, model: &str, selected: Option<String>) {
        let key = thinking_variant_key(provider_id, model);
        let selected = selected.filter(|value| !value.trim().is_empty());
        if self.selected.get(&key).map(String::as_str) == selected.as_deref() {
            return;
        }
        if let Some(selected) = &selected {
            self.selected.insert(key.clone(), selected.clone());
        } else {
            self.selected.remove(&key);
        }
        self.changes.insert(key, selected);
    }

    pub(crate) fn rename_provider(&mut self, old_id: &str, new_id: &str) {
        if old_id == new_id {
            return;
        }
        rename_thinking_variant_entries(&mut self.selected, old_id, new_id);
        rename_thinking_variant_entries(&mut self.changes, old_id, new_id);
        self.provider_renames
            .push((old_id.to_string(), new_id.to_string()));
    }

    /// True when `save` would write anything to disk.
    pub(crate) fn is_dirty(&self) -> bool {
        !self.changes.is_empty() || !self.provider_renames.is_empty()
    }

    pub(crate) fn save(&self, paths: &MiyuPaths) -> Result<()> {
        if self.changes.is_empty() && self.provider_renames.is_empty() {
            return Ok(());
        }

        let path = thinking_variant_preferences_file(paths);
        let parent = path
            .parent()
            .ok_or_else(|| anyhow::anyhow!("thinking variant state path has no parent"))?;
        std::fs::create_dir_all(parent)?;
        let _lock = lock_thinking_variant_preferences(paths)?;
        let mut persisted = Self::load_for_update(paths)?;
        for (old_id, new_id) in &self.provider_renames {
            rename_thinking_variant_entries(&mut persisted.selected, old_id, new_id);
        }
        for (key, selected) in &self.changes {
            if let Some(selected) = selected {
                persisted.selected.insert(key.clone(), selected.clone());
            } else {
                persisted.selected.remove(key);
            }
        }
        let mut temp = tempfile::NamedTempFile::new_in(parent)?;
        temp.write_all(serde_json::to_string_pretty(&persisted)?.as_bytes())?;
        temp.persist(path).map_err(|error| error.error)?;
        Ok(())
    }
}

pub(in crate::llm::openai_compatible) fn chat_variant_body(
    provider: &ProviderConfig,
    info: &ModelReasoningInfo,
    setting: ReasoningSetting,
) -> Option<Map<String, Value>> {
    let npm = info.provider_npm.as_deref().unwrap_or_default();
    match setting {
        ReasoningSetting::Effort(effort)
            if is_openrouter_provider(provider) || npm == "@openrouter/ai-sdk-provider" =>
        {
            Some(
                json!({ "reasoning": { "effort": effort } })
                    .as_object()?
                    .clone(),
            )
        }
        ReasoningSetting::BudgetTokens(budget)
            if is_openrouter_provider(provider) || npm == "@openrouter/ai-sdk-provider" =>
        {
            Some(
                json!({ "reasoning": { "max_tokens": budget } })
                    .as_object()?
                    .clone(),
            )
        }
        ReasoningSetting::Effort(effort) => {
            Some(json!({ "reasoning_effort": effort }).as_object()?.clone())
        }
        ReasoningSetting::Toggle(enabled) if uses_enable_thinking(provider, info) => {
            Some(json!({ "enable_thinking": enabled }).as_object()?.clone())
        }
        _ => None,
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ThinkingVariantOptions {
    pub provider_id: String,
    pub model: String,
    pub variants: Vec<String>,
    pub selected: Option<String>,
}

pub(crate) fn thinking_variant_options_for_model(
    provider: &ProviderConfig,
    model: &str,
    selected: Option<&str>,
) -> ThinkingVariantOptions {
    let variants = supported_reasoning_variants(provider, model)
        .into_iter()
        .map(|variant| variant.id)
        .collect::<Vec<_>>();
    let selected = selected
        .filter(|selected| variants.iter().any(|variant| variant == *selected))
        .map(str::to_string);
    ThinkingVariantOptions {
        provider_id: provider.id.clone(),
        model: model.to_string(),
        variants,
        selected,
    }
}

pub(in crate::llm::openai_compatible) fn reasoning_visibility(
    config: &AppConfig,
) -> ReasoningVisibility {
    match config
        .display
        .reasoning
        .trim()
        .to_ascii_lowercase()
        .as_str()
    {
        "hidden" => ReasoningVisibility::Hidden,
        "full" => ReasoningVisibility::Full,
        _ => ReasoningVisibility::Summary,
    }
}

pub(in crate::llm::openai_compatible) fn reasoning_summary_is_detailed(config: &AppConfig) -> bool {
    config.display.reasoning.trim().eq_ignore_ascii_case("full")
}

pub(in crate::llm::openai_compatible) fn provider_looks_anthropic(
    provider: &ProviderConfig,
) -> bool {
    let id = provider.id.to_ascii_lowercase();
    let display_name = provider.display_name.to_ascii_lowercase();
    let base_url = provider.base_url.to_ascii_lowercase();
    id == "anthropic"
        || id == "claude"
        || id.contains("anthropic")
        || display_name.contains("anthropic")
        || base_url.contains("api.anthropic.com")
        || base_url.contains("anthropic.com/v1")
}

pub(in crate::llm::openai_compatible) fn provider_looks_claude_related(
    provider: &ProviderConfig,
) -> bool {
    let id = provider.id.to_ascii_lowercase();
    let display_name = provider.display_name.to_ascii_lowercase();
    let base_url = provider.base_url.to_ascii_lowercase();
    let model = provider.default_model.to_ascii_lowercase();
    provider_looks_anthropic(provider)
        || id.contains("claude")
        || display_name.contains("claude")
        || model.starts_with("claude")
        || base_url.contains("claude")
}

pub(in crate::llm::openai_compatible) fn claude_protocol_hint(
    provider: &ProviderConfig,
) -> &'static str {
    let protocol = provider.protocol.trim();
    if (protocol.is_empty()
        || protocol.eq_ignore_ascii_case("auto")
        || protocol.eq_ignore_ascii_case("openai-chat"))
        && provider_looks_claude_related(provider)
        && !provider_looks_anthropic(provider)
    {
        return "\nHint: if this provider is the official Anthropic Claude API, set provider protocol to anthropic and base_url to https://api.anthropic.com/v1. If it is an OpenAI-compatible Claude proxy, keep openai-chat/auto.";
    }
    ""
}

pub(in crate::llm::openai_compatible) fn anthropic_thinking_config() -> Value {
    json!({ "type": "adaptive", "display": "summarized" })
}

/// DeepSeek thinking mode 400s an assistant tool_calls turn whose
/// `reasoning_content` KEY is absent from the request JSON, while many other
/// OpenAI-compatible gateways reject the unknown field outright. Send the key
/// only to providers known to understand it and strip it everywhere else, so
/// the transport copy stays byte-identical to the pre-A17 shape on unrelated
/// endpoints (prompt-cache prefix preserved).
pub(in crate::llm::openai_compatible) fn provider_accepts_reasoning_content(
    provider: &ProviderConfig,
) -> bool {
    let haystack = format!(
        "{} {} {}",
        provider.id.to_ascii_lowercase(),
        provider.base_url.to_ascii_lowercase(),
        provider.default_model.to_ascii_lowercase()
    );
    ["deepseek", "glm-", "zhipu", "bigmodel", "kimi", "moonshot"]
        .iter()
        .any(|needle| haystack.contains(needle))
}
