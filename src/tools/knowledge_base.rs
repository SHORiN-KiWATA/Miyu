use super::{ToolRegistry, ToolSpec};
use crate::config::{AppConfig, KnowledgeBasePluginConfig, ProviderConfig};
use crate::paths::MiyuPaths;
use anyhow::{bail, Context, Result};
use chrono::Local;
use reqwest::Client;
use rusqlite::{params, Connection};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::path::{Component, Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::process::Command;

pub fn register(registry: &mut ToolRegistry, config: AppConfig, paths: MiyuPaths) {
    register_readonly(registry, config.clone(), paths.clone());
    if config.plugins.knowledge_base.upload_tool_enabled {
        let upload_config = config.clone();
        let upload_paths = paths.clone();
        registry.register(ToolSpec::new(
                "upload_text_to_knowledge_base",
            "Create a new knowledge-base file or replace an entire existing file. For updating part of an existing file, first search/read it and prefer edit_knowledge_base_file. Never use this for skills, memory, persona, identity, or configuration.",
            json!({
                "type": "object",
                "properties": {
                    "content": { "type": "string", "description": "Text content to save." },
                    "title": { "type": "string", "description": "Optional title used for markdown heading and default file name." },
                    "file_name": { "type": "string", "description": "Optional knowledge base relative path." }
                },
                "required": ["content"],
                "additionalProperties": false
            }),
            move |args| {
                let config = upload_config.clone();
                let paths = upload_paths.clone();
                async move { tool_upload(args, config, paths).await }
            },
        ).writes());
        let edit_config = config.clone();
        let edit_paths = paths.clone();
        registry.register(ToolSpec::new(
            "edit_knowledge_base_file",
            "Edit an existing knowledge-base file by replacing an inclusive 1-based line range. Use after search_knowledge_base/read_knowledge_base_file identifies the exact file and line numbers. This updates metadata and refreshes semantic indexing when embeddings are enabled.",
            json!({
                "type": "object",
                "properties": {
                    "file_name": { "type": "string", "description": "Knowledge base relative path to edit." },
                    "start_line": { "type": "integer", "description": "1-based first line to replace." },
                    "end_line": { "type": "integer", "description": "1-based last line to replace, inclusive." },
                    "replacement": { "type": "string", "description": "Replacement text. May contain multiple lines. Empty text deletes the line range." }
                },
                "required": ["file_name", "start_line", "end_line", "replacement"],
                "additionalProperties": false
            }),
            move |args| {
                let config = edit_config.clone();
                let paths = edit_paths.clone();
                async move { tool_edit(args, config, paths).await }
            },
        ).writes());
        let remove_config = config.clone();
        let remove_paths = paths.clone();
        registry.register(ToolSpec::new(
            "remove_knowledge_base_file",
            "Remove a knowledge-base file by relative path. Use only after the user asks to delete a knowledge-base entry or confirms the exact file. This also removes its metadata and semantic chunks.",
            json!({
                "type": "object",
                "properties": {
                    "file_name": { "type": "string", "description": "Knowledge base relative path to remove." }
                },
                "required": ["file_name"],
                "additionalProperties": false
            }),
            move |args| {
                let config = remove_config.clone();
                let paths = remove_paths.clone();
                async move { tool_remove(args, config, paths).await }
            },
        ).writes());
    }
}

pub fn register_readonly(registry: &mut ToolRegistry, config: AppConfig, paths: MiyuPaths) {
    registry.register(ToolSpec::new(
        "search_knowledge_base",
        "Search the local knowledge base content. Returns file paths and original text snippets. Use read_knowledge_base_file if snippets are insufficient. Mention paths only when useful or when the user asks.",
        json!({
            "type": "object",
            "properties": {
                "query": { "type": "string", "description": "Search keywords or user question." },
                "max_results": { "type": "integer", "description": "Optional result limit." }
            },
            "required": ["query"],
            "additionalProperties": false
        }),
        {
            let config = config.clone();
            let paths = paths.clone();
            move |args| {
                let config = config.clone();
                let paths = paths.clone();
                async move { tool_search_readonly(args, config, paths).await }
            }
        },
    ));
    registry.register(ToolSpec::new(
        "search_knowledge_base_by_name",
        "Find knowledge base files by file name, directory, extension, or path fragment. Returns relative paths for read_knowledge_base_file. Mention paths only when useful or when the user asks.",
        json!({
            "type": "object",
            "properties": {
                "file_name_query": { "type": "string", "description": "File name, directory, extension, or path fragment." },
                "max_results": { "type": "integer", "description": "Optional result limit." }
            },
            "required": ["file_name_query"],
            "additionalProperties": false
        }),
        {
            let config = config.clone();
            let paths = paths.clone();
            move |args| {
                let config = config.clone();
                let paths = paths.clone();
                async move { tool_find_readonly(args, config, paths).await }
            }
        },
    ));
    registry.register(ToolSpec::new(
        "read_knowledge_base_file",
        "Read a knowledge base file by relative path with line pagination. Prefer paths returned by search_knowledge_base or search_knowledge_base_by_name. Summarize the relevant content without exposing raw tool JSON.",
        json!({
            "type": "object",
            "properties": {
                "file_name": { "type": "string", "description": "Knowledge base relative path." },
                "start_line": { "type": "integer", "description": "1-based start line." },
                "max_lines": { "type": "integer", "description": "Optional line limit." }
            },
            "required": ["file_name"],
            "additionalProperties": false
        }),
        {
            let config = config.clone();
            let paths = paths.clone();
            move |args| {
                let config = config.clone();
                let paths = paths.clone();
                async move { tool_read_readonly(args, config, paths).await }
            }
        },
    ));
}

pub struct KnowledgeBase {
    config: AppConfig,
    root: PathBuf,
    files_dir: PathBuf,
    meta_db: PathBuf,
    semantic_db: PathBuf,
}

impl KnowledgeBase {
    pub fn new(config: AppConfig, paths: MiyuPaths) -> Result<Self> {
        let root = kb_root(&config.plugins.knowledge_base, &paths);
        let files_dir = root.join("files");
        let meta_db = root.join("kb_meta.db");
        let semantic_db = root.join("semantic_index.db");
        Ok(Self {
            config,
            root,
            files_dir,
            meta_db,
            semantic_db,
        })
    }

    pub fn init(&self) -> Result<()> {
        std::fs::create_dir_all(&self.files_dir)?;
        let conn = self.meta_conn()?;
        init_meta_db(&conn)?;
        let semantic = self.semantic_conn()?;
        init_semantic_db(&semantic)?;
        Ok(())
    }

    fn readonly_available(&self) -> bool {
        self.root.is_dir() && self.files_dir.is_dir() && self.meta_db.is_file()
    }

    pub async fn add_path(&self, source: &Path) -> Result<Vec<String>> {
        self.init()?;
        let mut added = Vec::new();
        if source.is_dir() {
            let root_name = source
                .file_name()
                .and_then(|name| name.to_str())
                .context("source directory has no valid directory name")?;
            for file in collect_files(source)? {
                let rel = file.strip_prefix(source).unwrap_or(&file);
                let name = normalize_relative_path(&format!(
                    "{}/{}",
                    root_name,
                    rel.display().to_string().replace('\\', "/")
                ))?;
                if let Ok(name) = self.import_file(&file, &name) {
                    added.push(name);
                }
            }
        } else {
            let name = normalize_relative_path(
                source
                    .file_name()
                    .and_then(|name| name.to_str())
                    .context("source file has no valid file name")?,
            )?;
            added.push(self.import_file(source, &name)?);
        }
        self.spawn_embedding_reindex()?;
        Ok(added)
    }

    pub fn replace_default_files(&self, source: &Path) -> Result<Vec<String>> {
        self.init()?;
        self.remove_prefix("default-kb/")?;
        let mut added = Vec::new();
        for file in collect_files(source)? {
            let rel = file.strip_prefix(source).unwrap_or(&file);
            let rel = rel.display().to_string().replace('\\', "/");
            let name = normalize_relative_path(&format!("default-kb/{rel}"))?;
            if let Ok(name) = self.import_file(&file, &name) {
                added.push(name);
            }
        }
        self.spawn_embedding_reindex()?;
        Ok(added)
    }

    pub fn list(&self) -> Result<Vec<FileRecord>> {
        self.init()?;
        self.list_existing()
    }

    fn list_existing(&self) -> Result<Vec<FileRecord>> {
        let conn = self.meta_conn()?;
        let mut stmt =
            conn.prepare("SELECT name, path, size_bytes, content_sha256 FROM files ORDER BY name")?;
        let rows = stmt.query_map([], |row| {
            Ok(FileRecord {
                name: row.get(0)?,
                path: row.get(1)?,
                size_bytes: row.get(2)?,
                content_sha256: row.get(3)?,
            })
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    pub async fn search(&self, query: &str, max_results: Option<usize>) -> Result<Value> {
        self.init()?;
        self.search_existing(query, max_results, true).await
    }

    pub async fn search_readonly(&self, query: &str, max_results: Option<usize>) -> Result<Value> {
        if !self.readonly_available() {
            return Ok(
                json!({"ok": true, "query": query, "total_matches": 0, "semantic_used": false, "results": []}),
            );
        }
        self.search_existing(query, max_results, self.semantic_db.is_file())
            .await
    }

    async fn search_existing(
        &self,
        query: &str,
        max_results: Option<usize>,
        allow_semantic: bool,
    ) -> Result<Value> {
        let limit = max_results
            .unwrap_or(self.config.plugins.knowledge_base.max_search_results)
            .clamp(1, 50);
        let mut results = self.keyword_search(query, limit)?;
        let strongest = results.first().map(|item| item.score).unwrap_or(0.0);
        let mut semantic_used = false;
        if allow_semantic
            && self.config.plugins.knowledge_base.embedding_enabled
            && strongest
                < self
                    .config
                    .plugins
                    .knowledge_base
                    .keyword_strong_score_threshold
        {
            if let Ok(semantic) = self.semantic_search(query).await {
                semantic_used = !semantic.is_empty();
                merge_results(&mut results, semantic, limit);
            }
        }
        Ok(json!({
            "ok": true,
            "query": query,
            "total_matches": results.len(),
            "semantic_used": semantic_used,
            "results": results.iter().map(SearchResult::to_json).collect::<Vec<_>>(),
        }))
    }

    pub fn find_by_name(&self, query: &str, max_results: Option<usize>) -> Result<Value> {
        self.init()?;
        self.find_by_name_existing(query, max_results)
    }

    pub fn find_by_name_readonly(&self, query: &str, max_results: Option<usize>) -> Result<Value> {
        if !self.readonly_available() {
            return Ok(json!({"ok": true, "query": query, "total_matches": 0, "results": []}));
        }
        self.find_by_name_existing(query, max_results)
    }

    fn find_by_name_existing(&self, query: &str, max_results: Option<usize>) -> Result<Value> {
        let limit = max_results
            .unwrap_or(self.config.plugins.knowledge_base.max_search_results)
            .clamp(1, 50);
        let mut results = Vec::new();
        for record in self.list()? {
            let (score, reason) = score_file_name(query, &record.name);
            if score <= 0.0 {
                continue;
            }
            results.push(json!({
                "path": record.name,
                "name": file_name(&record.name),
                "directory": directory_name(&record.name),
                "score": score,
                "match_reason": reason,
                "size_kb": (record.size_bytes as f64 / 1024.0 * 10.0).round() / 10.0,
            }));
        }
        results.sort_by(|a, b| {
            b.get("score")
                .and_then(Value::as_f64)
                .unwrap_or_default()
                .partial_cmp(&a.get("score").and_then(Value::as_f64).unwrap_or_default())
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        results.truncate(limit);
        Ok(json!({
            "ok": true,
            "query": query,
            "total_matches": results.len(),
            "results": results,
        }))
    }

    pub fn read_file(
        &self,
        name: &str,
        start_line: usize,
        max_lines: Option<usize>,
    ) -> Result<String> {
        self.init()?;
        self.read_file_existing(name, start_line, max_lines, true)
    }

    pub fn read_file_readonly(
        &self,
        name: &str,
        start_line: usize,
        max_lines: Option<usize>,
    ) -> Result<String> {
        if !self.readonly_available() {
            bail!("knowledge base is not initialized")
        }
        self.read_file_existing(name, start_line, max_lines, false)
    }

    fn read_file_existing(
        &self,
        name: &str,
        start_line: usize,
        max_lines: Option<usize>,
        create_parent: bool,
    ) -> Result<String> {
        let rel = normalize_relative_path(name)?;
        let path = if create_parent {
            self.safe_file_path(&rel)?
        } else {
            self.existing_file_path(&rel)?
        };
        if !path.exists() {
            bail!("knowledge base file not found: {rel}")
        }
        let content = std::fs::read_to_string(&path)?;
        let start = start_line.max(1);
        let max_lines = max_lines
            .unwrap_or(self.config.plugins.knowledge_base.max_read_lines)
            .clamp(1, 5000);
        let mut total = 0usize;
        let mut selected = Vec::new();
        for (index, line) in content.lines().enumerate() {
            let line_no = index + 1;
            total = line_no;
            if line_no >= start && selected.len() < max_lines {
                selected.push(line);
            }
        }
        if start > total.max(1) {
            return Ok(format!(
                "=== {rel} | start_line {start} out of range / {total} lines ==="
            ));
        }
        let end = (start + max_lines - 1).min(total);
        let mut output = format!("=== {rel} | lines {start}-{end} / {total} ===\n");
        output.push_str(&selected.join("\n"));
        if end < total {
            output.push_str(&format!(
                "\n\n... {remaining} more lines; continue with start_line={next}",
                remaining = total - end,
                next = end + 1
            ));
        }
        Ok(output)
    }

    pub fn remove(&self, name: &str) -> Result<()> {
        self.init()?;
        let rel = normalize_relative_path(name)?;
        let path = self.safe_file_path(&rel)?;
        if path.exists() {
            std::fs::remove_file(&path)?;
        }
        let conn = self.meta_conn()?;
        conn.execute("DELETE FROM files WHERE name=?1", params![rel])?;
        let semantic = self.semantic_conn()?;
        semantic.execute(
            "DELETE FROM semantic_chunks WHERE file_name=?1",
            params![rel],
        )?;
        Ok(())
    }

    pub fn edit_lines(
        &self,
        name: &str,
        start_line: usize,
        end_line: usize,
        replacement: &str,
    ) -> Result<EditResult> {
        self.init()?;
        let rel = normalize_relative_path(name)?;
        if start_line == 0 || end_line == 0 {
            bail!("line numbers must be 1-based")
        }
        if start_line > end_line {
            bail!("start_line must be less than or equal to end_line")
        }
        let path = self.existing_file_path(&rel)?;
        if !path.exists() {
            bail!("knowledge base file not found: {rel}")
        }
        let original = std::fs::read_to_string(&path)?;
        let had_trailing_newline = original.ends_with('\n');
        let mut lines = original.lines().map(str::to_string).collect::<Vec<_>>();
        let total_lines = lines.len();
        if start_line > total_lines || end_line > total_lines {
            bail!("line range {start_line}-{end_line} out of range: {total_lines} lines")
        }
        let replacement = replacement.replace("\r\n", "\n").replace('\r', "\n");
        let replacement_lines = if replacement.is_empty() {
            Vec::new()
        } else {
            replacement.lines().map(str::to_string).collect::<Vec<_>>()
        };
        lines.splice(start_line - 1..end_line, replacement_lines);
        let mut updated = lines.join("\n");
        if had_trailing_newline && !updated.is_empty() {
            updated.push('\n');
        }
        let temp = tempfile::NamedTempFile::new()?;
        std::fs::write(temp.path(), updated.as_bytes())?;
        self.import_file(temp.path(), &rel)?;
        let semantic_refreshed = self.refresh_semantic_after_write(&rel)?;
        Ok(EditResult {
            path: rel,
            old_line_count: total_lines,
            new_line_count: lines.len(),
            semantic_refreshed,
        })
    }

    fn remove_prefix(&self, prefix: &str) -> Result<()> {
        let conn = self.meta_conn()?;
        let mut stmt = conn.prepare("SELECT name FROM files WHERE name LIKE ?1")?;
        let names = stmt
            .query_map(params![format!("{prefix}%")], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        for name in names {
            let path = self.safe_file_path(&name)?;
            if path.exists() {
                std::fs::remove_file(path)?;
            }
            conn.execute("DELETE FROM files WHERE name=?1", params![name])?;
            self.semantic_conn()?.execute(
                "DELETE FROM semantic_chunks WHERE file_name=?1",
                params![name],
            )?;
        }
        Ok(())
    }

    pub async fn reindex_embeddings(&self, quiet: bool) -> Result<usize> {
        self.init()?;
        if !self.config.plugins.knowledge_base.embedding_enabled {
            if !quiet {
                println!("embedding is disabled");
            }
            return Ok(0);
        }
        let Some((provider, model)) = self.embedding_provider()? else {
            if !quiet {
                println!("embedding provider/model is not configured; skipped");
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
        let result = self
            .reindex_embeddings_inner(&provider, &model, quiet)
            .await;
        let _ = std::fs::remove_file(lock_path);
        result
    }

    pub fn stats(&self) -> Result<Value> {
        self.init()?;
        let files = self.list()?;
        let semantic = self.semantic_conn()?;
        let chunks: i64 =
            semantic.query_row("SELECT COUNT(*) FROM semantic_chunks", [], |row| row.get(0))?;
        Ok(json!({
            "ok": true,
            "root": self.root.display().to_string(),
            "files_dir": self.files_dir.display().to_string(),
            "files": files.len(),
            "total_size_kb": (files.iter().map(|file| file.size_bytes).sum::<i64>() as f64 / 1024.0 * 10.0).round() / 10.0,
            "semantic_chunks": chunks,
            "embedding_enabled": self.config.plugins.knowledge_base.embedding_enabled,
            "embedding_provider_id": self.config.plugins.knowledge_base.embedding_provider_id,
            "embedding_model": self.config.plugins.knowledge_base.embedding_model,
        }))
    }

    fn import_file(&self, source: &Path, name: &str) -> Result<String> {
        let bytes = std::fs::read(source)?;
        self.validate_file(name, &bytes)?;
        let dest = self.safe_file_path(name)?;
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&dest, &bytes)?;
        let hash = sha256_hex(&bytes);
        let mtime = unix_time(std::fs::metadata(&dest)?.modified()?);
        let conn = self.meta_conn()?;
        init_meta_db(&conn)?;
        conn.execute(
            "INSERT INTO files (name, path, size_bytes, mtime, content_sha256, updated_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6) ON CONFLICT(name) DO UPDATE SET path=excluded.path, size_bytes=excluded.size_bytes, mtime=excluded.mtime, content_sha256=excluded.content_sha256, updated_at=excluded.updated_at",
            params![name, dest.display().to_string(), bytes.len() as i64, mtime, hash, now_secs()],
        )?;
        Ok(name.to_string())
    }

    fn refresh_semantic_after_write(&self, name: &str) -> Result<bool> {
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

    fn keyword_search(&self, query: &str, limit: usize) -> Result<Vec<SearchResult>> {
        let tokens = query_tokens(query);
        let phrase = query.to_ascii_lowercase();
        let mut results = Vec::new();
        for record in self.list()? {
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
            if let Some((start, end, coverage)) = best_window(
                &positions_by_token,
                &tokens,
                self.config.plugins.knowledge_base.proximity_window_chars,
            ) {
                score += coverage * 120.0;
                let snippet = snippet_chars(
                    &content,
                    start,
                    end,
                    self.config.plugins.knowledge_base.snippet_context_chars,
                );
                results.push(SearchResult::new(
                    record.name,
                    score,
                    vec![snippet],
                    "keyword",
                ));
                continue;
            }
            if score > 0.0 {
                let snippets = extract_snippets(
                    &content,
                    &content_lower,
                    &tokens,
                    self.config.plugins.knowledge_base.snippet_context_chars,
                );
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

    async fn semantic_search(&self, query: &str) -> Result<Vec<SearchResult>> {
        let Some((provider, model)) = self.embedding_provider()? else {
            return Ok(Vec::new());
        };
        let query_embedding = embed_text(&self.config, &provider, &model, query).await?;
        let semantic = self.semantic_conn()?;
        let mut stmt = semantic.prepare(
            "SELECT file_name, start_char, end_char, text, embedding_json FROM semantic_chunks",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, usize>(1)?,
                row.get::<_, usize>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
            ))
        })?;
        let mut results = Vec::new();
        for row in rows {
            let (file_name, _start, _end, text, embedding_json) = row?;
            let Ok(embedding) = serde_json::from_str::<Vec<f32>>(&embedding_json) else {
                continue;
            };
            let score = cosine(&query_embedding, &embedding);
            if score < self.config.plugins.knowledge_base.semantic_min_score {
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

    async fn reindex_embeddings_inner(
        &self,
        provider: &ProviderConfig,
        model: &str,
        quiet: bool,
    ) -> Result<usize> {
        let files = self.list()?;
        let semantic = self.semantic_conn()?;
        init_semantic_db(&semantic)?;
        let mut indexed = 0usize;
        for record in files {
            let content = match std::fs::read_to_string(&record.path) {
                Ok(content) => content,
                Err(_) => continue,
            };
            let chunks = build_chunks(
                &content,
                self.config.plugins.knowledge_base.semantic_chunk_chars,
                self.config.plugins.knowledge_base.semantic_chunk_overlap,
            );
            semantic.execute(
                "DELETE FROM semantic_chunks WHERE file_name=?1",
                params![record.name],
            )?;
            for chunk in chunks {
                let embedding = match embed_text(&self.config, provider, model, &chunk.text).await {
                    Ok(value) => value,
                    Err(err) => {
                        if !quiet {
                            eprintln!(
                                "embedding failed for {} chunk {}: {err}",
                                record.name, chunk.index
                            );
                        }
                        continue;
                    }
                };
                semantic.execute(
                    "INSERT INTO semantic_chunks (provider_id, model, file_name, content_sha256, chunk_index, start_char, end_char, text, embedding_json, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                    params![provider.id, model, record.name, record.content_sha256, chunk.index as i64, chunk.start as i64, chunk.end as i64, chunk.text, serde_json::to_string(&embedding)?, now_secs()],
                )?;
                indexed += 1;
            }
        }
        if !quiet {
            println!("indexed semantic chunks: {indexed}");
        }
        Ok(indexed)
    }

    fn spawn_embedding_reindex(&self) -> Result<()> {
        if !self.config.plugins.knowledge_base.embedding_enabled {
            return Ok(());
        }
        if self
            .config
            .plugins
            .knowledge_base
            .embedding_provider_id
            .trim()
            .is_empty()
            || self
                .config
                .plugins
                .knowledge_base
                .embedding_model
                .trim()
                .is_empty()
        {
            return Ok(());
        }
        let exe = std::env::current_exe()?;
        Command::new(exe)
            .args(["kb", "embed", "reindex", "--quiet"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?;
        Ok(())
    }

    fn validate_file(&self, name: &str, bytes: &[u8]) -> Result<()> {
        if bytes.is_empty() {
            bail!("file is empty")
        }
        if bytes.len() > self.config.plugins.knowledge_base.max_file_size_kb * 1024 {
            bail!("file too large: {} bytes", bytes.len())
        }
        std::str::from_utf8(bytes).context("file is not valid UTF-8 text")?;
        let file_name = file_name(name).to_ascii_lowercase();
        let ext = Path::new(&file_name)
            .extension()
            .and_then(|ext| ext.to_str())
            .map(|ext| format!(".{ext}"));
        let allowed_ext = split_csv(&self.config.plugins.knowledge_base.allowed_extensions);
        let allowed_names = split_csv(&self.config.plugins.knowledge_base.allowed_filenames);
        if ext.as_ref().is_some_and(|ext| allowed_ext.contains(ext))
            || allowed_names.contains(&file_name)
        {
            Ok(())
        } else {
            bail!("unsupported file type or name: {file_name}")
        }
    }

    fn embedding_provider(&self) -> Result<Option<(ProviderConfig, String)>> {
        let kb = &self.config.plugins.knowledge_base;
        if kb.embedding_provider_id.trim().is_empty() || kb.embedding_model.trim().is_empty() {
            return Ok(None);
        }
        let mut provider = self
            .config
            .provider(Some(kb.embedding_provider_id.trim()))?
            .clone();
        provider.default_model = kb.embedding_model.trim().to_string();
        Ok(Some((provider, kb.embedding_model.trim().to_string())))
    }

    fn meta_conn(&self) -> Result<Connection> {
        if let Some(parent) = self.meta_db.parent() {
            std::fs::create_dir_all(parent)?;
        }
        Ok(Connection::open(&self.meta_db)?)
    }

    fn semantic_conn(&self) -> Result<Connection> {
        if let Some(parent) = self.semantic_db.parent() {
            std::fs::create_dir_all(parent)?;
        }
        Ok(Connection::open(&self.semantic_db)?)
    }

    fn safe_file_path(&self, rel: &str) -> Result<PathBuf> {
        let rel = normalize_relative_path(rel)?;
        let path = self.files_dir.join(&rel);
        std::fs::create_dir_all(&self.files_dir)?;
        let base = self.files_dir.canonicalize()?;
        let parent = path.parent().unwrap_or(&self.files_dir);
        std::fs::create_dir_all(parent)?;
        let resolved_parent = parent.canonicalize()?;
        if !resolved_parent.starts_with(&base) {
            bail!("knowledge base path escapes files dir")
        }
        Ok(path)
    }

    fn existing_file_path(&self, rel: &str) -> Result<PathBuf> {
        let rel = normalize_relative_path(rel)?;
        let path = self.files_dir.join(&rel);
        let base = self
            .files_dir
            .canonicalize()
            .unwrap_or_else(|_| self.files_dir.clone());
        let parent = path.parent().unwrap_or(&self.files_dir);
        let resolved_parent = parent.canonicalize()?;
        if !resolved_parent.starts_with(&base) {
            bail!("knowledge base path escapes files dir")
        }
        Ok(path)
    }
}

#[derive(Clone)]
pub struct FileRecord {
    pub name: String,
    path: String,
    pub size_bytes: i64,
    content_sha256: String,
}

#[derive(Debug)]
pub struct EditResult {
    path: String,
    old_line_count: usize,
    new_line_count: usize,
    semantic_refreshed: bool,
}

struct SearchResult {
    path: String,
    score: f32,
    snippets: Vec<String>,
    source: &'static str,
}

impl SearchResult {
    fn new(path: String, score: f32, snippets: Vec<String>, source: &'static str) -> Self {
        Self {
            path,
            score,
            snippets,
            source,
        }
    }

    fn to_json(&self) -> Value {
        json!({
            "path": self.path,
            "name": file_name(&self.path),
            "directory": directory_name(&self.path),
            "score": (self.score * 10.0).round() / 10.0,
            "source": self.source,
            "snippets": self.snippets,
        })
    }
}

struct Chunk {
    index: usize,
    start: usize,
    end: usize,
    text: String,
}

async fn tool_search_readonly(args: Value, config: AppConfig, paths: MiyuPaths) -> Result<String> {
    ensure_enabled(&config)?;
    let query = args
        .get("query")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim();
    if query.is_empty() {
        bail!("query is required")
    }
    let max_results = args
        .get("max_results")
        .and_then(Value::as_u64)
        .map(|value| value as usize);
    Ok(KnowledgeBase::new(config, paths)?
        .search_readonly(query, max_results)
        .await?
        .to_string())
}

async fn tool_find_readonly(args: Value, config: AppConfig, paths: MiyuPaths) -> Result<String> {
    ensure_enabled(&config)?;
    let query = args
        .get("file_name_query")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim();
    if query.is_empty() {
        bail!("file_name_query is required")
    }
    let max_results = args
        .get("max_results")
        .and_then(Value::as_u64)
        .map(|value| value as usize);
    Ok(KnowledgeBase::new(config, paths)?
        .find_by_name_readonly(query, max_results)?
        .to_string())
}

async fn tool_read_readonly(args: Value, config: AppConfig, paths: MiyuPaths) -> Result<String> {
    ensure_enabled(&config)?;
    let name = args
        .get("file_name")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim();
    if name.is_empty() {
        bail!("file_name is required")
    }
    let start_line = args.get("start_line").and_then(Value::as_u64).unwrap_or(1) as usize;
    let max_lines = args
        .get("max_lines")
        .and_then(Value::as_u64)
        .map(|value| value as usize);
    KnowledgeBase::new(config, paths)?.read_file_readonly(name, start_line, max_lines)
}

async fn tool_upload(args: Value, config: AppConfig, paths: MiyuPaths) -> Result<String> {
    ensure_enabled(&config)?;
    if !config.plugins.knowledge_base.upload_tool_enabled {
        bail!("knowledge base upload tool is disabled")
    }
    let content = args
        .get("content")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim();
    if content.is_empty() {
        bail!("content is required")
    }
    let title = args
        .get("title")
        .and_then(Value::as_str)
        .unwrap_or("knowledge note")
        .trim();
    let file_name = args
        .get("file_name")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim();
    reject_non_kb_upload(content, title, file_name)?;
    let rel = if file_name.is_empty() {
        format!(
            "chat_uploads/{}/{}.md",
            Local::now().format("%Y-%m-%d"),
            slug(title)
        )
    } else {
        normalize_relative_path(file_name)?
    };
    let body = format!(
        "# {}\n\n> 来源：用户要求保存到本地知识库\n> 上传时间：{}\n\n{}\n",
        if title.is_empty() {
            Path::new(&rel)
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("knowledge note")
        } else {
            title
        },
        Local::now().format("%Y-%m-%d %H:%M:%S"),
        content
    );
    let kb = KnowledgeBase::new(config, paths)?;
    kb.init()?;
    let temp = tempfile::NamedTempFile::new()?;
    std::fs::write(temp.path(), body.as_bytes())?;
    let saved = kb.import_file(temp.path(), &rel)?;
    kb.spawn_embedding_reindex()?;
    Ok(json!({
        "ok": true,
        "path": saved,
    })
    .to_string())
}

async fn tool_edit(args: Value, config: AppConfig, paths: MiyuPaths) -> Result<String> {
    ensure_enabled(&config)?;
    let name = args
        .get("file_name")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim();
    if name.is_empty() {
        bail!("file_name is required")
    }
    let start_line = args
        .get("start_line")
        .and_then(Value::as_u64)
        .context("start_line is required")? as usize;
    let end_line = args
        .get("end_line")
        .and_then(Value::as_u64)
        .context("end_line is required")? as usize;
    let replacement = args
        .get("replacement")
        .and_then(Value::as_str)
        .context("replacement is required")?;
    let result =
        KnowledgeBase::new(config, paths)?.edit_lines(name, start_line, end_line, replacement)?;
    Ok(json!({
        "ok": true,
        "path": result.path,
        "old_line_count": result.old_line_count,
        "new_line_count": result.new_line_count,
        "semantic_refreshed": result.semantic_refreshed,
        "warning": if name.starts_with("default-kb/") { Some("default-kb files may be overwritten by miyu update-default-kb") } else { None::<&str> },
    })
    .to_string())
}

async fn tool_remove(args: Value, config: AppConfig, paths: MiyuPaths) -> Result<String> {
    ensure_enabled(&config)?;
    let name = args
        .get("file_name")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim();
    if name.is_empty() {
        bail!("file_name is required")
    }
    let rel = normalize_relative_path(name)?;
    KnowledgeBase::new(config, paths)?.remove(&rel)?;
    Ok(json!({
        "ok": true,
        "path": rel,
        "warning": if name.starts_with("default-kb/") { Some("default-kb files may be restored by miyu update-default-kb") } else { None::<&str> },
    })
    .to_string())
}

fn reject_non_kb_upload(content: &str, title: &str, file_name: &str) -> Result<()> {
    let text = format!("{content}\n{title}\n{file_name}").to_ascii_lowercase();
    let forbidden = [
        "skill", "skills/", "skll", "记忆", "memory", "persona", "identity", "prompt", "配置",
        "config",
    ];
    if forbidden.iter().any(|needle| text.contains(needle)) {
        bail!("this content looks like a skill, memory, prompt, identity, or config request; do not upload it to the knowledge base")
    }
    Ok(())
}

pub async fn embed_text(
    config: &AppConfig,
    provider: &ProviderConfig,
    model: &str,
    text: &str,
) -> Result<Vec<f32>> {
    let api_key = provider.api_key.as_deref().unwrap_or_default().trim();
    if api_key.is_empty() {
        bail!("embedding provider {} has no api_key", provider.id)
    }
    let client = Client::builder()
        .timeout(Duration::from_secs(
            config.plugins.knowledge_base.embedding_timeout_seconds,
        ))
        .build()?;
    let url = format!("{}/embeddings", provider.base_url.trim_end_matches('/'));
    let response = client
        .post(&url)
        .bearer_auth(api_key)
        .json(&json!({ "model": model, "input": text }))
        .send()
        .await?;
    let status = response.status();
    if !status.is_success() {
        let text = response.text().await.unwrap_or_default();
        bail!(
            "embedding API error at {url} ({status}): {}",
            compact_whitespace(&text)
        );
    }
    let data: Value = response.json().await?;
    let embedding = data
        .get("data")
        .and_then(Value::as_array)
        .and_then(|items| items.first())
        .and_then(|item| item.get("embedding"))
        .and_then(Value::as_array)
        .context("embedding response missing data[0].embedding")?;
    Ok(embedding
        .iter()
        .filter_map(Value::as_f64)
        .map(|value| value as f32)
        .collect())
}

fn ensure_enabled(config: &AppConfig) -> Result<()> {
    if !config.plugins.knowledge_base.enabled {
        bail!("knowledge base plugin is disabled")
    }
    Ok(())
}

fn init_meta_db(conn: &Connection) -> Result<()> {
    conn.execute(
        "CREATE TABLE IF NOT EXISTS files (name TEXT PRIMARY KEY, path TEXT NOT NULL, size_bytes INTEGER NOT NULL, mtime REAL NOT NULL, content_sha256 TEXT NOT NULL, updated_at REAL NOT NULL)",
        [],
    )?;
    Ok(())
}

fn init_semantic_db(conn: &Connection) -> Result<()> {
    conn.execute(
        "CREATE TABLE IF NOT EXISTS semantic_chunks (id INTEGER PRIMARY KEY AUTOINCREMENT, provider_id TEXT NOT NULL, model TEXT NOT NULL, file_name TEXT NOT NULL, content_sha256 TEXT NOT NULL, chunk_index INTEGER NOT NULL, start_char INTEGER NOT NULL, end_char INTEGER NOT NULL, text TEXT NOT NULL, embedding_json TEXT NOT NULL, created_at REAL NOT NULL)",
        [],
    )?;
    conn.execute("CREATE INDEX IF NOT EXISTS idx_semantic_file ON semantic_chunks(file_name, content_sha256)", [])?;
    Ok(())
}

fn kb_root(config: &KnowledgeBasePluginConfig, paths: &MiyuPaths) -> PathBuf {
    let configured = config.data_dir.trim();
    if configured.is_empty() {
        paths.data_dir.join("kb")
    } else {
        expand_path(configured)
    }
}

fn normalize_relative_path(value: &str) -> Result<String> {
    let path = Path::new(value.trim());
    if path.is_absolute() {
        bail!("knowledge base path must be relative")
    }
    let mut parts = Vec::new();
    for component in path.components() {
        match component {
            Component::Normal(part) => {
                let part = part.to_string_lossy();
                if part.contains('\0') || part.trim().is_empty() {
                    bail!("invalid path component")
                }
                parts.push(part.to_string());
            }
            Component::CurDir => {}
            _ => bail!("knowledge base path contains illegal component"),
        }
    }
    if parts.is_empty() {
        bail!("knowledge base path is empty")
    }
    Ok(parts.join("/"))
}

fn collect_files(root: &Path) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(root)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            out.extend(collect_files(&path)?);
        } else if path.is_file() {
            out.push(path);
        }
    }
    Ok(out)
}

fn split_csv(value: &str) -> HashSet<String> {
    value
        .split(',')
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| value.to_ascii_lowercase())
        .collect()
}

fn query_tokens(value: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut ascii = String::new();
    let mut chinese = Vec::new();
    for ch in value.chars() {
        if ch.is_ascii_alphanumeric() {
            ascii.push(ch.to_ascii_lowercase());
            flush_chinese(&mut chinese, &mut tokens);
        } else if ('\u{4e00}'..='\u{9fff}').contains(&ch) {
            if !ascii.is_empty() {
                tokens.push(std::mem::take(&mut ascii));
            }
            chinese.push(ch);
        } else {
            if !ascii.is_empty() {
                tokens.push(std::mem::take(&mut ascii));
            }
            flush_chinese(&mut chinese, &mut tokens);
        }
    }
    if !ascii.is_empty() {
        tokens.push(ascii);
    }
    flush_chinese(&mut chinese, &mut tokens);
    let mut seen = HashSet::new();
    tokens
        .into_iter()
        .filter(|token| token.chars().count() > 1 || !token.is_ascii())
        .filter(|token| seen.insert(token.clone()))
        .collect()
}

fn flush_chinese(chars: &mut Vec<char>, tokens: &mut Vec<String>) {
    if chars.is_empty() {
        return;
    }
    let text = chars.iter().collect::<String>();
    tokens.push(text);
    for window in chars.windows(2) {
        tokens.push(window.iter().collect());
    }
    chars.clear();
}

fn find_positions(content: &str, needle: &str, limit: usize) -> Vec<usize> {
    let mut positions = Vec::new();
    let mut start = 0;
    while let Some(pos) = content[start..].find(needle) {
        let absolute = start + pos;
        positions.push(absolute);
        if positions.len() >= limit {
            break;
        }
        start = absolute + needle.len().max(1);
    }
    positions
}

fn best_window(
    positions_by_token: &HashMap<String, Vec<usize>>,
    tokens: &[String],
    window_chars: usize,
) -> Option<(usize, usize, f32)> {
    let mut events = Vec::new();
    for token in tokens {
        for pos in positions_by_token.get(token).into_iter().flatten() {
            events.push((*pos, token.as_str()));
        }
    }
    events.sort_by_key(|event| event.0);
    let mut best = None;
    for left in 0..events.len() {
        let mut seen = HashSet::new();
        let start = events[left].0;
        let mut end = start;
        for (pos, token) in events.iter().skip(left) {
            if *pos - start > window_chars {
                break;
            }
            seen.insert(*token);
            end = *pos + token.len();
        }
        let coverage = seen.len() as f32 / tokens.len().max(1) as f32;
        if best.map(|(_, _, score)| coverage > score).unwrap_or(true) {
            best = Some((start, end, coverage));
        }
    }
    best.filter(|(_, _, coverage)| *coverage > 0.0)
}

fn extract_snippets(
    content: &str,
    content_lower: &str,
    tokens: &[String],
    context: usize,
) -> Vec<String> {
    let mut snippets = Vec::new();
    for token in tokens {
        if let Some(pos) = content_lower.find(token) {
            snippets.push(snippet_chars(content, pos, pos + token.len(), context));
        }
        if snippets.len() >= 3 {
            break;
        }
    }
    if snippets.is_empty() && !content.trim().is_empty() {
        snippets.push(compact_whitespace(
            &content.chars().take(context * 2).collect::<String>(),
        ));
    }
    snippets
}

fn snippet_chars(content: &str, start: usize, end: usize, context: usize) -> String {
    let start = content[..start.min(content.len())]
        .char_indices()
        .rev()
        .nth(context)
        .map(|(idx, _)| idx)
        .unwrap_or(0);
    let end = content[end.min(content.len())..]
        .char_indices()
        .nth(context)
        .map(|(idx, _)| end.min(content.len()) + idx)
        .unwrap_or(content.len());
    compact_whitespace(&content[start..end])
}

fn build_chunks(content: &str, chunk_chars: usize, overlap: usize) -> Vec<Chunk> {
    let chars = content.char_indices().collect::<Vec<_>>();
    let mut chunks = Vec::new();
    let mut start_char = 0usize;
    let mut index = 0usize;
    let total_chars = content.chars().count();
    while start_char < total_chars {
        let end_char = (start_char + chunk_chars).min(total_chars);
        let start_byte = chars.get(start_char).map(|(idx, _)| *idx).unwrap_or(0);
        let end_byte = chars
            .get(end_char)
            .map(|(idx, _)| *idx)
            .unwrap_or(content.len());
        let text = content[start_byte..end_byte].to_string();
        if !text.trim().is_empty() {
            chunks.push(Chunk {
                index,
                start: start_byte,
                end: end_byte,
                text,
            });
            index += 1;
        }
        if end_char >= total_chars {
            break;
        }
        start_char = end_char.saturating_sub(overlap).max(start_char + 1);
    }
    chunks
}

fn merge_results(results: &mut Vec<SearchResult>, semantic: Vec<SearchResult>, limit: usize) {
    for item in semantic {
        if let Some(existing) = results.iter_mut().find(|result| result.path == item.path) {
            existing.score += item.score * 0.6;
            existing.snippets.extend(item.snippets);
            existing.snippets.truncate(4);
        } else {
            results.push(item);
        }
    }
    results.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    results.truncate(limit);
}

fn score_file_name(query: &str, name: &str) -> (f64, &'static str) {
    let query = query.replace('\\', "/").to_ascii_lowercase();
    let name = name.replace('\\', "/").to_ascii_lowercase();
    let base = file_name(&name);
    if query == name {
        (1000.0, "exact_path")
    } else if query == base {
        (950.0, "exact_file_name")
    } else if name.contains(&query) {
        (820.0 + query.len().min(60) as f64, "path_contains")
    } else if base.contains(&query) {
        (760.0 + query.len().min(60) as f64, "file_name_contains")
    } else {
        let tokens = query_tokens(&query);
        let matched = tokens.iter().filter(|token| name.contains(*token)).count();
        if matched == 0 {
            (0.0, "")
        } else {
            (300.0 + matched as f64 * 80.0, "partial_name_terms")
        }
    }
}

fn cosine(left: &[f32], right: &[f32]) -> f32 {
    if left.len() != right.len() || left.is_empty() {
        return 0.0;
    }
    let mut dot = 0.0;
    let mut left_norm = 0.0;
    let mut right_norm = 0.0;
    for (a, b) in left.iter().zip(right) {
        dot += a * b;
        left_norm += a * a;
        right_norm += b * b;
    }
    if left_norm <= 0.0 || right_norm <= 0.0 {
        0.0
    } else {
        dot / (left_norm.sqrt() * right_norm.sqrt())
    }
}

fn file_name(path: &str) -> String {
    path.rsplit('/').next().unwrap_or(path).to_string()
}

fn directory_name(path: &str) -> String {
    path.rsplit_once('/')
        .map(|(dir, _)| dir.to_string())
        .unwrap_or_default()
}

fn compact_whitespace(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

fn now_secs() -> f64 {
    unix_time(SystemTime::now())
}

fn unix_time(time: SystemTime) -> f64 {
    time.duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
}

fn expand_path(value: &str) -> PathBuf {
    if let Some(rest) = value.trim().strip_prefix("~/") {
        if let Some(home) = directories::BaseDirs::new().map(|dirs| dirs.home_dir().to_path_buf()) {
            return home.join(rest);
        }
    }
    PathBuf::from(value.trim())
}

fn slug(value: &str) -> String {
    let mut slug = value
        .chars()
        .filter_map(|ch| {
            if ch.is_ascii_alphanumeric() {
                Some(ch.to_ascii_lowercase())
            } else if ch.is_whitespace() || matches!(ch, '-' | '_') {
                Some('-')
            } else {
                None
            }
        })
        .collect::<String>();
    while slug.contains("--") {
        slug = slug.replace("--", "-");
    }
    let slug = slug.trim_matches('-');
    if slug.is_empty() {
        format!("note-{}", Local::now().format("%H%M%S"))
    } else {
        slug.chars().take(48).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::paths::MiyuPaths;

    fn test_paths(root: &Path) -> MiyuPaths {
        MiyuPaths {
            config_dir: root.join("config"),
            config_file: root.join("config/config.jsonc"),
            skills_dir: root.join("config/skills"),
            data_dir: root.join("data"),
            cache_dir: root.join("cache"),
            state_dir: root.join("state"),
            pictures_dir: root.join("pictures"),
            fish_hook_file: root.join("fish/conf.d/miyu.fish"),
            bash_hook_file: root.join("config/shell/bash-hook.sh"),
            zsh_hook_file: root.join("config/shell/zsh-hook.zsh"),
            scripts_dir: root.join("config/scripts"),
            system_scripts_dir: PathBuf::new(),
        }
    }

    #[test]
    fn edit_lines_replaces_inclusive_range() {
        let temp = tempfile::tempdir().unwrap();
        let paths = test_paths(temp.path());
        let config = AppConfig::default();
        let kb = KnowledgeBase::new(config, paths).unwrap();
        let source = temp.path().join("note.md");
        std::fs::write(&source, "one\ntwo\nthree\n").unwrap();
        kb.import_file(&source, "notes/note.md").unwrap();

        let result = kb.edit_lines("notes/note.md", 2, 2, "TWO\nTWO-B").unwrap();

        assert_eq!(result.old_line_count, 3);
        assert_eq!(result.new_line_count, 4);
        assert!(!result.semantic_refreshed);
        let edited =
            std::fs::read_to_string(kb.existing_file_path("notes/note.md").unwrap()).unwrap();
        assert_eq!(edited, "one\nTWO\nTWO-B\nthree\n");
        let chunks: i64 = kb
            .semantic_conn()
            .unwrap()
            .query_row("SELECT COUNT(*) FROM semantic_chunks", [], |row| row.get(0))
            .unwrap();
        assert_eq!(chunks, 0);
    }

    #[test]
    fn edit_lines_empty_replacement_deletes_range() {
        let temp = tempfile::tempdir().unwrap();
        let paths = test_paths(temp.path());
        let config = AppConfig::default();
        let kb = KnowledgeBase::new(config, paths).unwrap();
        let source = temp.path().join("note.md");
        std::fs::write(&source, "one\ntwo\nthree").unwrap();
        kb.import_file(&source, "note.md").unwrap();

        let result = kb.edit_lines("note.md", 2, 3, "").unwrap();

        assert_eq!(result.old_line_count, 3);
        assert_eq!(result.new_line_count, 1);
        let edited = std::fs::read_to_string(kb.existing_file_path("note.md").unwrap()).unwrap();
        assert_eq!(edited, "one");
    }

    #[test]
    fn edit_lines_rejects_out_of_range() {
        let temp = tempfile::tempdir().unwrap();
        let paths = test_paths(temp.path());
        let config = AppConfig::default();
        let kb = KnowledgeBase::new(config, paths).unwrap();
        let source = temp.path().join("note.md");
        std::fs::write(&source, "one\n").unwrap();
        kb.import_file(&source, "note.md").unwrap();

        let error = kb.edit_lines("note.md", 2, 2, "two").unwrap_err();

        assert!(error.to_string().contains("out of range"));
    }
}
