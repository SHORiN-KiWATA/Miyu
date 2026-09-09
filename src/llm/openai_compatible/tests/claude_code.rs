//! claude-code 中转协议:假 `claude` 脚本回放 stream-json,验证参数拼装、
//! 载荷翻译、流事件→chunk、用量归一与会话续传。不碰真实订阅。

use crate::llm::openai_compatible::tests::shared::*;
use crate::llm::openai_compatible::*;
use crate::llm::{ChatMessage, ChatStreamKind};

fn fake_claude_script(dir: &std::path::Path) -> std::path::PathBuf {
    let script = dir.join("claude");
    // 记录 argv 与 stdin,再回放一段固定的 stream-json;带 --resume 时在
    // 结果帧里沿用同一个会话 id(真 CLI 的行为)。
    std::fs::write(
        &script,
        r#"#!/usr/bin/env bash
dir="$(cd "$(dirname "$0")" && pwd)"
# 会话 id 按测试目录唯一:真实 claude 的会话 id 是全局唯一 UUID,共用一个
# 假 id 会让并行测试在全局会话映射里互相顶掉(按会话 id 去重)。
sid="sess-$(basename "$dir")"
printf '%s\n' "$@" > "$dir/args.txt"
cat > "$dir/stdin.txt"
if [ -f "$dir/fail.txt" ]; then
  echo "{\"type\":\"result\",\"subtype\":\"error_during_execution\",\"is_error\":true,\"session_id\":\"$sid\",\"result\":\"Claude AI usage limit reached\"}"
  exit 0
fi
echo "{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"$sid\"}"
echo '{"type":"stream_event","event":{"type":"message_start","message":{"usage":{"input_tokens":10,"output_tokens":1}}}}'
echo '{"type":"stream_event","event":{"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}}'
echo '{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"pondering"}}}'
echo '{"type":"stream_event","event":{"type":"content_block_stop","index":0}}'
echo '{"type":"stream_event","event":{"type":"content_block_start","index":1,"content_block":{"type":"text","text":""}}}'
echo '{"type":"stream_event","event":{"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"Hello from fake"}}}'
echo '{"type":"stream_event","event":{"type":"content_block_stop","index":1}}'
echo '{"type":"stream_event","event":{"type":"content_block_start","index":2,"content_block":{"type":"tool_use","id":"toolu_1","name":"mcp__miyu__use_meme","input":{}}}}'
echo '{"type":"stream_event","event":{"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":"{\"action\":\"show\""}}}'
echo '{"type":"stream_event","event":{"type":"content_block_start","index":3,"content_block":{"type":"tool_use","id":"toolu_q","name":"mcp__miyu__ask_question","input":{}}}}'
echo '{"type":"stream_event","event":{"type":"content_block_stop","index":2}}'
echo '{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","id":"toolu_1","name":"mcp__miyu__use_meme","input":{"action":"show","id":"m1"}}]}}'
echo '{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_1","content":[{"type":"text","text":"meme sent ok"}]}]}}'
echo '{"type":"stream_event","event":{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"input_tokens":10,"output_tokens":5}}}'
echo "{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"session_id\":\"$sid\",\"result\":\"Hello from fake\",\"usage\":{\"input_tokens\":10,\"cache_read_input_tokens\":90,\"cache_creation_input_tokens\":20,\"output_tokens\":5,\"output_tokens_details\":{\"thinking_tokens\":2}}}"
"#,
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    script
}

fn claude_code_client(dir: &std::path::Path, provider_id: &str) -> OpenAiCompatibleClient {
    let mut provider = test_provider(provider_id, "");
    provider.protocol = "claude-code".to_string();
    provider.default_model = "haiku".to_string();
    let mut client = test_client(provider);
    client.claude_code = Some(Arc::new(ClaudeCodeRuntime {
        binary: fake_claude_script(dir),
        native_tools: "off".to_string(),
        miyu_tools: "off".to_string(),
        permission_mode: "bypassPermissions".to_string(),
        autocompact: HashMap::new(),
        idle_timeout: Duration::from_secs(30),
        prefer_subscription: true,
    }));
    client
}

fn read(dir: &std::path::Path, name: &str) -> String {
    std::fs::read_to_string(dir.join(name)).unwrap_or_default()
}

/// 首轮:全量参数拼装 + 流事件翻译 + Anthropic 口径用量归一。
#[tokio::test]
async fn first_turn_spawns_fresh_session_with_full_flags() {
    let dir = tempfile::tempdir().unwrap();
    let client = claude_code_client(dir.path(), "cc-first");
    let mut chunks = Vec::new();
    let result = client
        .chat_stream_inner(
            vec![
                ChatMessage::system("persona prompt"),
                ChatMessage::plain("user", "hello"),
            ],
            Vec::new(),
            None,
            false,
            &mut |chunk| {
                chunks.push(chunk);
                Ok(())
            },
        )
        .await
        .unwrap();

    assert_eq!(result.content, "Hello from fake");
    assert_eq!(result.reasoning.as_deref(), Some("pondering"));
    assert!(result.tool_calls.is_empty());
    assert_eq!(result.finish_reason.as_deref(), Some("stop"));
    // 结果帧优先:prompt = 10 + 90(读) + 20(写) = 120。
    let usage = result.usage.unwrap();
    assert_eq!(usage.prompt_tokens, 120);
    assert_eq!(usage.completion_tokens, 5);
    assert_eq!(usage.cache_read_tokens, 90);
    assert_eq!(usage.cache_write_tokens, 20);
    assert_eq!(usage.reasoning_tokens, 2);
    assert!(usage.cache_reported);
    assert!(chunks
        .iter()
        .any(|chunk| chunk.kind == ChatStreamKind::Content && chunk.text.contains("Hello")));
    assert!(chunks
        .iter()
        .any(|chunk| chunk.kind == ChatStreamKind::Reasoning && chunk.text.contains("pondering")));
    // 工具块开始(入参还在流)→ RemoteToolPreparing,先于完整帧翻出的 started;
    // 名字剥前缀,首个不算批量;桥问答(ask_question)不发——它有自己的
    // question.* 事件,发了「准备问题」会黏住。
    let preparing: Vec<serde_json::Value> = chunks
        .iter()
        .filter(|chunk| chunk.kind == ChatStreamKind::RemoteToolPreparing)
        .map(|chunk| serde_json::from_str(&chunk.text).unwrap())
        .collect();
    assert_eq!(preparing.len(), 1, "{preparing:?}");
    assert_eq!(preparing[0]["name"], "use_meme");
    assert_eq!(preparing[0]["batch"], false);
    let preparing_at = chunks
        .iter()
        .position(|chunk| chunk.kind == ChatStreamKind::RemoteToolPreparing)
        .unwrap();
    let started_at = chunks
        .iter()
        .position(|chunk| chunk.kind == ChatStreamKind::RemoteToolStarted)
        .unwrap();
    assert!(preparing_at < started_at, "准备提示必须先于卡片");
    // claude 侧工具活动翻成标准卡片事件:started 带剥前缀的名字与入参,
    // finished 带 ok 与结果文本。
    let started = chunks
        .iter()
        .find(|chunk| chunk.kind == ChatStreamKind::RemoteToolStarted)
        .expect("tool_use 帧应翻成 RemoteToolStarted");
    let payload: serde_json::Value = serde_json::from_str(&started.text).unwrap();
    assert_eq!(payload["name"], "use_meme");
    assert_eq!(payload["id"], "toolu_1");
    assert_eq!(payload["input"]["action"], "show");
    let finished = chunks
        .iter()
        .find(|chunk| chunk.kind == ChatStreamKind::RemoteToolFinished)
        .expect("tool_result 帧应翻成 RemoteToolFinished");
    let payload: serde_json::Value = serde_json::from_str(&finished.text).unwrap();
    assert_eq!(payload["name"], "use_meme");
    assert_eq!(payload["ok"], true);
    assert_eq!(payload["output"], "meme sent ok");

    let args = read(dir.path(), "args.txt");
    assert!(args.contains("--model\nhaiku"), "args: {args}");
    assert!(
        args.contains("--system-prompt\npersona prompt"),
        "args: {args}"
    );
    assert!(args.contains("--tools\n\n"), "内置工具应整体关闭: {args}");
    assert!(args.contains("--strict-mcp-config"), "args: {args}");
    assert!(!args.contains("--resume"), "首轮不该续传: {args}");
    // 无会话作用域(测试环境)⇒ 不挂 MCP 桥。
    assert!(!args.contains("--mcp-config"), "args: {args}");
    let stdin: serde_json::Value =
        serde_json::from_str(read(dir.path(), "stdin.txt").trim()).unwrap();
    assert_eq!(stdin["type"], "user");
    assert_eq!(stdin["message"]["content"][0]["text"], "hello");
}

/// 次轮 append-only 延伸:命中前缀链 ⇒ --resume 同一会话,stdin 只带增量。
#[tokio::test]
async fn appended_turn_resumes_and_sends_only_the_delta() {
    let dir = tempfile::tempdir().unwrap();
    let client = claude_code_client(dir.path(), "cc-resume");
    let system = ChatMessage::system("persona prompt");
    let turn1_user = ChatMessage::plain("user", "hello");
    client
        .chat_stream_inner(
            vec![system.clone(), turn1_user.clone()],
            Vec::new(),
            None,
            false,
            &mut |_| Ok(()),
        )
        .await
        .unwrap();

    // 化石回放形态:assistant 正文 + 新一轮输入,前缀与预测严格一致。
    let history = vec![
        system.clone(),
        turn1_user.clone(),
        ChatMessage::assistant("Hello from fake", None),
        ChatMessage::plain("user", "and again"),
    ];
    client
        .chat_stream_inner(history, Vec::new(), None, false, &mut |_| Ok(()))
        .await
        .unwrap();
    let args = read(dir.path(), "args.txt");
    assert!(args.contains("--resume\nsess-"), "应续传: {args}");
    let stdin = read(dir.path(), "stdin.txt");
    assert!(stdin.contains("and again"), "stdin: {stdin}");
    assert!(!stdin.contains("hello"), "增量不该重放首轮输入: {stdin}");
    assert!(!stdin.contains("conversation-history"), "stdin: {stdin}");

    // 改写历史(redo 形态):匹配不上 ⇒ 重开会话,全量转写重放。
    let rewritten = vec![
        system,
        turn1_user,
        ChatMessage::assistant("a different reply", None),
        ChatMessage::plain("user", "third"),
    ];
    client
        .chat_stream_inner(rewritten, Vec::new(), None, false, &mut |_| Ok(()))
        .await
        .unwrap();
    let args = read(dir.path(), "args.txt");
    assert!(!args.contains("--resume"), "改写历史后不该续传: {args}");
    let stdin = read(dir.path(), "stdin.txt");
    assert!(
        stdin.contains("conversation-history") && stdin.contains("a different reply"),
        "全量重放应带历史转写: {stdin}"
    );
}

/// 订阅限流错误要翻译成端点调度认识的 429/RateLimit(冷却+可转移)。
#[tokio::test]
async fn usage_limit_failures_classify_as_rate_limit() {
    let dir = tempfile::tempdir().unwrap();
    let client = claude_code_client(dir.path(), "cc-limit");
    std::fs::write(dir.path().join("fail.txt"), "1").unwrap();
    let error = client
        .chat_claude_code_stream(
            vec![ChatMessage::plain("user", "hi")],
            Vec::new(),
            "req-test",
            &mut |_| Ok(()),
        )
        .await
        .unwrap_err();
    let failure = error
        .downcast_ref::<HttpStatusFailure>()
        .expect("usage limit should classify as an HTTP-style failure");
    assert_eq!(failure.kind, HttpFailureKind::RateLimit);
    assert_eq!(failure.status, 429);
}

/// binary 缺失要报可操作的错误,而不是裸 ENOENT。
#[tokio::test]
async fn missing_binary_reports_actionable_error() {
    let dir = tempfile::tempdir().unwrap();
    let mut client = claude_code_client(dir.path(), "cc-missing");
    client.claude_code = Some(Arc::new(ClaudeCodeRuntime {
        binary: dir.path().join("no-such-claude"),
        native_tools: "off".to_string(),
        miyu_tools: "off".to_string(),
        permission_mode: "bypassPermissions".to_string(),
        autocompact: HashMap::new(),
        idle_timeout: Duration::from_secs(30),
        prefer_subscription: true,
    }));
    let error = client
        .chat_claude_code_stream(
            vec![ChatMessage::plain("user", "hi")],
            Vec::new(),
            "req-test",
            &mut |_| Ok(()),
        )
        .await
        .unwrap_err();
    let text = format!("{error:#}");
    assert!(
        text.contains("plugins.claude_code.binary"),
        "应指路配置项: {text}"
    );
}

/// 思考档:thinking-variant 选择映射成 CLI 的 --effort(用 opus 测——
/// haiku 不支持思考力度调整,已无档位,见下一条)。
#[tokio::test]
async fn selected_thinking_variant_maps_to_effort_flag() {
    let dir = tempfile::tempdir().unwrap();
    let mut client = claude_code_client(dir.path(), "cc-effort");
    client.provider.default_model = "opus".to_string();
    client.thinking_variants.insert(
        thinking_variant_key("cc-effort", "opus"),
        "high".to_string(),
    );
    client
        .chat_claude_code_stream(
            vec![ChatMessage::plain("user", "hi")],
            Vec::new(),
            "req-test",
            &mut |_| Ok(()),
        )
        .await
        .unwrap();
    let args = read(dir.path(), "args.txt");
    assert!(args.contains("--effort\nhigh"), "args: {args}");
}

/// haiku 不支持思考力度调整:无档位,残留的档位选择也不产生 --effort。
#[tokio::test]
async fn haiku_has_no_thinking_variants_and_sends_no_effort() {
    let dir = tempfile::tempdir().unwrap();
    let mut client = claude_code_client(dir.path(), "cc-haiku-effort");
    client.thinking_variants.insert(
        thinking_variant_key("cc-haiku-effort", "haiku"),
        "high".to_string(),
    );
    client
        .chat_claude_code_stream(
            vec![ChatMessage::plain("user", "hi")],
            Vec::new(),
            "req-test",
            &mut |_| Ok(()),
        )
        .await
        .unwrap();
    let args = read(dir.path(), "args.txt");
    assert!(!args.contains("--effort"), "args: {args}");
}

/// 未启用的供应商在端点池装配时给出可操作的报错。
#[tokio::test]
async fn disabled_provider_is_rejected_at_pool_assembly() {
    let temp = tempfile::tempdir().unwrap();
    let paths = test_paths(temp.path());
    let mut config = AppConfig::default();
    config.normalize_builtin_providers();
    // 唯一激活模型指向默认禁用的 claude-code。
    config.active_provider_models = Some(vec![crate::config::ActiveProviderModelConfig {
        provider_id: "claude-code".to_string(),
        model: "sonnet".to_string(),
    }]);
    let choices = vec![crate::config::ProviderModelChoice {
        provider_id: "claude-code".to_string(),
        provider_name: "Claude Code".to_string(),
        model: "sonnet".to_string(),
    }];
    let error = match OpenAiCompatibleClient::from_choices(&config, &paths, &choices) {
        Ok(_) => panic!("禁用的供应商不该装配出端点池"),
        Err(error) => error,
    };
    let text = format!("{error:#}");
    assert!(
        text.contains("disabled") || text.contains("未启用"),
        "{text}"
    );
}

/// 双四档作用域:原生工具开 ⇒ 换 --permission-mode 且不再关内置工具;
/// 关 ⇒ --tools "";dev 档随客户端的会话模式旗标翻转。
#[tokio::test]
async fn native_tool_scope_shapes_the_cli_args() {
    let dir = tempfile::tempdir().unwrap();
    let script = fake_claude_script(dir.path());
    let runtime = |scope: &str| {
        Arc::new(ClaudeCodeRuntime {
            binary: script.clone(),
            native_tools: scope.to_string(),
            miyu_tools: "off".to_string(),
            permission_mode: "bypassPermissions".to_string(),
            autocompact: HashMap::new(),
            idle_timeout: Duration::from_secs(30),
            prefer_subscription: true,
        })
    };
    let run = |client: OpenAiCompatibleClient| async move {
        client
            .chat_claude_code_stream(
                vec![ChatMessage::plain("user", "hi")],
                Vec::new(),
                "req-test",
                &mut |_| Ok(()),
            )
            .await
            .unwrap();
    };

    // all:普通会话也开原生工具。
    let mut client = claude_code_client(dir.path(), "cc-scope-all");
    client.claude_code = Some(runtime("all"));
    run(client).await;
    let args = read(dir.path(), "args.txt");
    assert!(
        args.contains("--permission-mode\nbypassPermissions"),
        "{args}"
    );
    assert!(
        !args.contains("--tools"),
        "原生工具开时不该关内置工具: {args}"
    );
    assert!(args.contains("--strict-mcp-config"), "{args}");

    // dev 档 + 普通会话:原生工具关。
    let mut client = claude_code_client(dir.path(), "cc-scope-dev-off");
    client.claude_code = Some(runtime("dev"));
    run(client).await;
    let args = read(dir.path(), "args.txt");
    assert!(args.contains("--tools\n\n"), "{args}");
    assert!(!args.contains("--permission-mode"), "{args}");

    // dev 档 + dev 会话:原生工具开。
    let mut client = claude_code_client(dir.path(), "cc-scope-dev-on");
    client.claude_code = Some(runtime("dev"));
    let client = client.with_claude_code_dev_mode(true);
    run(client).await;
    let args = read(dir.path(), "args.txt");
    assert!(
        args.contains("--permission-mode\nbypassPermissions"),
        "{args}"
    );

    // normal 档 + dev 会话:原生工具关。
    let mut client = claude_code_client(dir.path(), "cc-scope-normal");
    client.claude_code = Some(runtime("normal"));
    let client = client.with_claude_code_dev_mode(true);
    run(client).await;
    let args = read(dir.path(), "args.txt");
    assert!(args.contains("--tools\n\n"), "{args}");
}

/// 两套同开时,MCP 桥的 env 里点名剔除与原生重复的 Miyu 工具。
#[tokio::test]
async fn duplicate_miyu_tools_are_excluded_when_both_toolsets_are_on() {
    let dir = tempfile::tempdir().unwrap();
    let mut client = claude_code_client(dir.path(), "cc-dedupe");
    client.claude_code = Some(Arc::new(ClaudeCodeRuntime {
        binary: fake_claude_script(dir.path()),
        native_tools: "all".to_string(),
        miyu_tools: "all".to_string(),
        permission_mode: "bypassPermissions".to_string(),
        autocompact: HashMap::new(),
        idle_timeout: Duration::from_secs(30),
        prefer_subscription: true,
    }));
    // MCP 桥要求会话作用域(env 里带 MIYU_SESSION)。
    crate::tools::workspace::with_session(std::sync::Arc::from("sess-dedupe"), async {
        client
            .chat_claude_code_stream(
                vec![ChatMessage::plain("user", "hi")],
                Vec::new(),
                "req-test",
                &mut |_| Ok(()),
            )
            .await
            .unwrap();
    })
    .await;
    let args = read(dir.path(), "args.txt");
    assert!(args.contains("--mcp-config"), "{args}");
    assert!(
        args.contains("MIYU_MCP_EXCLUDE") && args.contains("run_command"),
        "两套同开应点名剔除重复工具: {args}"
    );
    assert!(
        args.contains("glob") && args.contains("todowrite"),
        "glob/grep/todowrite 也在剔除表: {args}"
    );
    // 09-09:edit 加入剔除表。原生 Edit/Write 覆盖同一个域(edit 只管文件
    // 系统,kb:/artifact: 是另外两件工具),而中转线上它连 diff 都渲染不出来
    // ——patch_preview 走 progress 的 Message 事件,桥不转发这一类。
    assert!(
        args.contains("edit"),
        "edit 与原生 Edit/Write 重复,应剔除: {args}"
    );
}

/// --autocompact 跟随 Miyu 有效窗口(runtime 装配期夹到 100k–1M)。
#[tokio::test]
async fn autocompact_follows_the_effective_window() {
    let dir = tempfile::tempdir().unwrap();
    let mut client = claude_code_client(dir.path(), "cc-window");
    let mut runtime = ClaudeCodeRuntime {
        binary: fake_claude_script(dir.path()),
        native_tools: "off".to_string(),
        miyu_tools: "off".to_string(),
        permission_mode: "bypassPermissions".to_string(),
        autocompact: HashMap::new(),
        idle_timeout: Duration::from_secs(30),
        prefer_subscription: true,
    };
    runtime
        .autocompact
        .insert("cc-window\thaiku".to_string(), 168_000);
    client.claude_code = Some(Arc::new(runtime));
    client
        .chat_claude_code_stream(
            vec![ChatMessage::plain("user", "hi")],
            Vec::new(),
            "req-test",
            &mut |_| Ok(()),
        )
        .await
        .unwrap();
    let args = read(dir.path(), "args.txt");
    assert!(args.contains("--autocompact\n168000"), "{args}");
}
