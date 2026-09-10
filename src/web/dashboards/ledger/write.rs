//! 记账面板的写接口：流水的增改删，以及账本、账户、分类、预算的建立。
//!
//! 与读接口分开只是为了不让一个文件长成上帝文件；共享的请求类型与 DTO
//! 都在父模块。

use super::*;

pub(in crate::web) async fn dash_ledger_create_entry(
    State(state): State<DaemonState>,
    headers: HeaderMap,
    Json(body): Json<EntryBody>,
) -> std::result::Result<Json<Value>, ApiError> {
    require_mutation(&headers, &state)?;
    let identity = require_identity(&headers, &state)?;
    // 锁不能跨 await 持有，配置先取出来。
    let rate_config = {
        let manager = state.manager.lock().unwrap();
        manager.config.plugins.exchange_rate.clone()
    };
    let db = open(&state, &identity)?;
    let book = pick_book(&db, &body.book)?;

    let kind = match opt(&body.kind) {
        Some(value) => EntryKind::parse(value).map_err(bad_request)?,
        None => EntryKind::Expense,
    };
    let currency = match opt(&body.currency) {
        Some(value) => validate_currency(value).map_err(bad_request)?,
        None => book.base_currency.clone(),
    };
    let amount_minor = parse_amount(&body.amount, &currency).map_err(bad_request)?;
    let (occurred_at, occurred_day) = resolve_day(opt(&body.date)).map_err(bad_request)?;

    let account_id = match opt(&body.account) {
        Some(value) => Some(
            db.resolve_account(&book.book_id, value)
                .map_err(bad_request)?
                .account_id,
        ),
        None => None,
    };
    let to_account_id = match opt(&body.to_account) {
        Some(value) => Some(
            db.resolve_account(&book.book_id, value)
                .map_err(bad_request)?
                .account_id,
        ),
        None => None,
    };
    let category_id = match opt(&body.category) {
        Some(value) => {
            let direction = match kind {
                EntryKind::Income => Direction::Income,
                _ => Direction::Expense,
            };
            Some(
                db.resolve_category(&book.book_id, value, Some(direction))
                    .map_err(bad_request)?
                    .category_id,
            )
        }
        None => None,
    };

    // 面板上手点也可能连击两次，同一道闸照走。
    if !body.force {
        let existing = db
            .find_recent_duplicate(
                &book.book_id,
                kind,
                amount_minor,
                &currency,
                category_id.as_deref(),
                body.note.trim(),
                DUPLICATE_WINDOW_SECS,
            )
            .map_err(bad_request)?;
        if let Some(existing) = existing {
            return Ok(Json(json!({
                "ok": false,
                "reason": "possible_duplicate",
                "existing": entry_dto(&db, &existing).map_err(bad_request)?,
            })));
        }
    }

    let conversion = convert_for_book(&db, &book, amount_minor, &currency, &rate_config).await;
    let entry = db
        .add_entry(NewEntry {
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
            note: body.note.trim().to_string(),
            merchant: body.merchant.trim().to_string(),
            source: EntrySource::Webui,
        })
        .map_err(bad_request)?;

    let mut result = json!({ "ok": true, "entry": entry_dto(&db, &entry).map_err(bad_request)? });
    if let Some(status) = db
        .budget_alert_for_entry(&book, &entry)
        .map_err(bad_request)?
    {
        result
            .as_object_mut()
            .unwrap()
            .insert("budget".to_string(), budget_dto(&status, ""));
    }
    Ok(Json(result))
}

pub(in crate::web) async fn dash_ledger_update_entry(
    State(state): State<DaemonState>,
    headers: HeaderMap,
    Path(entry_id): Path<String>,
    Json(body): Json<EntryPatchBody>,
) -> std::result::Result<Json<Value>, ApiError> {
    require_mutation(&headers, &state)?;
    let identity = require_identity(&headers, &state)?;
    let db_path = ledger_db_path(&state, &identity)?;
    let value = tokio::task::spawn_blocking(move || -> anyhow::Result<Value> {
        let db = LedgerDb::open_at(&db_path)?;
        let entry = db
            .get_entry(&entry_id)?
            .ok_or_else(|| anyhow::anyhow!("entry {entry_id} not found"))?;
        let book = db.get_book(&entry.book_id)?;

        let mut patch = EntryPatch::default();
        if let Some(amount) = body.amount.as_deref().and_then(opt) {
            patch.amount_minor = Some(parse_amount(amount, &entry.currency)?);
        }
        if let Some(note) = &body.note {
            patch.note = Some(note.trim().to_string());
        }
        if let Some(merchant) = &body.merchant {
            patch.merchant = Some(merchant.trim().to_string());
        }
        if let Some(category) = &body.category {
            patch.category_id = match opt(category) {
                Some(value) => {
                    let direction = match entry.kind {
                        EntryKind::Income => Direction::Income,
                        _ => Direction::Expense,
                    };
                    Some(Some(
                        db.resolve_category(&book.book_id, value, Some(direction))?
                            .category_id,
                    ))
                }
                // 空字符串是「清空分类」，与不传这个字段不是一回事。
                None => Some(None),
            };
        }
        if let Some(date) = body.date.as_deref().and_then(opt) {
            let (occurred_at, occurred_day) = resolve_day(Some(date))?;
            patch.occurred_at = Some(occurred_at);
            patch.occurred_day = Some(occurred_day);
        }
        let updated = db.update_entry(&entry.entry_id, body.revision, patch)?;
        Ok(json!({ "ok": true, "entry": entry_dto(&db, &updated)? }))
    })
    .await
    .map_err(ApiError::internal)?
    .map_err(bad_request)?;
    Ok(Json(value))
}

pub(in crate::web) async fn dash_ledger_delete_entry(
    State(state): State<DaemonState>,
    headers: HeaderMap,
    Path(entry_id): Path<String>,
) -> std::result::Result<Json<Value>, ApiError> {
    require_mutation(&headers, &state)?;
    let identity = require_identity(&headers, &state)?;
    let db = open(&state, &identity)?;
    db.delete_entry(&entry_id).map_err(bad_request)?;
    Ok(Json(json!({ "ok": true })))
}

pub(in crate::web) async fn dash_ledger_restore_entry(
    State(state): State<DaemonState>,
    headers: HeaderMap,
    Path(entry_id): Path<String>,
) -> std::result::Result<Json<Value>, ApiError> {
    require_mutation(&headers, &state)?;
    let identity = require_identity(&headers, &state)?;
    let db = open(&state, &identity)?;
    db.restore_entry(&entry_id).map_err(bad_request)?;
    Ok(Json(json!({ "ok": true })))
}

pub(in crate::web) async fn dash_ledger_create_book(
    State(state): State<DaemonState>,
    headers: HeaderMap,
    Json(body): Json<BookBody>,
) -> std::result::Result<Json<Value>, ApiError> {
    require_mutation(&headers, &state)?;
    let identity = require_identity(&headers, &state)?;
    let db = open(&state, &identity)?;
    let currency = validate_currency(&body.currency).map_err(bad_request)?;
    let book = db.create_book(&body.name, &currency).map_err(bad_request)?;
    Ok(Json(json!({
        "ok": true,
        "book": { "id": book.book_id, "name": book.name, "currency": book.base_currency },
    })))
}

pub(in crate::web) async fn dash_ledger_create_account(
    State(state): State<DaemonState>,
    headers: HeaderMap,
    Json(body): Json<AccountBody>,
) -> std::result::Result<Json<Value>, ApiError> {
    require_mutation(&headers, &state)?;
    let identity = require_identity(&headers, &state)?;
    let db = open(&state, &identity)?;
    let book = pick_book(&db, &body.book)?;
    let kind = AccountKind::parse(opt(&body.kind).unwrap_or("other")).map_err(bad_request)?;
    let account = db
        .create_account(
            &book.book_id,
            &body.name,
            kind,
            opt(&body.currency),
            opt(&body.opening_balance),
        )
        .map_err(bad_request)?;
    Ok(Json(json!({ "ok": true, "id": account.account_id })))
}

pub(in crate::web) async fn dash_ledger_create_category(
    State(state): State<DaemonState>,
    headers: HeaderMap,
    Json(body): Json<CategoryBody>,
) -> std::result::Result<Json<Value>, ApiError> {
    require_mutation(&headers, &state)?;
    let identity = require_identity(&headers, &state)?;
    let db = open(&state, &identity)?;
    let book = pick_book(&db, &body.book)?;
    let direction =
        Direction::parse(opt(&body.direction).unwrap_or("expense")).map_err(bad_request)?;
    let category = db
        .create_category(
            &book.book_id,
            &body.name,
            direction,
            opt(&body.parent),
            opt(&body.icon),
        )
        .map_err(bad_request)?;
    Ok(Json(json!({ "ok": true, "id": category.category_id })))
}

pub(in crate::web) async fn dash_ledger_set_budget(
    State(state): State<DaemonState>,
    headers: HeaderMap,
    Json(body): Json<BudgetBody>,
) -> std::result::Result<Json<Value>, ApiError> {
    require_mutation(&headers, &state)?;
    let identity = require_identity(&headers, &state)?;
    let db = open(&state, &identity)?;
    let book = pick_book(&db, &body.book)?;
    let amount_minor = parse_amount(&body.amount, &book.base_currency).map_err(bad_request)?;
    let category_id = match opt(&body.category) {
        Some(value) => Some(
            db.resolve_category(&book.book_id, value, Some(Direction::Expense))
                .map_err(bad_request)?
                .category_id,
        ),
        None => None,
    };
    let budget = db
        .set_budget(&book.book_id, category_id.as_deref(), amount_minor)
        .map_err(bad_request)?;
    Ok(Json(json!({ "ok": true, "id": budget.budget_id })))
}

pub(in crate::web) async fn dash_ledger_delete_budget(
    State(state): State<DaemonState>,
    headers: HeaderMap,
    Path(budget_id): Path<String>,
) -> std::result::Result<Json<Value>, ApiError> {
    require_mutation(&headers, &state)?;
    let identity = require_identity(&headers, &state)?;
    let db = open(&state, &identity)?;
    db.delete_budget(&budget_id).map_err(bad_request)?;
    Ok(Json(json!({ "ok": true })))
}

/// 给待换算的账目补上汇率。手动触发，一次最多补 50 笔。
pub(in crate::web) async fn dash_ledger_backfill_rates(
    State(state): State<DaemonState>,
    headers: HeaderMap,
    Query(query): Query<OverviewQuery>,
) -> std::result::Result<Json<Value>, ApiError> {
    require_mutation(&headers, &state)?;
    let identity = require_identity(&headers, &state)?;
    let rate_config = {
        let manager = state.manager.lock().unwrap();
        manager.config.plugins.exchange_rate.clone()
    };
    let db = open(&state, &identity)?;
    let book = pick_book(&db, &query.book)?;
    let pending = db
        .pending_rate_entries(&book.book_id, 50)
        .map_err(bad_request)?;
    let mut filled = 0;
    for entry in &pending {
        if backfill_entry(&db, &book, entry, &rate_config)
            .await
            .unwrap_or(false)
        {
            filled += 1;
        }
    }
    Ok(Json(json!({
        "ok": true,
        "pending": pending.len(),
        "filled": filled,
    })))
}
