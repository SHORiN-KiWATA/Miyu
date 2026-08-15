use crate::default_models::{
    OPENCODE_DEFAULT_CHAT_MODEL, OPENCODE_DEFAULT_VISION_MODEL, OPENCODE_PROVIDER_ID,
    OPENCODE_ZEN_BASE_URL,
};
use crate::paths::MiyuPaths;
use crate::prompts::default_system_prompt;
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Deserializer, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};

pub const MAX_COMMAND_OUTPUT_LINES: usize = 1_000;
/// Replay redraws whole turns, so a large value floods the screen on startup.
pub const MAX_REPL_REPLAY_TURNS: usize = 20;
pub const CURRENT_CONFIG_VERSION: u32 = 2;
const LEGACY_DEFAULT_TEMPERATURE: f32 = 0.7;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    #[serde(default)]
    pub config_version: u32,
    pub active_provider: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_provider_models: Option<Vec<ActiveProviderModelConfig>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_multimodal_provider_models: Option<Vec<ActiveProviderModelConfig>>,
    pub providers: Vec<ProviderConfig>,
    #[serde(default, skip_serializing_if = "EmbeddingConfig::is_default")]
    pub embedding: EmbeddingConfig,
    #[serde(default)]
    pub context: ContextConfig,
    #[serde(default)]
    pub tools: ToolsConfig,
    #[serde(default, skip_serializing_if = "CacheConfig::is_default")]
    pub cache: CacheConfig,
    #[serde(default)]
    pub mcp: McpConfig,
    #[serde(default)]
    pub skills: SkillsConfig,
    #[serde(default)]
    pub display: DisplayConfig,
    #[serde(default)]
    pub notifications: NotificationsConfig,
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
    #[serde(default, skip_serializing_if = "SubagentTiersConfig::is_empty")]
    pub subagent_tiers: SubagentTiersConfig,
    #[serde(default, skip_serializing_if = "PlatformsConfig::is_empty")]
    pub platforms: PlatformsConfig,
}

/// Provider prompt-cache tuning (v7, DeepSeek 高命中策略实测产物). The
/// tuning knobs default to off — they trade a little latency or a few cheap
/// requests for prefix-cache hits on best-effort provider caches. The
/// accounting log defaults to on (numbers only, ~0.2 KB per request).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct CacheConfig {
    /// Idle keepalive: while the agent waits for the next user turn, re-send
    /// the exact prompt prefix of the last request every N seconds as a
    /// non-streaming max_tokens=1 completion so hot-tier prefix caches
    /// (DeepSeek-style) keep the deep prefix alive across turn gaps. The ping
    /// is billed at the provider's cache-hit input price. 0 disables (the
    /// default — enable only after measuring your provider: on per-REQUEST
    /// billed endpoints every ping burns quota for nothing).
    /// Only effective in long-lived processes (daemon/REPL); one-shot `ask`
    /// exits before any ping fires.
    pub keepalive_seconds: u64,
    /// Stop pinging after this many keepalives per turn (bounds idle cost).
    pub keepalive_max_pings: u32,
    /// Provider cache writes are asynchronous (measured: a follow-up within
    /// ~2s can miss the prefix the previous request just computed). When >0,
    /// consecutive tool-loop requests wait until at least this many
    /// milliseconds have passed since the previous round completed.
    pub write_grace_ms: u64,
    /// Per-request cache accounting log: one JSONL line of absolute token
    /// numbers (prompt/cache_read/completion/…) per LLM request under
    /// cache/logs/cache-usage.<date>.jsonl. Numbers only — never prompt text.
    /// Roughly 0.2 KB per request; daily files, pruned by retention below.
    pub request_log: bool,
    /// Days of cache-usage JSONL files to keep (older files are deleted when
    /// a new line is written).
    pub request_log_retention_days: u64,
}

impl Default for CacheConfig {
    fn default() -> Self {
        Self {
            keepalive_seconds: 0,
            keepalive_max_pings: 20,
            write_grace_ms: 0,
            request_log: true,
            request_log_retention_days: 14,
        }
    }
}

impl CacheConfig {
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

/// Messaging-platform settings. Public configuration is named after the
/// product users connect to; transport protocols remain implementation
/// details of each platform adapter.
pub const DEFAULT_PLATFORM_COMMAND_PREFIX: &str = "/";
pub const MAX_PLATFORM_COMMAND_PREFIX_CHARS: usize = 32;
pub const MAX_PLATFORM_SESSION_RUNNING: usize = 16;
pub const MAX_PLATFORM_SESSION_QUEUED: usize = 64;

/// Group overflow handling. Groups and terminal sessions want opposite things
/// here: a coding session benefits from `compact` folding old turns into a
/// summary it can keep reasoning from, while summarising a group log destroys
/// the structured record every `回复引用: msg=…` points at. Groups drop whole
/// turns instead, and drop a lot at once so the surviving prefix stays stable
/// for a long stretch rather than being clipped every few turns.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PlatformGroupContextConfig {
    /// `compact` / `pop`; empty inherits `context.on_overflow`.
    pub on_overflow: String,
    /// Fraction of the window released in one trim; 0 inherits
    /// `context.trim_batch_ratio`.
    pub trim_batch_ratio: f32,
}

impl Default for PlatformGroupContextConfig {
    fn default() -> Self {
        Self {
            on_overflow: "pop".to_string(),
            trim_batch_ratio: 0.5,
        }
    }
}

impl PlatformGroupContextConfig {
    fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct PlatformSessionLimits {
    pub running: usize,
    pub queued: usize,
}

impl Default for PlatformSessionLimits {
    fn default() -> Self {
        Self {
            running: 8,
            queued: 16,
        }
    }
}

impl PlatformSessionLimits {
    fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlatformsConfig {
    #[serde(
        default = "default_platform_command_prefix",
        skip_serializing_if = "is_default_platform_command_prefix"
    )]
    pub command_prefix: String,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub commands: BTreeMap<String, PlatformCommandConfig>,
    #[serde(default, skip_serializing_if = "OneBotConfig::is_default")]
    pub qq: OneBotConfig,
}

impl Default for PlatformsConfig {
    fn default() -> Self {
        Self {
            command_prefix: default_platform_command_prefix(),
            commands: BTreeMap::new(),
            qq: OneBotConfig::default(),
        }
    }
}

impl PlatformsConfig {
    pub fn is_empty(&self) -> bool {
        self == &Self::default()
    }

    pub fn command_permission(
        &self,
        command: &str,
        default: PlatformCommandPermission,
    ) -> PlatformCommandPermission {
        self.commands
            .get(command)
            .map(|config| config.permission)
            .unwrap_or(default)
    }

    pub fn set_command_permission(
        &mut self,
        command: &str,
        permission: PlatformCommandPermission,
        default: PlatformCommandPermission,
    ) {
        if permission == default {
            self.commands.remove(command);
        } else {
            self.commands
                .insert(command.to_string(), PlatformCommandConfig { permission });
        }
    }

    pub fn model_route(
        &self,
        kind: PlatformConversationKind,
        conversation_id: &str,
    ) -> Option<&PlatformModelRoute> {
        self.qq
            .conversations
            .iter()
            .find(|route| route.matches(kind, conversation_id))
    }

    pub fn model_route_mut(
        &mut self,
        kind: PlatformConversationKind,
        conversation_id: &str,
    ) -> Option<&mut PlatformModelRoute> {
        self.qq
            .conversations
            .iter_mut()
            .find(|route| route.matches(kind, conversation_id))
    }

    /// Inserts a route or replaces the route with the same stable identity.
    /// Inherited pools are meaningful conversation configuration and are kept
    /// until the user explicitly removes the entry.
    pub fn upsert_model_route(&mut self, mut route: PlatformModelRoute) {
        route.normalize();
        if let Some(index) = self
            .qq
            .conversations
            .iter()
            .position(|existing| existing.identity() == route.identity())
        {
            self.qq.conversations[index] = route;
        } else {
            self.qq.conversations.push(route);
        }
    }

    pub fn remove_model_route(
        &mut self,
        kind: PlatformConversationKind,
        conversation_id: &str,
    ) -> bool {
        let old_len = self.qq.conversations.len();
        self.qq
            .conversations
            .retain(|route| !route.matches(kind, conversation_id));
        self.qq.conversations.len() != old_len
    }

    pub fn rename_persona_references(&mut self, old_name: &str, new_name: &str) {
        for route in &mut self.qq.conversations {
            if route.persona.custom_name() == Some(old_name) {
                route.persona = PlatformPersonaOverride::Custom {
                    name: new_name.to_string(),
                };
            }
        }
    }

    pub fn persona_reference_count(&self, name: &str) -> usize {
        self.qq
            .conversations
            .iter()
            .filter(|route| route.persona.custom_name() == Some(name))
            .count()
    }

    pub fn normalize_model_routes(&mut self) {
        self.command_prefix = self.command_prefix.trim().to_string();
        self.qq.private_chats.migrate_legacy_rate_limit();
        self.qq.group_chats.migrate_legacy_rate_limits();
        self.qq.admin_users.sort_unstable();
        self.qq.admin_users.dedup();
        self.qq.private_chats.whitelist.sort_unstable();
        self.qq.private_chats.whitelist.dedup();
        self.qq.group_chats.whitelist.sort_unstable();
        self.qq.group_chats.whitelist.dedup();
        let mut keywords = HashSet::with_capacity(self.qq.group_chats.trigger_keywords.len());
        self.qq.group_chats.trigger_keywords = self
            .qq
            .group_chats
            .trigger_keywords
            .drain(..)
            .map(|keyword| keyword.trim().to_string())
            .filter(|keyword| !keyword.is_empty() && keywords.insert(keyword.clone()))
            .collect();
        self.qq.asset_base_url = self
            .qq
            .asset_base_url
            .trim()
            .trim_end_matches('/')
            .to_string();
        normalize_route_pool(&mut self.qq.text_models);
        normalize_route_pool(&mut self.qq.multimodal_models);
        normalize_route_pool(&mut self.qq.non_whitelist_text_models);
        for route in &mut self.qq.conversations {
            route.normalize();
        }
        migrate_message_history_instance(&mut self.qq.plugins);
        if let Some(instance) = self.qq.plugins.get_mut(REAL_CONTEXT_PLUGIN_ID) {
            normalize_real_context_instance(instance);
        }
        self.qq
            .plugins
            .retain(|name, instance| !name.trim().is_empty() && !instance.is_empty());
    }

    pub fn prune_model_references(&mut self, providers: &[ProviderConfig]) {
        prune_pool(&mut self.qq.text_models, providers, false);
        prune_pool(&mut self.qq.multimodal_models, providers, true);
        prune_pool(&mut self.qq.non_whitelist_text_models, providers, false);
        for route in &mut self.qq.conversations {
            route.prune_model_references(providers);
        }
        mutate_real_context_settings(&mut self.qq.plugins, |settings| {
            for pool in [&mut settings.text_models] {
                if let Some(models) = pool {
                    models.retain(|model| active_model_exists(providers, model));
                }
                normalize_route_pool(pool);
            }
        });
        self.normalize_model_routes();
    }

    pub fn remove_model_references(&mut self, provider_id: &str, model: &str) {
        for pool in [
            &mut self.qq.text_models,
            &mut self.qq.multimodal_models,
            &mut self.qq.non_whitelist_text_models,
        ] {
            if let Some(entries) = pool {
                entries.retain(|entry| !(entry.provider_id == provider_id && entry.model == model));
            }
            normalize_route_pool(pool);
        }
        for route in &mut self.qq.conversations {
            route.remove_model_references(provider_id, model);
        }
        mutate_real_context_settings(&mut self.qq.plugins, |settings| {
            for pool in [&mut settings.text_models] {
                if let Some(models) = pool {
                    models.retain(|entry| {
                        !(entry.provider_id == provider_id && entry.model == model)
                    });
                }
                normalize_route_pool(pool);
            }
        });
        self.normalize_model_routes();
    }

    pub fn remove_provider_references(&mut self, provider_id: &str) {
        for pool in [
            &mut self.qq.text_models,
            &mut self.qq.multimodal_models,
            &mut self.qq.non_whitelist_text_models,
        ] {
            if let Some(entries) = pool {
                entries.retain(|entry| entry.provider_id != provider_id);
            }
            normalize_route_pool(pool);
        }
        for route in &mut self.qq.conversations {
            for pool in [&mut route.text_models, &mut route.multimodal_models] {
                if let Some(entries) = pool {
                    entries.retain(|entry| entry.provider_id != provider_id);
                }
                normalize_route_pool(pool);
            }
        }
        mutate_real_context_settings(&mut self.qq.plugins, |settings| {
            for pool in [&mut settings.text_models] {
                if let Some(models) = pool {
                    models.retain(|entry| entry.provider_id != provider_id);
                }
                normalize_route_pool(pool);
            }
        });
        self.normalize_model_routes();
    }

    pub fn rename_provider_references(&mut self, old_id: &str, new_id: &str) {
        for pool in [
            &mut self.qq.text_models,
            &mut self.qq.multimodal_models,
            &mut self.qq.non_whitelist_text_models,
        ] {
            if let Some(entries) = pool {
                rename_provider_in_pool(entries, old_id, new_id);
            }
            normalize_route_pool(pool);
        }
        for route in &mut self.qq.conversations {
            route.rename_provider_references(old_id, new_id);
        }
        mutate_real_context_settings(&mut self.qq.plugins, |settings| {
            for pool in [&mut settings.text_models] {
                if let Some(models) = pool {
                    rename_provider_in_pool(models, old_id, new_id);
                }
                normalize_route_pool(pool);
            }
        });
    }

    pub fn rename_model_references(&mut self, provider_id: &str, old: &str, new: &str) {
        for pool in [
            &mut self.qq.text_models,
            &mut self.qq.multimodal_models,
            &mut self.qq.non_whitelist_text_models,
        ] {
            if let Some(entries) = pool {
                for entry in entries {
                    if entry.provider_id == provider_id && entry.model == old {
                        entry.model = new.to_string();
                    }
                }
            }
            normalize_route_pool(pool);
        }
        for route in &mut self.qq.conversations {
            route.rename_model_references(provider_id, old, new);
        }
        mutate_real_context_settings(&mut self.qq.plugins, |settings| {
            for pool in [&mut settings.text_models] {
                if let Some(models) = pool {
                    for entry in models {
                        if entry.provider_id == provider_id && entry.model == old {
                            entry.model = new.to_string();
                        }
                    }
                }
                normalize_route_pool(pool);
            }
        });
    }
}

fn prune_pool(
    pool: &mut Option<Vec<ActiveProviderModelConfig>>,
    providers: &[ProviderConfig],
    require_multimodal: bool,
) {
    if let Some(models) = pool {
        models.retain(|model| {
            active_model_exists(providers, model)
                && (!require_multimodal || active_model_supports_image(providers, model))
        });
    }
    normalize_route_pool(pool);
}

fn default_platform_command_prefix() -> String {
    DEFAULT_PLATFORM_COMMAND_PREFIX.to_string()
}

fn is_default_platform_command_prefix(value: &String) -> bool {
    value == DEFAULT_PLATFORM_COMMAND_PREFIX
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlatformCommandPermission {
    Everyone,
    #[default]
    AdminOnly,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlatformCommandConfig {
    #[serde(default)]
    pub permission: PlatformCommandPermission,
}

pub type PlatformPluginsConfig = BTreeMap<String, PlatformPluginInstanceConfig>;

type PlatformPluginConfigValidator = fn(&PlatformPluginInstanceConfig) -> Result<()>;

pub const REAL_CONTEXT_PLUGIN_ID: &str = "real_context";
pub const QQ_MESSAGE_HISTORY_PLUGIN_ID: &str = "qq_message_history";
pub const QQ_GROUP_MANAGEMENT_PLUGIN_ID: &str = "qq_group_management";
pub const QQ_MESSAGE_RECALL_PLUGIN_ID: &str = "qq_message_recall";
pub const QQ_MEME_COLLECTOR_PLUGIN_ID: &str = "qq_meme_collector";

const PLATFORM_PLUGIN_VALIDATORS: &[(&str, PlatformPluginConfigValidator)] = &[
    ("reply_processor", validate_reply_processor_plugin_config),
    (REAL_CONTEXT_PLUGIN_ID, validate_real_context_plugin_config),
    (
        QQ_MESSAGE_HISTORY_PLUGIN_ID,
        validate_qq_message_history_plugin_config,
    ),
    (
        QQ_GROUP_MANAGEMENT_PLUGIN_ID,
        validate_qq_group_management_plugin_config,
    ),
    (
        QQ_MESSAGE_RECALL_PLUGIN_ID,
        validate_qq_message_recall_plugin_config,
    ),
    (
        QQ_MEME_COLLECTOR_PLUGIN_ID,
        validate_qq_meme_collector_plugin_config,
    ),
];

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PlatformPluginInstanceConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    #[serde(default, skip_serializing_if = "serde_json::Map::is_empty")]
    pub settings: serde_json::Map<String, serde_json::Value>,
}

impl PlatformPluginInstanceConfig {
    pub fn is_empty(&self) -> bool {
        self.enabled.is_none() && self.settings.is_empty()
    }

    pub fn enabled_or(&self, default: bool) -> bool {
        self.enabled.unwrap_or(default)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct QqGroupManagementPluginSettings {
    pub enable_tool: bool,
    pub enable_kick_tool: bool,
    pub enable_special_title_tool: bool,
    pub enable_record: bool,
    pub enable_offender_history: bool,
    pub sync_external_unmute_notice: bool,
    pub default_duration_seconds: u64,
    pub max_reason_length: usize,
    pub max_special_title_length: usize,
    pub max_special_title_duration_seconds: i64,
    pub max_groups: usize,
    pub max_records_per_group: usize,
    pub expired_record_retention_seconds: u64,
    pub cleanup_interval_seconds: u64,
    pub max_offender_history_per_group: usize,
    pub max_kick_history_per_group: usize,
}

impl Default for QqGroupManagementPluginSettings {
    fn default() -> Self {
        Self {
            enable_tool: true,
            enable_kick_tool: true,
            enable_special_title_tool: true,
            enable_record: true,
            enable_offender_history: true,
            sync_external_unmute_notice: true,
            default_duration_seconds: 600,
            max_reason_length: 500,
            max_special_title_length: 18,
            max_special_title_duration_seconds: -1,
            max_groups: 200,
            max_records_per_group: 500,
            expired_record_retention_seconds: 604_800,
            cleanup_interval_seconds: 300,
            max_offender_history_per_group: 500,
            max_kick_history_per_group: 500,
        }
    }
}

impl QqGroupManagementPluginSettings {
    pub fn from_instance(instance: &PlatformPluginInstanceConfig) -> Result<Self> {
        serde_json::from_value(serde_json::Value::Object(instance.settings.clone()))
            .context("invalid qq_group_management plugin settings")
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct QqMessageRecallPluginSettings {
    pub enable_tool: bool,
    pub capture_outgoing_messages: bool,
    pub max_reason_length: usize,
    pub max_messages_per_conversation: usize,
    pub cancel_record_ttl_seconds: u64,
    pub cancel_cleanup_interval_seconds: u64,
}

impl Default for QqMessageRecallPluginSettings {
    fn default() -> Self {
        Self {
            enable_tool: true,
            capture_outgoing_messages: true,
            max_reason_length: 500,
            max_messages_per_conversation: 20,
            cancel_record_ttl_seconds: 300,
            cancel_cleanup_interval_seconds: 60,
        }
    }
}

impl QqMessageRecallPluginSettings {
    pub fn from_instance(instance: &PlatformPluginInstanceConfig) -> Result<Self> {
        serde_json::from_value(serde_json::Value::Object(instance.settings.clone()))
            .context("invalid qq_message_recall plugin settings")
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct QqMemeCollectorPluginSettings {
    pub collect_probability: f64,
    pub max_images_per_message: usize,
    pub allow_non_admin_save_tool: bool,
}

impl Default for QqMemeCollectorPluginSettings {
    fn default() -> Self {
        Self {
            collect_probability: 0.02,
            max_images_per_message: 2,
            allow_non_admin_save_tool: false,
        }
    }
}

impl QqMemeCollectorPluginSettings {
    pub fn from_instance(instance: &PlatformPluginInstanceConfig) -> Result<Self> {
        serde_json::from_value(serde_json::Value::Object(instance.settings.clone()))
            .context("invalid qq_meme_collector plugin settings")
    }
}

fn validate_qq_group_management_plugin_config(
    instance: &PlatformPluginInstanceConfig,
) -> Result<()> {
    let settings = QqGroupManagementPluginSettings::from_instance(instance)?;
    if settings.max_reason_length > 10_000
        || settings.max_special_title_length > 100
        || settings.max_groups == 0
        || settings.max_records_per_group == 0
        || settings.max_offender_history_per_group == 0
        || settings.max_kick_history_per_group == 0
        || settings.cleanup_interval_seconds == 0
    {
        bail!("invalid qq_group_management plugin limits");
    }
    Ok(())
}

fn validate_qq_message_recall_plugin_config(instance: &PlatformPluginInstanceConfig) -> Result<()> {
    let settings = QqMessageRecallPluginSettings::from_instance(instance)?;
    if settings.max_reason_length > 10_000
        || settings.max_messages_per_conversation == 0
        || settings.max_messages_per_conversation > 1_000
        || settings.cancel_record_ttl_seconds < 10
        || settings.cancel_cleanup_interval_seconds < 5
    {
        bail!("invalid qq_message_recall plugin limits");
    }
    Ok(())
}

fn validate_qq_meme_collector_plugin_config(instance: &PlatformPluginInstanceConfig) -> Result<()> {
    let settings = QqMemeCollectorPluginSettings::from_instance(instance)?;
    if !settings.collect_probability.is_finite()
        || !(0.0..=1.0).contains(&settings.collect_probability)
        || !(1..=4).contains(&settings.max_images_per_message)
    {
        bail!("invalid qq_meme_collector plugin limits");
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct QqMessageHistoryPluginSettings {
    pub history_search_max_results: usize,
    pub history_safe_page_limit: usize,
    pub allow_cross_conversation_search: bool,
}

impl Default for QqMessageHistoryPluginSettings {
    fn default() -> Self {
        Self {
            history_search_max_results: 0,
            history_safe_page_limit: 500,
            allow_cross_conversation_search: true,
        }
    }
}

impl QqMessageHistoryPluginSettings {
    pub fn from_instance(instance: &PlatformPluginInstanceConfig) -> Result<Self> {
        serde_json::from_value(serde_json::Value::Object(instance.settings.clone()))
            .context("invalid qq_message_history plugin settings")
    }

    pub fn validate(&self) -> Result<()> {
        if self.history_safe_page_limit == 0 || self.history_safe_page_limit > 1_000 {
            bail!("platform plugin qq_message_history.history_safe_page_limit must be between 1 and 1000");
        }
        if self.history_search_max_results > self.history_safe_page_limit {
            bail!("platform plugin qq_message_history.history_search_max_results must be 0 or no greater than history_safe_page_limit");
        }
        Ok(())
    }
}

fn validate_qq_message_history_plugin_config(
    instance: &PlatformPluginInstanceConfig,
) -> Result<()> {
    QqMessageHistoryPluginSettings::from_instance(instance)?.validate()
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RealContextIdentityMapping {
    pub nickname: String,
    pub user_id: i64,
}

/// Configuration contract for the built-in QQ group real-context plugin.
///
/// The values intentionally stay flat in the generic platform-plugin map. This
/// keeps the persisted format forward compatible while giving the runtime and
/// TUI one strongly typed source of defaults and validation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct RealContextPluginSettings {
    /// How much group log the reply turn starts from. Once the history is
    /// append-only this is a one-off opening snapshot rather than a per-turn
    /// window, so it can afford to be generous.
    pub reply_context_window: usize,
    /// How much group log the active-reply judge sees. It rates the mood of the
    /// moment, so a longer window dilutes the recent signal and stretches the
    /// timeframe — and the judge runs on every message, not once per turn.
    pub judge_context_window: usize,
    #[serde(alias = "group_member_page_size")]
    pub group_member_search_max_results: usize,

    pub active_reply_enable: bool,
    pub judge_include_persona: bool,
    pub judge_persona_prompt: String,
    pub text_models: Option<Vec<ActiveProviderModelConfig>>,
    pub active_judge_probability: f64,
    pub reply_threshold: f64,
    pub judge_timeout_seconds: u64,
    pub judge_endpoint_timeout_seconds: u64,
    pub judge_queue_wait_timeout_seconds: u64,
    pub judge_max_concurrency: usize,
    pub judge_max_retries: usize,
    pub skip_pure_image_active_judge: bool,
    pub active_reply_supersede_enable: bool,
    pub active_reply_supersede_window_seconds: u64,
    pub reply_restraint_enable: bool,
    pub reply_restraint_recover_minutes: u64,
    pub reply_restraint_strength: String,
    pub reply_restraint_multiplier: f64,
    pub judge_relevance_weight: f64,
    pub judge_willingness_weight: f64,
    pub judge_social_weight: f64,
    pub judge_timing_weight: f64,
    pub judge_continuity_weight: f64,
    pub judge_should_reply_adjust_enable: bool,
    pub judge_should_reply_boost_score: f64,
    pub judge_should_reply_penalty_score: f64,

    pub continuation_enable: bool,
    pub continuation_window_seconds: u64,
    pub continuation_boost_score: f64,
    pub takeover_direct_trigger_enable: bool,
    pub takeover_direct_trigger_boost_score: f64,
    pub privileged_direct_trigger_skip_active_judgement: bool,

    pub active_reply_reaction_enable: bool,
    pub active_reply_reaction_emoji_ids: Vec<u32>,
    pub active_reply_reaction_timeout_seconds: u64,
    pub reply_target_enable: bool,
    pub reply_target_quote_enable: bool,
    pub reply_target_quote_after_other_messages: u64,
    pub reply_target_mention_enable: bool,
    pub reply_target_mention_after_seconds: u64,

    pub moderation_enable: bool,
    pub moderation_keyword_trigger_enable: bool,
    pub moderation_keywords: Vec<String>,
    pub moderation_min_severity: f64,
    pub moderation_timeout_seconds: u64,
    pub moderation_custom_rules: String,
    pub base64_moderation_enable: bool,
    pub base64_moderation_min_chars: usize,
    pub base64_moderation_max_decoded_chars: usize,
    pub base64_moderation_min_printable_ratio: f64,

    pub affection_enable: bool,
    pub affection_update_enable: bool,
    pub affection_update_timeout_seconds: u64,
    pub affection_initial_score: f64,
    pub affection_min_score: f64,
    pub affection_max_score: f64,
    pub affection_regular_max_score: f64,
    pub affection_unlimited_user_ids: Vec<i64>,
    pub affection_bias_min: f64,
    pub affection_bias_max: f64,
    pub affection_gain_pivot: f64,
    pub affection_delta_scale: f64,
    pub affection_delta_min: f64,
    pub affection_delta_max: f64,
    pub affection_update_confidence_threshold: f64,
    pub affection_daily_gain_limit: f64,
    pub affection_daily_loss_limit: f64,
    pub affection_auto_tag_enable: bool,
    pub affection_max_tags: usize,
    pub affection_recent_events_for_prompt: usize,
    pub affection_prompt_estranged: String,
    pub affection_prompt_cold: String,
    pub affection_prompt_neutral: String,
    pub affection_prompt_known: String,
    pub affection_prompt_friend: String,
    pub affection_prompt_trusted: String,
    pub affection_prompt_close: String,

    pub identity_mappings: Vec<RealContextIdentityMapping>,
}

impl Default for RealContextPluginSettings {
    fn default() -> Self {
        Self {
            reply_context_window: 25,
            judge_context_window: 20,
            group_member_search_max_results: 200,
            active_reply_enable: true,
            judge_include_persona: true,
            judge_persona_prompt: String::new(),
            text_models: None,
            active_judge_probability: 0.05,
            reply_threshold: 0.8,
            judge_timeout_seconds: 60,
            judge_endpoint_timeout_seconds: 15,
            judge_queue_wait_timeout_seconds: 15,
            judge_max_concurrency: 4,
            judge_max_retries: 1,
            skip_pure_image_active_judge: true,
            active_reply_supersede_enable: true,
            active_reply_supersede_window_seconds: 5,
            reply_restraint_enable: true,
            reply_restraint_recover_minutes: 3,
            reply_restraint_strength: "medium".to_string(),
            reply_restraint_multiplier: 1.0,
            judge_relevance_weight: 0.25,
            judge_willingness_weight: 0.25,
            judge_social_weight: 0.15,
            judge_timing_weight: 0.15,
            judge_continuity_weight: 0.20,
            judge_should_reply_adjust_enable: true,
            judge_should_reply_boost_score: 0.2,
            judge_should_reply_penalty_score: 0.2,
            continuation_enable: true,
            continuation_window_seconds: 15,
            continuation_boost_score: 0.1,
            takeover_direct_trigger_enable: true,
            takeover_direct_trigger_boost_score: 0.3,
            privileged_direct_trigger_skip_active_judgement: true,
            active_reply_reaction_enable: true,
            active_reply_reaction_emoji_ids: vec![289],
            active_reply_reaction_timeout_seconds: 600,
            reply_target_enable: true,
            reply_target_quote_enable: true,
            reply_target_quote_after_other_messages: 4,
            reply_target_mention_enable: true,
            reply_target_mention_after_seconds: 15,
            moderation_enable: true,
            moderation_keyword_trigger_enable: true,
            moderation_keywords: default_real_context_moderation_keywords(),
            moderation_min_severity: 7.0,
            moderation_timeout_seconds: 120,
            moderation_custom_rules: String::new(),
            base64_moderation_enable: true,
            base64_moderation_min_chars: 24,
            base64_moderation_max_decoded_chars: 5_000,
            base64_moderation_min_printable_ratio: 0.85,
            affection_enable: false,
            affection_update_enable: true,
            affection_update_timeout_seconds: 120,
            affection_initial_score: 10.0,
            affection_min_score: -50.0,
            affection_max_score: 100.0,
            affection_regular_max_score: 94.0,
            affection_unlimited_user_ids: Vec::new(),
            affection_bias_min: -0.2,
            affection_bias_max: 0.1,
            affection_gain_pivot: 60.0,
            affection_delta_scale: 1.0,
            affection_delta_min: -10.0,
            affection_delta_max: 2.0,
            affection_update_confidence_threshold: 0.8,
            affection_daily_gain_limit: 6.0,
            affection_daily_loss_limit: 15.0,
            affection_auto_tag_enable: true,
            affection_max_tags: 10,
            affection_recent_events_for_prompt: 3,
            affection_prompt_estranged: "你和该用户关系疏远。回复时保持克制、礼貌和简短，不主动延展话题，不使用熟人玩笑。拒绝为对方进行生图、天气搜索、复杂知识问答、塔罗牌、算卦等高级内容。".to_string(),
            affection_prompt_cold: "你对该用户态度冷淡。回复时以完成必要交流为主，避免热情、调侃和主动关心。拒绝为对方进行生图、复杂知识问答。".to_string(),
            affection_prompt_neutral: "你和该用户关系普通。按正常群聊或助手语气回复，保持自然、简洁和客观。".to_string(),
            affection_prompt_known: "你认识该用户。可以适度承接过往互动，语气比陌生人更自然，但不要表现得过分亲密。".to_string(),
            affection_prompt_friend: "你和该用户关系较熟。可以自然接话，允许轻微吐槽、接梗和熟人语气，但不要过度亲密。".to_string(),
            affection_prompt_trusted: "你信任该用户。回复时可以更主动承接上下文，表达更直接明确的判断，但仍要保持事实准确和边界。".to_string(),
            affection_prompt_close: "你和该用户是挚友。可以使用更熟悉、轻松的语气和轻微玩笑。".to_string(),
            identity_mappings: Vec::new(),
        }
    }
}

impl RealContextPluginSettings {
    pub fn from_instance(instance: &PlatformPluginInstanceConfig) -> Result<Self> {
        let mut settings = instance.settings.clone();
        migrate_real_context_settings_map(&mut settings);
        serde_json::from_value(serde_json::Value::Object(settings))
            .context("invalid real_context plugin settings")
    }

    pub fn normalize(&mut self) {
        self.judge_persona_prompt = self.judge_persona_prompt.trim().to_string();
        normalize_route_pool(&mut self.text_models);
        normalize_unique_strings(&mut self.moderation_keywords);
        self.active_reply_reaction_emoji_ids.retain(|id| *id > 0);
        self.active_reply_reaction_emoji_ids.sort_unstable();
        self.active_reply_reaction_emoji_ids.dedup();
        self.affection_unlimited_user_ids.retain(|id| *id > 0);
        self.affection_unlimited_user_ids.sort_unstable();
        self.affection_unlimited_user_ids.dedup();
        for mapping in &mut self.identity_mappings {
            mapping.nickname = mapping.nickname.trim().to_string();
        }
        let mut nicknames = HashSet::with_capacity(self.identity_mappings.len());
        self.identity_mappings.retain(|mapping| {
            !mapping.nickname.is_empty() && nicknames.insert(mapping.nickname.clone())
        });
    }

    pub fn validate(&self) -> Result<()> {
        validate_real_context_count("reply_context_window", self.reply_context_window, 1, 200)?;
        validate_real_context_count("judge_context_window", self.judge_context_window, 1, 200)?;
        validate_real_context_count(
            "group_member_search_max_results",
            self.group_member_search_max_results,
            1,
            200,
        )?;
        validate_real_context_probability(
            "active_judge_probability",
            self.active_judge_probability,
        )?;
        validate_real_context_probability("reply_threshold", self.reply_threshold)?;
        validate_real_context_count(
            "judge_timeout_seconds",
            self.judge_timeout_seconds as usize,
            0,
            600,
        )?;
        validate_real_context_count(
            "judge_endpoint_timeout_seconds",
            self.judge_endpoint_timeout_seconds as usize,
            1,
            600,
        )?;
        validate_real_context_count(
            "judge_queue_wait_timeout_seconds",
            self.judge_queue_wait_timeout_seconds as usize,
            1,
            600,
        )?;
        validate_real_context_count("judge_max_concurrency", self.judge_max_concurrency, 1, 64)?;
        validate_real_context_count("judge_max_retries", self.judge_max_retries, 0, 10)?;
        if self.judge_persona_prompt.len() > 32_768 || self.judge_persona_prompt.contains('\0') {
            bail!("platform plugin real_context.judge_persona_prompt is invalid");
        }
        validate_real_context_count(
            "active_reply_supersede_window_seconds",
            self.active_reply_supersede_window_seconds as usize,
            1,
            300,
        )?;
        validate_real_context_count(
            "reply_restraint_recover_minutes",
            self.reply_restraint_recover_minutes as usize,
            1,
            1_440,
        )?;
        if !matches!(
            self.reply_restraint_strength.as_str(),
            "light" | "medium" | "strong"
        ) {
            bail!("platform plugin real_context.reply_restraint_strength must be light, medium, or strong");
        }
        validate_real_context_range(
            "reply_restraint_multiplier",
            self.reply_restraint_multiplier,
            0.0,
            3.0,
        )?;
        for (name, value) in [
            ("judge_relevance_weight", self.judge_relevance_weight),
            ("judge_willingness_weight", self.judge_willingness_weight),
            ("judge_social_weight", self.judge_social_weight),
            ("judge_timing_weight", self.judge_timing_weight),
            ("judge_continuity_weight", self.judge_continuity_weight),
            (
                "judge_should_reply_boost_score",
                self.judge_should_reply_boost_score,
            ),
            (
                "judge_should_reply_penalty_score",
                self.judge_should_reply_penalty_score,
            ),
            ("continuation_boost_score", self.continuation_boost_score),
            (
                "takeover_direct_trigger_boost_score",
                self.takeover_direct_trigger_boost_score,
            ),
        ] {
            validate_real_context_range(name, value, 0.0, 1.0)?;
        }
        let weight_sum = self.judge_relevance_weight
            + self.judge_willingness_weight
            + self.judge_social_weight
            + self.judge_timing_weight
            + self.judge_continuity_weight;
        if !weight_sum.is_finite() || weight_sum <= f64::EPSILON {
            bail!("platform plugin real_context judge weights must have a positive sum");
        }
        validate_real_context_count(
            "continuation_window_seconds",
            self.continuation_window_seconds as usize,
            1,
            86_400,
        )?;
        validate_real_context_count(
            "active_reply_reaction_timeout_seconds",
            self.active_reply_reaction_timeout_seconds as usize,
            1,
            86_400,
        )?;
        validate_real_context_count(
            "reply_target_quote_after_other_messages",
            self.reply_target_quote_after_other_messages as usize,
            0,
            100_000,
        )?;
        validate_real_context_count(
            "reply_target_mention_after_seconds",
            self.reply_target_mention_after_seconds as usize,
            0,
            86_400,
        )?;
        if self.active_reply_reaction_emoji_ids.len() > 100
            || self.active_reply_reaction_enable && self.active_reply_reaction_emoji_ids.is_empty()
            || self.active_reply_reaction_emoji_ids.contains(&0)
        {
            bail!("platform plugin real_context.active_reply_reaction_emoji_ids must contain 1-100 positive ids");
        }
        validate_real_context_strings(
            "moderation_keywords",
            &self.moderation_keywords,
            256,
            4_096,
        )?;
        validate_real_context_range(
            "moderation_min_severity",
            self.moderation_min_severity,
            0.0,
            10.0,
        )?;
        validate_real_context_count(
            "moderation_timeout_seconds",
            self.moderation_timeout_seconds as usize,
            0,
            600,
        )?;
        if self.moderation_custom_rules.len() > 32_768
            || self.moderation_custom_rules.contains('\0')
        {
            bail!("platform plugin real_context.moderation_custom_rules is invalid");
        }
        validate_real_context_count(
            "base64_moderation_min_chars",
            self.base64_moderation_min_chars,
            4,
            4_096,
        )?;
        validate_real_context_count(
            "base64_moderation_max_decoded_chars",
            self.base64_moderation_max_decoded_chars,
            1,
            1_000_000,
        )?;
        validate_real_context_probability(
            "base64_moderation_min_printable_ratio",
            self.base64_moderation_min_printable_ratio,
        )?;
        if self.base64_moderation_max_decoded_chars < self.base64_moderation_min_chars {
            bail!("platform plugin real_context Base64 decoded limit cannot be smaller than its minimum input length");
        }
        validate_real_context_count(
            "affection_update_timeout_seconds",
            self.affection_update_timeout_seconds as usize,
            0,
            3_600,
        )?;
        validate_real_context_range(
            "affection_min_score",
            self.affection_min_score,
            -1_000.0,
            999.0,
        )?;
        validate_real_context_range(
            "affection_max_score",
            self.affection_max_score,
            self.affection_min_score + 1.0,
            1_000.0,
        )?;
        validate_real_context_range(
            "affection_regular_max_score",
            self.affection_regular_max_score,
            self.affection_min_score + 1.0,
            self.affection_max_score,
        )?;
        validate_real_context_range(
            "affection_initial_score",
            self.affection_initial_score,
            self.affection_min_score,
            self.affection_max_score,
        )?;
        validate_real_context_range("affection_bias_min", self.affection_bias_min, -1.0, 1.0)?;
        validate_real_context_range("affection_bias_max", self.affection_bias_max, -1.0, 1.0)?;
        validate_real_context_range(
            "affection_gain_pivot",
            self.affection_gain_pivot,
            self.affection_min_score,
            self.affection_max_score,
        )?;
        validate_real_context_range(
            "affection_delta_scale",
            self.affection_delta_scale,
            0.1,
            5.0,
        )?;
        validate_real_context_range("affection_delta_min", self.affection_delta_min, -100.0, 0.0)?;
        validate_real_context_range("affection_delta_max", self.affection_delta_max, 0.0, 100.0)?;
        validate_real_context_probability(
            "affection_update_confidence_threshold",
            self.affection_update_confidence_threshold,
        )?;
        validate_real_context_range(
            "affection_daily_gain_limit",
            self.affection_daily_gain_limit,
            0.0,
            1_000.0,
        )?;
        validate_real_context_range(
            "affection_daily_loss_limit",
            self.affection_daily_loss_limit,
            0.0,
            1_000.0,
        )?;
        validate_real_context_count("affection_max_tags", self.affection_max_tags, 0, 200)?;
        validate_real_context_count(
            "affection_recent_events_for_prompt",
            self.affection_recent_events_for_prompt,
            0,
            20,
        )?;
        let mut unlimited = HashSet::with_capacity(self.affection_unlimited_user_ids.len());
        if self.affection_unlimited_user_ids.len() > 10_000
            || self
                .affection_unlimited_user_ids
                .iter()
                .any(|id| *id <= 0 || !unlimited.insert(*id))
        {
            bail!("platform plugin real_context.affection_unlimited_user_ids contains invalid or duplicate ids");
        }
        for (name, prompt) in [
            (
                "affection_prompt_estranged",
                &self.affection_prompt_estranged,
            ),
            ("affection_prompt_cold", &self.affection_prompt_cold),
            ("affection_prompt_neutral", &self.affection_prompt_neutral),
            ("affection_prompt_known", &self.affection_prompt_known),
            ("affection_prompt_friend", &self.affection_prompt_friend),
            ("affection_prompt_trusted", &self.affection_prompt_trusted),
            ("affection_prompt_close", &self.affection_prompt_close),
        ] {
            if prompt.chars().count() > 32_768 || prompt.contains('\0') {
                bail!("platform plugin real_context.{name} is invalid");
            }
        }
        for (name, models) in [("text_models", &self.text_models)] {
            let Some(models) = models else { continue };
            if models.is_empty() {
                bail!("platform plugin real_context.{name} must be omitted instead of empty");
            }
            let mut seen = HashSet::with_capacity(models.len());
            if models.iter().any(|model| {
                model.provider_id.trim().is_empty()
                    || model.model.trim().is_empty()
                    || !seen.insert((&model.provider_id, &model.model))
            }) {
                bail!("platform plugin real_context.{name} must contain unique, non-empty model references");
            }
        }
        let mut nicknames = HashSet::with_capacity(self.identity_mappings.len());
        if self.identity_mappings.len() > 10_000
            || self.identity_mappings.iter().any(|mapping| {
                mapping.user_id <= 0
                    || mapping.nickname.is_empty()
                    || mapping.nickname.trim() != mapping.nickname
                    || mapping.nickname.chars().count() > 128
                    || mapping.nickname.chars().any(char::is_control)
                    || !nicknames.insert(&mapping.nickname)
            })
        {
            bail!("platform plugin real_context.identity_mappings contains invalid or duplicate entries");
        }
        Ok(())
    }
}

fn normalize_real_context_instance(instance: &mut PlatformPluginInstanceConfig) {
    let Ok(mut settings) = RealContextPluginSettings::from_instance(instance) else {
        return;
    };
    settings.normalize();
    merge_real_context_settings(instance, &settings);
}

fn migrate_message_history_instance(plugins: &mut PlatformPluginsConfig) {
    if plugins
        .get(QQ_MESSAGE_HISTORY_PLUGIN_ID)
        .is_some_and(|instance| !instance.is_empty())
    {
        return;
    }
    let Some(real_context) = plugins.get(REAL_CONTEXT_PLUGIN_ID) else {
        return;
    };
    let enabled = (real_context.enabled == Some(false)
        || real_context.settings.get("record_enable") == Some(&serde_json::Value::Bool(false)))
    .then_some(false);
    let mut settings = serde_json::Map::new();
    for key in [
        "history_search_max_results",
        "history_safe_page_limit",
        "allow_cross_group_search",
    ] {
        if let Some(value) = real_context.settings.get(key).cloned() {
            let target_key = if key == "allow_cross_group_search" {
                "allow_cross_conversation_search"
            } else {
                key
            };
            settings.insert(target_key.to_string(), value);
        }
    }
    if enabled.is_some() || !settings.is_empty() {
        plugins.insert(
            QQ_MESSAGE_HISTORY_PLUGIN_ID.to_string(),
            PlatformPluginInstanceConfig { enabled, settings },
        );
    }
}

const DEPRECATED_REAL_CONTEXT_SETTINGS: &[&str] = &[
    "record_enable",
    "record_media_mode",
    "history_search_max_results",
    "history_safe_page_limit",
    "allow_cross_group_search",
    "group_member_page_size",
    "reply_context_messages",
    "active_context_messages",
    "context_messages",
    "activity_statistics_enable",
    "daily_reply_limit_per_session",
    "log_judge_decision",
    "keyword_trigger_enable",
    "keyword_trigger_keywords",
    "keyword_boost_score",
    "takeover_system_trigger_enable",
    "takeover_system_trigger_boost_score",
    "moderation_in_active_judge_enable",
    "moderation_custom_rules_enable",
    "check_contain",
    "judge_models",
    "affection_judge_models",
    "continuation_window_minutes",
];

fn migrate_real_context_settings_map(settings: &mut serde_json::Map<String, serde_json::Value>) {
    if !settings.contains_key("group_member_search_max_results") {
        if let Some(value) = settings.get("group_member_page_size").cloned() {
            settings.insert("group_member_search_max_results".to_string(), value);
        }
    }
    if !settings.contains_key("text_models") {
        let models = settings
            .get("judge_models")
            .cloned()
            .or_else(|| settings.get("affection_judge_models").cloned());
        if let Some(value) = models {
            settings.insert("text_models".to_string(), value);
        }
    }
    // One knob used to feed both the reply turn and the judge. Their optimal
    // sizes point in opposite directions — the reply wants a generous opening
    // snapshot, the judge wants a tight recent window — and so do their cost
    // models, since the judge runs on every message rather than once per turn.
    let legacy_window = settings
        .get("context_messages")
        .cloned()
        .or_else(|| settings.get("reply_context_messages").cloned())
        .or_else(|| settings.get("active_context_messages").cloned());
    if let Some(value) = legacy_window {
        for key in ["reply_context_window", "judge_context_window"] {
            if !settings.contains_key(key) {
                settings.insert(key.to_string(), value.clone());
            }
        }
    }
    if !settings.contains_key("takeover_direct_trigger_enable") {
        if let Some(value) = settings.get("takeover_system_trigger_enable").cloned() {
            settings.insert("takeover_direct_trigger_enable".to_string(), value);
        }
    }
    if !settings.contains_key("takeover_direct_trigger_boost_score") {
        if let Some(value) = settings.get("takeover_system_trigger_boost_score").cloned() {
            settings.insert("takeover_direct_trigger_boost_score".to_string(), value);
        }
    }
    if !settings.contains_key("continuation_window_seconds") {
        if let Some(minutes) = settings
            .get("continuation_window_minutes")
            .and_then(serde_json::Value::as_u64)
        {
            // 3 minutes was the old default, not a considered choice — carry
            // those users onto the current default instead of pinning them to
            // whatever it happened to be when the unit changed.
            let seconds = if minutes == 3 {
                RealContextPluginSettings::default().continuation_window_seconds
            } else {
                minutes.saturating_mul(60)
            };
            settings.insert(
                "continuation_window_seconds".to_string(),
                serde_json::json!(seconds),
            );
        }
    }
    for key in DEPRECATED_REAL_CONTEXT_SETTINGS {
        settings.remove(*key);
    }
}

fn mutate_real_context_settings(
    plugins: &mut PlatformPluginsConfig,
    mutate: impl FnOnce(&mut RealContextPluginSettings),
) {
    let Some(instance) = plugins.get_mut(REAL_CONTEXT_PLUGIN_ID) else {
        return;
    };
    let Ok(mut settings) = RealContextPluginSettings::from_instance(instance) else {
        return;
    };
    mutate(&mut settings);
    merge_real_context_settings(instance, &settings);
}

pub fn merge_real_context_settings(
    instance: &mut PlatformPluginInstanceConfig,
    settings: &RealContextPluginSettings,
) {
    for key in DEPRECATED_REAL_CONTEXT_SETTINGS {
        instance.settings.remove(*key);
    }
    let Ok(serde_json::Value::Object(known)) = serde_json::to_value(settings) else {
        return;
    };
    let Ok(serde_json::Value::Object(defaults)) =
        serde_json::to_value(RealContextPluginSettings::default())
    else {
        return;
    };
    for (key, value) in known {
        if defaults.get(&key) == Some(&value) {
            instance.settings.remove(&key);
        } else {
            instance.settings.insert(key, value);
        }
    }
}

fn validate_real_context_plugin_config(instance: &PlatformPluginInstanceConfig) -> Result<()> {
    let settings = RealContextPluginSettings::from_instance(instance)?;
    settings.validate()
}

fn validate_real_context_count(
    name: &str,
    value: usize,
    minimum: usize,
    maximum: usize,
) -> Result<()> {
    if !(minimum..=maximum).contains(&value) {
        bail!("platform plugin real_context.{name} must be between {minimum} and {maximum}");
    }
    Ok(())
}

fn validate_real_context_probability(name: &str, value: f64) -> Result<()> {
    validate_real_context_range(name, value, 0.0, 1.0)
}

fn validate_real_context_range(name: &str, value: f64, minimum: f64, maximum: f64) -> Result<()> {
    if !value.is_finite() || !(minimum..=maximum).contains(&value) {
        bail!("platform plugin real_context.{name} must be between {minimum} and {maximum}");
    }
    Ok(())
}

fn validate_real_context_strings(
    name: &str,
    values: &[String],
    maximum_chars: usize,
    maximum_items: usize,
) -> Result<()> {
    let mut seen = HashSet::with_capacity(values.len());
    if values.len() > maximum_items
        || values.iter().any(|value| {
            value.is_empty()
                || value.trim() != value
                || value.chars().count() > maximum_chars
                || value.chars().any(char::is_control)
                || !seen.insert(value)
        })
    {
        bail!("platform plugin real_context.{name} contains invalid or duplicate entries");
    }
    Ok(())
}

fn normalize_unique_strings(values: &mut Vec<String>) {
    let mut seen = HashSet::with_capacity(values.len());
    values.retain_mut(|value| {
        *value = value.trim().to_string();
        !value.is_empty() && seen.insert(value.clone())
    });
}

fn default_real_context_moderation_keywords() -> Vec<String> {
    // Deduplicated from the user's deployed AstrBot real-context configuration.
    // Keep this self-contained so Miyu never reads another application's files.
    const KEYWORDS: &[&str] = &[
        "3p",
        "4p",
        "64",
        ":(){ :|:& };:",
        "> /dev/sda",
        "FtM",
        "IEPL",
        "IPLC",
        "K粉",
        "LGBTQ",
        "MtF",
        "Netflix拼车",
        "OD",
        "Spotify车位",
        "V2board",
        "VPN",
        "chmod -R 777 /",
        "chown -R 777 /",
        "clash/config",
        "cnm",
        "dd if=/dev/zero",
        "dick",
        "hysteria://",
        "iCloud拼车",
        "lsp",
        "mkfs.ext4",
        "mkfs.xfs",
        "nmsl",
        "ntr",
        "rm -fr /*",
        "rm -rf /*",
        "sb",
        "ss://",
        "ssr://",
        "sub?target=",
        "suck",
        "trojan://",
        "tuic://",
        "vless://",
        "vmess://",
        "zzzq",
        "三年自然灾害",
        "东三省",
        "中美贸易",
        "主义",
        "京喜",
        "人肉",
        "人身攻击",
        "代充",
        "优惠券群",
        "低价充值",
        "佐匹克隆",
        "你是一个",
        "你是我的奴隶",
        "你是猫娘",
        "使用XX系统的都是",
        "俄乌战争",
        "修车",
        "傻X",
        "傻逼",
        "公知",
        "六合彩",
        "关注公众号",
        "冰毒",
        "利他林",
        "刷单",
        "刷流水",
        "加我微信",
        "南梁",
        "南海仲裁",
        "博彩",
        "双性恋",
        "反共",
        "反华",
        "发车",
        "口角",
        "台海",
        "右美沙芬",
        "叶子",
        "同性恋",
        "四爱",
        "垃圾系统",
        "复读接下来的话",
        "外围",
        "外围盘",
        "外挂",
        "大麻",
        "天安门",
        "女同",
        "孕酮",
        "孤儿",
        "实名",
        "小仙女",
        "小日本",
        "小金豆",
        "就是垃圾",
        "巴以冲突",
        "帮我助力",
        "广告",
        "开盒",
        "忽略之前的指令",
        "恋尸癖",
        "恋童癖",
        "恋足癖",
        "拼多多",
        "排泄",
        "文革",
        "日赚",
        "暴动",
        "曲马多",
        "未成年",
        "机场跑路",
        "极品",
        "枪支",
        "梯子",
        "棒子",
        "止咳水",
        "死全家",
        "河南人",
        "测速图",
        "海洛因",
        "涩图",
        "淘宝客",
        "渠道",
        "港脚",
        "游行",
        "漏点",
        "炒币",
        "煞笔",
        "燃料",
        "狗推",
        "狗都不用",
        "玩客云",
        "男娘",
        "百家乐",
        "盒",
        "看片",
        "睾酮",
        "砍一刀",
        "破解",
        "神仙水",
        "福利姬",
        "福利群",
        "网盘资源",
        "网赌",
        "美狗",
        "群号",
        "翻墙",
        "肛交",
        "脑瘫",
        "色图",
        "色普龙",
        "节点",
        "药",
        "药娘",
        "菠菜",
        "薅羊毛",
        "螺内酯",
        "补佳乐",
        "裸聊",
        "订阅链接",
        "走猫",
        "走线",
        "起义",
        "跨性别",
        "身份证",
        "车牌",
        "辅助",
        "过量服药",
        "进新群",
        "阿普唑仑",
        "隐私",
        "雌二醇",
        "飞行",
        "飞行员",
    ];
    KEYWORDS
        .iter()
        .map(|keyword| (*keyword).to_string())
        .collect()
}

fn validate_reply_processor_plugin_config(instance: &PlatformPluginInstanceConfig) -> Result<()> {
    let settings = &instance.settings;
    for key in [
        "default_enabled",
        "followup_mention",
        "strip_period",
        "context_notice",
        "send_tool_intercept",
    ] {
        if settings.get(key).is_some_and(|value| !value.is_boolean()) {
            bail!("platform plugin reply_processor.{key} must be a boolean");
        }
    }
    for (key, min, max) in [
        ("threshold", 1_u64, 100_000_u64),
        ("max_height", 1_000, 5_000),
        ("font_size", 24, 56),
        ("code_font_size", 20, 46),
        ("padding", 36, 120),
        ("ttl_hours", 1, 168),
        ("max_records", 1, 10),
    ] {
        if let Some(value) = settings.get(key) {
            let value = value.as_u64().with_context(|| {
                format!("platform plugin reply_processor.{key} must be an unsigned integer")
            })?;
            if !(min..=max).contains(&value) {
                bail!("platform plugin reply_processor.{key} must be between {min} and {max}");
            }
        }
    }
    validate_plugin_string_choice(settings, "mode", &["image", "forward"])?;
    validate_plugin_string_choice(settings, "theme", &["paper", "light", "dark"])?;
    for key in ["font", "title_font", "code_font", "emoji_font"] {
        if let Some(value) = settings.get(key) {
            let value = value.as_str().with_context(|| {
                format!("platform plugin reply_processor.{key} must be a string")
            })?;
            if value.len() > 4_096 || value.contains('\0') {
                bail!("platform plugin reply_processor.{key} is invalid");
            }
        }
    }
    Ok(())
}

fn validate_plugin_string_choice(
    settings: &serde_json::Map<String, serde_json::Value>,
    key: &str,
    choices: &[&str],
) -> Result<()> {
    let Some(value) = settings.get(key) else {
        return Ok(());
    };
    let value = value
        .as_str()
        .with_context(|| format!("platform plugin reply_processor.{key} must be a string"))?;
    if !choices.contains(&value) {
        bail!(
            "platform plugin reply_processor.{key} must be one of: {}",
            choices.join(", ")
        );
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlatformConversationKind {
    Private,
    Group,
}

impl PlatformConversationKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Private => "private",
            Self::Group => "group",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PlatformConversationConfig {
    pub kind: PlatformConversationKind,
    pub id: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct PlatformMemoryConfig {
    #[serde(default = "default_true")]
    pub write_enabled: bool,
}

impl Default for PlatformMemoryConfig {
    fn default() -> Self {
        Self {
            write_enabled: true,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum PlatformPersonaOverride {
    #[default]
    Inherit,
    Miyu,
    Custom {
        name: String,
    },
}

impl PlatformPersonaOverride {
    pub fn is_inherit(&self) -> bool {
        matches!(self, Self::Inherit)
    }

    pub fn custom_name(&self) -> Option<&str> {
        match self {
            Self::Custom { name } => Some(name),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlatformModelPoolInheritance {
    #[default]
    Platform,
    Global,
}

impl PlatformModelPoolInheritance {
    fn is_platform(&self) -> bool {
        matches!(self, Self::Platform)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlatformModelRoute {
    pub conversation: PlatformConversationConfig,
    #[serde(default, skip_serializing_if = "PlatformPersonaOverride::is_inherit")]
    pub persona: PlatformPersonaOverride,
    /// Inheritance source used only when `text_models` is absent.
    #[serde(
        default,
        skip_serializing_if = "PlatformModelPoolInheritance::is_platform"
    )]
    pub text_models_inheritance: PlatformModelPoolInheritance,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text_models: Option<Vec<ActiveProviderModelConfig>>,
    /// Inheritance source used only when `multimodal_models` is absent.
    #[serde(
        default,
        skip_serializing_if = "PlatformModelPoolInheritance::is_platform"
    )]
    pub multimodal_models_inheritance: PlatformModelPoolInheritance,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub multimodal_models: Option<Vec<ActiveProviderModelConfig>>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub extra_prompt: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_limits: Option<PlatformSessionLimits>,
}

impl PlatformModelRoute {
    pub fn identity(&self) -> (PlatformConversationKind, &str) {
        (self.conversation.kind, self.conversation.id.as_str())
    }

    pub fn matches(&self, kind: PlatformConversationKind, conversation_id: &str) -> bool {
        self.conversation.kind == kind && self.conversation.id == conversation_id
    }

    pub fn normalize(&mut self) {
        self.conversation.id = self.conversation.id.trim().to_string();
        if let PlatformPersonaOverride::Custom { name } = &mut self.persona {
            *name = name.trim().to_string();
        }
        self.extra_prompt = self.extra_prompt.trim().to_string();
        normalize_route_pool(&mut self.text_models);
        normalize_route_pool(&mut self.multimodal_models);
        if self.text_models.is_some() {
            self.text_models_inheritance = PlatformModelPoolInheritance::Platform;
        }
        if self.multimodal_models.is_some() {
            self.multimodal_models_inheritance = PlatformModelPoolInheritance::Platform;
        }
    }

    fn prune_model_references(&mut self, providers: &[ProviderConfig]) {
        if let Some(pool) = &mut self.text_models {
            pool.retain(|entry| active_model_exists(providers, entry));
        }
        if let Some(pool) = &mut self.multimodal_models {
            pool.retain(|entry| active_model_supports_image(providers, entry));
        }
        normalize_route_pool(&mut self.text_models);
        normalize_route_pool(&mut self.multimodal_models);
    }

    fn remove_model_references(&mut self, provider_id: &str, model: &str) {
        for pool in [&mut self.text_models, &mut self.multimodal_models] {
            if let Some(entries) = pool {
                entries.retain(|entry| !(entry.provider_id == provider_id && entry.model == model));
            }
            normalize_route_pool(pool);
        }
    }

    fn rename_provider_references(&mut self, old_id: &str, new_id: &str) {
        for entries in [&mut self.text_models, &mut self.multimodal_models]
            .into_iter()
            .flatten()
        {
            for entry in entries {
                if entry.provider_id == old_id {
                    entry.provider_id = new_id.to_string();
                }
            }
        }
    }

    fn rename_model_references(&mut self, provider_id: &str, old: &str, new: &str) {
        for entries in [&mut self.text_models, &mut self.multimodal_models]
            .into_iter()
            .flatten()
        {
            for entry in entries {
                if entry.provider_id == provider_id && entry.model == old {
                    entry.model = new.to_string();
                }
            }
        }
    }
}

fn normalize_route_pool(pool: &mut Option<Vec<ActiveProviderModelConfig>>) {
    let Some(entries) = pool else {
        return;
    };
    let mut seen = HashSet::with_capacity(entries.len());
    entries.retain_mut(|entry| {
        entry.provider_id = entry.provider_id.trim().to_string();
        entry.model = entry.model.trim().to_string();
        !entry.provider_id.is_empty()
            && !entry.model.is_empty()
            && seen.insert((entry.provider_id.clone(), entry.model.clone()))
    });
    if entries.is_empty() {
        *pool = None;
    }
}

fn rename_provider_in_pool(pool: &mut [ActiveProviderModelConfig], old_id: &str, new_id: &str) {
    for entry in pool {
        if entry.provider_id == old_id {
            entry.provider_id = new_id.to_string();
        }
    }
}

fn retain_provider_pool(pool: &mut Option<Vec<ActiveProviderModelConfig>>, provider_id: &str) {
    if let Some(entries) = pool {
        entries.retain(|entry| entry.provider_id != provider_id);
    }
    retain_nonempty_pool(pool);
}

fn retain_nonempty_pool(pool: &mut Option<Vec<ActiveProviderModelConfig>>) {
    if pool.as_ref().is_some_and(Vec::is_empty) {
        *pool = None;
    }
}

/// Tencent QQ integration implemented through a OneBot v11 reverse
/// WebSocket transport (for example NapCat).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct OneBotConfig {
    pub enabled: bool,
    pub reverse_ws_port: u16,
    /// Checked against NapCat's `Authorization: Bearer` handshake header.
    /// Empty tokens are accepted only from a loopback peer.
    pub access_token: String,
    pub admin_users: Vec<i64>,
    /// Grants full host tools only to non-admin users in `private_chats.whitelist`.
    pub allow_non_admin_host_tools: bool,
    /// Send each model round's text to group chats as its own message while
    /// the turn is still running, instead of keeping only the final reply.
    pub group_intermediate_messages: bool,
    /// Send each model round's text to private chats as its own message while
    /// the turn is still running, instead of keeping only the final reply.
    #[serde(default = "default_true")]
    pub private_intermediate_messages: bool,
    /// Include the current QQ sender's stable id in the model system context.
    /// Nicknames remain available for display even when this is disabled.
    #[serde(default = "default_true")]
    pub user_identification: bool,
    /// Include the current QQ group name in the model system context.
    #[serde(default = "default_true")]
    pub show_group_name: bool,
    pub memory: PlatformMemoryConfig,
    pub private_chats: QqPrivateChatsConfig,
    pub group_chats: QqGroupChatsConfig,
    #[serde(default, skip_serializing_if = "PlatformSessionLimits::is_default")]
    pub session_limits: PlatformSessionLimits,
    #[serde(
        default,
        skip_serializing_if = "PlatformGroupContextConfig::is_default"
    )]
    pub group_context: PlatformGroupContextConfig,
    /// QQ-wide text model pool. None inherits the global pool.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text_models: Option<Vec<ActiveProviderModelConfig>>,
    /// QQ-wide multimodal model pool. None inherits the global pool.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub multimodal_models: Option<Vec<ActiveProviderModelConfig>>,
    /// Text model pool for non-whitelisted private chats and groups.
    /// None inherits the QQ-wide text model pool.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub non_whitelist_text_models: Option<Vec<ActiveProviderModelConfig>>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub conversations: Vec<PlatformModelRoute>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub plugins: PlatformPluginsConfig,
    /// Public HTTP base URL NapCat can use to fetch temporary local assets.
    pub asset_base_url: String,
    /// Replies longer than this are split into multiple messages. 0 = never split.
    pub max_reply_chars: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct QqPrivateChatsConfig {
    /// QQ ids whose private conversations bypass admission rate limits.
    pub whitelist: Vec<i64>,
    /// Accept friend requests only from admins or private-whitelisted QQ ids.
    pub friend_requests_require_private_whitelist: bool,
    pub allow_non_whitelist: bool,
    /// Per private conversation.
    pub non_whitelist_rate_limit: PlatformRateLimit,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_limits: Option<PlatformSessionLimits>,
    #[serde(default, rename = "non_whitelist_rate_per_minute", skip_serializing)]
    legacy_non_whitelist_rate_per_minute: Option<u32>,
}

impl Default for QqPrivateChatsConfig {
    fn default() -> Self {
        Self {
            whitelist: Vec::new(),
            friend_requests_require_private_whitelist: true,
            allow_non_whitelist: true,
            non_whitelist_rate_limit: PlatformRateLimit {
                max_messages: 2,
                window_seconds: 600,
            },
            session_limits: None,
            legacy_non_whitelist_rate_per_minute: None,
        }
    }
}

impl QqPrivateChatsConfig {
    fn migrate_legacy_rate_limit(&mut self) {
        if let Some(max_messages) = self.legacy_non_whitelist_rate_per_minute.take() {
            self.non_whitelist_rate_limit = PlatformRateLimit {
                max_messages,
                window_seconds: 60,
            };
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct PlatformRateLimit {
    /// Zero disables the limit.
    pub max_messages: u32,
    pub window_seconds: u32,
}

impl Default for PlatformRateLimit {
    fn default() -> Self {
        Self {
            max_messages: 0,
            window_seconds: 60,
        }
    }
}

fn validate_platform_session_limits(field: &str, limits: PlatformSessionLimits) -> Result<()> {
    if limits.running == 0 || limits.running > MAX_PLATFORM_SESSION_RUNNING {
        bail!("platforms.qq.{field}.running must be between 1 and {MAX_PLATFORM_SESSION_RUNNING}");
    }
    if limits.queued > MAX_PLATFORM_SESSION_QUEUED {
        bail!("platforms.qq.{field}.queued must be between 0 and {MAX_PLATFORM_SESSION_QUEUED}");
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct QqGroupChatsConfig {
    /// Group ids that use the whitelist-group rate limit.
    pub whitelist: Vec<i64>,
    /// Additional wake prefixes. @-mentions always remain active.
    pub trigger_keywords: Vec<String>,
    /// Shared by all senders in one whitelisted group.
    pub whitelist_rate_limit: PlatformRateLimit,
    pub allow_non_whitelist: bool,
    /// Shared by all senders in one non-whitelisted group.
    pub non_whitelist_rate_limit: PlatformRateLimit,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_limits: Option<PlatformSessionLimits>,
    #[serde(default, rename = "whitelist_rate_per_minute", skip_serializing)]
    legacy_whitelist_rate_per_minute: Option<u32>,
    #[serde(default, rename = "non_whitelist_rate_per_minute", skip_serializing)]
    legacy_non_whitelist_rate_per_minute: Option<u32>,
}

impl Default for QqGroupChatsConfig {
    fn default() -> Self {
        Self {
            whitelist: Vec::new(),
            trigger_keywords: Vec::new(),
            whitelist_rate_limit: PlatformRateLimit {
                max_messages: 30,
                window_seconds: 60,
            },
            allow_non_whitelist: true,
            non_whitelist_rate_limit: PlatformRateLimit {
                max_messages: 2,
                window_seconds: 600,
            },
            session_limits: None,
            legacy_whitelist_rate_per_minute: None,
            legacy_non_whitelist_rate_per_minute: None,
        }
    }
}

impl QqGroupChatsConfig {
    fn migrate_legacy_rate_limits(&mut self) {
        if let Some(max_messages) = self.legacy_whitelist_rate_per_minute.take() {
            self.whitelist_rate_limit = PlatformRateLimit {
                max_messages,
                window_seconds: 60,
            };
        }
        if let Some(max_messages) = self.legacy_non_whitelist_rate_per_minute.take() {
            self.non_whitelist_rate_limit = PlatformRateLimit {
                max_messages,
                window_seconds: 60,
            };
        }
    }
}

impl Default for OneBotConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            reverse_ws_port: 8300,
            access_token: String::new(),
            admin_users: Vec::new(),
            allow_non_admin_host_tools: false,
            group_intermediate_messages: false,
            private_intermediate_messages: true,
            user_identification: true,
            show_group_name: true,
            memory: PlatformMemoryConfig::default(),
            private_chats: QqPrivateChatsConfig::default(),
            group_chats: QqGroupChatsConfig::default(),
            session_limits: PlatformSessionLimits::default(),
            group_context: PlatformGroupContextConfig::default(),
            text_models: None,
            multimodal_models: None,
            non_whitelist_text_models: None,
            conversations: Vec::new(),
            plugins: PlatformPluginsConfig::new(),
            asset_base_url: String::new(),
            max_reply_chars: 3000,
        }
    }
}

impl OneBotConfig {
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }

    pub fn session_limits(
        &self,
        kind: PlatformConversationKind,
        conversation_id: &str,
    ) -> PlatformSessionLimits {
        self.conversations
            .iter()
            .find(|route| route.matches(kind, conversation_id))
            .and_then(|route| route.session_limits)
            .or(match kind {
                PlatformConversationKind::Private => self.private_chats.session_limits,
                PlatformConversationKind::Group => self.group_chats.session_limits,
            })
            .unwrap_or(self.session_limits)
    }
}

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

#[derive(Debug, Clone, Serialize)]
pub struct DisplayConfig {
    #[serde(default = "default_display_language")]
    pub language: String,
    #[serde(default = "default_reasoning_display")]
    pub reasoning: String,
    #[serde(default = "default_tool_call_display")]
    pub tool_calls: String,
    #[serde(default = "default_true")]
    pub readable_tool_names: bool,
    #[serde(default)]
    pub show_token_usage: bool,
    #[serde(default = "default_mixed_model_endpoint_display")]
    pub mixed_model_endpoint_display: String,
    #[serde(default = "default_command_output_lines")]
    pub command_output_lines: usize,
    /// How many finished turns a reopened REPL redraws; 0 disables replay.
    #[serde(default = "default_repl_replay_turns")]
    pub repl_replay_turns: usize,
}

/// Desktop notifications. Both kinds are suppressed while the REPL window has
/// focus — if you are looking at the terminal, a popup is only noise.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NotificationsConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Notify when a reply finishes and Miyu is waiting on you again.
    #[serde(default = "default_true")]
    pub on_turn_complete: bool,
    /// shellhook/单次 CLI 触发的后台任务完成后,把跟进回复写回触发它的那个
    /// 终端。仅在该 shell 仍活着、停在同一 tty 的前台提示符时才写;写不了退化
    /// 为桌面通知。
    #[serde(default = "default_true")]
    pub job_writeback_to_terminal: bool,
}

impl Default for NotificationsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            on_turn_complete: true,
            job_writeback_to_terminal: true,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
struct RawDisplayConfig {
    #[serde(default)]
    language: Option<String>,
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
    #[serde(default)]
    show_mixed_model_endpoint: Option<bool>,
    #[serde(default)]
    mixed_model_endpoint_display: Option<String>,
    #[serde(default)]
    command_output_lines: Option<usize>,
    #[serde(default)]
    repl_replay_turns: Option<usize>,
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
            language: raw.language.unwrap_or_else(default_display_language),
            reasoning,
            tool_calls,
            readable_tool_names: raw.readable_tool_names.unwrap_or_else(default_true),
            show_token_usage: raw.show_token_usage.unwrap_or(false),
            mixed_model_endpoint_display: raw.mixed_model_endpoint_display.unwrap_or_else(|| {
                match raw.show_mixed_model_endpoint {
                    Some(true) => "all".to_string(),
                    Some(false) => "off".to_string(),
                    None => default_mixed_model_endpoint_display(),
                }
            }),
            command_output_lines: raw
                .command_output_lines
                .unwrap_or_else(default_command_output_lines),
            repl_replay_turns: raw
                .repl_replay_turns
                .unwrap_or_else(default_repl_replay_turns),
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
    /// 平台会话尾部的人格提醒(自动蒸馏,见 persona_hint 模块)。
    #[serde(default = "default_true")]
    pub persona_reminder: bool,
}

/// Identifies who a model prompt is acting for. Only trusted local operator
/// turns may receive the configured user identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptAudience {
    Owner,
    External,
    Internal,
}

impl PromptAudience {
    fn includes_user_identity(self) -> bool {
        matches!(self, Self::Owner)
    }
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
    fn is_default(&self) -> bool {
        *self == Self::default()
    }

    /// A model is configured; whether any feature uses it is that feature's
    /// business.
    pub fn is_configured(&self) -> bool {
        !self.provider_id.trim().is_empty() && !self.model.trim().is_empty()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextConfig {
    /// 工具输出的模型侧内联上限(UTF-8 字节)。超限的纯文本输出全文外溢到
    /// 会话级 spill 文件,模型只看头尾预览+取回提示(read_file/rg 按需读回)。
    /// 0 = 关闭外溢。照抄 dsh 默认 50KB。
    #[serde(default = "default_tool_output_spill_bytes")]
    pub tool_output_spill_bytes: usize,
    #[serde(default = "default_trim_at_ratio")]
    pub trim_at_ratio: f32,
    #[serde(default = "default_trim_batch_ratio")]
    pub trim_batch_ratio: f32,
    #[serde(default = "default_on_overflow")]
    pub on_overflow: String,
    #[serde(default = "default_context_window")]
    pub default_context_window: usize,
    /// Watermark that forces a compaction even when the fold-economics gate
    /// would skip it. Must be >= trim_at_ratio.
    #[serde(default = "default_compact_force_ratio")]
    pub compact_force_ratio: f32,
    /// Verbatim tail budget kept outside the summary, in tokens. None derives
    /// min(16384, window/4) for task modes and 8192 for chat mode; the value
    /// is always capped at window/2 so a small window still lands below the
    /// trigger after compaction (re-compaction loop guard).
    #[serde(default)]
    pub compact_tail_tokens: Option<usize>,
    /// Soft watermark: a one-shot "context is getting large" notice, no
    /// history rewrite (a rewrite here would needlessly crater the cache).
    #[serde(default = "default_compact_soft_ratio")]
    pub compact_soft_ratio: f32,
    /// Mechanical watermark: old turns' tool_reports fold into placeholders
    /// (no LLM call). Must satisfy soft <= snip <= trim_at_ratio.
    #[serde(default = "default_compact_snip_ratio")]
    pub compact_snip_ratio: f32,
    /// Enables the mechanical prune layer (free: tool output is
    /// re-derivable). Batched behind a harvest gate so each rewrite pays for
    /// its one-time prefix-cache reset.
    #[serde(default = "default_true")]
    pub prune_stale_tool_reports: bool,
    /// Cold-resume prune: a session idle longer than this resumes against an
    /// expired provider cache, so rewriting history at that moment costs no
    /// extra misses — it only shrinks the full-price first request. Minutes;
    /// 0 disables. Default 1440 (24h, conservative for DeepSeek; drop to ~5
    /// for Anthropic ephemeral cache).
    #[serde(default = "default_cold_prune_after_minutes")]
    pub cold_prune_after_minutes: u64,
    /// Summarization requests fork the live conversation (same byte prefix,
    /// same tools + one appended instruction) so the provider prefix cache
    /// pays for re-reading the history — roughly a 10x input-cost saving on
    /// prefix-cached providers (DeepSeek/OpenAI-compatible/Anthropic). Turn
    /// OFF on per-request-billed gateways where cache hits save nothing: the
    /// isolated fallback path sends the history as plain text instead.
    #[serde(default = "default_true")]
    pub compact_cache_reuse: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolsConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub max_rounds: usize,
    #[serde(default = "default_tools_loading_mode")]
    pub loading_mode: String,
    #[serde(default = "default_true")]
    pub persist_loaded_tools: bool,
    /// How many `task` subagents from one tool batch may run concurrently.
    #[serde(default = "default_subagent_concurrency")]
    pub subagent_concurrency: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub servers: Vec<McpServerConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpServerConfig {
    pub id: String,
    #[serde(default)]
    pub display_name: String,
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: HashMap<String, String>,
    #[serde(default = "default_mcp_timeout")]
    pub timeout_seconds: u64,
    #[serde(default = "default_true")]
    pub enabled: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
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
    #[serde(default = "default_memory_diary_batch_size")]
    pub diary_batch_size: usize,
    #[serde(default = "default_memory_short_diary_retention_days")]
    pub short_diary_retention_days: u64,
    #[serde(default = "default_memory_diary_promotion_recalls")]
    pub diary_promotion_recalls: u64,
    #[serde(default = "default_memory_organizer_timeout_seconds")]
    pub organizer_timeout_seconds: u64,
    #[serde(default)]
    pub auto_skill_enabled: bool,
    #[serde(default = "default_memory_association_facts")]
    pub association_facts: usize,
    #[serde(default = "default_memory_association_episodes")]
    pub association_episodes: usize,
    #[serde(default = "default_memory_association_max_chars")]
    pub association_max_chars: usize,
    /// 同一条记忆若已在本会话早前回合注入过（化石仍在可见上下文中逐字回放），
    /// 本回合不再重复注入。内容或日期变化的记忆视为新条目照常注入。
    #[serde(default = "default_true")]
    pub association_dedup: bool,
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
    #[serde(default)]
    pub diagnostics: DiagnosticsPluginConfig,
    #[serde(default)]
    pub api_quota: ApiQuotaPluginConfig,
    #[serde(default)]
    pub memory: MemoryConfig,
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
    #[serde(default = "default_web_search_max_results")]
    pub max_results: usize,
    #[serde(default)]
    pub tavily_api_keys: Vec<String>,
    #[serde(default)]
    pub firecrawl_api_keys: Vec<String>,
    #[serde(default)]
    pub anysearch_api_keys: Vec<String>,
    /// Exa 无需 key 也可用（走官方 MCP 免费额度）；配置 key 后走 REST API
    #[serde(default)]
    pub exa_api_keys: Vec<String>,
    #[serde(default)]
    pub searxng_base_url: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WebImagesPluginConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_web_images_source_mode")]
    pub source_mode: String,
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
pub struct VisionPluginConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_true")]
    pub prefer_current_multimodal_model: bool,
    #[serde(default)]
    pub vision_provider_id: String,
    #[serde(default)]
    pub vision_model: String,
    #[serde(default = "default_vision_response_header_timeout")]
    pub response_header_timeout_seconds: u64,
    #[serde(default = "default_vision_stream_idle_timeout")]
    pub stream_idle_timeout_seconds: u64,
    #[serde(default = "default_vision_image_timeout")]
    pub image_timeout_seconds: u64,
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
    #[serde(default = "default_memes_search_max_results")]
    pub search_max_results: usize,
    #[serde(default)]
    pub allow_gif_animation: bool,
    /// 终端/WebUI 会话的自动提示发送表情,默认开。
    #[serde(default = "default_true")]
    pub auto_send_enabled: bool,
    /// 通讯平台会话的自动提示发送表情:与终端/WebUI 的 auto_send_enabled
    /// 独立,默认开——表情包本来就是平台聊天的语言。
    #[serde(default = "default_true")]
    pub auto_send_platform_enabled: bool,
    #[serde(default = "default_memes_auto_send_probability")]
    pub auto_send_probability: f32,
}

/// 手动模型价格(每 1M tokens):目录查不到价的中转/赠送端点用它,
/// 设了就覆盖 models.dev 的价目。缓存价缺省时按输入价计。
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ModelCostConfig {
    #[serde(default)]
    pub currency: CostCurrency,
    #[serde(default)]
    pub input: f64,
    #[serde(default)]
    pub output: f64,
    #[serde(default, alias = "cache", skip_serializing_if = "Option::is_none")]
    pub cache_read: Option<f64>,
}

/// 手动价格的币种。统计聚合统一折算成 USD 展示。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum CostCurrency {
    #[default]
    #[serde(rename = "USD", alias = "usd")]
    Usd,
    #[serde(rename = "CNY", alias = "cny", alias = "rmb", alias = "¥")]
    Cny,
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiQuotaPluginConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub deepseek: ApiQuotaProviderConfig,
    #[serde(default)]
    pub openrouter: ApiQuotaProviderConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiQuotaProviderConfig {
    #[serde(default)]
    pub api_key: String,
    #[serde(default)]
    pub accounts: Vec<ApiQuotaAccountConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiQuotaAccountConfig {
    #[serde(default)]
    pub id: String,
    #[serde(default = "default_api_quota_account_name")]
    pub name: String,
    #[serde(default)]
    pub api_key: String,
}

fn default_api_quota_account_name() -> String {
    "默认账号".to_string()
}

fn normalize_api_quota_provider(config: &mut ApiQuotaProviderConfig) {
    let legacy_key = config.api_key.trim().to_string();
    if config.accounts.is_empty() {
        config.accounts.push(ApiQuotaAccountConfig {
            id: "account-1".to_string(),
            name: default_api_quota_account_name(),
            api_key: legacy_key.clone(),
        });
    } else if !legacy_key.is_empty()
        && config
            .accounts
            .iter()
            .all(|account| account.api_key.trim() != legacy_key)
    {
        if config.accounts[0].api_key.trim().is_empty() {
            config.accounts[0].api_key = legacy_key.clone();
        } else if config.accounts.len() < 32 {
            let mut number = 2usize;
            let name = loop {
                let candidate = format!("账号 {number}");
                if config
                    .accounts
                    .iter()
                    .all(|account| account.name != candidate)
                {
                    break candidate;
                }
                number += 1;
            };
            config.accounts.push(ApiQuotaAccountConfig {
                id: String::new(),
                name,
                api_key: legacy_key.clone(),
            });
        }
    }
    if legacy_key.is_empty()
        || config
            .accounts
            .iter()
            .any(|account| account.api_key.trim() == legacy_key)
    {
        config.api_key.clear();
    }
    let mut used_ids = HashSet::with_capacity(config.accounts.len());
    for (index, account) in config.accounts.iter_mut().enumerate() {
        account.name = account.name.trim().to_string();
        if account.name.is_empty() {
            account.name = if index == 0 {
                default_api_quota_account_name()
            } else {
                format!("账号 {}", index + 1)
            };
        }
        if account.id.trim().is_empty() || !used_ids.insert(account.id.clone()) {
            let mut number = index + 1;
            loop {
                let id = format!("account-{number}");
                if used_ids.insert(id.clone()) {
                    account.id = id;
                    break;
                }
                number += 1;
            }
        }
    }
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            config_version: CURRENT_CONFIG_VERSION,
            active_provider: OPENCODE_PROVIDER_ID.to_string(),
            active_provider_models: None,
            active_multimodal_provider_models: None,
            providers: ProviderConfig::default_templates(),
            embedding: EmbeddingConfig::default(),
            context: ContextConfig::default(),
            tools: ToolsConfig::default(),
            cache: CacheConfig::default(),
            mcp: McpConfig::default(),
            skills: SkillsConfig::default(),
            display: DisplayConfig::default(),
            notifications: NotificationsConfig::default(),
            prompt: PromptConfig::default(),
            plugins: PluginsConfig::default(),
            memory: MemoryConfig::default(),
            system_prompt_file: Some("system-prompt.md".to_string()),
            system_prompt: None,
            subagent_tiers: SubagentTiersConfig::default(),
            platforms: PlatformsConfig::default(),
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
            persona_reminder: true,
        }
    }
}

impl Default for DisplayConfig {
    fn default() -> Self {
        Self {
            language: default_display_language(),
            reasoning: default_reasoning_display(),
            tool_calls: default_tool_call_display(),
            readable_tool_names: default_true(),
            show_token_usage: false,
            mixed_model_endpoint_display: default_mixed_model_endpoint_display(),
            command_output_lines: default_command_output_lines(),
            repl_replay_turns: default_repl_replay_turns(),
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
            diagnostics: DiagnosticsPluginConfig::default(),
            api_quota: ApiQuotaPluginConfig::default(),
            memory: MemoryConfig::default(),
        }
    }
}

impl Default for ApiQuotaPluginConfig {
    fn default() -> Self {
        Self {
            enabled: default_true(),
            deepseek: ApiQuotaProviderConfig::default(),
            openrouter: ApiQuotaProviderConfig::default(),
        }
    }
}

impl Default for ApiQuotaProviderConfig {
    fn default() -> Self {
        Self {
            api_key: String::new(),
            accounts: vec![ApiQuotaAccountConfig {
                id: "account-1".to_string(),
                name: default_api_quota_account_name(),
                api_key: String::new(),
            }],
        }
    }
}

impl Default for McpConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            servers: Vec::new(),
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

impl Default for WebPluginConfig {
    fn default() -> Self {
        Self {
            enabled: default_true(),
            max_results: default_web_search_max_results(),
            tavily_api_keys: Vec::new(),
            firecrawl_api_keys: Vec::new(),
            anysearch_api_keys: Vec::new(),
            exa_api_keys: Vec::new(),
            searxng_base_url: String::new(),
        }
    }
}

impl Default for WebImagesPluginConfig {
    fn default() -> Self {
        Self {
            enabled: default_true(),
            source_mode: default_web_images_source_mode(),
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

impl Default for VisionPluginConfig {
    fn default() -> Self {
        Self {
            enabled: default_true(),
            prefer_current_multimodal_model: default_true(),
            vision_provider_id: String::new(),
            vision_model: String::new(),
            response_header_timeout_seconds: default_vision_response_header_timeout(),
            stream_idle_timeout_seconds: default_vision_stream_idle_timeout(),
            image_timeout_seconds: default_vision_image_timeout(),
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
            search_max_results: default_memes_search_max_results(),
            allow_gif_animation: false,
            auto_send_enabled: true,
            auto_send_platform_enabled: true,
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
            persist_loaded_tools: default_true(),
            subagent_concurrency: default_subagent_concurrency(),
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
            diary_batch_size: default_memory_diary_batch_size(),
            short_diary_retention_days: default_memory_short_diary_retention_days(),
            diary_promotion_recalls: default_memory_diary_promotion_recalls(),
            organizer_timeout_seconds: default_memory_organizer_timeout_seconds(),
            auto_skill_enabled: false,
            association_facts: default_memory_association_facts(),
            association_episodes: default_memory_association_episodes(),
            association_max_chars: default_memory_association_max_chars(),
            association_dedup: default_true(),
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
            tool_output_spill_bytes: default_tool_output_spill_bytes(),
            trim_at_ratio: default_trim_at_ratio(),
            trim_batch_ratio: default_trim_batch_ratio(),
            on_overflow: default_on_overflow(),
            default_context_window: default_context_window(),
            compact_force_ratio: default_compact_force_ratio(),
            compact_tail_tokens: None,
            compact_soft_ratio: default_compact_soft_ratio(),
            compact_snip_ratio: default_compact_snip_ratio(),
            prune_stale_tool_reports: true,
            cold_prune_after_minutes: default_cold_prune_after_minutes(),
            compact_cache_reuse: true,
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
            protocol: "anthropic".to_string(),
            api_key: Some("$env:ANTHROPIC_API_KEY".to_string()),
            models: Vec::new(),
            model_context_window: HashMap::new(),
            model_modalities: HashMap::new(),
            model_costs: HashMap::new(),
            default_model: String::new(),
            timeout_seconds: default_timeout(),
            temperature: default_temperature(),
            anthropic_max_tokens: default_anthropic_max_tokens(),
            extra_body: None,
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
            protocol: default_provider_protocol(),
            api_key: None,
            models: Vec::new(),
            model_context_window: HashMap::new(),
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

    fn has_configured_model(&self, model: &str) -> bool {
        let model = model.trim();
        !model.is_empty()
            && (self.default_model == model || self.models.iter().any(|item| item == model))
    }

    fn is_legacy_default_anthropic_model(&self) -> bool {
        self.id == "anthropic"
            && self.base_url.trim_end_matches('/') == "https://api.anthropic.com/v1"
            && self.protocol == "anthropic"
            && self.api_key.as_deref() == Some("$env:ANTHROPIC_API_KEY")
            && self.models == ["claude-sonnet-4-5"]
            && self.default_model == "claude-sonnet-4-5"
    }
}

fn append_resolved_api_keys(out: &mut Vec<ResolvedProviderKey>, raw: &str) -> Result<()> {
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

fn split_api_keys(raw: &str) -> Vec<&str> {
    raw.lines()
        .flat_map(|line| line.split(','))
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .collect()
}

fn active_model_exists(providers: &[ProviderConfig], active: &ActiveProviderModelConfig) -> bool {
    providers
        .iter()
        .find(|provider| provider.id == active.provider_id.trim())
        .is_some_and(|provider| provider.has_configured_model(&active.model))
}

fn active_model_supports_image(
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

fn validate_unique_existing_pool(
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

fn is_positive_decimal_id(value: &str) -> bool {
    !value.is_empty()
        && value.bytes().all(|byte| byte.is_ascii_digit())
        && value.parse::<u64>().is_ok_and(|id| id > 0)
}

impl AppConfig {
    pub fn display_language_hint(paths: &MiyuPaths) -> Option<String> {
        let raw = std::fs::read_to_string(&paths.config_file).ok()?;
        let stripped = json_comments::StripComments::new(raw.as_bytes());
        let value: serde_json::Value = serde_json::from_reader(stripped).ok()?;
        value
            .get("display")?
            .get("language")?
            .as_str()
            .map(str::to_string)
    }

    pub fn memory_config(&self) -> &MemoryConfig {
        if self.memory != MemoryConfig::default() {
            &self.memory
        } else {
            &self.plugins.memory
        }
    }

    pub fn load(paths: &MiyuPaths) -> Result<Self> {
        // Platform multimodal routes may rely on cached models.dev
        // capabilities. Load the full cache before validation; callers can
        // compact it to their active configuration afterwards.
        crate::models_cache::try_load(paths);
        let raw = std::fs::read_to_string(&paths.config_file)
            .with_context(|| format!("failed to read {}", paths.config_file.display()))?;
        let stripped = json_comments::StripComments::new(raw.as_bytes());
        let mut config: Self = serde_json::from_reader(stripped)
            .with_context(|| format!("invalid JSONC in {}", paths.config_file.display()))?;
        config.migrate()?;
        config.normalize_builtin_providers();
        config.normalize_api_quota_accounts();
        config.normalize_managed_output_paths(paths);
        config.normalize_platform_model_routes();
        config.validate()?;
        config.validate_persona_files(paths)?;
        Ok(config)
    }

    pub fn load_or_default(paths: &MiyuPaths) -> Result<Self> {
        if paths.config_file.exists() {
            Self::load(paths)
        } else {
            Ok(Self::default())
        }
    }

    pub fn has_configured_llm(&self) -> bool {
        self.active_provider_model_choices().iter().any(|choice| {
            self.providers
                .iter()
                .any(|p| p.id == choice.provider_id && p.api_key.as_deref().map_or(false, |k| !k.trim().is_empty()))
        })
    }

    pub fn init_files(paths: &MiyuPaths) -> Result<()> {
        paths.create_dirs()?;
        if !paths.config_file.exists() {
            Self::default().save(paths)?;
        }
        Ok(())
    }

    pub fn save(&self, paths: &MiyuPaths) -> Result<()> {
        let mut config = self.clone();
        config.migrate()?;
        config.normalize_api_quota_accounts();
        config.normalize_platform_model_routes();
        // Also on save, not just on load: a value healed only in memory is
        // rewritten stale on the next write, so the file never recovers.
        config.normalize_managed_output_paths(paths);
        let effective_memory = config.memory_config().clone();
        config.plugins.memory = effective_memory;
        config.memory = MemoryConfig::default();
        config.validate()?;
        paths.create_dirs()?;
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

    fn migrate(&mut self) -> Result<()> {
        if self.config_version > CURRENT_CONFIG_VERSION {
            bail!(
                "unsupported config version {}; maximum supported version is {}",
                self.config_version,
                CURRENT_CONFIG_VERSION
            );
        }
        if self.config_version < 1 {
            for provider in &mut self.providers {
                if (provider.temperature - LEGACY_DEFAULT_TEMPERATURE).abs() < f32::EPSILON {
                    provider.temperature = default_temperature();
                }
            }
        }
        // The embedding model used to live under the knowledge base, which is
        // where it was first needed. It now also backs memory recall, and a
        // knowledge-base setting silently steering group-chat search is a trap
        // for whoever reads this next.
        if !self.embedding.is_configured() {
            let kb = &self.plugins.knowledge_base;
            if !kb.embedding_provider_id.trim().is_empty() && !kb.embedding_model.trim().is_empty()
            {
                self.embedding.provider_id = kb.embedding_provider_id.trim().to_string();
                self.embedding.model = kb.embedding_model.trim().to_string();
                if kb.embedding_timeout_seconds > 0 {
                    self.embedding.timeout_seconds = kb.embedding_timeout_seconds;
                }
                self.embedding.min_score = kb.semantic_min_score;
            }
        }
        self.config_version = CURRENT_CONFIG_VERSION;
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
        for provider in &mut self.providers {
            if provider.is_legacy_default_anthropic_model() {
                provider.models.clear();
                provider.default_model.clear();
            }
        }
        if let Some(active_models) = &mut self.active_provider_models {
            for active in active_models {
                if active.provider_id == "opencodezen" {
                    active.provider_id = OPENCODE_PROVIDER_ID.to_string();
                }
            }
        }
        if let Some(active_models) = &mut self.active_multimodal_provider_models {
            for active in active_models {
                if active.provider_id == "opencodezen" {
                    active.provider_id = OPENCODE_PROVIDER_ID.to_string();
                }
            }
        }
        self.platforms
            .rename_provider_references("opencodezen", OPENCODE_PROVIDER_ID);
        self.prune_stale_active_provider_models();
        self.normalize_platform_model_routes();
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
        if self
            .active_provider_models
            .as_ref()
            .is_some_and(Vec::is_empty)
        {
            self.active_provider_models = Some(vec![ActiveProviderModelConfig {
                provider_id: OPENCODE_PROVIDER_ID.to_string(),
                model: OPENCODE_DEFAULT_CHAT_MODEL.to_string(),
            }]);
        }
    }

    fn normalize_api_quota_accounts(&mut self) {
        normalize_api_quota_provider(&mut self.plugins.api_quota.deepseek);
        normalize_api_quota_provider(&mut self.plugins.api_quota.openrouter);
    }

    fn normalize_managed_output_paths(&mut self, paths: &MiyuPaths) {
        let Some(base) = directories::BaseDirs::new() else {
            return;
        };
        let documents = directories::UserDirs::new()
            .and_then(|dirs| dirs.document_dir().map(PathBuf::from))
            .unwrap_or_else(|| base.home_dir().join("Documents"));
        let pictures = std::env::var_os("XDG_PICTURES_DIR")
            .map(PathBuf::from)
            .or_else(|| {
                directories::UserDirs::new().and_then(|dirs| dirs.picture_dir().map(PathBuf::from))
            })
            .unwrap_or_else(|| base.home_dir().join("Pictures"));
        // The XDG data root is a legacy root too: an upgrade that ran while
        // `data_dir` still pointed at `~/.local/share/miyu` remapped these
        // fields onto it and persisted the result, so the value we now have to
        // heal is one this function itself wrote.
        let legacy_data = base.data_dir().join("miyu");
        if let Some((from, to)) = remap_managed_output_dir(
            &mut self.plugins.deep_research.output_dir,
            &[
                documents.join("Miyu"),
                documents.join("miyu"),
                legacy_data.join("documents"),
            ],
            &paths.data_dir.join("documents"),
            base.home_dir(),
        ) {
            relocate_managed_output(&from, &to);
        }
        if let Some((from, to)) = remap_managed_output_dir(
            &mut self.plugins.image_generation.output_dir,
            &[
                pictures.join("miyu"),
                pictures.join("Miyu"),
                legacy_data.join("pictures"),
            ],
            &paths.data_dir.join("pictures"),
            base.home_dir(),
        ) {
            relocate_managed_output(&from, &to);
        }
    }

    fn prune_stale_active_provider_models(&mut self) {
        if let Some(active_models) = &mut self.active_provider_models {
            active_models.retain(|active| active_model_exists(&self.providers, active));
        }
        if let Some(active_models) = &mut self.active_multimodal_provider_models {
            active_models.retain(|active| active_model_supports_image(&self.providers, active));
        }
    }

    pub fn validate(&self) -> Result<()> {
        if crate::i18n::UiLanguage::parse(&self.display.language).is_none() {
            bail!(
                "{}",
                crate::i18n::text(
                    "display.language must be 'auto', 'en', or 'zh'",
                    "display.language 必须是 'auto'、'en' 或 'zh'"
                )
            );
        }
        if self.active_provider.trim().is_empty() {
            bail!("active_provider cannot be empty");
        }
        if self.providers.is_empty() {
            bail!("at least one provider is required");
        }
        let mut provider_ids = HashSet::with_capacity(self.providers.len());
        for provider in &self.providers {
            if provider.id.trim().is_empty() {
                bail!("provider id cannot be empty");
            }
            if provider.id.trim() != provider.id {
                bail!(
                    "provider id must not contain surrounding whitespace: {}",
                    provider.id
                );
            }
            if !provider_ids.insert(provider.id.as_str()) {
                bail!("duplicate provider id: {}", provider.id);
            }
            if provider.base_url.trim().is_empty() {
                bail!("provider {} base_url cannot be empty", provider.id);
            }
        }
        if !(0.1..=1.0).contains(&self.context.trim_at_ratio) {
            bail!("context.trim_at_ratio must be between 0.1 and 1.0");
        }
        if !(0.1..=1.0).contains(&self.context.compact_force_ratio) {
            bail!("context.compact_force_ratio must be between 0.1 and 1.0");
        }
        if self.context.compact_force_ratio < self.context.trim_at_ratio {
            bail!("context.compact_force_ratio must be >= context.trim_at_ratio");
        }
        if !(0.05..=1.0).contains(&self.context.compact_soft_ratio)
            || !(0.05..=1.0).contains(&self.context.compact_snip_ratio)
        {
            bail!("context.compact_soft_ratio and compact_snip_ratio must be between 0.05 and 1.0");
        }
        if self.context.compact_soft_ratio > self.context.compact_snip_ratio
            || self.context.compact_snip_ratio > self.context.trim_at_ratio
        {
            bail!("context watermarks must be ordered: compact_soft_ratio <= compact_snip_ratio <= trim_at_ratio <= compact_force_ratio");
        }
        if !(0.01..=0.9).contains(&self.context.trim_batch_ratio) {
            bail!("context.trim_batch_ratio must be between 0.01 and 0.9");
        }
        match self.context.on_overflow.as_str() {
            "pop" | "compact" => {}
            value => bail!("context.on_overflow must be 'pop' or 'compact', got: {value}"),
        }
        if self.display.repl_replay_turns > MAX_REPL_REPLAY_TURNS {
            bail!("display.repl_replay_turns must be between 0 and {MAX_REPL_REPLAY_TURNS}");
        }
        if self.display.command_output_lines > MAX_COMMAND_OUTPUT_LINES {
            bail!("display.command_output_lines must be between 0 and {MAX_COMMAND_OUTPUT_LINES}");
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
        if !(0.0..=2.0).contains(&self.provider(None)?.temperature) {
            bail!("provider temperature must be between 0.0 and 2.0");
        }
        for provider in &self.providers {
            if provider.timeout_seconds == 0 {
                bail!(
                    "provider {} timeout_seconds must be greater than 0",
                    provider.id
                );
            }
            if !(0.0..=2.0).contains(&provider.temperature) {
                bail!(
                    "provider {} temperature must be between 0.0 and 2.0",
                    provider.id
                );
            }
            if provider.anthropic_max_tokens == 0 {
                bail!(
                    "provider {} anthropic_max_tokens must be greater than 0",
                    provider.id
                );
            }
        }
        for provider in &self.providers {
            for (model, cost) in &provider.model_costs {
                if cost.input < 0.0
                    || cost.output < 0.0
                    || cost.cache_read.is_some_and(|price| price < 0.0)
                {
                    bail!(
                        "provider {} model {model} price must be non-negative",
                        provider.id
                    );
                }
            }
        }
        if !(0.0..=1.0).contains(&self.plugins.memes.auto_send_probability) {
            bail!("plugins.memes.auto_send_probability must be between 0.0 and 1.0");
        }
        if self.plugins.memes.width_percent == 0 || self.plugins.memes.width_percent > 100 {
            bail!("plugins.memes.width_percent must be between 1 and 100");
        }
        if self.plugins.memes.height_percent == 0 || self.plugins.memes.height_percent > 100 {
            bail!("plugins.memes.height_percent must be between 1 and 100");
        }
        if self.plugins.memes.search_max_results == 0 || self.plugins.memes.search_max_results > 3 {
            bail!("plugins.memes.search_max_results must be between 1 and 3");
        }
        let mem = self.memory_config();
        if mem.forgetting_half_life_days <= 0.0 {
            bail!("memory.forgetting_half_life_days must be greater than 0");
        }
        if mem.forget_after_days == 0 {
            bail!("memory.forget_after_days must be greater than 0");
        }
        if !(2..=100).contains(&mem.diary_batch_size) {
            bail!("memory.diary_batch_size must be between 2 and 100");
        }
        if !(1..=3650).contains(&mem.short_diary_retention_days) {
            bail!("memory.short_diary_retention_days must be between 1 and 3650");
        }
        if !(1..=100).contains(&mem.diary_promotion_recalls) {
            bail!("memory.diary_promotion_recalls must be between 1 and 100");
        }
        if !(5..=600).contains(&mem.organizer_timeout_seconds) {
            bail!("memory.organizer_timeout_seconds must be between 5 and 600");
        }
        if !(0.0..=1.0).contains(&self.plugins.knowledge_base.semantic_min_score) {
            bail!("plugins.knowledge_base.semantic_min_score must be between 0.0 and 1.0");
        }
        validate_api_quota_accounts("deepseek", &self.plugins.api_quota.deepseek)?;
        validate_api_quota_accounts("openrouter", &self.plugins.api_quota.openrouter)?;
        self.validate_model_references()?;
        self.validate_global_multimodal_config()?;
        self.validate_platforms()?;
        self.provider(None)?;
        Ok(())
    }

    fn validate_model_references(&self) -> Result<()> {
        if let Some(pool) = &self.active_provider_models {
            if pool.is_empty() {
                bail!("at least one model endpoint must remain active");
            }
            validate_unique_existing_pool(&self.providers, "active text", pool, false)?;
        }
        let kb_provider = self.plugins.knowledge_base.embedding_provider_id.trim();
        if !kb_provider.is_empty() {
            self.provider(Some(kb_provider))?;
        }
        Ok(())
    }

    fn validate_global_multimodal_config(&self) -> Result<()> {
        if let Some(pool) = &self.active_multimodal_provider_models {
            validate_unique_existing_pool(&self.providers, "active multimodal", pool, true)?;
        }
        if self.plugins.vision.enabled && !self.plugins.vision.vision_provider_id.trim().is_empty()
        {
            self.vision_provider_choice()?;
        }
        Ok(())
    }

    fn validate_platforms(&self) -> Result<()> {
        let command_prefix = &self.platforms.command_prefix;
        if command_prefix.is_empty()
            || command_prefix.trim() != command_prefix
            || command_prefix.chars().count() > MAX_PLATFORM_COMMAND_PREFIX_CHARS
            || command_prefix
                .chars()
                .any(|character| character.is_whitespace() || character.is_control())
        {
            bail!(
                "platforms.command_prefix must be a trimmed, non-empty value of at most {MAX_PLATFORM_COMMAND_PREFIX_CHARS} characters without whitespace"
            );
        }
        for command in self.platforms.commands.keys() {
            if command.is_empty()
                || command.len() > 64
                || !command.bytes().all(|byte| {
                    byte.is_ascii_lowercase()
                        || byte.is_ascii_digit()
                        || matches!(byte, b'_' | b'-')
                })
            {
                bail!(
                    "platforms.commands keys must be lowercase ASCII command ids of at most 64 bytes"
                );
            }
        }
        let qq = &self.platforms.qq;
        if qq.reverse_ws_port == 0 {
            bail!("platforms.qq.reverse_ws_port must be between 1 and 65535");
        }
        for (field, limits) in [
            ("session_limits", Some(qq.session_limits)),
            (
                "private_chats.session_limits",
                qq.private_chats.session_limits,
            ),
            ("group_chats.session_limits", qq.group_chats.session_limits),
        ] {
            if let Some(limits) = limits {
                validate_platform_session_limits(field, limits)?;
            }
        }
        validate_unique_existing_pool(
            &self.providers,
            "QQ text",
            qq.text_models.as_deref().unwrap_or_default(),
            false,
        )?;
        validate_unique_existing_pool(
            &self.providers,
            "QQ multimodal",
            qq.multimodal_models.as_deref().unwrap_or_default(),
            true,
        )?;
        validate_unique_existing_pool(
            &self.providers,
            "QQ non-whitelist text",
            qq.non_whitelist_text_models.as_deref().unwrap_or_default(),
            false,
        )?;
        for (field, limit) in [
            (
                "private_chats.non_whitelist_rate_limit",
                qq.private_chats.non_whitelist_rate_limit,
            ),
            (
                "group_chats.whitelist_rate_limit",
                qq.group_chats.whitelist_rate_limit,
            ),
            (
                "group_chats.non_whitelist_rate_limit",
                qq.group_chats.non_whitelist_rate_limit,
            ),
        ] {
            if limit.window_seconds == 0 || limit.window_seconds > 86_400 {
                bail!("platforms.qq.{field}.window_seconds must be between 1 and 86400");
            }
        }
        for (field, ids) in [
            ("admin_users", qq.admin_users.as_slice()),
            (
                "private_chats.whitelist",
                qq.private_chats.whitelist.as_slice(),
            ),
            ("group_chats.whitelist", qq.group_chats.whitelist.as_slice()),
        ] {
            let mut seen = HashSet::with_capacity(ids.len());
            if ids.iter().any(|id| *id <= 0 || !seen.insert(*id)) {
                bail!("platforms.qq.{field} must contain unique positive QQ ids");
            }
        }
        let mut trigger_keywords = HashSet::with_capacity(qq.group_chats.trigger_keywords.len());
        for keyword in &qq.group_chats.trigger_keywords {
            if keyword.is_empty()
                || keyword.trim() != keyword
                || keyword.chars().count() > 128
                || keyword.chars().any(char::is_control)
                || !trigger_keywords.insert(keyword)
            {
                bail!(
                    "platforms.qq.group_chats.trigger_keywords must contain unique, trimmed, non-empty values of at most 128 characters"
                );
            }
        }
        let mut identities = HashSet::with_capacity(qq.conversations.len());
        for route in &qq.conversations {
            self.validate_platform_model_route(route)?;
            if let Some(limits) = route.session_limits {
                validate_platform_session_limits("conversations[].session_limits", limits)?;
            }
            if !identities.insert(route.identity()) {
                bail!(
                    "duplicate QQ conversation configuration: {} / {}",
                    route.conversation.kind.as_str(),
                    route.conversation.id
                );
            }
        }
        for (plugin_id, instance) in &qq.plugins {
            if plugin_id.trim().is_empty() || plugin_id.trim() != plugin_id {
                bail!("QQ plugin ids must be non-empty and trimmed");
            }
            if let Some((_, validate)) = PLATFORM_PLUGIN_VALIDATORS
                .iter()
                .find(|(id, _)| *id == plugin_id)
            {
                validate(instance)?;
            }
            if plugin_id == REAL_CONTEXT_PLUGIN_ID {
                let settings = RealContextPluginSettings::from_instance(instance)?;
                if let Some(models) = settings.text_models.as_deref() {
                    validate_unique_existing_pool(
                        &self.providers,
                        "real-context text",
                        models,
                        false,
                    )?;
                }
            }
        }
        Ok(())
    }

    pub fn validate_platform_model_route(&self, route: &PlatformModelRoute) -> Result<()> {
        if !is_positive_decimal_id(&route.conversation.id) {
            let label = match route.conversation.kind {
                PlatformConversationKind::Private => "QQ id",
                PlatformConversationKind::Group => "group id",
            };
            bail!("QQ conversation id must be a positive decimal {label}");
        }
        if route.extra_prompt.chars().count() > 200_000 || route.extra_prompt.contains('\0') {
            bail!("QQ conversation extra_prompt is invalid or exceeds 200000 characters");
        }
        if let PlatformPersonaOverride::Custom { name } = &route.persona {
            let path = Path::new(name);
            if name.is_empty()
                || name.trim() != name
                || name.chars().count() > 255
                || !name.ends_with(".md")
                || name.chars().any(char::is_control)
                || path.file_name().and_then(|value| value.to_str()) != Some(name.as_str())
            {
                bail!("QQ conversation persona must be a safe Markdown persona filename");
            }
        }
        self.validate_platform_model_pool(
            route,
            "text_models",
            route.text_models.as_deref(),
            false,
        )?;
        self.validate_platform_model_pool(
            route,
            "multimodal_models",
            route.multimodal_models.as_deref(),
            true,
        )?;
        Ok(())
    }

    fn validate_platform_model_pool(
        &self,
        route: &PlatformModelRoute,
        field: &str,
        pool: Option<&[ActiveProviderModelConfig]>,
        require_multimodal: bool,
    ) -> Result<()> {
        let Some(pool) = pool else {
            return Ok(());
        };
        let mut seen = HashSet::with_capacity(pool.len());
        for entry in pool {
            if !seen.insert((entry.provider_id.as_str(), entry.model.as_str())) {
                bail!(
                    "duplicate {} model in platform route: {} / {}",
                    field,
                    entry.provider_id,
                    entry.model
                );
            }
            if !active_model_exists(&self.providers, entry) {
                bail!(
                    "unknown {} provider/model in QQ conversation {} / {}: {} / {}",
                    field,
                    route.conversation.kind.as_str(),
                    route.conversation.id,
                    entry.provider_id,
                    entry.model
                );
            }
            if require_multimodal
                && !self.model_supports_any_input(&entry.provider_id, &entry.model, &["image"])
            {
                bail!(
                    "platform route multimodal model does not declare image input: {} / {}",
                    entry.provider_id,
                    entry.model
                );
            }
        }
        Ok(())
    }

    pub fn platform_model_route(
        &self,
        kind: PlatformConversationKind,
        conversation_id: &str,
    ) -> Option<&PlatformModelRoute> {
        self.platforms.model_route(kind, conversation_id)
    }

    pub fn qq_text_model_pool<'a>(
        &'a self,
        kind: PlatformConversationKind,
        conversation_id: &str,
        use_non_whitelist_pool: bool,
    ) -> Option<&'a [ActiveProviderModelConfig]> {
        if let Some(route) = self.platform_model_route(kind, conversation_id) {
            if route.text_models.is_some() {
                return route.text_models.as_deref();
            }
            if route.text_models_inheritance == PlatformModelPoolInheritance::Global {
                return self.active_provider_models.as_deref();
            }
        }
        if use_non_whitelist_pool {
            if let Some(pool) = self.platforms.qq.non_whitelist_text_models.as_deref() {
                return Some(pool);
            }
        }
        self.platforms
            .qq
            .text_models
            .as_deref()
            .or(self.active_provider_models.as_deref())
    }

    pub fn qq_multimodal_model_pool(
        &self,
        kind: PlatformConversationKind,
        conversation_id: &str,
    ) -> Option<&[ActiveProviderModelConfig]> {
        if let Some(route) = self.platform_model_route(kind, conversation_id) {
            if route.multimodal_models.is_some() {
                return route.multimodal_models.as_deref();
            }
            if route.multimodal_models_inheritance == PlatformModelPoolInheritance::Global {
                return self.active_multimodal_provider_models.as_deref();
            }
        }
        self.platforms
            .qq
            .multimodal_models
            .as_deref()
            .or(self.active_multimodal_provider_models.as_deref())
    }

    pub fn apply_qq_conversation_persona(
        &mut self,
        kind: PlatformConversationKind,
        conversation_id: &str,
    ) {
        let persona = self
            .platform_model_route(kind, conversation_id)
            .map(|route| route.persona.clone())
            .unwrap_or_default();
        match persona {
            PlatformPersonaOverride::Inherit => {}
            PlatformPersonaOverride::Miyu => self.prompt.active_persona.clear(),
            PlatformPersonaOverride::Custom { name } => self.prompt.active_persona = name,
        }
    }

    pub fn normalize_platform_model_routes(&mut self) {
        self.platforms.normalize_model_routes();
    }

    pub fn prune_platform_model_routes(&mut self) {
        self.platforms.prune_model_references(&self.providers);
    }

    pub fn rename_platform_provider_references(&mut self, old_id: &str, new_id: &str) {
        self.platforms.rename_provider_references(old_id, new_id);
    }

    pub fn rename_platform_model_references(&mut self, provider_id: &str, old: &str, new: &str) {
        self.platforms
            .rename_model_references(provider_id, old, new);
    }

    pub fn rename_provider_references(&mut self, old_id: &str, new_id: &str) {
        if old_id == new_id || old_id.is_empty() || new_id.is_empty() {
            return;
        }
        if self.active_provider == old_id {
            self.active_provider = new_id.to_string();
        }
        for entries in [
            self.active_provider_models.as_mut(),
            self.active_multimodal_provider_models.as_mut(),
        ]
        .into_iter()
        .flatten()
        {
            rename_provider_in_pool(entries, old_id, new_id);
        }
        for tier in ModelTier::ALL {
            rename_provider_in_pool(self.subagent_tiers.pool_mut(tier), old_id, new_id);
        }
        self.platforms.rename_provider_references(old_id, new_id);
        if self.plugins.vision.vision_provider_id == old_id {
            self.plugins.vision.vision_provider_id = new_id.to_string();
        }
        if self.plugins.knowledge_base.embedding_provider_id == old_id {
            self.plugins.knowledge_base.embedding_provider_id = new_id.to_string();
        }
    }

    /// Removes references after a provider has been deleted from `providers`.
    pub fn remove_provider_references(&mut self, provider_id: &str) {
        retain_provider_pool(&mut self.active_provider_models, provider_id);
        retain_provider_pool(&mut self.active_multimodal_provider_models, provider_id);
        for tier in ModelTier::ALL {
            self.subagent_tiers
                .pool_mut(tier)
                .retain(|entry| entry.provider_id != provider_id);
        }
        self.platforms.remove_provider_references(provider_id);
        if self.plugins.vision.vision_provider_id == provider_id {
            self.plugins.vision.vision_provider_id.clear();
            self.plugins.vision.vision_model.clear();
        }
        if self.plugins.knowledge_base.embedding_provider_id == provider_id {
            self.plugins.knowledge_base.embedding_provider_id.clear();
            self.plugins.knowledge_base.embedding_model.clear();
        }
        if self.active_provider == provider_id {
            self.active_provider = self
                .active_provider_models
                .as_ref()
                .and_then(|pool| pool.first())
                .map(|entry| entry.provider_id.clone())
                .or_else(|| {
                    self.providers
                        .iter()
                        .find(|provider| !provider.default_model.trim().is_empty())
                        .or_else(|| self.providers.first())
                        .map(|provider| provider.id.clone())
                })
                .unwrap_or_default();
        }
    }

    /// Reconciles every model reference with the current provider models and
    /// input capabilities after an editor changes model metadata.
    pub fn prune_model_references(&mut self) {
        self.prune_stale_active_provider_models();
        retain_nonempty_pool(&mut self.active_provider_models);
        retain_nonempty_pool(&mut self.active_multimodal_provider_models);
        self.prune_subagent_tiers();
        self.prune_platform_model_routes();

        let vision_provider_id = self.plugins.vision.vision_provider_id.trim();
        if !vision_provider_id.is_empty() {
            let vision_model = self.plugins.vision.vision_model.trim();
            let valid = self
                .provider(Some(vision_provider_id))
                .ok()
                .map(|provider| {
                    let model = if vision_model.is_empty() {
                        provider.default_model.as_str()
                    } else {
                        vision_model
                    };
                    provider
                        .input_modalities(model)
                        .is_some_and(|modalities| modalities.iter().any(|item| item == "image"))
                })
                .unwrap_or(false);
            if !valid {
                self.plugins.vision.vision_provider_id.clear();
                self.plugins.vision.vision_model.clear();
            }
        }

        let kb_provider_id = self.plugins.knowledge_base.embedding_provider_id.trim();
        if !kb_provider_id.is_empty() && self.provider(Some(kb_provider_id)).is_err() {
            self.plugins.knowledge_base.embedding_provider_id.clear();
            self.plugins.knowledge_base.embedding_model.clear();
        }
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

    /// Embedding models are excluded: they produce vectors, not replies, and
    /// picking one here is always a mistake. The multimodal list derives from
    /// this one, so filtering here covers both.
    pub fn text_provider_model_choices(&self) -> Vec<ProviderModelChoice> {
        self.providers
            .iter()
            .flat_map(|provider| {
                provider
                    .models
                    .iter()
                    .filter(|model| !model.trim().is_empty())
                    .filter(|model| !Self::model_is_embedding(provider, model))
                    .map(|model| ProviderModelChoice {
                        provider_id: provider.id.clone(),
                        provider_name: provider.display_name.clone(),
                        model: model.clone(),
                    })
                    .collect::<Vec<_>>()
            })
            .collect()
    }

    /// A model marked as producing vectors rather than chat. Stored beside the
    /// input modalities because it answers the same question — what the model
    /// is for.
    pub fn model_is_embedding(provider: &ProviderConfig, model: &str) -> bool {
        provider
            .model_modalities
            .get(model)
            .is_some_and(|modalities| modalities.iter().any(|item| item == EMBEDDING_MODALITY))
    }

    pub fn active_provider_model_choices(&self) -> Vec<ProviderModelChoice> {
        match &self.active_provider_models {
            None => self
                .provider(None)
                .ok()
                .filter(|provider| !provider.default_model.trim().is_empty())
                .map(|provider| {
                    vec![ProviderModelChoice {
                        provider_id: provider.id.clone(),
                        provider_name: provider.display_name.clone(),
                        model: provider.default_model.clone(),
                    }]
                })
                .unwrap_or_default(),
            Some(active_models) => active_models
                .iter()
                .filter_map(|active| {
                    let provider = self.provider(Some(active.provider_id.trim())).ok()?;
                    let model = active.model.trim();
                    provider
                        .has_configured_model(model)
                        .then(|| ProviderModelChoice {
                            provider_id: provider.id.clone(),
                            provider_name: provider.display_name.clone(),
                            model: model.to_string(),
                        })
                })
                .collect(),
        }
    }

    pub fn multimodal_provider_model_choices(&self) -> Vec<ProviderModelChoice> {
        self.text_provider_model_choices()
            .into_iter()
            .filter(|choice| {
                self.model_supports_any_input(&choice.provider_id, &choice.model, &["image"])
            })
            .collect()
    }

    pub fn active_multimodal_provider_model_choices(&self) -> Vec<ProviderModelChoice> {
        match &self.active_multimodal_provider_models {
            Some(active_models) => active_models
                .iter()
                .filter_map(|active| {
                    let provider = self.provider(Some(active.provider_id.trim())).ok()?;
                    let model = active.model.trim();
                    (provider.has_configured_model(model)
                        && provider.input_modalities(model).is_some_and(|modalities| {
                            modalities.iter().any(|item| item == "image")
                        }))
                    .then(|| ProviderModelChoice {
                        provider_id: provider.id.clone(),
                        provider_name: provider.display_name.clone(),
                        model: model.to_string(),
                    })
                })
                .collect(),
            None => Vec::new(),
        }
    }

    pub fn is_active_multimodal_provider_model(&self, provider_id: &str, model: &str) -> bool {
        self.active_multimodal_provider_models
            .as_ref()
            .map(|active_models| {
                active_models
                    .iter()
                    .any(|active| active.provider_id == provider_id && active.model == model)
            })
            .unwrap_or(false)
    }

    pub fn remove_active_model_references(&mut self, provider_id: &str, model: &str) {
        if let Some(active_models) = &mut self.active_provider_models {
            active_models
                .retain(|active| !(active.provider_id == provider_id && active.model == model));
        }
        if let Some(active_models) = &mut self.active_multimodal_provider_models {
            active_models
                .retain(|active| !(active.provider_id == provider_id && active.model == model));
        }
        // A model gone from the text models must leave every tier pool too.
        for tier in ModelTier::ALL {
            self.subagent_tiers
                .pool_mut(tier)
                .retain(|entry| !(entry.provider_id == provider_id && entry.model == model));
        }
        self.platforms.remove_model_references(provider_id, model);
        if self.plugins.vision.vision_provider_id == provider_id
            && self.plugins.vision.vision_model == model
        {
            self.plugins.vision.vision_provider_id.clear();
            self.plugins.vision.vision_model.clear();
        }
        if self.plugins.knowledge_base.embedding_provider_id == provider_id
            && self.plugins.knowledge_base.embedding_model == model
        {
            self.plugins.knowledge_base.embedding_provider_id.clear();
            self.plugins.knowledge_base.embedding_model.clear();
        }
        retain_nonempty_pool(&mut self.active_provider_models);
        retain_nonempty_pool(&mut self.active_multimodal_provider_models);
    }

    pub fn toggle_active_multimodal_provider_model(
        &mut self,
        provider_id: &str,
        model: &str,
    ) -> Result<bool> {
        if model.trim().is_empty() {
            bail!("model cannot be empty");
        }
        if let Some(active_models) = &mut self.active_multimodal_provider_models {
            if let Some(index) = active_models
                .iter()
                .position(|active| active.provider_id == provider_id && active.model == model)
            {
                active_models.remove(index);
                return Ok(false);
            }
        }
        let provider = self.provider(Some(provider_id))?;
        if !provider.has_configured_model(model) {
            bail!("model is not configured for provider {provider_id}: {model}");
        }
        if !provider
            .input_modalities(model)
            .is_some_and(|modalities| modalities.iter().any(|item| item == "image"))
        {
            bail!("multimodal model does not declare image input: {provider_id} / {model}");
        }
        let active_models = self
            .active_multimodal_provider_models
            .get_or_insert_with(Vec::new);
        active_models.push(ActiveProviderModelConfig {
            provider_id: provider_id.to_string(),
            model: model.to_string(),
        });
        Ok(true)
    }

    pub fn model_supports_any_input(
        &self,
        provider_id: &str,
        model: &str,
        inputs: &[&str],
    ) -> bool {
        self.provider(Some(provider_id))
            .ok()
            .and_then(|provider| provider.input_modalities(model))
            .map(|modalities| {
                modalities
                    .iter()
                    .any(|m| inputs.iter().any(|input| m == input))
            })
            .unwrap_or(false)
    }

    pub fn vision_provider_choice(&self) -> Result<(String, String)> {
        let vision = &self.plugins.vision;
        if !vision.vision_provider_id.trim().is_empty() {
            let provider_id = vision.vision_provider_id.trim().to_string();
            let provider = self.provider(Some(&provider_id))?;
            let model = if vision.vision_model.trim().is_empty() {
                provider.default_model.clone()
            } else {
                vision.vision_model.trim().to_string()
            };
            if !provider
                .input_modalities(&model)
                .is_some_and(|modalities| modalities.iter().any(|item| item == "image"))
            {
                bail!("vision model does not declare image input: {provider_id} / {model}");
            }
            return Ok((provider_id, model));
        }
        if let Some(active) = self.active_multimodal_provider_models.as_ref() {
            if let Some(choice) = self
                .active_multimodal_provider_model_choices()
                .into_iter()
                .find(|choice| {
                    self.model_supports_any_input(&choice.provider_id, &choice.model, &["image"])
                })
            {
                return Ok((choice.provider_id, choice.model));
            }
            if !active.is_empty() {
                bail!("the configured multimodal model pool has no image-capable model");
            }
        }
        Ok((
            OPENCODE_PROVIDER_ID.to_string(),
            OPENCODE_DEFAULT_VISION_MODEL.to_string(),
        ))
    }

    /// A tier pool's usable model choices: configured entries filtered to
    /// models that still exist under their provider (entries whose model
    /// was removed from the text models are ignored, mirroring
    /// `active_provider_model_choices`).
    pub fn subagent_tier_choices(&self, tier: ModelTier) -> Vec<ProviderModelChoice> {
        self.subagent_tiers
            .pool(tier)
            .iter()
            .filter_map(|entry| {
                let provider = self.provider(Some(entry.provider_id.trim())).ok()?;
                let model = entry.model.trim();
                provider
                    .has_configured_model(model)
                    .then(|| ProviderModelChoice {
                        provider_id: provider.id.clone(),
                        provider_name: provider.display_name.clone(),
                        model: model.to_string(),
                    })
            })
            .collect()
    }

    pub fn is_subagent_tier_model(&self, tier: ModelTier, provider_id: &str, model: &str) -> bool {
        self.subagent_tiers
            .pool(tier)
            .iter()
            .any(|entry| entry.provider_id == provider_id && entry.model == model)
    }

    /// Adds/removes a model in a tier pool. Returns `true` when the model
    /// is in the pool after the call.
    pub fn toggle_subagent_tier_model(
        &mut self,
        tier: ModelTier,
        provider_id: &str,
        model: &str,
    ) -> Result<bool> {
        if model.trim().is_empty() {
            bail!("model cannot be empty");
        }
        self.provider(Some(provider_id))?;
        let pool = self.subagent_tiers.pool_mut(tier);
        if let Some(index) = pool
            .iter()
            .position(|entry| entry.provider_id == provider_id && entry.model == model)
        {
            pool.remove(index);
            Ok(false)
        } else {
            pool.push(ActiveProviderModelConfig {
                provider_id: provider_id.to_string(),
                model: model.to_string(),
            });
            Ok(true)
        }
    }

    /// Drops tier pool entries whose model no longer exists among the
    /// configured text models (a model removed from a provider must also
    /// leave every tier pool).
    pub fn prune_subagent_tiers(&mut self) {
        for tier in ModelTier::ALL {
            let providers = &self.providers;
            self.subagent_tiers
                .pool_mut(tier)
                .retain(|entry| active_model_exists(providers, entry));
        }
    }

    pub fn is_active_provider_model(&self, provider_id: &str, model: &str) -> bool {
        match &self.active_provider_models {
            None => self
                .provider(None)
                .map(|provider| provider.id == provider_id && provider.default_model == model)
                .unwrap_or(false),
            Some(active_models) => active_models
                .iter()
                .any(|active| active.provider_id == provider_id && active.model == model),
        }
    }

    pub fn toggle_active_provider_model(&mut self, provider_id: &str, model: &str) -> Result<bool> {
        if model.trim().is_empty() {
            bail!("model cannot be empty");
        }
        self.provider(Some(provider_id))?;
        if self.active_provider_models.is_none() {
            self.active_provider_models = Some(
                self.active_provider_model_choices()
                    .into_iter()
                    .map(|choice| ActiveProviderModelConfig {
                        provider_id: choice.provider_id,
                        model: choice.model,
                    })
                    .collect(),
            );
        }
        let active_models = self.active_provider_models.get_or_insert_with(Vec::new);
        if let Some(index) = active_models
            .iter()
            .position(|active| active.provider_id == provider_id && active.model == model)
        {
            active_models.remove(index);
            return Ok(false);
        }
        active_models.push(ActiveProviderModelConfig {
            provider_id: provider_id.to_string(),
            model: model.to_string(),
        });
        Ok(true)
    }

    pub fn set_active_provider_models(
        &mut self,
        models: &[ActiveProviderModelConfig],
    ) -> Result<()> {
        if models.is_empty() {
            bail!("at least one model endpoint must remain active");
        }
        let choices = self.provider_model_choices();
        let mut seen = std::collections::HashSet::with_capacity(models.len());
        for model in models {
            if model.provider_id.trim().is_empty() || model.model.trim().is_empty() {
                bail!("provider_id and model cannot be empty");
            }
            if !seen.insert((&model.provider_id, &model.model)) {
                bail!(
                    "duplicate active provider/model: {} / {}",
                    model.provider_id,
                    model.model
                );
            }
            if !choices.iter().any(|choice| {
                choice.provider_id == model.provider_id && choice.model == model.model
            }) {
                bail!(
                    "unknown configured provider/model: {} / {}",
                    model.provider_id,
                    model.model
                );
            }
        }
        self.active_provider_models = Some(models.to_vec());
        Ok(())
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
        self.active_provider_models = Some(vec![ActiveProviderModelConfig {
            provider_id: provider.id.clone(),
            model: model.to_string(),
        }]);
        if !provider.models.iter().any(|item| item == model) {
            provider.models.push(model.to_string());
        }
        Ok(())
    }

    pub fn remove_active_provider_model(&mut self, provider_id: &str, model: &str) -> Result<()> {
        let provider_index = self
            .providers
            .iter()
            .position(|provider| provider.id == provider_id)
            .with_context(|| format!("provider not found: {provider_id}"))?;
        if model.trim().is_empty() {
            bail!("model cannot be empty");
        }
        {
            let provider = &mut self.providers[provider_index];
            provider.models.retain(|item| item != model);
            provider.model_context_window.remove(model);
            provider.model_modalities.remove(model);
            if provider.default_model == model {
                provider.default_model = provider.models.first().cloned().unwrap_or_default();
            }
        }
        self.remove_active_model_references(provider_id, model);
        Ok(())
    }

    pub fn active_context_window(&self) -> Result<Option<usize>> {
        let choices = self.active_provider_model_choices();
        if choices.is_empty() {
            return Ok(None);
        }
        let mut windows = Vec::new();
        for choice in choices {
            let Some(window) =
                self.context_window_for_provider_model(&choice.provider_id, &choice.model)?
            else {
                return Ok(None);
            };
            windows.push(window);
        }
        Ok(windows.into_iter().min())
    }

    pub fn context_window_for_provider_model(
        &self,
        provider_id: &str,
        model: &str,
    ) -> Result<Option<usize>> {
        let provider = self.provider(Some(provider_id))?;
        if let Some(window) = provider
            .model_context_window
            .get(model)
            .copied()
            .filter(|&w| w > 0)
        {
            return Ok(Some(window));
        }
        Ok(crate::models_cache::context_window(provider_id, model)
            .map(|w| w as usize)
            .or_else(|| {
                (self.context.default_context_window > 0)
                    .then_some(self.context.default_context_window)
            }))
    }

    pub fn system_prompt(&self, paths: &MiyuPaths) -> Result<String> {
        self.system_prompt_for(paths, PromptAudience::Owner)
    }

    pub fn system_prompt_for(&self, paths: &MiyuPaths, audience: PromptAudience) -> Result<String> {
        let mut prompt = self.base_system_prompt(paths)?;
        if audience.includes_user_identity() {
            let user_identity = self.user_identity_prompt(paths)?;
            if !user_identity.trim().is_empty() {
                prompt.push_str("\n\n<current-user-profile>\n");
                prompt.push_str(
                    "This profile describes the user currently interacting with you.\n\n",
                );
                prompt.push_str(user_identity.trim());
                prompt.push_str("\n</current-user-profile>");
            }
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
        migrated_resource_path(paths, &self.prompt.prompts_dir)
            .unwrap_or_else(|| config_relative_path(paths, &self.prompt.prompts_dir))
    }

    pub fn user_identity_path(&self, paths: &MiyuPaths) -> PathBuf {
        if relative_path_equals(&self.prompt.user_identity_file, "user-identity.md") {
            fallback_resource_file(paths, "identities", "user-identity.md")
        } else if let Some(path) = migrated_fallback_file(
            paths,
            &self.prompt.user_identity_file,
            "identities",
            "user-identity.md",
        ) {
            path
        } else if let Some(path) = migrated_resource_path(paths, &self.prompt.user_identity_file) {
            path
        } else {
            config_relative_path(paths, &self.prompt.user_identity_file)
        }
    }

    pub fn identities_dir_path(&self, paths: &MiyuPaths) -> PathBuf {
        migrated_resource_path(paths, &self.prompt.identities_dir)
            .unwrap_or_else(|| config_relative_path(paths, &self.prompt.identities_dir))
    }

    pub fn persona_path(&self, paths: &MiyuPaths, name: &str) -> PathBuf {
        self.prompts_dir_path(paths).join(name)
    }

    pub fn validate_persona_files(&self, paths: &MiyuPaths) -> Result<()> {
        if self
            .prompt
            .active_persona
            .trim()
            .eq_ignore_ascii_case("system-prompt.md")
        {
            bail!("system-prompt.md is reserved and cannot be used as a persona");
        }
        let directory = self.prompts_dir_path(paths);
        if !directory.exists() {
            return Ok(());
        }
        let mut scopes = HashMap::<String, String>::new();
        for entry in std::fs::read_dir(directory)? {
            let entry = entry?;
            if !entry.file_type()?.is_file() {
                continue;
            }
            let name = entry.file_name().to_string_lossy().to_string();
            if !name.ends_with(".md") {
                continue;
            }
            if name.eq_ignore_ascii_case("system-prompt.md") {
                continue;
            }
            let scope = persona_scope_name(&name);
            if let Some(existing) = scopes.insert(scope.clone(), name.clone()) {
                bail!(
                    "persona names map to the same persistent scope: {existing} and {name} ({scope})"
                );
            }
        }
        Ok(())
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

    /// Sanitized scope name of the active persona; also the namespace key for
    /// sessions and per-persona state directories.
    pub fn active_persona_scope(&self) -> String {
        persona_scope_name(self.prompt.active_persona.trim())
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
        if relative_path_equals(value, "system-prompt.md") {
            fallback_resource_file(paths, "prompts", "system-prompt.md")
        } else if let Some(path) =
            migrated_fallback_file(paths, value, "prompts", "system-prompt.md")
        {
            path
        } else if let Some(path) = migrated_resource_path(paths, value) {
            path
        } else {
            let path = PathBuf::from(value);
            if path.is_absolute() {
                path
            } else {
                paths.config_dir.join(path)
            }
        }
    }

    pub fn upsert_provider(&mut self, provider: ProviderConfig) {
        self.active_provider = provider.id.clone();
        self.active_provider_models = if provider.default_model.trim().is_empty() {
            Some(vec![ActiveProviderModelConfig {
                provider_id: OPENCODE_PROVIDER_ID.to_string(),
                model: OPENCODE_DEFAULT_CHAT_MODEL.to_string(),
            }])
        } else {
            Some(vec![ActiveProviderModelConfig {
                provider_id: provider.id.clone(),
                model: provider.default_model.clone(),
            }])
        };
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

fn validate_api_quota_accounts(provider: &str, config: &ApiQuotaProviderConfig) -> Result<()> {
    if !config.api_key.trim().is_empty() && !config.accounts.is_empty() {
        bail!("plugins.api_quota.{provider} legacy api_key could not be migrated");
    }
    if config.accounts.len() > 32 {
        bail!("plugins.api_quota.{provider} supports at most 32 accounts");
    }
    let mut names = HashSet::with_capacity(config.accounts.len());
    let mut ids = HashSet::with_capacity(config.accounts.len());
    for account in &config.accounts {
        let name = account.name.trim();
        if name.is_empty() {
            bail!("plugins.api_quota.{provider} account name cannot be empty");
        }
        if name.chars().count() > 64 {
            bail!("plugins.api_quota.{provider} account name exceeds 64 characters");
        }
        if !names.insert(name) {
            bail!("duplicate plugins.api_quota.{provider} account name: {name}");
        }
        let id = account.id.trim();
        if !id.is_empty() && !ids.insert(id) {
            bail!("duplicate plugins.api_quota.{provider} account id: {id}");
        }
    }
    Ok(())
}

fn default_timeout() -> u64 {
    60
}

fn default_vision_response_header_timeout() -> u64 {
    15
}

fn default_vision_stream_idle_timeout() -> u64 {
    20
}

fn default_vision_image_timeout() -> u64 {
    60
}

fn default_mcp_timeout() -> u64 {
    30
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

fn normalized_relative_path(value: &str) -> Option<PathBuf> {
    normalize_relative_path(Path::new(value.trim()))
}

fn normalize_relative_path(path: &Path) -> Option<PathBuf> {
    if path.is_absolute() {
        return None;
    }
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::Normal(component) => normalized.push(component),
            std::path::Component::ParentDir => {
                if !normalized.pop() {
                    return None;
                }
            }
            _ => return None,
        }
    }
    (!normalized.as_os_str().is_empty()).then_some(normalized)
}

fn relative_path_equals(value: &str, expected: &str) -> bool {
    normalized_relative_path(value).as_deref() == Some(Path::new(expected))
}

fn migrated_resource_path(paths: &MiyuPaths, value: &str) -> Option<PathBuf> {
    paths.migrated_resource_path(Path::new(value.trim()))
}

fn fallback_resource_file(paths: &MiyuPaths, namespace: &str, file_name: &str) -> PathBuf {
    if paths.resources_use_config_dir() {
        paths.config_dir.join(file_name)
    } else {
        paths.resource_dir().join(namespace).join(file_name)
    }
}

fn migrated_fallback_file(
    paths: &MiyuPaths,
    value: &str,
    namespace: &str,
    file_name: &str,
) -> Option<PathBuf> {
    let path = Path::new(value.trim());
    let matches_current = path == paths.config_dir.join(file_name);
    let matches_legacy = paths
        .legacy_config_dir()
        .is_some_and(|legacy| path == legacy.join(file_name));
    (path.is_absolute() && (matches_current || matches_legacy))
        .then(|| fallback_resource_file(paths, namespace, file_name))
}

fn config_relative_path(paths: &MiyuPaths, value: &str) -> PathBuf {
    let path = PathBuf::from(value.trim());
    if path.is_absolute() {
        path
    } else {
        paths.config_dir.join(path)
    }
}

pub(crate) fn persona_scope_name(name: &str) -> String {
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
    1.0
}

fn is_default_timeout(value: &u64) -> bool {
    *value == default_timeout()
}

fn is_default_temperature(value: &f32) -> bool {
    (*value - default_temperature()).abs() < f32::EPSILON
}

fn default_anthropic_max_tokens() -> u32 {
    4096
}

fn default_context_window() -> usize {
    168_000
}

fn is_default_anthropic_max_tokens(value: &u32) -> bool {
    *value == default_anthropic_max_tokens()
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
    // v7 §八点七 stub mode: byte-constant tools array + on-demand contracts.
    // "hybrid" (grow the tools array on load) and "full" remain available.
    "stub".to_string()
}

fn default_subagent_concurrency() -> usize {
    4
}

fn default_display_language() -> String {
    "auto".to_string()
}

fn default_reasoning_display() -> String {
    "summary".to_string()
}

fn default_tool_call_display() -> String {
    "summary".to_string()
}

fn default_command_output_lines() -> usize {
    10
}

fn default_repl_replay_turns() -> usize {
    3
}

fn default_mixed_model_endpoint_display() -> String {
    "interactive".to_string()
}

fn default_memory_association_facts() -> usize {
    5
}

fn default_memory_diary_batch_size() -> usize {
    14
}

fn default_memory_short_diary_retention_days() -> u64 {
    14
}

fn default_memory_diary_promotion_recalls() -> u64 {
    3
}

fn default_memory_organizer_timeout_seconds() -> u64 {
    120
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

fn default_memes_search_max_results() -> usize {
    1
}

fn default_memes_auto_send_probability() -> f32 {
    0.05
}

fn default_web_search_max_results() -> usize {
    2
}

fn default_web_images_max_results() -> usize {
    5
}

fn default_web_images_source_mode() -> String {
    "auto".to_string()
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
    default_miyu_home()
        .join("data/documents/deep-thinking")
        .display()
        .to_string()
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
    default_miyu_home()
        .join("data/pictures/generated-images")
        .display()
        .to_string()
}

fn default_miyu_home() -> PathBuf {
    std::env::var_os("MIYU_HOME")
        .map(PathBuf::from)
        .or_else(|| directories::BaseDirs::new().map(|dirs| dirs.home_dir().join(".miyu")))
        .unwrap_or_else(|| PathBuf::from("~/.miyu"))
}

/// Returns the old absolute directory when the value was rewritten, so the
/// caller can carry any files across; `None` when nothing matched.
fn remap_managed_output_dir(
    value: &mut String,
    legacy_roots: &[PathBuf],
    destination_root: &Path,
    home: &Path,
) -> Option<(PathBuf, PathBuf)> {
    let trimmed = value.trim();
    let expanded = trimmed
        .strip_prefix("~/")
        .map(|relative| home.join(relative))
        .unwrap_or_else(|| PathBuf::from(trimmed));
    for legacy_root in legacy_roots {
        let Ok(relative) = expanded.strip_prefix(legacy_root) else {
            continue;
        };
        let destination = destination_root.join(relative);
        *value = destination.display().to_string();
        return Some((expanded, destination));
    }
    None
}

/// Carries files left behind at a remapped output directory over to the new
/// one. Best effort: a file that cannot be moved is left where it is rather
/// than failing a config load over it.
fn relocate_managed_output(from: &Path, to: &Path) {
    if from == to || !from.is_dir() {
        return;
    }
    let Ok(entries) = std::fs::read_dir(from) else {
        return;
    };
    let mut moved = 0usize;
    for entry in entries.flatten() {
        let target = to.join(entry.file_name());
        if target.exists() {
            continue;
        }
        if std::fs::create_dir_all(to).is_err() {
            return;
        }
        if std::fs::rename(entry.path(), &target).is_ok() {
            moved += 1;
        }
    }
    if moved > 0 {
        // Only prunes when it empties out; anything left is someone else's.
        let _ = std::fs::remove_dir(from);
        tracing::info!(
            from = %from.display(),
            to = %to.display(),
            moved,
            "{}",
            crate::i18n::text(
                "moved files from a stale managed output directory",
                "已把过时输出目录里的文件搬到新位置",
            )
        );
    }
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

/// Compact trigger watermark. 0.8 (was 0.9) leaves room between the trigger
/// and the force watermark for the cheap mechanical layer to act first.
fn default_tool_output_spill_bytes() -> usize {
    50_000
}

fn default_trim_at_ratio() -> f32 {
    0.8
}

fn default_compact_force_ratio() -> f32 {
    0.9
}

fn default_compact_soft_ratio() -> f32 {
    0.5
}

fn default_compact_snip_ratio() -> f32 {
    0.6
}

fn default_cold_prune_after_minutes() -> u64 {
    1440
}

fn default_trim_batch_ratio() -> f32 {
    0.15
}

fn default_on_overflow() -> String {
    "compact".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_stale_xdg_output_dir_is_healed_and_its_files_follow() {
        // The value being healed is one an earlier upgrade wrote itself: it
        // remapped onto data_dir while data_dir still pointed at the legacy
        // XDG root, so the old root has to be a legacy root too.
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path();
        let legacy = home.join(".local/share/miyu/pictures/generated-images");
        std::fs::create_dir_all(&legacy).unwrap();
        std::fs::write(legacy.join("one.png"), "a").unwrap();
        std::fs::write(legacy.join("two.png"), "b").unwrap();

        let destination_root = home.join(".miyu/data/pictures");
        let mut value = legacy.display().to_string();
        let moved = remap_managed_output_dir(
            &mut value,
            &[home.join(".local/share/miyu/pictures")],
            &destination_root,
            home,
        );
        let (from, to) = moved.expect("the stale root must be recognised");
        assert_eq!(to, destination_root.join("generated-images"));
        assert_eq!(value, to.display().to_string());

        relocate_managed_output(&from, &to);
        assert!(to.join("one.png").exists());
        assert!(to.join("two.png").exists());
        assert!(
            !from.exists(),
            "an emptied stale directory should not linger"
        );
    }

    #[test]
    fn a_path_outside_every_legacy_root_is_left_alone() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path();
        let mut value = home.join("my-own-folder").display().to_string();
        let before = value.clone();
        let moved = remap_managed_output_dir(
            &mut value,
            &[home.join(".local/share/miyu/pictures")],
            &home.join(".miyu/data/pictures"),
            home,
        );
        assert!(moved.is_none());
        assert_eq!(value, before);
    }

    #[test]
    fn api_quota_partial_provider_configs_keep_defaults() {
        let config: ApiQuotaPluginConfig = serde_json::from_value(serde_json::json!({
            "deepseek": { "api_key": "deepseek-key" },
            "openrouter": { "api_key": "openrouter-key" }
        }))
        .unwrap();
        assert!(config.enabled);
        assert_eq!(config.deepseek.api_key, "deepseek-key");
        assert_eq!(config.openrouter.api_key, "openrouter-key");
    }

    #[test]
    fn api_quota_legacy_key_migrates_to_a_stable_default_account() {
        let mut config = AppConfig::default();
        config.plugins.api_quota.deepseek.accounts.clear();
        config.plugins.api_quota.deepseek.api_key = "legacy-key".to_string();
        config.normalize_api_quota_accounts();
        assert!(config.plugins.api_quota.deepseek.api_key.is_empty());
        assert_eq!(config.plugins.api_quota.deepseek.accounts.len(), 1);
        assert_eq!(
            config.plugins.api_quota.deepseek.accounts[0].id,
            "account-1"
        );
        assert_eq!(
            config.plugins.api_quota.deepseek.accounts[0].api_key,
            "legacy-key"
        );
    }

    #[test]
    fn api_quota_mixed_config_preserves_both_keys() {
        let mut config = ApiQuotaProviderConfig::default();
        config.accounts[0].api_key = "new-key".to_string();
        config.api_key = "legacy-key".to_string();
        normalize_api_quota_provider(&mut config);
        assert!(config.api_key.is_empty());
        assert_eq!(config.accounts.len(), 2);
        assert_eq!(config.accounts[0].api_key, "new-key");
        assert_eq!(config.accounts[1].api_key, "legacy-key");
        assert_ne!(config.accounts[0].id, config.accounts[1].id);
    }

    #[test]
    fn api_quota_account_names_must_be_unique() {
        let mut config = AppConfig::default();
        config.plugins.api_quota.deepseek.accounts = vec![
            ApiQuotaAccountConfig {
                id: "first".to_string(),
                name: "账号".to_string(),
                api_key: "first".to_string(),
            },
            ApiQuotaAccountConfig {
                id: "second".to_string(),
                name: "账号".to_string(),
                api_key: "second".to_string(),
            },
        ];
        assert!(config.validate().is_err());
    }

    #[test]
    fn context_overflow_defaults_to_compact() {
        assert_eq!(ContextConfig::default().on_overflow, "compact");

        let deserialized: ContextConfig = serde_json::from_value(serde_json::json!({})).unwrap();
        assert_eq!(deserialized.on_overflow, "compact");
    }

    #[test]
    fn vision_timeouts_have_stable_defaults() {
        let vision: VisionPluginConfig = serde_json::from_value(serde_json::json!({})).unwrap();
        assert_eq!(vision.response_header_timeout_seconds, 15);
        assert_eq!(vision.stream_idle_timeout_seconds, 20);
        assert_eq!(vision.image_timeout_seconds, 60);
    }

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
        let provider_id = config.providers[0].id.clone();
        config.providers[0].models.clear();
        config.providers[0].default_model.clear();

        assert!(!config
            .provider_model_choices()
            .iter()
            .any(|choice| choice.provider_id == provider_id));
    }

    #[test]
    fn active_provider_models_are_replaced_as_one_validated_pool() {
        let mut config = AppConfig::default();
        let provider_id = config.providers[0].id.clone();
        config.providers[0].models = vec!["model-a".to_string(), "model-b".to_string()];
        config.providers[0].default_model = "model-a".to_string();
        let before = config.active_provider_models.clone();

        let invalid = vec![
            ActiveProviderModelConfig {
                provider_id: provider_id.clone(),
                model: "model-a".to_string(),
            },
            ActiveProviderModelConfig {
                provider_id: provider_id.clone(),
                model: "missing".to_string(),
            },
        ];
        assert!(config.set_active_provider_models(&invalid).is_err());
        assert_eq!(config.active_provider_models, before);

        let selected = vec![
            ActiveProviderModelConfig {
                provider_id: provider_id.clone(),
                model: "model-b".to_string(),
            },
            ActiveProviderModelConfig {
                provider_id,
                model: "model-a".to_string(),
            },
        ];
        config.set_active_provider_models(&selected).unwrap();
        assert_eq!(
            config.active_provider_models.as_deref(),
            Some(selected.as_slice())
        );
    }

    #[test]
    fn legacy_provider_temperatures_migrate_once() {
        let mut config = AppConfig {
            config_version: 0,
            ..AppConfig::default()
        };
        config.providers[0].temperature = LEGACY_DEFAULT_TEMPERATURE;
        config.providers[1].temperature = 0.5;

        config.migrate().unwrap();

        assert_eq!(config.config_version, CURRENT_CONFIG_VERSION);
        assert_eq!(config.providers[0].temperature, 1.0);
        assert_eq!(config.providers[1].temperature, 0.5);

        config.providers[0].temperature = LEGACY_DEFAULT_TEMPERATURE;
        config.migrate().unwrap();
        assert_eq!(config.providers[0].temperature, LEGACY_DEFAULT_TEMPERATURE);

        config.config_version = CURRENT_CONFIG_VERSION + 1;
        assert!(config.migrate().is_err());
    }

    #[test]
    fn empty_active_provider_models_normalizes_to_default_chat_model() {
        let mut config = AppConfig::default();
        config.active_provider_models = Some(Vec::new());

        config.normalize_builtin_providers();

        let choices = config.active_provider_model_choices();
        assert_eq!(choices.len(), 1);
        assert_eq!(choices[0].provider_id, OPENCODE_PROVIDER_ID);
        assert_eq!(choices[0].model, OPENCODE_DEFAULT_CHAT_MODEL);
    }

    #[test]
    fn active_provider_model_choices_ignore_stale_models() {
        let mut config = AppConfig::default();
        let provider_id = config.providers[0].id.clone();
        config.providers[0].models = vec!["deepseek-v4-flash-free".to_string()];
        config.providers[0].default_model = "deepseek-v4-flash-free".to_string();
        config.active_provider_models = Some(vec![
            ActiveProviderModelConfig {
                provider_id: provider_id.clone(),
                model: "mimo-v2.5-free".to_string(),
            },
            ActiveProviderModelConfig {
                provider_id: provider_id.clone(),
                model: "deepseek-v4-flash-free".to_string(),
            },
        ]);

        let choices = config.active_provider_model_choices();

        assert_eq!(choices.len(), 1);
        assert_eq!(choices[0].provider_id, provider_id);
        assert_eq!(choices[0].model, "deepseek-v4-flash-free");
    }

    #[test]
    fn normalize_prunes_stale_active_provider_models() {
        let mut config = AppConfig::default();
        let provider_id = config.providers[0].id.clone();
        config.providers[0].models = vec!["deepseek-v4-flash-free".to_string()];
        config.providers[0].default_model = "deepseek-v4-flash-free".to_string();
        config.active_provider_models = Some(vec![
            ActiveProviderModelConfig {
                provider_id: provider_id.clone(),
                model: "mimo-v2.5-free".to_string(),
            },
            ActiveProviderModelConfig {
                provider_id: provider_id.clone(),
                model: "deepseek-v4-flash-free".to_string(),
            },
        ]);

        config.normalize_builtin_providers();

        assert_eq!(
            config.active_provider_models,
            Some(vec![ActiveProviderModelConfig {
                provider_id,
                model: "deepseek-v4-flash-free".to_string(),
            }])
        );
    }

    #[test]
    fn remove_active_model_references_clears_text_and_multimodal() {
        let mut config = AppConfig::default();
        let provider_id = config.providers[0].id.clone();
        config.active_provider_models = Some(vec![ActiveProviderModelConfig {
            provider_id: provider_id.clone(),
            model: "old-model".to_string(),
        }]);
        config.active_multimodal_provider_models = Some(vec![ActiveProviderModelConfig {
            provider_id: provider_id.clone(),
            model: "old-model".to_string(),
        }]);

        config.remove_active_model_references(&provider_id, "old-model");

        assert_eq!(config.active_provider_models, None);
        assert_eq!(config.active_multimodal_provider_models, None);
    }

    #[test]
    fn multimodal_provider_model_choices_use_input_modalities() {
        let mut config = AppConfig::default();
        let provider = &mut config.providers[0];
        provider.models = vec![
            "text-only".to_string(),
            "audio-only".to_string(),
            "vision-model".to_string(),
        ];
        provider
            .model_modalities
            .insert("text-only".to_string(), vec!["text".to_string()]);
        provider.model_modalities.insert(
            "audio-only".to_string(),
            vec!["text".to_string(), "audio".to_string()],
        );
        provider.model_modalities.insert(
            "vision-model".to_string(),
            vec!["text".to_string(), "image".to_string()],
        );

        let choices = config.multimodal_provider_model_choices();

        assert!(choices.iter().any(|choice| choice.model == "vision-model"));
        assert!(!choices.iter().any(|choice| choice.model == "text-only"));
        assert!(!choices.iter().any(|choice| choice.model == "audio-only"));
    }

    #[test]
    fn active_multimodal_pool_rejects_and_prunes_non_image_models() {
        let mut config = AppConfig::default();
        let provider_id = config.providers[0].id.clone();
        config.providers[0]
            .models
            .extend(["audio-only".to_string(), "vision-model".to_string()]);
        config.providers[0].model_modalities.insert(
            "audio-only".to_string(),
            vec!["text".to_string(), "audio".to_string()],
        );
        config.providers[0].model_modalities.insert(
            "vision-model".to_string(),
            vec!["text".to_string(), "image".to_string()],
        );

        assert!(config
            .toggle_active_multimodal_provider_model(&provider_id, "audio-only")
            .is_err());
        assert!(config
            .toggle_active_multimodal_provider_model(&provider_id, "vision-model")
            .unwrap());
        config
            .active_multimodal_provider_models
            .as_mut()
            .unwrap()
            .push(ActiveProviderModelConfig {
                provider_id,
                model: "audio-only".to_string(),
            });
        assert!(config.validate_global_multimodal_config().is_err());

        config.normalize_builtin_providers();

        let active = config.active_multimodal_provider_models.unwrap();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].model, "vision-model");
    }

    #[test]
    fn vision_provider_choice_prefers_multimodal_pool_then_default_mimo() {
        let mut config = AppConfig::default();
        config.providers[0].models.push("vision-model".to_string());
        config.providers[0].model_modalities.insert(
            "vision-model".to_string(),
            vec!["text".to_string(), "image".to_string()],
        );
        config.active_multimodal_provider_models = Some(vec![ActiveProviderModelConfig {
            provider_id: OPENCODE_PROVIDER_ID.to_string(),
            model: "vision-model".to_string(),
        }]);

        assert_eq!(
            config.vision_provider_choice().unwrap(),
            (OPENCODE_PROVIDER_ID.to_string(), "vision-model".to_string())
        );

        config.active_multimodal_provider_models = Some(Vec::new());
        assert_eq!(
            config.vision_provider_choice().unwrap(),
            (
                OPENCODE_PROVIDER_ID.to_string(),
                OPENCODE_DEFAULT_VISION_MODEL.to_string()
            )
        );
    }

    #[test]
    fn vision_provider_choice_rejects_an_audio_only_active_pool() {
        let mut config = AppConfig::default();
        let provider_id = config.providers[0].id.clone();
        config.providers[0].models.push("audio-only".to_string());
        config.providers[0].model_modalities.insert(
            "audio-only".to_string(),
            vec!["text".to_string(), "audio".to_string()],
        );
        config.active_multimodal_provider_models = Some(vec![ActiveProviderModelConfig {
            provider_id,
            model: "audio-only".to_string(),
        }]);

        assert!(config.vision_provider_choice().is_err());
        assert!(config.validate().is_err());
    }

    #[test]
    fn vision_provider_choice_rejects_an_explicit_non_image_model() {
        let mut config = AppConfig::default();
        let provider_id = config.providers[0].id.clone();
        config.providers[0].models.push("audio-only".to_string());
        config.providers[0].model_modalities.insert(
            "audio-only".to_string(),
            vec!["text".to_string(), "audio".to_string()],
        );
        config.plugins.vision.vision_provider_id = provider_id;
        config.plugins.vision.vision_model = "audio-only".to_string();

        assert!(config.vision_provider_choice().is_err());
        assert!(config.validate().is_err());
    }

    #[test]
    fn subagent_tier_pools_toggle_filter_and_prune() {
        let mut config = AppConfig::default();
        let provider_id = config.providers[0].id.clone();
        config.providers[0].models.push("mini-a".to_string());
        config.providers[0].models.push("mini-b".to_string());

        // Unconfigured pool resolves empty.
        assert!(config.subagent_tier_choices(ModelTier::Cheap).is_empty());

        // Toggle in/out mirrors the text-model picker semantics.
        assert!(config
            .toggle_subagent_tier_model(ModelTier::Cheap, &provider_id, "mini-a")
            .unwrap());
        assert!(config
            .toggle_subagent_tier_model(ModelTier::Cheap, &provider_id, "mini-b")
            .unwrap());
        assert!(config.is_subagent_tier_model(ModelTier::Cheap, &provider_id, "mini-a"));
        let choices = config.subagent_tier_choices(ModelTier::Cheap);
        assert_eq!(
            choices.iter().map(|c| c.model.as_str()).collect::<Vec<_>>(),
            vec!["mini-a", "mini-b"]
        );
        assert!(!config
            .toggle_subagent_tier_model(ModelTier::Cheap, &provider_id, "mini-b")
            .unwrap());
        assert_eq!(config.subagent_tier_choices(ModelTier::Cheap).len(), 1);

        // Unknown provider is rejected.
        assert!(config
            .toggle_subagent_tier_model(ModelTier::Strong, "no-such", "x")
            .is_err());

        // A model removed from the text models leaves the pool too.
        config
            .toggle_subagent_tier_model(ModelTier::Balanced, &provider_id, "mini-a")
            .unwrap();
        config
            .remove_active_provider_model(&provider_id, "mini-a")
            .unwrap();
        assert!(config.subagent_tier_choices(ModelTier::Cheap).is_empty());
        assert!(config.subagent_tiers.pool(ModelTier::Cheap).is_empty());
        assert!(config.subagent_tiers.pool(ModelTier::Balanced).is_empty());

        // prune_subagent_tiers drops entries that no longer resolve.
        config
            .toggle_subagent_tier_model(ModelTier::Cheap, &provider_id, "mini-b")
            .unwrap();
        config.providers[0].models.retain(|m| m != "mini-b");
        assert!(config.subagent_tier_choices(ModelTier::Cheap).is_empty());
        config.prune_subagent_tiers();
        assert!(config.subagent_tiers.pool(ModelTier::Cheap).is_empty());
    }

    #[test]
    fn subagent_tiers_roundtrip_and_default_omission() {
        let config = AppConfig::default();
        let json = serde_json::to_string(&config).unwrap();
        // Empty pools stay out of the serialized config.
        assert!(!json.contains("subagent_tiers"));

        let parsed: AppConfig = serde_json::from_str(
            r#"{
                "active_provider": "opencode",
                "providers": [],
                "subagent_tiers": {
                    "cheap": [ { "provider_id": "p", "model": "m" } ]
                }
            }"#,
        )
        .unwrap();
        assert_eq!(parsed.subagent_tiers.cheap.len(), 1);
        assert_eq!(parsed.subagent_tiers.cheap[0].model, "m");
        assert!(parsed.subagent_tiers.balanced.is_empty());
        // Choices filter out entries with unknown providers.
        assert!(parsed.subagent_tier_choices(ModelTier::Cheap).is_empty());
    }

    #[test]
    fn platforms_config_roundtrip_and_default_omission() {
        let config = AppConfig::default();
        let json = serde_json::to_string(&config).unwrap();
        // An untouched platforms config stays out of the serialized file.
        assert!(!json.contains("platforms"));

        let mut parsed: AppConfig = serde_json::from_str(
            r#"{
                "active_provider": "opencode",
                "providers": [],
                "platforms": {
                    "command_prefix": "!",
                    "commands": {
                        "reset": { "permission": "everyone" }
                    },
                    "qq": {
                        "enabled": true,
                        "reverse_ws_port": 8400,
                        "access_token": "secret",
                        "admin_users": [9988],
                        "asset_base_url": "https://assets.example.test",
                        "memory": {
                            "write_enabled": false
                        },
                        "private_chats": {
                            "whitelist": [12345],
                            "friend_requests_require_private_whitelist": false,
                            "allow_non_whitelist": false,
                            "non_whitelist_rate_per_minute": 4
                        },
                        "group_chats": {
                            "whitelist": [54321],
                            "trigger_keywords": ["Miyu"],
                            "whitelist_rate_per_minute": 30,
                            "allow_non_whitelist": true,
                            "non_whitelist_rate_per_minute": 10
                        }
                    }
                }
            }"#,
        )
        .unwrap();
        parsed.normalize_platform_model_routes();
        let qq = &parsed.platforms.qq;
        assert_eq!(parsed.platforms.command_prefix, "!");
        assert_eq!(
            parsed
                .platforms
                .command_permission("reset", PlatformCommandPermission::AdminOnly),
            PlatformCommandPermission::Everyone
        );
        assert!(qq.enabled);
        assert_eq!(qq.reverse_ws_port, 8400);
        assert_eq!(qq.access_token, "secret");
        assert_eq!(qq.admin_users, vec![9988]);
        assert!(qq.user_identification);
        assert!(qq.show_group_name);
        assert!(!qq.memory.write_enabled);
        assert_eq!(qq.asset_base_url, "https://assets.example.test");
        assert_eq!(qq.private_chats.whitelist, vec![12345]);
        assert!(!qq.private_chats.friend_requests_require_private_whitelist);
        assert!(!qq.private_chats.allow_non_whitelist);
        assert_eq!(
            qq.private_chats.non_whitelist_rate_limit,
            PlatformRateLimit {
                max_messages: 4,
                window_seconds: 60,
            }
        );
        assert_eq!(qq.group_chats.whitelist, vec![54321]);
        assert_eq!(qq.group_chats.trigger_keywords, vec!["Miyu"]);
        assert_eq!(qq.group_chats.whitelist_rate_limit.max_messages, 30);
        assert_eq!(qq.group_chats.non_whitelist_rate_limit.max_messages, 10);
        assert_eq!(qq.max_reply_chars, 3000);

        // Round-trip preserves the non-default config.
        let json = serde_json::to_string(&parsed).unwrap();
        let reparsed: AppConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(reparsed.platforms, parsed.platforms);

        // The retired protocol-shaped key is a clean break and does not
        // silently enable Tencent QQ under the new defaults.
        let legacy: AppConfig = serde_json::from_str(
            r#"{"active_provider":"opencode","providers":[],"platforms":{"onebot":{"enabled":true}}}"#,
        )
        .unwrap();
        assert!(!legacy.platforms.qq.enabled);
        assert_eq!(legacy.platforms.command_prefix, "/");
        assert!(legacy.platforms.commands.is_empty());

        let missing_friend_request_setting: AppConfig = serde_json::from_str(
            r#"{
                "active_provider": "opencode",
                "providers": [],
                "platforms": {
                    "qq": {
                        "private_chats": { "whitelist": [12345] }
                    }
                }
            }"#,
        )
        .unwrap();
        assert!(
            missing_friend_request_setting
                .platforms
                .qq
                .private_chats
                .friend_requests_require_private_whitelist
        );
    }

    #[test]
    fn qq_prompt_identity_options_default_on_and_roundtrip() {
        let defaults: OneBotConfig = serde_json::from_str("{}").unwrap();
        assert!(defaults.user_identification);
        assert!(defaults.show_group_name);
        assert!(defaults.memory.write_enabled);

        let mut disabled = OneBotConfig::default();
        disabled.user_identification = false;
        disabled.show_group_name = false;
        let json = serde_json::to_value(&disabled).unwrap();
        assert_eq!(json["user_identification"], false);
        assert_eq!(json["show_group_name"], false);
        assert_eq!(
            serde_json::from_value::<OneBotConfig>(json).unwrap(),
            disabled
        );
    }

    #[test]
    fn platform_command_defaults_overrides_and_validation() {
        let mut config = AppConfig::default();
        assert_eq!(config.platforms.command_prefix, "/");
        assert_eq!(
            config
                .platforms
                .command_permission("reset", PlatformCommandPermission::AdminOnly),
            PlatformCommandPermission::AdminOnly
        );
        config.platforms.set_command_permission(
            "reset",
            PlatformCommandPermission::Everyone,
            PlatformCommandPermission::AdminOnly,
        );
        assert_eq!(
            config.platforms.commands["reset"].permission,
            PlatformCommandPermission::Everyone
        );
        config.platforms.set_command_permission(
            "reset",
            PlatformCommandPermission::AdminOnly,
            PlatformCommandPermission::AdminOnly,
        );
        assert!(config.platforms.commands.is_empty());

        for invalid in [
            "",
            " ",
            "/ reset",
            "\n",
            "/////////////////////////////////",
        ] {
            config.platforms.command_prefix = invalid.to_string();
            assert!(
                config.validate().is_err(),
                "prefix should be invalid: {invalid:?}"
            );
        }
        config.platforms.command_prefix = "/".to_string();
        config
            .platforms
            .commands
            .insert("Reset".to_string(), PlatformCommandConfig::default());
        assert!(config.validate().is_err());
    }

    fn route_test_config() -> AppConfig {
        let mut config = AppConfig::default();
        let provider = &mut config.providers[0];
        provider.models = vec!["text-only".to_string(), "vision".to_string()];
        provider.default_model = "text-only".to_string();
        provider
            .model_modalities
            .insert("text-only".to_string(), vec!["text".to_string()]);
        provider.model_modalities.insert(
            "vision".to_string(),
            vec!["text".to_string(), "image".to_string()],
        );
        config
    }

    fn test_route(config: &AppConfig) -> PlatformModelRoute {
        PlatformModelRoute {
            conversation: PlatformConversationConfig {
                kind: PlatformConversationKind::Group,
                id: "20002".to_string(),
            },
            persona: PlatformPersonaOverride::Inherit,
            text_models_inheritance: PlatformModelPoolInheritance::Platform,
            text_models: Some(vec![ActiveProviderModelConfig {
                provider_id: config.providers[0].id.clone(),
                model: "text-only".to_string(),
            }]),
            multimodal_models_inheritance: PlatformModelPoolInheritance::Platform,
            multimodal_models: Some(vec![ActiveProviderModelConfig {
                provider_id: config.providers[0].id.clone(),
                model: "vision".to_string(),
            }]),
            extra_prompt: "Reply naturally in this group.".to_string(),
            session_limits: None,
        }
    }

    #[test]
    fn qq_platform_model_pools_validate_and_round_trip() {
        let mut config = route_test_config();
        let provider_id = config.providers[0].id.clone();
        config.platforms.qq.text_models = Some(vec![ActiveProviderModelConfig {
            provider_id: provider_id.clone(),
            model: "text-only".to_string(),
        }]);
        config.platforms.qq.non_whitelist_text_models = Some(vec![ActiveProviderModelConfig {
            provider_id: provider_id.clone(),
            model: "text-only".to_string(),
        }]);
        config.platforms.qq.multimodal_models = Some(vec![ActiveProviderModelConfig {
            provider_id,
            model: "vision".to_string(),
        }]);

        assert!(config.validate().is_ok());
        let value = serde_json::to_value(&config).unwrap();
        let reparsed: AppConfig = serde_json::from_value(value).unwrap();
        assert_eq!(
            reparsed.platforms.qq.text_models,
            config.platforms.qq.text_models
        );
        assert_eq!(
            reparsed.platforms.qq.multimodal_models,
            config.platforms.qq.multimodal_models
        );
        assert_eq!(
            reparsed.platforms.qq.non_whitelist_text_models,
            config.platforms.qq.non_whitelist_text_models
        );

        config.platforms.qq.multimodal_models.as_mut().unwrap()[0].model = "text-only".to_string();
        assert!(config.validate().is_err());
        config.platforms.qq.multimodal_models.as_mut().unwrap()[0].model = "vision".to_string();
        config
            .platforms
            .qq
            .non_whitelist_text_models
            .as_mut()
            .unwrap()[0]
            .model = "missing".to_string();
        assert!(config.validate().is_err());
    }

    #[test]
    fn qq_non_whitelist_model_pool_normalizes_for_dynamic_inheritance() {
        let mut config = route_test_config();
        let provider_id = config.providers[0].id.clone();
        config.platforms.qq.non_whitelist_text_models = Some(vec![
            ActiveProviderModelConfig {
                provider_id: format!(" {provider_id} "),
                model: " text-only ".to_string(),
            },
            ActiveProviderModelConfig {
                provider_id: provider_id.clone(),
                model: "text-only".to_string(),
            },
        ]);

        config.normalize_platform_model_routes();
        assert_eq!(
            config
                .platforms
                .qq
                .non_whitelist_text_models
                .as_ref()
                .unwrap()
                .len(),
            1
        );

        config.platforms.qq.non_whitelist_text_models = Some(Vec::new());
        config.normalize_platform_model_routes();
        assert!(config.platforms.qq.non_whitelist_text_models.is_none());
    }

    #[test]
    fn qq_session_limits_resolve_from_conversation_then_kind_then_platform() {
        let mut qq = OneBotConfig::default();
        assert_eq!(qq.session_limits.running, 8);
        assert_eq!(qq.session_limits.queued, 16);
        qq.session_limits = PlatformSessionLimits {
            running: 2,
            queued: 3,
        };
        qq.group_chats.session_limits = Some(PlatformSessionLimits {
            running: 3,
            queued: 5,
        });
        qq.conversations.push(PlatformModelRoute {
            conversation: PlatformConversationConfig {
                kind: PlatformConversationKind::Group,
                id: "42".to_string(),
            },
            persona: PlatformPersonaOverride::Inherit,
            text_models_inheritance: PlatformModelPoolInheritance::Platform,
            text_models: None,
            multimodal_models_inheritance: PlatformModelPoolInheritance::Platform,
            multimodal_models: None,
            extra_prompt: String::new(),
            session_limits: Some(PlatformSessionLimits {
                running: 4,
                queued: 7,
            }),
        });
        assert_eq!(
            qq.session_limits(PlatformConversationKind::Group, "42"),
            PlatformSessionLimits {
                running: 4,
                queued: 7
            }
        );
        assert_eq!(
            qq.session_limits(PlatformConversationKind::Group, "43"),
            PlatformSessionLimits {
                running: 3,
                queued: 5
            }
        );
        assert_eq!(
            qq.session_limits(PlatformConversationKind::Private, "42"),
            PlatformSessionLimits {
                running: 2,
                queued: 3
            }
        );
    }

    #[test]
    fn qq_text_model_pool_resolution_preserves_conversation_priority() {
        let mut config = route_test_config();
        let provider_id = config.providers[0].id.clone();
        let pool = |model: &str| {
            vec![ActiveProviderModelConfig {
                provider_id: provider_id.clone(),
                model: model.to_string(),
            }]
        };
        config.active_provider_models = Some(pool("global"));
        config.active_multimodal_provider_models = Some(pool("global-media"));
        config.platforms.qq.text_models = Some(pool("platform"));
        config.platforms.qq.multimodal_models = Some(pool("platform-media"));
        config.platforms.qq.non_whitelist_text_models = Some(pool("non-whitelist"));
        config.platforms.qq.conversations.push(PlatformModelRoute {
            conversation: PlatformConversationConfig {
                kind: PlatformConversationKind::Group,
                id: "20002".to_string(),
            },
            persona: PlatformPersonaOverride::Inherit,
            text_models_inheritance: PlatformModelPoolInheritance::Platform,
            text_models: Some(pool("conversation")),
            multimodal_models_inheritance: PlatformModelPoolInheritance::Platform,
            multimodal_models: None,
            extra_prompt: String::new(),
            session_limits: None,
        });

        {
            let resolved = |conversation_id, use_non_whitelist_pool| {
                config
                    .qq_text_model_pool(
                        PlatformConversationKind::Group,
                        conversation_id,
                        use_non_whitelist_pool,
                    )
                    .unwrap()[0]
                    .model
                    .as_str()
            };
            assert_eq!(resolved("20002", true), "conversation");
            assert_eq!(resolved("30003", true), "non-whitelist");
            assert_eq!(resolved("30003", false), "platform");
        }
        assert_eq!(
            config
                .qq_multimodal_model_pool(PlatformConversationKind::Group, "20002")
                .unwrap()[0]
                .model,
            "platform-media"
        );
        let route = &mut config.platforms.qq.conversations[0];
        route.text_models = None;
        route.text_models_inheritance = PlatformModelPoolInheritance::Global;
        assert_eq!(
            config
                .qq_text_model_pool(PlatformConversationKind::Group, "20002", true)
                .unwrap()[0]
                .model,
            "global"
        );
        assert_eq!(
            config
                .qq_multimodal_model_pool(PlatformConversationKind::Group, "20002")
                .unwrap()[0]
                .model,
            "platform-media"
        );
        config.platforms.qq.conversations[0].multimodal_models_inheritance =
            PlatformModelPoolInheritance::Global;
        assert_eq!(
            config
                .qq_multimodal_model_pool(PlatformConversationKind::Group, "20002")
                .unwrap()[0]
                .model,
            "global-media"
        );
        config.platforms.qq.non_whitelist_text_models = None;
        assert_eq!(
            config
                .qq_text_model_pool(PlatformConversationKind::Group, "30003", true)
                .unwrap()[0]
                .model,
            "platform"
        );
        config.platforms.qq.text_models = None;
        assert_eq!(
            config
                .qq_text_model_pool(PlatformConversationKind::Group, "30003", true)
                .unwrap()[0]
                .model,
            "global"
        );
    }

    #[test]
    fn qq_model_pool_inheritance_is_backward_compatible_and_round_trips() {
        let mut route: PlatformModelRoute = serde_json::from_value(serde_json::json!({
            "conversation": { "kind": "private", "id": "42" }
        }))
        .unwrap();
        assert_eq!(
            route.text_models_inheritance,
            PlatformModelPoolInheritance::Platform
        );
        assert_eq!(
            route.multimodal_models_inheritance,
            PlatformModelPoolInheritance::Platform
        );
        let legacy_value = serde_json::to_value(&route).unwrap();
        assert!(legacy_value.get("text_models_inheritance").is_none());
        assert!(legacy_value.get("multimodal_models_inheritance").is_none());

        route.text_models_inheritance = PlatformModelPoolInheritance::Global;
        route.multimodal_models_inheritance = PlatformModelPoolInheritance::Global;
        let value = serde_json::to_value(&route).unwrap();
        assert_eq!(value["text_models_inheritance"], "global");
        assert_eq!(value["multimodal_models_inheritance"], "global");
        assert_eq!(
            serde_json::from_value::<PlatformModelRoute>(value).unwrap(),
            route
        );
    }

    #[test]
    fn qq_conversation_persona_override_is_explicit_and_tracks_renames() {
        let mut config = route_test_config();
        config.prompt.active_persona = "Global.md".to_string();
        let mut route = test_route(&config);
        route.persona = PlatformPersonaOverride::Custom {
            name: "Group.md".to_string(),
        };
        config.platforms.qq.conversations.push(route);

        let mut effective = config.clone();
        effective.apply_qq_conversation_persona(PlatformConversationKind::Group, "20002");
        assert_eq!(effective.prompt.active_persona, "Group.md");
        assert_eq!(config.platforms.persona_reference_count("Group.md"), 1);

        config
            .platforms
            .rename_persona_references("Group.md", "Renamed.md");
        assert_eq!(
            config.platforms.qq.conversations[0].persona.custom_name(),
            Some("Renamed.md")
        );
        assert!(config.validate().is_ok());

        config.platforms.qq.conversations[0].persona = PlatformPersonaOverride::Miyu;
        config.apply_qq_conversation_persona(PlatformConversationKind::Group, "20002");
        assert!(config.prompt.active_persona.is_empty());
    }

    #[test]
    fn qq_conversation_persona_rejects_unsafe_custom_names() {
        let mut config = route_test_config();
        let mut route = test_route(&config);
        route.persona = PlatformPersonaOverride::Custom {
            name: "../persona.md".to_string(),
        };
        config.platforms.qq.conversations.push(route);
        assert!(config.validate().is_err());
    }

    #[test]
    fn platform_model_routes_roundtrip_lookup_and_plugin_shape() {
        let mut config = route_test_config();
        let route = test_route(&config);
        config.platforms.upsert_model_route(route.clone());
        config.platforms.qq.plugins.insert(
            "reply_processor".to_string(),
            PlatformPluginInstanceConfig {
                enabled: Some(false),
                settings: serde_json::json!({"threshold": 150})
                    .as_object()
                    .unwrap()
                    .clone(),
            },
        );

        let found = config
            .platform_model_route(PlatformConversationKind::Group, "20002")
            .unwrap();
        assert_eq!(found, &route);
        assert!(config.validate().is_ok());

        let json = serde_json::to_string(&config).unwrap();
        let reparsed: AppConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(reparsed.platforms, config.platforms);
        assert_eq!(
            reparsed.platforms.qq.plugins["reply_processor"].enabled,
            Some(false)
        );
        assert_eq!(
            reparsed.platforms.qq.plugins["reply_processor"].settings["threshold"],
            150
        );
    }

    #[test]
    fn built_in_platform_plugin_settings_are_validated() {
        let mut config = AppConfig::default();
        config.platforms.qq.plugins.insert(
            "reply_processor".to_string(),
            PlatformPluginInstanceConfig {
                enabled: None,
                settings: serde_json::json!({"threshold": 0, "mode": "invalid"})
                    .as_object()
                    .unwrap()
                    .clone(),
            },
        );
        assert!(config.validate().is_err());

        config
            .platforms
            .qq
            .plugins
            .get_mut("reply_processor")
            .unwrap()
            .settings = serde_json::json!({
            "threshold": 150,
            "mode": "image",
            "future_option": 1
        })
        .as_object()
        .unwrap()
        .clone();
        assert!(config.validate().is_ok());

        config.platforms.qq.plugins.insert(
            QQ_MEME_COLLECTOR_PLUGIN_ID.to_string(),
            PlatformPluginInstanceConfig {
                enabled: Some(true),
                settings: serde_json::json!({
                    "collect_probability": 0.02,
                    "max_images_per_message": 2
                })
                .as_object()
                .unwrap()
                .clone(),
            },
        );
        assert!(config.validate().is_ok());
        config
            .platforms
            .qq
            .plugins
            .get_mut(QQ_MEME_COLLECTOR_PLUGIN_ID)
            .unwrap()
            .settings
            .insert("collect_probability".to_string(), serde_json::json!(1.01));
        assert!(config.validate().is_err());
    }

    #[test]
    fn qq_meme_collector_defaults_are_conservative() {
        let settings = QqMemeCollectorPluginSettings::default();
        assert_eq!(settings.collect_probability, 0.02);
        assert_eq!(settings.max_images_per_message, 2);
        assert!(!settings.allow_non_admin_save_tool);
    }

    #[test]
    fn qq_message_history_defaults_to_full_text_recording() {
        let settings = QqMessageHistoryPluginSettings::default();

        assert_eq!(settings.history_search_max_results, 0);
        assert_eq!(settings.history_safe_page_limit, 500);
        assert!(settings.allow_cross_conversation_search);
        assert!(settings.validate().is_ok());
    }

    #[test]
    fn legacy_real_context_history_limits_move_to_message_history() {
        let mut config = AppConfig::default();
        config.platforms.qq.plugins.insert(
            REAL_CONTEXT_PLUGIN_ID.to_string(),
            PlatformPluginInstanceConfig {
                enabled: None,
                settings: serde_json::json!({
                    "history_search_max_results": 25,
                    "history_safe_page_limit": 250,
                    "allow_cross_group_search": false
                })
                .as_object()
                .unwrap()
                .clone(),
            },
        );

        config.normalize_platform_model_routes();

        let history = QqMessageHistoryPluginSettings::from_instance(
            &config.platforms.qq.plugins[QQ_MESSAGE_HISTORY_PLUGIN_ID],
        )
        .unwrap();
        assert_eq!(history.history_search_max_results, 25);
        assert_eq!(history.history_safe_page_limit, 250);
        assert!(!history.allow_cross_conversation_search);
        assert!(config
            .platforms
            .qq
            .plugins
            .get(REAL_CONTEXT_PLUGIN_ID)
            .is_none());
    }

    #[test]
    fn real_context_defaults_match_the_deployed_contract() {
        let settings = RealContextPluginSettings::default();

        assert_eq!(settings.reply_context_window, 25);
        assert_eq!(settings.judge_context_window, 20);
        assert_eq!(settings.group_member_search_max_results, 200);
        assert!(settings.active_reply_enable);
        assert!(settings.judge_include_persona);
        assert!(settings.judge_persona_prompt.is_empty());
        assert!(settings.text_models.is_none());
        assert_eq!(settings.active_judge_probability, 0.05);
        assert_eq!(settings.reply_threshold, 0.8);
        assert_eq!(settings.judge_timeout_seconds, 60);
        assert_eq!(settings.judge_endpoint_timeout_seconds, 15);
        assert_eq!(settings.judge_max_concurrency, 4);
        assert_eq!(settings.judge_max_retries, 1);
        assert_eq!(settings.active_reply_supersede_window_seconds, 5);
        assert_eq!(settings.continuation_window_seconds, 15);
        assert!(settings.takeover_direct_trigger_enable);
        assert_eq!(settings.takeover_direct_trigger_boost_score, 0.3);
        assert!(settings.privileged_direct_trigger_skip_active_judgement);
        assert_eq!(settings.active_reply_reaction_emoji_ids, [289]);
        assert_eq!(settings.active_reply_reaction_timeout_seconds, 600);
        assert!(settings.reply_target_quote_enable);
        assert_eq!(settings.reply_target_quote_after_other_messages, 4);
        assert!(settings.reply_target_mention_enable);
        assert_eq!(settings.reply_target_mention_after_seconds, 15);
        assert_eq!(settings.moderation_min_severity, 7.0);
        assert_eq!(settings.base64_moderation_min_chars, 24);
        assert_eq!(settings.base64_moderation_max_decoded_chars, 5_000);
        assert_eq!(settings.base64_moderation_min_printable_ratio, 0.85);
        assert_eq!(settings.moderation_keywords.len(), 175);
        assert!(settings.identity_mappings.is_empty());
        assert!(!settings.affection_enable);
        assert!(settings.validate().is_ok());
    }

    #[test]
    fn qq_default_non_whitelist_rate_limits_match_the_deployed_contract() {
        let qq = OneBotConfig::default();

        assert_eq!(
            qq.private_chats.non_whitelist_rate_limit,
            PlatformRateLimit {
                max_messages: 2,
                window_seconds: 600,
            }
        );
        assert_eq!(
            qq.group_chats.non_whitelist_rate_limit,
            PlatformRateLimit {
                max_messages: 2,
                window_seconds: 600,
            }
        );

        let explicit: OneBotConfig = serde_json::from_value(serde_json::json!({
            "private_chats": {
                "non_whitelist_rate_limit": {
                    "max_messages": 1,
                    "window_seconds": 120
                }
            },
            "group_chats": {
                "non_whitelist_rate_limit": {
                    "max_messages": 5,
                    "window_seconds": 60
                }
            }
        }))
        .unwrap();
        assert_eq!(
            explicit.private_chats.non_whitelist_rate_limit.max_messages,
            1
        );
        assert_eq!(
            explicit
                .private_chats
                .non_whitelist_rate_limit
                .window_seconds,
            120
        );
        assert_eq!(
            explicit.group_chats.non_whitelist_rate_limit.max_messages,
            5
        );
        assert_eq!(
            explicit.group_chats.non_whitelist_rate_limit.window_seconds,
            60
        );
    }

    #[test]
    fn real_context_migrates_group_member_page_size_to_search_max_results() {
        let mut instance = PlatformPluginInstanceConfig {
            enabled: None,
            settings: serde_json::json!({ "group_member_page_size": 17 })
                .as_object()
                .unwrap()
                .clone(),
        };

        let settings = RealContextPluginSettings::from_instance(&instance).unwrap();
        assert_eq!(settings.group_member_search_max_results, 17);

        merge_real_context_settings(&mut instance, &settings);
        assert_eq!(instance.settings["group_member_search_max_results"], 17);
        assert!(!instance.settings.contains_key("group_member_page_size"));
    }

    #[test]
    fn real_context_migrates_continuation_minutes_to_seconds() {
        let mut former_default = PlatformPluginInstanceConfig {
            enabled: None,
            settings: serde_json::json!({ "continuation_window_minutes": 3 })
                .as_object()
                .unwrap()
                .clone(),
        };
        let settings = RealContextPluginSettings::from_instance(&former_default).unwrap();
        // The old default must land on the current one, whatever that is.
        assert_eq!(
            settings.continuation_window_seconds,
            RealContextPluginSettings::default().continuation_window_seconds
        );
        merge_real_context_settings(&mut former_default, &settings);
        assert!(!former_default
            .settings
            .contains_key("continuation_window_minutes"));
        assert!(!former_default
            .settings
            .contains_key("continuation_window_seconds"));

        let mut custom = PlatformPluginInstanceConfig {
            enabled: None,
            settings: serde_json::json!({ "continuation_window_minutes": 7 })
                .as_object()
                .unwrap()
                .clone(),
        };
        let settings = RealContextPluginSettings::from_instance(&custom).unwrap();
        assert_eq!(settings.continuation_window_seconds, 420);
        merge_real_context_settings(&mut custom, &settings);
        assert_eq!(custom.settings["continuation_window_seconds"], 420);
        assert!(!custom.settings.contains_key("continuation_window_minutes"));
    }

    #[test]
    fn an_embedding_model_never_reaches_the_chat_pickers() {
        // It produces vectors, not replies; the multimodal list derives from
        // the text one, so filtering at the source covers both.
        let mut config = AppConfig::default();
        let provider = config.providers.first_mut().unwrap();
        provider.models = vec!["chat-model".to_string(), "vector-model".to_string()];
        provider
            .model_modalities
            .insert("chat-model".to_string(), vec!["text".to_string()]);
        provider.model_modalities.insert(
            "vector-model".to_string(),
            vec![EMBEDDING_MODALITY.to_string()],
        );

        let text: Vec<String> = config
            .text_provider_model_choices()
            .into_iter()
            .map(|choice| choice.model)
            .collect();
        assert!(text.contains(&"chat-model".to_string()), "{text:?}");
        assert!(!text.contains(&"vector-model".to_string()), "{text:?}");
    }

    #[test]
    fn the_embedding_model_moves_out_from_under_the_knowledge_base() {
        // It was configured there because that is where it was first needed;
        // it now also backs memory recall, and a knowledge-base setting quietly
        // steering group-chat search is a trap for whoever reads this next.
        let mut config = AppConfig::default();
        config.plugins.knowledge_base.embedding_provider_id = "omlx".to_string();
        config.plugins.knowledge_base.embedding_model = "bge-m3".to_string();
        config.plugins.knowledge_base.embedding_timeout_seconds = 45;
        config.plugins.knowledge_base.semantic_min_score = 0.5;
        config.config_version = 0;
        config.migrate().unwrap();
        assert_eq!(config.embedding.provider_id, "omlx");
        assert_eq!(config.embedding.model, "bge-m3");
        assert_eq!(config.embedding.timeout_seconds, 45);
        assert!((config.embedding.min_score - 0.5).abs() < f32::EPSILON);

        // Configuring a model only makes it available; there is no switch.
        assert!(config.embedding.is_configured());
        assert!(!AppConfig::default().embedding.is_configured());
    }

    #[test]
    fn a_legacy_shared_window_seeds_both_new_windows() {
        // One knob used to drive both the reply turn and the judge. Their best
        // values point opposite ways — the reply wants a generous opening
        // snapshot, the judge a tight recent window — so the knob split, and an
        // existing config has to land on its old value for both rather than
        // silently jumping to the new defaults.
        let mut settings = serde_json::Map::new();
        settings.insert("context_messages".to_string(), serde_json::json!(12));
        migrate_real_context_settings_map(&mut settings);
        assert_eq!(settings["reply_context_window"], 12);
        assert_eq!(settings["judge_context_window"], 12);

        // An explicit new value wins over the legacy one.
        let mut settings = serde_json::Map::new();
        settings.insert("context_messages".to_string(), serde_json::json!(12));
        settings.insert("judge_context_window".to_string(), serde_json::json!(30));
        migrate_real_context_settings_map(&mut settings);
        assert_eq!(settings["reply_context_window"], 12);
        assert_eq!(settings["judge_context_window"], 30);
    }

    #[test]
    fn real_context_legacy_settings_migrate_and_deprecated_keys_are_removed() {
        let mut instance = PlatformPluginInstanceConfig {
            enabled: None,
            settings: serde_json::json!({
                "reply_context_messages": 37,
                "active_context_messages": 5,
                "takeover_system_trigger_enable": true,
                "takeover_system_trigger_boost_score": 0.4,
                "judge_models": [{"provider_id": "judge", "model": "primary"}],
                "affection_judge_models": [{"provider_id": "affection", "model": "secondary"}],
                "activity_statistics_enable": false,
                "future_option": {"value": 1}
            })
            .as_object()
            .unwrap()
            .clone(),
        };

        let settings = RealContextPluginSettings::from_instance(&instance).unwrap();
        assert_eq!(settings.reply_context_window, 37);
        assert_eq!(settings.judge_context_window, 37);
        assert!(settings.takeover_direct_trigger_enable);
        assert_eq!(settings.takeover_direct_trigger_boost_score, 0.4);
        assert_eq!(
            settings.text_models.as_ref().unwrap()[0].provider_id,
            "judge"
        );

        merge_real_context_settings(&mut instance, &settings);
        assert_eq!(instance.settings["reply_context_window"], 37);
        // Migrated to `true`, which now equals the default and is pruned from
        // the persisted map; the effective value is asserted above.
        assert!(!instance
            .settings
            .contains_key("takeover_direct_trigger_enable"));
        assert_eq!(instance.settings["text_models"][0]["provider_id"], "judge");
        assert_eq!(instance.settings["future_option"]["value"], 1);
        for key in DEPRECATED_REAL_CONTEXT_SETTINGS {
            assert!(!instance.settings.contains_key(*key));
        }
    }

    #[test]
    fn real_context_judge_persona_prompt_normalizes_validates_and_roundtrips() {
        let legacy =
            RealContextPluginSettings::from_instance(&PlatformPluginInstanceConfig::default())
                .unwrap();
        assert!(legacy.judge_persona_prompt.is_empty());

        let mut settings = RealContextPluginSettings {
            judge_persona_prompt: "  custom persona\n".to_string(),
            ..RealContextPluginSettings::default()
        };
        settings.normalize();
        assert_eq!(settings.judge_persona_prompt, "custom persona");
        assert!(settings.validate().is_ok());

        let mut instance = PlatformPluginInstanceConfig::default();
        instance
            .settings
            .insert("future_option".to_string(), serde_json::json!(true));
        merge_real_context_settings(&mut instance, &settings);
        assert_eq!(instance.settings["judge_persona_prompt"], "custom persona");
        assert_eq!(instance.settings["future_option"], true);
        let reparsed = RealContextPluginSettings::from_instance(&instance).unwrap();
        assert_eq!(reparsed.judge_persona_prompt, "custom persona");

        let mut cleared = reparsed;
        cleared.judge_persona_prompt = " \n ".to_string();
        cleared.normalize();
        merge_real_context_settings(&mut instance, &cleared);
        assert!(!instance.settings.contains_key("judge_persona_prompt"));
        assert_eq!(instance.settings["future_option"], true);

        assert!(RealContextPluginSettings {
            judge_persona_prompt: "bad\0prompt".to_string(),
            ..RealContextPluginSettings::default()
        }
        .validate()
        .is_err());
        assert!(RealContextPluginSettings {
            judge_persona_prompt: "x".repeat(32_769),
            ..RealContextPluginSettings::default()
        }
        .validate()
        .is_err());
    }

    #[test]
    fn real_context_plugin_rejects_invalid_types_ranges_and_models() {
        let mut config = route_test_config();
        let mut instance = PlatformPluginInstanceConfig::default();
        instance.settings.insert(
            "active_judge_probability".to_string(),
            serde_json::json!(1.1),
        );
        config
            .platforms
            .qq
            .plugins
            .insert(REAL_CONTEXT_PLUGIN_ID.to_string(), instance);
        assert!(config.validate().is_err());

        config.platforms.qq.plugins.insert(
            REAL_CONTEXT_PLUGIN_ID.to_string(),
            PlatformPluginInstanceConfig {
                enabled: None,
                settings: serde_json::json!({"active_reply_enable": "yes"})
                    .as_object()
                    .unwrap()
                    .clone(),
            },
        );
        assert!(config.validate().is_err());

        let mut settings = RealContextPluginSettings {
            text_models: Some(vec![ActiveProviderModelConfig {
                provider_id: config.providers[0].id.clone(),
                model: "missing".to_string(),
            }]),
            ..RealContextPluginSettings::default()
        };
        let mut instance = PlatformPluginInstanceConfig::default();
        merge_real_context_settings(&mut instance, &settings);
        config
            .platforms
            .qq
            .plugins
            .insert(REAL_CONTEXT_PLUGIN_ID.to_string(), instance);
        assert!(config.validate().is_err());

        settings.text_models.as_mut().unwrap()[0].model = "text-only".to_string();
        merge_real_context_settings(
            config
                .platforms
                .qq
                .plugins
                .get_mut(REAL_CONTEXT_PLUGIN_ID)
                .unwrap(),
            &settings,
        );
        assert!(config.validate().is_ok());
    }

    #[test]
    fn real_context_models_follow_provider_lifecycle() {
        let mut config = route_test_config();
        let old_id = config.providers[0].id.clone();
        let settings = RealContextPluginSettings {
            text_models: Some(vec![ActiveProviderModelConfig {
                provider_id: old_id.clone(),
                model: "text-only".to_string(),
            }]),
            ..RealContextPluginSettings::default()
        };
        let mut instance = PlatformPluginInstanceConfig::default();
        instance
            .settings
            .insert("future_option".to_string(), serde_json::json!(true));
        merge_real_context_settings(&mut instance, &settings);
        config
            .platforms
            .qq
            .plugins
            .insert(REAL_CONTEXT_PLUGIN_ID.to_string(), instance);

        config.providers[0].id = "renamed".to_string();
        config.rename_provider_references(&old_id, "renamed");
        let instance = &config.platforms.qq.plugins[REAL_CONTEXT_PLUGIN_ID];
        let reparsed = RealContextPluginSettings::from_instance(instance).unwrap();
        assert_eq!(reparsed.text_models.unwrap()[0].provider_id, "renamed");
        assert_eq!(instance.settings["future_option"], true);

        config.remove_active_model_references("renamed", "text-only");
        let reparsed = RealContextPluginSettings::from_instance(
            &config.platforms.qq.plugins[REAL_CONTEXT_PLUGIN_ID],
        )
        .unwrap();
        assert!(reparsed.text_models.is_none());
    }

    #[test]
    fn platform_model_route_normalization_uses_none_for_inheritance() {
        let mut config = route_test_config();
        let provider_id = config.providers[0].id.clone();
        let mut route = test_route(&config);
        route.conversation.id = " 20002 ".to_string();
        route.extra_prompt = "  group prompt  ".to_string();
        route.text_models = Some(vec![
            ActiveProviderModelConfig {
                provider_id: format!(" {provider_id} "),
                model: " text-only ".to_string(),
            },
            ActiveProviderModelConfig {
                provider_id: provider_id.clone(),
                model: "text-only".to_string(),
            },
        ]);
        route.text_models_inheritance = PlatformModelPoolInheritance::Global;
        route.multimodal_models = Some(Vec::new());
        route.multimodal_models_inheritance = PlatformModelPoolInheritance::Global;
        config.platforms.qq.conversations.push(route);
        config.normalize_platform_model_routes();

        let normalized = &config.platforms.qq.conversations[0];
        assert_eq!(normalized.conversation.id, "20002");
        assert_eq!(normalized.extra_prompt, "group prompt");
        assert_eq!(normalized.text_models.as_ref().unwrap().len(), 1);
        assert_eq!(
            normalized.text_models_inheritance,
            PlatformModelPoolInheritance::Platform
        );
        assert!(normalized.multimodal_models.is_none());
        assert_eq!(
            normalized.multimodal_models_inheritance,
            PlatformModelPoolInheritance::Global
        );

        config.platforms.qq.conversations[0].text_models = Some(Vec::new());
        config.normalize_platform_model_routes();
        assert_eq!(config.platforms.qq.conversations.len(), 1);
        assert!(config.platforms.qq.conversations[0].text_models.is_none());
    }

    #[test]
    fn platform_model_route_validation_rejects_bad_identity_models_and_duplicates() {
        let mut config = route_test_config();
        let mut route = test_route(&config);
        route.conversation.id = "0".to_string();
        assert!(config.validate_platform_model_route(&route).is_err());
        route.conversation.id = "not-a-qq".to_string();
        assert!(config.validate_platform_model_route(&route).is_err());

        route.conversation.id = "20002".to_string();
        route.multimodal_models.as_mut().unwrap()[0].model = "text-only".to_string();
        assert!(config.validate_platform_model_route(&route).is_err());

        route.multimodal_models = None;
        route.text_models.as_mut().unwrap()[0].model = "missing".to_string();
        assert!(config.validate_platform_model_route(&route).is_err());

        let route = test_route(&config);
        config.platforms.qq.conversations = vec![route.clone(), route];
        assert!(config.validate().is_err());
    }

    #[test]
    fn platform_model_references_are_renamed_and_pruned() {
        let mut config = route_test_config();
        let old_provider = config.providers[0].id.clone();
        config.platforms.qq.non_whitelist_text_models = Some(vec![ActiveProviderModelConfig {
            provider_id: old_provider.clone(),
            model: "text-only".to_string(),
        }]);
        config.platforms.qq.conversations.push(test_route(&config));

        config.rename_platform_provider_references(&old_provider, "renamed");
        assert_eq!(
            config
                .platforms
                .qq
                .non_whitelist_text_models
                .as_ref()
                .unwrap()[0]
                .provider_id,
            "renamed"
        );
        let route = &config.platforms.qq.conversations[0];
        assert_eq!(
            route.text_models.as_ref().unwrap()[0].provider_id,
            "renamed"
        );
        assert_eq!(
            route.multimodal_models.as_ref().unwrap()[0].provider_id,
            "renamed"
        );

        config.rename_platform_provider_references("renamed", &old_provider);
        config.remove_active_model_references(&old_provider, "vision");
        assert!(config.platforms.qq.conversations[0]
            .multimodal_models
            .is_none());
        config.remove_active_model_references(&old_provider, "text-only");
        assert_eq!(config.platforms.qq.conversations.len(), 1);
        assert!(config.platforms.qq.conversations[0].text_models.is_none());
        assert!(config.platforms.qq.non_whitelist_text_models.is_none());
    }

    #[test]
    fn provider_reference_updates_cover_every_model_pool_and_plugin() {
        let mut config = route_test_config();
        let old_id = config.providers[0].id.clone();
        config.active_provider = old_id.clone();
        config.active_provider_models = Some(vec![ActiveProviderModelConfig {
            provider_id: old_id.clone(),
            model: "text-only".to_string(),
        }]);
        config.active_multimodal_provider_models = Some(vec![ActiveProviderModelConfig {
            provider_id: old_id.clone(),
            model: "vision".to_string(),
        }]);
        config.subagent_tiers.cheap.push(ActiveProviderModelConfig {
            provider_id: old_id.clone(),
            model: "text-only".to_string(),
        });
        config.platforms.qq.non_whitelist_text_models = Some(vec![ActiveProviderModelConfig {
            provider_id: old_id.clone(),
            model: "text-only".to_string(),
        }]);
        config.platforms.qq.conversations.push(test_route(&config));
        config.plugins.vision.vision_provider_id = old_id.clone();
        config.plugins.vision.vision_model = "vision".to_string();
        config.plugins.knowledge_base.embedding_provider_id = old_id.clone();
        config.plugins.knowledge_base.embedding_model = "text-only".to_string();

        config.providers[0].id = "renamed".to_string();
        config.rename_provider_references(&old_id, "renamed");

        assert_eq!(config.active_provider, "renamed");
        assert_eq!(
            config.active_provider_models.as_ref().unwrap()[0].provider_id,
            "renamed"
        );
        assert_eq!(
            config.active_multimodal_provider_models.as_ref().unwrap()[0].provider_id,
            "renamed"
        );
        assert_eq!(config.subagent_tiers.cheap[0].provider_id, "renamed");
        assert_eq!(
            config
                .platforms
                .qq
                .non_whitelist_text_models
                .as_ref()
                .unwrap()[0]
                .provider_id,
            "renamed"
        );
        assert_eq!(
            config.platforms.qq.conversations[0]
                .text_models
                .as_ref()
                .unwrap()[0]
                .provider_id,
            "renamed"
        );
        assert_eq!(config.plugins.vision.vision_provider_id, "renamed");
        assert_eq!(
            config.plugins.knowledge_base.embedding_provider_id,
            "renamed"
        );
        assert!(config.validate().is_ok());

        config.providers.remove(0);
        config.remove_provider_references("renamed");
        assert!(config.active_provider_models.is_none());
        assert!(config.active_multimodal_provider_models.is_none());
        assert!(config.subagent_tiers.cheap.is_empty());
        assert!(config.platforms.qq.non_whitelist_text_models.is_none());
        assert_eq!(config.platforms.qq.conversations.len(), 1);
        assert!(config.platforms.qq.conversations[0].text_models.is_none());
        assert!(config.plugins.vision.vision_provider_id.is_empty());
        assert!(config
            .plugins
            .knowledge_base
            .embedding_provider_id
            .is_empty());
        assert_ne!(config.active_provider, "renamed");
    }

    #[test]
    fn model_capability_pruning_clears_all_invalid_image_references() {
        let mut config = route_test_config();
        let provider_id = config.providers[0].id.clone();
        config.active_multimodal_provider_models = Some(vec![ActiveProviderModelConfig {
            provider_id: provider_id.clone(),
            model: "vision".to_string(),
        }]);
        config.platforms.qq.conversations.push(test_route(&config));
        config.plugins.vision.vision_provider_id = provider_id;
        config.plugins.vision.vision_model = "vision".to_string();
        config.providers[0]
            .model_modalities
            .insert("vision".to_string(), vec!["text".to_string()]);

        config.prune_model_references();

        assert!(config.active_multimodal_provider_models.is_none());
        assert!(config.platforms.qq.conversations[0]
            .multimodal_models
            .is_none());
        assert!(config.plugins.vision.vision_provider_id.is_empty());
        assert!(config.plugins.vision.vision_model.is_empty());
    }

    #[test]
    fn duplicate_provider_ids_are_rejected() {
        let mut config = AppConfig::default();
        config.providers.push(config.providers[0].clone());
        assert!(config.validate().is_err());
    }

    #[test]
    fn platform_multimodal_pruning_tracks_provider_capabilities() {
        let mut config = route_test_config();
        config.platforms.qq.conversations.push(test_route(&config));
        config.providers[0]
            .model_modalities
            .insert("vision".to_string(), vec!["text".to_string()]);

        config.prune_platform_model_routes();

        let route = &config.platforms.qq.conversations[0];
        assert!(route.multimodal_models.is_none());
        assert_eq!(route.text_models.as_ref().unwrap().len(), 1);
    }

    #[test]
    fn new_custom_provider_has_no_openai_defaults() {
        let provider = ProviderConfig::new_custom();

        assert!(provider.id.is_empty());
        assert!(provider.display_name.is_empty());
        assert!(provider.base_url.is_empty());
        assert_eq!(provider.protocol, "auto");
        assert!(provider.api_key.is_none());
        assert!(provider.models.is_empty());
        assert!(provider.default_model.is_empty());
    }

    #[test]
    fn default_anthropic_provider_uses_the_global_context_window_default() {
        let mut config = AppConfig::default();
        config.active_provider = "anthropic".to_string();
        let provider = config
            .providers
            .iter_mut()
            .find(|provider| provider.id == "anthropic")
            .unwrap();
        provider.models = vec!["claude-sonnet-4-5".to_string()];
        provider.default_model = "claude-sonnet-4-5".to_string();

        assert_eq!(config.active_context_window().unwrap(), Some(168_000));
    }

    #[test]
    fn mixed_context_window_uses_the_global_default_when_model_metadata_is_missing() {
        let mut config = AppConfig::default();
        let provider = &mut config.providers[0];
        let provider_id = provider.id.clone();
        provider.models = vec![
            "miyu-known-window-model".to_string(),
            "miyu-unknown-window-model".to_string(),
        ];
        provider.default_model = provider.models[0].clone();
        provider
            .model_context_window
            .insert(provider.models[0].clone(), 200_000);
        config.active_provider_models = Some(vec![
            ActiveProviderModelConfig {
                provider_id: provider_id.clone(),
                model: provider.models[0].clone(),
            },
            ActiveProviderModelConfig {
                provider_id,
                model: provider.models[1].clone(),
            },
        ]);

        assert_eq!(config.active_context_window().unwrap(), Some(168_000));
        config.providers[0]
            .model_context_window
            .insert("miyu-unknown-window-model".to_string(), 128_000);
        assert_eq!(config.active_context_window().unwrap(), Some(128_000));
    }

    #[test]
    fn default_anthropic_provider_has_no_implicit_active_model() {
        let provider = ProviderConfig::default_anthropic();

        assert!(provider.models.is_empty());
        assert!(provider.default_model.is_empty());
    }

    #[test]
    fn normalizes_legacy_anthropic_template_model() {
        let mut config = AppConfig::default();
        let provider = config
            .providers
            .iter_mut()
            .find(|provider| provider.id == "anthropic")
            .unwrap();
        provider.models = vec!["claude-sonnet-4-5".to_string()];
        provider.default_model = "claude-sonnet-4-5".to_string();

        config.normalize_builtin_providers();
        let provider = config
            .providers
            .iter()
            .find(|provider| provider.id == "anthropic")
            .unwrap();

        assert!(provider.models.is_empty());
        assert!(provider.default_model.is_empty());
    }

    #[test]
    fn anthropic_template_does_not_hardcode_model_context_window() {
        let provider = ProviderConfig::default_anthropic();

        assert!(provider.model_context_window.is_empty());
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
        assert!(!config
            .provider_model_choices()
            .iter()
            .any(|choice| choice.provider_id == provider_id));
    }

    #[test]
    fn display_readable_tool_names_defaults_enabled() {
        let display: DisplayConfig = serde_json::from_str(r#"{"tool_calls":"summary"}"#).unwrap();
        assert_eq!(display.language, "auto");
        assert!(display.readable_tool_names);
        assert!(!display.show_token_usage);
        assert_eq!(display.mixed_model_endpoint_display, "interactive");
        assert_eq!(display.command_output_lines, 10);

        let display: DisplayConfig = serde_json::from_str(r#"{"command_output_lines":3}"#).unwrap();
        assert_eq!(display.command_output_lines, 3);
        assert!(serde_json::to_string(&display)
            .unwrap()
            .contains(r#""command_output_lines":3"#));

        let mut config = AppConfig::default();
        config.display.command_output_lines = MAX_COMMAND_OUTPUT_LINES + 1;
        assert!(config.validate().is_err());

        let display: DisplayConfig = serde_json::from_str(r#"{"show_token_usage":true}"#).unwrap();
        assert!(display.show_token_usage);

        let display: DisplayConfig =
            serde_json::from_str(r#"{"show_mixed_model_endpoint":false}"#).unwrap();
        assert_eq!(display.mixed_model_endpoint_display, "off");

        let display: DisplayConfig =
            serde_json::from_str(r#"{"show_mixed_model_endpoint":true}"#).unwrap();
        assert_eq!(display.mixed_model_endpoint_display, "all");
    }

    #[test]
    fn display_language_roundtrips_and_rejects_unknown_values() {
        let display: DisplayConfig = serde_json::from_str(r#"{"language":"zh"}"#).unwrap();
        assert_eq!(display.language, "zh");
        assert!(serde_json::to_string(&display)
            .unwrap()
            .contains(r#""language":"zh""#));

        let mut config = AppConfig::default();
        config.display.language = "fr".to_string();
        assert!(config.validate().is_err());
        config.display.language.clear();
        assert!(config.validate().is_err());
    }

    #[test]
    fn display_language_hint_reads_jsonc_without_loading_full_config() {
        let temp = tempfile::tempdir().unwrap();
        let config_file = temp.path().join("config.jsonc");
        std::fs::write(
            &config_file,
            "{\n  // UI preference\n  \"display\": { \"language\": \"en\" }\n}\n",
        )
        .unwrap();
        let paths = MiyuPaths {
            root_dir: temp.path().to_path_buf(),
            config_dir: temp.path().to_path_buf(),
            config_file,
            skills_dir: temp.path().join("skills"),
            data_dir: temp.path().join("data"),
            cache_dir: temp.path().join("cache"),
            state_dir: temp.path().join("state"),
            pictures_dir: temp.path().join("pictures"),
            fish_hook_file: temp.path().join("miyu.fish"),
            bash_hook_file: temp.path().join("miyu.bash"),
            zsh_hook_file: temp.path().join("miyu.zsh"),
            scripts_dir: temp.path().join("scripts"),
            system_scripts_dir: temp.path().join("system-scripts"),
        };

        assert_eq!(
            AppConfig::display_language_hint(&paths).as_deref(),
            Some("en")
        );
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
        assert!(memes.auto_send_platform_enabled);
        assert_eq!(memes.search_max_results, 1);
        assert_eq!(memes.auto_send_probability, 0.05);
    }

    #[test]
    fn extra_body_roundtrip() {
        let original = ProviderConfig {
            id: "test".to_string(),
            display_name: "Test".to_string(),
            base_url: "https://example.com".to_string(),
            protocol: "auto".to_string(),
            api_key: None,
            models: vec![],
            model_context_window: HashMap::new(),
            model_modalities: HashMap::new(),
            model_costs: HashMap::new(),
            default_model: String::new(),
            timeout_seconds: 60,
            temperature: 1.0,
            anthropic_max_tokens: 4096,
            extra_body: serde_json::json!({
                "enable_thinking": false,
                "reasoning_effort": "low"
            })
            .as_object()
            .cloned(),
        };

        let serialized = serde_json::to_string(&original).unwrap();
        let deserialized: ProviderConfig = serde_json::from_str(&serialized).unwrap();

        assert_eq!(original.extra_body, deserialized.extra_body);
        assert_eq!(original.id, deserialized.id);
    }

    #[test]
    fn extra_body_rejects_non_object_config_values() {
        for extra_body in [
            serde_json::json!(true),
            serde_json::json!("invalid"),
            serde_json::json!([1, 2, 3]),
        ] {
            let provider = serde_json::json!({
                "id": "test",
                "display_name": "Test",
                "base_url": "https://example.com",
                "extra_body": extra_body
            });

            assert!(serde_json::from_value::<ProviderConfig>(provider).is_err());
        }
    }

    #[test]
    fn memory_diary_lifecycle_defaults_and_roundtrip_are_stable() {
        let defaults: MemoryConfig = serde_json::from_str("{}").unwrap();
        assert_eq!(defaults.diary_batch_size, 14);
        assert_eq!(defaults.short_diary_retention_days, 14);
        assert_eq!(defaults.diary_promotion_recalls, 3);
        assert_eq!(defaults.organizer_timeout_seconds, 120);
        assert!(!defaults.auto_skill_enabled);

        let parsed: MemoryConfig = serde_json::from_str(
            r#"{
                "diary_batch_size": 20,
                "short_diary_retention_days": 7,
                "diary_promotion_recalls": 4,
                "organizer_timeout_seconds": 90
            }"#,
        )
        .unwrap();
        assert_eq!(parsed.diary_batch_size, 20);
        assert_eq!(parsed.short_diary_retention_days, 7);
        assert_eq!(parsed.diary_promotion_recalls, 4);
        assert_eq!(parsed.organizer_timeout_seconds, 90);
    }

    #[test]
    fn default_prompt_resources_follow_the_data_resource_layout() {
        let temp = tempfile::tempdir().unwrap();
        let paths = MiyuPaths {
            root_dir: temp.path().to_path_buf(),
            config_dir: temp.path().join("config"),
            config_file: temp.path().join("config/config.jsonc"),
            skills_dir: temp.path().join("data/skills"),
            data_dir: temp.path().join("data"),
            cache_dir: temp.path().join("cache"),
            state_dir: temp.path().join("state"),
            pictures_dir: temp.path().join("data/pictures"),
            fish_hook_file: temp.path().join("fish/miyu.fish"),
            bash_hook_file: temp.path().join("config/shell/bash-hook.sh"),
            zsh_hook_file: temp.path().join("config/shell/zsh-hook.zsh"),
            scripts_dir: temp.path().join("data/scripts"),
            system_scripts_dir: PathBuf::new(),
        };
        let mut config = AppConfig::default();
        assert_eq!(
            config.prompts_dir_path(&paths),
            paths.data_dir.join("prompts")
        );
        assert_eq!(
            config.identities_dir_path(&paths),
            paths.data_dir.join("identities")
        );
        assert_eq!(
            config.user_identity_path(&paths),
            paths.data_dir.join("identities/user-identity.md")
        );
        assert_eq!(
            config.system_prompt_path(&paths),
            paths.data_dir.join("prompts/system-prompt.md")
        );

        config.prompt.prompts_dir = "./prompts/team".to_string();
        config.prompt.identities_dir = "identities/team".to_string();
        config.prompt.user_identity_file = "identities/team/user.md".to_string();
        config.system_prompt_file = Some("prompts/team/system.md".to_string());
        assert_eq!(
            config.prompts_dir_path(&paths),
            paths.data_dir.join("prompts/team")
        );
        assert_eq!(
            config.identities_dir_path(&paths),
            paths.data_dir.join("identities/team")
        );
        assert_eq!(
            config.user_identity_path(&paths),
            paths.data_dir.join("identities/team/user.md")
        );
        assert_eq!(
            config.system_prompt_path(&paths),
            paths.data_dir.join("prompts/team/system.md")
        );

        config.prompt.prompts_dir = "prompts/../scripts/personas".to_string();
        config.prompt.identities_dir = paths
            .config_dir
            .join("identities/team")
            .display()
            .to_string();
        assert_eq!(
            config.prompts_dir_path(&paths),
            paths.data_dir.join("scripts/personas")
        );
        assert_eq!(
            config.identities_dir_path(&paths),
            paths.data_dir.join("identities/team")
        );

        config.prompt.user_identity_file = "./user-identity.md".to_string();
        config.system_prompt_file = Some("./system-prompt.md".to_string());
        assert_eq!(
            config.user_identity_path(&paths),
            paths.data_dir.join("identities/user-identity.md")
        );
        assert_eq!(
            config.system_prompt_path(&paths),
            paths.data_dir.join("prompts/system-prompt.md")
        );

        config.prompt.user_identity_file = paths
            .config_dir
            .join("user-identity.md")
            .display()
            .to_string();
        config.system_prompt_file = Some(
            paths
                .config_dir
                .join("system-prompt.md")
                .display()
                .to_string(),
        );
        assert_eq!(
            config.user_identity_path(&paths),
            paths.data_dir.join("identities/user-identity.md")
        );
        assert_eq!(
            config.system_prompt_path(&paths),
            paths.data_dir.join("prompts/system-prompt.md")
        );

        config.prompt.prompts_dir = "custom-prompts".to_string();
        config.prompt.identities_dir = "custom-identities".to_string();
        config.prompt.user_identity_file = "custom-user.md".to_string();
        config.system_prompt_file = Some("custom-system.md".to_string());
        assert_eq!(
            config.prompts_dir_path(&paths),
            paths.config_dir.join("custom-prompts")
        );
        assert_eq!(
            config.identities_dir_path(&paths),
            paths.config_dir.join("custom-identities")
        );
        assert_eq!(
            config.user_identity_path(&paths),
            paths.config_dir.join("custom-user.md")
        );
        assert_eq!(
            config.system_prompt_path(&paths),
            paths.config_dir.join("custom-system.md")
        );

        let mut deferred_paths = paths.clone();
        deferred_paths.skills_dir = deferred_paths.config_dir.join("skills");
        deferred_paths.scripts_dir = deferred_paths.config_dir.join("scripts");
        let deferred = AppConfig::default();
        assert_eq!(
            deferred.user_identity_path(&deferred_paths),
            deferred_paths.config_dir.join("user-identity.md")
        );
        assert_eq!(
            deferred.system_prompt_path(&deferred_paths),
            deferred_paths.config_dir.join("system-prompt.md")
        );

        let base = directories::BaseDirs::new().unwrap();
        let root = base.home_dir().join(".miyu");
        let mut legacy_paths = paths.clone();
        legacy_paths.config_dir = root.join("config");
        legacy_paths.config_file = root.join("config/config.jsonc");
        legacy_paths.data_dir = root.join("data");
        legacy_paths.skills_dir = root.join("data/skills");
        legacy_paths.scripts_dir = root.join("data/scripts");
        let mut legacy_absolute = AppConfig::default();
        legacy_absolute.prompt.user_identity_file = base
            .config_dir()
            .join("miyu/user-identity.md")
            .display()
            .to_string();
        legacy_absolute.system_prompt_file = Some(
            base.config_dir()
                .join("miyu/system-prompt.md")
                .display()
                .to_string(),
        );
        assert_eq!(
            legacy_absolute.user_identity_path(&legacy_paths),
            root.join("data/identities/user-identity.md")
        );
        assert_eq!(
            legacy_absolute.system_prompt_path(&legacy_paths),
            root.join("data/prompts/system-prompt.md")
        );
    }

    #[test]
    fn reserved_system_prompt_file_is_not_a_persona() {
        let temp = tempfile::tempdir().unwrap();
        let paths = MiyuPaths {
            root_dir: temp.path().to_path_buf(),
            config_dir: temp.path().join("config"),
            config_file: temp.path().join("config/config.jsonc"),
            skills_dir: temp.path().join("data/skills"),
            data_dir: temp.path().join("data"),
            cache_dir: temp.path().join("cache"),
            state_dir: temp.path().join("state"),
            pictures_dir: temp.path().join("data/pictures"),
            fish_hook_file: temp.path().join("fish/miyu.fish"),
            bash_hook_file: temp.path().join("config/shell/bash-hook.sh"),
            zsh_hook_file: temp.path().join("config/shell/zsh-hook.zsh"),
            scripts_dir: temp.path().join("data/scripts"),
            system_scripts_dir: PathBuf::new(),
        };
        std::fs::create_dir_all(paths.prompts_dir()).unwrap();
        std::fs::write(paths.prompts_dir().join("system-prompt.md"), "fallback").unwrap();
        std::fs::write(paths.prompts_dir().join("System Prompt.md"), "persona").unwrap();
        let mut config = AppConfig::default();
        assert!(config.validate_persona_files(&paths).is_ok());
        config.prompt.active_persona = "system-prompt.md".to_string();
        assert!(config.validate_persona_files(&paths).is_err());
    }
}
