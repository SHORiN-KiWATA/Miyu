//! 记忆联想的语义辅助：关键词候选与向量候选做 RRF 融合。
//!
//! 向量存在 data 库的 `memory_embeddings`（kind,id 主键，带 model 与内容 sha256），
//! 换模型或改内容后自然重算。补建分两层：每次联想顶多补 32 条（不让一轮对话
//! 为整库买单），另起一个后台任务把剩余的慢慢补完（每批 64 条、批间歇一下）。
//! 语义任何一步失败都退回纯关键词候选——它是辅助,不是前提。
//!
//! 连接不跨 await 持有：`rusqlite::Connection` 不是 `Sync`，回合 future 需要
//! `Send`，所以"查 → 嵌入 → 写"是三段各自开关连接。

use crate::embedding::{cosine, rrf_fuse, vector_from_blob, vector_to_blob, Embedder, RRF_K};
use crate::memory::*;
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::sync::{Mutex, OnceLock};

/// Rows embedded inline per association call.
const INLINE_TOP_UP: usize = 32;
/// Rows embedded per background batch, and the pause between batches.
const BACKFILL_BATCH: usize = 64;
const BACKFILL_PAUSE: std::time::Duration = std::time::Duration::from_millis(250);
/// Candidate depth handed to the fusion per kind.
const SEMANTIC_CANDIDATES: usize = 12;
const CORPUS_LIMIT: usize = 5000;

fn content_sha(content: &str) -> String {
    hex::encode(Sha256::digest(content.as_bytes()))
}

fn kind_name(kind: MemoryKind) -> &'static str {
    match kind {
        MemoryKind::Fact => "fact",
        MemoryKind::Diary => "episode",
    }
}

struct CorpusRow {
    id: i64,
    content: String,
    sha: String,
    /// Stored vector for the current model, when up to date.
    vector: Option<Vec<f32>>,
}

impl MemoryStore {
    /// [`Self::association`] plus the semantic pass. Falls back to the keyword
    /// result whenever the embedder is unavailable or fails mid-way.
    pub async fn association_with_semantic(
        &self,
        query: &str,
        exclude: Option<&AssociationExclusion>,
    ) -> Result<Option<AssociationContext>> {
        if !self.config.enabled || !self.config.association_enabled {
            return Ok(None);
        }
        let Some(embedder) = Embedder::from_config(&self.app_config) else {
            return self.association(query, exclude);
        };
        // Phase 1 (sync): keyword candidates + corpus snapshot.
        let (facts, episodes, mut fact_rows, mut episode_rows) = {
            let conn = self.data_conn()?;
            let (facts, episodes) = self.association_candidates(&conn, query, exclude)?;
            let fact_rows =
                self.memory_semantic_corpus(&conn, MemoryKind::Fact, embedder.model_id())?;
            let episode_rows =
                self.memory_semantic_corpus(&conn, MemoryKind::Diary, embedder.model_id())?;
            (facts, episodes, fact_rows, episode_rows)
        };
        // Phase 2 (async): embed the query and a bounded slice of stale rows.
        let fused = self
            .semantic_fuse(
                &embedder,
                query,
                exclude,
                &facts,
                &episodes,
                &mut fact_rows,
                &mut episode_rows,
            )
            .await;
        let conn = self.data_conn()?;
        match fused {
            Ok((facts, episodes)) => self.finish_association(&conn, facts, episodes),
            Err(error) => {
                tracing::debug!(error = %error, "memory semantic pass unavailable; keyword only");
                self.finish_association(&conn, facts, episodes)
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn semantic_fuse(
        &self,
        embedder: &Embedder,
        query: &str,
        exclude: Option<&AssociationExclusion>,
        keyword_facts: &[MemoryHit],
        keyword_episodes: &[MemoryHit],
        fact_rows: &mut Vec<CorpusRow>,
        episode_rows: &mut Vec<CorpusRow>,
    ) -> Result<(Vec<MemoryHit>, Vec<MemoryHit>)> {
        // Top up: newest rows first (corpus is ordered by updated_at DESC).
        let mut pending: Vec<(MemoryKind, usize)> = Vec::new();
        for (kind, rows) in [
            (MemoryKind::Fact, &*fact_rows),
            (MemoryKind::Diary, &*episode_rows),
        ] {
            for (index, row) in rows.iter().enumerate() {
                if row.vector.is_none() {
                    pending.push((kind, index));
                }
            }
        }
        let remaining = pending.len().saturating_sub(INLINE_TOP_UP);
        pending.truncate(INLINE_TOP_UP);
        let mut texts: Vec<String> = Vec::with_capacity(pending.len() + 1);
        for (kind, index) in &pending {
            let rows = match kind {
                MemoryKind::Fact => &*fact_rows,
                MemoryKind::Diary => &*episode_rows,
            };
            texts.push(rows[*index].content.clone());
        }
        let query_vector = embedder.embed_query(query).await?;
        let vectors = embedder.embed(&texts).await?;
        // Phase 3 (sync): persist new vectors, rank, materialize.
        let conn = self.data_conn()?;
        let now = now();
        for ((kind, index), vector) in pending.iter().zip(vectors) {
            let rows = match kind {
                MemoryKind::Fact => &mut *fact_rows,
                MemoryKind::Diary => &mut *episode_rows,
            };
            let row = &mut rows[*index];
            conn.execute(
                "INSERT INTO memory_embeddings (kind, id, model, content_sha256, embedding, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                 ON CONFLICT (kind, id) DO UPDATE SET
                    model = excluded.model,
                    content_sha256 = excluded.content_sha256,
                    embedding = excluded.embedding,
                    created_at = excluded.created_at",
                params![
                    kind_name(*kind),
                    row.id,
                    embedder.model_id(),
                    row.sha,
                    vector_to_blob(&vector),
                    now
                ],
            )?;
            row.vector = Some(vector);
        }
        if remaining > 0 {
            self.spawn_backfill(embedder.model_id().to_string());
        }
        let facts = self.fuse_kind(
            &conn,
            MemoryKind::Fact,
            &query_vector,
            embedder.min_score(),
            keyword_facts,
            fact_rows,
            self.config.association_facts,
            None,
        )?;
        let episodes = self.fuse_kind(
            &conn,
            MemoryKind::Diary,
            &query_vector,
            embedder.min_score(),
            keyword_episodes,
            episode_rows,
            self.config.association_episodes,
            exclude,
        )?;
        Ok((facts, episodes))
    }

    #[allow(clippy::too_many_arguments)]
    fn fuse_kind(
        &self,
        conn: &Connection,
        kind: MemoryKind,
        query_vector: &[f32],
        min_score: f32,
        keyword: &[MemoryHit],
        rows: &[CorpusRow],
        limit: usize,
        exclude: Option<&AssociationExclusion>,
    ) -> Result<Vec<MemoryHit>> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let mut scored: Vec<(f32, i64)> = rows
            .iter()
            .filter_map(|row| {
                let vector = row.vector.as_ref()?;
                let score = cosine(query_vector, vector);
                (score >= min_score).then_some((score, row.id))
            })
            .collect();
        scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
        scored.truncate(SEMANTIC_CANDIDATES);
        let semantic_ids: Vec<i64> = scored.iter().map(|(_, id)| *id).collect();
        if semantic_ids.is_empty() {
            return Ok(keyword.to_vec());
        }
        let keyword_ids: Vec<i64> = keyword.iter().map(|hit| hit.id).collect();
        let fused = rrf_fuse(&[keyword_ids, semantic_ids], RRF_K);
        let mut by_id: HashMap<i64, MemoryHit> =
            keyword.iter().map(|hit| (hit.id, hit.clone())).collect();
        let missing: Vec<i64> = fused
            .iter()
            .map(|(id, _)| *id)
            .filter(|id| !by_id.contains_key(id))
            .collect();
        let mut fetched = self.hits_by_ids(conn, kind, &missing)?;
        if kind == MemoryKind::Diary {
            self.apply_self_echo_exclusion(&mut fetched, exclude);
        }
        for hit in fetched {
            by_id.insert(hit.id, hit);
        }
        let semantic_score: HashMap<i64, f32> = scored.into_iter().map(|(s, id)| (id, s)).collect();
        let mut result = Vec::with_capacity(limit);
        for (id, fused_score) in fused {
            let Some(mut hit) = by_id.remove(&id) else {
                continue;
            };
            // Keep the lexical score when it exists; semantic-only hits get a
            // comparable magnitude so downstream ordering stays meaningful.
            if hit.score <= 0.0 {
                hit.score = semantic_score.get(&id).copied().unwrap_or(0.0) * SEMANTIC_SCORE_WEIGHT;
            }
            hit.score += fused_score as f32;
            result.push(hit);
            if result.len() >= limit {
                break;
            }
        }
        Ok(result)
    }

    /// Visible rows of one kind with their stored vector (if current).
    fn memory_semantic_corpus(
        &self,
        conn: &Connection,
        kind: MemoryKind,
        model: &str,
    ) -> Result<Vec<CorpusRow>> {
        let table = match kind {
            MemoryKind::Fact => "facts",
            MemoryKind::Diary => "episodes",
        };
        let status_filter = if kind == MemoryKind::Fact {
            "status!='forgotten' AND truth_status!='rejected'"
        } else {
            "status!='forgotten'"
        };
        let access_filter = if self.access.principal_key().is_some() {
            " AND (visibility='public' OR (visibility='principal' AND owner_principal=?1))"
        } else {
            ""
        };
        let sql = format!(
            "SELECT t.id, t.content, e.content_sha256, e.embedding
             FROM {table} t
             LEFT JOIN memory_embeddings e ON e.kind = ?{} AND e.id = t.id AND e.model = ?{}
             WHERE {status_filter}{access_filter}
             ORDER BY t.updated_at DESC LIMIT {CORPUS_LIMIT}",
            if access_filter.is_empty() { 1 } else { 2 },
            if access_filter.is_empty() { 2 } else { 3 },
        );
        let mut stmt = conn.prepare(&sql)?;
        let mut params: Vec<rusqlite::types::Value> = Vec::new();
        if let Some(principal) = self.access.principal_key() {
            params.push(principal.to_string().into());
        }
        params.push(kind_name(kind).to_string().into());
        params.push(model.to_string().into());
        let mut rows_iter = stmt.query(rusqlite::params_from_iter(params))?;
        let mut rows = Vec::new();
        while let Some(row) = rows_iter.next()? {
            let id: i64 = row.get(0)?;
            let content: String = row.get(1)?;
            let stored_sha: Option<String> = row.get(2)?;
            let blob: Option<Vec<u8>> = row.get(3)?;
            let sha = content_sha(&content);
            let vector = match (stored_sha, blob) {
                (Some(stored), Some(blob)) if stored == sha => vector_from_blob(&blob),
                _ => None,
            };
            rows.push(CorpusRow {
                id,
                content,
                sha,
                vector,
            });
        }
        Ok(rows)
    }

    /// One backfill task per (database, model) at a time; later calls are
    /// no-ops while it runs.
    fn spawn_backfill(&self, model: String) {
        static RUNNING: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
        let key = format!("{}|{model}", self.data_db.display());
        {
            let mut running = RUNNING.get_or_init(Default::default).lock().unwrap();
            if !running.insert(key.clone()) {
                return;
            }
        }
        let store = self.clone();
        tokio::spawn(async move {
            if let Err(error) = store.backfill_embeddings(&model).await {
                tracing::debug!(error = %error, "memory embedding backfill stopped");
            }
            RUNNING
                .get_or_init(Default::default)
                .lock()
                .unwrap()
                .remove(&key);
        });
    }

    /// Embed every visible row that has no current vector, in bounded batches.
    /// Also used by `miyu memory embed reindex`-style maintenance.
    pub(crate) async fn backfill_embeddings(&self, model: &str) -> Result<usize> {
        let Some(embedder) = Embedder::from_config(&self.app_config) else {
            return Ok(0);
        };
        if embedder.model_id() != model {
            return Ok(0);
        }
        let mut total = 0usize;
        loop {
            let batch: Vec<(MemoryKind, i64, String, String)> = {
                let conn = self.data_conn()?;
                let mut batch = Vec::new();
                for kind in [MemoryKind::Fact, MemoryKind::Diary] {
                    for row in self.memory_semantic_corpus(&conn, kind, model)? {
                        if row.vector.is_none() {
                            batch.push((kind, row.id, row.content, row.sha));
                        }
                        if batch.len() >= BACKFILL_BATCH {
                            break;
                        }
                    }
                    if batch.len() >= BACKFILL_BATCH {
                        break;
                    }
                }
                batch
            };
            if batch.is_empty() {
                return Ok(total);
            }
            let texts: Vec<String> = batch
                .iter()
                .map(|(_, _, content, _)| content.clone())
                .collect();
            let vectors = embedder.embed(&texts).await?;
            let conn = self.data_conn()?;
            let now = now();
            for ((kind, id, _, sha), vector) in batch.iter().zip(vectors) {
                conn.execute(
                    "INSERT INTO memory_embeddings (kind, id, model, content_sha256, embedding, created_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                     ON CONFLICT (kind, id) DO UPDATE SET
                        model = excluded.model,
                        content_sha256 = excluded.content_sha256,
                        embedding = excluded.embedding,
                        created_at = excluded.created_at",
                    params![kind_name(*kind), id, model, sha, vector_to_blob(&vector), now],
                )?;
                total += 1;
            }
            drop(conn);
            tokio::time::sleep(BACKFILL_PAUSE).await;
        }
    }

    /// Counts for diagnostics: (rows with a current vector, rows without).
    pub(crate) fn embedding_coverage(&self, model: &str) -> Result<(usize, usize)> {
        if !self.data_db.is_file() {
            return Ok((0, 0));
        }
        let conn = self.data_conn_existing()?;
        let mut done = 0;
        let mut missing = 0;
        for kind in [MemoryKind::Fact, MemoryKind::Diary] {
            for row in self.memory_semantic_corpus(&conn, kind, model)? {
                if row.vector.is_some() {
                    done += 1;
                } else {
                    missing += 1;
                }
            }
        }
        Ok((done, missing))
    }
}
