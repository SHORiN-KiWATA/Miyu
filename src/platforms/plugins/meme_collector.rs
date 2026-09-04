use super::{PlatformPlugin, PluginDescriptor};
use crate::config::{QqMemeCollectorPluginSettings, QQ_MEME_COLLECTOR_PLUGIN_ID};
use crate::platforms::{
    ConversationKind, OutboundBody, OutboundMessage, OutboundSegment, PlatformInboundEvent,
    PlatformInboundEventKind, PlatformMediaKind, PlatformTurnContext, SendReceipt,
};
use crate::state::PlatformMemeRefRecord;
use crate::tools::memes::{
    collect_meme_from_local_image, current_persona_library, delete_meme_reference, meme_ref_exists,
    MemeCollectionOutcome, MemeOrigin, MemeRef,
};
use crate::tools::{ToolRegistry, ToolSpec};
use anyhow::{bail, Context, Result};
use futures_util::future::BoxFuture;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::sync::Arc;
use tokio::sync::mpsc;

const COLLECTOR_QUEUE_CAPACITY: usize = 32;

struct CollectionJob {
    config: crate::config::AppConfig,
    paths: crate::paths::MiyuPaths,
    state: crate::state::StateStore,
    conversation: crate::platforms::PlatformConversation,
    message_id: String,
    origin: MemeOrigin,
    selected_indices: BTreeSet<usize>,
    images: BoxFuture<'static, Result<Vec<crate::platforms::PlatformImageData>>>,
}

/// 平台消息时间戳（unix 秒）→ RFC3339；无效值返回空串
fn platform_sent_at(timestamp: i64) -> String {
    if timestamp <= 0 {
        return String::new();
    }
    chrono::DateTime::from_timestamp(timestamp, 0)
        .map(|value| value.to_rfc3339())
        .unwrap_or_default()
}

pub(super) struct MemeCollectorPlugin {
    jobs: mpsc::Sender<CollectionJob>,
}

impl MemeCollectorPlugin {
    pub(super) fn new() -> Self {
        let (jobs, mut receiver) = mpsc::channel::<CollectionJob>(COLLECTOR_QUEUE_CAPACITY);
        tokio::spawn(async move {
            while let Some(job) = receiver.recv().await {
                if let Err(error) = process_collection_job(job).await {
                    tracing::warn!(
                        %error,
                        "{}",
                        crate::i18n::text("QQ meme collection job failed", "QQ 表情包收集任务失败")
                    );
                }
            }
        });
        Self { jobs }
    }
}

impl PlatformPlugin for MemeCollectorPlugin {
    fn descriptor(&self) -> PluginDescriptor {
        PluginDescriptor {
            id: QQ_MEME_COLLECTOR_PLUGIN_ID,
            priority: 180,
            default_enabled: true,
        }
    }

    fn observe_inbound<'a>(
        &'a self,
        context: &'a PlatformTurnContext,
        event: &'a PlatformInboundEvent,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            if event.kind != PlatformInboundEventKind::Message
                || event.conversation.platform != "onebot"
                || event.conversation.kind != ConversationKind::Group
                || event.sender_id == event.conversation.account_id
                || !context.config.plugins.memes.enabled
            {
                return Ok(());
            }
            let settings = collector_settings(context)?;
            if settings.collect_probability <= 0.0 {
                return Ok(());
            }
            let mut selected_indices = BTreeSet::new();
            for (index, _) in event
                .media
                .iter()
                .filter(|media| media.kind == PlatformMediaKind::Image)
                .take(settings.max_images_per_message)
                .enumerate()
            {
                if rand::random::<f64>() < settings.collect_probability {
                    selected_indices.insert(index);
                }
            }
            if selected_indices.is_empty() {
                return Ok(());
            }
            let job = CollectionJob {
                config: context.config.clone(),
                paths: context.paths.clone(),
                state: context.state_store.clone(),
                conversation: context.conversation.clone(),
                message_id: event.message_id.clone(),
                origin: MemeOrigin {
                    platform: event.conversation.platform.clone(),
                    conversation_kind: event.conversation.kind.as_str().to_string(),
                    conversation_id: event.conversation.conversation_id.clone(),
                    sender_id: event.sender_id.clone(),
                    sender_name: event.sender_display_name.clone(),
                    message_id: event.message_id.clone(),
                    sent_at: platform_sent_at(event.timestamp),
                    collected_at: String::new(),
                    reason: String::new(),
                },
                selected_indices,
                images: context.message_images_task(event.message_id.clone()),
            };
            if self.jobs.try_send(job).is_err() {
                tracing::debug!(
                    "{}",
                    crate::i18n::text(
                        "QQ meme collector queue is full; dropping sampled message",
                        "QQ 表情包收集队列已满；丢弃抽样消息"
                    )
                );
            }
            Ok(())
        })
    }

    fn register_tools(
        &self,
        registry: &mut ToolRegistry,
        context: Arc<PlatformTurnContext>,
    ) -> Result<()> {
        let settings = collector_settings(&context)?;
        if save_tool_allowed(context.is_admin, &settings) {
            let save_context = context.clone();
            registry.register(
                ToolSpec::new(
                    "save_current_message_meme",
                    "Save an image from the current QQ message or its directly quoted message as a meme. Arbitrary messages cannot be targeted, and the strict meme check must pass.",
                    json!({
                        "type": "object",
                        "properties": {
                            "image_index": {
                                "type": "integer",
                                "minimum": 1,
                                "description": "当前消息中的图片序号，从 1 开始；默认保存第一张。"
                            },
                            "reason": {
                                "type": "string",
                                "description": "一句话说说为什么想把这张图存下来（只给库主人在面板里看，之后不会再回给你）。"
                            }
                        },
                        "additionalProperties": false
                    }),
                    move |args| {
                        let context = save_context.clone();
                        async move { save_current_message_meme(context, args).await }
                    },
                )
                .writes(),
            );
        }
        if context.is_admin && context.conversation.kind == ConversationKind::Group {
            let delete_context = context.clone();
            registry.register(
                ToolSpec::new(
                    "delete_referenced_meme",
                    "Delete the meme referenced by the current QQ message. Only the exact meme of the quoted message can be deleted; arbitrary ids are rejected.",
                    json!({
                        "type": "object",
                        "properties": {},
                        "additionalProperties": false
                    }),
                    move |_args| {
                        let context = delete_context.clone();
                        async move { delete_referenced_meme(context).await }
                    },
                )
                .writes(),
            );
        }
        Ok(())
    }

    fn after_send<'a>(
        &'a self,
        context: &'a PlatformTurnContext,
        message: &'a OutboundMessage,
        receipt: &'a SendReceipt,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            if receipt.image_message_ids.is_empty() {
                return Ok(());
            }
            let library = current_persona_library(&context.config);
            let mut memes = Vec::new();
            if let OutboundBody::Segments(segments) = &message.body {
                for segment in segments {
                    // 只需要 digest:ImageBytes 直接借用,免掉整图 to_vec 拷贝。
                    let digest = match segment {
                        OutboundSegment::ImageBytes { data, .. } => Some(Sha256::digest(data)),
                        OutboundSegment::ImagePath { path, .. } => tokio::fs::read(path)
                            .await
                            .ok()
                            .map(|bytes| Sha256::digest(&bytes)),
                        _ => None,
                    };
                    let Some(digest) = digest else { continue };
                    let meme = MemeRef {
                        library: library.clone(),
                        id: format!("sha256:{digest:x}"),
                    };
                    if meme_ref_exists(&context.paths, &meme)? {
                        memes.push(meme);
                    }
                }
            }
            for message_id in &receipt.image_message_ids {
                for meme in &memes {
                    put_mapping(context, message_id, meme, "outbound")?;
                }
            }
            Ok(())
        })
    }
}

async fn process_collection_job(job: CollectionJob) -> Result<()> {
    let images = job.images.await?;
    for (index, image) in images.into_iter().enumerate() {
        if !job.selected_indices.contains(&index) {
            continue;
        }
        let suffix = match image.mime.as_str() {
            "image/jpeg" => ".jpg",
            "image/png" => ".png",
            "image/gif" => ".gif",
            "image/webp" => ".webp",
            _ => ".img",
        };
        let file = tempfile::Builder::new().suffix(suffix).tempfile()?;
        tokio::fs::write(file.path(), &image.data).await?;
        match collect_meme_from_local_image(
            file.path(),
            &job.config,
            &job.paths,
            Some(job.origin.clone()),
        )
        .await?
        {
            MemeCollectionOutcome::Accepted { meme }
            | MemeCollectionOutcome::AlreadyExists { meme } => {
                let record = PlatformMemeRefRecord {
                    platform: job.conversation.platform.clone(),
                    account_id: job.conversation.account_id.clone(),
                    conversation_kind: job.conversation.kind.as_str().to_string(),
                    conversation_id: job.conversation.conversation_id.clone(),
                    message_id: job.message_id.clone(),
                    library: meme.library,
                    meme_id: meme.id,
                    direction: "inbound".to_string(),
                    created_at: chrono::Utc::now().to_rfc3339(),
                };
                job.state.put_platform_meme_ref(&record)?;
            }
            MemeCollectionOutcome::Rejected { reason } => {
                tracing::debug!(
                    %reason,
                    "{}",
                    crate::i18n::text(
                        "sampled QQ image did not pass strict meme screening",
                        "抽样 QQ 图片未通过严格表情包筛选"
                    )
                );
            }
        }
    }
    Ok(())
}

async fn save_current_message_meme(
    context: Arc<PlatformTurnContext>,
    args: Value,
) -> Result<String> {
    let settings = collector_settings(&context)?;
    if !save_tool_allowed(context.is_admin, &settings) {
        bail!("non-admin use of the meme save tool is disabled");
    }
    let event = context
        .inbound_event()
        .context("the current message is unavailable")?;
    if event.kind != PlatformInboundEventKind::Message {
        bail!("the current event is not a message");
    }
    let image_index = args.get("image_index").and_then(Value::as_u64).unwrap_or(1);
    if !(1..=4).contains(&image_index) {
        bail!("image_index must be between 1 and 4");
    }
    // 模型自己给的「添加理由」优先;没给就用分类模型的那句(见 item_from_classification)。
    let reason: String = args
        .get("reason")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim()
        .chars()
        .take(200)
        .collect();
    let from_current_message = event
        .media
        .iter()
        .any(|media| media.kind == PlatformMediaKind::Image);
    let message_id = if from_current_message {
        event.message_id.clone()
    } else {
        event
            .reply_to_message_id
            .clone()
            .context("send an image or directly reply to an image message")?
    };
    // 来源者取图片所在消息的发送者：引用保存时是被引用消息的发送者
    let origin = if from_current_message {
        MemeOrigin {
            platform: event.conversation.platform.clone(),
            conversation_kind: event.conversation.kind.as_str().to_string(),
            conversation_id: event.conversation.conversation_id.clone(),
            sender_id: event.sender_id.clone(),
            sender_name: event.sender_display_name.clone(),
            message_id: message_id.clone(),
            sent_at: platform_sent_at(event.timestamp),
            collected_at: String::new(),
            reason: reason.clone(),
        }
    } else {
        let replied = event.replied_message.as_ref();
        MemeOrigin {
            platform: event.conversation.platform.clone(),
            conversation_kind: event.conversation.kind.as_str().to_string(),
            conversation_id: event.conversation.conversation_id.clone(),
            sender_id: replied.map(|m| m.sender_id.clone()).unwrap_or_default(),
            sender_name: replied
                .map(|m| m.sender_display_name.clone())
                .unwrap_or_default(),
            message_id: message_id.clone(),
            sent_at: replied
                .map(|m| platform_sent_at(m.timestamp))
                .unwrap_or_default(),
            collected_at: String::new(),
            reason: reason.clone(),
        }
    };
    let images = context.message_images_task(message_id.clone()).await?;
    let image = images
        .into_iter()
        .nth(image_index as usize - 1)
        .context("the requested image could not be read from the current message")?;
    let suffix = match image.mime.as_str() {
        "image/jpeg" => ".jpg",
        "image/png" => ".png",
        "image/gif" => ".gif",
        "image/webp" => ".webp",
        _ => ".img",
    };
    let file = tempfile::Builder::new().suffix(suffix).tempfile()?;
    tokio::fs::write(file.path(), &image.data).await?;
    let outcome =
        collect_meme_from_local_image(file.path(), &context.config, &context.paths, Some(origin))
            .await?;
    let response = match outcome {
        MemeCollectionOutcome::Accepted { meme } => {
            put_mapping(&context, &message_id, &meme, "inbound")?;
            json!({
                "success": true,
                "action": "saved",
                "library": meme.library,
                "id": meme.id,
            })
        }
        MemeCollectionOutcome::AlreadyExists { meme } => {
            put_mapping(&context, &message_id, &meme, "inbound")?;
            json!({
                "success": true,
                "already_exists": true,
                "library": meme.library,
                "id": meme.id,
            })
        }
        MemeCollectionOutcome::Rejected { reason } => json!({
            "success": false,
            "rejected": true,
            "reason": reason,
        }),
    };
    Ok(response.to_string())
}

async fn delete_referenced_meme(context: Arc<PlatformTurnContext>) -> Result<String> {
    if !context.is_admin || context.conversation.kind != ConversationKind::Group {
        bail!("only configured Miyu administrators can delete collected memes");
    }
    let reply_id = context
        .inbound_event()
        .and_then(|event| event.reply_to_message_id.as_deref())
        .filter(|id| !id.trim().is_empty())
        .context("reply to the meme message that should be deleted")?;
    let records = context.state_store.platform_meme_refs_for_message(
        &context.conversation.platform,
        &context.conversation.account_id,
        context.conversation.kind.as_str(),
        &context.conversation.conversation_id,
        reply_id,
    )?;
    let unique = records
        .iter()
        .map(|record| (record.library.clone(), record.meme_id.clone()))
        .collect::<BTreeSet<_>>();
    if unique.is_empty() {
        bail!("the referenced message is not mapped to a collected meme");
    }
    if unique.len() != 1 {
        bail!("the referenced message contains multiple memes; deletion is ambiguous");
    }
    let (library, id) = unique.into_iter().next().unwrap();
    let meme = MemeRef { library, id };
    delete_meme_reference(&meme, &context.config, &context.paths).await?;
    context
        .state_store
        .delete_platform_meme_ref(&meme.library, &meme.id)?;
    Ok(json!({
        "success": true,
        "library": meme.library,
        "id": meme.id,
        "action": "soft_deleted_referenced_meme"
    })
    .to_string())
}

fn put_mapping(
    context: &PlatformTurnContext,
    message_id: &str,
    meme: &MemeRef,
    direction: &str,
) -> Result<()> {
    context
        .state_store
        .put_platform_meme_ref(&PlatformMemeRefRecord {
            platform: context.conversation.platform.clone(),
            account_id: context.conversation.account_id.clone(),
            conversation_kind: context.conversation.kind.as_str().to_string(),
            conversation_id: context.conversation.conversation_id.clone(),
            message_id: message_id.to_string(),
            library: meme.library.clone(),
            meme_id: meme.id.clone(),
            direction: direction.to_string(),
            created_at: chrono::Utc::now().to_rfc3339(),
        })
}

fn collector_settings(context: &PlatformTurnContext) -> Result<QqMemeCollectorPluginSettings> {
    context
        .config
        .platforms
        .qq
        .plugins
        .get(QQ_MEME_COLLECTOR_PLUGIN_ID)
        .map(QqMemeCollectorPluginSettings::from_instance)
        .transpose()
        .map(|settings| settings.unwrap_or_default())
}

fn save_tool_allowed(is_admin: bool, settings: &QqMemeCollectorPluginSettings) -> bool {
    is_admin || settings.allow_non_admin_save_tool
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn save_tool_is_admin_only_by_default() {
        let settings = QqMemeCollectorPluginSettings::default();
        assert!(save_tool_allowed(true, &settings));
        assert!(!save_tool_allowed(false, &settings));
    }

    #[test]
    fn setting_can_allow_non_admin_save_tool() {
        let settings = QqMemeCollectorPluginSettings {
            allow_non_admin_save_tool: true,
            ..Default::default()
        };
        assert!(save_tool_allowed(false, &settings));
    }
}
