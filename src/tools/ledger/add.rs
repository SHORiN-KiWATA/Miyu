//! 记一笔。
//!
//! 顺序是刻意的：**先查重、再换汇、最后落库**。查重放在网络请求前面，
//! 一次重演风暴就不会连带打出几次汇率请求；换汇失败不阻断落库，账照记、
//! 数字待补。

use super::*;
use crate::ledger::entries::{NewEntry, DUPLICATE_WINDOW_SECS};
use crate::ledger::money::{parse_amount, validate_currency};
use crate::ledger::rates::convert_for_book;
use crate::ledger::{local_day_now, now_rfc3339};

pub(super) async fn run(args: Value, paths: MiyuPaths, config: AppConfig) -> Result<String> {
    let db = open_db(&paths)?;
    // 一本账都没有时先建一本，别让「记一笔」先撞上一道配置题。
    if db.list_books(false)?.is_empty() && opt_str(&args, "book").is_none() {
        db.ensure_default_book()?;
    }
    let book = resolve_book(&db, &args)?;

    let kind = match opt_str(&args, "kind") {
        Some(value) => EntryKind::parse(value)?,
        None => EntryKind::Expense,
    };
    let currency = match opt_str(&args, "currency") {
        Some(value) => validate_currency(value)?,
        None => book.base_currency.clone(),
    };
    let amount_minor = parse_amount(required_str(&args, "amount")?, &currency)?;
    let (occurred_at, occurred_day) = resolve_when(&args)?;
    let note = opt_str(&args, "note").unwrap_or_default().to_string();

    let (account_id, to_account_id, category_id) = resolve_targets(&db, &book, &args, kind)?;

    // 重复闸。端点抖动时模型会把「记一笔」重演成同义变体，而账本里多出
    // 一笔比少一笔更难发现——挡下来交给调用方确认，比事后对账便宜。
    if !opt_bool(&args, "force") {
        if let Some(existing) = db.find_recent_duplicate(
            &book.book_id,
            kind,
            amount_minor,
            &currency,
            category_id.as_deref(),
            &note,
            DUPLICATE_WINDOW_SECS,
        )? {
            return Ok(json!({
                "ok": false,
                "reason": "possible_duplicate",
                "existing": entry_json(&db, &existing)?,
                "hint": "an identical entry was recorded minutes ago; pass force=true to record this one anyway"
            })
            .to_string());
        }
    }

    let conversion = convert_for_book(
        &db,
        &book,
        amount_minor,
        &currency,
        &config.plugins.exchange_rate,
    )
    .await;

    let entry = db.add_entry(NewEntry {
        book_id: book.book_id.clone(),
        kind,
        amount_minor,
        currency,
        base_amount_minor: conversion.base_amount_minor,
        base_currency: book.base_currency.clone(),
        rate: conversion.rate,
        rate_source: conversion.rate_source,
        rate_at: conversion.rate_at,
        rate_status: conversion.status,
        account_id,
        to_account_id,
        category_id,
        occurred_at,
        occurred_day,
        note,
        // 商家并进备注：模型侧少一个参数，DB 列留给 CSV 导入的数据。
        merchant: String::new(),
        source: EntrySource::Chat,
    })?;

    let mut result = json!({
        "ok": true,
        "book": book.name,
        "entry": entry_json(&db, &entry)?,
    });
    // 预算平安时干脆不出现这个字段——没话说的时候不占 token。
    if let Some(status) = db.budget_alert_for_entry(&book, &entry)? {
        result
            .as_object_mut()
            .unwrap()
            .insert("budget".to_string(), budget_json(&status));
    }
    Ok(result.to_string())
}

/// 解析发生时间。只认 `YYYY-MM-DD`，缺省是今天。
fn resolve_when(args: &Value) -> Result<(String, String)> {
    match opt_str(args, "date") {
        Some(date) => day_to_timestamps(date),
        None => Ok((now_rfc3339(), local_day_now())),
    }
}

/// 解析账户与分类，并把两种记账形态的约束在这里讲清楚。
///
/// 转账要两个账户、不要分类；收支要分类、不要转入方。这些约束库层的
/// CHECK 也会兜住，但在这里拦下来能给出一句人话，而不是一条 SQLite 报错。
fn resolve_targets(
    db: &LedgerDb,
    book: &BookRecord,
    args: &Value,
    kind: EntryKind,
) -> Result<(Option<String>, Option<String>, Option<String>)> {
    let account = match opt_str(args, "account") {
        Some(value) => Some(db.resolve_account(&book.book_id, value)?),
        None => None,
    };
    let to_account = match opt_str(args, "to_account") {
        Some(value) => Some(db.resolve_account(&book.book_id, value)?),
        None => None,
    };

    if kind == EntryKind::Transfer {
        let (Some(from), Some(to)) = (&account, &to_account) else {
            bail!("a transfer needs both account and to_account");
        };
        if from.account_id == to.account_id {
            bail!("account and to_account must differ");
        }
        if opt_str(args, "category").is_some() {
            bail!("a transfer moves money between accounts and takes no category");
        }
        return Ok((
            Some(from.account_id.clone()),
            Some(to.account_id.clone()),
            None,
        ));
    }

    if to_account.is_some() {
        bail!("to_account only applies to kind=transfer");
    }
    let direction = match kind {
        EntryKind::Income => Direction::Income,
        _ => Direction::Expense,
    };
    let category = match opt_str(args, "category") {
        Some(value) => Some(db.resolve_category(&book.book_id, value, Some(direction))?),
        None => None,
    };
    Ok((
        account.map(|account| account.account_id),
        None,
        category.map(|category| category.category_id),
    ))
}
