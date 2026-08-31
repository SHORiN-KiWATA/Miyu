//! 「回给谁」与主动插话的目标选择。
//!
//! 主动回复的目标要从上下文里挑，并且有条数与字节上限（`MAX_ACTIVE_*`）：这段
//! 会拼进提示词，让它跟着群消息量线性增长就等于把窗口交出去。
//!
//! `safe_prompt_string` / `safe_prompt_field` 是注入边界——群名、昵称、消息内
//! 容全是别人写的，直接拼进提示词就是给任何人一个改指令的入口。

use crate::platforms::plugins::real_context::*;

pub(in crate::platforms::plugins::real_context) const TRIGGER_KEY: &str = "real_context.trigger";

pub(in crate::platforms::plugins::real_context) const MODERATION_NOTICE_KEY: &str =
    "real_context.moderation_notice";

pub(in crate::platforms::plugins::real_context) const REPLY_MARKED_KEY: &str =
    "real_context.reply_marked";

pub(in crate::platforms::plugins::real_context) const ACTIVE_TARGETS_KEY: &str =
    "real_context.active_targets";

pub(in crate::platforms::plugins::real_context) const MAX_ACTIVE_TARGET_MESSAGES: usize = 8;

pub(in crate::platforms::plugins::real_context) const MAX_ACTIVE_SUPPLEMENT_MESSAGES: usize = 5;

pub(in crate::platforms::plugins::real_context) const MAX_ACTIVE_CURRENT_CONTENT_BYTES: usize =
    16 * 1024;

pub(in crate::platforms::plugins::real_context) const MAX_ACTIVE_TARGET_PROMPT_BYTES: usize =
    128 * 1024;

pub(in crate::platforms::plugins::real_context) const REPLY_WATERMARK_KEY: &str =
    "reply_ingress_watermark";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::platforms::plugins::real_context) enum TriggerKind {
    Probability,
    Continuation,
    Direct,
    Moderation,
    Supersede,
}

impl TriggerKind {
    pub(in crate::platforms::plugins::real_context) fn as_str(self) -> &'static str {
        match self {
            Self::Probability => "probability",
            Self::Continuation => "continuation",
            Self::Direct => "direct",
            Self::Moderation => "moderation",
            Self::Supersede => "supersede",
        }
    }

    pub(in crate::platforms::plugins::real_context) fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "probability" => Self::Probability,
            "continuation" => Self::Continuation,
            "direct" | "system" => Self::Direct,
            "moderation" => Self::Moderation,
            "supersede" => Self::Supersede,
            _ => return None,
        })
    }

    pub(in crate::platforms::plugins::real_context) fn log_label(
        self,
        locale: Locale,
    ) -> &'static str {
        match (locale, self) {
            (Locale::Zh, Self::Probability) => "概率抽样 (probability)",
            (Locale::Zh, Self::Continuation) => "自然续聊 (continuation)",
            (Locale::Zh, Self::Direct) => "直接触发 (direct)",
            (Locale::Zh, Self::Moderation) => "安全初判 (moderation)",
            (Locale::Zh, Self::Supersede) => "接管上一轮 (supersede)",
            (Locale::En, Self::Probability) => "probability sample (probability)",
            (Locale::En, Self::Continuation) => "natural continuation (continuation)",
            (Locale::En, Self::Direct) => "direct trigger (direct)",
            (Locale::En, Self::Moderation) => "moderation precheck (moderation)",
            (Locale::En, Self::Supersede) => "previous-turn takeover (supersede)",
        }
    }

    pub(in crate::platforms::plugins::real_context) fn decision_log_title(
        self,
        should_reply: bool,
        locale: Locale,
    ) -> String {
        if locale == Locale::Zh {
            let kind = if self == Self::Continuation {
                "续聊窗口判断"
            } else {
                "主动回复判断"
            };
            format!(
                "【{kind}：{}】",
                if should_reply { "回复" } else { "不回复" }
            )
        } else {
            let kind = if self == Self::Continuation {
                "Continuation decision"
            } else {
                "Active reply decision"
            };
            format!(
                "[{kind}: {}]",
                if should_reply { "reply" } else { "no reply" }
            )
        }
    }
}

pub(in crate::platforms::plugins::real_context) fn select_trigger(
    system_triggered: bool,
    moderation_candidate: bool,
    inherited: Option<TriggerKind>,
    continuation: bool,
    probabilistic: bool,
) -> Option<TriggerKind> {
    if system_triggered {
        Some(TriggerKind::Direct)
    } else if moderation_candidate {
        Some(TriggerKind::Moderation)
    } else if let Some(origin) = inherited {
        // 覆盖继承保留原始触发。这个标签是好感度归类(mod.rs 的
        // direct_interaction)、`<qq-join-in>` 注入与判官加分的判据:一律写成
        // Supersede 会让概率承诺被顶替后白拿直呼好感、注入哑火(08-29 定位,
        // 08-31 修)。调用方在原始触发不可考时才传 Supersede 兜底。
        Some(origin)
    } else if continuation {
        Some(TriggerKind::Continuation)
    } else if probabilistic {
        Some(TriggerKind::Probability)
    } else {
        None
    }
}

pub(in crate::platforms::plugins::real_context) fn select_trigger_for_policy(
    active_judgement_allowed: bool,
    system_triggered: bool,
    moderation_candidate: bool,
    inherited: Option<TriggerKind>,
    continuation: bool,
    probabilistic: bool,
) -> Option<TriggerKind> {
    if moderation_candidate && !active_judgement_allowed {
        Some(TriggerKind::Moderation)
    } else {
        select_trigger(
            system_triggered,
            moderation_candidate,
            inherited,
            continuation,
            probabilistic,
        )
    }
}

pub(in crate::platforms::plugins::real_context) fn active_judgement_allowed(
    settings: &RealContextPluginSettings,
    direct_triggered: bool,
    privileged_sender: bool,
    skip_active_judgement: bool,
) -> bool {
    settings.active_reply_enable
        && !skip_active_judgement
        && (!direct_triggered
            || settings.takeover_direct_trigger_enable
                && !(privileged_sender && settings.privileged_direct_trigger_skip_active_judgement))
}

pub(in crate::platforms::plugins::real_context) fn active_reply_target(
    event: &PlatformInboundEvent,
) -> ActiveReplyTarget {
    let supplemental = event.text.trim().is_empty()
        && !event.media.is_empty()
        && event.media.iter().all(|media| {
            matches!(
                media.kind,
                PlatformMediaKind::Image | PlatformMediaKind::Emoji
            )
        });
    let replied = event.replied_message.as_ref();
    ActiveReplyTarget {
        message_id: event.message_id.clone(),
        sender_id: event.sender_id.clone(),
        sender_name: event.sender_display_name.clone(),
        timestamp: event.timestamp,
        content: truncate_utf8(event.text.trim(), 4_096).to_string(),
        reply_message_id: event
            .reply_to_message_id
            .clone()
            .or_else(|| replied.map(|message| message.message_id.clone())),
        reply_sender_id: replied.map(|message| message.sender_id.clone()),
        reply_sender_name: replied.map(|message| message.sender_display_name.clone()),
        reply_content: replied
            .map(|message| truncate_utf8(message.text.trim(), 2_048).to_string())
            .filter(|content| !content.is_empty()),
        mentioned_user_ids: event.mentioned_user_ids.clone(),
        mentioned_users: event.mentioned_users.clone(),
        supplemental,
    }
}

pub(in crate::platforms::plugins::real_context) fn normalize_active_targets(
    targets: &mut Vec<ActiveReplyTarget>,
    sender_id: &str,
) {
    targets.retain(|target| target.sender_id == sender_id);
    let mut seen = std::collections::HashSet::new();
    targets.retain(|target| target.message_id.is_empty() || seen.insert(target.message_id.clone()));
    while targets.iter().filter(|target| !target.supplemental).count() > MAX_ACTIVE_TARGET_MESSAGES
    {
        if let Some(index) = targets.iter().position(|target| !target.supplemental) {
            targets.remove(index);
        }
    }
    while targets.iter().filter(|target| target.supplemental).count()
        > MAX_ACTIVE_SUPPLEMENT_MESSAGES
    {
        if let Some(index) = targets.iter().position(|target| target.supplemental) {
            targets.remove(index);
        }
    }
}

pub(in crate::platforms::plugins::real_context) fn active_targets_from_context(
    context: &PlatformTurnContext,
) -> Vec<ActiveReplyTarget> {
    context
        .plugin_value(ACTIVE_TARGETS_KEY)
        .and_then(|value| serde_json::from_value(value).ok())
        .unwrap_or_default()
}

pub(in crate::platforms::plugins::real_context) fn set_active_targets(
    context: &PlatformTurnContext,
    targets: &[ActiveReplyTarget],
) {
    if let Ok(value) = serde_json::to_value(targets) {
        context.set_plugin_value(ACTIVE_TARGETS_KEY, value);
    }
}

/// `@mentions:` 行。`self_id` 给出时,机器人自己那一项渲染成 `[you]`——
/// 与历史块里 `[you] marks your own messages` 同一个记号。
///
/// 08-29:`<qq-request-context>` 原来还带一个 `"mentioned_bot": false`,那是
/// 全项目唯一一处「陈述一件没发生的事」。判官放行、回合已经起来之后,这个
/// 字段对人格模型没有任何正当用途,却正好是一个现成判据——实测她拿它推出
/// 「没提到我 不接」并当成正文发进群里(当天八次)。字段已删,这里补上正向
/// 的一半:被 @ 时事实照常在场,没被 @ 时单纯不出现,不再有可供立规矩的否定
/// 陈述。
pub(in crate::platforms::plugins::real_context) fn format_mentioned_users(
    users: &[PlatformMention],
    user_ids: &[String],
    show_ids: bool,
    self_id: Option<&str>,
) -> Option<String> {
    let users = if users.is_empty() {
        user_ids
            .iter()
            .map(|user_id| PlatformMention {
                user_id: user_id.clone(),
                display_name: None,
            })
            .collect::<Vec<_>>()
    } else {
        users.to_vec()
    };
    if users.is_empty() {
        return None;
    }
    Some(
        users
            .iter()
            .map(|user| {
                if self_id.is_some_and(|self_id| self_id == user.user_id) {
                    return "[you]".to_string();
                }
                match user.display_name.as_deref() {
                    Some(name) if show_ids => format!(
                        "{}(QQ:{})",
                        safe_prompt_field(name),
                        safe_prompt_field(&user.user_id)
                    ),
                    Some(name) => safe_prompt_field(name),
                    None if show_ids => format!("QQ:{}", safe_prompt_field(&user.user_id)),
                    None => "unresolved group member".to_string(),
                }
            })
            .collect::<Vec<_>>()
            .join("、"),
    )
}

/// 本轮真正要回答的那条消息 id。默认是触发消息,纯附件(图片/表情)让位给
/// 合并集里最新的文字消息——历史块要据此把它摘出去(否则同一条既在
/// `[Prior group chat records]` 又在 `[New messages received this turn]`,
/// 08-26 审查抓到)。
pub(in crate::platforms::plugins::real_context) fn answer_target_id(
    context: &PlatformTurnContext,
    event: &PlatformInboundEvent,
) -> String {
    let mut targets = active_targets_from_context(context);
    if !targets
        .iter()
        .any(|target| target.message_id == event.message_id)
    {
        targets.push(active_reply_target(event));
    }
    normalize_active_targets(&mut targets, &event.sender_id);
    let current_is_supplemental = targets
        .iter()
        .find(|target| target.message_id == event.message_id)
        .is_some_and(|target| target.supplemental)
        && targets.iter().any(|target| !target.supplemental);
    if !current_is_supplemental {
        return event.message_id.clone();
    }
    targets
        .iter()
        .filter(|target| !target.supplemental)
        .next_back()
        .map(|target| target.message_id.clone())
        .unwrap_or_else(|| event.message_id.clone())
}

pub(in crate::platforms::plugins::real_context) fn active_target_prompt(
    context: &PlatformTurnContext,
    event: &PlatformInboundEvent,
    current_content: &str,
) -> String {
    let mut targets = active_targets_from_context(context);
    if !targets
        .iter()
        .any(|target| target.message_id == event.message_id)
    {
        targets.push(active_reply_target(event));
    }
    normalize_active_targets(&mut targets, &event.sender_id);
    if !targets.iter().any(|target| !target.supplemental) {
        if let Some(current) = targets
            .iter_mut()
            .find(|target| target.message_id == event.message_id)
        {
            current.supplemental = false;
        }
    }

    let show_ids = context.config.platforms.qq.user_identification;
    let format_target = |target: &ActiveReplyTarget| {
        let content = if target.message_id == event.message_id {
            truncate_utf8(current_content.trim(), MAX_ACTIVE_CURRENT_CONTENT_BYTES)
        } else {
            target.content.trim()
        };
        let content = if content.is_empty() {
            "(no text content; contains images or stickers)".to_string()
        } else {
            content.to_string()
        };
        let sender = if show_ids {
            format!(
                "{}(QQ:{})",
                safe_prompt_field(&target.sender_name),
                safe_prompt_field(&target.sender_id)
            )
        } else {
            safe_prompt_field(&target.sender_name)
        };
        let mut line = format!(
            "[{}] {} [msg={}]: {}",
            format_history_time(target.timestamp),
            sender,
            safe_prompt_field(&target.message_id),
            safe_prompt_field(&content)
        );
        if let Some(message_id) = target.reply_message_id.as_ref() {
            // 引用行的作者/时态/引号三重显式标记(08-25:管道格式 "reply-to:
            // … | 名字 | 原文" 会被弱模型与当前消息连读,把引用内容当成本次
            // 发言的一部分——渲染层就要把"旧话、别人说的"钉死,不能只靠
            // <qq-reply-format> 规则自觉)。
            let author = match (
                target.reply_sender_name.as_ref(),
                target.reply_sender_id.as_ref().filter(|_| show_ids),
            ) {
                (Some(name), Some(id)) => {
                    format!("{}(QQ:{})", safe_prompt_field(name), safe_prompt_field(id))
                }
                (Some(name), None) => safe_prompt_field(name),
                (None, Some(id)) => format!("QQ:{}", safe_prompt_field(id)),
                (None, None) => "an earlier sender".to_string(),
            };
            line.push_str(&format!(
                "\n  quoted earlier message [msg={}] by {author}",
                safe_prompt_field(message_id)
            ));
            if let Some(content) = target.reply_content.as_ref() {
                line.push_str(&format!(": \u{201c}{}\u{201d}", safe_prompt_field(content)));
            }
        }
        if let Some(mentions) = format_mentioned_users(
            &target.mentioned_users,
            &target.mentioned_user_ids,
            show_ids,
            Some(context.conversation.account_id.as_str()),
        ) {
            line.push_str(&format!("\n  @mentions: {mentions}"));
        }
        line
    };

    // 谁占"当前消息"位。默认是最新那条,但纯附件(图片/表情,supplemental)
    // 让位给合并集里最新的文字消息(08-26 取证:文字提问触发回复后补一张
    // 表情包,表情占了当前消息位,模型先评图再答题,真正的问题被降级成
    // 背景)。`supplemental` 本来就是"补充材料,不该被单独回复"的意思,
    // 这里让结构兑现它——加一句"看到图片先答文字"的指令是压不住的。
    let answer_target = answer_target_id(context, event);
    // 当前消息同样走 format_target:署名/时间/msg id/reply-to/@mentions 全套
    // 坐标(08-24 取证:裸文本"卡死了？"贴在他人求助截图后,模型把 A 的问题
    // 安到了 B 头上——判读线索被我们自己削没了)。
    let current = targets
        .iter()
        .find(|target| target.message_id == answer_target)
        .map(&format_target)
        .unwrap_or_else(|| current_content.trim().to_string());
    // 占了当前消息位的那条不再进其它块——否则同一条渲染两遍(08-26 实录:
    // 表情包同时出现在"本轮新消息"和"随后补充"里,双份强调)。
    let rest = targets
        .iter()
        .filter(|target| target.message_id != answer_target);
    let previous = rest
        .clone()
        .filter(|target| !target.supplemental)
        .map(&format_target)
        .collect::<Vec<_>>();
    let supplements = rest
        .filter(|target| target.supplemental)
        .map(&format_target)
        .collect::<Vec<_>>();
    // 块标记同样只描述内容本身。原来结尾那条「只回复当前消息…补充材料不应被单独
    // 回复。需要调用工具时…」整条删除:前两句是跨轮指令丢失的语义来源,末句是多余
    // 的输出约束,而唯一有信息量的「以后文为准」已由标记里的"按时间先后排列"覆盖。
    let head = format!("[New messages received this turn]\n{current}");
    let mut sections = vec![head.clone()];
    if !previous.is_empty() {
        sections.extend([
            "\n[Earlier messages from the same sender this turn, in chronological order]"
                .to_string(),
            previous.join("\n"),
        ]);
    }
    if !supplements.is_empty() {
        sections.extend([
            "\n[Follow-up messages sent later by the same sender, in chronological order]"
                .to_string(),
            supplements.join("\n"),
        ]);
    }
    let body = sections.join("\n");
    let body = if body.len() > MAX_ACTIVE_TARGET_PROMPT_BYTES {
        let marker = "\n\n(earlier merged messages omitted due to length limits)\n";
        let suffix_budget = MAX_ACTIVE_TARGET_PROMPT_BYTES
            .saturating_sub(head.len())
            .saturating_sub(marker.len());
        format!("{head}{marker}{}", truncate_utf8_tail(&body, suffix_budget))
    } else {
        body
    };
    body
}

pub(in crate::platforms::plugins::real_context) fn response_target(
    event: &PlatformInboundEvent,
    settings: &RealContextPluginSettings,
) -> Option<ResponseTarget> {
    if !settings.reply_target_enable {
        return None;
    }
    let target = ResponseTarget {
        message_id: event.message_id.clone(),
        user_id: event.sender_id.clone(),
        quote: settings.reply_target_quote_enable,
        mention: settings.reply_target_mention_enable,
        explicit_mention_user_ids: Vec::new(),
    };
    target.is_effective().then_some(target)
}

pub(in crate::platforms::plugins::real_context) fn adaptive_response_target(
    context: &PlatformTurnContext,
    event: &PlatformInboundEvent,
    settings: &RealContextPluginSettings,
) -> Option<ResponseTarget> {
    let mut target = response_target(event, settings);
    // 纯附件让出"当前消息"位之后,引用也跟着钉回那条文字消息(08-26):
    // 答的是问题,却引用一张表情包,读起来是两码事。
    if let Some(target) = target.as_mut() {
        if active_reply_target(event).supplemental {
            if let Some(text_target) = active_targets_from_context(context)
                .into_iter()
                .filter(|candidate| {
                    !candidate.supplemental
                        && candidate.sender_id == event.sender_id
                        && !candidate.message_id.is_empty()
                })
                .next_back()
            {
                target.message_id = text_target.message_id;
            }
        }
    }
    context.set_adaptive_response_target(
        target.clone(),
        AdaptiveResponseTargetPolicy::new(
            event.message_position,
            event.received_at,
            settings.reply_target_quote_after_other_messages,
            settings.reply_target_mention_after_seconds,
        ),
    );
    target
}

pub(in crate::platforms::plugins::real_context) fn restore_core_trigger(
    context: &PlatformTurnContext,
    decision: &mut TriggerDecision,
    fallback: &TriggerDecision,
) {
    restore_trigger_decision(decision, fallback);
    context.set_response_target(decision.response_target.clone());
}

pub(in crate::platforms::plugins::real_context) fn restore_trigger_decision(
    decision: &mut TriggerDecision,
    fallback: &TriggerDecision,
) {
    *decision = fallback.clone();
}

/// 概率抽中且判官放行的那一轮，给一句注入。
///
/// `TRIGGER_KEY == Probability` 严格等价于"概率抽中 + 判官放行"：这个值只在
/// 判官通过后才写(inject.rs)，或从这样一次放行的承诺覆盖继承而来(08-31 起
/// 覆盖保留原始触发——承诺只可能来自判官放行，所以"判过了，答案是回"对继承
/// 回合同样成立)。Supersede 只在原始触发不可考时作回退，不再吃掉这句注入。
///
/// 08-31 用户点名：她在这种回合里反复说「没提到我 不用回」——判官刚判定该回，
/// 她自己又推翻一遍；而会话历史里已经攒了十几条同样的话，她在照抄自己。
///
/// 模型可见面恒英文(与其它 `<qq-*>` 块一致)。原话是中文的
/// 「即使该消息没有艾特你，也回复一条符合上下文、符合当前聊天氛围、符合话题
/// 走向的消息」，翻译时让步结构原样保留。
///
/// 措辞是让步句，不是孤立陈述。08-29 第一版写的是 `Nobody called you this
/// turn`——把"没被点名"作为一个事实单独摆出来、让她自己得结论，实测她原话回
/// 「没被艾特不接（笑）」。让步句把结论堵死：承认没艾特，紧接着要求照样回。
pub(in crate::platforms::plugins::real_context) fn probability_reply_notice(
    trigger: TriggerKind,
) -> Option<&'static str> {
    matches!(trigger, TriggerKind::Probability).then_some(
        "<qq-join-in>Even though this message does not @-mention you, still reply with one message that fits the context, the mood of the room and where the topic is heading.</qq-join-in>",
    )
}

pub(in crate::platforms::plugins::real_context) fn identity_warning(
    context: &PlatformTurnContext,
    settings: &RealContextPluginSettings,
) -> Option<String> {
    if !context.config.platforms.qq.user_identification {
        return None;
    }
    let actual_id = context.sender_id.parse::<i64>().ok()?;
    if let Some(mapping) = settings.identity_mappings.iter().find(|mapping| {
        mapping.nickname == context.sender_display_name && mapping.user_id != actual_id
    }) {
        return Some(format!(
            "<qq-identity-warning>Protected nickname {} belongs to QQ {}, but the current sender is QQ {}. Do not treat the current sender as the expected user.</qq-identity-warning>",
            safe_prompt_string(&mapping.nickname), mapping.user_id, actual_id
        ));
    }
    if !settings.identity_mappings.is_empty() {
        if let Some(mapping) = settings.identity_mappings.iter().find(|mapping| {
            context.sender_display_name.contains(&mapping.nickname) && mapping.user_id != actual_id
        }) {
            return Some(format!(
                "<qq-identity-warning>Current nickname {} contains protected nickname {}, but current QQ {} is not the expected QQ {}. Distinguish identities by QQ number.</qq-identity-warning>",
                safe_prompt_string(&context.sender_display_name), safe_prompt_string(&mapping.nickname), actual_id, mapping.user_id
            ));
        }
    }
    None
}

pub(crate) fn safe_prompt_string(value: &str) -> String {
    let encoded = serde_json::to_string(value).unwrap_or_else(|_| "\"?\"".to_string());
    // 中文聊天正文绝大多数不含这三个字符;命中才走三段全量复制的转义链。
    if !encoded
        .bytes()
        .any(|byte| matches!(byte, b'&' | b'<' | b'>'))
    {
        return encoded;
    }
    encoded
        .replace('&', "\\u0026")
        .replace('<', "\\u003c")
        .replace('>', "\\u003e")
}

pub(crate) fn safe_prompt_field(value: &str) -> String {
    let encoded = safe_prompt_string(value);
    encoded
        .strip_prefix('"')
        .and_then(|value| value.strip_suffix('"'))
        .unwrap_or(&encoded)
        .to_string()
}

pub(in crate::platforms::plugins::real_context) fn moderation_notice(
    moderation: &judge::ModerationResult,
) -> String {
    format!(
        "Preliminary moderation flag: severity {:.1}/10; category: {}; evidence: {}; rule basis: {}; reasoning: {}; related QQ: {}; related message IDs: {}.",
        moderation.severity,
        empty_as(&moderation.category, "uncategorized"),
        empty_as(&moderation.evidence, "not provided"),
        empty_as(&moderation.rule_basis, "the fixed safety baseline"),
        empty_as(&moderation.reasoning, "not provided"),
        moderation.related_user_ids.join(", "),
        moderation.related_message_ids.join(", "),
    )
}

pub(in crate::platforms::plugins::real_context) fn empty_as<'a>(
    value: &'a str,
    fallback: &'a str,
) -> &'a str {
    if value.trim().is_empty() {
        fallback
    } else {
        value
    }
}

pub(in crate::platforms::plugins::real_context) fn find_keyword<'a>(
    keywords: &'a [String],
    text: &str,
) -> Option<&'a str> {
    let mut folded = None;
    keywords
        .iter()
        .find(|keyword| {
            if keyword.is_ascii() {
                return contains_ascii_case_insensitive(text, keyword);
            }
            if !keyword
                .chars()
                .any(|character| character.is_lowercase() || character.is_uppercase())
            {
                return text.contains(keyword.as_str());
            }
            folded
                .get_or_insert_with(|| text.to_lowercase())
                .contains(&keyword.to_lowercase())
        })
        .map(String::as_str)
}

pub(in crate::platforms::plugins::real_context) fn contains_ascii_case_insensitive(
    text: &str,
    needle: &str,
) -> bool {
    if needle.is_empty() {
        return false;
    }
    text.as_bytes()
        .windows(needle.len())
        .any(|window| window.eq_ignore_ascii_case(needle.as_bytes()))
}
