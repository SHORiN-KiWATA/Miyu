//! 供应商、模型与它们的能力标签。
//!
//! `ProviderModelChoice` 是「哪个供应商的哪个模型」的唯一表示，界面上到处在用
//! 它做下拉选项。`resolve_provider_model_argument` 把命令行传进来的字符串还原
//! 成它，容忍几种写法但拒绝歧义。
//!
//! 能力（视觉、嵌入、思考）是**每个模型**的属性而不是供应商的：同一个供应商下
//! 既有能看图的也有不能的，池里随机选一个就会随机失败。

use crate::config::*;

/// Tiered model pools. Four capability tiers, each a load-balanced pool the
/// same way the global text pool is. Consumers: the `task` tool (the main
/// model picks a tier per task), the auxiliary roles under `roles`, and any
/// platform slot that references a tier by name (see `ModelPoolRef`).
///
/// An unconfigured tier falls back to the global text pool — never to a
/// neighbouring tier (user decision 2026-09-05).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ModelTiersConfig {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub lite: Vec<ActiveProviderModelConfig>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub cheap: Vec<ActiveProviderModelConfig>,
    /// Old name `balanced` stays readable; saving writes `standard`.
    #[serde(default, alias = "balanced", skip_serializing_if = "Vec::is_empty")]
    pub standard: Vec<ActiveProviderModelConfig>,
    /// Old name `strong` stays readable; saving writes `flagship`.
    #[serde(default, alias = "strong", skip_serializing_if = "Vec::is_empty")]
    pub flagship: Vec<ActiveProviderModelConfig>,
    /// Auxiliary request roles (see [`AuxRole`]) → tier name or `"global"`.
    /// A missing key means the role's built-in default
    /// ([`AuxRole::default_tier`]), so roles take effect the moment the
    /// matching tier is configured; `"global"` pins a role to the global
    /// text pool explicitly.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub roles: BTreeMap<String, String>,
}

impl ModelTiersConfig {
    pub fn is_empty(&self) -> bool {
        ModelTier::ALL
            .iter()
            .all(|tier| self.pool(*tier).is_empty())
            && self.roles.is_empty()
    }

    pub fn pool(&self, tier: ModelTier) -> &Vec<ActiveProviderModelConfig> {
        match tier {
            ModelTier::Lite => &self.lite,
            ModelTier::Cheap => &self.cheap,
            ModelTier::Standard => &self.standard,
            ModelTier::Flagship => &self.flagship,
        }
    }

    pub fn pool_mut(&mut self, tier: ModelTier) -> &mut Vec<ActiveProviderModelConfig> {
        match tier {
            ModelTier::Lite => &mut self.lite,
            ModelTier::Cheap => &mut self.cheap,
            ModelTier::Standard => &mut self.standard,
            ModelTier::Flagship => &mut self.flagship,
        }
    }

    /// The tier an auxiliary role routes through; `None` means the global
    /// text pool. Absent key → the role's default; `"global"` → `None`.
    /// Unknown values also resolve to `None` here — `validate_roles` rejects
    /// them at load time so a typo never silently downgrades.
    pub fn role_tier(&self, role: AuxRole) -> Option<ModelTier> {
        match self.roles.get(role.key()) {
            None => Some(role.default_tier()),
            Some(value) => ModelTier::from_str(value),
        }
    }

    /// Whether the role carries an explicit value (as opposed to its default).
    pub fn role_is_explicit(&self, role: AuxRole) -> bool {
        self.roles.contains_key(role.key())
    }

    /// Set a role to a tier (`Some`) or the global pool (`None`).
    pub fn set_role(&mut self, role: AuxRole, tier: Option<ModelTier>) {
        let value = tier.map_or(GLOBAL_POOL_LABEL, |tier| tier.label());
        self.roles.insert(role.key().to_string(), value.to_string());
    }

    /// Drop the explicit value so the role returns to its default.
    pub fn reset_role(&mut self, role: AuxRole) {
        self.roles.remove(role.key());
    }

    /// Rejects unknown role keys and unknown tier names. The error names the
    /// accepted values so a config typo is fixable without reading source.
    pub(crate) fn validate_roles(&self) -> Result<()> {
        for (role, tier) in &self.roles {
            if AuxRole::from_key(role).is_none() {
                bail!(
                    "model_tiers.roles: unknown role '{role}'; accepted roles: {}",
                    AuxRole::ALL
                        .iter()
                        .map(|role| role.key())
                        .collect::<Vec<_>>()
                        .join(", ")
                );
            }
            if ModelTier::from_str(tier).is_none() && tier.trim() != GLOBAL_POOL_LABEL {
                bail!(
                    "model_tiers.roles.{role}: unknown tier '{tier}'; accepted: lite, cheap, standard, flagship, global"
                );
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelTier {
    Lite,
    Cheap,
    Standard,
    Flagship,
}

impl ModelTier {
    pub const ALL: [Self; 4] = [Self::Lite, Self::Cheap, Self::Standard, Self::Flagship];

    /// Accepts the current names plus the pre-09-05 `balanced` / `strong`.
    pub fn from_str(value: &str) -> Option<Self> {
        match value.trim() {
            "lite" => Some(Self::Lite),
            "cheap" => Some(Self::Cheap),
            "standard" | "balanced" => Some(Self::Standard),
            "flagship" | "strong" => Some(Self::Flagship),
            _ => None,
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            Self::Lite => "lite",
            Self::Cheap => "cheap",
            Self::Standard => "standard",
            Self::Flagship => "flagship",
        }
    }
}

/// Explicit "use the global text pool" value for `model_tiers.roles` and for
/// platform pool references.
pub const GLOBAL_POOL_LABEL: &str = "global";
/// "Inherit from the parent slot" value for platform pool references.
pub const INHERIT_POOL_LABEL: &str = "inherit";

/// Auxiliary LLM requests routed through a tier via `model_tiers.roles`.
/// Each is an independent cache/session state (AGENTS 1.7), so moving it off
/// the global pool never touches the main conversation's prefix cache.
/// Compaction is deliberately absent: its fork-style summary reuses the live
/// conversation prefix and must stay on the model that owns that cache.
/// Platform-side requests (QQ judge, affection, group-join approval) are not
/// roles either — they are platform slots resolved through `ModelPoolRef`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuxRole {
    /// WebUI session title refinement from the first user message.
    SessionTitle,
    /// Background diary → long-term memory distillation (memory organizer).
    MemoryOrganizer,
    /// The `deep_research` tool's researcher/reviewer loop.
    DeepResearch,
}

impl AuxRole {
    pub const ALL: [Self; 3] = [
        Self::SessionTitle,
        Self::MemoryOrganizer,
        Self::DeepResearch,
    ];

    /// Config key under `model_tiers.roles`.
    pub fn key(&self) -> &'static str {
        match self {
            Self::SessionTitle => "session_title",
            Self::MemoryOrganizer => "memory_organizer",
            Self::DeepResearch => "deep_research",
        }
    }

    /// Built-in tier when the role has no explicit value. With no tiers
    /// configured every default still resolves to the global pool, so a
    /// fresh install behaves exactly as before.
    pub fn default_tier(&self) -> ModelTier {
        match self {
            Self::SessionTitle | Self::MemoryOrganizer => ModelTier::Lite,
            Self::DeepResearch => ModelTier::Standard,
        }
    }

    pub fn from_key(value: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|role| role.key() == value.trim())
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

/// CLI 中转线的工具作用域(off/dev/normal/all)在本模式下是否放行。
/// 中转层与 agent 侧共用这一份判定,免得两边各写一套 match。
pub fn relay_scope_allows(scope: &str, dev_mode: bool) -> bool {
    match scope.trim().to_ascii_lowercase().as_str() {
        "all" => true,
        "dev" => dev_mode,
        "normal" => !dev_mode,
        _ => false,
    }
}
/// Claude Code 预置模型:CLI 认的别名。
pub const CLAUDE_CODE_PRESET_MODELS: &[&str] = &["fable", "opus", "sonnet", "haiku"];
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
    /// 用户手填的模型名。供应商的 `/models` 目录是动态拉取的,内测模型不在
    /// 里面,手填的名字得有个自己的落脚点:只记在 `models` 里的话,一取消
    /// 激活它就从配置里没了,模型菜单(列表其余部分全来自拉取结果)里也就
    /// 跟着消失。有了这份清单,自定义模型跟拉取来的模型一样能激活能取消,
    /// 删掉要显式删。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub custom_models: Vec<String>,
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
    /// 工具结果(role=tool)能否直接带图片/视频块。留空按协议推断:
    /// openai-chat 端点默认能(智谱 09-03 实测),OpenAI 官方端点与本机 CLI
    /// 中转不能(前者 400,后者只传文本),anthropic 协议的下沉层暂未接图。
    /// 不能的走"工具结果之后再补一条带图的用户消息"。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_result_media: Option<bool>,
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

/// Which backend produces vectors. `Auto` (the default, and what every config
/// written before 2026-09 reads as) keeps a configured remote model and falls
/// back to the bundled local model otherwise, so upgrading never silently
/// swaps a user's remote bge-m3 for the local small model.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum EmbeddingBackend {
    #[default]
    Auto,
    Local,
    Remote,
}

/// Which model turns text into vectors, and the settings that belong to that
/// model rather than to any one feature — a similarity floor means different
/// things on different models. Semantic retrieval is an assist on top of
/// keyword search everywhere it is used: `enabled: false`, a missing runtime
/// or a dead endpoint all degrade to keyword-only, never to an error.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct EmbeddingConfig {
    /// Master switch for the semantic assist across knowledge base, memory
    /// association, memes and evicted-context search.
    pub enabled: bool,
    pub backend: EmbeddingBackend,
    /// Local model id (a directory under the model search chain) or a path to
    /// a model directory. Only used by the local backend.
    pub local_model: String,
    /// Id of an existing provider; the model is named separately, so a provider
    /// serving both chat and embedding models is still configured once.
    /// Only used by the remote backend.
    pub provider_id: String,
    pub model: String,
    pub timeout_seconds: u64,
    /// Cosine similarity below this is not a hit (remote backend; local models
    /// carry their own floor in `manifest.json`).
    pub min_score: f32,
    /// The local inference worker exits after this much idle time so an idle
    /// daemon holds no model in memory.
    pub idle_unload_seconds: u64,
}

impl Default for EmbeddingConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            backend: EmbeddingBackend::Auto,
            local_model: DEFAULT_LOCAL_EMBEDDING_MODEL.to_string(),
            provider_id: String::new(),
            model: String::new(),
            timeout_seconds: 60,
            min_score: 0.35,
            idle_unload_seconds: 600,
        }
    }
}

/// Marks a model as producing vectors rather than chat.
pub const EMBEDDING_MODALITY: &str = "embedding";

/// The model shipped under `assets/models/`.
pub const DEFAULT_LOCAL_EMBEDDING_MODEL: &str = "bge-small-zh-v1.5-int8";

impl EmbeddingConfig {
    pub(crate) fn is_default(&self) -> bool {
        *self == Self::default()
    }

    /// A remote provider/model pair is named. Says nothing about reachability.
    pub fn remote_is_configured(&self) -> bool {
        !self.provider_id.trim().is_empty() && !self.model.trim().is_empty()
    }

    /// `Auto` resolved: remote when a remote model is named, local otherwise.
    pub fn resolved_backend(&self) -> EmbeddingBackend {
        match self.backend {
            EmbeddingBackend::Auto if self.remote_is_configured() => EmbeddingBackend::Remote,
            EmbeddingBackend::Auto => EmbeddingBackend::Local,
            explicit => explicit,
        }
    }

    /// Something is configured for the semantic pass. Whether it actually
    /// works (runtime library present, endpoint reachable) is only known at
    /// call time; `Embedder::from_config` is the runtime-side check.
    pub fn is_configured(&self) -> bool {
        if !self.enabled {
            return false;
        }
        match self.resolved_backend() {
            EmbeddingBackend::Remote => self.remote_is_configured(),
            _ => !self.local_model.trim().is_empty(),
        }
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
            custom_models: Vec::new(),
            model_context_window: HashMap::new(),
            model_temperature: HashMap::new(),
            model_tools_loading_mode: HashMap::new(),
            model_modalities: HashMap::new(),
            tool_result_media: None,
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
            custom_models: Vec::new(),
            model_context_window: HashMap::new(),
            model_temperature: HashMap::new(),
            model_tools_loading_mode: HashMap::new(),
            model_modalities: HashMap::new(),
            tool_result_media: None,
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
            models: CLAUDE_CODE_PRESET_MODELS
                .iter()
                .map(|name| name.to_string())
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
            models: CODEX_PRESET_MODELS
                .iter()
                .map(|model| model.to_string())
                .collect(),
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

    /// 见 `tool_result_media` 字段。
    pub fn tool_result_carries_media(&self) -> bool {
        if let Some(explicit) = self.tool_result_media {
            return explicit;
        }
        if self.is_builtin_cli_provider() || self.protocol.trim() == "anthropic" {
            return false;
        }
        let host = self
            .base_url
            .trim()
            .trim_start_matches("https://")
            .trim_start_matches("http://")
            .split('/')
            .next()
            .unwrap_or("")
            .to_ascii_lowercase();
        host != "api.openai.com"
    }

    /// 内置 CLI 供应商的模型目录(没有 /models 端点,目录就是预置别名表)。
    /// 配置里的 `models` 是"已激活"集合,不是目录——两者混为一谈时,用户
    /// 只激活了一个模型,TUI 就只剩这一个可选(09-03 报"没有模型了")。
    pub fn preset_model_catalog(&self) -> &'static [&'static str] {
        if self.is_claude_code() {
            CLAUDE_CODE_PRESET_MODELS
        } else if self.is_antigravity() {
            ANTIGRAVITY_PRESET_MODELS
        } else if self.is_codex() {
            CODEX_PRESET_MODELS
        } else {
            &[]
        }
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
            custom_models: Vec::new(),
            model_context_window: HashMap::new(),
            model_temperature: HashMap::new(),
            model_tools_loading_mode: HashMap::new(),
            model_modalities: HashMap::new(),
            tool_result_media: None,
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
            custom_models: Vec::new(),
            model_context_window: HashMap::new(),
            model_temperature: HashMap::new(),
            model_tools_loading_mode: HashMap::new(),
            model_modalities: HashMap::new(),
            tool_result_media: None,
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

    /// 模型**具备**的输入能力(配置声明优先,否则查 models.dev 目录)。
    ///
    /// 这是"能不能看"的答案,决定它能不能进多模态池、能不能被标成看图模型。
    /// "能不能把媒体塞进消息"是另一个问题,见 [`Self::message_input_modalities`]:
    /// 09-04 两者曾被合成一个(antigravity 直接硬编码成 text),结果用户在设置页
    /// 给 gemini 勾了视频、多模态池里却永远找不到它。
    pub fn input_modalities(&self, model: &str) -> Option<Vec<String>> {
        if let Some(modalities) = self.model_modalities.get(model) {
            return Some(modalities.clone());
        }
        crate::models_cache::input_modalities(&self.id, model)
    }

    /// 模型能直接吃进**消息**里的输入种类。
    ///
    /// agy 中转线恒为纯文本:它的 stream-json 只收 text 块(09-04 实测,image/
    /// media 块一律 `not supported (only "text")`),模型看媒体只能自己调原生
    /// `view_file`。目录里 Gemini 标着 image 输入,照抄就会让 Miyu 把图内联进
    /// 消息——中转层再降级成占位文本,图没到模型,活体消息与化石还因此字节
    /// 不同,续传链逢图必断(09-04 群 130515298 实证)。内联、视觉旁路选客户端
    /// 都要问这个;池成员资格问 [`Self::input_modalities`]。
    pub fn message_input_modalities(&self, model: &str) -> Option<Vec<String>> {
        if self.views_media_with_native_file_tool() {
            return Some(vec!["text".to_string()]);
        }
        self.input_modalities(model)
    }

    /// 本线上模型看媒体靠自己调原生文件工具(`view_file` 对图片/视频/音频/PDF
    /// 都返回媒体本体,09-04 实测),而不是消息内联或视觉旁路。
    pub fn views_media_with_native_file_tool(&self) -> bool {
        self.is_antigravity()
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
