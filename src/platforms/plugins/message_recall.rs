use super::{PlatformPlugin, PlatformTurnInput, PluginDescriptor};
use crate::config::QqMessageRecallPluginSettings;
use crate::platforms::{
    ConversationKind, OutboundMessage, PlatformInboundEvent, PlatformInboundEventKind,
    PlatformMessageInfo, PlatformTurnContext, SendReceipt,
};
use crate::tools::{ToolRegistry, ToolSpec};
use anyhow::{bail, Context, Result};
use futures_util::future::BoxFuture;
use serde_json::{json, Value};
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::watch;

pub(crate) const MESSAGE_RECALL_PLUGIN_ID: &str = "qq_message_recall";
const MAX_SCOPES: usize = 512;

#[derive(Default)]
struct ScopeState {
    sent: VecDeque<String>,
    recalled: HashMap<String, Instant>,
    pending: HashMap<String, watch::Sender<bool>>,
    touched_at: Option<Instant>,
}

#[derive(Default)]
struct RecallState {
    scopes: HashMap<String, ScopeState>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TargetSource {
    Argument,
    Reply,
}

impl TargetSource {
    fn as_str(self) -> &'static str {
        match self {
            Self::Argument => "argument",
            Self::Reply => "reply",
        }
    }
}

struct RecallTarget {
    message_id: String,
    source: TargetSource,
}

pub(crate) struct MessageRecallPlugin {
    state: Arc<Mutex<RecallState>>,
}

impl MessageRecallPlugin {
    pub(crate) fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(RecallState::default())),
        }
    }

    fn prune(state: &mut RecallState, now: Instant, ttl: Duration) {
        state.scopes.retain(|_, scope| {
            scope.recalled.retain(|_, at| now.duration_since(*at) < ttl);
            scope
                .touched_at
                .is_some_and(|at| now.duration_since(at) < ttl)
                || !scope.pending.is_empty()
        });
        while state.scopes.len() > MAX_SCOPES {
            let Some(oldest) = state
                .scopes
                .iter()
                .filter(|(_, scope)| scope.pending.is_empty())
                .min_by_key(|(_, scope)| scope.touched_at)
                .map(|(key, _)| key.clone())
            else {
                break;
            };
            state.scopes.remove(&oldest);
        }
    }

    fn recalled(&self, context: &PlatformTurnContext) -> bool {
        let Some(message_id) = context
            .inbound_event()
            .map(|event| event.message_id.as_str())
        else {
            return false;
        };
        let settings = recall_settings(context).unwrap_or_default();
        let mut state = self.state.lock().unwrap();
        Self::prune(
            &mut state,
            Instant::now(),
            Duration::from_secs(settings.cancel_record_ttl_seconds),
        );
        state
            .scopes
            .get(&context.conversation.scope_key())
            .is_some_and(|scope| scope.recalled.contains_key(message_id))
    }

    /// 该消息是否已有撤回记录(TTL 窗口内;窗口外查不到就走接口如实报错)。
    fn recorded_recalled(&self, context: &PlatformTurnContext, message_id: &str) -> bool {
        let settings = recall_settings(context).unwrap_or_default();
        let mut state = self.state.lock().unwrap();
        Self::prune(
            &mut state,
            Instant::now(),
            Duration::from_secs(settings.cancel_record_ttl_seconds),
        );
        state
            .scopes
            .get(&context.conversation.scope_key())
            .is_some_and(|scope| scope.recalled.contains_key(message_id))
    }

    fn belongs(context: &PlatformTurnContext, info: &PlatformMessageInfo) -> bool {
        match (info.conversation_kind, info.conversation_id.as_deref()) {
            (Some(kind), Some(id)) => {
                kind == context.conversation.kind && id == context.conversation.conversation_id
            }
            _ => false,
        }
    }

    async fn withdraw(&self, context: Arc<PlatformTurnContext>, args: Value) -> Result<String> {
        let batch_ids = message_ids_arg(&args)?;
        if !batch_ids.is_empty() {
            let mut results = Vec::with_capacity(batch_ids.len());
            let mut failed = 0usize;
            for (index, id) in batch_ids.iter().enumerate() {
                if index > 0 {
                    // 撤回接口按条限速,批内留间隔避免触发风控。
                    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
                }
                let raw = self
                    .withdraw_one(context.clone(), &args, Some(id.clone()))
                    .await?;
                let item: Value = serde_json::from_str(&raw).unwrap_or(Value::String(raw));
                if !item
                    .get("success")
                    .and_then(Value::as_bool)
                    .unwrap_or(false)
                {
                    failed += 1;
                }
                results.push(json!({ "message_id": id, "result": item }));
            }
            let succeeded = batch_ids.len() - failed;
            return Ok(json!({
                "success": failed == 0,
                "message": format!("撤回 {succeeded} 条、失败 {failed} 条"),
                "succeeded": succeeded,
                "failed": failed,
                "results": results,
            })
            .to_string());
        }
        self.withdraw_one(context, &args, None).await
    }

    /// 单条撤回。`forced_id`=批量路径逐条指定的目标,它会压过回复定向;
    /// 不传则退回"显式参数 → 回复目标"的老规矩。
    /// 撤他人消息只看两件事:必须是群、且 Miyu 是该群管理员。不再做二次确认。
    async fn withdraw_one(
        &self,
        context: Arc<PlatformTurnContext>,
        args: &Value,
        forced_id: Option<String>,
    ) -> Result<String> {
        let explicit = match forced_id {
            Some(_) => None,
            None => explicit_id(args)?,
        };
        let target = match resolve_target(forced_id, explicit, reply_id(&context)) {
            Ok(target) => target,
            Err(error) => {
                return failure_response(
                    "target_required",
                    false,
                    "无法确定目标消息。请回复需要撤回的消息后重试。",
                    json!({ "detail": error.to_string() }),
                );
            }
        };
        let id = &target.message_id;
        let settings = recall_settings(&context)?;
        let reason = reason(args, settings.max_reason_length)?;
        // 已撤回短路:Miyu 记录过该消息的撤回事件(TTL 窗口内)就不再调接口,
        // 如实告知模型"此前已被撤回"——这是成功态,不是可重试失败。
        if self.recorded_recalled(&context, id) {
            return response(
                true,
                "该消息此前已被撤回，无需重复操作",
                json!({
                    "message_id": id,
                    "already_recalled": true,
                    "target_source": target.source.as_str()
                }),
            );
        }
        let info = match replied_info(&context, id) {
            Some(info) => info.clone(),
            None => match context.message_info(id).await {
                Ok(Some(info)) => info,
                Ok(None) => {
                    return failure_response(
                        "message_not_found",
                        false,
                        "目标消息不存在或已无法查询（可能已被撤回或过期），请不要改撤其他消息。",
                        json!({ "message_id": id, "target_source": target.source.as_str() }),
                    );
                }
                Err(error) => {
                    return failure_response(
                        "message_lookup_failed",
                        false,
                        "无法核验目标消息，未执行撤回，请不要改撤其他消息。",
                        json!({
                            "message_id": id,
                            "target_source": target.source.as_str(),
                            "detail": error.to_string()
                        }),
                    );
                }
            },
        };
        if !Self::belongs(&context, &info) {
            return failure_response(
                "wrong_conversation",
                false,
                "目标消息不属于当前会话",
                json!({ "message_id": id, "target_source": target.source.as_str() }),
            );
        }
        let own_message = info.sender_id == context.conversation.account_id;
        if !own_message {
            if context.conversation.kind != ConversationKind::Group {
                return failure_response(
                    "permission_denied",
                    false,
                    "私聊中只能撤回 Miyu 自己发送的消息",
                    json!({ "message_id": id }),
                );
            }
            if !context.bot_group_role().await.can_manage() {
                return failure_response(
                    "permission_denied",
                    false,
                    "Miyu 不是当前群的管理员，无法撤回群友消息",
                    json!({ "message_id": id }),
                );
            }
        }
        if let Err(error) = context.delete_message(id).await {
            if self.recorded_recalled(&context, id) {
                return response(
                    true,
                    "该消息已被（他人或此前的操作）撤回",
                    json!({ "message_id": id, "already_recalled": true }),
                );
            }
            tracing::warn!(
                target: "miyu::qq",
                error = %error,
                message_id = %id,
                target_source = target.source.as_str(),
                conversation = %context.conversation.scope_key(),
                "{}",
                crate::i18n::text("QQ message recall failed", "QQ 消息撤回失败")
            );
            return recall_failure_response(
                &error,
                id,
                target.source,
                if own_message { "miyu" } else { "group_member" },
            );
        }
        if own_message {
            if let Some(scope) = self
                .state
                .lock()
                .unwrap()
                .scopes
                .get_mut(&context.conversation.scope_key())
            {
                scope.sent.retain(|old| old != id);
            }
        }
        response(
            true,
            "消息已撤回",
            json!({
                "message_id": id,
                "sender_id": info.sender_id,
                "target_kind": if own_message { "miyu" } else { "group_member" },
                "reason": reason,
                "target_source": target.source.as_str()
            }),
        )
    }
}

impl PlatformPlugin for MessageRecallPlugin {
    fn descriptor(&self) -> PluginDescriptor {
        PluginDescriptor {
            id: MESSAGE_RECALL_PLUGIN_ID,
            priority: 190,
            default_enabled: true,
        }
    }

    fn register_tools(
        &self,
        registry: &mut ToolRegistry,
        context: Arc<PlatformTurnContext>,
    ) -> Result<()> {
        let settings = recall_settings(&context)?;
        if !settings.enable_tool {
            return Ok(());
        }
        let plugin = self.clone();
        registry.register(
            ToolSpec::new(
                "qq_withdraw_message",
                "Recall QQ messages in the current conversation. Works on your own messages and, in a group where you are an admin, on other people's. If the current user message replies to a target, omit message_id and the trusted reply target is used. Otherwise pass message_id, or message_ids to withdraw several at once — ids come from the [msg=...] marker on each history line. Explicit ids in message_ids are used as given and are not overridden by the reply target. Never guess a recent message and never retry with another target after a failure.",
                schema(),
                move |args| {
                    let plugin = plugin.clone();
                    let context = context.clone();
                    async move { plugin.withdraw(context, args).await }
                },
            )
            .writes()
            .with_display_name("撤回 QQ 消息"),
        );
        Ok(())
    }

    fn before_turn<'a>(
        &'a self,
        context: &'a PlatformTurnContext,
        input: &'a mut PlatformTurnInput,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let settings = recall_settings(context)?;
            if !settings.enable_tool {
                return Ok(());
            }
            input.system_context.push(
                "<qq-recall-rule>Withdraw messages with qq_withdraw_message. It covers your own messages and, in a group where you are an admin, other people's. When the current message replies to a target, omit message_id and the trusted reply target is used. Otherwise pass an explicit message_id, or message_ids for several at once; every history line carries its id in the [msg=...] marker, so several messages from one person can be withdrawn in a single call. Phrases like \"that message\" with no id and no reply cannot identify a target; ask the user to reply to it. After a failure, never withdraw a different message and never claim success.</qq-recall-rule>"
                    .to_string(),
            );
            Ok(())
        })
    }

    fn turn_started(&self, context: &PlatformTurnContext, cancel: watch::Sender<bool>) {
        let Some(message_id) = context
            .inbound_event()
            .map(|event| event.message_id.clone())
            .filter(|id| !id.is_empty())
        else {
            return;
        };
        let now = Instant::now();
        let settings = recall_settings(context).unwrap_or_default();
        let mut state = self.state.lock().unwrap();
        Self::prune(
            &mut state,
            now,
            Duration::from_secs(settings.cancel_record_ttl_seconds),
        );
        let scope = state
            .scopes
            .entry(context.conversation.scope_key())
            .or_default();
        scope.touched_at = Some(now);
        if scope.recalled.contains_key(&message_id) {
            cancel.send_replace(true);
        } else {
            scope.pending.insert(message_id, cancel);
        }
    }

    fn observe_inbound<'a>(
        &'a self,
        context: &'a PlatformTurnContext,
        event: &'a PlatformInboundEvent,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            if event.kind != PlatformInboundEventKind::MessageRecall || event.message_id.is_empty()
            {
                return Ok(());
            }
            let now = Instant::now();
            let settings = recall_settings(context).unwrap_or_default();
            let mut state = self.state.lock().unwrap();
            Self::prune(
                &mut state,
                now,
                Duration::from_secs(settings.cancel_record_ttl_seconds),
            );
            let scope = state
                .scopes
                .entry(context.conversation.scope_key())
                .or_default();
            scope.touched_at = Some(now);
            scope.recalled.insert(event.message_id.clone(), now);
            scope
                .sent
                .retain(|message_id| message_id != &event.message_id);
            if let Some(cancel) = scope.pending.remove(&event.message_id) {
                cancel.send_replace(true);
            }
            Ok(())
        })
    }

    fn turn_is_superseded(&self, context: &PlatformTurnContext) -> bool {
        self.recalled(context)
    }

    fn after_send<'a>(
        &'a self,
        context: &'a PlatformTurnContext,
        _message: &'a OutboundMessage,
        receipt: &'a SendReceipt,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let settings = recall_settings(context)?;
            if !settings.capture_outgoing_messages {
                return Ok(());
            }
            let now = Instant::now();
            let mut state = self.state.lock().unwrap();
            let scope = state
                .scopes
                .entry(context.conversation.scope_key())
                .or_default();
            scope.touched_at = Some(now);
            if let Some(id) = context.inbound_event().map(|event| &event.message_id) {
                scope.pending.remove(id);
            }
            for id in &receipt.message_ids {
                if !id.is_empty() {
                    scope.sent.retain(|old| old != id);
                    scope.sent.push_back(id.clone());
                }
            }
            while scope.sent.len() > settings.max_messages_per_conversation {
                scope.sent.pop_front();
            }
            Ok(())
        })
    }

    fn after_turn_aborted<'a>(
        &'a self,
        context: &'a PlatformTurnContext,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            if let Some(id) = context.inbound_event().map(|event| &event.message_id) {
                if let Some(scope) = self
                    .state
                    .lock()
                    .unwrap()
                    .scopes
                    .get_mut(&context.conversation.scope_key())
                {
                    scope.pending.remove(id);
                }
            }
            Ok(())
        })
    }
}

impl Clone for MessageRecallPlugin {
    fn clone(&self) -> Self {
        Self {
            state: self.state.clone(),
        }
    }
}

fn schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "message_id": {
                "type": ["string", "integer"],
                "description": "Required only when the current QQ message does not reply to a target. A trusted reply always overrides this argument. Never pass the current request message ID and never guess a recent message."
            },
            "message_ids": {
                "type": "array",
                "items": { "type": ["string", "integer"] },
                "description": "Withdraw several messages in one call (at most 20). Each id is used exactly as given: this overrides both message_id and the reply target. Ids come from the [msg=...] marker on history lines."
            },
            "reason": { "type": "string", "maxLength": 500 }
        },
        "additionalProperties": false
    })
}
fn explicit_id(args: &Value) -> Result<Option<String>> {
    let Some(value) = args.get("message_id") else {
        return Ok(None);
    };
    if value.is_null() {
        return Ok(None);
    }
    let id = parse_message_id_value(value)?;
    Ok((!id.is_empty()).then_some(id))
}

/// OneBot 消息 id 是有符号 i32(NapCat/snowluma 实发负 id,08-24 日志实录
/// -178436401),负号必须放行;空串按"未提供"由调用方处理。
fn parse_message_id_value(value: &Value) -> Result<String> {
    let id = match value {
        Value::String(id) => id.trim().to_string(),
        Value::Number(id) => id.to_string(),
        _ => bail!("message_id must be a numeric string or integer"),
    };
    if id.is_empty() {
        return Ok(id);
    }
    let digits = id.strip_prefix('-').unwrap_or(&id);
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        bail!("message_id must be numeric");
    }
    let numeric = id
        .parse::<i64>()
        .context("message_id is outside the supported numeric range")?;
    if numeric > i32::MAX as i64 || numeric < i32::MIN as i64 {
        bail!("message_id is outside the supported OneBot range");
    }
    Ok(id)
}

/// 批量参数:message_ids 数组,上限 20;缺席=空。
fn message_ids_arg(args: &Value) -> Result<Vec<String>> {
    let Some(list) = args.get("message_ids").and_then(Value::as_array) else {
        return Ok(Vec::new());
    };
    if list.len() > 20 {
        bail!("message_ids supports at most 20 messages per call");
    }
    let mut ids = Vec::with_capacity(list.len());
    for value in list {
        let id = parse_message_id_value(value)?;
        if id.is_empty() {
            bail!("message_ids must not contain empty entries");
        }
        ids.push(id);
    }
    Ok(ids)
}
fn reply_id(context: &PlatformTurnContext) -> Option<String> {
    context
        .inbound_event()
        .and_then(|event| event.reply_to_message_id.clone())
        .filter(|id| !id.is_empty())
}

/// 批量逐条指定的 `forced_id` 必须压过回复定向:否则当前消息只要带引用,
/// 一批 N 个 id 会被回复目标全部覆盖 —— 表现为"同一条消息撤了 N 次"。
/// 没有 `forced_id` 时维持原规矩(见 `select_target`)。
fn resolve_target(
    forced_id: Option<String>,
    explicit: Option<String>,
    reply: Option<String>,
) -> Result<RecallTarget> {
    if let Some(message_id) = forced_id {
        return Ok(RecallTarget {
            message_id,
            source: TargetSource::Argument,
        });
    }
    select_target(explicit, reply)
}

fn select_target(explicit: Option<String>, reply: Option<String>) -> Result<RecallTarget> {
    if let Some(message_id) = reply {
        // The reply relation is trusted platform metadata. Models often
        // confuse the current request ID with the quoted target ID, so a
        // quoted group message always owns the recall target.
        return Ok(RecallTarget {
            message_id,
            source: TargetSource::Reply,
        });
    }
    explicit
        .map(|message_id| RecallTarget {
            message_id,
            source: TargetSource::Argument,
        })
        .context("message_id or a replied-to message is required")
}
fn replied_info<'a>(
    context: &'a PlatformTurnContext,
    message_id: &str,
) -> Option<&'a PlatformMessageInfo> {
    context
        .inbound_event()
        .and_then(|event| event.replied_message.as_ref())
        .filter(|message| message.message_id == message_id)
}
fn recall_settings(context: &PlatformTurnContext) -> Result<QqMessageRecallPluginSettings> {
    context
        .config
        .platforms
        .qq
        .plugins
        .get(MESSAGE_RECALL_PLUGIN_ID)
        .map(QqMessageRecallPluginSettings::from_instance)
        .transpose()
        .map(|settings| settings.unwrap_or_default())
}
fn reason(args: &Value, maximum: usize) -> Result<String> {
    let value = args
        .get("reason")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim();
    if value.chars().count() > maximum {
        bail!("reason exceeds configured maximum length");
    }
    Ok(value.to_string())
}
fn response(success: bool, message: &str, data: Value) -> Result<String> {
    Ok(json!({ "success": success, "message": message, "data": data }).to_string())
}

fn failure_response(code: &str, retryable: bool, message: &str, data: Value) -> Result<String> {
    Ok(json!({
        "success": false,
        "code": code,
        "retryable": retryable,
        "message": message,
        "data": data
    })
    .to_string())
}

fn recall_failure_response(
    error: &anyhow::Error,
    message_id: &str,
    source: TargetSource,
    target_kind: &str,
) -> Result<String> {
    let detail = error.to_string();
    let decode_failed = detail.contains("retcode=1200") && detail.contains("decode failed");
    failure_response(
        if decode_failed {
            "napcat_recall_decode_failed"
        } else {
            "qq_recall_failed"
        },
        false,
        if decode_failed {
            "QQ 内核拒绝了撤回请求，消息没有被撤回。不要重试或改撤其他消息。"
        } else {
            "QQ 撤回接口调用失败，消息没有被撤回。不要改撤其他消息。"
        },
        json!({
            "message_id": message_id,
            "target_source": source.as_str(),
            "target_kind": target_kind,
            "detail": detail
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 负数 id 是 OneBot 常态(snowluma 实发 -178436401),必须放行;
    /// 批量参数上限 20、逐项校验。退回 parse_message_id_value 前负 id 报
    /// "must be numeric",第一断言红。
    #[test]
    fn negative_ids_and_batch_argument_are_accepted() {
        assert_eq!(
            explicit_id(&json!({ "message_id": "-178436401" })).unwrap(),
            Some("-178436401".to_string())
        );
        assert_eq!(
            explicit_id(&json!({ "message_id": -178436401i64 })).unwrap(),
            Some("-178436401".to_string())
        );
        assert_eq!(
            message_ids_arg(&json!({ "message_ids": ["123", -5, "678"] })).unwrap(),
            vec!["123", "-5", "678"]
        );
        assert!(message_ids_arg(&json!({ "message_ids": ["abc"] })).is_err());
        assert!(message_ids_arg(&json!({})).unwrap().is_empty());
        let too_many: Vec<i64> = (0..21).collect();
        assert!(message_ids_arg(&json!({ "message_ids": too_many })).is_err());
    }

    #[test]
    fn explicit_message_id_must_be_numeric() {
        assert_eq!(
            explicit_id(&json!({ "message_id": "123" })).unwrap(),
            Some("123".to_string())
        );
        assert!(explicit_id(&json!({ "message_id": "abc" })).is_err());
        assert_eq!(
            explicit_id(&json!({ "message_id": 123 })).unwrap(),
            Some("123".to_string())
        );
        assert_eq!(explicit_id(&json!({})).unwrap(), None);
        assert!(explicit_id(&json!({ "message_id": i32::MAX as u64 + 1 })).is_err());
    }

    #[test]
    fn response_contract_is_structured_json() {
        let value: Value =
            serde_json::from_str(&response(true, "ok", json!({ "message_id": "1" })).unwrap())
                .unwrap();
        assert_eq!(value["success"], true);
        assert_eq!(value["message"], "ok");
    }

    #[test]
    fn quoted_group_target_overrides_any_model_argument() {
        let target = select_target(
            Some("current-request-id".to_string()),
            Some("quoted-target-id".to_string()),
        )
        .unwrap();
        assert_eq!(target.message_id, "quoted-target-id");
        assert_eq!(target.source, TargetSource::Reply);

        let target = select_target(None, Some("quoted-target-id".to_string())).unwrap();
        assert_eq!(target.message_id, "quoted-target-id");
        assert_eq!(target.source, TargetSource::Reply);
    }

    #[test]
    fn batch_ids_are_not_hijacked_by_the_reply_target() {
        // 回归:曾经批量路径也走回复定向,当前消息带引用时,传进去的每个 id
        // 都被回复目标顶掉 —— 一批 20 条撤的全是被引用的那一条。
        for id in ["batch-1", "batch-2"] {
            let target = resolve_target(
                Some(id.to_string()),
                None,
                Some("quoted-target-id".to_string()),
            )
            .unwrap();
            assert_eq!(target.message_id, id);
            assert_eq!(target.source, TargetSource::Argument);
        }
        // 非批量仍旧由引用说了算
        let target = resolve_target(None, Some("model-guess".to_string()), Some("quoted".into()))
            .unwrap();
        assert_eq!(target.message_id, "quoted");
        assert_eq!(target.source, TargetSource::Reply);
    }

    #[test]
    fn non_reply_group_target_uses_verified_argument() {
        let target = select_target(Some("history-id".to_string()), None).unwrap();
        assert_eq!(target.message_id, "history-id");
        assert_eq!(target.source, TargetSource::Argument);
        assert!(select_target(None, None).is_err());
    }

    #[test]
    fn napcat_decode_failure_is_non_retryable_and_truthful() {
        let error = anyhow::anyhow!("OneBot API delete_msg failed: retcode=1200, decode failed");
        let value: Value = serde_json::from_str(
            &recall_failure_response(&error, "600025761", TargetSource::Reply, "group_member")
                .unwrap(),
        )
        .unwrap();
        assert_eq!(value["success"], false);
        assert_eq!(value["retryable"], false);
        assert_eq!(value["code"], "napcat_recall_decode_failed");
        assert_eq!(value["data"]["message_id"], "600025761");
        assert!(value["message"].as_str().unwrap().contains("没有被撤回"));
    }
}
