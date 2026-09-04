//! 好感度变化的日志。
//!
//! 分值由模型给，所以每次变化都要能复盘：改了多少、依据什么、被哪条规则截断。
//! `format_*_log` 一族把这些拼成一行，`*_for` 版本便于对着固定输出写测试。

use crate::platforms::plugins::real_context::affection::*;

pub(in crate::platforms::plugins::real_context::affection) fn log_profile_initialized(
    context: &PlatformTurnContext,
    settings: &RealContextPluginSettings,
    profile: &AffectionProfile,
) {
    let level = level_for_score(settings, profile.score, &profile.user_id);
    let readable = format_affection_initialized_log(
        &context.conversation.account_id,
        &context.conversation.conversation_id,
        &profile.sender_name,
        &profile.user_id,
        &context.config.active_persona_scope(),
        profile.score,
        level.name,
        crate::i18n::locale(),
    );
    tracing::info!(
        target: "miyu::qq",
        "\n{readable}"
    );
}

pub(crate) fn format_affection_initialized_log(
    account_id: &str,
    group_id: &str,
    sender_name: &str,
    sender_id: &str,
    persona: &str,
    score: f64,
    level: &str,
    locale: Locale,
) -> String {
    if locale == Locale::Zh {
        format!(
            "【好感度：初始化】\n会话：群聊 {group_id}（机器人 QQ {account_id}）\n用户：{}（QQ {sender_id}）\n人格：{}\n初始关系：{}\n初始分数：{score:.3}",
            empty_log_value(sender_name, "未知用户"),
            empty_log_value(persona, "default"),
            localized_level(level, locale),
        )
    } else {
        format!(
            "[Affection: initialized]\nConversation: group {group_id} (bot QQ {account_id})\nUser: {} (QQ {sender_id})\nPersona: {}\nInitial relationship: {}\nInitial score: {score:.3}",
            empty_log_value(sender_name, "unknown user"),
            empty_log_value(persona, "default"),
            localized_level(level, locale),
        )
    }
}

pub(super) fn log_update_skipped(
    job: &AffectionUpdateJob,
    reason: &str,
    confidence: Option<f64>,
    threshold: Option<f64>,
) {
    let readable = format_affection_skipped_log(
        job.group.account_id(),
        job.group.group_id(),
        &job.sender_name,
        &job.sender_id,
        reason,
        confidence,
        threshold,
        crate::i18n::locale(),
    );
    tracing::debug!(
        target: "miyu::qq",
        "\n{readable}"
    );
}

pub(in crate::platforms::plugins::real_context::affection) fn format_affection_update_log(
    job: &AffectionUpdateJob,
    outcome: &AffectionUpdateOutcome,
    locale: Locale,
) -> String {
    let changed = outcome.actual_delta.abs() > 0.0001
        || !outcome.tags_added.is_empty()
        || !outcome.tags_removed.is_empty();
    let before = level_for_score(&job.settings, outcome.score_before, &job.sender_id).name;
    let after = level_for_score(&job.settings, outcome.score_after, &job.sender_id).name;
    let mut lines = if locale == Locale::Zh {
        vec![
            if changed {
                "【好感度：发生变化】".to_string()
            } else {
                "【好感度：无变化】".to_string()
            },
            format!(
                "会话：群聊 {}（机器人 QQ {}）",
                job.group.group_id(),
                job.group.account_id()
            ),
            format!(
                "用户：{}（QQ {}）",
                empty_log_value(&job.sender_name, "未知用户"),
                job.sender_id
            ),
            format!(
                "关系：{} → {}",
                localized_level(before, locale),
                localized_level(after, locale)
            ),
            format!(
                "分数：{:.3} → {:.3}（实际变化 {}）",
                outcome.score_before,
                outcome.score_after,
                signed_score(outcome.actual_delta)
            ),
            format!(
                "模型变化：{}（置信度 {:.2}）",
                signed_score(outcome.raw_delta),
                outcome.confidence
            ),
            format!("原因：{}", empty_log_value(&outcome.reason, "模型未提供")),
        ]
    } else {
        vec![
            if changed {
                "[Affection: changed]".to_string()
            } else {
                "[Affection: unchanged]".to_string()
            },
            format!(
                "Conversation: group {} (bot QQ {})",
                job.group.group_id(),
                job.group.account_id()
            ),
            format!(
                "User: {} (QQ {})",
                empty_log_value(&job.sender_name, "unknown user"),
                job.sender_id
            ),
            format!(
                "Relationship: {} -> {}",
                localized_level(before, locale),
                localized_level(after, locale)
            ),
            format!(
                "Score: {:.3} -> {:.3} (actual change {})",
                outcome.score_before,
                outcome.score_after,
                signed_score(outcome.actual_delta)
            ),
            format!(
                "Model delta: {} (confidence {:.2})",
                signed_score(outcome.raw_delta),
                outcome.confidence
            ),
            format!(
                "Reason: {}",
                empty_log_value(&outcome.reason, "not provided by model")
            ),
        ]
    };
    if !outcome.tags_added.is_empty() {
        lines.push(format!(
            "{}: {}",
            text_for(locale, "Tags added", "新增标签"),
            outcome.tags_added.join(text_for(locale, ", ", "、"))
        ));
    }
    if !outcome.tags_removed.is_empty() {
        lines.push(format!(
            "{}: {}",
            text_for(locale, "Tags removed", "删除标签"),
            outcome.tags_removed.join(text_for(locale, ", ", "、"))
        ));
    }
    lines.join("\n")
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn format_affection_skipped_log(
    account_id: &str,
    group_id: &str,
    sender_name: &str,
    sender_id: &str,
    reason: &str,
    confidence: Option<f64>,
    threshold: Option<f64>,
    locale: Locale,
) -> String {
    let reason = match (locale, reason) {
        (Locale::Zh, "queue_full") => "更新队列已满",
        (Locale::Zh, "queue_closed") => "更新队列已关闭",
        (Locale::Zh, "low_confidence") => "置信度不足",
        (Locale::Zh, "profile_missing") => "关系档案不存在",
        (Locale::Zh, "auto_update_disabled") => "自动更新已关闭",
        (Locale::En, "queue_full") => "update queue is full",
        (Locale::En, "queue_closed") => "update queue is closed",
        (Locale::En, "low_confidence") => "confidence below threshold",
        (Locale::En, "profile_missing") => "relationship profile does not exist",
        (Locale::En, "auto_update_disabled") => "automatic updates are disabled",
        (_, value) => value,
    };
    let mut lines = if locale == Locale::Zh {
        vec![
            "【好感度：评估跳过】".to_string(),
            format!("会话：群聊 {group_id}（机器人 QQ {account_id}）"),
            format!(
                "用户：{}（QQ {sender_id}）",
                empty_log_value(sender_name, "未知用户")
            ),
            format!("原因：{reason}"),
        ]
    } else {
        vec![
            "[Affection: evaluation skipped]".to_string(),
            format!("Conversation: group {group_id} (bot QQ {account_id})"),
            format!(
                "User: {} (QQ {sender_id})",
                empty_log_value(sender_name, "unknown user")
            ),
            format!("Reason: {reason}"),
        ]
    };
    if let Some(confidence) = confidence {
        lines.push(if locale == Locale::Zh {
            format!("置信度：{confidence:.2}")
        } else {
            format!("Confidence: {confidence:.2}")
        });
    }
    if let Some(threshold) = threshold {
        lines.push(if locale == Locale::Zh {
            format!("阈值：{threshold:.2}")
        } else {
            format!("Threshold: {threshold:.2}")
        });
    }
    lines.join("\n")
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn format_affection_failure_log(
    account_id: &str,
    group_id: &str,
    sender_name: &str,
    sender_id: &str,
    stage: &str,
    error: &str,
    locale: Locale,
) -> String {
    if locale == Locale::Zh {
        format!(
            "【好感度：更新失败】\n会话：群聊 {group_id}（机器人 QQ {account_id}）\n用户：{}（QQ {sender_id}）\n阶段：{stage}\n错误：{}",
            empty_log_value(sender_name, "未知用户"),
            bounded_single_line(error, MAX_REASON_CHARS),
        )
    } else {
        format!(
            "[Affection: update failed]\nConversation: group {group_id} (bot QQ {account_id})\nUser: {} (QQ {sender_id})\nStage: {stage}\nError: {}",
            empty_log_value(sender_name, "unknown user"),
            bounded_single_line(error, MAX_REASON_CHARS),
        )
    }
}

pub(crate) fn empty_log_value<'a>(value: &'a str, fallback: &'a str) -> &'a str {
    if value.trim().is_empty() {
        fallback
    } else {
        value
    }
}

pub(in crate::platforms::plugins::real_context::affection) fn tag_change_suffix(
    event: &AffectionEvent,
) -> String {
    let mut changes = Vec::new();
    if !event.tags_add.is_empty() {
        changes.push(format!("新增标签 {}", event.tags_add.join("、")));
    }
    if !event.tags_remove.is_empty() {
        changes.push(format!("删除标签 {}", event.tags_remove.join("、")));
    }
    if changes.is_empty() {
        String::new()
    } else {
        format!("；{}", changes.join("；"))
    }
}

pub(crate) fn format_timestamp(timestamp: i64) -> String {
    chrono::DateTime::from_timestamp(timestamp, 0)
        .map(|value| {
            value
                .with_timezone(&Local)
                .format("%Y-%m-%d %H:%M")
                .to_string()
        })
        .unwrap_or_else(|| "未知时间".to_string())
}
