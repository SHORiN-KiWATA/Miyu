//! 赞助记账：合计、榜单、增删改。
//!
//! 这一层的断言围绕两件事：**钱不能算错**（整数分、多币种分列、折算值只在记账
//! 当刻算一次），以及**榜单口径要稳**（同一批数据换个排序参数不会换出另一套
//! 合计）。

use super::shared::*;
use crate::state::*;

fn db() -> (tempfile::TempDir, ConversationDb) {
    let temp = tempfile::tempdir().unwrap();
    let db = ConversationDb::open(&test_paths(temp.path()).state_dir).unwrap();
    (temp, db)
}

fn record(
    sponsor_id: &str,
    name: &str,
    amount_minor: i64,
    currency: &str,
    cny_minor: i64,
) -> NewSponsorRecord {
    NewSponsorRecord {
        platform: "onebot".into(),
        account_id: "10000".into(),
        sponsor_id: sponsor_id.into(),
        sponsor_name: name.into(),
        amount_minor,
        currency: currency.into(),
        cny_minor,
        fx_rate: if currency == "CNY" { 0.0 } else { 7.1 },
        fx_source: if currency == "CNY" { "" } else { "live" }.into(),
        note: String::new(),
        recorded_by: "admin".into(),
        sponsored_at: String::new(),
    }
}

#[test]
fn totals_split_by_currency_and_rank_by_the_converted_value() {
    let (_temp, db) = db();
    // 甲：30 元 + 10 美元（按 7.1 折 71 元）= 101 元
    db.add_sponsor_record(&record("111", "甲", 3000, "CNY", 3000))
        .unwrap();
    db.add_sponsor_record(&record("111", "甲", 1000, "USD", 7100))
        .unwrap();
    // 乙：只有 80 元，笔数比甲少
    db.add_sponsor_record(&record("222", "乙", 8000, "CNY", 8000))
        .unwrap();

    let by_amount = db.sponsor_totals(SponsorOrder::Amount, 10).unwrap();
    assert_eq!(by_amount.len(), 2);
    assert_eq!(by_amount[0].sponsor_id, "111");
    assert_eq!(by_amount[0].cny_minor, 10100);
    // 原始金额按币种分列，不折在一起——折算值只是排序用的一个口径。
    assert_eq!(by_amount[0].by_currency.get("CNY"), Some(&3000));
    assert_eq!(by_amount[0].by_currency.get("USD"), Some(&1000));
    assert_eq!(by_amount[1].sponsor_id, "222");

    let by_count = db.sponsor_totals(SponsorOrder::Count, 10).unwrap();
    assert_eq!(by_count[0].sponsor_id, "111");
    assert_eq!(by_count[0].record_count, 2);
    // 换排序不该换出另一套合计。
    assert_eq!(by_count[0].cny_minor, by_amount[0].cny_minor);
}

#[test]
fn an_unconverted_record_stays_off_the_cny_leaderboard() {
    let (_temp, db) = db();
    let mut unconverted = record("333", "丙", 5000, "USD", 0);
    unconverted.fx_rate = 0.0;
    unconverted.fx_source = "unconverted".into();
    db.add_sponsor_record(&unconverted).unwrap();
    db.add_sponsor_record(&record("444", "丁", 100, "CNY", 100))
        .unwrap();

    let totals = db.sponsor_totals(SponsorOrder::Amount, 10).unwrap();
    // 丁只有 1 元，却排在没折算的丙前面——这是有意的：宁可排错也不编一个汇率。
    assert_eq!(totals[0].sponsor_id, "444");
    let unconverted_total = totals.iter().find(|t| t.sponsor_id == "333").unwrap();
    assert_eq!(unconverted_total.cny_minor, 0);
    assert_eq!(unconverted_total.by_currency.get("USD"), Some(&5000));

    let summary = db.sponsor_summary().unwrap();
    assert_eq!(summary.sponsor_count, 2);
    assert_eq!(summary.record_count, 2);
    assert_eq!(summary.by_currency.get("USD"), Some(&5000));
}

#[test]
fn the_leaderboard_shows_the_latest_name_a_sponsor_used() {
    let (_temp, db) = db();
    db.add_sponsor_record(&record("555", "旧名字", 100, "CNY", 100))
        .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(5));
    db.add_sponsor_record(&record("555", "新名字", 100, "CNY", 100))
        .unwrap();
    let totals = db.sponsor_totals(SponsorOrder::Amount, 10).unwrap();
    assert_eq!(totals[0].sponsor_name, "新名字");
}

#[test]
fn notes_and_names_are_editable_but_a_deleted_record_leaves_the_totals() {
    let (_temp, db) = db();
    let first = db
        .add_sponsor_record(&record("666", "戊", 2000, "CNY", 2000))
        .unwrap();
    db.add_sponsor_record(&record("666", "戊", 1000, "CNY", 1000))
        .unwrap();

    let updated = db
        .update_sponsor_record(first.record_id, Some("买咖啡"), Some("戊戊"))
        .unwrap()
        .unwrap();
    assert_eq!(updated.note, "买咖啡");
    assert_eq!(updated.sponsor_name, "戊戊");
    // 金额不受编辑影响：那是记账当刻冻结的事实。
    assert_eq!(updated.amount_minor, 2000);

    assert!(db.delete_sponsor_record(first.record_id).unwrap());
    assert!(!db.delete_sponsor_record(first.record_id).unwrap());
    let totals = db.sponsor_totals(SponsorOrder::Amount, 10).unwrap();
    assert_eq!(totals[0].cny_minor, 1000);
    assert_eq!(totals[0].record_count, 1);
}

#[test]
fn an_empty_ledger_answers_with_zeros_rather_than_an_error() {
    let (_temp, db) = db();
    assert!(db
        .sponsor_totals(SponsorOrder::Amount, 10)
        .unwrap()
        .is_empty());
    assert!(db.sponsor_records(10, 0).unwrap().is_empty());
    assert!(db.sponsor_records_for("nobody", 10).unwrap().is_empty());
    let summary = db.sponsor_summary().unwrap();
    assert_eq!(summary.record_count, 0);
    assert_eq!(summary.cny_minor, 0);
    assert!(summary.by_currency.is_empty());
}
