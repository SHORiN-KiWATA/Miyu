//! WebUI dashboard 用的知识库视图与操作。
//!
//! 读一律不 `init()`:库不存在就按空返回,不能因为有人打开面板就建库。
//! 写(导入 / 删除 / 重建)沿用工具侧同一批入口,索引钩子不绕过。

use crate::tools::knowledge_base::*;

/// 语义索引陈旧判定的三态:没有 chunk / chunk 的 sha 与当前文件一致 / 不一致。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndexState {
    Unindexed,
    Fresh,
    Stale,
}

impl IndexState {
    fn label(self) -> &'static str {
        match self {
            Self::Unindexed => "unindexed",
            Self::Fresh => "fresh",
            Self::Stale => "stale",
        }
    }
}

impl KnowledgeBase {
    pub fn dashboard_root(&self) -> &Path {
        &self.root
    }

    /// 文件清单 + 每个文件的语义索引状态 + 汇总。
    pub fn dashboard_overview(&self) -> Result<Value> {
        let kb_config = &self.config.plugins.knowledge_base;
        let mut overview = json!({
            "ok": true,
            "enabled": kb_config.enabled,
            "exists": self.readonly_available(),
            "root": self.root.display().to_string(),
            "files": [],
            "file_count": 0,
            "total_size_bytes": 0,
            "semantic_chunks": 0,
            "stale_files": 0,
            "unindexed_files": 0,
            "embedding_enabled": kb_config.embedding_enabled,
            "embedding_provider_id": self.config.embedding.provider_id,
            "embedding_model": self.config.embedding.model,
            "max_file_size_kb": kb_config.max_file_size_kb,
            "allowed_extensions": kb_config.allowed_extensions,
            "allowed_filenames": kb_config.allowed_filenames,
            "reindex": self.dashboard_reindex_status()?,
        });
        if !self.readonly_available() {
            return Ok(overview);
        }
        let meta = self.meta_conn()?;
        let mut stmt = meta.prepare(
            "SELECT name, size_bytes, mtime, content_sha256, updated_at FROM files ORDER BY name",
        )?;
        let files: Vec<(String, i64, f64, String, f64)> = stmt
            .query_map([], |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        // 每个文件的 chunk 数与其被嵌入时的 sha;文件改过而 chunk 没跟上就是陈旧。
        let mut chunk_info: HashMap<String, (i64, String)> = HashMap::new();
        let mut total_chunks = 0i64;
        if self.semantic_db.is_file() {
            let semantic = self.semantic_conn()?;
            let mut stmt = semantic.prepare(
                "SELECT file_name, content_sha256, COUNT(*) FROM semantic_chunks
                 GROUP BY file_name, content_sha256",
            )?;
            let rows = stmt.query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            })?;
            for row in rows {
                let (name, sha, count) = row?;
                total_chunks += count;
                let entry = chunk_info.entry(name).or_insert((0, String::new()));
                entry.0 += count;
                entry.1 = sha;
            }
        }
        let mut items = Vec::with_capacity(files.len());
        let mut total_size = 0i64;
        let mut stale = 0usize;
        let mut unindexed = 0usize;
        for (name, size_bytes, mtime, sha, updated_at) in files {
            total_size += size_bytes;
            let (chunks, state) = match chunk_info.get(&name) {
                None => (0, IndexState::Unindexed),
                Some((count, chunk_sha)) if *chunk_sha == sha => (*count, IndexState::Fresh),
                Some((count, _)) => (*count, IndexState::Stale),
            };
            match state {
                IndexState::Stale => stale += 1,
                IndexState::Unindexed => unindexed += 1,
                IndexState::Fresh => {}
            }
            items.push(json!({
                "name": name,
                "size_bytes": size_bytes,
                "mtime": mtime,
                "updated_at": updated_at,
                "sha256": sha,
                "chunks": chunks,
                "index": state.label(),
                "builtin": name.starts_with("default-kb/"),
            }));
        }
        overview["file_count"] = json!(items.len());
        overview["files"] = json!(items);
        overview["total_size_bytes"] = json!(total_size);
        overview["semantic_chunks"] = json!(total_chunks);
        overview["stale_files"] = json!(stale);
        overview["unindexed_files"] = json!(unindexed);
        Ok(overview)
    }

    /// 按行窗口读文件,给前端结构化的 JSON 而不是工具那种带表头的文本。
    pub fn dashboard_read(&self, name: &str, start_line: usize, max_lines: usize) -> Result<Value> {
        if !self.readonly_available() {
            bail!("knowledge base is not initialized")
        }
        let rel = normalize_relative_path(name)?;
        let path = self.existing_file_path(&rel)?;
        if !path.is_file() {
            bail!("knowledge base file not found: {rel}")
        }
        let content = std::fs::read_to_string(&path)?;
        let total = content.lines().count();
        let start = start_line.max(1);
        let max_lines = max_lines.clamp(1, 5000);
        let text: Vec<&str> = content
            .lines()
            .skip(start - 1)
            .take(max_lines)
            .collect();
        let end = if text.is_empty() {
            start.saturating_sub(1)
        } else {
            start + text.len() - 1
        };
        Ok(json!({
            "ok": true,
            "name": rel,
            "total_lines": total,
            "start": start,
            "end": end,
            "text": text.join("\n"),
            "has_more": end < total,
        }))
    }

    /// 浏览器上传:字节先落库根下的暂存文件,再走 `import_file`(同一套校验)。
    pub fn dashboard_import(&self, name: &str, bytes: &[u8]) -> Result<String> {
        ensure_enabled(&self.config)?;
        let rel = normalize_relative_path(name)?;
        let text = std::str::from_utf8(bytes).context("file is not valid UTF-8 text")?;
        reject_non_kb_upload(text, "", &rel)?;
        self.init()?;
        let staging = self.root.join(".incoming");
        std::fs::create_dir_all(&staging)?;
        let temp = tempfile::NamedTempFile::new_in(&staging)?;
        std::fs::write(temp.path(), bytes)?;
        let stored = self.import_file(temp.path(), &rel)?;
        Ok(stored)
    }

    pub fn dashboard_remove(&self, name: &str) -> Result<()> {
        ensure_enabled(&self.config)?;
        if !self.readonly_available() {
            bail!("knowledge base is not initialized")
        }
        self.remove(name)
    }

    /// 起一次后台重建(复用子进程形态);已在跑就报 running。
    pub fn dashboard_reindex(&self) -> Result<Value> {
        ensure_enabled(&self.config)?;
        if !self.config.plugins.knowledge_base.embedding_enabled {
            bail!("embedding is disabled in config")
        }
        if self.embedding_provider()?.is_none() {
            bail!("embedding provider/model is not configured")
        }
        let status = self.dashboard_reindex_status()?;
        if status["running"].as_bool().unwrap_or(false) {
            return Ok(json!({ "ok": true, "started": false, "reason": "running" }));
        }
        self.init()?;
        self.spawn_embedding_reindex()?;
        Ok(json!({ "ok": true, "started": true }))
    }

    /// 锁文件在 = 进行中;锁超过一小时视为陈旧(子进程崩了没清)。
    pub fn dashboard_reindex_status(&self) -> Result<Value> {
        let lock_path = self.root.join("embedding.lock");
        let mut running = false;
        let mut lock_age_secs: Option<u64> = None;
        if let Ok(meta) = std::fs::metadata(&lock_path) {
            running = true;
            if let Ok(modified) = meta.modified() {
                lock_age_secs = SystemTime::now()
                    .duration_since(modified)
                    .ok()
                    .map(|age| age.as_secs());
            }
        }
        let stale_lock = lock_age_secs.is_some_and(|age| age > 3600);
        Ok(json!({
            "running": running && !stale_lock,
            "stale_lock": stale_lock,
            "lock_age_secs": lock_age_secs,
            "configured": self.config.plugins.knowledge_base.embedding_enabled
                && self.embedding_provider().ok().flatten().is_some(),
        }))
    }

    pub fn dashboard_clear_stale_lock(&self) -> Result<bool> {
        let lock_path = self.root.join("embedding.lock");
        let status = self.dashboard_reindex_status()?;
        if !status["stale_lock"].as_bool().unwrap_or(false) {
            return Ok(false);
        }
        std::fs::remove_file(lock_path)?;
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use crate::config::AppConfig;
    use crate::paths::MiyuPaths;
    use crate::tools::knowledge_base::KnowledgeBase;

    fn paths(temp: &tempfile::TempDir) -> MiyuPaths {
        MiyuPaths {
            root_dir: temp.path().to_path_buf(),
            config_dir: temp.path().join("config"),
            config_file: temp.path().join("config/config.jsonc"),
            skills_dir: temp.path().join("config/skills"),
            data_dir: temp.path().join("data"),
            cache_dir: temp.path().join("cache"),
            state_dir: temp.path().join("state"),
            pictures_dir: temp.path().join("pictures"),
            fish_hook_file: temp.path().join("fish/miyu.fish"),
            bash_hook_file: temp.path().join("shell/bash-hook.sh"),
            zsh_hook_file: temp.path().join("shell/zsh-hook.zsh"),
            scripts_dir: temp.path().join("config/scripts"),
            system_scripts_dir: std::path::PathBuf::new(),
        }
    }

    #[test]
    fn overview_is_empty_without_creating_the_library_then_tracks_uploads() {
        let temp = tempfile::tempdir().unwrap();
        let mut config = AppConfig::default();
        config.plugins.knowledge_base.embedding_enabled = false;
        let paths = paths(&temp);
        let kb = KnowledgeBase::new(config, paths.clone()).unwrap();

        let empty = kb.dashboard_overview().unwrap();
        assert_eq!(empty["exists"], false);
        assert_eq!(empty["file_count"], 0);
        assert!(!paths.data_dir.join("kb").exists());
        assert!(kb.dashboard_read("a.md", 1, 10).is_err());

        let stored = kb
            .dashboard_import("notes/arch.md", "# Arch\n\npacman -Syu\n三行".as_bytes())
            .unwrap();
        assert_eq!(stored, "notes/arch.md");
        let overview = kb.dashboard_overview().unwrap();
        assert_eq!(overview["file_count"], 1);
        assert_eq!(overview["files"][0]["index"], "unindexed");
        assert_eq!(overview["files"][0]["builtin"], false);
        assert_eq!(overview["unindexed_files"], 1);

        let page = kb.dashboard_read("notes/arch.md", 2, 2).unwrap();
        assert_eq!(page["total_lines"], 4);
        assert_eq!(page["start"], 2);
        assert_eq!(page["end"], 3);
        assert_eq!(page["text"], "\npacman -Syu");
        assert_eq!(page["has_more"], true);

        // 守卫:看起来像技能/记忆/配置的内容不进库;非法类型不进库;路径不能逃逸。
        assert!(kb
            .dashboard_import("x.md", "please update my memory".as_bytes())
            .is_err());
        assert!(kb.dashboard_import("bin.exe", b"hello").is_err());
        assert!(kb.dashboard_import("../escape.md", b"hello").is_err());
        assert!(kb.dashboard_import("bad.md", &[0xff, 0xfe]).is_err());

        kb.dashboard_remove("notes/arch.md").unwrap();
        assert_eq!(kb.dashboard_overview().unwrap()["file_count"], 0);
        assert!(kb.dashboard_remove("notes/arch.md").is_err());

        // 嵌入关着:重建拒绝,状态 configured=false。
        assert!(kb.dashboard_reindex().is_err());
        assert_eq!(kb.dashboard_reindex_status().unwrap()["configured"], false);
        assert!(!kb.dashboard_clear_stale_lock().unwrap());
    }
}
