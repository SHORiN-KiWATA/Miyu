//! 模型池引用：平台与插件侧「用哪个池」的统一表示。
//!
//! 一处配置要么指向一个有名字的池（`inherit` = 上一层、`global` = 全局池、
//! 或某个分级档位），要么自己带一份显式模型列表（长度 1 就是「指定模型」）。
//! 模型只在全局池与分级池里被枚举；引用永远不会因为删模型而悬空，只有显式
//! 列表需要剪枝。旧配置里的数组照旧解析成显式列表，缺省 / `null` 解析成
//! `inherit`，零迁移。
use crate::config::*;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(untagged)]
pub enum ModelPoolRef {
    /// `inherit` / `global` / a tier label (see [`ModelTier::from_str`]).
    Named(String),
    /// An explicit list owned by this slot alone.
    Models(Vec<ActiveProviderModelConfig>),
}

impl Default for ModelPoolRef {
    fn default() -> Self {
        Self::Named(INHERIT_POOL_LABEL.to_string())
    }
}

impl<'de> Deserialize<'de> for ModelPoolRef {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Raw {
            Named(String),
            Models(Vec<ActiveProviderModelConfig>),
        }
        Ok(match Option::<Raw>::deserialize(deserializer)? {
            None => Self::default(),
            Some(Raw::Named(name)) => Self::Named(name),
            Some(Raw::Models(models)) => Self::Models(models),
        })
    }
}

impl ModelPoolRef {
    pub fn inherit() -> Self {
        Self::default()
    }

    pub fn global() -> Self {
        Self::Named(GLOBAL_POOL_LABEL.to_string())
    }

    pub fn tier(tier: ModelTier) -> Self {
        Self::Named(tier.label().to_string())
    }

    pub fn models(models: Vec<ActiveProviderModelConfig>) -> Self {
        let mut value = Self::Models(models);
        value.normalize();
        value
    }

    /// `inherit`, or an explicit list that is empty (same meaning).
    pub fn is_inherit(&self) -> bool {
        match self {
            Self::Named(name) => name.trim() == INHERIT_POOL_LABEL,
            Self::Models(models) => models.is_empty(),
        }
    }

    pub fn is_global(&self) -> bool {
        matches!(self, Self::Named(name) if name.trim() == GLOBAL_POOL_LABEL)
    }

    pub fn tier_ref(&self) -> Option<ModelTier> {
        match self {
            Self::Named(name) => ModelTier::from_str(name),
            Self::Models(_) => None,
        }
    }

    /// The explicit list, when this slot owns one (non-empty).
    pub fn explicit_models(&self) -> Option<&[ActiveProviderModelConfig]> {
        match self {
            Self::Models(models) if !models.is_empty() => Some(models),
            _ => None,
        }
    }

    pub fn explicit_models_mut(&mut self) -> Option<&mut Vec<ActiveProviderModelConfig>> {
        match self {
            Self::Models(models) if !models.is_empty() => Some(models),
            _ => None,
        }
    }

    /// Trim, dedupe, and canonicalize: an empty list becomes `inherit`, an
    /// old tier alias (`balanced` / `strong`) becomes its current name.
    pub fn normalize(&mut self) {
        match self {
            Self::Named(name) => {
                let trimmed = name.trim();
                *name = match ModelTier::from_str(trimmed) {
                    Some(tier) => tier.label().to_string(),
                    None => trimmed.to_string(),
                };
                if name.is_empty() {
                    *self = Self::default();
                }
            }
            Self::Models(models) => {
                let mut wrapped = Some(std::mem::take(models));
                normalize_route_pool(&mut wrapped);
                match wrapped {
                    Some(entries) => *models = entries,
                    None => *self = Self::default(),
                }
            }
        }
    }

    /// Named values must be `inherit`, `global`, or a tier (tiers are text
    /// pools, so a multimodal slot may not reference one); explicit lists
    /// must be unique, existing, and image-capable where required.
    pub fn validate(
        &self,
        providers: &[ProviderConfig],
        label: &str,
        require_image: bool,
    ) -> Result<()> {
        match self {
            Self::Named(name) => {
                let name = name.trim();
                if name == INHERIT_POOL_LABEL || name == GLOBAL_POOL_LABEL {
                    return Ok(());
                }
                match ModelTier::from_str(name) {
                    Some(_) if require_image => bail!(
                        "{label} pool cannot reference a tier ({name}): tiers hold text models; use inherit, global, or an explicit list"
                    ),
                    Some(_) => Ok(()),
                    None => bail!(
                        "{label} pool references unknown pool '{name}'; accepted: inherit, global, lite, cheap, standard, flagship, or a model list"
                    ),
                }
            }
            Self::Models(models) => {
                validate_unique_existing_pool(providers, label, models, require_image)
            }
        }
    }

    /// Drop explicit entries whose model no longer exists (or lost image
    /// support where required). Named references never need pruning.
    pub fn prune(&mut self, providers: &[ProviderConfig], require_image: bool) {
        if let Self::Models(models) = self {
            models.retain(|model| {
                active_model_exists(providers, model)
                    && (!require_image || active_model_supports_image(providers, model))
            });
        }
        self.normalize();
    }

    pub fn remove_model(&mut self, provider_id: &str, model: &str) {
        if let Self::Models(models) = self {
            models.retain(|entry| !(entry.provider_id == provider_id && entry.model == model));
        }
        self.normalize();
    }

    pub fn remove_provider(&mut self, provider_id: &str) {
        if let Self::Models(models) = self {
            models.retain(|entry| entry.provider_id != provider_id);
        }
        self.normalize();
    }

    pub fn rename_provider(&mut self, old_id: &str, new_id: &str) {
        if let Self::Models(models) = self {
            rename_provider_in_pool(models, old_id, new_id);
        }
        self.normalize();
    }

    pub fn rename_model(&mut self, provider_id: &str, old: &str, new: &str) {
        if let Self::Models(models) = self {
            for entry in models {
                if entry.provider_id == provider_id && entry.model == old {
                    entry.model = new.to_string();
                }
            }
        }
        self.normalize();
    }

    /// Explicit entries for catalog/metadata scans; named refs contribute none.
    pub fn explicit_entries(&self) -> &[ActiveProviderModelConfig] {
        match self {
            Self::Models(models) => models,
            Self::Named(_) => &[],
        }
    }
}

impl AppConfig {
    /// Resolve a pool reference into the concrete list a client is built
    /// from. `None` keeps the legacy "follow the active provider's default
    /// model" meaning of an unset global pool. `parent` supplies the
    /// inherited pool; a tier whose pool is empty falls back to the global
    /// pool (user decision 2026-09-05: never to a neighbouring tier).
    pub fn resolve_pool_ref(
        &self,
        pool: &ModelPoolRef,
        multimodal: bool,
        parent: impl FnOnce() -> Option<Vec<ActiveProviderModelConfig>>,
    ) -> Option<Vec<ActiveProviderModelConfig>> {
        if pool.is_inherit() {
            return parent();
        }
        match pool {
            ModelPoolRef::Models(models) => Some(models.clone()),
            ModelPoolRef::Named(_) if pool.is_global() => self.global_pool(multimodal),
            ModelPoolRef::Named(_) => match pool.tier_ref() {
                Some(tier) => {
                    let entries = self.tier_entries(tier);
                    if entries.is_empty() {
                        self.global_pool(multimodal)
                    } else {
                        Some(entries)
                    }
                }
                None => self.global_pool(multimodal),
            },
        }
    }

    fn global_pool(&self, multimodal: bool) -> Option<Vec<ActiveProviderModelConfig>> {
        if multimodal {
            self.active_multimodal_provider_models.clone()
        } else {
            self.active_provider_models.clone()
        }
    }

    /// A tier's usable members as pool entries.
    pub fn tier_entries(&self, tier: ModelTier) -> Vec<ActiveProviderModelConfig> {
        self.tier_choices(tier)
            .into_iter()
            .map(|choice| ActiveProviderModelConfig {
                provider_id: choice.provider_id,
                model: choice.model,
            })
            .collect()
    }
}

/// Structural check for plugin settings, which validate before providers are
/// at hand: named values must be known, explicit lists unique and non-empty
/// in every field. Existence is checked later by `AppConfig::validate`.
pub(crate) fn validate_pool_ref_shape(pool: &ModelPoolRef, field: &str) -> Result<()> {
    match pool {
        ModelPoolRef::Named(name) => {
            let name = name.trim();
            if name == INHERIT_POOL_LABEL
                || name == GLOBAL_POOL_LABEL
                || ModelTier::from_str(name).is_some()
            {
                Ok(())
            } else {
                bail!(
                    "platform plugin {field} references unknown pool '{name}'; accepted: inherit, global, lite, cheap, standard, flagship, or a model list"
                )
            }
        }
        ModelPoolRef::Models(models) => {
            if models.is_empty() {
                bail!("platform plugin {field} must be omitted instead of empty");
            }
            let mut seen = HashSet::with_capacity(models.len());
            if models.iter().any(|model| {
                model.provider_id.trim().is_empty()
                    || model.model.trim().is_empty()
                    || !seen.insert((&model.provider_id, &model.model))
            }) {
                bail!("platform plugin {field} must contain unique, non-empty model references");
            }
            Ok(())
        }
    }
}
