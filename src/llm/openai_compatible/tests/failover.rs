//! 端点故障转移、重试与冷却。

use super::shared::*;
use crate::llm::openai_compatible::*;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpListener;

#[tokio::test]
async fn response_header_timeout_stops_a_stalled_endpoint() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/v1", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        read_http_headers(&mut stream).await;
        tokio::time::sleep(Duration::from_millis(100)).await;
    });

    let mut provider = test_provider("header-timeout-test", &url);
    provider.protocol = "openai-chat".to_string();
    let client = test_client(provider)
        .with_request_timeouts(Duration::from_millis(20), Duration::from_secs(1));
    let error = client
        .chat_stream(vec![ChatMessage::plain("user", "hi")], Vec::new(), |_| {
            Ok(())
        })
        .await
        .unwrap_err();

    let message = format!("{error:#}");
    assert!(message.contains("response header timed out"), "{message}");
    server.await.unwrap();
}

#[tokio::test]
async fn response_header_timeout_fails_over_to_the_next_endpoint() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/v1", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        let (mut stalled, _) = listener.accept().await.unwrap();
        read_http_headers(&mut stalled).await;
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            drop(stalled);
        });
        let (mut healthy, _) = listener.accept().await.unwrap();
        read_http_headers(&mut healthy).await;
        write_http_sse_response(
            &mut healthy,
            concat!(
                "data: {\"choices\":[{\"delta\":{\"content\":\"fallback\"}}]}\n\n",
                "data: [DONE]\n\n"
            ),
        )
        .await;
    });

    let mut first = test_provider("header-timeout-first", &url);
    first.protocol = "openai-chat".to_string();
    let mut second = test_provider("header-timeout-second", &url);
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
        reasoning_visibility: ReasoningVisibility::Hidden,
        buffered_delivery: false,
        detailed_reasoning_summary: false,
        request_timeouts: Some(RequestTimeouts {
            response_header: Duration::from_millis(20),
            stream_idle: Duration::from_secs(1),
        }),
        max_tokens_override: None,
        request_scope: "chat",
        continuation_health: ResponsesContinuationHealth::detached(),
        claude_code: None,
        antigravity: None,
        codex: None,
        claude_code_dev_mode: false,
    };

    let result = client
        .chat_buffered(vec![ChatMessage::plain("user", "hi")], Vec::new())
        .await
        .unwrap();
    assert_eq!(result.content, "fallback");
    server.await.unwrap();
}

#[tokio::test]
async fn stream_idle_timeout_stops_a_stalled_endpoint() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/v1", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        read_http_headers(&mut stream).await;
        stream
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n",
            )
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
    });

    let mut provider = test_provider("stream-idle-test", &url);
    provider.protocol = "openai-chat".to_string();
    let client = test_client(provider)
        .with_request_timeouts(Duration::from_secs(1), Duration::from_millis(20));
    let error = client
        .chat_stream(vec![ChatMessage::plain("user", "hi")], Vec::new(), |_| {
            Ok(())
        })
        .await
        .unwrap_err();

    let message = format!("{error:#}");
    assert!(message.contains("response stream was idle"), "{message}");
    server.await.unwrap();
}

#[tokio::test]
async fn a_stream_that_stops_before_any_end_signal_is_not_a_completion() {
    // The failure this reproduces: the model was still emitting reasoning
    // when the connection went away, so there is no content, no tool call,
    // no `[DONE]` and no finish_reason. Accepting that as a finished turn
    // is what made a QQ reply vanish silently.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/v1", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        for _ in 0..MAX_SEND_ATTEMPTS {
            let Ok((mut stream, _)) = listener.accept().await else {
                return;
            };
            read_http_headers(&mut stream).await;
            write_truncated_sse_response(
                &mut stream,
                "data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"在想第一步\"}}]}\n\n",
            )
            .await;
        }
    });

    let mut provider = test_provider("truncated-stream-test", &url);
    provider.protocol = "openai-chat".to_string();
    provider.default_model = "test-model".to_string();
    let client = test_client(provider);

    let outcome = client
        .chat_stream(vec![ChatMessage::plain("user", "hi")], Vec::new(), |_| {
            Ok(())
        })
        .await;

    let error = outcome.expect_err("a truncated stream must not read as a finished turn");
    let message = format!("{error:#}");
    assert!(
        message.contains("ended before") || message.contains("提前结束"),
        "the error should name the truncation: {message}"
    );
    server.abort();
}

#[tokio::test]
async fn an_empty_error_field_does_not_fail_the_turn() {
    // Some gateways send `{"error":""}` next to the terminal usage event.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/v1", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        read_http_headers(&mut stream).await;
        write_http_sse_response(
            &mut stream,
            concat!(
                "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n",
                "data: {\"error\":\"\",\"choices\":[{\"finish_reason\":\"stop\",\"delta\":{}}]}\n\n",
                "data: [DONE]\n\n"
            ),
        )
        .await;
    });

    let mut provider = test_provider("empty-error-test", &url);
    provider.protocol = "openai-chat".to_string();
    provider.default_model = "test-model".to_string();
    let client = test_client(provider);

    let result = client
        .chat_stream(vec![ChatMessage::plain("user", "hi")], Vec::new(), |_| {
            Ok(())
        })
        .await
        .expect("an empty error field is not an error");
    assert_eq!(result.content, "hi");
    server.await.unwrap();
}

#[tokio::test]
async fn a_real_error_field_still_fails_the_turn() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/v1", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        read_http_headers(&mut stream).await;
        write_http_sse_response(
            &mut stream,
            concat!(
                "data: {\"error\":{\"message\":\"上游炸了\"},\"choices\":[]}\n\n",
                "data: [DONE]\n\n"
            ),
        )
        .await;
    });

    let mut provider = test_provider("real-error-test", &url);
    provider.protocol = "openai-chat".to_string();
    provider.default_model = "test-model".to_string();
    let client = test_client(provider);

    let error = client
        .chat_stream(vec![ChatMessage::plain("user", "hi")], Vec::new(), |_| {
            Ok(())
        })
        .await
        .expect_err("an in-band error must not be dressed up as a completion");
    assert!(format!("{error:#}").contains("上游炸了"));
    server.abort();
}

#[tokio::test]
async fn buffered_delivery_lets_a_committed_attempt_be_retried() {
    // A platform turn collects a whole round before posting it, so content
    // streamed before the drop reached nobody and retrying is invisible.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/v1", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        read_http_headers(&mut stream).await;
        // Content, not just reasoning: this is what used to pin the turn
        // to the failed attempt.
        write_truncated_sse_response(
            &mut stream,
            "data: {\"choices\":[{\"delta\":{\"content\":\"半句\"}}]}\n\n",
        )
        .await;

        let (mut stream, _) = listener.accept().await.unwrap();
        read_http_headers(&mut stream).await;
        write_http_sse_response(
            &mut stream,
            concat!(
                "data: {\"choices\":[{\"delta\":{\"content\":\"完整回复\"}}]}\n\n",
                "data: [DONE]\n\n"
            ),
        )
        .await;
    });

    let mut provider = test_provider("buffered-delivery-test", &url);
    provider.protocol = "openai-chat".to_string();
    provider.default_model = "test-model".to_string();
    let client = test_client(provider).with_buffered_delivery(true);

    let result = client
        .chat_stream(vec![ChatMessage::plain("user", "hi")], Vec::new(), |_| {
            Ok(())
        })
        .await
        .expect("buffered delivery means the false start was never seen");
    assert_eq!(result.content, "完整回复");
    server.await.unwrap();
}

#[tokio::test]
async fn a_stream_that_ends_on_finish_reason_alone_is_a_completion() {
    // Some OpenAI-compatible servers never send `[DONE]` (llama.cpp's
    // Responses endpoint, for one). A finish_reason is end enough.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/v1", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        read_http_headers(&mut stream).await;
        write_truncated_sse_response(
            &mut stream,
            concat!(
                "data: {\"choices\":[{\"delta\":{\"content\":\"done thinking\"}}]}\n\n",
                "data: {\"choices\":[{\"finish_reason\":\"stop\",\"delta\":{}}]}\n\n"
            ),
        )
        .await;
    });

    let mut provider = test_provider("no-done-marker-test", &url);
    provider.protocol = "openai-chat".to_string();
    provider.default_model = "test-model".to_string();
    let client = test_client(provider);

    let result = client
        .chat_stream(vec![ChatMessage::plain("user", "hi")], Vec::new(), |_| {
            Ok(())
        })
        .await
        .expect("finish_reason without [DONE] is a normal completion");
    assert_eq!(result.content, "done thinking");
    server.await.unwrap();
}

#[tokio::test]
async fn a_stream_that_ends_on_a_usage_frame_alone_is_a_completion() {
    // 08-19 实测 opencode zen 的 muse-spark-1.2-contributor:正文发完,
    // `finish_reason` 全程 null,末尾补一个 usage 帧和一个非标准的 cost 帧,
    // 然后直接断开,`[DONE]` 一次没有。usage 只有生成结束后才算得出来,所以
    // 它就是这条链路说「我说完了」的方式。此前这里被判成截断,而正文已经流
    // 给用户了,端点切换被抑制,整轮连同工具调用一起废掉。
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/v1", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        read_http_headers(&mut stream).await;
        write_truncated_sse_response(
            &mut stream,
            concat!(
                "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"你好呀！\"},\"finish_reason\":null}]}\n\n",
                "data: {\"choices\":[]}\n\n",
                "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":9,\"completion_tokens\":362,\"total_tokens\":371}}\n\n",
                "data: {\"choices\":[],\"cost\":\"0\"}\n\n"
            ),
        )
        .await;
    });

    let mut provider = test_provider("usage-tail-test", &url);
    provider.protocol = "openai-chat".to_string();
    provider.default_model = "test-model".to_string();
    let client = test_client(provider);

    let result = client
        .chat_stream(vec![ChatMessage::plain("user", "hi")], Vec::new(), |_| {
            Ok(())
        })
        .await
        .expect("a usage frame without [DONE] or finish_reason is a normal completion");
    assert_eq!(result.content, "你好呀！");
    assert_eq!(
        result.usage.as_ref().map(|usage| usage.total_tokens),
        Some(371)
    );
    server.await.unwrap();
}

#[tokio::test]
async fn endpoint_accepts_reasoning_only_completion() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/v1", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        read_http_headers(&mut stream).await;
        let body = concat!(
            "data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"partial reasoning\"}}]}\n\n",
            "data: {\"choices\":[{\"finish_reason\":\"length\",\"delta\":{}}]}\n\n",
            "data: [DONE]\n\n"
        );
        write_http_sse_response(&mut stream, body).await;
    });

    let mut provider = test_provider("reasoning-only-test", &url);
    provider.protocol = "openai-chat".to_string();
    provider.default_model = "test-model".to_string();
    let client = test_client(provider);
    let mut chunks = Vec::new();

    let result = client
        .chat_stream(
            vec![ChatMessage::plain("user", "hi")],
            Vec::new(),
            |chunk| {
                chunks.push(chunk);
                Ok(())
            },
        )
        .await
        .unwrap();

    assert!(result.content.is_empty());
    assert_eq!(result.reasoning.as_deref(), Some("partial reasoning"));
    assert_eq!(
        chunks.iter().map(|chunk| chunk.kind).collect::<Vec<_>>(),
        vec![
            ChatStreamKind::ReasoningPartStart,
            ChatStreamKind::Reasoning,
            ChatStreamKind::ReasoningPartEnd,
        ]
    );
    server.await.unwrap();
}

#[tokio::test]
async fn insufficient_streaming_quota_falls_back_to_non_streaming_once() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/v1", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        let (mut first, _) = listener.accept().await.unwrap();
        read_http_headers(&mut first).await;
        let quota = r#"{"error":{"message":"quota","code":"insufficient_quota"}}"#;
        let response = format!(
            "HTTP/1.1 429 Too Many Requests\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            quota.len(),
            quota
        );
        first.write_all(response.as_bytes()).await.unwrap();

        let (mut second, _) = listener.accept().await.unwrap();
        read_http_headers(&mut second).await;
        let body = r#"{"choices":[{"finish_reason":"stop","message":{"reasoning_content":"think","content":"answer"}}],"usage":{"prompt_tokens":3,"completion_tokens":2,"total_tokens":5}}"#;
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        second.write_all(response.as_bytes()).await.unwrap();
    });

    let mut provider = test_provider("quota-fallback-test", &url);
    provider.protocol = "openai-chat".to_string();
    provider.default_model = "test-model".to_string();
    let client = test_client(provider);
    let mut chunks = Vec::new();
    let result = client
        .chat_stream(
            vec![ChatMessage::plain("user", "hi")],
            Vec::new(),
            |chunk| {
                chunks.push(chunk);
                Ok(())
            },
        )
        .await
        .unwrap();

    assert_eq!(result.content, "answer");
    assert_eq!(result.reasoning.as_deref(), Some("think"));
    assert_eq!(result.usage.unwrap().total_tokens, 5);
    assert_eq!(
        chunks.iter().map(|chunk| chunk.kind).collect::<Vec<_>>(),
        vec![
            ChatStreamKind::ReasoningPartStart,
            ChatStreamKind::Reasoning,
            ChatStreamKind::ReasoningPartEnd,
            ChatStreamKind::Content,
        ]
    );
    server.await.unwrap();
}

#[tokio::test]
async fn endpoint_failover_resets_partial_reasoning_before_retry() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/v1", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        let bodies = [
            concat!(
                "data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"old\"}}]}\n\n",
                "data: {\"error\":{\"message\":\"upstream stream failed\"}}\n\n"
            ),
            concat!(
                "data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"new\"}}]}\n\n",
                "data: {\"choices\":[{\"delta\":{\"content\":\"answer\"}}]}\n\n",
                "data: [DONE]\n\n"
            ),
        ];
        for body in bodies {
            let (mut stream, _) = listener.accept().await.unwrap();
            read_http_headers(&mut stream).await;
            write_http_sse_response(&mut stream, body).await;
        }
    });

    let mut first = test_provider("failover-first-test", &url);
    first.protocol = "openai-chat".to_string();
    first.default_model = "test-model".to_string();
    let mut second = test_provider("failover-second-test", &url);
    second.protocol = "openai-chat".to_string();
    second.default_model = "test-model".to_string();
    let first_client = reqwest::Client::new();
    let second_client = reqwest::Client::new();
    let endpoints = vec![
        LlmEndpoint {
            client: first_client.clone(),
            provider: first.clone(),
            api_key: "first".to_string(),
            key_index: 0,
        },
        LlmEndpoint {
            client: second_client,
            provider: second,
            api_key: "second".to_string(),
            key_index: 0,
        },
    ];
    let client = OpenAiCompatibleClient {
        client: first_client,
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
    let mut chunks = Vec::new();

    let result = client
        .chat_stream(
            vec![ChatMessage::plain("user", "hi")],
            Vec::new(),
            |chunk| {
                chunks.push(chunk);
                Ok(())
            },
        )
        .await
        .unwrap();

    assert_eq!(result.reasoning.as_deref(), Some("new"));
    assert_eq!(result.content, "answer");
    assert_eq!(
        chunks.iter().map(|chunk| chunk.kind).collect::<Vec<_>>(),
        vec![
            ChatStreamKind::ReasoningPartStart,
            ChatStreamKind::Reasoning,
            ChatStreamKind::ReasoningReset,
            ChatStreamKind::ReasoningPartStart,
            ChatStreamKind::Reasoning,
            ChatStreamKind::ReasoningPartEnd,
            ChatStreamKind::Content,
        ]
    );
    server.await.unwrap();
}

#[tokio::test]
async fn buffered_completion_fails_over_after_partial_content() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/v1", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        let bodies = [
            concat!(
                "data: {\"choices\":[{\"delta\":{\"content\":\"incomplete\"}}]}\n\n",
                "data: {\"error\":{\"message\":\"upstream stream failed\"}}\n\n"
            ),
            concat!(
                "data: {\"choices\":[{\"delta\":{\"content\":\"answer\"}}]}\n\n",
                "data: [DONE]\n\n"
            ),
        ];
        for body in bodies {
            let (mut stream, _) = listener.accept().await.unwrap();
            read_http_headers(&mut stream).await;
            write_http_sse_response(&mut stream, body).await;
        }
    });

    let mut first = test_provider("buffered-first-test", &url);
    first.protocol = "openai-chat".to_string();
    let mut second = test_provider("buffered-second-test", &url);
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

    let result = client
        .chat_buffered(vec![ChatMessage::plain("user", "hi")], Vec::new())
        .await
        .unwrap();
    assert_eq!(result.content, "answer");
    server.await.unwrap();
}

#[tokio::test]
async fn endpoint_client_reuses_one_tcp_connection() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/reuse", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        for _ in 0..2 {
            read_http_headers(&mut stream).await;
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: keep-alive\r\n\r\nok",
                )
                .await
                .unwrap();
        }
    });

    let client = test_client(test_provider("test", "http://example.invalid/v1"));
    for request_id in ["request-one", "request-two"] {
        let endpoint_client = client.with_endpoint(&client.endpoints[0]);
        let response = tokio::time::timeout(
            Duration::from_secs(2),
            endpoint_client.send_with_transport_retry(request_id, "chat.send", || {
                endpoint_client.client.get(&url)
            }),
        )
        .await
        .expect("request timed out instead of reusing the connection")
        .unwrap();
        assert_eq!(response.text().await.unwrap(), "ok");
    }
    tokio::time::timeout(Duration::from_secs(2), server)
        .await
        .expect("server did not observe two requests on one connection")
        .unwrap();
}

#[tokio::test]
async fn transport_error_keeps_source_chain_without_url() {
    let unavailable = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = unavailable.local_addr().unwrap();
    drop(unavailable);
    let url = format!("http://{addr}/secret?api_key=do-not-log");
    let client = test_client(test_provider("test", "http://example.invalid/v1"));

    let error = client
        .send_with_transport_retry("request-test", "chat.send", || client.client.get(&url))
        .await
        .unwrap_err();
    let rendered = format!("{error:#}");

    assert!(rendered.contains("chat.send transport failed (connect)"));
    assert!(rendered.contains("error sending request"));
    assert!(!rendered.contains("api_key"));
    assert!(!rendered.contains("do-not-log"));
}

#[test]
fn endpoint_failover_stops_after_irreversible_stream_output() {
    let reasoning = ChatStreamChunk {
        kind: ChatStreamKind::Reasoning,
        text: "partial".to_string(),
    };
    assert!(!stream_chunk_commits_attempt(
        &reasoning,
        ReasoningVisibility::Hidden
    ));
    assert!(!stream_chunk_commits_attempt(
        &reasoning,
        ReasoningVisibility::Summary
    ));
    assert!(stream_chunk_commits_attempt(
        &reasoning,
        ReasoningVisibility::Full
    ));
    assert!(!stream_chunk_commits_attempt(
        &ChatStreamChunk {
            kind: ChatStreamKind::Content,
            text: String::new(),
        },
        ReasoningVisibility::Full,
    ));
    let reasoning_end = ChatStreamChunk {
        kind: ChatStreamKind::ReasoningPartEnd,
        text: String::new(),
    };
    assert!(!stream_chunk_commits_attempt(
        &reasoning_end,
        ReasoningVisibility::Hidden
    ));
    assert!(stream_chunk_commits_attempt(
        &reasoning_end,
        ReasoningVisibility::Summary
    ));
    for chunk in [
        ChatStreamChunk {
            kind: ChatStreamKind::Content,
            text: "answer".to_string(),
        },
        ChatStreamChunk {
            kind: ChatStreamKind::ToolCall,
            text: "ask_question".to_string(),
        },
    ] {
        assert!(stream_chunk_commits_attempt(
            &chunk,
            ReasoningVisibility::Hidden
        ));
    }
}
