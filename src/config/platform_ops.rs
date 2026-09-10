//! 平台配置的校验、路由与连带改名。
//!
//! 供应商或模型改名时，所有引用它的地方都要跟着改
//! （`rename_platform_model_references` 一族）。漏一处不会报错，只会在某次群消
//! 息命中那条路由时才失败——所以这些函数是成组存在的。

use crate::config::*;

impl AppConfig {
    pub(crate) fn validate_platforms(&self) -> Result<()> {
        let command_prefix = &self.platforms.command_prefix;
        if command_prefix.is_empty()
            || command_prefix.trim() != command_prefix
            || command_prefix.chars().count() > MAX_PLATFORM_COMMAND_PREFIX_CHARS
            || command_prefix
                .chars()
                .any(|character| character.is_whitespace() || character.is_control())
        {
            bail!(
                "platforms.command_prefix must be a trimmed, non-empty value of at most {MAX_PLATFORM_COMMAND_PREFIX_CHARS} characters without whitespace"
            );
        }
        for command in self.platforms.commands.keys() {
            if command.is_empty()
                || command.len() > 64
                || !command.bytes().all(|byte| {
                    byte.is_ascii_lowercase()
                        || byte.is_ascii_digit()
                        || matches!(byte, b'_' | b'-')
                })
            {
                bail!(
                    "platforms.commands keys must be lowercase ASCII command ids of at most 64 bytes"
                );
            }
        }
        let qq = &self.platforms.qq;
        if qq.reverse_ws_port == 0 {
            bail!("platforms.qq.reverse_ws_port must be between 1 and 65535");
        }
        if let Err(error) = super::platform::parse_sleep_hours(&qq.sleep_hours) {
            bail!("platforms.qq.sleep_hours: {error}");
        }
        for (field, limits) in [
            ("session_limits", Some(qq.session_limits)),
            (
                "private_chats.session_limits",
                qq.private_chats.session_limits,
            ),
            ("group_chats.session_limits", qq.group_chats.session_limits),
        ] {
            if let Some(limits) = limits {
                validate_platform_session_limits(field, limits)?;
            }
        }
        qq.text_models.validate(&self.providers, "QQ text", false)?;
        qq.multimodal_models
            .validate(&self.providers, "QQ multimodal", true)?;
        qq.non_whitelist_text_models
            .validate(&self.providers, "QQ non-whitelist text", false)?;
        for (field, limit) in [
            (
                "private_chats.non_whitelist_rate_limit",
                qq.private_chats.non_whitelist_rate_limit,
            ),
            (
                "group_chats.whitelist_rate_limit",
                qq.group_chats.whitelist_rate_limit,
            ),
            (
                "group_chats.non_whitelist_rate_limit",
                qq.group_chats.non_whitelist_rate_limit,
            ),
        ] {
            if limit.window_seconds == 0 || limit.window_seconds > 86_400 {
                bail!("platforms.qq.{field}.window_seconds must be between 1 and 86400");
            }
        }
        for (field, ids) in [
            ("admin_users", qq.admin_users.as_slice()),
            (
                "private_chats.whitelist",
                qq.private_chats.whitelist.as_slice(),
            ),
            ("group_chats.whitelist", qq.group_chats.whitelist.as_slice()),
        ] {
            let mut seen = HashSet::with_capacity(ids.len());
            if ids.iter().any(|id| *id <= 0 || !seen.insert(*id)) {
                bail!("platforms.qq.{field} must contain unique positive QQ ids");
            }
        }
        let mut trigger_keywords = HashSet::with_capacity(qq.group_chats.trigger_keywords.len());
        for keyword in &qq.group_chats.trigger_keywords {
            if keyword.is_empty()
                || keyword.trim() != keyword
                || keyword.chars().count() > 128
                || keyword.chars().any(char::is_control)
                || !trigger_keywords.insert(keyword)
            {
                bail!(
                    "platforms.qq.group_chats.trigger_keywords must contain unique, trimmed, non-empty values of at most 128 characters"
                );
            }
        }
        let mut identities = HashSet::with_capacity(qq.conversations.len());
        for route in &qq.conversations {
            self.validate_platform_model_route(route)?;
            if let Some(limits) = route.session_limits {
                validate_platform_session_limits("conversations[].session_limits", limits)?;
            }
            if !identities.insert(route.identity()) {
                bail!(
                    "duplicate QQ conversation configuration: {} / {}",
                    route.conversation.kind.as_str(),
                    route.conversation.id
                );
            }
        }
        for (plugin_id, instance) in &qq.plugins {
            if plugin_id.trim().is_empty() || plugin_id.trim() != plugin_id {
                bail!("QQ plugin ids must be non-empty and trimmed");
            }
            if let Some((_, validate)) = PLATFORM_PLUGIN_VALIDATORS
                .iter()
                .find(|(id, _)| *id == plugin_id)
            {
                validate(instance)?;
            }
            if plugin_id == REAL_CONTEXT_PLUGIN_ID {
                let settings = RealContextPluginSettings::from_instance(instance)?;
                settings
                    .text_models
                    .validate(&self.providers, "real-context text", false)?;
                settings.affection_text_models.validate(
                    &self.providers,
                    "real-context affection text",
                    false,
                )?;
            }
            if plugin_id == QQ_GROUP_JOIN_APPROVAL_PLUGIN_ID {
                let settings = QqGroupJoinApprovalPluginSettings::from_instance(instance)?;
                settings.text_models.validate(
                    &self.providers,
                    "group-join-approval text",
                    false,
                )?;
            }
        }
        Ok(())
    }

    pub fn validate_platform_model_route(&self, route: &PlatformModelRoute) -> Result<()> {
        if !is_positive_decimal_id(&route.conversation.id) {
            let label = match route.conversation.kind {
                PlatformConversationKind::Private => "QQ id",
                PlatformConversationKind::Group => "group id",
            };
            bail!("QQ conversation id must be a positive decimal {label}");
        }
        if route.extra_prompt.chars().count() > 200_000 || route.extra_prompt.contains('\0') {
            bail!("QQ conversation extra_prompt is invalid or exceeds 200000 characters");
        }
        if let PlatformPersonaOverride::Custom { name } = &route.persona {
            let path = Path::new(name);
            if name.is_empty()
                || name.trim() != name
                || name.chars().count() > 255
                || !name.ends_with(".md")
                || name.chars().any(char::is_control)
                || path.file_name().and_then(|value| value.to_str()) != Some(name.as_str())
            {
                bail!("QQ conversation persona must be a safe Markdown persona filename");
            }
        }
        self.validate_platform_model_pool(
            route,
            "text_models",
            route.text_models.as_deref(),
            false,
        )?;
        self.validate_platform_model_pool(
            route,
            "multimodal_models",
            route.multimodal_models.as_deref(),
            true,
        )?;
        Ok(())
    }

    pub(crate) fn validate_platform_model_pool(
        &self,
        route: &PlatformModelRoute,
        field: &str,
        pool: Option<&[ActiveProviderModelConfig]>,
        require_multimodal: bool,
    ) -> Result<()> {
        let Some(pool) = pool else {
            return Ok(());
        };
        let mut seen = HashSet::with_capacity(pool.len());
        for entry in pool {
            if !seen.insert((entry.provider_id.as_str(), entry.model.as_str())) {
                bail!(
                    "duplicate {} model in platform route: {} / {}",
                    field,
                    entry.provider_id,
                    entry.model
                );
            }
            if !active_model_exists(&self.providers, entry) {
                bail!(
                    "unknown {} provider/model in QQ conversation {} / {}: {} / {}",
                    field,
                    route.conversation.kind.as_str(),
                    route.conversation.id,
                    entry.provider_id,
                    entry.model
                );
            }
            if require_multimodal
                && !self.model_supports_any_input(&entry.provider_id, &entry.model, &["image"])
            {
                bail!(
                    "platform route multimodal model does not declare image input: {} / {}",
                    entry.provider_id,
                    entry.model
                );
            }
        }
        Ok(())
    }

    pub fn platform_model_route(
        &self,
        kind: PlatformConversationKind,
        conversation_id: &str,
    ) -> Option<&PlatformModelRoute> {
        self.platforms.model_route(kind, conversation_id)
    }

    /// QQ default text pool: the platform-wide reference, `inherit` = global.
    pub fn qq_default_text_pool(&self) -> Option<Vec<ActiveProviderModelConfig>> {
        self.resolve_pool_ref(&self.platforms.qq.text_models, false, || {
            self.active_provider_models.clone()
        })
    }

    /// QQ default multimodal pool, `inherit` = the global multimodal pool.
    pub fn qq_default_multimodal_pool(&self) -> Option<Vec<ActiveProviderModelConfig>> {
        self.resolve_pool_ref(&self.platforms.qq.multimodal_models, true, || {
            self.active_multimodal_provider_models.clone()
        })
    }

    /// Non-whitelist text pool, `inherit` = the QQ default text pool.
    pub fn qq_non_whitelist_text_pool(&self) -> Option<Vec<ActiveProviderModelConfig>> {
        self.resolve_pool_ref(&self.platforms.qq.non_whitelist_text_models, false, || {
            self.qq_default_text_pool()
        })
    }

    /// The effective text pool for one conversation: a per-conversation
    /// route wins, then the non-whitelist pool when applicable, then the QQ
    /// default text pool. `None` keeps the legacy "active provider's default
    /// model" meaning.
    pub fn qq_text_model_pool(
        &self,
        kind: PlatformConversationKind,
        conversation_id: &str,
        use_non_whitelist_pool: bool,
    ) -> Option<Vec<ActiveProviderModelConfig>> {
        if let Some(route) = self.platform_model_route(kind, conversation_id) {
            if route.text_models.is_some() {
                return route.text_models.clone();
            }
            if route.text_models_inheritance == PlatformModelPoolInheritance::Global {
                return self.active_provider_models.clone();
            }
        }
        if use_non_whitelist_pool {
            return self.qq_non_whitelist_text_pool();
        }
        self.qq_default_text_pool()
    }

    pub fn qq_multimodal_model_pool(
        &self,
        kind: PlatformConversationKind,
        conversation_id: &str,
    ) -> Option<Vec<ActiveProviderModelConfig>> {
        if let Some(route) = self.platform_model_route(kind, conversation_id) {
            if route.multimodal_models.is_some() {
                return route.multimodal_models.clone();
            }
            if route.multimodal_models_inheritance == PlatformModelPoolInheritance::Global {
                return self.active_multimodal_provider_models.clone();
            }
        }
        self.qq_default_multimodal_pool()
    }

    pub fn apply_qq_conversation_persona(
        &mut self,
        kind: PlatformConversationKind,
        conversation_id: &str,
    ) {
        let persona = self
            .platform_model_route(kind, conversation_id)
            .map(|route| route.persona.clone())
            .unwrap_or_default();
        match persona {
            PlatformPersonaOverride::Inherit => {}
            PlatformPersonaOverride::Miyu => self.prompt.active_persona.clear(),
            PlatformPersonaOverride::Custom { name } => self.prompt.active_persona = name,
        }
    }

    pub fn normalize_platform_model_routes(&mut self) {
        self.platforms.normalize_model_routes();
    }

    pub fn prune_platform_model_routes(&mut self) {
        self.platforms.prune_model_references(&self.providers);
    }

    pub fn rename_platform_provider_references(&mut self, old_id: &str, new_id: &str) {
        self.platforms.rename_provider_references(old_id, new_id);
    }

    pub fn rename_platform_model_references(&mut self, provider_id: &str, old: &str, new: &str) {
        self.platforms
            .rename_model_references(provider_id, old, new);
    }
}
