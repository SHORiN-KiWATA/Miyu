//! codex 中转协议:假 `codex` 脚本回放 `exec --json` 的 JSONL,验证参数拼装
//! (逐进程 -c 覆盖、resume)、stdin 正文、事件→chunk(正文/思考摘要/命令/MCP/
//! 文件改动)、用量与失败判定。不碰真实登录态。

use crate::llm::openai_compatible::codex::CodexRuntime;
use crate::llm::openai_compatible::tests::shared::*;
use crate::llm::openai_compatible::*;
use crate::llm::{ChatMessage, ChatStreamKind};

fn fake_codex_script(dir: &std::path::Path) -> std::path::PathBuf {
    let script = dir.join("codex");
    std::fs::write(
        &script,
        r#"#!/usr/bin/env bash
dir="$(cd "$(dirname "$0")" && pwd)"
tid="thread-$(basename "$dir")"
printf '%s\n' "$@" > "$dir/args.txt"
cat > "$dir/stdin.txt"
resume=""
prev=""
for a in "$@"; do
  [ "$prev" = "resume" ] && resume="$a"
  prev="$a"
done
if [ -n "$resume" ] && [ -f "$dir/lost.txt" ]; then
  echo "Error: thread/resume: thread/resume failed: no rollout found for thread id $resume (code -32600)" >&2
  exit 1
fi
[ -n "$resume" ] && tid="$resume"
if [ -f "$dir/auth.txt" ]; then
  echo "{\"type\":\"thread.started\",\"thread_id\":\"$tid\"}"
  echo '{"type":"turn.started"}'
  echo '{"type":"error","message":"Reconnecting... 2/5 (unexpected status 401 Unauthorized: Missing bearer)"}'
  exit 101
fi
echo "{\"type\":\"thread.started\",\"thread_id\":\"$tid\"}"
echo '{"type":"turn.started"}'
echo '{"type":"item.completed","item":{"id":"item_0","type":"reasoning","text":"thinking about it"}}'
echo '{"type":"item.started","item":{"id":"item_1","type":"command_execution","command":"bash -lc ls","aggregated_output":"","status":"in_progress"}}'
echo '{"type":"item.completed","item":{"id":"item_1","type":"command_execution","command":"bash -lc ls","aggregated_output":"a\r\nb\r\n","exit_code":0,"status":"completed"}}'
echo '{"type":"item.started","item":{"id":"item_2","type":"mcp_tool_call","server":"miyu","tool":"use_meme","arguments":{"action":"show","id":"m1"},"status":"in_progress"}}'
echo '{"type":"item.completed","item":{"id":"item_2","type":"mcp_tool_call","server":"miyu","tool":"use_meme","arguments":{"action":"show","id":"m1"},"result":{"content":[{"type":"text","text":"meme sent ok"}]},"status":"completed"}}'
echo '{"type":"item.started","item":{"id":"item_9","type":"mcp_tool_call","server":"miyu","tool":"ask_question","arguments":{},"status":"in_progress"}}'
echo '{"type":"item.completed","item":{"id":"item_9","type":"mcp_tool_call","server":"miyu","tool":"ask_question","arguments":{},"result":{"content":[]},"status":"completed"}}'
echo '{"type":"item.completed","item":{"id":"item_3","type":"file_change","changes":[{"path":"/w/a.rs","kind":"update"}],"status":"completed"}}'
echo '{"type":"item.completed","item":{"id":"item_4","type":"agent_message","text":"Hello from fake"}}'
echo '{"type":"turn.completed","usage":{"input_tokens":100,"cached_input_tokens":40,"output_tokens":5,"reasoning_output_tokens":2}}'
"#,
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    script
}

fn codex_client(
    dir: &std::path::Path,
    provider_id: &str,
    native: &str,
    miyu: &str,
) -> OpenAiCompatibleClient {
    let mut provider = test_provider(provider_id, "");
    provider.protocol = "codex".to_string();
    provider.default_model = "gpt-5.6-terra".to_string();
    let mut client = test_client(provider);
    client.codex = Some(Arc::new(CodexRuntime {
        binary: fake_codex_script(dir),
        native_tools: native.to_string(),
        miyu_tools: miyu.to_string(),
        sandbox_mode: "danger-full-access".to_string(),
        ignore_user_config: true,
        idle_timeout: Duration::from_secs(30),
        instructions_dir: dir.join("instr"),
    }));
    client
}

fn read(dir: &std::path::Path, name: &str) -> String {
    std::fs::read_to_string(dir.join(name)).unwrap_or_default()
}

async fn run(
    client: &OpenAiCompatibleClient,
    messages: Vec<ChatMessage>,
) -> (Result<ChatResult>, Vec<ChatStreamChunk>) {
    let mut chunks = Vec::new();
    let result = client
        .chat_stream_inner(messages, Vec::new(), None, false, &mut |chunk| {
            chunks.push(chunk);
            Ok(())
        })
        .await;
    (result, chunks)
}

#[tokio::test]
async fn first_turn_injects_config_per_process_and_translates_items() {
    let dir = tempfile::tempdir().unwrap();
    let client = codex_client(dir.path(), "cx-first", "all", "all");
    let (result, chunks) = run(
        &client,
        vec![
            ChatMessage::system("persona prompt"),
            ChatMessage::plain("user", "hello"),
        ],
    )
    .await;
    let result = result.unwrap();
    assert_eq!(result.content, "Hello from fake");
    assert_eq!(result.reasoning.as_deref(), Some("thinking about it"));
    let usage = result.usage.unwrap();
    assert_eq!(usage.prompt_tokens, 100);
    assert_eq!(usage.cache_read_tokens, 40);
    assert_eq!(usage.reasoning_tokens, 2);
    assert!(chunks
        .iter()
        .any(|c| c.kind == ChatStreamKind::Reasoning && c.text.contains("thinking")));

    let started: Vec<serde_json::Value> = chunks
        .iter()
        .filter(|c| c.kind == ChatStreamKind::RemoteToolStarted)
        .map(|c| serde_json::from_str(&c.text).unwrap())
        .collect();
    let names: Vec<&str> = started
        .iter()
        .map(|s| s["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["run_command", "use_meme", "edit"], "{started:?}");
    assert_eq!(started[0]["input"]["command"], "bash -lc ls");
    assert_eq!(started[2]["input"]["path"], "/w/a.rs");
    let finished: Vec<serde_json::Value> = chunks
        .iter()
        .filter(|c| c.kind == ChatStreamKind::RemoteToolFinished)
        .map(|c| serde_json::from_str(&c.text).unwrap())
        .collect();
    assert_eq!(finished.len(), 3, "桥问答不翻成卡片: {finished:?}");
    assert_eq!(finished[0]["output"], "a\nb");
    assert_eq!(finished[1]["output"], "meme sent ok");
    assert_eq!(finished[2]["ok"], true);

    let args = read(dir.path(), "args.txt");
    for needle in [
        "exec\n--json\n--skip-git-repo-check\n-C\n",
        "-m\ngpt-5.6-terra",
        "--ignore-user-config",
        "--dangerously-bypass-approvals-and-sandbox",
        "-c\nproject_doc_max_bytes=0",
        "-c\napproval_policy=\"never\"",
        "model_instructions_file=\"",
    ] {
        assert!(args.contains(needle), "missing {needle:?} in args: {args}");
    }
    assert!(
        args.trim_end().ends_with("\n-"),
        "提示词应从 stdin 读: {args}"
    );
    assert!(!args.contains("\nresume\n"), "首轮不该续传: {args}");
    // 测试里没有回合作用域(无 Miyu 会话)→ 不挂桥。
    assert!(!args.contains("mcp_servers.miyu"), "{args}");
    assert_eq!(read(dir.path(), "stdin.txt"), "hello\n");
    // 指令文件 = 人格 + 环境事实,按内容哈希落盘。
    let instr = std::fs::read_dir(dir.path().join("instr"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap();
    let text = std::fs::read_to_string(instr.path()).unwrap();
    assert!(text.starts_with("persona prompt"));
    assert!(text.contains("<relay-environment>"));
}

#[tokio::test]
async fn native_off_disables_shell_and_web_search_and_no_notes() {
    let dir = tempfile::tempdir().unwrap();
    let client = codex_client(dir.path(), "cx-off", "off", "off");
    let (result, _) = run(
        &client,
        vec![ChatMessage::system("p"), ChatMessage::plain("user", "hi")],
    )
    .await;
    result.unwrap();
    let args = read(dir.path(), "args.txt");
    assert!(args.contains("features.shell_tool=false"), "{args}");
    assert!(args.contains("web_search=\"disabled\""), "{args}");
    let instr = std::fs::read_dir(dir.path().join("instr"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap();
    assert!(!std::fs::read_to_string(instr.path())
        .unwrap()
        .contains("<relay-environment>"));
}

#[tokio::test]
async fn second_turn_resumes_with_only_the_delta() {
    let dir = tempfile::tempdir().unwrap();
    let client = codex_client(dir.path(), "cx-resume", "all", "off");
    let first = vec![
        ChatMessage::system("persona"),
        ChatMessage::plain("user", "one"),
    ];
    let (result, _) = run(&client, first.clone()).await;
    let reply = result.unwrap().content;
    let mut second = first;
    second.push(ChatMessage::assistant(reply, None));
    second.push(ChatMessage::plain("user", "two"));
    let (result, _) = run(&client, second).await;
    result.unwrap();
    let args = read(dir.path(), "args.txt");
    let expected = format!(
        "resume\nthread-{}\n-",
        dir.path().file_name().unwrap().to_string_lossy()
    );
    assert!(args.contains(&expected), "{args}");
    let stdin = read(dir.path(), "stdin.txt");
    assert_eq!(stdin, "two\n");
}

#[tokio::test]
async fn lost_rollout_is_replayed_in_full() {
    let dir = tempfile::tempdir().unwrap();
    let client = codex_client(dir.path(), "cx-lost", "all", "off");
    let first = vec![
        ChatMessage::system("persona"),
        ChatMessage::plain("user", "one"),
    ];
    let (result, _) = run(&client, first.clone()).await;
    let reply = result.unwrap().content;
    std::fs::write(dir.path().join("lost.txt"), "").unwrap();
    let mut second = first;
    second.push(ChatMessage::assistant(reply, None));
    second.push(ChatMessage::plain("user", "two"));
    let (result, _) = run(&client, second).await;
    assert_eq!(result.unwrap().content, "Hello from fake");
    let args = read(dir.path(), "args.txt");
    assert!(!args.contains("\nresume\n"), "{args}");
    let stdin = read(dir.path(), "stdin.txt");
    assert!(
        stdin.contains("conversation-history") && stdin.contains("one"),
        "{stdin}"
    );
}

#[tokio::test]
async fn auth_failure_is_classified() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("auth.txt"), "").unwrap();
    let client = codex_client(dir.path(), "cx-auth", "all", "off");
    let error = client
        .chat_codex_stream(
            vec![ChatMessage::plain("user", "hi")],
            Vec::new(),
            "req",
            &mut |_| Ok(()),
        )
        .await
        .unwrap_err();
    let failure = error
        .downcast_ref::<HttpStatusFailure>()
        .expect("401 should classify as an HTTP-style failure");
    assert_eq!(failure.status, 401);
}

#[tokio::test]
async fn auxiliary_scope_is_ephemeral_and_toolless() {
    let dir = tempfile::tempdir().unwrap();
    let client = codex_client(dir.path(), "cx-aux", "all", "all").with_request_scope("compact");
    let messages = vec![ChatMessage::system("p"), ChatMessage::plain("user", "sum")];
    let (result, _) = run(&client, messages.clone()).await;
    result.unwrap();
    let args = read(dir.path(), "args.txt");
    assert!(args.contains("--ephemeral"), "{args}");
    assert!(args.contains("features.shell_tool=false"), "{args}");
    let (result, _) = run(&client, messages).await;
    result.unwrap();
    assert!(!read(dir.path(), "args.txt").contains("\nresume\n"));
}
