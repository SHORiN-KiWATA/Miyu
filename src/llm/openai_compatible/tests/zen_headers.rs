//! 发往 opencode Zen 的客户端识别头。
//!
//! 断言打在真实 HTTP 请求头上而不是构造函数上：这几个头的意义全在「服务端到底
//! 收没收到」，中间任何一层把它们吃掉都是 bug。

use super::shared::*;
use crate::llm::openai_compatible::*;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpListener;

fn header_value(head: &str, name: &str) -> Option<String> {
    head.lines()
        .filter_map(|line| line.split_once(':'))
        .find(|(key, _)| key.trim().eq_ignore_ascii_case(name))
        .map(|(_, value)| value.trim().to_string())
}

/// Zen 端点：五个头全发；换端点重试时 `x-opencode-request` 不变（它对应的是
/// 一次逻辑调用，不是一次 HTTP 请求）。供应商 id 被用户改过也照发——判定看
/// 打到哪个地址。
#[tokio::test]
async fn zen_requests_carry_the_opencode_client_headers() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!(
        "http://{}/v1/chat/completions",
        listener.local_addr().unwrap()
    );
    let server = tokio::spawn(async move {
        let mut heads = Vec::new();
        for status in [500, 200] {
            let (mut stream, _) = listener.accept().await.unwrap();
            heads.push(read_http_request_head(&mut stream).await);
            let body = if status == 200 { "ok" } else { "error" };
            let response = format!(
                "HTTP/1.1 {status} X\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(response.as_bytes()).await.unwrap();
        }
        heads
    });

    let client = test_client(test_provider("myopencode", OPENCODE_ZEN_BASE_URL));
    client
        .send_with_transport_retry("llm_1730000000000_7", "chat.send", || {
            client.client.post(&url)
        })
        .await
        .unwrap();
    let heads = server.await.unwrap();
    assert_eq!(heads.len(), 2, "500 之后应重试一次");

    let head = &heads[0];
    assert_eq!(
        header_value(head, "x-opencode-client").as_deref(),
        Some("cli")
    );
    assert_eq!(
        header_value(head, "x-opencode-project").as_deref(),
        Some("global")
    );
    assert_eq!(
        header_value(head, "user-agent").as_deref(),
        Some("opencode/1.18.29 ai-sdk/provider-utils/4.0.46 runtime/bun/1.4.0")
    );

    // 形状与抓包一致：前缀之后恒 26 位，字符集是 base62。
    for (name, prefix) in [
        ("x-opencode-session", "ses_"),
        ("x-opencode-request", "msg_"),
    ] {
        let value = header_value(head, name).unwrap_or_default();
        let suffix = value.strip_prefix(prefix).unwrap_or_else(|| {
            panic!("{name} 应以 {prefix} 开头，实际是 {value}");
        });
        assert_eq!(suffix.len(), 26, "{name} 前缀之后应是 26 位");
        assert!(
            suffix.chars().all(|ch| ch.is_ascii_alphanumeric()),
            "{name} 只该有 base62 字符，实际是 {value}"
        );
    }

    assert_eq!(
        header_value(&heads[0], "x-opencode-request"),
        header_value(&heads[1], "x-opencode-request"),
        "同一次逻辑调用重试时消息 id 不该变"
    );
    assert_eq!(
        header_value(&heads[0], "x-opencode-session"),
        header_value(&heads[1], "x-opencode-session"),
    );
}

/// 别的供应商一个识别头都不带——那是 Zen 的协议，拿去给别人只会平白泄漏
/// 我们在跑什么。
#[tokio::test]
async fn other_providers_get_no_opencode_headers() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!(
        "http://{}/v1/chat/completions",
        listener.local_addr().unwrap()
    );
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let head = read_http_request_head(&mut stream).await;
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok")
            .await
            .unwrap();
        head
    });

    let client = test_client(test_provider("deepseek", "https://api.deepseek.com/v1"));
    client
        .send_with_transport_retry("llm_1730000000000_8", "chat.send", || {
            client.client.post(&url)
        })
        .await
        .unwrap();

    let head = server.await.unwrap();
    assert!(
        !head.to_ascii_lowercase().contains("x-opencode-"),
        "非 Zen 端点不该出现任何 x-opencode-* 头：{head}"
    );
    assert!(
        !head.to_ascii_lowercase().contains("user-agent: opencode/"),
        "非 Zen 端点不该报成 opencode：{head}"
    );
}

/// 不同的逻辑调用要有不同的消息 id，否则服务端那边整段会话看着像同一条请求。
#[tokio::test]
async fn separate_calls_get_separate_message_ids() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!(
        "http://{}/v1/chat/completions",
        listener.local_addr().unwrap()
    );
    let server = tokio::spawn(async move {
        let mut heads = Vec::new();
        for _ in 0..2 {
            let (mut stream, _) = listener.accept().await.unwrap();
            heads.push(read_http_request_head(&mut stream).await);
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok")
                .await
                .unwrap();
        }
        heads
    });

    let client = test_client(test_provider("opencode", OPENCODE_ZEN_BASE_URL));
    for request_id in ["llm_1730000000000_1", "llm_1730000000000_2"] {
        client
            .send_with_transport_retry(request_id, "chat.send", || client.client.post(&url))
            .await
            .unwrap();
    }

    let heads = server.await.unwrap();
    assert_ne!(
        header_value(&heads[0], "x-opencode-request"),
        header_value(&heads[1], "x-opencode-request"),
    );
}
