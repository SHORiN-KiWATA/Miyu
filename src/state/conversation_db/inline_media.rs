//! 回合内追加给模型的媒体块(v30 `turn_inline_media`)。
//!
//! 写入发生在工具调用刚结束、消息被推进活体对话的同一刻;读取只在历史
//! 重放需要时按回合取一次,不挂进 `attach_turn_children`(那是每回合
//! 5-8 次的热路径,多数回合没有这种块)。

use crate::state::conversation_db::*;

impl ConversationDb {
    pub fn insert_turn_inline_media(&self, turn_id: &str, items: &[TurnInlineMedia]) -> Result<()> {
        if items.is_empty() {
            return Ok(());
        }
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let created_at = Utc::now().to_rfc3339();
        for item in items {
            tx.execute(
                "INSERT INTO turn_inline_media
                    (turn_id, call_id, seq, kind, mime, source, data, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    turn_id,
                    item.call_id,
                    item.seq,
                    item.kind,
                    item.mime,
                    item.source,
                    item.data.as_deref(),
                    created_at,
                ],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn load_turn_inline_media(&self, turn_id: &str) -> Result<Vec<TurnInlineMedia>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT call_id, seq, kind, mime, source, data
             FROM turn_inline_media WHERE turn_id = ?1
             ORDER BY call_id, seq, media_id",
        )?;
        let rows = stmt
            .query_map(params![turn_id], |row| {
                Ok(TurnInlineMedia {
                    call_id: row.get(0)?,
                    seq: row.get(1)?,
                    kind: row.get(2)?,
                    mime: row.get(3)?,
                    source: row.get(4)?,
                    data: row.get::<_, Option<Vec<u8>>>(5)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }
}
