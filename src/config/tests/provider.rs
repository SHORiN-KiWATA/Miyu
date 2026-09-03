//! 供应商、模型池与能力标签。

use super::shared::*;
use crate::config::*;

/// Claude Code 置顶后 providers[0] 是禁用的内置条目;位置式引用改按
/// opencode 模板定位。
fn first_http_provider(config: &mut AppConfig) -> &mut ProviderConfig {
    config
        .providers
        .iter_mut()
        .find(|provider| !provider.is_builtin_cli_provider())
        .expect("default templates always carry HTTP providers")
}

#[test]
fn model_temperature_override_beats_provider_default() {
    let mut provider = ProviderConfig::default_opencodezen();
    provider.temperature = 0.6;
    provider.default_model = "a".to_string();
    assert_eq!(provider.effective_temperature(), 0.6);
    provider.model_temperature.insert("a".to_string(), 0.1);
    assert_eq!(provider.effective_temperature(), 0.1);
    // 别的模型不受覆盖牵连(验收:曾把供应商全局温度当模型温度写)。
    provider.default_model = "b".to_string();
    assert_eq!(provider.effective_temperature(), 0.6);
}

#[test]
fn api_quota_partial_provider_configs_keep_defaults() {
    let config: ApiQuotaPluginConfig = serde_json::from_value(serde_json::json!({
        "deepseek": { "api_key": "deepseek-key" },
        "openrouter": { "api_key": "openrouter-key" }
    }))
    .unwrap();
    assert!(config.enabled);
    assert_eq!(config.deepseek.api_key, "deepseek-key");
    assert_eq!(config.openrouter.api_key, "openrouter-key");
}

#[test]
fn provider_config_can_be_saved_without_active_model() {
    let mut config = AppConfig::default();
    config.providers[0].models.clear();
    config.providers[0].default_model.clear();
    assert!(config.validate().is_ok());
}

#[test]
fn provider_model_choices_ignore_unconfigured_models() {
    let mut config = AppConfig::default();
    let provider_id = config.providers[0].id.clone();
    config.providers[0].models.clear();
    config.providers[0].default_model.clear();

    assert!(!config
        .provider_model_choices()
        .iter()
        .any(|choice| choice.provider_id == provider_id));
}

#[test]
fn active_provider_models_are_replaced_as_one_validated_pool() {
    let mut config = AppConfig::default();
    let provider_id = first_http_provider(&mut config).id.clone();
    first_http_provider(&mut config).models = vec!["model-a".to_string(), "model-b".to_string()];
    first_http_provider(&mut config).default_model = "model-a".to_string();
    let before = config.active_provider_models.clone();

    let invalid = vec![
        ActiveProviderModelConfig {
            provider_id: provider_id.clone(),
            model: "model-a".to_string(),
        },
        ActiveProviderModelConfig {
            provider_id: provider_id.clone(),
            model: "missing".to_string(),
        },
    ];
    assert!(config.set_active_provider_models(&invalid).is_err());
    assert_eq!(config.active_provider_models, before);

    let selected = vec![
        ActiveProviderModelConfig {
            provider_id: provider_id.clone(),
            model: "model-b".to_string(),
        },
        ActiveProviderModelConfig {
            provider_id,
            model: "model-a".to_string(),
        },
    ];
    config.set_active_provider_models(&selected).unwrap();
    assert_eq!(
        config.active_provider_models.as_deref(),
        Some(selected.as_slice())
    );
}

#[test]
fn legacy_provider_temperatures_migrate_once() {
    let mut config = AppConfig {
        config_version: 0,
        ..AppConfig::default()
    };
    config.providers[0].temperature = LEGACY_DEFAULT_TEMPERATURE;
    config.providers[1].temperature = 0.5;

    config.migrate().unwrap();

    assert_eq!(config.config_version, CURRENT_CONFIG_VERSION);
    assert_eq!(config.providers[0].temperature, 1.0);
    assert_eq!(config.providers[1].temperature, 0.5);

    config.providers[0].temperature = LEGACY_DEFAULT_TEMPERATURE;
    config.migrate().unwrap();
    assert_eq!(config.providers[0].temperature, LEGACY_DEFAULT_TEMPERATURE);

    config.config_version = CURRENT_CONFIG_VERSION + 1;
    assert!(config.migrate().is_err());
}

#[test]
fn empty_active_provider_models_normalizes_to_default_chat_model() {
    let mut config = AppConfig::default();
    config.active_provider_models = Some(Vec::new());

    config.normalize_builtin_providers();

    let choices = config.active_provider_model_choices();
    assert_eq!(choices.len(), 1);
    assert_eq!(choices[0].provider_id, OPENCODE_PROVIDER_ID);
    assert_eq!(choices[0].model, OPENCODE_DEFAULT_CHAT_MODEL);
}

#[test]
fn active_provider_model_choices_ignore_stale_models() {
    let mut config = AppConfig::default();
    let provider_id = config.providers[0].id.clone();
    config.providers[0].models = vec!["deepseek-v4-flash-free".to_string()];
    config.providers[0].default_model = "deepseek-v4-flash-free".to_string();
    config.active_provider_models = Some(vec![
        ActiveProviderModelConfig {
            provider_id: provider_id.clone(),
            model: "mimo-v2.5-free".to_string(),
        },
        ActiveProviderModelConfig {
            provider_id: provider_id.clone(),
            model: "deepseek-v4-flash-free".to_string(),
        },
    ]);

    let choices = config.active_provider_model_choices();

    assert_eq!(choices.len(), 1);
    assert_eq!(choices[0].provider_id, provider_id);
    assert_eq!(choices[0].model, "deepseek-v4-flash-free");
}

#[test]
fn normalize_prunes_stale_active_provider_models() {
    let mut config = AppConfig::default();
    let provider_id = config.providers[0].id.clone();
    config.providers[0].models = vec!["deepseek-v4-flash-free".to_string()];
    config.providers[0].default_model = "deepseek-v4-flash-free".to_string();
    config.active_provider_models = Some(vec![
        ActiveProviderModelConfig {
            provider_id: provider_id.clone(),
            model: "mimo-v2.5-free".to_string(),
        },
        ActiveProviderModelConfig {
            provider_id: provider_id.clone(),
            model: "deepseek-v4-flash-free".to_string(),
        },
    ]);

    config.normalize_builtin_providers();

    assert_eq!(
        config.active_provider_models,
        Some(vec![ActiveProviderModelConfig {
            provider_id,
            model: "deepseek-v4-flash-free".to_string(),
        }])
    );
}

#[test]
fn remove_active_model_references_clears_text_and_multimodal() {
    let mut config = AppConfig::default();
    let provider_id = config.providers[0].id.clone();
    config.active_provider_models = Some(vec![ActiveProviderModelConfig {
        provider_id: provider_id.clone(),
        model: "old-model".to_string(),
    }]);
    config.active_multimodal_provider_models = Some(vec![ActiveProviderModelConfig {
        provider_id: provider_id.clone(),
        model: "old-model".to_string(),
    }]);

    config.remove_active_model_references(&provider_id, "old-model");

    assert_eq!(config.active_provider_models, None);
    assert_eq!(config.active_multimodal_provider_models, None);
}

#[test]
fn multimodal_provider_model_choices_use_input_modalities() {
    let mut config = AppConfig::default();
    let provider = first_http_provider(&mut config);
    provider.models = vec![
        "text-only".to_string(),
        "audio-only".to_string(),
        "vision-model".to_string(),
    ];
    provider
        .model_modalities
        .insert("text-only".to_string(), vec!["text".to_string()]);
    provider.model_modalities.insert(
        "audio-only".to_string(),
        vec!["text".to_string(), "audio".to_string()],
    );
    provider.model_modalities.insert(
        "vision-model".to_string(),
        vec!["text".to_string(), "image".to_string()],
    );

    let choices = config.multimodal_provider_model_choices();

    assert!(choices.iter().any(|choice| choice.model == "vision-model"));
    assert!(!choices.iter().any(|choice| choice.model == "text-only"));
    assert!(!choices.iter().any(|choice| choice.model == "audio-only"));
}

#[test]
fn active_multimodal_pool_rejects_and_prunes_non_image_models() {
    let mut config = AppConfig::default();
    let provider_id = first_http_provider(&mut config).id.clone();
    first_http_provider(&mut config)
        .models
        .extend(["audio-only".to_string(), "vision-model".to_string()]);
    first_http_provider(&mut config).model_modalities.insert(
        "audio-only".to_string(),
        vec!["text".to_string(), "audio".to_string()],
    );
    first_http_provider(&mut config).model_modalities.insert(
        "vision-model".to_string(),
        vec!["text".to_string(), "image".to_string()],
    );

    assert!(config
        .toggle_active_multimodal_provider_model(&provider_id, "audio-only")
        .is_err());
    assert!(config
        .toggle_active_multimodal_provider_model(&provider_id, "vision-model")
        .unwrap());
    config
        .active_multimodal_provider_models
        .as_mut()
        .unwrap()
        .push(ActiveProviderModelConfig {
            provider_id,
            model: "audio-only".to_string(),
        });
    assert!(config.validate_global_multimodal_config().is_err());

    config.normalize_builtin_providers();

    let active = config.active_multimodal_provider_models.unwrap();
    assert_eq!(active.len(), 1);
    assert_eq!(active[0].model, "vision-model");
}

#[test]
fn vision_provider_choice_prefers_multimodal_pool_then_default_mimo() {
    let mut config = AppConfig::default();
    first_http_provider(&mut config)
        .models
        .push("vision-model".to_string());
    first_http_provider(&mut config).model_modalities.insert(
        "vision-model".to_string(),
        vec!["text".to_string(), "image".to_string()],
    );
    config.active_multimodal_provider_models = Some(vec![ActiveProviderModelConfig {
        provider_id: OPENCODE_PROVIDER_ID.to_string(),
        model: "vision-model".to_string(),
    }]);

    assert_eq!(
        config.vision_provider_choice().unwrap(),
        (OPENCODE_PROVIDER_ID.to_string(), "vision-model".to_string())
    );

    config.active_multimodal_provider_models = Some(Vec::new());
    assert_eq!(
        config.vision_provider_choice().unwrap(),
        (
            OPENCODE_PROVIDER_ID.to_string(),
            OPENCODE_DEFAULT_VISION_MODEL.to_string()
        )
    );
}

#[test]
fn vision_provider_choice_rejects_an_audio_only_active_pool() {
    let mut config = AppConfig::default();
    let provider_id = config.providers[0].id.clone();
    config.providers[0].models.push("audio-only".to_string());
    config.providers[0].model_modalities.insert(
        "audio-only".to_string(),
        vec!["text".to_string(), "audio".to_string()],
    );
    config.active_multimodal_provider_models = Some(vec![ActiveProviderModelConfig {
        provider_id,
        model: "audio-only".to_string(),
    }]);

    assert!(config.vision_provider_choice().is_err());
    assert!(config.validate().is_err());
}

#[test]
fn vision_provider_choice_rejects_an_explicit_non_image_model() {
    let mut config = AppConfig::default();
    let provider_id = config.providers[0].id.clone();
    config.providers[0].models.push("audio-only".to_string());
    config.providers[0].model_modalities.insert(
        "audio-only".to_string(),
        vec!["text".to_string(), "audio".to_string()],
    );
    config.plugins.vision.vision_provider_id = provider_id;
    config.plugins.vision.vision_model = "audio-only".to_string();

    assert!(config.vision_provider_choice().is_err());
    assert!(config.validate().is_err());
}

#[test]
fn subagent_tier_pools_toggle_filter_and_prune() {
    let mut config = AppConfig::default();
    let provider_id = config.providers[0].id.clone();
    config.providers[0].models.push("mini-a".to_string());
    config.providers[0].models.push("mini-b".to_string());

    // Unconfigured pool resolves empty.
    assert!(config.subagent_tier_choices(ModelTier::Cheap).is_empty());

    // Toggle in/out mirrors the text-model picker semantics.
    assert!(config
        .toggle_subagent_tier_model(ModelTier::Cheap, &provider_id, "mini-a")
        .unwrap());
    assert!(config
        .toggle_subagent_tier_model(ModelTier::Cheap, &provider_id, "mini-b")
        .unwrap());
    assert!(config.is_subagent_tier_model(ModelTier::Cheap, &provider_id, "mini-a"));
    let choices = config.subagent_tier_choices(ModelTier::Cheap);
    assert_eq!(
        choices.iter().map(|c| c.model.as_str()).collect::<Vec<_>>(),
        vec!["mini-a", "mini-b"]
    );
    assert!(!config
        .toggle_subagent_tier_model(ModelTier::Cheap, &provider_id, "mini-b")
        .unwrap());
    assert_eq!(config.subagent_tier_choices(ModelTier::Cheap).len(), 1);

    // Unknown provider is rejected.
    assert!(config
        .toggle_subagent_tier_model(ModelTier::Strong, "no-such", "x")
        .is_err());

    // A model removed from the text models leaves the pool too.
    config
        .toggle_subagent_tier_model(ModelTier::Balanced, &provider_id, "mini-a")
        .unwrap();
    config
        .remove_active_provider_model(&provider_id, "mini-a")
        .unwrap();
    assert!(config.subagent_tier_choices(ModelTier::Cheap).is_empty());
    assert!(config.subagent_tiers.pool(ModelTier::Cheap).is_empty());
    assert!(config.subagent_tiers.pool(ModelTier::Balanced).is_empty());

    // prune_subagent_tiers drops entries that no longer resolve.
    config
        .toggle_subagent_tier_model(ModelTier::Cheap, &provider_id, "mini-b")
        .unwrap();
    config.providers[0].models.retain(|m| m != "mini-b");
    assert!(config.subagent_tier_choices(ModelTier::Cheap).is_empty());
    config.prune_subagent_tiers();
    assert!(config.subagent_tiers.pool(ModelTier::Cheap).is_empty());
}

#[test]
fn subagent_tiers_roundtrip_and_default_omission() {
    let config = AppConfig::default();
    let json = serde_json::to_string(&config).unwrap();
    // Empty pools stay out of the serialized config.
    assert!(!json.contains("subagent_tiers"));

    let parsed: AppConfig = serde_json::from_str(
        r#"{
            "active_provider": "opencode",
            "providers": [],
            "subagent_tiers": {
                "cheap": [ { "provider_id": "p", "model": "m" } ]
            }
        }"#,
    )
    .unwrap();
    assert_eq!(parsed.subagent_tiers.cheap.len(), 1);
    assert_eq!(parsed.subagent_tiers.cheap[0].model, "m");
    assert!(parsed.subagent_tiers.balanced.is_empty());
    // Choices filter out entries with unknown providers.
    assert!(parsed.subagent_tier_choices(ModelTier::Cheap).is_empty());
}

#[test]
fn an_embedding_model_never_reaches_the_chat_pickers() {
    // It produces vectors, not replies; the multimodal list derives from
    // the text one, so filtering at the source covers both.
    let mut config = AppConfig::default();
    let provider = first_http_provider(&mut config);
    provider.models = vec!["chat-model".to_string(), "vector-model".to_string()];
    provider
        .model_modalities
        .insert("chat-model".to_string(), vec!["text".to_string()]);
    provider.model_modalities.insert(
        "vector-model".to_string(),
        vec![EMBEDDING_MODALITY.to_string()],
    );

    let text: Vec<String> = config
        .text_provider_model_choices()
        .into_iter()
        .map(|choice| choice.model)
        .collect();
    assert!(text.contains(&"chat-model".to_string()), "{text:?}");
    assert!(!text.contains(&"vector-model".to_string()), "{text:?}");
}

#[test]
fn the_embedding_model_moves_out_from_under_the_knowledge_base() {
    // It was configured there because that is where it was first needed;
    // it now also backs memory recall, and a knowledge-base setting quietly
    // steering group-chat search is a trap for whoever reads this next.
    let mut config = AppConfig::default();
    config.plugins.knowledge_base.embedding_provider_id = "omlx".to_string();
    config.plugins.knowledge_base.embedding_model = "bge-m3".to_string();
    config.plugins.knowledge_base.embedding_timeout_seconds = 45;
    config.plugins.knowledge_base.semantic_min_score = 0.5;
    config.config_version = 0;
    config.migrate().unwrap();
    assert_eq!(config.embedding.provider_id, "omlx");
    assert_eq!(config.embedding.model, "bge-m3");
    assert_eq!(config.embedding.timeout_seconds, 45);
    assert!((config.embedding.min_score - 0.5).abs() < f32::EPSILON);

    // Configuring a model only makes it available; there is no switch.
    assert!(config.embedding.is_configured());
    assert!(!AppConfig::default().embedding.is_configured());
}

#[test]
fn real_context_models_follow_provider_lifecycle() {
    let mut config = route_test_config();
    let old_id = config.providers[0].id.clone();
    let settings = RealContextPluginSettings {
        text_models: Some(vec![ActiveProviderModelConfig {
            provider_id: old_id.clone(),
            model: "text-only".to_string(),
        }]),
        ..RealContextPluginSettings::default()
    };
    let mut instance = PlatformPluginInstanceConfig::default();
    instance
        .settings
        .insert("future_option".to_string(), serde_json::json!(true));
    merge_real_context_settings(&mut instance, &settings);
    config
        .platforms
        .qq
        .plugins
        .insert(REAL_CONTEXT_PLUGIN_ID.to_string(), instance);

    config.providers[0].id = "renamed".to_string();
    config.rename_provider_references(&old_id, "renamed");
    let instance = &config.platforms.qq.plugins[REAL_CONTEXT_PLUGIN_ID];
    let reparsed = RealContextPluginSettings::from_instance(instance).unwrap();
    assert_eq!(reparsed.text_models.unwrap()[0].provider_id, "renamed");
    assert_eq!(instance.settings["future_option"], true);

    config.remove_active_model_references("renamed", "text-only");
    let reparsed = RealContextPluginSettings::from_instance(
        &config.platforms.qq.plugins[REAL_CONTEXT_PLUGIN_ID],
    )
    .unwrap();
    assert!(reparsed.text_models.is_none());
}

#[test]
fn provider_reference_updates_cover_every_model_pool_and_plugin() {
    let mut config = route_test_config();
    let old_id = config.providers[0].id.clone();
    config.active_provider = old_id.clone();
    config.active_provider_models = Some(vec![ActiveProviderModelConfig {
        provider_id: old_id.clone(),
        model: "text-only".to_string(),
    }]);
    config.active_multimodal_provider_models = Some(vec![ActiveProviderModelConfig {
        provider_id: old_id.clone(),
        model: "vision".to_string(),
    }]);
    config.subagent_tiers.cheap.push(ActiveProviderModelConfig {
        provider_id: old_id.clone(),
        model: "text-only".to_string(),
    });
    config.platforms.qq.non_whitelist_text_models = Some(vec![ActiveProviderModelConfig {
        provider_id: old_id.clone(),
        model: "text-only".to_string(),
    }]);
    config.platforms.qq.conversations.push(test_route(&config));
    config.plugins.vision.vision_provider_id = old_id.clone();
    config.plugins.vision.vision_model = "vision".to_string();
    config.plugins.knowledge_base.embedding_provider_id = old_id.clone();
    config.plugins.knowledge_base.embedding_model = "text-only".to_string();

    config.providers[0].id = "renamed".to_string();
    config.rename_provider_references(&old_id, "renamed");

    assert_eq!(config.active_provider, "renamed");
    assert_eq!(
        config.active_provider_models.as_ref().unwrap()[0].provider_id,
        "renamed"
    );
    assert_eq!(
        config.active_multimodal_provider_models.as_ref().unwrap()[0].provider_id,
        "renamed"
    );
    assert_eq!(config.subagent_tiers.cheap[0].provider_id, "renamed");
    assert_eq!(
        config
            .platforms
            .qq
            .non_whitelist_text_models
            .as_ref()
            .unwrap()[0]
            .provider_id,
        "renamed"
    );
    assert_eq!(
        config.platforms.qq.conversations[0]
            .text_models
            .as_ref()
            .unwrap()[0]
            .provider_id,
        "renamed"
    );
    assert_eq!(config.plugins.vision.vision_provider_id, "renamed");
    assert_eq!(
        config.plugins.knowledge_base.embedding_provider_id,
        "renamed"
    );
    assert!(config.validate().is_ok());

    config.providers.remove(0);
    config.remove_provider_references("renamed");
    assert!(config.active_provider_models.is_none());
    assert!(config.active_multimodal_provider_models.is_none());
    assert!(config.subagent_tiers.cheap.is_empty());
    assert!(config.platforms.qq.non_whitelist_text_models.is_none());
    assert_eq!(config.platforms.qq.conversations.len(), 1);
    assert!(config.platforms.qq.conversations[0].text_models.is_none());
    assert!(config.plugins.vision.vision_provider_id.is_empty());
    assert!(config
        .plugins
        .knowledge_base
        .embedding_provider_id
        .is_empty());
    assert_ne!(config.active_provider, "renamed");
}

#[test]
fn model_capability_pruning_clears_all_invalid_image_references() {
    let mut config = route_test_config();
    let provider_id = config.providers[0].id.clone();
    config.active_multimodal_provider_models = Some(vec![ActiveProviderModelConfig {
        provider_id: provider_id.clone(),
        model: "vision".to_string(),
    }]);
    config.platforms.qq.conversations.push(test_route(&config));
    config.plugins.vision.vision_provider_id = provider_id;
    config.plugins.vision.vision_model = "vision".to_string();
    config.providers[0]
        .model_modalities
        .insert("vision".to_string(), vec!["text".to_string()]);

    config.prune_model_references();

    assert!(config.active_multimodal_provider_models.is_none());
    assert!(config.platforms.qq.conversations[0]
        .multimodal_models
        .is_none());
    assert!(config.plugins.vision.vision_provider_id.is_empty());
    assert!(config.plugins.vision.vision_model.is_empty());
}

#[test]
fn duplicate_provider_ids_are_rejected() {
    let mut config = AppConfig::default();
    config.providers.push(config.providers[0].clone());
    assert!(config.validate().is_err());
}

#[test]
fn platform_multimodal_pruning_tracks_provider_capabilities() {
    let mut config = route_test_config();
    config.platforms.qq.conversations.push(test_route(&config));
    config.providers[0]
        .model_modalities
        .insert("vision".to_string(), vec!["text".to_string()]);

    config.prune_platform_model_routes();

    let route = &config.platforms.qq.conversations[0];
    assert!(route.multimodal_models.is_none());
    assert_eq!(route.text_models.as_ref().unwrap().len(), 1);
}

#[test]
fn new_custom_provider_has_no_openai_defaults() {
    let provider = ProviderConfig::new_custom();

    assert!(provider.id.is_empty());
    assert!(provider.display_name.is_empty());
    assert!(provider.base_url.is_empty());
    assert_eq!(provider.protocol, "auto");
    assert!(provider.api_key.is_none());
    assert!(provider.models.is_empty());
    assert!(provider.default_model.is_empty());
}

#[test]
fn default_anthropic_provider_uses_the_global_context_window_default() {
    let mut config = AppConfig::default();
    config.active_provider = "anthropic".to_string();
    let provider = config
        .providers
        .iter_mut()
        .find(|provider| provider.id == "anthropic")
        .unwrap();
    provider.models = vec!["claude-sonnet-4-5".to_string()];
    provider.default_model = "claude-sonnet-4-5".to_string();

    assert_eq!(config.active_context_window().unwrap(), Some(168_000));
}

#[test]
fn default_anthropic_provider_has_no_implicit_active_model() {
    let provider = ProviderConfig::default_anthropic();

    assert!(provider.models.is_empty());
    assert!(provider.default_model.is_empty());
}

#[test]
fn normalizes_legacy_anthropic_template_model() {
    let mut config = AppConfig::default();
    let provider = config
        .providers
        .iter_mut()
        .find(|provider| provider.id == "anthropic")
        .unwrap();
    provider.models = vec!["claude-sonnet-4-5".to_string()];
    provider.default_model = "claude-sonnet-4-5".to_string();

    config.normalize_builtin_providers();
    let provider = config
        .providers
        .iter()
        .find(|provider| provider.id == "anthropic")
        .unwrap();

    assert!(provider.models.is_empty());
    assert!(provider.default_model.is_empty());
}

#[test]
fn anthropic_template_does_not_hardcode_model_context_window() {
    let provider = ProviderConfig::default_anthropic();

    assert!(provider.model_context_window.is_empty());
}

#[test]
fn remove_active_provider_model_clears_removed_current_model() {
    let mut config = AppConfig::default();
    let provider_id = config.providers[0].id.clone();
    config.providers[0].models = vec!["old-model".to_string(), "next-model".to_string()];
    config.providers[0].default_model = "old-model".to_string();
    config.providers[0]
        .model_context_window
        .insert("old-model".to_string(), 8192);
    config.providers[0]
        .model_modalities
        .insert("old-model".to_string(), vec!["text".to_string()]);

    config
        .remove_active_provider_model(&provider_id, "old-model")
        .unwrap();

    assert_eq!(config.providers[0].models, vec!["next-model"]);
    assert_eq!(config.providers[0].default_model, "next-model");
    assert!(!config.providers[0]
        .model_context_window
        .contains_key("old-model"));
    assert!(!config.providers[0]
        .model_modalities
        .contains_key("old-model"));
}

#[test]
fn remove_active_provider_model_clears_last_current_model() {
    let mut config = AppConfig::default();
    let provider_id = config.providers[0].id.clone();
    config.providers[0].models = vec!["old-model".to_string()];
    config.providers[0].default_model = "old-model".to_string();

    config
        .remove_active_provider_model(&provider_id, "old-model")
        .unwrap();

    assert!(config.providers[0].models.is_empty());
    assert!(config.providers[0].default_model.is_empty());
    assert!(!config
        .provider_model_choices()
        .iter()
        .any(|choice| choice.provider_id == provider_id));
}

#[test]
fn extra_body_roundtrip() {
    let original = ProviderConfig {
        enabled: true,
        id: "test".to_string(),
        display_name: "Test".to_string(),
        base_url: "https://example.com".to_string(),
        protocol: "auto".to_string(),
        api_key: None,
        models: vec![],
        model_context_window: HashMap::new(),
        model_temperature: HashMap::new(),
        model_tools_loading_mode: HashMap::new(),
        model_modalities: HashMap::new(),
        tool_result_media: None,
        model_costs: HashMap::new(),
        default_model: String::new(),
        timeout_seconds: 60,
        temperature: 1.0,
        anthropic_max_tokens: 4096,
        extra_body: serde_json::json!({
            "enable_thinking": false,
            "reasoning_effort": "low"
        })
        .as_object()
        .cloned(),
    };

    let serialized = serde_json::to_string(&original).unwrap();
    let deserialized: ProviderConfig = serde_json::from_str(&serialized).unwrap();

    assert_eq!(original.extra_body, deserialized.extra_body);
    assert_eq!(original.id, deserialized.id);
}

#[test]
fn extra_body_rejects_non_object_config_values() {
    for extra_body in [
        serde_json::json!(true),
        serde_json::json!("invalid"),
        serde_json::json!([1, 2, 3]),
    ] {
        let provider = serde_json::json!({
            "id": "test",
            "display_name": "Test",
            "base_url": "https://example.com",
            "extra_body": extra_body
        });

        assert!(serde_json::from_value::<ProviderConfig>(provider).is_err());
    }
}

// ── 窗口值的出处 ────────────────────────────────────────────────

/// 谁都没给窗口时用的是 `context.default_context_window`——那是个通用常数，
/// 跟具体模型没有任何关系。必须报成 `Assumed`，否则 footer 会拿它算出一个
/// 看起来很确定的百分比。
#[test]
fn a_window_that_falls_through_to_the_global_default_is_marked_assumed() {
    let mut config = AppConfig::default();
    config.active_provider = "anthropic".to_string();
    let provider = config
        .providers
        .iter_mut()
        .find(|provider| provider.id == "anthropic")
        .unwrap();
    provider.models = vec!["a-model-nobody-has-heard-of".to_string()];
    provider.default_model = "a-model-nobody-has-heard-of".to_string();

    assert_eq!(
        config.active_context_window_with_source().unwrap(),
        Some((168_000, ContextWindowSource::Assumed))
    );
}

/// 用户在配置里写死的窗口是有出处的，照常出百分比。
#[test]
fn a_window_written_in_the_config_is_known() {
    let mut config = AppConfig::default();
    config.active_provider = "anthropic".to_string();
    let provider = config
        .providers
        .iter_mut()
        .find(|provider| provider.id == "anthropic")
        .unwrap();
    provider.models = vec!["a-model-nobody-has-heard-of".to_string()];
    provider.default_model = "a-model-nobody-has-heard-of".to_string();
    provider
        .model_context_window
        .insert("a-model-nobody-has-heard-of".to_string(), 1_000_000);

    assert_eq!(
        config.active_context_window_with_source().unwrap(),
        Some((1_000_000, ContextWindowSource::Known))
    );
}

/// 池子里混了一个猜的，整池就算猜的。
///
/// 显示的是各模型的最小值——那个「猜的」成员真实窗口要是比最小值还小，最小值
/// 本身就是错的，所以不能因为最小值恰好来自有出处的那个就报 `Known`。
#[test]
fn one_assumed_model_makes_the_whole_pool_assumed() {
    let mut config = AppConfig::default();
    config.active_provider = "anthropic".to_string();
    {
        let provider = config
            .providers
            .iter_mut()
            .find(|provider| provider.id == "anthropic")
            .unwrap();
        provider.models = vec!["pinned".to_string(), "guessed".to_string()];
        provider.default_model = "pinned".to_string();
        // 写死的那个比兜底值**小**，所以最小值来自「有出处」的那一个
        provider
            .model_context_window
            .insert("pinned".to_string(), 100_000);
    }
    config.active_provider_models = Some(vec![
        ActiveProviderModelConfig {
            provider_id: "anthropic".to_string(),
            model: "pinned".to_string(),
        },
        ActiveProviderModelConfig {
            provider_id: "anthropic".to_string(),
            model: "guessed".to_string(),
        },
    ]);

    let (window, source) = config.active_context_window_with_source().unwrap().unwrap();
    assert_eq!(window, 100_000, "最小值应该来自写死的那个");
    assert_eq!(
        source,
        ContextWindowSource::Assumed,
        "池子里有一个是猜的，整池就不能算有出处"
    );
}

/// 量尺：`MIYU_HOME=~/.miyu cargo test --lib real_config_window_source -- --ignored --nocapture`
///
/// 拿**用户真实的配置和模型缓存**跑一遍窗口解析，看它到底解出多少、算不算有
/// 出处。纯读，不写任何东西。
///
/// 存在的理由：这条链有三级兜底、还带按 base_url 的供应商别名对齐，光看代码
/// 和翻缓存文件很容易推错——我就推错过两次。
#[test]
#[ignore]
fn real_config_window_source() {
    let Some(home) = std::env::var_os("MIYU_HOME") else {
        println!("\n  跳过：没给 MIYU_HOME");
        return;
    };
    let paths = crate::paths::MiyuPaths::new().unwrap();
    println!("\n  MIYU_HOME = {}", std::path::Path::new(&home).display());
    let config = AppConfig::load(&paths).unwrap();
    crate::models_cache::ensure_active_metadata(&paths, &config);

    for choice in config.active_provider_model_choices() {
        let resolved = config
            .context_window_with_source(&choice.provider_id, &choice.model)
            .unwrap();
        println!(
            "  {} / {} → {:?}",
            choice.provider_id, choice.model, resolved
        );
    }
    println!(
        "  池子聚合 → {:?}",
        config.active_context_window_with_source().unwrap()
    );
}

/// 08-20:Claude Code 是内置特殊供应商——normalize 自动注入、默认禁用、
/// 模型预置 CLI 别名;未启用时不进任何模型选择器,启用即出现。
#[test]
fn claude_code_builtin_provider_is_injected_disabled_with_preset_models() {
    let mut config = AppConfig::default();
    config.normalize_builtin_providers();
    let provider = config
        .providers
        .iter()
        .find(|provider| provider.is_claude_code())
        .expect("内置 Claude Code 供应商应被注入");
    // 用户拍板的列表次序:恒置顶。
    assert!(config.providers[0].is_claude_code());
    assert_eq!(provider.id, "claude-code");
    assert!(!provider.enabled, "默认必须是禁用态");
    assert_eq!(provider.models, ["fable", "opus", "sonnet", "haiku"]);
    assert_eq!(provider.default_model, "sonnet");
    assert!(!config.claude_code_enabled());

    // 未启用 ⇒ 选择器里不可见。
    assert!(!config
        .text_provider_model_choices()
        .iter()
        .any(|choice| choice.provider_id == "claude-code"));

    for provider in &mut config.providers {
        if provider.is_claude_code() {
            provider.enabled = true;
        }
    }
    assert!(config.claude_code_enabled());
    let choices = config.text_provider_model_choices();
    assert!(choices
        .iter()
        .any(|choice| choice.provider_id == "claude-code" && choice.model == "sonnet"));

    // 重复 normalize 不会二次注入。
    config.normalize_builtin_providers();
    assert_eq!(
        config
            .providers
            .iter()
            .filter(|provider| provider.is_claude_code())
            .count(),
        1
    );
}

/// 09-03:Antigravity 是第二个内置 CLI 中转供应商——normalize 注入、紧随
/// Claude Code 之后、默认禁用、模型预置 agy 别名;未启用不进选择器。
#[test]
fn antigravity_builtin_provider_is_injected_disabled_after_claude_code() {
    let mut config = AppConfig::default();
    config.normalize_builtin_providers();
    assert!(config.providers[0].is_claude_code());
    assert!(config.providers[1].is_antigravity());
    assert!(
        config.providers[2].is_codex(),
        "Codex 紧随 Antigravity 之后"
    );
    assert!(!config.providers[2].enabled);
    assert_eq!(config.providers[2].default_model, "gpt-5.6-terra");
    assert!(!config.codex_enabled());
    let provider = &config.providers[1];
    assert_eq!(provider.id, "antigravity");
    assert!(!provider.enabled, "默认必须是禁用态");
    assert_eq!(provider.default_model, "gemini-3.8-flash-high");
    assert!(provider
        .models
        .iter()
        .any(|model| model == "claude-sonnet-4-6"));
    assert!(!config.antigravity_enabled());
    assert!(!config
        .text_provider_model_choices()
        .iter()
        .any(|choice| choice.provider_id == "antigravity"));

    // 存量配置把它排到后面:normalize 搬回第二位;重复 normalize 不二次注入。
    let moved = config.providers.remove(1);
    config.providers.push(moved);
    let moved = config.providers.remove(1);
    config.providers.push(moved);
    config.normalize_builtin_providers();
    assert!(config.providers[1].is_antigravity());
    assert!(config.providers[2].is_codex());
    assert_eq!(
        config
            .providers
            .iter()
            .filter(|provider| provider.is_antigravity())
            .count(),
        1
    );
    for provider in &mut config.providers {
        if provider.is_antigravity() {
            provider.enabled = true;
        }
    }
    assert!(config.antigravity_enabled());
    assert!(config
        .text_provider_model_choices()
        .iter()
        .any(|choice| choice.provider_id == "antigravity"));
}

/// 会话模型覆盖指向已删除的模型时,必须能退回全局池而不是把入口锁死。
///
/// 08-28 实录:`opencodego/glm-5.3-flash` 被移出供应商的 models 之后,钉着它
/// 的会话让 `miyu normal` 直接报 "no active provider/model endpoint is
/// configured"——而且尾巴是空的,因为筛掉的原因被静默丢了。用户连 REPL 都进
/// 不去,也就没机会用 `/model` 换一个。
#[test]
fn a_stale_session_override_falls_back_instead_of_locking_the_entry() {
    let mut config = AppConfig::default();
    let provider_id = config.providers[0].id.clone();
    config.providers[0].models = vec!["deepseek-v4-flash-free".to_string()];
    config.providers[0].default_model = "deepseek-v4-flash-free".to_string();

    // 覆盖里全是已删除的模型 → 无一可用 → 调用方据此退回全局池。
    let all_stale = vec![ActiveProviderModelConfig {
        provider_id: provider_id.clone(),
        model: "glm-5.3-flash".to_string(),
    }];
    assert!(config.usable_model_override(all_stale).is_none());

    // 覆盖里指向不存在的供应商同样算不可用。
    let unknown_provider = vec![ActiveProviderModelConfig {
        provider_id: "no-such-provider".to_string(),
        model: "deepseek-v4-flash-free".to_string(),
    }];
    assert!(config.usable_model_override(unknown_provider).is_none());

    // 还剩得下的就只保留那些,不因为有坏条目就整份作废。
    let mixed = vec![
        ActiveProviderModelConfig {
            provider_id: provider_id.clone(),
            model: "glm-5.3-flash".to_string(),
        },
        ActiveProviderModelConfig {
            provider_id: provider_id.clone(),
            model: "deepseek-v4-flash-free".to_string(),
        },
    ];
    let usable = config.usable_model_override(mixed).expect("还有可用条目");
    assert_eq!(usable.len(), 1);
    assert_eq!(usable[0].model, "deepseek-v4-flash-free");
}

/// 工具结果带图的能力推断:openai-chat 端点默认能,OpenAI 官方/本机 CLI/
/// anthropic 协议不能,显式配置压过推断(09-03 智谱实测)。
#[test]
fn tool_result_media_capability_is_inferred_per_provider() {
    let mut zhipu =
        ProviderConfig::template("bigmodel", "智谱", "https://open.bigmodel.cn/api/paas/v4");
    assert!(zhipu.tool_result_carries_media());
    let openai = ProviderConfig::template("openai", "OpenAI", "https://api.openai.com/v1");
    assert!(!openai.tool_result_carries_media());
    assert!(!ProviderConfig::codex_template().tool_result_carries_media());
    assert!(!ProviderConfig::antigravity_template().tool_result_carries_media());
    let mut anthropic = ProviderConfig::template("a", "A", "https://api.anthropic.com");
    anthropic.protocol = "anthropic".to_string();
    assert!(!anthropic.tool_result_carries_media());
    zhipu.tool_result_media = Some(false);
    assert!(!zhipu.tool_result_carries_media());
}
