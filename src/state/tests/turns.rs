//! 回合生命周期、打断与陈旧恢复。

use super::shared::*;
use crate::state::*;

#[test]
fn turn_lifecycle() {
    let temp = tempfile::tempdir().unwrap();
    let store = StateStore::new(&MiyuPaths {
        root_dir: temp.path().to_path_buf(),
        config_dir: temp.path().join("config"),
        config_file: temp.path().join("config/config.jsonc"),
        skills_dir: temp.path().join("config/skills"),
        data_dir: temp.path().join("data"),
        cache_dir: temp.path().join("cache"),
        state_dir: temp.path().join("state"),
        pictures_dir: temp.path().join("pictures"),
        fish_hook_file: temp.path().join("fish/miyu.fish"),
        bash_hook_file: temp.path().join("shell/bash-hook.sh"),
        zsh_hook_file: temp.path().join("shell/zsh-hook.zsh"),
        scripts_dir: temp.path().join("config/scripts"),
        system_scripts_dir: PathBuf::new(),
    })
    .unwrap();

    store.init_files().unwrap();
    assert!(!temp.path().join("state/miyu.log").exists());

    store.start_turn("turn_1", "hello", 999999).unwrap();
    let turns = store.load_turns().unwrap();
    assert_eq!(turns.len(), 1);
    assert_eq!(turns[0].status, TurnStatus::Running);
    assert_eq!(turns[0].assistant_content, pending_placeholder());

    store.complete_turn("turn_1", "hi there", None).unwrap();
    let turns = store.load_turns().unwrap();
    assert_eq!(turns[0].status, TurnStatus::Completed);
    assert_eq!(turns[0].assistant_content, "hi there");
}

#[test]
fn question_exchange_persists_with_user_role_history() {
    let temp = tempfile::tempdir().unwrap();
    let store = StateStore::new(&MiyuPaths {
        root_dir: temp.path().to_path_buf(),
        config_dir: temp.path().join("config"),
        config_file: temp.path().join("config/config.jsonc"),
        skills_dir: temp.path().join("config/skills"),
        data_dir: temp.path().join("data"),
        cache_dir: temp.path().join("cache"),
        state_dir: temp.path().join("state"),
        pictures_dir: temp.path().join("pictures"),
        fish_hook_file: temp.path().join("fish/miyu.fish"),
        bash_hook_file: temp.path().join("shell/bash-hook.sh"),
        zsh_hook_file: temp.path().join("shell/zsh-hook.zsh"),
        scripts_dir: temp.path().join("config/scripts"),
        system_scripts_dir: PathBuf::new(),
    })
    .unwrap();
    store.start_turn("turn_1", "配置它", 999999).unwrap();
    let request = crate::question::QuestionRequest {
        questions: vec![crate::question::QuestionPrompt {
            header: "范围".to_string(),
            question: "修改哪些部分？".to_string(),
            options: vec![crate::question::QuestionOption {
                label: "全部".to_string(),
                description: String::new(),
            }],
            multiple: false,
            custom: true,
        }],
    };
    let exchange =
        crate::question::QuestionExchange::new(request, vec![vec!["全部".to_string()]]).unwrap();
    store.append_question_exchange("turn_1", &exchange).unwrap();
    store.complete_turn("turn_1", "已经配置。", None).unwrap();

    let turns = store.load_turns().unwrap();
    assert_eq!(turns[0].question_exchanges, vec![exchange]);
    let history = store.load_conversation().unwrap();
    assert_eq!(history[1].role, "assistant_clarification");
    assert_eq!(history[2].role, "user_clarification");
    assert!(history[2].content.contains("全部"));
}

#[test]
fn interrupt_turn() {
    let temp = tempfile::tempdir().unwrap();
    let store = StateStore::new(&MiyuPaths {
        root_dir: temp.path().to_path_buf(),
        config_dir: temp.path().join("config"),
        config_file: temp.path().join("config/config.jsonc"),
        skills_dir: temp.path().join("config/skills"),
        data_dir: temp.path().join("data"),
        cache_dir: temp.path().join("cache"),
        state_dir: temp.path().join("state"),
        pictures_dir: temp.path().join("pictures"),
        fish_hook_file: temp.path().join("fish/miyu.fish"),
        bash_hook_file: temp.path().join("shell/bash-hook.sh"),
        zsh_hook_file: temp.path().join("shell/zsh-hook.zsh"),
        scripts_dir: temp.path().join("config/scripts"),
        system_scripts_dir: PathBuf::new(),
    })
    .unwrap();

    store.start_turn("turn_1", "do something", 999999).unwrap();
    store.interrupt_turn("turn_1").unwrap();
    let turns = store.load_turns().unwrap();
    assert_eq!(turns[0].status, TurnStatus::Interrupted);
    assert_eq!(turns[0].assistant_content, interrupted_text());
}

/// 并发回合完成序追加:与已完成回合重叠的回合在完成/中断时移到
/// 会话末尾,已完成历史跨请求 append-only,不再出现插入型缓存
/// 断点;无重叠回合与 redo 修订保持原位。
#[test]
fn overlapping_turns_reorder_to_completion_order() {
    let (_temp, store) = test_store();
    // A 先开跑,B 后开但先答完(群聊并发形态)——回放顺序按完成序。
    store.start_turn("turn_a", "先来的", 999999).unwrap();
    store.start_turn("turn_b", "后来的", 999999).unwrap();
    store.complete_turn("turn_b", "B 先答完", None).unwrap();
    store.complete_turn("turn_a", "A 后答完", None).unwrap();
    let turns = store.load_turns().unwrap();
    let order = turns
        .iter()
        .map(|turn| turn.turn_id.as_str())
        .collect::<Vec<_>>();
    assert_eq!(order, ["turn_b", "turn_a"]);

    // 无重叠的后续回合不发生无谓跳位。
    store.start_turn("turn_c", "单独回合", 999999).unwrap();
    store.complete_turn("turn_c", "顺序完成", None).unwrap();
    let turns = store.load_turns().unwrap();
    assert_eq!(turns[2].turn_id, "turn_c");
    assert_eq!(turns[2].seq, turns[1].seq + 1);

    // 中断同样是"首次变为可回放",一样追加到末尾。
    store.start_turn("turn_d", "被打断的", 999999).unwrap();
    store.start_turn("turn_e", "插队的", 999999).unwrap();
    store.complete_turn("turn_e", "插队先完", None).unwrap();
    store.interrupt_turn("turn_d").unwrap();
    let turns = store.load_turns().unwrap();
    let order = turns
        .iter()
        .map(|turn| turn.turn_id.as_str())
        .collect::<Vec<_>>();
    assert_eq!(order, ["turn_b", "turn_a", "turn_c", "turn_e", "turn_d"]);

    // redo 修订原位改写:turn_d 重跑完成后位置不动。
    let candidate = store.redo_candidate().unwrap().unwrap();
    assert_eq!(candidate.turn_id, "turn_d");
    let redo = store
        .begin_redo(
            "turn_d",
            "turn_d",
            RedoInputKind::Initial,
            candidate.revision,
            "重打的输入",
            "重打的输入",
            std::process::id(),
        )
        .unwrap();
    store
        .complete_turn_revision_with_usage_and_model(
            "turn_d",
            redo.revision,
            "重答",
            None,
            None,
            None,
            TurnTokens::default(),
            false,
        )
        .unwrap();
    let turns = store.load_turns().unwrap();
    let order = turns
        .iter()
        .map(|turn| turn.turn_id.as_str())
        .collect::<Vec<_>>();
    assert_eq!(order, ["turn_b", "turn_a", "turn_c", "turn_e", "turn_d"]);
}

#[test]
fn interrupted_turn_materializes_persisted_journal_output() {
    let temp = tempfile::tempdir().unwrap();
    let store = StateStore::new(&MiyuPaths {
        root_dir: temp.path().to_path_buf(),
        config_dir: temp.path().join("config"),
        config_file: temp.path().join("config/config.jsonc"),
        skills_dir: temp.path().join("config/skills"),
        data_dir: temp.path().join("data"),
        cache_dir: temp.path().join("cache"),
        state_dir: temp.path().join("state"),
        pictures_dir: temp.path().join("pictures"),
        fish_hook_file: temp.path().join("fish/miyu.fish"),
        bash_hook_file: temp.path().join("shell/bash-hook.sh"),
        zsh_hook_file: temp.path().join("shell/zsh-hook.zsh"),
        scripts_dir: temp.path().join("config/scripts"),
        system_scripts_dir: PathBuf::new(),
    })
    .unwrap();
    store
        .start_turn("turn_journal", "long task", 999999)
        .unwrap();
    store
        .append_turn_journal_event(
            "turn_journal",
            0,
            0,
            "assistant_content",
            None,
            None,
            Some("first persisted part"),
            None,
            None,
        )
        .unwrap();
    store
        .append_turn_journal_event(
            "turn_journal",
            0,
            0,
            "assistant_reasoning",
            None,
            None,
            Some("private reasoning"),
            None,
            None,
        )
        .unwrap();
    store.interrupt_turn("turn_journal").unwrap();

    let turn = store.load_turns().unwrap().remove(0);
    assert_eq!(turn.status, TurnStatus::Interrupted);
    assert!(turn.assistant_content.contains("first persisted part"));
    assert!(turn.assistant_content.contains(interrupted_text()));
    assert_eq!(
        turn.assistant_reasoning.as_deref(),
        Some("private reasoning")
    );
    assert_eq!(turn.journal_events.len(), 2);
}

#[test]
fn superseded_journal_keeps_completed_tool_events_without_partial_text() {
    let (_temp, store) = test_store();
    store.start_turn("superseded", "long task", 999999).unwrap();
    store
        .append_turn_journal_event(
            "superseded",
            0,
            0,
            "assistant_content",
            None,
            None,
            Some("discarded partial answer"),
            None,
            None,
        )
        .unwrap();
    store
        .append_turn_journal_event(
            "superseded",
            0,
            0,
            "tool_call",
            Some("call-1"),
            Some("read_file"),
            Some("{\"path\":\"README.md\"}"),
            None,
            None,
        )
        .unwrap();
    store
        .append_turn_journal_event(
            "superseded",
            0,
            0,
            "tool_result",
            Some("call-1"),
            Some("read_file"),
            Some("completed tool output"),
            None,
            Some(true),
        )
        .unwrap();
    store
        .supersede_turn_journal_segment("superseded", 0, 0)
        .unwrap();

    let turn = store.load_turns().unwrap().remove(0);
    assert!(!turn
        .journal_events
        .iter()
        .any(|event| event.kind == "assistant_content"));
    assert!(turn
        .journal_events
        .iter()
        .any(|event| event.kind == "tool_call"));
    assert!(turn
        .journal_events
        .iter()
        .any(|event| event.kind == "tool_result"));
}

#[test]
fn recover_stale_running() {
    let temp = tempfile::tempdir().unwrap();
    let store = StateStore::new(&MiyuPaths {
        root_dir: temp.path().to_path_buf(),
        config_dir: temp.path().join("config"),
        config_file: temp.path().join("config/config.jsonc"),
        skills_dir: temp.path().join("config/skills"),
        data_dir: temp.path().join("data"),
        cache_dir: temp.path().join("cache"),
        state_dir: temp.path().join("state"),
        pictures_dir: temp.path().join("pictures"),
        fish_hook_file: temp.path().join("fish/miyu.fish"),
        bash_hook_file: temp.path().join("shell/bash-hook.sh"),
        zsh_hook_file: temp.path().join("shell/zsh-hook.zsh"),
        scripts_dir: temp.path().join("config/scripts"),
        system_scripts_dir: PathBuf::new(),
    })
    .unwrap();

    store.start_turn("turn_1", "task a", 999999).unwrap();
    store.start_turn("turn_2", "task b", 999999).unwrap();
    assert!(store.has_running_turns().unwrap());

    let recovered = store.recover_stale_turns().unwrap();
    assert_eq!(recovered, 2);

    let turns = store.load_turns().unwrap();
    assert_eq!(turns.len(), 2);
    assert!(turns.iter().all(|t| t.status == TurnStatus::Interrupted));
}

#[test]
fn recover_stale_skips_alive_owner() {
    let (_temp, store) = test_store();

    let current_pid = std::process::id();
    store
        .start_turn("turn_1", "终端1的prompt", current_pid)
        .unwrap();
    store.start_turn("turn_dead", "孤儿turn", 999999).unwrap();

    let recovered = store.recover_stale_turns().unwrap();
    assert_eq!(recovered, 1);

    let turns = store.load_turns().unwrap();
    let turn1 = turns.iter().find(|t| t.turn_id == "turn_1").unwrap();
    assert_eq!(turn1.status, TurnStatus::Running);
    assert_eq!(turn1.assistant_content, pending_placeholder());

    let dead = turns.iter().find(|t| t.turn_id == "turn_dead").unwrap();
    assert_eq!(dead.status, TurnStatus::Interrupted);
}

#[test]
fn interrupt_keeps_consumed_prompts_attached_to_the_interrupted_turn() {
    let (_temp, store) = test_store();
    store
        .enqueue_prompt("q1", "followup", "followup", &[])
        .unwrap();
    store.start_turn("turn_1", "initial", 999999).unwrap();
    store
        .consume_queued_prompts(
            "turn_1",
            &[("q1".to_string(), "followup".to_string())],
            None,
            None,
        )
        .unwrap();

    store.interrupt_turn("turn_1").unwrap();

    assert!(store.load_queued_prompts().unwrap().is_empty());
    let turns = store.load_turns().unwrap();
    assert_eq!(turns[0].status, TurnStatus::Interrupted);
    assert_eq!(turns[0].followups.len(), 1);
    assert_eq!(turns[0].followups[0].prompt_id, "q1");
}

#[test]
fn stale_turn_recovery_keeps_consumed_prompts_consumed() {
    let (_temp, store) = test_store();
    store
        .enqueue_prompt("q1", "followup", "followup", &[])
        .unwrap();
    store.start_turn("turn_1", "initial", 999999).unwrap();
    store
        .consume_queued_prompts(
            "turn_1",
            &[("q1".to_string(), "followup".to_string())],
            None,
            None,
        )
        .unwrap();

    assert_eq!(store.recover_stale_turns().unwrap(), 1);
    assert!(store.load_queued_prompts().unwrap().is_empty());
    let turns = store.load_turns().unwrap();
    assert_eq!(turns[0].status, TurnStatus::Interrupted);
    assert_eq!(turns[0].followups[0].prompt_id, "q1");
}

#[test]
fn stale_turn_recovery_consumes_accepted_queued_prompts() {
    let (_temp, store) = test_store();
    store.start_turn("turn_1", "initial", 999999).unwrap();
    store
        .append_turn_journal_event(
            "turn_1",
            0,
            0,
            "assistant_content",
            None,
            None,
            Some("partial answer"),
            None,
            None,
        )
        .unwrap();
    let target = store.running_turn_queue_target().unwrap().unwrap();
    store
        .enqueue_prompt_for_target(&target, "q1", "followup", "followup", &[])
        .unwrap();

    assert_eq!(store.recover_stale_turns().unwrap(), 1);
    assert!(store
        .load_queued_prompts_for_target(&target)
        .unwrap()
        .is_empty());
    let turn = store.load_turns().unwrap().remove(0);
    assert_eq!(turn.status, TurnStatus::Interrupted);
    assert_eq!(turn.followups.len(), 1);
    assert_eq!(turn.followups[0].prompt_id, "q1");
    assert_eq!(
        turn.followups[0].preceding_assistant_content.as_deref(),
        Some("partial answer")
    );
    assert!(turn
        .journal_events
        .iter()
        .any(|event| event.kind == "queued_prompts_consumed"));
}

#[test]
fn finished_turn_cleanup_preserves_a_late_queued_prompt() {
    let (_temp, store) = test_store();
    store
        .start_turn("turn_1", "initial", std::process::id())
        .unwrap();
    store.complete_turn("turn_1", "answer", None).unwrap();
    store
        .enqueue_prompt("late", "followup", "followup", &[])
        .unwrap();

    assert_eq!(store.discard_queued_prompts().unwrap(), 1);
    let turn = store.load_turns().unwrap().remove(0);
    assert_eq!(turn.followups.len(), 1);
    assert_eq!(turn.followups[0].prompt_id, "late");
    assert_eq!(
        turn.followups[0].preceding_assistant_content.as_deref(),
        Some("answer")
    );
}

#[test]
fn cancelled_turn_cleanup_deletes_queued_prompts_without_folding() {
    let (_temp, store) = test_store();
    store
        .start_turn("turn_1", "initial", std::process::id())
        .unwrap();
    store
        .enqueue_prompt("q1", "排队消息", "排队消息", &[])
        .unwrap();
    store.interrupt_turn("turn_1").unwrap();

    let dropped = store.delete_queued_prompts().unwrap();
    assert_eq!(dropped, vec!["q1".to_string()]);
    // Neither still queued nor folded into the turn as a follow-up.
    assert!(store.load_queued_prompts().unwrap().is_empty());
    let turn = store.load_turns().unwrap().remove(0);
    assert!(turn.followups.is_empty());
    // Idempotent on an already-empty queue.
    assert!(store.delete_queued_prompts().unwrap().is_empty());
}

#[test]
fn undo_removes_last_turn() {
    let temp = tempfile::tempdir().unwrap();
    let store = StateStore::new(&MiyuPaths {
        root_dir: temp.path().to_path_buf(),
        config_dir: temp.path().join("config"),
        config_file: temp.path().join("config/config.jsonc"),
        skills_dir: temp.path().join("config/skills"),
        data_dir: temp.path().join("data"),
        cache_dir: temp.path().join("cache"),
        state_dir: temp.path().join("state"),
        pictures_dir: temp.path().join("pictures"),
        fish_hook_file: temp.path().join("fish/miyu.fish"),
        bash_hook_file: temp.path().join("shell/bash-hook.sh"),
        zsh_hook_file: temp.path().join("shell/zsh-hook.zsh"),
        scripts_dir: temp.path().join("config/scripts"),
        system_scripts_dir: PathBuf::new(),
    })
    .unwrap();

    store.start_turn("turn_1", "hello", 999999).unwrap();
    store.complete_turn("turn_1", "hi", None).unwrap();
    store.start_turn("turn_2", "bye", 999999).unwrap();
    store.complete_turn("turn_2", "goodbye", None).unwrap();

    let (removed, prompt) = store.undo_last_turn().unwrap();
    assert_eq!(removed, 1);
    assert_eq!(prompt.as_deref(), Some("bye"));

    let turns = store.load_turns().unwrap();
    assert_eq!(turns.len(), 1);
    assert_eq!(turns[0].turn_id, "turn_1");
}

#[test]
fn finished_turns_keep_a_replayable_transcript() {
    let (_temp, store) = test_store();
    store.init_files().unwrap();
    store.start_turn("t1", "改一下 README", 999_999).unwrap();
    let db = store.conv_db();
    for (kind, call_id, name, payload, ok) in [
        ("assistant_content", None, None, Some("这就去改。"), None),
        (
            "tool_call",
            Some("c1"),
            Some("edit_string"),
            Some("{\"path\":\"README.md\"}"),
            None,
        ),
        (
            "tool_result",
            Some("c1"),
            None,
            Some("1 处替换"),
            Some(true),
        ),
        ("tool_progress", Some("c1"), None, Some("忽略我"), None),
        ("assistant_content", None, None, Some("改好了。"), None),
    ] {
        db.append_turn_journal_event("t1", 0, 0, kind, call_id, name, payload, None, ok)
            .unwrap();
    }
    store.complete_turn("t1", "改好了。", None).unwrap();

    let replays = store.session_replay(5).unwrap();
    assert_eq!(replays.len(), 1);
    let entries = &replays[0].entries;
    assert_eq!(replays[0].display_content, "改一下 README");
    // Prose and tool blocks keep their original interleaving, and the
    // live-only progress ticks are gone.
    assert_eq!(
        entries,
        &vec![
            ReplayEntry::Text {
                text: "这就去改。".to_string()
            },
            ReplayEntry::ToolCall {
                name: "edit_string".to_string(),
                arguments: "{\"path\":\"README.md\"}".to_string(),
            },
            ReplayEntry::ToolResult {
                name: "edit_string".to_string(),
                ok: true,
                output: "1 处替换".to_string(),
            },
            ReplayEntry::Text {
                text: "改好了。".to_string()
            },
        ]
    );

    // A turn without a stored transcript still replays its reply.
    store.start_turn("t2", "再问一句", 999_999).unwrap();
    store.complete_turn("t2", "好的。", None).unwrap();
    let replays = store.session_replay(5).unwrap();
    assert_eq!(replays.len(), 2);
    assert!(replays[1].entries.is_empty());
    assert_eq!(replays[1].assistant_content, "好的。");
    // Oldest first, so the caller can print them top to bottom.
    assert_eq!(replays[0].display_content, "改一下 README");
    assert!(replays.iter().all(|replay| !replay.is_synthetic));

    // A background-job wake turn is daemon-synthesized: the replay must be
    // able to tell it apart so it is not drawn as something the user typed.
    store
        .start_turn_with_display(
            "t3",
            "<background-job-report>子代理「后台测试A」已执行完毕</background-job-report>",
            "[后台任务完成] 子代理完成 82bea3 · 后台测试A",
            999_999,
            None,
        )
        .unwrap();
    store.complete_turn("t3", "跑完了。", None).unwrap();
    let replays = store.session_replay(5).unwrap();
    assert_eq!(replays.len(), 3);
    assert!(replays[2].is_synthetic);
    assert_eq!(
        replays[2].display_content,
        "[后台任务完成] 子代理完成 82bea3 · 后台测试A"
    );
}

#[test]
fn interrupted_turn_is_evictable_but_summary_and_running_turn_are_not() {
    let (_temp, store) = test_store();
    store
        .insert_summary_turn(
            "summary",
            TurnTokens {
                total: 1,
                ..Default::default()
            },
            false,
        )
        .unwrap();
    store.start_turn("completed", "completed", 999999).unwrap();
    store.complete_turn("completed", "reply", None).unwrap();
    store
        .start_turn("interrupted", "interrupted", 999999)
        .unwrap();
    store.interrupt_turn("interrupted").unwrap();
    store
        .start_turn("running", "pending", std::process::id())
        .unwrap();

    let evicted = store.oldest_evictable_visible_turns(10).unwrap();
    assert_eq!(
        evicted
            .iter()
            .map(|turn| turn.turn_id.as_str())
            .collect::<Vec<_>>(),
        vec!["completed", "interrupted"]
    );
    assert_eq!(evicted[1].status, TurnStatus::Interrupted);
}

/// 量尺：`cargo test --lib state::tests::turns::tool_report_write_amplification -- --ignored --nocapture`
///
/// 改前 `append_tool_report` 每追加一条都要「读整列 → 解析 → push → 整个序列化
/// → 写回」：第 k 次写回的是当前全部 k 条，总写入 O(N²)。v25 之后是往
/// `turn_tool_reports` 子表 INSERT 一行，**一次读都没有**。
///
/// 两种算法都在这里真跑一遍（不是套公式），量的是耗时和实际写出去的字节。
#[test]
#[ignore]
fn tool_report_write_amplification() {
    use rusqlite::{params, Connection};

    /// 改前：JSON 列的读-改-写。
    fn append_by_rewriting(conn: &Connection, turn_id: &str, report: &str) -> usize {
        let existing: String = conn
            .query_row(
                "SELECT tool_reports FROM t WHERE turn_id = ?1",
                params![turn_id],
                |row| row.get(0),
            )
            .unwrap();
        let mut reports: Vec<String> = serde_json::from_str(&existing).unwrap_or_default();
        reports.push(report.to_string());
        let encoded = serde_json::to_string(&reports).unwrap();
        let written = encoded.len();
        conn.execute(
            "UPDATE t SET tool_reports = ?1 WHERE turn_id = ?2",
            params![encoded, turn_id],
        )
        .unwrap();
        written
    }

    /// 改后：子表 INSERT。
    fn append_by_inserting(conn: &Connection, turn_id: &str, report: &str) -> usize {
        conn.execute(
            "INSERT INTO c (turn_id, report) VALUES (?1, ?2)",
            params![turn_id, report],
        )
        .unwrap();
        report.len()
    }

    println!("\n  报告数    改前 写入/耗时          改后 写入/耗时        放大比");
    let report = "x".repeat(2 * 1024);
    for count in [10usize, 20, 40, 80] {
        let mut measured = Vec::new();
        for rewriting in [true, false] {
            let conn = Connection::open_in_memory().unwrap();
            conn.execute_batch(
                "CREATE TABLE t(turn_id TEXT PRIMARY KEY, tool_reports TEXT NOT NULL);
                 CREATE TABLE c(report_id INTEGER PRIMARY KEY, turn_id TEXT NOT NULL,
                                report TEXT NOT NULL);
                 CREATE INDEX ix ON c(turn_id, report_id);
                 INSERT INTO t VALUES('probe', '[]');",
            )
            .unwrap();
            let started = std::time::Instant::now();
            let mut bytes = 0usize;
            for _ in 0..count {
                bytes += if rewriting {
                    append_by_rewriting(&conn, "probe", &report)
                } else {
                    append_by_inserting(&conn, "probe", &report)
                };
            }
            measured.push((bytes, started.elapsed().as_secs_f64() * 1000.0));
        }
        let ((before_bytes, before_ms), (after_bytes, after_ms)) = (measured[0], measured[1]);
        println!(
            "  {count:>6}  {:>8.0} KB {before_ms:>7.1} ms   {:>8.0} KB {after_ms:>7.2} ms  {:>6.1}×",
            before_bytes as f64 / 1024.0,
            after_bytes as f64 / 1024.0,
            before_bytes as f64 / after_bytes as f64,
        );
    }
}

/// 量尺：`cargo test --lib state::tests::turns::history_limit_scaling -- --ignored --nocapture`
///
/// `history(limit)` 把整个会话的所有回合（含每个回合的子表挂载）全读出来，
/// 再取尾部 limit 条。会话越长，`miyu history -n 10` 越慢。
#[test]
#[ignore]
fn history_limit_scaling() {
    println!("\n  会话回合数   history(10) 耗时(ms)");
    for turns in [50usize, 100, 200, 400] {
        let (_temp, store) = test_store();
        for index in 0..turns {
            let turn_id = format!("turn{index}");
            store
                .start_turn(&turn_id, &format!("问题 {index}"), std::process::id())
                .unwrap();
            store
                .complete_turn(&turn_id, &format!("回答 {index}"), None)
                .unwrap();
        }
        let start = std::time::Instant::now();
        let got = store.history(10).unwrap();
        let ms = start.elapsed().as_secs_f64() * 1000.0;
        assert!(got.len() <= 20, "history(10) 返回条目数异常：{}", got.len());
        println!("  {turns:>10}   {ms:>18.2}");
    }
}

// ── v25：工具报告子表 ────────────────────────────────────────────

/// 追加进去的顺序必须原样读回来。子表用 `report_id` 自增排序，写入端不查
/// `MAX(seq)`——顺序全靠这个，所以得钉死。
#[test]
fn tool_reports_come_back_in_the_order_they_were_appended() {
    let (_temp, store) = test_store();
    store.start_turn("t1", "hello", std::process::id()).unwrap();
    for index in 0..25 {
        store
            .append_persisted_context("t1", &format!("报告 {index}"))
            .unwrap();
    }
    store.complete_turn("t1", "done", None).unwrap();

    let turns = store.load_visible_turns().unwrap();
    let turn = turns.iter().find(|turn| turn.turn_id == "t1").unwrap();
    let expected: Vec<String> = (0..25).map(|index| format!("报告 {index}")).collect();
    assert_eq!(turn.tool_reports, expected);
}

/// 批量追加跟单条追加混着来，顺序照样是调用顺序。
#[test]
fn batched_and_single_appends_keep_one_order() {
    let (_temp, store) = test_store();
    store.start_turn("t1", "hello", std::process::id()).unwrap();
    store.append_persisted_context("t1", "a").unwrap();
    store
        .append_persisted_contexts("t1", &["b".to_string(), "c".to_string()])
        .unwrap();
    store.append_persisted_context("t1", "d").unwrap();
    store.complete_turn("t1", "done", None).unwrap();

    let turns = store.load_visible_turns().unwrap();
    let turn = turns.iter().find(|turn| turn.turn_id == "t1").unwrap();
    assert_eq!(turn.tool_reports, ["a", "b", "c", "d"]);
}

/// 跨升级的回合：老报告在 JSON 列里，新报告在子表里，读回来必须**先列后子表**
/// ——那正是它们真实发生的顺序。v25 不回填不删列，所以这种回合确实存在。
#[test]
fn legacy_column_reports_come_before_child_table_ones() {
    let (temp, store) = test_store();
    store.start_turn("t1", "hello", std::process::id()).unwrap();
    // 直接写列，模拟 v25 之前写下的报告
    {
        let conn = rusqlite::Connection::open(temp.path().join("state/conversation.db")).unwrap();
        conn.execute(
            "UPDATE turns SET tool_reports = ?1 WHERE turn_id = 't1'",
            rusqlite::params![r#"["老报告1","老报告2"]"#],
        )
        .unwrap();
    }
    store.append_persisted_context("t1", "新报告").unwrap();
    store.complete_turn("t1", "done", None).unwrap();

    let turns = store.load_visible_turns().unwrap();
    let turn = turns.iter().find(|turn| turn.turn_id == "t1").unwrap();
    assert_eq!(turn.tool_reports, ["老报告1", "老报告2", "新报告"]);
}

/// 删回合要连带删掉子表行，不能留孤儿。靠 `ON DELETE CASCADE` + 打开时的
/// `PRAGMA foreign_keys = ON`。
#[test]
fn deleting_a_turn_takes_its_reports_with_it() {
    let (temp, store) = test_store();
    store.start_turn("t1", "hello", std::process::id()).unwrap();
    store
        .append_persisted_context("t1", "会被一起删掉")
        .unwrap();
    store.complete_turn("t1", "done", None).unwrap();

    let db = temp.path().join("state/conversation.db");
    let count = |path: &std::path::Path| -> i64 {
        let conn = rusqlite::Connection::open(path).unwrap();
        conn.query_row("SELECT COUNT(*) FROM turn_tool_reports", [], |row| {
            row.get(0)
        })
        .unwrap()
    };
    assert_eq!(count(&db), 1);
    {
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute_batch("PRAGMA foreign_keys = ON;").unwrap();
        conn.execute("DELETE FROM turns WHERE turn_id = 't1'", [])
            .unwrap();
    }
    assert_eq!(count(&db), 0, "回合删了但报告还留着");
}
