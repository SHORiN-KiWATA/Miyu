//! 整理器的语义去重：模型说「新建」，库里已经有换个说法的同一条时，改写成
//! 对已有事实的 update。
//!
//! 这里只跑不需要嵌入器的路径（归一化文本），向量那条路要真实模型，属于量尺
//! 范畴（见 testkit/memory-quality）。用例照样守得住关键行为：近义改写、
//! 无关内容不动、没有嵌入器时扩展候选是空操作。

use super::shared::{diary_config, record_turn, test_paths};
use crate::config::AppConfig;
use crate::memory::*;

fn config_without_embeddings(batch_size: usize) -> AppConfig {
    let mut config = diary_config(batch_size);
    config.embedding.enabled = false;
    config
}

fn insert_fact(store: &MemoryStore, content: &str, visibility: &str) -> i64 {
    let conn = store.data_conn().unwrap();
    conn.execute(
        "INSERT INTO facts (
            content, source, status, confidence, strength, recall_count,
            created_at, updated_at, truth_status, importance, visibility,
            owner_principal, owner_display_name, subjects
         ) VALUES (?1, 'test', 'active', 0.9, 1.0, 0, ?2, ?2, 'reported', 3, ?3, '', '', '[]')",
        rusqlite::params![content, "2026-09-01T00:00:00Z", visibility],
    )
    .unwrap();
    conn.last_insert_rowid()
}

fn candidate(id: i64, content: &str, visibility: &str) -> ExistingMemoryRecord {
    ExistingMemoryRecord {
        id,
        kind: "knowledge".to_string(),
        content: content.to_string(),
        truth_status: "reported".to_string(),
        visibility: visibility.to_string(),
        owner_principal: String::new(),
        owner_display_name: String::new(),
    }
}

fn create_action(content: &str, diary_id: i64, visibility: &str) -> KnowledgeAction {
    KnowledgeAction {
        operation: "create".to_string(),
        target_id: None,
        memory_type: "fact".to_string(),
        content: content.to_string(),
        truth_status: "reported".to_string(),
        importance: 3,
        confidence: 0.8,
        visibility: visibility.to_string(),
        subjects: Vec::new(),
        tags: Vec::new(),
        diary_ids: vec![diary_id],
    }
}

async fn batch_with_diaries(store: &MemoryStore) -> OrganizationBatch {
    assert!(record_turn(store, "第一轮问题", "第一轮回答"));
    assert!(record_turn(store, "第二轮问题", "第二轮回答"));
    let batch = store.next_organization_batch().unwrap().unwrap();
    assert_eq!(batch.diaries.len(), 2, "fixture should yield one batch");
    batch
}

#[tokio::test]
async fn a_near_duplicate_create_is_folded_into_an_update() {
    let temp = tempfile::tempdir().unwrap();
    let config = config_without_embeddings(2);
    let store = MemoryStore::new(&config, &test_paths(&temp));
    let mut batch = batch_with_diaries(&store).await;

    let stored = "本机没装 npm,用 bunx 跑 dsh";
    let fact_id = insert_fact(&store, stored, "privileged");
    batch
        .existing
        .push(candidate(fact_id, stored, "privileged"));
    // 只差标点与大小写：归一化后是同一句。
    let mut output = OrganizedOutput {
        knowledge: vec![create_action(
            "本机没装 npm，用 bunx 跑 DSH",
            batch.diaries[0].id,
            "privileged",
        )],
        long_diaries: Vec::new(),
    };
    assert_eq!(
        store.fold_near_duplicate_creates(&batch, &mut output).await,
        1
    );
    assert_eq!(output.knowledge[0].operation, "update");
    assert_eq!(output.knowledge[0].target_id, Some(fact_id));
    assert_eq!(output.knowledge[0].content, "本机没装 npm，用 bunx 跑 DSH");
}

#[tokio::test]
async fn an_unrelated_create_is_left_alone() {
    let temp = tempfile::tempdir().unwrap();
    let config = config_without_embeddings(2);
    let store = MemoryStore::new(&config, &test_paths(&temp));
    let mut batch = batch_with_diaries(&store).await;

    let stored = "本机没装 npm,用 bunx 跑 dsh";
    let fact_id = insert_fact(&store, stored, "privileged");
    batch
        .existing
        .push(candidate(fact_id, stored, "privileged"));
    let mut output = OrganizedOutput {
        knowledge: vec![create_action(
            "用户偏好深色主题",
            batch.diaries[0].id,
            "privileged",
        )],
        long_diaries: Vec::new(),
    };
    assert_eq!(
        store.fold_near_duplicate_creates(&batch, &mut output).await,
        0
    );
    assert_eq!(output.knowledge[0].operation, "create");
    assert_eq!(output.knowledge[0].target_id, None);
}

/// 可见性不同不是「同一条」：改写会被 validate 打回，那就别改。
#[tokio::test]
async fn a_visibility_mismatch_keeps_the_create() {
    let temp = tempfile::tempdir().unwrap();
    let config = config_without_embeddings(2);
    let store = MemoryStore::new(&config, &test_paths(&temp));
    let mut batch = batch_with_diaries(&store).await;

    let stored = "本机没装 npm,用 bunx 跑 dsh";
    let fact_id = insert_fact(&store, stored, "public");
    batch.existing.push(candidate(fact_id, stored, "public"));
    let mut output = OrganizedOutput {
        knowledge: vec![create_action(
            "本机没装 npm，用 bunx 跑 dsh",
            batch.diaries[0].id,
            "privileged",
        )],
        long_diaries: Vec::new(),
    };
    assert_eq!(
        store.fold_near_duplicate_creates(&batch, &mut output).await,
        0
    );
    assert_eq!(output.knowledge[0].operation, "create");
}

#[tokio::test]
async fn widening_is_a_no_op_without_an_embedder() {
    let temp = tempfile::tempdir().unwrap();
    let config = config_without_embeddings(2);
    let store = MemoryStore::new(&config, &test_paths(&temp));
    let mut batch = batch_with_diaries(&store).await;

    insert_fact(&store, "本机没装 npm,用 bunx 跑 dsh", "privileged");
    let before = batch.existing.len();
    assert_eq!(store.widen_existing_candidates(&mut batch).await, 0);
    assert_eq!(batch.existing.len(), before);
}

/// 向量那条路要真实模型:换个说法、词面完全对不上的近义事实也要折叠。
#[tokio::test]
async fn a_paraphrased_create_is_folded_by_embeddings() {
    if crate::embedding::runtime_library().is_err() {
        eprintln!("skipping: ONNX Runtime library not installed");
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let mut config = diary_config(2);
    config.embedding.backend = crate::config::EmbeddingBackend::Local;
    let store = MemoryStore::new(&config, &test_paths(&temp));
    let mut batch = batch_with_diaries(&store).await;

    let stored = "本机没装 npm,用 bunx 跑 dsh";
    let fact_id = insert_fact(&store, stored, "privileged");
    batch
        .existing
        .push(candidate(fact_id, stored, "privileged"));
    // 词面完全不同(归一化比较抓不到),只有向量认得出是同一件事。
    let mut output = OrganizedOutput {
        knowledge: vec![create_action(
            "这台机器上没有 npm,启动 dsh 得用 bunx",
            batch.diaries[0].id,
            "privileged",
        )],
        long_diaries: Vec::new(),
    };
    assert_eq!(
        store.fold_near_duplicate_creates(&batch, &mut output).await,
        1
    );
    assert_eq!(output.knowledge[0].operation, "update");
    assert_eq!(output.knowledge[0].target_id, Some(fact_id));
    crate::embedding::shutdown_worker().await;
}

/// 语义扩展要真实模型:词面完全没有交集的旧事实也得补进候选列表,并把现算
/// 出来的向量存回库里(下一批直接复用)。
#[tokio::test]
async fn widening_adds_a_semantically_near_fact_and_keeps_its_vector() {
    if crate::embedding::runtime_library().is_err() {
        eprintln!("skipping: ONNX Runtime library not installed");
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let mut config = diary_config(2);
    config.embedding.backend = crate::config::EmbeddingBackend::Local;
    let store = MemoryStore::new(&config, &test_paths(&temp));
    assert!(record_turn(
        &store,
        "dsh 怎么启动",
        "本机没装 npm,用 bunx 跑 dsh"
    ));
    assert!(record_turn(
        &store,
        "再问一次 dsh",
        "还是 bunx,本机没有 npm"
    ));
    let mut batch = store.next_organization_batch().unwrap().unwrap();

    let near = insert_fact(&store, "启动 dsh 要用 bunx,因为本机没有 npm", "privileged");
    let unrelated = insert_fact(&store, "今天午饭吃了麻辣烫", "privileged");
    let added = store.widen_existing_candidates(&mut batch).await;
    assert!(added >= 1, "语义相近的旧事实应被补进候选列表");
    let ids = batch
        .existing
        .iter()
        .map(|memory| memory.id)
        .collect::<Vec<_>>();
    assert!(ids.contains(&near), "near fact missing from {ids:?}");
    assert!(
        !ids.contains(&unrelated),
        "unrelated fact leaked in: {ids:?}"
    );
    let stored: i64 = store
        .data_conn()
        .unwrap()
        .query_row(
            "SELECT COUNT(*) FROM memory_embeddings WHERE kind='fact' AND id=?1",
            [near],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(stored, 1, "现算的向量应存回库");
    crate::embedding::shutdown_worker().await;
}
