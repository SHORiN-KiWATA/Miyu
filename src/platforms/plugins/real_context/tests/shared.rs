//! 真实感插件测试共用的 fixture。

use crate::paths::MiyuPaths;
use crate::platforms::plugins::real_context::*;
use crate::platforms::PlatformAdapter;
use crate::state::StateStore;

pub(super) fn test_context(
    adapter: Arc<dyn PlatformAdapter>,
) -> (tempfile::TempDir, PlatformTurnContext) {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let paths = MiyuPaths {
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
    };
    let context = PlatformTurnContext::new(
        crate::platforms::PlatformConversation {
            platform: "onebot".to_string(),
            account_id: "10000".to_string(),
            kind: ConversationKind::Group,
            conversation_id: "20000".to_string(),
        },
        "30000".to_string(),
        "测试用户".to_string(),
        false,
        crate::config::AppConfig::default(),
        paths.clone(),
        StateStore::new(&paths).unwrap(),
        adapter,
        Arc::new(crate::platforms::plugins::PlatformPluginRegistry::default()),
    )
    .with_inbound_event(inbound_event());
    (temp, context)
}

/// 私聊版上下文。群聊那份走 real_context 全套,私聊只借用图片引用。
pub(super) fn private_availability_context(
    availability: BotSendAvailability,
) -> (tempfile::TempDir, PlatformTurnContext) {
    let (temp, context) = availability_context(availability);
    let mut conversation = context.conversation.clone();
    conversation.kind = ConversationKind::Private;
    conversation.conversation_id = "30000".to_string();
    let mut event = inbound_event();
    event.conversation = conversation.clone();
    let rebuilt = PlatformTurnContext::new(
        conversation,
        context.sender_id.clone(),
        context.sender_display_name.clone(),
        false,
        context.config.clone(),
        context.paths.clone(),
        context.state_store.clone(),
        context.adapter.clone(),
        context.plugins.clone(),
    )
    .with_inbound_event(event);
    (temp, rebuilt)
}

pub(super) fn availability_context(
    availability: BotSendAvailability,
) -> (tempfile::TempDir, PlatformTurnContext) {
    test_context(Arc::new(AvailabilityAdapter(availability)))
}

pub(super) fn history_message(message_id: &str, text: &str) -> HistoryMessage {
    HistoryMessage {
        row_id: 1,
        group: GroupKey::new("onebot", "10000", "20000").unwrap(),
        message_id: message_id.to_string(),
        sender_id: "30000".to_string(),
        sender_name: "测试用户".to_string(),
        content: SanitizedContent::new(text, Vec::new()),
        reply_to_message_id: None,
        is_bot: false,
        sent_at: 1,
        ingress_order: Some(1),
        recalled_at: None,
    }
}

pub(super) fn inbound_event() -> PlatformInboundEvent {
    PlatformInboundEvent {
        kind: PlatformInboundEventKind::Message,
        conversation: crate::platforms::PlatformConversation {
            platform: "onebot".to_string(),
            account_id: "10000".to_string(),
            kind: ConversationKind::Group,
            conversation_id: "20000".to_string(),
        },
        conversation_display_name: Some("测试群".to_string()),
        message_id: "message-1".to_string(),
        sender_id: "30000".to_string(),
        sender_display_name: "测试用户".to_string(),
        operator_id: None,
        timestamp: 1,
        received_at: Instant::now(),
        message_position: Some(crate::platforms::PlatformMessagePosition {
            total_messages: 1,
            sender_messages: 1,
        }),
        ingress_order: Some(1),
        text: "测试".to_string(),
        reply_to_message_id: None,
        replied_message: None,
        mentioned_user_ids: Vec::new(),
        mentioned_users: Vec::new(),
        mentioned_bot: false,
        media: Vec::new(),
        notice_sub_type: None,
        duration_seconds: None,
    }
}

pub(super) struct AvailabilityAdapter(BotSendAvailability);

pub(super) struct ReactionAdapter {
    pub(super) reactions: Arc<Mutex<Vec<(String, String, bool)>>>,
}

impl PlatformAdapter for AvailabilityAdapter {
    fn send<'a>(&'a self, _message: OutboundMessage) -> BoxFuture<'a, Result<SendReceipt>> {
        Box::pin(async { Ok(SendReceipt::default()) })
    }

    fn bot_display_name<'a>(&'a self) -> BoxFuture<'a, Result<String>> {
        Box::pin(async { Ok("Miyu".to_string()) })
    }

    fn bot_send_availability<'a>(&'a self) -> BoxFuture<'a, Result<BotSendAvailability>> {
        let availability = self.0;
        Box::pin(async move { Ok(availability) })
    }
}

impl PlatformAdapter for ReactionAdapter {
    fn send<'a>(&'a self, _message: OutboundMessage) -> BoxFuture<'a, Result<SendReceipt>> {
        Box::pin(async { Ok(SendReceipt::default()) })
    }

    fn bot_display_name<'a>(&'a self) -> BoxFuture<'a, Result<String>> {
        Box::pin(async { Ok("Miyu".to_string()) })
    }

    fn bot_send_availability<'a>(&'a self) -> BoxFuture<'a, Result<BotSendAvailability>> {
        Box::pin(async { Ok(BotSendAvailability::Available) })
    }

    fn set_message_reaction<'a>(
        &'a self,
        message_id: &'a str,
        reaction_id: &'a str,
        active: bool,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            self.reactions.lock().unwrap().push((
                message_id.to_string(),
                reaction_id.to_string(),
                active,
            ));
            Ok(())
        })
    }
}

pub(super) fn empty_turn_input() -> PlatformTurnInput {
    PlatformTurnInput {
        content: "当前输入".to_string(),
        memory_content: "当前输入".to_string(),
        system_context: Vec::new(),
        turn_system_context: Vec::new(),
        context_images: Vec::new(),
        context_files: Vec::new(),
    }
}
