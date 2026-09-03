//! dashboard 记忆浏览:分页、过滤、删除;库不存在时按空处理。

use super::shared::*;
use crate::config::AppConfig;
use crate::memory::browse::{BrowseQuery, BrowseTable};
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
