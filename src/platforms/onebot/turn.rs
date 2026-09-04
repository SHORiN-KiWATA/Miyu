//! 从一条平台消息到一个回合。
//!
//! `build_and_run_turn` 是入站的终点、agent 的起点：拼上下文、建会话、跑回合、
//! 把结果交给投递。
//!
//! 工具追加（`enqueue_tool_followup`）是这里最微妙的一块：工具产出的内容要作为
//! 独立消息补发，但不能和主回复抢顺序，也不能在回合被取消后还发出去，所以先
//! `reserve_tool_followup` 占位再入队。

use crate::platforms::onebot::*;

/// 合成唤醒事件的 user_id:优先 spawn 回合记录的真实发起者;私聊退回会话
/// 对端(该私聊唯一的人类);群聊无记录时保持机器人自身——不凭空授予权限,
/// 只是回到修复前的降级行为。
pub(in crate::platforms::onebot) fn wake_sender_user_id(
    initiator: Option<&str>,
    target: Target,
    self_id: i64,
) -> i64 {
    initiator
        .and_then(|id| id.trim().parse().ok())
        .or(match target {
            Target::Private { user_id } => Some(user_id),
            Target::Group { .. } => None,
        })
        .unwrap_or(self_id)
}

/// Background-job completion wake: a self-initiated model turn in a bound
/// QQ conversation. There is no inbound event — reply targeting, affection
/// and trigger judging all no-op — the sender display name stays "System",
/// so the model reads the job result and reports it into the conversation
/// in its own voice.
pub(crate) async fn wake_conversation_for_job(
    state: &DaemonState,
    account_id: &str,
    conversation_kind: &str,
    conversation_id: &str,
    initiator: Option<&str>,
    content: String,
) -> Result<()> {
    let self_id: i64 = account_id
        .parse()
        .context("invalid QQ account id for a job wake")?;
    let conn = state
        .platforms
        .onebot
        .lock()
        .unwrap()
        .handle(self_id)
        .context("the QQ account is not connected")?;
    let target_id: i64 = conversation_id
        .parse()
        .context("invalid QQ conversation id for a job wake")?;
    let target = match conversation_kind {
        "group" => Target::Group {
            group_id: target_id,
        },
        "private" => Target::Private { user_id: target_id },
        other => bail!("unsupported QQ conversation kind: {other}"),
    };
    let config = state.manager.lock().unwrap().config.clone();
    // issue #29:合成事件的 user_id 决定 is_admin → host_tools_allowed →
    // 工具表选择。必须继承真实发起者的身份,伪装成机器人自己会把跟进 turn
    // 降级成受限工具集,job_status 都不存在。
    let sender_user_id = wake_sender_user_id(initiator, target, self_id);
    let event = json!({
        "self_id": self_id,
        "user_id": sender_user_id,
        "sender": { "nickname": "System" },
    });
    let context = Arc::new(platform_turn_context(
        state, conn, target, &event, config, None,
    )?);
    let session_id = resolve_onebot_session(state, &context, target, &event)?;
    let conversation_kind_enum = match target {
        Target::Private { .. } => PlatformConversationKind::Private,
        Target::Group { .. } => PlatformConversationKind::Group,
    };
    // Run the normal turn preparation so plugins inject group history and
    // context blocks — the wake turn should see the conversation exactly
    // like an inbound turn would.
    let prepared = context.prepare_turn(content).await;
    let mut turn_system_context = vec![
        "This turn was triggered automatically by the system: a background job just finished, \
         and its report and results are in this turn's message. This is not a message from any \
         group member or user; deliver the results into the conversation naturally, in your own voice."
            .to_string(),
    ];
    turn_system_context.extend(prepared.turn_system_context);
    let profile = crate::platforms::TurnProfile {
        active_persona: Some(context.config.prompt.active_persona.clone()),
        text_models: context.config.active_provider_models.clone(),
        multimodal_models: context
            .config
            .qq_multimodal_model_pool(
                conversation_kind_enum,
                &context.conversation.conversation_id,
            )
            .map(<[_]>::to_vec),
        system_context: prepared.system_context,
        turn_system_context,
        memory_content: Some(prepared.memory_content),
        context_images: prepared.context_images,
        context_files: prepared.context_files.into_boxed_slice(),
        image_cache_namespace: Some("qq".to_string()),
        image_source_label: Some("QQ".to_string()),
        memory_write_enabled: context.config.platforms.qq.memory.write_enabled,
        // Groups keep their own turn history now. The structured log still
        // carries who said what — the protocol offers no third role and drops
        // `name`, so identity can only live in the text — but the log is
        // additive: each turn appends what arrived since the last one, and
        // earlier turns replay verbatim. Miyu's own turns become real
        // assistant messages instead of one `[你]` line in a rolling window.
        suppress_session_history: false,
        group_context: (context.conversation.kind == ConversationKind::Group)
            .then(|| context.config.platforms.qq.group_context.clone()),
        platform: Some(context.clone()),
        followup: None,
    };
    let dispatch =
        run_platform_turn(state, session_id, prepared.content, Vec::new(), profile).await?;
    deliver_dispatch(state, &context, dispatch).await?;
    Ok(())
}

pub(in crate::platforms::onebot) fn platform_turn_context(
    state: &DaemonState,
    conn: ConnectionHandle,
    target: Target,
    event: &Value,
    config: crate::config::AppConfig,
    inbound_event: Option<PlatformInboundEvent>,
) -> Result<PlatformTurnContext> {
    platform_turn_context_with_activity(state, conn, target, event, config, inbound_event, None)
}

pub(in crate::platforms::onebot) fn platform_turn_context_with_activity(
    state: &DaemonState,
    conn: ConnectionHandle,
    target: Target,
    event: &Value,
    mut config: crate::config::AppConfig,
    inbound_event: Option<PlatformInboundEvent>,
    activity: Option<crate::platforms::MessageActivityHandle>,
) -> Result<PlatformTurnContext> {
    let self_id = event.get("self_id").and_then(Value::as_i64).unwrap_or(0);
    let user_id = event.get("user_id").and_then(Value::as_i64).unwrap_or(0);
    let user_id_text = user_id.to_string();
    let conversation = platform_conversation(target, self_id);
    let conversation_kind = match target {
        Target::Private { .. } => PlatformConversationKind::Private,
        Target::Group { .. } => PlatformConversationKind::Group,
    };
    config.apply_qq_conversation_persona(conversation_kind, &conversation.conversation_id);
    if !config.prompt.active_persona.trim().is_empty()
        && !config
            .persona_path(&state.paths, config.prompt.active_persona.trim())
            .is_file()
    {
        bail!(
            "QQ conversation persona does not exist: {}",
            config.prompt.active_persona
        );
    }
    let sender_display_name = event_sender_display_name(event);
    let is_admin = config.platforms.qq.admin_users.contains(&user_id)
        || has_dynamic_access(
            &state.state_store,
            &conversation.account_id,
            AccessPermission::Administrator,
            &user_id_text,
        );
    let adapter = Arc::new(OneBotAdapter {
        conn,
        registry: state.platforms.onebot.clone(),
        http: state.platforms.http_client()?,
        self_id,
        target,
        max_reply_chars: config.platforms.qq.max_reply_chars,
        file_store_lock: state.platforms.file_store_lock.clone(),
    });
    let mut context = PlatformTurnContext::new(
        conversation,
        user_id_text,
        sender_display_name,
        is_admin,
        config,
        state.paths.clone(),
        state.state_store.clone(),
        adapter,
        state.platforms.plugins()?,
    )
    .with_config_manager(state.manager.clone());
    if let Some(activity) = activity {
        context = context.with_message_activity(activity);
    }
    Ok(match inbound_event {
        Some(event) => context.with_inbound_event(event),
        None => context,
    })
}

/// Turns a parsed inbound message into agent input (downloading media),
/// resolves the dedicated session and runs the turn. `Ok(None)` means
/// the message needs no reply (e.g. sticker-only).
pub(in crate::platforms::onebot) fn platform_update_target(
    state: &DaemonState,
    session_id: &str,
    conversation: &PlatformConversation,
    sender_id: &str,
) -> Option<(String, String, Arc<PlatformFollowupRun>)> {
    let manager = state.manager.lock().unwrap();
    manager
        .active_runs
        .iter()
        .filter(|(_, run)| &*run.session_id == session_id)
        .filter_map(|(run_id, run)| {
            let followup = run.platform_followup.as_ref()?;
            if followup.conversation != *conversation || followup.sender_id != sender_id {
                return None;
            }
            Some((
                followup.started(),
                run_id.clone(),
                run.turn_id.clone()?,
                followup.clone(),
            ))
        })
        .max_by_key(|(started, _, _, _)| *started)
        .map(|(_, run_id, turn_id, followup)| (run_id, turn_id, followup))
}

pub(in crate::platforms::onebot) fn reserve_tool_followup(
    state: &DaemonState,
    session_id: &str,
    conversation: &PlatformConversation,
    sender_id: &str,
) -> Option<(
    String,
    String,
    Arc<PlatformFollowupRun>,
    crate::agent::QueueIngressReservation,
)> {
    let (run_id, turn_id, followup) =
        platform_update_target(state, session_id, conversation, sender_id)?;
    let reservation = followup.try_reserve()?;
    Some((run_id, turn_id, followup, reservation))
}

#[allow(clippy::too_many_arguments)]
pub(in crate::platforms::onebot) async fn enqueue_tool_followup(
    state: &DaemonState,
    conn: &ConnectionHandle,
    _target: Target,
    event: &Value,
    mut parsed: InboundMessage,
    inbound_event: &PlatformInboundEvent,
    context: &PlatformTurnContext,
    followup: &PlatformFollowupRun,
    session_id: &str,
    run_id: &str,
    turn_id: &str,
    mode: TurnUpdateMode,
) -> Result<()> {
    if !parsed.unresolved_image_files.is_empty() {
        resolve_current_message_images(conn, &mut parsed).await;
    }
    let current_message_id = event
        .get("message_id")
        .and_then(value_id_string)
        .unwrap_or_default();
    let quoted_message_data = parsed.quoted_message_data.take();
    let quoted_images = merge_quoted_message_images(
        conn,
        &current_message_id,
        &mut parsed,
        quoted_message_data.as_ref(),
    )
    .await
    .unwrap_or_else(|error| {
        tracing::warn!(
            target: "miyu::qq",
            error = %error,
            message_id = %current_message_id,
            "{}",
            t("OneBot follow-up quoted images could not be prepared", "无法准备 OneBot 后续消息的引用图片")
        );
        0
    });
    let mut content = parsed.text.trim().to_string();
    let prepared_images = prepare_inbound_images(state, parsed.images).await?;
    let attempted_images = prepared_images.attempted;
    let failed_images = prepared_images.failed;
    let mut attachments = Vec::with_capacity(prepared_images.attachments.len());
    for image in prepared_images.attachments.into_iter().flatten() {
        match image {
            ImageAttachment::Binary { mime, data } => {
                attachments.push(QueuedPromptAttachment::Binary {
                    mime,
                    data_base64: BASE64.encode(data),
                });
            }
            ImageAttachment::Path { path } => {
                attachments.push(QueuedPromptAttachment::Path { path });
            }
        }
    }
    let (file_placeholders, queued_files) =
        inbound_file_placeholders(&current_message_id, &parsed.files);
    if !file_placeholders.is_empty() {
        if !content.is_empty() {
            content.push('\n');
        }
        content.push_str(&file_placeholders);
    }
    if content.is_empty() {
        if !attachments.is_empty() {
            content = image_only_prompt(attachments.len());
        } else if attempted_images > 0 {
            bail!("the follow-up image could not be downloaded");
        } else if parsed.at_self {
            content = "(they @-mentioned you without any text)".to_string();
        } else {
            bail!("the follow-up message had no model-visible content");
        }
    }
    if failed_images > 0 {
        content.push_str("\n(the message also contained an image that could not be downloaded; do not claim to have seen it)");
    }
    if quoted_images > 0 {
        content.push_str(&quoted_image_prompt(quoted_images));
    }
    let display_content = content.clone();
    content.push_str("\n\nTrusted metadata for this QQ follow-up message: ");
    content.push_str(&format!(
        "sender QQ={}; message ID={}",
        inbound_event.sender_id, inbound_event.message_id
    ));
    if let Some(reply) = inbound_event.replied_message.as_ref() {
        content.push_str(&format!(
            "; replied-to message ID={}; replied-to sender QQ={}",
            reply.message_id, reply.sender_id
        ));
    }
    if !inbound_event.mentioned_user_ids.is_empty() {
        let mentions = if inbound_event.mentioned_users.is_empty() {
            inbound_event
                .mentioned_user_ids
                .iter()
                .map(|user_id| format!("QQ:{user_id}"))
                .collect::<Vec<_>>()
        } else {
            inbound_event
                .mentioned_users
                .iter()
                .map(|mention| match mention.display_name.as_deref() {
                    Some(name) => format!("{}(QQ:{})", qq_metadata_string(name), mention.user_id),
                    None => format!("QQ:{}", mention.user_id),
                })
                .collect::<Vec<_>>()
        };
        content.push_str(&format!("; @-mentions={}", mentions.join(", ")));
    }

    context.observe_inbound(inbound_event).await;
    let receipt = enqueue_turn_update(
        state,
        TurnUpdateRequest {
            run_id: run_id.to_string(),
            turn_id: turn_id.to_string(),
            session_id: Some(session_id.into()),
            audience: crate::config::PromptAudience::External,
            content,
            display_content,
            attachments,
            uploaded_attachment_ids: Vec::new(),
            mode,
        },
    )?;
    if !queued_files.is_empty() {
        followup
            .context
            .stash_queued_files(&receipt.prompt.prompt_id, queued_files);
    }
    followup.context.accept_followup(inbound_event);
    Ok(())
}

pub(in crate::platforms::onebot) async fn build_and_run_turn(
    state: &DaemonState,
    conn: &ConnectionHandle,
    _target: Target,
    event: &Value,
    mut parsed: InboundMessage,
    context: Arc<PlatformTurnContext>,
    session_id: Arc<str>,
) -> Result<Option<TurnDispatch>> {
    if context.turn_is_superseded() {
        return Ok(None);
    }
    if !parsed.unresolved_image_files.is_empty() {
        resolve_current_message_images(conn, &mut parsed).await;
    }
    let current_message_id = event
        .get("message_id")
        .and_then(value_id_string)
        .unwrap_or_default();
    let quoted_message_data = parsed.quoted_message_data.take();
    let quoted_images = match merge_quoted_message_images(
        conn,
        &current_message_id,
        &mut parsed,
        quoted_message_data.as_ref(),
    )
    .await
    {
        Ok(added) => {
            if added > 0 {
                tracing::info!(
                    target: "miyu::qq",
                    quoted_message_id = parsed.reply_to_message_id.as_deref().unwrap_or_default(),
                    images = added,
                    "{}",
                    t("OneBot quoted-message images added to the model input", "OneBot 引用消息图片已加入模型输入")
                );
            }
            added
        }
        Err(error) => {
            tracing::warn!(
                target: "miyu::qq",
                error = %error,
                quoted_message_id = parsed.reply_to_message_id.as_deref().unwrap_or_default(),
                "{}",
                t("OneBot quoted-message lookup failed", "OneBot 引用消息查询失败")
            );
            0
        }
    };
    let mut content = parsed.text.trim().to_string();

    let prepared_images = prepare_inbound_images(state, parsed.images).await?;
    let attempted_images = prepared_images.attempted;
    let failed_images = prepared_images.failed;
    let images = prepared_images.attachments;
    if attempted_images > 0 {
        tracing::info!(
            target: "miyu::qq",
            attempted = attempted_images,
            prepared = images.len(),
            failed = failed_images,
            duplicates = prepared_images.duplicates,
            total_bytes = prepared_images.total_bytes,
            "{}",
            t("OneBot inbound images prepared for the model", "OneBot 传入图片已为模型准备完成")
        );
    }

    let inbound_message_id = context
        .inbound_event()
        .map(|event| event.message_id.clone())
        .unwrap_or_default();
    let (file_placeholders, current_files) =
        inbound_file_placeholders(&inbound_message_id, &parsed.files);
    if !file_placeholders.is_empty() {
        if !content.is_empty() {
            content.push('\n');
        }
        content.push_str(&file_placeholders);
    }

    if content.is_empty() {
        if !images.is_empty() {
            content = image_only_prompt(images.len());
        } else if attempted_images > 0 {
            context
                .send_bypass_plugins(OutboundMessage::text(
                    OutboundOrigin::Command,
                    t(
                        "I couldn't read that image. Please send it again.",
                        "图片接收失败了，请重新发送一次。",
                    ),
                ))
                .await?;
            return Ok(None);
        } else if parsed.at_self {
            content = "(they @-mentioned you without any text)".to_string();
        } else {
            return Ok(None);
        }
    }
    if failed_images > 0 && !content.is_empty() {
        content.push_str("\n(the message also contained an image that could not be downloaded; do not claim to have seen it)");
    }
    if quoted_images > 0 {
        content.push_str(&quoted_image_prompt(quoted_images));
    }

    if context.turn_is_superseded() {
        return Ok(None);
    }
    let mut prepared = context.prepare_turn(content).await;
    prepared.context_files.extend(current_files);
    // 历史里的 + 当前消息里的文件/视频引用一起登记,供看图工具(含 MCP 桥
    // 另建的工具面)按 id 懒下载(09-04)。
    context.set_context_files(prepared.context_files.clone());
    let content = prepared.content;
    let group_name = context
        .inbound_event()
        .and_then(|event| event.conversation_display_name.as_deref());
    let conversation_kind = match context.conversation.kind {
        ConversationKind::Private => crate::config::PlatformConversationKind::Private,
        ConversationKind::Group => crate::config::PlatformConversationKind::Group,
    };
    let route = context
        .config
        .platforms
        .model_route(conversation_kind, &context.conversation.conversation_id);
    // v7 Phase 2.1: the per-message transport block (sender identity JSON,
    // message ids, mentions) changes on every inbound message. It rides the
    // turn tail via `turn_system_context`; only stable policy text stays in the
    // system prompt so the provider prefix cache survives across messages.
    let mut turn_system_context = vec![qq_turn_system_context(
        &context.config.platforms.qq,
        &context.conversation,
        &context.sender_id,
        &context.sender_display_name,
        context.is_admin,
        context.inbound_event(),
        group_name,
    )];
    // 并发回合的事实注入(08-25 拯救者重复答题取证):历史块里"@我却无人
    // 应答"的消息是跨线代答的最强诱饵——告知那条已有回合在处理,诱饵即拆。
    // 瞬态尾巴通道,化石化为当时事实,不掰前缀。
    {
        let current_id = context
            .inbound_event()
            .map(|event| event.message_id.as_str())
            .unwrap_or("");
        let inflight = crate::platforms::inflight::other_inflight_messages(
            &context.conversation.scope_key(),
            current_id,
        );
        if !inflight.is_empty() {
            let ids = inflight
                .iter()
                .map(|id| format!("msg={id}"))
                .collect::<Vec<_>>()
                .join(", ");
            turn_system_context.push(format!(
                "<qq-inflight>Replies to these recorded messages are already in progress in parallel turns: {ids}. Do not answer them here; they are not abandoned.</qq-inflight>"
            ));
        }
    }
    turn_system_context.extend(prepared.turn_system_context);
    let mut system_context = vec![
        qq_identity_policy(context.conversation.kind),
        qq_history_format(context.config.platforms.qq.user_identification),
        "<qq-context-images>A <context-images> block lists IDs of images sent earlier in this conversation, viewable on demand. You have not seen their content. Call vision_analyze with an ID only when the answer truly depends on the image; never guess image content from placeholders.</qq-context-images>".to_string(),
        // reply_to 语义:被引消息是旧消息、作者是 reply_to 里的人、不是说给
        // 你的话——不解释这三点,模型会把引用内容当成当前对话里别人对它说
        // 的新话(用户 08-20 实测点名)。会话级常量,不掰缓存。
        // 回复定向(08-23 实锤:被"现在咋这么慢"触发的回合开场去研究历史里的
        // AUR 话题)。这条曾内联在本轮消息块里,因随化石重放造成跨轮语义错乱
        // 而被删——放 system 常量既不化石也不掰缓存,是它唯一正确的位置。
        "<qq-reply-target>Answer the messages under [New messages received this turn]. They directly continue [Prior group chat records] in chronological order and are the latest words in this conversation. The records are read-only background you have already seen; never answer a recorded message here, even one that mentions you — it is handled by its own turn. When the new message itself has little content, respond to it briefly in character instead of substituting a recorded question.</qq-reply-target>".to_string(),
        // 08-21 文风批:精简 ~25%,三个语义点(旧消息/作者是 reply_to 里的人/
        // 不是说给你的)一个不丢。
        "<qq-reply-format>A `quoted earlier message` line (and message.reply_to in <qq-request-context>) is old content written earlier by the user named there, not by the current sender. It is background the sender points at, not part of what they say now and not addressed to you. Only the text on the message line itself is the current sender's own words.</qq-reply-format>".to_string(),
    ];
    if let Some(prompt) = route
        .map(|route| route.extra_prompt.trim())
        .filter(|prompt| !prompt.is_empty())
    {
        system_context.push(format!(
            "Additional rules for this QQ conversation:\n{prompt}"
        ));
    }
    system_context.extend(prepared.system_context);
    let profile = TurnProfile {
        active_persona: Some(context.config.prompt.active_persona.clone()),
        text_models: context.config.active_provider_models.clone(),
        multimodal_models: context
            .config
            .qq_multimodal_model_pool(conversation_kind, &context.conversation.conversation_id)
            .map(<[_]>::to_vec),
        system_context,
        turn_system_context,
        memory_content: Some(prepared.memory_content),
        context_images: prepared.context_images,
        context_files: prepared.context_files.into_boxed_slice(),
        image_cache_namespace: Some("qq".to_string()),
        image_source_label: Some("QQ".to_string()),
        memory_write_enabled: context.config.platforms.qq.memory.write_enabled,
        // Groups keep their own turn history now. The structured log still
        // carries who said what — the protocol offers no third role and drops
        // `name`, so identity can only live in the text — but the log is
        // additive: each turn appends what arrived since the last one, and
        // earlier turns replay verbatim. Miyu's own turns become real
        // assistant messages instead of one `[你]` line in a rolling window.
        suppress_session_history: false,
        group_context: (context.conversation.kind == ConversationKind::Group)
            .then(|| context.config.platforms.qq.group_context.clone()),
        platform: Some(context),
        followup: None,
    };
    let dispatch = run_platform_turn(state, session_id, content, images, profile).await?;
    Ok(Some(dispatch))
}

pub(in crate::platforms::onebot) fn resolve_onebot_session(
    state: &DaemonState,
    context: &PlatformTurnContext,
    target: Target,
    event: &Value,
) -> Result<Arc<str>> {
    let session_name = session_name_for(target, event);
    let legacy_name = legacy_session_name_for(target);
    resolve_platform_session(
        state,
        &context.conversation,
        &context.config.active_persona_scope(),
        None,
        &session_name,
        Some(&legacy_name),
    )
}

/// Session-name key for this conversation. Group history is always shared by
/// the whole group; the bot account still isolates multiple QQ adapters.
pub(in crate::platforms::onebot) fn session_name_for(target: Target, event: &Value) -> String {
    let self_id = event.get("self_id").and_then(Value::as_i64).unwrap_or(0);
    match target {
        Target::Private { user_id } => format!("qq:{self_id}:private:{user_id}"),
        Target::Group { group_id } => format!("qq:{self_id}:group:{group_id}"),
    }
}

pub(in crate::platforms::onebot) fn legacy_session_name_for(target: Target) -> String {
    match target {
        Target::Private { user_id } => format!("qq:private:{user_id}"),
        Target::Group { group_id } => format!("qq:group:{group_id}"),
    }
}
