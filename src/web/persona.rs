//! 人格与提示词文档。
//!
//! 一个人格 = 元数据 + 一组提示词文档 + 素材。改人格是**跨存储的事务**：
//! 提示词文件在磁盘、会话与记忆的归属在库里、QQ 侧还有一份引用。任何一步失败
//! 都要能整体回退，所以这里有 `PersonaScopeBackup` 与 `PersonaDbRenameGuard`
//! 这套备份/守卫——不是过度设计，是改一半崩掉会把会话挂到不存在的人格上。

use crate::web::*;

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub(in crate::web) struct PersonaMetadata {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(in crate::web) avatar_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(in crate::web) board_image_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(in crate::web) board_title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(in crate::web) board_subtitle: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(in crate::web) composer_placeholder: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(in crate::web) starter_prompts: Option<Vec<String>>,
}

#[derive(Clone, Debug, Serialize)]
pub(in crate::web) struct PersonaIdentity {
    pub(in crate::web) name: String,
    pub(in crate::web) avatar_url: Option<String>,
    pub(in crate::web) board_image_url: Option<String>,
    pub(in crate::web) board_title: String,
    pub(in crate::web) board_subtitle: String,
    /// 已经解析过默认值,前端直接用。默认跟着人格名走,所以是算出来的而不是
    /// 一个常量——改人格名之后输入框仍写死 "给 Miyu 发消息" 是原来的毛病。
    pub(in crate::web) composer_placeholder: String,
    pub(in crate::web) starter_prompts: Vec<String>,
}

pub(in crate::web) fn active_persona_scope(state: &DaemonState) -> String {
    state.manager.lock().unwrap().config.active_persona_scope()
}

pub(in crate::web) async fn persona_avatar(
    State(state): State<DaemonState>,
    headers: HeaderMap,
    Query(query): Query<std::collections::HashMap<String, String>>,
) -> std::result::Result<Response, ApiError> {
    require_auth(&headers, &state)?;
    let (config, prompts) = {
        let manager = state.manager.lock().unwrap();
        let prompts =
            read_prompt_documents(&manager.config, &state.paths).map_err(ApiError::internal)?;
        (manager.config.clone(), prompts)
    };
    let path = if let Some(path) = query.get("path").filter(|p| !p.is_empty()) {
        managed_persona_asset_path(&state.paths, path).ok_or_else(|| {
            ApiError::new(
                StatusCode::BAD_REQUEST,
                "invalid managed persona asset path",
            )
        })?
    } else if query.contains_key("board") {
        active_persona_board_path(&config, &prompts, &state.paths)
            .ok_or_else(|| ApiError::new(StatusCode::NOT_FOUND, "persona board image not found"))?
    } else if let Some(path) = active_persona_avatar_path(&config, &prompts, &state.paths) {
        path
    } else {
        return Err(ApiError::new(
            StatusCode::NOT_FOUND,
            "persona avatar not found",
        ));
    };
    if path.starts_with(state.paths.persona_avatars_dir()) {
        validate_managed_persona_asset_file(&state.paths, &path)
            .map_err(|_| ApiError::new(StatusCode::NOT_FOUND, "persona avatar not found"))?;
    }
    let bytes = tokio::fs::read(&path)
        .await
        .map_err(|_| ApiError::new(StatusCode::NOT_FOUND, "persona avatar not found"))?;
    if bytes.len() > 8 * 1024 * 1024 {
        return Err(ApiError::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            "persona avatar is too large",
        ));
    }
    let format = image::guess_format(&bytes)
        .map_err(|_| ApiError::new(StatusCode::NOT_FOUND, "persona avatar is not an image"))?;
    let mime = match format {
        image::ImageFormat::Png => "image/png",
        image::ImageFormat::Jpeg => "image/jpeg",
        image::ImageFormat::Gif => "image/gif",
        image::ImageFormat::WebP => "image/webp",
        image::ImageFormat::Bmp => "image/bmp",
        _ => {
            return Err(ApiError::new(
                StatusCode::NOT_FOUND,
                "persona avatar format is unsupported",
            ))
        }
    };
    let mut response = bytes.into_response();
    response
        .headers_mut()
        .insert(CONTENT_TYPE, HeaderValue::from_static(mime));
    response
        .headers_mut()
        .insert(CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    response
        .headers_mut()
        .insert(X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    Ok(response)
}

pub(in crate::web) fn persona_identity(
    config: &AppConfig,
    prompts: &PromptDocuments,
) -> PersonaIdentity {
    let active = config.prompt.active_persona.trim();
    if active.is_empty() {
        return PersonaIdentity {
            name: "Miyu".to_string(),
            avatar_url: Some("/assets/miyu-logo.png".to_string()),
            board_image_url: Some("/assets/miyuwallpaper.png".to_string()),
            board_title: DEFAULT_BOARD_TITLE.to_string(),
            board_subtitle: DEFAULT_BOARD_SUBTITLE.to_string(),
            composer_placeholder: default_composer_placeholder("Miyu"),
            starter_prompts: DEFAULT_STARTER_PROMPTS.map(str::to_string).to_vec(),
        };
    }
    let document = prompts
        .personas
        .iter()
        .find(|document| document.name == active);
    let avatar_url = document
        .and_then(|document| document.avatar_path.as_deref())
        .filter(|path| !path.trim().is_empty())
        .and_then(|_| Some("/api/persona/avatar".to_string()));
    let board_image_url = if document
        .and_then(|document| document.board_image_path.as_deref())
        .is_some_and(|path| !path.trim().is_empty())
    {
        Some("/api/persona/avatar?board=1".to_string())
    } else {
        None
    };
    let board_title = document
        .and_then(|document| document.board_title.as_deref())
        .filter(|value| !value.trim().is_empty())
        .unwrap_or(DEFAULT_BOARD_TITLE)
        .to_string();
    let board_subtitle = document
        .and_then(|document| document.board_subtitle.as_deref())
        .filter(|value| !value.trim().is_empty())
        .unwrap_or(DEFAULT_BOARD_SUBTITLE)
        .to_string();
    let name = active.strip_suffix(".md").unwrap_or(active).to_string();
    let composer_placeholder = document
        .and_then(|document| document.composer_placeholder.as_deref())
        .filter(|value| !value.trim().is_empty())
        .map_or_else(|| default_composer_placeholder(&name), str::to_string);
    let configured_prompts = document.and_then(|document| document.starter_prompts.as_deref());
    let starter_prompts = DEFAULT_STARTER_PROMPTS
        .iter()
        .enumerate()
        .map(|(index, fallback)| {
            configured_prompts
                .and_then(|values| values.get(index))
                .filter(|value| !value.trim().is_empty())
                .map_or_else(|| (*fallback).to_string(), Clone::clone)
        })
        .collect();
    PersonaIdentity {
        name,
        avatar_url,
        board_image_url,
        board_title,
        board_subtitle,
        composer_placeholder,
        starter_prompts,
    }
}

pub(in crate::web) fn active_persona_avatar_path(
    config: &AppConfig,
    prompts: &PromptDocuments,
    paths: &MiyuPaths,
) -> Option<PathBuf> {
    let active = config.prompt.active_persona.trim();
    if active.is_empty() {
        return None;
    }
    let value = prompts
        .personas
        .iter()
        .find(|document| document.name == active)
        .and_then(|document| document.avatar_path.as_deref())?;
    resolve_persona_asset_path(paths, value)
}

pub(in crate::web) fn active_persona_board_path(
    config: &AppConfig,
    prompts: &PromptDocuments,
    paths: &MiyuPaths,
) -> Option<PathBuf> {
    let active = config.prompt.active_persona.trim();
    let value = prompts
        .personas
        .iter()
        .find(|document| document.name == active)
        .and_then(|document| document.board_image_path.as_deref())?;
    resolve_persona_asset_path(paths, value)
}

pub(in crate::web) fn validate_prompt_documents(
    config: &AppConfig,
    prompts: &PromptDocuments,
) -> std::result::Result<(), ApiError> {
    validate_prompt_document_list("persona", &prompts.personas)?;
    validate_prompt_document_list("identity", &prompts.identities)?;
    let mut persona_scopes = HashMap::<String, &str>::new();
    for document in &prompts.personas {
        if document.name.eq_ignore_ascii_case("system-prompt.md") {
            return Err(ApiError::new(
                StatusCode::BAD_REQUEST,
                "system-prompt.md is reserved and cannot be used as a persona",
            ));
        }
        let scope = crate::config::persona_scope_name(&document.name);
        if let Some(existing) = persona_scopes.insert(scope.clone(), &document.name) {
            return Err(ApiError::new(
                StatusCode::BAD_REQUEST,
                format!(
                    "persona names map to the same persistent scope: {existing} and {} ({scope})",
                    document.name
                ),
            ));
        }
    }
    if !config.prompt.active_persona.trim().is_empty()
        && !prompts
            .personas
            .iter()
            .any(|document| document.name == config.prompt.active_persona)
    {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "the active persona does not exist",
        ));
    }
    for route in &config.platforms.qq.conversations {
        let Some(name) = route.persona.custom_name() else {
            continue;
        };
        if !prompts
            .personas
            .iter()
            .any(|document| document.name == name)
        {
            return Err(ApiError::new(
                StatusCode::BAD_REQUEST,
                format!("QQ conversation persona does not exist: {name}"),
            ));
        }
    }
    if !config.prompt.active_identity.trim().is_empty()
        && !prompts
            .identities
            .iter()
            .any(|document| document.name == config.prompt.active_identity)
    {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "the active identity does not exist",
        ));
    }
    Ok(())
}

pub(in crate::web) fn reconcile_qq_persona_references(
    config: &mut AppConfig,
    prompts: &PromptDocuments,
) {
    let renames = prompts
        .personas
        .iter()
        .filter_map(|document| {
            document
                .original_name
                .as_deref()
                .filter(|original| *original != document.name)
                .map(|original| (original.to_string(), document.name.clone()))
        })
        .collect::<HashMap<_, _>>();
    for route in &mut config.platforms.qq.conversations {
        let Some(current) = route.persona.custom_name() else {
            continue;
        };
        if let Some(next) = renames.get(current) {
            route.persona = crate::config::PlatformPersonaOverride::Custom { name: next.clone() };
        }
    }
}

pub(in crate::web) fn validate_prompt_document_list(
    kind: &str,
    documents: &[PromptDocument],
) -> std::result::Result<(), ApiError> {
    if documents.len() > MAX_PROMPT_DOCUMENTS {
        return Err(ApiError::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            format!("at most {MAX_PROMPT_DOCUMENTS} {kind} documents are allowed"),
        ));
    }
    let mut names = HashSet::with_capacity(documents.len());
    let mut original_names = HashSet::with_capacity(documents.len());
    for document in documents {
        validate_prompt_document_name(&document.name, kind)?;
        if !names.insert(document.name.as_str()) {
            return Err(ApiError::new(
                StatusCode::BAD_REQUEST,
                format!("duplicate {kind} document: {}", document.name),
            ));
        }
        if document.content.chars().count() > MAX_PROMPT_DOCUMENT_CHARS
            || document.content.contains('\0')
        {
            return Err(ApiError::new(
                StatusCode::PAYLOAD_TOO_LARGE,
                format!("{kind} document is too large: {}", document.name),
            ));
        }
        for (field, value) in [
            ("avatar", document.avatar_path.as_deref()),
            ("board image", document.board_image_path.as_deref()),
        ] {
            if value.is_some_and(|path| {
                path.len() > 4_096 || path.contains('\0') || path.trim() != path
            }) {
                return Err(ApiError::new(
                    StatusCode::BAD_REQUEST,
                    format!("invalid {kind} {field} path: {}", document.name),
                ));
            }
        }
        for (field, value) in [
            ("board title", document.board_title.as_deref()),
            ("board subtitle", document.board_subtitle.as_deref()),
            (
                "composer placeholder",
                document.composer_placeholder.as_deref(),
            ),
        ] {
            if value.is_some_and(|text| {
                text.chars().count() > 200 || text.chars().any(char::is_control)
            }) {
                return Err(ApiError::new(
                    StatusCode::BAD_REQUEST,
                    format!("invalid {kind} {field}: {}", document.name),
                ));
            }
        }
        if let Some(prompts) = document.starter_prompts.as_deref() {
            if prompts.len() > 4
                || prompts
                    .iter()
                    .any(|text| text.chars().count() > 200 || text.chars().any(char::is_control))
            {
                return Err(ApiError::new(
                    StatusCode::BAD_REQUEST,
                    format!("invalid {kind} starter prompts: {}", document.name),
                ));
            }
        }
        if let Some(original) = document.original_name.as_deref() {
            validate_prompt_document_name(original, kind)?;
            if !original_names.insert(original) {
                return Err(ApiError::new(
                    StatusCode::BAD_REQUEST,
                    format!("duplicate original {kind} document: {original}"),
                ));
            }
        }
    }
    Ok(())
}

pub(in crate::web) fn validate_prompt_document_name(
    name: &str,
    kind: &str,
) -> std::result::Result<(), ApiError> {
    let valid = name == name.trim()
        && name.ends_with(".md")
        && name.len() <= 240
        && name.len() > 3
        && !name.starts_with('.')
        && !name.contains(['/', '\\'])
        && !name.chars().any(char::is_control)
        && FilePath::new(name)
            .file_name()
            .and_then(|value| value.to_str())
            == Some(name);
    if !valid {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            format!("invalid {kind} document name: {name}"),
        ));
    }
    Ok(())
}

pub(in crate::web) fn read_prompt_documents(
    config: &AppConfig,
    paths: &MiyuPaths,
) -> Result<PromptDocuments> {
    Ok(PromptDocuments {
        personas: read_prompt_document_dir(&config.prompts_dir_path(paths), true)?,
        identities: read_prompt_document_dir(&config.identities_dir_path(paths), false)?,
    })
}

pub(in crate::web) fn read_prompt_document_dir(
    dir: &FilePath,
    with_avatar_metadata: bool,
) -> Result<Vec<PromptDocument>> {
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut documents = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        if !name.ends_with(".md") {
            continue;
        }
        if with_avatar_metadata && name.eq_ignore_ascii_case("system-prompt.md") {
            continue;
        }
        let content = std::fs::read_to_string(entry.path())?;
        let metadata = with_avatar_metadata
            .then(|| read_prompt_metadata(&entry.path()))
            .flatten()
            .unwrap_or_default();
        documents.push(PromptDocument {
            original_name: Some(name.clone()),
            name,
            content,
            avatar_path: metadata.avatar_path,
            board_image_path: metadata.board_image_path,
            board_title: metadata.board_title,
            board_subtitle: metadata.board_subtitle,
            composer_placeholder: metadata.composer_placeholder,
            starter_prompts: metadata.starter_prompts,
        });
    }
    documents.sort_by(|left, right| left.name.cmp(&right.name));
    Ok(documents)
}

pub(in crate::web) fn read_prompt_metadata(path: &FilePath) -> Option<PersonaMetadata> {
    let sidecar = path.with_extension("json");
    let raw = std::fs::read_to_string(sidecar).ok()?;
    serde_json::from_str(&raw).ok()
}

pub(in crate::web) fn prompt_configuration_changed(
    current: &AppConfig,
    candidate: &AppConfig,
) -> bool {
    serde_json::to_value(&current.prompt).ok() != serde_json::to_value(&candidate.prompt).ok()
        || current.system_prompt_file != candidate.system_prompt_file
        || current.system_prompt != candidate.system_prompt
}

pub(in crate::web) fn prompt_documents_changed(
    current: &PromptDocuments,
    candidate: &PromptDocuments,
) -> bool {
    canonical_prompt_documents(&current.personas) != canonical_prompt_documents(&candidate.personas)
        || canonical_prompt_documents(&current.identities)
            != canonical_prompt_documents(&candidate.identities)
}

pub(in crate::web) fn canonical_prompt_documents(
    documents: &[PromptDocument],
) -> Vec<(String, String)> {
    let mut values = documents
        .iter()
        .map(|document| (document.name.clone(), document.content.clone()))
        .collect::<Vec<_>>();
    values.sort_by(|left, right| left.0.cmp(&right.0));
    values
}

pub(in crate::web) struct PersonaScopeBackup {
    pub(in crate::web) original: PathBuf,
    pub(in crate::web) staged: PathBuf,
    pub(in crate::web) destination: Option<PathBuf>,
}

pub(in crate::web) struct PersonaDbRenameGuard {
    pub(in crate::web) state: StateStore,
    pub(in crate::web) renames: Vec<(String, String)>,
    pub(in crate::web) committed: bool,
}

impl PersonaDbRenameGuard {
    pub(in crate::web) fn new(
        state: StateStore,
        changes: &[(String, Option<String>)],
    ) -> Result<Self> {
        let renames = changes
            .iter()
            .filter_map(|(old_name, new_name)| {
                let new_name = new_name.as_deref()?;
                let old_scope = crate::config::persona_scope_name(old_name);
                let new_scope = crate::config::persona_scope_name(new_name);
                (old_scope != new_scope).then_some((old_scope, new_scope))
            })
            .collect::<Vec<_>>();
        migrate_persona_db_scopes(&state, &renames)?;
        Ok(Self {
            state,
            renames,
            committed: false,
        })
    }

    pub(in crate::web) fn commit(&mut self) {
        self.committed = true;
    }
}

impl Drop for PersonaDbRenameGuard {
    fn drop(&mut self) {
        if self.committed || self.renames.is_empty() {
            return;
        }
        let reverse = self
            .renames
            .iter()
            .map(|(old, new)| (new.clone(), old.clone()))
            .collect::<Vec<_>>();
        let _ = migrate_persona_db_scopes(&self.state, &reverse);
    }
}

pub(in crate::web) fn migrate_persona_db_scopes(
    state: &StateStore,
    renames: &[(String, String)],
) -> Result<()> {
    let staged = renames
        .iter()
        .map(|(old, new)| {
            (
                old.clone(),
                format!("persona-migration-{}", random_token(18)),
                new.clone(),
            )
        })
        .collect::<Vec<_>>();
    for (staged_count, (old, temporary, _)) in staged.iter().enumerate() {
        if let Err(error) = state.rename_persona_scope(old, temporary) {
            for (old, temporary, _) in staged[..staged_count].iter().rev() {
                let _ = state.rename_persona_scope(temporary, old);
            }
            return Err(error);
        }
    }
    for (finalized, (_, temporary, new)) in staged.iter().enumerate() {
        if let Err(error) = state.rename_persona_scope(temporary, new) {
            for (_, temporary, new) in staged[..finalized].iter().rev() {
                let _ = state.rename_persona_scope(new, temporary);
            }
            for (old, temporary, _) in staged.iter().rev() {
                let _ = state.rename_persona_scope(temporary, old);
            }
            return Err(error);
        }
    }
    Ok(())
}

pub(in crate::web) fn apply_prompt_documents(
    current_config: &AppConfig,
    next_config: &AppConfig,
    current: &PromptDocuments,
    next: &PromptDocuments,
    paths: &MiyuPaths,
) -> Result<Vec<FileBackup>> {
    let mut mutations = HashMap::<PathBuf, Option<Vec<u8>>>::new();
    collect_prompt_file_mutations(
        &current.personas,
        &next.personas,
        &current_config.prompts_dir_path(paths),
        &next_config.prompts_dir_path(paths),
        &mut mutations,
        true,
    );
    collect_prompt_file_mutations(
        &current.identities,
        &next.identities,
        &current_config.identities_dir_path(paths),
        &next_config.identities_dir_path(paths),
        &mut mutations,
        false,
    );
    let backups = mutations
        .keys()
        .map(|path| FileBackup {
            path: path.clone(),
            content: std::fs::read(path).ok(),
        })
        .collect::<Vec<_>>();
    for (path, content) in mutations {
        let result = if let Some(content) = content {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&path, content)
        } else if path.exists() {
            std::fs::remove_file(&path)
        } else {
            Ok(())
        };
        if let Err(error) = result {
            restore_file_backups(&backups);
            return Err(error.into());
        }
    }
    Ok(backups)
}

pub(in crate::web) fn apply_persona_scope_changes(
    current_config: &AppConfig,
    next_config: &AppConfig,
    current: &PromptDocuments,
    next: &PromptDocuments,
    paths: &MiyuPaths,
) -> Result<Vec<PersonaScopeBackup>> {
    let changes = persona_document_changes(current, next);
    let mut backups = Vec::new();
    let stage_result = (|| -> Result<()> {
        for (change_index, (old_name, new_name)) in changes.iter().enumerate() {
            let old_paths = [
                current_config.persona_memory_data_dir(paths, old_name),
                current_config.persona_memory_state_dir(paths, old_name),
                current_config.persona_skills_dir(paths, old_name),
            ];
            let new_paths = new_name.as_ref().map(|name| {
                [
                    next_config.persona_memory_data_dir(paths, name),
                    next_config.persona_memory_state_dir(paths, name),
                    next_config.persona_skills_dir(paths, name),
                ]
            });
            for (scope_index, original) in old_paths.into_iter().enumerate() {
                if !original.exists() {
                    continue;
                }
                let parent = original
                    .parent()
                    .context("persona scope path has no parent")?;
                let staged = parent.join(format!(
                    ".miyu-web-scope-{}-{change_index}-{scope_index}",
                    random_token(10)
                ));
                std::fs::rename(&original, &staged)?;
                backups.push(PersonaScopeBackup {
                    original,
                    staged,
                    destination: new_paths.as_ref().map(|paths| paths[scope_index].clone()),
                });
            }
        }
        Ok(())
    })();
    if let Err(error) = stage_result {
        restore_persona_scope_backups(&backups);
        return Err(error);
    }

    let result = (|| -> Result<()> {
        for backup in &backups {
            let Some(destination) = &backup.destination else {
                continue;
            };
            if destination.exists() {
                anyhow::bail!(
                    "persona scope destination already exists: {}",
                    destination.display()
                );
            }
            if let Some(parent) = destination.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::rename(&backup.staged, destination)?;
        }
        Ok(())
    })();
    if let Err(error) = result {
        restore_persona_scope_backups(&backups);
        return Err(error);
    }
    Ok(backups)
}

pub(in crate::web) fn persona_document_changes(
    current: &PromptDocuments,
    next: &PromptDocuments,
) -> Vec<(String, Option<String>)> {
    let mut changes = Vec::new();
    for document in &current.personas {
        let represented = next.personas.iter().find(|next_document| {
            next_document.original_name.as_deref() == Some(document.name.as_str())
                || next_document.original_name.is_none() && next_document.name == document.name
        });
        match represented {
            Some(next_document) if next_document.name != document.name => {
                changes.push((document.name.clone(), Some(next_document.name.clone())));
            }
            None => changes.push((document.name.clone(), None)),
            _ => {}
        }
    }
    changes
}

pub(in crate::web) fn restore_persona_scope_backups(backups: &[PersonaScopeBackup]) {
    for backup in backups.iter().rev() {
        if let Some(destination) = &backup.destination {
            if destination.exists() && !backup.staged.exists() {
                let _ = std::fs::rename(destination, &backup.staged);
            }
        }
        if backup.staged.exists() {
            if let Some(parent) = backup.original.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let _ = std::fs::rename(&backup.staged, &backup.original);
        }
    }
}

pub(in crate::web) fn finalize_persona_scope_backups(backups: &[PersonaScopeBackup]) {
    for backup in backups {
        if backup.destination.is_none() && backup.staged.exists() {
            let _ = std::fs::remove_dir_all(&backup.staged);
        }
    }
}
