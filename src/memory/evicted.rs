//! 被逐出上下文的回合的检索。
//!
//! 压缩掉的回合不是消失了，而是转成可检索的记录。这条路和长期记忆是**两套
//! 库**：逐出内容按会话走、量大、寿命短；长期记忆跟人格走。
//!
//! 检索是混合的（`search_evicted_context_hybrid`）：关键词打底、语义加权，
//! 嵌入不可用就退回纯关键词。

use crate::memory::*;

impl MemoryStore {
    #[allow(dead_code)]
    pub fn remember_evicted_turns(&self, turns: &[EvictedTurn]) -> Result<()> {
        if !self.config.enabled
            || !self.writes_enabled
            || !self.config.evicted_context_enabled
            || turns.is_empty()
        {
            return Ok(());
        }
        self.init()?;
        let fallback = self.writer_ownership();
        let mut conn = self.state_conn()?;
        let tx = conn.transaction()?;
        for turn in turns {
            let visibility = if turn.visibility.trim().is_empty() {
                fallback.visibility
            } else {
                turn.visibility.as_str()
            };
            let owner_principal = if turn.owner_principal.trim().is_empty() {
                fallback.owner_principal.as_str()
            } else {
                turn.owner_principal.as_str()
            };
            let owner_display_name = if turn.owner_display_name.trim().is_empty() {
                fallback.owner_display_name.as_str()
            } else {
                turn.owner_display_name.as_str()
            };
            tx.execute(
                "INSERT OR IGNORE INTO evicted_turns (
                    source_id, timestamp, role, content, created_at,
                    visibility, owner_principal, owner_display_name
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    turn.source_id,
                    turn.timestamp,
                    turn.role,
                    turn.content,
                    now(),
                    visibility,
                    owner_principal,
                    owner_display_name,
                ],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    pub(crate) fn prepare_evicted_context_db(&self) -> Result<Option<PathBuf>> {
        if !self.config.enabled || !self.writes_enabled || !self.config.evicted_context_enabled {
            return Ok(None);
        }
        self.init()?;
        Ok(Some(self.state_db.clone()))
    }

    pub fn clear_evicted_context(&self) -> Result<()> {
        self.init()?;
        self.state_conn()?
            .execute("DELETE FROM evicted_turns", [])?;
        Ok(())
    }

    #[allow(dead_code)]
    pub fn search_evicted_context(&self, query: &str, limit: usize) -> Result<Value> {
        self.init()?;
        self.search_evicted_context_existing(query, limit)
    }

    pub fn search_evicted_context_readonly(
        &self,
        query: &str,
        limit: usize,
        start: Option<&str>,
        end: Option<&str>,
    ) -> Result<Value> {
        if !self.state_db.is_file() {
            return Ok(json!({ "ok": true, "query": query, "results": [] }));
        }
        self.search_evicted_context_filtered(query, limit, start, end)
    }

    /// Keyword first, semantics only when the keywords came back weak — the
    /// same shape the knowledge base uses. Exact terms (error codes, package
    /// names) are what keyword matching is best at and what most of these
    /// lookups are; the embedding pass is for "what were we talking about",
    /// where the record says `[ERRO]` and the question says 报错.
    ///
    /// Every embedding step is best effort. The service being unreachable, or
    /// having produced no vectors yet, must never turn a working keyword search
    /// into a failure.
    pub async fn search_evicted_context_hybrid(
        &self,
        query: &str,
        limit: usize,
        start: Option<&str>,
        end: Option<&str>,
    ) -> Result<Value> {
        let mut base = self.search_evicted_context_readonly(query, limit, start, end)?;
        let strongest = base["results"]
            .as_array()
            .and_then(|hits| hits.first())
            .and_then(|hit| hit["score"].as_f64())
            .unwrap_or(0.0);
        if !self.semantic_enabled() || strongest >= SEMANTIC_SKIP_SCORE {
            return Ok(base);
        }
        let semantic = match self.semantic_evicted_hits(query, limit, start, end).await {
            Ok(hits) => hits,
            Err(error) => {
                tracing::debug!(error = %error, "evicted-context semantic pass unavailable");
                return Ok(base);
            }
        };
        if semantic.is_empty() {
            return Ok(base);
        }
        merge_evicted_hits(&mut base, semantic, limit);
        Ok(base)
    }

    /// Rows are embedded on demand rather than at eviction time: pop must not
    /// wait on a network round trip, and a record nobody ever searches for
    /// never costs an embedding. Each call tops up a bounded slice of the
    /// backlog, so coverage fills in over successive searches.
    pub(crate) async fn semantic_evicted_hits(
        &self,
        query: &str,
        limit: usize,
        start: Option<&str>,
        end: Option<&str>,
    ) -> Result<Vec<Value>> {
        let embedder = crate::embedding::Embedder::from_config(&self.app_config)
            .context("embedding model is not configured")?;
        let model = embedder.model_id().to_string();

        let corpus = self.semantic_corpus(start, end)?;
        let missing: Vec<(i64, String)> = {
            let conn = self.state_conn()?;
            let mut pending = Vec::new();
            for (id, content) in &corpus {
                if pending.len() >= SEMANTIC_EMBED_BATCH {
                    break;
                }
                let known: Option<String> = conn
                    .query_row(
                        "SELECT model FROM evicted_embeddings WHERE id = ?1 AND embedding IS NOT NULL",
                        params![id],
                        |row| row.get(0),
                    )
                    .ok();
                if known.as_deref() != Some(model.as_str()) {
                    pending.push((*id, content.clone()));
                }
            }
            pending
        };
        if !missing.is_empty() {
            let texts: Vec<String> = missing.iter().map(|(_, content)| content.clone()).collect();
            if let Ok(vectors) = embedder.embed(&texts).await {
                let conn = self.state_conn()?;
                for ((id, _), vector) in missing.iter().zip(vectors) {
                    conn.execute(
                        "INSERT INTO evicted_embeddings (id, model, embedding_json, embedding, created_at)
                         VALUES (?1, ?2, '', ?3, ?4)
                         ON CONFLICT (id) DO UPDATE SET
                            model = excluded.model,
                            embedding_json = excluded.embedding_json,
                            embedding = excluded.embedding,
                            created_at = excluded.created_at",
                        params![id, model, crate::embedding::vector_to_blob(&vector), now()],
                    )?;
                }
            }
        }

        let query_vector = embedder.embed_query(query).await?;
        let conn = self.state_conn()?;
        let mut hits = Vec::new();
        for (id, content) in &corpus {
            let stored: Option<Vec<u8>> = conn
                .query_row(
                    "SELECT embedding FROM evicted_embeddings WHERE id = ?1 AND model = ?2",
                    params![id, model],
                    |row| row.get(0),
                )
                .ok()
                .flatten();
            let Some(vector) = stored
                .as_deref()
                .and_then(crate::embedding::vector_from_blob)
            else {
                continue;
            };
            let score = cosine_similarity(&query_vector, &vector);
            if score < embedder.min_score() {
                continue;
            }
            hits.push(json!({
                "id": id,
                "score": score * SEMANTIC_SCORE_WEIGHT,
                "semantic": true,
                "snippet": truncate_chars(&compact_line(content), 400),
            }));
        }
        sort_json_hits(&mut hits);
        hits.truncate(limit);
        Ok(hits)
    }

    pub(crate) fn config_provider(&self, id: &str) -> Option<crate::config::ProviderConfig> {
        if id.is_empty() {
            return None;
        }
        self.app_config.provider(Some(id)).ok().cloned()
    }

    /// Newest rows only, and bounded: this pass answers "what were we talking
    /// about", which is a recency question, and an unbounded corpus would make
    /// every miss pay for the whole archive.
    pub(crate) fn semantic_corpus(
        &self,
        start: Option<&str>,
        end: Option<&str>,
    ) -> Result<Vec<(i64, String)>> {
        let conn = self.state_conn()?;
        let mut clauses = Vec::new();
        let mut params: Vec<String> = Vec::new();
        if let Some(principal) = self.access.principal_key() {
            params.push(principal.to_string());
            clauses.push(format!(
                "(visibility='public' OR (visibility='principal' AND owner_principal=?{}))",
                params.len()
            ));
        }
        if let Some(start) = start {
            params.push(start.to_string());
            clauses.push(format!("timestamp >= ?{}", params.len()));
        }
        if let Some(end) = end {
            params.push(end.to_string());
            clauses.push(format!("timestamp <= ?{}", params.len()));
        }
        let where_clause = if clauses.is_empty() {
            String::new()
        } else {
            format!("WHERE {}", clauses.join(" AND "))
        };
        let mut stmt = conn.prepare(&format!(
            "SELECT id, content FROM evicted_turns {where_clause}
              ORDER BY id DESC LIMIT {SEMANTIC_CORPUS_LIMIT}"
        ))?;
        let rows = stmt.query_map(rusqlite::params_from_iter(params.iter()), |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
        })?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    /// No switch of its own: an embedding model being configured is what makes
    /// the semantic pass available, and the keyword path stands on its own when
    /// it is not.
    pub(crate) fn semantic_enabled(&self) -> bool {
        self.app_config.embedding.is_configured()
    }

    pub(crate) fn search_evicted_context_existing(
        &self,
        query: &str,
        limit: usize,
    ) -> Result<Value> {
        self.search_evicted_context_filtered(query, limit, None, None)
    }

    /// `start`/`end` are RFC 3339 bounds on the stored timestamp. "What were we
    /// talking about this morning" is a question about *when*, and time is a
    /// far stronger signal there than any keyword — the log says `[ERRO]` where
    /// the question says 报错.
    pub(crate) fn search_evicted_context_filtered(
        &self,
        query: &str,
        limit: usize,
        start: Option<&str>,
        end: Option<&str>,
    ) -> Result<Value> {
        let tokens = query_tokens(query);
        let conn = self.state_conn()?;
        let mut clauses = Vec::new();
        let mut params: Vec<String> = Vec::new();
        if let Some(principal) = self.access.principal_key() {
            params.push(principal.to_string());
            clauses.push(format!(
                "(visibility='public' OR (visibility='principal' AND owner_principal=?{}))",
                params.len()
            ));
        }
        if let Some(start) = start {
            params.push(start.to_string());
            clauses.push(format!("timestamp >= ?{}", params.len()));
        }
        if let Some(end) = end {
            params.push(end.to_string());
            clauses.push(format!("timestamp <= ?{}", params.len()));
        }
        // The trigram index does the filtering, so the scan no longer has to be
        // capped at the newest 1000 rows — those beyond it used to be stored
        // forever and reachable never.
        if !tokens.is_empty() {
            // Trigram index: terms shorter than three characters cannot be
            // matched by it, so those fall through to the scoring pass below
            // rather than narrowing the candidate set.
            let indexed: Vec<String> = tokens
                .iter()
                .filter(|token| token.chars().count() >= 3)
                .cloned()
                .collect();
            if !indexed.is_empty() {
                params.push(build_evicted_fts_query(&indexed));
                clauses.push(format!(
                    "id IN (SELECT rowid FROM evicted_turns_fts WHERE evicted_turns_fts MATCH ?{})",
                    params.len()
                ));
            }
        }
        let where_clause = if clauses.is_empty() {
            String::new()
        } else {
            format!("WHERE {}", clauses.join(" AND "))
        };
        let mut stmt = conn.prepare(&format!(
            "SELECT id, timestamp, role, content, visibility,
                    owner_principal, owner_display_name
               FROM evicted_turns {where_clause}
              ORDER BY id DESC"
        ))?;
        let mut rows = stmt.query(rusqlite::params_from_iter(params.iter()))?;
        let normalized_query = compact_line(query).to_ascii_lowercase();
        let mut hits = Vec::new();
        while let Some(row) = rows.next()? {
            let id = row.get::<_, i64>(0)?;
            let timestamp = row.get::<_, String>(1)?;
            let role = row.get::<_, String>(2)?;
            let content = row.get::<_, String>(3)?;
            let visibility = row.get::<_, String>(4)?;
            let owner_principal = row.get::<_, String>(5)?;
            let owner_display_name = row.get::<_, String>(6)?;
            let score = score_text(&content, &normalized_query, &tokens);
            if score <= 0.0 {
                continue;
            }
            hits.push(json!({
                "id": id,
                "timestamp": timestamp,
                "role": role,
                "score": score,
                "visibility": visibility,
                "owner_principal": owner_principal,
                "owner_display_name": truncate_chars(&compact_line(&owner_display_name), 128),
                "snippet": snippet(&content, &tokens, self.kb_config.snippet_context_chars),
            }));
        }
        sort_json_hits(&mut hits);
        hits.truncate(limit.clamp(1, 50));
        Ok(json!({ "ok": true, "query": query, "results": hits }))
    }
}
