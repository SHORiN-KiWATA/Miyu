//! dashboard 记忆浏览:分页、过滤、删除;库不存在时按空处理。

use super::shared::*;
use crate::config::AppConfig;
use crate::memory::browse::{BrowsePatch, BrowseQuery, BrowseTable, EvictedQuery};
use crate::memory::*;

#[test]
fn browse_pages_filters_and_deletes_without_creating_databases() {
    let temp = tempfile::tempdir().unwrap();
    let config = AppConfig::default();
    let paths = test_paths(&temp);
    let store = MemoryStore::new(&config, &paths);

    // 还没有库:空页,且不能因为浏览就建库。
    let empty = store
        .browse(BrowseTable::Facts, &BrowseQuery::default())
        .unwrap();
    assert_eq!(empty.total, 0);
    assert!(!config
        .active_persona_memory_data_dir(&paths)
        .join("memory.db")
        .exists());

    let a = store
        .remember_fact("用户喜欢 100% 纯黑咖啡", "test")
        .unwrap();
    let _b = store.remember_fact("用户住在东京", "test").unwrap();
    let _c = store.remember_fact("用户养了一只猫", "test").unwrap();

    let all = store
        .browse(
            BrowseTable::Facts,
            &BrowseQuery {
                limit: 2,
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(all.total, 3);
    assert_eq!(all.items.len(), 2);
    let page2 = store
        .browse(
            BrowseTable::Facts,
            &BrowseQuery {
                limit: 2,
                offset: 2,
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(page2.items.len(), 1);

    // LIKE 通配符要转义:搜 "100%" 只能命中那一条。
    let hit = store
        .browse(
            BrowseTable::Facts,
            &BrowseQuery {
                text: "100%".into(),
                limit: 50,
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(hit.total, 1);
    assert_eq!(hit.items[0]["id"], a);

    assert!(store.delete_item(BrowseTable::Facts, a).unwrap());
    assert!(!store.delete_item(BrowseTable::Facts, a).unwrap());
    let rest = store
        .browse(
            BrowseTable::Facts,
            &BrowseQuery {
                limit: 50,
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(rest.total, 2);
    assert!(store
        .browse(
            BrowseTable::Episodes,
            &BrowseQuery {
                limit: 50,
                ..Default::default()
            }
        )
        .unwrap()
        .items
        .is_empty());
    assert!(BrowseTable::parse("turns").is_none());
}

#[test]
fn browse_detail_patch_revisions_and_readonly_stats() {
    let temp = tempfile::tempdir().unwrap();
    let config = AppConfig::default();
    let paths = test_paths(&temp);
    let store = MemoryStore::new(&config, &paths);

    // 库不存在:统计全零、详情为空、编辑返回 false,且都不建库。
    let empty = store.stats_readonly().unwrap();
    assert_eq!(empty["exists"], false);
    assert_eq!(empty["facts"], 0);
    assert!(store.browse_item(BrowseTable::Facts, 1).unwrap().is_none());
    assert!(!store
        .update_item(BrowseTable::Facts, 1, &BrowsePatch::default())
        .unwrap());
    assert!(!config
        .active_persona_memory_data_dir(&paths)
        .join("memory.db")
        .exists());

    let id = store.remember_fact("用户在东京工作", "test").unwrap();
    let detail = store.browse_item(BrowseTable::Facts, id).unwrap().unwrap();
    assert_eq!(detail["memory_type"], "fact");
    assert_eq!(detail["truth_status"], "reported");
    assert_eq!(detail["importance"], 3);
    assert_eq!(detail["tags"], serde_json::json!([]));
    assert!(store.browse_revisions(id).unwrap().is_empty());

    // 改内容:写一条修订;改枚举字段与标签;非法值拒绝。
    let patch = BrowsePatch {
        content: Some("用户在大阪工作".into()),
        importance: Some(5),
        memory_type: Some("preference".into()),
        truth_status: Some("accepted".into()),
        tags: Some(vec!["工作".into(), " 城市 ".into(), "".into()]),
        ..Default::default()
    };
    assert!(store.update_item(BrowseTable::Facts, id, &patch).unwrap());
    let after = store.browse_item(BrowseTable::Facts, id).unwrap().unwrap();
    assert_eq!(after["content"], "用户在大阪工作");
    assert_eq!(after["importance"], 5);
    assert_eq!(after["memory_type"], "preference");
    assert_eq!(after["truth_status"], "accepted");
    assert_eq!(after["tags"], serde_json::json!(["工作", "城市"]));
    let revisions = store.browse_revisions(id).unwrap();
    assert_eq!(revisions.len(), 1);
    assert_eq!(revisions[0]["old_content"], "用户在东京工作");
    assert_eq!(revisions[0]["new_content"], "用户在大阪工作");
    // 内容没变就不再写修订。
    assert!(store
        .update_item(
            BrowseTable::Facts,
            id,
            &BrowsePatch {
                content: Some("用户在大阪工作".into()),
                ..Default::default()
            }
        )
        .unwrap());
    assert_eq!(store.browse_revisions(id).unwrap().len(), 1);
    assert!(store
        .update_item(
            BrowseTable::Facts,
            id,
            &BrowsePatch {
                truth_status: Some("maybe".into()),
                ..Default::default()
            }
        )
        .is_err());
    assert!(store
        .update_item(
            BrowseTable::Facts,
            id,
            &BrowsePatch {
                content: Some("   ".into()),
                ..Default::default()
            }
        )
        .is_err());

    // 遗忘 → 救回:状态回 active 且强度回满。
    assert!(store
        .update_item(
            BrowseTable::Facts,
            id,
            &BrowsePatch {
                status: Some("forgotten".into()),
                ..Default::default()
            }
        )
        .unwrap());
    let stats = store.stats_readonly().unwrap();
    assert_eq!(stats["facts"], 0);
    assert_eq!(stats["facts_forgotten"], 1);
    assert_eq!(stats["revisions"], 1);
    let by_status = store
        .browse(
            BrowseTable::Facts,
            &BrowseQuery {
                status: "forgotten".into(),
                limit: 10,
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(by_status.total, 1);
    let by_tag = store
        .browse(
            BrowseTable::Facts,
            &BrowseQuery {
                tag: "工作".into(),
                limit: 10,
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(by_tag.total, 1);
    let by_type = store
        .browse(
            BrowseTable::Facts,
            &BrowseQuery {
                memory_type: "fact".into(),
                limit: 10,
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(by_type.total, 0);
    assert!(store
        .update_item(
            BrowseTable::Facts,
            id,
            &BrowsePatch {
                status: Some("active".into()),
                ..Default::default()
            }
        )
        .unwrap());
    let revived = store.browse_item(BrowseTable::Facts, id).unwrap().unwrap();
    assert_eq!(revived["status"], "active");
    assert_eq!(revived["strength"], 1.0);

    // 逐出归档:不存在按空;写入后可分页、按角色过滤、按 id 取全文、删除。
    let none = store.browse_evicted(&EvictedQuery::default()).unwrap();
    assert_eq!(none.total, 0);
    store
        .remember_evicted_turns(&[
            EvictedTurn {
                source_id: "t1".into(),
                timestamp: "2026-09-01T10:00:00+00:00".into(),
                role: "user".into(),
                content: "昨天我们聊了 rust 的所有权".into(),
                visibility: "privileged".into(),
                owner_principal: String::new(),
                owner_display_name: String::new(),
            },
            EvictedTurn {
                source_id: "t2".into(),
                timestamp: "2026-09-01T10:01:00+00:00".into(),
                role: "assistant".into(),
                content: "对,借用检查器那段".into(),
                visibility: "privileged".into(),
                owner_principal: String::new(),
                owner_display_name: String::new(),
            },
        ])
        .unwrap();
    let all = store
        .browse_evicted(&EvictedQuery {
            limit: 10,
            ..Default::default()
        })
        .unwrap();
    assert_eq!(all.total, 2);
    assert_eq!(all.items[0]["role"], "assistant");
    let users = store
        .browse_evicted(&EvictedQuery {
            role: "user".into(),
            limit: 10,
            ..Default::default()
        })
        .unwrap();
    assert_eq!(users.total, 1);
    let searched = store
        .browse_evicted(&EvictedQuery {
            text: "所有权".into(),
            limit: 10,
            ..Default::default()
        })
        .unwrap();
    assert_eq!(searched.total, 1);
    let first_id = users.items[0]["id"].as_i64().unwrap();
    let full = store.browse_evicted_item(first_id).unwrap().unwrap();
    assert_eq!(full["content"], "昨天我们聊了 rust 的所有权");
    let stats = store.stats_readonly().unwrap();
    assert_eq!(stats["evicted_turns"], 2);
    assert!(store.delete_evicted_item(first_id).unwrap());
    assert!(!store.delete_evicted_item(first_id).unwrap());
    assert_eq!(store.stats_readonly().unwrap()["evicted_turns"], 1);
}
