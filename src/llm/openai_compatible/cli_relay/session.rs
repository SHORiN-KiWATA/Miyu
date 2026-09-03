//! CLI 侧会话与 Miyu 消息前缀的对应关系(进程内;三条中转线共用,键含 provider)。
//!
//! 键是「逐消息哈希链」:chain[i] = 前 i 条会话消息的链哈希,种子掺入
//! provider/model/system prompt。Miyu 的历史回放是字节级 append-only 的,
//! 所以"本次请求延续上次"⇔"上次记录的 (长度, 链哈希) 是本次链的前缀"。
//! 匹配不上(redo/compact/系统提示词变更/daemon 重启)就重开会话全量重放
//! ——只损失效率,不损失正确性,因此状态不做持久化。
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
use std::hash::{DefaultHasher, Hash, Hasher};

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

static SESSIONS: Mutex<Vec<SessionEntry>> = Mutex::new(Vec::new());

/// clear-at-cap 定式:超上限整表清空,宁可全量重放一轮,不做 LRU 簿记。
const SESSION_CAP: usize = 64;

fn hash_step(previous: u64, bytes: &[u8]) -> u64 {
    let mut hasher = DefaultHasher::new();
    previous.hash(&mut hasher);
    bytes.hash(&mut hasher);
    hasher.finish()
}

fn message_bytes(message: &ChatMessage) -> Vec<u8> {
    serde_json::to_vec(message).unwrap_or_default()
}

pub(in crate::llm::openai_compatible) fn prefix_chain(
    provider_id: &str,
    model: &str,
    system_prompt: &str,
    conversation: &[ChatMessage],
) -> Vec<u64> {
    let seed = {
        let mut hasher = DefaultHasher::new();
        provider_id.hash(&mut hasher);
        model.hash(&mut hasher);
        system_prompt.hash(&mut hasher);
        hasher.finish()
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

/// 找可续传的最长前缀:返回 (claude 会话 id, 已覆盖的消息数)。要求严格短于
/// 本次会话消息数——增量为空说明不是正常的新一轮,按全量重放处理。
pub(in crate::llm::openai_compatible) fn find_resumable(
    provider_id: &str,
    model: &str,
    miyu_session: Option<&str>,
    host_tools: bool,
    chain: &[u64],
    conversation_len: usize,
) -> Option<(String, usize)> {
    let sessions = SESSIONS.lock().ok()?;
    sessions
        .iter()
        .filter(|entry| entry.provider_id == provider_id && entry.model == model)
        .filter(|entry| entry.miyu_session.as_deref() == miyu_session)
        .filter(|entry| entry.host_tools == host_tools)
        .filter(|entry| {
            entry.prefix_len < conversation_len && chain[entry.prefix_len] == entry.prefix_hash
        })
        .max_by_key(|entry| entry.prefix_len)
        .map(|entry| (entry.claude_session.clone(), entry.prefix_len))
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
    let Ok(mut sessions) = SESSIONS.lock() else {
        return;
    };
    if sessions.len() >= SESSION_CAP {
        sessions.clear();
    }
    // 同一 claude 会话只保留最新指针:续传成功后旧前缀已被新前缀覆盖。
    sessions.retain(|entry| entry.claude_session != claude_session);
    sessions.push(SessionEntry {
        provider_id: provider_id.to_string(),
        model: model.to_string(),
        miyu_session: miyu_session.map(str::to_string),
        host_tools,
        prefix_len,
        prefix_hash,
        claude_session,
    });
}

/// 清空 Miyu 会话时联动:丢弃它名下的全部映射,返回对应的 claude 会话 id
/// (调用方拿去做 claude 侧转录的尽力删除)。
pub(in crate::llm::openai_compatible) fn forget_miyu_session(miyu_session: &str) -> Vec<String> {
    let Ok(mut sessions) = SESSIONS.lock() else {
        return Vec::new();
    };
    let mut removed = Vec::new();
    sessions.retain(|entry| {
        if entry.miyu_session.as_deref() == Some(miyu_session) {
            removed.push(entry.claude_session.clone());
            false
        } else {
            true
        }
    });
    removed
}

pub(in crate::llm::openai_compatible) fn forget_session(claude_session: &str) {
    if let Ok(mut sessions) = SESSIONS.lock() {
        sessions.retain(|entry| entry.claude_session != claude_session);
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
            Some(("sess-1".to_string(), 2))
        );

        // 别的 Miyu 会话即使字节级同前缀,也绝不共用 claude 会话。
        assert_eq!(
            find_resumable("p", "m", Some("miyu-b"), true, &chain, extended.len()),
            None
        );

        // 改写历史(redo):第 2 条字节变了,匹配不上。
        let mut rewritten = vec![message("user", "hi"), message("assistant", "changed")];
        rewritten.push(message("user", "next"));
        let chain = prefix_chain("p", "m", "sys", &rewritten);
        assert_eq!(
            find_resumable("p", "m", Some("miyu-a"), true, &chain, rewritten.len()),
            None
        );

        // 系统提示词变更:种子不同,匹配不上。
        let chain = prefix_chain("p", "m", "other-sys", &extended);
        assert_eq!(
            find_resumable("p", "m", Some("miyu-a"), true, &chain, extended.len()),
            None
        );

        // 增量为空(长度相同)不算续传。
        let chain = prefix_chain("p", "m", "sys", &base);
        assert_eq!(
            find_resumable("p", "m", Some("miyu-a"), true, &chain, base.len()),
            None
        );

        // 清空 Miyu 会话 ⇒ 名下映射整体丢弃,并交回 claude 会话 id。
        assert_eq!(forget_miyu_session("miyu-a"), vec!["sess-1".to_string()]);
        let chain = prefix_chain("p", "m", "sys", &extended);
        assert_eq!(
            find_resumable("p", "m", Some("miyu-a"), true, &chain, extended.len()),
            None
        );
        forget_session("sess-1");
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
            None
        );
        assert_eq!(
            find_resumable("p", "m", Some("miyu-t"), false, &chain, admin_turn.len()),
            Some(("guest-1".to_string(), 2))
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
            Some(("guest-1".to_string(), 2))
        );
        // 两档并存,各认各的:管理员档从自己上次覆盖点(4)续。
        assert_eq!(
            find_resumable("p", "m", Some("miyu-t"), true, &chain, back.len()),
            Some(("admin-1".to_string(), 4))
        );

        // 清空 Miyu 会话要把两档一起丢掉,不能只丢一档。
        let mut removed = forget_miyu_session("miyu-t");
        removed.sort();
        assert_eq!(removed, vec!["admin-1".to_string(), "guest-1".to_string()]);
    }
}
