use crate::llm::{ChatResult, ChatStreamChunk, ChatStreamKind};
use anyhow::Result;
use crossterm::cursor::{Hide, MoveToColumn, Show};
use crossterm::style::{Color, ResetColor, SetForegroundColor};
use crossterm::{execute, terminal};
use std::collections::BTreeMap;
use std::io::{self, IsTerminal, Write};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReasoningDisplayMode {
    Hidden,
    Summary,
    Full,
}

impl ReasoningDisplayMode {
    pub fn from_config(value: &str) -> Self {
        match value.trim().to_ascii_lowercase().as_str() {
            "hidden" => Self::Hidden,
            "full" => Self::Full,
            _ => Self::Summary,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ToolCallDisplayMode {
    Hidden,
    Summary,
    Full,
}

impl ToolCallDisplayMode {
    pub fn from_config(value: &str) -> Self {
        match value.trim().to_ascii_lowercase().as_str() {
            "hidden" => Self::Hidden,
            "full" => Self::Full,
            _ => Self::Summary,
        }
    }
}

pub fn print_assistant_response(response: &ChatResult, show_reasoning: bool) -> Result<()> {
    if show_reasoning {
        if let Some(reasoning) = response
            .reasoning
            .as_deref()
            .filter(|text| !text.trim().is_empty())
        {
            print_reasoning(reasoning)?;
        }
    }
    print_markdown(&response.content);
    Ok(())
}

pub fn print_markdown(markdown: &str) {
    let skin = termimad::MadSkin::default();
    println!("{}", skin.term_text(markdown.trim_end()));
}

pub struct StreamRenderer {
    reasoning_mode: ReasoningDisplayMode,
    tool_call_mode: ToolCallDisplayMode,
    plain: bool,
    mode: Option<ChatStreamKind>,
    cursor_hidden: bool,
    markdown: MarkdownStreamRenderer,
    reasoning_chars: usize,
    reasoning_lines: usize,
    tool_stats: BTreeMap<String, ToolStats>,
    summary_line_active: bool,
    live_summary: bool,
}

impl StreamRenderer {
    pub fn new(
        reasoning_mode: ReasoningDisplayMode,
        tool_call_mode: ToolCallDisplayMode,
        plain: bool,
    ) -> Self {
        Self {
            reasoning_mode,
            tool_call_mode,
            plain,
            mode: None,
            cursor_hidden: false,
            markdown: MarkdownStreamRenderer::new(),
            reasoning_chars: 0,
            reasoning_lines: 0,
            tool_stats: BTreeMap::new(),
            summary_line_active: false,
            live_summary: io::stdout().is_terminal(),
        }
    }

    pub fn write_chunk(&mut self, chunk: ChatStreamChunk) -> Result<()> {
        self.hide_cursor()?;
        if self.plain && chunk.kind == ChatStreamKind::Reasoning {
            return Ok(());
        }
        if self.reasoning_mode == ReasoningDisplayMode::Hidden
            && chunk.kind == ChatStreamKind::Reasoning
        {
            return Ok(());
        }
        if self.reasoning_mode == ReasoningDisplayMode::Summary
            && chunk.kind == ChatStreamKind::Reasoning
        {
            self.finalize_tools_summary()?;
            self.reasoning_chars += chunk.text.chars().count();
            self.reasoning_lines += chunk.text.matches('\n').count();
            self.mode = Some(ChatStreamKind::Reasoning);
            self.render_summary_line(&self.reasoning_summary_text())?;
            return Ok(());
        }
        if self.mode != Some(chunk.kind) {
            if chunk.kind == ChatStreamKind::Content {
                self.finalize_reasoning_summary()?;
                self.finalize_tools_summary()?;
            }
            self.switch_mode(chunk.kind)?;
        }
        let mut stdout = io::stdout();
        if self.plain || chunk.kind == ChatStreamKind::Reasoning {
            write!(stdout, "{}", chunk.text)?;
        } else {
            write!(stdout, "{}", self.markdown.push(&chunk.text))?;
        }
        stdout.flush()?;
        Ok(())
    }

    pub fn write_tool_call(&mut self, name: &str, arguments: &str) -> Result<()> {
        if self.plain {
            return Ok(());
        }
        self.end_active_stream_line()?;
        self.finalize_reasoning_summary()?;
        if self.tool_call_mode == ToolCallDisplayMode::Full {
            let mut stdout = io::stdout();
            writeln!(stdout, "tool {name}")?;
            write_tool_payload(&mut stdout, "args", arguments)?;
            stdout.flush()?;
        } else if self.tool_call_mode == ToolCallDisplayMode::Summary {
            self.tool_stats.entry(name.to_string()).or_default().calls += 1;
            self.render_summary_line(&self.tool_summary_text())?;
        }
        Ok(())
    }

    pub fn write_tool_result(&mut self, name: &str, ok: bool, output: &str) -> Result<()> {
        if self.plain {
            return Ok(());
        }
        let status = if ok { "ok" } else { "error" };
        if self.tool_call_mode == ToolCallDisplayMode::Full {
            let mut stdout = io::stdout();
            writeln!(stdout, "result {name} {status}")?;
            write_tool_payload(&mut stdout, "output", output)?;
            stdout.flush()?;
        } else if self.tool_call_mode == ToolCallDisplayMode::Summary {
            let stats = self.tool_stats.entry(name.to_string()).or_default();
            if ok {
                stats.ok += 1;
            } else {
                stats.error += 1;
            }
            self.render_summary_line(&self.tool_summary_text())?;
        }
        Ok(())
    }

    pub fn finish(&mut self) -> Result<()> {
        if self.mode == Some(ChatStreamKind::Content) && !self.plain {
            let mut stdout = io::stdout();
            write!(stdout, "{}", self.markdown.flush())?;
            stdout.flush()?;
        }
        if self.mode == Some(ChatStreamKind::Reasoning) {
            execute!(io::stdout(), ResetColor)?;
        }
        if self.mode.is_some() {
            println!();
        }
        self.finalize_reasoning_summary()?;
        self.finalize_tools_summary()?;
        self.mode = None;
        self.show_cursor()?;
        Ok(())
    }

    fn switch_mode(&mut self, mode: ChatStreamKind) -> Result<()> {
        let mut stdout = io::stdout();
        match mode {
            ChatStreamKind::Reasoning => {
                if self.mode.is_some() {
                    writeln!(stdout)?;
                }
                execute!(stdout, SetForegroundColor(Color::DarkGrey))?;
                writeln!(stdout, "thinking")?;
            }
            ChatStreamKind::Content => {
                if self.mode == Some(ChatStreamKind::Reasoning) {
                    execute!(stdout, ResetColor)?;
                    writeln!(stdout)?;
                }
            }
        }
        stdout.flush()?;
        self.mode = Some(mode);
        Ok(())
    }

    fn end_active_stream_line(&mut self) -> Result<()> {
        if self.reasoning_mode == ReasoningDisplayMode::Summary
            && self.mode == Some(ChatStreamKind::Reasoning)
        {
            self.mode = None;
            return Ok(());
        }
        if self.mode == Some(ChatStreamKind::Reasoning) {
            execute!(io::stdout(), ResetColor)?;
        } else if self.mode == Some(ChatStreamKind::Content) && !self.plain {
            let mut stdout = io::stdout();
            write!(stdout, "{}", self.markdown.flush())?;
            stdout.flush()?;
        }
        if self.mode.is_some() {
            println!();
            self.mode = None;
        }
        Ok(())
    }

    fn finalize_reasoning_summary(&mut self) -> Result<()> {
        if self.reasoning_mode == ReasoningDisplayMode::Summary && self.reasoning_chars > 0 {
            if self.summary_line_active {
                println!();
                self.summary_line_active = false;
            } else {
                println!("\x1b[2m{}\x1b[0m", self.reasoning_summary_text());
            }
            self.reasoning_chars = 0;
            self.reasoning_lines = 0;
            self.mode = None;
        }
        Ok(())
    }

    fn finalize_tools_summary(&mut self) -> Result<()> {
        if self.tool_call_mode == ToolCallDisplayMode::Summary && !self.tool_stats.is_empty() {
            if self.summary_line_active {
                println!();
                self.summary_line_active = false;
            } else {
                println!("\x1b[2m{}\x1b[0m", self.tool_summary_text());
            }
            self.tool_stats.clear();
        }
        Ok(())
    }

    fn render_summary_line(&mut self, text: &str) -> Result<()> {
        if !self.live_summary {
            return Ok(());
        }
        let mut stdout = io::stdout();
        execute!(stdout, MoveToColumn(0))?;
        write!(stdout, "\x1b[2m{text}\x1b[0m\x1b[K")?;
        stdout.flush()?;
        self.summary_line_active = true;
        Ok(())
    }

    fn reasoning_summary_text(&self) -> String {
        format!(
            "thinking · {} lines · {} chars",
            self.reasoning_lines.max(1),
            self.reasoning_chars
        )
    }

    fn tool_summary_text(&self) -> String {
        let parts = self
            .tool_stats
            .iter()
            .map(|(name, stats)| {
                format!(
                    "{name}×{} ok:{} err:{}",
                    stats.calls.max(stats.ok + stats.error),
                    stats.ok,
                    stats.error
                )
            })
            .collect::<Vec<_>>()
            .join(", ");
        format!("tools: {parts}")
    }

    fn hide_cursor(&mut self) -> Result<()> {
        if !self.cursor_hidden {
            execute!(io::stdout(), Hide)?;
            self.cursor_hidden = true;
        }
        Ok(())
    }

    fn show_cursor(&mut self) -> Result<()> {
        if self.cursor_hidden {
            execute!(io::stdout(), Show)?;
            self.cursor_hidden = false;
        }
        Ok(())
    }
}

#[derive(Default)]
struct ToolStats {
    calls: usize,
    ok: usize,
    error: usize,
}

struct MarkdownStreamRenderer {
    buffer: String,
    line_renderer: MarkdownLineRenderer,
}

impl MarkdownStreamRenderer {
    fn new() -> Self {
        Self {
            buffer: String::new(),
            line_renderer: MarkdownLineRenderer::new(),
        }
    }

    fn push(&mut self, delta: &str) -> String {
        self.buffer.push_str(delta);
        let mut output = String::new();
        while let Some(index) = self.buffer.find('\n') {
            let line = self.buffer[..index].to_string();
            self.buffer = self.buffer[index + 1..].to_string();
            output.push_str(&self.line_renderer.render_line(&line));
        }
        output
    }

    fn flush(&mut self) -> String {
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
    code_lang: String,
    table_buffer: Vec<String>,
}

impl MarkdownLineRenderer {
    fn new() -> Self {
        Self {
            in_code_block: false,
            code_lang: String::new(),
            table_buffer: Vec::new(),
        }
    }

    fn render_line(&mut self, line: &str) -> String {
        if line.trim_start().starts_with("```") {
            let pending = self.flush();
            if self.in_code_block {
                self.in_code_block = false;
                self.code_lang.clear();
                return format!("{pending}\x1b[2m`--\x1b[0m\n");
            }
            self.in_code_block = true;
            self.code_lang = line
                .trim_start()
                .trim_start_matches('`')
                .split_whitespace()
                .next()
                .unwrap_or_default()
                .to_string();
            let label = if self.code_lang.is_empty() {
                "code".to_string()
            } else {
                format!("code {}", self.code_lang)
            };
            return format!("{pending}\x1b[2m,-- {label}\x1b[0m\n");
        }
        if self.in_code_block {
            return format!("{}\n", highlight_code_line(&self.code_lang, line));
        }
        if line.contains('|') {
            self.table_buffer.push(line.to_string());
            return String::new();
        }
        let mut output = self.flush();
        output.push_str(&render_markdown_line(line));
        output.push('\n');
        output
    }

    fn flush(&mut self) -> String {
        if self.table_buffer.is_empty() {
            return String::new();
        }
        let lines = std::mem::take(&mut self.table_buffer);
        if lines.len() >= 2 && is_table_separator(lines.get(1).map(String::as_str).unwrap_or("")) {
            render_table(&lines)
        } else {
            let mut output = String::new();
            for line in lines {
                output.push_str(&render_markdown_line(&line));
                output.push('\n');
            }
            output
        }
    }
}

fn render_markdown_line(line: &str) -> String {
    let trimmed = line.trim_start();
    let indent = &line[..line.len() - trimmed.len()];
    if let Some(header) = render_header(trimmed) {
        return header;
    }
    if let Some((depth, rest)) = parse_blockquote(trimmed) {
        let bars = "\x1b[36m|\x1b[0m ".repeat(depth);
        return format!(
            "{indent}{bars}\x1b[3m\x1b[36m{}\x1b[0m",
            render_inline(rest)
        );
    }
    if let Some(rest) = trimmed
        .strip_prefix("- ")
        .or_else(|| trimmed.strip_prefix("* "))
        .or_else(|| trimmed.strip_prefix("+ "))
    {
        return format!("{indent}\x1b[2m-\x1b[0m {}", render_inline(rest));
    }
    let digits = trimmed.chars().take_while(|ch| ch.is_ascii_digit()).count();
    if digits > 0
        && trimmed.as_bytes().get(digits) == Some(&b'.')
        && trimmed.as_bytes().get(digits + 1) == Some(&b' ')
    {
        let marker = &trimmed[..=digits];
        let rest = &trimmed[digits + 2..];
        return format!("{indent}\x1b[2m{marker}\x1b[0m {}", render_inline(rest));
    }
    if is_horizontal_rule(trimmed) {
        return horizontal_rule();
    }
    render_inline(line)
}

fn parse_blockquote(line: &str) -> Option<(usize, &str)> {
    let mut depth = 0;
    let mut rest = line;
    while let Some(stripped) = rest.strip_prefix('>') {
        depth += 1;
        rest = stripped.strip_prefix(' ').unwrap_or(stripped);
    }
    (depth > 0).then_some((depth, rest))
}

fn render_header(line: &str) -> Option<String> {
    let level = line.chars().take_while(|ch| *ch == '#').count();
    if level == 0 || level > 6 || line.as_bytes().get(level) != Some(&b' ') {
        return None;
    }
    Some(format!(
        "\x1b[1m\x1b[36m{}\x1b[0m",
        render_inline(&line[level + 1..])
    ))
}

fn render_inline(text: &str) -> String {
    let mut output = String::new();
    let chars = text.chars().collect::<Vec<_>>();
    let mut index = 0;
    while index < chars.len() {
        if index + 1 < chars.len() && chars[index] == '!' && chars[index + 1] == '[' {
            if let Some(label_end) = find_marker(&chars, index + 2, ']') {
                if chars.get(label_end + 1) == Some(&'(') {
                    if let Some(url_end) = find_marker(&chars, label_end + 2, ')') {
                        let alt = chars[index + 2..label_end].iter().collect::<String>();
                        output.push_str("\x1b[35m[image");
                        if !alt.is_empty() {
                            output.push_str(": ");
                            output.push_str(&alt);
                        }
                        output.push_str("]\x1b[0m(");
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
            if let Some(end) = find_marker(&chars, index + 1, '`') {
                output.push_str("\x1b[33m");
                output.extend(chars[index + 1..end].iter());
                output.push_str("\x1b[0m");
                index = end + 1;
                continue;
            }
        }
        if index + 1 < chars.len() && chars[index] == '~' && chars[index + 1] == '~' {
            if let Some(end) = find_double_marker(&chars, index + 2, '~') {
                output.push_str("\x1b[9m");
                output.extend(chars[index + 2..end].iter());
                output.push_str("\x1b[0m");
                index = end + 2;
                continue;
            }
        }
        if index + 1 < chars.len() && chars[index] == '*' && chars[index + 1] == '*' {
            if let Some(end) = find_double_marker(&chars, index + 2, '*') {
                output.push_str("\x1b[1m");
                output.extend(chars[index + 2..end].iter());
                output.push_str("\x1b[0m");
                index = end + 2;
                continue;
            }
        }
        if chars[index] == '*' {
            if let Some(end) = find_marker(&chars, index + 1, '*') {
                output.push_str("\x1b[3m");
                output.extend(chars[index + 1..end].iter());
                output.push_str("\x1b[0m");
                index = end + 1;
                continue;
            }
        }
        if chars[index] == '_' {
            if is_emphasis_start(&chars, index) {
                if let Some(end) = find_emphasis_end(&chars, index + 1, '_') {
                    output.push_str("\x1b[3m");
                    output.extend(chars[index + 1..end].iter());
                    output.push_str("\x1b[0m");
                    index = end + 1;
                    continue;
                }
            }
        }
        if chars[index] == '[' {
            if let Some(label_end) = find_marker(&chars, index + 1, ']') {
                if chars.get(label_end + 1) == Some(&'(') {
                    if let Some(url_end) = find_marker(&chars, label_end + 2, ')') {
                        output.push_str("\x1b[4m\x1b[36m");
                        output.extend(chars[index + 1..label_end].iter());
                        output.push_str("\x1b[0m ");
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
                    output.push_str("\x1b[0m");
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

fn render_url(url: &str) -> String {
    format!("\x1b[2m\x1b[34m{url}\x1b[0m")
}

fn render_url_wrapped(url: &str) -> String {
    format!("<{}>", render_url(url))
}

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

fn horizontal_rule() -> String {
    let width = terminal::size()
        .map(|(width, _)| usize::from(width))
        .unwrap_or(48)
        .clamp(16, 120);
    format!("\x1b[2m{}\x1b[0m", "-".repeat(width))
}

fn render_table(lines: &[String]) -> String {
    let alignments = lines
        .get(1)
        .filter(|line| is_table_separator(line))
        .map(|line| parse_table_alignments(line))
        .unwrap_or_default();
    let rows = lines
        .iter()
        .filter(|line| !is_table_separator(line))
        .map(|line| {
            line.trim()
                .trim_matches('|')
                .split('|')
                .map(|cell| render_inline(cell.trim()))
                .collect::<Vec<_>>()
        })
        .collect::<Vec<_>>();
    let cols = rows.iter().map(Vec::len).max().unwrap_or(0);
    let mut widths = vec![0usize; cols];
    for row in &rows {
        for (index, cell) in row.iter().enumerate() {
            widths[index] = widths[index].max(visible_width(cell));
        }
    }
    let mut output = String::new();
    output.push_str(&table_border(&widths, '+', '+', '+'));
    for (row_index, row) in rows.iter().enumerate() {
        output.push('|');
        for (index, width) in widths.iter().enumerate() {
            let cell = row.get(index).map(String::as_str).unwrap_or("");
            let cell = if row_index == 0 {
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
            output.push('|');
        }
        output.push('\n');
        if row_index == 0 {
            output.push_str(&table_border(&widths, '+', '+', '+'));
        }
    }
    output.push_str(&table_border(&widths, '+', '+', '+'));
    output
}

#[derive(Clone, Copy)]
enum TableAlign {
    Left,
    Center,
    Right,
}

fn parse_table_alignments(line: &str) -> Vec<TableAlign> {
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

fn table_border(widths: &[usize], left: char, mid: char, right: char) -> String {
    let mut output = String::new();
    output.push_str("\x1b[2m");
    output.push(left);
    for (index, width) in widths.iter().enumerate() {
        output.push_str(&"-".repeat(width + 2));
        output.push(if index + 1 == widths.len() {
            right
        } else {
            mid
        });
    }
    output.push_str("\x1b[0m\n");
    output
}

fn highlight_code_line(lang: &str, line: &str) -> String {
    let keywords = match lang {
        "rs" | "rust" => &[
            "fn", "let", "mut", "pub", "struct", "enum", "impl", "use", "match", "if", "else",
            "async", "await",
        ][..],
        "js" | "ts" | "tsx" | "jsx" => &[
            "function", "const", "let", "return", "if", "else", "import", "export", "async",
            "await",
        ][..],
        "py" | "python" => &[
            "def", "class", "import", "from", "return", "if", "else", "elif", "async", "await",
        ][..],
        _ => &[],
    };
    if keywords.is_empty() {
        return format!("\x1b[33m{line}\x1b[0m");
    }
    let mut output = String::new();
    let mut highlighted = false;
    for token in line.split_inclusive(|ch: char| !ch.is_ascii_alphanumeric() && ch != '_') {
        let bare = token.trim_matches(|ch: char| !ch.is_ascii_alphanumeric() && ch != '_');
        if keywords.contains(&bare) {
            highlighted = true;
            output.push_str(&token.replace(bare, &format!("\x1b[1m\x1b[36m{bare}\x1b[0m\x1b[2m")));
        } else {
            output.push_str(token);
        }
    }
    if highlighted {
        output
    } else {
        format!("\x1b[33m{line}\x1b[0m")
    }
}

fn is_table_separator(line: &str) -> bool {
    let trimmed = line.trim().trim_matches('|').trim();
    !trimmed.is_empty()
        && trimmed
            .chars()
            .all(|ch| matches!(ch, '-' | ':' | '|' | ' '))
        && trimmed.contains('-')
}

fn is_horizontal_rule(line: &str) -> bool {
    let trimmed = line.trim();
    trimmed.len() >= 3 && trimmed.chars().all(|ch| ch == '-')
}

fn find_marker(chars: &[char], start: usize, marker: char) -> Option<usize> {
    (start..chars.len()).find(|index| chars[*index] == marker)
}

fn find_emphasis_end(chars: &[char], start: usize, marker: char) -> Option<usize> {
    (start..chars.len()).find(|index| chars[*index] == marker && is_emphasis_end(chars, *index))
}

fn is_emphasis_start(chars: &[char], index: usize) -> bool {
    !chars
        .get(index.wrapping_sub(1))
        .is_some_and(|ch| is_word_char(*ch))
        && chars
            .get(index + 1)
            .is_some_and(|ch| !ch.is_whitespace() && *ch != '_')
}

fn is_emphasis_end(chars: &[char], index: usize) -> bool {
    chars
        .get(index.wrapping_sub(1))
        .is_some_and(|ch| !ch.is_whitespace() && *ch != '_')
        && !chars.get(index + 1).is_some_and(|ch| is_word_char(*ch))
}

fn is_word_char(ch: char) -> bool {
    ch.is_ascii_alphanumeric()
}

fn find_double_marker(chars: &[char], start: usize, marker: char) -> Option<usize> {
    (start..chars.len().saturating_sub(1))
        .find(|index| chars[*index] == marker && chars[index + 1] == marker)
}

fn visible_width(text: &str) -> usize {
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

fn write_tool_payload(stdout: &mut io::Stdout, label: &str, payload: &str) -> Result<()> {
    let formatted = format_tool_payload(payload);
    writeln!(stdout, "\x1b[2m{label}:\x1b[0m")?;
    for line in formatted.lines() {
        writeln!(stdout, "\x1b[2m  {line}\x1b[0m")?;
    }
    Ok(())
}

fn format_tool_payload(payload: &str) -> String {
    let text = payload.trim();
    let formatted = serde_json::from_str::<serde_json::Value>(text)
        .ok()
        .and_then(|value| serde_json::to_string_pretty(&value).ok())
        .unwrap_or_else(|| text.to_string());
    truncate_chars(&formatted, 2400)
}

fn truncate_chars(text: &str, max_chars: usize) -> String {
    let total = text.chars().count();
    if total <= max_chars {
        return text.to_string();
    }
    let omitted = total - max_chars;
    format!(
        "{}\n... truncated {omitted} chars ...",
        text.chars().take(max_chars).collect::<String>()
    )
}

impl Drop for StreamRenderer {
    fn drop(&mut self) {
        let _ = self.show_cursor();
        let _ = execute!(io::stdout(), ResetColor);
    }
}

fn print_reasoning(reasoning: &str) -> Result<()> {
    let mut stdout = io::stdout();
    execute!(stdout, SetForegroundColor(Color::DarkGrey))?;
    writeln!(stdout, "thinking")?;
    for line in reasoning.trim().lines() {
        writeln!(stdout, "  {line}")?;
    }
    execute!(stdout, ResetColor)?;
    if terminal::size().is_ok() {
        writeln!(stdout)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn streams_only_complete_lines() {
        let mut renderer = MarkdownStreamRenderer::new();
        assert_eq!(renderer.push("**bo"), "");
        assert_eq!(renderer.push("ld**\n"), "\x1b[1mbold\x1b[0m\n");
    }

    #[test]
    fn flushes_partial_final_line() {
        let mut renderer = MarkdownStreamRenderer::new();
        assert_eq!(renderer.push("# Title"), "");
        assert_eq!(renderer.flush(), "\x1b[1m\x1b[36mTitle\x1b[0m\n");
    }

    #[test]
    fn buffers_tables_until_non_table_line() {
        let mut renderer = MarkdownStreamRenderer::new();
        assert_eq!(renderer.push("| a | b |\n| - | - |\n| 1 | 2 |\n"), "");
        let output = renderer.push("done\n");
        assert!(output.contains("+---+---+"));
        assert!(output.contains("\x1b[1ma\x1b[0m"));
        assert!(output.ends_with("done\n"));
    }

    #[test]
    fn blockquote_is_visually_distinct() {
        let mut renderer = MarkdownStreamRenderer::new();
        let output = renderer.push(">> quoted\n");
        assert!(output.contains("\x1b[36m|\x1b[0m \x1b[36m|\x1b[0m"));
        assert!(output.contains("\x1b[3m\x1b[36mquoted\x1b[0m"));
    }

    #[test]
    fn code_block_has_label_and_readable_content() {
        let mut renderer = MarkdownStreamRenderer::new();
        let output = renderer.push("```rust\nfn main() {}\n```\n");
        assert!(output.contains(",-- code rust"));
        assert!(!output.contains("\x1b[2m|\x1b[0m"));
        assert!(output.contains("\x1b[1m\x1b[36mfn\x1b[0m"));
        assert!(output.contains("`--"));
    }

    #[test]
    fn code_block_content_has_default_color() {
        let mut renderer = MarkdownStreamRenderer::new();
        let output = renderer.push("```\nXMODIFIERS \"@im=fcitx\"\n```\n");
        assert!(output.contains("\x1b[33mXMODIFIERS \"@im=fcitx\"\x1b[0m"));
    }

    #[test]
    fn renders_more_inline_markdown() {
        let output = render_inline(
            "*i* ~~gone~~ [site](https://example.com) <https://example.org> ![pic](https://img)",
        );
        assert!(output.contains("\x1b[3mi\x1b[0m"));
        assert!(output.contains("\x1b[9mgone\x1b[0m"));
        assert!(output.contains("<\x1b[2m\x1b[34mhttps://example.com\x1b[0m>"));
        assert!(output.contains("\x1b[4m<\x1b[2m\x1b[34mhttps://example.org\x1b[0m>\x1b[0m"));
        assert!(output.contains("\x1b[35m[image: pic]\x1b[0m(\x1b[2m\x1b[34mhttps://img\x1b[0m)"));
        assert!(!output.contains("\x1b[35mimage\x1b[0m"));
    }

    #[test]
    fn keeps_identifier_underscores_literal() {
        let output = render_inline("GTK_IM_MODULE and _italic_");
        assert!(output.contains("GTK_IM_MODULE"));
        assert!(output.contains("\x1b[3mitalic\x1b[0m"));
        assert!(!output.contains("GTK\x1b[3mIM\x1b[0mMODULE"));
        assert_eq!(render_inline("abc_def_ghi"), "abc_def_ghi");
    }

    #[test]
    fn renders_selected_inline_html_tags() {
        let output = render_inline("<u>under</u> H<sub>2</sub> x<sup>2</sup><br>next");
        assert!(output.contains("\x1b[4munder\x1b[0m"));
        assert!(output.contains("H\x1b[2m2\x1b[0m"));
        assert!(output.contains("x\x1b[1m2\x1b[0m"));
        assert!(output.contains("\nnext"));
    }

    #[test]
    fn horizontal_rule_uses_terminal_width_fallback() {
        let output = render_markdown_line("---");
        assert!(output.starts_with("\x1b[2m"));
        assert!(output.ends_with("\x1b[0m"));
        assert!(visible_width(&output) >= 16);
    }

    #[test]
    fn supports_table_alignment_markers() {
        let mut renderer = MarkdownStreamRenderer::new();
        assert_eq!(
            renderer.push("| left | mid | right |\n| :--- | :---: | ---: |\n| a | b | c |\n"),
            ""
        );
        let output = renderer.flush();
        assert!(output.contains("+"));
        assert!(!output.contains(":---"));
        assert!(output.contains("\x1b[1mleft\x1b[0m"));
    }
}
