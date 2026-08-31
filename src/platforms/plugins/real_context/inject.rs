//! 往回合里注入群上下文，以及收尾。
//!
//! `inject_context` 把群历史、身份、好感度拼成一段塞进提示词——这段进的是前缀，
//! 顺序和措辞必须稳定（见 AGENTS.md 1.1）。
//!
//! `finish_reply` 是回合结束后的登记：更新水位、消掉待发、记录这次回了什么。

use crate::platforms::plugins::real_context::*;

impl RealContextPlugin {
    pub(in crate::platforms::plugins::real_context) async fn decide_group_trigger(
        &self,
        context: &PlatformTurnContext,
        event: &PlatformInboundEvent,
        decision: &mut TriggerDecision,
        settings: &RealContextPluginSettings,
    ) -> Result<()> {
        let system_triggered = decision.should_reply;
        if system_triggered {
            let target = adaptive_response_target(context, event, settings);
            decision.response_target = target.clone();
        }
        let core_fallback = system_triggered.then(|| decision.clone());

        if !context.reply_rate_available() {
            self.clear_cancelled_pending(context, &event.sender_id)
                .await;
            decision.should_reply = system_triggered;
            // 限额耗尽时直触发照样回复,但整段主动判断被跳过。这里不写
            // TRIGGER_KEY 的话,下游拿不到本轮的唤醒理由(08-29:注入块因此
            // 整条哑火,排查时被误判成"改了没生效")。
            if decision.should_reply {
                context.set_plugin_value(
                    TRIGGER_KEY,
                    Value::String(TriggerKind::Direct.as_str().to_string()),
                );
                self.log_bypass(
                    context,
                    TriggerKind::Direct,
                    "回复限额已用尽，本轮不做主动判断",
                );
            }
            return Ok(());
        }

        let decoded_base64 = if settings.base64_moderation_enable && settings.moderation_enable {
            judge::decode_base64_text(
                &event.text,
                settings.base64_moderation_min_chars,
                settings.base64_moderation_max_decoded_chars,
                settings.base64_moderation_min_printable_ratio,
            )
        } else {
            String::new()
        };
        let moderation_keyword = settings.moderation_enable
            && settings.moderation_keyword_trigger_enable
            && (find_keyword(&settings.moderation_keywords, &event.text).is_some()
                || (!decoded_base64.is_empty()
                    && find_keyword(&settings.moderation_keywords, &decoded_base64).is_some()));
        let moderation_candidate = moderation_keyword;
        let privileged_sender = context.is_admin
            || context
                .sender_id
                .parse::<i64>()
                .ok()
                .is_some_and(|sender_id| {
                    context
                        .config
                        .platforms
                        .qq
                        .private_chats
                        .whitelist
                        .contains(&sender_id)
                });
        let active_judgement_without_skip =
            active_judgement_allowed(settings, system_triggered, privileged_sender, false);
        let skip_active_judgement = active_judgement_without_skip
            && match active_judgement_skip::contains(&context.state_store, &event.sender_id) {
                Ok(skip) => skip,
                Err(error) => {
                    tracing::warn!(
                        target: "miyu::qq",
                        error = %error,
                        sender_id = %event.sender_id,
                        "{}",
                        crate::i18n::text(
                            "failed to read active judgement skip list; skipping social judgement",
                            "读取主动判断跳过名单失败；跳过社交主动判断"
                        )
                    );
                    true
                }
            };
        let active_judgement_allowed = active_judgement_without_skip && !skip_active_judgement;
        if !active_judgement_allowed && !moderation_candidate {
            if system_triggered
                && context.bot_send_availability().await == BotSendAvailability::Muted
            {
                self.clear_cancelled_pending(context, &event.sender_id)
                    .await;
                decision.should_reply = false;
                return Ok(());
            }
            self.clear_cancelled_pending(context, &event.sender_id)
                .await;
            if let Some(fallback) = core_fallback.as_ref() {
                restore_core_trigger(context, decision, fallback);
            }
            if decision.should_reply {
                context.set_plugin_value(
                    TRIGGER_KEY,
                    Value::String(TriggerKind::Direct.as_str().to_string()),
                );
                self.commit_direct_reply(context, event, settings).await;
                self.log_bypass(context, TriggerKind::Direct, "本会话不做主动判断");
            }
            return Ok(());
        }
        let now = Instant::now();
        let session_key = runtime_session_key(context);
        let preempted_targets = active_targets_from_context(context);
        let (
            continuation,
            inherited,
            inherited_committed,
            old_reactions,
            mut inherited_targets,
            heat,
        ) = {
            let mut runtime = self.runtime.lock().unwrap();
            runtime.prune(now);
            let session = runtime.session_mut(&session_key, now);
            session.decay_heat(now, settings.reply_restraint_recover_minutes);
            let continuation =
                session.continuation_match(&event.sender_id, now, settings.continuation_enable);
            let pending = session.pending.get(&event.sender_id).filter(|pending| {
                now.duration_since(pending.started)
                    <= Duration::from_secs(settings.active_reply_supersede_window_seconds)
            });
            let inherited = settings.active_reply_supersede_enable
                && (!preempted_targets.is_empty() || pending.is_some());
            // 承诺已成立(preempt 回落 = 生成早已开始;或旧 pending 已 committed)
            // 时,补救消息直接顶替,不再重新判断。
            let inherited_committed = inherited
                && (!preempted_targets.is_empty()
                    || pending.is_some_and(|pending| pending.committed));
            let old_reactions = if inherited {
                pending
                    .map(|pending| pending.reactions.clone())
                    .unwrap_or_default()
            } else {
                Vec::new()
            };
            let inherited_targets = if inherited {
                if preempted_targets.is_empty() {
                    pending
                        .map(|pending| pending.targets.clone())
                        .unwrap_or_default()
                } else {
                    preempted_targets.clone()
                }
            } else {
                Vec::new()
            };
            (
                continuation,
                inherited,
                inherited_committed,
                old_reactions,
                inherited_targets,
                session.heat,
            )
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

        let pure_image = event.text.trim().is_empty()
            && !event.media.is_empty()
            && event.media.iter().all(|media| {
                matches!(
                    media.kind,
                    PlatformMediaKind::Image | PlatformMediaKind::Emoji
                )
            });
        let probabilistic = !pure_image || !settings.skip_pure_image_active_judge;
        let probabilistic = probabilistic
            && rand::random::<f64>() < settings.active_judge_probability.clamp(0.0, 1.0);
        // When a direct platform trigger is intentionally not being taken over,
        // a moderation candidate must use the moderation-only judge mode. It
        // still restores the original core trigger below, but the safety
        // check should not spend a full social-reply score or accidentally
        // turn a keyword hit into an unsolicited response.
        let trigger = select_trigger_for_policy(
            active_judgement_allowed,
            system_triggered,
            moderation_candidate,
            inherited,
            continuation,
            probabilistic,
        );
        decision.should_reply = false;
        let Some(trigger) = trigger else {
            return Ok(());
        };
        inherited_targets.push(active_reply_target(event));
        normalize_active_targets(&mut inherited_targets, &event.sender_id);
        set_active_targets(context, &inherited_targets);
        if context.bot_send_availability().await == BotSendAvailability::Muted {
            self.clear_cancelled_pending(context, &event.sender_id)
                .await;
            return Ok(());
        }
        if inherited_committed {
            // 覆盖窗口的语义是「发错了马上改」:回复承诺已成立,补救消息
            // 沿用结论直接顶替,表情随之转移(旧的已在上方摘除)。
            context.set_plugin_value(TRIGGER_KEY, Value::String(trigger.as_str().to_string()));
            decision.should_reply = true;
            decision.response_target = adaptive_response_target(context, event, settings);
            let reactions = self.add_reactions(context, event, settings).await;
            self.register_committed_pending(
                &session_key,
                &event.sender_id,
                trigger,
                reactions,
                inherited_targets,
            );
            self.log_bypass(context, trigger, "覆盖窗口内沿用上一轮已承诺的回复");
            return Ok(());
        }
        let (cancel_tx, mut cancel_rx) = tokio::sync::watch::channel(false);
        let generation = {
            let mut runtime = self.runtime.lock().unwrap();
            runtime.next_generation = runtime.next_generation.wrapping_add(1).max(1);
            let generation = runtime.next_generation;
            runtime.session_mut(&session_key, now).pending.insert(
                event.sender_id.clone(),
                PendingReply {
                    generation,
                    started: now,
                    trigger,
                    committed: false,
                    reactions: Vec::new(),
                    targets: inherited_targets,
                    cancel: cancel_tx,
                },
            );
            generation
        };

        let _global_permit = match tokio::select! {
            biased;
            _ = wait_for_supersede(&mut cancel_rx) => {
                self.log_skip(context, settings, trigger, "并发等待期间已被同一用户的新消息覆盖");
                return Ok(());
            }
            permit = self.global_judge_gate.acquire(
                settings.judge_max_concurrency,
                Duration::from_secs(settings.judge_queue_wait_timeout_seconds),
            ) => permit,
        } {
            Some(permit) => permit,
            None => {
                self.fail_current_attempt(
                    context,
                    decision,
                    core_fallback.as_ref(),
                    &session_key,
                    &event.sender_id,
                    generation,
                    settings,
                    trigger,
                    "全局主动判断并发等待超时",
                );
                return Ok(());
            }
        };
        if !self.is_current_pending(&session_key, &event.sender_id, generation) {
            self.log_skip(
                context,
                settings,
                trigger,
                "排队期间已被同一用户的新消息覆盖",
            );
            return Ok(());
        }

        let history = async {
            self.store(context)
                .recent(
                    RecentQuery::for_context(
                        group_key(context)?,
                        context.config.active_persona_scope(),
                        history_query_limit(settings.judge_context_window),
                    )
                    .before_ingress_order(event.ingress_order),
                )
                .await
                .map(|page| page.messages)
        }
        .await;
        let mut history = match history {
            Ok(history) => history,
            Err(error) => {
                self.fail_current_attempt(
                    context,
                    decision,
                    core_fallback.as_ref(),
                    &session_key,
                    &event.sender_id,
                    generation,
                    settings,
                    trigger,
                    "读取真实群聊历史失败",
                );
                tracing::warn!(
                    target: "miyu::qq",
                    error = %error,
                    group_id = %event.conversation.conversation_id,
                    sender_id = %event.sender_id,
                    "{}",
                    crate::i18n::text(
                        "real-context history lookup failed before active reply judge",
                        "主动回复判断前查询真实上下文历史失败",
                    )
                );
                return Ok(());
            }
        };
        prepare_history(
            &mut history,
            &event.message_id,
            settings.judge_context_window,
        );
        let (heat_penalty, heat_threshold_boost) = restraint_adjustments(
            settings.reply_restraint_enable,
            &settings.reply_restraint_strength,
            heat,
        );
        let continuation_boost = matches!(trigger, TriggerKind::Continuation) as u8 as f64
            * settings.continuation_boost_score;
        let system_boost = matches!(trigger, TriggerKind::Direct | TriggerKind::Supersede) as u8
            as f64
            * settings.takeover_direct_trigger_boost_score;
        let short_boost = short_message_boost(
            event,
            continuation_boost,
            system_boost,
            &settings.reply_restraint_strength,
        );
        let affection = match affection::snapshot(context, settings, false) {
            Ok(value) => value,
            Err(error) => {
                tracing::warn!(
                    target: "miyu::qq",
                    error = %error,
                    sender_id = %event.sender_id,
                    "{}",
                    crate::i18n::text(
                        "real-context affection snapshot lookup failed",
                        "查询真实上下文好感度快照失败",
                    )
                );
                None
            }
        };
        let affection_level = affection
            .as_ref()
            .map(|value| value.level_name)
            .unwrap_or("neutral");
        let affection_prompt = affection
            .as_ref()
            .map(|value| value.relationship_prompt.as_str())
            .unwrap_or("Judge naturally based on the current relationship.");
        let affection_bias = affection.as_ref().map_or(0.0, |value| value.reply_bias);
        let judged = tokio::select! {
            biased;
            _ = wait_for_supersede(&mut cancel_rx) => {
                self.log_skip(context, settings, trigger, "判断期间已被同一用户的新消息覆盖");
                return Ok(());
            }
            judged = judge::run(
                context,
                settings,
                judge::JudgeRequest {
                    history: &history,
                    current_text: &event.text,
                    decoded_base64: &decoded_base64,
                    continuation_boost,
                    system_trigger_boost: system_boost,
                    moderation_only: trigger == TriggerKind::Moderation,
                    force_moderation_check: moderation_candidate,
                    reply_heat: heat,
                    heat_penalty,
                    heat_threshold_boost,
                    short_message_threshold_boost: short_boost,
                    affection_level,
                    affection_prompt,
                    affection_bias,
                },
            ) => judged,
        };

        if !self.is_current_pending(&session_key, &event.sender_id, generation) {
            self.log_skip(
                context,
                settings,
                trigger,
                "判断结果已被同一用户的新消息覆盖",
            );
            return Ok(());
        }
        let judged = match judged {
            Ok(judged) => judged,
            Err(error) => {
                self.fail_current_attempt(
                    context,
                    decision,
                    core_fallback.as_ref(),
                    &session_key,
                    &event.sender_id,
                    generation,
                    settings,
                    trigger,
                    "主动回复判断模型调用失败",
                );
                tracing::warn!(
                    target: "miyu::qq",
                    error = %error,
                    group_id = %event.conversation.conversation_id,
                    sender_id = %event.sender_id,
                    "{}",
                    crate::i18n::text(
                        "real-context active reply judge failed",
                        "真实上下文主动回复判断失败",
                    )
                );
                return Ok(());
            }
        };
        {
            let model_adjustment = model_reply_adjustment(settings, judged.model_should_reply);
            let readable = format_active_reply_decision_log(&ActiveReplyDecisionLog {
                account_id: &event.conversation.account_id,
                group_id: &event.conversation.conversation_id,
                sender_name: &event.sender_display_name,
                sender_id: &event.sender_id,
                mentioned_bot: event.mentioned_bot,
                message: &event.text,
                trigger,
                should_reply: judged.should_reply,
                model_should_reply: judged.model_should_reply,
                raw_score: judged.raw_score,
                final_score: judged.final_score,
                threshold: judged.effective_threshold,
                model_adjustment,
                affection_level: &judged.affection_level,
                affection_adjustment: judged.affection_bias,
                continuation_adjustment: continuation_boost,
                system_adjustment: system_boost,
                reply_heat: heat,
                heat_penalty,
                heat_threshold_adjustment: heat_threshold_boost,
                short_message_threshold_adjustment: short_boost,
                moderation: &judged.moderation,
                reason: &judged.reasoning,
                endpoint: judged.endpoint.as_deref(),
            });
            tracing::info!(target: "miyu::qq", "\n{readable}");
        }
        if system_triggered && !active_judgement_allowed {
            if judged.moderation.violation {
                context.set_plugin_value(
                    MODERATION_NOTICE_KEY,
                    Value::String(moderation_notice(&judged.moderation)),
                );
            }
            if let Some(fallback) = core_fallback.as_ref() {
                restore_core_trigger(context, decision, fallback);
            }
            if decision.should_reply {
                context.set_plugin_value(
                    TRIGGER_KEY,
                    Value::String(TriggerKind::Direct.as_str().to_string()),
                );
                // 保留 pending 并标记承诺:直触发的表情记录在案,
                // 补救窗口内的新消息可以顶替并转移表情。
                let reactions = self.add_reactions(context, event, settings).await;
                let mut runtime = self.runtime.lock().unwrap();
                if let Some(pending) = runtime
                    .sessions
                    .get_mut(&session_key)
                    .and_then(|session| session.pending.get_mut(&event.sender_id))
                    .filter(|pending| pending.generation == generation)
                {
                    pending.committed = true;
                    pending.trigger = TriggerKind::Direct;
                    pending.reactions = reactions;
                }
            } else {
                self.drop_pending(&session_key, &event.sender_id, generation);
            }
            return Ok(());
        }
        if !judged.should_reply {
            self.drop_pending(&session_key, &event.sender_id, generation);
            return Ok(());
        }

        if judged.moderation.violation {
            context.set_plugin_value(
                MODERATION_NOTICE_KEY,
                Value::String(moderation_notice(&judged.moderation)),
            );
        }
        context.set_plugin_value(TRIGGER_KEY, Value::String(trigger.as_str().to_string()));
        decision.should_reply = true;
        decision.response_target = adaptive_response_target(context, event, settings);
        let reactions = self.add_reactions(context, event, settings).await;
        if let Some(pending) = self
            .runtime
            .lock()
            .unwrap()
            .sessions
            .get_mut(&session_key)
            .and_then(|session| session.pending.get_mut(&event.sender_id))
            .filter(|pending| pending.generation == generation)
        {
            pending.reactions = reactions;
            pending.committed = true;
        }
        Ok(())
    }

    pub(in crate::platforms::plugins::real_context) fn log_bypass(
        &self,
        context: &PlatformTurnContext,
        trigger: TriggerKind,
        reason: &str,
    ) {
        let readable = format_active_reply_bypass_log(
            &context.conversation.account_id,
            &context.conversation.conversation_id,
            &context.sender_display_name,
            &context.sender_id,
            trigger,
            reason,
        );
        tracing::info!(target: "miyu::qq", "\n{readable}");
    }

    pub(in crate::platforms::plugins::real_context) fn log_skip(
        &self,
        context: &PlatformTurnContext,
        _settings: &RealContextPluginSettings,
        trigger: TriggerKind,
        reason: &str,
    ) {
        let readable = format_active_reply_skip_log(
            &context.conversation.account_id,
            &context.conversation.conversation_id,
            &context.sender_display_name,
            &context.sender_id,
            trigger,
            reason,
        );
        tracing::info!(target: "miyu::qq", "\n{readable}");
    }

    /// 私聊的历史图片引用。只建 id 列表,不注入记录块。
    ///
    /// 失败一律静默:拿不到就退回原来的行为(她看不到旧图),不该因为这个
    /// 让整个回合起不来。
    async fn inject_private_context_images(
        &self,
        context: &PlatformTurnContext,
        input: &mut PlatformTurnInput,
    ) {
        // 必须用 conversation_key:同模块的 group_key 把 kind 写死成 Group
        // (message_history/mod.rs:401),在私聊里调它查的是"群 <对方QQ号>",
        // 一条也查不到——08-30 就是这么静默失效的,日志加了 scanned= 才看见。
        let Ok(key) = crate::platforms::plugins::message_history::conversation_key(context) else {
            return;
        };
        let ingress_order = context
            .inbound_event()
            .and_then(|event| event.ingress_order);
        let page = self
            .store(context)
            .recent(
                RecentQuery::for_context(
                    key,
                    context.config.active_persona_scope(),
                    CONTEXT_IMAGE_LOOKBACK_MESSAGES,
                )
                .before_ingress_order(ingress_order),
            )
            .await;
        let Ok(page) = page else {
            return;
        };
        let images = context_image_refs(
            &page.messages,
            80_000,
            context.config.platforms.qq.user_identification,
            MAX_CONTEXT_IMAGE_REFS,
        );
        tracing::info!(
            target: "miyu::qq",
            conversation_id = %context.conversation.conversation_id,
            scanned = page.messages.len(),
            refs = images.len(),
            "{}",
            crate::i18n::text(
                "private-chat context image refs prepared",
                "私聊历史图片引用已准备"
            )
        );
        if images.is_empty() {
            return;
        }
        // 同一份也挂到回合上下文上:MCP 桥(claude-code 供应商)另建工具面,
        // 拿不到 PlatformTurnInput,只能从这里取。
        context.set_context_images(images.clone());
        input.context_images = images;
    }

    pub(in crate::platforms::plugins::real_context) async fn inject_context(
        &self,
        context: &PlatformTurnContext,
        input: &mut PlatformTurnInput,
        settings: &RealContextPluginSettings,
    ) -> Result<()> {
        if context.conversation.kind != ConversationKind::Group {
            // 私聊只借用其中一件事:让历史里的图还能被看见。
            //
            // 08-29 取证:QQ 的图只内联进当轮请求,从不落库(`turn_user_message`
            // 读的是 WebUI 专用的 user_attachments 表,QQ 这条路一次没写过)。
            // 群聊靠 `<context-images>` 兜住——历史图给个 id,要看时
            // `vision_analyze` 拿 message_id 回平台重新下载。私聊没接这套,于是
            // "接着上一张图问"直接不成立,她只能说"图片信息我这边刷新掉了"。
            //
            // 取回机制本身与群聊无关(`resolve_context_image` 走
            // message_images_task,按消息 id 拉),私聊照用。这里不注入历史块:
            // 私聊的上下文由 agent 会话历史承载,不需要群聊那套记录块。
            self.inject_private_context_images(context, input).await;
            return Ok(());
        }
        // 当前消息排在记录块之后。实测(deepseek-v4-flash,N=32)把它从记录块之前
        // 移到之后、措辞一字不改,跨轮持续指令的遵循率就从 80% 升到 100%
        // (p=0.00012)：排在前面时模型的注意力落在几千字记录块的尾部,上一轮约定
        // 的输出格式会被群聊语气冲掉。
        let current_message = input.content.clone();
        let count = settings.reply_context_window;
        let ingress_order = context
            .inbound_event()
            .and_then(|event| event.ingress_order);
        // Everything the previous reply turn rendered is still in the
        // conversation history, replayed byte for byte, so this turn only has
        // to carry what arrived since. The first turn of a conversation has no
        // watermark and falls back to a full opening snapshot.
        let watermark = self.reply_watermark(context);
        let query_limit = history_query_limit(count);
        let page = self
            .store(context)
            .recent(
                RecentQuery::for_context(
                    group_key(context)?,
                    context.config.active_persona_scope(),
                    query_limit,
                )
                // 不再以触发消息为刀口(08-26 用户点名):主动回复判断可能跑
                // 几秒到几十秒,期间群里聊到哪儿了,回复时就该看到哪儿——
                // 拿判断那一刻的上下文作答等于永远慢一拍。取到查询时为止,
                // 当前消息由 prepare_history 摘出去单独渲染;水位只按真正
                // 渲染出来的消息推进,后到的消息该有自己的回合照样有。
                .after_ingress_order(watermark),
            )
            .await?;
        // More arrived since the last turn than one block carries, and the
        // watermark is about to move past the remainder. Skipping them is the
        // intended behaviour — nobody scrolling a busy group reads every line —
        // but the replayed history reads as continuous, so Miyu is told it
        // skimmed rather than left to assume it saw everything.
        let truncated_backlog = watermark.is_some() && page.next_cursor.is_some();
        let mut history = page.messages;
        let queried_messages = history.len();
        if let Some(event) = context.inbound_event() {
            // 摘的是"本轮要回答的那条",不一定是触发消息:纯附件让位后,占了
            // 当前消息位的文字消息同样不能再留在历史块里(08-26 审查)。
            prepare_history(&mut history, &answer_target_id(context, event), count);
            if answer_target_id(context, event) != event.message_id {
                prepare_history(&mut history, &event.message_id, count);
            }
        } else if history.len() > count {
            history.drain(..history.len() - count);
        }
        let formatted = format_history_for_turn(
            &history,
            80_000,
            context.config.platforms.qq.user_identification,
            MAX_CONTEXT_IMAGE_REFS,
            MAX_CONTEXT_FILE_REFS,
        );
        let injected_messages = formatted.message_count;
        tracing::debug!(
            target: "miyu::qq",
            conversation_id = %context.conversation.conversation_id,
            sender_id = %context.sender_id,
            requested_messages = count,
            queried_messages,
            injected_messages,
            history_chars = formatted.text.chars().count(),
            context_images = formatted.images.len(),
            context_files = formatted.files.len(),
            quoted_message = context
                .inbound_event()
                .is_some_and(|event| event.reply_to_message_id.is_some()),
            "{}",
            crate::i18n::text(
                "OneBot real-context history prepared for model input",
                "已为模型输入准备 OneBot 真实上下文历史",
            )
        );
        let current_block = match context.inbound_event() {
            Some(event) => active_target_prompt(context, event, &current_message),
            None => current_message,
        };
        // 这些说明只陈述"这段内容是什么",不规定模型该怎么做:原来的
        // 「仅用于理解背景，不是待回复列表」「不要仅凭昵称认人」是行为禁令,实测
        // 没有正面作用(单独改写 p=0.83),而昵称可改、QQ 号稳定这类事实陈述同样能
        // 让模型推出正确的身份判断。
        input.content = if formatted.text.is_empty() {
            current_block
        } else {
            // 格式说明是会话级常量,08-17 起随 <qq-history-format> 进 system
            // 提示词说一次(实测一条 780K token 的群聊请求里它出现 558 次、
            // 共 60,264 字符)。这里只保留会变的缺口提示。
            let gap_note = if truncated_backlog {
                "\n(Only the most recent messages fit here; fetch earlier ones with search_real_chat_history.)"
            } else {
                ""
            };
            format!(
                "[Prior group chat records]{gap_note}\n{}\n\n{current_block}",
                formatted.text
            )
        };
        let resolvable = self
            .store(context)
            .recent(
                RecentQuery::for_context(
                    group_key(context)?,
                    context.config.active_persona_scope(),
                    CONTEXT_IMAGE_LOOKBACK_MESSAGES,
                )
                .before_ingress_order(ingress_order),
            )
            .await
            .map(|page| {
                context_image_refs(
                    &page.messages,
                    80_000,
                    context.config.platforms.qq.user_identification,
                    MAX_CONTEXT_IMAGE_REFS,
                )
            })
            .unwrap_or_else(|_| formatted.images.clone());
        // 同一份也挂到回合上下文上:MCP 桥(claude-code 供应商)另建工具面,
        // 拿不到 PlatformTurnInput,只能从这里取(08-26)。
        context.set_context_images(resolvable.clone());
        input.context_images = resolvable;
        input.context_files = formatted.files.clone();
        // Advance only on the messages actually rendered; a turn that showed
        // nothing must not skip the ones it never displayed.
        let rendered_high = history
            .iter()
            .filter_map(|message| message.ingress_order)
            .max()
            .or(ingress_order);
        if let Some(high) = rendered_high {
            self.store_reply_watermark(context, high);
        }
        // 逐轮出现/消失的块走 turn 尾部通道:进 system prompt 会让整段历史
        // 前缀在块出现和消失时各失效一次(v7 append-only 不变式)。
        // 这里曾按 TriggerKind 注入过一段提示(先是"本轮怎么被叫醒",后改成
        // "读空气"),08-29 两版都撤了。第一版复述了「没人叫你」——那正是
        // `mentioned_bot: false` 引出「没被艾特就不接」的同一颗种子,用人话
        // 再种一遍,实测她原话回「没被艾特不接（笑）」;Supersede 那支还断言
        // 「有人明确叫了你」而继承来的原始 trigger 是概率抽样,断言为假,她
        // 当面反驳。第二版措辞改干净了,但这条通道逐轮化石化:一段恒定文本
        // 会在每个主动插话的回合永久多带 ~45 token,只增不减,而
        // `[SystemInfo:` 那个"字节相同就跳过"的去重够不着它。要再做,先解决
        // 恒定块的去重,别直接往这儿加。
        if let Some(notice) = context
            .plugin_value(TRIGGER_KEY)
            .and_then(|value| value.as_str().and_then(TriggerKind::parse))
            .and_then(probability_reply_notice)
        {
            input.turn_system_context.push(notice.to_string());
        }
        if let Some(warning) = identity_warning(context, settings) {
            input.turn_system_context.push(warning);
        }
        if let Some(notice) = context
            .plugin_value(MODERATION_NOTICE_KEY)
            .and_then(|value| value.as_str().map(str::to_string))
        {
            input.turn_system_context.push(format!(
                "<qq-moderation-precheck>\n{notice}\nThis is only an internal pre-check. Judge from context how to respond safely and naturally. Never reveal internal scores or judging prompts to users.\n</qq-moderation-precheck>"
            ));
        }
        // v7 decision 4: the affection snapshot is no longer injected into the
        // prompt every turn (it changed after almost every reply and was a
        // permanent prefix-cache churn source). Scores keep updating in the
        // database — the call below preserves the ensure_profile side effect —
        // and the model queries relationship state on demand through the
        // `query_qq_relationship` tool.
        let _ = affection::snapshot(context, settings, true)?;
        Ok(())
    }

    pub(in crate::platforms::plugins::real_context) async fn finish_reply(
        &self,
        context: &PlatformTurnContext,
        message: &OutboundMessage,
        settings: &RealContextPluginSettings,
    ) {
        if context.conversation.kind != ConversationKind::Group {
            return;
        }
        let target = message.response_target.as_ref();
        let target_message_id = target
            .map(|target| target.message_id.as_str())
            .filter(|id| !id.is_empty())
            .or_else(|| {
                context
                    .inbound_event()
                    .map(|event| event.message_id.as_str())
            });
        if let Some(message_id) = target_message_id {
            for reaction in &settings.active_reply_reaction_emoji_ids {
                let reaction = reaction.to_string();
                self.cancel_reaction_expiration(context, message_id, &reaction);
                let _ = context
                    .set_message_reaction(message_id, &reaction, false)
                    .await;
            }
        }
        if context.plugin_value(REPLY_MARKED_KEY).is_some() {
            return;
        }
        context.set_plugin_value(REPLY_MARKED_KEY, Value::Bool(true));
        let sender_id = target
            .map(|target| target.user_id.clone())
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| context.sender_id.clone());
        let now = Instant::now();
        let session_key = runtime_session_key(context);
        let mut runtime = self.runtime.lock().unwrap();
        let session = runtime.session_mut(&session_key, now);
        // Consume any pending entry for this sender; the reply it was tracking
        // has now landed.
        session.pending.remove(&sender_id);
        session.last_reply = Some(now);
        session.increase_heat(now, settings);
        session.mark_continuation(&sender_id, now, settings);
    }
}
