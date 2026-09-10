//! 推理阶段的计时与摘要。
//!
//! 「等待」有好几种，显示的字不一样：刚发出请求、模型在推理、工具在流参数
//! （`start_preparing_question` / `set_tool_waiting_phase`）。用户看的是这一行，
//! 用错阶段会让人以为卡住了。
//!
//! 计时要能**冻结**（`freeze_reasoning_elapsed_at`）：一段推理结束后显示的是它
//! 实际用的时间，不能继续跟着走。

use crate::render::*;

impl StreamRenderer {
    pub fn start_waiting(&mut self) -> Result<()> {
        if self.plain
            || self.wait_spinner.is_some()
            || self.command_display.is_some()
            || !WaitSpinner::supported()
        {
            return Ok(());
        }
        self.hide_cursor()?;
        let phase = self.waiting_phase_text();
        self.wait_spinner = Some(WaitSpinner::start(phase, SpinnerStyle::Scanner));
        self.last_tick = None;
        self.tick_spinner()?;
        Ok(())
    }

    pub fn start_reasoning_phase(&mut self, received_at: std::time::Instant) -> Result<()> {
        self.preparing_question_started_at = None;
        self.tool_preparing = None;
        self.tool_preparing_since = None;
        if self.reasoning_mode == ReasoningDisplayMode::Summary {
            self.reasoning_started_at = Some(received_at);
            self.reasoning_elapsed = None;
            self.reasoning_title = None;
            self.reasoning_text.clear();
            self.reasoning_tokens = 0;
        }
        self.start_waiting()?;
        if self.wait_spinner.is_some() {
            self.set_waiting_phase(self.waiting_phase_text());
            self.last_tick = None;
            self.tick_spinner()?;
        }
        Ok(())
    }

    pub(crate) fn waiting_phase_text(&self) -> String {
        if let Some(started_at) = self.preparing_question_started_at {
            return format!(
                "{} · {}",
                t("~ Preparing question", "~ 准备问题"),
                format_reasoning_elapsed(started_at.elapsed())
            );
        }
        if let Some((phase, started_at)) = self.tool_preparing {
            return format!(
                "~ {phase} · {}",
                format_reasoning_elapsed(started_at.elapsed())
            );
        }
        match self.reasoning_mode {
            ReasoningDisplayMode::Summary => {
                if self.reasoning_title.is_some() || !self.reasoning_text.is_empty() {
                    self.reasoning_live_text()
                } else {
                    self.reasoning_elapsed_text()
                }
            }
            ReasoningDisplayMode::Full => String::new(),
            ReasoningDisplayMode::Hidden => t("thinking", "思考").to_string(),
        }
    }

    pub fn write_reasoning_title(&mut self, title: &str) -> Result<()> {
        if self.reasoning_mode != ReasoningDisplayMode::Summary || self.plain {
            return Ok(());
        }
        let title = redact_sensitive_inline(&sanitize_terminal_text(title));
        let title = clip_progress_line(&title, 80);
        if title.is_empty() {
            return Ok(());
        }
        self.reasoning_title = Some(title);
        self.ensure_waiting_phase(self.reasoning_live_text(), SpinnerStyle::Scanner)
    }

    pub fn start_reasoning_part(&mut self, received_at: std::time::Instant) -> Result<()> {
        if self.reasoning_mode != ReasoningDisplayMode::Summary {
            return Ok(());
        }
        let has_pending_summary = self.reasoning_title.is_some() || !self.reasoning_text.is_empty();
        let mid_content = self.mode == Some(ChatStreamKind::Content);
        // 正文行还开着、又没有待写的摘要时不截断正文:分段开始本身不落
        // 一个字,截了只会把一段话拆成一行一段(09-10 截图:每个 delta 单独
        // 成段,起因是供应商在正文 delta 上附带空推理字段)。真有推理文本
        // 到来时 write_chunk 会先收行再起转轮;摘要要写时这里照旧断行。
        if has_pending_summary || !mid_content {
            self.end_active_stream_line()?;
        }
        if has_pending_summary {
            self.freeze_reasoning_elapsed_at(received_at);
            self.finalize_reasoning_summary()?;
            self.reasoning_started_at = Some(received_at);
        } else if self.reasoning_started_at.is_none() {
            self.reasoning_started_at = Some(received_at);
        }
        self.reasoning_elapsed = None;
        self.reasoning_title = None;
        self.reasoning_text.clear();
        self.reasoning_tokens = 0;
        if mid_content && !has_pending_summary {
            // 转轮会 MoveToColumn(0)+清行,正文行开着时起它会抹掉半行字。
            return Ok(());
        }
        self.start_waiting()
    }

    pub fn finish_reasoning_part(&mut self, received_at: std::time::Instant) -> Result<()> {
        if self.reasoning_mode != ReasoningDisplayMode::Summary {
            return Ok(());
        }
        if self.reasoning_title.is_some() || !self.reasoning_text.is_empty() {
            self.freeze_reasoning_elapsed_at(received_at);
            self.finalize_reasoning_summary()?;
            self.reasoning_started_at = Some(received_at);
            self.reasoning_elapsed = None;
        }
        Ok(())
    }

    pub fn reset_reasoning_phase(&mut self, received_at: std::time::Instant) -> Result<()> {
        if self.reasoning_mode != ReasoningDisplayMode::Summary {
            return Ok(());
        }
        self.stop_waiting()?;
        if self.summary_line_active {
            self.clear_summary_lines()?;
        }
        self.reasoning_title = None;
        self.reasoning_text.clear();
        self.reasoning_tokens = 0;
        self.reasoning_started_at = Some(received_at);
        self.reasoning_elapsed = None;
        self.mode = None;
        self.start_waiting()
    }

    pub fn tick_spinner(&mut self) -> Result<()> {
        let now = std::time::Instant::now();
        let should_tick = self
            .last_tick
            .map(|last| now.duration_since(last) >= SPINNER_INTERVAL)
            .unwrap_or(true);
        if should_tick {
            let subagent_timer_active = self.has_running_subagent_timer();
            // Both sticky hints win over the tool/reasoning summaries below:
            // they describe what the turn is blocked on right now, and the
            // summaries would otherwise overwrite them on the very first tick
            // after they are set — before the spinner has drawn once.
            if (self.preparing_question_started_at.is_some() || self.tool_preparing.is_some())
                && self.wait_spinner.is_some()
            {
                self.set_waiting_phase(self.waiting_phase_text());
            } else if self.tool_call_mode == ToolCallDisplayMode::Summary
                && !self.tool_stats.is_empty()
                && self.wait_spinner.is_some()
            {
                let (header, sub) = self.tool_summary_live();
                self.set_tool_waiting_phase(&header, sub.as_deref());
            } else if self.reasoning_mode == ReasoningDisplayMode::Summary
                && self.reasoning_started_at.is_some()
                && self.wait_spinner.is_some()
            {
                self.set_waiting_phase(self.waiting_phase_text());
            }
            if let Some(display) = &mut self.command_display {
                debug_assert!(self.wait_spinner.is_none());
                display.tick(&mut self.output)?;
            } else if let Some(spinner) = &mut self.wait_spinner {
                spinner.tick(&mut self.output)?;
            }
            if self.wait_spinner.is_some()
                || self.command_display.is_some()
                || subagent_timer_active
            {
                self.last_tick = Some(now);
            }
        }
        Ok(())
    }

    pub(crate) fn finalize_reasoning_summary(&mut self) -> Result<()> {
        if self.reasoning_mode == ReasoningDisplayMode::Summary
            && (self.reasoning_title.is_some() || !self.reasoning_text.is_empty())
        {
            self.stop_waiting()?;
            let summary = self.reasoning_summary_text();
            if self.summary_line_active {
                self.clear_summary_lines()?;
                self.summary_line_active = false;
                self.summary_lines_active = 0;
            }
            let stdout = &mut self.output;
            write_activity_summary(stdout, &summary, SummaryStyle::Reasoning)?;
            stdout.flush()?;
            self.reasoning_text.clear();
            self.reasoning_tokens = 0;
            self.reasoning_title = None;
            self.reasoning_started_at = None;
            self.reasoning_elapsed = None;
            self.mode = None;
        }
        Ok(())
    }

    pub(crate) fn reasoning_summary_text(&self) -> String {
        let elapsed = self.reasoning_elapsed_text();
        format!("{} · {elapsed}", self.reasoning_live_metrics_text())
    }

    pub(crate) fn reasoning_live_text(&self) -> String {
        if self.reasoning_started_at.is_none() {
            return match &self.reasoning_title {
                Some(title) if crate::i18n::is_zh() => {
                    format!("{}：{title}", t("thinking", "思考"))
                }
                Some(title) => format!("{}: {title}", t("thinking", "思考")),
                None => t("thinking", "思考").to_string(),
            };
        }
        let elapsed = self.reasoning_elapsed_text();
        format!("{} · {elapsed}", self.reasoning_live_metrics_text())
    }

    pub(crate) fn reasoning_elapsed_text(&self) -> String {
        self.reasoning_elapsed
            .or_else(|| self.reasoning_started_at.map(|started| started.elapsed()))
            .map(format_reasoning_elapsed)
            .unwrap_or_else(|| "0ms".to_string())
    }

    /// 工具跑完就把等待计时器重新锚定到此刻。
    ///
    /// 计时锚点是 `ReasoningStart`,而它**每次 LLM 请求发一次**。本地工具每轮
    /// 一问一答,锚点自然跟着重置;claude-code 的工具在它自己进程里闭环执行,
    /// 用 RemoteToolStarted/Finished 在**同一条流**里报告,整轮只发一次请求
    /// ——锚点只设一次,秒数从回合开头一路累加到结尾(08-26 用户点名)。
    ///
    /// 有待落地的思考摘要时不动:那条摘要要显示的是它自己那段的耗时,重锚会
    /// 把它算短。
    pub(crate) fn reanchor_wait_timer(&mut self) {
        if self.reasoning_started_at.is_none() {
            return;
        }
        if self.reasoning_title.is_some() || !self.reasoning_text.is_empty() {
            return;
        }
        self.reasoning_started_at = Some(std::time::Instant::now());
        self.reasoning_elapsed = None;
    }

    pub(crate) fn freeze_reasoning_elapsed_at(&mut self, received_at: std::time::Instant) {
        self.reasoning_elapsed = self
            .reasoning_started_at
            .map(|started_at| received_at.saturating_duration_since(started_at));
    }

    pub(crate) fn reasoning_live_metrics_text(&self) -> String {
        let phase = match &self.reasoning_title {
            Some(title) if crate::i18n::is_zh() => {
                format!("{}：{title}", t("thinking", "思考"))
            }
            Some(title) => format!("{}: {title}", t("thinking", "思考")),
            None => t("thinking", "思考").to_string(),
        };
        if self.reasoning_tokens == 0 {
            return phase;
        }
        format!(
            "{phase} · {} {}",
            self.reasoning_tokens,
            t("tokens", "词元")
        )
    }

    pub(crate) fn record_reasoning_text(&mut self, text: &str) {
        self.reasoning_started_at
            .get_or_insert_with(std::time::Instant::now);
        self.reasoning_text.push_str(text);
        // Incremental: recounting the whole accumulated text on every chunk is
        // O(n²) over the stream and the value only feeds the spinner label.
        // Per-chunk sums drift <1% from a full recount (BPE merges across
        // chunk boundaries) — fine for a display estimate.
        self.reasoning_tokens += crate::token_estimate::estimate_tokens(text);
    }

    pub(crate) fn set_waiting_phase(&mut self, phase: String) {
        if let Some(spinner) = &mut self.wait_spinner {
            spinner.set_phase(phase);
        }
    }

    pub(crate) fn ensure_waiting_phase(
        &mut self,
        phase: String,
        style: SpinnerStyle,
    ) -> Result<()> {
        if self.command_display.is_some() {
            return Ok(());
        }
        if self.plain || !WaitSpinner::supported() {
            if self.summary_line_active {
                self.clear_summary_lines()?;
            }
            self.render_summary_line(&phase, summary_style_for(style))?;
            return Ok(());
        }
        if self.wait_spinner.is_none() {
            self.wait_spinner = Some(WaitSpinner::start(phase, style));
            self.last_tick = None;
            self.tick_spinner()?;
        } else {
            self.set_waiting_phase(phase);
        }
        Ok(())
    }

    pub(crate) fn ensure_tool_waiting_phase(&mut self) -> Result<()> {
        debug_assert!(self.command_display.is_none());
        let (header, sub) = self.tool_summary_live();
        if self.plain || !self.live_summary {
            let summary = match &sub {
                Some(s) if header.is_empty() => s.clone(),
                Some(s) => format!("{header}\n{s}"),
                None => header,
            };
            let summary = summary.replace(wait_spinner::BLOCK_MARKER, "");
            if self.summary_line_active {
                self.clear_summary_lines()?;
            }
            self.last_tool_summary = summary.clone();
            return self.render_summary_line(&summary, SummaryStyle::Tool);
        }
        if self.summary_line_active {
            self.clear_summary_lines()?;
        }
        if self.wait_spinner.is_none() {
            self.hide_cursor()?;
            self.wait_spinner = Some(WaitSpinner::start(header, SpinnerStyle::Braille));
            self.last_tick = None;
        } else {
            self.set_waiting_phase(header);
        }
        if let Some(spinner) = &mut self.wait_spinner {
            spinner.set_sub_phase(sub);
        }
        self.tick_spinner()
    }

    pub(crate) fn start_preparing_question(&mut self) -> Result<()> {
        if self.plain || self.preparing_question_started_at.is_some() {
            return Ok(());
        }
        self.release_transient_output()?;
        self.preparing_question_started_at = Some(std::time::Instant::now());
        if !WaitSpinner::supported() {
            return Ok(());
        }
        self.hide_cursor()?;
        self.wait_spinner = Some(WaitSpinner::start(
            self.waiting_phase_text(),
            SpinnerStyle::Braille,
        ));
        self.last_tick = None;
        self.tick_spinner()
    }

    pub(crate) fn set_tool_waiting_phase(&mut self, header: &str, sub: Option<&str>) {
        if let Some(spinner) = &mut self.wait_spinner {
            spinner.set_phase(header.to_string());
            spinner.set_sub_phase(sub.map(|s| s.to_string()));
        }
    }

    pub(crate) fn stop_waiting(&mut self) -> Result<()> {
        if let Some(mut spinner) = self.wait_spinner.take() {
            spinner.stop(&mut self.output)?;
        }
        self.last_tick = None;
        Ok(())
    }
}
