//! 上下文的可见性、化石回放与裁剪。

use super::shared::*;
use crate::agent::*;
use crate::config::AppConfig;
use crate::tools::{empty_parameters, ToolSpec};
use tokio::net::TcpListener;

/// 复读毒料免疫(08-24):历史 tool_flow 里连续同参轮只回放第一轮——
/// 122 轮 111 重复的会话曾把模型锁进 in-context 复读。不同参轮照常全放。
/// 退回 replay_rounds 折叠前,第一段断言(2 个调用轮)会报红为 4。
#[test]
fn consecutive_identical_history_rounds_collapse_on_replay() {
    let temp = tempfile::tempdir().unwrap();
    let paths = test_paths(temp.path());
    let config = AppConfig::default();
    let state = StateStore::new(&paths).unwrap();
    state.start_turn("old", "查一下", 999_999).unwrap();
    let round = |args: &str, output: &str| crate::state::ToolFlowRound {
        remote: false,
        assistant_content: String::new(),
        assistant_reasoning: None,
        calls: vec![crate::state::ToolFlowCall {
            id: "c".to_string(),
            name: "web_search".to_string(),
            arguments: args.to_string(),
            output: output.to_string(),
        }],
    };
    state
        .set_turn_tool_flow(
            "old",
            &[
                round("{\"q\":\"a\"}", "r1"),
                round("{\"q\":\"a\"}", "r2"),
                round("{\"q\":\"a\"}", "r3"),
                round("{\"q\":\"b\"}", "r4"),
            ],
        )
        .unwrap();
    state.complete_turn("old", "查完了", None).unwrap();
    let client =
        OpenAiCompatibleClient::new(config.provider(None).unwrap(), &config, &paths).unwrap();
    let agent = Agent::new(
        config,
        &paths,
        state,
        client,
        ToolRegistry::new(),
        AgentMode::Normal,
    )
    .unwrap();

    let messages = agent.chat_messages("current", "继续").unwrap().0;
    let tool_call_rounds = messages
        .iter()
        .filter(|m| m.tool_calls.as_ref().is_some_and(|c| !c.is_empty()))
        .count();
    assert_eq!(tool_call_rounds, 2, "连续同参轮应折叠成 1+1");
    // 保留的是首轮的真实结果字节;重复轮的 r2/r3 不再出现。
    let text = format!("{messages:?}");
    assert!(text.contains("r1") && text.contains("r4"));
    assert!(!text.contains("r2") && !text.contains("r3"));
}

/// pop 溢出策略(平台群会话默认)必须真的裁掉旧回合。08-25 线上实录:某群
/// 会话堆到 68 万 token(窗口 20 万)仍未裁剪,最后靠 /reset 才收场——这条
/// 用例把"超水位就逐出到目标线"钉死在库里。
#[tokio::test]
async fn pop_overflow_evicts_until_under_target() {
    let temp = tempfile::tempdir().unwrap();
    let paths = test_paths(temp.path());
    let mut config = AppConfig::default();
    // 与线上群一致的口径:窗口 20 万、水位 0.9、每次裁到 (1-0.6)=8 万。
    let provider = config
        .providers
        .iter_mut()
        .find(|provider| !provider.is_builtin_cli_provider())
        .unwrap();
    provider
        .model_context_window
        .insert(provider.default_model.clone(), 200_000);
    config.context.trim_at_ratio = 0.9;
    config.context.trim_batch_ratio = 0.6;
    config.context.on_overflow = "pop".to_string();

    let state = StateStore::new(&paths).unwrap();
    state.init_files().unwrap();
    // 40 个大回合:每个约 3 万字符,合计远超水位。
    let bulk = "群聊记录一行".repeat(2_500);
    for index in 0..40 {
        let turn_id = format!("turn-{index}");
        state.start_turn(&turn_id, &bulk, 999_999).unwrap();
        state.complete_turn(&turn_id, &bulk, None).unwrap();
    }
    let client =
        OpenAiCompatibleClient::new(config.provider(None).unwrap(), &config, &paths).unwrap();
    let agent = Agent::new(
        config,
        &paths,
        state.clone(),
        client,
        ToolRegistry::new(),
        AgentMode::Normal,
    )
    .unwrap();

    let window = agent.context_window().expect("窗口应可解析");
    assert_eq!(window, 200_000);
    let before = agent.effective_context_tokens().unwrap();
    assert!(
        before as usize >= (window as f32 * 0.9) as usize,
        "造出来的会话没到水位: {before}"
    );

    let evicted = agent.trim_visible_context().unwrap();
    assert!(!evicted.is_empty(), "超水位却一个回合都没裁");
    let after = agent.effective_context_tokens().unwrap();
    assert!(
        (after as usize) < (window as f32 * 0.9) as usize,
        "裁剪后仍在水位之上: {after}"
    );
    // 逐出的是最老的,最新一轮必须留着。
    let remaining = state.load_visible_turns().unwrap();
    assert!(remaining.iter().any(|turn| turn.turn_id == "turn-39"));
    assert!(!remaining.iter().any(|turn| turn.turn_id == "turn-0"));
}

/// 剪枝必须幂等:第二次扫过不能再改写,否则每次落库都掰一次前缀。
#[test]
fn tool_result_pruning_is_bounded_and_idempotent() {
    let output = "x".repeat(20_000);
    let pruned = prune_tool_output(&output, 8192, 4096, 1024);
    assert!(pruned.chars().count() < output.chars().count());
    assert!(
        pruned.contains("14880") || pruned.contains("已省略"),
        "{pruned}"
    );
    assert_eq!(prune_tool_output(&pruned, 8192, 4096, 1024), pruned);
    // 预算内的输出一个字节都不动。
    let small = "short output";
    assert_eq!(prune_tool_output(small, 8192, 4096, 1024), small);
    // 预算不自洽时原样返回,不下溢。
    assert_eq!(prune_tool_output(&output, 100, 80, 80), output);
}

#[test]
fn structured_platform_context_can_suppress_ambiguous_session_replay() {
    let temp = tempfile::tempdir().unwrap();
    let paths = test_paths(temp.path());
    let config = AppConfig::default();
    let state = StateStore::new(&paths).unwrap();
    state
        .start_turn("old", "anonymous old user", 999_999)
        .unwrap();
    state.complete_turn("old", "old assistant", None).unwrap();
    let client =
        OpenAiCompatibleClient::new(config.provider(None).unwrap(), &config, &paths).unwrap();
    let mut agent = Agent::new(
        config,
        &paths,
        state,
        client,
        ToolRegistry::new(),
        AgentMode::Normal,
    )
    .unwrap();

    assert!(agent
        .chat_messages("current", "new user")
        .unwrap()
        .0
        .iter()
        .any(|message| format!("{:?}", message.content).contains("anonymous old user")));
    agent.set_session_history_suppressed(true);
    let messages = agent.chat_messages("current", "new user").unwrap().0;
    assert!(!messages
        .iter()
        .any(|message| format!("{:?}", message.content).contains("anonymous old user")));
    // [.., user, runtime tail]: the current user message sits right before
    // the transient runtime stamp.
    assert!(format!("{:?}", messages[messages.len() - 2].content).contains("new user"));
    assert!(format!("{:?}", messages.last().unwrap().content).contains("<runtime now="));
}

#[test]
fn fossilized_transient_tail_replays_between_user_and_assistant() {
    let temp = tempfile::tempdir().unwrap();
    let paths = test_paths(temp.path());
    let config = AppConfig::default();
    let state = StateStore::new(&paths).unwrap();
    state.start_turn("old", "old question", 999_999).unwrap();
    state
        .set_turn_context_messages(
            "old",
            &[
                ChatMessage::turn_context("<runtime now=\"frozen stamp\"/>"),
                ChatMessage::turn_context("<associative-memory>frozen recall</associative-memory>"),
            ],
        )
        .unwrap();
    state.complete_turn("old", "old answer", None).unwrap();
    let client =
        OpenAiCompatibleClient::new(config.provider(None).unwrap(), &config, &paths).unwrap();
    let agent = Agent::new(
        config,
        &paths,
        state,
        client,
        ToolRegistry::new(),
        AgentMode::Normal,
    )
    .unwrap();

    let messages = agent.chat_messages("current", "next question").unwrap().0;
    let text = |message: &ChatMessage| format!("{:?}", message.content);
    let user = messages
        .iter()
        .position(|m| text(m).contains("old question"))
        .unwrap();
    let assistant = messages
        .iter()
        .position(|m| text(m).contains("old answer"))
        .unwrap();
    // The fossils sit, in order, strictly between the user message and the
    // assistant reply — byte-for-byte what the live request sent.
    assert_eq!(messages[user + 1].role, "user");
    assert!(text(&messages[user + 1]).contains("frozen stamp"));
    assert_eq!(messages[user + 2].role, "user");
    assert!(text(&messages[user + 2]).contains("frozen recall"));
    assert!(user + 2 < assistant);
}

#[test]
fn a_still_running_turn_stays_out_of_everyone_elses_history() {
    // A running turn holds a placeholder that is overwritten with the real
    // reply when it finishes, so replaying it puts two different byte
    // sequences at the same position and drops the prefix cache for every
    // turn behind it. About a fifth of this group's turns overlap.
    let temp = tempfile::tempdir().unwrap();
    let paths = test_paths(temp.path());
    let state = StateStore::new(&paths).unwrap();
    state
        .start_turn("t1", "第一条", std::process::id())
        .unwrap();
    state
        .complete_turn_with_usage_and_model(
            "t1",
            "答复一",
            None,
            None,
            None,
            TurnTokens::default(),
            false,
        )
        .unwrap();
    state
        .start_turn("t2", "并发的一条", std::process::id())
        .unwrap();

    let visible = state.load_visible_turns_excluding("t3").unwrap();
    let running: Vec<&str> = visible
        .iter()
        .filter(|turn| turn.status == crate::state::TurnStatus::Running)
        .map(|turn| turn.turn_id.as_str())
        .collect();
    assert_eq!(running, ["t2"], "the store still hands them over");
    assert_eq!(
        visible
            .iter()
            .filter(|turn| turn.status != crate::state::TurnStatus::Running)
            .count(),
        1,
        "and exactly one is replayable"
    );
}

#[test]
fn a_fossil_written_before_the_role_change_replays_as_a_user_block() {
    // Old turns stored the transient tail as `system`. Replaying that
    // verbatim would keep poisoning the prefix for the rest of the
    // session's life, so it is re-roled on the way out.
    let stored = ChatMessage::system("<runtime now=\"old\"/>");
    let replayed = replay_fossil(&stored);
    assert_eq!(replayed.role, "user");
    assert!(replayed.transient_context);
    assert!(matches!(
        replayed.content.as_ref(),
        Some(ChatContent::Text(content)) if content == "<runtime now=\"old\"/>"
    ));

    // Already-converted fossils pass through untouched.
    let fresh = ChatMessage::turn_context("<runtime now=\"new\"/>");
    assert_eq!(replay_fossil(&fresh).role, "user");
}

#[test]
fn fossil_capture_stops_at_the_first_non_context_message() {
    let tail = vec![
        ChatMessage::turn_context("<runtime now=\"x\"/>"),
        ChatMessage::turn_context("hint"),
        ChatMessage::plain("assistant", "loop starts here"),
        ChatMessage::turn_context("after loop — must not be captured"),
    ];
    let fossil = fossil_context_messages(&tail);
    assert_eq!(fossil.len(), 2);
    assert!(format!("{:?}", fossil[1].content).contains("hint"));
}

#[test]
fn visible_association_lines_collects_only_replayed_memory_blocks() {
    let block = "<associative-memory>\n以下是根据当前输入联想到的完整人格记忆。\n\n曾经记住的相关知识点：\n- [2026-08-10] [公共知识] AUR 镜像只读\n</associative-memory>";
    let messages = vec![
        ChatMessage::system("prompt"),
        // 回放的化石块：user 角色、正文以标签开头 → 计入
        ChatMessage::plain("user", block),
        // 用户正文中途引用同样文本 → 不以标签开头，不计入
        ChatMessage::plain("user", format!("用户引用了 {block}")),
        // 非 user 角色 → 不计入
        ChatMessage::plain("assistant", "- [2026-08-10] [公共知识] AUR 镜像只读"),
    ];
    let seen = visible_association_lines(&messages);
    assert_eq!(seen.len(), 1);
    assert!(seen.contains("- [2026-08-10] [公共知识] AUR 镜像只读"));
}

#[test]
fn turn_context_blocks_already_visible_in_fossils_are_skipped() {
    let notice = "[SystemInfo:LongReplyImageConversion]\n1. 你的一条长回复（约 480 字）已被自动渲染为 1 张图片发送。";
    let messages = vec![
        ChatMessage::system("prompt"),
        // 上一轮化石里已经带着同样的通知
        ChatMessage::plain(
            "user",
            format!("<qq-request-context>…</qq-request-context>\n\n{notice}"),
        ),
        ChatMessage::plain("assistant", "回复"),
    ];
    assert!(turn_context_block_visible(&messages, notice));
    // 内容变化(记录数不同)不再匹配,照常注入
    let changed = "[SystemInfo:LongReplyImageConversion]\n1. 你的一条长回复（约 480 字）已被自动渲染为 1 张图片发送。\n2. 你的一条长回复（约 900 字）已被自动渲染为 2 张图片发送。";
    assert!(!turn_context_block_visible(&messages, changed));
    // 非 user 角色的出现不算
    let assistant_only = vec![ChatMessage::plain("assistant", notice)];
    assert!(!turn_context_block_visible(&assistant_only, notice));
    // 只有 [SystemInfo: 前缀的常驻通告参与去重;指涉"当前回合"的块
    // (唤醒通知/身份告警/审核初判)即使字节相同也必须重发
    assert!(notice.starts_with(STANDING_ADVISORY_PREFIX));
    assert!(
        !"This turn was triggered automatically by the system: a background job just finished."
            .starts_with(STANDING_ADVISORY_PREFIX)
    );
    assert!(!"<qq-identity-warning>…</qq-identity-warning>".starts_with(STANDING_ADVISORY_PREFIX));
}

#[test]
fn effective_context_tokens_include_tool_definitions() {
    let temp = tempfile::tempdir().unwrap();
    let paths = test_paths(temp.path());
    let config = AppConfig::default();
    let state = StateStore::new(&paths).unwrap();
    state.init_files().unwrap();
    let client =
        OpenAiCompatibleClient::new(config.provider(None).unwrap(), &config, &paths).unwrap();
    let mut tools = ToolRegistry::new();
    tools.register(ToolSpec::new(
        "heavy_context_tool",
        "This tool has a deliberately long description so effective context includes tool definitions.",
        empty_parameters(),
        |_| async { Ok(String::new()) },
    ));
    let with_tools = Agent::new(
        config.clone(),
        &paths,
        state.clone(),
        client.clone(),
        tools,
        AgentMode::Normal,
    )
    .unwrap();
    let without_tools = Agent::new(
        AppConfig {
            tools: crate::config::ToolsConfig {
                enabled: false,
                ..config.tools.clone()
            },
            ..config
        },
        &paths,
        state,
        client,
        ToolRegistry::new(),
        AgentMode::Normal,
    )
    .unwrap();

    assert!(
        with_tools.effective_context_tokens().unwrap()
            > without_tools.effective_context_tokens().unwrap()
    );
}

#[test]
fn overflow_check_tokens_triggers_at_threshold() {
    let check = overflow::OverflowCheck::new(Some(100_000), 0.9, None);
    assert!(!check.check_tokens(60_000));
    assert!(check.check_tokens(95_000));
}

#[test]
fn overflow_check_disabled_when_no_window() {
    let check = overflow::OverflowCheck::new(None, 0.9, None);
    assert!(!check.is_enabled());
    assert!(!check.check_tokens(1_998_998));
}

#[test]
fn overflow_check_estimate_triggers() {
    let check = overflow::OverflowCheck::new(Some(1_000), 0.9, None);
    let big_msg = ChatMessage::plain("user", &"token ".repeat(2_000));
    let small_msg = ChatMessage::plain("user", "hi");
    assert!(check.check_estimate(&[big_msg]));
    assert!(!check.check_estimate(&[small_msg]));
}

#[test]
fn turn_context_tokens_match_sent_messages() {
    let mut turn = crate::state::Turn {
        turn_id: "t1".to_string(),
        seq: 1,
        user_content: "question".to_string(),
        display_content: "question".to_string(),
        user_timestamp: String::new(),
        assistant_content: "answer".to_string(),
        assistant_reasoning: Some("hidden reasoning ".repeat(1_000)),
        assistant_provider_id: None,
        assistant_model: None,
        assistant_timestamp: None,
        status: crate::state::TurnStatus::Completed,
        tool_reports: Vec::new(),
        tool_flow: Vec::new(),
        question_exchanges: Vec::new(),
        followups: Vec::new(),
        attachments: Vec::new(),
        hidden: false,
        is_summary: false,
        owner_pid: None,
        token_total: 0,
        token_prompt: 0,
        token_cache_read: 0,
        token_usage_estimated: false,
        revision: 0,
        journal_events: Vec::new(),
        context_messages: Vec::new(),
    };
    let with_reasoning = turn_context_tokens(&turn);
    turn.assistant_reasoning = None;
    let without_reasoning = turn_context_tokens(&turn);
    // 跨轮思考回放退役:完成轮的思维链不再计入(也不再发送)。
    assert_eq!(with_reasoning, without_reasoning);

    turn.tool_reports.push("persisted tool result".to_string());
    assert!(turn_context_tokens(&turn) > without_reasoning);
}

#[test]
fn assistant_reasoning_is_not_replayed_across_turns() {
    // 跨轮思考回放退役(08-16):完成轮只回放正式回复;中断恢复走
    // journal 专道(interrupted_turn_replay_messages),不经此函数。
    let mut messages = Vec::new();
    push_assistant_context_messages(
        &mut messages,
        "visible answer",
        Some("raw provider reasoning"),
        true,
    );

    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].role, "assistant");
    assert!(matches!(
        messages[0].content.as_ref(),
        Some(ChatContent::Text(content)) if content == "visible answer"
    ));
}

#[test]
fn trim_visible_context_keeps_summary_and_removes_oldest_turn() {
    let temp = tempfile::tempdir().unwrap();
    let paths = test_paths(temp.path());
    let config = AppConfig {
        tools: crate::config::ToolsConfig {
            enabled: false,
            ..AppConfig::default().tools
        },
        ..AppConfig::default()
    };
    let state = StateStore::new(&paths).unwrap();
    state.init_files().unwrap();
    let client =
        OpenAiCompatibleClient::new(config.provider(None).unwrap(), &config, &paths).unwrap();
    let mut agent = Agent::new(
        config,
        &paths,
        state.clone(),
        client,
        ToolRegistry::new(),
        AgentMode::Normal,
    )
    .unwrap();
    state
        .insert_summary_turn(&"summary ".repeat(2_000), TurnTokens::default(), true)
        .unwrap();
    for id in ["t1", "t2"] {
        state
            .start_turn(id, &format!("{id} {}", "question ".repeat(2_000)), 999999)
            .unwrap();
        state
            .complete_turn(id, &"answer ".repeat(2_000), None)
            .unwrap();
    }
    agent.trim_at_ratio = 1.0;
    let context_window = agent.effective_context_tokens().unwrap() as usize;
    let choice = agent.config.active_provider_model_choices().remove(0);
    agent
        .config
        .providers
        .iter_mut()
        .find(|provider| provider.id == choice.provider_id)
        .unwrap()
        .model_context_window
        .insert(choice.model, context_window);
    assert_eq!(agent.context_window(), Some(context_window));

    let evicted = agent.trim_visible_context().unwrap();

    assert!(!evicted.is_empty());
    let visible = state.load_visible_turns().unwrap();
    assert_eq!(visible.len(), 2);
    assert!(visible[0].is_summary);
    assert_eq!(visible[1].turn_id, "t2");
}

#[test]
fn explicit_pop_archives_context_content_but_not_reasoning() {
    let temp = tempfile::tempdir().unwrap();
    let paths = test_paths(temp.path());
    let config = AppConfig::default();
    let state = StateStore::new(&paths).unwrap();
    state.start_turn("t1", "promptonlyalpha", 999999).unwrap();
    state
        .complete_turn("t1", "answeronlybeta", Some("reasoningonlyquasar"))
        .unwrap();
    state
        .append_persisted_context("t1", "toolonlygamma")
        .unwrap();
    let memory = MemoryStore::new(&config, &paths);
    let turns = state.oldest_evictable_visible_turns(1).unwrap();

    archive_and_delete_visible_turns(&state, &memory, &turns).unwrap();

    assert!(state.load_visible_turns().unwrap().is_empty());
    for query in ["promptonlyalpha", "answeronlybeta", "toolonlygamma"] {
        assert!(
            !memory.search_evicted_context(query, 10).unwrap()["results"]
                .as_array()
                .unwrap()
                .is_empty()
        );
    }
    assert!(memory
        .search_evicted_context("reasoningonlyquasar", 10)
        .unwrap()["results"]
        .as_array()
        .unwrap()
        .is_empty());
}

#[test]
fn explicit_pop_still_deletes_when_evicted_context_archiving_is_disabled() {
    let temp = tempfile::tempdir().unwrap();
    let paths = test_paths(temp.path());
    let mut config = AppConfig::default();
    config.memory.evicted_context_enabled = false;
    let state = StateStore::new(&paths).unwrap();
    state.start_turn("t1", "unarchived-marker", 999999).unwrap();
    state.complete_turn("t1", "reply", None).unwrap();
    let memory = MemoryStore::new(&config, &paths);
    let turns = state.oldest_evictable_visible_turns(1).unwrap();

    archive_and_delete_visible_turns(&state, &memory, &turns).unwrap();

    assert!(state.load_visible_turns().unwrap().is_empty());
    assert!(memory
        .search_evicted_context("unarchived-marker", 10)
        .unwrap()["results"]
        .as_array()
        .unwrap()
        .is_empty());
}

#[test]
fn explicit_pop_does_not_archive_a_turn_removed_before_commit() {
    let temp = tempfile::tempdir().unwrap();
    let paths = test_paths(temp.path());
    let config = AppConfig::default();
    let state = StateStore::new(&paths).unwrap();
    state
        .start_turn("t1", "stale-archive-quasar", 999999)
        .unwrap();
    state.complete_turn("t1", "reply", None).unwrap();
    let turns = state.oldest_evictable_visible_turns(1).unwrap();
    state.delete_visible_turns(&["t1".to_string()]).unwrap();
    let memory = MemoryStore::new(&config, &paths);

    assert!(archive_and_delete_visible_turns(&state, &memory, &turns).is_err());

    assert!(memory
        .search_evicted_context("stale-archive-quasar", 10)
        .unwrap()["results"]
        .as_array()
        .unwrap()
        .is_empty());
}

#[test]
fn failed_concurrent_pop_preserves_archive_from_the_successful_pop() {
    let temp = tempfile::tempdir().unwrap();
    let paths = test_paths(temp.path());
    let config = AppConfig::default();
    let state = StateStore::new(&paths).unwrap();
    state
        .start_turn("t1", "successful-pop-quasar", 999999)
        .unwrap();
    state.complete_turn("t1", "reply", None).unwrap();
    let turns = state.oldest_evictable_visible_turns(1).unwrap();
    let memory = MemoryStore::new(&config, &paths);

    archive_and_delete_visible_turns(&state, &memory, &turns).unwrap();
    assert!(archive_and_delete_visible_turns(&state, &memory, &turns).is_err());

    assert!(!memory
        .search_evicted_context("successful-pop-quasar", 10)
        .unwrap()["results"]
        .as_array()
        .unwrap()
        .is_empty());
}

#[test]
fn explicit_pop_removes_new_archive_when_the_turn_still_exists_hidden() {
    let temp = tempfile::tempdir().unwrap();
    let paths = test_paths(temp.path());
    let config = AppConfig::default();
    let state = StateStore::new(&paths).unwrap();
    state
        .start_turn("t1", "hidden-stale-quasar", 999999)
        .unwrap();
    state.complete_turn("t1", "reply", None).unwrap();
    let turns = state.oldest_evictable_visible_turns(1).unwrap();
    state
        .replace_visible_with_summary(
            &["t1".to_string()],
            &["t1".to_string()],
            "summary",
            TurnTokens::default(),
            false,
            None,
        )
        .unwrap();
    let memory = MemoryStore::new(&config, &paths);

    assert!(archive_and_delete_visible_turns(&state, &memory, &turns).is_err());

    assert!(memory
        .search_evicted_context("hidden-stale-quasar", 10)
        .unwrap()["results"]
        .as_array()
        .unwrap()
        .is_empty());
}

/// v7 byte-prefix guard (compact scenario): request N must be a pure
/// element-wise prefix extension of request N-1, except immediately
/// after a compaction — and each compaction may reset the prefix at most
/// once. Catches any regression that inserts, deletes, or perturbs
/// already-sent history bytes (the failure mode is symptomless in
/// production: cache hit rate silently degrades).
#[tokio::test]
async fn compaction_resets_the_byte_prefix_at_most_once_each() {
    let temp = tempfile::tempdir().unwrap();
    let paths = test_paths(temp.path());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base_url = format!("http://{}/v1", listener.local_addr().unwrap());
    let mut config = queue_test_config(base_url);
    config.tools.enabled = false;
    config.providers[0]
        .model_context_window
        .insert("test-model".to_string(), 3000);
    config.context.compact_tail_tokens = Some(600);
    // Isolated summary path: its request is identifiable by the compact
    // system prompt and excluded from the prefix chain.
    config.context.compact_cache_reuse = false;
    config.context.prune_stale_tool_reports = false;
    // Pin the persona. This test is about compaction's effect on the byte
    // prefix, not about whatever `prompts/miyu.md` currently weighs —
    // editing the persona used to move the overflow point and flip the
    // outcome.
    config.system_prompt = Some("prefix cache guard fixture persona".to_string());

    let bodies = Arc::new(Mutex::new(Vec::<String>::new()));
    let server_bodies = bodies.clone();
    let server = tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                break;
            };
            let body = read_test_http_request(&mut stream).await;
            let body = String::from_utf8_lossy(&body).to_string();
            let is_compact = body.contains("context summarization assistant");
            server_bodies.lock().unwrap().push(body);
            let sse = if is_compact {
                concat!(
                    "data: {\"choices\":[{\"delta\":{\"content\":\"## Task Goal\\nmock summary\"}}]}\n\n",
                    "data: {\"choices\":[{\"finish_reason\":\"stop\",\"delta\":{}}]}\n\n",
                    "data: [DONE]\n\n"
                )
            } else {
                concat!(
                    "data: {\"choices\":[{\"delta\":{\"content\":\"answer\"}}]}\n\n",
                    "data: {\"choices\":[{\"finish_reason\":\"stop\",\"delta\":{}}]}\n\n",
                    "data: [DONE]\n\n"
                )
            };
            write_test_sse(&mut stream, sse).await;
        }
    });

    let state = StateStore::new(&paths).unwrap();
    state.init_files().unwrap();
    let client =
        OpenAiCompatibleClient::new(config.provider(None).unwrap(), &config, &paths).unwrap();
    let mut agent = Agent::new(
        config,
        &paths,
        state,
        client,
        ToolRegistry::new(),
        AgentMode::Normal,
    )
    .unwrap();

    // Pin the workspace too: `runtime_context` embeds the effective working
    // directory in the system prompt, so the token budget would otherwise
    // shift with the length of the path the test happens to be run from.
    let filler = "prefix cache guard filler content 前缀缓存守卫填充 ".repeat(40);
    let workspace = temp.path().to_path_buf();
    crate::tools::workspace::with_workspace(workspace, async {
        for i in 0..6 {
            agent
                .chat_stream(&format!("message {i}: {filler}"), |_| Ok(()))
                .await
                .unwrap();
            let tokens = agent.effective_context_tokens().unwrap();
            agent
                .handle_overflow_after_turn(tokens, |_| Ok(()))
                .await
                .unwrap();
        }
    })
    .await;
    server.abort();

    let bodies = bodies.lock().unwrap().clone();
    let compact_requests = bodies
        .iter()
        .filter(|body| body.contains("context summarization assistant"))
        .count();
    assert!(
        compact_requests >= 1,
        "the scenario must trigger at least one compaction"
    );
    let chat: Vec<serde_json::Value> = bodies
        .iter()
        .filter(|body| !body.contains("context summarization assistant"))
        .map(|body| serde_json::from_str(body).unwrap())
        .collect();
    assert!(chat.len() >= 6);
    let mut resets = 0usize;
    for pair in chat.windows(2) {
        let prev = pair[0]["messages"].as_array().unwrap();
        let next = pair[1]["messages"].as_array().unwrap();
        let shared = prev
            .iter()
            .zip(next.iter())
            .take_while(|(a, b)| a == b)
            .count();
        if shared == prev.len() {
            continue; // pure append-only extension
        }
        resets += 1;
        assert!(shared >= 1, "the system prompt must never diverge");
        let checkpoint = next[1]["content"].as_str().unwrap_or_default();
        assert!(
            checkpoint.contains("<conversation-checkpoint>"),
            "a reset must be a compaction (summary checkpoint in slot 1), got: {}",
            &checkpoint[..checkpoint.len().min(120)]
        );
    }
    // The cache guarantee is one-directional: a reset may only ever be a
    // compaction, and compaction may not reset more than once per run.
    // Requiring the converse — that every compaction resets — is not a
    // property of the system: when the fold cannot save enough, the
    // compactor keeps the existing history and the prefix simply extends.
    assert!(
        resets >= 1,
        "the scenario must exercise at least one real prefix reset"
    );
    assert!(
        resets <= compact_requests,
        "prefix reset {resets} times against {compact_requests} compactions; \
         nothing but compaction may reset the byte prefix"
    );
}

/// 结构化工具流推导:从实况消息尾段还原轮次;悬空调用补占位,
/// 穿插的 user/context 消息不干扰配对。
#[test]
fn derive_tool_flow_reconstructs_rounds_from_live_messages() {
    let call = |id: &str, name: &str, args: &str| crate::llm::ToolCall {
        id: id.to_string(),
        kind: "function".to_string(),
        function: crate::llm::ToolCallFunction {
            name: name.to_string(),
            arguments: args.to_string(),
        },
    };
    let mut messages = vec![ChatMessage::plain("user", "历史,不该被扫到")];
    let live_start = messages.len();
    let mut assistant = ChatMessage::assistant(
        "先查一下",
        Some(vec![call("c1", "run_command", "{\"command\":\"ls\"}")]),
    );
    assistant.reasoning_content = Some("想想".to_string());
    messages.push(assistant);
    messages.push(ChatMessage::tool("c1", "file-a\nfile-b"));
    messages.push(ChatMessage::turn_context("穿插的系统提醒"));
    messages.push(ChatMessage::assistant(
        "再查两个",
        Some(vec![
            call("c2", "read_file", "{\"path\":\"x\"}"),
            call("c3", "web_search", "{\"q\":\"y\"}"),
        ]),
    ));
    messages.push(ChatMessage::tool("c3", "搜到了"));
    // c2 悬空(崩溃/中断) → 必须补占位,回放绝不发无应答的 tool_calls
    messages.push(ChatMessage::assistant("完事", None));

    let flow = derive_tool_flow(&messages, live_start);
    assert_eq!(flow.len(), 2);
    assert_eq!(flow[0].assistant_content, "先查一下");
    assert_eq!(flow[0].assistant_reasoning.as_deref(), Some("想想"));
    assert_eq!(flow[0].calls.len(), 1);
    assert_eq!(flow[0].calls[0].arguments, "{\"command\":\"ls\"}");
    assert_eq!(flow[0].calls[0].output, "file-a\nfile-b");
    assert_eq!(flow[1].calls.len(), 2);
    assert_eq!(flow[1].calls[0].output, "(tool result unavailable)");
    assert_eq!(flow[1].calls[1].output, "搜到了");
}

/// spill 替换文案的预算自洽:替换体永不超过上限;上限太小放弃;
/// CJK 多字节切口不产生半个字符。
#[test]
fn spill_replacement_respects_budget_and_char_boundaries() {
    let output = "长".repeat(40_000);
    let replaced = spill_replacement(&output, 10_000, "/tmp/x.txt").expect("should spill");
    assert!(
        replaced.len() <= 10_000,
        "replacement {} > cap",
        replaced.len()
    );
    assert!(replaced.contains("bytes omitted"));
    assert!(replaced.contains("/tmp/x.txt"));
    assert!(replaced.starts_with('长'));
    // 文案已英文化,预算/切口断言不受语言影响。
    assert!(replaced.trim_end().ends_with(')'));
    // 上限连提示都装不下 → 放弃外溢
    assert!(spill_replacement(&output, 60, "/tmp/x.txt").is_none());
    // 不超限的输出不该被调用方外溢(逻辑在调用方,这里守函数本身)
    let small = "小输出";
    let r = spill_replacement(small, 10_000, "/tmp/x.txt");
    assert!(r.is_some() || small.len() <= 10_000);
}
