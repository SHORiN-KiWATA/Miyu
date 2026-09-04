//! 行映射与子表挂载。
//!
//! 一个回合的完整形态散在六张表里（流水、附件、提问、追加、排队附件……）。
//! `attach_*_locked` 一族把它们挂回同一个 `Turn` 上。
//!
//! 都带 `_locked` 后缀是提醒：这些函数假设调用方已经持有连接锁，自己不再取。

use crate::state::conversation_db::*;

pub(crate) const SESSION_COLUMNS: &str = "session_id, persona, name, kind, parent_session_id, workspace, archived, created_at, updated_at, sort_key";

pub(crate) fn session_record_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<SessionRecord> {
    Ok(SessionRecord {
        session_id: row.get("session_id")?,
        persona: row.get("persona")?,
        name: row.get("name")?,
        kind: row.get("kind")?,
        parent_session_id: row.get("parent_session_id")?,
        workspace: row.get("workspace")?,
        archived: row.get("archived")?,
        created_at: row.get("created_at")?,
        updated_at: row.get("updated_at")?,
        sort_key: row.get("sort_key")?,
    })
}

pub(crate) fn interrupted_prefix(content: &str) -> String {
    let suffix = format!("\n\n{INTERRUPTED_TEXT}");
    content
        .strip_suffix(&suffix)
        .unwrap_or_else(|| content.strip_suffix(INTERRUPTED_TEXT).unwrap_or(content))
        .to_string()
}

#[allow(dead_code)]
pub fn pending_placeholder() -> &'static str {
    PENDING_PLACEHOLDER
}

#[allow(dead_code)]
pub fn interrupted_text() -> &'static str {
    INTERRUPTED_TEXT
}

pub(crate) fn map_turn_row(row: &rusqlite::Row) -> rusqlite::Result<Turn> {
    let tool_reports_json: String = row.get(11)?;
    let tool_reports: Vec<String> = serde_json::from_str(&tool_reports_json).unwrap_or_default();
    let context_messages_json: String = row.get::<_, Option<String>>(18)?.unwrap_or_default();
    let context_messages: Vec<ChatMessage> =
        serde_json::from_str(&context_messages_json).unwrap_or_default();
    let tool_flow_json: String = row.get::<_, Option<String>>(21)?.unwrap_or_default();
    let tool_flow: Vec<ToolFlowRound> = serde_json::from_str(&tool_flow_json).unwrap_or_default();
    Ok(Turn {
        turn_id: row.get(0)?,
        seq: row.get(1)?,
        user_content: row.get(2)?,
        display_content: row.get(3)?,
        user_timestamp: row.get(4)?,
        assistant_content: row.get(5)?,
        assistant_reasoning: row.get(6)?,
        assistant_provider_id: row.get(7)?,
        assistant_model: row.get(8)?,
        assistant_timestamp: row.get(9)?,
        status: TurnStatus::from_str(row.get::<_, String>(10)?.as_str()),
        tool_reports,
        tool_flow,
        question_exchanges: Vec::new(),
        followups: Vec::new(),
        attachments: Vec::new(),
        hidden: row.get::<_, i64>(12)? != 0,
        is_summary: row.get::<_, i64>(13)? != 0,
        owner_pid: row.get(14)?,
        token_total: row.get::<_, i64>(15)?.max(0) as u64,
        token_prompt: row.get::<_, i64>(19)?.max(0) as u64,
        token_cache_read: row.get::<_, i64>(20)?.max(0) as u64,
        token_usage_estimated: row.get::<_, i64>(16)? != 0,
        revision: row.get(17)?,
        journal_events: Vec::new(),
        context_messages,
    })
}

pub(crate) fn map_user_attachment_row(row: &rusqlite::Row) -> rusqlite::Result<UserAttachment> {
    Ok(UserAttachment {
        attachment_id: row.get(0)?,
        file_name: row.get(1)?,
        mime: row.get(2)?,
        kind: row.get(3)?,
        size_bytes: row.get::<_, i64>(4)?.max(0) as u64,
        width: row.get::<_, i64>(5)?.max(0) as u32,
        height: row.get::<_, i64>(6)?.max(0) as u32,
        created_at: row.get(7)?,
    })
}

pub(crate) fn map_image_asset_row(row: &rusqlite::Row) -> rusqlite::Result<ImageAsset> {
    Ok(ImageAsset {
        asset_id: row.get(0)?,
        turn_id: row.get(1)?,
        tool_id: row.get(2)?,
        mime: row.get(3)?,
        width: row.get::<_, i64>(4)?.max(0) as u32,
        height: row.get::<_, i64>(5)?.max(0) as u32,
        alt: row.get(6)?,
        created_at: row.get(7)?,
    })
}

pub(crate) fn map_artifact_asset_row(row: &rusqlite::Row) -> rusqlite::Result<ArtifactAsset> {
    Ok(ArtifactAsset {
        asset_id: row.get(0)?,
        turn_id: row.get(1)?,
        tool_id: row.get(2)?,
        source_key: row.get(3)?,
        file_name: row.get(4)?,
        mime: row.get(5)?,
        kind: row.get(6)?,
        size_bytes: row.get::<_, i64>(7)?.max(0) as u64,
        created_at: row.get(8)?,
        updated_at: row.get(9)?,
    })
}

pub(crate) fn attach_turn_children_locked(conn: &Connection, turns: &mut [Turn]) -> Result<()> {
    attach_tool_reports_locked(conn, turns)?;
    attach_question_exchanges_locked(conn, turns)?;
    attach_followups_locked(conn, turns)?;
    attach_turn_attachments_locked(conn, turns)?;
    attach_turn_journal_events_locked(conn, turns)
}

/// Folds the live journal of a just-finished turn into `turns.replay_journal`.
/// Everything only the live view needed — reasoning, progress ticks, command
/// output blobs — is dropped; what is left is the ordered prose/tool sequence
/// the REPL redraws when the session is reopened.
pub(crate) fn store_replay_journal(tx: &Transaction, turn_id: &str) -> Result<()> {
    // 只取当前修订的事件:被 redo 的 interrupted 回合会同时残留新旧两个
    // revision 的事件(interrupt 不删 segments),混着快照会串台。
    let mut stmt = tx.prepare(
        "SELECT kind, call_id, name, text_payload, ok
           FROM turn_journal_events
          WHERE turn_id = ?1
            AND revision = (SELECT revision FROM turns WHERE turn_id = ?1)
            AND kind IN ('assistant_content', 'tool_call', 'tool_result')
          ORDER BY event_id",
    )?;
    let rows = stmt
        .query_map(params![turn_id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, Option<i64>>(4)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    drop(stmt);

    let mut entries: Vec<ReplayEntry> = Vec::new();
    let mut call_names: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();
    let mut text = String::new();
    let flush_text = |entries: &mut Vec<ReplayEntry>, text: &mut String| {
        if !text.trim().is_empty() {
            entries.push(ReplayEntry::Text {
                text: truncate_chars_owned(text, REPLAY_ENTRY_MAX_CHARS),
            });
        }
        text.clear();
    };
    for (kind, call_id, name, payload, ok) in rows {
        match kind.as_str() {
            "assistant_content" => text.push_str(payload.as_deref().unwrap_or_default()),
            "tool_call" => {
                flush_text(&mut entries, &mut text);
                let Some(name) = name else { continue };
                if let Some(call_id) = call_id {
                    call_names.insert(call_id, name.clone());
                }
                entries.push(ReplayEntry::ToolCall {
                    name,
                    arguments: truncate_chars_owned(
                        payload.as_deref().unwrap_or_default(),
                        REPLAY_ENTRY_MAX_CHARS,
                    ),
                });
            }
            "tool_result" => {
                flush_text(&mut entries, &mut text);
                let name = call_id
                    .as_deref()
                    .and_then(|id| call_names.get(id).cloned())
                    .or(name)
                    .unwrap_or_default();
                if name.is_empty() {
                    continue;
                }
                entries.push(ReplayEntry::ToolResult {
                    name,
                    ok: ok.unwrap_or(1) != 0,
                    output: truncate_chars_owned(
                        payload.as_deref().unwrap_or_default(),
                        REPLAY_ENTRY_MAX_CHARS,
                    ),
                });
            }
            _ => {}
        }
    }
    flush_text(&mut entries, &mut text);
    if entries.is_empty() {
        return Ok(());
    }
    // Whole-turn budget: drop the oldest entries, so what survives is the tail
    // the user was actually looking at when the turn ended.
    let mut encoded = serde_json::to_string(&entries)?;
    while encoded.len() > REPLAY_JOURNAL_MAX_CHARS && entries.len() > 1 {
        entries.remove(0);
        encoded = serde_json::to_string(&entries)?;
    }
    tx.execute(
        "UPDATE turns SET replay_journal = ?1 WHERE turn_id = ?2",
        params![encoded, turn_id],
    )?;
    Ok(())
}

pub(crate) fn truncate_chars_owned(value: &str, max: usize) -> String {
    if value.chars().count() <= max {
        return value.to_string();
    }
    let kept: String = value.chars().take(max).collect();
    format!("{kept}…")
}

pub(crate) fn attach_turn_journal_events_locked(
    conn: &Connection,
    turns: &mut [Turn],
) -> Result<()> {
    // BTreeMap keeps the chunking below deterministic; HashMap iteration order
    // would shuffle turn ids across the 900-id chunks between calls.
    let indexes = turns
        .iter()
        .enumerate()
        .filter(|(_, turn)| turn.status != TurnStatus::Completed)
        .map(|(index, turn)| (turn.turn_id.clone(), index))
        .collect::<std::collections::BTreeMap<_, _>>();
    if indexes.is_empty() {
        return Ok(());
    }
    let turn_ids = indexes.keys().collect::<Vec<_>>();
    for chunk in turn_ids.chunks(900) {
        let placeholders = std::iter::repeat_n("?", chunk.len())
            .collect::<Vec<_>>()
            .join(", ");
        let sql = format!(
            "SELECT e.turn_id, e.event_id, e.revision, e.segment_index, e.kind,
                    e.call_id, e.name, e.text_payload, e.blob_payload, e.ok
             FROM turn_journal_events e
             INNER JOIN turn_journal_segments s
               ON s.turn_id = e.turn_id AND s.revision = e.revision
              AND s.segment_index = e.segment_index
             INNER JOIN turns t ON t.turn_id = e.turn_id AND t.revision = e.revision
             WHERE e.turn_id IN ({placeholders})
                AND (
                    s.status != 'superseded'
                    OR (
                        e.kind IN (
                            'tool_call', 'tool_result', 'tool_progress',
                            'command_stdout', 'command_stderr', 'image', 'artifact'
                        )
                        AND EXISTS(
                            SELECT 1 FROM turn_journal_events result_event
                            WHERE result_event.turn_id = e.turn_id
                              AND result_event.revision = e.revision
                              AND result_event.segment_index = e.segment_index
                              AND result_event.kind = 'tool_result'
                              AND result_event.call_id = e.call_id
                        )
                    )
                )
             ORDER BY e.event_id"
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(rusqlite::params_from_iter(chunk.iter()), |row| {
            Ok((
                row.get::<_, String>(0)?,
                TurnJournalEvent {
                    event_id: row.get(1)?,
                    revision: row.get(2)?,
                    segment_index: row.get(3)?,
                    kind: row.get(4)?,
                    call_id: row.get(5)?,
                    name: row.get(6)?,
                    text_payload: row.get(7)?,
                    blob_payload: row.get(8)?,
                    ok: row.get::<_, Option<i64>>(9)?.map(|value| value != 0),
                },
            ))
        })?;
        for row in rows {
            let (turn_id, event) = row?;
            if let Some(index) = indexes.get(&turn_id).copied() {
                turns[index].journal_events.push(event);
            }
        }
    }
    Ok(())
}

pub(crate) fn attach_turn_attachments_locked(conn: &Connection, turns: &mut [Turn]) -> Result<()> {
    if turns.is_empty() {
        return Ok(());
    }
    let indexes = turns
        .iter()
        .enumerate()
        .map(|(index, turn)| (turn.turn_id.clone(), index))
        .collect::<std::collections::HashMap<_, _>>();
    let turn_ids = indexes.keys().collect::<Vec<_>>();
    for chunk in turn_ids.chunks(900) {
        let placeholders = std::iter::repeat_n("?", chunk.len())
            .collect::<Vec<_>>()
            .join(", ");
        let sql = format!(
            "SELECT turn_id, attachment_id, file_name, mime, kind, size_bytes,
                    width, height, created_at FROM user_attachments
             WHERE turn_id IN ({placeholders}) ORDER BY created_at, attachment_id"
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(rusqlite::params_from_iter(chunk.iter()), |row| {
            Ok((
                row.get::<_, String>(0)?,
                UserAttachment {
                    attachment_id: row.get(1)?,
                    file_name: row.get(2)?,
                    mime: row.get(3)?,
                    kind: row.get(4)?,
                    size_bytes: row.get::<_, i64>(5)?.max(0) as u64,
                    width: row.get::<_, i64>(6)?.max(0) as u32,
                    height: row.get::<_, i64>(7)?.max(0) as u32,
                    created_at: row.get(8)?,
                },
            ))
        })?;
        for row in rows {
            let (turn_id, attachment) = row?;
            if let Some(index) = indexes.get(&turn_id).copied() {
                turns[index].attachments.push(attachment);
            }
        }
    }
    Ok(())
}

pub(crate) fn attach_question_exchanges_locked(
    conn: &Connection,
    turns: &mut [Turn],
) -> Result<()> {
    if turns.is_empty() {
        return Ok(());
    }
    let indexes = turns
        .iter()
        .enumerate()
        .map(|(index, turn)| (turn.turn_id.clone(), index))
        .collect::<std::collections::HashMap<_, _>>();
    let turn_ids = indexes.keys().collect::<Vec<_>>();
    for chunk in turn_ids.chunks(900) {
        let placeholders = std::iter::repeat_n("?", chunk.len())
            .collect::<Vec<_>>()
            .join(", ");
        let sql = format!(
            "SELECT turn_id, payload FROM question_exchanges
             WHERE turn_id IN ({placeholders}) ORDER BY turn_id, exchange_index"
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(rusqlite::params_from_iter(chunk.iter()), |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        for row in rows {
            let (turn_id, payload) = row?;
            let Some(index) = indexes.get(&turn_id).copied() else {
                continue;
            };
            let exchange = serde_json::from_str::<QuestionExchange>(&payload)
                .with_context(|| format!("invalid question exchange for turn {turn_id}"))?;
            turns[index].question_exchanges.push(exchange);
        }
    }
    Ok(())
}

pub(crate) fn attach_followups_locked(conn: &Connection, turns: &mut [Turn]) -> Result<()> {
    if turns.is_empty() {
        return Ok(());
    }
    let indexes = turns
        .iter()
        .enumerate()
        .map(|(index, turn)| (turn.turn_id.clone(), index))
        .collect::<std::collections::HashMap<_, _>>();
    let turn_ids = indexes.keys().collect::<Vec<_>>();
    for chunk in turn_ids.chunks(900) {
        let placeholders = std::iter::repeat_n("?", chunk.len())
            .collect::<Vec<_>>()
            .join(", ");
        let sql = format!(
            "SELECT prompt_id, turn_id, COALESCE(context_content, content), display_content,
                    attachments, submitted_at, preceding_assistant_content,
                    preceding_assistant_reasoning, preceding_assistant_provider_id,
                    preceding_assistant_model, COALESCE(context_messages, '[]')
             FROM queued_prompts
             WHERE status = 'consumed' AND turn_id IN ({placeholders})
             ORDER BY seq ASC"
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(rusqlite::params_from_iter(chunk.iter()), |row| {
            Ok((
                row.get::<_, String>(1)?,
                TurnFollowup {
                    prompt_id: row.get(0)?,
                    content: row.get(2)?,
                    display_content: row.get(3)?,
                    attachments: serde_json::from_str(&row.get::<_, String>(4)?)
                        .unwrap_or_default(),
                    uploaded_attachments: Vec::new(),
                    submitted_at: row.get(5)?,
                    preceding_assistant_content: row.get(6)?,
                    preceding_assistant_reasoning: row.get(7)?,
                    preceding_assistant_provider_id: row.get(8)?,
                    preceding_assistant_model: row.get(9)?,
                    context_messages_json: row.get(10)?,
                },
            ))
        })?;
        for row in rows {
            let (turn_id, followup) = row?;
            let Some(index) = indexes.get(&turn_id).copied() else {
                continue;
            };
            turns[index].followups.push(followup);
        }
    }
    attach_followup_attachments_locked(conn, turns)?;
    Ok(())
}

pub(crate) fn attach_prompt_attachments_locked(
    conn: &Connection,
    prompts: &mut [QueuedPrompt],
) -> Result<()> {
    let indexes = prompts
        .iter()
        .enumerate()
        .map(|(index, prompt)| (prompt.prompt_id.clone(), index))
        .collect::<std::collections::HashMap<_, _>>();
    if indexes.is_empty() {
        return Ok(());
    }
    let prompt_ids = indexes.keys().collect::<Vec<_>>();
    for chunk in prompt_ids.chunks(900) {
        let placeholders = std::iter::repeat_n("?", chunk.len())
            .collect::<Vec<_>>()
            .join(", ");
        let sql = format!(
            "SELECT prompt_id, attachment_id, file_name, mime, kind, size_bytes,
                    width, height, created_at FROM user_attachments
             WHERE prompt_id IN ({placeholders}) ORDER BY created_at, attachment_id"
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(rusqlite::params_from_iter(chunk.iter()), |row| {
            Ok((
                row.get::<_, String>(0)?,
                UserAttachment {
                    attachment_id: row.get(1)?,
                    file_name: row.get(2)?,
                    mime: row.get(3)?,
                    kind: row.get(4)?,
                    size_bytes: row.get::<_, i64>(5)?.max(0) as u64,
                    width: row.get::<_, i64>(6)?.max(0) as u32,
                    height: row.get::<_, i64>(7)?.max(0) as u32,
                    created_at: row.get(8)?,
                },
            ))
        })?;
        for row in rows {
            let (prompt_id, attachment) = row?;
            if let Some(index) = indexes.get(&prompt_id).copied() {
                prompts[index].uploaded_attachments.push(attachment);
            }
        }
    }
    Ok(())
}

pub(crate) fn attach_followup_attachments_locked(
    conn: &Connection,
    turns: &mut [Turn],
) -> Result<()> {
    let mut locations = std::collections::HashMap::new();
    for (turn_index, turn) in turns.iter().enumerate() {
        for (followup_index, followup) in turn.followups.iter().enumerate() {
            locations.insert(followup.prompt_id.clone(), (turn_index, followup_index));
        }
    }
    if locations.is_empty() {
        return Ok(());
    }
    let prompt_ids = locations.keys().collect::<Vec<_>>();
    for chunk in prompt_ids.chunks(900) {
        let placeholders = std::iter::repeat_n("?", chunk.len())
            .collect::<Vec<_>>()
            .join(", ");
        let sql = format!(
            "SELECT prompt_id, attachment_id, file_name, mime, kind, size_bytes,
                    width, height, created_at FROM user_attachments
             WHERE prompt_id IN ({placeholders}) ORDER BY created_at, attachment_id"
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(rusqlite::params_from_iter(chunk.iter()), |row| {
            Ok((
                row.get::<_, String>(0)?,
                UserAttachment {
                    attachment_id: row.get(1)?,
                    file_name: row.get(2)?,
                    mime: row.get(3)?,
                    kind: row.get(4)?,
                    size_bytes: row.get::<_, i64>(5)?.max(0) as u64,
                    width: row.get::<_, i64>(6)?.max(0) as u32,
                    height: row.get::<_, i64>(7)?.max(0) as u32,
                    created_at: row.get(8)?,
                },
            ))
        })?;
        for row in rows {
            let (prompt_id, attachment) = row?;
            if let Some((turn_index, followup_index)) = locations.get(&prompt_id).copied() {
                turns[turn_index].followups[followup_index]
                    .uploaded_attachments
                    .push(attachment);
            }
        }
    }
    Ok(())
}

/// 把 `turn_tool_reports` 里的报告接到各回合尾部（v25）。
///
/// **接在尾部，不是覆盖**：老回合的报告还在 `turns.tool_reports` 那个 JSON 列
/// 里（v25 不回填、不删列），`map_turn_row` 已经把它读进来了。跨升级的那种
/// 回合两边都有内容，先列后子表正是它们真实发生的顺序。
///
/// 排序用 `report_id`——自增主键，插入顺序即报告顺序，所以写入端连
/// `MAX(seq)` 都不用查。
///
/// 900 一批是跟着同文件其它 `attach_*` 走的：SQLite 默认变量上限 999。
pub(crate) fn attach_tool_reports_locked(conn: &Connection, turns: &mut [Turn]) -> Result<()> {
    if turns.is_empty() {
        return Ok(());
    }
    let indexes = turns
        .iter()
        .enumerate()
        .map(|(index, turn)| (turn.turn_id.clone(), index))
        .collect::<std::collections::HashMap<_, _>>();
    let turn_ids = indexes.keys().collect::<Vec<_>>();
    for chunk in turn_ids.chunks(900) {
        let placeholders = std::iter::repeat_n("?", chunk.len())
            .collect::<Vec<_>>()
            .join(", ");
        let sql = format!(
            "SELECT turn_id, report FROM turn_tool_reports
             WHERE turn_id IN ({placeholders})
             ORDER BY report_id ASC"
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt
            .query_map(rusqlite::params_from_iter(chunk.iter()), |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        for (turn_id, report) in rows {
            if let Some(&index) = indexes.get(&turn_id) {
                turns[index].tool_reports.push(report);
            }
        }
    }
    Ok(())
}
