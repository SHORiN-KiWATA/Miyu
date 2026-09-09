//! 一次平台回合的上下文对象。
//!
//! 它是插件、工具、适配器之间的**唯一通道**：谁想知道「这条消息来自哪、回给
//! 谁、能不能用主机工具」，都问它。做成一个大对象而不是几个小的，是因为这些
//! 状态互相依赖——回复目标会被插件改，改完工具侧要看到。
//!
//! `take_final_reply_suppression` 那一族处理「工具已经直接发出去了」的情形：
//! 正文里就不该再出现一次。

use crate::platforms::*;

pub(crate) struct PlatformFollowupRun {
    pub(crate) conversation: PlatformConversation,
    pub(crate) sender_id: String,
    pub(crate) context: Arc<PlatformTurnContext>,
    pub(crate) ingress: Arc<QueueIngressBarrier>,
    pub(crate) enqueue: tokio::sync::Mutex<()>,
    pub(crate) started: Instant,
}

impl PlatformFollowupRun {
    pub(crate) fn new(context: Arc<PlatformTurnContext>) -> Arc<Self> {
        Arc::new(Self {
            conversation: context.conversation.clone(),
            sender_id: context.sender_id.clone(),
            context,
            ingress: Arc::new(QueueIngressBarrier::default()),
            enqueue: tokio::sync::Mutex::new(()),
            started: Instant::now(),
        })
    }

    pub(crate) fn ingress(&self) -> Arc<QueueIngressBarrier> {
        self.ingress.clone()
    }

    pub(crate) fn try_reserve(&self) -> Option<QueueIngressReservation> {
        self.ingress.try_reserve()
    }

    pub(crate) async fn lock_enqueue(&self) -> tokio::sync::MutexGuard<'_, ()> {
        self.enqueue.lock().await
    }

    pub(crate) fn started(&self) -> Instant {
        self.started
    }

    pub(crate) fn close(&self) {
        self.ingress.close();
    }
}

pub(crate) struct PlatformTurnContext {
    pub(crate) conversation: PlatformConversation,
    pub(crate) sender_id: String,
    pub(crate) sender_display_name: String,
    pub(crate) is_admin: bool,
    pub(crate) config: AppConfig,
    pub(crate) paths: MiyuPaths,
    pub(crate) state_store: StateStore,
    pub(crate) adapter: Arc<dyn PlatformAdapter>,
    pub(crate) plugins: Arc<plugins::PlatformPluginRegistry>,
    pub(crate) config_manager: Option<Weak<Mutex<crate::runtime::ManagerState>>>,
    pub(crate) inbound_event: Option<Arc<PlatformInboundEvent>>,
    /// 正在处理中登记的 RAII 守卫:context 掉落即注销(见 inflight 模块头)。
    pub(crate) inflight_guard: Option<crate::platforms::inflight::InflightGuard>,
    pub(crate) message_activity: Option<MessageActivityHandle>,
    /// 本回合可看的上下文图片(群里最近若干条消息里的图)。真实回合把它交给
    /// `vision::register_scoped_platform`;MCP 桥另建工具面时也要用同一份,
    /// 否则 claude-code 供应商那边整条看图链路是空的(08-26)。
    pub(crate) context_images: Mutex<Vec<PlatformContextImageRef>>,
    /// 本回合可懒下载的上下文文件/视频引用,用途同上(09-04)。
    pub(crate) context_files: Mutex<Vec<PlatformContextFileRef>>,
    pub(crate) response_target: Mutex<Option<PendingResponseTarget>>,
    pub(crate) group_member_cache: Mutex<HashMap<String, PlatformGroupMember>>,
    pub(crate) plugin_values: Mutex<BTreeMap<String, Value>>,
    pub(crate) delivered_image_digests: Mutex<HashSet<blake3::Hash>>,
    /// 本回合已投递的回复文本(归一化 bigram 集)。文字版幂等闸(08-22 复读
    /// 取证):端点故障日模型会把"发送回复"重演成同义变体,digest 拦不住,
    /// 按近似度拦。仅回合内生效——跨回合相似回复可能是正当的再次回答。
    pub(crate) delivered_reply_texts: Mutex<Vec<DeliveredReplyText>>,
    /// Lazy file refs for queued follow-up prompts, keyed by prompt id.
    pub(crate) queued_files: Mutex<BTreeMap<String, Vec<PlatformContextFileRef>>>,
    pub(crate) reply_rate_available: AtomicBool,
    pub(crate) pending_final_reply_suppression: AtomicBool,
    pub(crate) pending_prior_reply_suppression: AtomicBool,
}

impl PlatformTurnContext {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        conversation: PlatformConversation,
        sender_id: String,
        sender_display_name: String,
        is_admin: bool,
        config: AppConfig,
        paths: MiyuPaths,
        state_store: StateStore,
        adapter: Arc<dyn PlatformAdapter>,
        plugins: Arc<plugins::PlatformPluginRegistry>,
    ) -> Self {
        Self {
            conversation,
            sender_id,
            sender_display_name,
            is_admin,
            config,
            paths,
            state_store,
            adapter,
            plugins,
            config_manager: None,
            inbound_event: None,
            inflight_guard: None,
            message_activity: None,
            context_images: Mutex::new(Vec::new()),
            context_files: Mutex::new(Vec::new()),
            response_target: Mutex::new(None),
            group_member_cache: Mutex::new(HashMap::new()),
            plugin_values: Mutex::new(BTreeMap::new()),
            delivered_image_digests: Mutex::new(HashSet::new()),
            delivered_reply_texts: Mutex::new(Vec::new()),
            queued_files: Mutex::new(BTreeMap::new()),
            reply_rate_available: AtomicBool::new(true),
            pending_final_reply_suppression: AtomicBool::new(false),
            pending_prior_reply_suppression: AtomicBool::new(false),
        }
    }

    pub(crate) fn with_inbound_event(mut self, event: PlatformInboundEvent) -> Self {
        self.inflight_guard = Some(crate::platforms::inflight::InflightGuard::register(
            self.conversation.scope_key(),
            event.message_id.clone(),
        ));
        self.inbound_event = Some(Arc::new(event));
        self
    }

    pub(crate) fn with_message_activity(mut self, activity: MessageActivityHandle) -> Self {
        self.message_activity = Some(activity);
        self
    }

    pub(crate) fn with_config_manager(
        mut self,
        manager: Arc<Mutex<crate::runtime::ManagerState>>,
    ) -> Self {
        self.config_manager = Some(Arc::downgrade(&manager));
        self
    }

    pub(crate) fn with_current_config<T>(&self, read: impl FnOnce(&AppConfig) -> T) -> T {
        match self.config_manager.as_ref().and_then(Weak::upgrade) {
            Some(manager) => read(&manager.lock().unwrap().config),
            None => read(&self.config),
        }
    }

    pub(crate) fn inbound_event(&self) -> Option<&PlatformInboundEvent> {
        self.inbound_event.as_deref()
    }

    pub(crate) fn principal(&self) -> PlatformPrincipal {
        PlatformPrincipal {
            platform: self.conversation.platform.clone(),
            account_id: self.conversation.account_id.clone(),
            user_id: self.sender_id.clone(),
        }
    }

    /// 登记本回合可看的上下文图片。插件算出来之后调一次,供 MCP 桥取用。
    pub(crate) fn set_context_images(&self, images: Vec<PlatformContextImageRef>) {
        *self.context_images.lock().unwrap() = images;
    }

    pub(crate) fn context_images(&self) -> Vec<PlatformContextImageRef> {
        self.context_images.lock().unwrap().clone()
    }

    /// 登记本回合可懒下载的文件引用(历史里的 + 当前消息里的)。
    pub(crate) fn set_context_files(&self, files: Vec<PlatformContextFileRef>) {
        *self.context_files.lock().unwrap() = files;
    }

    pub(crate) fn context_files(&self) -> Vec<PlatformContextFileRef> {
        self.context_files.lock().unwrap().clone()
    }

    pub(crate) fn set_response_target(&self, target: Option<ResponseTarget>) {
        let target = target.filter(ResponseTarget::is_effective);
        let mut pending = self.response_target.lock().unwrap();
        match target {
            Some(target)
                if pending
                    .as_ref()
                    .is_some_and(|existing| existing.target == target) =>
            {
                pending.as_mut().expect("target exists").target = target;
            }
            Some(target) => {
                *pending = Some(PendingResponseTarget {
                    target,
                    policy: None,
                });
            }
            None => *pending = None,
        }
    }

    pub(crate) fn set_adaptive_response_target(
        &self,
        target: Option<ResponseTarget>,
        policy: AdaptiveResponseTargetPolicy,
    ) {
        let mut pending = self.response_target.lock().unwrap();
        let explicit_mentions = pending
            .as_ref()
            .map(|pending| pending.target.explicit_mention_user_ids.clone())
            .filter(|mentions| !mentions.is_empty());
        let target = target.filter(ResponseTarget::is_effective);
        *pending = match (target, explicit_mentions) {
            (Some(mut target), Some(mentions)) => {
                target.mention = false;
                target.explicit_mention_user_ids = mentions;
                Some(PendingResponseTarget {
                    target,
                    policy: Some(policy),
                })
            }
            (Some(target), None) => Some(PendingResponseTarget {
                target,
                policy: Some(policy),
            }),
            (None, Some(mentions)) => Some(PendingResponseTarget {
                target: ResponseTarget {
                    message_id: String::new(),
                    user_id: String::new(),
                    quote: false,
                    mention: false,
                    explicit_mention_user_ids: mentions,
                },
                policy: None,
            }),
            (None, None) => None,
        };
    }

    pub(crate) fn response_target(&self) -> Option<ResponseTarget> {
        self.response_target
            .lock()
            .unwrap()
            .as_ref()
            .map(|pending| pending.target.clone())
    }

    pub(crate) fn set_explicit_response_mentions(&self, user_ids: Vec<String>) {
        if user_ids.is_empty() {
            return;
        }
        let mut pending = self.response_target.lock().unwrap();
        if let Some(pending) = pending.as_mut() {
            pending.target.mention = false;
            pending.target.explicit_mention_user_ids = user_ids;
        } else {
            *pending = Some(PendingResponseTarget {
                target: ResponseTarget {
                    message_id: String::new(),
                    user_id: String::new(),
                    quote: false,
                    mention: false,
                    explicit_mention_user_ids: user_ids,
                },
                policy: None,
            });
        }
    }

    pub(crate) fn set_plugin_value(&self, key: impl Into<String>, value: Value) {
        self.plugin_values.lock().unwrap().insert(key.into(), value);
    }

    pub(crate) fn remove_plugin_value(&self, key: &str) {
        self.plugin_values.lock().unwrap().remove(key);
    }

    pub(crate) fn plugin_value(&self, key: &str) -> Option<Value> {
        self.plugin_values.lock().unwrap().get(key).cloned()
    }

    pub(crate) fn set_reply_rate_available(&self, available: bool) {
        self.reply_rate_available
            .store(available, Ordering::Release);
    }

    pub(crate) fn reply_rate_available(&self) -> bool {
        self.reply_rate_available.load(Ordering::Acquire)
    }

    pub(crate) fn plugin_enabled(&self, id: &str, default_enabled: bool) -> bool {
        self.config
            .platforms
            .qq
            .plugins
            .get(id)
            .and_then(|plugin| plugin.enabled)
            .unwrap_or(default_enabled)
    }

    /// 平台生图豁免：管理员，或私聊白名单成员（静态配置 ∪ 动态授权）。
    /// 与 `allow_non_admin_host_tools` 解耦——那是宿主工具的开关，不管生图。
    pub(crate) fn image_generation_unlimited(&self) -> bool {
        if self.is_admin {
            return true;
        }
        if self.conversation.kind != ConversationKind::Private {
            return false;
        }
        let statically_whitelisted = self.sender_id.parse::<i64>().ok().is_some_and(|sender| {
            self.config
                .platforms
                .qq
                .private_chats
                .whitelist
                .contains(&sender)
        });
        statically_whitelisted
            || access_control::has_dynamic_access(
                &self.state_store,
                &self.conversation.account_id,
                access_control::AccessPermission::PrivateWhitelist,
                &self.sender_id,
            )
    }

    pub(crate) fn host_tools_allowed(&self) -> bool {
        if self.is_admin {
            return true;
        }
        self.conversation.kind == ConversationKind::Private
            && self.config.platforms.qq.allow_non_admin_host_tools
            && self.sender_id.parse::<i64>().ok().is_some_and(|sender| {
                self.config
                    .platforms
                    .qq
                    .private_chats
                    .whitelist
                    .contains(&sender)
                    || access_control::has_dynamic_access(
                        &self.state_store,
                        &self.conversation.account_id,
                        access_control::AccessPermission::PrivateWhitelist,
                        &self.sender_id,
                    )
            })
    }

    pub(crate) async fn handle_command(&self, text: &str) -> Option<OutboundMessage> {
        self.plugins.handle_command(self, text).await
    }

    pub(crate) async fn prepare_turn(&self, content: String) -> plugins::PlatformTurnInput {
        let mut input = plugins::PlatformTurnInput {
            memory_content: content.clone(),
            content,
            system_context: Vec::new(),
            turn_system_context: Vec::new(),
            context_images: Vec::new(),
            context_files: Vec::new(),
        };
        self.plugins.before_turn(self, &mut input).await;
        input
    }

    pub(crate) async fn observe_inbound(&self, event: &PlatformInboundEvent) {
        self.plugins.observe_inbound(self, event).await;
    }

    pub(crate) fn accept_followup(&self, event: &PlatformInboundEvent) {
        self.plugins.accept_followup(self, event);
    }

    pub(crate) fn preempt_inbound(&self, event: &PlatformInboundEvent) -> bool {
        self.plugins.preempt_inbound(self, event)
    }

    pub(crate) async fn confirm_supersede(&self, event: &PlatformInboundEvent) {
        self.plugins.confirm_supersede(self, event).await;
    }

    pub(crate) fn turn_is_superseded(&self) -> bool {
        self.plugins.turn_is_superseded(self)
    }

    pub(crate) fn turn_started(&self, cancel: tokio::sync::watch::Sender<bool>) {
        self.plugins.turn_started(self, cancel);
    }

    pub(crate) async fn after_turn_aborted(&self) {
        self.plugins.after_turn_aborted(self).await;
    }

    pub(crate) async fn decide_trigger(
        &self,
        event: &PlatformInboundEvent,
        decision: &mut TriggerDecision,
    ) {
        self.plugins.decide_trigger(self, event, decision).await;
    }

    pub(crate) async fn after_session_reset(&self) -> Result<()> {
        self.plugins.after_session_reset(self).await
    }

    pub(crate) async fn send(&self, mut message: OutboundMessage) -> Result<SendReceipt> {
        // 工具模板泄漏过滤:模型复读退化时会把 <tool_call>/<function=> 模板
        // 当正文吐出(08-23 取证),剥掉泄漏 span,剥空就整条抑制。
        if matches!(
            message.origin,
            OutboundOrigin::FinalReply | OutboundOrigin::IntermediateReply | OutboundOrigin::Tool
        ) && strip_tool_call_leaks(&mut message)
        {
            tracing::warn!(
                platform = %self.conversation.platform,
                conversation_kind = self.conversation.kind.as_str(),
                conversation_id = %self.conversation.conversation_id,
                "{}",
                crate::i18n::text(
                    "stripped a leaked tool-call template from an outgoing reply",
                    "已从出站回复中剥离泄漏的工具调用模板",
                )
            );
            if !message_has_deliverable_content(&message) {
                return Ok(SendReceipt::default());
            }
        }
        if matches!(
            message.origin,
            OutboundOrigin::FinalReply | OutboundOrigin::IntermediateReply | OutboundOrigin::Tool
        ) && message_is_parenthetical_only(&message)
        {
            tracing::info!(
                platform = %self.conversation.platform,
                conversation_kind = self.conversation.kind.as_str(),
                conversation_id = %self.conversation.conversation_id,
                "{}",
                crate::i18n::text(
                    "suppressed a parenthetical-only model reply",
                    "已抑制仅含括号内容的模型回复",
                )
            );
            return Ok(SendReceipt::default());
        }
        let reserved_target = if message.response_target.is_none()
            && matches!(
                message.origin,
                OutboundOrigin::FinalReply | OutboundOrigin::Tool
            ) {
            self.response_target.lock().unwrap().take()
        } else {
            None
        };
        if let Some(target) = reserved_target.as_ref() {
            message.response_target = Some(target.target.clone());
        }
        let mut prepared = self.plugins.before_send(self, message).await;
        if let Some(target) = reserved_target.as_ref() {
            let current = self
                .message_activity
                .as_ref()
                .map(|activity| activity.position_for(&target.target.user_id));
            let last_message_is_own = self
                .message_activity
                .as_ref()
                .is_some_and(|activity| activity.last_message_is_own());
            let resolved = target
                .policy
                .and_then(|policy| {
                    policy.resolve(
                        target.target.clone(),
                        current,
                        last_message_is_own,
                        Instant::now(),
                    )
                })
                .or_else(|| target.policy.is_none().then(|| target.target.clone()));
            apply_resolved_response_target(
                &mut prepared.primary,
                &target.target,
                resolved.as_ref(),
            );
            if let Some(fallback) = prepared.fallback.as_mut() {
                apply_resolved_response_target(fallback, &target.target, resolved.as_ref());
            }
        }
        let primary = prepared.primary;
        let delivered = match self.adapter.send(primary.clone()).await {
            Ok(receipt) => Ok((primary, receipt, true)),
            Err(error) => {
                let (partially_delivered, response_target_delivered) =
                    self.record_partial_delivery(&error);
                match (partially_delivered, prepared.fallback) {
                    (true, _) => {
                        // `miyu::qq` and not the module default: these are
                        // delivery outcomes an operator reads next to the
                        // "回复已投递" lines, and every other target is filtered
                        // to ERROR unless MIYU_LOG says otherwise (see
                        // `logging::init`), which kept this whole branch
                        // invisible in the QQ log.
                        tracing::warn!(
                            target: "miyu::qq",
                            error = %error,
                            "{}",
                            crate::i18n::text(
                                "platform message partially succeeded; skipped the full fallback to avoid duplicate delivery",
                                "平台消息部分发送成功；为避免重复投递，已跳过完整回退消息",
                            )
                        );
                        Err((error, response_target_delivered))
                    }
                    (false, Some(fallback)) => {
                        tracing::warn!(target: "miyu::qq", error = %error, "{}", crate::i18n::text("transformed platform message failed; sending fallback", "转换后的平台消息发送失败；正在发送回退消息"));
                        match self.adapter.send(fallback.clone()).await {
                            Ok(receipt) => Ok((fallback, receipt, false)),
                            Err(error) => {
                                let (_, response_target_delivered) =
                                    self.record_partial_delivery(&error);
                                Err((error, response_target_delivered))
                            }
                        }
                    }
                    (false, None) => Err((error, false)),
                }
            }
        };
        let (delivered_message, receipt, transformed_primary_succeeded) = match delivered {
            Ok(delivered) => delivered,
            Err((error, response_target_delivered)) => {
                if !response_target_delivered {
                    if let Some(target) = reserved_target {
                        self.restore_response_target(target);
                    }
                }
                return Err(error);
            }
        };
        self.record_delivered_images(&receipt);
        // 投递成功 = 此刻会话里最后一条是她自己的。下一条回复据此决定要不要
        // 引用(连发时艾特和引用会一起哑火,见 AdaptiveResponseTargetPolicy)。
        if let Some(activity) = self.message_activity.as_ref() {
            activity.record_own_message();
        }
        self.plugins
            .after_send(self, &delivered_message, &receipt)
            .await;
        for message in prepared.after_success {
            let history_text = outbound_text_for_history(&message);
            match self.adapter.send(message).await {
                Ok(receipt) => {
                    self.record_delivered_images(&receipt);
                    let message_id = receipt
                        .message_ids
                        .first()
                        .map(String::as_str)
                        .unwrap_or("");
                    self.plugins
                        .record_external_bot_message(self, message_id, &history_text)
                        .await;
                }
                Err(error) => {
                    let _ = self.record_partial_delivery(&error);
                    tracing::warn!(target: "miyu::qq", error = %error, "{}", crate::i18n::text("platform plugin follow-up send failed", "平台插件后续消息发送失败"));
                }
            }
        }
        if prepared.suppress_final_reply
            && transformed_primary_succeeded
            && delivered_message.origin == OutboundOrigin::Tool
        {
            self.pending_final_reply_suppression
                .store(true, Ordering::Release);
            if prepared.suppress_prior_reply {
                self.pending_prior_reply_suppression
                    .store(true, Ordering::Release);
            }
        }
        Ok(receipt)
    }

    /// 直发，不过 `before_send`/`after_send`（历史仍经
    /// `record_external_bot_message` 记一份）。
    ///
    /// 只留给**故障路径上的短通知**：限流提示、会话解析失败、回合异常。
    /// 这些消息恰恰在插件可能不可用或正是问题来源的时候要发出去，多一层
    /// 依赖就多一次发不出的机会；而且都短到不触发任何插件的阈值，绕过与
    /// 否看不出差别。
    ///
    /// 命令输出**不属于**这一类——它和模型回复一样是正常产物，走 `send()`
    /// 过回复处理插件（长清单转图片等）。
    pub(crate) async fn send_bypass_plugins(
        &self,
        message: OutboundMessage,
    ) -> Result<SendReceipt> {
        let history_text = outbound_text_for_history(&message);
        match self.adapter.send(message).await {
            Ok(receipt) => {
                self.record_delivered_images(&receipt);
                let message_id = receipt
                    .message_ids
                    .first()
                    .map(String::as_str)
                    .unwrap_or("");
                self.plugins
                    .record_external_bot_message(self, message_id, &history_text)
                    .await;
                Ok(receipt)
            }
            Err(error) => {
                let _ = self.record_partial_delivery(&error);
                Err(error)
            }
        }
    }

    pub(crate) fn is_duplicate_reply_text(&self, text: &str) -> bool {
        if self.repeats_delivered_reply_text(text) {
            return true;
        }
        let grams = reply_text_bigrams(text);
        // 短文本(问候/单句确认)天然高相似,近似闸不管它们。
        if grams.len() < 16 {
            return false;
        }
        // 跨回合支路(08-25 并发重复答题取证):同会话 2 分钟内投过的近似
        // 内容拒发,阈值 0.75 比回合内更严。
        crate::platforms::activity::recent_conversation_reply_similar(
            &self.conversation.scope_key(),
            text,
        )
    }

    /// 本回合内已经由工具发出去的文本,最终回复再发一遍就是复读(09-09 生图后
    /// 「画好了,拿去当壁纸吧」发了两次):归一化后逐字相同直接算重复,短句也算;
    /// 长文本再看 bigram 近似。只看本回合,跨回合那条更严的闸不在这里。
    pub(crate) fn repeats_delivered_reply_text(&self, text: &str) -> bool {
        let normalized = reply_text_normalized(text);
        if normalized.is_empty() {
            return false;
        }
        let grams = reply_text_bigrams(text);
        let delivered = self.delivered_reply_texts.lock().unwrap();
        delivered.iter().any(|prev| {
            prev.normalized == normalized
                || (grams.len() >= 16 && bigram_jaccard(&grams, &prev.grams) >= 0.66)
        })
    }

    pub(crate) fn record_delivered_reply_text(&self, text: &str) {
        crate::platforms::activity::record_recent_conversation_reply(
            &self.conversation.scope_key(),
            text,
        );
        let normalized = reply_text_normalized(text);
        if normalized.is_empty() {
            return;
        }
        self.delivered_reply_texts
            .lock()
            .unwrap()
            .push(DeliveredReplyText {
                grams: reply_text_bigrams(text),
                normalized,
            });
    }

    pub(crate) fn record_delivered_images(&self, receipt: &SendReceipt) {
        if receipt.image_digests.is_empty() {
            return;
        }
        self.delivered_image_digests
            .lock()
            .unwrap()
            .extend(receipt.image_digests.iter().copied());
        record_recent_conversation_images(&self.conversation.scope_key(), &receipt.image_digests);
    }

    pub(crate) fn record_partial_delivery(&self, error: &anyhow::Error) -> (bool, bool) {
        let Some(partial) = error.downcast_ref::<PartialSendError>() else {
            return (false, false);
        };
        self.record_delivered_images(partial.receipt());
        (
            partial.receipt().has_delivery(),
            partial.receipt().response_target_delivered,
        )
    }

    pub(crate) fn restore_response_target(&self, target: PendingResponseTarget) {
        let mut available = self.response_target.lock().unwrap();
        match available.as_mut() {
            Some(current)
                if current.target.explicit_mention_user_ids.is_empty()
                    && !target.target.explicit_mention_user_ids.is_empty() =>
            {
                current.target.mention = false;
                current.target.explicit_mention_user_ids = target.target.explicit_mention_user_ids;
            }
            Some(_) => {}
            None => *available = Some(target),
        }
    }

    pub(crate) fn delivered_image_digests(&self) -> HashSet<blake3::Hash> {
        let mut digests = self.delivered_image_digests.lock().unwrap().clone();
        digests.extend(recent_conversation_images(&self.conversation.scope_key()));
        digests
    }

    pub(crate) async fn bot_display_name(&self) -> Result<String> {
        self.adapter.bot_display_name().await
    }

    pub(crate) async fn bot_send_availability(&self) -> crate::platform_types::BotSendAvailability {
        match self.adapter.bot_send_availability().await {
            Ok(availability) => availability,
            Err(error) => {
                tracing::debug!(error = %error, "{}", crate::i18n::text("platform bot send availability lookup failed", "平台机器人发送可用性查询失败"));
                crate::platform_types::BotSendAvailability::Unknown
            }
        }
    }

    pub(crate) async fn set_message_reaction(
        &self,
        message_id: &str,
        reaction_id: &str,
        active: bool,
    ) -> Result<()> {
        self.adapter
            .set_message_reaction(message_id, reaction_id, active)
            .await
    }

    pub(crate) fn schedule_message_reaction_removal(
        &self,
        message_id: String,
        reaction_id: String,
        delay: Duration,
    ) -> tokio::task::AbortHandle {
        let adapter = self.adapter.clone();
        tokio::spawn(async move {
            tokio::time::sleep(delay).await;
            if let Err(error) = adapter
                .set_message_reaction(&message_id, &reaction_id, false)
                .await
            {
                tracing::debug!(
                    error = %error,
                    %message_id,
                    %reaction_id,
                    "{}",
                    crate::i18n::text(
                        "expired platform reaction could not be removed",
                        "无法移除已过期的平台表情回应",
                    )
                );
            }
        })
        .abort_handle()
    }

    pub(crate) async fn message_info(
        &self,
        message_id: &str,
    ) -> Result<Option<PlatformMessageInfo>> {
        self.adapter.message_info(message_id).await
    }

    pub(crate) fn message_images_task(
        &self,
        message_id: String,
    ) -> futures_util::future::BoxFuture<'static, Result<Vec<PlatformImageData>>> {
        let adapter = self.adapter.clone();
        Box::pin(async move { adapter.message_images(&message_id).await })
    }

    pub(crate) fn fetch_platform_file_task(
        &self,
        file_ref: PlatformContextFileRef,
    ) -> futures_util::future::BoxFuture<'static, Result<PlatformFileDownload>> {
        let adapter = self.adapter.clone();
        let paths = self.paths.clone();
        Box::pin(async move { adapter.fetch_platform_file(&file_ref, &paths).await })
    }

    pub(crate) async fn group_members(&self) -> Result<Vec<PlatformGroupMember>> {
        let members = self.adapter.group_members().await?;
        self.group_member_cache.lock().unwrap().extend(
            members
                .iter()
                .cloned()
                .map(|member| (member.user_id.clone(), member)),
        );
        Ok(members)
    }

    /// Store lazy file refs for a queued prompt until the running agent claims
    /// them before its next model request.
    pub(crate) fn stash_queued_files(&self, prompt_id: &str, files: Vec<PlatformContextFileRef>) {
        self.queued_files
            .lock()
            .unwrap()
            .insert(prompt_id.to_string(), files);
    }

    pub(crate) fn take_queued_files(&self, prompt_id: &str) -> Vec<PlatformContextFileRef> {
        self.queued_files
            .lock()
            .unwrap()
            .remove(prompt_id)
            .unwrap_or_default()
    }

    /// Resolve one `read_platform_file` context id into a locally cached file.
    pub(crate) async fn fetch_platform_file(
        &self,
        file_ref: &PlatformContextFileRef,
    ) -> Result<PlatformFileDownload> {
        self.adapter
            .fetch_platform_file(file_ref, &self.paths)
            .await
    }

    pub(crate) async fn group_member(&self, user_id: &str) -> Result<Option<PlatformGroupMember>> {
        if let Some(member) = self
            .group_member_cache
            .lock()
            .unwrap()
            .get(user_id)
            .cloned()
        {
            return Ok(Some(member));
        }
        let member = self.adapter.group_member(user_id).await?;
        if let Some(member) = member.as_ref() {
            self.group_member_cache
                .lock()
                .unwrap()
                .insert(member.user_id.clone(), member.clone());
        }
        Ok(member)
    }

    /// Membership as the server sees it *now*, skipping both the per-turn cache
    /// and the platform's roster cache. Destructive actions validate through
    /// this so a member who already left is refused here instead of failing
    /// deep inside the bridge.
    pub(crate) async fn group_member_fresh(
        &self,
        user_id: &str,
    ) -> Result<Option<PlatformGroupMember>> {
        let member = self.adapter.group_member_fresh(user_id).await?;
        let mut cache = self.group_member_cache.lock().unwrap();
        match member.as_ref() {
            Some(member) => {
                cache.insert(member.user_id.clone(), member.clone());
            }
            None => {
                cache.remove(user_id);
            }
        }
        Ok(member)
    }

    /// Drops a member from the per-turn cache — used when a leave/kick notice
    /// arrives so later lookups in the same turn cannot resurrect them.
    pub(crate) fn forget_group_member(&self, user_id: &str) {
        self.group_member_cache.lock().unwrap().remove(user_id);
    }

    pub(crate) async fn bot_group_role(&self) -> crate::platform_types::BotGroupRole {
        self.adapter
            .bot_group_role()
            .await
            .unwrap_or(crate::platform_types::BotGroupRole::Unknown)
    }

    pub(crate) async fn delete_message(&self, message_id: &str) -> Result<()> {
        self.adapter.delete_message(message_id).await
    }

    pub(crate) async fn set_group_ban(&self, user_id: &str, duration_seconds: u64) -> Result<()> {
        self.adapter.set_group_ban(user_id, duration_seconds).await
    }

    pub(crate) async fn set_group_kick(
        &self,
        user_id: &str,
        reject_add_request: bool,
    ) -> Result<()> {
        self.adapter
            .set_group_kick(user_id, reject_add_request)
            .await
    }

    pub(crate) async fn set_group_special_title(
        &self,
        user_id: &str,
        special_title: &str,
        duration_seconds: i64,
    ) -> Result<()> {
        self.adapter
            .set_group_special_title(user_id, special_title, duration_seconds)
            .await
    }

    pub(crate) async fn record_external_bot_message(&self, message_id: &str, text: &str) {
        self.plugins
            .record_external_bot_message(self, message_id, text)
            .await;
    }

    pub(crate) fn take_final_reply_suppression(&self) -> bool {
        let suppress = self
            .pending_final_reply_suppression
            .swap(false, Ordering::AcqRel);
        self.pending_prior_reply_suppression
            .store(false, Ordering::Release);
        suppress
    }

    pub(crate) fn take_final_reply_suppression_start(&self, text_len: usize) -> Option<usize> {
        if !self
            .pending_final_reply_suppression
            .swap(false, Ordering::AcqRel)
        {
            return None;
        }
        let suppress_prior = self
            .pending_prior_reply_suppression
            .swap(false, Ordering::AcqRel);
        Some(if suppress_prior { 0 } else { text_len })
    }
}

pub(crate) fn register_platform_tools(
    registry: &mut crate::tools::ToolRegistry,
    context: Arc<PlatformTurnContext>,
) {
    tool::register(registry, context.clone());
    context.plugins.register_tools(registry, context.clone());
}

/// 本回合已投递的一条文本:归一化原文(逐字比对用)与 bigram 集(近似比对用)。
pub(crate) struct DeliveredReplyText {
    pub(crate) normalized: String,
    pub(crate) grams: std::collections::HashSet<(char, char)>,
}

/// 去标点空白、小写后的文本。
pub(crate) fn reply_text_normalized(text: &str) -> String {
    text.chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

/// 归一化(去标点空白、小写)后的字符 bigram 集。同义改写变体的用词高度
/// 重叠,bigram Jaccard 是廉价且够用的近似度。
pub(crate) fn reply_text_bigrams(text: &str) -> std::collections::HashSet<(char, char)> {
    let normalized: Vec<char> = reply_text_normalized(text).chars().collect();
    normalized
        .windows(2)
        .map(|pair| (pair[0], pair[1]))
        .collect()
}

pub(crate) fn bigram_jaccard(
    a: &std::collections::HashSet<(char, char)>,
    b: &std::collections::HashSet<(char, char)>,
) -> f64 {
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    let intersection = a.intersection(b).count();
    let union = a.len() + b.len() - intersection;
    intersection as f64 / union as f64
}
