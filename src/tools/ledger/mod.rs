//! 记账工具。
//!
//! 两件工具一组：`ledger` 管流水（记、查、改、删、汇总），`manage_ledger`
//! 管账本自身的配置（账本、账户、分类、预算）。分法照 `use_meme` /
//! `manage_meme`：高频的那件单独露脸，低频的那件并进工具组。
//!
//! **注册位置就是权限边界**：这两件工具只进 `builtin_registry` 与
//! `dev_registry`（终端 / WebUI / daemon 直连），不进
//! `restricted_platform_registry`。QQ 群里的模型上下文里连工具名都不存在——
//! 比运行时判断身份更彻底，也不白占 token。

mod add;
mod manage;
mod query;

#[cfg(test)]
mod tests;

use super::{ToolRegistry, ToolSpec};
use crate::config::AppConfig;
use crate::ledger::money::format_amount;
use crate::ledger::types::*;
use crate::ledger::LedgerDb;
use crate::paths::MiyuPaths;
use anyhow::{bail, Result};
use serde_json::{json, Value};

pub fn register(registry: &mut ToolRegistry, config: AppConfig, paths: MiyuPaths) {
    // 描述与 schema 的真相源是 descriptions/ledger.json，这里只是占位。
    let placeholder = json!({
        "type": "object",
        "properties": { "action": { "type": "string" } },
        "required": ["action"],
        "additionalProperties": false
    });

    let entry_paths = paths.clone();
    let entry_config = config.clone();
    registry.register(
        ToolSpec::new(
            "ledger",
            "Record and query personal income and expenses.",
            placeholder.clone(),
            move |args| {
                let paths = entry_paths.clone();
                let config = entry_config.clone();
                async move { run_ledger(args, paths, config).await }
            },
        )
        .writes(),
    );

    registry.register(
        ToolSpec::new(
            "manage_ledger",
            "Manage ledger books, accounts, categories and budgets.",
            placeholder,
            move |args| {
                let paths = paths.clone();
                let config = config.clone();
                async move { manage::run(args, paths, config).await }
            },
        )
        .writes(),
    );
}

async fn run_ledger(args: Value, paths: MiyuPaths, config: AppConfig) -> Result<String> {
    match args
        .get("action")
        .and_then(Value::as_str)
        .unwrap_or_default()
    {
        "add" => add::run(args, paths, config).await,
        "list" => query::list(args, paths, config).await,
        "summary" => query::summary(args, paths, config).await,
        "update" => query::update(args, paths, config).await,
        "delete" => query::delete(args, paths, config).await,
        other => bail!("unknown action: {other}; expected add, list, summary, update or delete"),
    }
}

// ── 共享零件 ────────────────────────────────────────────────

/// 打开账本库。每次调用现开连接：账本操作低频，连接池是纯粹的复杂度。
pub(super) fn open_db(config: &AppConfig, paths: &MiyuPaths) -> Result<LedgerDb> {
    LedgerDb::open_for(config, paths)
}

/// 取字符串参数，去空白后为空视作没给。
pub(super) fn opt_str<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

pub(super) fn required_str<'a>(args: &'a Value, key: &str) -> Result<&'a str> {
    opt_str(args, key).ok_or_else(|| anyhow::anyhow!("{key} is required"))
}

pub(super) fn opt_bool(args: &Value, key: &str) -> bool {
    args.get(key).and_then(Value::as_bool).unwrap_or(false)
}

/// 解析账本。没指定且只有一本时用那一本；多本而没指定就报错列出候选——
/// 记错账本是几个月后才会发现的错误，不值得为省一次追问去猜。
pub(super) fn resolve_book(db: &LedgerDb, args: &Value) -> Result<BookRecord> {
    db.resolve_book(opt_str(args, "book"))
}

/// 一笔账在结果里的样子。字段刻意少：模型要的是「记成什么样了」，
/// 不是整行数据。
pub(super) fn entry_json(db: &LedgerDb, entry: &EntryRecord) -> Result<Value> {
    let mut value = json!({
        "id": entry.entry_id,
        "date": entry.occurred_day,
        "kind": entry.kind,
        "amount": format!("{} {}", format_amount(entry.amount_minor, &entry.currency), entry.currency),
    });
    let object = value.as_object_mut().expect("object literal");
    if let Some(category_id) = &entry.category_id {
        let name = db.category_display_name(category_id)?;
        if !name.is_empty() {
            object.insert("category".to_string(), json!(name));
        }
    }
    if !entry.note.is_empty() {
        object.insert("note".to_string(), json!(entry.note));
    }
    if !entry.merchant.is_empty() {
        object.insert("merchant".to_string(), json!(entry.merchant));
    }
    // 换算信息只在真的换算过时才出现，同币种的账不必背这几个字段。
    match entry.rate_status {
        RateStatus::Ok => {
            if let Some(base_minor) = entry.base_amount_minor {
                object.insert(
                    "converted".to_string(),
                    json!(format!(
                        "{} {}",
                        format_amount(base_minor, &entry.base_currency),
                        entry.base_currency
                    )),
                );
            }
            if let Some(rate) = &entry.rate {
                object.insert("rate".to_string(), json!(rate));
            }
        }
        RateStatus::Pending => {
            object.insert("rate_status".to_string(), json!("pending"));
            object.insert(
                "note_on_rate".to_string(),
                json!("exchange rate unavailable; this entry is not in the totals yet"),
            );
        }
        RateStatus::Same => {}
    }
    Ok(value)
}

/// 预算执行情况在结果里的样子。只在 near / exceeded 时构造。
pub(super) fn budget_json(status: &BudgetStatus) -> Value {
    let over = status.used_minor - status.limit_minor;
    let mut value = json!({
        "scope": status.scope,
        "period": status.period,
        "limit": format_amount(status.limit_minor, &status.currency),
        "used": format_amount(status.used_minor, &status.currency),
        "currency": status.currency,
        "state": status.state,
    });
    if over > 0 {
        value.as_object_mut().unwrap().insert(
            "over".to_string(),
            json!(format_amount(over, &status.currency)),
        );
    }
    value
}

/// 账户在结果里的样子。
pub(super) fn account_json(db: &LedgerDb, account: &AccountRecord) -> Result<Value> {
    let balance = db.account_balance(account)?;
    let mut value = json!({
        "id": account.account_id,
        "name": account.name,
        "kind": account.kind,
        "currency": account.currency,
        "balance": format_amount(balance.minor, &account.currency),
    });
    // 余额少了一截总得说一声，不然模型会拿着一个偏低的数去跟用户对账。
    if balance.unconverted_count > 0 {
        value.as_object_mut().unwrap().insert(
            "balance_note".to_string(),
            json!(format!(
                "{} entries are in another currency with no rate yet and are not in this balance",
                balance.unconverted_count
            )),
        );
    }
    Ok(value)
}

/// 把 `YYYY-MM-DD` 解析成（发生时刻, 本地自然日）两列。
///
/// 补记往日的账时刻不可知，统一落在那天本地时间正午——这样 `occurred_at`
/// 转回本地自然日必然还是同一天，两列永远自洽，不会因为时区把账挪到
/// 隔壁那天。`add` 与 `update` 必须用同一套口径，所以收口在这里。
pub(super) fn day_to_timestamps(date: &str) -> Result<(String, String)> {
    use chrono::{Local, NaiveDate, TimeZone};
    let parsed = NaiveDate::parse_from_str(date, "%Y-%m-%d")
        .map_err(|_| anyhow::anyhow!("date must look like 2026-09-08, got {date:?}"))?;
    let noon = parsed
        .and_hms_opt(12, 0, 0)
        .ok_or_else(|| anyhow::anyhow!("invalid date {date:?}"))?;
    let local = Local
        .from_local_datetime(&noon)
        .single()
        .ok_or_else(|| anyhow::anyhow!("ambiguous local time for {date:?}"))?;
    let occurred_at = local.to_utc().to_rfc3339();
    let occurred_day = crate::ledger::local_day_of(&occurred_at)?;
    Ok((occurred_at, occurred_day))
}
