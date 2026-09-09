//! 赞助记账：一笔一行，总额与榜单现算。
//!
//! 刻意不存「赞助人档案」那种汇总行。汇总一旦落库，每次增删改都得重算它，而
//! 重算漏一条路径就是长期对不上账——这类缓存的收益（一次 SUM）远小于它的失效
//! 面（AGENTS §3.2）。表里现在最多几百行，`SUM`+`GROUP BY` 是微秒级的事。
//!
//! 金额一律存最小货币单位的整数（分）。钱不用浮点，这条没有例外。

use crate::state::conversation_db::*;

/// 一笔赞助。
#[derive(Debug, Clone, Serialize)]
pub struct SponsorRecord {
    pub record_id: i64,
    pub platform: String,
    pub account_id: String,
    pub sponsor_id: String,
    pub sponsor_name: String,
    /// 原始金额，最小货币单位（分）。
    pub amount_minor: i64,
    /// `CNY` / `USD`。
    pub currency: String,
    /// 记账当刻折算出的人民币值（分）。人民币记录等于 `amount_minor`。
    pub cny_minor: i64,
    pub fx_rate: f64,
    /// `live` / `cached` / `fallback` / 空（人民币不需要折算）。
    pub fx_source: String,
    pub note: String,
    pub recorded_by: String,
    pub sponsored_at: String,
    pub created_at: String,
    pub updated_at: String,
}

/// 一个人的合计。榜单与 `query` 共用。
#[derive(Debug, Clone, Serialize)]
pub struct SponsorTotal {
    pub sponsor_id: String,
    pub sponsor_name: String,
    /// 折算后的人民币合计（分）——榜单的排序依据。
    pub cny_minor: i64,
    /// 按币种分列的原始合计（分），键是货币代码。
    pub by_currency: BTreeMap<String, i64>,
    pub record_count: i64,
    pub first_sponsored_at: String,
    pub last_sponsored_at: String,
}

/// 全库概览。
#[derive(Debug, Clone, Default, Serialize)]
pub struct SponsorSummary {
    pub sponsor_count: i64,
    pub record_count: i64,
    pub cny_minor: i64,
    pub by_currency: BTreeMap<String, i64>,
    pub last_sponsored_at: String,
}

/// 新记一笔时要填的东西。
#[derive(Debug, Clone)]
pub struct NewSponsorRecord {
    pub platform: String,
    pub account_id: String,
    pub sponsor_id: String,
    pub sponsor_name: String,
    pub amount_minor: i64,
    pub currency: String,
    pub cny_minor: i64,
    pub fx_rate: f64,
    pub fx_source: String,
    pub note: String,
    pub recorded_by: String,
    pub sponsored_at: String,
}

/// 榜单/列表的排序口径。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SponsorOrder {
    /// 折算人民币合计，从多到少。
    Amount,
    /// 笔数，从多到少。
    Count,
    /// 最近一笔的时间，从新到旧。
    Recent,
}

impl SponsorOrder {
    pub fn parse(value: &str) -> Self {
        match value.trim().to_ascii_lowercase().as_str() {
            "count" | "records" => Self::Count,
            "recent" | "time" | "latest" => Self::Recent,
            _ => Self::Amount,
        }
    }
}

const RECORD_COLUMNS: &str = "record_id, platform, account_id, sponsor_id, sponsor_name, \
     amount_minor, currency, cny_minor, fx_rate, fx_source, note, recorded_by, \
     sponsored_at, created_at, updated_at";

fn map_sponsor_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<SponsorRecord> {
    Ok(SponsorRecord {
        record_id: row.get(0)?,
        platform: row.get(1)?,
        account_id: row.get(2)?,
        sponsor_id: row.get(3)?,
        sponsor_name: row.get(4)?,
        amount_minor: row.get(5)?,
        currency: row.get(6)?,
        cny_minor: row.get(7)?,
        fx_rate: row.get(8)?,
        fx_source: row.get(9)?,
        note: row.get(10)?,
        recorded_by: row.get(11)?,
        sponsored_at: row.get(12)?,
        created_at: row.get(13)?,
        updated_at: row.get(14)?,
    })
}

impl ConversationDb {
    pub fn add_sponsor_record(&self, new: &NewSponsorRecord) -> Result<SponsorRecord> {
        let now = Utc::now().to_rfc3339();
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO sponsor_records (platform, account_id, sponsor_id, sponsor_name, \
             amount_minor, currency, cny_minor, fx_rate, fx_source, note, recorded_by, \
             sponsored_at, created_at, updated_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?13)",
            params![
                new.platform,
                new.account_id,
                new.sponsor_id,
                new.sponsor_name,
                new.amount_minor,
                new.currency,
                new.cny_minor,
                new.fx_rate,
                new.fx_source,
                new.note,
                new.recorded_by,
                if new.sponsored_at.trim().is_empty() {
                    now.clone()
                } else {
                    new.sponsored_at.clone()
                },
                now,
            ],
        )?;
        let record_id = conn.last_insert_rowid();
        drop(conn);
        self.sponsor_record(record_id)?
            .context("sponsor record vanished right after insert")
    }

    pub fn sponsor_record(&self, record_id: i64) -> Result<Option<SponsorRecord>> {
        let conn = self.conn.lock().unwrap();
        let mut statement = conn.prepare(&format!(
            "SELECT {RECORD_COLUMNS} FROM sponsor_records WHERE record_id = ?1"
        ))?;
        let mut rows = statement.query_map(params![record_id], map_sponsor_row)?;
        Ok(match rows.next() {
            Some(row) => Some(row?),
            None => None,
        })
    }

    /// 一个人的全部记录，新的在前。
    pub fn sponsor_records_for(
        &self,
        sponsor_id: &str,
        limit: usize,
    ) -> Result<Vec<SponsorRecord>> {
        let conn = self.conn.lock().unwrap();
        let mut statement = conn.prepare(&format!(
            "SELECT {RECORD_COLUMNS} FROM sponsor_records WHERE sponsor_id = ?1 \
             ORDER BY sponsored_at DESC, record_id DESC LIMIT ?2"
        ))?;
        let rows = statement.query_map(params![sponsor_id, limit as i64], map_sponsor_row)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// 明细，新的在前。
    pub fn sponsor_records(&self, limit: usize, offset: usize) -> Result<Vec<SponsorRecord>> {
        let conn = self.conn.lock().unwrap();
        let mut statement = conn.prepare(&format!(
            "SELECT {RECORD_COLUMNS} FROM sponsor_records \
             ORDER BY sponsored_at DESC, record_id DESC LIMIT ?1 OFFSET ?2"
        ))?;
        let rows = statement.query_map(params![limit as i64, offset as i64], map_sponsor_row)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn update_sponsor_record(
        &self,
        record_id: i64,
        note: Option<&str>,
        sponsor_name: Option<&str>,
    ) -> Result<Option<SponsorRecord>> {
        if note.is_none() && sponsor_name.is_none() {
            return self.sponsor_record(record_id);
        }
        let conn = self.conn.lock().unwrap();
        // 金额与汇率不给改:那两个是记账当刻冻结的事实,要改就是记错了、该删了
        // 重记,而不是把历史悄悄改成另一个样子。
        if let Some(note) = note {
            conn.execute(
                "UPDATE sponsor_records SET note = ?2, updated_at = ?3 WHERE record_id = ?1",
                params![record_id, note, Utc::now().to_rfc3339()],
            )?;
        }
        if let Some(name) = sponsor_name {
            conn.execute(
                "UPDATE sponsor_records SET sponsor_name = ?2, updated_at = ?3 WHERE record_id = ?1",
                params![record_id, name, Utc::now().to_rfc3339()],
            )?;
        }
        drop(conn);
        self.sponsor_record(record_id)
    }

    pub fn delete_sponsor_record(&self, record_id: i64) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        Ok(conn.execute(
            "DELETE FROM sponsor_records WHERE record_id = ?1",
            params![record_id],
        )? > 0)
    }

    /// 按人合计。`order` 决定名次口径，`limit` 是榜单长度。
    pub fn sponsor_totals(&self, order: SponsorOrder, limit: usize) -> Result<Vec<SponsorTotal>> {
        let order_sql = match order {
            SponsorOrder::Amount => "total_cny DESC, records DESC, last_at DESC",
            SponsorOrder::Count => "records DESC, total_cny DESC, last_at DESC",
            SponsorOrder::Recent => "last_at DESC, total_cny DESC",
        };
        let conn = self.conn.lock().unwrap();
        // 昵称取最近一笔留下的那个:人会改名,榜上该显示他现在叫什么。
        let mut statement = conn.prepare(&format!(
            "SELECT sponsor_id, \
                    SUM(cny_minor) AS total_cny, \
                    COUNT(*) AS records, \
                    MIN(sponsored_at) AS first_at, \
                    MAX(sponsored_at) AS last_at, \
                    (SELECT sponsor_name FROM sponsor_records inner_records \
                       WHERE inner_records.sponsor_id = sponsor_records.sponsor_id \
                         AND sponsor_name <> '' \
                       ORDER BY sponsored_at DESC, record_id DESC LIMIT 1) AS display_name \
             FROM sponsor_records GROUP BY sponsor_id ORDER BY {order_sql} LIMIT ?1"
        ))?;
        let rows = statement.query_map(params![limit as i64], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, Option<String>>(5)?.unwrap_or_default(),
            ))
        })?;
        // 先把行收干净再放锁:下面按人查币种分列还要再拿同一把锁,连接锁不可
        // 重入,握着它递归进去就是死锁。
        let collected = rows.collect::<rusqlite::Result<Vec<_>>>()?;
        drop(statement);
        drop(conn);
        let mut totals = Vec::new();
        for (sponsor_id, cny_minor, record_count, first_at, last_at, display_name) in collected {
            totals.push(SponsorTotal {
                by_currency: self.sponsor_currency_totals(Some(&sponsor_id))?,
                sponsor_id,
                sponsor_name: display_name,
                cny_minor,
                record_count,
                first_sponsored_at: first_at,
                last_sponsored_at: last_at,
            });
        }
        Ok(totals)
    }

    /// 一个人（或全库）按币种分列的原始合计。
    pub fn sponsor_currency_totals(
        &self,
        sponsor_id: Option<&str>,
    ) -> Result<BTreeMap<String, i64>> {
        let conn = self.conn.lock().unwrap();
        let mut totals = BTreeMap::new();
        match sponsor_id {
            Some(sponsor_id) => {
                let mut statement = conn.prepare(
                    "SELECT currency, SUM(amount_minor) FROM sponsor_records \
                     WHERE sponsor_id = ?1 GROUP BY currency",
                )?;
                let rows = statement.query_map(params![sponsor_id], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
                })?;
                for row in rows {
                    let (currency, amount) = row?;
                    totals.insert(currency, amount);
                }
            }
            None => {
                let mut statement = conn.prepare(
                    "SELECT currency, SUM(amount_minor) FROM sponsor_records GROUP BY currency",
                )?;
                let rows = statement.query_map([], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
                })?;
                for row in rows {
                    let (currency, amount) = row?;
                    totals.insert(currency, amount);
                }
            }
        }
        Ok(totals)
    }

    pub fn sponsor_summary(&self) -> Result<SponsorSummary> {
        let by_currency = self.sponsor_currency_totals(None)?;
        let conn = self.conn.lock().unwrap();
        let mut statement = conn.prepare(
            "SELECT COUNT(DISTINCT sponsor_id), COUNT(*), COALESCE(SUM(cny_minor), 0), \
                    COALESCE(MAX(sponsored_at), '') FROM sponsor_records",
        )?;
        let (sponsor_count, record_count, cny_minor, last_sponsored_at) =
            statement.query_row([], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, String>(3)?,
                ))
            })?;
        Ok(SponsorSummary {
            sponsor_count,
            record_count,
            cny_minor,
            by_currency,
            last_sponsored_at,
        })
    }
}
