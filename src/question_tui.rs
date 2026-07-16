use crate::i18n::text as t;
use crate::question::{
    validate_answers, QuestionAnswers, QuestionPrompt, QuestionRequest, QuestionResponse,
    MAX_CUSTOM_ANSWER_CHARS,
};
use anyhow::{bail, Result};
use crossterm::cursor::{Hide, MoveTo, Show};
use crossterm::event::{
    self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers,
};
use crossterm::terminal::{self, Clear, ClearType};
use crossterm::{execute, queue};
use std::io::{self, IsTerminal, Write};
use std::time::{Duration, Instant};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

const MAX_PANEL_LINES: u16 = 16;
const CANCEL_CONFIRM_WINDOW: Duration = Duration::from_secs(2);
const BAR: &str = "\x1b[1m\x1b[35m┃\x1b[0m";
const ANSWERED_BAR: &str = "\x1b[2m\x1b[90m┃\x1b[0m";

pub fn available(plain: bool) -> bool {
    if plain || !io::stdout().is_terminal() {
        return false;
    }
    #[cfg(unix)]
    {
        std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/tty")
            .is_ok()
    }
    #[cfg(not(unix))]
    {
        io::stdin().is_terminal()
    }
}

pub fn ask(request: &QuestionRequest) -> Result<QuestionResponse> {
    request.validate()?;
    if !available(false) {
        bail!("interactive terminal is unavailable");
    }

    let panel_lines = terminal::size()
        .map(|(_, rows)| rows.saturating_sub(1).clamp(1, MAX_PANEL_LINES))
        .unwrap_or(12);
    reserve_space(panel_lines)?;
    let mut session = QuestionSession::start(panel_lines)?;
    let mut state = QuestionState::new(request);

    loop {
        if state
            .cancel_armed_until
            .is_some_and(|deadline| Instant::now() >= deadline)
        {
            state.cancel_armed_until = None;
        }
        draw(&mut session, request, &mut state)?;

        if !event::poll(Duration::from_millis(100))? {
            continue;
        }
        let event = event::read()?;
        match event {
            Event::Resize(_, rows) => {
                session.resize_to_terminal(rows);
                continue;
            }
            Event::Paste(text) if state.editing => {
                insert_text(&mut state.edit_buffer, &mut state.edit_cursor, &text);
            }
            Event::Key(key) => {
                if key.kind != KeyEventKind::Press {
                    continue;
                }
                if matches!(key.code, KeyCode::Char('c'))
                    && key.modifiers.contains(KeyModifiers::CONTROL)
                {
                    session.finish_cancelled()?;
                    return Ok(QuestionResponse::Cancelled);
                }
                if state.editing {
                    if handle_editing_key(request, &mut state, key)? && !request.needs_review() {
                        if let Some(answers) = submitted_answers(request, &state)? {
                            session.finish_answered(request, &answers)?;
                            return Ok(QuestionResponse::Answered(answers));
                        }
                    }
                    continue;
                }

                if key.code == KeyCode::Esc {
                    if state
                        .cancel_armed_until
                        .is_some_and(|deadline| Instant::now() < deadline)
                    {
                        session.finish_cancelled()?;
                        return Ok(QuestionResponse::Cancelled);
                    }
                    state.cancel_armed_until = Some(Instant::now() + CANCEL_CONFIRM_WINDOW);
                    continue;
                }
                state.cancel_armed_until = None;

                if state.on_confirm(request) {
                    match key.code {
                        KeyCode::Left | KeyCode::Char('h') => state.previous_tab(request),
                        KeyCode::Right | KeyCode::Char('l') => state.next_tab(request),
                        KeyCode::Enter => {
                            if let Some(answers) = submitted_answers(request, &state)? {
                                session.finish_answered(request, &answers)?;
                                return Ok(QuestionResponse::Answered(answers));
                            }
                            state.go_to_first_unanswered(request);
                        }
                        _ => {}
                    }
                    continue;
                }

                let question = &request.questions[state.tab];
                match key.code {
                    KeyCode::Left | KeyCode::Char('h') => state.previous_tab(request),
                    KeyCode::Right | KeyCode::Char('l') => state.next_tab(request),
                    KeyCode::Up | KeyCode::Char('k') => state.previous_option(question),
                    KeyCode::Down | KeyCode::Char('j') => state.next_option(question),
                    KeyCode::Tab | KeyCode::Char(' ') if question.multiple => {
                        state.toggle_current(request)?;
                    }
                    KeyCode::Enter if question.multiple => {
                        state.activate_current(request)?;
                    }
                    KeyCode::Enter => {
                        state.activate_current(request)?;
                        if !request.needs_review() {
                            if let Some(answers) = submitted_answers(request, &state)? {
                                session.finish_answered(request, &answers)?;
                                return Ok(QuestionResponse::Answered(answers));
                            }
                        }
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }
}

struct QuestionState {
    tab: usize,
    selected: Vec<usize>,
    scroll_starts: Vec<usize>,
    answers: QuestionAnswers,
    custom_answers: Vec<String>,
    editing: bool,
    edit_buffer: String,
    edit_cursor: usize,
    cancel_armed_until: Option<Instant>,
}

impl QuestionState {
    fn new(request: &QuestionRequest) -> Self {
        Self {
            tab: 0,
            selected: vec![0; request.questions.len()],
            scroll_starts: vec![0; request.questions.len() + usize::from(request.needs_review())],
            answers: vec![Vec::new(); request.questions.len()],
            custom_answers: vec![String::new(); request.questions.len()],
            editing: false,
            edit_buffer: String::new(),
            edit_cursor: 0,
            cancel_armed_until: None,
        }
    }

    fn on_confirm(&self, request: &QuestionRequest) -> bool {
        request.needs_review() && self.tab == request.questions.len()
    }

    fn tab_count(&self, request: &QuestionRequest) -> usize {
        request.questions.len() + usize::from(request.needs_review())
    }

    fn previous_tab(&mut self, request: &QuestionRequest) {
        let count = self.tab_count(request);
        self.tab = (self.tab + count - 1) % count;
    }

    fn next_tab(&mut self, request: &QuestionRequest) {
        self.tab = (self.tab + 1) % self.tab_count(request);
    }

    fn previous_option(&mut self, question: &QuestionPrompt) {
        let count = option_count(question);
        if count > 0 {
            let selected = &mut self.selected[self.tab];
            *selected = (*selected + count - 1) % count;
        }
    }

    fn next_option(&mut self, question: &QuestionPrompt) {
        let count = option_count(question);
        if count > 0 {
            self.selected[self.tab] = (self.selected[self.tab] + 1) % count;
        }
    }

    fn activate_current(&mut self, request: &QuestionRequest) -> Result<()> {
        let question = &request.questions[self.tab];
        let selected = self.selected[self.tab];
        if selected == question.options.len() && question.custom {
            self.editing = true;
            self.edit_buffer = self.custom_answers[self.tab].clone();
            self.edit_cursor = self.edit_buffer.chars().count();
            return Ok(());
        }
        let Some(option) = question.options.get(selected) else {
            bail!("selected question option is out of range");
        };
        if question.multiple {
            toggle_answer(&mut self.answers[self.tab], &option.label);
        } else {
            self.answers[self.tab] = vec![option.label.clone()];
            self.advance_after_single(request);
        }
        Ok(())
    }

    fn toggle_current(&mut self, request: &QuestionRequest) -> Result<()> {
        let question = &request.questions[self.tab];
        let selected = self.selected[self.tab];
        if selected == question.options.len() && question.custom {
            let custom = self.custom_answers[self.tab].trim();
            if custom.is_empty() {
                return self.activate_current(request);
            }
            toggle_answer(&mut self.answers[self.tab], custom);
            return Ok(());
        }
        self.activate_current(request)
    }

    fn advance_after_single(&mut self, request: &QuestionRequest) {
        if request.questions.len() == 1 && !request.needs_review() {
            return;
        }
        self.tab = (self.tab + 1).min(self.tab_count(request) - 1);
    }

    fn go_to_first_unanswered(&mut self, request: &QuestionRequest) {
        if let Some(index) = self.answers.iter().position(Vec::is_empty) {
            self.tab = index.min(request.questions.len().saturating_sub(1));
        }
    }
}

fn handle_editing_key(
    request: &QuestionRequest,
    state: &mut QuestionState,
    key: KeyEvent,
) -> Result<bool> {
    match key.code {
        KeyCode::Char('j') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            insert_text(&mut state.edit_buffer, &mut state.edit_cursor, "\n");
        }
        KeyCode::Esc => {
            state.editing = false;
            state.edit_buffer.clear();
            state.edit_cursor = 0;
        }
        KeyCode::Enter => {
            let value = state.edit_buffer.trim().to_string();
            if value.is_empty() {
                let previous = std::mem::take(&mut state.custom_answers[state.tab]);
                state.answers[state.tab].retain(|answer| answer != &previous);
                state.editing = false;
                state.edit_buffer.clear();
                state.edit_cursor = 0;
                return Ok(false);
            }
            let question = &request.questions[state.tab];
            let previous = std::mem::replace(&mut state.custom_answers[state.tab], value.clone());
            if !previous.is_empty() {
                state.answers[state.tab].retain(|answer| answer != &previous);
            }
            if question.multiple {
                if !state.answers[state.tab].contains(&value) {
                    state.answers[state.tab].push(value);
                }
            } else {
                state.answers[state.tab] = vec![value];
            }
            state.editing = false;
            state.edit_buffer.clear();
            state.edit_cursor = 0;
            if !question.multiple {
                state.advance_after_single(request);
            }
            return Ok(true);
        }
        KeyCode::Left => state.edit_cursor = state.edit_cursor.saturating_sub(1),
        KeyCode::Right => {
            state.edit_cursor = (state.edit_cursor + 1).min(state.edit_buffer.chars().count())
        }
        KeyCode::Home => state.edit_cursor = 0,
        KeyCode::End => state.edit_cursor = state.edit_buffer.chars().count(),
        KeyCode::Backspace => remove_before_cursor(&mut state.edit_buffer, &mut state.edit_cursor),
        KeyCode::Delete => remove_at_cursor(&mut state.edit_buffer, state.edit_cursor),
        KeyCode::Char(ch) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
            insert_text(
                &mut state.edit_buffer,
                &mut state.edit_cursor,
                &ch.to_string(),
            );
        }
        _ => {}
    }
    Ok(false)
}

fn submitted_answers(
    request: &QuestionRequest,
    state: &QuestionState,
) -> Result<Option<QuestionAnswers>> {
    if state.editing || state.answers.iter().any(Vec::is_empty) {
        return Ok(None);
    }
    if request.needs_review() && !state.on_confirm(request) {
        return Ok(None);
    }
    validate_answers(request, &state.answers)?;
    Ok(Some(state.answers.clone()))
}

fn option_count(question: &QuestionPrompt) -> usize {
    question.options.len() + usize::from(question.custom)
}

fn toggle_answer(answers: &mut Vec<String>, value: &str) {
    if let Some(index) = answers.iter().position(|answer| answer == value) {
        answers.remove(index);
    } else {
        answers.push(value.to_string());
    }
}

struct QuestionSession {
    stdout: io::Stdout,
    anchor_y: u16,
    panel_lines: u16,
}

impl QuestionSession {
    fn start(panel_lines: u16) -> Result<Self> {
        terminal::enable_raw_mode()?;
        let mut stdout = io::stdout();
        if let Err(err) = execute!(stdout, EnableBracketedPaste, Hide) {
            let _ = execute!(stdout, DisableBracketedPaste, Show);
            let _ = terminal::disable_raw_mode();
            return Err(err.into());
        }
        let (_, cursor_y) =
            crossterm::cursor::position().unwrap_or((0, panel_lines.saturating_sub(1)));
        let anchor_y = cursor_y.saturating_sub(panel_lines.saturating_sub(1));
        Ok(Self {
            stdout,
            anchor_y,
            panel_lines,
        })
    }

    fn finish_answered(
        &mut self,
        request: &QuestionRequest,
        answers: &QuestionAnswers,
    ) -> Result<()> {
        self.clear()?;
        let width = terminal::size().map(|(cols, _)| cols).unwrap_or(80) as usize;
        let content_width = width.saturating_sub(3).max(1);
        let keeps_blank_line = self.panel_lines > 1;
        let content_rows = self
            .panel_lines
            .saturating_sub(u16::from(keeps_blank_line))
            .max(1);
        let answer_capacity = content_rows.saturating_sub(1) as usize;
        let omitted = request.questions.len().saturating_sub(answer_capacity);
        let mut row = 0u16;
        let heading = if omitted == 0 {
            format!(
                "{} {} {}",
                t("Answered", "已回答"),
                request.questions.len(),
                t("questions", "个问题")
            )
        } else {
            format!(
                "{} {} {} · {} {}",
                t("Answered", "已回答"),
                request.questions.len(),
                t("questions", "个问题"),
                t("omitted", "省略"),
                omitted
            )
        };
        self.write_answered_line(row, &heading, content_width)?;
        row += 1;
        for (question, selected) in request.questions.iter().zip(answers).take(answer_capacity) {
            self.write_answered_line(
                row,
                &format!(
                    "{}: {}",
                    question.header,
                    display_inline(&selected.join("、"))
                ),
                content_width,
            )?;
            row += 1;
        }
        if keeps_blank_line {
            queue!(
                self.stdout,
                MoveTo(0, self.anchor_y.saturating_add(row)),
                Clear(ClearType::CurrentLine),
                crossterm::style::Print("\r\n")
            )?;
        } else {
            queue!(
                self.stdout,
                MoveTo(0, self.anchor_y.saturating_add(row.saturating_sub(1))),
                crossterm::style::Print("\r\n")
            )?;
        }
        queue!(self.stdout, Clear(ClearType::CurrentLine), Show)?;
        self.stdout.flush()?;
        Ok(())
    }

    fn finish_cancelled(&mut self) -> Result<()> {
        self.clear()?;
        queue!(
            self.stdout,
            MoveTo(0, self.anchor_y),
            crossterm::style::Print(format!(
                "{BAR} \x1b[2m{}\x1b[0m",
                t("Question cancelled", "已取消提问")
            )),
            MoveTo(0, self.anchor_y.saturating_add(1)),
            Clear(ClearType::CurrentLine),
            Show
        )?;
        self.stdout.flush()?;
        Ok(())
    }

    fn write_answered_line(&mut self, row: u16, text: &str, width: usize) -> Result<()> {
        queue!(
            self.stdout,
            MoveTo(0, self.anchor_y.saturating_add(row)),
            Clear(ClearType::CurrentLine),
            crossterm::style::Print(ANSWERED_BAR),
            crossterm::style::Print(" \x1b[2m\x1b[90m"),
            crossterm::style::Print(truncate_width(text, width)),
            crossterm::style::Print("\x1b[0m")
        )?;
        Ok(())
    }

    fn clear(&mut self) -> Result<()> {
        for row in 0..self.panel_lines {
            queue!(
                self.stdout,
                MoveTo(0, self.anchor_y.saturating_add(row)),
                Clear(ClearType::CurrentLine)
            )?;
        }
        Ok(())
    }

    fn resize_to_terminal(&mut self, rows: u16) {
        self.panel_lines = rows.saturating_sub(1).clamp(1, MAX_PANEL_LINES);
        self.anchor_y = self.anchor_y.min(rows.saturating_sub(self.panel_lines));
    }
}

impl Drop for QuestionSession {
    fn drop(&mut self) {
        let _ = execute!(self.stdout, DisableBracketedPaste, Show);
        let _ = terminal::disable_raw_mode();
    }
}

fn draw(
    session: &mut QuestionSession,
    request: &QuestionRequest,
    state: &mut QuestionState,
) -> Result<()> {
    session.clear()?;
    let (cols, _) = terminal::size().unwrap_or((80, 24));
    let content_width = (cols as usize).saturating_sub(3).max(1);
    let mut top_lines = Vec::new();
    let mut body_lines = Vec::new();
    let mut footer_lines = Vec::new();
    let mut edit_body_index = None;
    let mut edit_cursor_offset = 0usize;
    let mut edit_cursor_column = 0usize;
    let mut focused_body_index = None;

    if request.needs_review() {
        top_lines.push(tab_line(request, state));
        top_lines.push(String::new());
    }
    if state.on_confirm(request) {
        for (question, selected) in request.questions.iter().zip(&state.answers) {
            let value = if selected.is_empty() {
                format!("\x1b[31m{}\x1b[0m", t("unanswered", "未回答"))
            } else {
                format!("\x1b[2m{}\x1b[0m", display_inline(&selected.join("、")))
            };
            body_lines.push(format!("{}: {value}", question.header));
        }
        footer_lines.push(String::new());
        footer_lines.push(format!(
            "\x1b[2m{}\x1b[0m",
            t(
                "Enter submit · Left/Right switch · Esc twice cancel",
                "Enter 提交 · ←/→ 换题 · Esc 两次取消",
            )
        ));
    } else {
        let question = &request.questions[state.tab];
        top_lines.push(format!("\x1b[1m{}\x1b[0m", question.question.trim()));
        top_lines.push(String::new());
        for (index, option) in question.options.iter().enumerate() {
            let picked = state.answers[state.tab].contains(&option.label);
            if state.selected[state.tab] == index {
                focused_body_index = Some(body_lines.len());
            }
            body_lines.extend(option_lines(
                &option.label,
                &option.description,
                state.selected[state.tab] == index,
                picked,
                question.multiple,
                content_width,
            ));
        }
        if question.custom {
            let index = question.options.len();
            let custom = &state.custom_answers[state.tab];
            let picked = !custom.is_empty() && state.answers[state.tab].contains(custom);
            if state.selected[state.tab] == index {
                focused_body_index = Some(body_lines.len());
            }
            if state.editing && state.selected[state.tab] == index {
                edit_body_index = Some(body_lines.len());
                let editor_prefix_width = if question.multiple { 6 } else { 2 };
                let (editor, cursor_offset) = editor_view(
                    &state.edit_buffer,
                    state.edit_cursor,
                    content_width.saturating_sub(editor_prefix_width),
                );
                edit_cursor_offset = cursor_offset;
                edit_cursor_column = UnicodeWidthStr::width(if question.multiple {
                    "┃ › [ ] "
                } else {
                    "┃ › "
                });
                body_lines.push(editor_option_line(question.multiple, picked, &editor));
            } else {
                let label = if custom.is_empty() {
                    t("Type your own answer", "输入其他答案").to_string()
                } else {
                    format!("{}: {}", t("Custom", "自定义"), display_inline(custom))
                };
                body_lines.extend(option_lines(
                    &label,
                    "",
                    state.selected[state.tab] == index,
                    picked,
                    question.multiple,
                    content_width,
                ));
            }
        }
        footer_lines.push(String::new());
        if state.editing {
            footer_lines.push(format!(
                "\x1b[2m{}\x1b[0m",
                t(
                    "Enter save · Ctrl+J newline · Esc stop editing",
                    "Enter 保存 · Ctrl+J 换行 · Esc 退出编辑"
                )
            ));
        } else {
            let help = if question.multiple {
                t(
                    "Up/Down select · Tab/Space toggle · Enter select/edit · Left/Right switch",
                    "↑/↓ 选择 · Tab/Space 切换 · Enter 选择/编辑 · ←/→ 换题",
                )
            } else {
                t(
                    "Up/Down select · Enter submit · Left/Right switch",
                    "↑/↓ 选择 · Enter 提交 · ←/→ 换题",
                )
            };
            footer_lines.push(format!(
                "\x1b[2m{help} · Esc ×2 {}\x1b[0m",
                t("cancel", "取消")
            ));
        }
    }

    if state.cancel_armed_until.is_some() {
        footer_lines.push(format!(
            "\x1b[1m\x1b[33m{}\x1b[0m",
            t(
                "Press Esc again to cancel this response",
                "再次按 Esc 取消本轮回复"
            )
        ));
    }

    let max_content_lines = session.panel_lines as usize;
    let layout = panel_layout(
        top_lines.len(),
        body_lines.len(),
        footer_lines.len(),
        max_content_lines,
        focused_body_index,
        state.scroll_starts[state.tab],
    );
    state.scroll_starts[state.tab] = layout.body_start;
    let visible_lines = top_lines
        .iter()
        .skip(layout.top_start)
        .chain(
            body_lines
                .iter()
                .skip(layout.body_start)
                .take(layout.body_capacity),
        )
        .chain(footer_lines.iter().skip(layout.footer_start));
    for (row, line) in visible_lines.enumerate() {
        queue!(
            session.stdout,
            MoveTo(0, session.anchor_y.saturating_add(row as u16)),
            Clear(ClearType::CurrentLine),
            crossterm::style::Print(BAR),
            crossterm::style::Print(" "),
            crossterm::style::Print(truncate_width(line, content_width))
        )?;
    }
    if state.editing {
        if let Some(index) = edit_body_index.filter(|index| {
            *index >= layout.body_start
                && *index < layout.body_start.saturating_add(layout.body_capacity)
        }) {
            let row = layout.top_budget + index - layout.body_start;
            let cursor_x = edit_cursor_column.saturating_add(edit_cursor_offset);
            queue!(
                session.stdout,
                MoveTo(
                    cursor_x.min(cols.saturating_sub(1) as usize) as u16,
                    session.anchor_y.saturating_add(row as u16)
                ),
                Show
            )?;
        } else {
            queue!(session.stdout, Show)?;
        }
    } else {
        queue!(session.stdout, Hide)?;
    }
    session.stdout.flush()?;
    Ok(())
}

#[derive(Debug, PartialEq, Eq)]
struct PanelLayout {
    top_start: usize,
    top_budget: usize,
    body_start: usize,
    body_capacity: usize,
    footer_start: usize,
}

fn panel_layout(
    top_len: usize,
    body_len: usize,
    footer_len: usize,
    max_lines: usize,
    focused_body_index: Option<usize>,
    current_body_start: usize,
) -> PanelLayout {
    let footer_budget = footer_len.min(max_lines);
    let top_budget = top_len.min(max_lines.saturating_sub(footer_budget));
    let body_capacity = max_lines.saturating_sub(top_budget + footer_budget);
    let max_body_start = body_len.saturating_sub(body_capacity);
    let mut body_start = current_body_start.min(max_body_start);
    if body_capacity == 0 {
        body_start = 0;
    } else if let Some(index) = focused_body_index {
        if index < body_start {
            body_start = index;
        } else if index >= body_start.saturating_add(body_capacity) {
            body_start = index
                .saturating_add(1)
                .saturating_sub(body_capacity)
                .min(max_body_start);
        }
    }
    PanelLayout {
        top_start: top_len.saturating_sub(top_budget),
        top_budget,
        body_start,
        body_capacity,
        footer_start: footer_len.saturating_sub(footer_budget),
    }
}

fn tab_line(request: &QuestionRequest, state: &QuestionState) -> String {
    let mut parts = Vec::new();
    for (index, question) in request.questions.iter().enumerate() {
        let answered = !state.answers[index].is_empty();
        let label = if answered {
            format!("{} ✓", question.header)
        } else {
            question.header.clone()
        };
        if state.tab == index {
            parts.push(format!("\x1b[7m {label} \x1b[0m"));
        } else {
            parts.push(format!("\x1b[2m{label}\x1b[0m"));
        }
    }
    if request.needs_review() {
        if state.tab == request.questions.len() {
            parts.push(format!("\x1b[7m {} \x1b[0m", t("Review", "确认")));
        } else {
            parts.push(format!("\x1b[2m{}\x1b[0m", t("Review", "确认")));
        }
    }
    parts.join("  ")
}

fn option_lines(
    label: &str,
    description: &str,
    active: bool,
    picked: bool,
    multiple: bool,
    content_width: usize,
) -> Vec<String> {
    let marker = if multiple {
        if picked {
            "\x1b[35m[✓]\x1b[0m "
        } else {
            "\x1b[2m[ ]\x1b[0m "
        }
    } else {
        ""
    };
    let label = if active || picked {
        format!("\x1b[35m{label}\x1b[0m")
    } else {
        label.to_string()
    };
    let pointer = if active { "\x1b[35m›\x1b[0m " } else { "  " };
    let mut lines = vec![format!("{pointer}{marker}{label}")];
    let description = description.trim();
    if !description.is_empty() {
        let indent = if multiple { "      " } else { "  " };
        let width = content_width
            .saturating_sub(UnicodeWidthStr::width(indent))
            .max(1);
        lines.extend(
            wrap_display_text(description, width)
                .into_iter()
                .map(|line| format!("{indent}\x1b[2m{line}\x1b[0m")),
        );
    }
    lines
}

fn editor_option_line(multiple: bool, picked: bool, editor: &str) -> String {
    let marker = if multiple {
        if picked {
            "\x1b[35m[✓]\x1b[0m "
        } else {
            "\x1b[2m[ ]\x1b[0m "
        }
    } else {
        ""
    };
    let value = if editor.is_empty() {
        format!(
            "\x1b[2m{}\x1b[0m",
            t("Type your own answer", "输入其他答案")
        )
    } else {
        editor.to_string()
    };
    format!("\x1b[35m›\x1b[0m {marker}{value}")
}

fn wrap_display_text(value: &str, width: usize) -> Vec<String> {
    let mut lines = Vec::new();
    let mut current = String::new();
    let mut current_width = 0usize;
    for ch in value.chars() {
        let char_width = ch.width().unwrap_or(0);
        if current_width > 0 && current_width.saturating_add(char_width) > width {
            lines.push(std::mem::take(&mut current));
            current_width = 0;
        }
        current.push(ch);
        current_width = current_width.saturating_add(char_width);
    }
    if !current.is_empty() {
        lines.push(current);
    }
    lines
}

fn reserve_space(lines: u16) -> Result<()> {
    for _ in 1..lines {
        println!();
    }
    io::stdout().flush()?;
    Ok(())
}

fn insert_text(value: &mut String, cursor: &mut usize, text: &str) {
    let remaining = MAX_CUSTOM_ANSWER_CHARS.saturating_sub(value.chars().count());
    if remaining == 0 {
        return;
    }
    let sanitized = text
        .chars()
        .flat_map(|ch| {
            if ch == '\t' {
                "  ".chars().collect::<Vec<_>>()
            } else if ch == '\n' || !ch.is_control() {
                vec![ch]
            } else {
                Vec::new()
            }
        })
        .take(remaining)
        .collect::<String>();
    let byte = byte_index(value, *cursor);
    value.insert_str(byte, &sanitized);
    *cursor += sanitized.chars().count();
}

fn display_inline(value: &str) -> String {
    value
        .chars()
        .filter_map(|ch| match ch {
            '\n' | '\r' => Some('↵'),
            '\t' => Some(' '),
            ch if ch.is_control() => None,
            ch => Some(ch),
        })
        .collect()
}

fn editor_view(value: &str, cursor: usize, width: usize) -> (String, usize) {
    if width == 0 {
        return (String::new(), 0);
    }
    let display = display_inline(value);
    let before = display_inline(&value.chars().take(cursor).collect::<String>());
    let cursor_width = UnicodeWidthStr::width(before.as_str());
    if UnicodeWidthStr::width(display.as_str()) <= width {
        return (display, cursor_width.min(width));
    }
    if cursor_width < width {
        return (truncate_plain_width(&display, width), cursor_width);
    }

    let tail_budget = width.saturating_sub(1);
    let mut tail = String::new();
    let mut tail_width = 0usize;
    for ch in before.chars().rev() {
        let ch_width = ch.width().unwrap_or(0);
        if tail_width + ch_width > tail_budget {
            break;
        }
        tail.insert(0, ch);
        tail_width += ch_width;
    }
    let after = display
        .chars()
        .skip(before.chars().count())
        .collect::<String>();
    let mut view = format!("…{tail}");
    let remaining = width.saturating_sub(1 + tail_width);
    view.push_str(&truncate_plain_width(&after, remaining));
    (view, (1 + tail_width).min(width))
}

fn truncate_plain_width(value: &str, max_width: usize) -> String {
    let mut output = String::new();
    let mut width = 0usize;
    for ch in value.chars() {
        let ch_width = ch.width().unwrap_or(0);
        if width + ch_width > max_width {
            break;
        }
        output.push(ch);
        width += ch_width;
    }
    output
}

fn remove_before_cursor(value: &mut String, cursor: &mut usize) {
    if *cursor == 0 {
        return;
    }
    let start = byte_index(value, *cursor - 1);
    let end = byte_index(value, *cursor);
    value.replace_range(start..end, "");
    *cursor -= 1;
}

fn remove_at_cursor(value: &mut String, cursor: usize) {
    if cursor >= value.chars().count() {
        return;
    }
    let start = byte_index(value, cursor);
    let end = byte_index(value, cursor + 1);
    value.replace_range(start..end, "");
}

fn byte_index(value: &str, char_index: usize) -> usize {
    value
        .char_indices()
        .nth(char_index)
        .map(|(index, _)| index)
        .unwrap_or(value.len())
}

fn truncate_width(value: &str, max_width: usize) -> String {
    if UnicodeWidthStr::width(strip_ansi(value).as_str()) <= max_width {
        return value.to_string();
    }
    if max_width <= 3 {
        return ".".repeat(max_width);
    }
    let budget = max_width.saturating_sub(3);
    let mut output = String::new();
    let mut width = 0usize;
    let mut in_escape = false;
    for ch in value.chars() {
        if ch == '\x1b' {
            in_escape = true;
            output.push(ch);
            continue;
        }
        if in_escape {
            output.push(ch);
            if ch == 'm' {
                in_escape = false;
            }
            continue;
        }
        let char_width = ch.width().unwrap_or(0);
        if width + char_width > budget {
            break;
        }
        output.push(ch);
        width += char_width;
    }
    output.push_str("...\x1b[0m");
    output
}

fn strip_ansi(value: &str) -> String {
    let mut output = String::new();
    let mut in_escape = false;
    for ch in value.chars() {
        if ch == '\x1b' {
            in_escape = true;
        } else if in_escape {
            if ch == 'm' {
                in_escape = false;
            }
        } else {
            output.push(ch);
        }
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::question::QuestionOption;

    fn multi_request() -> QuestionRequest {
        QuestionRequest {
            questions: vec![QuestionPrompt {
                header: "范围".to_string(),
                question: "选择范围".to_string(),
                options: vec![
                    QuestionOption {
                        label: "代码".to_string(),
                        description: String::new(),
                    },
                    QuestionOption {
                        label: "文档".to_string(),
                        description: String::new(),
                    },
                ],
                multiple: true,
                custom: true,
            }],
        }
    }

    #[test]
    fn multi_activation_toggles_selected_option() {
        let request = multi_request();
        let mut state = QuestionState::new(&request);
        state.activate_current(&request).unwrap();
        assert_eq!(state.answers[0], vec!["代码"]);
        state.activate_current(&request).unwrap();
        assert!(state.answers[0].is_empty());
    }

    #[test]
    fn left_and_right_cycle_question_tabs() {
        let mut request = multi_request();
        request.questions.push(request.questions[0].clone());
        let mut state = QuestionState::new(&request);
        state.next_tab(&request);
        assert_eq!(state.tab, 1);
        state.previous_tab(&request);
        assert_eq!(state.tab, 0);
    }

    #[test]
    fn custom_input_is_sanitized_and_bounded() {
        let mut value = String::new();
        let mut cursor = 0;
        let input = format!("a\u{1b}\t{}", "b".repeat(MAX_CUSTOM_ANSWER_CHARS));
        insert_text(&mut value, &mut cursor, &input);
        assert!(!value.contains('\u{1b}'));
        assert!(!value.contains('\t'));
        assert_eq!(value.chars().count(), MAX_CUSTOM_ANSWER_CHARS);
    }

    #[test]
    fn editor_view_keeps_caret_visible() {
        let (view, cursor) = editor_view("abcdefghijkl", 10, 6);
        assert!(view.starts_with('…'));
        assert!(cursor <= 6);
        assert!(UnicodeWidthStr::width(view.as_str()) <= 6);
    }

    #[test]
    fn final_answer_waits_on_review_tab() {
        let mut request = multi_request();
        request.questions[0].multiple = false;
        request.questions[0].custom = false;
        request.questions.push(request.questions[0].clone());
        let mut state = QuestionState::new(&request);
        state.activate_current(&request).unwrap();
        state.activate_current(&request).unwrap();
        assert!(state.on_confirm(&request));
        assert!(submitted_answers(&request, &state).unwrap().is_some());
    }

    #[test]
    fn existing_custom_answer_reopens_for_editing() {
        let request = QuestionRequest {
            questions: vec![QuestionPrompt {
                header: "范围".to_string(),
                question: "选择范围".to_string(),
                options: Vec::new(),
                multiple: false,
                custom: true,
            }],
        };
        let mut state = QuestionState::new(&request);
        state.custom_answers[0] = "已有答案".to_string();
        state.answers[0] = vec!["已有答案".to_string()];
        state.activate_current(&request).unwrap();
        assert!(state.editing);
        assert_eq!(state.edit_buffer, "已有答案");
    }

    #[test]
    fn existing_multi_custom_answer_can_be_toggled_off() {
        let mut request = multi_request();
        let mut state = QuestionState::new(&request);
        state.selected[0] = request.questions[0].options.len();
        state.custom_answers[0] = "已有答案".to_string();
        state.answers[0] = vec!["已有答案".to_string()];
        state.toggle_current(&request).unwrap();
        assert!(state.answers[0].is_empty());

        request.questions[0].multiple = false;
        state.activate_current(&request).unwrap();
        assert!(state.editing);
    }

    #[test]
    fn option_rows_have_no_numbers_and_put_description_below_title() {
        let lines = option_lines("烧烤", "烤肉串、烤鸡翅、烤韭菜", true, false, false, 16);
        let visible = lines
            .iter()
            .map(|line| strip_ansi(line))
            .collect::<Vec<_>>();
        assert_eq!(visible[0], "› 烧烤");
        assert!(!visible.iter().any(|line| line.contains("1.")));
        assert!(visible[1..].iter().all(|line| line.starts_with("  ")));
        assert!(lines[1..].iter().all(|line| line.contains("\x1b[2m")));
    }

    #[test]
    fn multi_option_rows_keep_checkbox_without_number() {
        let lines = option_lines("代码", "修改实现和测试", true, true, true, 18);
        assert_eq!(strip_ansi(&lines[0]), "› [✓] 代码");
        assert!(strip_ansi(&lines[1]).starts_with("      "));
    }

    #[test]
    fn description_soft_wrap_preserves_indentation_budget() {
        let lines = option_lines("烧烤", "烤肉串烤鸡翅烤韭菜", false, false, false, 10);
        assert!(lines.len() > 2);
        for line in &lines[1..] {
            assert!(UnicodeWidthStr::width(strip_ansi(line).as_str()) <= 10);
            assert!(strip_ansi(line).starts_with("  "));
        }
    }

    #[test]
    fn resize_recovers_panel_height_after_terminal_grows() {
        let mut session = std::mem::ManuallyDrop::new(QuestionSession {
            stdout: io::stdout(),
            anchor_y: 8,
            panel_lines: 12,
        });
        session.resize_to_terminal(3);
        assert_eq!(session.panel_lines, 2);
        session.resize_to_terminal(24);
        assert_eq!(session.panel_lines, MAX_PANEL_LINES);
    }

    #[test]
    fn truncation_honors_very_narrow_widths() {
        assert_eq!(truncate_width("abcdef", 1), ".");
        assert_eq!(truncate_width("abcdef", 2), "..");
        assert_eq!(
            UnicodeWidthStr::width(truncate_width("中文测试", 3).as_str()),
            3
        );
    }

    #[test]
    fn selected_option_uses_color_without_bold() {
        let lines = option_lines("烧烤", "", true, false, false, 20);
        assert!(lines[0].contains("\x1b[35m"));
        assert!(!lines[0].contains("\x1b[1m"));
    }

    #[test]
    fn custom_editor_has_no_extra_ascii_pointer() {
        let line = editor_option_line(false, false, "自定义内容");
        assert_eq!(strip_ansi(&line), "› 自定义内容");
        assert!(!strip_ansi(&line).contains('>'));
    }

    #[test]
    fn ctrl_j_inserts_custom_answer_newline() {
        let request = QuestionRequest {
            questions: vec![QuestionPrompt {
                header: "说明".to_string(),
                question: "补充说明".to_string(),
                options: Vec::new(),
                multiple: false,
                custom: true,
            }],
        };
        let mut state = QuestionState::new(&request);
        state.editing = true;
        state.edit_buffer = "前".to_string();
        state.edit_cursor = 1;
        handle_editing_key(
            &request,
            &mut state,
            KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL),
        )
        .unwrap();
        assert_eq!(state.edit_buffer, "前\n");
    }

    #[test]
    fn scrolling_only_changes_body_window() {
        let first = panel_layout(3, 30, 1, 16, Some(0), 0);
        let last = panel_layout(3, 30, 1, 16, Some(29), first.body_start);
        assert_eq!(first.top_budget, 3);
        assert_eq!(last.top_budget, 3);
        assert_eq!(first.footer_start, 0);
        assert_eq!(last.footer_start, 0);
        assert_eq!(first.body_capacity, 12);
        assert_ne!(first.body_start, last.body_start);
    }

    #[test]
    fn scrolling_waits_until_focus_crosses_viewport_edge() {
        let inside = panel_layout(2, 12, 1, 8, Some(4), 0);
        assert_eq!(inside.body_capacity, 5);
        assert_eq!(inside.body_start, 0);

        let below = panel_layout(2, 12, 1, 8, Some(5), inside.body_start);
        assert_eq!(below.body_start, 1);

        let still_inside = panel_layout(2, 12, 1, 8, Some(4), below.body_start);
        assert_eq!(still_inside.body_start, 1);

        let above = panel_layout(2, 12, 1, 8, Some(0), still_inside.body_start);
        assert_eq!(above.body_start, 0);
    }
}
