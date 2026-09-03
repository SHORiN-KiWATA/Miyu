//! 会话相关的 IPC 命令分发。
//!
//! `handle_session_command` 是一个大 match：终端那边的每个会话操作都从这里进。
//! 做成一个函数而不是一堆小函数，是因为它们共享同一套前置检查（会话存不存在、
//! 有没有在跑回合、是不是平台会话）。

use crate::web::*;

/// Old WebUI versions could make a platform-owned conversation the global
/// current session. Repair that pointer before constructing the local agent
/// so QQ history can never become the WebUI/CLI startup conversation.
pub(in crate::web) fn ensure_local_current_session(
    state_store: &StateStore,
    persona: &str,
) -> Result<()> {
    let current_session_id = state_store.session_id();
    if is_available_local_session(state_store, &current_session_id, persona)? {
        return Ok(());
    }

    let target_session_id = match state_store.list_local_sessions(persona)?.into_iter().next() {
        Some(overview) => overview.record.session_id,
        None => {
            state_store
                .create_session(persona, "", "user", None)?
                .session_id
        }
    };
    state_store.switch_session(&target_session_id)
}

pub(in crate::web) fn is_available_local_session(
    state_store: &StateStore,
    session_id: &str,
    persona: &str,
) -> Result<bool> {
    let usable = state_store
        .session_record(session_id)?
        .is_some_and(|record| record.persona == persona && record.kind == "user");
    Ok(usable && !state_store.is_platform_session(session_id)?)
}

/// Handles the session-management IPC commands. Returns the `AdminResult`
/// payload on success or a user-facing error message.
pub(in crate::web) async fn handle_session_command(
    state: &DaemonState,
    command: IpcCommand,
) -> std::result::Result<Value, String> {
    let store = &state.state_store;
    let persona = active_persona_scope(state);
    match command {
        IpcCommand::SetRequestLogging { enabled } => {
            // 此刻可能还没构造过任何 LLM 客户端,目录未必已安装——就地
            // 安装,免得 current_file 返回 None、监控端拿兜底路径扑空。
            crate::llm::request_log::install_dir(state.paths.logs_dir());
            crate::llm::request_log::set_enabled(enabled);
            Ok(json!({
                "enabled": enabled,
                "file": crate::llm::request_log::current_file()
                    .map(|path| path.display().to_string()),
            }))
        }
        IpcCommand::ResetMemory { mode } => {
            // dev 记忆挂保留人格名下,与 Agent 构造同一把 dev_scoped 钥匙;
            // 生成号在 reset_all 里自增,进行中的回合据此识别陈旧句柄。
            let config = state.manager.lock().unwrap().config.clone();
            let config = if mode.as_deref() == Some("dev") {
                config.dev_scoped()
            } else {
                config
            };
            let memory = crate::memory::MemoryStore::new(&config, &state.paths);
            memory
                .reset_all(false)
                .map_err(|error| safe_error_message(&error))?;
            Ok(json!({}))
        }
        IpcCommand::ListSessions { mode } => {
            // dev 列表以 dev REPL 指针为"当前":全局指针指向普通会话,
            // 用它高亮永远落空。"all" 是管理面(miyu session):普通+dev
            // 合并按更新时间排,别的人格仍不可见。
            let dev = mode.as_deref() == Some("dev");
            let all = mode.as_deref() == Some("all");
            let current = if dev {
                store
                    .repl_session(crate::state::DEV_PERSONA)
                    .ok()
                    .flatten()
                    .unwrap_or_default()
                    .into()
            } else {
                store.session_id()
            };
            let sessions = if all {
                sessions_with_dev(store, &persona).map_err(|error| safe_error_message(&error))?
            } else {
                let scope = if dev {
                    crate::state::DEV_PERSONA.to_string()
                } else {
                    persona.clone()
                };
                store
                    .list_local_sessions(&scope)
                    .map_err(|error| safe_error_message(&error))?
            };
            let sessions: Vec<Value> = sessions
                .iter()
                .map(|overview| session_overview_json(overview, &current))
                .collect();
            Ok(json!({ "current": &*current, "sessions": sessions }))
        }
        IpcCommand::CreateSession {
            name,
            switch,
            kind,
            mode,
        } => {
            // Whitelisted: `ask` is the only non-user kind a client may mint,
            // and it is deliberately unswitchable — subagent audit sessions and
            // anything else stay daemon-internal.
            let kind = match kind.as_deref() {
                None | Some(crate::state::USER_SESSION_KIND) => crate::state::USER_SESSION_KIND,
                Some(crate::state::ASK_SESSION_KIND) if !switch => crate::state::ASK_SESSION_KIND,
                Some(_) => {
                    return Err(t("unsupported session kind", "不支持的会话类型").to_string())
                }
            };
            // No explicit name: leave it empty; the session is auto-named
            // from the first prompt when its first turn completes.
            let name = name.map(|name| name.trim().to_string()).unwrap_or_default();
            // dev 会话建到保留人格名下,模式由 persona 推导(见 DEV_PERSONA)。
            let session_persona = if mode.as_deref() == Some("dev") {
                crate::state::DEV_PERSONA
            } else {
                persona.as_str()
            };
            let record = store
                .create_session(session_persona, &name, kind, None)
                .map_err(|error| safe_error_message(&error))?;
            if kind == crate::state::USER_SESSION_KIND {
                // mode 必须一起发。前端收到事件就把会话插进列表了，此后
                // `createSession` 的 HTTP 响应（带 mode 的那份）会因为
                // 「已存在」被跳过——事件里少一个字段，新建的 dev 会话就
                // 一直挂在「普通模式」组下，直到刷新走 /api/sessions 才纠正。
                state.events.publish(
                    "session.created",
                    json!({
                        "session_id": record.session_id,
                        "name": record.name,
                        "mode": session_mode_label(&record),
                    }),
                );
            }
            if switch {
                switch_session_via_actor(state, record.session_id.clone()).await?;
            }
            Ok(json!({ "session": session_record_json(&record) }))
        }
        IpcCommand::ReorderSessions { session_ids } => {
            if session_ids.is_empty() {
                return Err(t("no sessions to reorder", "没有可排序的会话").to_string());
            }
            store
                .reorder_sessions(&session_ids)
                .map_err(|error| safe_error_message(&error))?;
            // 广播给其它客户端刷新列表;发起端在本地已乐观重排。
            state
                .events
                .publish("session.reordered", json!({ "session_ids": session_ids }));
            Ok(json!({ "ok": true }))
        }
        IpcCommand::ToolCall {
            session,
            name,
            arguments,
            origin,
            depth,
        } => {
            if depth >= crate::tools::workspace::MAX_BRIDGE_DEPTH {
                return Err(format!(
                    "tool bridge recursion limit reached (depth {depth})"
                ));
            }
            let session_id = match session {
                Some(session) => {
                    // 桥必须能寻址阅后即焚(ask)会话:回合正跑在里面,run_command 的
                    // 脚本与 MCP 桥的内层调用都以它为身份。只认 user 会话会让
                    // 单次 CLI 形态下的整条工具桥 404(真机实测踩坑)。
                    resolve_tool_bridge_session_ref(state, &ipc::SessionRef::Id { id: session })?
                        .session_id
                }
                None => store.session_id().to_string(),
            };
            let record = store
                .session_record(&session_id)
                .map_err(|error| safe_error_message(&error))?
                .ok_or_else(|| "session not found".to_string())?;
            let mode = turn_mode_for_session(store, &session_id, AgentMode::Normal);
            // 与回合同源的 registry(guard/超时齐备);会话工作区与来源
            // 一并作用域化,内层工具看到的世界和回合内一致。
            let config = { state.manager.lock().unwrap().config.clone() };
            let mut registry =
                crate::tools::build_tool_registry(&config, &state.paths, mode, false)
                    .map_err(|error| safe_error_message(&error))?;
            attach_owner_turn_tools(&mut registry, state, &config, mode, &session_id);
            if !registry.contains(&name) {
                // 桥专属报错:dev 实测里裸 "unknown tool" 让脚本作者盲试了
                // 一轮,这里把近似建议和"查目录"的路标一并给出。
                return Err(format!(
                    "tool error: {:#}. {}",
                    registry.unknown_tool_error(&name),
                    t(
                        "run `miyu tool-call --list` to see tools callable in this session",
                        "用 `miyu tool-call --list` 查看本会话可调用的工具"
                    )
                ));
            }
            let turn_origin: crate::tools::workspace::TurnOrigin = origin
                .as_deref()
                .and_then(|raw| serde_json::from_str(raw).ok())
                .unwrap_or(crate::tools::workspace::TurnOrigin::Human);
            let workspace = record
                .workspace
                .clone()
                .map(std::path::PathBuf::from)
                .filter(|path| path.is_dir())
                .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| ".".into()));
            let session_arc: Arc<str> = session_id.clone().into();
            let output = crate::tools::workspace::with_workspace(
                workspace,
                crate::tools::workspace::with_session(
                    session_arc,
                    crate::tools::workspace::with_turn_origin(
                        turn_origin,
                        crate::tools::workspace::with_bridge_depth(depth + 1, async {
                            // ask_question 是交互特例:注册表里只有报错桩,真实
                            // 流程走 broker 问答(与回合内同一条前端通道)。
                            if name == "ask_question" {
                                Ok(bridge_ask_question(state, &session_id, &arguments).await)
                            } else {
                                call_with_bridge_progress(
                                    state,
                                    &session_id,
                                    &registry,
                                    &name,
                                    &arguments,
                                )
                                .await
                            }
                        }),
                    ),
                ),
            )
            .await
            .map_err(|error| format!("tool error: {error:#}"))?;
            Ok(json!({ "output": output }))
        }
        IpcCommand::ToolCatalog {
            session,
            name,
            full,
        } => {
            // 与 ToolCall 同一条解析链(会话→模式→registry):`--list` 列出的
            // 就是本会话真能调的集合,`--describe` 查的合同也同源。此前
            // 客户端本地建表(按 MIYU_TURN_MODE 环境变量,run_command 并不
            // 注入它),dev 会话里 --list 展示的是普通人格全量目录,实测
            // 逐个调用全报 unknown tool。
            let session_id = match session {
                Some(session) => {
                    // 桥必须能寻址阅后即焚(ask)会话:回合正跑在里面,run_command 的
                    // 脚本与 MCP 桥的内层调用都以它为身份。只认 user 会话会让
                    // 单次 CLI 形态下的整条工具桥 404(真机实测踩坑)。
                    resolve_tool_bridge_session_ref(state, &ipc::SessionRef::Id { id: session })?
                        .session_id
                }
                None => store.session_id().to_string(),
            };
            let mode = turn_mode_for_session(store, &session_id, AgentMode::Normal);
            let config = { state.manager.lock().unwrap().config.clone() };
            let mut registry =
                crate::tools::build_tool_registry(&config, &state.paths, mode, false)
                    .map_err(|error| safe_error_message(&error))?;
            attach_owner_turn_tools(&mut registry, state, &config, mode, &session_id);
            let mode_label = match mode {
                AgentMode::Dev => "dev",
                AgentMode::Normal => "normal",
            };
            match name {
                Some(name) => {
                    let Some(spec) = registry.get(&name) else {
                        return Err(format!("{:#}", registry.unknown_tool_error(&name)));
                    };
                    Ok(json!({
                        "mode": mode_label,
                        "tool": {
                            "name": spec.name,
                            "description": spec.description,
                            "parameters": spec.parameters,
                        },
                    }))
                }
                None => {
                    let mut names = registry.tool_names();
                    names.sort();
                    let tools = names
                        .iter()
                        .map(|name| {
                            if full {
                                let spec = registry.get(name);
                                json!({
                                    "name": name,
                                    "display_name": registry.display_name(name),
                                    "description": spec.map(|spec| spec.description.clone()),
                                    "parameters": spec.map(|spec| spec.parameters.clone()),
                                })
                            } else {
                                json!({
                                    "name": name,
                                    "display_name": registry.display_name(name),
                                })
                            }
                        })
                        .collect::<Vec<_>>();
                    Ok(json!({ "mode": mode_label, "tools": tools }))
                }
            }
        }
        IpcCommand::SetReplSession { target } => {
            let record = resolve_available_local_session_ref(state, &target)?;
            // 终端集成会话可以切过去用(活体在 REPL 进程里),但指针不落盘:
            // 下次启动回到上一条普通会话,而不是终端车道(08-25 用户裁定)。
            if record.session_id != crate::state::DEFAULT_SESSION_ID {
                store
                    .set_repl_session(&record.persona, &record.session_id)
                    .map_err(|error| safe_error_message(&error))?;
            }
            Ok(json!({ "session": session_record_json(&record) }))
        }
        IpcCommand::RenameSession { target, name } => {
            let record = resolve_local_session_ref(state, &target)?;
            if record.session_id == crate::state::DEFAULT_SESSION_ID {
                return Err(t(
                    "the terminal-integration session cannot be renamed",
                    "终端集成会话不可重命名",
                )
                .to_string());
            }
            let name = name.trim();
            if name.is_empty() {
                return Err(t("session name cannot be empty", "会话名称不能为空").to_string());
            }
            store
                .rename_session(&record.session_id, name)
                .map_err(|error| safe_error_message(&error))?;
            state.events.publish(
                "session.renamed",
                json!({ "session_id": record.session_id, "name": name }),
            );
            Ok(json!({}))
        }
        IpcCommand::Goal { target, input } => {
            let record = resolve_local_session_ref(state, &target)?;
            let session_id = record.session_id;
            let text = crate::web::apply_goal_command(state, &session_id, &input);
            Ok(json!({ "text": text }))
        }
        IpcCommand::DeleteSession { target } => {
            // Accepts `ask` too: a one-shot turn deletes its own session here.
            let record = resolve_local_session_ref_with_kinds(state, &target, TURN_TARGET_KINDS)?;
            // 终端集成会话是 CLI/shellhook 的固定入口,永远只有这一个;
            // 清空用 /reset,删除免谈(验收:WebUI 不许改默认会话)。
            if record.session_id == crate::state::DEFAULT_SESSION_ID {
                return Err(t(
                    "the terminal-integration session cannot be deleted",
                    "终端集成会话不可删除",
                )
                .to_string());
            }
            // 运行中的会话也能删：先替用户按停止，等 run 退场再删。
            if state
                .manager
                .lock()
                .unwrap()
                .session_has_runs(&record.session_id)
            {
                crate::web::stop_session_runs(
                    state,
                    &record.session_id,
                    std::time::Duration::from_secs(5),
                )
                .await;
            }
            reserve_admin_for_session(&state.manager, &record.session_id)
                .map_err(|error| error.message)?;
            if &*store.session_id() == record.session_id.as_str() {
                let fallback = match fallback_session_id(state, &record.session_id) {
                    Ok(fallback) => fallback,
                    Err(error) => {
                        release_admin(&state.manager);
                        return Err(error);
                    }
                };
                if let Err(error) = switch_session_via_actor_reserved(state, fallback).await {
                    release_admin(&state.manager);
                    return Err(error);
                }
            }
            let result = store
                .delete_session(&record.session_id)
                .map_err(|error| safe_error_message(&error));
            crate::llm::forget_relay_sessions(&record.session_id);
            release_admin(&state.manager);
            result?;
            // 库里的目标行随会话级联删除；进程内的 goal 状态（armed 等）
            // 也一起清，不然条目在内存里陪跑到进程退出。
            crate::tools::goal::forget_session(&record.session_id);
            state.events.publish(
                "session.deleted",
                json!({ "session_id": record.session_id }),
            );
            Ok(json!({}))
        }
        IpcCommand::SetWorkspace { target, path } => {
            let record = resolve_local_session_ref(state, &target)?;
            let workspace = match path {
                Some(path) => {
                    if !path.is_dir() {
                        return Err(format!(
                            "{}: {}",
                            t("workspace is not a directory", "workspace 不是目录"),
                            path.display()
                        ));
                    }
                    Some(path.to_string_lossy().into_owned())
                }
                None => None,
            };
            store
                .set_session_workspace(&record.session_id, workspace.as_deref())
                .map_err(|error| safe_error_message(&error))?;
            state.events.publish(
                "session.updated",
                json!({ "session_id": record.session_id, "workspace": workspace }),
            );
            Ok(json!({}))
        }
        IpcCommand::SetSessionModels { target, models } => {
            let record = resolve_local_session_ref(state, &target)?;
            let models = (!models.is_empty()).then_some(models);
            if let Some(models) = &models {
                let choices = {
                    let manager = state.manager.lock().unwrap();
                    manager.config.text_provider_model_choices()
                };
                for model in models {
                    if !choices.iter().any(|choice| {
                        choice.provider_id == model.provider_id && choice.model == model.model
                    }) {
                        return Err(format!(
                            "{}{}/{}",
                            t("unknown model: ", "未知模型："),
                            model.provider_id,
                            model.model
                        ));
                    }
                }
            }
            store
                .set_session_model_override(&record.session_id, models.as_deref())
                .map_err(|error| safe_error_message(&error))?;
            state.events.publish(
                "session.updated",
                json!({
                    "session_id": record.session_id,
                    "model_override": models,
                }),
            );
            Ok(json!({ "session_id": record.session_id }))
        }
        _ => Err("unsupported session command".to_string()),
    }
}

/// 与回合装配同源的本机加料(task.rs 同条件):artifact 与 share 工具是
/// 每回合注册进 normal 表的,基础注册表里没有;桥的目录与调用两边都要补,
/// 否则 claude 经桥看到的世界与回合内不同源。平台会话进不了桥(解析器已
/// 拒),dev 表与回合装配一样不含这两组。
pub(in crate::web) fn attach_owner_turn_tools(
    registry: &mut crate::tools::ToolRegistry,
    state: &DaemonState,
    config: &AppConfig,
    mode: AgentMode,
    session_id: &str,
) {
    if !config.tools.enabled {
        return;
    }
    // 平台会话:把正在跑的那一轮的上下文取回来注册平台工具。claude-code
    // 供应商忽略请求里的 tools,全靠这条 MCP 桥——不挂上去,群管理/撤回/
    // 艾特/发送在群聊里整套消失(08-26 用户点名)。权限判定同源:用的就是
    // 主线回合那个上下文。
    //
    // 这里必须与 turns/task.rs 的平台回合底座一致(08-26 审查抓到:桥原来
    // 拿的是 owner 面全量 registry,非管理员群友经由桥就能调 run_command、
    // claude_code——§09 owner-only 被绕过)。收口在 apply_platform_turn_scope。
    if let Some(platform) = crate::platforms::live_turn_context(session_id) {
        crate::platforms::apply_platform_turn_scope(
            registry,
            config,
            &state.paths,
            &platform,
            None,
        );
        if !config.platforms.qq.memory.write_enabled {
            registry.unregister("remember_fact");
        }
        // 看图与生图的作用域也要在这条路上装一遍(08-26 用户点名"图我看不了")。
        // 真实回合是在 agent/input.rs 准备输入时注册的,桥另建工具面走不到那
        // 里:于是 claude-code 供应商拿到的工具面里根本没有 vision_analyze,
        // 群上下文里的图只给了 id 却没有任何手段去看——Miyu 说看不了是实话。
        //
        // 顺带堵上同一处的另一个洞:受限底座注册的是**不受限**的
        // generate_image,它的参考图解析器能吃宿主任意路径;
        // register_scoped_platform 会用作用域版本把它换掉。非管理员的
        // allow_general_access 为假,只认已入库的 context_image_N。
        if config.tools.enabled
            && (config.plugins.vision.enabled || config.plugins.image_generation.enabled)
        {
            let context_images = platform.context_images();
            crate::tools::vision::register_scoped_platform(
                registry,
                config.clone(),
                state.paths.clone(),
                Vec::new(),
                context_images,
                platform.clone(),
            );
        }
        crate::platforms::register_platform_tools(registry, platform);
        return;
    }
    crate::tools::register_ask_question(registry);
    // artifact / 分享是 WebUI 专有:预览文件要网页端才渲染得出来。REPL 里
    // 拿到它,模型就会把文档"扔进 artifact"——那地方你根本看不见
    // (08-29 用户实测,东京攻略写进了 data/artifacts 没人知道)。
    //
    // 主线回合按 `is_local_webui_request` 收口(turns/task.rs:224),桥这条路
    // 原来只看 mode,注释还写着"与 task.rs 相同的条件",其实少了一半。
    // claude-code 的工具**只能**从 MCP 桥拿,于是 REPL 里照样拿到——
    // 08-26 那次"群友经由桥能调 run_command"是同一个坑的另一半。
    if mode == AgentMode::Normal && session_is_running_local_webui(state, session_id) {
        crate::tools::register_webui_artifact_tools(registry, &state.paths, session_id);
        crate::tools::register_webui_share_tools(registry, config, state.state_store.clone());
    }
}

/// 本会话正在跑的回合是不是 WebUI 面。判据与主线回合同源——同一个
/// `is_local_webui_request`,只是 audience 与 profile 改从 `RunInfo` 上读,
/// 桥没有别的途径知道自己在为谁服务。
///
/// 没有在跑的回合就判否:桥也可能来自 `miyu tool-call` 这类回合外调用,
/// 那时宁可少给(与 08-26 收口同一取向)。
fn session_is_running_local_webui(state: &DaemonState, session_id: &str) -> bool {
    let manager = state.manager.lock().unwrap();
    let local =
        |info: &RunInfo| is_local_webui_request(info.audience, info.platform_followup.is_some());
    let mut runs = manager
        .active_runs
        .values()
        .filter(|info| &*info.session_id == session_id);
    runs.next()
        .is_some_and(|first| local(first) && runs.all(local))
}

pub(in crate::web) fn session_api_error(message: String) -> ApiError {
    ApiError::new(StatusCode::BAD_REQUEST, message)
}

pub(in crate::web) fn require_local_web_session(
    state: &DaemonState,
    session_id: &str,
) -> std::result::Result<crate::state::SessionRecord, ApiError> {
    let record = state
        .state_store
        .session_record(session_id)
        .map_err(ApiError::internal)?;
    let is_platform = state
        .state_store
        .is_platform_session(session_id)
        .map_err(ApiError::internal)?;
    match record {
        // dev 会话(保留人格)对 WebUI 可见:侧栏分组列它,打开/改名/删除
        // 也得放行,否则点进去 404「会话不存在」(验收三轮)。
        Some(record)
            if !is_platform
                && record.kind == "user"
                && (record.persona == active_persona_scope(state)
                    || record.persona == crate::state::DEV_PERSONA) =>
        {
            Ok(record)
        }
        _ => Err(ApiError::new(StatusCode::NOT_FOUND, "session not found")),
    }
}
