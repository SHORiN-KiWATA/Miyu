mod decision_log;
mod history;
mod inject;
mod pending;
mod runtime;
mod targeting;
use decision_log::*;
use history::*;
use runtime::*;
// onebot 侧也用 safe_prompt_*（拼提示词前的注入边界）
pub(crate) use targeting::safe_prompt_field;
pub(in crate::platforms::plugins::real_context) use targeting::*;
pub(super) mod active_judgement_skip;
pub(crate) mod affection;
pub(crate) mod emotion;
mod judge;

use super::message_history::{self, store, ORIGINAL_TEXT_KEY};
use super::{
    PlatformPersonaResetContext, PlatformPlugin, PlatformTurnInput, PluginDescriptor, PreparedSend,
};
use crate::config::{
    PlatformPluginInstanceConfig, RealContextPluginSettings, REAL_CONTEXT_PLUGIN_ID,
};
use crate::i18n::{text_for, Locale};
use crate::platforms::{
    AdaptiveResponseTargetPolicy, BotSendAvailability, ConversationKind, OutboundBody,
    OutboundMessage, OutboundOrigin, OutboundSegment, PlatformContextFileRef, PlatformInboundEvent,
    PlatformInboundEventKind, PlatformMediaKind, PlatformMention, PlatformTurnContext,
    ResponseTarget, SendReceipt, TriggerDecision,
};
use crate::tools::ToolRegistry;
use anyhow::Result;
use futures_util::future::BoxFuture;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use store::{
    AccountKey, GroupKey, HistoryMessage, HistoryStore, MediaKind, MediaPlaceholder, RecentQuery,
};
#[cfg(test)]
use store::{NewHistoryMessage, SanitizedContent};
use tokio::sync::Notify;

pub(super) struct RealContextPlugin {
    settings_cache: Mutex<
        Option<(
            Option<PlatformPluginInstanceConfig>,
            Arc<RealContextPluginSettings>,
        )>,
    >,
    runtime: Mutex<RuntimeState>,
    global_judge_gate: DynamicGate,
    reaction_expirations: Mutex<HashMap<(String, String, String), tokio::task::AbortHandle>>,
    affection_updates: affection::AffectionUpdateQueue,
}

impl RealContextPlugin {
    pub(super) fn new() -> Self {
        Self {
            settings_cache: Mutex::new(None),
            runtime: Mutex::new(RuntimeState::default()),
            global_judge_gate: DynamicGate::default(),
            reaction_expirations: Mutex::new(HashMap::new()),
            affection_updates: affection::AffectionUpdateQueue::default(),
        }
    }

    fn settings(&self, context: &PlatformTurnContext) -> Result<Arc<RealContextPluginSettings>> {
        let instance = context
            .config
            .platforms
            .qq
            .plugins
            .get(REAL_CONTEXT_PLUGIN_ID);
        let mut cache = self.settings_cache.lock().unwrap();
        if let Some((cached_instance, settings)) = cache.as_ref() {
            if cached_instance.as_ref() == instance {
                return Ok(settings.clone());
            }
        }
        let settings = Arc::new(
            instance
                .map(RealContextPluginSettings::from_instance)
                .transpose()?
                .unwrap_or_default(),
        );
        *cache = Some((instance.cloned(), settings.clone()));
        Ok(settings)
    }

    fn store(&self, context: &PlatformTurnContext) -> HistoryStore {
        message_history::store_for_paths(&context.paths)
    }
}

impl PlatformPlugin for RealContextPlugin {
    fn descriptor(&self) -> PluginDescriptor {
        PluginDescriptor {
            id: REAL_CONTEXT_PLUGIN_ID,
            priority: 200,
            default_enabled: true,
        }
    }

    fn preempt_inbound(
        &self,
        context: &PlatformTurnContext,
        event: &PlatformInboundEvent,
    ) -> Result<bool> {
        if event.kind != PlatformInboundEventKind::Message
            || event.conversation.kind != ConversationKind::Group
        {
            return Ok(false);
        }
        let settings = self.settings(context)?;
        if !settings.active_reply_supersede_enable {
            return Ok(false);
        }
        let now = Instant::now();
        let session_key = runtime_session_key(context);
        let supersede_window = Duration::from_secs(settings.active_reply_supersede_window_seconds);
        let generation = {
            let runtime = self.runtime.lock().unwrap();
            let pending = runtime
                .sessions
                .get(&session_key)
                .and_then(|session| session.pending.get(&event.sender_id));
            // 只接管已承诺的回复(直触发,或判官已放行)。未承诺 = 判官还在
            // 判,没有任何生成可接管;这里一旦放行,没有活跃回合可顶替时的
            // 回落路径会把移植进上下文的目标当成"承诺已成立"免判直回
            // (inject.rs 的 preempted_targets)——08-31 取证:当天三条
            // 「没@我就先旁听」全是这么强起的。按 `PendingReply::committed`
            // 的设计语义,未承诺的覆盖走常规入场:取消旧判断、对新消息
            // 重新判断。
            let Some(pending) = pending.filter(|pending| {
                pending.committed && now.duration_since(pending.started) <= supersede_window
            }) else {
                return Ok(false);
            };
            pending.generation
        };
        match active_judgement_skip::contains(&context.state_store, &event.sender_id) {
            Ok(true) => return Ok(false),
            Ok(false) => {}
            Err(error) => {
                tracing::warn!(
                    target: "miyu::qq",
                    error = %error,
                    sender_id = %event.sender_id,
                    "{}",
                    crate::i18n::text(
                        "failed to read active judgement skip list; skipping supersede",
                        "读取主动判断跳过名单失败；跳过接管当前生成"
                    )
                );
                return Ok(false);
            }
        }
        let targets = {
            let runtime = self.runtime.lock().unwrap();
            runtime
                .sessions
                .get(&session_key)
                .and_then(|session| session.pending.get(&event.sender_id))
                .filter(|pending| {
                    pending.generation == generation
                        && Instant::now().duration_since(pending.started) <= supersede_window
                })
                .map(|pending| pending.targets.clone())
        };
        let Some(targets) = targets else {
            return Ok(false);
        };
        set_active_targets(context, &targets);
        Ok(true)
    }

    fn turn_is_superseded(&self, context: &PlatformTurnContext) -> bool {
        self.runtime
            .lock()
            .unwrap()
            .sessions
            .get(&runtime_session_key(context))
            .and_then(|session| session.pending.get(&context.sender_id))
            .is_some_and(|pending| *pending.cancel.borrow())
    }

    fn confirm_supersede<'a>(
        &'a self,
        context: &'a PlatformTurnContext,
        event: &'a PlatformInboundEvent,
    ) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            let Ok(settings) = self.settings(context) else {
                return;
            };
            let now = Instant::now();
            let session_key = runtime_session_key(context);
            let old_reactions = {
                let mut runtime = self.runtime.lock().unwrap();
                let Some(pending) = runtime
                    .sessions
                    .get_mut(&session_key)
                    .and_then(|session| session.pending.get_mut(&event.sender_id))
                else {
                    return;
                };
                // 链式覆盖:补救窗口从新消息重新起算;目标并入新消息,
                // 旧表情摘出待转移。
                pending.started = now;
                pending.committed = true;
                pending.targets.push(active_reply_target(event));
                normalize_active_targets(&mut pending.targets, &event.sender_id);
                std::mem::take(&mut pending.reactions)
            };
            for (message_id, reaction_id) in old_reactions {
                self.cancel_reaction_expiration(context, &message_id, &reaction_id);
                if let Err(error) = context
                    .set_message_reaction(&message_id, &reaction_id, false)
                    .await
                {
                    tracing::debug!(error = %error, %message_id, "{}", crate::i18n::text("superseded QQ reaction could not be removed", "无法移除已被新消息覆盖的 QQ 表情回应"));
                }
            }
            let reactions = self.add_reactions(context, event, &settings).await;
            let mut runtime = self.runtime.lock().unwrap();
            if let Some(pending) = runtime
                .sessions
                .get_mut(&session_key)
                .and_then(|session| session.pending.get_mut(&event.sender_id))
            {
                pending.reactions = reactions;
            }
        })
    }

    fn after_turn_aborted<'a>(
        &'a self,
        context: &'a PlatformTurnContext,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let settings = self.settings(context)?;
            let session_key = runtime_session_key(context);
            let mut reactions = {
                let mut runtime = self.runtime.lock().unwrap();
                let pending = runtime
                    .sessions
                    .get_mut(&session_key)
                    .and_then(|session| session.pending.get(&context.sender_id));
                if pending.is_some_and(|pending| *pending.cancel.borrow()) {
                    return Ok(());
                }
                runtime
                    .sessions
                    .get_mut(&session_key)
                    .and_then(|session| session.pending.remove(&context.sender_id))
                    .map(|pending| pending.reactions)
                    .unwrap_or_default()
            };
            if reactions.is_empty() && settings.active_reply_reaction_enable {
                if let Some(event) = context
                    .inbound_event()
                    .filter(|event| !event.message_id.is_empty())
                {
                    reactions.extend(
                        settings
                            .active_reply_reaction_emoji_ids
                            .iter()
                            .map(|reaction| (event.message_id.clone(), reaction.to_string())),
                    );
                }
            }
            for (message_id, reaction_id) in reactions {
                self.cancel_reaction_expiration(context, &message_id, &reaction_id);
                if let Err(error) = context
                    .set_message_reaction(&message_id, &reaction_id, false)
                    .await
                {
                    tracing::debug!(error = %error, %message_id, %reaction_id, "{}", crate::i18n::text("aborted QQ reaction could not be removed", "无法移除已中止的 QQ 表情回应"));
                }
            }
            Ok(())
        })
    }

    fn register_tools(
        &self,
        registry: &mut ToolRegistry,
        context: Arc<PlatformTurnContext>,
    ) -> Result<()> {
        let settings = self.settings(&context)?;
        active_judgement_skip::register_tools(registry, context.clone());
        if context.conversation.kind == ConversationKind::Group {
            message_history::register_group_member_tool(
                registry,
                context.clone(),
                settings.group_member_search_max_results,
            );
        } else {
            message_history::register_avatar_tool(registry, context.clone());
        }
        affection::register_query_tool(registry, context.clone(), settings);
        Ok(())
    }

    fn accept_followup(
        &self,
        context: &PlatformTurnContext,
        event: &PlatformInboundEvent,
    ) -> Result<()> {
        let settings = self.settings(context)?;
        adaptive_response_target(context, event, &settings);
        context.remove_plugin_value(REPLY_MARKED_KEY);
        let session_key = runtime_session_key(context);
        if let Some(pending) = self
            .runtime
            .lock()
            .unwrap()
            .sessions
            .get_mut(&session_key)
            .and_then(|session| session.pending.get_mut(&event.sender_id))
        {
            pending.targets.push(active_reply_target(event));
        }
        Ok(())
    }

    fn decide_trigger<'a>(
        &'a self,
        context: &'a PlatformTurnContext,
        event: &'a PlatformInboundEvent,
        decision: &'a mut TriggerDecision,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            if event.kind != PlatformInboundEventKind::Message
                || event.conversation.kind != ConversationKind::Group
            {
                return Ok(());
            }
            let settings = self.settings(context)?;
            self.decide_group_trigger(context, event, decision, &settings)
                .await
        })
    }

    fn before_turn<'a>(
        &'a self,
        context: &'a PlatformTurnContext,
        input: &'a mut PlatformTurnInput,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let settings = self.settings(context)?;
            self.inject_context(context, input, &settings).await
        })
    }

    fn before_send<'a>(
        &'a self,
        _context: &'a PlatformTurnContext,
        mut message: OutboundMessage,
    ) -> BoxFuture<'a, Result<PreparedSend>> {
        Box::pin(async move {
            if !message.metadata.contains_key(ORIGINAL_TEXT_KEY) {
                message.metadata.insert(
                    ORIGINAL_TEXT_KEY.to_string(),
                    Value::String(outbound_text(&message)),
                );
            }
            Ok(PreparedSend::unchanged(message))
        })
    }

    fn after_send<'a>(
        &'a self,
        context: &'a PlatformTurnContext,
        message: &'a OutboundMessage,
        _receipt: &'a SendReceipt,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let settings = self.settings(context)?;
            if matches!(
                message.origin,
                OutboundOrigin::FinalReply | OutboundOrigin::Tool
            ) {
                self.finish_reply(context, message, &settings).await;
            }
            if message.origin == OutboundOrigin::FinalReply
                && context.conversation.kind == ConversationKind::Group
            {
                let trigger = context
                    .plugin_value(TRIGGER_KEY)
                    .and_then(|value| value.as_str().and_then(TriggerKind::parse));
                let direct_interaction = matches!(
                    trigger,
                    Some(TriggerKind::Direct | TriggerKind::Continuation | TriggerKind::Supersede)
                );
                affection::touch_after_reply(context, &settings, direct_interaction)?;
                let reply = message
                    .metadata
                    .get(ORIGINAL_TEXT_KEY)
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .unwrap_or_else(|| outbound_text(message));
                let mut affection_job = None;
                if !reply.trim().is_empty() {
                    let store = self.store(context);
                    affection_job = affection::update_job(
                        context,
                        settings.clone(),
                        store,
                        group_key(context)?,
                        &reply,
                    );
                }
                // 情绪层①在这里;层②(LLM 语义增量)会随好感度更新一起回来,
                // 那种情况下这里只计互动不加分,免得两层叠加。
                let llm_pending = settings.emotion_llm_enrich_enable && affection_job.is_some();
                let facts = emotion::ReplyFacts {
                    direct: direct_interaction,
                    active: matches!(trigger, Some(TriggerKind::Probability)),
                    moderation_hit: context.plugin_value(MODERATION_NOTICE_KEY).is_some(),
                    reply_chars: reply.chars().count(),
                };
                if let Err(error) = emotion::touch_after_reply(context, &settings, &facts, llm_pending) {
                    tracing::warn!(target: "miyu::qq", error = %error, "{}", crate::i18n::text("emotion update after reply failed", "回复后更新情绪状态失败"));
                }
                if let Some(job) = affection_job {
                    self.affection_updates.enqueue(job);
                }
            }
            Ok(())
        })
    }

    fn after_session_reset<'a>(
        &'a self,
        context: &'a PlatformTurnContext,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            if context.conversation.kind != ConversationKind::Group {
                return Ok(());
            }
            self.store(context)
                .reset_context(
                    group_key(context)?,
                    context.config.active_persona_scope(),
                    now_unix(),
                )
                .await?;
            self.runtime
                .lock()
                .unwrap()
                .sessions
                .remove(&runtime_session_key(context));
            Ok(())
        })
    }

    fn after_persona_reset<'a>(
        &'a self,
        context: &'a PlatformPersonaResetContext<'a>,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let persona = context.config.active_persona_scope();
            let store = message_history::store_for_paths(context.paths);
            for binding in context
                .bindings
                .iter()
                .filter(|binding| binding.key.conversation_kind == "group")
            {
                let group = GroupKey::new(
                    binding.key.platform.clone(),
                    binding.key.account_id.clone(),
                    binding.key.conversation_id.clone(),
                )?;
                store
                    .reset_context(group, persona.clone(), now_unix())
                    .await?;
            }

            let mut runtime = self.runtime.lock().unwrap();
            for binding in context.bindings {
                let key = format!(
                    "{}:{}:{}:{}|persona:{}",
                    binding.key.platform,
                    binding.key.account_id,
                    binding.key.conversation_kind,
                    binding.key.conversation_id,
                    persona
                );
                if let Some(session) = runtime.sessions.remove(&key) {
                    for pending in session.pending.into_values() {
                        let _ = pending.cancel.send(true);
                    }
                }
            }
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests;
