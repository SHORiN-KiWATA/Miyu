//! 供应商模型目录:WebUI「拉取模型列表」与「从目录补全」两个按钮的后端。
//!
//! 入参是**草稿**供应商(还没保存的 base_url / 协议也要能试),密钥留空时借用
//! 当前配置里同 id 供应商的密钥——前端拿到的密钥本来就是掩码。HTTP 供应商
//! 打 `/models`,内置 CLI 供应商问 CLI 要目录(同 TUI),再逐个附上 models.dev
//! 的上下文窗口 / 输入模态 / 价格,前端一键写进表单,不用手填 JSON。

use crate::web::*;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::web) struct ProviderModelsRequest {
    pub(in crate::web) provider: Value,
    /// true = 向供应商拉取目录;false = 只给 `models` 里的名字补目录元数据。
    #[serde(default)]
    pub(in crate::web) fetch: bool,
    #[serde(default)]
    pub(in crate::web) models: Vec<String>,
}

#[derive(Serialize)]
pub(in crate::web) struct ProviderModelsResponse {
    pub(in crate::web) source: &'static str,
    pub(in crate::web) models: Vec<crate::models_cache::ModelCatalogEntry>,
}

pub(in crate::web) async fn provider_models(
    State(state): State<DaemonState>,
    headers: HeaderMap,
    Json(request): Json<ProviderModelsRequest>,
) -> std::result::Result<Json<ProviderModelsResponse>, ApiError> {
    require_auth(&headers, &state)?;
    let mut provider: ProviderConfig =
        serde_json::from_value(request.provider).map_err(|error| {
            ApiError::new(
                StatusCode::BAD_REQUEST,
                format!("invalid provider: {}", safe_error_message(error)),
            )
        })?;
    let (current, paths) = {
        let manager = state.manager.lock().unwrap();
        (manager.config.clone(), state.paths.clone())
    };
    if provider
        .api_key
        .as_deref()
        .map(str::trim)
        .is_none_or(str::is_empty)
    {
        provider.api_key = current
            .providers
            .iter()
            .find(|item| item.id == provider.id)
            .and_then(|item| item.api_key.clone());
    }
    if request.fetch && !provider.is_builtin_cli_provider() && provider.base_url.trim().is_empty() {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "provider base_url is required to fetch models",
        ));
    }
    let fetch = request.fetch;
    let requested = request.models;
    let result = tokio::task::spawn_blocking(move || -> anyhow::Result<ProviderModelsResponse> {
        let (source, ids) = if fetch {
            let cli_binary = crate::config_tui::builtin_cli_binary(&current, &provider);
            let ids = crate::config_tui::fetch_models(&provider, cli_binary.as_deref())?;
            (
                if provider.is_builtin_cli_provider() {
                    "cli"
                } else {
                    "http"
                },
                ids,
            )
        } else {
            ("catalog", requested)
        };
        let ids: Vec<String> = ids
            .into_iter()
            .map(|id| id.trim().to_string())
            .filter(|id| !id.is_empty())
            .collect();
        let models =
            crate::models_cache::describe_models(&paths, &provider.id, &provider.base_url, &ids);
        Ok(ProviderModelsResponse { source, models })
    })
    .await
    .map_err(|error| ApiError::internal(anyhow::anyhow!(error)))?;
    result.map(Json).map_err(|error| {
        ApiError::new(
            StatusCode::BAD_GATEWAY,
            format!("failed to list models: {}", safe_error_message(error)),
        )
    })
}
