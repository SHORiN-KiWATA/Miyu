//! 回合构建、唤醒与工具追加。

use super::shared::*;
use crate::platforms::onebot::*;

/// issue #29:唤醒合成事件必须继承发起者身份,不能伪装成机器人自己。
#[test]
fn wake_sender_inherits_recorded_initiator() {
    let group = Target::Group { group_id: 777 };
    assert_eq!(wake_sender_user_id(Some("10086"), group, 999), 10086);
    let private = Target::Private { user_id: 555 };
    assert_eq!(wake_sender_user_id(Some("10086"), private, 999), 10086);
}

#[test]
fn wake_sender_falls_back_to_private_peer_then_self() {
    // 私聊无记录:对端就是这个私聊唯一的人类。
    let private = Target::Private { user_id: 555 };
    assert_eq!(wake_sender_user_id(None, private, 999), 555);
    assert_eq!(wake_sender_user_id(Some("not-a-number"), private, 999), 555);
    // 群聊无记录:保持 self_id,不凭空授予任何成员的权限。
    let group = Target::Group { group_id: 777 };
    assert_eq!(wake_sender_user_id(None, group, 999), 999);
}

#[tokio::test]
async fn text_tool_followup_is_observed_and_queued_for_the_running_turn() {
    let temp = tempfile::tempdir().unwrap();
    let state = test_web_state(temp.path(), 0);
    let config = state.manager.lock().unwrap().config.clone();
    let target = Target::Group { group_id: 99 };
    let event = json!({
        "self_id": 10000,
        "user_id": 42,
        "message_id": 123,
        "message_type": "group",
        "group_id": 99,
        "message": "再检查一下",
        "sender": { "nickname": "Alice" }
    });
    let (connection, _frames) = test_connection(None);
    let parsed = InboundMessage {
        text: "再检查一下".to_string(),
        ..InboundMessage::default()
    };
    let inbound = message_event(target, &event, &parsed);
    let context = Arc::new(
        platform_turn_context(
            &state,
            connection.clone(),
            target,
            &event,
            config,
            Some(inbound.clone()),
        )
        .unwrap(),
    );
    let followup = PlatformFollowupRun::new(context.clone());
    let session_id = state.state_store.session_id();
    let turn_store = state.state_store.pinned_for_turn(&session_id);
    turn_store
        .start_turn("running_followup", "first", std::process::id())
        .unwrap();
    let (cancel, _cancel_rx) = watch::channel(false);
    state.manager.lock().unwrap().active_runs.insert(
        "run-followup".to_string(),
        crate::runtime::RunInfo {
            session_id: session_id.clone(),
            mode: crate::agent::AgentMode::Normal,
            audience: crate::config::PromptAudience::External,
            cancel,
            turn_id: Some("running_followup".to_string()),
            queue_target: Some(turn_store.queue_target("running_followup")),
            supersede: Arc::new(crate::agent::TurnSupersedeSignal::default()),
            platform_followup: Some(followup.clone()),
            operation: crate::runtime::RunOperation::Create,
            job_wake: false,
            turn_origin: crate::tools::workspace::TurnOrigin::Human,
            job_wake_label: None,
        },
    );

    enqueue_tool_followup(
        &state,
        &connection,
        target,
        &event,
        parsed,
        &inbound,
        &context,
        &followup,
        &session_id,
        "run-followup",
        "running_followup",
        TurnUpdateMode::Followup,
    )
    .await
    .unwrap();

    let queued = turn_store.load_queued_prompts().unwrap();
    assert_eq!(queued.len(), 1);
    assert_eq!(queued[0].display_content, "再检查一下");
    assert!(queued[0].content.starts_with("再检查一下"));
    assert!(queued[0].content.contains("sender QQ=42; message ID=123"));
}

#[tokio::test]
async fn reset_command_uses_configured_admins_and_clears_the_bound_session() {
    let temp = tempfile::tempdir().unwrap();
    let (state, actor_join) =
        DaemonState::for_test_with_actor(test_paths(temp.path()), 8300).unwrap();
    let target = Target::Group { group_id: 99 };
    let event = json!({
        "self_id": 10000,
        "user_id": 42,
        "message_type": "group",
        "group_id": 99,
        "message_id": 7,
        "message": [{ "type": "text", "data": { "text": "/reset extra" } }],
        "sender": { "nickname": "Alice", "role": "owner" }
    });
    state.manager.lock().unwrap().config.platforms.qq.enabled = true;
    let (connection, mut frames) = test_connection(None);
    let persona = state.manager.lock().unwrap().config.active_persona_scope();
    let sessions_before = state.state_store.list_sessions(&persona).unwrap().len();

    // QQ group roles never grant Miyu command administration.
    let denied = tokio::spawn(handle_message(
        state.clone(),
        connection.clone(),
        event.clone(),
        next_ingress_order(),
    ));
    denied.await.unwrap();
    assert!(frames.try_recv().is_err());
    assert_eq!(
        state.state_store.list_sessions(&persona).unwrap().len(),
        sessions_before
    );

    state
        .manager
        .lock()
        .unwrap()
        .config
        .platforms
        .qq
        .admin_users
        .push(42);
    let context = platform_turn_context(
        &state,
        connection.clone(),
        target,
        &event,
        state.manager.lock().unwrap().config.clone(),
        None,
    )
    .unwrap();
    assert!(context.is_admin);
    let session_id = resolve_onebot_session(&state, &context, target, &event).unwrap();
    let store = state.state_store.pinned(&session_id);
    store
        .start_turn("qq_history", "hello", std::process::id())
        .unwrap();
    store.complete_turn("qq_history", "world", None).unwrap();

    let mut raw_reset_event = event.clone();
    raw_reset_event["message"] = json!("[CQ:reply,id=6]/reset");
    let reset = tokio::spawn(handle_message(
        state.clone(),
        connection.clone(),
        raw_reset_event,
        next_ingress_order(),
    ));
    let reset_frame: Value = serde_json::from_str(&frames.recv().await.unwrap()).unwrap();
    assert_eq!(reset_frame["action"], "send_group_msg");
    route_api_response(
        &connection,
        json!({
            "status": "ok",
            "retcode": 0,
            "data": { "message_id": 71 },
            "echo": reset_frame["echo"],
        }),
    );
    reset.await.unwrap();
    assert!(store.load_turns().unwrap().is_empty());
    assert!(temp
        .path()
        .join("data/platforms/onebot/message_history/history.sqlite3")
        .is_file());
    assert_eq!(
        resolve_onebot_session(&state, &context, target, &event).unwrap(),
        session_id
    );
    assert!(!state.manager.lock().unwrap().admin_busy);

    state
        .actor_tx
        .send(crate::runtime::ActorCommand::Shutdown)
        .unwrap();
    actor_join.join().unwrap().unwrap();
}

#[tokio::test]
async fn wipe_clears_every_local_session_of_the_active_persona() {
    let temp = tempfile::tempdir().unwrap();
    let (state, actor_join) =
        DaemonState::for_test_with_actor(test_paths(temp.path()), 8300).unwrap();
    let mut config = state.manager.lock().unwrap().config.clone();
    config.platforms.qq.admin_users.push(42);
    let persona = config.active_persona_scope();
    state
        .state_store
        .adopt_sessions_for_persona(&persona)
        .unwrap();
    let active = state
        .state_store
        .create_session(&persona, "active", "user", None)
        .unwrap();
    let second = state
        .state_store
        .create_session(&persona, "second", "user", None)
        .unwrap();
    for (session_id, turn_id) in [
        (&active.session_id, "active-before-reset-all"),
        (&second.session_id, "second-before-reset-all"),
    ] {
        let store = state.state_store.pinned(session_id);
        store
            .start_turn(turn_id, "before", std::process::id())
            .unwrap();
        store.complete_turn(turn_id, "after", None).unwrap();
    }

    let generated_skill = config
        .active_persona_skills_dir(&state.paths)
        .join("generated-test");
    std::fs::create_dir_all(&generated_skill).unwrap();
    std::fs::write(
        generated_skill.join("SKILL.md"),
        "---\ngenerated_by: miyu\n---\n",
    )
    .unwrap();

    let target = Target::Private { user_id: 42 };
    let event = json!({
        "self_id": 10000,
        "user_id": 42,
        "message_type": "private",
        "message_id": 8,
        "message": [{ "type": "text", "data": { "text": "/reset all" } }],
        "sender": { "nickname": "Alice" }
    });
    let (connection, _frames) = test_connection(None);
    let context = platform_turn_context(&state, connection, target, &event, config, None).unwrap();
    let response = execute_builtin_command(
        &state,
        &context,
        target,
        &event,
        commands::ParsedPlatformCommand::Wipe { confirmed: false },
    )
    .await
    .expect("an unconfirmed wipe answers with what it would erase");
    let asked = format!("{:?}", response.body);
    assert!(asked.contains("confirm"), "{asked}");
    // Nothing may be gone yet: the word `confirm` is the only dialog box a
    // chat platform gets.
    assert!(!state
        .state_store
        .pinned(&active.session_id)
        .load_turns()
        .unwrap()
        .is_empty());

    let response = execute_builtin_command(
        &state,
        &context,
        target,
        &event,
        commands::ParsedPlatformCommand::Wipe { confirmed: true },
    )
    .await
    .expect("a confirmed wipe returns a response");

    assert!(matches!(response.body, OutboundBody::Segments(_)));
    assert!(state
        .state_store
        .pinned(&active.session_id)
        .load_turns()
        .unwrap()
        .is_empty());
    // 归档豁免已随功能移除:/reset all 现在清掉本人格全部本地会话。
    assert!(state
        .state_store
        .pinned(&second.session_id)
        .load_turns()
        .unwrap()
        .is_empty());
    // wipe 只抹记忆和会话:技能是磁盘上的文件,得留着。
    assert!(generated_skill.exists());
    assert!(!state.manager.lock().unwrap().admin_busy);

    state
        .actor_tx
        .send(crate::runtime::ActorCommand::Shutdown)
        .unwrap();
    actor_join.join().unwrap().unwrap();
}

#[tokio::test]
async fn stop_command_cancels_the_session_and_preserves_completed_history() {
    let temp = tempfile::tempdir().unwrap();
    let state = test_web_state(temp.path(), 8300);
    let target = Target::Private { user_id: 42 };
    let event = json!({
        "self_id": 10000,
        "user_id": 42,
        "message_type": "private",
        "message_id": 8,
        "message": [{ "type": "text", "data": { "text": "/stop" } }],
        "sender": { "nickname": "Alice" }
    });
    let (connection, _frames) = test_connection(None);
    let mut config = state.manager.lock().unwrap().config.clone();
    config.platforms.qq.admin_users.push(42);
    let context = platform_turn_context(&state, connection, target, &event, config, None).unwrap();
    let session_id = resolve_onebot_session(&state, &context, target, &event).unwrap();
    let store = state.state_store.pinned(&session_id);
    store
        .start_turn("completed_before_stop", "hello", std::process::id())
        .unwrap();
    store
        .complete_turn("completed_before_stop", "world", None)
        .unwrap();
    let (cancel, cancel_rx) = watch::channel(false);
    state.manager.lock().unwrap().active_runs.insert(
        "active_stop_test".to_string(),
        crate::runtime::RunInfo {
            session_id: session_id.clone(),
            mode: crate::agent::AgentMode::Normal,
            audience: crate::config::PromptAudience::External,
            cancel,
            turn_id: None,
            queue_target: None,
            supersede: Arc::new(crate::agent::TurnSupersedeSignal::default()),
            platform_followup: None,
            operation: crate::runtime::RunOperation::Create,
            job_wake: false,
            turn_origin: crate::tools::workspace::TurnOrigin::Human,
            job_wake_label: None,
        },
    );

    let response = execute_builtin_command(
        &state,
        &context,
        target,
        &event,
        commands::ParsedPlatformCommand::Stop {
            has_arguments: false,
        },
    )
    .await;

    assert!(*cancel_rx.borrow());
    assert_eq!(store.load_turns().unwrap().len(), 1);
    let OutboundBody::Segments(segments) = response.expect("stop returns a response").body else {
        panic!("stop response must be a normal message");
    };
    assert!(matches!(
        segments.as_slice(),
        [OutboundSegment::Text(text)]
            if text.contains("已打断 1 个运行中的任务") || text.contains("Interrupted 1 running task")
    ));
    state
        .manager
        .lock()
        .unwrap()
        .active_runs
        .remove("active_stop_test");
}

#[test]
fn private_message_info_uses_target_peer_and_sender_fallbacks() {
    let sent = parse_message_info(
        &json!({
            "message_type": "private",
            "message_id": 1,
            "target_id": 20000,
            "sender": { "user_id": 10000, "nickname": "Miyu" },
            "message": [{ "type": "text", "data": { "text": "hello" } }],
        }),
        10000,
    )
    .unwrap();
    assert_eq!(sent.conversation_kind, Some(ConversationKind::Private));
    assert_eq!(sent.conversation_id.as_deref(), Some("20000"));
    assert_eq!(sent.sender_id, "10000");

    let received = parse_message_info(
        &json!({
            "message_type": "private",
            "message_id": "2",
            "sender": { "user_id": "20000", "nickname": "user" },
            "message": [],
        }),
        10000,
    )
    .unwrap();
    assert_eq!(received.conversation_id.as_deref(), Some("20000"));

    // 用户发来的消息:`get_msg` 回来的 target_id 是**收件人**,也就是机器人
    // 自己。原来不过滤就直接采信,会话 id 成了自己的号,`adapter.rs` 的归属
    // 校验判成"属于另一个会话",vision_analyze 取历史图必失败
    // (08-30 测具实测)。私聊的会话 id 永远是对方。
    let inbound_with_self_target = parse_message_info(
        &json!({
            "message_type": "private",
            "message_id": 3,
            "target_id": 10000,
            "user_id": 20000,
            "sender": { "user_id": 20000, "nickname": "user" },
            "message": [{ "type": "text", "data": { "text": "hi" } }],
        }),
        10000,
    )
    .unwrap();
    assert_eq!(
        inbound_with_self_target.conversation_id.as_deref(),
        Some("20000"),
        "私聊会话 id 必须是对方,不能是自己"
    );
}

#[tokio::test]
async fn quoted_images_are_fetched_once_merged_and_bounded() {
    let (handle, mut frames) = test_connection(None);
    let mut parsed = InboundMessage {
        images: vec![MediaRef::Url("https://img.example/current.png".to_string())],
        reply_to_message_id: Some("91".to_string()),
        ..Default::default()
    };
    let lookup_handle = handle.clone();
    let lookup = tokio::spawn(async move {
        let added = merge_quoted_message_images(&lookup_handle, "90", &mut parsed, None).await?;
        Result::<_>::Ok((added, parsed))
    });

    let frame: Value = serde_json::from_str(&frames.recv().await.unwrap()).unwrap();
    assert_eq!(frame["action"], "get_msg");
    assert_eq!(frame["params"]["message_id"], 91);
    route_api_response(
        &handle,
        json!({
            "status": "ok",
            "retcode": 0,
            "data": {
                "message_id": 91,
                "message": [
                    { "type": "reply", "data": { "id": 80 } },
                    { "type": "image", "data": { "url": "https://img.example/current.png" } },
                    { "type": "image", "data": { "file": "base64://AQ==" } },
                    { "type": "image", "data": { "file": "base64://Ag==" } },
                    { "type": "image", "data": { "file": "base64://Aw==" } },
                    { "type": "image", "data": { "file": "base64://BA==" } }
                ]
            },
            "echo": frame["echo"],
        }),
    );
    let (added, parsed) = lookup.await.unwrap().unwrap();
    assert_eq!(added, 3);
    assert_eq!(parsed.images.len(), MAX_INBOUND_IMAGES);
    assert!(matches!(&parsed.images[0], MediaRef::Url(url) if url.ends_with("current.png")));
    assert!(matches!(&parsed.images[1], MediaRef::Bytes(bytes) if bytes == &[1]));
    assert!(matches!(&parsed.images[3], MediaRef::Bytes(bytes) if bytes == &[3]));
    assert!(
        frames.try_recv().is_err(),
        "nested replies must not be fetched"
    );

    let mut self_reply = InboundMessage {
        reply_to_message_id: Some("90".to_string()),
        ..Default::default()
    };
    assert_eq!(
        merge_quoted_message_images(&handle, "90", &mut self_reply, None)
            .await
            .unwrap(),
        0
    );
    assert!(frames.try_recv().is_err());
}

#[tokio::test]
async fn preloaded_quoted_metadata_avoids_a_second_message_lookup() {
    let (handle, mut frames) = test_connection(None);
    let mut parsed = InboundMessage {
        reply_to_message_id: Some("91".to_string()),
        ..Default::default()
    };
    let data = json!({
        "message_id": 91,
        "sender": { "user_id": 8, "nickname": "eight" },
        "message": [{ "type": "image", "data": { "file": "base64://AQ==" } }]
    });

    assert_eq!(
        merge_quoted_message_images(&handle, "90", &mut parsed, Some(&data))
            .await
            .unwrap(),
        1
    );
    assert!(frames.try_recv().is_err());
}

#[tokio::test]
async fn quoted_napcat_file_image_uses_get_image_fallback() {
    let (handle, mut frames) = test_connection(None);
    let mut parsed = InboundMessage {
        reply_to_message_id: Some("91".to_string()),
        ..Default::default()
    };
    let lookup_handle = handle.clone();
    let lookup = tokio::spawn(async move {
        let added = merge_quoted_message_images(&lookup_handle, "90", &mut parsed, None).await?;
        Result::<_>::Ok((added, parsed))
    });

    let get_msg: Value = serde_json::from_str(&frames.recv().await.unwrap()).unwrap();
    assert_eq!(get_msg["action"], "get_msg");
    route_api_response(
        &handle,
        json!({
            "status": "ok",
            "retcode": 0,
            "data": {
                "message_id": 91,
                // NapCat get_msg disables URL resolution and normally
                // exposes only the registered image file identifier.
                "message": [{
                    "type": "image",
                    "data": { "file": "napcat-image.jpg", "url": "" }
                }]
            },
            "echo": get_msg["echo"],
        }),
    );

    let get_image: Value = serde_json::from_str(&frames.recv().await.unwrap()).unwrap();
    assert_eq!(get_image["action"], "get_image");
    assert_eq!(get_image["params"]["file"], "napcat-image.jpg");
    route_api_response(
        &handle,
        json!({
            "status": "ok",
            "retcode": 0,
            "data": {
                "file": "/tmp/napcat-image.jpg",
                "url": "https://img.example/quoted.jpg"
            },
            "echo": get_image["echo"],
        }),
    );

    let (added, parsed) = lookup.await.unwrap().unwrap();
    assert_eq!(added, 1);
    assert!(matches!(
        &parsed.images[0],
        MediaRef::Url(url) if url == "https://img.example/quoted.jpg"
    ));
    assert!(frames.try_recv().is_err());
}

#[tokio::test]
async fn current_napcat_file_image_uses_get_image_fallback() {
    let (handle, mut frames) = test_connection(None);
    let message = json!([{
        "type": "image",
        "data": { "file": "current-napcat-image.jpg", "url": "" }
    }]);
    let mut parsed = parse_message(Some(&message), None, 10001);
    assert!(parsed.images.is_empty());
    assert_eq!(parsed.unresolved_image_files, ["current-napcat-image.jpg"]);
    let lookup_handle = handle.clone();
    let lookup = tokio::spawn(async move {
        resolve_current_message_images(&lookup_handle, &mut parsed).await;
        parsed
    });

    let get_image: Value = serde_json::from_str(&frames.recv().await.unwrap()).unwrap();
    assert_eq!(get_image["action"], "get_image");
    assert_eq!(get_image["params"]["file"], "current-napcat-image.jpg");
    route_api_response(
        &handle,
        json!({
            "status": "ok",
            "retcode": 0,
            "data": { "base64": "AQID" },
            "echo": get_image["echo"],
        }),
    );
    let parsed = lookup.await.unwrap();
    assert!(parsed.unresolved_image_files.is_empty());
    assert!(matches!(&parsed.images[0], MediaRef::Bytes(bytes) if bytes == &[1, 2, 3]));
}

#[tokio::test]
async fn adapter_history_images_preserve_order_and_reject_other_groups() {
    let (handle, mut frames) = test_connection(None);
    let adapter = Arc::new(test_adapter(handle.clone(), Target::Group { group_id: 42 }));
    let lookup = {
        let adapter = adapter.clone();
        tokio::spawn(async move { adapter.message_images("90").await })
    };
    let get_msg: Value = serde_json::from_str(&frames.recv().await.unwrap()).unwrap();
    route_api_response(
        &handle,
        json!({
            "status": "ok",
            "retcode": 0,
            "data": {
                "message_id": 90,
                "message_type": "group",
                "group_id": 42,
                "sender": { "user_id": 7, "nickname": "sender" },
                "message": [
                    { "type": "image", "data": { "file": "base64://AQID" } },
                    { "type": "image", "data": { "file": "base64://AQID" } }
                ]
            },
            "echo": get_msg["echo"],
        }),
    );
    let images = lookup.await.unwrap().unwrap();
    assert_eq!(images.len(), 2);
    assert_eq!(&*images[0].data, &[1, 2, 3]);
    assert_eq!(&*images[1].data, &[1, 2, 3]);

    let rejected = {
        let adapter = adapter.clone();
        tokio::spawn(async move { adapter.message_images("91").await })
    };
    let get_msg: Value = serde_json::from_str(&frames.recv().await.unwrap()).unwrap();
    route_api_response(
        &handle,
        json!({
            "status": "ok",
            "retcode": 0,
            "data": {
                "message_id": 91,
                "message_type": "group",
                "group_id": 99,
                "sender": { "user_id": 8, "nickname": "other" },
                "message": [{ "type": "image", "data": { "file": "base64://BAUG" } }]
            },
            "echo": get_msg["echo"],
        }),
    );
    let error = rejected.await.unwrap().unwrap_err();
    assert!(error
        .to_string()
        .contains("belongs to another conversation"));
}

#[tokio::test]
async fn adapter_exposes_reactions_message_details_and_group_members() {
    let (handle, mut frames) = test_connection(None);
    let adapter = Arc::new(test_adapter(handle.clone(), Target::Group { group_id: 42 }));

    let reaction = {
        let adapter = adapter.clone();
        tokio::spawn(async move { adapter.set_message_reaction("90", "289", true).await })
    };
    let frame: Value = serde_json::from_str(&frames.recv().await.unwrap()).unwrap();
    assert_eq!(frame["action"], "set_msg_emoji_like");
    assert_eq!(frame["params"]["message_id"], 90);
    assert_eq!(frame["params"]["emoji_id"], 289);
    assert_eq!(frame["params"]["set"], true);
    route_api_response(
        &handle,
        json!({ "status": "ok", "retcode": 0, "data": null, "echo": frame["echo"] }),
    );
    reaction.await.unwrap().unwrap();

    let members = {
        let adapter = adapter.clone();
        tokio::spawn(async move { adapter.group_members().await })
    };
    let frame: Value = serde_json::from_str(&frames.recv().await.unwrap()).unwrap();
    assert_eq!(frame["action"], "get_group_member_list");
    assert_eq!(frame["params"]["group_id"], 42);
    route_api_response(
        &handle,
        json!({
            "status": "ok",
            "retcode": 0,
            "data": [{
                "group_id": 42,
                "user_id": 7,
                "nickname": "nick",
                "card": "card",
                "role": "admin",
                "join_time": 10,
                "last_sent_time": 20
            }],
            "echo": frame["echo"],
        }),
    );
    let members = members.await.unwrap().unwrap();
    assert_eq!(members.len(), 1);
    assert_eq!(members[0].user_id, "7");
    assert_eq!(members[0].display_name(), "card");
    assert_eq!(members[0].role, "admin");

    let member = {
        let adapter = adapter.clone();
        tokio::spawn(async move { adapter.group_member("8").await })
    };
    let frame: Value = serde_json::from_str(&frames.recv().await.unwrap()).unwrap();
    assert_eq!(frame["action"], "get_group_member_info");
    assert_eq!(frame["params"]["user_id"], 8);
    route_api_response(
        &handle,
        json!({
            "status": "ok",
            "retcode": 0,
            "data": { "group_id": 42, "user_id": 8, "nickname": "eight" },
            "echo": frame["echo"],
        }),
    );
    assert_eq!(member.await.unwrap().unwrap().unwrap().nickname, "eight");

    let info = {
        let adapter = adapter.clone();
        tokio::spawn(async move { adapter.message_info("91").await })
    };
    let frame: Value = serde_json::from_str(&frames.recv().await.unwrap()).unwrap();
    assert_eq!(frame["action"], "get_msg");
    route_api_response(
        &handle,
        json!({
            "status": "ok",
            "retcode": 0,
            "data": {
                "message_id": 91,
                "time": 123,
                "sender": { "user_id": 8, "nickname": "eight" },
                "message": [
                    { "type": "reply", "data": { "id": 80 } },
                    { "type": "at", "data": { "qq": 9 } },
                    { "type": "text", "data": { "text": "hello" } }
                ]
            },
            "echo": frame["echo"],
        }),
    );
    let info = info.await.unwrap().unwrap().unwrap();
    assert_eq!(info.message_id, "91");
    assert_eq!(info.sender_id, "8");
    assert_eq!(info.text, "hello");
    assert_eq!(info.reply_to_message_id.as_deref(), Some("80"));
    assert_eq!(info.mentioned_user_ids, vec!["9"]);
}
