//! 配置的读取、校验与落盘。
//!
//! 出口要脱敏、入口要还原：`config_response` 把密钥抹成掩码发给前端，
//! `restore_config_secrets` 在写回时把掩码换回原值——否则用户在网页上改个无关
//! 选项就会把 API key 存成一串星号。
//!
//! 另一件事是判断改动要不要打断当前回合（`config_change_requires_interrupt`）：
//! 换模型、换提示词得立刻生效，调个显示选项不该把用户正在跑的回合掐掉。

use crate::web::*;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::web) struct UpdateConfigRequest {
    pub(in crate::web) config: Value,
    #[serde(default)]
    pub(in crate::web) secrets: HashMap<String, SecretMutation>,
    pub(in crate::web) prompts: PromptDocuments,
    #[serde(default)]
    pub(in crate::web) reset_conversation: bool,
}

#[derive(Serialize)]
pub(in crate::web) struct ConfigResponse {
    pub(in crate::web) config: Value,
    pub(in crate::web) secret_states: HashMap<String, bool>,
    pub(in crate::web) prompts: PromptDocuments,
    pub(in crate::web) models: Vec<SafeModel>,
    pub(in crate::web) multimodal_models: Vec<SafeModel>,
    pub(in crate::web) display: WebDisplayConfig,
    pub(in crate::web) context: ContextSnapshot,
    pub(in crate::web) persona: PersonaIdentity,
}

#[derive(Clone, Serialize)]
pub(in crate::web) struct WebDisplayConfig {
    pub(in crate::web) reasoning: String,
    pub(in crate::web) tool_calls: String,
    pub(in crate::web) readable_tool_names: bool,
    pub(in crate::web) command_output_lines: usize,
    pub(in crate::web) mixed_model_endpoint_display: String,
    pub(in crate::web) show_mixed_model_endpoint: bool,
}

pub(in crate::web) async fn get_config(
    State(state): State<DaemonState>,
    headers: HeaderMap,
) -> std::result::Result<Response, ApiError> {
    require_admin(&headers, &state)?;
    let (config, context) = {
        let manager = state.manager.lock().unwrap();
        (manager.config.clone(), manager.context)
    };
    let mut response = Json(config_response(&config, context, &state.paths)?).into_response();
    response
        .headers_mut()
        .insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    Ok(response)
}

pub(in crate::web) async fn update_config(
    State(state): State<DaemonState>,
    headers: HeaderMap,
    Json(request): Json<UpdateConfigRequest>,
) -> std::result::Result<Json<ConfigResponse>, ApiError> {
    require_admin_mutation(&headers, &state)?;

    let current = state.manager.lock().unwrap().config.clone();
    let current_prompts =
        read_prompt_documents(&current, &state.paths).map_err(ApiError::internal)?;
    let mut candidate: AppConfig = serde_json::from_value(request.config).map_err(|error| {
        ApiError::new(
            StatusCode::BAD_REQUEST,
            format!("invalid configuration: {}", safe_error_message(error)),
        )
    })?;
    reconcile_qq_persona_references(&mut candidate, &request.prompts);
    candidate.normalize_platform_model_routes();
    restore_config_secrets(&mut candidate, &current, &request.secrets)?;
    validate_config_candidate(&candidate)?;
    validate_prompt_documents(&candidate, &request.prompts)?;
    // candidate 随后被 move 进 ActorCommand,改名对要先算出来。
    let renames = crate::config::detect_provider_renames(&current.providers, &candidate.providers);
    let qq_listener = state
        .platforms
        .qq_listener
        .prepare(&state, Some(&current.platforms.qq), &candidate.platforms.qq)
        .await
        .map_err(|error| {
            ApiError::new(
                StatusCode::BAD_REQUEST,
                format!(
                    "Tencent QQ listener configuration failed: {}",
                    safe_error_message(error)
                ),
            )
        })?;
    let requested_prompts = request.prompts.clone();
    // Allowed while turns run: the ApplyConfig handler interrupts running
    // turns only for persona layout changes; everything else hot-applies.
    reserve_admin_light(&state.manager)?;
    let (reply, receiver) = oneshot::channel();
    if state
        .actor_tx
        .send(ActorCommand::ApplyConfig {
            config: Box::new(candidate),
            prompts: request.prompts,
            reset_conversation: false,
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
        Ok(Ok(())) => qq_listener.commit(),
        Ok(Err(AdminFailure::Invalid(message))) => {
            return Err(ApiError::new(StatusCode::BAD_REQUEST, message));
        }
        Ok(Err(AdminFailure::Internal(message))) => {
            tracing::error!(
                error = %message,
                "{}",
                t("WebUI configuration update failed", "WebUI 配置更新失败")
            );
            return Err(ApiError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                safe_error_message(&message),
            ));
        }
        Err(_) => {
            release_admin(&state.manager);
            return Err(ApiError::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "agent worker stopped before updating the configuration",
            ));
        }
    }
    sync_usage_ledger_after_rename(&state, renames).await;
    cleanup_persona_assets(&state.paths, &current_prompts, &requested_prompts);
    let manager = state.manager.lock().unwrap();
    Ok(Json(config_response(
        &manager.config,
        manager.context,
        &state.paths,
    )?))
}

/// 供应商 id 改名后把用量账本里的旧 id 一起改掉,否则统计页会把同一个供应商
/// 拆成新旧两行,旧行还因为查不到 base_url 算不出费用。
///
/// 账本可能几十 MB,重写走 `spawn_blocking`。改失败只记日志:配置已经生效了,
/// 不能让一次账本重写把保存变成报错。
async fn sync_usage_ledger_after_rename(state: &DaemonState, renames: Vec<(String, String)>) {
    if renames.is_empty() {
        return;
    }
    let store = state.state_store.clone();
    let applied = tokio::task::spawn_blocking(move || {
        renames
            .into_iter()
            .map(|(old, new)| {
                let result = store.rename_usage_provider(&old, &new);
                (old, new, result)
            })
            .collect::<Vec<_>>()
    })
    .await;
    let applied = match applied {
        Ok(applied) => applied,
        Err(error) => {
            tracing::warn!(error = %error, "renaming usage ledger providers failed");
            return;
        }
    };
    for (old, new, result) in applied {
        match result {
            Ok(rows) => tracing::info!(
                old = %old,
                new = %new,
                rows,
                "{}",
                t(
                    "usage ledger provider renamed",
                    "用量账本供应商 id 已同步改名"
                )
            ),
            Err(error) => tracing::warn!(
                error = %error, old = %old, new = %new,
                "renaming usage ledger providers failed"
            ),
        }
    }
}

pub(in crate::web) async fn get_thinking_variants(
    State(state): State<DaemonState>,
    headers: HeaderMap,
) -> std::result::Result<Response, ApiError> {
    // 只读:成员的模型菜单也要显示档位;改档位仍是管理员的事。
    require_auth(&headers, &state)?;
    let config = state.manager.lock().unwrap().config.clone();
    let options =
        active_thinking_variant_options(&config, &state.paths).map_err(ApiError::internal)?;
    let mut response = Json(ThinkingVariantsResponse { options }).into_response();
    response
        .headers_mut()
        .insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    Ok(response)
}

pub(in crate::web) async fn set_thinking_variants(
    State(state): State<DaemonState>,
    headers: HeaderMap,
    Json(request): Json<SetThinkingVariantsRequest>,
) -> std::result::Result<Json<ThinkingVariantsResponse>, ApiError> {
    require_admin_mutation(&headers, &state)?;
    let updates = validate_thinking_variant_updates(request.updates)?;
    reserve_admin(&state.manager)?;
    let (reply, receiver) = oneshot::channel();
    if state
        .actor_tx
        .send(ActorCommand::SetThinkingVariants { updates, reply })
        .is_err()
    {
        release_admin(&state.manager);
        return Err(ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "agent worker is unavailable",
        ));
    }
    match receiver.await {
        Ok(Ok(())) => {}
        Ok(Err(AdminFailure::Invalid(message))) => {
            return Err(ApiError::new(StatusCode::BAD_REQUEST, message));
        }
        Ok(Err(AdminFailure::Internal(message))) => {
            tracing::error!(
                error = %message,
                "{}",
                t(
                    "WebUI thinking variant update failed",
                    "WebUI 思考程度更新失败"
                )
            );
            return Err(ApiError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                safe_error_message(&message),
            ));
        }
        Err(_) => {
            release_admin(&state.manager);
            return Err(ApiError::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "agent worker stopped before updating the thinking variant",
            ));
        }
    }
    let config = state.manager.lock().unwrap().config.clone();
    let options =
        active_thinking_variant_options(&config, &state.paths).map_err(ApiError::internal)?;
    Ok(Json(ThinkingVariantsResponse { options }))
}

pub(in crate::web) async fn get_session_models_http(
    State(state): State<DaemonState>,
    headers: HeaderMap,
    Path(session_id): Path<String>,
) -> std::result::Result<Json<SessionModelsResponse>, ApiError> {
    require_auth(&headers, &state)?;
    let record = require_local_web_session(&state, &headers, &session_id)?;
    let model_override = state
        .stores
        .for_session(&record.session_id)
        .session_model_override(&record.session_id)
        .map_err(ApiError::internal)?;
    Ok(Json(SessionModelsResponse { model_override }))
}

pub(in crate::web) async fn set_session_models_http(
    State(state): State<DaemonState>,
    headers: HeaderMap,
    Path(session_id): Path<String>,
    Json(request): Json<SetSessionModelsRequest>,
) -> std::result::Result<Json<SessionModelsResponse>, ApiError> {
    require_mutation(&headers, &state)?;
    let record = require_local_web_session(&state, &headers, &session_id)?;
    let models = (!request.models.is_empty()).then(|| request.models);
    if let Some(models) = &models {
        let choices = {
            let manager = state.manager.lock().unwrap();
            manager.config.text_provider_model_choices()
        };
        for model in models {
            if !choices.iter().any(|choice| {
                choice.provider_id == model.provider_id && choice.model == model.model
            }) {
                return Err(ApiError::new(
                    StatusCode::BAD_REQUEST,
                    format!("unknown model: {}/{}", model.provider_id, model.model),
                ));
            }
        }
    }
    state
        .stores
        .for_session(&record.session_id)
        .set_session_model_override(&record.session_id, models.as_deref())
        .map_err(ApiError::internal)?;
    state.events.publish(
        "session.updated",
        json!({ "session_id": record.session_id, "model_override": models }),
    );
    Ok(Json(SessionModelsResponse {
        model_override: models,
    }))
}

pub(in crate::web) async fn set_models(
    State(state): State<DaemonState>,
    headers: HeaderMap,
    Json(request): Json<SetModelsRequest>,
) -> std::result::Result<Json<ModelResponse>, ApiError> {
    require_admin_mutation(&headers, &state)?;
    let models = validate_model_selection(request.models)?;
    reserve_admin_light(&state.manager)?;
    let (reply, receiver) = oneshot::channel();
    if state
        .actor_tx
        .send(ActorCommand::SetModels { models, reply })
        .is_err()
    {
        release_admin(&state.manager);
        return Err(ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "agent worker is unavailable",
        ));
    }
    match receiver.await {
        Ok(Ok(())) => {}
        Ok(Err(AdminFailure::Invalid(message))) => {
            return Err(ApiError::new(StatusCode::BAD_REQUEST, message));
        }
        Ok(Err(AdminFailure::Internal(message))) => {
            tracing::error!(
                error = %message,
                "{}",
                t("WebUI model update failed", "WebUI 模型更新失败")
            );
            return Err(ApiError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                safe_error_message(&message),
            ));
        }
        Err(_) => {
            release_admin(&state.manager);
            return Err(ApiError::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "agent worker stopped before updating the model",
            ));
        }
    }
    let manager = state.manager.lock().unwrap();
    Ok(Json(ModelResponse {
        models: safe_models(&manager.config),
        display: web_display_config(&manager.config),
        context: manager.context,
    }))
}

#[allow(clippy::too_many_arguments)]
pub(in crate::web) fn rebuild_for_config(
    agent: &mut Option<Agent>,
    config: &mut AppConfig,
    paths: &MiyuPaths,
    state_store: &StateStore,
    manager: &Arc<Mutex<ManagerState>>,
    events: &EventHub,
    next_config: AppConfig,
    prompts: &PromptDocuments,
    reset_conversation: bool,
) -> std::result::Result<(), AdminFailure> {
    let _ = reset_conversation;
    let mut next_config = next_config;
    // Models removed from the text models must leave the tier pools too.
    next_config.prune_model_tiers();
    let previous_prompts = read_prompt_documents(config, paths)
        .map_err(|error| AdminFailure::Internal(safe_error_message(error)))?;
    let persona_changes = persona_document_changes(&previous_prompts, prompts);
    let mut persona_db_guard = PersonaDbRenameGuard::new(state_store.clone(), &persona_changes)
        .map_err(|error| AdminFailure::Internal(safe_error_message(error)))?;
    let previous_scope = config.active_persona_scope();
    let next_scope = next_config.active_persona_scope();
    let migrated_previous_scope = persona_changes
        .iter()
        .find_map(|(old_name, new_name)| {
            (crate::config::persona_scope_name(old_name) == previous_scope)
                .then(|| new_name.as_deref().map(crate::config::persona_scope_name))
                .flatten()
        })
        .unwrap_or_else(|| previous_scope.clone());
    let persona_changed = migrated_previous_scope != next_scope;
    let previous_session_id = state_store.session_id().to_string();
    let target_session_id = if persona_changed {
        session_for_persona(state_store, manager, &next_scope)
            .map_err(|error| AdminFailure::Internal(safe_error_message(error)))?
    } else {
        previous_session_id.clone()
    };
    if persona_changed {
        state_store
            .set_persona_current_session(&migrated_previous_scope, &previous_session_id)
            .map_err(|error| AdminFailure::Internal(safe_error_message(error)))?;
    }
    let target_state_store = if persona_changed {
        state_store.pinned(&target_session_id)
    } else {
        state_store.clone()
    };
    let prompt_backups =
        apply_prompt_documents(config, &next_config, &previous_prompts, prompts, paths)
            .map_err(|error| AdminFailure::Internal(safe_error_message(error)))?;
    let scope_backups = match apply_persona_scope_changes(
        config,
        &next_config,
        &previous_prompts,
        prompts,
        paths,
    ) {
        Ok(backups) => backups,
        Err(error) => {
            restore_file_backups(&prompt_backups);
            return Err(AdminFailure::Internal(safe_error_message(error)));
        }
    };
    let config_backup = FileBackup {
        path: paths.config_file.clone(),
        content: std::fs::read(&paths.config_file).ok(),
    };
    let system_prompt_backup = next_config.system_prompt.as_ref().map(|_| FileBackup {
        path: next_config.system_prompt_path(paths),
        content: std::fs::read(next_config.system_prompt_path(paths)).ok(),
    });

    let build_agent = || -> Result<Agent> {
        crate::models_cache::ensure_active_metadata(paths, &next_config);
        let client = OpenAiCompatibleClient::from_config(&next_config, paths)?;
        let registry = build_tool_registry(&next_config, paths, AgentMode::Normal, true)?;
        Ok(Agent::new(
            next_config.clone(),
            paths,
            target_state_store.clone(),
            client,
            registry,
            AgentMode::Normal,
        )?
        .with_headless_pacing())
    };
    let next_agent = if agent.is_some() {
        match build_agent() {
            Ok(agent) => Some(agent),
            Err(error) => {
                restore_file_backups(&prompt_backups);
                restore_persona_scope_backups(&scope_backups);
                return Err(AdminFailure::Invalid(safe_error_message(error)));
            }
        }
    } else {
        None
    };
    let context = match next_agent.as_ref().map_or_else(
        || cold_context(&next_config, paths, &target_state_store),
        current_context,
    ) {
        Ok(context) => context,
        Err(error) => {
            restore_file_backups(&prompt_backups);
            restore_persona_scope_backups(&scope_backups);
            return Err(AdminFailure::Invalid(safe_error_message(error)));
        }
    };
    if let Err(error) = next_config.save(paths) {
        restore_file_backups(&prompt_backups);
        restore_persona_scope_backups(&scope_backups);
        restore_file_backups(std::slice::from_ref(&config_backup));
        if let Some(backup) = &system_prompt_backup {
            restore_file_backups(std::slice::from_ref(backup));
        }
        return Err(AdminFailure::Internal(safe_error_message(error)));
    }

    if persona_changed {
        if let Err(error) = state_store.switch_session(&target_session_id) {
            restore_file_backups(&prompt_backups);
            restore_persona_scope_backups(&scope_backups);
            restore_file_backups(std::slice::from_ref(&config_backup));
            if let Some(backup) = &system_prompt_backup {
                restore_file_backups(std::slice::from_ref(backup));
            }
            return Err(AdminFailure::Internal(safe_error_message(error)));
        }
        if let Err(error) = state_store.set_persona_current_session(&next_scope, &target_session_id)
        {
            let _ = state_store.switch_session(&previous_session_id);
            restore_file_backups(&prompt_backups);
            restore_persona_scope_backups(&scope_backups);
            restore_file_backups(std::slice::from_ref(&config_backup));
            if let Some(backup) = &system_prompt_backup {
                restore_file_backups(std::slice::from_ref(backup));
            }
            return Err(AdminFailure::Internal(safe_error_message(error)));
        }
    }

    *agent = next_agent;
    *config = next_config.clone();
    let mut manager = manager.lock().unwrap();
    let migrated_session_ids = persona_changes
        .iter()
        .filter_map(|(old_name, new_name)| {
            let old_scope = crate::config::persona_scope_name(old_name);
            let new_scope = new_name.as_deref().map(crate::config::persona_scope_name)?;
            manager
                .persona_session_ids
                .remove(&old_scope)
                .map(|session_id| (new_scope, session_id))
        })
        .collect::<Vec<_>>();
    manager.persona_session_ids.extend(migrated_session_ids);
    if persona_changed {
        manager
            .persona_session_ids
            .insert(migrated_previous_scope, previous_session_id);
        manager
            .persona_session_ids
            .insert(next_scope, target_session_id.clone());
    }
    manager.config = next_config;
    manager.context = context;
    drop(manager);
    if persona_changed {
        events.publish(
            "session.current_changed",
            json!({ "session_id": target_session_id }),
        );
    }
    persona_db_guard.commit();
    finalize_persona_scope_backups(&scope_backups);
    for (old_name, new_name) in &persona_changes {
        if new_name.is_none() {
            if let Err(error) =
                state_store.delete_persona_scope(&crate::config::persona_scope_name(old_name))
            {
                tracing::warn!(
                    %error,
                    %old_name,
                    "{}",
                    t(
                        "deleted persona state cleanup failed",
                        "已删除角色的状态清理失败"
                    )
                );
            }
        }
    }
    Ok(())
}

pub(in crate::web) fn config_response(
    config: &AppConfig,
    context: ContextSnapshot,
    paths: &MiyuPaths,
) -> std::result::Result<ConfigResponse, ApiError> {
    let mut redacted = config.clone();
    let mut secret_states = HashMap::new();
    for (index, provider) in redacted.providers.iter_mut().enumerate() {
        secret_states.insert(
            format!("providers.{index}.api_key"),
            provider
                .api_key
                .as_deref()
                .is_some_and(|value| !value.trim().is_empty()),
        );
        provider.api_key = None;
    }
    redact_secret_list(
        &mut secret_states,
        "plugins.web.tavily_api_keys",
        &mut redacted.plugins.web.tavily_api_keys,
    );
    redact_secret_list(
        &mut secret_states,
        "plugins.web.firecrawl_api_keys",
        &mut redacted.plugins.web.firecrawl_api_keys,
    );
    redact_secret_list(
        &mut secret_states,
        "plugins.web.anysearch_api_keys",
        &mut redacted.plugins.web.anysearch_api_keys,
    );
    redact_secret_list(
        &mut secret_states,
        "plugins.web.exa_api_keys",
        &mut redacted.plugins.web.exa_api_keys,
    );
    secret_states.insert(
        "plugins.exchange_rate.api_key".to_string(),
        !redacted.plugins.exchange_rate.api_key.trim().is_empty(),
    );
    redacted.plugins.exchange_rate.api_key.clear();
    secret_states.insert(
        "platforms.qq.access_token".to_string(),
        !redacted.platforms.qq.access_token.trim().is_empty(),
    );
    redacted.platforms.qq.access_token.clear();
    redact_secret_list(
        &mut secret_states,
        "plugins.image_generation.api_keys",
        &mut redacted.plugins.image_generation.api_keys,
    );
    redact_api_quota_provider(
        &mut secret_states,
        "plugins.api_quota.deepseek",
        &mut redacted.plugins.api_quota.deepseek,
    );
    redact_api_quota_provider(
        &mut secret_states,
        "plugins.api_quota.openrouter",
        &mut redacted.plugins.api_quota.openrouter,
    );
    let mut config_value = serde_json::to_value(&redacted).map_err(ApiError::internal)?;
    if let Value::Object(config_object) = &mut config_value {
        config_object.insert(
            "memory".to_string(),
            serde_json::to_value(redacted.memory_config()).map_err(ApiError::internal)?,
        );
    }
    let prompts = read_prompt_documents(config, paths).map_err(ApiError::internal)?;
    let persona = persona_identity(config, &prompts);
    Ok(ConfigResponse {
        config: config_value,
        secret_states,
        prompts,
        models: safe_models(config),
        multimodal_models: safe_multimodal_models(config),
        display: web_display_config(config),
        context,
        persona,
    })
}

pub(in crate::web) fn restore_config_secrets(
    candidate: &mut AppConfig,
    current: &AppConfig,
    mutations: &HashMap<String, SecretMutation>,
) -> std::result::Result<(), ApiError> {
    let mut recognized = HashSet::new();
    for (index, provider) in candidate.providers.iter_mut().enumerate() {
        let key = format!("providers.{index}.api_key");
        recognized.insert(key.clone());
        let existing = current
            .providers
            .iter()
            .find(|item| item.id == provider.id)
            .and_then(|item| item.api_key.clone());
        provider.api_key = match mutations.get(&key) {
            Some(SecretMutation::Set(value)) => normalize_single_secret(value, &key)?,
            Some(SecretMutation::Clear) => None,
            None => existing,
        };
    }

    restore_secret_list(
        candidate,
        current,
        mutations,
        &mut recognized,
        "plugins.web.tavily_api_keys",
        |config| &mut config.plugins.web.tavily_api_keys,
        |config| &config.plugins.web.tavily_api_keys,
    )?;
    restore_secret_list(
        candidate,
        current,
        mutations,
        &mut recognized,
        "plugins.web.firecrawl_api_keys",
        |config| &mut config.plugins.web.firecrawl_api_keys,
        |config| &config.plugins.web.firecrawl_api_keys,
    )?;
    restore_secret_list(
        candidate,
        current,
        mutations,
        &mut recognized,
        "plugins.web.anysearch_api_keys",
        |config| &mut config.plugins.web.anysearch_api_keys,
        |config| &config.plugins.web.anysearch_api_keys,
    )?;
    restore_secret_list(
        candidate,
        current,
        mutations,
        &mut recognized,
        "plugins.web.exa_api_keys",
        |config| &mut config.plugins.web.exa_api_keys,
        |config| &config.plugins.web.exa_api_keys,
    )?;
    restore_secret_list(
        candidate,
        current,
        mutations,
        &mut recognized,
        "plugins.image_generation.api_keys",
        |config| &mut config.plugins.image_generation.api_keys,
        |config| &config.plugins.image_generation.api_keys,
    )?;

    let exchange_key = "plugins.exchange_rate.api_key";
    recognized.insert(exchange_key.to_string());
    candidate.plugins.exchange_rate.api_key = match mutations.get(exchange_key) {
        Some(SecretMutation::Set(value)) => {
            normalize_single_secret(value, exchange_key)?.unwrap_or_default()
        }
        Some(SecretMutation::Clear) => String::new(),
        None => current.plugins.exchange_rate.api_key.clone(),
    };

    restore_api_quota_provider(
        &mut candidate.plugins.api_quota.deepseek,
        &current.plugins.api_quota.deepseek,
        mutations,
        &mut recognized,
        "plugins.api_quota.deepseek",
    )?;
    restore_api_quota_provider(
        &mut candidate.plugins.api_quota.openrouter,
        &current.plugins.api_quota.openrouter,
        mutations,
        &mut recognized,
        "plugins.api_quota.openrouter",
    )?;

    let onebot_token_key = "platforms.qq.access_token";
    recognized.insert(onebot_token_key.to_string());
    candidate.platforms.qq.access_token = match mutations.get(onebot_token_key) {
        Some(SecretMutation::Set(value)) => {
            normalize_single_secret(value, onebot_token_key)?.unwrap_or_default()
        }
        Some(SecretMutation::Clear) => String::new(),
        None => current.platforms.qq.access_token.clone(),
    };

    if let Some(key) = mutations.keys().find(|key| !recognized.contains(*key)) {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            format!("unknown secret field: {key}"),
        ));
    }
    Ok(())
}

pub(in crate::web) fn validate_config_candidate(
    config: &AppConfig,
) -> std::result::Result<(), ApiError> {
    config.validate().map_err(|error| {
        ApiError::new(
            StatusCode::BAD_REQUEST,
            format!("invalid configuration: {}", safe_error_message(error)),
        )
    })?;
    let mut provider_ids = HashSet::with_capacity(config.providers.len());
    for provider in &config.providers {
        if !provider_ids.insert(provider.id.trim()) {
            return Err(ApiError::new(
                StatusCode::BAD_REQUEST,
                format!("duplicate provider id: {}", provider.id),
            ));
        }
    }
    if let Some(active) = &config.active_provider_models {
        let mut checked = config.clone();
        checked
            .set_active_provider_models(active)
            .map_err(|error| ApiError::new(StatusCode::BAD_REQUEST, safe_error_message(error)))?;
    }
    if let Some(active) = &config.active_multimodal_provider_models {
        let choices = config.multimodal_provider_model_choices();
        let mut seen = HashSet::with_capacity(active.len());
        for model in active {
            if !seen.insert((&model.provider_id, &model.model))
                || !choices.iter().any(|choice| {
                    choice.provider_id == model.provider_id && choice.model == model.model
                })
            {
                return Err(ApiError::new(
                    StatusCode::BAD_REQUEST,
                    format!(
                        "invalid multimodal provider/model: {} / {}",
                        model.provider_id, model.model
                    ),
                ));
            }
        }
    }
    Ok(())
}

/// True when applying `next` cannot safely coexist with running turns:
/// persona renames/deletions and active-persona switches migrate or delete
/// the session state those turns are using. Everything else hot-applies.
pub(in crate::web) fn config_change_requires_interrupt(
    current: &AppConfig,
    next: &AppConfig,
    paths: &MiyuPaths,
    next_prompts: &PromptDocuments,
) -> bool {
    let Ok(previous_prompts) = read_prompt_documents(current, paths) else {
        // The safe direction: interrupt when the current prompt layout cannot
        // be read to prove the change is turn-safe.
        return true;
    };
    if !persona_document_changes(&previous_prompts, next_prompts).is_empty() {
        return true;
    }
    current.active_persona_scope() != next.active_persona_scope()
}

pub(in crate::web) fn web_display_config(config: &AppConfig) -> WebDisplayConfig {
    let mixed_model_endpoint_display = config.display.mixed_model_endpoint_display.clone();
    WebDisplayConfig {
        reasoning: config.display.reasoning.clone(),
        tool_calls: config.display.tool_calls.clone(),
        readable_tool_names: config.display.readable_tool_names,
        command_output_lines: config.display.command_output_lines,
        show_mixed_model_endpoint: config.active_provider_model_choices().len() > 1
            && matches!(mixed_model_endpoint_display.as_str(), "interactive" | "all"),
        mixed_model_endpoint_display,
    }
}
