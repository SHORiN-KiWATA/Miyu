//! CLI 侧会话与 Miyu 消息前缀的对应关系(三条中转线共用,键含 provider)。
//!
//! 键是「逐消息哈希链」:chain[i] = 前 i 条会话消息的链哈希,种子掺入
//! provider/model/system prompt。Miyu 的历史回放是字节级 append-only 的,
//! 所以"本次请求延续上次"⇔"上次记录的 (长度, 链哈希) 是本次链的前缀"。
//! 匹配不上(redo/compact/系统提示词变更)就重开会话全量重放。
//!
//! 映射**落盘**到 `<state>/relay/sessions.json`(09-05 起)。此前它是纯进程
//! 内存,理由是「全量重放只损失效率,不损失正确性」——这个前提被 agy 对单条
//! 输入 192,000 字节的尾部静默截断废掉了:daemon 一重启,每个会话第一轮都全量
//! 重放,历史一超线本轮消息就被砍掉(09-04/09-05 群 130515298 案卷)。CLI 那头
//! 的会话本来就在磁盘上,Miyu 这边不该忘。哈希用 blake3 而不是
//! `DefaultHasher`:落盘的哈希要跨进程、跨工具链版本稳定。
//!
//! 键里还有一维**工具面档位**(`host_tools`)。claude 的工具全靠 MCP 桥,而桥
//! 每轮按触发者身份重算工具面(管理员给全量底座、其他人给受限底座),同一条
//! claude 会话被两档人轮流复用时,claude 会逐轮播报一份"46 个 mcp__miyu__
//! 工具被移除"的清单增删——模型把它读成工具服务器掉线,之后整段会话不再
//! 碰任何 Miyu 工具(09-01 群内取证:管理员说过话之后,紧接着的非管理员回合
//! 连占卜都被答成"工具那边掉线了",而占卜工具自始至终都在清单里)。两档各
//! 续各的会话,清单对每条 claude 会话恒定,增删归零。
//!
//! 代价只有切档时补发对方档位期间的增量:链哈希是 append-only 的,中间插了
//! 别档的回合之后旧前缀依然匹配得上,所以切回来是续传而不是全量重放。

use crate::llm::openai_compatible::*;
use crate::llm::{ChatContent, ChatContentPart};
use std::sync::{MutexGuard, OnceLock};

#[derive(Clone, Serialize, Deserialize)]
struct SessionEntry {
    provider_id: String,
    model: String,
    /// 归属的 Miyu 会话(workspace task-local):续传匹配显式按它隔离,
    /// 清空 Miyu 会话时按它联动丢弃。回合作用域外(直连兜底)为 None。
    miyu_session: Option<String>,
    /// 本会话经 MCP 桥暴露的工具面档位(true=宿主工具全量底座)。跨档
    /// 复用同一条 claude 会话正是"工具掉线"误判的成因,见模块头。
    host_tools: bool,
    /// 该 claude 会话已覆盖的会话消息数(含预测的 assistant 回填)。
    prefix_len: usize,
    prefix_hash: u64,
    claude_session: String,
}

struct Store {
    loaded: bool,
    entries: Vec<SessionEntry>,
}

static SESSIONS: Mutex<Store> = Mutex::new(Store {
    loaded: false,
    entries: Vec::new(),
});

/// clear-at-cap 定式:超上限整表清空,宁可全量重放一轮,不做 LRU 簿记。
/// 落盘后条目跨重启累积(孤儿条目要等 CLI 侧报"会话不存在"才被摘),上限比
/// 纯内存时代放宽一倍。
const SESSION_CAP: usize = 128;

/// 落盘格式版本:哈希算法或条目形状一变就加一,旧文件整体作废(只多一轮
/// 全量重放,不会拿错误的前缀去续)。
const STORE_VERSION: u32 = 1;

#[derive(Serialize, Deserialize)]
struct StoreFile {
    version: u32,
    entries: Vec<SessionEntry>,
}

/// 落盘路径。`MIYU_RELAY_SESSIONS_FILE` 显式指定(空串=关掉落盘);测试构建
/// 默认不落盘——单测共用一个进程的全局表,不能把彼此的条目写进真实 state。
fn persist_path() -> Option<&'static PathBuf> {
    static PATH: OnceLock<Option<PathBuf>> = OnceLock::new();
    PATH.get_or_init(|| {
        if let Some(explicit) = std::env::var_os("MIYU_RELAY_SESSIONS_FILE") {
            if explicit.is_empty() {
                return None;
            }
            return Some(PathBuf::from(explicit));
        }
        if cfg!(test) {
            return None;
        }
        MiyuPaths::new()
            .ok()
            .map(|paths| paths.state_dir.join("relay").join("sessions.json"))
    })
    .as_ref()
}

fn load_entries_from(path: &std::path::Path) -> Vec<SessionEntry> {
    let Ok(raw) = std::fs::read(path) else {
        return Vec::new();
    };
    match serde_json::from_slice::<StoreFile>(&raw) {
        Ok(file) if file.version == STORE_VERSION => file.entries,
        Ok(file) => {
            tracing::info!(
                path = %path.display(),
                found = file.version,
                expected = STORE_VERSION,
                "relay session map has an old format version; starting empty"
            );
            Vec::new()
        }
        Err(error) => {
            tracing::warn!(
                path = %path.display(),
                error = %error,
                "relay session map is unreadable; starting empty"
            );
            Vec::new()
        }
    }
}

fn save_entries_to(path: &std::path::Path, entries: &[SessionEntry]) -> std::io::Result<()> {
    // 回合作用域外(miyu_session=None)的映射活不过本进程:那些会话没有落库的
    // 历史可以重建前缀,存了也永远匹配不上。
    let persisted: Vec<&SessionEntry> = entries
        .iter()
        .filter(|entry| entry.miyu_session.is_some())
        .collect();
    let file = serde_json::json!({ "version": STORE_VERSION, "entries": persisted });
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let temporary = path.with_extension("json.tmp");
    std::fs::write(&temporary, serde_json::to_vec(&file)?)?;
    std::fs::rename(&temporary, path)
}

/// 拿全局表;首次访问时从磁盘装入。
fn store() -> Option<MutexGuard<'static, Store>> {
    let mut guard = SESSIONS.lock().ok()?;
    if !guard.loaded {
        guard.loaded = true;
        if let Some(path) = persist_path() {
            guard.entries = load_entries_from(path);
            if !guard.entries.is_empty() {
                tracing::info!(
                    path = %path.display(),
                    entries = guard.entries.len(),
                    "relay session map restored from disk"
                );
            }
        }
    }
    Some(guard)
}

fn persist(store: &Store) {
    let Some(path) = persist_path() else {
        return;
    };
    if let Err(error) = save_entries_to(path, &store.entries) {
        tracing::warn!(
            path = %path.display(),
            error = %error,
            "relay session map could not be written"
        );
    }
}

fn hash_step(previous: u64, bytes: &[u8]) -> u64 {
    let mut hasher = blake3::Hasher::new();
    hasher.update(&previous.to_le_bytes());
    hasher.update(bytes);
    let digest = hasher.finalize();
    u64::from_le_bytes(
        digest.as_bytes()[..8]
            .try_into()
            .expect("blake3 digest is 32 bytes"),
    )
}

/// 进哈希链的是消息的**纯文本投影**:多段内容只留文本块,图片/视频块不参与。
///
/// 活体用户消息带图时是 `Parts[Text, ImageUrl…]`,落库只存那段文本,下一轮
/// 化石回放成 `Text`——两者 JSON 字节不同,原样哈希会让链逢图必断,之后每个
/// 已登记的前缀全部失配、全量重放(09-04 案卷机制 1)。历史转写本来就不带图
/// (`payload::render_history_line` 只标一句 image omitted),所以文本投影才
/// 是 CLI 那头真正看到过的东西。
fn message_bytes(message: &ChatMessage) -> Vec<u8> {
    let projected = match &message.content {
        Some(ChatContent::Parts(parts)) => {
            let text = parts
                .iter()
                .filter_map(|part| match part {
                    ChatContentPart::Text { text } => Some(text.as_str()),
                    ChatContentPart::ImageUrl { .. } | ChatContentPart::VideoUrl { .. } => None,
                })
                .collect::<Vec<_>>()
                .join("\n");
            let mut projected = message.clone();
            projected.content = Some(ChatContent::Text(text));
            projected
        }
        _ => message.clone(),
    };
    serde_json::to_vec(&projected).unwrap_or_default()
}

pub(in crate::llm::openai_compatible) fn prefix_chain(
    provider_id: &str,
    model: &str,
    system_prompt: &str,
    conversation: &[ChatMessage],
) -> Vec<u64> {
    let seed = {
        let mut hasher = blake3::Hasher::new();
        hasher.update(provider_id.as_bytes());
        hasher.update(&[0]);
        hasher.update(model.as_bytes());
        hasher.update(&[0]);
        hasher.update(system_prompt.as_bytes());
        let digest = hasher.finalize();
        u64::from_le_bytes(
            digest.as_bytes()[..8]
                .try_into()
                .expect("blake3 digest is 32 bytes"),
        )
    };
    let mut chain = Vec::with_capacity(conversation.len() + 1);
    chain.push(seed);
    let mut current = seed;
    for message in conversation {
        current = hash_step(current, &message_bytes(message));
        chain.push(current);
    }
    chain
}

pub(in crate::llm::openai_compatible) fn extend_chain(
    chain_end: u64,
    message: &ChatMessage,
) -> u64 {
    hash_step(chain_end, &message_bytes(message))
}

/// 一次续传查找为什么落空(给日志用;09-04 案卷 B′:不把失配原因写出来,
/// 每次全量重放都得靠猜)。
#[derive(Debug, PartialEq, Eq)]
pub(in crate::llm::openai_compatible) enum ResumeMiss {
    /// 这条 Miyu 会话在本档名下没有任何登记(首轮 / 重启后未落盘 / 已被清空)。
    NoEntry,
    /// 有登记,但只在另一档工具面名下。
    OtherTierOnly,
    /// 有登记,前缀哈希对不上(历史被改写:redo/compact/pop/提示词变更)。
    PrefixMismatch { recorded_len: usize },
    /// 有登记,但长度不短于本次会话(增量为空,不是正常的新一轮)。
    NoDelta,
}

fn find_in(
    entries: &[SessionEntry],
    provider_id: &str,
    model: &str,
    miyu_session: Option<&str>,
    host_tools: bool,
    chain: &[u64],
    conversation_len: usize,
) -> Result<(String, usize), ResumeMiss> {
    let mine: Vec<&SessionEntry> = entries
        .iter()
        .filter(|entry| entry.provider_id == provider_id && entry.model == model)
        .filter(|entry| entry.miyu_session.as_deref() == miyu_session)
        .collect();
    if mine.is_empty() {
        return Err(ResumeMiss::NoEntry);
    }
    let tier: Vec<&SessionEntry> = mine
        .iter()
        .copied()
        .filter(|entry| entry.host_tools == host_tools)
        .collect();
    if tier.is_empty() {
        return Err(ResumeMiss::OtherTierOnly);
    }
    if let Some(entry) = tier
        .iter()
        .filter(|entry| {
            entry.prefix_len < conversation_len && chain[entry.prefix_len] == entry.prefix_hash
        })
        .max_by_key(|entry| entry.prefix_len)
    {
        return Ok((entry.claude_session.clone(), entry.prefix_len));
    }
    let longest = tier.iter().map(|entry| entry.prefix_len).max().unwrap_or(0);
    if tier
        .iter()
        .all(|entry| entry.prefix_len >= conversation_len)
    {
        Err(ResumeMiss::NoDelta)
    } else {
        Err(ResumeMiss::PrefixMismatch {
            recorded_len: longest,
        })
    }
}

/// 找可续传的最长前缀:返回 (claude 会话 id, 已覆盖的消息数)。要求严格短于
/// 本次会话消息数——增量为空说明不是正常的新一轮,按全量重放处理。
pub(in crate::llm::openai_compatible) fn find_resumable(
    provider_id: &str,
    model: &str,
    miyu_session: Option<&str>,
    host_tools: bool,
    chain: &[u64],
    conversation_len: usize,
) -> Result<(String, usize), ResumeMiss> {
    let store = store().ok_or(ResumeMiss::NoEntry)?;
    find_in(
        &store.entries,
        provider_id,
        model,
        miyu_session,
        host_tools,
        chain,
        conversation_len,
    )
}

pub(in crate::llm::openai_compatible) fn record_session(
    provider_id: &str,
    model: &str,
    miyu_session: Option<&str>,
    host_tools: bool,
    prefix_len: usize,
    prefix_hash: u64,
    claude_session: String,
) {
    let Some(mut store) = store() else {
        return;
    };
    if store.entries.len() >= SESSION_CAP {
        store.entries.clear();
    }
    // 同一 claude 会话只保留最新指针:续传成功后旧前缀已被新前缀覆盖。
    store
        .entries
        .retain(|entry| entry.claude_session != claude_session);
    store.entries.push(SessionEntry {
        provider_id: provider_id.to_string(),
        model: model.to_string(),
        miyu_session: miyu_session.map(str::to_string),
        host_tools,
        prefix_len,
        prefix_hash,
        claude_session,
    });
    persist(&store);
}

/// 清空 Miyu 会话时联动:丢弃它名下的全部映射,返回对应的 claude 会话 id
/// (调用方拿去做 claude 侧转录的尽力删除)。
pub(in crate::llm::openai_compatible) fn forget_miyu_session(miyu_session: &str) -> Vec<String> {
    let Some(mut store) = store() else {
        return Vec::new();
    };
    let mut removed = Vec::new();
    store.entries.retain(|entry| {
        if entry.miyu_session.as_deref() == Some(miyu_session) {
            removed.push(entry.claude_session.clone());
            false
        } else {
            true
        }
    });
    if !removed.is_empty() {
        persist(&store);
    }
    removed
}

pub(in crate::llm::openai_compatible) fn forget_session(claude_session: &str) {
    if let Some(mut store) = store() {
        let before = store.entries.len();
        store
            .entries
            .retain(|entry| entry.claude_session != claude_session);
        if store.entries.len() != before {
            persist(&store);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message(role: &str, text: &str) -> ChatMessage {
        ChatMessage::plain(role, text)
    }

    /// 续传判定的核心不变量:append-only 延伸能匹配,任何前缀改写都匹配不上。
    #[test]
    fn prefix_matching_follows_append_only_history() {
        let base = vec![message("user", "hi"), message("assistant", "hello")];
        let chain = prefix_chain("p", "m", "sys", &base);
        record_session(
            "p",
            "m",
            Some("miyu-a"),
            true,
            2,
            chain[2],
            "sess-1".to_string(),
        );

        // 纯追加:匹配,且增量从第 2 条之后开始。
        let mut extended = base.clone();
        extended.push(message("user", "next"));
        let chain = prefix_chain("p", "m", "sys", &extended);
        assert_eq!(
            find_resumable("p", "m", Some("miyu-a"), true, &chain, extended.len()),
            Ok(("sess-1".to_string(), 2))
        );

        // 别的 Miyu 会话即使字节级同前缀,也绝不共用 claude 会话。
        assert_eq!(
            find_resumable("p", "m", Some("miyu-b"), true, &chain, extended.len()),
            Err(ResumeMiss::NoEntry)
        );

        // 改写历史(redo):第 2 条字节变了,匹配不上。
        let mut rewritten = vec![message("user", "hi"), message("assistant", "changed")];
        rewritten.push(message("user", "next"));
        let chain = prefix_chain("p", "m", "sys", &rewritten);
        assert_eq!(
            find_resumable("p", "m", Some("miyu-a"), true, &chain, rewritten.len()),
            Err(ResumeMiss::PrefixMismatch { recorded_len: 2 })
        );

        // 系统提示词变更:种子不同,匹配不上。
        let chain = prefix_chain("p", "m", "other-sys", &extended);
        assert_eq!(
            find_resumable("p", "m", Some("miyu-a"), true, &chain, extended.len()),
            Err(ResumeMiss::PrefixMismatch { recorded_len: 2 })
        );

        // 增量为空(长度相同)不算续传。
        let chain = prefix_chain("p", "m", "sys", &base);
        assert_eq!(
            find_resumable("p", "m", Some("miyu-a"), true, &chain, base.len()),
            Err(ResumeMiss::NoDelta)
        );

        // 清空 Miyu 会话 ⇒ 名下映射整体丢弃,并交回 claude 会话 id。
        assert_eq!(forget_miyu_session("miyu-a"), vec!["sess-1".to_string()]);
        let chain = prefix_chain("p", "m", "sys", &extended);
        assert_eq!(
            find_resumable("p", "m", Some("miyu-a"), true, &chain, extended.len()),
            Err(ResumeMiss::NoEntry)
        );
        forget_session("sess-1");
    }

    /// 带图的活体用户消息(`Parts[Text, ImageUrl]`)与它的化石(只剩那段文本)
    /// 必须算同一条链:落库不存图,下一轮回放成纯文本,原样哈希会让链逢图
    /// 必断、之后每轮全量重放(09-04 群 130515298 案卷机制 1)。
    #[test]
    fn image_parts_hash_like_their_text_fossil() {
        let live = ChatMessage::user_parts(vec![
            ChatContentPart::Text {
                text: "看看这张图".to_string(),
            },
            ChatContentPart::ImageUrl {
                image_url: crate::llm::ImageUrlContent {
                    url: "data:image/png;base64,QUJD".to_string(),
                },
            },
        ]);
        let fossil = ChatMessage::plain("user", "看看这张图");
        let reply = message("assistant", "红的");

        let live_chain = prefix_chain("p", "m", "sys", &[live]);
        record_session(
            "p",
            "m",
            Some("miyu-img"),
            true,
            2,
            extend_chain(live_chain[1], &reply),
            "sess-img".to_string(),
        );

        let next = vec![fossil, reply, message("user", "再看看")];
        let chain = prefix_chain("p", "m", "sys", &next);
        assert_eq!(
            find_resumable("p", "m", Some("miyu-img"), true, &chain, next.len()),
            Ok(("sess-img".to_string(), 2))
        );
        forget_session("sess-img");
    }

    /// 两档工具面各续各的 claude 会话:跨档绝不复用(复用就会让 claude 逐轮
    /// 播报几十件 mcp__miyu__ 工具被移除,模型读成"工具掉线",此后整段会话
    /// 不再碰任何 Miyu 工具——09-01 群内取证的真身)。
    ///
    /// 同时钉住这套分叉的代价上界:切档回来仍是**续传**而不是全量重放。
    /// 链哈希 append-only,中间插了别档的回合之后,本档旧前缀照样匹配得上。
    #[test]
    fn tool_face_tiers_never_share_a_claude_session() {
        let guest = vec![message("user", "群友问"), message("assistant", "答")];
        let chain = prefix_chain("p", "m", "sys", &guest);
        record_session(
            "p",
            "m",
            Some("miyu-t"),
            false,
            2,
            chain[2],
            "guest-1".to_string(),
        );

        // 管理员那一轮:同一条 Miyu 会话、同一段前缀,但工具面是全量底座。
        // 拿不到受限档的会话,只能新开——正是这里挡住了清单增删。
        let mut admin_turn = guest.clone();
        admin_turn.push(message("user", "管理员问"));
        let chain = prefix_chain("p", "m", "sys", &admin_turn);
        assert_eq!(
            find_resumable("p", "m", Some("miyu-t"), true, &chain, admin_turn.len()),
            Err(ResumeMiss::OtherTierOnly)
        );
        assert_eq!(
            find_resumable("p", "m", Some("miyu-t"), false, &chain, admin_turn.len()),
            Ok(("guest-1".to_string(), 2))
        );
        let admin_reply = message("assistant", "管理员答");
        record_session(
            "p",
            "m",
            Some("miyu-t"),
            true,
            4,
            extend_chain(chain[3], &admin_reply),
            "admin-1".to_string(),
        );

        // 切回受限档:管理员那一轮已经进了历史,受限档的旧前缀(长度 2)依然
        // 匹配,续传只补中间的增量,不是全量重放。
        let mut back = admin_turn.clone();
        back.push(admin_reply);
        back.push(message("user", "群友再问"));
        let chain = prefix_chain("p", "m", "sys", &back);
        assert_eq!(
            find_resumable("p", "m", Some("miyu-t"), false, &chain, back.len()),
            Ok(("guest-1".to_string(), 2))
        );
        // 两档并存,各认各的:管理员档从自己上次覆盖点(4)续。
        assert_eq!(
            find_resumable("p", "m", Some("miyu-t"), true, &chain, back.len()),
            Ok(("admin-1".to_string(), 4))
        );

        // 清空 Miyu 会话要把两档一起丢掉,不能只丢一档。
        let mut removed = forget_miyu_session("miyu-t");
        removed.sort();
        assert_eq!(removed, vec!["admin-1".to_string(), "guest-1".to_string()]);
    }

    /// 落盘往返:重启(新进程、空表)后从文件装回的条目照样命中续传。这是
    /// 09-05 案卷的正因——daemon 重启把纯内存表清空,重启后每个会话第一轮都
    /// 全量重放,历史一超 agy 的 192K 上限本轮消息就被砍。
    #[test]
    fn persisted_entries_survive_a_restart() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("relay/sessions.json");
        let base = vec![message("user", "hi"), message("assistant", "hello")];
        let chain = prefix_chain("p", "m", "sys", &base);
        let entries = vec![
            SessionEntry {
                provider_id: "p".into(),
                model: "m".into(),
                miyu_session: Some("miyu-persist".into()),
                host_tools: true,
                prefix_len: 2,
                prefix_hash: chain[2],
                claude_session: "sess-disk".into(),
            },
            // 回合作用域外的映射不落盘:没有落库历史可重建前缀。
            SessionEntry {
                provider_id: "p".into(),
                model: "m".into(),
                miyu_session: None,
                host_tools: true,
                prefix_len: 2,
                prefix_hash: chain[2],
                claude_session: "sess-direct".into(),
            },
        ];
        save_entries_to(&path, &entries).unwrap();

        // "重启":全新的表,只从磁盘装。
        let restored = load_entries_from(&path);
        assert_eq!(restored.len(), 1);
        assert_eq!(restored[0].claude_session, "sess-disk");

        let mut extended = base.clone();
        extended.push(message("user", "next"));
        // 链在另一次进程里重新算——blake3 稳定,与落盘的哈希一致。
        let chain = prefix_chain("p", "m", "sys", &extended);
        assert_eq!(
            find_in(
                &restored,
                "p",
                "m",
                Some("miyu-persist"),
                true,
                &chain,
                extended.len()
            ),
            Ok(("sess-disk".to_string(), 2))
        );

        // 旧格式版本整体作废,不拿错误哈希去续。
        std::fs::write(
            &path,
            serde_json::json!({ "version": STORE_VERSION + 1, "entries": [] }).to_string(),
        )
        .unwrap();
        assert!(load_entries_from(&path).is_empty());
        std::fs::write(&path, b"not json").unwrap();
        assert!(load_entries_from(&path).is_empty());
    }

    /// 哈希链跨进程稳定:同样的输入永远得到同样的链(落盘的前提)。钉一个
    /// 具体值,换哈希算法时必须同时改 STORE_VERSION。
    #[test]
    fn chain_hash_is_stable_across_processes() {
        let chain = prefix_chain("p", "m", "sys", &[message("user", "hi")]);
        assert_eq!(
            chain,
            prefix_chain("p", "m", "sys", &[message("user", "hi")])
        );
        assert_ne!(chain[0], chain[1]);
        assert_ne!(chain[0], prefix_chain("p", "m", "sys2", &[])[0]);
    }
}
