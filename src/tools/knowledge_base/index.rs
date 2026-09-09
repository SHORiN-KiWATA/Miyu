//! 建索引：关键词与语义。
//!
//! 语义索引是**异步补上**的（`spawn_embedding_reindex`）：写文件不该等嵌入服务
//! 返回。所以查询时可能只有关键词索引，语义那半还在路上——这是可接受的降级，
//! 见 [`super::search`]。
//!
//! 后台那趟重建在库根留三样东西，缺一样就有一类故障会变成哑的：
//!
//! | 文件 | 回答的问题 |
//! |---|---|
//! | `embedding.lock` | 在不在跑（`create_new` 即互斥） |
//! | `embedding-reindex.json` | 跑到哪了 / 上次为什么死（面板轮询读它） |
//! | `embedding-reindex.log` | 子进程 stdout+stderr 原文（给人看） |
//!
//! 09-09 之前只有第一样：子进程两路输出全丢 `/dev/null`、也没人 `wait`，于是
//! 「起不来 / 半路死 / 一个文件都没嵌上」在界面上和「跑完了」长得一模一样——
//! 卡片转瞬回到「空闲」，用户实拍的就是这一幕。

use crate::tools::knowledge_base::*;

/// 一趟重建最多重跑几遍。
///
/// 每遍开头都会把「重跑单」抹掉：跑的过程中新写进来的文件会重新立一张，收尾时
/// 看到就再来一遍。批量导入（一次拖进几千个文件、模型连写十几篇）因此收敛成
/// 一两趟，而不是每个文件一个子进程、还全被锁挡在门外。
const MAX_REINDEX_PASSES: usize = 8;

/// 失败时贴进状态里的日志尾巴长度。够看清一条报错，又不至于把整份状态撑爆。
const LOG_TAIL_BYTES: usize = 2000;

impl KnowledgeBase {
    pub(in crate::tools::knowledge_base) fn reindex_lock_path(&self) -> PathBuf {
        self.root.join("embedding.lock")
    }

    pub(in crate::tools::knowledge_base) fn reindex_progress_path(&self) -> PathBuf {
        self.root.join("embedding-reindex.json")
    }

    pub(in crate::tools::knowledge_base) fn reindex_log_path(&self) -> PathBuf {
        self.root.join("embedding-reindex.log")
    }

    /// 「跑完了再来一遍」的单子。存在即代表清单在这趟开跑之后又变过。
    pub(in crate::tools::knowledge_base) fn reindex_rerun_path(&self) -> PathBuf {
        self.root.join("embedding-reindex.rerun")
    }

    pub(in crate::tools::knowledge_base) fn read_reindex_progress(&self) -> Option<Value> {
        let text = std::fs::read_to_string(self.reindex_progress_path()).ok()?;
        serde_json::from_str(&text).ok()
    }

    /// 写临时文件再 rename。面板每秒读一次，读到半截 JSON 会被当成「没有进度」，
    /// 于是进度条自己闪；rename 是原子的，读侧要么看到旧的一份、要么看到新的。
    pub(in crate::tools::knowledge_base) fn write_reindex_progress(&self, value: &Value) {
        write_reindex_progress_at(&self.reindex_progress_path(), value);
    }

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
        let lock_path = self.reindex_lock_path();
        let lock = match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&lock_path)
        {
            Ok(lock) => lock,
            Err(_) => {
                // 已经有一份在跑。原来这里就地静默返回——而在跑的那份**开跑那一刻
                // 就把文件清单定死了**，此刻刚写进来的文件它一个都不知道，于是那些
                // 文件永远停在「未索引」，谁也不会再来管（09-09 根因之一）。
                // 留一张重跑单，让在跑的那份收尾时自己再走一遍。
                let _ = std::fs::write(self.reindex_rerun_path(), b"");
                if !quiet {
                    println!(
                        "embedding reindex already running; queued a rerun. lock file: {}",
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
        let mut progress = ReindexProgress::new(self, embedder.model_id());
        let mut indexed = 0usize;
        let mut result = Ok(0usize);
        for _ in 0..MAX_REINDEX_PASSES {
            // 先抹单子再取清单：抹在后面的话，这趟开始之后、抹之前写进来的文件
            // 会连同单子一起被吞掉。
            let _ = std::fs::remove_file(self.reindex_rerun_path());
            match self
                .reindex_embeddings_inner(&embedder, quiet, &mut progress)
                .await
            {
                Ok(count) => indexed += count,
                Err(error) => {
                    result = Err(error);
                    break;
                }
            }
            result = Ok(indexed);
            if !self.reindex_rerun_path().exists() {
                break;
            }
        }
        match &result {
            Ok(_) => progress.finish(None),
            Err(error) => progress.finish(Some(format!("{error:#}"))),
        }
        let _ = std::fs::remove_file(lock_path);
        if !quiet {
            if let Ok(count) = &result {
                println!("indexed semantic chunks: {count}");
            }
        }
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
        progress: &mut ReindexProgress,
    ) -> Result<usize> {
        let files = self.list()?;
        let semantic = self.semantic_conn()?;
        init_semantic_db(&semantic)?;
        let model = embedder.model_id().to_string();
        let mut indexed = 0usize;
        // 已经建好的一次查完,而不是每个文件问一次:一趟要走全库,而全库里
        // 绝大多数文件是「上次就建好了、这次直接跳过」的。
        let already: std::collections::HashSet<(String, String)> = {
            let mut stmt = semantic.prepare(
                "SELECT DISTINCT file_name, content_sha256 FROM semantic_chunks \
                 WHERE model = ?1 AND embedding IS NOT NULL",
            )?;
            let rows = stmt.query_map(params![model], |row| Ok((row.get(0)?, row.get(1)?)))?;
            rows.collect::<rusqlite::Result<_>>()?
        };
        let needs_work = |record: &FileRecord| {
            !already.contains(&(record.name.clone(), record.content_sha256.clone()))
        };
        // 分母是「这趟真要嵌的文件数」，不是全库文件数。传 200 个新文件进来时
        // 分母写 6707 的话，进度条会从 0 一路爬到 97% 都还没开始干活，然后在
        // 最后 3% 里跑完全部工作——用户看到的就是「2%」而实际只剩两百个
        // （09-09 实拍）。跳过的那些照样计入 skipped，只是不占分母。
        progress.begin_pass(files.iter().filter(|record| needs_work(record)).count());
        for record in files {
            if !needs_work(&record) {
                progress.skip_file();
                continue;
            }
            progress.start_file(&record.name);
            let content = match std::fs::read_to_string(&record.path) {
                Ok(content) => content,
                Err(error) => {
                    if !quiet {
                        eprintln!("reading {} failed: {error}", record.name);
                    }
                    progress.fail_file(&format!("{error}"));
                    continue;
                }
            };
            let chunks = build_chunks(
                &content,
                self.config.plugins.knowledge_base.semantic_chunk_chars,
                self.config.plugins.knowledge_base.semantic_chunk_overlap,
            );
            // 整篇只有空白的文件切不出块(空文件在导入那关就被挡了,只剩「一个
            // 换行」这类残页)。它建不出语义块,于是每趟都被当成「未索引」重来
            // 一遍——「待重建」永远清不掉零,而界面上看不出为什么。算作失败并
            // 带上原因,至少这一格数字有个交代。
            if chunks.is_empty() {
                semantic.execute(
                    "DELETE FROM semantic_chunks WHERE file_name=?1",
                    params![record.name],
                )?;
                if !quiet {
                    eprintln!("no text to embed in {}", record.name);
                }
                progress.fail_file("file has no text to embed");
                continue;
            }
            let texts: Vec<String> = chunks.iter().map(|chunk| chunk.text.clone()).collect();
            let vectors = match embedder.embed(&texts).await {
                Ok(vectors) => vectors,
                Err(err) => {
                    if !quiet {
                        eprintln!("embedding failed for {}: {err:#}", record.name);
                    }
                    // 每个文件都失败时整趟仍然「成功」返回 0——面板要能看出
                    // 这和「本来就没东西要建」不是一回事,所以把最后一条原因
                    // 和失败计数一起带进进度里。
                    progress.fail_file(&format!("{err:#}"));
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
            progress.end_file(chunks.len());
        }
        // Vectors from other models are dead weight once the current model
        // has covered the library; the reindex is the natural sweep point.
        semantic.execute(
            "DELETE FROM semantic_chunks WHERE model != ?1",
            params![model],
        )?;
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
        std::fs::create_dir_all(&self.root)?;
        let log = self.open_reindex_log()?;
        let exe = crate::paths::miyu_executable()?;
        // 起手先写一帧「正在启动」:子进程从 exec 到建锁有几百毫秒,而前端 POST
        // 完立刻刷新概览——不占住这个窗口,用户看到的第一帧就是「空闲」,于是
        // 认定点了没反应(09-09 实拍)。已经有人在跑就别碰,那份进度是人家的。
        if !self.reindex_lock_path().exists() {
            self.write_reindex_progress(&json!({
                "phase": "starting",
                "started_at": now_secs(),
                "updated_at": now_secs(),
                "total": 0,
                "done": 0,
                "indexed": 0,
                "skipped": 0,
                "failed": 0,
                "current": "",
                "model": "",
                "error": "",
                "last_file_error": "",
            }));
        }
        // `--quiet` 撤了:输出现在有去处(日志文件),而「哪个文件为什么嵌不上」
        // 正是唯一能救场的信息。
        let mut child = Command::new(exe)
            .args(["kb", "embed", "reindex"])
            .stdin(Stdio::null())
            .stdout(Stdio::from(log.try_clone()?))
            .stderr(Stdio::from(log))
            .spawn()?;
        let progress_path = self.reindex_progress_path();
        let log_path = self.reindex_log_path();
        let lock_path = self.reindex_lock_path();
        // 不 wait 就等于「死了也没人知道」。子进程 200ms 内退出时进度还停在
        // starting,面板会一直显示「进行中」;这个 watcher 只在子进程走了、
        // 进度却没落到终态时补一笔失败,并把日志末尾贴进去。
        tokio::spawn(async move {
            let Ok(status) = child.wait().await else {
                return;
            };
            // 锁还在 = 这次是被「已有一份在跑」挡回来的,别动别人的进度。
            if lock_path.exists() {
                return;
            }
            let mut value = std::fs::read_to_string(&progress_path)
                .ok()
                .and_then(|text| serde_json::from_str::<Value>(&text).ok())
                .unwrap_or_else(|| json!({}));
            match value.get("phase").and_then(Value::as_str) {
                Some("done") | Some("failed") => return,
                _ => {}
            }
            value["phase"] = json!("failed");
            value["finished_at"] = json!(now_secs());
            value["updated_at"] = json!(now_secs());
            value["error"] = json!(if status.success() {
                "reindex process exited without indexing anything".to_string()
            } else {
                format!("reindex process exited: {status}")
            });
            value["log_tail"] = json!(reindex_log_tail(&log_path));
            write_reindex_progress_at(&progress_path, &value);
        });
        Ok(())
    }

    /// 追加打开;太大就先截断。上一趟的日志有参考价值,但不能无限长。
    fn open_reindex_log(&self) -> Result<std::fs::File> {
        let path = self.reindex_log_path();
        let too_big = std::fs::metadata(&path).is_ok_and(|meta| meta.len() > 512 * 1024);
        Ok(std::fs::OpenOptions::new()
            .create(true)
            .append(!too_big)
            .write(true)
            .truncate(too_big)
            .open(&path)?)
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
        tuned_conn(&self.meta_db)
    }

    pub(in crate::tools::knowledge_base) fn semantic_conn(&self) -> Result<Connection> {
        if let Some(parent) = self.semantic_db.parent() {
            std::fs::create_dir_all(parent)?;
        }
        tuned_conn(&self.semantic_db)
    }
}

/// 库的两个库都是「一个进程写、另一个进程同时读」的形状：重建子进程逐文件写，
/// daemon 这边面板每秒读一遍全表统计。回滚日志模式下读会挡住写提交，而写失败
/// 是 `?` 直接把整趟重建掀掉（掀了以前还没人知道）。WAL 让读写互不相干，
/// busy_timeout 兜住偶发争用——记忆库与消息库早就是这么开的。
fn tuned_conn(path: &Path) -> Result<Connection> {
    let conn = Connection::open(path)?;
    conn.busy_timeout(std::time::Duration::from_secs(15))?;
    // 只读挂载等场景下 WAL 切不过去，不是致命错：切不了就照旧跑。
    let _ = conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;");
    Ok(conn)
}

fn write_reindex_progress_at(path: &Path, value: &Value) {
    let temp = path.with_extension("json.tmp");
    if std::fs::write(&temp, value.to_string()).is_ok() {
        let _ = std::fs::rename(&temp, path);
    }
}

/// 日志末尾几行。失败原因通常就在最后一两行（ORT 装没装、模型加载超时、
/// 端点报的话）。
fn reindex_log_tail(path: &Path) -> String {
    let Ok(text) = std::fs::read_to_string(path) else {
        return String::new();
    };
    let start = text.len().saturating_sub(LOG_TAIL_BYTES);
    text[text
        .char_indices()
        .map(|(index, _)| index)
        .find(|index| *index >= start)
        .unwrap_or(0)..]
        .trim()
        .to_string()
}

/// 重建过程写给面板看的一帧一帧。
///
/// 每个文件都落一次盘太浪费（6000 个文件 = 6000 次 rename），完全不落又等于
/// 没有进度：250ms 一帧是「人眼看得出在动」和「别把时间花在写自己身上」之间
/// 的折中，收尾那一帧强制写。
#[cfg(test)]
mod progress_tests {
    use super::*;

    fn progress(total: usize) -> ReindexProgress {
        let temp = tempfile::tempdir().unwrap();
        let mut progress = ReindexProgress {
            path: temp.path().join("progress.json"),
            started_at: 0.0,
            model: "test".to_string(),
            total: 0,
            done: 0,
            indexed: 0,
            skipped: 0,
            failed: 0,
            current: String::new(),
            last_file_error: String::new(),
            last_flush: std::time::Instant::now(),
        };
        // temp 掉了也无所谓:flush 写不进去只是没有进度文件,计数照样对。
        progress.begin_pass(total);
        progress
    }

    /// 09-09 用户实拍:传 200 个新文件进来,卡片显示「195/6707 · 2%」。
    ///
    /// 分母写成全库文件数的话,进度条会从 0 一路爬到 97% 都还没开始干活,
    /// 然后在最后 3% 里跑完全部工作。分母必须是「这趟真要嵌的文件数」;
    /// 上次就建好的那些照样计入 skipped,但不占分母、也不推进 done。
    #[test]
    fn skipped_files_stay_out_of_the_denominator() {
        let mut progress = progress(200);
        for _ in 0..6_507 {
            progress.skip_file();
        }
        assert_eq!(progress.total, 200, "分母是这趟的工作量");
        assert_eq!(progress.done, 0, "跳过的文件不推进进度");
        assert_eq!(progress.skipped, 6_507);

        for _ in 0..195 {
            progress.end_file(3);
        }
        assert_eq!(progress.done, 195);
        assert_eq!(progress.total, 200);
        // 195/200 = 97%,而不是 195/6707 = 2%。
        assert_eq!(progress.done * 100 / progress.total, 97);
        assert_eq!(progress.indexed, 195 * 3);
    }

    #[test]
    fn a_failed_file_still_counts_as_processed() {
        let mut progress = progress(3);
        progress.end_file(2);
        progress.fail_file("boom");
        assert_eq!(
            (progress.done, progress.failed, progress.indexed),
            (2, 1, 2)
        );
    }
}

pub(in crate::tools::knowledge_base) struct ReindexProgress {
    path: PathBuf,
    started_at: f64,
    model: String,
    total: usize,
    done: usize,
    indexed: usize,
    skipped: usize,
    failed: usize,
    current: String,
    last_file_error: String,
    last_flush: std::time::Instant,
}

impl ReindexProgress {
    fn new(kb: &KnowledgeBase, model: &str) -> Self {
        let mut progress = Self {
            path: kb.reindex_progress_path(),
            started_at: now_secs(),
            model: model.to_string(),
            total: 0,
            done: 0,
            indexed: 0,
            skipped: 0,
            failed: 0,
            current: String::new(),
            last_file_error: String::new(),
            last_flush: std::time::Instant::now(),
        };
        progress.flush("running", "", true);
        progress
    }

    /// 一趟的开始。done/skipped/failed 讲的是「这一趟走到哪、这一趟有多少没建
    /// 上」，所以随 total 一起归零；只有 indexed 是整次重建的累计（人问的是
    /// 「这次一共建了多少块」，不是「最后一趟建了多少」）。
    fn begin_pass(&mut self, total: usize) {
        self.total = total;
        self.done = 0;
        self.skipped = 0;
        self.failed = 0;
        self.last_file_error = String::new();
        self.flush("running", "", true);
    }

    fn start_file(&mut self, name: &str) {
        self.current = name.to_string();
        self.flush("running", "", false);
    }

    /// 上次就建好、这趟不用动的文件。**不进分母**——它不是这趟的工作量。
    fn skip_file(&mut self) {
        self.skipped += 1;
    }

    fn end_file(&mut self, chunks: usize) {
        self.done += 1;
        self.indexed += chunks;
        self.flush("running", "", false);
    }

    fn fail_file(&mut self, error: &str) {
        self.done += 1;
        self.failed += 1;
        self.last_file_error = error.to_string();
        self.flush("running", "", false);
    }

    fn finish(&mut self, error: Option<String>) {
        self.current = String::new();
        let phase = if error.is_some() { "failed" } else { "done" };
        self.flush(phase, &error.unwrap_or_default(), true);
    }

    fn flush(&mut self, phase: &str, error: &str, force: bool) {
        if !force && self.last_flush.elapsed() < std::time::Duration::from_millis(250) {
            return;
        }
        self.last_flush = std::time::Instant::now();
        let mut value = json!({
            "phase": phase,
            "started_at": self.started_at,
            "updated_at": now_secs(),
            "model": self.model,
            "total": self.total,
            "done": self.done,
            "indexed": self.indexed,
            "skipped": self.skipped,
            "failed": self.failed,
            "current": self.current,
            "error": error,
            "last_file_error": self.last_file_error,
        });
        if phase == "done" || phase == "failed" {
            value["finished_at"] = json!(now_secs());
        }
        write_reindex_progress_at(&self.path, &value);
    }
}
