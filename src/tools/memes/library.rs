//! 表情库的索引、查找与落盘。
//!
//! 内置库和用户库分开（`builtin_library_dir` / `user_library_dir`），用户库按人
//! 格分。查找支持短 ID（`unique_short_id_from_ids`）：完整哈希太长，模型引用时
//! 用前缀，只要在当前库里唯一就行。
//!
//! 写索引要加锁（`library_lock`）：两个回合同时收集表情会互相覆盖。

use crate::tools::memes::*;

pub(crate) const BUILTIN_MEMES_DIR: &str = "/usr/share/miyu/memes";

pub(crate) const MIN_SHORT_MEME_ID_LEN: usize = 7;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(crate) struct MemeIndex {
    #[serde(default)]
    pub(crate) library: String,
    #[serde(default)]
    pub(crate) version: u32,
    #[serde(default)]
    pub(crate) memes: Vec<MemeItem>,
    #[serde(default)]
    pub(crate) disabled_ids: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct MemeItem {
    pub(crate) id: String,
    pub(crate) name: LocalizedName,
    pub(crate) file: String,
    pub(crate) mime_type: String,
    #[serde(default)]
    pub(crate) animated: bool,
    pub(crate) description: String,
    pub(crate) usage: String,
    #[serde(default)]
    pub(crate) tags: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) origin: Option<MemeOrigin>,
}

/// 表情包的收集来源：从哪个平台会话、谁发的、什么时候发/收的。
/// 本地 add_meme 入库的表情没有该字段。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(crate) struct MemeOrigin {
    #[serde(default)]
    pub(crate) platform: String,
    #[serde(default)]
    pub(crate) conversation_kind: String,
    #[serde(default)]
    pub(crate) conversation_id: String,
    #[serde(default)]
    pub(crate) sender_id: String,
    #[serde(default)]
    pub(crate) sender_name: String,
    #[serde(default)]
    pub(crate) message_id: String,
    /// 消息发送时刻（RFC3339；平台未提供时为空）
    #[serde(default)]
    pub(crate) sent_at: String,
    /// 入库时刻（RFC3339）
    #[serde(default)]
    pub(crate) collected_at: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct LocalizedName {
    #[serde(default)]
    pub(crate) zh: String,
    #[serde(default)]
    pub(crate) en: String,
}

#[derive(Debug, Clone)]
pub(crate) struct LoadedMeme {
    pub(crate) item: MemeItem,
    pub(crate) path: PathBuf,
    pub(crate) source: MemeSource,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub(crate) enum MemeSource {
    Builtin,
    User,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MemeLibraryCacheKey {
    pub(crate) library: String,
    pub(crate) builtin_index: PathBuf,
    pub(crate) user_index: PathBuf,
    pub(crate) builtin_mtime: Option<SystemTime>,
    pub(crate) user_mtime: Option<SystemTime>,
}

#[derive(Debug, Clone)]
pub(crate) struct MemeLibraryCache {
    pub(crate) key: MemeLibraryCacheKey,
    pub(crate) memes: Vec<LoadedMeme>,
}

pub(crate) fn load_library(paths: &MiyuPaths, library: &str) -> Result<Vec<LoadedMeme>> {
    let builtin_dir = builtin_library_dir(library);
    let user_dir = user_library_dir(paths, library);
    let builtin_index = builtin_dir.join("index.json");
    let user_index = user_dir.join("index.json");
    let key = MemeLibraryCacheKey {
        library: sanitize_library(library),
        builtin_mtime: index_mtime(&builtin_index),
        user_mtime: index_mtime(&user_index),
        builtin_index: builtin_index.clone(),
        user_index: user_index.clone(),
    };
    let cache = MEME_LIBRARY_CACHE.get_or_init(|| RwLock::new(None));
    if let Some(cached) = cache.read().unwrap().as_ref() {
        if cached.key == key {
            return Ok(cached.memes.clone());
        }
    }
    let builtin = load_index(&builtin_index)?.unwrap_or_default();
    let user = load_index(&user_index)?.unwrap_or_default();
    let disabled = user.disabled_ids;
    let mut user_ids = Vec::new();
    let mut result = Vec::new();
    for item in user.memes {
        if disabled.iter().any(|id| ids_match(id, &item.id)) {
            continue;
        }
        user_ids.push(item.id.clone());
        result.push(LoadedMeme {
            path: user_dir.join(&item.file),
            item,
            source: MemeSource::User,
        });
    }
    for item in builtin.memes {
        if disabled.iter().any(|id| ids_match(id, &item.id))
            || user_ids.iter().any(|id| ids_match(id, &item.id))
        {
            continue;
        }
        result.push(LoadedMeme {
            path: builtin_dir.join(&item.file),
            item,
            source: MemeSource::Builtin,
        });
    }
    *cache.write().unwrap() = Some(MemeLibraryCache {
        key,
        memes: result.clone(),
    });
    Ok(result)
}

pub(crate) fn index_mtime(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path)
        .and_then(|meta| meta.modified())
        .ok()
}

pub(crate) fn find_meme(paths: &MiyuPaths, library: &str, id: &str) -> Result<Option<LoadedMeme>> {
    find_meme_in(load_library(paths, library)?, id)
}

/// 与 [`find_meme`] 相同的匹配规则,但不滤 disabled:重新启用
/// (`enabled=true`)必须能找到已禁用条目,否则禁用成了单向门。
/// 管理操作低频,不走库缓存。
pub(crate) fn find_meme_any(
    paths: &MiyuPaths,
    library: &str,
    id: &str,
) -> Result<Option<LoadedMeme>> {
    let builtin_dir = builtin_library_dir(library);
    let user_dir = user_library_dir(paths, library);
    let builtin = load_index(&builtin_dir.join("index.json"))?.unwrap_or_default();
    let user = load_index(&user_dir.join("index.json"))?.unwrap_or_default();
    let mut user_ids = Vec::new();
    let mut result = Vec::new();
    for item in user.memes {
        user_ids.push(item.id.clone());
        result.push(LoadedMeme {
            path: user_dir.join(&item.file),
            item,
            source: MemeSource::User,
        });
    }
    for item in builtin.memes {
        if user_ids.iter().any(|id| ids_match(id, &item.id)) {
            continue;
        }
        result.push(LoadedMeme {
            path: builtin_dir.join(&item.file),
            item,
            source: MemeSource::Builtin,
        });
    }
    find_meme_in(result, id)
}

pub(crate) fn find_meme_in(memes: Vec<LoadedMeme>, id: &str) -> Result<Option<LoadedMeme>> {
    let requested = id_hash_part(id);
    if requested.is_empty() {
        return Ok(None);
    }
    if !is_full_hash(requested) && requested.len() < MIN_SHORT_MEME_ID_LEN {
        bail!(
            "meme id prefix is too short: {requested}; use at least {MIN_SHORT_MEME_ID_LEN} hex characters"
        );
    }
    let mut matches = memes
        .into_iter()
        .filter(|meme| id_hash_part(&meme.item.id).starts_with(requested))
        .collect::<Vec<_>>();
    match matches.len() {
        0 => Ok(None),
        1 => Ok(matches.pop()),
        _ => bail!("meme id prefix is ambiguous: {requested}; use a longer id"),
    }
}

pub(crate) fn ids_match(stored: &str, requested: &str) -> bool {
    let stored = id_hash_part(stored);
    let requested = id_hash_part(requested);
    !requested.is_empty() && stored.starts_with(requested)
}

pub(crate) fn id_hash_part(value: &str) -> &str {
    let value = value.trim();
    value.strip_prefix("sha256:").unwrap_or(value)
}

pub(crate) fn is_full_hash(value: &str) -> bool {
    value.len() == 64 && value.chars().all(|ch| ch.is_ascii_hexdigit())
}

pub(crate) fn meme_ids(memes: &[LoadedMeme]) -> Vec<String> {
    memes.iter().map(|meme| meme.item.id.clone()).collect()
}

pub(crate) fn unique_short_id_from_ids(ids: &[String], id: &str) -> String {
    let hash = id_hash_part(id);
    if hash.len() <= MIN_SHORT_MEME_ID_LEN {
        return hash.to_string();
    }
    for len in MIN_SHORT_MEME_ID_LEN..=hash.len() {
        let prefix = &hash[..len];
        let matches = ids
            .iter()
            .filter(|candidate| id_hash_part(candidate).starts_with(prefix))
            .count();
        if matches <= 1 {
            return prefix.to_string();
        }
    }
    hash.to_string()
}

pub(crate) fn load_index(path: &Path) -> Result<Option<MemeIndex>> {
    if !path.is_file() {
        return Ok(None);
    }
    Ok(Some(serde_json::from_str(&std::fs::read_to_string(path)?)?))
}

pub(crate) fn save_index(path: &Path, index: &MemeIndex) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
        let mut temp = tempfile::NamedTempFile::new_in(parent)?;
        serde_json::to_writer_pretty(&mut temp, index)?;
        temp.write_all(b"\n")?;
        temp.as_file().sync_all()?;
        temp.persist(path)
            .map_err(|error| error.error)
            .with_context(|| format!("atomically replacing meme index {}", path.display()))?;
        return Ok(());
    }
    bail!("meme index path has no parent: {}", path.display())
}

pub(crate) fn selected_library(args: &Value, config: &AppConfig) -> String {
    args.get("library")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(sanitize_library)
        .unwrap_or_else(|| current_persona_library(config))
}

pub(crate) fn current_persona_library(config: &AppConfig) -> String {
    sanitize_library(
        &config
            .plugins
            .memes
            .library_for_persona(&config.prompt.active_persona),
    )
}

pub(crate) fn meme_ref_exists(paths: &MiyuPaths, meme: &MemeRef) -> Result<bool> {
    Ok(find_meme(paths, &meme.library, &meme.id)?.is_some())
}

pub(crate) async fn delete_meme_reference(
    meme: &MemeRef,
    config: &AppConfig,
    paths: &MiyuPaths,
) -> Result<()> {
    let result = delete_meme(
        json!({
            "library": meme.library,
            "id": meme.id,
            "hard_delete": false,
        }),
        config,
        paths,
    )
    .await?;
    let result: Value = serde_json::from_str(&result)?;
    if result.get("success").and_then(Value::as_bool) == Some(true) {
        Ok(())
    } else {
        bail!("meme deletion did not succeed")
    }
}

pub(crate) fn library_lock(library: &str) -> Arc<tokio::sync::Mutex<()>> {
    let key = sanitize_library(library);
    let mut locks = MEME_LIBRARY_LOCKS
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap();
    locks
        .entry(key)
        .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
        .clone()
}

pub(crate) fn sanitize_library(value: &str) -> String {
    let value = value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
                ch.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect::<String>()
        .trim_matches('-')
        .to_string();
    if value.is_empty() {
        "default".to_string()
    } else {
        value
    }
}

pub(crate) fn builtin_library_dir(library: &str) -> PathBuf {
    if let Some(path) = std::env::var_os("MIYU_MEMES_DIR") {
        return PathBuf::from(path).join(library);
    }
    let dev = PathBuf::from("src/memes").join(library);
    if dev.is_dir() {
        return dev;
    }
    PathBuf::from(BUILTIN_MEMES_DIR).join(library)
}

pub(crate) fn user_library_dir(paths: &MiyuPaths, library: &str) -> PathBuf {
    paths.data_dir.join("memes").join(sanitize_library(library))
}

pub(crate) fn score_meme(item: &MemeItem, query: &str, tags: &[String]) -> f32 {
    let query = normalize(&format!("{query} {}", tags.join(" ")));
    let terms = search_terms(&query);
    if terms.is_empty() {
        return 0.1;
    }
    let name = normalize(&format!("{} {}", item.name.zh, item.name.en));
    let description = normalize(&item.description);
    let usage = normalize(&item.usage);
    let tag_text = normalize(&item.tags.join(" "));
    let mut score: f32 = 0.0;
    for term in terms {
        if tag_text.contains(&term) {
            score += 3.0;
        }
        if name.contains(&term) {
            score += 2.5;
        }
        if usage.contains(&term) {
            score += 2.0;
        }
        if description.contains(&term) {
            score += 1.2;
        }
    }
    let haystack = format!("{name} {description} {usage} {tag_text}");
    if !query.is_empty() && haystack.contains(&query) {
        score += 2.0;
    }
    score.max(0.0)
}

pub(crate) fn search_terms(query: &str) -> Vec<String> {
    let mut terms = Vec::new();
    for token in query.split_whitespace() {
        if token.chars().count() > 1 {
            terms.push(token.to_string());
        }
        if token.chars().any(|ch| !ch.is_ascii()) {
            let chars = token.chars().collect::<Vec<_>>();
            for pair in chars.windows(2) {
                terms.push(pair.iter().collect());
            }
        }
    }
    terms.sort();
    terms.dedup();
    terms
}

pub(crate) fn normalize(value: &str) -> String {
    value
        .to_ascii_lowercase()
        .chars()
        .map(|ch| {
            if ch.is_ascii_punctuation()
                || matches!(
                    ch,
                    '，' | '。' | '！' | '？' | '、' | '；' | '：' | '（' | '）' | '“' | '”'
                )
            {
                ' '
            } else {
                ch
            }
        })
        .collect::<String>()
}

pub(crate) fn expand_path(value: &str) -> PathBuf {
    if let Some(rest) = value.trim().strip_prefix("~/") {
        if let Some(home) = directories::BaseDirs::new().map(|dirs| dirs.home_dir().to_path_buf()) {
            return home.join(rest);
        }
    }
    let path = Path::new(value.trim());
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        crate::tools::workspace::effective_workdir().join(path)
    }
}

pub(crate) fn source_label(source: MemeSource) -> &'static str {
    match source {
        MemeSource::Builtin => "builtin",
        MemeSource::User => "user",
    }
}
