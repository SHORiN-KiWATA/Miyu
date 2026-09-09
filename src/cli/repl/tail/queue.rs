//! 活动区里的排队消息与流式片段。
//!
//! 用户在回合跑着时敲的内容先进队列，显示在活动区下方。片段（chunk）攒着批量
//! 刷（`flush_pending_chunks`）——每来一个 token 就重绘一次，终端跟不上。

use crate::cli::repl::tail::*;

impl LiveReplTail {
    pub(in crate::cli) fn enqueue(&mut self, prompt: QueuedPrompt) -> Result<()> {
        let output_cursor = self.output_cursor;
        self.suspend()?;
        self.append_queued(prompt);
        self.resume_at(output_cursor)
    }

    pub(in crate::cli) fn append_queued(&mut self, prompt: QueuedPrompt) {
        self.queued.push(prompt);
        self.queued.sort_by_key(|prompt| prompt.seq);
    }

    pub(in crate::cli) fn queue_stream_chunk(&mut self, chunk: ChatStreamChunk) {
        if let Some(pending) = self
            .pending_chunks
            .last_mut()
            .filter(|pending| pending.kind == chunk.kind)
        {
            pending.text.push_str(&chunk.text);
        } else {
            self.pending_chunks.push(chunk);
        }
    }

    pub(in crate::cli) fn flush_pending_chunks(
        &mut self,
        renderer: &mut render::StreamRenderer,
    ) -> Result<()> {
        for chunk in std::mem::take(&mut self.pending_chunks) {
            renderer.write_chunk(chunk)?;
        }
        Ok(())
    }

    pub(in crate::cli) fn discard_pending_chunks(&mut self) {
        self.pending_chunks.clear();
    }

    /// 提交回显与活动区重画放在同一个同步块里。
    ///
    /// 回显把屏幕顶满时,光标会被推到最后一行左端。以前这里只写回显,输入
    /// 框要等 daemon 接受回合之后才画回去,隐藏着的光标在左下角停一百多
    /// 毫秒——kitty 的 cursor_trail 不看光标隐不隐藏,照样把拖尾画到左下角
    /// 再飞回输入框(09-09 无头 kitty 逐帧截图坐实,pyte 回放看不见是因为
    /// 光标本体确实隐藏着)。现在回显写完立刻把活动区画回去,光标从不在
    /// 左下角落脚;写完后的输出光标按帧追踪器推算,同步块内零 ESC[6n。
    pub(in crate::cli) fn commit_submission(&mut self, submission: &LiveSubmission) -> Result<()> {
        self.suspend()?;
        let (cols, rows) = terminal::size().unwrap_or((80, 24));
        // suspend 已把光标 MoveTo(output_cursor):列已知。
        let frame = committed_user_messages_frame(
            &[(submission.display_content.as_str(), self.editor.mode)],
            true,
            self.output_cursor.0,
            usize::from(cols),
        );
        let mut stdout = io::stdout();
        write!(stdout, "{frame}")?;
        stdout.flush()?;
        let output_cursor = cursor_after_frame(frame.as_bytes(), self.output_cursor, cols, rows);
        self.output_cursor = output_cursor;
        self.resume_at(output_cursor)
    }

    pub(in crate::cli) fn commit_empty_submission(&mut self) -> Result<()> {
        let mode = self.editor.mode;
        self.editor.clear();
        self.suspend()?;
        write_committed_user_messages(&[("", mode)], true)?;
        let output_cursor = cursor_position_or(self.output_cursor);
        self.output_cursor = output_cursor;
        self.resume_at(output_cursor)
    }

    /// Print a background-command wake reply into the scrollback while the
    /// REPL idles: dim header, then the assistant's report.
    pub(in crate::cli) fn show_background_report(
        &mut self,
        report: &BackgroundReport,
    ) -> Result<()> {
        self.suspend()?;
        let mut stdout = io::stdout();
        queue!(
            stdout,
            Print(format!(
                "\x1b[2m⚙ {}\x1b[0m\r\n\r\n",
                job_wake_headline(&report.headline)
            ))
        )?;
        for line in report.reply.lines() {
            queue!(
                stdout,
                Print(format!("{}\r\n", render::render_markdown_line(line)))
            )?;
        }
        queue!(stdout, Print("\r\n"))?;
        stdout.flush()?;
        self.output_cursor = cursor_position_or(self.output_cursor);
        let output_cursor = self.output_cursor;
        self.resume_at(output_cursor)
    }

    /// Remove queued bubbles without committing them as sent messages —
    /// the daemon dropped these prompts (explicit cancel), they were never
    /// answered and never entered the conversation.
    pub(in crate::cli) fn drop_queued(&mut self, prompt_ids: &[String]) -> Result<()> {
        let ids = prompt_ids.iter().collect::<std::collections::HashSet<_>>();
        if !self
            .queued
            .iter()
            .any(|prompt| ids.contains(&prompt.prompt_id))
        {
            return Ok(());
        }
        let output_cursor = self.output_cursor;
        self.suspend()?;
        self.queued
            .retain(|prompt| !ids.contains(&prompt.prompt_id));
        self.resume_at(output_cursor)
    }

    pub(in crate::cli) fn consume_queued(
        &mut self,
        prompt_ids: &[String],
        mode: AgentMode,
    ) -> Result<()> {
        self.suspend()?;
        let ids = prompt_ids.iter().collect::<std::collections::HashSet<_>>();
        let consumed = self
            .queued
            .iter()
            .filter(|prompt| ids.contains(&prompt.prompt_id))
            .map(|prompt| (prompt.display_content.as_str(), mode))
            .collect::<Vec<_>>();
        write_committed_user_messages(&consumed, true)?;
        self.queued
            .retain(|prompt| !ids.contains(&prompt.prompt_id));
        let output_cursor = cursor_position_or(self.output_cursor);
        self.output_cursor = output_cursor;
        self.resume_at(output_cursor)
    }

    pub(in crate::cli) fn reload_queue(&mut self, state: &StateStore) -> Result<()> {
        let output_cursor = self.output_cursor;
        self.suspend()?;
        self.queued = state.load_queued_prompts()?;
        self.resume_at(output_cursor)
    }
}
