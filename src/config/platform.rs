//! 平台接入配置：会话、限流、模型路由、人格覆盖。
//!
//! 模型池的继承是三层的：会话 → 会话类型 → 平台。`PlatformModelPoolInheritance`
//! 用 `None` 表示「继承上层」而不是「空池」——两者语义完全不同，用同一个表示
//! 会让「我就是要一个空池」变得表达不出来。
//!
//! `rename_provider_in_pool` / `retain_provider_pool` 那一族是供应商改名或删除
//! 时的连带更新：漏一处就会留下指向不存在模型的路由。

use crate::config::*;

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
    pub(crate) fn is_default(&self) -> bool {
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
    pub(crate) fn is_default(&self) -> bool {
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
    /// 平台回合的工具轮数上限(0=不限)。本机会话跟随 tools.max_rounds,
    /// 平台回合失控时没人守在终端里按停,单独一道闸(真机 web_search 同
    /// query 222 连事故)。
    #[serde(
        default = "default_platform_max_tool_rounds",
        skip_serializing_if = "is_default_platform_max_tool_rounds"
    )]
    pub max_tool_rounds: usize,
    /// 允许 AI 从终端/WebUI/shellhook 会话主动发消息到通讯平台(`send_qq_message`
    /// 工具)。收件人只能是各平台配置的管理员,QQ 即 `qq.admin_users`,第一个为主管理员。
    #[serde(default = "default_terminal_outreach")]
    pub terminal_outreach: bool,
    #[serde(default, skip_serializing_if = "OneBotConfig::is_default")]
    pub qq: OneBotConfig,
}

fn is_false(value: &bool) -> bool {
    !*value
}

fn default_terminal_outreach() -> bool {
    true
}

pub(crate) fn default_platform_max_tool_rounds() -> usize {
    32
}

fn is_default_platform_max_tool_rounds(value: &usize) -> bool {
    *value == default_platform_max_tool_rounds()
}

impl Default for PlatformsConfig {
    fn default() -> Self {
        Self {
            command_prefix: default_platform_command_prefix(),
            commands: BTreeMap::new(),
            max_tool_rounds: default_platform_max_tool_rounds(),
            terminal_outreach: default_terminal_outreach(),
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
        self.qq.text_models.normalize();
        self.qq.multimodal_models.normalize();
        self.qq.non_whitelist_text_models.normalize();
        for route in &mut self.qq.conversations {
            route.normalize();
        }
        migrate_message_history_instance(&mut self.qq.plugins);
        if let Some(instance) = self.qq.plugins.get_mut(REAL_CONTEXT_PLUGIN_ID) {
            normalize_real_context_instance(instance);
        }
        if let Some(instance) = self.qq.plugins.get_mut(QQ_GROUP_JOIN_APPROVAL_PLUGIN_ID) {
            normalize_group_join_approval_instance(instance);
        }
        self.qq
            .plugins
            .retain(|name, instance| !name.trim().is_empty() && !instance.is_empty());
    }

    pub fn prune_model_references(&mut self, providers: &[ProviderConfig]) {
        self.qq.text_models.prune(providers, false);
        self.qq.multimodal_models.prune(providers, true);
        self.qq.non_whitelist_text_models.prune(providers, false);
        for route in &mut self.qq.conversations {
            route.prune_model_references(providers);
        }
        mutate_real_context_settings(&mut self.qq.plugins, |settings| {
            settings.text_models.prune(providers, false);
            settings.affection_text_models.prune(providers, false);
        });
        mutate_group_join_approval_settings(&mut self.qq.plugins, |settings| {
            settings.text_models.prune(providers, false);
        });
        self.normalize_model_routes();
    }

    pub fn remove_model_references(&mut self, provider_id: &str, model: &str) {
        for pool in self.qq_pool_refs_mut() {
            pool.remove_model(provider_id, model);
        }
        for route in &mut self.qq.conversations {
            route.remove_model_references(provider_id, model);
        }
        mutate_real_context_settings(&mut self.qq.plugins, |settings| {
            settings.text_models.remove_model(provider_id, model);
            settings
                .affection_text_models
                .remove_model(provider_id, model);
        });
        mutate_group_join_approval_settings(&mut self.qq.plugins, |settings| {
            settings.text_models.remove_model(provider_id, model);
        });
        self.normalize_model_routes();
    }

    /// The three QQ-wide pool references, for reference maintenance.
    fn qq_pool_refs_mut(&mut self) -> [&mut ModelPoolRef; 3] {
        [
            &mut self.qq.text_models,
            &mut self.qq.multimodal_models,
            &mut self.qq.non_whitelist_text_models,
        ]
    }

    pub fn remove_provider_references(&mut self, provider_id: &str) {
        for pool in self.qq_pool_refs_mut() {
            pool.remove_provider(provider_id);
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
            settings.text_models.remove_provider(provider_id);
            settings.affection_text_models.remove_provider(provider_id);
        });
        mutate_group_join_approval_settings(&mut self.qq.plugins, |settings| {
            settings.text_models.remove_provider(provider_id);
        });
        self.normalize_model_routes();
    }

    pub fn rename_provider_references(&mut self, old_id: &str, new_id: &str) {
        for pool in self.qq_pool_refs_mut() {
            pool.rename_provider(old_id, new_id);
        }
        for route in &mut self.qq.conversations {
            route.rename_provider_references(old_id, new_id);
        }
        mutate_real_context_settings(&mut self.qq.plugins, |settings| {
            settings.text_models.rename_provider(old_id, new_id);
            settings
                .affection_text_models
                .rename_provider(old_id, new_id);
        });
        mutate_group_join_approval_settings(&mut self.qq.plugins, |settings| {
            settings.text_models.rename_provider(old_id, new_id);
        });
    }

    pub fn rename_model_references(&mut self, provider_id: &str, old: &str, new: &str) {
        for pool in self.qq_pool_refs_mut() {
            pool.rename_model(provider_id, old, new);
        }
        for route in &mut self.qq.conversations {
            route.rename_model_references(provider_id, old, new);
        }
        mutate_real_context_settings(&mut self.qq.plugins, |settings| {
            settings.text_models.rename_model(provider_id, old, new);
            settings
                .affection_text_models
                .rename_model(provider_id, old, new);
        });
        mutate_group_join_approval_settings(&mut self.qq.plugins, |settings| {
            settings.text_models.rename_model(provider_id, old, new);
        });
    }
}

pub(crate) fn prune_pool(
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

pub(crate) fn default_platform_command_prefix() -> String {
    DEFAULT_PLATFORM_COMMAND_PREFIX.to_string()
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
    pub(crate) fn is_platform(&self) -> bool {
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

    pub(crate) fn prune_model_references(&mut self, providers: &[ProviderConfig]) {
        if let Some(pool) = &mut self.text_models {
            pool.retain(|entry| active_model_exists(providers, entry));
        }
        if let Some(pool) = &mut self.multimodal_models {
            pool.retain(|entry| active_model_supports_image(providers, entry));
        }
        normalize_route_pool(&mut self.text_models);
        normalize_route_pool(&mut self.multimodal_models);
    }

    pub(crate) fn remove_model_references(&mut self, provider_id: &str, model: &str) {
        for pool in [&mut self.text_models, &mut self.multimodal_models] {
            if let Some(entries) = pool {
                entries.retain(|entry| !(entry.provider_id == provider_id && entry.model == model));
            }
            normalize_route_pool(pool);
        }
    }

    pub(crate) fn rename_provider_references(&mut self, old_id: &str, new_id: &str) {
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

    pub(crate) fn rename_model_references(&mut self, provider_id: &str, old: &str, new: &str) {
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

pub(crate) fn normalize_route_pool(pool: &mut Option<Vec<ActiveProviderModelConfig>>) {
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

pub(crate) fn rename_provider_in_pool(
    pool: &mut [ActiveProviderModelConfig],
    old_id: &str,
    new_id: &str,
) {
    for entry in pool {
        if entry.provider_id == old_id {
            entry.provider_id = new_id.to_string();
        }
    }
}

pub(crate) fn retain_provider_pool(
    pool: &mut Option<Vec<ActiveProviderModelConfig>>,
    provider_id: &str,
) {
    if let Some(entries) = pool {
        entries.retain(|entry| entry.provider_id != provider_id);
    }
    retain_nonempty_pool(pool);
}

pub(crate) fn retain_nonempty_pool(pool: &mut Option<Vec<ActiveProviderModelConfig>>) {
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
    /// 管理员别名(键 = QQ 号字符串):终端发消息工具的 `to` 选项用它列出
    /// 能发给谁;没有别名的显示号码。
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub admin_aliases: BTreeMap<String, String>,
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
    /// 会话内并行:同一个会话(群/私聊)允许几个回合同时跑。默认关闭=串行,
    /// 一个会话同时只答一条(08-25 取证:并发回合各答各的,后到回合的历史
    /// 里躺着"@我却无人应答"的消息,是跨线代答与重复答题的土壤)。串行时
    /// 挡下的消息由「多少秒多少条」限流决定丢弃与否,超限即当没看到。
    /// 每个平台各自设定;跨会话并发不受此开关影响。
    #[serde(default, skip_serializing_if = "is_false")]
    pub session_parallel: bool,
    /// 会话内并行数量与排队上限(按会话/按类型的覆盖优先)。
    #[serde(default, skip_serializing_if = "PlatformSessionLimits::is_default")]
    pub session_limits: PlatformSessionLimits,
    #[serde(
        default,
        skip_serializing_if = "PlatformGroupContextConfig::is_default"
    )]
    pub group_context: PlatformGroupContextConfig,
    /// QQ default text pool: `inherit` = the global text pool.
    #[serde(default, skip_serializing_if = "ModelPoolRef::is_inherit")]
    pub text_models: ModelPoolRef,
    /// QQ default multimodal pool: `inherit` = the global multimodal pool.
    /// Tiers are text pools and may not be referenced here.
    #[serde(default, skip_serializing_if = "ModelPoolRef::is_inherit")]
    pub multimodal_models: ModelPoolRef,
    /// Text pool for non-whitelisted private chats and groups: `inherit` =
    /// the QQ default text pool.
    #[serde(default, skip_serializing_if = "ModelPoolRef::is_inherit")]
    pub non_whitelist_text_models: ModelPoolRef,
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
    pub(crate) legacy_non_whitelist_rate_per_minute: Option<u32>,
}

impl Default for QqPrivateChatsConfig {
    fn default() -> Self {
        Self {
            whitelist: Vec::new(),
            friend_requests_require_private_whitelist: true,
            allow_non_whitelist: true,
            // 非白名单默认限流 300 秒 5 条(08-26 用户口径):原来的 600 秒 2 条
            // 对正常提问都嫌紧,又挡不住刷屏——刷屏靠的是会话串行与队列上限。
            non_whitelist_rate_limit: PlatformRateLimit {
                max_messages: 5,
                window_seconds: 300,
            },
            session_limits: None,
            legacy_non_whitelist_rate_per_minute: None,
        }
    }
}

impl QqPrivateChatsConfig {
    pub(crate) fn migrate_legacy_rate_limit(&mut self) {
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

pub(crate) fn validate_platform_session_limits(
    field: &str,
    limits: PlatformSessionLimits,
) -> Result<()> {
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
    pub(crate) legacy_whitelist_rate_per_minute: Option<u32>,
    #[serde(default, rename = "non_whitelist_rate_per_minute", skip_serializing)]
    pub(crate) legacy_non_whitelist_rate_per_minute: Option<u32>,
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
            // 非白名单默认限流 300 秒 5 条(08-26 用户口径):原来的 600 秒 2 条
            // 对正常提问都嫌紧,又挡不住刷屏——刷屏靠的是会话串行与队列上限。
            non_whitelist_rate_limit: PlatformRateLimit {
                max_messages: 5,
                window_seconds: 300,
            },
            session_limits: None,
            legacy_whitelist_rate_per_minute: None,
            legacy_non_whitelist_rate_per_minute: None,
        }
    }
}

impl QqGroupChatsConfig {
    pub(crate) fn migrate_legacy_rate_limits(&mut self) {
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
            admin_aliases: BTreeMap::new(),
            allow_non_admin_host_tools: false,
            group_intermediate_messages: false,
            private_intermediate_messages: true,
            user_identification: true,
            show_group_name: true,
            memory: PlatformMemoryConfig::default(),
            private_chats: QqPrivateChatsConfig::default(),
            group_chats: QqGroupChatsConfig::default(),
            session_parallel: false,
            session_limits: PlatformSessionLimits::default(),
            group_context: PlatformGroupContextConfig::default(),
            text_models: ModelPoolRef::inherit(),
            multimodal_models: ModelPoolRef::inherit(),
            non_whitelist_text_models: ModelPoolRef::inherit(),
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

    /// 一个会话的回合并发闸值。覆盖优先级:按会话 > 按会话类型 > QQ 默认;
    /// **会话内并行关闭时并行数一律为 1**(串行),排队上限照旧生效。
    pub fn session_limits(
        &self,
        kind: PlatformConversationKind,
        conversation_id: &str,
    ) -> PlatformSessionLimits {
        let mut limits = self
            .conversations
            .iter()
            .find(|route| route.matches(kind, conversation_id))
            .and_then(|route| route.session_limits)
            .or(match kind {
                PlatformConversationKind::Private => self.private_chats.session_limits,
                PlatformConversationKind::Group => self.group_chats.session_limits,
            })
            .unwrap_or(self.session_limits);
        if !self.session_parallel {
            limits.running = 1;
        }
        limits
    }
}
