//! WebUI dashboard 用的消息历史视图。
//!
//! 读走一条独立的只读连接(WAL 下与 actor 并发读没有冲突),库不存在按空返回、
//! 不建库;删除与上下文边界这类写走 `HistoryStore` 的 actor,与工具侧同一条路。

use super::store::{
    map_message, query_activity_ranking, ActivityRankingQuery, ConversationKey, DeleteMode,
    DeleteRequest, HistoryCursor, HistoryScope, HistoryStore, MESSAGE_COLUMNS,
};
pub(crate) use super::store::{ConversationKey as DashConversationKey, HistoryCursor as DashHistoryCursor};
use crate::paths::MiyuPaths;
use anyhow::{bail, Result};
use rusqlite::{params_from_iter, types::Value as SqlValue, Connection, OpenFlags};
use serde_json::{json, Value};
use std::path::PathBuf;

const PLATFORM: &str = "onebot";

pub(crate) fn dashboard_store(paths: &MiyuPaths) -> HistoryStore {
    super::store_for_paths(paths)
}

fn db_path(paths: &MiyuPaths) -> PathBuf {
    super::history_db_path(paths)
}

fn open_readonly(paths: &MiyuPaths) -> Result<Option<Connection>> {
    let path = db_path(paths);
    if !path.is_file() {
        return Ok(None);
    }
    let conn = Connection::open_with_flags(
        &path,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    conn.busy_timeout(std::time::Duration::from_secs(5))?;
    Ok(Some(conn))
}

pub(crate) fn conversation_key(account_id: &str, kind: &str, conversation_id: &str) -> Result<ConversationKey> {
    let kind = match kind {
        "group" => crate::platforms::ConversationKind::Group,
        "private" => crate::platforms::ConversationKind::Private,
        other => bail!("unknown conversation kind: {other}"),
    };
    ConversationKey::for_kind(PLATFORM, account_id, kind, conversation_id)
}

/// 会话清单:每个 (账号, 类型, 会话) 的消息数、首末时间、撤回数;附库文件大小。
pub(crate) fn dashboard_conversations(paths: &MiyuPaths, account_id: Option<&str>) -> Result<Value> {
    let path = db_path(paths);
    let db_bytes = std::fs::metadata(&path).map(|meta| meta.len()).unwrap_or(0)
        + std::fs::metadata(path.with_extension("sqlite3-wal")).map(|meta| meta.len()).unwrap_or(0);
    let Some(conn) = open_readonly(paths)? else {
        return Ok(json!({ "ok": true, "exists": false, "conversations": [], "accounts": [], "total_messages": 0, "db_bytes": 0 }));
    };
    let mut sql = String::from(
        "SELECT account_id, conversation_kind, conversation_id, COUNT(*), MIN(sent_at), MAX(sent_at),
                SUM(CASE WHEN recalled_at IS NOT NULL THEN 1 ELSE 0 END),
                SUM(CASE WHEN is_bot = 1 THEN 1 ELSE 0 END)
           FROM messages WHERE platform = ?1",
    );
    let mut args = vec![SqlValue::Text(PLATFORM.to_string())];
    if let Some(account) = account_id.filter(|value| !value.is_empty()) {
        sql.push_str(" AND account_id = ?2");
        args.push(SqlValue::Text(account.to_string()));
    }
    sql.push_str(" GROUP BY account_id, conversation_kind, conversation_id ORDER BY MAX(sent_at) DESC");
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt
        .query_map(params_from_iter(args.iter()), |row| {
            Ok(json!({
                "account_id": row.get::<_, String>(0)?,
                "kind": row.get::<_, String>(1)?,
                "id": row.get::<_, String>(2)?,
                "messages": row.get::<_, i64>(3)?,
                "first_at": row.get::<_, i64>(4)?,
                "last_at": row.get::<_, i64>(5)?,
                "recalled": row.get::<_, i64>(6)?,
                "bot_messages": row.get::<_, i64>(7)?,
            }))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let total: i64 = rows.iter().map(|row| row["messages"].as_i64().unwrap_or(0)).sum();
    let mut accounts: Vec<String> = rows
        .iter()
        .filter_map(|row| row["account_id"].as_str().map(str::to_string))
        .collect();
    accounts.sort();
    accounts.dedup();
    Ok(json!({
        "ok": true,
        "exists": true,
        "db_path": path.display().to_string(),
        "db_bytes": db_bytes,
        "total_messages": total,
        "accounts": accounts,
        "conversations": rows,
    }))
}

pub(crate) struct MessagesQuery {
    pub(crate) key: ConversationKey,
    pub(crate) text: String,
    pub(crate) sender_id: String,
    pub(crate) since: Option<i64>,
    pub(crate) until: Option<i64>,
    pub(crate) only_recalled: bool,
    pub(crate) only_media: bool,
    pub(crate) before: Option<HistoryCursor>,
    pub(crate) limit: usize,
}

/// 消息列表:游标向前翻(sent_at, row_id 双键),关键词 ≥3 字全走 trigram,
/// 否则 LIKE;结果新在前。
pub(crate) fn dashboard_messages(paths: &MiyuPaths, query: MessagesQuery) -> Result<Value> {
    let Some(conn) = open_readonly(paths)? else {
        return Ok(json!({ "ok": true, "items": [], "next_cursor": Value::Null }));
    };
    let limit = query.limit.clamp(1, 200);
    let terms = super::store::search_terms(&query.text)?;
    let use_fts = !terms.is_empty() && terms.iter().all(|term| term.chars().count() >= 3);
    let mut args = Vec::<SqlValue>::new();
    let mut conditions = Vec::<String>::new();
    let from = if use_fts {
        args.push(SqlValue::Text(super::store::build_fts_query(&terms)));
        conditions.push("messages_fts MATCH ?1".to_string());
        "messages_fts JOIN messages AS m ON m.id = messages_fts.rowid"
    } else {
        for term in &terms {
            args.push(SqlValue::Text(term.clone()));
            let n = args.len();
            conditions.push(format!(
                "(instr(lower(m.text), lower(?{n})) > 0 OR instr(lower(m.sender_name), lower(?{n})) > 0)"
            ));
        }
        "messages AS m"
    };
    for (column, value) in [
        ("m.platform", query.key.platform.clone()),
        ("m.account_id", query.key.account_id.clone()),
        ("m.conversation_kind", query.key.conversation_kind.clone()),
        ("m.conversation_id", query.key.conversation_id.clone()),
    ] {
        args.push(SqlValue::Text(value));
        conditions.push(format!("{column} = ?{}", args.len()));
    }
    let sender = query.sender_id.trim();
    if !sender.is_empty() {
        args.push(SqlValue::Text(sender.to_string()));
        conditions.push(format!("m.sender_id = ?{}", args.len()));
    }
    if let Some(since) = query.since {
        args.push(SqlValue::Integer(since));
        conditions.push(format!("m.sent_at >= ?{}", args.len()));
    }
    if let Some(until) = query.until {
        args.push(SqlValue::Integer(until));
        conditions.push(format!("m.sent_at <= ?{}", args.len()));
    }
    if query.only_recalled {
        conditions.push("m.recalled_at IS NOT NULL".to_string());
    }
    if query.only_media {
        conditions.push("m.media_json <> '[]'".to_string());
    }
    if let Some(before) = query.before {
        args.push(SqlValue::Integer(before.sent_at));
        let sent = args.len();
        args.push(SqlValue::Integer(before.row_id));
        let row = args.len();
        conditions.push(format!("(m.sent_at < ?{sent} OR (m.sent_at = ?{sent} AND m.id < ?{row}))"));
    }
    let sql = format!(
        "SELECT {MESSAGE_COLUMNS} FROM {from} WHERE {} ORDER BY m.sent_at DESC, m.id DESC LIMIT {}",
        conditions.join(" AND "),
        limit + 1
    );
    let mut stmt = conn.prepare(&sql)?;
    let mut messages = stmt
        .query_map(params_from_iter(args.iter()), map_message)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let has_more = messages.len() > limit;
    messages.truncate(limit);
    let next_cursor = if has_more {
        messages.last().map(|last| json!({ "sent_at": last.sent_at, "row_id": last.row_id }))
    } else {
        None
    };
    let items: Vec<Value> = messages
        .iter()
        .map(|message| {
            json!({
                "row_id": message.row_id,
                "message_id": message.message_id,
                "sender_id": message.sender_id,
                "sender_name": message.sender_name,
                "text": message.content.text,
                "media": message.content.media,
                "mentions": message.content.mentioned_users,
                "reply_to": message.reply_to_message_id,
                "is_bot": message.is_bot,
                "sent_at": message.sent_at,
                "recalled_at": message.recalled_at,
            })
        })
        .collect();
    Ok(json!({ "ok": true, "items": items, "next_cursor": next_cursor.unwrap_or(Value::Null), "fts": use_fts }))
}

/// 统计:发言榜(复用工具同款查询)+ 星期×小时热力 + 人机占比 + 媒体类型。
pub(crate) fn dashboard_stats(
    paths: &MiyuPaths,
    key: ConversationKey,
    since: i64,
    until: i64,
    limit: usize,
) -> Result<Value> {
    let Some(conn) = open_readonly(paths)? else {
        let heat = [[0i64; 24]; 7];
        return Ok(json!({ "ok": true, "total": 0, "ranking": { "items": [], "total_messages": 0, "participant_count": 0 }, "heat": heat, "bot": 0, "human": 0, "media": {} }));
    };
    let ranking = query_activity_ranking(
        &conn,
        ActivityRankingQuery {
            group: key.clone(),
            since,
            until,
            limit: limit.clamp(1, 200),
            include_bot: true,
        },
    )?;
    let scope_args = [
        SqlValue::Text(key.platform.clone()),
        SqlValue::Text(key.account_id.clone()),
        SqlValue::Text(key.conversation_kind.clone()),
        SqlValue::Text(key.conversation_id.clone()),
        SqlValue::Integer(since),
        SqlValue::Integer(until),
    ];
    let scope_sql = "platform = ?1 AND account_id = ?2 AND conversation_kind = ?3 AND conversation_id = ?4 AND sent_at BETWEEN ?5 AND ?6";
    let mut heat = [[0i64; 24]; 7];
    let mut stmt = conn.prepare(&format!(
        "SELECT CAST(strftime('%w', sent_at, 'unixepoch', 'localtime') AS INTEGER),
                CAST(strftime('%H', sent_at, 'unixepoch', 'localtime') AS INTEGER), COUNT(*)
           FROM messages WHERE {scope_sql} GROUP BY 1, 2"
    ))?;
    for row in stmt.query_map(params_from_iter(scope_args.iter()), |row| {
        Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?, row.get::<_, i64>(2)?))
    })? {
        let (weekday, hour, count) = row?;
        if (0..7).contains(&weekday) && (0..24).contains(&hour) {
            heat[weekday as usize][hour as usize] = count;
        }
    }
    let (bot, human): (i64, i64) = conn.query_row(
        &format!(
            "SELECT SUM(CASE WHEN is_bot = 1 THEN 1 ELSE 0 END), SUM(CASE WHEN is_bot = 0 THEN 1 ELSE 0 END)
               FROM messages WHERE {scope_sql}"
        ),
        params_from_iter(scope_args.iter()),
        |row| Ok((row.get::<_, Option<i64>>(0)?.unwrap_or(0), row.get::<_, Option<i64>>(1)?.unwrap_or(0))),
    )?;
    let mut media: std::collections::BTreeMap<String, i64> = std::collections::BTreeMap::new();
    let mut stmt = conn.prepare(&format!(
        "SELECT media_json FROM messages WHERE {scope_sql} AND media_json <> '[]'"
    ))?;
    for row in stmt.query_map(params_from_iter(scope_args.iter()), |row| row.get::<_, String>(0))? {
        if let Ok(Value::Array(items)) = serde_json::from_str::<Value>(&row?) {
            for item in items {
                if let Some(kind) = item["kind"].as_str() {
                    *media.entry(kind.to_string()).or_insert(0) += 1;
                }
            }
        }
    }
    Ok(json!({
        "ok": true,
        "since": since,
        "until": until,
        "total": bot + human,
        "bot": bot,
        "human": human,
        "ranking": ranking,
        "heat": heat,
        "media": media,
    }))
}

/// 撤回记录:撤回表按时间倒序,能对上原消息就带上正文。
pub(crate) fn dashboard_recalls(paths: &MiyuPaths, key: ConversationKey, limit: usize, offset: usize) -> Result<Value> {
    let Some(conn) = open_readonly(paths)? else {
        return Ok(json!({ "ok": true, "items": [], "total": 0 }));
    };
    let args = [
        SqlValue::Text(key.platform.clone()),
        SqlValue::Text(key.account_id.clone()),
        SqlValue::Text(key.conversation_kind.clone()),
        SqlValue::Text(key.conversation_id.clone()),
    ];
    let scope_sql = "r.platform = ?1 AND r.account_id = ?2 AND r.conversation_kind = ?3 AND r.conversation_id = ?4";
    let total: i64 = conn.query_row(
        &format!("SELECT COUNT(*) FROM recalls r WHERE {scope_sql}"),
        params_from_iter(args.iter()),
        |row| row.get(0),
    )?;
    let limit = limit.clamp(1, 200) as i64;
    let mut stmt = conn.prepare(&format!(
        "SELECT r.message_id, r.operator_id, r.recalled_at, m.sender_id, m.sender_name, m.text, m.sent_at
           FROM recalls r
           LEFT JOIN messages m ON m.platform = r.platform AND m.account_id = r.account_id
                AND m.conversation_kind = r.conversation_kind AND m.conversation_id = r.conversation_id
                AND m.message_id = r.message_id
          WHERE {scope_sql}
          ORDER BY r.recalled_at DESC, r.id DESC LIMIT {limit} OFFSET {}",
        offset as i64
    ))?;
    let items = stmt
        .query_map(params_from_iter(args.iter()), |row| {
            Ok(json!({
                "message_id": row.get::<_, String>(0)?,
                "operator_id": row.get::<_, Option<String>>(1)?,
                "recalled_at": row.get::<_, i64>(2)?,
                "sender_id": row.get::<_, Option<String>>(3)?,
                "sender_name": row.get::<_, Option<String>>(4)?,
                "text": row.get::<_, Option<String>>(5)?,
                "sent_at": row.get::<_, Option<i64>>(6)?,
            }))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(json!({ "ok": true, "items": items, "total": total }))
}

pub(crate) struct DeleteSpec {
    /// None = 该账号下全部群聊。
    pub(crate) key: Option<ConversationKey>,
    pub(crate) account_id: String,
    pub(crate) keep_days: Option<u32>,
    pub(crate) sender_id: String,
    pub(crate) since: Option<i64>,
    pub(crate) until: Option<i64>,
}

pub(crate) async fn dashboard_delete(paths: &MiyuPaths, spec: DeleteSpec) -> Result<Value> {
    let scope = match spec.key {
        Some(key) if key.is_group() => HistoryScope::Group(key),
        Some(key) => HistoryScope::Private(key),
        None => HistoryScope::AllGroups(super::store::AccountKey::new(PLATFORM, spec.account_id)?),
    };
    let now = chrono::Utc::now().timestamp();
    let mut request = DeleteRequest::all(scope, now);
    if let Some(days) = spec.keep_days {
        request.mode = DeleteMode::KeepDays(days);
    }
    let sender = spec.sender_id.trim();
    if !sender.is_empty() {
        request.sender_id = Some(sender.to_string());
    }
    request.since = spec.since;
    request.until = spec.until;
    let report = dashboard_store(paths).delete_history(request).await?;
    Ok(json!({
        "ok": true,
        "messages_deleted": report.messages_deleted,
        "recalls_deleted": report.recalls_deleted,
        "boundaries_deleted": report.boundaries_deleted,
    }))
}

pub(crate) async fn dashboard_boundary(paths: &MiyuPaths, key: ConversationKey, persona_scope: String) -> Result<Value> {
    let boundary = dashboard_store(paths).context_boundary(key, persona_scope).await?;
    Ok(json!({ "ok": true, "boundary": boundary }))
}

pub(crate) async fn dashboard_reset_context(paths: &MiyuPaths, key: ConversationKey, persona_scope: String) -> Result<Value> {
    let boundary = dashboard_store(paths)
        .reset_context(key, persona_scope, chrono::Utc::now().timestamp())
        .await?;
    Ok(json!({ "ok": true, "boundary": boundary }))
}
