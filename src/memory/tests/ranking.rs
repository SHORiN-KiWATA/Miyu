//! 召回排序：词面分相同的一批事实靠什么分先后。
//!
//! 09-10 真实库取证：一条事实被召回 238 次、长期霸住注入名额（每轮只有两个
//! 事实名额），同主题的其他事实永远排不上去。这里的用例锁住排序里那三项：
//! 可信度、把握、召回疲劳。

use super::shared::{diary_config, test_paths};
use crate::memory::*;

fn store(temp: &tempfile::TempDir) -> MemoryStore {
    let store = MemoryStore::new(&diary_config(2), &test_paths(temp));
    store.init().unwrap();
    store
}

fn insert_fact(
    store: &MemoryStore,
    content: &str,
    updated_at: &str,
    confidence: f64,
    truth_status: &str,
    recall_count: i64,
) -> i64 {
    let conn = store.data_conn().unwrap();
    conn.execute(
        "INSERT INTO facts (
            content, source, status, confidence, strength, recall_count,
            created_at, updated_at, truth_status, importance, visibility,
            owner_principal, owner_display_name, subjects
         ) VALUES (?1, 'test', 'active', ?2, 1.0, ?3, ?4, ?4, ?5, 3, 'privileged', '', '', '[]')",
        rusqlite::params![content, confidence, recall_count, updated_at, truth_status],
    )
    .unwrap();
    conn.last_insert_rowid()
}

/// 词面分相同时按 `updated_at DESC` 出队，所以每条用例都让期望胜出的那条更
/// 老：不修的话它排在后面，用例报红。
fn ranking_ids(store: &MemoryStore, query: &str) -> Vec<i64> {
    let conn = store.data_conn().unwrap();
    store
        .search_facts(&conn, query, 10, false)
        .unwrap()
        .into_iter()
        .map(|hit| hit.id)
        .collect()
}

#[test]
fn an_over_recalled_fact_sinks_below_a_fresh_one() {
    let temp = tempfile::tempdir().unwrap();
    let store = store(&temp);
    let fresh = insert_fact(
        &store,
        "本机用 niri 桌面环境",
        "2026-09-01T00:00:00Z",
        1.0,
        "reported",
        0,
    );
    let hoarder = insert_fact(
        &store,
        "本机用 niri 桌面环境",
        "2026-09-02T00:00:00Z",
        1.0,
        "reported",
        200,
    );
    assert_eq!(ranking_ids(&store, "niri 桌面环境"), vec![fresh, hoarder]);
}

#[test]
fn an_accepted_fact_outranks_an_uncertain_one() {
    let temp = tempfile::tempdir().unwrap();
    let store = store(&temp);
    let accepted = insert_fact(
        &store,
        "本机用 niri 桌面环境",
        "2026-09-01T00:00:00Z",
        1.0,
        "accepted",
        0,
    );
    let uncertain = insert_fact(
        &store,
        "本机用 niri 桌面环境",
        "2026-09-02T00:00:00Z",
        1.0,
        "uncertain",
        0,
    );
    assert_eq!(
        ranking_ids(&store, "niri 桌面环境"),
        vec![accepted, uncertain]
    );
}

#[test]
fn a_confident_fact_outranks_a_shaky_one() {
    let temp = tempfile::tempdir().unwrap();
    let store = store(&temp);
    let confident = insert_fact(
        &store,
        "本机用 niri 桌面环境",
        "2026-09-01T00:00:00Z",
        0.95,
        "reported",
        0,
    );
    let shaky = insert_fact(
        &store,
        "本机用 niri 桌面环境",
        "2026-09-02T00:00:00Z",
        0.2,
        "reported",
        0,
    );
    assert_eq!(ranking_ids(&store, "niri 桌面环境"), vec![confident, shaky]);
}

/// 疲劳度是对数饱和的：召回几次不该把一条好事实压下去。
#[test]
fn a_few_recalls_do_not_sink_a_fact() {
    let temp = tempfile::tempdir().unwrap();
    let store = store(&temp);
    let settled = insert_fact(
        &store,
        "本机用 niri 桌面环境",
        "2026-09-01T00:00:00Z",
        1.0,
        "reported",
        3,
    );
    let untested = insert_fact(
        &store,
        "本机用 niri 桌面环境",
        "2026-09-02T00:00:00Z",
        0.6,
        "reported",
        0,
    );
    // 3 次召回的疲劳惩罚不到 1.4 分，压不过 0.4 的把握差（1.6 分）。
    assert_eq!(
        ranking_ids(&store, "niri 桌面环境"),
        vec![settled, untested]
    );
}
