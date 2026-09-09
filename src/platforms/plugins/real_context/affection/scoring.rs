//! 好感度的分值计算。
//!
//! 分值不是线性的：`smoothstep` 让接近上限时增长变慢，`gain_multiplier` 让低
//! 分区涨得快。目的是「一开始容易建立、后面难以刷满」。
//!
//! `max_score_for_user` 按身份给上限——陌生人刷不到亲密档。

use crate::platforms::plugins::real_context::affection::*;

#[derive(Clone, Copy)]
pub(crate) struct AffectionLevel<'a> {
    pub(crate) name: &'static str,
    pub(crate) prompt: &'a str,
}

/// 档位的规范名是英文 slug——模型面、判官、面板键都用它。
///
/// 中文档位名（「中立」「好友」）是可以直接念出口的成品词：她拿到工具结果
/// 就照着播报「当前好感度是中立」。08-29 那次「没被艾特不接」是同一族——
/// 一个现成的中文判据摆在那儿，她就会拿它立规矩。英文 slug 要念出来得先
/// 翻译一道，而翻译天然逼她改写成自己的话。
pub(crate) fn level_display(level: &str) -> &str {
    match level {
        "estranged" => "刻意疏远",
        "cold" => "冷漠",
        "neutral" => "中立",
        "acquainted" => "认识",
        "friend" => "好友",
        "trusted" => "信任",
        "close" => "亲近",
        other => other,
    }
}

pub(crate) fn localized_level<'a>(level: &'a str, locale: Locale) -> &'a str {
    if locale == Locale::Zh {
        level_display(level)
    } else {
        level
    }
}

pub(crate) fn signed_score(value: f64) -> String {
    if value.abs() < 0.0005 {
        "0.000".to_string()
    } else {
        format!("{value:+.3}")
    }
}

pub(crate) fn level_for_score<'a>(
    settings: &'a RealContextPluginSettings,
    score: f64,
    user_id: &str,
) -> AffectionLevel<'a> {
    let score = clamp_score(settings, score, user_id);
    if score < -25.0 {
        AffectionLevel {
            name: "estranged",
            prompt: &settings.affection_prompt_estranged,
        }
    } else if score < 0.0 {
        AffectionLevel {
            name: "cold",
            prompt: &settings.affection_prompt_cold,
        }
    } else if score < 25.0 {
        AffectionLevel {
            name: "neutral",
            prompt: &settings.affection_prompt_neutral,
        }
    } else if score < 60.0 {
        AffectionLevel {
            name: "acquainted",
            prompt: &settings.affection_prompt_known,
        }
    } else if score < 85.0 {
        AffectionLevel {
            name: "friend",
            prompt: &settings.affection_prompt_friend,
        }
    } else if score < 95.0 {
        AffectionLevel {
            name: "trusted",
            prompt: &settings.affection_prompt_trusted,
        }
    } else {
        AffectionLevel {
            name: "close",
            prompt: &settings.affection_prompt_close,
        }
    }
}

pub(crate) fn reply_bias(settings: &RealContextPluginSettings, score: f64, user_id: &str) -> f64 {
    let value = clamp_score(settings, score, user_id);
    let neutral = clamp_score(settings, settings.affection_initial_score, user_id);
    if value >= neutral {
        let maximum = max_score_for_user(settings, user_id);
        let span = (maximum - neutral).max(f64::EPSILON);
        settings.affection_bias_max * smoothstep((value - neutral) / span)
    } else {
        let anchor = settings
            .affection_min_score
            .max(0.0_f64.min(neutral - f64::EPSILON));
        let span = (neutral - anchor).max(f64::EPSILON);
        settings.affection_bias_min * smoothstep((neutral - value) / span)
    }
}

pub(crate) fn gain_multiplier(
    settings: &RealContextPluginSettings,
    score: f64,
    user_id: &str,
) -> f64 {
    let score = clamp_score(settings, score, user_id);
    let maximum = max_score_for_user(settings, user_id);
    let pivot = settings
        .affection_gain_pivot
        .clamp(settings.affection_min_score, maximum);
    if score <= pivot {
        return 1.0;
    }
    let x = ((score - pivot) / (maximum - pivot).max(f64::EPSILON)).clamp(0.0, 1.0);
    (-1.2 * smoothstep(x)).exp().max(0.05)
}

pub(crate) fn max_score_for_user(settings: &RealContextPluginSettings, user_id: &str) -> f64 {
    let unlimited = user_id
        .parse::<i64>()
        .ok()
        .is_some_and(|id| settings.affection_unlimited_user_ids.contains(&id));
    if unlimited {
        settings.affection_max_score
    } else {
        settings
            .affection_regular_max_score
            .min(settings.affection_max_score)
    }
}

pub(crate) fn clamp_score(settings: &RealContextPluginSettings, score: f64, user_id: &str) -> f64 {
    finite(score, settings.affection_initial_score).clamp(
        settings.affection_min_score,
        max_score_for_user(settings, user_id),
    )
}

pub(crate) fn smoothstep(value: f64) -> f64 {
    let value = value.clamp(0.0, 1.0);
    value * value * (3.0 - 2.0 * value)
}
