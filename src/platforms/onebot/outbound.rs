//! 出站消息的分帧与投递。
//!
//! 长回复要切成多帧发（`push_message_frame` / `append_text_chunks`），于是「发
//! 送失败」不再是布尔值：可能前三帧成功第四帧超时。`partial_send_error` 保留
//! 这个区分——对用户来说「一条没发出去」和「发了一半」是两回事。
//!
//! 超时按内容量算（`send_timeout_for`）：一张大图和一行字用同一个超时，要么小
//! 图等太久，要么大图必然失败。

use crate::platforms::onebot::*;

pub(in crate::platforms::onebot) const MAX_OUTBOUND_IMAGE_BYTES: usize = 20 * 1024 * 1024;

pub(in crate::platforms::onebot) const MAX_OUTBOUND_IMAGE_DIMENSION: u32 = 16_384;

pub(in crate::platforms::onebot) const MAX_OUTBOUND_IMAGE_DECODE_ALLOC: u64 = 256 * 1024 * 1024;

/// 校验字节可解码为图片(解码结果仅校验即丢)。带解码限额:几十 KB 的
/// 30000×30000 像素炸弹解压时会分配数 GB,分配失败直接 abort 全进程。
/// 同步解码放 spawn_blocking,不占 actor 单线程 runtime。
pub(in crate::platforms::onebot) async fn validate_outbound_image(
    bytes: Vec<u8>,
    path: PathBuf,
) -> Result<Vec<u8>> {
    tokio::task::spawn_blocking(move || {
        let mut reader = image::ImageReader::new(std::io::Cursor::new(&bytes))
            .with_guessed_format()
            .with_context(|| format!("decoding image {}", path.display()))?;
        let mut limits = image::Limits::default();
        limits.max_image_width = Some(MAX_OUTBOUND_IMAGE_DIMENSION);
        limits.max_image_height = Some(MAX_OUTBOUND_IMAGE_DIMENSION);
        limits.max_alloc = Some(MAX_OUTBOUND_IMAGE_DECODE_ALLOC);
        reader.limits(limits);
        reader
            .decode()
            .with_context(|| format!("decoding image {}", path.display()))?;
        Ok(bytes)
    })
    .await
    .context("outbound image validation task failed")?
}

pub(in crate::platforms::onebot) const MAX_OUTBOUND_FILE_BYTES: usize = 50 * 1024 * 1024;

/// Backstop for attachment sends (see `send_timeout_for`). Not a budget: it
/// only exists so a connected-but-silent NapCat cannot wedge a conversation.
pub(in crate::platforms::onebot) const MAX_SEND_TIMEOUT: Duration = Duration::from_secs(180);

pub(in crate::platforms::onebot) struct MessageFrame {
    pub(in crate::platforms::onebot) segments: Vec<Value>,
    pub(in crate::platforms::onebot) image_digests: Vec<blake3::Hash>,
}

pub(in crate::platforms::onebot) fn push_message_frame(
    frames: &mut Vec<MessageFrame>,
    current: &mut Vec<Value>,
    current_image_digests: &mut Vec<blake3::Hash>,
) {
    if current.is_empty() {
        return;
    }
    frames.push(MessageFrame {
        segments: std::mem::take(current),
        image_digests: std::mem::take(current_image_digests),
    });
}

pub(in crate::platforms::onebot) fn append_text_chunks(
    frames: &mut Vec<MessageFrame>,
    current: &mut Vec<Value>,
    current_image_digests: &mut Vec<blake3::Hash>,
    text: &str,
    max_reply_chars: usize,
) {
    let chunks = split_reply(text, max_reply_chars);
    let count = chunks.len();
    for (index, chunk) in chunks.into_iter().enumerate() {
        current.push(text_segment(&chunk));
        if index + 1 < count {
            push_message_frame(frames, current, current_image_digests);
        }
    }
}

pub(in crate::platforms::onebot) fn partial_send_error(
    error: anyhow::Error,
    receipt: SendReceipt,
) -> anyhow::Error {
    if receipt.has_delivery() {
        anyhow::Error::new(PartialSendError::new(error, receipt))
    } else {
        error
    }
}

/// Sends carrying base64 images need far longer than a plain text call: a
/// 2 MiB picture is ~2.9 MB of JSON that NapCat has to receive, decode and
/// upload to QQ. Timing out early is worse than waiting — the message is
/// still delivered, but Miyu treats the send as failed and posts the plain
/// text fallback, so the group gets the picture *and* the text.
///
/// Size-scaling the budget only moved the cliff, and it moved it unevenly: the
/// old `div_ceil` step gave 0.99 MiB the same 30s as 64 KiB, so payloads just
/// under a megabyte boundary had the tightest work-to-budget ratio of all. An
/// attachment send now simply waits for NapCat instead of guessing how long it
/// should take.
///
/// `MAX_SEND_TIMEOUT` stays as a backstop rather than a budget. Losing the
/// connection already frees an in-flight call — `connection_loop` explicitly
/// drains the per-connection `pending` map on exit (clones of the handle held
/// by message tasks keep the Arc alive, so dropping alone would not do it),
/// so every waiting `oneshot` resolves immediately. The backstop only covers
/// a NapCat that stays connected but never answers this one echo, which would
/// otherwise wedge the conversation forever (same-conversation turns are
/// serialized and each in-flight message holds one of `MAX_IN_FLIGHT_MESSAGES`).
pub(in crate::platforms::onebot) fn send_timeout_for(segments: &[Value]) -> Duration {
    let carries_attachment = segments.iter().any(|segment| {
        segment
            .get("data")
            .and_then(|data| data.get("file"))
            .and_then(Value::as_str)
            .is_some_and(|file| !file.is_empty())
    });
    if carries_attachment {
        MAX_SEND_TIMEOUT
    } else {
        API_CALL_TIMEOUT
    }
}

pub(in crate::platforms::onebot) fn image_segment(bytes: &[u8]) -> Value {
    json!({
        "type": "image",
        "data": { "file": format!("base64://{}", BASE64.encode(bytes)) },
    })
}

pub(in crate::platforms::onebot) async fn read_file_capped(
    path: &std::path::Path,
    cap: usize,
) -> Result<Vec<u8>> {
    let file = tokio::fs::File::open(path)
        .await
        .with_context(|| format!("opening attachment: {}", path.display()))?;
    let metadata = file
        .metadata()
        .await
        .with_context(|| format!("reading attachment metadata: {}", path.display()))?;
    if !metadata.is_file() {
        bail!("attachment is not a regular file: {}", path.display());
    }
    if metadata.len() > cap as u64 {
        bail!("attachment exceeds the {} MiB limit", cap / 1024 / 1024);
    }
    let limit = u64::try_from(cap.saturating_add(1)).unwrap_or(u64::MAX);
    let mut reader = file.take(limit);
    let mut bytes = Vec::with_capacity(metadata.len().min(cap as u64) as usize);
    reader
        .read_to_end(&mut bytes)
        .await
        .with_context(|| format!("reading attachment: {}", path.display()))?;
    if bytes.len() > cap {
        bail!("attachment exceeds the {} MiB limit", cap / 1024 / 1024);
    }
    Ok(bytes)
}

pub(in crate::platforms::onebot) async fn deliver_dispatch(
    state: &DaemonState,
    context: &Arc<PlatformTurnContext>,
    dispatch: TurnDispatch,
) -> Result<bool> {
    match dispatch {
        TurnDispatch::Cancelled => {
            context.after_turn_aborted().await;
            tracing::debug!(
                target: "miyu::qq",
                conversation_kind = context.conversation.kind.as_str(),
                "{}",
                t("OneBot turn cancelled; nothing to deliver", "OneBot 回合已取消,无需投递")
            );
            return Ok(false);
        }
        TurnDispatch::Failed(message) => {
            context.after_turn_aborted().await;
            if context.conversation.kind == ConversationKind::Group {
                tracing::info!(
                    target: "miyu::qq",
                    error = %message,
                    "{}",
                    t("suppressed an internal OneBot group error", "已抑制 OneBot 群聊内部错误")
                );
                return Ok(false);
            }
            context
                .send_bypass_plugins(OutboundMessage::text(
                    OutboundOrigin::Command,
                    format!("{}{message}", t("Something went wrong: ", "出错了：")),
                ))
                .await?;
        }
        TurnDispatch::Completed(mut outcome) => {
            if context.turn_is_superseded() {
                context.after_turn_aborted().await;
                return Ok(false);
            }
            let mut segments = Vec::new();
            let reply_text = final_reply_text(&outcome);
            let delivered_image_digests = context.delivered_image_digests();
            let mut image_digests = delivered_image_digests.clone();
            let mut matched_delivered_image = false;
            let mut unresolved_image_count = 0usize;
            let mut image_count = 0usize;
            for asset_id in &outcome.image_assets {
                match state.state_store.load_image_asset(asset_id) {
                    Ok(Some(asset)) => {
                        let digest = blake3::hash(&asset.bytes);
                        if !image_digests.insert(digest) {
                            let already_delivered = delivered_image_digests.contains(&digest);
                            if already_delivered {
                                matched_delivered_image = true;
                            }
                            tracing::debug!(
                                target: "miyu::qq",
                                asset_id,
                                "{}",
                                if already_delivered {
                                    t(
                                        "suppressed a OneBot reply image already delivered to this conversation",
                                        "已抑制本会话中先前已投递的 OneBot 回复图片",
                                    )
                                } else {
                                    t(
                                        "suppressed a duplicate OneBot reply image",
                                        "已抑制重复的 OneBot 回复图片",
                                    )
                                }
                            );
                            continue;
                        }
                        segments.push(OutboundSegment::ImageBytes {
                            mime: asset.asset.mime,
                            data: Arc::from(asset.bytes),
                            alt: asset.asset.alt,
                        });
                        image_count += 1;
                    }
                    Ok(None) => {
                        unresolved_image_count += 1;
                        tracing::warn!(
                            target: "miyu::qq",
                            asset_id,
                            "{}",
                            t(
                                "a OneBot reply image asset was not found",
                                "未找到 OneBot 回复图片资源",
                            )
                        );
                    }
                    Err(error) => {
                        unresolved_image_count += 1;
                        tracing::warn!(error = %error, asset_id, "{}", t("loading an image asset for OneBot failed", "为 OneBot 加载图片资源失败"));
                    }
                }
            }
            if matched_delivered_image && image_count == 0 && unresolved_image_count == 0 {
                outcome.final_reply_already_sent = true;
            }
            let readable = crate::platforms::format_platform_final_reply_log(
                &outcome,
                context,
                &reply_text,
                image_count,
            );
            if !reply_text.trim().is_empty() {
                segments.insert(0, OutboundSegment::Markdown(reply_text));
            }
            if segments.is_empty() {
                if outcome.final_reply_already_sent {
                    tracing::info!(target: "miyu::qq", "\n{readable}");
                    return Ok(true);
                }
                tracing::info!(
                    target: "miyu::qq",
                    "{}",
                    t("suppressed an empty OneBot model reply", "已抑制空的 OneBot 模型回复")
                );
                return Ok(false);
            }
            context
                .send(OutboundMessage::segments(
                    OutboundOrigin::FinalReply,
                    segments,
                ))
                .await?;
            tracing::info!(target: "miyu::qq", "\n{readable}");
        }
    }
    Ok(true)
}

pub(in crate::platforms::onebot) fn final_reply_text(
    outcome: &crate::platforms::TurnOutcome,
) -> String {
    crate::platforms::cut_suppressed_ranges(&outcome.text, &outcome.suppressed_reply_ranges)
}

pub(in crate::platforms::onebot) fn text_segment(text: &str) -> Value {
    json!({ "type": "text", "data": { "text": text } })
}
