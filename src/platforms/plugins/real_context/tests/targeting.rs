//! 主动回复的目标选择与提示词拼装。

use super::shared::*;
use crate::platforms::plugins::real_context::*;

#[test]
fn disabled_targeting_returns_none_and_core_fallback_is_preserved_exactly() {
    let settings = RealContextPluginSettings {
        reply_target_enable: false,
        ..RealContextPluginSettings::default()
    };
    assert_eq!(response_target(&inbound_event(), &settings), None);

    let core = TriggerDecision {
        should_reply: true,
        content: "核心触发内容".to_string(),
        response_target: Some(ResponseTarget::quoted("core-message", "core-user")),
    };
    let mut changed = TriggerDecision {
        should_reply: false,
        content: "插件临时内容".to_string(),
        response_target: Some(ResponseTarget {
            message_id: "guessed-message".to_string(),
            user_id: "guessed-user".to_string(),
            quote: true,
            mention: true,
            explicit_mention_user_ids: Vec::new(),
        }),
    };

    restore_trigger_decision(&mut changed, &core);

    assert_eq!(changed.should_reply, core.should_reply);
    assert_eq!(changed.content, core.content);
    assert_eq!(changed.response_target, core.response_target);
}

#[test]
fn active_target_limits_keep_recent_text_and_supplements() {
    let event = inbound_event();
    let mut targets = (0..12)
        .map(|index| {
            let mut target = active_reply_target(&event);
            target.message_id = format!("text-{index}");
            target.content = format!("text {index}");
            target
        })
        .collect::<Vec<_>>();
    targets.extend((0..8).map(|index| {
        let mut target = active_reply_target(&event);
        target.message_id = format!("image-{index}");
        target.content.clear();
        target.supplemental = true;
        target
    }));

    normalize_active_targets(&mut targets, &event.sender_id);

    assert_eq!(
        targets.iter().filter(|target| !target.supplemental).count(),
        8
    );
    assert_eq!(
        targets.iter().filter(|target| target.supplemental).count(),
        5
    );
    assert!(!targets.iter().any(|target| target.message_id == "text-0"));
    assert!(targets.iter().any(|target| target.message_id == "text-11"));
    assert!(!targets.iter().any(|target| target.message_id == "image-0"));
    assert!(targets.iter().any(|target| target.message_id == "image-7"));
}

#[test]
fn active_target_prompt_is_bounded_and_keeps_the_current_message() {
    let (_temp, context) = availability_context(BotSendAvailability::Available);
    let mut current = inbound_event();
    current.message_id = "current".to_string();
    let mut targets = (0..MAX_ACTIVE_TARGET_MESSAGES)
        .map(|index| {
            let mut target = active_reply_target(&current);
            target.message_id = format!("old-{index}");
            target.content = "旧".repeat(20_000);
            target
        })
        .collect::<Vec<_>>();
    targets.push(active_reply_target(&current));
    set_active_targets(&context, &targets);

    let current_content = format!("CURRENT:{}", "新".repeat(20_000));
    let prompt = active_target_prompt(&context, &current, &current_content);

    assert!(prompt.len() <= MAX_ACTIVE_TARGET_PROMPT_BYTES);
    assert!(prompt.contains("CURRENT:"));
    assert!(prompt.contains("earlier merged messages omitted due to length limits"));
    // 截断保留的头部是带标记+署名行的当前消息,而不是裸正文。
    assert!(prompt.starts_with("[New messages received this turn]\n["));
    assert!(prompt.lines().nth(1).unwrap().contains("CURRENT:"));
}

#[test]
fn supersede_inherits_targets_only_for_the_same_sender() {
    let plugin = RealContextPlugin::new();
    let (_temp, context) = availability_context(BotSendAvailability::Available);
    let event = inbound_event();
    let (cancel, _receiver) = tokio::sync::watch::channel(false);
    let target = active_reply_target(&event);
    plugin
        .runtime
        .lock()
        .unwrap()
        .session_mut(&runtime_session_key(&context), Instant::now())
        .pending
        .insert(
            event.sender_id.clone(),
            PendingReply {
                generation: 1,
                started: Instant::now(),
                trigger: TriggerKind::Probability,
                // 接管只对已承诺的回复开放;未承诺的场景见 trigger.rs 的
                // an_uncommitted_takeover_goes_back_to_the_judge_not_the_bypass。
                committed: true,
                reactions: Vec::new(),
                targets: vec![target],
                cancel,
            },
        );

    assert!(plugin.preempt_inbound(&context, &event).unwrap());
    let inherited = active_targets_from_context(&context);
    assert_eq!(inherited.len(), 1);
    assert_eq!(inherited[0].sender_id, event.sender_id);

    active_judgement_skip::apply_active_judgement_skip_editor_changes(
        &context.state_store,
        &[],
        &[event.sender_id.parse().unwrap()],
    )
    .unwrap();
    assert!(!plugin.preempt_inbound(&context, &event).unwrap());

    let mut other = event.clone();
    other.sender_id = "other-user".to_string();
    assert!(!plugin.preempt_inbound(&context, &other).unwrap());
}

/// 被 @ 时,自己那一项渲染成 `[you]`;没被 @ 时这行里单纯没有自己——
/// 不再有任何「你没被提到」的否定陈述可供她立规矩(08-29)。
#[test]
fn the_mention_line_marks_the_bot_itself_and_says_nothing_when_absent() {
    let mentions = vec![
        PlatformMention {
            user_id: "10000".to_string(),
            display_name: Some("Miyu".to_string()),
        },
        PlatformMention {
            user_id: "40000".to_string(),
            display_name: Some("yuyi".to_string()),
        },
    ];
    let ids = vec!["10000".to_string(), "40000".to_string()];

    let rendered = format_mentioned_users(&mentions, &ids, true, Some("10000")).unwrap();
    assert_eq!(rendered, "[you]、yuyi(QQ:40000)");

    // 别人之间互相 @：这行里没有自己,也没有任何"你没被提到"的说法。
    let others = vec![PlatformMention {
        user_id: "40000".to_string(),
        display_name: Some("yuyi".to_string()),
    }];
    let rendered =
        format_mentioned_users(&others, &["40000".to_string()], true, Some("10000")).unwrap();
    assert_eq!(rendered, "yuyi(QQ:40000)");
    assert!(!rendered.contains("you"));

    // 历史块传 None:行为与改动前一致。
    let rendered = format_mentioned_users(&mentions, &ids, true, None).unwrap();
    assert_eq!(rendered, "Miyu(QQ:10000)、yuyi(QQ:40000)");
}
