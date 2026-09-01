//! IPC 服务端。
//!
//! 终端与 daemon 之间的那条 Unix socket：终端发一个回合请求，daemon 把事件
//! 流推回去。它和 HTTP 那一半是并列的两个前端，共用底下的 actor 与状态——
//! WebUI 走 SSE，终端走这条。
//!
//! 一个连接的生命周期比看上去长：客户端可能中途断开（回合归 daemon 所有，
//! 不能因此取消）、可能重连接上已经在跑的回合、也可能只是来问一句状态。

use crate::web::*;

pub(in crate::web) fn start_ipc_server(
    state: &DaemonState,
) -> Result<(crate::ipc::WebCoreLease, TokioJoinHandle<()>)> {
    let lease = ipc::acquire_web_core(&state.paths)
        .context("another Miyu core is already running or starting")?;
    let socket_path = state.paths.ipc_socket();
    let listener = tokio::net::UnixListener::bind(&socket_path)
        .with_context(|| format!("binding Miyu IPC socket at {}", socket_path.display()))?;
    std::fs::set_permissions(&socket_path, std::fs::Permissions::from_mode(0o600))?;

    let server_state = state.clone();
    let permits = Arc::new(Semaphore::new(32));
    let task = tokio::spawn(async move {
        loop {
            let (stream, _) = match listener.accept().await {
                Ok(connection) => connection,
                Err(error) => {
                    tracing::warn!(
                        error = %error,
                        "{}",
                        t("Miyu IPC listener stopped", "Miyu IPC 监听器已停止")
                    );
                    break;
                }
            };
            let permit = match permits.clone().acquire_owned().await {
                Ok(permit) => permit,
                Err(_) => break,
            };
            let connection_state = server_state.clone();
            tokio::spawn(async move {
                let _permit = permit;
                if let Err(error) = handle_ipc_connection(connection_state, stream).await {
                    tracing::debug!(
                        error = %error,
                        "{}",
                        t(
                            "Miyu IPC connection closed with an error",
                            "Miyu IPC 连接因错误关闭"
                        )
                    );
                }
            });
        }
    });
    Ok((lease, task))
}

pub(in crate::web) async fn handle_ipc_connection(
    state: DaemonState,
    mut stream: tokio::net::UnixStream,
) -> Result<()> {
    let Some(request) = tokio::time::timeout(
        Duration::from_secs(5),
        ipc::receive::<IpcRequest>(&mut stream),
    )
    .await
    .context("timed out waiting for a Miyu IPC request")??
    else {
        return Ok(());
    };
    if request.version != ipc::PROTOCOL_VERSION
        && !matches!(&request.command, IpcCommand::Ping | IpcCommand::Shutdown)
    {
        ipc::send(
            &mut stream,
            &IpcFrame::error(format!(
                "unsupported IPC protocol version {}; expected {}",
                request.version,
                ipc::PROTOCOL_VERSION
            )),
        )
        .await?;
        return Ok(());
    }

    match request.command {
        IpcCommand::Ping => {
            ipc::send(
                &mut stream,
                &IpcFrame::Ready {
                    pid: std::process::id(),
                    web_port: state.web_port,
                    web_public: state.web_public,
                    web_bind: Some(state.web_bind),
                    build_id: ipc::BUILD_ID.to_string(),
                },
            )
            .await?;
        }
        IpcCommand::Shutdown => {
            ipc::send(&mut stream, &IpcFrame::Ack).await?;
            let _ = state.shutdown_tx.send(());
        }
        IpcCommand::JobsOverview => {
            let wake_runs = {
                let manager = state.manager.lock().unwrap();
                manager
                    .active_runs
                    .iter()
                    .filter(|(_, info)| info.job_wake)
                    .map(|(run_id, info)| {
                        json!({
                            "run_id": run_id,
                            "session_id": &*info.session_id,
                            "label": info.job_wake_label,
                        })
                    })
                    .collect::<Vec<_>>()
            };
            ipc::send(
                &mut stream,
                &IpcFrame::AdminResult {
                    state: session_state(&state.manager, &state.state_store)?,
                    data: json!({ "jobs": tools::jobs::overview(), "wake_runs": wake_runs }),
                },
            )
            .await?;
        }
        IpcCommand::FollowRun { run_id } => {
            follow_run(&state, &mut stream, run_id).await?;
        }
        IpcCommand::StopSessionJobs { session_id } => {
            let stopped = tools::jobs::stop_session_jobs(&session_id).await;
            state
                .events
                .publish("job.acknowledged", json!({ "session_id": session_id }));
            ipc::send(
                &mut stream,
                &IpcFrame::AdminResult {
                    state: session_state(&state.manager, &state.state_store)?,
                    data: json!({ "stopped": stopped }),
                },
            )
            .await?;
        }
        IpcCommand::GetStatus => {
            let qq_enabled = state.manager.lock().unwrap().config.platforms.qq.enabled;
            let qq_port = state.platforms.qq_listener.active_port();
            let connected_accounts = state.platforms.onebot.lock().unwrap().connected_accounts();
            ipc::send(
                &mut stream,
                &IpcFrame::AdminResult {
                    state: session_state(&state.manager, &state.state_store)?,
                    data: json!({
                        "runtime": {
                            "turn_engine": state.turn_engine.label(),
                        },
                        "platforms": {
                            "qq": {
                                "enabled": qq_enabled,
                                "listen_port": qq_port,
                                "connected_accounts": connected_accounts,
                            }
                        }
                    }),
                },
            )
            .await?;
        }
        IpcCommand::GetReplSession { mode } => {
            let dev = mode.as_deref() == Some("dev");
            let persona = if dev {
                crate::state::DEV_PERSONA.to_string()
            } else {
                active_persona_scope(&state)
            };
            let store = &state.state_store;
            // 终端集成、普通、开发是**三条并行车道**,各有各的会话指针。指针
            // 缺失或指向已删/已归档的会话时,一律自举一个新的本地会话。
            //
            // normal 以前在这两处都退到 `store.session_id()`——那是终端集成
            // (shellhook)的车道。于是第一次 `miyu normal` 就把 REPL 焊在终端
            // 会话上,两边的对话混成一摊。dev 早就是自举的,normal 没跟上。
            //
            // 空名字是有意的:首条消息会自动命名(与 dev 同路)。不动
            // `store.session_id()`,终端车道保持原样;要回去用 `/session`。
            let session_id = store
                .ensure_repl_session(&persona)
                .map_err(|error| anyhow::anyhow!(safe_error_message(&error)))?;
            // 指针有效但会话已归档/不是本地会话时同样换一条新的,别把 REPL
            // 卡在一个进不去的会话上。
            let target = ipc::SessionRef::Id { id: session_id };
            let session_id = match resolve_available_local_session_ref(&state, &target) {
                Ok(record) => record.session_id,
                Err(reason) => {
                    // 这条分支会把车道指针换成一条**新建的空会话**,于是重进
                    // REPL 时没有历史可放、上键调不出输入记录(输入历史按会话
                    // id 分文件)、`/undo` 也没得撤销——用户 08-26 报的 dev 三
                    // 连全出自这里。原先丢掉了失败原因,只能靠翻库反推;把它
                    // 打出来,一次就能定论。
                    tracing::info!(
                        target: "miyu::qq",
                        %persona,
                        target = ?target,
                        %reason,
                        "{}",
                        t(
                            "REPL session pointer could not be resolved; starting a fresh session",
                            "REPL 会话指针解析失败,改用新建会话"
                        )
                    );
                    store
                        .new_repl_session(&persona)
                        .map_err(|error| anyhow::anyhow!(safe_error_message(&error)))?
                }
            };
            let _ = store.set_repl_session(&persona, &session_id);
            ipc::send(
                &mut stream,
                &IpcFrame::AdminResult {
                    state: session_state_for(&state, &session_id)?,
                    data: json!({}),
                },
            )
            .await?;
        }
        IpcCommand::GetSessionState { target } => {
            let record = match resolve_available_local_session_ref(&state, &target) {
                Ok(record) => record,
                Err(message) => {
                    ipc::send(&mut stream, &IpcFrame::error(message)).await?;
                    return Ok(());
                }
            };
            ipc::send(
                &mut stream,
                &IpcFrame::AdminResult {
                    state: session_state_for(&state, &record.session_id)?,
                    data: json!({}),
                },
            )
            .await?;
        }
        IpcCommand::ReloadConfig => {
            let current_config = state.manager.lock().unwrap().config.clone();
            let next_config = match AppConfig::load_or_default(&state.paths) {
                Ok(config) => config,
                Err(error) => {
                    ipc::send(
                        &mut stream,
                        &IpcFrame::error(format!(
                            "invalid configuration: {}",
                            safe_error_message(error)
                        )),
                    )
                    .await?;
                    return Ok(());
                }
            };
            let prompts = match read_prompt_documents(&next_config, &state.paths) {
                Ok(prompts) => prompts,
                Err(error) => {
                    ipc::send(&mut stream, &IpcFrame::error(safe_error_message(error))).await?;
                    return Ok(());
                }
            };
            let qq_listener = match state
                .platforms
                .qq_listener
                .prepare(
                    &state,
                    Some(&current_config.platforms.qq),
                    &next_config.platforms.qq,
                )
                .await
            {
                Ok(listener) => listener,
                Err(error) => {
                    ipc::send(
                        &mut stream,
                        &IpcFrame::error(format!(
                            "Tencent QQ listener configuration failed: {}",
                            safe_error_message(error)
                        )),
                    )
                    .await?;
                    return Ok(());
                }
            };
            // Light reservation: reloading is allowed while turns run. Running
            // turns keep the config snapshot they started with; new turns pick
            // up the reloaded config. Persona layout changes interrupt running
            // turns inside the ApplyConfig handler instead of failing here.
            if let Err(error) = reserve_admin_light(&state.manager) {
                ipc::send(
                    &mut stream,
                    &IpcFrame::coded_error(ipc::ErrorCode::Busy, error.message),
                )
                .await?;
                return Ok(());
            }
            let (reply, receiver) = oneshot::channel();
            if state
                .actor_tx
                .send(ActorCommand::ApplyConfig {
                    config: Box::new(next_config),
                    prompts,
                    reset_conversation: false,
                    reply,
                })
                .is_err()
            {
                release_admin(&state.manager);
                ipc::send(
                    &mut stream,
                    &IpcFrame::error("Miyu core worker is unavailable"),
                )
                .await?;
                return Ok(());
            }
            match receiver.await {
                Ok(Ok(())) => {
                    qq_listener.commit();
                    match session_state(&state.manager, &state.state_store) {
                        Ok(session) => {
                            ipc::send(
                                &mut stream,
                                &IpcFrame::AdminResult {
                                    state: session,
                                    data: json!({}),
                                },
                            )
                            .await?
                        }
                        Err(error) => {
                            ipc::send(&mut stream, &IpcFrame::error(safe_error_message(error)))
                                .await?
                        }
                    }
                }
                Ok(Err(AdminFailure::Invalid(message) | AdminFailure::Internal(message))) => {
                    ipc::send(&mut stream, &IpcFrame::error(message)).await?
                }
                Err(_) => {
                    release_admin(&state.manager);
                    ipc::send(
                        &mut stream,
                        &IpcFrame::error("Miyu core stopped while reloading configuration"),
                    )
                    .await?
                }
            }
        }
        IpcCommand::ResetConversation { target } => {
            let target_record = match resolve_available_local_session_ref(&state, &target) {
                Ok(record) => record,
                Err(message) => {
                    ipc::send(&mut stream, &IpcFrame::error(message)).await?;
                    return Ok(());
                }
            };
            let session_id: Arc<str> = target_record.session_id.into();
            reserve_admin_for_session(&state.manager, &session_id)
                .map_err(|error| anyhow::anyhow!(error.message))?;
            let (reply, receiver) = oneshot::channel();
            if state
                .actor_tx
                .send(ActorCommand::ResetConversation {
                    session_id: session_id.clone(),
                    reply,
                })
                .is_err()
            {
                release_admin(&state.manager);
                anyhow::bail!("Miyu core worker is unavailable");
            }
            match receiver.await {
                Ok(Ok(())) => {
                    ipc::send(
                        &mut stream,
                        &IpcFrame::AdminResult {
                            state: session_state_for(&state, &session_id)?,
                            data: json!({}),
                        },
                    )
                    .await?
                }
                Ok(Err(AdminFailure::Invalid(message) | AdminFailure::Internal(message))) => {
                    ipc::send(&mut stream, &IpcFrame::error(message)).await?
                }
                Err(_) => {
                    release_admin(&state.manager);
                    anyhow::bail!("Miyu core stopped while resetting the conversation");
                }
            }
        }
        IpcCommand::WipePersona => {
            let config = state.manager.lock().unwrap().config.clone();
            let current = state.state_store.session_id().to_string();
            match reset_platform_persona_state(&state, &config).await {
                Ok(sessions) => {
                    ipc::send(
                        &mut stream,
                        &IpcFrame::AdminResult {
                            state: session_state_for(&state, &current)?,
                            data: json!({ "sessions": sessions }),
                        },
                    )
                    .await?;
                }
                Err(PlatformPersonaResetError::Busy) => {
                    ipc::send(
                        &mut stream,
                        &IpcFrame::coded_error(ipc::ErrorCode::Busy, ipc::ADMIN_BUSY_MESSAGE),
                    )
                    .await?;
                }
                Err(PlatformPersonaResetError::Unavailable) => {
                    anyhow::bail!("Miyu core worker is unavailable");
                }
                Err(PlatformPersonaResetError::Internal(message)) => {
                    ipc::send(&mut stream, &IpcFrame::error(message)).await?;
                }
            }
        }
        IpcCommand::Undo { target } => {
            let record = match resolve_available_local_session_ref(&state, &target) {
                Ok(record) => record,
                Err(message) => {
                    ipc::send(&mut stream, &IpcFrame::error(message)).await?;
                    return Ok(());
                }
            };
            let session_id: Arc<str> = record.session_id.into();
            reserve_admin_for_session(&state.manager, &session_id)
                .map_err(|error| anyhow::anyhow!(error.message))?;
            let (reply, receiver) = oneshot::channel();
            if state
                .actor_tx
                .send(ActorCommand::Undo {
                    session_id: session_id.clone(),
                    reply,
                })
                .is_err()
            {
                release_admin(&state.manager);
                anyhow::bail!("Miyu core worker is unavailable");
            }
            match receiver.await {
                Ok(Ok(data)) => {
                    ipc::send(
                        &mut stream,
                        &IpcFrame::AdminResult {
                            state: session_state_for(&state, &session_id)?,
                            data,
                        },
                    )
                    .await?
                }
                Ok(Err(AdminFailure::Invalid(message) | AdminFailure::Internal(message))) => {
                    ipc::send(&mut stream, &IpcFrame::error(message)).await?
                }
                Err(_) => {
                    release_admin(&state.manager);
                    anyhow::bail!("Miyu core stopped while undoing the conversation");
                }
            }
        }
        IpcCommand::Pop { target, turn_ids } => {
            let record = match resolve_available_local_session_ref(&state, &target) {
                Ok(record) => record,
                Err(message) => {
                    ipc::send(&mut stream, &IpcFrame::error(message)).await?;
                    return Ok(());
                }
            };
            let session_id: Arc<str> = record.session_id.into();
            reserve_admin_for_session(&state.manager, &session_id)
                .map_err(|error| anyhow::anyhow!(error.message))?;
            let (reply, receiver) = oneshot::channel();
            if state
                .actor_tx
                .send(ActorCommand::Pop {
                    session_id: session_id.clone(),
                    turn_ids,
                    reply,
                })
                .is_err()
            {
                release_admin(&state.manager);
                anyhow::bail!("Miyu core worker is unavailable");
            }
            match receiver.await {
                Ok(Ok(data)) => {
                    ipc::send(
                        &mut stream,
                        &IpcFrame::AdminResult {
                            state: session_state_for(&state, &session_id)?,
                            data,
                        },
                    )
                    .await?
                }
                Ok(Err(AdminFailure::Invalid(message) | AdminFailure::Internal(message))) => {
                    ipc::send(&mut stream, &IpcFrame::error(message)).await?
                }
                Err(_) => {
                    release_admin(&state.manager);
                    anyhow::bail!("Miyu core stopped while popping the conversation");
                }
            }
        }
        IpcCommand::Compact { target } => {
            let record = match resolve_available_local_session_ref(&state, &target) {
                Ok(record) => record,
                Err(message) => {
                    ipc::send(&mut stream, &IpcFrame::error(message)).await?;
                    return Ok(());
                }
            };
            let session_id: Arc<str> = record.session_id.into();
            reserve_admin_for_session(&state.manager, &session_id)
                .map_err(|error| anyhow::anyhow!(error.message))?;
            let (reply, receiver) = oneshot::channel();
            if state
                .actor_tx
                .send(ActorCommand::Compact {
                    session_id: session_id.clone(),
                    reply,
                })
                .is_err()
            {
                release_admin(&state.manager);
                anyhow::bail!("Miyu core worker is unavailable");
            }
            match receiver.await {
                Ok(Ok(data)) => {
                    ipc::send(
                        &mut stream,
                        &IpcFrame::AdminResult {
                            state: session_state_for(&state, &session_id)?,
                            data,
                        },
                    )
                    .await?
                }
                Ok(Err(AdminFailure::Invalid(message) | AdminFailure::Internal(message))) => {
                    ipc::send(&mut stream, &IpcFrame::error(message)).await?
                }
                Err(_) => {
                    release_admin(&state.manager);
                    anyhow::bail!("Miyu core stopped while compacting the conversation");
                }
            }
        }
        IpcCommand::StartTurn {
            content,
            mode,
            images,
            cwd,
            session_id,
            origin_tty,
        } => {
            handle_ipc_turn(
                &state,
                &mut stream,
                content,
                mode,
                images,
                cwd,
                session_id,
                origin_tty,
            )
            .await?;
        }
        IpcCommand::QueueTurnUpdate {
            run_id,
            turn_id,
            content,
            display_content,
            images,
            supersede,
        } => {
            let attachments = images
                .into_iter()
                .flatten()
                .map(|image| match image {
                    ImageAttachment::Binary { mime, data } => {
                        crate::state::QueuedPromptAttachment::Binary {
                            mime,
                            data_base64: base64::engine::general_purpose::STANDARD.encode(data),
                        }
                    }
                    ImageAttachment::Path { path } => {
                        crate::state::QueuedPromptAttachment::Path { path }
                    }
                })
                .collect();
            match enqueue_turn_update(
                &state,
                TurnUpdateRequest {
                    run_id,
                    turn_id,
                    session_id: None,
                    audience: PromptAudience::Owner,
                    content,
                    display_content,
                    attachments,
                    uploaded_attachment_ids: Vec::new(),
                    mode: if supersede {
                        TurnUpdateMode::Supersede
                    } else {
                        TurnUpdateMode::Followup
                    },
                },
            ) {
                Ok(receipt) => {
                    ipc::send(
                        &mut stream,
                        &IpcFrame::TurnUpdateAccepted {
                            run_id: receipt.run_id,
                            turn_id: receipt.turn_id,
                            prompt_id: receipt.prompt.prompt_id,
                            seq: receipt.prompt.seq,
                            submitted_at: receipt.prompt.submitted_at,
                        },
                    )
                    .await?;
                }
                Err(error) => {
                    ipc::send(&mut stream, &IpcFrame::error(error.to_string())).await?;
                }
            }
        }
        IpcCommand::Cancel { run_id } => {
            if cancel_run_and_disarm_goal(&state, &run_id) {
                ipc::send(&mut stream, &IpcFrame::Ack).await?;
            } else {
                ipc::send(&mut stream, &IpcFrame::error("active run not found")).await?;
            }
        }
        IpcCommand::CloseQuestion { question_id } => {
            let _ = state.questions.close(&question_id, |run_id| {
                state.events.publish(
                    "question.closed",
                    json!({
                        "run_id": run_id,
                        "question_id": question_id,
                    }),
                );
            });
            ipc::send(&mut stream, &IpcFrame::Ack).await?
        }
        IpcCommand::AnswerQuestion {
            question_id,
            answers,
        } => match state
            .questions
            .answer(&question_id, answers, |run_id, answers| {
                state.events.publish(
                    "question.answered",
                    json!({
                        "run_id": run_id,
                        "question_id": question_id,
                        "answers": answers,
                    }),
                );
            }) {
            Ok(()) => ipc::send(&mut stream, &IpcFrame::Ack).await?,
            Err(error) => {
                let message = match error {
                    AnswerFailure::NotFound => "pending question not found".to_string(),
                    AnswerFailure::Invalid(message) => message,
                    AnswerFailure::Gone => "pending question is no longer active".to_string(),
                };
                ipc::send(&mut stream, &IpcFrame::error(message)).await?;
            }
        },
        session_command => match handle_session_command(&state, session_command).await {
            Ok(data) => {
                ipc::send(
                    &mut stream,
                    &IpcFrame::AdminResult {
                        state: session_state(&state.manager, &state.state_store)?,
                        data,
                    },
                )
                .await?
            }
            Err(message) => ipc::send(&mut stream, &IpcFrame::error(message)).await?,
        },
    }
    Ok(())
}

pub(in crate::web) async fn switch_session_via_actor(
    state: &DaemonState,
    session_id: String,
) -> std::result::Result<(), String> {
    reserve_admin_light(&state.manager).map_err(|error| error.message)?;
    let (reply, receiver) = oneshot::channel();
    if state
        .actor_tx
        .send(ActorCommand::SwitchSession {
            session_id,
            release_reservation: true,
            reply,
        })
        .is_err()
    {
        release_admin(&state.manager);
        return Err("Miyu core worker is unavailable".to_string());
    }
    match receiver.await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(AdminFailure::Invalid(message) | AdminFailure::Internal(message))) => Err(message),
        Err(_) => {
            release_admin(&state.manager);
            Err("Miyu core stopped while switching sessions".to_string())
        }
    }
}

pub(in crate::web) async fn switch_session_via_actor_reserved(
    state: &DaemonState,
    session_id: String,
) -> std::result::Result<(), String> {
    let (reply, receiver) = oneshot::channel();
    if state
        .actor_tx
        .send(ActorCommand::SwitchSession {
            session_id,
            release_reservation: false,
            reply,
        })
        .is_err()
    {
        return Err("Miyu core worker is unavailable".to_string());
    }
    match receiver.await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(AdminFailure::Invalid(message) | AdminFailure::Internal(message))) => Err(message),
        Err(_) => Err("Miyu core stopped while switching sessions".to_string()),
    }
}

#[allow(clippy::too_many_arguments)]
pub(in crate::web) async fn handle_ipc_turn(
    state: &DaemonState,
    stream: &mut tokio::net::UnixStream,
    content: String,
    mode: String,
    images: Vec<Option<ImageAttachment>>,
    cwd: Option<std::path::PathBuf>,
    session_id: Option<String>,
    origin_tty: Option<crate::ipc::OriginTty>,
) -> Result<()> {
    let content = match validate_content(content) {
        Ok(content) => content,
        Err(error) => {
            ipc::send(stream, &IpcFrame::error(error.message)).await?;
            return Ok(());
        }
    };
    let mode = match parse_mode(&mode) {
        Ok(mode) => mode,
        Err(error) => {
            ipc::send(stream, &IpcFrame::error(error.message)).await?;
            return Ok(());
        }
    };
    // Turns run in parallel — several may be active at once, including in
    // the same session (placeholder semantics). The only rejection is a
    // transient admin mutation window.
    let run_id = random_id("run", 18);
    let session_id = match resolve_turn_session(state, session_id) {
        Ok(session_id) => session_id,
        // (mode 在会话解析后按会话记录强制,见下。)
        Err(message) => {
            ipc::send(stream, &IpcFrame::error(message)).await?;
            return Ok(());
        }
    };
    // 会话模式创建时定死:以会话记录为准强制,客户端传参只是遗留字段。
    let mode = turn_mode_for_session(&state.state_store, &session_id, mode);
    let (cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
    let busy = {
        let mut manager = state.manager.lock().unwrap();
        if manager.admin_busy {
            true
        } else {
            manager.active_runs.insert(
                run_id.clone(),
                RunInfo {
                    session_id: session_id.clone(),
                    mode,
                    audience: PromptAudience::Owner,
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
            false
        }
    };
    if busy {
        ipc::send(
            stream,
            &IpcFrame::coded_error(ipc::ErrorCode::Busy, ipc::ADMIN_BUSY_MESSAGE),
        )
        .await?;
        return Ok(());
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
            mode,
            images,
            cwd,
            origin_tty,
            audience: PromptAudience::Owner,
            profile: None,
            cancel: cancel_rx,
            turn_origin: Box::new(crate::tools::workspace::TurnOrigin::Human),
        })
        .is_err()
    {
        finish_run(&state.manager, &run_id, None);
        ipc::send(stream, &IpcFrame::error("Miyu core worker is unavailable")).await?;
        return Ok(());
    }
    let mut run_guard = IpcRunGuard {
        manager: state.manager.clone(),
        run_id: run_id.clone(),
        finished: false,
    };
    ipc::send(
        stream,
        &IpcFrame::Accepted {
            run_id: run_id.clone(),
            turn_id: None,
        },
    )
    .await?;

    let mut last_id = after;
    loop {
        let record = if let Some(record) = subscription.pending.pop_front() {
            record
        } else {
            match subscription.receiver.recv().await {
                Ok(record) => record,
                Err(broadcast::error::RecvError::Lagged(_)) => {
                    subscription.pending = state.events.replay_after(last_id);
                    continue;
                }
                Err(broadcast::error::RecvError::Closed) => break,
            }
        };
        if record.kind == "resync_required" {
            // 共享事件广播缓冲(4096 条/4MB)被**别的会话**的密集事件冲爆、本
            // 连接一时落后时,replay 会返回 resync。以前这里直接给前端发
            // 「the turn was cancelled」并断流——可本连接的回合仍在 daemon 里
            // 好好跑着,于是并发的另一个会话被误报「已取消」(09-01 跨会话
            // 误取消根因:两个会话并行,一个刷屏把另一个的事件挤出缓冲)。
            //
            // 正确做法:从最新事件处**续流**,不谎报取消。丢失的中间 chunk
            // 不再渲染(前端可能有一小段空白),但回合正常收尾,run.completed
            // 照常到达。只有回合确已结束(连终态事件都被挤掉)时才收尾——
            // 且发的是「刷新可见」而非「已取消」,前端据此重取最终状态。
            last_id = serde_json::from_str::<Value>(&record.data)
                .ok()
                .and_then(|data| data.get("latest_event_id").and_then(Value::as_u64))
                .unwrap_or(last_id);
            let still_running = state
                .manager
                .lock()
                .unwrap()
                .active_runs
                .contains_key(&run_id);
            if still_running {
                continue;
            }
            ipc::send(
                stream,
                &IpcFrame::error(
                    "Miyu core event history was exhausted; the reply already finished — refresh to see it",
                ),
            )
            .await?;
            break;
        }
        last_id = record.id;
        let Ok(data) = serde_json::from_str::<Value>(&record.data) else {
            continue;
        };
        if data.get("run_id").and_then(Value::as_str) != Some(run_id.as_str()) {
            continue;
        }
        let terminal = matches!(
            record.kind.as_str(),
            "run.completed" | "run.failed" | "run.cancelled"
        );
        ipc::send(
            stream,
            &IpcFrame::Event {
                id: record.id,
                kind: record.kind,
                data,
            },
        )
        .await?;
        if terminal {
            run_guard.finish();
            break;
        }
    }
    Ok(())
}
