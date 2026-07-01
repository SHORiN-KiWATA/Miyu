use anyhow::Result;
use crossterm::cursor::MoveToColumn;
use crossterm::execute;
use crossterm::terminal::{Clear, ClearType};
use std::io::{self, IsTerminal, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

const WIDTH: usize = 8;
const TRAIL_LEN: usize = 6;
const HOLD_END: usize = 9;
const HOLD_START: usize = 30;
const INTERVAL: Duration = Duration::from_millis(80);
const MIN_FADE_ALPHA: f64 = 0.12;
const ACTIVE_DOTS: [&str; TRAIL_LEN] = ["▪", "▪", "▫", "▫", "·", "·"];
const INACTIVE_DOT: &str = "·";

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
    start: Instant,
}

impl WaitSpinner {
    pub(crate) fn supported() -> bool {
        io::stdout().is_terminal()
    }

    pub(crate) fn start(phase: String) -> Self {
        let state = Arc::new(Mutex::new(WaitSpinnerState {
            phase,
            start: Instant::now(),
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

    pub(crate) fn stop(&mut self) -> Result<()> {
        self.running.store(false, Ordering::SeqCst);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
        clear_spinner_line()
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
        let line = match state.lock() {
            Ok(state) => render_frame(frame, &state),
            Err(_) => String::new(),
        };
        if !line.is_empty() {
            let _ = write_spinner_line(&line);
        }
        thread::sleep(INTERVAL);
        frame = (frame + 1) % total_frames();
    }
}

fn render_frame(frame: usize, state: &WaitSpinnerState) -> String {
    let scanner = scanner_state(frame % total_frames());
    let elapsed = state.start.elapsed();
    let elapsed = if elapsed > Duration::from_secs(1) {
        format!(" {:.1}s", elapsed.as_secs_f64())
    } else {
        String::new()
    };
    let cells = (0..WIDTH)
        .map(|char_index| render_cell(char_index, scanner))
        .collect::<String>();
    format!(
        "{} {}{}",
        cells,
        paint_secondary(&state.phase),
        paint_secondary(&elapsed)
    )
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

fn paint_inactive_dot(_fade: f64) -> String {
    format!("\x1b[2m\x1b[36m{INACTIVE_DOT}\x1b[0m")
}

fn total_frames() -> usize {
    WIDTH + HOLD_END + (WIDTH - 1) + HOLD_START
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

fn write_spinner_line(line: &str) -> Result<()> {
    let mut stdout = io::stdout();
    execute!(stdout, MoveToColumn(0), Clear(ClearType::CurrentLine))?;
    write!(stdout, "{line}")?;
    stdout.flush()?;
    Ok(())
}

fn clear_spinner_line() -> Result<()> {
    let mut stdout = io::stdout();
    execute!(stdout, MoveToColumn(0), Clear(ClearType::CurrentLine))?;
    stdout.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_frame_has_phase_without_face() {
        let state = WaitSpinnerState {
            phase: "思考".to_string(),
            start: Instant::now(),
        };

        let frame = render_frame(0, &state);

        assert!(frame.contains("思考"));
        assert!(!frame.contains('('));
    }

    #[test]
    fn frames_loop_over_pattern() {
        let state = WaitSpinnerState {
            phase: "thinking".to_string(),
            start: Instant::now(),
        };

        assert_eq!(
            render_frame(0, &state),
            render_frame(total_frames(), &state)
        );
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
}
