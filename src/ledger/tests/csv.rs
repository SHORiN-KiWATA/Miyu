//! CSV 导入导出。
//!
//! 重点是往返一致：导出的文件必须能原样解析回来，含逗号、引号、换行的
//! 备注一个字符都不能变形。

use super::temp_db;
use crate::ledger::csv::{parse_csv, HEADER};
use crate::ledger::entries::NewEntry;
use crate::ledger::types::*;
use crate::ledger::{now_rfc3339, LedgerDb};

fn add(db: &LedgerDb, book: &BookRecord, amount_minor: i64, note: &str, day: &str) -> EntryRecord {
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
        category_id: None,
        occurred_at: now_rfc3339(),
        occurred_day: day.to_string(),
        note: note.to_string(),
        merchant: String::new(),
        source: EntrySource::Chat,
    })
    .expect("add entry")
}

#[test]
fn export_starts_with_the_documented_header() {
    let (_dir, db) = temp_db();
    let book = db.create_book("生活", "CNY").unwrap();
    let csv = db.export_csv(&book, "2026-09-01", "2026-09-31").unwrap();
    assert_eq!(csv.lines().next().unwrap(), HEADER.join(","));
}

#[test]
fn awkward_notes_survive_the_round_trip() {
    let (_dir, db) = temp_db();
    let book = db.create_book("生活", "CNY").unwrap();
    // 逗号、双引号、换行——CSV 的三个经典雷,一次全踩。
    let note = "麦当劳, \"大\"份\n加一杯咖啡";
    add(&db, &book, 3550, note, "2026-09-08");

    let csv = db.export_csv(&book, "2026-09-01", "2026-09-31").unwrap();
    let rows = parse_csv(&csv).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].note, note);
    assert_eq!(rows[0].amount, "35.50");
    assert_eq!(rows[0].currency, "CNY");
    assert_eq!(rows[0].kind, "expense");
    assert_eq!(rows[0].date, "2026-09-08");
}

#[test]
fn deleted_entries_are_not_exported() {
    let (_dir, db) = temp_db();
    let book = db.create_book("生活", "CNY").unwrap();
    let entry = add(&db, &book, 3550, "麦当劳", "2026-09-08");
    add(&db, &book, 1200, "便利店", "2026-09-08");
    db.delete_entry(&entry.entry_id).unwrap();

    let csv = db.export_csv(&book, "2026-09-01", "2026-09-31").unwrap();
    let rows = parse_csv(&csv).unwrap();
    assert_eq!(rows.len(), 1, "删掉的账在账面上已经不存在");
    assert_eq!(rows[0].note, "便利店");
}

#[test]
fn export_pages_past_the_query_limit() {
    let (_dir, db) = temp_db();
    let book = db.create_book("生活", "CNY").unwrap();
    // 单次查询上限是 500 行，导出必须翻页而不是截断。
    for index in 0..520 {
        add(
            &db,
            &book,
            100 + index,
            &format!("第 {index} 笔"),
            "2026-09-08",
        );
    }
    let csv = db.export_csv(&book, "2026-09-01", "2026-09-31").unwrap();
    assert_eq!(parse_csv(&csv).unwrap().len(), 520);
}

#[test]
fn parses_quoted_fields_and_a_bom() {
    let text = "\u{feff}date,kind,amount,currency,note\n\
                2026-09-08,expense,35.50,CNY,\"带,逗号\"\n\
                2026-09-09,income,100,CNY,普通\n";
    let rows = parse_csv(text).unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].note, "带,逗号");
    assert_eq!(rows[1].kind, "income");
}

#[test]
fn column_order_does_not_matter_and_extras_are_ignored() {
    let text = "amount,date,kind,不认识的列,note\n35.50,2026-09-08,expense,忽略我,午饭\n";
    let rows = parse_csv(text).unwrap();
    assert_eq!(rows[0].amount, "35.50");
    assert_eq!(rows[0].date, "2026-09-08");
    assert_eq!(rows[0].note, "午饭");
}

#[test]
fn blank_lines_are_skipped_not_errors() {
    let text = "date,kind,amount\n2026-09-08,expense,10\n\n,,\n2026-09-09,expense,20\n";
    assert_eq!(parse_csv(text).unwrap().len(), 2);
}

#[test]
fn a_header_without_the_essential_columns_is_rejected() {
    assert!(parse_csv("").is_err());
    assert!(parse_csv("foo,bar\n1,2\n").is_err());
    assert!(
        parse_csv("date,kind\n2026-09-08,expense\n").is_err(),
        "缺 amount"
    );
}

#[test]
fn import_dedup_counts_rows_instead_of_asking_whether_one_exists() {
    let (_dir, db) = temp_db();
    let book = db.create_book("生活", "CNY").unwrap();
    let imported = |note: &str| {
        db.add_entry(NewEntry {
            book_id: book.book_id.clone(),
            kind: EntryKind::Expense,
            amount_minor: 1200,
            currency: "CNY".to_string(),
            base_amount_minor: Some(1200),
            base_currency: "CNY".to_string(),
            rate: None,
            rate_source: None,
            rate_at: None,
            rate_status: RateStatus::Same,
            account_id: None,
            to_account_id: None,
            category_id: None,
            occurred_at: now_rfc3339(),
            occurred_day: "2026-09-05".to_string(),
            note: note.to_string(),
            merchant: String::new(),
            source: EntrySource::Import,
        })
        .unwrap()
    };
    let count = || {
        db.count_import_rows(
            &book.book_id,
            "2026-09-05",
            EntryKind::Expense,
            1200,
            "CNY",
            "地铁",
        )
        .unwrap()
    };

    // 这条是 09-09 真机实测钉下来的:一份写了两趟同样车费的文件,第一次
    // 导入该进两笔。只回答「有没有」的判据会把第二趟永久挡在门外。
    assert_eq!(count(), 0);
    imported("地铁");
    assert_eq!(count(), 1, "库里一条,文件里第二行仍该放行");
    imported("地铁");
    assert_eq!(count(), 2, "两条都在,整份文件再导一次才会全被抵扣");

    // 换一天、换备注都是另一个内容键。
    assert_eq!(
        db.count_import_rows(
            &book.book_id,
            "2026-09-06",
            EntryKind::Expense,
            1200,
            "CNY",
            "地铁"
        )
        .unwrap(),
        0
    );
    assert_eq!(
        db.count_import_rows(
            &book.book_id,
            "2026-09-05",
            EntryKind::Expense,
            1200,
            "CNY",
            "公交"
        )
        .unwrap(),
        0
    );
}

#[test]
fn deleting_an_imported_row_frees_its_slot() {
    let (_dir, db) = temp_db();
    let book = db.create_book("生活", "CNY").unwrap();
    let entry = db
        .add_entry(NewEntry {
            book_id: book.book_id.clone(),
            kind: EntryKind::Expense,
            amount_minor: 3550,
            currency: "CNY".to_string(),
            base_amount_minor: Some(3550),
            base_currency: "CNY".to_string(),
            rate: None,
            rate_source: None,
            rate_at: None,
            rate_status: RateStatus::Same,
            account_id: None,
            to_account_id: None,
            category_id: None,
            occurred_at: now_rfc3339(),
            occurred_day: "2026-09-08".to_string(),
            note: "麦当劳".to_string(),
            merchant: String::new(),
            source: EntrySource::Import,
        })
        .unwrap();
    let count = || {
        db.count_import_rows(
            &book.book_id,
            "2026-09-08",
            EntryKind::Expense,
            3550,
            "CNY",
            "麦当劳",
        )
        .unwrap()
    };
    assert_eq!(count(), 1);
    // 删掉之后再导入同一个文件,应该能重新导进来。
    db.delete_entry(&entry.entry_id).unwrap();
    assert_eq!(count(), 0);
}

#[test]
fn manually_recorded_entries_do_not_block_import() {
    let (_dir, db) = temp_db();
    let book = db.create_book("生活", "CNY").unwrap();
    // source=chat 的账不算「导入过」——否则手记过一笔就再也导不进同样的一笔。
    add(&db, &book, 3550, "麦当劳", "2026-09-08");
    assert_eq!(
        db.count_import_rows(
            &book.book_id,
            "2026-09-08",
            EntryKind::Expense,
            3550,
            "CNY",
            "麦当劳"
        )
        .unwrap(),
        0
    );
}
