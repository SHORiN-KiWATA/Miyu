//! 入站消息解析与各类上限。

use super::shared::*;
use crate::platforms::onebot::*;

#[test]
fn parses_segment_arrays_with_mixed_content() {
    let message = json!([
        { "type": "at", "data": { "qq": "10001" } },
        { "type": "text", "data": { "text": " 你好" } },
        { "type": "image", "data": { "file": "x.jpg", "url": "https://img.example/x.jpg" } },
        { "type": "image", "data": { "file": "base64://aGk=" } },
        { "type": "file", "data": { "file_id": "f1", "file_name": "报告.pdf" } },
        { "type": "reply", "data": { "id": "5" } },
    ]);
    let parsed = parse_message(Some(&message), None, 10001);
    assert!(parsed.at_self);
    assert_eq!(parsed.text, " 你好");
    assert_eq!(parsed.images.len(), 2);
    assert!(matches!(&parsed.images[0], MediaRef::Url(url) if url == "https://img.example/x.jpg"));
    assert!(matches!(&parsed.images[1], MediaRef::Bytes(bytes) if bytes == b"hi"));
    assert_eq!(parsed.files.len(), 1);
    assert_eq!(parsed.files[0].name, "报告.pdf");
    assert_eq!(parsed.files[0].file_id.as_deref(), Some("f1"));
    assert_eq!(parsed.reply_to_message_id.as_deref(), Some("5"));
    assert_eq!(parsed.mentioned_user_ids, vec!["10001"]);
    assert_eq!(parsed.media.len(), 3);
    assert_eq!(parsed.media[0].kind, PlatformMediaKind::Image);
    assert_eq!(parsed.media[2].kind, PlatformMediaKind::File);
    let inbound = message_event(
        Target::Group { group_id: 42 },
        &json!({
            "self_id": 10001,
            "user_id": 7,
            "message_id": 90
        }),
        &parsed,
    );
    assert!(inbound.mentioned_bot);

    // Someone else being @-ed does not wake the bot.
    let other = json!([{ "type": "at", "data": { "qq": "999" } }]);
    assert!(!parse_message(Some(&other), None, 10001).at_self);
}

#[test]
fn ingress_history_event_uses_bound_account_and_supports_private_messages() {
    let frame = json!({
        "post_type": "message",
        "message_type": "private",
        "user_id": 42,
        "message_id": 90,
        "time": 123,
        "sender": { "nickname": "Alice" },
        "message": [
            { "type": "text", "data": { "text": "hello" } },
            { "type": "image", "data": { "file": "photo.jpg" } }
        ]
    });

    let inbound = ingress_message_event(&frame, 10001, 7, None).unwrap();
    assert_eq!(inbound.conversation.account_id, "10001");
    assert_eq!(inbound.conversation.kind, ConversationKind::Private);
    assert_eq!(inbound.conversation.conversation_id, "42");
    assert_eq!(inbound.ingress_order, Some(7));
    assert_eq!(inbound.text, "hello");
    assert_eq!(inbound.media.len(), 1);

    let bot_echo = json!({
        "post_type": "message",
        "message_type": "private",
        "user_id": 10001,
        "message_id": 91,
        "message": "echo"
    });
    assert!(ingress_message_event(&bot_echo, 10001, 8, None).is_none());
}

#[test]
fn cq_string_images_use_the_same_model_input_parser_as_segment_arrays() {
    let message = json!(
        "说明[CQ:image,file=https://img.example/a.png,url=https://img.example/a&#44;b.png][CQ:image,file=base64://aGk=]"
    );
    let parsed = parse_message(Some(&message), None, 10001);

    assert_eq!(parsed.text, "说明");
    assert_eq!(parsed.images.len(), 2);
    assert!(matches!(
        &parsed.images[0],
        MediaRef::Url(url) if url == "https://img.example/a,b.png"
    ));
    assert!(matches!(&parsed.images[1], MediaRef::Bytes(bytes) if bytes == b"hi"));
    assert_eq!(parsed.media.len(), 2);
    assert!(parsed
        .media
        .iter()
        .all(|media| media.kind == PlatformMediaKind::Image));
    let mention = json!("[CQ:at,qq=10001]你好");
    let parsed = parse_message(Some(&mention), None, 10001);
    assert!(parsed.at_self);
    let inbound = message_event(
        Target::Group { group_id: 42 },
        &json!({ "self_id": 10001, "user_id": 7, "message_id": 91 }),
        &parsed,
    );
    assert!(inbound.mentioned_bot);
}

#[test]
fn ordered_history_image_sources_preserve_duplicate_positions() {
    let message = json!([
        { "type": "image", "data": { "file": "base64://AQID" } },
        { "type": "image", "data": { "file": "base64://AQID" } }
    ]);

    let sources = ordered_message_image_sources(Some(&message), None);
    assert_eq!(sources.len(), 2);
    assert!(matches!(
        &sources[0],
        OrderedMessageImageSource::Media(MediaRef::Bytes(bytes)) if bytes == &[1, 2, 3]
    ));
    assert!(matches!(
        &sources[1],
        OrderedMessageImageSource::Media(MediaRef::Bytes(bytes)) if bytes == &[1, 2, 3]
    ));
}

#[test]
fn image_reference_budget_deduplicates_and_caps_total_inline_bytes() {
    let mut images = Vec::new();
    assert!(push_image_ref_with_limits(
        &mut images,
        MediaRef::Bytes(vec![1, 2, 3]),
        4,
        5,
    ));
    assert!(!push_image_ref_with_limits(
        &mut images,
        MediaRef::Bytes(vec![1, 2, 3]),
        4,
        5,
    ));
    assert!(!push_image_ref_with_limits(
        &mut images,
        MediaRef::Bytes(vec![4, 5, 6]),
        4,
        5,
    ));
    assert!(push_image_ref_with_limits(
        &mut images,
        MediaRef::Url("https://img.example/a.png".to_string()),
        4,
        5,
    ));
    assert!(!push_image_ref_with_limits(
        &mut images,
        MediaRef::Url("https://img.example/a.png".to_string()),
        4,
        5,
    ));
    assert_eq!(images.len(), 2);
}

#[tokio::test]
async fn prepared_images_become_binary_attachments_and_deduplicate_content() {
    let temp = tempfile::tempdir().unwrap();
    let state = test_web_state(temp.path(), 8300);
    let png = vec![0x89, b'P', b'N', b'G', 1];
    let prepared = prepare_inbound_images(
        &state,
        vec![
            MediaRef::Bytes(png.clone()),
            MediaRef::Bytes(png),
            MediaRef::Bytes(vec![0xFF, 0xD8, 0xFF, 2]),
        ],
    )
    .await
    .unwrap();

    assert_eq!(prepared.attempted, 3);
    assert_eq!(prepared.attachments.len(), 2);
    assert_eq!(prepared.duplicates, 1);
    assert_eq!(prepared.failed, 0);
    assert_eq!(prepared.total_bytes, 9);
    assert!(matches!(
        &prepared.attachments[0],
        Some(ImageAttachment::Binary { mime, data })
            if mime == "image/png" && data.starts_with(&[0x89, b'P', b'N', b'G'])
    ));
    assert!(matches!(
        &prepared.attachments[1],
        Some(ImageAttachment::Binary { mime, .. }) if mime == "image/jpeg"
    ));
}

/// 视频段(NapCat: file/url/file_id/file_size)要像文件一样进 `files`,否则当轮
/// 正文里没有 id,模型不知道有段视频可看(09-04「miyu 看不了别人发的视频」)。
#[test]
fn video_segments_become_lazy_file_refs() {
    let message = json!([
        { "type": "video", "data": {
            "file": "a1b2c3.mp4",
            "url": "https://multimedia.nt.qq.com.cn/download?x=1",
            "file_id": "vid-1",
            "file_size": "123456"
        } },
        { "type": "video", "data": { "file": "noext", "file_id": "vid-2" } },
        { "type": "video", "data": { "file": "orphan.mp4" } },
    ]);
    let parsed = parse_message(Some(&message), None, 10001);
    assert_eq!(
        parsed.files.len(),
        2,
        "没有 id 也没有 url 的视频无法下载,不进 files"
    );
    assert_eq!(parsed.files[0].name, "a1b2c3.mp4");
    assert_eq!(parsed.files[0].file_id.as_deref(), Some("vid-1"));
    assert_eq!(
        parsed.files[0].url.as_deref(),
        Some("https://multimedia.nt.qq.com.cn/download?x=1")
    );
    assert_eq!(
        parsed.files[1].name, "noext.mp4",
        "无扩展名补 .mp4,下游靠扩展名认视频"
    );
    assert_eq!(parsed.media.len(), 3);
    assert!(parsed
        .media
        .iter()
        .all(|media| media.kind == PlatformMediaKind::Video));
    assert_eq!(parsed.media[0].name.as_deref(), Some("a1b2c3.mp4"));

    let (text, refs) = inbound_file_placeholders("77", &parsed.files);
    assert!(
        text.contains("[视频 id=file_77_1, label=a1b2c3.mp4]")
            || text.contains("[video id=file_77_1, label=a1b2c3.mp4]"),
        "{text}"
    );
    assert_eq!(refs.len(), 2);
    assert_eq!(refs[0].file_id, "vid-1");
    assert_eq!(refs[1].id, "file_77_2");
}

#[test]
fn inbound_file_placeholders_are_lazy_and_carry_provider_refs() {
    let files = vec![FileRef {
        file_id: Some("/file-id".to_string()),
        name: "README.md".to_string(),
        url: Some("https://example.invalid/README.md".to_string()),
    }];
    let (text, refs) = inbound_file_placeholders("msg-1", &files);
    assert!(text.contains("file_msg-1_1"));
    assert!(text.contains("README.md"));
    assert_eq!(refs.len(), 1);
    assert_eq!(refs[0].file_id, "/file-id");
    assert_eq!(
        refs[0].url.as_deref(),
        Some("https://example.invalid/README.md")
    );
}

#[test]
fn falls_back_to_raw_string_messages() {
    let message = json!("纯文本消息");
    let parsed = parse_message(Some(&message), None, 1);
    assert_eq!(parsed.text, "纯文本消息");

    let raw = json!("raw 兜底");
    let parsed = parse_message(None, Some(&raw), 1);
    assert_eq!(parsed.text, "raw 兜底");

    let reply_command = json!("[CQ:reply,id=5][CQ:at,qq=10001] /reset");
    let parsed = parse_message(Some(&reply_command), None, 10001);
    assert!(parsed.at_self);
    assert_eq!(parsed.text, " /reset");
    assert_eq!(parsed.reply_to_message_id.as_deref(), Some("5"));
    assert_eq!(parsed.mentioned_user_ids, vec!["10001"]);
    assert_eq!(
        commands::parse(&crate::config::PlatformsConfig::default(), &parsed.text),
        Some(commands::ParsedPlatformCommand::Reset {
            scope: Some(commands::ResetScope::Current)
        })
    );

    let escaped_literal = json!("&#91;CQ:reply,id=5&#93;/reset");
    let parsed = parse_message(Some(&escaped_literal), None, 1);
    assert_eq!(parsed.text, "[CQ:reply,id=5]/reset");
}

#[test]
fn inbound_parser_caps_media_segment_counts() {
    let message = Value::Array(
        (0..8)
            .flat_map(|index| {
                [
                    json!({
                        "type": "image",
                        "data": { "url": format!("https://img.example/{index}.png") }
                    }),
                    json!({
                        "type": "file",
                        "data": { "file_id": format!("f{index}"), "file_name": "x.txt" }
                    }),
                ]
            })
            .collect(),
    );
    let parsed = parse_message(Some(&message), None, 1);
    assert_eq!(parsed.images.len(), MAX_INBOUND_IMAGES);
    assert_eq!(parsed.files.len(), MAX_INBOUND_FILES);
}

#[test]
fn inbound_parser_rejects_oversized_text_and_segment_arrays_early() {
    let oversized = json!([{
        "type": "text",
        "data": { "text": "界".repeat(MAX_INBOUND_TEXT_CHARS + 1) }
    }]);
    let parsed = parse_message(Some(&oversized), None, 1);
    assert!(parsed.rejected_reason.is_some());
    assert_eq!(parsed.text.chars().count(), MAX_INBOUND_TEXT_CHARS);

    let too_many = Value::Array(
        (0..=MAX_INBOUND_SEGMENTS)
            .map(|_| json!({ "type": "text", "data": { "text": "x" } }))
            .collect(),
    );
    let parsed = parse_message(Some(&too_many), None, 1);
    assert_eq!(
        parsed.rejected_reason,
        Some("message has too many OneBot segments")
    );
}

#[test]
fn inbound_mentions_are_bounded_and_non_numeric_targets_are_ignored() {
    let mut segments = (1..=MAX_INBOUND_MENTIONS + 8)
        .map(|id| json!({ "type": "at", "data": { "qq": id.to_string() } }))
        .collect::<Vec<_>>();
    segments.push(json!({ "type": "at", "data": { "qq": "all" } }));
    let parsed = parse_message(Some(&Value::Array(segments)), None, 99_999);
    assert_eq!(parsed.mentioned_user_ids.len(), MAX_INBOUND_MENTIONS);
    assert!(parsed
        .mentioned_user_ids
        .iter()
        .all(|id| id.bytes().all(|byte| byte.is_ascii_digit())));
}
