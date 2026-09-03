//! LLM 客户端测试共用的 fixture：假 HTTP/SSE 端、限流端点、测试客户端。

use crate::llm::openai_compatible::*;
use std::sync::atomic::AtomicUsize;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

pub(super) fn run_responses_test_events(lines: &[&str]) -> Result<ResponsesTestOutput> {
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
    let mut terminal = false;
    let mut on_chunk = |chunk| {
        chunks.push(chunk);
        Ok(())
    };
    for line in lines {
        terminal = handle_responses_sse_line(
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
        )?;
        if terminal {
            break;
        }
    }
    Ok(ResponsesTestOutput {
        content,
        chunks,
        response_id,
        terminal,
    })
}

/// Writes an SSE body and then hangs up without `[DONE]`, the way a proxy
/// that drops the connection mid-generation does.
pub(super) async fn write_truncated_sse_response(stream: &mut tokio::net::TcpStream, body: &str) {
    // No Content-Length and no terminating chunk: the peer sees the socket
    // close, which is exactly the "graceful close mid-stream" case.
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n{body}"
    );
    stream.write_all(response.as_bytes()).await.unwrap();
    stream.flush().await.unwrap();
    stream.shutdown().await.unwrap();
}

pub(super) async fn read_http_headers(stream: &mut tokio::net::TcpStream) {
    let mut request = Vec::new();
    let mut byte = [0u8; 1];
    while !request.ends_with(b"\r\n\r\n") {
        let read = stream.read(&mut byte).await.unwrap();
        assert_ne!(read, 0, "connection closed before request headers");
        request.push(byte[0]);
    }
}

pub(super) async fn write_http_sse_response(stream: &mut tokio::net::TcpStream, body: &str) {
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    );
    stream.write_all(response.as_bytes()).await.unwrap();
}

/// Serves an endless stream of opencode-zen-shaped 429s, counting hits.
/// The listener is bound before the task is spawned: `#[tokio::test]` runs
/// a current-thread runtime, so handing the address back over a blocking
/// channel would deadlock the only thread that could serve it.
pub(super) async fn spawn_rate_limited_endpoint(
) -> (String, Arc<AtomicUsize>, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/v1", listener.local_addr().unwrap());
    let hits = Arc::new(AtomicUsize::new(0));
    let counter = hits.clone();
    let server = tokio::spawn(async move {
        loop {
            let (mut stream, _) = listener.accept().await.unwrap();
            read_http_headers(&mut stream).await;
            counter.fetch_add(1, Ordering::SeqCst);
            let body = concat!(
                r#"{"type":"error","error":{"type":"FreeUsageLimitError","#,
                r#""message":"Error from provider (Console): Rate limit exceeded."}}"#
            );
            stream
                .write_all(
                    format!(
                        "HTTP/1.1 429 Too Many Requests\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
        }
    });
    (url, hits, server)
}

pub(super) fn rate_limit_test_endpoint(id: &str, url: &str) -> LlmEndpoint {
    let mut provider = test_provider(id, url);
    provider.protocol = "openai-chat".to_string();
    provider.default_model = "big-pickle".to_string();
    LlmEndpoint {
        client: reqwest::Client::new(),
        provider,
        api_key: "public".to_string(),
        key_index: 0,
    }
}

pub(super) fn client_over(endpoints: Vec<LlmEndpoint>) -> OpenAiCompatibleClient {
    let first = endpoints[0].clone();
    OpenAiCompatibleClient {
        client: first.client.clone(),
        provider: first.provider.clone(),
        api_key: first.api_key.clone(),
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
    }
}

pub(super) fn test_client(provider: ProviderConfig) -> OpenAiCompatibleClient {
    let client = reqwest::Client::new();
    let endpoint = LlmEndpoint {
        client: client.clone(),
        provider: provider.clone(),
        api_key: "test".to_string(),
        key_index: 0,
    };
    OpenAiCompatibleClient {
        client,
        provider,
        api_key: "test".to_string(),
        endpoints: Arc::new(vec![endpoint]),
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
    }
}

pub(super) fn test_paths(root: &std::path::Path) -> MiyuPaths {
    MiyuPaths {
        root_dir: root.to_path_buf(),
        config_dir: root.join("config"),
        config_file: root.join("config/config.jsonc"),
        skills_dir: root.join("config/skills"),
        data_dir: root.join("data"),
        cache_dir: root.join("cache"),
        state_dir: root.join("state"),
        pictures_dir: root.join("pictures"),
        fish_hook_file: root.join("fish/miyu.fish"),
        bash_hook_file: root.join("shell/bash-hook.sh"),
        zsh_hook_file: root.join("shell/zsh-hook.zsh"),
        scripts_dir: root.join("config/scripts"),
        system_scripts_dir: root.join("system/scripts"),
    }
}

pub(super) fn test_provider(id: &str, base_url: &str) -> ProviderConfig {
    ProviderConfig {
        enabled: true,
        id: id.to_string(),
        display_name: id.to_string(),
        base_url: base_url.to_string(),
        protocol: "auto".to_string(),
        api_key: None,
        models: Vec::new(),
        model_context_window: std::collections::HashMap::new(),
        model_temperature: std::collections::HashMap::new(),
        model_tools_loading_mode: std::collections::HashMap::new(),
        model_modalities: std::collections::HashMap::new(),
        tool_result_media: None,
        model_costs: std::collections::HashMap::new(),
        default_model: String::new(),
        timeout_seconds: 60,
        temperature: 1.0,
        anthropic_max_tokens: 4096,
        extra_body: None,
    }
}

#[derive(Debug)]
pub(super) struct ResponsesTestOutput {
    pub(super) content: String,
    pub(super) chunks: Vec<ChatStreamChunk>,
    pub(super) response_id: Option<String>,
    pub(super) terminal: bool,
}
