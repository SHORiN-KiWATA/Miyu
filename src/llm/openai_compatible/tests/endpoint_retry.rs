//! 同一端点内的重试与冷却。

use super::shared::*;
use crate::llm::openai_compatible::*;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpListener;

#[test]
fn quota_compatibility_retry_is_narrowly_scoped() {
    assert!(non_stream_quota_fallback_candidate(
        429,
        r#"{"error":{"code":"insufficient_quota"}}"#
    ));
    assert!(!non_stream_quota_fallback_candidate(
        429,
        r#"{"error":{"code":"rate_limit_exceeded"}}"#
    ));
    assert!(!non_stream_quota_fallback_candidate(
        400,
        r#"{"error":{"code":"insufficient_quota"}}"#
    ));
}

#[test]
fn zen_upstream_failed_detects_opencode_zen_compat_error() {
    let provider = test_provider("myopencode", OPENCODE_ZEN_BASE_URL);

    assert!(zen_upstream_failed(
        &provider,
        400,
        r#"{"error":{"message":"Error from provider (Console): Upstream request failed"}}"#,
    ));
    assert!(!zen_upstream_failed(
        &provider,
        401,
        "Upstream request failed"
    ));
    assert!(!zen_upstream_failed(
        &test_provider("other", "https://example.com/v1"),
        400,
        "Upstream request failed"
    ));
}

#[tokio::test]
async fn transport_connect_failure_is_retried_once() {
    let unavailable = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let unavailable_addr = unavailable.local_addr().unwrap();
    drop(unavailable);

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let available_url = format!("http://{}/ok", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        read_http_headers(&mut stream).await;
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok")
            .await
            .unwrap();
    });

    let client = test_client(test_provider("test", "http://example.invalid/v1"));
    let unavailable_url = format!("http://{unavailable_addr}/unavailable");
    let mut builds = 0;
    let response = client
        .send_with_transport_retry("request-test", "chat.send", || {
            builds += 1;
            client.client.get(if builds == 1 {
                &unavailable_url
            } else {
                &available_url
            })
        })
        .await
        .unwrap();

    assert_eq!(builds, 2);
    assert_eq!(response.text().await.unwrap(), "ok");
    server.await.unwrap();
}

#[tokio::test]
async fn transient_http_server_errors_are_retried() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/retry", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        for status in [500, 503, 200] {
            let (mut stream, _) = listener.accept().await.unwrap();
            read_http_headers(&mut stream).await;
            let reason = if status == 200 {
                "OK"
            } else {
                "Internal Server Error"
            };
            let body = if status == 200 { "ok" } else { "error" };
            let response = format!(
                "HTTP/1.1 {status} {reason}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(response.as_bytes()).await.unwrap();
        }
    });

    let client = test_client(test_provider("test", "http://example.invalid/v1"));
    let mut builds = 0;
    let response = client
        .send_with_transport_retry("request-test", "chat.send", || {
            builds += 1;
            client.client.get(&url)
        })
        .await
        .unwrap();

    assert_eq!(builds, 3);
    assert_eq!(response.status().as_u16(), 200);
    assert_eq!(response.text().await.unwrap(), "ok");
    server.await.unwrap();
}

#[tokio::test]
async fn persistent_http_server_errors_stop_after_three_attempts() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/retry", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        for _ in 0..MAX_SEND_ATTEMPTS {
            let (mut stream, _) = listener.accept().await.unwrap();
            read_http_headers(&mut stream).await;
            stream
                .write_all(
                    b"HTTP/1.1 500 Internal Server Error\r\nContent-Length: 5\r\nConnection: close\r\n\r\nerror",
                )
                .await
                .unwrap();
        }
    });

    let client = test_client(test_provider("test", "http://example.invalid/v1"));
    let mut builds = 0;
    let error = tokio::time::timeout(
        Duration::from_secs(1),
        client.send_with_transport_retry("request-test", "chat.send", || {
            builds += 1;
            client.client.get(&url)
        }),
    )
    .await
    .expect("persistent 5xx retries did not stop")
    .unwrap_err();

    assert_eq!(builds, MAX_SEND_ATTEMPTS);
    let failure = error.downcast_ref::<HttpStatusFailure>().unwrap();
    assert_eq!(failure.status, 500);
    server.await.unwrap();
}

#[tokio::test]
async fn a_lone_endpoint_still_gets_retried() {
    // Attempts used to equal endpoints, so the person with a single model
    // — the one with nowhere else to go — got no retry at all.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/v1", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        // First connection dies mid-stream; the second answers properly.
        let (mut stream, _) = listener.accept().await.unwrap();
        read_http_headers(&mut stream).await;
        write_truncated_sse_response(
            &mut stream,
            "data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"想了一半\"}}]}\n\n",
        )
        .await;

        let (mut stream, _) = listener.accept().await.unwrap();
        read_http_headers(&mut stream).await;
        write_http_sse_response(
            &mut stream,
            concat!(
                "data: {\"choices\":[{\"delta\":{\"content\":\"第二次成功\"}}]}\n\n",
                "data: [DONE]\n\n"
            ),
        )
        .await;
    });

    let mut provider = test_provider("lone-endpoint-test", &url);
    provider.protocol = "openai-chat".to_string();
    provider.default_model = "test-model".to_string();
    let client = test_client(provider);
    assert_eq!(client.endpoints.len(), 1, "the point is a single endpoint");

    let result = client
        .chat_stream(vec![ChatMessage::plain("user", "hi")], Vec::new(), |_| {
            Ok(())
        })
        .await
        .expect("a single endpoint should still be retried");
    assert_eq!(result.content, "第二次成功");
    server.await.unwrap();
}

#[test]
fn typed_failures_drive_endpoint_cooldowns() {
    let rate_limit =
        anyhow::anyhow!("provider body").context(HttpStatusFailure::classify(429, "provider body"));
    let quota = anyhow::anyhow!("provider body")
        .context(HttpStatusFailure::classify(400, "quota exceeded"));
    let invalid_key = anyhow::anyhow!("provider body")
        .context(HttpStatusFailure::classify(400, "invalid api key"));
    let transport = anyhow::anyhow!("socket source").context(TransportFailure {
        stage: "chat.send",
        kind: TransportFailureKind::Connect,
    });
    let protocol = anyhow::anyhow!("invalid response shape");

    assert_eq!(
        cooldown_for_error(&rate_limit),
        Some(Duration::from_secs(600))
    );
    assert_eq!(cooldown_for_error(&quota), Some(Duration::from_secs(600)));
    assert_eq!(
        cooldown_for_error(&invalid_key),
        Some(Duration::from_secs(600))
    );
    assert_eq!(
        cooldown_for_error(&transport),
        Some(Duration::from_secs(120))
    );
    assert_eq!(cooldown_for_error(&protocol), None);
}

#[test]
fn structured_provider_errors_drive_failure_semantics() {
    let rate_limit = HttpStatusFailure::classify(
        400,
        r#"{"error":{"type":"rate_limit_error","code":"rate_limit_exceeded"}}"#,
    );
    let invalid_key = HttpStatusFailure::classify(
        400,
        r#"{"error":{"type":"authentication_error","code":"invalid_api_key"}}"#,
    );
    let unavailable_model = HttpStatusFailure::classify(
        400,
        r#"{"error":{"type":"invalid_request_error","code":"model_not_available"}}"#,
    );
    let incompatible_endpoint = HttpStatusFailure::classify(
        400,
        r#"{"error":{"type":"invalid_request_error","message":"Unknown parameter: tools"}}"#,
    );
    let invalid_request = HttpStatusFailure::classify(
        400,
        r#"{"error":{"type":"invalid_request_error","message":"Malformed request body"}}"#,
    );
    let google_invalid_request = HttpStatusFailure::classify(
        400,
        r#"{"error":{"status":"InvalidArgument","message":"request rejected"}}"#,
    );
    let azure_missing_deployment = HttpStatusFailure::classify(
        400,
        r#"{"error":{"code":"DeploymentNotFound","message":"missing"}}"#,
    );
    let unknown = HttpStatusFailure::classify(400, r#"{"error":{"message":"failed"}}"#);

    assert_eq!(rate_limit.kind, HttpFailureKind::RateLimit);
    assert_eq!(invalid_key.kind, HttpFailureKind::Authentication);
    assert_eq!(unavailable_model.kind, HttpFailureKind::EndpointUnavailable);
    assert_eq!(
        incompatible_endpoint.kind,
        HttpFailureKind::EndpointIncompatible
    );
    assert_eq!(invalid_request.kind, HttpFailureKind::InvalidRequest);
    assert_eq!(google_invalid_request.kind, HttpFailureKind::InvalidRequest);
    assert_eq!(
        azure_missing_deployment.kind,
        HttpFailureKind::EndpointUnavailable
    );
    assert_eq!(unknown.kind, HttpFailureKind::Status);

    assert!(endpoint_failover_allowed(&anyhow::Error::new(
        incompatible_endpoint
    )));
    let invalid_request = anyhow::Error::new(invalid_request);
    assert_eq!(cooldown_for_error(&invalid_request), None);
    assert!(!endpoint_failover_allowed(&invalid_request));
    assert!(endpoint_failover_allowed(&anyhow::Error::new(unknown)));
}

#[test]
fn scheduler_skips_cooling_endpoints_and_reports_an_exhausted_pool() {
    let first = test_client(test_provider(
        "scheduler-first",
        "http://example.invalid/v1",
    ));
    let second = test_client(test_provider(
        "scheduler-second",
        "http://example.invalid/v1",
    ));
    let endpoints = vec![first.endpoints[0].clone(), second.endpoints[0].clone()];
    let mut scheduler = LlmScheduler::default();

    scheduler.mark_failure(endpoints[0].id(), Duration::from_secs(60));
    assert_eq!(scheduler.ordered_indices(&endpoints), vec![1]);

    scheduler.mark_failure(endpoints[1].id(), Duration::from_secs(60));
    assert!(scheduler.ordered_indices(&endpoints).is_empty());

    scheduler.mark_success(&endpoints[0].id());
    assert_eq!(scheduler.ordered_indices(&endpoints), vec![0]);
}

#[tokio::test]
async fn invalid_request_does_not_fail_over_to_another_endpoint() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/v1", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        read_http_headers(&mut stream).await;
        let body =
            r#"{"error":{"type":"invalid_request_error","message":"Malformed request body"}}"#;
        let response = format!(
            "HTTP/1.1 400 Bad Request\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        stream.write_all(response.as_bytes()).await.unwrap();
        tokio::time::timeout(Duration::from_millis(100), listener.accept())
            .await
            .is_err()
    });

    let mut first = test_provider("invalid-request-first", &url);
    first.protocol = "openai-chat".to_string();
    let mut second = test_provider("invalid-request-second", &url);
    second.protocol = "openai-chat".to_string();
    let http_client = reqwest::Client::new();
    let endpoints = vec![
        LlmEndpoint {
            client: http_client.clone(),
            provider: first.clone(),
            api_key: "first".to_string(),
            key_index: 0,
        },
        LlmEndpoint {
            client: http_client.clone(),
            provider: second,
            api_key: "second".to_string(),
            key_index: 0,
        },
    ];
    let client = OpenAiCompatibleClient {
        client: http_client,
        provider: first,
        api_key: "first".to_string(),
        endpoints: Arc::new(endpoints),
        thinking_variants: HashMap::new(),
        reasoning_visibility: ReasoningVisibility::Summary,
        buffered_delivery: false,
        detailed_reasoning_summary: false,
        request_timeouts: None,
        max_tokens_override: None,
        request_scope: "chat",
        claude_code: None,
        antigravity: None,
        codex: None,
        claude_code_dev_mode: false,
        continuation_health: ResponsesContinuationHealth::detached(),
    };

    let error = client
        .chat_stream(vec![ChatMessage::plain("user", "hi")], Vec::new(), |_| {
            Ok(())
        })
        .await
        .unwrap_err();

    assert!(format!("{error:#}").contains("endpoint failover was suppressed"));
    assert!(
        server.await.unwrap(),
        "a second endpoint received the request"
    );
}

#[test]
fn only_connect_failures_are_retried() {
    assert!(retryable_transport_failure(TransportFailureKind::Connect));
    assert!(!retryable_transport_failure(TransportFailureKind::Timeout));
    assert!(!retryable_transport_failure(TransportFailureKind::Other));
    assert!(retryable_http_status(500));
    assert!(retryable_http_status(599));
    assert!(!retryable_http_status(429));
    assert!(!retryable_http_status(400));
}

#[test]
fn http_status_retry_delay_caps_at_configured_maximum() {
    assert_eq!(http_status_retry_delay(1), Duration::from_millis(10));
    assert_eq!(http_status_retry_delay(2), Duration::from_millis(20));
    assert_eq!(http_status_retry_delay(3), Duration::from_millis(40));
    assert_eq!(http_status_retry_delay(4), Duration::from_millis(80));
    assert_eq!(http_status_retry_delay(5), Duration::from_millis(120));
    assert_eq!(
        http_status_retry_delay(usize::MAX),
        Duration::from_millis(120)
    );
}

#[tokio::test]
async fn a_rate_limited_endpoint_costs_one_request_per_turn_not_three() {
    // Regression: `MIN_ENDPOINT_ATTEMPTS` padded the attempt list by
    // cycling the only endpoint, so a single 429 fired three back-to-back
    // requests with no backoff — and the 600s cooldown then refilled the
    // whole pool, repeating the triple every turn for the entire cooldown.
    let (url, hits, server) = spawn_rate_limited_endpoint().await;
    let client = client_over(vec![rate_limit_test_endpoint(
        "rate-limit-single-endpoint-test",
        &url,
    )]);

    for turn in 1..=3 {
        let before = hits.load(Ordering::SeqCst);
        let error = client
            .chat_stream(vec![ChatMessage::plain("user", "hi")], Vec::new(), |_| {
                Ok(())
            })
            .await
            .unwrap_err();
        assert!(
            format!("{error:#}").contains("429"),
            "turn {turn} did not surface the rate limit: {error:#}"
        );
        assert_eq!(
            hits.load(Ordering::SeqCst) - before,
            1,
            "turn {turn} spent more than one request on a rate-limited endpoint"
        );
    }

    server.abort();
}

#[tokio::test]
async fn a_rate_limited_endpoint_still_fails_over_to_a_different_one() {
    // The same-endpoint suppression must not cost cross-endpoint failover:
    // each distinct endpoint is still tried exactly once.
    let (first_url, first_hits, first_server) = spawn_rate_limited_endpoint().await;
    let (second_url, second_hits, second_server) = spawn_rate_limited_endpoint().await;
    let client = client_over(vec![
        rate_limit_test_endpoint("rate-limit-failover-first-test", &first_url),
        rate_limit_test_endpoint("rate-limit-failover-second-test", &second_url),
    ]);

    let error = client
        .chat_stream(vec![ChatMessage::plain("user", "hi")], Vec::new(), |_| {
            Ok(())
        })
        .await
        .unwrap_err();

    assert_eq!(first_hits.load(Ordering::SeqCst), 1);
    assert_eq!(second_hits.load(Ordering::SeqCst), 1);
    let rendered = format!("{error:#}");
    for id in [
        "rate-limit-failover-first-test",
        "rate-limit-failover-second-test",
    ] {
        assert!(
            rendered.contains(id),
            "{id} missing from the failure report: {rendered}"
        );
    }

    first_server.abort();
    second_server.abort();
}
