//! 压缩、裁剪与可撤销性。

use super::shared::*;
use crate::state::*;

/// Returns (non-summary fold ids, all visible ids) mirroring what the
/// compactor passes for a full fold of the current history.
fn visible_snapshot(store: &StateStore) -> (Vec<String>, Vec<String>) {
    let turns = store.load_visible_turns().unwrap();
    let fold_ids = turns
        .iter()
        .filter(|turn| !turn.is_summary)
        .map(|turn| turn.turn_id.clone())
        .collect();
    let turn_ids = turns.into_iter().map(|turn| turn.turn_id).collect();
    (fold_ids, turn_ids)
}

#[test]
fn hidden_turns_excluded_from_visible() {
    let (_temp, store) = test_store();
    store.start_turn("t1", "first", 999999).unwrap();
    store.complete_turn("t1", "reply1", None).unwrap();
    store.start_turn("t2", "second", 999999).unwrap();
    store.complete_turn("t2", "reply2", None).unwrap();

    let visible = store.load_visible_turns().unwrap();
    assert_eq!(visible.len(), 2);

    let hidden_count = store.hide_turns_before_seq(visible[0].seq).unwrap();
    assert_eq!(hidden_count, 1);

    let visible_after = store.load_visible_turns().unwrap();
    assert_eq!(visible_after.len(), 1);
    assert_eq!(visible_after[0].turn_id, "t2");

    let all = store.load_turns().unwrap();
    assert_eq!(all.len(), 2);
    assert!(all[0].hidden);
    assert!(!all[1].hidden);
}

#[test]
fn summary_turn_insert_and_load() {
    let (_temp, store) = test_store();
    store.start_turn("t1", "hello", 999999).unwrap();
    store.complete_turn("t1", "hi", None).unwrap();

    store
        .insert_summary_turn(
            "## Task Goal\nDo stuff",
            TurnTokens {
                total: 12,
                ..Default::default()
            },
            true,
        )
        .unwrap();

    let summary = store.load_last_summary().unwrap();
    assert!(summary.is_some());
    let summary = summary.unwrap();
    assert!(summary.is_summary);
    assert!(!summary.hidden);
    assert_eq!(summary.assistant_content, "## Task Goal\nDo stuff");
    assert_eq!(summary.token_total, 12);
    assert!(summary.token_usage_estimated);

    let visible = store.load_visible_turns().unwrap();
    assert_eq!(visible.len(), 2);
    assert!(visible.iter().any(|t| t.is_summary));
    assert!(visible.iter().any(|t| !t.is_summary));
}

#[test]
fn session_loaded_tools_persist_until_reset() {
    let (_temp, store) = test_store();
    store
        .add_session_loaded_tools(&["web_search".to_string()], Some("t1"))
        .unwrap();
    store
        .add_session_loaded_targets(&["group:gaming".to_string()], Some("t1"))
        .unwrap();

    let loaded = store.load_session_loaded_tools().unwrap();
    assert!(loaded.contains("web_search"));

    store.reset_conversation().unwrap();
    assert!(store.load_session_loaded_tools().unwrap().is_empty());
}

#[test]
fn hide_before_seq_hides_old_summary_too() {
    let (_temp, store) = test_store();
    store.start_turn("t1", "old", 999999).unwrap();
    store.complete_turn("t1", "old reply", None).unwrap();
    store
        .insert_summary_turn(
            "summary of old",
            TurnTokens {
                total: 8,
                ..Default::default()
            },
            true,
        )
        .unwrap();
    store.start_turn("t2", "new", 999999).unwrap();
    store.complete_turn("t2", "new reply", None).unwrap();

    let visible = store.load_visible_turns().unwrap();
    assert_eq!(visible.len(), 3);

    let t2_seq = visible.last().unwrap().seq;
    let hidden = store.hide_turns_before_seq(t2_seq).unwrap();
    assert_eq!(hidden, 3);

    let visible_after = store.load_visible_turns().unwrap();
    assert!(visible_after.is_empty());
}

#[test]
fn evictable_turns_are_deleted_only_after_explicit_commit() {
    let (_temp, store) = test_store();
    for i in 0..10 {
        let id = format!("t{i}");
        let content = "x".repeat(1000);
        store.start_turn(&id, &content, 999999).unwrap();
        store.complete_turn(&id, &content, None).unwrap();
    }

    let evicted = store.oldest_evictable_visible_turns(3).unwrap();
    assert_eq!(evicted.len(), 3);
    assert_eq!(store.load_visible_turns().unwrap().len(), 10);

    let ids = evicted
        .iter()
        .map(|turn| turn.turn_id.clone())
        .collect::<Vec<_>>();
    store.delete_visible_turns(&ids).unwrap();

    let visible = store.load_visible_turns().unwrap();
    assert_eq!(visible.len(), 7);
}

#[test]
fn deleting_no_visible_turns_is_a_noop() {
    let (_temp, store) = test_store();
    store.start_turn("t1", "short", 999999).unwrap();
    store.complete_turn("t1", "reply", None).unwrap();

    assert_eq!(store.delete_visible_turns(&[]).unwrap(), 0);

    let visible = store.load_visible_turns().unwrap();
    assert_eq!(visible.len(), 1);
}

#[test]
fn deleting_visible_turns_rolls_back_when_any_id_changed() {
    let (_temp, store) = test_store();
    for id in ["t1", "t2"] {
        store.start_turn(id, id, 999999).unwrap();
        store.complete_turn(id, "reply", None).unwrap();
    }
    store
        .add_session_loaded_tools(&["from_t1".to_string()], Some("t1"))
        .unwrap();
    store
        .add_session_loaded_tools(&["from_t2".to_string()], Some("t2"))
        .unwrap();

    assert!(store
        .delete_visible_turns(&["t1".to_string(), "missing".to_string()])
        .is_err());
    assert_eq!(store.load_visible_turns().unwrap().len(), 2);
    assert_eq!(
        store.load_session_loaded_tools().unwrap(),
        BTreeSet::from(["from_t1".to_string(), "from_t2".to_string()])
    );
}

#[test]
fn checked_pop_rolls_back_when_loaded_tool_sources_change() {
    let (_temp, store) = test_store();
    for id in ["t1", "t2"] {
        store.start_turn(id, id, 999999).unwrap();
        store.complete_turn(id, "reply", None).unwrap();
    }
    store
        .add_session_loaded_tools(&["dynamic_tool".to_string()], Some("t1"))
        .unwrap();
    let expected = store.load_session_loaded_tools_with_sources().unwrap();
    store
        .add_session_loaded_tools(&["dynamic_tool".to_string()], Some("t2"))
        .unwrap();

    assert!(store
        .delete_visible_turns_checked(&["t1".to_string()], Some(&expected))
        .is_err());

    assert_eq!(store.load_visible_turns().unwrap().len(), 2);
    assert_eq!(
        store.load_session_loaded_tools_with_sources().unwrap(),
        vec![("dynamic_tool".to_string(), Some("t2".to_string()))]
    );
}

#[test]
fn deleting_visible_turns_unloads_only_items_sourced_from_deleted_turns() {
    let (_temp, store) = test_store();
    for id in ["t1", "t2"] {
        store.start_turn(id, id, 999999).unwrap();
        store.complete_turn(id, "reply", None).unwrap();
    }
    store
        .add_session_loaded_tools(&["popped_tool".to_string()], Some("t1"))
        .unwrap();
    store
        .add_session_loaded_tools(&["kept_tool".to_string()], Some("t2"))
        .unwrap();
    store
        .add_session_loaded_tools(&["global_tool".to_string()], None)
        .unwrap();
    store
        .add_session_loaded_targets(&["popped_target".to_string()], Some("t1"))
        .unwrap();
    store
        .add_session_loaded_targets(&["kept_target".to_string()], Some("t2"))
        .unwrap();

    assert_eq!(store.delete_visible_turns(&["t1".to_string()]).unwrap(), 1);

    assert_eq!(
        store.load_session_loaded_tools().unwrap(),
        BTreeSet::from(["global_tool".to_string(), "kept_tool".to_string()])
    );
    assert_eq!(
        store
            .conv_db
            .load_session_loaded_items(&store.session_id(), "target")
            .unwrap(),
        BTreeSet::from(["kept_target".to_string()])
    );
}

#[test]
fn compact_is_reversible_with_undo() {
    let (_temp, store) = test_store();
    for id in ["t1", "t2"] {
        store.start_turn(id, id, 999999).unwrap();
        store.complete_turn(id, "reply", None).unwrap();
    }
    let (fold_ids, turn_ids) = visible_snapshot(&store);

    store
        .replace_visible_with_summary(
            &fold_ids,
            &turn_ids,
            "summary",
            TurnTokens {
                total: 10,
                ..Default::default()
            },
            true,
            None,
            None,
        )
        .unwrap();

    let all = store.load_turns().unwrap();
    assert_eq!(all.len(), 3);
    assert!(all[0].hidden && all[1].hidden);
    assert_eq!(store.load_visible_turns().unwrap().len(), 1);
    assert_eq!(
        store
            .load_conversation()
            .unwrap()
            .into_iter()
            .filter(|entry| entry.role == "user")
            .map(|entry| entry.content)
            .collect::<Vec<_>>(),
        vec!["t1", "t2"]
    );

    let (removed, prompt) = store.undo_last_turn().unwrap();
    assert_eq!(removed, 1);
    assert!(prompt.is_none());
    let visible = store.load_visible_turns().unwrap();
    assert_eq!(
        visible
            .iter()
            .map(|turn| turn.turn_id.as_str())
            .collect::<Vec<_>>(),
        vec!["t1", "t2"]
    );
}

#[test]
fn nested_compact_undo_restores_one_layer_at_a_time() {
    let (_temp, store) = test_store();
    for id in ["t1", "t2"] {
        store.start_turn(id, id, 999999).unwrap();
        store.complete_turn(id, "reply", None).unwrap();
    }
    let (fold_ids, turn_ids) = visible_snapshot(&store);
    store
        .replace_visible_with_summary(
            &fold_ids,
            &turn_ids,
            "summary one",
            TurnTokens::default(),
            false,
            None,
            None,
        )
        .unwrap();
    store.start_turn("t3", "third", 999999).unwrap();
    store.complete_turn("t3", "reply", None).unwrap();
    let (fold_ids, turn_ids) = visible_snapshot(&store);
    store
        .replace_visible_with_summary(
            &fold_ids,
            &turn_ids,
            "summary two",
            TurnTokens::default(),
            false,
            None,
            None,
        )
        .unwrap();

    assert_eq!(
        store
            .load_last_summary()
            .unwrap()
            .unwrap()
            .assistant_content,
        "summary two"
    );
    assert_eq!(store.undo_last_turn().unwrap(), (1, None));
    let visible = store.load_visible_turns().unwrap();
    assert_eq!(visible.len(), 2);
    assert_eq!(visible[0].assistant_content, "summary one");
    assert_eq!(visible[1].turn_id, "t3");

    assert_eq!(store.undo_last_turn().unwrap().1.as_deref(), Some("third"));
    assert_eq!(store.undo_last_turn().unwrap(), (1, None));
    let visible = store.load_visible_turns().unwrap();
    assert_eq!(
        visible
            .iter()
            .map(|turn| turn.turn_id.as_str())
            .collect::<Vec<_>>(),
        vec!["t1", "t2"]
    );
}

#[test]
fn tail_retention_compact_folds_only_the_selected_turns() {
    let (_temp, store) = test_store();
    for id in ["t1", "t2", "t3", "t4"] {
        store.start_turn(id, id, 999999).unwrap();
        store.complete_turn(id, "reply", None).unwrap();
    }
    let (_, all_ids) = visible_snapshot(&store);
    store
        .replace_visible_with_summary(
            &["t1".to_string(), "t2".to_string()],
            &all_ids,
            "summary",
            TurnTokens::default(),
            false,
            None,
            None,
        )
        .unwrap();

    let visible = store.load_visible_turns().unwrap();
    let ids: Vec<&str> = visible.iter().map(|t| t.turn_id.as_str()).collect();
    assert_eq!(&ids[..2], &["t3", "t4"]);
    assert_eq!(visible.len(), 3);
    assert!(visible[2].is_summary);
    assert_eq!(
        store
            .load_last_summary()
            .unwrap()
            .unwrap()
            .assistant_content,
        "summary"
    );

    // Undo restores exactly the folded set and deletes the summary.
    assert_eq!(store.undo_last_turn().unwrap(), (1, None));
    let visible = store.load_visible_turns().unwrap();
    assert_eq!(
        visible
            .iter()
            .map(|t| t.turn_id.as_str())
            .collect::<Vec<_>>(),
        vec!["t1", "t2", "t3", "t4"]
    );
}

#[test]
fn second_tail_compact_supersedes_the_previous_summary() {
    let (_temp, store) = test_store();
    for id in ["t1", "t2", "t3"] {
        store.start_turn(id, id, 999999).unwrap();
        store.complete_turn(id, "reply", None).unwrap();
    }
    let (_, all_ids) = visible_snapshot(&store);
    store
        .replace_visible_with_summary(
            &["t1".to_string()],
            &all_ids,
            "summary one",
            TurnTokens::default(),
            false,
            None,
            None,
        )
        .unwrap();
    store.start_turn("t4", "fourth", 999999).unwrap();
    store.complete_turn("t4", "reply", None).unwrap();

    // Second compaction folds t2 (oldest visible non-summary turn); the
    // superseded summary must be hidden together with it even though its
    // seq is higher than the tail turns'.
    let (_, all_ids) = visible_snapshot(&store);
    store
        .replace_visible_with_summary(
            &["t2".to_string()],
            &all_ids,
            "summary two",
            TurnTokens::default(),
            false,
            None,
            None,
        )
        .unwrap();

    let visible = store.load_visible_turns().unwrap();
    let ids: Vec<&str> = visible.iter().map(|t| t.turn_id.as_str()).collect();
    assert_eq!(&ids[..2], &["t3", "t4"]);
    assert_eq!(visible.len(), 3);
    assert_eq!(
        store
            .load_last_summary()
            .unwrap()
            .unwrap()
            .assistant_content,
        "summary two"
    );
    assert_eq!(
        visible.iter().filter(|t| t.is_summary).count(),
        1,
        "the superseded summary must not stay visible"
    );

    // Undo restores t2 and summary one, drops summary two.
    assert_eq!(store.undo_last_turn().unwrap(), (1, None));
    assert_eq!(
        store
            .load_last_summary()
            .unwrap()
            .unwrap()
            .assistant_content,
        "summary one"
    );
    let visible = store.load_visible_turns().unwrap();
    assert!(visible.iter().any(|t| t.turn_id == "t2" && !t.hidden));
}

#[test]
fn empty_summary_leaves_visible_turns_unchanged() {
    let (_temp, store) = test_store();
    store.start_turn("t1", "hello", 999999).unwrap();
    store.complete_turn("t1", "reply", None).unwrap();
    let (fold_ids, turn_ids) = visible_snapshot(&store);

    assert!(store
        .replace_visible_with_summary(
            &fold_ids,
            &turn_ids,
            "  ",
            TurnTokens::default(),
            false,
            None,
            None,
        )
        .is_err());

    let visible = store.load_visible_turns().unwrap();
    assert_eq!(visible.len(), 1);
    assert_eq!(visible[0].turn_id, "t1");
}

#[test]
fn compact_insert_failure_rolls_back_hidden_turns() {
    let (temp, store) = test_store();
    store.start_turn("t1", "hello", 999999).unwrap();
    store.complete_turn("t1", "reply", None).unwrap();
    let (fold_ids, turn_ids) = visible_snapshot(&store);
    let conn = rusqlite::Connection::open(temp.path().join("state/conversation.db")).unwrap();
    conn.execute_batch(
        "CREATE TRIGGER fail_summary_insert
         BEFORE INSERT ON turns WHEN NEW.is_summary = 1
         BEGIN SELECT RAISE(ABORT, 'injected summary failure'); END;",
    )
    .unwrap();

    assert!(store
        .replace_visible_with_summary(
            &fold_ids,
            &turn_ids,
            "summary",
            TurnTokens::default(),
            false,
            None,
            None,
        )
        .is_err());
    let visible = store.load_visible_turns().unwrap();
    assert_eq!(visible.len(), 1);
    assert_eq!(visible[0].turn_id, "t1");
    assert!(!visible[0].hidden);
}

#[test]
fn irreversible_legacy_summary_is_not_deleted_by_undo() {
    let (_temp, store) = test_store();
    store
        .insert_summary_turn("legacy summary", TurnTokens::default(), false)
        .unwrap();

    assert_eq!(store.undo_last_turn().unwrap(), (0, None));
    assert_eq!(
        store
            .load_last_summary()
            .unwrap()
            .unwrap()
            .assistant_content,
        "legacy summary"
    );
}

#[test]
fn irreversible_nested_legacy_summary_is_not_downgraded_by_undo() {
    let (_temp, store) = test_store();
    store
        .insert_summary_turn("legacy summary one", TurnTokens::default(), false)
        .unwrap();
    let first_seq = store.load_visible_turns().unwrap()[0].seq;
    store.hide_turns_before_seq(first_seq).unwrap();
    store
        .insert_summary_turn("legacy summary two", TurnTokens::default(), false)
        .unwrap();

    assert_eq!(store.undo_last_turn().unwrap(), (0, None));
    assert_eq!(
        store
            .load_last_summary()
            .unwrap()
            .unwrap()
            .assistant_content,
        "legacy summary two"
    );
}

#[test]
fn undo_does_not_remove_a_running_turn() {
    let (_temp, store) = test_store();
    store.start_turn("t1", "completed", 999999).unwrap();
    store.complete_turn("t1", "reply", None).unwrap();
    store
        .start_turn("running", "active", std::process::id())
        .unwrap();

    assert_eq!(store.undo_last_turn().unwrap(), (0, None));
    assert_eq!(store.load_visible_turns().unwrap().len(), 2);
}

#[test]
fn compact_rejects_a_changed_snapshot() {
    let (_temp, store) = test_store();
    store.start_turn("t1", "first", 999999).unwrap();
    store.complete_turn("t1", "reply", None).unwrap();
    let (fold_ids, turn_ids) = visible_snapshot(&store);
    store.undo_last_turn().unwrap();

    assert!(store
        .replace_visible_with_summary(
            &fold_ids,
            &turn_ids,
            "stale",
            TurnTokens::default(),
            false,
            None,
            None,
        )
        .is_err());
    assert!(store.load_visible_turns().unwrap().is_empty());
}

#[test]
fn compact_rejects_a_new_turn_after_snapshot() {
    let (_temp, store) = test_store();
    store.start_turn("t1", "first", 999999).unwrap();
    store.complete_turn("t1", "reply", None).unwrap();
    let (fold_ids, turn_ids) = visible_snapshot(&store);
    store.start_turn("t2", "second", 999999).unwrap();
    store.complete_turn("t2", "reply", None).unwrap();

    assert!(store
        .replace_visible_with_summary(
            &fold_ids,
            &turn_ids,
            "stale",
            TurnTokens::default(),
            false,
            None,
            None,
        )
        .is_err());
    assert_eq!(store.load_visible_turns().unwrap().len(), 2);
}

/// 摘要行的 JSON 附件（压后回灌 + 折叠转录路径）存得进、读得回；撤销压缩
/// 连摘要行一起删掉，附件跟着消失。state 层只当它是字符串，不解析。
#[test]
fn compact_stores_and_loads_extras_json() {
    let (_temp, store) = test_store();
    for id in ["t1", "t2"] {
        store.start_turn(id, "hello", 999999).unwrap();
        store.complete_turn(id, "reply", None).unwrap();
    }
    let (fold_ids, turn_ids) = visible_snapshot(&store);
    let extras = r#"{"transcripts":["/state/compact/s/fold-1.md"],"read_hint":true}"#;

    store
        .replace_visible_with_summary(
            &fold_ids,
            &turn_ids,
            "## Task Goal\nsummary",
            TurnTokens::default(),
            false,
            None,
            Some(extras),
        )
        .unwrap();

    let summary = store.load_last_summary().unwrap().unwrap();
    assert_eq!(
        store
            .load_summary_extras_json(&summary.turn_id)
            .unwrap()
            .as_deref(),
        Some(extras),
    );

    store.undo_last_turn().unwrap();
    assert!(store.load_last_summary().unwrap().is_none());
    assert!(store
        .load_summary_extras_json(&summary.turn_id)
        .unwrap()
        .is_none());
}
