use crate::render::style::{RESET, TABLE_BORDER_STYLE};
use crossterm::terminal;

const TOP_LEFT: char = '┌';
const TOP_MID: char = '┬';
const TOP_RIGHT: char = '┐';
const MID_LEFT: char = '├';
const MID_MID: char = '┼';
const MID_RIGHT: char = '┤';
const BOTTOM_LEFT: char = '└';
const BOTTOM_MID: char = '┴';
const BOTTOM_RIGHT: char = '┘';
const HORIZONTAL: char = '─';
const VERTICAL: char = '│';

#[derive(Clone, Copy)]
pub(crate) enum TableAlign {
    Left,
    Center,
    Right,
}

/// 渲染完整 Markdown 表格。
///
/// 参数:
/// - `lines`: 表格原始行
/// - `render_cell`: 单元格内联渲染函数
///
/// 返回:
/// - 带细实线边框的终端表格文本
pub(crate) fn render_table<F>(lines: &[String], render_cell: F) -> String
where
    F: Fn(&str) -> String + Copy,
{
    let alignments = lines
        .get(1)
        .filter(|line| is_table_separator(line))
        .map(|line| parse_table_alignments(line))
        .unwrap_or_default();
    let rows = lines
        .iter()
        .filter(|line| !is_table_separator(line))
        .map(|line| parse_table_row(line, render_cell))
        .collect::<Vec<_>>();
    let cols = rows.iter().map(Vec::len).max().unwrap_or(0);
    let mut widths = vec![0usize; cols];
    for row in &rows {
        for (index, cell) in row.iter().enumerate() {
            widths[index] = widths[index].max(visible_width(cell));
        }
    }
    widths = bounded_table_widths(widths);

    let mut output = String::new();
    output.push_str(&top_border(&widths));
    for (row_index, row) in rows.iter().enumerate() {
        output.push_str(&render_table_row(row, &widths, &alignments, row_index == 0));
        if row_index == 0 {
            output.push_str(&middle_border(&widths));
        }
    }
    output.push_str(&bottom_border(&widths));
    output
}

/// 判断一行是否为 Markdown 表格分隔行。
///
/// 参数:
/// - `line`: 原始行
///
/// 返回:
/// - 是否为表格分隔行
pub(crate) fn is_table_separator(line: &str) -> bool {
    let trimmed = line.trim().trim_matches('|').trim();
    !trimmed.is_empty()
        && trimmed
            .chars()
            .all(|ch| matches!(ch, '-' | ':' | '|' | ' '))
        && trimmed.contains('-')
}

/// 判断一行是否像 Markdown 表格行。
///
/// 参数:
/// - `line`: 原始行
///
/// 返回:
/// - 是否满足表格行基本形态
pub(crate) fn looks_like_table_row(line: &str) -> bool {
    let trimmed = line.trim();
    trimmed.starts_with('|') && trimmed.ends_with('|') && trimmed.matches('|').count() >= 2
}

/// 解析表格数据行。
///
/// 参数:
/// - `line`: 原始表格行
/// - `render_cell`: 单元格内联渲染函数
///
/// 返回:
/// - 已渲染的单元格列表
pub(crate) fn parse_table_row<F>(line: &str, render_cell: F) -> Vec<String>
where
    F: Fn(&str) -> String,
{
    line.trim()
        .trim_matches('|')
        .split('|')
        .map(|cell| render_cell(cell.trim()))
        .collect()
}

/// 解析表格对齐标记。
///
/// 参数:
/// - `line`: Markdown 表格分隔行
///
/// 返回:
/// - 每列的对齐方式
pub(crate) fn parse_table_alignments(line: &str) -> Vec<TableAlign> {
    line.trim()
        .trim_matches('|')
        .split('|')
        .map(|cell| {
            let cell = cell.trim();
            match (cell.starts_with(':'), cell.ends_with(':')) {
                (true, true) => TableAlign::Center,
                (false, true) => TableAlign::Right,
                _ => TableAlign::Left,
            }
        })
        .collect()
}

/// 根据列数量计算初始表格宽度。
///
/// 参数:
/// - `cols`: 列数量
///
/// 返回:
/// - 每列的目标宽度
pub(crate) fn bounded_table_widths_for_cols(cols: usize) -> Vec<usize> {
    if cols == 0 {
        return Vec::new();
    }
    let terminal_width = terminal::size()
        .map(|(width, _)| usize::from(width))
        .unwrap_or(100)
        .saturating_sub(1)
        .max(20);
    let border_overhead = cols.saturating_mul(3).saturating_add(1);
    let available = terminal_width.saturating_sub(border_overhead).max(cols);
    let base = (available / cols).max(1);
    let mut widths = vec![base; cols];
    for width in widths.iter_mut().take(available % cols) {
        *width += 1;
    }
    widths
}

/// 渲染单个表格数据行。
///
/// 参数:
/// - `row`: 单元格列表
/// - `widths`: 每列宽度
/// - `alignments`: 每列对齐方式
/// - `header`: 是否为表头
///
/// 返回:
/// - 带终端样式的表格行
pub(crate) fn render_table_row(
    row: &[String],
    widths: &[usize],
    alignments: &[TableAlign],
    header: bool,
) -> String {
    let wrapped = widths
        .iter()
        .enumerate()
        .map(|(index, width)| {
            let cell = row.get(index).map(String::as_str).unwrap_or("");
            wrap_ansi_text(cell, *width)
        })
        .collect::<Vec<_>>();
    let row_height = wrapped.iter().map(Vec::len).max().unwrap_or(1);
    let mut output = String::new();
    for line_index in 0..row_height {
        push_vertical(&mut output);
        for (index, width) in widths.iter().enumerate() {
            let cell = wrapped
                .get(index)
                .and_then(|lines| lines.get(line_index))
                .map(String::as_str)
                .unwrap_or("");
            let cell = if header && !cell.is_empty() {
                format!("\x1b[1m{cell}\x1b[0m")
            } else {
                cell.to_string()
            };
            output.push(' ');
            output.push_str(&aligned_cell(
                &cell,
                *width,
                alignments.get(index).copied().unwrap_or(TableAlign::Left),
            ));
            output.push(' ');
            push_vertical(&mut output);
        }
        output.push('\n');
    }
    output
}

/// 渲染表格顶部边框。
///
/// 参数:
/// - `widths`: 每列宽度
///
/// 返回:
/// - 顶部边框文本
pub(crate) fn top_border(widths: &[usize]) -> String {
    table_border(widths, TOP_LEFT, TOP_MID, TOP_RIGHT)
}

/// 渲染表格中部分隔边框。
///
/// 参数:
/// - `widths`: 每列宽度
///
/// 返回:
/// - 中部分隔边框文本
pub(crate) fn middle_border(widths: &[usize]) -> String {
    table_border(widths, MID_LEFT, MID_MID, MID_RIGHT)
}

/// 渲染表格底部边框。
///
/// 参数:
/// - `widths`: 每列宽度
///
/// 返回:
/// - 底部边框文本
pub(crate) fn bottom_border(widths: &[usize]) -> String {
    table_border(widths, BOTTOM_LEFT, BOTTOM_MID, BOTTOM_RIGHT)
}

/// 计算去除 ANSI 转义后的显示宽度。
///
/// 参数:
/// - `text`: 原始文本
///
/// 返回:
/// - 终端显示宽度
pub(crate) fn visible_width(text: &str) -> usize {
    let mut width = 0;
    let mut escape = false;
    for ch in text.chars() {
        if ch == '\x1b' {
            escape = true;
        } else if escape {
            if ch == 'm' {
                escape = false;
            }
        } else if (ch as u32) >= 0x2e80 {
            width += 2;
        } else {
            width += 1;
        }
    }
    width
}

/// 将表格宽度限制在终端宽度内。
///
/// 参数:
/// - `widths`: 原始列宽
///
/// 返回:
/// - 调整后的列宽
fn bounded_table_widths(mut widths: Vec<usize>) -> Vec<usize> {
    if widths.is_empty() {
        return widths;
    }
    let terminal_width = terminal::size()
        .map(|(width, _)| usize::from(width))
        .unwrap_or(100)
        .saturating_sub(1)
        .max(20);
    let border_overhead = widths.len().saturating_mul(3).saturating_add(1);
    let available = terminal_width
        .saturating_sub(border_overhead)
        .max(widths.len());
    while widths.iter().sum::<usize>() > available {
        let Some((index, width)) = widths
            .iter()
            .enumerate()
            .max_by_key(|(_, width)| **width)
            .map(|(index, width)| (index, *width))
        else {
            break;
        };
        if width <= 8 {
            break;
        }
        widths[index] -= 1;
    }
    widths
}

/// 将含 ANSI 转义的文本按宽度换行。
///
/// 参数:
/// - `text`: 原始文本
/// - `width`: 目标宽度
///
/// 返回:
/// - 换行后的文本行
fn wrap_ansi_text(text: &str, width: usize) -> Vec<String> {
    if text.is_empty() {
        return vec![String::new()];
    }
    let width = width.max(1);
    let mut lines = Vec::new();
    let mut current = String::new();
    let mut current_width = 0usize;
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\x1b' {
            current.push(ch);
            for next in chars.by_ref() {
                current.push(next);
                if next == 'm' {
                    break;
                }
            }
            continue;
        }
        let ch_width = char_display_width(ch);
        if current_width > 0 && current_width + ch_width > width {
            lines.push(current);
            current = String::new();
            current_width = 0;
        }
        current.push(ch);
        current_width += ch_width;
    }
    lines.push(current);
    lines
}

/// 计算单个字符的显示宽度。
///
/// 参数:
/// - `ch`: 待计算字符
///
/// 返回:
/// - 终端显示宽度
fn char_display_width(ch: char) -> usize {
    if ch.is_ascii() {
        1
    } else {
        2
    }
}

/// 按对齐方式补齐单元格。
///
/// 参数:
/// - `cell`: 单元格内容
/// - `width`: 目标宽度
/// - `align`: 对齐方式
///
/// 返回:
/// - 补齐后的单元格内容
fn aligned_cell(cell: &str, width: usize, align: TableAlign) -> String {
    let padding = width.saturating_sub(visible_width(cell));
    match align {
        TableAlign::Left => format!("{cell}{}", " ".repeat(padding)),
        TableAlign::Right => format!("{}{cell}", " ".repeat(padding)),
        TableAlign::Center => {
            let left = padding / 2;
            let right = padding - left;
            format!("{}{cell}{}", " ".repeat(left), " ".repeat(right))
        }
    }
}

/// 渲染通用表格边框。
///
/// 参数:
/// - `widths`: 每列宽度
/// - `left`: 左侧边框字符
/// - `mid`: 中间分隔字符
/// - `right`: 右侧边框字符
///
/// 返回:
/// - 带终端样式的边框行
fn table_border(widths: &[usize], left: char, mid: char, right: char) -> String {
    let mut output = String::new();
    output.push_str(TABLE_BORDER_STYLE);
    output.push(left);
    for (index, width) in widths.iter().enumerate() {
        output.push_str(&HORIZONTAL.to_string().repeat(width + 2));
        output.push(if index + 1 == widths.len() {
            right
        } else {
            mid
        });
    }
    output.push_str(RESET);
    output.push('\n');
    output
}

/// 写入表格竖线。
///
/// 参数:
/// - `output`: 输出缓冲区
fn push_vertical(output: &mut String) {
    output.push_str(TABLE_BORDER_STYLE);
    output.push(VERTICAL);
    output.push_str(RESET);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render::markdown::render_inline;

    #[test]
    fn table_uses_thin_box_borders() {
        let output = render_table(
            &[
                "| a | b |".to_string(),
                "| - | - |".to_string(),
                "| 1 | 2 |".to_string(),
            ],
            render_inline,
        );
        assert!(output.contains('┌'));
        assert!(output.contains('┬'));
        assert!(output.contains('├'));
        assert!(output.contains('┼'));
        assert!(output.contains('└'));
        assert!(output.contains('│'));
        assert!(output.contains('─'));
        assert!(!output.contains('+'));
    }

    #[test]
    fn wraps_wide_table_cells_to_terminal_width() {
        let output = render_table(
            &[
                "| 项目 | 内容 |".to_string(),
                "|---|---|".to_string(),
                format!("| 很长 | {} |", "这是一段非常长的内容".repeat(20)),
            ],
            render_inline,
        );
        let terminal_width = terminal::size()
            .map(|(width, _)| usize::from(width))
            .unwrap_or(100);
        for line in output.lines() {
            assert!(
                visible_width(line) < terminal_width,
                "line too wide: {line}"
            );
        }
        assert!(output.lines().count() > 5);
    }
}
