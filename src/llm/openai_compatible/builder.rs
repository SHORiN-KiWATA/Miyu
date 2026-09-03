//! 客户端的构造与逐次调用的选项。
//!
//! `with_*` 全部返回改过的自身：一次调用的选项（端点、超时、上限、缓冲投递）
//! 是**每次不同**的，而客户端本身要能复用连接池。所以选项走建造式，客户端本体
//! 保持共享。

use crate::llm::openai_compatible::*;

impl OpenAiCompatibleClient {
    /// 当前主 provider id,用量历史记账用(具体模型以 ChatResult 为准)。
    pub fn provider_id(&self) -> &str {
        &self.provider.id
    }

    /// 该端点的 Responses 续传是否已被记为不可用(记录或本进程自愈置位)。
    pub(crate) fn responses_continuation_suppressed(&self) -> bool {
        self.continuation_health
            .unsupported
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// 自愈标记:上游拒了 previous_response_id(任务#16 签名 400)。进程内
    /// 立即生效并持久化;首个标记者负责落盘,后续幂等。
    pub fn mark_responses_continuation_unsupported(&self) {
        let health = &self.continuation_health;
        if !health
            .unsupported
            .swap(true, std::sync::atomic::Ordering::Relaxed)
        {
            if !health.base_url.is_empty() {
                crate::llm::provider_capabilities::record_continuation_unsupported(
                    &health.store,
                    &health.base_url,
                );
            }
            tracing::warn!(
                provider = %health.provider_id,
                "responses continuation rejected upstream; falling back to stateless full replay for this provider"
            );
        }
    }

    pub fn from_config(config: &AppConfig, paths: &MiyuPaths) -> Result<Self> {
        crate::llm::cache_log::configure(paths, &config.cache);
        let endpoints = llm_endpoints(config, paths)?;
        let first = endpoints
            .first()
            .with_context(|| "no active provider/model endpoint is configured")?;
        let continuation_health = ResponsesContinuationHealth::for_provider(paths, &first.provider);
        let claude_code = claude_code_runtime(&endpoints, config);
        let antigravity = antigravity_runtime(&endpoints, config);
        let codex = codex_runtime(&endpoints, config);
        let mut client = Self {
            client: first.client.clone(),
            provider: first.provider.clone(),
            api_key: first.api_key.clone(),
            endpoints: Arc::new(endpoints),
            thinking_variants: HashMap::new(),
            reasoning_visibility: reasoning_visibility(config),
            buffered_delivery: false,
            detailed_reasoning_summary: reasoning_summary_is_detailed(config),
            request_timeouts: None,
            max_tokens_override: None,
            request_scope: "chat",
            continuation_health,
            claude_code,
            antigravity,
            codex,
            claude_code_dev_mode: false,
        };
        client.restore_saved_thinking_variants(paths);
        Ok(client)
    }

    /// Builds a client over an explicit provider/model pool (e.g. a
    /// subagent tier pool). Requests load-balance across the pool through
    /// the shared endpoint scheduler, exactly like the main model pool.
    pub fn from_choices(
        config: &AppConfig,
        paths: &MiyuPaths,
        choices: &[crate::config::ProviderModelChoice],
    ) -> Result<Self> {
        crate::llm::cache_log::configure(paths, &config.cache);
        let mut endpoints = Vec::new();
        let mut errors = Vec::new();
        for choice in choices {
            let mut provider = match config.provider(Some(&choice.provider_id)) {
                Ok(provider) => provider.clone(),
                Err(err) => {
                    errors.push(format!("{} / {}: {err}", choice.provider_id, choice.model));
                    continue;
                }
            };
            if !provider.enabled {
                errors.push(format!(
                    "{} / {}: {}",
                    provider.id,
                    choice.model,
                    t(
                        "provider is disabled; enable it in the provider settings",
                        "供应商未启用;请在供应商设置里启用"
                    )
                ));
                continue;
            }
            provider.default_model = choice.model.clone();
            let client = endpoint_client(&provider)?;
            if provider_uses_cli_relay(&provider) {
                // CLI 用订阅登录态,没有 API key;单端点直进池。
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
        let first = match endpoints.first() {
            Some(first) => first,
            None => bail!(
                "no usable endpoint in the model pool:\n- {}",
                errors.join("\n- ")
            ),
        };
        let continuation_health = ResponsesContinuationHealth::for_provider(paths, &first.provider);
        let claude_code = claude_code_runtime(&endpoints, config);
        let antigravity = antigravity_runtime(&endpoints, config);
        let codex = codex_runtime(&endpoints, config);
        let mut client = Self {
            client: first.client.clone(),
            provider: first.provider.clone(),
            api_key: first.api_key.clone(),
            endpoints: Arc::new(endpoints),
            thinking_variants: HashMap::new(),
            reasoning_visibility: reasoning_visibility(config),
            buffered_delivery: false,
            detailed_reasoning_summary: reasoning_summary_is_detailed(config),
            request_timeouts: None,
            max_tokens_override: None,
            request_scope: "chat",
            continuation_health,
            claude_code,
            antigravity,
            codex,
            claude_code_dev_mode: false,
        };
        client.restore_saved_thinking_variants(paths);
        Ok(client)
    }

    pub fn new(provider: &ProviderConfig, config: &AppConfig, paths: &MiyuPaths) -> Result<Self> {
        if !provider.enabled {
            bail!(
                "{}: {}",
                t(
                    "provider is disabled; enable it in the provider settings",
                    "供应商未启用;请在供应商设置里启用"
                ),
                provider.id
            );
        }
        if provider.default_model.trim().is_empty() {
            bail!(
                "{}: {}",
                t(
                    "provider has no active model; select a model before chatting",
                    "provider 没有当前模型；请先选择模型再聊天",
                ),
                provider.id
            );
        }
        let client = endpoint_client(provider)?;
        let (key_value, key_index) = if provider_uses_cli_relay(provider) {
            (String::new(), 0)
        } else {
            let key = provider
                .resolved_api_keys(paths)?
                .into_iter()
                .next()
                .with_context(|| format!("missing API key for provider {}", provider.id))?;
            (key.value, key.index)
        };
        let endpoint = LlmEndpoint {
            client: client.clone(),
            provider: provider.clone(),
            api_key: key_value.clone(),
            key_index,
        };
        let continuation_health = ResponsesContinuationHealth::for_provider(paths, provider);
        let endpoints = vec![endpoint];
        let claude_code = claude_code_runtime(&endpoints, config);
        let antigravity = antigravity_runtime(&endpoints, config);
        let codex = codex_runtime(&endpoints, config);
        let mut client = Self {
            client,
            provider: provider.clone(),
            api_key: key_value,
            endpoints: Arc::new(endpoints),
            thinking_variants: HashMap::new(),
            reasoning_visibility: reasoning_visibility(config),
            buffered_delivery: false,
            detailed_reasoning_summary: reasoning_summary_is_detailed(config),
            request_timeouts: None,
            max_tokens_override: None,
            request_scope: "chat",
            continuation_health,
            claude_code,
            antigravity,
            codex,
            claude_code_dev_mode: false,
        };
        client.restore_saved_thinking_variants(paths);
        Ok(client)
    }

    pub fn context_window(&self, config: &AppConfig) -> Result<Option<usize>> {
        Ok(self
            .context_window_with_source(config)?
            .map(|(window, _)| window))
    }

    /// 同上，外带这个数是哪来的。
    ///
    /// 端点池里只要有一个模型的窗口是猜的，整池就算猜的——取的是最小值，而那个
    /// 「猜的」成员真实窗口要是更小，最小值本身就是错的。
    pub fn context_window_with_source(
        &self,
        config: &AppConfig,
    ) -> Result<Option<(usize, crate::config::ContextWindowSource)>> {
        use crate::config::ContextWindowSource;
        let choices = self.endpoint_model_choices();
        let mut windows = Vec::with_capacity(choices.len());
        let mut assumed = false;
        for (provider_id, model) in choices {
            let Some((window, source)) = config.context_window_with_source(&provider_id, &model)?
            else {
                return Ok(None);
            };
            assumed |= source == ContextWindowSource::Assumed;
            windows.push(window);
        }
        let source = if assumed {
            ContextWindowSource::Assumed
        } else {
            ContextWindowSource::Known
        };
        Ok(windows.into_iter().min().map(|window| (window, source)))
    }

    /// Marks a client whose caller collects output and delivers it in one
    /// piece. A truncated stream can then be retried without the person
    /// seeing the false start.
    pub fn with_buffered_delivery(mut self, buffered: bool) -> Self {
        self.buffered_delivery = buffered;
        self
    }

    pub fn for_subagent_output(mut self, full: bool) -> Self {
        self.reasoning_visibility = if full {
            ReasoningVisibility::Full
        } else {
            ReasoningVisibility::Hidden
        };
        self.detailed_reasoning_summary = full;
        self
    }

    pub fn with_request_timeouts(
        mut self,
        response_header: Duration,
        stream_idle: Duration,
    ) -> Self {
        self.request_timeouts = Some(RequestTimeouts {
            response_header: response_header.max(Duration::from_millis(1)),
            stream_idle: stream_idle.max(Duration::from_millis(1)),
        });
        self
    }

    pub fn models_without_context_window(&self, config: &AppConfig) -> Vec<String> {
        self.endpoint_model_choices()
            .into_iter()
            .filter(|(provider_id, model)| {
                config
                    .context_window_for_provider_model(provider_id, model)
                    .ok()
                    .flatten()
                    .is_none()
            })
            .map(|(provider_id, model)| format!("{provider_id} / {model}"))
            .collect()
    }

    /// 这个客户端一共能试几个端点。
    ///
    /// 调用方要用它算「总预算」：把一个固定的总超时罩在故障转移外面，端点一多
    /// 就会在中途把还没试的砍掉（08-18 实测：视觉 60s 预算 / 单端点 15s 响应头
    /// 超时 = 最多容得下 4 个卡住的端点，第 5 个之后的永远轮不到）。
    pub(crate) fn endpoint_count(&self) -> usize {
        self.endpoints.len()
    }

    pub(crate) fn endpoint_model_choices(&self) -> BTreeSet<(String, String)> {
        self.endpoints
            .iter()
            .map(|endpoint| {
                (
                    endpoint.provider.id.clone(),
                    endpoint.provider.default_model.clone(),
                )
            })
            .collect()
    }

    pub(in crate::llm::openai_compatible) fn with_endpoint(&self, endpoint: &LlmEndpoint) -> Self {
        Self {
            client: endpoint.client.clone(),
            provider: endpoint.provider.clone(),
            api_key: endpoint.api_key.clone(),
            endpoints: self.endpoints.clone(),
            thinking_variants: self.thinking_variants.clone(),
            reasoning_visibility: self.reasoning_visibility,
            buffered_delivery: self.buffered_delivery,
            detailed_reasoning_summary: self.detailed_reasoning_summary,
            request_timeouts: self.request_timeouts,
            max_tokens_override: self.max_tokens_override,
            request_scope: self.request_scope,
            // failover 换端点共享同一健康位(续传本就钉在原端点)。
            continuation_health: self.continuation_health.clone(),
            claude_code: self.claude_code.clone(),
            antigravity: self.antigravity.clone(),
            codex: self.codex.clone(),
            claude_code_dev_mode: self.claude_code_dev_mode,
        }
    }

    /// Agent 构造时声明会话模式:claude-code 的原生工具/Miyu 工具双作用域
    /// (off/dev/normal/all)按它裁决。其他协议不受影响。
    pub fn with_claude_code_dev_mode(mut self, dev: bool) -> Self {
        self.claude_code_dev_mode = dev;
        self
    }

    /// Returns a clone whose chat completions are capped at `max_tokens`.
    pub fn with_request_scope(mut self, scope: &'static str) -> Self {
        self.request_scope = scope;
        self
    }

    pub fn with_max_tokens(&self, max_tokens: u32) -> Self {
        let mut clone = self.clone();
        clone.max_tokens_override = Some(max_tokens.max(1));
        clone
    }

    pub(crate) fn uses_openai_responses(&self) -> bool {
        let model = self.provider.default_model.to_ascii_lowercase();
        model.starts_with("gpt-5")
            || model.starts_with("o1")
            || model.starts_with("o3")
            || model.starts_with("o4")
    }

    pub(crate) fn uses_anthropic_messages(&self) -> bool {
        provider_looks_anthropic(&self.provider)
    }
}

/// 端点池里出现 codex 协议端点时,解析一份共享运行时参数。
pub(in crate::llm::openai_compatible) fn codex_runtime(
    endpoints: &[LlmEndpoint],
    config: &AppConfig,
) -> Option<Arc<CodexRuntime>> {
    endpoints
        .iter()
        .any(|endpoint| provider_uses_codex(&endpoint.provider))
        .then(|| Arc::new(CodexRuntime::from_config(config)))
}

/// 端点池里出现 antigravity 协议端点时,解析一份共享运行时参数。
pub(in crate::llm::openai_compatible) fn antigravity_runtime(
    endpoints: &[LlmEndpoint],
    config: &AppConfig,
) -> Option<Arc<AntigravityRuntime>> {
    endpoints
        .iter()
        .any(|endpoint| provider_uses_antigravity(&endpoint.provider))
        .then(|| Arc::new(AntigravityRuntime::from_config(config)))
}

/// 端点池里出现 claude-code 协议端点时,解析一份共享运行时参数。
pub(in crate::llm::openai_compatible) fn claude_code_runtime(
    endpoints: &[LlmEndpoint],
    config: &AppConfig,
) -> Option<Arc<ClaudeCodeRuntime>> {
    if !endpoints
        .iter()
        .any(|endpoint| provider_uses_claude_code(&endpoint.provider))
    {
        return None;
    }
    let mut runtime = ClaudeCodeRuntime::from_config(config);
    for endpoint in endpoints {
        if !provider_uses_claude_code(&endpoint.provider) {
            continue;
        }
        let provider_id = &endpoint.provider.id;
        let model = &endpoint.provider.default_model;
        // 与上下文测算同源的有效窗口(显式配置→目录→默认),夹到 CLI 的
        // 100k–1M 后透传给 claude 自压缩。
        let window = config
            .context_window_with_source(provider_id, model)
            .ok()
            .flatten()
            .map(|(window, _)| window as u64)
            .unwrap_or(168_000);
        runtime.autocompact.insert(
            format!("{provider_id}\t{model}"),
            window.clamp(100_000, 1_000_000),
        );
    }
    Some(Arc::new(runtime))
}
