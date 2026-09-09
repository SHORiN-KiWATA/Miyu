//! 可见性与归属：谁能看见谁的记忆。

use super::shared::*;
use crate::config::AppConfig;
use crate::memory::*;

#[test]
fn ordinary_principals_recall_only_public_and_owned_memories() {
    let temp = tempfile::tempdir().unwrap();
    let config = AppConfig::default();
    let paths = test_paths(&temp);
    let admin = MemoryStore::new(&config, &paths);
    admin.init().unwrap();
    let timestamp = now();
    admin
        .data_conn()
        .unwrap()
        .execute(
            "INSERT INTO facts (
                content, source, status, confidence, recall_count, created_at, updated_at,
                visibility, owner_principal, owner_display_name
             ) VALUES (?1, 'test', 'active', 1.0, 0, ?2, ?2, 'public', '', '')",
            params!["隔离测试 公共知识", timestamp],
        )
        .unwrap();

    let origin_a = platform_origin("7", "Alice");
    let origin_b = platform_origin("8", "Bob");
    let user_a = scoped_store(&config, &paths, &origin_a, false);
    let user_b = scoped_store(&config, &paths, &origin_b, false);
    user_a
        .remember_fact("隔离测试 Alice 私密事实", "test")
        .unwrap();
    user_b
        .remember_fact("隔离测试 Bob 私密事实", "test")
        .unwrap();
    let (database_id, generation) = user_a.identity().unwrap();
    user_a
        .process_after_turn(
            "隔离测试 Alice 的旧事件",
            "只属于 Alice",
            &origin_a,
            &database_id,
            generation,
        )
        .unwrap();

    let a = user_a
        .recall_memories("隔离测试", 20, false)
        .unwrap()
        .to_string();
    assert!(a.contains("公共知识"));
    assert!(a.contains("Alice 私密事实"));
    assert!(a.contains("Alice 的旧事件"));
    assert!(!a.contains("Bob 私密事实"));

    let b = user_b
        .recall_memories("隔离测试", 20, false)
        .unwrap()
        .to_string();
    assert!(b.contains("公共知识"));
    assert!(b.contains("Bob 私密事实"));
    assert!(!b.contains("Alice 私密事实"));
    assert!(!b.contains("Alice 的旧事件"));
    let b_events = user_b
        .recall_past_events("隔离测试", 20)
        .unwrap()
        .to_string();
    assert!(!b_events.contains("Alice 的旧事件"));
    let a_events = user_a
        .recall_past_events("隔离测试", 20)
        .unwrap()
        .to_string();
    assert!(a_events.contains("Alice 的旧事件"));

    let privileged = admin
        .recall_memories("隔离测试", 20, false)
        .unwrap()
        .to_string();
    assert!(privileged.contains("公共知识"));
    assert!(privileged.contains("Alice 私密事实"));
    assert!(privileged.contains("Bob 私密事实"));
    assert!(privileged.contains("Alice 的旧事件"));
}

#[test]
fn access_migration_backfills_platform_principals_conservatively() {
    let temp = tempfile::tempdir().unwrap();
    let config = AppConfig::default();
    let paths = test_paths(&temp);
    let store = MemoryStore::new(&config, &paths);
    let origin = platform_origin("7", "Alice");
    let (database_id, generation) = store.identity().unwrap();
    store
        .process_after_turn(
            "迁移归属测试",
            "迁移回答",
            &origin,
            &database_id,
            generation,
        )
        .unwrap();
    let conn = store.data_conn().unwrap();
    let episode_id = conn
        .query_row("SELECT id FROM episodes LIMIT 1", [], |row| {
            row.get::<_, i64>(0)
        })
        .unwrap();
    conn.execute(
        "INSERT INTO facts (
            content, source, status, confidence, recall_count, created_at, updated_at,
            source_episode_ids, visibility, owner_principal, owner_display_name
         ) VALUES ('迁移事实', 'test', 'active', 1.0, 0, ?1, ?1, ?2,
                   'privileged', '', '')",
        params![now(), serde_json::to_string(&vec![episode_id]).unwrap()],
    )
    .unwrap();
    conn.execute(
        "UPDATE episodes SET visibility='privileged', owner_principal='', owner_display_name=''",
        [],
    )
    .unwrap();
    conn.execute(
        "UPDATE memory_meta SET access_schema_version=0 WHERE id=1",
        [],
    )
    .unwrap();
    drop(conn);

    store.init().unwrap();
    let expected = origin.principal_ownership().unwrap().owner_principal;
    let conn = store.data_conn().unwrap();
    let episode_owner = conn
        .query_row(
            "SELECT visibility, owner_principal FROM episodes WHERE id=?1",
            [episode_id],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        )
        .unwrap();
    let fact_owner = conn
        .query_row(
            "SELECT visibility, owner_principal FROM facts WHERE content='迁移事实'",
            [],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        )
        .unwrap();
    assert_eq!(
        episode_owner,
        (VISIBILITY_PRINCIPAL.to_string(), expected.clone())
    );
    assert_eq!(fact_owner, (VISIBILITY_PRINCIPAL.to_string(), expected));
}

#[test]
fn organizer_can_publish_general_facts_but_cannot_update_another_principal() {
    let temp = tempfile::tempdir().unwrap();
    let config = diary_config(2);
    let paths = test_paths(&temp);
    let origin_a = platform_origin("7", "Alice");
    let origin_b = platform_origin("8", "Bob");
    let user_a = scoped_store(&config, &paths, &origin_a, false);
    let user_b = scoped_store(&config, &paths, &origin_b, false);
    let bob_fact = user_b
        .remember_fact("Linux 隔离主题是 Bob 的私人偏好", "test")
        .unwrap();
    let (database_id, generation) = user_b.identity().unwrap();
    user_b
        .process_after_turn(
            "Linux 隔离主题 Bob 的设置",
            "Bob 使用另一种方式",
            &origin_b,
            &database_id,
            generation,
        )
        .unwrap();
    let (database_id, generation) = user_a.identity().unwrap();
    user_a
        .process_after_turn(
            "Linux 隔离主题与通用命令",
            "使用 systemctl --user",
            &origin_a,
            &database_id,
            generation,
        )
        .unwrap();
    let batch = MemoryStore::new(&config, &paths)
        .next_organization_batch()
        .unwrap()
        .unwrap();
    assert!(batch.existing.iter().any(|memory| memory.id == bob_fact));
    let alice_principal = origin_a.principal_ownership().unwrap().owner_principal;
    let source_id = batch
        .diaries
        .iter()
        .find(|diary| diary.owner_principal.as_deref() == Some(alice_principal.as_str()))
        .unwrap()
        .id;
    let cross_user_update = OrganizedOutput {
        knowledge: vec![KnowledgeAction {
            operation: "update".to_string(),
            target_id: Some(bob_fact),
            memory_type: "preference".to_string(),
            content: "Linux 隔离主题被 Alice 覆盖".to_string(),
            truth_status: "reported".to_string(),
            importance: 3,
            confidence: 0.8,
            visibility: VISIBILITY_PRINCIPAL.to_string(),
            subjects: Vec::new(),
            tags: Vec::new(),
            diary_ids: vec![source_id],
        }],
        long_diaries: Vec::new(),
    };
    // 不合规的项目单条丢弃、整批照常落地(以前整批 bail 会让批次无限重试)。
    MemoryStore::new(&config, &paths)
        .apply_organized_batch(&batch, cross_user_update)
        .unwrap();
    let store = MemoryStore::new(&config, &paths);
    let conn = store.data_conn().unwrap();
    let bob_content: String = conn
        .query_row("SELECT content FROM facts WHERE id=?1", [bob_fact], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(
        bob_content, "Linux 隔离主题是 Bob 的私人偏好",
        "跨主体的 update 必须被丢掉"
    );
    drop(conn);

    let leaky_public_fact = OrganizedOutput {
        knowledge: vec![KnowledgeAction {
            operation: "create".to_string(),
            target_id: None,
            memory_type: "fact".to_string(),
            content: "Alice 使用 Linux 的私人经历".to_string(),
            truth_status: "reported".to_string(),
            importance: 3,
            confidence: 0.8,
            visibility: VISIBILITY_PUBLIC.to_string(),
            subjects: Vec::new(),
            tags: Vec::new(),
            diary_ids: vec![source_id],
        }],
        long_diaries: Vec::new(),
    };
    MemoryStore::new(&config, &paths)
        .apply_organized_batch(&batch, leaky_public_fact)
        .unwrap();
    let conn = store.data_conn().unwrap();
    let leaked: i64 = conn
        .query_row(
            "SELECT count(*) FROM facts WHERE content='Alice 使用 Linux 的私人经历'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(leaked, 0, "带私人身份标记的 public 事实必须被丢掉");
    drop(conn);

    MemoryStore::new(&config, &paths)
        .apply_organized_batch(
            &batch,
            OrganizedOutput {
                knowledge: vec![KnowledgeAction {
                    operation: "create".to_string(),
                    target_id: None,
                    memory_type: "fact".to_string(),
                    content: "Linux 通用知识使用 systemctl --user".to_string(),
                    truth_status: "accepted".to_string(),
                    importance: 3,
                    confidence: 0.9,
                    visibility: VISIBILITY_PUBLIC.to_string(),
                    subjects: Vec::new(),
                    tags: vec!["Linux".to_string()],
                    diary_ids: vec![source_id],
                }],
                long_diaries: Vec::new(),
            },
        )
        .unwrap();
    let bob_recall = user_b
        .recall_memories("systemctl user", 10, false)
        .unwrap()
        .to_string();
    assert!(bob_recall.contains("Linux 通用知识"));
}

#[test]
fn association_excludes_own_sessions_visible_echo() {
    let temp = tempfile::tempdir().unwrap();
    let config = AppConfig::default();
    let paths = test_paths(&temp);
    let store = MemoryStore::new(&config, &paths);
    store.init().unwrap();
    store
        .data_conn()
        .unwrap()
        .execute(
            "INSERT INTO episodes (content, source, status, recall_count, created_at, updated_at, retention, origin_session_id)
             VALUES ('对方提到自回声话题', 'auto_diary', 'active', 0, ?1, ?1, 'short_term', 's1')",
            [chrono::Utc::now().to_rfc3339()],
        )
        .unwrap();
    // 无排除:能召回。
    assert!(store.association("自回声", None).unwrap().is_some());
    // 同会话且晚于最老可见轮 → 自回声被滤(原对话就在眼前)。
    let exclusion = AssociationExclusion {
        session_id: "s1".to_string(),
        since: "2000-01-01T00:00:00Z".to_string(),
    };
    assert!(store
        .association("自回声", Some(&exclusion))
        .unwrap()
        .is_none());
    // 别的会话不受排除影响。
    let other = AssociationExclusion {
        session_id: "s2".to_string(),
        since: "2000-01-01T00:00:00Z".to_string(),
    };
    assert!(store.association("自回声", Some(&other)).unwrap().is_some());
}

#[test]
fn association_dedup_filters_visible_lines_and_keeps_changed_ones() {
    let temp = tempfile::tempdir().unwrap();
    let config = AppConfig::default();
    let paths = test_paths(&temp);
    let store = MemoryStore::new(&config, &paths);
    assert!(store.association_dedup_enabled());
    let stamp = now();
    let fact = MemoryHit {
        id: 1,
        kind: MemoryKind::Fact,
        content: "AUR 的 GitHub 镜像只读".to_string(),
        score: 1.0,
        timestamp: stamp.clone(),
        source: "test".to_string(),
        retention: None,
        visibility: VISIBILITY_PUBLIC.to_string(),
        owner_principal: String::new(),
        owner_display_name: String::new(),
        subjects: "[]".to_string(),
        source_episode_ids: Vec::new(),
        origin_session_id: String::new(),
    };
    let diary = MemoryHit {
        id: 2,
        kind: MemoryKind::Diary,
        content: "对方说：测试；我回：通过".to_string(),
        retention: Some(SHORT_TERM.to_string()),
        ..fact.clone()
    };
    let updated_fact = MemoryHit {
        id: 1,
        content: "AUR 的 GitHub 镜像只读，推送需走官方地址".to_string(),
        ..fact.clone()
    };
    // 第一回合的注入块回放时携带的行
    let first = store.format_association(&AssociationContext {
        facts: vec![fact.clone()],
        episodes: vec![diary.clone()],
        organization_due: false,
    });
    let seen = first
        .lines()
        .filter(|line| line.starts_with("- ["))
        .collect::<HashSet<_>>();
    assert_eq!(seen.len(), 2);
    // 未变化的 fact 与 diary 被过滤；内容更新过的 fact 保留
    let mut association = AssociationContext {
        facts: vec![fact.clone(), updated_fact],
        episodes: vec![diary],
        organization_due: false,
    };
    store.retain_unseen_association(&mut association, &seen);
    assert_eq!(association.facts.len(), 1);
    assert!(association.facts[0].content.contains("官方地址"));
    assert!(association.episodes.is_empty());
    // 空 seen 集不过滤
    let mut untouched = AssociationContext {
        facts: vec![fact],
        episodes: Vec::new(),
        organization_due: false,
    };
    store.retain_unseen_association(&mut untouched, &HashSet::new());
    assert_eq!(untouched.facts.len(), 1);
}

#[test]
fn disabled_writes_block_content_but_allow_recall_reinforcement() {
    let temp = tempfile::tempdir().unwrap();
    let config = AppConfig::default();
    let paths = test_paths(&temp);
    let mut store = MemoryStore::new(&config, &paths);
    let fact_id = store
        .remember_fact("Niri 输入法需要 XMODIFIERS", "test")
        .unwrap();

    store.set_writes_enabled(false);
    assert_eq!(store.remember_fact("不应保存", "test").unwrap(), 0);
    assert!(!record_turn(&store, "不应写入日记", "不会写入"));
    assert!(store.prepare_evicted_context_db().unwrap().is_none());

    let association = store.association("Niri XMODIFIERS", None).unwrap();
    assert!(association.is_some());
    let conn = store.data_conn().unwrap();
    let recall_count = conn
        .query_row(
            "SELECT recall_count FROM facts WHERE id=?1",
            [fact_id],
            |row| row.get::<_, i64>(0),
        )
        .unwrap();
    assert_eq!(recall_count, 1);
    assert_eq!(count_rows(&conn, "facts").unwrap(), 1);
    assert_eq!(count_rows(&conn, "episodes").unwrap(), 0);
    assert_eq!(count_rows(&conn, "pending_events").unwrap(), 0);
}
