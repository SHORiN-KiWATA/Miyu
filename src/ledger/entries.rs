//! 流水的增删改查。
//!
//! 两条与别处不同的规矩：
//!
//! - **软删除**。`delete` 只置 `deleted_at`，行永远留着。财务数据里「少了
//!   一笔」比「多了一笔」难发现得多，硬删除不给回头路。
//! - **乐观并发**。改一笔要带上读到的 `revision`，对不上就报错让调用方
//!   重读。WebUI 和模型可能同时在动同一笔账。

use super::types::*;
use super::{new_id, now_rfc3339, LedgerDb};
use anyhow::{bail, Result};
use rusqlite::{params, OptionalExtension};

const ENTRY_COLUMNS: &str = "entry_id, book_id, kind, amount_minor, currency, base_amount_minor, \
     base_currency, rate, rate_source, rate_at, rate_status, account_id, to_account_id, \
     category_id, occurred_at, occurred_day, note, merchant, source, revision, deleted_at, \
     created_at, updated_at";

/// 重复记账闸的回看窗口。
///
/// 端点抖动时模型会把「记一笔」重演成同义变体（QQ 投递幂等闸踩过同一个
/// 坑）。五分钟足以盖住一次重试风暴，又不至于挡住「同一家店连买两杯
/// 一样的咖啡」这种真实场景——那种情况调用方带 `force` 再记一次即可。
pub const DUPLICATE_WINDOW_SECS: i64 = 300;

/// 待插入的一笔账。字段与 [`EntryRecord`] 同构，缺的是库来填的部分。
#[derive(Clone, Debug)]
pub struct NewEntry {
    pub book_id: String,
    pub kind: EntryKind,
    pub amount_minor: i64,
    pub currency: String,
    pub base_amount_minor: Option<i64>,
    pub base_currency: String,
    pub rate: Option<String>,
    pub rate_source: Option<String>,
    pub rate_at: Option<String>,
    pub rate_status: RateStatus,
    pub account_id: Option<String>,
    pub to_account_id: Option<String>,
    pub category_id: Option<String>,
    pub occurred_at: String,
    pub occurred_day: String,
    pub note: String,
    pub merchant: String,
    pub source: EntrySource,
}

/// 查询条件。全部可选，组合成一条固定模板的 SQL——条件用
/// `(?n IS NULL OR ...)` 表达而不是拼字符串，绑定参数一个都不漏。
#[derive(Clone, Debug, Default)]
pub struct EntryFilter {
    pub book_id: String,
    pub kind: Option<EntryKind>,
    pub category_id: Option<String>,
    pub account_id: Option<String>,
    /// 本地自然日下界，含。
    pub from_day: Option<String>,
    /// 本地自然日上界，含。
    pub to_day: Option<String>,
    /// 在备注与商家里做大小写无关的子串匹配。
    pub query: Option<String>,
    pub include_deleted: bool,
    pub offset: i64,
    pub limit: i64,
}

/// 改一笔账。`None` 表示这个字段不动；清空文本字段传 `Some("")`。
#[derive(Clone, Debug, Default)]
pub struct EntryPatch {
    pub amount_minor: Option<i64>,
    pub currency: Option<String>,
    pub base_amount_minor: Option<Option<i64>>,
    pub rate: Option<Option<String>>,
    pub rate_source: Option<Option<String>>,
    pub rate_at: Option<Option<String>>,
    pub rate_status: Option<RateStatus>,
    pub account_id: Option<Option<String>>,
    pub to_account_id: Option<Option<String>>,
    pub category_id: Option<Option<String>>,
    pub occurred_at: Option<String>,
    pub occurred_day: Option<String>,
    pub note: Option<String>,
    pub merchant: Option<String>,
}

fn entry_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<EntryRecord> {
    let kind_raw: String = row.get(2)?;
    let rate_status_raw: String = row.get(10)?;
    let source_raw: String = row.get(18)?;
    Ok(EntryRecord {
        entry_id: row.get(0)?,
        book_id: row.get(1)?,
        // CHECK 约束保证取值合法；解析不出只可能是库被手工改过，
        // 退回 expense 让它至少能显示出来而不是整条查询失败。
        kind: EntryKind::parse(&kind_raw).unwrap_or(EntryKind::Expense),
        amount_minor: row.get(3)?,
        currency: row.get(4)?,
        base_amount_minor: row.get(5)?,
        base_currency: row.get(6)?,
        rate: row.get(7)?,
        rate_source: row.get(8)?,
        rate_at: row.get(9)?,
        rate_status: RateStatus::parse(&rate_status_raw).unwrap_or(RateStatus::Pending),
        account_id: row.get(11)?,
        to_account_id: row.get(12)?,
        category_id: row.get(13)?,
        occurred_at: row.get(14)?,
        occurred_day: row.get(15)?,
        note: row.get(16)?,
        merchant: row.get(17)?,
        source: EntrySource::parse(&source_raw).unwrap_or(EntrySource::Chat),
        revision: row.get(19)?,
        deleted_at: row.get(20)?,
        created_at: row.get(21)?,
        updated_at: row.get(22)?,
    })
}

impl LedgerDb {
    pub fn add_entry(&self, new: NewEntry) -> Result<EntryRecord> {
        let now = now_rfc3339();
        let record = EntryRecord {
            entry_id: new_id("en"),
            book_id: new.book_id,
            kind: new.kind,
            amount_minor: new.amount_minor,
            currency: new.currency,
            base_amount_minor: new.base_amount_minor,
            base_currency: new.base_currency,
            rate: new.rate,
            rate_source: new.rate_source,
            rate_at: new.rate_at,
            rate_status: new.rate_status,
            account_id: new.account_id,
            to_account_id: new.to_account_id,
            category_id: new.category_id,
            occurred_at: new.occurred_at,
            occurred_day: new.occurred_day,
            note: new.note,
            merchant: new.merchant,
            source: new.source,
            revision: 1,
            deleted_at: None,
            created_at: now.clone(),
            updated_at: now,
        };
        self.with_tx(|tx| {
            tx.execute(
                "INSERT INTO ledger_entries
                 (entry_id, book_id, kind, amount_minor, currency, base_amount_minor,
                  base_currency, rate, rate_source, rate_at, rate_status, account_id,
                  to_account_id, category_id, occurred_at, occurred_day, note, merchant,
                  source, revision, deleted_at, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14,
                         ?15, ?16, ?17, ?18, ?19, 1, NULL, ?20, ?21)",
                params![
                    record.entry_id,
                    record.book_id,
                    record.kind.as_str(),
                    record.amount_minor,
                    record.currency,
                    record.base_amount_minor,
                    record.base_currency,
                    record.rate,
                    record.rate_source,
                    record.rate_at,
                    record.rate_status.as_str(),
                    record.account_id,
                    record.to_account_id,
                    record.category_id,
                    record.occurred_at,
                    record.occurred_day,
                    record.note,
                    record.merchant,
                    record.source.as_str(),
                    record.created_at,
                    record.updated_at
                ],
            )?;
            Ok(())
        })?;
        Ok(record)
    }

    /// 找出窗口内内容完全相同的一笔。
    ///
    /// 判据是「同账本 + 同性质 + 同金额同币种 + 同分类 + 同备注」，比对的是
    /// 内容而不是 id：重演出来的那笔是新 id、新时间戳，只有内容一样。
    pub fn find_recent_duplicate(
        &self,
        book_id: &str,
        kind: EntryKind,
        amount_minor: i64,
        currency: &str,
        category_id: Option<&str>,
        note: &str,
        within_secs: i64,
    ) -> Result<Option<EntryRecord>> {
        let cutoff = (chrono::Utc::now() - chrono::Duration::seconds(within_secs)).to_rfc3339();
        self.with_conn(|conn| {
            let sql = format!(
                "SELECT {ENTRY_COLUMNS} FROM ledger_entries
                 WHERE deleted_at IS NULL
                   AND book_id = ?1 AND kind = ?2 AND amount_minor = ?3 AND currency = ?4
                   AND ((category_id IS NULL AND ?5 IS NULL) OR category_id = ?5)
                   AND note = ?6
                   AND created_at >= ?7
                 ORDER BY created_at DESC
                 LIMIT 1"
            );
            Ok(conn
                .query_row(
                    &sql,
                    params![
                        book_id,
                        kind.as_str(),
                        amount_minor,
                        currency,
                        category_id,
                        note,
                        cutoff
                    ],
                    entry_from_row,
                )
                .optional()?)
        })
    }

    pub fn list_entries(&self, filter: &EntryFilter) -> Result<(Vec<EntryRecord>, i64)> {
        let limit = filter.limit.clamp(1, 500);
        let offset = filter.offset.max(0);
        self.with_conn(|conn| {
            const WHERE_CLAUSE: &str = "WHERE book_id = ?1
                   AND (?2 = 1 OR deleted_at IS NULL)
                   AND (?3 IS NULL OR kind = ?3)
                   AND (?4 IS NULL OR category_id = ?4)
                   AND (?5 IS NULL OR account_id = ?5 OR to_account_id = ?5)
                   AND (?6 IS NULL OR occurred_day >= ?6)
                   AND (?7 IS NULL OR occurred_day <= ?7)
                   AND (?8 IS NULL
                        OR instr(lower(note), lower(?8)) > 0
                        OR instr(lower(merchant), lower(?8)) > 0)";
            let kind = filter.kind.map(|kind| kind.as_str());
            let base_params = params![
                filter.book_id,
                i64::from(filter.include_deleted),
                kind,
                filter.category_id,
                filter.account_id,
                filter.from_day,
                filter.to_day,
                filter.query,
            ];

            let total: i64 = conn.query_row(
                &format!("SELECT COUNT(*) FROM ledger_entries {WHERE_CLAUSE}"),
                base_params,
                |row| row.get(0),
            )?;

            // 同一天记的多笔按插入顺序倒序，让「刚记的」排在最前面。
            let sql = format!(
                "SELECT {ENTRY_COLUMNS} FROM ledger_entries {WHERE_CLAUSE}
                 ORDER BY occurred_day DESC, created_at DESC, entry_id DESC
                 LIMIT ?9 OFFSET ?10"
            );
            let mut statement = conn.prepare(&sql)?;
            let rows = statement.query_map(
                params![
                    filter.book_id,
                    i64::from(filter.include_deleted),
                    kind,
                    filter.category_id,
                    filter.account_id,
                    filter.from_day,
                    filter.to_day,
                    filter.query,
                    limit,
                    offset,
                ],
                entry_from_row,
            )?;
            let items = rows.collect::<rusqlite::Result<Vec<_>>>()?;
            Ok((items, total))
        })
    }

    pub fn get_entry(&self, entry_id: &str) -> Result<Option<EntryRecord>> {
        self.with_conn(|conn| {
            let sql = format!("SELECT {ENTRY_COLUMNS} FROM ledger_entries WHERE entry_id = ?1");
            Ok(conn
                .query_row(&sql, params![entry_id], entry_from_row)
                .optional()?)
        })
    }

    /// 按完整 id 或 id 前缀找一笔。模型转述 id 时经常只报前几位。
    pub fn resolve_entry(&self, book_id: &str, query: &str) -> Result<EntryRecord> {
        let query = query.trim();
        if query.is_empty() {
            bail!("entry id is required");
        }
        if let Some(entry) = self.get_entry(query)? {
            if entry.book_id == book_id {
                return Ok(entry);
            }
        }
        self.with_conn(|conn| {
            let sql = format!(
                "SELECT {ENTRY_COLUMNS} FROM ledger_entries
                 WHERE book_id = ?1 AND deleted_at IS NULL AND entry_id LIKE ?2 || '%'
                 LIMIT 2"
            );
            let mut statement = conn.prepare(&sql)?;
            let rows = statement.query_map(params![book_id, query], entry_from_row)?;
            let matches = rows.collect::<rusqlite::Result<Vec<_>>>()?;
            match matches.len() {
                0 => bail!("no entry matches {query:?}"),
                1 => Ok(matches.into_iter().next().unwrap()),
                _ => bail!("{query:?} matches more than one entry; give more of the id"),
            }
        })
    }

    /// 改一笔账。`expected_revision` 是读到的版本号，对不上说明这笔在你
    /// 读出来之后被别处改过——报错而不是覆盖。
    pub fn update_entry(
        &self,
        entry_id: &str,
        expected_revision: i64,
        patch: EntryPatch,
    ) -> Result<EntryRecord> {
        self.with_tx(|tx| {
            let current: Option<(i64, Option<String>)> = tx
                .query_row(
                    "SELECT revision, deleted_at FROM ledger_entries WHERE entry_id = ?1",
                    params![entry_id],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()?;
            let Some((revision, deleted_at)) = current else {
                bail!("entry {entry_id} not found");
            };
            if deleted_at.is_some() {
                bail!("entry {entry_id} is deleted; restore it before editing");
            }
            if revision != expected_revision {
                bail!(
                    "entry {entry_id} changed since it was read (revision {revision}, expected \
                     {expected_revision}); read it again"
                );
            }
            // COALESCE 让「不动的字段」保持原值；需要清空的字段由调用方
            // 传 Some(None)，在下面单独展开成显式 NULL。
            tx.execute(
                "UPDATE ledger_entries SET
                     amount_minor      = COALESCE(?2, amount_minor),
                     currency          = COALESCE(?3, currency),
                     occurred_at       = COALESCE(?4, occurred_at),
                     occurred_day      = COALESCE(?5, occurred_day),
                     note              = COALESCE(?6, note),
                     merchant          = COALESCE(?7, merchant),
                     rate_status       = COALESCE(?8, rate_status),
                     revision          = revision + 1,
                     updated_at        = ?9
                 WHERE entry_id = ?1",
                params![
                    entry_id,
                    patch.amount_minor,
                    patch.currency,
                    patch.occurred_at,
                    patch.occurred_day,
                    patch.note,
                    patch.merchant,
                    patch.rate_status.map(|status| status.as_str()),
                    now_rfc3339(),
                ],
            )?;
            // 可空字段单独处理：这些字段「设为空」与「不修改」是两种意思，
            // COALESCE 表达不了。
            apply_nullable(tx, entry_id, "base_amount_minor", &patch.base_amount_minor)?;
            apply_nullable(tx, entry_id, "rate", &patch.rate)?;
            apply_nullable(tx, entry_id, "rate_source", &patch.rate_source)?;
            apply_nullable(tx, entry_id, "rate_at", &patch.rate_at)?;
            apply_nullable(tx, entry_id, "account_id", &patch.account_id)?;
            apply_nullable(tx, entry_id, "to_account_id", &patch.to_account_id)?;
            apply_nullable(tx, entry_id, "category_id", &patch.category_id)?;
            Ok(())
        })?;
        self.get_entry(entry_id)?
            .ok_or_else(|| anyhow::anyhow!("entry {entry_id} vanished during update"))
    }

    /// 软删除。已经删掉的再删一次不报错——重试安全。
    pub fn delete_entry(&self, entry_id: &str) -> Result<()> {
        self.with_tx(|tx| {
            let changed = tx.execute(
                "UPDATE ledger_entries SET deleted_at = ?2, revision = revision + 1,
                     updated_at = ?2
                 WHERE entry_id = ?1 AND deleted_at IS NULL",
                params![entry_id, now_rfc3339()],
            )?;
            if changed == 0 {
                let exists: bool = tx.query_row(
                    "SELECT EXISTS(SELECT 1 FROM ledger_entries WHERE entry_id = ?1)",
                    params![entry_id],
                    |row| row.get(0),
                )?;
                if !exists {
                    bail!("entry {entry_id} not found");
                }
            }
            Ok(())
        })
    }

    pub fn restore_entry(&self, entry_id: &str) -> Result<()> {
        self.with_tx(|tx| {
            let changed = tx.execute(
                "UPDATE ledger_entries SET deleted_at = NULL, revision = revision + 1,
                     updated_at = ?2
                 WHERE entry_id = ?1 AND deleted_at IS NOT NULL",
                params![entry_id, now_rfc3339()],
            )?;
            if changed == 0 {
                bail!("entry {entry_id} is not in the deleted state");
            }
            Ok(())
        })
    }

    /// 列出待换算的账目，供补算流程扫。
    pub fn pending_rate_entries(&self, book_id: &str, limit: i64) -> Result<Vec<EntryRecord>> {
        self.with_conn(|conn| {
            let sql = format!(
                "SELECT {ENTRY_COLUMNS} FROM ledger_entries
                 WHERE book_id = ?1 AND rate_status = 'pending' AND deleted_at IS NULL
                 ORDER BY occurred_day DESC
                 LIMIT ?2"
            );
            let mut statement = conn.prepare(&sql)?;
            let rows =
                statement.query_map(params![book_id, limit.clamp(1, 500)], entry_from_row)?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        })
    }
}

/// 可空列的三态更新：`None` 不动、`Some(None)` 置空、`Some(Some(v))` 设值。
fn apply_nullable<T: rusqlite::ToSql>(
    tx: &rusqlite::Transaction<'_>,
    entry_id: &str,
    column: &str,
    patch: &Option<Option<T>>,
) -> Result<()> {
    let Some(value) = patch else {
        return Ok(());
    };
    // 列名来自本文件里的字面量，不是外部输入。
    let sql = format!("UPDATE ledger_entries SET {column} = ?2 WHERE entry_id = ?1");
    tx.execute(&sql, params![entry_id, value])?;
    Ok(())
}
