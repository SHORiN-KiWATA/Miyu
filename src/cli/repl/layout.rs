//! 输入区的行数与光标位置计算。
//!
//! 所有函数都有一个 `_for_cols` 版本：终端宽度作为参数传进来而不是现查，这样
//! 才能对着固定宽度写测试。软换行、双宽字符、多行输入叠在一起时，「光标在第几
//! 行第几列」是纯计算，不该依赖真终端。

use crate::cli::*;

pub(in crate::cli) const REPL_MAX_VISIBLE_INPUT_ROWS: u16 = 12;

pub(in crate::cli) fn terminal_frame_layout(
    frame: &[u8],
    start: (u16, u16),
    columns: u16,
    bottom_margin: Option<u16>,
) -> TerminalFrameLayout {
    terminal_frame_layout_with_scrolls(frame, start, columns, bottom_margin).0
}

/// 同 `terminal_frame_layout`,另外按顺序给出帧里每一次顶到页底的滚动
/// 发生在哪个字节偏移(见 `FrameScroll`)。逐字节喂,追踪器才知道当前字节
/// 的偏移;vte 的解析器本来就是字节状态机,跨调用切开 UTF-8 也没问题。
pub(in crate::cli) fn terminal_frame_layout_with_scrolls(
    frame: &[u8],
    start: (u16, u16),
    columns: u16,
    bottom_margin: Option<u16>,
) -> (TerminalFrameLayout, Vec<FrameScroll>) {
    let mut parser = VteParser::new();
    let mut tracker = TerminalFrameTracker::new(start, columns, bottom_margin);
    for (index, byte) in frame.iter().enumerate() {
        tracker.byte_index = index.saturating_add(1);
        parser.advance(&mut tracker, std::slice::from_ref(byte));
    }
    tracker.byte_index = frame.len();
    tracker.finish_with_scrolls()
}

#[derive(Clone, Copy)]
pub(in crate::cli) enum CursorAfterUpdate {
    Preserve,
    Shown,
    Hidden,
}

/// 输入区太长时折成「首行 + 已隐藏 N 行 + 末行」。
///
/// `raw_pasted_lines` 是当前缓冲区里由**生粘贴**(没被折成占位符的那种)带进来
/// 的行数。判据原本是"上一个动作是不是粘贴",于是在已经敲了十几行之后再粘一
/// 小段,整个输入框——连同用户自己敲的内容——都会被折起来,随便再打个字又恢复
/// (08-26 用户实测)。折叠的本意是挡住粘进来的大段原文,不是藏用户自己写的
/// 东西,所以要求生粘贴内容确实占满了缓冲区才折。
pub(in crate::cli) fn repl_visible_input_lines(
    prefix: &str,
    lines: &[String],
    max_rows: u16,
    raw_pasted_lines: usize,
) -> Vec<String> {
    let total_rows = repl_prompt_rows(prefix, lines);
    let dominated_by_paste = raw_pasted_lines >= lines.len().saturating_sub(2);
    if total_rows <= max_rows || lines.len() <= 2 || raw_pasted_lines == 0 || !dominated_by_paste {
        return lines.to_vec();
    }

    let omitted_lines = lines.len().saturating_sub(2);
    let omitted = if is_zh() {
        format!("... 已隐藏 {omitted_lines} 行粘贴内容 ...")
    } else {
        format!("... {omitted_lines} pasted lines hidden ...")
    };
    vec![lines[0].clone(), omitted, lines[lines.len() - 1].clone()]
}

pub(in crate::cli) fn ensure_repl_space(
    stdout: &mut io::Stdout,
    input_row: &mut u16,
    needed_rows: u16,
) -> Result<()> {
    let (_, term_rows) = terminal::size().unwrap_or((80, 24));
    let term_rows = term_rows.max(1);
    if (*input_row).saturating_add(needed_rows) < term_rows {
        return Ok(());
    }
    let overflow = (*input_row)
        .saturating_add(needed_rows)
        .saturating_sub(term_rows.saturating_sub(1));
    queue!(stdout, MoveTo(0, term_rows.saturating_sub(1)))?;
    for _ in 0..overflow {
        queue!(stdout, Print("\n"))?;
    }
    *input_row = (*input_row).saturating_sub(overflow);
    Ok(())
}

pub(in crate::cli) fn submitted_echo_lines(
    mode: AgentMode,
    input: &str,
    cols: usize,
) -> Vec<String> {
    let max_text_width = cols.saturating_sub(3).max(1);
    let bar = submitted_echo_bar(mode);
    let mut output = Vec::new();
    output.push(bar.clone());
    for line in input.split('\n') {
        let mut chunks = wrap_visible_width(line, max_text_width);
        if chunks.is_empty() {
            chunks.push(String::new());
        }
        for chunk in chunks {
            output.push(format!("{bar} {}", colorize_repl_placeholders(&chunk)));
        }
    }
    output.push(bar);
    output
}

pub(in crate::cli) fn submitted_echo_bar(mode: AgentMode) -> String {
    match mode {
        AgentMode::Normal => "\x1b[1m\x1b[34m┃\x1b[0m".to_string(),
        // 与 footer 模式标签同为 tertiary(35 酒红),整条 dev 视觉一致。
        AgentMode::Dev => "\x1b[1m\x1b[35m┃\x1b[0m".to_string(),
    }
}

pub(in crate::cli) fn input_prompt_bar(mode: AgentMode) -> String {
    format!("{} ", submitted_echo_bar(mode))
}

pub(in crate::cli) fn repl_shortcut_hint_line(mode: AgentMode, cols: usize) -> String {
    let bar = input_prompt_bar(mode);
    let text = t(
        "Shift+Enter newline; Ctrl+J newline; Ctrl+V paste clipboard",
        "Shift+Enter 换行；Ctrl+J 换行；Ctrl+V 粘贴剪贴板",
    );
    let text_width = cols.saturating_sub(visible_width(&bar)).max(1);
    format!(
        "{bar}\x1b[2m{}\x1b[0m",
        truncate_visible_width(text, text_width)
    )
}

pub(in crate::cli) fn repl_wrapped_input_rows_for_cols(
    prefix: &str,
    lines: &[String],
    cols: usize,
) -> Vec<String> {
    let max_width = repl_content_width_for_cols(prefix, cols);
    let mut rows = Vec::new();
    for line in lines {
        let mut current = String::new();
        let mut width = 0usize;
        for ch in line.chars() {
            let char_width = visible_width(&ch.to_string());
            if width > 0 && width.saturating_add(char_width) > max_width {
                rows.push(std::mem::take(&mut current));
                width = 0;
            }
            current.push(ch);
            width = width.saturating_add(char_width);
        }
        rows.push(current);
        if width > 0 && width % max_width == 0 {
            rows.push(String::new());
        }
    }
    if rows.is_empty() {
        rows.push(String::new());
    }
    rows
}

pub(in crate::cli) fn repl_cursor_position_for_line_for_cols(
    prefix: &str,
    line: &str,
    cursor: usize,
    cols: usize,
) -> (u16, u16) {
    let cols = cols.max(1);
    let prefix_width = repl_prefix_width_for_cols(prefix, cols);
    let content_width = repl_content_width_for_cols(prefix, cols);
    let mut col = 0usize;
    let mut row = 0usize;
    for ch in line.chars().take(cursor) {
        let char_width = visible_width(&ch.to_string()).max(1);
        if col > 0 && col.saturating_add(char_width) > content_width {
            row = row.saturating_add(1);
            col = 0;
        }
        col = col.saturating_add(char_width);
        if col >= content_width {
            row = row.saturating_add(1);
            col = 0;
        }
    }
    (
        prefix_width.saturating_add(col).min(u16::MAX as usize) as u16,
        row.min(u16::MAX as usize) as u16,
    )
}

pub(in crate::cli) fn repl_move_cursor_vertical(
    prefix: &str,
    input: &str,
    cursor: usize,
    direction: i32,
) -> usize {
    if input.is_empty() || direction == 0 {
        return cursor.min(input.chars().count());
    }
    repl_move_cursor_vertical_for_cols(prefix, input, cursor, direction, terminal_cols())
}

pub(in crate::cli) fn repl_move_cursor_vertical_for_cols(
    prefix: &str,
    input: &str,
    cursor: usize,
    direction: i32,
    cols: usize,
) -> usize {
    let positions = repl_cursor_layout_positions_for_cols(prefix, input, cols);
    let cursor = cursor.min(positions.len().saturating_sub(1));
    let (_, current_row, current_col) = positions[cursor];
    let last_row = positions.last().map(|(_, row, _)| *row).unwrap_or(0);
    let target_row = if direction < 0 {
        current_row.saturating_sub(1)
    } else {
        current_row.saturating_add(1).min(last_row)
    };
    if target_row == current_row {
        return cursor;
    }

    positions
        .iter()
        .filter(|(_, row, _)| *row == target_row)
        .min_by_key(|(index, _, col)| (col.abs_diff(current_col), usize::MAX - *index))
        .map(|(index, _, _)| *index)
        .unwrap_or(cursor)
}

pub(in crate::cli) fn repl_cursor_layout_positions_for_cols(
    prefix: &str,
    input: &str,
    cols: usize,
) -> Vec<(usize, usize, usize)> {
    let content_width = repl_content_width_for_cols(prefix, cols);
    let mut positions = Vec::with_capacity(input.chars().count() + 1);
    let mut row = 0usize;
    let mut col = 0usize;
    positions.push((0, row, col));
    for (index, ch) in input.chars().enumerate() {
        if ch == '\n' {
            row = row.saturating_add(1);
            col = 0;
            positions.push((index + 1, row, col));
            continue;
        }
        let char_width = visible_width(&ch.to_string()).max(1);
        if col > 0 && col.saturating_add(char_width) > content_width {
            row = row.saturating_add(1);
            col = 0;
        }
        col = col.saturating_add(char_width);
        if col >= content_width {
            row = row.saturating_add(1);
            col = 0;
        }
        positions.push((index + 1, row, col));
    }
    positions
}

pub(in crate::cli) fn repl_prompt_rows(prefix: &str, lines: &[String]) -> u16 {
    repl_prompt_rows_for_cols(prefix, lines, terminal_cols())
}

pub(in crate::cli) fn repl_cursor_position(prefix: &str, input: &str, cursor: usize) -> (u16, u16) {
    repl_cursor_position_for_cols(prefix, input, cursor, terminal_cols())
}

pub(in crate::cli) fn repl_line_rows_for_cols(prefix: &str, line: &str, cols: usize) -> u16 {
    let content_width = repl_content_width_for_cols(prefix, cols);
    let width = visible_width(line);
    (width / content_width + 1).min(u16::MAX as usize) as u16
}

pub(in crate::cli) fn repl_prefix_width_for_cols(prefix: &str, cols: usize) -> usize {
    visible_width(prefix).min(cols.max(1).saturating_sub(1))
}

pub(in crate::cli) fn repl_content_width_for_cols(prefix: &str, cols: usize) -> usize {
    cols.max(1)
        .saturating_sub(repl_prefix_width_for_cols(prefix, cols))
        .max(1)
}

pub(in crate::cli) fn repl_prompt_rows_for_cols(
    prefix: &str,
    lines: &[String],
    cols: usize,
) -> u16 {
    let cols = cols.max(1);
    let mut rows = 0usize;
    for line in lines {
        rows += repl_line_rows_for_cols(prefix, line, cols) as usize;
    }
    rows.max(1).min(u16::MAX as usize) as u16
}

pub(in crate::cli) fn repl_cursor_position_for_cols(
    prefix: &str,
    input: &str,
    cursor: usize,
    cols: usize,
) -> (u16, u16) {
    let cols = cols.max(1);
    let before_cursor = take_chars(input, cursor);
    let lines = repl_input_lines(&before_cursor);
    let last_index = lines.len().saturating_sub(1);
    let mut row_offset = 0usize;
    for (index, line) in lines.iter().enumerate() {
        if index == last_index {
            let (col, row) =
                repl_cursor_position_for_line_for_cols(prefix, line, line.chars().count(), cols);
            return (
                col,
                row_offset
                    .saturating_add(row as usize)
                    .min(u16::MAX as usize) as u16,
            );
        }
        row_offset += repl_line_rows_for_cols(prefix, line, cols) as usize;
    }
    (
        repl_prefix_width_for_cols(prefix, cols).min(u16::MAX as usize) as u16,
        0,
    )
}

pub(in crate::cli) fn insert_char_at_cursor(value: &mut String, cursor: &mut usize, ch: char) {
    let byte_index = byte_index_for_char(value, *cursor);
    value.insert(byte_index, ch);
    *cursor += 1;
}

pub(in crate::cli) fn insert_str_at_cursor(value: &mut String, cursor: &mut usize, text: &str) {
    let byte_index = byte_index_for_char(value, *cursor);
    value.insert_str(byte_index, text);
    *cursor += text.chars().count();
}

pub(in crate::cli) fn insert_newline_at_cursor(value: &mut String, cursor: &mut usize) {
    insert_char_at_cursor(value, cursor, '\n');
}

pub(in crate::cli) fn remove_char_before_cursor(value: &mut String, cursor: &mut usize) {
    let end = byte_index_for_char(value, *cursor);
    let start = byte_index_for_char(value, cursor.saturating_sub(1));
    value.replace_range(start..end, "");
    *cursor -= 1;
}

/// Ctrl+Left 的落点：先跨过光标左边的连续空白，再跨过一整个词。
///
/// 分词只按空白，不按标点——第一版刻意保守：`remove_word_before_cursor`
/// （Ctrl+W）用的就是这套语义，两个键的「词」必须是同一个词，否则
/// Ctrl+Left 再 Ctrl+W 会删掉和你看到的不一样的东西。
///
/// 落点若掉进 `[Image N: ...]` 占位符中段（占位符自带空格，按空白分词一定会
/// 切进去），整块跳到它的**开头**——占位符在编辑器里是一个不可分割的字符。
pub(in crate::cli) fn word_start_before_cursor(value: &str, cursor: usize) -> usize {
    let chars = value.chars().collect::<Vec<_>>();
    let mut start = cursor.min(chars.len());
    while start > 0 && chars[start - 1].is_whitespace() {
        start -= 1;
    }
    while start > 0 && !chars[start - 1].is_whitespace() {
        start -= 1;
    }
    match placeholder_at_cursor(value, start) {
        Some((placeholder_start, _)) => placeholder_start,
        None => start,
    }
}

/// Ctrl+Right 的落点：先跨过光标右边的连续空白，再跨过一整个词。
/// 占位符处理同 [`word_start_before_cursor`]，只是snap 到**结尾**。
pub(in crate::cli) fn word_end_after_cursor(value: &str, cursor: usize) -> usize {
    let chars = value.chars().collect::<Vec<_>>();
    let mut end = cursor.min(chars.len());
    while end < chars.len() && chars[end].is_whitespace() {
        end += 1;
    }
    while end < chars.len() && !chars[end].is_whitespace() {
        end += 1;
    }
    match placeholder_at_cursor(value, end) {
        Some((_, placeholder_end)) => placeholder_end,
        None => end,
    }
}

pub(in crate::cli) fn remove_char_at_cursor(value: &mut String, cursor: usize) {
    if cursor >= value.chars().count() {
        return;
    }
    let start = byte_index_for_char(value, cursor);
    let end = byte_index_for_char(value, cursor + 1);
    value.replace_range(start..end, "");
}

pub(in crate::cli) fn byte_index_for_char(value: &str, char_index: usize) -> usize {
    value
        .char_indices()
        .nth(char_index)
        .map(|(index, _)| index)
        .unwrap_or(value.len())
}

pub(in crate::cli) fn take_chars(value: &str, count: usize) -> String {
    value.chars().take(count).collect()
}

pub(in crate::cli) fn terminal_cols() -> usize {
    terminal::size()
        .map(|(cols, _)| cols.max(1) as usize)
        .unwrap_or(80)
}

pub(in crate::cli) fn strip_terminal_control_sequences(input: &str) -> String {
    let mut output = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\x1b' {
            if chars.peek() == Some(&'[') {
                chars.next();
                for next in chars.by_ref() {
                    if ('@'..='~').contains(&next) {
                        break;
                    }
                }
            } else {
                chars.next();
            }
            continue;
        }
        if is_disallowed_control_char(ch) {
            continue;
        }
        output.push(ch);
    }
    output
}

pub(in crate::cli) fn is_disallowed_control_char(ch: char) -> bool {
    ch.is_control() && !matches!(ch, '\n' | '\t')
}
