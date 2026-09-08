//! 提示词文件的批量改写与回滚。
//!
//! 改人格提示词要同时动好几个文件。`FileBackup` 在动手前留底，任一步失败就
//! 全部还原——改一半的人格比不改更糟。

use crate::web::*;

/// 每个**已配置**模型的思考档位。
///
/// 以前只返回全局激活的那批，因为档位有自己的按钮和浮层，浮层只谈"当前在用的
/// 模型"。现在档位挂在模型菜单的每一行上，而那张菜单列的是全部已配置模型——
/// 只给激活的那批发档位，其余行就永远显示不出档位，哪怕它们支持。
///
/// 没有推理元数据的模型 `variants` 是空的，前端据此不画档位小片。
pub(in crate::web) fn active_thinking_variant_options(
    config: &AppConfig,
    paths: &MiyuPaths,
) -> Result<Vec<ThinkingVariantOptions>> {
    crate::models_cache::ensure_active_metadata(paths, config);
    let preferences = ThinkingVariantPreferences::load(paths);
    config
        .provider_model_choices()
        .into_iter()
        .map(|choice| {
            let provider = config.provider(Some(&choice.provider_id))?;
            Ok(thinking_variant_options_for_model(
                provider,
                &choice.model,
                preferences.selected(&choice.provider_id, &choice.model),
            ))
        })
        .collect()
}

pub(in crate::web) fn apply_thinking_variant_updates(
    agent: &mut Option<Agent>,
    config: &AppConfig,
    paths: &MiyuPaths,
    updates: &[ThinkingVariantUpdate],
) -> std::result::Result<(), AdminFailure> {
    let options = active_thinking_variant_options(config, paths)
        .map_err(|error| AdminFailure::Internal(safe_error_message(error)))?;
    for update in updates {
        let option = options
            .iter()
            .find(|option| option.provider_id == update.provider_id && option.model == update.model)
            .ok_or_else(|| {
                AdminFailure::Invalid(format!(
                    "inactive model: {} / {}",
                    update.provider_id, update.model
                ))
            })?;
        if let Some(selected) = &update.selected {
            if !option.variants.iter().any(|variant| variant == selected) {
                return Err(AdminFailure::Invalid(format!(
                    "thinking variant is unavailable for {} / {}: {}",
                    update.provider_id, update.model, selected
                )));
            }
        }
    }

    let selections = updates
        .iter()
        .map(|update| {
            (
                update.provider_id.clone(),
                update.model.clone(),
                update.selected.clone(),
            )
        })
        .collect::<Vec<_>>();
    let next_client = agent
        .as_ref()
        .map(|current| {
            let mut client = current.cloned_client();
            client
                .set_thinking_variants(&selections)
                .map_err(|error| AdminFailure::Invalid(safe_error_message(error)))?;
            Ok(client)
        })
        .transpose()?;

    let mut preferences = ThinkingVariantPreferences::load(paths);
    for update in updates {
        preferences.set(&update.provider_id, &update.model, update.selected.clone());
    }
    preferences
        .save(paths)
        .map_err(|error| AdminFailure::Internal(safe_error_message(error)))?;

    if let (Some(agent), Some(client)) = (agent.as_mut(), next_client) {
        agent.replace_client(client);
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub(in crate::web) fn rebuild_for_models(
    agent: &mut Option<Agent>,
    config: &mut AppConfig,
    paths: &MiyuPaths,
    state_store: &StateStore,
    manager: &Arc<Mutex<ManagerState>>,
    models: &[ActiveProviderModelConfig],
) -> std::result::Result<(), AdminFailure> {
    let mut next_config = config.clone();
    next_config
        .set_active_provider_models(models)
        .map_err(|error| AdminFailure::Invalid(safe_error_message(&error)))?;
    if next_config.active_provider_models == config.active_provider_models {
        return Ok(());
    }
    let next_agent = if agent.is_some() {
        crate::models_cache::ensure_active_metadata(paths, &next_config);
        let client = OpenAiCompatibleClient::from_config(&next_config, paths)
            .map_err(|error| AdminFailure::Internal(safe_error_message(&error)))?;
        let registry = build_tool_registry(&next_config, paths, AgentMode::Normal, true)
            .map_err(|error| AdminFailure::Internal(safe_error_message(&error)))?;
        Some(
            Agent::new(
                next_config.clone(),
                paths,
                state_store.clone(),
                client,
                registry,
                AgentMode::Normal,
            )
            .map_err(|error| AdminFailure::Internal(safe_error_message(&error)))?
            .with_headless_pacing(),
        )
    } else {
        None
    };
    let context = next_agent
        .as_ref()
        .map_or_else(
            || cold_context(&next_config, paths, state_store),
            current_context,
        )
        .map_err(|error| AdminFailure::Internal(safe_error_message(&error)))?;
    next_config
        .save(paths)
        .map_err(|error| AdminFailure::Internal(safe_error_message(&error)))?;
    *agent = next_agent;
    *config = next_config.clone();
    let mut manager = manager.lock().unwrap();
    manager.config = next_config;
    manager.context = context;
    Ok(())
}

pub(in crate::web) struct FileBackup {
    pub(in crate::web) path: PathBuf,
    pub(in crate::web) content: Option<Vec<u8>>,
}

pub(in crate::web) fn collect_prompt_file_mutations(
    current: &[PromptDocument],
    next: &[PromptDocument],
    current_dir: &FilePath,
    next_dir: &FilePath,
    mutations: &mut HashMap<PathBuf, Option<Vec<u8>>>,
    with_avatar_metadata: bool,
) {
    for document in next {
        let content = document.content.trim_end();
        let content = if content.is_empty() {
            Vec::new()
        } else {
            format!("{content}\n").into_bytes()
        };
        mutations.insert(next_dir.join(&document.name), Some(content));
        if with_avatar_metadata {
            let metadata_path = next_dir.join(&document.name).with_extension("json");
            let metadata = PersonaMetadata {
                avatar_path: document
                    .avatar_path
                    .as_deref()
                    .map(str::trim)
                    .filter(|path| !path.is_empty())
                    .map(str::to_string),
                board_image_path: document
                    .board_image_path
                    .as_deref()
                    .map(str::trim)
                    .filter(|path| !path.is_empty())
                    .map(str::to_string),
                board_title: document
                    .board_title
                    .as_deref()
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .map(str::to_string),
                board_subtitle: document
                    .board_subtitle
                    .as_deref()
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .map(str::to_string),
                composer_placeholder: document
                    .composer_placeholder
                    .as_deref()
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .map(str::to_string),
                starter_prompts: document.starter_prompts.clone(),
            };
            let metadata = if metadata.avatar_path.is_none()
                && metadata.board_image_path.is_none()
                && metadata.board_title.is_none()
                && metadata.board_subtitle.is_none()
                && metadata.composer_placeholder.is_none()
                && metadata.starter_prompts.is_none()
            {
                None
            } else {
                Some(
                    serde_json::to_vec_pretty(&metadata)
                        .expect("serializing persona metadata cannot fail"),
                )
            };
            mutations.insert(
                metadata_path,
                metadata.map(|mut bytes| {
                    bytes.push(b'\n');
                    bytes
                }),
            );
        }
    }
    for document in current {
        let represented = next.iter().find(|next_document| {
            next_document.original_name.as_deref() == Some(document.name.as_str())
                || next_document.original_name.is_none() && next_document.name == document.name
        });
        let old_path = current_dir.join(&document.name);
        let retained_at_same_path = represented
            .map(|next_document| next_dir.join(&next_document.name) == old_path)
            .unwrap_or(false);
        if !retained_at_same_path {
            mutations.entry(old_path).or_insert(None);
            if with_avatar_metadata {
                mutations
                    .entry(current_dir.join(&document.name).with_extension("json"))
                    .or_insert(None);
            }
        }
    }
}

pub(in crate::web) fn restore_file_backups(backups: &[FileBackup]) {
    for backup in backups {
        restore_optional_file(&backup.path, backup.content.as_deref());
    }
}

pub(in crate::web) fn restore_optional_file(path: &FilePath, content: Option<&[u8]>) {
    if let Some(content) = content {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::write(path, content);
    } else if path.exists() {
        let _ = std::fs::remove_file(path);
    }
}
