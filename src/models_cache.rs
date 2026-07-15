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
}

#[derive(Debug, Deserialize)]
struct ApiModel {
    #[serde(default)]
    modalities: Option<ApiModalities>,
    #[serde(default)]
    limit: Option<ApiLimit>,
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
}

#[derive(Debug, Clone)]
pub struct ModelInfo {
    pub input_modalities: Vec<String>,
    pub context_window: Option<u64>,
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
            models.insert(
                model_id,
                ModelInfo {
                    input_modalities: input,
                    context_window: model.limit.and_then(|l| l.context),
                },
            );
        }
        result.insert(provider_id, models);
    }
    Ok(result)
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
}
