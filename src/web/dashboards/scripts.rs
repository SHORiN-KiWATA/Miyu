//! 脚本面板:脚本工具清单 / 头部源码预览 / 禁用·启用·删除 / 补描述注册。
//! 数据与写操作全走 tools::scripts::dashboard,与 manage_script 同一套逻辑。
//!
//! 所有接口都收 `persona`:脚本四层里有两层按人格分(内置人格层、用户人格层),
//! 面板按人格切视角,看到的就是那个人格上线时模型真正拿到的工具集。写操作也
//! 落到那个人格的目录里。

use super::persona_scoped_config;
use crate::config::AppConfig;
use crate::paths::MiyuPaths;
use crate::tools::{
    scripts_dashboard_delete, scripts_dashboard_disable, scripts_dashboard_enable,
    scripts_dashboard_overview, scripts_dashboard_register, scripts_dashboard_source,
};
use crate::web::*;

#[derive(Deserialize)]
pub(in crate::web) struct PersonaQuery {
    #[serde(default)]
    persona: String,
}

#[derive(Deserialize)]
pub(in crate::web) struct SourceQuery {
    #[serde(default)]
    persona: String,
    #[serde(default)]
    id: String,
    #[serde(default)]
    path: String,
    #[serde(default = "default_source_lines")]
    lines: usize,
}

#[derive(Deserialize)]
pub(in crate::web) struct IdQuery {
    #[serde(default)]
    persona: String,
    id: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::web) struct IdBody {
    #[serde(default)]
    persona: String,
    id: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::web) struct RegisterBody {
    #[serde(default)]
    persona: String,
    path: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    id: String,
}

fn default_source_lines() -> usize {
    120
}

fn scoped(
    state: &DaemonState,
    persona: &str,
) -> std::result::Result<(AppConfig, MiyuPaths), ApiError> {
    Ok((persona_scoped_config(state, persona)?, state.paths.clone()))
}

/// 用户输入引起的错误(找不到 id、路径越界、缺描述)原样回 400。
async fn blocking_user<T, F>(work: F) -> std::result::Result<T, ApiError>
where
    T: Send + 'static,
    F: FnOnce() -> anyhow::Result<T> + Send + 'static,
{
    tokio::task::spawn_blocking(work)
        .await
        .map_err(ApiError::internal)?
        .map_err(|error| ApiError::new(StatusCode::BAD_REQUEST, safe_error_message(&error)))
}

/// 有脚本目录或有人格定义的人格清单 + 当前活跃人格。
pub(in crate::web) async fn dash_scripts_personas(
    State(state): State<DaemonState>,
    headers: HeaderMap,
) -> std::result::Result<Json<Value>, ApiError> {
    require_admin(&headers, &state)?;
    let config = state.manager.lock().unwrap().config.clone();
    let active = crate::config::persona_scope_name(&config.prompt.active_persona);
    let mut names = std::collections::BTreeSet::new();
    names.insert(active.clone());
    for directory in [
        state.paths.data_dir.join("personas"),
        state.paths.scripts_dir.join("personas"),
    ] {
        if let Ok(entries) = std::fs::read_dir(directory) {
            for entry in entries.flatten() {
                if entry.path().is_dir() {
                    if let Some(name) = entry.file_name().to_str() {
                        names.insert(name.to_string());
                    }
                }
            }
        }
    }
    Ok(Json(json!({ "active": active, "personas": names })))
}

pub(in crate::web) async fn dash_scripts_overview(
    State(state): State<DaemonState>,
    headers: HeaderMap,
    Query(query): Query<PersonaQuery>,
) -> std::result::Result<Json<Value>, ApiError> {
    require_admin(&headers, &state)?;
    let (config, paths) = scoped(&state, &query.persona)?;
    let overview = tokio::task::spawn_blocking(move || scripts_dashboard_overview(&config, &paths))
        .await
        .map_err(ApiError::internal)?
        .map_err(|error| ApiError::internal(safe_error_message(&error)))?;
    Ok(Json(overview))
}

pub(in crate::web) async fn dash_scripts_source(
    State(state): State<DaemonState>,
    headers: HeaderMap,
    Query(query): Query<SourceQuery>,
) -> std::result::Result<Json<Value>, ApiError> {
    require_admin(&headers, &state)?;
    let (config, paths) = scoped(&state, &query.persona)?;
    let source = blocking_user(move || {
        scripts_dashboard_source(&config, &paths, &query.id, &query.path, query.lines)
    })
    .await?;
    Ok(Json(source))
}

pub(in crate::web) async fn dash_scripts_enable(
    State(state): State<DaemonState>,
    headers: HeaderMap,
    Json(body): Json<IdBody>,
) -> std::result::Result<Json<Value>, ApiError> {
    require_admin_mutation(&headers, &state)?;
    let (config, paths) = scoped(&state, &body.persona)?;
    let result = blocking_user(move || scripts_dashboard_enable(&config, &paths, &body.id)).await?;
    Ok(Json(result))
}

pub(in crate::web) async fn dash_scripts_disable(
    State(state): State<DaemonState>,
    headers: HeaderMap,
    Json(body): Json<IdBody>,
) -> std::result::Result<Json<Value>, ApiError> {
    require_admin_mutation(&headers, &state)?;
    let (config, paths) = scoped(&state, &body.persona)?;
    let result =
        blocking_user(move || scripts_dashboard_disable(&config, &paths, &body.id)).await?;
    Ok(Json(result))
}

pub(in crate::web) async fn dash_scripts_delete(
    State(state): State<DaemonState>,
    headers: HeaderMap,
    Query(query): Query<IdQuery>,
) -> std::result::Result<Json<Value>, ApiError> {
    require_admin_mutation(&headers, &state)?;
    let (config, paths) = scoped(&state, &query.persona)?;
    let result =
        blocking_user(move || scripts_dashboard_delete(&config, &paths, &query.id)).await?;
    Ok(Json(result))
}

pub(in crate::web) async fn dash_scripts_register(
    State(state): State<DaemonState>,
    headers: HeaderMap,
    Json(body): Json<RegisterBody>,
) -> std::result::Result<Json<Value>, ApiError> {
    require_admin_mutation(&headers, &state)?;
    let (config, paths) = scoped(&state, &body.persona)?;
    let result = blocking_user(move || {
        scripts_dashboard_register(&config, &paths, &body.path, &body.description, &body.id)
    })
    .await?;
    Ok(Json(result))
}
