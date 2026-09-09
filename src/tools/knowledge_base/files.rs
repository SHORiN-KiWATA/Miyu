//! 文件的增删改查。
//!
//! 路径全部过 `safe_file_path`——名字来自模型，必须限制在库根之内。
//! `*_readonly` 与 `*_existing` 是三套并行的入口：只读模式不许写、
//! 「existing」变体要求库已存在而不是顺手建一个。

use crate::tools::knowledge_base::*;

impl KnowledgeBase {
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

    pub(in crate::tools::knowledge_base) fn list_existing(&self) -> Result<Vec<FileRecord>> {
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

    pub(in crate::tools::knowledge_base) fn find_by_name_existing(
        &self,
        query: &str,
        max_results: Option<usize>,
    ) -> Result<Value> {
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

    /// Resolve a caller-supplied path to a stored record name, tolerating
    /// omitted directory prefixes (e.g. `4. xx/文件.md` for
    /// `default-kb/kb/4. xx/文件.md`): exact match first, then a unique
    /// suffix match; otherwise fail with concrete candidates so the model
    /// can self-correct in one step.
    pub(in crate::tools::knowledge_base) fn resolve_stored_name(
        &self,
        rel: &str,
    ) -> Result<String> {
        let records = self.list_existing()?;
        if records.iter().any(|record| record.name == rel) {
            return Ok(rel.to_string());
        }
        let suffix = format!("/{rel}");
        let matches = records
            .iter()
            .filter(|record| record.name.ends_with(&suffix))
            .map(|record| record.name.clone())
            .collect::<Vec<_>>();
        match matches.len() {
            1 => Ok(matches.into_iter().next().unwrap()),
            0 => {
                let mut scored = records
                    .iter()
                    .map(|record| (score_file_name(rel, &record.name).0, &record.name))
                    .filter(|(score, _)| *score > 0.0)
                    .collect::<Vec<_>>();
                scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
                let hints = scored
                    .iter()
                    .take(3)
                    .map(|(_, name)| name.as_str())
                    .collect::<Vec<_>>();
                if hints.is_empty() {
                    bail!("knowledge base file not found: {rel}")
                }
                bail!(
                    "knowledge base file not found: {rel}；相近的文件: {}",
                    hints.join("、")
                )
            }
            _ => bail!(
                "path {rel} matches multiple knowledge base files: {}；请用完整路径",
                matches.join("、")
            ),
        }
    }

    pub(in crate::tools::knowledge_base) fn read_file_existing(
        &self,
        name: &str,
        start_line: usize,
        max_lines: Option<usize>,
        create_parent: bool,
    ) -> Result<String> {
        let mut rel = normalize_relative_path(name)?;
        let build_path = |rel: &str| -> Result<PathBuf> {
            if create_parent {
                self.safe_file_path(rel)
            } else {
                // Fails with ENOENT when the parent directory itself is
                // missing — treat that the same as "file not found" so the
                // prefix-tolerant resolution below still gets its chance.
                self.existing_file_path(rel)
            }
        };
        let mut path = build_path(&rel).unwrap_or_default();
        if !path.exists() {
            rel = self.resolve_stored_name(&rel)?;
            path = build_path(&rel)?;
        }
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
        let file_existed = path.exists();
        if file_existed {
            std::fs::remove_file(&path)?;
        }
        let conn = self.meta_conn()?;
        let deleted_rows = conn.execute("DELETE FROM files WHERE name=?1", params![rel])?;
        let semantic = self.semantic_conn()?;
        semantic.execute(
            "DELETE FROM semantic_chunks WHERE file_name=?1",
            params![rel],
        )?;
        // 目标本来就不存在时必须报错:静默返回 ok 会让模型以为删除成功。
        if !file_existed && deleted_rows == 0 {
            anyhow::bail!("knowledge base file not found: {rel}");
        }
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

    pub(in crate::tools::knowledge_base) fn remove_prefix(&self, prefix: &str) -> Result<()> {
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

    pub(in crate::tools) fn import_file(&self, source: &Path, name: &str) -> Result<String> {
        // 先看元数据再整读:超大文件不该先撑满 RAM 再被大小校验拒绝。
        let max_bytes = self.config.plugins.knowledge_base.max_file_size_kb * 1024;
        let size = std::fs::metadata(source)?.len();
        if size > max_bytes as u64 {
            bail!("file too large: {size} bytes");
        }
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

    pub(in crate::tools::knowledge_base) fn validate_file(
        &self,
        name: &str,
        bytes: &[u8],
    ) -> Result<()> {
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

    pub(in crate::tools) fn safe_file_path(&self, rel: &str) -> Result<PathBuf> {
        let rel = normalize_relative_path(rel)?;
        let path = self.files_dir.join(&rel);
        let parent = path.parent().unwrap_or(&self.files_dir);
        // 两个目录都建好之后再取真实路径。基准目录还不存在时 canonicalize 会
        // 失败、退回未解析的原始路径,而 parent 那边解析成功——于是路径里只要
        // 有一层符号链接,两边就对不上,第一次往新库写文件必然报「逃出目录」。
        // macOS 的 /tmp→/private/tmp、/var→/private/var 是现成的踩法。
        std::fs::create_dir_all(&self.files_dir)?;
        std::fs::create_dir_all(parent)?;
        let base = self
            .files_dir
            .canonicalize()
            .unwrap_or_else(|_| self.files_dir.clone());
        let resolved_parent = parent.canonicalize()?;
        if !resolved_parent.starts_with(&base) {
            bail!("knowledge base path escapes files dir")
        }
        Ok(path)
    }

    pub(in crate::tools::knowledge_base) fn existing_file_path(
        &self,
        rel: &str,
    ) -> Result<PathBuf> {
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
