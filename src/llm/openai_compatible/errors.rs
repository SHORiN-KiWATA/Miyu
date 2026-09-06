//! 失败的分类。
//!
//! 「这次失败该不该重试、该不该换端点」全靠这里的分类。分成两层：传输层
//! （连不上、超时）和 HTTP 层（状态码 + 响应体）。
//!
//! `classify_provider_error_body` 存在的原因是**供应商的状态码经常不可信**：
//! 配额耗尽返回 200 带一个 error 字段、参数错误返回 500 的都有。所以除了看状
//! 态码，还要读响应体里的信号。这也是「报错信息是嫌疑人」在代码里的样子。

use crate::llm::openai_compatible::*;

pub(in crate::llm::openai_compatible) const TRANSPORT_RETRY_DELAY: Duration =
    Duration::from_millis(250);

pub(in crate::llm::openai_compatible) const MAX_SEND_ATTEMPTS: usize = 3;

/// Attempts a request gets before giving up, however few endpoints exist. With
/// several endpoints these are failovers; with one they are plain retries.
pub(in crate::llm::openai_compatible) const MIN_ENDPOINT_ATTEMPTS: usize = 3;

#[cfg(not(test))]
pub(in crate::llm::openai_compatible) const HTTP_STATUS_RETRY_INITIAL_DELAY: Duration =
    Duration::from_secs(2);

#[cfg(test)]
pub(in crate::llm::openai_compatible) const HTTP_STATUS_RETRY_INITIAL_DELAY: Duration =
    Duration::from_millis(10);

#[cfg(not(test))]
pub(in crate::llm::openai_compatible) const HTTP_STATUS_RETRY_MAX_DELAY: Duration =
    Duration::from_secs(120);

#[cfg(test)]
pub(in crate::llm::openai_compatible) const HTTP_STATUS_RETRY_MAX_DELAY: Duration =
    Duration::from_millis(120);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::llm::openai_compatible) enum TransportFailureKind {
    Connect,
    Timeout,
    Other,
}

impl std::fmt::Display for TransportFailureKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Connect => "connect",
            Self::Timeout => "timeout",
            Self::Other => "request",
        })
    }
}

pub(in crate::llm::openai_compatible) fn retryable_transport_failure(
    kind: TransportFailureKind,
) -> bool {
    kind == TransportFailureKind::Connect
}

pub(in crate::llm::openai_compatible) fn retryable_http_status(status: u16) -> bool {
    (500..=599).contains(&status)
}

pub(in crate::llm::openai_compatible) fn http_status_retry_delay(attempt: usize) -> Duration {
    HTTP_STATUS_RETRY_INITIAL_DELAY
        .saturating_mul(1 << attempt.saturating_sub(1).min(6))
        .min(HTTP_STATUS_RETRY_MAX_DELAY)
}

#[derive(Debug)]
pub(in crate::llm::openai_compatible) struct TransportFailure {
    pub(in crate::llm::openai_compatible) stage: &'static str,
    pub(in crate::llm::openai_compatible) kind: TransportFailureKind,
}

impl std::fmt::Display for TransportFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} transport failed ({})", self.stage, self.kind)
    }
}

impl std::error::Error for TransportFailure {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::llm::openai_compatible) enum HttpFailureKind {
    Status,
    Authentication,
    RateLimit,
    EndpointUnavailable,
    EndpointIncompatible,
    InvalidRequest,
    /// 供应商的内容策略拦下了这条提示词(Google「Prohibited Use policy」
    /// 那种):是对这条内容的裁定,不是端点故障——不冷却、可切别的端点、
    /// 同端点重打必然再撞。
    ContentPolicy,
}

impl std::fmt::Display for HttpFailureKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Status => "status",
            Self::Authentication => "authentication",
            Self::RateLimit => "rate_limit",
            Self::EndpointUnavailable => "endpoint_unavailable",
            Self::EndpointIncompatible => "endpoint_incompatible",
            Self::InvalidRequest => "invalid_request",
            Self::ContentPolicy => "content_policy",
        })
    }
}

#[derive(Debug)]
pub(in crate::llm::openai_compatible) struct HttpStatusFailure {
    pub(in crate::llm::openai_compatible) status: u16,
    pub(in crate::llm::openai_compatible) kind: HttpFailureKind,
}

impl HttpStatusFailure {
    pub(in crate::llm::openai_compatible) fn classify(status: u16, body: &str) -> Self {
        let kind = match status {
            401 | 403 => HttpFailureKind::Authentication,
            429 => HttpFailureKind::RateLimit,
            // 404 从 LLM 端点回来只有一个意思:这儿没有这个模型/这条路径。
            // 一定是端点自己的问题,换一个端点有意义——不该按「服务端一时
            // 抖动」给冷却。不靠报文措辞判,因为措辞五花八门(08-18 实测:
            // LM Studio 回的是 `not_found_error` + "Model 'X' not found",
            // 模型名夹在中间,拼不出 `model_not_found` 这个关键词)。
            404 => HttpFailureKind::EndpointUnavailable,
            408 | 500..=599 => HttpFailureKind::Status,
            _ => classify_provider_error_body(body).unwrap_or(HttpFailureKind::Status),
        };
        Self { status, kind }
    }
}

pub(in crate::llm::openai_compatible) fn classify_provider_error_body(
    body: &str,
) -> Option<HttpFailureKind> {
    let structured = serde_json::from_str::<Value>(body).ok();
    let error = structured
        .as_ref()
        .and_then(|value| value.get("error"))
        .or(structured.as_ref());
    let mut signals = Vec::with_capacity(3);
    if let Some(error) = error {
        for field in ["code", "type", "status", "message"] {
            if let Some(value) = error.get(field).and_then(Value::as_str) {
                signals.push(normalize_error_signal(value));
            }
        }
    }
    if signals.is_empty() {
        signals.push(normalize_error_signal(body));
    }

    for signal in &signals {
        if contains_any(
            signal,
            &[
                "invalid_api_key",
                "incorrect_api_key",
                "authentication",
                "unauthorized",
                "forbidden",
                "permission_denied",
            ],
        ) {
            return Some(HttpFailureKind::Authentication);
        }
    }
    for signal in &signals {
        if contains_any(
            signal,
            &["rate_limit", "ratelimit", "quota", "too_many_requests"],
        ) {
            return Some(HttpFailureKind::RateLimit);
        }
    }
    for signal in &signals {
        if contains_any(
            signal,
            &[
                "model_not_found",
                "model_not_available",
                "model_unavailable",
                "unsupported_model",
                "deployment_not_found",
                "model_access_denied",
                "no_available_provider",
                "not_found",
                "provider_unavailable",
                "upstream_request_failed",
                "service_unavailable",
                "overloaded",
            ],
        ) {
            return Some(HttpFailureKind::EndpointUnavailable);
        }
    }
    for signal in &signals {
        if contains_any(
            signal,
            &[
                "context_length",
                "context_window",
                "max_tokens",
                "unsupported_parameter",
                "unknown_parameter",
                "unsupported_feature",
                "not_supported",
            ],
        ) {
            return Some(HttpFailureKind::EndpointIncompatible);
        }
    }
    for signal in &signals {
        if contains_any(
            signal,
            &[
                "invalid_request",
                "invalid_argument",
                "malformed",
                "validation_error",
            ],
        ) {
            return Some(HttpFailureKind::InvalidRequest);
        }
    }
    None
}

pub(in crate::llm::openai_compatible) fn normalize_error_signal(value: &str) -> String {
    let mut normalized = String::with_capacity(value.len());
    let mut separator = false;
    let bytes = value.as_bytes();
    for (index, byte) in bytes.iter().copied().enumerate() {
        if byte.is_ascii_alphanumeric() {
            let previous = index
                .checked_sub(1)
                .and_then(|index| bytes.get(index))
                .copied();
            let next = bytes.get(index + 1).copied();
            let camel_case_boundary = byte.is_ascii_uppercase()
                && previous.is_some_and(|previous| {
                    previous.is_ascii_lowercase()
                        || previous.is_ascii_digit()
                        || (previous.is_ascii_uppercase()
                            && next.is_some_and(|next_byte| next_byte.is_ascii_lowercase()))
                });
            if camel_case_boundary && !separator && !normalized.is_empty() {
                normalized.push('_');
            }
            normalized.push((byte as char).to_ascii_lowercase());
            separator = false;
        } else if !separator && !normalized.is_empty() {
            normalized.push('_');
            separator = true;
        }
    }
    if normalized.ends_with('_') {
        normalized.pop();
    }
    normalized
}

pub(in crate::llm::openai_compatible) fn contains_any(value: &str, needles: &[&str]) -> bool {
    needles.iter().any(|needle| value.contains(needle))
}

impl std::fmt::Display for HttpStatusFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "upstream returned HTTP {}", self.status)
    }
}

impl std::error::Error for HttpStatusFailure {}

pub(in crate::llm::openai_compatible) fn format_error_chain(
    error: &(dyn std::error::Error + 'static),
) -> String {
    let mut message = error.to_string();
    let mut source = error.source();
    while let Some(error) = source {
        message.push_str(": ");
        message.push_str(&error.to_string());
        source = error.source();
    }
    message
}

pub(in crate::llm::openai_compatible) fn anthropic_thinking_unsupported(
    status: u16,
    body: &str,
) -> bool {
    if status != 400 && status != 422 {
        return false;
    }
    let body = body.to_ascii_lowercase();
    body.contains("thinking")
        && (body.contains("unsupported")
            || body.contains("not supported")
            || body.contains("unknown")
            || body.contains("invalid")
            || body.contains("unrecognized"))
}

pub(in crate::llm::openai_compatible) fn responses_unsupported(status: u16, body: &str) -> bool {
    if status == 404 || status == 405 {
        return true;
    }
    if status != 400 {
        return false;
    }
    let body = body.to_ascii_lowercase();
    body.contains("unsupported")
        || body.contains("not supported")
        || body.contains("unknown parameter")
        || body.contains("invalid endpoint")
        || body.contains("not found")
}

pub(in crate::llm::openai_compatible) fn stream_options_unsupported(
    status: u16,
    body: &str,
) -> bool {
    if status != 400 && status != 422 {
        return false;
    }
    let body = body.to_ascii_lowercase();
    body.contains("stream_options")
        && (body.contains("unsupported")
            || body.contains("not supported")
            || body.contains("unknown")
            || body.contains("unrecognized")
            || body.contains("invalid")
            || body.contains("extra"))
}

pub(in crate::llm::openai_compatible) fn non_stream_quota_fallback_candidate(
    status: u16,
    body: &str,
) -> bool {
    status == 429 && body.to_ascii_lowercase().contains("insufficient_quota")
}

pub(in crate::llm::openai_compatible) fn zen_upstream_failed(
    provider: &ProviderConfig,
    status: u16,
    body: &str,
) -> bool {
    status == 400
        && provider.base_url.trim_end_matches('/') == OPENCODE_ZEN_BASE_URL
        && body
            .to_ascii_lowercase()
            .contains("upstream request failed")
}

pub(in crate::llm::openai_compatible) fn is_empty_error(value: &Value) -> bool {
    match value {
        Value::String(text) => text.trim().is_empty(),
        Value::Object(fields) => fields.is_empty(),
        Value::Null => true,
        _ => false,
    }
}

pub(in crate::llm::openai_compatible) fn provider_error_text(value: &Value) -> String {
    value
        .get("message")
        .and_then(Value::as_str)
        .or_else(|| {
            value
                .get("error")
                .and_then(|error| error.get("message"))
                .and_then(Value::as_str)
        })
        .map(|message| clean_plain_text(message.to_string()))
        .unwrap_or_else(|| clean_plain_text(value.to_string()))
}

#[cfg(test)]
mod classify_tests {
    use super::*;

    /// 08-18 实测：LM Studio 对着一个已经删掉的模型名回 404，报文是
    /// `not_found_error`，而分类器只认 `model_not_found`——名字夹在中间就拼不
    /// 出关键词，于是落到 `Status`，冷却时长按「一时的服务端抖动」给，而不是
    /// 按「这个端点没有这个模型」给。
    #[test]
    fn a_404_from_an_endpoint_is_endpoint_unavailable() {
        let body = r#"{"error":{"message":"Model 'Qwen3.5-2B-MLX-8bit' not found. Available models: bge-m3-mlx-8bit, Qwen3.6-35B-A3B-DFlash-MLX-6bit, Qwen3.6-35B-A3B-4bit","type":"not_found_error","param":null,"code":null}}"#;
        assert_eq!(
            HttpStatusFailure::classify(404, body).kind,
            HttpFailureKind::EndpointUnavailable
        );
    }

    /// 报文自己说 not_found 时，即便状态码不是 404 也该按端点不可用处理。
    #[test]
    fn a_not_found_body_is_endpoint_unavailable_whatever_the_status() {
        let body = r#"{"error":{"type":"not_found_error","message":"no such deployment"}}"#;
        assert_eq!(
            classify_provider_error_body(body),
            Some(HttpFailureKind::EndpointUnavailable)
        );
    }

    /// 别把真正的请求错误也吞成端点问题——那类不该换端点重试。
    #[test]
    fn a_malformed_request_still_stops_the_failover() {
        let body =
            r#"{"error":{"type":"invalid_request_error","message":"messages must be an array"}}"#;
        assert_eq!(
            classify_provider_error_body(body),
            Some(HttpFailureKind::InvalidRequest)
        );
        assert!(!endpoint_failover_allowed(&anyhow::Error::new(
            HttpStatusFailure::classify(400, body)
        )));
    }
}
