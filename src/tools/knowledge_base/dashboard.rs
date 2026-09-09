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
        let embedder = self.embedder();
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
            // 真相只有一处:能不能造出 Embedder。原先这里直接读
            // `config.embedding.provider_id/model`,那两个字段只有**远程**后端
            // 才填——用内置的本地模型时它们是空的,面板于是报「未配置模型」,
            // 而同一屏的重建卡按 embedder().is_some() 判定,报「嵌入已配置」。
            // 同屏自相矛盾(09-09 用户实拍)。
            "embedding_enabled": kb_config.embedding_enabled,
            "embedding_configured": embedder.is_some(),
            "embedding_model_id": embedder
                .as_ref()
                .map(|embedder| embedder.model_id().to_string())
                .unwrap_or_default(),
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
        let text: Vec<&str> = content.lines().skip(start - 1).take(max_lines).collect();
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

    /// 起一次后台重建(复用子进程形态);已在跑就排队,不丢请求。
    pub fn dashboard_reindex(&self) -> Result<Value> {
        ensure_enabled(&self.config)?;
        if !self.config.plugins.knowledge_base.embedding_enabled {
            bail!("embedding is disabled in config")
        }
        if self.embedder().is_none() {
            bail!("embedding provider/model is not configured")
        }
        let status = self.dashboard_reindex_status()?;
        // 挡住新请求的凭据只认**锁**,不认状态里那个 running——running 还包含
        // 「正在启动」,而一个刚 exec 就死掉的子进程从来没建过锁,也就没人会来
        // 接那张重跑单:拿它挡门等于把后面每一次点击都吞掉。多起一个进程反而
        // 无害:真有人占着锁,新起的那个自己会留单子退场。
        let locked =
            self.reindex_lock_path().exists() && !status["stale_lock"].as_bool().unwrap_or(false);
        if locked {
            // 在跑的那趟**开跑那一刻就把文件清单定死了**,刚传进来的文件它一个
            // 都不知道。原来这里回一句「在跑」就完事,这次请求就此蒸发,那批文件
            // 于是永远停在「未索引」——而等那趟跑完锁一清,卡片又回到「空闲」,
            // 界面上一点痕迹都没有(09-09 用户实拍的根因)。留一张重跑单。
            std::fs::write(self.reindex_rerun_path(), b"")?;
            return Ok(json!({ "ok": true, "started": false, "reason": "queued" }));
        }
        self.init()?;
        self.spawn_embedding_reindex()?;
        Ok(json!({ "ok": true, "started": true }))
    }

    /// 重建状态 = 锁(在不在跑)+ 进度文件(跑到哪了 / 上次为什么死)。
    ///
    /// 锁单独一个是不够的:子进程从 exec 到建锁有几百毫秒,面板恰好在这个窗口
    /// 刷新就会读到「没有锁 = 空闲」;而子进程半路死掉时锁被留下,老规矩要等
    /// 满一小时才肯认。进度文件带 `updated_at`,两头都能答得准。
    pub fn dashboard_reindex_status(&self) -> Result<Value> {
        let lock_path = self.reindex_lock_path();
        let mut lock_present = false;
        let mut lock_age_secs: Option<u64> = None;
        if let Ok(meta) = std::fs::metadata(&lock_path) {
            lock_present = true;
            if let Ok(modified) = meta.modified() {
                lock_age_secs = SystemTime::now()
                    .duration_since(modified)
                    .ok()
                    .map(|age| age.as_secs());
            }
        }
        let progress = self.read_reindex_progress().unwrap_or_else(|| json!({}));
        let phase = progress
            .get("phase")
            .and_then(Value::as_str)
            .unwrap_or("idle")
            .to_string();
        let active = matches!(phase.as_str(), "starting" | "running");
        let idle_secs = progress
            .get("updated_at")
            .and_then(Value::as_f64)
            .map(|updated| (now_secs() - updated).max(0.0));
        // 多久不动算「那个进程已经不在了」。跑起来之后单个文件最坏也就是
        // 「worker 冷启动 30s + 重试一次」,五分钟很宽;而「正在启动」只该持续
        // 一次 exec + 读配置,一分钟还没建锁就是根本没起来(配置读坏、二进制
        // 换掉)——daemon 起的那份有 watcher 当场判死,命令行起的那份没有,
        // 靠这个上限收尸。
        let stall_after = if phase == "starting" { 60.0 } else { 300.0 };
        let stalled = active && idle_secs.is_some_and(|idle| idle > stall_after);
        let stale_lock = lock_present && (lock_age_secs.is_some_and(|age| age > 3600) || stalled);
        let running = (lock_present && !stale_lock) || (active && !stalled);
        let mut status = json!({
            "running": running,
            "stale_lock": stale_lock,
            "lock_age_secs": lock_age_secs,
            "configured": self.config.plugins.knowledge_base.embedding_enabled
                && self.embedder().is_some(),
            "phase": if stalled { "failed" } else { phase.as_str() },
            "total": progress.get("total").cloned().unwrap_or(json!(0)),
            "done": progress.get("done").cloned().unwrap_or(json!(0)),
            "indexed": progress.get("indexed").cloned().unwrap_or(json!(0)),
            "skipped": progress.get("skipped").cloned().unwrap_or(json!(0)),
            "failed": progress.get("failed").cloned().unwrap_or(json!(0)),
            "current": progress.get("current").cloned().unwrap_or(json!("")),
            "started_at": progress.get("started_at").cloned().unwrap_or(Value::Null),
            "finished_at": progress.get("finished_at").cloned().unwrap_or(Value::Null),
            "last_file_error": progress.get("last_file_error").cloned().unwrap_or(json!("")),
            "log_path": self.reindex_log_path().display().to_string(),
        });
        // 失败原因只在真失败时给:平时挂一条旧报错在卡片上会让人以为又坏了。
        status["last_error"] = if stalled {
            json!(format!(
                "reindex process is gone (no progress for {} seconds)",
                idle_secs.unwrap_or_default() as u64
            ))
        } else if phase == "failed" {
            let mut message = progress
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let tail = progress
                .get("log_tail")
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim();
            if !tail.is_empty() {
                message = format!("{message}\n{tail}");
            }
            json!(message.trim())
        } else {
            json!("")
        };
        Ok(status)
    }

    pub fn dashboard_clear_stale_lock(&self) -> Result<bool> {
        let status = self.dashboard_reindex_status()?;
        if !status["stale_lock"].as_bool().unwrap_or(false) {
            return Ok(false);
        }
        std::fs::remove_file(self.reindex_lock_path())?;
        // 进度也一并落定,否则卡片会继续举着一条永远走不完的进度条。
        if let Some(mut progress) = self.read_reindex_progress() {
            if matches!(
                progress.get("phase").and_then(Value::as_str),
                Some("starting") | Some("running")
            ) {
                progress["phase"] = json!("failed");
                progress["finished_at"] = json!(now_secs());
                progress["error"] = json!("stale lock cleared by the dashboard");
                self.write_reindex_progress(&progress);
            }
        }
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use crate::config::AppConfig;
    use crate::paths::MiyuPaths;
    use crate::tools::knowledge_base::KnowledgeBase;
    use serde_json::{json, Value};

    fn now() -> f64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs_f64()
    }

    /// 手工摆出重建进程留下的那份进度,免得测试真去起一个子进程。
    fn write_progress(kb: &KnowledgeBase, value: Value) {
        std::fs::create_dir_all(kb.dashboard_root()).unwrap();
        std::fs::write(
            kb.dashboard_root().join("embedding-reindex.json"),
            value.to_string(),
        )
        .unwrap();
    }

    fn touch_lock(kb: &KnowledgeBase) {
        std::fs::create_dir_all(kb.dashboard_root()).unwrap();
        std::fs::write(kb.dashboard_root().join("embedding.lock"), b"").unwrap();
    }

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

        // 09-09:正文里出现 config / memory / 配置 / 记忆 不再是拒收理由。
        // 旧闸拿这几个词扫全文,WebUI 手动上传的资料十有八九都被它挡在门外。
        assert_eq!(
            kb.dashboard_import(
                "notes/macos.md",
                "装完 Homebrew 改 ~/.config/fish/config.fish;memory pressure 高时先关它。"
                    .as_bytes()
            )
            .unwrap(),
            "notes/macos.md"
        );
        // 守卫:Miyu 自己的资产目录不进库;非法类型不进库;路径不能逃逸。
        assert!(kb
            .dashboard_import("personas/miyu.md", "她是谁".as_bytes())
            .is_err());
        assert!(kb.dashboard_import("bin.exe", b"hello").is_err());
        assert!(kb.dashboard_import("../escape.md", b"hello").is_err());
        assert!(kb.dashboard_import("bad.md", &[0xff, 0xfe]).is_err());

        kb.dashboard_remove("notes/macos.md").unwrap();
        kb.dashboard_remove("notes/arch.md").unwrap();
        assert_eq!(kb.dashboard_overview().unwrap()["file_count"], 0);
        assert!(kb.dashboard_remove("notes/arch.md").is_err());

        // 嵌入关着:重建拒绝,状态 configured=false。
        assert!(kb.dashboard_reindex().is_err());
        assert_eq!(kb.dashboard_reindex_status().unwrap()["configured"], false);
        assert!(!kb.dashboard_clear_stale_lock().unwrap());
    }
    /// 09-09 用户实拍那一幕的回归:一整份 wiki 拖进来、点「重建语义索引」、
    /// 「待重建」纹丝不动、重建卡瞬间回到「空闲」。
    ///
    /// 真凶不是嵌入本身——是**在跑的那趟开跑时就把文件清单定死了**,而此刻
    /// 点下去的这次请求原地蒸发(旧代码回一句 running 就完事)。等那趟跑完锁
    /// 一清,卡片回到「空闲」,刚传进来的几千个文件永远停在「未索引」,界面上
    /// 一点痕迹都没有。留一张重跑单,那趟收尾时会自己再走一遍。
    #[test]
    fn reindex_request_is_queued_instead_of_dropped_while_a_pass_runs() {
        let temp = tempfile::tempdir().unwrap();
        let paths = paths(&temp);
        let kb = KnowledgeBase::new(AppConfig::default(), paths).unwrap();
        kb.dashboard_import("notes/a.md", "hello world".as_bytes())
            .unwrap();
        assert_eq!(
            kb.dashboard_reindex_status().unwrap()["configured"],
            true,
            "内置模型没解析出来,这条测的前提就不成立"
        );
        touch_lock(&kb);
        write_progress(
            &kb,
            json!({ "phase": "running", "updated_at": now(), "total": 82, "done": 40 }),
        );

        let result = kb.dashboard_reindex().unwrap();

        assert_eq!(result["started"], false);
        assert_eq!(result["reason"], "queued");
        assert!(
            kb.dashboard_root().join("embedding-reindex.rerun").exists(),
            "没留重跑单:这批文件跑完也没人回头管"
        );
    }

    /// 子进程 exec 完就死(配置读坏、二进制被换掉)时,进度停在 starting 而锁
    /// 从来没建起来。此时再点重建**必须真起一个**:拿「正在启动」当在跑挡回去
    /// 的话,那张重跑单没人接,点击照样蒸发——第一版修完就踩了这个。
    #[test]
    fn reindex_starts_when_the_previous_child_died_before_taking_the_lock() {
        let temp = tempfile::tempdir().unwrap();
        let paths = paths(&temp);
        let kb = KnowledgeBase::new(AppConfig::default(), paths).unwrap();
        kb.dashboard_import("notes/a.md", "hello world".as_bytes())
            .unwrap();
        write_progress(
            &kb,
            json!({ "phase": "starting", "updated_at": now() - 120.0 }),
        );

        let status = kb.dashboard_reindex_status().unwrap();
        assert_eq!(status["running"], false, "起都没起来还报进行中");
        assert!(status["last_error"]
            .as_str()
            .unwrap()
            .contains("reindex process is gone"));

        // cfg(test) 下不会真起子进程(那会变成 fork 炸弹),这里要的是
        // 「走了 spawn 那条路而不是排队那条」。
        let result = kb.dashboard_reindex().unwrap();
        assert_eq!(result["started"], true);
    }

    /// 子进程从 exec 到建锁有几百毫秒。前端点完立刻刷新概览,旧状态只认锁,
    /// 于是用户看到的第一帧就是「空闲」——「点了没反应」的直接来源。
    /// 进度文件里的 starting/running 要能独立撑起「进行中」,并带出 done/total。
    #[test]
    fn reindex_status_shows_progress_before_the_lock_appears() {
        let temp = tempfile::tempdir().unwrap();
        let paths = paths(&temp);
        let kb = KnowledgeBase::new(AppConfig::default(), paths).unwrap();
        kb.dashboard_import("notes/a.md", "hello world".as_bytes())
            .unwrap();

        write_progress(&kb, json!({ "phase": "starting", "updated_at": now() }));
        let status = kb.dashboard_reindex_status().unwrap();
        assert_eq!(status["running"], true, "启动窗口里被报成空闲");
        assert_eq!(status["phase"], "starting");

        write_progress(
            &kb,
            json!({
                "phase": "running", "updated_at": now(), "started_at": now() - 12.0,
                "total": 6426, "done": 128, "indexed": 391, "current": "wiki/pacman.md",
            }),
        );
        let status = kb.dashboard_reindex_status().unwrap();
        assert_eq!(status["running"], true);
        assert_eq!(status["done"], 128);
        assert_eq!(status["total"], 6426);
        assert_eq!(status["indexed"], 391);
        assert_eq!(status["current"], "wiki/pacman.md");
        assert_eq!(status["last_error"], "", "没失败就别挂着旧报错");
    }

    /// 子进程死掉时旧代码什么都不留:输出丢 /dev/null、没人 wait、状态里也没有
    /// 「失败」这一态。现在失败原因(带日志尾巴)必须能被面板读到;而卡在半路
    /// 的锁不必再等满一小时——进度五分钟没动就是那个进程不在了。
    #[test]
    fn reindex_status_surfaces_failures_and_a_stalled_pass() {
        let temp = tempfile::tempdir().unwrap();
        let paths = paths(&temp);
        let kb = KnowledgeBase::new(AppConfig::default(), paths).unwrap();
        kb.dashboard_import("notes/a.md", "hello world".as_bytes())
            .unwrap();

        write_progress(
            &kb,
            json!({
                "phase": "failed", "updated_at": now(), "finished_at": now(),
                "total": 12, "done": 3, "failed": 3,
                "error": "reindex process exited: signal: 9 (SIGKILL)",
                "log_tail": "ONNX Runtime library not found",
                "last_file_error": "embedding worker exceeded its 20-second timeout",
            }),
        );
        let status = kb.dashboard_reindex_status().unwrap();
        assert_eq!(status["running"], false);
        assert_eq!(status["phase"], "failed");
        let error = status["last_error"].as_str().unwrap();
        assert!(error.contains("SIGKILL"), "{error}");
        assert!(error.contains("ONNX Runtime library not found"), "{error}");
        assert!(status["last_file_error"]
            .as_str()
            .unwrap()
            .contains("20-second timeout"));
        assert!(status["log_path"]
            .as_str()
            .unwrap()
            .ends_with("embedding-reindex.log"));

        // 锁还在、进度十分钟没动:进程已经不在了,不该再显示「进行中」。
        touch_lock(&kb);
        write_progress(
            &kb,
            json!({ "phase": "running", "updated_at": now() - 600.0, "total": 12, "done": 3 }),
        );
        let status = kb.dashboard_reindex_status().unwrap();
        assert_eq!(status["running"], false, "进程没了还在报进行中");
        assert_eq!(status["stale_lock"], true);

        assert!(kb.dashboard_clear_stale_lock().unwrap());
        let status = kb.dashboard_reindex_status().unwrap();
        assert_eq!(status["phase"], "failed", "清完锁进度还举着半截进度条");
        assert_eq!(status["running"], false);
    }
}
