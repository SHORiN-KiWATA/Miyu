//! 回合任务本体与四种收尾。
//!
//! `run_turn_task` 从建 agent 一直跑到产出落库。四种终局（完成、取消、失败、
//! 上下文超限）各有各的收尾动作——要不要保留排队消息、要不要通知前端、要不要
//! 写归档都不同，所以是四个 `finish_*` 而不是一个带 flag 的分支。

use crate::web::*;

pub(in crate::web) enum TurnTaskInput {
    Create {
        content: String,
        display_content: String,
        attachment_run_id: Option<String>,
        images: Vec<Option<ImageAttachment>>,
        /// 程序驱动 CLI 的「仅本回合」覆盖,见 `TurnOverrides`。
        overrides: Option<Box<crate::ipc::TurnOverrides>>,
    },
    Redo {
        candidate: crate::state::RedoCandidate,
        prompts: Vec<RedoWebPrompt>,
    },
}

pub(in crate::web) fn into_pasted_images(
    images: Vec<Option<ImageAttachment>>,
) -> Vec<Option<crate::clipboard::PastedImage>> {
    images
        .into_iter()
        .map(|image| {
            image.map(|image| match image {
                ImageAttachment::Binary { mime, data } => crate::clipboard::PastedImage::Binary(
                    crate::clipboard::ClipboardImage::new(mime, data),
                ),
                ImageAttachment::Path { path } => crate::clipboard::PastedImage::Path(path),
            })
        })
        .collect()
}

/// Executes one turn as a self-contained task. Multiple turn tasks run
/// concurrently on the actor's LocalSet — each with its own Agent, a
/// StateStore pinned to the turn's session, and an independent cancel signal.
#[allow(clippy::too_many_arguments)]
pub(in crate::web) async fn run_turn_task(
    config: AppConfig,
    paths: MiyuPaths,
    store: StateStore,
    base_store: StateStore,
    manager: Arc<Mutex<ManagerState>>,
    events: EventHub,
    questions: QuestionBroker,
    run_id: String,
    session_id: Arc<str>,
    input: TurnTaskInput,
    mode: AgentMode,
    audience: PromptAudience,
    profile: Option<platforms::TurnProfile>,
    cancel: tokio::sync::watch::Receiver<bool>,
    resource_cache: Arc<Mutex<TurnResourceCache>>,
    turn_engine: TurnEngineState,
    memory_organizer: Option<MemoryOrganizerHandle>,
) {
    // 平台回合挂生图配额(管理员/私聊白名单豁免);本地回合不挂,保持无限。
    // 包在整个 turn future 外面,turn 内所有工具执行路径都能看到同一计数器。
    let image_limit = profile
        .as_ref()
        .and_then(|profile| profile.platform.as_ref())
        .filter(|context| !context.image_generation_unlimited())
        .map(|_| crate::tools::workspace::ImageGenLimit::new(1));
    // 巨型 future 装箱落堆:外层还有五层 with_* 泛型包装再 spawn_local,
    // debug 构建下逐层栈拷贝会撞穿 actor 线程 16MB 栈(实测 SIGABRT)。
    crate::tools::workspace::with_image_gen_limit(
        image_limit,
        Box::pin(run_turn_task_inner(
            config,
            paths,
            store,
            base_store,
            manager,
            events,
            questions,
            run_id,
            session_id,
            input,
            mode,
            audience,
            profile,
            cancel,
            resource_cache,
            turn_engine,
            memory_organizer,
        )),
    )
    .await
}

async fn run_turn_task_inner(
    mut config: AppConfig,
    paths: MiyuPaths,
    store: StateStore,
    base_store: StateStore,
    manager: Arc<Mutex<ManagerState>>,
    events: EventHub,
    questions: QuestionBroker,
    run_id: String,
    session_id: Arc<str>,
    input: TurnTaskInput,
    mode: AgentMode,
    audience: PromptAudience,
    profile: Option<platforms::TurnProfile>,
    mut cancel: tokio::sync::watch::Receiver<bool>,
    resource_cache: Arc<Mutex<TurnResourceCache>>,
    turn_engine: TurnEngineState,
    memory_organizer: Option<MemoryOrganizerHandle>,
) {
    let attachment_run_id = match &input {
        TurnTaskInput::Create {
            attachment_run_id, ..
        } => attachment_run_id.clone(),
        TurnTaskInput::Redo { .. } => None,
    };
    let _attachment_guard = AttachmentRunGuard::new(base_store.clone(), attachment_run_id.clone());
    if let Some(profile) = &profile {
        if let Some(active_persona) = &profile.active_persona {
            config.prompt.active_persona.clone_from(active_persona);
        }
        if let Some(models) = &profile.text_models {
            config.active_provider_models = Some(models.clone());
        }
        // Groups drop whole turns instead of summarising: a compaction would
        // fold the structured group log into prose and every
        // `回复引用: msg=…` in the surviving turns would point at nothing.
        if let Some(group_context) = &profile.group_context {
            if !group_context.on_overflow.trim().is_empty() {
                config.context.on_overflow = group_context.on_overflow.trim().to_string();
            }
            if group_context.trim_batch_ratio > 0.0 {
                config.context.trim_batch_ratio = group_context.trim_batch_ratio;
            }
        }
        if let Some(models) = &profile.multimodal_models {
            config.active_multimodal_provider_models = Some(models.clone());
            // A conversation-specific multimodal pool is an explicit
            // override of the global vision plugin's single-model choice.
            config.plugins.vision.vision_provider_id.clear();
            config.plugins.vision.vision_model.clear();
        }
    }
    // Local sessions (REPL/WebUI/shell hook) may pin their own model pool.
    // Platform turns were already routed through the platform pools above.
    if profile
        .as_ref()
        .is_none_or(|profile| profile.text_models.is_none())
    {
        apply_session_model_override_to(&mut config, &store, &session_id);
    }
    // 程序驱动 CLI 的「仅本回合」覆盖。模型池改的是这份私有 config(与会话
    // 覆盖同路,取值有限,TurnResourceCache 扛得住);其余项走 Agent 字段,
    // 在下面的 setup 闭包里套。模型对不上不静默退回全局池——后端调用方
    // 指名要某个模型,换一个悄悄跑完比报错更糟。
    let overrides = match &input {
        TurnTaskInput::Create { overrides, .. } => overrides.as_deref().cloned(),
        TurnTaskInput::Redo { .. } => None,
    };
    let mut override_error = None;
    if let Some(models) = overrides
        .as_ref()
        .filter(|overrides| !overrides.models.is_empty())
        .map(|overrides| overrides.models.clone())
    {
        match config.usable_model_override(models.clone()) {
            Some(usable) if usable.len() == models.len() => {
                config.active_provider_models = Some(usable);
            }
            _ => {
                let labels = models
                    .iter()
                    .map(|model| format!("{}/{}", model.provider_id, model.model))
                    .collect::<Vec<_>>()
                    .join(", ");
                override_error = Some(anyhow::anyhow!(
                    "turn model override is not configured: {labels}"
                ));
            }
        }
    }
    let manager = &manager;
    let events = &events;
    let questions = &questions;
    let run_id = run_id.as_str();
    let operation = match &input {
        TurnTaskInput::Create { .. } => "create",
        TurnTaskInput::Redo { .. } => "redo",
    };
    events.publish(
        "run.started",
        json!({
            "run_id": run_id,
            "session_id": &*session_id,
            "mode": mode_name(mode),
            "operation": operation,
        }),
    );
    let title_seed: String = match &input {
        TurnTaskInput::Create { content, .. } => content.chars().take(80).collect(),
        TurnTaskInput::Redo { candidate, .. } => {
            candidate.display_content.chars().take(80).collect()
        }
    };
    // 成员会话挂在私有人格上(阶段 8):提示词/清单/记忆/技能/脚本全部跟着
    // `home/<用户>/personas/<slug>` 走。改的是本回合的配置副本;工具面按它建
    // (资源缓存键含这个目录)。
    let mut member_persona_applied = false;
    if profile.is_none() && !store.usage_account().is_empty() {
        let owner = store.usage_account().to_string();
        let scope = store
            .session_record(&session_id)
            .ok()
            .flatten()
            .map(|record| record.persona)
            .unwrap_or_default();
        if let Some(account) = base_store.account_by_id(&owner).ok().flatten() {
            // 家目录先进配置:知识库、账本按人分家,用共享 Miyu 也一样。
            config.accounts.home_dir =
                Some(paths.user_home_dir(&account.username).display().to_string());
            if let Some(persona) =
                member_persona::persona_for_scope(&paths, &account.username, &scope)
            {
                member_persona::apply_to_config(&mut config, &persona);
                member_persona_applied = true;
            }
        }
    }
    let _ = member_persona_applied;
    let warming = !turn_engine.is_ready();
    if warming {
        turn_engine.set(TurnEngineState::INITIALIZING);
    }
    let setup = (|| -> Result<(Agent, AgentTurnControl)> {
        if let Some(error) = override_error.take() {
            return Err(error);
        }
        let platform_context = profile
            .as_ref()
            .and_then(|profile| profile.platform.as_deref());
        let local_webui = is_local_webui_request(audience, profile.is_some());
        let resources = resource_cache
            .lock()
            .map_err(|_| anyhow::anyhow!("turn resource cache is poisoned"))?
            .get_or_build(&config, &paths)?;
        let mut normal_tools = resources.normal_tools.clone();
        let mut dev_tools = resources.dev_tools.clone();
        // 平台回合的工具面收口在 tools::apply_platform_turn_scope,与 MCP 桥
        // (web/session_cmds.rs)共用同一份规则——两边各写一遍正是 08-26 审查
        // 抓到的权限绕过成因。受限底座沿用缓存,不每轮重建。
        if let Some(context) = platform_context {
            platforms::apply_platform_turn_scope(
                &mut normal_tools,
                &config,
                &paths,
                context,
                Some(&resources.restricted_tools),
            );
            platforms::apply_platform_turn_scope(
                &mut dev_tools,
                &config,
                &paths,
                context,
                Some(&resources.restricted_tools),
            );
        }
        if local_webui && config.tools.enabled {
            tools::register_webui_artifact_tools(&mut normal_tools, &paths, &session_id);
            // 分享是全局清单,用根库而不是会话钉定克隆。
            tools::register_webui_share_tools(&mut normal_tools, &config, base_store.clone());
        }
        if profile
            .as_ref()
            .is_some_and(|profile| !profile.memory_write_enabled)
        {
            normal_tools.unregister("remember_fact");
            dev_tools.unregister("remember_fact");
        }
        if platform_context.is_none() && config.tools.enabled {
            tools::register_ask_question(&mut normal_tools);
            tools::register_ask_question(&mut dev_tools);
        }
        if config.tools.enabled {
            if let Some(context) = profile
                .as_ref()
                .and_then(|profile| profile.platform.clone())
            {
                platforms::register_platform_tools(&mut normal_tools, context.clone());
                platforms::register_platform_tools(&mut dev_tools, context);
            }
        }
        // 回合级工具面裁剪放在所有注册之后:两张表同裁,中途切模式白名单
        // 才不失效(AgentTurnControl 拿的也是这两张表)。
        if let Some(overrides) = overrides.as_ref() {
            if overrides.memory_writes == Some(false) {
                normal_tools.unregister("remember_fact");
                dev_tools.unregister("remember_fact");
            }
            if let Some(allow) = overrides.tool_allowlist.as_deref() {
                normal_tools.retain_named(allow);
                dev_tools.retain_named(allow);
            }
        }
        let active_tools = match mode {
            AgentMode::Normal => normal_tools.clone(),
            AgentMode::Dev => dev_tools.clone(),
        };
        // 成员的档案(阶段 6):`home/<用户名>/profile.md` 顶替管理员的属主档案。
        // 只改 Agent 手里的配置副本,资源缓存键不变;通讯平台受众本就不注入档案。
        let mut agent_config = config.clone();
        if platform_context.is_none() && !store.usage_account().is_empty() {
            if let Some(account) = base_store
                .account_by_id(store.usage_account())
                .ok()
                .flatten()
            {
                agent_config.prompt.user_identity_file = paths
                    .user_profile_file(&account.username)
                    .display()
                    .to_string();
                agent_config.prompt.active_identity.clear();
            }
        }
        let mut agent = Agent::new_for_audience(
            agent_config,
            &paths,
            store.clone(),
            // A platform turn buffers a whole round and posts it as one
            // message, so a stream that dies mid-round showed the group
            // nothing and can be retried on another endpoint — or the same
            // one — without anybody seeing a false start.
            resources
                .client
                .clone()
                .with_buffered_delivery(platform_context.is_some()),
            active_tools,
            mode,
            audience,
        )?
        .with_headless_pacing();
        let mut runtime_system_context = profile
            .as_ref()
            .map(|profile| profile.system_context.clone())
            .unwrap_or_default();
        let mut turn_system_context = profile
            .as_ref()
            .map(|profile| profile.turn_system_context.clone())
            .unwrap_or_default();
        if local_webui && mode == AgentMode::Normal {
            let manifest =
                tools::webui_artifact_manifest(&paths, &session_id).unwrap_or_else(|_| {
                    "(the artifact manifest is temporarily unavailable)".to_string()
                });
            // v7 Phase 2.1: the manifest changes whenever artifacts change, so
            // it rides the turn tail; only the static policy stays in the
            // system prompt.
            turn_system_context.push(format!(
                "<artifact-workspace>\n{manifest}\nUse read_artifact and apply_artifact_patch with bare artifact file names to work on existing artifacts; do not glob the managed directory or guess ~/.miyu paths.\n</artifact-workspace>"
            ));
            runtime_system_context.push(
                "<artifact-policy>\n\
                You are working in the Miyu WebUI and have artifact presentation tools.\n\
                - When the user explicitly asks for a report, document, web page, table, data file, standalone code file, or another downloadable deliverable, you must create or present an artifact.\n\
                - For text deliverables you write yourself, prefer create_artifact; filename must carry the correct extension.\n\
                - For files already produced by commands or other tools, call present_artifact.\n\
                - To update an existing artifact, read_artifact first, then apply_artifact_patch for targeted edits; patch paths use the bare artifact file name. Do not overwrite the whole file with create_artifact unless the user explicitly asks for a full rewrite.\n\
                - Publish only after the content is complete and self-checked. Do not publish ordinary project source edits, config changes, test fixtures, or short answers as artifacts.\n\
                - The artifact is part of the answer; after publishing succeeds, tell the user briefly in text.\n\
                </artifact-policy>"
                    .to_string(),
            );
        }
        // 宿主追加指令进 system 侧(每请求新组装、不化石,AGENTS.md §1.4);
        // 宿主每回合传同一段时前缀逐字节稳定。
        if let Some(prompt) = overrides
            .as_ref()
            .and_then(|overrides| overrides.append_system_prompt.as_deref())
            .map(str::trim)
            .filter(|prompt| !prompt.is_empty())
        {
            runtime_system_context.push(format!(
                "<host-instructions>\n{prompt}\n</host-instructions>"
            ));
        }
        if !runtime_system_context.is_empty() {
            agent.set_runtime_system_context(runtime_system_context)?;
        }
        if !turn_system_context.is_empty() {
            agent.set_turn_system_context(turn_system_context);
        }
        // 成员的 WebUI 回合(阶段 5):记忆按 principal 隔离——日记/联想只看
        // 自己的层,可写 public;情绪/好感度是全局的,不在这里动。归属从
        // 会话记录来(pinned_for_turn 已填进 store),不信任请求方声明。
        if platform_context.is_none() && !store.usage_account().is_empty() {
            let owner = store.usage_account().to_string();
            let display_name = base_store
                .account_by_id(&owner)
                .ok()
                .flatten()
                .map(|account| account.display_name)
                .unwrap_or_default();
            let principal = format!("web:{owner}");
            agent.set_memory_request_context(
                MemoryAccess::principal(principal.clone()),
                Some(principal),
                display_name,
            );
        }
        if let Some(profile) = &profile {
            agent.set_memory_writes_enabled(profile.memory_write_enabled);
            agent.set_memory_content(profile.memory_content.clone());
            agent.set_session_history_suppressed(profile.suppress_session_history);
            if let Some(namespace) = profile.image_cache_namespace.as_deref() {
                agent.set_image_platform(
                    namespace,
                    profile.image_source_label.as_deref().unwrap_or(namespace),
                );
            }
            if let Some(context) = profile.platform.as_deref() {
                // 平台回合的工具轮数兜底(max_rounds=0 时生效,见方法注释)。
                agent.cap_tool_rounds_for_platform();
                let principal = context.principal().stable_key();
                agent.set_memory_request_context(
                    if context.is_admin {
                        MemoryAccess::Privileged
                    } else {
                        MemoryAccess::principal(principal.clone())
                    },
                    Some(principal),
                    context.sender_display_name.clone(),
                );
                agent.set_memory_origin(MemoryOrigin {
                    kind: "platform".to_string(),
                    platform: context.conversation.platform.clone(),
                    account_id: context.conversation.account_id.clone(),
                    conversation_kind: context.conversation.kind.as_str().to_string(),
                    conversation_id: context.conversation.conversation_id.clone(),
                    sender_id: context.sender_id.clone(),
                    sender_display_name: context.sender_display_name.clone(),
                    session_id: session_id.to_string(),
                    message_id: context
                        .inbound_event()
                        .map(|event| event.message_id.clone())
                        .unwrap_or_default(),
                });
            }
            if let Some(context) = profile.platform.clone() {
                agent.set_platform_context_images(context.clone(), profile.context_images.clone());
                agent.set_platform_context_files(context, profile.context_files.to_vec());
            }
        }
        // 回合级覆盖在平台 profile 之后套,覆盖赢。
        let mut memory_writes_disabled = false;
        if let Some(overrides) = overrides.as_ref() {
            if let Some(prompt) = overrides.system_prompt.clone() {
                agent.set_system_prompt_override(prompt);
            }
            if let Some(window) = overrides.context_window {
                agent.set_context_window_override(window);
            }
            if overrides.memory_writes == Some(false) {
                agent.set_memory_writes_enabled(false);
                memory_writes_disabled = true;
            }
        }
        if let Some(organizer) = memory_organizer.clone() {
            if !memory_writes_disabled {
                agent.set_memory_organizer(organizer);
            }
        }
        agent.prepare_for_turn()?;
        let mut control = AgentTurnControl::new(mode, normal_tools, dev_tools);
        if let Some(signal) = manager
            .lock()
            .unwrap()
            .active_runs
            .get(run_id)
            .map(|run| run.supersede.clone())
        {
            control.set_supersede_signal(signal);
        }
        if let Some(ingress) = profile
            .as_ref()
            .and_then(|profile| profile.followup.as_ref())
            .map(|followup| followup.ingress())
        {
            control.set_queue_ingress(ingress);
        }
        Ok((agent, control))
    })();
    // 平台回合期间把上下文登记下来:MCP 桥(claude-code 供应商唯一的工具
    // 通道)回来问工具时靠它拿到平台工具,见 live_turns 模块头。**必须绑在
    // 回合任务体上**——绑在上面的 setup 闭包里会随闭包返回立刻掉落,整轮
    // 都登记不上(08-26 审查抓到,正是"群里调不到管理工具"的真身)。
    let _live_turn = profile
        .as_ref()
        .and_then(|profile| profile.platform.as_ref())
        .map(|context| platforms::LiveTurnGuard::register(&session_id, context));
    let (mut agent, control) = match setup {
        Ok(setup) => {
            turn_engine.set(TurnEngineState::READY);
            setup
        }
        Err(error) => {
            if warming {
                turn_engine.set(TurnEngineState::FAILED);
            }
            questions.cancel_run(run_id);
            finish_run(manager, run_id, None);
            // 带上原因链:只给最外层那句「stream failed …」用户查不到根因(09-11 todolist)。
            let message = safe_error_message(format!("{error:#}"));
            tracing::error!(
                run_id,
                error = %error,
                "{}",
                t("WebUI agent run setup failed", "WebUI 智能体运行初始化失败")
            );
            events.publish(
                "run.failed",
                json!({ "run_id": run_id, "session_id": &*session_id, "message": message }),
            );
            return;
        }
    };
    if let TurnTaskInput::Create {
        display_content, ..
    } = &input
    {
        agent.set_turn_persistence(display_content.clone(), attachment_run_id);
    }
    // The daemon-wide context snapshot tracks the *current* session; a turn
    // for another session must not overwrite it.
    let updates_context = || *base_store.session_id() == *session_id;
    let agent = &mut agent;
    let (redo_input_id, redo_display_content) = match &input {
        TurnTaskInput::Redo { candidate, prompts } => (
            Some(candidate.input_id.clone()),
            prompts.last().map(|prompt| prompt.display_content.clone()),
        ),
        TurnTaskInput::Create { .. } => (None, None),
    };

    let mapper = Arc::new(Mutex::new(RunEventMapper::new(
        run_id.to_string(),
        events.clone(),
        questions.clone(),
        store.clone(),
        manager.clone(),
        profile
            .as_ref()
            .and_then(|profile| profile.followup.as_ref())
            .map(|followup| followup.ingress()),
        operation,
        redo_input_id,
        redo_display_content,
        config.display.command_output_lines,
    )));
    let chat_outcome = match input {
        TurnTaskInput::Create {
            content, images, ..
        } => {
            let callback_mapper = mapper.clone();
            let images = into_pasted_images(images);
            let chat = agent.chat_stream_with_control(&content, &images, &control, move |event| {
                callback_mapper.lock().unwrap().handle(event);
                Ok(())
            });
            tokio::pin!(chat);
            loop {
                tokio::select! {
                    biased;
                    result = &mut chat => break TurnOutcome::Finished(result),
                    changed = cancel.changed() => {
                        if changed.is_err() || *cancel.borrow() {
                            questions.cancel_run(run_id);
                            break TurnOutcome::Cancelled;
                        }
                    }
                }
            }
        }
        TurnTaskInput::Redo { candidate, prompts } => {
            let callback_mapper = mapper.clone();
            let prompts = prompts
                .into_iter()
                .map(|prompt| crate::agent::RedoPromptInput {
                    prompt_id: prompt.prompt_id,
                    content: prompt.content,
                    display_content: prompt.display_content,
                    images: into_pasted_images(prompt.images),
                })
                .collect();
            let chat =
                agent.redo_stream_with_control(&candidate, prompts, &control, move |event| {
                    callback_mapper.lock().unwrap().handle(event);
                    Ok(())
                });
            tokio::pin!(chat);
            loop {
                tokio::select! {
                    biased;
                    result = &mut chat => break TurnOutcome::Finished(result),
                    changed = cancel.changed() => {
                        if changed.is_err() || *cancel.borrow() {
                            questions.cancel_run(run_id);
                            break TurnOutcome::Cancelled;
                        }
                    }
                }
            }
        }
    };

    let result = match chat_outcome {
        TurnOutcome::Cancelled => {
            drop_cancelled_queue(&store, events, run_id, &session_id);
            finish_cancelled_run(
                manager,
                events,
                agent,
                run_id,
                &session_id,
                updates_context(),
            );
            finish_turn_task(&config, &paths, &store, &title_seed, events, false);
            return;
        }
        TurnOutcome::Finished(Err(error)) if question::is_question_cancelled(&error) => {
            questions.cancel_run(run_id);
            drop_cancelled_queue(&store, events, run_id, &session_id);
            finish_cancelled_run(
                manager,
                events,
                agent,
                run_id,
                &session_id,
                updates_context(),
            );
            finish_turn_task(&config, &paths, &store, &title_seed, events, false);
            return;
        }
        TurnOutcome::Finished(Err(error)) => {
            finish_failed_run(
                manager,
                events,
                questions,
                agent,
                run_id,
                &session_id,
                updates_context(),
                &error,
            );
            finish_turn_task(&config, &paths, &store, &title_seed, events, false);
            return;
        }
        TurnOutcome::Finished(Ok(result)) => result,
    };

    questions.cancel_run(run_id);
    let context_tokens = match agent.effective_context_tokens() {
        Ok(tokens) => tokens,
        Err(error) => {
            finish_completed_with_context_error(
                manager,
                events,
                agent,
                run_id,
                &session_id,
                updates_context(),
                &result,
                &error,
            );
            finish_turn_task(&config, &paths, &store, &title_seed, events, true);
            return;
        }
    };
    let overflow_outcome = {
        let callback_mapper = mapper;
        let overflow = agent.handle_overflow_after_turn(context_tokens, move |event| {
            callback_mapper.lock().unwrap().handle(event);
            Ok(())
        });
        tokio::pin!(overflow);
        loop {
            tokio::select! {
                biased;
                result = &mut overflow => break OverflowOutcome::Finished(result),
                changed = cancel.changed() => {
                    if changed.is_err() || *cancel.borrow() {
                        break OverflowOutcome::Cancelled;
                    }
                }
            }
        }
    };
    match overflow_outcome {
        OverflowOutcome::Cancelled => {
            drop_cancelled_queue(&store, events, run_id, &session_id);
            let context =
                current_context(agent).unwrap_or_else(|_| manager.lock().unwrap().context);
            finish_run(manager, run_id, updates_context().then_some(context));
            publish_completed(events, run_id, &session_id, &result, context);
            finish_turn_task(&config, &paths, &store, &title_seed, events, true);
            return;
        }
        OverflowOutcome::Finished(Err(error)) => {
            finish_completed_with_context_error(
                manager,
                events,
                agent,
                run_id,
                &session_id,
                updates_context(),
                &result,
                &error,
            );
            finish_turn_task(&config, &paths, &store, &title_seed, events, true);
            return;
        }
        OverflowOutcome::Finished(Ok(_)) => {}
    }
    let context = match current_context(agent) {
        Ok(context) => context,
        Err(error) => {
            finish_completed_with_context_error(
                manager,
                events,
                agent,
                run_id,
                &session_id,
                updates_context(),
                &result,
                &error,
            );
            finish_turn_task(&config, &paths, &store, &title_seed, events, true);
            return;
        }
    };
    finish_run(manager, run_id, updates_context().then_some(context));
    publish_completed(events, run_id, &session_id, &result, context);
    finish_turn_task(&config, &paths, &store, &title_seed, events, true);
}

/// Shared per-turn cleanup: auto-naming, activity timestamp, queue-identity
/// cleanup, and allocator trimming. `store` is the turn's pinned store, so
/// session-scoped operations hit the turn's own session.
pub(in crate::web) fn finish_turn_task(
    config: &AppConfig,
    paths: &MiyuPaths,
    store: &StateStore,
    title_seed: &str,
    events: &EventHub,
    completed: bool,
) {
    if completed {
        if let Some(fallback) = maybe_auto_name_session(store, events, title_seed) {
            spawn_session_title_refinement(config, paths, store, events, fallback, title_seed);
        }
        let _ = store.touch_session(&store.session_id());
    }
    let _ = store.discard_queued_prompts();
    trim_process_memory();
}

pub(crate) enum TurnOutcome {
    Finished(Result<ChatResult>),
    Cancelled,
}

pub(in crate::web) enum OverflowOutcome {
    Finished(Result<Option<ChatResult>>),
    Cancelled,
}

/// An explicit cancel withdraws the follow-ups still queued behind the
/// reply: the user aborted the exchange, so folding them into context would
/// keep answering messages they no longer want processed. Published before
/// `run.cancelled` so clients still draining the event stream can clear
/// their queue bubbles.
pub(in crate::web) fn drop_cancelled_queue(
    store: &StateStore,
    events: &EventHub,
    run_id: &str,
    session_id: &str,
) {
    match store.delete_queued_prompts() {
        Ok(prompt_ids) => {
            for prompt_id in prompt_ids {
                events.publish(
                    "queue.removed",
                    json!({
                        "session_id": session_id,
                        "run_id": run_id,
                        "prompt_id": prompt_id,
                    }),
                );
            }
        }
        Err(error) => {
            tracing::warn!(
                run_id,
                error = %error,
                "{}",
                t(
                    "failed to drop queued prompts for a cancelled turn",
                    "无法丢弃已取消回复的排队消息"
                )
            );
        }
    }
}

pub(in crate::web) fn finish_cancelled_run(
    manager: &Arc<Mutex<ManagerState>>,
    events: &EventHub,
    agent: &Agent,
    run_id: &str,
    session_id: &str,
    updates_context: bool,
) {
    let context = current_context(agent).ok().filter(|_| updates_context);
    let mut payload = json!({ "run_id": run_id, "session_id": session_id });
    if let Some(context) = &context {
        // The interrupted turn is persisted into the context; keep the client
        // context meters honest instead of leaving them at the pre-turn value.
        payload["context_tokens"] = json!(context.tokens);
        payload["context_window"] = json!(context.window);
        payload["cumulative_tokens"] = json!(context.cumulative_tokens);
        payload["cumulative_prompt_tokens"] = json!(context.cumulative_prompt_tokens);
        payload["cumulative_cache_read_tokens"] = json!(context.cumulative_cache_read_tokens);
    }
    finish_run(manager, run_id, context);
    events.publish("run.cancelled", payload);
}

#[allow(clippy::too_many_arguments)]
pub(in crate::web) fn finish_failed_run(
    manager: &Arc<Mutex<ManagerState>>,
    events: &EventHub,
    questions: &QuestionBroker,
    agent: &Agent,
    run_id: &str,
    session_id: &str,
    updates_context: bool,
    error: &anyhow::Error,
) {
    questions.cancel_run(run_id);
    let context = current_context(agent).ok().filter(|_| updates_context);
    finish_run(manager, run_id, context);
    let message = safe_error_message(format!("{error:#}"));
    tracing::error!(
        run_id,
        error = %error,
        "{}",
        t("WebUI agent run failed", "WebUI 智能体运行失败")
    );
    events.publish(
        "run.failed",
        json!({ "run_id": run_id, "session_id": session_id, "message": message }),
    );
}

#[allow(clippy::too_many_arguments)]
pub(in crate::web) fn finish_completed_with_context_error(
    manager: &Arc<Mutex<ManagerState>>,
    events: &EventHub,
    agent: &Agent,
    run_id: &str,
    session_id: &str,
    updates_context: bool,
    result: &ChatResult,
    error: &anyhow::Error,
) {
    let message = safe_error_message(error);
    tracing::error!(
        run_id,
        error = %error,
        "{}",
        t(
            "WebUI post-turn context maintenance failed",
            "WebUI 回合后上下文维护失败"
        )
    );
    events.publish(
        "context.error",
        json!({ "run_id": run_id, "session_id": session_id, "message": message }),
    );
    let context = current_context(agent).unwrap_or_else(|_| manager.lock().unwrap().context);
    finish_run(manager, run_id, updates_context.then_some(context));
    publish_completed(events, run_id, session_id, result, context);
}

pub(in crate::web) fn publish_completed(
    events: &EventHub,
    run_id: &str,
    session_id: &str,
    result: &ChatResult,
    context: ContextSnapshot,
) {
    // Always the local estimate of the persisted context: provider-reported
    // request usage measures what this turn consumed, not what the context
    // holds now — the two diverge after post-turn compaction/pruning, and
    // the footer meter must refresh with those rewrites.
    let context_tokens = context.tokens;
    events.publish(
        "run.completed",
        json!({
            "run_id": run_id,
            "session_id": session_id,
            // 最终正文随终态一起发:程序驱动的客户端不用再靠 delta 累加。
            "content": result.content,
            "usage": result.usage,
            "usage_estimated": result.usage_estimated,
            "provider_id": result.provider_id,
            "model": result.model,
            "context_tokens": context_tokens,
            "context_window": context.window,
            "cumulative_tokens": context.cumulative_tokens,
            "cumulative_prompt_tokens": context.cumulative_prompt_tokens,
            "cumulative_cache_read_tokens": context.cumulative_cache_read_tokens,
        }),
    );
}

pub(in crate::web) fn current_context(agent: &Agent) -> Result<ContextSnapshot> {
    let cumulative = agent.conversation_usage_token_totals()?;
    Ok(ContextSnapshot {
        tokens: agent.effective_context_tokens()?,
        window: agent.context_window(),
        window_assumed: agent.context_window_assumed(),
        cumulative_tokens: cumulative.total,
        cumulative_prompt_tokens: cumulative.prompt,
        cumulative_cache_read_tokens: cumulative.cache_read,
    })
}
