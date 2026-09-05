//! 插件设置的默认值与校验。

use crate::config::{
    AppConfig, PlatformPluginInstanceConfig, QqGroupJoinApprovalGroupConfig,
    RealContextPluginSettings, REAL_CONTEXT_PLUGIN_ID,
};
use crate::config_tui::{
    apply_real_context_values, apply_reply_processor_values, group_join_approval_group_label,
    group_join_approval_values, parse_real_context_identity_lines, parse_real_context_string_lines,
    real_context_values, reply_processor_mode_label, reply_processor_mode_value,
    reply_processor_values, t, upsert_group_join_approval_group, validate_reply_processor_settings,
    ReplyProcessorSettingsForm, REPLY_PROCESSOR_PLUGIN_ID,
};

#[test]
fn group_join_approval_defaults_to_enabled_with_empty_groups() {
    let config = AppConfig::default();
    let (enabled, settings) = group_join_approval_values(&config).unwrap();
    assert!(enabled);
    assert!(settings.groups.is_empty());
    assert_eq!(settings.timeout_seconds, 60);
    assert_eq!(settings.max_retries, 1);
    assert_eq!(
        settings.text_models.tier_ref(),
        Some(crate::config::ModelTier::Lite)
    );
}

#[test]
fn group_join_approval_upsert_keeps_one_entry_per_group() {
    let mut groups = vec![
        QqGroupJoinApprovalGroupConfig {
            group_id: 1,
            approve_condition: "first".to_string(),
        },
        QqGroupJoinApprovalGroupConfig {
            group_id: 2,
            approve_condition: "second".to_string(),
        },
    ];
    upsert_group_join_approval_group(
        &mut groups,
        QqGroupJoinApprovalGroupConfig {
            group_id: 1,
            approve_condition: "replaced".to_string(),
        },
    );
    assert_eq!(groups.len(), 2);
    assert_eq!(
        groups[0],
        QqGroupJoinApprovalGroupConfig {
            group_id: 1,
            approve_condition: "replaced".to_string(),
        }
    );
    assert!(
        group_join_approval_group_label(&groups[1]).starts_with("2 · "),
        "group label should contain the group id"
    );
}

#[test]
fn reply_processor_defaults_match_platform_contract() {
    let config = AppConfig::default();
    let (enabled, settings) = reply_processor_values(&config).unwrap();

    assert!(enabled);
    assert!(settings.default_enabled);
    assert_eq!(settings.threshold, 200);
    assert_eq!(settings.mode, "image");
    assert!(settings.followup_mention);
    assert!(settings.strip_period);
    assert_eq!(settings.theme, "paper");
    assert_eq!(settings.max_height, 2600);
    assert_eq!(settings.font_size, 36);
    assert_eq!(settings.code_font_size, 30);
    assert_eq!(settings.padding, 64);
    assert!(settings.context_notice);
    assert_eq!(settings.ttl_hours, 24);
    assert_eq!(settings.max_records, 3);
    assert!(settings.send_tool_intercept);
    assert!(settings.font.is_empty());
    assert!(settings.title_font.is_empty());
    assert!(settings.code_font.is_empty());
    assert!(settings.emoji_font.is_empty());
}

#[test]
fn reply_processor_mode_labels_preserve_config_values() {
    assert_eq!(
        reply_processor_mode_label("image"),
        t("Convert to image", "转图片")
    );
    assert_eq!(
        reply_processor_mode_label("forward"),
        t("Merged forward", "合并转发")
    );
    assert_eq!(reply_processor_mode_value("转图片"), Some("image"));
    assert_eq!(
        reply_processor_mode_value("Merged forward"),
        Some("forward")
    );
    assert_eq!(reply_processor_mode_value("unsupported"), None);
}

#[test]
fn reply_processor_settings_use_generic_map_and_preserve_unknown_keys() {
    let mut config = AppConfig::default();
    let mut instance = PlatformPluginInstanceConfig {
        enabled: Some(false),
        ..PlatformPluginInstanceConfig::default()
    };
    instance
        .settings
        .insert("future_option".to_string(), serde_json::json!({"value": 1}));
    config
        .platforms
        .qq
        .plugins
        .insert(REPLY_PROCESSOR_PLUGIN_ID.to_string(), instance);
    let settings = ReplyProcessorSettingsForm {
        threshold: 512,
        mode: "forward".to_string(),
        ..ReplyProcessorSettingsForm::default()
    };

    apply_reply_processor_values(&mut config, true, &settings).unwrap();

    let instance = &config.platforms.qq.plugins[REPLY_PROCESSOR_PLUGIN_ID];
    assert_eq!(instance.enabled, None);
    assert_eq!(instance.settings["threshold"], 512);
    assert_eq!(instance.settings["mode"], "forward");
    assert_eq!(instance.settings["future_option"]["value"], 1);
    let (enabled, reparsed) = reply_processor_values(&config).unwrap();
    assert!(enabled);
    assert_eq!(reparsed, settings);
}

#[test]
fn reply_processor_range_validation_rejects_unsafe_render_settings() {
    assert!(validate_reply_processor_settings(&ReplyProcessorSettingsForm::default()).is_ok());
    assert!(
        validate_reply_processor_settings(&ReplyProcessorSettingsForm {
            threshold: 0,
            ..ReplyProcessorSettingsForm::default()
        })
        .is_err()
    );
    assert!(
        validate_reply_processor_settings(&ReplyProcessorSettingsForm {
            max_height: 999,
            ..ReplyProcessorSettingsForm::default()
        })
        .is_err()
    );
    assert!(
        validate_reply_processor_settings(&ReplyProcessorSettingsForm {
            ttl_hours: 169,
            ..ReplyProcessorSettingsForm::default()
        })
        .is_err()
    );
}

#[test]
fn real_context_settings_use_generic_map_and_preserve_unknown_keys() {
    let mut config = AppConfig::default();
    let mut instance = PlatformPluginInstanceConfig::default();
    instance
        .settings
        .insert("future_option".to_string(), serde_json::json!({"value": 1}));
    config
        .platforms
        .qq
        .plugins
        .insert(REAL_CONTEXT_PLUGIN_ID.to_string(), instance);
    let settings = RealContextPluginSettings {
        reply_threshold: 0.9,
        reply_context_window: 42,
        judge_persona_prompt: "judge persona".to_string(),
        ..RealContextPluginSettings::default()
    };

    apply_real_context_values(&mut config, false, &settings);

    let instance = &config.platforms.qq.plugins[REAL_CONTEXT_PLUGIN_ID];
    assert_eq!(instance.enabled, Some(false));
    assert_eq!(instance.settings["reply_threshold"], 0.9);
    assert_eq!(instance.settings["reply_context_window"], 42);
    assert_eq!(instance.settings["judge_persona_prompt"], "judge persona");
    assert_eq!(instance.settings["future_option"]["value"], 1);
    let (enabled, reparsed) = real_context_values(&config).unwrap();
    assert!(!enabled);
    assert_eq!(reparsed, settings);
}

#[test]
fn real_context_batch_parsers_are_line_based_and_deduplicated() {
    let mappings =
        parse_real_context_identity_lines("# 昵称<Tab>QQ号\nMiyu\t123\n小羽 = 456").unwrap();
    assert_eq!(mappings.len(), 2);
    assert_eq!(mappings[0].nickname, "Miyu");
    assert_eq!(mappings[0].user_id, 123);
    assert!(parse_real_context_identity_lines("Miyu\t123\nMiyu\t456").is_err());
    assert!(parse_real_context_identity_lines("Miyu 123").is_err());

    assert_eq!(
        parse_real_context_string_lines("晚安\n 晚安 \nMiyu", 128).unwrap(),
        vec!["晚安", "Miyu"]
    );
}
