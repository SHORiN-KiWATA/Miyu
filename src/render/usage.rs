//! token 用量的计量与显示。
//!
//! 缓存命中率（`cache_percent`）是这里最该显眼的数字——命中的 token 只按十分
//! 之一计价，掉下来就是账单翻十倍，而功能上一点症状都没有。

use crate::render::*;

/// Everything the token meters show. Grouped into one struct because the two
/// cache rates each need a numerator *and* a denominator, and threading eight
/// loose `u64`s through four call layers was already past readable.
#[derive(Clone, Copy, Debug, Default)]
pub struct TokenMeter {
    pub turn_tokens: u64,
    /// Denominator of the turn cache rate. A cache hit is an input-side
    /// property — output tokens only enter the prompt on the *next* turn — so
    /// the rate is read/prompt, never read/total, which is what every provider
    /// reports too (DeepSeek splits the prompt into hit+miss; OpenAI's
    /// `cached_tokens` is a subset of `prompt_tokens`; Anthropic names all
    /// three fields `*_input_tokens`).
    pub turn_prompt_tokens: u64,
    pub turn_cached_tokens: u64,
    pub session_tokens: u64,
    pub context_window: Option<usize>,
    /// `context_window` 是不是猜的（配置里的通用兜底常数，跟具体模型无关）。
    /// 猜的时候只显示带 `~` 的数、不出百分比——同 `cache_percent` 的规矩：
    /// 没有真实依据的比率不能渲染成一个看起来很确定的数字。
    pub context_window_assumed: bool,
    /// Σ: session-lifetime total. `None` hides it on narrow terminals.
    pub cumulative_tokens: Option<u64>,
    pub cumulative_prompt_tokens: u64,
    pub cumulative_cached_tokens: u64,
    /// 输出速度(最近一个回合的样本,见 `Usage::generation_ms`)。两者任一
    /// 为零就不显示——没测到的速度不能渲染成 0 tok/s。
    pub generation_tokens: u64,
    pub generation_ms: u64,
}

impl TokenMeter {
    pub fn generation_speed(&self) -> GenerationSpeed {
        GenerationSpeed {
            tokens: self.generation_tokens,
            millis: self.generation_ms,
        }
    }

    pub fn with_generation_speed(self, speed: GenerationSpeed) -> Self {
        Self {
            generation_tokens: speed.tokens,
            generation_ms: speed.millis,
            ..self
        }
    }
}

/// `361 tok/s`;十以下保留一位小数,免得慢模型显示成一串 `0 tok/s`。
pub(crate) fn format_tokens_per_second(speed: GenerationSpeed) -> Option<String> {
    speed.tokens_per_second().map(|rate| {
        if rate >= 10.0 {
            format!("{} tok/s", rate.round() as u64)
        } else {
            format!("{rate:.1} tok/s")
        }
    })
}

/// `None` when there is nothing honest to report: a provider that never said
/// anything about caching must not be rendered as a flat 0%.
///
/// 显示口径(09-11 用户拍板):只有 >99.9 才显示成 100(99.5 这类高命中不该被
/// 抹成满分);99.1–99.9 保留一位小数(临满未满看得见);99.0 及以下取整
/// (99 就是 "99",不写 "99.0")。
pub(crate) fn cache_percent(cached: u64, prompt: u64) -> Option<String> {
    if cached == 0 || prompt == 0 {
        return None;
    }
    let raw = ((cached as f64 / prompt as f64) * 100.0).min(100.0);
    let rounded_one = (raw * 10.0).round() / 10.0;
    Some(if rounded_one >= 100.0 {
        "100".to_string()
    } else if rounded_one > 99.0 {
        format!("{rounded_one:.1}")
    } else {
        format!("{}", raw.round() as u64)
    })
}

pub(crate) fn cache_suffix(cached: u64, prompt: u64) -> String {
    cache_percent(cached, prompt)
        .map(|percent| format!("(C{percent}%)"))
        .unwrap_or_default()
}

pub fn print_token_usage(meter: &TokenMeter, estimated: bool) -> Result<()> {
    let output = token_usage_output(meter, estimated);
    let mut stdout = io::stdout();
    write!(stdout, "{output}")?;
    stdout.flush()?;
    Ok(())
}

pub(crate) fn token_usage_output(meter: &TokenMeter, estimated: bool) -> String {
    let prefix = if estimated {
        t("Estimated ", "估算")
    } else {
        ""
    };
    let line = format!("{prefix}Token: {}", format_token_usage_inline(meter));
    format!("\x1b[2m{line}\x1b[0m\n\n")
}

pub(crate) fn format_token_usage_inline(meter: &TokenMeter) -> String {
    format_token_usage_inline_opts(meter, true, true)
}

pub(crate) fn format_token_usage_inline_opts(
    meter: &TokenMeter,
    show_percent: bool,
    show_speed: bool,
) -> String {
    let context_window = meter.context_window.map(|value| value as u64);
    let context = context_window
        .map(|value| {
            // `~` 是「这个数没有出处」的标记：既没在配置里写死，models.dev 和
            // 供应商的 /models 也都不报，用的是通用兜底常数。数照给——溢出判定
            // 确实会按它办事——但得让人看出来它是估的。
            let assumed = if meter.context_window_assumed {
                "~"
            } else {
                ""
            };
            format!("{assumed}{}", format_compact_count(value))
        })
        .unwrap_or_else(|| "?".to_string());
    // 窗口是猜的时候不出百分比。`47k/168k(28%)` 里那个 28% 看起来是量出来的，
    // 实际分母是编的——用户没法分辨，还可能因此去手动 compact。宁可不给。
    let usage_ratio = context_window
        .filter(|value| *value > 0)
        .filter(|_| !meter.context_window_assumed)
        .map(|context_window| {
            format!(
                "{:.1}%",
                meter.session_tokens as f64 / context_window as f64 * 100.0
            )
        });

    let mut session = match usage_ratio {
        Some(usage_ratio) if show_percent => format!(
            "{}/{}({usage_ratio})",
            format_compact_count(meter.session_tokens),
            context,
        ),
        _ => format!("{}/{}", format_compact_count(meter.session_tokens), context),
    };
    if let Some(cumulative_tokens) = meter.cumulative_tokens {
        session.push_str(&format!(
            " · Σ{}{}",
            format_compact_count(cumulative_tokens),
            cache_suffix(
                meter.cumulative_cached_tokens,
                meter.cumulative_prompt_tokens
            ),
        ));
    }
    // 速度紧跟本轮用量之后、上下文表之前:footer 里没有本轮用量,它就打头。
    let speed = show_speed
        .then(|| format_tokens_per_second(meter.generation_speed()))
        .flatten();
    if let Some(speed) = speed {
        session = format!("{speed} · {session}");
    }
    if meter.turn_tokens == 0 {
        session
    } else {
        format!(
            "{}{} · {session}",
            format_compact_count(meter.turn_tokens),
            cache_suffix(meter.turn_cached_tokens, meter.turn_prompt_tokens),
        )
    }
}

pub fn usage_total(usage: &Usage) -> u64 {
    usage.effective_total_tokens()
}

pub(crate) fn format_compact_count(value: u64) -> String {
    const K: f64 = 1_000.0;
    const M: f64 = 1_000_000.0;
    if value >= 1_000_000 {
        format_compact_unit(value as f64 / M, "M")
    } else if value >= 1_000 {
        format_compact_unit(value as f64 / K, "k")
    } else {
        value.to_string()
    }
}

pub(crate) fn format_compact_unit(value: f64, suffix: &str) -> String {
    if (value.fract() - 0.0).abs() < f64::EPSILON {
        format!("{value:.0}{suffix}")
    } else {
        format!("{value:.1}{suffix}")
    }
}

#[cfg(test)]
mod cache_percent_tests {
    use super::cache_percent;

    fn pct(cached: u64, prompt: u64) -> Option<String> {
        cache_percent(cached, prompt)
    }

    #[test]
    fn only_above_99_9_shows_100() {
        // 99.5% 不再被抹成满分。
        assert_eq!(pct(995, 1000).as_deref(), Some("99.5"));
        // 99.94% 一位小数四舍五入到 99.9,仍是小数。
        assert_eq!(pct(9994, 10000).as_deref(), Some("99.9"));
        // 99.96% → 100。
        assert_eq!(pct(9996, 10000).as_deref(), Some("100"));
        assert_eq!(pct(1000, 1000).as_deref(), Some("100"));
    }

    #[test]
    fn ninety_nine_band_shows_one_decimal_from_99_1() {
        // 恰好 99.0 取整为 "99",不写 "99.0"。
        assert_eq!(pct(990, 1000).as_deref(), Some("99"));
        // 99.04 一位小数舍到 99.0 → 取整 "99"。
        assert_eq!(pct(9904, 10000).as_deref(), Some("99"));
        // 99.1 起显示一位小数。
        assert_eq!(pct(9910, 10000).as_deref(), Some("99.1"));
    }

    #[test]
    fn below_99_is_integer() {
        assert_eq!(pct(880, 1000).as_deref(), Some("88"));
        assert_eq!(pct(1, 2).as_deref(), Some("50"));
    }

    #[test]
    fn nothing_to_report_is_none() {
        assert_eq!(pct(0, 1000), None);
        assert_eq!(pct(500, 0), None);
    }
}
