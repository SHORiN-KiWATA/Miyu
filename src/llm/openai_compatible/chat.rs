//! Chat Completions 协议的请求与流处理。
//!
//! `send_with_transport_retry` 是重试的收口：只有传输层失败才在同一端点重试，
//! 其余交给端点调度（见 [`super::endpoints`]）。
//!
//! `cache_keepalive` 定期发一个极小的请求，把供应商的前缀缓存续上——缓存有 TTL，
//! 让它过期等于下一轮从 token 0 开始重算。

use crate::llm::openai_compatible::*;

impl OpenAiCompatibleClient {
    pub async fn chat_stream<F>(
        &self,
        messages: Vec<ChatMessage>,
        tools: Vec<ToolDefinition>,
        mut on_chunk: F,
    ) -> Result<ChatResult>
    where
        F: FnMut(ChatStreamChunk) -> Result<()>,
    {
        self.chat_stream_inner(messages, tools, None, false, &mut on_chunk)
            .await
    }

    pub(crate) async fn chat_stream_with_continuation<F>(
        &self,
        messages: Vec<ChatMessage>,
        tools: Vec<ToolDefinition>,
        continuation: Option<&ResponsesContinuation>,
        mut on_chunk: F,
    ) -> Result<ChatResult>
    where
        F: FnMut(ChatStreamChunk) -> Result<()>,
    {
        self.chat_stream_inner(messages, tools, continuation, false, &mut on_chunk)
            .await
    }

    /// Runs an internal completion without exposing partial output. Since no
    /// chunk is committed to a user, a failed endpoint can be safely replaced
    /// even after it emitted an incomplete response.
    pub async fn chat_buffered(
        &self,
        messages: Vec<ChatMessage>,
        tools: Vec<ToolDefinition>,
    ) -> Result<ChatResult> {
        self.chat_stream_inner(messages, tools, None, true, &mut |_| Ok(()))
            .await
    }

    /// Cache keepalive ping (v7 DeepSeek 高命中策略): re-sends the exact
    /// prompt prefix of the last live request as a non-streaming
    /// max_tokens=1 completion so best-effort provider caches keep the deep
    /// prefix alive between user turns. The messages/tools serialization goes
    /// through the same path as live chat, so the server-rendered prompt is
    /// byte-identical (measured: extra body params like max_tokens do not
    /// affect the provider prefix cache key). Returns the reported usage, or
    /// None when the selected endpoint speaks a protocol where the ping does
    /// not apply (Anthropic / OpenAI Responses).
    pub async fn cache_keepalive(
        &self,
        messages: Vec<ChatMessage>,
        tools: Vec<ToolDefinition>,
        endpoint_hint: Option<&(String, String)>,
    ) -> Result<Option<Usage>> {
        let endpoints = self.endpoints.as_ref();
        // 钉住上一条真实请求的 endpoint:缓存按 (供应商, 前缀) 存活,
        // 轮转选出的"下一家"没有这份前缀,ping 过去只是白买 miss。
        let hinted = endpoint_hint.and_then(|(provider, model)| {
            endpoints.iter().position(|endpoint| {
                endpoint.provider.id == *provider && endpoint.provider.default_model == *model
            })
        });
        let index = hinted.unwrap_or_else(|| {
            ordered_endpoint_indices(endpoints)
                .first()
                .copied()
                .unwrap_or(0)
        });
        let endpoint = endpoints
            .get(index)
            .context("no LLM endpoint configured for cache keepalive")?;
        let client = self.with_endpoint(endpoint);
        if client.uses_openai_responses()
            || client.uses_anthropic_messages()
            || provider_uses_cli_relay(&client.provider)
        {
            return Ok(None);
        }
        client.cache_keepalive_single(messages, tools).await
    }

    pub(crate) async fn cache_keepalive_single(
        &self,
        messages: Vec<ChatMessage>,
        tools: Vec<ToolDefinition>,
    ) -> Result<Option<Usage>> {
        let request_id = gen_llm_request_id();
        let extra_body = merge_extra_body(
            sanitize_extra_body(self.provider.extra_body.clone(), CHAT_RESERVED_BODY_KEYS),
            self.chat_variant_extra_body(),
        );
        let messages = prepare_chat_messages_for_provider(&self.provider, messages);
        let request = ChatRequest {
            model: self.provider.default_model.clone(),
            messages,
            temperature: self.provider.effective_temperature(),
            stream: false,
            stream_options: None,
            max_tokens: Some(1),
            tools: (!tools.is_empty()).then_some(tools),
            chat_template_kwargs: taotoken_glm_chat_template_kwargs(&self.provider),
            extra_body,
        };
        let url = format!(
            "{}/chat/completions",
            self.provider.base_url.trim_end_matches('/')
        );
        let response = self
            .send_chat_completion_request(&url, &request, &request_id, "chat.cache_keepalive")
            .await?;
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        if !status.is_success() {
            bail!("cache keepalive ping failed with HTTP {status}: {body}");
        }
        let value: serde_json::Value = serde_json::from_str(&body)
            .with_context(|| "cache keepalive response was not valid JSON")?;
        let usage = value
            .get("usage")
            .cloned()
            .and_then(|usage| serde_json::from_value::<Usage>(usage).ok())
            .map(|mut usage| {
                usage.normalize_cache_fields();
                usage
            });
        // 保温 ping 也进 cache-usage 记账:不然命中率诊断里多出一段
        // "看不见的流量"(deepseek 报告 P2 的观测盲区)。
        crate::llm::cache_log::record(
            "keepalive",
            &self.provider.id,
            &self.provider.default_model,
            0,
            &request_id,
            usage.as_ref(),
        );
        Ok(usage)
    }

    pub(crate) async fn chat_stream_inner<F>(
        &self,
        messages: Vec<ChatMessage>,
        tools: Vec<ToolDefinition>,
        continuation: Option<&ResponsesContinuation>,
        buffered: bool,
        on_chunk: &mut F,
    ) -> Result<ChatResult>
    where
        F: FnMut(ChatStreamChunk) -> Result<()>,
    {
        let request_id = gen_llm_request_id();
        let endpoints = self.endpoints.as_ref();
        let mut errors = Vec::new();
        let mut order = if let Some(continuation) = continuation {
            let index = endpoints
                .iter()
                .position(|endpoint| endpoint.id() == continuation.endpoint_id)
                .with_context(|| {
                    format!(
                        "Responses continuation endpoint is no longer available: {}",
                        continuation.endpoint_id
                    )
                })?;
            vec![index]
        } else {
            ordered_endpoint_indices(endpoints)
        };
        // Every endpoint is cooling down. Refusing outright would strand a
        // single-endpoint user for the whole cooldown, so one probe still goes
        // out — but exactly one, and to whichever endpoint recovers first.
        // Refilling the pool here (and then padding it below) meant a rate
        // limit cost three requests per turn *for the entire cooldown*, which
        // made the cooldown worse than useless.
        let probe_only = order.is_empty();
        if probe_only {
            tracing::warn!(
                request_id,
                endpoint_count = endpoints.len(),
                all_endpoints_cooling_down = true,
                "{}",
                t(
                    "All LLM endpoints are cooling down; sending a single probe",
                    "所有 LLM 端点均在冷却；仅发送一次探测请求"
                )
            );
            order = soonest_ready_endpoint_index(endpoints)
                .into_iter()
                .collect();
        }
        // A dropped stream or a 5xx is a moment in time, not a verdict on the
        // endpoint. Tying the number of attempts to the number of configured
        // endpoints meant someone with a single model got no retry at all,
        // which is backwards: they are the ones with nowhere else to go. Pad
        // the attempt list by cycling so every setup gets the same budget.
        // Errors that a retry cannot fix still stop on the first attempt —
        // `endpoint_failover_allowed` returns before the next one is tried,
        // and `same_endpoint_retry_allowed` skips the padded repeats of an
        // endpoint that answered 429/401.
        if !probe_only && !order.is_empty() && order.len() < MIN_ENDPOINT_ATTEMPTS {
            let cycle: Vec<usize> = order.clone();
            while order.len() < MIN_ENDPOINT_ATTEMPTS {
                order.extend(cycle.iter().copied());
            }
            order.truncate(MIN_ENDPOINT_ATTEMPTS);
        }
        tracing::debug!(
            request_id,
            endpoint_count = order.len(),
            message_count = messages.len(),
            tool_count = tools.len(),
            continued = continuation.is_some(),
            "{}",
            t("LLM request started", "LLM 请求已开始")
        );
        let mut exhausted: Vec<String> = Vec::new();
        let mut previous_endpoint: Option<String> = None;
        for (attempt, index) in order.into_iter().enumerate() {
            let endpoint = &endpoints[index];
            if exhausted.contains(&endpoint.id()) {
                continue;
            }
            // 同端点的填充重试稍作退避:5xx/断流是瞬时故障,零间隔连打同
            // 一端点多半撞上同一故障;切到不同端点仍零间隔(failover 不变)。
            if previous_endpoint.as_deref() == Some(endpoint.id().as_str()) {
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
            previous_endpoint = Some(endpoint.id());
            let client = self.with_endpoint(endpoint);
            if attempt > 0 {
                on_chunk(ChatStreamChunk {
                    kind: ChatStreamKind::ReasoningReset,
                    text: String::new(),
                })?;
            }
            let started = Instant::now();
            tracing::debug!(
                request_id,
                attempt = attempt + 1,
                provider = %endpoint.provider.id,
                model = %endpoint.provider.default_model,
                key_index = endpoint.key_index + 1,
                "{}",
                t("LLM endpoint attempt started", "LLM 端点尝试已开始")
            );
            let mut attempt_committed = false;
            let result = {
                let buffered = buffered || self.buffered_delivery;
                let mut attempt_on_chunk = |chunk: ChatStreamChunk| {
                    if !buffered {
                        attempt_committed |=
                            stream_chunk_commits_attempt(&chunk, client.reasoning_visibility);
                    }
                    on_chunk(chunk)
                };
                client
                    .chat_stream_single(
                        messages.clone(),
                        tools.clone(),
                        continuation.map(|continuation| continuation.response_id.as_str()),
                        &request_id,
                        &mut attempt_on_chunk,
                    )
                    .await
            };
            match result {
                Ok(mut result) => {
                    result.provider_id = Some(endpoint.provider.id.clone());
                    result.model = Some(endpoint.provider.default_model.clone());
                    if let Some(next) = result.responses_continuation.as_mut() {
                        next.endpoint_id = endpoint.id();
                    }
                    mark_endpoint_success(endpoint);
                    crate::llm::cache_log::record(
                        self.request_scope,
                        &endpoint.provider.id,
                        &endpoint.provider.default_model,
                        endpoint.key_index,
                        &request_id,
                        result.usage.as_ref(),
                    );
                    tracing::debug!(
                        request_id,
                        attempt = attempt + 1,
                        provider = %endpoint.provider.id,
                        model = %endpoint.provider.default_model,
                        elapsed_ms = started.elapsed().as_millis(),
                        "{}",
                        t("LLM endpoint succeeded", "LLM 端点请求成功")
                    );
                    return Ok(result);
                }
                Err(err) => {
                    let cooldown = mark_endpoint_failure(endpoint, &err);
                    let endpoint_cooling_down = cooldown.is_some();
                    let cooldown_seconds = cooldown.map(|duration| duration.as_secs()).unwrap_or(0);
                    if let Some(failure) = err.downcast_ref::<TransportFailure>() {
                        tracing::error!(
                            request_id,
                            attempt = attempt + 1,
                            provider = %endpoint.provider.id,
                            model = %endpoint.provider.default_model,
                            stage = failure.stage,
                            transport_kind = %failure.kind,
                            endpoint_cooling_down,
                            cooldown_seconds,
                            elapsed_ms = started.elapsed().as_millis(),
                            error = %format!("{err:#}"),
                            "{}",
                            t("LLM endpoint transport failure", "LLM 端点传输失败")
                        );
                    } else if let Some(failure) = err.downcast_ref::<HttpStatusFailure>() {
                        tracing::error!(
                            request_id,
                            attempt = attempt + 1,
                            provider = %endpoint.provider.id,
                            model = %endpoint.provider.default_model,
                            status = failure.status,
                            failure_kind = %failure.kind,
                            endpoint_cooling_down,
                            cooldown_seconds,
                            elapsed_ms = started.elapsed().as_millis(),
                            "{}",
                            t("LLM endpoint HTTP failure", "LLM 端点 HTTP 请求失败")
                        );
                    } else {
                        tracing::error!(
                            request_id,
                            attempt = attempt + 1,
                            provider = %endpoint.provider.id,
                            model = %endpoint.provider.default_model,
                            endpoint_cooling_down,
                            cooldown_seconds,
                            elapsed_ms = started.elapsed().as_millis(),
                            error = %format!("{err:#}"),
                            "{}",
                            t(
                                "LLM endpoint failed outside the HTTP send stage",
                                "LLM 端点在 HTTP 发送阶段之外失败"
                            )
                        );
                    }
                    let message = format!("{err:#}");
                    errors.push(format!(
                        "{} / {} key#{}: {message}",
                        endpoint.provider.id,
                        endpoint.provider.default_model,
                        endpoint.key_index + 1
                    ));
                    if !same_endpoint_retry_allowed(&err) {
                        exhausted.push(endpoint.id());
                    }
                    if attempt_committed {
                        return Err(err.context(
                            "LLM stream failed after emitting output; endpoint failover was suppressed",
                        ));
                    }
                    if !endpoint_failover_allowed(&err) {
                        return Err(err.context(
                            "LLM request was rejected; endpoint failover was suppressed",
                        ));
                    }
                }
            }
        }
        bail!(
            "no LLM provider/model endpoint succeeded (request {request_id}):\n- {}",
            errors.join("\n- ")
        )
    }

    pub(crate) async fn chat_stream_single<F>(
        &self,
        messages: Vec<ChatMessage>,
        tools: Vec<ToolDefinition>,
        previous_response_id: Option<&str>,
        request_id: &str,
        on_chunk: &mut F,
    ) -> Result<ChatResult>
    where
        F: FnMut(ChatStreamChunk) -> Result<()>,
    {
        let protocol = ProviderProtocol::from_provider(&self.provider)?;
        // CLI 中转线的 future 装箱:三条线的状态机(子进程泵/事件解析)都很
        // 大,内联进本函数的 future 会把 debug 构建下 2MB 的线程栈撑爆
        // (agent 层测试实录),装箱后外层只剩一个指针。
        if protocol == ProviderProtocol::ClaudeCode {
            return Box::pin(self.chat_claude_code_stream(messages, tools, request_id, on_chunk))
                .await;
        }
        if protocol == ProviderProtocol::Antigravity {
            return Box::pin(self.chat_antigravity_stream(messages, tools, request_id, on_chunk))
                .await;
        }
        if protocol == ProviderProtocol::Codex {
            return Box::pin(self.chat_codex_stream(messages, tools, request_id, on_chunk)).await;
        }
        let uses_responses = protocol == ProviderProtocol::OpenAiResponses
            || (protocol == ProviderProtocol::Auto && self.uses_openai_responses());
        if previous_response_id.is_some() && !uses_responses {
            bail!("Responses continuation endpoint no longer uses the Responses protocol");
        }
        if protocol == ProviderProtocol::Anthropic
            || (protocol == ProviderProtocol::Auto && self.uses_anthropic_messages())
        {
            return self
                .chat_anthropic_stream(messages, tools, request_id, on_chunk)
                .await;
        }
        if uses_responses {
            if let Some(result) = self
                .chat_responses_stream(
                    messages.clone(),
                    tools.clone(),
                    previous_response_id,
                    request_id,
                    on_chunk,
                )
                .await?
            {
                return Ok(result);
            }
            if previous_response_id.is_some() {
                bail!("OpenAI Responses continuation is not supported by this provider");
            }
            if protocol == ProviderProtocol::OpenAiResponses {
                bail!("OpenAI Responses protocol is not supported by this provider");
            }
            if let Some((info, variant)) = self.selected_reasoning_variant() {
                if !reasoning_variant_supported_for_protocol(
                    &self.provider,
                    &info,
                    &variant,
                    ProviderProtocol::OpenAiChat,
                ) {
                    bail!(
                        "thinking variant '{}' cannot be applied after falling back from OpenAI Responses to Chat Completions",
                        variant.id
                    );
                }
            }
        }
        let extra_body = merge_extra_body(
            sanitize_extra_body(self.provider.extra_body.clone(), CHAT_RESERVED_BODY_KEYS),
            self.chat_variant_extra_body(),
        );
        let messages = prepare_chat_messages_for_provider(&self.provider, messages);
        let mut request = ChatRequest {
            model: self.provider.default_model.clone(),
            messages,
            temperature: self.provider.effective_temperature(),
            stream: true,
            stream_options: Some(ChatStreamOptions {
                include_usage: true,
            }),
            max_tokens: self.max_tokens_override,
            tools: (!tools.is_empty()).then_some(tools),
            chat_template_kwargs: taotoken_glm_chat_template_kwargs(&self.provider),
            extra_body,
        };
        let url = format!(
            "{}/chat/completions",
            self.provider.base_url.trim_end_matches('/')
        );
        let mut response = self
            .send_chat_completion_request(&url, &request, request_id, "chat.send")
            .await?;
        let mut status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            if non_stream_quota_fallback_candidate(status.as_u16(), &body) {
                let mut retry = request.clone();
                retry.stream = false;
                retry.stream_options = None;
                let response = self
                    .send_chat_completion_request(
                        &url,
                        &retry,
                        request_id,
                        "chat.retry_without_streaming",
                    )
                    .await?;
                let retry_status = response.status();
                if retry_status.is_success() {
                    tracing::info!(
                        request_id,
                        provider = %self.provider.id,
                        model = %self.provider.default_model,
                        "{}",
                        t(
                            "streaming quota was unavailable; non-streaming compatibility retry succeeded",
                            "流式配额不可用；非流式兼容重试成功"
                        )
                    );
                    return self
                        .consume_chat_completion_response(response, on_chunk)
                        .await;
                }
                let retry_body = response.text().await.unwrap_or_default();
                tracing::debug!(
                    request_id,
                    status = retry_status.as_u16(),
                    "{}",
                    t(
                        "non-streaming quota compatibility retry returned an HTTP error",
                        "非流式配额兼容重试返回 HTTP 错误"
                    )
                );
                return self.bail_chat_completion_failure(retry_status.as_u16(), &retry_body);
            }
            if stream_options_unsupported(status.as_u16(), &body) {
                request.stream_options = None;
                response = self
                    .send_chat_completion_request(
                        &url,
                        &request,
                        request_id,
                        "chat.retry_without_stream_options",
                    )
                    .await?;
                status = response.status();
                if status.is_success() {
                    return self
                        .consume_chat_completion_stream(response, on_chunk)
                        .await;
                }
                let body = response.text().await.unwrap_or_default();
                if let Some(result) = self
                    .try_zen_chat_completion_compat_retry(
                        &url,
                        &request,
                        status.as_u16(),
                        &body,
                        request_id,
                        on_chunk,
                    )
                    .await?
                {
                    return Ok(result);
                }
                return self.bail_chat_completion_failure(status.as_u16(), &body);
            }
            if let Some(result) = self
                .try_zen_chat_completion_compat_retry(
                    &url,
                    &request,
                    status.as_u16(),
                    &body,
                    request_id,
                    on_chunk,
                )
                .await?
            {
                return Ok(result);
            }
            return self.bail_chat_completion_failure(status.as_u16(), &body);
        }

        self.consume_chat_completion_stream(response, on_chunk)
            .await
    }
}
