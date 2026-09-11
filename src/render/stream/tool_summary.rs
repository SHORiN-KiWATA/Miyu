//! 工具活动摘要的渲染。
//!
//! 摘要是**活的**：工具还在跑时逐帧刷新，跑完就固定下来
//! （`finish_subagent_timer`）。并行子代理各占一块，settled 的要冻在原地，不能
//! 因为别人还在跑就跟着重排。

use crate::render::*;

impl StreamRenderer {
    pub fn write_tool_call(&mut self, name: &str, arguments: &str) -> Result<()> {
        // The arguments finished arriving, so the "still receiving" hint has
        // done its job and hands the spinner back to the tool summary.
        self.tool_preparing = None;
        if self.plain {
            return Ok(());
        }
        if name == "ask_question" {
            return self.start_preparing_question();
        }
        self.release_transient_output()?;
        if is_silent_tool(name) {
            return Ok(());
        }
        if is_command_tool(name) {
            let mut display = CommandLiveDisplay::new(
                arguments,
                self.command_output_lines,
                self.tool_call_mode != ToolCallDisplayMode::Hidden,
                self.tool_call_mode == ToolCallDisplayMode::Full,
            );
            if self.live_summary {
                display.tick(&mut self.output)?;
                self.last_tick = None;
            }
            self.command_display = Some(display);
            return Ok(());
        }
        if is_subagent_tool(name) && self.tool_call_mode != ToolCallDisplayMode::Hidden {
            let stats = self.tool_stats_entry(name);
            stats.started_at = Some(std::time::Instant::now());
            stats.elapsed = None;
        }
        if self.tool_call_mode == ToolCallDisplayMode::Full {
            let display_name = self.display_tool_name(name);
            let stdout = &mut self.output;
            writeln!(stdout, "{} {}", t("tool", "工具"), display_name)?;
            write_tool_payload(stdout, t("args", "参数"), arguments)?;
            stdout.flush()?;
        } else if self.tool_call_mode == ToolCallDisplayMode::Summary {
            let stats = self.tool_stats_entry(name);
            stats.calls += 1;
            stats.subject = tool_subject(name, arguments);
            self.ensure_tool_waiting_phase()?;
        }
        Ok(())
    }

    pub fn write_tool_preparing(&mut self, name: &str, batch: bool) -> Result<()> {
        if self.plain {
            return Ok(());
        }
        // 工具自己的提示优先——它说得更具体；没有的话，批量场景退到通用
        // 提示，总比整段空白强。
        let phase = crate::tools::preparing_phase(name)
            .or_else(|| batch.then(crate::tools::batch_preparing_phase));
        let Some(phase) = phase else {
            return Ok(());
        };
        self.release_transient_output()?;
        // Set before the spinner exists: `ensure_waiting_phase` ticks
        // immediately, and that tick re-derives the phase from renderer state.
        // Without the sticky field the tool summary or the reasoning timer wins
        // there and the hint never reaches the screen.
        let since = *self
            .tool_preparing_since
            .get_or_insert_with(std::time::Instant::now);
        self.tool_preparing = Some((phase, since));
        // Braille + the dim tool palette: this is a tool starting up, not the
        // model thinking, and the scanner/green pair reads as the latter.
        self.ensure_waiting_phase(self.waiting_phase_text(), SpinnerStyle::Braille)
    }

    pub fn write_tool_result(&mut self, name: &str, ok: bool, output: &str) -> Result<()> {
        if self.plain {
            return Ok(());
        }
        if is_silent_tool(name) && ok {
            return Ok(());
        }
        self.stop_waiting()?;
        self.tool_preparing_since = None;
        self.reanchor_wait_timer();
        self.end_subagent_stream_line()?;
        let status = if ok { "ok" } else { "err" };
        let elapsed = self.finish_subagent_timer(name);
        if is_command_tool(name) {
            if let Some(mut display) = self.command_display.take() {
                display.set_result(ok);
                let include_output = self.tool_call_mode == ToolCallDisplayMode::Summary
                    || (self.tool_call_mode == ToolCallDisplayMode::Full && !ok);
                if self.live_summary {
                    display.commit(&mut self.output, include_output)?;
                } else {
                    display.write_static(&mut self.output, include_output)?;
                }
                self.last_tick = None;
            }
            if self.tool_call_mode == ToolCallDisplayMode::Full {
                let stdout = &mut self.output;
                write_command_result_blocks(stdout, output)?;
                stdout.flush()?;
            }
            return Ok(());
        }
        if name == "todowrite" && ok {
            self.release_transient_output()?;
            let stdout = &mut self.output;
            // 旧 JSON 输出(历史回放)在这里画表;新文本输出的表格已由
            // __todo_table__ progress 画过,这里只收掉状态行不再重复展示。
            let rendered = write_todo_table(stdout, output)?;
            if rendered {
                stdout.flush()?;
            }
            if rendered || output.trim_start().starts_with("todo list ") {
                if self.tool_call_mode == ToolCallDisplayMode::Summary {
                    let stats = self.tool_stats_entry(name);
                    stats.ok += 1;
                    stats.progress = None;
                    self.tool_stats.clear();
                    self.last_tool_summary.clear();
                }
                return Ok(());
            }
        }
        if self.tool_call_mode == ToolCallDisplayMode::Full {
            self.release_transient_output()?;
            let display_name = self.display_tool_name(name);
            let stdout = &mut self.output;
            writeln!(
                stdout,
                "{} {} {}",
                t("result", "结果"),
                display_name,
                tool_result_status(status, elapsed)
            )?;
            write_tool_payload(stdout, t("output", "输出"), output)?;
            stdout.flush()?;
            self.tool_stats.remove(name);
        } else if self.tool_call_mode == ToolCallDisplayMode::Summary {
            let stats = self.tool_stats_entry(name);
            if ok {
                stats.ok += 1;
            } else {
                stats.error += 1;
            }
            stats.progress = None;
            if self.tool_stats.values().any(|stats| !stats.settled()) {
                // Siblings still running (parallel subagents): freeze this
                // tool's block in the live area; commit only when the whole
                // batch settles.
                self.update_tool_summary_display()?;
            } else {
                self.finalize_tools_summary()?;
            }
        }
        Ok(())
    }

    pub fn write_tool_progress(&mut self, name: &str, message: &str) -> Result<()> {
        if let Some(phase) = message.strip_prefix("__tool_phase__") {
            if self.plain {
                let stdout = &mut self.output;
                writeln!(stdout, "{phase}")?;
                stdout.flush()?;
            } else if self.wait_spinner.is_some() {
                self.set_waiting_phase(phase.to_string());
                self.tick_spinner()?;
            } else {
                self.render_summary_line(phase, SummaryStyle::Tool)?;
            }
            return Ok(());
        }
        if let Some(json) = message.strip_prefix("__patch_preview__") {
            self.release_transient_output()?;
            let stdout = &mut self.output;
            if write_patch_result(stdout, json)? {
                stdout.flush()?;
            }
            return Ok(());
        }
        // 08-21 token-diet:todowrite 不再向模型回显整表,表格数据改由
        // progress 侧信道送达,载荷形状与旧工具输出一致。
        if let Some(json) = message.strip_prefix("__todo_table__") {
            self.release_transient_output()?;
            let stdout = &mut self.output;
            if write_todo_table(stdout, json)? {
                stdout.flush()?;
            }
            return Ok(());
        }
        if self.plain {
            return Ok(());
        }
        if message == "__external_output__" {
            self.prepare_for_external_output()?;
            return Ok(());
        }
        if let Some(text) = message.strip_prefix("__subagent_detach__") {
            if self.tool_call_mode == ToolCallDisplayMode::Full {
                self.release_transient_output()?;
                let display_name = self.display_tool_name(name);
                let stdout = &mut self.output;
                writeln!(stdout, "{} {}: {text}", t("progress", "进度"), display_name)?;
                stdout.flush()?;
            } else if self.tool_call_mode == ToolCallDisplayMode::Summary {
                // Lands as the block's `↳` subject line, not the final `✓`
                // stats line — detach is a fact about the call, not a result.
                let stats = self.tool_stats_entry(name);
                stats.subject = Some(text.to_string());
                stats.detached = true;
                self.update_tool_summary_display()?;
            }
            return Ok(());
        }
        if let Some(text) = message.strip_prefix(crate::tools::TOOL_SUMMARY_PREFIX) {
            if self.tool_call_mode == ToolCallDisplayMode::Full {
                self.release_transient_output()?;
                let stdout = &mut self.output;
                for line in text.lines() {
                    writeln!(stdout, "{line}")?;
                }
                stdout.flush()?;
            } else if self.tool_call_mode == ToolCallDisplayMode::Summary {
                self.tool_stats_entry(name).final_progress = Some(text.to_string());
                self.update_tool_summary_display()?;
            }
            return Ok(());
        }
        if let Some(text) = message.strip_prefix("__subagent_stats__") {
            if self.tool_call_mode == ToolCallDisplayMode::Full {
                self.release_transient_output()?;
                let display_name = self.display_tool_name(name);
                let stdout = &mut self.output;
                writeln!(stdout, "{} {}: {text}", t("progress", "进度"), display_name)?;
                stdout.flush()?;
            } else if self.tool_call_mode == ToolCallDisplayMode::Summary {
                self.tool_stats_entry(name).final_progress = Some(text.to_string());
                self.update_tool_summary_display()?;
            }
            return Ok(());
        }
        if let Some(text) = message.strip_prefix("__subagent_reasoning__") {
            let text = normalize_stream_text(text);
            if self.tool_call_mode == ToolCallDisplayMode::Full {
                if self.subagent_mode != Some(ChatStreamKind::Reasoning) {
                    self.stop_waiting()?;
                    self.clear_summary_lines()?;
                    self.end_active_stream_line()?;
                    let stdout = &mut self.output;
                    writeln!(stdout)?;
                    stdout.flush()?;
                }
                let stdout = &mut self.output;
                write_full_reasoning_chunk(stdout, &text)?;
                stdout.flush()?;
                self.subagent_mode = Some(ChatStreamKind::Reasoning);
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
                    self.release_transient_output()?;
                    let display_name = self.display_tool_name(tool_name);
                    let stdout = &mut self.output;
                    if tool_name == "run_command" {
                        write_command_block(stdout, args)?;
                    } else {
                        writeln!(stdout, "{} {}", t("tool", "工具"), display_name)?;
                        write_tool_payload(stdout, t("args", "参数"), args)?;
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
                    let args = value.get("args").and_then(Value::as_str).unwrap_or("");
                    let output = value.get("output").and_then(Value::as_str).unwrap_or("");
                    let status = if ok { "ok" } else { "err" };
                    self.release_transient_output()?;
                    let display_name = self.display_tool_name(tool_name);
                    let stdout = &mut self.output;
                    if tool_name == "run_command" {
                        write_command_block_with_status(
                            stdout,
                            args,
                            if ok {
                                CommandStatus::Ok
                            } else {
                                CommandStatus::Error
                            },
                        )?;
                        write_command_result_blocks(stdout, output)?;
                        write_command_block_gap(stdout, true)?;
                    } else {
                        writeln!(stdout, "{} {} {status}", t("result", "结果"), display_name)?;
                        write_tool_payload(stdout, t("output", "输出"), output)?;
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
            self.release_transient_output()?;
            let display_name = self.display_tool_name(name);
            let stdout = &mut self.output;
            writeln!(
                stdout,
                "{} {}: {message}",
                t("progress", "进度"),
                display_name
            )?;
            stdout.flush()?;
        } else if self.tool_call_mode == ToolCallDisplayMode::Summary {
            self.tool_stats_entry(name).progress = Some(message.to_string());
            self.update_tool_summary_display()?;
        }
        Ok(())
    }

    pub(crate) fn update_tool_summary_display(&mut self) -> Result<()> {
        self.end_subagent_stream_line()?;
        if self.wait_spinner.is_some() {
            let (header, sub) = self.tool_summary_live();
            self.set_tool_waiting_phase(&header, sub.as_deref());
        } else {
            self.end_active_stream_line()?;
            self.finalize_reasoning_summary()?;
            self.ensure_tool_waiting_phase()?;
        }
        Ok(())
    }

    pub(crate) fn end_subagent_stream_line(&mut self) -> Result<()> {
        let was_reasoning = self.subagent_mode == Some(ChatStreamKind::Reasoning);
        if was_reasoning {
            execute!(self.output, ResetColor)?;
        }
        if self.subagent_mode.is_some() {
            writeln!(self.output)?;
            if was_reasoning {
                writeln!(self.output)?;
            }
            self.subagent_mode = None;
        }
        Ok(())
    }

    pub(crate) fn finalize_tools_summary(&mut self) -> Result<()> {
        if self.tool_call_mode == ToolCallDisplayMode::Summary && !self.tool_stats.is_empty() {
            self.stop_waiting()?;
            execute!(self.output, ResetColor)?;
            let summary = self.tool_summary_text();
            if self.summary_line_active {
                self.clear_summary_lines()?;
                self.summary_line_active = false;
                self.summary_lines_active = 0;
            }
            let stdout = &mut self.output;
            write_activity_summary(stdout, &summary, SummaryStyle::Tool)?;
            stdout.flush()?;
            self.tool_stats.clear();
            self.last_tool_summary.clear();
        }
        Ok(())
    }

    pub(crate) fn render_summary_line(&mut self, text: &str, style: SummaryStyle) -> Result<()> {
        self.stop_waiting()?;
        if !self.live_summary {
            return Ok(());
        }
        self.clear_summary_lines()?;
        let stdout = &mut self.output;
        let lines = transient_summary_lines(text, command_terminal_width());
        for (index, line) in lines.iter().enumerate() {
            if index > 0 {
                writeln!(stdout)?;
            }
            execute!(stdout, MoveToColumn(0))?;
            write!(stdout, "{}\x1b[K", style_summary_text(line, style))?;
        }
        stdout.flush()?;
        self.summary_line_active = true;
        self.summary_lines_active = lines.len().max(1) as u16;
        Ok(())
    }

    pub(crate) fn clear_summary_lines(&mut self) -> Result<()> {
        if !self.summary_line_active {
            return Ok(());
        }
        let stdout = &mut self.output;
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

    /// Gets or creates a tool's stats entry, stamping first-seen order so
    /// parallel blocks render in launch order rather than name order.
    pub(crate) fn tool_stats_entry(&mut self, name: &str) -> &mut ToolStats {
        self.tool_seq += 1;
        let seq = self.tool_seq;
        self.tool_stats
            .entry(name.to_string())
            .or_insert_with(|| ToolStats {
                seq,
                ..ToolStats::default()
            })
    }

    /// Tools in first-seen order (stable for direct test inserts with seq 0).
    pub(crate) fn ordered_tool_stats(&self) -> Vec<(&String, &ToolStats)> {
        let mut entries: Vec<_> = self.tool_stats.iter().collect();
        entries.sort_by_key(|(_, stats)| stats.seq);
        entries
    }

    pub(crate) fn tool_summary_text(&self) -> String {
        self.ordered_tool_stats()
            .into_iter()
            .map(|(name, stats)| self.tool_block_lines(name, stats, false).join("\n"))
            .collect::<Vec<_>>()
            .join("\n\n")
    }

    /// Builds one tool's display block: a `~`-prefixed status header plus
    /// its own subject/progress lines. In `live` mode a still-running tool's
    /// header carries [`wait_spinner::BLOCK_MARKER`] so the spinner animates
    /// it, and a settled tool freezes into its final `✓` stats in place. The
    /// committed variant (`live == false`) prefers `final_progress` with `✓`.
    pub(crate) fn tool_block_lines(
        &self,
        name: &str,
        stats: &ToolStats,
        live: bool,
    ) -> Vec<String> {
        let display = self.display_tool_name(name);
        let mut header = tool_status_text(&display, stats, is_subagent_tool(name));
        if inline_tool_subject(name) {
            if let Some(subject) = &stats.subject {
                header.push_str(" · ");
                header.push_str(subject);
            }
        }
        let header = self.tool_summary_with_prefix(header);
        // In live mode a running block's detail lines are indented to sit
        // under its spinner glyph; a settled block drops the glyph and sits
        // flush, matching the committed layout.
        let running_live = live && !stats.settled();
        // Detail lines always sit two columns in, matching command blocks
        // (`$ …` / `  ↳ cmd` / `  │ output`) and avoiding the leftward jump
        // a block used to make when it settled.
        let detail_indent = "  ";
        let mut lines = Vec::new();
        if running_live {
            lines.push(format!("{}{header}", wait_spinner::BLOCK_MARKER));
        } else {
            lines.push(header);
        }
        if !inline_tool_subject(name) {
            if let Some(subject) = &stats.subject {
                // Subagent headers already carry the description — don't
                // repeat it as a subject line.
                if !lines[0].contains(subject.as_str()) {
                    lines.push(format!("{detail_indent}↳ {subject}"));
                }
            }
        }
        let (progress_text, is_final) = if live {
            if stats.settled() {
                (stats.final_progress.as_ref(), true)
            } else {
                (stats.progress.as_ref(), false)
            }
        } else if stats.final_progress.is_some() {
            (stats.final_progress.as_ref(), true)
        } else {
            (stats.progress.as_ref(), false)
        };
        let progress_prefix = if is_final { "✓" } else { "↳" };
        if let Some(message) = progress_text {
            for line in message.lines().filter(|line| !line.trim().is_empty()) {
                let line = if is_final {
                    clip_progress_line_preserving_spaces(line, 120)
                } else {
                    clip_progress_line(line, 120)
                };
                // 自带记号的行原样保留:失败清单是 `✗ …`,再套一个 `✓` 就成了
                // 「✓ ✗ 权限不足」。
                if line.starts_with('✗') {
                    lines.push(format!("{detail_indent}{line}"));
                } else {
                    lines.push(format!("{detail_indent}{progress_prefix} {line}"));
                }
            }
        }
        lines
    }

    pub(crate) fn tool_summary_header(&self) -> String {
        let parts = self
            .ordered_tool_stats()
            .into_iter()
            .map(|(name, stats)| {
                let display = self.display_tool_name(name);
                let mut header = tool_status_text(&display, stats, is_subagent_tool(name));
                if inline_tool_subject(name) {
                    if let Some(subject) = &stats.subject {
                        header.push_str(" · ");
                        header.push_str(subject);
                    }
                }
                header
            })
            .collect::<Vec<_>>()
            .join(", ");
        self.tool_summary_with_prefix(parts)
    }

    /// Live status for the wait spinner. A single tool keeps the classic
    /// one-line phase + progress sub-block. Multiple tools (e.g. parallel
    /// subagents) switch the spinner into block mode: the phase line is
    /// empty and every tool renders as its own block — running blocks carry
    /// their own animated glyph, settled blocks freeze into their final
    /// stats, and blocks are separated by blank lines:
    ///
    /// ```text
    /// ⠋ ~ 子代理·任务A×1 运行中 · 3s
    ///   ↳ 任务A进度
    ///
    ///   ~ 子代理·任务B×1 ok · 2s
    ///   ✓ 工具调用 1 次
    /// ```
    pub(crate) fn tool_summary_live(&self) -> (String, Option<String>) {
        if self.tool_stats.len() <= 1 {
            return (self.tool_summary_header(), self.tool_summary_progress());
        }
        let blocks = self
            .ordered_tool_stats()
            .into_iter()
            .map(|(name, stats)| self.tool_block_lines(name, stats, true).join("\n"))
            .collect::<Vec<_>>()
            .join("\n\n");
        (String::new(), Some(blocks))
    }

    pub(crate) fn tool_summary_with_prefix(&self, parts: String) -> String {
        if self.tool_stats.len() == 1
            && self
                .tool_stats
                .keys()
                .next()
                .is_some_and(|name| name == "run_command")
        {
            format!("$ {parts}")
        } else {
            format!("~ {parts}")
        }
    }

    pub(crate) fn tool_summary_progress(&self) -> Option<String> {
        for (name, stats) in self.ordered_tool_stats() {
            let mut lines = Vec::new();
            if !inline_tool_subject(name) {
                if let Some(subject) = &stats.subject {
                    // Skip subjects already shown in the header (subagent
                    // descriptions are part of the display name).
                    if !self.display_tool_name(name).contains(subject.as_str()) {
                        lines.push(format!("  ↳ {subject}"));
                    }
                }
            }
            if let Some(message) = &stats.progress {
                let progress = message
                    .lines()
                    .filter(|line| !line.trim().is_empty())
                    .map(|line| format!("  ↳ {}", clip_progress_line(line, 120)))
                    .collect::<Vec<_>>()
                    .join("\n");
                if !progress.is_empty() {
                    lines.push(progress);
                }
            }
            if !lines.is_empty() {
                return Some(lines.join("\n"));
            }
        }
        None
    }

    pub(crate) fn has_running_subagent_timer(&self) -> bool {
        self.tool_stats
            .iter()
            .any(|(name, stats)| is_subagent_tool(name) && stats.started_at.is_some())
    }

    pub(crate) fn finish_subagent_timer(&mut self, name: &str) -> Option<std::time::Duration> {
        if !is_subagent_tool(name) {
            return None;
        }
        let stats = self.tool_stats.get_mut(name)?;
        let elapsed = stats.started_at.take()?.elapsed();
        stats.elapsed = Some(elapsed);
        Some(elapsed)
    }

    pub(crate) fn display_tool_name<'a>(&self, name: &'a str) -> String {
        // Subagents keep their per-call description so parallel subagent
        // calls show as separate lines: "子代理·<描述>".
        // "task:" 是改名前的事件名,历史回放里还在。
        if let Some(description) = name
            .strip_prefix("subagent:")
            .or_else(|| name.strip_prefix("task:"))
        {
            let base = if self.readable_tool_names {
                readable_tool_name("subagent")
            } else {
                "subagent".to_string()
            };
            return format!("{base}·{description}");
        }
        let name = tool_event_base_name(name);
        if self.readable_tool_names {
            readable_tool_name(name)
        } else {
            name.to_string()
        }
    }
}
