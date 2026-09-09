//! 回合的创建、重做、排队与收尾。
//!
//! 一个回合有四种终局：正常完成、被取消、失败、以及上下文超限。它们的收尾动
//! 作不同（要不要落库、要不要留住排队消息、要不要通知前端），所以分成
//! `finish_*` 四个函数而不是一个带一堆 flag 的分支。
//!
//! 排队（`queue_prompt`）是「回合还在跑时用户又发了一条」的处理：不打断当前
//! 回合，也不丢消息。取消时排队的内容要跟着清掉，见 `drop_cancelled_queue`。

mod task;
pub(in crate::web) use task::*;

use crate::web::*;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::web) struct CreateTurnRequest {
    pub(in crate::web) content: String,
    /// 兼容字段:旧前端仍会带 mode;会话模式创建时定死,daemon 按会话
    /// 记录强制,这个值只解析不采信。缺省即普通。
    #[serde(default)]
    pub(in crate::web) mode: Option<String>,
    #[serde(default)]
    pub(in crate::web) attachment_ids: Vec<String>,
    /// Target session; defaults to the global current session. The turn runs
    /// there without moving the current pointer (per-view WebUI sessions).
    #[serde(default)]
    pub(in crate::web) session_id: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::web) struct QueuePromptRequest {
    pub(in crate::web) content: String,
    pub(in crate::web) run_id: String,
    pub(in crate::web) turn_id: String,
    #[serde(default)]
    pub(in crate::web) attachment_ids: Vec<String>,
    /// Target session; defaults to the global current session.
    #[serde(default)]
    pub(in crate::web) session_id: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::web) struct RedoTurnRequest {
    pub(in crate::web) expected_revision: i64,
    pub(in crate::web) input_id: String,
    #[serde(default)]
    pub(in crate::web) content: Option<String>,
    /// 同 CreateTurnRequest.mode:兼容旧前端,只解析不采信。
    #[serde(default)]
    pub(in crate::web) mode: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::web) struct AnswerQuestionRequest {
    pub(in crate::web) answers: QuestionAnswers,
}

pub(in crate::web) fn validate_message_content(
    content: String,
    has_attachments: bool,
) -> std::result::Result<String, ApiError> {
    if content.trim().is_empty() && has_attachments {
        return Ok(String::new());
    }
    validate_content(content)
}

pub(in crate::web) async fn redo_turn(
    State(state): State<DaemonState>,
    headers: HeaderMap,
    Path((session_id, turn_id)): Path<(String, String)>,
    Json(request): Json<RedoTurnRequest>,
) -> std::result::Result<Response, ApiError> {
    require_mutation(&headers, &state)?;
    require_local_web_session(&state, &session_id)?;
    let mode = parse_mode(request.mode.as_deref().unwrap_or("normal"))?;
    let store = state.state_store.pinned_for_turn(&session_id);
    let candidate = store
        .redo_candidate()
        .map_err(ApiError::internal)?
        .ok_or_else(|| ApiError::new(StatusCode::CONFLICT, "the last input cannot be redone"))?;
    if candidate.turn_id != turn_id
        || candidate.input_id != request.input_id
        || candidate.revision != request.expected_revision
    {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "the conversation changed before redo could start",
        ));
    }

    let mut prompts = Vec::new();
    match candidate.input_kind {
        crate::state::RedoInputKind::Initial => {
            let attachments = store
                .load_user_attachment_data_for_turn(&turn_id)
                .map_err(ApiError::internal)?;
            let display_content = validate_message_content(
                request
                    .content
                    .unwrap_or_else(|| candidate.display_content.clone()),
                !attachments.is_empty(),
            )?;
            let prepared = prepare_web_attachment_data(&display_content, attachments)?;
            prompts.push(RedoWebPrompt {
                prompt_id: candidate.input_id.clone(),
                content: prepared.content,
                display_content,
                images: prepared.images,
            });
        }
        crate::state::RedoInputKind::Followup => {
            let batch = store
                .load_redo_batch_prompts(&turn_id, &candidate.batch_prompt_ids)
                .map_err(ApiError::internal)?;
            for prompt in batch {
                if !prompt.attachments.is_empty() {
                    return Err(ApiError::new(
                        StatusCode::CONFLICT,
                        "this follow-up uses non-durable attachments and cannot be redone",
                    ));
                }
                let attachments = store
                    .load_user_attachment_data_for_prompt(&prompt.prompt_id)
                    .map_err(ApiError::internal)?;
                let display_content = if prompt.prompt_id == candidate.input_id {
                    validate_message_content(
                        request
                            .content
                            .clone()
                            .unwrap_or_else(|| prompt.display_content.clone()),
                        !attachments.is_empty(),
                    )?
                } else {
                    prompt.display_content
                };
                let prepared = prepare_web_attachment_data(&display_content, attachments)?;
                prompts.push(RedoWebPrompt {
                    prompt_id: prompt.prompt_id,
                    content: prepared.content,
                    display_content,
                    images: prepared.images,
                });
            }
        }
    }

    let run_id = random_id("run", 18);
    let (cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
    {
        let mut manager = state.manager.lock().unwrap();
        if manager.admin_blocks_session(&session_id) || manager.session_has_runs(&session_id) {
            return Err(ApiError::new(
                StatusCode::CONFLICT,
                "Miyu is busy in this conversation",
            ));
        }
        manager.active_runs.insert(
            run_id.clone(),
            RunInfo {
                session_id: session_id.clone().into(),
                mode,
                audience: PromptAudience::External,
                cancel: cancel_tx,
                turn_id: Some(turn_id.clone()),
                queue_target: None,
                supersede: Arc::new(crate::agent::TurnSupersedeSignal::default()),
                platform_followup: None,
                operation: RunOperation::Redo {
                    turn_id: turn_id.clone(),
                    input_id: candidate.input_id.clone(),
                },
                job_wake: false,
                turn_origin: crate::tools::workspace::TurnOrigin::Human,
                job_wake_label: None,
            },
        );
    }
    if state
        .actor_tx
        .send(ActorCommand::RedoTurn {
            run_id: run_id.clone(),
            session_id: session_id.into(),
            candidate,
            prompts,
            mode,
            cancel: cancel_rx,
        })
        .is_err()
    {
        finish_run(&state.manager, &run_id, None);
        return Err(ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "agent worker is unavailable",
        ));
    }
    Ok((
        StatusCode::ACCEPTED,
        Json(json!({
            "run_id": run_id,
            "turn_id": turn_id,
            "operation": "redo",
        })),
    )
        .into_response())
}

pub(in crate::web) fn unique_run_target(
    manager: &ManagerState,
    session_id: &str,
    audience: PromptAudience,
) -> Option<(String, String)> {
    let mut runs = manager.active_runs.iter().filter(|(_, run)| {
        &*run.session_id == session_id && run.audience == audience && run.turn_id.is_some()
    });
    let (run_id, run) = runs.next()?;
    if runs.next().is_some() {
        return None;
    }
    Some((run_id.clone(), run.turn_id.clone()?))
}

/// WebUI 往一个正在跑的 run 里排消息时该报的身份。
///
/// 目标续轮是机器自己开的 `Owner` 轮，WebUI（`External`）排进去是设计决定
/// （见 [`queue_into_running_session`]）；其余 run 一律报 `External`，于是
/// REPL 起的 `Owner` 轮对不上 `enqueue_turn_update` 的第一道校验，跨端隔离
/// 原样保留。
pub(in crate::web) fn web_followup_audience(run: &RunInfo) -> PromptAudience {
    if matches!(
        run.turn_origin,
        crate::tools::workspace::TurnOrigin::GoalRound { .. }
    ) {
        PromptAudience::Owner
    } else {
        PromptAudience::External
    }
}

/// 会话里已有轮在跑时，把这条消息排进那一轮。
///
/// 返回 `Ok(None)` = 这个会话没有可排队的轮（没有跑着的轮，或跑的是 REPL 起
/// 的轮），调用方照常走起新轮的路——那条路上的 `session_has_runs` 会给出
/// 409「Miyu is busy」，跨端隔离不变。
///
/// 目标续轮也走排队：它是机器自己开的轮，人开口该优先，而不是撞一个 409 让人
/// 重发。回合循环在每个工具边界都会取排队的输入。续轮的 audience 是 `Owner`，
/// 排队请求必须报同一个身份，否则 `enqueue_turn_update` 的 audience 校验会把
/// 它挡回来（这正是「续轮跑着时 WebUI 发消息报 409」的根因）。
pub(in crate::web) fn queue_into_running_session(
    state: &DaemonState,
    session_id: &Arc<str>,
    content: &str,
    display_content: &str,
    uploaded_attachment_ids: &[String],
) -> std::result::Result<Option<TurnUpdateReceipt>, ApiError> {
    if !state
        .state_store
        .pinned(session_id)
        .has_running_turns()
        .map_err(ApiError::internal)?
    {
        return Ok(None);
    }
    let target = {
        let manager = state.manager.lock().unwrap();
        if !manager.session_runs_match_audience(session_id, PromptAudience::External)
            && !manager.session_runs_are_goal_rounds(session_id)
        {
            return Ok(None);
        }
        // 上面两道闸保证这个会话的轮是同一类（全 External，或全目标续轮），
        // 所以随便挑一条都能定出该报的身份。
        let audience = manager
            .active_runs
            .values()
            .find(|run| run.session_id.as_ref() == session_id.as_ref())
            .map_or(PromptAudience::External, web_followup_audience);
        unique_run_target(&manager, session_id, audience)
            .map(|(run_id, turn_id)| (run_id, turn_id, audience))
    };
    let (run_id, turn_id, audience) = target.ok_or_else(|| {
        ApiError::new(
            StatusCode::CONFLICT,
            "the running turn is not ready or is ambiguous",
        )
    })?;
    let receipt = enqueue_turn_update(
        state,
        TurnUpdateRequest {
            run_id,
            turn_id,
            session_id: Some(session_id.clone()),
            audience,
            content: content.to_string(),
            display_content: display_content.to_string(),
            attachments: Vec::new(),
            uploaded_attachment_ids: uploaded_attachment_ids.to_vec(),
            mode: TurnUpdateMode::Followup,
        },
    )
    .map_err(|error| ApiError::new(StatusCode::CONFLICT, error.to_string()))?;
    Ok(Some(receipt))
}

pub(in crate::web) async fn create_turn(
    State(state): State<DaemonState>,
    headers: HeaderMap,
    Json(request): Json<CreateTurnRequest>,
) -> std::result::Result<Response, ApiError> {
    require_mutation(&headers, &state)?;
    let attachment_ids = request.attachment_ids;
    let display_content = validate_message_content(request.content, !attachment_ids.is_empty())?;
    let mode = parse_mode(request.mode.as_deref().unwrap_or("normal"))?;
    let session_id = resolve_turn_session(&state, request.session_id).map_err(session_api_error)?;
    state
        .state_store
        .recover_stale_turns()
        .map_err(ApiError::internal)?;
    // A running turn in the *target* session gets the message as a queued
    // follow-up (composer tray UX); other sessions run in parallel.
    let target_store = state.state_store.pinned(&session_id);
    let prepared = prepare_web_attachments(&target_store, &display_content, &attachment_ids)?;
    if let Some(receipt) = queue_into_running_session(
        &state,
        &session_id,
        &prepared.content,
        &display_content,
        &attachment_ids,
    )? {
        let prompt = SafeQueuedPrompt::from(receipt.prompt);
        return Ok((
            StatusCode::ACCEPTED,
            Json(json!({
                "queued": true,
                "prompt": prompt,
                "run_id": receipt.run_id,
                "running_turn_id": receipt.turn_id,
            })),
        )
            .into_response());
    }
    let run_id = random_id("run", 18);
    let (cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
    {
        let mut manager = state.manager.lock().unwrap();
        if manager.admin_blocks_session(&session_id) || manager.session_has_runs(&session_id) {
            return Err(ApiError::new(
                StatusCode::CONFLICT,
                "Miyu is busy in this conversation",
            ));
        }
        manager.active_runs.insert(
            run_id.clone(),
            RunInfo {
                session_id: session_id.clone(),
                mode,
                audience: PromptAudience::External,
                cancel: cancel_tx,
                turn_id: None,
                queue_target: None,
                supersede: Arc::new(crate::agent::TurnSupersedeSignal::default()),
                platform_followup: None,
                operation: RunOperation::Create,
                job_wake: false,
                turn_origin: crate::tools::workspace::TurnOrigin::Human,
                job_wake_label: None,
            },
        );
    }
    if let Err(error) = target_store.reserve_user_attachments(&attachment_ids, &run_id) {
        finish_run(&state.manager, &run_id, None);
        return Err(ApiError::new(StatusCode::BAD_REQUEST, error.to_string()));
    }
    if state
        .actor_tx
        .send(ActorCommand::StartTurn {
            run_id: run_id.clone(),
            session_id,
            display_content,
            content: prepared.content,
            attachment_run_id: (!attachment_ids.is_empty()).then_some(run_id.clone()),
            mode,
            images: prepared.images,
            cwd: None,
            origin_tty: None,
            audience: PromptAudience::External,
            profile: None,
            overrides: None,
            cancel: cancel_rx,
            turn_origin: Box::new(crate::tools::workspace::TurnOrigin::Human),
        })
        .is_err()
    {
        let _ = target_store.release_user_attachments_for_run(&run_id);
        finish_run(&state.manager, &run_id, None);
        return Err(ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "agent worker is unavailable",
        ));
    }
    Ok((StatusCode::ACCEPTED, Json(json!({ "run_id": run_id }))).into_response())
}

pub(in crate::web) async fn queue_prompt(
    State(state): State<DaemonState>,
    headers: HeaderMap,
    Json(request): Json<QueuePromptRequest>,
) -> std::result::Result<Response, ApiError> {
    require_mutation(&headers, &state)?;
    let attachment_ids = request.attachment_ids;
    let display_content = validate_message_content(request.content, !attachment_ids.is_empty())?;
    let session_id = resolve_turn_session(&state, request.session_id).map_err(session_api_error)?;
    let store = state.state_store.pinned(&session_id);
    let prepared = prepare_web_attachments(&store, &display_content, &attachment_ids)?;
    // 前端把续轮挂成 live 之后,第二条起走这里;续轮要报 Owner,写死 External
    // 会被 enqueue_turn_update 的 audience 校验挡成 409。
    let audience = {
        let manager = state.manager.lock().unwrap();
        let run = manager
            .active_runs
            .get(&request.run_id)
            // 409 而不是 404:run 刚跑完就是这条路,前端认 409 才会给出
            // 「会话刚开始新的一轮」那句并重载视图(与 enqueue_turn_update
            // 自己报这条错时的状态码一致)。
            .ok_or_else(|| ApiError::new(StatusCode::CONFLICT, "active run not found"))?;
        web_followup_audience(run)
    };
    let receipt = enqueue_turn_update(
        &state,
        TurnUpdateRequest {
            run_id: request.run_id,
            turn_id: request.turn_id,
            session_id: Some(session_id),
            audience,
            content: prepared.content,
            display_content,
            attachments: Vec::new(),
            uploaded_attachment_ids: attachment_ids,
            mode: TurnUpdateMode::Followup,
        },
    )
    .map_err(|error| ApiError::new(StatusCode::CONFLICT, error.to_string()))?;
    let safe = SafeQueuedPrompt::from(receipt.prompt);
    Ok((StatusCode::ACCEPTED, Json(safe)).into_response())
}

pub(in crate::web) async fn remove_queue_prompt(
    State(state): State<DaemonState>,
    headers: HeaderMap,
    Path((run_id, turn_id, prompt_id)): Path<(String, String, String)>,
) -> std::result::Result<StatusCode, ApiError> {
    require_mutation(&headers, &state)?;
    if prompt_id.len() > 96
        || prompt_id.is_empty()
        || !prompt_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
    {
        return Err(ApiError::new(
            StatusCode::NOT_FOUND,
            "queued prompt not found",
        ));
    }
    let manager = state.manager.lock().unwrap();
    let run = manager
        .active_runs
        .get(&run_id)
        .ok_or_else(|| ApiError::new(StatusCode::NOT_FOUND, "queued prompt target not found"))?;
    if run.audience != web_followup_audience(run) || run.turn_id.as_deref() != Some(&turn_id) {
        return Err(ApiError::new(
            StatusCode::NOT_FOUND,
            "queued prompt target not found",
        ));
    }
    let target = run
        .queue_target
        .clone()
        .ok_or_else(|| ApiError::new(StatusCode::CONFLICT, "the active turn is not ready"))?;
    let session_id = run.session_id.clone();
    drop(manager);
    let removed = state
        .state_store
        .pinned(&session_id)
        .remove_queued_prompt_for_target(&target, &prompt_id)
        .map_err(ApiError::internal)?;
    if !removed {
        return Err(ApiError::new(
            StatusCode::NOT_FOUND,
            "queued prompt not found",
        ));
    };
    state.events.publish(
        "queue.removed",
        json!({
            "session_id": &*session_id,
            "run_id": run_id,
            "turn_id": turn_id,
            "prompt_id": prompt_id,
        }),
    );
    Ok(StatusCode::NO_CONTENT)
}

pub(in crate::web) async fn list_jobs_http(
    State(state): State<DaemonState>,
    headers: HeaderMap,
) -> std::result::Result<Response, ApiError> {
    require_auth(&headers, &state)?;
    Ok(Json(json!({ "jobs": tools::jobs::overview() })).into_response())
}

pub(in crate::web) async fn stop_job_http(
    State(state): State<DaemonState>,
    headers: HeaderMap,
    Path(job_id): Path<String>,
) -> std::result::Result<Response, ApiError> {
    require_mutation(&headers, &state)?;
    tools::jobs::stop_job(&job_id)
        .await
        .map_err(|error| ApiError::new(StatusCode::NOT_FOUND, safe_error_message(&error)))?;
    tools::jobs::acknowledge(&job_id);
    state
        .events
        .publish("job.acknowledged", json!({ "job_id": job_id }));
    Ok(StatusCode::NO_CONTENT.into_response())
}

/// 取消一个在飞的 run；命中就返回 true。
///
/// 顺带解除目标的武装：打断一个自主轮 = 人明确说停。不解除的话这一轮刚被
/// 掐掉、驱动器转头就开下一轮——退出 REPL 再进来看到它自己又跑起来了，
/// 正是这么来的。要接着跑得 `/goal resume`。
///
/// REPL 走 IPC、WebUI 走 HTTP，两条路都从这里过：各写一份判断迟早分叉。
/// 停掉一个会话的全部在飞 run 并等它们退场（删除会话前用）。
///
/// 「运行中不许删」对用户是个谜语——他要的是这个会话消失，跑没跑完他不关心。
/// 先替他按停止，再走正常删除。等待有限时：万一 run 卡死不退场，回落到原来
/// 的「管理占用」报错，而不是把 IPC 处理器挂死。
pub(in crate::web) async fn stop_session_runs(
    state: &DaemonState,
    session_id: &str,
    timeout: std::time::Duration,
) -> bool {
    // 先解除武装再取消：取消时仍在武装的目标会被驱动器当作「人刚重新授权」
    // 立刻重开一轮（edit 的中断重开语义靠的就是这个），和删除抢跑。
    crate::tools::goal::set_armed(session_id, false);
    {
        let manager = state.manager.lock().unwrap();
        for run in manager.active_runs.values() {
            if &*run.session_id == session_id {
                run.request_cancel();
            }
        }
    }
    // 事件驱动：run 结束由 finish_run 的 runs_changed 通知。notified()
    // 先于条件检查注册，不存在漏通知窗口；比旧 100ms 轮询响应还快。
    let deadline = tokio::time::Instant::now() + timeout;
    let notify = state.manager.lock().unwrap().runs_changed.clone();
    loop {
        let notified = notify.notified();
        if !state.manager.lock().unwrap().session_has_runs(session_id) {
            return true;
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::select! {
            _ = notified => {}
            _ = tokio::time::sleep_until(deadline) => {}
        }
    }
}

pub(in crate::web) fn cancel_run_and_disarm_goal(state: &DaemonState, run_id: &str) -> bool {
    let cancelled = {
        let manager = state.manager.lock().unwrap();
        manager.active_runs.get(run_id).map(|run| {
            run.request_cancel();
            (
                run.session_id.to_string(),
                matches!(
                    run.turn_origin,
                    crate::tools::workspace::TurnOrigin::GoalRound { .. }
                ),
            )
        })
    };
    let Some((session_id, was_goal_round)) = cancelled else {
        return false;
    };
    if was_goal_round {
        crate::tools::goal::set_armed(&session_id, false);
        tracing::info!(
            session = %session_id,
            "goal disarmed after the user cancelled an autonomous round"
        );
    }
    true
}

pub(in crate::web) async fn cancel_run(
    State(state): State<DaemonState>,
    headers: HeaderMap,
    Path(run_id): Path<String>,
) -> std::result::Result<Response, ApiError> {
    require_mutation(&headers, &state)?;
    let cancelled = cancel_run_and_disarm_goal(&state, &run_id).then_some(());
    if cancelled.is_none() {
        return Err(ApiError::new(StatusCode::NOT_FOUND, "active run not found"));
    }
    Ok((
        StatusCode::ACCEPTED,
        Json(json!({
            "run_id": run_id,
            "cancellation_requested": true,
        })),
    )
        .into_response())
}

pub(in crate::web) async fn answer_question(
    State(state): State<DaemonState>,
    headers: HeaderMap,
    Path(question_id): Path<String>,
    Json(request): Json<AnswerQuestionRequest>,
) -> std::result::Result<StatusCode, ApiError> {
    require_mutation(&headers, &state)?;
    match state
        .questions
        .answer(&question_id, request.answers, |run_id, answers| {
            state.events.publish(
                "question.answered",
                json!({
                    "run_id": run_id,
                    "question_id": question_id,
                    "answers": answers,
                }),
            );
        }) {
        Ok(()) => {}
        Err(AnswerFailure::NotFound) => {
            return Err(ApiError::new(
                StatusCode::NOT_FOUND,
                "pending question not found",
            ));
        }
        Err(AnswerFailure::Invalid(message)) => {
            return Err(ApiError::new(StatusCode::BAD_REQUEST, message));
        }
        Err(AnswerFailure::Gone) => {
            return Err(ApiError::new(
                StatusCode::CONFLICT,
                "the question is no longer awaiting an answer",
            ));
        }
    }
    Ok(StatusCode::NO_CONTENT)
}

pub(in crate::web) async fn close_question(
    State(state): State<DaemonState>,
    headers: HeaderMap,
    Path(question_id): Path<String>,
) -> std::result::Result<StatusCode, ApiError> {
    require_mutation(&headers, &state)?;
    match state.questions.close(&question_id, |run_id| {
        state.events.publish(
            "question.closed",
            json!({
                "run_id": run_id,
                "question_id": question_id,
            }),
        );
    }) {
        Ok(()) => {}
        Err(AnswerFailure::NotFound) => {
            return Err(ApiError::new(
                StatusCode::NOT_FOUND,
                "pending question not found",
            ));
        }
        Err(AnswerFailure::Gone) => {
            return Err(ApiError::new(
                StatusCode::CONFLICT,
                "the question is no longer awaiting an answer",
            ));
        }
        Err(AnswerFailure::Invalid(_)) => unreachable!("closing a question has no answer payload"),
    }
    Ok(StatusCode::NO_CONTENT)
}
