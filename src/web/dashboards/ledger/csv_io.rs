//! 记账面板的 CSV 进出。
//!
//! 导出的是原始金额与币种，换算结果与汇率快照跟在后面——一份导出既能给
//! 人看，也能原样导回来。

use super::*;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::web) struct ImportBody {
    #[serde(default)]
    book: String,
    /// CSV 正文。表头至少要有 date、kind、amount 三列。
    csv: String,
    /// 遇到账本里没有的分类或账户时自动建出来，而不是整批拒绝。
    #[serde(default)]
    create_missing: bool,
}

/// 导出一个月的流水为 CSV。
///
/// 导出的是原始金额与币种，换算结果与汇率快照跟在后面——这样一份文件
/// 既能给人看，也能原样导回来。

pub(in crate::web) async fn dash_ledger_export(
    State(state): State<DaemonState>,
    headers: HeaderMap,
    Query(query): Query<OverviewQuery>,
) -> std::result::Result<Response, ApiError> {
    require_auth(&headers, &state)?;
    let paths = state.paths.clone();
    let (name, body) = tokio::task::spawn_blocking(move || -> anyhow::Result<(String, String)> {
        let db = LedgerDb::open(&paths)?;
        let book = match opt(&query.book) {
            Some(value) => db.resolve_book(Some(value))?,
            None => db.ensure_default_book()?,
        };
        let period = period_or_now(&query.period);
        let csv = db.export_csv(&book, &format!("{period}-01"), &format!("{period}-31"))?;
        Ok((format!("ledger-{}-{period}.csv", book.name), csv))
    })
    .await
    .map_err(ApiError::internal)?
    .map_err(bad_request)?;

    // 文件名里可能有中文与空格，走 RFC 5987 的 filename* 而不是裸 filename。
    let disposition = format!(
        "attachment; filename=\"ledger.csv\"; filename*=UTF-8''{}",
        urlencoding_lite(&name)
    );
    let mut response = Response::new(body.into());
    response.headers_mut().insert(
        CONTENT_TYPE,
        HeaderValue::from_static("text/csv; charset=utf-8"),
    );
    if let Ok(value) = HeaderValue::from_str(&disposition) {
        response.headers_mut().insert(CONTENT_DISPOSITION, value);
    }
    Ok(response)
}

/// 只对 RFC 5987 需要转义的字符做百分号编码。
fn urlencoding_lite(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for byte in raw.as_bytes() {
        let c = *byte as char;
        if c.is_ascii_alphanumeric() || matches!(c, '-' | '.' | '_' | '~') {
            out.push(c);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// 从 CSV 导入流水。
///
/// 去重按内容而不是时间窗口：重复导入同一个文件应当是幂等的。记账时那道
/// 五分钟重复闸在这里会误伤（同一批里两笔一样的地铁票是合法的）。
pub(in crate::web) async fn dash_ledger_import(
    State(state): State<DaemonState>,
    headers: HeaderMap,
    Json(body): Json<ImportBody>,
) -> std::result::Result<Json<Value>, ApiError> {
    require_mutation(&headers, &state)?;
    let rate_config = {
        let manager = state.manager.lock().unwrap();
        manager.config.plugins.exchange_rate.clone()
    };
    let db = open(&state)?;
    let book = pick_book(&db, &body.book)?;
    let rows = crate::ledger::csv::parse_csv(&body.csv).map_err(bad_request)?;
    if rows.len() > 2000 {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "at most 2000 rows per import",
        ));
    }

    let mut imported = 0usize;
    let mut skipped = 0usize;
    let mut failures: Vec<Value> = Vec::new();
    // 每个内容键第一次出现时查一次库，之后在内存里递减。这样「文件里两趟
    // 一样的车费」第一次导入会进两笔，而整份文件再导一次一笔都不进。
    let mut quota: HashMap<String, i64> = HashMap::new();

    for (index, row) in rows.iter().enumerate() {
        match import_one(
            &db,
            &book,
            row,
            body.create_missing,
            &rate_config,
            &mut quota,
        )
        .await
        {
            Ok(true) => imported += 1,
            Ok(false) => skipped += 1,
            Err(error) => {
                // 只回前 20 条明细：一个格式写错的文件会把每一行都点亮，
                // 全量回灌既没人看也把响应撑爆。
                if failures.len() < 20 {
                    failures.push(json!({
                        "line": index + 2,
                        "message": safe_error_message(&error),
                    }));
                }
            }
        }
    }

    Ok(Json(json!({
        "ok": true,
        "imported": imported,
        "skipped": skipped,
        "failed": rows.len() - imported - skipped,
        "failures": failures,
    })))
}

/// 导入一行。返回 `Ok(false)` 表示这行已经导入过、跳过。
async fn import_one(
    db: &LedgerDb,
    book: &BookRecord,
    row: &crate::ledger::csv::ImportRow,
    create_missing: bool,
    rate_config: &crate::config::ExchangeRatePluginConfig,
    quota: &mut HashMap<String, i64>,
) -> anyhow::Result<bool> {
    let kind = EntryKind::parse(if row.kind.is_empty() {
        "expense"
    } else {
        &row.kind
    })?;
    let currency = match row.currency.trim() {
        "" => book.base_currency.clone(),
        value => validate_currency(value)?,
    };
    let amount_minor = parse_amount(&row.amount, &currency)?;
    let (occurred_at, occurred_day) = resolve_day(Some(row.date.trim()))?;

    // 去重按配额而不是「存在与否」：库里已有几条同样内容的导入记录，就
    // 抵扣掉文件里的前几行，多出来的才真正插入。
    let note = row.note.trim();
    let key = format!(
        "{occurred_day}|{}|{amount_minor}|{currency}|{note}",
        kind.as_str()
    );
    let remaining = match quota.entry(key) {
        std::collections::hash_map::Entry::Occupied(slot) => slot.into_mut(),
        std::collections::hash_map::Entry::Vacant(slot) => slot.insert(db.count_import_rows(
            &book.book_id,
            &occurred_day,
            kind,
            amount_minor,
            &currency,
            note,
        )?),
    };
    if *remaining > 0 {
        *remaining -= 1;
        return Ok(false);
    }

    let direction = match kind {
        EntryKind::Income => Direction::Income,
        _ => Direction::Expense,
    };
    let category_id = match row.category.trim() {
        "" => None,
        name => match db.resolve_category(&book.book_id, name, Some(direction)) {
            Ok(category) => Some(category.category_id),
            Err(error) if create_missing => {
                // 「餐饮/外卖」这种带斜杠的名字按父子两级建出来。
                let (parent, leaf) = match name.split_once('/') {
                    Some((parent, leaf)) => (Some(parent.trim()), leaf.trim()),
                    None => (None, name),
                };
                if let Some(parent) = parent {
                    if db
                        .resolve_category(&book.book_id, parent, Some(direction))
                        .is_err()
                    {
                        db.create_category(&book.book_id, parent, direction, None, None)?;
                    }
                }
                let _ = error;
                Some(
                    db.create_category(&book.book_id, leaf, direction, parent, None)?
                        .category_id,
                )
            }
            Err(error) => return Err(error),
        },
    };
    let account_id = match row.account.trim() {
        "" => None,
        name => match db.resolve_account(&book.book_id, name) {
            Ok(account) => Some(account.account_id),
            Err(_) if create_missing => Some(
                db.create_account(&book.book_id, name, AccountKind::Other, None, None)?
                    .account_id,
            ),
            Err(error) => return Err(error),
        },
    };
    // 转账要两个账户，CSV 里表达不了，导入一律按收支处理。
    if kind == EntryKind::Transfer {
        anyhow::bail!("transfers cannot be imported from CSV; record them in the dashboard");
    }

    let conversion = convert_for_book(db, book, amount_minor, &currency, rate_config).await;
    db.add_entry(NewEntry {
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
        to_account_id: None,
        category_id,
        occurred_at,
        occurred_day,
        note: note.to_string(),
        merchant: row.merchant.trim().to_string(),
        source: EntrySource::Import,
    })?;
    Ok(true)
}
