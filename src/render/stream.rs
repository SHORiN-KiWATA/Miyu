use crate::i18n::text as t;
use crate::llm::{ChatResult, ChatStreamChunk, ChatStreamKind};
use crate::render::command_output::{
    write_command_block, write_command_error_block, write_command_result_blocks, write_tool_payload,
};
use crate::render::markdown::MarkdownStreamRenderer;
use crate::render::tool_names::readable_tool_name;
use anyhow::Result;
use crossterm::cursor::{Hide, MoveToColumn, Show};
use crossterm::style::{Color, ResetColor, SetForegroundColor};
use crossterm::terminal::{Clear, ClearType};
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
    /// 从配置文本解析推理展示模式。
    ///
    /// 参数:
    /// - `value`: 配置值
    ///
    /// 返回:
    /// - 推理展示模式
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
    /// 从配置文本解析工具调用展示模式。
    ///
    /// 参数:
    /// - `value`: 配置值
    ///
    /// 返回:
    /// - 工具调用展示模式
    pub fn from_config(value: &str) -> Self {
        match value.trim().to_ascii_lowercase().as_str() {
            "hidden" => Self::Hidden,
            "full" => Self::Full,
            _ => Self::Summary,
        }
    }
}

/// 打印一次完整助手回复。
///
/// 参数:
/// - `response`: 聊天结果
/// - `show_reasoning`: 是否打印推理内容
///
/// 返回:
/// - 打印是否成功
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

/// 打印 Markdown 文本。
///
/// 参数:
/// - `markdown`: 原始 Markdown 文本
pub fn print_markdown(markdown: &str) {
    let mut renderer = MarkdownStreamRenderer::new();
    let markdown = markdown.trim_end();
    let mut stdout = io::stdout();
    let _ = write!(stdout, "{}", renderer.push(markdown));
    let _ = write!(stdout, "{}", renderer.flush());
    let _ = stdout.flush();
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
    readable_tool_names: bool,
    summary_line_active: bool,
    summary_lines_active: u16,
    live_summary: bool,
}

impl StreamRenderer {
    /// 创建流式响应渲染器。
    ///
    /// 参数:
    /// - `reasoning_mode`: 推理内容展示模式
    /// - `tool_call_mode`: 工具调用展示模式
    /// - `plain`: 是否使用纯文本输出
    /// - `readable_tool_names`: 是否展示可读工具名称
    ///
    /// 返回:
    /// - 新的流式渲染器
    pub fn new(
        reasoning_mode: ReasoningDisplayMode,
        tool_call_mode: ToolCallDisplayMode,
        plain: bool,
        readable_tool_names: bool,
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
            readable_tool_names,
            summary_line_active: false,
            summary_lines_active: 0,
            live_summary: io::stdout().is_terminal(),
        }
    }

    /// 写入模型流式文本片段。
    ///
    /// 参数:
    /// - `chunk`: 模型流式片段
    ///
    /// 返回:
    /// - 写入是否成功
    pub fn write_chunk(&mut self, chunk: ChatStreamChunk) -> Result<()> {
        self.hide_cursor()?;
        let text = normalize_stream_text(&chunk.text);
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
            self.reasoning_chars += text.chars().count();
            self.reasoning_lines += text.matches('\n').count();
            self.mode = Some(ChatStreamKind::Reasoning);
            self.render_summary_line(&self.reasoning_summary_text(), SummaryStyle::Reasoning)?;
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
            write!(stdout, "{text}")?;
        } else {
            write!(stdout, "{}", self.markdown.push(&text))?;
        }
        stdout.flush()?;
        Ok(())
    }

    /// 写入工具调用。
    ///
    /// 参数:
    /// - `name`: 工具名称
    /// - `arguments`: 工具参数
    ///
    /// 返回:
    /// - 写入是否成功
    pub fn write_tool_call(&mut self, name: &str, arguments: &str) -> Result<()> {
        if self.plain {
            return Ok(());
        }
        self.end_active_stream_line()?;
        self.finalize_reasoning_summary()?;
        if name == "run_command" {
            let mut stdout = io::stdout();
            write_command_block(&mut stdout, arguments)?;
            stdout.flush()?;
            if self.tool_call_mode == ToolCallDisplayMode::Summary {
                self.tool_stats.entry(name.to_string()).or_default().calls += 1;
            }
            return Ok(());
        }
        if self.tool_call_mode == ToolCallDisplayMode::Full {
            let mut stdout = io::stdout();
            writeln!(stdout, "tool {}", self.display_tool_name(name))?;
            write_tool_payload(&mut stdout, t("args", "参数"), arguments)?;
            stdout.flush()?;
        } else if self.tool_call_mode == ToolCallDisplayMode::Summary {
            self.tool_stats.entry(name.to_string()).or_default().calls += 1;
            self.render_summary_line(&self.tool_summary_text(), SummaryStyle::Tool)?;
        }
        Ok(())
    }

    /// 写入工具结果。
    ///
    /// 参数:
    /// - `name`: 工具名称
    /// - `ok`: 工具是否成功
    /// - `output`: 工具输出
    ///
    /// 返回:
    /// - 写入是否成功
    pub fn write_tool_result(&mut self, name: &str, ok: bool, output: &str) -> Result<()> {
        if self.plain {
            return Ok(());
        }
        let status = if ok { "ok" } else { "err" };
        if name == "run_command" {
            if self.tool_call_mode == ToolCallDisplayMode::Summary {
                let stats = self.tool_stats.entry(name.to_string()).or_default();
                if ok {
                    stats.ok += 1;
                } else {
                    stats.error += 1;
                    stats.progress = None;
                    let mut stdout = io::stdout();
                    write_command_error_block(&mut stdout, output)?;
                    stdout.flush()?;
                }
                return Ok(());
            }
            if self.tool_call_mode == ToolCallDisplayMode::Full {
                let mut stdout = io::stdout();
                write_command_result_blocks(&mut stdout, output)?;
                stdout.flush()?;
                return Ok(());
            }
        }
        if self.tool_call_mode == ToolCallDisplayMode::Full {
            let mut stdout = io::stdout();
            writeln!(stdout, "result {} {status}", self.display_tool_name(name))?;
            write_tool_payload(&mut stdout, t("output", "输出"), output)?;
            stdout.flush()?;
        } else if self.tool_call_mode == ToolCallDisplayMode::Summary {
            let stats = self.tool_stats.entry(name.to_string()).or_default();
            if ok {
                stats.ok += 1;
            } else {
                stats.error += 1;
                stats.progress = None;
            }
            self.render_summary_line(&self.tool_summary_text(), SummaryStyle::Tool)?;
        }
        Ok(())
    }

    /// 写入工具进度。
    ///
    /// 参数:
    /// - `name`: 工具名称
    /// - `message`: 进度信息
    ///
    /// 返回:
    /// - 写入是否成功
    pub fn write_tool_progress(&mut self, name: &str, message: &str) -> Result<()> {
        if self.plain {
            return Ok(());
        }
        if message == "__external_output__" {
            self.prepare_for_external_output()?;
            return Ok(());
        }
        self.end_active_stream_line()?;
        self.finalize_reasoning_summary()?;
        if self.tool_call_mode == ToolCallDisplayMode::Full {
            let mut stdout = io::stdout();
            writeln!(
                stdout,
                "progress {}: {message}",
                self.display_tool_name(name)
            )?;
            stdout.flush()?;
        } else if self.tool_call_mode == ToolCallDisplayMode::Summary {
            self.tool_stats
                .entry(name.to_string())
                .or_default()
                .progress = Some(message.to_string());
            self.render_summary_line(&self.tool_summary_text(), SummaryStyle::Tool)?;
        }
        Ok(())
    }

    /// 为外部程序直接输出终端内容做准备。
    ///
    /// 返回:
    /// - 准备是否成功
    pub fn prepare_for_external_output(&mut self) -> Result<()> {
        if self.summary_line_active {
            let mut stdout = io::stdout();
            execute!(stdout, MoveToColumn(0), Clear(ClearType::CurrentLine))?;
            stdout.flush()?;
            self.summary_line_active = false;
        }
        self.end_active_stream_line()?;
        self.finalize_reasoning_summary()?;
        self.finalize_tools_summary()?;
        self.show_cursor()?;
        Ok(())
    }

    /// 完成当前流式渲染。
    ///
    /// 返回:
    /// - 收尾是否成功
    pub fn finish(&mut self) -> Result<()> {
        if self.summary_line_active {
            self.clear_summary_lines()?;
            self.summary_line_active = false;
        }
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

    /// 切换当前流式输出模式。
    ///
    /// 参数:
    /// - `mode`: 新输出模式
    ///
    /// 返回:
    /// - 切换是否成功
    fn switch_mode(&mut self, mode: ChatStreamKind) -> Result<()> {
        let mut stdout = io::stdout();
        match mode {
            ChatStreamKind::Reasoning => {
                if self.mode.is_some() {
                    writeln!(stdout)?;
                }
                execute!(stdout, SetForegroundColor(Color::DarkCyan))?;
                writeln!(stdout, "{}", t("thinking", "思考"))?;
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

    /// 结束当前活动流式行。
    ///
    /// 返回:
    /// - 结束是否成功
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

    /// 固化推理摘要。
    ///
    /// 返回:
    /// - 固化是否成功
    fn finalize_reasoning_summary(&mut self) -> Result<()> {
        if self.reasoning_mode == ReasoningDisplayMode::Summary && self.reasoning_chars > 0 {
            if self.summary_line_active {
                let mut stdout = io::stdout();
                self.clear_summary_lines()?;
                writeln!(
                    stdout,
                    "{}",
                    style_summary_text(&self.reasoning_summary_text(), SummaryStyle::Reasoning)
                )?;
                stdout.flush()?;
                self.summary_line_active = false;
                self.summary_lines_active = 0;
            } else {
                println!(
                    "{}",
                    style_summary_text(&self.reasoning_summary_text(), SummaryStyle::Reasoning)
                );
            }
            self.reasoning_chars = 0;
            self.reasoning_lines = 0;
            self.mode = None;
        }
        Ok(())
    }

    /// 固化工具调用摘要。
    ///
    /// 返回:
    /// - 固化是否成功
    fn finalize_tools_summary(&mut self) -> Result<()> {
        if self.tool_call_mode == ToolCallDisplayMode::Summary && !self.tool_stats.is_empty() {
            if self.summary_line_active {
                let mut stdout = io::stdout();
                self.clear_summary_lines()?;
                writeln!(
                    stdout,
                    "{}",
                    style_summary_text(&self.tool_summary_text(), SummaryStyle::Tool)
                )?;
                stdout.flush()?;
                self.summary_line_active = false;
                self.summary_lines_active = 0;
            } else {
                println!(
                    "{}",
                    style_summary_text(&self.tool_summary_text(), SummaryStyle::Tool)
                );
            }
            self.tool_stats.clear();
        }
        Ok(())
    }

    /// 渲染实时摘要行。
    ///
    /// 参数:
    /// - `text`: 摘要文本
    /// - `style`: 摘要样式
    ///
    /// 返回:
    /// - 渲染是否成功
    fn render_summary_line(&mut self, text: &str, style: SummaryStyle) -> Result<()> {
        if !self.live_summary {
            return Ok(());
        }
        let mut stdout = io::stdout();
        self.clear_summary_lines()?;
        let lines = text.lines().collect::<Vec<_>>();
        for (index, line) in lines.iter().enumerate() {
            if index > 0 {
                writeln!(stdout)?;
            }
            write!(stdout, "{}\x1b[K", style_summary_text(line, style))?;
        }
        stdout.flush()?;
        self.summary_line_active = true;
        self.summary_lines_active = lines.len().max(1) as u16;
        Ok(())
    }

    /// 清除当前实时摘要行。
    ///
    /// 返回:
    /// - 清除是否成功
    fn clear_summary_lines(&mut self) -> Result<()> {
        if !self.summary_line_active {
            return Ok(());
        }
        let mut stdout = io::stdout();
        let lines = self.summary_lines_active.max(1);
        for index in 0..lines {
            if index > 0 {
                execute!(stdout, crossterm::cursor::MoveUp(1))?;
            }
            execute!(stdout, MoveToColumn(0), Clear(ClearType::CurrentLine))?;
        }
        stdout.flush()?;
        self.summary_line_active = false;
        self.summary_lines_active = 0;
        Ok(())
    }

    /// 生成推理摘要文本。
    ///
    /// 返回:
    /// - 推理摘要文本
    fn reasoning_summary_text(&self) -> String {
        format!(
            "{} · {} {} · {} {}",
            t("thinking", "思考"),
            self.reasoning_lines.max(1),
            t("lines", "行"),
            self.reasoning_chars,
            t("chars", "字符")
        )
    }

    /// 生成工具调用摘要文本。
    ///
    /// 返回:
    /// - 工具调用摘要文本
    fn tool_summary_text(&self) -> String {
        let parts = self
            .tool_stats
            .iter()
            .map(|(name, stats)| {
                let header = tool_status_text(self.display_tool_name(name), stats);
                stats.progress.as_ref().map_or(header.clone(), |message| {
                    let progress = message
                        .lines()
                        .filter(|line| !line.trim().is_empty())
                        .map(|line| format!("· {}", clip_progress_line(line, 120)))
                        .collect::<Vec<_>>()
                        .join("\n");
                    if progress.is_empty() {
                        header
                    } else {
                        format!("{header}\n{progress}")
                    }
                })
            })
            .collect::<Vec<_>>()
            .join(", ");
        format!("{}: {parts}", t("tools", "工具"))
    }

    /// 返回展示用工具名称。
    ///
    /// 参数:
    /// - `name`: 工具原始名称
    ///
    /// 返回:
    /// - 展示名称
    fn display_tool_name<'a>(&self, name: &'a str) -> &'a str {
        if self.readable_tool_names {
            readable_tool_name(name)
        } else {
            name
        }
    }

    /// 隐藏终端光标。
    ///
    /// 返回:
    /// - 操作是否成功
    fn hide_cursor(&mut self) -> Result<()> {
        if !self.cursor_hidden {
            execute!(io::stdout(), Hide)?;
            self.cursor_hidden = true;
        }
        Ok(())
    }

    /// 显示终端光标。
    ///
    /// 返回:
    /// - 操作是否成功
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
    progress: Option<String>,
}

#[derive(Clone, Copy)]
enum SummaryStyle {
    Reasoning,
    Tool,
}

/// 为摘要文本添加终端样式。
///
/// 参数:
/// - `text`: 摘要文本
/// - `style`: 摘要类型
///
/// 返回:
/// - 带 ANSI 样式的摘要文本
fn style_summary_text(text: &str, style: SummaryStyle) -> String {
    match style {
        SummaryStyle::Reasoning => format!("\x1b[2m\x1b[36m{text}\x1b[0m"),
        SummaryStyle::Tool => format!("\x1b[2m{text}\x1b[0m"),
    }
}

/// 生成工具状态文本。
///
/// 参数:
/// - `name`: 工具展示名称
/// - `stats`: 工具调用统计
///
/// 返回:
/// - 工具状态摘要
fn tool_status_text(name: &str, stats: &ToolStats) -> String {
    let calls = stats.calls.max(stats.ok + stats.error).max(1);
    let running = stats.calls.saturating_sub(stats.ok + stats.error);
    if calls == 1 {
        if running > 0 {
            return format!("{name}×1 {}", t("running", "运行中"));
        }
        if stats.error > 0 {
            return format!("{name}×1 err");
        }
        if stats.ok > 0 {
            return format!("{name}×1 ok");
        }
    }
    if running > 0 {
        format!(
            "{name}×{calls} {}:{} ok:{} err:{}",
            t("running", "运行中"),
            running,
            stats.ok,
            stats.error
        )
    } else {
        format!("{name}×{calls} ok:{} err:{}", stats.ok, stats.error)
    }
}

/// 压缩进度文本为单行。
///
/// 参数:
/// - `text`: 原始文本
/// - `max_chars`: 最大字符数
///
/// 返回:
/// - 压缩后的文本
fn clip_progress_line(text: &str, max_chars: usize) -> String {
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if text.chars().count() <= max_chars {
        text
    } else {
        format!(
            "{}...",
            text.chars()
                .take(max_chars.saturating_sub(3))
                .collect::<String>()
        )
    }
}

impl Drop for StreamRenderer {
    fn drop(&mut self) {
        if self.summary_line_active {
            let _ = self.clear_summary_lines();
            eprintln!();
        }
        let _ = self.show_cursor();
        let _ = execute!(io::stdout(), ResetColor);
    }
}

/// 归一化流式文本换行。
///
/// 参数:
/// - `text`: 原始文本
///
/// 返回:
/// - 使用 `\n` 的文本
fn normalize_stream_text(text: &str) -> String {
    text.replace("\r\n", "\n").replace('\r', "\n")
}

/// 打印推理内容块。
///
/// 参数:
/// - `reasoning`: 推理内容
///
/// 返回:
/// - 打印是否成功
fn print_reasoning(reasoning: &str) -> Result<()> {
    let mut stdout = io::stdout();
    execute!(stdout, SetForegroundColor(Color::DarkCyan))?;
    writeln!(stdout, "{}", t("thinking", "思考"))?;
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
#[path = "stream_tests.rs"]
mod tests;
