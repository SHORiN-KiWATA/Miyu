//! 思考变体的选取、保存与按协议映射。
//!
//! 变体偏好按「供应商 + 模型」存盘并跨会话保留，所以这里既要管内存里的当前选
//! 择，也要管落盘（`save_thinking_variants` / `restore_saved_thinking_variants`）。
//!
//! 同一个变体在三套协议里长得完全不同：Chat 是 `reasoning_effort`，Responses 是
//! `reasoning.effort` + summary，Anthropic 是 thinking budget。三个
//! `*_reasoning` / `*_variant` 方法各管一套。

use crate::llm::openai_compatible::*;

impl OpenAiCompatibleClient {
    pub fn available_thinking_variants(&self) -> Vec<String> {
        let options = self.thinking_variant_options();
        (options.len() == 1)
            .then(|| options[0].variants.clone())
            .unwrap_or_default()
    }

    pub fn set_thinking_variant(&mut self, variant: Option<String>) -> Result<()> {
        let options = self.thinking_variant_options();
        if options.len() != 1 {
            bail!("a model must be specified when multiple models are active");
        }
        let option = &options[0];
        self.set_thinking_variants(&[(option.provider_id.clone(), option.model.clone(), variant)])
    }

    pub fn set_thinking_variants(
        &mut self,
        selections: &[(String, String, Option<String>)],
    ) -> Result<()> {
        let options = self.thinking_variant_options();
        for (provider_id, model, selected) in selections {
            let option = options
                .iter()
                .find(|option| option.provider_id == *provider_id && option.model == *model)
                .ok_or_else(|| anyhow::anyhow!("inactive model: {provider_id} / {model}"))?;
            if let Some(selected) = selected {
                if !option.variants.iter().any(|variant| variant == selected) {
                    bail!(
                        "thinking variant is unavailable for {provider_id} / {model}: {selected}"
                    );
                }
            }
        }
        for (provider_id, model, selected) in selections {
            let key = thinking_variant_key(provider_id, model);
            if let Some(selected) = selected.as_ref().filter(|value| !value.trim().is_empty()) {
                self.thinking_variants.insert(key, selected.clone());
            } else {
                self.thinking_variants.remove(&key);
            }
        }
        Ok(())
    }

    pub fn restore_thinking_variants(&mut self, selections: &[(String, String, String)]) {
        let active = self.endpoint_model_preferences();
        for (provider_id, model, selected) in selections {
            if active.iter().any(|(active_provider, active_model)| {
                active_provider == provider_id && active_model == model
            }) {
                self.thinking_variants
                    .insert(thinking_variant_key(provider_id, model), selected.clone());
            }
        }
    }

    pub(crate) fn restore_saved_thinking_variants(&mut self, paths: &MiyuPaths) {
        crate::llm::request_log::install_dir(paths.logs_dir());
        let preferences = load_thinking_variant_preferences(paths);
        let selections = self
            .endpoint_model_preferences()
            .into_iter()
            .filter_map(|(provider_id, model)| {
                let selected = preferences
                    .selected(&provider_id, &model)
                    .map(str::to_string)?;
                Some((provider_id, model, selected))
            })
            .collect::<Vec<_>>();
        self.restore_thinking_variants(&selections);
    }

    pub fn save_thinking_variants(&self, paths: &MiyuPaths) -> Result<()> {
        let mut preferences = load_thinking_variant_preferences(paths);
        for (provider_id, model) in self.endpoint_model_preferences() {
            let key = thinking_variant_key(&provider_id, &model);
            preferences.set(
                &provider_id,
                &model,
                self.thinking_variants.get(&key).cloned(),
            );
        }
        preferences.save(paths)
    }

    pub fn thinking_variant_options(&self) -> Vec<ThinkingVariantOptions> {
        self.endpoint_model_preferences()
            .into_iter()
            .filter_map(|(provider_id, model)| {
                let provider = &self
                    .endpoints
                    .iter()
                    .find(|endpoint| {
                        endpoint.provider.id == provider_id
                            && endpoint.provider.default_model == model
                    })?
                    .provider;
                let selected = self
                    .thinking_variants
                    .get(&thinking_variant_key(&provider_id, &model))
                    .map(String::as_str);
                Some(thinking_variant_options_for_model(
                    provider, &model, selected,
                ))
            })
            .collect()
    }

    pub fn thinking_variant_summary(&self) -> Option<String> {
        let options = self.thinking_variant_options();
        let mut variants = options.iter().map(|option| option.selected.as_deref());
        let first = variants.next()?;
        if variants.all(|variant| variant == first) {
            first.map(str::to_string)
        } else {
            Some("mixed".to_string())
        }
    }

    pub fn thinking_variant_for(&self, provider_id: &str, model: &str) -> Option<String> {
        self.thinking_variant_options()
            .into_iter()
            .find(|options| options.provider_id == provider_id && options.model == model)
            .and_then(|options| options.selected)
    }

    pub fn endpoint_model_preferences(&self) -> Vec<(String, String)> {
        self.endpoints
            .iter()
            .map(|endpoint| {
                (
                    endpoint.provider.id.clone(),
                    endpoint.provider.default_model.clone(),
                )
            })
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect()
    }

    pub(crate) fn selected_reasoning_variant(
        &self,
    ) -> Option<(ModelReasoningInfo, ReasoningVariant)> {
        let id = self.selected_thinking_variant_id()?;
        // Claude Code 不在 models.dev 目录里,思考档由协议自供(--effort 五档)。
        let info = if provider_uses_claude_code(&self.provider) {
            ModelReasoningInfo {
                provider_npm: None,
                variants: claude_code_reasoning_variants(&self.provider.default_model),
            }
        } else if provider_uses_antigravity(&self.provider) {
            ModelReasoningInfo {
                provider_npm: None,
                variants: antigravity_reasoning_variants(&self.provider.default_model),
            }
        } else if provider_uses_codex(&self.provider) {
            ModelReasoningInfo {
                provider_npm: None,
                variants: codex_reasoning_variants(&self.provider.default_model),
            }
        } else {
            models_cache::reasoning_info(&self.provider.id, &self.provider.default_model)?
        };
        let variant = info
            .variants
            .iter()
            .find(|candidate| candidate.id.as_str() == id)
            .cloned()?;
        reasoning_variant_supported(
            &self.provider,
            &self.provider.default_model,
            &info,
            &variant,
        )
        .then_some((info, variant))
    }

    pub(crate) fn selected_thinking_variant_id(&self) -> Option<&str> {
        self.thinking_variants
            .get(&thinking_variant_key(
                &self.provider.id,
                &self.provider.default_model,
            ))
            .map(String::as_str)
    }

    pub(crate) fn chat_variant_extra_body(&self) -> Option<Map<String, Value>> {
        let (info, variant) = self.selected_reasoning_variant()?;
        chat_variant_body(&self.provider, &info, variant.setting)
    }

    pub(in crate::llm::openai_compatible) fn responses_reasoning(
        &self,
    ) -> Option<ResponsesReasoning> {
        let summary = self.responses_reasoning_summary();
        let Some((_, variant)) = self.selected_reasoning_variant() else {
            return Some(default_responses_reasoning(summary));
        };
        match variant.setting {
            ReasoningSetting::Effort(effort) => Some(ResponsesReasoning {
                effort: Some(effort),
                summary: Some(summary.to_string()),
            }),
            ReasoningSetting::Toggle(true) => Some(default_responses_reasoning(summary)),
            ReasoningSetting::Toggle(false) | ReasoningSetting::Disabled => None,
            ReasoningSetting::BudgetTokens(_) => Some(default_responses_reasoning(summary)),
        }
    }

    pub(crate) fn responses_reasoning_summary(&self) -> &'static str {
        if self.detailed_reasoning_summary {
            "detailed"
        } else {
            "auto"
        }
    }

    pub(crate) fn anthropic_variant(
        &self,
        thinking_enabled: bool,
    ) -> (Option<Value>, Option<Map<String, Value>>) {
        if !thinking_enabled {
            return (None, None);
        }
        let Some((_, variant)) = self.selected_reasoning_variant() else {
            return (Some(anthropic_thinking_config()), None);
        };
        match variant.setting {
            ReasoningSetting::Effort(effort) => (
                Some(anthropic_thinking_config()),
                Some(
                    json!({ "output_config": { "effort": effort } })
                        .as_object()
                        .unwrap()
                        .clone(),
                ),
            ),
            ReasoningSetting::Toggle(true) => (Some(anthropic_thinking_config()), None),
            ReasoningSetting::Toggle(false) | ReasoningSetting::Disabled => (None, None),
            ReasoningSetting::BudgetTokens(budget) => {
                let budget = anthropic_reasoning_budget(self.provider.anthropic_max_tokens, budget)
                    .expect("unsupported Anthropic budget variant should be filtered");
                (
                    Some(json!({ "type": "enabled", "budget_tokens": budget })),
                    None,
                )
            }
        }
    }
}
