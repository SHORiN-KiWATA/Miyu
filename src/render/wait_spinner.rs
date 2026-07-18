use super::clip_to_display_width;
use anyhow::Result;
use crossterm::cursor::{MoveDown, MoveToColumn, MoveUp};
use crossterm::execute;
use crossterm::terminal::{self, Clear, ClearType};
use std::io::{self, IsTerminal, Write};
use std::time::Duration;

const WIDTH: usize = 7;
const TRAIL_LEN: usize = 6;
const HOLD_END: usize = 9;
const HOLD_START: usize = 30;
pub(crate) const SPINNER_INTERVAL: Duration = Duration::from_millis(42);
const MIN_FADE_ALPHA: f64 = 0.12;
const ACTIVE_DOTS: [&str; TRAIL_LEN] = ["▪", "▪", "▫", "▫", "·", "·"];
const INACTIVE_DOT: &str = "·";
const BRAILLE_FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

pub(crate) fn braille_frame(frame: usize) -> &'static str {
    BRAILLE_FRAMES[frame % BRAILLE_FRAMES.len()]
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum SpinnerStyle {
    Scanner,
    Braille,
}

#[derive(Clone, Copy)]
struct ScannerState {
    active_position: usize,
    is_holding: bool,
    hold_progress: usize,
    hold_total: usize,
    movement_progress: usize,
    movement_total: usize,
    is_moving_forward: bool,
}

pub(crate) struct WaitSpinner {
    phase: String,
    sub_phase: Option<String>,
    rendered_line_widths: Vec<usize>,
    style: SpinnerStyle,
    frame: usize,
}

impl WaitSpinner {
    pub(crate) fn supported() -> bool {
        io::stdout().is_terminal()
    }

    pub(crate) fn start(phase: String, style: SpinnerStyle) -> Self {
        Self {
            phase,
            sub_phase: None,
            rendered_line_widths: Vec::new(),
            style,
            frame: 0,
        }
    }

    pub(crate) fn set_phase(&mut self, phase: String) {
        self.phase = phase;
    }

    pub(crate) fn set_sub_phase(&mut self, sub_phase: Option<String>) {
        self.sub_phase = sub_phase;
    }

    pub(crate) fn tick(&mut self) -> Result<()> {
        let terminal_width = terminal::size()
            .map(|(width, _)| usize::from(width))
            .unwrap_or(120);
        let (output, _) = render_frame_at_width(self.frame, self, terminal_width);
        if !output.is_empty() {
            let widths = output
                .lines()
                .map(super::command_ansi_width)
                .collect::<Vec<_>>();
            write_spinner_lines(&output, &self.rendered_line_widths, terminal_width)?;
            self.rendered_line_widths = widths;
        }
        let total = total_frames_for_style(self.style);
        self.frame = (self.frame + 1) % total.max(1);
        Ok(())
    }

    pub(crate) fn stop(&mut self) -> Result<()> {
        clear_spinner_lines(&self.rendered_line_widths)?;
        self.rendered_line_widths.clear();
        Ok(())
    }
}

#[cfg(test)]
fn render_frame(frame: usize, state: &WaitSpinner) -> (String, u16) {
    let width = terminal::size()
        .map(|(width, _)| usize::from(width))
        .unwrap_or(120);
    render_frame_at_width(frame, state, width)
}

fn render_frame_at_width(
    frame: usize,
    state: &WaitSpinner,
    terminal_width: usize,
) -> (String, u16) {
    let (spinner_prefix, spinner_width) = match state.style {
        SpinnerStyle::Scanner => {
            let scanner = scanner_state(frame % total_frames_scanner());
            (
                (0..WIDTH)
                    .map(|char_index| render_cell(char_index, scanner))
                    .collect::<String>(),
                WIDTH,
            )
        }
        SpinnerStyle::Braille => (paint_secondary(braille_frame(frame)), 1),
    };
    let usable = terminal_width.saturating_sub(1).max(1);
    let phase_width = usable.saturating_sub(spinner_width + 1);
    let phase = clip_to_display_width(&state.phase, phase_width);
    let main_line = if phase.is_empty() {
        spinner_prefix
    } else {
        format!(
            "{} {}",
            spinner_prefix,
            paint_for_style(&phase, state.style)
        )
    };
    let mut lines = vec![main_line];
    match &state.sub_phase {
        Some(sub) if !sub.trim().is_empty() => {
            for line in sub.lines().filter(|line| !line.trim().is_empty()) {
                let line = clip_to_display_width(line, usable.saturating_sub(2));
                lines.push(format!("  {}", paint_for_style(&line, state.style)));
            }
        }
        _ => {}
    }
    let count = lines.len().min(u16::MAX as usize) as u16;
    (lines.join("\n"), count)
}

fn render_cell(char_index: usize, state: ScannerState) -> String {
    match color_index(char_index, state) {
        Some(index) if index < TRAIL_LEN => paint_active_dot(index),
        _ => paint_inactive_dot(),
    }
}

fn paint_active_dot(index: usize) -> String {
    let dot = ACTIVE_DOTS[index.min(ACTIVE_DOTS.len() - 1)];
    match index {
        0 => format!("\x1b[38;5;10m{dot}\x1b[0m"),
        1 => format!("\x1b[38;5;10m{dot}\x1b[0m"),
        2 => format!("\x1b[2m\x1b[38;5;10m{dot}\x1b[0m"),
        3 => format!("\x1b[2m\x1b[38;5;10m{dot}\x1b[0m"),
        _ => format!("\x1b[2m\x1b[38;5;10m{dot}\x1b[0m"),
    }
}

fn paint_inactive_dot() -> String {
    format!("\x1b[2m\x1b[38;5;10m{INACTIVE_DOT}\x1b[0m")
}

fn total_frames_scanner() -> usize {
    WIDTH + HOLD_END + (WIDTH - 1) + HOLD_START
}

fn total_frames_for_style(style: SpinnerStyle) -> usize {
    match style {
        SpinnerStyle::Scanner => total_frames_scanner(),
        SpinnerStyle::Braille => BRAILLE_FRAMES.len(),
    }
}

fn scanner_state(mut frame: usize) -> ScannerState {
    if frame < WIDTH {
        return ScannerState {
            active_position: frame,
            is_holding: false,
            hold_progress: 0,
            hold_total: 0,
            movement_progress: frame,
            movement_total: WIDTH,
            is_moving_forward: true,
        };
    }
    frame -= WIDTH;
    if frame < HOLD_END {
        return ScannerState {
            active_position: WIDTH - 1,
            is_holding: true,
            hold_progress: frame,
            hold_total: HOLD_END,
            movement_progress: 0,
            movement_total: 0,
            is_moving_forward: true,
        };
    }
    frame -= HOLD_END;
    if frame < WIDTH - 1 {
        return ScannerState {
            active_position: WIDTH - 2 - frame,
            is_holding: false,
            hold_progress: 0,
            hold_total: 0,
            movement_progress: frame,
            movement_total: WIDTH - 1,
            is_moving_forward: false,
        };
    }
    frame -= WIDTH - 1;
    ScannerState {
        active_position: 0,
        is_holding: true,
        hold_progress: frame,
        hold_total: HOLD_START,
        movement_progress: 0,
        movement_total: 0,
        is_moving_forward: false,
    }
}

fn color_index(char_index: usize, state: ScannerState) -> Option<usize> {
    let distance = if state.is_moving_forward {
        state.active_position as isize - char_index as isize
    } else {
        char_index as isize - state.active_position as isize
    };
    if state.is_holding {
        return usize::try_from(distance)
            .ok()
            .map(|distance| distance + state.hold_progress);
    }
    if distance == 0 {
        return Some(0);
    }
    if distance > 0 && distance < TRAIL_LEN as isize {
        return usize::try_from(distance).ok();
    }
    None
}

#[allow(dead_code)]
fn fade_factor(state: ScannerState) -> f64 {
    if state.is_holding && state.hold_total > 0 {
        let progress = (state.hold_progress as f64 / state.hold_total as f64).min(1.0);
        (1.0 - progress * (1.0 - MIN_FADE_ALPHA)).max(MIN_FADE_ALPHA)
    } else if !state.is_holding && state.movement_total > 0 {
        let denominator = state.movement_total.saturating_sub(1).max(1);
        let progress = (state.movement_progress as f64 / denominator as f64).min(1.0);
        MIN_FADE_ALPHA + progress * (1.0 - MIN_FADE_ALPHA)
    } else {
        1.0
    }
}

fn paint_secondary(text: &str) -> String {
    format!("\x1b[2m\x1b[36m{text}\x1b[0m")
}

fn paint_for_style(text: &str, style: SpinnerStyle) -> String {
    match style {
        SpinnerStyle::Scanner => format!("\x1b[38;5;10m{text}\x1b[0m"),
        SpinnerStyle::Braille => paint_secondary(text),
    }
}

fn write_spinner_lines(
    output: &str,
    previous_widths: &[usize],
    terminal_width: usize,
) -> Result<()> {
    let mut stdout = io::stdout();
    if !previous_widths.is_empty() {
        clear_spinner_lines_with_writer(&mut stdout, previous_widths, terminal_width)?;
    }
    let output_lines = output.lines().collect::<Vec<_>>();
    for (index, line) in output_lines.iter().enumerate() {
        execute!(stdout, MoveToColumn(0), Clear(ClearType::CurrentLine))?;
        write!(stdout, "{line}")?;
        if index + 1 < output_lines.len() {
            write!(stdout, "\n")?;
        }
    }
    stdout.flush()?;
    Ok(())
}

fn clear_spinner_lines(widths: &[usize]) -> Result<()> {
    if widths.is_empty() {
        return Ok(());
    }
    let mut stdout = io::stdout();
    let terminal_width = terminal::size()
        .map(|(width, _)| usize::from(width))
        .unwrap_or(120);
    clear_spinner_lines_with_writer(&mut stdout, widths, terminal_width)?;
    stdout.flush()?;
    Ok(())
}

fn clear_spinner_lines_with_writer(
    stdout: &mut impl Write,
    widths: &[usize],
    terminal_width: usize,
) -> Result<()> {
    if widths.is_empty() {
        return Ok(());
    }
    let columns = terminal_width.max(1);
    let rows = widths
        .iter()
        .map(|width| (*width).max(1).div_ceil(columns))
        .sum::<usize>()
        .min(u16::MAX as usize) as u16;
    if rows > 1 {
        execute!(stdout, MoveUp(rows - 1))?;
    }
    for index in 0..rows {
        execute!(stdout, MoveToColumn(0), Clear(ClearType::CurrentLine))?;
        if index + 1 < rows {
            execute!(stdout, MoveDown(1))?;
        }
    }
    if rows > 1 {
        execute!(stdout, MoveUp(rows - 1))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_spinner(phase: &str, sub_phase: Option<&str>, style: SpinnerStyle) -> WaitSpinner {
        WaitSpinner {
            phase: phase.to_string(),
            sub_phase: sub_phase.map(|s| s.to_string()),
            rendered_line_widths: Vec::new(),
            style,
            frame: 0,
        }
    }

    #[test]
    fn render_frame_scanner_has_phase_without_face() {
        let spinner = make_spinner("思考", None, SpinnerStyle::Scanner);

        let (frame, lines) = render_frame(0, &spinner);

        assert!(frame.contains("思考"));
        assert!(frame.contains("\x1b[38;5;10m"));
        assert!(!frame.contains("\x1b[36m思考"));
        assert!(!frame.contains('('));
        assert_eq!(lines, 1);
    }

    #[test]
    fn render_frame_scanner_without_phase_has_no_separator() {
        let spinner = make_spinner("", None, SpinnerStyle::Scanner);

        let (frame, lines) = render_frame(0, &spinner);

        assert_eq!(crate::render::command_ansi_width(&frame), WIDTH);
        assert_eq!(lines, 1);
    }

    #[test]
    fn render_frame_braille_has_phase() {
        let spinner = make_spinner("~ 输入法诊断×1 运行中", None, SpinnerStyle::Braille);

        let (frame, lines) = render_frame(0, &spinner);

        assert!(frame.contains("输入法诊断"));
        assert!(frame.contains("⠋"));
        assert!(frame.contains("\x1b[2m\x1b[36m"));
        assert_eq!(lines, 1);
    }

    #[test]
    fn render_frame_with_sub_phase_produces_two_lines() {
        let spinner = make_spinner(
            "~ 输入法诊断×1 运行中",
            Some("第 1 轮：诊断中"),
            SpinnerStyle::Scanner,
        );

        let (frame, lines) = render_frame(0, &spinner);

        assert!(frame.contains("输入法诊断"));
        assert!(frame.contains("第 1 轮"));
        assert_eq!(lines, 2);
    }

    #[test]
    fn long_unicode_phase_never_soft_wraps() {
        let spinner = make_spinner(
            &format!("思考：{}", "中文".repeat(40)),
            Some(&format!("↳ {}", "👨‍👩‍👧‍👦测试".repeat(30))),
            SpinnerStyle::Scanner,
        );
        for width in [20, 40, 80] {
            let (frame, lines) = render_frame_at_width(3, &spinner, width);
            assert_eq!(lines, 2);
            for line in frame.lines() {
                assert!(
                    crate::render::command_ansi_width(line) < width,
                    "line exceeded {width} columns: {line:?}"
                );
            }
        }
    }

    #[test]
    fn clearing_multiline_spinner_returns_cursor_to_block_top() {
        let mut output = Vec::new();
        clear_spinner_lines_with_writer(&mut output, &[20, 20], 80).unwrap();
        let output = String::from_utf8(output).unwrap();

        assert!(output.starts_with("\x1b[1A"));
        assert!(output.contains("\x1b[1B"));
        assert!(output.ends_with("\x1b[1A"));
    }

    #[test]
    fn clearing_spinner_counts_soft_wrapped_physical_rows() {
        let mut output = Vec::new();
        clear_spinner_lines_with_writer(&mut output, &[100, 20], 40).unwrap();
        let output = String::from_utf8(output).unwrap();

        assert!(output.starts_with("\x1b[3A"));
        assert_eq!(output.matches("\x1b[1B").count(), 3);
        assert!(output.ends_with("\x1b[3A"));
    }

    #[test]
    fn multiline_sub_phase_reports_every_rendered_row() {
        let spinner = make_spinner(
            "~ 子代理×1 运行中 · 4s",
            Some("↳ 查询磁盘占用\n↳ 工具 #2：运行命令 运行中"),
            SpinnerStyle::Braille,
        );

        let (frame, lines) = render_frame_at_width(0, &spinner, 80);

        assert_eq!(lines, 3);
        assert_eq!(frame.lines().count(), 3);
        assert_eq!(frame.matches("子代理×1").count(), 1);
        assert_eq!(frame.matches("4s").count(), 1);
    }

    #[test]
    fn braille_frames_loop_over_pattern() {
        let spinner = make_spinner("thinking", None, SpinnerStyle::Braille);

        let (f1, _) = render_frame(0, &spinner);
        let (f2, _) = render_frame(BRAILLE_FRAMES.len(), &spinner);

        assert_eq!(f1, f2);
    }

    #[test]
    fn scanner_frames_loop_over_pattern() {
        let spinner = make_spinner("thinking", None, SpinnerStyle::Scanner);

        let (f1, _) = render_frame(0, &spinner);
        let (f2, _) = render_frame(total_frames_scanner(), &spinner);

        assert_eq!(f1, f2);
    }

    #[test]
    fn scanner_has_trail_behind_active_position() {
        let state = scanner_state(4);

        assert_eq!(color_index(4, state), Some(0));
        assert_eq!(color_index(3, state), Some(1));
        assert_eq!(color_index(7, state), None);
    }

    #[test]
    fn active_and_inactive_dots_match_pr_style() {
        assert!(render_cell(4, scanner_state(4)).contains("▪"));
        assert!(paint_inactive_dot().contains(INACTIVE_DOT));
    }

    #[test]
    fn braille_cycles_through_all_frames() {
        let spinner = make_spinner("test", None, SpinnerStyle::Braille);

        let chars: std::collections::HashSet<&str> = (0..BRAILLE_FRAMES.len())
            .map(|i| {
                let (frame, _) = render_frame(i, &spinner);
                let first_char = frame.split_whitespace().next().unwrap_or("");
                BRAILLE_FRAMES
                    .iter()
                    .find(|&&b| first_char.contains(b))
                    .copied()
                    .unwrap_or("")
            })
            .collect();

        assert_eq!(chars.len(), BRAILLE_FRAMES.len());
    }
}
