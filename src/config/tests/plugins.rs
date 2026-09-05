//! 插件设置的默认值、校验与迁移。

use super::shared::*;
use crate::config::*;

#[test]
fn legacy_real_context_history_limits_move_to_message_history() {
    let mut config = AppConfig::default();
    config.platforms.qq.plugins.insert(
        REAL_CONTEXT_PLUGIN_ID.to_string(),
        PlatformPluginInstanceConfig {
            enabled: None,
            settings: serde_json::json!({
                "history_search_max_results": 25,
                "history_safe_page_limit": 250,
                "allow_cross_group_search": false
            })
            .as_object()
            .unwrap()
            .clone(),
        },
    );

    config.normalize_platform_model_routes();

    let history = QqMessageHistoryPluginSettings::from_instance(
        &config.platforms.qq.plugins[QQ_MESSAGE_HISTORY_PLUGIN_ID],
    )
    .unwrap();
    assert_eq!(history.history_search_max_results, 25);
    assert_eq!(history.history_safe_page_limit, 250);
    assert!(!history.allow_cross_conversation_search);
    assert!(config
        .platforms
        .qq
        .plugins
        .get(REAL_CONTEXT_PLUGIN_ID)
        .is_none());
}

#[test]
fn real_context_defaults_match_the_deployed_contract() {
    let settings = RealContextPluginSettings::default();

    assert_eq!(settings.reply_context_window, 25);
    assert_eq!(settings.judge_context_window, 20);
    assert_eq!(settings.group_member_search_max_results, 200);
    assert!(settings.active_reply_enable);
    assert!(settings.judge_include_persona);
    assert!(settings.judge_persona_prompt.is_empty());
    // The judge runs on every group message: it ships on the lite tier and
    // affection follows the judge unless told otherwise.
    assert_eq!(
        settings.text_models.tier_ref(),
        Some(crate::config::ModelTier::Lite)
    );
    assert!(settings.affection_text_models.is_inherit());
    assert_eq!(settings.active_judge_probability, 0.05);
    assert_eq!(settings.reply_threshold, 0.8);
    assert_eq!(settings.judge_timeout_seconds, 60);
    assert_eq!(settings.judge_endpoint_timeout_seconds, 15);
    assert_eq!(settings.judge_max_concurrency, 4);
    assert_eq!(settings.judge_max_retries, 1);
    assert_eq!(settings.active_reply_supersede_window_seconds, 5);
    assert_eq!(settings.continuation_window_seconds, 15);
    assert!(settings.takeover_direct_trigger_enable);
    assert_eq!(settings.takeover_direct_trigger_boost_score, 0.3);
    assert!(settings.privileged_direct_trigger_skip_active_judgement);
    assert_eq!(settings.active_reply_reaction_emoji_ids, [289]);
    assert_eq!(settings.active_reply_reaction_timeout_seconds, 600);
    assert!(settings.reply_target_quote_enable);
    assert_eq!(settings.reply_target_quote_after_other_messages, 4);
    assert!(settings.reply_target_mention_enable);
    assert_eq!(settings.reply_target_mention_after_seconds, 15);
    assert_eq!(settings.moderation_min_severity, 7.0);
    assert_eq!(settings.base64_moderation_min_chars, 24);
    assert_eq!(settings.base64_moderation_max_decoded_chars, 5_000);
    assert_eq!(settings.base64_moderation_min_printable_ratio, 0.85);
    assert_eq!(settings.moderation_keywords.len(), 175);
    assert!(settings.identity_mappings.is_empty());
    assert!(!settings.affection_enable);
    assert!(settings.validate().is_ok());
}

#[test]
fn real_context_migrates_group_member_page_size_to_search_max_results() {
    let mut instance = PlatformPluginInstanceConfig {
        enabled: None,
        settings: serde_json::json!({ "group_member_page_size": 17 })
            .as_object()
            .unwrap()
            .clone(),
    };

    let settings = RealContextPluginSettings::from_instance(&instance).unwrap();
    assert_eq!(settings.group_member_search_max_results, 17);

    merge_real_context_settings(&mut instance, &settings);
    assert_eq!(instance.settings["group_member_search_max_results"], 17);
    assert!(!instance.settings.contains_key("group_member_page_size"));
}

#[test]
fn real_context_migrates_continuation_minutes_to_seconds() {
    let mut former_default = PlatformPluginInstanceConfig {
        enabled: None,
        settings: serde_json::json!({ "continuation_window_minutes": 3 })
            .as_object()
            .unwrap()
            .clone(),
    };
    let settings = RealContextPluginSettings::from_instance(&former_default).unwrap();
    // The old default must land on the current one, whatever that is.
    assert_eq!(
        settings.continuation_window_seconds,
        RealContextPluginSettings::default().continuation_window_seconds
    );
    merge_real_context_settings(&mut former_default, &settings);
    assert!(!former_default
        .settings
        .contains_key("continuation_window_minutes"));
    assert!(!former_default
        .settings
        .contains_key("continuation_window_seconds"));

    let mut custom = PlatformPluginInstanceConfig {
        enabled: None,
        settings: serde_json::json!({ "continuation_window_minutes": 7 })
            .as_object()
            .unwrap()
            .clone(),
    };
    let settings = RealContextPluginSettings::from_instance(&custom).unwrap();
    assert_eq!(settings.continuation_window_seconds, 420);
    merge_real_context_settings(&mut custom, &settings);
    assert_eq!(custom.settings["continuation_window_seconds"], 420);
    assert!(!custom.settings.contains_key("continuation_window_minutes"));
}

#[test]
fn a_legacy_shared_window_seeds_both_new_windows() {
    // One knob used to drive both the reply turn and the judge. Their best
    // values point opposite ways — the reply wants a generous opening
    // snapshot, the judge a tight recent window — so the knob split, and an
    // existing config has to land on its old value for both rather than
    // silently jumping to the new defaults.
    let mut settings = serde_json::Map::new();
    settings.insert("context_messages".to_string(), serde_json::json!(12));
    migrate_real_context_settings_map(&mut settings);
    assert_eq!(settings["reply_context_window"], 12);
    assert_eq!(settings["judge_context_window"], 12);

    // An explicit new value wins over the legacy one.
    let mut settings = serde_json::Map::new();
    settings.insert("context_messages".to_string(), serde_json::json!(12));
    settings.insert("judge_context_window".to_string(), serde_json::json!(30));
    migrate_real_context_settings_map(&mut settings);
    assert_eq!(settings["reply_context_window"], 12);
    assert_eq!(settings["judge_context_window"], 30);
}

#[test]
fn real_context_legacy_settings_migrate_and_deprecated_keys_are_removed() {
    let mut instance = PlatformPluginInstanceConfig {
        enabled: None,
        settings: serde_json::json!({
            "reply_context_messages": 37,
            "active_context_messages": 5,
            "takeover_system_trigger_enable": true,
            "takeover_system_trigger_boost_score": 0.4,
            "judge_models": [{"provider_id": "judge", "model": "primary"}],
            "affection_judge_models": [{"provider_id": "affection", "model": "secondary"}],
            "activity_statistics_enable": false,
            "future_option": {"value": 1}
        })
        .as_object()
        .unwrap()
        .clone(),
    };

    let settings = RealContextPluginSettings::from_instance(&instance).unwrap();
    assert_eq!(settings.reply_context_window, 37);
    assert_eq!(settings.judge_context_window, 37);
    assert!(settings.takeover_direct_trigger_enable);
    assert_eq!(settings.takeover_direct_trigger_boost_score, 0.4);
    assert_eq!(
        settings.text_models.explicit_models().unwrap()[0].provider_id,
        "judge"
    );

    merge_real_context_settings(&mut instance, &settings);
    assert_eq!(instance.settings["reply_context_window"], 37);
    // Migrated to `true`, which now equals the default and is pruned from
    // the persisted map; the effective value is asserted above.
    assert!(!instance
        .settings
        .contains_key("takeover_direct_trigger_enable"));
    assert_eq!(instance.settings["text_models"][0]["provider_id"], "judge");
    assert_eq!(instance.settings["future_option"]["value"], 1);
    for key in DEPRECATED_REAL_CONTEXT_SETTINGS {
        assert!(!instance.settings.contains_key(*key));
    }
}

#[test]
fn real_context_judge_persona_prompt_normalizes_validates_and_roundtrips() {
    let legacy =
        RealContextPluginSettings::from_instance(&PlatformPluginInstanceConfig::default()).unwrap();
    assert!(legacy.judge_persona_prompt.is_empty());

    let mut settings = RealContextPluginSettings {
        judge_persona_prompt: "  custom persona\n".to_string(),
        ..RealContextPluginSettings::default()
    };
    settings.normalize();
    assert_eq!(settings.judge_persona_prompt, "custom persona");
    assert!(settings.validate().is_ok());

    let mut instance = PlatformPluginInstanceConfig::default();
    instance
        .settings
        .insert("future_option".to_string(), serde_json::json!(true));
    merge_real_context_settings(&mut instance, &settings);
    assert_eq!(instance.settings["judge_persona_prompt"], "custom persona");
    assert_eq!(instance.settings["future_option"], true);
    let reparsed = RealContextPluginSettings::from_instance(&instance).unwrap();
    assert_eq!(reparsed.judge_persona_prompt, "custom persona");

    let mut cleared = reparsed;
    cleared.judge_persona_prompt = " \n ".to_string();
    cleared.normalize();
    merge_real_context_settings(&mut instance, &cleared);
    assert!(!instance.settings.contains_key("judge_persona_prompt"));
    assert_eq!(instance.settings["future_option"], true);

    assert!(RealContextPluginSettings {
        judge_persona_prompt: "bad\0prompt".to_string(),
        ..RealContextPluginSettings::default()
    }
    .validate()
    .is_err());
    assert!(RealContextPluginSettings {
        judge_persona_prompt: "x".repeat(32_769),
        ..RealContextPluginSettings::default()
    }
    .validate()
    .is_err());
}

#[test]
fn real_context_plugin_rejects_invalid_types_ranges_and_models() {
    let mut config = route_test_config();
    let mut instance = PlatformPluginInstanceConfig::default();
    instance.settings.insert(
        "active_judge_probability".to_string(),
        serde_json::json!(1.1),
    );
    config
        .platforms
        .qq
        .plugins
        .insert(REAL_CONTEXT_PLUGIN_ID.to_string(), instance);
    assert!(config.validate().is_err());

    config.platforms.qq.plugins.insert(
        REAL_CONTEXT_PLUGIN_ID.to_string(),
        PlatformPluginInstanceConfig {
            enabled: None,
            settings: serde_json::json!({"active_reply_enable": "yes"})
                .as_object()
                .unwrap()
                .clone(),
        },
    );
    assert!(config.validate().is_err());

    let mut settings = RealContextPluginSettings {
        text_models: crate::config::ModelPoolRef::models(vec![ActiveProviderModelConfig {
            provider_id: config.providers[0].id.clone(),
            model: "missing".to_string(),
        }]),
        ..RealContextPluginSettings::default()
    };
    let mut instance = PlatformPluginInstanceConfig::default();
    merge_real_context_settings(&mut instance, &settings);
    config
        .platforms
        .qq
        .plugins
        .insert(REAL_CONTEXT_PLUGIN_ID.to_string(), instance);
    assert!(config.validate().is_err());

    settings.text_models.explicit_models_mut().unwrap()[0].model = "text-only".to_string();
    merge_real_context_settings(
        config
            .platforms
            .qq
            .plugins
            .get_mut(REAL_CONTEXT_PLUGIN_ID)
            .unwrap(),
        &settings,
    );
    assert!(config.validate().is_ok());
}

#[test]
fn meme_library_defaults_follow_persona() {
    let memes = MemesPluginConfig::default();
    assert_eq!(memes.library_for_persona(""), "miyu");
    assert_eq!(
        memes.library_for_persona("Custom Persona"),
        "custom-persona"
    );
    assert!(memes.auto_send_enabled);
    assert!(memes.auto_send_platform_enabled);
    assert_eq!(memes.search_max_results, 1);
    assert_eq!(memes.auto_send_probability, 0.05);
}
