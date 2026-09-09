use super::{http_response, ToolRegistry, ToolSpec};
use crate::config::ExchangeRatePluginConfig;
use anyhow::{bail, Result};
use serde_json::{json, Value};

pub fn register(registry: &mut ToolRegistry, config: ExchangeRatePluginConfig) {
    registry.register(ToolSpec::new(
        "get_exchange_rate",
        "Query exchange rate between two currencies. Supports ISO codes such as USD/EUR/JPY and common Chinese names.",
        json!({
            "type": "object",
            "properties": {
                "base": { "type": "string", "description": "Base currency, e.g. USD or 美元." },
                "target": { "type": "string", "description": "Target currency, e.g. JPY or 日元." }
            },
            "required": ["base", "target"],
            "additionalProperties": false
        }),
        move |args| {
            let config = config.clone();
            async move { get_exchange_rate(args, config).await }
        },
    ));
}

async fn get_exchange_rate(args: Value, config: ExchangeRatePluginConfig) -> Result<String> {
    let base = currency_code(args.get("base").and_then(Value::as_str).unwrap_or_default());
    let target = currency_code(
        args.get("target")
            .and_then(Value::as_str)
            .unwrap_or_default(),
    );
    if base.is_empty() || target.is_empty() {
        bail!("base and target are required");
    }
    let (rate, _source) = fetch_rate(&base, &target, &config).await?;
    Ok(format!("{base} 到 {target} 的汇率是: {rate}"))
}

/// 取一对币种的汇率，返回汇率与数据源标识。
///
/// 从工具处理函数里抽出来，因为账本记外币账时也要它——那条路上需要的是
/// 数字而不是给模型看的句子，而且**换算必须在 Rust 里做**：让模型自己乘
/// 汇率是在给一个概率模型派确定性工作。
///
/// 两个源的取舍不变：配了 key 就先问付费源，它的任何失败（网络 / 401 /
/// 解析）都不终止流程，掉到免费源兜底。
pub(crate) async fn fetch_rate(
    base: &str,
    target: &str,
    config: &ExchangeRatePluginConfig,
) -> Result<(f64, &'static str)> {
    if !config.api_key.trim().is_empty() {
        let url = format!(
            "https://v6.exchangerate-api.com/v6/{}/latest/{base}",
            config.api_key.trim()
        );
        let data = async {
            http_response::shared_client()
                .get(url)
                .send()
                .await?
                .error_for_status()?
                .json::<Value>()
                .await
        }
        .await;
        if let Ok(data) = data {
            if data.get("result").and_then(Value::as_str) == Some("success") {
                if let Some(rate) = data
                    .get("conversion_rates")
                    .and_then(|rates| rates.get(target))
                    .and_then(Value::as_f64)
                {
                    return Ok((rate, "exchangerate-api"));
                }
            }
        }
    }
    if !config.free_fallback_enabled {
        bail!("exchange rate API key failed or missing and free fallback is disabled");
    }
    let url = format!("https://open.er-api.com/v6/latest/{base}");
    let data: Value = http_response::shared_client()
        .get(url)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let rate = data
        .get("rates")
        .and_then(|rates| rates.get(target))
        .and_then(Value::as_f64)
        .ok_or_else(|| anyhow::anyhow!("target currency not found: {target}"))?;
    Ok((rate, "er-api"))
}

fn currency_code(value: &str) -> String {
    match value.trim().to_uppercase().as_str() {
        "美元" | "美金" => "USD".to_string(),
        "人民币" | "元" => "CNY".to_string(),
        "日元" => "JPY".to_string(),
        "欧元" => "EUR".to_string(),
        "英镑" => "GBP".to_string(),
        "港币" => "HKD".to_string(),
        "台币" | "新台币" => "TWD".to_string(),
        "韩元" => "KRW".to_string(),
        code => code.to_string(),
    }
}
