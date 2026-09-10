//! 流水的增删改查、重复闸、软删除、乐观并发。

use super::temp_db;
use crate::ledger::entries::{EntryFilter, EntryPatch, NewEntry, DUPLICATE_WINDOW_SECS};
use crate::ledger::money::parse_amount;
use crate::ledger::types::*;
use crate::ledger::{local_day_now, now_rfc3339, LedgerDb};

/// 记一笔最普通的账，测试里到处要用。
pub(crate) fn add_simple(
    db: &LedgerDb,
    book: &BookRecord,
    kind: EntryKind,
    amount: &str,
    from: Option<&AccountRecord>,
    to: Option<&AccountRecord>,
) -> EntryRecord {
    let amount_minor = parse_amount(amount, &book.base_currency).unwrap();
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
        account_id: from.map(|account| account.account_id.clone()),
        to_account_id: to.map(|account| account.account_id.clone()),
        category_id: None,
        occurred_at: now_rfc3339(),
        occurred_day: local_day_now(),
        note: String::new(),
        merchant: String::new(),
        source: EntrySource::Chat,
    })
    .expect("add entry")
}

/// 记一笔外币支出，换算结果已经冻结在账目行上（`rate_status = ok`）。
pub(crate) fn add_converted(
    db: &LedgerDb,
    book: &BookRecord,
    amount: &str,
    currency: &str,
    base_amount_minor: i64,
    from: Option<&AccountRecord>,
) -> EntryRecord {
    let amount_minor = parse_amount(amount, currency).unwrap();
    db.add_entry(NewEntry {
        book_id: book.book_id.clone(),
        kind: EntryKind::Expense,
        amount_minor,
        currency: currency.to_string(),
        base_amount_minor: Some(base_amount_minor),
        base_currency: book.base_currency.clone(),
        rate: Some("0.043694".to_string()),
        rate_source: Some("test".to_string()),
        rate_at: Some(now_rfc3339()),
        rate_status: RateStatus::Ok,
        account_id: from.map(|account| account.account_id.clone()),
        to_account_id: None,
        category_id: None,
        occurred_at: now_rfc3339(),
        occurred_day: local_day_now(),
        note: String::new(),
        merchant: String::new(),
        source: EntrySource::Chat,
    })
    .expect("add entry")
}

/// 记一笔还没取到汇率的外币支出。
pub(crate) fn add_pending(
    db: &LedgerDb,
    book: &BookRecord,
    amount: &str,
    currency: &str,
    from: Option<&AccountRecord>,
) -> EntryRecord {
    let amount_minor = parse_amount(amount, currency).unwrap();
    db.add_entry(NewEntry {
        book_id: book.book_id.clone(),
        kind: EntryKind::Expense,
        amount_minor,
        currency: currency.to_string(),
        base_amount_minor: None,
        base_currency: book.base_currency.clone(),
        rate: None,
        rate_source: None,
        rate_at: None,
        rate_status: RateStatus::Pending,
        account_id: from.map(|account| account.account_id.clone()),
        to_account_id: None,
        category_id: None,
        occurred_at: now_rfc3339(),
        occurred_day: local_day_now(),
        note: String::new(),
        merchant: String::new(),
        source: EntrySource::Chat,
    })
    .expect("add entry")
}

fn book_with_category(db: &LedgerDb) -> (BookRecord, CategoryRecord) {
    let book = db.create_book("生活", "CNY").unwrap();
    let category = db
        .resolve_category(&book.book_id, "餐饮", Some(Direction::Expense))
        .unwrap();
    (book, category)
}

fn add_meal(
    db: &LedgerDb,
    book: &BookRecord,
    category: &CategoryRecord,
    amount: &str,
    note: &str,
) -> EntryRecord {
    let amount_minor = parse_amount(amount, &book.base_currency).unwrap();
    db.add_entry(NewEntry {
        book_id: book.book_id.clone(),
        kind: EntryKind::Expense,
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
        category_id: Some(category.category_id.clone()),
        occurred_at: now_rfc3339(),
        occurred_day: local_day_now(),
        note: note.to_string(),
        merchant: String::new(),
        source: EntrySource::Chat,
    })
    .expect("add entry")
}

#[test]
fn records_and_lists_an_entry() {
    let (_dir, db) = temp_db();
    let (book, category) = book_with_category(&db);
    let entry = add_meal(&db, &book, &category, "35.50", "麦当劳");

    assert_eq!(entry.amount_minor, 3550);
    assert_eq!(entry.revision, 1);
    assert!(entry.deleted_at.is_none());

    let (items, total) = db
        .list_entries(&EntryFilter {
            book_id: book.book_id.clone(),
            limit: 50,
            ..Default::default()
        })
        .unwrap();
    assert_eq!(total, 1);
    assert_eq!(items[0].entry_id, entry.entry_id);
}

#[test]
fn duplicate_gate_catches_a_replayed_entry() {
    let (_dir, db) = temp_db();
    let (book, category) = book_with_category(&db);
    let first = add_meal(&db, &book, &category, "35.50", "麦当劳");

    // 端点抖动时模型会把同一笔重演一遍:新 id、新时间戳,内容一模一样。
    let hit = db
        .find_recent_duplicate(
            &book.book_id,
            EntryKind::Expense,
            3550,
            "CNY",
            Some(&category.category_id),
            "麦当劳",
            DUPLICATE_WINDOW_SECS,
        )
        .unwrap();
    assert_eq!(
        hit.map(|entry| entry.entry_id),
        Some(first.entry_id),
        "内容相同的一笔应当被闸住"
    );
}

#[test]
fn duplicate_gate_does_not_block_genuinely_different_entries() {
    let (_dir, db) = temp_db();
    let (book, category) = book_with_category(&db);
    add_meal(&db, &book, &category, "35.50", "麦当劳");

    // 金额不同、备注不同,都是另一笔账,不能挡。
    for (amount, note) in [(3551_i64, "麦当劳"), (3550, "肯德基")] {
        let hit = db
            .find_recent_duplicate(
                &book.book_id,
                EntryKind::Expense,
                amount,
                "CNY",
                Some(&category.category_id),
                note,
                DUPLICATE_WINDOW_SECS,
            )
            .unwrap();
        assert!(hit.is_none(), "{amount}/{note} 不该被当成重复");
    }
}

#[test]
fn duplicate_gate_only_looks_back_inside_the_window() {
    let (_dir, db) = temp_db();
    let (book, category) = book_with_category(&db);
    add_meal(&db, &book, &category, "35.50", "麦当劳");

    // 窗口设成 0 秒等于「只看此刻之后」,刚记的那笔就落在窗口外了。
    let hit = db
        .find_recent_duplicate(
            &book.book_id,
            EntryKind::Expense,
            3550,
            "CNY",
            Some(&category.category_id),
            "麦当劳",
            0,
        )
        .unwrap();
    assert!(hit.is_none(), "窗口之外的旧账不该算重复");
}

#[test]
fn delete_is_soft_and_reversible() {
    let (_dir, db) = temp_db();
    let (book, category) = book_with_category(&db);
    let entry = add_meal(&db, &book, &category, "35.50", "麦当劳");

    db.delete_entry(&entry.entry_id).unwrap();
    let (items, total) = db
        .list_entries(&EntryFilter {
            book_id: book.book_id.clone(),
            limit: 50,
            ..Default::default()
        })
        .unwrap();
    assert_eq!(total, 0, "删掉的账不该出现在默认列表里");
    assert!(items.is_empty());

    // 行还在,带上开关就能看到,也能恢复。
    let (_, total_with_deleted) = db
        .list_entries(&EntryFilter {
            book_id: book.book_id.clone(),
            include_deleted: true,
            limit: 50,
            ..Default::default()
        })
        .unwrap();
    assert_eq!(total_with_deleted, 1, "软删除不该真的丢行");

    db.restore_entry(&entry.entry_id).unwrap();
    let (_, total_after_restore) = db
        .list_entries(&EntryFilter {
            book_id: book.book_id.clone(),
            limit: 50,
            ..Default::default()
        })
        .unwrap();
    assert_eq!(total_after_restore, 1);
}

#[test]
fn deleting_twice_is_not_an_error() {
    let (_dir, db) = temp_db();
    let (book, category) = book_with_category(&db);
    let entry = add_meal(&db, &book, &category, "35.50", "麦当劳");
    db.delete_entry(&entry.entry_id).unwrap();
    // 重试安全:模型没收到上一次的结果时会再调一遍。
    db.delete_entry(&entry.entry_id).unwrap();
    assert!(db.delete_entry("en_deadbeef0000").is_err());
}

#[test]
fn stale_revision_is_refused_instead_of_overwriting() {
    let (_dir, db) = temp_db();
    let (book, category) = book_with_category(&db);
    let entry = add_meal(&db, &book, &category, "35.50", "麦当劳");

    // 第一次改成功,版本号推进。
    let updated = db
        .update_entry(
            &entry.entry_id,
            entry.revision,
            EntryPatch {
                note: Some("麦当劳午餐".to_string()),
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(updated.revision, 2);
    assert_eq!(updated.note, "麦当劳午餐");

    // 拿着旧版本号再改就该被拒——否则 WebUI 与模型同时改会静默覆盖。
    let error = db
        .update_entry(
            &entry.entry_id,
            entry.revision,
            EntryPatch {
                note: Some("别的".to_string()),
                ..Default::default()
            },
        )
        .unwrap_err();
    assert!(
        error.to_string().contains("changed since it was read"),
        "{error}"
    );
    assert_eq!(
        db.get_entry(&entry.entry_id).unwrap().unwrap().note,
        "麦当劳午餐"
    );
}

#[test]
fn patch_can_clear_a_nullable_field() {
    let (_dir, db) = temp_db();
    let (book, category) = book_with_category(&db);
    let entry = add_meal(&db, &book, &category, "35.50", "麦当劳");
    assert!(entry.category_id.is_some());

    // Some(None) 是「清空」,与 None 的「不动」是两回事。
    let updated = db
        .update_entry(
            &entry.entry_id,
            entry.revision,
            EntryPatch {
                category_id: Some(None),
                ..Default::default()
            },
        )
        .unwrap();
    assert!(updated.category_id.is_none());
    assert_eq!(updated.note, "麦当劳", "没传的字段不该被动");
}

#[test]
fn filters_narrow_by_kind_day_and_text() {
    let (_dir, db) = temp_db();
    let (book, category) = book_with_category(&db);
    add_meal(&db, &book, &category, "35.50", "麦当劳");
    add_meal(&db, &book, &category, "12.00", "便利店");
    add_simple(&db, &book, EntryKind::Income, "5000", None, None);

    let by_kind = db
        .list_entries(&EntryFilter {
            book_id: book.book_id.clone(),
            kind: Some(EntryKind::Income),
            limit: 50,
            ..Default::default()
        })
        .unwrap();
    assert_eq!(by_kind.1, 1);

    let by_text = db
        .list_entries(&EntryFilter {
            book_id: book.book_id.clone(),
            query: Some("麦当劳".to_string()),
            limit: 50,
            ..Default::default()
        })
        .unwrap();
    assert_eq!(by_text.1, 1);

    // 只查明天,今天记的三笔都不该出现。
    let tomorrow = "2999-01-01".to_string();
    let by_day = db
        .list_entries(&EntryFilter {
            book_id: book.book_id.clone(),
            from_day: Some(tomorrow),
            limit: 50,
            ..Default::default()
        })
        .unwrap();
    assert_eq!(by_day.1, 0);
}

#[test]
fn entry_resolves_by_id_prefix() {
    let (_dir, db) = temp_db();
    let (book, category) = book_with_category(&db);
    let entry = add_meal(&db, &book, &category, "35.50", "麦当劳");

    // 模型转述 id 时经常只报前几位。
    let resolved = db
        .resolve_entry(&book.book_id, &entry.entry_id[..8])
        .unwrap();
    assert_eq!(resolved.entry_id, entry.entry_id);
    assert!(db.resolve_entry(&book.book_id, "en_nope").is_err());
}

#[test]
fn transfer_between_the_same_account_is_rejected_by_the_schema() {
    let (_dir, db) = temp_db();
    let book = db.create_book("生活", "CNY").unwrap();
    // 建账本时已经铺好了「现金」，直接用。
    let cash = db.resolve_account(&book.book_id, "现金").unwrap();

    let result = db.add_entry(NewEntry {
        book_id: book.book_id.clone(),
        kind: EntryKind::Transfer,
        amount_minor: 1000,
        currency: "CNY".to_string(),
        base_amount_minor: Some(1000),
        base_currency: "CNY".to_string(),
        rate: None,
        rate_source: None,
        rate_at: None,
        rate_status: RateStatus::Same,
        account_id: Some(cash.account_id.clone()),
        to_account_id: Some(cash.account_id.clone()),
        category_id: None,
        occurred_at: now_rfc3339(),
        occurred_day: local_day_now(),
        note: String::new(),
        merchant: String::new(),
        source: EntrySource::Chat,
    });
    assert!(result.is_err(), "转给自己应当被 CHECK 约束挡住");
}

#[test]
fn pending_entry_cannot_carry_a_converted_amount() {
    let (_dir, db) = temp_db();
    let book = db.create_book("生活", "CNY").unwrap();

    // 待换算与「已经有换算金额」是互斥状态,库层面就该挡住半截数据。
    let result = db.add_entry(NewEntry {
        book_id: book.book_id.clone(),
        kind: EntryKind::Expense,
        amount_minor: 3000,
        currency: "JPY".to_string(),
        base_amount_minor: Some(14700),
        base_currency: "CNY".to_string(),
        rate: None,
        rate_source: None,
        rate_at: None,
        rate_status: RateStatus::Pending,
        account_id: None,
        to_account_id: None,
        category_id: None,
        occurred_at: now_rfc3339(),
        occurred_day: local_day_now(),
        note: String::new(),
        merchant: String::new(),
        source: EntrySource::Chat,
    });
    assert!(result.is_err());
}
