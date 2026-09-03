//! 其余默认值与显示设置。

use crate::config::*;

#[test]
fn context_overflow_defaults_to_compact() {
    assert_eq!(ContextConfig::default().on_overflow, "compact");

    let deserialized: ContextConfig = serde_json::from_value(serde_json::json!({})).unwrap();
    assert_eq!(deserialized.on_overflow, "compact");
}

#[test]
fn vision_timeouts_have_stable_defaults() {
    let vision: VisionPluginConfig = serde_json::from_value(serde_json::json!({})).unwrap();
    // 09-03 用户裁定:三个超时只剩"防僵尸"一个用途,统一一小时。
    assert_eq!(vision.response_header_timeout_seconds, 3600);
    assert_eq!(vision.stream_idle_timeout_seconds, 3600);
    assert_eq!(vision.image_timeout_seconds, 3600);
}

#[test]
fn mixed_context_window_uses_the_global_default_when_model_metadata_is_missing() {
    let mut config = AppConfig::default();
    let provider = &mut config.providers[0];
    let provider_id = provider.id.clone();
    provider.models = vec![
        "miyu-known-window-model".to_string(),
        "miyu-unknown-window-model".to_string(),
    ];
    provider.default_model = provider.models[0].clone();
    provider
        .model_context_window
        .insert(provider.models[0].clone(), 200_000);
    config.active_provider_models = Some(vec![
        ActiveProviderModelConfig {
            provider_id: provider_id.clone(),
            model: provider.models[0].clone(),
        },
        ActiveProviderModelConfig {
            provider_id,
            model: provider.models[1].clone(),
        },
    ]);

    assert_eq!(config.active_context_window().unwrap(), Some(168_000));
    config.providers[0]
        .model_context_window
        .insert("miyu-unknown-window-model".to_string(), 128_000);
    assert_eq!(config.active_context_window().unwrap(), Some(128_000));
}

#[test]
fn display_readable_tool_names_defaults_enabled() {
    let display: DisplayConfig = serde_json::from_str(r#"{"tool_calls":"summary"}"#).unwrap();
    assert_eq!(display.language, "auto");
    assert!(display.readable_tool_names);
    assert!(!display.show_token_usage);
    assert_eq!(display.mixed_model_endpoint_display, "interactive");
    assert_eq!(display.command_output_lines, 10);

    let display: DisplayConfig = serde_json::from_str(r#"{"command_output_lines":3}"#).unwrap();
    assert_eq!(display.command_output_lines, 3);
    assert!(serde_json::to_string(&display)
        .unwrap()
        .contains(r#""command_output_lines":3"#));

    let mut config = AppConfig::default();
    config.display.command_output_lines = MAX_COMMAND_OUTPUT_LINES + 1;
    assert!(config.validate().is_err());

    let display: DisplayConfig = serde_json::from_str(r#"{"show_token_usage":true}"#).unwrap();
    assert!(display.show_token_usage);

    let display: DisplayConfig =
        serde_json::from_str(r#"{"show_mixed_model_endpoint":false}"#).unwrap();
    assert_eq!(display.mixed_model_endpoint_display, "off");

    let display: DisplayConfig =
        serde_json::from_str(r#"{"show_mixed_model_endpoint":true}"#).unwrap();
    assert_eq!(display.mixed_model_endpoint_display, "all");
}

#[test]
fn display_language_roundtrips_and_rejects_unknown_values() {
    let display: DisplayConfig = serde_json::from_str(r#"{"language":"zh"}"#).unwrap();
    assert_eq!(display.language, "zh");
    assert!(serde_json::to_string(&display)
        .unwrap()
        .contains(r#""language":"zh""#));

    let mut config = AppConfig::default();
    config.display.language = "fr".to_string();
    assert!(config.validate().is_err());
    config.display.language.clear();
    assert!(config.validate().is_err());
}

#[test]
fn display_language_hint_reads_jsonc_without_loading_full_config() {
    let temp = tempfile::tempdir().unwrap();
    let config_file = temp.path().join("config.jsonc");
    std::fs::write(
        &config_file,
        "{\n  // UI preference\n  \"display\": { \"language\": \"en\" }\n}\n",
    )
    .unwrap();
    let paths = MiyuPaths {
        root_dir: temp.path().to_path_buf(),
        config_dir: temp.path().to_path_buf(),
        config_file,
        skills_dir: temp.path().join("skills"),
        data_dir: temp.path().join("data"),
        cache_dir: temp.path().join("cache"),
        state_dir: temp.path().join("state"),
        pictures_dir: temp.path().join("pictures"),
        fish_hook_file: temp.path().join("miyu.fish"),
        bash_hook_file: temp.path().join("miyu.bash"),
        zsh_hook_file: temp.path().join("miyu.zsh"),
        scripts_dir: temp.path().join("scripts"),
        system_scripts_dir: temp.path().join("system-scripts"),
    };

    assert_eq!(
        AppConfig::display_language_hint(&paths).as_deref(),
        Some("en")
    );
}

#[test]
fn memory_diary_lifecycle_defaults_and_roundtrip_are_stable() {
    let defaults: MemoryConfig = serde_json::from_str("{}").unwrap();
    assert_eq!(defaults.diary_batch_size, 14);
    assert_eq!(defaults.short_diary_retention_days, 14);
    assert_eq!(defaults.diary_promotion_recalls, 3);
    assert_eq!(defaults.organizer_timeout_seconds, 120);
    assert!(!defaults.auto_skill_enabled);

    let parsed: MemoryConfig = serde_json::from_str(
        r#"{
            "diary_batch_size": 20,
            "short_diary_retention_days": 7,
            "diary_promotion_recalls": 4,
            "organizer_timeout_seconds": 90
        }"#,
    )
    .unwrap();
    assert_eq!(parsed.diary_batch_size, 20);
    assert_eq!(parsed.short_diary_retention_days, 7);
    assert_eq!(parsed.diary_promotion_recalls, 4);
    assert_eq!(parsed.organizer_timeout_seconds, 90);
}
