//! 汇总统计与预算告警。

use super::temp_db;
use crate::ledger::entries::NewEntry;
use crate::ledger::types::*;
use crate::ledger::{now_rfc3339, LedgerDb};

const PERIOD: &str = "2026-09";

/// 记一笔落在指定日子的账，统计测试要能控制日期。
fn add_on(
    db: &LedgerDb,
    book: &BookRecord,
    kind: EntryKind,
    amount_minor: i64,
    category: Option<&str>,
    day: &str,
) -> EntryRecord {
    db.add_entry(NewEntry {
        book_id: book.book_id.clone(),
        kind,
        amount_minor,
        currency: book.base_currency.clone(),
        base_amount_minor: Some(amount_minor),
        base_currency: book.base_currency.clone(),
        rate: None,
        rate_source: None,
        rate_at: None,
        rate_status: RateStatus::Same,
        account_id: None,
        to_account_id: None,
        category_id: category.map(str::to_string),
        occurred_at: now_rfc3339(),
        occurred_day: day.to_string(),
        note: String::new(),
        merchant: String::new(),
        source: EntrySource::Chat,
    })
    .expect("add entry")
}

/// 记一笔取不到汇率的外币账。
fn add_pending(db: &LedgerDb, book: &BookRecord, amount_minor: i64, day: &str) -> EntryRecord {
    db.add_entry(NewEntry {
        book_id: book.book_id.clone(),
        kind: EntryKind::Expense,
        amount_minor,
        currency: "JPY".to_string(),
        base_amount_minor: None,
        base_currency: book.base_currency.clone(),
        rate: None,
        rate_source: None,
        rate_at: None,
        rate_status: RateStatus::Pending,
        account_id: None,
        to_account_id: None,
        category_id: None,
        occurred_at: now_rfc3339(),
        occurred_day: day.to_string(),
        note: String::new(),
        merchant: String::new(),
        source: EntrySource::Chat,
    })
    .expect("add pending entry")
}

fn setup() -> (tempfile::TempDir, LedgerDb, BookRecord) {
    let (dir, db) = temp_db();
    let book = db.create_book("生活", "CNY").unwrap();
    (dir, db, book)
}

#[test]
fn summary_nets_income_against_expense() {
    let (_dir, db, book) = setup();
    add_on(&db, &book, EntryKind::Expense, 3550, None, "2026-09-08");
    add_on(&db, &book, EntryKind::Expense, 1200, None, "2026-09-09");
    add_on(&db, &book, EntryKind::Income, 500_000, None, "2026-09-01");

    let summary = db.period_summary(&book, PERIOD).unwrap();
    assert_eq!(summary.expense_minor, 4750);
    assert_eq!(summary.income_minor, 500_000);
    assert_eq!(summary.net_minor, 495_250);
    assert_eq!(summary.entry_count, 3);
    assert_eq!(summary.currency, "CNY");
}

#[test]
fn transfers_move_money_without_touching_income_or_expense() {
    let (_dir, db, book) = setup();
    let cash = db
        .create_account(&book.book_id, "现金", AccountKind::Cash, None, None)
        .unwrap();
    let bank = db
        .create_account(&book.book_id, "银行卡", AccountKind::Bank, None, None)
        .unwrap();
    db.add_entry(NewEntry {
        book_id: book.book_id.clone(),
        kind: EntryKind::Transfer,
        amount_minor: 10_000,
        currency: "CNY".to_string(),
        base_amount_minor: Some(10_000),
        base_currency: "CNY".to_string(),
        rate: None,
        rate_source: None,
        rate_at: None,
        rate_status: RateStatus::Same,
        account_id: Some(cash.account_id.clone()),
        to_account_id: Some(bank.account_id.clone()),
        category_id: None,
        occurred_at: now_rfc3339(),
        occurred_day: "2026-09-08".to_string(),
        note: String::new(),
        merchant: String::new(),
        source: EntrySource::Chat,
    })
    .unwrap();

    let summary = db.period_summary(&book, PERIOD).unwrap();
    assert_eq!(summary.expense_minor, 0, "转账不是支出");
    assert_eq!(summary.income_minor, 0, "转账不是收入");
    assert_eq!(summary.entry_count, 1, "但它确实是一笔账");
}

#[test]
fn pending_entries_are_counted_but_never_summed() {
    let (_dir, db, book) = setup();
    add_on(&db, &book, EntryKind::Expense, 3550, None, "2026-09-08");
    add_pending(&db, &book, 3000, "2026-09-08");

    let summary = db.period_summary(&book, PERIOD).unwrap();
    // 拿一个猜的汇率把数字凑齐,比明说「还有一笔没算」要糟得多。
    assert_eq!(summary.expense_minor, 3550, "待换算的钱不该混进合计");
    assert_eq!(summary.pending_count, 1, "但必须让人知道它存在");
}

#[test]
fn sub_category_spending_rolls_up_to_its_parent() {
    let (_dir, db, book) = setup();
    let meals = db
        .resolve_category(&book.book_id, "餐饮", Some(Direction::Expense))
        .unwrap();
    let takeout = db
        .create_category(
            &book.book_id,
            "外卖",
            Direction::Expense,
            Some("餐饮"),
            None,
        )
        .unwrap();

    add_on(
        &db,
        &book,
        EntryKind::Expense,
        2000,
        Some(&meals.category_id),
        "2026-09-08",
    );
    add_on(
        &db,
        &book,
        EntryKind::Expense,
        3000,
        Some(&takeout.category_id),
        "2026-09-08",
    );

    let totals = db
        .category_totals(&book.book_id, PERIOD, EntryKind::Expense)
        .unwrap();
    assert_eq!(totals.len(), 1, "子分类该并进父分类,不单独成行");
    assert_eq!(totals[0].name, "餐饮");
    assert_eq!(totals[0].amount_minor, 5000);
    assert_eq!(totals[0].entry_count, 2);
}

#[test]
fn uncategorised_spending_still_shows_up() {
    let (_dir, db, book) = setup();
    add_on(&db, &book, EntryKind::Expense, 2000, None, "2026-09-08");
    let totals = db
        .category_totals(&book.book_id, PERIOD, EntryKind::Expense)
        .unwrap();
    assert_eq!(totals.len(), 1);
    assert_eq!(totals[0].name, "未分类");
}

#[test]
fn budget_state_has_three_bands() {
    let (_dir, db, book) = setup();
    db.set_budget(&book.book_id, None, 100_000).unwrap();

    // 50%:平安无事。
    add_on(&db, &book, EntryKind::Expense, 50_000, None, "2026-09-01");
    let status = db.budget_status(&book, None, PERIOD).unwrap().unwrap();
    assert_eq!(status.state, BudgetState::Ok);

    // 85%:接近了。
    add_on(&db, &book, EntryKind::Expense, 35_000, None, "2026-09-02");
    let status = db.budget_status(&book, None, PERIOD).unwrap().unwrap();
    assert_eq!(status.state, BudgetState::Near);
    assert_eq!(status.used_minor, 85_000);

    // 超了。
    add_on(&db, &book, EntryKind::Expense, 20_000, None, "2026-09-03");
    let status = db.budget_status(&book, None, PERIOD).unwrap().unwrap();
    assert_eq!(status.state, BudgetState::Exceeded);
    assert_eq!(status.used_minor, 105_000);
    assert_eq!(status.limit_minor, 100_000);
    assert_eq!(status.scope, "total");
}

#[test]
fn category_budget_counts_sub_category_spending() {
    let (_dir, db, book) = setup();
    let meals = db
        .resolve_category(&book.book_id, "餐饮", Some(Direction::Expense))
        .unwrap();
    let takeout = db
        .create_category(
            &book.book_id,
            "外卖",
            Direction::Expense,
            Some("餐饮"),
            None,
        )
        .unwrap();
    db.set_budget(&book.book_id, Some(&meals.category_id), 10_000)
        .unwrap();

    // 给「餐饮」设的额度,记在「餐饮/外卖」上的钱当然要占额度。
    add_on(
        &db,
        &book,
        EntryKind::Expense,
        12_000,
        Some(&takeout.category_id),
        "2026-09-08",
    );
    let status = db
        .budget_status(&book, Some(&meals.category_id), PERIOD)
        .unwrap()
        .unwrap();
    assert_eq!(status.used_minor, 12_000);
    assert_eq!(status.state, BudgetState::Exceeded);
    assert_eq!(status.scope, "category:餐饮");
}

#[test]
fn alert_prefers_the_category_budget_over_the_total() {
    let (_dir, db, book) = setup();
    let meals = db
        .resolve_category(&book.book_id, "餐饮", Some(Direction::Expense))
        .unwrap();
    db.set_budget(&book.book_id, None, 1_000_000).unwrap();
    db.set_budget(&book.book_id, Some(&meals.category_id), 10_000)
        .unwrap();

    let entry = add_on(
        &db,
        &book,
        EntryKind::Expense,
        12_000,
        Some(&meals.category_id),
        "2026-09-08",
    );
    let alert = db.budget_alert_for_entry(&book, &entry).unwrap().unwrap();
    assert_eq!(alert.scope, "category:餐饮", "先报最贴近这笔账的那个额度");
    assert_eq!(alert.state, BudgetState::Exceeded);
}

#[test]
fn no_alert_when_everything_is_within_budget() {
    let (_dir, db, book) = setup();
    db.set_budget(&book.book_id, None, 1_000_000).unwrap();
    let entry = add_on(&db, &book, EntryKind::Expense, 1000, None, "2026-09-08");
    // 没话说的时候就该什么都不返回,免得白占 token。
    assert!(db.budget_alert_for_entry(&book, &entry).unwrap().is_none());
}

#[test]
fn income_never_triggers_a_budget_alert() {
    let (_dir, db, book) = setup();
    db.set_budget(&book.book_id, None, 1000).unwrap();
    let entry = add_on(&db, &book, EntryKind::Income, 999_999, None, "2026-09-08");
    assert!(db.budget_alert_for_entry(&book, &entry).unwrap().is_none());
}

#[test]
fn setting_a_budget_twice_updates_it_instead_of_duplicating() {
    let (_dir, db, book) = setup();
    db.set_budget(&book.book_id, None, 100_000).unwrap();
    let second = db.set_budget(&book.book_id, None, 200_000).unwrap();
    assert_eq!(second.amount_minor, 200_000);
    assert_eq!(db.list_budgets(&book.book_id).unwrap().len(), 1);
}

#[test]
fn daily_totals_only_return_days_that_have_entries() {
    let (_dir, db, book) = setup();
    add_on(&db, &book, EntryKind::Expense, 1000, None, "2026-09-01");
    add_on(&db, &book, EntryKind::Expense, 2000, None, "2026-09-01");
    add_on(&db, &book, EntryKind::Income, 5000, None, "2026-09-05");

    let series = db
        .daily_totals(&book.book_id, "2026-09-01", "2026-09-30")
        .unwrap();
    assert_eq!(series.len(), 2, "空白日子交给前端补零");
    assert_eq!(series[0].day, "2026-09-01");
    assert_eq!(series[0].expense_minor, 3000);
    assert_eq!(series[1].income_minor, 5000);
}

#[test]
fn deleted_entries_leave_the_statistics() {
    let (_dir, db, book) = setup();
    let entry = add_on(&db, &book, EntryKind::Expense, 3000, None, "2026-09-08");
    db.delete_entry(&entry.entry_id).unwrap();
    let summary = db.period_summary(&book, PERIOD).unwrap();
    assert_eq!(summary.expense_minor, 0);
    assert_eq!(summary.entry_count, 0);
}

#[test]
fn malformed_period_is_rejected() {
    let (_dir, db, book) = setup();
    for bad in ["2026", "2026-9", "26-09", "abcdefg", ""] {
        assert!(db.period_summary(&book, bad).is_err(), "{bad:?} 该被拒");
    }
}

#[test]
fn only_months_that_actually_have_entries_are_listed() {
    let (_dir, db, book) = setup();
    add_on(&db, &book, EntryKind::Expense, 1000, None, "2026-09-08");
    add_on(&db, &book, EntryKind::Expense, 2000, None, "2026-09-20");
    add_on(&db, &book, EntryKind::Income, 5000, None, "2026-06-01");

    // 列一整年的空月份,会让人以为那些月份有账、点开却是空的
    // (09-09 用户走查意见)。当月由调用方补,这里只报真的有账的。
    let periods = db.periods_with_entries(&book.book_id).unwrap();
    assert_eq!(periods, vec!["2026-09", "2026-06"], "新的在前,同月只算一次");
}

#[test]
fn a_deleted_entry_takes_its_month_off_the_list() {
    let (_dir, db, book) = setup();
    let entry = add_on(&db, &book, EntryKind::Expense, 1000, None, "2026-06-01");
    add_on(&db, &book, EntryKind::Expense, 1000, None, "2026-09-01");
    db.delete_entry(&entry.entry_id).unwrap();
    assert_eq!(
        db.periods_with_entries(&book.book_id).unwrap(),
        vec!["2026-09"]
    );
}
