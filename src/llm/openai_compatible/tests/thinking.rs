//! 思考变体的保存、迁移与线路映射。

use super::shared::*;
use crate::llm::openai_compatible::*;

#[test]
fn reasoning_failover_visibility_only_follows_reasoning_display() {
    let mut config = AppConfig::default();
    assert_eq!(reasoning_visibility(&config), ReasoningVisibility::Summary);

    config.display.reasoning = " full ".to_string();
    assert_eq!(reasoning_visibility(&config), ReasoningVisibility::Full);

    config.display.reasoning = "hidden".to_string();
    config.display.tool_calls = "FULL".to_string();
    assert_eq!(reasoning_visibility(&config), ReasoningVisibility::Hidden);
}

#[test]
fn subagent_output_visibility_follows_tool_detail_mode() {
    let provider = test_provider("openai", "https://api.openai.com/v1");
    let hidden = test_client(provider.clone()).for_subagent_output(false);
    assert_eq!(hidden.reasoning_visibility, ReasoningVisibility::Hidden);
    assert!(!hidden.detailed_reasoning_summary);

    let full = test_client(provider).for_subagent_output(true);
    assert_eq!(full.reasoning_visibility, ReasoningVisibility::Full);
    assert!(full.detailed_reasoning_summary);
}

#[test]
fn client_constructors_restore_saved_thinking_variants() {
    let temp = tempfile::tempdir().unwrap();
    let paths = test_paths(temp.path());
    std::fs::create_dir_all(&paths.state_dir).unwrap();

    let mut provider = test_provider("custom", "https://example.com/v1");
    provider.default_model = "reasoning-model".to_string();
    provider.models = vec![provider.default_model.clone()];
    provider.api_key = Some("test-key".to_string());
    let preferences = ThinkingVariantPreferences {
        selected: HashMap::from([(
            thinking_variant_key(&provider.id, &provider.default_model),
            "high".to_string(),
        )]),
        ..ThinkingVariantPreferences::default()
    };
    std::fs::write(
        thinking_variant_preferences_file(&paths),
        serde_json::to_string(&preferences).unwrap(),
    )
    .unwrap();

    let config = AppConfig {
        active_provider: provider.id.clone(),
        active_provider_models: None,
        providers: vec![provider.clone()],
        ..AppConfig::default()
    };

    let configured = OpenAiCompatibleClient::from_config(&config, &paths).unwrap();
    assert_eq!(configured.selected_thinking_variant_id(), Some("high"));

    let direct = OpenAiCompatibleClient::new(&provider, &config, &paths).unwrap();
    assert_eq!(direct.selected_thinking_variant_id(), Some("high"));
}

#[test]
fn saving_thinking_variants_preserves_inactive_models_and_clears_unset_active_model() {
    let temp = tempfile::tempdir().unwrap();
    let paths = test_paths(temp.path());
    std::fs::create_dir_all(&paths.state_dir).unwrap();
    let inactive_key = thinking_variant_key("inactive", "old-model");
    let active_key = thinking_variant_key("custom", "reasoning-model");
    let preferences = ThinkingVariantPreferences {
        selected: HashMap::from([(inactive_key.clone(), "max".to_string())]),
        ..ThinkingVariantPreferences::default()
    };
    std::fs::write(
        thinking_variant_preferences_file(&paths),
        serde_json::to_string(&preferences).unwrap(),
    )
    .unwrap();

    let mut provider = test_provider("custom", "https://example.com/v1");
    provider.default_model = "reasoning-model".to_string();
    let mut client = test_client(provider);
    client
        .thinking_variants
        .insert(active_key.clone(), "high".to_string());
    client.save_thinking_variants(&paths).unwrap();

    let saved = load_thinking_variant_preferences(&paths);
    assert_eq!(
        saved.selected.get(&inactive_key).map(String::as_str),
        Some("max")
    );
    assert_eq!(
        saved.selected.get(&active_key).map(String::as_str),
        Some("high")
    );

    client.thinking_variants.remove(&active_key);
    client.save_thinking_variants(&paths).unwrap();
    let saved = load_thinking_variant_preferences(&paths);
    assert_eq!(
        saved.selected.get(&inactive_key).map(String::as_str),
        Some("max")
    );
    assert!(!saved.selected.contains_key(&active_key));
}

#[test]
fn staged_thinking_variant_update_merges_only_the_edited_inactive_model() {
    let temp = tempfile::tempdir().unwrap();
    let paths = test_paths(temp.path());
    let mut staged = ThinkingVariantPreferences::load(&paths);
    staged.set("future-provider", "future-model", Some("high".to_string()));

    std::fs::create_dir_all(&paths.state_dir).unwrap();
    let concurrent_key = thinking_variant_key("other-provider", "other-model");
    let concurrent = ThinkingVariantPreferences {
        selected: HashMap::from([(concurrent_key.clone(), "max".to_string())]),
        ..ThinkingVariantPreferences::default()
    };
    std::fs::write(
        thinking_variant_preferences_file(&paths),
        serde_json::to_string(&concurrent).unwrap(),
    )
    .unwrap();

    staged.save(&paths).unwrap();

    let saved = ThinkingVariantPreferences::load(&paths);
    assert_eq!(
        saved.selected("future-provider", "future-model"),
        Some("high")
    );
    assert_eq!(
        saved.selected.get(&concurrent_key).map(String::as_str),
        Some("max")
    );
}

#[test]
fn malformed_thinking_variant_state_is_not_overwritten() {
    let temp = tempfile::tempdir().unwrap();
    let paths = test_paths(temp.path());
    std::fs::create_dir_all(&paths.state_dir).unwrap();
    let path = thinking_variant_preferences_file(&paths);
    std::fs::write(&path, "{not-json").unwrap();
    let mut preferences = ThinkingVariantPreferences::load(&paths);
    preferences.set("provider", "model", Some("high".to_string()));

    let error = preferences.save(&paths).unwrap_err();

    assert!(format!("{error:#}").contains("failed to parse thinking variant state"));
    assert_eq!(std::fs::read_to_string(path).unwrap(), "{not-json");
}

#[test]
fn thinking_variant_preferences_follow_provider_renames() {
    let temp = tempfile::tempdir().unwrap();
    let paths = test_paths(temp.path());
    std::fs::create_dir_all(&paths.state_dir).unwrap();
    let preferences = ThinkingVariantPreferences {
        selected: HashMap::from([
            (thinking_variant_key("old", "first"), "high".to_string()),
            (thinking_variant_key("old", "second"), "max".to_string()),
            (thinking_variant_key("other", "first"), "low".to_string()),
        ]),
        ..ThinkingVariantPreferences::default()
    };
    std::fs::write(
        thinking_variant_preferences_file(&paths),
        serde_json::to_string(&preferences).unwrap(),
    )
    .unwrap();
    let mut preferences = ThinkingVariantPreferences::load(&paths);

    preferences.set("old", "second", Some("low".to_string()));
    preferences.rename_provider("old", "new");
    let mut concurrent = ThinkingVariantPreferences::load(&paths);
    concurrent.set("old", "first", Some("medium".to_string()));
    concurrent.set("old", "second", Some("high".to_string()));
    concurrent.set("old", "late", Some("medium".to_string()));
    concurrent.save(&paths).unwrap();
    preferences.save(&paths).unwrap();

    let saved = ThinkingVariantPreferences::load(&paths);
    assert_eq!(saved.selected("new", "first"), Some("medium"));
    assert_eq!(saved.selected("new", "second"), Some("low"));
    assert_eq!(saved.selected("new", "late"), Some("medium"));
    assert_eq!(saved.selected("other", "first"), Some("low"));
    assert_eq!(saved.selected("old", "first"), None);
    assert_eq!(saved.selected("old", "second"), None);
    assert_eq!(saved.selected("old", "late"), None);
}

#[test]
fn provider_rename_replays_when_the_initial_variant_snapshot_was_empty() {
    let temp = tempfile::tempdir().unwrap();
    let paths = test_paths(temp.path());
    let mut renaming = ThinkingVariantPreferences::load(&paths);
    renaming.rename_provider("old", "new");

    let mut concurrent = ThinkingVariantPreferences::load(&paths);
    concurrent.set("old", "late", Some("high".to_string()));
    concurrent.save(&paths).unwrap();
    renaming.save(&paths).unwrap();

    let saved = ThinkingVariantPreferences::load(&paths);
    assert_eq!(saved.selected("new", "late"), Some("high"));
    assert_eq!(saved.selected("old", "late"), None);
}

#[test]
fn concurrent_thinking_variant_updates_keep_distinct_models() {
    let temp = tempfile::tempdir().unwrap();
    let paths = test_paths(temp.path());
    let barrier = Arc::new(std::sync::Barrier::new(2));
    let handles = ["first", "second"].map(|model| {
        let paths = paths.clone();
        let barrier = barrier.clone();
        std::thread::spawn(move || {
            let mut preferences = ThinkingVariantPreferences::load(&paths);
            preferences.set("provider", model, Some("high".to_string()));
            barrier.wait();
            preferences.save(&paths).unwrap();
        })
    });
    for handle in handles {
        handle.join().unwrap();
    }

    let saved = ThinkingVariantPreferences::load(&paths);
    assert_eq!(saved.selected("provider", "first"), Some("high"));
    assert_eq!(saved.selected("provider", "second"), Some("high"));
}

#[test]
fn reasoning_variants_use_current_wire_protocol_mapping() {
    let info = ModelReasoningInfo {
        provider_npm: Some("@openrouter/ai-sdk-provider".to_string()),
        variants: Vec::new(),
    };
    let effort = ReasoningVariant {
        id: "high".to_string(),
        setting: ReasoningSetting::Effort("high".to_string()),
    };
    let budget = ReasoningVariant {
        id: "max".to_string(),
        setting: ReasoningSetting::BudgetTokens(8000),
    };
    let provider = test_provider("openrouter", "https://openrouter.ai/api/v1");
    assert!(reasoning_variant_supported(
        &provider,
        "test-model",
        &info,
        &effort
    ));
    assert!(reasoning_variant_supported(
        &provider,
        "test-model",
        &info,
        &budget
    ));

    let unknown_info = ModelReasoningInfo {
        provider_npm: Some("@unknown/provider".to_string()),
        variants: Vec::new(),
    };
    let unknown = test_provider("proxy", "https://proxy.example/v1");
    assert!(reasoning_variant_supported(
        &unknown,
        "test-model",
        &unknown_info,
        &effort
    ));
    assert!(!reasoning_variant_supported(
        &unknown,
        "test-model",
        &unknown_info,
        &budget
    ));

    let alibaba = test_provider("alibaba-token-plan", "https://example.com/v1");
    let toggle = ReasoningVariant {
        id: "on".to_string(),
        setting: ReasoningSetting::Toggle(true),
    };
    assert!(reasoning_variant_supported(
        &alibaba,
        "test-model",
        &unknown_info,
        &toggle
    ));

    assert!(reasoning_variant_supported(
        &unknown,
        "gpt-5-mini",
        &unknown_info,
        &toggle
    ));
    assert!(!reasoning_variant_supported(
        &unknown,
        "gpt-4.1",
        &unknown_info,
        &toggle
    ));
    assert!(!reasoning_variant_supported_for_protocol(
        &unknown,
        &unknown_info,
        &toggle,
        ProviderProtocol::OpenAiChat
    ));
}

#[test]
fn custom_openai_compatible_provider_uses_reasoning_effort() {
    let mut provider = test_provider("ririxin", "https://token.sensenova.cn/v1");
    provider.default_model = "deepseek-v4-flash".to_string();
    let info = ModelReasoningInfo {
        provider_npm: Some("@ai-sdk/openai-compatible".to_string()),
        variants: Vec::new(),
    };

    let body = chat_variant_body(
        &provider,
        &info,
        ReasoningSetting::Effort("high".to_string()),
    )
    .unwrap();
    assert_eq!(body["reasoning_effort"], "high");
    assert!(body.get("reasoning").is_none());
}

#[test]
fn mixed_client_keeps_variants_per_provider_and_model() {
    let mut first = test_provider("ririxin", "https://token.sensenova.cn/v1");
    first.default_model = "deepseek-v4-flash".to_string();
    let mut second = test_provider("opencode", "https://opencode.ai/zen/v1");
    second.default_model = "mimo-v2.5-free".to_string();
    let first_client = reqwest::Client::new();
    let second_client = reqwest::Client::new();
    let endpoints = vec![
        LlmEndpoint {
            client: first_client.clone(),
            provider: first.clone(),
            api_key: "first".to_string(),
            key_index: 0,
        },
        LlmEndpoint {
            client: second_client,
            provider: second,
            api_key: "second".to_string(),
            key_index: 0,
        },
    ];
    let mut client = OpenAiCompatibleClient {
        client: first_client,
        provider: first,
        api_key: "first".to_string(),
        endpoints: Arc::new(endpoints),
        thinking_variants: HashMap::from([(
            thinking_variant_key("ririxin", "deepseek-v4-flash"),
            "high".to_string(),
        )]),
        reasoning_visibility: ReasoningVisibility::Summary,
        buffered_delivery: false,
        detailed_reasoning_summary: false,
        request_timeouts: None,
        max_tokens_override: None,
        request_scope: "chat",
        claude_code: None,
        antigravity: None,
        codex: None,
        claude_code_dev_mode: false,
        continuation_health: ResponsesContinuationHealth::detached(),
    };

    let first_endpoint = client.with_endpoint(&client.endpoints[0]);
    let second_endpoint = client.with_endpoint(&client.endpoints[1]);
    assert_eq!(first_endpoint.selected_thinking_variant_id(), Some("high"));
    assert_eq!(second_endpoint.selected_thinking_variant_id(), None);
    client.thinking_variants.insert(
        thinking_variant_key("opencode", "mimo-v2.5-free"),
        "max".to_string(),
    );
    let second_endpoint = client.with_endpoint(&client.endpoints[1]);
    assert_eq!(second_endpoint.selected_thinking_variant_id(), Some("max"));
    assert_eq!(first_endpoint.selected_thinking_variant_id(), Some("high"));
}

#[test]
fn variant_extra_body_merges_nested_reasoning_fields() {
    let base = json!({ "reasoning": { "exclude": true }, "custom": 1 })
        .as_object()
        .cloned();
    let variant = json!({ "reasoning": { "effort": "high" } })
        .as_object()
        .cloned();

    let merged = merge_extra_body(base, variant).unwrap();
    assert_eq!(merged["reasoning"]["exclude"], true);
    assert_eq!(merged["reasoning"]["effort"], "high");
    assert_eq!(merged["custom"], 1);
}
