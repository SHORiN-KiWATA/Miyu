use crate::default_models::{
    OPENCODE_DEFAULT_CHAT_MODEL, OPENCODE_PROVIDER_ID, OPENCODE_ZEN_BASE_URL,
};
use crate::paths::MiyuPaths;
use crate::prompts::default_system_prompt;
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
    #[serde(default, skip_serializing)]
    pub memory: MemoryConfig,
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
    #[serde(default = "default_true")]
    pub readable_tool_names: bool,
    #[serde(default)]
    pub show_token_usage: bool,
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
    #[serde(default)]
    readable_tool_names: Option<bool>,
    #[serde(default)]
    show_token_usage: Option<bool>,
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
            readable_tool_names: raw.readable_tool_names.unwrap_or_else(default_true),
            show_token_usage: raw.show_token_usage.unwrap_or(false),
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderConfig {
    pub id: String,
    pub display_name: String,
    pub base_url: String,
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
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub model_modalities: HashMap<String, Vec<String>>,
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
    #[serde(default = "default_trim_at_ratio")]
    pub trim_at_ratio: f32,
    #[serde(default = "default_trim_batch_ratio")]
    pub trim_batch_ratio: f32,
    #[serde(default = "default_on_overflow")]
    pub on_overflow: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolsConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub max_rounds: usize,
    #[serde(default = "default_tools_loading_mode")]
    pub loading_mode: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillsConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub allow_command_execution: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MemoryConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_true")]
    pub evicted_context_enabled: bool,
    #[serde(default = "default_true")]
    pub association_enabled: bool,
    #[serde(default = "default_true")]
    pub auto_diary_enabled: bool,
    #[serde(default = "default_true")]
    pub auto_fact_enabled: bool,
    #[serde(default = "default_true")]
    pub auto_skill_enabled: bool,
    #[serde(default = "default_memory_association_facts")]
    pub association_facts: usize,
    #[serde(default = "default_memory_association_episodes")]
    pub association_episodes: usize,
    #[serde(default = "default_memory_association_max_chars")]
    pub association_max_chars: usize,
    #[serde(default = "default_memory_snippet_chars")]
    pub snippet_chars: usize,
    #[serde(default = "default_memory_forget_after_days")]
    pub forget_after_days: u64,
    #[serde(default = "default_true")]
    pub forgetting_enabled: bool,
    #[serde(default = "default_memory_half_life_days")]
    pub forgetting_half_life_days: f64,
    #[serde(default = "default_memory_min_strength")]
    pub forgetting_min_strength: f64,
    #[serde(default = "default_memory_review_boost")]
    pub forgetting_review_boost: f64,
    #[serde(default = "default_memory_min_task_chars")]
    pub learning_min_task_chars: usize,
    #[serde(default = "default_memory_min_method_chars")]
    pub learning_min_method_chars: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginsConfig {
    #[serde(default)]
    pub weather: PluginEnabledConfig,
    #[serde(default)]
    pub web: WebPluginConfig,
    #[serde(default)]
    pub web_images: WebImagesPluginConfig,
    #[serde(default)]
    pub deep_research: DeepResearchPluginConfig,
    #[serde(default)]
    pub deep_diagnose: DeepDiagnosePluginConfig,
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
    pub memes: MemesPluginConfig,
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
    #[serde(default)]
    pub package_advisor: PluginEnabledConfig,
    #[serde(default, alias = "linux_game_compatibility")]
    pub deep_research_linux_game_compatibility: LinuxGameCompatibilityConfig,
    #[serde(default)]
    pub diagnostics: DiagnosticsPluginConfig,
    #[serde(default)]
    pub memory: MemoryConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginEnabledConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LinuxGameCompatibilityConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_subagent_max_tool_steps")]
    pub max_tool_steps: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WebPluginConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_web_search_max_results")]
    pub max_results: usize,
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
pub struct WebImagesPluginConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_web_images_max_results")]
    pub max_results: usize,
    #[serde(default = "default_web_images_max_download_mb")]
    pub max_download_mb: f64,
    #[serde(default = "default_true")]
    pub safe_search: bool,
    #[serde(default = "default_true")]
    pub vision_screening_enabled: bool,
    #[serde(default = "default_true")]
    pub auto_preview: bool,
    #[serde(default = "default_web_images_preview_count")]
    pub preview_count: usize,
    #[serde(default = "default_web_images_timeout")]
    pub timeout_seconds: u64,
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
pub struct DeepDiagnosePluginConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
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
    #[serde(default = "default_subagent_max_tool_steps")]
    pub max_tool_steps: usize,
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
pub struct MemesPluginConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub persona_libraries: HashMap<String, String>,
    #[serde(default = "default_memes_width_percent")]
    pub width_percent: u8,
    #[serde(default = "default_memes_height_percent")]
    pub height_percent: u8,
    #[serde(default = "default_memes_max_image_mb")]
    pub max_image_mb: u64,
    #[serde(default)]
    pub allow_gif_animation: bool,
    #[serde(default)]
    pub auto_send_enabled: bool,
    #[serde(default = "default_memes_auto_send_probability")]
    pub auto_send_probability: f32,
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiagnosticsPluginConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_diagnostics_timeout")]
    pub command_timeout_seconds: u64,
    #[serde(default = "default_diagnostics_max_stdout_chars")]
    pub max_stdout_chars: usize,
    #[serde(default = "default_diagnostics_max_stderr_chars")]
    pub max_stderr_chars: usize,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SecretsConfig {
    #[serde(default)]
    pub api_keys: HashMap<String, String>,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            active_provider: OPENCODE_PROVIDER_ID.to_string(),
            providers: ProviderConfig::default_templates(),
            context: ContextConfig::default(),
            tools: ToolsConfig::default(),
            skills: SkillsConfig::default(),
            display: DisplayConfig::default(),
            prompt: PromptConfig::default(),
            plugins: PluginsConfig::default(),
            memory: MemoryConfig::default(),
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
            readable_tool_names: default_true(),
            show_token_usage: false,
        }
    }
}

impl Default for PluginsConfig {
    fn default() -> Self {
        Self {
            weather: PluginEnabledConfig::default(),
            web: WebPluginConfig::default(),
            web_images: WebImagesPluginConfig::default(),
            deep_research: DeepResearchPluginConfig::default(),
            deep_diagnose: DeepDiagnosePluginConfig::default(),
            vision: VisionPluginConfig::default(),
            exchange_rate: ExchangeRatePluginConfig::default(),
            xuanxue: PluginEnabledConfig::default(),
            image_generation: ImageGenerationPluginConfig::default(),
            print_image: PrintImagePluginConfig::default(),
            memes: MemesPluginConfig::default(),
            knowledge_base: KnowledgeBasePluginConfig::default(),
            archlinux: PluginEnabledConfig::default(),
            man: PluginEnabledConfig::default(),
            moegirl: PluginEnabledConfig::default(),
            hash_codec: PluginEnabledConfig::default(),
            calculator: CalculatorPluginConfig::default(),
            package_advisor: PluginEnabledConfig::default(),
            deep_research_linux_game_compatibility: LinuxGameCompatibilityConfig::default(),
            diagnostics: DiagnosticsPluginConfig::default(),
            memory: MemoryConfig::default(),
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

impl Default for LinuxGameCompatibilityConfig {
    fn default() -> Self {
        Self {
            enabled: default_true(),
            max_tool_steps: default_subagent_max_tool_steps(),
        }
    }
}

impl Default for WebPluginConfig {
    fn default() -> Self {
        Self {
            enabled: default_true(),
            max_results: default_web_search_max_results(),
            tavily_api_keys: Vec::new(),
            firecrawl_api_keys: Vec::new(),
            anysearch_api_keys: Vec::new(),
            searxng_base_url: String::new(),
        }
    }
}

impl Default for WebImagesPluginConfig {
    fn default() -> Self {
        Self {
            enabled: default_true(),
            max_results: default_web_images_max_results(),
            max_download_mb: default_web_images_max_download_mb(),
            safe_search: default_true(),
            vision_screening_enabled: default_true(),
            auto_preview: default_true(),
            preview_count: default_web_images_preview_count(),
            timeout_seconds: default_web_images_timeout(),
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

impl Default for DeepDiagnosePluginConfig {
    fn default() -> Self {
        Self {
            enabled: default_true(),
            thinking_depth: default_deep_research_depth(),
            max_review_revisions: default_deep_research_max_review_revisions(),
            max_tool_steps_per_round: default_deep_research_max_tool_steps(),
            max_final_answer_chars: 0,
            tool_call_timeout_seconds: default_deep_research_tool_timeout(),
            max_tool_steps: default_subagent_max_tool_steps(),
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

impl Default for MemesPluginConfig {
    fn default() -> Self {
        Self {
            enabled: default_true(),
            persona_libraries: HashMap::new(),
            width_percent: default_memes_width_percent(),
            height_percent: default_memes_height_percent(),
            max_image_mb: default_memes_max_image_mb(),
            allow_gif_animation: false,
            auto_send_enabled: true,
            auto_send_probability: default_memes_auto_send_probability(),
        }
    }
}

impl MemesPluginConfig {
    pub fn library_for_persona(&self, persona: &str) -> String {
        if persona.trim().is_empty() {
            return self
                .persona_libraries
                .get("default")
                .cloned()
                .unwrap_or_else(|| "miyu".to_string());
        }
        let persona = persona_scope_name(persona);
        self.persona_libraries
            .get(&persona)
            .cloned()
            .unwrap_or(persona)
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
            embedding_enabled: false,
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

impl Default for DiagnosticsPluginConfig {
    fn default() -> Self {
        Self {
            enabled: default_true(),
            command_timeout_seconds: default_diagnostics_timeout(),
            max_stdout_chars: default_diagnostics_max_stdout_chars(),
            max_stderr_chars: default_diagnostics_max_stderr_chars(),
        }
    }
}

impl Default for ToolsConfig {
    fn default() -> Self {
        Self {
            enabled: default_true(),
            max_rounds: 0,
            loading_mode: default_tools_loading_mode(),
        }
    }
}

impl Default for SkillsConfig {
    fn default() -> Self {
        Self {
            enabled: default_true(),
            allow_command_execution: default_true(),
        }
    }
}

impl Default for MemoryConfig {
    fn default() -> Self {
        Self {
            enabled: default_true(),
            evicted_context_enabled: default_true(),
            association_enabled: default_true(),
            auto_diary_enabled: default_true(),
            auto_fact_enabled: default_true(),
            auto_skill_enabled: false,
            association_facts: default_memory_association_facts(),
            association_episodes: default_memory_association_episodes(),
            association_max_chars: default_memory_association_max_chars(),
            snippet_chars: default_memory_snippet_chars(),
            forget_after_days: default_memory_forget_after_days(),
            forgetting_enabled: default_true(),
            forgetting_half_life_days: default_memory_half_life_days(),
            forgetting_min_strength: default_memory_min_strength(),
            forgetting_review_boost: default_memory_review_boost(),
            learning_min_task_chars: default_memory_min_task_chars(),
            learning_min_method_chars: default_memory_min_method_chars(),
        }
    }
}

impl Default for ContextConfig {
    fn default() -> Self {
        Self {
            trim_at_ratio: default_trim_at_ratio(),
            trim_batch_ratio: default_trim_batch_ratio(),
            on_overflow: default_on_overflow(),
        }
    }
}

impl ProviderConfig {
    pub fn default_opencodezen() -> Self {
        Self {
            id: OPENCODE_PROVIDER_ID.to_string(),
            display_name: "opencode Zen".to_string(),
            base_url: OPENCODE_ZEN_BASE_URL.to_string(),
            protocol: default_provider_protocol(),
            api_key: None,
            models: vec![OPENCODE_DEFAULT_CHAT_MODEL.to_string()],
            model_context_window: HashMap::new(),
            model_modalities: HashMap::new(),
            default_model: OPENCODE_DEFAULT_CHAT_MODEL.to_string(),
            timeout_seconds: default_timeout(),
            temperature: default_temperature(),
        }
    }

    pub fn default_openai() -> Self {
        Self {
            id: "openai".to_string(),
            display_name: "OpenAI-compatible".to_string(),
            base_url: "https://api.openai.com/v1".to_string(),
            protocol: default_provider_protocol(),
            api_key: Some("$env:OPENAI_API_KEY".to_string()),
            models: vec!["gpt-4o-mini".to_string()],
            model_context_window: HashMap::new(),
            model_modalities: HashMap::new(),
            default_model: "gpt-4o-mini".to_string(),
            timeout_seconds: default_timeout(),
            temperature: default_temperature(),
        }
    }

    pub fn default_templates() -> Vec<Self> {
        let mut providers = vec![Self::default_opencodezen()];
        providers.extend([
            Self::template("openai", "OpenAI", "https://api.openai.com/v1"),
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
        providers
    }

    fn template(id: &str, display_name: &str, base_url: &str) -> Self {
        Self {
            id: id.to_string(),
            display_name: display_name.to_string(),
            base_url: base_url.to_string(),
            protocol: default_provider_protocol(),
            api_key: None,
            models: Vec::new(),
            model_context_window: HashMap::new(),
            model_modalities: HashMap::new(),
            default_model: String::new(),
            timeout_seconds: default_timeout(),
            temperature: default_temperature(),
        }
    }

    pub fn new_openai_compatible() -> Self {
        let mut provider = Self::default_openai();
        provider.models.clear();
        provider.default_model.clear();
        provider
    }

    pub fn supports_vision(&self, model: &str) -> Option<bool> {
        if let Some(modalities) = self.model_modalities.get(model) {
            return Some(modalities.iter().any(|m| m == "image"));
        }
        crate::models_cache::supports_vision(&self.id, model)
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
        if let Some(api_key) = secrets
            .api_keys
            .get(&self.id)
            .cloned()
            .filter(|value| !value.trim().is_empty())
        {
            return Ok(api_key);
        }

        if self.is_opencode_zen() {
            return Ok("public".to_string());
        }

        bail!("missing API key for provider {}", self.id)
    }

    pub fn is_opencode_zen(&self) -> bool {
        matches!(self.id.as_str(), OPENCODE_PROVIDER_ID | "opencodezen")
            && self.base_url.trim_end_matches('/') == OPENCODE_ZEN_BASE_URL
    }
}

impl AppConfig {
    pub fn memory_config(&self) -> &MemoryConfig {
        if self.memory != MemoryConfig::default() {
            &self.memory
        } else {
            &self.plugins.memory
        }
    }

    pub fn load(paths: &MiyuPaths) -> Result<Self> {
        let raw = std::fs::read_to_string(&paths.config_file)
            .with_context(|| format!("failed to read {}", paths.config_file.display()))?;
        let stripped = json_comments::StripComments::new(raw.as_bytes());
        let mut config: Self = serde_json::from_reader(stripped)
            .with_context(|| format!("invalid JSONC in {}", paths.config_file.display()))?;
        config.normalize_builtin_providers();
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

    fn normalize_builtin_providers(&mut self) {
        for provider in ProviderConfig::default_templates() {
            if !self.providers.iter().any(|item| {
                item.id == provider.id
                    || provider.id == OPENCODE_PROVIDER_ID && item.is_opencode_zen()
            }) {
                self.providers.push(provider);
            }
        }
        if self.active_provider == "opencodezen" {
            self.active_provider = OPENCODE_PROVIDER_ID.to_string();
        }
        if self.plugins.vision.vision_provider_id == "opencodezen" {
            self.plugins.vision.vision_provider_id = OPENCODE_PROVIDER_ID.to_string();
        }
        if self
            .provider(None)
            .map(|provider| provider.default_model.trim().is_empty())
            .unwrap_or(true)
        {
            self.active_provider = OPENCODE_PROVIDER_ID.to_string();
        }
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
        }
        if !(0.1..=1.0).contains(&self.context.trim_at_ratio) {
            bail!("context.trim_at_ratio must be between 0.1 and 1.0");
        }
        if !(0.01..=0.9).contains(&self.context.trim_batch_ratio) {
            bail!("context.trim_batch_ratio must be between 0.01 and 0.9");
        }
        match self.context.on_overflow.as_str() {
            "pop" | "compact" => {}
            value => bail!("context.on_overflow must be 'pop' or 'compact', got: {value}"),
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
        if self.plugins.web.max_results == 0 {
            bail!("plugins.web.max_results must be greater than 0");
        }
        match self.plugins.deep_research.thinking_depth.as_str() {
            "minimal" | "low" | "medium" | "high" | "xhigh" => {}
            value => bail!("plugins.deep_research.thinking_depth is invalid: {value}"),
        }
        match self.plugins.deep_diagnose.thinking_depth.as_str() {
            "minimal" | "low" | "medium" | "high" | "xhigh" => {}
            value => bail!("plugins.deep_diagnose.thinking_depth is invalid: {value}"),
        }
        if self.plugins.deep_diagnose.tool_call_timeout_seconds == 0 {
            bail!("plugins.deep_diagnose.tool_call_timeout_seconds must be greater than 0");
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
                let models =
                    if provider.models.is_empty() && !provider.default_model.trim().is_empty() {
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

    pub fn remove_active_provider_model(&mut self, provider_id: &str, model: &str) -> Result<()> {
        let provider = self
            .providers
            .iter_mut()
            .find(|provider| provider.id == provider_id)
            .with_context(|| format!("provider not found: {provider_id}"))?;
        if model.trim().is_empty() {
            bail!("model cannot be empty");
        }
        provider.models.retain(|item| item != model);
        provider.model_context_window.remove(model);
        provider.model_modalities.remove(model);
        if provider.default_model == model {
            provider.default_model = provider.models.first().cloned().unwrap_or_default();
        }
        Ok(())
    }

    pub fn active_context_window(&self) -> Result<Option<usize>> {
        let provider = self.provider(None)?;
        let model = &provider.default_model;
        if let Some(window) = provider
            .model_context_window
            .get(model)
            .copied()
            .filter(|&w| w > 0)
        {
            return Ok(Some(window));
        }
        Ok(crate::models_cache::context_window(model).map(|w| w as usize))
    }

    pub fn system_prompt(&self, paths: &MiyuPaths) -> Result<String> {
        let mut prompt = self.base_system_prompt(paths)?;
        let user_identity = self.user_identity_prompt(paths)?;
        if !user_identity.trim().is_empty() {
            prompt.push_str("\n\n<current-user-profile>\n");
            prompt.push_str("This profile describes the user currently interacting with you.\n\n");
            prompt.push_str(user_identity.trim());
            prompt.push_str("\n</current-user-profile>");
        }
        Ok(prompt)
    }

    pub fn base_system_prompt(&self, paths: &MiyuPaths) -> Result<String> {
        let persona = self.active_persona_prompt(paths)?;
        if persona.trim().is_empty() {
            Ok(default_system_prompt())
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

    pub fn persona_memory_data_dir(&self, paths: &MiyuPaths, persona: &str) -> PathBuf {
        paths
            .data_dir
            .join("personas")
            .join(persona_scope_name(persona))
    }

    pub fn persona_memory_state_dir(&self, paths: &MiyuPaths, persona: &str) -> PathBuf {
        paths
            .state_dir
            .join("personas")
            .join(persona_scope_name(persona))
    }

    pub fn persona_skills_dir(&self, paths: &MiyuPaths, persona: &str) -> PathBuf {
        paths
            .skills_dir
            .join("personas")
            .join(persona_scope_name(persona))
    }

    pub fn active_persona_memory_data_dir(&self, paths: &MiyuPaths) -> PathBuf {
        self.persona_memory_data_dir(paths, self.prompt.active_persona.trim())
    }

    pub fn active_persona_memory_state_dir(&self, paths: &MiyuPaths) -> PathBuf {
        self.persona_memory_state_dir(paths, self.prompt.active_persona.trim())
    }

    pub fn active_persona_skills_dir(&self, paths: &MiyuPaths) -> PathBuf {
        self.persona_skills_dir(paths, self.prompt.active_persona.trim())
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

fn persona_scope_name(name: &str) -> String {
    let name = name.trim();
    if name.is_empty() {
        return "default".to_string();
    }
    let normalized = name
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
                ch.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect::<String>()
        .trim_matches('-')
        .to_string();
    if normalized.is_empty() {
        format!("persona-{}", &blake3::hash(name.as_bytes()).to_hex()[..12])
    } else {
        normalized
    }
}

fn default_temperature() -> f32 {
    0.7
}

fn is_default_timeout(value: &u64) -> bool {
    *value == default_timeout()
}

fn is_default_temperature(value: &f32) -> bool {
    (*value - default_temperature()).abs() < f32::EPSILON
}

fn default_provider_protocol() -> String {
    "auto".to_string()
}

fn is_auto_protocol(value: &str) -> bool {
    value.trim().is_empty() || value == "auto"
}

fn default_true() -> bool {
    true
}

fn default_tools_loading_mode() -> String {
    "lazy".to_string()
}

fn default_reasoning_display() -> String {
    "summary".to_string()
}

fn default_tool_call_display() -> String {
    "summary".to_string()
}

fn default_memory_association_facts() -> usize {
    5
}

fn default_memory_association_episodes() -> usize {
    3
}

fn default_memory_association_max_chars() -> usize {
    1800
}

fn default_memory_snippet_chars() -> usize {
    500
}

fn default_memory_forget_after_days() -> u64 {
    90
}

fn default_memory_half_life_days() -> f64 {
    7.0
}

fn default_memory_min_strength() -> f64 {
    0.15
}

fn default_memory_review_boost() -> f64 {
    0.35
}

fn default_memory_min_task_chars() -> usize {
    16
}

fn default_memory_min_method_chars() -> usize {
    120
}

fn default_print_image_width_percent() -> u8 {
    45
}

fn default_print_image_height_percent() -> u8 {
    35
}

fn default_memes_width_percent() -> u8 {
    35
}

fn default_memes_height_percent() -> u8 {
    25
}

fn default_memes_max_image_mb() -> u64 {
    10
}

fn default_memes_auto_send_probability() -> f32 {
    0.2
}

fn default_web_search_max_results() -> usize {
    2
}

fn default_web_images_max_results() -> usize {
    5
}

fn default_web_images_max_download_mb() -> f64 {
    4.0
}

fn default_web_images_preview_count() -> usize {
    1
}

fn default_web_images_timeout() -> u64 {
    20
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

fn default_subagent_max_tool_steps() -> usize {
    100
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
            return pictures.join("miyu/generated-images").display().to_string();
        }
    }
    "~/Pictures/miyu/generated-images".to_string()
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

fn default_diagnostics_timeout() -> u64 {
    5
}

fn default_diagnostics_max_stdout_chars() -> usize {
    8_000
}

fn default_diagnostics_max_stderr_chars() -> usize {
    4_000
}

fn default_calculator_backend() -> String {
    "rust-simple".to_string()
}

fn default_trim_at_ratio() -> f32 {
    0.9
}

fn default_trim_batch_ratio() -> f32 {
    0.15
}

fn default_on_overflow() -> String {
    "pop".to_string()
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_config_can_be_saved_without_active_model() {
        let mut config = AppConfig::default();
        config.providers[0].models.clear();
        config.providers[0].default_model.clear();
        assert!(config.validate().is_ok());
    }

    #[test]
    fn provider_model_choices_ignore_unconfigured_models() {
        let mut config = AppConfig::default();
        config.providers[0].models.clear();
        config.providers[0].default_model.clear();
        assert!(config.provider_model_choices().is_empty());
    }

    #[test]
    fn new_openai_compatible_provider_has_no_active_model() {
        let provider = ProviderConfig::new_openai_compatible();

        assert!(provider.models.is_empty());
        assert!(provider.default_model.is_empty());
    }

    #[test]
    fn remove_active_provider_model_clears_removed_current_model() {
        let mut config = AppConfig::default();
        let provider_id = config.providers[0].id.clone();
        config.providers[0].models = vec!["old-model".to_string(), "next-model".to_string()];
        config.providers[0].default_model = "old-model".to_string();
        config.providers[0]
            .model_context_window
            .insert("old-model".to_string(), 8192);
        config.providers[0]
            .model_modalities
            .insert("old-model".to_string(), vec!["text".to_string()]);

        config
            .remove_active_provider_model(&provider_id, "old-model")
            .unwrap();

        assert_eq!(config.providers[0].models, vec!["next-model"]);
        assert_eq!(config.providers[0].default_model, "next-model");
        assert!(!config.providers[0]
            .model_context_window
            .contains_key("old-model"));
        assert!(!config.providers[0]
            .model_modalities
            .contains_key("old-model"));
    }

    #[test]
    fn remove_active_provider_model_clears_last_current_model() {
        let mut config = AppConfig::default();
        let provider_id = config.providers[0].id.clone();
        config.providers[0].models = vec!["old-model".to_string()];
        config.providers[0].default_model = "old-model".to_string();

        config
            .remove_active_provider_model(&provider_id, "old-model")
            .unwrap();

        assert!(config.providers[0].models.is_empty());
        assert!(config.providers[0].default_model.is_empty());
        assert!(config.provider_model_choices().is_empty());
    }

    #[test]
    fn display_readable_tool_names_defaults_enabled() {
        let display: DisplayConfig = serde_json::from_str(r#"{"tool_calls":"summary"}"#).unwrap();
        assert!(display.readable_tool_names);
        assert!(!display.show_token_usage);

        let display: DisplayConfig = serde_json::from_str(r#"{"show_token_usage":true}"#).unwrap();
        assert!(display.show_token_usage);
    }

    #[test]
    fn meme_library_defaults_follow_persona() {
        let memes = MemesPluginConfig::default();
        assert_eq!(memes.library_for_persona(""), "miyu");
        assert_eq!(
            memes.library_for_persona("Custom Persona"),
            "custom-persona"
        );
        assert!(memes.auto_send_enabled);
        assert_eq!(memes.auto_send_probability, 0.2);
    }
}
