use anyhow::{Context, Result};
use serde::Deserialize;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

const API_URL: &str = "https://models.dev/api.json";

#[derive(Debug, Deserialize)]
struct ApiResponse(HashMap<String, ApiProvider>);

#[derive(Debug, Deserialize)]
struct ApiProvider {
    #[serde(default)]
    models: HashMap<String, ApiModel>,
    #[serde(default)]
    npm: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ApiModel {
    #[serde(default)]
    modalities: Option<ApiModalities>,
    #[serde(default)]
    limit: Option<ApiLimit>,
    #[serde(default)]
    reasoning_options: Vec<ApiReasoningOption>,
    #[serde(default)]
    provider: Option<ApiModelProvider>,
}

#[derive(Debug, Deserialize)]
struct ApiModalities {
    #[serde(default)]
    input: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct ApiLimit {
    #[serde(default)]
    context: Option<u64>,
    #[serde(default)]
    output: Option<u64>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
enum ApiReasoningOption {
    #[serde(rename = "effort")]
    Effort {
        #[serde(default)]
        values: Vec<Option<String>>,
    },
    #[serde(rename = "toggle")]
    Toggle,
    #[serde(rename = "budget_tokens")]
    BudgetTokens {
        #[serde(default)]
        min: Option<i64>,
        #[serde(default)]
        max: Option<i64>,
    },
}

#[derive(Debug, Deserialize)]
struct ApiModelProvider {
    #[serde(default)]
    npm: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelInfo {
    pub input_modalities: Vec<String>,
    pub context_window: Option<u64>,
    reasoning: Option<ModelReasoningInfo>,
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
}

static CACHE: OnceLock<Mutex<Option<Cache>>> = OnceLock::new();
static REFRESH_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

fn cache_lock() -> &'static Mutex<Option<Cache>> {
    CACHE.get_or_init(|| Mutex::new(None))
}

fn refresh_lock() -> &'static Mutex<()> {
    REFRESH_LOCK.get_or_init(|| Mutex::new(()))
}

pub fn is_loaded() -> bool {
    cache_lock().lock().unwrap().is_some()
}

fn cache_file(paths: &crate::paths::MiyuPaths) -> PathBuf {
    paths.cache_dir.join("models_cache.json")
}

fn load_from_disk(path: &PathBuf) -> Result<HashMap<String, HashMap<String, ModelInfo>>> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read models cache: {}", path.display()))?;
    parse_api_response(&text)
}

fn parse_api_response(text: &str) -> Result<HashMap<String, HashMap<String, ModelInfo>>> {
    let api: ApiResponse = serde_json::from_str(&text).context("failed to parse models cache")?;
    let mut result = HashMap::new();
    for (provider_id, provider) in api.0 {
        let mut models = HashMap::new();
        for (model_id, model) in provider.models {
            let input = model.modalities.map(|m| m.input).unwrap_or_default();
            let limit = model.limit.unwrap_or(ApiLimit {
                context: None,
                output: None,
            });
            let variants = reasoning_variants(&model.reasoning_options, limit.output);
            models.insert(
                model_id,
                ModelInfo {
                    input_modalities: input,
                    context_window: limit.context,
                    reasoning: (!variants.is_empty()).then_some(ModelReasoningInfo {
                        provider_npm: model
                            .provider
                            .and_then(|model_provider| model_provider.npm)
                            .or_else(|| provider.npm.clone()),
                        variants,
                    }),
                },
            );
        }
        result.insert(provider_id, models);
    }
    Ok(result)
}

fn reasoning_variants(
    options: &[ApiReasoningOption],
    output_limit: Option<u64>,
) -> Vec<ReasoningVariant> {
    if let Some(ApiReasoningOption::Effort { values }) = options
        .iter()
        .find(|option| matches!(option, ApiReasoningOption::Effort { .. }))
    {
        return values
            .iter()
            .map(|value| match value.as_deref().map(str::trim) {
                Some(value) if !value.is_empty() => ReasoningVariant {
                    id: value.to_string(),
                    setting: ReasoningSetting::Effort(value.to_string()),
                },
                _ => ReasoningVariant {
                    id: "none".to_string(),
                    setting: ReasoningSetting::Disabled,
                },
            })
            .collect();
    }
    let mut variants = Vec::new();
    for option in options {
        match option {
            ApiReasoningOption::Effort { .. } => unreachable!(),
            ApiReasoningOption::Toggle => {
                push_variant(
                    &mut variants,
                    "on".to_string(),
                    ReasoningSetting::Toggle(true),
                );
                push_variant(
                    &mut variants,
                    "off".to_string(),
                    ReasoningSetting::Toggle(false),
                );
            }
            ApiReasoningOption::BudgetTokens { min, max } => {
                let maximum = max
                    .and_then(|value| u64::try_from(value).ok())
                    .or(output_limit)
                    .unwrap_or_default();
                if maximum == 0 {
                    continue;
                }
                let minimum = min
                    .and_then(|value| u64::try_from(value).ok())
                    .unwrap_or_default()
                    .min(maximum);
                let high = ((maximum.saturating_add(1)) / 2).max(minimum);
                push_variant(
                    &mut variants,
                    "high".to_string(),
                    ReasoningSetting::BudgetTokens(high),
                );
                if high != maximum {
                    push_variant(
                        &mut variants,
                        "max".to_string(),
                        ReasoningSetting::BudgetTokens(maximum),
                    );
                }
            }
        }
    }
    variants
}

fn push_variant(variants: &mut Vec<ReasoningVariant>, id: String, setting: ReasoningSetting) {
    if variants.iter().any(|variant| variant.id == id) {
        return;
    }
    variants.push(ReasoningVariant { id, setting });
}

fn fetch_and_cache(path: &PathBuf) -> Result<HashMap<String, HashMap<String, ModelInfo>>> {
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
    let data = parse_api_response(&text)?;
    let parent = path.parent().context("models cache path has no parent")?;
    std::fs::create_dir_all(parent)?;
    let mut temp = tempfile::NamedTempFile::new_in(parent)?;
    use std::io::Write;
    temp.write_all(text.as_bytes())?;
    temp.persist(path)
        .map_err(|error| error.error)
        .context("failed to replace models cache")?;
    Ok(data)
}

pub fn try_load(paths: &crate::paths::MiyuPaths) {
    let path = cache_file(paths);
    let data = load_from_disk(&path).ok();
    if let Some(data) = data {
        let mut lock = cache_lock().lock().unwrap();
        *lock = Some(Cache { data });
    }
}

pub fn spawn_background_refresh(paths: crate::paths::MiyuPaths) {
    let path = cache_file(&paths);
    std::thread::spawn(move || {
        let _refresh = refresh_lock().lock().unwrap();
        let fetched = fetch_and_cache(&path).ok();
        if let Some(data) = fetched {
            let mut lock = cache_lock().lock().unwrap();
            *lock = Some(Cache { data });
        }
    });
}

pub fn input_modalities(provider_id: &str, model_id: &str) -> Option<Vec<String>> {
    let lock = cache_lock().lock().unwrap();
    let cache = lock.as_ref()?;
    lookup_input_modalities(&cache.data, provider_id, model_id)
}

pub fn input_modalities_blocking(
    paths: &crate::paths::MiyuPaths,
    provider_id: &str,
    model_id: &str,
) -> Option<Vec<String>> {
    if let Some(modalities) = input_modalities(provider_id, model_id) {
        return Some(modalities);
    }
    refresh_blocking(paths).ok()?;
    input_modalities(provider_id, model_id)
}

fn lookup_input_modalities(
    data: &HashMap<String, HashMap<String, ModelInfo>>,
    provider_id: &str,
    model_id: &str,
) -> Option<Vec<String>> {
    if let Some(info) = data
        .get(provider_id)
        .and_then(|provider| provider.get(model_id))
        .filter(|info| !info.input_modalities.is_empty())
    {
        return Some(info.input_modalities.clone());
    }

    let mut matches = data
        .values()
        .filter_map(|provider| provider.get(model_id))
        .filter(|info| !info.input_modalities.is_empty())
        .map(|info| info.input_modalities.clone())
        .collect::<Vec<_>>();
    matches.sort();
    matches.dedup();
    if matches.len() == 1 {
        matches.pop()
    } else {
        None
    }
}

pub fn context_window(provider_id: &str, model_id: &str) -> Option<u64> {
    let lock = cache_lock().lock().unwrap();
    let cache = lock.as_ref()?;
    lookup_context_window(&cache.data, provider_id, model_id)
}

pub fn reasoning_info(provider_id: &str, model_id: &str) -> Option<ModelReasoningInfo> {
    let lock = cache_lock().lock().unwrap();
    let cache = lock.as_ref()?;
    lookup_reasoning_info(&cache.data, provider_id, model_id)
}

fn lookup_reasoning_info(
    data: &HashMap<String, HashMap<String, ModelInfo>>,
    provider_id: &str,
    model_id: &str,
) -> Option<ModelReasoningInfo> {
    if let Some(info) = data
        .get(provider_id)
        .and_then(|provider| provider.get(model_id))
    {
        return info.reasoning.clone();
    }

    for canonical_provider in canonical_provider_candidates(data, model_id) {
        if let Some(info) = data
            .get(&canonical_provider)
            .and_then(|provider| provider.get(model_id))
        {
            return info.reasoning.clone();
        }
    }

    let matches = data
        .values()
        .filter_map(|provider| provider.get(model_id))
        .map(|info| info.reasoning.clone())
        .collect::<Vec<_>>();
    let mut groups = Vec::<(Option<ModelReasoningInfo>, usize)>::new();
    for info in matches {
        if let Some((existing, count)) =
            groups
                .iter_mut()
                .find(|(existing, _)| match (existing.as_ref(), info.as_ref()) {
                    (Some(existing), Some(info)) => existing.variants == info.variants,
                    (None, None) => true,
                    _ => false,
                })
        {
            *count += 1;
            if let (Some(existing), Some(info)) = (existing.as_mut(), info.as_ref()) {
                if existing.provider_npm != info.provider_npm {
                    existing.provider_npm = None;
                }
            }
        } else {
            groups.push((info, 1));
        }
    }
    groups.sort_by(|left, right| right.1.cmp(&left.1));
    let (info, count) = groups.first()?;
    if groups
        .get(1)
        .is_some_and(|(_, next_count)| next_count == count)
    {
        return None;
    }
    info.clone()
}

fn canonical_provider_candidates(
    data: &HashMap<String, HashMap<String, ModelInfo>>,
    model_id: &str,
) -> Vec<String> {
    let lower = model_id.to_ascii_lowercase();
    let mut candidates = Vec::new();
    if let Some((namespace, _)) = lower.split_once('/') {
        candidates.push(namespace.to_string());
    }
    let alias = if lower.starts_with("gpt-")
        || lower.starts_with("o1")
        || lower.starts_with("o3")
        || lower.starts_with("o4")
    {
        Some("openai")
    } else if lower.starts_with("claude-") {
        Some("anthropic")
    } else if lower.starts_with("gemini-") {
        Some("google")
    } else if lower.starts_with("grok-") {
        Some("xai")
    } else if lower.starts_with("qwen") {
        Some("alibaba")
    } else {
        None
    };
    if let Some(alias) = alias {
        candidates.push(alias.to_string());
    }
    let mut prefixes = data
        .keys()
        .filter(|provider_id| lower.starts_with(&format!("{}-", provider_id.to_ascii_lowercase())))
        .cloned()
        .collect::<Vec<_>>();
    prefixes.sort_by_key(|provider_id| std::cmp::Reverse(provider_id.len()));
    candidates.extend(prefixes);
    candidates.dedup();
    candidates
}

fn lookup_context_window(
    data: &HashMap<String, HashMap<String, ModelInfo>>,
    provider_id: &str,
    model_id: &str,
) -> Option<u64> {
    if let Some(window) = data
        .get(provider_id)
        .and_then(|provider| provider.get(model_id))
        .and_then(|info| info.context_window)
    {
        return Some(window);
    }

    let mut matches = data
        .values()
        .filter_map(|provider| provider.get(model_id))
        .filter_map(|info| info.context_window)
        .collect::<Vec<_>>();
    matches.sort_unstable();
    matches.dedup();
    (matches.len() == 1).then(|| matches[0])
}

#[allow(dead_code)]
pub fn refresh_blocking(paths: &crate::paths::MiyuPaths) -> Result<()> {
    let _refresh = refresh_lock().lock().unwrap();
    if is_loaded() {
        return Ok(());
    }
    let path = cache_file(paths);
    let data = fetch_and_cache(&path)?;
    let mut lock = cache_lock().lock().unwrap();
    *lock = Some(Cache { data });
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
        }
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
    fn context_window_fallback_requires_one_unique_value() {
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
            None
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

        let info = lookup_reasoning_info(&data, "openrouter", "example").unwrap();
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
