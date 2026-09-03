pub(super) mod store;
pub(super) mod tools;

use super::{PlatformPlugin, PluginDescriptor, PreparedSend};
use crate::config::{QqMessageHistoryPluginSettings, QQ_MESSAGE_HISTORY_PLUGIN_ID};
use crate::paths::MiyuPaths;
use crate::platforms::{
    ConversationKind, OutboundBody, OutboundMessage, OutboundSegment, PlatformInboundEvent,
    PlatformInboundEventKind, PlatformMediaKind, PlatformTurnContext, SendReceipt,
};
use crate::tools::ToolRegistry;
use anyhow::Result;
use futures_util::future::BoxFuture;
use serde_json::Value;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};
use store::{
    AccountKey, ConversationKey, GroupKey, HistoryStore, MediaKind, MediaPlaceholder,
    NewHistoryMessage, NewRecall, SanitizedContent,
};

pub(super) const PLUGIN_ID: &str = QQ_MESSAGE_HISTORY_PLUGIN_ID;
pub(super) const ORIGINAL_TEXT_KEY: &str = "message_history.original_text";
const HISTORY_DB: &str = "platforms/onebot/message_history/history.sqlite3";
const LEGACY_HISTORY_DB: &str = "platforms/onebot/real_context/history.sqlite3";

fn stores() -> &'static Mutex<HashMap<PathBuf, HistoryStore>> {
    static STORES: OnceLock<Mutex<HashMap<PathBuf, HistoryStore>>> = OnceLock::new();
    STORES.get_or_init(|| Mutex::new(HashMap::new()))
}

pub(super) fn store_for_paths(paths: &MiyuPaths) -> HistoryStore {
    let path = history_db_path(paths);
    let mut stores = stores().lock().unwrap();
    stores
        .entry(path.clone())
        .or_insert_with(|| HistoryStore::new(path))
        .clone()
}

fn history_db_path(paths: &MiyuPaths) -> PathBuf {
    let path = paths.data_dir.join(HISTORY_DB);
    if path.exists() {
        return path;
    }
    let legacy = paths.data_dir.join(LEGACY_HISTORY_DB);
    if !legacy.exists() {
        return path;
    }
    let Some(parent) = path.parent() else {
        return legacy;
    };
    if let Err(error) = copy_legacy_history(&legacy, &path, parent) {
        tracing::warn!(
            error = %error,
            legacy = %legacy.display(),
            current = %path.display(),
            "{}",
            crate::i18n::text(
                "unable to move the legacy message history database; keeping its path",
                "无法移动旧消息历史数据库，将继续使用旧路径"
            )
        );
        legacy
    } else {
        path
    }
}

fn copy_legacy_history(
    legacy: &std::path::Path,
    path: &std::path::Path,
    parent: &std::path::Path,
) -> std::io::Result<()> {
    std::fs::create_dir_all(parent)?;
    let files = ["-wal", "-shm", ""]
        .into_iter()
        .map(|suffix| (sqlite_file(legacy, suffix), sqlite_file(path, suffix)))
        .filter(|(source, _)| source.exists())
        .collect::<Vec<_>>();
    for (source, target) in &files {
        if let Err(error) = std::fs::copy(source, target) {
            for (_, copied) in &files {
                let _ = std::fs::remove_file(copied);
            }
            return Err(error);
        }
    }
    for (source, _) in files {
        let _ = std::fs::remove_file(source);
    }
    Ok(())
}

fn sqlite_file(path: &std::path::Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(suffix);
    PathBuf::from(name)
}

pub(super) struct MessageHistoryPlugin {}

/// 把正文里的 `<qq-forward>…</qq-forward>` 整块换成一条有界摘要。
///
/// 返回 `None` = 没有转发块,正文原样入库。摘要形如
/// `[forward x3] 阿: 外层一句 / 布布: [image] / 策: 里层一句`,截到
/// `MAX_FORWARD_DIGEST_CHARS` 为止。当轮投喂给模型的仍是完整展开——这里只管
/// 落库的那一份。
fn forward_digest(text: &str) -> Option<String> {
    const OPEN: &str = "\n<qq-forward>\n";
    const CLOSE: &str = "</qq-forward>";
    const MAX_FORWARD_DIGEST_CHARS: usize = 160;
    let open = text.find(OPEN)?;
    let close = text[open..].find(CLOSE)? + open;
    let body = &text[open + OPEN.len()..close];
    let lines = body
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && *line != "(forwarded content truncated)")
        .collect::<Vec<_>>();
    let summary = format!("[forward x{}] {}", lines.len(), lines.join(" / "));
    let summary = if summary.chars().count() > MAX_FORWARD_DIGEST_CHARS {
        format!(
            "{}…",
            summary
                .chars()
                .take(MAX_FORWARD_DIGEST_CHARS)
                .collect::<String>()
        )
    } else {
        summary
    };
    let mut digested = String::with_capacity(text.len());
    digested.push_str(&text[..open]);
    if !digested.is_empty() && !digested.ends_with(char::is_whitespace) {
        digested.push(' ');
    }
    digested.push_str(&summary);
    digested.push_str(&text[close + CLOSE.len()..]);
    Some(digested)
}

impl MessageHistoryPlugin {
    pub(super) fn new() -> Self {
        Self {}
    }

    fn settings(context: &PlatformTurnContext) -> Result<Arc<QqMessageHistoryPluginSettings>> {
        Ok(Arc::new(
            context
                .config
                .platforms
                .qq
                .plugins
                .get(QQ_MESSAGE_HISTORY_PLUGIN_ID)
                .map(QqMessageHistoryPluginSettings::from_instance)
                .transpose()?
                .unwrap_or_default(),
        ))
    }

    async fn record_inbound(paths: &MiyuPaths, event: &PlatformInboundEvent) -> Result<()> {
        let key = ConversationKey::for_kind(
            event.conversation.platform.clone(),
            event.conversation.account_id.clone(),
            event.conversation.kind,
            event.conversation.conversation_id.clone(),
        )?;
        let store = store_for_paths(paths);
        match event.kind {
            PlatformInboundEventKind::Message | PlatformInboundEventKind::GroupFileUpload => {
                // Media has to survive into history, or an image-only message
                // renders later as "[无文字内容]" and the model cannot tell that
                // a picture was ever posted — nor can it be handed a
                // `context_image_N` id to look at.
                let media = event
                    .media
                    .iter()
                    .map(|media| {
                        let placeholder = MediaPlaceholder::new(
                            media_kind(media.kind),
                            media.name.clone().or_else(|| media.id.clone()),
                            None::<String>,
                        );
                        if media.kind == PlatformMediaKind::File {
                            placeholder.with_media_id(media.id.clone())
                        } else {
                            placeholder
                        }
                    })
                    .collect();
                // 合并转发在当轮完整投喂给模型,历史库只留有界摘要:一条转发
                // 动辄上千字,全量入库会把群历史撑大好几倍(08-26 用户要求)。
                let digested = forward_digest(&event.text);
                let stored_text = digested.clone().unwrap_or_else(|| event.text.clone());
                let mut content = SanitizedContent::new(stored_text.clone(), media);
                content.mentioned_user_ids = event.mentioned_user_ids.clone();
                content.mentioned_users = event.mentioned_users.clone();
                store
                    .record_message(NewHistoryMessage {
                        group: key.clone(),
                        message_id: event_message_id(event),
                        sender_id: event.sender_id.clone(),
                        sender_name: event.sender_display_name.clone(),
                        content,
                        reply_to_message_id: event.reply_to_message_id.clone(),
                        is_bot: false,
                        sent_at: normalized_timestamp(event.timestamp),
                        ingress_order: event.ingress_order,
                    })
                    .await?;
                // 连接层的 observe_ingress 抢在展开之前就写过这条了(那时只有
                // 一个 `[forward]` 记号),而写入是 INSERT OR IGNORE,上面这次
                // 会被静默丢弃。摘要靠一次显式补写落库。
                if digested.is_some() {
                    store
                        .update_message_text(key.clone(), event_message_id(event), stored_text)
                        .await?;
                }
            }
            PlatformInboundEventKind::MessageRecall => {
                store
                    .record_recall(NewRecall {
                        group: key,
                        message_id: event_message_id(event),
                        operator_id: event.operator_id.clone(),
                        recalled_at: normalized_timestamp(event.timestamp),
                    })
                    .await?;
            }
            PlatformInboundEventKind::GroupBan | PlatformInboundEventKind::GroupDecrease => {}
        }
        Ok(())
    }

    async fn record_outbound(
        context: &PlatformTurnContext,
        message: &OutboundMessage,
        receipt: &SendReceipt,
    ) -> Result<()> {
        let text = message
            .metadata
            .get(ORIGINAL_TEXT_KEY)
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| outbound_text(message));
        let timestamp = now_unix();
        let message_id = receipt
            .message_ids
            .first()
            .cloned()
            .unwrap_or_else(|| format!("miyu-{timestamp}-{:08x}", rand::random::<u32>()));
        let sender_name = context
            .bot_display_name()
            .await
            .unwrap_or_else(|_| "Miyu".to_string());
        store_for_paths(&context.paths)
            .record_message(NewHistoryMessage {
                group: conversation_key(context)?,
                message_id,
                sender_id: context.conversation.account_id.clone(),
                sender_name,
                content: SanitizedContent::new(text, Vec::new()),
                reply_to_message_id: message
                    .response_target
                    .as_ref()
                    .filter(|target| target.quote)
                    .map(|target| target.message_id.clone()),
                is_bot: true,
                sent_at: timestamp,
                ingress_order: context
                    .inbound_event()
                    .and_then(|event| event.ingress_order),
            })
            .await?;
        Ok(())
    }
}

impl PlatformPlugin for MessageHistoryPlugin {
    fn descriptor(&self) -> PluginDescriptor {
        PluginDescriptor {
            id: PLUGIN_ID,
            priority: 300,
            default_enabled: true,
        }
    }

    fn observe_ingress<'a>(
        &'a self,
        paths: &'a MiyuPaths,
        _config: &'a crate::config::AppConfig,
        event: &'a PlatformInboundEvent,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move { Self::record_inbound(paths, event).await })
    }

    fn register_tools(
        &self,
        registry: &mut ToolRegistry,
        context: Arc<PlatformTurnContext>,
    ) -> Result<()> {
        tools::register(
            registry,
            context.clone(),
            store_for_paths(&context.paths),
            Self::settings(&context)?,
        );
        Ok(())
    }

    fn observe_inbound<'a>(
        &'a self,
        context: &'a PlatformTurnContext,
        event: &'a PlatformInboundEvent,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move { Self::record_inbound(&context.paths, event).await })
    }

    fn before_send<'a>(
        &'a self,
        _context: &'a PlatformTurnContext,
        mut message: OutboundMessage,
    ) -> BoxFuture<'a, Result<PreparedSend>> {
        Box::pin(async move {
            if !message.metadata.contains_key(ORIGINAL_TEXT_KEY) {
                message.metadata.insert(
                    ORIGINAL_TEXT_KEY.to_string(),
                    Value::String(outbound_text(&message)),
                );
            }
            Ok(PreparedSend::unchanged(message))
        })
    }

    fn after_send<'a>(
        &'a self,
        context: &'a PlatformTurnContext,
        message: &'a OutboundMessage,
        receipt: &'a SendReceipt,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move { Self::record_outbound(context, message, receipt).await })
    }

    fn record_external_bot_message<'a>(
        &'a self,
        context: &'a PlatformTurnContext,
        message_id: &'a str,
        text: &'a str,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let timestamp = now_unix();
            let sender_name = context
                .bot_display_name()
                .await
                .unwrap_or_else(|_| "Miyu".to_string());
            store_for_paths(&context.paths)
                .record_message(NewHistoryMessage {
                    group: conversation_key(context)?,
                    message_id: if message_id.trim().is_empty() {
                        format!("miyu-{timestamp}-{:08x}", rand::random::<u32>())
                    } else {
                        message_id.to_string()
                    },
                    sender_id: context.conversation.account_id.clone(),
                    sender_name,
                    content: SanitizedContent::new(text, Vec::new()),
                    reply_to_message_id: context
                        .inbound_event()
                        .map(|event| event.message_id.clone()),
                    is_bot: true,
                    sent_at: timestamp,
                    ingress_order: context
                        .inbound_event()
                        .and_then(|event| event.ingress_order),
                })
                .await?;
            Ok(())
        })
    }
}

pub(super) fn conversation_key(context: &PlatformTurnContext) -> Result<ConversationKey> {
    ConversationKey::for_kind(
        context.conversation.platform.clone(),
        context.conversation.account_id.clone(),
        context.conversation.kind,
        context.conversation.conversation_id.clone(),
    )
}

pub(super) fn group_key(context: &PlatformTurnContext) -> Result<GroupKey> {
    group_key_for(context, &context.conversation.conversation_id)
}

pub(super) fn group_key_for(context: &PlatformTurnContext, group_id: &str) -> Result<GroupKey> {
    ConversationKey::for_kind(
        context.conversation.platform.clone(),
        context.conversation.account_id.clone(),
        ConversationKind::Group,
        group_id.to_string(),
    )
}

pub(super) fn account_key(context: &PlatformTurnContext) -> Result<AccountKey> {
    AccountKey::new(
        context.conversation.platform.clone(),
        context.conversation.account_id.clone(),
    )
}

pub(super) fn register_group_member_tool(
    registry: &mut ToolRegistry,
    context: Arc<PlatformTurnContext>,
    maximum: usize,
) {
    tools::register_group_members(registry, context.clone(), maximum);
    tools::register_avatar(registry, context);
}

/// 私聊等非群会话也要有 get_avatar(08-22 用户实测:私聊里模型答"没有这个
/// 工具")。群成员搜索仍是群聊专属,头像不是。
pub(super) fn register_avatar_tool(registry: &mut ToolRegistry, context: Arc<PlatformTurnContext>) {
    tools::register_avatar(registry, context);
}

fn event_message_id(event: &PlatformInboundEvent) -> String {
    if !event.message_id.trim().is_empty() {
        return event.message_id.clone();
    }
    event
        .ingress_order
        .map(|order| format!("ingress-{order}"))
        .unwrap_or_else(|| format!("ingress-{}-{:08x}", now_unix(), rand::random::<u32>()))
}

fn outbound_text(message: &OutboundMessage) -> String {
    let mut parts = Vec::new();
    match &message.body {
        OutboundBody::Segments(segments) => append_segment_text(&mut parts, segments),
        OutboundBody::Forward(nodes) => {
            for node in nodes {
                append_segment_text(&mut parts, &node.segments);
            }
        }
    }
    parts.join("\n").trim().to_string()
}

fn append_segment_text(parts: &mut Vec<String>, segments: &[OutboundSegment]) {
    for segment in segments {
        match segment {
            OutboundSegment::Markdown(text) | OutboundSegment::Text(text) => {
                if !text.trim().is_empty() {
                    parts.push(text.clone());
                }
            }
            OutboundSegment::Mention(user_id) => parts.push(format!("@{user_id}")),
            OutboundSegment::ImageBytes { .. }
            | OutboundSegment::ImagePath { .. }
            | OutboundSegment::FilePath { .. } => {}
        }
    }
}

fn normalized_timestamp(timestamp: i64) -> i64 {
    if timestamp > 0 {
        timestamp
    } else {
        now_unix()
    }
}

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

/// Emoji becomes a sticker rather than an image on purpose: only `Image` earns
/// a `context_image_N` id, and a face sticker is not worth a vision call.
fn media_kind(kind: PlatformMediaKind) -> MediaKind {
    match kind {
        PlatformMediaKind::Image => MediaKind::Image,
        PlatformMediaKind::Emoji => MediaKind::Sticker,
        PlatformMediaKind::File => MediaKind::File,
        PlatformMediaKind::Audio => MediaKind::Audio,
        PlatformMediaKind::Video => MediaKind::Video,
        PlatformMediaKind::Other => MediaKind::Other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 转发块入库前收成有界摘要:模型当轮拿的是完整展开,历史库只留这一条。
    #[test]
    fn forward_blocks_shrink_to_a_bounded_digest() {
        assert_eq!(forward_digest("普通消息"), None);

        let expanded =
            "看这个\n<qq-forward>\n阿: 外层一句\n布布: [image]\n  策: 里层一句\n</qq-forward>";
        let digest = forward_digest(expanded).expect("有转发块");
        assert_eq!(
            digest,
            "看这个 [forward x3] 阿: 外层一句 / 布布: [image] / 策: 里层一句"
        );
        // 原正文保留,展开块不再出现。
        assert!(!digest.contains("<qq-forward>"));

        // 超长转发被截断,且远小于当轮投喂的完整展开。
        let long = format!(
            "\n<qq-forward>\n{}</qq-forward>",
            (0..80)
                .map(|i| format!("发送者{i}: 这是一条相当长的转发内容用来撑爆预算\n"))
                .collect::<String>()
        );
        let digest = forward_digest(&long).expect("有转发块");
        assert!(digest.chars().count() <= 161, "{}", digest.chars().count());
        assert!(digest.chars().count() * 8 < long.chars().count());
        assert!(digest.ends_with('…'));

        // 截断标记不占摘要条数。
        let truncated = "\n<qq-forward>\n阿: 一\n(forwarded content truncated)\n</qq-forward>";
        assert_eq!(forward_digest(truncated).unwrap(), "[forward x1] 阿: 一");
    }

    use crate::platforms::{
        PlatformConversation, PlatformInboundMedia, PlatformMediaKind, PlatformMessagePosition,
    };
    use std::time::Instant;

    fn test_paths(root: &std::path::Path) -> MiyuPaths {
        MiyuPaths {
            root_dir: root.to_path_buf(),
            config_dir: root.join("config"),
            config_file: root.join("config/config.jsonc"),
            skills_dir: root.join("config/skills"),
            data_dir: root.join("data"),
            cache_dir: root.join("cache"),
            state_dir: root.join("state"),
            pictures_dir: root.join("pictures"),
            fish_hook_file: root.join("fish"),
            bash_hook_file: root.join("bash"),
            zsh_hook_file: root.join("zsh"),
            scripts_dir: root.join("scripts"),
            system_scripts_dir: root.join("system-scripts"),
        }
    }

    #[tokio::test]
    async fn inbound_private_messages_are_idempotent_and_text_only() {
        let temp = tempfile::tempdir().unwrap();
        let paths = test_paths(temp.path());
        let event = PlatformInboundEvent {
            kind: PlatformInboundEventKind::Message,
            conversation: PlatformConversation {
                platform: "onebot".to_string(),
                account_id: "10000".to_string(),
                kind: ConversationKind::Private,
                conversation_id: "42".to_string(),
            },
            conversation_display_name: None,
            message_id: "123".to_string(),
            sender_id: "42".to_string(),
            sender_display_name: "Alice".to_string(),
            operator_id: None,
            timestamp: 100,
            received_at: Instant::now(),
            message_position: Some(PlatformMessagePosition {
                total_messages: 1,
                sender_messages: 1,
            }),
            ingress_order: Some(1),
            text: "hello".to_string(),
            reply_to_message_id: None,
            replied_message: None,
            mentioned_user_ids: Vec::new(),
            mentioned_users: Vec::new(),
            mentioned_bot: false,
            media: vec![PlatformInboundMedia {
                kind: PlatformMediaKind::Image,
                id: Some("image-id".to_string()),
                name: Some("photo.png".to_string()),
                url: Some("https://example.invalid/photo.png".to_string()),
            }],
            notice_sub_type: None,
            duration_seconds: None,
        };

        MessageHistoryPlugin::record_inbound(&paths, &event)
            .await
            .unwrap();
        MessageHistoryPlugin::record_inbound(&paths, &event)
            .await
            .unwrap();
        let page = store_for_paths(&paths)
            .recent(store::RecentQuery::for_history(private("10000", "42"), 20))
            .await
            .unwrap();
        assert_eq!(page.messages.len(), 1);
        assert_eq!(page.messages[0].content.text, "hello");
        // Media must survive into history: without it an image-only message
        // renders as "[无文字内容]" later and never earns a context_image id,
        // so the model cannot be told which picture is being asked about.
        let media = &page.messages[0].content.media;
        assert_eq!(media.len(), 1);
        assert_eq!(media[0].kind, MediaKind::Image);
        assert_eq!(media[0].label.as_deref(), Some("photo.png"));
    }

    #[test]
    fn a_face_sticker_is_not_an_image() {
        // Only Image earns a context_image id downstream; mapping Emoji to
        // Image would offer the model face stickers to run vision on.
        assert_eq!(media_kind(PlatformMediaKind::Emoji), MediaKind::Sticker);
        assert_eq!(media_kind(PlatformMediaKind::Image), MediaKind::Image);
    }

    #[test]
    fn legacy_database_files_are_moved_as_one_set() {
        let temp = tempfile::tempdir().unwrap();
        let paths = test_paths(temp.path());
        let legacy = paths.data_dir.join(LEGACY_HISTORY_DB);
        let current = paths.data_dir.join(HISTORY_DB);
        std::fs::create_dir_all(legacy.parent().unwrap()).unwrap();
        std::fs::write(&legacy, b"database").unwrap();
        std::fs::write(sqlite_file(&legacy, "-wal"), b"wal").unwrap();
        std::fs::write(sqlite_file(&legacy, "-shm"), b"shm").unwrap();

        assert_eq!(history_db_path(&paths), current);
        assert_eq!(std::fs::read(&current).unwrap(), b"database");
        assert_eq!(
            std::fs::read(sqlite_file(&current, "-wal")).unwrap(),
            b"wal"
        );
        assert_eq!(
            std::fs::read(sqlite_file(&current, "-shm")).unwrap(),
            b"shm"
        );
        assert!(!legacy.exists());
    }

    fn private(account_id: &str, user_id: &str) -> ConversationKey {
        ConversationKey::for_kind("onebot", account_id, ConversationKind::Private, user_id).unwrap()
    }
}
