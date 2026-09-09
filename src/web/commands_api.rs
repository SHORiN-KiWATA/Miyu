//! WebUI 的斜杠命令平面。
//!
//! 命令清单来自 `crate::slash_commands` 的同一张表（`GET /api/commands` 按
//! `web` 标记过滤），前端不维护第二份——两份清单迟早分叉。
//!
//! 这里只放**WebUI 里没有别的办法做到**的那几条对应的端点。会话增删改、换模
//! 型、改人格早就有 REST 与 GUI 入口，再给一条命令是两条路做同一件事。

use crate::runtime::*;
use crate::web::*;

/// 一条命令在前端补全菜单里需要的东西。
#[derive(serde::Serialize)]
pub(in crate::web) struct WebCommand {
    name: &'static str,
    arg_hint: &'static str,
    help: &'static str,
}

/// WebUI 输入框认得哪些命令。前端据此渲染 `/` 补全菜单。
///
/// 不在这张表里的 `/xxx` 前端一律当普通消息发给模型——与 REPL 同一语义
/// （见 `slash_commands::parse_repl_input`），两个界面不能分叉。
pub(in crate::web) async fn list_commands(
    State(state): State<DaemonState>,
    headers: HeaderMap,
) -> std::result::Result<Json<Value>, ApiError> {
    require_auth(&headers, &state)?;
    let commands = crate::slash_commands::web_commands()
        .into_iter()
        .map(|spec| WebCommand {
            name: spec.name,
            arg_hint: spec.arg_hint,
            help: spec.help(),
        })
        .collect::<Vec<_>>();
    Ok(Json(json!({ "ok": true, "commands": commands })))
}

/// `/compact`：立即压缩当前会话上下文。
///
/// 请求体与 `/api/conversation/reset` 同形（只有一个可选 session_id），直接
#[derive(Deserialize)]
pub(in crate::web) struct GoalCommandRequest {
    pub(in crate::web) session_id: String,
    #[serde(default)]
    pub(in crate::web) input: String,
}

/// `/goal ...` 的 WebUI 入口。
///
/// 和 REPL 走同一个 `execute_goal_command`——命令的语法、拒绝文案、以及
/// 「armed 只在 daemon 内存里」这条，两个界面必须是同一份实现。
pub(in crate::web) async fn goal_command_http(
    State(state): State<DaemonState>,
    headers: HeaderMap,
    Json(request): Json<GoalCommandRequest>,
) -> std::result::Result<Json<Value>, ApiError> {
    require_mutation(&headers, &state)?;
    require_local_web_session(&state, &request.session_id)?;
    let text = crate::web::apply_goal_command(&state, &request.session_id, &request.input);
    Ok(Json(json!({ "text": text })))
}

/// 会话当前目标的结构化快照。
///
/// 与 `/api/goal` 返回的人类可读文本分开：那条是命令的回执（一段话），
/// 这条是给状态行用的字段，前端要按阶段决定显示哪些按钮。
pub(in crate::web) async fn session_goal_http(
    State(state): State<DaemonState>,
    headers: HeaderMap,
    Path(session_id): Path<String>,
) -> std::result::Result<Json<Value>, ApiError> {
    require_auth(&headers, &state)?;
    require_local_web_session(&state, &session_id)?;
    let goal = crate::tools::goal::session_goal_json(&state.paths, &session_id);
    Ok(Json(goal))
}

/// 复用 `ResetConversationRequest`，不再造一个字段一样的类型。
///
/// 与 `IpcCommand::Compact` 同一条 actor 路径，回合运行中一律拒绝——压缩会重
/// 写消息数组，正在跑的回合手里那份就成了悬空引用。
pub(in crate::web) async fn compact_conversation(
    State(state): State<DaemonState>,
    headers: HeaderMap,
    Json(request): Json<ResetConversationRequest>,
) -> std::result::Result<Json<Value>, ApiError> {
    require_mutation(&headers, &state)?;
    let session_id = request
        .session_id
        .unwrap_or_else(|| state.state_store.session_id().to_string());
    require_local_web_session(&state, &session_id)?;
    let session_id: std::sync::Arc<str> = session_id.into();
    reserve_admin_for_session(&state.manager, &session_id)?;
    let (reply, receiver) = oneshot::channel();
    if state
        .actor_tx
        .send(ActorCommand::Compact {
            session_id: session_id.clone(),
            // WebUI 的压缩进度条挂在**回合气泡**上(`handleContextEvent` 要一个
            // live 气泡),手动压缩没有气泡可挂,所以这里不订流,行为不变。
            events: None,
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
        Ok(Ok(data)) => Ok(Json(json!({ "ok": true, "result": data }))),
        Ok(Err(AdminFailure::Invalid(message))) => {
            Err(ApiError::new(StatusCode::CONFLICT, message))
        }
        Ok(Err(AdminFailure::Internal(message))) => Err(ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            safe_error_message(&message),
        )),
        Err(_) => {
            release_admin(&state.manager);
            Err(ApiError::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "agent worker stopped while compacting the conversation",
            ))
        }
    }
}

#[derive(Deserialize)]
pub(in crate::web) struct PopConversationRequest {
    pub(in crate::web) session_id: Option<String>,
    #[serde(default)]
    pub(in crate::web) count: Option<usize>,
    /// 指定要弹出的轮次（来自弹出选择器的多选）。非空时优先于 `count`。
    #[serde(default)]
    pub(in crate::web) turn_ids: Vec<String>,
}

/// 可弹出轮次清单，给 `/pop` 的多选列表用。
///
/// 判定收口在 `oldest_evictable_visible_turns`——和 REPL 的交互式挑选、
/// 按数量弹出是同一套「哪些轮能弹」的口径，三处分叉的话同一轮在一个入口
/// 能弹、另一个入口说不能。
pub(in crate::web) async fn poppable_turns_http(
    State(state): State<DaemonState>,
    headers: HeaderMap,
    axum::extract::Path(session_id): axum::extract::Path<String>,
) -> std::result::Result<Json<Value>, ApiError> {
    require_auth(&headers, &state)?;
    require_local_web_session(&state, &session_id)?;
    let turns = state
        .state_store
        .pinned(&session_id)
        .oldest_evictable_visible_turns(usize::MAX)
        .map_err(ApiError::internal)?;
    let items = turns
        .into_iter()
        .map(|turn| {
            let preview: String = turn.display_content.chars().take(120).collect();
            json!({
                "turn_id": turn.turn_id,
                "preview": preview,
                "timestamp": turn.user_timestamp,
                "tokens": turn.token_total,
            })
        })
        .collect::<Vec<_>>();
    Ok(Json(json!({ "turns": items })))
}

/// `/pop <count>`：从当前上下文弹出最旧的 N 轮。
///
/// REPL 的无参数形态是交互式挑选，离不开终端；WebUI 对齐的是
/// `miyu pop <count>` 的按数量形态。轮次在这里解析成 id 再交给
/// `ActorCommand::Pop`——actor 侧会拿可弹出集合再过滤一遍，读取和
/// 预订管理锁之间被人抢先弹掉的轮次不会被弹两次。
pub(in crate::web) async fn pop_conversation(
    State(state): State<DaemonState>,
    headers: HeaderMap,
    Json(request): Json<PopConversationRequest>,
) -> std::result::Result<Json<Value>, ApiError> {
    require_mutation(&headers, &state)?;
    let session_id = request
        .session_id
        .unwrap_or_else(|| state.state_store.session_id().to_string());
    require_local_web_session(&state, &session_id)?;
    let turn_ids: Vec<String> = if !request.turn_ids.is_empty() {
        // 选择器多选：具体弹哪些由 actor 再按可弹出集合过滤一遍，这里
        // 不重复校验（读取与执行之间状态可能已变，校验也只是快照）。
        request.turn_ids
    } else {
        let count = request.count.unwrap_or(1);
        if count == 0 {
            return Err(ApiError::new(
                StatusCode::BAD_REQUEST,
                "pop count must be greater than zero",
            ));
        }
        state
            .state_store
            .pinned(&session_id)
            .oldest_evictable_visible_turns(count)
            .map_err(ApiError::internal)?
            .into_iter()
            .map(|turn| turn.turn_id)
            .collect()
    };
    if turn_ids.is_empty() {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "no evictable turns in the active context",
        ));
    }
    let session_id: std::sync::Arc<str> = session_id.into();
    reserve_admin_for_session(&state.manager, &session_id)?;
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
        return Err(ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "agent worker is unavailable",
        ));
    }
    match receiver.await {
        Ok(Ok(data)) => Ok(Json(json!({ "ok": true, "result": data }))),
        Ok(Err(AdminFailure::Invalid(message))) => {
            Err(ApiError::new(StatusCode::CONFLICT, message))
        }
        Ok(Err(AdminFailure::Internal(message))) => Err(ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            safe_error_message(&message),
        )),
        Err(_) => {
            release_admin(&state.manager);
            Err(ApiError::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "agent worker stopped while popping the conversation",
            ))
        }
    }
}

#[derive(serde::Deserialize)]
pub(in crate::web) struct ResetMemoryRequest {
    /// `"dev"` 走开发模式那份记忆；其余（含缺省）是普通模式的。
    #[serde(default)]
    mode: Option<String>,
    /// 会话级重置清哪个会话。缺省用 daemon 的当前指针——但 WebUI 可以同时
    /// 开着好几个会话，那个指针未必是发命令的这一个，所以前端一定要带上。
    #[serde(default)]
    session_id: Option<String>,
}

/// `/reset-memory`：只清这次会话记下的长期记忆。
///
/// dev 的记忆挂在保留人格名下，钥匙必须与 Agent 构造时同一把 `dev_scoped()`，
/// 否则清的是另一份（与 `IpcCommand::ResetMemory` 同一段逻辑）。
pub(in crate::web) async fn reset_memory_http(
    State(state): State<DaemonState>,
    headers: HeaderMap,
    Json(request): Json<ResetMemoryRequest>,
) -> std::result::Result<Json<Value>, ApiError> {
    require_mutation(&headers, &state)?;
    let config = reset_memory_config(&state, request.mode.as_deref());
    let session_id = match request.session_id {
        Some(session_id) if !session_id.trim().is_empty() => session_id,
        _ => state.state_store.session_id().to_string(),
    };
    let summary = crate::memory::MemoryStore::new(&config, &state.paths)
        .reset_session(&session_id)
        .map_err(|error| ApiError::internal(safe_error_message(&error)))?;
    Ok(Json(json!({ "ok": true, "text": summary.describe() })))
}

/// `/reset-all-memory`：清空当前模式的全部长期记忆。
pub(in crate::web) async fn reset_all_memory_http(
    State(state): State<DaemonState>,
    headers: HeaderMap,
    Json(request): Json<ResetMemoryRequest>,
) -> std::result::Result<Json<Value>, ApiError> {
    require_mutation(&headers, &state)?;
    let config = reset_memory_config(&state, request.mode.as_deref());
    crate::memory::MemoryStore::new(&config, &state.paths)
        .reset_all(false)
        .map_err(|error| ApiError::internal(safe_error_message(&error)))?;
    Ok(Json(json!({ "ok": true })))
}

fn reset_memory_config(state: &DaemonState, mode: Option<&str>) -> crate::config::AppConfig {
    let config = state.manager.lock().unwrap().config.clone();
    if mode == Some("dev") {
        config.dev_scoped()
    } else {
        config
    }
}
