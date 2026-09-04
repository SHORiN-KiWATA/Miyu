//! 查询：最近消息、全文搜索、活跃度排行。
//!
//! 搜索词有条数与字节上限（`MAX_SEARCH_TERMS` / `MAX_SEARCH_BYTES`）：FTS 的查询
//! 串是拼出来的，词一多查询本身就成了压力源。
//!
//! 分页靠 rowid 游标而不是 OFFSET——历史一直在增长，OFFSET 分页会漏也会重。

use crate::platforms::plugins::message_history::store::*;

pub(crate) const MAX_PAGE_SIZE: usize = 1_000;

pub(crate) const MAX_SEARCH_BYTES: usize = 1_024;

pub(crate) const MAX_SEARCH_TERMS: usize = 32;

pub(crate) const MAX_ACTIVITY_RANKING_LIMIT: usize = 200;

#[derive(Debug, Clone)]
pub(crate) struct RecentQuery {
    pub(crate) group: GroupKey,
    pub(crate) persona_scope: String,
    pub(crate) before: Option<HistoryCursor>,
    pub(crate) limit: usize,
    pub(crate) respect_context_boundary: bool,
    pub(crate) include_recalled: bool,
    pub(crate) before_ingress_order: Option<i64>,
    /// Lower bound used by the reply turn: everything the previous turn already
    /// rendered stays in the conversation history, so a turn only has to carry
    /// what arrived since.
    pub(crate) after_ingress_order: Option<i64>,
}

impl RecentQuery {
    pub(crate) fn for_context(
        group: GroupKey,
        persona_scope: impl Into<String>,
        limit: usize,
    ) -> Self {
        Self {
            group,
            persona_scope: persona_scope.into(),
            before: None,
            limit,
            respect_context_boundary: true,
            include_recalled: false,
            before_ingress_order: None,
            after_ingress_order: None,
        }
    }

    pub(crate) fn for_history(group: GroupKey, limit: usize) -> Self {
        Self {
            group,
            persona_scope: String::new(),
            before: None,
            limit,
            respect_context_boundary: false,
            include_recalled: false,
            before_ingress_order: None,
            after_ingress_order: None,
        }
    }

    pub(crate) fn after_ingress_order(mut self, order: Option<i64>) -> Self {
        self.after_ingress_order = order;
        self
    }

    pub(crate) fn before_ingress_order(mut self, order: Option<i64>) -> Self {
        self.before_ingress_order = order;
        self
    }
}

#[derive(Debug, Clone)]
pub(crate) struct SearchQuery {
    pub(crate) scope: HistoryScope,
    pub(crate) text: String,
    pub(crate) sender_id: Option<String>,
    pub(crate) before: Option<HistoryCursor>,
    pub(crate) since: Option<i64>,
    pub(crate) until: Option<i64>,
    pub(crate) limit: usize,
    pub(crate) include_recalled: bool,
    pub(crate) include_bot: bool,
}

impl SearchQuery {
    pub(crate) fn new(scope: HistoryScope, text: impl Into<String>, limit: usize) -> Self {
        Self {
            scope,
            text: text.into(),
            sender_id: None,
            before: None,
            since: None,
            until: None,
            limit,
            include_recalled: false,
            include_bot: true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct HistoryPage {
    /// Search results are newest-first. Recent-history results are chronological
    /// within the selected newest page so they can be injected directly.
    pub(crate) messages: Vec<HistoryMessage>,
    pub(crate) next_cursor: Option<HistoryCursor>,
}

#[derive(Debug, Clone)]
pub(crate) struct ActivityRankingQuery {
    pub(crate) group: GroupKey,
    pub(crate) since: i64,
    pub(crate) until: i64,
    pub(crate) limit: usize,
    pub(crate) include_bot: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct ActivityRankingItem {
    pub(crate) rank: u64,
    pub(crate) sender_id: String,
    pub(crate) sender_name: String,
    pub(crate) message_count: u64,
    pub(crate) active_days: u64,
    pub(crate) first_sent_at: i64,
    pub(crate) last_sent_at: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct ActivityRanking {
    pub(crate) total_messages: u64,
    pub(crate) participant_count: u64,
    pub(crate) items: Vec<ActivityRankingItem>,
}

pub(crate) const MESSAGE_COLUMNS: &str = "m.id, m.platform, m.account_id, m.conversation_kind, \
    m.conversation_id, m.message_id, m.sender_id, m.sender_name, m.text, m.media_json, \
    m.mentions_json, m.reply_to_message_id, m.is_bot, m.sent_at, m.ingress_order, m.recalled_at";

pub(crate) fn query_recent(conn: &Connection, query: RecentQuery) -> Result<HistoryPage> {
    let page_size = page_size(query.limit);
    let fetch_size = page_size + 1;
    let before = query.before.unwrap_or(HistoryCursor {
        sent_at: i64::MAX,
        row_id: i64::MAX,
    });
    let boundary = if query.respect_context_boundary {
        read_boundary(conn, &query.group, &query.persona_scope)?
            .map(|boundary| boundary.after_row_id)
            .unwrap_or(0)
    } else {
        0
    };
    let sql = format!(
        "SELECT {MESSAGE_COLUMNS} FROM messages AS m
         WHERE m.platform = ?1 AND m.account_id = ?2
           AND m.conversation_kind = ?3 AND m.conversation_id = ?4
           AND m.id > ?5
           AND (?6 OR m.recalled_at IS NULL)
           AND (m.sent_at < ?7 OR (m.sent_at = ?7 AND m.id < ?8))
           AND (?9 IS NULL OR m.ingress_order IS NULL OR m.ingress_order < ?9)
           AND (?11 IS NULL OR (m.ingress_order IS NOT NULL AND m.ingress_order > ?11))
          ORDER BY
            CASE WHEN ?9 IS NOT NULL AND m.ingress_order IS NOT NULL THEN 0 ELSE 1 END ASC,
            CASE WHEN ?9 IS NOT NULL THEN m.ingress_order END DESC,
            m.sent_at DESC,
            m.id DESC
         LIMIT ?10"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(
        params![
            query.group.platform,
            query.group.account_id,
            query.group.conversation_kind,
            query.group.conversation_id,
            boundary,
            query.include_recalled,
            before.sent_at,
            before.row_id,
            query.before_ingress_order,
            fetch_size as i64,
            query.after_ingress_order,
        ],
        map_message,
    )?;
    let mut messages = Vec::with_capacity(page_size.min(64));
    let mut has_more = false;
    for row in rows {
        let message = row?;
        if messages.len() == page_size {
            has_more = true;
            break;
        }
        messages.push(message);
    }
    let next_cursor = has_more.then(|| cursor_for(messages.last().expect("non-empty page")));
    messages.reverse();
    Ok(HistoryPage {
        messages,
        next_cursor,
    })
}

pub(crate) fn query_search(conn: &Connection, query: SearchQuery) -> Result<HistoryPage> {
    let terms = search_terms(&query.text)?;
    let page_size = page_size(query.limit);
    let fetch_size = page_size + 1;
    let before = query.before.unwrap_or(HistoryCursor {
        sent_at: i64::MAX,
        row_id: i64::MAX,
    });
    let use_fts = !terms.is_empty() && terms.iter().all(|term| term.chars().count() >= 3);
    let mut arguments = Vec::<SqlValue>::new();
    let mut conditions = Vec::<String>::new();
    let from = if use_fts {
        arguments.push(SqlValue::Text(build_fts_query(&terms)));
        conditions.push("messages_fts MATCH ?1".to_string());
        "messages_fts JOIN messages AS m ON m.id = messages_fts.rowid"
    } else {
        for term in &terms {
            arguments.push(SqlValue::Text(term.clone()));
            let parameter = arguments.len();
            conditions.push(format!(
                "(instr(lower(m.text), lower(?{parameter})) > 0 OR \
                 instr(lower(m.sender_name), lower(?{parameter})) > 0)"
            ));
        }
        "messages AS m"
    };

    match &query.scope {
        HistoryScope::Group(conversation) | HistoryScope::Private(conversation) => {
            arguments.push(SqlValue::Text(conversation.platform.clone()));
            let platform = arguments.len();
            arguments.push(SqlValue::Text(conversation.account_id.clone()));
            let account = arguments.len();
            arguments.push(SqlValue::Text(conversation.conversation_kind.clone()));
            let kind = arguments.len();
            arguments.push(SqlValue::Text(conversation.conversation_id.clone()));
            let conversation_id = arguments.len();
            conditions.push(format!(
                "m.platform = ?{platform} AND m.account_id = ?{account} \
                 AND m.conversation_kind = ?{kind} AND m.conversation_id = ?{conversation_id}"
            ));
        }
        HistoryScope::AllGroups(account) => {
            arguments.push(SqlValue::Text(account.platform.clone()));
            let platform = arguments.len();
            arguments.push(SqlValue::Text(account.account_id.clone()));
            let account = arguments.len();
            conditions.push(format!(
                "m.platform = ?{platform} AND m.account_id = ?{account} \
                 AND m.conversation_kind = 'group'"
            ));
        }
        HistoryScope::Account(account) => {
            arguments.push(SqlValue::Text(account.platform.clone()));
            let platform = arguments.len();
            arguments.push(SqlValue::Text(account.account_id.clone()));
            let account = arguments.len();
            conditions.push(format!(
                "m.platform = ?{platform} AND m.account_id = ?{account}"
            ));
        }
    }

    if let Some(sender_id) = query.sender_id {
        arguments.push(SqlValue::Text(sender_id));
        let sender = arguments.len();
        conditions.push(format!("m.sender_id = ?{sender}"));
    }
    arguments.push(SqlValue::Integer(i64::from(query.include_recalled)));
    let recalled = arguments.len();
    conditions.push(format!("(?{recalled} OR m.recalled_at IS NULL)"));
    arguments.push(SqlValue::Integer(i64::from(query.include_bot)));
    let bot = arguments.len();
    conditions.push(format!("(?{bot} OR NOT m.is_bot)"));
    arguments.push(query.since.map(SqlValue::Integer).unwrap_or(SqlValue::Null));
    let since = arguments.len();
    conditions.push(format!("(?{since} IS NULL OR m.sent_at >= ?{since})"));
    arguments.push(query.until.map(SqlValue::Integer).unwrap_or(SqlValue::Null));
    let until = arguments.len();
    conditions.push(format!("(?{until} IS NULL OR m.sent_at <= ?{until})"));
    arguments.push(SqlValue::Integer(before.sent_at));
    let before_at = arguments.len();
    arguments.push(SqlValue::Integer(before.row_id));
    let before_id = arguments.len();
    conditions.push(format!(
        "(m.sent_at < ?{before_at} OR (m.sent_at = ?{before_at} AND m.id < ?{before_id}))"
    ));
    arguments.push(SqlValue::Integer(fetch_size as i64));
    let limit = arguments.len();

    let sql = format!(
        "SELECT {MESSAGE_COLUMNS} FROM {from}
         WHERE {}
         ORDER BY m.sent_at DESC, m.id DESC
         LIMIT ?{limit}",
        conditions.join(" AND ")
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(rusqlite::params_from_iter(arguments.iter()), map_message)?;
    let mut messages = Vec::with_capacity(page_size.min(64));
    let mut has_more = false;
    for row in rows {
        let message = row?;
        if messages.len() == page_size {
            has_more = true;
            break;
        }
        messages.push(message);
    }
    let next_cursor = has_more.then(|| cursor_for(messages.last().expect("non-empty page")));
    Ok(HistoryPage {
        messages,
        next_cursor,
    })
}

pub(crate) fn query_activity_ranking(
    conn: &Connection,
    query: ActivityRankingQuery,
) -> Result<ActivityRanking> {
    // 09-04:原先用 ROW_NUMBER() 窗口给每个发送者挑最新昵称,再把 1216 个聚合行
    // LEFT JOIN 到 13 万行的窗口结果上,没有索引可用,一个大群要跑 40 秒。
    // 改为聚合时记下 MAX(id)(插入序即时间序),昵称按主键单点回查。
    let mut stmt = conn.prepare(
        "WITH aggregated AS (
             SELECT CASE WHEN is_bot = 1 THEN ?2 ELSE sender_id END AS effective_sender_id,
                    COUNT(*) AS message_count,
                    COUNT(DISTINCT date(sent_at, 'unixepoch', 'localtime')) AS active_days,
                    MIN(sent_at) AS first_sent_at,
                    MAX(sent_at) AS last_sent_at,
                    MAX(id) AS last_id
               FROM messages
              WHERE platform = ?1 AND account_id = ?2
                AND conversation_kind = ?3 AND conversation_id = ?4
                AND sent_at >= ?5 AND sent_at <= ?6
                AND (?7 OR is_bot = 0)
              GROUP BY effective_sender_id
         ),
         ranked AS (
             SELECT ROW_NUMBER() OVER (
                        ORDER BY aggregated.message_count DESC,
                                 aggregated.last_sent_at DESC,
                                 aggregated.effective_sender_id ASC
                    ) AS rank,
                    aggregated.effective_sender_id,
                    COALESCE(
                        (SELECT m.sender_name FROM messages AS m WHERE m.id = aggregated.last_id),
                        aggregated.effective_sender_id
                    ) AS sender_name,
                    aggregated.message_count,
                    aggregated.active_days,
                    aggregated.first_sent_at,
                    aggregated.last_sent_at,
                    SUM(aggregated.message_count) OVER () AS total_messages,
                    COUNT(*) OVER () AS participant_count
             FROM aggregated
         )
         SELECT rank, effective_sender_id, sender_name, message_count, active_days,
                first_sent_at, last_sent_at, total_messages, participant_count
         FROM ranked
         ORDER BY rank
         LIMIT ?8",
    )?;
    let rows = stmt.query_map(
        params![
            query.group.platform,
            query.group.account_id,
            query.group.conversation_kind,
            query.group.conversation_id,
            query.since,
            query.until,
            query.include_bot,
            query.limit as i64,
        ],
        |row| {
            Ok((
                ActivityRankingItem {
                    rank: row.get(0)?,
                    sender_id: row.get(1)?,
                    sender_name: row.get(2)?,
                    message_count: row.get(3)?,
                    active_days: row.get(4)?,
                    first_sent_at: row.get(5)?,
                    last_sent_at: row.get(6)?,
                },
                row.get::<_, u64>(7)?,
                row.get::<_, u64>(8)?,
            ))
        },
    )?;
    let mut items = Vec::with_capacity(query.limit.min(32));
    let mut total_messages = 0;
    let mut participant_count = 0;
    for row in rows {
        let (item, total, participants) = row?;
        total_messages = total;
        participant_count = participants;
        items.push(item);
    }
    Ok(ActivityRanking {
        total_messages,
        participant_count,
        items,
    })
}

pub(crate) fn map_message(row: &rusqlite::Row<'_>) -> rusqlite::Result<HistoryMessage> {
    let media_json: String = row.get(9)?;
    let media = serde_json::from_str(&media_json).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(9, rusqlite::types::Type::Text, Box::new(error))
    })?;
    let mentions_json: String = row.get(10)?;
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum StoredMentions {
        Users(Vec<PlatformMention>),
        Ids(Vec<String>),
    }
    let (mentioned_user_ids, mentioned_users) =
        match serde_json::from_str(&mentions_json).map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                10,
                rusqlite::types::Type::Text,
                Box::new(error),
            )
        })? {
            StoredMentions::Users(users) => (
                users
                    .iter()
                    .map(|mention| mention.user_id.clone())
                    .collect(),
                users,
            ),
            StoredMentions::Ids(ids) => (ids, Vec::new()),
        };
    Ok(HistoryMessage {
        row_id: row.get(0)?,
        group: GroupKey {
            platform: row.get(1)?,
            account_id: row.get(2)?,
            conversation_kind: row.get(3)?,
            conversation_id: row.get(4)?,
        },
        message_id: row.get(5)?,
        sender_id: row.get(6)?,
        sender_name: row.get(7)?,
        content: SanitizedContent {
            text: row.get(8)?,
            media,
            mentioned_user_ids,
            mentioned_users,
        },
        reply_to_message_id: row.get(11)?,
        is_bot: row.get(12)?,
        sent_at: row.get(13)?,
        ingress_order: row.get(14)?,
        recalled_at: row.get(15)?,
    })
}

pub(crate) fn cursor_for(message: &HistoryMessage) -> HistoryCursor {
    HistoryCursor {
        sent_at: message.sent_at,
        row_id: message.row_id,
    }
}

pub(crate) fn search_terms(text: &str) -> Result<Vec<String>> {
    let text = sanitize_multiline(text, MAX_SEARCH_BYTES);
    let terms = text
        .split_whitespace()
        .filter(|term| !term.is_empty())
        .take(MAX_SEARCH_TERMS)
        .map(ToString::to_string)
        .collect::<Vec<_>>();
    Ok(terms)
}

pub(crate) fn build_fts_query(terms: &[String]) -> String {
    terms
        .iter()
        .map(|term| format!("\"{}\"", term.replace('"', "\"\"")))
        .collect::<Vec<_>>()
        .join(" AND ")
}

pub(crate) fn page_size(requested: usize) -> usize {
    requested.clamp(1, MAX_PAGE_SIZE)
}
