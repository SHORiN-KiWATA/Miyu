mod api;
mod lookup;
mod provider_api;
pub(crate) use api::*;
pub(crate) use lookup::*;
pub(crate) use provider_api::*;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

#[derive(Debug, Clone, PartialEq)]
pub struct ModelInfo {
    pub input_modalities: Vec<String>,
    pub context_window: Option<u64>,
    reasoning: Option<ModelReasoningInfo>,
    pub cost: Option<ApiCost>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelReasoningInfo {
    pub provider_npm: Option<String>,
    pub variants: Vec<ReasoningVariant>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReasoningVariant {
    pub id: String,
    pub setting: ReasoningSetting,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReasoningSetting {
    Effort(String),
    Toggle(bool),
    BudgetTokens(u64),
    Disabled,
}

struct Cache {
    data: HashMap<String, HashMap<String, ModelInfo>>,
    /// models.dev 供应商键 → 其 API base URL(尾斜杠归一),配合配置里的
    /// base_url 做供应商对齐。
    provider_api: HashMap<String, String>,
}

static CACHE: OnceLock<Mutex<Option<Cache>>> = OnceLock::new();
static REFRESH_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
static ACTIVE_METADATA_STARTED: AtomicBool = AtomicBool::new(false);

fn cache_lock() -> &'static Mutex<Option<Cache>> {
    CACHE.get_or_init(|| Mutex::new(None))
}

fn refresh_lock() -> &'static Mutex<()> {
    REFRESH_LOCK.get_or_init(|| Mutex::new(()))
}

fn provider_api_cache_lock() -> &'static Mutex<HashMap<(String, String), u64>> {
    PROVIDER_API_CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

pub fn is_loaded() -> bool {
    cache_lock().lock().unwrap().is_some()
}

fn cache_file(paths: &crate::paths::MiyuPaths) -> PathBuf {
    paths.cache_dir.join("models_cache.json")
}

fn load_from_disk(path: &PathBuf) -> Result<Cache> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read models cache: {}", path.display()))?;
    parse_api_response(&text)
}

fn fetch_and_cache(path: &PathBuf) -> Result<Cache> {
    let client = reqwest::blocking::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(30))
        .build()?;
    let text = client
        .get(API_URL)
        .header("User-Agent", "Mozilla/5.0 Miyu/0.1")
        .send()?
        .error_for_status()?
        .text()?;
    if text.trim().is_empty() {
        anyhow::bail!("models.dev returned empty response");
    }
    let cache = parse_api_response(&text)?;
    let parent = path.parent().context("models cache path has no parent")?;
    std::fs::create_dir_all(parent)?;
    let mut temp = tempfile::NamedTempFile::new_in(parent)?;
    use std::io::Write;
    temp.write_all(text.as_bytes())?;
    temp.persist(path)
        .map_err(|error| error.error)
        .context("failed to replace models cache")?;
    Ok(cache)
}

pub fn try_load(paths: &crate::paths::MiyuPaths) {
    let path = cache_file(paths);
    let cache = load_from_disk(&path).ok();
    if let Some(cache) = cache {
        let mut lock = cache_lock().lock().unwrap();
        *lock = Some(cache);
    }
}

pub fn try_load_active(paths: &crate::paths::MiyuPaths, config: &crate::config::AppConfig) {
    let path = cache_file(paths);
    let cache = load_from_disk(&path).ok();
    if let Some(mut cache) = cache {
        retain_configured_models(&mut cache.data, config);
        let mut lock = cache_lock().lock().unwrap();
        *lock = Some(cache);
        drop(lock);
        // 3.6MB 的目录 JSON 全量解析出的临时树刚被释放，立刻还给 OS，
        // 别让它抬着进程高水位（REPL 冷启动路径也走这里）。
        crate::runtime::trim_process_memory();
    }
}

pub fn spawn_background_refresh(paths: crate::paths::MiyuPaths) {
    let path = cache_file(&paths);
    std::thread::spawn(move || {
        let _refresh = refresh_lock().lock().unwrap();
        let fetched = fetch_and_cache(&path).ok();
        if let Some(cache) = fetched {
            let mut lock = cache_lock().lock().unwrap();
            *lock = Some(cache);
        }
    });
}

pub fn spawn_background_refresh_active(
    paths: crate::paths::MiyuPaths,
    config: crate::config::AppConfig,
) {
    spawn_provider_api_refresh(config.providers.clone());
    let path = cache_file(&paths);
    std::thread::spawn(move || {
        let _refresh = refresh_lock().lock().unwrap();
        let fetched = fetch_and_cache(&path).ok();
        if let Some(mut cache) = fetched {
            retain_configured_models(&mut cache.data, &config);
            let mut lock = cache_lock().lock().unwrap();
            *lock = Some(cache);
            drop(lock);
            crate::runtime::trim_process_memory();
        }
    });
}

pub fn ensure_active_metadata(paths: &crate::paths::MiyuPaths, config: &crate::config::AppConfig) {
    if !is_loaded() {
        try_load_active(paths, config);
    }
    if ACTIVE_METADATA_STARTED
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_ok()
    {
        spawn_background_refresh_active(paths.clone(), config.clone());
    }
}

fn retain_configured_models(
    data: &mut HashMap<String, HashMap<String, ModelInfo>>,
    config: &crate::config::AppConfig,
) {
    let mut selected = HashMap::<String, HashSet<String>>::new();
    let mut selected_model_ids = HashSet::new();
    for provider in &config.providers {
        selected
            .entry(provider.id.clone())
            .or_default()
            .insert(provider.default_model.clone());
        if !provider.default_model.trim().is_empty() {
            selected_model_ids.insert(provider.default_model.clone());
        }
    }
    let conversation_models = config.platforms.qq.conversations.iter().flat_map(|route| {
        route
            .text_models
            .iter()
            .flatten()
            .chain(route.multimodal_models.iter().flatten())
    });
    let real_context_models = config
        .platforms
        .qq
        .plugins
        .get(crate::config::REAL_CONTEXT_PLUGIN_ID)
        .and_then(|instance| crate::config::RealContextPluginSettings::from_instance(instance).ok())
        .and_then(|settings| settings.text_models)
        .unwrap_or_default();
    for choice in config
        .active_provider_models
        .iter()
        .flatten()
        .chain(config.active_multimodal_provider_models.iter().flatten())
        .chain(config.platforms.qq.text_models.iter().flatten())
        .chain(config.platforms.qq.multimodal_models.iter().flatten())
        .chain(
            config
                .platforms
                .qq
                .non_whitelist_text_models
                .iter()
                .flatten(),
        )
        .chain(conversation_models)
        .chain(real_context_models.iter())
    {
        selected
            .entry(choice.provider_id.clone())
            .or_default()
            .insert(choice.model.clone());
        selected_model_ids.insert(choice.model.clone());
    }
    data.retain(|provider_id, models| {
        let provider_models = selected.get(provider_id);
        models.retain(|model_id, _| {
            provider_models.is_some_and(|ids| ids.contains(model_id))
                || selected_model_ids.contains(model_id)
        });
        !models.is_empty()
    });
}

pub fn refresh_blocking(paths: &crate::paths::MiyuPaths) -> Result<()> {
    let _refresh = refresh_lock().lock().unwrap();
    if is_loaded() {
        return Ok(());
    }
    let path = cache_file(paths);
    let cache = fetch_and_cache(&path)?;
    let mut lock = cache_lock().lock().unwrap();
    *lock = Some(cache);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model(window: u64) -> ModelInfo {
        ModelInfo {
            input_modalities: Vec::new(),
            context_window: Some(window),
            reasoning: None,
            cost: None,
        }
    }

    /// 手动价格优先于目录价:目录未收录的中转端点靠 model_costs。
    #[test]
    fn manual_model_cost_overrides_catalogue() {
        let mut config = crate::config::AppConfig::default();
        config.providers.push(crate::config::ProviderConfig {
            enabled: true,
            id: "relay".to_string(),
            display_name: "Relay".to_string(),
            base_url: "https://relay.example/v1".to_string(),
            protocol: "openai-chat".to_string(),
            api_key: None,
            models: vec!["m".to_string()],
            model_context_window: HashMap::new(),
            model_temperature: HashMap::new(),
            model_tools_loading_mode: HashMap::new(),
            model_modalities: HashMap::new(),
            tool_result_media: None,
            model_costs: HashMap::from([(
                "m".to_string(),
                crate::config::ModelCostConfig {
                    currency: crate::config::CostCurrency::Usd,
                    input: 1.5,
                    output: 3.0,
                    cache_read: Some(0.15),
                },
            )]),
            default_model: "m".to_string(),
            timeout_seconds: 60,
            temperature: 1.0,
            anthropic_max_tokens: 4096,
            extra_body: None,
        });
        {
            let price = pricing_resolver(&config);
            let cost = price("relay", "m").expect("manual price should resolve");
            assert_eq!(cost.input, 1.5);
            assert_eq!(cost.output, 3.0);
            assert_eq!(cost.cache_read, Some(0.15));
            assert_eq!(cost.cache_write, None);
        }
        // CNY 手动价按估算汇率折 USD
        config.providers.last_mut().unwrap().model_costs.insert(
            "m".to_string(),
            crate::config::ModelCostConfig {
                currency: crate::config::CostCurrency::Cny,
                input: 7.25,
                output: 14.5,
                cache_read: None,
            },
        );
        let price = pricing_resolver(&config);
        let cost = price("relay", "m").unwrap();
        assert!((cost.input - 1.0).abs() < 1e-9);
        assert!((cost.output - 2.0).abs() < 1e-9);
        assert_eq!(cost.cache_read, None);
    }

    /// 单价解析与估算:cache_read ⊆ prompt,命中按缓存价、未命中按输入价。
    #[test]
    fn cost_parses_and_estimates() {
        let parsed = parse_api_response(
            r#"{"opencode-go":{"api":"https://opencode.ai/zen/go/v1/","models":{
                "deepseek-v4-flash":{"cost":{"input":0.07,"output":0.14,"cache_read":0.0014}},
                "no-cost":{}
            }}}"#,
        )
        .unwrap();
        assert_eq!(
            parsed.provider_api["opencode-go"],
            "https://opencode.ai/zen/go/v1"
        );
        assert!(parsed.data["opencode-go"]["no-cost"].cost.is_none());
        let cost = parsed.data["opencode-go"]["deepseek-v4-flash"]
            .cost
            .unwrap();
        // 200 万 prompt(其中 100 万命中)+ 100 万输出
        let est = cost.estimate(2_000_000, 1_000_000, 1_000_000, 0);
        assert!((est - (0.07 + 0.0014 + 0.14)).abs() < 1e-9, "{est}");
        // 无缓存价时命中按输入价计
        let flat = ApiCost {
            input: 1.0,
            output: 2.0,
            cache_read: None,
            cache_write: None,
        };
        assert!((flat.estimate(1_000_000, 0, 400_000, 0) - 1.0).abs() < 1e-9);
    }

    #[test]
    fn catalogue_context_window_is_capped_by_the_input_limit() {
        // opencode's big-pickle advertises a 200k context against a 160k input
        // cap; budgeting against 200k puts compaction past the point the
        // provider still accepts the request.
        let parsed = parse_api_response(
            r#"{"opencode":{"models":{
                "big-pickle":{"limit":{"context":200000,"input":160000,"output":32000}},
                "context-only":{"limit":{"context":128000,"output":8000}},
                "input-only":{"limit":{"input":64000}},
                "input-zero":{"limit":{"context":32000,"input":0}},
                "no-limit":{}
            }}}"#,
        )
        .unwrap();
        let models = &parsed.data["opencode"];

        assert_eq!(models["big-pickle"].context_window, Some(160_000));
        assert_eq!(models["context-only"].context_window, Some(128_000));
        assert_eq!(models["input-only"].context_window, Some(64_000));
        assert_eq!(models["input-zero"].context_window, Some(32_000));
        assert_eq!(models["no-limit"].context_window, None);
    }

    #[test]
    fn context_window_prefers_exact_provider() {
        let data = HashMap::from([
            (
                "provider-a".to_string(),
                HashMap::from([("shared-model".to_string(), model(128_000))]),
            ),
            (
                "provider-b".to_string(),
                HashMap::from([("shared-model".to_string(), model(200_000))]),
            ),
        ]);

        assert_eq!(
            lookup_context_window(&data, "provider-a", "shared-model"),
            Some(128_000)
        );
    }

    #[test]
    fn compact_cache_retains_only_configured_models() {
        let config = crate::config::AppConfig::default();
        let provider = &config.providers[0];
        let mut data = HashMap::from([
            (
                provider.id.clone(),
                HashMap::from([
                    (provider.default_model.clone(), model(128_000)),
                    ("unused-model".to_string(), model(64_000)),
                ]),
            ),
            (
                "unused-provider".to_string(),
                HashMap::from([("unused-model".to_string(), model(32_000))]),
            ),
        ]);

        retain_configured_models(&mut data, &config);

        assert!(!data.contains_key("unused-provider"));
        assert!(data[&provider.id].contains_key(&provider.default_model));
        assert!(!data[&provider.id].contains_key("unused-model"));
    }

    #[test]
    fn compact_cache_retains_models_used_only_by_platform_routes() {
        let mut config = crate::config::AppConfig::default();
        let provider_id = config.providers[0].id.clone();
        config.providers[0].models.extend([
            "route-text".to_string(),
            "route-vision".to_string(),
            "platform-text".to_string(),
            "non-whitelist-text".to_string(),
            "context-text".to_string(),
        ]);
        config.platforms.qq.text_models = Some(vec![crate::config::ActiveProviderModelConfig {
            provider_id: provider_id.clone(),
            model: "platform-text".to_string(),
        }]);
        config.platforms.qq.non_whitelist_text_models =
            Some(vec![crate::config::ActiveProviderModelConfig {
                provider_id: provider_id.clone(),
                model: "non-whitelist-text".to_string(),
            }]);
        let mut real_context = crate::config::PlatformPluginInstanceConfig::default();
        crate::config::merge_real_context_settings(
            &mut real_context,
            &crate::config::RealContextPluginSettings {
                text_models: Some(vec![crate::config::ActiveProviderModelConfig {
                    provider_id: provider_id.clone(),
                    model: "context-text".to_string(),
                }]),
                ..Default::default()
            },
        );
        config.platforms.qq.plugins.insert(
            crate::config::REAL_CONTEXT_PLUGIN_ID.to_string(),
            real_context,
        );
        config
            .platforms
            .qq
            .conversations
            .push(crate::config::PlatformModelRoute {
                conversation: crate::config::PlatformConversationConfig {
                    kind: crate::config::PlatformConversationKind::Group,
                    id: "20000".to_string(),
                },
                persona: crate::config::PlatformPersonaOverride::Inherit,
                text_models_inheritance: crate::config::PlatformModelPoolInheritance::Platform,
                text_models: Some(vec![crate::config::ActiveProviderModelConfig {
                    provider_id: provider_id.clone(),
                    model: "route-text".to_string(),
                }]),
                multimodal_models_inheritance:
                    crate::config::PlatformModelPoolInheritance::Platform,
                multimodal_models: Some(vec![crate::config::ActiveProviderModelConfig {
                    provider_id: provider_id.clone(),
                    model: "route-vision".to_string(),
                }]),
                extra_prompt: String::new(),
                session_limits: None,
            });
        let mut data = HashMap::from([(
            provider_id.clone(),
            HashMap::from([
                (config.providers[0].default_model.clone(), model(128_000)),
                ("route-text".to_string(), model(64_000)),
                ("route-vision".to_string(), model(96_000)),
                ("platform-text".to_string(), model(64_000)),
                ("non-whitelist-text".to_string(), model(64_000)),
                ("context-text".to_string(), model(64_000)),
                ("unused-model".to_string(), model(32_000)),
            ]),
        )]);

        retain_configured_models(&mut data, &config);

        let retained = &data[&provider_id];
        assert!(retained.contains_key("route-text"));
        assert!(retained.contains_key("route-vision"));
        assert!(retained.contains_key("platform-text"));
        assert!(retained.contains_key("non-whitelist-text"));
        assert!(retained.contains_key("context-text"));
        assert!(!retained.contains_key("unused-model"));
    }

    #[test]
    fn compact_cache_retains_same_model_metadata_from_other_providers() {
        let mut config = crate::config::AppConfig::default();
        let provider = &mut config.providers[0];
        provider.models = vec!["custom-model".to_string()];
        provider.default_model = "custom-model".to_string();
        let mut data = HashMap::from([
            (
                provider.id.clone(),
                HashMap::from([("custom-model".to_string(), model(64_000))]),
            ),
            (
                "catalog-provider".to_string(),
                HashMap::from([("custom-model".to_string(), model(128_000))]),
            ),
        ]);

        retain_configured_models(&mut data, &config);

        assert!(data.contains_key("catalog-provider"));
        assert_eq!(
            lookup_context_window(&data, "custom-provider", "custom-model"),
            Some(64_000)
        );
    }

    #[test]
    fn provider_api_context_window_accepts_common_metadata_shapes() {
        assert_eq!(
            api_context_window(&serde_json::json!({"context_window": 128000})),
            Some(128000)
        );
        assert_eq!(
            api_context_window(&serde_json::json!({"limit": {"context": 64000}})),
            Some(64000)
        );
        assert_eq!(
            api_context_window(&serde_json::json!({"id": "model"})),
            None
        );
    }

    #[test]
    fn context_window_fallback_uses_the_conservative_minimum() {
        let same = HashMap::from([
            (
                "provider-a".to_string(),
                HashMap::from([("shared-model".to_string(), model(200_000))]),
            ),
            (
                "provider-b".to_string(),
                HashMap::from([("shared-model".to_string(), model(200_000))]),
            ),
        ]);
        assert_eq!(
            lookup_context_window(&same, "custom", "shared-model"),
            Some(200_000)
        );

        let mut conflicting = same;
        conflicting
            .get_mut("provider-b")
            .unwrap()
            .insert("shared-model".to_string(), model(128_000));
        assert_eq!(
            lookup_context_window(&conflicting, "custom", "shared-model"),
            Some(128_000)
        );
    }

    #[test]
    fn parses_reasoning_options_with_provider_mapping() {
        let data = parse_api_response(
            r#"{
                "openrouter": {
                    "npm": "@openrouter/ai-sdk-provider",
                    "models": {
                        "example": {
                            "limit": { "context": 128000, "output": 32000 },
                            "reasoning_options": [
                                { "type": "effort", "values": ["low", "high", null] },
                                { "type": "budget_tokens", "min": -1, "max": 8000 }
                            ]
                        }
                    }
                }
            }"#,
        )
        .unwrap();

        let info = lookup_reasoning_info(&data.data, "openrouter", "example").unwrap();
        assert_eq!(
            info.provider_npm.as_deref(),
            Some("@openrouter/ai-sdk-provider")
        );
        assert_eq!(
            info.variants,
            vec![
                ReasoningVariant {
                    id: "low".to_string(),
                    setting: ReasoningSetting::Effort("low".to_string()),
                },
                ReasoningVariant {
                    id: "high".to_string(),
                    setting: ReasoningSetting::Effort("high".to_string()),
                },
                ReasoningVariant {
                    id: "none".to_string(),
                    setting: ReasoningSetting::Disabled,
                },
            ]
        );
    }

    #[test]
    fn negative_budget_min_uses_zero_floor() {
        let variants = reasoning_variants(
            &[ApiReasoningOption::BudgetTokens {
                min: Some(-1),
                max: Some(8000),
            }],
            Some(32_000),
        );
        assert_eq!(
            variants,
            vec![
                ReasoningVariant {
                    id: "high".to_string(),
                    setting: ReasoningSetting::BudgetTokens(4000),
                },
                ReasoningVariant {
                    id: "max".to_string(),
                    setting: ReasoningSetting::BudgetTokens(8000),
                },
            ]
        );
    }

    #[test]
    fn reasoning_fallback_keeps_shared_variants_without_provider_mapping() {
        let variants = vec![ReasoningVariant {
            id: "high".to_string(),
            setting: ReasoningSetting::Effort("high".to_string()),
        }];
        let data = HashMap::from([
            (
                "provider-a".to_string(),
                HashMap::from([(
                    "shared-model".to_string(),
                    ModelInfo {
                        input_modalities: Vec::new(),
                        context_window: None,
                        reasoning: Some(ModelReasoningInfo {
                            provider_npm: Some("@provider/a".to_string()),
                            variants: variants.clone(),
                        }),
                        cost: None,
                    },
                )]),
            ),
            (
                "provider-b".to_string(),
                HashMap::from([(
                    "shared-model".to_string(),
                    ModelInfo {
                        input_modalities: Vec::new(),
                        context_window: None,
                        reasoning: Some(ModelReasoningInfo {
                            provider_npm: Some("@provider/b".to_string()),
                            variants,
                        }),
                        cost: None,
                    },
                )]),
            ),
        ]);

        let info = lookup_reasoning_info(&data, "custom", "shared-model").unwrap();
        assert_eq!(info.provider_npm, None);
        assert_eq!(info.variants.len(), 1);
    }

    #[test]
    fn reasoning_fallback_prefers_canonical_model_provider() {
        let high_max = vec![
            ReasoningVariant {
                id: "high".to_string(),
                setting: ReasoningSetting::Effort("high".to_string()),
            },
            ReasoningVariant {
                id: "max".to_string(),
                setting: ReasoningSetting::Effort("max".to_string()),
            },
        ];
        let low = vec![ReasoningVariant {
            id: "low".to_string(),
            setting: ReasoningSetting::Effort("low".to_string()),
        }];
        let reasoning = |variants| ModelInfo {
            cost: None,
            input_modalities: Vec::new(),
            context_window: None,
            reasoning: Some(ModelReasoningInfo {
                provider_npm: Some("@ai-sdk/openai-compatible".to_string()),
                variants,
            }),
        };
        let data = HashMap::from([
            (
                "deepseek".to_string(),
                HashMap::from([("deepseek-v4-flash".to_string(), reasoning(high_max.clone()))]),
            ),
            (
                "gateway".to_string(),
                HashMap::from([("deepseek-v4-flash".to_string(), reasoning(low))]),
            ),
        ]);

        let info = lookup_reasoning_info(&data, "ririxin", "deepseek-v4-flash").unwrap();
        assert_eq!(info.variants, high_max);
    }

    #[test]
    fn reasoning_fallback_counts_models_without_variants() {
        let reasoning = ModelInfo {
            cost: None,
            input_modalities: Vec::new(),
            context_window: None,
            reasoning: Some(ModelReasoningInfo {
                provider_npm: None,
                variants: vec![ReasoningVariant {
                    id: "high".to_string(),
                    setting: ReasoningSetting::Effort("high".to_string()),
                }],
            }),
        };
        let without_reasoning = ModelInfo {
            cost: None,
            input_modalities: Vec::new(),
            context_window: None,
            reasoning: None,
        };
        let data = HashMap::from([
            (
                "gateway-a".to_string(),
                HashMap::from([("custom-model".to_string(), reasoning)]),
            ),
            (
                "gateway-b".to_string(),
                HashMap::from([("custom-model".to_string(), without_reasoning.clone())]),
            ),
            (
                "gateway-c".to_string(),
                HashMap::from([("custom-model".to_string(), without_reasoning)]),
            ),
        ]);

        assert_eq!(
            lookup_reasoning_info(&data, "private", "custom-model"),
            None
        );
    }
}
