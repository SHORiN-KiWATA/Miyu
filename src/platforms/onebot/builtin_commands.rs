//! 平台侧的内建命令（`/models`、停止、取消等）。
//!
//! 这些命令**不进模型**，直接在平台层执行并回复。所以它们的输出仍要走一遍回复
//! 处理插件——对用户来说，命令的回复和普通回复应该长得一样。

use crate::platforms::onebot::*;

pub(in crate::platforms::onebot) fn message_info_matches_target(
    info: &PlatformMessageInfo,
    target: Target,
) -> bool {
    let expected_kind = match target {
        Target::Private { .. } => ConversationKind::Private,
        Target::Group { .. } => ConversationKind::Group,
    };
    info.conversation_kind == Some(expected_kind)
        && info.conversation_id.as_deref() == Some(target.conversation_id().to_string().as_str())
}

pub(in crate::platforms::onebot) async fn execute_builtin_command(
    state: &DaemonState,
    context: &PlatformTurnContext,
    target: Target,
    event: &Value,
    command: commands::ParsedPlatformCommand,
) -> Option<OutboundMessage> {
    let response = match command {
        commands::ParsedPlatformCommand::Reset { scope } => {
            let descriptor = commands::descriptor(commands::RESET_COMMAND_ID)
                .expect("the reset command descriptor is registered");
            if !commands::is_allowed(&context.config.platforms, descriptor, context.is_admin) {
                return None;
            } else if scope.is_none() {
                commands::reset_usage_message(&context.config.platforms)
            } else {
                match resolve_onebot_session(state, context, target, event) {
                    Err(error) => {
                        tracing::warn!(target: "miyu::qq", error = %error, "{}", t("resolving the QQ session for reset failed", "解析待重置的 QQ 会话失败"));
                        t(
                            "The conversation could not be reset. Check the daemon logs for details.",
                            "无法重置当前会话，请查看 daemon 日志。",
                        )
                        .to_string()
                    }
                    Ok(session_id) => {
                        let ticket = state.platforms.preempt_session_turns(
                            &session_id,
                            context.config.platforms.qq.session_limits(
                                match context.conversation.kind {
                                    ConversationKind::Private => PlatformConversationKind::Private,
                                    ConversationKind::Group => PlatformConversationKind::Group,
                                },
                                &context.conversation.conversation_id,
                            ),
                        );
                        cancel_session_runs(state, &session_id);
                        let _session_turn = ticket.acquire().await.ok();
                        match clear_platform_session_content(state, session_id.clone()).await {
                            Ok(()) => match context.after_session_reset().await {
                                Ok(()) => {
                                tracing::info!(
                                    target: "miyu::qq",
                                    session_id = %session_id,
                                    sender_id = %context.sender_id,
                                    "{}",
                                    t("QQ conversation reset", "QQ 会话已重置")
                                );
                                t(
                                    "The current conversation has been reset.",
                                    "当前会话已重置。",
                                )
                                .to_string()
                                }
                                Err(error) => {
                                    tracing::warn!(
                                        target: "miyu::qq",
                                        session_id = %session_id,
                                        error = %error,
                                        "{}",
                                        t("QQ conversation reset but plugin state update failed", "QQ 会话已重置，但插件状态更新失败")
                                    );
                                    t(
                                        "The conversation was cleared, but its platform history boundary could not be updated. Run /reset again.",
                                        "会话内容已清空，但通讯平台历史边界更新失败，请再次执行 /reset。",
                                    )
                                    .to_string()
                                }
                            },
                            Err(PlatformSessionResetError::Busy) => t(
                                "This conversation is replying right now. Try resetting it again after the reply finishes.",
                                "当前会话正在回复，请在回复结束后重试。",
                            )
                            .to_string(),
                            Err(PlatformSessionResetError::Unavailable) => t(
                                "The Miyu core is unavailable, so the conversation was not reset.",
                                "Miyu 核心当前不可用，会话未重置。",
                            )
                            .to_string(),
                            Err(PlatformSessionResetError::Internal(error)) => {
                                tracing::warn!(target: "miyu::qq", session_id = %session_id, error = %error, "{}", t("resetting the QQ conversation failed", "重置 QQ 会话失败"));
                                t(
                                    "The conversation could not be reset. Check the daemon logs for details.",
                                    "无法重置当前会话，请查看 daemon 日志。",
                                )
                                .to_string()
                            }
                        }
                    }
                }
            }
        }
        commands::ParsedPlatformCommand::Wipe { confirmed } => {
            let descriptor = commands::descriptor(commands::WIPE_COMMAND_ID)
                .expect("the wipe command descriptor is registered");
            if !commands::is_allowed(&context.config.platforms, descriptor, context.is_admin) {
                return None;
            }
            if !confirmed {
                commands::wipe_confirm_message(&context.config.platforms)
            } else {
                match reset_platform_persona_state(state, &context.config).await {
                    Ok(_) => t(
                        "Memory, every conversation's contents and group-chat contexts for the current persona have been erased. Skills and scripts were left alone.",
                        "当前人格的记忆、全部会话内容、群聊上下文已抹掉；技能和脚本文件保留。",
                    )
                    .to_string(),
                    Err(PlatformPersonaResetError::Busy) => t(
                        "Miyu is busy. Try again shortly.",
                        "Miyu 正忙，请稍后重试。",
                    )
                    .to_string(),
                    Err(PlatformPersonaResetError::Unavailable) => t(
                        "The wipe service is temporarily unavailable.",
                        "抹除服务暂时不可用。",
                    )
                    .to_string(),
                    Err(PlatformPersonaResetError::Internal(error)) => {
                        tracing::warn!(target: "miyu::qq", %error, "{}", t("wiping the QQ persona state failed", "抹除 QQ 人格状态失败"));
                        t(
                            "The wipe could not be completed. Check the daemon logs for details.",
                            "抹除未能完成，请查看 daemon 日志。",
                        )
                        .to_string()
                    }
                }
            }
        }
        commands::ParsedPlatformCommand::ResetMemory => {
            let descriptor = commands::descriptor(commands::RESET_MEMORY_COMMAND_ID)
                .expect("the reset-memory command descriptor is registered");
            if !commands::is_allowed(&context.config.platforms, descriptor, context.is_admin) {
                return None;
            }
            // 群/私聊里的"本会话"就是这条对话对应的那个会话 id。
            match resolve_onebot_session(state, context, target, event) {
                Err(error) => {
                    tracing::warn!(target: "miyu::qq", error = %error, "{}", t("resolving the QQ session for the memory reset failed", "解析待清记忆的 QQ 会话失败"));
                    t(
                        "The memory reset could not be completed. Check the daemon logs for details.",
                        "记忆清空未能完成，请查看 daemon 日志。",
                    )
                    .to_string()
                }
                Ok(session_id) => {
                    // context.config 已按平台人格覆盖作用域化:清的就是这个
                    // 会话所属人格的记忆命名空间;会话历史与技能不动。
                    match crate::memory::MemoryStore::new(&context.config, &state.paths)
                        .reset_session(&session_id)
                    {
                        Ok(summary) => summary.describe(),
                        Err(error) => {
                            tracing::warn!(target: "miyu::qq", %error, "{}", t("resetting platform session memory failed", "平台会话记忆清空失败"));
                            t(
                                "The memory reset could not be completed. Check the daemon logs for details.",
                                "记忆清空未能完成，请查看 daemon 日志。",
                            )
                            .to_string()
                        }
                    }
                }
            }
        }
        commands::ParsedPlatformCommand::ResetAllMemory => {
            let descriptor = commands::descriptor(commands::RESET_ALL_MEMORY_COMMAND_ID)
                .expect("the reset-all-memory command descriptor is registered");
            if !commands::is_allowed(&context.config.platforms, descriptor, context.is_admin) {
                return None;
            }
            match crate::memory::MemoryStore::new(&context.config, &state.paths).reset_all(false) {
                Ok(()) => t("All long-term memory erased.", "全部长期记忆已清空。").to_string(),
                Err(error) => {
                    tracing::warn!(target: "miyu::qq", %error, "{}", t("resetting platform memory failed", "平台记忆清空失败"));
                    t(
                        "The memory reset could not be completed. Check the daemon logs for details.",
                        "记忆清空未能完成，请查看 daemon 日志。",
                    )
                    .to_string()
                }
            }
        }
        commands::ParsedPlatformCommand::Stop { has_arguments } => {
            let descriptor = commands::descriptor(commands::STOP_COMMAND_ID)
                .expect("the stop command descriptor is registered");
            if !commands::is_allowed(&context.config.platforms, descriptor, context.is_admin) {
                // 与 /reset 对齐:非管理员的管理命令静默忽略(08-25 用户裁定)。
                // 回拒绝语等于向群友广播命令的存在,还给试探者反馈;日志留痕
                // 保住管理员排查"为什么我的命令没反应"的通路。
                tracing::info!(
                    target: "miyu::qq",
                    sender_id = %context.sender_id,
                    "{}",
                    t(
                        "ignored /stop from a non-admin",
                        "已静默忽略非管理员的 /stop",
                    )
                );
                return None;
            } else if has_arguments {
                commands::stop_usage_message(&context.config.platforms)
            } else {
                match resolve_onebot_session(state, context, target, event) {
                    Err(error) => {
                        tracing::warn!(target: "miyu::qq", error = %error, "{}", t("resolving the QQ session for stop failed", "解析待停止的 QQ 会话失败"));
                        t(
                            "The current conversation could not be stopped. Check the daemon logs for details.",
                            "无法停止当前会话，请查看 daemon 日志。",
                        )
                        .to_string()
                    }
                    Ok(session_id) => {
                        let queued = state
                            .platforms
                            .queued_turns(&context.conversation.scope_key());
                        let ticket = state.platforms.preempt_session_turns(
                            &session_id,
                            context.config.platforms.qq.session_limits(
                                match context.conversation.kind {
                                    ConversationKind::Private => PlatformConversationKind::Private,
                                    ConversationKind::Group => PlatformConversationKind::Group,
                                },
                                &context.conversation.conversation_id,
                            ),
                        );
                        let cancelled = cancel_session_runs(state, &session_id);
                        // 等被取消的回合真正退出再报数,但设上限:回合卡在
                        // LLM 超时重试/慢工具里退不出来时,/stop 的回复被
                        // 这里拖住,在用户看来就像命令排队了(08-20 实测
                        // 正常场景 0.5s,风暴场景数十秒)。超时先回话,
                        // 停止信号早已发出。
                        let _session_turn = tokio::time::timeout(
                            std::time::Duration::from_secs(8),
                            ticket.acquire(),
                        )
                        .await
                        .ok()
                        .and_then(Result::ok);
                        tracing::info!(
                            target: "miyu::qq",
                            session_id = %session_id,
                            sender_id = %context.sender_id,
                            cancelled,
                            queued,
                            "{}",
                            t("QQ conversation stopped", "QQ 会话已停止")
                        );
                        stop_response_message(cancelled, queued)
                    }
                }
            }
        }
        commands::ParsedPlatformCommand::Models { argument } => {
            let descriptor = commands::descriptor(commands::MODELS_COMMAND_ID)
                .expect("the models command descriptor is registered");
            if !commands::is_allowed(&context.config.platforms, descriptor, context.is_admin) {
                // Deliberately silent for non-admins, like /reset: no reply
                // and no log line.
                return None;
            }
            execute_models_command(state, target, argument.as_deref())
        }
    };
    Some(OutboundMessage::text(OutboundOrigin::Command, response))
}

/// `/models` lists the globally configured models; `/models <index|provider/model>`
/// switches this conversation's text model by writing a single-model pool into
/// its per-conversation route (私聊/群聊专属配置), creating the route if needed.
pub(in crate::platforms::onebot) fn execute_models_command(
    state: &DaemonState,
    target: Target,
    argument: Option<&str>,
) -> String {
    let kind = match target {
        Target::Private { .. } => PlatformConversationKind::Private,
        Target::Group { .. } => PlatformConversationKind::Group,
    };
    let conversation_id = target.conversation_id().to_string();
    let mut manager = state.manager.lock().unwrap();
    let choices = manager.config.text_provider_model_choices();
    if choices.is_empty() {
        return t("No models are configured.", "尚未配置任何模型。").to_string();
    }
    let Some(argument) = argument else {
        let effective = manager
            .config
            .qq_text_model_pool(kind, &conversation_id, false)
            .unwrap_or_default();
        // Plain numbered lines read best in QQ: no alignment padding (IM
        // fonts are proportional) and no empty checkbox noise — only the
        // effective models carry a marker.
        let mut lines = vec![t("Available models:", "可用模型：").to_string()];
        for (index, choice) in choices.iter().enumerate() {
            let active = effective.iter().any(|active| {
                active.provider_id == choice.provider_id && active.model == choice.model
            });
            let marker = if active {
                t(" ✅current", " ✅当前")
            } else {
                ""
            };
            lines.push(format!("{}. {}{marker}", index + 1, choice.label()));
        }
        lines.push(format!(
            "{}{}",
            t("Switch with: ", "切换模型："),
            commands::models_switch_hint(&manager.config.platforms)
        ));
        return lines.join("\n");
    };
    let selected = match crate::config::resolve_provider_model_argument(&choices, argument) {
        Ok(choice) => choice.clone(),
        Err(message) => return message,
    };
    if manager.admin_busy {
        return t(
            "Miyu is busy with another admin operation. Try again shortly.",
            "Miyu 正忙于其他管理操作，请稍后再试。",
        )
        .to_string();
    }
    let mut next_config = manager.config.clone();
    let mut route = next_config
        .platforms
        .model_route(kind, &conversation_id)
        .cloned()
        .unwrap_or_else(|| crate::config::PlatformModelRoute {
            conversation: crate::config::PlatformConversationConfig {
                kind,
                id: conversation_id.clone(),
            },
            persona: crate::config::PlatformPersonaOverride::default(),
            text_models_inheritance: crate::config::PlatformModelPoolInheritance::default(),
            text_models: None,
            multimodal_models_inheritance: crate::config::PlatformModelPoolInheritance::default(),
            multimodal_models: None,
            extra_prompt: String::new(),
            session_limits: None,
            probability_reply: None,
        });
    route.text_models = Some(vec![crate::config::ActiveProviderModelConfig {
        provider_id: selected.provider_id.clone(),
        model: selected.model.clone(),
    }]);
    next_config.platforms.upsert_model_route(route);
    if let Err(error) = next_config.save(&state.paths) {
        tracing::warn!(
            target: "miyu::qq",
            error = %error,
            "{}",
            t(
                "saving the conversation model override failed",
                "保存会话专属模型配置失败"
            )
        );
        return t(
            "The model could not be saved. Check the daemon logs for details.",
            "模型切换保存失败，请查看 daemon 日志。",
        )
        .to_string();
    }
    manager.config = next_config;
    format!(
        "{}{}",
        t(
            "This conversation now uses (saved to its dedicated settings): ",
            "本会话已切换模型（已写入私聊/群聊专属配置）："
        ),
        selected.label()
    )
}

pub(in crate::platforms::onebot) fn stop_response_message(
    cancelled: usize,
    queued: usize,
) -> String {
    if crate::i18n::is_zh() {
        match (cancelled, queued) {
            (0, 0) => "当前会话没有正在运行的任务。".to_string(),
            (_, 0) => format!("已打断 {cancelled} 个运行中的任务。"),
            (0, _) => format!("已丢弃 {queued} 个排队中的任务。"),
            _ => format!("已打断 {cancelled} 个运行中的任务、{queued} 个排队中的任务。"),
        }
    } else {
        match (cancelled, queued) {
            (0, 0) => "No running tasks to stop in the current conversation.".to_string(),
            (_, 0) => format!("Interrupted {cancelled} running task(s)."),
            (0, _) => format!("Discarded {queued} queued task(s)."),
            _ => format!(
                "Interrupted {cancelled} running task(s) and discarded {queued} queued task(s)."
            ),
        }
    }
}

pub(in crate::platforms::onebot) fn cancel_session_runs(
    state: &DaemonState,
    session_id: &str,
) -> usize {
    let manager = state.manager.lock().unwrap();
    let mut cancelled = 0;
    for run in manager
        .active_runs
        .values()
        .filter(|run| &*run.session_id == session_id)
    {
        run.request_cancel();
        cancelled += 1;
    }
    cancelled
}
