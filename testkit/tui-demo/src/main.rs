//! ratatui 全屏 TUI 手感演示——**沿用 Miyu 现有 REPL 的视觉语言**。
//!
//! 复刻自 src/render/style.rs、src/cli/footer.rs、src/cli/repl/layout.rs、
//! src/cli/inline_picker.rs、src/render/wait_spinner.rs、src/render/stream/*、
//! src/terminal/kitty.rs：蓝色粗体 `┃` 竖条包住用户消息与输入区，助手正文平铺无装饰，
//! 思考摘要亮绿，工具块 dim，扫描点 spinner，footer 声波，选择器 `› [*]`，
//! kitty Unicode 占位符图片。全屏模型只是让「分页 / 悬浮 / 自绘选择」成为可能，画面本身不变。
//!
//! v4：
//!   - 输入 `/` 时命令菜单以**列表**锚在输入框上方盖住正文，↑↓ 选、Tab 补全、Enter 执行、Esc 清空
//!   - 程序自己接管鼠标拖选：高亮不含左侧 `┃` 装饰，松开即通过 OSC 52 写剪贴板（不带装饰）
//!     滚轮滚历史；按住 Shift 拖仍是终端原生选择；F5 可关掉捕获对比
//!   - 排队气泡后补上与输入区之间的空行（对应 resume_at 的 queue_gap）
//!   - 启动时重放历史（对应真 REPL 恢复会话时的回放）
//!   - 底部留一行空、右侧 dim 滚动条、kitty 图片、空闲不重画
use std::io::{self, Write};
use std::time::{Duration, Instant};

use base64::Engine;
use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind, KeyModifiers,
    MouseButton, MouseEventKind,
};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use ratatui::{Frame, Terminal};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

// ───────────────────────── 调色板（对应 render/style.rs） ─────────────────────────

const BOLD_BLUE: Style = Style::new().fg(Color::Blue).add_modifier(Modifier::BOLD);
const BOLD_MAGENTA: Style = Style::new().fg(Color::Magenta).add_modifier(Modifier::BOLD);
const MAGENTA: Style = Style::new().fg(Color::Magenta);
const CYAN: Style = Style::new().fg(Color::Cyan);
const DIM: Style = Style::new().add_modifier(Modifier::DIM);
const DIM_CYAN: Style = Style::new().fg(Color::Cyan).add_modifier(Modifier::DIM);
const GREEN10: Style = Style::new().fg(Color::Indexed(10));
const URL: Style = Style::new().fg(Color::Indexed(75)).add_modifier(Modifier::DIM);
const IMAGE_LABEL: Style = Style::new().fg(Color::Indexed(183));
const BOLD: Style = Style::new().add_modifier(Modifier::BOLD);
const BOLD_GREEN: Style = Style::new().fg(Color::Green).add_modifier(Modifier::BOLD);
const CODE_KW: Style = Style::new().fg(Color::Rgb(196, 167, 231));
const CODE_FN: Style = Style::new().fg(Color::Rgb(156, 207, 216));
const CODE_STR: Style = Style::new().fg(Color::Rgb(166, 214, 160));
const CODE_NUM: Style = Style::new().fg(Color::Rgb(246, 193, 119));
const CODE_COMMENT: Style = Style::new().fg(Color::Green);

// ───────────────────────── 数据 ─────────────────────────

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    Normal,
    Dev,
}

impl Mode {
    fn bar(self) -> Style {
        match self {
            Mode::Normal => BOLD_BLUE,
            Mode::Dev => BOLD_MAGENTA,
        }
    }
    fn label(self) -> &'static str {
        match self {
            Mode::Normal => "普通",
            Mode::Dev => "开发",
        }
    }
}

struct ImageEntry {
    id: u32,
    name: &'static str,
    px: (u32, u32),
    cols: u16,
    rows: u16,
    transmitted: bool,
}

enum Entry {
    User { text: String, mode: Mode },
    Reasoning { summary: String, body: String, expanded: bool },
    ToolBlock { header: Vec<String>, body: Vec<String>, expanded: bool },
    Assistant(String),
    Note(String),
    Image(ImageEntry),
}

#[derive(Clone, Copy, PartialEq)]
enum Phase {
    Reasoning,
    Tool,
    Text,
}

struct Turn {
    phase: Phase,
    phase_started: Instant,
    full: String,
    done: usize,
}

#[derive(Clone, Copy, PartialEq)]
enum PickerKind {
    Models,
    Sessions,
}

#[derive(Clone)]
struct Picker {
    kind: PickerKind,
    query: String,
    page: usize,
    selected: usize,
}

enum Overlay {
    None,
    Picker(Picker),
    Help { scroll: u16 },
    Job(usize),
}

#[derive(Clone, Copy, PartialEq)]
enum JobKind {
    Cmd,
    Agent,
}

/// 后台任务（命令 / 子代理），演示里内容按运行秒数伪造
struct Job {
    kind: JobKind,
    id: &'static str,
    title: &'static str,
    started: Instant,
}

impl Job {
    fn kind_label(&self) -> &'static str {
        match self.kind {
            JobKind::Cmd => "命令",
            JobKind::Agent => "子代理",
        }
    }
    /// 详情面板正文：沿用工具块的词汇（`$ 命令` / `  ↳ 补充` / `  │ 输出` / `  ✓ 完成`），
    /// 一律缩进到输入框文字列；说明文字不进正文
    fn detail_lines(&self, frame: usize) -> Vec<Line<'static>> {
        let secs = self.started.elapsed().as_secs() as usize;
        let sp = BRAILLE[frame % 10];
        match self.kind {
            JobKind::Cmd => {
                let all = [
                    "Compiling proc-macro2 v1.0.86",
                    "Compiling unicode-ident v1.0.12",
                    "Compiling libc v0.2.155",
                    "Compiling serde v1.0.204",
                    "Compiling tokio v1.38.0",
                    "Compiling ratatui v0.29.0",
                    "Compiling crossterm v0.28.1",
                    "Compiling miyu v0.5.0 (/home/shorin/Documents/github/Miyu)",
                    "warning: unused variable: `rows` --> src/cli/repl/layout.rs:212",
                    "Compiling miyu (bin) ...",
                ];
                let n = (secs / 2 + 1).min(all.len());
                let mut lines = vec![
                    Line::from(vec![Span::styled("  $ ", DIM), Span::raw("cargo build --release")]),
                    Line::from(Span::styled("    ↳ ~/Documents/github/Miyu", DIM)),
                ];
                for l in &all[n.saturating_sub(6)..n] {
                    lines.push(Line::from(vec![Span::styled("    │ ", DIM), Span::styled(l.to_string(), DIM)]));
                }
                lines
            }
            JobKind::Agent => {
                let steps: [(&str, &str); 5] = [
                    ("先看 ratatui 的 Buffer 结构", "读文件 · ratatui-0.29.0/src/buffer/buffer.rs"),
                    ("Cell 里 symbol 是 CompactString，24 字节", "搜索 · \"CompactString\" \"struct Cell\""),
                    ("双缓冲 = cols×rows×2 个 Cell", "运行命令 · cargo tree -p ratatui"),
                    ("200×60 约 1.3MB，和实测 2.1MB 对得上", "写文件 · report.md"),
                    ("整理成表格", "读文件 · report.md"),
                ];
                let n = (secs / 3).min(steps.len() - 1);
                let mut lines = vec![Line::from(vec![Span::styled("  ↳ ", DIM), Span::raw("调研 ratatui 的内存占用，给出表格")])];
                lines.push(Line::from(""));
                // 已完成的只留最近两步，当前一步带 spinner；思考摘要用绿色，和正文里一样
                for (r, t) in steps.iter().take(n).skip(n.saturating_sub(2)) {
                    lines.push(Line::from(vec![Span::styled("  ✓ ", DIM), Span::styled(t.to_string(), DIM)]));
                    let _ = r;
                }
                let (r, t) = steps[n];
                lines.push(Line::from(vec![Span::raw("  "), Span::styled(format!("思考：{r} · {}.{}s", secs % 3, (frame * 7) % 10), GREEN10)]));
                lines.push(Line::from(vec![Span::raw("  "), Span::styled(sp, DIM_CYAN), Span::raw(" "), Span::styled(t.to_string(), DIM_CYAN)]));
                lines
            }
        }
    }
}

/// 历史区的一行：画的内容 + 可复制的纯文本 + 纯文本从第几列开始（前面是装饰）
struct Row {
    line: Line<'static>,
    copy: String,
    copy_col: u16,
    /// 可点击目标（任务状态行 / 工具块头 / 菜单项）；悬停时这一行去 dim 提亮
    click: Option<Click>,
}

#[derive(Clone, Copy, PartialEq)]
enum Click {
    Job(usize),
    Block(usize),
    Menu(usize),
    PickerItem(usize),
}

/// 鼠标选区，绝对行号（含滚动）+ 列
#[derive(Clone, Copy)]
struct Selection {
    anchor: (usize, u16),
    cur: (usize, u16),
    dragging: bool,
}

struct App {
    mode: Mode,
    history: Vec<Entry>,
    turn: Option<Turn>,
    queue: Vec<String>,
    input: String,
    slash_sel: usize,
    scroll: usize,
    follow: bool,
    overlay: Overlay,
    frame: usize,
    models: Vec<&'static str>,
    model: usize,
    sessions: Vec<String>,
    jobs: Vec<Job>,
    session_tokens_k: f32,
    mouse_capture: bool,
    toggle_mouse: bool,
    next_image_id: u32,
    dirty: bool,
    selection: Option<Selection>,
    pending_copy: Option<String>,
    body: Rect,
    hist_len: usize,
    toast: Option<(String, Instant)>,
    hover: Option<(u16, u16)>,
    hits: Vec<(u16, Click)>, // 悬浮块里可点击的行（屏幕行 → 目标）
    cursor: usize, // 输入框光标（char 下标）
    pasted: Vec<String>, // 折成占位符的粘贴原文
    images: usize, // 已粘贴的图片数（演示只计数）
}

const MODELS: [&str; 8] = [
    "继承全局模型池",
    "Claude Code / fable",
    "Claude Code / opus",
    "Claude Code / sonnet",
    "Antigravity / gemini-3.8-flash-high",
    "Codex / gpt-5.6-luna",
    "opencode Zen / big-pickle",
    "taotoken / glm_for_coding",
];
const PAGE: usize = 8;
const SLASH: [(&str, &str); 13] = [
    ("/new", "创建新会话并切换过去"),
    ("/session", "列出会话，或切换到指定会话"),
    ("/models", "切换当前会话使用的模型"),
    ("/persona", "切换当前人格"),
    ("/usage", "显示 Token 用量详情"),
    ("/config", "打开配置界面"),
    ("/variant", "查看或切换思考档位"),
    ("/undo", "撤销上一轮或上下文压缩"),
    ("/compact", "立即压缩当前会话上下文"),
    ("/history", "显示最近的会话历史"),
    ("/img", "（演示）往正文里放一张 kitty 图片"),
    ("/help", "显示此帮助"),
    ("/exit", "退出 REPL"),
];

const DEMO_PNG: &[u8] = include_bytes!("../../../pics/hash.png");
const DEMO_PNG_NAME: &str = "hash.png";
const DEMO_PNG_PX: (u32, u32) = (1041, 283);

fn fake_reply(n: usize) -> String {
    format!(
        "先说结论：**悬浮窗和分页都是程序自己画的**，不依赖终端 scrollback；画面本身沿用现在的样子。这是第 {n} 轮。

# 这段演示里能看到的

- 用户消息还是 `┃` 竖条包着，助手正文还是平铺，底部留一行空
- 输入 `/` 出命令列表，锚在输入框上方，↑↓ 选、Tab 补全、Enter 执行
- 鼠标直接拖选：高亮和复制都不带左侧竖条，松开就进剪贴板
- `/session` `/models` 同样锚在上方；`/img` 放一张 kitty 图片进正文，随历史一起滚

```rust
fn anchored(tail: Rect, h: u16, w: u16) -> Rect {{
    // 菜单贴着活动区上沿，盖住下面的正文
    Rect {{ x: 0, y: tail.y.saturating_sub(h), width: w, height: h }}
}}
```

参考 https://ratatui.rs/ 的 Clear 组件。回翻历史时新内容继续流入不会把你拽回底部，按 End 才回底。\
这一段故意写长一点，好让一屏放不下。The quick brown fox jumps over the lazy dog. 再来一遍。\
The quick brown fox jumps over the lazy dog."
    )
}

fn is_kitty() -> bool {
    std::env::var("KITTY_WINDOW_ID").is_ok()
        || std::env::var("TERM").map(|t| t.contains("kitty")).unwrap_or(false)
}

impl App {
    fn new() -> Self {
        let mut app = Self {
            mode: Mode::Normal,
            history: Vec::new(),
            turn: None,
            queue: Vec::new(),
            input: String::new(),
            slash_sel: 0,
            scroll: 0,
            follow: true,
            overlay: Overlay::None,
            frame: 0,
            models: MODELS.to_vec(),
            model: 2,
            sessions: (1..=27)
                .map(|i| {
                    let mode = if i % 5 == 0 { "开发" } else { "普通" };
                    let name = ["聊天", "写代码", "记账", "翻译", "折腾 Arch"][i % 5];
                    let snippet = ["发一个链接", "帮我看看这个报错", "午饭 32", "把这段翻成日语", "kernel 7.2 zen"][i % 5];
                    format!("{mode}：{name} {i:02} · {snippet}")
                })
                .collect(),
            jobs: Vec::new(),
            session_tokens_k: 21.2,
            mouse_capture: true,
            toggle_mouse: false,
            next_image_id: 0x4d69,
            dirty: true,
            selection: None,
            pending_copy: None,
            body: Rect::default(),
            hist_len: 0,
            toast: None,
            hover: None,
            hits: Vec::new(),
            cursor: 0,
            pasted: Vec::new(),
            images: 0,
        };
        // 启动时重放历史（真 REPL 恢复会话时的回放；内容照 09-10 真机快照）
        for (u, a) in [
            ("发一个链接", "https://www.bilibili.com/video/BV1GJ411x7h7"),
            ("呃呃，链接后面跟着点正文", "要哪方面的，说了我才好配正文"),
            ("随意", "那就说说折腾的事吧 https://archlinux.org/ Arch 最近又是一波更新，kernel 7.2 的 zen 分支已经进来了，N 卡那边 nvidia-open 也跟着适配了。你要是还挂着 580xx-dkms 的话可以准备切回去了。"),
        ] {
            app.history.push(Entry::User { text: u.into(), mode: Mode::Normal });
            app.history.push(Entry::Assistant(a.into()));
        }
        app.history.push(Entry::Note("以上是重放的历史。输入 `/` 看命令列表；直接用鼠标拖选正文试试复制；`/img` 放图；F4 起后台任务。".into()));
        app
    }

    fn assistant_count(&self) -> usize {
        self.history.iter().filter(|e| matches!(e, Entry::Assistant(_))).count()
    }

    fn push_image(&mut self) {
        let (cell_w, cell_h) = cell_pixels();
        let (max_cols, max_rows) = (60u64, 10u64);
        let (iw, ih) = (DEMO_PNG_PX.0 as u64, DEMO_PNG_PX.1 as u64);
        let (cols, rows) = if iw * max_rows * cell_h >= ih * max_cols * cell_w {
            let rows = (ih * max_cols * cell_w).div_ceil(iw * cell_h);
            (max_cols as u16, rows.clamp(1, max_rows) as u16)
        } else {
            let cols = (iw * max_rows * cell_h).div_ceil(ih * cell_w);
            (cols.clamp(1, max_cols) as u16, max_rows as u16)
        };
        let id = self.next_image_id;
        self.next_image_id += 1;
        self.history.push(Entry::Image(ImageEntry { id, name: DEMO_PNG_NAME, px: DEMO_PNG_PX, cols, rows, transmitted: false }));
        self.follow = true;
    }

    fn slash_matches(&self) -> Vec<(&'static str, &'static str)> {
        if !self.input.starts_with('/') || self.input.contains(' ') {
            return Vec::new();
        }
        SLASH.iter().copied().filter(|(c, _)| c.starts_with(&self.input)).collect()
    }

    fn run_command(&mut self, text: &str) {
        let cmd = text.trim_start_matches('/').split_whitespace().next().unwrap_or("");
        match cmd {
            "models" => self.overlay = Overlay::Picker(Picker { kind: PickerKind::Models, query: String::new(), page: 0, selected: self.model }),
            "session" => self.overlay = Overlay::Picker(Picker { kind: PickerKind::Sessions, query: String::new(), page: 0, selected: 0 }),
            "help" => self.overlay = Overlay::Help { scroll: 0 },
            "img" => self.push_image(),
            "exit" => {}
            other => self.history.push(Entry::Note(format!("演示里没接 /{other}"))),
        }
        self.follow = true;
    }

    fn submit(&mut self) {
        let text = self.input.trim().to_string();
        if text.is_empty() {
            return;
        }
        self.input.clear();
        self.cursor = 0;
        self.slash_sel = 0;
        if text.starts_with('/') {
            self.run_command(&text);
            return;
        }
        if self.turn.is_some() {
            self.queue.push(text);
            return;
        }
        self.start_turn(text);
    }

    fn start_turn(&mut self, text: String) {
        self.history.push(Entry::User { text, mode: self.mode });
        let n = self.assistant_count() + 1;
        self.turn = Some(Turn { phase: Phase::Reasoning, phase_started: Instant::now(), full: fake_reply(n), done: 0 });
        self.follow = true;
    }

    /// 有没有需要逐帧重画的东西（回合 spinner / 任务条计时 / 通知）。没有就只在事件时画。
    fn animating(&self) -> bool {
        self.turn.is_some() || !self.jobs.is_empty() || self.toast.is_some()
    }

    fn tick(&mut self) {
        self.frame += 1;
        if self.toast.as_ref().is_some_and(|(_, t)| t.elapsed() > Duration::from_millis(2500)) {
            self.toast = None;
        }
        let Some(turn) = &mut self.turn else { return };
        let elapsed = turn.phase_started.elapsed();
        match turn.phase {
            Phase::Reasoning => {
                if elapsed > Duration::from_millis(1400) {
                    self.history.push(Entry::Reasoning {
                        summary: format!("思考：先确认要演示哪些窗口效果 · 312 词元 · {:.1}s", elapsed.as_secs_f64()),
                        body: "用户要看的是窗口效果本身，不是功能是否真的接上了。所以这一轮先把命令列表、选择器、任务详情这三类悬浮块各走一遍，\
再演示鼠标：拖选、悬停提亮、点击展开。\n\n正文内容不重要，但要够长，一屏放不下才能看出回翻和钉住视口的行为。\
代码块放一个 anchored 的例子就够，说明悬浮块只是 area 里的一块 Rect。\n\n输出前再核对一遍：竖条只包用户消息和输入区，\
输入框上方的东西一律不带竖条。".into(),
                        expanded: false,
                    });
                    turn.phase = Phase::Tool;
                    turn.phase_started = Instant::now();
                }
            }
            Phase::Tool => {
                if elapsed > Duration::from_millis(1100) {
                    self.history.push(Entry::ToolBlock {
                        header: vec![
                            "~ 读文件×1 ok · src/cli/repl/layout.rs".into(),
                            "  ✓ 457 行 · input_prompt_bar / submitted_echo_lines".into(),
                        ],
                        body: (95..=105)
                            .map(|n| format!("{n:>4}  {}", [
                                "pub(in crate::cli) fn submitted_echo_lines(",
                                "    mode: AgentMode,",
                                "    input: &str,",
                                "    cols: usize,",
                                ") -> Vec<String> {",
                                "    let max_text_width = cols.saturating_sub(3).max(1);",
                                "    let bar = submitted_echo_bar(mode);",
                                "    let mut output = Vec::new();",
                                "    output.push(bar.clone());",
                                "    for line in input.split('\\n') {",
                                "        let mut chunks = wrap_visible_width(line, max_text_width);",
                            ][n - 95]))
                            .collect(),
                        expanded: false,
                    });
                    self.history.push(Entry::Assistant(String::new()));
                    turn.phase = Phase::Text;
                    turn.phase_started = Instant::now();
                }
            }
            Phase::Text => {
                let mut rest = turn.full[turn.done..].chars();
                let mut advanced = 0;
                for _ in 0..3 {
                    if let Some(c) = rest.next() {
                        advanced += c.len_utf8();
                    }
                }
                turn.done += advanced;
                let shown = turn.full[..turn.done].to_string();
                if let Some(Entry::Assistant(t)) = self.history.last_mut() {
                    *t = shown;
                }
                if turn.done >= turn.full.len() {
                    self.turn = None;
                    self.session_tokens_k += 1.7;
                    if !self.queue.is_empty() {
                        let next = self.queue.remove(0);
                        self.start_turn(next);
                    }
                }
            }
        }
    }
}

// ───────────────────────── kitty 图片（对应 terminal/kitty.rs） ─────────────────────────

const PLACEHOLDER: char = '\u{10eeee}';
const ROW_DIACRITICS: &[char] = &[
    '\u{0305}', '\u{030d}', '\u{030e}', '\u{0310}', '\u{0312}', '\u{033d}', '\u{033e}', '\u{033f}',
    '\u{0346}', '\u{034a}', '\u{034b}', '\u{034c}', '\u{0350}', '\u{0351}', '\u{0352}', '\u{0357}',
    '\u{035b}', '\u{0363}', '\u{0364}', '\u{0365}', '\u{0366}', '\u{0367}', '\u{0368}', '\u{0369}',
    '\u{036a}', '\u{036b}', '\u{036c}', '\u{036d}', '\u{036e}', '\u{036f}', '\u{0483}', '\u{0484}',
    '\u{0485}', '\u{0486}', '\u{0487}', '\u{0592}', '\u{0593}', '\u{0594}', '\u{0595}', '\u{0597}',
    '\u{0598}', '\u{0599}', '\u{059c}', '\u{059d}', '\u{059e}', '\u{059f}', '\u{05a0}', '\u{05a1}',
    '\u{05a8}', '\u{05a9}', '\u{05ab}', '\u{05ac}', '\u{05af}', '\u{05c4}', '\u{0610}', '\u{0611}',
    '\u{0612}', '\u{0613}', '\u{0614}', '\u{0615}', '\u{0616}', '\u{0617}', '\u{0657}', '\u{0658}',
];

fn cell_pixels() -> (u64, u64) {
    match crossterm::terminal::window_size() {
        Ok(ws) if ws.width > 0 && ws.height > 0 && ws.columns > 0 && ws.rows > 0 => {
            ((ws.width / ws.columns).max(1) as u64, (ws.height / ws.rows).max(1) as u64)
        }
        _ => (10, 20),
    }
}

fn transmit_png(out: &mut impl Write, img: &ImageEntry) -> io::Result<()> {
    let encoded = base64::engine::general_purpose::STANDARD.encode(DEMO_PNG);
    let chunks: Vec<&[u8]> = encoded.as_bytes().chunks(4096).collect();
    for (i, chunk) in chunks.iter().enumerate() {
        write!(out, "\x1b_Gq=2,")?;
        if i == 0 {
            write!(out, "i={},a=T,U=1,f=100,t=d,c={},r={},", img.id, img.cols, img.rows)?;
        }
        write!(out, "m={};", u8::from(i + 1 < chunks.len()))?;
        out.write_all(chunk)?;
        write!(out, "\x1b\\")?;
    }
    out.flush()
}

fn image_rows(img: &ImageEntry) -> Vec<Row> {
    if !is_kitty() {
        let label = format!("[图片 {} {}×{}]", img.name, img.px.0, img.px.1);
        return vec![Row { line: Line::from(Span::styled(label.clone(), IMAGE_LABEL)), copy: label, copy_col: 0, click: None }];
    }
    let [_, r, g, b] = img.id.to_be_bytes();
    let fg = Style::new().fg(Color::Rgb(r, g, b));
    (0..img.rows)
        .map(|row| {
            let mut s = String::new();
            for col in 0..img.cols {
                s.push(PLACEHOLDER);
                s.push(ROW_DIACRITICS[row as usize]);
                s.push(ROW_DIACRITICS[col as usize]);
            }
            // 图片格子不可复制
            Row { line: Line::from(Span::styled(s, fg)), copy: String::new(), copy_col: u16::MAX, click: None }
        })
        .collect()
}

// ───────────────────────── 文本工具 ─────────────────────────

fn wrap_chars(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut rows = Vec::new();
    let mut cur = String::new();
    let mut w = 0;
    for ch in text.chars() {
        let cw = ch.width().unwrap_or(0);
        if w > 0 && w + cw > width {
            rows.push(std::mem::take(&mut cur));
            w = 0;
        }
        cur.push(ch);
        w += cw;
    }
    rows.push(cur);
    rows
}

/// 把一串 span 按显示宽度折成多行（样式跟着字符走）
fn wrap_spans(spans: Vec<Span<'static>>, width: usize) -> Vec<Vec<Span<'static>>> {
    let width = width.max(1);
    let mut rows: Vec<Vec<Span<'static>>> = vec![Vec::new()];
    let mut w = 0;
    for span in spans {
        let style = span.style;
        let mut cur = String::new();
        for ch in span.content.chars() {
            let cw = ch.width().unwrap_or(0);
            if w > 0 && w + cw > width {
                if !cur.is_empty() {
                    rows.last_mut().unwrap().push(Span::styled(std::mem::take(&mut cur), style));
                }
                rows.push(Vec::new());
                w = 0;
            }
            cur.push(ch);
            w += cw;
        }
        if !cur.is_empty() {
            rows.last_mut().unwrap().push(Span::styled(cur, style));
        }
    }
    rows
}

fn truncate(text: &str, width: usize) -> String {
    let mut out = String::new();
    let mut w = 0;
    for ch in text.chars() {
        let cw = ch.width().unwrap_or(0);
        if w + cw > width {
            break;
        }
        out.push(ch);
        w += cw;
    }
    out
}

// ───────────────────────── 输入框占位符（对应 cli/repl/placeholder.rs） ─────────────────────────

const PASTE_MIN_LINES: usize = 3;
const PASTE_MIN_ROWS: usize = 3;

/// `[Image N: 名字]` / `[粘贴 N: ~L 行]` 的 char 区间
fn find_placeholders(input: &str) -> Vec<(usize, usize)> {
    let chars: Vec<char> = input.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let rest: String = chars[i..].iter().collect();
        let prefix = if rest.starts_with("[Image ") { Some(7) } else if rest.starts_with("[粘贴 ") { Some(4) } else { None };
        if let Some(pl) = prefix {
            let mut j = i + pl;
            while j < chars.len() && chars[j].is_ascii_digit() { j += 1; }
            if j < chars.len() && chars[j] == ':' {
                while j < chars.len() && chars[j] != ']' { j += 1; }
                if j < chars.len() { out.push((i, j + 1)); i = j + 1; continue; }
            }
        }
        i += 1;
    }
    out
}

fn placeholder_spans(text: &str) -> Vec<Span<'static>> {
    let chars: Vec<char> = text.chars().collect();
    let mut spans = Vec::new();
    let mut last = 0;
    for (a, b) in find_placeholders(text) {
        if a > last { spans.push(Span::raw(chars[last..a].iter().collect::<String>())); }
        spans.push(Span::styled(chars[a..b].iter().collect::<String>(), MAGENTA));
        last = b;
    }
    if last < chars.len() { spans.push(Span::raw(chars[last..].iter().collect::<String>())); }
    spans
}

impl App {
    fn input_len(&self) -> usize { self.input.chars().count() }

    fn insert_str(&mut self, text: &str) {
        // 光标落在占位符内部就先跳到它后面
        if let Some((_, end)) = find_placeholders(&self.input).into_iter().find(|(a, b)| *a < self.cursor && self.cursor < *b) {
            self.cursor = end;
        }
        let mut chars: Vec<char> = self.input.chars().collect();
        let at = self.cursor.min(chars.len());
        let ins: Vec<char> = text.chars().collect();
        let n = ins.len();
        chars.splice(at..at, ins);
        self.input = chars.into_iter().collect();
        self.cursor = at + n;
        self.slash_sel = 0;
    }

    fn backspace(&mut self) {
        if self.cursor == 0 { return; }
        let (a, b) = match find_placeholders(&self.input).into_iter().find(|(a, b)| *a < self.cursor && self.cursor <= *b) {
            Some((a, b)) => (a, b), // 整块删
            None => (self.cursor - 1, self.cursor),
        };
        let mut chars: Vec<char> = self.input.chars().collect();
        chars.drain(a..b);
        self.input = chars.into_iter().collect();
        self.cursor = a;
        self.slash_sel = 0;
    }

    fn move_left(&mut self) {
        if self.cursor == 0 { return; }
        let target = self.cursor - 1;
        self.cursor = find_placeholders(&self.input).into_iter().find(|(a, b)| *a <= target && target < *b).map(|(a, _)| a).unwrap_or(target);
    }

    fn move_right(&mut self) {
        let len = self.input_len();
        if self.cursor >= len { return; }
        let c = self.cursor;
        self.cursor = find_placeholders(&self.input).into_iter().find(|(a, b)| *a <= c && c < *b).map(|(_, b)| b).unwrap_or(c + 1);
    }

    /// 粘贴：够长就折成 `[粘贴 N: ~L 行]`，否则原样插入
    fn paste_text(&mut self, text: &str, cols: usize) {
        let text = text.replace("\r\n", "\n").replace('\r', "\n");
        if text.is_empty() { return; }
        let lines = text.split('\n').count();
        let rows: usize = text.split('\n').map(|l| wrap_chars(l, cols.saturating_sub(2)).len()).sum();
        if lines >= PASTE_MIN_LINES || rows >= PASTE_MIN_ROWS {
            self.pasted.push(text.clone());
            let ph = format!("[粘贴 {}: ~{} 行]", self.pasted.len(), lines);
            self.insert_str(&ph);
        } else {
            self.insert_str(&text.replace('\n', " "));
        }
    }

    /// Ctrl+V：演示里当作剪贴板有一张图
    fn paste_image(&mut self) {
        self.images += 1;
        let ph = format!("[Image {}: clipboard-{}.png]", self.images, 1720 + self.images);
        self.insert_str(&ph);
    }

    /// 光标在折行后的 (行, 列)
    fn cursor_pos(&self, cols: usize) -> (u16, u16) {
        let width = cols.saturating_sub(2).max(1);
        let (mut row, mut col) = (0u16, 0usize);
        for (i, ch) in self.input.chars().enumerate() {
            if i == self.cursor { break; }
            let cw = ch.width().unwrap_or(0);
            if col > 0 && col + cw > width { row += 1; col = 0; }
            col += cw;
        }
        (row, col as u16)
    }
}

fn plain(spans: &[Span<'static>]) -> String {
    spans.iter().map(|s| s.content.as_ref()).collect()
}

// ───────────────────────── markdown → span 行（对应 render/markdown.rs、code.rs） ─────────────────────────

fn render_inline(text: &str) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    let mut rest = text;
    while !rest.is_empty() {
        if let Some(r) = rest.strip_prefix("**") {
            if let Some(end) = r.find("**") {
                spans.push(Span::styled(r[..end].to_string(), BOLD_BLUE));
                rest = &r[end + 2..];
                continue;
            }
        }
        if let Some(r) = rest.strip_prefix('`') {
            if let Some(end) = r.find('`') {
                spans.push(Span::styled(r[..end].to_string(), CYAN));
                rest = &r[end + 1..];
                continue;
            }
        }
        if rest.starts_with("http://") || rest.starts_with("https://") {
            let end = rest.find(|c: char| c.is_whitespace() || "，。、）)".contains(c)).unwrap_or(rest.len());
            spans.push(Span::styled(rest[..end].to_string(), URL));
            rest = &rest[end..];
            continue;
        }
        let first = rest.chars().next().map(char::len_utf8).unwrap_or(1);
        let next = rest[first..]
            .find(|c: char| c == '*' || c == '`' || c == 'h')
            .map(|i| i + first)
            .unwrap_or(rest.len());
        spans.push(Span::raw(rest[..next].to_string()));
        rest = &rest[next..];
    }
    spans
}

fn highlight_code(line: &str) -> Vec<Span<'static>> {
    if let Some(i) = line.find("//") {
        let mut s = highlight_code(&line[..i]);
        s.push(Span::styled(line[i..].to_string(), CODE_COMMENT));
        return s;
    }
    const KW: [&str; 10] = ["fn", "let", "mut", "pub", "struct", "impl", "match", "if", "else", "return"];
    let mut spans = Vec::new();
    let mut word = String::new();
    let mut in_str = false;
    let mut buf = String::new();
    let flush_word = |word: &mut String, spans: &mut Vec<Span<'static>>, next: Option<char>| {
        if word.is_empty() {
            return;
        }
        let st = if KW.contains(&word.as_str()) {
            CODE_KW
        } else if word.chars().all(|c| c.is_ascii_digit()) {
            CODE_NUM
        } else if next == Some('(') {
            CODE_FN
        } else {
            Style::new()
        };
        spans.push(Span::styled(std::mem::take(word), st));
    };
    for c in line.chars() {
        if in_str {
            buf.push(c);
            if c == '"' {
                spans.push(Span::styled(std::mem::take(&mut buf), CODE_STR));
                in_str = false;
            }
            continue;
        }
        if c == '"' {
            flush_word(&mut word, &mut spans, Some(c));
            in_str = true;
            buf.push(c);
        } else if c.is_alphanumeric() || c == '_' {
            word.push(c);
        } else {
            flush_word(&mut word, &mut spans, Some(c));
            spans.push(Span::raw(c.to_string()));
        }
    }
    flush_word(&mut word, &mut spans, None);
    if !buf.is_empty() {
        spans.push(Span::styled(buf, CODE_STR));
    }
    spans
}

fn render_markdown(text: &str) -> Vec<Vec<Span<'static>>> {
    let mut out = Vec::new();
    let mut code: Option<(String, Vec<String>)> = None;
    for raw in text.lines() {
        if let Some((lang, lines)) = &mut code {
            if raw.starts_with("```") {
                let width = lines.iter().map(|l| l.width()).chain([lang.width() + 5]).max().unwrap_or(24).max(24);
                let header = format!("╭─ {lang} ");
                out.push(vec![Span::styled(format!("{header}{}", "─".repeat(width.saturating_sub(header.width()))), CYAN)]);
                for l in lines.iter() {
                    out.push(highlight_code(l));
                }
                out.push(vec![Span::styled("─".repeat(width), CYAN)]);
                code = None;
            } else {
                lines.push(raw.to_string());
            }
            continue;
        }
        if let Some(lang) = raw.strip_prefix("```") {
            code = Some((if lang.is_empty() { "code".into() } else { lang.to_string() }, Vec::new()));
            continue;
        }
        let trimmed = raw.trim_start();
        if let Some(h) = trimmed.strip_prefix("# ") {
            out.push(vec![Span::styled(format!("# {h}"), BOLD_MAGENTA)]);
        } else if let Some(item) = trimmed.strip_prefix("- ") {
            let mut spans = vec![Span::styled("-", MAGENTA), Span::raw(" ")];
            spans.extend(render_inline(item));
            out.push(spans);
        } else if trimmed.len() >= 3 && trimmed.chars().all(|c| c == '-') {
            out.push(vec![Span::styled("─".repeat(24), DIM)]);
        } else {
            out.push(render_inline(raw));
        }
    }
    if let Some((lang, lines)) = code {
        let header = format!("╭─ {lang} ");
        out.push(vec![Span::styled(format!("{header}{}", "─".repeat(24usize.saturating_sub(header.width()))), CYAN)]);
        for l in lines {
            out.push(highlight_code(&l));
        }
    }
    out
}

// ───────────────────────── spinner（对应 render/wait_spinner.rs） ─────────────────────────

const SCAN_W: usize = 7;
const TRAIL: usize = 6;
const HOLD_END: usize = 9;
const HOLD_START: usize = 30;
const ACTIVE_DOTS: [&str; TRAIL] = ["▪", "▪", "▫", "▫", "·", "·"];
const BRAILLE: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

fn scanner_spans(frame: usize) -> Vec<Span<'static>> {
    let total = SCAN_W + HOLD_END + (SCAN_W - 1) + HOLD_START;
    let mut f = frame % total;
    let (pos, holding, hold_progress, forward) = if f < SCAN_W {
        (f, false, 0, true)
    } else {
        f -= SCAN_W;
        if f < HOLD_END {
            (SCAN_W - 1, true, f, true)
        } else {
            f -= HOLD_END;
            if f < SCAN_W - 1 {
                (SCAN_W - 2 - f, false, 0, false)
            } else {
                (0, true, f - (SCAN_W - 1), false)
            }
        }
    };
    (0..SCAN_W)
        .map(|i| {
            let distance = if forward { pos as isize - i as isize } else { i as isize - pos as isize };
            let idx = if holding {
                usize::try_from(distance).ok().map(|d| d + hold_progress)
            } else if distance == 0 {
                Some(0)
            } else if distance > 0 && distance < TRAIL as isize {
                Some(distance as usize)
            } else {
                None
            };
            match idx {
                Some(k) if k < TRAIL => Span::styled(ACTIVE_DOTS[k], if k < 2 { GREEN10 } else { GREEN10.add_modifier(Modifier::DIM) }),
                _ => Span::styled("·", GREEN10.add_modifier(Modifier::DIM)),
            }
        })
        .collect()
}

fn sound_wave(frame: usize, dev: bool) -> Vec<Span<'static>> {
    const LEVELS: [&str; 7] = ["▁", "▂", "▃", "▄", "▅", "▆", "▇"];
    let (hi, mid, low) = if dev {
        (BOLD_MAGENTA, MAGENTA, MAGENTA.add_modifier(Modifier::DIM))
    } else {
        (BOLD_BLUE, Style::new().fg(Color::LightCyan), DIM_CYAN)
    };
    let t = frame as f32 * 0.24;
    (0..5)
        .map(|i| {
            let h = ((t - i as f32 * 0.9).sin() + 1.0) / 2.0;
            let g = LEVELS[((h * 6.0) as usize).min(6)];
            Span::styled(g, if h > 0.72 { hi } else if h > 0.35 { mid } else { low })
        })
        .collect()
}

// ───────────────────────── 历史区行模型 ─────────────────────────

fn bar_spans(mode: Mode, rest: Vec<Span<'static>>) -> Vec<Span<'static>> {
    let mut spans = vec![Span::styled("┃", mode.bar())];
    if !rest.is_empty() {
        spans.push(Span::raw(" "));
        spans.extend(rest);
    }
    spans
}

fn bar_line(mode: Mode, rest: Vec<Span<'static>>) -> Line<'static> {
    Line::from(bar_spans(mode, rest))
}

fn row_plain(spans: Vec<Span<'static>>) -> Row {
    let copy = plain(&spans);
    Row { line: Line::from(spans), copy, copy_col: 0, click: None }
}

/// 竖条行：可复制文本从第 2 列开始（`┃ ` 是装饰）
fn row_bar(mode: Mode, text: &str) -> Row {
    let spans = if text.is_empty() { bar_spans(mode, vec![]) } else { bar_spans(mode, vec![Span::raw(text.to_string())]) };
    Row { line: Line::from(spans), copy: text.to_string(), copy_col: 2, click: None }
}

fn row_bar_styled(mode: Mode, spans: Vec<Span<'static>>) -> Row {
    let copy = plain(&spans);
    Row { line: bar_line(mode, spans), copy, copy_col: 2, click: None }
}

fn blank_row() -> Row {
    Row { line: Line::from(""), copy: String::new(), copy_col: 0, click: None }
}

fn push_wrapped(rows: &mut Vec<Row>, spans: Vec<Span<'static>>, width: usize) {
    for r in wrap_spans(spans, width) {
        rows.push(row_plain(r));
    }
}

fn history_rows(app: &App, width: usize) -> Vec<Row> {
    let mut rows = Vec::new();
    for (idx, entry) in app.history.iter().enumerate() {
        match entry {
            Entry::User { text, mode } => {
                rows.push(row_bar(*mode, ""));
                for r in wrap_chars(text, width.saturating_sub(3)) {
                    rows.push(row_bar_styled(*mode, placeholder_spans(&r)));
                }
                rows.push(row_bar(*mode, ""));
                rows.push(blank_row());
            }
            Entry::Reasoning { summary, body, expanded } => {
                // 思考摘要也是一整块：点任意处展开/收起完整思考，展开内容绿色 dim
                let first = rows.len();
                push_wrapped(&mut rows, vec![Span::styled(summary.clone(), GREEN10)], width);
                if *expanded {
                    for para in body.split('\n') {
                        if para.is_empty() {
                            rows.push(blank_row());
                        } else {
                            for r in wrap_chars(para, width.saturating_sub(2)) {
                                rows.push(row_plain(vec![Span::raw("  "), Span::styled(r, GREEN10.add_modifier(Modifier::DIM))]));
                            }
                        }
                    }
                }
                for r in &mut rows[first..] {
                    r.click = Some(Click::Block(idx));
                }
                rows.push(blank_row());
            }
            Entry::ToolBlock { header, body, expanded } => {
                // 头行可点击：展开/收起输出；输出按命令块的 `  │ ` 词汇画
                // 整块（头行 + 展开后的输出行）都是同一个点击目标：点任意处都能展开/收起
                let first = rows.len();
                for l in header.iter() {
                    rows.push(row_plain(vec![Span::styled(l.clone(), DIM)]));
                }
                if *expanded {
                    for l in body {
                        push_wrapped(&mut rows, vec![Span::styled("  │ ", DIM), Span::styled(l.clone(), DIM)], width);
                    }
                }
                for r in &mut rows[first..] {
                    r.click = Some(Click::Block(idx));
                }
                rows.push(blank_row());
            }
            Entry::Assistant(t) => {
                for spans in render_markdown(t) {
                    push_wrapped(&mut rows, spans, width);
                }
                rows.push(blank_row());
                rows.push(blank_row());
            }
            Entry::Note(t) => {
                push_wrapped(&mut rows, vec![Span::styled(t.clone(), DIM)], width);
                rows.push(blank_row());
            }
            Entry::Image(img) => {
                rows.extend(image_rows(img));
                rows.push(blank_row());
            }
        }
    }
    if let Some(turn) = &app.turn {
        match turn.phase {
            Phase::Reasoning => {
                let mut spans = scanner_spans(app.frame);
                spans.push(Span::raw(" "));
                spans.push(Span::styled(format!("思考 · {:.1}s", turn.phase_started.elapsed().as_secs_f64()), GREEN10));
                rows.push(row_plain(spans));
            }
            Phase::Tool => {
                rows.push(row_plain(vec![
                    Span::styled(BRAILLE[app.frame % 10], DIM_CYAN),
                    Span::raw(" "),
                    Span::styled(format!("~ 读文件×1 运行中 · src/cli/repl/layout.rs · {:.1}s", turn.phase_started.elapsed().as_secs_f64()), DIM_CYAN),
                ]));
                rows.push(row_plain(vec![Span::styled("  ↳ 457 行", DIM_CYAN)]));
            }
            Phase::Text => {}
        }
        // spinner 与活动区之间空一行
        if turn.phase != Phase::Text {
            rows.push(blank_row());
        }
    }
    // 排队气泡（queued_prompt_lines），之后与输入区之间空一行（resume_at 的 queue_gap）
    for (i, q) in app.queue.iter().enumerate() {
        if i > 0 {
            rows.push(blank_row());
        }
        rows.push(row_bar(app.mode, ""));
        for r in wrap_chars(q, width.saturating_sub(3)) {
            rows.push(row_bar(app.mode, &r));
        }
        rows.push(row_bar(app.mode, ""));
        rows.push(row_bar_styled(app.mode, vec![Span::styled("排队中", app.mode.bar())]));
    }
    if !app.queue.is_empty() {
        rows.push(blank_row());
    }
    rows
}

/// 选区文本：逐行按显示列切，跳过每行的装饰列
fn selection_text(rows: &[Row], sel: Selection) -> String {
    let (a, b) = if sel.anchor <= sel.cur { (sel.anchor, sel.cur) } else { (sel.cur, sel.anchor) };
    let mut out = Vec::new();
    for abs in a.0..=b.0.min(rows.len().saturating_sub(1)) {
        let row = &rows[abs];
        if row.copy_col == u16::MAX {
            continue;
        }
        let start = if abs == a.0 { a.1.max(row.copy_col) } else { row.copy_col };
        let end = if abs == b.0 { b.1 } else { u16::MAX };
        let mut s = String::new();
        let mut col = row.copy_col;
        for ch in row.copy.chars() {
            let cw = ch.width().unwrap_or(0) as u16;
            if col + cw > start && col <= end {
                s.push(ch);
            }
            col += cw;
        }
        out.push(s.trim_end().to_string());
    }
    out.join("\n")
}

// ───────────────────────── 渲染 ─────────────────────────

fn footer_line(app: &App, cols: usize) -> Line<'static> {
    let mut left: Vec<Span<'static>> = vec![
        Span::styled("┃", app.mode.bar()),
        Span::raw(" "),
        Span::styled(app.mode.label(), app.mode.bar()),
        Span::raw(" · "),
        Span::raw(app.models[app.model].split(" / ").last().unwrap_or("").to_string()),
        Span::raw(" "),
        Span::styled(app.models[app.model].split(" / ").next().unwrap_or("").to_lowercase().replace(' ', "-"), DIM),
        Span::raw(" · "),
        Span::styled("high", app.mode.bar()),
    ];
    if app.turn.is_some() {
        left.push(Span::raw("   "));
        left.extend(sound_wave(app.frame, app.mode == Mode::Dev));
    }
    let right = format!("{:.1}k/200k({:.1}%) · Σ82.0k(C86%)", app.session_tokens_k, app.session_tokens_k / 2.0);
    let lw: usize = left.iter().map(|s| s.width()).sum();
    let gap = cols.saturating_sub(lw + right.width()).max(1);
    left.push(Span::raw(" ".repeat(gap)));
    left.push(Span::styled(right, DIM));
    Line::from(left)
}

/// 后台任务状态条（background_job_lines）：前导空行，每任务一行，计时右对齐；行可点击
fn job_rows(app: &App, cols: usize) -> Vec<Row> {
    if app.jobs.is_empty() {
        return Vec::new();
    }
    let kind_col = app.jobs.iter().map(|j| j.kind_label().width()).max().unwrap_or(0);
    let mut rows = vec![blank_row()];
    for (i, job) in app.jobs.iter().enumerate() {
        let secs = job.started.elapsed().as_secs();
        let timer = if secs >= 60 { format!("{}m {:02}s", secs / 60, secs % 60) } else { format!("{secs}s") };
        let kind = job.kind_label();
        let left = format!("{} {kind}{} {} · {}", BRAILLE[app.frame % 10], " ".repeat(kind_col - kind.width()), job.id, job.title);
        let pad = cols.saturating_sub(left.width() + timer.width()).max(1);
        let text = format!("{left}{}{timer}", " ".repeat(pad));
        let opened = matches!(app.overlay, Overlay::Job(j) if j == i);
        rows.push(Row { line: Line::from(Span::styled(text.clone(), if opened { Style::new() } else { DIM })), copy: text, copy_col: 0, click: Some(Click::Job(i)) });
    }
    rows
}

/// 活动区的行（也进选区模型，输入框文字可拖选）：┃ 空行 / ┃ 输入行… / ┃ 空行 / footer / 任务条 / 底部留一行空
fn tail_rows(app: &App, cols: usize) -> Vec<Row> {
    let mut rows = vec![row_bar(app.mode, "")];
    for r in wrap_chars(&app.input, cols.saturating_sub(2)) {
        rows.push(row_bar_styled(app.mode, placeholder_spans(&r)));
    }
    rows.push(row_bar(app.mode, ""));
    rows.push(Row { line: footer_line(app, cols), copy: String::new(), copy_col: u16::MAX, click: None });
    rows.extend(job_rows(app, cols));
    rows.push(blank_row());
    rows
}

fn draw(f: &mut Frame, app: &mut App) {
    let area = f.area();
    let cols = area.width as usize;

    let input_rows = wrap_chars(&app.input, cols.saturating_sub(2));
    let tail_model = tail_rows(app, cols);
    let tail_h = tail_model.len() as u16;
    let body = Rect { x: 0, y: 0, width: area.width, height: area.height.saturating_sub(tail_h) };
    let tail = Rect { x: 0, y: body.height, width: area.width, height: tail_h };
    app.body = body;

    // 历史区：自己折行
    let text_w = body.width as usize;
    let rows = history_rows(app, text_w);
    app.hist_len = rows.len();
    let max_scroll = rows.len().saturating_sub(body.height as usize);
    if app.follow {
        app.scroll = max_scroll;
    } else {
        app.scroll = app.scroll.min(max_scroll);
    }
    let visible: Vec<Line<'static>> = rows.iter().skip(app.scroll).take(body.height as usize).map(|r| r.line.clone()).collect();
    f.render_widget(Paragraph::new(visible), Rect { width: text_w as u16, ..body });

    // 活动区
    let tail_lines: Vec<Line<'static>> = tail_model.iter().map(|r| r.line.clone()).collect();
    f.render_widget(Paragraph::new(tail_lines), tail);

    // 悬停效果：鼠标停在可点击的行上，这一行去 dim 提亮（任务状态行 / 工具块头）
    if let Some((_, hy)) = app.hover {
        let hovered_click = if hy < body.height {
            rows.get(app.scroll + hy as usize).and_then(|r| r.click)
        } else {
            tail_model.get((hy - body.height) as usize).and_then(|r| r.click)
        };
        // 同一个点击目标的所有可见行一起提亮（工具块是一整块）
        if let Some(target) = hovered_click {
            let buf = f.buffer_mut();
            for y in 0..area.height {
                let click = if y < body.height {
                    rows.get(app.scroll + y as usize).and_then(|r| r.click)
                } else {
                    tail_model.get((y - body.height) as usize).and_then(|r| r.click)
                };
                if click != Some(target) {
                    continue;
                }
                for x in 0..area.width {
                    if let Some(cell) = buf.cell_mut((x, y)) {
                        cell.set_style(cell.style().remove_modifier(Modifier::DIM));
                    }
                }
            }
        }
    }

    // 选区高亮（历史区 + 输入框）：只反显可复制的列，左侧装饰不亮
    if let Some(sel) = app.selection {
        let (a, b) = if sel.anchor <= sel.cur { (sel.anchor, sel.cur) } else { (sel.cur, sel.anchor) };
        let hist_len = rows.len();
        let buf = f.buffer_mut();
        for y in 0..area.height {
            let (abs, row) = if y < body.height {
                let abs = app.scroll + y as usize;
                if abs >= hist_len { continue; }
                (abs, &rows[abs])
            } else {
                let i = (y - body.height) as usize;
                if i >= tail_model.len() { continue; }
                (hist_len + i, &tail_model[i])
            };
            if abs < a.0 || abs > b.0 {
                continue;
            }
            if row.copy_col == u16::MAX || row.copy.is_empty() {
                continue;
            }
            let start = if abs == a.0 { a.1.max(row.copy_col) } else { row.copy_col };
            let end = if abs == b.0 { b.1 } else { (row.copy_col + row.copy.width() as u16).saturating_sub(1) };
            let end = end.min((row.copy_col + row.copy.width() as u16).saturating_sub(1));
            for x in start..=end {
                if x < text_w as u16 {
                    if let Some(cell) = buf.cell_mut((x, y)) {
                        cell.set_style(cell.style().add_modifier(Modifier::REVERSED));
                    }
                }
            }
        }
    }


    let (crow, ccol) = app.cursor_pos(cols);
    let _ = &input_rows;
    f.set_cursor_position((2 + ccol, tail.y + 1 + crow));

    // 命令菜单：锚在输入框上方的列表
    app.hits.clear();
    let matches = app.slash_matches();
    if !matches.is_empty() {
        let hits = draw_slash_menu(f, app, &matches, tail);
        app.hits = hits;
    }

    match &app.overlay {
        Overlay::None => {}
        Overlay::Picker(p) => {
            let p = p.clone();
            let hits = draw_picker(f, app, &p, tail);
            app.hits = hits;
        }
        Overlay::Help { scroll } => draw_help(f, app, area, *scroll),
        Overlay::Job(i) => {
            if let Some(job) = app.jobs.get(*i) {
                // 标题行：左「类别 · 标题」粗体，右「id · 时长 · Esc」dim，像 footer 一样两端对齐
                let secs = job.started.elapsed().as_secs();
                let timer = if secs >= 60 { format!("{}m {:02}s", secs / 60, secs % 60) } else { format!("{secs}s") };
                let left = format!("  {} · {}", job.kind_label(), job.title);
                let right = format!("{} · {timer} · Esc 关闭  ", job.id);
                let gap = cols.saturating_sub(left.width() + right.width()).max(1);
                let mut lines = vec![Line::from(vec![
                    Span::styled(left, BOLD),
                    Span::raw(" ".repeat(gap)),
                    Span::styled(right, DIM),
                ])];
                lines.push(Line::from(""));
                lines.extend(job.detail_lines(app.frame));
                anchored_block(f, tail, lines, cols);
            }
        }
    }

    // 通知小悬浮窗：带边框的小盒子，绿点 + 粗体文字，居中悬在输入框上方，2.5s 自动消失
    if let Some((text, _)) = &app.toast {
        let w = (text.width() + 6) as u16;
        let r = Rect { x: area.width.saturating_sub(w) / 2, y: tail.y.saturating_sub(3), width: w.min(area.width), height: 3 };
        f.render_widget(Clear, r);
        let block = Block::default().borders(Borders::ALL).border_style(Style::new().fg(Color::Green));
        let inner = block.inner(r);
        f.render_widget(block, r);
        f.render_widget(
            Paragraph::new(Line::from(vec![Span::styled(" ● ", Style::new().fg(Color::Green)), Span::styled(text.clone(), BOLD)])),
            inner,
        );
    }
}

/// 锚在活动区上沿的一块区域，清空后画 `┃` 行。
/// 锚在活动区上沿的一块区域：与输入区之间留一行空，整行清空后画内容
fn anchored_block(f: &mut Frame, tail: Rect, lines: Vec<Line<'static>>, width: usize) -> Rect {
    let h = lines.len() as u16;
    let r = Rect { x: 0, y: tail.y.saturating_sub(h + 1), width: width.min(tail.width as usize) as u16, height: h };
    f.render_widget(Clear, Rect { x: 0, y: r.y, width: tail.width, height: h + 1 });
    f.render_widget(Paragraph::new(lines), r);
    r
}

/// 命令列表：只占 4 行，选中项超出就滚窗口；不带左侧竖条，`›` 在第 0 列、文字对齐输入框文字
const SLASH_ROWS: usize = 4;

fn draw_slash_menu(f: &mut Frame, app: &App, matches: &[(&str, &str)], tail: Rect) -> Vec<(u16, Click)> {
    let w = (tail.width as usize).min(72);
    let sel = app.slash_sel.min(matches.len() - 1);
    let start = sel.saturating_sub(SLASH_ROWS - 1).min(matches.len().saturating_sub(SLASH_ROWS));
    let mut lines = Vec::new();
    for (i, (c, d)) in matches.iter().enumerate().skip(start).take(SLASH_ROWS) {
        let spans = if i == sel {
            vec![Span::styled("›", BOLD_MAGENTA), Span::raw(" "), Span::styled(format!("{c:<11}"), BOLD), Span::raw(truncate(d, w.saturating_sub(13)))]
        } else {
            vec![Span::raw("  "), Span::styled(format!("{c:<11}{}", truncate(d, w.saturating_sub(13))), DIM)]
        };
        lines.push(Line::from(spans));
    }
    let r = anchored_block(f, tail, lines, w);
    (start..start + r.height as usize).enumerate().map(|(k, i)| (r.y + k as u16, Click::Menu(i))).collect()
}

fn picker_items(app: &App, p: &Picker) -> Vec<(usize, String)> {
    let all: Vec<String> = match p.kind {
        PickerKind::Models => app.models.iter().map(|m| m.to_string()).collect(),
        PickerKind::Sessions => app.sessions.clone(),
    };
    let q = p.query.trim().to_lowercase();
    all.into_iter()
        .enumerate()
        .filter(|(_, s)| q.is_empty() || s.to_lowercase().contains(&q))
        .collect()
}

fn draw_picker(f: &mut Frame, app: &App, p: &Picker, tail: Rect) -> Vec<(u16, Click)> {
    let items = picker_items(app, p);
    let pages = items.len().div_ceil(PAGE).max(1);
    let page = p.page.min(pages - 1);
    let visible: Vec<&(usize, String)> = items.iter().skip(page * PAGE).take(PAGE).collect();
    let w = (tail.width as usize).min(72);
    let (title, help) = match p.kind {
        PickerKind::Models => ("选择模型", "输入搜索 · ↑↓ 移动 · Enter 选定 · Tab 多选 · Esc 关闭".to_string()),
        PickerKind::Sessions => ("选择会话", format!("输入搜索 · ↑↓ 移动 · ←→ 翻页 {}/{} · Enter 选择 · Ctrl+D 删除 · Esc 取消", page + 1, pages)),
    };
    let title = if p.query.is_empty() { title.to_string() } else { format!("{title} · {}", p.query) };
    let mut lines = vec![Line::from(Span::styled(title, BOLD))];
    for (i, (idx, text)) in visible.iter().enumerate() {
        let (marker, active) = match p.kind {
            PickerKind::Models => (if *idx == app.model { "[*] " } else { "[ ] " }, *idx == app.model),
            PickerKind::Sessions => (if *idx == 0 { "* " } else { "  " }, false),
        };
        let body = truncate(&format!("{marker}{text}"), w.saturating_sub(4));
        let spans = if i == p.selected {
            vec![Span::styled("›", BOLD_MAGENTA), Span::raw(" "), Span::styled(body, BOLD)]
        } else if active {
            vec![Span::raw("  "), Span::styled(body, BOLD_GREEN)]
        } else {
            vec![Span::raw("  "), Span::styled(body, DIM)]
        };
        lines.push(Line::from(spans));
    }
    lines.push(Line::from(Span::styled(truncate(&help, w), DIM)));
    let n = visible.len();
    let r = anchored_block(f, tail, lines, w);
    (0..n).map(|i| (r.y + 1 + i as u16, Click::PickerItem(i))).collect()
}

fn draw_help(f: &mut Frame, app: &App, area: Rect, scroll: u16) {
    let r = Rect { x: 2, y: 1, width: area.width.saturating_sub(4), height: area.height.saturating_sub(2) };
    f.render_widget(Clear, r);
    let mut lines = vec![Line::from(Span::styled("命令:", BOLD))];
    for (c, d) in SLASH {
        lines.push(Line::from(format!("  {c:<52}{d}")));
    }
    lines.push(Line::from(Span::styled("快捷键:", BOLD)));
    for (k, d) in [
        ("/", "输入后弹出命令列表：↑↓ 选择、Tab 补全、Enter 执行、Esc 清空"),
        ("Tab", "输入为空时切换 普通/开发"),
        ("Enter", "发送消息；回复中则排队"),
        ("Ctrl+V / 粘贴", "Ctrl+V 粘贴剪贴板图片 → [Image N: …]；粘贴 ≥3 行文本 → [粘贴 N: ~L 行]；占位符整块跳、整块删（F6 模拟长文本粘贴）"),
        ("鼠标", "拖选正文即复制（不含左侧竖条），滚轮滚历史；Shift+拖 = 终端原生选择"),
        ("↑/↓ PgUp/PgDn", "输入为空时回翻历史，End 回底"),
        ("F4", "切换后台任务条（一条命令 + 一个子代理）；点击状态行看详情，Esc 关"),
        ("F5", "切换鼠标捕获（关掉后拖选走终端原生，会带上竖条）"),
        ("Ctrl+C", "先关弹层；再清空输入；再中断回复；都没有则退出"),
        ("Ctrl+D", "退出"),
    ] {
        lines.push(Line::from(format!("  {k:<16}{d}")));
    }
    lines.push(Line::from(""));
    lines.push(bar_line(app.mode, vec![Span::styled("整页覆盖层 · ↑↓ 或 PgUp/PgDn 滚动 · Esc 关闭", DIM)]));
    f.render_widget(Paragraph::new(lines).scroll((scroll, 0)), r);
}

// ───────────────────────── 事件 ─────────────────────────

fn handle_picker_key(app: &mut App, code: KeyCode, ctrl: bool) {
    let Overlay::Picker(p) = &app.overlay else { return };
    let mut p = p.clone();
    let items = picker_items(app, &p);
    let pages = items.len().div_ceil(PAGE).max(1);
    p.page = p.page.min(pages - 1);
    let on_page = items.len().saturating_sub(p.page * PAGE).min(PAGE);
    match code {
        KeyCode::Esc => { app.overlay = Overlay::None; return; }
        KeyCode::Up => p.selected = p.selected.saturating_sub(1),
        KeyCode::Down => p.selected = (p.selected + 1).min(on_page.saturating_sub(1)),
        KeyCode::Left | KeyCode::PageUp => { p.page = p.page.saturating_sub(1); p.selected = 0; }
        KeyCode::Right | KeyCode::PageDown => { p.page = (p.page + 1).min(pages - 1); p.selected = 0; }
        KeyCode::Backspace => { p.query.pop(); p.page = 0; p.selected = 0; }
        KeyCode::Char(c) if !ctrl => { p.query.push(c); p.page = 0; p.selected = 0; }
        KeyCode::Enter | KeyCode::Tab => {
            if let Some((idx, text)) = items.get(p.page * PAGE + p.selected) {
                match p.kind {
                    PickerKind::Models => app.model = *idx,
                    PickerKind::Sessions => {
                        app.history.push(Entry::Note(format!("已切换到 {text}（演示只插一条提示）")));
                        app.follow = true;
                    }
                }
            }
            app.overlay = Overlay::None;
            return;
        }
        _ => {}
    }
    app.overlay = Overlay::Picker(p);
}

fn handle_key(app: &mut App, code: KeyCode, mods: KeyModifiers) -> bool {
    let ctrl = mods.contains(KeyModifiers::CONTROL);
    if ctrl && code == KeyCode::Char('d') {
        return !matches!(app.overlay, Overlay::Picker(_));
    }
    if ctrl && code == KeyCode::Char('c') {
        if !matches!(app.overlay, Overlay::None) {
            app.overlay = Overlay::None;
        } else if !app.input.is_empty() {
            app.input.clear();
            app.cursor = 0;
        } else if app.turn.is_some() {
            app.turn = None;
            app.history.push(Entry::Note("已中断".into()));
        } else {
            return true;
        }
        return false;
    }
    match &mut app.overlay {
        Overlay::Picker(_) => handle_picker_key(app, code, ctrl),
        Overlay::Job(_) => {
            if matches!(code, KeyCode::Esc | KeyCode::Char('q')) {
                app.overlay = Overlay::None;
            }
        }
        Overlay::Help { scroll } => match code {
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::F(1) => app.overlay = Overlay::None,
            KeyCode::Down => *scroll += 1,
            KeyCode::Up => *scroll = scroll.saturating_sub(1),
            KeyCode::PageDown => *scroll += 10,
            KeyCode::PageUp => *scroll = scroll.saturating_sub(10),
            _ => {}
        },
        Overlay::None => {
            let menu = app.slash_matches();
            if !menu.is_empty() {
                // 命令列表打开时的按键
                let n = menu.len();
                match code {
                    KeyCode::Up => { app.slash_sel = app.slash_sel.saturating_sub(1); return false; }
                    KeyCode::Down => { app.slash_sel = (app.slash_sel + 1).min(n - 1); return false; }
                    KeyCode::Tab => { app.input = menu[app.slash_sel.min(n - 1)].0.to_string(); return false; }
                    KeyCode::Enter => { app.input = menu[app.slash_sel.min(n - 1)].0.to_string(); app.submit(); return false; }
                    KeyCode::Esc => { app.input.clear(); app.cursor = 0; app.slash_sel = 0; return false; }
                    _ => {}
                }
            }
            match code {
                KeyCode::F(1) => app.overlay = Overlay::Help { scroll: 0 },
                KeyCode::F(4) => {
                    if app.jobs.is_empty() {
                        app.jobs.push(Job { kind: JobKind::Cmd, id: "a1b2c3", title: "cargo build --release", started: Instant::now() });
                        app.jobs.push(Job { kind: JobKind::Agent, id: "b7c2d1", title: "调研 ratatui 内存", started: Instant::now() });
                    } else {
                        app.jobs.clear();
                        if matches!(app.overlay, Overlay::Job(_)) { app.overlay = Overlay::None; }
                    }
                }
                KeyCode::F(5) => app.toggle_mouse = true,
                KeyCode::Tab if app.input.is_empty() => {
                    app.mode = if app.mode == Mode::Normal { Mode::Dev } else { Mode::Normal };
                }
                KeyCode::Enter => app.submit(),
                KeyCode::Backspace => app.backspace(),
                KeyCode::Left => app.move_left(),
                KeyCode::Right => app.move_right(),
                KeyCode::Home if !app.input.is_empty() => app.cursor = 0,
                KeyCode::End if !app.input.is_empty() => app.cursor = app.input_len(),
                KeyCode::Char('v') if ctrl => app.paste_image(),
                KeyCode::F(6) => {
                    let cols = app.body.width as usize;
                    app.paste_text("fn main() {\n    println!(\"hello\");\n}\n// 这是模拟的一大段粘贴\n// 第五行", cols);
                }
                KeyCode::PageUp => { app.follow = false; app.scroll = app.scroll.saturating_sub(5); }
                KeyCode::PageDown => { app.scroll = app.scroll.saturating_add(5); }
                KeyCode::Up if app.input.is_empty() => { app.follow = false; app.scroll = app.scroll.saturating_sub(3); }
                KeyCode::Down if app.input.is_empty() => { app.scroll = app.scroll.saturating_add(3); }
                KeyCode::End => app.follow = true,
                KeyCode::Esc => app.selection = None,
                KeyCode::Char(c) if !ctrl => app.insert_str(&c.to_string()),
                _ => {}
            }
        }
    }
    false
}

fn handle_mouse(app: &mut App, kind: MouseEventKind, x: u16, y: u16) {
    let body = app.body;
    let selectable = x < body.width;
    // 屏幕行 → 选区模型的绝对行：历史区按滚动偏移，活动区接在历史之后
    let abs_of = |app: &App, y: u16| if y < body.height { app.scroll + y as usize } else { app.hist_len + (y - body.height) as usize };
    match kind {
        MouseEventKind::ScrollUp => { app.follow = false; app.scroll = app.scroll.saturating_sub(3); }
        MouseEventKind::ScrollDown => { app.scroll = app.scroll.saturating_add(3); }
        MouseEventKind::Moved => {
            app.hover = Some((x, y));
            // 悬浮块里的列表项：悬停即选中
            if let Some((_, c)) = app.hits.iter().find(|(hy, _)| *hy == y).copied() {
                match c {
                    Click::Menu(i) => app.slash_sel = i,
                    Click::PickerItem(i) => {
                        if let Overlay::Picker(p) = &mut app.overlay { p.selected = i; }
                    }
                    _ => {}
                }
            }
        }
        MouseEventKind::Down(MouseButton::Left) => {
            // 悬浮块里的列表项：点击 = Enter
            if let Some((_, c)) = app.hits.iter().find(|(hy, _)| *hy == y).copied() {
                match c {
                    Click::Menu(i) => {
                        let m = app.slash_matches();
                        if let Some((cmd, _)) = m.get(i) { app.input = cmd.to_string(); app.submit(); }
                    }
                    Click::PickerItem(i) => {
                        if let Overlay::Picker(p) = &mut app.overlay { p.selected = i; }
                        handle_picker_key(app, KeyCode::Enter, false);
                    }
                    _ => {}
                }
                return;
            }
            // 点在后台任务状态行上：打开/关闭任务详情，不开始选区
            if y >= body.height {
                let rows = tail_rows(app, body.width as usize);
                if let Some(Click::Job(j)) = rows.get((y - body.height) as usize).and_then(|r| r.click) {
                    app.overlay = if matches!(app.overlay, Overlay::Job(k) if k == j) { Overlay::None } else { Overlay::Job(j) };
                    return;
                }
            } else {
                // 点在工具块头上：展开 / 收起输出
                let rows = history_rows(app, body.width as usize);
                if let Some(Click::Block(idx)) = rows.get(app.scroll + y as usize).and_then(|r| r.click) {
                    match app.history.get_mut(idx) {
                        Some(Entry::ToolBlock { expanded, .. }) | Some(Entry::Reasoning { expanded, .. }) => *expanded = !*expanded,
                        _ => {}
                    }
                    // 展开/收起时钉住当前视口，头行留在原处，不被「跟随底部」拽走
                    app.follow = false;
                    return;
                }
            }
            let abs = abs_of(app, y);
            app.selection = selectable.then(|| Selection { anchor: (abs, x), cur: (abs, x), dragging: true });
        }
        MouseEventKind::Drag(MouseButton::Left) => {
            let abs = abs_of(app, y);
            if let Some(sel) = &mut app.selection {
                if sel.dragging {
                    sel.cur = (abs, x);
                }
            }
        }
        MouseEventKind::Up(MouseButton::Left) => {
            if let Some(mut sel) = app.selection {
                sel.dragging = false;
                app.selection = Some(sel);
                if sel.anchor == sel.cur {
                    app.selection = None;
                } else {
                    let mut rows = history_rows(app, body.width as usize);
                    rows.extend(tail_rows(app, body.width as usize));
                    let text = selection_text(&rows, sel);
                    if !text.trim().is_empty() {
                        app.pending_copy = Some(text);
                    }
                }
            }
        }
        _ => {}
    }
}

fn main() -> io::Result<()> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    let mut app = App::new();

    let tick = Duration::from_millis(40);
    let mut last_tick = Instant::now();
    let result = loop {
        for e in app.history.iter_mut() {
            if let Entry::Image(img) = e {
                if !img.transmitted {
                    if is_kitty() {
                        transmit_png(terminal.backend_mut(), img)?;
                    }
                    img.transmitted = true;
                }
            }
        }
        if app.toggle_mouse {
            app.toggle_mouse = false;
            app.mouse_capture = !app.mouse_capture;
            if app.mouse_capture {
                execute!(terminal.backend_mut(), EnableMouseCapture)?;
            } else {
                execute!(terminal.backend_mut(), DisableMouseCapture)?;
            }
            app.history.push(Entry::Note(if app.mouse_capture {
                "鼠标捕获：开（程序自己画选区并复制，不带竖条）".into()
            } else {
                "鼠标捕获：关（拖选走终端原生，会带上竖条）".into()
            }));
            app.follow = true;
        }
        // 松开鼠标后：OSC 52 写剪贴板（kitty 默认允许 write-clipboard）
        if let Some(text) = app.pending_copy.take() {
            let b64 = base64::engine::general_purpose::STANDARD.encode(text.as_bytes());
            let out = terminal.backend_mut();
            write!(out, "\x1b]52;c;{b64}\x07")?;
            out.flush()?;
            let n = text.chars().count();
            app.toast = Some((format!("已复制 {n} 个字符"), Instant::now()));
            app.selection = None;
        }
        if app.dirty || app.animating() {
            if let Err(e) = terminal.draw(|f| draw(f, &mut app)) {
                break Err(e);
            }
            app.dirty = false;
        }
        let timeout = if app.animating() { tick.saturating_sub(last_tick.elapsed()) } else { Duration::from_secs(3600) };
        if event::poll(timeout)? {
            app.dirty = true;
            match event::read()? {
                Event::Key(k) if k.kind != KeyEventKind::Release => {
                    if handle_key(&mut app, k.code, k.modifiers) {
                        break Ok(());
                    }
                }
                Event::Mouse(m) => {
                    if std::env::var("MIYU_DEMO_LOG").is_ok() {
                        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(std::env::var("MIYU_DEMO_LOG").unwrap()) {
                            let _ = writeln!(f, "mouse {:?} col={} row={} body_h={} overlay_job={}", m.kind, m.column, m.row, app.body.height, matches!(app.overlay, Overlay::Job(_)));
                        }
                    }
                    handle_mouse(&mut app, m.kind, m.column, m.row)
                }
                Event::Paste(s) => { let cols = app.body.width as usize; app.paste_text(&s, cols); }
                _ => {}
            }
        }
        if app.animating() && last_tick.elapsed() >= tick {
            app.tick();
            last_tick = Instant::now();
        }
    };

    disable_raw_mode()?;
    if app.mouse_capture {
        execute!(terminal.backend_mut(), DisableMouseCapture)?;
    }
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    result
}
