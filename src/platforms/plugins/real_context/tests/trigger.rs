//! 触发判定与延续窗口。

use super::shared::*;
use crate::platforms::plugins::real_context::*;

#[test]
fn explicit_direct_trigger_precedes_moderation_only_candidates() {
    assert_eq!(
        select_trigger(true, true, Some(TriggerKind::Supersede), true, true),
        Some(TriggerKind::Direct)
    );
    assert_eq!(
        select_trigger(false, true, Some(TriggerKind::Supersede), true, true),
        Some(TriggerKind::Moderation)
    );
}

/// 覆盖继承保留原始触发:标签是好感度归类、`<qq-join-in>` 注入与判官加分的
/// 判据。一律写 Supersede 的旧行为让概率承诺被顶替后白拿直呼好感、注入哑火。
#[test]
fn a_takeover_inherits_the_original_trigger() {
    for origin in [
        TriggerKind::Probability,
        TriggerKind::Direct,
        TriggerKind::Moderation,
        TriggerKind::Continuation,
        TriggerKind::Supersede,
    ] {
        assert_eq!(
            select_trigger(false, false, Some(origin), true, true),
            Some(origin)
        );
    }
    // 不继承时维持原有的续聊/概率次序。
    assert_eq!(
        select_trigger(false, false, None, true, true),
        Some(TriggerKind::Continuation)
    );
    assert_eq!(
        select_trigger(false, false, None, false, true),
        Some(TriggerKind::Probability)
    );
    assert_eq!(select_trigger(false, false, None, false, false), None);
}

#[test]
fn direct_trigger_judgement_respects_takeover_and_privileged_bypass() {
    let mut settings = RealContextPluginSettings::default();
    settings.takeover_direct_trigger_enable = false;
    assert!(!active_judgement_allowed(&settings, true, false, false));
    assert!(active_judgement_allowed(&settings, false, false, false));

    settings.takeover_direct_trigger_enable = true;
    assert!(active_judgement_allowed(&settings, true, false, false));
    assert!(!active_judgement_allowed(&settings, true, true, false));

    settings.privileged_direct_trigger_skip_active_judgement = false;
    assert!(active_judgement_allowed(&settings, true, true, false));
    assert!(!active_judgement_allowed(&settings, false, false, true));
    assert!(!active_judgement_allowed(&settings, true, true, true));
}

#[test]
fn skipped_social_judgement_preserves_moderation_only_trigger() {
    assert_eq!(
        select_trigger_for_policy(false, true, true, Some(TriggerKind::Supersede), true, true),
        Some(TriggerKind::Moderation)
    );
    assert_eq!(
        select_trigger_for_policy(true, true, true, Some(TriggerKind::Supersede), true, true),
        Some(TriggerKind::Direct)
    );
}

#[test]
fn continuation_window_is_inclusive_at_its_boundary() {
    let settings = RealContextPluginSettings::default();
    assert_eq!(settings.continuation_window_seconds, 15);
    let window = Duration::from_secs(settings.continuation_window_seconds);
    let started = Instant::now();
    let mut session = SessionRuntime::new(started);
    session.mark_continuation("30000", started, &settings);

    assert!(session.continuation_match("30000", started + window, true));
    assert!(!session.continuation_match("30000", started + window + Duration::from_nanos(1), true,));
}

#[test]
fn replying_inside_the_window_keeps_extending_it() {
    // The turn cap used to end a continuation after a few exchanges even
    // while the user kept talking; now only silence closes it.
    let settings = RealContextPluginSettings::default();
    let window = Duration::from_secs(settings.continuation_window_seconds);
    let mut now = Instant::now();
    let mut session = SessionRuntime::new(now);
    session.mark_continuation("30000", now, &settings);

    for _ in 0..10 {
        now += window - Duration::from_secs(1);
        assert!(
            session.continuation_match("30000", now, true),
            "the window should still be open"
        );
        // A reply landed inside the window: restart the clock.
        session.mark_continuation("30000", now, &settings);
    }

    // Silence past the window still closes it.
    assert!(!session.continuation_match("30000", now + window + Duration::from_secs(1), true));
}

#[test]
fn a_different_speaker_does_not_inherit_the_window() {
    let settings = RealContextPluginSettings::default();
    let started = Instant::now();
    let mut session = SessionRuntime::new(started);
    session.mark_continuation("30000", started, &settings);
    assert!(!session.continuation_match("40000", started, true));
}

#[tokio::test]
async fn direct_trigger_bypass_adds_and_cleans_up_the_waiting_reaction() {
    let reactions = Arc::new(Mutex::new(Vec::new()));
    let (_temp, context) = test_context(Arc::new(ReactionAdapter {
        reactions: reactions.clone(),
    }));
    let plugin = RealContextPlugin::new();
    let event = inbound_event();
    // The bypass path under test requires takeover to stay off.
    let mut settings = RealContextPluginSettings::default();
    settings.takeover_direct_trigger_enable = false;
    let mut decision = TriggerDecision {
        should_reply: true,
        content: event.text.clone(),
        response_target: None,
    };

    plugin
        .decide_group_trigger(&context, &event, &mut decision, &settings)
        .await
        .unwrap();
    assert!(decision.should_reply);
    assert_eq!(
        reactions.lock().unwrap().as_slice(),
        &[(
            event.message_id.clone(),
            settings.active_reply_reaction_emoji_ids[0].to_string(),
            true,
        )]
    );

    plugin.after_turn_aborted(&context).await.unwrap();
    assert_eq!(reactions.lock().unwrap().last().unwrap().2, false);
}

#[tokio::test]
async fn correction_within_window_supersedes_committed_reply_and_moves_reactions() {
    let recorded = Arc::new(Mutex::new(Vec::new()));
    let (_temp, context) = test_context(Arc::new(ReactionAdapter {
        reactions: recorded.clone(),
    }));
    let plugin = RealContextPlugin::new();
    let settings = RealContextPluginSettings::default();
    let first = inbound_event();
    // 已承诺的回复(直触发或判断已通过),表情挂在旧消息上
    plugin.register_committed_pending(
        &runtime_session_key(&context),
        &first.sender_id,
        TriggerKind::Direct,
        vec![("message-1".to_string(), "289".to_string())],
        vec![active_reply_target(&first)],
    );
    // 补救窗口内同发送者的新消息:不再判断,直接顶替
    let mut correction = inbound_event();
    correction.message_id = "message-2".to_string();
    correction.text = "说错了,是另一件事".to_string();
    let mut decision = TriggerDecision {
        should_reply: false,
        content: correction.text.clone(),
        response_target: None,
    };
    plugin
        .decide_group_trigger(&context, &correction, &mut decision, &settings)
        .await
        .unwrap();
    assert!(decision.should_reply, "承诺沿用,补救消息应直接回复");
    assert_eq!(
        context
            .plugin_value(TRIGGER_KEY)
            .and_then(|value| value.as_str().and_then(TriggerKind::parse)),
        Some(TriggerKind::Direct),
        "顶替回合应沿用原始触发标签"
    );
    let calls = recorded.lock().unwrap().clone();
    assert!(
        calls.contains(&("message-1".to_string(), "289".to_string(), false)),
        "旧消息的表情应被摘除: {calls:?}"
    );
    assert!(
        calls.contains(&("message-2".to_string(), "289".to_string(), true)),
        "新消息应贴上表情: {calls:?}"
    );
    // pending 已刷新:承诺保持、目标并入两条消息、原始触发随链传递
    let runtime = plugin.runtime.lock().unwrap();
    let pending = runtime
        .sessions
        .get(&runtime_session_key(&context))
        .and_then(|session| session.pending.get(&first.sender_id))
        .expect("补救后 pending 应保留以支持链式覆盖");
    assert!(pending.committed);
    assert_eq!(pending.trigger, TriggerKind::Direct);
    assert_eq!(pending.targets.len(), 2);
    assert_eq!(
        pending.reactions,
        vec![("message-2".to_string(), "289".to_string())]
    );
}

/// 概率承诺被顶替:标签保持 Probability,`<qq-join-in>` 注入照发。
///
/// 08-31 主排查:泄漏主力正是覆盖回合——判官放行的概率承诺一被同发送者的
/// 快发消息顶替,标签就被改写成 Supersede,注入哑火,她面对一条明显不是说给
/// 自己的消息、又没有任何"为什么叫你"的说明,只好把「没@我就先旁听」说出口。
#[tokio::test]
async fn takeover_of_a_probability_commitment_keeps_the_join_in_notice() {
    let (_temp, context) = test_context(Arc::new(ReactionAdapter {
        reactions: Arc::new(Mutex::new(Vec::new())),
    }));
    let plugin = RealContextPlugin::new();
    let settings = RealContextPluginSettings::default();
    let first = inbound_event();
    plugin.register_committed_pending(
        &runtime_session_key(&context),
        &first.sender_id,
        TriggerKind::Probability,
        Vec::new(),
        vec![active_reply_target(&first)],
    );
    let mut correction = inbound_event();
    correction.message_id = "message-2".to_string();
    correction.text = "接着刚才那句再补一条".to_string();
    let mut decision = TriggerDecision {
        should_reply: false,
        content: correction.text.clone(),
        response_target: None,
    };
    plugin
        .decide_group_trigger(&context, &correction, &mut decision, &settings)
        .await
        .unwrap();

    assert!(decision.should_reply);
    assert_eq!(
        context
            .plugin_value(TRIGGER_KEY)
            .and_then(|value| value.as_str().and_then(TriggerKind::parse)),
        Some(TriggerKind::Probability),
        "概率承诺被顶替后标签不得改写成 Supersede"
    );
    {
        let runtime = plugin.runtime.lock().unwrap();
        let pending = runtime
            .sessions
            .get(&runtime_session_key(&context))
            .and_then(|session| session.pending.get(&first.sender_id))
            .expect("顶替后 pending 应保留以支持链式覆盖");
        assert_eq!(pending.trigger, TriggerKind::Probability, "链式覆盖应传递原始触发");
    }

    let mut input = empty_turn_input();
    plugin
        .inject_context(&context, &mut input, &settings)
        .await
        .unwrap();
    assert!(
        input
            .turn_system_context
            .iter()
            .any(|block| block.starts_with("<qq-join-in>")),
        "顶替回合应照发 <qq-join-in> 注入"
    );
}

/// 判官还在判(未承诺)时,新消息不得沿用承诺免判直回。
///
/// 旧行为:`preempt_inbound` 不看 committed 就移植目标,回落路径把"判断中"
/// 当成"承诺已成立"——08-31 三条「没@我就先旁听」泄漏全是这么强起的。
/// 测试环境没有可用的判官模型,判官路会立刻失败并丢弃;免判路则会置
/// should_reply、写 TRIGGER_KEY、登记承诺——三个观测点足以区分两条路。
#[tokio::test]
async fn an_uncommitted_takeover_goes_back_to_the_judge_not_the_bypass() {
    let (_temp, context) = test_context(Arc::new(ReactionAdapter {
        reactions: Arc::new(Mutex::new(Vec::new())),
    }));
    let plugin = RealContextPlugin::new();
    let settings = RealContextPluginSettings::default();
    let first = inbound_event();
    let (cancel, _receiver) = tokio::sync::watch::channel(false);
    plugin
        .runtime
        .lock()
        .unwrap()
        .session_mut(&runtime_session_key(&context), Instant::now())
        .pending
        .insert(
            first.sender_id.clone(),
            PendingReply {
                generation: 7,
                started: Instant::now(),
                trigger: TriggerKind::Probability,
                committed: false,
                reactions: Vec::new(),
                targets: vec![active_reply_target(&first)],
                cancel,
            },
        );

    let mut followup = inbound_event();
    followup.message_id = "message-2".to_string();
    followup.text = "这句其实是说给别人的".to_string();

    // 未承诺的 pending 不该被接管:不移植目标,也就没有免判回落。
    assert!(!plugin.preempt_inbound(&context, &followup).unwrap());
    assert!(active_targets_from_context(&context).is_empty());

    let mut decision = TriggerDecision {
        should_reply: false,
        content: followup.text.clone(),
        response_target: None,
    };
    plugin
        .decide_group_trigger(&context, &followup, &mut decision, &settings)
        .await
        .unwrap();

    assert!(
        !decision.should_reply,
        "判官不可用时应丢弃,而不是免判直回"
    );
    assert!(
        context.plugin_value(TRIGGER_KEY).is_none(),
        "免判路才会写 TRIGGER_KEY"
    );
    let runtime = plugin.runtime.lock().unwrap();
    assert!(
        runtime
            .sessions
            .get(&runtime_session_key(&context))
            .and_then(|session| session.pending.get(&first.sender_id))
            .is_none(),
        "判官失败应丢弃 pending,免判路则会登记承诺"
    );
}

#[tokio::test]
async fn confirm_supersede_moves_reactions_and_restarts_the_window() {
    let recorded = Arc::new(Mutex::new(Vec::new()));
    let (_temp, context) = test_context(Arc::new(ReactionAdapter {
        reactions: recorded.clone(),
    }));
    let plugin = RealContextPlugin::new();
    let first = inbound_event();
    let (cancel, _receiver) = tokio::sync::watch::channel(false);
    let old_started = Instant::now() - Duration::from_secs(3);
    plugin
        .runtime
        .lock()
        .unwrap()
        .session_mut(&runtime_session_key(&context), Instant::now())
        .pending
        .insert(
            first.sender_id.clone(),
            PendingReply {
                generation: 7,
                started: old_started,
                trigger: TriggerKind::Direct,
                committed: true,
                reactions: vec![("message-1".to_string(), "289".to_string())],
                targets: vec![active_reply_target(&first)],
                cancel,
            },
        );
    let mut correction = inbound_event();
    correction.message_id = "message-2".to_string();
    plugin.confirm_supersede(&context, &correction).await;
    let calls = recorded.lock().unwrap().clone();
    assert!(calls.contains(&("message-1".to_string(), "289".to_string(), false)));
    assert!(calls.contains(&("message-2".to_string(), "289".to_string(), true)));
    let runtime = plugin.runtime.lock().unwrap();
    let pending = runtime
        .sessions
        .get(&runtime_session_key(&context))
        .and_then(|session| session.pending.get(&first.sender_id))
        .expect("覆盖后 pending 应保留");
    assert!(pending.started > old_started, "补救窗口应从新消息重新起算");
    assert_eq!(pending.targets.len(), 2);
    assert_eq!(
        pending.reactions,
        vec![("message-2".to_string(), "289".to_string())]
    );
}

#[tokio::test]
async fn direct_trigger_registers_a_committed_pending_for_correction() {
    let recorded = Arc::new(Mutex::new(Vec::new()));
    let (_temp, context) = test_context(Arc::new(ReactionAdapter {
        reactions: recorded.clone(),
    }));
    let plugin = RealContextPlugin::new();
    let settings = RealContextPluginSettings {
        takeover_direct_trigger_enable: false,
        ..RealContextPluginSettings::default()
    };
    let event = inbound_event();
    let mut decision = TriggerDecision {
        should_reply: true,
        content: event.text.clone(),
        response_target: None,
    };
    plugin
        .decide_group_trigger(&context, &event, &mut decision, &settings)
        .await
        .unwrap();
    assert!(decision.should_reply);
    let runtime = plugin.runtime.lock().unwrap();
    let pending = runtime
        .sessions
        .get(&runtime_session_key(&context))
        .and_then(|session| session.pending.get(&event.sender_id))
        .expect("直触发应登记可被补救的 pending");
    assert!(pending.committed);
    assert_eq!(
        pending.reactions,
        vec![("message-1".to_string(), "289".to_string())]
    );
}

#[tokio::test]
async fn muted_bot_suppresses_direct_group_trigger_while_unknown_fails_open() {
    let plugin = RealContextPlugin::new();
    // The availability check this test is about lives on the path taken
    // when active judgement is *not* running. `takeover_direct_trigger_enable`
    // defaults to true, which sends a direct trigger through the full
    // judgement flow instead — so with plain defaults the branch below is
    // never reached and the assertions pass or fail for unrelated reasons.
    let settings = RealContextPluginSettings {
        takeover_direct_trigger_enable: false,
        ..RealContextPluginSettings::default()
    };
    let event = inbound_event();
    let (_temp, muted_context) = availability_context(BotSendAvailability::Muted);
    let mut muted = TriggerDecision {
        should_reply: true,
        content: event.text.clone(),
        response_target: None,
    };
    plugin
        .decide_group_trigger(&muted_context, &event, &mut muted, &settings)
        .await
        .unwrap();
    assert!(!muted.should_reply);

    let probabilistic_settings = RealContextPluginSettings {
        active_judge_probability: 1.0,
        ..RealContextPluginSettings::default()
    };
    let mut probabilistic = TriggerDecision {
        should_reply: false,
        content: event.text.clone(),
        response_target: None,
    };
    plugin
        .decide_group_trigger(
            &muted_context,
            &event,
            &mut probabilistic,
            &probabilistic_settings,
        )
        .await
        .unwrap();
    assert!(!probabilistic.should_reply);

    let (_temp, unknown_context) = availability_context(BotSendAvailability::Unknown);
    let mut unknown = TriggerDecision {
        should_reply: true,
        content: event.text.clone(),
        response_target: None,
    };
    plugin
        .decide_group_trigger(&unknown_context, &event, &mut unknown, &settings)
        .await
        .unwrap();
    assert!(unknown.should_reply);
}

#[tokio::test]
async fn supersede_signal_wakes_an_inflight_judgement() {
    let (sender, mut receiver) = tokio::sync::watch::channel(false);
    let waiter = tokio::spawn(async move {
        wait_for_supersede(&mut receiver).await;
    });
    sender.send_replace(true);
    tokio::time::timeout(Duration::from_secs(1), waiter)
        .await
        .unwrap()
        .unwrap();
}

#[test]
fn directly_triggered_image_is_a_primary_target() {
    let (_temp, context) = availability_context(BotSendAvailability::Available);
    let mut current = inbound_event();
    current.text.clear();
    current.media.push(crate::platforms::PlatformInboundMedia {
        kind: PlatformMediaKind::Image,
        id: Some("image-1".to_string()),
        name: None,
        url: None,
    });

    let prompt = active_target_prompt(&context, &current, "（对方发送了 1 张图片）");

    // 署名行版:纯图消息的正文槽位是占位文案,坐标信息照带。
    assert!(prompt.starts_with("[New messages received this turn]\n["));
    let head = prompt.lines().nth(1).unwrap();
    assert!(head.contains("（对方发送了 1 张图片）"), "{head}");
    assert!(!prompt.contains("无明确文字目标消息"));
    assert!(!prompt.contains("同一用户随后发送的补充材料"));
}

/// 限额耗尽时直触发照样回复,但整段主动判断被跳过。这条出口过去不写
/// TRIGGER_KEY,下游拿不到唤醒理由——08-29 排查时注入块因此整条哑火,
/// 而日志里一个字都没有,只能靠"回复出现了却没有判官决定"倒推,推错了
/// 一次。现在既写 trigger 也留痕。
#[tokio::test]
async fn an_exhausted_reply_quota_still_records_the_trigger() {
    let (_temp, context) = availability_context(BotSendAvailability::Available);
    context.set_reply_rate_available(false);
    let event = inbound_event();
    let mut decision = TriggerDecision {
        should_reply: true,
        content: event.text.clone(),
        response_target: None,
    };

    RealContextPlugin::new()
        .decide_group_trigger(
            &context,
            &event,
            &mut decision,
            &RealContextPluginSettings::default(),
        )
        .await
        .unwrap();

    assert!(decision.should_reply, "限额耗尽不该吃掉直接触发的回复");
    assert_eq!(
        context
            .plugin_value(TRIGGER_KEY)
            .and_then(|value| value.as_str().and_then(TriggerKind::parse)),
        Some(TriggerKind::Direct),
    );
}

/// 只有"概率抽中且判官放行"的回合才注入,其余触发一律不给。
///
/// `TRIGGER_KEY == Probability` 只在判官通过后才写,或从这样一次放行的承诺
/// 覆盖继承而来(覆盖保留原始触发)——两种来源都满足"该不该回已经判过了,
/// 答案是回"。裸的 Supersede 只剩"原始触发不可考"的回退一种来源,不注入。
#[tokio::test]
async fn only_a_judge_approved_probability_turn_gets_the_join_in_notice() {
    for (trigger, wants) in [
        (TriggerKind::Probability, true),
        (TriggerKind::Continuation, false),
        (TriggerKind::Supersede, false),
        (TriggerKind::Direct, false),
        (TriggerKind::Moderation, false),
    ] {
        let (_temp, context) = availability_context(BotSendAvailability::Available);
        context.set_plugin_value(TRIGGER_KEY, Value::String(trigger.as_str().to_string()));
        let mut input = empty_turn_input();
        RealContextPlugin::new()
            .inject_context(&context, &mut input, &RealContextPluginSettings::default())
            .await
            .unwrap();

        assert_eq!(
            input
                .turn_system_context
                .iter()
                .any(|block| block.starts_with("<qq-join-in>")),
            wants,
            "{trigger:?} 的注入有无不符预期"
        );
    }
}

/// 措辞必须是让步句:承认没艾特,紧接着要求照样回。
///
/// 08-29 第一版把"没被点名"作为孤立事实单独摆出来
/// (`Nobody called you this turn`),让她自己得结论——实测她原话回
/// 「没被艾特不接（笑）」。差别不在提不提,在提完之后有没有把结论堵死。
#[test]
fn the_join_in_notice_concedes_and_then_overrides() {
    let notice = probability_reply_notice(TriggerKind::Probability).unwrap();
    assert!(
        notice.contains("Even though this message does not @-mention you"),
        "缺少让步:{notice}"
    );
    assert!(
        notice.contains("still reply with one message"),
        "让步之后必须紧跟要求:{notice}"
    );
    assert!(
        notice.contains("the mood of the room"),
        "要求必须落在读空气上:{notice}"
    );
    assert!(
        notice.is_ascii(),
        "模型可见面恒英文,与其它 <qq-*> 块一致:{notice}"
    );
}
