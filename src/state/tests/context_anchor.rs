//! 上下文锚点：最新一条已完成普通回合的供应商实测占用。

use super::shared::*;
use crate::state::*;

fn complete(store: &StateStore, turn_id: &str, estimated: bool) {
    store.start_turn(turn_id, "hello", 999999).unwrap();
    store
        .complete_turn_with_usage_and_model(
            turn_id,
            "reply",
            None,
            Some("deepseek"),
            Some("deepseek-v4-flash"),
            TurnTokens {
                total: 100,
                ..Default::default()
            },
            estimated,
        )
        .unwrap();
}

#[test]
fn context_anchor_reads_the_newest_completed_turn() {
    let (_temp, store) = test_store();
    complete(&store, "t1", false);
    complete(&store, "t2", false);
    store.set_turn_context_end("t1", Some(500)).unwrap();
    store.set_turn_context_end("t2", Some(1234)).unwrap();

    let anchor = store.load_context_anchor().unwrap().unwrap();
    assert_eq!(anchor.turn_id, "t2");
    assert_eq!(anchor.tokens, 1234);
    assert_eq!(anchor.provider_id.as_deref(), Some("deepseek"));
    assert_eq!(anchor.model.as_deref(), Some("deepseek-v4-flash"));
}

#[test]
fn context_anchor_is_none_without_a_recorded_value() {
    let (_temp, store) = test_store();
    complete(&store, "t1", false);
    assert!(store.load_context_anchor().unwrap().is_none());

    store.set_turn_context_end("t1", Some(0)).unwrap();
    assert!(store.load_context_anchor().unwrap().is_none());
}

#[test]
fn context_anchor_ignores_estimated_usage() {
    let (_temp, store) = test_store();
    complete(&store, "t1", true);
    store.set_turn_context_end("t1", Some(900)).unwrap();
    assert!(store.load_context_anchor().unwrap().is_none());
}

#[test]
fn context_anchor_is_none_after_compaction() {
    let (_temp, store) = test_store();
    complete(&store, "t1", false);
    complete(&store, "t2", false);
    store.set_turn_context_end("t2", Some(1234)).unwrap();
    assert!(store.load_context_anchor().unwrap().is_some());

    store
        .replace_visible_with_summary(
            &["t1".to_string()],
            &["t1".to_string(), "t2".to_string()],
            "## Task Goal\nsummary",
            TurnTokens::default(),
            false,
            None,
            None,
        )
        .unwrap();

    // 摘要行落在最后：它的 token_context_end 从来没写过,而且它也不代表
    // 下一次请求的前缀大小 —— 必须退回估算。
    assert!(store.load_context_anchor().unwrap().is_none());
}

#[test]
fn context_anchor_ignores_an_interrupted_tail_turn() {
    let (_temp, store) = test_store();
    complete(&store, "t1", false);
    store.set_turn_context_end("t1", Some(700)).unwrap();
    store.start_turn("t2", "second", 999999).unwrap();
    store.interrupt_turn("t2").unwrap();

    assert!(store.load_context_anchor().unwrap().is_none());
}
