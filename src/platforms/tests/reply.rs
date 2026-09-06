//! 回复整形、分段与投递抑制。

use super::shared::*;
use crate::platforms::*;
use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

/// Regression: an auto-attached reply image delivered in one turn must
/// stay suppressed for the recovery turn that follows an interrupted
/// send — that replay is what duplicated pictures in QQ groups.
#[test]
fn delivered_images_stay_deduplicated_across_turns_per_conversation() {
    let first = blake3::hash(b"generated-picture");
    let second = blake3::hash(b"another-picture");
    let scope = "onebot:1:group:duplicate-image-regression";
    let other_scope = "onebot:1:group:unrelated";

    assert!(recent_conversation_images(scope).is_empty());
    record_recent_conversation_images(scope, &[first]);
    assert_eq!(recent_conversation_images(scope), vec![first]);
    // Other conversations are unaffected.
    assert!(recent_conversation_images(other_scope).is_empty());

    // Re-recording keeps one entry per digest.
    record_recent_conversation_images(scope, &[first, second]);
    let mut seen = recent_conversation_images(scope);
    seen.sort_by_key(|digest| digest.as_bytes().to_vec());
    let mut expected = vec![first, second];
    expected.sort_by_key(|digest| digest.as_bytes().to_vec());
    assert_eq!(seen, expected);
}

#[test]
fn parenthetical_only_filter_ignores_mentions_but_preserves_real_content() {
    let filtered = OutboundMessage::segments(
        OutboundOrigin::FinalReply,
        vec![
            OutboundSegment::Mention("123".to_string()),
            OutboundSegment::Text("  （这个消息与我无关）  ".to_string()),
        ],
    );
    assert!(message_is_parenthetical_only(&filtered));

    let nested = OutboundMessage::text(OutboundOrigin::FinalReply, "（外层（说明））");
    assert!(message_is_parenthetical_only(&nested));
    let two = OutboundMessage::text(OutboundOrigin::FinalReply, "（动作）（说明）");
    assert!(!message_is_parenthetical_only(&two));
    let sentence = OutboundMessage::text(OutboundOrigin::FinalReply, "你好（说明）");
    assert!(!message_is_parenthetical_only(&sentence));
    let media = OutboundMessage::segments(
        OutboundOrigin::FinalReply,
        vec![
            OutboundSegment::Text("（图片）".to_string()),
            OutboundSegment::ImageBytes {
                mime: "image/png".to_string(),
                data: Arc::from([1_u8]),
                alt: String::new(),
            },
        ],
    );
    assert!(!message_is_parenthetical_only(&media));
}

#[tokio::test]
async fn parenthetical_only_model_reply_never_reaches_the_adapter() {
    let (_temp, context, adapter) = test_turn_context(false);
    context
        .send(OutboundMessage::segments(
            OutboundOrigin::FinalReply,
            vec![
                OutboundSegment::Mention("123".to_string()),
                OutboundSegment::Text("（无视）".to_string()),
            ],
        ))
        .await
        .unwrap();
    assert_eq!(adapter.calls.load(AtomicOrdering::Relaxed), 0);

    context
        .send(OutboundMessage::text(
            OutboundOrigin::FinalReply,
            "正常回复（补充）",
        ))
        .await
        .unwrap();
    assert_eq!(adapter.calls.load(AtomicOrdering::Relaxed), 1);
}

#[tokio::test]
async fn intermediate_flush_sends_round_text_once() {
    let (_temp, context, adapter) = test_turn_context(false);
    let suppression = ReplySuppression::default();
    flush_intermediate_reply(&context, "第一轮的说明。", &suppression).await;
    let messages = adapter.messages.lock().unwrap();
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].origin, OutboundOrigin::IntermediateReply);
    let OutboundBody::Segments(segments) = &messages[0].body else {
        panic!("intermediate reply must be a normal message");
    };
    assert!(matches!(
        segments.as_slice(),
        [OutboundSegment::Markdown(text)] if text == "第一轮的说明。"
    ));
}

#[tokio::test]
async fn intermediate_flush_skips_empty_and_cuts_direct_send_ranges() {
    let (_temp, context, adapter) = test_turn_context(false);

    // Nothing to say: no message goes out. A lone zero-width space (what the
    // model emits when it has nothing to add after a voice message) counts
    // as nothing too — 09-06 it went out as an empty QQ bubble.
    flush_intermediate_reply(&context, "   ", &ReplySuppression::default()).await;
    flush_intermediate_reply(&context, "\u{200B}", &ReplySuppression::default()).await;
    assert!(adapter.messages.lock().unwrap().is_empty());
    assert!(visibly_blank("\u{200B}\u{FEFF} \n"));
    assert!(!visibly_blank("👨\u{200D}👩"));
    assert!(!visibly_blank("好"));

    // The model continuation after a confirmed direct tool send is
    // suppressed, so only the part before the send is flushed.
    let text = "前半部分。已被工具直发的确认。";
    let mut suppression = ReplySuppression::default();
    suppression.direct_send_succeeded("前半部分。".len());
    flush_intermediate_reply(&context, text, &suppression).await;
    let messages = adapter.messages.lock().unwrap();
    assert_eq!(messages.len(), 1);
    let OutboundBody::Segments(segments) = &messages[0].body else {
        panic!("intermediate reply must be a normal message");
    };
    assert!(matches!(
        segments.as_slice(),
        [OutboundSegment::Markdown(text)] if text == "前半部分。"
    ));
}

#[test]
fn markdown_to_plain_strips_decoration_keeps_content() {
    let input = "# 标题\n\n**加粗** 与 `代码` 和 [链接](https://a.b)\n\n```rust\nlet x = 1; // **不动**\n```\n\n- 列表项\n> 引用";
    let plain = markdown_to_plain(input);
    assert_eq!(
        plain,
        "标题\n\n加粗 与 代码 和 链接 (https://a.b)\n\nlet x = 1; // **不动**\n\n- 列表项\n引用"
    );
}

#[test]
fn markdown_link_edge_cases() {
    assert_eq!(strip_inline_markup("[a](b"), "[a](b");
    assert_eq!(strip_inline_markup("纯 [文本] 括号"), "纯 [文本] 括号");
    // Identical text/url collapses to one copy.
    assert_eq!(
        strip_inline_markup("[https://x.y](https://x.y)"),
        "https://x.y"
    );
}

#[test]
fn split_reply_paragraph_line_and_hard_boundaries() {
    assert_eq!(split_reply("短", 10), vec!["短"]);
    assert!(split_reply("  ", 10).is_empty());
    // 0 disables splitting.
    let long = "a".repeat(50);
    assert_eq!(split_reply(&long, 0), vec![long.clone()]);

    let text = "第一段落。\n\n第二段落。";
    let chunks = split_reply(text, 6);
    assert_eq!(chunks, vec!["第一段落。", "第二段落。"]);

    // CJK hard split never panics and keeps every char.
    let cjk = "汉".repeat(25);
    let chunks = split_reply(&cjk, 10);
    assert_eq!(chunks.len(), 3);
    assert_eq!(chunks.join(""), cjk);
}

#[test]
fn sniff_image_mime_by_magic() {
    assert_eq!(sniff_image_mime(&[0x89, b'P', b'N', b'G', 0]), "image/png");
    assert_eq!(sniff_image_mime(&[0xFF, 0xD8, 0xFF, 0xE0]), "image/jpeg");
    assert_eq!(sniff_image_mime(b"GIF89a"), "image/gif");
    assert_eq!(sniff_image_mime(b"RIFF\0\0\0\0WEBPVP8 "), "image/webp");
    assert_eq!(sniff_image_mime(b"????"), "image/png");
}

#[test]
fn adaptive_response_target_uses_independent_inclusive_boundaries() {
    let now = Instant::now();
    let start = PlatformMessagePosition {
        total_messages: 10,
        sender_messages: 2,
    };
    let target = ResponseTarget {
        message_id: "message-1".to_string(),
        user_id: "alice".to_string(),
        quote: true,
        mention: true,
        explicit_mention_user_ids: Vec::new(),
    };
    let policy = AdaptiveResponseTargetPolicy::new(Some(start), now, 5, 15);

    let before_both = policy.resolve(
        target.clone(),
        Some(PlatformMessagePosition {
            total_messages: 15,
            sender_messages: 3,
        }),
        false,
        now + Duration::from_secs(14),
    );
    assert!(before_both.is_none());

    let quote_only = policy
        .resolve(
            target.clone(),
            Some(PlatformMessagePosition {
                total_messages: 15,
                sender_messages: 2,
            }),
            false,
            now + Duration::from_secs(14),
        )
        .unwrap();
    assert!(quote_only.quote);
    assert!(!quote_only.mention);

    let mention_only = policy
        .resolve(
            target.clone(),
            Some(PlatformMessagePosition {
                total_messages: 15,
                sender_messages: 3,
            }),
            false,
            now + Duration::from_secs(15),
        )
        .unwrap();
    assert!(!mention_only.quote);
    assert!(mention_only.mention);

    let both = policy
        .resolve(
            target,
            Some(PlatformMessagePosition {
                total_messages: 15,
                sender_messages: 2,
            }),
            false,
            now + Duration::from_secs(15),
        )
        .unwrap();
    assert!(both.quote);
    assert!(both.mention);
}

#[test]
fn adaptive_response_target_mention_uses_known_message_activity() {
    let now = Instant::now();
    let start = PlatformMessagePosition {
        total_messages: 10,
        sender_messages: 2,
    };
    let target = ResponseTarget {
        message_id: "message-1".to_string(),
        user_id: "alice".to_string(),
        quote: false,
        mention: true,
        explicit_mention_user_ids: Vec::new(),
    };
    let policy = AdaptiveResponseTargetPolicy::new(Some(start), now, 5, 15);
    let same_sender_message = PlatformMessagePosition {
        total_messages: 11,
        sender_messages: 3,
    };
    let other_sender_message = PlatformMessagePosition {
        total_messages: 11,
        sender_messages: 2,
    };
    let cases = [
        ("before threshold without messages", Some(start), 14, false),
        ("at threshold without messages", Some(start), 15, false),
        (
            "at threshold after same sender",
            Some(same_sender_message),
            15,
            false,
        ),
        (
            "before threshold after other sender",
            Some(other_sender_message),
            14,
            false,
        ),
        (
            "at threshold after other sender",
            Some(other_sender_message),
            15,
            true,
        ),
        ("before threshold with unknown activity", None, 14, false),
        ("at threshold with unknown activity", None, 15, true),
    ];

    for (case, current, elapsed_seconds, expected) in cases {
        let mention = policy
            .resolve(
                target.clone(),
                current,
                false,
                now + Duration::from_secs(elapsed_seconds),
            )
            .is_some_and(|target| target.mention);
        assert_eq!(mention, expected, "{case}");
    }
}

#[tokio::test]
async fn direct_final_suppression_requires_primary_send_success() {
    let (_temp, success, _adapter) = test_turn_context(false);
    success
        .send(OutboundMessage::text(OutboundOrigin::Tool, "sent"))
        .await
        .unwrap();
    assert!(success.take_final_reply_suppression());
    assert!(!success.take_final_reply_suppression());

    let (_temp, fallback, _adapter) = test_turn_context(true);
    fallback
        .send(OutboundMessage::text(OutboundOrigin::Tool, "fallback"))
        .await
        .unwrap();
    assert!(!fallback.take_final_reply_suppression());
}

#[tokio::test]
async fn delivery_ledger_records_only_confirmed_images() {
    let bytes: Arc<[u8]> = Arc::from([1_u8, 2, 3]);
    let digest = blake3::hash(&bytes);
    let image_message = || {
        OutboundMessage::segments(
            OutboundOrigin::Tool,
            vec![OutboundSegment::ImageBytes {
                mime: "image/png".to_string(),
                data: bytes.clone(),
                alt: String::new(),
            }],
        )
    };

    let (_temp, success, _adapter) = test_turn_context(false);
    success.send(image_message()).await.unwrap();
    assert!(success.delivered_image_digests().contains(&digest));

    let (_temp, mut failed, _adapter) = test_turn_context(true);
    failed.plugins = Arc::new(plugins::PlatformPluginRegistry::default());
    assert!(failed.send(image_message()).await.is_err());
    assert!(failed.delivered_image_digests().is_empty());
}

#[tokio::test]
async fn partial_delivery_is_recorded_without_sending_a_full_fallback() {
    let temp = tempfile::tempdir().unwrap();
    let paths = test_paths(temp.path());
    let digest = blake3::hash(&[1_u8, 2, 3]);
    let adapter = Arc::new(PartialFailureAdapter {
        calls: AtomicUsize::new(0),
        digest,
        response_target_delivered: false,
    });
    let context = PlatformTurnContext::new(
        PlatformConversation {
            platform: "onebot".to_string(),
            account_id: "10000".to_string(),
            kind: ConversationKind::Private,
            conversation_id: "20000".to_string(),
        },
        "20000".to_string(),
        "tester".to_string(),
        false,
        AppConfig::default(),
        paths.clone(),
        StateStore::new(&paths).unwrap(),
        adapter.clone(),
        Arc::new(plugins::PlatformPluginRegistry::new(vec![Arc::new(
            SuppressingToolPlugin,
        )])),
    );

    let result = context
        .send(OutboundMessage::segments(
            OutboundOrigin::Tool,
            vec![OutboundSegment::ImageBytes {
                mime: "image/png".to_string(),
                data: Arc::from([1_u8, 2, 3]),
                alt: String::new(),
            }],
        ))
        .await;

    assert!(result.is_err());
    assert_eq!(adapter.calls.load(AtomicOrdering::Relaxed), 1);
    assert!(context.delivered_image_digests().contains(&digest));
}

#[tokio::test]
async fn response_target_is_consumed_once_and_survives_primary_fallback() {
    let (_temp, context, adapter) = test_turn_context(true);
    let target = ResponseTarget {
        message_id: "message-9".to_string(),
        user_id: "user-4".to_string(),
        quote: true,
        mention: true,
        explicit_mention_user_ids: Vec::new(),
    };
    context.set_response_target(Some(target.clone()));

    context
        .send(OutboundMessage::text(OutboundOrigin::Tool, "first"))
        .await
        .unwrap();
    context
        .send(OutboundMessage::text(OutboundOrigin::FinalReply, "second"))
        .await
        .unwrap();

    let messages = adapter.messages.lock().unwrap();
    assert_eq!(messages.len(), 3);
    assert_eq!(messages[0].response_target, Some(target.clone()));
    assert_eq!(messages[1].response_target, Some(target));
    assert_eq!(messages[2].response_target, None);
    assert_eq!(context.response_target(), None);
}

#[tokio::test]
async fn partially_delivered_response_target_is_not_restored() {
    let temp = tempfile::tempdir().unwrap();
    let paths = test_paths(temp.path());
    let adapter = Arc::new(PartialFailureAdapter {
        calls: AtomicUsize::new(0),
        digest: blake3::hash(&[1_u8]),
        response_target_delivered: true,
    });
    let context = PlatformTurnContext::new(
        PlatformConversation {
            platform: "onebot".to_string(),
            account_id: "10000".to_string(),
            kind: ConversationKind::Group,
            conversation_id: "20000".to_string(),
        },
        "20000".to_string(),
        "tester".to_string(),
        false,
        AppConfig::default(),
        paths.clone(),
        StateStore::new(&paths).unwrap(),
        adapter,
        Arc::new(plugins::PlatformPluginRegistry::default()),
    );
    context.set_explicit_response_mentions(vec!["30000".to_string()]);

    assert!(context
        .send(OutboundMessage::text(OutboundOrigin::FinalReply, "first"))
        .await
        .is_err());
    assert!(context.response_target().is_none());
}

#[test]
fn failed_older_send_merges_mentions_into_a_newer_response_target() {
    let (_temp, context, _adapter) = test_turn_context(false);
    context.set_explicit_response_mentions(vec!["30000".to_string()]);
    let reserved = context
        .response_target
        .lock()
        .unwrap()
        .take()
        .expect("explicit target exists");
    context.set_adaptive_response_target(
        Some(ResponseTarget {
            message_id: "message-2".to_string(),
            user_id: "20000".to_string(),
            quote: true,
            mention: true,
            explicit_mention_user_ids: Vec::new(),
        }),
        AdaptiveResponseTargetPolicy::new(None, Instant::now(), 1, 1),
    );

    context.restore_response_target(reserved);

    assert_eq!(
        context.response_target(),
        Some(ResponseTarget {
            message_id: "message-2".to_string(),
            user_id: "20000".to_string(),
            quote: true,
            mention: false,
            explicit_mention_user_ids: vec!["30000".to_string()],
        })
    );
}

#[tokio::test]
async fn adaptive_response_target_is_identical_on_primary_and_fallback() {
    let (_temp, mut context, adapter) = test_turn_context(true);
    let registry = MessageActivityRegistry::default();
    let (activity, start, _) = registry.observe("onebot:1:group:2", "m1", "alice", Instant::now());
    for index in 0..5 {
        registry.observe(
            "onebot:1:group:2",
            &format!("other-{index}"),
            "bob",
            Instant::now(),
        );
    }
    context.message_activity = Some(activity);
    let target = ResponseTarget {
        message_id: "m1".to_string(),
        user_id: "alice".to_string(),
        quote: true,
        mention: true,
        explicit_mention_user_ids: Vec::new(),
    };
    context.set_adaptive_response_target(
        Some(target.clone()),
        AdaptiveResponseTargetPolicy::new(
            Some(start),
            Instant::now().checked_sub(Duration::from_secs(15)).unwrap(),
            5,
            15,
        ),
    );
    // The OneBot trigger pipeline writes the final static decision after
    // the plugin has selected its adaptive policy; the matching target
    // must not discard that policy.
    context.set_response_target(Some(target.clone()));

    context
        .send(OutboundMessage::text(OutboundOrigin::Tool, "answer"))
        .await
        .unwrap();

    let messages = adapter.messages.lock().unwrap();
    assert_eq!(messages.len(), 2);
    assert_eq!(messages[0].response_target, Some(target.clone()));
    assert_eq!(messages[1].response_target, Some(target));
}

#[test]
fn direct_send_without_later_prompt_covers_an_empty_final_reply() {
    let mut suppression = ReplySuppression::default();
    suppression.direct_send_succeeded(8);
    let (ranges, already_sent) = suppression.finish(8);
    assert!(ranges.is_empty());
    assert!(already_sent);
}

#[test]
fn model_round_boundary_keeps_only_the_latest_visible_text() {
    let mut text = String::new();
    let mut suppression = ReplySuppression::default();

    start_model_reply(&mut text, &mut suppression);
    text.push_str("text before tool");
    start_model_reply(&mut text, &mut suppression);
    text.push_str("final tool follow-up");

    assert_eq!(text, "final tool follow-up");
    assert_eq!(suppression.finish(text.len()), (Vec::new(), false));
}

#[test]
fn ordinary_single_round_reply_is_unchanged() {
    let mut text = String::new();
    let mut suppression = ReplySuppression::default();

    start_model_reply(&mut text, &mut suppression);
    text.push_str("ordinary single round");

    assert_eq!(text, "ordinary single round");
    assert_eq!(suppression.finish(text.len()), (Vec::new(), false));
}

#[test]
fn direct_send_suppresses_the_next_model_round() {
    let mut text = String::new();
    let mut suppression = ReplySuppression::default();
    start_model_reply(&mut text, &mut suppression);
    text.push_str("text before tool");
    suppression.direct_send_succeeded(text.len());

    start_model_reply(&mut text, &mut suppression);
    text.push_str("duplicate confirmation");
    let (ranges, already_sent) = suppression.finish(text.len());

    assert_eq!(ranges, vec![(0, text.len())]);
    assert!(already_sent);
}

#[test]
fn queued_followup_resets_prior_direct_send_suppression() {
    let mut text = String::new();
    let mut suppression = ReplySuppression::default();
    start_model_reply(&mut text, &mut suppression);
    suppression.direct_send_succeeded(0);
    start_model_reply(&mut text, &mut suppression);
    text.push_str("reply before queued follow-up");
    suppression.queued_prompt_consumed();

    start_model_reply(&mut text, &mut suppression);
    text.push_str("queued follow-up answer");

    assert_eq!(text, "queued follow-up answer");
    assert_eq!(suppression.finish(text.len()), (Vec::new(), false));
}

/// 文字投递幂等闸(08-22 复读取证):同回合内同义改写变体要被拦,
/// 不同话题放行,短文本不设闸。
#[test]
fn reply_text_gate_blocks_paraphrase_variants_only() {
    let temp = tempfile::tempdir().unwrap();
    let paths = test_paths(temp.path());
    let context = PlatformTurnContext::new(
        PlatformConversation {
            platform: "onebot".to_string(),
            account_id: "10000".to_string(),
            kind: ConversationKind::Group,
            conversation_id: "130515298".to_string(),
        },
        "20000".to_string(),
        "tester".to_string(),
        false,
        AppConfig::default(),
        paths.clone(),
        StateStore::new(&paths).unwrap(),
        Arc::new(CountingAdapter {
            calls: AtomicUsize::new(0),
            fail_first: false,
            messages: Mutex::new(Vec::new()),
            group_members: Vec::new(),
        }),
        Arc::new(plugins::PlatformPluginRegistry::new(Vec::new())),
    );

    // 截图同款:两段同义改写变体。
    let first = "怼得好，这种批量灌水issue的就该治。重复提交的可以直接标duplicate关掉，信息不全的按模板要求补齐，不补就关。GitHub的issue不是许愿池，维护者的精力应该花在解决问题上而不是猜谜上。要是这人持续骚扰，还可以在仓库里加个行为规范说明，屡教不改的直接拉黑禁言issue区。";
    let variant = "怼得好，这种批量灌水issue的就该治。重复提交的直接标duplicate关掉，信息不全的按模板要求补齐，不补就关。GitHub的issue不是许愿池，维护者精力应该花在解决问题而不是猜谜上。要是这人持续骚扰，仓库里加个行为规范说明，屡教不改直接拉黑禁言issue区。";
    let unrelated = "版本号不按你的命名规范来，说明他压根没装过你这个插件，或者装的是别人魔改的二道贩子版本，连issue都提错仓库了。直接在issue里回一句该版本号不属于本插件，请确认你使用的来源，等他自证。";

    assert!(!context.is_duplicate_reply_text(first), "首发不该被拦");
    context.record_delivered_reply_text(first);
    assert!(
        context.is_duplicate_reply_text(variant),
        "同义改写变体必须命中幂等闸"
    );
    assert!(
        !context.is_duplicate_reply_text(unrelated),
        "不同话题的回复不该被拦"
    );
    context.record_delivered_reply_text(unrelated);
    assert!(
        !context.is_duplicate_reply_text("好，收到。"),
        "短文本不设闸"
    );
}

/// 工具模板泄漏剥离(08-23 取证形态):完整 <tool_call> 块、缺闭合的流式
/// 截断残模板、裸 <function=> span 全剥;夹在正常文字中间只剥模板保留文
/// 字;剥空+纯 mention = 无可投递内容;干净文本一个字节不动。
#[test]
fn tool_call_template_leaks_are_stripped_from_outgoing_replies() {
    let leak = "@雾雨凝霜 <tool_call>\n<function=web_search>\n<parameter=query>debian nvidia</parameter>\n</function>\n</tool_call>";
    let mut message = OutboundMessage::segments(
        OutboundOrigin::FinalReply,
        vec![
            OutboundSegment::Mention("123".to_string()),
            OutboundSegment::Markdown(leak.to_string()),
        ],
    );
    assert!(strip_tool_call_leaks(&mut message));
    // "@雾雨凝霜 " 是模型正文里的裸文本而非 Mention 段,剥掉模板后只剩它
    // 加空白;这里它随段保留,但真实泄漏消息里正文只有模板 → 段被移除。
    let mut pure = OutboundMessage::segments(
        OutboundOrigin::FinalReply,
        vec![
            OutboundSegment::Mention("123".to_string()),
            OutboundSegment::Markdown(
                "<tool_call>\n<function=vision_analyze>\n<parameter=image>/tmp/a.png</parameter>\n</function>\n</tool_call>".to_string(),
            ),
        ],
    );
    assert!(strip_tool_call_leaks(&mut pure));
    assert!(!message_has_deliverable_content(&pure));

    // 流式截断:没有闭合标签,剥到结尾,保留前面的真实回复。
    let mut truncated = OutboundMessage::text(
        OutboundOrigin::FinalReply,
        "先说结论:开源内核模块就行。<tool_call>\n<function=web_search>\n<parameter=query>又来",
    );
    assert!(strip_tool_call_leaks(&mut truncated));
    assert!(message_has_deliverable_content(&truncated));
    let OutboundBody::Segments(segments) = &truncated.body else {
        panic!("expected segments")
    };
    let OutboundSegment::Text(text) = &segments[0] else {
        panic!("expected text")
    };
    assert_eq!(text, "先说结论:开源内核模块就行。");

    // 干净文本零改动。
    let mut clean = OutboundMessage::text(OutboundOrigin::FinalReply, "正常回复,不含模板。");
    assert!(!strip_tool_call_leaks(&mut clean));
}

/// 活回合登记(08-26,MCP 桥拿平台工具的钥匙):守卫在场能取到上下文,
/// 掉落即取不到;并行回合下后来者覆盖登记,旧守卫掉落不误删新的。
#[test]
fn live_turn_guard_scopes_the_platform_context() {
    use crate::platforms::{live_turn_context, LiveTurnGuard};
    let (_temp, context, _adapter) = test_turn_context(true);
    let context = std::sync::Arc::new(context);
    assert!(live_turn_context("sess-live").is_none());
    let guard = LiveTurnGuard::register("sess-live", &context);
    assert!(live_turn_context("sess-live").is_some());

    let (_temp2, second, _adapter2) = test_turn_context(true);
    let second = std::sync::Arc::new(second);
    let newer = LiveTurnGuard::register("sess-live", &second);
    drop(guard);
    assert!(
        live_turn_context("sess-live").is_some(),
        "旧守卫掉落不应摘掉后来者的登记"
    );
    drop(newer);
    assert!(live_turn_context("sess-live").is_none());
}

/// 连发时要靠引用分流。艾特要求"隔了时间**且**别人说过话",引用要求
/// "隔了几条别人的消息";她回完 A 立刻回 B,两个条件都不满足,于是两条
/// 定向线索一起哑火——而上一条还是她自己的,读的人分不清她在跟谁说话
/// (08-29 用户点名)。最后一条是自己发的时,引用无条件成立。
#[test]
fn a_reply_right_after_my_own_message_still_quotes() {
    let now = Instant::now();
    let start = PlatformMessagePosition {
        total_messages: 10,
        sender_messages: 2,
    };
    let target = ResponseTarget {
        message_id: "message-1".to_string(),
        user_id: "alice".to_string(),
        quote: true,
        mention: true,
        explicit_mention_user_ids: Vec::new(),
    };
    // 阈值 5 条,实际一条没隔;时间也没到 → 原本两条线索都会哑火。
    let policy = AdaptiveResponseTargetPolicy::new(Some(start), now, 5, 15);
    let current = Some(PlatformMessagePosition {
        total_messages: 10,
        sender_messages: 2,
    });

    let silent = policy.resolve(target.clone(), current, false, now + Duration::from_secs(1));
    assert!(silent.is_none(), "既不引用也不艾特,这正是要修的现象");

    let after_own = policy
        .resolve(target, current, true, now + Duration::from_secs(1))
        .expect("最后一条是自己发的就该引用");
    assert!(after_own.quote, "应当引用");
    assert!(!after_own.mention, "艾特的判据不变,不该被顺带打开");
}
