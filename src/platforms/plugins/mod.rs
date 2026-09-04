use super::PlatformTurnContext;
use crate::config::AppConfig;
use crate::paths::MiyuPaths;
use crate::platform_types::{
    OutboundMessage, OutboundOrigin, PlatformContextFileRef, PlatformContextImageRef,
    PlatformConversation, PlatformInboundEvent, SendReceipt, TriggerDecision,
};
use crate::state::PlatformSessionBinding;
use crate::tools::ToolRegistry;
use anyhow::Result;
use futures_util::future::BoxFuture;
use serde_json::{json, Value};
use std::sync::Arc;
pub(super) const FIXED_OUTPUT_METADATA_KEY: &str = "platform.fixed_output";
pub(super) const SUPPRESS_FINAL_REPLY_METADATA_KEY: &str = "platform.suppress_final_reply";
pub(super) const SUPPRESS_PRIOR_REPLY_METADATA_KEY: &str = "platform.suppress_prior_reply";

async fn send_fixed_tool_output(context: &PlatformTurnContext, text: &str) -> Result<()> {
    let mut message = OutboundMessage::text(OutboundOrigin::Tool, text);
    message
        .metadata
        .insert(FIXED_OUTPUT_METADATA_KEY.to_string(), Value::Bool(true));
    message.metadata.insert(
        SUPPRESS_FINAL_REPLY_METADATA_KEY.to_string(),
        Value::Bool(true),
    );
    message.metadata.insert(
        SUPPRESS_PRIOR_REPLY_METADATA_KEY.to_string(),
        Value::Bool(true),
    );
    context.send(message).await?;
    Ok(())
}

mod access_manager;
pub(crate) mod group_management;
mod meme_collector;
pub(crate) mod message_history;
mod message_recall;
pub(crate) mod real_context;
mod renderer;
mod reply_processor;
pub(crate) mod scheduled_messages;

pub(crate) use real_context::active_judgement_skip::{
    active_judgement_skip_ids, apply_active_judgement_skip_editor_changes,
};
pub(crate) use renderer::{renderer_worker_requested, run_renderer_worker};

#[derive(Clone, Copy, Debug)]
pub(crate) struct PluginDescriptor {
    pub(crate) id: &'static str,
    pub(crate) priority: i32,
    pub(crate) default_enabled: bool,
}

pub(crate) struct PlatformTurnInput {
    pub(crate) content: String,
    /// 插件运行前的输入快照(用户原话+入站附注)。记忆日记只读它,不读被
    /// 插件包装后的 `content`(指令样板/群聊记录块)——C10「三份内容分离」
    /// 的最小实现。插件不得修改此字段。
    pub(crate) memory_content: String,
    /// Stable policy text folded into the system prompt. Only content that is
    /// byte-identical on every turn of the conversation belongs here — a block
    /// that appears, changes, or disappears per turn breaks the provider
    /// prefix cache from the system prompt onwards.
    pub(crate) system_context: Vec<String>,
    /// Per-turn transport/control blocks. They ride the turn tail after the
    /// user message and get fossilized (v7 §三 [E] 区), so the system prompt
    /// stays byte-stable no matter how often they come and go.
    pub(crate) turn_system_context: Vec<String>,
    pub(crate) context_images: Vec<PlatformContextImageRef>,
    pub(crate) context_files: Vec<PlatformContextFileRef>,
}

pub(crate) struct PlatformPersonaResetContext<'a> {
    pub(crate) config: &'a AppConfig,
    pub(crate) paths: &'a MiyuPaths,
    pub(crate) bindings: &'a [PlatformSessionBinding],
}

pub(crate) struct PreparedSend {
    pub(crate) primary: OutboundMessage,
    pub(crate) after_success: Vec<OutboundMessage>,
    pub(crate) fallback: Option<OutboundMessage>,
    pub(crate) suppress_final_reply: bool,
    pub(crate) suppress_prior_reply: bool,
}

/// Ordinary members may ask the model to perform a sensitive platform action.
/// Keep the tool visible so the model can explain the capability, but require
/// it to repeat the exact call after receiving this confirmation frame.
pub(super) async fn require_ai_confirmation(
    context: &PlatformTurnContext,
    action: &str,
    arguments: &Value,
) -> Result<Option<String>> {
    // 免确认门槛用 fresh 查询而非回合缓存:刚被撤权的请求者不该还能在
    // 缓存窗口内跳过二次确认(执行侧校验同样走 fresh,两处口径一致)。
    let requester_is_manager = context
        .group_member_fresh(&context.sender_id)
        .await?
        .is_some_and(|member| matches!(member.role.as_str(), "owner" | "admin"));
    if context.is_admin || requester_is_manager {
        return Ok(None);
    }

    let key = format!("qq.confirmation.{action}");
    let canonical_arguments = canonical_confirmation_arguments(arguments);
    if context.plugin_value(&key).is_some_and(|pending| {
        confirmation_matches(
            &pending,
            &context.sender_id,
            arguments,
            &canonical_arguments,
        )
    }) {
        context.remove_plugin_value(&key);
        return Ok(None);
    }

    let token = random_confirmation_token();
    context.set_plugin_value(
        key,
        json!({
            "sender_id": context.sender_id,
            "canonical_arguments": canonical_arguments,
            "confirmation_token": token,
        }),
    );
    Ok(Some(
        json!({
            "success": false,
            "confirmation_required": true,
            "confirmation_token": token,
            // 声明式+先说"不是拒绝":旧文案以否定句开头,模型会读成权限
            // 拒绝直接放弃(非管理员触发"群管用不了"的实际根因,08-20)。
            "message": "This is an automatic double-check, not a permission denial. The requester is not a Miyu admin or a QQ group owner/admin, so the platform asks you to confirm the action yourself before it runs. If you judge the action appropriate, call the same tool again in this turn with this confirmation_token and every other parameter unchanged; the repeated call executes normally.",
            "action": action,
        })
        .to_string(),
    ))
}

fn random_confirmation_token() -> String {
    format!("qq-confirm-{:032x}", rand::random::<u128>())
}

fn canonical_confirmation_arguments(arguments: &Value) -> Value {
    match arguments {
        Value::Object(object) => Value::Object(
            object
                .iter()
                .filter(|(key, _)| key.as_str() != "confirmation_token")
                .map(|(key, value)| (key.clone(), canonical_confirmation_arguments(value)))
                .collect(),
        ),
        Value::Array(values) => Value::Array(
            values
                .iter()
                .map(canonical_confirmation_arguments)
                .collect(),
        ),
        value => value.clone(),
    }
}

fn confirmation_token(arguments: &Value) -> Option<&str> {
    match arguments {
        Value::Object(object) => object
            .get("confirmation_token")
            .and_then(Value::as_str)
            .or_else(|| object.values().find_map(confirmation_token)),
        Value::Array(values) => values.iter().find_map(confirmation_token),
        _ => None,
    }
}

fn confirmation_matches(
    pending: &Value,
    sender_id: &str,
    arguments: &Value,
    canonical_arguments: &Value,
) -> bool {
    pending.get("sender_id").and_then(Value::as_str) == Some(sender_id)
        && pending.get("canonical_arguments") == Some(canonical_arguments)
        && pending.get("confirmation_token").and_then(Value::as_str)
            == confirmation_token(arguments)
}

#[cfg(test)]
mod confirmation_tests {
    use super::*;

    fn pending(arguments: &Value) -> Value {
        json!({
            "sender_id": "member-1",
            "canonical_arguments": canonical_confirmation_arguments(arguments),
            "confirmation_token": "qq-confirm-secret",
        })
    }

    #[test]
    fn confirmation_without_token_does_not_match() {
        let arguments = json!({"arguments": {"user_id": "12345"}});
        let canonical = canonical_confirmation_arguments(&arguments);
        assert!(!confirmation_matches(
            &pending(&arguments),
            "member-1",
            &arguments,
            &canonical
        ));
    }

    #[test]
    fn confirmation_with_matching_token_matches_once() {
        let arguments = json!({
            "arguments": {"user_id": "12345", "confirmation_token": "qq-confirm-secret"}
        });
        let canonical = canonical_confirmation_arguments(&arguments);
        assert!(confirmation_matches(
            &pending(&arguments),
            "member-1",
            &arguments,
            &canonical
        ));
    }

    #[test]
    fn confirmation_rejects_changed_arguments_even_with_token() {
        let first = json!({
            "arguments": {"user_id": "12345", "confirmation_token": "qq-confirm-secret"}
        });
        let changed = json!({
            "arguments": {"user_id": "67890", "confirmation_token": "qq-confirm-secret"}
        });
        let canonical = canonical_confirmation_arguments(&changed);
        assert!(!confirmation_matches(
            &pending(&first),
            "member-1",
            &changed,
            &canonical
        ));
    }
}

impl PreparedSend {
    pub(super) fn unchanged(message: OutboundMessage) -> Self {
        Self {
            primary: message,
            after_success: Vec::new(),
            fallback: None,
            suppress_final_reply: false,
            suppress_prior_reply: false,
        }
    }
}

pub(crate) trait PlatformPlugin: Send + Sync {
    fn descriptor(&self) -> PluginDescriptor;

    /// Archives a transport message before admission, command handling, and
    /// reply-queue limits. Implementations must keep this hook lightweight.
    fn observe_ingress<'a>(
        &'a self,
        _paths: &'a MiyuPaths,
        _config: &'a AppConfig,
        _event: &'a PlatformInboundEvent,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async { Ok(()) })
    }

    fn handle_command<'a>(
        &'a self,
        _context: &'a PlatformTurnContext,
        _text: &'a str,
    ) -> BoxFuture<'a, Result<Option<OutboundMessage>>> {
        Box::pin(async { Ok(None) })
    }

    fn register_tools(
        &self,
        _registry: &mut ToolRegistry,
        _context: Arc<PlatformTurnContext>,
    ) -> Result<()> {
        Ok(())
    }

    /// Gives plugins a lightweight chance to cancel an older turn before the
    /// per-session FIFO lease is acquired. No history or agent state may be
    /// mutated from this hook.
    fn preempt_inbound(
        &self,
        _context: &PlatformTurnContext,
        _event: &PlatformInboundEvent,
    ) -> Result<bool> {
        Ok(false)
    }

    fn turn_is_superseded(&self, _context: &PlatformTurnContext) -> bool {
        false
    }

    /// Runs after a preempted message successfully superseded the active
    /// generation, so plugins can move per-message side effects (reactions,
    /// pending-reply bookkeeping) onto the new message and refresh their
    /// supersede window.
    fn confirm_supersede<'a>(
        &'a self,
        _context: &'a PlatformTurnContext,
        _event: &'a PlatformInboundEvent,
    ) -> BoxFuture<'a, ()> {
        Box::pin(async {})
    }

    fn turn_started(
        &self,
        _context: &PlatformTurnContext,
        _cancel: tokio::sync::watch::Sender<bool>,
    ) {
    }

    fn after_turn_aborted<'a>(
        &'a self,
        _context: &'a PlatformTurnContext,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async { Ok(()) })
    }

    /// Observe every admitted platform event, including group messages that
    /// remain silent and message recalls. Full transport archiving belongs to
    /// `observe_ingress`; this hook is for state that needs admission context.
    fn observe_inbound<'a>(
        &'a self,
        _context: &'a PlatformTurnContext,
        _event: &'a PlatformInboundEvent,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async { Ok(()) })
    }

    /// Updates state owned by the currently running turn after an admitted
    /// same-sender message has been queued as a tool-time follow-up.
    fn accept_followup(
        &self,
        _context: &PlatformTurnContext,
        _event: &PlatformInboundEvent,
    ) -> Result<()> {
        Ok(())
    }

    /// Adjust the core platform trigger decision. All enabled plugins see the
    /// same mutable decision in priority order, so observation and trigger
    /// ownership remain composable.
    fn decide_trigger<'a>(
        &'a self,
        _context: &'a PlatformTurnContext,
        _event: &'a PlatformInboundEvent,
        _decision: &'a mut TriggerDecision,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async { Ok(()) })
    }

    fn after_session_reset<'a>(
        &'a self,
        _context: &'a PlatformTurnContext,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async { Ok(()) })
    }

    fn after_persona_reset<'a>(
        &'a self,
        _context: &'a PlatformPersonaResetContext<'a>,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async { Ok(()) })
    }

    fn before_turn<'a>(
        &'a self,
        _context: &'a PlatformTurnContext,
        _input: &'a mut PlatformTurnInput,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async { Ok(()) })
    }

    fn before_send<'a>(
        &'a self,
        _context: &'a PlatformTurnContext,
        message: OutboundMessage,
    ) -> BoxFuture<'a, Result<PreparedSend>> {
        Box::pin(async move { Ok(PreparedSend::unchanged(message)) })
    }

    fn after_send<'a>(
        &'a self,
        _context: &'a PlatformTurnContext,
        _message: &'a OutboundMessage,
        _receipt: &'a SendReceipt,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async { Ok(()) })
    }

    fn record_external_bot_message<'a>(
        &'a self,
        _context: &'a PlatformTurnContext,
        _message_id: &'a str,
        _text: &'a str,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async { Ok(()) })
    }
}

#[derive(Default)]
pub(crate) struct PlatformPluginRegistry {
    plugins: Vec<Arc<dyn PlatformPlugin>>,
}

impl PlatformPluginRegistry {
    pub(crate) fn built_in() -> Result<Self> {
        Ok(Self::new(vec![
            Arc::new(access_manager::AccessManagerPlugin::new()),
            Arc::new(message_history::MessageHistoryPlugin::new()),
            Arc::new(real_context::RealContextPlugin::new()),
            Arc::new(message_recall::MessageRecallPlugin::new()),
            Arc::new(meme_collector::MemeCollectorPlugin::new()),
            Arc::new(Arc::new(group_management::GroupManagementPlugin::new())),
            Arc::new(reply_processor::ReplyProcessorPlugin::new()?),
            Arc::new(scheduled_messages::ScheduledMessagesPlugin::new()),
        ]))
    }

    pub(crate) fn new(mut plugins: Vec<Arc<dyn PlatformPlugin>>) -> Self {
        plugins.sort_by(|left, right| {
            right
                .descriptor()
                .priority
                .cmp(&left.descriptor().priority)
                .then_with(|| left.descriptor().id.cmp(right.descriptor().id))
        });
        Self { plugins }
    }

    pub(crate) async fn handle_command(
        &self,
        context: &PlatformTurnContext,
        text: &str,
    ) -> Option<OutboundMessage> {
        for plugin in self.enabled_plugins(context) {
            match plugin.handle_command(context, text).await {
                Ok(Some(response)) => return Some(response),
                Ok(None) => {}
                Err(error) => tracing::warn!(
                    plugin = plugin.descriptor().id,
                    error = %error,
                    "{}",
                    crate::i18n::text("platform plugin command failed", "平台插件命令处理失败")
                ),
            }
        }
        None
    }

    pub(crate) async fn observe_ingress(
        &self,
        paths: &MiyuPaths,
        config: &AppConfig,
        event: &PlatformInboundEvent,
    ) {
        for plugin in self.plugins.iter().filter(|plugin| {
            let descriptor = plugin.descriptor();
            config
                .platforms
                .qq
                .plugins
                .get(descriptor.id)
                .and_then(|instance| instance.enabled)
                .unwrap_or(descriptor.default_enabled)
        }) {
            if let Err(error) = plugin.observe_ingress(paths, config, event).await {
                tracing::warn!(
                    plugin = plugin.descriptor().id,
                    error = %error,
                    "{}",
                    crate::i18n::text(
                        "platform plugin ingress observer failed",
                        "平台插件入站归档失败"
                    )
                );
            }
        }
    }

    pub(crate) fn register_tools(
        &self,
        registry: &mut ToolRegistry,
        context: Arc<PlatformTurnContext>,
    ) {
        for plugin in self.enabled_plugins(&context) {
            if let Err(error) = plugin.register_tools(registry, context.clone()) {
                tracing::warn!(
                    plugin = plugin.descriptor().id,
                    error = %error,
                    "{}",
                    crate::i18n::text("platform plugin tool registration failed", "平台插件工具注册失败")
                );
            }
        }
    }

    pub(crate) fn preempt_inbound(
        &self,
        context: &PlatformTurnContext,
        event: &PlatformInboundEvent,
    ) -> bool {
        let mut cancel_active_turn = false;
        for plugin in self.enabled_plugins(context) {
            match plugin.preempt_inbound(context, event) {
                Ok(cancel) => cancel_active_turn |= cancel,
                Err(error) => tracing::warn!(
                    plugin = plugin.descriptor().id,
                    error = %error,
                    "{}",
                    crate::i18n::text("platform plugin inbound preemption failed", "平台插件入站抢占失败")
                ),
            }
        }
        cancel_active_turn
    }

    pub(crate) fn turn_is_superseded(&self, context: &PlatformTurnContext) -> bool {
        self.enabled_plugins(context)
            .any(|plugin| plugin.turn_is_superseded(context))
    }

    pub(crate) async fn confirm_supersede(
        &self,
        context: &PlatformTurnContext,
        event: &PlatformInboundEvent,
    ) {
        for plugin in self.enabled_plugins(context) {
            plugin.confirm_supersede(context, event).await;
        }
    }

    pub(crate) fn turn_started(
        &self,
        context: &PlatformTurnContext,
        cancel: tokio::sync::watch::Sender<bool>,
    ) {
        for plugin in self.enabled_plugins(context) {
            plugin.turn_started(context, cancel.clone());
        }
    }

    pub(crate) async fn after_turn_aborted(&self, context: &PlatformTurnContext) {
        for plugin in self.enabled_plugins(context) {
            if let Err(error) = plugin.after_turn_aborted(context).await {
                tracing::warn!(
                    plugin = plugin.descriptor().id,
                    error = %error,
                    "{}",
                    crate::i18n::text("platform plugin aborted-turn cleanup failed", "平台插件中止轮次清理失败")
                );
            }
        }
    }

    pub(crate) async fn observe_inbound(
        &self,
        context: &PlatformTurnContext,
        event: &PlatformInboundEvent,
    ) {
        for plugin in self.enabled_plugins(context) {
            if let Err(error) = plugin.observe_inbound(context, event).await {
                tracing::warn!(
                    plugin = plugin.descriptor().id,
                    error = %error,
                    "{}",
                    crate::i18n::text("platform plugin inbound observer failed", "平台插件入站观察器失败")
                );
            }
        }
    }

    pub(crate) fn accept_followup(
        &self,
        context: &PlatformTurnContext,
        event: &PlatformInboundEvent,
    ) {
        for plugin in self.enabled_plugins(context) {
            if let Err(error) = plugin.accept_followup(context, event) {
                tracing::warn!(
                    plugin = plugin.descriptor().id,
                    error = %error,
                    "{}",
                    crate::i18n::text("platform plugin follow-up hook failed", "平台插件后续消息钩子失败")
                );
            }
        }
    }

    pub(crate) async fn decide_trigger(
        &self,
        context: &PlatformTurnContext,
        event: &PlatformInboundEvent,
        decision: &mut TriggerDecision,
    ) {
        for plugin in self.enabled_plugins(context) {
            if let Err(error) = plugin.decide_trigger(context, event, decision).await {
                tracing::warn!(
                    plugin = plugin.descriptor().id,
                    error = %error,
                    "{}",
                    crate::i18n::text("platform plugin trigger hook failed", "平台插件触发钩子失败")
                );
            }
        }
    }

    pub(crate) async fn after_session_reset(&self, context: &PlatformTurnContext) -> Result<()> {
        let mut first_error = None;
        for plugin in self.enabled_plugins(context) {
            if let Err(error) = plugin.after_session_reset(context).await {
                tracing::warn!(
                    plugin = plugin.descriptor().id,
                    error = %error,
                    "{}",
                    crate::i18n::text("platform plugin post-reset hook failed", "平台插件重置后钩子失败")
                );
                if first_error.is_none() {
                    first_error = Some(anyhow::anyhow!(
                        "platform plugin {} failed after session reset: {error}",
                        plugin.descriptor().id
                    ));
                }
            }
        }
        match first_error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    pub(crate) async fn after_persona_reset(
        &self,
        context: &PlatformPersonaResetContext<'_>,
    ) -> Result<()> {
        let mut first_error = None;
        for plugin in &self.plugins {
            if let Err(error) = plugin.after_persona_reset(context).await {
                tracing::warn!(
                    plugin = plugin.descriptor().id,
                    error = %error,
                    "{}",
                    crate::i18n::text(
                        "platform plugin post-persona-reset hook failed",
                        "平台插件角色重置后钩子失败"
                    )
                );
                if first_error.is_none() {
                    first_error = Some(anyhow::anyhow!(
                        "platform plugin {} failed after persona reset: {error}",
                        plugin.descriptor().id
                    ));
                }
            }
        }
        match first_error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    pub(crate) async fn before_turn(
        &self,
        context: &PlatformTurnContext,
        input: &mut PlatformTurnInput,
    ) {
        for plugin in self.enabled_plugins(context) {
            if let Err(error) = plugin.before_turn(context, input).await {
                tracing::warn!(
                    target: "miyu::qq",
                    plugin = plugin.descriptor().id,
                    error = %error,
                    "{}",
                    crate::i18n::text(
                        "platform plugin before-turn hook failed; its context was not injected",
                        "平台插件轮次前钩子失败；未注入其上下文"
                    )
                );
            }
        }
    }

    pub(crate) async fn before_send(
        &self,
        context: &PlatformTurnContext,
        message: OutboundMessage,
    ) -> PreparedSend {
        if message
            .metadata
            .get(FIXED_OUTPUT_METADATA_KEY)
            .and_then(Value::as_bool)
            == Some(true)
        {
            return PreparedSend {
                suppress_final_reply: message
                    .metadata
                    .get(SUPPRESS_FINAL_REPLY_METADATA_KEY)
                    .and_then(Value::as_bool)
                    == Some(true),
                suppress_prior_reply: message
                    .metadata
                    .get(SUPPRESS_PRIOR_REPLY_METADATA_KEY)
                    .and_then(Value::as_bool)
                    == Some(true),
                primary: message,
                after_success: Vec::new(),
                fallback: None,
            };
        }
        let mut prepared = PreparedSend::unchanged(message);
        for plugin in self.enabled_plugins(context) {
            let previous = prepared.primary.clone();
            match plugin.before_send(context, prepared.primary).await {
                Ok(mut next) => {
                    if next.fallback.is_none() && next.primary.metadata != previous.metadata {
                        next.fallback = Some(previous);
                    }
                    prepared.after_success.append(&mut next.after_success);
                    prepared.suppress_final_reply |= next.suppress_final_reply;
                    prepared.suppress_prior_reply |= next.suppress_prior_reply;
                    if prepared.fallback.is_none() {
                        prepared.fallback = next.fallback;
                    }
                    prepared.primary = next.primary;
                }
                Err(error) => {
                    tracing::warn!(
                        plugin = plugin.descriptor().id,
                        error = %error,
                        "{}",
                        crate::i18n::text("platform plugin before-send hook failed", "平台插件发送前钩子失败")
                    );
                    prepared.primary = previous;
                }
            }
        }
        prepared
    }

    pub(crate) async fn after_send(
        &self,
        context: &PlatformTurnContext,
        message: &OutboundMessage,
        receipt: &SendReceipt,
    ) {
        for plugin in self.enabled_plugins(context) {
            if let Err(error) = plugin.after_send(context, message, receipt).await {
                tracing::warn!(
                    plugin = plugin.descriptor().id,
                    error = %error,
                    "{}",
                    crate::i18n::text("platform plugin after-send hook failed", "平台插件发送后钩子失败")
                );
            }
        }
    }

    pub(crate) async fn record_external_bot_message(
        &self,
        context: &PlatformTurnContext,
        message_id: &str,
        text: &str,
    ) {
        let _ = self
            .try_record_external_bot_message(context, message_id, text)
            .await;
    }

    pub(crate) async fn try_record_external_bot_message(
        &self,
        context: &PlatformTurnContext,
        message_id: &str,
        text: &str,
    ) -> Result<()> {
        let mut first_error = None;
        for plugin in self.enabled_plugins(context) {
            if let Err(error) = plugin
                .record_external_bot_message(context, message_id, text)
                .await
            {
                tracing::warn!(
                    plugin = plugin.descriptor().id,
                    error = %error,
                    "{}",
                    crate::i18n::text(
                        "platform plugin external history record failed",
                        "平台插件外部历史记录失败"
                    )
                );
                if first_error.is_none() {
                    first_error = Some(anyhow::anyhow!(
                        "platform plugin {} failed to record external history: {error}",
                        plugin.descriptor().id
                    ));
                }
            }
        }
        match first_error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    fn enabled_plugins<'a>(
        &'a self,
        context: &'a PlatformTurnContext,
    ) -> impl Iterator<Item = &'a Arc<dyn PlatformPlugin>> + 'a {
        self.plugins.iter().filter(move |plugin| {
            let descriptor = plugin.descriptor();
            context.plugin_enabled(descriptor.id, descriptor.default_enabled)
        })
    }
}

#[allow(dead_code)]
fn _conversation_is_stable_key(_conversation: &PlatformConversation) {}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestPlugin {
        id: &'static str,
        priority: i32,
    }

    impl PlatformPlugin for TestPlugin {
        fn descriptor(&self) -> PluginDescriptor {
            PluginDescriptor {
                id: self.id,
                priority: self.priority,
                default_enabled: true,
            }
        }
    }

    #[test]
    fn registry_orders_by_priority_then_stable_id() {
        let registry = PlatformPluginRegistry::new(vec![
            Arc::new(TestPlugin {
                id: "z-last",
                priority: 1,
            }),
            Arc::new(TestPlugin {
                id: "b-second",
                priority: 10,
            }),
            Arc::new(TestPlugin {
                id: "a-first",
                priority: 10,
            }),
        ]);
        assert_eq!(
            registry
                .plugins
                .iter()
                .map(|plugin| plugin.descriptor().id)
                .collect::<Vec<_>>(),
            vec!["a-first", "b-second", "z-last"]
        );
    }
}
