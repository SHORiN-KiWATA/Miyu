use crate::render::asset_block;
use crate::render::code_block::render_code_block;
use crate::render::style::{
    BOLD_STYLE, HEADER_STYLE, IMAGE_STYLE, INLINE_CODE_STYLE, ITALIC_STYLE, LINK_LABEL_STYLE,
    RESET, STRIKE_STYLE, TERTIARY_STYLE, URL_STYLE,
};
use crate::render::table::{self, TableAlign};
use crossterm::terminal;

pub(crate) struct MarkdownStreamRenderer {
    buffer: String,
    line_renderer: MarkdownLineRenderer,
}

impl MarkdownStreamRenderer {
    /// 创建流式 Markdown 渲染器。
    ///
    /// 返回:
    /// - 新的流式渲染器
    pub(crate) fn new() -> Self {
        Self {
            buffer: String::new(),
            line_renderer: MarkdownLineRenderer::new(),
        }
    }

    /// 推入流式 Markdown 增量。
    ///
    /// 参数:
    /// - `delta`: 新收到的 Markdown 文本片段
    ///
    /// 返回:
    /// - 已完整渲染的终端文本
    pub(crate) fn push(&mut self, delta: &str) -> String {
        self.buffer.push_str(delta);
        let mut output = String::new();
        while let Some(index) = self.buffer.find('\n') {
            let line = self.buffer[..index].to_string();
            self.buffer = self.buffer[index + 1..].to_string();
            output.push_str(&self.line_renderer.render_line(&line));
        }
        output
    }

    /// 刷新剩余 Markdown 缓冲。
    ///
    /// 返回:
    /// - 最后一段渲染文本
    pub(crate) fn flush(&mut self) -> String {
        let mut output = String::new();
        if !self.buffer.is_empty() {
            let line = std::mem::take(&mut self.buffer);
            output.push_str(&self.line_renderer.render_line(&line));
        }
        output.push_str(&self.line_renderer.flush());
        output
    }
}

struct MarkdownLineRenderer {
    in_code_block: bool,
    in_math_block: bool,
    code_lang: String,
    code_buffer: Vec<String>,
    math_buffer: Vec<String>,
    table_buffer: Vec<String>,
    active_table: Option<ActiveTable>,
}

struct ActiveTable {
    widths: Vec<usize>,
    alignments: Vec<TableAlign>,
}

impl MarkdownLineRenderer {
    /// 创建按行 Markdown 渲染器。
    ///
    /// 返回:
    /// - 新的按行渲染器
    fn new() -> Self {
        Self {
            in_code_block: false,
            in_math_block: false,
            code_lang: String::new(),
            code_buffer: Vec::new(),
            math_buffer: Vec::new(),
            table_buffer: Vec::new(),
            active_table: None,
        }
    }

    /// 渲染单行 Markdown。
    ///
    /// 参数:
    /// - `line`: 单行 Markdown 文本
    ///
    /// 返回:
    /// - 当前可输出的终端文本
    fn render_line(&mut self, line: &str) -> String {
        if line.trim_start().starts_with("```") {
            if self.in_code_block {
                self.in_code_block = false;
                let code = self.render_finished_code_block();
                self.code_lang.clear();
                self.code_buffer.clear();
                return code;
            }
            let pending = self.flush();
            self.in_code_block = true;
            self.code_lang = line
                .trim_start()
                .trim_start_matches('`')
                .split_whitespace()
                .next()
                .unwrap_or_default()
                .to_string();
            self.code_buffer.clear();
            return pending;
        }
        if self.in_code_block {
            self.code_buffer.push(line.to_string());
            return String::new();
        }
        if line.trim() == "$$" {
            if self.in_math_block {
                self.in_math_block = false;
                let output = asset_block::render_math_block(&self.math_buffer);
                self.math_buffer.clear();
                return output;
            }
            let pending = self.flush();
            self.in_math_block = true;
            self.math_buffer.clear();
            return pending;
        }
        if self.in_math_block {
            self.math_buffer.push(line.to_string());
            return String::new();
        }
        if let Some(table) = &self.active_table {
            if table::looks_like_table_row(line) {
                let row = table::parse_table_row(line, render_inline);
                return table::render_table_row(&row, &table.widths, &table.alignments, false);
            }
            let mut output = table::bottom_border(&table.widths);
            self.active_table = None;
            output.push_str(&self.render_line(line));
            return output;
        }
        if table::looks_like_table_row(line) {
            self.table_buffer.push(line.to_string());
            if self.table_buffer.len() < 2 {
                return String::new();
            }
            let first = self.table_buffer.first().cloned().unwrap_or_default();
            let second = self.table_buffer.get(1).cloned().unwrap_or_default();
            if table::is_table_separator(&second) {
                let header = table::parse_table_row(&first, render_inline);
                let alignments = table::parse_table_alignments(&second);
                let widths =
                    table::bounded_table_widths_for_cols(header.len().max(alignments.len()));
                self.table_buffer.clear();
                self.active_table = Some(ActiveTable {
                    widths: widths.clone(),
                    alignments: alignments.clone(),
                });
                let mut output = table::top_border(&widths);
                output.push_str(&table::render_table_row(
                    &header,
                    &widths,
                    &alignments,
                    true,
                ));
                output.push_str(&table::middle_border(&widths));
                return output;
            }
            return self.flush();
        }
        let mut output = self.flush();
        output.push_str(&render_markdown_line(line));
        output.push('\n');
        output
    }

    /// 刷新行级渲染器缓冲。
    ///
    /// 返回:
    /// - 缓冲区渲染结果
    fn flush(&mut self) -> String {
        if self.in_code_block {
            self.in_code_block = false;
            let output = self.render_finished_code_block();
            self.code_lang.clear();
            self.code_buffer.clear();
            return output;
        }
        if self.in_math_block {
            self.in_math_block = false;
            let output = asset_block::render_math_block(&self.math_buffer);
            self.math_buffer.clear();
            return output;
        }
        if let Some(table) = self.active_table.take() {
            return table::bottom_border(&table.widths);
        }
        if self.table_buffer.is_empty() {
            return String::new();
        }
        let lines = std::mem::take(&mut self.table_buffer);
        if lines.len() >= 2
            && table::is_table_separator(lines.get(1).map(String::as_str).unwrap_or(""))
        {
            table::render_table(&lines, render_inline)
        } else {
            let mut output = String::new();
            for line in lines {
                output.push_str(&render_markdown_line(&line));
                output.push('\n');
            }
            output
        }
    }

    /// 渲染已经收集完成的代码块。
    ///
    /// 返回:
    /// - 代码块或资产块渲染结果
    fn render_finished_code_block(&self) -> String {
        if asset_block::is_asset_language(&self.code_lang) {
            asset_block::render_asset_block(&self.code_lang, &self.code_buffer)
        } else {
            render_code_block(&self.code_lang, &self.code_buffer)
        }
    }
}

/// 渲染单行 Markdown 文本。
///
/// 参数:
/// - `line`: 原始 Markdown 行
///
/// 返回:
/// - 渲染后的终端文本，不包含结尾换行
pub(crate) fn render_markdown_line(line: &str) -> String {
    let trimmed = line.trim_start();
    let indent = &line[..line.len() - trimmed.len()];
    if let Some(header) = render_header(trimmed) {
        return header;
    }
    if let Some((depth, rest)) = parse_blockquote(trimmed) {
        let bars = "\x1b[32m| \x1b[0m".repeat(depth);
        return format!("{indent}{bars}\x1b[32m{}\x1b[0m", render_inline(rest));
    }
    if let Some(rest) = trimmed
        .strip_prefix("- ")
        .or_else(|| trimmed.strip_prefix("* "))
        .or_else(|| trimmed.strip_prefix("+ "))
    {
        return format!("{indent}{TERTIARY_STYLE}-{RESET} {}", render_inline(rest));
    }
    let digits = trimmed.chars().take_while(|ch| ch.is_ascii_digit()).count();
    if digits > 0
        && trimmed.as_bytes().get(digits) == Some(&b'.')
        && trimmed.as_bytes().get(digits + 1) == Some(&b' ')
    {
        let marker = &trimmed[..=digits];
        let rest = &trimmed[digits + 2..];
        return format!(
            "{indent}{TERTIARY_STYLE}{marker}{RESET} {}",
            render_inline(rest)
        );
    }
    if is_horizontal_rule(trimmed) {
        return horizontal_rule();
    }
    render_inline(line)
}

/// 渲染 Markdown 行内语法。
///
/// 参数:
/// - `text`: 原始行内文本
///
/// 返回:
/// - 带 ANSI 样式的行内文本
pub(crate) fn render_inline(text: &str) -> String {
    let mut output = String::new();
    let chars = text.chars().collect::<Vec<_>>();
    let mut index = 0;
    while index < chars.len() {
        if index + 1 < chars.len() && chars[index] == '!' && chars[index + 1] == '[' {
            if let Some(label_end) = find_marker(&chars, index + 2, ']') {
                if chars.get(label_end + 1) == Some(&'(') {
                    if let Some(url_end) = find_marker(&chars, label_end + 2, ')') {
                        let alt = chars[index + 2..label_end].iter().collect::<String>();
                        output.push_str(IMAGE_STYLE);
                        output.push_str("[image");
                        if !alt.is_empty() {
                            output.push_str(": ");
                            output.push_str(&alt);
                        }
                        output.push(']');
                        output.push_str(RESET);
                        output.push('(');
                        output.push_str(&render_url(
                            &chars[label_end + 2..url_end].iter().collect::<String>(),
                        ));
                        output.push(')');
                        index = url_end + 1;
                        continue;
                    }
                }
            }
        }
        if chars[index] == '`' {
            if is_line_start_formula_prefix(&output, &chars, index) {
                index += 1;
                continue;
            }
            if let Some(end) = find_marker(&chars, index + 1, '`') {
                output.push_str(INLINE_CODE_STYLE);
                output.extend(chars[index + 1..end].iter());
                output.push_str(RESET);
                index = end + 1;
                continue;
            }
        }
        if chars[index] == '、' && is_line_start_formula_prefix(&output, &chars, index) {
            index += 1;
            continue;
        }
        if index + 1 < chars.len() && chars[index] == '$' && chars[index + 1] == '$' {
            if let Some(end) = find_double_marker(&chars, index + 2, '$') {
                let formula = chars[index + 2..end].iter().collect::<String>();
                output.push_str(&asset_block::render_inline_math(&formula));
                index = end + 2;
                continue;
            }
        }
        if chars[index] == '$' {
            if let Some(end) = find_marker(&chars, index + 1, '$') {
                let formula = chars[index + 1..end].iter().collect::<String>();
                output.push_str(&asset_block::render_inline_math(&formula));
                index = end + 1;
                continue;
            }
        }
        if index + 1 < chars.len() && chars[index] == '~' && chars[index + 1] == '~' {
            if let Some(end) = find_double_marker(&chars, index + 2, '~') {
                output.push_str(STRIKE_STYLE);
                output.extend(chars[index + 2..end].iter());
                output.push_str(RESET);
                index = end + 2;
                continue;
            }
        }
        if index + 1 < chars.len() && chars[index] == '*' && chars[index + 1] == '*' {
            if let Some(end) = find_double_marker(&chars, index + 2, '*') {
                output.push_str(BOLD_STYLE);
                output.extend(chars[index + 2..end].iter());
                output.push_str(RESET);
                index = end + 2;
                continue;
            }
        }
        if chars[index] == '*' {
            if let Some(end) = find_marker(&chars, index + 1, '*') {
                output.push_str(ITALIC_STYLE);
                output.extend(chars[index + 1..end].iter());
                output.push_str(RESET);
                index = end + 1;
                continue;
            }
        }
        if chars[index] == '_' {
            if is_emphasis_start(&chars, index) {
                if let Some(end) = find_emphasis_end(&chars, index + 1, '_') {
                    output.push_str(ITALIC_STYLE);
                    output.extend(chars[index + 1..end].iter());
                    output.push_str(RESET);
                    index = end + 1;
                    continue;
                }
            }
        }
        if chars[index] == '[' {
            if let Some(label_end) = find_marker(&chars, index + 1, ']') {
                if chars.get(label_end + 1) == Some(&'(') {
                    if let Some(url_end) = find_marker(&chars, label_end + 2, ')') {
                        output.push_str(LINK_LABEL_STYLE);
                        output.extend(chars[index + 1..label_end].iter());
                        output.push_str(RESET);
                        output.push(' ');
                        output.push_str(&render_url_wrapped(
                            &chars[label_end + 2..url_end].iter().collect::<String>(),
                        ));
                        index = url_end + 1;
                        continue;
                    }
                }
            }
        }
        if chars[index] == '<' {
            if let Some(end) = find_marker(&chars, index + 1, '>') {
                let value = chars[index + 1..end].iter().collect::<String>();
                if value.starts_with("http://") || value.starts_with("https://") {
                    output.push_str("\x1b[4m");
                    output.push_str(&render_url_wrapped(&value));
                    output.push_str(RESET);
                    index = end + 1;
                    continue;
                }
                if let Some(rendered) = render_html_tag(&value) {
                    output.push_str(&rendered);
                    index = end + 1;
                    continue;
                }
            }
        }
        output.push(chars[index]);
        index += 1;
    }
    output
}

/// 判断当前位置是否为独立公式行前的孤立前缀。
///
/// 参数:
/// - `output`: 已渲染输出
/// - `chars`: 原始字符数组
/// - `index`: 当前索引
///
/// 返回:
/// - 是否可清理此前缀
fn is_line_start_formula_prefix(output: &str, chars: &[char], index: usize) -> bool {
    output.trim().is_empty()
        && matches!(chars.get(index + 1), Some('$'))
        && chars[index + 1..].iter().filter(|ch| **ch == '$').count() >= 2
}

/// 解析 Markdown 引用层级。
///
/// 参数:
/// - `line`: 原始行
///
/// 返回:
/// - 引用层级和剩余文本
fn parse_blockquote(line: &str) -> Option<(usize, &str)> {
    let mut depth = 0;
    let mut rest = line;
    while let Some(stripped) = rest.strip_prefix('>') {
        depth += 1;
        rest = stripped.strip_prefix(' ').unwrap_or(stripped);
    }
    (depth > 0).then_some((depth, rest))
}

/// 渲染 Markdown 标题。
///
/// 参数:
/// - `line`: 去除缩进后的行
///
/// 返回:
/// - 标题渲染结果
fn render_header(line: &str) -> Option<String> {
    let level = line.chars().take_while(|ch| *ch == '#').count();
    if level == 0 || level > 6 || line.as_bytes().get(level) != Some(&b' ') {
        return None;
    }
    let prefix = "#".repeat(level);
    Some(format!(
        "{HEADER_STYLE}{prefix} {}{RESET}",
        render_inline(&line[level + 1..])
    ))
}

/// 渲染普通链接地址。
///
/// 参数:
/// - `url`: 链接地址
///
/// 返回:
/// - 带样式的链接地址
fn render_url(url: &str) -> String {
    format!("{URL_STYLE}{url}{RESET}")
}

/// 渲染带尖括号的链接地址。
///
/// 参数:
/// - `url`: 链接地址
///
/// 返回:
/// - 带尖括号的链接地址
fn render_url_wrapped(url: &str) -> String {
    format!("<{}>", render_url(url))
}

/// 渲染允许的 HTML 行内标签。
///
/// 参数:
/// - `tag`: 标签内容
///
/// 返回:
/// - 对应终端样式
fn render_html_tag(tag: &str) -> Option<String> {
    match tag.trim().to_ascii_lowercase().as_str() {
        "u" => Some("\x1b[4m".to_string()),
        "/u" => Some("\x1b[0m".to_string()),
        "sub" => Some("\x1b[2m".to_string()),
        "/sub" => Some("\x1b[0m".to_string()),
        "sup" => Some("\x1b[1m".to_string()),
        "/sup" => Some("\x1b[0m".to_string()),
        "br" | "br/" | "br /" => Some("\n".to_string()),
        _ => None,
    }
}

/// 渲染 Markdown 水平分隔线。
///
/// 返回:
/// - 分隔线文本
fn horizontal_rule() -> String {
    let width = terminal::size()
        .map(|(width, _)| usize::from(width) / 3)
        .unwrap_or(24)
        .clamp(16, 40);
    format!("\x1b[2m{}\x1b[0m", "─".repeat(width))
}

/// 判断是否为水平分隔线。
///
/// 参数:
/// - `line`: 去除缩进后的行
///
/// 返回:
/// - 是否为水平分隔线
fn is_horizontal_rule(line: &str) -> bool {
    let trimmed = line.trim();
    trimmed.len() >= 3 && trimmed.chars().all(|ch| ch == '-')
}

/// 查找单字符标记。
///
/// 参数:
/// - `chars`: 字符数组
/// - `start`: 起始索引
/// - `marker`: 标记字符
///
/// 返回:
/// - 命中的索引
fn find_marker(chars: &[char], start: usize, marker: char) -> Option<usize> {
    (start..chars.len()).find(|index| chars[*index] == marker)
}

/// 查找强调结束标记。
///
/// 参数:
/// - `chars`: 字符数组
/// - `start`: 起始索引
/// - `marker`: 标记字符
///
/// 返回:
/// - 命中的索引
fn find_emphasis_end(chars: &[char], start: usize, marker: char) -> Option<usize> {
    (start..chars.len()).find(|index| chars[*index] == marker && is_emphasis_end(chars, *index))
}

/// 判断下划线是否可作为强调起点。
///
/// 参数:
/// - `chars`: 字符数组
/// - `index`: 当前索引
///
/// 返回:
/// - 是否为强调起点
fn is_emphasis_start(chars: &[char], index: usize) -> bool {
    !chars
        .get(index.wrapping_sub(1))
        .is_some_and(|ch| is_word_char(*ch))
        && chars
            .get(index + 1)
            .is_some_and(|ch| !ch.is_whitespace() && *ch != '_')
}

/// 判断下划线是否可作为强调终点。
///
/// 参数:
/// - `chars`: 字符数组
/// - `index`: 当前索引
///
/// 返回:
/// - 是否为强调终点
fn is_emphasis_end(chars: &[char], index: usize) -> bool {
    chars
        .get(index.wrapping_sub(1))
        .is_some_and(|ch| !ch.is_whitespace() && *ch != '_')
        && !chars.get(index + 1).is_some_and(|ch| is_word_char(*ch))
}

/// 判断字符是否为英文单词字符。
///
/// 参数:
/// - `ch`: 待判断字符
///
/// 返回:
/// - 是否为英文单词字符
fn is_word_char(ch: char) -> bool {
    ch.is_ascii_alphanumeric()
}

/// 查找连续两个相同标记。
///
/// 参数:
/// - `chars`: 字符数组
/// - `start`: 起始索引
/// - `marker`: 标记字符
///
/// 返回:
/// - 第一个标记的索引
fn find_double_marker(chars: &[char], start: usize, marker: char) -> Option<usize> {
    (start..chars.len().saturating_sub(1))
        .find(|index| chars[*index] == marker && chars[index + 1] == marker)
}

#[cfg(test)]
#[path = "markdown_tests.rs"]
mod tests;
