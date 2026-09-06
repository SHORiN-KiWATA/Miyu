//! 建索引：关键词与语义。
//!
//! 语义索引是**异步补上**的（`spawn_embedding_reindex`）：写文件不该等嵌入服务
//! 返回。所以查询时可能只有关键词索引，语义那半还在路上——这是可接受的降级，
//! 见 [`super::search`]。

use crate::tools::knowledge_base::*;

impl KnowledgeBase {
    pub async fn reindex_embeddings(&self, quiet: bool) -> Result<usize> {
        self.init()?;
        if !self.config.plugins.knowledge_base.embedding_enabled {
            if !quiet {
                println!("embedding is disabled");
            }
            return Ok(0);
        }
        let Some(embedder) = self.embedder() else {
            if !quiet {
                println!("embedding model is not configured or not installed; skipped");
            }
            return Ok(0);
        };
        let lock_path = self.root.join("embedding.lock");
        let lock = match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&lock_path)
        {
            Ok(lock) => lock,
            Err(_) => {
                if !quiet {
                    println!(
                        "embedding reindex already running; lock file: {}",
                        lock_path.display()
                    );
                    println!(
                        "if no miyu reindex process is running, remove the stale lock file and retry"
                    );
                }
                return Ok(0);
            }
        };
        drop(lock);
        let result = self.reindex_embeddings_inner(&embedder, quiet).await;
        let _ = std::fs::remove_file(lock_path);
        result
    }

    pub(in crate::tools::knowledge_base) fn refresh_semantic_after_write(
        &self,
        name: &str,
    ) -> Result<bool> {
        if !self.config.plugins.knowledge_base.embedding_enabled {
            return Ok(false);
        }
        self.semantic_conn()?.execute(
            "DELETE FROM semantic_chunks WHERE file_name=?1",
            params![name],
        )?;
        self.spawn_embedding_reindex()?;
        Ok(true)
    }

    /// 关键词检索：库里每个文件整读一遍再逐词扫。
    ///
    /// 这活儿是**无界**的（跟库大小成正比，实测约 7.3 ms/MB），原来直接在
    /// `async fn search_existing` 里同步跑——整段时间一个 tokio worker 被占死，
    /// 100MB 的库就是 700ms 的运行时冻结。挪进 `spawn_blocking`：算什么、
    /// 算多久一个字节都没变，只是不再占着异步线程。
    ///
    /// 循环只依赖 `list()` 的结果和三个配置数值，全部拷进闭包即可，不必让
    /// 闭包借到 `&self`。
    pub(in crate::tools::knowledge_base) async fn keyword_search(
        &self,
        query: &str,
        limit: usize,
    ) -> Result<Vec<SearchResult>> {
        let records = self.list()?;
        let proximity_window_chars = self.config.plugins.knowledge_base.proximity_window_chars;
        let snippet_context_chars = self.config.plugins.knowledge_base.snippet_context_chars;
        let query = query.to_string();
        tokio::task::spawn_blocking(move || {
            keyword_search_blocking(
                records,
                &query,
                limit,
                proximity_window_chars,
                snippet_context_chars,
            )
        })
        .await?
    }
}

pub(in crate::tools::knowledge_base) fn keyword_search_blocking(
    records: Vec<FileRecord>,
    query: &str,
    limit: usize,
    proximity_window_chars: usize,
    snippet_context_chars: usize,
) -> Result<Vec<SearchResult>> {
    {
        let tokens = query_tokens(query);
        let phrase = query.to_ascii_lowercase();
        let mut results = Vec::new();
        for record in records {
            let path = PathBuf::from(&record.path);
            let Ok(content) = std::fs::read_to_string(&path) else {
                continue;
            };
            let content_lower = content.to_ascii_lowercase();
            let name_lower = record.name.to_ascii_lowercase();
            let mut score = 0.0;
            let mut positions_by_token: HashMap<String, Vec<usize>> = HashMap::new();
            let mut matched = HashSet::new();
            if phrase.len() > 1 && content_lower.contains(&phrase) {
                score += 90.0;
                matched.insert(phrase.clone());
            }
            if phrase.len() > 1 && name_lower.contains(&phrase) {
                score += 140.0;
            }
            for token in &tokens {
                let positions = find_positions(&content_lower, token, 100);
                if !positions.is_empty() {
                    score += 20.0 + positions.len().min(10) as f32 * 2.0;
                    matched.insert(token.clone());
                    positions_by_token.insert(token.clone(), positions);
                }
                if name_lower.contains(token) {
                    score += 45.0;
                    matched.insert(token.clone());
                }
            }
            if !tokens.is_empty() {
                score += (matched.len() as f32 / tokens.len() as f32) * 55.0;
            }
            if let Some((start, end, coverage)) =
                best_window(&positions_by_token, &tokens, proximity_window_chars)
            {
                score += coverage * 120.0;
                let snippet = snippet_chars(&content, start, end, snippet_context_chars);
                results.push(SearchResult::new(
                    record.name,
                    score,
                    vec![snippet],
                    "keyword",
                ));
                continue;
            }
            if score > 0.0 {
                let snippets =
                    extract_snippets(&content, &content_lower, &tokens, snippet_context_chars);
                results.push(SearchResult::new(record.name, score, snippets, "keyword"));
            }
        }
        results.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        results.truncate(limit);
        Ok(results)
    }
}

impl KnowledgeBase {
    pub(in crate::tools::knowledge_base) async fn semantic_search(
        &self,
        query: &str,
    ) -> Result<Vec<SearchResult>> {
        let Some(embedder) = self.embedder() else {
            return Ok(Vec::new());
        };
        let query_embedding = embedder.embed_query(query).await?;
        let semantic = self.semantic_conn()?;
        // Only vectors from the current model are comparable; rows left by a
        // previous model wait for the reindex.
        let mut stmt = semantic.prepare(
            "SELECT file_name, start_char, end_char, text, embedding, embedding_json
             FROM semantic_chunks WHERE model = ?1",
        )?;
        let rows = stmt.query_map(params![embedder.model_id()], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, usize>(1)?,
                row.get::<_, usize>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, Option<Vec<u8>>>(4)?,
                row.get::<_, String>(5)?,
            ))
        })?;
        let mut results = Vec::new();
        for row in rows {
            let (file_name, _start, _end, text, blob, embedding_json) = row?;
            let embedding = match blob.as_deref().and_then(crate::embedding::vector_from_blob) {
                Some(vector) => vector,
                None => match serde_json::from_str::<Vec<f32>>(&embedding_json) {
                    Ok(vector) => vector,
                    Err(_) => continue,
                },
            };
            let score = cosine(&query_embedding, &embedding);
            if score < embedder.min_score() {
                continue;
            }
            results.push(SearchResult::new(
                file_name,
                score * 200.0,
                vec![compact_whitespace(&text)],
                "semantic",
            ));
        }
        results.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        results.truncate(self.config.plugins.knowledge_base.semantic_top_k);
        Ok(results)
    }

    /// Files whose chunks already carry vectors from this model and content
    /// hash are skipped, so a reindex after a model switch or a few edits only
    /// pays for what changed. Chunks are embedded one file at a time; a
    /// failure skips that file and keeps going.
    pub(in crate::tools::knowledge_base) async fn reindex_embeddings_inner(
        &self,
        embedder: &crate::embedding::Embedder,
        quiet: bool,
    ) -> Result<usize> {
        let files = self.list()?;
        let semantic = self.semantic_conn()?;
        init_semantic_db(&semantic)?;
        let model = embedder.model_id().to_string();
        let mut indexed = 0usize;
        for record in files {
            let current: i64 = semantic.query_row(
                "SELECT COUNT(*) FROM semantic_chunks WHERE file_name=?1 AND content_sha256=?2 AND model=?3 AND embedding IS NOT NULL",
                params![record.name, record.content_sha256, model],
                |row| row.get(0),
            )?;
            if current > 0 {
                continue;
            }
            let content = match std::fs::read_to_string(&record.path) {
                Ok(content) => content,
                Err(_) => continue,
            };
            let chunks = build_chunks(
                &content,
                self.config.plugins.knowledge_base.semantic_chunk_chars,
                self.config.plugins.knowledge_base.semantic_chunk_overlap,
            );
            let texts: Vec<String> = chunks.iter().map(|chunk| chunk.text.clone()).collect();
            let vectors = match embedder.embed(&texts).await {
                Ok(vectors) => vectors,
                Err(err) => {
                    if !quiet {
                        eprintln!("embedding failed for {}: {err:#}", record.name);
                    }
                    continue;
                }
            };
            semantic.execute(
                "DELETE FROM semantic_chunks WHERE file_name=?1",
                params![record.name],
            )?;
            for (chunk, vector) in chunks.iter().zip(vectors) {
                semantic.execute(
                    "INSERT INTO semantic_chunks (provider_id, model, file_name, content_sha256, chunk_index, start_char, end_char, text, embedding_json, embedding, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, '', ?9, ?10)",
                    params![embedder.describe(), model, record.name, record.content_sha256, chunk.index as i64, chunk.start as i64, chunk.end as i64, chunk.text, crate::embedding::vector_to_blob(&vector), now_secs()],
                )?;
                indexed += 1;
            }
        }
        // Vectors from other models are dead weight once the current model
        // has covered the library; the reindex is the natural sweep point.
        semantic.execute(
            "DELETE FROM semantic_chunks WHERE model != ?1",
            params![model],
        )?;
        if !quiet {
            println!("indexed semantic chunks: {indexed}");
        }
        Ok(indexed)
    }

    pub(in crate::tools) fn spawn_embedding_reindex(&self) -> Result<()> {
        if !self.config.plugins.knowledge_base.embedding_enabled || self.embedder().is_none() {
            return Ok(());
        }
        // 测试里不起后台重建:测试二进制"再执行自己"会变成 fork 炸弹(见
        // `paths::miyu_executable` 的说明,那里也有一道闸)。
        if cfg!(test) {
            return Ok(());
        }
        let exe = crate::paths::miyu_executable()?;
        Command::new(exe)
            .args(["kb", "embed", "reindex", "--quiet"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?;
        Ok(())
    }

    /// The knowledge base has its own switch on top of the global one: a
    /// large library makes the semantic pass expensive to (re)build.
    pub(in crate::tools::knowledge_base) fn embedder(&self) -> Option<crate::embedding::Embedder> {
        if !self.config.plugins.knowledge_base.embedding_enabled {
            return None;
        }
        crate::embedding::Embedder::from_config(&self.config)
    }

    pub(in crate::tools::knowledge_base) fn meta_conn(&self) -> Result<Connection> {
        if let Some(parent) = self.meta_db.parent() {
            std::fs::create_dir_all(parent)?;
        }
        Ok(Connection::open(&self.meta_db)?)
    }

    pub(in crate::tools::knowledge_base) fn semantic_conn(&self) -> Result<Connection> {
        if let Some(parent) = self.semantic_db.parent() {
            std::fs::create_dir_all(parent)?;
        }
        Ok(Connection::open(&self.semantic_db)?)
    }
}
