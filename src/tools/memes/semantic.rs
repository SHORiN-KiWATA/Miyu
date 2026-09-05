//! 表情包语义检索：把每张表情的名字/描述/用法/标签嵌入成向量，查询时与
//! 关键词排名做 RRF 融合。
//!
//! 向量缓存在 `cache_dir/meme-embeddings/<library>.db`（内置库目录只读，
//! 且向量是派生数据，放 cache 合适），按 (model, 文本 sha256) 判断新旧，
//! 换模型或改描述都会自然重算。任何一步失败都返回 `None`——关键词排名
//! 独立成立（09-05 实测:hit@3 关键词 16% → 融合 33%）。

use super::LoadedMeme;
use crate::config::AppConfig;
use crate::embedding::{cosine, vector_from_blob, vector_to_blob, Embedder};
use crate::paths::MiyuPaths;
use anyhow::Result;
use rusqlite::{params, Connection};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::PathBuf;

/// New vectors computed per search; a fresh 100-meme library fills in on the
/// first call (~1 s locally), a huge one over a few calls.
const EMBED_BATCH_PER_SEARCH: usize = 128;

pub(crate) fn meme_text(meme: &LoadedMeme) -> String {
    let item = &meme.item;
    let tags = item.tags.join(" ");
    [
        item.name.zh.trim(),
        item.name.en.trim(),
        item.description.trim(),
        item.usage.trim(),
        tags.trim(),
    ]
    .into_iter()
    .filter(|part| !part.is_empty())
    .collect::<Vec<_>>()
    .join(" ")
}

fn text_sha(text: &str) -> String {
    hex::encode(Sha256::digest(text.as_bytes()))
}

fn vectors_db(paths: &MiyuPaths, library: &str) -> Result<PathBuf> {
    let dir = paths.cache_dir.join("meme-embeddings");
    std::fs::create_dir_all(&dir)?;
    Ok(dir.join(format!("{}.db", super::sanitize_library(library))))
}

fn open(path: &PathBuf) -> Result<Connection> {
    let conn = Connection::open(path)?;
    conn.busy_timeout(std::time::Duration::from_secs(5))?;
    conn.execute_batch(
        "PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;
         CREATE TABLE IF NOT EXISTS meme_vectors (
            id TEXT PRIMARY KEY,
            model TEXT NOT NULL,
            text_sha256 TEXT NOT NULL,
            embedding BLOB NOT NULL,
            updated_at REAL NOT NULL
         );",
    )?;
    Ok(conn)
}

struct Entry {
    id: String,
    text: String,
    sha: String,
}

fn entries(memes: &[LoadedMeme]) -> Vec<Entry> {
    memes
        .iter()
        .map(|meme| {
            let text = meme_text(meme);
            let sha = text_sha(&text);
            Entry {
                id: meme.item.id.clone(),
                text,
                sha,
            }
        })
        .collect()
}

/// Current vectors by meme id, plus the entries that still need one.
fn load_vectors<'a>(
    conn: &Connection,
    model: &str,
    entries: &'a [Entry],
) -> Result<(HashMap<String, Vec<f32>>, Vec<&'a Entry>)> {
    let mut stored = HashMap::new();
    let mut missing = Vec::new();
    let mut stmt = conn
        .prepare("SELECT text_sha256, embedding FROM meme_vectors WHERE id = ?1 AND model = ?2")?;
    for entry in entries {
        let row: Option<(String, Vec<u8>)> = stmt
            .query_row(params![entry.id, model], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })
            .ok();
        match row {
            Some((known_sha, blob)) if known_sha == entry.sha => match vector_from_blob(&blob) {
                Some(vector) => {
                    stored.insert(entry.id.clone(), vector);
                }
                None => missing.push(entry),
            },
            _ => missing.push(entry),
        }
    }
    Ok((stored, missing))
}

/// Embed up to one batch of memes lacking a current vector; returns how many.
async fn top_up(
    embedder: &Embedder,
    paths: &MiyuPaths,
    library: &str,
    memes: &[LoadedMeme],
) -> Result<usize> {
    let db_path = vectors_db(paths, library)?;
    let model = embedder.model_id().to_string();
    let all = entries(memes);
    let batch: Vec<&Entry> = {
        let conn = open(&db_path)?;
        let (_, missing) = load_vectors(&conn, &model, &all)?;
        missing.into_iter().take(EMBED_BATCH_PER_SEARCH).collect()
    };
    if batch.is_empty() {
        return Ok(0);
    }
    let inputs: Vec<String> = batch.iter().map(|entry| entry.text.clone()).collect();
    let vectors = embedder.embed(&inputs).await?;
    let conn = open(&db_path)?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or_default();
    let mut count = 0;
    for (entry, vector) in batch.iter().zip(vectors) {
        conn.execute(
            "INSERT INTO meme_vectors (id, model, text_sha256, embedding, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT (id) DO UPDATE SET
                model = excluded.model,
                text_sha256 = excluded.text_sha256,
                embedding = excluded.embedding,
                updated_at = excluded.updated_at",
            params![entry.id, model, entry.sha, vector_to_blob(&vector), now],
        )?;
        count += 1;
    }
    Ok(count)
}

/// Embed every meme in `library` that has no current vector. Returns how
/// many were embedded.
pub(crate) async fn reindex_library(
    config: &AppConfig,
    paths: &MiyuPaths,
    library: &str,
) -> Result<usize> {
    let Some(embedder) = Embedder::from_config(config) else {
        return Ok(0);
    };
    let memes = super::load_library(paths, library)?;
    let mut total = 0;
    loop {
        let embedded = top_up(&embedder, paths, library, &memes).await?;
        if embedded == 0 {
            return Ok(total);
        }
        total += embedded;
    }
}

/// Meme ids ranked best-first by semantic similarity to `query`, or `None`
/// when the semantic pass is unavailable. Also tops up missing vectors.
pub(crate) async fn semantic_rank(
    config: &AppConfig,
    paths: &MiyuPaths,
    library: &str,
    memes: &[LoadedMeme],
    query: &str,
) -> Option<Vec<String>> {
    let query = query.trim();
    if query.is_empty() || memes.is_empty() {
        return None;
    }
    let embedder = Embedder::from_config(config)?;
    match semantic_rank_inner(&embedder, paths, library, memes, query).await {
        Ok(ranked) => Some(ranked),
        Err(error) => {
            tracing::debug!(error = %error, library, "meme semantic search unavailable");
            None
        }
    }
}

async fn semantic_rank_inner(
    embedder: &Embedder,
    paths: &MiyuPaths,
    library: &str,
    memes: &[LoadedMeme],
    query: &str,
) -> Result<Vec<String>> {
    top_up(embedder, paths, library, memes).await?;
    let query_vector = embedder.embed_query(query).await?;
    let all = entries(memes);
    let stored = {
        let conn = open(&vectors_db(paths, library)?)?;
        load_vectors(&conn, embedder.model_id(), &all)?.0
    };
    let mut scored: Vec<(f32, &str)> = all
        .iter()
        .filter_map(|entry| {
            let vector = stored.get(&entry.id)?;
            let score = cosine(&query_vector, vector);
            (score >= embedder.min_score()).then_some((score, entry.id.as_str()))
        })
        .collect();
    scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
    Ok(scored.into_iter().map(|(_, id)| id.to_string()).collect())
}
