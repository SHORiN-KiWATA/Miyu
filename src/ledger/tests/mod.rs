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

/// v1 时代建的账本一个账户都没有，打开时要补上默认那套——但只补这一次。
#[test]
fn opening_a_v1_database_backfills_default_accounts() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("ledger.db");
    let db = LedgerDb::open_at(&path).unwrap();
    let book = db.create_book("生活", "CNY").unwrap();

    // 退回 v1 的样子：那时候建出来的账本是光的。
    db.with_conn(|conn| {
        conn.execute("DELETE FROM ledger_accounts", [])?;
        conn.execute_batch("PRAGMA user_version = 1;")?;
        Ok(())
    })
    .unwrap();
    drop(db);

    let reopened = LedgerDb::open_at(&path).unwrap();
    let names: Vec<String> = reopened
        .list_accounts(&book.book_id, false)
        .unwrap()
        .into_iter()
        .map(|account| account.name)
        .collect();
    assert_eq!(names.len(), 5, "{names:?}");
    assert!(names.iter().any(|name| name == "交通卡"), "{names:?}");

    // 归档掉一个再开，不该复活：迁移跑过就是跑过了，不会每次启动都来一遍。
    let victim = reopened.resolve_account(&book.book_id, "信用卡").unwrap();
    reopened.archive_account(&victim.account_id, true).unwrap();
    drop(reopened);

    let again = LedgerDb::open_at(&path).unwrap();
    let live = again.list_accounts(&book.book_id, false).unwrap();
    assert_eq!(live.len(), 4, "归档过的默认账户不该被补回来");
}

/// 已经自己配了账户的老账本，一个默认账户都不该被塞进去。
///
/// 那套账户是照着自己的钱包配的，可能还是外币的（日元的现金、日元的信用
/// 卡）。在旁边堆三个用不上的人民币空账户不是帮忙。
#[test]
fn backfill_skips_a_book_that_already_has_accounts() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("ledger.db");
    let db = LedgerDb::open_at(&path).unwrap();
    let book = db.create_book("日常", "CNY").unwrap();
    db.with_conn(|conn| {
        conn.execute("DELETE FROM ledger_accounts", [])?;
        conn.execute_batch("PRAGMA user_version = 1;")?;
        Ok(())
    })
    .unwrap();
    // 自己建的两个账户，都是日元的。
    db.create_account(
        &book.book_id,
        "Suica",
        AccountKind::Other,
        Some("JPY"),
        None,
    )
    .unwrap();
    db.create_account(&book.book_id, "现金", AccountKind::Cash, Some("JPY"), None)
        .unwrap();
    drop(db);

    let reopened = LedgerDb::open_at(&path).unwrap();
    let accounts = reopened.list_accounts(&book.book_id, true).unwrap();
    let names: Vec<&str> = accounts.iter().map(|a| a.name.as_str()).collect();
    assert_eq!(accounts.len(), 2, "{names:?}");
    // 尤其不该出现一个人民币的「现金」跟日元的那个并排站着。
    let cash = accounts.iter().find(|a| a.name == "现金").unwrap();
    assert_eq!(cash.currency, "JPY");
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
    // 名字避开默认账户：建账本时已经铺过一套「现金/银行卡/……」。
    let cash = db
        .create_account(&book.book_id, "钱包", AccountKind::Cash, None, Some("100"))
        .unwrap();
    let bank = db
        .create_account(&book.book_id, "储蓄卡", AccountKind::Bank, None, None)
        .unwrap();

    entries::add_simple(
        &db,
        &book,
        EntryKind::Transfer,
        "30",
        Some(&cash),
        Some(&bank),
    );

    assert_eq!(db.account_balance(&cash).unwrap().minor, 7000);
    assert_eq!(db.account_balance(&bank).unwrap().minor, 3000);
}

#[test]
fn a_new_book_comes_with_default_accounts() {
    let (_dir, db) = temp_db();
    let book = db.create_book("生活", "CNY").unwrap();
    let names: Vec<String> = db
        .list_accounts(&book.book_id, false)
        .unwrap()
        .into_iter()
        .map(|account| account.name)
        .collect();
    for expected in ["现金", "电子支付", "交通卡", "信用卡", "银行卡"] {
        assert!(names.iter().any(|name| name == expected), "缺了 {expected}");
    }
    // 都跟着账本的目标币种，这样余额直接吃账目行上冻结的换算结果。
    for account in db.list_accounts(&book.book_id, false).unwrap() {
        assert_eq!(account.currency, "CNY");
    }
    // 幂等：再补一次一个也不该多出来。
    assert_eq!(db.ensure_default_accounts(&book.book_id).unwrap(), 0);
}

/// 账户币种与账目币种不同时，余额按记账当时的汇率折算——不是整笔跳过。
#[test]
fn foreign_entries_convert_into_the_account_currency() {
    let (_dir, db) = temp_db();
    let book = db.create_book("日常", "CNY").unwrap();
    let wallet = db
        .create_account(&book.book_id, "钱包", AccountKind::Cash, None, None)
        .unwrap();

    // 1500 日元、当天汇率 0.043694 → 65.54 元，落库时就冻结在账目行上。
    entries::add_converted(&db, &book, "1500", "JPY", 6554, Some(&wallet));

    let balance = db.account_balance(&wallet).unwrap();
    assert_eq!(balance.minor, -6554, "日元支出要折成人民币扣，不是跳过");
    assert_eq!(balance.unconverted_count, 0);
}

/// 汇率还没取到的账目进不了余额，但要被数出来——余额少一截却没人说一声，
/// 是这套账本最不该出现的错。
#[test]
fn entries_awaiting_a_rate_are_counted_not_swallowed() {
    let (_dir, db) = temp_db();
    let book = db.create_book("日常", "CNY").unwrap();
    let wallet = db
        .create_account(&book.book_id, "钱包", AccountKind::Cash, None, None)
        .unwrap();

    entries::add_pending(&db, &book, "1500", "JPY", Some(&wallet));

    let balance = db.account_balance(&wallet).unwrap();
    assert_eq!(balance.minor, 0);
    assert_eq!(balance.unconverted_count, 1);
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
