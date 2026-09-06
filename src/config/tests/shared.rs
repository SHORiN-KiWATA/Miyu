//! 配置测试共用的 fixture。

use crate::config::*;

pub(super) fn route_test_config() -> AppConfig {
    let mut config = AppConfig::default();
    let provider = &mut config.providers[0];
    provider.models = vec!["text-only".to_string(), "vision".to_string()];
    provider.default_model = "text-only".to_string();
    provider
        .model_modalities
        .insert("text-only".to_string(), vec!["text".to_string()]);
    provider.model_modalities.insert(
        "vision".to_string(),
        vec!["text".to_string(), "image".to_string()],
    );
    config
}

pub(super) fn test_route(config: &AppConfig) -> PlatformModelRoute {
    PlatformModelRoute {
        conversation: PlatformConversationConfig {
            kind: PlatformConversationKind::Group,
            id: "20002".to_string(),
        },
        persona: PlatformPersonaOverride::Inherit,
        text_models_inheritance: PlatformModelPoolInheritance::Platform,
        text_models: Some(vec![ActiveProviderModelConfig {
            provider_id: config.providers[0].id.clone(),
            model: "text-only".to_string(),
        }]),
        multimodal_models_inheritance: PlatformModelPoolInheritance::Platform,
        multimodal_models: Some(vec![ActiveProviderModelConfig {
            provider_id: config.providers[0].id.clone(),
            model: "vision".to_string(),
        }]),
        extra_prompt: "Reply naturally in this group.".to_string(),
        session_limits: None,
        probability_reply: None,
    }
}
