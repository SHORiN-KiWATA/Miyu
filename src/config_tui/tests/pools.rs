//! 分级模型池 / 平台模型配置 / Embedding 菜单的摘要措辞：界面只按 locale 显示一个
//! 名字，不再把配置 id 和翻译并排；「继承」说清继承到哪一层。

use crate::config::{
    AppConfig, AuxRole, EmbeddingBackend, ModelPoolRef, ModelTier, ProviderConfig,
};
use crate::config_tui::{
    aux_role_label, aux_role_summary, embedding_model_label, pool_ref_summary, qq_pool_slots, t,
    tier_hint, tier_pool_summary,
};

fn config_with_text_model() -> AppConfig {
    let mut config = AppConfig::default();
    let mut provider = ProviderConfig::default_opencodezen();
    provider.id = "p1".to_string();
    provider.models = vec!["m1".to_string()];
    config.providers.push(provider);
    config
}

#[test]
fn tier_rows_show_one_localized_name_and_inheritance() {
    let mut config = config_with_text_model();
    assert_eq!(tier_hint(ModelTier::Lite), t("lite", "轻量"));
    assert_eq!(
        tier_pool_summary(&config, ModelTier::Lite),
        t("inherits global pool", "继承全局池")
    );
    config
        .toggle_tier_model(ModelTier::Lite, "p1", "m1")
        .unwrap();
    assert_eq!(tier_pool_summary(&config, ModelTier::Lite), "m1");
}

#[test]
fn aux_roles_are_localized_and_never_repeat_the_fallback_note() {
    let config = AppConfig::default();
    assert_eq!(
        aux_role_label(AuxRole::DeepResearch),
        t("Deep research", "深度研究")
    );
    // 缺省档的池是空的，行内只写档名；空池回退全局池是档位行自己的事。
    assert_eq!(
        aux_role_summary(&config, AuxRole::SessionTitle),
        t("lite", "轻量")
    );
    assert_eq!(
        aux_role_summary(&config, AuxRole::DeepResearch),
        t("standard", "普通")
    );
    let mut config = config;
    config.model_tiers.set_role(AuxRole::SessionTitle, None);
    assert_eq!(
        aux_role_summary(&config, AuxRole::SessionTitle),
        t("global pool", "全局池")
    );
}

#[test]
fn pool_ref_summary_names_the_inherited_layer_without_config_ids() {
    let config = AppConfig::default();
    let inherit = t("inherits platform pool", "继承平台池");
    assert_eq!(
        pool_ref_summary(&config, &ModelPoolRef::inherit(), inherit),
        inherit
    );
    assert_eq!(
        pool_ref_summary(&config, &ModelPoolRef::global(), inherit),
        t("global pool", "全局池")
    );
    assert_eq!(
        pool_ref_summary(&config, &ModelPoolRef::tier(ModelTier::Cheap), inherit),
        t("cheap", "便宜")
    );
}

#[test]
fn qq_slots_are_named_platform_pools_and_inherit_upwards() {
    let slots = qq_pool_slots();
    assert_eq!(slots[0].label, t("Platform text pool", "平台文本池"));
    assert_eq!(
        slots[0].inherit_label,
        t("inherits global pool", "继承全局池")
    );
    assert_eq!(
        slots[1].label,
        t("Platform multimodal pool", "平台多模态池")
    );
    assert_eq!(
        slots[2].inherit_label,
        t("inherits platform pool", "继承平台池")
    );
}

#[test]
fn embedding_label_reads_local_or_remote_choice() {
    let mut config = AppConfig::default();
    assert_eq!(
        embedding_model_label(&config),
        format!("{} · bge-small-zh-v1.5-int8", t("local", "本地"))
    );
    config.embedding.provider_id = "p1".to_string();
    config.embedding.model = "embed".to_string();
    assert_eq!(embedding_model_label(&config), "p1/embed");
    config.embedding.backend = EmbeddingBackend::Local;
    assert_eq!(
        embedding_model_label(&config),
        format!("{} · bge-small-zh-v1.5-int8", t("local", "本地"))
    );
    config.embedding.enabled = false;
    assert_eq!(embedding_model_label(&config), t("disabled", "已关闭"));
}
