use super::{send_fixed_tool_output, PlatformPlugin, PluginDescriptor};
use crate::platforms::access_control::{
    administrator_authorization, global_grant_key, is_effective_admin, AccessPermission,
    ONEBOT_PLATFORM,
};
use crate::platforms::{ConversationKind, PlatformInboundEventKind, PlatformTurnContext};
use crate::state::{
    PlatformAccessActor, PlatformAccessGrant, PlatformAccessMutation, PlatformAccessMutationResult,
    GLOBAL_PLATFORM_ACCOUNT_SCOPE,
};
use crate::tools::{ToolRegistry, ToolSpec};
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::sync::Arc;

pub(crate) const ACCESS_MANAGER_PLUGIN_ID: &str = "platform_access_manager";
const MAX_LIST_ENTRIES: usize = 100;

#[derive(Default)]
pub(crate) struct AccessManagerPlugin;

impl AccessManagerPlugin {
    pub(crate) fn new() -> Self {
        Self
    }
}

impl PlatformPlugin for AccessManagerPlugin {
    fn descriptor(&self) -> PluginDescriptor {
        PluginDescriptor {
            id: ACCESS_MANAGER_PLUGIN_ID,
            priority: 300,
            default_enabled: true,
        }
    }

    fn register_tools(
        &self,
        registry: &mut ToolRegistry,
        context: Arc<PlatformTurnContext>,
    ) -> Result<()> {
        if context.conversation.platform != ONEBOT_PLATFORM
            || (!context.is_admin && !effective_admin(&context))
        {
            return Ok(());
        }
        registry.register(
            ToolSpec::new(
                super::TOOL_MANAGE_PLATFORM_ACCESS,
                "Directly manage Miyu's QQ administrators, private-chat whitelist, and group-chat whitelist. Call this when the current Miyu administrator asks to grant, revoke, or list access. Grant and revoke take effect immediately. The Rust host sends the final QQ result, so do not call send_message_to_user and do not add another acknowledgement.",
                json!({
                    "type": "object",
                    "properties": {
                        "operation": {
                            "type": "string",
                            "enum": ["grant", "revoke", "list"]
                        },
                        "permission": {
                            "type": "string",
                            "enum": [
                                "administrator",
                                "private_whitelist",
                                "group_whitelist"
                            ]
                        },
                        "target_id": {
                            "type": "string",
                            "description": "QQ user id for administrator/private_whitelist, or QQ group id for group_whitelist. group_whitelist defaults to the current group when omitted. A single trusted mention or reply may identify a user when omitted."
                        }
                    },
                    "required": ["operation"],
                    "additionalProperties": false
                }),
                move |arguments| {
                    let context = context.clone();
                    async move { manage_access(arguments, context).await }
                },
            )
            .writes()
            .with_display_name("管理通讯平台权限"),
        );
        Ok(())
    }
}

async fn manage_access(arguments: Value, context: Arc<PlatformTurnContext>) -> Result<String> {
    let response = match execute_access(&arguments, &context) {
        Ok(response) => response,
        Err(error) => {
            tracing::warn!(
                target: "miyu::qq",
                error = %error,
                sender_id = %context.sender_id,
                "platform access tool request failed"
            );
            "操作失败".to_string()
        }
    };
    send_fixed_tool_output(&context, &response).await?;
    Ok(json!({
        "ok": response != "操作失败",
        "message_sent": true,
        "do_not_send_another_reply": true
    })
    .to_string())
}

fn execute_access(arguments: &Value, context: &PlatformTurnContext) -> Result<String> {
    if context.conversation.platform != ONEBOT_PLATFORM || !effective_admin(context) {
        bail!("platform access management requires a current Miyu administrator");
    }
    let operation = required_string(arguments, "operation")?;
    if operation == "list" {
        return format_access_list(context, optional_permission(arguments)?);
    }
    let permission = required_permission(arguments)?;
    let target_id = resolve_target_id(arguments, permission, context)?;
    let actor = access_actor(context)?;
    match operation.as_str() {
        "grant" => apply_change(
            context,
            permission,
            target_id,
            &actor,
            PlatformAccessMutation::Grant,
        ),
        "revoke" => apply_change(
            context,
            permission,
            target_id,
            &actor,
            PlatformAccessMutation::Revoke,
        ),
        _ => bail!("operation must be grant, revoke, or list"),
    }
}

fn apply_change(
    context: &PlatformTurnContext,
    permission: AccessPermission,
    target_id: i64,
    actor: &PlatformAccessActor,
    operation: PlatformAccessMutation,
) -> Result<String> {
    context.with_current_config(|config| {
        let qq = &config.platforms.qq;
        let static_entry = permission.statically_contains(qq, target_id);
        let target_id_text = target_id.to_string();
        let dynamic_entry = context.state_store.has_platform_access_grant(
            ONEBOT_PLATFORM,
            GLOBAL_PLATFORM_ACCOUNT_SCOPE,
            permission.as_str(),
            permission.subject_kind(),
            &target_id_text,
        );
        match operation {
            PlatformAccessMutation::Grant if static_entry || dynamic_entry => {
                return Ok(existing_message(permission, target_id));
            }
            PlatformAccessMutation::Revoke if static_entry => {
                return Ok(format!(
                    "配置名单不可撤销：{} {}",
                    permission.label(),
                    target_id
                ));
            }
            PlatformAccessMutation::Revoke if !dynamic_entry => {
                return Ok(missing_message(permission, target_id));
            }
            _ => {}
        }

        let result = context
            .state_store
            .mutate_platform_access_grant_if_authorized(
                &global_grant_key(permission, target_id_text),
                actor,
                operation,
                &administrator_authorization(
                    qq,
                    &context.conversation.account_id,
                    &context.sender_id,
                ),
            )?;
        match result {
            PlatformAccessMutationResult::Unauthorized => {
                bail!("the requesting user is no longer a Miyu administrator")
            }
            PlatformAccessMutationResult::Unchanged => Ok(match operation {
                PlatformAccessMutation::Grant => existing_message(permission, target_id),
                PlatformAccessMutation::Revoke => missing_message(permission, target_id),
            }),
            PlatformAccessMutationResult::Changed => {
                tracing::info!(
                    target: "miyu::qq",
                    operation = ?operation,
                    permission = permission.as_str(),
                    target_id,
                    actor_id = %context.sender_id,
                    "platform access changed"
                );
                Ok(success_message(operation, permission, target_id))
            }
        }
    })
}

fn effective_admin(context: &PlatformTurnContext) -> bool {
    context.with_current_config(|config| {
        is_effective_admin(
            &config.platforms.qq,
            &context.state_store,
            &context.conversation.account_id,
            &context.sender_id,
        )
    })
}

fn access_actor(context: &PlatformTurnContext) -> Result<PlatformAccessActor> {
    let event = context
        .inbound_event()
        .context("platform access management requires a live platform message")?;
    if event.kind != PlatformInboundEventKind::Message
        || event.sender_id != context.sender_id
        || event.conversation != context.conversation
    {
        bail!("platform access identity does not match the current message");
    }
    Ok(PlatformAccessActor {
        platform: context.conversation.platform.clone(),
        account_id: context.conversation.account_id.clone(),
        user_id: context.sender_id.clone(),
        conversation_kind: context.conversation.kind.as_str().to_string(),
        conversation_id: context.conversation.conversation_id.clone(),
        message_id: event.message_id.clone(),
    })
}

fn resolve_target_id(
    arguments: &Value,
    permission: AccessPermission,
    context: &PlatformTurnContext,
) -> Result<i64> {
    if let Some(value) = arguments.get("target_id") {
        return positive_id(value).context("target_id must be a positive decimal id");
    }
    if permission == AccessPermission::GroupWhitelist
        && context.conversation.kind == ConversationKind::Group
    {
        return context
            .conversation
            .conversation_id
            .parse::<i64>()
            .ok()
            .filter(|id| *id > 0)
            .context("the current group id is invalid");
    }
    let event = context
        .inbound_event()
        .context("target_id or a live QQ message is required")?;
    let mut candidates = Vec::new();
    if let Some(replied) = event.replied_message.as_ref() {
        if let Ok(id) = replied.sender_id.parse::<i64>() {
            if id > 0 && replied.sender_id != context.conversation.account_id {
                candidates.push(id);
            }
        }
    }
    for user_id in &event.mentioned_user_ids {
        if user_id == &context.conversation.account_id {
            continue;
        }
        if let Ok(id) = user_id.parse::<i64>() {
            if id > 0 {
                candidates.push(id);
            }
        }
    }
    candidates.sort_unstable();
    candidates.dedup();
    match candidates.as_slice() {
        [target_id] => Ok(*target_id),
        [] => bail!("target_id, one trusted mention, or one replied user is required"),
        _ => bail!("exactly one target user is required"),
    }
}

fn required_string(arguments: &Value, key: &str) -> Result<String> {
    arguments
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .with_context(|| format!("{key} is required"))
}

fn optional_permission(arguments: &Value) -> Result<Option<AccessPermission>> {
    arguments
        .get("permission")
        .and_then(Value::as_str)
        .map(str::trim)
        .map(|value| {
            AccessPermission::parse(value).with_context(|| format!("invalid permission: {value}"))
        })
        .transpose()
}

fn required_permission(arguments: &Value) -> Result<AccessPermission> {
    optional_permission(arguments)?.context("permission is required for grant or revoke")
}

fn positive_id(value: &Value) -> Option<i64> {
    value
        .as_i64()
        .or_else(|| value.as_str()?.trim().parse().ok())
        .filter(|id| *id > 0)
}

fn success_message(
    operation: PlatformAccessMutation,
    permission: AccessPermission,
    target_id: i64,
) -> String {
    match (operation, permission) {
        (PlatformAccessMutation::Grant, AccessPermission::Administrator) => {
            format!("已添加管理员：{target_id}")
        }
        (PlatformAccessMutation::Revoke, AccessPermission::Administrator) => {
            format!("已移除管理员：{target_id}")
        }
        (PlatformAccessMutation::Grant, permission) => {
            format!("已加入{}：{target_id}", permission.label())
        }
        (PlatformAccessMutation::Revoke, permission) => {
            format!("已移出{}：{target_id}", permission.label())
        }
    }
}

fn existing_message(permission: AccessPermission, target_id: i64) -> String {
    if permission == AccessPermission::Administrator {
        format!("已是管理员：{target_id}")
    } else {
        format!("已在{}中：{target_id}", permission.label())
    }
}

fn missing_message(permission: AccessPermission, target_id: i64) -> String {
    if permission == AccessPermission::Administrator {
        format!("不是管理员：{target_id}")
    } else {
        format!("不在{}中：{target_id}", permission.label())
    }
}

#[derive(Default)]
struct AccessSources {
    configured: bool,
    dynamic: bool,
}

fn format_access_list(
    context: &PlatformTurnContext,
    selected: Option<AccessPermission>,
) -> Result<String> {
    let (config, grants) = context.with_current_config(|config| -> Result<_> {
        let qq = &config.platforms.qq;
        let grants = context
            .state_store
            .platform_access_grants_if_authorized(
                ONEBOT_PLATFORM,
                &administrator_authorization(
                    qq,
                    &context.conversation.account_id,
                    &context.sender_id,
                ),
            )?
            .context("the requesting user is no longer a Miyu administrator")?;
        Ok((qq.clone(), grants))
    })?;
    let permissions = selected.map_or_else(
        || {
            vec![
                AccessPermission::Administrator,
                AccessPermission::PrivateWhitelist,
                AccessPermission::GroupWhitelist,
            ]
        },
        |permission| vec![permission],
    );
    Ok(permissions
        .into_iter()
        .map(|permission| {
            format_permission_list(
                &config,
                &context.conversation.account_id,
                &grants,
                permission,
            )
        })
        .collect::<Vec<_>>()
        .join("\n"))
}

fn format_permission_list(
    config: &crate::config::OneBotConfig,
    account_id: &str,
    grants: &[PlatformAccessGrant],
    permission: AccessPermission,
) -> String {
    let mut entries = BTreeMap::<i64, AccessSources>::new();
    let configured = match permission {
        AccessPermission::Administrator => &config.admin_users,
        AccessPermission::PrivateWhitelist => &config.private_chats.whitelist,
        AccessPermission::GroupWhitelist => &config.group_chats.whitelist,
    };
    for target_id in configured {
        entries.entry(*target_id).or_default().configured = true;
    }
    for grant in grants.iter().filter(|grant| {
        grant.key.permission == permission.as_str()
            && grant.key.subject_kind == permission.subject_kind()
            && (grant.key.account_scope == GLOBAL_PLATFORM_ACCOUNT_SCOPE
                || grant.key.account_scope == account_id)
    }) {
        let Ok(target_id) = grant.key.subject_id.parse::<i64>() else {
            tracing::warn!(
                permission = permission.as_str(),
                subject_id = %grant.key.subject_id,
                "ignored invalid persisted platform access id"
            );
            continue;
        };
        entries.entry(target_id).or_default().dynamic = true;
    }
    if entries.is_empty() {
        return format!("{}：空", permission.label());
    }
    let configured = entries
        .iter()
        .filter(|(_, source)| source.configured)
        .map(|(id, _)| *id)
        .take(MAX_LIST_ENTRIES)
        .collect::<Vec<_>>();
    let dynamic = entries
        .iter()
        .filter(|(_, source)| source.dynamic && !source.configured)
        .map(|(id, _)| *id)
        .take(MAX_LIST_ENTRIES)
        .collect::<Vec<_>>();
    let mut lines = vec![permission.label().to_string()];
    if !configured.is_empty() {
        lines.push(format!("配置：{}", join_ids(&configured)));
    }
    if !dynamic.is_empty() {
        lines.push(format!("工具添加：{}", join_ids(&dynamic)));
    }
    if entries.len() > configured.len().saturating_add(dynamic.len()) {
        lines.push(format!("仅显示前 {MAX_LIST_ENTRIES} 项"));
    }
    lines.join("\n")
}

fn join_ids(ids: &[i64]) -> String {
    ids.iter()
        .map(i64::to_string)
        .collect::<Vec<_>>()
        .join("、")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AppConfig;
    use crate::paths::MiyuPaths;
    use crate::platforms::plugins::PlatformPluginRegistry;
    use crate::platforms::{
        OutboundBody, OutboundMessage, OutboundSegment, PlatformAdapter, PlatformConversation,
        PlatformInboundEvent, SendReceipt,
    };
    use crate::state::StateStore;
    use futures_util::future::BoxFuture;
    use std::path::PathBuf;
    use std::sync::Mutex;
    use std::time::Instant;

    #[derive(Default)]
    struct RecordingAdapter {
        messages: Mutex<Vec<OutboundMessage>>,
    }

    impl PlatformAdapter for RecordingAdapter {
        fn send<'a>(&'a self, message: OutboundMessage) -> BoxFuture<'a, Result<SendReceipt>> {
            Box::pin(async move {
                self.messages.lock().unwrap().push(message);
                Ok(SendReceipt {
                    delivered_parts: 1,
                    ..SendReceipt::default()
                })
            })
        }

        fn bot_display_name<'a>(&'a self) -> BoxFuture<'a, Result<String>> {
            Box::pin(async { Ok("Miyu".to_string()) })
        }
    }

    fn test_paths(root: &std::path::Path) -> MiyuPaths {
        MiyuPaths {
            root_dir: root.to_path_buf(),
            config_dir: root.join("config"),
            config_file: root.join("config/config.jsonc"),
            skills_dir: root.join("config/skills"),
            data_dir: root.join("data"),
            cache_dir: root.join("cache"),
            state_dir: root.join("state"),
            pictures_dir: root.join("pictures"),
            fish_hook_file: root.join("fish/miyu.fish"),
            bash_hook_file: root.join("shell/bash-hook.sh"),
            zsh_hook_file: root.join("shell/zsh-hook.zsh"),
            scripts_dir: root.join("config/scripts"),
            system_scripts_dir: PathBuf::new(),
        }
    }

    fn test_context(
        paths: &MiyuPaths,
        state: StateStore,
        adapter: Arc<RecordingAdapter>,
        static_admin: bool,
        configure: impl FnOnce(&mut AppConfig),
    ) -> Arc<PlatformTurnContext> {
        let conversation = PlatformConversation {
            platform: ONEBOT_PLATFORM.to_string(),
            account_id: "10000".to_string(),
            kind: ConversationKind::Private,
            conversation_id: "42".to_string(),
        };
        let mut config = AppConfig::default();
        if static_admin {
            config.platforms.qq.admin_users.push(42);
        }
        configure(&mut config);
        let registry = Arc::new(PlatformPluginRegistry::new(vec![Arc::new(
            AccessManagerPlugin::new(),
        )]));
        Arc::new(
            PlatformTurnContext::new(
                conversation.clone(),
                "42".to_string(),
                "admin".to_string(),
                static_admin,
                config,
                paths.clone(),
                state,
                adapter,
                registry,
            )
            .with_inbound_event(PlatformInboundEvent {
                kind: PlatformInboundEventKind::Message,
                conversation,
                conversation_display_name: None,
                message_id: "message-1".to_string(),
                sender_id: "42".to_string(),
                sender_display_name: "admin".to_string(),
                operator_id: None,
                timestamp: 1,
                received_at: Instant::now(),
                message_position: None,
                ingress_order: None,
                text: "直接修改权限".to_string(),
                reply_to_message_id: None,
                replied_message: None,
                mentioned_user_ids: Vec::new(),
                mentioned_users: Vec::new(),
                mentioned_bot: false,
                media: Vec::new(),
                notice_sub_type: None,
                duration_seconds: None,
            }),
        )
    }

    fn outbound_text(message: &OutboundMessage) -> String {
        let OutboundBody::Segments(segments) = &message.body else {
            panic!("expected text segments");
        };
        segments
            .iter()
            .filter_map(|segment| match segment {
                OutboundSegment::Text(text) | OutboundSegment::Markdown(text) => {
                    Some(text.as_str())
                }
                _ => None,
            })
            .collect()
    }

    #[test]
    fn tool_visibility_depends_only_on_effective_admin_access() {
        let temp = tempfile::tempdir().unwrap();
        let paths = test_paths(temp.path());
        let state = StateStore::new(&paths).unwrap();
        let plugin = AccessManagerPlugin::new();

        let admin = test_context(
            &paths,
            state.clone(),
            Arc::new(RecordingAdapter::default()),
            true,
            |_| {},
        );
        let mut registry = ToolRegistry::new();
        plugin.register_tools(&mut registry, admin).unwrap();
        assert!(registry.get("manage_platform_access").is_some());

        let ordinary = test_context(
            &paths,
            state,
            Arc::new(RecordingAdapter::default()),
            false,
            |_| {},
        );
        let mut registry = ToolRegistry::new();
        plugin.register_tools(&mut registry, ordinary).unwrap();
        assert!(registry.get("manage_platform_access").is_none());
    }

    #[test]
    fn registered_tool_is_visible_to_the_model_and_is_a_write_operation() {
        let temp = tempfile::tempdir().unwrap();
        let paths = test_paths(temp.path());
        let state = StateStore::new(&paths).unwrap();
        let admin = test_context(
            &paths,
            state,
            Arc::new(RecordingAdapter::default()),
            true,
            |_| {},
        );
        let mut registry = ToolRegistry::new();

        crate::platforms::register_platform_tools(&mut registry, admin);

        let tool = registry.get("manage_platform_access").unwrap();
        assert_eq!(tool.permission, crate::tools::ToolPermission::Writes);
        assert!(tool.always_loaded);
        assert!(registry
            .lazy_definitions(&std::collections::BTreeSet::new())
            .iter()
            .any(|definition| definition.function.name == "manage_platform_access"));
    }

    #[tokio::test]
    async fn grant_and_revoke_apply_immediately_with_one_fixed_output() {
        let temp = tempfile::tempdir().unwrap();
        let paths = test_paths(temp.path());
        let state = StateStore::new(&paths).unwrap();
        let adapter = Arc::new(RecordingAdapter::default());
        let context = test_context(&paths, state.clone(), adapter.clone(), true, |_| {});

        manage_access(
            json!({
                "operation": "grant",
                "permission": "private_whitelist",
                "target_id": "2477342916"
            }),
            context.clone(),
        )
        .await
        .unwrap();
        assert!(state.has_platform_access_grant(
            ONEBOT_PLATFORM,
            "another-bot",
            "private_whitelist",
            "user",
            "2477342916"
        ));
        assert_eq!(adapter.messages.lock().unwrap().len(), 1);
        assert_eq!(
            outbound_text(&adapter.messages.lock().unwrap()[0]),
            "已加入私聊白名单：2477342916"
        );
        assert_eq!(context.take_final_reply_suppression_start(15), Some(0));

        manage_access(
            json!({
                "operation": "revoke",
                "permission": "private_whitelist",
                "target_id": "2477342916"
            }),
            context,
        )
        .await
        .unwrap();
        assert!(!state.has_platform_access_grant(
            ONEBOT_PLATFORM,
            "10000",
            "private_whitelist",
            "user",
            "2477342916"
        ));
        assert_eq!(adapter.messages.lock().unwrap().len(), 2);
        assert_eq!(
            outbound_text(&adapter.messages.lock().unwrap()[1]),
            "已移出私聊白名单：2477342916"
        );
    }

    #[tokio::test]
    async fn configured_root_access_cannot_be_revoked() {
        let temp = tempfile::tempdir().unwrap();
        let paths = test_paths(temp.path());
        let state = StateStore::new(&paths).unwrap();
        let adapter = Arc::new(RecordingAdapter::default());
        let context = test_context(&paths, state, adapter.clone(), true, |config| {
            config.platforms.qq.private_chats.whitelist.push(99);
        });

        manage_access(
            json!({
                "operation": "revoke",
                "permission": "private_whitelist",
                "target_id": "99"
            }),
            context,
        )
        .await
        .unwrap();
        assert_eq!(
            outbound_text(&adapter.messages.lock().unwrap()[0]),
            "配置名单不可撤销：私聊白名单 99"
        );
    }

    #[tokio::test]
    async fn list_separates_configured_and_tool_added_access() {
        let temp = tempfile::tempdir().unwrap();
        let paths = test_paths(temp.path());
        let state = StateStore::new(&paths).unwrap();
        let adapter = Arc::new(RecordingAdapter::default());
        let context = test_context(&paths, state.clone(), adapter.clone(), true, |config| {
            config.platforms.qq.private_chats.whitelist.push(88);
        });
        let actor = access_actor(&context).unwrap();
        state
            .add_platform_access_grant(
                &global_grant_key(AccessPermission::PrivateWhitelist, "99"),
                &actor,
            )
            .unwrap();

        manage_access(
            json!({
                "operation": "list",
                "permission": "private_whitelist"
            }),
            context,
        )
        .await
        .unwrap();
        assert_eq!(
            outbound_text(&adapter.messages.lock().unwrap()[0]),
            "私聊白名单\n配置：88\n工具添加：99"
        );
    }
}
