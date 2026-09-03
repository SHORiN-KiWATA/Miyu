//! 投递：分帧、转发标记、直发抑制与部分失败。

use super::shared::*;
use crate::platforms::onebot::*;

#[test]
fn image_only_turns_receive_nonempty_model_instructions() {
    for count in [1, 2, 4] {
        let prompt = image_only_prompt(count);
        assert!(!prompt.trim().is_empty());
        assert!(prompt.contains(&count.to_string()));
    }
}

#[test]
fn confirmed_direct_send_only_suppresses_later_assistant_text() {
    let outcome = crate::platforms::TurnOutcome {
        run_id: "run-test".to_string(),
        text: "首条消息的回答\n工具发送后的重复确认".to_string(),
        provider_id: None,
        model: None,
        image_assets: Vec::new(),
        suppressed_reply_ranges: vec![(
            "首条消息的回答".len(),
            "首条消息的回答\n工具发送后的重复确认".len(),
        )],
        final_reply_already_sent: true,
    };
    assert_eq!(final_reply_text(&outcome), "首条消息的回答");

    let unsuppressed = crate::platforms::TurnOutcome {
        suppressed_reply_ranges: Vec::new(),
        final_reply_already_sent: false,
        ..outcome
    };
    assert_eq!(
        final_reply_text(&unsuppressed),
        "首条消息的回答\n工具发送后的重复确认"
    );
}

#[test]
fn direct_send_suppression_preserves_text_outside_the_suppressed_range() {
    let prefix = "首条回答";
    let duplicate = "工具确认";
    let later = "后续回答";
    let text = format!("{prefix}{duplicate}{later}");
    let outcome = crate::platforms::TurnOutcome {
        run_id: "run-test".to_string(),
        text,
        provider_id: None,
        model: None,
        image_assets: Vec::new(),
        suppressed_reply_ranges: vec![(prefix.len(), prefix.len() + duplicate.len())],
        final_reply_already_sent: false,
    };
    assert_eq!(final_reply_text(&outcome), format!("{prefix}{later}"));
}

#[tokio::test]
async fn internal_failures_are_silent_in_groups() {
    let temp = tempfile::tempdir().unwrap();
    let paths = test_paths(temp.path());
    let state = test_web_state(temp.path(), 8300);
    let (handle, mut frames) = test_connection(None);
    let target = Target::Group { group_id: 42 };
    let context = Arc::new(PlatformTurnContext::new(
        unique_test_conversation(target),
        "7".to_string(),
        "seven".to_string(),
        false,
        crate::config::AppConfig::default(),
        paths.clone(),
        crate::state::StateStore::new(&paths).unwrap(),
        Arc::new(test_adapter(handle, target)),
        Arc::new(crate::platforms::plugins::PlatformPluginRegistry::default()),
    ));

    let delivered = deliver_dispatch(
        &state,
        &context,
        TurnDispatch::Failed("provider secret".to_string()),
    )
    .await
    .unwrap();
    assert!(!delivered);
    assert!(frames.try_recv().is_err());
}

/// 撤回/取代导致的取消不是错误:私聊也不能回"出错了:本轮被取消了"。
#[tokio::test]
async fn cancelled_turns_send_nothing_even_in_private_chats() {
    let temp = tempfile::tempdir().unwrap();
    let paths = test_paths(temp.path());
    let state = test_web_state(temp.path(), 8300);
    let (handle, mut frames) = test_connection(None);
    let target = Target::Private { user_id: 7 };
    let context = Arc::new(PlatformTurnContext::new(
        unique_test_conversation(target),
        "7".to_string(),
        "seven".to_string(),
        false,
        crate::config::AppConfig::default(),
        paths.clone(),
        crate::state::StateStore::new(&paths).unwrap(),
        Arc::new(test_adapter(handle, target)),
        Arc::new(crate::platforms::plugins::PlatformPluginRegistry::default()),
    ));

    let delivered = deliver_dispatch(&state, &context, TurnDispatch::Cancelled)
        .await
        .unwrap();
    assert!(!delivered);
    assert!(frames.try_recv().is_err());

    // 对照:真正的失败在私聊里仍会回"出错了",证明上面的静默不是测具收不到帧。
    let _ = deliver_dispatch(&state, &context, TurnDispatch::Failed("boom".to_string())).await;
    assert!(frames.try_recv().is_ok());
}

#[tokio::test]
async fn final_delivery_deduplicates_identical_image_content() {
    let temp = tempfile::tempdir().unwrap();
    let state = test_web_state(temp.path(), 8300);
    let store = state.state_store.clone();
    store
        .start_turn("image_turn", "show images", std::process::id())
        .unwrap();
    let duplicate_path = temp.path().join("duplicate.png");
    let distinct_path = temp.path().join("distinct.png");
    image::RgbaImage::from_pixel(2, 2, image::Rgba([255, 0, 0, 255]))
        .save(&duplicate_path)
        .unwrap();
    image::RgbaImage::from_pixel(2, 2, image::Rgba([0, 0, 255, 255]))
        .save(&distinct_path)
        .unwrap();
    let first = store
        .save_image_asset("image_turn", Some("tool_1"), &duplicate_path, "first")
        .unwrap();
    let duplicate = store
        .save_image_asset("image_turn", Some("tool_2"), &duplicate_path, "duplicate")
        .unwrap();
    let distinct = store
        .save_image_asset("image_turn", Some("tool_3"), &distinct_path, "distinct")
        .unwrap();
    store.complete_turn("image_turn", "done", None).unwrap();

    let (handle, mut frames) = test_connection(None);
    let target = Target::Private { user_id: 7 };
    let context = Arc::new(PlatformTurnContext::new(
        unique_test_conversation(target),
        "7".to_string(),
        "seven".to_string(),
        false,
        crate::config::AppConfig::default(),
        test_paths(temp.path()),
        store,
        Arc::new(test_adapter(handle.clone(), target)),
        Arc::new(crate::platforms::plugins::PlatformPluginRegistry::default()),
    ));
    let dispatch = TurnDispatch::Completed(crate::platforms::TurnOutcome {
        run_id: "run-test".to_string(),
        text: "reply".to_string(),
        provider_id: Some("provider-test".to_string()),
        model: Some("model-test".to_string()),
        image_assets: vec![first.asset_id, duplicate.asset_id, distinct.asset_id],
        suppressed_reply_ranges: Vec::new(),
        final_reply_already_sent: false,
    });
    let delivery_state = state.clone();
    let delivery_context = context.clone();
    let delivery = tokio::spawn(async move {
        deliver_dispatch(&delivery_state, &delivery_context, dispatch).await
    });

    let frame: Value = serde_json::from_str(&frames.recv().await.unwrap()).unwrap();
    let segments = frame["params"]["message"].as_array().unwrap();
    assert_eq!(
        segments
            .iter()
            .filter(|segment| segment["type"] == "image")
            .count(),
        2
    );
    route_api_response(
        &handle,
        json!({
            "status": "ok",
            "retcode": 0,
            "data": { "message_id": 70 },
            "echo": frame["echo"],
        }),
    );
    assert!(delivery.await.unwrap().unwrap());
}

#[tokio::test]
async fn final_delivery_skips_an_image_confirmed_by_a_tool_send() {
    let temp = tempfile::tempdir().unwrap();
    let state = test_web_state(temp.path(), 8300);
    let store = state.state_store.clone();
    store
        .start_turn("direct_image_turn", "draw", std::process::id())
        .unwrap();
    let image_path = temp.path().join("generated.png");
    image::RgbaImage::from_pixel(2, 2, image::Rgba([255, 0, 0, 255]))
        .save(&image_path)
        .unwrap();
    let asset = store
        .save_image_asset(
            "direct_image_turn",
            Some("generate_image"),
            &image_path,
            "generated",
        )
        .unwrap();
    store
        .complete_turn("direct_image_turn", "done", None)
        .unwrap();

    let (handle, mut frames) = test_connection(None);
    let target = Target::Private { user_id: 7 };
    let context = Arc::new(PlatformTurnContext::new(
        unique_test_conversation(target),
        "7".to_string(),
        "seven".to_string(),
        false,
        crate::config::AppConfig::default(),
        test_paths(temp.path()),
        store,
        Arc::new(test_adapter(handle.clone(), target)),
        Arc::new(crate::platforms::plugins::PlatformPluginRegistry::default()),
    ));

    let direct_context = context.clone();
    let direct_path = image_path.clone();
    let direct_send = tokio::spawn(async move {
        direct_context
            .send(OutboundMessage::segments(
                OutboundOrigin::Tool,
                vec![OutboundSegment::ImagePath {
                    path: direct_path,
                    alt: "generated".to_string(),
                }],
            ))
            .await
    });
    let direct_frame: Value = serde_json::from_str(
        &tokio::time::timeout(Duration::from_secs(1), frames.recv())
            .await
            .expect("direct image send timed out")
            .expect("direct image frame channel closed"),
    )
    .unwrap();
    assert_eq!(
        direct_frame["params"]["message"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|segment| segment["type"] == "image")
            .count(),
        1
    );
    route_api_response(
        &handle,
        json!({
            "status": "ok",
            "retcode": 0,
            "data": { "message_id": 70 },
            "echo": direct_frame["echo"],
        }),
    );
    direct_send.await.unwrap().unwrap();

    let dispatch = TurnDispatch::Completed(crate::platforms::TurnOutcome {
        run_id: "run-direct-image".to_string(),
        text: "画好了".to_string(),
        provider_id: Some("provider-test".to_string()),
        model: Some("model-test".to_string()),
        image_assets: vec![asset.asset_id],
        suppressed_reply_ranges: Vec::new(),
        final_reply_already_sent: false,
    });
    let delivery_state = state.clone();
    let delivery_context = context.clone();
    let delivery = tokio::spawn(async move {
        deliver_dispatch(&delivery_state, &delivery_context, dispatch).await
    });
    let final_frame: Value = serde_json::from_str(
        &tokio::time::timeout(Duration::from_secs(1), frames.recv())
            .await
            .expect("final text send timed out")
            .expect("final text frame channel closed"),
    )
    .unwrap();
    let final_segments = final_frame["params"]["message"].as_array().unwrap();
    assert!(final_segments
        .iter()
        .any(|segment| segment["data"]["text"] == "画好了"));
    assert!(!final_segments
        .iter()
        .any(|segment| segment["type"] == "image"));
    route_api_response(
        &handle,
        json!({
            "status": "ok",
            "retcode": 0,
            "data": { "message_id": 71 },
            "echo": final_frame["echo"],
        }),
    );
    assert!(delivery.await.unwrap().unwrap());
    assert!(frames.try_recv().is_err());
}

#[tokio::test]
async fn image_only_final_delivery_accepts_an_already_delivered_image() {
    let temp = tempfile::tempdir().unwrap();
    let state = test_web_state(temp.path(), 8300);
    let store = state.state_store.clone();
    store
        .start_turn("direct_only_turn", "draw", std::process::id())
        .unwrap();
    let image_path = temp.path().join("generated.png");
    image::RgbaImage::from_pixel(2, 2, image::Rgba([255, 0, 0, 255]))
        .save(&image_path)
        .unwrap();
    let asset = store
        .save_image_asset(
            "direct_only_turn",
            Some("generate_image"),
            &image_path,
            "generated",
        )
        .unwrap();
    store
        .complete_turn("direct_only_turn", "done", None)
        .unwrap();

    let (handle, mut frames) = test_connection(None);
    let target = Target::Private { user_id: 7 };
    let context = Arc::new(PlatformTurnContext::new(
        unique_test_conversation(target),
        "7".to_string(),
        "seven".to_string(),
        false,
        crate::config::AppConfig::default(),
        test_paths(temp.path()),
        store,
        Arc::new(test_adapter(handle.clone(), target)),
        Arc::new(crate::platforms::plugins::PlatformPluginRegistry::default()),
    ));

    let direct_context = context.clone();
    let direct_path = image_path.clone();
    let direct_send = tokio::spawn(async move {
        direct_context
            .send(OutboundMessage::segments(
                OutboundOrigin::Tool,
                vec![OutboundSegment::ImagePath {
                    path: direct_path,
                    alt: "generated".to_string(),
                }],
            ))
            .await
    });
    let direct_frame: Value = serde_json::from_str(
        &tokio::time::timeout(Duration::from_secs(1), frames.recv())
            .await
            .expect("direct image send timed out")
            .expect("direct image frame channel closed"),
    )
    .unwrap();
    route_api_response(
        &handle,
        json!({
            "status": "ok",
            "retcode": 0,
            "data": { "message_id": 72 },
            "echo": direct_frame["echo"],
        }),
    );
    direct_send.await.unwrap().unwrap();

    let delivered = deliver_dispatch(
        &state,
        &context,
        TurnDispatch::Completed(crate::platforms::TurnOutcome {
            run_id: "run-direct-only".to_string(),
            text: String::new(),
            provider_id: Some("provider-test".to_string()),
            model: Some("model-test".to_string()),
            image_assets: vec![asset.asset_id.clone()],
            suppressed_reply_ranges: Vec::new(),
            final_reply_already_sent: false,
        }),
    )
    .await
    .unwrap();
    assert!(delivered);
    assert!(frames.try_recv().is_err());

    let unresolved = deliver_dispatch(
        &state,
        &context,
        TurnDispatch::Completed(crate::platforms::TurnOutcome {
            run_id: "run-direct-with-missing".to_string(),
            text: String::new(),
            provider_id: Some("provider-test".to_string()),
            model: Some("model-test".to_string()),
            image_assets: vec![asset.asset_id, "missing-asset".to_string()],
            suppressed_reply_ranges: Vec::new(),
            final_reply_already_sent: false,
        }),
    )
    .await
    .unwrap();
    assert!(!unresolved);
    assert!(frames.try_recv().is_err());
}

#[test]
fn outbound_frames_have_the_onebot_shape() {
    let frame: Value = serde_json::from_str(&api_frame(
        "send_private_msg",
        json!({ "user_id": 42, "message": [text_segment("hi")] }),
        "test",
    ))
    .unwrap();
    assert_eq!(frame["action"], "send_private_msg");
    assert_eq!(frame["params"]["user_id"], 42);
    assert_eq!(frame["params"]["message"][0]["type"], "text");
    assert_eq!(frame["params"]["message"][0]["data"]["text"], "hi");
    assert!(frame["echo"].as_str().is_some());

    let frame: Value = serde_json::from_str(&api_frame(
        "send_group_msg",
        json!({ "group_id": 7, "message": [text_segment("x")] }),
        "test",
    ))
    .unwrap();
    assert_eq!(frame["action"], "send_group_msg");
    assert_eq!(frame["params"]["group_id"], 7);
}

#[tokio::test]
async fn file_upload_falls_back_to_base64_after_url_failure() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("sample.txt");
    tokio::fs::write(&path, b"hello").await.unwrap();
    let (handle, mut frames) = test_connection(Some("http://miyu.test:8300".to_string()));
    let adapter = test_adapter(handle.clone(), Target::Private { user_id: 42 });
    let upload = tokio::spawn(async move { adapter.upload_file(&path, None).await });

    let first: Value = serde_json::from_str(&frames.recv().await.unwrap()).unwrap();
    assert_eq!(first["action"], "upload_private_file");
    assert!(first["params"]["file"]
        .as_str()
        .unwrap()
        .starts_with("http://miyu.test:8300/api/platform-assets/"));
    route_api_response(
        &handle,
        json!({
            "status": "failed",
            "retcode": 100,
            "data": null,
            "echo": first["echo"],
        }),
    );

    let second: Value = serde_json::from_str(&frames.recv().await.unwrap()).unwrap();
    assert_eq!(second["action"], "upload_private_file");
    assert_eq!(second["params"]["file"], "base64://aGVsbG8=");
    route_api_response(
        &handle,
        json!({
            "status": "ok",
            "retcode": 0,
            "data": { "file_id": "file-1" },
            "echo": second["echo"],
        }),
    );
    assert_eq!(upload.await.unwrap().unwrap().as_deref(), Some("file-1"));
}

#[tokio::test]
async fn adapter_reports_confirmed_images_on_later_attachment_failure() {
    let temp = tempfile::tempdir().unwrap();
    let missing_file = temp.path().join("missing.txt");
    let (handle, mut frames) = test_connection(None);
    let adapter = Arc::new(test_adapter(handle.clone(), Target::Private { user_id: 7 }));
    let send = {
        let adapter = adapter.clone();
        tokio::spawn(async move {
            adapter
                .send_message(OutboundMessage::segments(
                    OutboundOrigin::Tool,
                    vec![
                        OutboundSegment::ImageBytes {
                            mime: "image/png".to_string(),
                            data: Arc::from([1_u8, 2, 3]),
                            alt: "sample".to_string(),
                        },
                        OutboundSegment::FilePath {
                            path: missing_file,
                            name: None,
                        },
                    ],
                ))
                .await
        })
    };

    let frame: Value = serde_json::from_str(
        &tokio::time::timeout(Duration::from_secs(1), frames.recv())
            .await
            .expect("image send timed out")
            .expect("image frame channel closed"),
    )
    .unwrap();
    route_api_response(
        &handle,
        json!({
            "status": "ok",
            "retcode": 0,
            "data": { "message_id": 122 },
            "echo": frame["echo"],
        }),
    );

    let error = send.await.unwrap().unwrap_err();
    let partial = error
        .downcast_ref::<PartialSendError>()
        .expect("partial send error");
    assert_eq!(partial.receipt().delivered_parts, 1);
    assert_eq!(partial.receipt().message_ids, vec!["122"]);
    assert_eq!(
        partial.receipt().image_digests,
        vec![blake3::hash(&[1_u8, 2, 3])]
    );
    assert!(frames.try_recv().is_err());
}

#[tokio::test]
async fn adapter_smoke_test_sends_replies_images_and_forward_nodes() {
    let (handle, mut frames) = test_connection(None);
    let adapter = Arc::new(test_adapter(handle.clone(), Target::Group { group_id: 42 }));
    let mut message = OutboundMessage::segments(
        OutboundOrigin::FinalReply,
        vec![
            OutboundSegment::Text("hello".to_string()),
            OutboundSegment::ImageBytes {
                mime: "image/png".to_string(),
                data: Arc::from([1_u8, 2, 3]),
                alt: "sample".to_string(),
            },
        ],
    );
    message.response_target = Some(ResponseTarget {
        message_id: "99".to_string(),
        user_id: "77".to_string(),
        quote: true,
        mention: true,
        explicit_mention_user_ids: Vec::new(),
    });
    let send = {
        let adapter = adapter.clone();
        tokio::spawn(async move { adapter.send_message(message).await })
    };
    let frame: Value = serde_json::from_str(&frames.recv().await.unwrap()).unwrap();
    assert_eq!(frame["action"], "send_group_msg");
    assert_eq!(frame["params"]["group_id"], 42);
    assert_eq!(frame["params"]["message"][0]["type"], "reply");
    assert_eq!(frame["params"]["message"][1]["type"], "at");
    assert_eq!(frame["params"]["message"][1]["data"]["qq"], "77");
    assert_eq!(frame["params"]["message"][2]["data"]["text"], " ");
    assert_eq!(frame["params"]["message"][3]["data"]["text"], "hello");
    assert_eq!(
        frame["params"]["message"][4]["data"]["file"],
        "base64://AQID"
    );
    route_api_response(
        &handle,
        json!({
            "status": "ok",
            "retcode": 0,
            "data": { "message_id": 123 },
            "echo": frame["echo"],
        }),
    );
    let receipt = send.await.unwrap().unwrap();
    assert_eq!(receipt.message_ids, vec!["123"]);
    assert_eq!(receipt.image_message_ids, vec!["123"]);
    assert_eq!(receipt.delivered_parts, 1);
    assert_eq!(receipt.image_digests, vec![blake3::hash(&[1_u8, 2, 3])]);

    let forward = OutboundMessage {
        body: OutboundBody::Forward(vec![ForwardNode {
            user_id: "10000".to_string(),
            display_name: "Miyu".to_string(),
            segments: vec![OutboundSegment::Markdown("**long**".to_string())],
        }]),
        response_target: Some(ResponseTarget {
            message_id: "98".to_string(),
            user_id: "76".to_string(),
            quote: true,
            mention: true,
            explicit_mention_user_ids: Vec::new(),
        }),
        origin: OutboundOrigin::Plugin,
        metadata: Default::default(),
    };
    let send = {
        let adapter = adapter.clone();
        tokio::spawn(async move { adapter.send_message(forward).await })
    };
    let frame: Value = serde_json::from_str(&frames.recv().await.unwrap()).unwrap();
    assert_eq!(frame["action"], "send_group_forward_msg");
    assert_eq!(frame["params"]["messages"][0]["type"], "node");
    assert_eq!(
        frame["params"]["messages"][0]["data"]["content"][0]["data"]["text"],
        "long"
    );
    route_api_response(
        &handle,
        json!({
            "status": "ok",
            "retcode": 0,
            "data": { "message_id": "forward-1" },
            "echo": frame["echo"],
        }),
    );
    let marker: Value = serde_json::from_str(&frames.recv().await.unwrap()).unwrap();
    assert_eq!(marker["action"], "send_group_msg");
    assert_eq!(marker["params"]["message"][0]["type"], "reply");
    assert_eq!(marker["params"]["message"][0]["data"]["id"], "98");
    assert_eq!(marker["params"]["message"][1]["type"], "at");
    assert_eq!(marker["params"]["message"][1]["data"]["qq"], "76");
    assert_eq!(marker["params"]["message"][2]["data"]["text"], " ");
    route_api_response(
        &handle,
        json!({
            "status": "ok",
            "retcode": 0,
            "data": { "message_id": "marker-1" },
            "echo": marker["echo"],
        }),
    );
    assert_eq!(
        send.await.unwrap().unwrap().message_ids,
        vec!["forward-1", "marker-1"]
    );
}

#[tokio::test]
async fn split_replies_encode_the_response_target_only_on_the_first_frame() {
    let (handle, mut frames) = test_connection(None);
    let mut adapter = test_adapter(handle.clone(), Target::Group { group_id: 42 });
    adapter.max_reply_chars = 3;
    let adapter = Arc::new(adapter);
    let mut message = OutboundMessage::text(OutboundOrigin::FinalReply, "abcdef");
    message.response_target = Some(ResponseTarget {
        message_id: "99".to_string(),
        user_id: "7".to_string(),
        quote: true,
        mention: true,
        explicit_mention_user_ids: Vec::new(),
    });
    let send = {
        let adapter = adapter.clone();
        tokio::spawn(async move { adapter.send_message(message).await })
    };

    let first: Value = serde_json::from_str(&frames.recv().await.unwrap()).unwrap();
    assert_eq!(first["params"]["message"][0]["type"], "reply");
    assert_eq!(first["params"]["message"][1]["type"], "at");
    assert_eq!(first["params"]["message"][2]["data"]["text"], " ");
    route_api_response(
        &handle,
        json!({
            "status": "ok",
            "retcode": 0,
            "data": { "message_id": 1 },
            "echo": first["echo"],
        }),
    );

    let second: Value = serde_json::from_str(&frames.recv().await.unwrap()).unwrap();
    assert_eq!(second["params"]["message"][0]["type"], "text");
    assert!(second["params"]["message"]
        .as_array()
        .unwrap()
        .iter()
        .all(|segment| !matches!(segment["type"].as_str(), Some("reply" | "at"))));
    route_api_response(
        &handle,
        json!({
            "status": "ok",
            "retcode": 0,
            "data": { "message_id": 2 },
            "echo": second["echo"],
        }),
    );
    let receipt = send.await.unwrap().unwrap();
    assert_eq!(receipt.message_ids, vec!["1", "2"]);
    assert!(receipt.response_target_delivered);
}

#[tokio::test]
async fn split_failure_reports_that_the_response_target_was_delivered() {
    let (handle, mut frames) = test_connection(None);
    let mut adapter = test_adapter(handle.clone(), Target::Group { group_id: 42 });
    adapter.max_reply_chars = 3;
    let adapter = Arc::new(adapter);
    let mut message = OutboundMessage::text(OutboundOrigin::FinalReply, "abcdef");
    message.response_target = Some(ResponseTarget {
        message_id: String::new(),
        user_id: String::new(),
        quote: false,
        mention: false,
        explicit_mention_user_ids: vec!["30000".to_string(), "40000".to_string()],
    });
    let send = {
        let adapter = adapter.clone();
        tokio::spawn(async move { adapter.send_message(message).await })
    };

    let first: Value = serde_json::from_str(&frames.recv().await.unwrap()).unwrap();
    assert_eq!(first["params"]["message"][0]["data"]["qq"], "30000");
    assert_eq!(first["params"]["message"][2]["data"]["qq"], "40000");
    route_api_response(
        &handle,
        json!({
            "status": "ok",
            "retcode": 0,
            "data": { "message_id": 1 },
            "echo": first["echo"],
        }),
    );

    let second: Value = serde_json::from_str(&frames.recv().await.unwrap()).unwrap();
    route_api_response(
        &handle,
        json!({
            "status": "failed",
            "retcode": 100,
            "data": null,
            "echo": second["echo"],
        }),
    );
    let error = send.await.unwrap().unwrap_err();
    let partial = error.downcast_ref::<PartialSendError>().unwrap();
    assert_eq!(partial.receipt().delivered_parts, 1);
    assert!(partial.receipt().response_target_delivered);
}

#[tokio::test]
async fn forward_marker_failure_is_reported_as_partial_delivery() {
    let (handle, mut frames) = test_connection(None);
    let adapter = Arc::new(test_adapter(handle.clone(), Target::Group { group_id: 42 }));
    let message = OutboundMessage {
        body: OutboundBody::Forward(vec![ForwardNode {
            user_id: "10000".to_string(),
            display_name: "Miyu".to_string(),
            segments: vec![OutboundSegment::Text("forward".to_string())],
        }]),
        response_target: Some(ResponseTarget {
            message_id: String::new(),
            user_id: String::new(),
            quote: false,
            mention: false,
            explicit_mention_user_ids: vec!["30000".to_string()],
        }),
        origin: OutboundOrigin::FinalReply,
        metadata: Default::default(),
    };
    let send = {
        let adapter = adapter.clone();
        tokio::spawn(async move { adapter.send_message(message).await })
    };

    let forward: Value = serde_json::from_str(&frames.recv().await.unwrap()).unwrap();
    assert_eq!(forward["action"], "send_group_forward_msg");
    route_api_response(
        &handle,
        json!({
            "status": "ok",
            "retcode": 0,
            "data": { "message_id": "forward-1" },
            "echo": forward["echo"],
        }),
    );

    let marker: Value = serde_json::from_str(&frames.recv().await.unwrap()).unwrap();
    assert_eq!(marker["action"], "send_group_msg");
    route_api_response(
        &handle,
        json!({
            "status": "failed",
            "retcode": 100,
            "data": null,
            "echo": marker["echo"],
        }),
    );

    let error = send.await.unwrap().unwrap_err();
    let partial = error.downcast_ref::<PartialSendError>().unwrap();
    assert_eq!(partial.receipt().delivered_parts, 1);
    assert!(!partial.receipt().response_target_delivered);
}

#[tokio::test]
async fn invalid_attachment_does_not_send_a_bare_response_marker() {
    let temp = tempfile::tempdir().unwrap();
    let missing = temp.path().join("missing.txt");
    let (handle, mut frames) = test_connection(None);
    let adapter = test_adapter(handle, Target::Group { group_id: 42 });
    let message = OutboundMessage::segments(
        OutboundOrigin::FinalReply,
        vec![OutboundSegment::FilePath {
            path: missing,
            name: None,
        }],
    );
    let mut message = message;
    message.response_target = Some(ResponseTarget {
        message_id: String::new(),
        user_id: String::new(),
        quote: false,
        mention: false,
        explicit_mention_user_ids: vec!["30000".to_string()],
    });

    assert!(adapter.send_message(message).await.is_err());
    assert!(frames.try_recv().is_err());
}
