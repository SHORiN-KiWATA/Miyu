//! 判定过程的日志。
//!
//! 「为什么这条没回 / 为什么插话了」是这个插件最常被问的问题，所以判定的每个
//! 输入都要落进日志。`*_for` 版本把格式参数外置，便于对着固定输出写测试。

use crate::platforms::plugins::real_context::*;

pub(in crate::platforms::plugins::real_context) struct ActiveReplyDecisionLog<'a> {
    pub(in crate::platforms::plugins::real_context) account_id: &'a str,
    pub(in crate::platforms::plugins::real_context) group_id: &'a str,
    pub(in crate::platforms::plugins::real_context) sender_name: &'a str,
    pub(in crate::platforms::plugins::real_context) sender_id: &'a str,
    pub(in crate::platforms::plugins::real_context) mentioned_bot: bool,
    pub(in crate::platforms::plugins::real_context) message: &'a str,
    pub(in crate::platforms::plugins::real_context) trigger: TriggerKind,
    pub(in crate::platforms::plugins::real_context) should_reply: bool,
    pub(in crate::platforms::plugins::real_context) model_should_reply: Option<bool>,
    pub(in crate::platforms::plugins::real_context) raw_score: f64,
    pub(in crate::platforms::plugins::real_context) final_score: f64,
    pub(in crate::platforms::plugins::real_context) threshold: f64,
    pub(in crate::platforms::plugins::real_context) model_adjustment: f64,
    pub(in crate::platforms::plugins::real_context) affection_level: &'a str,
    pub(in crate::platforms::plugins::real_context) affection_adjustment: f64,
    pub(in crate::platforms::plugins::real_context) emotion_adjustment: f64,
    pub(in crate::platforms::plugins::real_context) continuation_adjustment: f64,
    pub(in crate::platforms::plugins::real_context) system_adjustment: f64,
    pub(in crate::platforms::plugins::real_context) reply_heat: f64,
    pub(in crate::platforms::plugins::real_context) heat_penalty: f64,
    pub(in crate::platforms::plugins::real_context) heat_threshold_adjustment: f64,
    pub(in crate::platforms::plugins::real_context) short_message_threshold_adjustment: f64,
    pub(in crate::platforms::plugins::real_context) moderation: &'a judge::ModerationResult,
    pub(in crate::platforms::plugins::real_context) reason: &'a str,
    pub(in crate::platforms::plugins::real_context) endpoint: Option<&'a str>,
}

pub(in crate::platforms::plugins::real_context) fn format_active_reply_decision_log(
    log: &ActiveReplyDecisionLog<'_>,
) -> String {
    format_active_reply_decision_log_for(log, crate::i18n::locale())
}

pub(in crate::platforms::plugins::real_context) fn format_active_reply_decision_log_for(
    log: &ActiveReplyDecisionLog<'_>,
    locale: Locale,
) -> String {
    let model_result = match log.model_should_reply {
        Some(true) => text_for(locale, "should reply", "应该回复"),
        Some(false) => text_for(locale, "should not reply", "不应该回复"),
        None => text_for(locale, "not returned by model", "模型未返回"),
    };
    let mut lines = vec![
        log.trigger.decision_log_title(log.should_reply, locale),
        if locale == Locale::Zh {
            format!(
                "会话：群聊 {}（机器人 QQ {}）",
                log.group_id, log.account_id
            )
        } else {
            format!(
                "Conversation: group {} (bot QQ {})",
                log.group_id, log.account_id
            )
        },
        if locale == Locale::Zh {
            format!(
                "发送者：{}（QQ {}）",
                empty_as(log.sender_name, "未知用户"),
                log.sender_id
            )
        } else {
            format!(
                "Sender: {} (QQ {})",
                empty_as(log.sender_name, "unknown user"),
                log.sender_id
            )
        },
        format_decision_log_field(
            locale,
            text_for(locale, "Mentioned bot", "@机器人"),
            if log.mentioned_bot {
                text_for(locale, "yes", "是")
            } else {
                text_for(locale, "no", "否")
            },
        ),
        format_decision_log_field(
            locale,
            text_for(locale, "Message", "消息"),
            &format_log_message(log.message, locale),
        ),
        format_decision_log_field(
            locale,
            text_for(locale, "Trigger", "触发"),
            log.trigger.log_label(locale),
        ),
        format_decision_log_field(
            locale,
            text_for(locale, "Model", "模型"),
            log.endpoint.unwrap_or("unknown"),
        ),
        format_decision_log_field(
            locale,
            text_for(locale, "Result", "结果"),
            if log.should_reply {
                text_for(locale, "reply", "回复")
            } else {
                text_for(locale, "no reply", "不回复")
            },
        ),
        if locale == Locale::Zh {
            format!(
                "分数：{:.3}（原始 {:.3}，阈值 {:.3}）",
                log.final_score, log.raw_score, log.threshold
            )
        } else {
            format!(
                "Score: {:.3} (raw {:.3}, threshold {:.3})",
                log.final_score, log.raw_score, log.threshold
            )
        },
        format_decision_log_field(
            locale,
            text_for(locale, "Reply tendency adjustment", "回复倾向调整"),
            &format!(
                "{} {}",
                model_result,
                format_adjustment(log.model_adjustment)
            ),
        ),
        format_decision_log_field(
            locale,
            text_for(locale, "Affection adjustment", "好感度调整"),
            &format!(
                "{} {}",
                localized_affection_level(empty_as(log.affection_level, "中立"), locale),
                format_adjustment(log.affection_adjustment)
            ),
        ),
    ];
    if log.emotion_adjustment.abs() >= 0.0005 {
        lines.push(format_decision_log_field(
            locale,
            text_for(locale, "Emotion threshold adjustment", "情绪阈值调整"),
            &format_adjustment(log.emotion_adjustment),
        ));
    }
    if log.continuation_adjustment.abs() >= 0.0005 {
        lines.push(format_decision_log_field(
            locale,
            text_for(locale, "Continuation adjustment", "自然续聊调整"),
            &format_adjustment(log.continuation_adjustment),
        ));
    }
    if log.system_adjustment.abs() >= 0.0005 {
        lines.push(format_decision_log_field(
            locale,
            text_for(locale, "Direct trigger adjustment", "直接触发调整"),
            &format_adjustment(log.system_adjustment),
        ));
    }
    if log.heat_penalty.abs() >= 0.0005 || log.heat_threshold_adjustment.abs() >= 0.0005 {
        lines.push(if locale == Locale::Zh {
            format!(
                "冷静机制调整：扣分 {}，阈值 {}（冷静度 {:.3}）",
                format_adjustment(-log.heat_penalty),
                format_adjustment(log.heat_threshold_adjustment),
                log.reply_heat
            )
        } else {
            format!(
                "Restraint adjustment: penalty {}, threshold {} (heat {:.3})",
                format_adjustment(-log.heat_penalty),
                format_adjustment(log.heat_threshold_adjustment),
                log.reply_heat
            )
        });
    }
    if log.short_message_threshold_adjustment.abs() >= 0.0005 {
        lines.push(format_decision_log_field(
            locale,
            text_for(locale, "Short-message threshold adjustment", "短句阈值调整"),
            &format_adjustment(log.short_message_threshold_adjustment),
        ));
    }
    if log.moderation.violation {
        let category = if log.moderation.category.trim().is_empty() {
            String::new()
        } else {
            format!(" {}", log.moderation.category.trim())
        };
        lines.push(if locale == Locale::Zh {
            format!(
                "安全初判：命中{}（严重度 {:.1}/10）",
                category, log.moderation.severity
            )
        } else {
            format!(
                "Moderation precheck: violation{} (severity {:.1}/10)",
                category, log.moderation.severity
            )
        });
    }
    lines.push(format_decision_log_field(
        locale,
        text_for(locale, "Reason", "判断理由"),
        empty_as(
            log.reason,
            text_for(locale, "not provided by model", "模型未提供"),
        ),
    ));
    lines.join("\n")
}

pub(in crate::platforms::plugins::real_context) fn format_decision_log_field(
    locale: Locale,
    label: &str,
    value: &str,
) -> String {
    if locale == Locale::Zh {
        format!("{label}：{value}")
    } else {
        format!("{label}: {value}")
    }
}

pub(in crate::platforms::plugins::real_context) fn format_log_message(
    message: &str,
    locale: Locale,
) -> String {
    let compact = message.split_whitespace().collect::<Vec<_>>().join(" ");
    if compact.is_empty() {
        return text_for(locale, "[non-text message]", "[非文本消息]").to_string();
    }
    const MAX_CHARS: usize = 300;
    let mut chars = compact.chars();
    let shortened = chars.by_ref().take(MAX_CHARS).collect::<String>();
    if chars.next().is_some() {
        format!("{shortened}...")
    } else {
        shortened
    }
}

pub(in crate::platforms::plugins::real_context) fn format_active_reply_skip_log(
    account_id: &str,
    group_id: &str,
    sender_name: &str,
    sender_id: &str,
    trigger: TriggerKind,
    reason: &str,
) -> String {
    format_active_reply_skip_log_for(
        account_id,
        group_id,
        sender_name,
        sender_id,
        trigger,
        reason,
        crate::i18n::locale(),
    )
}

pub(in crate::platforms::plugins::real_context) fn format_active_reply_skip_log_for(
    account_id: &str,
    group_id: &str,
    sender_name: &str,
    sender_id: &str,
    trigger: TriggerKind,
    reason: &str,
    locale: Locale,
) -> String {
    if locale == Locale::Zh {
        format!(
            "（跳过主动判断）\n会话：群聊 {group_id}（机器人 QQ {account_id}）\n发送者：{}（QQ {sender_id}）\n触发：{}\n结果：跳过\n判断原因：{}",
            empty_as(sender_name, "未知用户"),
            trigger.log_label(locale),
            empty_as(reason, "未提供"),
        )
    } else {
        format!(
            "[Active reply decision skipped]\nConversation: group {group_id} (bot QQ {account_id})\nSender: {} (QQ {sender_id})\nTrigger: {}\nResult: skipped\nReason: {}",
            empty_as(sender_name, "unknown user"),
            trigger.log_label(locale),
            empty_as(reason, "not provided"),
        )
    }
}

/// 未经判官直接回复的留痕。这三条路径(限额耗尽、本会话不做主动判断、
/// 覆盖窗口沿用已承诺回复)此前一行日志都不打,取证时是黑洞:08-29 排查
/// 两次「元层拒答」,只能靠"回复出现了但没有判官决定"做减法,推错过一次。
pub(in crate::platforms::plugins::real_context) fn format_active_reply_bypass_log(
    account_id: &str,
    group_id: &str,
    sender_name: &str,
    sender_id: &str,
    trigger: TriggerKind,
    reason: &str,
) -> String {
    format_active_reply_bypass_log_for(
        account_id,
        group_id,
        sender_name,
        sender_id,
        trigger,
        reason,
        crate::i18n::locale(),
    )
}

pub(in crate::platforms::plugins::real_context) fn format_active_reply_bypass_log_for(
    account_id: &str,
    group_id: &str,
    sender_name: &str,
    sender_id: &str,
    trigger: TriggerKind,
    reason: &str,
    locale: Locale,
) -> String {
    if locale == Locale::Zh {
        format!(
            "（未经判断直接回复）\n会话：群聊 {group_id}（机器人 QQ {account_id}）\n发送者：{}（QQ {sender_id}）\n触发：{}\n结果：回复\n跳过判断的原因：{}",
            empty_as(sender_name, "未知用户"),
            trigger.log_label(locale),
            empty_as(reason, "未提供"),
        )
    } else {
        format!(
            "[Replied without an active-reply judgement]\nConversation: group {group_id} (bot QQ {account_id})\nSender: {} (QQ {sender_id})\nTrigger: {}\nResult: reply\nJudgement skipped because: {}",
            empty_as(sender_name, "unknown user"),
            trigger.log_label(locale),
            empty_as(reason, "not provided"),
        )
    }
}

pub(in crate::platforms::plugins::real_context) fn localized_affection_level<'a>(
    level: &'a str,
    locale: Locale,
) -> &'a str {
    if locale == Locale::Zh {
        return level;
    }
    match level {
        "刻意疏远" => "estranged",
        "冷漠" => "cold",
        "中立" => "neutral",
        "认识" => "acquainted",
        "好友" => "friend",
        "信任" => "trusted",
        "亲近" => "close",
        _ => level,
    }
}

pub(in crate::platforms::plugins::real_context) fn model_reply_adjustment(
    settings: &RealContextPluginSettings,
    model_should_reply: Option<bool>,
) -> f64 {
    if !settings.judge_should_reply_adjust_enable {
        return 0.0;
    }
    match model_should_reply {
        Some(true) => settings.judge_should_reply_boost_score,
        Some(false) => -settings.judge_should_reply_penalty_score,
        None => 0.0,
    }
}

pub(in crate::platforms::plugins::real_context) fn format_adjustment(value: f64) -> String {
    if value.abs() < 0.0005 {
        "0.000".to_string()
    } else {
        format!("{value:+.3}")
    }
}
