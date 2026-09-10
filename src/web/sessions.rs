//! 会话的增删改查与解析。
//!
//! 「会话引用」不等于会话 ID：前端可以传 ID、也可以传 `current` 这类别名，
//! 还要按 kind 过滤（有些接口只接受能承载回合的会话）。`resolve_local_session_ref*`
//! 这一族就是把这些形态归一到一个真实会话上，失败时给出前端能理解的错误。
//!
//! 自动命名（`maybe_auto_name_session`）放在这里而不是回合模块：它是会话的属
//! 性变更，只是恰好由第一条消息触发。

use crate::web::*;

pub(in crate::web) async fn list_sessions_http(
    State(state): State<DaemonState>,
    headers: HeaderMap,
) -> std::result::Result<Response, ApiError> {
    let identity = require_identity(&headers, &state)?;
    let persona = active_persona_scope(&state);
    // 侧栏按模式分组:普通+dev 一起下发,mode 字段区分(问题七)。
    // 归属(阶段 5):各人只看自己名下的;管理员名下 = 遗留 + 终端 + 自己建的。
    let store = state
        .stores
        .for_identity(&identity)
        .map_err(ApiError::internal)?;
    let sessions =
        sessions_with_dev(&store, &persona, identity.owner_key()).map_err(ApiError::internal)?;
    let current = current_session_for(&state, &identity, &sessions);
    let sessions = sessions
        .iter()
        .map(|overview| session_overview_json(overview, &current))
        .collect::<Vec<_>>();
    let data = json!({ "current": current, "sessions": sessions });
    Ok(Json(data).into_response())
}

/// 「当前会话」:管理员是 daemon 的全局指针(与 REPL 共用);成员没有全局
/// 指针,拿名下最近活跃的一条。
pub(in crate::web) fn current_session_for(
    state: &DaemonState,
    identity: &WebIdentity,
    sessions: &[crate::state::SessionOverview],
) -> String {
    if identity.admin {
        return state.state_store.session_id().to_string();
    }
    sessions
        .iter()
        .max_by_key(|overview| overview.record.updated_at.clone())
        .map(|overview| overview.record.session_id.clone())
        .unwrap_or_default()
}

/// 成员的当前会话 id:没有就建一条(自动按第一句话命名)。
pub(in crate::web) fn member_current_session(
    state: &DaemonState,
    owner: &str,
) -> std::result::Result<String, String> {
    let store = state
        .stores
        .for_owner(owner)
        .map_err(|error| safe_error_message(&error))?;
    let sessions = store
        .list_owner_sessions(owner)
        .map_err(|error| safe_error_message(&error))?;
    if let Some(overview) = sessions
        .iter()
        .max_by_key(|overview| overview.record.updated_at.clone())
    {
        return Ok(overview.record.session_id.clone());
    }
    let persona = member_session_persona(state, owner);
    let record = store
        .create_session_for_owner(&persona, "", crate::state::USER_SESSION_KIND, None, owner)
        .map_err(|error| safe_error_message(&error))?;
    state.stores.note_session_owner(&record.session_id, owner);
    publish_session_created(state, &record);
    Ok(record.session_id)
}

/// 成员新会话挂哪个人格:settings 里指着自己的私有人格就用它的 scope,否则共享 Miyu。
pub(in crate::web) fn member_session_persona(state: &DaemonState, owner: &str) -> String {
    let username = state
        .state_store
        .account_by_id(owner)
        .ok()
        .flatten()
        .map(|account| account.username);
    if let Some(username) = username {
        if let Some(persona) = member_persona::active_persona(&state.paths, &username) {
            return persona.scope();
        }
    }
    active_persona_scope(state)
}

pub(in crate::web) fn publish_session_created(
    state: &DaemonState,
    record: &crate::state::SessionRecord,
) {
    state.events.publish(
        "session.created",
        json!({
            "session_id": record.session_id,
            "name": record.name,
            "mode": session_mode_label(record),
        }),
    );
}

#[derive(Deserialize)]
pub(in crate::web) struct CreateSessionRequest {
    #[serde(default)]
    pub(in crate::web) name: Option<String>,
    #[serde(default)]
    pub(in crate::web) switch: bool,
    /// "dev" 建 Build 模式会话(保留人格 dev);缺省=当前人格普通会话。
    #[serde(default)]
    pub(in crate::web) mode: Option<String>,
}

#[derive(Deserialize)]
pub(in crate::web) struct ResetConversationRequest {
    pub(in crate::web) session_id: Option<String>,
}

pub(in crate::web) async fn create_session_http(
    State(state): State<DaemonState>,
    headers: HeaderMap,
    Json(request): Json<CreateSessionRequest>,
) -> std::result::Result<Response, ApiError> {
    require_mutation(&headers, &state)?;
    let identity = require_identity(&headers, &state)?;
    if !identity.admin {
        // 成员的会话归成员名下,不动全局指针;dev 会话(run_command 等
        // 属主工具)只有管理员能开。
        if request.mode.as_deref() == Some("dev") {
            return Err(ApiError::new(
                StatusCode::FORBIDDEN,
                "dev sessions are admin only",
            ));
        }
        let name = request
            .name
            .map(|name| name.trim().to_string())
            .unwrap_or_default();
        let record = state
            .stores
            .for_identity(&identity)
            .map_err(ApiError::internal)?
            .create_session_for_owner(
                &member_session_persona(&state, identity.owner_key()),
                &name,
                crate::state::USER_SESSION_KIND,
                None,
                identity.owner_key(),
            )
            .map_err(ApiError::internal)?;
        state
            .stores
            .note_session_owner(&record.session_id, identity.owner_key());
        publish_session_created(&state, &record);
        let data = json!({ "session": session_record_json(&record) });
        return Ok((StatusCode::CREATED, Json(data)).into_response());
    }
    let data = handle_session_command(
        &state,
        IpcCommand::CreateSession {
            name: request.name,
            switch: request.switch,
            kind: None,
            mode: request.mode,
        },
    )
    .await
    .map_err(session_api_error)?;
    Ok((StatusCode::CREATED, Json(data)).into_response())
}

#[derive(Deserialize)]
pub(in crate::web) struct UpdateSessionRequest {
    #[serde(default)]
    pub(in crate::web) name: Option<String>,
    /// `Some("")` unbinds the workspace; a non-empty value binds it.
    #[serde(default)]
    pub(in crate::web) workspace: Option<String>,
}

#[derive(Deserialize)]
pub(in crate::web) struct ReorderSessionsRequest {
    pub(in crate::web) session_ids: Vec<String>,
}

/// 侧栏拖拽排序:按给定顺序重写会话展示序。
pub(in crate::web) async fn reorder_sessions_http(
    State(state): State<DaemonState>,
    headers: HeaderMap,
    Json(request): Json<ReorderSessionsRequest>,
) -> std::result::Result<Response, ApiError> {
    require_mutation(&headers, &state)?;
    for session_id in &request.session_ids {
        require_local_web_session(&state, &headers, session_id)?;
    }
    let data = handle_session_command(
        &state,
        IpcCommand::ReorderSessions {
            session_ids: request.session_ids,
        },
    )
    .await
    .map_err(session_api_error)?;
    Ok(Json(data).into_response())
}

pub(in crate::web) async fn update_session_http(
    State(state): State<DaemonState>,
    headers: HeaderMap,
    Path(session_id): Path<String>,
    Json(request): Json<UpdateSessionRequest>,
) -> std::result::Result<Response, ApiError> {
    require_mutation(&headers, &state)?;
    require_local_web_session(&state, &headers, &session_id)?;
    let target = || ipc::SessionRef::Id {
        id: session_id.clone(),
    };
    if let Some(name) = request.name {
        handle_session_command(
            &state,
            IpcCommand::RenameSession {
                target: target(),
                name,
            },
        )
        .await
        .map_err(session_api_error)?;
    }
    if let Some(workspace) = request.workspace {
        let path = (!workspace.trim().is_empty()).then(|| std::path::PathBuf::from(workspace));
        handle_session_command(
            &state,
            IpcCommand::SetWorkspace {
                target: target(),
                path,
            },
        )
        .await
        .map_err(session_api_error)?;
    }
    Ok(Json(json!({})).into_response())
}

/// 某个会话当前的待办清单。
///
/// WebUI 侧边有一块常驻面板显示它。工具事件只在 `todowrite` 跑的那一刻发生
/// 一次，刷新页面或切回来就没了；这个接口让面板每次进会话都能拿到当前状态。
pub(in crate::web) async fn session_todos_http(
    State(state): State<DaemonState>,
    headers: HeaderMap,
    Path(session_id): Path<String>,
) -> std::result::Result<Json<Value>, ApiError> {
    require_auth(&headers, &state)?;
    require_local_web_session(&state, &headers, &session_id)?;
    let todos = tools::session_todos(&state.paths, &session_id);
    Ok(Json(json!({ "todos": todos })))
}

/// Read-only snapshot of one session's conversation for per-view browsing:
/// turns, queued follow-ups, and its currently running turns. Does not touch
/// the global current-session pointer.
pub(in crate::web) async fn session_turns_http(
    State(state): State<DaemonState>,
    headers: HeaderMap,
    Path(session_id): Path<String>,
) -> std::result::Result<Response, ApiError> {
    require_auth(&headers, &state)?;
    require_local_web_session(&state, &headers, &session_id)?;
    let store = state.stores.for_session(&session_id).pinned(&session_id);
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
    let turns: Vec<SafeTurn> = store
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
    let running_target = store
        .running_turn_queue_target()
        .map_err(ApiError::internal)?;
    let queued_prompts: Vec<SafeQueuedPrompt> = match running_target.as_ref() {
        Some(target) => store
            .load_queued_prompts_for_target(target)
            .map_err(ApiError::internal)?,
        None => Vec::new(),
    }
    .into_iter()
    .map(SafeQueuedPrompt::from)
    .collect();
    let runs: Vec<Value> = state
        .manager
        .lock()
        .unwrap()
        .active_runs
        .iter()
        .filter(|(_, info)| &*info.session_id == session_id.as_str())
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
    let redo_candidate = if runs.is_empty() {
        store
            .redo_candidate()
            .map_err(ApiError::internal)?
            .map(SafeRedoCandidate::from)
    } else {
        None
    };
    let mut response = Json(json!({
        "session_id": session_id,
        "turns": turns,
        "queued_prompts": queued_prompts,
        "running_turn_id": running_target.as_ref().map(|target| target.turn_id.as_str()),
        "runs": runs,
        "redo_candidate": redo_candidate,
    }))
    .into_response();
    response
        .headers_mut()
        .insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    Ok(response)
}

pub(in crate::web) async fn delete_session_http(
    State(state): State<DaemonState>,
    headers: HeaderMap,
    Path(session_id): Path<String>,
) -> std::result::Result<Response, ApiError> {
    require_mutation(&headers, &state)?;
    require_local_web_session(&state, &headers, &session_id)?;
    // 删的是侧栏里最后一个会话时,顶替的新会话由这里建,不留给客户端:
    // 每个开着这个会话的页面(桌面 + 手机、两个标签页)都会收到
    // session.deleted 并各自兜底新建,删一个凭空多出两个(09-10 复现)。
    // 先建后删,`session.created` 就排在 `session.deleted` 前面到达,
    // 其它客户端兜底时列表里已经有它,不会再自己 POST 一个。
    let identity = require_identity(&headers, &state)?;
    let replacement = replacement_for_last_session(&state, &identity, &session_id)?;
    let deleted = handle_session_command(
        &state,
        IpcCommand::DeleteSession {
            target: ipc::SessionRef::Id { id: session_id },
        },
    )
    .await;
    if let Err(error) = deleted {
        if let Some(record) = &replacement {
            // 没删成就不该多出一个空会话;删不掉也只是留个空壳,不遮原错误。
            let _ = state
                .stores
                .for_session(&record.session_id)
                .delete_session(&record.session_id);
            state.events.publish(
                "session.deleted",
                json!({ "session_id": record.session_id }),
            );
        }
        return Err(session_api_error(error));
    }
    Ok(Json(json!({ "fallback": replacement.as_ref().map(session_record_json) })).into_response())
}

/// `session_id` 是不是侧栏里最后一个会话(普通 + dev 分组,终端集成会话不算);
/// 是的话先建一个空会话顶上,并广播 `session.created`。
fn replacement_for_last_session(
    state: &DaemonState,
    identity: &WebIdentity,
    session_id: &str,
) -> std::result::Result<Option<crate::state::SessionRecord>, ApiError> {
    let persona = active_persona_scope(state);
    let own_store = state
        .stores
        .for_identity(identity)
        .map_err(ApiError::internal)?;
    let others_remain = sessions_with_dev(&own_store, &persona, identity.owner_key())
        .map_err(ApiError::internal)?
        .iter()
        .any(|overview| {
            let id = overview.record.session_id.as_str();
            id != session_id && id != crate::state::DEFAULT_SESSION_ID
        });
    if others_remain {
        return Ok(None);
    }
    // 只在被删的确实是个可见会话时才顶替:id 打错了直接让删除那步报 404。
    let exists = own_store
        .session_record(session_id)
        .map_err(ApiError::internal)?
        .is_some();
    if !exists {
        return Ok(None);
    }
    // 成员的顶替会话归成员、挂他当前的人格。
    let persona = if identity.admin {
        persona
    } else {
        member_session_persona(state, identity.owner_key())
    };
    let record = own_store
        .create_session_for_owner(
            &persona,
            "",
            crate::state::USER_SESSION_KIND,
            None,
            identity.owner_key(),
        )
        .map_err(ApiError::internal)?;
    state
        .stores
        .note_session_owner(&record.session_id, identity.owner_key());
    publish_session_created(state, &record);
    Ok(Some(record))
}

pub(in crate::web) fn resolve_local_session_ref(
    state: &DaemonState,
    target: &ipc::SessionRef,
) -> std::result::Result<crate::state::SessionRecord, String> {
    resolve_local_session_ref_with_kinds(state, target, &[crate::state::USER_SESSION_KIND], None)
}

/// Same, but for the two callers that must also reach one-shot `ask` sessions
/// (running their turn, then deleting them). `SessionRef::Name` still cannot
/// find those — the DB lookup filters to user sessions — so only the client
/// holding the freshly minted id can address one.
/// `owner`(阶段 5):Some(归属键) 时只放行该账号名下的会话——HTTP 路径
/// 一律带;IPC/工具桥/测试传 None(终端就是管理员,不再另查)。
pub(in crate::web) fn resolve_local_session_ref_with_kinds(
    state: &DaemonState,
    target: &ipc::SessionRef,
    kinds: &[&str],
    owner: Option<&str>,
) -> std::result::Result<crate::state::SessionRecord, String> {
    // 归属键给了就用那个人的库(成员自己一份);IPC/桥(None)按 id 找会话在
    // 谁的库里(HTTP 路径已经用身份验过归属才走到这),其余 = 管理员库。
    let store = match owner {
        Some(owner) if !owner.is_empty() => state
            .stores
            .for_owner(owner)
            .map_err(|error| safe_error_message(&error))?,
        _ => match target {
            ipc::SessionRef::Id { id } => state.stores.for_session(id),
            _ => state.state_store.clone(),
        },
    };
    let store = &store;
    let persona = active_persona_scope(state);
    let record = match target {
        ipc::SessionRef::Current => match owner {
            Some(owner) if !owner.is_empty() => {
                let id = member_current_session(state, owner)?;
                store
                    .session_record(&id)
                    .map_err(|error| safe_error_message(&error))?
            }
            _ => store
                .session_record(&store.session_id())
                .map_err(|error| safe_error_message(&error))?,
        },
        ipc::SessionRef::Id { id } => store
            .session_record(id)
            .map_err(|error| safe_error_message(&error))?,
        ipc::SessionRef::Name { name } => store
            .find_local_session_by_name(&persona, name)
            .map_err(|error| safe_error_message(&error))?,
    };
    let Some(record) = record else {
        return Err(t("session not found", "找不到该会话").to_string());
    };
    let is_platform = store
        .is_platform_session(&record.session_id)
        .map_err(|error| safe_error_message(&error))?;
    // 人格过滤只约束按名寻址与当前指针:显式 id 是不可猜测的能力凭据,
    // 且 dev 会话(保留人格 "dev")必须能被 dev REPL 按 id 操作——否则
    // 起回合/切换/指针全部 404(验收问题二:dev 首启即被踢回默认会话)。
    // 成员的会话可能挂在私有人格上:归属对得上就不看人格。
    let member_owned = owner.is_some_and(|owner| !owner.is_empty() && record.owner == owner);
    let persona_ok = record.persona == persona
        || record.persona == crate::state::DEV_PERSONA
        || matches!(target, ipc::SessionRef::Id { .. })
        || member_owned;
    let owner_ok = owner.is_none_or(|owner| record.owner == owner);
    if !persona_ok || !owner_ok || !kinds.contains(&record.kind.as_str()) || is_platform {
        return Err(t("session not found", "找不到该会话").to_string());
    }
    Ok(record)
}

/// 工具桥专用的会话寻址:在本地会话之外**额外**放行"正有回合在跑"的平台
/// 会话。MCP 桥(claude-code 供应商唯一的工具通道)带的就是平台会话 id,被
/// 本地解析一律挡掉时,群聊里整套平台工具都调不到(08-26 实测 `tool-call
/// --list` 报"找不到该会话")。放行窗口卡在活回合上:回合结束登记即注销,
/// 桥也随之失去这条会话的寻址能力。
pub(in crate::web) fn resolve_tool_bridge_session_ref(
    state: &DaemonState,
    target: &ipc::SessionRef,
) -> std::result::Result<crate::state::SessionRecord, String> {
    match resolve_local_session_ref_with_kinds(state, target, TURN_TARGET_KINDS, None) {
        Ok(record) => Ok(record),
        Err(error) => {
            let ipc::SessionRef::Id { id } = target else {
                return Err(error);
            };
            if crate::platforms::live_turn_context(id).is_none() {
                return Err(error);
            }
            state
                .state_store
                .session_record(id)
                .map_err(|error| safe_error_message(&error))?
                .ok_or(error)
        }
    }
}

pub(in crate::web) fn resolve_available_local_session_ref(
    state: &DaemonState,
    target: &ipc::SessionRef,
) -> std::result::Result<crate::state::SessionRecord, String> {
    resolve_local_session_ref(state, target)
}

/// Turn targets and deletions additionally accept one-shot `ask` sessions.
pub(in crate::web) const TURN_TARGET_KINDS: &[&str] = &[
    crate::state::USER_SESSION_KIND,
    crate::state::ASK_SESSION_KIND,
    crate::state::VOICE_SESSION_KIND,
];

/// Most recently updated other user session, or a fresh default session when
/// none is left.
pub(in crate::web) fn fallback_session_id(
    state: &DaemonState,
    exclude: &str,
) -> std::result::Result<String, String> {
    let persona = active_persona_scope(state);
    // 全局指针只在管理员名下的会话里挪,不能落到成员的会话上。
    let sessions = state
        .state_store
        .list_local_sessions_for_owner(&persona, "")
        .map_err(|error| safe_error_message(&error))?;
    if let Some(overview) = sessions
        .iter()
        .find(|overview| overview.record.session_id != exclude)
    {
        return Ok(overview.record.session_id.clone());
    }
    let record = state
        .state_store
        .create_session(
            &persona,
            t("Terminal session", "终端集成会话"),
            "user",
            None,
        )
        .map_err(|error| safe_error_message(&error))?;
    state.events.publish(
        "session.created",
        json!({ "session_id": record.session_id, "name": record.name }),
    );
    Ok(record.session_id)
}

/// 普通人格 + dev 保留人格的本地会话合并,按更新时间排。WebUI 侧栏与
/// `miyu session` 管理面共用:mode 字段(session_record_json)区分分组。
pub(in crate::web) fn sessions_with_dev(
    store: &StateStore,
    persona: &str,
    owner: &str,
) -> anyhow::Result<Vec<crate::state::SessionOverview>> {
    // 成员(owner 非空)名下不分人格:他的会话可能挂在自己的私有人格上。
    let mut rows = if owner.is_empty() {
        store.list_local_sessions_for_owner(persona, owner)?
    } else {
        store.list_owner_sessions(owner)?
    };
    if owner.is_empty() && persona != crate::state::DEV_PERSONA {
        rows.extend(store.list_local_sessions_for_owner(crate::state::DEV_PERSONA, owner)?);
    }
    // 手动排序键优先(v28,越小越靠前);同键退回最近活跃。
    rows.sort_by(|a, b| {
        a.record
            .sort_key
            .cmp(&b.record.sort_key)
            .then_with(|| b.record.updated_at.cmp(&a.record.updated_at))
    });
    Ok(rows)
}

/// 会话模式由人格推导（创建时定死）。
///
/// 单独一个函数是因为它有两个发布口——REST 的会话对象和 `session.created`
/// 事件——而前端两条路都要用它分组。之前只有 REST 那份带上了，事件那份漏了，
/// 结果新建的 dev 会话在刷新之前一直显示在「普通模式」组里。
pub(in crate::web) fn session_mode_label(record: &crate::state::SessionRecord) -> &'static str {
    if record.persona == crate::state::DEV_PERSONA {
        "dev"
    } else {
        "normal"
    }
}

pub(in crate::web) fn session_record_json(record: &crate::state::SessionRecord) -> Value {
    json!({
        "session_id": record.session_id,
        "name": record.name,
        "kind": record.kind,
        "workspace": record.workspace,
        "created_at": record.created_at,
        "updated_at": record.updated_at,
        "mode": session_mode_label(record),
    })
}

pub(in crate::web) fn session_overview_json(
    overview: &crate::state::SessionOverview,
    current: &str,
) -> Value {
    let mut value = session_record_json(&overview.record);
    value["turn_count"] = json!(overview.turn_count);
    value["last_user_content"] = json!(overview.last_user_content);
    value["is_current"] = json!(overview.record.session_id == current);
    value
}

/// Resolves an optional turn-target session id: validates existence and that
/// it is a user or one-shot session; `None` falls back to the global current
/// session.
/// 会话模式创建时定死:dev 人格(DEV_PERSONA)会话永远 Dev,其余永远
/// Normal——客户端传什么都不构成中途切换路径。
pub(in crate::web) fn turn_mode_for_session(
    store: &StateStore,
    session_id: &str,
    requested: AgentMode,
) -> AgentMode {
    match store.session_record(session_id) {
        Ok(Some(record)) if record.persona == crate::state::DEV_PERSONA => AgentMode::Dev,
        _ => {
            if requested == AgentMode::Dev {
                tracing::debug!(%session_id, "client asked for dev mode on a non-dev session; forcing normal");
            }
            AgentMode::Normal
        }
    }
}

/// `owner` 同 [`resolve_local_session_ref_with_kinds`]:HTTP 路径传登录者的
/// 归属键,IPC 传 None。没给会话 id 时,管理员/IPC 落到全局当前会话,成员落到
/// 自己名下最近的一条(没有就建)。
pub(in crate::web) fn resolve_turn_session(
    state: &DaemonState,
    owner: Option<&str>,
    session_id: Option<String>,
) -> std::result::Result<Arc<str>, String> {
    match session_id {
        None => match owner {
            Some(owner) if !owner.is_empty() => Ok(member_current_session(state, owner)?.into()),
            _ => Ok(state.state_store.session_id()),
        },
        Some(session_id) => {
            let record = resolve_local_session_ref_with_kinds(
                state,
                &ipc::SessionRef::Id { id: session_id },
                TURN_TARGET_KINDS,
                owner,
            )?;
            Ok(record.session_id.into())
        }
    }
}

pub(in crate::web) async fn reset_conversation(
    State(state): State<DaemonState>,
    headers: HeaderMap,
    Json(request): Json<ResetConversationRequest>,
) -> std::result::Result<StatusCode, ApiError> {
    require_mutation(&headers, &state)?;
    let identity = require_identity(&headers, &state)?;
    let session_id = match request.session_id {
        Some(session_id) => session_id,
        None => resolve_turn_session(&state, Some(identity.owner_key()), None)
            .map_err(session_api_error)?
            .to_string(),
    };
    require_local_web_session(&state, &headers, &session_id)?;
    let store = state.stores.for_session(&session_id).pinned(&session_id);
    if store.has_running_turns().map_err(ApiError::internal)? {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "a conversation turn is already running",
        ));
    }
    reserve_admin_for_session(&state.manager, &session_id)?;
    let (reply, receiver) = oneshot::channel();
    if state
        .actor_tx
        .send(ActorCommand::ResetConversation {
            session_id: session_id.into(),
            reply,
        })
        .is_err()
    {
        release_admin(&state.manager);
        return Err(ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "agent worker is unavailable",
        ));
    }
    match receiver.await {
        Ok(Ok(())) => Ok(StatusCode::NO_CONTENT),
        Ok(Err(AdminFailure::Invalid(message))) => {
            Err(ApiError::new(StatusCode::CONFLICT, message))
        }
        Ok(Err(AdminFailure::Internal(message))) => {
            tracing::error!(
                error = %message,
                "{}",
                t("WebUI conversation reset failed", "WebUI 对话重置失败")
            );
            Err(ApiError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                safe_error_message(&message),
            ))
        }
        Err(_) => {
            release_admin(&state.manager);
            Err(ApiError::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "agent worker stopped before resetting the conversation",
            ))
        }
    }
}

/// Best-effort AI pass over the truncated default session name: ask the
/// main model pool for a concise title and apply it only if the
/// auto-generated name is still in place (a user rename wins). Runs
/// detached on the actor's LocalSet — never blocks the turn.
pub(in crate::web) fn spawn_session_title_refinement(
    config: &AppConfig,
    paths: &MiyuPaths,
    store: &StateStore,
    events: &EventHub,
    fallback: String,
    seed: &str,
) {
    // 标题走 model_tiers.roles.session_title 指定的档位池(未配置=主池):
    // 一条 16 字标题不值一次旗舰调用。
    let Ok(client) =
        OpenAiCompatibleClient::from_aux_role(config, paths, crate::config::AuxRole::SessionTitle)
    else {
        return;
    };
    // 标题生成是侧信道:scope 留在默认 "chat" 会让缓存记账把它算进主对话
    // (08-10 调研 P2),claude-code 中转还会为它建持久会话且不进会话映射
    // (清空联动删不到)。
    let client = client.with_request_scope("session-title");
    let store = store.clone();
    let events = events.clone();
    let seed = seed.to_string();
    tokio::task::spawn_local(async move {
        let session_id = store.session_id();
        let prompt = format!(
            "为下面这条用户消息生成一个简洁的会话标题：不超过 16 个字，概括主题，只输出标题本身，不要引号、句号或解释。

用户消息：{seed}"
        );
        let result = client
            .chat_stream(
                vec![
                    crate::llm::ChatMessage::system("你是会话标题生成器，只输出标题本身。"),
                    crate::llm::ChatMessage::plain("user", prompt),
                ],
                Vec::new(),
                |_| Ok(()),
            )
            .await;
        let Ok(result) = result else { return };
        let title = sanitize_session_title(&result.content);
        if title.is_empty() {
            return;
        }
        let Ok(Some(record)) = store.session_record(&session_id) else {
            return;
        };
        if record.name != fallback {
            return;
        }
        if store.rename_session(&record.session_id, &title).is_ok() {
            events.publish(
                "session.renamed",
                json!({ "session_id": record.session_id, "name": title }),
            );
        }
        if let Some(usage) = result.usage.as_ref() {
            let meta = crate::state::UsageMeta {
                source: "agent",
                provider: result.provider_id.as_deref(),
                model: result.model.as_deref(),
                kind: None,
            };
            let _ = store.add_auxiliary_usage(usage, meta);
        }
    });
}

/// Cleans an LLM-generated title down to a single short line: first line
/// only, surrounding quotes/punctuation stripped, clipped to 20 chars.
pub(in crate::web) fn sanitize_session_title(raw: &str) -> String {
    let cleaned = raw
        .trim()
        .lines()
        .next()
        .unwrap_or("")
        .trim()
        .trim_matches(|c: char| {
            matches!(
                c,
                '"' | '\''
                    | '“'
                    | '”'
                    | '‘'
                    | '’'
                    | '「'
                    | '」'
                    | '《'
                    | '》'
                    | '。'
                    | '.'
                    | '，'
                    | ','
            )
        })
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    cleaned.chars().take(20).collect()
}

#[allow(clippy::too_many_arguments)]
pub(in crate::web) fn session_for_persona(
    state_store: &StateStore,
    manager: &Arc<Mutex<ManagerState>>,
    persona: &str,
) -> Result<String> {
    if let Some(session_id) = state_store.persona_current_session(persona)? {
        if is_available_local_session(state_store, &session_id, persona)? {
            return Ok(session_id);
        }
    }
    let remembered = manager
        .lock()
        .unwrap()
        .persona_session_ids
        .get(persona)
        .cloned();
    if let Some(session_id) = remembered {
        if is_available_local_session(state_store, &session_id, persona)? {
            return Ok(session_id);
        }
    }
    if let Some(overview) = state_store
        .list_local_sessions_for_owner(persona, "")?
        .into_iter()
        .next()
    {
        return Ok(overview.record.session_id);
    }
    Ok(state_store
        .create_session(persona, "", "user", None)?
        .session_id)
}

/// Auto-names a still-unnamed session from its first prompt once a turn has
/// run in it. Explicit names (given at creation or via rename) are never
/// overwritten.
pub(in crate::web) fn maybe_auto_name_session(
    state_store: &StateStore,
    events: &EventHub,
    seed: &str,
) -> Option<String> {
    let session_id = state_store.session_id();
    let record = state_store.session_record(&session_id).ok().flatten()?;
    if !record.name.trim().is_empty() {
        return None;
    }
    let title = session_title_from_prompt(seed);
    if title.is_empty() {
        return None;
    }
    if state_store
        .rename_session(&record.session_id, &title)
        .is_ok()
    {
        events.publish(
            "session.renamed",
            json!({ "session_id": record.session_id, "name": title }),
        );
        return Some(title);
    }
    None
}

pub(in crate::web) fn session_title_from_prompt(prompt: &str) -> String {
    let cleaned = prompt
        .trim()
        .lines()
        .next()
        .unwrap_or("")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let mut title: String = cleaned.chars().take(20).collect();
    if cleaned.chars().count() > 20 {
        title.push('…');
    }
    title
}

/// 把目标会话钉的模型池套到 `config` 上。回合路与压缩路共用同一条规则，
/// 否则摘要会被路由到全局池里的另一家供应商，拿不到该会话的前缀缓存。
pub(in crate::web) fn apply_session_model_override_to(
    config: &mut AppConfig,
    store: &StateStore,
    session_id: &str,
) {
    match store.session_model_override(session_id) {
        Ok(Some(models)) => config.active_provider_models = Some(models),
        Ok(None) => {}
        Err(error) => tracing::warn!(
            error = %error,
            session_id,
            "{}",
            t(
                "loading the session model override failed",
                "读取会话模型覆盖失败"
            )
        ),
    }
}

pub(in crate::web) fn build_session_agent(
    config: &AppConfig,
    paths: &MiyuPaths,
    state: &StateStore,
    mode: AgentMode,
) -> Result<Agent> {
    crate::models_cache::ensure_active_metadata(paths, config);
    let client = OpenAiCompatibleClient::from_config(config, paths)?;
    let registry = build_tool_registry(config, paths, mode, true)?;
    Ok(
        Agent::new(config.clone(), paths, state.clone(), client, registry, mode)?
            .with_headless_pacing(),
    )
}

pub(in crate::web) fn session_state(
    manager: &Arc<Mutex<ManagerState>>,
    state_store: &StateStore,
) -> Result<ipc::SessionState> {
    let context = manager.lock().unwrap().context;
    let session_id = state_store.session_id();
    let record = state_store.session_record(&session_id)?;
    Ok(ipc::SessionState {
        context_tokens: context.tokens,
        context_window: context.window,
        context_window_assumed: context.window_assumed,
        cumulative_tokens: context.cumulative_tokens,
        cumulative_prompt_tokens: context.cumulative_prompt_tokens,
        cumulative_cache_read_tokens: context.cumulative_cache_read_tokens,
        session_id: session_id.to_string(),
        session_name: record
            .as_ref()
            .map(|record| record.name.clone())
            .unwrap_or_default(),
        workspace: record.and_then(|record| record.workspace),
    })
}

/// 会话上下文占用快照，给输入框角落的上下文条用。
///
/// 切到非当前会话时 `session_state_for` 会冷装配一次该会话的上下文，有成本，
/// 但只在切换那一下发生；之后靠 run 事件携带的增量刷新。没有它，切换会话后
/// 上下文条一直显示上一个会话的数字，直到跑完一轮才纠正。
pub(in crate::web) async fn session_context_http(
    State(state): State<DaemonState>,
    headers: HeaderMap,
    Path(session_id): Path<String>,
) -> std::result::Result<Json<serde_json::Value>, ApiError> {
    require_auth(&headers, &state)?;
    require_local_web_session(&state, &headers, &session_id)?;
    let snapshot = session_state_for(&state, &session_id).map_err(ApiError::internal)?;
    Ok(Json(json!({
        "context_tokens": snapshot.context_tokens,
        "context_window": snapshot.context_window,
        "context_window_assumed": snapshot.context_window_assumed,
    })))
}

pub(in crate::web) fn session_state_for(
    state: &DaemonState,
    session_id: &str,
) -> Result<ipc::SessionState> {
    let session_store = state.stores.for_session(session_id);
    let record = session_store
        .session_record(session_id)?
        .with_context(|| format!("session not found: {session_id}"))?;
    let current_session_id = state.state_store.session_id();
    // 会话钉了模型池就按那个池算窗口。回合路(turns/task.rs)与压缩路
    // (actor)都套了这条覆盖,快照路此前漏了:打开页面、刷新、切换会话时
    // 上下文条显示的都是全局池的窗口,要跑完一轮才被 run.completed 纠正。
    let mut config = state.manager.lock().unwrap().config.clone();
    apply_session_model_override_to(&mut config, &session_store, session_id);
    let mut context = if &*current_session_id == session_id {
        state.manager.lock().unwrap().context
    } else {
        // dev 会话按 dev 装配估算：系统提示词、工具表、记忆钥匙都跟着模式
        // 走，拿 Normal 硬算的话，dev 空会话和普通空会话永远是同一个数。
        let (config, mode) = if record.persona == crate::state::DEV_PERSONA {
            (config.dev_scoped(), AgentMode::Dev)
        } else {
            (config.clone(), AgentMode::Normal)
        };
        let store = session_store.pinned(session_id);
        current_context(&build_session_agent(&config, &state.paths, &store, mode)?)?
    };
    if let Some((window, source)) = config.active_context_window_with_source()? {
        context.window = Some(window);
        context.window_assumed = matches!(source, crate::config::ContextWindowSource::Assumed);
    }
    Ok(ipc::SessionState {
        context_tokens: context.tokens,
        context_window: context.window,
        context_window_assumed: context.window_assumed,
        cumulative_tokens: context.cumulative_tokens,
        cumulative_prompt_tokens: context.cumulative_prompt_tokens,
        cumulative_cache_read_tokens: context.cumulative_cache_read_tokens,
        session_id: record.session_id,
        session_name: record.name,
        workspace: record.workspace,
    })
}

/// Global admin reservation (config/model changes): requires that no turn is
/// running in any session.
pub(in crate::web) fn reserve_admin(
    manager: &Arc<Mutex<ManagerState>>,
) -> std::result::Result<(), ApiError> {
    let mut manager = manager.lock().unwrap();
    if !manager.active_runs.is_empty() || manager.admin_busy {
        return Err(ApiError::new(StatusCode::CONFLICT, ipc::ADMIN_BUSY_MESSAGE));
    }
    manager.admin_busy = true;
    manager.admin_session = None;
    Ok(())
}

/// Per-session admin reservation (reset/undo/pop/compact/delete/archive):
/// only the target session must be idle; turns in other sessions keep
/// running.
pub(in crate::web) fn reserve_admin_for_session(
    manager: &Arc<Mutex<ManagerState>>,
    session_id: &str,
) -> std::result::Result<(), ApiError> {
    let mut manager = manager.lock().unwrap();
    if manager.admin_busy || manager.session_has_runs(session_id) {
        return Err(ApiError::new(StatusCode::CONFLICT, ipc::ADMIN_BUSY_MESSAGE));
    }
    manager.admin_busy = true;
    // 预约限定到这个会话:压缩/pop/undo 重写的是它自己的消息数组,别的
    // 会话该照常开回合。以前这里只置全局位,压一个会话等于停掉整台机器。
    manager.admin_session = Some(session_id.to_string());
    Ok(())
}

/// Light admin reservation (session/model updates): serializes against other
/// admin operations but is allowed while turns are running.
pub(in crate::web) fn reserve_admin_light(
    manager: &Arc<Mutex<ManagerState>>,
) -> std::result::Result<(), ApiError> {
    let mut manager = manager.lock().unwrap();
    if manager.admin_busy {
        return Err(ApiError::new(StatusCode::CONFLICT, ipc::ADMIN_BUSY_MESSAGE));
    }
    manager.admin_busy = true;
    manager.admin_session = None;
    Ok(())
}

pub(in crate::web) fn require_no_running_turn(
    state_store: &StateStore,
) -> std::result::Result<(), ApiError> {
    if state_store
        .has_any_running_turns()
        .map_err(ApiError::internal)?
    {
        Err(ApiError::new(
            StatusCode::CONFLICT,
            "a conversation turn is already running",
        ))
    } else {
        Ok(())
    }
}
