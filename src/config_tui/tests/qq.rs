//! QQ 会话、人格与模型选择。

use crate::config::{
    AppConfig, PlatformConversationKind, PlatformModelPoolInheritance, PlatformPersonaOverride,
};
use crate::config_tui::{
    parse_id_lines, parse_id_list, parse_keyword_lines, platform_conversation_id_label,
    platform_conversation_kind_label, platform_persona_summary, route_pool_summary, t,
    vision_provider_model_choice_values, PersonaMenuTarget,
};

#[test]
fn route_pool_and_id_helpers_express_inheritance_and_positive_ids() {
    assert_eq!(
        route_pool_summary(None, PlatformModelPoolInheritance::Platform),
        t("inherit platform", "继承 QQ 平台池")
    );
    assert_eq!(
        route_pool_summary(Some(&[]), PlatformModelPoolInheritance::Platform),
        t("inherit platform", "继承 QQ 平台池")
    );
    assert_eq!(
        route_pool_summary(None, PlatformModelPoolInheritance::Global),
        t("inherit global", "继承全局池")
    );
    assert_eq!(parse_id_list("123, 456").unwrap(), vec![123, 456]);
    assert!(parse_id_list("0").is_err());
    assert!(parse_id_list("-1").is_err());
    assert_eq!(parse_id_lines("123\n456\n123\n").unwrap(), vec![123, 456]);
    assert!(parse_id_lines("123\ninvalid\n456").is_err());
    assert_eq!(
        parse_keyword_lines("Miyu\n 小羽 \nMiyu").unwrap(),
        vec!["Miyu", "小羽"]
    );
}

#[test]
fn qq_batch_inputs_are_line_based_trimmed_and_deduplicated() {
    assert_eq!(
        parse_id_lines(" 123 \r\n\r\n456\n123\n").unwrap(),
        vec![123, 456]
    );
    assert!(parse_id_lines("123,456").is_err());
    assert_eq!(
        parse_keyword_lines(" Miyu \r\n\r\n小羽\nMiyu\n").unwrap(),
        vec!["Miyu", "小羽"]
    );
}

#[test]
fn qq_conversation_labels_are_localized_and_id_label_tracks_type() {
    assert_eq!(
        platform_conversation_kind_label(PlatformConversationKind::Private),
        t("Private chat", "私聊")
    );
    assert_eq!(
        platform_conversation_kind_label(PlatformConversationKind::Group),
        t("Group chat", "群聊")
    );
    assert_eq!(
        platform_conversation_id_label(PlatformConversationKind::Private),
        t("QQ id", "QQ 号")
    );
    assert_eq!(
        platform_conversation_id_label(PlatformConversationKind::Group),
        t("Group id", "群号")
    );
}

#[test]
fn qq_conversation_persona_summary_distinguishes_inheritance_and_miyu() {
    assert_eq!(
        platform_persona_summary(&PlatformPersonaOverride::Inherit),
        t("inherit current persona", "继承当前人格")
    );
    assert_eq!(
        platform_persona_summary(&PlatformPersonaOverride::Miyu),
        "Miyu"
    );
    assert_eq!(
        platform_persona_summary(&PlatformPersonaOverride::Custom {
            name: "Group.md".to_string()
        }),
        "Group"
    );
}

#[test]
fn qq_persona_menu_target_isolated_from_global_persona_and_tracks_renames() {
    let mut config = AppConfig::default();
    config.prompt.active_persona = "Global.md".to_string();
    let mut target = PersonaMenuTarget::Platform(PlatformPersonaOverride::Inherit);

    assert_eq!(target.custom_offset(), 2);
    target.activate_custom(&mut config, "Session.md".to_string());
    assert_eq!(config.prompt.active_persona, "Global.md");
    assert_eq!(target.custom_name(&config), Some("Session.md"));
    assert_eq!(target.pending_reference_count("Session.md"), 1);

    target.rename_custom("Session.md", "Renamed.md");
    assert_eq!(target.custom_name(&config), Some("Renamed.md"));
    assert_eq!(target.pending_reference_count("Session.md"), 0);
    assert_eq!(target.pending_reference_count("Renamed.md"), 1);

    target.activate_miyu(&mut config);
    assert!(target.is_miyu(&config));
    assert_eq!(config.prompt.active_persona, "Global.md");
    target.activate_inherit();
    assert!(matches!(
        target,
        PersonaMenuTarget::Platform(PlatformPersonaOverride::Inherit)
    ));
}

#[test]
fn global_persona_menu_target_preserves_activation_behavior() {
    let mut config = AppConfig::default();
    let mut target = PersonaMenuTarget::Global;

    assert_eq!(target.custom_offset(), 1);
    assert!(target.is_miyu(&config));
    target.activate_custom(&mut config, "Global.md".to_string());
    assert_eq!(target.custom_name(&config), Some("Global.md"));
    assert_eq!(target.pending_reference_count("Global.md"), 0);

    target.activate_miyu(&mut config);
    assert!(config.prompt.active_persona.is_empty());
    assert!(target.is_miyu(&config));
}

#[test]
fn explicit_vision_choices_only_include_image_capable_models() {
    let mut config = AppConfig::default();
    let provider = config
        .providers
        .iter_mut()
        .find(|provider| !provider.is_builtin_cli_provider())
        .unwrap();
    provider.models = vec!["text-only".to_string(), "vision".to_string()];
    provider
        .model_modalities
        .insert("text-only".to_string(), vec!["text".to_string()]);
    provider.model_modalities.insert(
        "vision".to_string(),
        vec!["text".to_string(), "image".to_string()],
    );
    let provider_id = provider.id.clone();

    let choices = vision_provider_model_choice_values(&config);

    assert!(choices.contains(&format!("{provider_id}\tvision")));
    assert!(!choices.contains(&format!("{provider_id}\ttext-only")));
}

/// 服务端下架模型之后，配置里会留一条指向不存在模型的残留。多模态/嵌入两个
/// 列表原本没有删除键，那条残留在 TUI 里删不掉——只能手改 config.jsonc。
///
/// 删除走的是文本模型列表同一个入口，语义是「整条删掉」：从供应商的 models
/// 里移除，连带清掉模态、上下文窗口，以及它在各个池子里的引用。
#[test]
fn removing_a_stale_model_clears_every_reference_to_it() {
    let mut config = AppConfig::default();
    let provider = config
        .providers
        .iter_mut()
        .find(|provider| !provider.is_builtin_cli_provider())
        .unwrap();
    let provider_id = provider.id.clone();
    provider.models = vec!["gone".to_string(), "alive".to_string()];
    for model in ["gone", "alive"] {
        provider.model_modalities.insert(
            model.to_string(),
            vec!["text".to_string(), "image".to_string()],
        );
        provider
            .model_context_window
            .insert(model.to_string(), 128_000);
    }
    config
        .toggle_active_multimodal_provider_model(&provider_id, "gone")
        .unwrap();
    config
        .toggle_active_multimodal_provider_model(&provider_id, "alive")
        .unwrap();
    config.embedding.provider_id = provider_id.clone();
    config.embedding.model = "gone".to_string();
    assert!(config.is_active_multimodal_provider_model(&provider_id, "gone"));

    config
        .remove_active_provider_model(&provider_id, "gone")
        .unwrap();

    let provider = config
        .providers
        .iter()
        .find(|provider| !provider.is_builtin_cli_provider())
        .unwrap();
    assert!(!provider.models.iter().any(|model| model == "gone"));
    assert!(!provider.model_modalities.contains_key("gone"));
    assert!(!provider.model_context_window.contains_key("gone"));
    assert!(!config.is_active_multimodal_provider_model(&provider_id, "gone"));
    // 同一个供应商里还活着的那个不受牵连
    assert!(config.is_active_multimodal_provider_model(&provider_id, "alive"));
}
