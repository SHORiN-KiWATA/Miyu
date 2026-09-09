//! 金额解析与换算。
//!
//! 这一组测试守的是同一条底线：**宁可报错，也不静默改用户报的数字**。

use crate::ledger::money::*;

#[test]
fn parses_plain_and_decorated_amounts() {
    assert_eq!(parse_amount("35.50", "CNY").unwrap(), 3550);
    assert_eq!(parse_amount("35.5", "CNY").unwrap(), 3550, "补齐小数位");
    assert_eq!(parse_amount("35", "CNY").unwrap(), 3500);
    assert_eq!(parse_amount("¥35.50", "CNY").unwrap(), 3550);
    assert_eq!(parse_amount("1,234.00", "CNY").unwrap(), 123400);
    assert_eq!(parse_amount(" 12 ", "CNY").unwrap(), 1200);
    assert_eq!(parse_amount(".5", "CNY").unwrap(), 50);
}

#[test]
fn zero_decimal_currencies_have_no_minor_unit() {
    assert_eq!(parse_amount("3000", "JPY").unwrap(), 3000);
    assert_eq!(format_amount(3000, "JPY"), "3000");
    assert_eq!(currency_scale("JPY"), 0);
    assert_eq!(currency_scale("KWD"), 3);
    assert_eq!(currency_scale("CNY"), 2);
    assert_eq!(currency_scale("ZZZ"), 2, "未知币种按两位处理");
}

#[test]
fn extra_decimal_places_are_rejected_not_rounded() {
    // 四舍五入会把「用户说错了」变成「账本上多了几分钱」,而后者要到
    // 对账时才发现。
    let error = parse_amount("35.555", "CNY").unwrap_err();
    assert!(error.to_string().contains("decimal places"), "{error}");
    let error = parse_amount("3000.5", "JPY").unwrap_err();
    assert!(error.to_string().contains("decimal places"), "{error}");
}

#[test]
fn negative_and_malformed_amounts_are_rejected() {
    // 收支方向由 kind 决定,负号进来说明调用方理解错了。
    assert!(parse_amount("-10", "CNY").is_err());
    assert!(parse_amount("0", "CNY").is_err());
    assert!(parse_amount("", "CNY").is_err());
    assert!(parse_amount("abc", "CNY").is_err());
    assert!(parse_amount("1e5", "CNY").is_err(), "不认科学计数法");
    assert!(parse_amount("99999999999999999999", "CNY").is_err());
}

#[test]
fn formats_round_trip() {
    for (minor, currency, text) in [
        (3550_i64, "CNY", "35.50"),
        (5_i64, "CNY", "0.05"),
        (3000_i64, "JPY", "3000"),
        (1_234_500_i64, "KWD", "1234.500"),
    ] {
        assert_eq!(format_amount(minor, currency), text);
        assert_eq!(parse_amount(text, currency).unwrap(), minor);
    }
}

#[test]
fn currency_names_normalise_to_iso_codes() {
    assert_eq!(validate_currency("日元").unwrap(), "JPY");
    assert_eq!(validate_currency("人民币").unwrap(), "CNY");
    assert_eq!(validate_currency("刀").unwrap(), "USD");
    assert_eq!(validate_currency("jpy").unwrap(), "JPY");
    assert!(validate_currency("不是币种").is_err());
    assert!(validate_currency("CNYY").is_err());
}

#[test]
fn conversion_crosses_currencies_with_different_precision() {
    // 3000 日元(scale 0)按 0.049 换成人民币(scale 2)= 147.00 元 = 14700 分。
    // 直接拿 minor 相乘会得到 147 分,差两个数量级——这就是这个函数存在的理由。
    assert_eq!(convert_minor(3000, "JPY", "CNY", 0.049).unwrap(), 14700);
    // 反向:100 元换日元。
    assert_eq!(convert_minor(10000, "CNY", "JPY", 20.4).unwrap(), 2040);
    // 同精度币种。
    assert_eq!(convert_minor(10000, "USD", "CNY", 7.1).unwrap(), 71000);
}

#[test]
fn conversion_rejects_impossible_rates() {
    assert!(convert_minor(100, "CNY", "JPY", 0.0).is_err());
    assert!(convert_minor(100, "CNY", "JPY", -1.0).is_err());
    assert!(convert_minor(100, "CNY", "JPY", f64::NAN).is_err());
    assert!(convert_minor(100, "CNY", "JPY", f64::INFINITY).is_err());
}

#[test]
fn rate_snapshots_survive_a_round_trip_through_text() {
    for rate in [0.049_f64, 7.123_456, 20.4, 1.0] {
        let text = format_rate(rate);
        assert!(!text.contains('e'), "落库形式不该是指数记法: {text}");
        let parsed = parse_rate(&text).unwrap();
        assert!((parsed - rate).abs() < 1e-9, "{rate} -> {text} -> {parsed}");
    }
    assert!(parse_rate("0").is_err());
    assert!(parse_rate("abc").is_err());
}
