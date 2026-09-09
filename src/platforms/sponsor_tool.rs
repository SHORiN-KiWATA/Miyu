//! `sponsor`：通讯平台会话里的赞助记账工具。
//!
//! 一个工具、五个动作（add / list / query / update / delete）。域内聚合而不是
//! 拆成五个名字——记账、看榜、查人是同一件事的三个面，拆开只会让 tools 数组多
//! 背四份外壳（AGENTS §2.2）。
//!
//! **写限管理员，但不硬报错。** 非管理员调 add 拿到的是一句 `ok:false` 的说明
//! 而不是 `Err`：报错会被当成工具故障，模型多半会换个参数重试；一句"这个只有
//! 管理员能记"她能直接转述给群友。
//!
//! **美元按记账当刻的实时汇率折成人民币，冻结在行里。** 排行榜要的是稳定名
//! 次，不能每次打开都重新拉一次汇率、让历史名次自己晃。拉不到汇率也照样记，
//! 只是这笔暂不计入人民币榜——宁可榜上少一笔，也不能往账本里塞一个编造的数。

use super::PlatformTurnContext;
use crate::state::{NewSponsorRecord, SponsorOrder, SponsorRecord, SponsorTotal};
use crate::tools::{ToolRegistry, ToolSpec};
use anyhow::{bail, Result};
use serde_json::{json, Value};
use std::sync::Arc;

/// 榜单/明细一次最多回这么多条。回给模型的东西要克制。
const DEFAULT_LIMIT: usize = 10;
const MAX_LIMIT: usize = 50;
/// 一笔的上限（分）：一亿元。真有这种赞助请手动改库。
const MAX_AMOUNT_MINOR: i64 = 100_000_000 * 100;
const MAX_NOTE_CHARS: usize = 200;

pub(crate) fn register(registry: &mut ToolRegistry, context: Arc<PlatformTurnContext>) {
    registry.register(
        ToolSpec::new(
            "sponsor",
            "Keep the sponsorship ledger for this bot. action=add records one donation. action=list returns the leaderboard. action=query returns one person's records and totals. action=update edits a record's note or the sponsor's display name. action=delete removes a record. Only an administrator may add, update or delete. Amounts are given in whole currency units, cny or usd; a usd amount is converted to cny at the rate on the day it is recorded and that rate is frozen with the record.",
            json!({
                "type": "object",
                "properties": {
                    "action": {
                        "type": "string",
                        "enum": ["add", "list", "query", "update", "delete"],
                        "description": "Defaults to list."
                    },
                    "sponsor_id": {
                        "type": "string",
                        "description": "The sponsor's platform id (a QQ number). Required by add and query."
                    },
                    "sponsor_name": {
                        "type": "string",
                        "description": "Display name to show on the leaderboard. Optional with add, settable with update."
                    },
                    "amount": {
                        "type": "number",
                        "description": "Amount in whole currency units, for example 30 or 9.99. Required by add."
                    },
                    "currency": {
                        "type": "string",
                        "enum": ["cny", "usd"],
                        "description": "Defaults to cny."
                    },
                    "note": {
                        "type": "string",
                        "description": "Free-form note, for example what it was for. Optional."
                    },
                    "order": {
                        "type": "string",
                        "enum": ["amount", "count", "recent"],
                        "description": "list only. amount ranks by converted cny total and is the default."
                    },
                    "limit": {
                        "type": "integer",
                        "description": "How many rows to return. Defaults to 10, capped at 50."
                    },
                    "record_id": {
                        "type": "integer",
                        "description": "Required by update and delete. Comes from query."
                    }
                },
                "additionalProperties": false
            }),
            move |args: Value| {
                let context = context.clone();
                async move { run(args, context).await }
            },
        )
        .writes()
        .with_always_loaded(false)
        .with_display_name("Sponsorships"),
    );
}

/// 非管理员碰写操作时的软拒绝。
fn refused(action: &str) -> Result<String> {
    Ok(json!({
        "ok": false,
        "refused": true,
        "reason": format!("only an administrator may {action} sponsorship records"),
    })
    .to_string())
}

fn text_arg<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

fn limit_arg(args: &Value) -> usize {
    args.get("limit")
        .and_then(Value::as_u64)
        .map(|limit| limit as usize)
        .filter(|limit| *limit > 0)
        .unwrap_or(DEFAULT_LIMIT)
        .min(MAX_LIMIT)
}

/// 主币种单位 → 分。模型可能把金额写成字符串，收下（AGENTS §2.4）。
fn amount_minor(args: &Value) -> Result<i64> {
    let raw = match args.get("amount") {
        Some(Value::Number(number)) => number.as_f64(),
        Some(Value::String(text)) => text
            .trim()
            .trim_start_matches(['¥', '$', '￥'])
            .parse()
            .ok(),
        _ => None,
    };
    let Some(raw) = raw else {
        bail!("amount is required with action add, as a number of whole currency units")
    };
    if !raw.is_finite() || raw <= 0.0 {
        bail!("amount must be a positive number, got {raw}")
    }
    let minor = (raw * 100.0).round() as i64;
    if minor <= 0 || minor > MAX_AMOUNT_MINOR {
        bail!("amount is out of range")
    }
    Ok(minor)
}

fn currency_arg(args: &Value) -> Result<String> {
    let raw = text_arg(args, "currency").unwrap_or("cny");
    match raw.to_ascii_lowercase().as_str() {
        "cny" | "rmb" | "yuan" | "人民币" | "元" => Ok("CNY".to_string()),
        "usd" | "dollar" | "美元" | "美金" => Ok("USD".to_string()),
        other => bail!("unsupported currency {other}; expected cny or usd"),
    }
}

fn money(minor: i64, currency: &str) -> String {
    let symbol = if currency == "USD" { "$" } else { "¥" };
    format!("{symbol}{:.2}", minor as f64 / 100.0)
}

fn record_json(record: &SponsorRecord) -> Value {
    let mut value = json!({
        "record_id": record.record_id,
        "sponsor_id": record.sponsor_id,
        "sponsor_name": record.sponsor_name,
        "amount": money(record.amount_minor, &record.currency),
        "currency": record.currency,
        "note": record.note,
        "sponsored_at": record.sponsored_at,
    });
    if record.currency != "CNY" {
        let map = value.as_object_mut().expect("object");
        if record.fx_source == "unconverted" {
            map.insert("cny_equivalent".into(), json!(null));
            map.insert(
                "fx_note".into(),
                json!("no exchange rate was available, so this one is not on the cny leaderboard"),
            );
        } else {
            map.insert(
                "cny_equivalent".into(),
                json!(money(record.cny_minor, "CNY")),
            );
            map.insert("fx_rate".into(), json!(record.fx_rate));
        }
    }
    value
}

fn total_json(rank: usize, total: &SponsorTotal) -> Value {
    let by_currency: Vec<String> = total
        .by_currency
        .iter()
        .map(|(currency, minor)| money(*minor, currency))
        .collect();
    json!({
        "rank": rank,
        "sponsor_id": total.sponsor_id,
        "sponsor_name": total.sponsor_name,
        "total_cny": money(total.cny_minor, "CNY"),
        "totals": by_currency,
        "records": total.record_count,
        "last_sponsored_at": total.last_sponsored_at,
    })
}

async fn run(args: Value, context: Arc<PlatformTurnContext>) -> Result<String> {
    let action = text_arg(&args, "action").unwrap_or("list");
    match action {
        "list" => list(&args, &context),
        "query" => query(&args, &context),
        "add" => add(args, context).await,
        "update" => update(&args, &context),
        "delete" => delete(&args, &context),
        other => bail!("unknown action: {other}; expected add, list, query, update or delete"),
    }
}

fn list(args: &Value, context: &PlatformTurnContext) -> Result<String> {
    let order = SponsorOrder::parse(text_arg(args, "order").unwrap_or("amount"));
    let totals = context.state_store.sponsor_totals(order, limit_arg(args))?;
    let summary = context.state_store.sponsor_summary()?;
    let rows: Vec<Value> = totals
        .iter()
        .enumerate()
        .map(|(index, total)| total_json(index + 1, total))
        .collect();
    Ok(json!({
        "ok": true,
        "leaderboard": rows,
        "sponsors": summary.sponsor_count,
        "records": summary.record_count,
        "total_cny": money(summary.cny_minor, "CNY"),
        "totals": summary
            .by_currency
            .iter()
            .map(|(currency, minor)| money(*minor, currency))
            .collect::<Vec<_>>(),
    })
    .to_string())
}

fn query(args: &Value, context: &PlatformTurnContext) -> Result<String> {
    let Some(sponsor_id) = text_arg(args, "sponsor_id") else {
        bail!("sponsor_id is required with action query")
    };
    let records = context
        .state_store
        .sponsor_records_for(sponsor_id, limit_arg(args))?;
    if records.is_empty() {
        return Ok(json!({
            "ok": true,
            "sponsor_id": sponsor_id,
            "records": [],
            "note": "no sponsorship recorded for this id",
        })
        .to_string());
    }
    let by_currency = context
        .state_store
        .sponsor_totals(SponsorOrder::Amount, MAX_LIMIT)?
        .into_iter()
        .find(|total| total.sponsor_id == sponsor_id);
    Ok(json!({
        "ok": true,
        "sponsor_id": sponsor_id,
        "sponsor_name": records.first().map(|record| record.sponsor_name.clone()).unwrap_or_default(),
        "total_cny": by_currency.as_ref().map(|total| money(total.cny_minor, "CNY")),
        "totals": by_currency
            .as_ref()
            .map(|total| total
                .by_currency
                .iter()
                .map(|(currency, minor)| money(*minor, currency))
                .collect::<Vec<_>>())
            .unwrap_or_default(),
        "records": records.iter().map(record_json).collect::<Vec<_>>(),
    })
    .to_string())
}

async fn add(args: Value, context: Arc<PlatformTurnContext>) -> Result<String> {
    if !context.is_admin {
        return refused("add");
    }
    let Some(sponsor_id) = text_arg(&args, "sponsor_id") else {
        bail!("sponsor_id is required with action add")
    };
    let amount_minor = amount_minor(&args)?;
    let currency = currency_arg(&args)?;
    let note: String = text_arg(&args, "note")
        .unwrap_or_default()
        .chars()
        .take(MAX_NOTE_CHARS)
        .collect();
    let (cny_minor, fx_rate, fx_source) = if currency == "CNY" {
        (amount_minor, 0.0, String::new())
    } else {
        match super::sponsor_fx::usd_to_cny(&context.config).await {
            Some((rate, source)) => (
                ((amount_minor as f64) * rate).round() as i64,
                rate,
                source.to_string(),
            ),
            None => (0, 0.0, "unconverted".to_string()),
        }
    };
    let sponsor_name = text_arg(&args, "sponsor_name")
        .map(str::to_string)
        .unwrap_or_default();
    let record = context.state_store.add_sponsor_record(&NewSponsorRecord {
        platform: context.conversation.platform.clone(),
        account_id: context.conversation.account_id.clone(),
        sponsor_id: sponsor_id.to_string(),
        sponsor_name,
        amount_minor,
        currency,
        cny_minor,
        fx_rate,
        fx_source,
        note,
        recorded_by: context.sender_id.clone(),
        sponsored_at: String::new(),
    })?;
    Ok(json!({ "ok": true, "recorded": record_json(&record) }).to_string())
}

fn update(args: &Value, context: &PlatformTurnContext) -> Result<String> {
    if !context.is_admin {
        return refused("update");
    }
    let Some(record_id) = args.get("record_id").and_then(Value::as_i64) else {
        bail!("record_id is required with action update")
    };
    let note = text_arg(args, "note");
    let sponsor_name = text_arg(args, "sponsor_name");
    if note.is_none() && sponsor_name.is_none() {
        bail!("give note or sponsor_name; an amount cannot be edited, delete the record and add it again")
    }
    let Some(record) = context
        .state_store
        .update_sponsor_record(record_id, note, sponsor_name)?
    else {
        bail!("no sponsorship record with id {record_id}")
    };
    Ok(json!({ "ok": true, "updated": record_json(&record) }).to_string())
}

fn delete(args: &Value, context: &PlatformTurnContext) -> Result<String> {
    if !context.is_admin {
        return refused("delete");
    }
    let Some(record_id) = args.get("record_id").and_then(Value::as_i64) else {
        bail!("record_id is required with action delete")
    };
    let removed = context.state_store.delete_sponsor_record(record_id)?;
    if !removed {
        bail!("no sponsorship record with id {record_id}")
    }
    Ok(json!({ "ok": true, "deleted": record_id }).to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn amounts_come_in_as_numbers_or_strings() {
        assert_eq!(amount_minor(&json!({ "amount": 30 })).unwrap(), 3000);
        assert_eq!(amount_minor(&json!({ "amount": 9.99 })).unwrap(), 999);
        assert_eq!(amount_minor(&json!({ "amount": "12.50" })).unwrap(), 1250);
        assert_eq!(amount_minor(&json!({ "amount": "¥8" })).unwrap(), 800);
        // 分以下的零头四舍五入,不是截断(0.125 在 f64 里是精确值,取 13)。
        assert_eq!(amount_minor(&json!({ "amount": 0.125 })).unwrap(), 13);
        assert_eq!(amount_minor(&json!({ "amount": 1234.56 })).unwrap(), 123456);
    }

    #[test]
    fn nonsense_amounts_are_refused() {
        for bad in [
            json!({}),
            json!({ "amount": 0 }),
            json!({ "amount": -5 }),
            json!({ "amount": "免费" }),
        ] {
            assert!(amount_minor(&bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn currencies_take_the_shapes_people_actually_type() {
        for (input, expected) in [
            ("cny", "CNY"),
            ("RMB", "CNY"),
            ("元", "CNY"),
            ("usd", "USD"),
            ("美金", "USD"),
        ] {
            assert_eq!(
                currency_arg(&json!({ "currency": input })).unwrap(),
                expected
            );
        }
        assert_eq!(currency_arg(&json!({})).unwrap(), "CNY");
        assert!(currency_arg(&json!({ "currency": "jpy" })).is_err());
    }

    #[test]
    fn money_is_formatted_from_minor_units() {
        assert_eq!(money(3000, "CNY"), "¥30.00");
        assert_eq!(money(999, "USD"), "$9.99");
        assert_eq!(money(5, "CNY"), "¥0.05");
    }

    #[test]
    fn limits_are_capped() {
        assert_eq!(limit_arg(&json!({})), DEFAULT_LIMIT);
        assert_eq!(limit_arg(&json!({ "limit": 3 })), 3);
        assert_eq!(limit_arg(&json!({ "limit": 9999 })), MAX_LIMIT);
        assert_eq!(limit_arg(&json!({ "limit": 0 })), DEFAULT_LIMIT);
    }
}
