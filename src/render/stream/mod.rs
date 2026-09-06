//! 回合输出的流式渲染器。
//!
//! `StreamRenderer` 是终端这一侧的总入口：模型的文本、推理、工具调用、命令输
//! 出全从它过一遍再落到屏幕上。
//!
//! `SentMemeStreamFilter` 处理的是「已经作为图片发出去的表情，不要在正文里再
//! 打一遍」——过滤要在流式条件下做，所以得记住最长的部分匹配前缀
//! （`longest_sent_meme_prefix_suffix`），不能等看完整段。

mod reasoning_phase;
mod tool_summary;

use crate::render::*;

pub(crate) fn rendered_physical_rows(widths: &[usize], terminal_width: usize) -> u16 {
    let columns = terminal_width.max(1);
    widths
        .iter()
        .map(|width| (*width).max(1).div_ceil(columns))
        .sum::<usize>()
        .min(u16::MAX as usize) as u16
}

pub(crate) enum RenderOutput {
    Terminal,
    Buffered(Vec<u8>),
}

impl Write for RenderOutput {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        match self {
            Self::Terminal => io::stdout().write(bytes),
            Self::Buffered(buffer) => buffer.write(bytes),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            Self::Terminal => io::stdout().flush(),
            Self::Buffered(_) => Ok(()),
        }
    }
}

pub struct StreamRenderer {
    pub(crate) reasoning_mode: ReasoningDisplayMode,
    pub(crate) tool_call_mode: ToolCallDisplayMode,
    pub(crate) plain: bool,
    pub(crate) mode: Option<ChatStreamKind>,
    pub(crate) cursor_hidden: bool,
    pub(crate) external_cursor_control: bool,
    pub(crate) output: RenderOutput,
    pub(crate) markdown: MarkdownStreamRenderer,
    pub(crate) reasoning_text: String,
    pub(crate) reasoning_tokens: usize,
    pub(crate) reasoning_title: Option<String>,
    pub(crate) reasoning_started_at: Option<std::time::Instant>,
    pub(crate) reasoning_elapsed: Option<std::time::Duration>,
    pub(crate) tool_stats: BTreeMap<String, ToolStats>,
    pub(crate) tool_seq: usize,
    pub(crate) readable_tool_names: bool,
    pub(crate) command_output_lines: usize,
    pub(crate) command_display: Option<CommandLiveDisplay>,
    pub(crate) summary_line_active: bool,
    pub(crate) summary_lines_active: u16,
    pub(crate) last_tool_summary: String,
    pub(crate) live_summary: bool,
    pub(crate) wait_spinner: Option<WaitSpinner>,
    pub(crate) last_tick: Option<std::time::Instant>,
    pub(crate) preparing_question_started_at: Option<std::time::Instant>,
    /// Phase text and start time for the "still receiving arguments" hint.
    /// Sticky like `preparing_question_started_at` and for the same reason:
    /// `tick_spinner` re-derives the phase from renderer state on every tick,
    /// so a phase merely pushed into the spinner is overwritten before it can
    /// be drawn.
    pub(crate) tool_preparing: Option<(&'static str, std::time::Instant)>,
    /// 整个准备窗口的起点，跨 write_tool_call 存活。
    ///
    /// `tool_preparing` 每次工具调用完成就被清掉，计时锚点跟着它走的话，
    /// 批量调用里第二个工具一到就归零——屏幕上的秒数来回横跳，反映不出
    /// 已经等了多久。窗口真正结束（新一轮思考／外部输出／工具跑完／回合
    /// 结束）才清这个。
    pub(crate) tool_preparing_since: Option<std::time::Instant>,
    pub(crate) subagent_mode: Option<ChatStreamKind>,
    pub(crate) sent_meme_filter: SentMemeStreamFilter,
    /// 模型正文/思维链的流式转义过滤状态:与命令输出同一套状态机,
    /// 拦截 `\x1b[2J`/OSC 等正文里的终端控制序列(清屏/藏光标/伪造 UI)。
    pub(crate) stream_control: TerminalControlState,
}

impl StreamRenderer {
    pub fn new(
        reasoning_mode: ReasoningDisplayMode,
        tool_call_mode: ToolCallDisplayMode,
        plain: bool,
        readable_tool_names: bool,
        command_output_lines: usize,
    ) -> Self {
        Self {
            reasoning_mode,
            tool_call_mode,
            plain,
            mode: None,
            cursor_hidden: false,
            external_cursor_control: false,
            output: RenderOutput::Terminal,
            markdown: MarkdownStreamRenderer::new(),
            reasoning_text: String::new(),
            reasoning_tokens: 0,
            reasoning_title: None,
            reasoning_started_at: None,
            reasoning_elapsed: None,
            tool_stats: BTreeMap::new(),
            tool_seq: 0,
            readable_tool_names,
            command_output_lines,
            command_display: None,
            summary_line_active: false,
            summary_lines_active: 0,
            last_tool_summary: String::new(),
            live_summary: io::stdout().is_terminal(),
            wait_spinner: None,
            last_tick: None,
            preparing_question_started_at: None,
            tool_preparing: None,
            tool_preparing_since: None,
            subagent_mode: None,
            sent_meme_filter: SentMemeStreamFilter::default(),
            stream_control: TerminalControlState::default(),
        }
    }

    pub fn use_external_cursor_control(&mut self) {
        self.external_cursor_control = true;
    }

    pub fn use_buffered_output(&mut self) {
        self.output = RenderOutput::Buffered(Vec::new());
    }

    pub fn take_output_frame(&mut self) -> Vec<u8> {
        match &mut self.output {
            RenderOutput::Terminal => Vec::new(),
            RenderOutput::Buffered(buffer) => std::mem::take(buffer),
        }
    }

    pub fn write_chunk(&mut self, chunk: ChatStreamChunk) -> Result<()> {
        if chunk.kind == ChatStreamKind::ToolCall {
            if chunk.text == "ask_question" {
                self.start_preparing_question()?;
            }
            return Ok(());
        }
        if matches!(
            chunk.kind,
            ChatStreamKind::ReasoningPartStart
                | ChatStreamKind::ReasoningPartEnd
                | ChatStreamKind::ReasoningReset
        ) {
            return Ok(());
        }
        if !self.plain {
            self.hide_cursor()?;
        }
        let text = normalize_stream_text(&chunk.text);
        // 正文/思维链与命令输出同权:全部过转义状态机,模型输出里的
        // `\x1b[2J`、OSC 8 等控制序列不能直接打到用户终端上生效。
        // 状态跨 delta 持有,序列被 delta 切断也拦得住。
        let text = sanitize_stream_chunk(&mut self.stream_control, &text);
        let text = if chunk.kind == ChatStreamKind::Content {
            self.sent_meme_filter.push(&text)
        } else {
            text
        };
        if text.is_empty() {
            return Ok(());
        }
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
            self.ensure_waiting_phase(self.reasoning_live_text(), SpinnerStyle::Scanner)?;
            return Ok(());
        }
        self.stop_waiting()?;
        if self.mode != Some(chunk.kind) {
            if chunk.kind == ChatStreamKind::Content {
                self.finalize_reasoning_summary()?;
                self.finalize_tools_summary()?;
            } else if chunk.kind == ChatStreamKind::Reasoning {
                self.finalize_tools_summary()?;
            }
            self.switch_mode(chunk.kind)?;
        }
        let stdout = &mut self.output;
        if chunk.kind == ChatStreamKind::Reasoning {
            write_full_reasoning_chunk(stdout, &text)?;
        } else if self.plain {
            write!(stdout, "{text}")?;
        } else {
            write!(stdout, "{}", self.markdown.push(&text))?;
        }
        stdout.flush()?;
        Ok(())
    }

    pub fn write_command_output(
        &mut self,
        name: &str,
        stream: CommandOutputStream,
        chunk: &[u8],
    ) -> Result<()> {
        if self.plain || !is_command_tool(name) {
            return Ok(());
        }
        if let Some(display) = &mut self.command_display {
            display.push(stream, chunk);
        }
        Ok(())
    }

    pub fn prepare_for_external_output(&mut self) -> Result<()> {
        self.preparing_question_started_at = None;
        self.tool_preparing = None;
        self.tool_preparing_since = None;
        self.release_transient_output()?;
        self.finalize_tools_summary()?;
        self.show_cursor()?;
        Ok(())
    }

    pub fn write_system_message(&mut self, message: &str) -> Result<()> {
        self.prepare_for_external_output()?;
        let stdout = &mut self.output;
        execute!(stdout, SetForegroundColor(Color::DarkGrey), MoveToColumn(0))?;
        writeln!(stdout, "{message}")?;
        execute!(stdout, ResetColor)?;
        stdout.flush()?;
        Ok(())
    }

    pub fn write_compact_chunk(&mut self, chunk: &ChatStreamChunk) -> Result<()> {
        if chunk.kind != ChatStreamKind::Content {
            return Ok(());
        }
        self.prepare_for_external_output()?;
        let stdout = &mut self.output;
        execute!(stdout, SetForegroundColor(Color::DarkGrey))?;
        write!(stdout, "{}", chunk.text)?;
        execute!(stdout, ResetColor)?;
        stdout.flush()?;
        Ok(())
    }

    pub fn finish_compact(&mut self) -> Result<()> {
        let stdout = &mut self.output;
        execute!(stdout, ResetColor)?;
        writeln!(stdout)?;
        stdout.flush()?;
        Ok(())
    }

    pub fn finish(&mut self) -> Result<()> {
        self.preparing_question_started_at = None;
        self.tool_preparing = None;
        self.tool_preparing_since = None;
        self.stop_waiting()?;
        if let Some(mut display) = self.command_display.take() {
            display.commit(
                &mut self.output,
                self.tool_call_mode == ToolCallDisplayMode::Summary,
            )?;
        }
        self.end_subagent_stream_line()?;
        if self.mode == Some(ChatStreamKind::Content) && !self.plain {
            let stdout = &mut self.output;
            let pending = self.sent_meme_filter.finish();
            if !pending.is_empty() {
                write!(stdout, "{}", self.markdown.push(&pending))?;
            }
            write!(stdout, "{}", self.markdown.flush())?;
            stdout.flush()?;
        }
        if self.mode == Some(ChatStreamKind::Reasoning) {
            execute!(self.output, ResetColor)?;
        }
        if stream_needs_terminating_newline(self.mode, self.reasoning_mode) {
            writeln!(self.output)?;
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

    pub(crate) fn switch_mode(&mut self, mode: ChatStreamKind) -> Result<()> {
        let stdout = &mut self.output;
        match mode {
            // 中转侧工具卡片不改变流式排版模式。
            ChatStreamKind::RemoteToolPreparing
            | ChatStreamKind::RemoteToolStarted
            | ChatStreamKind::RemoteToolFinished => {}
            ChatStreamKind::Reasoning => {
                if self.mode.is_some() {
                    writeln!(stdout)?;
                }
            }
            ChatStreamKind::Content => {
                if self.mode == Some(ChatStreamKind::Reasoning) {
                    execute!(stdout, ResetColor)?;
                    writeln!(stdout)?;
                    writeln!(stdout)?;
                }
            }
            ChatStreamKind::ToolCall => return Ok(()),
            ChatStreamKind::ReasoningPartStart | ChatStreamKind::ReasoningPartEnd => return Ok(()),
            ChatStreamKind::ReasoningReset => return Ok(()),
        }
        stdout.flush()?;
        self.mode = Some(mode);
        Ok(())
    }

    pub(crate) fn end_active_stream_line(&mut self) -> Result<()> {
        if self.reasoning_mode == ReasoningDisplayMode::Summary
            && self.mode == Some(ChatStreamKind::Reasoning)
        {
            self.mode = None;
            return Ok(());
        }
        let was_reasoning = self.mode == Some(ChatStreamKind::Reasoning);
        if was_reasoning {
            execute!(self.output, ResetColor)?;
        } else if self.mode == Some(ChatStreamKind::Content) && !self.plain {
            let stdout = &mut self.output;
            write!(stdout, "{}", self.markdown.flush())?;
            stdout.flush()?;
        }
        if self.mode.is_some() {
            writeln!(self.output)?;
            if was_reasoning {
                writeln!(self.output)?;
            }
            self.mode = None;
        }
        Ok(())
    }

    pub(crate) fn hide_cursor(&mut self) -> Result<()> {
        if self.external_cursor_control {
            return Ok(());
        }
        if !self.cursor_hidden && !self.plain && self.wait_spinner.is_none() {
            execute!(self.output, Hide)?;
            self.cursor_hidden = true;
        }
        Ok(())
    }

    pub(crate) fn show_cursor(&mut self) -> Result<()> {
        if self.external_cursor_control {
            return Ok(());
        }
        if self.cursor_hidden && !self.plain {
            execute!(self.output, Show)?;
            self.cursor_hidden = false;
        }
        Ok(())
    }

    pub(crate) fn release_transient_output(&mut self) -> Result<()> {
        self.stop_waiting()?;
        if let Some(mut display) = self.command_display.take() {
            display.commit(
                &mut self.output,
                self.tool_call_mode == ToolCallDisplayMode::Summary,
            )?;
        }
        self.end_subagent_stream_line()?;
        self.end_active_stream_line()?;
        self.finalize_reasoning_summary()?;
        self.clear_summary_lines()
    }
}

pub(crate) fn stream_needs_terminating_newline(
    mode: Option<ChatStreamKind>,
    reasoning_mode: ReasoningDisplayMode,
) -> bool {
    mode.is_some()
        && !(mode == Some(ChatStreamKind::Reasoning)
            && reasoning_mode == ReasoningDisplayMode::Summary)
}

#[derive(Default)]
pub(crate) struct SentMemeStreamFilter {
    pub(crate) pending: String,
    pub(crate) inside_tag: bool,
}

impl SentMemeStreamFilter {
    pub(crate) fn push(&mut self, text: &str) -> String {
        self.pending.push_str(text);
        let mut output = String::new();
        loop {
            if self.inside_tag {
                if let Some(end) = self.pending.find("</sent_meme>") {
                    let after = end + "</sent_meme>".len();
                    self.pending.drain(..after);
                    self.inside_tag = false;
                    continue;
                }
                self.pending.clear();
                return output;
            }

            let Some(start) = self.pending.find("<sent_meme>") else {
                let keep = longest_sent_meme_prefix_suffix(&self.pending);
                let emit_len = self.pending.len().saturating_sub(keep);
                output.push_str(&self.pending[..emit_len]);
                self.pending.drain(..emit_len);
                return output;
            };

            output.push_str(&self.pending[..start]);
            self.pending.drain(..start + "<sent_meme>".len());
            self.inside_tag = true;
        }
    }

    pub(crate) fn finish(&mut self) -> String {
        if self.inside_tag {
            self.pending.clear();
            self.inside_tag = false;
            return String::new();
        }
        std::mem::take(&mut self.pending)
    }
}

pub(crate) fn longest_sent_meme_prefix_suffix(text: &str) -> usize {
    const TAG: &str = "<sent_meme>";
    let max = TAG.len().saturating_sub(1).min(text.len());
    for len in (1..=max).rev() {
        if text.ends_with(&TAG[..len]) {
            return len;
        }
    }
    0
}

pub(crate) fn truncate_chars(text: &str, max_chars: usize) -> String {
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

pub(crate) fn clip_progress_line(text: &str, max_chars: usize) -> String {
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

pub(crate) fn clip_progress_line_preserving_spaces(text: &str, max_chars: usize) -> String {
    let text = text.trim();
    if text.chars().count() <= max_chars {
        text.to_string()
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
        if let Some(mut display) = self.command_display.take() {
            let _ = display.clear(&mut self.output);
        }
        if self.summary_line_active {
            let _ = self.clear_summary_lines();
            eprintln!();
        }
        let _ = self.show_cursor();
        if !self.plain {
            let _ = execute!(self.output, ResetColor);
        }
    }
}

pub(crate) fn normalize_stream_text(text: &str) -> String {
    text.replace("\r\n", "\n").replace('\r', "\n")
}

/// 流式版终端转义过滤:与 [`sanitize_terminal_text`] 同一状态机,但状态由
/// 调用方跨 delta 持有,转义序列被流切成两半也能整段拦下。
pub(crate) fn sanitize_stream_chunk(state: &mut TerminalControlState, text: &str) -> String {
    let mut output = String::with_capacity(text.len());
    for ch in text.chars() {
        if let Some(ch) = sanitize_terminal_char(state, ch) {
            output.push(ch);
        }
    }
    output
}

pub(crate) fn write_full_reasoning_chunk(writer: &mut impl Write, text: &str) -> Result<()> {
    execute!(writer, SetForegroundColor(Color::Green))?;
    write!(writer, "{text}")?;
    Ok(())
}

pub(crate) fn print_reasoning(reasoning: &str) -> Result<()> {
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
