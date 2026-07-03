mod wait_spinner;

use crate::i18n::text as t;
use crate::llm::{ChatResult, ChatStreamChunk, ChatStreamKind};
use crate::render::wait_spinner::{SpinnerStyle, WaitSpinner};
use anyhow::Result;
use crossterm::cursor::{Hide, MoveToColumn, Show};
use crossterm::style::{Color, ResetColor, SetForegroundColor};
use crossterm::terminal::{Clear, ClearType};
use crossterm::{execute, terminal};
use serde_json::Value;
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
    reasoning_line_open: bool,
    tool_stats: BTreeMap<String, ToolStats>,
    readable_tool_names: bool,
    summary_line_active: bool,
    summary_lines_active: u16,
    last_tool_summary: String,
    live_summary: bool,
    wait_spinner: Option<WaitSpinner>,
    subagent_mode: Option<ChatStreamKind>,
    subagent_reasoning_chars: usize,
    subagent_reasoning_lines: usize,
    subagent_reasoning_line_open: bool,
}

impl StreamRenderer {
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
            reasoning_line_open: false,
            tool_stats: BTreeMap::new(),
            readable_tool_names,
            summary_line_active: false,
            summary_lines_active: 0,
            last_tool_summary: String::new(),
            live_summary: io::stdout().is_terminal(),
            wait_spinner: None,
            subagent_mode: None,
            subagent_reasoning_chars: 0,
            subagent_reasoning_lines: 0,
            subagent_reasoning_line_open: false,
        }
    }

    pub fn start_waiting(&mut self) -> Result<()> {
        if self.plain || self.wait_spinner.is_some() || !WaitSpinner::supported() {
            return Ok(());
        }
        self.hide_cursor()?;
        self.wait_spinner = Some(WaitSpinner::start(
            t("thinking", "思考").to_string(),
            SpinnerStyle::Scanner,
        ));
        Ok(())
    }

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
            self.record_reasoning_text(&text);
            self.mode = Some(ChatStreamKind::Reasoning);
            self.ensure_waiting_phase(self.reasoning_summary_text())?;
            return Ok(());
        }
        self.stop_waiting()?;
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

    pub fn write_tool_call(&mut self, name: &str, arguments: &str) -> Result<()> {
        if self.plain {
            return Ok(());
        }
        let use_tool_spinner = self.tool_call_mode == ToolCallDisplayMode::Summary
            && !is_silent_tool(name)
            && name != "run_command";
        if !use_tool_spinner {
            self.stop_waiting()?;
        }
        self.end_active_stream_line()?;
        self.finalize_reasoning_summary()?;
        if is_silent_tool(name) {
            if self.summary_line_active {
                self.clear_summary_lines()?;
            }
            return Ok(());
        }
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
            self.ensure_tool_waiting_phase()?;
        }
        Ok(())
    }

    pub fn write_tool_result(&mut self, name: &str, ok: bool, output: &str) -> Result<()> {
        if self.plain {
            return Ok(());
        }
        self.stop_waiting()?;
        self.end_subagent_stream_line()?;
        if is_silent_tool(name) && ok {
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
                    let mut stdout = io::stdout();
                    write_command_error_block(&mut stdout, output)?;
                    stdout.flush()?;
                }
                stats.progress = None;
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
            }
            stats.progress = None;
            let summary = self.tool_summary_text();
            self.last_tool_summary = summary.clone();
            self.render_summary_line(&summary, SummaryStyle::Tool)?;
        }
        Ok(())
    }

    pub fn write_tool_progress(&mut self, name: &str, message: &str) -> Result<()> {
        if self.plain {
            return Ok(());
        }
        if message == "__external_output__" {
            self.prepare_for_external_output()?;
            return Ok(());
        }
        if let Some(text) = message.strip_prefix("__subagent_reasoning__") {
            let text = normalize_stream_text(text);
            if self.tool_call_mode == ToolCallDisplayMode::Full {
                self.stop_waiting()?;
                if self.subagent_mode != Some(ChatStreamKind::Reasoning) {
                    self.end_subagent_stream_line()?;
                    let mut stdout = io::stdout();
                    if self.subagent_mode.is_some() {
                        writeln!(stdout)?;
                    }
                    execute!(stdout, SetForegroundColor(Color::Green))?;
                    writeln!(stdout)?;
                    stdout.flush()?;
                }
                let mut stdout = io::stdout();
                write!(stdout, "{text}")?;
                stdout.flush()?;
                self.subagent_mode = Some(ChatStreamKind::Reasoning);
            } else if self.tool_call_mode == ToolCallDisplayMode::Summary {
                self.record_subagent_reasoning_text(&text);
                let summary = self.subagent_reasoning_summary_text();
                self.tool_stats
                    .entry(name.to_string())
                    .or_default()
                    .progress = Some(summary);
                self.update_tool_summary_display()?;
            }
            return Ok(());
        }
        if let Some(json) = message.strip_prefix("__subtool_call__") {
            if let Ok(value) = serde_json::from_str::<Value>(json) {
                let tool_name = value
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown");
                if self.tool_call_mode == ToolCallDisplayMode::Full {
                    let args = value.get("args").and_then(Value::as_str).unwrap_or("");
                    self.stop_waiting()?;
                    self.end_subagent_stream_line()?;
                    self.end_active_stream_line()?;
                    self.finalize_reasoning_summary()?;
                    let mut stdout = io::stdout();
                    if tool_name == "run_command" {
                        write_command_block(&mut stdout, args)?;
                    } else {
                        writeln!(stdout, "tool {}", self.display_tool_name(tool_name))?;
                        write_tool_payload(&mut stdout, t("args", "参数"), args)?;
                    }
                    stdout.flush()?;
                }
            }
            return Ok(());
        }
        if let Some(json) = message.strip_prefix("__subtool_result__") {
            if let Ok(value) = serde_json::from_str::<Value>(json) {
                let tool_name = value
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown");
                let ok = value.get("ok").and_then(Value::as_bool).unwrap_or(true);
                if self.tool_call_mode == ToolCallDisplayMode::Full {
                    let output = value.get("output").and_then(Value::as_str).unwrap_or("");
                    let status = if ok { "ok" } else { "err" };
                    self.stop_waiting()?;
                    self.end_subagent_stream_line()?;
                    self.end_active_stream_line()?;
                    self.finalize_reasoning_summary()?;
                    let mut stdout = io::stdout();
                    if tool_name == "run_command" {
                        write_command_result_blocks(&mut stdout, output)?;
                    } else {
                        writeln!(
                            stdout,
                            "result {} {status}",
                            self.display_tool_name(tool_name)
                        )?;
                        write_tool_payload(&mut stdout, t("output", "输出"), output)?;
                    }
                    stdout.flush()?;
                }
            }
            return Ok(());
        }
        if is_silent_tool(name) {
            return Ok(());
        }
        if self.tool_call_mode == ToolCallDisplayMode::Full {
            self.stop_waiting()?;
            self.end_subagent_stream_line()?;
            self.end_active_stream_line()?;
            self.finalize_reasoning_summary()?;
            let mut stdout = io::stdout();
            writeln!(
                stdout,
                "progress {}: {message}",
                self.display_tool_name(name)
            )?;
            stdout.flush()?;
        } else if self.tool_call_mode == ToolCallDisplayMode::Summary {
            self.subagent_reasoning_chars = 0;
            self.subagent_reasoning_lines = 0;
            self.subagent_reasoning_line_open = false;
            self.tool_stats
                .entry(name.to_string())
                .or_default()
                .progress = Some(message.to_string());
            self.update_tool_summary_display()?;
        }
        Ok(())
    }

    fn update_tool_summary_display(&mut self) -> Result<()> {
        if self.wait_spinner.is_some() {
            let header = self.tool_summary_header();
            let sub = self.tool_summary_progress();
            self.set_tool_waiting_phase(&header, sub.as_deref());
        } else {
            self.end_active_stream_line()?;
            self.finalize_reasoning_summary()?;
            let new_summary = self.tool_summary_text();
            if self.summary_line_active && new_summary == self.last_tool_summary {
                return Ok(());
            }
            self.last_tool_summary = new_summary.clone();
            self.render_summary_line(&new_summary, SummaryStyle::Tool)?;
        }
        Ok(())
    }

    pub fn prepare_for_external_output(&mut self) -> Result<()> {
        self.stop_waiting()?;
        self.end_subagent_stream_line()?;
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

    pub fn finish(&mut self) -> Result<()> {
        self.stop_waiting()?;
        self.end_subagent_stream_line()?;
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
        if self.summary_line_active {
            self.clear_summary_lines()?;
        }
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
                execute!(stdout, SetForegroundColor(Color::Green))?;
                writeln!(stdout)?;
            }
            ChatStreamKind::Content => {
                if self.mode == Some(ChatStreamKind::Reasoning) {
                    execute!(stdout, ResetColor)?;
                    writeln!(stdout)?;
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
        let was_reasoning = self.mode == Some(ChatStreamKind::Reasoning);
        if was_reasoning {
            execute!(io::stdout(), ResetColor)?;
        } else if self.mode == Some(ChatStreamKind::Content) && !self.plain {
            let mut stdout = io::stdout();
            write!(stdout, "{}", self.markdown.flush())?;
            stdout.flush()?;
        }
        if self.mode.is_some() {
            println!();
            if was_reasoning {
                println!();
            }
            self.mode = None;
        }
        Ok(())
    }

    fn finalize_reasoning_summary(&mut self) -> Result<()> {
        if self.reasoning_mode == ReasoningDisplayMode::Summary && self.reasoning_chars > 0 {
            self.stop_waiting()?;
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
            self.reasoning_line_open = false;
            self.mode = None;
            println!();
        }
        Ok(())
    }

    fn end_subagent_stream_line(&mut self) -> Result<()> {
        let was_reasoning = self.subagent_mode == Some(ChatStreamKind::Reasoning);
        if was_reasoning {
            execute!(io::stdout(), ResetColor)?;
        }
        if self.subagent_mode.is_some() {
            println!();
            if was_reasoning {
                println!();
            }
            self.subagent_mode = None;
        }
        Ok(())
    }

    fn record_subagent_reasoning_text(&mut self, text: &str) {
        for ch in text.chars() {
            self.subagent_reasoning_chars += 1;
            if ch == '\n' {
                if self.subagent_reasoning_line_open {
                    self.subagent_reasoning_lines += 1;
                    self.subagent_reasoning_line_open = false;
                }
            } else {
                self.subagent_reasoning_line_open = true;
            }
        }
    }

    fn subagent_reasoning_summary_text(&self) -> String {
        let line_count = self.subagent_reasoning_lines
            + usize::from(self.subagent_reasoning_line_open);
        format!(
            "{} · {} {} · {} {}",
            t("thinking", "思考"),
            line_count,
            t("lines", "行"),
            self.subagent_reasoning_chars,
            t("chars", "字符")
        )
    }

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
            self.last_tool_summary.clear();
        }
        Ok(())
    }

    fn render_summary_line(&mut self, text: &str, style: SummaryStyle) -> Result<()> {
        self.stop_waiting()?;
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

    fn reasoning_summary_text(&self) -> String {
        format!(
            "{} · {} {} · {} {}",
            t("thinking", "思考"),
            self.reasoning_line_count(),
            t("lines", "行"),
            self.reasoning_chars,
            t("chars", "字符")
        )
    }

    fn record_reasoning_text(&mut self, text: &str) {
        for ch in text.chars() {
            self.reasoning_chars += 1;
            if ch == '\n' {
                if self.reasoning_line_open {
                    self.reasoning_lines += 1;
                    self.reasoning_line_open = false;
                }
            } else {
                self.reasoning_line_open = true;
            }
        }
    }

    fn reasoning_line_count(&self) -> usize {
        self.reasoning_lines + usize::from(self.reasoning_line_open)
    }

    fn tool_summary_text(&self) -> String {
        let parts = self
            .tool_stats
            .iter()
            .map(|(name, stats)| {
                let header = tool_status_text(&self.display_tool_name(name), stats, is_subagent_tool(name));
                stats.progress.as_ref().map_or(header.clone(), |message| {
                    let progress = message
                        .lines()
                        .filter(|line| !line.trim().is_empty())
                        .map(|line| format!("↳ {}", clip_progress_line(line, 120)))
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

    fn tool_summary_header(&self) -> String {
        let parts = self
            .tool_stats
            .iter()
            .map(|(name, stats)| tool_status_text(&self.display_tool_name(name), stats, is_subagent_tool(name)))
            .collect::<Vec<_>>()
            .join(", ");
        format!("{}: {parts}", t("tools", "工具"))
    }

    fn tool_summary_progress(&self) -> Option<String> {
        for (_name, stats) in &self.tool_stats {
            if let Some(message) = &stats.progress {
                let progress = message
                    .lines()
                    .filter(|line| !line.trim().is_empty())
                    .map(|line| format!("↳ {}", clip_progress_line(line, 120)))
                    .collect::<Vec<_>>()
                    .join("\n");
                if !progress.is_empty() {
                    return Some(progress);
                }
            }
        }
        None
    }

    fn display_tool_name<'a>(&self, name: &'a str) -> &'a str {
        if self.readable_tool_names {
            readable_tool_name(name)
        } else {
            name
        }
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

    fn set_waiting_phase(&mut self, phase: String) {
        if let Some(spinner) = &self.wait_spinner {
            spinner.set_phase(phase);
        }
    }

    fn ensure_waiting_phase(&mut self, phase: String) -> Result<()> {
        if self.plain || !WaitSpinner::supported() {
            if self.summary_line_active {
                self.clear_summary_lines()?;
            }
            self.render_summary_line(&phase, SummaryStyle::Reasoning)?;
            return Ok(());
        }
        if self.wait_spinner.is_none() {
            self.wait_spinner = Some(WaitSpinner::start(phase, SpinnerStyle::Scanner));
        } else {
            self.set_waiting_phase(phase);
        }
        Ok(())
    }

    fn ensure_tool_waiting_phase(&mut self) -> Result<()> {
        let header = self.tool_summary_header();
        let sub = self.tool_summary_progress();
        if self.plain || !WaitSpinner::supported() {
            let summary = match &sub {
                Some(s) => format!("{header}\n{s}"),
                None => header,
            };
            if self.summary_line_active {
                self.clear_summary_lines()?;
            }
            self.last_tool_summary = summary.clone();
            self.render_summary_line(&summary, SummaryStyle::Tool)?;
            return Ok(());
        }
        if self.summary_line_active {
            self.clear_summary_lines()?;
        }
        if self.wait_spinner.is_none() {
            self.wait_spinner = Some(WaitSpinner::start(header, SpinnerStyle::Braille));
        } else {
            self.set_waiting_phase(header);
        }
        if let Some(spinner) = &self.wait_spinner {
            spinner.set_sub_phase(sub);
        }
        Ok(())
    }

    fn set_tool_waiting_phase(&mut self, header: &str, sub: Option<&str>) {
        if let Some(spinner) = &self.wait_spinner {
            spinner.set_phase(header.to_string());
            spinner.set_sub_phase(sub.map(|s| s.to_string()));
        }
    }

    fn stop_waiting(&mut self) -> Result<()> {
        if let Some(mut spinner) = self.wait_spinner.take() {
            spinner.stop()?;
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

fn style_summary_text(text: &str, style: SummaryStyle) -> String {
    match style {
        SummaryStyle::Reasoning => format!("\x1b[2m\x1b[36m{text}\x1b[0m"),
        SummaryStyle::Tool => format!("\x1b[2m{text}\x1b[0m"),
    }
}

fn tool_status_text(name: &str, stats: &ToolStats, subagent: bool) -> String {
    let calls = stats.calls.max(stats.ok + stats.error).max(1);
    let running = stats.calls.saturating_sub(stats.ok + stats.error);
    if calls == 1 {
        let suffix = if subagent { "" } else { "×1" };
        if running > 0 {
            return format!("{name}{suffix} {}", t("running", "运行中"));
        }
        if stats.error > 0 {
            return format!("{name}{suffix} err");
        }
        if stats.ok > 0 {
            return format!("{name}{suffix} ok");
        }
    }
    if running > 0 {
        let mut text = format!(
            "{name}×{calls} {}:{} ok:{}",
            t("running", "运行中"),
            running,
            stats.ok,
        );
        if stats.error > 0 {
            text.push_str(&format!(" err:{}", stats.error));
        }
        text
    } else if stats.error > 0 {
        format!("{name}×{calls} ok:{} err:{}", stats.ok, stats.error)
    } else {
        format!("{name}×{calls} ok:{}", stats.ok)
    }
}

fn is_silent_tool(name: &str) -> bool {
    matches!(name, "show_meme")
}

fn is_subagent_tool(name: &str) -> bool {
    matches!(
        name,
        "linux_input_method_diagnose" | "linux_game_compatibility" | "deep_research"
    )
}

fn readable_tool_name(name: &str) -> &str {
    crate::tools::readable_tool_name(name)
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
    in_math_block: bool,
    code_lang: String,
    code_buffer: Vec<String>,
    table_buffer: Vec<String>,
    active_table: Option<ActiveTable>,
}

struct ActiveTable {
    widths: Vec<usize>,
    alignments: Vec<TableAlign>,
}

impl MarkdownLineRenderer {
    fn new() -> Self {
        Self {
            in_code_block: false,
            in_math_block: false,
            code_lang: String::new(),
            code_buffer: Vec::new(),
            table_buffer: Vec::new(),
            active_table: None,
        }
    }

    fn render_line(&mut self, line: &str) -> String {
        if line.trim_start().starts_with("```") {
            if self.in_code_block {
                self.in_code_block = false;
                let code = render_code_block(&self.code_lang, &self.code_buffer);
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
            let pending = self.flush();
            self.in_math_block = !self.in_math_block;
            return format!("{pending}\x1b[36m$$\x1b[0m\n");
        }
        if self.in_math_block {
            return format!("\x1b[36m{}\x1b[0m\n", line.trim_end());
        }
        if let Some(table) = &self.active_table {
            if looks_like_table_row(line) {
                let row = parse_table_row(line);
                let mut output = middle_table_border(&table.widths);
                output.push_str(&render_table_row(
                    &row,
                    &table.widths,
                    &table.alignments,
                    false,
                ));
                return output;
            }
            let mut output = bottom_table_border(&table.widths);
            self.active_table = None;
            output.push_str(&self.render_line(line));
            return output;
        }
        if looks_like_table_row(line) {
            self.table_buffer.push(line.to_string());
            if self.table_buffer.len() < 3 {
                return String::new();
            }
            let second = self.table_buffer.get(1).cloned().unwrap_or_default();
            if is_table_separator(&second) {
                let header =
                    parse_table_row(self.table_buffer.first().map(String::as_str).unwrap_or(""));
                let alignments = parse_table_alignments(&second);
                let first_row =
                    parse_table_row(self.table_buffer.get(2).map(String::as_str).unwrap_or(""));
                let widths = table_widths_for_rows(&[header.clone(), first_row.clone()]);
                self.table_buffer.clear();
                self.active_table = Some(ActiveTable {
                    widths: widths.clone(),
                    alignments: alignments.clone(),
                });
                let mut output = top_table_border(&widths);
                output.push_str(&render_table_row(&header, &widths, &alignments, true));
                output.push_str(&middle_table_border(&widths));
                output.push_str(&render_table_row(&first_row, &widths, &alignments, false));
                return output;
            }
            return self.flush();
        }
        let mut output = self.flush();
        output.push_str(&render_markdown_line(line));
        output.push('\n');
        output
    }

    fn flush(&mut self) -> String {
        if self.in_code_block {
            self.in_code_block = false;
            let output = render_code_block(&self.code_lang, &self.code_buffer);
            self.code_lang.clear();
            self.code_buffer.clear();
            return output;
        }
        if let Some(table) = self.active_table.take() {
            return bottom_table_border(&table.widths);
        }
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
    let prefix = "#".repeat(level);
    Some(format!(
        "{HEADER_STYLE}{prefix} {}{RESET}",
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
                        output.push_str(IMAGE_STYLE);
                        output.push_str("[image");
                        if !alt.is_empty() {
                            output.push_str(": ");
                            output.push_str(&alt);
                        }
                        output.push_str("]");
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
            if let Some(end) = find_marker(&chars, index + 1, '`') {
                output.push_str(INLINE_CODE_STYLE);
                output.extend(chars[index + 1..end].iter());
                output.push_str(RESET);
                index = end + 1;
                continue;
            }
        }
        if index + 1 < chars.len() && chars[index] == '$' && chars[index + 1] == '$' {
            if let Some(end) = find_double_marker(&chars, index + 2, '$') {
                output.push_str(MATH_STYLE);
                output.push_str("$$ ");
                output.extend(chars[index + 2..end].iter());
                output.push_str(" $$");
                output.push_str(RESET);
                index = end + 2;
                continue;
            }
        }
        if chars[index] == '$' {
            if let Some(end) = find_marker(&chars, index + 1, '$') {
                output.push_str(MATH_STYLE);
                output.push('$');
                output.extend(chars[index + 1..end].iter());
                output.push('$');
                output.push_str(RESET);
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

const RESET: &str = "\x1b[0m";
const PRIMARY_STYLE: &str = "\x1b[38;5;189m";
const SECONDARY_STYLE: &str = "\x1b[36m";
const TERTIARY_STYLE: &str = "\x1b[35m";
const HEADER_STYLE: &str = "\x1b[1m\x1b[35m";
const INLINE_CODE_STYLE: &str = SECONDARY_STYLE;
const LINK_LABEL_STYLE: &str = "\x1b[38;5;117m";
const URL_STYLE: &str = "\x1b[2m\x1b[38;5;75m";
const IMAGE_STYLE: &str = "\x1b[38;5;183m";
const MATH_STYLE: &str = "\x1b[38;5;117m";
const BOLD_STYLE: &str = "\x1b[1m\x1b[34m";
const ITALIC_STYLE: &str = "\x1b[3m\x1b[38;5;250m";
const STRIKE_STYLE: &str = "\x1b[9m";
const CODE_BLOCK_BG: &str = "";
const CODE_BLOCK_FRAME_STYLE: &str = SECONDARY_STYLE;
const CODE_TOKEN_RESET: &str = "\x1b[0m";
const CODE_KEYWORD_STYLE: &str = "\x1b[38;2;196;167;231m";
const CODE_FUNCTION_STYLE: &str = "\x1b[38;2;156;207;216m";
const CODE_STRING_STYLE: &str = "\x1b[38;2;166;214;160m";
const CODE_NUMBER_STYLE: &str = "\x1b[38;2;246;193;119m";
const CODE_COMMENT_STYLE: &str = "\x1b[32m";

fn render_url(url: &str) -> String {
    format!("{URL_STYLE}{url}{RESET}")
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
        .map(|(width, _)| usize::from(width) / 3)
        .unwrap_or(24)
        .clamp(16, 40);
    format!("\x1b[2m{}\x1b[0m", "─".repeat(width))
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
    let widths = table_widths_for_rows(&rows);
    let mut output = String::new();
    output.push_str(&top_table_border(&widths));
    for (row_index, row) in rows.iter().enumerate() {
        output.push_str(&render_table_row(row, &widths, &alignments, row_index == 0));
        if row_index + 1 < rows.len() {
            output.push_str(&middle_table_border(&widths));
        }
    }
    output.push_str(&bottom_table_border(&widths));
    output
}

fn parse_table_row(line: &str) -> Vec<String> {
    line.trim()
        .trim_matches('|')
        .split('|')
        .map(|cell| render_inline(cell.trim()))
        .collect()
}

fn table_widths_for_rows(rows: &[Vec<String>]) -> Vec<usize> {
    let cols = rows.iter().map(Vec::len).max().unwrap_or(0);
    let mut widths = vec![0usize; cols];
    for row in rows {
        for (index, cell) in row.iter().enumerate() {
            widths[index] = widths[index].max(visible_width(cell));
        }
    }
    let readable_min = readable_table_min_width(cols);
    for width in &mut widths {
        *width = (*width).max(readable_min);
    }
    bounded_table_widths(widths)
}

fn readable_table_min_width(cols: usize) -> usize {
    match cols {
        0 => 0,
        1 => 16,
        2 => 14,
        3 | 4 => 10,
        _ => 8,
    }
}

fn render_table_row(
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
        push_table_vertical(&mut output);
        for (index, width) in widths.iter().enumerate() {
            let cell = wrapped
                .get(index)
                .and_then(|lines| lines.get(line_index))
                .map(String::as_str)
                .unwrap_or("");
            let cell = if header && !cell.is_empty() {
                format!("{BOLD_STYLE}{cell}{RESET}")
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
            push_table_vertical(&mut output);
        }
        output.push('\n');
    }
    output
}

fn top_table_border(widths: &[usize]) -> String {
    table_border(widths, '┌', '┬', '┐')
}

fn middle_table_border(widths: &[usize]) -> String {
    table_border(widths, '├', '┼', '┤')
}

fn bottom_table_border(widths: &[usize]) -> String {
    table_border(widths, '└', '┴', '┘')
}

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
        if width <= 1 {
            break;
        }
        widths[index] -= 1;
    }
    widths
}

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

fn char_display_width(ch: char) -> usize {
    if ch.is_ascii() {
        1
    } else {
        2
    }
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
        output.push_str(&"─".repeat(width + 2));
        output.push(if index + 1 == widths.len() {
            right
        } else {
            mid
        });
    }
    output.push_str("\x1b[0m\n");
    output
}

fn push_table_vertical(output: &mut String) {
    output.push_str("\x1b[2m│\x1b[0m");
}

fn highlight_code_line(lang: &str, line: &str) -> String {
    let lang = lang.trim().to_ascii_lowercase();
    if lang.is_empty() {
        return line.to_string();
    }
    let comment_marker = match lang.as_str() {
        "py" | "python" | "sh" | "bash" | "zsh" | "fish" | "toml" | "yaml" | "yml" => Some('#'),
        "rs" | "rust" | "js" | "ts" | "tsx" | "jsx" | "c" | "cpp" | "java" | "go" => None,
        _ => None,
    };
    let mut output = String::new();
    let chars = line.chars().collect::<Vec<_>>();
    let mut index = 0;
    while index < chars.len() {
        if let Some(marker) = comment_marker {
            if chars[index] == marker {
                output.push_str(CODE_COMMENT_STYLE);
                output.extend(chars[index..].iter());
                output.push_str(CODE_TOKEN_RESET);
                return output;
            }
        }
        if index + 1 < chars.len() && chars[index] == '/' && chars[index + 1] == '/' {
            output.push_str(CODE_COMMENT_STYLE);
            output.extend(chars[index..].iter());
            output.push_str(CODE_TOKEN_RESET);
            return output;
        }
        if chars[index] == '"'
            || chars[index] == '\''
            || (chars[index] == '`'
                && matches!(lang.as_str(), "js" | "ts" | "tsx" | "jsx" | "sh" | "bash"))
        {
            let quote = chars[index];
            let start = index;
            index += 1;
            let mut escaped = false;
            while index < chars.len() {
                if escaped {
                    escaped = false;
                } else if chars[index] == '\\' {
                    escaped = true;
                } else if chars[index] == quote {
                    index += 1;
                    break;
                }
                index += 1;
            }
            output.push_str(CODE_STRING_STYLE);
            output.extend(chars[start..index].iter());
            output.push_str(CODE_TOKEN_RESET);
            continue;
        }
        if chars[index].is_ascii_digit() {
            let start = index;
            index += 1;
            while index < chars.len()
                && (chars[index].is_ascii_alphanumeric() || matches!(chars[index], '_' | '.'))
            {
                index += 1;
            }
            output.push_str(CODE_NUMBER_STYLE);
            output.extend(chars[start..index].iter());
            output.push_str(CODE_TOKEN_RESET);
            continue;
        }
        if is_code_word_start(chars[index]) {
            let start = index;
            index += 1;
            while index < chars.len() && is_code_word_char(chars[index]) {
                index += 1;
            }
            let token = chars[start..index].iter().collect::<String>();
            let style = if code_keywords(&lang).contains(&token.as_str()) {
                Some(CODE_KEYWORD_STYLE)
            } else if matches!(
                token.as_str(),
                "true" | "false" | "null" | "None" | "Some" | "Ok" | "Err"
            ) {
                Some(CODE_NUMBER_STYLE)
            } else if next_non_space_is_open_paren(&chars, index) {
                Some(CODE_FUNCTION_STYLE)
            } else {
                None
            };
            if let Some(style) = style {
                output.push_str(style);
                output.push_str(&token);
                output.push_str(CODE_TOKEN_RESET);
            } else {
                output.push_str(PRIMARY_STYLE);
                output.push_str(&token);
                output.push_str(CODE_TOKEN_RESET);
            }
            continue;
        }
        output.push(chars[index]);
        index += 1;
    }
    output
}

fn render_code_block(lang: &str, lines: &[String]) -> String {
    let label = if lang.is_empty() {
        "code".to_string()
    } else {
        format!("code {lang}")
    };
    let header = format!("-- {label}");
    let footer = "--";
    let width = lines
        .iter()
        .map(|line| line.chars().count())
        .chain([header.chars().count(), footer.chars().count()])
        .max()
        .unwrap_or(footer.len())
        .max(24);
    let mut output = String::new();
    output.push_str(&render_code_block_frame(&header, width));
    output.push('\n');
    for line in lines {
        output.push_str(&render_code_block_line_with_width(lang, line, width));
        output.push('\n');
    }
    output.push_str(&render_code_block_frame(footer, width));
    output.push('\n');
    output
}

fn render_code_block_frame(text: &str, width: usize) -> String {
    if text == "--" {
        return format!("{CODE_BLOCK_FRAME_STYLE}{}{RESET}", "─".repeat(width));
    }
    let label = text.strip_prefix("-- ").unwrap_or(text);
    let prefix = format!("╭─ {label} ");
    format!(
        "{CODE_BLOCK_FRAME_STYLE}{prefix}{}{RESET}",
        "─".repeat(width.saturating_sub(prefix.chars().count()))
    )
}

fn render_code_block_line_with_width(lang: &str, line: &str, width: usize) -> String {
    let line_width = line.chars().count();
    let padding = " ".repeat(width.saturating_sub(line_width));
    let highlighted = highlight_code_line(lang, line);
    if highlighted.is_empty() {
        format!("{CODE_BLOCK_BG}{}{RESET}", " ".repeat(width.max(1)))
    } else {
        format!("{CODE_BLOCK_BG}{highlighted}{padding}{RESET}")
    }
}

fn code_keywords(lang: &str) -> &'static [&'static str] {
    match lang {
        "rs" | "rust" => &[
            "as", "async", "await", "break", "const", "continue", "crate", "else", "enum", "fn",
            "for", "if", "impl", "in", "let", "loop", "match", "mod", "move", "mut", "pub", "ref",
            "return", "self", "Self", "static", "struct", "trait", "type", "unsafe", "use",
            "where", "while",
        ],
        "py" | "python" => &[
            "and", "as", "async", "await", "break", "class", "continue", "def", "elif", "else",
            "except", "finally", "for", "from", "if", "import", "in", "is", "lambda", "not", "or",
            "pass", "raise", "return", "try", "while", "with", "yield",
        ],
        "js" | "ts" | "tsx" | "jsx" => &[
            "async", "await", "break", "case", "catch", "class", "const", "continue", "default",
            "else", "export", "extends", "finally", "for", "from", "function", "if", "import",
            "let", "new", "return", "switch", "throw", "try", "typeof", "var", "while",
        ],
        "sh" | "bash" | "zsh" | "fish" => &[
            "case", "do", "done", "elif", "else", "esac", "fi", "for", "function", "if", "in",
            "then", "while",
        ],
        "json" | "toml" | "yaml" | "yml" => &["true", "false", "null"],
        _ => &[],
    }
}

fn is_code_word_start(ch: char) -> bool {
    ch.is_ascii_alphabetic() || ch == '_'
}

fn is_code_word_char(ch: char) -> bool {
    ch.is_ascii_alphanumeric() || ch == '_'
}

fn next_non_space_is_open_paren(chars: &[char], mut index: usize) -> bool {
    while index < chars.len() && chars[index].is_whitespace() {
        index += 1;
    }
    chars.get(index) == Some(&'(')
}

fn is_table_separator(line: &str) -> bool {
    let trimmed = line.trim().trim_matches('|').trim();
    !trimmed.is_empty()
        && trimmed
            .chars()
            .all(|ch| matches!(ch, '-' | ':' | '|' | ' '))
        && trimmed.contains('-')
}

fn looks_like_table_row(line: &str) -> bool {
    let trimmed = line.trim();
    trimmed.starts_with('|') && trimmed.ends_with('|') && trimmed.matches('|').count() >= 2
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

fn write_command_block(stdout: &mut io::Stdout, arguments: &str) -> Result<()> {
    let parsed = serde_json::from_str::<Value>(arguments).ok();
    let command = parsed
        .as_ref()
        .and_then(|value| value.get("command"))
        .and_then(Value::as_str)
        .unwrap_or(arguments)
        .trim();
    writeln!(stdout, "\x1b[2m,-- {}\x1b[0m", t("command", "命令"))?;
    writeln!(stdout, "\x1b[33m$ {command}\x1b[0m")?;
    writeln!(stdout, "\x1b[2m`--\x1b[0m")?;
    Ok(())
}

fn write_command_result_blocks(stdout: &mut io::Stdout, output: &str) -> Result<()> {
    let Some(result) = parse_command_result(output) else {
        return write_tool_payload(stdout, t("output", "输出"), output);
    };
    if !result.stdout.trim().is_empty() {
        write_fenced_block(stdout, t("output", "输出"), &result.stdout)?;
    }
    if !result.stderr.trim().is_empty() {
        let label = result
            .exit_code
            .map(|code| format!("err exit {code}"))
            .unwrap_or_else(|| "err".to_string());
        write_fenced_block(stdout, &label, &result.stderr)?;
    } else if !result.success {
        let label = result
            .exit_code
            .map(|code| format!("err exit {code}"))
            .unwrap_or_else(|| "err".to_string());
        write_fenced_block(
            stdout,
            &label,
            t(
                "command failed without stderr",
                "命令失败，但没有 stderr 输出",
            ),
        )?;
    }
    Ok(())
}

fn write_command_error_block(stdout: &mut io::Stdout, output: &str) -> Result<()> {
    let Some(result) = parse_command_result(output) else {
        return write_fenced_block(stdout, "err", output);
    };
    if result.success {
        return Ok(());
    }
    let label = result
        .exit_code
        .map(|code| format!("err exit {code}"))
        .unwrap_or_else(|| "err".to_string());
    let message = if result.stderr.trim().is_empty() {
        result.stdout.as_str()
    } else {
        result.stderr.as_str()
    };
    write_fenced_block(stdout, &label, message)
}

fn write_fenced_block(stdout: &mut io::Stdout, label: &str, text: &str) -> Result<()> {
    writeln!(stdout, "\x1b[2m,-- {label}\x1b[0m")?;
    for line in truncate_chars(text.trim(), 2400).lines() {
        writeln!(stdout, "\x1b[33m{line}\x1b[0m")?;
    }
    writeln!(stdout, "\x1b[2m`--\x1b[0m")?;
    Ok(())
}

struct CommandResult {
    success: bool,
    exit_code: Option<i64>,
    stdout: String,
    stderr: String,
}

fn parse_command_result(output: &str) -> Option<CommandResult> {
    let value = serde_json::from_str::<Value>(output.trim()).ok()?;
    Some(CommandResult {
        success: value.get("success")?.as_bool()?,
        exit_code: value.get("exit_code").and_then(Value::as_i64),
        stdout: value
            .get("stdout")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        stderr: value
            .get("stderr")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
    })
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
        "{}\n... {} {omitted} {} ...",
        text.chars().take(max_chars).collect::<String>(),
        t("truncated", "已截断"),
        t("chars", "字符")
    )
}

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
        let _ = self.stop_waiting();
        if self.summary_line_active {
            let _ = self.clear_summary_lines();
            eprintln!();
        }
        let _ = self.show_cursor();
        let _ = execute!(io::stdout(), ResetColor);
    }
}

fn normalize_stream_text(text: &str) -> String {
    text.replace("\r\n", "\n").replace('\r', "\n")
}

fn print_reasoning(reasoning: &str) -> Result<()> {
    let mut stdout = io::stdout();
    execute!(stdout, SetForegroundColor(Color::Green))?;
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
        assert_eq!(
            renderer.push("ld**\n"),
            format!("{BOLD_STYLE}bold{RESET}\n")
        );
    }

    #[test]
    fn flushes_partial_final_line() {
        let mut renderer = MarkdownStreamRenderer::new();
        assert_eq!(renderer.push("# Title"), "");
        assert_eq!(renderer.flush(), format!("{HEADER_STYLE}# Title{RESET}\n"));
    }

    #[test]
    fn headings_use_one_color_and_distinct_prefix_lengths() {
        assert_eq!(
            render_markdown_line("# One"),
            format!("{HEADER_STYLE}# One{RESET}")
        );
        assert_eq!(
            render_markdown_line("## Two"),
            format!("{HEADER_STYLE}## Two{RESET}")
        );
        assert_eq!(
            render_markdown_line("### Three"),
            format!("{HEADER_STYLE}### Three{RESET}")
        );
        assert_eq!(
            render_markdown_line("###### Six"),
            format!("{HEADER_STYLE}###### Six{RESET}")
        );
    }

    #[test]
    fn list_markers_use_tertiary_color() {
        assert!(render_markdown_line("- item").contains(&format!("{TERTIARY_STYLE}-{RESET}")));
        assert!(render_markdown_line("1. item").contains(&format!("{TERTIARY_STYLE}1.{RESET}")));
    }

    #[test]
    fn buffers_tables_until_non_table_line() {
        let mut renderer = MarkdownStreamRenderer::new();
        assert_eq!(renderer.push("| a | b |\n"), "");
        assert_eq!(renderer.push("| - | - |\n"), "");
        let output = renderer.push("| 1 | 2 |\n");
        assert!(output.contains(&format!("{BOLD_STYLE}a{RESET}")));
        assert!(output.contains("1"));
        assert!(output.contains('┌'));
        assert!(output.contains('┬'));
        assert!(output.contains('├'));
        assert!(output.contains('┼'));
        assert!(output.contains("\x1b[2m│\x1b[0m"));
        assert!(output.contains('─'));
        assert!(!output.contains('+'));
        let output = renderer.push("done\n");
        assert!(output.contains('└'));
        assert!(output.ends_with("done\n"));
    }

    #[test]
    fn short_tables_use_content_width() {
        let output = render_table(&[
            "| 项目 | 内容 |".to_string(),
            "|---|---|".to_string(),
            "| 名字 | 未有 / Miyu |".to_string(),
            "| 年龄 | 18 |".to_string(),
        ]);
        let terminal_width = terminal::size()
            .map(|(width, _)| usize::from(width))
            .unwrap_or(100);
        let widest = output.lines().map(visible_width).max().unwrap_or(0);
        assert!(widest < terminal_width / 2, "table too wide: {widest}");
    }

    #[test]
    fn wraps_wide_table_cells_to_terminal_width() {
        let output = render_table(&[
            "| 项目 | 内容 |".to_string(),
            "|---|---|".to_string(),
            format!("| 很长 | {} |", "这是一段非常长的内容".repeat(20)),
        ]);
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

    #[test]
    fn many_column_tables_stay_within_terminal_width() {
        let output = render_table(&[
            "| 参数名 | 参数类型 | 默认值 | 是否必填 | 说明 | 取值范围 | 示例值 | 适用版本 | 更新日志 | 备注 |".to_string(),
            "|---|---|---|---|---|---|---|---|---|---|".to_string(),
            "| database_host | string | localhost | 否 | 数据库主机地址 | 合法IP或域名 | 192.168.1.100 | v1.0+ | 无 | 支持IPv6 |".to_string(),
        ]);
        let terminal_width = terminal::size()
            .map(|(width, _)| usize::from(width))
            .unwrap_or(100);
        for line in output.lines() {
            assert!(
                visible_width(line) < terminal_width,
                "line too wide: {line}"
            );
        }
    }

    #[test]
    fn blockquote_is_visually_distinct() {
        let mut renderer = MarkdownStreamRenderer::new();
        let output = renderer.push(">> quoted\n");
        assert!(output.contains("\x1b[32m| \x1b[0m\x1b[32m| \x1b[0m"));
        assert!(output.contains("\x1b[32mquoted\x1b[0m"));
        assert!(!output.contains("48;5;236"));
    }

    #[test]
    fn code_block_has_label_and_readable_content() {
        let mut renderer = MarkdownStreamRenderer::new();
        let output = renderer.push("```rust\nfn main() {}\n```\n");
        assert!(output.contains("╭─ code rust"));
        assert!(!output.contains(",-- code rust"));
        assert!(!output.contains("\x1b[2m|\x1b[0m"));
        assert!(output.contains(&format!(
            "{CODE_BLOCK_BG}{CODE_KEYWORD_STYLE}fn{CODE_TOKEN_RESET}"
        )));
        assert!(output.contains(&format!("{CODE_FUNCTION_STYLE}main{CODE_TOKEN_RESET}")));
        assert!(output.contains(&format!("{CODE_BLOCK_FRAME_STYLE}╭─ code rust ─")));
        assert!(output.contains(&format!(
            "{CODE_BLOCK_FRAME_STYLE}{}{RESET}",
            "─".repeat(24)
        )));
        assert!(!output.contains("`--"));
    }

    #[test]
    fn code_block_content_has_default_color() {
        let mut renderer = MarkdownStreamRenderer::new();
        let output = renderer.push("```\nXMODIFIERS \"@im=fcitx\"\n```\n");
        assert!(output.contains(&format!(
            "{CODE_BLOCK_BG}XMODIFIERS \"@im=fcitx\"{}{RESET}",
            " ".repeat(2)
        )));
        assert!(!output.contains("\x1b[33mXMODIFIERS"));
    }

    #[test]
    fn code_block_variables_use_primary_color() {
        let mut renderer = MarkdownStreamRenderer::new();
        let output = renderer.push("```rust\nlet msg = String::from(\"hi\");\n```\n");
        assert!(output.contains(&format!("{PRIMARY_STYLE}msg{CODE_TOKEN_RESET}")));
    }

    #[test]
    fn code_block_background_uses_longest_line_width() {
        let mut renderer = MarkdownStreamRenderer::new();
        let output = renderer.push("```\nshort\nlonger line\n```\n");
        assert!(output.contains(&format!("{CODE_BLOCK_BG}short{}{RESET}", " ".repeat(19))));
        assert!(output.contains(&format!(
            "{CODE_BLOCK_BG}longer line{}{RESET}",
            " ".repeat(13)
        )));
        assert!(output.contains(&format!(
            "{CODE_BLOCK_FRAME_STYLE}{}{RESET}",
            "─".repeat(24)
        )));
        assert!(!output.contains("48;5;236"));
    }

    #[test]
    fn renders_more_inline_markdown() {
        let output = render_inline(
            "*i* ~~gone~~ [site](https://example.com) <https://example.org> ![pic](https://img)",
        );
        assert!(output.contains(&format!("{ITALIC_STYLE}i{RESET}")));
        assert!(output.contains(&format!("{STRIKE_STYLE}gone{RESET}")));
        assert!(output.contains(&format!("<{URL_STYLE}https://example.com{RESET}>")));
        assert!(output.contains(&format!(
            "\x1b[4m<{URL_STYLE}https://example.org{RESET}>{RESET}"
        )));
        assert!(output.contains(&format!(
            "{IMAGE_STYLE}[image: pic]{RESET}({URL_STYLE}https://img{RESET})"
        )));
        assert!(!output.contains("\x1b[35mimage\x1b[0m"));
    }

    #[test]
    fn renders_inline_code_at_start_of_bullet() {
        let output = render_markdown_line("- `read_file` — 读文件内容");
        assert!(output.contains(&format!("{INLINE_CODE_STYLE}read_file\x1b[0m")));
        assert!(output.contains("— 读文件内容"));
    }

    #[test]
    fn renders_multiple_inline_code_spans_in_bullet_with_chinese_text() {
        let output = render_markdown_line(
            "- `~/.config/Thunar/` - 里面有 `accels.scm`（快捷键绑定）和 `uca.xml`（自定义右键菜单）",
        );
        assert!(output.contains(&format!("{INLINE_CODE_STYLE}~/.config/Thunar/\x1b[0m")));
        assert!(output.contains(&format!("{INLINE_CODE_STYLE}accels.scm\x1b[0m")));
        assert!(output.contains(&format!("{INLINE_CODE_STYLE}uca.xml\x1b[0m")));
        assert!(!output.contains('`'));
    }

    #[test]
    fn renders_inline_code_when_stream_chunks_split_backticks() {
        let mut renderer = MarkdownStreamRenderer::new();
        assert_eq!(renderer.push("- `~/.config/Thu"), "");
        let output = renderer.push("nar/` - 里面有 `accels.scm`\n");
        assert!(output.contains(&format!("{INLINE_CODE_STYLE}~/.config/Thunar/\x1b[0m")));
        assert!(output.contains(&format!("{INLINE_CODE_STYLE}accels.scm\x1b[0m")));
        assert!(!output.contains('`'));
    }

    #[test]
    fn tool_status_prefers_running_for_single_active_call() {
        let stats = ToolStats {
            calls: 1,
            ok: 0,
            error: 0,
            progress: None,
        };
        assert_eq!(
            tool_status_text("grep", &stats, false),
            "grep×1 运行中"
        );
    }

    #[test]
    fn tool_status_uses_simple_single_success() {
        let stats = ToolStats {
            calls: 1,
            ok: 1,
            error: 0,
            progress: None,
        };
        assert_eq!(
            tool_status_text("grep", &stats, false),
            "grep×1 ok"
        );
    }

    #[test]
    fn tool_status_subagent_tool_omits_count_suffix() {
        let stats = ToolStats {
            calls: 1,
            ok: 0,
            error: 0,
            progress: None,
        };
        assert_eq!(
            tool_status_text("linux_input_method_diagnose", &stats, true),
            "linux_input_method_diagnose 运行中"
        );
        let stats = ToolStats {
            calls: 1,
            ok: 1,
            error: 0,
            progress: None,
        };
        assert_eq!(
            tool_status_text("deep_research", &stats, true),
            "deep_research ok"
        );
    }

    #[test]
    fn tool_status_counts_mixed_multiple_calls() {
        let stats = ToolStats {
            calls: 3,
            ok: 1,
            error: 1,
            progress: None,
        };
        assert_eq!(
            tool_status_text("grep", &stats, false),
            "grep×3 运行中:1 ok:1 err:1"
        );
    }

    #[test]
    fn show_meme_is_a_silent_tool() {
        assert!(is_silent_tool("show_meme"));
        assert!(!is_silent_tool("search_meme"));
    }

    #[test]
    fn readable_tool_names_translate_known_tools_and_fallback_unknown() {
        assert_eq!(readable_tool_name("deep_research"), "深度研究");
        assert_eq!(readable_tool_name("read_file"), "读取文件");
        assert_eq!(readable_tool_name("check_issue"), "检查问题");
        assert_eq!(
            readable_tool_name("linux_input_method_diagnose"),
            "输入法诊断"
        );
        assert_eq!(readable_tool_name("check_os_info"), "查看系统信息");
        assert_eq!(readable_tool_name("get_weather"), "天气查询");
        assert_eq!(readable_tool_name("get_exchange_rate"), "汇率查询");
        assert_eq!(readable_tool_name("draw_zhouyi_hexagram"), "周易起卦");
        assert_eq!(readable_tool_name("draw_tarot_card"), "抽塔罗牌");
        assert_eq!(readable_tool_name("draw_fortune_lot"), "吉凶占");
        assert_eq!(readable_tool_name("vision_analyze"), "分析图片");
        assert_eq!(readable_tool_name("search_meme"), "搜索表情包");
        assert_eq!(readable_tool_name("show_meme"), "发送表情");
        assert_eq!(readable_tool_name("add_meme"), "添加表情包");
        assert_eq!(readable_tool_name("task_agent"), "创建子任务");
        assert_eq!(
            readable_tool_name("upload_text_to_knowledge_base"),
            "导入知识库"
        );
        assert_eq!(readable_tool_name("search_evicted_context"), "搜索旧上下文");
        assert_eq!(readable_tool_name("recall_past_events"), "回忆往事");
        assert_eq!(readable_tool_name("aur_check_status"), "查询 AUR 状态");
        assert_eq!(readable_tool_name("online_man_search"), "搜索在线手册");
        assert_eq!(readable_tool_name("online_man_get_page"), "读取在线手册");
        assert_eq!(
            readable_tool_name("fcitx5_input_method_wiki_qurey"),
            "查询 Fcitx5 Wiki"
        );
        assert_eq!(readable_tool_name("install_aur_package"), "安装 AUR 包");
        assert_eq!(
            readable_tool_name("search_knowledge_base_by_name"),
            "按名称搜索知识库"
        );
        assert_eq!(readable_tool_name("recall_memories"), "召回记忆");
        assert_eq!(readable_tool_name("custom_skill"), "custom_skill");
    }

    #[test]
    fn summary_styles_distinguish_reasoning_from_tools() {
        assert_eq!(
            style_summary_text("工具", SummaryStyle::Tool),
            "\x1b[2m工具\x1b[0m"
        );
        assert_eq!(
            style_summary_text("思考", SummaryStyle::Reasoning),
            "\x1b[2m\x1b[36m思考\x1b[0m"
        );
    }

    #[test]
    fn finish_keeps_pending_reasoning_summary_state() {
        let mut renderer = StreamRenderer::new(
            ReasoningDisplayMode::Summary,
            ToolCallDisplayMode::Summary,
            false,
            true,
        );
        renderer.reasoning_chars = 12;
        renderer.reasoning_lines = 1;
        renderer.reasoning_line_open = true;
        renderer.finish().unwrap();
        assert_eq!(renderer.reasoning_chars, 0);
        assert_eq!(renderer.reasoning_lines, 0);
        assert!(!renderer.reasoning_line_open);
        assert!(!renderer.summary_line_active);
    }

    #[test]
    fn reasoning_summary_counts_streamed_lines() {
        let mut renderer = StreamRenderer::new(
            ReasoningDisplayMode::Summary,
            ToolCallDisplayMode::Summary,
            false,
            true,
        );
        renderer.record_reasoning_text("one\nt");
        renderer.record_reasoning_text("wo\nthree");
        assert_eq!(renderer.reasoning_chars, 13);
        assert_eq!(renderer.reasoning_line_count(), 3);
        assert!(renderer.reasoning_summary_text().contains("3 行"));
    }

    #[test]
    fn keeps_identifier_underscores_literal() {
        let output = render_inline("GTK_IM_MODULE and _italic_");
        assert!(output.contains("GTK_IM_MODULE"));
        assert!(output.contains(&format!("{ITALIC_STYLE}italic{RESET}")));
        assert!(!output.contains("GTK\x1b[3mIM\x1b[0mMODULE"));
        assert_eq!(render_inline("abc_def_ghi"), "abc_def_ghi");
    }

    #[test]
    fn renders_math_formulas_visibly() {
        let output = render_inline("inline $E=mc^2$ and display $$a^2+b^2=c^2$$");
        assert!(output.contains(&format!("{MATH_STYLE}$E=mc^2${RESET}")));
        assert!(output.contains(&format!("{MATH_STYLE}$$ a^2+b^2=c^2 $${RESET}")));
    }

    #[test]
    fn renders_multiline_math_blocks_visibly() {
        let mut renderer = MarkdownStreamRenderer::new();
        let output = renderer.push("$$\na^2 + b^2 = c^2\n$$\n");
        assert!(output.contains("\x1b[36m$$\x1b[0m"));
        assert!(output.contains("\x1b[36ma^2 + b^2 = c^2\x1b[0m"));
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
        let output =
            renderer.push("| left | mid | right |\n| :--- | :---: | ---: |\n| a | b | c |\n");
        let output = format!("{output}{}", renderer.flush());
        assert!(output.contains('┌'));
        assert!(output.contains('│'));
        assert!(!output.contains('+'));
        assert!(!output.contains(":---"));
        assert!(output.contains(&format!("{BOLD_STYLE}left{RESET}")));
    }

    #[test]
    fn does_not_buffer_plain_lines_with_pipes_as_tables() {
        let mut renderer = MarkdownStreamRenderer::new();
        let output = renderer.push("echo hi | wc -l\nnext\n");
        assert!(output.contains("echo hi | wc -l\nnext\n"));
    }

    #[test]
    fn parses_command_result_json() {
        let result = parse_command_result(
            r#"{"success":false,"exit_code":1,"stdout":"unused","stderr":"not found"}"#,
        )
        .unwrap();
        assert!(!result.success);
        assert_eq!(result.exit_code, Some(1));
        assert_eq!(result.stdout, "unused");
        assert_eq!(result.stderr, "not found");
    }
}
