//! 平台配置与模型路由。

use super::shared::*;
use crate::config::*;

#[test]
fn platforms_config_roundtrip_and_default_omission() {
    let config = AppConfig::default();
    let json = serde_json::to_string(&config).unwrap();
    // An untouched platforms config stays out of the serialized file.
    assert!(!json.contains("platforms"));

    let mut parsed: AppConfig = serde_json::from_str(
        r#"{
            "active_provider": "opencode",
            "providers": [],
            "platforms": {
                "command_prefix": "!",
                "commands": {
                    "reset": { "permission": "everyone" }
                },
                "qq": {
                    "enabled": true,
                    "reverse_ws_port": 8400,
                    "access_token": "secret",
                    "admin_users": [9988],
                    "asset_base_url": "https://assets.example.test",
                    "memory": {
                        "write_enabled": false
                    },
                    "private_chats": {
                        "whitelist": [12345],
                        "friend_requests_require_private_whitelist": false,
                        "allow_non_whitelist": false,
                        "non_whitelist_rate_per_minute": 4
                    },
                    "group_chats": {
                        "whitelist": [54321],
                        "trigger_keywords": ["Miyu"],
                        "whitelist_rate_per_minute": 30,
                        "allow_non_whitelist": true,
                        "non_whitelist_rate_per_minute": 10
                    }
                }
            }
        }"#,
    )
    .unwrap();
    parsed.normalize_platform_model_routes();
    let qq = &parsed.platforms.qq;
    assert_eq!(parsed.platforms.command_prefix, "!");
    assert_eq!(
        parsed
            .platforms
            .command_permission("reset", PlatformCommandPermission::AdminOnly),
        PlatformCommandPermission::Everyone
    );
    assert!(qq.enabled);
    assert_eq!(qq.reverse_ws_port, 8400);
    assert_eq!(qq.access_token, "secret");
    assert_eq!(qq.admin_users, vec![9988]);
    assert!(qq.user_identification);
    assert!(qq.show_group_name);
    assert!(!qq.memory.write_enabled);
    assert_eq!(qq.asset_base_url, "https://assets.example.test");
    assert_eq!(qq.private_chats.whitelist, vec![12345]);
    assert!(!qq.private_chats.friend_requests_require_private_whitelist);
    assert!(!qq.private_chats.allow_non_whitelist);
    assert_eq!(
        qq.private_chats.non_whitelist_rate_limit,
        PlatformRateLimit {
            max_messages: 4,
            window_seconds: 60,
        }
    );
    assert_eq!(qq.group_chats.whitelist, vec![54321]);
    assert_eq!(qq.group_chats.trigger_keywords, vec!["Miyu"]);
    assert_eq!(qq.group_chats.whitelist_rate_limit.max_messages, 30);
    assert_eq!(qq.group_chats.non_whitelist_rate_limit.max_messages, 10);
    assert_eq!(qq.max_reply_chars, 3000);

    // Round-trip preserves the non-default config.
    let json = serde_json::to_string(&parsed).unwrap();
    let reparsed: AppConfig = serde_json::from_str(&json).unwrap();
    assert_eq!(reparsed.platforms, parsed.platforms);

    // The retired protocol-shaped key is a clean break and does not
    // silently enable Tencent QQ under the new defaults.
    let legacy: AppConfig = serde_json::from_str(
        r#"{"active_provider":"opencode","providers":[],"platforms":{"onebot":{"enabled":true}}}"#,
    )
    .unwrap();
    assert!(!legacy.platforms.qq.enabled);
    assert_eq!(legacy.platforms.command_prefix, "/");
    assert!(legacy.platforms.commands.is_empty());

    let missing_friend_request_setting: AppConfig = serde_json::from_str(
        r#"{
            "active_provider": "opencode",
            "providers": [],
            "platforms": {
                "qq": {
                    "private_chats": { "whitelist": [12345] }
                }
            }
        }"#,
    )
    .unwrap();
    assert!(
        missing_friend_request_setting
            .platforms
            .qq
            .private_chats
            .friend_requests_require_private_whitelist
    );
}

#[test]
fn qq_prompt_identity_options_default_on_and_roundtrip() {
    let defaults: OneBotConfig = serde_json::from_str("{}").unwrap();
    assert!(defaults.user_identification);
    assert!(defaults.show_group_name);
    assert!(defaults.memory.write_enabled);

    let mut disabled = OneBotConfig::default();
    disabled.user_identification = false;
    disabled.show_group_name = false;
    let json = serde_json::to_value(&disabled).unwrap();
    assert_eq!(json["user_identification"], false);
    assert_eq!(json["show_group_name"], false);
    assert_eq!(
        serde_json::from_value::<OneBotConfig>(json).unwrap(),
        disabled
    );
}

#[test]
fn platform_command_defaults_overrides_and_validation() {
    let mut config = AppConfig::default();
    assert_eq!(config.platforms.command_prefix, "/");
    assert_eq!(
        config
            .platforms
            .command_permission("reset", PlatformCommandPermission::AdminOnly),
        PlatformCommandPermission::AdminOnly
    );
    config.platforms.set_command_permission(
        "reset",
        PlatformCommandPermission::Everyone,
        PlatformCommandPermission::AdminOnly,
    );
    assert_eq!(
        config.platforms.commands["reset"].permission,
        PlatformCommandPermission::Everyone
    );
    config.platforms.set_command_permission(
        "reset",
        PlatformCommandPermission::AdminOnly,
        PlatformCommandPermission::AdminOnly,
    );
    assert!(config.platforms.commands.is_empty());

    for invalid in [
        "",
        " ",
        "/ reset",
        "\n",
        "/////////////////////////////////",
    ] {
        config.platforms.command_prefix = invalid.to_string();
        assert!(
            config.validate().is_err(),
            "prefix should be invalid: {invalid:?}"
        );
    }
    config.platforms.command_prefix = "/".to_string();
    config
        .platforms
        .commands
        .insert("Reset".to_string(), PlatformCommandConfig::default());
    assert!(config.validate().is_err());
}

#[test]
fn qq_platform_model_pools_validate_and_round_trip() {
    let mut config = route_test_config();
    let provider_id = config.providers[0].id.clone();
    config.platforms.qq.text_models =
        crate::config::ModelPoolRef::models(vec![ActiveProviderModelConfig {
            provider_id: provider_id.clone(),
            model: "text-only".to_string(),
        }]);
    config.platforms.qq.non_whitelist_text_models =
        crate::config::ModelPoolRef::models(vec![ActiveProviderModelConfig {
            provider_id: provider_id.clone(),
            model: "text-only".to_string(),
        }]);
    config.platforms.qq.multimodal_models =
        crate::config::ModelPoolRef::models(vec![ActiveProviderModelConfig {
            provider_id,
            model: "vision".to_string(),
        }]);

    assert!(config.validate().is_ok());
    let value = serde_json::to_value(&config).unwrap();
    let reparsed: AppConfig = serde_json::from_value(value).unwrap();
    assert_eq!(
        reparsed.platforms.qq.text_models,
        config.platforms.qq.text_models
    );
    assert_eq!(
        reparsed.platforms.qq.multimodal_models,
        config.platforms.qq.multimodal_models
    );
    assert_eq!(
        reparsed.platforms.qq.non_whitelist_text_models,
        config.platforms.qq.non_whitelist_text_models
    );

    config
        .platforms
        .qq
        .multimodal_models
        .explicit_models_mut()
        .unwrap()[0]
        .model = "text-only".to_string();
    assert!(config.validate().is_err());
    config
        .platforms
        .qq
        .multimodal_models
        .explicit_models_mut()
        .unwrap()[0]
        .model = "vision".to_string();
    config
        .platforms
        .qq
        .non_whitelist_text_models
        .explicit_models_mut()
        .unwrap()[0]
        .model = "missing".to_string();
    assert!(config.validate().is_err());
}

#[test]
fn qq_non_whitelist_model_pool_normalizes_for_dynamic_inheritance() {
    let mut config = route_test_config();
    let provider_id = config.providers[0].id.clone();
    config.platforms.qq.non_whitelist_text_models = crate::config::ModelPoolRef::models(vec![
        ActiveProviderModelConfig {
            provider_id: format!(" {provider_id} "),
            model: " text-only ".to_string(),
        },
        ActiveProviderModelConfig {
            provider_id: provider_id.clone(),
            model: "text-only".to_string(),
        },
    ]);

    config.normalize_platform_model_routes();
    assert_eq!(
        config
            .platforms
            .qq
            .non_whitelist_text_models
            .explicit_models()
            .unwrap()
            .len(),
        1
    );

    config.platforms.qq.non_whitelist_text_models = crate::config::ModelPoolRef::models(Vec::new());
    config.normalize_platform_model_routes();
    assert!(config.platforms.qq.non_whitelist_text_models.is_inherit());
}

#[test]
fn session_limits_resolve_from_conversation_then_kind_then_qq() {
    let mut qq = OneBotConfig::default();
    assert_eq!(qq.session_limits.running, 8);
    assert_eq!(qq.session_limits.queued, 16);
    // 会话内并行默认关闭:无论配了几个并行位,解析出来都是串行。
    assert!(!qq.session_parallel);
    assert_eq!(
        qq.session_limits(PlatformConversationKind::Group, "42"),
        PlatformSessionLimits {
            running: 1,
            queued: 16
        }
    );

    qq.session_parallel = true;
    qq.session_limits = PlatformSessionLimits {
        running: 2,
        queued: 3,
    };
    qq.group_chats.session_limits = Some(PlatformSessionLimits {
        running: 3,
        queued: 5,
    });
    qq.conversations.push(PlatformModelRoute {
        conversation: PlatformConversationConfig {
            kind: PlatformConversationKind::Group,
            id: "42".to_string(),
        },
        persona: PlatformPersonaOverride::Inherit,
        text_models_inheritance: PlatformModelPoolInheritance::Platform,
        text_models: None,
        multimodal_models_inheritance: PlatformModelPoolInheritance::Platform,
        multimodal_models: None,
        extra_prompt: String::new(),
        session_limits: Some(PlatformSessionLimits {
            running: 4,
            queued: 7,
        }),
    });
    assert_eq!(
        qq.session_limits(PlatformConversationKind::Group, "42"),
        PlatformSessionLimits {
            running: 4,
            queued: 7
        }
    );
    assert_eq!(
        qq.session_limits(PlatformConversationKind::Group, "43"),
        PlatformSessionLimits {
            running: 3,
            queued: 5
        }
    );
    assert_eq!(
        qq.session_limits(PlatformConversationKind::Private, "42"),
        PlatformSessionLimits {
            running: 2,
            queued: 3
        }
    );
    // 串行模式压掉的只是并行数,覆盖项的队列深度照旧生效。
    qq.session_parallel = false;
    assert_eq!(
        qq.session_limits(PlatformConversationKind::Group, "42"),
        PlatformSessionLimits {
            running: 1,
            queued: 7
        }
    );
}

#[test]
fn qq_text_model_pool_resolution_preserves_conversation_priority() {
    let mut config = route_test_config();
    let provider_id = config.providers[0].id.clone();
    let pool = |model: &str| {
        vec![ActiveProviderModelConfig {
            provider_id: provider_id.clone(),
            model: model.to_string(),
        }]
    };
    config.active_provider_models = Some(pool("global"));
    config.active_multimodal_provider_models = Some(pool("global-media"));
    config.platforms.qq.text_models = crate::config::ModelPoolRef::models(pool("platform"));
    config.platforms.qq.multimodal_models =
        crate::config::ModelPoolRef::models(pool("platform-media"));
    config.platforms.qq.non_whitelist_text_models =
        crate::config::ModelPoolRef::models(pool("non-whitelist"));
    config.platforms.qq.conversations.push(PlatformModelRoute {
        conversation: PlatformConversationConfig {
            kind: PlatformConversationKind::Group,
            id: "20002".to_string(),
        },
        persona: PlatformPersonaOverride::Inherit,
        text_models_inheritance: PlatformModelPoolInheritance::Platform,
        text_models: Some(pool("conversation")),
        multimodal_models_inheritance: PlatformModelPoolInheritance::Platform,
        multimodal_models: None,
        extra_prompt: String::new(),
        session_limits: None,
    });

    {
        let resolved = |conversation_id, use_non_whitelist_pool| {
            config
                .qq_text_model_pool(
                    PlatformConversationKind::Group,
                    conversation_id,
                    use_non_whitelist_pool,
                )
                .unwrap()[0]
                .model
                .clone()
        };
        assert_eq!(resolved("20002", true), "conversation");
        assert_eq!(resolved("30003", true), "non-whitelist");
        assert_eq!(resolved("30003", false), "platform");
    }
    assert_eq!(
        config
            .qq_multimodal_model_pool(PlatformConversationKind::Group, "20002")
            .unwrap()[0]
            .model,
        "platform-media"
    );
    let route = &mut config.platforms.qq.conversations[0];
    route.text_models = None;
    route.text_models_inheritance = PlatformModelPoolInheritance::Global;
    assert_eq!(
        config
            .qq_text_model_pool(PlatformConversationKind::Group, "20002", true)
            .unwrap()[0]
            .model,
        "global"
    );
    assert_eq!(
        config
            .qq_multimodal_model_pool(PlatformConversationKind::Group, "20002")
            .unwrap()[0]
            .model,
        "platform-media"
    );
    config.platforms.qq.conversations[0].multimodal_models_inheritance =
        PlatformModelPoolInheritance::Global;
    assert_eq!(
        config
            .qq_multimodal_model_pool(PlatformConversationKind::Group, "20002")
            .unwrap()[0]
            .model,
        "global-media"
    );
    config.platforms.qq.non_whitelist_text_models = crate::config::ModelPoolRef::inherit();
    assert_eq!(
        config
            .qq_text_model_pool(PlatformConversationKind::Group, "30003", true)
            .unwrap()[0]
            .model,
        "platform"
    );
    config.platforms.qq.text_models = crate::config::ModelPoolRef::inherit();
    assert_eq!(
        config
            .qq_text_model_pool(PlatformConversationKind::Group, "30003", true)
            .unwrap()[0]
            .model,
        "global"
    );
}

#[test]
fn qq_model_pool_inheritance_is_backward_compatible_and_round_trips() {
    let mut route: PlatformModelRoute = serde_json::from_value(serde_json::json!({
        "conversation": { "kind": "private", "id": "42" }
    }))
    .unwrap();
    assert_eq!(
        route.text_models_inheritance,
        PlatformModelPoolInheritance::Platform
    );
    assert_eq!(
        route.multimodal_models_inheritance,
        PlatformModelPoolInheritance::Platform
    );
    let legacy_value = serde_json::to_value(&route).unwrap();
    assert!(legacy_value.get("text_models_inheritance").is_none());
    assert!(legacy_value.get("multimodal_models_inheritance").is_none());

    route.text_models_inheritance = PlatformModelPoolInheritance::Global;
    route.multimodal_models_inheritance = PlatformModelPoolInheritance::Global;
    let value = serde_json::to_value(&route).unwrap();
    assert_eq!(value["text_models_inheritance"], "global");
    assert_eq!(value["multimodal_models_inheritance"], "global");
    assert_eq!(
        serde_json::from_value::<PlatformModelRoute>(value).unwrap(),
        route
    );
}

#[test]
fn qq_conversation_persona_override_is_explicit_and_tracks_renames() {
    let mut config = route_test_config();
    config.prompt.active_persona = "Global.md".to_string();
    let mut route = test_route(&config);
    route.persona = PlatformPersonaOverride::Custom {
        name: "Group.md".to_string(),
    };
    config.platforms.qq.conversations.push(route);

    let mut effective = config.clone();
    effective.apply_qq_conversation_persona(PlatformConversationKind::Group, "20002");
    assert_eq!(effective.prompt.active_persona, "Group.md");
    assert_eq!(config.platforms.persona_reference_count("Group.md"), 1);

    config
        .platforms
        .rename_persona_references("Group.md", "Renamed.md");
    assert_eq!(
        config.platforms.qq.conversations[0].persona.custom_name(),
        Some("Renamed.md")
    );
    assert!(config.validate().is_ok());

    config.platforms.qq.conversations[0].persona = PlatformPersonaOverride::Miyu;
    config.apply_qq_conversation_persona(PlatformConversationKind::Group, "20002");
    assert!(config.prompt.active_persona.is_empty());
}

#[test]
fn qq_conversation_persona_rejects_unsafe_custom_names() {
    let mut config = route_test_config();
    let mut route = test_route(&config);
    route.persona = PlatformPersonaOverride::Custom {
        name: "../persona.md".to_string(),
    };
    config.platforms.qq.conversations.push(route);
    assert!(config.validate().is_err());
}

#[test]
fn platform_model_routes_roundtrip_lookup_and_plugin_shape() {
    let mut config = route_test_config();
    let route = test_route(&config);
    config.platforms.upsert_model_route(route.clone());
    config.platforms.qq.plugins.insert(
        "reply_processor".to_string(),
        PlatformPluginInstanceConfig {
            enabled: Some(false),
            settings: serde_json::json!({"threshold": 150})
                .as_object()
                .unwrap()
                .clone(),
        },
    );

    let found = config
        .platform_model_route(PlatformConversationKind::Group, "20002")
        .unwrap();
    assert_eq!(found, &route);
    assert!(config.validate().is_ok());

    let json = serde_json::to_string(&config).unwrap();
    let reparsed: AppConfig = serde_json::from_str(&json).unwrap();
    assert_eq!(reparsed.platforms, config.platforms);
    assert_eq!(
        reparsed.platforms.qq.plugins["reply_processor"].enabled,
        Some(false)
    );
    assert_eq!(
        reparsed.platforms.qq.plugins["reply_processor"].settings["threshold"],
        150
    );
}

#[test]
fn built_in_platform_plugin_settings_are_validated() {
    let mut config = AppConfig::default();
    config.platforms.qq.plugins.insert(
        "reply_processor".to_string(),
        PlatformPluginInstanceConfig {
            enabled: None,
            settings: serde_json::json!({"threshold": 0, "mode": "invalid"})
                .as_object()
                .unwrap()
                .clone(),
        },
    );
    assert!(config.validate().is_err());

    config
        .platforms
        .qq
        .plugins
        .get_mut("reply_processor")
        .unwrap()
        .settings = serde_json::json!({
        "threshold": 150,
        "mode": "image",
        "future_option": 1
    })
    .as_object()
    .unwrap()
    .clone();
    assert!(config.validate().is_ok());

    config.platforms.qq.plugins.insert(
        QQ_MEME_COLLECTOR_PLUGIN_ID.to_string(),
        PlatformPluginInstanceConfig {
            enabled: Some(true),
            settings: serde_json::json!({
                "collect_probability": 0.02,
                "max_images_per_message": 2
            })
            .as_object()
            .unwrap()
            .clone(),
        },
    );
    assert!(config.validate().is_ok());
    config
        .platforms
        .qq
        .plugins
        .get_mut(QQ_MEME_COLLECTOR_PLUGIN_ID)
        .unwrap()
        .settings
        .insert("collect_probability".to_string(), serde_json::json!(1.01));
    assert!(config.validate().is_err());
}

#[test]
fn qq_meme_collector_defaults_are_conservative() {
    let settings = QqMemeCollectorPluginSettings::default();
    assert_eq!(settings.collect_probability, 0.02);
    assert_eq!(settings.max_images_per_message, 2);
    assert!(!settings.allow_non_admin_save_tool);
}

#[test]
fn qq_message_history_defaults_to_full_text_recording() {
    let settings = QqMessageHistoryPluginSettings::default();

    assert_eq!(settings.history_search_max_results, 0);
    assert_eq!(settings.history_safe_page_limit, 500);
    assert!(settings.allow_cross_conversation_search);
    assert!(settings.validate().is_ok());
}

#[test]
fn qq_group_join_approval_defaults_are_safe() {
    let settings = QqGroupJoinApprovalPluginSettings::default();

    assert_eq!(settings.timeout_seconds, 60);
    assert_eq!(settings.max_retries, 1);
    // Ships on the lite tier: approvals leave the flagship pool as soon as a
    // lite pool exists, and resolve to the global pool until then.
    assert_eq!(settings.text_models.tier_ref(), Some(ModelTier::Lite));
    assert!(settings.groups.is_empty());
    assert!(settings.validate().is_ok());
}

#[test]
fn qq_group_join_approval_settings_are_validated() {
    let mut config = route_test_config();
    config.platforms.qq.plugins.insert(
        QQ_GROUP_JOIN_APPROVAL_PLUGIN_ID.to_string(),
        PlatformPluginInstanceConfig {
            enabled: Some(true),
            settings: serde_json::json!({
                "timeout_seconds": 60,
                "max_retries": 1,
                "groups": [
                    {"group_id": 130515298, "approve_condition": "Arch 相关通过"}
                ]
            })
            .as_object()
            .unwrap()
            .clone(),
        },
    );
    assert!(config.validate().is_ok());

    config
        .platforms
        .qq
        .plugins
        .get_mut(QQ_GROUP_JOIN_APPROVAL_PLUGIN_ID)
        .unwrap()
        .settings["groups"]
        .as_array_mut()
        .unwrap()
        .push(serde_json::json!({
            "group_id": 130515298,
            "approve_condition": "duplicate"
        }));
    assert!(config.validate().is_err());

    let instance = config
        .platforms
        .qq
        .plugins
        .get_mut(QQ_GROUP_JOIN_APPROVAL_PLUGIN_ID)
        .unwrap();
    instance.settings = serde_json::json!({
        "timeout_seconds": 60,
        "max_retries": 1,
        "groups": [{"group_id": 0, "approve_condition": "invalid group"}]
    })
    .as_object()
    .unwrap()
    .clone();
    assert!(config.validate().is_err());

    let instance = config
        .platforms
        .qq
        .plugins
        .get_mut(QQ_GROUP_JOIN_APPROVAL_PLUGIN_ID)
        .unwrap();
    instance.settings = serde_json::json!({
        "timeout_seconds": 0,
        "groups": []
    })
    .as_object()
    .unwrap()
    .clone();
    assert!(config.validate().is_err());

    let instance = config
        .platforms
        .qq
        .plugins
        .get_mut(QQ_GROUP_JOIN_APPROVAL_PLUGIN_ID)
        .unwrap();
    instance.settings = serde_json::json!({
        "max_retries": 4,
        "groups": []
    })
    .as_object()
    .unwrap()
    .clone();
    assert!(config.validate().is_err());
}

#[test]
fn qq_group_join_approval_rejects_invalid_conditions_and_unknown_fields_pass() {
    let mut config = route_test_config();
    let long = "x".repeat(200_001);
    for condition in [
        String::new(),
        "  padded  ".to_string(),
        format!("bad\0condition"),
        long,
    ] {
        config.platforms.qq.plugins.insert(
            QQ_GROUP_JOIN_APPROVAL_PLUGIN_ID.to_string(),
            PlatformPluginInstanceConfig {
                enabled: None,
                settings: serde_json::json!({
                    "groups": [{"group_id": 1, "approve_condition": condition}]
                })
                .as_object()
                .unwrap()
                .clone(),
            },
        );
        assert!(
            config.validate().is_err(),
            "condition should fail: {condition:?}"
        );
    }

    config.platforms.qq.plugins.insert(
        QQ_GROUP_JOIN_APPROVAL_PLUGIN_ID.to_string(),
        PlatformPluginInstanceConfig {
            enabled: None,
            settings: serde_json::json!({
                "future_option": 1,
                "groups": [{"group_id": 1, "approve_condition": "valid"}]
            })
            .as_object()
            .unwrap()
            .clone(),
        },
    );
    assert!(config.validate().is_ok());
}

#[test]
fn qq_group_join_approval_normalizes_groups_and_merges_defaults() {
    let mut config = route_test_config();
    config.platforms.qq.plugins.insert(
        QQ_GROUP_JOIN_APPROVAL_PLUGIN_ID.to_string(),
        PlatformPluginInstanceConfig {
            enabled: Some(true),
            settings: serde_json::json!({
                "timeout_seconds": 60,
                "max_retries": 1,
                "groups": [
                    {"group_id": 2, "approve_condition": "  second  "},
                    {"group_id": 1, "approve_condition": " first "},
                    {"group_id": 2, "approve_condition": " replaced "}
                ]
            })
            .as_object()
            .unwrap()
            .clone(),
        },
    );

    config.normalize_platform_model_routes();

    let instance = &config.platforms.qq.plugins[QQ_GROUP_JOIN_APPROVAL_PLUGIN_ID];
    assert_eq!(instance.enabled, Some(true));
    assert!(instance.settings.get("timeout_seconds").is_none());
    assert!(instance.settings.get("max_retries").is_none());
    let settings = QqGroupJoinApprovalPluginSettings::from_instance(instance).unwrap();
    assert_eq!(settings.groups.len(), 2);
    assert_eq!(settings.groups[0].group_id, 1);
    assert_eq!(settings.groups[0].approve_condition, "first");
    assert_eq!(settings.groups[1].approve_condition, "replaced");
    assert!(settings.validate().is_ok());
}

#[test]
fn qq_default_non_whitelist_rate_limits_match_the_deployed_contract() {
    let qq = OneBotConfig::default();

    // 08-26 起默认 300 秒 5 条(私聊/群聊同口径)。
    assert_eq!(
        qq.private_chats.non_whitelist_rate_limit,
        PlatformRateLimit {
            max_messages: 5,
            window_seconds: 300,
        }
    );
    assert_eq!(
        qq.group_chats.non_whitelist_rate_limit,
        PlatformRateLimit {
            max_messages: 5,
            window_seconds: 300,
        }
    );

    let explicit: OneBotConfig = serde_json::from_value(serde_json::json!({
        "private_chats": {
            "non_whitelist_rate_limit": {
                "max_messages": 1,
                "window_seconds": 120
            }
        },
        "group_chats": {
            "non_whitelist_rate_limit": {
                "max_messages": 5,
                "window_seconds": 60
            }
        }
    }))
    .unwrap();
    assert_eq!(
        explicit.private_chats.non_whitelist_rate_limit.max_messages,
        1
    );
    assert_eq!(
        explicit
            .private_chats
            .non_whitelist_rate_limit
            .window_seconds,
        120
    );
    assert_eq!(
        explicit.group_chats.non_whitelist_rate_limit.max_messages,
        5
    );
    assert_eq!(
        explicit.group_chats.non_whitelist_rate_limit.window_seconds,
        60
    );
}

#[test]
fn platform_model_route_normalization_uses_none_for_inheritance() {
    let mut config = route_test_config();
    let provider_id = config.providers[0].id.clone();
    let mut route = test_route(&config);
    route.conversation.id = " 20002 ".to_string();
    route.extra_prompt = "  group prompt  ".to_string();
    route.text_models = Some(vec![
        ActiveProviderModelConfig {
            provider_id: format!(" {provider_id} "),
            model: " text-only ".to_string(),
        },
        ActiveProviderModelConfig {
            provider_id: provider_id.clone(),
            model: "text-only".to_string(),
        },
    ]);
    route.text_models_inheritance = PlatformModelPoolInheritance::Global;
    route.multimodal_models = Some(Vec::new());
    route.multimodal_models_inheritance = PlatformModelPoolInheritance::Global;
    config.platforms.qq.conversations.push(route);
    config.normalize_platform_model_routes();

    let normalized = &config.platforms.qq.conversations[0];
    assert_eq!(normalized.conversation.id, "20002");
    assert_eq!(normalized.extra_prompt, "group prompt");
    assert_eq!(normalized.text_models.as_ref().unwrap().len(), 1);
    assert_eq!(
        normalized.text_models_inheritance,
        PlatformModelPoolInheritance::Platform
    );
    assert!(normalized.multimodal_models.is_none());
    assert_eq!(
        normalized.multimodal_models_inheritance,
        PlatformModelPoolInheritance::Global
    );

    config.platforms.qq.conversations[0].text_models = Some(Vec::new());
    config.normalize_platform_model_routes();
    assert_eq!(config.platforms.qq.conversations.len(), 1);
    assert!(config.platforms.qq.conversations[0].text_models.is_none());
}

#[test]
fn platform_model_route_validation_rejects_bad_identity_models_and_duplicates() {
    let mut config = route_test_config();
    let mut route = test_route(&config);
    route.conversation.id = "0".to_string();
    assert!(config.validate_platform_model_route(&route).is_err());
    route.conversation.id = "not-a-qq".to_string();
    assert!(config.validate_platform_model_route(&route).is_err());

    route.conversation.id = "20002".to_string();
    route.multimodal_models.as_mut().unwrap()[0].model = "text-only".to_string();
    assert!(config.validate_platform_model_route(&route).is_err());

    route.multimodal_models = None;
    route.text_models.as_mut().unwrap()[0].model = "missing".to_string();
    assert!(config.validate_platform_model_route(&route).is_err());

    let route = test_route(&config);
    config.platforms.qq.conversations = vec![route.clone(), route];
    assert!(config.validate().is_err());
}

#[test]
fn platform_model_references_are_renamed_and_pruned() {
    let mut config = route_test_config();
    let old_provider = config.providers[0].id.clone();
    config.platforms.qq.non_whitelist_text_models =
        crate::config::ModelPoolRef::models(vec![ActiveProviderModelConfig {
            provider_id: old_provider.clone(),
            model: "text-only".to_string(),
        }]);
    config.platforms.qq.conversations.push(test_route(&config));

    config.rename_platform_provider_references(&old_provider, "renamed");
    assert_eq!(
        config
            .platforms
            .qq
            .non_whitelist_text_models
            .explicit_models()
            .unwrap()[0]
            .provider_id,
        "renamed"
    );
    let route = &config.platforms.qq.conversations[0];
    assert_eq!(
        route.text_models.as_ref().unwrap()[0].provider_id,
        "renamed"
    );
    assert_eq!(
        route.multimodal_models.as_ref().unwrap()[0].provider_id,
        "renamed"
    );

    config.rename_platform_provider_references("renamed", &old_provider);
    config.remove_active_model_references(&old_provider, "vision");
    assert!(config.platforms.qq.conversations[0]
        .multimodal_models
        .is_none());
    config.remove_active_model_references(&old_provider, "text-only");
    assert_eq!(config.platforms.qq.conversations.len(), 1);
    assert!(config.platforms.qq.conversations[0].text_models.is_none());
    assert!(config.platforms.qq.non_whitelist_text_models.is_inherit());
}

fn tiered_qq_config() -> (AppConfig, String) {
    let mut config = AppConfig::default();
    let provider_id = config.active_provider.clone();
    let provider = config
        .providers
        .iter_mut()
        .find(|provider| provider.id == provider_id)
        .unwrap();
    for model in ["global-a", "lite-a", "cheap-a", "pinned"] {
        provider.models.push(model.to_string());
    }
    config.active_provider_models = Some(vec![ActiveProviderModelConfig {
        provider_id: provider_id.clone(),
        model: "global-a".to_string(),
    }]);
    config
        .toggle_tier_model(ModelTier::Lite, &provider_id, "lite-a")
        .unwrap();
    (config, provider_id)
}

fn models_of(pool: Option<Vec<ActiveProviderModelConfig>>) -> Vec<String> {
    pool.unwrap_or_default()
        .into_iter()
        .map(|entry| entry.model)
        .collect()
}

#[test]
fn pool_refs_parse_strings_arrays_and_null_and_omit_inherit() {
    let parsed: AppConfig = serde_json::from_str(
        r#"{
            "active_provider": "opencode",
            "providers": [],
            "platforms": { "qq": {
                "text_models": "cheap",
                "multimodal_models": null,
                "non_whitelist_text_models": [ { "provider_id": "p", "model": "m" } ]
            } }
        }"#,
    )
    .unwrap();
    let qq = &parsed.platforms.qq;
    assert_eq!(qq.text_models.tier_ref(), Some(ModelTier::Cheap));
    assert!(qq.multimodal_models.is_inherit());
    assert_eq!(
        qq.non_whitelist_text_models.explicit_models().unwrap()[0].model,
        "m"
    );
    let json = serde_json::to_string(&parsed).unwrap();
    assert!(json.contains("\"text_models\":\"cheap\""), "{json}");
    assert!(!json.contains("multimodal_models"), "{json}");

    // Old alias in a reference canonicalizes on normalize.
    let mut old = ModelPoolRef::Named("balanced".to_string());
    old.normalize();
    assert_eq!(old, ModelPoolRef::tier(ModelTier::Standard));
    // An emptied explicit list is `inherit`.
    assert!(ModelPoolRef::models(Vec::new()).is_inherit());
}

#[test]
fn pool_ref_validation_rejects_unknown_names_and_tiers_on_multimodal_slots() {
    let (mut config, _) = tiered_qq_config();
    config.platforms.qq.text_models = ModelPoolRef::Named("chep".to_string());
    let error = config.validate().unwrap_err().to_string();
    assert!(error.contains("unknown pool 'chep'"), "{error}");

    config.platforms.qq.text_models = ModelPoolRef::inherit();
    config.platforms.qq.multimodal_models = ModelPoolRef::tier(ModelTier::Lite);
    let error = config.validate().unwrap_err().to_string();
    assert!(error.contains("cannot reference a tier"), "{error}");

    config.platforms.qq.multimodal_models = ModelPoolRef::global();
    assert!(config.validate().is_ok());
}

#[test]
fn pool_refs_resolve_through_inherit_global_tier_and_explicit() {
    let (mut config, provider_id) = tiered_qq_config();
    let global = || vec!["global-a".to_string()];

    // inherit → global; a configured tier → its members.
    assert_eq!(models_of(config.qq_default_text_pool()), global());
    config.platforms.qq.text_models = ModelPoolRef::tier(ModelTier::Lite);
    assert_eq!(models_of(config.qq_default_text_pool()), vec!["lite-a"]);
    // An unconfigured tier falls back to the global pool, not a neighbour.
    config.platforms.qq.text_models = ModelPoolRef::tier(ModelTier::Cheap);
    assert_eq!(models_of(config.qq_default_text_pool()), global());
    // Explicit list is used as-is.
    config.platforms.qq.text_models = ModelPoolRef::models(vec![ActiveProviderModelConfig {
        provider_id: provider_id.clone(),
        model: "pinned".to_string(),
    }]);
    assert_eq!(models_of(config.qq_default_text_pool()), vec!["pinned"]);

    // Non-whitelist: inherit → default text; global → global.
    assert_eq!(
        models_of(config.qq_non_whitelist_text_pool()),
        vec!["pinned"]
    );
    config.platforms.qq.non_whitelist_text_models = ModelPoolRef::global();
    assert_eq!(models_of(config.qq_non_whitelist_text_pool()), global());
    assert_eq!(
        models_of(config.qq_text_model_pool(PlatformConversationKind::Group, "1", true)),
        global()
    );
    assert_eq!(
        models_of(config.qq_text_model_pool(PlatformConversationKind::Group, "1", false)),
        vec!["pinned"]
    );

    // Plugin slots: judge inherits from its parent closure; affection inherits from judge.
    let judge = ModelPoolRef::tier(ModelTier::Lite);
    let affection = ModelPoolRef::inherit();
    let resolved_judge = config.resolve_pool_ref(&judge, false, || {
        config.qq_text_model_pool(PlatformConversationKind::Group, "1", false)
    });
    assert_eq!(models_of(resolved_judge.clone()), vec!["lite-a"]);
    let resolved_affection = config.resolve_pool_ref(&affection, false, || resolved_judge);
    assert_eq!(models_of(resolved_affection), vec!["lite-a"]);
}

#[test]
fn deleting_a_model_clears_it_from_every_explicit_slot_but_not_from_references() {
    let (mut config, provider_id) = tiered_qq_config();
    let pinned = || {
        ModelPoolRef::models(vec![ActiveProviderModelConfig {
            provider_id: provider_id.clone(),
            model: "pinned".to_string(),
        }])
    };
    config.platforms.qq.text_models = ModelPoolRef::tier(ModelTier::Lite);
    config.platforms.qq.non_whitelist_text_models = pinned();
    let mut real_context = PlatformPluginInstanceConfig::default();
    merge_real_context_settings(
        &mut real_context,
        &RealContextPluginSettings {
            text_models: pinned(),
            affection_text_models: pinned(),
            ..RealContextPluginSettings::default()
        },
    );
    config
        .platforms
        .qq
        .plugins
        .insert(REAL_CONTEXT_PLUGIN_ID.to_string(), real_context);
    assert!(config.validate().is_ok());

    config
        .remove_active_provider_model(&provider_id, "pinned")
        .unwrap();

    assert!(config.platforms.qq.non_whitelist_text_models.is_inherit());
    let settings = RealContextPluginSettings::from_instance(
        &config.platforms.qq.plugins[REAL_CONTEXT_PLUGIN_ID],
    )
    .unwrap();
    assert!(settings.text_models.is_inherit());
    assert!(settings.affection_text_models.is_inherit());
    // The tier reference is untouched by deletion.
    assert_eq!(
        config.platforms.qq.text_models.tier_ref(),
        Some(ModelTier::Lite)
    );
    assert!(config.validate().is_ok());
}
