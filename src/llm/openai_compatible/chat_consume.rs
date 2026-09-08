//! Chat Completions 响应的消费。
//!
//! 流式与非流式两条路（`consume_chat_completion_stream` / `_response`）：有些
//! 供应商在配额不足时只接受非流式，`chat` 那边会降级过来。
//!
//! `send_with_transport_retry` 是重试的收口：只有传输层失败才在同一端点重试，
//! 其余交给端点调度。

use crate::llm::openai_compatible::*;

impl OpenAiCompatibleClient {
    pub(in crate::llm::openai_compatible) async fn send_chat_completion_request(
        &self,
        url: &str,
        request: &ChatRequest,
        request_id: &str,
        stage: &'static str,
    ) -> Result<reqwest::Response> {
        crate::llm::request_log::record(
            &self.provider.id,
            &self.provider.default_model,
            "chat",
            self.request_scope,
            url,
            request,
        );
        self.send_with_transport_retry(request_id, stage, || {
            self.client
                .post(url)
                .bearer_auth(&self.api_key)
                .json(request)
        })
        .await
    }

    pub(crate) async fn send_with_transport_retry<F>(
        &self,
        request_id: &str,
        stage: &'static str,
        mut build_request: F,
    ) -> Result<reqwest::Response>
    where
        F: FnMut() -> reqwest::RequestBuilder,
    {
        let mut connect_retry_used = false;
        let mut attempt = 0usize;
        loop {
            attempt = attempt.saturating_add(1);
            let started = Instant::now();
            // Zen 的识别头在这里统一补:chat / responses / anthropic 三条线
            // 的发送都收口在本函数,别处再加就会漏掉其中一条。
            let send = zen_headers::apply(
                build_request(),
                &self.provider,
                self.zen_session.as_deref(),
                request_id,
            )
            .send();
            let response = if let Some(timeouts) = self.request_timeouts {
                match tokio::time::timeout(timeouts.response_header, send).await {
                    Ok(response) => response,
                    Err(_) => {
                        tracing::warn!(
                            request_id,
                            stage,
                            attempt,
                            timeout_seconds = timeouts.response_header.as_secs(),
                            "{}",
                            t("LLM response header timed out", "LLM 响应头等待超时")
                        );
                        return Err(anyhow::anyhow!(
                            "LLM response header timed out after {} seconds",
                            timeouts.response_header.as_secs()
                        )
                        .context(TransportFailure {
                            stage,
                            kind: TransportFailureKind::Timeout,
                        }));
                    }
                }
            } else {
                send.await
            };
            match response {
                Ok(response) => {
                    let status = response.status().as_u16();
                    let retryable_status = retryable_http_status(status);
                    let will_retry = retryable_status && attempt < MAX_SEND_ATTEMPTS;
                    tracing::debug!(
                        request_id,
                        stage,
                        attempt,
                        status,
                        will_retry,
                        elapsed_ms = started.elapsed().as_millis(),
                        "{}",
                        t(
                            "LLM HTTP response headers received",
                            "已收到 LLM HTTP 响应头"
                        )
                    );
                    if will_retry {
                        let delay = http_status_retry_delay(attempt);
                        tracing::warn!(
                            request_id,
                            stage,
                            attempt,
                            status,
                            retry_delay_ms = delay.as_millis(),
                            "{}",
                            t(
                                "LLM HTTP request returned a transient server error",
                                "LLM HTTP 请求返回临时服务器错误"
                            )
                        );
                        let _ = response.bytes().await;
                        tokio::time::sleep(delay).await;
                        continue;
                    }
                    if retryable_status {
                        let body = response.text().await.unwrap_or_default();
                        return Err(anyhow::anyhow!(
                            "LLM HTTP request failed after {attempt} attempts: {body}"
                        )
                        .context(HttpStatusFailure::classify(status, &body)));
                    }
                    return Ok(response);
                }
                Err(error) => {
                    let kind = if error.is_connect() {
                        TransportFailureKind::Connect
                    } else if error.is_timeout() {
                        TransportFailureKind::Timeout
                    } else {
                        TransportFailureKind::Other
                    };
                    let will_retry = attempt < MAX_SEND_ATTEMPTS
                        && !connect_retry_used
                        && retryable_transport_failure(kind);
                    connect_retry_used |= will_retry;
                    let error = error.without_url();
                    tracing::warn!(
                        request_id,
                        stage,
                        attempt,
                        transport_kind = %kind,
                        will_retry,
                        elapsed_ms = started.elapsed().as_millis(),
                        error = %format_error_chain(&error),
                        "{}",
                        t("LLM HTTP transport attempt failed", "LLM HTTP 传输尝试失败")
                    );
                    if will_retry {
                        tokio::time::sleep(TRANSPORT_RETRY_DELAY).await;
                        continue;
                    }
                    return Err(anyhow::Error::new(error).context(TransportFailure { stage, kind }));
                }
            }
        }
    }

    pub(crate) async fn next_response_chunk<S, T>(
        &self,
        stream: &mut S,
        stage: &'static str,
    ) -> Result<Option<T>>
    where
        S: Stream<Item = std::result::Result<T, reqwest::Error>> + Unpin,
    {
        let next = if let Some(timeouts) = self.request_timeouts {
            match tokio::time::timeout(timeouts.stream_idle, stream.next()).await {
                Ok(next) => next,
                Err(_) => {
                    return Err(anyhow::anyhow!(
                        "LLM response stream was idle for {} seconds",
                        timeouts.stream_idle.as_secs()
                    )
                    .context(TransportFailure {
                        stage,
                        kind: TransportFailureKind::Timeout,
                    }));
                }
            }
        } else {
            stream.next().await
        };
        next.transpose().map_err(|error| {
            anyhow::Error::new(error).context(TransportFailure {
                stage,
                kind: TransportFailureKind::Other,
            })
        })
    }

    pub(in crate::llm::openai_compatible) async fn try_zen_chat_completion_compat_retry<F>(
        &self,
        url: &str,
        request: &ChatRequest,
        status: u16,
        body: &str,
        request_id: &str,
        on_chunk: &mut F,
    ) -> Result<Option<ChatResult>>
    where
        F: FnMut(ChatStreamChunk) -> Result<()>,
    {
        if !zen_upstream_failed(&self.provider, status, body) {
            return Ok(None);
        }

        let mut retries = Vec::new();
        if request.stream_options.is_some() {
            let mut retry = request.clone();
            retry.stream_options = None;
            retries.push(retry);
        }
        if request.tools.is_some() {
            let mut retry = request.clone();
            retry.stream_options = None;
            retry.tools = None;
            retries.push(retry);
        }

        for (attempt, retry) in retries.into_iter().enumerate() {
            let response = self
                .send_chat_completion_request(
                    url,
                    &retry,
                    request_id,
                    "chat.zen_compatibility_retry",
                )
                .await?;
            let status = response.status();
            if status.is_success() {
                return self
                    .consume_chat_completion_stream(response, on_chunk)
                    .await
                    .map(Some);
            }
            tracing::debug!(
                request_id,
                attempt = attempt + 1,
                status = status.as_u16(),
                "{}",
                t(
                    "Zen compatibility retry returned an HTTP error",
                    "Zen 兼容重试返回 HTTP 错误"
                )
            );
            let _ = response.text().await;
        }

        Ok(None)
    }

    pub(crate) async fn consume_chat_completion_stream<F>(
        &self,
        response: reqwest::Response,
        on_chunk: &mut F,
    ) -> Result<ChatResult>
    where
        F: FnMut(ChatStreamChunk) -> Result<()>,
    {
        let dsml = dsml_enabled_for(&self.provider);
        let mut buffer = Utf8LineBuffer::default();
        let mut content = String::new();
        let mut content_emitted = 0usize;
        let mut reasoning = String::new();
        let mut reasoning_emitted = 0usize;
        let mut reasoning_part_active = false;
        let mut finish_reason = None;
        let mut usage = None;
        let mut tool_calls = ToolCallAccumulator::default();
        let mut stream = response.bytes_stream();
        while let Some(chunk) = self.next_response_chunk(&mut stream, "chat.stream").await? {
            for line in buffer.push(&chunk)? {
                if let Some(done) = handle_sse_line(
                    &line,
                    &mut content,
                    &mut content_emitted,
                    &mut reasoning,
                    &mut reasoning_emitted,
                    &mut reasoning_part_active,
                    &mut finish_reason,
                    &mut usage,
                    &mut tool_calls,
                    &mut *on_chunk,
                )? {
                    if done {
                        return finalize_stream_result(
                            content,
                            reasoning,
                            usage,
                            tool_calls.finish(),
                            dsml,
                        );
                    }
                }
            }
        }
        for line in buffer.finish()? {
            let _ = handle_sse_line(
                &line,
                &mut content,
                &mut content_emitted,
                &mut reasoning,
                &mut reasoning_emitted,
                &mut reasoning_part_active,
                &mut finish_reason,
                &mut usage,
                &mut tool_calls,
                &mut *on_chunk,
            )?;
        }
        // Reaching here means the socket closed without `[DONE]` — the loop
        // above returns early on that marker. A provider that ends this way
        // still has to have said it was finished somewhere, and two places
        // count as saying it.
        //
        // `finish_reason` is the obvious one (llama.cpp's Responses endpoint,
        // for one, never sends `[DONE]`). A usage frame is the other: gateways
        // only know the token counts once generation is over, so they append
        // that frame at the end and nowhere else. A stream cut mid-generation
        // cannot carry one. 08-19 实测 opencode zen 的
        // muse-spark-1.2-contributor 就是这么收尾的:全程 `finish_reason:
        // null`,末尾一个 usage 帧然后直接断开,没有 `[DONE]`——同网关的
        // deepseek-v4-flash / mimo-v2.5 都照发 `[DONE]`,所以这不是端点坏了,
        // 是这一条链路的收尾方言。
        //
        // With neither signal the response is a truncated fragment, and
        // returning it as a completed turn is how an empty reply reaches the
        // user with nothing logged.
        //
        // Reported as a transport failure so the existing machinery retries it
        // across endpoints and resets the partial reasoning already streamed.
        // Retrying is safe here: tool calls execute after this returns, so a
        // truncated turn has run nothing yet.
        if finish_reason.is_none() && usage.is_none() {
            return Err(anyhow::anyhow!(t(
                "the response stream ended before the model said it was done",
                "模型还没说完，响应流就提前结束了"
            ))
            .context(TransportFailure {
                stage: "chat.stream",
                kind: TransportFailureKind::Other,
            }));
        }
        flush_buffer(
            &reasoning,
            &mut reasoning_emitted,
            ChatStreamKind::Reasoning,
            &mut *on_chunk,
            true,
        )?;
        flush_buffer(
            &content,
            &mut content_emitted,
            ChatStreamKind::Content,
            &mut *on_chunk,
            true,
        )?;
        tracing::debug!(
            provider = %self.provider.id,
            model = %self.provider.default_model,
            finish_reason = finish_reason.as_deref(),
            content_chars = content.chars().count(),
            reasoning_chars = reasoning.chars().count(),
            tool_call_count = tool_calls.calls.len(),
            "{}",
            t("Chat completions stream reached EOF", "聊天补全流已到达 EOF")
        );
        let mut result =
            finalize_stream_result(content, reasoning, usage, tool_calls.finish(), dsml)?;
        result.finish_reason = finish_reason;
        if reasoning_part_active {
            on_chunk(ChatStreamChunk {
                kind: ChatStreamKind::ReasoningPartEnd,
                text: String::new(),
            })?;
        }
        Ok(result)
    }

    pub(crate) async fn consume_chat_completion_response<F>(
        &self,
        response: reqwest::Response,
        on_chunk: &mut F,
    ) -> Result<ChatResult>
    where
        F: FnMut(ChatStreamChunk) -> Result<()>,
    {
        const MAX_RESPONSE_BYTES: usize = 16 * 1024 * 1024;

        if response
            .content_length()
            .is_some_and(|length| length > MAX_RESPONSE_BYTES as u64)
        {
            bail!("non-streaming chat response exceeds the 16 MiB limit");
        }
        let mut bytes = Vec::new();
        let mut stream = response.bytes_stream();
        while let Some(chunk) = self
            .next_response_chunk(&mut stream, "chat.response")
            .await?
        {
            if bytes.len().saturating_add(chunk.len()) > MAX_RESPONSE_BYTES {
                bail!("non-streaming chat response exceeds the 16 MiB limit");
            }
            bytes.extend_from_slice(&chunk);
        }
        let response: ChatCompletionResponse =
            serde_json::from_slice(&bytes).with_context(|| {
                format!(
                    "{}: {}",
                    t(
                        "invalid non-streaming chat completions response",
                        "无效的非流式聊天响应",
                    ),
                    clean_plain_text(String::from_utf8_lossy(&bytes).to_string())
                )
            })?;
        if let Some(error) = response.error {
            bail!(
                "{}: {}",
                t(
                    "non-streaming chat completions returned an error",
                    "非流式聊天响应返回错误"
                ),
                provider_error_text(&error)
            );
        }
        let choice = response
            .choices
            .into_iter()
            .next()
            .context("non-streaming chat response contained no choices")?;
        let mut tool_calls = ToolCallAccumulator::default();
        let reasoning = delta_reasoning_text(&choice.message).unwrap_or_default();
        if !reasoning.is_empty() {
            on_chunk(ChatStreamChunk {
                kind: ChatStreamKind::ReasoningPartStart,
                text: String::new(),
            })?;
            on_chunk(ChatStreamChunk {
                kind: ChatStreamKind::Reasoning,
                text: reasoning.clone(),
            })?;
            on_chunk(ChatStreamChunk {
                kind: ChatStreamKind::ReasoningPartEnd,
                text: String::new(),
            })?;
        }
        let content = choice.message.content.unwrap_or_default();
        if !content.is_empty() {
            on_chunk(ChatStreamChunk {
                kind: ChatStreamKind::Content,
                text: content.clone(),
            })?;
        }
        for tool_call in choice.message.tool_calls {
            if let Some(name) = tool_calls.push(tool_call) {
                on_chunk(ChatStreamChunk {
                    kind: ChatStreamKind::ToolCall,
                    text: name,
                })?;
            }
        }
        tracing::debug!(
            provider = %self.provider.id,
            model = %self.provider.default_model,
            finish_reason = choice.finish_reason.as_deref(),
            content_chars = content.chars().count(),
            reasoning_chars = reasoning.chars().count(),
            tool_call_count = tool_calls.calls.len(),
            "{}",
            t(
                "Non-streaming chat completions response consumed",
                "非流式聊天补全响应已处理"
            )
        );
        let mut result = finalize_stream_result(
            content,
            reasoning,
            response.usage,
            tool_calls.finish(),
            dsml_enabled_for(&self.provider),
        )?;
        result.finish_reason = choice.finish_reason;
        Ok(result)
    }

    pub(crate) fn bail_chat_completion_failure<T>(&self, status: u16, body: &str) -> Result<T> {
        let hint = claude_protocol_hint(&self.provider);
        Err(anyhow::anyhow!(
            "{} ({}): {}{}",
            t("chat completions stream request failed", "聊天流式请求失败",),
            status,
            body,
            hint
        )
        .context(HttpStatusFailure::classify(status, body)))
    }
}
