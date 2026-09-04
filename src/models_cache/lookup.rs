//! 按「供应商 + 模型」查元数据。
//!
//! 难在**供应商名字对不上**：配置里的 base_url 和 models.dev 的 provider id 不
//! 是一回事（`opencodego` vs `opencode-go`）。`canonical_provider_candidates`
//! 生成几个候选写法逐个试。
//!
//! 查不到就返回 None，由调用方显示「—」。**不要猜一个值**——猜错的价格比没有
//! 价格更糟。

use crate::models_cache::*;

/// 人民币手动价折算 USD 的估算汇率。计费本就是估算,固定汇率的误差
/// 远小于价格本身的不确定度;真要精确对账应直接看供应商账单。
pub(crate) const CNY_PER_USD: f64 = 7.25;

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

pub(crate) fn lookup_input_modalities(
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
    if let Some(window) = provider_api_cache_lock()
        .lock()
        .unwrap()
        .get(&(provider_id.to_string(), model_id.to_string()))
        .copied()
    {
        return Some(window);
    }
    let lock = cache_lock().lock().unwrap();
    let cache = lock.as_ref()?;
    lookup_context_window(&cache.data, provider_id, model_id)
}

pub fn reasoning_info(provider_id: &str, model_id: &str) -> Option<ModelReasoningInfo> {
    let lock = cache_lock().lock().unwrap();
    let cache = lock.as_ref()?;
    lookup_reasoning_info(&cache.data, provider_id, model_id)
}

pub(crate) fn lookup_reasoning_info(
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

pub(crate) fn canonical_provider_candidates(
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

pub(crate) fn lookup_context_window(
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
    matches.into_iter().min()
}

/// 模型单价查询,供计费估算。供应商对齐两步走:① Miyu 供应商 id 恰好是
/// models.dev 键(deepseek、openrouter 等官方模板);② 按 base_url 对齐
/// (自定义 id,如 opencodego → opencode-go)。都对不上就不猜——同名
/// 模型在不同渠道价格不同,跨供应商模糊匹配会算错钱。
pub fn model_cost(provider_id: &str, base_url: &str, model_id: &str) -> Option<ApiCost> {
    let lock = cache_lock().lock().unwrap();
    let cache = lock.as_ref()?;
    if let Some(cost) = cache
        .data
        .get(provider_id)
        .and_then(|models| models.get(model_id))
        .and_then(|info| info.cost)
    {
        return Some(cost);
    }
    let normalized = base_url.trim().trim_end_matches('/');
    if normalized.is_empty() {
        return None;
    }
    cache
        .provider_api
        .iter()
        .filter(|(_, api)| api.as_str() == normalized)
        .find_map(|(key, _)| {
            cache
                .data
                .get(key)
                .and_then(|models| models.get(model_id))
                .and_then(|info| info.cost)
        })
}

/// 用量统计的计价器:usage 记录只存供应商 id,这里借 config 把 id 解析
/// 成 base_url 再查目录。查不到价的记录计 None(前端显示为无估算),
/// 绝不糊弄一个数字。
pub fn pricing_resolver(
    config: &crate::config::AppConfig,
) -> impl Fn(&str, &str) -> Option<ApiCost> + '_ {
    move |provider_id: &str, model_id: &str| {
        if provider_id.is_empty() || model_id.is_empty() {
            return None;
        }
        let provider = config
            .providers
            .iter()
            .find(|provider| provider.id == provider_id);
        // 手动价格优先:目录没收录的中转/赠送端点靠它。CNY 按估算汇率
        // 折成 USD 聚合(统计页统一以 $ 展示)。
        if let Some(manual) = provider.and_then(|p| p.model_costs.get(model_id)) {
            let rate = match manual.currency {
                crate::config::CostCurrency::Usd => 1.0,
                crate::config::CostCurrency::Cny => 1.0 / CNY_PER_USD,
            };
            return Some(ApiCost {
                input: manual.input * rate,
                output: manual.output * rate,
                cache_read: manual.cache_read.map(|price| price * rate),
                cache_write: None,
            });
        }
        let base_url = provider.map(|p| p.base_url.as_str()).unwrap_or("");
        model_cost(provider_id, base_url, model_id)
    }
}

/// 供 WebUI「拉取模型列表 / 从目录补全」用的一次性全量查询。
///
/// 常驻缓存在 daemon 里是按已配置模型裁剪过的(`retain_configured_models`),
/// 新拉下来的模型查不到;这里直接读磁盘上的完整目录(没有就联网取一次),
/// 不碰全局缓存。3.6MB 的解析只在用户点按钮时发生。
#[derive(Debug, Clone, Serialize)]
pub struct ModelCatalogEntry {
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_window: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub modalities: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cost: Option<ModelCatalogCost>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ModelCatalogCost {
    pub input: f64,
    pub output: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_read: Option<f64>,
}

pub fn describe_models(
    paths: &crate::paths::MiyuPaths,
    provider_id: &str,
    base_url: &str,
    models: &[String],
) -> Vec<ModelCatalogEntry> {
    let path = super::cache_file(paths);
    let cache = match super::load_from_disk(&path).or_else(|_| super::fetch_and_cache(&path)) {
        Ok(cache) => cache,
        Err(_) => {
            return models
                .iter()
                .map(|id| ModelCatalogEntry {
                    id: id.clone(),
                    context_window: None,
                    modalities: None,
                    cost: None,
                })
                .collect();
        }
    };
    let normalized_base = base_url.trim().trim_end_matches('/');
    let aligned_providers: Vec<&String> = cache
        .provider_api
        .iter()
        .filter(|(_, api)| !normalized_base.is_empty() && api.as_str() == normalized_base)
        .map(|(key, _)| key)
        .collect();
    models
        .iter()
        .map(|model_id| {
            let cost = cache
                .data
                .get(provider_id)
                .and_then(|models| models.get(model_id))
                .and_then(|info| info.cost)
                .or_else(|| {
                    aligned_providers.iter().find_map(|key| {
                        cache
                            .data
                            .get(key.as_str())
                            .and_then(|models| models.get(model_id))
                            .and_then(|info| info.cost)
                    })
                })
                .map(|cost| ModelCatalogCost {
                    input: cost.input,
                    output: cost.output,
                    cache_read: cost.cache_read,
                });
            ModelCatalogEntry {
                id: model_id.clone(),
                context_window: lookup_context_window(&cache.data, provider_id, model_id),
                modalities: lookup_input_modalities(&cache.data, provider_id, model_id),
                cost,
            }
        })
        .collect()
}
