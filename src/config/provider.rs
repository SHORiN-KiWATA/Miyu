//! 供应商、模型与它们的能力标签。
//!
//! `ProviderModelChoice` 是「哪个供应商的哪个模型」的唯一表示，界面上到处在用
//! 它做下拉选项。`resolve_provider_model_argument` 把命令行传进来的字符串还原
//! 成它，容忍几种写法但拒绝歧义。
//!
//! 能力（视觉、嵌入、思考）是**每个模型**的属性而不是供应商的：同一个供应商下
//! 既有能看图的也有不能的，池里随机选一个就会随机失败。

use crate::config::*;

/// Subagent model tier pools. When the main agent spawns a subagent it
/// picks a tier by task complexity (cheap/balanced/strong); requests then
/// load-balance across that tier's pool exactly like the main text-model
/// pool. Tiers are subagent-only — the main conversation and auxiliary
/// work always use the user-selected main models. An unconfigured or
/// unavailable pool falls back to the main model pool.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SubagentTiersConfig {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub cheap: Vec<ActiveProviderModelConfig>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub balanced: Vec<ActiveProviderModelConfig>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub strong: Vec<ActiveProviderModelConfig>,
}

impl SubagentTiersConfig {
    pub fn is_empty(&self) -> bool {
        self.cheap.is_empty() && self.balanced.is_empty() && self.strong.is_empty()
    }

    pub fn pool(&self, tier: ModelTier) -> &Vec<ActiveProviderModelConfig> {
        match tier {
            ModelTier::Cheap => &self.cheap,
            ModelTier::Balanced => &self.balanced,
            ModelTier::Strong => &self.strong,
        }
    }

    pub fn pool_mut(&mut self, tier: ModelTier) -> &mut Vec<ActiveProviderModelConfig> {
        match tier {
            ModelTier::Cheap => &mut self.cheap,
            ModelTier::Balanced => &mut self.balanced,
            ModelTier::Strong => &mut self.strong,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelTier {
    Cheap,
    Balanced,
    Strong,
}

impl ModelTier {
    pub const ALL: [Self; 3] = [Self::Cheap, Self::Balanced, Self::Strong];

    pub fn from_str(value: &str) -> Option<Self> {
        match value.trim() {
            "cheap" => Some(Self::Cheap),
            "balanced" => Some(Self::Balanced),
            "strong" => Some(Self::Strong),
            _ => None,
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            Self::Cheap => "cheap",
            Self::Balanced => "balanced",
            Self::Strong => "strong",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActiveProviderModelConfig {
    pub provider_id: String,
    pub model: String,
}

/// Claude Code 特殊供应商的内部协议标识(不暴露成用户概念)。
pub const CLAUDE_CODE_PROTOCOL: &str = "claude-code";
/// Antigravity(agy CLI)特殊供应商的内部协议标识(不暴露成用户概念)。
pub const ANTIGRAVITY_PROTOCOL: &str = "antigravity";
/// Codex(OpenAI codex CLI)特殊供应商的内部协议标识。
pub const CODEX_PROTOCOL: &str = "codex";
/// Codex 预置模型:`codex debug models` 的目录(09-03,codex 0.147)。
pub const CODEX_PRESET_MODELS: &[&str] = &[
    "gpt-5.6-terra",
    "gpt-5.6-sol",
    "gpt-5.6-luna",
    "gpt-5.5",
    "gpt-5.4",
    "gpt-5.4-mini",
    "gpt-5.2",
];
/// Antigravity 预置模型:`agy models` 的输出(09-03);本机 CLI 没有 /models
/// 端点,列表就是这份别名。gemini 的思考档位编码在模型名后缀里。
pub const ANTIGRAVITY_PRESET_MODELS: &[&str] = &[
    "gemini-3.8-flash-high",
    "gemini-3.8-flash-medium",
    "gemini-3.8-flash-low",
    "gemini-3.7-flash-high",
    "gemini-3.7-flash-medium",
    "gemini-3.7-flash-low",
    "gemini-3.6-flash-high",
    "gemini-3.6-flash-medium",
    "gemini-3.6-flash-low",
    "gemini-3.1-pro-high",
    "gemini-3.1-pro-low",
    "claude-sonnet-4-6",
    "claude-opus-4-6-thinking",
    "gpt-oss-120b-medium",
];

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderConfig {
    pub id: String,
    pub display_name: String,
    pub base_url: String,
    /// 供应商总开关。目前只有内置的 Claude Code 特殊供应商默认关(要用户
    /// 显式启用订阅中转);普通 HTTP 供应商恒为 true 且不落盘。
    #[serde(default = "default_true", skip_serializing_if = "bool_is_true")]
    pub enabled: bool,
    #[serde(
        default = "default_provider_protocol",
        skip_serializing_if = "is_auto_protocol"
    )]
    pub protocol: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub models: Vec<String>,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub model_context_window: HashMap<String, usize>,
    /// 按模型温度覆盖;缺项回退 `temperature`(供应商默认)。验收:模型
    /// 菜单里的温度曾误写供应商全局,牵连所有模型。
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub model_temperature: HashMap<String, f32>,
    /// 按模型工具加载模式覆盖("full"/"stub");缺项回退全局
    /// `tools.loading_mode`。约束解码型模型(如 bigmodel glm-5.3-flash)把
    /// 参数生成硬限制在声明 schema 内,吃不下空壳 stub,给它们单独配 full。
    /// 池级解析取最保守,见 `tools::effective_tools_loading_mode`(09-01)。
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub model_tools_loading_mode: HashMap<String, String>,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub model_modalities: HashMap<String, Vec<String>>,
    /// 手动模型价格,键为模型名;设了就覆盖 models.dev 目录价。
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub model_costs: HashMap<String, ModelCostConfig>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub default_model: String,
    #[serde(
        default = "default_timeout",
        skip_serializing_if = "is_default_timeout"
    )]
    pub timeout_seconds: u64,
    #[serde(
        default = "default_temperature",
        skip_serializing_if = "is_default_temperature"
    )]
    pub temperature: f32,
    #[serde(
        default = "default_anthropic_max_tokens",
        skip_serializing_if = "is_default_anthropic_max_tokens"
    )]
    pub anthropic_max_tokens: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub extra_body: Option<serde_json::Map<String, serde_json::Value>>,
}

#[derive(Debug, Clone)]
pub struct ResolvedProviderKey {
    pub index: usize,
    pub value: String,
}

#[derive(Debug, Clone)]
pub struct ProviderModelChoice {
    pub provider_id: String,
    pub provider_name: String,
    pub model: String,
}

impl ProviderModelChoice {
    pub fn value(&self) -> String {
        format!("{}\t{}", self.provider_id, self.model)
    }

    pub fn label(&self) -> String {
        format!("{} / {}", self.provider_name, self.model)
    }
}

/// Resolves a user-supplied model argument against `choices`: a 1-based list
/// index, a fully-qualified `provider_id/model`, or a bare model name when it
/// is unambiguous. The error is a ready-to-display bilingual message.
pub fn resolve_provider_model_argument<'a>(
    choices: &'a [ProviderModelChoice],
    argument: &str,
) -> std::result::Result<&'a ProviderModelChoice, String> {
    use crate::i18n::text as t;
    let argument = argument.trim();
    if let Ok(index) = argument.parse::<usize>() {
        return choices.get(index.wrapping_sub(1)).ok_or_else(|| {
            format!(
                "{} 1..={}",
                t(
                    "The model index is out of range; valid range:",
                    "模型序号超出范围，有效范围："
                ),
                choices.len()
            )
        });
    }
    // Fully-qualified "provider_id/model". Model ids may themselves contain
    // '/', so match by provider prefix instead of splitting at the first '/'.
    if let Some(choice) = choices.iter().find(|choice| {
        argument
            .strip_prefix(choice.provider_id.as_str())
            .and_then(|rest| rest.strip_prefix('/'))
            .is_some_and(|model| model == choice.model)
    }) {
        return Ok(choice);
    }
    let matches: Vec<&ProviderModelChoice> = choices
        .iter()
        .filter(|choice| choice.model == argument)
        .collect();
    match matches.as_slice() {
        [choice] => Ok(choice),
        [] => Err(format!(
            "{}{argument}",
            t("No configured model matches: ", "没有匹配的已配置模型：")
        )),
        multiple => Err(format!(
            "{}\n{}",
            t(
                "Multiple providers offer this model; use one of:",
                "多个供应商都提供该模型，请使用以下之一："
            ),
            multiple
                .iter()
                .map(|choice| format!("{}/{}", choice.provider_id, choice.model))
                .collect::<Vec<_>>()
                .join("\n")
        )),
    }
}

/// Which model turns text into vectors, and the settings that belong to that
/// model rather than to any one feature — a similarity floor means different
/// things on different models. Deliberately has no on/off switch: configuring a
/// model only makes it available, and each feature decides whether to use it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct EmbeddingConfig {
    /// Id of an existing provider; the model is named separately, so a provider
    /// serving both chat and embedding models is still configured once.
    pub provider_id: String,
    pub model: String,
    pub timeout_seconds: u64,
    /// Cosine similarity below this is not a hit.
    pub min_score: f32,
}

impl Default for EmbeddingConfig {
    fn default() -> Self {
        Self {
            provider_id: String::new(),
            model: String::new(),
            timeout_seconds: 60,
            min_score: 0.35,
        }
    }
}

/// Marks a model as producing vectors rather than chat.
pub const EMBEDDING_MODALITY: &str = "embedding";

impl EmbeddingConfig {
    pub(crate) fn is_default(&self) -> bool {
        *self == Self::default()
    }

    /// A model is configured; whether any feature uses it is that feature's
    /// business.
    pub fn is_configured(&self) -> bool {
        !self.provider_id.trim().is_empty() && !self.model.trim().is_empty()
    }
}

impl ProviderConfig {
    /// 当前选中模型(`default_model`)的有效温度:按模型覆盖优先,缺项
    /// 回退供应商默认。
    pub fn effective_temperature(&self) -> f32 {
        self.model_temperature
            .get(&self.default_model)
            .copied()
            .unwrap_or(self.temperature)
    }

    pub fn default_opencodezen() -> Self {
        Self {
            id: OPENCODE_PROVIDER_ID.to_string(),
            display_name: "opencode Zen".to_string(),
            base_url: OPENCODE_ZEN_BASE_URL.to_string(),
            enabled: true,
            protocol: default_provider_protocol(),
            api_key: None,
            models: vec![OPENCODE_DEFAULT_CHAT_MODEL.to_string()],
            model_context_window: HashMap::new(),
            model_temperature: HashMap::new(),
            model_tools_loading_mode: HashMap::new(),
            model_modalities: HashMap::new(),
            model_costs: HashMap::new(),
            default_model: OPENCODE_DEFAULT_CHAT_MODEL.to_string(),
            timeout_seconds: default_timeout(),
            temperature: default_temperature(),
            anthropic_max_tokens: default_anthropic_max_tokens(),
            extra_body: None,
        }
    }

    pub fn default_anthropic() -> Self {
        Self {
            id: "anthropic".to_string(),
            display_name: "Anthropic".to_string(),
            base_url: "https://api.anthropic.com/v1".to_string(),
            enabled: true,
            protocol: "anthropic".to_string(),
            api_key: Some("$env:ANTHROPIC_API_KEY".to_string()),
            models: Vec::new(),
            model_context_window: HashMap::new(),
            model_temperature: HashMap::new(),
            model_tools_loading_mode: HashMap::new(),
            model_modalities: HashMap::new(),
            model_costs: HashMap::new(),
            default_model: String::new(),
            timeout_seconds: default_timeout(),
            temperature: default_temperature(),
            anthropic_max_tokens: default_anthropic_max_tokens(),
            extra_body: None,
        }
    }

    /// 内置的 Claude Code 特殊供应商:不是 HTTP 端点,是本机 `claude` CLI 的
    /// 订阅中转。恒存在于供应商列表、默认禁用;base_url/api_key/协议对它没有
    /// 意义,模型列表预置 CLI 认识的别名,思考档接 `--effort`。
    pub fn claude_code_template() -> Self {
        Self {
            enabled: false,
            protocol: CLAUDE_CODE_PROTOCOL.to_string(),
            models: ["fable", "opus", "sonnet", "haiku"]
                .into_iter()
                .map(str::to_string)
                .collect(),
            default_model: "sonnet".to_string(),
            ..Self::template("claude-code", "Claude Code", "")
        }
    }

    /// 该条目是否 Claude Code 特殊供应商(按协议判定,协议是内部实现细节,
    /// 不暴露在 TUI 表单里)。
    pub fn is_claude_code(&self) -> bool {
        let protocol = self.protocol.trim();
        protocol.eq_ignore_ascii_case(CLAUDE_CODE_PROTOCOL)
            || protocol.eq_ignore_ascii_case("claude-code-cli")
    }

    /// 内置的 Antigravity 特殊供应商:本机 `agy` CLI 的 Google 登录态中转。
    /// 形态与 Claude Code 完全同构(恒存在、默认禁用、无 HTTP 字段)。
    pub fn antigravity_template() -> Self {
        Self {
            enabled: false,
            protocol: ANTIGRAVITY_PROTOCOL.to_string(),
            models: ANTIGRAVITY_PRESET_MODELS
                .iter()
                .map(|model| model.to_string())
                .collect(),
            default_model: "gemini-3.8-flash-high".to_string(),
            ..Self::template("antigravity", "Antigravity", "")
        }
    }

    /// 内置的 Codex 特殊供应商:本机 `codex` CLI 的 ChatGPT 登录态中转。
    pub fn codex_template() -> Self {
        Self {
            enabled: false,
            protocol: CODEX_PROTOCOL.to_string(),
            models: CODEX_PRESET_MODELS.iter().map(|model| model.to_string()).collect(),
            default_model: "gpt-5.6-terra".to_string(),
            ..Self::template("codex", "Codex", "")
        }
    }

    /// 该条目是否 Codex 特殊供应商(按协议判定)。
    pub fn is_codex(&self) -> bool {
        let protocol = self.protocol.trim();
        protocol.eq_ignore_ascii_case(CODEX_PROTOCOL) || protocol.eq_ignore_ascii_case("codex-cli")
    }

    /// 该条目是否 Antigravity 特殊供应商(按协议判定)。
    pub fn is_antigravity(&self) -> bool {
        let protocol = self.protocol.trim();
        protocol.eq_ignore_ascii_case(ANTIGRAVITY_PROTOCOL)
            || protocol.eq_ignore_ascii_case("antigravity-cli")
            || protocol.eq_ignore_ascii_case("agy")
    }

    /// 内置的本机 CLI 中转供应商(Claude Code / Antigravity):没有 URL、
    /// API key 概念,列表里恒存在且不可删除。
    pub fn is_builtin_cli_provider(&self) -> bool {
        self.is_claude_code() || self.is_antigravity() || self.is_codex()
    }

    pub fn default_templates() -> Vec<Self> {
        let mut providers = vec![Self::default_opencodezen()];
        providers.extend([
            Self::template("opencodego", "OpenCode Go", "https://opencode.ai/zen/go/v1"),
            Self::template("openai", "OpenAI", "https://api.openai.com/v1"),
            Self::default_anthropic(),
            Self::template("deepseek", "DeepSeek", "https://api.deepseek.com"),
            Self::template(
                "gemini",
                "Gemini",
                "https://generativelanguage.googleapis.com/v1beta/openai",
            ),
            Self::template(
                "xiaomi",
                "Xiaomi",
                "https://token-plan-sgp.xiaomimimo.com/v1",
            ),
            Self::template("minimax", "Minimax", "https://api.minimaxi.com/v1"),
            Self::template("openrouter", "OpenRouter", "https://openrouter.ai/api/v1"),
            Self::template("ollama", "Ollama", "http://localhost:11434/v1"),
            Self::template("lmstudio", "LMStudio", "http://localhost:1234/v1"),
        ]);
        // Claude Code 置顶:用户拍板的列表次序;Antigravity 紧随其后。
        providers.insert(0, Self::claude_code_template());
        providers.insert(1, Self::antigravity_template());
        providers.insert(2, Self::codex_template());
        providers
    }

    pub(crate) fn template(id: &str, display_name: &str, base_url: &str) -> Self {
        Self {
            id: id.to_string(),
            display_name: display_name.to_string(),
            base_url: base_url.to_string(),
            enabled: true,
            protocol: default_provider_protocol(),
            api_key: None,
            models: Vec::new(),
            model_context_window: HashMap::new(),
            model_temperature: HashMap::new(),
            model_tools_loading_mode: HashMap::new(),
            model_modalities: HashMap::new(),
            model_costs: HashMap::new(),
            default_model: String::new(),
            timeout_seconds: default_timeout(),
            temperature: default_temperature(),
            anthropic_max_tokens: default_anthropic_max_tokens(),
            extra_body: None,
        }
    }

    pub fn new_custom() -> Self {
        Self {
            id: String::new(),
            display_name: String::new(),
            base_url: String::new(),
            enabled: true,
            protocol: default_provider_protocol(),
            api_key: None,
            models: Vec::new(),
            model_context_window: HashMap::new(),
            model_temperature: HashMap::new(),
            model_tools_loading_mode: HashMap::new(),
            model_modalities: HashMap::new(),
            model_costs: HashMap::new(),
            default_model: String::new(),
            timeout_seconds: default_timeout(),
            temperature: default_temperature(),
            anthropic_max_tokens: default_anthropic_max_tokens(),
            extra_body: None,
        }
    }

    pub fn supports_vision(&self, model: &str) -> Option<bool> {
        self.input_modalities(model)
            .map(|modalities| modalities.iter().any(|m| m == "image"))
    }

    pub fn input_modalities(&self, model: &str) -> Option<Vec<String>> {
        if let Some(modalities) = self.model_modalities.get(model) {
            return Some(modalities.clone());
        }
        crate::models_cache::input_modalities(&self.id, model)
    }

    pub fn resolved_api_keys(&self, _paths: &MiyuPaths) -> Result<Vec<ResolvedProviderKey>> {
        let mut keys = Vec::new();
        if let Some(api_key) = self.api_key.as_deref() {
            append_resolved_api_keys(&mut keys, api_key)?;
        }

        if keys.is_empty() && self.is_opencode_zen() {
            keys.push(ResolvedProviderKey {
                index: 0,
                value: "public".to_string(),
            });
        }

        if keys.is_empty() {
            bail!("missing API key for provider {}", self.id)
        }
        for (index, key) in keys.iter_mut().enumerate() {
            key.index = index;
        }
        Ok(keys)
    }

    pub fn is_opencode_zen(&self) -> bool {
        matches!(self.id.as_str(), OPENCODE_PROVIDER_ID | "opencodezen")
            && self.base_url.trim_end_matches('/') == OPENCODE_ZEN_BASE_URL
    }

    pub(crate) fn has_configured_model(&self, model: &str) -> bool {
        let model = model.trim();
        !model.is_empty()
            && (self.default_model == model || self.models.iter().any(|item| item == model))
    }

    pub(crate) fn is_legacy_default_anthropic_model(&self) -> bool {
        self.id == "anthropic"
            && self.base_url.trim_end_matches('/') == "https://api.anthropic.com/v1"
            && self.protocol == "anthropic"
            && self.api_key.as_deref() == Some("$env:ANTHROPIC_API_KEY")
            && self.models == ["claude-sonnet-4-5"]
            && self.default_model == "claude-sonnet-4-5"
    }
}

pub(crate) fn append_resolved_api_keys(
    out: &mut Vec<ResolvedProviderKey>,
    raw: &str,
) -> Result<()> {
    for item in split_api_keys(raw) {
        let value = if let Some(env_name) = item.strip_prefix("$env:") {
            std::env::var(env_name)
                .with_context(|| format!("environment variable {env_name} is not set"))?
        } else {
            item.to_string()
        };
        let value = value.trim();
        if !value.is_empty() {
            out.push(ResolvedProviderKey {
                index: out.len(),
                value: value.to_string(),
            });
        }
    }
    Ok(())
}

pub(crate) fn split_api_keys(raw: &str) -> Vec<&str> {
    raw.lines()
        .flat_map(|line| line.split(','))
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .collect()
}

pub(crate) fn active_model_exists(
    providers: &[ProviderConfig],
    active: &ActiveProviderModelConfig,
) -> bool {
    providers
        .iter()
        .find(|provider| provider.id == active.provider_id.trim())
        .is_some_and(|provider| provider.has_configured_model(&active.model))
}

pub(crate) fn active_model_supports_image(
    providers: &[ProviderConfig],
    active: &ActiveProviderModelConfig,
) -> bool {
    providers
        .iter()
        .find(|provider| provider.id == active.provider_id.trim())
        .filter(|provider| provider.has_configured_model(&active.model))
        .and_then(|provider| provider.input_modalities(&active.model))
        .is_some_and(|modalities| modalities.iter().any(|input| input == "image"))
}

pub(crate) fn validate_unique_existing_pool(
    providers: &[ProviderConfig],
    label: &str,
    pool: &[ActiveProviderModelConfig],
    require_image: bool,
) -> Result<()> {
    let mut seen = HashSet::with_capacity(pool.len());
    for entry in pool {
        if !seen.insert((entry.provider_id.as_str(), entry.model.as_str())) {
            bail!(
                "duplicate {label} model: {} / {}",
                entry.provider_id,
                entry.model
            );
        }
        let valid = if require_image {
            active_model_supports_image(providers, entry)
        } else {
            active_model_exists(providers, entry)
        };
        if !valid {
            let requirement = if require_image {
                "configured image-capable"
            } else {
                "configured"
            };
            bail!(
                "unknown or non-{requirement} {label} model: {} / {}",
                entry.provider_id,
                entry.model
            );
        }
    }
    Ok(())
}

pub(crate) fn is_positive_decimal_id(value: &str) -> bool {
    !value.is_empty()
        && value.bytes().all(|byte| byte.is_ascii_digit())
        && value.parse::<u64>().is_ok_and(|id| id > 0)
}
