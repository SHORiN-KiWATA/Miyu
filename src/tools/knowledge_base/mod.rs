mod search;
mod store;
pub(crate) use search::embed_text;
mod dashboard;
mod files;
mod index;
#[cfg(test)]
use index::keyword_search_blocking;
pub(in crate::tools) use store::reject_non_kb_upload;

use search::*;
use store::*;

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

// 08-21 Edit/Read 统一(用户裁定):upload/edit/remove/read 四个 CRUD 工具退场,
// 写走统一 `edit` 的 kb: 命名空间(apply_patch.rs 路由回本模块的 import_file/
// remove,索引钩子不绕过),读走统一 `read` 的 kb: 前缀。只留语义检索。
pub fn register(registry: &mut ToolRegistry, config: AppConfig, paths: MiyuPaths) {
    register_readonly(registry, config.clone(), paths.clone());
    // 08-21 二次裁定:知识库写入独立成 `kb` 工具(补丁语义,域名即广告)。
    if config.plugins.knowledge_base.upload_tool_enabled {
        crate::tools::apply_patch::register_kb(registry, config, paths);
    }
}

pub fn register_readonly(registry: &mut ToolRegistry, config: AppConfig, paths: MiyuPaths) {
    registry.register(ToolSpec::new(
        "search_knowledge_base",
        // 内容检索与文件名检索合并(08-17):同一个知识库的两种检索口径,
        // 拆成两个工具只是让 tools 数组多背一份外壳。by 缺省 content。
        "Search the local knowledge base. by=content (default) searches file contents and returns paths plus original snippets; by=name finds files by file name, directory, extension, or path fragment and returns relative paths. Use read_knowledge_base_file if snippets are insufficient. Mention paths only when useful or when the user asks.",
        json!({
            "type": "object",
            "properties": {
                "query": { "type": "string", "description": "Search keywords, user question, or (with by=name) a file name / directory / extension / path fragment." },
                "by": { "type": "string", "enum": ["content", "name"], "description": "content searches text, name searches paths. Defaults to content." },
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
                async move {
                    match args.get("by").and_then(Value::as_str).unwrap_or("content") {
                        "content" => tool_search_readonly(args, config, paths).await,
                        "name" => tool_find_readonly(args, config, paths).await,
                        other => bail!("unknown by: {other}; expected content or name"),
                    }
                }
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
        let mut results = self.keyword_search(query, limit).await?;
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
    // 合并后统一用 query;file_name_query 保留为兼容别名。
    let query = args
        .get("query")
        .or_else(|| args.get("file_name_query"))
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
        .find_by_name_readonly(query, max_results)?
        .to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::paths::MiyuPaths;

    pub(super) fn test_paths(root: &Path) -> MiyuPaths {
        MiyuPaths {
            root_dir: root.to_path_buf(),
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

#[cfg(test)]
mod scaling_probe {
    use super::*;
    use std::time::Instant;

    /// 量尺，不是断言：`cargo test --lib knowledge_base::scaling_probe -- --ignored --nocapture`
    ///
    /// keyword_search 对库里**每个**文件做「整读 + 整份 lowercase 拷贝」，
    /// 而且是在 `async fn search_existing` 里同步跑——这段时间 tokio worker
    /// 是卡住的。这里量的就是那段卡住有多长。
    #[test]
    #[ignore]
    fn keyword_search_scaling() {
        println!("\n  文件数  每文件KB   库总量MB   搜索耗时(ms)");
        for (files, kb_each) in [(20usize, 32usize), (50, 32), (100, 32), (200, 32)] {
            let temp = tempfile::tempdir().unwrap();
            let paths = super::tests::test_paths(temp.path());
            let kb = KnowledgeBase::new(AppConfig::default(), paths).unwrap();
            // 内容里不含查询词,走的是「全扫一遍都没命中」这条最坏路径
            let body = "lorem ipsum dolor sit amet ".repeat(kb_each * 1024 / 27);
            for index in 0..files {
                let source = temp.path().join(format!("doc{index}.md"));
                std::fs::write(&source, &body).unwrap();
                kb.import_file(&source, &format!("docs/doc{index}.md"))
                    .unwrap();
            }
            let start = Instant::now();
            let found = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(kb.keyword_search("需要检索的关键词", 5))
                .unwrap();
            let ms = start.elapsed().as_secs_f64() * 1000.0;
            std::hint::black_box(found);
            let total_mb = (files * kb_each) as f64 / 1024.0;
            println!("  {files:>6}  {kb_each:>8}  {total_mb:>9.1}  {ms:>13.1}");
        }
    }

    /// 真正要证明的不是搜索本身变快了（活儿一样多），而是**搜索期间别的
    /// 异步任务还转不转**。单 worker 运行时上放一个 5ms 心跳，量它被堵住的
    /// 最长间隔：同步跑 = 堵满整个搜索时长，spawn_blocking = 基本不堵。
    #[test]
    #[ignore]
    fn keyword_search_does_not_freeze_the_runtime() {
        let temp = tempfile::tempdir().unwrap();
        let paths = super::tests::test_paths(temp.path());
        let kb = KnowledgeBase::new(AppConfig::default(), paths).unwrap();
        let body = "lorem ipsum dolor sit amet ".repeat(200 * 1024 / 27);
        for index in 0..30 {
            let source = temp.path().join(format!("doc{index}.md"));
            std::fs::write(&source, &body).unwrap();
            kb.import_file(&source, &format!("docs/doc{index}.md"))
                .unwrap();
        }

        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();

        for (label, blocking) in [
            ("同步跑（改前的做法）", true),
            ("spawn_blocking（现在）", false),
        ] {
            let gap = runtime.block_on(async {
                let worst = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
                let probe = worst.clone();
                let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
                let halt = stop.clone();
                let ticker = tokio::spawn(async move {
                    let mut last = Instant::now();
                    while !halt.load(std::sync::atomic::Ordering::Relaxed) {
                        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                        let gap = last.elapsed().as_millis() as u64;
                        probe.fetch_max(gap, std::sync::atomic::Ordering::Relaxed);
                        last = Instant::now();
                    }
                });
                tokio::task::yield_now().await;
                // 搜索必须也在 spawn 出来的任务里跑,才会和心跳抢同一个
                // worker——`block_on` 的 future 跑在调用线程上,放这儿量不出
                // 任何东西(第一版探针就是这么白跑的)。
                let records = kb.list().unwrap();
                let search = tokio::spawn(async move {
                    if blocking {
                        // 改前的形状:在 async 上下文里直接同步跑
                        let found =
                            keyword_search_blocking(records, "需要检索的关键词", 5, 200, 200);
                        std::hint::black_box(found.unwrap());
                    } else {
                        let found = tokio::task::spawn_blocking(move || {
                            keyword_search_blocking(records, "需要检索的关键词", 5, 200, 200)
                        })
                        .await
                        .unwrap();
                        std::hint::black_box(found.unwrap());
                    }
                });
                let _ = search.await;
                stop.store(true, std::sync::atomic::Ordering::Relaxed);
                let _ = ticker.await;
                worst.load(std::sync::atomic::Ordering::Relaxed)
            });
            println!("  {label:<26} 心跳最长被堵 {gap:>5} ms");
        }
    }
}
