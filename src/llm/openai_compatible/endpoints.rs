//! 端点调度：轮换、冷却与故障转移。
//!
//! 一个供应商可以配多个端点。`LlmScheduler` 决定这一次用哪个，并在失败后按
//! 失败类型给冷却时间——429 冷却久一点，连接失败短一点，参数错误根本不换端点
//! （换了还是错，只是白烧三次配额）。
//!
//! `endpoint_failover_allowed` 有个硬约束：**已经吐给用户的内容不能回退**。流
//! 式输出一旦提交就不能换端点重来，否则用户会看到同一段话说两遍。

use crate::llm::openai_compatible::*;

/// Responses 续传健康位(任务#16 自愈)。跨 clone 共享:压缩器等辅助克隆
/// 与主客户端看到同一份;置位即进程内立即生效,并持久化到
/// provider-capabilities.json 供后续会话读取。多供应商混池时按主
/// provider 记录(续传本就钉在单端点上,混池仅有过度抑制的轻微风险)。
#[derive(Clone)]
pub(in crate::llm::openai_compatible) struct ResponsesContinuationHealth {
    pub(in crate::llm::openai_compatible) unsupported: Arc<std::sync::atomic::AtomicBool>,
    pub(in crate::llm::openai_compatible) store: std::path::PathBuf,
    pub(in crate::llm::openai_compatible) base_url: String,
    pub(in crate::llm::openai_compatible) provider_id: String,
}

impl ResponsesContinuationHealth {
    pub(in crate::llm::openai_compatible) fn for_provider(
        paths: &MiyuPaths,
        provider: &ProviderConfig,
    ) -> Self {
        let store = crate::llm::provider_capabilities::store_path(&paths.cache_dir);
        let unsupported =
            crate::llm::provider_capabilities::continuation_unsupported(&store, &provider.base_url);
        Self {
            unsupported: Arc::new(std::sync::atomic::AtomicBool::new(unsupported)),
            store,
            base_url: provider.base_url.clone(),
            provider_id: provider.id.clone(),
        }
    }

    /// 测试用:无持久化、乐观放行。
    pub(in crate::llm::openai_compatible) fn detached() -> Self {
        Self {
            unsupported: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            store: std::path::PathBuf::new(),
            base_url: String::new(),
            provider_id: String::new(),
        }
    }
}

#[derive(Clone)]
pub(in crate::llm::openai_compatible) struct LlmEndpoint {
    pub(in crate::llm::openai_compatible) client: Client,
    pub(in crate::llm::openai_compatible) provider: ProviderConfig,
    pub(in crate::llm::openai_compatible) api_key: String,
    pub(in crate::llm::openai_compatible) key_index: usize,
}

impl LlmEndpoint {
    pub(in crate::llm::openai_compatible) fn id(&self) -> String {
        endpoint_id(
            &self.provider.id,
            &self.provider.default_model,
            self.key_index,
        )
    }
}

#[derive(Default)]
pub(in crate::llm::openai_compatible) struct LlmScheduler {
    pub(in crate::llm::openai_compatible) cursor: usize,
    pub(in crate::llm::openai_compatible) cooldowns: HashMap<String, Instant>,
    /// 连败计数(按端点 id)。冷却到期被探测不清零,只有一次真实成功才清
    /// ——持续故障的端点探测频率按此指数衰减(08-24:扁平 120s 让挂死的
    /// mimo 车道每两分钟被重新信任一次,每次又是 30s+ 的等待)。
    pub(in crate::llm::openai_compatible) failure_streaks: HashMap<String, u32>,
}

/// 瞬时故障档(120s 基础)的冷却上限。
const TRANSIENT_COOLDOWN_CAP: Duration = Duration::from_secs(30 * 60);
/// 限流/认证档(600s 基础)的冷却上限。
const QUOTA_COOLDOWN_CAP: Duration = Duration::from_secs(60 * 60);

/// 连败指数退避:第 n 次连败 = base × 2^(n-1),按档位封顶。
fn escalated_cooldown(base: Duration, streak: u32) -> Duration {
    let cap = if base >= Duration::from_secs(600) {
        QUOTA_COOLDOWN_CAP
    } else {
        TRANSIENT_COOLDOWN_CAP
    };
    let factor = 1u32 << streak.saturating_sub(1).min(16);
    base.saturating_mul(factor).min(cap)
}

impl LlmScheduler {
    pub(in crate::llm::openai_compatible) fn ordered_indices(
        &mut self,
        endpoints: &[LlmEndpoint],
    ) -> Vec<usize> {
        let available = endpoints
            .iter()
            .enumerate()
            .filter_map(|(index, endpoint)| self.is_ready(&endpoint.id()).then_some(index))
            .collect::<Vec<_>>();
        if available.is_empty() {
            return Vec::new();
        }
        let start = self.cursor % available.len();
        self.cursor = self.cursor.wrapping_add(1);
        rotate_from(available, start)
    }

    pub(in crate::llm::openai_compatible) fn is_ready(&mut self, id: &str) -> bool {
        match self.cooldowns.get(id).copied() {
            Some(until) if until > Instant::now() => false,
            Some(_) => {
                self.cooldowns.remove(id);
                true
            }
            None => true,
        }
    }

    /// The endpoint whose cooldown lifts first, for the single probe sent when
    /// every endpoint is cooling down. `None` sorts ahead of any deadline, so
    /// an endpoint with no cooldown recorded wins outright.
    pub(in crate::llm::openai_compatible) fn soonest_ready_index(
        &self,
        endpoints: &[LlmEndpoint],
    ) -> Option<usize> {
        endpoints
            .iter()
            .enumerate()
            .min_by_key(|(_, endpoint)| self.cooldowns.get(&endpoint.id()).copied())
            .map(|(index, _)| index)
    }

    pub(in crate::llm::openai_compatible) fn mark_success(&mut self, id: &str) {
        self.cooldowns.remove(id);
        self.failure_streaks.remove(id);
    }

    /// 记一次失败,返回实际生效的(可能已升级的)冷却时长。
    pub(in crate::llm::openai_compatible) fn mark_failure(
        &mut self,
        id: String,
        base: Duration,
    ) -> Duration {
        let streak = self.failure_streaks.entry(id.clone()).or_insert(0);
        *streak = streak.saturating_add(1);
        let effective = escalated_cooldown(base, *streak);
        self.cooldowns.insert(id, Instant::now() + effective);
        effective
    }
}

pub(in crate::llm::openai_compatible) fn rotate_from<T>(mut items: Vec<T>, start: usize) -> Vec<T> {
    items.rotate_left(start);
    items
}

pub(in crate::llm::openai_compatible) fn endpoint_id(
    provider_id: &str,
    model: &str,
    key_index: usize,
) -> String {
    format!("{provider_id}\t{model}\t{key_index}")
}

pub(in crate::llm::openai_compatible) fn ordered_endpoint_indices(
    endpoints: &[LlmEndpoint],
) -> Vec<usize> {
    LLM_SCHEDULER
        .lock()
        .map(|mut scheduler| scheduler.ordered_indices(endpoints))
        .unwrap_or_else(|_| (0..endpoints.len()).collect())
}

pub(in crate::llm::openai_compatible) fn soonest_ready_endpoint_index(
    endpoints: &[LlmEndpoint],
) -> Option<usize> {
    LLM_SCHEDULER
        .lock()
        .ok()
        .and_then(|scheduler| scheduler.soonest_ready_index(endpoints))
        .or_else(|| (!endpoints.is_empty()).then_some(0))
}

pub(in crate::llm::openai_compatible) fn mark_endpoint_success(endpoint: &LlmEndpoint) {
    if let Ok(mut scheduler) = LLM_SCHEDULER.lock() {
        scheduler.mark_success(&endpoint.id());
    }
}

pub(in crate::llm::openai_compatible) fn mark_endpoint_failure(
    endpoint: &LlmEndpoint,
    error: &anyhow::Error,
) -> Option<Duration> {
    let base = cooldown_for_error(error)?;
    let mut scheduler = LLM_SCHEDULER.lock().ok()?;
    Some(scheduler.mark_failure(endpoint.id(), base))
}

pub(in crate::llm::openai_compatible) fn cooldown_for_status(status: u16) -> Option<Duration> {
    match status {
        401 | 403 | 429 => Some(Duration::from_secs(600)),
        408 | 500..=599 => Some(Duration::from_secs(120)),
        _ => None,
    }
}

pub(in crate::llm::openai_compatible) fn cooldown_for_error(
    error: &anyhow::Error,
) -> Option<Duration> {
    if let Some(failure) = error.downcast_ref::<HttpStatusFailure>() {
        return match failure.kind {
            HttpFailureKind::Authentication | HttpFailureKind::RateLimit => {
                Some(Duration::from_secs(600))
            }
            HttpFailureKind::EndpointUnavailable => Some(Duration::from_secs(120)),
            HttpFailureKind::EndpointIncompatible | HttpFailureKind::InvalidRequest => None,
            HttpFailureKind::Status => cooldown_for_status(failure.status),
        };
    }
    if error.downcast_ref::<TransportFailure>().is_some() {
        return Some(Duration::from_secs(120));
    }
    error
        .downcast_ref::<reqwest::Error>()
        .filter(|error| error.is_connect() || error.is_timeout())
        .map(|_| Duration::from_secs(120))
}

pub(in crate::llm::openai_compatible) fn endpoint_failover_allowed(error: &anyhow::Error) -> bool {
    !error
        .downcast_ref::<HttpStatusFailure>()
        .is_some_and(|failure| failure.kind == HttpFailureKind::InvalidRequest)
}

/// Whether the *same* endpoint may be tried again inside one request. A 429 or
/// a rejected key is a verdict on that provider/model/key, not a moment in
/// time: the retries `MIN_ENDPOINT_ATTEMPTS` pads in would fire back-to-back
/// with no backoff and spend more of a quota that already said no — which on a
/// shared free tier is what exhausted it. Failover to a *different* endpoint is
/// unaffected; that is `endpoint_failover_allowed`'s job.
pub(in crate::llm::openai_compatible) fn same_endpoint_retry_allowed(
    error: &anyhow::Error,
) -> bool {
    !error
        .downcast_ref::<HttpStatusFailure>()
        .is_some_and(|failure| {
            matches!(
                failure.kind,
                HttpFailureKind::Authentication | HttpFailureKind::RateLimit
            )
        })
}

pub(in crate::llm::openai_compatible) fn endpoint_client(
    provider: &ProviderConfig,
) -> Result<Client> {
    // Auxiliary callers (judge/affection/organizer) rebuild their client per
    // call; without this cache every judge run pays fresh TLS setup and loses
    // connection reuse. Keyed by every input the builder consumes, so a config
    // edit that changes the timeout naturally mints a new client; the map is
    // bounded by the number of distinct providers. `reqwest::Client` is an Arc
    // handle — clones share one pool.
    static CLIENTS: std::sync::OnceLock<std::sync::Mutex<HashMap<(String, u64), Client>>> =
        std::sync::OnceLock::new();
    let timeout = provider.timeout_seconds.clamp(5, 30);
    let key = (provider.id.clone(), timeout);
    let mut cache = CLIENTS
        .get_or_init(|| std::sync::Mutex::new(HashMap::new()))
        .lock()
        .unwrap();
    if let Some(client) = cache.get(&key) {
        return Ok(client.clone());
    }
    let client = Client::builder()
        .connect_timeout(Duration::from_secs(timeout))
        .build()
        .with_context(|| format!("building HTTP client for provider {}", provider.id))?;
    cache.insert(key, client.clone());
    Ok(client)
}

/// 配了活跃模型却一个都解析不动时,逐条说明原因。
///
/// `active_provider_model_choices` 把解析不动的条目**静默筛掉**,于是
/// `llm_endpoints` 的循环一次都不进,报错尾巴是空的:
/// "no active provider/model endpoint is configured:\n- "。用户和排查者都
/// 看不出发生了什么(08-28 实录:会话模型覆盖指向已从供应商移除的模型)。
fn stale_active_model_reasons(config: &AppConfig) -> Vec<String> {
    config
        .active_provider_models
        .iter()
        .flatten()
        .map(|active| {
            let provider_id = active.provider_id.trim();
            let model = active.model.trim();
            let reason = match config.provider(Some(provider_id)) {
                Err(_) => t(
                    "provider not found in the configuration",
                    "配置里找不到这个供应商",
                ),
                Ok(_) => t(
                    "the provider no longer lists this model (a session model override may point at a removed model)",
                    "该模型已不在供应商的模型列表里(会话模型覆盖可能指向了已删除的模型)",
                ),
            };
            format!("{provider_id} / {model}: {reason}")
        })
        .collect()
}

pub(in crate::llm::openai_compatible) fn llm_endpoints(
    config: &AppConfig,
    paths: &MiyuPaths,
) -> Result<Vec<LlmEndpoint>> {
    let mut endpoints = Vec::new();
    let mut errors = Vec::new();
    let choices = config.active_provider_model_choices();
    if choices.is_empty() {
        errors.extend(stale_active_model_reasons(config));
    }
    for choice in choices {
        let mut provider = config.provider(Some(&choice.provider_id))?.clone();
        if !provider.enabled {
            errors.push(format!(
                "{}: {}",
                provider.id,
                t(
                    "provider is disabled; enable it in the provider settings",
                    "供应商未启用;请在供应商设置里启用"
                )
            ));
            continue;
        }
        provider.default_model = choice.model;
        let client = endpoint_client(&provider)?;
        if provider_uses_cli_relay(&provider) {
            // claude-code/antigravity 走本机 CLI 的登录态,没有 API key;单端点直进池。
            endpoints.push(LlmEndpoint {
                client: client.clone(),
                provider: provider.clone(),
                api_key: String::new(),
                key_index: 0,
            });
            continue;
        }
        match provider.resolved_api_keys(paths) {
            Ok(keys) => {
                for key in keys {
                    endpoints.push(LlmEndpoint {
                        client: client.clone(),
                        provider: provider.clone(),
                        api_key: key.value,
                        key_index: key.index,
                    });
                }
            }
            Err(err) => errors.push(format!(
                "{} / {}: {err}",
                provider.id, provider.default_model
            )),
        }
    }
    if endpoints.is_empty() {
        bail!(
            "no active provider/model endpoint is configured:\n- {}",
            errors.join("\n- ")
        )
    }
    Ok(endpoints)
}

pub(in crate::llm::openai_compatible) fn stream_chunk_commits_attempt(
    chunk: &ChatStreamChunk,
    reasoning_visibility: ReasoningVisibility,
) -> bool {
    (chunk.kind == ChatStreamKind::ReasoningPartEnd
        && reasoning_visibility != ReasoningVisibility::Hidden)
        || chunk.kind == ChatStreamKind::ToolCall
        || chunk.kind == ChatStreamKind::RemoteToolStarted
        || chunk.kind == ChatStreamKind::RemoteToolFinished
        || (chunk.kind == ChatStreamKind::Content && !chunk.text.is_empty())
        || (reasoning_visibility == ReasoningVisibility::Full
            && chunk.kind == ChatStreamKind::Reasoning
            && !chunk.text.is_empty())
}

#[cfg(test)]
mod cooldown_tests {
    use super::*;

    /// 连败指数退避:120s 档 120→240→480…封顶 30 分钟;600s 档封顶 1 小时;
    /// 一次成功清零回基础档。退回 escalated_cooldown 前首断言(第 2 败
    /// 240s)报红为 120s。
    #[test]
    fn consecutive_failures_escalate_and_success_resets() {
        let mut scheduler = LlmScheduler::default();
        let base = Duration::from_secs(120);
        assert_eq!(
            scheduler.mark_failure("e".into(), base),
            Duration::from_secs(120)
        );
        assert_eq!(
            scheduler.mark_failure("e".into(), base),
            Duration::from_secs(240)
        );
        assert_eq!(
            scheduler.mark_failure("e".into(), base),
            Duration::from_secs(480)
        );
        for _ in 0..10 {
            scheduler.mark_failure("e".into(), base);
        }
        assert_eq!(
            scheduler.mark_failure("e".into(), base),
            Duration::from_secs(30 * 60)
        );
        scheduler.mark_success("e");
        assert_eq!(
            scheduler.mark_failure("e".into(), base),
            Duration::from_secs(120)
        );

        let quota = Duration::from_secs(600);
        for _ in 0..10 {
            scheduler.mark_failure("q".into(), quota);
        }
        assert_eq!(
            scheduler.mark_failure("q".into(), quota),
            Duration::from_secs(60 * 60)
        );
        // 不同端点互不影响。
        assert_eq!(
            scheduler.mark_failure("f".into(), base),
            Duration::from_secs(120)
        );
    }

    /// 冷却到期的探测(is_ready 清掉过期项)不重置连败——只有成功才算数。
    #[test]
    fn probe_after_expiry_keeps_the_streak() {
        let mut scheduler = LlmScheduler::default();
        scheduler.mark_failure("e".into(), Duration::from_secs(120));
        // 直接模拟到期:清掉冷却但不动 streak(is_ready 的行为)。
        scheduler.cooldowns.clear();
        assert!(scheduler.is_ready("e"));
        assert_eq!(
            scheduler.mark_failure("e".into(), Duration::from_secs(120)),
            Duration::from_secs(240)
        );
    }

    /// 活跃模型全都解析不动时,报错必须说清是**哪一条、为什么**。
    ///
    /// 08-28 实录:会话覆盖指向已从供应商移除的模型,选项被静默筛掉,报错尾巴
    /// 是空的——"no active provider/model endpoint is configured:\n- "。
    #[test]
    fn stale_active_models_explain_themselves() {
        let mut config = crate::config::AppConfig::default();
        let provider_id = config.providers[0].id.clone();
        config.providers[0].models = vec!["kept".to_string()];
        config.providers[0].default_model = "kept".to_string();
        config.active_provider_models = Some(vec![
            crate::config::ActiveProviderModelConfig {
                provider_id: provider_id.clone(),
                model: "removed-model".to_string(),
            },
            crate::config::ActiveProviderModelConfig {
                provider_id: "no-such-provider".to_string(),
                model: "whatever".to_string(),
            },
        ]);

        assert!(config.active_provider_model_choices().is_empty());
        let reasons = stale_active_model_reasons(&config);
        assert_eq!(reasons.len(), 2, "两条都要有说法:{reasons:?}");
        assert!(
            reasons[0].contains("removed-model"),
            "要点名模型:{reasons:?}"
        );
        assert!(
            reasons[1].contains("no-such-provider"),
            "供应商缺失也要点名:{reasons:?}"
        );
        // 报错尾巴不再是空的。
        assert!(reasons.iter().all(|reason| reason.contains(": ")));
    }
}
