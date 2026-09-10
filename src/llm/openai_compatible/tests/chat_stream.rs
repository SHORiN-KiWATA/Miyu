//! Chat Completions 协议的流式解析。

use super::shared::*;
use crate::llm::openai_compatible::*;

#[test]
fn strip_tagged_sections_handles_truncated_open_tag() {
    // 流被 finish_reason=length 截断在标签中间：恰以 `<system-reminder`
    // 结尾（无 `>`）曾触发 content_start 越界 panic。
    let text = "text before <system-reminder".to_string();
    assert_eq!(
        strip_tagged_sections(text, "system-reminder"),
        "text before "
    );
    let multibyte = "前文<system-reminder中".to_string();
    assert_eq!(strip_tagged_sections(multibyte, "system-reminder"), "前文");
    let normal = "a<system-reminder>hidden</system-reminder>b".to_string();
    assert_eq!(strip_tagged_sections(normal, "system-reminder"), "ab");
    let unclosed = "a<system-reminder>hidden".to_string();
    assert_eq!(strip_tagged_sections(unclosed, "system-reminder"), "a");
}

#[test]
fn tool_call_accumulators_drop_out_of_range_indices() {
    // A malformed upstream chunk with a huge index must not make the
    // accumulator allocate gigabytes (regression: 160GB VmSize).
    let mut acc = ToolCallAccumulator::default();
    let huge = ToolCallDelta {
        index: 1 << 30,
        id: Some("x".to_string()),
        kind: None,
        function: ToolCallFunctionDelta {
            name: Some("evil".to_string()),
            arguments: None,
        },
    };
    assert!(acc.push(huge).is_none());
    assert!(acc.calls.is_empty());
    let ok = ToolCallDelta {
        index: 0,
        id: Some("a".to_string()),
        kind: None,
        function: ToolCallFunctionDelta {
            name: Some("fine".to_string()),
            arguments: Some("{}".to_string()),
        },
    };
    assert!(acc.push(ok).is_some());
    assert_eq!(acc.calls.len(), 1);

    let mut anthropic = AnthropicToolAccumulator::default();
    assert!(anthropic
        .start(
            usize::MAX,
            AnthropicStreamBlock {
                kind: "tool_use".to_string(),
                id: Some("x".to_string()),
                name: Some("evil".to_string()),
                text: None,
                thinking: None,
            },
        )
        .is_none());
    anthropic.append_arguments(1 << 30, "{}".to_string());
    assert!(anthropic.calls.is_empty());
}

#[test]
fn stream_chunk_accepts_null_tool_calls() {
    let raw = r#"{"choices":[{"delta":{"content":"在","tool_calls":null}}]}"#;
    let parsed: ChatStreamResponse = serde_json::from_str(raw).unwrap();

    assert_eq!(parsed.choices.len(), 1);
    assert_eq!(parsed.choices[0].delta.content.as_deref(), Some("在"));
    assert!(parsed.choices[0].delta.tool_calls.is_empty());
}

#[test]
fn stream_chunk_accepts_taotoken_glm_nulls() {
    let raw = r#"{"created":1782742568,"usage":null,"model":"glm_for_coding","id":"9981f6121a31494387131c61bd2ad7a2","choices":[{"finish_reason":null,"matched_stop":null,"delta":{"role":null,"tool_calls":null,"content":"在","reasoning_content":null},"index":0,"logprobs":null}],"object":"chat.completion.chunk"}"#;
    let parsed: ChatStreamResponse = serde_json::from_str(raw).unwrap();

    assert!(parsed.usage.is_none());
    assert_eq!(parsed.choices.len(), 1);
    assert!(parsed.choices[0].finish_reason.is_none());
    assert_eq!(parsed.choices[0].delta.content.as_deref(), Some("在"));
    assert!(parsed.choices[0].delta.reasoning_content.is_none());
    assert!(parsed.choices[0].delta.tool_calls.is_empty());
}

#[test]
fn stream_chunk_emits_glm_reasoning_content() {
    let mut content = String::new();
    let mut content_emitted = 0usize;
    let mut reasoning = String::new();
    let mut reasoning_emitted = 0usize;
    let mut reasoning_part_active = false;
    let mut finish_reason = None;
    let mut usage = None;
    let mut tool_calls = ToolCallAccumulator::default();
    let mut chunks = Vec::new();
    let mut on_chunk = |chunk| {
        chunks.push(chunk);
        Ok(())
    };

    handle_sse_line(
        r#"data: {"choices":[{"finish_reason":"length","delta":{"reasoning_content":"先想一下","content":"","tool_calls":null}}]}"#,
        &mut content,
        &mut content_emitted,
        &mut reasoning,
        &mut reasoning_emitted,
        &mut reasoning_part_active,
        &mut finish_reason,
        &mut usage,
        &mut tool_calls,
        &mut on_chunk,
    )
    .unwrap();

    assert_eq!(chunks.len(), 2);
    assert_eq!(chunks[0].kind, ChatStreamKind::ReasoningPartStart);
    assert_eq!(chunks[1].kind, ChatStreamKind::Reasoning);
    assert_eq!(chunks[1].text, "先想一下");
    assert_eq!(finish_reason.as_deref(), Some("length"));
}

#[test]
fn chat_stream_announces_question_tool_before_arguments() {
    let mut content = String::new();
    let mut content_emitted = 0usize;
    let mut reasoning = String::new();
    let mut reasoning_emitted = 0usize;
    let mut reasoning_part_active = false;
    let mut finish_reason = None;
    let mut usage = None;
    let mut tool_calls = ToolCallAccumulator::default();
    let mut chunks = Vec::new();
    let mut on_chunk = |chunk| {
        chunks.push(chunk);
        Ok(())
    };

    handle_sse_line(
        r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"ask_question","arguments":""}}]}}]}"#,
        &mut content,
        &mut content_emitted,
        &mut reasoning,
        &mut reasoning_emitted,
        &mut reasoning_part_active,
        &mut finish_reason,
        &mut usage,
        &mut tool_calls,
        &mut on_chunk,
    )
    .unwrap();

    assert_eq!(chunks.len(), 1);
    assert_eq!(chunks[0].kind, ChatStreamKind::ToolCall);
    assert_eq!(chunks[0].text, "ask_question");
}

#[test]
fn chat_stream_surfaces_sse_error_objects() {
    let mut content = String::new();
    let mut content_emitted = 0usize;
    let mut reasoning = String::new();
    let mut reasoning_emitted = 0usize;
    let mut reasoning_part_active = false;
    let mut finish_reason = None;
    let mut usage = None;
    let mut tool_calls = ToolCallAccumulator::default();

    let error = handle_sse_line(
        r#"data: {"error":{"message":"upstream generation timed out"}}"#,
        &mut content,
        &mut content_emitted,
        &mut reasoning,
        &mut reasoning_emitted,
        &mut reasoning_part_active,
        &mut finish_reason,
        &mut usage,
        &mut tool_calls,
        &mut |_| Ok(()),
    )
    .unwrap_err();

    assert!(format!("{error:#}").contains("upstream generation timed out"));
}

#[test]
fn reasoning_only_stream_result_is_preserved() {
    let result = finalize_stream_result(
        String::new(),
        "完整思考内容".to_string(),
        None,
        Vec::new(),
        false,
    )
    .unwrap();

    assert!(result.content.is_empty());
    assert_eq!(result.reasoning.as_deref(), Some("完整思考内容"));
}

#[test]
fn fully_empty_stream_result_is_rejected() {
    let error =
        finalize_stream_result(String::new(), String::new(), None, Vec::new(), false).unwrap_err();

    let message = format!("{error:#}");
    assert!(message.contains("流式响应为空") || message.contains("stream response was empty"));
}

#[test]
fn sse_buffer_preserves_utf8_split_across_byte_chunks() {
    let line = r#"data: {"choices":[{"delta":{"content":"等","tool_calls":null}}]}"#;
    let split = line.find("等").unwrap() + 1;
    let mut buffer = Utf8LineBuffer::default();

    assert!(buffer.push(&line.as_bytes()[..split]).unwrap().is_empty());
    let lines = buffer.push(&line.as_bytes()[split..]).unwrap();

    assert!(lines.is_empty());
    assert_eq!(buffer.finish().unwrap(), vec![line]);
}

#[test]
fn previous_lossy_chunk_decode_corrupts_split_utf8() {
    let text = "等";
    let mut decoded = String::new();

    decoded.push_str(&String::from_utf8_lossy(&text.as_bytes()[..1]));
    decoded.push_str(&String::from_utf8_lossy(&text.as_bytes()[1..]));

    assert_eq!(decoded, "���");
}

#[test]
fn taotoken_glm_request_enables_thinking() {
    let mut provider = test_provider("taotoken", "https://taotoken.net/api/v1");
    provider.default_model = "glm_for_coding".to_string();

    assert!(
        taotoken_glm_chat_template_kwargs(&provider).is_some_and(|kwargs| kwargs.enable_thinking)
    );
}

#[test]
fn non_taotoken_glm_request_keeps_default_body() {
    let mut provider = test_provider("local", "http://localhost:11434/v1");
    provider.default_model = "glm-5".to_string();

    assert!(taotoken_glm_chat_template_kwargs(&provider).is_none());
}

#[test]
fn chat_request_includes_stream_usage_options() {
    let request = ChatRequest {
        model: "model".to_string(),
        messages: vec![ChatMessage::plain("user", "hi")],
        temperature: 0.0,
        stream: true,
        stream_options: Some(ChatStreamOptions {
            include_usage: true,
        }),
        max_tokens: None,
        tools: None,
        chat_template_kwargs: None,
        extra_body: None,
    };

    let value = serde_json::to_value(request).unwrap();

    assert_eq!(value["stream_options"]["include_usage"], true);
}

#[test]
fn stream_options_unsupported_detects_retryable_error() {
    assert!(stream_options_unsupported(
        400,
        "unknown parameter: stream_options"
    ));
    assert!(stream_options_unsupported(
        422,
        "stream_options is not supported"
    ));
    assert!(!stream_options_unsupported(403, "stream_options forbidden"));
    assert!(!stream_options_unsupported(400, "invalid api key"));
}

#[test]
fn openai_tool_schema_flattens_top_level_any_of() {
    let schema = json!({
        "anyOf": [
            {"type":"object","properties":{"path":{"type":"string"}},"required":["path"]},
            {"type":"object","properties":{"resource":{"anyOf":[{"type":"string"},{"type":"null"}]}},"required":["resource"]}
        ]
    });

    let normalized = openai_tool_input_schema(schema);

    assert_eq!(normalized["type"], "object");
    assert_eq!(normalized["additionalProperties"], false);
    assert_eq!(normalized["properties"]["path"]["type"], "string");
    assert_eq!(normalized["properties"]["resource"]["type"], "string");
    assert!(normalized.get("anyOf").is_none());
}

#[test]
fn stream_filter_skips_split_system_reminder() {
    let mut content = String::new();
    let mut emitted = 0usize;
    let mut chunks = Vec::new();
    let mut on_chunk = |chunk| {
        chunks.push(chunk);
        Ok(())
    };

    push_buffered_chunk(
        &mut content,
        &mut emitted,
        ChatStreamKind::Content,
        "hello <system-rem".to_string(),
        &mut on_chunk,
    )
    .unwrap();
    push_buffered_chunk(
        &mut content,
        &mut emitted,
        ChatStreamKind::Content,
        "inder>hidden</system-reminder> world".to_string(),
        &mut on_chunk,
    )
    .unwrap();

    assert_eq!(chunks.len(), 2);
    assert_eq!(chunks[0].text, "hello ");
    assert_eq!(chunks[1].text, " world");
}

#[test]
fn stream_filter_skips_underscore_system_reminder() {
    let mut content = String::new();
    let mut emitted = 0usize;
    let mut chunks = Vec::new();
    let mut on_chunk = |chunk| {
        chunks.push(chunk);
        Ok(())
    };

    push_buffered_chunk(
        &mut content,
        &mut emitted,
        ChatStreamKind::Content,
        "a<system_reminder>hidden</system_reminder>b".to_string(),
        &mut on_chunk,
    )
    .unwrap();

    assert_eq!(chunks.len(), 2);
    assert_eq!(chunks[0].text, "a");
    assert_eq!(chunks[1].text, "b");
}

#[test]
fn test_chat_request_extra_body_flatten() {
    use serde_json::json;

    let extra = json!({
        "model": "override",
        "messages": [],
        "enable_thinking": false,
        "custom_param": "value"
    })
    .as_object()
    .cloned();

    let request = ChatRequest {
        model: "gpt-4".to_string(),
        messages: vec![ChatMessage::plain("user", "Hello")],
        temperature: 0.7,
        stream: true,
        stream_options: Some(ChatStreamOptions {
            include_usage: true,
        }),
        max_tokens: None,
        tools: None,
        chat_template_kwargs: None,
        extra_body: sanitize_extra_body(extra, CHAT_RESERVED_BODY_KEYS),
    };

    let serialized = serde_json::to_string(&request).unwrap();
    let value = serde_json::to_value(&request).unwrap();

    assert_eq!(value["enable_thinking"], false);
    assert_eq!(value["custom_param"], "value");
    assert_eq!(value["model"], "gpt-4");
    let temp = value["temperature"].as_f64().unwrap();
    assert!((temp - 0.7).abs() < 1e-6);
    assert!(value.get("extra_body").is_none());
    assert_eq!(serialized.matches("\"model\":").count(), 1);
    assert_eq!(serialized.matches("\"messages\":").count(), 1);
}

/// 09-10 终端截图:正文被拆成一行一段。供应商在每个正文 delta 上附带
/// `reasoning_content: ""`,若把空串当一段推理,每个正文分片都会被包上一对
/// ReasoningPartStart/End,终端每收到一次分段开始就截断正文行。
#[test]
fn empty_reasoning_field_beside_content_does_not_open_a_reasoning_part() {
    let mut content = String::new();
    let mut content_emitted = 0usize;
    let mut reasoning = String::new();
    let mut reasoning_emitted = 0usize;
    let mut reasoning_part_active = false;
    let mut finish_reason = None;
    let mut usage = None;
    let mut tool_calls = ToolCallAccumulator::default();
    let mut chunks = Vec::new();
    let mut on_chunk = |chunk| {
        chunks.push(chunk);
        Ok(())
    };

    for line in [
        r#"data: {"choices":[{"delta":{"reasoning_content":"先想","content":null}}]}"#,
        r#"data: {"choices":[{"delta":{"reasoning_content":"","content":"问我的模型"}}]}"#,
        r#"data: {"choices":[{"delta":{"reasoning_content":"","content":"的话"}}]}"#,
    ] {
        handle_sse_line(
            line,
            &mut content,
            &mut content_emitted,
            &mut reasoning,
            &mut reasoning_emitted,
            &mut reasoning_part_active,
            &mut finish_reason,
            &mut usage,
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
            ChatStreamKind::Content,
            ChatStreamKind::Content,
        ],
        "空 reasoning 字段不能再开一段:正文分片之间不该夹分段事件"
    );
    assert!(!reasoning_part_active);
    assert_eq!(content, "问我的模型的话");
}
