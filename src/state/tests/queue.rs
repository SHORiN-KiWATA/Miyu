//! 排队消息的持久化与清理。

use super::shared::*;
use crate::state::*;

#[test]
fn queued_prompts_persist_and_attach_to_a_turn_in_order() {
    let (_temp, store) = test_store();
    let first = store
        .enqueue_prompt(
            "q1",
            "first expanded",
            "first",
            &[QueuedPromptAttachment::Path {
                path: "/tmp/image.png".to_string(),
            }],
        )
        .unwrap();
    let second = store
        .enqueue_prompt("q2", "second expanded", "second", &[])
        .unwrap();

    assert!(first.seq < second.seq);
    assert_eq!(
        store.load_queued_prompts().unwrap(),
        vec![first.clone(), second]
    );

    store.start_turn("t1", "initial", 999999).unwrap();
    store
        .consume_queued_prompts(
            "t1",
            &[
                ("q1".to_string(), "first context".to_string()),
                ("q2".to_string(), "second context".to_string()),
            ],
            Some("before followup"),
            Some("reasoning before followup"),
        )
        .unwrap();
    store.complete_turn("t1", "final answer", None).unwrap();

    assert!(store.load_queued_prompts().unwrap().is_empty());
    let turns = store.load_turns().unwrap();
    assert_eq!(turns[0].followups.len(), 2);
    assert_eq!(turns[0].followups[0].content, "first context");
    assert_eq!(turns[0].followups[0].attachments, first.attachments);
    assert_eq!(
        turns[0].followups[0]
            .preceding_assistant_reasoning
            .as_deref(),
        Some("reasoning before followup")
    );
    assert!(turns[0].followups[1].preceding_assistant_content.is_none());

    let history = store.load_conversation().unwrap();
    assert_eq!(
        history
            .iter()
            .map(|entry| (entry.role.as_str(), entry.content.as_str()))
            .collect::<Vec<_>>(),
        vec![
            ("user", "initial"),
            ("assistant", "before followup"),
            ("user", "first context"),
            ("user", "second context"),
            ("assistant", "final answer"),
        ]
    );

    store
        .enqueue_prompt("q3", "still queued", "still queued", &[])
        .unwrap();
    store.reset_conversation().unwrap();
    assert!(store.load_queued_prompts().unwrap().is_empty());
}

#[test]
fn running_turn_exposes_its_queue_as_a_cross_process_target() {
    let (temp, owner_store) = test_store();
    owner_store
        .start_turn("running", "still working", std::process::id())
        .unwrap();
    let web_store = StateStore::new(&test_paths(temp.path())).unwrap();

    let target = web_store.running_turn_queue_target().unwrap().unwrap();
    assert_eq!(target.turn_id, "running");
    assert!(target.queue_session_id.is_some());
    assert_eq!(target.owner_pid, Some(std::process::id()));

    let queued = web_store
        .enqueue_prompt_for_target(&target, "followup", "next", "next", &[])
        .unwrap();
    assert_eq!(owner_store.load_queued_prompts().unwrap(), vec![queued]);
}

#[test]
fn independent_process_stores_can_append_and_read_running_turns() {
    let (temp, first_store) = test_store();
    let second_store = StateStore::new(&test_paths(temp.path())).unwrap();

    first_store
        .start_turn("first", "first prompt", std::process::id())
        .unwrap();
    second_store
        .start_turn("second", "second prompt", std::process::id())
        .unwrap();

    let turns = first_store.load_visible_turns().unwrap();
    assert_eq!(turns.len(), 2);
    assert!(turns.iter().all(|turn| turn.status == TurnStatus::Running));
    assert!(turns
        .iter()
        .all(|turn| turn.assistant_content == pending_placeholder()));
}

#[test]
fn queued_prompts_survive_prompt_changes_but_not_a_new_store_session() {
    let (temp, store) = test_store();
    store.reset_if_prompt_changed("system prompt one").unwrap();
    store
        .enqueue_prompt("q1", "queued content", "queued", &[])
        .unwrap();
    store.reset_if_prompt_changed("system prompt two").unwrap();
    assert_eq!(store.load_queued_prompts().unwrap().len(), 1);
    drop(store);

    let paths = MiyuPaths {
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
    };
    let reopened = StateStore::new(&paths).unwrap();
    assert!(reopened.load_queued_prompts().unwrap().is_empty());
}

#[test]
fn prompt_fingerprint_changes_never_delete_history() {
    let (_temp, store) = test_store();
    store
        .reset_if_prompt_changed("persona plus owner identity")
        .unwrap();
    store
        .start_turn("turn", "hello", std::process::id())
        .unwrap();
    store.complete_turn("turn", "reply", None).unwrap();

    store
        .reset_if_prompt_changed_with_compatible(
            "persona only",
            Some("persona plus owner identity"),
        )
        .unwrap();
    assert_eq!(store.load_visible_turns().unwrap().len(), 1);

    // v7 Release 3: a prompt text change is a planned cache cold start and
    // must never destroy conversation data.
    store.reset_if_prompt_changed("different persona").unwrap();
    assert_eq!(store.load_visible_turns().unwrap().len(), 1);
}

#[test]
fn prompt_fingerprints_are_isolated_per_session() {
    let (_temp, store) = test_store();
    let first = store
        .create_session("first", "first", "user", None)
        .unwrap();
    let second = store
        .create_session("second", "second", "user", None)
        .unwrap();
    let first_store = store.pinned(&first.session_id);
    let second_store = store.pinned(&second.session_id);
    first_store.reset_if_prompt_changed("prompt A").unwrap();
    second_store.reset_if_prompt_changed("prompt B").unwrap();
    first_store
        .start_turn("first-turn", "hello", std::process::id())
        .unwrap();
    first_store
        .complete_turn("first-turn", "first reply", None)
        .unwrap();
    second_store
        .start_turn("second-turn", "hello", std::process::id())
        .unwrap();
    second_store
        .complete_turn("second-turn", "second reply", None)
        .unwrap();

    first_store.reset_if_prompt_changed("prompt A").unwrap();
    second_store.reset_if_prompt_changed("prompt B").unwrap();

    assert_eq!(first_store.load_visible_turns().unwrap().len(), 1);
    assert_eq!(second_store.load_visible_turns().unwrap().len(), 1);
}

#[test]
fn stale_queue_cleanup_preserves_another_live_process_session() {
    let (_temp, store) = test_store();
    let live_owner = std::process::id();
    store
        .conv_db
        .enqueue_prompt(
            &store.session_id(),
            None,
            "other-q",
            "content",
            "display",
            &[],
            &[],
            "other-session",
            live_owner,
        )
        .unwrap();
    let different_pid = live_owner.wrapping_add(1).max(1);

    assert_eq!(
        store
            .conv_db
            .discard_stale_queued_prompts("new-session", different_pid)
            .unwrap(),
        0
    );
    assert_eq!(
        store
            .conv_db
            .load_queued_prompts(&store.session_id(), "other-session")
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn normal_session_cleanup_discards_unsent_prompts() {
    let (_temp, store) = test_store();
    store
        .enqueue_prompt("q1", "content", "display", &[])
        .unwrap();

    assert_eq!(store.discard_queued_prompts().unwrap(), 1);
    assert!(store.load_queued_prompts().unwrap().is_empty());
}

/// followup 随发的瞬态尾巴要和正文一起化石化(v31):回放少了它,活体与
/// 化石在 followup 处字节不同,缓存前缀与 CLI 续传链都在这里掰断。
#[test]
fn followup_context_messages_round_trip() {
    let (_temp, store) = test_store();
    store
        .start_turn("t1", "initial", std::process::id())
        .unwrap();
    store
        .enqueue_prompt("q1", "followup", "followup", &[])
        .unwrap();
    let tail = vec![
        crate::llm::ChatMessage::turn_context("<runtime now=\"x\"/>"),
        crate::llm::ChatMessage::turn_context("<context-images>img_1</context-images>"),
    ];
    store
        .consume_queued_prompts_with_checkpoint(
            "t1",
            &[(
                "q1".to_string(),
                "followup".to_string(),
                serde_json::to_string(&tail).unwrap(),
            )],
            Some("before followup"),
            None,
            None,
            None,
            TurnRedoCheckpointPayload {
                replay_messages: Vec::new(),
                prefix_tool_reports: Vec::new(),
                tool_rounds: 0,
                question_rounds: 0,
                loaded_items: Vec::new(),
                prefix_question_count: 0,
                prefix_image_asset_ids: Vec::new(),
                prefix_artifact_asset_ids: Vec::new(),
            },
        )
        .unwrap();
    store.complete_turn("t1", "final", None).unwrap();

    let turns = store.load_turns().unwrap();
    let replayed = turns[0].followups[0].context_messages();
    assert_eq!(replayed.len(), 2);
    assert!(matches!(
        replayed[0].content.as_ref(),
        Some(crate::llm::ChatContent::Text(text)) if text == "<runtime now=\"x\"/>"
    ));
    assert!(matches!(
        replayed[1].content.as_ref(),
        Some(crate::llm::ChatContent::Text(text)) if text.contains("img_1")
    ));
}
