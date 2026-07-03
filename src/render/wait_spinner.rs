use anyhow::Result;
use crossterm::cursor::{MoveToColumn, MoveUp};
use crossterm::execute;
use crossterm::terminal::{Clear, ClearType};
use std::io::{self, IsTerminal, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

const WIDTH: usize = 7;
const TRAIL_LEN: usize = 6;
const HOLD_END: usize = 9;
const HOLD_START: usize = 30;
const INTERVAL: Duration = Duration::from_millis(80);
const MIN_FADE_ALPHA: f64 = 0.12;
const ACTIVE_DOTS: [&str; TRAIL_LEN] = ["▪", "▪", "▫", "▫", "·", "·"];
const INACTIVE_DOT: &str = "·";
const BRAILLE_FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

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
    state: Arc<Mutex<WaitSpinnerState>>,
    running: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

struct WaitSpinnerState {
    phase: String,
    sub_phase: Option<String>,
    start: Instant,
    lines_rendered: u16,
    style: SpinnerStyle,
}

impl WaitSpinner {
    pub(crate) fn supported() -> bool {
        io::stdout().is_terminal()
    }

    pub(crate) fn start(phase: String, style: SpinnerStyle) -> Self {
        let state = Arc::new(Mutex::new(WaitSpinnerState {
            phase,
            sub_phase: None,
            start: Instant::now(),
            lines_rendered: 0,
            style,
        }));
        let running = Arc::new(AtomicBool::new(true));
        let thread_state = Arc::clone(&state);
        let thread_running = Arc::clone(&running);
        let handle = thread::spawn(move || run_spinner_loop(thread_state, thread_running));
        Self {
            state,
            running,
            handle: Some(handle),
        }
    }

    pub(crate) fn set_phase(&self, phase: String) {
        if let Ok(mut state) = self.state.lock() {
            state.phase = phase;
        }
    }

    pub(crate) fn set_sub_phase(&self, sub_phase: Option<String>) {
        if let Ok(mut state) = self.state.lock() {
            state.sub_phase = sub_phase;
        }
    }

    pub(crate) fn stop(&mut self) -> Result<()> {
        self.running.store(false, Ordering::SeqCst);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
        let lines = self
            .state
            .lock()
            .map(|state| state.lines_rendered)
            .unwrap_or(0);
        clear_spinner_lines(lines)
    }
}

impl Drop for WaitSpinner {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

fn run_spinner_loop(state: Arc<Mutex<WaitSpinnerState>>, running: Arc<AtomicBool>) {
    let mut frame = 0usize;
    while running.load(Ordering::SeqCst) {
        let (output, prev_lines, lines) = match state.lock() {
            Ok(mut guard) => {
                let prev = guard.lines_rendered;
                let (output, lines) = render_frame(frame, &guard);
                guard.lines_rendered = lines;
                (output, prev, lines)
            }
            Err(_) => (String::new(), 0, 0),
        };
        if !output.is_empty() {
            let _ = write_spinner_lines(&output, prev_lines, lines);
        }
        thread::sleep(INTERVAL);
        let total = total_frames_for_style(state.lock().map(|s| s.style).unwrap_or(SpinnerStyle::Scanner));
        frame = (frame + 1) % total.max(1);
    }
}

fn render_frame(frame: usize, state: &WaitSpinnerState) -> (String, u16) {
    let elapsed = state.start.elapsed();
    let elapsed = if elapsed > Duration::from_secs(1) {
        format!(" {:.1}s", elapsed.as_secs_f64())
    } else {
        String::new()
    };
    let spinner_prefix = match state.style {
        SpinnerStyle::Scanner => {
            let scanner = scanner_state(frame % total_frames_scanner());
            (0..WIDTH)
                .map(|char_index| render_cell(char_index, scanner))
                .collect::<String>()
        }
        SpinnerStyle::Braille => {
            paint_secondary(BRAILLE_FRAMES[frame % BRAILLE_FRAMES.len()])
        }
    };
    let main_line = format!(
        "{} {}{}",
        spinner_prefix,
        paint_secondary(&state.phase),
        paint_secondary(&elapsed)
    );
    match &state.sub_phase {
        Some(sub) if !sub.trim().is_empty() => {
            let sub_line = format!("  {}", paint_secondary(sub));
            (format!("{main_line}\n{sub_line}"), 2)
        }
        _ => (main_line, 1),
    }
}

fn render_cell(char_index: usize, state: ScannerState) -> String {
    let fade = fade_factor(state);
    match color_index(char_index, state) {
        Some(index) if index < TRAIL_LEN => paint_active_dot(index),
        _ => paint_inactive_dot(fade),
    }
}

fn paint_active_dot(index: usize) -> String {
    let dot = ACTIVE_DOTS[index.min(ACTIVE_DOTS.len() - 1)];
    match index {
        0 => format!("\x1b[36m{dot}\x1b[0m"),
        1 => format!("\x1b[36m{dot}\x1b[0m"),
        2 => format!("\x1b[2m\x1b[36m{dot}\x1b[0m"),
        3 => format!("\x1b[2m\x1b[36m{dot}\x1b[0m"),
        _ => format!("\x1b[2m\x1b[36m{dot}\x1b[0m"),
    }
}

fn paint_inactive_dot(fade: f64) -> String {
    if fade > 0.24 {
        format!("\x1b[2m\x1b[36m{INACTIVE_DOT}\x1b[0m")
    } else {
        format!("\x1b[2m\x1b[36m{INACTIVE_DOT}\x1b[0m")
    }
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

fn write_spinner_lines(output: &str, prev_lines: u16, lines: u16) -> Result<()> {
    let mut stdout = io::stdout();
    if prev_lines > 1 {
        for _ in 1..prev_lines {
            execute!(stdout, MoveUp(1))?;
        }
    }
    for (index, line) in output.lines().enumerate() {
        execute!(stdout, MoveToColumn(0), Clear(ClearType::CurrentLine))?;
        write!(stdout, "{line}")?;
        if index + 1 < output.lines().count() {
            write!(stdout, "\n")?;
        }
    }
    if prev_lines > lines {
        for _ in lines..prev_lines {
            execute!(stdout, MoveUp(1))?;
            execute!(stdout, MoveToColumn(0), Clear(ClearType::CurrentLine))?;
        }
    }
    stdout.flush()?;
    Ok(())
}

fn clear_spinner_lines(lines: u16) -> Result<()> {
    let mut stdout = io::stdout();
    for i in 0..lines {
        if i > 0 {
            execute!(stdout, MoveUp(1))?;
        }
        execute!(stdout, MoveToColumn(0), Clear(ClearType::CurrentLine))?;
    }
    stdout.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_state(phase: &str, sub_phase: Option<&str>, style: SpinnerStyle) -> WaitSpinnerState {
        WaitSpinnerState {
            phase: phase.to_string(),
            sub_phase: sub_phase.map(|s| s.to_string()),
            start: Instant::now(),
            lines_rendered: 0,
            style,
        }
    }

    #[test]
    fn render_frame_scanner_has_phase_without_face() {
        let state = make_state("思考", None, SpinnerStyle::Scanner);

        let (frame, lines) = render_frame(0, &state);

        assert!(frame.contains("思考"));
        assert!(!frame.contains('('));
        assert_eq!(lines, 1);
    }

    #[test]
    fn render_frame_braille_has_phase() {
        let state = make_state("工具: 输入法诊断×1 运行中", None, SpinnerStyle::Braille);

        let (frame, lines) = render_frame(0, &state);

        assert!(frame.contains("输入法诊断"));
        assert!(frame.contains("⠋"));
        assert_eq!(lines, 1);
    }

    #[test]
    fn render_frame_with_sub_phase_produces_two_lines() {
        let state = make_state(
            "工具: 输入法诊断×1 运行中",
            Some("第 1 轮：诊断中"),
            SpinnerStyle::Scanner,
        );

        let (frame, lines) = render_frame(0, &state);

        assert!(frame.contains("输入法诊断"));
        assert!(frame.contains("第 1 轮"));
        assert_eq!(lines, 2);
    }

    #[test]
    fn braille_frames_loop_over_pattern() {
        let state = make_state("thinking", None, SpinnerStyle::Braille);

        let (f1, _) = render_frame(0, &state);
        let (f2, _) = render_frame(BRAILLE_FRAMES.len(), &state);

        assert_eq!(f1, f2);
    }

    #[test]
    fn scanner_frames_loop_over_pattern() {
        let state = make_state("thinking", None, SpinnerStyle::Scanner);

        let (f1, _) = render_frame(0, &state);
        let (f2, _) = render_frame(total_frames_scanner(), &state);

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
        assert!(paint_inactive_dot(1.0).contains(INACTIVE_DOT));
    }

    #[test]
    fn braille_cycles_through_all_frames() {
        let state = make_state("test", None, SpinnerStyle::Braille);

        let chars: std::collections::HashSet<&str> = (0..BRAILLE_FRAMES.len())
            .map(|i| {
                let (frame, _) = render_frame(i, &state);
                let first_char = frame.split_whitespace().next().unwrap_or("");
                BRAILLE_FRAMES.iter().find(|&&b| first_char.contains(b)).copied().unwrap_or("")
            })
            .collect();

        assert_eq!(chars.len(), BRAILLE_FRAMES.len());
    }
}
