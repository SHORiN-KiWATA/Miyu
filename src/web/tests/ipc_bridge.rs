//! IPC 桥：会话通道、工具桥、配置热重载。

use super::shared::*;
use crate::state::PlatformSessionBindingKey;
use crate::web::*;

#[tokio::test]
async fn one_shot_sessions_are_mintable_runnable_and_deletable_but_nothing_else() {
    let temp = tempfile::tempdir().unwrap();
    let state = DaemonState::for_test(test_paths(temp.path()), 8300).unwrap();
    let persona = active_persona_scope(&state);
    state
        .state_store
        .adopt_sessions_for_persona(&persona)
        .unwrap();
    let terminal = state.state_store.session_id().to_string();

    let data = handle_session_command(
        &state,
        IpcCommand::CreateSession {
            name: Some("一次性对话".to_string()),
            switch: false,
            kind: Some(crate::state::ASK_SESSION_KIND.to_string()),
            mode: None,
        },
    )
    .await
    .unwrap();
    let ask_id = data["session"]["session_id"].as_str().unwrap().to_string();

    // Minting it must not move the terminal lane, and it must not surface
    // in the session list.
    assert_eq!(&*state.state_store.session_id(), terminal.as_str());
    let listed = handle_session_command(&state, IpcCommand::ListSessions { mode: None })
        .await
        .unwrap();
    assert!(listed["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .all(|session| session["session_id"] != ask_id.as_str()));

    // A turn may target it. (SwitchSession 已随「终端集成会话不可改」
    // 整体移除,外部再无切换全局指针的入口。)
    assert_eq!(
        resolve_turn_session(&state, Some(ask_id.clone())).unwrap(),
        ask_id.clone().into()
    );

    // Other kinds are not mintable over IPC, and `ask` may not be created
    // as the session to switch into.
    assert!(handle_session_command(
        &state,
        IpcCommand::CreateSession {
            name: None,
            switch: false,
            kind: Some("subagent".to_string()),
            mode: None,
        },
    )
    .await
    .is_err());
    assert!(handle_session_command(
        &state,
        IpcCommand::CreateSession {
            name: None,
            switch: true,
            kind: Some(crate::state::ASK_SESSION_KIND.to_string()),
            mode: None,
        },
    )
    .await
    .is_err());

    // Deleting it is the teardown a one-shot turn performs.
    handle_session_command(
        &state,
        IpcCommand::DeleteSession {
            target: ipc::SessionRef::Id { id: ask_id.clone() },
        },
    )
    .await
    .unwrap();
    assert!(state.state_store.session_record(&ask_id).unwrap().is_none());
    assert!(resolve_turn_session(&state, Some(ask_id)).is_err());
}

/// 终端集成、普通 REPL、开发 REPL 是三条并行车道。
///
/// normal 以前在指针缺失时退到 `session_id()`——也就是终端集成那条——于是第一
/// 次 `miyu normal` 就把两边焊在同一个会话上，shellhook 的对话和 REPL 的对话
/// 混进同一段历史。dev 早就是自举的，normal 没跟上。
#[test]
fn the_three_repl_lanes_never_share_a_session() {
    let temp = tempfile::tempdir().unwrap();
    let store = StateStore::new(&test_paths(temp.path())).unwrap();
    store.init_files().unwrap();
    let terminal = store.session_id().to_string();

    let normal = store.ensure_repl_session("default").unwrap();
    let dev = store
        .ensure_repl_session(crate::state::DEV_PERSONA)
        .unwrap();

    assert_ne!(normal, terminal, "普通 REPL 不能落在终端集成会话上");
    assert_ne!(dev, terminal, "开发 REPL 不能落在终端集成会话上");
    assert_ne!(normal, dev, "普通与开发是两条车道");
    // 自举不许移动终端车道——shellhook 还在那边说话。
    assert_eq!(&*store.session_id(), terminal.as_str());

    // 指针钉住了：再进一次回到同一条，不会每次开新会话。
    assert_eq!(store.ensure_repl_session("default").unwrap(), normal);
    assert_eq!(
        store
            .ensure_repl_session(crate::state::DEV_PERSONA)
            .unwrap(),
        dev
    );

    // 会话被删之后指针失效，换一条新的，而不是掉回终端车道。
    store.delete_session(&normal).unwrap();
    let healed = store.ensure_repl_session("default").unwrap();
    assert_ne!(healed, normal);
    assert_ne!(healed, terminal, "指针失效也不该退到终端集成会话");
}

#[tokio::test]
async fn repl_session_lane_resumes_and_heals_without_moving_the_terminal_lane() {
    let temp = tempfile::tempdir().unwrap();
    let state = DaemonState::for_test(test_paths(temp.path()), 8300).unwrap();
    let persona = active_persona_scope(&state);
    state
        .state_store
        .adopt_sessions_for_persona(&persona)
        .unwrap();
    let terminal = state.state_store.session_id().to_string();
    let repl = state
        .state_store
        .create_session(&persona, "repl lane", crate::state::USER_SESSION_KIND, None)
        .unwrap();

    handle_session_command(
        &state,
        IpcCommand::SetReplSession {
            target: ipc::SessionRef::Id {
                id: repl.session_id.clone(),
            },
        },
    )
    .await
    .unwrap();
    assert_eq!(
        state.state_store.repl_session(&persona).unwrap().as_deref(),
        Some(repl.session_id.as_str())
    );
    assert_eq!(&*state.state_store.session_id(), terminal.as_str());

    // A deleted REPL session must not strand the next REPL: the pointer
    // falls back to the terminal session and is healed in place.
    state.state_store.delete_session(&repl.session_id).unwrap();
    assert!(state.state_store.repl_session(&persona).unwrap().is_none());

    // One-shot sessions are not a valid REPL lane either.
    let ask = state
        .state_store
        .create_session(&persona, "一次性对话", crate::state::ASK_SESSION_KIND, None)
        .unwrap();
    assert!(handle_session_command(
        &state,
        IpcCommand::SetReplSession {
            target: ipc::SessionRef::Id {
                id: ask.session_id.clone(),
            },
        },
    )
    .await
    .is_err());
}

#[tokio::test]
async fn ipc_session_list_excludes_platform_owned_sessions() {
    let temp = tempfile::tempdir().unwrap();
    let state = DaemonState::for_test(test_paths(temp.path()), 8300).unwrap();
    let persona = active_persona_scope(&state);
    state
        .state_store
        .adopt_sessions_for_persona(&persona)
        .unwrap();
    let local = state
        .state_store
        .create_session(&persona, "local", "user", None)
        .unwrap();
    let platform = state
        .state_store
        .create_session(&persona, "platform", "user", None)
        .unwrap();
    state
        .state_store
        .bind_platform_session(
            &PlatformSessionBindingKey {
                platform: "onebot".to_string(),
                account_id: "10000".to_string(),
                conversation_kind: "group".to_string(),
                conversation_id: "20000".to_string(),
                participant_id: None,
                persona: persona.clone(),
            },
            &platform.session_id,
        )
        .unwrap();

    let data = handle_session_command(&state, IpcCommand::ListSessions { mode: None })
        .await
        .unwrap();
    let ids = data["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|session| session["session_id"].as_str())
        .collect::<Vec<_>>();

    assert!(ids.contains(&local.session_id.as_str()));
    assert!(!ids.contains(&platform.session_id.as_str()));
}

#[tokio::test]
async fn tool_bridge_executes_with_session_scope_and_depth_guard() {
    let temp = tempfile::tempdir().unwrap();
    let state = DaemonState::for_test(test_paths(temp.path()), 8300).unwrap();
    let persona = active_persona_scope(&state);
    let session = state
        .state_store
        .create_session(&persona, "bridge", crate::state::USER_SESSION_KIND, None)
        .unwrap()
        .session_id;
    // 会话作用域生效:get_goal 在指定会话身份下执行,拿到 goal:null。
    let data = handle_session_command(
        &state,
        IpcCommand::ToolCall {
            session: Some(session.clone()),
            name: "job".to_string(),
            arguments: "{}".to_string(),
            origin: None,
            depth: 0,
        },
    )
    .await
    .unwrap();
    let output = data["output"].as_str().unwrap();
    assert!(!output.is_empty(), "unexpected: {output}");
    // 深度护栏。
    let denied = handle_session_command(
        &state,
        IpcCommand::ToolCall {
            session: Some(session),
            name: "job".to_string(),
            arguments: "{}".to_string(),
            origin: None,
            depth: crate::tools::workspace::MAX_BRIDGE_DEPTH,
        },
    )
    .await
    .unwrap_err();
    assert!(denied.contains("recursion limit"));
}

/// 目录↔调用同源(dev 实测回归):--list 走的 ToolCatalog 与 ToolCall
/// 同一条会话→模式→registry 解析链;dev 目录=dev registry,普通会话
/// 反之;未列出的名字调用报 unknown + `--list` 路标。
#[tokio::test]
async fn tool_catalog_matches_the_bridge_callable_set() {
    let temp = tempfile::tempdir().unwrap();
    let state = DaemonState::for_test(test_paths(temp.path()), 8300).unwrap();
    let data = handle_session_command(
        &state,
        IpcCommand::CreateSession {
            name: Some("dev bridge".to_string()),
            switch: false,
            kind: None,
            mode: Some("dev".to_string()),
        },
    )
    .await
    .unwrap();
    let dev_session = data["session"]["session_id"].as_str().unwrap().to_string();

    let catalog = handle_session_command(
        &state,
        IpcCommand::ToolCatalog {
            full: false,
            session: Some(dev_session.clone()),
            name: None,
        },
    )
    .await
    .unwrap();
    assert_eq!(catalog["mode"], "dev");
    let names = catalog["tools"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|tool| tool["name"].as_str())
        .collect::<Vec<_>>();
    assert!(names.contains(&"run_command"), "{names:?}");
    assert!(names.contains(&"edit"), "{names:?}");
    assert!(
        !names.contains(&"trash_path"),
        "dev 目录不应混入普通人格工具: {names:?}"
    );

    // 未列出的名字调用即报 unknown,且带 --list 路标。
    let denied = handle_session_command(
        &state,
        IpcCommand::ToolCall {
            session: Some(dev_session.clone()),
            name: "trash_path".to_string(),
            arguments: "{}".to_string(),
            origin: None,
            depth: 0,
        },
    )
    .await
    .unwrap_err();
    assert!(denied.contains("unknown tool"), "{denied}");
    assert!(denied.contains("--list"), "{denied}");

    // 普通会话(缺省=当前会话)目录反之。
    let catalog = handle_session_command(
        &state,
        IpcCommand::ToolCatalog {
            full: false,
            session: None,
            name: None,
        },
    )
    .await
    .unwrap();
    assert_eq!(catalog["mode"], "normal");
    let names = catalog["tools"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|tool| tool["name"].as_str())
        .collect::<Vec<_>>();
    assert!(names.contains(&"trash_path"), "{names:?}");

    // describe 同源返回完整合同。
    let described = handle_session_command(
        &state,
        IpcCommand::ToolCatalog {
            full: false,
            session: Some(dev_session),
            name: Some("run_command".to_string()),
        },
    )
    .await
    .unwrap();
    assert_eq!(described["tool"]["name"], "run_command");
    assert!(described["tool"]["parameters"]["properties"]["command"].is_object());
}

#[tokio::test]
async fn dev_sessions_live_under_the_reserved_persona_and_pin_dev_mode() {
    let temp = tempfile::tempdir().unwrap();
    let state = DaemonState::for_test(test_paths(temp.path()), 8300).unwrap();
    // mode:"dev" 建到保留人格 dev 名下。
    let data = handle_session_command(
        &state,
        IpcCommand::CreateSession {
            name: Some("dev work".to_string()),
            switch: false,
            kind: None,
            mode: Some("dev".to_string()),
        },
    )
    .await
    .unwrap();
    let session_id = data["session"]["session_id"].as_str().unwrap().to_string();
    let record = state
        .state_store
        .session_record(&session_id)
        .unwrap()
        .unwrap();
    assert_eq!(record.persona, crate::state::DEV_PERSONA);
    // 会话模式由记录强制:dev 会话怎么请求都是 Dev,普通会话反之。
    assert_eq!(
        turn_mode_for_session(&state.state_store, &session_id, AgentMode::Normal),
        AgentMode::Dev
    );
    let normal_id = state.state_store.session_id().to_string();
    assert_eq!(
        turn_mode_for_session(&state.state_store, &normal_id, AgentMode::Dev),
        AgentMode::Normal
    );
}

#[tokio::test]
async fn creating_a_repl_session_does_not_move_the_default_session() {
    let temp = tempfile::tempdir().unwrap();
    let state = DaemonState::for_test(test_paths(temp.path()), 8300).unwrap();
    let persona = active_persona_scope(&state);
    state
        .state_store
        .adopt_sessions_for_persona(&persona)
        .unwrap();
    let default_session_id = state.state_store.session_id();

    let data = handle_session_command(
        &state,
        IpcCommand::CreateSession {
            name: Some("repl local".to_string()),
            switch: false,
            kind: None,
            mode: None,
        },
    )
    .await
    .unwrap();

    assert_ne!(
        data["session"]["session_id"].as_str(),
        Some(default_session_id.as_ref())
    );
    assert_eq!(&*state.state_store.session_id(), &*default_session_id);
}

#[tokio::test]
async fn actor_undo_is_scoped_to_the_requested_session() {
    let temp = tempfile::tempdir().unwrap();
    let (state, actor_join) = test_daemon_with_actor(temp.path());
    let persona = active_persona_scope(&state);
    state
        .state_store
        .adopt_sessions_for_persona(&persona)
        .unwrap();
    let default_session_id = state.state_store.session_id();
    let default_store = state.state_store.pinned(&default_session_id);
    default_store
        .start_turn("default-turn", "default", std::process::id())
        .unwrap();
    default_store
        .complete_turn("default-turn", "default reply", None)
        .unwrap();
    let local = state
        .state_store
        .create_session(&persona, "repl local", "user", None)
        .unwrap();
    let local_store = state.state_store.pinned(&local.session_id);
    local_store
        .start_turn("local-turn", "local", std::process::id())
        .unwrap();
    local_store
        .complete_turn("local-turn", "local reply", None)
        .unwrap();

    let (reply, receiver) = oneshot::channel();
    state
        .actor_tx
        .send(ActorCommand::Undo {
            session_id: local.session_id.clone().into(),
            reply,
        })
        .unwrap();
    receiver.await.unwrap().unwrap();

    assert!(local_store.load_turns().unwrap().is_empty());
    assert_eq!(default_store.load_turns().unwrap().len(), 1);
    assert_eq!(&*state.state_store.session_id(), &*default_session_id);
    state.actor_tx.send(ActorCommand::Shutdown).unwrap();
    actor_join.join().unwrap().unwrap();
}

#[tokio::test]
async fn reset_memory_bumps_generation_in_the_requested_scope_only() {
    let temp = tempfile::tempdir().unwrap();
    let state = DaemonState::for_test(test_paths(temp.path()), 8300).unwrap();
    let config = state.manager.lock().unwrap().config.clone();
    let dev_store = crate::memory::MemoryStore::new(&config.dev_scoped(), &state.paths);
    dev_store.init().unwrap();
    let normal_store = crate::memory::MemoryStore::new(&config, &state.paths);
    normal_store.init().unwrap();
    let (_, dev_gen_before) = dev_store.identity().unwrap();
    let (_, normal_gen_before) = normal_store.identity().unwrap();

    handle_session_command(
        &state,
        IpcCommand::ResetMemory {
            mode: Some("dev".to_string()),
            scope: ipc::MemoryResetScope::All,
            session: None,
        },
    )
    .await
    .unwrap();

    // 只有 dev 命名空间的代数被抬升,普通人格纹丝不动。
    let (_, dev_gen_after) = dev_store.identity().unwrap();
    let (_, normal_gen_after) = normal_store.identity().unwrap();
    assert_eq!(dev_gen_after, dev_gen_before + 1);
    assert_eq!(normal_gen_after, normal_gen_before);
}

/// 会话级重置清的是命令里点名的那个会话，不是 daemon 的全局指针——REPL 可以
/// 挂在一条会话上而指针早就跟着别处走了。
#[tokio::test]
async fn session_scoped_memory_reset_honours_the_named_session() {
    let temp = tempfile::tempdir().unwrap();
    let state = DaemonState::for_test(test_paths(temp.path()), 8300).unwrap();
    let persona = active_persona_scope(&state);
    state
        .state_store
        .adopt_sessions_for_persona(&persona)
        .unwrap();
    let target = state
        .state_store
        .create_session(&persona, "被点名的会话", "user", None)
        .unwrap();
    let config = state.manager.lock().unwrap().config.clone();
    let store = crate::memory::MemoryStore::new(&config, &state.paths);
    store
        .clone()
        .with_session_id(&target.session_id)
        .remember_fact("点名会话记住 XMODIFIERS 这件事", "test")
        .unwrap();
    store
        .clone()
        .with_session_id(&state.state_store.session_id())
        .remember_fact("指针会话记住 XMODIFIERS 这件事", "test")
        .unwrap();

    let data = handle_session_command(
        &state,
        IpcCommand::ResetMemory {
            mode: None,
            scope: ipc::MemoryResetScope::Session,
            session: Some(ipc::SessionRef::Id {
                id: target.session_id.clone(),
            }),
        },
    )
    .await
    .unwrap();
    assert_eq!(data["summary"]["facts"], 1);

    let recalled = store
        .recall_memories("XMODIFIERS", 5, false)
        .unwrap()
        .to_string();
    assert!(!recalled.contains("点名会话"), "点名的会话没被清掉");
    assert!(recalled.contains("指针会话"), "清到全局指针那条会话头上了");
}

#[tokio::test]
async fn dev_sessions_resolve_by_id_and_list_under_dev_mode() {
    let temp = tempfile::tempdir().unwrap();
    let state = DaemonState::for_test(test_paths(temp.path()), 8300).unwrap();
    let persona = active_persona_scope(&state);
    state
        .state_store
        .adopt_sessions_for_persona(&persona)
        .unwrap();
    let dev = state
        .state_store
        .create_session(crate::state::DEV_PERSONA, "编译修复", "user", None)
        .unwrap();

    // 验收问题二:显式 id 寻址必须穿过人格过滤,否则 dev REPL 的
    // 起回合/切换全部 404 并落回默认会话。
    let resolved = resolve_local_session_ref(
        &state,
        &ipc::SessionRef::Id {
            id: dev.session_id.clone(),
        },
    )
    .unwrap();
    assert_eq!(resolved.session_id, dev.session_id);
    assert_eq!(resolved.persona, crate::state::DEV_PERSONA);

    // dev 会话不进普通人格的名字空间;dev 模式列表只见 dev 会话。
    assert!(resolve_local_session_ref(
        &state,
        &ipc::SessionRef::Name {
            name: "编译修复".to_string(),
        },
    )
    .is_err());
    let listed = handle_session_command(
        &state,
        IpcCommand::ListSessions {
            mode: Some("dev".to_string()),
        },
    )
    .await
    .unwrap();
    let ids: Vec<&str> = listed["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|session| session["session_id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, vec![dev.session_id.as_str()]);
    let normal = handle_session_command(&state, IpcCommand::ListSessions { mode: None })
        .await
        .unwrap();
    assert!(normal["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .all(|session| session["session_id"].as_str() != Some(dev.session_id.as_str())));
}

#[tokio::test]
async fn config_reload_applies_disk_config() {
    let temp = tempfile::tempdir().unwrap();
    let (state, actor_join) = test_daemon_with_actor(temp.path());
    let mut next_config = state.manager.lock().unwrap().config.clone();
    next_config.display.show_token_usage = !next_config.display.show_token_usage;
    let expected = next_config.display.show_token_usage;
    next_config.save(&state.paths).unwrap();

    let (mut client, server) = tokio::net::UnixStream::pair().unwrap();
    let server_state = state.clone();
    let task = tokio::spawn(async move { handle_ipc_connection(server_state, server).await });
    ipc::send(&mut client, &IpcRequest::new(IpcCommand::ReloadConfig))
        .await
        .unwrap();
    let response = ipc::receive::<IpcFrame>(&mut client)
        .await
        .unwrap()
        .unwrap();

    assert!(matches!(response, IpcFrame::AdminResult { .. }));
    task.await.unwrap().unwrap();
    let manager = state.manager.lock().unwrap();
    assert_eq!(manager.config.display.show_token_usage, expected);
    assert!(!manager.admin_busy);
    drop(manager);

    state.actor_tx.send(ActorCommand::Shutdown).unwrap();
    actor_join.join().unwrap().unwrap();
}

#[tokio::test]
async fn failed_config_reload_preserves_the_candidate_file() {
    let temp = tempfile::tempdir().unwrap();
    let state = DaemonState::for_test(test_paths(temp.path()), 8300).unwrap();
    let runtime_value = state
        .manager
        .lock()
        .unwrap()
        .config
        .display
        .show_token_usage;
    let mut candidate = state.manager.lock().unwrap().config.clone();
    candidate.display.show_token_usage = !runtime_value;
    candidate.save(&state.paths).unwrap();

    let (mut client, server) = tokio::net::UnixStream::pair().unwrap();
    let server_state = state.clone();
    let task = tokio::spawn(async move { handle_ipc_connection(server_state, server).await });
    ipc::send(&mut client, &IpcRequest::new(IpcCommand::ReloadConfig))
        .await
        .unwrap();
    let response = ipc::receive::<IpcFrame>(&mut client)
        .await
        .unwrap()
        .unwrap();

    assert!(matches!(
        response,
        IpcFrame::Error {
            code: None,
            message,
        } if message.contains("worker is unavailable")
    ));
    task.await.unwrap().unwrap();
    assert_eq!(
        AppConfig::load(&state.paths)
            .unwrap()
            .display
            .show_token_usage,
        !runtime_value
    );
    let manager = state.manager.lock().unwrap();
    assert_eq!(manager.config.display.show_token_usage, runtime_value);
    assert!(!manager.admin_busy);
}

#[tokio::test]
async fn busy_config_reload_returns_an_error_frame() {
    let temp = tempfile::tempdir().unwrap();
    let (state, actor_join) = test_daemon_with_actor(temp.path());
    state
        .manager
        .lock()
        .unwrap()
        .config
        .save(&state.paths)
        .unwrap();
    // Running turns no longer block a reload (they keep their own config
    // snapshot); only a concurrent admin operation does.
    state.manager.lock().unwrap().admin_busy = true;

    let (mut client, server) = tokio::net::UnixStream::pair().unwrap();
    let server_state = state.clone();
    let task = tokio::spawn(async move { handle_ipc_connection(server_state, server).await });
    ipc::send(&mut client, &IpcRequest::new(IpcCommand::ReloadConfig))
        .await
        .unwrap();
    let response = ipc::receive::<IpcFrame>(&mut client)
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(
        response,
        IpcFrame::Error {
            code: Some(ipc::ErrorCode::Busy),
            message,
        } if message.contains("busy with another operation")
    ));
    task.await.unwrap().unwrap();

    state.manager.lock().unwrap().admin_busy = false;
    state.actor_tx.send(ActorCommand::Shutdown).unwrap();
    actor_join.join().unwrap().unwrap();
}

#[tokio::test]
async fn config_reload_succeeds_and_keeps_turns_running() {
    let temp = tempfile::tempdir().unwrap();
    let (state, actor_join) = test_daemon_with_actor(temp.path());
    state
        .manager
        .lock()
        .unwrap()
        .config
        .save(&state.paths)
        .unwrap();
    let (cancel, cancel_rx) = tokio::sync::watch::channel(false);
    state.manager.lock().unwrap().active_runs.insert(
        "hot-reload-run".to_string(),
        RunInfo {
            session_id: state.state_store.session_id().into(),
            mode: AgentMode::Normal,
            audience: PromptAudience::External,
            cancel,
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

    let (mut client, server) = tokio::net::UnixStream::pair().unwrap();
    let server_state = state.clone();
    let task = tokio::spawn(async move { handle_ipc_connection(server_state, server).await });
    ipc::send(&mut client, &IpcRequest::new(IpcCommand::ReloadConfig))
        .await
        .unwrap();
    let response = ipc::receive::<IpcFrame>(&mut client)
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(response, IpcFrame::AdminResult { .. }));
    task.await.unwrap().unwrap();

    // A turn-safe reload neither cancels nor waits out the running turn.
    assert!(!*cancel_rx.borrow());
    {
        let manager = state.manager.lock().unwrap();
        assert!(manager.active_runs.contains_key("hot-reload-run"));
        assert!(!manager.admin_busy);
    }

    state.manager.lock().unwrap().active_runs.clear();
    state.actor_tx.send(ActorCommand::Shutdown).unwrap();
    actor_join.join().unwrap().unwrap();
}

#[tokio::test]
async fn set_session_models_ipc_pins_and_clears_the_override() {
    let temp = tempfile::tempdir().unwrap();
    let state = DaemonState::for_test(test_paths(temp.path()), 8300).unwrap();
    let choice = state
        .manager
        .lock()
        .unwrap()
        .config
        .text_provider_model_choices()
        .first()
        .cloned()
        .expect("the default config configures at least one model");
    let persona = active_persona_scope(&state);
    let record = state
        .state_store
        .create_session(&persona, "", "user", None)
        .unwrap();
    let target = ipc::SessionRef::Id {
        id: record.session_id.clone(),
    };

    handle_session_command(
        &state,
        IpcCommand::SetSessionModels {
            target: target.clone(),
            models: vec![crate::config::ActiveProviderModelConfig {
                provider_id: choice.provider_id.clone(),
                model: choice.model.clone(),
            }],
        },
    )
    .await
    .unwrap();
    let session_id = record.session_id.clone();
    let stored = state
        .state_store
        .session_model_override(&session_id)
        .unwrap()
        .expect("the override is stored");
    assert_eq!(stored.len(), 1);
    assert_eq!(stored[0].provider_id, choice.provider_id);
    assert_eq!(stored[0].model, choice.model);

    // Unknown models are rejected and leave the override untouched.
    let error = handle_session_command(
        &state,
        IpcCommand::SetSessionModels {
            target: target.clone(),
            models: vec![crate::config::ActiveProviderModelConfig {
                provider_id: "no-such-provider".to_string(),
                model: "no-such-model".to_string(),
            }],
        },
    )
    .await
    .unwrap_err();
    assert!(error.contains("no-such-provider"));
    assert!(state
        .state_store
        .session_model_override(&session_id)
        .unwrap()
        .is_some());

    // An empty list clears the override (follow the global pool again).
    handle_session_command(
        &state,
        IpcCommand::SetSessionModels {
            target,
            models: Vec::new(),
        },
    )
    .await
    .unwrap();
    assert!(state
        .state_store
        .session_model_override(&session_id)
        .unwrap()
        .is_none());
}

/// 桥的工具面必须和主线回合同一套判据。REPL 是 owner 面,artifact 那套
/// 预览文件只有 WebUI 渲染得出来;桥原来只看 mode,于是 claude-code
/// (工具只能从 MCP 桥拿)在 REPL 里照样拿到,模型把文档"扔进 artifact"
/// 而用户看不见(08-29 实测)。
#[tokio::test]
async fn the_bridge_hands_out_artifact_tools_only_to_webui_turns() {
    let temp = tempfile::tempdir().unwrap();
    let state = DaemonState::for_test(test_paths(temp.path()), 8300).unwrap();
    let persona = active_persona_scope(&state);
    state
        .state_store
        .adopt_sessions_for_persona(&persona)
        .unwrap();
    let session_id = state.state_store.session_id().to_string();
    let config = { state.manager.lock().unwrap().config.clone() };

    let tools_with_running_audience = |audience: Option<PromptAudience>| {
        {
            let mut manager = state.manager.lock().unwrap();
            manager.active_runs.clear();
            if let Some(audience) = audience {
                manager.active_runs.insert(
                    "run_probe".to_string(),
                    RunInfo {
                        session_id: session_id.clone().into(),
                        mode: AgentMode::Normal,
                        audience,
                        cancel: tokio::sync::watch::channel(false).0,
                        turn_id: None,
                        queue_target: None,
                        supersede: Arc::new(crate::agent::TurnSupersedeSignal::default()),
                        platform_followup: None,
                        operation: RunOperation::Create,
                        job_wake: false,
                        job_wake_label: None,
                        turn_origin: crate::tools::workspace::TurnOrigin::Human,
                    },
                );
            }
        }
        let mut registry =
            crate::tools::build_tool_registry(&config, &state.paths, AgentMode::Normal, false)
                .unwrap();
        attach_owner_turn_tools(
            &mut registry,
            &state,
            &config,
            AgentMode::Normal,
            &session_id,
        );
        registry.contains("artifact")
    };

    // WebUI 回合(External + 无平台 profile)才给。
    assert!(tools_with_running_audience(Some(PromptAudience::External)));
    // REPL 回合是 owner 面,不给。
    assert!(!tools_with_running_audience(Some(PromptAudience::Owner)));
    // 回合外的桥调用(miyu tool-call)同样不给:宁可少给。
    assert!(!tools_with_running_audience(None));
}
