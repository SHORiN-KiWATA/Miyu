//! 库的目录、元数据与文件读写。
//!
//! `normalize_relative_path` 与 `ensure_enabled` 是两道边界：路径来自模型，
//! 必须限制在库根之内；工具在未启用时一律拒绝而不是静默返回空。

use crate::tools::knowledge_base::*;

#[derive(Clone)]
pub struct FileRecord {
    pub name: String,
    pub(in crate::tools::knowledge_base) path: String,
    pub size_bytes: i64,
    pub(in crate::tools::knowledge_base) content_sha256: String,
}

#[derive(Debug)]
pub struct EditResult {
    pub(in crate::tools::knowledge_base) path: String,
    pub(in crate::tools::knowledge_base) old_line_count: usize,
    pub(in crate::tools::knowledge_base) new_line_count: usize,
    pub(in crate::tools::knowledge_base) semantic_refreshed: bool,
}

/// 知识库只收参考资料，不收 Miyu 自己的东西（技能文件、人格提示词、配置、
/// 记忆库）。
///
/// 这道闸原先是拿 `skill` / `memory` / `prompt` / `config` / `记忆` / `配置`
/// 这几个词去扫**整篇正文**。于是一篇讲内存管理的文档、任何出现过 config 的
/// 教程、任何带「配置」二字的中文资料，统统进不来——闸门比它要拦的东西宽了
/// 好几个数量级，而正经资料里出现这些词才是常态（09-09 用户报的就是这个：
/// WebUI 手动上传和模型用 `kb` 写入都会被挡）。
///
/// 现在只认两样不会误伤的证据：落点路径是不是 Miyu 自己的资产目录，以及正文
/// 是不是一份带明确标记的技能文件。正文里出现什么词一概不管。
pub(in crate::tools) fn reject_non_kb_upload(
    content: &str,
    title: &str,
    file_name: &str,
) -> Result<()> {
    let path = format!("{file_name}/{title}").to_ascii_lowercase();
    let is_miyu_asset_path = path.split('/').any(|segment| {
        matches!(
            segment.trim(),
            "skill.md" | "skills" | "persona" | "personas" | "config.toml" | "config.json"
        )
    }) || path.contains("memory.db")
        || path.contains("conversation.db");
    if is_miyu_asset_path {
        bail!("that path is where Miyu keeps its own skills, persona, or config; the knowledge base only takes reference documents")
    }
    if crate::skills::is_generated_skill(content) {
        bail!("this file is a skill definition; publish it with manage_skill instead of putting it in the knowledge base")
    }
    Ok(())
}

pub(in crate::tools::knowledge_base) fn ensure_enabled(config: &AppConfig) -> Result<()> {
    if !config.plugins.knowledge_base.enabled {
        bail!("knowledge base plugin is disabled")
    }
    Ok(())
}

pub(in crate::tools::knowledge_base) fn init_meta_db(conn: &Connection) -> Result<()> {
    conn.execute(
        "CREATE TABLE IF NOT EXISTS files (name TEXT PRIMARY KEY, path TEXT NOT NULL, size_bytes INTEGER NOT NULL, mtime REAL NOT NULL, content_sha256 TEXT NOT NULL, updated_at REAL NOT NULL)",
        [],
    )?;
    Ok(())
}

pub(in crate::tools::knowledge_base) fn init_semantic_db(conn: &Connection) -> Result<()> {
    conn.execute(
        "CREATE TABLE IF NOT EXISTS semantic_chunks (id INTEGER PRIMARY KEY AUTOINCREMENT, provider_id TEXT NOT NULL, model TEXT NOT NULL, file_name TEXT NOT NULL, content_sha256 TEXT NOT NULL, chunk_index INTEGER NOT NULL, start_char INTEGER NOT NULL, end_char INTEGER NOT NULL, text TEXT NOT NULL, embedding_json TEXT NOT NULL, created_at REAL NOT NULL)",
        [],
    )?;
    conn.execute("CREATE INDEX IF NOT EXISTS idx_semantic_file ON semantic_chunks(file_name, content_sha256)", [])?;
    // 09-05: vectors moved from JSON text to f32 BLOBs (136 MB → 43 MB for a
    // 10k-chunk library, and no per-query parse). Legacy rows keep their JSON
    // until the next reindex rewrites them.
    let has_blob_column = conn
        .prepare("SELECT embedding FROM semantic_chunks LIMIT 0")
        .is_ok();
    if !has_blob_column {
        conn.execute("ALTER TABLE semantic_chunks ADD COLUMN embedding BLOB", [])?;
    }
    Ok(())
}

pub(in crate::tools::knowledge_base) fn kb_root(
    config: &KnowledgeBasePluginConfig,
    paths: &MiyuPaths,
) -> PathBuf {
    let configured = config.data_dir.trim();
    if configured.is_empty() {
        paths.data_dir.join("kb")
    } else {
        expand_path(configured)
    }
}

/// 成员的知识库在自己家目录 `home/<用户>/kb`(「开知识库」= 自己的资料库);
/// 管理员/终端按插件配置。
pub(in crate::tools::knowledge_base) fn kb_root_for(
    config: &crate::config::AppConfig,
    paths: &MiyuPaths,
) -> PathBuf {
    match config.member_home_dir() {
        Some(home) => home.join("kb"),
        None => kb_root(&config.plugins.knowledge_base, paths),
    }
}

pub(in crate::tools::knowledge_base) fn normalize_relative_path(value: &str) -> Result<String> {
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

pub(in crate::tools::knowledge_base) fn collect_files(root: &Path) -> Result<Vec<PathBuf>> {
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

pub(in crate::tools::knowledge_base) fn split_csv(value: &str) -> HashSet<String> {
    value
        .split(',')
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| value.to_ascii_lowercase())
        .collect()
}

pub(in crate::tools::knowledge_base) fn file_name(path: &str) -> String {
    path.rsplit('/').next().unwrap_or(path).to_string()
}

pub(in crate::tools::knowledge_base) fn directory_name(path: &str) -> String {
    path.rsplit_once('/')
        .map(|(dir, _)| dir.to_string())
        .unwrap_or_default()
}

pub(in crate::tools::knowledge_base) fn compact_whitespace(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

pub(in crate::tools::knowledge_base) fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

pub(in crate::tools::knowledge_base) fn now_secs() -> f64 {
    unix_time(SystemTime::now())
}

pub(in crate::tools::knowledge_base) fn unix_time(time: SystemTime) -> f64 {
    time.duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
}

pub(in crate::tools::knowledge_base) fn expand_path(value: &str) -> PathBuf {
    if let Some(rest) = value.trim().strip_prefix("~/") {
        if let Some(home) = directories::BaseDirs::new().map(|dirs| dirs.home_dir().to_path_buf()) {
            return home.join(rest);
        }
    }
    PathBuf::from(value.trim())
}

pub(in crate::tools::knowledge_base) fn slug(value: &str) -> String {
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
