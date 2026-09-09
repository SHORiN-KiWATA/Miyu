//! 会话级重置。
//!
//! 关键的不变量有两条：一是别的会话的记忆一条都不能少，二是没有会话标记的
//! 老行必须留着——那批是改动之前存下的，谁都说不清它们属于哪次对话，只能等
//! `reset_all`。

use super::shared::*;
use crate::config::AppConfig;
use crate::memory::*;

fn session_store(
    config: &AppConfig,
    paths: &crate::paths::MiyuPaths,
    session: &str,
) -> MemoryStore {
    MemoryStore::new(config, paths).with_session_id(session)
}

fn record_session_turn(store: &MemoryStore, session: &str, user: &str, assistant: &str) {
    let (database_id, generation) = store.identity().unwrap();
    assert!(store
        .process_after_turn(
            user,
            assistant,
            &MemoryOrigin::local(session),
            &database_id,
            generation,
        )
        .unwrap());
}

fn session_counts(store: &MemoryStore, session: &str) -> (i64, i64, i64, i64) {
    let data = store.data_conn().unwrap();
    let state = store.state_conn().unwrap();
    let count = |conn: &rusqlite::Connection, table: &str| {
        conn.query_row(
            &format!("SELECT COUNT(*) FROM {table} WHERE origin_session_id = ?1"),
            [session],
            |row| row.get::<_, i64>(0),
        )
        .unwrap()
    };
    (
        count(&data, "facts"),
        count(&data, "episodes"),
        count(&data, "pending_events"),
        count(&state, "evicted_turns"),
    )
}

fn evicted_row(session: &str) -> EvictedTurn {
    EvictedTurn {
        source_id: format!("{session}:user"),
        timestamp: "2026-09-09T10:00:00+00:00".to_string(),
        role: "user".to_string(),
        content: format!("{session} 说过的旧上下文"),
        ..EvictedTurn::default()
    }
}

#[test]
fn reset_session_removes_only_the_rows_that_session_produced() {
    let temp = tempfile::tempdir().unwrap();
    let config = AppConfig::default();
    let paths = test_paths(&temp);
    let store_a = session_store(&config, &paths, "session-a");
    let store_b = session_store(&config, &paths, "session-b");
    store_a.init().unwrap();

    for (store, session) in [(&store_a, "session-a"), (&store_b, "session-b")] {
        store
            .remember_fact(&format!("{session} 的事实"), "test")
            .unwrap();
        store.remember_pending_event("你好", "在呢").unwrap();
        store
            .remember_evicted_turns(&[evicted_row(session)])
            .unwrap();
        record_session_turn(store, session, &format!("{session} 的问题"), "回答");
    }
    assert_eq!(session_counts(&store_a, "session-a"), (1, 1, 1, 1));
    assert_eq!(session_counts(&store_b, "session-b"), (1, 1, 1, 1));

    let summary = store_a.reset_session("session-a").unwrap();
    assert_eq!(
        summary,
        MemoryResetSummary {
            facts: 1,
            episodes: 1,
            pending_events: 1,
            evicted_turns: 1,
        }
    );
    assert_eq!(session_counts(&store_a, "session-a"), (0, 0, 0, 0));
    assert_eq!(
        session_counts(&store_b, "session-b"),
        (1, 1, 1, 1),
        "另一个会话的记忆被误伤了"
    );
}

/// 生成号必须跟着抬，否则飞在半路的整理批次会把刚清掉的日记再写回来
/// （`reset_all` 当年就是为这个才自增的）。
#[test]
fn reset_session_bumps_the_generation() {
    let temp = tempfile::tempdir().unwrap();
    let paths = test_paths(&temp);
    let store = session_store(&AppConfig::default(), &paths, "session-a");
    store.init().unwrap();
    store.remember_fact("生成号测试", "test").unwrap();
    let (_, before) = store.identity().unwrap();

    store.reset_session("session-a").unwrap();

    let (_, after) = store.identity().unwrap();
    assert_eq!(after, before + 1);
}

/// 空 id 会匹配上所有历史遗留行——那等于把 `reset_all` 伪装成会话级重置。
#[test]
fn reset_session_refuses_an_empty_session_id() {
    let temp = tempfile::tempdir().unwrap();
    let config = AppConfig::default();
    let paths = test_paths(&temp);
    let store = MemoryStore::new(&config, &paths);
    store.init().unwrap();
    store.remember_fact("没有会话标记的事实", "test").unwrap();
    store.remember_pending_event("你好", "在呢").unwrap();

    assert!(store.reset_session("").is_err());
    assert!(store.reset_session("   ").is_err());

    let conn = store.data_conn().unwrap();
    assert_eq!(count_rows(&conn, "facts").unwrap(), 1);
    assert_eq!(count_rows(&conn, "pending_events").unwrap(), 1);
}

/// 加列之前存下的行只有默认值（空串）。它们必须扛过会话级重置，并且仍然
/// 能被 `reset_all` 清掉。
#[test]
fn rows_written_before_the_column_existed_survive_a_session_reset() {
    let temp = tempfile::tempdir().unwrap();
    let config = AppConfig::default();
    let paths = test_paths(&temp);
    let store = session_store(&config, &paths, "session-a");
    store.init().unwrap();
    // 老行：不带 origin_session_id，走的就是列默认值。
    let conn = store.data_conn().unwrap();
    conn.execute(
        "INSERT INTO facts (content, source, status, created_at, updated_at)
         VALUES ('迁移之前记下的事实', 'test', 'active', ?1, ?1)",
        ["2026-01-01T00:00:00Z"],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO episodes (content, source, status, created_at, updated_at, retention)
         VALUES ('迁移之前记下的经历', 'episode', 'active', ?1, ?1, 'long_term')",
        ["2026-01-01T00:00:00Z"],
    )
    .unwrap();
    drop(conn);
    store.remember_fact("本会话的事实", "test").unwrap();

    let summary = store.reset_session("session-a").unwrap();
    assert_eq!(summary.facts, 1);
    let conn = store.data_conn().unwrap();
    assert_eq!(
        count_rows(&conn, "facts").unwrap(),
        1,
        "空标记的老行被会话级重置误删了"
    );
    assert_eq!(count_rows(&conn, "episodes").unwrap(), 1);
    drop(conn);

    store.reset_all(false).unwrap();
    let conn = store.data_conn().unwrap();
    assert_eq!(count_rows(&conn, "facts").unwrap(), 0);
    assert_eq!(count_rows(&conn, "episodes").unwrap(), 0);
}

/// 待处理事件消化成日记时会话标记要跟着走，否则那批日记永远无主，会话级
/// 重置一条也清不掉。
#[test]
fn flushed_pending_events_keep_their_session() {
    let temp = tempfile::tempdir().unwrap();
    let config = AppConfig::default();
    let paths = test_paths(&temp);
    let store = session_store(&config, &paths, "session-a");
    store
        .remember_pending_event("待整理的问题", "待整理的回答")
        .unwrap();
    store.flush_pending_events().unwrap();

    assert_eq!(session_counts(&store, "session-a").1, 1);
    let summary = store.reset_session("session-a").unwrap();
    assert_eq!(summary.episodes, 1);
}

/// 向量表没有触发器跟着删，行走了它还在，而 id 是自增的——迟早被新行撞上
/// 并读回一段别人的语义。
#[test]
fn reset_session_drops_the_vectors_of_the_rows_it_deletes() {
    let temp = tempfile::tempdir().unwrap();
    let config = AppConfig::default();
    let paths = test_paths(&temp);
    let store = session_store(&config, &paths, "session-a");
    let mine = store.remember_fact("本会话的事实", "test").unwrap();
    let theirs = session_store(&config, &paths, "session-b")
        .remember_fact("别人会话的事实", "test")
        .unwrap();
    let conn = store.data_conn().unwrap();
    for id in [mine, theirs] {
        conn.execute(
            "INSERT INTO memory_embeddings (kind, id, model, content_sha256, embedding, created_at)
             VALUES ('fact', ?1, 'test-model', 'sha', x'00', ?2)",
            rusqlite::params![id, "2026-09-09T10:00:00Z"],
        )
        .unwrap();
    }
    drop(conn);

    store.reset_session("session-a").unwrap();

    let conn = store.data_conn().unwrap();
    let remaining = conn
        .query_row("SELECT id FROM memory_embeddings", [], |row| {
            row.get::<_, i64>(0)
        })
        .unwrap();
    assert_eq!(remaining, theirs, "删的向量和删的行没有对上");
}
