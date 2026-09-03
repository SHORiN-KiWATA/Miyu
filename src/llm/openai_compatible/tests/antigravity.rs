//! antigravity 中转协议:假 `agy` 脚本回放 stream-json,验证参数拼装、载荷
//! 翻译、事件→chunk、用量归一、会话续传与两种静默失败的判定。不碰真实登录态,
//! 代理文件与 MCP 注册都落在测试临时目录。

use crate::llm::openai_compatible::antigravity::AntigravityRuntime;
use crate::llm::openai_compatible::tests::shared::*;
use crate::llm::openai_compatible::*;
use crate::llm::{ChatMessage, ChatStreamKind, FunctionDefinition, ToolDefinition};

fn fake_agy_script(dir: &std::path::Path) -> std::path::PathBuf {
    let script = dir.join("agy");
    // 记录 argv/stdin/环境,再回放一段固定事件流。会话 id 按测试目录唯一,
    // 免得并行测试在全局映射里互相顶掉。标记文件切换失败形态:
    //   fail.txt   → result ERROR(额度用尽措辞)
    //   empty.txt  → error_message 步 + 空 SUCCESS + 零用量(静默失败)
    //   noagent.txt→ init 不带 agent(人格没挂上)
    //   lost.txt   → 忽略 --conversation,新开会话(续传目标丢失)
    std::fs::write(
        &script,
        r#"#!/usr/bin/env bash
dir="$(cd "$(dirname "$0")" && pwd)"
sid="conv-$(basename "$dir")"
printf '%s\n' "$@" > "$dir/args.txt"
cat > "$dir/stdin.txt"
env | grep '^MIYU_' | sort > "$dir/env.txt" || true
resume=""
agent=""
prev=""
for a in "$@"; do
  [ "$prev" = "--conversation" ] && resume="$a"
  [ "$prev" = "--agent" ] && agent="$a"
  prev="$a"
done
if [ -n "$resume" ] && [ ! -f "$dir/lost.txt" ]; then sid="$resume"; fi
if [ -f "$dir/lost.txt" ]; then sid="fresh-$(basename "$dir")"; echo "warning: conversation \"$resume\" not found" >&2; fi
if [ -f "$dir/noagent.txt" ]; then
  echo "{\"event\":\"init\",\"conversation_id\":\"$sid\",\"init\":{\"model\":\"m\",\"cwd\":\"/\",\"tools\":[]}}"
  echo "Agent \"miyu\" not found, falling back to default" >&2
else
  echo "{\"event\":\"init\",\"conversation_id\":\"$sid\",\"init\":{\"model\":\"m\",\"cwd\":\"/\",\"agent\":\"$agent\",\"tools\":[]}}"
fi
if [ -f "$dir/fail.txt" ]; then
  echo "{\"event\":\"result\",\"result\":{\"conversation_id\":\"$sid\",\"status\":\"ERROR\",\"response\":\"\",\"error\":\"Quota exceeded for this model\",\"num_turns\":1,\"usage\":{\"input_tokens\":0,\"output_tokens\":0,\"thinking_tokens\":0,\"cache_read_tokens\":0,\"total_tokens\":0}}}"
  exit 0
fi
if [ -f "$dir/empty.txt" ]; then
  echo "{\"event\":\"step_update\",\"step_update\":{\"conversation_id\":\"$sid\",\"step_index\":0,\"state\":\"DONE\",\"step_type\":\"user_input\"}}"
  echo "{\"event\":\"step_update\",\"step_update\":{\"conversation_id\":\"$sid\",\"step_index\":1,\"state\":\"DONE\",\"step_type\":\"error_message\",\"text_delta\":\"failed to resolve components\"}}"
  echo "{\"event\":\"result\",\"result\":{\"conversation_id\":\"$sid\",\"status\":\"SUCCESS\",\"response\":\"\",\"num_turns\":1,\"usage\":{\"input_tokens\":0,\"output_tokens\":0,\"thinking_tokens\":0,\"cache_read_tokens\":0,\"total_tokens\":0}}}"
  exit 0
fi
echo "{\"event\":\"step_update\",\"step_update\":{\"conversation_id\":\"$sid\",\"step_index\":0,\"state\":\"DONE\",\"step_type\":\"user_input\"}}"
echo "{\"event\":\"step_update\",\"step_update\":{\"conversation_id\":\"$sid\",\"step_index\":1,\"state\":\"ACTIVE\",\"step_type\":\"agent_response\",\"text_delta\":\"Hello from \"}}"
echo "{\"event\":\"step_update\",\"step_update\":{\"conversation_id\":\"$sid\",\"step_index\":1,\"state\":\"DONE\",\"step_type\":\"agent_response\",\"text_delta\":\"fake\",\"usage\":{\"input_tokens\":40,\"output_tokens\":2,\"thinking_tokens\":1,\"cache_read_tokens\":10,\"total_tokens\":42}}}"
echo "{\"event\":\"step_update\",\"step_update\":{\"conversation_id\":\"$sid\",\"step_index\":2,\"state\":\"ACTIVE\",\"step_type\":\"tool\",\"tool_name\":\"run_command\",\"tool_info\":{\"name\":\"run_command\",\"parameters\":{\"CommandLine\":\"echo hi\"}}}}"
echo "{\"event\":\"step_update\",\"step_update\":{\"conversation_id\":\"$sid\",\"step_index\":2,\"state\":\"DONE\",\"step_type\":\"tool\",\"tool_name\":\"run_command\",\"tool_info\":{\"name\":\"run_command\",\"parameters\":{\"CommandLine\":\"echo hi\"},\"output\":\"a\\r\\nb\\r\\n\"}}}"
echo "{\"event\":\"step_update\",\"step_update\":{\"conversation_id\":\"$sid\",\"step_index\":3,\"state\":\"ACTIVE\",\"step_type\":\"tool\",\"tool_name\":\"mcp_miyu_use_meme\",\"tool_info\":{\"name\":\"mcp_miyu_use_meme\",\"parameters\":{\"action\":\"show\",\"id\":\"m1\"}}}}"
echo "{\"event\":\"step_update\",\"step_update\":{\"conversation_id\":\"$sid\",\"step_index\":3,\"state\":\"ERROR\",\"step_type\":\"tool\",\"tool_name\":\"mcp_miyu_use_meme\",\"tool_info\":{\"name\":\"mcp_miyu_use_meme\",\"parameters\":{\"action\":\"show\",\"id\":\"m1\"},\"error\":{\"type\":\"TOOL_ERROR\",\"message\":\"meme missing\"}}}}"
echo "{\"event\":\"step_update\",\"step_update\":{\"conversation_id\":\"$sid\",\"step_index\":9,\"state\":\"ACTIVE\",\"step_type\":\"tool\",\"tool_name\":\"mcp_miyu_ask_question\",\"tool_info\":{\"name\":\"mcp_miyu_ask_question\",\"parameters\":{\"questions\":[]}}}}"
echo "{\"event\":\"step_update\",\"step_update\":{\"conversation_id\":\"$sid\",\"step_index\":9,\"state\":\"DONE\",\"step_type\":\"tool\",\"tool_name\":\"mcp_miyu_ask_question\",\"tool_info\":{\"name\":\"mcp_miyu_ask_question\",\"output\":\"answered\"}}}}"
echo "{\"event\":\"step_update\",\"step_update\":{\"conversation_id\":\"$sid\",\"step_index\":4,\"state\":\"DONE\",\"step_type\":\"agent_response\",\"text_delta\":\"second\",\"usage\":{\"input_tokens\":60,\"output_tokens\":3,\"thinking_tokens\":0,\"cache_read_tokens\":0,\"total_tokens\":63}}}"
echo "{\"event\":\"result\",\"result\":{\"conversation_id\":\"$sid\",\"status\":\"SUCCESS\",\"response\":\"Hello from fake\\n\\nsecond\",\"num_turns\":1,\"usage\":{\"input_tokens\":100,\"output_tokens\":5,\"thinking_tokens\":1,\"cache_read_tokens\":10,\"total_tokens\":105}}}"
"#,
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    script
}

fn antigravity_client(
    dir: &std::path::Path,
    provider_id: &str,
    native: &str,
    miyu: &str,
) -> OpenAiCompatibleClient {
    let mut provider = test_provider(provider_id, "");
    provider.protocol = "antigravity".to_string();
    provider.default_model = "gemini-3.8-flash-high".to_string();
    let mut client = test_client(provider);
    client.antigravity = Some(Arc::new(AntigravityRuntime {
        binary: fake_agy_script(dir),
        native_tools: native.to_string(),
        miyu_tools: miyu.to_string(),
        miyu_tools_eager: true,
        idle_timeout: Duration::from_secs(30),
        print_timeout: Duration::from_secs(600),
        config_dir: dir.join("agyconfig"),
    }));
    client
}

fn read(dir: &std::path::Path, name: &str) -> String {
    std::fs::read_to_string(dir.join(name)).unwrap_or_default()
}

/// 代理目录按内容哈希命名(`miyu-<hash>`):测试里只关心唯一那份的内容。
fn agent_file(dir: &std::path::Path) -> (String, String) {
    let mut dirs: Vec<_> = std::fs::read_dir(dir.join("agyconfig/agents"))
        .unwrap()
        .flatten()
        .collect();
    assert_eq!(dirs.len(), 1, "应只有一份代理目录");
    let entry = dirs.pop().unwrap();
    let name = entry.file_name().to_string_lossy().to_string();
    assert!(name.starts_with("miyu-"), "{name}");
    (
        name.clone(),
        std::fs::read_to_string(entry.path().join("agent.md")).unwrap(),
    )
}

fn tool(name: &str) -> ToolDefinition {
    ToolDefinition {
        kind: "function",
        function: FunctionDefinition {
            name: name.to_string(),
            description: String::new(),
            parameters: serde_json::json!({ "type": "object" }),
        },
    }
}

async fn run(
    client: &OpenAiCompatibleClient,
    messages: Vec<ChatMessage>,
    tools: Vec<ToolDefinition>,
) -> (Result<ChatResult>, Vec<ChatStreamChunk>) {
    let mut chunks = Vec::new();
    let result = client
        .chat_stream_inner(messages, tools, None, false, &mut |chunk| {
            chunks.push(chunk);
            Ok(())
        })
        .await;
    (result, chunks)
}

/// 首轮:参数拼装 + 代理文件/MCP 注册落盘 + 事件翻译 + 用量。
#[tokio::test]
async fn first_turn_writes_agent_and_bridge_then_translates_the_stream() {
    let dir = tempfile::tempdir().unwrap();
    let client = antigravity_client(dir.path(), "agy-first", "all", "all");
    let tools = vec![tool("use_meme"), tool("run_command"), tool("alarm")];
    let (result, chunks) = run(
        &client,
        vec![
            ChatMessage::system("persona prompt"),
            ChatMessage::plain("user", "hello"),
        ],
        tools,
    )
    .await;
    let result = result.unwrap();
    assert_eq!(result.content, "Hello from fake\n\nsecond");
    assert!(result.tool_calls.is_empty());
    assert_eq!(result.finish_reason.as_deref(), Some("stop"));
    // 整轮累计用 result 帧;单次用最后一个 agent_response DONE 的。
    let usage = result.usage.unwrap();
    assert_eq!(usage.prompt_tokens, 100);
    assert_eq!(usage.completion_tokens, 5);
    assert_eq!(usage.cache_read_tokens, 10);
    assert_eq!(usage.reasoning_tokens, 1);
    assert!(usage.cache_reported);
    assert_eq!(result.last_request_usage.unwrap().prompt_tokens, 60);

    // 正文分片按步到达,步间补空行。
    let content: String = chunks
        .iter()
        .filter(|chunk| chunk.kind == ChatStreamKind::Content)
        .map(|chunk| chunk.text.as_str())
        .collect();
    assert_eq!(content, "Hello from fake\n\nsecond");
    // 原生 run_command:入参键归一,输出保换行、去 \r。
    let started: Vec<serde_json::Value> = chunks
        .iter()
        .filter(|chunk| chunk.kind == ChatStreamKind::RemoteToolStarted)
        .map(|chunk| serde_json::from_str(&chunk.text).unwrap())
        .collect();
    // 桥问答有自己的 question.* 事件,不翻成工具卡片(否则「准备问题」黏住)。
    assert_eq!(started.len(), 2, "{started:?}");
    assert!(
        !started.iter().any(|s| s["name"] == "ask_question"),
        "ask_question 不该出现在 RemoteToolStarted 里: {started:?}"
    );
    assert_eq!(started[0]["name"], "run_command");
    assert_eq!(started[0]["input"]["command"], "echo hi");
    assert_eq!(started[0]["id"], "agy-2");
    assert_eq!(started[1]["name"], "use_meme", "桥工具剥 mcp_miyu_ 前缀");
    let finished: Vec<serde_json::Value> = chunks
        .iter()
        .filter(|chunk| chunk.kind == ChatStreamKind::RemoteToolFinished)
        .map(|chunk| serde_json::from_str(&chunk.text).unwrap())
        .collect();
    assert_eq!(finished[0]["ok"], true);
    assert_eq!(finished[0]["output"], "a\nb");
    assert_eq!(finished[1]["ok"], false);
    assert_eq!(finished[1]["output"], "meme missing");
    assert_eq!(finished.len(), 2, "{finished:?}");

    let args = read(dir.path(), "args.txt");
    let (agent_name, agent) = agent_file(dir.path());
    for needle in [
        "--print=",
        "--input-format\nstream-json",
        "--output-format\nstream-json",
        "--dangerously-skip-permissions",
        "--model\ngemini-3.8-flash-high",
        &format!("--agent\n{agent_name}"),
        "--add-dir\n",
        "--print-timeout\n600s",
    ] {
        assert!(args.contains(needle), "missing {needle:?} in args: {args}");
    }
    assert!(!args.contains("--conversation"), "首轮不该续传: {args}");
    let stdin = read(dir.path(), "stdin.txt");
    let line: serde_json::Value = serde_json::from_str(stdin.trim()).unwrap();
    assert_eq!(line["event"], "user");
    assert_eq!(line["message"]["content"][0]["text"], "hello");

    // 代理文件:提示词 + 环境事实,白名单不含 ask_question。
    assert!(agent.contains(&format!("name: {agent_name}\n")), "{agent}");
    assert!(agent.contains("  - run_command\n"));
    assert!(!agent.contains("ask_question\n"));
    assert!(agent.contains("persona prompt"));
    assert!(agent.contains("<relay-environment>"));
    assert!(agent.contains("<relay-environment-tools>"));
    // 测试里没有回合作用域(无 Miyu 会话)→ 不注册桥、不给会话身份:桥本就
    // 应答空表,写一份空 eager 名单只会覆盖别的会话正在用的那份。
    assert!(
        !dir.path().join("agyconfig/mcp_config.json").exists(),
        "无会话不该写 mcp_config"
    );
    let env = read(dir.path(), "env.txt");
    assert!(!env.contains("MIYU_SESSION="), "{env}");
}

/// 原生工具关掉时代理文件写空白名单;桥关掉时 eager 名单为空。
#[tokio::test]
async fn scopes_shape_the_agent_file_and_bridge_entry() {
    let dir = tempfile::tempdir().unwrap();
    let client = antigravity_client(dir.path(), "agy-scopes", "off", "off");
    let (result, _) = run(
        &client,
        vec![
            ChatMessage::system("persona"),
            ChatMessage::plain("user", "hi"),
        ],
        vec![tool("use_meme")],
    )
    .await;
    result.unwrap();
    let (_, agent) = agent_file(dir.path());
    assert!(agent.contains("tools: []\n"), "{agent}");
    assert!(!agent.contains("<relay-environment>"), "无工具不附环境事实");
    assert!(
        !dir.path().join("agyconfig/mcp_config.json").exists(),
        "桥关着不写 mcp_config"
    );
}

/// 第二轮命中前缀:带 --conversation,stdin 只有增量、没有历史转写。
#[tokio::test]
async fn second_turn_resumes_with_only_the_delta() {
    let dir = tempfile::tempdir().unwrap();
    let client = antigravity_client(dir.path(), "agy-resume", "all", "off");
    let first = vec![
        ChatMessage::system("persona"),
        ChatMessage::plain("user", "one"),
    ];
    let (result, _) = run(&client, first.clone(), Vec::new()).await;
    let reply = result.unwrap().content;
    let mut second = first;
    second.push(ChatMessage::assistant(reply, None));
    second.push(ChatMessage::plain("user", "two"));
    let (result, _) = run(&client, second, Vec::new()).await;
    result.unwrap();
    let args = read(dir.path(), "args.txt");
    assert!(
        args.contains(&format!(
            "--conversation\nconv-{}",
            dir.path().file_name().unwrap().to_string_lossy()
        )),
        "{args}"
    );
    let stdin = read(dir.path(), "stdin.txt");
    assert!(stdin.contains("\"two\""), "{stdin}");
    assert!(!stdin.contains("conversation-history"), "{stdin}");
    assert!(!stdin.contains("\"one\""), "{stdin}");
}

/// 续传目标丢失:agy 静默新开会话,init 的 id 对不上 → 杀掉重来,整段重放。
#[tokio::test]
async fn lost_conversation_is_detected_from_init_and_replayed() {
    let dir = tempfile::tempdir().unwrap();
    let client = antigravity_client(dir.path(), "agy-lost", "all", "off");
    let first = vec![
        ChatMessage::system("persona"),
        ChatMessage::plain("user", "one"),
    ];
    let (result, _) = run(&client, first.clone(), Vec::new()).await;
    let reply = result.unwrap().content;
    std::fs::write(dir.path().join("lost.txt"), "").unwrap();
    let mut second = first;
    second.push(ChatMessage::assistant(reply, None));
    second.push(ChatMessage::plain("user", "two"));
    let (result, _) = run(&client, second, Vec::new()).await;
    assert_eq!(result.unwrap().content, "Hello from fake\n\nsecond");
    let args = read(dir.path(), "args.txt");
    assert!(!args.contains("--conversation"), "重放不再续传: {args}");
    let stdin = read(dir.path(), "stdin.txt");
    assert!(stdin.contains("conversation-history"), "{stdin}");
    assert!(
        stdin.contains("\"one\"") || stdin.contains("User:\\none"),
        "{stdin}"
    );
}

/// 额度用尽:result ERROR 的措辞翻成 429,进冷却/故障转移。
#[tokio::test]
async fn quota_failure_is_classified_as_rate_limit() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("fail.txt"), "").unwrap();
    let client = antigravity_client(dir.path(), "agy-fail", "all", "off");
    let error = client
        .chat_antigravity_stream(
            vec![ChatMessage::plain("user", "hi")],
            Vec::new(),
            "req-test",
            &mut |_| Ok(()),
        )
        .await
        .unwrap_err();
    let failure = error
        .downcast_ref::<HttpStatusFailure>()
        .expect("quota error should classify as an HTTP-style failure");
    assert_eq!(failure.kind, HttpFailureKind::RateLimit);
    assert_eq!(failure.status, 429);
}

/// 静默失败:SUCCESS + 空正文 + 零用量 → 报错并带 error_message 步的正文。
#[tokio::test]
async fn empty_success_is_an_error() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("empty.txt"), "").unwrap();
    let client = antigravity_client(dir.path(), "agy-empty", "all", "off");
    let (result, _) = run(&client, vec![ChatMessage::plain("user", "hi")], Vec::new()).await;
    let text = format!("{:#}", result.unwrap_err());
    assert!(text.contains("no output"), "{text}");
    assert!(text.contains("failed to resolve components"), "{text}");
}

/// 人格没挂上(init.agent 缺失)= 错误,不能静默跑在默认提示词上。
#[tokio::test]
async fn missing_persona_agent_is_an_error() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("noagent.txt"), "").unwrap();
    let client = antigravity_client(dir.path(), "agy-noagent", "all", "off");
    let (result, _) = run(&client, vec![ChatMessage::plain("user", "hi")], Vec::new()).await;
    let text = format!("{:#}", result.unwrap_err());
    assert!(text.contains("persona agent"), "{text}");
}

/// 辅助请求(scope≠chat):无工具、不续传。
#[tokio::test]
async fn auxiliary_scope_has_no_tools_and_no_resume() {
    let dir = tempfile::tempdir().unwrap();
    let client =
        antigravity_client(dir.path(), "agy-aux", "all", "all").with_request_scope("compact");
    let messages = vec![
        ChatMessage::system("persona"),
        ChatMessage::plain("user", "summarize"),
    ];
    let (result, _) = run(&client, messages.clone(), vec![tool("use_meme")]).await;
    result.unwrap();
    let (_, agent) = agent_file(dir.path());
    assert!(agent.contains("tools: []\n"), "{agent}");
    let (result, _) = run(&client, messages, vec![tool("use_meme")]).await;
    result.unwrap();
    assert!(!read(dir.path(), "args.txt").contains("--conversation"));
}
