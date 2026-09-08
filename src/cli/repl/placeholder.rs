//! 输入框里的占位符。
//!
//! 粘贴一张图或一大段文字时，输入框里放的是 `[图片 1]` 这样的占位符，真内容存
//! 在旁边。这样长文本不会把输入框撑爆，也不会在每次重绘时重排几千行。
//!
//! 占位符要能被当成**一个整体**编辑：光标经过是整块跳，退格是整块删
//! （`placeholder_before_cursor` 那一族）。半个占位符留在文本里就再也解析不回
//! 去了。

use crate::cli::*;

pub(in crate::cli) const REPL_PASTE_PLACEHOLDER_MIN_LINES: usize = 3;

pub(in crate::cli) const REPL_PASTE_PLACEHOLDER_MIN_CHARS: usize = 150;

#[derive(Clone, Debug)]
pub(in crate::cli) struct PastedText {
    pub(in crate::cli) text: String,
}

pub(in crate::cli) fn extract_image_placeholders(
    message: &str,
) -> (String, Vec<Option<crate::clipboard::PastedImage>>) {
    let placeholders = find_image_placeholders(message);
    if placeholders.is_empty() {
        return (message.to_string(), Vec::new());
    }

    let cache_images_dir = MiyuPaths::new()
        .map(|p| p.cache_dir.join("clipboard_images"))
        .ok();

    let chars: Vec<char> = message.chars().collect();
    let mut clean = String::new();
    let mut images: Vec<Option<crate::clipboard::PastedImage>> = Vec::new();
    let mut last_end = 0;

    for (start, end) in &placeholders {
        clean.extend(&chars[last_end..*start]);
        let segment: String = chars[*start..*end].iter().collect();
        let name_str = media_placeholder_prefix(&segment)
            .and_then(|prefix| segment.strip_prefix(prefix))
            .and_then(|s| {
                // 序号可能是多位数("[Image 10: ...]"),char pattern 的
                // strip_prefix 只剥一位,会把第 10 张起的图静默丢弃。
                let rest = s.trim_start_matches(|c: char| c.is_ascii_digit());
                (rest.len() < s.len()).then_some(rest)
            })
            .and_then(|s| s.strip_prefix(':'))
            .and_then(|s| s.strip_suffix(']'))
            .map(|s| s.trim().to_string());

        if let Some(name_str) = name_str {
            if let Some(dir) = &cache_images_dir {
                let candidate = dir.join(&name_str);
                if candidate.exists() {
                    images.push(Some(crate::clipboard::PastedImage::Path(
                        candidate.display().to_string(),
                    )));
                } else {
                    images.push(None);
                }
            } else {
                images.push(None);
            }
        } else {
            images.push(None);
        }
        clean.push_str(&format!("[Image {}]", images.len()));
        last_end = *end;
    }
    clean.extend(&chars[last_end..]);

    (clean, images)
}

pub(in crate::cli) fn should_summarize_pasted_text(text: &str) -> bool {
    !text.is_empty()
        && (pasted_text_line_count(text) >= REPL_PASTE_PLACEHOLDER_MIN_LINES
            || text.chars().count() > REPL_PASTE_PLACEHOLDER_MIN_CHARS)
}

pub(in crate::cli) fn pasted_text_line_count(text: &str) -> usize {
    if text.is_empty() {
        0
    } else {
        text.chars().filter(|ch| *ch == '\n').count() + 1
    }
}

pub(in crate::cli) fn pasted_text_placeholder(index: usize, line_count: usize) -> String {
    if is_zh() {
        format!("[粘贴 {index}: ~{line_count} 行]")
    } else {
        format!("[Pasted {index}: ~{line_count} lines]")
    }
}

/// 返回**生插入**的行数(被折成占位符时为 0)。
///
/// 调用方拿它判断"输入框里有多少内容是粘进来的原文",输入区的整体折叠只该
/// 在这种内容占满缓冲区时才发生——见 `repl_visible_input_lines`。
pub(in crate::cli) fn insert_pasted_text_at_cursor(
    input: &mut String,
    cursor: &mut usize,
    text: String,
    pasted_texts: &mut Vec<Option<PastedText>>,
) -> usize {
    let text = strip_terminal_control_sequences(&text);
    if should_summarize_pasted_text(&text) {
        let index = pasted_texts.len() + 1;
        let placeholder = pasted_text_placeholder(index, pasted_text_line_count(&text));
        insert_str_at_cursor(input, cursor, &placeholder);
        pasted_texts.push(Some(PastedText { text }));
        0
    } else {
        let lines = pasted_text_line_count(&text);
        insert_str_at_cursor(input, cursor, &text);
        lines
    }
}

pub(in crate::cli) use crate::clipboard::{media_placeholder_prefix, MEDIA_PLACEHOLDER_PREFIXES};

/// 按剪贴板里的文件挑标签。
pub(in crate::cli) fn media_placeholder_label(path: &str) -> &'static str {
    if crate::tools::vision::video_mime(path).is_some() {
        "Video"
    } else if crate::tools::vision::pdf_mime(path).is_some() {
        "PDF"
    } else {
        "Image"
    }
}

pub(in crate::cli) fn find_repl_placeholders(input: &str) -> Vec<(usize, usize)> {
    let mut result = Vec::new();
    let chars: Vec<char> = input.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        // 媒体前缀按各自长度匹配。曾经写死 7("[Image "/"[Video " 恰好都是 7),
        // 加一个更短的前缀就会整条漏掉——`[PDF ` 只有 5 位。
        let prefix_len = if let Some(prefix) = MEDIA_PLACEHOLDER_PREFIXES.iter().find(|prefix| {
            let len = prefix.chars().count();
            i + len <= chars.len() && chars[i..i + len].iter().collect::<String>() == **prefix
        }) {
            Some(prefix.chars().count())
        } else if i + 8 <= chars.len() && chars[i..i + 8].iter().collect::<String>() == "[Pasted " {
            Some(8)
        } else if i + 4 <= chars.len() && chars[i..i + 4].iter().collect::<String>() == "[粘贴 " {
            Some(4)
        } else {
            None
        };

        if let Some(prefix_len) = prefix_len {
            let mut j = i + prefix_len;
            while j < chars.len() && chars[j].is_ascii_digit() {
                j += 1;
            }
            if j < chars.len() && chars[j] == ':' {
                j += 1;
                while j < chars.len() && chars[j] != ']' {
                    j += 1;
                }
                if j < chars.len() && chars[j] == ']' {
                    result.push((i, j + 1));
                    i = j + 1;
                    continue;
                }
            } else if j < chars.len() && chars[j] == ']' {
                result.push((i, j + 1));
                i = j + 1;
                continue;
            }
        }
        i += 1;
    }
    result
}

pub(in crate::cli) fn find_image_placeholders(input: &str) -> Vec<(usize, usize)> {
    find_repl_placeholders(input)
        .into_iter()
        .filter(|(start, end)| parse_image_placeholder_index(input, *start, *end).is_some())
        .collect()
}

pub(in crate::cli) fn find_pasted_text_placeholders(input: &str) -> Vec<(usize, usize, usize)> {
    find_repl_placeholders(input)
        .into_iter()
        .filter_map(|(start, end)| {
            parse_pasted_text_placeholder_index(input, start, end).map(|index| (start, end, index))
        })
        .collect()
}

pub(in crate::cli) fn placeholder_at_cursor(input: &str, cursor: usize) -> Option<(usize, usize)> {
    let placeholders = find_repl_placeholders(input);
    for (start, end) in &placeholders {
        if cursor > *start && cursor < *end {
            return Some((*start, *end));
        }
    }
    None
}

pub(in crate::cli) fn placeholder_before_cursor(
    input: &str,
    cursor: usize,
) -> Option<(usize, usize)> {
    let placeholders = find_repl_placeholders(input);
    for (start, end) in &placeholders {
        if *end == cursor {
            return Some((*start, *end));
        }
    }
    None
}

pub(in crate::cli) fn placeholder_before_or_at_cursor(
    input: &str,
    cursor: usize,
) -> Option<(usize, usize)> {
    placeholder_at_cursor(input, cursor).or_else(|| placeholder_before_cursor(input, cursor))
}

pub(in crate::cli) fn placeholder_after_cursor(
    input: &str,
    cursor: usize,
) -> Option<(usize, usize)> {
    let placeholders = find_repl_placeholders(input);
    for (start, end) in &placeholders {
        if *start == cursor {
            return Some((*start, *end));
        }
    }
    None
}

pub(in crate::cli) fn placeholder_after_or_at_cursor(
    input: &str,
    cursor: usize,
) -> Option<(usize, usize)> {
    placeholder_at_cursor(input, cursor).or_else(|| placeholder_after_cursor(input, cursor))
}

pub(in crate::cli) fn remove_range_chars(value: &mut String, char_start: usize, char_end: usize) {
    let byte_start = byte_index_for_char(value, char_start);
    let byte_end = byte_index_for_char(value, char_end);
    value.replace_range(byte_start..byte_end, "");
}

pub(in crate::cli) fn parse_image_placeholder_index(
    input: &str,
    char_start: usize,
    char_end: usize,
) -> Option<usize> {
    let chars: Vec<char> = input.chars().collect();
    let segment: String = chars[char_start..char_end].iter().collect();
    let after_prefix = segment.strip_prefix(media_placeholder_prefix(&segment)?)?;
    let num_str: String = after_prefix
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect();
    num_str.parse::<usize>().ok()
}

pub(in crate::cli) fn parse_pasted_text_placeholder_index(
    input: &str,
    char_start: usize,
    char_end: usize,
) -> Option<usize> {
    let chars: Vec<char> = input.chars().collect();
    let segment: String = chars[char_start..char_end].iter().collect();
    let after_prefix = segment
        .strip_prefix("[Pasted ")
        .or_else(|| segment.strip_prefix("[粘贴 "))?;
    let num_str: String = after_prefix
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect();
    num_str.parse::<usize>().ok()
}

pub(in crate::cli) fn clear_placeholder_payload(
    input: &str,
    start: usize,
    end: usize,
    pasted_images: &mut [Option<crate::clipboard::PastedImage>],
    pasted_texts: &mut [Option<PastedText>],
) {
    if let Some(n) = parse_image_placeholder_index(input, start, end) {
        if n > 0 && n <= pasted_images.len() {
            pasted_images[n - 1] = None;
        }
    }
    if let Some(n) = parse_pasted_text_placeholder_index(input, start, end) {
        if n > 0 && n <= pasted_texts.len() {
            pasted_texts[n - 1] = None;
        }
    }
}

/// Ctrl+W：往前删一个词，占位符整块删并清掉它的载荷。
///
/// 这里收了两个坑。
///
/// 一、**隔着空白也要整块删**。以前只在光标**紧贴**占位符时走整块删除，否则
/// 退回按空白分词。但占位符自带空格（`[Pasted 1: ~3 lines]`），光标隔着一个
/// 空格按 Ctrl+W，分词就切进它中段：屏幕上留下 `[Pasted 1: ~3`，再也解析不
/// 回去，而载荷还挂在数组里，提交时被 `expand_pasted_text_placeholders`
/// 原样展开——用户看到的和发出去的对不上。
///
/// 二、**两个调用点必须同一份逻辑**。live 编辑器和旧输入路径原先各写了一份
/// 一模一样的分支，改一边漏一边就是行为分叉（AGENTS.md 2.4）。
pub(in crate::cli) fn remove_word_before_cursor(
    input: &mut String,
    cursor: &mut usize,
    pasted_images: &mut [Option<crate::clipboard::PastedImage>],
    pasted_texts: &mut [Option<PastedText>],
) {
    if *cursor == 0 {
        return;
    }
    // 光标落在占位符内部或紧贴其后时，连它**后半截**一起删——只删到光标会留残片。
    // 其余情况按词退，词首落进占位符中段时 `word_start_before_cursor` 会退到它开头。
    let (start, end) = match placeholder_before_or_at_cursor(input, *cursor) {
        Some(range) => range,
        None => (word_start_before_cursor(input, *cursor), *cursor),
    };
    // 一次 Ctrl+W 可能吞掉不止一个（`[Image 1][Image 2]` 中间没有空白）。
    for (placeholder_start, placeholder_end) in find_repl_placeholders(input) {
        if placeholder_start >= start && placeholder_end <= end {
            clear_placeholder_payload(
                input,
                placeholder_start,
                placeholder_end,
                pasted_images,
                pasted_texts,
            );
        }
    }
    remove_range_chars(input, start, end);
    *cursor = start;
}

pub(in crate::cli) fn expand_pasted_text_placeholders(
    input: &str,
    pasted_texts: &[Option<PastedText>],
) -> String {
    let placeholders = find_pasted_text_placeholders(input);
    if placeholders.is_empty() {
        return input.to_string();
    }

    let chars: Vec<char> = input.chars().collect();
    let mut expanded = String::new();
    let mut last_end = 0;
    for (start, end, index) in placeholders {
        expanded.extend(&chars[last_end..start]);
        if index > 0 {
            if let Some(Some(pasted_text)) = pasted_texts.get(index - 1) {
                expanded.push_str(&pasted_text.text);
            } else {
                expanded.extend(&chars[start..end]);
            }
        } else {
            expanded.extend(&chars[start..end]);
        }
        last_end = end;
    }
    expanded.extend(&chars[last_end..]);
    expanded
}

pub(in crate::cli) fn placeholder_text_near_cursor(
    input: &str,
    cursor: usize,
    pasted_texts: &[Option<PastedText>],
) -> Option<String> {
    let (start, end) = placeholder_at_cursor(input, cursor)
        .or_else(|| placeholder_before_cursor(input, cursor))
        .or_else(|| placeholder_after_cursor(input, cursor))?;
    let index = parse_pasted_text_placeholder_index(input, start, end)?;
    pasted_texts
        .get(index.checked_sub(1)?)
        .and_then(Option::as_ref)
        .map(|pasted_text| pasted_text.text.clone())
}

pub(in crate::cli) fn colorize_repl_placeholders(line: &str) -> String {
    let placeholders = find_repl_placeholders(line);
    if placeholders.is_empty() {
        return line.to_string();
    }

    let chars: Vec<char> = line.chars().collect();
    let mut result = String::new();
    let mut last_end = 0;
    for (start, end) in placeholders {
        result.extend(&chars[last_end..start]);
        result.push_str("\x1b[35m");
        result.extend(&chars[start..end]);
        result.push_str("\x1b[0m");
        last_end = end;
    }
    result.extend(&chars[last_end..]);
    result
}
