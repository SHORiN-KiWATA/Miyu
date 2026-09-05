//! QQ 侧几个插件的设置结构与校验。
//!
//! 群管、撤回、表情收集、入群审核、消息历史。每个都是「结构 + Default + 校验」
//! 三件套，由 `PLATFORM_PLUGIN_VALIDATORS` 按插件 ID 分派。

use crate::config::*;

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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QqGroupJoinApprovalGroupConfig {
    pub group_id: i64,
    pub approve_condition: String,
}

/// Configuration contract for the built-in QQ group-join approval plugin.
///
/// Like the real-context plugin, values stay flat in the generic
/// platform-plugin map. `text_models` is a pool reference: `inherit` = the
/// QQ default text pool, `global`, a tier, or an explicit list.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct QqGroupJoinApprovalPluginSettings {
    /// Wall-clock deadline for one approval decision, including retries.
    pub timeout_seconds: u64,
    /// Extra attempts only after an unparsable JSON response.
    pub max_retries: usize,
    /// Pool reference; `inherit` = the QQ default text pool. Ships as
    /// `lite` so approvals leave the flagship pool the moment a lite tier
    /// exists.
    pub text_models: ModelPoolRef,
    pub groups: Vec<QqGroupJoinApprovalGroupConfig>,
}

impl Default for QqGroupJoinApprovalPluginSettings {
    fn default() -> Self {
        Self {
            timeout_seconds: 60,
            max_retries: 1,
            text_models: ModelPoolRef::tier(ModelTier::Lite),
            groups: Vec::new(),
        }
    }
}

impl QqGroupJoinApprovalPluginSettings {
    pub fn from_instance(instance: &PlatformPluginInstanceConfig) -> Result<Self> {
        serde_json::from_value(serde_json::Value::Object(instance.settings.clone()))
            .context("invalid qq_group_join_approval plugin settings")
    }

    pub fn normalize(&mut self) {
        self.text_models.normalize();
        for group in &mut self.groups {
            group.approve_condition = group.approve_condition.trim().to_string();
        }
        self.groups.sort_unstable_by_key(|group| group.group_id);
        let mut indexes = HashMap::with_capacity(self.groups.len());
        let mut unique = Vec::with_capacity(self.groups.len());
        for group in self.groups.drain(..) {
            if let Some(index) = indexes.get(&group.group_id).copied() {
                unique[index] = group;
            } else {
                indexes.insert(group.group_id, unique.len());
                unique.push(group);
            }
        }
        self.groups = unique;
    }

    pub fn validate(&self) -> Result<()> {
        if !(1..=3_600).contains(&self.timeout_seconds) {
            bail!(
                "platform plugin qq_group_join_approval.timeout_seconds must be between 1 and 3600"
            );
        }
        if self.max_retries > 3 {
            bail!("platform plugin qq_group_join_approval.max_retries must be between 0 and 3");
        }
        validate_pool_ref_shape(&self.text_models, "qq_group_join_approval.text_models")?;
        let mut group_ids = HashSet::with_capacity(self.groups.len());
        if self.groups.len() > 10_000
            || self.groups.iter().any(|group| {
                group.group_id <= 0
                    || group.approve_condition.is_empty()
                    || group.approve_condition.trim() != group.approve_condition
                    || group.approve_condition.chars().count() > 200_000
                    || group.approve_condition.chars().any(char::is_control)
                    || !group_ids.insert(group.group_id)
            })
        {
            bail!("platform plugin qq_group_join_approval.groups must contain unique positive group ids and valid approval conditions");
        }
        Ok(())
    }
}

pub(crate) fn validate_qq_group_join_approval_plugin_config(
    instance: &PlatformPluginInstanceConfig,
) -> Result<()> {
    QqGroupJoinApprovalPluginSettings::from_instance(instance)?.validate()
}

pub(crate) fn validate_qq_group_management_plugin_config(
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

pub(crate) fn validate_qq_message_recall_plugin_config(
    instance: &PlatformPluginInstanceConfig,
) -> Result<()> {
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

pub(crate) fn validate_qq_meme_collector_plugin_config(
    instance: &PlatformPluginInstanceConfig,
) -> Result<()> {
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

pub(crate) fn validate_qq_message_history_plugin_config(
    instance: &PlatformPluginInstanceConfig,
) -> Result<()> {
    QqMessageHistoryPluginSettings::from_instance(instance)?.validate()
}

pub(crate) fn normalize_group_join_approval_instance(instance: &mut PlatformPluginInstanceConfig) {
    let Ok(mut settings) = QqGroupJoinApprovalPluginSettings::from_instance(instance) else {
        return;
    };
    settings.normalize();
    merge_group_join_approval_settings(instance, &settings);
}

pub(crate) fn merge_group_join_approval_settings(
    instance: &mut PlatformPluginInstanceConfig,
    settings: &QqGroupJoinApprovalPluginSettings,
) {
    let Ok(serde_json::Value::Object(known)) = serde_json::to_value(settings) else {
        return;
    };
    let Ok(serde_json::Value::Object(defaults)) =
        serde_json::to_value(QqGroupJoinApprovalPluginSettings::default())
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

pub(crate) fn migrate_message_history_instance(plugins: &mut PlatformPluginsConfig) {
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

/// 与 [`mutate_real_context_settings`] 同构:入群审批插件的 `text_models`
/// 也持有 provider/model 引用,provider 删除/改名的引用维护必须覆盖它,
/// 否则悬空引用会让审批模型静默失效。
pub(crate) fn mutate_group_join_approval_settings(
    plugins: &mut PlatformPluginsConfig,
    mutate: impl FnOnce(&mut QqGroupJoinApprovalPluginSettings),
) {
    let Some(instance) = plugins.get_mut(QQ_GROUP_JOIN_APPROVAL_PLUGIN_ID) else {
        return;
    };
    let Ok(mut settings) = QqGroupJoinApprovalPluginSettings::from_instance(instance) else {
        return;
    };
    mutate(&mut settings);
    merge_group_join_approval_settings(instance, &settings);
}
