//! 汇总统计与预算。
//!
//! 所有汇总都以账本目标币种（`base_amount_minor`）为口径——那是唯一可以
//! 跨币种相加的列。待换算的账目**不进汇总**，而是单独计数报出来：拿一个
//! 猜的汇率把数字凑齐，比明说「还有 3 笔没算」要糟得多。

use super::types::*;
use super::{new_id, now_rfc3339, LedgerDb};
use anyhow::{bail, Result};
use rusqlite::{params, OptionalExtension};

/// 一个月的收支概览。
#[derive(Clone, Debug, Default, serde::Serialize)]
pub struct PeriodSummary {
    /// `YYYY-MM`。
    pub period: String,
    pub currency: String,
    pub expense_minor: i64,
    pub income_minor: i64,
    /// 收入减支出，可以是负数。
    pub net_minor: i64,
    pub entry_count: i64,
    /// 待换算、因而没能进上面几个数的账目数。
    pub pending_count: i64,
}

/// 分类维度的汇总，面板上的占比条与工具的「花在哪了」都用它。
#[derive(Clone, Debug, serde::Serialize)]
pub struct CategoryTotal {
    pub category_id: Option<String>,
    pub name: String,
    pub icon: String,
    pub amount_minor: i64,
    pub entry_count: i64,
}

/// 某一天的支出合计，趋势图的一个点。
#[derive(Clone, Debug, serde::Serialize)]
pub struct DayTotal {
    pub day: String,
    pub expense_minor: i64,
    pub income_minor: i64,
}

/// 把 `YYYY-MM` 展开成本地自然日的闭区间。
///
/// 上界用 `-31` 而不是当月实际天数：字符串比较下 `2026-09-31` 落在
/// `2026-09-30` 之后、`2026-10-01` 之前，既盖全当月又不会漏进下月，
/// 还省掉一次「这个月有几天」的判断。
fn month_range(period: &str) -> Result<(String, String)> {
    let period = period.trim();
    let valid = period.len() == 7
        && period.as_bytes()[4] == b'-'
        && period[..4].chars().all(|c| c.is_ascii_digit())
        && period[5..].chars().all(|c| c.is_ascii_digit());
    if !valid {
        bail!("period must look like 2026-09, got {period:?}");
    }
    Ok((format!("{period}-01"), format!("{period}-31")))
}

/// 解析调用方给的时间范围：`2026-09` 是整月，`2026-09-01..2026-09-15`
/// 是自定义区间。
///
/// 两种写法收在一个参数里，是为了让工具少一对 from/to——模型多一个参数
/// 就多一个填错的地方，而「这个月」和「这半个月」本来就是同一件事的
/// 两种粒度。
pub fn parse_period(period: &str) -> Result<(String, String)> {
    let period = period.trim();
    let Some((from, to)) = period.split_once("..") else {
        return month_range(period);
    };
    let from = check_day(from.trim())?;
    let to = check_day(to.trim())?;
    if from > to {
        bail!("the range starts after it ends: {from}..{to}");
    }
    Ok((from, to))
}

fn check_day(value: &str) -> Result<String> {
    chrono::NaiveDate::parse_from_str(value, "%Y-%m-%d")
        .map_err(|_| anyhow::anyhow!("a day must look like 2026-09-08, got {value:?}"))?;
    Ok(value.to_string())
}

impl LedgerDb {
    /// 这本账里**真的有账目**的月份，新的在前。
    ///
    /// 面板的月份选择器只该列这些：列出一整年空月份，让人以为那些月份有账
    /// 却点开是空的（09-09 用户走查意见）。当前月由调用方补上——那个月即使
    /// 还没记账也要能选中。
    ///
    /// `substr` 用不上索引，但一本账就几千条，扫一遍是微秒级的事。
    pub fn periods_with_entries(&self, book_id: &str) -> Result<Vec<String>> {
        self.with_conn(|conn| {
            let mut statement = conn.prepare(
                "SELECT DISTINCT substr(occurred_day, 1, 7) AS period
                 FROM ledger_entries
                 WHERE book_id = ?1 AND deleted_at IS NULL
                 ORDER BY period DESC",
            )?;
            let rows = statement.query_map(params![book_id], |row| row.get::<_, String>(0))?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        })
    }

    pub fn period_summary(&self, book: &BookRecord, period: &str) -> Result<PeriodSummary> {
        let (from, to) = month_range(period)?;
        self.with_conn(|conn| {
            let mut summary = PeriodSummary {
                period: period.to_string(),
                currency: book.base_currency.clone(),
                ..Default::default()
            };
            let mut statement = conn.prepare(
                "SELECT kind, COALESCE(SUM(base_amount_minor), 0), COUNT(*)
                 FROM ledger_entries
                 WHERE book_id = ?1 AND deleted_at IS NULL
                   AND rate_status <> 'pending'
                   AND occurred_day BETWEEN ?2 AND ?3
                 GROUP BY kind",
            )?;
            let rows = statement.query_map(params![book.book_id, from, to], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            })?;
            for row in rows {
                let (kind, total, count) = row?;
                summary.entry_count += count;
                match kind.as_str() {
                    "expense" => summary.expense_minor = total,
                    "income" => summary.income_minor = total,
                    // 转账在账户之间搬钱，不是收也不是支，只计入笔数。
                    _ => {}
                }
            }
            summary.net_minor = summary.income_minor - summary.expense_minor;
            summary.pending_count = conn.query_row(
                "SELECT COUNT(*) FROM ledger_entries
                 WHERE book_id = ?1 AND deleted_at IS NULL AND rate_status = 'pending'
                   AND occurred_day BETWEEN ?2 AND ?3",
                params![book.book_id, from, to],
                |row| row.get(0),
            )?;
            Ok(summary)
        })
    }

    /// 按一级分类汇总。二级分类的钱算进它的父级——面板上先看大类，
    /// 点进去才看细目。
    pub fn category_totals(
        &self,
        book_id: &str,
        period: &str,
        kind: EntryKind,
    ) -> Result<Vec<CategoryTotal>> {
        let (from, to) = month_range(period)?;
        self.with_conn(|conn| {
            let mut statement = conn.prepare(
                "SELECT
                     COALESCE(parent.category_id, child.category_id) AS root_id,
                     COALESCE(parent.name, child.name, '未分类')      AS root_name,
                     COALESCE(parent.icon, child.icon, '')            AS root_icon,
                     SUM(entry.base_amount_minor),
                     COUNT(*)
                 FROM ledger_entries AS entry
                 LEFT JOIN ledger_categories AS child
                        ON child.category_id = entry.category_id
                 LEFT JOIN ledger_categories AS parent
                        ON parent.category_id = child.parent_id
                 WHERE entry.book_id = ?1 AND entry.deleted_at IS NULL
                   AND entry.rate_status <> 'pending'
                   AND entry.kind = ?2
                   AND entry.occurred_day BETWEEN ?3 AND ?4
                 GROUP BY root_id, root_name, root_icon
                 ORDER BY SUM(entry.base_amount_minor) DESC",
            )?;
            let rows = statement.query_map(params![book_id, kind.as_str(), from, to], |row| {
                Ok(CategoryTotal {
                    category_id: row.get(0)?,
                    name: row.get(1)?,
                    icon: row.get(2)?,
                    amount_minor: row.get(3)?,
                    entry_count: row.get(4)?,
                })
            })?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        })
    }

    /// 逐日收支，趋势图用。只返回有账的日子，补零交给前端——省得为一个
    /// 空月份返回三十个零。
    pub fn daily_totals(
        &self,
        book_id: &str,
        from_day: &str,
        to_day: &str,
    ) -> Result<Vec<DayTotal>> {
        self.with_conn(|conn| {
            let mut statement = conn.prepare(
                "SELECT occurred_day,
                        COALESCE(SUM(CASE WHEN kind = 'expense' THEN base_amount_minor END), 0),
                        COALESCE(SUM(CASE WHEN kind = 'income'  THEN base_amount_minor END), 0)
                 FROM ledger_entries
                 WHERE book_id = ?1 AND deleted_at IS NULL AND rate_status <> 'pending'
                   AND occurred_day BETWEEN ?2 AND ?3
                 GROUP BY occurred_day
                 ORDER BY occurred_day",
            )?;
            let rows = statement.query_map(params![book_id, from_day, to_day], |row| {
                Ok(DayTotal {
                    day: row.get(0)?,
                    expense_minor: row.get(1)?,
                    income_minor: row.get(2)?,
                })
            })?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        })
    }

    // ── 预算 ────────────────────────────────────────────────

    /// 设定月度预算。同一个作用域（总额或某个分类）只有一条，重复设置
    /// 就是改额度。
    pub fn set_budget(
        &self,
        book_id: &str,
        category_id: Option<&str>,
        amount_minor: i64,
    ) -> Result<BudgetRecord> {
        if amount_minor <= 0 {
            bail!("budget amount must be greater than zero");
        }
        let now = now_rfc3339();
        self.with_tx(|tx| {
            let existing: Option<String> = tx
                .query_row(
                    "SELECT budget_id FROM ledger_budgets
                     WHERE book_id = ?1
                       AND ((category_id IS NULL AND ?2 IS NULL) OR category_id = ?2)",
                    params![book_id, category_id],
                    |row| row.get(0),
                )
                .optional()?;
            match existing {
                Some(budget_id) => {
                    tx.execute(
                        "UPDATE ledger_budgets SET amount_minor = ?2, active = 1, updated_at = ?3
                         WHERE budget_id = ?1",
                        params![budget_id, amount_minor, now],
                    )?;
                }
                None => {
                    tx.execute(
                        "INSERT INTO ledger_budgets
                         (budget_id, book_id, category_id, amount_minor, active, created_at, updated_at)
                         VALUES (?1, ?2, ?3, ?4, 1, ?5, ?5)",
                        params![new_id("bg"), book_id, category_id, amount_minor, now],
                    )?;
                }
            }
            Ok(())
        })?;
        self.get_budget(book_id, category_id)?
            .ok_or_else(|| anyhow::anyhow!("budget vanished right after being written"))
    }

    pub fn get_budget(
        &self,
        book_id: &str,
        category_id: Option<&str>,
    ) -> Result<Option<BudgetRecord>> {
        self.with_conn(|conn| {
            Ok(conn
                .query_row(
                    "SELECT budget_id, book_id, category_id, amount_minor, active, created_at, updated_at
                     FROM ledger_budgets
                     WHERE book_id = ?1
                       AND ((category_id IS NULL AND ?2 IS NULL) OR category_id = ?2)",
                    params![book_id, category_id],
                    |row| {
                        Ok(BudgetRecord {
                            budget_id: row.get(0)?,
                            book_id: row.get(1)?,
                            category_id: row.get(2)?,
                            amount_minor: row.get(3)?,
                            active: row.get::<_, i64>(4)? != 0,
                            created_at: row.get(5)?,
                            updated_at: row.get(6)?,
                        })
                    },
                )
                .optional()?)
        })
    }

    pub fn list_budgets(&self, book_id: &str) -> Result<Vec<BudgetRecord>> {
        self.with_conn(|conn| {
            let mut statement = conn.prepare(
                "SELECT budget_id, book_id, category_id, amount_minor, active, created_at, updated_at
                 FROM ledger_budgets
                 WHERE book_id = ?1 AND active = 1
                 ORDER BY category_id IS NOT NULL, category_id",
            )?;
            let rows = statement.query_map(params![book_id], |row| {
                Ok(BudgetRecord {
                    budget_id: row.get(0)?,
                    book_id: row.get(1)?,
                    category_id: row.get(2)?,
                    amount_minor: row.get(3)?,
                    active: row.get::<_, i64>(4)? != 0,
                    created_at: row.get(5)?,
                    updated_at: row.get(6)?,
                })
            })?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        })
    }

    pub fn delete_budget(&self, budget_id: &str) -> Result<()> {
        self.with_tx(|tx| {
            let changed = tx.execute(
                "DELETE FROM ledger_budgets WHERE budget_id = ?1",
                params![budget_id],
            )?;
            if changed == 0 {
                bail!("budget {budget_id} not found");
            }
            Ok(())
        })
    }

    /// 某个作用域当月的执行情况。没设预算返回 `None`。
    pub fn budget_status(
        &self,
        book: &BookRecord,
        category_id: Option<&str>,
        period: &str,
    ) -> Result<Option<BudgetStatus>> {
        let Some(budget) = self.get_budget(&book.book_id, category_id)? else {
            return Ok(None);
        };
        if !budget.active {
            return Ok(None);
        }
        let (from, to) = month_range(period)?;
        let used_minor: i64 = self.with_conn(|conn| {
            // 分类预算把子分类的花销算进父分类：给「餐饮」设的额度，
            // 记在「餐饮/外卖」上的钱当然要占额度。
            Ok(conn.query_row(
                "SELECT COALESCE(SUM(entry.base_amount_minor), 0)
                 FROM ledger_entries AS entry
                 LEFT JOIN ledger_categories AS child
                        ON child.category_id = entry.category_id
                 WHERE entry.book_id = ?1 AND entry.deleted_at IS NULL
                   AND entry.rate_status <> 'pending'
                   AND entry.kind = 'expense'
                   AND entry.occurred_day BETWEEN ?3 AND ?4
                   AND (?2 IS NULL
                        OR entry.category_id = ?2
                        OR child.parent_id = ?2)",
                params![book.book_id, category_id, from, to],
                |row| row.get(0),
            )?)
        })?;

        let scope = match category_id {
            Some(category_id) => {
                let name = self.category_display_name(category_id)?;
                format!("category:{name}")
            }
            None => "total".to_string(),
        };
        Ok(Some(BudgetStatus {
            scope,
            period: period.to_string(),
            limit_minor: budget.amount_minor,
            used_minor,
            currency: book.base_currency.clone(),
            state: BudgetState::from_usage(used_minor, budget.amount_minor),
        }))
    }

    /// 记完一笔后要不要提醒。
    ///
    /// 先看这笔所属分类的预算，没有再看总预算；两个都平安就返回 `None`，
    /// 让结果里干脆不出现 budget 字段——没有话说的时候不占 token。
    pub fn budget_alert_for_entry(
        &self,
        book: &BookRecord,
        entry: &EntryRecord,
    ) -> Result<Option<BudgetStatus>> {
        if entry.kind != EntryKind::Expense {
            return Ok(None);
        }
        let period = entry.occurred_day.get(..7).unwrap_or_default().to_string();
        if period.len() != 7 {
            return Ok(None);
        }

        // 子分类的预算挂在父分类上，所以两级都要查一遍。
        let mut scopes: Vec<Option<String>> = Vec::new();
        if let Some(category_id) = &entry.category_id {
            scopes.push(Some(category_id.clone()));
            if let Some(category) = self.get_category(category_id)? {
                if let Some(parent_id) = category.parent_id {
                    scopes.push(Some(parent_id));
                }
            }
        }
        scopes.push(None);

        for scope in scopes {
            let status = self.budget_status(book, scope.as_deref(), &period)?;
            if let Some(status) = status {
                if status.state != BudgetState::Ok {
                    return Ok(Some(status));
                }
            }
        }
        Ok(None)
    }
}
