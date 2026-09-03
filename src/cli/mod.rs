use crate::agent::{
    archive_and_delete_visible_turns, Agent, AgentEvent, AgentMode, AgentTurnControl,
};
use crate::config::{ActiveProviderModelConfig, AppConfig};
use crate::i18n::{is_zh, text as t};
use crate::ipc::{self, Command as IpcCommand, Frame as IpcFrame, Request as IpcRequest};
use crate::llm::{
    ChatResult, ChatStreamChunk, OpenAiCompatibleClient, ThinkingVariantOptions, TurnTokens, Usage,
};
use crate::memory::{MemoryOrganizer, MemoryStore};
use crate::paths::MiyuPaths;
mod args;
mod daemon_cmds;
mod inline_picker;
mod localize;
mod mcp_schema;
mod mcp_serve;
mod setup;
mod stdin_input;
mod tool_cmds;
mod usage_view;
use args::*;
use daemon_cmds::*;
use inline_picker::*;
use localize::*;
use mcp_serve::*;
use setup::*;
use stdin_input::*;
use tool_cmds::*;
use usage_view::*;
mod alarm_worker;
mod daemon_log;
mod data_cmds;
mod footer;
mod migrate_cmds;
mod model_cmds;
mod pop_cmds;
mod repl;
mod select;
mod shell_bridge;

// 日志读取与格式化已拆到 daemon_log。
use alarm_worker::*;
use daemon_log::*;
use data_cmds::*;
use footer::*;
use migrate_cmds::*;
use model_cmds::*;
use pop_cmds::*;
use select::*;
use shell_bridge::*;
#[cfg(test)]
mod tests;

// 宽度计算与输入编辑已拆到 repl 子模块，这里引回来。
// repl 下几个新拆的子模块整组导入（原本就在 cli/mod.rs 里，平铺可见）
pub(in crate::cli) use repl::{commands::*, jobs::*, layout::*, placeholder::*, session::*};
// 命令表已上提到 crate 级与 WebUI 共用；这里再导出一次，cli 内的调用点不变。
pub(in crate::cli) use crate::slash_commands::*;
use repl::direct::{run_chat_with_images, run_chat_with_options, run_direct_repl};
use repl::editor::{load_repl_input_history, repl_input_lines};
use repl::input::render_repl_input_with_footer;
use repl::live_turn::{
    handle_live_agent_event, handle_live_post_turn_overflow, run_live_agent_turn,
};
use repl::remote::{run_remote_repl, try_run_remote_chat};
use repl::tail::{
    cursor_col_or, cursor_row_or, synchronized_terminal_update, LiveRawMode, LiveReplTail,
    TerminalFrameLayout, TerminalFrameTracker,
};
use repl::wake::follow_wake_run;
use repl::width::{truncate_visible_width, visible_width, wrap_visible_width};

use crate::render;
use crate::tools::build_tool_registry;

// 参数类型已下沉到基础层；这里 re-export，外部按 `cli::WebArgs` 引用不断。
pub use crate::args::WebArgs;
use crate::shell;
use crate::state::{QueuedPrompt, QueuedPromptAttachment, StateStore, Turn, TurnStatus};
use crate::tools;
use anyhow::{bail, Context, Result};
use base64::Engine;
use chrono::{DateTime, Local};
use clap::{Arg, ArgAction, Args, CommandFactory, FromArgMatches, Parser, Subcommand};
use crossterm::cursor::{self, Hide, MoveTo, Show};
use crossterm::event::{
    self, DisableBracketedPaste, DisableFocusChange, EnableBracketedPaste, EnableFocusChange,
    Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers,
};
use crossterm::style::{Color, Print, Stylize};
use crossterm::terminal::{self, BeginSynchronizedUpdate, Clear, ClearType, EndSynchronizedUpdate};
use crossterm::{execute, queue};
use fuzzy_matcher::skim::SkimMatcherV2;
use fuzzy_matcher::FuzzyMatcher;
use std::ffi::OsString;
use std::io::Cursor;
use std::io::{self, IsTerminal, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;
use vte::{Params as VteParams, Parser as VteParser, Perform as VtePerform};

mod keyboard_enhancement;

use keyboard_enhancement::KeyboardEnhancementState;

pub fn parse() -> Cli {
    parse_args(std::env::args_os().collect()).unwrap_or_else(|err| err.exit())
}

pub async fn run(cli: Cli, paths: MiyuPaths) -> Result<()> {
    if cli.shell_classify {
        let shell_name = cli.shell.as_deref().unwrap_or("fish");
        let message = shell_message_from_input(cli.stdin, cli.message)?;
        return run_shell_classify(shell_name, &message);
    }

    if cli.clipboard_paste {
        return run_clipboard_paste(&paths);
    }
    // A log viewer must not append its own startup record to the file it is
    // about to display. Apart from being confusing, that made `-n 1` return
    // the viewer's initialization line instead of the daemon's latest event.
    let skip_diagnostic_logging = matches!(
        &cli.command,
        Some(Command::Daemon(DaemonArgs {
            command: Some(DaemonCommand::Logs(_)),
            ..
        }))
    );
    let _logging_guard = if skip_diagnostic_logging {
        None
    } else {
        match crate::logging::init(&paths, cli.debug) {
            Ok(guard) => Some(guard),
            Err(err) => {
                eprintln!(
                    "{}: {err:#}",
                    t(
                        "warning: diagnostic logging is unavailable",
                        "警告：诊断日志不可用"
                    )
                );
                None
            }
        }
    };
    let mode = AgentMode::Normal;

    if cli.shell_intercept {
        let shell_name = cli.shell.as_deref().unwrap_or("fish");
        let message = shell_message_from_input(cli.stdin, cli.message)?;
        return run_shell_intercept(&paths, shell_name, message).await;
    }

    if !paths.config_file.exists()
        && !matches!(
            cli.command,
            Some(Command::Init)
                | Some(Command::FishInit)
                | Some(Command::BashInit)
                | Some(Command::ZshInit)
                | Some(Command::RemoveShellHook)
                | Some(Command::Paths)
                | Some(Command::Import(_))
        )
    {
        run_init(&paths, InitKind::FirstRun)?;
    }

    // Captured before `cli.command` is moved out: one-shot entry points below
    // need them to pick the session their turn lands in.
    let session_arg = cli.session.clone();
    let continue_session = cli.continue_session;

    match cli.command {
        Some(Command::AlarmWorker(args)) => run_alarm_worker(args),
        Some(Command::DaemonWorker(args)) => {
            let _logging_guard = crate::logging::init(&paths, cli.debug).ok();
            // daemon 的 stdout/stderr 被重定向进 daemon.log，而 tracing 写的是
            // 另一个按天滚动的文件。出了事翻错文件是常态——排查一次长回复不转
            // 图片，我在 daemon.log 里绕了很久，真正的 warning 一直躺在
            // miyu.YYYY-MM-DD.log 里。所以在这条日志的开头指一次路。
            println!(
                "{}",
                crate::i18n::text(
                    "Detailed logs (warnings, tool failures) go to miyu.YYYY-MM-DD.log in the same directory; this file only carries startup output.",
                    "详细日志（警告、工具失败）在同目录的 miyu.YYYY-MM-DD.log；本文件只有启动输出。"
                )
            );
            crate::daemon::run(paths, args).await
        }
        Some(Command::Tool(args)) => run_tool(&paths, mode, args).await,
        Some(Command::Ask(args)) => {
            let session =
                one_shot_session(&paths, session_arg.as_deref(), continue_session).await?;
            run_chat_with_options(
                &paths,
                join_message(args.message),
                None,
                cli.stdout,
                mode,
                session,
            )
            .await
        }
        Some(Command::Init) => run_init(&paths, InitKind::Explicit),
        Some(Command::Paths) => {
            paths.print();
            Ok(())
        }
        Some(Command::Config(args)) => {
            let saved = run_config(&paths, args).await?;
            if saved && ipc::daemon_info(&paths).await.is_some() {
                reload_daemon_if_running(&paths).await
            } else {
                if saved {
                    let config = AppConfig::load_or_default(&paths)?;
                    if config.platforms.qq.enabled {
                        println!(
                            "{}",
                            t(
                                "Tencent QQ is enabled; run `miyu daemon start` to begin listening.",
                                "腾讯 QQ 已启用；执行 `miyu daemon start` 后开始监听。",
                            )
                        );
                    }
                }
                Ok(())
            }
        }
        Some(Command::Reload) => run_reload(&paths).await,
        Some(Command::Models(args)) => {
            initialize_models_cache(&paths);
            run_models(&paths, args).await
        }
        Some(Command::Export(args)) => run_export(&paths, args),
        Some(Command::Import(args)) => run_import(&paths, args).await,
        Some(Command::ListModels) => {
            initialize_models_cache(&paths);
            run_list_models(&paths)
        }
        Some(Command::Variant(args)) => {
            initialize_models_cache(&paths);
            run_variant(&paths, args)?;
            reload_daemon_if_running(&paths).await
        }
        Some(Command::FishInit) => shell::fish::install(&paths),
        Some(Command::BashInit) => shell::bash::install(&paths),
        Some(Command::ZshInit) => shell::zsh::install(&paths),
        Some(Command::RemoveShellHook) => remove_shell_hooks(&paths),
        Some(Command::History(args)) => run_history(&paths, args),
        Some(Command::Pop(args)) => {
            if ipc::daemon_info(&paths).await.is_some() {
                run_pop_via_daemon(&paths, args).await
            } else {
                run_pop(&paths, args)
            }
        }
        Some(Command::Kb(args)) => run_kb(&paths, args).await,
        Some(Command::UpdateDefaultKb) => run_update_default_kb(&paths).await,
        Some(Command::Memory(args)) => run_memory(&paths, args),
        Some(Command::Skills(args)) => run_skills(&paths, args),
        Some(Command::ResetMemoryCli) => run_reset_memory_command(&paths).await,
        Some(Command::Reset) => {
            if ipc::daemon_info(&paths).await.is_some() {
                send_ipc_admin(
                    &paths,
                    IpcCommand::ResetConversation {
                        target: crate::ipc::SessionRef::Current,
                    },
                )
                .await?;
            } else {
                run_reset(&paths).await?;
            }
            print_reset_message();
            Ok(())
        }
        Some(Command::Wipe(args)) => run_wipe(&paths, args.yes).await,
        Some(Command::ToolCallCmd(args)) => run_tool_call(&paths, args).await,
        Some(Command::McpServe) => run_mcp_serve(&paths).await,
        Some(Command::Normal) => run_repl(&paths, AgentMode::Normal).await,
        Some(Command::Dev) => run_repl(&paths, AgentMode::Dev).await,
        Some(Command::Web(args)) => run_web(&paths, args).await,
        Some(Command::Daemon(args)) => run_daemon_command(&paths, args).await,
        None => {
            let message = join_message(cli.message);
            if message.is_empty() && io::stdin().is_terminal() {
                if session_arg.is_some() || continue_session {
                    bail!(
                        "{}",
                        t(
                            "--session and --continue only apply to one-shot commands; use /session inside the REPL",
                            "--session 与 --continue 仅用于一次性命令；REPL 内请使用 /session 切换"
                        )
                    );
                }
                // 裸 miyu:按 default_mode 配置分流;未配置则打印模式说明,
                // 逼一次显式选择(miyu normal / miyu dev)。
                let default_mode = AppConfig::load_or_default(&paths)
                    .map(|config| config.default_mode.trim().to_ascii_lowercase())
                    .unwrap_or_default();
                match default_mode.as_str() {
                    "normal" => run_repl(&paths, AgentMode::Normal).await,
                    "dev" => run_repl(&paths, AgentMode::Dev).await,
                    "" => {
                        print_mode_help();
                        Ok(())
                    }
                    other => bail!(
                        "{}: {other}",
                        t(
                            "invalid default_mode (expected normal or dev)",
                            "default_mode 配置无效(应为 normal 或 dev)"
                        )
                    ),
                }
            } else {
                let session =
                    one_shot_session(&paths, session_arg.as_deref(), continue_session).await?;
                run_chat_with_options(&paths, message, None, cli.stdout, mode, session).await
            }
        }
    }
}

async fn run_repl(paths: &MiyuPaths, initial_mode: AgentMode) -> Result<()> {
    if direct_mode_requested() {
        run_direct_repl(paths, initial_mode).await
    } else {
        run_remote_repl(paths, initial_mode).await
    }
}

fn direct_mode_requested() -> bool {
    std::env::var_os("MIYU_DIRECT").is_some_and(|value| value != "0")
}

fn reload_repl_config(
    paths: &MiyuPaths,
    state: &StateStore,
    config: &mut AppConfig,
    client: &mut OpenAiCompatibleClient,
) -> Result<()> {
    *config = AppConfig::load(paths)?;
    apply_session_model_override(state, config);
    *client = OpenAiCompatibleClient::from_config(config, paths)?;
    Ok(())
}

const REPL_HISTORY_CAP: usize = 200;

/// 一个会话一个历史文件。
///
/// 以前是全局一个 `state/repl-history.jsonl`，所有会话混在一起——上键会翻出
/// 别的会话里敲的东西。会话 id 形如 `sess_1787036807476_a188fc33`，本来就是
/// 安全的文件名，但它来自库里的字符串，还是过一遍白名单：一个 `../` 就能把
/// 写入指到 state 目录外面去。
fn repl_history_file(paths: &MiyuPaths, session_id: &str) -> PathBuf {
    let safe = session_id
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
                ch
            } else {
                '_'
            }
        })
        .collect::<String>();
    paths
        .state_dir
        .join("repl-history")
        .join(format!("{safe}.jsonl"))
}

/// 分会话之前的那个全局文件。**只读不写**：老记录都在里面，直接丢掉用户会
/// 觉得「历史没了」。新条目一律写进会话文件。
fn legacy_repl_history_file(paths: &MiyuPaths) -> PathBuf {
    paths.state_dir.join("repl-history.jsonl")
}

fn read_repl_history_file(path: &std::path::Path) -> Vec<String> {
    let Ok(content) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    content
        .lines()
        .filter_map(|line| serde_json::from_str::<String>(line).ok())
        .filter(|entry| !entry.trim().is_empty())
        .collect()
}

/// Prompt history that survives /reset and restarts: a per-session
/// append-only file, capped on load. Conversation resets delete turns, so the
/// file is the durable source; the turns-derived list only seeds sessions that
/// predate it.
fn load_persistent_repl_history(paths: &MiyuPaths, session_id: &str) -> Vec<String> {
    let path = repl_history_file(paths, session_id);
    let mut entries = read_repl_history_file(&path);
    if entries.len() > REPL_HISTORY_CAP {
        entries = entries.split_off(entries.len() - REPL_HISTORY_CAP);
        // Opportunistic rewrite keeps the file from growing without bound.
        let rewritten = entries
            .iter()
            .filter_map(|entry| serde_json::to_string(entry).ok())
            .collect::<Vec<_>>()
            .join("\n");
        let _ = std::fs::write(&path, rewritten + "\n");
    }
    entries
}

/// 会话内输入历史的容量上限:REPL 常开数天时防无界增长,超限丢最老。
const REPL_HISTORY_LIMIT: usize = 500;

fn push_history_capped(history: &mut Vec<String>, content: &str) {
    history.push(content.to_string());
    if history.len() > REPL_HISTORY_LIMIT {
        let excess = history.len() - REPL_HISTORY_LIMIT;
        history.drain(..excess);
    }
}

fn persist_repl_history_entry(paths: &MiyuPaths, session_id: &str, entry: &str) {
    let entry = entry.trim();
    if entry.is_empty() {
        return;
    }
    let Ok(line) = serde_json::to_string(entry) else {
        return;
    };
    let path = repl_history_file(paths, session_id);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .and_then(|mut file| {
            use std::io::Write as _;
            writeln!(file, "{line}")
        });
}

struct LiveSubmission {
    content: String,
    display_content: String,
    images: Vec<Option<crate::clipboard::PastedImage>>,
}

struct LiveAgentInput<'a> {
    content: &'a str,
    images: &'a [Option<crate::clipboard::PastedImage>],
}

fn queued_prompt_lines(prompts: &[QueuedPrompt], mode: AgentMode, cols: usize) -> Vec<String> {
    let mut lines = Vec::new();
    for (index, prompt) in prompts.iter().enumerate() {
        if index > 0 {
            lines.push(String::new());
        }
        lines.extend(submitted_echo_lines(mode, &prompt.display_content, cols));
        lines.push(format!(
            "{} {}",
            submitted_echo_bar(mode),
            primary_footer_text(t("Queued", "排队中"))
        ));
    }
    lines
}

fn write_committed_user_messages(messages: &[(&str, AgentMode)], leading_gap: bool) -> Result<()> {
    write_committed_user_messages_from(messages, leading_gap, None)
}

/// `known_col`:调用方已知的当前光标列。提交路径的同步块内禁止 ESC[6n
/// 查询(等应答会让 kitty 同步超时、提前提交半成品帧——光标闪屏),
/// suspend 之后列是确定的,直接传进来。
fn write_committed_user_messages_from(
    messages: &[(&str, AgentMode)],
    leading_gap: bool,
    known_col: Option<u16>,
) -> Result<()> {
    if messages.is_empty() {
        return Ok(());
    }
    let mut stdout = io::stdout();
    let col = known_col.unwrap_or_else(|| cursor_col_or(0));
    if col > 0 {
        writeln!(stdout)?;
    }
    let cols = terminal_cols();
    write!(
        stdout,
        "{}",
        committed_user_messages_text(messages, leading_gap, cols)
    )?;
    stdout.flush()?;
    Ok(())
}

fn committed_user_messages_text(
    messages: &[(&str, AgentMode)],
    leading_gap: bool,
    cols: usize,
) -> String {
    let mut output = String::new();
    if leading_gap {
        output.push('\n');
    }
    for (index, (content, mode)) in messages.iter().enumerate() {
        if index > 0 {
            output.push('\n');
        }
        for line in submitted_echo_lines(*mode, content, cols) {
            output.push_str(&line);
            output.push('\n');
        }
    }
    output.push('\n');
    output
}

/// Redraws finished turns of a session as one ANSI frame.
///
/// Feeds the stored transcript back through the same `StreamRenderer` a live
/// turn uses, so tool blocks and prose come out identical — and re-wrapped for
/// the terminal's *current* width, which a saved byte transcript could not do.
/// Turns older than the transcript column fall back to prompt + final reply.
fn session_replay_frame(
    replays: &[crate::state::TurnReplay],
    mode: AgentMode,
    config: &AppConfig,
    cols: usize,
) -> Result<Vec<u8>> {
    use crate::state::ReplayEntry;
    let mut frame = Vec::new();
    for replay in replays {
        if replay.display_content.starts_with("[目标续轮]") {
            // 目标续轮什么都不画——实时渲染也不打表头。一个长任务几十轮，
            // 每轮一行只会把真正的输出挤散。
        } else if replay.is_synthetic {
            // daemon 自己合成的轮：实时渲染画的是一条暗色 `⚙` 提示，回放要
            // 对齐，不能变成用户气泡。
            frame.extend_from_slice(
                format!(
                    "\n\x1b[2m⚙ {}\x1b[0m\n\n",
                    job_wake_headline(&replay.display_content)
                )
                .as_bytes(),
            );
        } else if !replay.display_content.trim().is_empty() {
            frame.extend_from_slice(
                committed_user_messages_text(&[(&replay.display_content, mode)], true, cols)
                    .as_bytes(),
            );
        }
        let mut renderer = render::StreamRenderer::new(
            render::ReasoningDisplayMode::Hidden,
            render::ToolCallDisplayMode::from_config(&config.display.tool_calls),
            false,
            config.display.readable_tool_names,
            config.display.command_output_lines,
        );
        renderer.use_external_cursor_control();
        renderer.use_buffered_output();
        if replay.entries.is_empty() {
            renderer.write_chunk(ChatStreamChunk {
                kind: crate::llm::ChatStreamKind::Content,
                text: replay.assistant_content.clone(),
            })?;
        } else {
            for entry in &replay.entries {
                match entry {
                    ReplayEntry::Text { text } => renderer.write_chunk(ChatStreamChunk {
                        kind: crate::llm::ChatStreamKind::Content,
                        text: text.clone(),
                    })?,
                    ReplayEntry::ToolCall { name, arguments } => {
                        renderer.write_tool_call(name, arguments)?
                    }
                    ReplayEntry::ToolResult { name, ok, output } => {
                        renderer.write_tool_result(name, *ok, output)?
                    }
                }
            }
        }
        renderer.finish()?;
        frame.extend_from_slice(&renderer.take_output_frame());
    }
    Ok(frame)
}

fn queued_prompt_attachments(
    images: &[Option<crate::clipboard::PastedImage>],
) -> Vec<QueuedPromptAttachment> {
    images
        .iter()
        .filter_map(|image| match image {
            Some(crate::clipboard::PastedImage::Binary(image)) => {
                Some(QueuedPromptAttachment::Binary {
                    mime: image.mime.clone(),
                    data_base64: base64::engine::general_purpose::STANDARD.encode(&image.data),
                })
            }
            Some(crate::clipboard::PastedImage::Path(path)) => {
                Some(QueuedPromptAttachment::Path { path: path.clone() })
            }
            None => None,
        })
        .collect()
}

fn persist_queued_submission(
    state: &StateStore,
    submission: &LiveSubmission,
) -> Result<QueuedPrompt> {
    let prompt_id = format!(
        "queued_{}_{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_millis())
            .unwrap_or(0),
        rand::random::<u16>()
    );
    state.enqueue_prompt(
        &prompt_id,
        &submission.content,
        &submission.display_content,
        &queued_prompt_attachments(&submission.images),
    )
}

/// Queues a submission for the turn currently running in the daemon, using
/// the cross-process queue target so the daemon consumes it mid-turn.
async fn persist_remote_queued_submission(
    paths: &MiyuPaths,
    run_id: &str,
    turn_id: &str,
    submission: &LiveSubmission,
) -> Result<QueuedPrompt> {
    let mut stream = ipc::connect(&paths.ipc_socket()).await?;
    ipc::send(
        &mut stream,
        &IpcRequest::new(IpcCommand::QueueTurnUpdate {
            run_id: run_id.to_string(),
            turn_id: turn_id.to_string(),
            content: submission.content.clone(),
            display_content: submission.display_content.clone(),
            images: ipc_images(&submission.images),
            supersede: false,
        }),
    )
    .await?;
    match ipc::receive::<IpcFrame>(&mut stream).await? {
        Some(IpcFrame::TurnUpdateAccepted {
            prompt_id,
            seq,
            submitted_at,
            ..
        }) => Ok(QueuedPrompt {
            prompt_id,
            seq,
            content: submission.content.clone(),
            display_content: submission.display_content.clone(),
            attachments: queued_prompt_attachments(&submission.images),
            uploaded_attachments: Vec::new(),
            submitted_at,
        }),
        Some(IpcFrame::Error { message, .. }) => bail!("{message}"),
        Some(_) => bail!("Miyu core returned an invalid queue response"),
        None => bail!("Miyu core closed the queue connection"),
    }
}

struct ReplCursorRestore;

impl Drop for ReplCursorRestore {
    fn drop(&mut self) {
        // 1. 会话级兜底：恢复括号粘贴与光标
        // 2. 再关闭 raw mode；键盘增强由 LiveRawMode / 局部输入作用域负责 Pop
        let _ = execute!(
            io::stdout(),
            DisableBracketedPaste,
            DisableFocusChange,
            Show
        );
        let _ = terminal::disable_raw_mode();
    }
}

#[cfg(unix)]
fn restore_live_output_processing() -> Result<()> {
    let mut attributes = std::mem::MaybeUninit::<libc::termios>::uninit();
    // Raw input is required for key events, but renderer output still relies on newline translation.
    unsafe {
        if libc::tcgetattr(libc::STDOUT_FILENO, attributes.as_mut_ptr()) != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        let mut attributes = attributes.assume_init();
        attributes.c_oflag |= libc::OPOST | libc::ONLCR;
        if libc::tcsetattr(libc::STDOUT_FILENO, libc::TCSANOW, &attributes) != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
    }
    Ok(())
}

#[cfg(not(unix))]
fn restore_live_output_processing() -> Result<()> {
    Ok(())
}

/// 终端已死(PTY 对端关闭):POLLHUP/POLLERR/POLLNVAL 任一命中。
/// 不发 SIGHUP 的断开路径(tmux kill-pane、终端崩溃、SSH 掉线)只能靠它
/// 兜底——否则 crossterm 的 poll 对 EOF fd 永远立即就绪、read 又读不出
/// 事件,REPL 主循环全速空转,留下一个 98% CPU 的残留进程。
/// 挂断看门狗:独立线程每 500ms 裸 poll 探测 stdin 挂断,确认后给优雅
/// 退出路径 5 秒宽限——主线程若卡死在 crossterm 对 HUP fd 的任何内部
/// 自旋(事件 poll、CPR 应答等待,均为实测形态),由这里强制收尾,
/// 保证关终端后绝不留下吃 CPU 的残留进程。
pub(crate) fn spawn_hangup_watchdog() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        std::thread::spawn(|| loop {
            std::thread::sleep(Duration::from_millis(500));
            if terminal_hangup() {
                std::thread::sleep(Duration::from_secs(5));
                if terminal_hangup() {
                    std::process::exit(1);
                }
            }
        });
    });
}

fn terminal_hangup() -> bool {
    let mut pollfd = libc::pollfd {
        fd: libc::STDIN_FILENO,
        events: 0,
        revents: 0,
    };
    let ready = unsafe { libc::poll(&mut pollfd, 1, 0) };
    ready == 1 && (pollfd.revents & (libc::POLLHUP | libc::POLLERR | libc::POLLNVAL)) != 0
}

enum LiveReplOutcome {
    Exit,
    Submit(
        AgentMode,
        String,
        Vec<Option<crate::clipboard::PastedImage>>,
    ),
    /// A daemon-initiated wake turn is running in this session; the caller
    /// should attach and render it live.
    FollowWake {
        run_id: String,
        label: String,
    },
    /// Ctrl+C on an empty line while this session has background work: stop
    /// the work and stay in the REPL. Pressing it again then exits.
    StopJobs,
}

fn repl_history_is_clean(
    input: &str,
    history: &[String],
    history_clean_index: Option<usize>,
) -> bool {
    history_clean_index
        .and_then(|index| history.get(index))
        .map(|entry| entry == input)
        .unwrap_or(false)
}

fn repl_should_browse_history(
    input: &str,
    history: &[String],
    history_clean_index: Option<usize>,
) -> bool {
    input.is_empty() || repl_history_is_clean(input, history, history_clean_index)
}

fn run_history(paths: &MiyuPaths, args: HistoryArgs) -> Result<()> {
    let state = StateStore::new(paths)?;
    run_history_with_state(&state, args)
}

fn run_history_with_state(state: &StateStore, args: HistoryArgs) -> Result<()> {
    for entry in state.history(args.limit)? {
        if args.raw {
            println!("{}", serde_json::to_string(&entry)?);
            continue;
        }
        let display_role = if entry.role.ends_with("_clarification") {
            entry.role.trim_end_matches("_clarification")
        } else {
            entry.role.as_str()
        };
        println!("{} {display_role}", entry.timestamp);
        if entry.role.starts_with("assistant") {
            let response = crate::llm::ChatResult {
                content: entry.content,
                reasoning: if args.no_thinking {
                    None
                } else {
                    entry.reasoning
                },
                usage: None,
                usage_estimated: false,
                tool_calls: Vec::new(),
                provider_id: None,
                model: None,
                finish_reason: None,
                thinking_signature: None,
                last_request_usage: None,
                responses_continuation: None,
            };
            render::print_assistant_response(&response, !args.no_thinking)?;
        } else {
            println!("{}", entry.content);
        }
        println!();
    }
    Ok(())
}

#[cfg(test)]
mod default_kb_progress_tests {
    use super::*;

    #[test]
    fn progress_is_emitted_as_a_complete_line() {
        let stage = crate::default_kb::UpdateStage::FetchingRepository;
        let mut output = Vec::new();

        write_default_kb_update_progress(&mut output, stage).unwrap();

        assert_eq!(
            String::from_utf8(output).unwrap(),
            format!("[default-kb] {}\n", stage.message())
        );
    }
}

fn join_message(parts: Vec<String>) -> String {
    parts.join(" ").trim().to_string()
}

fn handle_agent_event(renderer: &mut render::StreamRenderer, event: AgentEvent) -> Result<()> {
    match event {
        AgentEvent::TurnStarted { .. } => Ok(()),
        AgentEvent::RawReasoning(_) => Ok(()),
        AgentEvent::FlushJournal => Ok(()),
        // 单次输出模式没有常驻 footer,逐请求计量快照无处可画。
        AgentEvent::RoundUsage { .. } => Ok(()),
        AgentEvent::Chunk(chunk) => {
            renderer.write_chunk(chunk)?;
            renderer.tick_spinner()
        }
        AgentEvent::ReasoningStart { received_at } => renderer.start_reasoning_phase(received_at),
        AgentEvent::ReasoningReset { received_at } => renderer.reset_reasoning_phase(received_at),
        AgentEvent::ReasoningPartStart { received_at } => {
            renderer.start_reasoning_part(received_at)
        }
        AgentEvent::ReasoningPartEnd { received_at } => renderer.finish_reasoning_part(received_at),
        AgentEvent::ReasoningTitle(title) => {
            renderer.write_reasoning_title(&title)?;
            renderer.tick_spinner()
        }
        AgentEvent::ToolCall {
            name, arguments, ..
        } => {
            renderer.write_tool_call(&name, &arguments)?;
            renderer.tick_spinner()
        }
        AgentEvent::ToolPreparing { name, batch } => {
            renderer.write_tool_preparing(&name, batch)?;
            renderer.tick_spinner()
        }
        AgentEvent::ToolResult {
            name, ok, output, ..
        } => {
            renderer.write_tool_result(&name, ok, &output)?;
            renderer.tick_spinner()
        }
        AgentEvent::ToolProgress { name, message, .. } => {
            renderer.write_tool_progress(&name, &message)?;
            renderer.tick_spinner()
        }
        AgentEvent::CommandOutput {
            name,
            stream,
            chunk,
            ..
        } => {
            renderer.write_command_output(&name, stream, &chunk)?;
            renderer.tick_spinner()
        }
        AgentEvent::PrepareForExternalOutput { ready } => {
            renderer.prepare_for_external_output()?;
            let _ = ready.send(true);
            Ok(())
        }
        AgentEvent::Image { .. } | AgentEvent::Artifact { .. } => Ok(()),
        AgentEvent::AskQuestion {
            request, responder, ..
        } => {
            renderer.prepare_for_external_output()?;
            let response = crate::question_tui::ask(&request).unwrap_or_else(|err| {
                crate::question::QuestionResponse::Unavailable(err.to_string())
            });
            if !matches!(&response, crate::question::QuestionResponse::Cancelled) {
                renderer.start_waiting()?;
            }
            let _ = responder.send(response);
            Ok(())
        }
        AgentEvent::QueuedPromptsConsumed { .. } => Ok(()),
        AgentEvent::GenerationSuperseded { .. } => Ok(()),
        AgentEvent::SpinnerTick => renderer.tick_spinner(),
        AgentEvent::CompactStart => {
            renderer.write_system_message(t("Compacting context...", "正在压缩上下文..."))?;
            renderer.tick_spinner()
        }
        AgentEvent::CompactChunk(chunk) => {
            renderer.write_compact_chunk(&chunk)?;
            renderer.tick_spinner()
        }
        AgentEvent::CompactEnd => {
            renderer.finish_compact()?;
            renderer.tick_spinner()
        }
        AgentEvent::PopStart => renderer.tick_spinner(),
        AgentEvent::PopEnd => renderer.tick_spinner(),
        AgentEvent::Notice { text } => {
            renderer.write_system_message(&text)?;
            renderer.tick_spinner()
        }
    }
}
