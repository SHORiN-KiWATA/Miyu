//! 撤回、上传、禁言等通知事件。

use super::shared::*;
use crate::platforms::onebot::*;

#[test]
fn group_upload_notice_becomes_a_file_history_event() {
    let event = json!({
        "post_type": "notice",
        "notice_type": "group_upload",
        "time": 1786691192,
        "self_id": 10000,
        "group_id": 130515,
        "user_id": 29313,
        "file": {
            "id": "/8b25e30e-8ee2-4223-9e30-fd45ee24c797",
            "name": "配置.txt",
            "size": 11035,
            "busid": 102
        }
    });
    let inbound = group_upload_event(&event).expect("group upload notice should parse");
    assert_eq!(inbound.kind, PlatformInboundEventKind::GroupFileUpload);
    assert_eq!(inbound.sender_id, "29313");
    assert_eq!(inbound.timestamp, 1786691192);
    assert_eq!(
        inbound.message_id,
        "group_file_8b25e30e-8ee2-4223-9e30-fd45ee24c797"
    );
    assert_eq!(inbound.media.len(), 1);
    assert_eq!(inbound.media[0].kind, PlatformMediaKind::File);
    assert_eq!(
        inbound.media[0].id.as_deref(),
        Some("/8b25e30e-8ee2-4223-9e30-fd45ee24c797")
    );
    assert_eq!(inbound.media[0].name.as_deref(), Some("配置.txt"));
    assert!(inbound.ingress_order.is_some());
}

#[test]
fn recall_notices_become_structured_inbound_events() {
    let event = json!({
        "post_type": "notice",
        "notice_type": "group_recall",
        "self_id": 10000,
        "group_id": 42,
        "user_id": 7,
        "operator_id": 8,
        "message_id": 99,
        "time": 123,
    });
    assert!(is_message_recall(&event));
    let recalled = recall_event(Target::Group { group_id: 42 }, &event, 7);
    assert_eq!(recalled.kind, PlatformInboundEventKind::MessageRecall);
    assert_eq!(recalled.conversation.account_id, "10000");
    assert_eq!(recalled.conversation.conversation_id, "42");
    assert_eq!(recalled.message_id, "99");
    assert_eq!(recalled.sender_id, "7");
    assert_eq!(recalled.operator_id.as_deref(), Some("8"));
    assert_eq!(recalled.timestamp, 123);

    assert!(!is_message_recall(&json!({
        "post_type": "notice",
        "notice_type": "group_increase"
    })));
}

#[test]
fn group_mute_cache_expires_and_isolates_bot_accounts() {
    let start = Instant::now();
    let mut cache = GroupMuteCache::default();
    cache.insert(
        (10_001, 42),
        BotSendAvailability::Muted,
        Duration::from_secs(5),
        start,
    );
    cache.insert(
        (10_002, 42),
        BotSendAvailability::Available,
        Duration::from_secs(5),
        start,
    );
    assert_eq!(
        cache.get((10_001, 42), start),
        Some(BotSendAvailability::Muted)
    );
    assert_eq!(
        cache.get((10_002, 42), start),
        Some(BotSendAvailability::Available)
    );
    assert_eq!(
        cache.get((10_001, 42), start + Duration::from_secs(5)),
        None
    );
}

#[test]
fn group_ban_notices_update_bot_and_whole_group_mute_state() {
    let self_id = 91_001;
    let group_id = 92_001;
    group_mute_cache().lock().unwrap().remove_account(self_id);

    update_group_ban_notice(&json!({
        "post_type": "notice",
        "notice_type": "group_ban",
        "sub_type": "ban",
        "self_id": self_id,
        "group_id": group_id,
        "user_id": self_id,
        "duration": 120
    }));
    assert_eq!(
        group_mute_cache()
            .lock()
            .unwrap()
            .get((self_id, group_id), Instant::now()),
        Some(BotSendAvailability::Muted)
    );

    update_group_ban_notice(&json!({
        "post_type": "notice",
        "notice_type": "group_ban",
        "sub_type": "lift_ban",
        "self_id": self_id,
        "group_id": group_id,
        "user_id": self_id,
        "duration": 0
    }));
    assert_eq!(
        group_mute_cache()
            .lock()
            .unwrap()
            .get((self_id, group_id), Instant::now()),
        Some(BotSendAvailability::Available)
    );

    update_group_ban_notice(&json!({
        "post_type": "notice",
        "notice_type": "group_ban",
        "sub_type": "ban",
        "self_id": self_id,
        "group_id": group_id,
        "user_id": 0,
        "duration": 0
    }));
    assert_eq!(
        group_mute_cache()
            .lock()
            .unwrap()
            .get((self_id, group_id), Instant::now()),
        Some(BotSendAvailability::Muted)
    );
    group_mute_cache().lock().unwrap().remove_account(self_id);
}

#[tokio::test]
async fn bot_send_availability_queries_self_once_and_uses_the_cache() {
    let (handle, mut frames) = test_connection(None);
    let adapter = Arc::new(test_adapter(handle.clone(), Target::Group { group_id: 42 }));
    group_mute_cache()
        .lock()
        .unwrap()
        .remove_account(adapter.self_id);
    let lookup = {
        let adapter = adapter.clone();
        tokio::spawn(async move { adapter.bot_send_availability().await })
    };
    let frame: Value = serde_json::from_str(&frames.recv().await.unwrap()).unwrap();
    assert_eq!(frame["action"], "get_group_member_info");
    assert_eq!(frame["params"]["group_id"], 42);
    assert_eq!(frame["params"]["user_id"], adapter.self_id);
    // 09-11:这一条走了上游缓存,提前解禁之后问到的还是旧的 shut_up_timestamp,
    // 她在群里哑了两小时二十分。没有这条断言的话回滚不会报红。
    assert_eq!(frame["params"]["no_cache"], true);
    route_api_response(
        &handle,
        json!({
            "status": "ok",
            "retcode": 0,
            "data": {
                "group_id": 42,
                "user_id": adapter.self_id,
                "shut_up_timestamp": unix_now() + 60
            },
            "echo": frame["echo"]
        }),
    );
    assert_eq!(lookup.await.unwrap().unwrap(), BotSendAvailability::Muted);
    assert_eq!(
        adapter.bot_send_availability().await.unwrap(),
        BotSendAvailability::Muted
    );
    assert!(frames.try_recv().is_err());
    group_mute_cache()
        .lock()
        .unwrap()
        .remove_account(adapter.self_id);
}

/// 12 小时的禁言只信到复查窗口为止:提前解禁而通知没收到时,最坏 5 分钟自愈,
/// 而不是一路哑到原定解禁时刻。查询侧与通知侧都要守住。
#[tokio::test]
async fn a_long_mute_is_only_trusted_until_the_recheck_window() {
    let (handle, mut frames) = test_connection(None);
    let adapter = Arc::new(test_adapter(handle.clone(), Target::Group { group_id: 43 }));
    let key = (adapter.self_id, 43);
    group_mute_cache()
        .lock()
        .unwrap()
        .remove_account(adapter.self_id);

    let lookup = {
        let adapter = adapter.clone();
        tokio::spawn(async move { adapter.bot_send_availability().await })
    };
    let frame: Value = serde_json::from_str(&frames.recv().await.unwrap()).unwrap();
    route_api_response(
        &handle,
        json!({
            "status": "ok",
            "retcode": 0,
            "data": {
                "group_id": 43,
                "user_id": adapter.self_id,
                "shut_up_timestamp": unix_now() + 12 * 60 * 60
            },
            "echo": frame["echo"]
        }),
    );
    assert_eq!(lookup.await.unwrap().unwrap(), BotSendAvailability::Muted);

    let now = Instant::now();
    assert_eq!(
        group_mute_cache().lock().unwrap().get(key, now),
        Some(BotSendAvailability::Muted)
    );
    assert_eq!(
        group_mute_cache()
            .lock()
            .unwrap()
            .get(key, now + GROUP_MUTE_MUTED_TTL + Duration::from_secs(1)),
        None,
        "禁言状态过了复查窗口必须重新问一次"
    );

    group_mute_cache()
        .lock()
        .unwrap()
        .remove_account(adapter.self_id);
    update_group_ban_notice(&json!({
        "post_type": "notice",
        "notice_type": "group_ban",
        "sub_type": "ban",
        "self_id": adapter.self_id,
        "group_id": 43,
        "user_id": adapter.self_id,
        "duration": 12 * 60 * 60
    }));
    let now = Instant::now();
    assert_eq!(
        group_mute_cache().lock().unwrap().get(key, now),
        Some(BotSendAvailability::Muted)
    );
    assert_eq!(
        group_mute_cache()
            .lock()
            .unwrap()
            .get(key, now + GROUP_MUTE_MUTED_TTL + Duration::from_secs(1)),
        None,
        "通知给的禁言时长同样只是上界"
    );
    group_mute_cache()
        .lock()
        .unwrap()
        .remove_account(adapter.self_id);
}
