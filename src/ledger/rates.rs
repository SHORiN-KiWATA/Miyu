//! 汇率：按天缓存、失败降级、快照冻结。
//!
//! 记一笔外币账时的完整流程：
//!
//! 1. 币种与账本目标币种相同 → [`RateStatus::Same`]，不碰网络。
//! 2. 当天缓存里有 → 直接用。
//! 3. 去网上取 → 存进当天缓存，用它换算。
//! 4. 取不到 → [`RateStatus::Pending`]，**账照记**，换算后的金额留空等补算。
//!
//! 第 4 条是这个模块存在的主要理由。网络抖一下就记不成账，是把一个可以
//! 稍后修复的问题升级成了当场丢数据。
//!
//! 换算用的汇率连同来源和时刻一起写进账目行，**永不重算**：历史汇率之后
//! 怎么变，都不该改动已经记下的账。

use super::money::{convert_minor, format_rate, parse_rate};
use super::types::{BookRecord, EntryRecord, RateStatus};
use super::{local_day_now, now_rfc3339, LedgerDb};
use crate::config::ExchangeRatePluginConfig;
use anyhow::Result;
use rusqlite::{params, OptionalExtension};

/// 一次换算的结果，直接对应账目里那一组 `rate*` 字段。
#[derive(Clone, Debug)]
pub struct Conversion {
    pub base_amount_minor: Option<i64>,
    pub rate: Option<String>,
    pub rate_source: Option<String>,
    pub rate_at: Option<String>,
    pub status: RateStatus,
}

impl Conversion {
    fn same(amount_minor: i64) -> Self {
        Self {
            base_amount_minor: Some(amount_minor),
            rate: None,
            rate_source: None,
            rate_at: None,
            status: RateStatus::Same,
        }
    }

    fn pending() -> Self {
        Self {
            base_amount_minor: None,
            rate: None,
            rate_source: None,
            rate_at: None,
            status: RateStatus::Pending,
        }
    }
}

impl LedgerDb {
    /// 查当天缓存。汇率一天内的波动对个人记账没有意义，按天缓存把网络
    /// 请求从「每笔一次」压到「每个币种对每天一次」。
    pub(crate) fn cached_rate(
        &self,
        day: &str,
        base: &str,
        target: &str,
    ) -> Result<Option<(f64, String)>> {
        self.with_conn(|conn| {
            let row: Option<(String, String)> = conn
                .query_row(
                    "SELECT rate, source FROM ledger_rates
                     WHERE day = ?1 AND base = ?2 AND target = ?3",
                    params![day, base, target],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()?;
            match row {
                Some((rate, source)) => Ok(Some((parse_rate(&rate)?, source))),
                None => Ok(None),
            }
        })
    }

    pub(crate) fn store_rate(
        &self,
        day: &str,
        base: &str,
        target: &str,
        rate: f64,
        source: &str,
    ) -> Result<()> {
        self.with_tx(|tx| {
            tx.execute(
                "INSERT INTO ledger_rates (day, base, target, rate, source, fetched_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                 ON CONFLICT(day, base, target) DO UPDATE SET
                     rate = excluded.rate,
                     source = excluded.source,
                     fetched_at = excluded.fetched_at",
                params![day, base, target, format_rate(rate), source, now_rfc3339()],
            )?;
            Ok(())
        })
    }
}

/// 把一笔外币金额换算到账本目标币种。
///
/// 换算本身在 Rust 里做——模型只负责说「3000 日元」，乘法和取整不经过它。
/// 让模型算钱是在给一个概率模型派确定性工作。
pub async fn convert_for_book(
    db: &LedgerDb,
    book: &BookRecord,
    amount_minor: i64,
    currency: &str,
    config: &ExchangeRatePluginConfig,
) -> Conversion {
    if currency.eq_ignore_ascii_case(&book.base_currency) {
        return Conversion::same(amount_minor);
    }
    let day = local_day_now();
    let cached = db
        .cached_rate(&day, currency, &book.base_currency)
        .ok()
        .flatten();
    let (rate, source) = match cached {
        Some(hit) => hit,
        None => {
            match crate::tools::exchange_rate::fetch_rate(currency, &book.base_currency, config)
                .await
            {
                Ok((rate, source)) => {
                    // 缓存写失败不该拖垮记账——这一笔仍然用刚取到的汇率。
                    let _ = db.store_rate(&day, currency, &book.base_currency, rate, source);
                    (rate, source.to_string())
                }
                Err(error) => {
                    tracing::warn!(
                        %error, from = %currency, to = %book.base_currency,
                        "取汇率失败,账目按待换算记录"
                    );
                    return Conversion::pending();
                }
            }
        }
    };

    match convert_minor(amount_minor, currency, &book.base_currency, rate) {
        Ok(base_amount_minor) => Conversion {
            base_amount_minor: Some(base_amount_minor),
            rate: Some(format_rate(rate)),
            rate_source: Some(source),
            rate_at: Some(now_rfc3339()),
            status: RateStatus::Ok,
        },
        Err(error) => {
            tracing::warn!(%error, "汇率换算越界,账目按待换算记录");
            Conversion::pending()
        }
    }
}

/// 给一笔待换算的账目补上汇率。
///
/// 用的是**今天**的汇率，不是账目发生当天的——免费汇率源只给最新值，
/// 拿不到历史汇率。补算结果照样冻结成快照，`rate_at` 会如实记下补算的
/// 时刻，事后能看出它与发生时刻的距离。
pub async fn backfill_entry(
    db: &LedgerDb,
    book: &BookRecord,
    entry: &EntryRecord,
    config: &ExchangeRatePluginConfig,
) -> Result<bool> {
    if entry.rate_status != RateStatus::Pending {
        return Ok(false);
    }
    let conversion = convert_for_book(db, book, entry.amount_minor, &entry.currency, config).await;
    if conversion.status == RateStatus::Pending {
        return Ok(false);
    }
    let patch = super::entries::EntryPatch {
        base_amount_minor: Some(conversion.base_amount_minor),
        rate: Some(conversion.rate),
        rate_source: Some(conversion.rate_source),
        rate_at: Some(conversion.rate_at),
        rate_status: Some(conversion.status),
        ..Default::default()
    };
    db.update_entry(&entry.entry_id, entry.revision, patch)?;
    Ok(true)
}
