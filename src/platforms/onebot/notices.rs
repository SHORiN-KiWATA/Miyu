//! 非消息事件：撤回、上传、禁言、进出群、好友申请。
//!
//! 这些事件和普通消息共用一条入口，靠 `is_*` 一族判定分流。判定写成独立函数
//! 而不是内联的 match，是因为同一个事件在不同 OneBot 实现里字段名不完全一致，
//! 兼容逻辑要有唯一的落点。
//!
//! 禁言通知会**回写缓存**（`update_group_ban_notice`）：被禁言时收到通知就直接
//! 更新，比等下次查询再发现要及时。

use crate::platforms::onebot::*;

pub(in crate::platforms::onebot) fn is_message_recall(event: &Value) -> bool {
    event.get("post_type").and_then(Value::as_str) == Some("notice")
        && matches!(
            event.get("notice_type").and_then(Value::as_str),
            Some("group_recall" | "friend_recall")
        )
}

pub(in crate::platforms::onebot) fn is_group_upload_notice(event: &Value) -> bool {
    event.get("post_type").and_then(Value::as_str) == Some("notice")
        && event.get("notice_type").and_then(Value::as_str) == Some("group_upload")
}

pub(in crate::platforms::onebot) fn is_friend_add_request(event: &Value) -> bool {
    event.get("post_type").and_then(Value::as_str) == Some("request")
        && event.get("request_type").and_then(Value::as_str) == Some("friend")
}

pub(in crate::platforms::onebot) fn is_group_request(event: &Value) -> bool {
    event.get("post_type").and_then(Value::as_str) == Some("request")
        && event.get("request_type").and_then(Value::as_str) == Some("group")
}

pub(in crate::platforms::onebot) fn is_group_add_request(event: &Value) -> bool {
    is_group_request(event) && event.get("sub_type").and_then(Value::as_str) == Some("add")
}

pub(in crate::platforms::onebot) fn is_group_invite_request(event: &Value) -> bool {
    is_group_request(event) && event.get("sub_type").and_then(Value::as_str) == Some("invite")
}

pub(in crate::platforms::onebot) fn is_group_ban_notice(event: &Value) -> bool {
    event.get("post_type").and_then(Value::as_str) == Some("notice")
        && event.get("notice_type").and_then(Value::as_str) == Some("group_ban")
}

pub(in crate::platforms::onebot) fn is_group_decrease_notice(event: &Value) -> bool {
    event.get("post_type").and_then(Value::as_str) == Some("notice")
        && event.get("notice_type").and_then(Value::as_str) == Some("group_decrease")
        && event.get("sub_type").and_then(Value::as_str) == Some("kick")
}

pub(in crate::platforms::onebot) fn update_group_ban_notice(event: &Value) {
    let self_id = event.get("self_id").and_then(Value::as_i64).unwrap_or(0);
    let group_id = event.get("group_id").and_then(Value::as_i64).unwrap_or(0);
    let user_id = event.get("user_id").and_then(Value::as_i64).unwrap_or(-1);
    if self_id == 0 || group_id == 0 || !matches!(user_id, 0) && user_id != self_id {
        return;
    }
    let duration = event.get("duration").and_then(Value::as_u64).unwrap_or(0);
    let sub_type = event.get("sub_type").and_then(Value::as_str);
    if user_id == 0 && duration == 0 && !matches!(sub_type, Some("ban" | "lift_ban")) {
        return;
    }
    let lifted = sub_type == Some("lift_ban") || user_id != 0 && duration == 0;
    let now = Instant::now();
    let (availability, ttl) = if lifted {
        (BotSendAvailability::Available, GROUP_MUTE_AVAILABLE_TTL)
    } else {
        (
            BotSendAvailability::Muted,
            if duration == 0 {
                GROUP_MUTE_WHOLE_NOTICE_TTL
            } else {
                // 上界是禁言时长,但到点要复查——提前解禁的通知可能收不到。
                Duration::from_secs(duration).min(GROUP_MUTE_MUTED_TTL)
            },
        )
    };
    group_mute_cache()
        .lock()
        .unwrap()
        .insert((self_id, group_id), availability, ttl, now);
}

pub(in crate::platforms::onebot) fn recall_event(
    target: Target,
    event: &Value,
    user_id: i64,
) -> PlatformInboundEvent {
    let self_id = event.get("self_id").and_then(Value::as_i64).unwrap_or(0);
    PlatformInboundEvent {
        kind: PlatformInboundEventKind::MessageRecall,
        conversation: platform_conversation(target, self_id),
        conversation_display_name: None,
        message_id: event
            .get("message_id")
            .and_then(value_id_string)
            .unwrap_or_default(),
        sender_id: user_id.to_string(),
        sender_display_name: event_sender_display_name(event),
        operator_id: event.get("operator_id").and_then(value_id_string),
        timestamp: event.get("time").and_then(Value::as_i64).unwrap_or(0),
        received_at: Instant::now(),
        message_position: None,
        ingress_order: None,
        text: String::new(),
        reply_to_message_id: None,
        replied_message: None,
        mentioned_user_ids: Vec::new(),
        mentioned_users: Vec::new(),
        mentioned_bot: false,
        media: Vec::new(),
        notice_sub_type: event
            .get("sub_type")
            .and_then(Value::as_str)
            .map(str::to_string),
        duration_seconds: None,
    }
}

pub(in crate::platforms::onebot) fn group_upload_event(
    event: &Value,
) -> Option<PlatformInboundEvent> {
    let self_id = event.get("self_id").and_then(Value::as_i64)?;
    let group_id = event.get("group_id").and_then(Value::as_i64)?;
    let user_id = event.get("user_id").and_then(Value::as_i64)?;
    let file = event.get("file")?;
    let file_id = file.get("id").and_then(Value::as_str).map(str::trim);
    let file_name = file
        .get("name")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|name| !name.is_empty());
    if self_id == 0 || group_id == 0 || user_id == 0 {
        return None;
    }
    let Some(file_id) = file_id.filter(|id| !id.is_empty()) else {
        return None;
    };
    let media_id = file_id.to_string();
    let message_id = format!(
        "group_file_{}",
        file_id
            .trim_start_matches('/')
            .chars()
            .map(|ch| {
                if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.') {
                    ch
                } else {
                    '_'
                }
            })
            .collect::<String>()
    );
    Some(PlatformInboundEvent {
        kind: PlatformInboundEventKind::GroupFileUpload,
        conversation: platform_conversation(Target::Group { group_id }, self_id),
        conversation_display_name: None,
        message_id,
        sender_id: user_id.to_string(),
        sender_display_name: String::new(),
        operator_id: None,
        timestamp: event.get("time").and_then(Value::as_i64).unwrap_or(0),
        received_at: Instant::now(),
        message_position: None,
        ingress_order: Some(next_ingress_order()),
        text: String::new(),
        reply_to_message_id: None,
        replied_message: None,
        mentioned_user_ids: Vec::new(),
        mentioned_users: Vec::new(),
        mentioned_bot: false,
        media: vec![PlatformInboundMedia {
            kind: PlatformMediaKind::File,
            id: Some(media_id),
            name: file_name.map(str::to_string),
            url: None,
        }],
        notice_sub_type: None,
        duration_seconds: None,
    })
}

pub(in crate::platforms::onebot) async fn handle_group_file_upload(
    state: DaemonState,
    conn: ConnectionHandle,
    event: Value,
) {
    let app_config = state.manager.lock().unwrap().config.clone();
    let config = &app_config.platforms.qq;
    if !config.enabled {
        return;
    }
    let self_id = event.get("self_id").and_then(Value::as_i64).unwrap_or(0);
    let user_id = event.get("user_id").and_then(Value::as_i64).unwrap_or(0);
    let Some(group_id) = event
        .get("group_id")
        .and_then(Value::as_i64)
        .filter(|group_id| *group_id != 0)
    else {
        return;
    };
    let Some(mut inbound) = group_upload_event(&event) else {
        return;
    };
    let target = Target::Group { group_id };
    if !admission_for_with_state(config, &state.state_store, target, self_id, user_id).allowed {
        return;
    }
    let context = match platform_turn_context(
        &state,
        conn,
        target,
        &event,
        app_config,
        Some(inbound.clone()),
    ) {
        Ok(context) => context,
        Err(error) => {
            tracing::warn!(target: "miyu::qq", error = %error, "{}", t("OneBot group-file observer initialization failed", "OneBot 群文件观察器初始化失败"));
            return;
        }
    };
    if inbound.sender_display_name.trim().is_empty() {
        if let Ok(Some(member)) = context.group_member(&inbound.sender_id).await {
            inbound.sender_display_name = member.display_name().trim().to_string();
        }
    }
    context.observe_inbound(&inbound).await;
}

pub(in crate::platforms::onebot) fn group_management_notice(
    event: &Value,
) -> Option<PlatformInboundEvent> {
    let self_id = event.get("self_id").and_then(Value::as_i64)?;
    let group_id = event.get("group_id").and_then(Value::as_i64)?;
    let user_id = event.get("user_id").and_then(Value::as_i64)?;
    let kind = match event.get("notice_type").and_then(Value::as_str)? {
        "group_ban" => PlatformInboundEventKind::GroupBan,
        "group_decrease" => PlatformInboundEventKind::GroupDecrease,
        _ => return None,
    };
    if self_id == 0 || group_id == 0 || user_id == 0 {
        return None;
    }
    Some(PlatformInboundEvent {
        kind,
        conversation: platform_conversation(Target::Group { group_id }, self_id),
        conversation_display_name: None,
        message_id: String::new(),
        sender_id: user_id.to_string(),
        sender_display_name: user_id.to_string(),
        operator_id: event.get("operator_id").and_then(value_id_string),
        timestamp: event.get("time").and_then(Value::as_i64).unwrap_or(0),
        received_at: Instant::now(),
        message_position: None,
        ingress_order: None,
        text: String::new(),
        reply_to_message_id: None,
        replied_message: None,
        mentioned_user_ids: Vec::new(),
        mentioned_users: Vec::new(),
        mentioned_bot: false,
        media: Vec::new(),
        notice_sub_type: event
            .get("sub_type")
            .and_then(Value::as_str)
            .map(str::to_string),
        duration_seconds: event.get("duration").and_then(Value::as_u64),
    })
}

pub(in crate::platforms::onebot) async fn handle_group_management_notice(
    state: DaemonState,
    conn: ConnectionHandle,
    event: Value,
) {
    let Some(inbound) = group_management_notice(&event) else {
        return;
    };
    let config = state.manager.lock().unwrap().config.clone();
    if !config.platforms.qq.enabled {
        return;
    }
    let group_id = inbound
        .conversation
        .conversation_id
        .parse::<i64>()
        .unwrap_or(0);
    let user_id = inbound.sender_id.parse::<i64>().unwrap_or(0);
    let self_id = inbound.conversation.account_id.parse::<i64>().unwrap_or(0);
    let target = Target::Group { group_id };
    if !admission_for_with_state(
        &config.platforms.qq,
        &state.state_store,
        target,
        self_id,
        user_id,
    )
    .allowed
    {
        return;
    }
    match platform_turn_context(&state, conn, target, &event, config, Some(inbound.clone())) {
        Ok(context) => context.observe_inbound(&inbound).await,
        Err(error) => {
            tracing::warn!(target: "miyu::qq", error = %error, "{}", t("OneBot group notice observer initialization failed", "OneBot 群通知观察器初始化失败"))
        }
    }
}

pub(in crate::platforms::onebot) async fn handle_message_recall(
    state: DaemonState,
    conn: ConnectionHandle,
    event: Value,
) {
    let app_config = state.manager.lock().unwrap().config.clone();
    let config = &app_config.platforms.qq;
    if !config.enabled {
        return;
    }
    let self_id = event.get("self_id").and_then(Value::as_i64).unwrap_or(0);
    let user_id = event.get("user_id").and_then(Value::as_i64).unwrap_or(0);
    if self_id == 0 || user_id == 0 {
        return;
    }
    let target = match event.get("notice_type").and_then(Value::as_str) {
        Some("group_recall") => event
            .get("group_id")
            .and_then(Value::as_i64)
            .filter(|group_id| *group_id != 0)
            .map(|group_id| Target::Group { group_id }),
        Some("friend_recall") => Some(Target::Private { user_id }),
        _ => None,
    };
    let Some(target) = target else { return };
    if !admission_for_with_state(config, &state.state_store, target, self_id, user_id).allowed {
        return;
    }
    let inbound = recall_event(target, &event, user_id);
    let context = match platform_turn_context(
        &state,
        conn,
        target,
        &event,
        app_config,
        Some(inbound.clone()),
    ) {
        Ok(context) => context,
        Err(error) => {
            tracing::warn!(target: "miyu::qq", error = %error, "{}", t("OneBot recall observer initialization failed", "OneBot 撤回观察器初始化失败"));
            return;
        }
    };
    context.observe_inbound(&inbound).await;
}
