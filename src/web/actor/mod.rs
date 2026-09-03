//! 回合 actor：daemon 里真正跑对话的那个循环。
//!
//! 所有回合都在这一个 actor 里排队执行——不是为了省资源，是为了让「同一时刻
//! 只有一个回合在动这个会话」成为结构性保证，而不是靠调用方自觉加锁。
//!
//! 后台任务完成后的唤醒也在这里：子代理跑完、闹钟到点，要把结果送回发起它的
//! 那个终端或平台会话，而那个会话可能早就换了、关了、或者正在跑别的回合。

mod job_wake;
pub(in crate::web) use job_wake::*;

use crate::web::*;

#[allow(clippy::too_many_arguments)]
pub(in crate::web) async fn actor_loop(
    mut config: AppConfig,
    paths: MiyuPaths,
    state_store: StateStore,
    manager: Arc<Mutex<ManagerState>>,
    events: EventHub,
    questions: QuestionBroker,
    turn_engine: TurnEngineState,
    memory_organizer: Option<MemoryOrganizerHandle>,
    mut receiver: mpsc::UnboundedReceiver<ActorCommand>,
) {
    let mut agent: Option<Agent> = None;
    let resource_cache = Arc::new(Mutex::new(TurnResourceCache::default()));
    while let Some(command) = receiver.recv().await {
        match command {
            ActorCommand::StartTurn {
                run_id,
                session_id,
                content,
                display_content,
                attachment_run_id,
                mode,
                images,
                cwd,
                origin_tty,
                audience,
                profile,
                cancel,
                turn_origin,
            } => {
                // Stale-turn recovery is owner-pid safe. Prompt maintenance is
                // performed after per-turn platform overrides are applied.
                let _ = state_store.recover_stale_turns();
                // 会话模式定死的最终防线:无论谁构造的 StartTurn(ipc/唤醒/
                // goal 驱动器),都按会话记录重derive 一次。
                let mode = turn_mode_for_session(&state_store, &session_id, mode);
                let store = state_store.pinned_for_turn(&session_id);
                // Per-turn workspace: a workspace bound to the session wins,
                // otherwise the calling client's cwd, otherwise the daemon
                // process cwd. The resolved path scopes the whole turn task.
                let workspace = store
                    .session_record(&session_id)
                    .ok()
                    .flatten()
                    .and_then(|record| record.workspace.map(std::path::PathBuf::from))
                    .filter(|path| path.is_dir())
                    .or_else(|| cwd.filter(|path| path.is_dir()))
                    .or_else(|| std::env::current_dir().ok())
                    .unwrap_or_else(|| std::path::PathBuf::from("."));
                // 平台回合的真实发起者。后台任务 spawn 时从 task-local 捕获,
                // 完成唤醒凭它还原身份(issue #29)。
                let platform_sender = profile
                    .as_ref()
                    .and_then(|profile| profile.platform.as_ref())
                    .map(|platform| platform.sender_id.clone());
                let task = run_turn_task(
                    config.clone(),
                    paths.clone(),
                    store,
                    state_store.clone(),
                    manager.clone(),
                    events.clone(),
                    questions.clone(),
                    run_id,
                    session_id.clone(),
                    TurnTaskInput::Create {
                        content,
                        display_content,
                        attachment_run_id,
                        images,
                    },
                    mode,
                    audience,
                    profile,
                    cancel,
                    resource_cache.clone(),
                    turn_engine.clone(),
                    memory_organizer.clone(),
                );
                tokio::task::spawn_local(crate::tools::workspace::with_workspace(
                    workspace,
                    crate::tools::workspace::with_session(
                        session_id,
                        crate::tools::workspace::with_origin_tty(
                            origin_tty,
                            crate::tools::workspace::with_platform_sender(
                                platform_sender,
                                crate::tools::workspace::with_turn_origin(*turn_origin, task),
                            ),
                        ),
                    ),
                ));
            }
            ActorCommand::RedoTurn {
                run_id,
                session_id,
                candidate,
                prompts,
                mode,
                cancel,
            } => {
                let _ = state_store.recover_stale_turns();
                let store = state_store.pinned_for_turn(&session_id);
                let workspace = store
                    .session_record(&session_id)
                    .ok()
                    .flatten()
                    .and_then(|record| record.workspace.map(std::path::PathBuf::from))
                    .filter(|path| path.is_dir())
                    .or_else(|| std::env::current_dir().ok())
                    .unwrap_or_else(|| std::path::PathBuf::from("."));
                let task = run_turn_task(
                    config.clone(),
                    paths.clone(),
                    store,
                    state_store.clone(),
                    manager.clone(),
                    events.clone(),
                    questions.clone(),
                    run_id,
                    session_id.clone(),
                    TurnTaskInput::Redo { candidate, prompts },
                    mode,
                    PromptAudience::External,
                    None,
                    cancel,
                    resource_cache.clone(),
                    turn_engine.clone(),
                    memory_organizer.clone(),
                );
                tokio::task::spawn_local(crate::tools::workspace::with_workspace(
                    workspace,
                    crate::tools::workspace::with_session(session_id, task),
                ));
            }
            ActorCommand::SetModels { models, reply } => {
                let result = rebuild_for_models(
                    &mut agent,
                    &mut config,
                    &paths,
                    &state_store,
                    &manager,
                    &models,
                );
                if result.is_ok() {
                    resource_cache.lock().unwrap().clear();
                    turn_engine.set(if agent.is_some() {
                        TurnEngineState::READY
                    } else {
                        TurnEngineState::COLD
                    });
                }
                release_admin(&manager);
                let _ = reply.send(result);
            }
            ActorCommand::SetThinkingVariants { updates, reply } => {
                let result = apply_thinking_variant_updates(&mut agent, &config, &paths, &updates);
                if result.is_ok() {
                    resource_cache.lock().unwrap().clear();
                }
                release_admin(&manager);
                let _ = reply.send(result);
            }
            ActorCommand::ApplyConfig {
                config: next_config,
                prompts,
                reset_conversation,
                reply,
            } => {
                // Persona layout changes migrate or delete session state that
                // running turns may be standing on, so those interrupt every
                // running turn before applying ("save after interrupting").
                // All other changes hot-apply: running turns keep the config
                // snapshot they cloned at start and later turns use the new
                // configuration.
                if config_change_requires_interrupt(&config, &next_config, &paths, &prompts) {
                    for info in manager.lock().unwrap().active_runs.values() {
                        info.request_cancel();
                    }
                    for _ in 0..100 {
                        if manager.lock().unwrap().active_runs.is_empty() {
                            break;
                        }
                        tokio::time::sleep(Duration::from_millis(50)).await;
                    }
                }
                let result = rebuild_for_config(
                    &mut agent,
                    &mut config,
                    &paths,
                    &state_store,
                    &manager,
                    &events,
                    *next_config,
                    &prompts,
                    reset_conversation,
                );
                if result.is_ok() {
                    resource_cache.lock().unwrap().clear();
                    turn_engine.set(if agent.is_some() {
                        TurnEngineState::READY
                    } else {
                        TurnEngineState::COLD
                    });
                    if let Some(handle) = memory_organizer.as_ref() {
                        handle.wake(config.clone(), paths.clone(), state_store.clone());
                    }
                }
                release_admin(&manager);
                let _ = reply.send(result);
            }
            ActorCommand::ResetConversation { session_id, reply } => {
                let result = reset_actor_conversation(
                    &mut agent,
                    &config,
                    &paths,
                    &state_store,
                    &manager,
                    &events,
                    &session_id,
                );
                release_admin(&manager);
                let _ = reply.send(result);
            }
            ActorCommand::ResetPersonaState {
                config: reset_config,
                reply,
            } => {
                let result = reset_actor_persona_state(
                    &mut agent,
                    &config,
                    &reset_config,
                    &paths,
                    &state_store,
                    &manager,
                    &events,
                );
                if result.is_ok() {
                    resource_cache.lock().unwrap().clear();
                }
                release_admin(&manager);
                let _ = reply.send(result);
            }
            ActorCommand::ClearSessionContent { session_id, reply } => {
                let result = clear_actor_session_content(
                    &mut agent,
                    &config,
                    &paths,
                    &state_store,
                    &manager,
                    &session_id,
                );
                release_admin(&manager);
                let _ = reply.send(result);
            }
            ActorCommand::SwitchSession {
                session_id,
                release_reservation,
                reply,
            } => {
                let result = switch_actor_session(
                    agent.as_ref(),
                    &config,
                    &paths,
                    &state_store,
                    &manager,
                    &events,
                    &session_id,
                );
                if release_reservation {
                    release_admin(&manager);
                }
                let _ = reply.send(result);
            }
            ActorCommand::Shutdown => {
                // Cancel every running turn, then drain briefly so they can
                // persist their interrupted state before the runtime drops.
                for info in manager.lock().unwrap().active_runs.values() {
                    info.request_cancel();
                }
                for _ in 0..100 {
                    if manager.lock().unwrap().active_runs.is_empty() {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
                break;
            }
            ActorCommand::Undo { session_id, reply } => {
                let result = (|| -> std::result::Result<Value, AdminFailure> {
                    let store = state_store.pinned(&session_id);
                    let (removed, prompt) = store
                        .undo_last_turn()
                        .map_err(|error| AdminFailure::Internal(safe_error_message(&error)))?;
                    if &*state_store.session_id() == &*session_id {
                        manager.lock().unwrap().context =
                            actor_context(&agent, &config, &paths, &state_store).map_err(
                                |error| AdminFailure::Internal(safe_error_message(&error)),
                            )?;
                    }
                    Ok(json!({ "removed": removed, "prompt": prompt }))
                })();
                release_admin(&manager);
                let _ = reply.send(result);
            }
            ActorCommand::Pop {
                session_id,
                turn_ids,
                reply,
            } => {
                let result = (|| -> std::result::Result<Value, AdminFailure> {
                    if turn_ids.is_empty() {
                        return Ok(json!({ "turns": 0, "archived": false }));
                    }
                    let store = state_store.pinned(&session_id);
                    let turns = store
                        .oldest_evictable_visible_turns(usize::MAX)
                        .map_err(|error| AdminFailure::Internal(safe_error_message(&error)))?;
                    let selected = turns
                        .into_iter()
                        .filter(|turn| turn_ids.iter().any(|id| id == &turn.turn_id))
                        .collect::<Vec<_>>();
                    if selected.len() != turn_ids.len() {
                        return Err(AdminFailure::Invalid(
                            "one or more conversation turns are no longer available".to_string(),
                        ));
                    }
                    let memory = MemoryStore::new(&config, &paths);
                    let memory_config = config.memory_config();
                    archive_and_delete_visible_turns(&store, &memory, &selected)
                        .map_err(|error| AdminFailure::Internal(safe_error_message(&error)))?;
                    if &*state_store.session_id() == &*session_id {
                        manager.lock().unwrap().context =
                            actor_context(&agent, &config, &paths, &state_store).map_err(
                                |error| AdminFailure::Internal(safe_error_message(&error)),
                            )?;
                    }
                    let data = json!({
                        "turns": selected.len(),
                        "archived": memory_config.enabled && memory_config.evicted_context_enabled
                    });
                    let mut event_data = data.clone();
                    event_data["session_id"] = json!(&*session_id);
                    events.publish("conversation.pop", event_data);
                    Ok(data)
                })();
                release_admin(&manager);
                let _ = reply.send(result);
            }
            ActorCommand::Compact { session_id, reply } => {
                let result = async {
                    let updates_default = &*state_store.session_id() == &*session_id;
                    let compact = if updates_default {
                        let agent = ensure_actor_agent(
                            &mut agent,
                            &config,
                            &paths,
                            &state_store,
                            &turn_engine,
                        )?;
                        let compact = agent
                            .compact_now(|_| Ok(()))
                            .await
                            .map_err(|error| AdminFailure::Internal(safe_error_message(&error)))?;
                        manager.lock().unwrap().context = current_context(agent)
                            .map_err(|error| AdminFailure::Internal(safe_error_message(&error)))?;
                        compact
                    } else {
                        let store = state_store.pinned(&session_id);
                        let target_agent = build_actor_agent(&config, &paths, &store)
                            .map_err(|error| AdminFailure::Internal(safe_error_message(&error)))?;
                        target_agent
                            .compact_now(|_| Ok(()))
                            .await
                            .map_err(|error| AdminFailure::Internal(safe_error_message(&error)))?
                    };
                    Ok::<Value, AdminFailure>(json!({
                        "compacted": compact.is_some(),
                        "usage": compact.as_ref().and_then(|result| result.usage.clone()),
                        "usage_estimated": compact
                            .as_ref()
                            .map(|result| result.usage_estimated)
                            .unwrap_or(false)
                    }))
                }
                .await;
                // 压缩重写了消息数组,已经打开这个会话的界面必须重新拉一次,
                // 否则屏幕上还是压缩前那串回合——用户会以为命令没生效。
                // 只在**真压缩了**的时候发:上下文没到水位时 compact_now 什么
                // 也不做,那种情况下发事件会让所有前端白刷一次。
                if result
                    .as_ref()
                    .ok()
                    .and_then(|data| data.get("compacted"))
                    .and_then(Value::as_bool)
                    .unwrap_or(false)
                {
                    events.publish(
                        "conversation.compacted",
                        json!({ "session_id": &*session_id }),
                    );
                }
                release_admin(&manager);
                let _ = reply.send(result);
            }
        }
    }
}

pub(in crate::web) fn switch_actor_session(
    agent: Option<&Agent>,
    config: &AppConfig,
    paths: &MiyuPaths,
    state_store: &StateStore,
    manager: &Arc<Mutex<ManagerState>>,
    events: &EventHub,
    session_id: &str,
) -> std::result::Result<(), AdminFailure> {
    // Notes: switching deliberately does not touch updated_at (viewing must
    // not reorder the session list), and runs no turn-entry maintenance —
    // switching is allowed while turns are running, so a prompt-change reset
    // here could wipe a session mid-turn.
    let switch = || -> Result<ContextSnapshot> {
        state_store.switch_session(session_id)?;
        agent.map_or_else(|| cold_context(config, paths, state_store), current_context)
    };
    let context = switch().map_err(|error| AdminFailure::Internal(safe_error_message(&error)))?;
    let mut manager_state = manager.lock().unwrap();
    manager_state.context = context;
    let persona_scope = manager_state.config.active_persona_scope();
    manager_state
        .persona_session_ids
        .insert(persona_scope.clone(), session_id.to_string());
    drop(manager_state);
    state_store
        .set_persona_current_session(&persona_scope, session_id)
        .map_err(|error| AdminFailure::Internal(safe_error_message(error)))?;
    events.publish(
        "session.current_changed",
        json!({ "session_id": session_id }),
    );
    Ok(())
}

pub(in crate::web) fn reset_actor_conversation(
    agent: &mut Option<Agent>,
    config: &AppConfig,
    paths: &MiyuPaths,
    state_store: &StateStore,
    manager: &Arc<Mutex<ManagerState>>,
    events: &EventHub,
    session_id: &str,
) -> std::result::Result<(), AdminFailure> {
    // "Reset" means the conversation starts over, so everything scoped to it
    // goes: history, per-session usage, and the recall caches that only make
    // sense against that history. This used to be gated on a flag that was
    // really asking "did the caller address the session as `Current`?" — an
    // implementation detail of each frontend, which left `/reset` and the
    // WebUI clearing strictly less than `miyu reset`. Platform sessions never
    // reach this command (both entry points reject them) and clear themselves
    // through `ClearSessionContent`, so there is nothing left for a flag to
    // protect.
    let mut reset = || -> Result<Option<ContextSnapshot>> {
        let store = state_store.pinned(session_id);
        store.clear_session_content()?;
        store.reset_conversation_usage()?;
        // 待办存在库外面，`clear_session_content` 够不到它。
        tools::clear_session_todos(paths, session_id)?;
        // 目标也一起清：重置就是从头来过。留着的话，armed 的旧目标会在重置后
        // 第一个回合结束时把驱动器重新拉起来，对着空历史推进一个被清掉的话题。
        if let Ok(Some(goal)) = store.goal(session_id) {
            let _ = store.clear_goal(session_id, &goal.goal_id, goal.revision);
        }
        tools::goal::forget_session(session_id);
        let memory = MemoryStore::new(config, paths);
        memory.clear_evicted_context()?;
        memory.clear_pending_events()?;
        tools::clear_aur_review_state(paths)?;
        if &*state_store.session_id() == session_id {
            if let Some(agent) = agent.as_mut() {
                agent.reset_memory()?;
                agent.prepare_for_turn()?;
                current_context(agent).map(Some)
            } else {
                cold_context(config, paths, &store).map(Some)
            }
        } else {
            Ok(None)
        }
    };
    let context = reset().map_err(|error| AdminFailure::Internal(safe_error_message(&error)))?;
    // claude-code 中转的联动:清空即丢弃该会话的续传映射并尽力删 claude 侧转录。
    crate::llm::forget_relay_sessions(session_id);
    if let Some(context) = context {
        manager.lock().unwrap().context = context;
    }
    events.publish("conversation.reset", json!({ "session_id": session_id }));
    Ok(())
}

pub(in crate::web) fn reset_actor_persona_state(
    agent: &mut Option<Agent>,
    daemon_config: &AppConfig,
    reset_config: &AppConfig,
    paths: &MiyuPaths,
    state_store: &StateStore,
    manager: &Arc<Mutex<ManagerState>>,
    events: &EventHub,
) -> std::result::Result<(), AdminFailure> {
    let mut reset = || -> Result<ContextSnapshot> {
        let persona = reset_config.active_persona_scope();
        let cleared_sessions = state_store.reset_persona_contexts(&persona, "onebot")?;
        for session_id in &cleared_sessions {
            crate::llm::forget_relay_sessions(session_id);
        }
        MemoryStore::new(reset_config, paths).reset_all(true)?;
        if persona != daemon_config.active_persona_scope() {
            return Ok(manager.lock().unwrap().context);
        }
        tools::clear_aur_review_state(paths)?;
        state_store.reset_conversation_usage()?;
        if let Some(agent) = agent.as_mut() {
            agent.reset_memory()?;
            agent.prepare_for_turn()?;
            current_context(agent)
        } else {
            cold_context(daemon_config, paths, state_store)
        }
    };
    let context = reset().map_err(|error| AdminFailure::Internal(safe_error_message(&error)))?;
    manager.lock().unwrap().context = context;
    events.publish(
        "conversation.reset",
        json!({ "scope": "persona", "persona": reset_config.active_persona_scope() }),
    );
    Ok(())
}

pub(in crate::web) fn clear_actor_session_content(
    agent: &mut Option<Agent>,
    config: &AppConfig,
    paths: &MiyuPaths,
    state_store: &StateStore,
    manager: &Arc<Mutex<ManagerState>>,
    session_id: &str,
) -> std::result::Result<(), AdminFailure> {
    let store = state_store.pinned(session_id);
    store
        .clear_session_content()
        .map_err(|error| AdminFailure::Internal(safe_error_message(error)))?;
    // 与 `reset_actor_conversation` 同理：待办在库外面，得单独清。
    crate::llm::forget_relay_sessions(session_id);
    tools::clear_session_todos(paths, session_id)
        .map_err(|error| AdminFailure::Internal(safe_error_message(&error)))?;

    // Platform sessions normally never become the daemon's current local
    // session. Keep the in-memory context coherent if a legacy binding points
    // at that session, without clearing persona-wide memory or usage totals.
    if &*state_store.session_id() == session_id {
        let context = if let Some(agent) = agent.as_mut() {
            agent
                .reset_memory()
                .and_then(|()| agent.prepare_for_turn())
                .and_then(|()| current_context(agent))
        } else {
            cold_context(config, paths, &store)
        }
        .map_err(|error| AdminFailure::Internal(safe_error_message(error)))?;
        manager.lock().unwrap().context = context;
    }
    Ok(())
}

pub(in crate::web) fn build_actor_agent(
    config: &AppConfig,
    paths: &MiyuPaths,
    state: &StateStore,
) -> Result<Agent> {
    let mut agent = build_session_agent(config, paths, state, AgentMode::Normal)?;
    agent.prepare_for_turn()?;
    Ok(agent)
}

pub(in crate::web) fn ensure_actor_agent<'a>(
    agent: &'a mut Option<Agent>,
    config: &AppConfig,
    paths: &MiyuPaths,
    state: &StateStore,
    turn_engine: &TurnEngineState,
) -> std::result::Result<&'a mut Agent, AdminFailure> {
    if agent.is_none() {
        turn_engine.set(TurnEngineState::INITIALIZING);
        match build_actor_agent(config, paths, state) {
            Ok(next) => {
                *agent = Some(next);
                turn_engine.set(TurnEngineState::READY);
            }
            Err(error) => {
                turn_engine.set(TurnEngineState::FAILED);
                return Err(AdminFailure::Internal(safe_error_message(error)));
            }
        }
    }
    Ok(agent.as_mut().expect("actor agent was initialized"))
}

pub(in crate::web) fn actor_context(
    agent: &Option<Agent>,
    config: &AppConfig,
    paths: &MiyuPaths,
    state: &StateStore,
) -> Result<ContextSnapshot> {
    agent
        .as_ref()
        .map_or_else(|| cold_context(config, paths, state), current_context)
}

#[allow(clippy::too_many_arguments)]
pub(in crate::web) fn spawn_actor(
    config: AppConfig,
    paths: MiyuPaths,
    state_store: StateStore,
    manager: Arc<Mutex<ManagerState>>,
    events: EventHub,
    questions: QuestionBroker,
    turn_engine: TurnEngineState,
    memory_organizer: Option<MemoryOrganizerHandle>,
) -> Result<(mpsc::UnboundedSender<ActorCommand>, JoinHandle<Result<()>>)> {
    let (sender, receiver) = mpsc::unbounded_channel();
    let join = std::thread::Builder::new()
        .name("miyu-daemon-core".to_string())
        // tiktoken 词元计数器首次初始化会走 fancy_regex/regex_automata 的深递归
        // 编译，debug 构建栈帧大，默认 2MB 线程栈会溢出（release 勉强够用）
        .stack_size(16 * 1024 * 1024)
        .spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .context("building daemon core runtime")?;
            // Turns are spawned as local tasks so several can run
            // concurrently on this thread (they are IO-bound); LocalSet
            // avoids imposing Send on the agent futures.
            let local = tokio::task::LocalSet::new();
            runtime.block_on(local.run_until(actor_loop(
                config,
                paths,
                state_store,
                manager,
                events,
                questions,
                turn_engine,
                memory_organizer,
                receiver,
            )));
            Ok(())
        })
        .context("starting daemon core thread")?;
    Ok((sender, join))
}
