use crate::paths::MiyuPaths;
use crate::prompts::DEFAULT_SYSTEM_PROMPT;
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Deserializer, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    pub active_provider: String,
    pub providers: Vec<ProviderConfig>,
    #[serde(default)]
    pub context: ContextConfig,
    #[serde(default)]
    pub tools: ToolsConfig,
    #[serde(default)]
    pub skills: SkillsConfig,
    #[serde(default)]
    pub display: DisplayConfig,
    #[serde(default)]
    pub prompt: PromptConfig,
    #[serde(default)]
    pub plugins: PluginsConfig,
    #[serde(default)]
    pub system_prompt_file: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_prompt: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct DisplayConfig {
    #[serde(default = "default_reasoning_display")]
    pub reasoning: String,
    #[serde(default = "default_tool_call_display")]
    pub tool_calls: String,
}

#[derive(Debug, Clone, Deserialize)]
struct RawDisplayConfig {
    #[serde(default)]
    reasoning: Option<String>,
    #[serde(default)]
    tool_calls: Option<String>,
    #[serde(default)]
    show_reasoning: Option<bool>,
    #[serde(default)]
    reasoning_mode: Option<String>,
    #[serde(default)]
    show_tool_details: Option<bool>,
}

impl<'de> Deserialize<'de> for DisplayConfig {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let raw = RawDisplayConfig::deserialize(deserializer)?;
        let reasoning = raw.reasoning.unwrap_or_else(|| {
            if raw.show_reasoning == Some(false) {
                "hidden".to_string()
            } else {
                raw.reasoning_mode.unwrap_or_else(default_reasoning_display)
            }
        });
        let tool_calls = raw.tool_calls.unwrap_or_else(|| {
            if raw.show_tool_details == Some(true) {
                "full".to_string()
            } else {
                default_tool_call_display()
            }
        });
        Ok(Self {
            reasoning,
            tool_calls,
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderConfig {
    pub id: String,
    pub display_name: String,
    pub base_url: String,
    #[serde(default)]
    pub api_key: Option<String>,
    #[serde(default)]
    pub models: Vec<String>,
    #[serde(default)]
    pub model_context_chars: HashMap<String, usize>,
    pub default_model: String,
    #[serde(default = "default_timeout")]
    pub timeout_seconds: u64,
    #[serde(default = "default_temperature")]
    pub temperature: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PromptConfig {
    #[serde(default = "default_prompts_dir")]
    pub prompts_dir: String,
    #[serde(default = "default_identities_dir")]
    pub identities_dir: String,
    #[serde(default = "default_user_identity_file")]
    pub user_identity_file: String,
    #[serde(default)]
    pub active_persona: String,
    #[serde(default)]
    pub active_identity: String,
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextConfig {
    #[serde(default = "default_context_chars")]
    pub default_max_chars: usize,
    #[serde(default = "default_trim_at_ratio")]
    pub trim_at_ratio: f32,
    #[serde(default = "default_trim_batch_ratio")]
    pub trim_batch_ratio: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolsConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub max_rounds: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillsConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub allow_command_execution: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginsConfig {
    #[serde(default)]
    pub weather: PluginEnabledConfig,
    #[serde(default)]
    pub web: WebPluginConfig,
    #[serde(default)]
    pub web_images: PluginEnabledConfig,
    #[serde(default)]
    pub deep_research: DeepResearchPluginConfig,
    #[serde(default)]
    pub vision: VisionPluginConfig,
    #[serde(default)]
    pub exchange_rate: ExchangeRatePluginConfig,
    #[serde(default)]
    pub xuanxue: PluginEnabledConfig,
    #[serde(default)]
    pub image_generation: ImageGenerationPluginConfig,
    #[serde(default)]
    pub print_image: PrintImagePluginConfig,
    #[serde(default)]
    pub knowledge_base: KnowledgeBasePluginConfig,
    #[serde(default)]
    pub archlinux: PluginEnabledConfig,
    #[serde(default)]
    pub man: PluginEnabledConfig,
    #[serde(default)]
    pub moegirl: PluginEnabledConfig,
    #[serde(default)]
    pub hash_codec: PluginEnabledConfig,
    #[serde(default)]
    pub calculator: CalculatorPluginConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginEnabledConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WebPluginConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub tavily_api_keys: Vec<String>,
    #[serde(default)]
    pub firecrawl_api_keys: Vec<String>,
    #[serde(default)]
    pub anysearch_api_keys: Vec<String>,
    #[serde(default)]
    pub searxng_base_url: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeepResearchPluginConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_deep_research_dir")]
    pub output_dir: String,
    #[serde(default = "default_deep_research_depth")]
    pub thinking_depth: String,
    #[serde(default = "default_deep_research_max_review_revisions")]
    pub max_review_revisions: usize,
    #[serde(default = "default_deep_research_max_tool_steps")]
    pub max_tool_steps_per_round: usize,
    #[serde(default)]
    pub max_final_answer_chars: usize,
    #[serde(default = "default_deep_research_tool_timeout")]
    pub tool_call_timeout_seconds: u64,
    #[serde(default = "default_true")]
    pub show_progress: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VisionPluginConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_true")]
    pub prefer_current_multimodal_model: bool,
    #[serde(default)]
    pub vision_provider_id: String,
    #[serde(default)]
    pub vision_model: String,
    #[serde(default = "default_true")]
    pub preview_with_chafa: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExchangeRatePluginConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub api_key: String,
    #[serde(default = "default_true")]
    pub free_fallback_enabled: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImageGenerationPluginConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_image_generation_provider_type")]
    pub provider_type: String,
    #[serde(default = "default_openai_images_base_url")]
    pub base_url: String,
    #[serde(default)]
    pub api_keys: Vec<String>,
    #[serde(default = "default_image_generation_model")]
    pub model: String,
    #[serde(default = "default_image_generation_aspect_ratio")]
    pub default_aspect_ratio: String,
    #[serde(default = "default_image_generation_resolution")]
    pub default_resolution: String,
    #[serde(default = "default_image_generation_output_dir")]
    pub output_dir: String,
    #[serde(default)]
    pub auto_print: bool,
    #[serde(default = "default_image_generation_timeout")]
    pub timeout_seconds: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PrintImagePluginConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_print_image_width_percent")]
    pub width_percent: u8,
    #[serde(default = "default_print_image_height_percent")]
    pub height_percent: u8,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KnowledgeBasePluginConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub data_dir: String,
    #[serde(default = "default_kb_max_search_results")]
    pub max_search_results: usize,
    #[serde(default = "default_kb_snippet_context_chars")]
    pub snippet_context_chars: usize,
    #[serde(default = "default_kb_proximity_window_chars")]
    pub proximity_window_chars: usize,
    #[serde(default = "default_kb_max_read_lines")]
    pub max_read_lines: usize,
    #[serde(default = "default_kb_max_file_size_kb")]
    pub max_file_size_kb: usize,
    #[serde(default = "default_kb_allowed_extensions")]
    pub allowed_extensions: String,
    #[serde(default = "default_kb_allowed_filenames")]
    pub allowed_filenames: String,
    #[serde(default = "default_true")]
    pub upload_tool_enabled: bool,
    #[serde(default = "default_true")]
    pub embedding_enabled: bool,
    #[serde(default)]
    pub embedding_provider_id: String,
    #[serde(default)]
    pub embedding_model: String,
    #[serde(default = "default_kb_semantic_chunk_chars")]
    pub semantic_chunk_chars: usize,
    #[serde(default = "default_kb_semantic_chunk_overlap")]
    pub semantic_chunk_overlap: usize,
    #[serde(default = "default_kb_semantic_top_k")]
    pub semantic_top_k: usize,
    #[serde(default = "default_kb_semantic_min_score")]
    pub semantic_min_score: f32,
    #[serde(default = "default_kb_keyword_strong_score_threshold")]
    pub keyword_strong_score_threshold: f32,
    #[serde(default = "default_kb_embedding_timeout_seconds")]
    pub embedding_timeout_seconds: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CalculatorPluginConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_calculator_backend")]
    pub backend: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SecretsConfig {
    #[serde(default)]
    pub api_keys: HashMap<String, String>,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            active_provider: "opencodezen".to_string(),
            providers: vec![ProviderConfig::default_opencodezen()],
            context: ContextConfig::default(),
            tools: ToolsConfig::default(),
            skills: SkillsConfig::default(),
            display: DisplayConfig::default(),
            prompt: PromptConfig::default(),
            plugins: PluginsConfig::default(),
            system_prompt_file: Some("system-prompt.md".to_string()),
            system_prompt: None,
        }
    }
}

impl Default for PromptConfig {
    fn default() -> Self {
        Self {
            prompts_dir: default_prompts_dir(),
            identities_dir: default_identities_dir(),
            user_identity_file: default_user_identity_file(),
            active_persona: String::new(),
            active_identity: String::new(),
        }
    }
}

impl Default for DisplayConfig {
    fn default() -> Self {
        Self {
            reasoning: default_reasoning_display(),
            tool_calls: default_tool_call_display(),
        }
    }
}

impl Default for PluginsConfig {
    fn default() -> Self {
        Self {
            weather: PluginEnabledConfig::default(),
            web: WebPluginConfig::default(),
            web_images: PluginEnabledConfig::disabled(),
            deep_research: DeepResearchPluginConfig::default(),
            vision: VisionPluginConfig::default(),
            exchange_rate: ExchangeRatePluginConfig::default(),
            xuanxue: PluginEnabledConfig::default(),
            image_generation: ImageGenerationPluginConfig::default(),
            print_image: PrintImagePluginConfig::default(),
            knowledge_base: KnowledgeBasePluginConfig::default(),
            archlinux: PluginEnabledConfig::default(),
            man: PluginEnabledConfig::default(),
            moegirl: PluginEnabledConfig::default(),
            hash_codec: PluginEnabledConfig::default(),
            calculator: CalculatorPluginConfig::default(),
        }
    }
}

impl Default for PluginEnabledConfig {
    fn default() -> Self {
        Self {
            enabled: default_true(),
        }
    }
}

impl PluginEnabledConfig {
    fn disabled() -> Self {
        Self { enabled: false }
    }
}

impl Default for WebPluginConfig {
    fn default() -> Self {
        Self {
            enabled: default_true(),
            tavily_api_keys: Vec::new(),
            firecrawl_api_keys: Vec::new(),
            anysearch_api_keys: Vec::new(),
            searxng_base_url: String::new(),
        }
    }
}

impl Default for DeepResearchPluginConfig {
    fn default() -> Self {
        Self {
            enabled: default_true(),
            output_dir: default_deep_research_dir(),
            thinking_depth: default_deep_research_depth(),
            max_review_revisions: default_deep_research_max_review_revisions(),
            max_tool_steps_per_round: default_deep_research_max_tool_steps(),
            max_final_answer_chars: 0,
            tool_call_timeout_seconds: default_deep_research_tool_timeout(),
            show_progress: default_true(),
        }
    }
}

impl Default for VisionPluginConfig {
    fn default() -> Self {
        Self {
            enabled: default_true(),
            prefer_current_multimodal_model: default_true(),
            vision_provider_id: String::new(),
            vision_model: String::new(),
            preview_with_chafa: default_true(),
        }
    }
}

impl Default for ExchangeRatePluginConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            api_key: String::new(),
            free_fallback_enabled: default_true(),
        }
    }
}

impl Default for ImageGenerationPluginConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            provider_type: default_image_generation_provider_type(),
            base_url: default_openai_images_base_url(),
            api_keys: Vec::new(),
            model: default_image_generation_model(),
            default_aspect_ratio: default_image_generation_aspect_ratio(),
            default_resolution: default_image_generation_resolution(),
            output_dir: default_image_generation_output_dir(),
            auto_print: default_true(),
            timeout_seconds: default_image_generation_timeout(),
        }
    }
}

impl Default for PrintImagePluginConfig {
    fn default() -> Self {
        Self {
            enabled: default_true(),
            width_percent: default_print_image_width_percent(),
            height_percent: default_print_image_height_percent(),
        }
    }
}

impl Default for KnowledgeBasePluginConfig {
    fn default() -> Self {
        Self {
            enabled: default_true(),
            data_dir: String::new(),
            max_search_results: default_kb_max_search_results(),
            snippet_context_chars: default_kb_snippet_context_chars(),
            proximity_window_chars: default_kb_proximity_window_chars(),
            max_read_lines: default_kb_max_read_lines(),
            max_file_size_kb: default_kb_max_file_size_kb(),
            allowed_extensions: default_kb_allowed_extensions(),
            allowed_filenames: default_kb_allowed_filenames(),
            upload_tool_enabled: default_true(),
            embedding_enabled: default_true(),
            embedding_provider_id: String::new(),
            embedding_model: String::new(),
            semantic_chunk_chars: default_kb_semantic_chunk_chars(),
            semantic_chunk_overlap: default_kb_semantic_chunk_overlap(),
            semantic_top_k: default_kb_semantic_top_k(),
            semantic_min_score: default_kb_semantic_min_score(),
            keyword_strong_score_threshold: default_kb_keyword_strong_score_threshold(),
            embedding_timeout_seconds: default_kb_embedding_timeout_seconds(),
        }
    }
}

impl Default for CalculatorPluginConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            backend: default_calculator_backend(),
        }
    }
}

impl Default for ToolsConfig {
    fn default() -> Self {
        Self {
            enabled: default_true(),
            max_rounds: 0,
        }
    }
}

impl Default for SkillsConfig {
    fn default() -> Self {
        Self {
            enabled: default_true(),
            allow_command_execution: false,
        }
    }
}

impl Default for ContextConfig {
    fn default() -> Self {
        Self {
            default_max_chars: default_context_chars(),
            trim_at_ratio: default_trim_at_ratio(),
            trim_batch_ratio: default_trim_batch_ratio(),
        }
    }
}

impl ProviderConfig {
    pub fn default_opencodezen() -> Self {
        Self {
            id: "opencodezen".to_string(),
            display_name: "opencodezen".to_string(),
            base_url: "https://opencode.ai/zen/v1".to_string(),
            api_key: Some(
                "sk-Ol1VX7IiknlhWk5kMfOK6ltkB34qR7e3RkirMd5xhAxdGmuhsGvSMzCbLNLftDIM".to_string(),
            ),
            models: vec!["big-pickle".to_string()],
            model_context_chars: HashMap::new(),
            default_model: "big-pickle".to_string(),
            timeout_seconds: default_timeout(),
            temperature: default_temperature(),
        }
    }

    pub fn default_openai() -> Self {
        Self {
            id: "openai".to_string(),
            display_name: "OpenAI-compatible".to_string(),
            base_url: "https://api.openai.com/v1".to_string(),
            api_key: Some("$env:OPENAI_API_KEY".to_string()),
            models: vec!["gpt-4o-mini".to_string()],
            model_context_chars: HashMap::new(),
            default_model: "gpt-4o-mini".to_string(),
            timeout_seconds: default_timeout(),
            temperature: default_temperature(),
        }
    }

    pub fn resolved_api_key(&self, paths: &MiyuPaths) -> Result<String> {
        if let Some(api_key) = self.api_key.as_deref() {
            if let Some(env_name) = api_key.strip_prefix("$env:") {
                return std::env::var(env_name)
                    .with_context(|| format!("environment variable {env_name} is not set"));
            }
            if !api_key.is_empty() {
                return Ok(api_key.to_string());
            }
        }

        let secrets = SecretsConfig::load(paths)?;
        secrets
            .api_keys
            .get(&self.id)
            .cloned()
            .with_context(|| format!("missing API key for provider {}", self.id))
    }
}

impl AppConfig {
    pub fn load(paths: &MiyuPaths) -> Result<Self> {
        let raw = std::fs::read_to_string(&paths.config_file)
            .with_context(|| format!("failed to read {}", paths.config_file.display()))?;
        let stripped = json_comments::StripComments::new(raw.as_bytes());
        let config: Self = serde_json::from_reader(stripped)
            .with_context(|| format!("invalid JSONC in {}", paths.config_file.display()))?;
        config.validate()?;
        Ok(config)
    }

    pub fn load_or_default(paths: &MiyuPaths) -> Result<Self> {
        if paths.config_file.exists() {
            Self::load(paths)
        } else {
            Ok(Self::default())
        }
    }

    pub fn init_files(paths: &MiyuPaths) -> Result<()> {
        paths.create_dirs()?;
        if !paths.config_file.exists() {
            Self::default().save(paths)?;
        }
        std::fs::create_dir_all(Self::default().prompts_dir_path(paths))?;
        std::fs::create_dir_all(Self::default().identities_dir_path(paths))?;
        let prompt_file = Self::default_system_prompt_file(paths);
        if !prompt_file.exists() {
            std::fs::write(prompt_file, "")?;
        }
        let user_identity_file = Self::default().user_identity_path(paths);
        if !user_identity_file.exists() {
            std::fs::write(user_identity_file, "")?;
        }
        if !paths.secrets_file.exists() {
            let raw = "{\n  // Optional provider API keys. Prefer $env:... in config.jsonc.\n  \"api_keys\": {}\n}\n";
            std::fs::write(&paths.secrets_file, raw)?;
            set_private_permissions(&paths.secrets_file)?;
        }
        Ok(())
    }

    pub fn save(&self, paths: &MiyuPaths) -> Result<()> {
        paths.create_dirs()?;
        let mut config = self.clone();
        if let Some(prompt) = config.system_prompt.take() {
            let prompt_file = config.system_prompt_path(paths);
            if let Some(parent) = prompt_file.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let prompt = prompt.trim_end();
            let content = if prompt.is_empty() {
                String::new()
            } else {
                format!("{prompt}\n")
            };
            std::fs::write(prompt_file, content)?;
        }
        if config
            .system_prompt_file
            .as_deref()
            .map(str::trim)
            .unwrap_or_default()
            .is_empty()
        {
            config.system_prompt_file = Some("system-prompt.md".to_string());
        }
        let raw = serde_json::to_string_pretty(&config)?;
        std::fs::write(&paths.config_file, format!("{raw}\n"))?;
        Ok(())
    }

    pub fn validate(&self) -> Result<()> {
        if self.active_provider.trim().is_empty() {
            bail!("active_provider cannot be empty");
        }
        if self.providers.is_empty() {
            bail!("at least one provider is required");
        }
        for provider in &self.providers {
            if provider.id.trim().is_empty() {
                bail!("provider id cannot be empty");
            }
            if provider.base_url.trim().is_empty() {
                bail!("provider {} base_url cannot be empty", provider.id);
            }
            if provider.default_model.trim().is_empty() {
                bail!("provider {} default_model cannot be empty", provider.id);
            }
        }
        if self.context.default_max_chars == 0 {
            bail!("context.default_max_chars must be greater than 0");
        }
        if !(0.1..=1.0).contains(&self.context.trim_at_ratio) {
            bail!("context.trim_at_ratio must be between 0.1 and 1.0");
        }
        if !(0.01..=0.9).contains(&self.context.trim_batch_ratio) {
            bail!("context.trim_batch_ratio must be between 0.01 and 0.9");
        }
        if self.plugins.print_image.width_percent == 0
            || self.plugins.print_image.width_percent > 100
        {
            bail!("plugins.print_image.width_percent must be between 1 and 100");
        }
        if self.plugins.print_image.height_percent == 0
            || self.plugins.print_image.height_percent > 100
        {
            bail!("plugins.print_image.height_percent must be between 1 and 100");
        }
        match self.plugins.deep_research.thinking_depth.as_str() {
            "minimal" | "low" | "medium" | "high" | "xhigh" => {}
            value => bail!("plugins.deep_research.thinking_depth is invalid: {value}"),
        }
        match self.plugins.image_generation.provider_type.as_str() {
            "openai" | "rightcode" => {}
            value => bail!("plugins.image_generation.provider_type is invalid: {value}"),
        }
        match self.plugins.image_generation.default_aspect_ratio.as_str() {
            "自动" | "1:1" | "2:3" | "3:2" | "3:4" | "4:3" | "4:5" | "5:4" | "9:16" | "16:9"
            | "21:9" => {}
            value => bail!("plugins.image_generation.default_aspect_ratio is invalid: {value}"),
        }
        match self.plugins.image_generation.default_resolution.as_str() {
            "1K" | "2K" | "4K" => {}
            value => bail!("plugins.image_generation.default_resolution is invalid: {value}"),
        }
        if self.plugins.image_generation.timeout_seconds == 0 {
            bail!("plugins.image_generation.timeout_seconds must be greater than 0");
        }
        if self.plugins.knowledge_base.max_search_results == 0 {
            bail!("plugins.knowledge_base.max_search_results must be greater than 0");
        }
        if self.plugins.knowledge_base.max_read_lines == 0 {
            bail!("plugins.knowledge_base.max_read_lines must be greater than 0");
        }
        if self.plugins.knowledge_base.max_file_size_kb == 0 {
            bail!("plugins.knowledge_base.max_file_size_kb must be greater than 0");
        }
        if self.plugins.knowledge_base.semantic_chunk_chars < 128 {
            bail!("plugins.knowledge_base.semantic_chunk_chars must be at least 128");
        }
        if self.plugins.knowledge_base.semantic_chunk_overlap
            >= self.plugins.knowledge_base.semantic_chunk_chars
        {
            bail!("plugins.knowledge_base.semantic_chunk_overlap must be smaller than semantic_chunk_chars");
        }
        if self.plugins.knowledge_base.semantic_top_k == 0 {
            bail!("plugins.knowledge_base.semantic_top_k must be greater than 0");
        }
        if self.plugins.knowledge_base.embedding_timeout_seconds == 0 {
            bail!("plugins.knowledge_base.embedding_timeout_seconds must be greater than 0");
        }
        self.provider(None)?;
        Ok(())
    }

    pub fn provider(&self, id: Option<&str>) -> Result<&ProviderConfig> {
        let target = id.unwrap_or(&self.active_provider);
        self.providers
            .iter()
            .find(|provider| provider.id == target)
            .with_context(|| format!("provider not found: {target}"))
    }

    pub fn provider_model_choices(&self) -> Vec<ProviderModelChoice> {
        self.providers
            .iter()
            .flat_map(|provider| {
                let models = if provider.models.is_empty() {
                    vec![provider.default_model.clone()]
                } else {
                    provider.models.clone()
                };
                models
                    .into_iter()
                    .filter(|model| !model.trim().is_empty())
                    .map(|model| ProviderModelChoice {
                        provider_id: provider.id.clone(),
                        provider_name: provider.display_name.clone(),
                        model,
                    })
                    .collect::<Vec<_>>()
            })
            .collect()
    }

    pub fn set_active_provider_model(&mut self, provider_id: &str, model: &str) -> Result<()> {
        let provider = self
            .providers
            .iter_mut()
            .find(|provider| provider.id == provider_id)
            .with_context(|| format!("provider not found: {provider_id}"))?;
        if model.trim().is_empty() {
            bail!("model cannot be empty");
        }
        self.active_provider = provider.id.clone();
        provider.default_model = model.to_string();
        if !provider.models.iter().any(|item| item == model) {
            provider.models.push(model.to_string());
        }
        Ok(())
    }

    pub fn active_context_chars(&self) -> Result<usize> {
        let provider = self.provider(None)?;
        Ok(provider
            .model_context_chars
            .get(&provider.default_model)
            .copied()
            .unwrap_or(self.context.default_max_chars))
    }

    pub fn system_prompt(&self, paths: &MiyuPaths) -> Result<String> {
        let mut prompt = self.base_system_prompt(paths)?;
        let user_identity = self.user_identity_prompt(paths)?;
        if !user_identity.trim().is_empty() {
            prompt.push_str("\n\n<user-identity>\n");
            prompt.push_str(user_identity.trim());
            prompt.push_str("\n</user-identity>");
        }
        Ok(prompt)
    }

    pub fn base_system_prompt(&self, paths: &MiyuPaths) -> Result<String> {
        let persona = self.active_persona_prompt(paths)?;
        if persona.trim().is_empty() {
            Ok(DEFAULT_SYSTEM_PROMPT.to_string())
        } else {
            Ok(persona)
        }
    }

    pub fn custom_system_prompt(&self, paths: &MiyuPaths) -> Result<String> {
        if let Some(prompt) = self
            .system_prompt
            .as_deref()
            .filter(|prompt| !prompt.trim().is_empty())
        {
            return Ok(prompt.to_string());
        }
        let prompt_file = self.system_prompt_path(paths);
        if prompt_file.exists() {
            return Ok(std::fs::read_to_string(prompt_file)?);
        }
        Ok(String::new())
    }

    pub fn prompts_dir_path(&self, paths: &MiyuPaths) -> PathBuf {
        config_relative_path(paths, &self.prompt.prompts_dir)
    }

    pub fn user_identity_path(&self, paths: &MiyuPaths) -> PathBuf {
        config_relative_path(paths, &self.prompt.user_identity_file)
    }

    pub fn identities_dir_path(&self, paths: &MiyuPaths) -> PathBuf {
        config_relative_path(paths, &self.prompt.identities_dir)
    }

    pub fn persona_path(&self, paths: &MiyuPaths, name: &str) -> PathBuf {
        self.prompts_dir_path(paths).join(name)
    }

    pub fn identity_path(&self, paths: &MiyuPaths, name: &str) -> PathBuf {
        self.identities_dir_path(paths).join(name)
    }

    pub fn active_persona_prompt(&self, paths: &MiyuPaths) -> Result<String> {
        if !self.prompt.active_persona.trim().is_empty() {
            let path = self.persona_path(paths, self.prompt.active_persona.trim());
            if path.exists() {
                return std::fs::read_to_string(&path)
                    .with_context(|| format!("failed to read {}", path.display()));
            }
        }
        if let Some(prompt) = self
            .system_prompt
            .as_deref()
            .filter(|prompt| !prompt.trim().is_empty())
        {
            return Ok(prompt.to_string());
        }
        let legacy = self.custom_system_prompt(paths)?;
        if legacy.trim().is_empty() {
            Ok(String::new())
        } else {
            Ok(legacy)
        }
    }

    pub fn user_identity_prompt(&self, paths: &MiyuPaths) -> Result<String> {
        if !self.prompt.active_identity.trim().is_empty() {
            let path = self.identity_path(paths, self.prompt.active_identity.trim());
            if path.exists() {
                return std::fs::read_to_string(&path)
                    .with_context(|| format!("failed to read {}", path.display()));
            }
        }
        let path = self.user_identity_path(paths);
        if path.exists() {
            return std::fs::read_to_string(&path)
                .with_context(|| format!("failed to read {}", path.display()));
        }
        Ok(String::new())
    }

    pub fn system_prompt_path(&self, paths: &MiyuPaths) -> PathBuf {
        let value = self
            .system_prompt_file
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .unwrap_or("system-prompt.md");
        let path = PathBuf::from(value);
        if path.is_absolute() {
            path
        } else {
            paths.config_dir.join(path)
        }
    }

    pub fn default_system_prompt_file(paths: &MiyuPaths) -> PathBuf {
        paths.config_dir.join("system-prompt.md")
    }

    pub fn upsert_provider(&mut self, provider: ProviderConfig) {
        self.active_provider = provider.id.clone();
        match self
            .providers
            .iter()
            .position(|item| item.id == provider.id)
        {
            Some(index) => self.providers[index] = provider,
            None => self.providers.push(provider),
        }
    }
}

impl SecretsConfig {
    pub fn load(paths: &MiyuPaths) -> Result<Self> {
        if !paths.secrets_file.exists() {
            return Ok(Self::default());
        }
        let raw = std::fs::read_to_string(&paths.secrets_file)?;
        let stripped = json_comments::StripComments::new(raw.as_bytes());
        Ok(serde_json::from_reader(stripped)?)
    }
}

fn default_timeout() -> u64 {
    60
}

fn default_prompts_dir() -> String {
    "prompts".to_string()
}

fn default_identities_dir() -> String {
    "identities".to_string()
}

fn default_user_identity_file() -> String {
    "user-identity.md".to_string()
}

fn config_relative_path(paths: &MiyuPaths, value: &str) -> PathBuf {
    let path = PathBuf::from(value.trim());
    if path.is_absolute() {
        path
    } else {
        paths.config_dir.join(path)
    }
}

fn default_temperature() -> f32 {
    0.7
}

fn default_true() -> bool {
    true
}

fn default_reasoning_display() -> String {
    "summary".to_string()
}

fn default_tool_call_display() -> String {
    "summary".to_string()
}

fn default_print_image_width_percent() -> u8 {
    60
}

fn default_print_image_height_percent() -> u8 {
    50
}

fn default_deep_research_dir() -> String {
    if let Some(dirs) = directories::UserDirs::new() {
        if let Some(documents) = dirs.document_dir() {
            return documents.join("Miyu/deep-thinking").display().to_string();
        }
    }
    "~/Documents/Miyu/deep-thinking".to_string()
}

fn default_deep_research_depth() -> String {
    "high".to_string()
}

fn default_deep_research_max_review_revisions() -> usize {
    0
}

fn default_deep_research_max_tool_steps() -> usize {
    0
}

fn default_deep_research_tool_timeout() -> u64 {
    90
}

fn default_image_generation_provider_type() -> String {
    "openai".to_string()
}

fn default_openai_images_base_url() -> String {
    "https://api.openai.com".to_string()
}

fn default_image_generation_model() -> String {
    "gpt-image-1".to_string()
}

fn default_image_generation_aspect_ratio() -> String {
    "自动".to_string()
}

fn default_image_generation_resolution() -> String {
    "1K".to_string()
}

fn default_image_generation_output_dir() -> String {
    if let Some(dirs) = directories::UserDirs::new() {
        if let Some(pictures) = dirs.picture_dir() {
            return pictures.join("Miyu/generated-images").display().to_string();
        }
    }
    "~/Pictures/Miyu/generated-images".to_string()
}

fn default_image_generation_timeout() -> u64 {
    180
}

fn default_kb_max_search_results() -> usize {
    5
}

fn default_kb_snippet_context_chars() -> usize {
    240
}

fn default_kb_proximity_window_chars() -> usize {
    512
}

fn default_kb_max_read_lines() -> usize {
    200
}

fn default_kb_max_file_size_kb() -> usize {
    1024
}

fn default_kb_allowed_extensions() -> String {
    ".txt,.md,.json,.jsonc,.json5,.yaml,.yml,.csv,.log,.py,.js,.ts,.jsx,.tsx,.mjs,.cjs,.html,.css,.scss,.sass,.less,.cfg,.ini,.conf,.toml,.kdl,.desktop,.service,.timer,.socket,.target,.mount,.rules,.network,.netdev,.properties,.hjson,.ron,.rst,.xml,.sh,.bash,.zsh,.fish,.nu,.ps1,.lua,.nix,.rasi,.yuck,.sql,.rs,.go,.c,.h,.cpp,.hpp,.java,.kt,.php,.rb,.pl,.org,.adoc,.tex".to_string()
}

fn default_kb_allowed_filenames() -> String {
    ".env,.env.local,.env.example,.env.sample,.envrc,.editorconfig,.gitignore,.gitattributes,.npmrc,.vimrc,.bashrc,.zshrc,.profile,.xinitrc,.xresources,config,dockerfile,containerfile,makefile,justfile,procfile,pkgbuild".to_string()
}

fn default_kb_semantic_chunk_chars() -> usize {
    512
}

fn default_kb_semantic_chunk_overlap() -> usize {
    80
}

fn default_kb_semantic_top_k() -> usize {
    5
}

fn default_kb_semantic_min_score() -> f32 {
    0.25
}

fn default_kb_keyword_strong_score_threshold() -> f32 {
    180.0
}

fn default_kb_embedding_timeout_seconds() -> u64 {
    60
}

fn default_calculator_backend() -> String {
    "rust-simple".to_string()
}

fn default_context_chars() -> usize {
    120_000
}

fn default_trim_at_ratio() -> f32 {
    0.9
}

fn default_trim_batch_ratio() -> f32 {
    0.15
}

#[cfg(unix)]
fn set_private_permissions(path: &std::path::Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut permissions = std::fs::metadata(path)?.permissions();
    permissions.set_mode(0o600);
    std::fs::set_permissions(path, permissions)?;
    Ok(())
}

#[cfg(not(unix))]
fn set_private_permissions(_path: &std::path::Path) -> Result<()> {
    Ok(())
}
