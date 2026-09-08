//! 手填模型名（供应商目录里没有的内测模型）在模型菜单里的行为。

use crate::config::{ActiveProviderModelConfig, AppConfig, ModelTier, ProviderConfig};
use crate::config_tui::{group_models, insert_custom_model, remove_custom_model};

fn config_with_provider() -> AppConfig {
    let mut config = AppConfig::default();
    config.providers.clear();
    let mut provider = ProviderConfig::default_opencodezen();
    provider.id = "p1".to_string();
    provider.models = vec!["listed-1".to_string()];
    provider.default_model = "listed-1".to_string();
    config.providers.push(provider);
    config
}

fn names(entries: &[crate::config_tui::ModelEntry]) -> Vec<String> {
    entries.iter().map(|entry| entry.full.clone()).collect()
}

/// 手填的排在拉取结果最前面；供应商目录里也有同一个名字时（内置 CLI 供应商的
/// 目录本来就并了 `models`）只留置顶那一份。
#[test]
fn custom_models_pin_to_the_top_without_doubling_up() {
    let custom = vec!["beta-x".to_string(), "listed-1".to_string()];
    let raw = vec![
        "listed-1".to_string(),
        "listed-2".to_string(),
        "beta-x".to_string(),
    ];
    let grouped = group_models(&custom, &raw, "");
    assert_eq!(
        names(&grouped["All"]),
        ["beta-x", "listed-1", "listed-2"],
        "手填的置顶，且每个名字只出现一次"
    );
}

/// 带组织前缀的手填模型在自己那一组里也置顶，"All" 组恒收全部。
#[test]
fn custom_models_pin_inside_their_organization_too() {
    let custom = vec!["acme/beta".to_string()];
    let raw = vec!["acme/stable".to_string(), "plain".to_string()];
    let grouped = group_models(&custom, &raw, "");
    assert_eq!(
        names(&grouped["All"]),
        ["acme/beta", "acme/stable", "plain"]
    );
    assert_eq!(names(&grouped["acme"]), ["acme/beta", "acme/stable"]);
    // 组织列里显示的是去掉前缀的短名。
    assert_eq!(grouped["acme"][0].name, "beta");
}

/// 搜索对手填的模型一视同仁。
#[test]
fn the_search_filter_applies_to_custom_models() {
    let custom = vec!["beta-x".to_string()];
    let raw = vec!["listed-1".to_string()];
    assert_eq!(
        names(&group_models(&custom, &raw, "beta")["All"]),
        ["beta-x"]
    );
    assert!(group_models(&custom, &raw, "zzz").is_empty());
}

/// 加一个手填模型：记进手填清单并同时激活；供应商还没有默认模型时顺手补上。
#[test]
fn inserting_a_custom_model_records_and_activates_it() {
    let mut config = config_with_provider();
    config.providers[0].models.clear();
    config.providers[0].default_model.clear();
    assert!(insert_custom_model(
        &mut config,
        0,
        &["listed-1".to_string()],
        "beta-x"
    ));
    assert_eq!(config.providers[0].custom_models, ["beta-x"]);
    assert_eq!(config.providers[0].models, ["beta-x"]);
    assert_eq!(config.providers[0].default_model, "beta-x");
}

/// 已经在供应商目录里、或者已经手填过的名字都不重复记：目录里的本来就是正常
/// 模型，再记一份只会让它被永久置顶，还多一条删得掉的假条目。
#[test]
fn a_name_already_known_is_not_recorded_twice() {
    let mut config = config_with_provider();
    let raw = vec!["listed-1".to_string()];
    assert!(!insert_custom_model(&mut config, 0, &raw, "listed-1"));
    assert!(config.providers[0].custom_models.is_empty());

    assert!(insert_custom_model(&mut config, 0, &raw, "beta-x"));
    assert!(!insert_custom_model(&mut config, 0, &raw, "beta-x"));
    assert_eq!(config.providers[0].custom_models, ["beta-x"]);
    assert_eq!(config.providers[0].models, ["listed-1", "beta-x"]);
}

/// 取消激活（Tab）只把它移出 `models`，手填清单还留着，所以它照样列在最上面
/// ——这正是手填的名字要有自己落脚点的原因：只记在 `models` 里的话，一取消
/// 激活它就从配置里没了，列表其余部分全来自拉取结果，它也就跟着消失。
#[test]
fn deactivating_a_custom_model_keeps_it_listed() {
    let mut config = config_with_provider();
    let raw = vec!["listed-1".to_string()];
    insert_custom_model(&mut config, 0, &raw, "beta-x");

    config.providers[0].models.retain(|model| model != "beta-x");

    assert_eq!(config.providers[0].custom_models, ["beta-x"]);
    let grouped = group_models(&config.providers[0].custom_models, &raw, "");
    assert_eq!(names(&grouped["All"]), ["beta-x", "listed-1"]);
}

/// 删除手填模型：手填清单、激活状态、按模型的设置、各处池子引用一起清掉，
/// `default_model` 让位给还在的模型。
#[test]
fn deleting_a_custom_model_clears_every_trace() {
    let mut config = config_with_provider();
    insert_custom_model(&mut config, 0, &["listed-1".to_string()], "beta-x");
    config.providers[0].default_model = "beta-x".to_string();
    config.providers[0]
        .model_context_window
        .insert("beta-x".to_string(), 32_000);
    config.providers[0]
        .model_modalities
        .insert("beta-x".to_string(), vec!["text".to_string()]);
    let reference = ActiveProviderModelConfig {
        provider_id: "p1".to_string(),
        model: "beta-x".to_string(),
    };
    config.active_provider_models = Some(vec![reference.clone()]);
    config
        .model_tiers
        .pool_mut(ModelTier::Lite)
        .push(reference.clone());

    assert!(remove_custom_model(&mut config, 0, "beta-x"));

    let provider = &config.providers[0];
    assert!(provider.custom_models.is_empty());
    assert!(!provider.models.iter().any(|model| model == "beta-x"));
    assert!(!provider.model_context_window.contains_key("beta-x"));
    assert!(!provider.model_modalities.contains_key("beta-x"));
    assert_eq!(provider.default_model, "listed-1");
    // 池子空了会被收成 None（见 `retain_nonempty_pool`），两种表示都算清干净。
    assert!(!config
        .active_provider_models
        .as_ref()
        .is_some_and(|pool| pool.iter().any(|entry| entry.model == "beta-x")));
    assert!(config.model_tiers.pool(ModelTier::Lite).is_empty());
}

/// 拉取来的模型删不掉：它是供应商目录的内容，这里只是显示。
#[test]
fn a_fetched_model_cannot_be_deleted_from_the_model_column() {
    let mut config = config_with_provider();
    assert!(!remove_custom_model(&mut config, 0, "listed-1"));
    assert_eq!(config.providers[0].models, ["listed-1"]);
}
