//! 屏幕底部的活动区。
//!
//! REPL 把屏幕分成两半：上面是只增不改的对话正文，下面几行是随时重画的活动
//! 区——输入框、footer、排队提示、后台任务条。活动区要在正文不断追加的同时
//! 稳稳待在底部，所以这里全是绝对行号与光标账：哪一行是活动区起点、正文能
//! 用到第几行、重画时该滚几行。
//!
//! 这套记账是终端渲染最容易出错的地方（2026-08-17 的图片错位就出在这里），
//! 改动前先看 `trace_tail_redraw` 留下的诊断开关。

// 活动区还用着一批留在 cli::mod 的东西（footer 结构、队列渲染、job 条）。
mod frame;
mod queue;

#[cfg(test)]
pub(in crate::cli) use frame::queue_lifted_frame;

use crate::cli::repl::editor::*;
use crate::cli::*;

/// crossterm asks the terminal where the cursor is (`ESC[6n`) and gives up if
/// the reply does not arrive within a fixed wait. Over a laggy SSH link that
/// wait expires routinely, and every `?` on it used to take the whole REPL
/// down with "The cursor position could not be read within a normal duration".
/// The answer is only ever used to re-anchor a redraw, so a stale one costs a
/// single imperfect frame — losing the session costs the session.
/// 活动区重绘轨迹（`MIYU_TAIL_TRACE=1` 打开，落
/// `~/.miyu/cache/logs/tail-trace.log`）。
///
/// 这段重绘靠绝对屏幕行号 + DECSTBM 受限滚动区 + 插入/删除行来搬动活动
/// 区（上边距为 1 时，受限区里滚出去的行照样进 scrollback）。kitty 的占位
/// 符图片在受限区滚动下会留残影（见 frame.rs 的 `queue_lifted_frame`），所
/// 以发过图之后腾地方改走整屏滚。要定位就得看出错那一刻实际发了哪些序列。
#[allow(clippy::too_many_arguments)]
pub(in crate::cli) fn trace_tail_redraw(
    tail_start: u16,
    next_tail: u16,
    shift: i32,
    tail_rows: u16,
    output_cursor: (u16, u16),
    output_bottom: Option<u16>,
    leading_scroll: u16,
    terminal_rows: u16,
    transaction: &[u8],
) {
    use std::io::Write as _;
    // 只记会搬动屏幕内容的序列,纯重绘噪声太大。
    let escapes = String::from_utf8_lossy(transaction);
    let mut moves = Vec::new();
    for (marker, label) in [
        ("L", "IL 插入行"),
        ("M", "DL 删除行"),
        ("r", "DECSTBM 滚动区"),
    ] {
        let pattern = format!("\x1b[");
        let mut rest = escapes.as_ref();
        while let Some(index) = rest.find(&pattern) {
            rest = &rest[index + pattern.len()..];
            if let Some(end) = rest.find(|c: char| c.is_ascii_alphabetic()) {
                if &rest[end..end + 1] == marker {
                    moves.push(format!("{label}({})", &rest[..end]));
                }
            }
        }
    }
    let line = format!(
        "tail {tail_start}→{next_tail} shift={shift} rows={tail_rows} \
         cursor={output_cursor:?} bottom={output_bottom:?} \
         leading_scroll={leading_scroll} term_rows={terminal_rows} \
         | {}\n",
        if moves.is_empty() {
            "无搬动".to_string()
        } else {
            moves.join(" ")
        }
    );
    let path = std::path::Path::new(&std::env::var("HOME").unwrap_or_default())
        .join(".miyu/cache/logs/tail-trace.log");
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        let _ = file.write_all(line.as_bytes());
    }
}

pub(in crate::cli) fn cursor_position_or(fallback: (u16, u16)) -> (u16, u16) {
    // 终端已挂断时 ESC[6n 永远等不到应答,而 crossterm 的应答等待对
    // HUP fd 会无限自旋(超时失效)——直接用回退值,让退出路径走完。
    if terminal_hangup() {
        return fallback;
    }
    cursor::position().unwrap_or(fallback)
}

/// 从 `start` 起把 `frame` 写进终端后光标停在哪。追踪器不知道页高,顶到页底
/// 之后终端是滚动而不是继续往下,所以行号封在最后一行。
pub(in crate::cli) fn cursor_after_frame(
    frame: &[u8],
    start: (u16, u16),
    columns: u16,
    terminal_rows: u16,
) -> (u16, u16) {
    let layout = terminal_frame_layout(frame, start, columns, None);
    (
        layout.cursor.0,
        layout.cursor.1.min(terminal_rows.saturating_sub(1)),
    )
}

pub(in crate::cli) fn cursor_row_or(fallback: u16) -> u16 {
    cursor_position_or((0, fallback)).1
}

pub(in crate::cli) fn cursor_col_or(fallback: u16) -> u16 {
    cursor_position_or((fallback, 0)).0
}

pub(in crate::cli) struct LiveReplTail {
    pub(in crate::cli) editor: LiveReplEditor,
    pub(in crate::cli) queued: Vec<QueuedPrompt>,
    pub(in crate::cli) pending_chunks: Vec<ChatStreamChunk>,
    pub(in crate::cli) footer: ReplFooterStatus,
    /// 回合中途逐请求刷新计量时的基线(回合开始前的 footer 快照)。
    /// 每次 RoundUsage 事件都从基线重新叠加,避免累计值重复相加;
    /// 任何权威更新(set_footer)都会清掉它。
    pub(in crate::cli) round_base_footer: Option<Box<ReplFooterStatus>>,
    /// footer 行相对 tail_start 的偏移(每次输入区渲染时更新)。存偏移而非
    /// 绝对行:apply_output_frame 用 \x1b[L/M 整体平移 tail 时不重画,绝对
    /// 行号会过期——tick 在旧行覆写就画出第二份 footer(孤儿),取消回合后
    /// 那行永远没人清(用户 08-20 截图实锤)。
    pub(in crate::cli) footer_offset: Option<u16>,
    pub(in crate::cli) footer_spinner_last: Option<std::time::Instant>,
    pub(in crate::cli) jobs: Vec<crate::tools::jobs::JobOverview>,
    pub(in crate::cli) job_spinner: usize,
    pub(in crate::cli) output_cursor: (u16, u16),
    pub(in crate::cli) tail_start: u16,
    pub(in crate::cli) tail_rows: u16,
    pub(in crate::cli) input_cursor: (u16, u16),
    pub(in crate::cli) rendered: bool,
    pub(in crate::cli) external_output_active: bool,
    pub(in crate::cli) raw_mode_handoff: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::cli) struct LiveTailPlacement {
    pub(in crate::cli) output_row: u16,
    pub(in crate::cli) tail_start: u16,
    pub(in crate::cli) overflow: u16,
    pub(in crate::cli) anchored: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::cli) struct TerminalFrameLayout {
    pub(in crate::cli) cursor: (u16, u16),
    pub(in crate::cli) occupied_bottom: Option<u16>,
}

/// 帧里一次「顶到页底、把正文滚上去一行」的事件。
///
/// `end` 是引发这次滚动的字节(换行符,或折行的那个字位)在帧里的**结束偏移**,
/// 帧从这里切开,前一段恰好滚了这么多次;`col_after` 是滚完之后光标停的列
/// (换行是 0,折行是那个字位的宽度),后一段从这里接着写。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::cli) struct FrameScroll {
    pub(in crate::cli) end: usize,
    pub(in crate::cli) col_after: u16,
}

pub(in crate::cli) struct TerminalFrameTracker {
    pub(in crate::cli) columns: usize,
    pub(in crate::cli) bottom_margin: Option<usize>,
    pub(in crate::cli) cursor_col: usize,
    pub(in crate::cli) cursor_row: usize,
    pub(in crate::cli) saved_cursor: (usize, usize, bool),
    pub(in crate::cli) pending_wrap: bool,
    pub(in crate::cli) pending_text: String,
    /// `pending_text` 里每个 char 在帧里的结束偏移,与其一一对应。
    pub(in crate::cli) pending_ends: Vec<usize>,
    pub(in crate::cli) occupied_bottom: Option<usize>,
    /// 正在喂给追踪器的字节在帧里的结束偏移(喂第 i 个字节时为 i+1)。
    pub(in crate::cli) byte_index: usize,
    pub(in crate::cli) scrolls: Vec<FrameScroll>,
}

impl TerminalFrameTracker {
    pub(in crate::cli) fn new(start: (u16, u16), columns: u16, bottom_margin: Option<u16>) -> Self {
        let columns = usize::from(columns.max(1));
        let cursor_col = usize::from(start.0).min(columns.saturating_sub(1));
        let cursor_row = usize::from(start.1);
        Self {
            columns,
            bottom_margin: bottom_margin.map(usize::from),
            cursor_col,
            cursor_row,
            saved_cursor: (cursor_col, cursor_row, false),
            pending_wrap: false,
            pending_text: String::new(),
            pending_ends: Vec::new(),
            occupied_bottom: None,
            byte_index: 0,
            scrolls: Vec::new(),
        }
    }

    pub(in crate::cli) fn finish(mut self) -> TerminalFrameLayout {
        self.flush_text();
        TerminalFrameLayout {
            cursor: (
                self.cursor_col.min(u16::MAX as usize) as u16,
                self.cursor_row.min(u16::MAX as usize) as u16,
            ),
            occupied_bottom: self
                .occupied_bottom
                .map(|row| row.min(u16::MAX as usize) as u16),
        }
    }

    pub(in crate::cli) fn finish_with_scrolls(mut self) -> (TerminalFrameLayout, Vec<FrameScroll>) {
        self.flush_text();
        let scrolls = std::mem::take(&mut self.scrolls);
        (self.finish(), scrolls)
    }

    pub(in crate::cli) fn flush_text(&mut self) {
        if self.pending_text.is_empty() {
            return;
        }
        let text = std::mem::take(&mut self.pending_text);
        let ends = std::mem::take(&mut self.pending_ends);
        // 折行引发的滚动要记在那个字位自己的结束偏移上,而不是记在触发
        // flush 的控制字节上——否则一个字位和紧随的换行会记成同一个偏移,
        // 帧就没法在两次滚动之间切开。
        let current = self.byte_index;
        let mut chars_seen = 0usize;
        for grapheme in text.graphemes(true) {
            chars_seen = chars_seen.saturating_add(grapheme.chars().count());
            self.byte_index = ends
                .get(chars_seen.saturating_sub(1))
                .copied()
                .unwrap_or(current);
            self.print_width(UnicodeWidthStr::width(grapheme));
        }
        self.byte_index = current;
    }

    pub(in crate::cli) fn print_width(&mut self, width: usize) {
        if width == 0 {
            return;
        }
        let mut scrolled = false;
        if self.pending_wrap || self.cursor_col.saturating_add(width) > self.columns {
            self.cursor_col = 0;
            scrolled = self.index();
            self.pending_wrap = false;
        }
        self.occupied_bottom = Some(
            self.occupied_bottom
                .map_or(self.cursor_row, |row| row.max(self.cursor_row)),
        );
        let next_col = self.cursor_col.saturating_add(width);
        if next_col >= self.columns {
            self.cursor_col = self.columns.saturating_sub(1);
            self.pending_wrap = true;
        } else {
            self.cursor_col = next_col;
        }
        if scrolled {
            if let Some(last) = self.scrolls.last_mut() {
                last.col_after = self.cursor_col.min(u16::MAX as usize) as u16;
            }
        }
    }

    /// 光标下移一行;顶在页底时记一次滚动并返回 true。
    pub(in crate::cli) fn index(&mut self) -> bool {
        if self
            .bottom_margin
            .is_some_and(|bottom| self.cursor_row >= bottom)
        {
            self.scrolls.push(FrameScroll {
                end: self.byte_index,
                col_after: self.cursor_col.min(u16::MAX as usize) as u16,
            });
            return true;
        }
        self.cursor_row = self.cursor_row.saturating_add(1);
        false
    }

    pub(in crate::cli) fn move_down(&mut self, count: usize) {
        self.pending_wrap = false;
        self.cursor_row = self.cursor_row.saturating_add(count);
        if let Some(bottom) = self.bottom_margin {
            self.cursor_row = self.cursor_row.min(bottom);
        }
    }

    pub(in crate::cli) fn move_up(&mut self, count: usize) {
        self.pending_wrap = false;
        self.cursor_row = self.cursor_row.saturating_sub(count);
    }

    pub(in crate::cli) fn move_right(&mut self, count: usize) {
        self.pending_wrap = false;
        self.cursor_col = self
            .cursor_col
            .saturating_add(count)
            .min(self.columns.saturating_sub(1));
    }

    pub(in crate::cli) fn move_left(&mut self, count: usize) {
        self.pending_wrap = false;
        self.cursor_col = self.cursor_col.saturating_sub(count);
    }

    pub(in crate::cli) fn set_row(&mut self, row: usize) {
        self.pending_wrap = false;
        self.cursor_row = row;
        if let Some(bottom) = self.bottom_margin {
            self.cursor_row = self.cursor_row.min(bottom);
        }
    }

    pub(in crate::cli) fn set_col(&mut self, col: usize) {
        self.pending_wrap = false;
        self.cursor_col = col.min(self.columns.saturating_sub(1));
    }

    pub(in crate::cli) fn param(params: &VteParams, index: usize, default: usize) -> usize {
        params
            .iter()
            .nth(index)
            .and_then(|param| param.first())
            .copied()
            .map(usize::from)
            .filter(|value| *value != 0)
            .unwrap_or(default)
    }
}

impl VtePerform for TerminalFrameTracker {
    fn print(&mut self, character: char) {
        self.pending_text.push(character);
        self.pending_ends.push(self.byte_index);
    }

    fn execute(&mut self, byte: u8) {
        self.flush_text();
        match byte {
            b'\n' => {
                self.cursor_col = 0;
                self.pending_wrap = false;
                self.index();
            }
            b'\r' => self.set_col(0),
            0x08 => self.move_left(1),
            b'\t' => {
                let next = (self.cursor_col / 8 + 1) * 8;
                self.set_col(next);
            }
            0x0b | 0x0c => {
                self.pending_wrap = false;
                self.index();
            }
            _ => {}
        }
    }

    fn csi_dispatch(
        &mut self,
        params: &VteParams,
        _intermediates: &[u8],
        ignore: bool,
        action: char,
    ) {
        self.flush_text();
        if ignore {
            return;
        }
        let count = Self::param(params, 0, 1);
        match action {
            'A' => self.move_up(count),
            'B' | 'e' => self.move_down(count),
            'C' | 'a' => self.move_right(count),
            'D' => self.move_left(count),
            'E' => {
                self.move_down(count);
                self.set_col(0);
            }
            'F' => {
                self.move_up(count);
                self.set_col(0);
            }
            'G' | '`' => self.set_col(count.saturating_sub(1)),
            'H' | 'f' => {
                self.set_row(Self::param(params, 0, 1).saturating_sub(1));
                self.set_col(Self::param(params, 1, 1).saturating_sub(1));
            }
            'd' => self.set_row(count.saturating_sub(1)),
            's' => {
                self.saved_cursor = (self.cursor_col, self.cursor_row, self.pending_wrap);
            }
            'u' => {
                (self.cursor_col, self.cursor_row, self.pending_wrap) = self.saved_cursor;
            }
            _ => {}
        }
    }

    fn esc_dispatch(&mut self, _intermediates: &[u8], ignore: bool, byte: u8) {
        self.flush_text();
        if ignore {
            return;
        }
        match byte {
            b'7' => self.saved_cursor = (self.cursor_col, self.cursor_row, self.pending_wrap),
            b'8' => {
                (self.cursor_col, self.cursor_row, self.pending_wrap) = self.saved_cursor;
            }
            b'D' => {
                self.pending_wrap = false;
                self.index();
            }
            b'E' => {
                self.cursor_col = 0;
                self.pending_wrap = false;
                self.index();
            }
            b'M' => self.move_up(1),
            _ => {}
        }
    }
}

pub(in crate::cli) fn live_frame_output_bottom(
    frame_margin: u16,
    layout: TerminalFrameLayout,
) -> Option<u16> {
    let ends_on_free_line = layout.cursor.0 == 0
        && layout
            .occupied_bottom
            .is_none_or(|bottom| layout.cursor.1 > bottom);
    if ends_on_free_line {
        Some(frame_margin)
    } else {
        frame_margin.checked_sub(1)
    }
}

pub(in crate::cli) fn synchronized_terminal_update<T>(
    cursor_after: CursorAfterUpdate,
    update: impl FnOnce() -> Result<T>,
) -> Result<T> {
    let mut stdout = io::stdout();
    match cursor_after {
        CursorAfterUpdate::Preserve => execute!(stdout, BeginSynchronizedUpdate)?,
        CursorAfterUpdate::Shown | CursorAfterUpdate::Hidden => {
            execute!(stdout, Hide, BeginSynchronizedUpdate)?
        }
    }
    let result = update();
    let end = match cursor_after {
        CursorAfterUpdate::Shown => execute!(stdout, EndSynchronizedUpdate, Show),
        CursorAfterUpdate::Preserve | CursorAfterUpdate::Hidden => {
            execute!(stdout, EndSynchronizedUpdate)
        }
    };
    match result {
        Ok(value) => {
            end?;
            Ok(value)
        }
        Err(error) => {
            let _ = end;
            Err(error)
        }
    }
}

/// Places the tail below the output. `was_anchored` says the tail was already
/// pinned to the bottom: without it a tail that *shrinks* (a background job
/// strip or a queue bubble going away) would spring back up to the output
/// cursor, leaving blank rows under the input box until later output pushed it
/// down again — the input visibly bouncing.
pub(in crate::cli) fn live_tail_placement(
    output_col: u16,
    output_row: u16,
    total_rows: u16,
    terminal_rows: u16,
    was_anchored: bool,
) -> LiveTailPlacement {
    let terminal_rows = terminal_rows.max(1);
    let last_row = terminal_rows.saturating_sub(2);
    let natural_start = output_row.saturating_add(u16::from(output_col > 0));
    let natural_end = natural_start.saturating_add(total_rows.saturating_sub(1));
    let overflow = natural_end.saturating_sub(last_row);
    let output_row = output_row.saturating_sub(overflow);
    let natural_start = output_row.saturating_add(u16::from(output_col > 0));
    let anchored = was_anchored || overflow > 0 || natural_end == last_row;
    let anchored_start = last_row.saturating_add(1).saturating_sub(total_rows);
    let tail_start = if anchored {
        natural_start.max(anchored_start)
    } else {
        natural_start
    };
    // `output_row` is deliberately left where the output actually ended, even
    // when the tail re-anchors below it. It is the contract between the
    // renderer's byte frames and the terminal — the wait spinner erases itself
    // by moving relative to that cursor — so nudging it down to hug the tail
    // leaves orphaned spinner frames in the scrollback.
    LiveTailPlacement {
        output_row,
        tail_start,
        overflow,
        anchored,
    }
}

/// Where a streaming output frame should leave the tail.
///
/// Normally the tail follows the output cursor. A tail already pinned to the
/// bottom stays pinned instead: output fills the rows above it (the frame sets
/// a scroll region so it cannot reach the tail). Letting it slide back up is
/// what made the input box bounce — the rows a finished job strip freed would
/// be reclaimed on the very next frame, then handed back a line of output
/// later.
pub(in crate::cli) fn live_tail_next_start(
    current_start: u16,
    desired_tail: u16,
    max_tail: u16,
) -> u16 {
    if current_start >= max_tail {
        max_tail
    } else {
        desired_tail.min(max_tail)
    }
}

pub(in crate::cli) fn max_live_tail_start(terminal_rows: u16, tail_rows: u16) -> u16 {
    terminal_rows
        .max(1)
        .saturating_sub(1)
        .saturating_sub(tail_rows)
}

impl LiveReplTail {
    pub(in crate::cli) fn new(
        mode: AgentMode,
        history: Vec<ReplHistoryEntry>,
        queued: Vec<QueuedPrompt>,
        footer: ReplFooterStatus,
    ) -> Result<Self> {
        Ok(Self {
            editor: LiveReplEditor::new(mode, history),
            queued,
            pending_chunks: Vec::new(),
            footer,
            round_base_footer: None,
            footer_offset: None,
            footer_spinner_last: None,
            jobs: Vec::new(),
            job_spinner: 0,
            output_cursor: cursor_position_or((0, 0)),
            tail_start: 0,
            tail_rows: 0,
            input_cursor: (0, 0),
            rendered: false,
            external_output_active: false,
            raw_mode_handoff: false,
        })
    }

    pub(in crate::cli) fn mode(&self) -> AgentMode {
        self.editor.mode
    }

    pub(in crate::cli) fn set_footer(&mut self, footer: ReplFooterStatus) {
        self.footer = footer;
        self.round_base_footer = None;
        self.footer_spinner_last = None;
    }

    /// 回合收尾:熄掉运行转轮并原地重绘 footer 行(不整帧重绘)。
    pub(in crate::cli) fn stop_footer_spinner(&mut self) -> Result<()> {
        if self.footer.running_spinner.take().is_none() {
            return Ok(());
        }
        self.footer_spinner_last = None;
        let Some(offset) = self.footer_offset else {
            return Ok(());
        };
        let row = self.tail_start.saturating_add(offset);
        if !self.rendered {
            return Ok(());
        }
        let (cols, rows) = terminal::size().unwrap_or((80, 24));
        if row >= rows {
            return Ok(());
        }
        let line =
            crate::cli::footer::repl_footer_line(self.editor.mode, &self.footer, usize::from(cols));
        let input_cursor = self.input_cursor;
        synchronized_terminal_update(CursorAfterUpdate::Preserve, || {
            let mut stdout = io::stdout();
            queue!(stdout, MoveTo(0, row), Print(line))?;
            queue!(stdout, MoveTo(input_cursor.0, input_cursor.1))?;
            stdout.flush()?;
            Ok(())
        })
    }

    /// 回合内一次模型请求结束:用基线+回合累计刷新计量并立即重绘。
    /// `context_tokens` 取该请求 prompt+completion,即当前上下文占用的
    /// 最新实测;回合结束后外层会用权威数字覆盖(set_footer 清基线)。
    pub(in crate::cli) fn refresh_round_usage(
        &mut self,
        context_tokens: u64,
        turn: TurnTokens,
        speed: GenerationSpeed,
    ) -> Result<()> {
        let base = self
            .round_base_footer
            .get_or_insert_with(|| Box::new(self.footer.clone()));
        let mut display = (**base).clone();
        display.apply_round_usage(context_tokens, turn, speed);
        // 基线快照拍于回合开始(转轮未起),别让计量刷新把转轮拍灭。
        display.running_spinner = self.footer.running_spinner;
        self.footer = display;
        if self.rendered && !self.external_output_active {
            synchronized_terminal_update(CursorAfterUpdate::Shown, || self.redraw())?;
        }
        Ok(())
    }

    /// Replaces the footer and redraws the live editor immediately when it is
    /// already on screen. Without the redraw, token/context updates remain
    /// invisible until the next input event causes the editor to render.
    /// Update the background-command strip; returns true when a redraw is
    /// needed (content changed, or spinners/timers must advance).
    pub(in crate::cli) fn set_jobs(&mut self, jobs: Vec<crate::tools::jobs::JobOverview>) -> bool {
        let changed = self.jobs.len() != jobs.len()
            || self
                .jobs
                .iter()
                .zip(jobs.iter())
                .any(|(a, b)| a.job_id != b.job_id || a.status != b.status);
        self.jobs = jobs;
        changed
    }

    /// Lightweight spinner/timer repaint of the job strip only — no full
    /// tail redraw, so it can run at animation frequency without flicker.
    pub(in crate::cli) fn tick_job_strip(&mut self) -> Result<()> {
        if !self.rendered || self.jobs.is_empty() {
            return Ok(());
        }
        self.job_spinner = self.job_spinner.wrapping_add(1);
        let (cols, _) = terminal::size().unwrap_or((80, 24));
        let lines = background_job_lines(&self.jobs, self.job_spinner, usize::from(cols));
        let rows = lines.len().min(u16::MAX as usize) as u16;
        if rows > self.tail_rows {
            return Ok(());
        }
        let start = self
            .tail_start
            .saturating_add(self.tail_rows)
            .saturating_sub(rows);
        let input_cursor = self.input_cursor;
        // Lines are padded to the full terminal width, so plain overwrites
        // suffice — no Clear, no intermediate blank state. The synchronized
        // block keeps the cursor hop invisible over slow links (SSH).
        synchronized_terminal_update(CursorAfterUpdate::Preserve, || {
            let mut stdout = io::stdout();
            let mut row = start;
            for line in &lines {
                queue!(stdout, MoveTo(0, row), Print(line))?;
                row = row.saturating_add(1);
            }
            queue!(stdout, MoveTo(input_cursor.0, input_cursor.1))?;
            stdout.flush()?;
            Ok(())
        })
    }

    pub(in crate::cli) fn refresh_footer(&mut self, footer: ReplFooterStatus) -> Result<()> {
        self.set_footer(footer);
        if self.rendered {
            synchronized_terminal_update(CursorAfterUpdate::Shown, || self.redraw())?;
        }
        Ok(())
    }

    pub(in crate::cli) fn tick_spinner(
        &mut self,
        renderer: &mut render::StreamRenderer,
    ) -> Result<()> {
        self.flush_pending_chunks(renderer)?;
        renderer.tick_spinner()?;
        self.apply_renderer_frame(renderer)?;
        self.tick_footer_spinner()
    }

    /// footer 里的运行转轮:单行覆写(footer 行全宽 padding,直接盖不闪),
    /// 33ms 的 spinner tick 上节流到 ~80ms 一帧。
    pub(in crate::cli) fn tick_footer_spinner(&mut self) -> Result<()> {
        if !self.rendered || self.external_output_active {
            return Ok(());
        }
        let now = std::time::Instant::now();
        if self
            .footer_spinner_last
            .is_some_and(|last| now.duration_since(last) < std::time::Duration::from_millis(80))
        {
            return Ok(());
        }
        self.footer_spinner_last = Some(now);
        self.footer.running_spinner =
            Some(self.footer.running_spinner.map_or(0, |f| f.wrapping_add(1)));
        let Some(offset) = self.footer_offset else {
            return Ok(());
        };
        let row = self.tail_start.saturating_add(offset);
        let (cols, rows) = terminal::size().unwrap_or((80, 24));
        if row >= rows {
            return Ok(());
        }
        let line =
            crate::cli::footer::repl_footer_line(self.editor.mode, &self.footer, usize::from(cols));
        let input_cursor = self.input_cursor;
        synchronized_terminal_update(CursorAfterUpdate::Preserve, || {
            let mut stdout = io::stdout();
            queue!(stdout, MoveTo(0, row), Print(line))?;
            queue!(stdout, MoveTo(input_cursor.0, input_cursor.1))?;
            stdout.flush()?;
            Ok(())
        })
    }
}

pub(in crate::cli) struct LiveRawMode {
    pub(in crate::cli) restore_terminal_on_drop: bool,
    pub(in crate::cli) keyboard_enhancement: KeyboardEnhancementState,
}

impl LiveRawMode {
    /// 进入 live REPL 的 raw 输入模式，并尽量启用键盘增强协议。
    ///
    /// 参数: 无
    ///
    /// 返回:
    /// - 成功时返回会在 Drop 时恢复终端的守卫对象
    pub(in crate::cli) fn start() -> Result<Self> {
        enable_live_raw_mode()?;
        let mut stdout = io::stdout();
        if let Err(error) = execute!(stdout, EnableBracketedPaste) {
            let _ = terminal::disable_raw_mode();
            return Err(error.into());
        }
        // Focus reporting is advisory: terminals that ignore it simply never
        // send the events, and the editor stays on its "focused" default.
        let _ = execute!(stdout, EnableFocusChange);
        Ok(Self {
            restore_terminal_on_drop: true,
            keyboard_enhancement: KeyboardEnhancementState::enable(&mut stdout),
        })
    }

    /// 接管上一段 live 输入已启用的终端模式，避免重复 Push 键盘增强。
    ///
    /// 参数: 无
    ///
    /// 返回:
    /// - 会在最终 Drop 时恢复终端的守卫对象
    pub(in crate::cli) fn adopt() -> Self {
        Self {
            restore_terminal_on_drop: true,
            keyboard_enhancement: KeyboardEnhancementState::assume_active(),
        }
    }

    pub(in crate::cli) fn handoff(&mut self) {
        self.restore_terminal_on_drop = false;
        // handoff 后由下一段 LiveRawMode::adopt 继续持有键盘增强状态
        self.keyboard_enhancement = KeyboardEnhancementState::default();
    }
}

pub(in crate::cli) fn enable_live_raw_mode() -> Result<()> {
    terminal::enable_raw_mode()?;
    spawn_hangup_watchdog();
    if let Err(error) = restore_live_output_processing() {
        let _ = terminal::disable_raw_mode();
        return Err(error);
    }
    Ok(())
}

impl Drop for LiveRawMode {
    fn drop(&mut self) {
        if !self.restore_terminal_on_drop {
            return;
        }
        let mut stdout = io::stdout();
        let _ = execute!(stdout, DisableBracketedPaste, DisableFocusChange, Show);
        // 1. 先 Pop 键盘增强协议
        // 2. 再退出 raw mode
        self.keyboard_enhancement.disable(&mut stdout);
        let _ = terminal::disable_raw_mode();
    }
}
