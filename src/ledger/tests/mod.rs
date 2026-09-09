//! 账本的单元测试。
//!
//! 每个测试自带一个临时库，互不干扰。断言的是行为（钱数、可见性、报错），
//! 不断言耗时。

mod csv;
mod entries;
mod money;
mod stats;

use super::types::*;
use super::LedgerDb;
use tempfile::TempDir;

/// 建一个空账本库。返回 `TempDir` 是必须的——它一被丢弃目录就没了。
pub(crate) fn temp_db() -> (TempDir, LedgerDb) {
    let dir = TempDir::new().expect("temp dir");
    let db = LedgerDb::open_at(&dir.path().join("ledger.db")).expect("open ledger db");
    (dir, db)
}

#[test]
fn migration_is_idempotent() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("ledger.db");
    let first = LedgerDb::open_at(&path).unwrap();
    let book = first.create_book("生活", "CNY").unwrap();
    drop(first);

    // 再开一次不该重跑建表、更不该丢数据。
    let second = LedgerDb::open_at(&path).unwrap();
    let books = second.list_books(false).unwrap();
    assert_eq!(books.len(), 1);
    assert_eq!(books[0].book_id, book.book_id);
}

#[test]
fn new_book_comes_with_default_categories() {
    let (_dir, db) = temp_db();
    let book = db.create_book("生活", "CNY").unwrap();
    let expense = db
        .list_categories(&book.book_id, Some(Direction::Expense), false)
        .unwrap();
    let income = db
        .list_categories(&book.book_id, Some(Direction::Income), false)
        .unwrap();
    assert!(expense.iter().any(|category| category.name == "餐饮"));
    assert!(income.iter().any(|category| category.name == "工资"));
    // 收支两棵树各自独立,「其他」在两边都有且互不干扰。
    assert_eq!(
        expense.iter().filter(|c| c.name == "其他").count(),
        1,
        "支出树里只该有一个「其他」"
    );
    assert_eq!(income.iter().filter(|c| c.name == "其他").count(), 1);
}

#[test]
fn duplicate_book_name_is_rejected() {
    let (_dir, db) = temp_db();
    db.create_book("生活", "CNY").unwrap();
    let error = db.create_book("生活", "JPY").unwrap_err();
    assert!(error.to_string().contains("already exists"), "{error}");
}

#[test]
fn resolve_book_refuses_to_guess_when_several_exist() {
    let (_dir, db) = temp_db();
    db.create_book("生活", "CNY").unwrap();
    db.create_book("工作", "CNY").unwrap();

    // 没指定时不选默认、不选最近使用——报错让上层去问清楚。
    let error = db.resolve_book(None).unwrap_err();
    assert!(error.to_string().contains("several books"), "{error}");

    // 指定了就要能按名字、按 id 前缀命中。
    let work = db.resolve_book(Some("工作")).unwrap();
    assert_eq!(work.name, "工作");
    let by_prefix = db.resolve_book(Some(&work.book_id[..6])).unwrap();
    assert_eq!(by_prefix.book_id, work.book_id);
}

#[test]
fn single_book_is_the_implicit_default() {
    let (_dir, db) = temp_db();
    let book = db.create_book("生活", "CNY").unwrap();
    let resolved = db.resolve_book(None).unwrap();
    assert_eq!(resolved.book_id, book.book_id);
}

#[test]
fn account_balance_counts_transfers_on_both_sides() {
    let (_dir, db) = temp_db();
    let book = db.create_book("生活", "CNY").unwrap();
    let cash = db
        .create_account(&book.book_id, "现金", AccountKind::Cash, None, Some("100"))
        .unwrap();
    let bank = db
        .create_account(&book.book_id, "银行卡", AccountKind::Bank, None, None)
        .unwrap();

    entries::add_simple(
        &db,
        &book,
        EntryKind::Transfer,
        "30",
        Some(&cash),
        Some(&bank),
    );

    assert_eq!(db.account_balance_minor(&cash).unwrap(), 7000);
    assert_eq!(db.account_balance_minor(&bank).unwrap(), 3000);
}

#[test]
fn archived_book_disappears_from_the_default_listing() {
    let (_dir, db) = temp_db();
    let book = db.create_book("旧账", "CNY").unwrap();
    db.archive_book(&book.book_id, true).unwrap();
    assert!(db.list_books(false).unwrap().is_empty());
    assert_eq!(db.list_books(true).unwrap().len(), 1);
}

#[test]
fn a_stale_book_id_falls_back_instead_of_failing_the_whole_view() {
    let (_dir, db) = temp_db();
    let book = db.create_book("日常", "CNY").unwrap();

    // 面板记住的 id 可能指向已经删掉的账本、或者换了一份数据。展示路径
    // 回退到第一本并把回退这件事说出来;严格路径照旧报错。
    // (09-09 用户实测:面板整页白屏在「no book matches "bk_…"」上。)
    let (picked, missing) = db
        .resolve_book_for_display(Some("bk_deadbeef0000"))
        .unwrap();
    assert_eq!(picked.book_id, book.book_id);
    assert!(missing, "回退了就必须说出来,不能静默换一本账给人看");

    let (picked, missing) = db.resolve_book_for_display(Some("日常")).unwrap();
    assert_eq!(picked.book_id, book.book_id);
    assert!(!missing);

    let (_, missing) = db.resolve_book_for_display(None).unwrap();
    assert!(!missing, "没指定不算「指定的那本不见了」");

    // 精确操作仍然拒绝:猜错账本会把钱记到别处。
    assert!(db.resolve_book(Some("bk_deadbeef0000")).is_err());
}

#[test]
fn display_fallback_creates_a_book_when_there_is_none() {
    let (_dir, db) = temp_db();
    // 一本账都没有时也不该白屏——现建一本默认的。
    let (book, missing) = db
        .resolve_book_for_display(Some("bk_deadbeef0000"))
        .unwrap();
    assert!(!book.book_id.is_empty());
    assert!(missing);
    assert_eq!(db.list_books(false).unwrap().len(), 1);
}
