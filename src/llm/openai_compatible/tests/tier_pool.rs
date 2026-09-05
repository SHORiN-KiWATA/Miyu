//! 档位池客户端构造(`from_tier` / `from_aux_role`)的回退契约:未配置=主池、
//! 配了但不可用=主池+notice。task 工具与辅助角色共用这一份语义。

use super::shared::test_paths;
use crate::config::{AppConfig, AuxRole, ModelTier};
use crate::llm::openai_compatible::*;

/// Default config with extra models on the *active* provider. `providers[0]`
/// is not necessarily the active one (builtin CLI providers ship disabled),
/// so pick by id.
fn config_with_models(models: &[&str]) -> AppConfig {
    let mut config = AppConfig::default();
    let active = config.active_provider.clone();
    let provider = config
        .providers
        .iter_mut()
        .find(|provider| provider.id == active)
        .expect("active provider present in default config");
    for model in models {
        provider.models.push((*model).to_string());
    }
    config
}

fn active_provider_id(config: &AppConfig) -> String {
    config.active_provider.clone()
}

#[test]
fn unconfigured_tier_uses_main_pool_without_notice() {
    let temp = tempfile::tempdir().unwrap();
    let paths = test_paths(temp.path());
    let config = config_with_models(&[]);
    let routed = OpenAiCompatibleClient::from_tier(&config, &paths, ModelTier::Cheap).unwrap();
    assert!(routed.notice.is_none());
    let main = config.active_provider_model_choices().remove(0);
    assert_eq!(routed.model_choice, Some((main.provider_id, main.model)));
}

#[test]
fn configured_tier_builds_its_own_pool() {
    let temp = tempfile::tempdir().unwrap();
    let paths = test_paths(temp.path());
    let mut config = config_with_models(&["mini-a"]);
    let provider_id = active_provider_id(&config);
    config
        .toggle_tier_model(ModelTier::Cheap, &provider_id, "mini-a")
        .unwrap();
    let routed = OpenAiCompatibleClient::from_tier(&config, &paths, ModelTier::Cheap).unwrap();
    assert!(routed.notice.is_none());
    assert_eq!(
        routed.model_choice,
        Some((provider_id, "mini-a".to_string()))
    );
    assert_eq!(routed.client.provider.default_model, "mini-a");
}

#[test]
fn tier_whose_models_vanished_falls_back_with_notice() {
    let temp = tempfile::tempdir().unwrap();
    let paths = test_paths(temp.path());
    let mut config = config_with_models(&["mini-a"]);
    let provider_id = active_provider_id(&config);
    config
        .toggle_tier_model(ModelTier::Flagship, &provider_id, "mini-a")
        .unwrap();
    // The model leaves the provider but the (stale) tier entry stays.
    config
        .providers
        .iter_mut()
        .find(|provider| provider.id == provider_id)
        .unwrap()
        .models
        .retain(|model| model != "mini-a");
    let routed = OpenAiCompatibleClient::from_tier(&config, &paths, ModelTier::Flagship).unwrap();
    let notice = routed.notice.expect("stale pool must explain the fallback");
    assert!(
        notice.contains("tier 'flagship' pool has no usable model"),
        "{notice}"
    );
    assert_ne!(routed.client.provider.default_model, "mini-a");
}

#[test]
fn tier_on_disabled_provider_falls_back_with_notice() {
    let temp = tempfile::tempdir().unwrap();
    let paths = test_paths(temp.path());
    let mut config = config_with_models(&[]);
    let mut dead = config
        .providers
        .iter()
        .find(|provider| provider.id == config.active_provider)
        .unwrap()
        .clone();
    dead.id = "dead".to_string();
    dead.enabled = false;
    dead.models = vec!["mini-a".to_string()];
    config.providers.push(dead);
    config
        .toggle_tier_model(ModelTier::Standard, "dead", "mini-a")
        .unwrap();
    let routed = OpenAiCompatibleClient::from_tier(&config, &paths, ModelTier::Standard).unwrap();
    let notice = routed
        .notice
        .expect("unbuildable pool must explain the fallback");
    assert!(
        notice.contains("tier 'standard' pool is unavailable"),
        "{notice}"
    );
    assert_ne!(routed.client.provider.id, "dead");
}

#[test]
fn aux_role_follows_its_configured_tier_or_the_main_pool() {
    let temp = tempfile::tempdir().unwrap();
    let paths = test_paths(temp.path());
    let mut config = config_with_models(&["mini-a"]);
    let provider_id = active_provider_id(&config);
    config
        .toggle_tier_model(ModelTier::Cheap, &provider_id, "mini-a")
        .unwrap();
    config
        .model_tiers
        .roles
        .insert(AuxRole::SessionTitle.key().to_string(), "cheap".to_string());

    let routed =
        OpenAiCompatibleClient::from_aux_role(&config, &paths, AuxRole::SessionTitle).unwrap();
    assert_eq!(routed.provider.default_model, "mini-a");

    // An unrouted role is byte-for-byte the main pool client.
    let unrouted =
        OpenAiCompatibleClient::from_aux_role(&config, &paths, AuxRole::MemoryOrganizer).unwrap();
    let main = OpenAiCompatibleClient::from_config(&config, &paths).unwrap();
    assert_eq!(unrouted.provider.default_model, main.provider.default_model);
    assert_eq!(unrouted.endpoints.len(), main.endpoints.len());
}
