//! HTTP 服务本体：路由、启动、健康检查与关停。
//!
//! `bootstrap` 是前端首屏要的一整包数据（会话、模型、能力、配置）——分成十几个
//! 请求会让打开界面变成一串瀑布。
//!
//! `shutdown_signal` 要同时接 Ctrl-C 与 SIGTERM：systemd 停服务发的是后者。

use crate::web::*;

pub async fn run(paths: MiyuPaths, args: WebArgs) -> Result<()> {
    AppConfig::init_files(&paths)?;
    let config = AppConfig::load_or_default(&paths)?;
    tools::jobs::init(&paths);
    let state_store = StateStore::new(&paths)?;
    state_store.init_files()?;
    let persona = config.active_persona_scope();
    state_store.adopt_sessions_for_persona(&persona)?;
    ensure_local_current_session(&state_store, &persona)?;
    // Subagent audit sessions are kept for a week, cleaned at startup and
    // then daily while the daemon runs. One-shot `ask` sessions delete
    // themselves as their turn ends, so the hour-old survivors swept here are
    // strictly orphans from a client that died mid-turn.
    const SUBAGENT_AUDIT_RETENTION_DAYS: i64 = 7;
    const ASK_SESSION_RETENTION_HOURS: i64 = 1;
    let _ = state_store.delete_subagent_sessions_older_than(SUBAGENT_AUDIT_RETENTION_DAYS);
    let _ = state_store.delete_ask_sessions_older_than(ASK_SESSION_RETENTION_HOURS);
    {
        let store = state_store.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(24 * 60 * 60));
            interval.tick().await;
            loop {
                interval.tick().await;
                let _ = store.delete_subagent_sessions_older_than(SUBAGENT_AUDIT_RETENTION_DAYS);
                let _ = store.delete_ask_sessions_older_than(ASK_SESSION_RETENTION_HOURS);
            }
        });
    }
    // 09-04 issue #36:上下文现算含 MCP tools/list,一个不可达的 MCP server
    // 曾让这一行卡到 CLI 的 8 秒就绪窗口耗尽、整个 daemon 被杀。限时等待,
    // 超时先用占位快照放行,真数算好后在下面回填。
    let (context, pending_context) = startup_context(&config, &paths, &state_store)?;

    // Default binds all interfaces so the WebUI is reachable from the LAN;
    // `--bind 127.0.0.1` restricts it to this machine. Access URLs matching
    // the effective bind are printed below.
    let bind_ip = args.bind.unwrap_or(IpAddr::V4(Ipv4Addr::UNSPECIFIED));
    let listener = match tokio::net::TcpListener::bind(SocketAddr::new(bind_ip, args.port)).await {
        Ok(listener) => listener,
        Err(error)
            if args.port == ipc::DEFAULT_WEB_PORT
                && error.kind() == std::io::ErrorKind::AddrInUse =>
        {
            tracing::warn!(
                requested_port = args.port,
                "{}",
                t(
                    "Miyu WebUI default port is occupied; selecting an ephemeral port",
                    "Miyu WebUI 默认端口已被占用；将选择临时端口"
                )
            );
            tokio::net::TcpListener::bind(SocketAddr::new(bind_ip, 0))
                .await
                .context("binding Miyu WebUI to an ephemeral fallback port")?
        }
        Err(error) => {
            return Err(error)
                .with_context(|| format!("binding Miyu WebUI to {bind_ip}:{}", args.port));
        }
    };
    let port = listener.local_addr()?.port();
    let boot_id: Arc<str> = random_id("boot", 18).into();
    let events = EventHub::new();
    let questions = QuestionBroker::new();
    let manager = Arc::new(Mutex::new(ManagerState {
        config: config.clone(),
        active_runs: HashMap::new(),
        admin_busy: false,
        admin_session: None,
        context,
        persona_session_ids: HashMap::from([(
            config.active_persona_scope(),
            state_store.session_id().to_string(),
        )]),
        runs_changed: Arc::new(tokio::sync::Notify::new()),
    }));
    if let Some(pending_context) = pending_context {
        let manager = manager.clone();
        let state_store = state_store.clone();
        let session_at_start = state_store.session_id();
        tokio::task::spawn_blocking(move || {
            let Ok(Ok(context)) = pending_context.recv() else {
                return;
            };
            let mut manager = manager.lock().unwrap();
            // 只在没人动过快照时回填:期间跑完的回合或切走的会话已经写入了
            // 各自的真数,拿启动会话的旧数盖上去反而错。
            let untouched = manager.context.tokens == 0
                && manager.active_runs.is_empty()
                && state_store.session_id() == session_at_start;
            if untouched {
                manager.context = context;
                tracing::info!(
                    tokens = context.tokens,
                    "startup context backfilled after slow calculation"
                );
            }
        });
    }
    let turn_engine = TurnEngineState::default();
    let memory_organizer = MemoryOrganizer::spawn()?;
    let memory_organizer_handle = memory_organizer.handle();
    memory_organizer_handle.wake(config.clone(), paths.clone(), state_store.clone());
    let stores = StoreRegistry::new(state_store.clone(), paths.clone());
    let (actor_tx, actor_join) = spawn_actor(
        config,
        paths.clone(),
        state_store.clone(),
        stores.clone(),
        manager.clone(),
        events.clone(),
        questions.clone(),
        turn_engine.clone(),
        Some(memory_organizer_handle),
    )?;
    let (shutdown_tx, mut shutdown_rx) = broadcast::channel(1);
    // 多用户(09-11 起 WebUI 永远要登录):没建管理员账号之前,内置账号 miyu/miyu
    // 登录即管理员,登录后先建号;建完号内置账号失效,只剩账号登录与邀请码注册。
    let state = DaemonState {
        auth: WebAuth::new(Some(BUILTIN_SETUP_PASSWORD)),
        boot_id,
        web_port: port,
        web_public: !bind_ip.is_loopback(),
        web_bind: bind_ip,
        paths,
        manager,
        stores,
        state_store,
        events,
        questions,
        actor_tx: actor_tx.clone(),
        shutdown_tx,
        turn_engine,
        platforms: PlatformRuntime::new()?,
    };
    let initial_qq = state.manager.lock().unwrap().config.platforms.qq.clone();
    state
        .platforms
        .qq_listener
        .prepare(&state, None, &initial_qq)
        .await?
        .commit();
    let (ipc_lease, ipc_task) = start_ipc_server(&state)?;
    install_background_job_hook(&state);
    // 语音前端(独立 miyu-voice 进程):只在 voice.enabled 时拉起。
    voice_bridge::install_state(&state);
    voice_bridge::spawn_if_enabled(&state);
    // 目标续轮驱动器。启动时故意**不**恢复任何自动续跑：目标还在库里，但
    // 「是否自动跑」驻内存、重启即失，必须由人 `/goal resume` 重新授权。
    // 不然一次崩溃重启就能让机器在无人看管的情况下继续自己开轮。
    spawn_goal_round_driver(state.clone());
    // QQ 定时消息:常驻 tick 循环,每个 tick 现读配置,启停/改表无需重启。
    crate::platforms::plugins::scheduled_messages::spawn_scheduled_message_worker(state.clone());
    let app = router(state.clone());
    let urls = ipc::web_access_urls_for(bind_ip, port);
    // share_file 工具用这些地址把相对下载路径拼成局域网完整链接。
    tools::set_share_url_bases(urls.clone());
    for url in &urls {
        println!("Miyu WebUI: {url}");
    }
    if !state.state_store.has_admin_account().unwrap_or(true) {
        eprintln!(
            "{}",
            t(
                "First visit: sign in as the built-in account (username `miyu`, password `miyu`) and create the admin account; the built-in account stops working afterwards.",
                "首次访问：用内置账号登录（用户名 miyu，密码 miyu）并创建管理员账号，建完号内置账号即失效。"
            )
        );
    }
    match crate::tools::sandbox::probe() {
        Some(abi) => tracing::info!(abi, "member sandbox: landlock available"),
        None => tracing::warn!(
            "member sandbox: landlock unavailable on this kernel; member commands will be refused"
        ),
    }
    std::io::stdout().flush().ok();

    let serve_result = {
        let server = axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .into_future();
        tokio::pin!(server);
        tokio::select! {
            result = &mut server => result,
            _ = shutdown_signal() => Ok(()),
            _ = shutdown_rx.recv() => Ok(()),
        }
    };
    let _ = actor_tx.send(ActorCommand::Shutdown);
    tools::jobs::shutdown_all();
    voice_bridge::shutdown();
    state.platforms.qq_listener.shutdown(&state).await;
    ipc_task.abort();
    let _ = ipc_task.await;
    let actor_result = tokio::task::spawn_blocking(move || actor_join.join())
        .await
        .context("joining WebUI actor task")?
        .map_err(|_| anyhow::anyhow!("WebUI actor thread panicked"))?;
    memory_organizer.shutdown();
    drop(ipc_lease);
    serve_result.context("serving Miyu WebUI")?;
    actor_result
}

/// Attach a client to an already-running turn (background-command wake):
/// forwards its event frames until terminal, without owning the run.
pub(in crate::web) async fn follow_run(
    state: &DaemonState,
    stream: &mut tokio::net::UnixStream,
    run_id: String,
) -> Result<()> {
    let mut subscription = state.events.subscribe_after(state.events.latest_id());
    let run_state = {
        let manager = state.manager.lock().unwrap();
        manager
            .active_runs
            .get(&run_id)
            .map(|info| info.turn_id.clone())
    };
    let Some(turn_id) = run_state else {
        ipc::send(stream, &IpcFrame::error("run is not active")).await?;
        return Ok(());
    };
    ipc::send(
        stream,
        &IpcFrame::Accepted {
            run_id: run_id.clone(),
            turn_id,
        },
    )
    .await?;
    let mut last_id = 0u64;
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
            ipc::send(
                stream,
                &IpcFrame::error("Miyu core event history was exhausted"),
            )
            .await?;
            break;
        }
        last_id = record.id;
        let Ok(data) = serde_json::from_str::<Value>(&record.data) else {
            continue;
        };
        if data.get("run_id").and_then(Value::as_str) != Some(run_id.as_str()) {
            // The run may have finished before we saw a frame; stop when it
            // is no longer active and nothing more will arrive for it.
            if !state
                .manager
                .lock()
                .unwrap()
                .active_runs
                .contains_key(&run_id)
            {
                break;
            }
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
            break;
        }
    }
    Ok(())
}

pub(in crate::web) fn router(state: DaemonState) -> Router {
    Router::new()
        .route("/", get(index_asset))
        .route("/styles.css", get(styles_asset))
        .route("/theme.css", get(theme_css))
        .route("/app.js", get(app_asset))
        .route("/commands.js", get(commands_js_asset))
        .route("/lightbox.js", get(lightbox_js_asset))
        .route("/preview.js", get(preview_js_asset))
        .route("/linkcards.js", get(linkcards_js_asset))
        .route("/todos.js", get(todos_js_asset))
        .route("/highlight.js", get(highlight_js_asset))
        // artifact 的沙箱 iframe 也来这里取库,而它是不透明源——浏览器会为此强制
        // 发 OPTIONS 预检,所以每条都得配一个 options 分支,漏一条那个库就加载不上。
        .route(
            "/vendor/prism/prism.min.js",
            get(prism_js_asset).options(vendor_preflight),
        )
        .route(
            "/vendor/katex/katex.min.js",
            get(katex_js_asset).options(vendor_preflight),
        )
        .route(
            "/vendor/katex/katex.min.css",
            get(katex_css_asset).options(vendor_preflight),
        )
        .route(
            "/vendor/katex/fonts/{font}",
            get(katex_font_asset).options(vendor_preflight),
        )
        .route(
            "/vendor/echarts/echarts.min.js",
            get(echarts_js_asset).options(vendor_preflight),
        )
        .route("/api/media", get(media_stream))
        // WebUI 链接卡片:元数据与缩略图都由 daemon 代抓,浏览器不直连第三方
        // (CSP img-src/connect-src 都是 'self',放宽等于给远程像素追踪开门)。
        .route("/api/link-preview", get(link_preview::link_preview))
        .route(
            "/api/link-preview/image/{asset_id}",
            get(link_preview::link_preview_image),
        )
        .route("/assets/miyu-logo.png", get(logo_asset))
        .route("/assets/miyuwallpaper.png", get(wallpaper_asset))
        .route("/api/health", get(health))
        .route("/api/auth/login", post(auth_login))
        .route("/api/auth/logout", post(auth_logout))
        .route("/api/auth/register", post(auth_register))
        .route("/api/auth/status", get(auth_status))
        .route("/api/auth/setup-admin", post(auth_setup_admin))
        .route("/api/account", get(account_me).patch(account_update))
        .route(
            "/api/account/personas",
            get(account_personas).post(account_persona_create),
        )
        .route(
            "/api/account/personas/{slug}",
            put(account_persona_update).delete(account_persona_delete),
        )
        .route(
            "/api/account/personas/{slug}/prompt",
            get(account_persona_prompt),
        )
        .route(
            "/api/account/personas/{slug}/image",
            put(account_persona_image).delete(account_persona_image_delete),
        )
        .route("/api/account/active-persona", put(account_active_persona))
        .route("/api/admin/accounts", get(admin_list_accounts))
        .route(
            "/api/admin/accounts/{account_id}",
            patch(admin_update_account),
        )
        .route(
            "/api/admin/invites",
            get(admin_list_invites).post(admin_create_invite),
        )
        .route(
            "/api/admin/invites/{invite_id}",
            delete(admin_delete_invite),
        )
        .route("/api/admin/usage/accounts", get(admin_usage_accounts))
        .route("/api/bootstrap", get(bootstrap))
        .route("/api/persona/avatar", get(persona_avatar))
        .route(
            "/api/persona/assets",
            post(upload_persona_asset).layer(DefaultBodyLimit::max(PERSONA_ASSET_LIMIT)),
        )
        .route("/api/config", get(get_config).put(update_config))
        .route("/api/ui-prefs", get(get_ui_prefs).put(update_ui_prefs))
        .route("/api/providers/models", post(provider_models))
        .route("/api/voice/status", get(voice_status))
        .route("/api/voice/devices", get(voice_devices))
        .route("/api/voice/stream", get(voice_stream))
        .route("/api/voice/tts/voices", get(voice_tts_voices))
        .route("/api/voice/tts/preview", post(voice_tts_preview))
        .route(
            "/api/voice/transcribe",
            post(voice_transcribe).layer(DefaultBodyLimit::max(VOICE_UPLOAD_LIMIT)),
        )
        .route(
            "/api/qq-group-management/history",
            get(qq_group_history_http),
        )
        .route(
            "/api/qq-group-management/history/clear",
            post(qq_group_history_clear_http),
        )
        .route(
            "/api/qq-group-management/offenders/{user_id}",
            delete(qq_group_offender_delete_http),
        )
        .route("/api/events", get(events))
        .route("/api/assets/{asset_id}", get(image_asset))
        .route("/api/artifacts/{asset_id}", get(artifact_asset))
        .route(
            "/api/shared",
            get(shared_files_list)
                .post(shared_file_upload)
                .layer(DefaultBodyLimit::disable()),
        )
        .route(
            "/api/shared/{share_id}",
            get(shared_file_download).delete(shared_file_delete),
        )
        .route("/shared.js", get(shared_js_asset))
        .route("/dash/{script}", get(dash_script_asset))
        .route("/api/dash/memory/personas", get(dash_memory_personas))
        .route("/api/dash/memory/stats", get(dash_memory_stats))
        .route("/api/dash/memory/items", get(dash_memory_items))
        .route(
            "/api/dash/memory/items/{table}/{id}",
            get(dash_memory_item)
                .patch(dash_memory_patch)
                .delete(dash_memory_delete),
        )
        .route("/api/dash/memory/facts", post(dash_memory_add_fact))
        .route("/api/dash/memory/evicted", get(dash_memory_evicted))
        .route(
            "/api/dash/memory/evicted/clear",
            post(dash_memory_evicted_clear),
        )
        .route(
            "/api/dash/memory/evicted/{id}",
            get(dash_memory_evicted_item).delete(dash_memory_evicted_delete),
        )
        .route(
            "/api/dash/memory/pending/clear",
            post(dash_memory_pending_clear),
        )
        .route("/api/dash/memory/reset", post(dash_memory_reset))
        .route("/api/dash/kb/overview", get(dash_kb_overview))
        .route("/api/dash/kb/file", get(dash_kb_file))
        .route("/api/dash/kb/search", get(dash_kb_search))
        .route(
            "/api/dash/kb/files",
            post(dash_kb_upload)
                .layer(DefaultBodyLimit::max(KB_UPLOAD_LIMIT))
                .delete(dash_kb_delete),
        )
        .route(
            "/api/dash/kb/reindex",
            get(dash_kb_reindex_status).post(dash_kb_reindex_start),
        )
        .route(
            "/api/dash/kb/reindex/lock",
            axum::routing::delete(dash_kb_reindex_unlock),
        )
        .route("/api/dash/kb/default", get(dash_kb_default))
        .route("/api/dash/kb/default/update", post(dash_kb_default_update))
        .route("/api/dash/scripts/personas", get(dash_scripts_personas))
        .route("/api/dash/scripts/overview", get(dash_scripts_overview))
        .route("/api/dash/scripts/source", get(dash_scripts_source))
        .route("/api/dash/scripts/enable", post(dash_scripts_enable))
        .route("/api/dash/scripts/disable", post(dash_scripts_disable))
        .route(
            "/api/dash/scripts/item",
            axum::routing::delete(dash_scripts_delete),
        )
        .route("/api/dash/scripts/register", post(dash_scripts_register))
        .route("/api/dash/ledger/overview", get(dash_ledger_overview))
        .route(
            "/api/dash/ledger/entries",
            get(dash_ledger_entries).post(dash_ledger_create_entry),
        )
        .route(
            "/api/dash/ledger/entries/{id}",
            axum::routing::patch(dash_ledger_update_entry).delete(dash_ledger_delete_entry),
        )
        .route(
            "/api/dash/ledger/entries/{id}/restore",
            post(dash_ledger_restore_entry),
        )
        .route("/api/dash/ledger/books", post(dash_ledger_create_book))
        .route(
            "/api/dash/ledger/accounts",
            post(dash_ledger_create_account),
        )
        .route(
            "/api/dash/ledger/categories",
            post(dash_ledger_create_category),
        )
        .route("/api/dash/ledger/budgets", post(dash_ledger_set_budget))
        .route(
            "/api/dash/ledger/budgets/{id}",
            axum::routing::delete(dash_ledger_delete_budget),
        )
        .route(
            "/api/dash/ledger/backfill-rates",
            post(dash_ledger_backfill_rates),
        )
        .route("/api/dash/ledger/export", get(dash_ledger_export))
        .route("/api/dash/ledger/import", post(dash_ledger_import))
        .route("/api/dash/memes/libraries", get(dash_memes_libraries))
        .route(
            "/api/dash/memes/items",
            get(dash_memes_items)
                .post(dash_memes_upload)
                .layer(DefaultBodyLimit::max(MEME_UPLOAD_LIMIT)),
        )
        .route(
            "/api/dash/memes/items/{id}",
            axum::routing::patch(dash_memes_patch).delete(dash_memes_delete),
        )
        .route(
            "/api/dash/memes/items/{id}/classify",
            post(dash_memes_classify),
        )
        .route("/api/dash/memes/image", get(dash_memes_image))
        .route("/api/dash/qq/accounts", get(dash_qq_accounts))
        .route("/api/dash/qq/conversations", get(dash_qq_conversations))
        .route("/api/dash/qq/messages", get(dash_qq_messages))
        .route("/api/dash/qq/messages/delete", post(dash_qq_delete))
        .route("/api/dash/qq/stats", get(dash_qq_stats))
        .route("/api/dash/qq/recalls", get(dash_qq_recalls))
        .route(
            "/api/dash/qq/boundary",
            get(dash_qq_boundary).post(dash_qq_reset_context),
        )
        .route("/api/dash/qq/groups", get(dash_qq_groups))
        .route("/api/dash/qq/management", get(dash_qq_management))
        .route(
            "/api/dash/qq/management/events/clear",
            post(dash_qq_management_clear_events),
        )
        .route("/api/dash/affection/scopes", get(dash_affection_scopes))
        .route("/api/dash/affection/items", get(dash_affection_items))
        .route(
            "/api/dash/affection/items/{user}",
            get(dash_affection_item)
                .patch(dash_affection_patch)
                .delete(dash_affection_delete),
        )
        .route(
            "/api/dash/affection/emotion",
            get(dash_emotion_state).put(dash_emotion_set),
        )
        .route(
            "/api/dash/affection/emotion/reset",
            post(dash_emotion_reset),
        )
        .route("/api/dash/sponsors/overview", get(dash_sponsors_overview))
        .route(
            "/api/dash/sponsors/records",
            get(dash_sponsors_records).post(dash_sponsors_create),
        )
        .route(
            "/api/dash/sponsors/records/{record_id}",
            patch(dash_sponsors_patch).delete(dash_sponsors_delete),
        )
        .route(
            "/api/attachments",
            post(upload_user_attachment).layer(DefaultBodyLimit::disable()),
        )
        .route(
            "/api/attachments/{attachment_id}",
            get(user_attachment).delete(delete_user_attachment),
        )
        .route(
            "/api/platform-assets/{token}",
            get(platforms::platform_asset),
        )
        .route(
            "/api/sessions",
            get(list_sessions_http).post(create_session_http),
        )
        .route("/api/sessions/order", put(reorder_sessions_http))
        .route(
            "/api/sessions/{session_id}",
            patch(update_session_http).delete(delete_session_http),
        )
        .route("/api/sessions/{session_id}/turns", get(session_turns_http))
        .route("/api/sessions/{session_id}/todos", get(session_todos_http))
        .route("/api/sessions/{session_id}/goal", get(session_goal_http))
        .route(
            "/api/sessions/{session_id}/context",
            get(session_context_http),
        )
        .route(
            "/api/sessions/{session_id}/poppable",
            get(poppable_turns_http),
        )
        .route(
            "/api/sessions/{session_id}/models",
            get(get_session_models_http).put(set_session_models_http),
        )
        .route(
            "/api/sessions/{session_id}/turns/{turn_id}/redo",
            post(redo_turn),
        )
        .route("/api/turns", post(create_turn))
        .route("/api/queue", post(queue_prompt))
        .route(
            "/api/runs/{run_id}/turns/{turn_id}/queue/{prompt_id}",
            delete(remove_queue_prompt),
        )
        .route("/api/runs/{run_id}/cancel", post(cancel_run))
        .route("/api/questions/{question_id}", delete(close_question))
        .route("/api/questions/{question_id}/answer", post(answer_question))
        .route("/api/models/active", put(set_models))
        .route(
            "/api/models/thinking-variants",
            get(get_thinking_variants).put(set_thinking_variants),
        )
        .route("/api/conversation/reset", post(reset_conversation))
        .route("/api/commands", get(list_commands))
        .route("/api/conversation/compact", post(compact_conversation))
        .route("/api/conversation/pop", post(pop_conversation))
        .route("/api/memory/reset", post(reset_memory_http))
        .route("/api/memory/reset-all", post(reset_all_memory_http))
        .route("/api/goal", post(goal_command_http))
        .route("/api/jobs", get(list_jobs_http))
        .route("/api/usage/stats", get(usage_stats_web))
        .route("/api/usage/details", get(usage_details_web))
        .route("/api/usage/clear", post(usage_clear_web))
        .route("/api/jobs/{job_id}", delete(stop_job_http))
        // OneBot v11 reverse-WS endpoint: NapCat connects here as a WS
        // client. Gated by platforms.qq config, not web auth.
        .route("/ws", get(platforms::onebot::onebot_ws_on_web_port))
        // Backward-compatible endpoint used by earlier Miyu releases.
        .route(
            "/onebot/v11/ws",
            get(platforms::onebot::onebot_ws_on_web_port),
        )
        .layer(DefaultBodyLimit::max(JSON_BODY_LIMIT))
        .with_state(state)
}

/// Strong validator shared by all build-embedded assets: the BUILD_ID
/// changes on any frontend edit (build.rs rerun triggers), so a 304 can
/// never pin a stale file.
pub(in crate::web) fn build_etag() -> &'static HeaderValue {
    static ETAG_VALUE: std::sync::LazyLock<HeaderValue> = std::sync::LazyLock::new(|| {
        HeaderValue::from_str(concat!("\"", env!("MIYU_BUILD_ID"), "\""))
            .expect("build id forms a valid header value")
    });
    &ETAG_VALUE
}

/// Optional MD3 token override generated by matugen from the wallpaper.
/// Read from disk on every request (the file is tiny and regenerated at any
/// time); 404 when absent so the WebUI falls back to the built-in palette.
pub(in crate::web) async fn theme_css(State(state): State<DaemonState>) -> Response {
    let path = state.paths.config_dir.join("webui-theme.css");
    match tokio::fs::read(&path).await {
        Ok(bytes) => finish_asset_response(bytes.into_response(), "text/css; charset=utf-8"),
        Err(_) => StatusCode::NOT_FOUND.into_response(),
    }
}

pub(in crate::web) async fn health() -> Json<Value> {
    Json(json!({
        "status": "ready",
        "version": env!("CARGO_PKG_VERSION"),
    }))
}

pub(in crate::web) async fn bootstrap(
    State(state): State<DaemonState>,
    headers: HeaderMap,
) -> std::result::Result<Response, ApiError> {
    let identity = require_identity(&headers, &state)?;
    let metadata_config = state.manager.lock().unwrap().config.clone();
    crate::models_cache::ensure_active_metadata(&state.paths, &metadata_config);
    let own_store = state
        .stores
        .for_identity(&identity)
        .map_err(ApiError::internal)?;
    own_store
        .recover_stale_turns()
        .map_err(ApiError::internal)?;
    // 归属(阶段 5):成员的「当前会话」是自己名下最近的一条(没有就建);
    // 管理员仍是 daemon 的全局指针。下面的回合/队列/重做候选都按它取。
    let current_session: Arc<str> = if identity.admin {
        state.state_store.session_id()
    } else {
        member_current_session(&state, identity.owner_key())
            .map_err(session_api_error)?
            .into()
    };
    let store = own_store.pinned(&current_session);
    let owned_sessions = sessions_with_dev(
        &own_store,
        &metadata_config.active_persona_scope(),
        identity.owner_key(),
    )
    .map_err(ApiError::internal)?;
    let owned_ids: HashSet<&str> = owned_sessions
        .iter()
        .map(|overview| overview.record.session_id.as_str())
        .collect();
    let (config, active_run_id, runs, context) = {
        let manager = state.manager.lock().unwrap();
        let runs: Vec<Value> = manager
            .active_runs
            .iter()
            .filter(|(_, info)| owned_ids.contains(&*info.session_id))
            .map(|(run_id, info)| {
                json!({
                    "run_id": run_id,
                    "session_id": &*info.session_id,
                    "mode": mode_name(info.mode),
                    "operation": info.operation.name(),
                    "turn_id": info.operation.turn_id(),
                    "input_id": info.operation.input_id(),
                })
            })
            .collect();
        (
            manager.config.clone(),
            manager.run_in_session(&current_session).cloned(),
            runs,
            manager.context,
        )
    };
    let running_target = store
        .running_turn_queue_target()
        .map_err(ApiError::internal)?;
    let external_target = active_run_id
        .is_none()
        .then_some(running_target.as_ref())
        .flatten();
    let mut assets_by_turn = HashMap::<String, Vec<ImageAsset>>::new();
    for asset in store.load_image_assets().map_err(ApiError::internal)? {
        assets_by_turn
            .entry(asset.turn_id.clone())
            .or_default()
            .push(asset);
    }
    let mut artifacts_by_turn = HashMap::<String, Vec<ArtifactAsset>>::new();
    for artifact in store.load_artifact_assets().map_err(ApiError::internal)? {
        artifacts_by_turn
            .entry(artifact.turn_id.clone())
            .or_default()
            .push(artifact);
    }
    let turns = store
        .load_turns()
        .map_err(ApiError::internal)?
        .into_iter()
        .filter(|turn| !turn.is_summary)
        .map(|turn| {
            let assets = assets_by_turn.remove(&turn.turn_id).unwrap_or_default();
            let artifacts = artifacts_by_turn.remove(&turn.turn_id).unwrap_or_default();
            SafeTurn::from_turn(turn, assets, artifacts)
        })
        .collect();
    let usage = state
        .state_store
        .usage_snapshot()
        .map_err(ApiError::internal)?
        .into();
    let queued_prompts = match external_target {
        Some(target) => store
            .load_queued_prompts_for_target(target)
            .map_err(ApiError::internal)?,
        None => store.load_queued_prompts().map_err(ApiError::internal)?,
    }
    .into_iter()
    .map(SafeQueuedPrompt::from)
    .collect();
    let running_turn_id = running_target.as_ref().map(|target| target.turn_id.clone());
    let external_queue_available = external_target
        .is_some_and(|target| target.queue_session_id.is_some() && target.owner_pid.is_some());
    let current_session_id = current_session.to_string();
    let sessions = owned_sessions
        .iter()
        .map(|overview| session_overview_json(overview, &current_session_id))
        .collect();
    let persona = member_persona_identity(&state.paths, &identity).unwrap_or_else(|| {
        persona_identity(
            &config,
            &read_prompt_documents(&config, &state.paths)
                .unwrap_or_else(|_| PromptDocuments::default()),
        )
    });
    let redo_candidate = if active_run_id.is_none() {
        store
            .redo_candidate()
            .map_err(ApiError::internal)?
            .map(SafeRedoCandidate::from)
    } else {
        None
    };
    let mut response = Json(BootstrapResponse {
        version: env!("CARGO_PKG_VERSION"),
        boot_id: state.boot_id.to_string(),
        latest_event_id: state.events.latest_id(),
        active_run_id,
        running_turn_id,
        external_queue_available,
        turns,
        queued_prompts,
        models: safe_models(&config),
        display: web_display_config(&config),
        context,
        usage,
        capabilities: Capabilities {
            multi_conversation: true,
            attachments: true,
            queue: true,
            redo: true,
            admin: identity.admin,
            multi_user: state.auth.required(),
        },
        sessions,
        current_session_id,
        runs,
        persona,
        redo_candidate,
        account: account_bootstrap_json(&state, &identity),
    })
    .into_response();
    response
        .headers_mut()
        .insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    Ok(response)
}

pub(in crate::web) async fn events(
    State(state): State<DaemonState>,
    headers: HeaderMap,
    Query(query): Query<EventsQuery>,
) -> std::result::Result<Sse<impl Stream<Item = std::result::Result<Event, Infallible>>>, ApiError>
{
    let identity = require_identity(&headers, &state)?;
    let owner_filter = EventOwnerFilter::new(state.clone(), &identity);
    let header_after = headers
        .get("last-event-id")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(0);
    let after = query.after.max(header_after);
    let subscription = state.events.subscribe_after(after);
    let stream_state = SseStreamState {
        pending: subscription.pending,
        receiver: subscription.receiver,
        events: state.events,
        last_id: after,
        owner_filter,
    };
    let events = stream::unfold(stream_state, |mut state| async move {
        loop {
            if let Some(record) = state.pending.pop_front() {
                if record.kind == "resync_required" {
                    state.last_id = record.id;
                    return Some((Ok(record_to_sse(record)), state));
                }
                if record.id <= state.last_id {
                    continue;
                }
                state.last_id = record.id;
                if !state.owner_filter.allows(&record) {
                    continue;
                }
                return Some((Ok(record_to_sse(record)), state));
            }
            match state.receiver.recv().await {
                Ok(record) if record.id > state.last_id => {
                    state.last_id = record.id;
                    if !state.owner_filter.allows(&record) {
                        continue;
                    }
                    return Some((Ok(record_to_sse(record)), state));
                }
                Ok(_) => {}
                Err(broadcast::error::RecvError::Lagged(_)) => {
                    state.pending = state.events.replay_after(state.last_id);
                }
                Err(broadcast::error::RecvError::Closed) => return None,
            }
        }
    });
    let ready =
        stream::once(async { Ok::<Event, Infallible>(Event::default().comment("connected")) });
    let stream = ready.chain(events);
    Ok(Sse::new(stream).keep_alive(
        KeepAlive::new()
            .interval(Duration::from_secs(15))
            .text("keepalive"),
    ))
}

/// 控制台「数据统计」数据源:选定范围的汇总/环比基线 + 364 天日序列 +
/// 按来源(agent/各平台)分组的模型明细。
pub(in crate::web) async fn usage_stats_web(
    State(state): State<DaemonState>,
    headers: HeaderMap,
    Query(query): Query<UsageStatsQuery>,
) -> std::result::Result<Response, ApiError> {
    let identity = require_identity(&headers, &state)?;
    let range = crate::state::UsageRange::parse(query.range.as_deref().unwrap_or("1d"));
    let config = state.manager.lock().unwrap().config.clone();
    crate::models_cache::ensure_active_metadata(&state.paths, &config);
    // 归属(阶段 5):成员只看自己;管理员默认看全部(附按人拆分),也可按账号筛。
    let account = usage_account_filter(&identity, query.account.as_deref());
    // 整读整解析 usage-history.jsonl，而那个文件只增不轮转：本机 5.7 天就
    // 攒到 2.2 MB / 86 ms，一年是 141 MB / 5.5 秒。同步跑就是把一个 tokio
    // worker 冻这么久。两条工具路径（platforms/tool.rs、tools/usage_query.rs）
    // 早就是 spawn_blocking，这两个 handler 漏了。
    let store = state.state_store.clone();
    let stats = tokio::task::spawn_blocking(move || {
        store.usage_stats_for_account(range, Some(&config), account.as_deref())
    })
    .await
    .map_err(ApiError::internal)?
    .map_err(ApiError::internal)?;
    Ok(Json(json!({ "ok": true, "stats": stats })).into_response())
}

/// 清空 token 统计明细。只删 usage-history.jsonl(图表与最近调用的数据源),
/// 累计正账 usage.json 保留——那是"一生用了多少"的唯一记录,删了找不回来。
pub(in crate::web) async fn usage_clear_web(
    State(state): State<DaemonState>,
    headers: HeaderMap,
) -> std::result::Result<Response, ApiError> {
    require_admin_mutation(&headers, &state)?;
    let store = state.state_store.clone();
    tokio::task::spawn_blocking(move || store.clear_usage_history())
        .await
        .map_err(ApiError::internal)?
        .map_err(ApiError::internal)?;
    Ok(Json(json!({ "ok": true })).into_response())
}

pub(in crate::web) async fn usage_details_web(
    State(state): State<DaemonState>,
    headers: HeaderMap,
    Query(query): Query<UsageDetailsQuery>,
) -> std::result::Result<Response, ApiError> {
    let identity = require_identity(&headers, &state)?;
    let limit = query.limit.unwrap_or(50).clamp(1, 500);
    let config = state.manager.lock().unwrap().config.clone();
    crate::models_cache::ensure_active_metadata(&state.paths, &config);
    let account = usage_account_filter(&identity, query.account.as_deref());
    let store = state.state_store.clone();
    let (src, model) = (query.src.clone(), query.model.clone());
    let records = tokio::task::spawn_blocking(move || {
        store.usage_details_for_account(
            limit,
            src.as_deref(),
            model.as_deref(),
            Some(&config),
            account.as_deref(),
        )
    })
    .await
    .map_err(ApiError::internal)?
    .map_err(ApiError::internal)?;
    Ok(Json(json!({ "ok": true, "records": records })).into_response())
}

/// 用量接口的账号过滤:成员锁死自己;管理员按 `account` 参数(None = 全部)。
fn usage_account_filter(identity: &WebIdentity, requested: Option<&str>) -> Option<String> {
    if identity.admin {
        requested.map(str::to_string)
    } else {
        Some(identity.owner_key().to_string())
    }
}

pub(in crate::web) use crate::runtime::trim_process_memory;

pub(in crate::web) async fn shutdown_signal() {
    // systemd stop / `kill` 发的是 SIGTERM：必须与 SIGINT 一样走优雅停机
    // （落盘运行中回合、清理 IPC lease），否则默认动作直接杀进程。
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        match signal(SignalKind::terminate()) {
            Ok(mut sigterm) => {
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => {}
                    _ = sigterm.recv() => {}
                }
            }
            Err(_) => {
                let _ = tokio::signal::ctrl_c().await;
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}
