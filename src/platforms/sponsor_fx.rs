//! 赞助记账的美元汇率。
//!
//! 只在**记一笔美元赞助的那一刻**用一次，拿到的值和来源一起冻进那行记录里。
//! 榜单之后再也不碰汇率——否则同一段历史今天一个名次、明天一个名次。
//!
//! 拉不到就拉不到：进程内还留着上一次的值就用那个（标 `cached`），一次都没
//! 拿到过就返回 `None`，调用方照样把这笔记下来、只是不计入人民币榜。这里**没有**
//! 兜底常量——账本里宁可缺一个数，也不能有一个编出来的数。

use crate::config::AppConfig;
use serde_json::Value;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

/// 缓存有效期。汇率日内波动对一笔赞助的名次没有意义，一小时足够新。
const CACHE_TTL: Duration = Duration::from_secs(60 * 60);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(6);

static CACHE: LazyLock<Mutex<Option<(f64, Instant)>>> = LazyLock::new(|| Mutex::new(None));

fn cached() -> Option<(f64, bool)> {
    let cache = CACHE.lock().ok()?;
    let (rate, at) = (*cache)?;
    Some((rate, at.elapsed() < CACHE_TTL))
}

fn remember(rate: f64) {
    if let Ok(mut cache) = CACHE.lock() {
        *cache = Some((rate, Instant::now()));
    }
}

fn plausible(rate: f64) -> bool {
    // 美元兑人民币历史上没出过这个区间。真出了，说明拿到的不是汇率。
    rate.is_finite() && (1.0..=30.0).contains(&rate)
}

async fn fetch(config: &AppConfig) -> Option<f64> {
    let client = reqwest::Client::builder()
        .timeout(REQUEST_TIMEOUT)
        .build()
        .ok()?;
    let key = config.plugins.exchange_rate.api_key.trim().to_string();
    if !key.is_empty() {
        let url = format!("https://v6.exchangerate-api.com/v6/{key}/latest/USD");
        if let Ok(data) = client
            .get(url)
            .send()
            .await
            .and_then(|r| r.error_for_status())
        {
            if let Ok(data) = data.json::<Value>().await {
                if let Some(rate) = data
                    .pointer("/conversion_rates/CNY")
                    .and_then(Value::as_f64)
                    .filter(|rate| plausible(*rate))
                {
                    return Some(rate);
                }
            }
        }
    }
    let data = client
        .get("https://open.er-api.com/v6/latest/USD")
        .send()
        .await
        .ok()?
        .error_for_status()
        .ok()?
        .json::<Value>()
        .await
        .ok()?;
    data.pointer("/rates/CNY")
        .and_then(Value::as_f64)
        .filter(|rate| plausible(*rate))
}

/// 返回 `(汇率, 来源)`。来源是 `live` 或 `cached`；一个都没有就是 `None`。
pub(crate) async fn usd_to_cny(config: &AppConfig) -> Option<(f64, &'static str)> {
    if let Some((rate, fresh)) = cached() {
        if fresh {
            return Some((rate, "live"));
        }
    }
    if let Some(rate) = fetch(config).await {
        remember(rate);
        return Some((rate, "live"));
    }
    // 过期的旧值也比没有强：它标成 cached，行里看得出来。
    cached().map(|(rate, _)| (rate, "cached"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn implausible_rates_are_rejected() {
        // 拿到 1.0 或者 0 多半是把「USD→USD」或者一个错字段当成了汇率。
        for bad in [0.0, 0.5, 40.0, f64::NAN, f64::INFINITY] {
            assert!(!plausible(bad), "{bad}");
        }
        assert!(plausible(7.12));
    }

    #[test]
    fn a_stale_cache_still_answers_but_says_so() {
        *CACHE.lock().unwrap() = Some((7.2, Instant::now() - CACHE_TTL * 2));
        let (rate, fresh) = cached().unwrap();
        assert_eq!(rate, 7.2);
        assert!(!fresh);
        *CACHE.lock().unwrap() = None;
    }
}
