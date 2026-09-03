//! 跑一个平台回合。
//!
//! `run_platform_turn` 串起闸门、插件链、agent、投递。它长是因为每一步的失败
//! 都有不同的收尾——被限流要不要提示、被超越要不要保留排队、投递失败要不要
//! 回滚已记的送达。

use crate::platforms::*;

pub(crate) struct TurnOutcome {
    pub(crate) run_id: String,
    pub(crate) text: String,
    pub(crate) provider_id: Option<String>,
    pub(crate) model: Option<String>,
    /// Image asset ids published during the turn (`tool.image` events);
    /// bridges load the bytes and re-send them platform-natively.
    pub(crate) image_assets: Vec<String>,
    /// Byte ranges produced after confirmed direct long-image tool sends.
    /// Direct-send acknowledgements are removed from the final fallback text.
    pub(crate) suppressed_reply_ranges: Vec<(usize, usize)>,
    /// The last response segment was delivered by a successful direct tool
    /// send, so an otherwise empty platform reply must not add a placeholder.
    pub(crate) final_reply_already_sent: bool,
}

pub(crate) enum TurnDispatch {
    Completed(TurnOutcome),
    Failed(String),
    /// 回合被取消(消息撤回、被新消息取代、/stop):不是错误,静默收场,
    /// 不给用户回"出错了",也不按内部错误记日志(09-03 用户报)。
    Cancelled,
}

/// Drives one agent turn for an inbound IM message and waits for the
/// final result. Mirrors `handle_ipc_turn`, minus the client stream.
pub(crate) async fn run_platform_turn(
    state: &DaemonState,
    session_id: Arc<str>,
    content: String,
    images: Vec<Option<ImageAttachment>>,
    mut profile: TurnProfile,
) -> Result<TurnDispatch> {
    let content = validate_content(content).map_err(|error| anyhow!(error.message))?;
    state.state_store.recover_stale_turns()?;

    let _global_permit = state
        .platforms
        .turn_permits
        .clone()
        .acquire_owned()
        .await
        .map_err(|_| anyhow!("the platform turn scheduler is closed"))?;

    let run_id = random_id("run", 18);
    let (cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
    let platform_context = profile.platform.clone();
    let intermediate_replies = platform_context.as_ref().is_some_and(|context| {
        let qq = &context.config.platforms.qq;
        match context.conversation.kind {
            ConversationKind::Group => qq.group_intermediate_messages,
            ConversationKind::Private => qq.private_intermediate_messages,
        }
    });
    let platform_followup = platform_context
        .as_ref()
        .map(|context| PlatformFollowupRun::new(context.clone()));
    profile.followup = platform_followup.clone();
    {
        let mut manager = state.manager.lock().unwrap();
        if manager.admin_busy {
            bail!("Miyu is busy with another operation");
        }
        manager.active_runs.insert(
            run_id.clone(),
            RunInfo {
                session_id: session_id.clone(),
                mode: AgentMode::Normal,
                audience: PromptAudience::External,
                cancel: cancel_tx.clone(),
                turn_id: None,
                queue_target: None,
                supersede: Arc::new(crate::agent::TurnSupersedeSignal::default()),
                platform_followup,
                operation: crate::runtime::RunOperation::Create,
                job_wake: false,
                job_wake_label: None,
                // 平台真实入站消息;wake 合成轮的来源细分待平台 goal 支持时一并做。
                turn_origin: crate::tools::workspace::TurnOrigin::Human,
            },
        );
    }
    if let Some(context) = platform_context.as_ref() {
        context.turn_started(cancel_tx);
    }
    if platform_context
        .as_ref()
        .is_some_and(|context| context.turn_is_superseded())
    {
        crate::runtime::finish_run(&state.manager, &run_id, None);
        return Ok(TurnDispatch::Failed(
            crate::i18n::text("the turn was superseded", "本轮已被新消息覆盖").to_string(),
        ));
    }
    let after = state.events.latest_id();
    let mut subscription = state.events.subscribe_after(after);
    if state
        .actor_tx
        .send(ActorCommand::StartTurn {
            run_id: run_id.clone(),
            session_id,
            display_content: content.clone(),
            content,
            attachment_run_id: None,
            mode: AgentMode::Normal,
            images,
            cwd: None,
            origin_tty: None,
            audience: PromptAudience::External,
            profile: Some(profile),
            cancel: cancel_rx,
            turn_origin: Box::new(crate::tools::workspace::TurnOrigin::Human),
        })
        .is_err()
    {
        crate::runtime::finish_run(&state.manager, &run_id, None);
        bail!("Miyu core worker is unavailable");
    }
    // Cancels the run if this task dies before the turn settles.
    let mut run_guard = IpcRunGuard {
        manager: state.manager.clone(),
        run_id: run_id.clone(),
        finished: false,
    };

    let deadline = tokio::time::Instant::now() + PLATFORM_TURN_TIMEOUT;
    let mut text = String::new();
    let mut image_assets = Vec::new();
    let mut reply_suppression = ReplySuppression::default();
    let mut last_id = after;
    let dispatch = loop {
        let record = if let Some(record) = subscription.pending.pop_front() {
            record
        } else {
            match tokio::time::timeout_at(deadline, subscription.receiver.recv()).await {
                Err(_) => {
                    break TurnDispatch::Failed(
                        crate::i18n::text("the reply timed out", "回复超时，本轮已取消")
                            .to_string(),
                    );
                }
                Ok(Ok(record)) => record,
                Ok(Err(broadcast::error::RecvError::Lagged(_))) => {
                    subscription.pending = state.events.replay_after(last_id);
                    continue;
                }
                Ok(Err(broadcast::error::RecvError::Closed)) => {
                    break TurnDispatch::Failed(
                        crate::i18n::text("Miyu core stopped", "Miyu 核心已停止").to_string(),
                    );
                }
            }
        };
        if record.kind == "resync_required" {
            break TurnDispatch::Failed(
                crate::i18n::text(
                    "event history was exhausted; the turn was cancelled",
                    "事件缓冲耗尽，本轮已取消",
                )
                .to_string(),
            );
        }
        last_id = record.id;
        let Ok(data) = serde_json::from_str::<Value>(&record.data) else {
            continue;
        };
        if data.get("run_id").and_then(Value::as_str) != Some(run_id.as_str()) {
            continue;
        }
        match record.kind.as_str() {
            "reasoning.start" => {
                if intermediate_replies {
                    if let Some(context) = platform_context.as_ref() {
                        flush_intermediate_reply(context, &text, &reply_suppression).await;
                    }
                }
                start_model_reply(&mut text, &mut reply_suppression);
            }
            "assistant.delta" => {
                if let Some(delta) = data.get("delta").and_then(Value::as_str) {
                    text.push_str(delta);
                }
            }
            "generation.superseded" => {
                text.clear();
                reply_suppression.model_started();
            }
            // 端点重试(08-22 复读取证第 4 条):上一 attempt 已流出的半截正文
            // 作废,不丢弃就会与重试的完整正文拼接,经 split_reply 变成
            // "变体复读"。语义同 reasoning.start,但绝不 flush——半截正文
            // 正是要丢的东西。
            "reasoning.reset" => {
                start_model_reply(&mut text, &mut reply_suppression);
            }
            "tool.started" => {
                let readable = format_platform_tool_started_log(&run_id, &data);
                tracing::info!(target: "miyu::qq", "\n{readable}");
            }
            "tool.image" => {
                if let Some(id) = data
                    .get("asset")
                    .and_then(|asset| asset.get("id"))
                    .and_then(Value::as_str)
                {
                    image_assets.push(id.to_string());
                } else {
                    // 无 asset 的 tool.image 是资产落库失败/turn 未知的错误
                    // 事件;静默吞掉=平台图片凭空消失且无迹可查。
                    let error = data
                        .get("error")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown")
                        .to_string();
                    tracing::warn!(
                        target: "miyu::qq",
                        run_id = %run_id,
                        error = %error,
                        "平台工具图片事件不带资产,已跳过(图片不会投递)"
                    );
                }
            }
            "tool.finished" => {
                let readable = format_platform_tool_finished_log(&run_id, &data);
                tracing::info!(target: "miyu::qq", "\n{readable}");
                let suppression_start = platform_context
                    .as_ref()
                    .and_then(|context| context.take_final_reply_suppression_start(text.len()));
                if let Some(start) = suppression_start {
                    reply_suppression.direct_send_succeeded(start);
                }
            }
            "queue.consumed" => {
                // Flush before the suppression reset below: the flushed text
                // still needs the direct-send ranges of the round it came
                // from, and the next round answers the newly consumed prompt.
                if intermediate_replies {
                    if let Some(context) = platform_context.as_ref() {
                        flush_intermediate_reply(context, &text, &reply_suppression).await;
                    }
                    text.clear();
                }
                reply_suppression.queued_prompt_consumed();
            }
            "run.completed" => {
                run_guard.finish();
                let (suppressed_reply_ranges, final_reply_already_sent) =
                    reply_suppression.finish(text.len());
                break TurnDispatch::Completed(TurnOutcome {
                    run_id: run_id.clone(),
                    text,
                    provider_id: data
                        .get("provider_id")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                    model: data
                        .get("model")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                    image_assets,
                    suppressed_reply_ranges,
                    final_reply_already_sent,
                });
            }
            "run.failed" => {
                run_guard.finish();
                let message = data
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown error")
                    .to_string();
                break TurnDispatch::Failed(message);
            }
            "run.cancelled" => {
                run_guard.finish();
                break TurnDispatch::Cancelled;
            }
            _ => {}
        }
    };
    Ok(dispatch)
}
