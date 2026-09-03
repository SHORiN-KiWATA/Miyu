//! OpenAI Responses 协议的流式与续接。

use super::shared::*;
use crate::llm::openai_compatible::*;
use tokio::net::TcpListener;

#[test]
fn openai_gpt5_uses_responses_api() {
    let mut provider = test_provider("openai", "https://api.openai.com/v1");
    provider.default_model = "gpt-5.5".to_string();
    let client = test_client(provider);

    assert!(client.uses_openai_responses());
}

#[test]
fn openai_compatible_gpt5_tries_responses_api() {
    let mut provider = test_provider("taotoken", "https://taotoken.net/api/v1");
    provider.default_model = "gpt-5.5".to_string();
    let client = test_client(provider);

    assert!(client.uses_openai_responses());
}

#[test]
fn responses_unsupported_allows_chat_fallback() {
    assert!(responses_unsupported(404, "not found"));
    assert!(responses_unsupported(400, "unsupported endpoint"));
    assert!(!responses_unsupported(401, "invalid api key"));
}

#[test]
fn responses_assistant_history_uses_easy_input_message() {
    let input = lower_responses_messages(vec![ChatMessage::assistant("prior answer", None)]);

    assert_eq!(
        input,
        vec![json!({"role": "assistant", "content": "prior answer"})]
    );
}

#[test]
fn responses_stream_emits_reasoning_and_content() {
    let mut content = String::new();
    let mut content_emitted = 0usize;
    let mut reasoning = String::new();
    let mut reasoning_emitted = 0usize;
    let mut reasoning_part_active = false;
    let mut usage = None;
    let mut content_started = false;
    let mut output_text_delta_parts = HashSet::new();
    let mut refusal_delta_parts = HashSet::new();
    let mut response_id = None;
    let mut tool_calls = ResponsesToolAccumulator::default();
    let mut chunks = Vec::new();
    let mut on_chunk = |chunk| {
        chunks.push(chunk);
        Ok(())
    };

    handle_responses_sse_line(
        r#"data: {"type":"response.reasoning_summary_text.delta","item_id":"rs_1","delta":"思考"}"#,
        &mut content,
        &mut content_emitted,
        &mut reasoning,
        &mut reasoning_emitted,
        &mut reasoning_part_active,
        &mut usage,
        &mut content_started,
        &mut output_text_delta_parts,
        &mut refusal_delta_parts,
        &mut response_id,
        &mut tool_calls,
        &mut on_chunk,
    )
    .unwrap();
    handle_responses_sse_line(
        r#"data: {"type":"response.output_text.delta","item_id":"msg_1","delta":""}"#,
        &mut content,
        &mut content_emitted,
        &mut reasoning,
        &mut reasoning_emitted,
        &mut reasoning_part_active,
        &mut usage,
        &mut content_started,
        &mut output_text_delta_parts,
        &mut refusal_delta_parts,
        &mut response_id,
        &mut tool_calls,
        &mut on_chunk,
    )
    .unwrap();
    handle_responses_sse_line(
        r#"data: {"type":"response.output_text.delta","item_id":"msg_1","delta":"答案"}"#,
        &mut content,
        &mut content_emitted,
        &mut reasoning,
        &mut reasoning_emitted,
        &mut reasoning_part_active,
        &mut usage,
        &mut content_started,
        &mut output_text_delta_parts,
        &mut refusal_delta_parts,
        &mut response_id,
        &mut tool_calls,
        &mut on_chunk,
    )
    .unwrap();

    assert_eq!(chunks.len(), 4);
    assert_eq!(chunks[0].kind, ChatStreamKind::ReasoningPartStart);
    assert_eq!(chunks[1].kind, ChatStreamKind::Reasoning);
    assert_eq!(chunks[1].text, "思考");
    assert_eq!(chunks[2].kind, ChatStreamKind::ReasoningPartEnd);
    assert_eq!(chunks[3].kind, ChatStreamKind::Content);
    assert_eq!(chunks[3].text, "答案");
}

#[test]
fn responses_reasoning_done_emits_content_boundary() {
    let mut content = String::new();
    let mut content_emitted = 0usize;
    let mut reasoning = String::new();
    let mut reasoning_emitted = 0usize;
    let mut reasoning_part_active = false;
    let mut usage = None;
    let mut content_started = false;
    let mut output_text_delta_parts = HashSet::new();
    let mut refusal_delta_parts = HashSet::new();
    let mut response_id = None;
    let mut tool_calls = ResponsesToolAccumulator::default();
    let mut chunks = Vec::new();
    let mut on_chunk = |chunk| {
        chunks.push(chunk);
        Ok(())
    };

    for line in [
        r#"data: {"type":"response.reasoning_summary_text.delta","item_id":"rs_1","delta":"思考"}"#,
        r#"data: {"type":"response.reasoning_summary_text.done","item_id":"rs_1"}"#,
        r#"data: {"type":"response.output_text.delta","item_id":"msg_1","delta":"答案"}"#,
        r#"data: {"type":"response.reasoning_summary_text.delta","item_id":"rs_1","delta":"晚到"}"#,
    ] {
        handle_responses_sse_line(
            line,
            &mut content,
            &mut content_emitted,
            &mut reasoning,
            &mut reasoning_emitted,
            &mut reasoning_part_active,
            &mut usage,
            &mut content_started,
            &mut output_text_delta_parts,
            &mut refusal_delta_parts,
            &mut response_id,
            &mut tool_calls,
            &mut on_chunk,
        )
        .unwrap();
    }

    assert_eq!(chunks.len(), 7);
    assert_eq!(chunks[0].kind, ChatStreamKind::ReasoningPartStart);
    assert_eq!(chunks[1].kind, ChatStreamKind::Reasoning);
    assert_eq!(chunks[1].text, "思考");
    assert_eq!(chunks[2].kind, ChatStreamKind::ReasoningPartEnd);
    assert_eq!(chunks[3].kind, ChatStreamKind::Content);
    assert!(chunks[3].text.is_empty());
    assert_eq!(chunks[4].kind, ChatStreamKind::Content);
    assert_eq!(chunks[4].text, "答案");
    assert_eq!(chunks[5].kind, ChatStreamKind::ReasoningPartStart);
    assert_eq!(chunks[6].kind, ChatStreamKind::Reasoning);
    assert_eq!(chunks[6].text, "\n\n晚到");
    assert_eq!(reasoning, "思考\n\n晚到");
}

#[test]
fn responses_stream_preserves_multiple_reasoning_summary_parts() {
    let mut content = String::new();
    let mut content_emitted = 0usize;
    let mut reasoning = String::new();
    let mut reasoning_emitted = 0usize;
    let mut reasoning_part_active = false;
    let mut usage = None;
    let mut content_started = false;
    let mut output_text_delta_parts = HashSet::new();
    let mut refusal_delta_parts = HashSet::new();
    let mut response_id = None;
    let mut tool_calls = ResponsesToolAccumulator::default();
    let mut chunks = Vec::new();
    let mut on_chunk = |chunk| {
        chunks.push(chunk);
        Ok(())
    };

    for line in [
        r#"data: {"type":"response.reasoning_summary_part.added","item_id":"rs_1","summary_index":0}"#,
        r#"data: {"type":"response.reasoning_summary_text.delta","item_id":"rs_1","summary_index":0,"delta":"**Planning response**"}"#,
        r#"data: {"type":"response.reasoning_summary_part.done","item_id":"rs_1","summary_index":0}"#,
        r#"data: {"type":"response.reasoning_summary_part.added","item_id":"rs_1","summary_index":1}"#,
        r#"data: {"type":"response.reasoning_summary_text.delta","item_id":"rs_1","summary_index":1,"delta":"**Designing helper**"}"#,
        r#"data: {"type":"response.reasoning_summary_part.done","item_id":"rs_1","summary_index":1}"#,
    ] {
        handle_responses_sse_line(
            line,
            &mut content,
            &mut content_emitted,
            &mut reasoning,
            &mut reasoning_emitted,
            &mut reasoning_part_active,
            &mut usage,
            &mut content_started,
            &mut output_text_delta_parts,
            &mut refusal_delta_parts,
            &mut response_id,
            &mut tool_calls,
            &mut on_chunk,
        )
        .unwrap();
    }

    let kinds = chunks.iter().map(|chunk| chunk.kind).collect::<Vec<_>>();
    assert_eq!(
        kinds,
        vec![
            ChatStreamKind::ReasoningPartStart,
            ChatStreamKind::Reasoning,
            ChatStreamKind::ReasoningPartEnd,
            ChatStreamKind::ReasoningPartStart,
            ChatStreamKind::Reasoning,
            ChatStreamKind::ReasoningPartEnd,
        ]
    );
    assert_eq!(reasoning, "**Planning response**\n\n**Designing helper**");
}

#[test]
fn responses_stream_collects_tool_calls() {
    let mut content = String::new();
    let mut content_emitted = 0usize;
    let mut reasoning = String::new();
    let mut reasoning_emitted = 0usize;
    let mut reasoning_part_active = false;
    let mut usage = None;
    let mut content_started = false;
    let mut output_text_delta_parts = HashSet::new();
    let mut refusal_delta_parts = HashSet::new();
    let mut response_id = None;
    let mut tool_calls = ResponsesToolAccumulator::default();
    let mut on_chunk = |_| Ok(());

    for line in [
        r#"data: {"type":"response.output_item.added","item":{"type":"function_call","id":"item_1","call_id":"call_1","name":"calc","arguments":""}}"#,
        r#"data: {"type":"response.function_call_arguments.delta","item_id":"item_1","delta":"{\"x\":"}"#,
        r#"data: {"type":"response.function_call_arguments.delta","item_id":"item_1","delta":"1}"}"#,
        r#"data: {"type":"response.output_item.done","item":{"type":"function_call","id":"item_1","call_id":"call_1","name":"calc","arguments":"{\"x\":1}"}}"#,
    ] {
        handle_responses_sse_line(
            line,
            &mut content,
            &mut content_emitted,
            &mut reasoning,
            &mut reasoning_emitted,
            &mut reasoning_part_active,
            &mut usage,
            &mut content_started,
            &mut output_text_delta_parts,
            &mut refusal_delta_parts,
            &mut response_id,
            &mut tool_calls,
            &mut on_chunk,
        )
        .unwrap();
    }

    let calls = tool_calls.finish();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].id, "call_1");
    assert_eq!(calls[0].function.name, "calc");
    assert_eq!(calls[0].function.arguments, r#"{"x":1}"#);
}

#[test]
fn responses_stream_announces_question_tool_when_item_starts() {
    let mut content = String::new();
    let mut content_emitted = 0usize;
    let mut reasoning = String::new();
    let mut reasoning_emitted = 0usize;
    let mut reasoning_part_active = false;
    let mut usage = None;
    let mut content_started = false;
    let mut output_text_delta_parts = HashSet::new();
    let mut refusal_delta_parts = HashSet::new();
    let mut response_id = None;
    let mut tool_calls = ResponsesToolAccumulator::default();
    let mut chunks = Vec::new();
    let mut on_chunk = |chunk| {
        chunks.push(chunk);
        Ok(())
    };

    handle_responses_sse_line(
        r#"data: {"type":"response.output_item.added","item":{"type":"function_call","id":"item_1","call_id":"call_1","name":"ask_question","arguments":""}}"#,
        &mut content,
        &mut content_emitted,
        &mut reasoning,
        &mut reasoning_emitted,
        &mut reasoning_part_active,
        &mut usage,
        &mut content_started,
        &mut output_text_delta_parts,
        &mut refusal_delta_parts,
        &mut response_id,
        &mut tool_calls,
        &mut on_chunk,
    )
    .unwrap();

    assert_eq!(chunks.len(), 1);
    assert_eq!(chunks[0].kind, ChatStreamKind::ToolCall);
    assert_eq!(chunks[0].text, "ask_question");
}

#[test]
fn responses_tool_arguments_follow_output_item_ids() {
    let mut tool_calls = ResponsesToolAccumulator::default();
    for (item_id, call_id, name) in [
        ("item_a", "call_a", "first"),
        ("item_b", "call_b", "second"),
    ] {
        tool_calls.start(ResponsesStreamItem {
            kind: "function_call".to_string(),
            id: Some(item_id.to_string()),
            call_id: Some(call_id.to_string()),
            name: Some(name.to_string()),
            arguments: Some(String::new()),
        });
    }

    tool_calls.append_arguments(Some("item_a".to_string()), "{\"a\":".to_string());
    tool_calls.append_arguments(Some("item_b".to_string()), "{\"b\":2}".to_string());
    tool_calls.append_arguments(Some("item_a".to_string()), "1}".to_string());
    tool_calls.append_arguments(Some("unknown".to_string()), "ignored".to_string());

    let calls = tool_calls.finish();
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0].id, "call_a");
    assert_eq!(calls[0].function.arguments, r#"{"a":1}"#);
    assert_eq!(calls[1].id, "call_b");
    assert_eq!(calls[1].function.arguments, r#"{"b":2}"#);
}

#[test]
fn responses_stream_surfaces_refusal_text() {
    let output = run_responses_test_events(&[
        r#"data: {"type":"response.created","response":{"id":"resp_refusal"}}"#,
        r#"data: {"type":"response.refusal.delta","item_id":"msg_1","delta":"Cannot "}"#,
        r#"data: {"type":"response.refusal.delta","item_id":"msg_1","delta":"help"}"#,
        r#"data: {"type":"response.refusal.done","item_id":"msg_1","refusal":"Cannot help"}"#,
        r#"data: {"type":"response.completed","response":{"id":"resp_refusal"}}"#,
    ])
    .unwrap();

    assert!(output.terminal);
    assert_eq!(output.content, "Cannot help");
    assert_eq!(output.response_id.as_deref(), Some("resp_refusal"));
    assert_eq!(
        output
            .chunks
            .iter()
            .filter(|chunk| chunk.kind == ChatStreamKind::Content)
            .map(|chunk| chunk.text.as_str())
            .collect::<String>(),
        "Cannot help"
    );
}

#[test]
fn responses_stream_accepts_done_only_refusal() {
    let output = run_responses_test_events(&[
        r#"data: {"type":"response.refusal.done","item_id":"msg_1","refusal":"Cannot help"}"#,
        r#"data: {"type":"response.completed","response":{"id":"resp_refusal"}}"#,
    ])
    .unwrap();

    assert_eq!(output.content, "Cannot help");
}

#[test]
fn responses_stream_accepts_done_only_output_text() {
    let output = run_responses_test_events(&[
        r#"data: {"type":"response.output_text.delta","item_id":"msg_1","delta":""}"#,
        r#"data: {"type":"response.output_text.done","item_id":"msg_1","text":"final text"}"#,
        r#"data: {"type":"response.output_text.done","item_id":"msg_2","text":" second"}"#,
        r#"data: {"type":"response.completed","response":{"id":"resp_text"}}"#,
    ])
    .unwrap();

    assert_eq!(output.content, "final text second");
}

#[test]
fn responses_incomplete_is_not_a_successful_terminal_event() {
    let error = run_responses_test_events(&[r#"data: {"type":"response.incomplete","response":{"id":"resp_incomplete","incomplete_details":{"reason":"max_output_tokens"}}}"#])
        .unwrap_err();

    assert!(error.to_string().contains("max_output_tokens"), "{error:#}");
}

#[test]
fn responses_tool_calls_require_stateful_continuation() {
    let tool_call = ToolCall {
        id: "call_1".to_string(),
        kind: "function".to_string(),
        function: ToolCallFunction {
            name: "calc".to_string(),
            arguments: "{}".to_string(),
        },
    };

    let store_error = finalize_responses_stream_result(
        String::new(),
        String::new(),
        None,
        vec![tool_call.clone()],
        false,
        Some("resp_1".to_string()),
        true,
        false,
    )
    .unwrap_err();
    assert!(store_error.to_string().contains("store=false"));

    let id_error = finalize_responses_stream_result(
        String::new(),
        String::new(),
        None,
        vec![tool_call.clone()],
        false,
        None,
        false,
        false,
    )
    .unwrap_err();
    assert!(id_error.to_string().contains("without a response ID"));

    // 续传被记为不可用:带工具调用也不设 continuation(无状态全量回放),
    // 且不再要求 response_id。
    let suppressed = finalize_responses_stream_result(
        String::new(),
        String::new(),
        None,
        vec![tool_call],
        false,
        None,
        false,
        true,
    )
    .unwrap();
    assert!(suppressed.responses_continuation.is_none());
}

#[tokio::test]
async fn responses_store_false_rejects_tools_before_sending() {
    let mut provider = test_provider("responses-store-test", "http://127.0.0.1:1/v1");
    provider.protocol = "openai-responses".to_string();
    provider.default_model = "gpt-5".to_string();
    provider.extra_body = json!({"store": false}).as_object().cloned();
    let client = test_client(provider);
    let tools = vec![ToolDefinition {
        kind: "function",
        function: crate::llm::FunctionDefinition {
            name: "calc".to_string(),
            description: "calculate".to_string(),
            parameters: json!({"type": "object", "properties": {}}),
        },
    }];

    let error = client
        .chat_responses_stream(
            vec![ChatMessage::plain("user", "hi")],
            tools,
            None,
            "request-test",
            &mut |_| Ok(()),
        )
        .await
        .unwrap_err();

    assert!(error.to_string().contains("remove store=false"));
}

#[tokio::test]
async fn responses_stream_rejects_eof_without_terminal_event() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/v1", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        read_http_headers(&mut stream).await;
        write_http_sse_response(
            &mut stream,
            concat!(
                "data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_partial\"}}\n\n",
                "data: {\"type\":\"response.output_text.delta\",\"item_id\":\"msg_1\",\"delta\":\"partial\"}\n\n"
            ),
        )
        .await;
    });

    let mut provider = test_provider("responses-eof-test", &url);
    provider.protocol = "openai-responses".to_string();
    provider.default_model = "gpt-5".to_string();
    let client = test_client(provider);

    let error = client
        .chat_stream(vec![ChatMessage::plain("user", "hi")], Vec::new(), |_| {
            Ok(())
        })
        .await
        .unwrap_err();

    assert!(
        format!("{error:#}").contains("before a terminal event"),
        "{error:#}"
    );
    server.await.unwrap();
}

#[tokio::test]
async fn responses_continuation_is_pinned_to_its_original_endpoint() {
    let first_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let first_url = format!("http://{}/v1", first_listener.local_addr().unwrap());
    let second_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let second_url = format!("http://{}/v1", second_listener.local_addr().unwrap());
    let first_server = tokio::spawn(async move {
        tokio::time::timeout(Duration::from_millis(200), first_listener.accept())
            .await
            .is_ok()
    });
    let second_server = tokio::spawn(async move {
        let (mut first, _) = second_listener.accept().await.unwrap();
        read_http_headers(&mut first).await;
        write_http_sse_response(
            &mut first,
            concat!(
                "data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_1\"}}\n\n",
                "data: {\"type\":\"response.output_item.added\",\"item\":{\"type\":\"function_call\",\"id\":\"item_1\",\"call_id\":\"call_1\",\"name\":\"calc\",\"arguments\":\"{}\"}}\n\n",
                "data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"function_call\",\"id\":\"item_1\",\"call_id\":\"call_1\",\"name\":\"calc\",\"arguments\":\"{}\"}}\n\n",
                "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\"}}\n\n"
            ),
        )
        .await;

        let (mut second, _) = second_listener.accept().await.unwrap();
        read_http_headers(&mut second).await;
        write_http_sse_response(
            &mut second,
            concat!(
                "data: {\"type\":\"response.output_text.delta\",\"item_id\":\"msg_2\",\"delta\":\"continued\"}\n\n",
                "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_2\"}}\n\n"
            ),
        )
        .await;
    });

    let mut first_provider = test_provider("responses-shared", &first_url);
    first_provider.protocol = "openai-responses".to_string();
    first_provider.default_model = "gpt-5".to_string();
    let mut original_provider = test_provider("responses-shared", &second_url);
    original_provider.protocol = "openai-responses".to_string();
    original_provider.default_model = "gpt-5".to_string();
    let http_client = reqwest::Client::new();
    let original_endpoint = LlmEndpoint {
        client: http_client.clone(),
        provider: original_provider.clone(),
        api_key: "second".to_string(),
        key_index: 1,
    };
    let initial_client = OpenAiCompatibleClient {
        client: http_client.clone(),
        provider: original_provider.clone(),
        api_key: "second".to_string(),
        endpoints: Arc::new(vec![original_endpoint.clone()]),
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
    let initial_result = initial_client
        .chat_stream(vec![ChatMessage::plain("user", "hi")], Vec::new(), |_| {
            Ok(())
        })
        .await
        .unwrap();
    let continuation = initial_result
        .responses_continuation
        .as_deref()
        .unwrap()
        .clone();
    assert_eq!(continuation.endpoint_id, original_endpoint.id());

    let endpoints = vec![
        LlmEndpoint {
            client: http_client.clone(),
            provider: first_provider.clone(),
            api_key: "first".to_string(),
            key_index: 0,
        },
        original_endpoint,
    ];
    let client = OpenAiCompatibleClient {
        client: http_client,
        provider: first_provider,
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
        .chat_stream_with_continuation(
            vec![ChatMessage::tool("call_1", "tool result")],
            Vec::new(),
            Some(&continuation),
            |_| Ok(()),
        )
        .await
        .unwrap();

    assert_eq!(result.content, "continued");
    assert_eq!(result.provider_id.as_deref(), Some("responses-shared"));
    assert!(
        !first_server.await.unwrap(),
        "continuation used another endpoint"
    );
    second_server.await.unwrap();
}

#[test]
fn responses_summary_uses_auto_and_full_uses_detailed() {
    let mut config = AppConfig::default();
    assert!(!reasoning_summary_is_detailed(&config));

    config.display.reasoning = " FULL ".to_string();
    assert!(reasoning_summary_is_detailed(&config));

    let provider = test_provider("openai", "https://api.openai.com/v1");
    let mut client = test_client(provider);
    let reasoning = client.responses_reasoning().unwrap();
    assert_eq!(reasoning.summary.as_deref(), Some("auto"));

    client.detailed_reasoning_summary = true;
    let reasoning = client.responses_reasoning().unwrap();
    assert_eq!(reasoning.summary.as_deref(), Some("detailed"));
}

#[test]
fn test_responses_request_extra_body_flatten() {
    use serde_json::json;

    let extra = json!({
        "input": [],
        "previous_response_id": "wrong",
        "reasoning": {"effort": "high"},
        "reasoning_effort": "high",
        "parallel_tool_calls": false
    })
    .as_object()
    .cloned();

    let request = ResponsesRequest {
        model: "gpt-5".to_string(),
        input: vec![json!({"role": "user", "content": "Hello"})],
        instructions: None,
        previous_response_id: Some("resp_good".to_string()),
        stream: true,
        tools: None,
        reasoning: Some(ResponsesReasoning {
            effort: Some("medium".to_string()),
            summary: Some("concise".to_string()),
        }),
        temperature: Some(0.5),
        extra_body: sanitize_extra_body(extra, RESPONSES_RESERVED_BODY_KEYS),
    };

    let serialized = serde_json::to_string(&request).unwrap();
    let value = serde_json::to_value(&request).unwrap();

    assert_eq!(value["reasoning_effort"], "high");
    assert_eq!(value["parallel_tool_calls"], false);
    assert_eq!(value["model"], "gpt-5");
    assert_eq!(value["previous_response_id"], "resp_good");
    assert_eq!(value["reasoning"]["effort"], "medium");
    assert_eq!(value["temperature"], 0.5);
    assert!(value.get("extra_body").is_none());
    assert_eq!(serialized.matches("\"input\":").count(), 1);
    assert_eq!(serialized.matches("\"previous_response_id\":").count(), 1);
    assert_eq!(serialized.matches("\"reasoning\":").count(), 1);
}
