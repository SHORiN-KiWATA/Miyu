//! 记忆联想的语义融合：关键词找不到的改写也能被向量找回；语义不可用时
//! 结果与纯关键词一字不差。真实推理走 worker 子进程，本机没装 ONNX Runtime
//! 时相关用例跳过（打印说明），不算失败。

use super::shared::test_paths;
use crate::config::{AppConfig, EmbeddingBackend};
use crate::memory::*;

fn store_with(config: &AppConfig, temp: &tempfile::TempDir) -> MemoryStore {
    let paths = test_paths(temp);
    let store = MemoryStore::new(config, &paths);
    store.init().unwrap();
    store
}

fn insert_episode(store: &MemoryStore, content: &str) -> i64 {
    let conn = store.data_conn().unwrap();
    conn.execute(
        "INSERT INTO episodes (content, source, status, recall_count, created_at, updated_at, retention, origin_session_id)
         VALUES (?1, 'auto_diary', 'active', 0, ?2, ?2, 'short_term', 'other-session')",
        rusqlite::params![content, chrono::Utc::now().to_rfc3339()],
    )
    .unwrap();
    conn.last_insert_rowid()
}

fn runtime_available() -> bool {
    if crate::embedding::runtime_library().is_err() {
        eprintln!("skipping: ONNX Runtime library not installed");
        return false;
    }
    true
}

#[tokio::test]
async fn disabled_semantic_matches_keyword_only_exactly() {
    let temp = tempfile::tempdir().unwrap();
    let mut config = AppConfig::default();
    config.embedding.enabled = false;
    let store = store_with(&config, &temp);
    insert_episode(
        &store,
        "对方说：grub 更新后进不了系统；我回：改的是 grubenv 那个环境块",
    );
    let keyword = store.association("grub 环境块", None).unwrap().unwrap();
    let fused = store
        .association_with_semantic("grub 环境块", None)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        store.format_association(&keyword),
        store.format_association(&fused)
    );
    // Nothing was embedded on the disabled path.
    let rows: i64 = store
        .data_conn()
        .unwrap()
        .query_row("SELECT COUNT(*) FROM memory_embeddings", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(rows, 0);
}

#[tokio::test]
async fn a_missing_runtime_degrades_to_keyword_results() {
    let temp = tempfile::tempdir().unwrap();
    let mut config = AppConfig::default();
    config.embedding.backend = EmbeddingBackend::Local;
    // A model directory that cannot possibly load: the worker must fail and
    // the association must still answer from keywords.
    let bogus = temp.path().join("bogus-model");
    std::fs::create_dir_all(&bogus).unwrap();
    std::fs::write(bogus.join("manifest.json"), r#"{"id":"bogus","dims":4}"#).unwrap();
    std::fs::write(bogus.join("model.onnx"), b"not a model").unwrap();
    std::fs::write(bogus.join("tokenizer.json"), b"{}").unwrap();
    config.embedding.local_model = bogus.to_string_lossy().into_owned();
    let store = store_with(&config, &temp);
    insert_episode(&store, "对方说：想吃炸鸡；我回：楼下那家新开的评价不错");
    let fused = store
        .association_with_semantic("炸鸡", None)
        .await
        .unwrap()
        .expect("keyword hit survives a broken semantic pass");
    assert_eq!(fused.episodes.len(), 1);
    crate::embedding::shutdown_worker().await;
}

#[tokio::test]
async fn paraphrase_is_found_by_the_semantic_pass() {
    if !runtime_available() {
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let mut config = AppConfig::default();
    config.embedding.backend = EmbeddingBackend::Local;
    let store = store_with(&config, &temp);
    let target = insert_episode(
        &store,
        "对方说：显卡驱动装完黑屏了；我回：先进 tty 把 nvidia 模块加进 mkinitcpio 再重建镜像",
    );
    for filler in [
        "对方说：今天中午吃什么；我回：楼下的麻辣烫",
        "对方说：推荐一部电影；我回：星际穿越",
        "对方说：周末去爬山吗；我回：看天气",
    ] {
        insert_episode(&store, filler);
    }
    // No shared token with any row — not even a single CJK character: the
    // keyword scorer keeps single-character tokens, so "卡" would match "显卡"
    // and "的" would match the filler "楼下的麻辣烫".
    let query = "GPU driver 弄好以后重启就没画面";
    let keyword_only = store.association(query, None).unwrap();
    assert!(
        !keyword_only
            .as_ref()
            .is_some_and(|context| context.episodes.iter().any(|hit| hit.id == target)),
        "keyword path must not find the target on its own"
    );
    let embedder = crate::embedding::Embedder::from_config(&config).unwrap();
    let pair = embedder
        .embed(&[
            query.to_string(),
            "对方说：显卡驱动装完黑屏了；我回：先进 tty 把 nvidia 模块加进 mkinitcpio 再重建镜像"
                .to_string(),
        ])
        .await
        .unwrap();
    let similarity = crate::embedding::cosine(&pair[0], &pair[1]);
    let fused = store
        .association_with_semantic(query, None)
        .await
        .unwrap()
        .unwrap_or_else(|| {
            panic!(
                "semantic pass finds the paraphrase (cosine {similarity}, floor {})",
                embedder.min_score()
            )
        });
    assert!(
        fused.episodes.iter().any(|hit| hit.id == target),
        "target missing from {:?} (cosine {similarity})",
        fused.episodes.iter().map(|hit| hit.id).collect::<Vec<_>>()
    );
    // Vectors were persisted with the model tag and content hash.
    let rows: i64 = store
        .data_conn()
        .unwrap()
        .query_row(
            "SELECT COUNT(*) FROM memory_embeddings WHERE model LIKE 'local:%'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(rows, 4);
    crate::embedding::shutdown_worker().await;
}

/// 量尺:用真实语料 + LLM 改写查询对比纯关键词与融合的 hit@3。
/// `MIYU_EMBED_EVAL_DIR` 指向含 `memory.jsonl`(`{"id","text"}`)与
/// `memory-q.jsonl`(`{"q","expect":[id]}`)的目录。
/// cargo test --lib memory::tests::semantic::eval_real_corpus -- --ignored --nocapture
#[tokio::test]
#[ignore]
async fn eval_real_corpus() {
    let Ok(dir) = std::env::var("MIYU_EMBED_EVAL_DIR") else {
        eprintln!("set MIYU_EMBED_EVAL_DIR");
        return;
    };
    if !runtime_available() {
        return;
    }
    let dir = std::path::PathBuf::from(dir);
    let temp = tempfile::tempdir().unwrap();
    let mut config = AppConfig::default();
    config.embedding.backend = EmbeddingBackend::Local;
    config.plugins.memory.association_episodes = 3;
    config.plugins.memory.association_facts = 3;
    let store = store_with(&config, &temp);
    let corpus = std::fs::read_to_string(dir.join("memory.jsonl")).unwrap();
    let mut id_map = std::collections::HashMap::new();
    for line in corpus.lines().filter(|l| !l.trim().is_empty()) {
        let row: serde_json::Value = serde_json::from_str(line).unwrap();
        let text = row["text"].as_str().unwrap();
        let rowid = insert_episode(&store, text);
        id_map.insert(row["id"].as_str().unwrap().to_string(), rowid);
    }
    // Fill every vector first so the comparison measures ranking, not backlog.
    let embedder = crate::embedding::Embedder::from_config(&config).unwrap();
    let embedded = store
        .backfill_embeddings(embedder.model_id())
        .await
        .unwrap();
    eprintln!("embedded {embedded} rows");
    let queries = std::fs::read_to_string(dir.join("memory-q.jsonl")).unwrap();
    let (mut n, mut keyword_hits, mut fused_hits) = (0, 0, 0);
    for line in queries.lines().filter(|l| !l.trim().is_empty()) {
        let row: serde_json::Value = serde_json::from_str(line).unwrap();
        let query = row["q"].as_str().unwrap();
        let Some(expect) = row["expect"][0].as_str().and_then(|id| id_map.get(id)) else {
            continue;
        };
        n += 1;
        if let Some(keyword) = store.association(query, None).unwrap() {
            if keyword.episodes.iter().any(|hit| hit.id == *expect) {
                keyword_hits += 1;
            }
        }
        if let Some(fused) = store.association_with_semantic(query, None).await.unwrap() {
            if fused.episodes.iter().any(|hit| hit.id == *expect) {
                fused_hits += 1;
            }
        }
    }
    eprintln!(
        "EVAL n={n} keyword hit@3={keyword_hits} ({:.0}%) fused hit@3={fused_hits} ({:.0}%)",
        100.0 * keyword_hits as f64 / n.max(1) as f64,
        100.0 * fused_hits as f64 / n.max(1) as f64
    );
    crate::embedding::shutdown_worker().await;
}
