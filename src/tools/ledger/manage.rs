//! 账本自身的配置：账本、账户、分类、预算。
//!
//! 低频操作，与高频的记账分成两件工具——`ledger` 单独露脸，这件并进
//! `ledger` 工具组，平时不占模型的注意力。

use super::*;
use crate::ledger::money::{format_amount, parse_amount, validate_currency};

pub(super) async fn run(args: Value, paths: MiyuPaths) -> Result<String> {
    let action = args
        .get("action")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let db = open_db(&paths)?;
    match action {
        "create_book" => create_book(&db, &args),
        "create_account" => create_account(&db, &args),
        "create_category" => create_category(&db, &args),
        "set_budget" => set_budget(&db, &args),
        "delete_budget" => delete_budget(&db, &args),
        // 四个 list_* 收敛成一个 action + what：九个 enum 值缩到六个，
        // 模型要记的形状少一半。
        "list" => match opt_str(&args, "what").unwrap_or("books") {
            "books" => list_books(&db),
            "accounts" => list_accounts(&db, &args),
            "categories" => list_categories(&db, &args),
            "budgets" => list_budgets(&db, &args),
            other => {
                bail!("unknown what: {other}; expected books, accounts, categories or budgets")
            }
        },
        other => bail!(
            "unknown action: {other}; expected create_book, create_account, create_category, \
             set_budget, delete_budget or list"
        ),
    }
}

fn create_book(db: &LedgerDb, args: &Value) -> Result<String> {
    let name = required_str(args, "name")?;
    // 目标币种是这本账所有统计的口径，必须显式给出——猜错会让每一笔
    // 外币账都按错误的方向换算。
    let currency = validate_currency(required_str(args, "currency")?)?;
    let book = db.create_book(name, &currency)?;
    Ok(json!({
        "ok": true,
        "book": { "id": book.book_id, "name": book.name, "currency": book.base_currency },
        "hint": "default categories were created with the book",
    })
    .to_string())
}

fn list_books(db: &LedgerDb) -> Result<String> {
    let books: Vec<Value> = db
        .list_books(false)?
        .into_iter()
        .map(
            |book| json!({ "id": book.book_id, "name": book.name, "currency": book.base_currency }),
        )
        .collect();
    Ok(json!({ "ok": true, "books": books }).to_string())
}

fn create_account(db: &LedgerDb, args: &Value) -> Result<String> {
    let book = resolve_book(db, args)?;
    // 账户类型与初始余额是展示属性，在面板里点两下就有；模型侧少两个
    // 参数，也少两个填错的地方。
    let account = db.create_account(
        &book.book_id,
        required_str(args, "name")?,
        AccountKind::Other,
        opt_str(args, "currency"),
        None,
    )?;
    Ok(json!({
        "ok": true,
        "book": book.name,
        "account": account_json(db, &account)?,
    })
    .to_string())
}

fn list_accounts(db: &LedgerDb, args: &Value) -> Result<String> {
    let book = resolve_book(db, args)?;
    let accounts: Vec<Value> = db
        .list_accounts(&book.book_id, false)?
        .iter()
        .map(|account| account_json(db, account))
        .collect::<Result<_>>()?;
    Ok(json!({ "ok": true, "book": book.name, "accounts": accounts }).to_string())
}

fn create_category(db: &LedgerDb, args: &Value) -> Result<String> {
    let book = resolve_book(db, args)?;
    let direction = Direction::parse(opt_str(args, "direction").unwrap_or("expense"))?;
    // 同理：二级分类与图标在面板里建。
    let category = db.create_category(
        &book.book_id,
        required_str(args, "name")?,
        direction,
        None,
        None,
    )?;
    Ok(json!({
        "ok": true,
        "book": book.name,
        "category": {
            "id": category.category_id,
            "name": db.category_display_name(&category.category_id)?,
            "direction": category.direction,
        },
    })
    .to_string())
}

fn list_categories(db: &LedgerDb, args: &Value) -> Result<String> {
    let book = resolve_book(db, args)?;
    let direction = match opt_str(args, "direction") {
        Some(value) => Some(Direction::parse(value)?),
        None => None,
    };
    let categories: Vec<Value> = db
        .list_categories(&book.book_id, direction, false)?
        .into_iter()
        .map(|category| {
            json!({
                "id": category.category_id,
                "name": category.name,
                "direction": category.direction,
                "parent": category.parent_id,
            })
        })
        .collect();
    Ok(json!({ "ok": true, "book": book.name, "categories": categories }).to_string())
}

fn set_budget(db: &LedgerDb, args: &Value) -> Result<String> {
    let book = resolve_book(db, args)?;
    // 预算额度以账本目标币种计——预算是拿来跟汇总比的，口径必须一致。
    let amount_minor = parse_amount(required_str(args, "amount")?, &book.base_currency)?;
    let category_id = match opt_str(args, "category") {
        Some(value) => Some(
            db.resolve_category(&book.book_id, value, Some(Direction::Expense))?
                .category_id,
        ),
        None => None,
    };
    let budget = db.set_budget(&book.book_id, category_id.as_deref(), amount_minor)?;
    let scope = match &budget.category_id {
        Some(category_id) => format!("category:{}", db.category_display_name(category_id)?),
        None => "total".to_string(),
    };
    Ok(json!({
        "ok": true,
        "book": book.name,
        "budget": {
            "id": budget.budget_id,
            "scope": scope,
            "amount": format_amount(budget.amount_minor, &book.base_currency),
            "currency": book.base_currency,
        },
        "hint": "monthly limit; spending in sub-categories counts towards the parent",
    })
    .to_string())
}

fn list_budgets(db: &LedgerDb, args: &Value) -> Result<String> {
    let book = resolve_book(db, args)?;
    let period = crate::ledger::local_day_now()[..7].to_string();
    let mut items = Vec::new();
    for budget in db.list_budgets(&book.book_id)? {
        if let Some(status) = db.budget_status(&book, budget.category_id.as_deref(), &period)? {
            let mut value = budget_json(&status);
            value
                .as_object_mut()
                .unwrap()
                .insert("id".to_string(), json!(budget.budget_id));
            items.push(value);
        }
    }
    Ok(json!({ "ok": true, "book": book.name, "period": period, "budgets": items }).to_string())
}

fn delete_budget(db: &LedgerDb, args: &Value) -> Result<String> {
    let book = resolve_book(db, args)?;
    let budget_id = required_str(args, "id")?;
    // 只删这本账里的预算：id 是随机串，但跨账本删除仍然该被挡住。
    let owned = db
        .list_budgets(&book.book_id)?
        .into_iter()
        .any(|budget| budget.budget_id == budget_id);
    if !owned {
        bail!("budget {budget_id} does not belong to book {:?}", book.name);
    }
    db.delete_budget(budget_id)?;
    Ok(json!({ "ok": true, "book": book.name, "deleted": budget_id }).to_string())
}
