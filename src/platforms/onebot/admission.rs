//! 「这条消息要不要理」的准入判定。
//!
//! 群里绝大多数消息不是说给 Miyu 听的，所以先判准入再谈别的。判定拆成三层：
//! `admission_for_access` 只看权限（黑白名单、管理员），`admission_for_with_state`
//! 叠上会话状态（是否正在忙、是否被 @），`admission_for` 是给调用方的门面。
//!
//! `observe_message_activity` 记的是群热度，用来决定要不要降低插话频率；
//! `sends_rate_limit_notice` 控制限流提示只发一次——每条都提示比不提示更烦人。

use crate::platforms::onebot::*;

pub(in crate::platforms::onebot) fn next_ingress_order() -> i64 {
    let wall_clock = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos().min(i64::MAX as u128) as i64)
        .unwrap_or_default();
    let mut previous = LAST_INGRESS_ORDER.load(AtomicOrdering::Relaxed);
    loop {
        let next = wall_clock.max(previous.saturating_add(1));
        match LAST_INGRESS_ORDER.compare_exchange_weak(
            previous,
            next,
            AtomicOrdering::AcqRel,
            AtomicOrdering::Relaxed,
        ) {
            Ok(_) => return next,
            Err(current) => previous = current,
        }
    }
}

#[derive(Clone)]
pub(in crate::platforms::onebot) struct InboundMessageActivity {
    pub(in crate::platforms::onebot) handle: crate::platforms::MessageActivityHandle,
    pub(in crate::platforms::onebot) position: PlatformMessagePosition,
    pub(in crate::platforms::onebot) received_at: Instant,
}

pub(in crate::platforms::onebot) fn observe_message_activity(
    state: &DaemonState,
    event: &Value,
    fallback_self_id: i64,
    received_at: Instant,
) -> Option<InboundMessageActivity> {
    let self_id = event
        .get("self_id")
        .and_then(Value::as_i64)
        .filter(|id| *id != 0)
        .unwrap_or(fallback_self_id);
    let user_id = event.get("user_id").and_then(Value::as_i64)?;
    if self_id == 0 || user_id == 0 || user_id == self_id {
        return None;
    }
    let target = match event.get("message_type").and_then(Value::as_str) {
        Some("private") => Target::Private { user_id },
        Some("group") => Target::Group {
            group_id: event
                .get("group_id")
                .and_then(Value::as_i64)
                .filter(|group_id| *group_id != 0)?,
        },
        _ => return None,
    };
    let conversation = platform_conversation(target, self_id);
    let message_id = event
        .get("message_id")
        .and_then(value_id_string)
        .unwrap_or_default();
    let sender_id = user_id.to_string();
    let (handle, position, received_at) = state.platforms.message_activity.observe(
        &conversation.scope_key(),
        &message_id,
        &sender_id,
        received_at,
    );
    Some(InboundMessageActivity {
        handle,
        position,
        received_at,
    })
}

pub(in crate::platforms::onebot) fn ingress_message_event(
    event: &Value,
    fallback_self_id: i64,
    ingress_order: i64,
    activity: Option<&InboundMessageActivity>,
) -> Option<PlatformInboundEvent> {
    let self_id = event
        .get("self_id")
        .and_then(Value::as_i64)
        .filter(|id| *id != 0)
        .unwrap_or(fallback_self_id);
    let user_id = event.get("user_id").and_then(Value::as_i64)?;
    if self_id == 0 || user_id == 0 || user_id == self_id {
        return None;
    }
    let target = match event.get("message_type").and_then(Value::as_str) {
        Some("private") => Target::Private { user_id },
        Some("group") => Target::Group {
            group_id: event
                .get("group_id")
                .and_then(Value::as_i64)
                .filter(|group_id| *group_id != 0)?,
        },
        _ => return None,
    };
    // 绝大多数帧自带正确 self_id,覆写是恒等操作:只在确需归一时才整帧
    // 深拷贝,避免每条入站消息为此付一次完整 Value 克隆。
    let normalized_event;
    let event_ref = if event.get("self_id").and_then(Value::as_i64) == Some(self_id) {
        event
    } else {
        let mut cloned = event.clone();
        cloned["self_id"] = Value::from(self_id);
        normalized_event = cloned;
        &normalized_event
    };
    let parsed = parse_message(
        event_ref.get("message"),
        event_ref.get("raw_message"),
        self_id,
    );
    let mut inbound = message_event_at(
        target,
        event_ref,
        &parsed,
        activity
            .map(|activity| activity.received_at)
            .unwrap_or_else(Instant::now),
        activity.map(|activity| activity.position),
    );
    inbound.ingress_order = Some(ingress_order);
    Some(inbound)
}

pub(in crate::platforms::onebot) fn sends_rate_limit_notice(target: Target) -> bool {
    matches!(target, Target::Group { .. })
}

pub(in crate::platforms::onebot) struct Admission {
    pub(in crate::platforms::onebot) allowed: bool,
    pub(in crate::platforms::onebot) rate_key: Option<String>,
    pub(in crate::platforms::onebot) rate_limit: PlatformRateLimit,
    pub(in crate::platforms::onebot) use_non_whitelist_text_models: bool,
}

pub(in crate::platforms::onebot) fn admission_for(
    config: &OneBotConfig,
    target: Target,
    self_id: i64,
    user_id: i64,
) -> Admission {
    admission_for_access(config, None, target, self_id, user_id)
}

pub(in crate::platforms::onebot) fn admission_for_with_state(
    config: &OneBotConfig,
    state: &StateStore,
    target: Target,
    self_id: i64,
    user_id: i64,
) -> Admission {
    admission_for_access(config, Some(state), target, self_id, user_id)
}

pub(in crate::platforms::onebot) fn admission_for_access(
    config: &OneBotConfig,
    state: Option<&StateStore>,
    target: Target,
    self_id: i64,
    user_id: i64,
) -> Admission {
    admission_for_access_at(
        config,
        state,
        target,
        self_id,
        user_id,
        chrono::Local::now().time(),
    )
}

/// 睡眠时间内的拒绝:不限流、不换模型池,单纯当没看见。
fn asleep() -> Admission {
    Admission {
        allowed: false,
        rate_key: None,
        rate_limit: PlatformRateLimit::default(),
        use_non_whitelist_text_models: false,
    }
}

/// `now` 是本机时区的墙钟时间,拆出来是为了让睡眠时间可测。
pub(in crate::platforms::onebot) fn admission_for_access_at(
    config: &OneBotConfig,
    state: Option<&StateStore>,
    target: Target,
    self_id: i64,
    user_id: i64,
    now: chrono::NaiveTime,
) -> Admission {
    let sleeping = config.is_sleeping_at(now);
    let account_id = self_id.to_string();
    let user_id_text = user_id.to_string();
    let is_admin = state.map_or_else(
        || config.admin_users.contains(&user_id),
        |state| {
            config.admin_users.contains(&user_id)
                || has_dynamic_access(
                    state,
                    &account_id,
                    AccessPermission::Administrator,
                    &user_id_text,
                )
        },
    );
    // 睡眠时间:管理员在哪都叫得醒;私聊白名单只在私聊里叫得醒;其余一概不理
    // (群消息不进历史、不概率插话,通知类事件同样走这一闸)。
    if sleeping && !is_admin && matches!(target, Target::Group { .. }) {
        return asleep();
    }
    match target {
        Target::Private { user_id } => {
            if is_admin {
                return Admission {
                    allowed: true,
                    rate_key: None,
                    rate_limit: PlatformRateLimit::default(),
                    use_non_whitelist_text_models: false,
                };
            }
            let whitelisted = state.map_or_else(
                || config.private_chats.whitelist.contains(&user_id),
                |state| {
                    config.private_chats.whitelist.contains(&user_id)
                        || has_dynamic_access(
                            state,
                            &account_id,
                            AccessPermission::PrivateWhitelist,
                            &user_id_text,
                        )
                },
            );
            if sleeping && !whitelisted {
                return asleep();
            }
            if whitelisted {
                Admission {
                    allowed: true,
                    rate_key: None,
                    rate_limit: PlatformRateLimit::default(),
                    use_non_whitelist_text_models: false,
                }
            } else {
                Admission {
                    allowed: config.private_chats.allow_non_whitelist,
                    rate_key: Some(format!("qq:{self_id}:private:{user_id}")),
                    rate_limit: config.private_chats.non_whitelist_rate_limit,
                    use_non_whitelist_text_models: true,
                }
            }
        }
        Target::Group { group_id } => {
            let group_id_text = group_id.to_string();
            let whitelisted = state.map_or_else(
                || config.group_chats.whitelist.contains(&group_id),
                |state| {
                    config.group_chats.whitelist.contains(&group_id)
                        || has_dynamic_access(
                            state,
                            &account_id,
                            AccessPermission::GroupWhitelist,
                            &group_id_text,
                        )
                },
            );
            if is_admin {
                return Admission {
                    allowed: true,
                    rate_key: None,
                    rate_limit: PlatformRateLimit::default(),
                    use_non_whitelist_text_models: !whitelisted,
                };
            }
            let privileged = state.map_or_else(
                || config.private_chats.whitelist.contains(&user_id),
                |state| {
                    config.private_chats.whitelist.contains(&user_id)
                        || has_dynamic_access(
                            state,
                            &account_id,
                            AccessPermission::PrivateWhitelist,
                            &user_id_text,
                        )
                },
            );
            Admission {
                allowed: whitelisted || config.group_chats.allow_non_whitelist,
                rate_key: (!privileged).then(|| format!("qq:{self_id}:group:{group_id}")),
                rate_limit: if whitelisted {
                    config.group_chats.whitelist_rate_limit
                } else {
                    config.group_chats.non_whitelist_rate_limit
                },
                use_non_whitelist_text_models: !whitelisted,
            }
        }
    }
}

pub(in crate::platforms::onebot) fn apply_admission_text_model_pool(
    config: &mut crate::config::AppConfig,
    target: Target,
    admission: &Admission,
) {
    let kind = match target {
        Target::Private { .. } => PlatformConversationKind::Private,
        Target::Group { .. } => PlatformConversationKind::Group,
    };
    let conversation_id = target.conversation_id().to_string();
    let models = config.qq_text_model_pool(
        kind,
        &conversation_id,
        admission.use_non_whitelist_text_models,
    );
    config.active_provider_models = models;
}
