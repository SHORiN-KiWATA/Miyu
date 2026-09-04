//! 运行时状态与日志。

use crate::platforms::plugins::real_context::*;

#[test]
fn inactive_session_runtime_cache_has_a_hard_soft_limit() {
    let now = Instant::now();
    let mut runtime = RuntimeState::default();
    for index in 0..SESSION_STATE_SOFT_LIMIT + 32 {
        runtime.session_mut(&format!("group-{index}"), now);
    }

    runtime.prune(now);

    assert_eq!(runtime.sessions.len(), SESSION_STATE_SOFT_LIMIT);
}

#[test]
fn active_reply_decision_log_is_structured_for_humans() {
    let moderation = judge::ModerationResult {
        violation: false,
        severity: 0.0,
        ..judge::ModerationResult::default()
    };
    let log = ActiveReplyDecisionLog {
        account_id: "10000",
        group_id: "20000",
        sender_name: "测试用户",
        sender_id: "30000",
        mentioned_bot: false,
        message: "引用上一条消息\n继续讨论",
        trigger: TriggerKind::Continuation,
        should_reply: true,
        model_should_reply: Some(true),
        raw_score: 0.72,
        final_score: 0.91,
        threshold: 0.84,
        model_adjustment: 0.2,
        affection_level: "熟人",
        affection_adjustment: 0.03,
        emotion_adjustment: 0.0,
        continuation_adjustment: 0.05,
        system_adjustment: 0.0,
        reply_heat: 1.25,
        heat_penalty: 0.06,
        heat_threshold_adjustment: 0.03,
        short_message_threshold_adjustment: 0.01,
        moderation: &moderation,
        reason: "当前消息延续了上一轮问题。",
        endpoint: Some("opencodego / mimo-v2.5"),
    };
    let rendered = format_active_reply_decision_log_for(&log, Locale::Zh);

    assert!(rendered.starts_with("【续聊窗口判断：回复】\n"));
    assert!(rendered.contains("会话：群聊 20000（机器人 QQ 10000）"));
    assert!(rendered.contains("发送者：测试用户（QQ 30000）"));
    assert!(rendered.contains("@机器人：否"));
    assert!(rendered.contains("模型：opencodego / mimo-v2.5"));
    assert!(rendered.contains("消息：引用上一条消息 继续讨论"));
    assert!(rendered.contains("触发：自然续聊 (continuation)"));
    assert!(rendered.contains("结果：回复"));
    assert!(rendered.contains("分数：0.910（原始 0.720，阈值 0.840）"));
    assert!(rendered.contains("回复倾向调整：应该回复 +0.200"));
    assert!(rendered.contains("好感度调整：熟人 +0.030"));
    assert!(rendered.contains("自然续聊调整：+0.050"));
    assert!(!rendered.contains("直接触发调整"));
    assert!(rendered.contains("冷静机制调整：扣分 -0.060，阈值 +0.030（冷静度 1.250）"));
    assert!(rendered.contains("短句阈值调整：+0.010"));
    assert!(!rendered.contains("安全初判"));
    assert!(rendered.ends_with("判断理由：当前消息延续了上一轮问题。"));
    assert_eq!(
        TriggerKind::Probability.decision_log_title(false, Locale::Zh),
        "【主动回复判断：不回复】"
    );
    let english = format_active_reply_decision_log_for(&log, Locale::En);
    assert!(english.starts_with("[Continuation decision: reply]\n"));
    assert!(english.contains("Conversation: group 20000 (bot QQ 10000)"));
    assert!(english.contains("Affection adjustment: 熟人 +0.030"));
    assert!(english.ends_with("Reason: 当前消息延续了上一轮问题。"));
}

#[test]
fn active_reply_skip_log_keeps_session_sender_and_reason() {
    assert_eq!(
        format_active_reply_skip_log_for(
            "10000",
            "20000",
            "测试用户",
            "30000",
            TriggerKind::Direct,
            "被新消息覆盖",
            Locale::Zh,
        ),
        "（跳过主动判断）\n会话：群聊 20000（机器人 QQ 10000）\n发送者：测试用户（QQ 30000）\n触发：直接触发 (direct)\n结果：跳过\n判断原因：被新消息覆盖"
    );
    assert!(format_active_reply_skip_log_for(
        "10000",
        "20000",
        "User",
        "30000",
        TriggerKind::Direct,
        "superseded",
        Locale::En,
    )
    .starts_with("[Active reply decision skipped]\nConversation: group 20000"));
}
