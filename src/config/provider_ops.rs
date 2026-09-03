//! 供应商与模型池的增删改。
//!
//! 模型的能力（视觉、嵌入、思考）决定它能进哪个池，所以每个 `toggle_*` 都要先
//! 判定能力再动池子。判定要求池里**每个**模型都支持——池内是随机选的，有一个
//! 不支持就会随机失败。

use crate::config::*;

impl AppConfig {
    pub fn rename_provider_references(&mut self, old_id: &str, new_id: &str) {
        if old_id == new_id || old_id.is_empty() || new_id.is_empty() {
            return;
        }
        if self.active_provider == old_id {
            self.active_provider = new_id.to_string();
        }
        for entries in [
            self.active_provider_models.as_mut(),
            self.active_multimodal_provider_models.as_mut(),
        ]
        .into_iter()
        .flatten()
        {
            rename_provider_in_pool(entries, old_id, new_id);
        }
        for tier in ModelTier::ALL {
            rename_provider_in_pool(self.subagent_tiers.pool_mut(tier), old_id, new_id);
        }
        self.platforms.rename_provider_references(old_id, new_id);
        if self.plugins.vision.vision_provider_id == old_id {
            self.plugins.vision.vision_provider_id = new_id.to_string();
        }
        if self.plugins.knowledge_base.embedding_provider_id == old_id {
            self.plugins.knowledge_base.embedding_provider_id = new_id.to_string();
        }
        if self.embedding.provider_id == old_id {
            self.embedding.provider_id = new_id.to_string();
        }
    }

    /// Removes references after a provider has been deleted from `providers`.
    pub fn remove_provider_references(&mut self, provider_id: &str) {
        retain_provider_pool(&mut self.active_provider_models, provider_id);
        retain_provider_pool(&mut self.active_multimodal_provider_models, provider_id);
        for tier in ModelTier::ALL {
            self.subagent_tiers
                .pool_mut(tier)
                .retain(|entry| entry.provider_id != provider_id);
        }
        self.platforms.remove_provider_references(provider_id);
        if self.plugins.vision.vision_provider_id == provider_id {
            self.plugins.vision.vision_provider_id.clear();
            self.plugins.vision.vision_model.clear();
        }
        if self.plugins.knowledge_base.embedding_provider_id == provider_id {
            self.plugins.knowledge_base.embedding_provider_id.clear();
            self.plugins.knowledge_base.embedding_model.clear();
        }
        if self.embedding.provider_id == provider_id {
            self.embedding.provider_id.clear();
            self.embedding.model.clear();
        }
        if self.active_provider == provider_id {
            self.active_provider = self
                .active_provider_models
                .as_ref()
                .and_then(|pool| pool.first())
                .map(|entry| entry.provider_id.clone())
                .or_else(|| {
                    self.providers
                        .iter()
                        .find(|provider| !provider.default_model.trim().is_empty())
                        .or_else(|| self.providers.first())
                        .map(|provider| provider.id.clone())
                })
                .unwrap_or_default();
        }
    }

    /// Reconciles every model reference with the current provider models and
    /// input capabilities after an editor changes model metadata.
    pub fn prune_model_references(&mut self) {
        self.prune_stale_active_provider_models();
        retain_nonempty_pool(&mut self.active_provider_models);
        retain_nonempty_pool(&mut self.active_multimodal_provider_models);
        self.prune_subagent_tiers();
        self.prune_platform_model_routes();

        let vision_provider_id = self.plugins.vision.vision_provider_id.trim();
        if !vision_provider_id.is_empty() {
            let vision_model = self.plugins.vision.vision_model.trim();
            let valid = self
                .provider(Some(vision_provider_id))
                .ok()
                .map(|provider| {
                    let model = if vision_model.is_empty() {
                        provider.default_model.as_str()
                    } else {
                        vision_model
                    };
                    provider
                        .input_modalities(model)
                        .is_some_and(|modalities| modalities.iter().any(|item| item == "image"))
                })
                .unwrap_or(false);
            if !valid {
                self.plugins.vision.vision_provider_id.clear();
                self.plugins.vision.vision_model.clear();
            }
        }

        let kb_provider_id = self.plugins.knowledge_base.embedding_provider_id.trim();
        if !kb_provider_id.is_empty() && self.provider(Some(kb_provider_id)).is_err() {
            self.plugins.knowledge_base.embedding_provider_id.clear();
            self.plugins.knowledge_base.embedding_model.clear();
        }
    }

    pub fn provider(&self, id: Option<&str>) -> Result<&ProviderConfig> {
        let target = id.unwrap_or(&self.active_provider);
        self.providers
            .iter()
            .find(|provider| provider.id == target)
            .with_context(|| format!("provider not found: {target}"))
    }

    pub fn provider_model_choices(&self) -> Vec<ProviderModelChoice> {
        self.providers
            .iter()
            .filter(|provider| provider.enabled)
            .flat_map(|provider| {
                let models =
                    if provider.models.is_empty() && !provider.default_model.trim().is_empty() {
                        vec![provider.default_model.clone()]
                    } else {
                        provider.models.clone()
                    };
                models
                    .into_iter()
                    .filter(|model| !model.trim().is_empty())
                    .map(|model| ProviderModelChoice {
                        provider_id: provider.id.clone(),
                        provider_name: provider.display_name.clone(),
                        model,
                    })
                    .collect::<Vec<_>>()
            })
            .collect()
    }

    /// Embedding models are excluded: they produce vectors, not replies, and
    /// picking one here is always a mistake. The multimodal list derives from
    /// this one, so filtering here covers both.
    pub fn text_provider_model_choices(&self) -> Vec<ProviderModelChoice> {
        self.providers
            .iter()
            // 未启用的供应商(内置 Claude Code 默认关)不进任何选择器。
            .filter(|provider| provider.enabled)
            .flat_map(|provider| {
                provider
                    .models
                    .iter()
                    .filter(|model| !model.trim().is_empty())
                    .filter(|model| !Self::model_is_embedding(provider, model))
                    .map(|model| ProviderModelChoice {
                        provider_id: provider.id.clone(),
                        provider_name: provider.display_name.clone(),
                        model: model.clone(),
                    })
                    .collect::<Vec<_>>()
            })
            .collect()
    }

    /// A model marked as producing vectors rather than chat. Stored beside the
    /// input modalities because it answers the same question — what the model
    /// is for.
    pub fn model_is_embedding(provider: &ProviderConfig, model: &str) -> bool {
        provider
            .model_modalities
            .get(model)
            .is_some_and(|modalities| modalities.iter().any(|item| item == EMBEDDING_MODALITY))
    }

    /// 会话模型覆盖里**还解析得动**的那些条目;一个都不剩时返回 `None`。
    ///
    /// 覆盖是会话级的快照,而供应商的模型列表会变。指向已删除模型的覆盖会让
    /// `active_provider_model_choices` 静默筛空,进而 `llm_endpoints` 报
    /// "no active provider/model endpoint is configured" 且错误列表是空的
    /// ——用户连 REPL 都进不去,更没机会用 `/model` 换一个,只能去改配置或改
    /// 库(08-28 实录:opencodego/glm-5.3-flash 被移出 models 之后
    /// `miyu normal` 直接起不来)。
    ///
    /// 调用方据此退回全局池:宁可用错模型,也别让整个入口打不开。
    pub fn usable_model_override(
        &self,
        models: Vec<ActiveProviderModelConfig>,
    ) -> Option<Vec<ActiveProviderModelConfig>> {
        let usable = models
            .into_iter()
            .filter(|active| {
                self.provider(Some(active.provider_id.trim()))
                    .is_ok_and(|provider| provider.has_configured_model(active.model.trim()))
            })
            .collect::<Vec<_>>();
        (!usable.is_empty()).then_some(usable)
    }

    pub fn active_provider_model_choices(&self) -> Vec<ProviderModelChoice> {
        match &self.active_provider_models {
            None => self
                .provider(None)
                .ok()
                .filter(|provider| !provider.default_model.trim().is_empty())
                .map(|provider| {
                    vec![ProviderModelChoice {
                        provider_id: provider.id.clone(),
                        provider_name: provider.display_name.clone(),
                        model: provider.default_model.clone(),
                    }]
                })
                .unwrap_or_default(),
            Some(active_models) => active_models
                .iter()
                .filter_map(|active| {
                    let provider = self.provider(Some(active.provider_id.trim())).ok()?;
                    let model = active.model.trim();
                    provider
                        .has_configured_model(model)
                        .then(|| ProviderModelChoice {
                            provider_id: provider.id.clone(),
                            provider_name: provider.display_name.clone(),
                            model: model.to_string(),
                        })
                })
                .collect(),
        }
    }

    pub fn multimodal_provider_model_choices(&self) -> Vec<ProviderModelChoice> {
        self.text_provider_model_choices()
            .into_iter()
            .filter(|choice| {
                self.model_supports_any_input(&choice.provider_id, &choice.model, &["image"])
            })
            .collect()
    }

    pub fn active_multimodal_provider_model_choices(&self) -> Vec<ProviderModelChoice> {
        match &self.active_multimodal_provider_models {
            Some(active_models) => active_models
                .iter()
                .filter_map(|active| {
                    let provider = self.provider(Some(active.provider_id.trim())).ok()?;
                    let model = active.model.trim();
                    (provider.has_configured_model(model)
                        && provider.input_modalities(model).is_some_and(|modalities| {
                            modalities.iter().any(|item| item == "image")
                        }))
                    .then(|| ProviderModelChoice {
                        provider_id: provider.id.clone(),
                        provider_name: provider.display_name.clone(),
                        model: model.to_string(),
                    })
                })
                .collect(),
            None => Vec::new(),
        }
    }

    pub fn is_active_multimodal_provider_model(&self, provider_id: &str, model: &str) -> bool {
        self.active_multimodal_provider_models
            .as_ref()
            .map(|active_models| {
                active_models
                    .iter()
                    .any(|active| active.provider_id == provider_id && active.model == model)
            })
            .unwrap_or(false)
    }

    pub fn remove_active_model_references(&mut self, provider_id: &str, model: &str) {
        if let Some(active_models) = &mut self.active_provider_models {
            active_models
                .retain(|active| !(active.provider_id == provider_id && active.model == model));
        }
        if let Some(active_models) = &mut self.active_multimodal_provider_models {
            active_models
                .retain(|active| !(active.provider_id == provider_id && active.model == model));
        }
        // A model gone from the text models must leave every tier pool too.
        for tier in ModelTier::ALL {
            self.subagent_tiers
                .pool_mut(tier)
                .retain(|entry| !(entry.provider_id == provider_id && entry.model == model));
        }
        self.platforms.remove_model_references(provider_id, model);
        if self.plugins.vision.vision_provider_id == provider_id
            && self.plugins.vision.vision_model == model
        {
            self.plugins.vision.vision_provider_id.clear();
            self.plugins.vision.vision_model.clear();
        }
        if self.plugins.knowledge_base.embedding_provider_id == provider_id
            && self.plugins.knowledge_base.embedding_model == model
        {
            self.plugins.knowledge_base.embedding_provider_id.clear();
            self.plugins.knowledge_base.embedding_model.clear();
        }
        if self.embedding.provider_id == provider_id && self.embedding.model == model {
            self.embedding.provider_id.clear();
            self.embedding.model.clear();
        }
        retain_nonempty_pool(&mut self.active_provider_models);
        retain_nonempty_pool(&mut self.active_multimodal_provider_models);
    }

    pub fn toggle_active_multimodal_provider_model(
        &mut self,
        provider_id: &str,
        model: &str,
    ) -> Result<bool> {
        if model.trim().is_empty() {
            bail!("model cannot be empty");
        }
        if let Some(active_models) = &mut self.active_multimodal_provider_models {
            if let Some(index) = active_models
                .iter()
                .position(|active| active.provider_id == provider_id && active.model == model)
            {
                active_models.remove(index);
                return Ok(false);
            }
        }
        let provider = self.provider(Some(provider_id))?;
        if !provider.has_configured_model(model) {
            bail!("model is not configured for provider {provider_id}: {model}");
        }
        if !provider
            .input_modalities(model)
            .is_some_and(|modalities| modalities.iter().any(|item| item == "image"))
        {
            bail!("multimodal model does not declare image input: {provider_id} / {model}");
        }
        let active_models = self
            .active_multimodal_provider_models
            .get_or_insert_with(Vec::new);
        active_models.push(ActiveProviderModelConfig {
            provider_id: provider_id.to_string(),
            model: model.to_string(),
        });
        Ok(true)
    }

    pub fn model_supports_any_input(
        &self,
        provider_id: &str,
        model: &str,
        inputs: &[&str],
    ) -> bool {
        self.provider(Some(provider_id))
            .ok()
            .and_then(|provider| provider.input_modalities(model))
            .map(|modalities| {
                modalities
                    .iter()
                    .any(|m| inputs.iter().any(|input| m == input))
            })
            .unwrap_or(false)
    }

    pub fn vision_provider_choice(&self) -> Result<(String, String)> {
        let vision = &self.plugins.vision;
        if !vision.vision_provider_id.trim().is_empty() {
            let provider_id = vision.vision_provider_id.trim().to_string();
            let provider = self.provider(Some(&provider_id))?;
            let model = if vision.vision_model.trim().is_empty() {
                provider.default_model.clone()
            } else {
                vision.vision_model.trim().to_string()
            };
            if !provider
                .input_modalities(&model)
                .is_some_and(|modalities| modalities.iter().any(|item| item == "image"))
            {
                bail!("vision model does not declare image input: {provider_id} / {model}");
            }
            return Ok((provider_id, model));
        }
        if let Some(active) = self.active_multimodal_provider_models.as_ref() {
            if let Some(choice) = self
                .active_multimodal_provider_model_choices()
                .into_iter()
                .find(|choice| {
                    self.model_supports_any_input(&choice.provider_id, &choice.model, &["image"])
                })
            {
                return Ok((choice.provider_id, choice.model));
            }
            if !active.is_empty() {
                bail!("the configured multimodal model pool has no image-capable model");
            }
        }
        Ok((
            OPENCODE_PROVIDER_ID.to_string(),
            OPENCODE_DEFAULT_VISION_MODEL.to_string(),
        ))
    }

    /// A tier pool's usable model choices: configured entries filtered to
    /// models that still exist under their provider (entries whose model
    /// was removed from the text models are ignored, mirroring
    /// `active_provider_model_choices`).
    pub fn subagent_tier_choices(&self, tier: ModelTier) -> Vec<ProviderModelChoice> {
        self.subagent_tiers
            .pool(tier)
            .iter()
            .filter_map(|entry| {
                let provider = self.provider(Some(entry.provider_id.trim())).ok()?;
                let model = entry.model.trim();
                provider
                    .has_configured_model(model)
                    .then(|| ProviderModelChoice {
                        provider_id: provider.id.clone(),
                        provider_name: provider.display_name.clone(),
                        model: model.to_string(),
                    })
            })
            .collect()
    }

    pub fn is_subagent_tier_model(&self, tier: ModelTier, provider_id: &str, model: &str) -> bool {
        self.subagent_tiers
            .pool(tier)
            .iter()
            .any(|entry| entry.provider_id == provider_id && entry.model == model)
    }

    /// Adds/removes a model in a tier pool. Returns `true` when the model
    /// is in the pool after the call.
    pub fn toggle_subagent_tier_model(
        &mut self,
        tier: ModelTier,
        provider_id: &str,
        model: &str,
    ) -> Result<bool> {
        if model.trim().is_empty() {
            bail!("model cannot be empty");
        }
        self.provider(Some(provider_id))?;
        let pool = self.subagent_tiers.pool_mut(tier);
        if let Some(index) = pool
            .iter()
            .position(|entry| entry.provider_id == provider_id && entry.model == model)
        {
            pool.remove(index);
            Ok(false)
        } else {
            pool.push(ActiveProviderModelConfig {
                provider_id: provider_id.to_string(),
                model: model.to_string(),
            });
            Ok(true)
        }
    }

    /// Drops tier pool entries whose model no longer exists among the
    /// configured text models (a model removed from a provider must also
    /// leave every tier pool).
    pub fn prune_subagent_tiers(&mut self) {
        for tier in ModelTier::ALL {
            let providers = &self.providers;
            self.subagent_tiers
                .pool_mut(tier)
                .retain(|entry| active_model_exists(providers, entry));
        }
    }

    pub fn is_active_provider_model(&self, provider_id: &str, model: &str) -> bool {
        match &self.active_provider_models {
            None => self
                .provider(None)
                .map(|provider| provider.id == provider_id && provider.default_model == model)
                .unwrap_or(false),
            Some(active_models) => active_models
                .iter()
                .any(|active| active.provider_id == provider_id && active.model == model),
        }
    }

    pub fn toggle_active_provider_model(&mut self, provider_id: &str, model: &str) -> Result<bool> {
        if model.trim().is_empty() {
            bail!("model cannot be empty");
        }
        self.provider(Some(provider_id))?;
        if self.active_provider_models.is_none() {
            self.active_provider_models = Some(
                self.active_provider_model_choices()
                    .into_iter()
                    .map(|choice| ActiveProviderModelConfig {
                        provider_id: choice.provider_id,
                        model: choice.model,
                    })
                    .collect(),
            );
        }
        let active_models = self.active_provider_models.get_or_insert_with(Vec::new);
        if let Some(index) = active_models
            .iter()
            .position(|active| active.provider_id == provider_id && active.model == model)
        {
            active_models.remove(index);
            return Ok(false);
        }
        active_models.push(ActiveProviderModelConfig {
            provider_id: provider_id.to_string(),
            model: model.to_string(),
        });
        Ok(true)
    }

    pub fn set_active_provider_models(
        &mut self,
        models: &[ActiveProviderModelConfig],
    ) -> Result<()> {
        if models.is_empty() {
            bail!("at least one model endpoint must remain active");
        }
        let choices = self.provider_model_choices();
        let mut seen = std::collections::HashSet::with_capacity(models.len());
        for model in models {
            if model.provider_id.trim().is_empty() || model.model.trim().is_empty() {
                bail!("provider_id and model cannot be empty");
            }
            if !seen.insert((&model.provider_id, &model.model)) {
                bail!(
                    "duplicate active provider/model: {} / {}",
                    model.provider_id,
                    model.model
                );
            }
            if !choices.iter().any(|choice| {
                choice.provider_id == model.provider_id && choice.model == model.model
            }) {
                bail!(
                    "unknown configured provider/model: {} / {}",
                    model.provider_id,
                    model.model
                );
            }
        }
        self.active_provider_models = Some(models.to_vec());
        Ok(())
    }

    pub fn set_active_provider_model(&mut self, provider_id: &str, model: &str) -> Result<()> {
        let provider = self
            .providers
            .iter_mut()
            .find(|provider| provider.id == provider_id)
            .with_context(|| format!("provider not found: {provider_id}"))?;
        if model.trim().is_empty() {
            bail!("model cannot be empty");
        }
        self.active_provider = provider.id.clone();
        provider.default_model = model.to_string();
        self.active_provider_models = Some(vec![ActiveProviderModelConfig {
            provider_id: provider.id.clone(),
            model: model.to_string(),
        }]);
        if !provider.models.iter().any(|item| item == model) {
            provider.models.push(model.to_string());
        }
        Ok(())
    }

    pub fn remove_active_provider_model(&mut self, provider_id: &str, model: &str) -> Result<()> {
        let provider_index = self
            .providers
            .iter()
            .position(|provider| provider.id == provider_id)
            .with_context(|| format!("provider not found: {provider_id}"))?;
        if model.trim().is_empty() {
            bail!("model cannot be empty");
        }
        {
            let provider = &mut self.providers[provider_index];
            provider.models.retain(|item| item != model);
            provider.model_context_window.remove(model);
            provider.model_modalities.remove(model);
            if provider.default_model == model {
                provider.default_model = provider.models.first().cloned().unwrap_or_default();
            }
        }
        self.remove_active_model_references(provider_id, model);
        Ok(())
    }

    pub fn active_context_window(&self) -> Result<Option<usize>> {
        Ok(self
            .active_context_window_with_source()?
            .map(|(window, _)| window))
    }

    /// 同上，外带**这个数是哪来的**。
    ///
    /// 池子里只要有一个模型的窗口是猜的，整个池子就算猜的：显示的是各模型的
    /// 最小值，而那个「猜的」成员真实窗口要是更小，最小值本身就是错的。
    pub fn active_context_window_with_source(
        &self,
    ) -> Result<Option<(usize, ContextWindowSource)>> {
        let choices = self.active_provider_model_choices();
        if choices.is_empty() {
            return Ok(None);
        }
        let mut windows = Vec::new();
        let mut assumed = false;
        for choice in choices {
            let Some((window, source)) =
                self.context_window_with_source(&choice.provider_id, &choice.model)?
            else {
                return Ok(None);
            };
            assumed |= source == ContextWindowSource::Assumed;
            windows.push(window);
        }
        let source = if assumed {
            ContextWindowSource::Assumed
        } else {
            ContextWindowSource::Known
        };
        Ok(windows.into_iter().min().map(|window| (window, source)))
    }

    pub fn context_window_for_provider_model(
        &self,
        provider_id: &str,
        model: &str,
    ) -> Result<Option<usize>> {
        Ok(self
            .context_window_with_source(provider_id, model)?
            .map(|(window, _)| window))
    }

    /// 解析一个模型的上下文窗口，并说清楚**这个数是哪来的**。
    ///
    /// 三级兜底里前两级有出处，第三级 `context.default_context_window` 是个通用
    /// 常数（默认 168000），跟这个模型没有任何关系——它只是让溢出判定有个数可
    /// 用，不是「这个模型的窗口就是 168k」。
    ///
    /// 分开报出来是因为两种用途要的东西相反：溢出/压缩必须拿到一个数才能干活，
    /// 而 footer 把猜的数渲染成 `47k/168k(28%)` 就是在撒谎——用户没法分辨那个
    /// 百分比是量出来的还是编的。同一个道理见 `render::usage::cache_percent`：
    /// 供应商没说过缓存，就不能渲染成 0%。
    pub fn context_window_with_source(
        &self,
        provider_id: &str,
        model: &str,
    ) -> Result<Option<(usize, ContextWindowSource)>> {
        let provider = self.provider(Some(provider_id))?;
        if let Some(window) = provider
            .model_context_window
            .get(model)
            .copied()
            .filter(|&w| w > 0)
        {
            return Ok(Some((window, ContextWindowSource::Known)));
        }
        if let Some(window) = crate::models_cache::context_window(provider_id, model) {
            return Ok(Some((window as usize, ContextWindowSource::Known)));
        }
        Ok((self.context.default_context_window > 0).then_some((
            self.context.default_context_window,
            ContextWindowSource::Assumed,
        )))
    }

    pub fn upsert_provider(&mut self, provider: ProviderConfig) {
        self.active_provider = provider.id.clone();
        self.active_provider_models = if provider.default_model.trim().is_empty() {
            Some(vec![ActiveProviderModelConfig {
                provider_id: OPENCODE_PROVIDER_ID.to_string(),
                model: OPENCODE_DEFAULT_CHAT_MODEL.to_string(),
            }])
        } else {
            Some(vec![ActiveProviderModelConfig {
                provider_id: provider.id.clone(),
                model: provider.default_model.clone(),
            }])
        };
        match self
            .providers
            .iter()
            .position(|item| item.id == provider.id)
        {
            Some(index) => self.providers[index] = provider,
            None => self.providers.push(provider),
        }
    }
}

impl AppConfig {
    /// 内置 Claude Code 特殊供应商是否启用(订阅中转的总开关)。
    pub fn claude_code_enabled(&self) -> bool {
        self.providers
            .iter()
            .any(|provider| provider.is_claude_code() && provider.enabled)
    }

    /// 内置 Antigravity 特殊供应商是否启用(agy CLI 中转的总开关)。
    pub fn antigravity_enabled(&self) -> bool {
        self.providers
            .iter()
            .any(|provider| provider.is_antigravity() && provider.enabled)
    }
}
