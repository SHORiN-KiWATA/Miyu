//! 金额与币种：最小货币单位（minor units）的解析与格式化。
//!
//! 账本里的钱一律是 `i64` 最小单位：日元 3000 存 3000，人民币 35.50 存 3550。
//! 浮点只在汇率换算的中间步骤出现一次，随即落回整数——**进库的永远是整数**。
//! 项目里既有的 `f64` 金额（usage 的成本估算）是「读取时算、不落盘」的派生量，
//! 与这里的权威数据不是一回事，不要照抄那边的口径。

use anyhow::{bail, Result};

/// 单笔金额上限，约 90 万亿最小单位。防的是解析出天文数字后在换算里溢出，
/// 不是业务限制。
pub(crate) const MAX_AMOUNT_MINOR: i64 = 90_000_000_000_000;

/// ISO 4217 里小数位不是 2 的币种。其余一律 2 位。
const SCALE_0: &[&str] = &[
    "JPY", "KRW", "VND", "CLP", "ISK", "PYG", "RWF", "UGX", "VUV", "XAF", "XOF", "XPF", "KMF",
    "DJF", "GNF", "BIF", "MGA",
];
const SCALE_3: &[&str] = &["BHD", "IQD", "JOD", "KWD", "LYD", "OMR", "TND"];

/// 币种的小数位数。未知币种按 2 位处理——这是绝大多数币种的实际精度，
/// 猜错也只是多存两位零，不会改变数值。
pub(crate) fn currency_scale(code: &str) -> u32 {
    let code = code.trim().to_uppercase();
    if SCALE_0.contains(&code.as_str()) {
        0
    } else if SCALE_3.contains(&code.as_str()) {
        3
    } else {
        2
    }
}

/// 把用户/模型说的币种名归一成 ISO 4217 代码。
///
/// 中文名映射与 `tools/exchange_rate.rs` 同源；这里多认几个日常叫法，
/// 因为记账入口是自然语言（「三千日元」「50 刀」）。认不出的原样返回，
/// 由 [`validate_currency`] 判定合法性——**不猜**。
pub(crate) fn normalize_currency(value: &str) -> String {
    let raw = value.trim();
    match raw.to_uppercase().as_str() {
        "人民币" | "元" | "块" | "块钱" | "RMB" | "￥" => "CNY".to_string(),
        "美元" | "美金" | "刀" | "美刀" | "$" => "USD".to_string(),
        "日元" | "日圆" | "円" => "JPY".to_string(),
        "欧元" | "€" => "EUR".to_string(),
        "英镑" | "£" => "GBP".to_string(),
        "港币" | "港元" => "HKD".to_string(),
        "台币" | "新台币" => "TWD".to_string(),
        "韩元" | "韩币" => "KRW".to_string(),
        "澳元" | "澳币" => "AUD".to_string(),
        "加元" | "加币" => "CAD".to_string(),
        "新加坡元" | "新币" => "SGD".to_string(),
        "泰铢" => "THB".to_string(),
        "卢布" => "RUB".to_string(),
        code => code.to_string(),
    }
}

/// 合法币种是三个 ASCII 字母。宽进严出：解析在前，校验在这里收口。
pub(crate) fn validate_currency(code: &str) -> Result<String> {
    let code = normalize_currency(code);
    if code.len() != 3 || !code.chars().all(|c| c.is_ascii_alphabetic()) {
        bail!("currency must be a 3-letter code like CNY or JPY, got {code:?}");
    }
    Ok(code.to_uppercase())
}

/// 解析人写的金额字符串成最小单位整数。
///
/// 接受 `"35.5"` / `"35.50"` / `"¥35.50"` / `"1,234.00"` / `"3000"`。
///
/// 三条硬规则，都是为了「宁可报错也不要静默改数字」：
/// 1. 小数位多于币种精度直接报错，不四舍五入（`35.555` 进人民币账是错误输入，
///    不是「约等于 35.56」）。
/// 2. 不接受负数——收支方向由 `kind` 决定，负号进来说明调用方理解错了。
/// 3. 不接受科学计数法、不接受空串。
pub(crate) fn parse_amount(input: &str, currency: &str) -> Result<i64> {
    let scale = currency_scale(currency);
    let cleaned: String = input
        .chars()
        .filter(|c| !c.is_whitespace() && *c != ',' && *c != '_')
        .collect();
    // 币种符号只在开头剥一次:结尾出现符号(如 "35¥")同样常见,一并处理。
    let cleaned = cleaned
        .trim_start_matches(['¥', '￥', '$', '€', '£', '₩', '฿'])
        .trim_end_matches(['¥', '￥', '$', '€', '£', '₩', '฿'])
        .to_string();
    if cleaned.is_empty() {
        bail!("amount is empty");
    }
    if cleaned.starts_with('-') {
        bail!("amount must be positive; the entry kind decides income or expense");
    }
    let cleaned = cleaned.trim_start_matches('+');

    let (whole, frac) = match cleaned.split_once('.') {
        Some((whole, frac)) => (whole, frac),
        None => (cleaned, ""),
    };
    if whole.is_empty() && frac.is_empty() {
        bail!("amount {input:?} is not a number");
    }
    if !whole.chars().all(|c| c.is_ascii_digit()) || !frac.chars().all(|c| c.is_ascii_digit()) {
        bail!("amount {input:?} is not a plain decimal number");
    }
    if frac.len() as u32 > scale {
        bail!(
            "amount {input:?} has {} decimal places but {currency} allows {scale}",
            frac.len()
        );
    }

    let whole: i64 = if whole.is_empty() {
        0
    } else {
        whole
            .parse()
            .map_err(|_| anyhow::anyhow!("amount {input:?} is too large"))?
    };
    let factor = 10_i64.pow(scale);
    // 补齐小数位:"35.5" 在 scale=2 下是 50 分而不是 5 分。
    let mut frac_value: i64 = if frac.is_empty() { 0 } else { frac.parse()? };
    for _ in frac.len() as u32..scale {
        frac_value *= 10;
    }
    let minor = whole
        .checked_mul(factor)
        .and_then(|value| value.checked_add(frac_value))
        .ok_or_else(|| anyhow::anyhow!("amount {input:?} is too large"))?;
    if minor <= 0 {
        bail!("amount must be greater than zero");
    }
    if minor > MAX_AMOUNT_MINOR {
        bail!("amount {input:?} exceeds the supported range");
    }
    Ok(minor)
}

/// 最小单位整数格式化成小数字符串，不带币种符号。`3550` + CNY → `"35.50"`。
pub(crate) fn format_amount(minor: i64, currency: &str) -> String {
    let scale = currency_scale(currency);
    let negative = minor < 0;
    let magnitude = minor.unsigned_abs();
    let factor = 10_u64.pow(scale);
    let whole = magnitude / factor;
    let frac = magnitude % factor;
    let sign = if negative { "-" } else { "" };
    if scale == 0 {
        format!("{sign}{whole}")
    } else {
        format!("{sign}{whole}.{frac:0width$}", width = scale as usize)
    }
}

/// 带币种的人类可读形式，给模型和 CSV 用：`"35.50 CNY"`。
///
/// 不用货币符号——`¥` 同时是人民币和日元，写成代码没有歧义。
pub(crate) fn format_with_currency(minor: i64, currency: &str) -> String {
    format!("{} {}", format_amount(minor, currency), currency)
}

/// 按汇率把原币种金额换算成账本目标币种，返回目标币种的最小单位。
///
/// 换算是整个账本里唯一碰浮点的地方，所以收口在这一个函数里：
/// 两端的最小单位精度不同（日元 0 位 → 人民币 2 位），必须先还原成
/// 「主单位数值」再乘汇率再放大，直接拿 minor 相乘会差几个数量级。
/// 结果按四舍五入取整——这里的舍入是换算固有的，不是在改用户报的数字。
pub(crate) fn convert_minor(
    amount_minor: i64,
    from_currency: &str,
    to_currency: &str,
    rate: f64,
) -> Result<i64> {
    if !rate.is_finite() || rate <= 0.0 {
        bail!("exchange rate must be a positive number, got {rate}");
    }
    let from_factor = 10_f64.powi(currency_scale(from_currency) as i32);
    let to_factor = 10_f64.powi(currency_scale(to_currency) as i32);
    let major = amount_minor as f64 / from_factor;
    let converted = (major * rate * to_factor).round();
    if !converted.is_finite() || converted < 0.0 || converted > MAX_AMOUNT_MINOR as f64 {
        bail!("converted amount is out of range");
    }
    Ok(converted as i64)
}

/// 汇率以字符串落库（全精度、不受二进制浮点表示影响），读回来再解析。
pub(crate) fn parse_rate(raw: &str) -> Result<f64> {
    let rate: f64 = raw
        .trim()
        .parse()
        .map_err(|_| anyhow::anyhow!("stored exchange rate {raw:?} is not a number"))?;
    if !rate.is_finite() || rate <= 0.0 {
        bail!("stored exchange rate {raw:?} is not positive");
    }
    Ok(rate)
}

/// 汇率的落库形式。用 `{:.10}` 而不是 `to_string()`：后者对某些值会给出
/// 指数形式，读回来虽然也能解析，但人在 CSV 或面板里看到 `4.9e-2` 会懵。
pub(crate) fn format_rate(rate: f64) -> String {
    let text = format!("{rate:.10}");
    let trimmed = text.trim_end_matches('0').trim_end_matches('.');
    if trimmed.is_empty() {
        "0".to_string()
    } else {
        trimmed.to_string()
    }
}
