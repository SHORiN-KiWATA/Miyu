//! 查账、改账、删账、看汇总。

use super::*;
use crate::ledger::entries::{EntryFilter, EntryPatch};
use crate::ledger::local_day_now;
use crate::ledger::money::{format_amount, parse_amount};

/// 一次最多返回几条。返回体是给模型读的，太长既费 token 又没人看得完；
/// 要更多让它翻页。
const DEFAULT_LIMIT: i64 = 20;
const MAX_LIMIT: i64 = 100;

pub(super) async fn list(args: Value, paths: MiyuPaths) -> Result<String> {
    let db = open_db(&paths)?;
    let book = resolve_book(&db, &args)?;

    let kind = match opt_str(&args, "kind") {
        Some(value) => Some(EntryKind::parse(value)?),
        None => None,
    };
    let category_id = match opt_str(&args, "category") {
        Some(value) => {
            let direction = kind.and_then(|kind| match kind {
                EntryKind::Income => Some(Direction::Income),
                EntryKind::Expense => Some(Direction::Expense),
                EntryKind::Transfer => None,
            });
            Some(
                db.resolve_category(&book.book_id, value, direction)?
                    .category_id,
            )
        }
        None => None,
    };
    let account_id = match opt_str(&args, "account") {
        Some(value) => Some(db.resolve_account(&book.book_id, value)?.account_id),
        None => None,
    };
    let limit = args
        .get("limit")
        .and_then(Value::as_i64)
        .unwrap_or(DEFAULT_LIMIT)
        .clamp(1, MAX_LIMIT);

    // 时间范围只有一个参数：`2026-09` 是整月，`2026-09-01..2026-09-15`
    // 是区间。分页留给面板——模型几乎不翻页，多一个 offset 只是多一处
    // 可以填错的地方。
    let period = opt_str(&args, "period")
        .map(str::to_string)
        .unwrap_or_else(|| local_day_now()[..7].to_string());
    let (from_day, to_day) = crate::ledger::stats::parse_period(&period)?;
    let filter = EntryFilter {
        book_id: book.book_id.clone(),
        kind,
        category_id,
        account_id,
        from_day: Some(from_day),
        to_day: Some(to_day),
        query: opt_str(&args, "query").map(str::to_string),
        include_deleted: false,
        offset: 0,
        limit,
    };
    let (entries, total) = db.list_entries(&filter)?;
    let items: Vec<Value> = entries
        .iter()
        .map(|entry| entry_json(&db, entry))
        .collect::<Result<_>>()?;

    Ok(json!({
        "ok": true,
        "book": book.name,
        "total": total,
        "shown": items.len(),
        "entries": items,
    })
    .to_string())
}

pub(super) async fn summary(args: Value, paths: MiyuPaths) -> Result<String> {
    let db = open_db(&paths)?;
    let book = resolve_book(&db, &args)?;
    let period = match opt_str(&args, "period") {
        Some(value) => value.to_string(),
        None => local_day_now()[..7].to_string(),
    };

    let summary = db.period_summary(&book, &period)?;
    let expenses = db.category_totals(&book.book_id, &period, EntryKind::Expense)?;
    let top: Vec<Value> = expenses
        .iter()
        .take(8)
        .map(|total| {
            json!({
                "name": total.name,
                "amount": format_amount(total.amount_minor, &book.base_currency),
                "count": total.entry_count,
            })
        })
        .collect();

    let mut budgets = Vec::new();
    for budget in db.list_budgets(&book.book_id)? {
        if let Some(status) = db.budget_status(&book, budget.category_id.as_deref(), &period)? {
            budgets.push(budget_json(&status));
        }
    }

    let mut result = json!({
        "ok": true,
        "book": book.name,
        "period": period,
        "currency": book.base_currency,
        "expense": format_amount(summary.expense_minor, &book.base_currency),
        "income": format_amount(summary.income_minor, &book.base_currency),
        "net": format_amount(summary.net_minor, &book.base_currency),
        "entries": summary.entry_count,
        "top_expense_categories": top,
    });
    let object = result.as_object_mut().unwrap();
    if !budgets.is_empty() {
        object.insert("budgets".to_string(), json!(budgets));
    }
    // 有账没算进合计时必须说出来，否则这份汇总是在骗人。
    if summary.pending_count > 0 {
        object.insert("pending_entries".to_string(), json!(summary.pending_count));
        object.insert(
            "pending_note".to_string(),
            json!("some entries have no exchange rate yet and are excluded from the totals"),
        );
    }
    Ok(result.to_string())
}

pub(super) async fn update(args: Value, paths: MiyuPaths) -> Result<String> {
    let db = open_db(&paths)?;
    let book = resolve_book(&db, &args)?;
    let entry = db.resolve_entry(&book.book_id, required_str(&args, "id")?)?;

    let mut patch = EntryPatch::default();
    let mut touched = false;

    if let Some(amount) = opt_str(&args, "amount") {
        patch.amount_minor = Some(parse_amount(amount, &entry.currency)?);
        touched = true;
    }
    if let Some(note) = args.get("note").and_then(Value::as_str) {
        patch.note = Some(note.trim().to_string());
        touched = true;
    }
    // 改分类时跟记一笔一样：没有的分类现建。筛选（list）那边不给这个待遇，
    // 那里建出来的分类一条账目都挂不上，只会给分类表长草。
    let mut created_category = None;
    if let Some(category) = opt_str(&args, "category") {
        let direction = match entry.kind {
            EntryKind::Income => Direction::Income,
            _ => Direction::Expense,
        };
        let resolved = match db.resolve_category_opt(&book.book_id, category, Some(direction))? {
            Some(found) => found,
            None => {
                super::add::ensure_direction_is_free(&db, &book.book_id, category, direction)?;
                let made = db.create_category(&book.book_id, category, direction, None, None)?;
                created_category = Some(made.name.clone());
                made
            }
        };
        patch.category_id = Some(Some(resolved.category_id));
        touched = true;
    }
    if let Some(date) = opt_str(&args, "date") {
        let (occurred_at, occurred_day) = day_to_timestamps(date)?;
        patch.occurred_at = Some(occurred_at);
        patch.occurred_day = Some(occurred_day);
        touched = true;
    }
    if !touched {
        bail!("nothing to update; pass amount, category, note or date");
    }

    // 读到的 revision 就在上面几行之前拿的，窗口只有这一次调用的长度。
    // WebUI 那边会把用户看到的版本号原样带回来，防的是跨请求的覆盖。
    let updated = db.update_entry(&entry.entry_id, entry.revision, patch)?;
    let mut result = json!({
        "ok": true,
        "book": book.name,
        "entry": entry_json(&db, &updated)?,
    });
    if let Some(name) = created_category {
        result.as_object_mut().unwrap().insert(
            "created_category".to_string(),
            json!(format!("{name} (new)")),
        );
    }
    Ok(result.to_string())
}

pub(super) async fn delete(args: Value, paths: MiyuPaths) -> Result<String> {
    let db = open_db(&paths)?;
    let book = resolve_book(&db, &args)?;
    let entry = db.resolve_entry(&book.book_id, required_str(&args, "id")?)?;
    db.delete_entry(&entry.entry_id)?;
    Ok(json!({
        "ok": true,
        "book": book.name,
        "deleted": entry_json(&db, &entry)?,
        "hint": "the entry is only marked deleted and can be restored from the dashboard",
    })
    .to_string())
}
