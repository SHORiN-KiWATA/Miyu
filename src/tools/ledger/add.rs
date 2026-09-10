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
    let db = open_db(&config, &paths)?;
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

    let Targets {
        account_id,
        to_account_id,
        mut category_id,
        new_category,
    } = resolve_targets(&db, &book, &args, kind)?;

    // 重复闸。端点抖动时模型会把「记一笔」重演成同义变体，而账本里多出
    // 一笔比少一笔更难发现——挡下来交给调用方确认，比事后对账便宜。
    //
    // 待建的分类此时还没落库，查重就少了分类这一维。这是有意的：既然这个
    // 分类原本不存在，几分钟前那笔同额同备注的账也不可能挂在它上面。
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

    // 分类拖到这里才真建：建在查重前面，被重复闸挡下的那一笔会在账本里
    // 留下一个没人用的分类。
    let created_category = match new_category {
        Some((name, direction)) => {
            let category = db.create_category(&book.book_id, &name, direction, None, None)?;
            category_id = Some(category.category_id);
            Some(category.name)
        }
        None => None,
    };

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
    // 账本里凭空多出一个分类，是模型该说出口的事——不然用户下次翻分类表
    // 会莫名其妙地多几个词。
    if let Some(name) = created_category {
        result.as_object_mut().unwrap().insert(
            "created_category".to_string(),
            json!(format!("{name} (new)")),
        );
    }
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

/// 顺手建分类之前的最后一道闸：这个名字是不是长在另一棵树上。
///
/// 分类按收支方向分成两棵，「工资」只该出现在收入里。记一笔支出却说
/// `category=工资`，多半是 `kind` 填错了——这时候在支出树下建一个同名的
/// 镜像分类，等于把一个可以当场说清的错误变成了分类表里两个「工资」。
/// 自动创建只对**两棵树都没有**的新名字生效。
pub(super) fn ensure_direction_is_free(
    db: &LedgerDb,
    book_id: &str,
    name: &str,
    direction: Direction,
) -> Result<()> {
    let opposite = match direction {
        Direction::Expense => Direction::Income,
        Direction::Income => Direction::Expense,
    };
    // 另一棵树上「像好几个」不算撞名，照常新建：那是含混，不是方向错。
    if let Ok(Some(found)) = db.resolve_category_opt(book_id, name, Some(opposite)) {
        bail!(
            "{:?} is an {} category, but this entry is an {}; fix kind, or name a different category",
            found.name,
            opposite.as_str(),
            direction.as_str()
        );
    }
    Ok(())
}

/// [`resolve_targets`] 的产物。
pub(super) struct Targets {
    account_id: Option<String>,
    to_account_id: Option<String>,
    category_id: Option<String>,
    /// 说了一个这本账里没有的分类：名字与方向留在这里，等查重放行后再建。
    new_category: Option<(String, Direction)>,
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
) -> Result<Targets> {
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
        return Ok(Targets {
            account_id: Some(from.account_id.clone()),
            to_account_id: Some(to.account_id.clone()),
            category_id: None,
            new_category: None,
        });
    }

    if to_account.is_some() {
        bail!("to_account only applies to kind=transfer");
    }
    let direction = match kind {
        EntryKind::Income => Direction::Income,
        _ => Direction::Expense,
    };
    // 分类不在表里就现建一个，不把「去 manage_ledger 建一个再回来」这一整轮
    // 甩给模型。名字含混（同时像好几个现有分类）仍然报错——那种时候新建
    // 只会让分类表更含混。
    let mut category_id = None;
    let mut new_category = None;
    if let Some(value) = opt_str(args, "category") {
        match db.resolve_category_opt(&book.book_id, value, Some(direction))? {
            Some(found) => category_id = Some(found.category_id),
            None => {
                ensure_direction_is_free(db, &book.book_id, value, direction)?;
                new_category = Some((value.to_string(), direction));
            }
        }
    }
    Ok(Targets {
        account_id: account.map(|account| account.account_id),
        to_account_id: None,
        category_id,
        new_category,
    })
}
