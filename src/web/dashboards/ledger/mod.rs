//! 记账面板的 HTTP 面。
//!
//! 账本是全局的（不按人格分层——钱是用户的，不是人格的），所以这里没有
//! `persona` 参数。读走 `require_auth`，写走 `require_mutation`。
//!
//! 金额同时给两种形态：`*_minor` 整数供前端排序与画图，`*_text` 已按币种
//! 精度格式化好供直接显示。让前端自己去判断日元没有小数位，迟早会错。
//!
//! 这里放请求类型、共享 helper 与两个读接口；写接口在 `write`，CSV 进出
//! 在 `csv_io`。

mod csv_io;
mod write;

pub(in crate::web) use csv_io::*;
pub(in crate::web) use write::*;

use crate::ledger::entries::{EntryFilter, EntryPatch, NewEntry, DUPLICATE_WINDOW_SECS};
use crate::ledger::money::{format_amount, parse_amount, validate_currency};
use crate::ledger::rates::{backfill_entry, convert_for_book};
use crate::ledger::types::*;
use crate::ledger::{local_day_now, local_day_of, now_rfc3339, LedgerDb};
use crate::web::*;
use chrono::{Local, NaiveDate, TimeZone};

/// 面板一页的流水条数。
const PAGE_SIZE: i64 = 30;

#[derive(Deserialize)]
pub(in crate::web) struct OverviewQuery {
    #[serde(default)]
    book: String,
    #[serde(default)]
    period: String,
}

#[derive(Deserialize)]
pub(in crate::web) struct EntriesQuery {
    #[serde(default)]
    book: String,
    #[serde(default)]
    period: String,
    #[serde(default)]
    kind: String,
    #[serde(default)]
    category: String,
    #[serde(default)]
    account: String,
    #[serde(default)]
    q: String,
    #[serde(default)]
    offset: i64,
    #[serde(default)]
    include_deleted: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::web) struct EntryBody {
    #[serde(default)]
    book: String,
    #[serde(default)]
    kind: String,
    amount: String,
    #[serde(default)]
    currency: String,
    #[serde(default)]
    category: String,
    #[serde(default)]
    account: String,
    #[serde(default)]
    to_account: String,
    #[serde(default)]
    note: String,
    #[serde(default)]
    merchant: String,
    #[serde(default)]
    date: String,
    #[serde(default)]
    force: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::web) struct EntryPatchBody {
    /// 读到的版本号原样带回来：面板停在旧数据上时不该静默覆盖别处的修改。
    revision: i64,
    #[serde(default)]
    amount: Option<String>,
    #[serde(default)]
    category: Option<String>,
    #[serde(default)]
    note: Option<String>,
    #[serde(default)]
    merchant: Option<String>,
    #[serde(default)]
    date: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::web) struct BookBody {
    name: String,
    currency: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::web) struct AccountBody {
    #[serde(default)]
    book: String,
    name: String,
    #[serde(default)]
    kind: String,
    #[serde(default)]
    currency: String,
    #[serde(default)]
    opening_balance: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::web) struct CategoryBody {
    #[serde(default)]
    book: String,
    name: String,
    #[serde(default)]
    direction: String,
    #[serde(default)]
    parent: String,
    #[serde(default)]
    icon: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::web) struct BudgetBody {
    #[serde(default)]
    book: String,
    #[serde(default)]
    category: String,
    amount: String,
}

fn open(state: &DaemonState) -> std::result::Result<LedgerDb, ApiError> {
    LedgerDb::open(&state.paths).map_err(|error| ApiError::internal(safe_error_message(&error)))
}

fn bad_request(error: anyhow::Error) -> ApiError {
    ApiError::new(StatusCode::BAD_REQUEST, safe_error_message(&error))
}

fn opt(value: &str) -> Option<&str> {
    let value = value.trim();
    (!value.is_empty()).then_some(value)
}

/// 面板永远带着账本走：没选就用第一本，一本都没有就现建一本默认的。
fn pick_book(db: &LedgerDb, requested: &str) -> std::result::Result<BookRecord, ApiError> {
    match opt(requested) {
        Some(value) => db.resolve_book(Some(value)).map_err(bad_request),
        None => {
            let books = db.list_books(false).map_err(bad_request)?;
            match books.into_iter().next() {
                Some(book) => Ok(book),
                None => db.ensure_default_book().map_err(bad_request),
            }
        }
    }
}

fn period_or_now(period: &str) -> String {
    match opt(period) {
        Some(value) => value.to_string(),
        None => local_day_now()[..7].to_string(),
    }
}

fn entry_dto(db: &LedgerDb, entry: &EntryRecord) -> anyhow::Result<Value> {
    let category = match &entry.category_id {
        Some(id) => db.category_display_name(id)?,
        None => String::new(),
    };
    Ok(json!({
        "id": entry.entry_id,
        "kind": entry.kind,
        "amount_minor": entry.amount_minor,
        "amount_text": format_amount(entry.amount_minor, &entry.currency),
        "currency": entry.currency,
        "base_amount_minor": entry.base_amount_minor,
        "base_amount_text": entry
            .base_amount_minor
            .map(|minor| format_amount(minor, &entry.base_currency)),
        "base_currency": entry.base_currency,
        "rate": entry.rate,
        "rate_status": entry.rate_status,
        "category_id": entry.category_id,
        "category": category,
        "account_id": entry.account_id,
        "to_account_id": entry.to_account_id,
        "date": entry.occurred_day,
        "note": entry.note,
        "merchant": entry.merchant,
        "revision": entry.revision,
        "deleted": entry.deleted_at.is_some(),
    }))
}

fn budget_dto(status: &BudgetStatus, budget_id: &str) -> Value {
    json!({
        "id": budget_id,
        "scope": status.scope,
        "limit_minor": status.limit_minor,
        "limit_text": format_amount(status.limit_minor, &status.currency),
        "used_minor": status.used_minor,
        "used_text": format_amount(status.used_minor, &status.currency),
        "currency": status.currency,
        "state": status.state,
    })
}

/// 面板首屏要的一切：账本清单、当月汇总、分类占比、逐日趋势、预算、账户。
pub(in crate::web) async fn dash_ledger_overview(
    State(state): State<DaemonState>,
    headers: HeaderMap,
    Query(query): Query<OverviewQuery>,
) -> std::result::Result<Json<Value>, ApiError> {
    require_auth(&headers, &state)?;
    let paths = state.paths.clone();
    let value = tokio::task::spawn_blocking(move || -> anyhow::Result<Value> {
        let db = LedgerDb::open(&paths)?;
        let books = db.list_books(false)?;
        let (book, requested_missing) = db.resolve_book_for_display(opt(&query.book))?;
        let period = period_or_now(&query.period);
        let summary = db.period_summary(&book, &period)?;
        let expense_totals = db.category_totals(&book.book_id, &period, EntryKind::Expense)?;
        let income_totals = db.category_totals(&book.book_id, &period, EntryKind::Income)?;
        let daily = db.daily_totals(
            &book.book_id,
            &format!("{period}-01"),
            &format!("{period}-31"),
        )?;

        let mut budgets = Vec::new();
        for budget in db.list_budgets(&book.book_id)? {
            if let Some(status) = db.budget_status(&book, budget.category_id.as_deref(), &period)? {
                budgets.push(budget_dto(&status, &budget.budget_id));
            }
        }

        let mut accounts = Vec::new();
        for account in db.list_accounts(&book.book_id, false)? {
            let balance = db.account_balance_minor(&account)?;
            accounts.push(json!({
                "id": account.account_id,
                "name": account.name,
                "kind": account.kind,
                "currency": account.currency,
                "balance_minor": balance,
                "balance_text": format_amount(balance, &account.currency),
            }));
        }

        let categories: Vec<Value> = db
            .list_categories(&book.book_id, None, false)?
            .into_iter()
            .map(|category| {
                json!({
                    "id": category.category_id,
                    "name": category.name,
                    "direction": category.direction,
                    "parent_id": category.parent_id,
                    "icon": category.icon,
                })
            })
            .collect();

        // 月份选择器只列真的有账目的月份，再补上当前月与正在看的这个月
        // ——那两个即使空着也得能选中。
        let mut periods = db.periods_with_entries(&book.book_id)?;
        for extra in [local_day_now()[..7].to_string(), period.clone()] {
            if !periods.contains(&extra) {
                periods.push(extra);
            }
        }
        periods.sort();
        periods.reverse();

        Ok(json!({
            "ok": true,
            "requested_book_missing": requested_missing,
            "periods": periods,
            "books": books
                .iter()
                .map(|book| json!({
                    "id": book.book_id,
                    "name": book.name,
                    "currency": book.base_currency,
                }))
                .collect::<Vec<_>>(),
            "book": {
                "id": book.book_id,
                "name": book.name,
                "currency": book.base_currency,
            },
            "period": period,
            "summary": {
                "expense_minor": summary.expense_minor,
                "expense_text": format_amount(summary.expense_minor, &book.base_currency),
                "income_minor": summary.income_minor,
                "income_text": format_amount(summary.income_minor, &book.base_currency),
                "net_minor": summary.net_minor,
                "net_text": format_amount(summary.net_minor.abs(), &book.base_currency),
                "net_negative": summary.net_minor < 0,
                "entries": summary.entry_count,
                "pending": summary.pending_count,
            },
            "expense_categories": totals_dto(&expense_totals, &book.base_currency),
            "income_categories": totals_dto(&income_totals, &book.base_currency),
            "daily": daily
                .iter()
                .map(|day| json!({
                    "day": day.day,
                    "expense_minor": day.expense_minor,
                    "income_minor": day.income_minor,
                }))
                .collect::<Vec<_>>(),
            "budgets": budgets,
            "accounts": accounts,
            "categories": categories,
        }))
    })
    .await
    .map_err(ApiError::internal)?
    .map_err(bad_request)?;
    Ok(Json(value))
}

fn totals_dto(totals: &[crate::ledger::stats::CategoryTotal], currency: &str) -> Vec<Value> {
    totals
        .iter()
        .map(|total| {
            json!({
                "id": total.category_id,
                "name": total.name,
                "icon": total.icon,
                "amount_minor": total.amount_minor,
                "amount_text": format_amount(total.amount_minor, currency),
                "count": total.entry_count,
            })
        })
        .collect()
}

pub(in crate::web) async fn dash_ledger_entries(
    State(state): State<DaemonState>,
    headers: HeaderMap,
    Query(query): Query<EntriesQuery>,
) -> std::result::Result<Json<Value>, ApiError> {
    require_auth(&headers, &state)?;
    let paths = state.paths.clone();
    let value = tokio::task::spawn_blocking(move || -> anyhow::Result<Value> {
        let db = LedgerDb::open(&paths)?;
        let book = match opt(&query.book) {
            Some(value) => db.resolve_book(Some(value))?,
            None => db.ensure_default_book()?,
        };
        let period = period_or_now(&query.period);
        let kind = match opt(&query.kind) {
            Some(value) => Some(EntryKind::parse(value)?),
            None => None,
        };
        let filter = EntryFilter {
            book_id: book.book_id.clone(),
            kind,
            category_id: opt(&query.category).map(str::to_string),
            account_id: opt(&query.account).map(str::to_string),
            from_day: Some(format!("{period}-01")),
            to_day: Some(format!("{period}-31")),
            query: opt(&query.q).map(str::to_string),
            include_deleted: query.include_deleted,
            offset: query.offset.max(0),
            limit: PAGE_SIZE,
        };
        let (entries, total) = db.list_entries(&filter)?;
        let items: Vec<Value> = entries
            .iter()
            .map(|entry| entry_dto(&db, entry))
            .collect::<anyhow::Result<_>>()?;
        Ok(json!({
            "ok": true,
            "total": total,
            "offset": filter.offset,
            "limit": filter.limit,
            "entries": items,
        }))
    })
    .await
    .map_err(ApiError::internal)?
    .map_err(bad_request)?;
    Ok(Json(value))
}

/// 把 `YYYY-MM-DD` 解析成（发生时刻, 本地自然日）。缺省是今天。
///
/// 与工具侧同一套口径：补记往日的账落在那天本地正午，两列永远自洽。
fn resolve_day(date: Option<&str>) -> anyhow::Result<(String, String)> {
    let Some(date) = date else {
        return Ok((now_rfc3339(), local_day_now()));
    };
    let parsed = NaiveDate::parse_from_str(date, "%Y-%m-%d")
        .map_err(|_| anyhow::anyhow!("date must look like 2026-09-08, got {date:?}"))?;
    let noon = parsed
        .and_hms_opt(12, 0, 0)
        .ok_or_else(|| anyhow::anyhow!("invalid date {date:?}"))?;
    let local = Local
        .from_local_datetime(&noon)
        .single()
        .ok_or_else(|| anyhow::anyhow!("ambiguous local time for {date:?}"))?;
    let occurred_at = local.to_utc().to_rfc3339();
    Ok((occurred_at.clone(), local_day_of(&occurred_at)?))
}
