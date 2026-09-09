//! 平台专属工具的注册与策略。

use super::shared::*;
use crate::platforms::*;
use serde_json::{json, Value};
use std::sync::atomic::Ordering as AtomicOrdering;

#[test]
fn host_tools_follow_admin_and_private_whitelist_policy() {
    let (_temp, mut context, _adapter) = test_turn_context(false);
    assert!(!context.host_tools_allowed());
    context.is_admin = true;
    assert!(context.host_tools_allowed());

    context.is_admin = false;
    context.config.platforms.qq.allow_non_admin_host_tools = true;
    assert!(!context.host_tools_allowed());
    let dynamic_key = access_control::global_grant_key(
        access_control::AccessPermission::PrivateWhitelist,
        "20000".to_string(),
    );
    let actor = crate::state::PlatformAccessActor {
        platform: "onebot".to_string(),
        account_id: "10000".to_string(),
        user_id: "42".to_string(),
        conversation_kind: "private".to_string(),
        conversation_id: "42".to_string(),
        message_id: "message-1".to_string(),
    };
    context
        .state_store
        .add_platform_access_grant(&dynamic_key, &actor)
        .unwrap();
    assert!(context.host_tools_allowed());
    context
        .state_store
        .remove_platform_access_grant(&dynamic_key, &actor)
        .unwrap();
    assert!(!context.host_tools_allowed());
    context
        .config
        .platforms
        .qq
        .private_chats
        .whitelist
        .push(20_000);
    assert!(context.host_tools_allowed());

    context.conversation.kind = ConversationKind::Group;
    assert!(!context.host_tools_allowed());
}

#[tokio::test]
async fn sponsor_writes_need_an_admin_but_refuse_softly() {
    let (_temp, mut context, _adapter) = test_turn_context(false);
    context.is_admin = false;
    let store = context.state_store.clone();
    let mut registry = crate::tools::ToolRegistry::new();
    register_platform_tools(&mut registry, Arc::new(context));
    let sponsor = registry.get("sponsor").expect("赞助工具在平台会话里注册");
    assert_eq!(sponsor.permission, crate::tools::ToolPermission::Writes);

    // 非管理员记账:要的是一句能转述的说明,不是 Err——报错会被模型当成工具
    // 故障,它会换个参数一直重试。
    let refused = registry
        .call(
            "sponsor",
            &json!({ "action": "add", "sponsor_id": "42", "amount": 10 }).to_string(),
        )
        .await
        .expect("软拒绝不该是 Err");
    let refused: Value = serde_json::from_str(&refused).unwrap();
    assert_eq!(refused["ok"], json!(false));
    assert_eq!(refused["refused"], json!(true));
    assert_eq!(
        store.sponsor_summary().unwrap().record_count,
        0,
        "被拒的调用不许落库"
    );

    // 看是开放的:群友问「榜上有谁」不该被挡。
    let listed = registry
        .call("sponsor", &json!({ "action": "list" }).to_string())
        .await
        .expect("非管理员也能看榜");
    let listed: Value = serde_json::from_str(&listed).unwrap();
    assert_eq!(listed["ok"], json!(true));
    assert_eq!(listed["leaderboard"], json!([]));
}

#[test]
fn untrusted_send_tool_schema_does_not_expose_local_attachments() {
    let (_temp, context, _adapter) = test_turn_context(false);
    let mut registry = crate::tools::ToolRegistry::new();
    register_platform_tools(&mut registry, Arc::new(context));
    let parameters = &registry.get("send_message_to_user").unwrap().parameters;

    assert!(parameters["properties"].get("text").is_some());
    // 非管理员也能发 Miyu 自产的生成图(执行侧按生图目录豁免校验),
    // 任意本地文件仍只对管理员开放。
    assert!(parameters["properties"].get("images").is_some());
    assert!(parameters["properties"].get("files").is_none());
}

#[tokio::test]
async fn usage_query_tool_reports_platform_history() {
    let (_temp, context, _adapter) = test_turn_context(false);
    context
        .state_store
        .add_usage(
            &crate::llm::Usage {
                prompt_tokens: 1000,
                completion_tokens: 200,
                total_tokens: 1200,
                cache_read_tokens: 400,
                ..crate::llm::Usage::default()
            },
            crate::state::UsageMeta {
                source: "onebot",
                provider: Some("prov"),
                model: Some("test-model"),
                kind: None,
            },
        )
        .unwrap();
    let mut registry = crate::tools::ToolRegistry::new();
    register_platform_tools(&mut registry, Arc::new(context));
    let output = registry
        .call("query_token_usage", r#"{"range":"7d"}"#)
        .await
        .unwrap();
    assert!(output.contains("**Token 消耗 · 近 7 天**"), "{output}");
    assert!(output.contains("**QQ**"), "{output}");
    assert!(output.contains("test-model"), "{output}");
    assert!(output.contains("缓存命中率 **40%**"), "{output}");
}

#[test]
fn multi_mention_tool_is_only_registered_for_group_turns() {
    let (_private_temp, private, _adapter) = test_turn_context(false);
    let mut private_tools = crate::tools::ToolRegistry::new();
    register_platform_tools(&mut private_tools, Arc::new(private));
    assert!(private_tools.get("qq_mention_users").is_none());

    let (_group_temp, mut group, _adapter) = test_turn_context(false);
    group.conversation.kind = ConversationKind::Group;
    let mut group_tools = crate::tools::ToolRegistry::new();
    register_platform_tools(&mut group_tools, Arc::new(group));
    assert!(group_tools.get("qq_mention_user").is_none());
    let tool = group_tools.get("qq_mention_users").unwrap();
    assert_eq!(tool.parameters["required"], serde_json::json!(["user_ids"]));
    assert_eq!(tool.parameters["additionalProperties"], false);
    assert_eq!(tool.parameters["properties"]["user_ids"]["minItems"], 1);
    assert_eq!(tool.parameters["properties"]["user_ids"]["maxItems"], 32);
    assert_eq!(
        tool.parameters["properties"]["user_ids"]["items"]["pattern"],
        "^[1-9][0-9]{4,11}$"
    );
}

#[tokio::test]
async fn multi_mention_tool_overrides_automatic_mention_without_sending_an_extra_message() {
    let (_temp, mut context, adapter) = test_turn_context(false);
    context.conversation.kind = ConversationKind::Group;
    let context = Arc::new(context);
    context.set_response_target(Some(ResponseTarget {
        message_id: "message-1".to_string(),
        user_id: "20000".to_string(),
        quote: true,
        mention: true,
        explicit_mention_user_ids: Vec::new(),
    }));
    let mut registry = crate::tools::ToolRegistry::new();
    register_platform_tools(&mut registry, context.clone());

    registry
        .call("qq_mention_users", r#"{"user_ids":["50000"]}"#)
        .await
        .unwrap();
    let output = registry
        .call(
            "qq_mention_users",
            r#"{"user_ids":["30000","40000","30000"]}"#,
        )
        .await
        .unwrap();
    let output: Value = serde_json::from_str(&output).unwrap();
    assert_eq!(output["user_ids"], serde_json::json!(["30000", "40000"]));
    assert_eq!(adapter.calls.load(AtomicOrdering::Relaxed), 0);
    assert_eq!(
        context.response_target(),
        Some(ResponseTarget {
            message_id: "message-1".to_string(),
            user_id: "20000".to_string(),
            quote: true,
            mention: false,
            explicit_mention_user_ids: vec!["30000".to_string(), "40000".to_string()],
        })
    );

    context
        .send(OutboundMessage::text(OutboundOrigin::FinalReply, "你好"))
        .await
        .unwrap();
    assert_eq!(adapter.calls.load(AtomicOrdering::Relaxed), 1);
    assert!(context.response_target().is_none());
    let messages = adapter.messages.lock().unwrap();
    assert_eq!(
        messages[0].response_target,
        Some(ResponseTarget {
            message_id: "message-1".to_string(),
            user_id: "20000".to_string(),
            quote: true,
            mention: false,
            explicit_mention_user_ids: vec!["30000".to_string(), "40000".to_string()],
        })
    );
}

#[tokio::test]
async fn multi_mention_tool_preserves_the_adaptive_quote_policy() {
    let (_temp, mut context, adapter) = test_turn_context(false);
    context.conversation.kind = ConversationKind::Group;
    let context = Arc::new(context);
    context.set_adaptive_response_target(
        Some(ResponseTarget {
            message_id: "message-1".to_string(),
            user_id: "20000".to_string(),
            quote: true,
            mention: true,
            explicit_mention_user_ids: Vec::new(),
        }),
        AdaptiveResponseTargetPolicy::new(None, Instant::now(), 1, 0),
    );
    let mut registry = crate::tools::ToolRegistry::new();
    register_platform_tools(&mut registry, context.clone());

    registry
        .call("qq_mention_users", r#"{"user_ids":["30000"]}"#)
        .await
        .unwrap();
    context.set_adaptive_response_target(
        Some(ResponseTarget {
            message_id: "message-2".to_string(),
            user_id: "20000".to_string(),
            quote: true,
            mention: true,
            explicit_mention_user_ids: Vec::new(),
        }),
        AdaptiveResponseTargetPolicy::new(None, Instant::now(), 1, 0),
    );
    context
        .send(OutboundMessage::text(OutboundOrigin::FinalReply, "你好"))
        .await
        .unwrap();

    let messages = adapter.messages.lock().unwrap();
    assert_eq!(
        messages[0].response_target,
        Some(ResponseTarget {
            message_id: "message-2".to_string(),
            user_id: "20000".to_string(),
            quote: false,
            mention: false,
            explicit_mention_user_ids: vec!["30000".to_string()],
        })
    );
}

#[tokio::test]
async fn multi_mention_tool_rejects_invalid_or_excessive_targets() {
    let (_temp, mut context, adapter) = test_turn_context(false);
    context.conversation.kind = ConversationKind::Group;
    let mut registry = crate::tools::ToolRegistry::new();
    register_platform_tools(&mut registry, Arc::new(context));

    let error = registry
        .call("qq_mention_users", r#"{"user_ids":["all"]}"#)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("5-12 digit QQ ID"));

    let error = registry
        .call("qq_mention_users", r#"{"user_ids":["+30000"]}"#)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("5-12 digit QQ ID"));

    let error = registry
        .call("qq_mention_users", r#"{"user_ids":[" 30000 "]}"#)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("5-12 digit QQ ID"));

    let error = registry
        .call(
            "qq_mention_users",
            r#"{"user_ids":["30000"],"group_id":"99999"}"#,
        )
        .await
        .unwrap_err();
    assert!(error.to_string().contains("only user_ids"));

    let error = registry
        .call("qq_mention_users", r#"{"user_ids":["60000"]}"#)
        .await
        .unwrap_err();
    assert!(error
        .to_string()
        .contains("not members of the current group"));

    let error = registry
        .call("qq_mention_users", r#"{"user_ids":[]}"#)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("at least one QQ ID"));

    let user_ids = (1..=33).map(|id| id.to_string()).collect::<Vec<_>>();
    let arguments = serde_json::json!({ "user_ids": user_ids }).to_string();
    let error = registry
        .call("qq_mention_users", &arguments)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("at most 32 users"));
    assert_eq!(adapter.calls.load(AtomicOrdering::Relaxed), 0);
}

#[tokio::test]
async fn one_recall_tool_is_registered_for_every_qq_turn() {
    let (_private_temp, private) = built_in_test_context(ConversationKind::Private);
    private.prepare_turn("test".to_string()).await;
    let mut private_tools = crate::tools::ToolRegistry::new();
    register_platform_tools(&mut private_tools, private);
    assert!(private_tools.get("qq_withdraw_message").is_some());

    let (_group_temp, group) = built_in_test_context(ConversationKind::Group);
    group.prepare_turn("test".to_string()).await;
    let mut group_tools = crate::tools::ToolRegistry::new();
    register_platform_tools(&mut group_tools, group);
    assert!(group_tools.get("qq_withdraw_message").is_some());

    let (_member_temp, member_group) = built_in_test_context(ConversationKind::Group);
    member_group.set_plugin_value(
        "qq_group_management.bot_role",
        Value::String("member".to_string()),
    );
    let mut member_tools = crate::tools::ToolRegistry::new();
    register_platform_tools(&mut member_tools, member_group);
    assert!(member_tools.get("qq_withdraw_message").is_some());
}
