//! 回复文本的整形与分段。
//!
//! 聊天平台不认 Markdown，所以要降成纯文本（`markdown_to_plain`）。
//!
//! `split_reply` 把长回复切成多条发：切点优先落在段落和句子边界上，硬切会把一
//! 句话腰斩。
//!
//! `ReplySuppression` 处理的是「工具已经把这段内容直接发出去了」——正文里就不
//! 该再出现一次。`cut_suppressed_ranges` 按区间裁，因为被抑制的可能只是中间
//! 一段。

use crate::platforms::*;

#[derive(Clone, Copy, Debug)]
pub(crate) struct AdaptiveResponseTargetPolicy {
    pub(crate) position: Option<PlatformMessagePosition>,
    pub(crate) received_at: Instant,
    pub(crate) quote_after_other_messages: u64,
    pub(crate) mention_after: Duration,
}

impl AdaptiveResponseTargetPolicy {
    pub(crate) fn new(
        position: Option<PlatformMessagePosition>,
        received_at: Instant,
        quote_after_other_messages: u64,
        mention_after_seconds: u64,
    ) -> Self {
        Self {
            position,
            received_at,
            quote_after_other_messages,
            mention_after: Duration::from_secs(mention_after_seconds),
        }
    }

    /// `last_message_is_own`:会话里最后一条是不是她自己刚发的。
    ///
    /// 08-29 用户点名:连发时两条定向线索会一起哑火。艾特要"隔了时间**且**
    /// 别人说过话",引用要"隔了几条别人的消息";她回完 A 立刻回 B,B 那条
    /// 两个条件都不满足,于是既不引用也不艾特——而上一条还是她自己的,读的
    /// 人分不清她在跟谁说话。真人在群里连着说话也会靠引用分流。
    pub(crate) fn resolve(
        self,
        mut target: ResponseTarget,
        current: Option<PlatformMessagePosition>,
        last_message_is_own: bool,
        now: Instant,
    ) -> Option<ResponseTarget> {
        let other_messages = self.position.zip(current).map(|(start, current)| {
            let total = current.total_messages.saturating_sub(start.total_messages);
            let same_sender = current
                .sender_messages
                .saturating_sub(start.sender_messages);
            total.saturating_sub(same_sender)
        });
        if target.quote {
            target.quote = last_message_is_own
                || self.quote_after_other_messages == 0
                || other_messages.is_some_and(|count| count >= self.quote_after_other_messages);
        }
        if target.mention {
            // Unknown activity preserves the original time-only mention behavior.
            target.mention = now
                .checked_duration_since(self.received_at)
                .unwrap_or_default()
                >= self.mention_after
                && other_messages.is_none_or(|count| count > 0);
        }
        target.is_effective().then_some(target)
    }
}

#[derive(Clone)]
pub(crate) struct PendingResponseTarget {
    pub(crate) target: ResponseTarget,
    pub(crate) policy: Option<AdaptiveResponseTargetPolicy>,
}

pub(crate) fn apply_resolved_response_target(
    message: &mut OutboundMessage,
    original: &ResponseTarget,
    resolved: Option<&ResponseTarget>,
) {
    if message.response_target.as_ref() == Some(original) {
        message.response_target = resolved.cloned();
    }
}

/// 剥掉模型漏进正文通道的工具调用模板(08-23 线上取证:mimo 系复读退化
/// 时把 `<tool_call><function=...>` 当文本吐出,中转解析器不认就落进
/// content,QQ 分段投递原样发到群里)。返回 true = 有段被改写或移除;剥完
/// 没有任何文本/媒体段的消息由调用方整条抑制。
pub(crate) fn strip_tool_call_leaks(message: &mut OutboundMessage) -> bool {
    let OutboundBody::Segments(segments) = &mut message.body else {
        return false;
    };
    let mut changed = false;
    segments.retain_mut(|segment| match segment {
        OutboundSegment::Markdown(part) | OutboundSegment::Text(part) => {
            let Some(stripped) = strip_leak_spans(part) else {
                return true;
            };
            changed = true;
            if stripped.trim().is_empty() {
                false
            } else {
                *part = stripped;
                true
            }
        }
        _ => true,
    });
    changed
}

/// 剥完后还有没有值得投递的内容(纯 Mention 不算)。
pub(crate) fn message_has_deliverable_content(message: &OutboundMessage) -> bool {
    let OutboundBody::Segments(segments) = &message.body else {
        return true;
    };
    segments.iter().any(|segment| match segment {
        OutboundSegment::Markdown(part) | OutboundSegment::Text(part) => !part.trim().is_empty(),
        OutboundSegment::Mention(_) => false,
        _ => true,
    })
}

/// 移除文本里的 `<tool_call>…</tool_call>` 与裸 `<function=…</function>`
/// span(缺闭合标签=剥到结尾,流式截断的残模板就是这个形态)。
/// None = 文本干净没动。
fn strip_leak_spans(text: &str) -> Option<String> {
    const SPANS: [(&str, &str); 2] = [
        ("<tool_call>", "</tool_call>"),
        ("<function=", "</function>"),
    ];
    let mut output = text.to_string();
    let mut changed = false;
    loop {
        let Some((start, open, close)) = SPANS
            .iter()
            .filter_map(|(open, close)| output.find(open).map(|at| (at, *open, *close)))
            .min_by_key(|(at, _, _)| *at)
        else {
            break;
        };
        let _ = open;
        let end = output[start..]
            .find(close)
            .map(|at| start + at + close.len())
            .unwrap_or(output.len());
        output.replace_range(start..end, "");
        changed = true;
    }
    changed.then_some(output)
}

/// 看起来是空的:只有空白或不可见字符(零宽空格/零宽连接符/BOM/软连字…)。
/// 模型"什么都不想说"时常吐一个 U+200B,`trim()` 不认它,发出去就是一条空气泡
/// (09-06 QQ 群里被人调侃「叛逆了」)。只用来判空,不用来改写正文——U+200D 在
/// emoji 序列里是有意义的。
pub(crate) fn visibly_blank(text: &str) -> bool {
    text.chars().all(|ch| {
        ch.is_whitespace()
            || matches!(
                ch,
                '\u{200B}'..='\u{200F}'
                    | '\u{2060}'..='\u{2064}'
                    | '\u{FEFF}'
                    | '\u{00AD}'
                    | '\u{180E}'
                    | '\u{2028}'
                    | '\u{2029}'
            )
    })
}

pub(crate) fn message_is_parenthetical_only(message: &OutboundMessage) -> bool {
    let OutboundBody::Segments(segments) = &message.body else {
        return false;
    };
    let mut text = String::new();
    for segment in segments {
        match segment {
            OutboundSegment::Markdown(part) | OutboundSegment::Text(part) => text.push_str(part),
            OutboundSegment::Mention(_) => {}
            OutboundSegment::ImageBytes { .. }
            | OutboundSegment::ImagePath { .. }
            | OutboundSegment::FilePath { .. }
            | OutboundSegment::AudioPath { .. } => return false,
        }
    }
    let text = text.trim();
    if text.is_empty() || !text.starts_with('（') || !text.ends_with('）') {
        return false;
    }
    let mut depth = 0_u32;
    let mut chars = text.chars().peekable();
    while let Some(character) = chars.next() {
        match character {
            '（' => depth = depth.saturating_add(1),
            '）' => {
                let Some(next_depth) = depth.checked_sub(1) else {
                    return false;
                };
                depth = next_depth;
                if depth == 0 && chars.peek().is_some() {
                    return false;
                }
            }
            _ if depth == 0 => return false,
            _ => {}
        }
    }
    depth == 0
}

pub(crate) fn outbound_text_for_history(message: &OutboundMessage) -> String {
    fn append(parts: &mut Vec<String>, segments: &[OutboundSegment]) {
        for segment in segments {
            match segment {
                OutboundSegment::Markdown(text) | OutboundSegment::Text(text) => {
                    if !text.trim().is_empty() {
                        parts.push(text.clone());
                    }
                }
                OutboundSegment::Mention(user_id) => parts.push(format!("@{user_id}")),
                OutboundSegment::ImageBytes { .. }
                | OutboundSegment::ImagePath { .. }
                | OutboundSegment::FilePath { .. } => {}
                OutboundSegment::AudioPath { transcript, .. } => {
                    parts.push(crate::platform_types::voice_history_text(transcript))
                }
            }
        }
    }

    let mut parts = Vec::new();
    match &message.body {
        OutboundBody::Segments(segments) => append(&mut parts, segments),
        OutboundBody::Forward(nodes) => {
            for node in nodes {
                append(&mut parts, &node.segments);
            }
        }
    }
    parts.join("\n").trim().to_string()
}

#[derive(Default)]
pub(crate) struct ReplySuppression {
    pub(crate) ranges: Vec<(usize, usize)>,
    pub(crate) open_at: Option<usize>,
    pub(crate) final_reply_already_sent: bool,
}

impl ReplySuppression {
    pub(crate) fn direct_send_succeeded(&mut self, text_len: usize) {
        self.open_at = Some(
            self.open_at
                .map_or(text_len, |existing| existing.min(text_len)),
        );
        self.final_reply_already_sent = true;
    }

    pub(crate) fn model_started(&mut self) {
        self.ranges.clear();
        // A direct tool send answers the same prompt across its model
        // continuation, so suppress that continuation from its first byte.
        self.open_at = self.final_reply_already_sent.then_some(0);
    }

    pub(crate) fn queued_prompt_consumed(&mut self) {
        self.ranges.clear();
        self.open_at = None;
        self.final_reply_already_sent = false;
    }

    /// 回合中途把已说的正文当中间消息发走、`text` 随即清空之后的复位。
    ///
    /// 与 [`Self::model_started`] 的区别在于「这不是新一轮模型回复」:抑制这件
    /// 事本身还在继续,只有区间偏移要跟着清空的 `text` 归零。`open_at` 是
    /// 「从这里往后都抑制」的游标,text 一空它的锚点就是 0;本来没在抑制的
    /// 仍旧没在抑制。`final_reply_already_sent` 属于整轮,不动。
    pub(crate) fn round_flushed(&mut self) {
        self.ranges.clear();
        self.open_at = self.open_at.map(|_| 0);
    }

    pub(crate) fn finish(mut self, text_len: usize) -> (Vec<(usize, usize)>, bool) {
        self.close_range(text_len);
        (self.ranges, self.final_reply_already_sent)
    }

    pub(crate) fn close_range(&mut self, text_len: usize) {
        if let Some(start) = self.open_at.take() {
            if start < text_len {
                self.ranges.push((start, text_len));
            }
        }
    }

    /// Ranges to cut when the current round's text is flushed mid-turn as an
    /// intermediate reply. Leaves the state untouched so the `model_started`
    /// reset that follows keeps its existing semantics.
    pub(crate) fn round_ranges(&self, text_len: usize) -> Vec<(usize, usize)> {
        let mut ranges = self.ranges.clone();
        if let Some(start) = self.open_at {
            if start < text_len {
                ranges.push((start, text_len));
            }
        }
        ranges
    }
}

pub(crate) fn cut_suppressed_ranges(text: &str, ranges: &[(usize, usize)]) -> String {
    if ranges.is_empty() {
        return text.to_string();
    }
    let mut result = String::with_capacity(text.len());
    let mut cursor = 0;
    for &(start, end) in ranges {
        let start = start.clamp(cursor, text.len());
        let end = end.clamp(start, text.len());
        let (Some(prefix), Some(_suppressed)) = (text.get(cursor..start), text.get(start..end))
        else {
            continue;
        };
        result.push_str(prefix);
        cursor = end;
    }
    if let Some(suffix) = text.get(cursor..) {
        result.push_str(suffix);
    }
    result
}

pub(crate) fn start_model_reply(text: &mut String, suppression: &mut ReplySuppression) {
    text.clear();
    suppression.model_started();
}

/// Sends the just-finished model round as its own platform message. The
/// round's direct-send suppression ranges still apply, so text a tool already
/// delivered is not repeated.
pub(crate) async fn flush_intermediate_reply(
    context: &PlatformTurnContext,
    text: &str,
    suppression: &ReplySuppression,
) {
    if context.turn_is_superseded() {
        return;
    }
    let visible = cut_suppressed_ranges(text, &suppression.round_ranges(text.len()));
    let visible = visible.trim();
    if visibly_blank(visible) {
        return;
    }
    match context
        .send(OutboundMessage::markdown(
            OutboundOrigin::IntermediateReply,
            visible.to_string(),
        ))
        .await
    {
        // 投递成功才进幂等闸登记(失败就登记会把之后的正当重发也拦掉):
        // 模型下一轮若用工具重发同段(端点故障日的重演惯犯),send 侧被拒。
        Ok(_) => {
            context.record_delivered_reply_text(visible);
            tracing::info!(
            target: "miyu::qq",
            chars = visible.chars().count(),
            "{}",
            crate::i18n::text(
                "sent an intermediate platform reply",
                "已发送平台中间消息",
            )
            );
        }
        Err(error) => tracing::warn!(
            target: "miyu::qq",
            error = %error,
            "{}",
            crate::i18n::text(
                "sending an intermediate platform reply failed",
                "发送平台中间消息失败",
            )
        ),
    }
}

/// Strips markdown decoration for plain-text IM surfaces (QQ renders no
/// markup). Deliberately conservative: fenced code bodies are kept
/// verbatim, single `*` stays (could be math), lists and newlines stay.
pub(crate) fn markdown_to_plain(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut in_fence = false;
    for line in text.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            in_fence = !in_fence;
            continue;
        }
        if in_fence {
            out.push_str(line);
            out.push('\n');
            continue;
        }
        let stripped = if trimmed.starts_with('#') {
            trimmed.trim_start_matches('#').trim_start()
        } else if let Some(rest) = trimmed.strip_prefix("> ") {
            rest
        } else {
            line
        };
        out.push_str(&strip_inline_markup(stripped));
        out.push('\n');
    }
    out.trim_end().to_string()
}

/// Removes `**`, `__`, backticks and rewrites `[text](url)` → `text (url)`.
/// 找出第一个下标 >= `from` 的 `needle` 位置。
///
/// `positions` 是升序的候选下标，`cursor` 是**跨调用保留**的游标。调用方的
/// `from` 只增不减，所以游标只前进不回退，整趟下来是 O(n) 而不是每次重扫。
fn next_position_at_or_after(
    positions: &[usize],
    cursor: &mut usize,
    from: usize,
) -> Option<usize> {
    while *cursor < positions.len() && positions[*cursor] < from {
        *cursor += 1;
    }
    positions.get(*cursor).copied()
}

pub(crate) fn strip_inline_markup(line: &str) -> String {
    let chars: Vec<char> = line.chars().collect();
    // 原来每遇到一个 `[` 就往后线性找 `]`（找不到就 i += 1 再来一遍），
    // 一串未匹配的 `[` 就是 O(n²)：实测 16,000 个 `[` 要 358ms，而每条 QQ
    // 出站消息都走这条路。改成预扫一遍位置 + 单调游标，整体降到 O(n)。
    let close_positions: Vec<usize> = chars
        .iter()
        .enumerate()
        .filter_map(|(index, c)| (*c == ']').then_some(index))
        .collect();
    let paren_positions: Vec<usize> = chars
        .iter()
        .enumerate()
        .filter_map(|(index, c)| (*c == ')').then_some(index))
        .collect();
    let mut close_cursor = 0;
    let mut paren_cursor = 0;
    let mut out = String::with_capacity(line.len());
    let mut i = 0;
    while i < chars.len() {
        match chars[i] {
            '*' if chars.get(i + 1) == Some(&'*') => i += 2,
            '_' if chars.get(i + 1) == Some(&'_') => i += 2,
            '`' => i += 1,
            '[' => {
                // Try [text](url); anything else is emitted verbatim.
                let close = next_position_at_or_after(&close_positions, &mut close_cursor, i + 1);
                let parsed = close.and_then(|close| {
                    if chars.get(close + 1) == Some(&'(') {
                        let end = next_position_at_or_after(
                            &paren_positions,
                            &mut paren_cursor,
                            close + 2,
                        );
                        end.map(|end| {
                            let text: String = chars[i + 1..close].iter().collect();
                            let url: String = chars[close + 2..end].iter().collect();
                            (end + 1, text, url)
                        })
                    } else {
                        None
                    }
                });
                match parsed {
                    Some((next, text, url)) => {
                        out.push_str(&text);
                        if !url.is_empty() && url != text {
                            out.push_str(" (");
                            out.push_str(&url);
                            out.push(')');
                        }
                        i = next;
                    }
                    None => {
                        out.push('[');
                        i += 1;
                    }
                }
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    out
}

/// Splits an over-long reply on paragraph, then line, then raw char
/// boundaries. Char-based so CJK never gets cut mid-codepoint.
pub(crate) fn split_reply(text: &str, max_chars: usize) -> Vec<String> {
    let text = text.trim();
    if text.is_empty() {
        return Vec::new();
    }
    if max_chars == 0 || text.chars().count() <= max_chars {
        return vec![text.to_string()];
    }
    let mut chunks = Vec::new();
    let mut current = String::new();
    let mut current_chars = 0;
    let flush = |current: &mut String, current_chars: &mut usize, chunks: &mut Vec<String>| {
        let piece = current.trim();
        if !piece.is_empty() {
            chunks.push(piece.to_string());
        }
        current.clear();
        *current_chars = 0;
    };
    for paragraph in text.split("\n\n") {
        let unit_chars = paragraph.chars().count();
        if unit_chars > max_chars {
            flush(&mut current, &mut current_chars, &mut chunks);
            // Oversized paragraph: pack by lines, hard-split huge lines.
            for line in paragraph.lines() {
                let line_chars = line.chars().count();
                if line_chars > max_chars {
                    flush(&mut current, &mut current_chars, &mut chunks);
                    let mut buffer = String::new();
                    let mut count = 0;
                    for c in line.chars() {
                        buffer.push(c);
                        count += 1;
                        if count == max_chars {
                            chunks.push(buffer.clone());
                            buffer.clear();
                            count = 0;
                        }
                    }
                    if !buffer.trim().is_empty() {
                        chunks.push(buffer.trim().to_string());
                    }
                    continue;
                }
                if current_chars + line_chars + 1 > max_chars {
                    flush(&mut current, &mut current_chars, &mut chunks);
                }
                if !current.is_empty() {
                    current.push('\n');
                    current_chars += 1;
                }
                current.push_str(line);
                current_chars += line_chars;
            }
            flush(&mut current, &mut current_chars, &mut chunks);
            continue;
        }
        if current_chars + unit_chars + 2 > max_chars {
            flush(&mut current, &mut current_chars, &mut chunks);
        }
        if !current.is_empty() {
            current.push_str("\n\n");
            current_chars += 2;
        }
        current.push_str(paragraph);
        current_chars += unit_chars;
    }
    flush(&mut current, &mut current_chars, &mut chunks);
    chunks
}

/// Sniffs the mime type of downloaded image bytes by magic numbers.
pub(crate) fn sniff_image_mime(bytes: &[u8]) -> &'static str {
    if bytes.starts_with(&[0x89, b'P', b'N', b'G']) {
        "image/png"
    } else if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        "image/jpeg"
    } else if bytes.starts_with(b"GIF8") {
        "image/gif"
    } else if bytes.len() > 11 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        "image/webp"
    } else {
        "image/png"
    }
}

/// Downloads a URL with a byte cap enforced while streaming, so an
/// oversized (or length-less) body can never balloon memory.
pub(crate) async fn download_capped(
    client: &reqwest::Client,
    url: &str,
    max_bytes: usize,
    timeout: Duration,
) -> Result<(Vec<u8>, Option<String>)> {
    let response = client
        .get(url)
        .timeout(timeout)
        .send()
        .await
        .with_context(|| format!("requesting {url}"))?
        .error_for_status()
        .with_context(|| format!("downloading {url}"))?;
    if let Some(length) = response.content_length() {
        if length as usize > max_bytes {
            bail!(
                "the file is larger than the {}MB limit",
                max_bytes / 1024 / 1024
            );
        }
    }
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string);
    let mut bytes = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.with_context(|| format!("reading {url}"))?;
        if bytes.len() + chunk.len() > max_bytes {
            bail!(
                "the file is larger than the {}MB limit",
                max_bytes / 1024 / 1024
            );
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok((bytes, content_type))
}

#[cfg(test)]
mod strip_markup_tests {
    use super::*;

    /// 改之前的实现，逐字抄下来当参照。
    ///
    /// 性能改动的铁律是「只改执行方式，不改结果」，而这条路决定用户在 QQ 里
    /// 看到的每一个字。断言几个手挑的用例守不住这种改动——真正的验证是拿旧
    /// 实现当预言机，在一大堆输入上比对逐字节相等。
    fn reference_strip_inline_markup(line: &str) -> String {
        let chars: Vec<char> = line.chars().collect();
        let mut out = String::with_capacity(line.len());
        let mut i = 0;
        while i < chars.len() {
            match chars[i] {
                '*' if chars.get(i + 1) == Some(&'*') => i += 2,
                '_' if chars.get(i + 1) == Some(&'_') => i += 2,
                '`' => i += 1,
                '[' => {
                    let close = chars[i + 1..].iter().position(|&c| c == ']');
                    let parsed = close.and_then(|offset| {
                        let close = i + 1 + offset;
                        if chars.get(close + 1) == Some(&'(') {
                            let end = chars[close + 2..].iter().position(|&c| c == ')');
                            end.map(|len| {
                                let text: String = chars[i + 1..close].iter().collect();
                                let url: String =
                                    chars[close + 2..close + 2 + len].iter().collect();
                                (close + 2 + len + 1, text, url)
                            })
                        } else {
                            None
                        }
                    });
                    match parsed {
                        Some((next, text, url)) => {
                            out.push_str(&text);
                            if !url.is_empty() && url != text {
                                out.push_str(" (");
                                out.push_str(&url);
                                out.push(')');
                            }
                            i = next;
                        }
                        None => {
                            out.push('[');
                            i += 1;
                        }
                    }
                }
                c => {
                    out.push(c);
                    i += 1;
                }
            }
        }
        out
    }

    /// 手挑的边界：嵌套、未闭合、空 URL、URL 与文本相同、中文、末尾截断。
    #[test]
    fn the_rewrite_matches_the_previous_implementation_on_edge_cases() {
        let cases = [
            "",
            "[",
            "]",
            "[]",
            "[]()",
            "[a](b)",
            "[a](a)",
            "[a]()",
            "[a](",
            "[a]b",
            "[[a]](b)",
            "[[[[[[",
            "[a][b][c]",
            "[中文](https://例子.测试)",
            "**粗**_斜_`码`[链接](u)",
            "prefix [a](b) middle [c](d) suffix",
            "[未闭合 [又一个 [还有",
            "[a](b))(",
            "```[a](b)```",
        ];
        for case in cases {
            assert_eq!(
                strip_inline_markup(case),
                reference_strip_inline_markup(case),
                "输入：{case:?}"
            );
        }
    }

    /// 用确定性的伪随机组合生成上万条输入，逐条比对——手挑的用例覆盖不到
    /// 「游标恰好停在某处」这类内部状态。
    #[test]
    fn the_rewrite_matches_the_previous_implementation_on_generated_input() {
        let alphabet = ['[', ']', '(', ')', '*', '_', '`', 'a', '中', ' '];
        // 线性同余，固定种子=可复现
        let mut state: u64 = 0x5DEE_CE66_D;
        let mut next = || {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1);
            (state >> 33) as usize
        };
        for _ in 0..5_000 {
            let len = next() % 40;
            let input: String = (0..len)
                .map(|_| alphabet[next() % alphabet.len()])
                .collect();
            assert_eq!(
                strip_inline_markup(&input),
                reference_strip_inline_markup(&input),
                "输入：{input:?}"
            );
        }
    }
}

#[cfg(test)]
mod scaling_probe {
    use super::*;
    use std::time::Instant;

    /// 测量伸缩性，不测绝对耗时——绝对值跟机器走，倍率不跟。
    ///
    /// 用 `cargo test --lib scaling_probe -- --ignored --nocapture` 手工跑。
    /// 平时不进测试套：它是量尺，不是断言。
    fn elapsed_ms(input: &str) -> f64 {
        let start = Instant::now();
        let out = strip_inline_markup(input);
        std::hint::black_box(out);
        start.elapsed().as_secs_f64() * 1000.0
    }

    #[test]
    #[ignore]
    fn strip_inline_markup_scaling() {
        println!("\n  输入长度      耗时(ms)   相对 2× 前");
        let mut previous: Option<f64> = None;
        for k in [1_000usize, 2_000, 4_000, 8_000, 16_000] {
            // 未匹配的 `[`:每个都会触发一次「往后找 `]`」的全量扫描
            let input = "[".repeat(k);
            let ms = elapsed_ms(&input);
            let ratio = previous.map(|p| ms / p).unwrap_or(f64::NAN);
            println!("  {k:>8}    {ms:>9.3}   {ratio:>8.2}×");
            previous = Some(ms);
        }
        println!("  （线性≈2×，二次方≈4×）");
    }
}
