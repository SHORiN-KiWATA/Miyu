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
use crate::render;
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
    Event, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers,
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

const REPL_MAX_VISIBLE_INPUT_ROWS: u16 = 12;
const REPL_PASTE_PLACEHOLDER_MIN_LINES: usize = 3;
const REPL_PASTE_PLACEHOLDER_MIN_CHARS: usize = 150;
const RELOAD_MAX_ATTEMPTS: usize = 12;
const RELOAD_RETRY_INTERVAL: Duration = Duration::from_secs(5);
const RELOAD_RESPONSE_TIMEOUT: Duration = Duration::from_secs(60);
#[derive(Clone, Debug)]
struct PastedText {
    text: String,
}

#[derive(Clone, Debug)]
struct ReplFooterStatus {
    provider: String,
    model: String,
    mixed_models: bool,
    thinking: Option<String>,
    token_usage: render::TokenMeter,
}

/// The daemon reports Σ as three flat numbers; regroup them for the meters.
fn state_cumulative(state: &ipc::SessionState) -> TurnTokens {
    TurnTokens {
        total: state.cumulative_tokens,
        prompt: state.cumulative_prompt_tokens,
        cache_read: state.cumulative_cache_read_tokens,
    }
}

/// Σ is hidden entirely when nothing has been spent yet, so an empty session
/// does not carry a "Σ0" that means nothing.
fn meter_cumulative(cumulative: TurnTokens) -> render::TokenMeter {
    render::TokenMeter {
        cumulative_tokens: (cumulative.total > 0).then_some(cumulative.total),
        cumulative_prompt_tokens: cumulative.prompt,
        cumulative_cached_tokens: cumulative.cache_read,
        ..Default::default()
    }
}

impl ReplFooterStatus {
    fn from_config(config: &AppConfig, session_tokens: u64, cumulative: TurnTokens) -> Self {
        let active = config.active_provider_model_choices();
        let mixed_models = active.len() > 1;
        let (provider_id, model) = match active.as_slice() {
            [] => ("-".to_string(), t("None", "无").to_string()),
            [choice] => (
                choice.provider_id.clone(),
                short_model_name(&choice.model, &choice.provider_id),
            ),
            _ => ("mixed".to_string(), t("Mixed", "混合").to_string()),
        };

        Self {
            model,
            provider: provider_id,
            mixed_models,
            thinking: None,
            token_usage: render::TokenMeter {
                session_tokens,
                context_window: config.active_context_window().ok().flatten(),
                ..meter_cumulative(cumulative)
            },
        }
    }

    fn update_token_usage(
        &mut self,
        result: &crate::llm::ChatResult,
        session_tokens: u64,
        context_window: Option<usize>,
        cumulative: TurnTokens,
    ) {
        if result.usage.is_some() {
            let turn = TurnTokens::from_usage(result.usage.as_ref());
            self.set_token_usage_with_cache(turn, session_tokens, context_window, cumulative);
        }
    }

    fn set_token_usage(
        &mut self,
        turn_tokens: u64,
        session_tokens: u64,
        context_window: Option<usize>,
        cumulative: TurnTokens,
    ) {
        self.set_token_usage_with_cache(
            TurnTokens {
                total: turn_tokens,
                ..TurnTokens::default()
            },
            session_tokens,
            context_window,
            cumulative,
        );
    }

    fn set_token_usage_with_cache(
        &mut self,
        turn: TurnTokens,
        session_tokens: u64,
        context_window: Option<usize>,
        cumulative: TurnTokens,
    ) {
        self.token_usage = render::TokenMeter {
            turn_tokens: turn.total,
            turn_prompt_tokens: turn.prompt,
            turn_cached_tokens: turn.cache_read,
            session_tokens,
            context_window,
            ..meter_cumulative(cumulative)
        };
    }

    fn update_session_tokens(&mut self, session_tokens: u64) {
        self.token_usage.session_tokens = session_tokens;
    }

    fn update_context_window(&mut self, context_window: Option<usize>) {
        self.token_usage.context_window = context_window;
    }

    /// Returns whether anything actually moved, so an idle tick only forces a
    /// redraw when the numbers changed.
    fn update_cumulative_tokens(&mut self, cumulative: TurnTokens) -> bool {
        let meter = meter_cumulative(cumulative);
        let changed = self.token_usage.cumulative_tokens != meter.cumulative_tokens
            || self.token_usage.cumulative_prompt_tokens != meter.cumulative_prompt_tokens
            || self.token_usage.cumulative_cached_tokens != meter.cumulative_cached_tokens;
        self.token_usage.cumulative_tokens = meter.cumulative_tokens;
        self.token_usage.cumulative_prompt_tokens = meter.cumulative_prompt_tokens;
        self.token_usage.cumulative_cached_tokens = meter.cumulative_cached_tokens;
        changed
    }

    fn reset_token_usage(&mut self, session_tokens: u64, context_window: Option<usize>) {
        self.token_usage = render::TokenMeter {
            session_tokens,
            context_window,
            ..Default::default()
        };
    }

    fn update_thinking_variant(&mut self, variant: Option<&str>) {
        self.thinking = if self.mixed_models {
            None
        } else {
            variant.map(str::to_string)
        };
    }
}

fn short_model_name(model: &str, provider: &str) -> String {
    model
        .strip_prefix(&format!("{provider}/"))
        .unwrap_or(model)
        .rsplit('/')
        .next()
        .unwrap_or(model)
        .to_string()
}

/// crossterm asks the terminal where the cursor is (`ESC[6n`) and gives up if
/// the reply does not arrive within a fixed wait. Over a laggy SSH link that
/// wait expires routinely, and every `?` on it used to take the whole REPL
/// down with "The cursor position could not be read within a normal duration".
/// The answer is only ever used to re-anchor a redraw, so a stale one costs a
/// single imperfect frame — losing the session costs the session.
fn cursor_position_or(fallback: (u16, u16)) -> (u16, u16) {
    cursor::position().unwrap_or(fallback)
}

fn cursor_row_or(fallback: u16) -> u16 {
    cursor_position_or((0, fallback)).1
}

fn cursor_col_or(fallback: u16) -> u16 {
    cursor_position_or((fallback, 0)).0
}

fn repl_footer_line(mode: AgentMode, footer: &ReplFooterStatus, cols: usize) -> String {
    let cols = cols.max(1);
    let bar = input_prompt_bar(mode);
    let bar_width = visible_width(&bar);
    // The footer carries only the two standing gauges — how much context is
    // left, and what the session has cost. The per-turn figure is transient and
    // already has its own home in the `Token:` line printed after each reply;
    // keeping it here cost 14 columns and pushed the whole footer past 80.
    let usage = render::TokenMeter {
        turn_tokens: 0,
        ..footer.token_usage
    };
    // Narrow terminals: drop the cumulative total first, then the percent,
    // so the core context meter survives as long as possible.
    let mut right_plain = String::new();
    for (with_cumulative, with_percent) in [(true, true), (false, true), (false, false)] {
        let meter = render::TokenMeter {
            cumulative_tokens: usage.cumulative_tokens.filter(|_| with_cumulative),
            ..usage
        };
        right_plain = render::format_token_usage_inline_opts(&meter, with_percent);
        let left_room = cols
            .saturating_sub(bar_width)
            .saturating_sub(visible_width(&right_plain));
        if left_room >= 24 {
            break;
        }
    }
    let right = format!("\x1b[2m{right_plain}\x1b[0m");
    let right_width = visible_width(&right);
    let left_budget = cols.saturating_sub(bar_width.saturating_add(right_width).saturating_add(1));
    let left = repl_footer_left(mode, footer, left_budget);
    let gap = cols
        .saturating_sub(
            bar_width
                .saturating_add(visible_width(&left))
                .saturating_add(right_width),
        )
        .max(1);
    format!("{bar}{left}{}{right}", " ".repeat(gap))
}

fn repl_footer_left(mode: AgentMode, footer: &ReplFooterStatus, width: usize) -> String {
    let thinking = footer.thinking.as_deref().unwrap_or_default();
    let colored_thinking = (!thinking.is_empty()).then(|| primary_footer_text(thinking));
    let colored_thinking = colored_thinking.as_deref().unwrap_or_default();
    let provider = format!("\x1b[2m{}\x1b[0m", footer.provider);
    let mode = colored_footer_mode_label(mode);
    let full = repl_footer_left_parts(&mode, &footer.model, Some(&provider), colored_thinking);
    if visible_width(&full) <= width {
        return full;
    }

    let compact = repl_footer_left_parts(&mode, &footer.model, None, colored_thinking);
    if visible_width(&compact) <= width {
        return compact;
    }

    let fixed_width =
        visible_width(&mode)
            .saturating_add(3)
            .saturating_add(if thinking.is_empty() {
                0
            } else {
                3 + visible_width(colored_thinking)
            });
    let model_budget = width.saturating_sub(fixed_width).max(1);
    let model = truncate_display(&footer.model, model_budget);
    repl_footer_left_parts(&mode, &model, None, colored_thinking)
}

fn repl_footer_left_parts(
    mode: &str,
    model: &str,
    provider: Option<&str>,
    thinking: &str,
) -> String {
    let mut endpoint = model.to_string();
    if let Some(provider) = provider.filter(|provider| !provider.is_empty()) {
        if !endpoint.is_empty() {
            endpoint.push(' ');
        }
        endpoint.push_str(provider);
    }
    let mut parts = vec![mode.to_string(), endpoint];
    if !thinking.is_empty() {
        parts.push(thinking.to_string());
    }
    parts.join(" · ")
}

fn print_mixed_model_endpoint(show: bool, result: &crate::llm::ChatResult, variant: Option<&str>) {
    if !show {
        return;
    }
    let provider = result.provider_id.as_deref().unwrap_or("-");
    let model = result.model.as_deref().unwrap_or("-");
    println!(
        "\x1b[2m{}\x1b[0m\n",
        mixed_model_endpoint_label(provider, model, variant)
    );
}

fn mixed_model_endpoint_label(provider: &str, model: &str, variant: Option<&str>) -> String {
    let variant = variant
        .filter(|variant| !variant.is_empty())
        .map(|variant| format!(" · {variant}"))
        .unwrap_or_default();
    format!("{provider} / {model}{variant}")
}

fn show_mixed_model_endpoint(config: &AppConfig, interactive: bool) -> bool {
    config.active_provider_model_choices().len() > 1
        && match config.display.mixed_model_endpoint_display.as_str() {
            "off" => false,
            "all" => true,
            _ => interactive,
        }
}

fn colored_footer_mode_label(mode: AgentMode) -> String {
    let label = mode.label();
    match mode {
        AgentMode::Normal => primary_footer_text(label),
        AgentMode::Chat => format!("\x1b[1m\x1b[32m{label}\x1b[0m"),
    }
}

fn primary_footer_text(text: &str) -> String {
    format!("\x1b[1m\x1b[34m{text}\x1b[0m")
}

#[derive(Debug, Parser)]
#[command(name = "miyu", version, about = "Miyu CLI AI Agent")]
pub struct Cli {
    #[arg(long, global = true)]
    pub debug: bool,

    #[arg(long)]
    pub stdout: bool,

    /// 仅为本次命令指定目标会话（名称或编号），不改变全局当前会话
    #[arg(long)]
    pub session: Option<String>,

    #[arg(short = 'c', long = "continue", conflicts_with = "session")]
    pub continue_session: bool,

    #[arg(long, hide = true)]
    pub shell_intercept: bool,

    #[arg(long, hide = true)]
    pub shell_classify: bool,

    #[arg(long, hide = true)]
    pub shell: Option<String>,

    #[arg(long, hide = true)]
    pub stdin: bool,

    #[arg(long, hide = true)]
    pub clipboard_paste: bool,

    #[command(subcommand)]
    pub command: Option<Command>,

    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    pub message: Vec<String>,
}

pub fn parse() -> Cli {
    parse_args(std::env::args_os().collect()).unwrap_or_else(|err| err.exit())
}

fn parse_args(mut args: Vec<OsString>) -> std::result::Result<Cli, clap::Error> {
    let debug = extract_debug_flag(&mut args);
    let matches = localized_command().try_get_matches_from(args)?;
    let web_port_explicit = matches
        .subcommand_matches("web")
        .and_then(|web| web.value_source("port"))
        == Some(clap::parser::ValueSource::CommandLine);
    let mut cli = Cli::from_arg_matches(&matches)?;
    if let Some(Command::Web(args)) = &mut cli.command {
        args.port_explicit = web_port_explicit;
    }
    cli.debug |= debug;
    Ok(cli)
}

fn extract_debug_flag(args: &mut Vec<OsString>) -> bool {
    let mut debug = false;
    let mut index = 1;
    while index < args.len() {
        if args[index] == "--" {
            break;
        }
        if args[index] == "--debug" {
            args.remove(index);
            debug = true;
        } else {
            index += 1;
        }
    }
    debug
}

fn localized_command() -> clap::Command {
    let mut command = Cli::command();
    command = command
        .about(t("Miyu CLI AI Agent", "Miyu 命令行 AI 助手"))
        .override_usage(t(
            "miyu [OPTIONS] [MESSAGE]... [COMMAND]",
            "miyu [选项] [消息]... [命令]",
        ));
    if is_zh() {
        command = command
            .subcommand_help_heading("命令")
            .arg_required_else_help(false)
            .next_help_heading("选项")
            .help_template("{about}\n\n用法: {usage}\n\n命令:\n{subcommands}\n参数:\n{positionals}\n选项:\n{options}\n{after-help}")
            .after_help("提示：不带参数进入 REPL；直接输入消息会发送一次对话。可在配置界面设置语言，MIYU_LANG 可临时覆盖。")
            .disable_help_subcommand(true);
    } else {
        command = command
            .after_help(
                "Tip: run without arguments to enter the REPL; pass MESSAGE to send one chat turn. Set the language in the configuration UI; MIYU_LANG is a temporary override.",
            )
            .disable_help_subcommand(true);
    }
    command = localize_top_args(command);
    command = localize_subcommands(command);
    command = apply_localized_help_flags(command, true);
    if is_zh() {
        command = apply_chinese_help_template(command);
    }
    command
}

fn apply_localized_help_flags(mut command: clap::Command, root: bool) -> clap::Command {
    command = command.disable_help_flag(true).arg(
        Arg::new("help")
            .short('h')
            .long("help")
            .help(t("Print help", "显示帮助"))
            .action(ArgAction::Help),
    );
    if root {
        command = command.disable_version_flag(true).arg(
            Arg::new("version")
                .short('V')
                .long("version")
                .help(t("Print version", "显示版本"))
                .action(ArgAction::Version),
        );
    }
    let subcommands = command
        .get_subcommands()
        .map(|subcommand| subcommand.get_name().to_string())
        .collect::<Vec<_>>();
    for name in subcommands {
        command = command.mut_subcommand(&name, |subcommand| {
            apply_localized_help_flags(subcommand, false)
        });
    }
    command
}

fn apply_chinese_help_template(mut command: clap::Command) -> clap::Command {
    let has_subcommands = command.get_subcommands().next().is_some();
    command = if has_subcommands {
        command.help_template(
            "{about}\n\n用法: {usage}\n\n命令:\n{subcommands}\n参数:\n{positionals}\n选项:\n{options}\n{after-help}",
        )
    } else {
        command.help_template(
            "{about}\n\n用法: {usage}\n\n参数:\n{positionals}\n选项:\n{options}\n{after-help}",
        )
    };
    let subcommands = command
        .get_subcommands()
        .map(|subcommand| subcommand.get_name().to_string())
        .collect::<Vec<_>>();
    for name in subcommands {
        command = command.mut_subcommand(&name, apply_chinese_help_template);
    }
    command
}

fn localize_top_args(command: clap::Command) -> clap::Command {
    command
        .mut_arg("debug", |arg| {
            arg.help(t(
                "Write detailed diagnostics to the Miyu log directory",
                "将详细诊断信息写入 Miyu 日志目录",
            ))
        })
        .mut_arg("stdout", |arg| {
            arg.help(t(
                "Plain output mode (no colors, no TUI); pipe-friendly for stdout redirection",
                "纯文本输出模式（无颜色、无 TUI）；适合管道重定向",
            ))
        })
        .mut_arg("continue_session", |arg| {
            arg.help(t(
                "Send this one-shot message to the terminal session instead of a throwaway one",
                "把这条一次性消息发进终端会话，而不是用完即弃的临时会话",
            ))
        })
        .mut_arg("message", |arg| {
            arg.help(t(
                "Message to send; omitted to enter REPL",
                "要发送的消息；省略则进入 REPL",
            ))
        })
}

fn localize_subcommands(mut command: clap::Command) -> clap::Command {
    let descriptions = [
        (
            "ask",
            "Send one message to the assistant",
            "向助手发送一条消息",
        ),
        (
            "init",
            "Create default config and state files; use <shell>-init for shell hooks",
            "创建默认配置和状态文件；Shell 集成请使用对应的 <shell>-init 命令",
        ),
        (
            "paths",
            "Show app config, data, and cache paths",
            "显示应用配置、数据和缓存路径",
        ),
        ("config", "Open or manage configuration", "打开或管理配置"),
        (
            "reload",
            "Reload configuration in the running Miyu daemon",
            "在运行中的 Miyu daemon 内重新加载配置",
        ),
        (
            "models",
            "Switch the current session's model",
            "切换当前会话使用的模型",
        ),
        (
            "list-models",
            "List the global model pool with indexes",
            "列出全局模型池并编号",
        ),
        (
            "variant",
            "View or switch thinking level",
            "查看或切换思考档位",
        ),
        (
            "fish-init",
            "Integrate with fish so you can chat in natural language directly in the terminal",
            "集成到 fish，集成后可在终端直接使用自然语言交流。",
        ),
        (
            "bash-init",
            "Integrate with bash so you can chat in natural language directly in the terminal",
            "集成到 bash，集成后可在终端直接使用自然语言交流。",
        ),
        (
            "zsh-init",
            "Integrate with zsh so you can chat in natural language directly in the terminal",
            "集成到 zsh，集成后可在终端直接使用自然语言交流。",
        ),
        (
            "remove-shell-hook",
            "Safely remove installed Miyu shell hooks",
            "安全删除已安装的 Miyu shell hook",
        ),
        ("history", "Show conversation history", "显示会话历史"),
        (
            "pop",
            "Move conversation turns out of active context",
            "将对话轮次移出当前上下文",
        ),
        ("kb", "Manage local knowledge base", "管理本地知识库"),
        (
            "update-default-kb",
            "Update Miyu default knowledge base",
            "更新 Miyu 默认知识库",
        ),
        (
            "memory",
            "Inspect or edit assistant memory",
            "查看或编辑助手记忆",
        ),
        ("skills", "Manage assistant skills", "管理助手 skills"),
        (
            "reset",
            "Start the current conversation over",
            "重新开始当前会话",
        ),
        (
            "wipe",
            "Erase memory, all conversations, group contexts and generated skills",
            "抹掉记忆、所有会话内容、群聊上下文和自动技能",
        ),
        (
            "new",
            "Create a new session and switch to it",
            "创建新会话并切换过去",
        ),
        (
            "session",
            "List sessions, or switch to one (Ctrl+D deletes in the picker)",
            "列出会话，或切换到指定会话（菜单内 Ctrl+D 删除）",
        ),
        ("rename", "Rename the current session", "重命名当前会话"),
        ("archive", "Archive the current session", "归档当前会话"),
        (
            "delete",
            "Delete a session (current by default)",
            "删除会话（默认当前会话）",
        ),
        (
            "workspace",
            "Show, bind, or unbind the session workspace",
            "查看、绑定或解绑会话工作目录",
        ),
        ("web", "Open the local Miyu WebUI", "访问本地 Miyu WebUI"),
        (
            "daemon",
            "Manage the unified Miyu background service",
            "管理 Miyu 统一后台服务",
        ),
        (
            "export",
            "Pack this installation into a portable archive",
            "把当前安装打包成可移植归档",
        ),
        (
            "import",
            "Restore an installation from an exported archive",
            "从导出的归档恢复安装",
        ),
    ];
    for (name, en, zh) in descriptions {
        command = command.mut_subcommand(name, |subcommand| subcommand.about(t(en, zh)));
    }
    command = command
        .mut_subcommand("ask", localize_ask_command)
        .mut_subcommand("models", localize_models_command)
        .mut_subcommand("variant", localize_variant_command)
        .mut_subcommand("history", localize_history_command)
        .mut_subcommand("pop", localize_pop_command)
        .mut_subcommand("kb", localize_kb_command)
        .mut_subcommand("memory", localize_memory_command)
        .mut_subcommand("skills", localize_skills_command)
        .mut_subcommand("config", localize_config_command)
        .mut_subcommand("web", localize_web_command)
        .mut_subcommand("daemon", localize_daemon_command)
        .mut_subcommand("export", localize_export_command)
        .mut_subcommand("import", localize_import_command);
    command
}

fn localize_export_command(command: clap::Command) -> clap::Command {
    command
        .mut_arg("output", |arg| {
            arg.help(t(
                "Archive path to write; omit to name it after this host and time",
                "要写入的归档路径；省略则按主机名与时间自动命名",
            ))
        })
        .mut_arg("all", |arg| {
            arg.help(t(
                "Include everything portable, index and platform history included",
                "包含全部可移植数据，含向量索引与平台历史",
            ))
        })
        .mut_arg("index", |arg| {
            arg.help(t(
                "Include the knowledge-base vector index (large; rebuildable with `miyu kb embed`)",
                "包含知识库向量索引（很大；可用 miyu kb embed 重建）",
            ))
        })
        .mut_arg("platforms", |arg| {
            arg.help(t(
                "Include chat-platform history",
                "包含通讯平台的聊天历史",
            ))
        })
        .mut_arg("no_secrets", |arg| {
            arg.help(t(
                "Blank out API keys and tokens (you must refill them after importing)",
                "清空 API key 与访问令牌（导入后需要自行补填）",
            ))
        })
        .mut_arg("dry_run", |arg| {
            arg.help(t(
                "Print what would be packed without writing an archive",
                "只打印将要打包的内容，不实际写归档",
            ))
        })
        .mut_arg("force", |arg| {
            arg.help(t("Overwrite an existing archive", "覆盖已存在的归档文件"))
        })
}

fn localize_import_command(command: clap::Command) -> clap::Command {
    command
        .mut_arg("archive", |arg| {
            arg.help(t("Archive produced by `miyu export`", "miyu export 生成的归档"))
        })
        .mut_arg("force", |arg| {
            arg.help(t(
                "Overwrite existing data (the current installation is backed up first)",
                "覆盖已有数据（覆盖前会先备份当前安装）",
            ))
        })
}

fn localize_ask_command(command: clap::Command) -> clap::Command {
    command.mut_arg("message", |arg| {
        arg.help(t("Message to send", "要发送的消息"))
    })
}

fn localize_models_command(command: clap::Command) -> clap::Command {
    command.mut_arg("target", |arg| {
        arg.help(t(
            "List index, provider/model, or 'default' to follow the global pool",
            "模型列表序号、供应商/模型名，或 default 恢复跟随全局模型池",
        ))
    })
}

fn localize_variant_command(command: clap::Command) -> clap::Command {
    command.mut_arg("name", |arg| {
        arg.help(t(
            "Thinking level to select; omit to choose interactively",
            "要选择的思考档位；省略则进入交互选择",
        ))
    })
}

fn localize_history_command(command: clap::Command) -> clap::Command {
    command
        .mut_arg("limit", |arg| {
            arg.help(t("Number of history entries to show", "显示的历史条数"))
        })
        .mut_arg("raw", |arg| {
            arg.help(t("Print raw JSONL entries", "输出原始 JSONL 条目"))
        })
        .mut_arg("no_thinking", |arg| {
            arg.help(t("Hide stored reasoning", "隐藏已保存的思考内容"))
        })
}

fn localize_pop_command(command: clap::Command) -> clap::Command {
    command.mut_arg("count", |arg| {
        arg.help(t(
            "Number of oldest turns to pop; omit to select interactively",
            "要弹出的最旧轮次数；省略则进入交互多选",
        ))
    })
}

fn localize_config_command(command: clap::Command) -> clap::Command {
    command
        .mut_subcommand("validate", |subcommand| {
            subcommand.about(t("Validate configuration", "校验配置"))
        })
        .mut_subcommand("paths", |subcommand| {
            subcommand.about(t("Show configuration paths", "显示配置路径"))
        })
}

fn localize_web_command(command: clap::Command) -> clap::Command {
    command
        .mut_arg("port", |arg| arg.help(t("Local TCP port", "本地 TCP 端口")))
        .mut_arg("password", |arg| {
            arg.help(t(
                "Prompt securely for a required password",
                "安全输入所需的访问密码",
            ))
        })
        .mut_arg("password_file", |arg| {
            arg.help(t(
                "Read the WebUI password from a file",
                "从文件读取 WebUI 访问密码",
            ))
        })
}

fn localize_daemon_command(mut command: clap::Command) -> clap::Command {
    let descriptions = [
        (
            "start",
            "Start all configured Miyu interfaces",
            "启动所有已配置的 Miyu 接口",
        ),
        (
            "stop",
            "Stop the Miyu background service",
            "停止 Miyu 后台服务",
        ),
        (
            "restart",
            "Restart the Miyu background service",
            "重启 Miyu 后台服务",
        ),
        (
            "status",
            "Show daemon and interface status",
            "显示 daemon 与接口状态",
        ),
        ("logs", "Follow daemon logs", "持续查看 daemon 日志"),
    ];
    for (name, en, zh) in descriptions {
        command = command.mut_subcommand(name, |subcommand| subcommand.about(t(en, zh)));
    }
    command
        .mut_arg("port", |arg| {
            arg.help(t("WebUI TCP port", "WebUI TCP 端口"))
        })
        .mut_subcommand("logs", |subcommand| {
            subcommand.mut_arg("lines", |arg| {
                arg.help(t(
                    "Print only the most recent N lines and exit",
                    "仅输出最近 N 行后退出",
                ))
            })
        })
}

fn localize_kb_command(mut command: clap::Command) -> clap::Command {
    let descriptions = [
        ("add", "Add a file or directory", "添加文件或目录"),
        ("list", "List indexed files", "列出已索引文件"),
        ("search", "Search knowledge base content", "搜索知识库内容"),
        ("find", "Find files by name", "按文件名查找文件"),
        ("read", "Read a knowledge base file", "读取知识库文件"),
        ("remove", "Remove a knowledge base file", "移除知识库文件"),
        (
            "reindex",
            "Rebuild keyword index on demand",
            "按需重建关键词索引",
        ),
        ("stats", "Show knowledge base statistics", "显示知识库统计"),
        ("embed", "Manage semantic embeddings", "管理语义嵌入"),
    ];
    for (name, en, zh) in descriptions {
        command = command.mut_subcommand(name, |subcommand| subcommand.about(t(en, zh)));
    }
    command
        .mut_subcommand("add", |subcommand| {
            subcommand
                .mut_arg("path", |arg| arg.help(t("Path to add", "要添加的路径")))
                .mut_arg("recursive", |arg| {
                    arg.help(t(
                        "Compatibility flag; directories are recursive by default",
                        "兼容参数；目录默认递归导入",
                    ))
                })
        })
        .mut_subcommand("search", |subcommand| {
            subcommand
                .mut_arg("query", |arg| arg.help(t("Search query", "搜索查询")))
                .mut_arg("limit", |arg| arg.help(t("Maximum results", "最大结果数")))
        })
        .mut_subcommand("find", |subcommand| {
            subcommand
                .mut_arg("query", |arg| arg.help(t("Filename query", "文件名查询")))
                .mut_arg("limit", |arg| arg.help(t("Maximum results", "最大结果数")))
        })
        .mut_subcommand("read", |subcommand| {
            subcommand
                .mut_arg("file", |arg| {
                    arg.help(t("Knowledge base file name", "知识库文件名"))
                })
                .mut_arg("start", |arg| arg.help(t("Starting line", "起始行")))
                .mut_arg("lines", |arg| arg.help(t("Number of lines", "读取行数")))
        })
        .mut_subcommand("remove", |subcommand| {
            subcommand.mut_arg("file", |arg| arg.help(t("File to remove", "要移除的文件")))
        })
}

fn localize_memory_command(mut command: clap::Command) -> clap::Command {
    let descriptions = [
        ("stats", "Show memory statistics", "显示记忆统计"),
        ("reset", "Clear assistant memory", "清空助手记忆"),
        ("search", "Search memories", "搜索记忆"),
        ("remember", "Save a manual fact", "手动保存事实"),
    ];
    for (name, en, zh) in descriptions {
        command = command.mut_subcommand(name, |subcommand| subcommand.about(t(en, zh)));
    }
    command
        .mut_subcommand("reset", |subcommand| {
            subcommand.mut_arg("include_skills", |arg| {
                arg.help(t(
                    "Also remove generated skills",
                    "同时移除自动生成的 skills",
                ))
            })
        })
        .mut_subcommand("search", |subcommand| {
            subcommand
                .mut_arg("query", |arg| arg.help(t("Search query", "搜索查询")))
                .mut_arg("limit", |arg| arg.help(t("Maximum results", "最大结果数")))
                .mut_arg("forgotten", |arg| {
                    arg.help(t("Include forgotten memories", "包含已遗忘记忆"))
                })
        })
        .mut_subcommand("remember", |subcommand| {
            subcommand
                .mut_arg("content", |arg| arg.help(t("Fact content", "事实内容")))
                .mut_arg("source", |arg| arg.help(t("Source label", "来源标签")))
        })
}

fn localize_skills_command(mut command: clap::Command) -> clap::Command {
    let descriptions = [
        ("list", "List skills", "列出 skills"),
        ("show", "Show a skill", "显示 skill"),
        ("enable", "Enable a skill", "启用 skill"),
        ("disable", "Disable a skill", "禁用 skill"),
        ("remove", "Remove a skill", "移除 skill"),
        ("stats", "Show skill statistics", "显示 skill 统计"),
        (
            "prune",
            "Remove disabled generated skills",
            "清理已禁用的自动 skills",
        ),
    ];
    for (name, en, zh) in descriptions {
        command = command.mut_subcommand(name, |subcommand| subcommand.about(t(en, zh)));
    }
    for name in ["show", "enable", "disable", "remove"] {
        command = command.mut_subcommand(name, |subcommand| {
            subcommand.mut_arg("name", |arg| arg.help(t("Skill name", "skill 名称")))
        });
    }
    command
}

#[derive(Debug, Subcommand)]
pub enum Command {
    #[command(name = "__alarm-worker", hide = true)]
    AlarmWorker(AlarmWorkerArgs),
    #[command(name = "__tool", hide = true)]
    Tool(ToolArgs),
    /// Internal: run as the Miyu daemon (spawned by the CLI via
    /// `current_exe`, replacing the former separate `miyud` binary).
    #[command(name = "__daemon", hide = true)]
    DaemonWorker(WebArgs),
    Ask(MessageArgs),
    Init,
    Paths,
    Config(ConfigArgs),
    Reload,
    Models(ModelsArgs),
    ListModels,
    Variant(VariantArgs),
    FishInit,
    BashInit,
    ZshInit,
    RemoveShellHook,
    History(HistoryArgs),
    Pop(PopArgs),
    Kb(KbArgs),
    Export(ExportArgs),
    Import(ImportArgs),
    UpdateDefaultKb,
    Memory(MemoryArgs),
    Skills(SkillsArgs),
    Reset,
    Wipe(WipeArgs),
    New(SessionNameArgs),
    Session(SessionTargetArgs),
    Rename(SessionRenameArgs),
    Archive,
    Delete(SessionTargetArgs),
    Workspace(WorkspaceArgs),
    Web(WebArgs),
    Daemon(DaemonArgs),
}

#[derive(Debug, Args)]
pub struct SessionNameArgs {
    pub name: Option<String>,
}

#[derive(Debug, Args)]
pub struct SessionTargetArgs {
    pub target: Option<String>,
}

#[derive(Debug, Args)]
pub struct SessionRenameArgs {
    pub name: String,
}

#[derive(Debug, Args)]
pub struct WorkspaceArgs {
    pub path: Option<String>,
}

#[derive(Debug, Args)]
pub struct MessageArgs {
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    pub message: Vec<String>,
}

#[derive(Debug, Args)]
pub struct WipeArgs {
    /// 跳过确认（供 shell hook 等非交互场景使用）。
    #[arg(long)]
    pub yes: bool,
}

#[derive(Args)]
pub struct WebArgs {
    #[arg(long, default_value_t = ipc::DEFAULT_WEB_PORT)]
    pub port: u16,

    /// WebUI 监听地址；默认 0.0.0.0（所有网卡），127.0.0.1 仅限本机访问。
    #[arg(long, value_name = "ADDR")]
    pub bind: Option<std::net::IpAddr>,

    #[arg(short = 'p', long, num_args = 0, default_missing_value = "")]
    pub password: Option<String>,

    #[arg(long, value_name = "PATH", conflicts_with = "password")]
    pub password_file: Option<PathBuf>,

    #[arg(skip)]
    pub port_explicit: bool,
}

#[derive(Debug, Args)]
pub struct DaemonArgs {
    #[arg(long, value_name = "PORT", global = true)]
    pub port: Option<u16>,

    #[command(subcommand)]
    pub command: Option<DaemonCommand>,
}

#[derive(Debug, Subcommand)]
pub enum DaemonCommand {
    Start,
    Stop,
    Restart,
    Status,
    Logs(DaemonLogsArgs),
}

#[derive(Debug, Args)]
pub struct DaemonLogsArgs {
    #[arg(short = 'n', long, value_name = "N")]
    pub lines: Option<usize>,
}

impl std::fmt::Debug for WebArgs {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("WebArgs")
            .field("port", &self.port)
            .field("password", &self.password.as_ref().map(|_| "<redacted>"))
            .field("password_file", &self.password_file)
            .field("port_explicit", &self.port_explicit)
            .finish()
    }
}

#[derive(Debug, Args)]
pub struct AlarmWorkerArgs {
    #[arg(long)]
    pub id: String,
    #[arg(long)]
    pub time: String,
    #[arg(long, default_value = "Miyu alarm")]
    pub label: String,
    #[arg(long)]
    pub state_dir: PathBuf,
    #[arg(long)]
    pub cache_dir: PathBuf,
    #[arg(long)]
    pub audio_file: Option<PathBuf>,
}

#[derive(Debug, Args)]
pub struct ToolArgs {
    pub name: String,
    pub arguments: Option<String>,
}

#[derive(Debug, Args)]
pub struct ConfigArgs {
    #[command(subcommand)]
    pub command: Option<ConfigCommand>,
}

#[derive(Debug, Args)]
pub struct HistoryArgs {
    #[arg(short, long, default_value_t = 20)]
    pub limit: usize,

    #[arg(long)]
    pub raw: bool,

    #[arg(long)]
    pub no_thinking: bool,
}

#[derive(Debug, Args)]
pub struct PopArgs {
    #[arg(value_parser = parse_positive_pop_count)]
    pub count: Option<usize>,
}

#[derive(Debug, Args)]
pub struct ExportArgs {
    /// Where to write the archive; defaults to a host- and time-stamped name
    /// in the current directory.
    pub output: Option<PathBuf>,
    #[arg(long)]
    pub all: bool,
    #[arg(long)]
    pub index: bool,
    #[arg(long)]
    pub platforms: bool,
    #[arg(long)]
    pub no_secrets: bool,
    #[arg(long)]
    pub dry_run: bool,
    #[arg(long)]
    pub force: bool,
}

#[derive(Debug, Args)]
pub struct ImportArgs {
    pub archive: PathBuf,
    #[arg(long)]
    pub force: bool,
}

#[derive(Debug, Args)]
pub struct ModelsArgs {
    /// 1-based list index, `provider/model`, a bare model name, or
    /// `default` to follow the global active pool again.
    pub target: Option<String>,
}

#[derive(Debug, Args)]
pub struct VariantArgs {
    pub name: Option<String>,
}

#[derive(Debug, Args)]
pub struct KbArgs {
    #[command(subcommand)]
    pub command: KbCommand,
}

#[derive(Debug, Args)]
pub struct MemoryArgs {
    #[command(subcommand)]
    pub command: MemoryCommand,
}

#[derive(Debug, Subcommand)]
pub enum MemoryCommand {
    Stats,
    Reset(MemoryResetArgs),
    Search(MemorySearchArgs),
    Remember(MemoryRememberArgs),
}

#[derive(Debug, Args)]
pub struct MemoryResetArgs {
    #[arg(long)]
    pub include_skills: bool,
}

#[derive(Debug, Args)]
pub struct MemorySearchArgs {
    pub query: Vec<String>,
    #[arg(short, long)]
    pub limit: Option<usize>,
    #[arg(long)]
    pub forgotten: bool,
}

#[derive(Debug, Args)]
pub struct MemoryRememberArgs {
    pub content: Vec<String>,
    #[arg(short, long, default_value = "manual")]
    pub source: String,
}

#[derive(Debug, Args)]
pub struct SkillsArgs {
    #[command(subcommand)]
    pub command: SkillsCommand,
}

#[derive(Debug, Subcommand)]
pub enum SkillsCommand {
    List,
    Show(SkillNameArgs),
    Enable(SkillNameArgs),
    Disable(SkillNameArgs),
    Remove(SkillNameArgs),
    Stats,
    Prune,
}

#[derive(Debug, Args)]
pub struct SkillNameArgs {
    pub name: String,
}

#[derive(Debug, Subcommand)]
pub enum KbCommand {
    Add(KbAddArgs),
    List,
    Search(KbSearchArgs),
    Find(KbFindArgs),
    Read(KbReadArgs),
    Remove(KbRemoveArgs),
    Reindex,
    Stats,
    Embed(KbEmbedArgs),
}

#[derive(Debug, Args)]
pub struct KbAddArgs {
    pub path: PathBuf,
    #[arg(
        short,
        long,
        help = "Compatibility flag; directories are recursive by default"
    )]
    pub recursive: bool,
}

#[derive(Debug, Args)]
pub struct KbSearchArgs {
    pub query: Vec<String>,
    #[arg(short, long)]
    pub limit: Option<usize>,
}

#[derive(Debug, Args)]
pub struct KbFindArgs {
    pub query: Vec<String>,
    #[arg(short, long)]
    pub limit: Option<usize>,
}

#[derive(Debug, Args)]
pub struct KbReadArgs {
    pub file: String,
    #[arg(long, default_value_t = 1)]
    pub start: usize,
    #[arg(long)]
    pub lines: Option<usize>,
}

#[derive(Debug, Args)]
pub struct KbRemoveArgs {
    pub file: String,
}

#[derive(Debug, Args)]
pub struct KbEmbedArgs {
    #[command(subcommand)]
    pub command: KbEmbedCommand,
}

#[derive(Debug, Subcommand)]
pub enum KbEmbedCommand {
    Reindex(KbEmbedReindexArgs),
}

#[derive(Debug, Args)]
pub struct KbEmbedReindexArgs {
    #[arg(long)]
    pub quiet: bool,
}

#[derive(Debug, Subcommand)]
pub enum ConfigCommand {
    Validate,
    Paths,
    #[command(hide = true)]
    PromptSource,
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
            crate::daemon::run(paths, args).await
        }
        Some(Command::Tool(args)) => run_tool(&paths, mode, args).await,
        Some(Command::Ask(args)) => {
            let session = one_shot_session(&paths, session_arg.as_deref(), continue_session).await?;
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
        Some(Command::New(args)) => run_session_new(&paths, args).await,
        Some(Command::Session(args)) => run_session_command(&paths, args).await,
        Some(Command::Rename(args)) => run_session_rename(&paths, args).await,
        Some(Command::Archive) => run_session_archive(&paths).await,
        Some(Command::Delete(args)) => run_session_delete(&paths, args).await,
        Some(Command::Workspace(args)) => run_workspace_command(&paths, args).await,
        Some(Command::Web(args)) => run_web(&paths, args).await,
        Some(Command::Daemon(args)) => run_daemon_command(&paths, args).await,
        None => {
            let message = join_message(cli.message.clone());
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
                if let Ok(config) = AppConfig::load_or_default(&paths) {
                    if !config.has_configured_llm() {
                        println!(
                            "{}\n",
                            t(
                                "No active provider API key configured. Launching interactive configuration...",
                                "检测到尚未配置有效的大模型 API Key，即将为你自动打开交互配置界面..."
                            )
                        );
                        std::thread::sleep(Duration::from_millis(1200));
                        crate::config_tui::run(&paths)?;
                    }
                }
                let res = run_repl(&paths, mode).await;
                #[cfg(windows)]
                {
                    if io::stdout().is_terminal() {
                        let mut line = String::new();
                        println!("\n{}", t("Press Enter to exit...", "按下 Enter 键退出程序..."));
                        let _ = io::stdin().read_line(&mut line);
                    }
                }
                res
            } else {
                let session =
                    one_shot_session(&paths, session_arg.as_deref(), continue_session).await?;
                run_chat_with_options(&paths, message, None, cli.stdout, mode, session).await
            }
        }
    }
}

fn initialize_models_cache(paths: &MiyuPaths) {
    crate::models_cache::try_load(paths);
    crate::models_cache::spawn_background_refresh(paths.clone());
    if let Ok(config) = AppConfig::load_or_default(paths) {
        crate::models_cache::spawn_provider_api_refresh(config.providers);
    }
}

async fn run_web(paths: &MiyuPaths, mut args: WebArgs) -> Result<()> {
    if let Some(info) = ipc::daemon_info(paths).await {
        if info.build_id == ipc::BUILD_ID {
            if args.port_explicit || args.password.is_some() || args.password_file.is_some() {
                bail!(
                    "{}",
                    t(
                        "the running Miyu daemon already owns Web settings; restart it to change them",
                        "当前 Miyu daemon 已接管 Web 设置；如需修改请先重启 daemon"
                    )
                );
            }
            for url in daemon_web_access_urls(&info) {
                println!("Miyu WebUI: {url}");
            }
            return Ok(());
        }
    }

    if args.password.as_deref() == Some("") {
        args.password = Some(rpassword::prompt_password(t(
            "WebUI password: ",
            "WebUI 密码：",
        ))?);
    }
    let launch = web_launch_config(paths, &args)?;
    let info = ipc::ensure_daemon(paths, launch.as_ref()).await?;
    for url in daemon_web_access_urls(&info) {
        println!("Miyu WebUI: {url}");
    }
    Ok(())
}

fn web_launch_config(paths: &MiyuPaths, args: &WebArgs) -> Result<Option<ipc::DaemonLaunchConfig>> {
    if !args.port_explicit
        && args.bind.is_none()
        && args.password.is_none()
        && args.password_file.is_none()
    {
        return Ok(None);
    }
    let password_file = match args.password.as_deref() {
        Some("") => bail!(
            "{}",
            t("WebUI password cannot be empty", "WebUI 密码不能为空")
        ),
        Some(password) if password.chars().count() > 1_024 => bail!(
            "{}",
            t(
                "WebUI password cannot exceed 1,024 characters",
                "WebUI 密码不能超过 1,024 个字符"
            )
        ),
        Some(password) => Some(ipc::stage_managed_web_password(paths, password)?),
        None => args
            .password_file
            .as_deref()
            .map(|path| ipc::stage_web_password_file(paths, path))
            .transpose()?,
    };
    Ok(Some(ipc::DaemonLaunchConfig {
        port: args.port,
        password_file,
        bind: args.bind,
    }))
}

fn daemon_web_access_urls(info: &ipc::DaemonInfo) -> Vec<String> {
    let bind = info
        .web_bind
        .unwrap_or(std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED));
    ipc::web_access_urls_for(bind, info.web_port)
}

async fn run_daemon_command(paths: &MiyuPaths, args: DaemonArgs) -> Result<()> {
    let command = args.command.unwrap_or(DaemonCommand::Start);
    if args.port.is_some() && !matches!(command, DaemonCommand::Start | DaemonCommand::Restart) {
        bail!(
            "{}",
            t(
                "--port only applies to daemon start or restart",
                "--port 仅适用于 daemon start 或 restart"
            )
        );
    }

    match command {
        DaemonCommand::Start => {
            let launch = args
                .port
                .map(|port| ipc::daemon_launch_config_with_port(paths, port))
                .transpose()?;
            if launch.is_some()
                && ipc::daemon_info(paths)
                    .await
                    .is_some_and(|info| info.build_id == ipc::BUILD_ID)
            {
                bail!(
                    "{}",
                    t(
                        "the running Miyu daemon already owns Web settings; use `miyu daemon restart` to change the port",
                        "当前 Miyu daemon 已接管 Web 设置；如需修改端口请使用 `miyu daemon restart`"
                    )
                );
            }
            ipc::ensure_daemon(paths, launch.as_ref()).await?;
            let refreshed = MiyuPaths::new()?;
            print_daemon_status(&refreshed).await
        }
        DaemonCommand::Stop => stop_daemon(paths).await,
        DaemonCommand::Restart => {
            let pending_launch = if let Some(port) = args.port {
                Some(ipc::daemon_launch_config_with_port(paths, port)?)
            } else {
                match ipc::daemon_info(paths).await {
                    Some(info) => ipc::recover_daemon_launch_if_missing(paths, info.pid)?,
                    None => None,
                }
            };
            if let Err(error) = stop_daemon(paths).await {
                if let Some(launch) = &pending_launch {
                    ipc::discard_daemon_launch_candidate(paths, launch);
                }
                return Err(error);
            };
            let refreshed = match MiyuPaths::new() {
                Ok(paths) => paths,
                Err(error) => {
                    if let Some(launch) = &pending_launch {
                        ipc::discard_daemon_launch_candidate(paths, launch);
                    }
                    return Err(error);
                }
            };
            if let Err(error) = ipc::ensure_daemon(&refreshed, pending_launch.as_ref()).await {
                if let Some(launch) = &pending_launch {
                    ipc::discard_daemon_launch_candidate(&refreshed, launch);
                }
                return Err(error);
            }
            print_daemon_status(&refreshed).await
        }
        DaemonCommand::Status => print_daemon_status(paths).await,
        DaemonCommand::Logs(args) => run_daemon_logs(paths, args).await,
    }
}

async fn stop_daemon(paths: &MiyuPaths) -> Result<()> {
    let Some(info) = ipc::daemon_info(paths).await else {
        println!("{}", t("Miyu daemon is not running", "Miyu daemon 未运行"));
        return Ok(());
    };
    ipc::shutdown_daemon(paths, &info).await?;
    println!("{}", t("Miyu daemon stopped", "Miyu daemon 已停止"));
    Ok(())
}

async fn print_daemon_status(paths: &MiyuPaths) -> Result<()> {
    let Some(info) = ipc::daemon_info(paths).await else {
        println!("{}", t("Miyu daemon: stopped", "Miyu daemon：已停止"));
        return Ok(());
    };
    let (_, data) = send_ipc_admin(paths, IpcCommand::GetStatus).await?;
    println!(
        "{} {} (PID {})",
        t("Miyu daemon:", "Miyu daemon："),
        t("running", "运行中"),
        info.pid,
    );
    for line in
        daemon_web_status_lines(t("WebUI:", "WebUI："), &daemon_web_access_urls(&info))
    {
        println!("{line}");
    }
    let engine = data
        .pointer("/runtime/turn_engine")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("ready");
    println!("{} {}", t("Turn engine:", "TurnEngine："), engine);

    let qq = data.pointer("/platforms/qq");
    let enabled = qq
        .and_then(|value| value.get("enabled"))
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    if !enabled {
        println!(
            "{} {}",
            t("Tencent QQ:", "腾讯 QQ："),
            t("disabled", "未启用")
        );
        return Ok(());
    }
    let port = qq
        .and_then(|value| value.get("listen_port"))
        .and_then(serde_json::Value::as_u64)
        .unwrap_or_default();
    let accounts = qq
        .and_then(|value| value.get("connected_accounts"))
        .and_then(serde_json::Value::as_array)
        .map(|values| {
            values
                .iter()
                .filter_map(serde_json::Value::as_i64)
                .map(|value| value.to_string())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let connection = if accounts.is_empty() {
        t("not connected", "尚未连接").to_string()
    } else {
        format!("{}: {}", t("connected", "已连接"), accounts.join(", "))
    };
    println!(
        "{} ws://localhost:{port}/ws · {connection}",
        t("Tencent QQ:", "腾讯 QQ：")
    );
    Ok(())
}

fn daemon_web_status_lines(label: &str, urls: &[String]) -> Vec<String> {
    let Some((first, remaining)) = urls.split_first() else {
        return vec![label.to_string()];
    };
    let indent = " ".repeat(visible_width(label).saturating_add(1));
    std::iter::once(format!("{label} {first}"))
        .chain(remaining.iter().map(|url| format!("{indent}{url}")))
        .collect()
}

async fn run_daemon_logs(paths: &MiyuPaths, args: DaemonLogsArgs) -> Result<()> {
    let ansi = io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none();
    if let Some(lines) = args.lines {
        if !(1..=100_000).contains(&lines) {
            bail!(
                "{}",
                t(
                    "--lines must be between 1 and 100000",
                    "--lines 必须在 1 到 100000 之间"
                )
            );
        }
        let snapshot = recent_daemon_log_snapshot(paths, lines)?;
        write_daemon_log_lines(&snapshot.lines, ansi)?;
        return Ok(());
    }

    let snapshot = recent_daemon_log_snapshot(paths, 50)?;
    write_daemon_log_lines(&snapshot.lines, ansi)?;
    // The cursor is tied to the exact EOF captured by the snapshot reader.
    // Bytes appended while the snapshot is printed or while the status probe
    // runs are therefore consumed by follow instead of being skipped.
    let cursor = snapshot.cursor;
    let Some(daemon) = ipc::daemon_info(paths).await else {
        bail!("{}", t("Miyu daemon is not running", "Miyu daemon 未运行"));
    };
    follow_daemon_log(paths, ansi, cursor, daemon.pid).await
}

#[derive(Debug, Eq, PartialEq)]
struct ParsedDaemonLogLine<'a> {
    timestamp: &'a str,
    level: &'a str,
    module: &'a str,
    message: &'a str,
}

fn parse_daemon_log_line(line: &str) -> Option<ParsedDaemonLogLine<'_>> {
    let timestamp_end = line.find(char::is_whitespace)?;
    let timestamp = &line[..timestamp_end];
    DateTime::parse_from_rfc3339(timestamp).ok()?;

    let remainder = line[timestamp_end..].trim_start();
    let level_end = remainder.find(char::is_whitespace)?;
    let level = &remainder[..level_end];
    if !matches!(level, "TRACE" | "DEBUG" | "INFO" | "WARN" | "ERROR") {
        return None;
    }

    let remainder = remainder[level_end..].trim_start();
    let (module, message) = remainder
        .split_once(": ")
        .filter(|(candidate, _)| is_miyu_log_target(candidate))
        .unwrap_or(("miyu", remainder));
    Some(ParsedDaemonLogLine {
        timestamp,
        level,
        module,
        message,
    })
}

fn is_miyu_log_target(value: &str) -> bool {
    value == "miyu"
        || value
            .strip_prefix("miyu::")
            .is_some_and(|suffix| !suffix.is_empty())
}

fn format_daemon_log_line(line: &str, ansi: bool) -> String {
    let mut decision_color = None;
    format_daemon_log_line_with_state(line, ansi, &mut decision_color)
}

fn format_daemon_log_line_with_state(
    line: &str,
    ansi: bool,
    decision_color: &mut Option<Color>,
) -> String {
    let Some(parsed) = parse_daemon_log_line(line) else {
        if let Some(color) = active_reply_log_color(line) {
            *decision_color = Some(color);
        }
        return decision_color.map_or_else(
            || line.to_string(),
            |color| color_log_part(line.to_string(), color, ansi),
        );
    };
    *decision_color = active_reply_log_color(parsed.message);
    let timestamp = DateTime::parse_from_rfc3339(parsed.timestamp)
        .map(|value| {
            value
                .with_timezone(&Local)
                .format("%H:%M:%S%.3f")
                .to_string()
        })
        .unwrap_or_else(|_| parsed.timestamp.to_string());
    let timestamp = color_log_part(format!("[{timestamp}]"), Color::DarkGreen, ansi);
    let level = color_log_part(
        format!("[{}]", parsed.level),
        log_level_color(parsed.level),
        ansi,
    );
    let module = color_log_part(format!("[{}]", parsed.module), Color::DarkCyan, ansi);
    if parsed.message.is_empty() {
        format!("{timestamp} {level} {module}")
    } else {
        format!(
            "{timestamp} {level} {module} {}",
            decision_color.map_or_else(
                || parsed.message.to_string(),
                |color| color_log_part(parsed.message.to_string(), color, ansi),
            )
        )
    }
}

fn active_reply_log_color(value: &str) -> Option<Color> {
    match value.trim_start().lines().next().unwrap_or_default() {
        "【续聊窗口判断：回复】"
        | "【主动回复判断：回复】"
        | "[Continuation decision: reply]"
        | "[Active reply decision: reply]" => Some(Color::Green),
        "【续聊窗口判断：不回复】"
        | "【主动回复判断：不回复】"
        | "[Continuation decision: no reply]"
        | "[Active reply decision: no reply]" => Some(Color::DarkGrey),
        _ => None,
    }
}

fn log_level_color(level: &str) -> Color {
    match level {
        "ERROR" => Color::Red,
        "WARN" => Color::Yellow,
        "INFO" => Color::Green,
        "DEBUG" => Color::Cyan,
        _ => Color::DarkGrey,
    }
}

fn color_log_part(value: String, color: Color, ansi: bool) -> String {
    if ansi {
        format!("{}", value.with(color))
    } else {
        value
    }
}

fn write_daemon_log_lines(lines: &[String], ansi: bool) -> Result<()> {
    let stdout = io::stdout();
    let mut output = stdout.lock();
    let mut decision_color = None;
    for line in lines {
        writeln!(
            output,
            "{}",
            format_daemon_log_line_with_state(line, ansi, &mut decision_color)
        )?;
    }
    output.flush()?;
    Ok(())
}

#[derive(Default)]
struct DaemonLogStreamFormatter {
    pending: Vec<u8>,
    decision_color: Option<Color>,
}

impl DaemonLogStreamFormatter {
    fn push(&mut self, bytes: &[u8], ansi: bool, output: &mut impl Write) -> io::Result<()> {
        self.pending.extend_from_slice(bytes);
        let Some(last_newline) = self.pending.iter().rposition(|byte| *byte == b'\n') else {
            return Ok(());
        };
        let remainder = self.pending.split_off(last_newline + 1);
        let complete = std::mem::replace(&mut self.pending, remainder);
        for mut line in complete[..last_newline].split(|byte| *byte == b'\n') {
            if line.last() == Some(&b'\r') {
                line = &line[..line.len() - 1];
            }
            write_daemon_log_line_bytes(line, ansi, &mut self.decision_color, output)?;
        }
        Ok(())
    }

    fn finish(&mut self, ansi: bool, output: &mut impl Write) -> io::Result<()> {
        if self.pending.is_empty() {
            return Ok(());
        }
        let mut line = std::mem::take(&mut self.pending);
        if line.last() == Some(&b'\r') {
            line.pop();
        }
        write_daemon_log_line_bytes(&line, ansi, &mut self.decision_color, output)
    }
}

fn write_daemon_log_line_bytes(
    line: &[u8],
    ansi: bool,
    decision_color: &mut Option<Color>,
    output: &mut impl Write,
) -> io::Result<()> {
    writeln!(
        output,
        "{}",
        format_daemon_log_line_with_state(&String::from_utf8_lossy(line), ansi, decision_color,)
    )
}

fn daemon_log_files(paths: &MiyuPaths) -> Result<Vec<PathBuf>> {
    let mut files = match std::fs::read_dir(paths.logs_dir()) {
        Ok(entries) => entries
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.path())
            .filter(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with("miyu.") && name.ends_with(".log"))
            })
            .collect::<Vec<_>>(),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Vec::new(),
        Err(error) => return Err(error.into()),
    };
    files.sort();
    // The daemon's inherited stdout/stderr goes to daemon.log. Keep it in
    // the history even when tracing has already created rolling log files;
    // startup failures and panics can happen before the tracing layer writes
    // anything useful.
    let fallback = paths.logs_dir().join("daemon.log");
    if fallback.is_file() && !files.iter().any(|path| path == &fallback) {
        // Treat the unstructured process stream as the oldest source for the
        // bounded recent view. The newest rolling file remains last.
        files.insert(0, fallback);
    }
    Ok(files)
}

fn is_daemon_fallback_log(path: &Path) -> bool {
    path.file_name().and_then(|name| name.to_str()) == Some("daemon.log")
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct DaemonLogFollowCursor {
    current: Option<PathBuf>,
    current_offset: u64,
    fallback: Option<PathBuf>,
    fallback_offset: u64,
}

#[derive(Debug, Eq, PartialEq)]
struct DaemonLogSnapshot {
    lines: Vec<String>,
    cursor: DaemonLogFollowCursor,
}

fn daemon_log_follow_cursor_for_files(
    files: &[PathBuf],
    offsets: &[(PathBuf, u64)],
) -> DaemonLogFollowCursor {
    let fallback = files
        .iter()
        .find(|path| is_daemon_fallback_log(path))
        .cloned();
    let current = files
        .iter()
        .rev()
        .find(|path| !is_daemon_fallback_log(path))
        .cloned()
        .or_else(|| fallback.clone());
    let file_offset = |path: Option<&PathBuf>| {
        path.and_then(|path| {
            offsets
                .iter()
                .find(|(candidate, _)| candidate == path)
                .map(|(_, offset)| *offset)
        })
        .unwrap_or_else(|| {
            path.and_then(|path| std::fs::metadata(path).ok())
                .map_or(0, |metadata| metadata.len())
        })
    };
    DaemonLogFollowCursor {
        current_offset: file_offset(current.as_ref()),
        fallback_offset: file_offset(fallback.as_ref()),
        current,
        fallback,
    }
}

fn recent_daemon_log_snapshot(paths: &MiyuPaths, limit: usize) -> Result<DaemonLogSnapshot> {
    let files = daemon_log_files(paths)?;
    if limit == 0 {
        return Ok(DaemonLogSnapshot {
            lines: Vec::new(),
            cursor: daemon_log_follow_cursor_for_files(&files, &[]),
        });
    }
    // Record the initial EOF of every source before reading any tails. A
    // writer that appends after this point is intentionally left for follow;
    // it can never be lost in the snapshot-to-follow hand-off.
    let mut offsets = files
        .iter()
        .map(|path| {
            let offset = std::fs::metadata(path).map_or(0, |metadata| metadata.len());
            (path.clone(), offset)
        })
        .collect::<Vec<_>>();
    let mut lines = Vec::with_capacity(limit.min(1024));
    for path in files.iter().rev() {
        let remaining = limit.saturating_sub(lines.len());
        if remaining == 0 {
            break;
        }
        let (mut file_lines, end_offset) = tail_file_lines_with_end(path, remaining)?;
        if let Some((_, offset)) = offsets.iter_mut().find(|(candidate, _)| candidate == path) {
            *offset = end_offset;
        }
        file_lines.extend(lines);
        lines = file_lines;
    }
    if lines.len() > limit {
        lines.drain(..lines.len() - limit);
    }
    Ok(DaemonLogSnapshot {
        lines,
        cursor: daemon_log_follow_cursor_for_files(&files, &offsets),
    })
}

fn recent_daemon_log_lines(paths: &MiyuPaths, limit: usize) -> Result<Vec<String>> {
    Ok(recent_daemon_log_snapshot(paths, limit)?.lines)
}

fn tail_file_lines_with_end(path: &Path, limit: usize) -> Result<(Vec<String>, u64)> {
    const CHUNK: usize = 8192;
    let mut file = std::fs::File::open(path)?;
    let mut position = file.seek(SeekFrom::End(0))?;
    let end_offset = position;
    let mut bytes = Vec::new();
    let mut newline_count = 0usize;
    while position > 0 && newline_count <= limit {
        let read_len = usize::try_from(position.min(CHUNK as u64)).unwrap_or(CHUNK);
        position -= read_len as u64;
        file.seek(SeekFrom::Start(position))?;
        let mut chunk = vec![0_u8; read_len];
        file.read_exact(&mut chunk)?;
        newline_count += chunk.iter().filter(|byte| **byte == b'\n').count();
        chunk.extend(bytes);
        bytes = chunk;
    }
    let text = String::from_utf8_lossy(&bytes);
    let mut lines = text.lines().map(str::to_string).collect::<Vec<_>>();
    if lines.len() > limit {
        lines.drain(..lines.len() - limit);
    }
    Ok((lines, end_offset))
}

#[derive(Debug, Eq, PartialEq)]
struct DaemonLogDelta {
    bytes: Vec<u8>,
    next_offset: u64,
    reset: bool,
}

fn read_daemon_log_delta(path: &Path, offset: u64) -> Result<DaemonLogDelta> {
    let mut file = std::fs::File::open(path)?;
    let len = file.metadata()?.len();
    let reset = len < offset;
    let start = if reset { 0 } else { offset };
    if len == start {
        return Ok(DaemonLogDelta {
            bytes: Vec::new(),
            next_offset: start,
            reset,
        });
    }
    file.seek(SeekFrom::Start(start))?;
    let mut bytes = Vec::with_capacity(usize::try_from(len - start).unwrap_or(0));
    file.read_to_end(&mut bytes)?;
    Ok(DaemonLogDelta {
        bytes,
        next_offset: file.stream_position()?,
        reset,
    })
}

fn write_daemon_log_delta(
    path: &Path,
    offset: &mut u64,
    formatter: &mut DaemonLogStreamFormatter,
    ansi: bool,
    output: &mut impl Write,
) -> Result<bool> {
    let delta = read_daemon_log_delta(path, *offset)?;
    if delta.reset {
        formatter.finish(ansi, output)?;
    }
    *offset = delta.next_offset;
    if delta.bytes.is_empty() {
        return Ok(false);
    }
    formatter.push(&delta.bytes, ansi, output)?;
    Ok(true)
}

#[cfg(unix)]
fn daemon_process_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    let result = unsafe { libc::kill(pid as libc::pid_t, 0) };
    result == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(not(unix))]
fn daemon_process_alive(_pid: u32) -> bool {
    // The IPC probe remains the authoritative check on platforms without a
    // portable process-existence primitive.
    true
}

fn finish_daemon_log_formatters(
    ansi: bool,
    current: Option<&PathBuf>,
    fallback: Option<&PathBuf>,
    formatter: &mut DaemonLogStreamFormatter,
    fallback_formatter: &mut DaemonLogStreamFormatter,
    output: &mut impl Write,
) -> io::Result<()> {
    if fallback == current {
        fallback_formatter.finish(ansi, output)?;
    } else {
        formatter.finish(ansi, output)?;
        fallback_formatter.finish(ansi, output)?;
    }
    output.flush()
}

async fn follow_daemon_log(
    paths: &MiyuPaths,
    ansi: bool,
    cursor: DaemonLogFollowCursor,
    initial_pid: u32,
) -> Result<()> {
    let mut current = cursor.current;
    let mut offset = cursor.current_offset;
    let mut fallback = cursor.fallback;
    let mut fallback_offset = cursor.fallback_offset;
    let mut interval = tokio::time::interval(Duration::from_millis(500));
    let mut formatter = DaemonLogStreamFormatter::default();
    let mut fallback_formatter = DaemonLogStreamFormatter::default();
    let mut known_pid = Some(initial_pid);
    let mut daemon_misses = 0_u8;

    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                let mut stdout = io::stdout().lock();
                finish_daemon_log_formatters(
                    ansi,
                    current.as_ref(),
                    fallback.as_ref(),
                    &mut formatter,
                    &mut fallback_formatter,
                    &mut stdout,
                )?;
                return Ok(());
            },
            _ = interval.tick() => {
                let files = daemon_log_files(paths)?;
                let latest = files
                    .iter()
                    .rev()
                    .find(|path| !is_daemon_fallback_log(path))
                    .cloned()
                    .or_else(|| files.iter().find(|path| is_daemon_fallback_log(path)).cloned());
                let latest_fallback = files.iter().find(|path| is_daemon_fallback_log(path)).cloned();
                if latest_fallback != fallback {
                    let mut stdout = io::stdout().lock();
                    if fallback.as_ref().is_some_and(|path| path.is_file()) {
                        write_daemon_log_delta(
                            fallback.as_ref().unwrap(),
                            &mut fallback_offset,
                            &mut fallback_formatter,
                            ansi,
                            &mut stdout,
                        )?;
                    }
                    fallback_formatter.finish(ansi, &mut stdout)?;
                    stdout.flush()?;
                    fallback = latest_fallback;
                    fallback_offset = 0;
                }
                if latest != current {
                    let mut stdout = io::stdout().lock();
                    if let Some(previous) = current
                        .as_ref()
                        .filter(|path| path.is_file() && Some(*path) != fallback.as_ref())
                    {
                        write_daemon_log_delta(
                            previous,
                            &mut offset,
                            &mut formatter,
                            ansi,
                            &mut stdout,
                        )?;
                    }
                    formatter.finish(ansi, &mut stdout)?;
                    stdout.flush()?;
                    current = latest;
                    offset = 0;
                }
                let mut changed = false;
                let mut stdout = io::stdout().lock();
                if let Some(path) = fallback.as_ref().filter(|path| path.is_file()) {
                    changed |= write_daemon_log_delta(
                        path,
                        &mut fallback_offset,
                        &mut fallback_formatter,
                        ansi,
                        &mut stdout,
                    )?;
                }
                if current.as_ref() != fallback.as_ref() {
                    if let Some(path) = current.as_ref().filter(|path| path.is_file()) {
                        changed |= write_daemon_log_delta(
                            path,
                            &mut offset,
                            &mut formatter,
                            ansi,
                            &mut stdout,
                        )?;
                    }
                }
                stdout.flush()?;
                drop(stdout);

                if changed {
                    daemon_misses = 0;
                    continue;
                }

                if let Some(info) = ipc::daemon_info(paths).await {
                    known_pid = Some(info.pid);
                    daemon_misses = 0;
                    continue;
                }

                // `daemon_info` deliberately has a short timeout. A busy
                // daemon can miss one or more probes while still being alive;
                // use the last known PID and a small grace window before
                // treating the stream as finished.
                let alive = known_pid.is_some_and(daemon_process_alive);
                let socket_exists = paths.ipc_socket().exists();
                if socket_exists && (alive || known_pid.is_none()) {
                    daemon_misses = 0;
                    continue;
                }
                daemon_misses = daemon_misses.saturating_add(1);
                if daemon_misses < 3 {
                    continue;
                }

                let mut stdout = io::stdout().lock();
                if let Some(path) = fallback.as_ref().filter(|path| path.is_file()) {
                    write_daemon_log_delta(
                        path,
                        &mut fallback_offset,
                        &mut fallback_formatter,
                        ansi,
                        &mut stdout,
                    )?;
                }
                if current.as_ref() != fallback.as_ref() {
                    if let Some(path) = current.as_ref().filter(|path| path.is_file()) {
                        write_daemon_log_delta(
                            path,
                            &mut offset,
                            &mut formatter,
                            ansi,
                            &mut stdout,
                        )?;
                    }
                }
                finish_daemon_log_formatters(
                    ansi,
                    current.as_ref(),
                    fallback.as_ref(),
                    &mut formatter,
                    &mut fallback_formatter,
                    &mut stdout,
                )?;
                return Ok(());
            }
        }
    }
}

async fn reload_daemon_if_running(paths: &MiyuPaths) -> Result<()> {
    if ipc::daemon_info(paths).await.is_some() {
        retry_config_reload(RELOAD_MAX_ATTEMPTS, RELOAD_RETRY_INTERVAL, || {
            request_config_reload(paths)
        })
        .await
        .with_context(|| {
            t(
                "configuration was saved, but the running daemon did not reload it",
                "配置已保存，但正在运行的 daemon 未能重新加载配置",
            )
        })?;
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ConfigReloadResponse {
    Reloaded,
    Busy,
}

fn validate_config_reload_response(frame: Option<IpcFrame>) -> Result<ConfigReloadResponse> {
    if let Some(IpcFrame::Error { code, message }) = &frame {
        if *code == Some(ipc::ErrorCode::Busy)
            || (code.is_none() && message == ipc::ADMIN_BUSY_MESSAGE)
        {
            return Ok(ConfigReloadResponse::Busy);
        }
    }
    validate_ipc_command_response(frame)?;
    Ok(ConfigReloadResponse::Reloaded)
}

async fn request_config_reload(paths: &MiyuPaths) -> Result<ConfigReloadResponse> {
    request_config_reload_at(&paths.ipc_socket(), RELOAD_RESPONSE_TIMEOUT).await
}

async fn request_config_reload_at(
    socket: &Path,
    response_timeout: Duration,
) -> Result<ConfigReloadResponse> {
    tokio::time::timeout(response_timeout, async {
        let mut stream = ipc::connect(socket).await?;
        ipc::send(&mut stream, &IpcRequest::new(IpcCommand::ReloadConfig)).await?;
        validate_config_reload_response(ipc::receive::<IpcFrame>(&mut stream).await?)
    })
    .await
    .with_context(|| {
        t(
            "timed out waiting for Miyu daemon to reload configuration",
            "等待 Miyu daemon 重新加载配置超时",
        )
    })?
}

async fn run_reload(paths: &MiyuPaths) -> Result<()> {
    if ipc::daemon_info(paths).await.is_none() {
        bail!("{}", t("Miyu daemon is not running", "Miyu daemon 未运行"));
    }
    retry_config_reload(RELOAD_MAX_ATTEMPTS, RELOAD_RETRY_INTERVAL, || {
        request_config_reload(paths)
    })
    .await?;
    println!("{}", t("configuration reloaded", "配置已重新加载"));
    Ok(())
}

async fn retry_config_reload<F, Fut>(
    max_attempts: usize,
    retry_interval: Duration,
    mut request_reload: F,
) -> Result<()>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<ConfigReloadResponse>>,
{
    if max_attempts == 0 {
        bail!("reload must allow at least one attempt");
    }

    for attempt in 1..=max_attempts {
        match request_reload().await? {
            ConfigReloadResponse::Reloaded => return Ok(()),
            ConfigReloadResponse::Busy if attempt < max_attempts => {
                let seconds = retry_interval.as_secs();
                let message = if is_zh() {
                    format!(
                        "Miyu daemon 正忙；将在 {seconds} 秒后重试配置重载（{attempt}/{max_attempts}）"
                    )
                } else {
                    format!(
                        "Miyu daemon is busy; retrying configuration reload in {seconds} seconds ({attempt}/{max_attempts})"
                    )
                };
                eprintln!("{message}");
                tokio::time::sleep(retry_interval).await;
            }
            ConfigReloadResponse::Busy => {
                let message = if is_zh() {
                    format!("Miyu daemon 在 {max_attempts} 次配置重载尝试后仍然忙碌")
                } else {
                    format!(
                        "Miyu daemon remained busy after {max_attempts} configuration reload attempts"
                    )
                };
                bail!("{message}");
            }
        }
    }
    unreachable!("reload loop always returns on its final attempt")
}

async fn run_tool(paths: &MiyuPaths, mode: AgentMode, args: ToolArgs) -> Result<()> {
    let config = AppConfig::load_or_default(paths)?;
    let registry = build_tool_registry(&config, paths, mode, false)?;
    let output = registry
        .call(&args.name, args.arguments.as_deref().unwrap_or("{}"))
        .await?;
    println!("{output}");
    Ok(())
}

#[derive(Clone, Copy)]
enum InitKind {
    FirstRun,
    Explicit,
}

fn run_init(paths: &MiyuPaths, kind: InitKind) -> Result<()> {
    let interactive = io::stdin().is_terminal() && io::stdout().is_terminal();
    if interactive {
        println!(
            "{}\n",
            match kind {
                InitKind::FirstRun => t("Miyu first start", "Miyu 首次启动"),
                InitKind::Explicit => t("Miyu initialization", "Miyu 初始化"),
            }
        );
    }
    print_init_step(
        interactive,
        t("Preparing config directory", "正在准备配置目录"),
        &paths.config_dir.display().to_string(),
    )?;
    AppConfig::init_files(paths)?;
    print_init_step(
        interactive,
        t("Writing default config", "正在写入默认配置"),
        &paths.config_file.display().to_string(),
    )?;
    print_init_step(
        interactive,
        t("Creating state files", "正在创建状态文件"),
        &paths.state_dir.display().to_string(),
    )?;
    StateStore::new(paths)?.init_files()?;
    let config = AppConfig::load_or_default(paths)?;
    if crate::default_kb::bundled_available() {
        print_init_step(
            interactive,
            t("Importing default knowledge base", "正在导入默认知识库"),
            &paths.data_dir.join("kb").display().to_string(),
        )?;
        if let Err(err) = crate::default_kb::ensure_initialized(paths, &config) {
            if interactive {
                eprintln!(
                    "{}: {err}",
                    t(
                        "default knowledge base import skipped",
                        "默认知识库导入已跳过"
                    )
                );
            }
        }
    }
    print_init_step(
        interactive,
        t("Preparing data directory", "正在准备数据目录"),
        &paths.data_dir.display().to_string(),
    )?;
    if interactive {
        println!("\n{}\n", t("Initialization complete.", "初始化完成。"));
    } else {
        println!(
            "{} {}",
            t("initialized Miyu at", "Miyu 已初始化于"),
            paths.config_dir.display()
        );
    }
    Ok(())
}

fn print_init_step(interactive: bool, label: &str, value: &str) -> Result<()> {
    if interactive {
        std::thread::sleep(Duration::from_millis(180));
        println!("  {label:<24} ✓ {value}");
        io::stdout().flush()?;
    }
    Ok(())
}

fn remove_shell_hooks(paths: &MiyuPaths) -> Result<()> {
    let removed = shell::fish::uninstall(paths)?;
    let removed = shell::bash::uninstall(paths)? || removed;
    let removed = shell::zsh::uninstall(paths)? || removed;
    if !removed {
        println!(
            "{}",
            t(
                "no installed Miyu shell hooks found",
                "未找到已安装的 Miyu shell hook"
            )
        );
    }
    Ok(())
}

fn run_alarm_worker(args: AlarmWorkerArgs) -> Result<()> {
    let paths = alarm_worker_paths(args.state_dir, args.cache_dir);
    let seconds = crate::alarm::parse_alarm_seconds(&args.time)?;
    let source = args
        .audio_file
        .as_ref()
        .map(|path| path.display().to_string())
        .unwrap_or_else(|| "builtin".to_string());
    let _ = append_alarm_log(
        &paths,
        &format!("{}: scheduled in {seconds}s; source={source}\n", args.id),
    );
    std::thread::sleep(Duration::from_secs(seconds));
    let _ = crate::alarm::update_status(&paths, &args.id, crate::alarm::AlarmStatus::Ringing);
    let _ = append_alarm_log(&paths, &format!("{}: playback starting\n", args.id));
    let result = play_alarm_once(args.audio_file.as_deref()).or_else(|err| {
        append_alarm_log(
            &paths,
            &format!("{}: audio playback failed: {err}\n", args.id),
        )?;
        terminal_bell_fallback();
        Ok(())
    });
    if result.is_ok() {
        let _ = append_alarm_log(&paths, &format!("{}: playback finished\n", args.id));
    }
    let _ = crate::alarm::remove(&paths, &args.id);
    result
}

fn play_alarm_once(audio_file: Option<&std::path::Path>) -> Result<()> {
    const ALARM_WAV: &[u8] = include_bytes!("assets/alarm.wav");
    let (_stream, handle) = rodio::OutputStream::try_default()?;
    let audio = match audio_file {
        Some(path) => std::fs::read(path)?,
        None => ALARM_WAV.to_vec(),
    };
    let cursor = Cursor::new(audio);
    let sink = rodio::Sink::try_new(&handle)?;
    let source = rodio::Decoder::new(cursor)?;
    sink.append(source);
    sink.sleep_until_end();
    Ok(())
}

fn terminal_bell_fallback() {
    for _ in 0..5 {
        let _ = std::io::stderr().write_all(b"\x07");
        let _ = std::io::stderr().flush();
        std::thread::sleep(Duration::from_secs(1));
    }
}

fn append_alarm_log(paths: &MiyuPaths, line: &str) -> Result<()> {
    std::fs::create_dir_all(paths.logs_dir())?;
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(crate::alarm::alarm_log_file(paths))?;
    file.write_all(line.as_bytes())?;
    Ok(())
}

fn alarm_worker_paths(state_dir: PathBuf, cache_dir: PathBuf) -> MiyuPaths {
    MiyuPaths {
        root_dir: PathBuf::new(),
        config_dir: PathBuf::new(),
        config_file: PathBuf::new(),
        skills_dir: PathBuf::new(),
        data_dir: PathBuf::new(),
        cache_dir,
        state_dir,
        pictures_dir: PathBuf::new(),
        fish_hook_file: PathBuf::new(),
        bash_hook_file: PathBuf::new(),
        zsh_hook_file: PathBuf::new(),
        scripts_dir: PathBuf::new(),
        system_scripts_dir: PathBuf::new(),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PopOutcome {
    turns: usize,
    archived: bool,
}

fn run_pop(paths: &MiyuPaths, args: PopArgs) -> Result<()> {
    let config = AppConfig::load_or_default(paths)?;
    let state = StateStore::new(paths)?;
    state.recover_stale_turns()?;
    if let Some(outcome) = execute_pop(paths, &config, &state, args.count)? {
        print_pop_outcome(outcome);
    }
    Ok(())
}

/// Pop while the daemon owns the core: candidates are selected locally
/// (read-only), but the mutation goes through IPC so the daemon stays the
/// single writer.
async fn run_pop_via_daemon(paths: &MiyuPaths, args: PopArgs) -> Result<()> {
    let state = StateStore::new(paths)?;
    let turn_ids: Vec<String> = match args.count {
        Some(count) => {
            validate_pop_count(count)?;
            state
                .oldest_evictable_visible_turns(count)?
                .into_iter()
                .map(|turn| turn.turn_id)
                .collect()
        }
        None => {
            if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
                bail!(
                    "{}",
                    t(
                        "interactive pop requires a terminal; use `miyu pop <count>`",
                        "交互 pop 需要终端；请使用 `miyu pop <数量>`",
                    )
                );
            }
            let limit = usize::try_from(i64::MAX).unwrap_or(usize::MAX);
            let candidates = state.oldest_evictable_visible_turns(limit)?;
            if candidates.is_empty() {
                print_nothing_to_pop();
                return Ok(());
            }
            let Some(selected) = inline_pop_select(&candidates)? else {
                return Ok(());
            };
            candidates
                .into_iter()
                .zip(selected)
                .filter_map(|(turn, selected)| selected.then_some(turn.turn_id))
                .collect()
        }
    };
    if turn_ids.is_empty() {
        print_nothing_to_pop();
        return Ok(());
    }
    let (_, data) = send_ipc_admin(
        paths,
        IpcCommand::Pop {
            target: crate::ipc::SessionRef::Current,
            turn_ids,
        },
    )
    .await?;
    let turns = data
        .get("turns")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);
    if turns > 0 {
        print_pop_outcome(PopOutcome {
            turns: turns as usize,
            archived: data
                .get("archived")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false),
        });
    } else {
        print_nothing_to_pop();
    }
    Ok(())
}

fn execute_pop(
    paths: &MiyuPaths,
    config: &AppConfig,
    state: &StateStore,
    count: Option<usize>,
) -> Result<Option<PopOutcome>> {
    let turns = match count {
        Some(count) => {
            validate_pop_count(count)?;
            state.oldest_evictable_visible_turns(count)?
        }
        None => {
            if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
                bail!(
                    "{}",
                    t(
                        "interactive pop requires a terminal; use `miyu pop <count>`",
                        "交互 pop 需要终端；请使用 `miyu pop <数量>`",
                    )
                );
            }
            let limit = usize::try_from(i64::MAX).unwrap_or(usize::MAX);
            let candidates = state.oldest_evictable_visible_turns(limit)?;
            if candidates.is_empty() {
                print_nothing_to_pop();
                return Ok(None);
            }
            let Some(selected) = inline_pop_select(&candidates)? else {
                return Ok(None);
            };
            let selected = candidates
                .into_iter()
                .zip(selected)
                .filter_map(|(turn, selected)| selected.then_some(turn))
                .collect::<Vec<_>>();
            if selected.is_empty() {
                return Ok(None);
            }
            selected
        }
    };
    if turns.is_empty() {
        print_nothing_to_pop();
        return Ok(None);
    }

    let memory = MemoryStore::new(config, paths);
    archive_and_delete_visible_turns(state, &memory, &turns)?;
    let memory_config = config.memory_config();
    Ok(Some(PopOutcome {
        turns: turns.len(),
        archived: memory_config.enabled && memory_config.evicted_context_enabled,
    }))
}

fn validate_pop_count(count: usize) -> Result<usize> {
    if count == 0 {
        bail!(
            "{}",
            t("pop count must be greater than zero", "pop 数量必须大于 0")
        );
    }
    Ok(count)
}

fn parse_positive_pop_count(value: &str) -> std::result::Result<usize, String> {
    let count = value.parse::<usize>().map_err(|_| {
        t(
            "pop count must be a positive integer",
            "pop 数量必须是正整数",
        )
        .to_string()
    })?;
    if count == 0 {
        return Err(t("pop count must be greater than zero", "pop 数量必须大于 0").to_string());
    }
    Ok(count)
}

fn parse_repl_pop_count(args: &str) -> Result<Option<usize>> {
    let mut parts = args.split_whitespace();
    let Some(value) = parts.next() else {
        return Ok(None);
    };
    if parts.next().is_some() {
        bail!(
            "{}",
            t("usage: /pop [positive integer]", "用法：/pop [正整数]")
        );
    }
    let count = parse_positive_pop_count(value).map_err(anyhow::Error::msg)?;
    validate_pop_count(count).map(Some)
}

fn print_pop_outcome(outcome: PopOutcome) {
    let message = if is_zh() {
        if outcome.archived {
            format!("已弹出 {} 轮 · 已归档", outcome.turns)
        } else {
            format!(
                "已弹出 {} 轮 · 未归档（弹出上下文归档已关闭）",
                outcome.turns
            )
        }
    } else {
        let turns = if outcome.turns == 1 { "turn" } else { "turns" };
        if outcome.archived {
            format!("popped {} {turns} · archived", outcome.turns)
        } else {
            format!(
                "popped {} {turns} · not archived (evicted-context archiving is disabled)",
                outcome.turns
            )
        }
    };
    println!("\x1b[2m{message}\x1b[0m\n");
}

fn print_nothing_to_pop() {
    println!(
        "\x1b[2m{}\x1b[0m\n",
        t(
            "no conversation turns are available to pop",
            "没有可弹出的上下文轮次"
        )
    );
}

fn inline_pop_select(turns: &[Turn]) -> Result<Option<Vec<bool>>> {
    let menu_lines = inline_pop_lines(turns.len());
    let visible_items = menu_lines.saturating_sub(2) as usize / 3;
    reserve_inline_fuzzy_space(menu_lines)?;
    let mut session = InlineRawMode::start()?;
    let matcher = SkimMatcherV2::default();
    let search_items = turns.iter().map(pop_search_text).collect::<Vec<_>>();
    let mut active = vec![false; turns.len()];
    let mut query = String::new();
    let mut selected = 0usize;
    let mut scroll = 0usize;
    let (_, cursor_y) = cursor::position().unwrap_or((0, menu_lines.saturating_sub(1)));
    let anchor_y = cursor_y.saturating_sub(menu_lines.saturating_sub(1));
    loop {
        let matches = pop_matches(&matcher, &search_items, &query);
        if selected >= matches.len() {
            selected = matches.len().saturating_sub(1);
        }
        let visible = matches.len().min(visible_items);
        scroll = inline_fuzzy_scroll(selected, scroll, visible);
        draw_inline_pop(
            &mut session.stdout,
            anchor_y,
            menu_lines,
            turns,
            &matches,
            selected,
            scroll,
            &active,
            &query,
        )?;
        if let Event::Key(KeyEvent {
            code, modifiers, ..
        }) = event::read()?
        {
            match code {
                KeyCode::Char('c') if modifiers.contains(KeyModifiers::CONTROL) => {
                    clear_inline_fuzzy(&mut session.stdout, anchor_y, menu_lines)?;
                    return Ok(None);
                }
                KeyCode::Esc => {
                    clear_inline_fuzzy(&mut session.stdout, anchor_y, menu_lines)?;
                    return Ok(None);
                }
                KeyCode::Char('q') => {
                    clear_inline_fuzzy(&mut session.stdout, anchor_y, menu_lines)?;
                    return Ok(None);
                }
                KeyCode::Enter => {
                    clear_inline_fuzzy(&mut session.stdout, anchor_y, menu_lines)?;
                    return Ok(Some(active));
                }
                KeyCode::Tab => {
                    if let Some(index) = matches.get(selected) {
                        if let Some(value) = active.get_mut(*index) {
                            *value = !*value;
                        }
                    }
                }
                KeyCode::Up | KeyCode::Char('k') => selected = selected.saturating_sub(1),
                KeyCode::Down | KeyCode::Char('j') => {
                    selected = (selected + 1).min(matches.len().saturating_sub(1));
                }
                KeyCode::Backspace => {
                    query.pop();
                    selected = 0;
                    scroll = 0;
                }
                KeyCode::Char(ch) if !modifiers.contains(KeyModifiers::CONTROL) => {
                    query.push(ch);
                    selected = 0;
                    scroll = 0;
                }
                _ => {}
            }
        }
    }
}

fn pop_matches(matcher: &SkimMatcherV2, items: &[String], query: &str) -> Vec<usize> {
    items
        .iter()
        .enumerate()
        .filter_map(|(index, item)| {
            (query.trim().is_empty() || matcher.fuzzy_match(item, query).is_some()).then_some(index)
        })
        .collect()
}

#[allow(clippy::too_many_arguments)]
fn draw_inline_pop(
    stdout: &mut io::Stdout,
    anchor_y: u16,
    menu_lines: u16,
    turns: &[Turn],
    matches: &[usize],
    selected: usize,
    scroll: usize,
    active: &[bool],
    query: &str,
) -> Result<()> {
    let (cols, _) = terminal::size().unwrap_or((80, 24));
    let bar = inline_fuzzy_bar();
    let width = (cols as usize).saturating_sub(visible_width(&bar)).max(1);
    let visible_items = menu_lines.saturating_sub(2) as usize / 3;
    queue!(stdout, Hide)?;
    for row in 0..menu_lines {
        queue!(
            stdout,
            MoveTo(0, anchor_y + row),
            Clear(ClearType::CurrentLine)
        )?;
    }
    queue!(
        stdout,
        MoveTo(0, anchor_y),
        Print(&bar),
        Print(pop_menu_header(
            query,
            active.iter().filter(|selected| **selected).count(),
            turns.len(),
            width,
        )),
    )?;
    if matches.is_empty() {
        queue!(
            stdout,
            MoveTo(0, anchor_y + 1),
            Print(&bar),
            Print(format!("\x1b[2m{}\x1b[0m", t("no matches", "没有匹配项")))
        )?;
    } else {
        for (row, item_index) in matches.iter().skip(scroll).take(visible_items).enumerate() {
            let focused = scroll + row == selected;
            let checked = active.get(*item_index).copied().unwrap_or(false);
            let lines = pop_menu_turn_lines(&turns[*item_index], focused, checked, width);
            for (line_offset, line) in lines.into_iter().enumerate() {
                queue!(
                    stdout,
                    MoveTo(0, anchor_y + 1 + row as u16 * 3 + line_offset as u16),
                    Print(&bar),
                    Print(line)
                )?;
            }
        }
    }
    queue!(
        stdout,
        MoveTo(0, anchor_y + menu_lines.saturating_sub(1)),
        Print(&bar),
        Print(pop_menu_help_line(width))
    )?;
    stdout.flush()?;
    Ok(())
}

fn pop_menu_header(query: &str, selected: usize, total: usize, width: usize) -> String {
    let title = if query.trim().is_empty() {
        t("Pop context", "弹出上下文").to_string()
    } else {
        format!(
            "{} · {}: {}",
            t("Pop context", "弹出上下文"),
            t("Search", "搜索"),
            query.trim()
        )
    };
    let count = if is_zh() {
        format!("已选 {selected} / {total}")
    } else {
        format!("selected {selected} / {total}")
    };
    let count_width = visible_width(&count);
    if count_width >= width {
        return format!("\x1b[2m{}\x1b[0m", truncate_visible_width(&count, width));
    }
    let title_width = width.saturating_sub(count_width + 1);
    let title = truncate_visible_width(&title, title_width);
    let gap = width
        .saturating_sub(visible_width(&title).saturating_add(count_width))
        .max(1);
    format!(
        "\x1b[1m{title}\x1b[0m{}\x1b[2m{count}\x1b[0m",
        " ".repeat(gap)
    )
}

fn pop_menu_turn_lines(turn: &Turn, focused: bool, checked: bool, width: usize) -> [String; 3] {
    let cursor = if focused { "›" } else { " " };
    let marker = if checked { "[*]" } else { "[ ]" };
    let lines = [
        format!(
            "{cursor} {marker} {}",
            pop_menu_timestamp(&turn.user_timestamp)
        ),
        format!(
            "      {}{}",
            t("You: ", "你："),
            pop_menu_summary(&turn.user_content)
        ),
        format!(
            "      {}{}",
            t("AI: ", "AI："),
            pop_menu_assistant_summary(turn)
        ),
    ];
    lines.map(|line| {
        let line = truncate_visible_width(&line, width);
        if focused {
            format!("\x1b[1m\x1b[35m{line}\x1b[0m")
        } else if checked {
            format!("\x1b[1m\x1b[32m{line}\x1b[0m")
        } else {
            format!("\x1b[2m{line}\x1b[0m")
        }
    })
}

fn pop_menu_timestamp(timestamp: &str) -> String {
    DateTime::parse_from_rfc3339(timestamp)
        .map(|timestamp| {
            timestamp
                .with_timezone(&Local)
                .format("%Y-%m-%d %H:%M")
                .to_string()
        })
        .unwrap_or_else(|_| pop_menu_summary(timestamp))
}

fn pop_menu_assistant_summary(turn: &Turn) -> String {
    if turn.status == TurnStatus::Interrupted {
        t("(reply interrupted)", "（回复已中断）").to_string()
    } else {
        pop_menu_summary(&turn.assistant_content)
    }
}

fn pop_menu_summary(content: &str) -> String {
    strip_terminal_control_sequences(content)
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or_else(|| t("(empty)", "（空）"))
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn pop_search_text(turn: &Turn) -> String {
    format!(
        "{} {} {}",
        pop_menu_timestamp(&turn.user_timestamp),
        pop_menu_summary(&turn.user_content),
        pop_menu_assistant_summary(turn)
    )
}

fn pop_menu_help_line(width: usize) -> String {
    let line = t(
        "Up/Down or j/k move · Tab toggle · Enter pop · Esc/q cancel",
        "↑/↓ 或 j/k 移动 · Tab 勾选 · Enter 弹出 · Esc/q 取消",
    );
    format!("\x1b[2m{}\x1b[0m", truncate_visible_width(line, width))
}

fn inline_pop_lines(item_count: usize) -> u16 {
    let (_, terminal_rows) = terminal::size().unwrap_or((80, 24));
    let available_items = terminal_rows.saturating_sub(2).saturating_div(3).max(1) as usize;
    let visible_items = item_count.min(5).min(available_items).max(1);
    (visible_items as u16).saturating_mul(3).saturating_add(2)
}

async fn run_models(paths: &MiyuPaths, args: ModelsArgs) -> Result<()> {
    run_models_for_session(paths, args, None).await
}

/// Switches the model pool of one session (the current session when
/// `session_id` is None). The override persists on the session, so reopening
/// it restores the model; the global pool is managed in `miyu config`.
async fn run_models_for_session(
    paths: &MiyuPaths,
    args: ModelsArgs,
    session_id: Option<&str>,
) -> Result<()> {
    let config = AppConfig::load(paths)?;
    let choices = config.text_provider_model_choices();
    if choices.is_empty() {
        bail!(
            "{}",
            t(
                "no configured provider models; configure a model first",
                "没有已配置的 provider 模型；请先配置模型",
            )
        );
    }
    if let Some(target) = args.target.as_deref() {
        let target = target.trim();
        if target.eq_ignore_ascii_case("default") || target.eq_ignore_ascii_case("global") {
            set_session_models(paths, session_id, Vec::new()).await?;
            println!(
                "{}",
                t(
                    "this session now follows the global active pool",
                    "当前会话已恢复跟随全局激活模型池"
                )
            );
            return Ok(());
        }
        let choice = crate::config::resolve_provider_model_argument(&choices, target)
            .map_err(anyhow::Error::msg)?;
        let label = choice.label();
        let models = vec![ActiveProviderModelConfig {
            provider_id: choice.provider_id.clone(),
            model: choice.model.clone(),
        }];
        set_session_models(paths, session_id, models).await?;
        println!("{}: {label}", t("session model", "当前会话模型"));
        return Ok(());
    }
    if io::stdout().is_terminal() && io::stdin().is_terminal() {
        let override_pool = session_model_override_snapshot(paths, session_id)?;
        let initial = choices
            .iter()
            .map(|choice| match override_pool.as_deref() {
                Some(pool) => pool.iter().any(|model| {
                    model.provider_id == choice.provider_id && model.model == choice.model
                }),
                None => config.is_active_provider_model(&choice.provider_id, &choice.model),
            })
            .collect::<Vec<_>>();
        if let Some(active) = inline_fuzzy_select(
            &choices
                .iter()
                .map(|choice| choice.label())
                .collect::<Vec<_>>(),
            initial.clone(),
        )? {
            if active == initial {
                println!("{}", t("no changes", "未做修改"));
                return Ok(());
            }
            let models = choices
                .iter()
                .zip(active)
                .filter_map(|(choice, active)| {
                    active.then(|| ActiveProviderModelConfig {
                        provider_id: choice.provider_id.clone(),
                        model: choice.model.clone(),
                    })
                })
                .collect::<Vec<_>>();
            let cleared = models.is_empty();
            set_session_models(paths, session_id, models).await?;
            if cleared {
                println!(
                    "{}",
                    t(
                        "this session now follows the global active pool",
                        "当前会话已恢复跟随全局激活模型池"
                    )
                );
            } else {
                println!("{}", t("session models updated", "已更新当前会话模型"));
            }
        }
        return Ok(());
    }
    print_model_choices(&config, &choices, None);
    Ok(())
}

const DEFAULT_PERSONA_LABEL_ZH: &str = "Miyu（内置默认）";
const DEFAULT_PERSONA_LABEL_EN: &str = "Miyu (built-in default)";

fn list_persona_files(paths: &MiyuPaths, config: &AppConfig) -> Result<Vec<String>> {
    let dir = config.prompts_dir_path(paths);
    let mut names = Vec::new();
    if dir.exists() {
        for entry in std::fs::read_dir(&dir)? {
            let entry = entry?;
            if entry.file_type()?.is_file() {
                let name = entry.file_name().to_string_lossy().to_string();
                if name.ends_with(".md") && !name.eq_ignore_ascii_case("system-prompt.md") {
                    names.push(name);
                }
            }
        }
    }
    names.sort();
    Ok(names)
}

/// Interactive persona picker (single-select). Returns true when the active
/// persona changed and the config was saved.
fn run_persona_picker(paths: &MiyuPaths, argument: &str) -> Result<bool> {
    let mut config = AppConfig::load(paths)?;
    let personas = list_persona_files(paths, &config)?;
    let current = config.prompt.active_persona.trim().to_string();
    let argument = argument.trim();
    let chosen: Option<String> = if !argument.is_empty() {
        if argument.eq_ignore_ascii_case("default")
            || argument.eq_ignore_ascii_case("miyu")
            || argument == "内置"
        {
            Some(String::new())
        } else {
            let needle = argument.to_ascii_lowercase();
            let matched = personas.iter().find(|name| {
                name.eq_ignore_ascii_case(argument)
                    || name
                        .to_ascii_lowercase()
                        .trim_end_matches(".md")
                        .contains(needle.trim_end_matches(".md"))
            });
            match matched {
                Some(name) => Some(name.clone()),
                None => bail!(
                    "{}: {argument}",
                    t("no persona file matches", "没有匹配的人格文件")
                ),
            }
        }
    } else if io::stdout().is_terminal() && io::stdin().is_terminal() {
        let default_label = t(DEFAULT_PERSONA_LABEL_EN, DEFAULT_PERSONA_LABEL_ZH).to_string();
        let mut items = vec![default_label];
        items.extend(personas.iter().cloned());
        let initial = if current.is_empty() {
            0
        } else {
            personas
                .iter()
                .position(|name| *name == current)
                .map(|index| index + 1)
                .unwrap_or(0)
        };
        match inline_fuzzy_select_single(&items, initial)? {
            Some(0) => Some(String::new()),
            Some(index) => personas.get(index - 1).cloned(),
            None => None,
        }
    } else {
        println!(
            "{}: {}",
            t("current persona", "当前人格"),
            if current.is_empty() {
                t(DEFAULT_PERSONA_LABEL_EN, DEFAULT_PERSONA_LABEL_ZH).to_string()
            } else {
                current.clone()
            }
        );
        for name in &personas {
            println!("  {name}");
        }
        println!("{}", t("switch with: /persona <name>", "切换：/persona <名称>"));
        return Ok(false);
    };
    let Some(target) = chosen else {
        return Ok(false);
    };
    if target == current {
        println!("{}", t("no changes", "未做修改"));
        return Ok(false);
    }
    config.prompt.active_persona = target.clone();
    config.save(paths)?;
    println!(
        "{}: {}",
        t("active persona", "当前人格"),
        if target.is_empty() {
            t(DEFAULT_PERSONA_LABEL_EN, DEFAULT_PERSONA_LABEL_ZH).to_string()
        } else {
            target
        }
    );
    Ok(true)
}

/// One line per context watermark: absolute tokens left before each tier
/// fires (soft notice / mechanical prune / compaction / forced compaction).
/// Absolute values, not percentages — same reasoning as the cache accounting
/// log line.
fn compact_watermark_text(
    context_tokens: usize,
    window: usize,
    context: &crate::config::ContextConfig,
) -> String {
    let tier = |label: &str, ratio: f32| -> String {
        let threshold = (window as f32 * ratio).max(1.0) as usize;
        if context_tokens >= threshold {
            format!("{label} {}✓", t("reached", "已达"))
        } else {
            format!("{label} -{}", threshold - context_tokens)
        }
    };
    format!(
        "{}: {} / {} · {}",
        t("Context watermarks", "上下文水位"),
        context_tokens,
        window,
        [
            tier(&t("notice", "提示"), context.compact_soft_ratio),
            tier(&t("prune", "折叠"), context.compact_snip_ratio),
            tier(&t("compact", "压缩"), context.trim_at_ratio),
            tier(&t("force", "强制"), context.compact_force_ratio),
        ]
        .join(" · ")
    )
}

fn usage_overview_text(
    snapshot: &crate::state::UsageSnapshot,
    context: Option<(u64, Option<usize>)>,
) -> String {
    let compact = render::format_compact_count;
    let mut lines = Vec::new();
    lines.push(format!(
        "\x1b[1m{}\x1b[0m \x1b[2m{}\x1b[0m",
        t("Token usage", "Token 用量"),
        t(
            "(global totals: all sessions + background calls)",
            "（全局累计：含所有会话与后台调用）"
        )
    ));
    lines.push(format!(
        "  {:<10} {}",
        t("requests", "请求次数"),
        compact(snapshot.requests)
    ));
    let cached = snapshot.cache_read_tokens;
    let fresh = snapshot.prompt_tokens.saturating_sub(cached);
    let mut input_line = format!(
        "  {:<10} {}",
        t("input", "输入"),
        compact(snapshot.prompt_tokens)
    );
    if cached > 0 {
        input_line.push_str(&format!(
            "\x1b[2m（{} {} · {} {}",
            t("cache hits", "缓存命中"),
            compact(cached),
            t("billed new", "计费新输入"),
            compact(fresh)
        ));
        if snapshot.cache_write_tokens > 0 {
            input_line.push_str(&format!(
                " · {} {}",
                t("cache writes", "缓存写入"),
                compact(snapshot.cache_write_tokens)
            ));
        }
        input_line.push_str("）\x1b[0m");
    }
    lines.push(input_line);
    let mut output_line = format!(
        "  {:<10} {}",
        t("output", "输出"),
        compact(snapshot.completion_tokens)
    );
    if snapshot.reasoning_tokens > 0 {
        output_line.push_str(&format!(
            "\x1b[2m（{} {}）\x1b[0m",
            t("reasoning", "其中思考"),
            compact(snapshot.reasoning_tokens)
        ));
    }
    lines.push(output_line);
    lines.push(format!(
        "  {:<10} {} \x1b[2m· {} Σ{}\x1b[0m",
        t("total", "总计"),
        compact(snapshot.total_tokens),
        t("conversation", "对话口径"),
        compact(snapshot.conversation_tokens)
    ));
    if let Some(last) = snapshot
        .last_conversation_usage
        .as_ref()
        .or(snapshot.last_usage.as_ref())
    {
        let mut line = format!(
            "  {:<10} in {}",
            t("last turn", "最近一轮"),
            compact(last.prompt_tokens)
        );
        if last.cache_read_tokens > 0 {
            line.push_str(&format!("(C{})", compact(last.cache_read_tokens)));
        }
        line.push_str(&format!(" · out {}", compact(last.completion_tokens)));
        lines.push(line);
    }
    if let Some((tokens, window)) = context {
        let window = window
            .map(|value| render::format_compact_count(value as u64))
            .unwrap_or_else(|| "?".to_string());
        lines.push(format!(
            "  {:<10} {} / {window}",
            t("context", "会话上下文"),
            compact(tokens)
        ));
    }
    lines.join("\n")
}

/// Suggested archive name when the user did not pick one.
fn default_export_name() -> String {
    let host = std::fs::read_to_string("/etc/hostname")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "miyu".to_string());
    let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S");
    format!("miyu-export-{host}-{stamp}.tar.gz")
}

fn readable_bytes(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut size = bytes as f64;
    let mut unit = 0;
    while size >= 1024.0 && unit + 1 < UNITS.len() {
        size /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} {}", UNITS[0])
    } else {
        format!("{size:.1} {}", UNITS[unit])
    }
}

/// `t` for messages built at runtime — the static version cannot take a `format!`.
fn owned(en: String, zh: String) -> String {
    if crate::i18n::is_zh() {
        zh
    } else {
        en
    }
}

fn run_export(paths: &MiyuPaths, args: ExportArgs) -> Result<()> {
    let output = args
        .output
        .unwrap_or_else(|| PathBuf::from(default_export_name()));
    let options = crate::transfer::export::ExportOptions {
        all: args.all,
        index: args.index,
        platforms: args.platforms,
        no_secrets: args.no_secrets,
        dry_run: args.dry_run,
        force: args.force,
    };
    let report = crate::transfer::export::export(paths, &output, &options)?;

    if options.dry_run {
        for (unit, bytes) in &report.by_unit {
            println!("  {:>10}  {unit}", readable_bytes(*bytes));
        }
    }

    let count = report.entries;
    let size = readable_bytes(report.bytes);
    println!(
        "{}",
        match &report.archive {
            None => owned(
                format!("Dry run: {count} files, {size} would be packed."),
                format!("试运行：将打包 {count} 个文件，共 {size}。"),
            ),
            Some(path) => {
                let path = path.display();
                owned(
                    format!("Exported {count} files ({size}) to {path}"),
                    format!("已导出 {count} 个文件（{size}）到 {path}"),
                )
            }
        }
    );

    // The archive is plaintext-credentialed unless asked otherwise; the user
    // needs to know that before they put it on a USB stick or a chat app.
    if report.secrets_included && report.archive.is_some() {
        eprintln!(
            "{}",
            t(
                "Warning: this archive contains API keys and access tokens in plain text. Keep it private, or re-export with --no-secrets.",
                "警告：归档内含明文 API key 与访问令牌。请妥善保管，或改用 --no-secrets 重新导出。",
            )
        );
    }
    if !options.all && !options.index {
        println!(
            "{}",
            t(
                "The knowledge-base vector index was left out; run `miyu kb embed` after importing (or re-export with --index).",
                "未包含知识库向量索引；导入后请运行 miyu kb embed（或改用 --index 重新导出）。",
            )
        );
    }
    Ok(())
}

async fn run_import(paths: &MiyuPaths, args: ImportArgs) -> Result<()> {
    // The daemon holds conversation.db's WAL open; replacing the file under it
    // would leave both the old process and the new database inconsistent.
    if crate::ipc::daemon_info(paths).await.is_some() {
        anyhow::bail!(
            "{}",
            t(
                "the Miyu daemon is running and holds the database open; stop it first with `miyu daemon stop`",
                "Miyu daemon 正在运行并占用数据库；请先执行 miyu daemon stop",
            )
        );
    }

    let options = crate::transfer::import::ImportOptions { force: args.force };
    let report = crate::transfer::import::import(paths, &args.archive, &options)?;

    if let Some(backup) = &report.backup {
        let path = backup.display();
        println!(
            "{}",
            owned(
                format!("Backed up the previous installation to {path}"),
                format!("已把覆盖前的安装备份到 {path}"),
            )
        );
    }
    let restored = report.restored;
    println!(
        "{}",
        owned(
            format!("Restored {restored} files."),
            format!("已恢复 {restored} 个文件。"),
        )
    );
    if !report.unknown_units.is_empty() {
        // A newer Miyu wrote data this build has no name for. It is on disk;
        // say so rather than let it look like it vanished.
        let units = report
            .unknown_units
            .iter()
            .cloned()
            .collect::<Vec<_>>()
            .join(", ");
        println!(
            "{}",
            owned(
                format!(
                    "Restored data this version does not recognise \
                     (written by a newer Miyu): {units}"
                ),
                format!("恢复了本版本不认识的数据（由更新版本的 Miyu 写入）：{units}"),
            )
        );
    }
    if report.cleared_workspaces > 0 {
        let cleared = report.cleared_workspaces;
        println!(
            "{}",
            owned(
                format!(
                    "Cleared {cleared} session workspace(s) pointing at \
                     directories this machine does not have."
                ),
                format!("已清除 {cleared} 个指向本机不存在目录的会话工作区。"),
            )
        );
    }

    println!("\n{}", t("Next steps:", "接下来需要手动完成："));
    println!(
        "  {}",
        t(
            "reinstall the shell integration: `miyu fish-init` / `bash-init` / `zsh-init`",
            "重装 shell 集成：miyu fish-init / bash-init / zsh-init",
        )
    );
    println!(
        "  {}",
        t(
            "`miyu kb reindex` — the knowledge base records absolute paths from the old machine",
            "miyu kb reindex —— 知识库记录的是旧机器上的绝对路径",
        )
    );
    if !report.index_included {
        println!(
            "  {}",
            t(
                "`miyu kb embed` — the vector index was not in the archive",
                "miyu kb embed —— 归档中不含向量索引",
            )
        );
    }
    if !report.secrets_included {
        println!(
            "  {}",
            t(
                "refill API keys and access tokens: `miyu config`",
                "补填 API key 与访问令牌：miyu config",
            )
        );
    }
    Ok(())
}

fn run_list_models(paths: &MiyuPaths) -> Result<()> {
    let config = AppConfig::load(paths)?;
    let choices = config.text_provider_model_choices();
    if choices.is_empty() {
        bail!(
            "{}",
            t(
                "no configured provider models; configure a model first",
                "没有已配置的 provider 模型；请先配置模型",
            )
        );
    }
    let override_pool = session_model_override_snapshot(paths, None)?;
    print_model_choices(&config, &choices, override_pool.as_deref());
    println!(
        "{}",
        t(
            "switch with: miyu models <index|provider/model>; 'miyu models default' follows the global pool",
            "切换：miyu models <序号|供应商/模型>；miyu models default 恢复跟随全局模型池"
        )
    );
    Ok(())
}

fn print_model_choices(
    config: &AppConfig,
    choices: &[crate::config::ProviderModelChoice],
    override_pool: Option<&[ActiveProviderModelConfig]>,
) {
    for (index, choice) in choices.iter().enumerate() {
        let active = match override_pool {
            Some(pool) => pool.iter().any(|model| {
                model.provider_id == choice.provider_id && model.model == choice.model
            }),
            None => config.is_active_provider_model(&choice.provider_id, &choice.model),
        };
        let marker = if active { "[*]" } else { "[ ]" };
        println!("{marker} {}. {}", index + 1, choice.label());
    }
    match override_pool {
        Some(_) => println!(
            "{}",
            t(
                "[*] = models pinned to the current session",
                "[*] = 当前会话固定使用的模型"
            )
        ),
        None => println!(
            "{}",
            t(
                "[*] = global active pool (the current session follows it)",
                "[*] = 全局激活模型池（当前会话跟随全局）"
            )
        ),
    }
}

/// Reads a session's model override straight from the shared state database;
/// works whether or not the daemon is running.
fn session_model_override_snapshot(
    paths: &MiyuPaths,
    session_id: Option<&str>,
) -> Result<Option<Vec<ActiveProviderModelConfig>>> {
    let store = StateStore::new(paths)?;
    let session_id = match session_id {
        Some(session_id) => session_id.to_string(),
        None => store.session_id().to_string(),
    };
    store.session_model_override(&session_id)
}

async fn set_session_models(
    paths: &MiyuPaths,
    session_id: Option<&str>,
    models: Vec<ActiveProviderModelConfig>,
) -> Result<()> {
    if ipc::daemon_info(paths).await.is_some() {
        let target = match session_id {
            Some(id) => ipc::SessionRef::Id { id: id.to_string() },
            None => ipc::SessionRef::Current,
        };
        send_ipc_command(paths, IpcCommand::SetSessionModels { target, models }).await?;
        return Ok(());
    }
    let config = AppConfig::load(paths)?;
    let choices = config.text_provider_model_choices();
    for model in &models {
        if !choices
            .iter()
            .any(|choice| choice.provider_id == model.provider_id && choice.model == model.model)
        {
            bail!(
                "{}{}/{}",
                t("unknown model: ", "未知模型："),
                model.provider_id,
                model.model
            );
        }
    }
    let store = StateStore::new(paths)?;
    let session_id = match session_id {
        Some(session_id) => session_id.to_string(),
        None => store.session_id().to_string(),
    };
    store.set_session_model_override(
        &session_id,
        (!models.is_empty()).then_some(models.as_slice()),
    )
}

fn inline_fuzzy_select(items: &[String], mut active: Vec<bool>) -> Result<Option<Vec<bool>>> {
    let menu_lines = inline_fuzzy_lines(items.len());
    reserve_inline_fuzzy_space(menu_lines)?;
    let mut session = InlineRawMode::start()?;
    let matcher = SkimMatcherV2::default();
    let mut query = String::new();
    let mut selected = 0usize;
    let mut scroll = 0usize;
    let (_, cursor_y) = cursor::position().unwrap_or((0, menu_lines.saturating_sub(1)));
    let anchor_y = cursor_y.saturating_sub(menu_lines.saturating_sub(1));
    loop {
        let matches = fuzzy_matches(&matcher, items, &query);
        if selected >= matches.len() {
            selected = matches.len().saturating_sub(1);
        }
        let visible = matches.len().min(menu_lines.saturating_sub(2) as usize);
        scroll = inline_fuzzy_scroll(selected, scroll, visible);
        draw_inline_fuzzy(
            &mut session.stdout,
            anchor_y,
            menu_lines,
            &query,
            items,
            &matches,
            selected,
            scroll,
            &active,
        )?;
        if let Event::Key(KeyEvent {
            code, modifiers, ..
        }) = event::read()?
        {
            match code {
                KeyCode::Char('c')
                    if modifiers.contains(KeyModifiers::CONTROL)
                        && !modifiers.contains(KeyModifiers::SHIFT) =>
                {
                    clear_inline_fuzzy(&mut session.stdout, anchor_y, menu_lines)?;
                    return Ok(None);
                }
                KeyCode::Esc => {
                    clear_inline_fuzzy(&mut session.stdout, anchor_y, menu_lines)?;
                    return Ok(Some(active));
                }
                KeyCode::Char('q') if query.is_empty() => {
                    clear_inline_fuzzy(&mut session.stdout, anchor_y, menu_lines)?;
                    return Ok(Some(active));
                }
                KeyCode::Enter => {
                    clear_inline_fuzzy(&mut session.stdout, anchor_y, menu_lines)?;
                    return Ok(Some(active));
                }
                KeyCode::Tab => {
                    if let Some((_, index)) = matches.get(selected) {
                        if let Some(value) = active.get_mut(*index) {
                            *value = !*value;
                        }
                    }
                }
                KeyCode::Up | KeyCode::Char('k') => selected = selected.saturating_sub(1),
                KeyCode::Down | KeyCode::Char('j') => {
                    selected = (selected + 1).min(matches.len().saturating_sub(1));
                }
                KeyCode::Backspace => {
                    query.pop();
                    selected = 0;
                    scroll = 0;
                }
                KeyCode::Char(ch) if !modifiers.contains(KeyModifiers::CONTROL) => {
                    query.push(ch);
                    selected = 0;
                    scroll = 0;
                }
                _ => {}
            }
        }
    }
}

/// Single-select variant of the inline fuzzy menu: Tab marks a row (radio),
/// Enter confirms the marked row (or the highlighted one when nothing is
/// marked); Esc / q cancels.
fn inline_fuzzy_select_single(items: &[String], initial: usize) -> Result<Option<usize>> {
    let mut active = vec![false; items.len()];
    if let Some(slot) = active.get_mut(initial) {
        *slot = true;
    }
    let menu_lines = inline_fuzzy_lines(items.len());
    reserve_inline_fuzzy_space(menu_lines)?;
    let mut session = InlineRawMode::start()?;
    let matcher = SkimMatcherV2::default();
    let mut query = String::new();
    let mut selected = 0usize;
    let mut scroll = 0usize;
    let (_, cursor_y) = cursor::position().unwrap_or((0, menu_lines.saturating_sub(1)));
    let anchor_y = cursor_y.saturating_sub(menu_lines.saturating_sub(1));
    loop {
        let matches = fuzzy_matches(&matcher, items, &query);
        if selected >= matches.len() {
            selected = matches.len().saturating_sub(1);
        }
        let visible = matches.len().min(menu_lines.saturating_sub(2) as usize);
        scroll = inline_fuzzy_scroll(selected, scroll, visible);
        draw_inline_fuzzy(
            &mut session.stdout,
            anchor_y,
            menu_lines,
            &query,
            items,
            &matches,
            selected,
            scroll,
            &active,
        )?;
        if let Event::Key(KeyEvent {
            code, modifiers, ..
        }) = event::read()?
        {
            match code {
                KeyCode::Char('c')
                    if modifiers.contains(KeyModifiers::CONTROL)
                        && !modifiers.contains(KeyModifiers::SHIFT) =>
                {
                    clear_inline_fuzzy(&mut session.stdout, anchor_y, menu_lines)?;
                    return Ok(None);
                }
                KeyCode::Esc => {
                    clear_inline_fuzzy(&mut session.stdout, anchor_y, menu_lines)?;
                    return Ok(None);
                }
                KeyCode::Char('q') if query.is_empty() => {
                    clear_inline_fuzzy(&mut session.stdout, anchor_y, menu_lines)?;
                    return Ok(None);
                }
                KeyCode::Enter => {
                    clear_inline_fuzzy(&mut session.stdout, anchor_y, menu_lines)?;
                    let marked = active.iter().position(|value| *value);
                    let fallback = matches.get(selected).map(|(_, index)| *index);
                    return Ok(marked.or(fallback));
                }
                KeyCode::Tab => {
                    if let Some((_, index)) = matches.get(selected) {
                        for slot in active.iter_mut() {
                            *slot = false;
                        }
                        if let Some(slot) = active.get_mut(*index) {
                            *slot = true;
                        }
                    }
                }
                KeyCode::Up | KeyCode::Char('k') => selected = selected.saturating_sub(1),
                KeyCode::Down | KeyCode::Char('j') => {
                    selected = (selected + 1).min(matches.len().saturating_sub(1));
                }
                KeyCode::Backspace => {
                    query.pop();
                    selected = 0;
                    scroll = 0;
                }
                KeyCode::Char(ch) if !modifiers.contains(KeyModifiers::CONTROL) => {
                    query.push(ch);
                    selected = 0;
                    scroll = 0;
                }
                _ => {}
            }
        }
    }
}

fn fuzzy_matches(matcher: &SkimMatcherV2, items: &[String], query: &str) -> Vec<(i64, usize)> {
    let mut matches = items
        .iter()
        .enumerate()
        .filter_map(|(index, item)| {
            if query.trim().is_empty() {
                Some((0, index))
            } else {
                matcher.fuzzy_match(item, query).map(|score| (score, index))
            }
        })
        .collect::<Vec<_>>();
    if !query.trim().is_empty() {
        matches.sort_by(|left, right| right.0.cmp(&left.0).then_with(|| left.1.cmp(&right.1)));
    }
    matches
}

fn draw_inline_fuzzy(
    stdout: &mut io::Stdout,
    anchor_y: u16,
    menu_lines: u16,
    query: &str,
    items: &[String],
    matches: &[(i64, usize)],
    selected: usize,
    scroll: usize,
    active: &[bool],
) -> Result<()> {
    let (cols, _) = terminal::size().unwrap_or((80, 24));
    let bar = inline_fuzzy_bar();
    let width = (cols as usize).saturating_sub(visible_width(&bar)).max(1);
    let visible = matches.len().min(menu_lines.saturating_sub(2) as usize);
    queue!(stdout, Hide)?;
    for row in 0..menu_lines {
        queue!(
            stdout,
            MoveTo(0, anchor_y + row),
            Clear(ClearType::CurrentLine)
        )?;
    }
    queue!(
        stdout,
        MoveTo(0, anchor_y),
        Print(&bar),
        Print(inline_fuzzy_header(query, width)),
    )?;
    if matches.is_empty() {
        queue!(
            stdout,
            MoveTo(0, anchor_y + 1),
            Print(&bar),
            Print(format!("\x1b[2m{}\x1b[0m", t("no matches", "没有匹配项")))
        )?;
    } else {
        for (row, (_, item_index)) in matches.iter().skip(scroll).take(visible).enumerate() {
            queue!(
                stdout,
                MoveTo(0, anchor_y + row as u16 + 1),
                Print(&bar),
                Print(inline_fuzzy_item_line(
                    items[*item_index].as_str(),
                    scroll + row == selected,
                    active.get(*item_index).copied().unwrap_or(false),
                    width
                ))
            )?;
        }
    }
    queue!(
        stdout,
        MoveTo(0, anchor_y + menu_lines.saturating_sub(1)),
        Print(&bar),
        Print(inline_fuzzy_help_line(width))
    )?;
    stdout.flush()?;
    Ok(())
}

fn inline_fuzzy_scroll(selected: usize, scroll: usize, visible: usize) -> usize {
    if visible == 0 || selected < scroll {
        selected
    } else if selected >= scroll + visible {
        selected + 1 - visible
    } else {
        scroll
    }
}

fn inline_fuzzy_bar() -> String {
    input_prompt_bar(AgentMode::Normal)
}

fn inline_fuzzy_header(query: &str, width: usize) -> String {
    let title = t("Select model", "选择模型");
    let line = if query.trim().is_empty() {
        title.to_string()
    } else {
        format!("{title} · {}", query.trim())
    };
    format!("\x1b[1m{}\x1b[0m", truncate_visible_width(&line, width))
}

fn inline_fuzzy_item_line(item: &str, selected: bool, active: bool, width: usize) -> String {
    let marker = if active { "[*]" } else { "[ ]" };
    let line = if selected {
        format!("› {marker} {item}")
    } else {
        format!("  {marker} {item}")
    };
    let line = truncate_visible_width(&line, width);
    if selected {
        format!(
            "\x1b[1m\x1b[35m›\x1b[0m\x1b[1m{}\x1b[0m",
            line.strip_prefix('›').unwrap_or(&line)
        )
    } else if active {
        format!("\x1b[1m\x1b[32m{}\x1b[0m", line)
    } else {
        format!("\x1b[2m{}\x1b[0m", line)
    }
}

fn inline_fuzzy_help_line(width: usize) -> String {
    let line = t(
        "type search · j/k move · Tab toggle · Enter/q confirm",
        "输入搜索 · j/k 移动 · Tab 切换 · Enter/q 确认",
    );
    format!("\x1b[2m{}\x1b[0m", truncate_visible_width(line, width))
}

fn clear_inline_fuzzy(stdout: &mut io::Stdout, anchor_y: u16, lines: u16) -> Result<()> {
    for row in 0..lines {
        queue!(
            stdout,
            MoveTo(0, anchor_y + row),
            Clear(ClearType::CurrentLine)
        )?;
    }
    queue!(stdout, MoveTo(0, anchor_y), Show)?;
    stdout.flush()?;
    Ok(())
}

fn reserve_inline_fuzzy_space(lines: u16) -> Result<()> {
    for _ in 1..lines {
        println!();
    }
    io::stdout().flush()?;
    Ok(())
}

fn inline_fuzzy_lines(item_count: usize) -> u16 {
    ((item_count.min(10) + 2) as u16).max(3)
}

/// In-place single-choice fuzzy picker; same environment and screen handling
/// as `inline_pop_select` (editor suspended, cooked mode on entry). `lines`
/// are the rendered rows and `search` the parallel fuzzy-match texts.
/// Returns the selected index, or `None` when cancelled.
/// What an inline picker returned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InlineSelectOutcome {
    Cancelled,
    Chosen(usize),
    /// Ctrl+D on a row, confirmed in place. The caller does the deletion and
    /// reopens the picker on the refreshed list.
    Deleted(usize),
}

/// A picker keystroke, resolved away from the draw loop so the delete
/// confirmation flow is testable without a terminal. Every printable character
/// is search input, which is why deletion needs a modifier key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InlineSelectKey {
    Cancel,
    Accept,
    Up,
    Down,
    Backspace,
    DeleteRequest,
    Char(char),
    Ignore,
}

fn inline_select_key(code: KeyCode, modifiers: KeyModifiers, deletable: bool) -> InlineSelectKey {
    let control = modifiers.contains(KeyModifiers::CONTROL);
    match code {
        KeyCode::Char('c') if control => InlineSelectKey::Cancel,
        KeyCode::Char('d') if control && deletable => InlineSelectKey::DeleteRequest,
        KeyCode::Delete if deletable => InlineSelectKey::DeleteRequest,
        KeyCode::Esc => InlineSelectKey::Cancel,
        KeyCode::Enter => InlineSelectKey::Accept,
        KeyCode::Up | KeyCode::Char('k') if !control => InlineSelectKey::Up,
        KeyCode::Down | KeyCode::Char('j') if !control => InlineSelectKey::Down,
        KeyCode::Backspace => InlineSelectKey::Backspace,
        KeyCode::Char(ch) if !control => InlineSelectKey::Char(ch),
        _ => InlineSelectKey::Ignore,
    }
}

fn inline_single_select(
    title: &str,
    lines: &[String],
    search: &[String],
    initial_selected: usize,
) -> Result<Option<usize>> {
    match inline_single_select_deletable(title, lines, search, initial_selected, None)? {
        InlineSelectOutcome::Chosen(index) => Ok(Some(index)),
        // Unreachable without delete labels, but folding it into `None` keeps
        // callers that never opted in from having to care.
        InlineSelectOutcome::Cancelled | InlineSelectOutcome::Deleted(_) => Ok(None),
    }
}

/// Fuzzy picker. Passing `delete_labels` (one per row, used in the inline
/// confirmation) enables Ctrl+D deletion and returns `Deleted` once the user
/// confirms; the caller performs the deletion and decides whether to reopen.
fn inline_single_select_deletable(
    title: &str,
    lines: &[String],
    search: &[String],
    initial_selected: usize,
    delete_labels: Option<&[String]>,
) -> Result<InlineSelectOutcome> {
    let menu_lines = inline_fuzzy_lines(lines.len());
    reserve_inline_fuzzy_space(menu_lines)?;
    let mut session = InlineRawMode::start()?;
    let matcher = SkimMatcherV2::default();
    let mut query = String::new();
    let mut selected = initial_selected.min(lines.len().saturating_sub(1));
    let mut scroll = 0usize;
    let (_, cursor_y) = cursor::position().unwrap_or((0, menu_lines.saturating_sub(1)));
    let anchor_y = cursor_y.saturating_sub(menu_lines.saturating_sub(1));
    let deletable = delete_labels.is_some();
    // Index awaiting a y/N answer. Confirming inside the picker keeps the
    // drawing intact instead of tearing it down for a separate prompt.
    let mut confirming: Option<usize> = None;
    loop {
        let matches = fuzzy_matches(&matcher, search, &query);
        if selected >= matches.len() {
            selected = matches.len().saturating_sub(1);
        }
        let visible = matches.len().min(menu_lines.saturating_sub(2) as usize);
        scroll = inline_fuzzy_scroll(selected, scroll, visible);
        let confirm_label = confirming.and_then(|index| {
            delete_labels
                .and_then(|labels| labels.get(index))
                .map(String::as_str)
        });
        draw_inline_single(
            &mut session.stdout,
            anchor_y,
            menu_lines,
            title,
            &query,
            lines,
            &matches,
            selected,
            scroll,
            deletable,
            confirm_label,
        )?;
        let Event::Key(KeyEvent {
            code, modifiers, ..
        }) = event::read()?
        else {
            continue;
        };
        if let Some(index) = confirming {
            // Only an explicit yes deletes; every other key backs out.
            let confirmed = matches!(code, KeyCode::Char('y') | KeyCode::Char('Y'));
            confirming = None;
            if confirmed {
                clear_inline_fuzzy(&mut session.stdout, anchor_y, menu_lines)?;
                return Ok(InlineSelectOutcome::Deleted(index));
            }
            continue;
        }
        match inline_select_key(code, modifiers, deletable) {
            InlineSelectKey::Cancel => {
                clear_inline_fuzzy(&mut session.stdout, anchor_y, menu_lines)?;
                return Ok(InlineSelectOutcome::Cancelled);
            }
            InlineSelectKey::Accept => {
                let choice = matches.get(selected).map(|(_, index)| *index);
                clear_inline_fuzzy(&mut session.stdout, anchor_y, menu_lines)?;
                return Ok(match choice {
                    Some(index) => InlineSelectOutcome::Chosen(index),
                    None => InlineSelectOutcome::Cancelled,
                });
            }
            InlineSelectKey::DeleteRequest => {
                confirming = matches.get(selected).map(|(_, index)| *index);
            }
            InlineSelectKey::Up => selected = selected.saturating_sub(1),
            InlineSelectKey::Down => {
                selected = (selected + 1).min(matches.len().saturating_sub(1));
            }
            InlineSelectKey::Backspace => {
                query.pop();
                selected = 0;
                scroll = 0;
            }
            InlineSelectKey::Char(ch) => {
                query.push(ch);
                selected = 0;
                scroll = 0;
            }
            InlineSelectKey::Ignore => {}
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_inline_single(
    stdout: &mut io::Stdout,
    anchor_y: u16,
    menu_lines: u16,
    title: &str,
    query: &str,
    lines: &[String],
    matches: &[(i64, usize)],
    selected: usize,
    scroll: usize,
    deletable: bool,
    confirm_label: Option<&str>,
) -> Result<()> {
    let (cols, _) = terminal::size().unwrap_or((80, 24));
    let bar = inline_fuzzy_bar();
    let width = (cols as usize).saturating_sub(visible_width(&bar)).max(1);
    let visible = matches.len().min(menu_lines.saturating_sub(2) as usize);
    queue!(stdout, Hide)?;
    for row in 0..menu_lines {
        queue!(
            stdout,
            MoveTo(0, anchor_y + row),
            Clear(ClearType::CurrentLine)
        )?;
    }
    let header = match confirm_label {
        Some(label) => inline_single_confirm_header(label, width),
        None => inline_single_header(title, query, width),
    };
    queue!(stdout, MoveTo(0, anchor_y), Print(&bar), Print(header))?;
    if matches.is_empty() {
        queue!(
            stdout,
            MoveTo(0, anchor_y + 1),
            Print(&bar),
            Print(format!("\x1b[2m{}\x1b[0m", t("no matches", "没有匹配项")))
        )?;
    } else {
        for (row, (_, item_index)) in matches.iter().skip(scroll).take(visible).enumerate() {
            queue!(
                stdout,
                MoveTo(0, anchor_y + row as u16 + 1),
                Print(&bar),
                Print(inline_single_item_line(
                    lines[*item_index].as_str(),
                    scroll + row == selected,
                    width
                ))
            )?;
        }
    }
    queue!(
        stdout,
        MoveTo(0, anchor_y + menu_lines.saturating_sub(1)),
        Print(&bar),
        Print(inline_single_help_line(width, deletable))
    )?;
    stdout.flush()?;
    Ok(())
}

fn inline_single_header(title: &str, query: &str, width: usize) -> String {
    let line = if query.trim().is_empty() {
        title.to_string()
    } else {
        format!("{title} · {}", query.trim())
    };
    format!("\x1b[1m{}\x1b[0m", truncate_visible_width(&line, width))
}

fn inline_single_item_line(item: &str, selected: bool, width: usize) -> String {
    let line = if selected {
        format!("› {item}")
    } else {
        format!("  {item}")
    };
    let line = truncate_visible_width(&line, width);
    if selected {
        format!(
            "\x1b[1m\x1b[35m›\x1b[0m\x1b[1m{}\x1b[0m",
            line.strip_prefix('›').unwrap_or(&line)
        )
    } else {
        format!("\x1b[2m{line}\x1b[0m")
    }
}

fn inline_single_confirm_header(label: &str, width: usize) -> String {
    let line = if is_zh() {
        format!("删除「{label}」？y/N")
    } else {
        format!("delete \"{label}\"? y/N")
    };
    format!("\x1b[1m\x1b[31m{}\x1b[0m", truncate_visible_width(&line, width))
}

fn inline_single_help_line(width: usize, deletable: bool) -> String {
    let line = if deletable {
        t(
            "type search · j/k move · Enter select · Ctrl+D delete · Esc cancel",
            "输入搜索 · j/k 移动 · Enter 选择 · Ctrl+D 删除 · Esc 取消",
        )
    } else {
        t(
            "type search · j/k move · Enter select · Esc cancel",
            "输入搜索 · j/k 移动 · Enter 选择 · Esc 取消",
        )
    };
    format!("\x1b[2m{}\x1b[0m", truncate_visible_width(line, width))
}

fn truncate_display(value: &str, max: usize) -> String {
    if value.chars().count() <= max {
        value.to_string()
    } else {
        format!(
            "{}…",
            value
                .chars()
                .take(max.saturating_sub(1))
                .collect::<String>()
        )
    }
}

struct InlineRawMode {
    stdout: io::Stdout,
}

impl InlineRawMode {
    fn start() -> Result<Self> {
        terminal::enable_raw_mode()?;
        Ok(Self {
            stdout: io::stdout(),
        })
    }
}

impl Drop for InlineRawMode {
    fn drop(&mut self) {
        let _ = execute!(self.stdout, Show);
        let _ = terminal::disable_raw_mode();
    }
}

async fn run_config(paths: &MiyuPaths, args: ConfigArgs) -> Result<bool> {
    match args.command {
        Some(ConfigCommand::Validate) => {
            AppConfig::load(paths)?;
            println!(
                "{}: {}",
                t("config is valid", "配置有效"),
                paths.config_file.display()
            );
            Ok(false)
        }
        Some(ConfigCommand::Paths) => {
            paths.print();
            Ok(false)
        }
        Some(ConfigCommand::PromptSource) => {
            let config = AppConfig::load(paths)?;
            let persona = config.prompt.active_persona.trim();
            let identity = config.prompt.active_identity.trim();
            let persona_path = (!persona.is_empty()).then(|| config.persona_path(paths, persona));
            let legacy_prompt = config.custom_system_prompt(paths)?;
            let legacy_prompt_path = config.system_prompt_path(paths);
            let base_prompt_source =
                if let Some(path) = persona_path.as_ref().filter(|path| path.exists()) {
                    format!("persona ({})", path.display())
                } else if !legacy_prompt.trim().is_empty() {
                    format!("legacy_custom ({})", legacy_prompt_path.display())
                } else {
                    "built-in".to_string()
                };
            println!("base_prompt_source: {}", base_prompt_source);
            println!(
                "active_persona: {}",
                if persona.is_empty() {
                    "(none)"
                } else {
                    persona
                }
            );
            if let Some(path) = persona_path {
                println!("active_persona_file: {}", path.display());
            }
            println!(
                "active_identity: {}",
                if identity.is_empty() {
                    "(none)"
                } else {
                    identity
                }
            );
            println!("prompts_dir: {}", config.prompts_dir_path(paths).display());
            println!(
                "identities_dir: {}",
                config.identities_dir_path(paths).display()
            );
            let system_prompt = config.system_prompt(paths)?;
            println!(
                "system_prompt_first_line: {}",
                system_prompt.lines().next().unwrap_or("")
            );
            println!("system_prompt_chars: {}", system_prompt.chars().count());
            Ok(false)
        }
        None => crate::config_tui::run(paths),
    }
}

fn run_clipboard_paste(paths: &MiyuPaths) -> Result<()> {
    match crate::clipboard::read_clipboard() {
        Ok(crate::clipboard::ClipboardContent::Image(img)) => {
            let path = img.write_temp_file(&paths.cache_dir, 0)?;
            let filename = path.file_name().and_then(|n| n.to_str()).unwrap_or("image");
            print!("[Image 1: {}]", filename);
            io::stdout().flush()?;
            Ok(())
        }
        Ok(crate::clipboard::ClipboardContent::ImagePath(path)) => {
            let filename = std::path::Path::new(&path)
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("image");
            let dir = paths.cache_dir.join("clipboard_images");
            std::fs::create_dir_all(&dir)?;
            crate::clipboard::cleanup_clipboard_images(&dir);
            let link_path = dir.join(filename);
            let need_create = if link_path.is_symlink() {
                !link_path.exists()
            } else {
                !link_path.exists()
            };
            if need_create {
                if link_path.exists() || link_path.is_symlink() {
                    std::fs::remove_file(&link_path)?;
                }
                #[cfg(unix)]
                std::os::unix::fs::symlink(&path, &link_path)?;
                #[cfg(windows)]
                std::os::windows::fs::symlink_file(&path, &link_path).or_else(|_| std::fs::copy(&path, &link_path).map(|_| ()))?;
            }
            print!("[Image 1: {}]", filename);
            io::stdout().flush()?;
            Ok(())
        }
        Ok(crate::clipboard::ClipboardContent::TextPath(path)) => {
            print!("{}", path);
            io::stdout().flush()?;
            Ok(())
        }
        Ok(crate::clipboard::ClipboardContent::Text(text)) => {
            if should_summarize_pasted_text(&text) {
                let index = shell_pasted_text_index(&paths.cache_dir, &text)?;
                let placeholder = pasted_text_placeholder(index, pasted_text_line_count(&text));
                print!("{}", placeholder);
            } else {
                print!("{}", text);
            }
            io::stdout().flush()?;
            Ok(())
        }
        _ => {
            std::process::exit(1);
        }
    }
}

fn shell_pasted_text_index(cache_dir: &std::path::Path, text: &str) -> Result<usize> {
    let dir = cache_dir.join("clipboard_texts");
    std::fs::create_dir_all(&dir)?;
    let mut index = 1;
    loop {
        let path = dir.join(format!("{index}.txt"));
        if !path.exists() {
            std::fs::write(path, text)?;
            return Ok(index);
        }
        index += 1;
    }
}

fn shell_message_from_input(use_stdin: bool, message: Vec<String>) -> Result<String> {
    if use_stdin {
        let mut input = String::new();
        io::stdin().read_to_string(&mut input)?;
        Ok(input)
    } else {
        Ok(join_message(message))
    }
}

fn run_shell_classify(shell_name: &str, message: &str) -> Result<()> {
    if !matches!(shell_name, "fish" | "bash" | "zsh") {
        std::process::exit(2);
    }
    if shell::is_shell_command(message, shell_name) {
        std::process::exit(0);
    }
    std::process::exit(1);
}

async fn run_shell_intercept(paths: &MiyuPaths, shell_name: &str, message: String) -> Result<()> {
    if !matches!(shell_name, "fish" | "bash" | "zsh") {
        bail!("{}: {shell_name}", t("unsupported shell", "不支持的 shell"));
    }
    if message.trim().is_empty() {
        bail!(
            "{}",
            t("not a natural language command", "不是自然语言命令")
        );
    }

    let message = expand_shell_pasted_text_placeholders(paths, &message)?;
    let (clean_message, pasted_images) = extract_image_placeholders(&message);

    let result = if pasted_images.is_empty() {
        // shell-hook keeps landing in the terminal session: that lane is the
        // whole point of typing natural language at the prompt.
        run_chat_with_options(
            paths,
            clean_message,
            None,
            false,
            AgentMode::Normal,
            TurnSession::Current,
        )
        .await
    } else {
        run_chat_with_images(paths, clean_message, pasted_images).await
    };
    drain_stdin();
    if let Err(err) = &result {
        println!("\x1b[31m{}: {err}\x1b[0m", t("error", "错误"));
    }
    result
}

fn expand_shell_pasted_text_placeholders(paths: &MiyuPaths, message: &str) -> Result<String> {
    let placeholders = find_pasted_text_placeholders(message);
    if placeholders.is_empty() {
        return Ok(message.to_string());
    }

    let chars: Vec<char> = message.chars().collect();
    let mut expanded = String::new();
    let mut last_end = 0;
    let dir = paths.cache_dir.join("clipboard_texts");
    for (start, end, index) in placeholders {
        expanded.extend(&chars[last_end..start]);
        let path = dir.join(format!("{index}.txt"));
        match std::fs::read_to_string(&path) {
            Ok(text) => expanded.push_str(&text),
            Err(_) => expanded.extend(&chars[start..end]),
        }
        last_end = end;
    }
    expanded.extend(&chars[last_end..]);
    Ok(expanded)
}

fn extract_image_placeholders(
    message: &str,
) -> (String, Vec<Option<crate::clipboard::PastedImage>>) {
    let placeholders = find_image_placeholders(message);
    if placeholders.is_empty() {
        return (message.to_string(), Vec::new());
    }

    let cache_images_dir = MiyuPaths::new()
        .map(|p| p.cache_dir.join("clipboard_images"))
        .ok();

    let chars: Vec<char> = message.chars().collect();
    let mut clean = String::new();
    let mut images: Vec<Option<crate::clipboard::PastedImage>> = Vec::new();
    let mut last_end = 0;

    for (start, end) in &placeholders {
        clean.extend(&chars[last_end..*start]);
        let segment: String = chars[*start..*end].iter().collect();
        let name_str = segment
            .strip_prefix("[Image ")
            .and_then(|s| s.strip_prefix(|c: char| c.is_ascii_digit()))
            .and_then(|s| s.strip_prefix(':'))
            .and_then(|s| s.strip_suffix(']'))
            .map(|s| s.trim().to_string());

        if let Some(name_str) = name_str {
            if let Some(dir) = &cache_images_dir {
                let candidate = dir.join(&name_str);
                if candidate.exists() {
                    images.push(Some(crate::clipboard::PastedImage::Path(
                        candidate.display().to_string(),
                    )));
                } else {
                    images.push(None);
                }
            } else {
                images.push(None);
            }
        } else {
            images.push(None);
        }
        clean.push_str(&format!("[Image {}]", images.len()));
        last_end = *end;
    }
    clean.extend(&chars[last_end..]);

    (clean, images)
}

async fn run_chat_with_images(
    paths: &MiyuPaths,
    message: String,
    pasted_images: Vec<Option<crate::clipboard::PastedImage>>,
) -> Result<()> {
    if !direct_mode_requested() {
        match try_run_remote_chat(
            paths,
            None,
            &message,
            None,
            false,
            AgentMode::Normal,
            &pasted_images,
            None,
            None,
        )
        .await
        {
            Ok(Some(_)) => return Ok(()),
            Ok(None) => {}
            Err(err) => return Err(err),
        }
    }
    let _core_lease = ipc::acquire_direct_core(paths)?;
    initialize_models_cache(paths);
    AppConfig::init_files(paths)?;
    let config = AppConfig::load_or_default(paths)?;
    let state = StateStore::new(paths)?;
    state.init_files()?;
    let memory_organizer = MemoryOrganizer::spawn()?;
    let memory_organizer_handle = memory_organizer.handle();
    memory_organizer_handle.wake(config.clone(), paths.clone(), state.clone());
    let client = OpenAiCompatibleClient::from_config(&config, paths)?;
    let registry = build_tool_registry(
        &config,
        paths,
        AgentMode::Normal,
        crate::question_tui::available(false),
    )?;
    let reasoning_mode = render::ReasoningDisplayMode::from_config(&config.display.reasoning);
    let tool_call_mode = render::ToolCallDisplayMode::from_config(&config.display.tool_calls);
    let readable_tool_names = config.display.readable_tool_names;
    let command_output_lines = config.display.command_output_lines;
    let show_token_usage = config.display.show_token_usage;
    let show_mixed_model_endpoint = show_mixed_model_endpoint(&config, false);
    let display_config = config.clone();
    let mut agent = Agent::new(
        config,
        paths,
        state.clone(),
        client,
        registry,
        AgentMode::Normal,
    )?;
    agent.set_memory_organizer(memory_organizer_handle);
    agent.prepare_for_turn()?;
    let mut renderer = render::StreamRenderer::new(
        reasoning_mode,
        tool_call_mode,
        false,
        readable_tool_names,
        command_output_lines,
    );
    renderer.start_waiting()?;
    let result = agent
        .chat_stream_with_images(&message, &pasted_images, |event| {
            handle_agent_event(&mut renderer, event)
        })
        .await;
    renderer.finish()?;
    let result = match result {
        Ok(result) => result,
        Err(err) if crate::question::is_question_cancelled(&err) => return Ok(()),
        Err(err) => return Err(err),
    };
    print_mixed_model_endpoint(show_mixed_model_endpoint, &result, None);
    let mut cumulative_tokens = TurnTokens::from_usage(result.usage.as_ref());
    let context_tokens = agent.effective_context_tokens()?;
    print_chat_token_usage(
        &result,
        show_token_usage,
        context_tokens,
        result_context_window(&display_config, &result).or(agent.context_window()),
        cumulative_tokens,
    )?;
    let overflow_result = handle_post_turn_overflow(
        &agent,
        &mut renderer,
        context_tokens,
        show_token_usage,
        Some(&mut cumulative_tokens),
    )
    .await?;
    let updated_context_tokens = agent.effective_context_tokens()?;
    if overflow_result.is_none() && updated_context_tokens != context_tokens {
        print_chat_token_usage(
            &result,
            show_token_usage,
            updated_context_tokens,
            result_context_window(&display_config, &result).or(agent.context_window()),
            cumulative_tokens,
        )?;
    }
    Ok(())
}

#[cfg(not(unix))]
fn drain_stdin() {}

#[cfg(unix)]
fn drain_stdin() {
    use std::os::fd::AsRawFd;

    let stdin = io::stdin();
    if !stdin.is_terminal() {
        return;
    }
    let fd = stdin.as_raw_fd();
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 {
        return;
    }
    if unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return;
    }

    let mut handle = stdin.lock();
    let mut buffer = [0_u8; 4096];
    loop {
        match handle.read(&mut buffer) {
            Ok(0) => break,
            Ok(_) => continue,
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => break,
            Err(_) => break,
        }
    }

    let _ = unsafe { libc::fcntl(fd, libc::F_SETFL, flags) };
}

const STDIN_MAX_CHARS: usize = 50_000;
const STDIN_TIMEOUT_SECS: u64 = 5;

#[cfg(unix)]
async fn append_stdin_if_piped(message: String) -> String {
    if io::stdin().is_terminal() {
        return message;
    }
    // The reader thread bounds itself with poll() deadlines instead of being
    // abandoned by an outer timeout: a thread stuck in a blocking read(0)
    // would make the tokio runtime hang forever on shutdown (the process
    // then never exits when stdin is a never-closing pipe).
    let read_result = tokio::task::spawn_blocking(|| -> std::io::Result<String> {
        use std::os::fd::AsRawFd;
        let stdin = std::io::stdin();
        let fd = stdin.as_raw_fd();
        let mut buf: Vec<u8> = Vec::new();
        let deadline =
            std::time::Instant::now() + std::time::Duration::from_secs(STDIN_TIMEOUT_SECS);
        loop {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() || buf.len() >= STDIN_MAX_CHARS {
                break;
            }
            let mut pollfd = libc::pollfd {
                fd,
                events: libc::POLLIN,
                revents: 0,
            };
            let timeout_ms = remaining.as_millis().min(i32::MAX as u128) as i32;
            let ready = unsafe { libc::poll(&mut pollfd, 1, timeout_ms) };
            if ready <= 0 {
                break;
            }
            let mut chunk = [0u8; 8192];
            let count = unsafe { libc::read(fd, chunk.as_mut_ptr().cast(), chunk.len()) };
            if count < 0 {
                let error = std::io::Error::last_os_error();
                if error.kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(error);
            }
            if count == 0 {
                break;
            }
            buf.extend_from_slice(&chunk[..count as usize]);
        }
        buf.truncate(STDIN_MAX_CHARS);
        Ok(String::from_utf8_lossy(&buf).into_owned())
    })
    .await;

    let stdin_content = match read_result {
        Ok(Ok(content)) if !content.trim().is_empty() => content.trim().to_string(),
        _ => return message,
    };

    if message.is_empty() {
        stdin_content
    } else {
        format!("{message}\n\n---\n(stdin)\n{stdin_content}")
    }
}

#[cfg(not(unix))]
async fn append_stdin_if_piped(message: String) -> String {
    if io::stdin().is_terminal() {
        return message;
    }
    let read_result = tokio::task::spawn_blocking(|| -> std::io::Result<String> {
        let mut buf = String::new();
        std::io::Read::read_to_string(&mut std::io::stdin(), &mut buf)?;
        if buf.len() > STDIN_MAX_CHARS {
            buf.truncate(STDIN_MAX_CHARS);
        }
        Ok(buf)
    })
    .await;

    let stdin_content = match read_result {
        Ok(Ok(content)) if !content.trim().is_empty() => content.trim().to_string(),
        _ => return message,
    };

    if message.is_empty() {
        stdin_content
    } else {
        format!("{message}\n\n---\n(stdin)\n{stdin_content}")
    }
}

/// Which session a one-shot CLI turn lands in.
#[derive(Clone, Debug, PartialEq, Eq)]
enum TurnSession {
    /// The terminal session — what shell-hook and `miyu new`/`session` drive.
    Current,
    /// An explicit `--session` target, resolved to a session id.
    Explicit(String),
    /// A throwaway session created for this turn and deleted right after, so a
    /// quick question never lands in a conversation the user cares about.
    Ephemeral,
}

/// Picks the session for `miyu ask` / a bare `miyu '<message>'`. Both default
/// to a throwaway session; `--session` and `--continue` opt back into a real
/// one (clap already rejects passing both).
async fn one_shot_session(
    paths: &MiyuPaths,
    session_arg: Option<&str>,
    continue_session: bool,
) -> Result<TurnSession> {
    if let Some(arg) = session_arg {
        return Ok(TurnSession::Explicit(
            resolve_session_id_for_turn(paths, arg).await?,
        ));
    }
    if continue_session {
        return Ok(TurnSession::Current);
    }
    Ok(TurnSession::Ephemeral)
}

/// Named rather than left blank on purpose: a row that leaks past the sweep is
/// recognisable, and a non-empty name also skips the daemon's auto-title LLM
/// call (`maybe_auto_name_session`) for a session about to be deleted.
fn ephemeral_session_name() -> String {
    t("One-shot", "一次性对话").to_string()
}

async fn create_ephemeral_session(paths: &MiyuPaths) -> Result<String> {
    let (_, data) = session_admin(
        paths,
        IpcCommand::CreateSession {
            name: Some(ephemeral_session_name()),
            switch: false,
            kind: Some(crate::state::ASK_SESSION_KIND.to_string()),
        },
    )
    .await?;
    data.get("session")
        .and_then(|session| session.get("session_id"))
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| anyhow::anyhow!("Miyu core returned an invalid response"))
}

/// Tears a throwaway session down. Background jobs go first so nothing is left
/// pointing at a session that is about to disappear. Best effort: a daemon
/// that has gone away leaves a row the startup sweep collects.
async fn discard_ephemeral_session(paths: &MiyuPaths, session_id: &str) {
    let _ = send_ipc_admin(
        paths,
        IpcCommand::StopSessionJobs {
            session_id: session_id.to_string(),
        },
    )
    .await;
    let _ = send_ipc_admin(
        paths,
        IpcCommand::DeleteSession {
            target: crate::ipc::SessionRef::Id {
                id: session_id.to_string(),
            },
        },
    )
    .await;
}

/// Deletes the throwaway session however the direct-mode turn unwinds — error,
/// cancelled question, or early return.
struct EphemeralSessionGuard {
    state: StateStore,
    session_id: String,
}

impl Drop for EphemeralSessionGuard {
    fn drop(&mut self) {
        let _ = self.state.delete_session(&self.session_id);
    }
}

async fn run_chat_with_options(
    paths: &MiyuPaths,
    message: String,
    show_reasoning: Option<bool>,
    plain: bool,
    mode: AgentMode,
    session: TurnSession,
) -> Result<()> {
    let message = append_stdin_if_piped(message).await;
    if message.is_empty() {
        return run_repl(paths, mode).await;
    }
    if !direct_mode_requested() {
        let session_override = match &session {
            TurnSession::Current => None,
            TurnSession::Explicit(session_id) => Some(session_id.clone()),
            TurnSession::Ephemeral => Some(create_ephemeral_session(paths).await?),
        };
        // Not `?`-through: the throwaway session has to be torn down on the
        // failure path too, otherwise a cancelled turn leaves it behind.
        let outcome = try_run_remote_chat(
            paths,
            None,
            &message,
            show_reasoning,
            plain,
            mode,
            &[],
            session_override.clone(),
            None,
        )
        .await;
        if session == TurnSession::Ephemeral {
            if let Some(session_id) = &session_override {
                discard_ephemeral_session(paths, session_id).await;
            }
        }
        match outcome {
            Ok(Some(_)) => return Ok(()),
            Ok(None) => {}
            Err(err) => return Err(err),
        }
    }
    let _core_lease = ipc::acquire_direct_core(paths)?;
    initialize_models_cache(paths);
    AppConfig::init_files(paths)?;
    let config = AppConfig::load_or_default(paths)?;
    let state = StateStore::new(paths)?;
    state.init_files()?;
    // Direct mode has no daemon to mint the throwaway session, so it makes its
    // own and pins the turn to it.
    let (state, _ephemeral_guard) = if session == TurnSession::Ephemeral {
        let record = state.create_session(
            &config.active_persona_scope(),
            &ephemeral_session_name(),
            crate::state::ASK_SESSION_KIND,
            None,
        )?;
        let guard = EphemeralSessionGuard {
            state: state.clone(),
            session_id: record.session_id.clone(),
        };
        (state.pinned(&record.session_id), Some(guard))
    } else {
        (state, None)
    };
    let memory_organizer = MemoryOrganizer::spawn()?;
    let memory_organizer_handle = memory_organizer.handle();
    memory_organizer_handle.wake(config.clone(), paths.clone(), state.clone());
    let client = OpenAiCompatibleClient::from_config(&config, paths)?;
    let registry =
        build_tool_registry(&config, paths, mode, crate::question_tui::available(plain))?;
    let reasoning_mode = if show_reasoning == Some(false) {
        render::ReasoningDisplayMode::Hidden
    } else {
        render::ReasoningDisplayMode::from_config(&config.display.reasoning)
    };
    let tool_call_mode = if plain {
        render::ToolCallDisplayMode::Hidden
    } else {
        render::ToolCallDisplayMode::from_config(&config.display.tool_calls)
    };
    let readable_tool_names = config.display.readable_tool_names;
    let command_output_lines = config.display.command_output_lines;
    let show_token_usage = config.display.show_token_usage && !plain;
    let show_mixed_model_endpoint = show_mixed_model_endpoint(&config, false);
    let display_config = config.clone();
    let mut agent = Agent::new(config, paths, state.clone(), client, registry, mode)?;
    agent.set_memory_organizer(memory_organizer_handle);
    agent.prepare_for_turn()?;
    let mut renderer = render::StreamRenderer::new(
        reasoning_mode,
        tool_call_mode,
        plain,
        readable_tool_names,
        command_output_lines,
    );
    renderer.start_waiting()?;
    let result = agent
        .chat_stream(&message, |event| handle_agent_event(&mut renderer, event))
        .await;
    renderer.finish()?;
    let result = match result {
        Ok(result) => result,
        Err(err) if crate::question::is_question_cancelled(&err) => return Ok(()),
        Err(err) => return Err(err),
    };
    print_mixed_model_endpoint(show_mixed_model_endpoint, &result, None);
    let mut cumulative_tokens = TurnTokens::from_usage(result.usage.as_ref());
    let context_tokens = agent.effective_context_tokens()?;
    print_chat_token_usage(
        &result,
        show_token_usage,
        context_tokens,
        result_context_window(&display_config, &result).or(agent.context_window()),
        cumulative_tokens,
    )?;
    let overflow_result = handle_post_turn_overflow(
        &agent,
        &mut renderer,
        context_tokens,
        show_token_usage,
        Some(&mut cumulative_tokens),
    )
    .await?;
    let updated_context_tokens = agent.effective_context_tokens()?;
    if overflow_result.is_none() && updated_context_tokens != context_tokens {
        print_chat_token_usage(
            &result,
            show_token_usage,
            updated_context_tokens,
            result_context_window(&display_config, &result).or(agent.context_window()),
            cumulative_tokens,
        )?;
    }
    Ok(())
}

struct RemoteTurnSummary {
    result: ChatResult,
    context_tokens: u64,
    context_window: Option<usize>,
    cumulative_tokens: TurnTokens,
}

/// Marker error for a remote turn interrupted by the user (Ctrl+C) or a
/// cancel from another client. The REPL catches it and returns to the prompt
/// instead of exiting; one-shot mode surfaces it as a normal error message.
#[derive(Debug)]
struct RemoteTurnCancelled;

impl std::fmt::Display for RemoteTurnCancelled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(t("cancelled", "已取消"))
    }
}

impl std::error::Error for RemoteTurnCancelled {}

fn is_remote_turn_cancelled(error: &anyhow::Error) -> bool {
    error.downcast_ref::<RemoteTurnCancelled>().is_some()
}

#[allow(clippy::too_many_arguments)]
async fn try_run_remote_chat(
    paths: &MiyuPaths,
    mut live: Option<&mut LiveReplTail>,
    message: &str,
    show_reasoning: Option<bool>,
    plain: bool,
    mode: AgentMode,
    images: &[Option<crate::clipboard::PastedImage>],
    session_override: Option<String>,
    jobs_feed: Option<&JobsFeed>,
) -> Result<Option<RemoteTurnSummary>> {
    let refreshed_paths = if direct_mode_requested() {
        None
    } else {
        // ensure_daemon also restarts a daemon left over from an older build.
        // Re-resolve paths because that shutdown may complete legacy layout migration.
        ipc::ensure_daemon(paths, None).await?;
        Some(MiyuPaths::new()?)
    };
    let paths = refreshed_paths.as_ref().unwrap_or(paths);
    let mut stream = if direct_mode_requested() {
        match ipc::connect(&paths.ipc_socket()).await {
            Ok(stream) => stream,
            Err(_) => return Ok(None),
        }
    } else {
        ipc::connect(&paths.ipc_socket()).await?
    };
    // Turns run in parallel daemon-side: a running turn in this session does
    // not block a new one (the old multi-process placeholder semantics).
    let state_probe = StateStore::new(paths)?;
    let state_probe = session_override
        .as_deref()
        .map(|session_id| state_probe.pinned(session_id))
        .unwrap_or(state_probe);
    ipc::send(
        &mut stream,
        &IpcRequest::new(IpcCommand::StartTurn {
            content: message.to_string(),
            mode: ipc_mode_name(mode).to_string(),
            images: ipc_images(images),
            cwd: std::env::current_dir().ok(),
            session_id: session_override,
        }),
    )
    .await?;
    let Some(first) = ipc::receive::<IpcFrame>(&mut stream).await? else {
        bail!("Miyu core closed the connection before accepting the turn");
    };
    let run_id = match first {
        IpcFrame::Accepted { run_id, .. } => run_id,
        IpcFrame::Error { message, .. } => bail!("{message}"),
        _ => bail!("Miyu core returned an invalid response"),
    };
    let mut turn_id: Option<String> = None;

    let config = AppConfig::load_or_default(paths)?;
    let reasoning_mode = if show_reasoning == Some(false) {
        render::ReasoningDisplayMode::Hidden
    } else {
        render::ReasoningDisplayMode::from_config(&config.display.reasoning)
    };
    let tool_call_mode = if plain {
        render::ToolCallDisplayMode::Hidden
    } else {
        render::ToolCallDisplayMode::from_config(&config.display.tool_calls)
    };
    let mut renderer = render::StreamRenderer::new(
        reasoning_mode,
        tool_call_mode,
        plain,
        config.display.readable_tool_names,
        config.display.command_output_lines,
    );
    let queue_state = Some(state_probe);
    if let Some(live) = live.as_deref_mut() {
        renderer.use_external_cursor_control();
        renderer.use_buffered_output();
        live.external_output_active = false;
        if !live.rendered {
            live.resume_at(live.output_cursor)?;
        }
    }
    // Keep the terminal in raw mode during the turn so the editor stays
    // interactive: typed input is queued for the running turn, mirroring the
    // direct REPL's input pump.
    let mut raw = match live.as_deref_mut() {
        Some(live) => Some(if std::mem::take(&mut live.raw_mode_handoff) {
            LiveRawMode::adopt()
        } else {
            LiveRawMode::start()?
        }),
        None => None,
    };
    renderer.start_waiting()?;
    if let Some(live) = live.as_deref_mut() {
        live.apply_renderer_frame(&mut renderer)?;
    }
    let mut content = String::new();
    let mut reasoning = String::new();
    let mut spinner_tick = tokio::time::interval(Duration::from_millis(33));
    let mut job_strip_tick: u32 = 0;
    spinner_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    spinner_tick.tick().await;
    let mut input_tick = tokio::time::interval(Duration::from_millis(16));
    input_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    input_tick.tick().await;
    let completion = loop {
        // The receive future must survive across select iterations: dropping
        // it after it consumed the 4-byte length prefix (but before the
        // payload arrived) would desynchronize the frame stream.
        let recv = ipc::receive::<IpcFrame>(&mut stream);
        tokio::pin!(recv);
        let frame = loop {
            tokio::select! {
                biased;
                _ = input_tick.tick(), if live.is_some() => {
                    if !event::poll(Duration::ZERO)? {
                        continue;
                    }
                    let event = event::read()?;
                    let Some(live_tail) = live.as_deref_mut() else {
                        continue;
                    };
                    if matches!(
                        &event,
                        Event::Key(KeyEvent {
                            code: KeyCode::Enter,
                            kind,
                            ..
                        }) if *kind != KeyEventKind::Release
                    ) && live_tail.editor.input.trim_start().starts_with('/')
                    {
                        if live_tail.external_output_active {
                            continue;
                        }
                        renderer.write_system_message(t(
                            "REPL commands are available after the current reply finishes",
                            "当前回复结束后才能执行 REPL 命令",
                        ))?;
                        // write_system_message tears down the wait spinner;
                        // restart it so progress keeps rendering.
                        renderer.start_waiting()?;
                        live_tail.apply_renderer_frame(&mut renderer)?;
                        continue;
                    }
                    match live_tail.editor.handle_event(event, paths, true)? {
                        LiveEditorAction::None => {}
                        LiveEditorAction::Redraw if !live_tail.external_output_active => {
                            synchronized_terminal_update(CursorAfterUpdate::Preserve, || {
                                live_tail.redraw()
                            })?
                        }
                        LiveEditorAction::ClearScreen if !live_tail.external_output_active => {
                            synchronized_terminal_update(CursorAfterUpdate::Preserve, || {
                                live_tail.clear_screen()
                            })?
                        }
                        LiveEditorAction::Redraw | LiveEditorAction::ClearScreen => {}
                        LiveEditorAction::EmptySubmit => {}
                        LiveEditorAction::Submit(submission) => {
                            let Some(target_turn_id) = turn_id.as_deref() else {
                                live_tail.editor.input = submission.display_content.clone();
                                live_tail.editor.cursor = live_tail.editor.input.chars().count();
                                renderer.write_system_message(t(
                                    "the reply is still starting; try sending the follow-up again",
                                    "当前回复仍在启动，请稍后重新发送追加消息",
                                ))?;
                                live_tail.apply_renderer_frame(&mut renderer)?;
                                continue;
                            };
                            match persist_remote_queued_submission(
                                paths,
                                &run_id,
                                target_turn_id,
                                &submission,
                            ).await {
                                Ok(prompt) => {
                                    live_tail.editor.record_history(&submission.content);
                                    if live_tail.external_output_active {
                                        live_tail.append_queued(prompt);
                                    } else {
                                        synchronized_terminal_update(
                                            CursorAfterUpdate::Preserve,
                                            || live_tail.enqueue(prompt),
                                        )?;
                                    }
                                }
                                Err(_) => {
                                    live_tail.editor.input =
                                        submission.display_content.clone();
                                    live_tail.editor.cursor =
                                        live_tail.editor.input.chars().count();
                                    renderer.write_system_message(t(
                                        "could not queue the message; the reply may have just finished",
                                        "无法排队消息；当前回复可能刚刚结束",
                                    ))?;
                                    live_tail.apply_renderer_frame(&mut renderer)?;
                                }
                            }
                        }
                        LiveEditorAction::Interrupt | LiveEditorAction::Exit => {
                            let _ = send_ipc_command(
                                paths,
                                IpcCommand::Cancel { run_id: run_id.clone() },
                            )
                            .await;
                        }
                    }
                },
                frame = &mut recv => break frame?,
                _ = spinner_tick.tick() => {
                    renderer.tick_spinner()?;
                    if let Some(live) = live.as_deref_mut() {
                        live.apply_renderer_frame(&mut renderer)?;
                        // The job strip is part of the live tail, so it keeps
                        // rendering during streaming; throttle to ~every 8th
                        // spinner frame.
                        if let Some(feed) = jobs_feed {
                            job_strip_tick = job_strip_tick.wrapping_add(1);
                            if job_strip_tick % 8 == 0 && !live.external_output_active {
                                if live.set_jobs(feed.current()) {
                                    synchronized_terminal_update(
                                        CursorAfterUpdate::Preserve,
                                        || live.redraw(),
                                    )?;
                                } else {
                                    live.tick_job_strip()?;
                                }
                            }
                        }
                    }
                }
                _ = tokio::signal::ctrl_c() => {
                    let _ = send_ipc_command(
                        paths,
                        IpcCommand::Cancel { run_id: run_id.clone() },
                    ).await;
                    renderer.finish()?;
                    if let Some(live) = live.as_deref_mut() {
                        live.apply_renderer_frame(&mut renderer)?;
                    }
                    return Err(anyhow::Error::new(RemoteTurnCancelled));
                }
            }
        };
        let Some(frame) = frame else {
            renderer.finish()?;
            if let Some(live) = live.as_deref_mut() {
                live.apply_renderer_frame(&mut renderer)?;
            }
            bail!("Miyu core disconnected during the turn");
        };
        let IpcFrame::Event { kind, data, .. } = frame else {
            if let IpcFrame::Error { message, .. } = frame {
                renderer.finish()?;
                if let Some(live) = live.as_deref_mut() {
                    live.apply_renderer_frame(&mut renderer)?;
                }
                bail!("{message}");
            }
            continue;
        };
        match kind.as_str() {
            "turn.started" => {
                let id = ipc_text(&data, "turn_id");
                if !id.is_empty() {
                    turn_id = Some(id.to_string());
                }
            }
            "assistant.delta" => {
                let delta = ipc_text(&data, "delta");
                content.push_str(delta);
                handle_agent_event(
                    &mut renderer,
                    AgentEvent::Chunk(ChatStreamChunk {
                        kind: crate::llm::ChatStreamKind::Content,
                        text: delta.to_string(),
                    }),
                )?;
            }
            "reasoning.delta" => {
                let delta = ipc_text(&data, "delta");
                reasoning.push_str(delta);
                handle_agent_event(
                    &mut renderer,
                    AgentEvent::Chunk(ChatStreamChunk {
                        kind: crate::llm::ChatStreamKind::Reasoning,
                        text: delta.to_string(),
                    }),
                )?;
            }
            "reasoning.start" => handle_agent_event(
                &mut renderer,
                AgentEvent::ReasoningStart {
                    received_at: Instant::now(),
                },
            )?,
            "reasoning.reset" => {
                reasoning.clear();
                handle_agent_event(
                    &mut renderer,
                    AgentEvent::ReasoningReset {
                        received_at: Instant::now(),
                    },
                )?;
            }
            "reasoning.part_start" => handle_agent_event(
                &mut renderer,
                AgentEvent::ReasoningPartStart {
                    received_at: Instant::now(),
                },
            )?,
            "reasoning.part_end" => handle_agent_event(
                &mut renderer,
                AgentEvent::ReasoningPartEnd {
                    received_at: Instant::now(),
                },
            )?,
            "reasoning.title" => handle_agent_event(
                &mut renderer,
                AgentEvent::ReasoningTitle(ipc_text(&data, "title").to_string()),
            )?,
            "tool.preparing" => handle_agent_event(
                &mut renderer,
                AgentEvent::ToolPreparing {
                    name: ipc_text(&data, "name").to_string(),
                },
            )?,
            "tool.started" => handle_agent_event(
                &mut renderer,
                AgentEvent::ToolCall {
                    call_id: ipc_text(&data, "tool_id").to_string(),
                    name: ipc_text(&data, "name").to_string(),
                    arguments: ipc_text(&data, "arguments").to_string(),
                },
            )?,
            "tool.progress" => handle_agent_event(
                &mut renderer,
                AgentEvent::ToolProgress {
                    call_id: ipc_text(&data, "tool_id").to_string(),
                    name: ipc_text(&data, "name").to_string(),
                    message: ipc_text(&data, "message").to_string(),
                },
            )?,
            "tool.output" => handle_agent_event(
                &mut renderer,
                AgentEvent::CommandOutput {
                    call_id: ipc_text(&data, "tool_id").to_string(),
                    name: ipc_text(&data, "name").to_string(),
                    stream: if ipc_text(&data, "stream") == "stderr" {
                        tools::CommandOutputStream::Stderr
                    } else {
                        tools::CommandOutputStream::Stdout
                    },
                    chunk: ipc_text(&data, "output").as_bytes().to_vec(),
                },
            )?,
            "tool.finished" => {
                handle_agent_event(
                    &mut renderer,
                    AgentEvent::ToolResult {
                        call_id: ipc_text(&data, "tool_id").to_string(),
                        name: ipc_text(&data, "name").to_string(),
                        ok: data
                            .get("ok")
                            .and_then(serde_json::Value::as_bool)
                            .unwrap_or(false),
                        output: ipc_text(&data, "output").to_string(),
                    },
                )?;
                if let Some(live) = live.as_deref_mut() {
                    if live.external_output_active {
                        live.external_output_active = false;
                        live.output_cursor = cursor_position_or(live.output_cursor);
                        live.resume_at(live.output_cursor)?;
                        live.apply_renderer_frame(&mut renderer)?;
                    }
                }
            }
            "tool.image" => {
                renderer.prepare_for_external_output()?;
                if let Some(live) = live.as_deref_mut() {
                    live.apply_renderer_frame(&mut renderer)?;
                    synchronized_terminal_update(CursorAfterUpdate::Hidden, || live.suspend())?;
                    live.external_output_active = true;
                }
                let state = queue_state
                    .as_ref()
                    .expect("queue state exists for a remote turn");
                let size = (ipc_text(&data, "name") == "show_meme")
                    .then(|| tools::memes::configured_meme_size(&config.plugins.memes))
                    .flatten();
                if let Err(error) = render_remote_tool_image(state, &data, size).await {
                    renderer.write_system_message(&format!(
                        "{}: {error}",
                        t("Could not display tool image", "工具图片显示失败")
                    ))?;
                }
            }
            "question.requested" => {
                renderer.prepare_for_external_output()?;
                if let Some(live) = live.as_deref_mut() {
                    live.apply_renderer_frame(&mut renderer)?;
                    synchronized_terminal_update(CursorAfterUpdate::Hidden, || live.suspend())?;
                }
                let request = crate::question::QuestionRequest {
                    questions: serde_json::from_value(
                        data.get("questions").cloned().unwrap_or_default(),
                    )?,
                };
                notify_if_unfocused(
                    &config,
                    live.as_deref().map(|live| live.editor.focused),
                    t("Miyu is waiting on you", "Miyu 在等你回答"),
                    request
                        .questions
                        .first()
                        .map(|prompt| prompt.question.as_str())
                        .unwrap_or_default(),
                );
                // A panel that cannot be shown is not a reason to abort the
                // turn: fall through to the same path a closed panel takes, so
                // the daemon gets an answer instead of the run dying on an
                // error the user cannot act on. The direct-mode handler has
                // always done this; this branch used to propagate instead.
                let asked = crate::question_tui::ask(&request).unwrap_or_else(|err| {
                    crate::question::QuestionResponse::Unavailable(err.to_string())
                });
                match asked {
                    crate::question::QuestionResponse::Answered(answers) => {
                        send_ipc_command(
                            paths,
                            IpcCommand::AnswerQuestion {
                                question_id: ipc_text(&data, "question_id").to_string(),
                                answers,
                            },
                        )
                        .await?;
                        renderer.start_waiting()?;
                    }
                    // Nobody could be shown the panel — no tty, or it failed to
                    // open. That is not the user calling the turn off, so the
                    // question is resolved and the turn carries on; the tool
                    // that asked finds out that nobody answered and can say so.
                    crate::question::QuestionResponse::Unavailable(_) => {
                        let _ = send_ipc_command(
                            paths,
                            IpcCommand::CloseQuestion {
                                question_id: ipc_text(&data, "question_id").to_string(),
                            },
                        )
                        .await;
                    }
                    // The terminal question UI maps its close gestures to
                    // Cancelled; that one really is "stop this turn".
                    crate::question::QuestionResponse::Closed
                    | crate::question::QuestionResponse::Cancelled => {
                        let _ = send_ipc_command(
                            paths,
                            IpcCommand::Cancel {
                                run_id: run_id.clone(),
                            },
                        )
                        .await;
                    }
                }
                if let Some(live) = live.as_deref_mut() {
                    live.external_output_active = false;
                    live.output_cursor = cursor_position_or(live.output_cursor);
                    live.resume_at(live.output_cursor)?;
                }
            }
            "queue.consumed" => {
                if let Some(live) = live.as_deref_mut() {
                    let prompt_ids: Vec<String> = data
                        .get("prompt_ids")
                        .and_then(serde_json::Value::as_array)
                        .map(|values| {
                            values
                                .iter()
                                .filter_map(|value| value.as_str().map(str::to_string))
                                .collect()
                        })
                        .unwrap_or_default();
                    let consumed_mode = match ipc_text(&data, "mode") {
                        "chat" => AgentMode::Chat,
                        _ => AgentMode::Normal,
                    };
                    renderer.prepare_for_external_output()?;
                    live.apply_renderer_frame(&mut renderer)?;
                    synchronized_terminal_update(CursorAfterUpdate::Preserve, || {
                        live.suspend()?;
                        live.consume_queued(&prompt_ids, consumed_mode)
                    })?;
                }
            }
            "queue.removed" => {
                if let Some(live) = live.as_deref_mut() {
                    if let Some(prompt_id) = data.get("prompt_id").and_then(serde_json::Value::as_str)
                    {
                        synchronized_terminal_update(CursorAfterUpdate::Preserve, || {
                            live.drop_queued(&[prompt_id.to_string()])
                        })?;
                    }
                }
            }
            "generation.superseded" => {
                content.clear();
                reasoning.clear();
                handle_agent_event(
                    &mut renderer,
                    AgentEvent::ReasoningReset {
                        received_at: Instant::now(),
                    },
                )?;
            }
            "context.compact_start" => handle_agent_event(&mut renderer, AgentEvent::CompactStart)?,
            "context.compact_delta" => handle_agent_event(
                &mut renderer,
                AgentEvent::CompactChunk(ChatStreamChunk {
                    kind: crate::llm::ChatStreamKind::Content,
                    text: ipc_text(&data, "delta").to_string(),
                }),
            )?,
            "context.compact_end" => handle_agent_event(&mut renderer, AgentEvent::CompactEnd)?,
            "context.pop_start" => handle_agent_event(&mut renderer, AgentEvent::PopStart)?,
            "context.pop_end" => handle_agent_event(&mut renderer, AgentEvent::PopEnd)?,
            "context.notice" => handle_agent_event(
                &mut renderer,
                AgentEvent::Notice {
                    text: ipc_text(&data, "text").to_string(),
                },
            )?,
            "run.completed" => break data,
            "run.failed" => {
                renderer.finish()?;
                if let Some(live) = live.as_deref_mut() {
                    live.apply_renderer_frame(&mut renderer)?;
                }
                bail!("{}", ipc_text(&data, "message"));
            }
            "run.cancelled" => {
                renderer.finish()?;
                if let Some(live) = live.as_deref_mut() {
                    live.apply_renderer_frame(&mut renderer)?;
                }
                return Err(anyhow::Error::new(RemoteTurnCancelled));
            }
            _ => {}
        }
        if let Some(live) = live.as_deref_mut() {
            live.apply_renderer_frame(&mut renderer)?;
        }
    };
    renderer.finish()?;
    let focused = live.as_deref().map(|live| live.editor.focused);
    if let Some(live) = live {
        live.apply_renderer_frame(&mut renderer)?;
        if let Some(raw) = raw.as_mut() {
            raw.handoff();
            live.raw_mode_handoff = true;
        }
    }

    let result = ChatResult {
        content,
        reasoning: (!reasoning.is_empty()).then_some(reasoning),
        usage: completion
            .get("usage")
            .cloned()
            .filter(|value| !value.is_null())
            .map(serde_json::from_value::<Usage>)
            .transpose()?,
        usage_estimated: completion
            .get("usage_estimated")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false),
        tool_calls: Vec::new(),
        provider_id: completion
            .get("provider_id")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string),
        model: completion
            .get("model")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string),
        finish_reason: None,
        thinking_signature: None,
        last_request_usage: None,
        responses_continuation: None,
    };
    if config.notifications.on_turn_complete {
        notify_if_unfocused(
            &config,
            focused,
            t("Miyu finished replying", "Miyu 回复完成"),
            &result.content,
        );
    }
    print_mixed_model_endpoint(show_mixed_model_endpoint(&config, false), &result, None);
    let context_tokens = completion
        .get("context_tokens")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or_default();
    let context_window = completion
        .get("context_window")
        .and_then(serde_json::Value::as_u64)
        .and_then(|value| usize::try_from(value).ok());
    let completion_u64 = |key: &str| {
        completion
            .get(key)
            .and_then(serde_json::Value::as_u64)
            .unwrap_or_default()
    };
    let cumulative_tokens = TurnTokens {
        total: completion_u64("cumulative_tokens"),
        prompt: completion_u64("cumulative_prompt_tokens"),
        cache_read: completion_u64("cumulative_cache_read_tokens"),
    };
    print_chat_token_usage(
        &result,
        config.display.show_token_usage && !plain,
        context_tokens,
        context_window,
        TurnTokens::from_usage(result.usage.as_ref()),
    )?;
    Ok(Some(RemoteTurnSummary {
        result,
        context_tokens,
        context_window,
        cumulative_tokens,
    }))
}

async fn send_ipc_command(paths: &MiyuPaths, command: IpcCommand) -> Result<()> {
    let mut stream = ipc::connect(&paths.ipc_socket()).await?;
    ipc::send(&mut stream, &IpcRequest::new(command)).await?;
    validate_ipc_command_response(ipc::receive::<IpcFrame>(&mut stream).await?)
}

fn validate_ipc_command_response(frame: Option<IpcFrame>) -> Result<()> {
    match frame {
        Some(IpcFrame::Ack) | Some(IpcFrame::Ready { .. }) | Some(IpcFrame::AdminResult { .. }) => {
            Ok(())
        }
        Some(IpcFrame::Error { message, .. }) => bail!("{message}"),
        Some(other) => bail!("Miyu core returned an unexpected response: {other:?}"),
        None => bail!("Miyu core closed the connection without a response"),
    }
}

/// Refreshes REPL-local state after the daemon switched to another session:
/// input history, queue tray, and the footer's token accounting.
/// Writes one line of REPL feedback through the live tail so the output
/// cursor stays in sync; never use bare `println!` inside the remote REPL.
fn repl_note(live: &mut LiveReplTail, text: &str) -> Result<()> {
    live.apply_output_frame(format!("{text}\n").as_bytes())
}

/// Remote-REPL text equivalent of `print_pop_outcome` (which stays
/// println-based for the direct REPL and one-shot `miyu pop`).
fn repl_pop_outcome_text(outcome: PopOutcome) -> String {
    let message = if is_zh() {
        if outcome.archived {
            format!("已弹出 {} 轮 · 已归档", outcome.turns)
        } else {
            format!(
                "已弹出 {} 轮 · 未归档（弹出上下文归档已关闭）",
                outcome.turns
            )
        }
    } else {
        let turns = if outcome.turns == 1 { "turn" } else { "turns" };
        if outcome.archived {
            format!("popped {} {turns} · archived", outcome.turns)
        } else {
            format!(
                "popped {} {turns} · not archived (evicted-context archiving is disabled)",
                outcome.turns
            )
        }
    };
    format!("\x1b[2m{message}\x1b[0m\n")
}

/// Remote-REPL text equivalent of `print_nothing_to_pop`.
fn repl_nothing_to_pop_text() -> String {
    format!(
        "\x1b[2m{}\x1b[0m\n",
        t(
            "no conversation turns are available to pop",
            "没有可弹出的上下文轮次"
        )
    )
}

/// Client-side display fallback for sessions the server has not named yet.
fn display_session_name(name: &str) -> &str {
    if name.trim().is_empty() {
        t("New session", "新会话")
    } else {
        name
    }
}

async fn apply_repl_session_switch(
    paths: &MiyuPaths,
    config: &AppConfig,
    state: &ipc::SessionState,
    active_session_id: &mut String,
    history: &mut Vec<String>,
    live_repl: &mut LiveReplTail,
    footer: &mut ReplFooterStatus,
    cumulative_tokens: &mut TurnTokens,
) -> Result<()> {
    if state.session_id.is_empty() {
        bail!("{}", t("session state has no id", "会话状态缺少 ID"));
    }
    let store = StateStore::new(paths)?.pinned(&state.session_id);
    active_session_id.clone_from(&state.session_id);
    *history = load_repl_input_history(&store, paths)?;
    live_repl.editor.history = history.clone();
    live_repl.editor.history_index = live_repl.editor.history.len();
    live_repl.editor.history_clean_index = None;
    live_repl.editor.input.clear();
    live_repl.editor.cursor = 0;
    repl_note(
        live_repl,
        &format!(
            "\x1b[2m{}: {}\x1b[0m\n",
            t("switched to session", "已切换到会话"),
            display_session_name(&state.session_name)
        ),
    )?;
    synchronized_terminal_update(CursorAfterUpdate::Shown, || live_repl.reload_queue(&store))?;
    // Rebuild rather than reset: the target session may pin its own model
    // pool, so provider/model/thinking have to be re-derived alongside the
    // token numbers. `refresh_footer` repaints straight away — merely storing
    // the footer left the previous session's numbers on screen until the next
    // turn finished.
    *cumulative_tokens = state_cumulative(&state);
    let session_config = footer_config_for_session(paths, config, &state.session_id);
    *footer =
        ReplFooterStatus::from_config(&session_config, state.context_tokens, *cumulative_tokens);
    let client = OpenAiCompatibleClient::from_config(&session_config, paths)?;
    footer.update_thinking_variant(client.thinking_variant_summary().as_deref());
    footer.update_context_window(state.context_window);
    live_repl.refresh_footer(footer.clone())?;
    // Every REPL session change funnels through here, so this is the one place
    // the REPL lane needs to be remembered. Best effort: losing the write only
    // means the next REPL starts on the terminal session.
    let _ = send_ipc_admin(
        paths,
        IpcCommand::SetReplSession {
            target: crate::ipc::SessionRef::Id {
                id: state.session_id.clone(),
            },
        },
    )
    .await;
    Ok(())
}

/// One row of the daemon's session list, parsed from `ListSessions` JSON.
struct SessionListEntry {
    id: String,
    name: String,
    is_current: bool,
    turns: u64,
    snippet: String,
    workspace: Option<String>,
}

fn session_list_entries(data: &serde_json::Value) -> Vec<SessionListEntry> {
    data.get("sessions")
        .and_then(serde_json::Value::as_array)
        .map(|sessions| sessions.iter().map(session_list_entry).collect())
        .unwrap_or_default()
}

fn session_list_entry(session: &serde_json::Value) -> SessionListEntry {
    let text = |key: &str| {
        session
            .get(key)
            .and_then(serde_json::Value::as_str)
            .map(str::to_string)
    };
    SessionListEntry {
        id: text("session_id").unwrap_or_default(),
        name: text("name").unwrap_or_default(),
        is_current: session
            .get("is_current")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false),
        turns: session
            .get("turn_count")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0),
        snippet: text("last_user_content")
            .map(|content| {
                let cleaned = content.trim().replace(['\n', '\r'], " ");
                let truncated: String = cleaned.chars().take(24).collect();
                if cleaned.chars().count() > 24 {
                    format!("{truncated}…")
                } else {
                    truncated
                }
            })
            .unwrap_or_default(),
        workspace: text("workspace"),
    }
}

/// Maps a user-facing 1-based session number to a session id ref.
fn session_ref_from_index(
    entries: &[SessionListEntry],
    index: usize,
) -> Option<crate::ipc::SessionRef> {
    index
        .checked_sub(1)
        .and_then(|index| entries.get(index))
        .map(|entry| crate::ipc::SessionRef::Id {
            id: entry.id.clone(),
        })
}

fn session_entry_is_active(entry: &SessionListEntry, active_session_id: Option<&str>) -> bool {
    active_session_id.map_or(entry.is_current, |session_id| entry.id == session_id)
}

fn session_select_line(entry: &SessionListEntry, active_session_id: Option<&str>) -> String {
    let marker = if session_entry_is_active(entry, active_session_id) {
        "* "
    } else {
        "  "
    };
    let mut line = format!(
        "{marker}{}  {} {}",
        display_session_name(&entry.name),
        entry.turns,
        t("turns", "轮")
    );
    if !entry.snippet.is_empty() {
        line.push_str("  ");
        line.push_str(&entry.snippet);
    }
    if let Some(workspace) = &entry.workspace {
        line.push_str(&format!("  [{workspace}]"));
    }
    line
}

fn session_select_search(entry: &SessionListEntry) -> String {
    format!(
        "{} {} {}",
        display_session_name(&entry.name),
        entry.snippet,
        entry.workspace.as_deref().unwrap_or_default()
    )
}

fn session_initial_selection(
    entries: &[SessionListEntry],
    active_session_id: Option<&str>,
) -> usize {
    entries
        .iter()
        .position(|entry| session_entry_is_active(entry, active_session_id))
        .unwrap_or(0)
}

/// What the interactive session picker came back with.
enum SessionPick {
    Cancelled,
    Switch(crate::ipc::SessionRef),
    /// Deletion confirmed inside the picker. `index` is where the cursor sat,
    /// so the caller can reopen the refreshed list at the same spot.
    Delete {
        session_id: String,
        index: usize,
    },
}

fn select_session_target(
    entries: &[SessionListEntry],
    active_session_id: Option<&str>,
    cursor: Option<usize>,
) -> Result<SessionPick> {
    let lines = entries
        .iter()
        .map(|entry| session_select_line(entry, active_session_id))
        .collect::<Vec<_>>();
    let search = entries
        .iter()
        .map(session_select_search)
        .collect::<Vec<_>>();
    let labels = entries
        .iter()
        .map(|entry| display_session_name(&entry.name).to_string())
        .collect::<Vec<_>>();
    let initial = cursor
        .map(|index| index.min(entries.len().saturating_sub(1)))
        .unwrap_or_else(|| session_initial_selection(entries, active_session_id));
    Ok(
        match inline_single_select_deletable(
            t("Select session", "选择会话"),
            &lines,
            &search,
            initial,
            Some(&labels),
        )? {
            InlineSelectOutcome::Cancelled => SessionPick::Cancelled,
            InlineSelectOutcome::Chosen(index) => SessionPick::Switch(crate::ipc::SessionRef::Id {
                id: entries[index].id.clone(),
            }),
            InlineSelectOutcome::Deleted(index) => SessionPick::Delete {
                session_id: entries[index].id.clone(),
                index,
            },
        },
    )
}

/// Resolves a user-typed `/session` / `/delete` argument into a session ref:
/// a number picks from the visible session list, anything else is a name.
async fn resolve_repl_session_target(
    paths: &MiyuPaths,
    live: &mut LiveReplTail,
    arg: &str,
) -> Result<Option<crate::ipc::SessionRef>> {
    if let Ok(index) = arg.parse::<usize>() {
        let Some((_, data)) = repl_ipc_admin(
            paths,
            live,
            IpcCommand::ListSessions {
                include_archived: false,
            },
        )
        .await?
        else {
            return Ok(None);
        };
        let entries = session_list_entries(&data);
        let Some(target) = session_ref_from_index(&entries, index) else {
            repl_note(
                live,
                &format!(
                    "\x1b[2m{}: {index}\x1b[0m\n",
                    t("no session with this number", "没有这个编号的会话")
                ),
            )?;
            return Ok(None);
        };
        Ok(Some(target))
    } else {
        Ok(Some(crate::ipc::SessionRef::Name {
            name: arg.to_string(),
        }))
    }
}

fn print_session_list(entries: &[SessionListEntry]) {
    let ansi = io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none();
    if entries.is_empty() {
        if ansi {
            println!("\x1b[2m{}\x1b[0m\n", t("no sessions", "没有会话"));
        } else {
            println!("{}\n", t("no sessions", "没有会话"));
        }
        return;
    }
    println!("{}", t("sessions:", "会话列表:"));
    for (index, entry) in entries.iter().enumerate() {
        println!("{}", session_list_line(index + 1, entry, ansi));
    }
    println!();
}

fn session_list_line(index: usize, entry: &SessionListEntry, ansi: bool) -> String {
    let name = display_session_name(&entry.name);
    let workspace = entry
        .workspace
        .as_deref()
        .map(|workspace| format!("  [{workspace}]"))
        .unwrap_or_default();
    let marker = if entry.is_current { "*" } else { " " };
    let turns_label = t("turns", "轮");
    let details = format!(
        "{} {turns_label}  {}{workspace}",
        entry.turns, entry.snippet
    );
    if ansi {
        format!("{marker}{index:>3}. {name}  \x1b[2m{details}\x1b[0m")
    } else {
        format!("{marker}{index:>3}. {name}  {details}")
    }
}

/// Repaints the queued-prompt strip from the store. A reset deletes those rows
/// underneath the REPL, so without this the strip keeps showing prompts that no
/// longer exist.
fn reload_repl_queue(live: &mut LiveReplTail, paths: &MiyuPaths, session_id: &str) -> Result<()> {
    let store = StateStore::new(paths)?.pinned(session_id);
    synchronized_terminal_update(CursorAfterUpdate::Shown, || live.reload_queue(&store))
}

fn confirm_inline(live: &mut LiveReplTail, prompt: &str) -> Result<bool> {
    live.apply_output_frame(format!("{prompt} [y/N] ").as_bytes())?;
    let mut answer = String::new();
    io::stdin().read_line(&mut answer)?;
    Ok(matches!(answer.trim(), "y" | "Y" | "yes" | "YES"))
}

fn confirm_stdin(prompt: &str) -> Result<bool> {
    print!("{prompt} [y/N] ");
    io::stdout().flush()?;
    let mut answer = String::new();
    io::stdin().read_line(&mut answer)?;
    Ok(matches!(answer.trim(), "y" | "Y" | "yes" | "YES"))
}

/// Sends an admin command from inside the REPL loop, printing failures (core
/// busy, core restarting, …) through the live tail instead of propagating
/// them so the REPL survives.
async fn repl_ipc_admin(
    paths: &MiyuPaths,
    live: &mut LiveReplTail,
    command: IpcCommand,
) -> Result<Option<(ipc::SessionState, serde_json::Value)>> {
    match send_ipc_admin(paths, command).await {
        Ok(result) => Ok(Some(result)),
        Err(err) => {
            repl_note(
                live,
                &format!("\x1b[31m{}: {err}\x1b[0m\n", t("error", "错误")),
            )?;
            Ok(None)
        }
    }
}

async fn repl_get_session_state(
    paths: &MiyuPaths,
    live: &mut LiveReplTail,
    target: crate::ipc::SessionRef,
) -> Result<Option<ipc::SessionState>> {
    Ok(
        repl_ipc_admin(paths, live, IpcCommand::GetSessionState { target })
            .await?
            .map(|(state, _)| state),
    )
}

async fn repl_fallback_session_state(
    paths: &MiyuPaths,
    live: &mut LiveReplTail,
) -> Result<Option<ipc::SessionState>> {
    let Some((_, data)) = repl_ipc_admin(
        paths,
        live,
        IpcCommand::ListSessions {
            include_archived: false,
        },
    )
    .await?
    else {
        return Ok(None);
    };
    let entries = session_list_entries(&data);
    let Some(entry) = entries
        .iter()
        .find(|entry| entry.is_current)
        .or_else(|| entries.first())
    else {
        return Ok(None);
    };
    repl_get_session_state(
        paths,
        live,
        crate::ipc::SessionRef::Id {
            id: entry.id.clone(),
        },
    )
    .await
}

/// Runs the interactive session picker inside the REPL, servicing Ctrl+D
/// deletions in place. Returns the session state to switch to — a fallback
/// session when the REPL's own session was one of the ones deleted, so backing
/// out never strands the REPL on a session that no longer exists.
async fn repl_pick_session(
    paths: &MiyuPaths,
    live: &mut LiveReplTail,
    active_session_id: &str,
) -> Result<Option<ipc::SessionState>> {
    let mut cursor = None;
    let mut lost_active = false;
    loop {
        let Some((_, data)) = repl_ipc_admin(
            paths,
            live,
            IpcCommand::ListSessions {
                include_archived: false,
            },
        )
        .await?
        else {
            return Ok(None);
        };
        let entries = session_list_entries(&data);
        if entries.is_empty() {
            repl_note(
                live,
                &format!("\x1b[2m{}\x1b[0m\n", t("no sessions", "没有会话")),
            )?;
            // Deleting the last session leaves the daemon to mint a fresh one.
            return if lost_active {
                repl_fallback_session_state(paths, live).await
            } else {
                Ok(None)
            };
        }
        match select_session_target(&entries, Some(active_session_id), cursor)? {
            SessionPick::Cancelled => {
                return if lost_active {
                    repl_fallback_session_state(paths, live).await
                } else {
                    Ok(None)
                };
            }
            SessionPick::Switch(target) => {
                return repl_get_session_state(paths, live, target).await;
            }
            SessionPick::Delete { session_id, index } => {
                let was_active = session_id == active_session_id;
                let deleted = repl_ipc_admin(
                    paths,
                    live,
                    IpcCommand::DeleteSession {
                        target: crate::ipc::SessionRef::Id { id: session_id },
                    },
                )
                .await?;
                if deleted.is_none() {
                    return if lost_active {
                        repl_fallback_session_state(paths, live).await
                    } else {
                        Ok(None)
                    };
                }
                lost_active |= was_active;
                // The rows below shift up, so holding the index parks the
                // cursor on the next session instead of jumping to the top.
                cursor = Some(index);
            }
        }
    }
}

async fn repl_active_or_default_state(
    paths: &MiyuPaths,
    active_session_id: &str,
) -> Result<(ipc::SessionState, bool)> {
    match send_ipc_admin(
        paths,
        IpcCommand::GetSessionState {
            target: crate::ipc::SessionRef::Id {
                id: active_session_id.to_string(),
            },
        },
    )
    .await
    {
        Ok((state, _)) => Ok((state, false)),
        Err(_) => {
            let (state, _) = send_ipc_admin(paths, IpcCommand::GetStatus).await?;
            let changed = state.session_id != active_session_id;
            Ok((state, changed))
        }
    }
}

/// Ensures the daemon is running, then sends one admin command; used by the
/// one-shot session subcommands (`miyu new/session/rename/...`).
async fn session_admin(
    paths: &MiyuPaths,
    command: IpcCommand,
) -> Result<(ipc::SessionState, serde_json::Value)> {
    ipc::ensure_daemon(paths, None).await?;
    let refreshed = MiyuPaths::new()?;
    send_ipc_admin(&refreshed, command).await
}

/// Resolves a `miyu session/delete` target argument outside the REPL:
/// numbers index into the visible session list, anything else is a name.
/// Resolves a `--session` argument (name or list index) to a concrete
/// session id, without moving the global current pointer.
async fn resolve_session_id_for_turn(paths: &MiyuPaths, arg: &str) -> Result<String> {
    let (_, data) = session_admin(
        paths,
        IpcCommand::ListSessions {
            include_archived: true,
        },
    )
    .await?;
    let entries = session_list_entries(&data);
    if let Ok(index) = arg.parse::<usize>() {
        if let Some(entry) = index.checked_sub(1).and_then(|index| entries.get(index)) {
            return Ok(entry.id.clone());
        }
        bail!(
            "{}: {index}",
            t("no session with this number", "没有这个编号的会话")
        );
    }
    entries
        .into_iter()
        .find(|entry| entry.name.eq_ignore_ascii_case(arg) || entry.id == arg)
        .map(|entry| entry.id)
        .ok_or_else(|| anyhow::anyhow!("{}: {arg}", t("session not found", "找不到该会话")))
}

async fn resolve_cli_session_target(
    paths: &MiyuPaths,
    arg: &str,
) -> Result<crate::ipc::SessionRef> {
    if let Ok(index) = arg.parse::<usize>() {
        let (_, data) = session_admin(
            paths,
            IpcCommand::ListSessions {
                include_archived: false,
            },
        )
        .await?;
        let entries = session_list_entries(&data);
        return session_ref_from_index(&entries, index).ok_or_else(|| {
            anyhow::anyhow!(
                "{}: {index}",
                t("no session with this number", "没有这个编号的会话")
            )
        });
    }
    Ok(crate::ipc::SessionRef::Name {
        name: arg.to_string(),
    })
}

async fn run_session_new(paths: &MiyuPaths, args: SessionNameArgs) -> Result<()> {
    let name = args
        .name
        .as_deref()
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(str::to_string);
    let (state, _) = session_admin(
        paths,
        IpcCommand::CreateSession {
            name,
            switch: true,
            kind: None,
        },
    )
    .await?;
    println!(
        "{}: {}",
        t("created and switched to session", "已创建并切换到会话"),
        display_session_name(&state.session_name)
    );
    Ok(())
}

/// Runs the interactive session picker, servicing Ctrl+D deletions in place
/// and reopening on the refreshed list until the user picks one or backs out.
async fn cli_pick_session(
    paths: &MiyuPaths,
    mut entries: Vec<SessionListEntry>,
) -> Result<Option<crate::ipc::SessionRef>> {
    let mut cursor = None;
    loop {
        match select_session_target(&entries, None, cursor)? {
            SessionPick::Cancelled => return Ok(None),
            SessionPick::Switch(target) => return Ok(Some(target)),
            SessionPick::Delete { session_id, index } => {
                session_admin(
                    paths,
                    IpcCommand::DeleteSession {
                        target: crate::ipc::SessionRef::Id { id: session_id },
                    },
                )
                .await?;
                let (_, data) = session_admin(
                    paths,
                    IpcCommand::ListSessions {
                        include_archived: false,
                    },
                )
                .await?;
                entries = session_list_entries(&data);
                if entries.is_empty() {
                    print_session_list(&entries);
                    return Ok(None);
                }
                // The rows below shift up, so holding the index parks the
                // cursor on the next session instead of jumping to the top.
                cursor = Some(index);
            }
        }
    }
}

async fn run_session_command(paths: &MiyuPaths, args: SessionTargetArgs) -> Result<()> {
    let arg = args
        .target
        .as_deref()
        .map(str::trim)
        .filter(|arg| !arg.is_empty());
    let target = if let Some(arg) = arg {
        resolve_cli_session_target(paths, arg).await?
    } else {
        let (_, data) = session_admin(
            paths,
            IpcCommand::ListSessions {
                include_archived: false,
            },
        )
        .await?;
        let entries = session_list_entries(&data);
        if entries.is_empty() {
            print_session_list(&entries);
            return Ok(());
        }
        if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
            print_session_list(&entries);
            return Ok(());
        }
        match cli_pick_session(paths, entries).await? {
            Some(target) => target,
            None => return Ok(()),
        }
    };
    let (state, _) = session_admin(paths, IpcCommand::SwitchSession { target }).await?;
    println!(
        "{}: {}",
        t("switched to session", "已切换到会话"),
        display_session_name(&state.session_name)
    );
    Ok(())
}

async fn run_session_rename(paths: &MiyuPaths, args: SessionRenameArgs) -> Result<()> {
    let name = args.name.trim().to_string();
    if name.is_empty() {
        bail!(
            "{}",
            t("usage: miyu rename <name>", "用法：miyu rename <新名称>")
        );
    }
    session_admin(
        paths,
        IpcCommand::RenameSession {
            target: crate::ipc::SessionRef::Current,
            name: name.clone(),
        },
    )
    .await?;
    println!("{}: {name}", t("session renamed", "会话已重命名"));
    Ok(())
}

async fn run_session_archive(paths: &MiyuPaths) -> Result<()> {
    let (state, _) = session_admin(
        paths,
        IpcCommand::ArchiveSession {
            target: crate::ipc::SessionRef::Current,
            archived: true,
        },
    )
    .await?;
    println!("{}", t("session archived", "当前会话已归档"));
    println!(
        "{}: {}",
        t("switched to session", "已切换到会话"),
        display_session_name(&state.session_name)
    );
    Ok(())
}

async fn run_session_delete(paths: &MiyuPaths, args: SessionTargetArgs) -> Result<()> {
    let target = match args
        .target
        .as_deref()
        .map(str::trim)
        .filter(|arg| !arg.is_empty())
    {
        Some(arg) => resolve_cli_session_target(paths, arg).await?,
        None => crate::ipc::SessionRef::Current,
    };
    if !confirm_stdin(t(
        "delete this session and all of its history?",
        "确认删除该会话及其全部历史？",
    ))? {
        println!("{}", t("cancelled", "已取消"));
        return Ok(());
    }
    let (state, _) = session_admin(paths, IpcCommand::DeleteSession { target }).await?;
    println!("{}", t("session deleted", "会话已删除"));
    println!(
        "{}: {}",
        t("switched to session", "已切换到会话"),
        display_session_name(&state.session_name)
    );
    Ok(())
}

async fn run_workspace_command(paths: &MiyuPaths, args: WorkspaceArgs) -> Result<()> {
    let Some(arg) = args
        .path
        .as_deref()
        .map(str::trim)
        .filter(|arg| !arg.is_empty())
    else {
        let (state, _) = session_admin(paths, IpcCommand::GetStatus).await?;
        match state.workspace {
            Some(workspace) => {
                println!("{}: {workspace}", t("session workspace", "会话工作目录"));
            }
            None => println!(
                "{}",
                t(
                    "no workspace bound; using the client working directory",
                    "未绑定工作目录；使用客户端当前目录"
                )
            ),
        }
        return Ok(());
    };
    if arg.eq_ignore_ascii_case("clear") {
        session_admin(
            paths,
            IpcCommand::SetWorkspace {
                target: crate::ipc::SessionRef::Current,
                path: None,
            },
        )
        .await?;
        println!("{}", t("workspace unbound", "已解绑工作目录"));
        return Ok(());
    }
    let path = std::fs::canonicalize(expand_tilde(arg)).map_err(|error| {
        anyhow::anyhow!(
            "{}: {arg} ({error})",
            t("invalid workspace path", "无效的工作目录路径")
        )
    })?;
    session_admin(
        paths,
        IpcCommand::SetWorkspace {
            target: crate::ipc::SessionRef::Current,
            path: Some(path.clone()),
        },
    )
    .await?;
    println!(
        "{}: {}",
        t("workspace bound", "已绑定工作目录"),
        path.display()
    );
    Ok(())
}

/// Expands a leading `~` or `~/…` to the user's home directory.
fn expand_tilde(path: &str) -> PathBuf {
    if let Some(home) = std::env::var_os("HOME") {
        if path == "~" {
            return PathBuf::from(home);
        }
        if let Some(rest) = path.strip_prefix("~/") {
            return PathBuf::from(home).join(rest);
        }
    }
    PathBuf::from(path)
}

async fn send_ipc_admin(
    paths: &MiyuPaths,
    command: IpcCommand,
) -> Result<(ipc::SessionState, serde_json::Value)> {
    let mut stream = ipc::connect(&paths.ipc_socket()).await?;
    ipc::send(&mut stream, &IpcRequest::new(command)).await?;
    match ipc::receive::<IpcFrame>(&mut stream).await? {
        Some(IpcFrame::AdminResult { state, data }) => Ok((state, data)),
        Some(IpcFrame::Error { message, .. }) => bail!("{message}"),
        _ => bail!("Miyu core returned an invalid admin response"),
    }
}

fn ipc_text<'a>(data: &'a serde_json::Value, key: &str) -> &'a str {
    data.get(key)
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
}

async fn render_remote_tool_image(
    state: &StateStore,
    event: &serde_json::Value,
    size: Option<String>,
) -> Result<()> {
    let asset_id = remote_tool_image_asset_id(event)
        .ok_or_else(|| anyhow::anyhow!("tool image event did not contain an asset id"))?;
    let asset = state
        .load_image_asset(asset_id)?
        .ok_or_else(|| anyhow::anyhow!("tool image asset is no longer available"))?;
    let preview = remote_image_preview(&asset)?;
    tools::vision::print_image_file(preview.path(), size).await
}

fn remote_tool_image_asset_id(event: &serde_json::Value) -> Option<&str> {
    event
        .get("asset")
        .and_then(|asset| asset.get("id"))
        .and_then(serde_json::Value::as_str)
        .filter(|id| !id.trim().is_empty())
}

fn remote_image_preview(asset: &crate::state::ImageAssetData) -> Result<tempfile::NamedTempFile> {
    let suffix = if asset.asset.mime == "image/gif" {
        ".png"
    } else {
        match asset.asset.mime.as_str() {
            "image/jpeg" => ".jpg",
            "image/png" => ".png",
            "image/webp" => ".webp",
            "image/bmp" => ".bmp",
            _ => ".img",
        }
    };
    let mut preview = tempfile::Builder::new().suffix(suffix).tempfile()?;
    if asset.asset.mime == "image/gif" {
        image::load_from_memory(&asset.bytes)?
            .write_to(preview.as_file_mut(), image::ImageFormat::Png)?;
    } else {
        preview.write_all(&asset.bytes)?;
        preview.flush()?;
    }
    Ok(preview)
}

fn ipc_mode_name(mode: AgentMode) -> &'static str {
    match mode {
        AgentMode::Normal => "normal",
        AgentMode::Chat => "chat",
    }
}

fn ipc_images(
    images: &[Option<crate::clipboard::PastedImage>],
) -> Vec<Option<crate::ipc::ImageAttachment>> {
    images
        .iter()
        .map(|image| {
            image.as_ref().map(|image| match image {
                crate::clipboard::PastedImage::Binary(image) => {
                    crate::ipc::ImageAttachment::Binary {
                        mime: image.mime.clone(),
                        data: image.data.clone(),
                    }
                }
                crate::clipboard::PastedImage::Path(path) => {
                    crate::ipc::ImageAttachment::Path { path: path.clone() }
                }
            })
        })
        .collect()
}

fn print_chat_token_usage(
    result: &crate::llm::ChatResult,
    enabled: bool,
    session_token_total: u64,
    context_window: Option<usize>,
    cumulative: TurnTokens,
) -> Result<()> {
    if enabled && result.usage.is_some() {
        let meter = turn_meter(
            TurnTokens::from_usage(result.usage.as_ref()),
            session_token_total,
            context_window,
            cumulative,
        );
        render::print_token_usage(&meter, result.usage_estimated)?;
    }
    Ok(())
}

fn turn_meter(
    turn: TurnTokens,
    session_tokens: u64,
    context_window: Option<usize>,
    cumulative: TurnTokens,
) -> render::TokenMeter {
    render::TokenMeter {
        turn_tokens: turn.total,
        turn_prompt_tokens: turn.prompt,
        turn_cached_tokens: turn.cache_read,
        session_tokens,
        context_window,
        ..meter_cumulative(cumulative)
    }
}

fn result_context_window(config: &AppConfig, result: &crate::llm::ChatResult) -> Option<usize> {
    if config.active_provider_model_choices().len() > 1 {
        return None;
    }
    let provider = result.provider_id.as_deref()?;
    let model = result.model.as_deref()?;
    config
        .context_window_for_provider_model(provider, model)
        .ok()
        .flatten()
}

async fn handle_post_turn_overflow(
    agent: &Agent,
    renderer: &mut render::StreamRenderer,
    context_tokens: u64,
    show_token_usage: bool,
    cumulative_tokens: Option<&mut TurnTokens>,
) -> Result<Option<crate::llm::ChatResult>> {
    let compact_result = agent
        .handle_overflow_after_turn(context_tokens, |event| handle_agent_event(renderer, event))
        .await?;
    renderer.finish()?;
    if let Some(compact_result) = compact_result {
        let mut cumulative_display = TurnTokens::default();
        if let Some(total) = cumulative_tokens {
            if let Some(usage) = compact_result.usage.as_ref() {
                total.add(TurnTokens::from_usage(Some(usage)));
                cumulative_display = *total;
            }
        }
        print_chat_token_usage(
            &compact_result,
            show_token_usage,
            agent.effective_context_tokens()?,
            agent.context_window(),
            cumulative_display,
        )?;
        return Ok(Some(compact_result));
    }
    Ok(None)
}

async fn handle_live_post_turn_overflow(
    live: &mut LiveReplTail,
    agent: &Agent,
    renderer: &mut render::StreamRenderer,
    context_tokens: u64,
    show_token_usage: bool,
    cumulative_tokens: Option<&mut TurnTokens>,
) -> Result<Option<crate::llm::ChatResult>> {
    let compact_result = agent
        .handle_overflow_after_turn(context_tokens, |event| {
            handle_live_agent_event(live, renderer, event)
        })
        .await?;
    renderer.finish()?;
    live.apply_renderer_frame(renderer)?;
    if let Some(compact_result) = compact_result {
        let mut cumulative_display = TurnTokens::default();
        if let Some(total) = cumulative_tokens {
            if let Some(usage) = compact_result.usage.as_ref() {
                total.add(TurnTokens::from_usage(Some(usage)));
                cumulative_display = *total;
            }
        }
        if show_token_usage {
            if let Some(usage) = compact_result.usage.as_ref() {
                let frame = render::token_usage_output(
                    &turn_meter(
                        TurnTokens::from_usage(Some(usage)),
                        agent.effective_context_tokens()?,
                        agent.context_window(),
                        cumulative_display,
                    ),
                    compact_result.usage_estimated,
                );
                live.apply_output_frame(frame.strip_suffix('\n').unwrap_or(&frame).as_bytes())?;
            }
        }
        return Ok(Some(compact_result));
    }
    Ok(None)
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum VariantOutcome {
    Updated,
    Cancelled,
    Rejected(String),
}

fn run_variant(paths: &MiyuPaths, args: VariantArgs) -> Result<()> {
    let selected = args
        .name
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty());
    if selected.is_none() && (!io::stdin().is_terminal() || !io::stdout().is_terminal()) {
        bail!(
            "{}",
            t(
                "interactive variant selection requires a terminal; use `miyu variant <name>`",
                "交互 variant 选择需要终端；请使用 `miyu variant <名称>`",
            )
        );
    }
    if !crate::models_cache::is_loaded() {
        crate::models_cache::refresh_blocking(paths).map_err(|error| {
            anyhow::anyhow!(
                "{}: {error:#}",
                t("failed to load model metadata", "无法加载模型元数据")
            )
        })?;
    }

    let config = AppConfig::load_or_default(paths)?;
    let mut client = OpenAiCompatibleClient::from_config(&config, paths)?;
    match execute_variant(paths, &mut client, selected, "miyu variant")? {
        VariantOutcome::Updated => print_variant_updated(),
        VariantOutcome::Cancelled => {}
        VariantOutcome::Rejected(message) => bail!("{message}"),
    }
    Ok(())
}

fn execute_variant(
    paths: &MiyuPaths,
    client: &mut OpenAiCompatibleClient,
    selected: Option<&str>,
    selector_command: &str,
) -> Result<VariantOutcome> {
    if let Some(selected) = selected {
        if client.thinking_variant_options().len() != 1 {
            let message = if is_zh() {
                format!("当前激活了多个模型；请使用 {selector_command} 在 TUI 中分别设置")
            } else {
                format!(
                    "multiple models are active; use {selector_command} and configure them in the TUI"
                )
            };
            return Ok(VariantOutcome::Rejected(message));
        }
        let available = client.available_thinking_variants();
        let variant = match resolve_variant_name(selected, &available) {
            Ok(variant) => variant,
            Err(message) => return Ok(VariantOutcome::Rejected(message)),
        };
        client.set_thinking_variant(variant)?;
    } else {
        let options = client.thinking_variant_options();
        let Some(selections) = inline_variant_select(&options)? else {
            return Ok(VariantOutcome::Cancelled);
        };
        client.set_thinking_variants(&selections)?;
    }

    client.save_thinking_variants(paths)?;
    Ok(VariantOutcome::Updated)
}

fn resolve_variant_name(
    selected: &str,
    available: &[String],
) -> std::result::Result<Option<String>, String> {
    let explicit_variant = selected.strip_prefix("variant:");
    if explicit_variant.is_none() && selected.eq_ignore_ascii_case("default") {
        return Ok(None);
    }
    let selected = explicit_variant.unwrap_or(selected);
    available
        .iter()
        .find(|candidate| candidate.eq_ignore_ascii_case(selected))
        .cloned()
        .map(Some)
        .ok_or_else(|| {
            format!(
                "{}: {selected}",
                t("unknown thinking variant", "未知思考档位")
            )
        })
}

fn print_variant_updated() {
    println!("{}\n", t("thinking variants updated", "已更新思考档位"));
}

async fn run_repl(paths: &MiyuPaths, initial_mode: AgentMode) -> Result<()> {
    if direct_mode_requested() {
        run_direct_repl(paths, initial_mode).await
    } else {
        run_remote_repl(paths, initial_mode).await
    }
}

async fn run_remote_repl(paths: &MiyuPaths, mut mode: AgentMode) -> Result<()> {
    let _cursor_restore = ReplCursorRestore;
    ipc::ensure_daemon(paths, None).await?;
    let refreshed = MiyuPaths::new()?;
    let paths = &refreshed;
    initialize_models_cache(paths);
    let mut config = AppConfig::load_or_default(paths)?;
    // The REPL resumes its own lane rather than the terminal session, so
    // reopening a REPL lands back where the last one left off while shell-hook
    // keeps talking to whatever session it was on.
    let (daemon_state, _) = send_ipc_admin(paths, IpcCommand::GetReplSession).await?;
    let mut active_session_id = daemon_state.session_id.clone();
    let history_state = StateStore::new(paths)?.pinned(&active_session_id);
    let mut history = load_repl_input_history(&history_state, paths)?;
    drop(history_state);
    let mut cumulative_tokens = state_cumulative(&daemon_state);
    let mut footer = ReplFooterStatus::from_config(
        &footer_config_for_session(paths, &config, &active_session_id),
        daemon_state.context_tokens,
        cumulative_tokens,
    );
    let client = OpenAiCompatibleClient::from_config(&config, paths)?;
    let thinking_summary = client.thinking_variant_summary();
    footer.update_thinking_variant(thinking_summary.as_deref());
    footer.update_context_window(daemon_state.context_window);
    let mut live_repl = LiveReplTail::new(mode, history.clone(), Vec::new(), footer.clone())?;
    let jobs_shared = spawn_jobs_poll_thread(paths.clone());
    let jobs_feed = JobsFeed::Shared(jobs_shared.clone());

    // Terminal closed (SIGHUP) or process killed (SIGTERM): the graceful
    // exit path at the bottom never runs, so stop this session's background
    // jobs from a signal task before dying. SIGKILL still leaks them — the
    // daemon keeps those running and their completion wakes queue up.
    #[cfg(unix)]
    {
        let paths = paths.clone();
        let feed = jobs_shared.clone();
        tokio::spawn(async move {
            use tokio::signal::unix::{signal, SignalKind};
            let (Ok(mut hangup), Ok(mut terminate)) = (
                signal(SignalKind::hangup()),
                signal(SignalKind::terminate()),
            ) else {
                return;
            };
            tokio::select! {
                _ = hangup.recv() => {}
                _ = terminate.recv() => {}
            }
            let session = { feed.repl_session.lock().unwrap().clone() };
            if let Some(session_id) = session {
                let _ = send_ipc_admin(&paths, IpcCommand::StopSessionJobs { session_id }).await;
            }
            std::process::exit(0);
        });
    }

    // Redraw the tail of the session we just resumed. The tail is not on
    // screen yet (`rendered == false`), so `apply_output_frame` writes the
    // frame raw and re-reads the cursor — no layout budget applies and the
    // frame can be arbitrarily long.
    if config.display.repl_replay_turns > 0 {
        let replay_store = StateStore::new(paths)?.pinned(&active_session_id);
        match replay_store.session_replay(config.display.repl_replay_turns) {
            Ok(replays) if !replays.is_empty() => {
                let (cols, _) = terminal::size().unwrap_or((80, 24));
                let frame =
                    session_replay_frame(&replays, mode, &config, usize::from(cols.max(1)))?;
                live_repl.apply_output_frame(&frame)?;
            }
            Ok(_) => {}
            Err(error) => tracing::debug!(error = %error, "session replay unavailable"),
        }
    }

    loop {
        // Keep the poll thread's session filter in step with /new & /session.
        *jobs_shared.repl_session.lock().unwrap() = Some(active_session_id.clone());
        live_repl.set_footer(footer.clone());
        let (next_mode, input, images) = match read_live_repl_input(
            &mut live_repl,
            paths,
            &jobs_feed,
            Some(&active_session_id),
        )? {
            LiveReplOutcome::Exit => break,
            LiveReplOutcome::StopJobs => {
                let stopped = match repl_ipc_admin(
                    paths,
                    &mut live_repl,
                    IpcCommand::StopSessionJobs {
                        session_id: active_session_id.clone(),
                    },
                )
                .await?
                {
                    Some((_, data)) => data
                        .get("stopped")
                        .and_then(serde_json::Value::as_u64)
                        .unwrap_or(0),
                    None => 0,
                };
                // Drop the strip now instead of waiting out the ~1s jobs poll:
                // every job of this session was just stopped, so an empty strip
                // is the truth.
                live_repl.set_jobs(Vec::new());
                repl_note(
                    &mut live_repl,
                    &format!(
                        "\x1b[2m{}\x1b[0m\n",
                        if is_zh() {
                            format!("已停止 {stopped} 个后台任务")
                        } else {
                            format!("stopped {stopped} background task(s)")
                        }
                    ),
                )?;
                continue;
            }
            LiveReplOutcome::FollowWake { run_id, label } => {
                if let Err(error) = follow_wake_run(
                    paths,
                    &mut live_repl,
                    &run_id,
                    &label,
                    &jobs_feed,
                    &jobs_shared,
                )
                .await
                {
                    tracing::debug!(error = %error, "wake follow detached with an error");
                }
                continue;
            }
            LiveReplOutcome::Submit(next_mode, input, images) => (next_mode, input, images),
        };
        mode = next_mode;
        let input = input.trim();
        if input.eq_ignore_ascii_case("exit") || input.eq_ignore_ascii_case("quit") {
            break;
        }
        let (slash_command, command_args) = match parse_repl_input(input) {
            ReplInput::Chat => (None, ""),
            ReplInput::Slash(command, args) => (Some(command), args),
            ReplInput::UnknownSlash(name) => {
                repl_note(
                    &mut live_repl,
                    &format!(
                        "\x1b[2m{}: {name}\x1b[0m\n",
                        t("unknown command; try /help", "未知命令，可用 /help 查看")
                    ),
                )?;
                continue;
            }
        };
        if let Some(command) = slash_command {
            let spec = repl_command_spec(command);
            if spec.arg_hint.is_empty() && !command_args.trim().is_empty() {
                repl_note(
                    &mut live_repl,
                    &format!(
                        "\x1b[2m{}: {}\x1b[0m\n",
                        t("this command takes no arguments", "该命令不接受参数"),
                        spec.name
                    ),
                )?;
                continue;
            }
            match command {
                ReplSlashCommand::Exit => break,
                ReplSlashCommand::Help => print_repl_help(),
                ReplSlashCommand::History => {
                    let state = StateStore::new(paths)?.pinned(&active_session_id);
                    run_history_with_state(
                        &state,
                        HistoryArgs {
                            limit: 20,
                            raw: false,
                            no_thinking: false,
                        },
                    )?
                }
                ReplSlashCommand::Clear => {
                    synchronized_terminal_update(CursorAfterUpdate::Preserve, || {
                        live_repl.clear_screen()
                    })?;
                }
                ReplSlashCommand::New => {
                    let name = command_args.trim();
                    let Some((_, data)) = repl_ipc_admin(
                        paths,
                        &mut live_repl,
                        IpcCommand::CreateSession {
                            name: (!name.is_empty()).then(|| name.to_string()),
                            switch: false,
                            kind: None,
                        },
                    )
                    .await?
                    else {
                        continue;
                    };
                    let Some(session_id) = data
                        .get("session")
                        .and_then(|session| session.get("session_id"))
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_string)
                    else {
                        repl_note(
                            &mut live_repl,
                            &format!(
                                "\x1b[31m{}\x1b[0m\n",
                                t("created session has no id", "新会话缺少 ID")
                            ),
                        )?;
                        continue;
                    };
                    let Some((state, _)) = repl_ipc_admin(
                        paths,
                        &mut live_repl,
                        IpcCommand::GetSessionState {
                            target: crate::ipc::SessionRef::Id { id: session_id },
                        },
                    )
                    .await?
                    else {
                        continue;
                    };
                    apply_repl_session_switch(
                        paths,
                        &config,
                        &state,
                        &mut active_session_id,
                        &mut history,
                        &mut live_repl,
                        &mut footer,
                        &mut cumulative_tokens,
                    )
                    .await?;
                }
                ReplSlashCommand::Session => {
                    let arg = command_args.trim();
                    let state = if arg.is_empty() {
                        match repl_pick_session(paths, &mut live_repl, &active_session_id).await? {
                            Some(state) => state,
                            None => continue,
                        }
                    } else {
                        let target =
                            match resolve_repl_session_target(paths, &mut live_repl, arg).await? {
                                Some(target) => target,
                                None => continue,
                            };
                        match repl_get_session_state(paths, &mut live_repl, target).await? {
                            Some(state) => state,
                            None => continue,
                        }
                    };
                    apply_repl_session_switch(
                        paths,
                        &config,
                        &state,
                        &mut active_session_id,
                        &mut history,
                        &mut live_repl,
                        &mut footer,
                        &mut cumulative_tokens,
                    )
                    .await?;
                }
                ReplSlashCommand::Rename => {
                    let name = command_args.trim().to_string();
                    if name.is_empty() {
                        repl_note(
                            &mut live_repl,
                            &format!(
                                "\x1b[2m{}\x1b[0m\n",
                                t("usage: /rename <name>", "用法：/rename <新名称>")
                            ),
                        )?;
                        continue;
                    }
                    if repl_ipc_admin(
                        paths,
                        &mut live_repl,
                        IpcCommand::RenameSession {
                            target: crate::ipc::SessionRef::Id {
                                id: active_session_id.clone(),
                            },
                            name: name.clone(),
                        },
                    )
                    .await?
                    .is_some()
                    {
                        repl_note(
                            &mut live_repl,
                            &format!(
                                "\x1b[2m{}: {name}\x1b[0m\n",
                                t("session renamed", "会话已重命名")
                            ),
                        )?;
                    }
                }
                ReplSlashCommand::Archive => {
                    let Some((_, _)) = repl_ipc_admin(
                        paths,
                        &mut live_repl,
                        IpcCommand::ArchiveSession {
                            target: crate::ipc::SessionRef::Id {
                                id: active_session_id.clone(),
                            },
                            archived: true,
                        },
                    )
                    .await?
                    else {
                        continue;
                    };
                    repl_note(
                        &mut live_repl,
                        &format!("\x1b[2m{}\x1b[0m", t("session archived", "当前会话已归档")),
                    )?;
                    let Some(state) = repl_fallback_session_state(paths, &mut live_repl).await?
                    else {
                        continue;
                    };
                    apply_repl_session_switch(
                        paths,
                        &config,
                        &state,
                        &mut active_session_id,
                        &mut history,
                        &mut live_repl,
                        &mut footer,
                        &mut cumulative_tokens,
                    )
                    .await?;
                }
                ReplSlashCommand::Delete => {
                    let arg = command_args.trim();
                    let target = if arg.is_empty() {
                        crate::ipc::SessionRef::Id {
                            id: active_session_id.clone(),
                        }
                    } else {
                        match resolve_repl_session_target(paths, &mut live_repl, arg).await? {
                            Some(target) => target,
                            None => continue,
                        }
                    };
                    let Some(target_state) =
                        repl_get_session_state(paths, &mut live_repl, target).await?
                    else {
                        continue;
                    };
                    let deleted_active = target_state.session_id == active_session_id;
                    if !confirm_inline(
                        &mut live_repl,
                        t(
                            "delete this session and all of its history?",
                            "确认删除该会话及其全部历史？",
                        ),
                    )? {
                        repl_note(
                            &mut live_repl,
                            &format!("\x1b[2m{}\x1b[0m\n", t("cancelled", "已取消")),
                        )?;
                        continue;
                    }
                    let Some((_, _)) = repl_ipc_admin(
                        paths,
                        &mut live_repl,
                        IpcCommand::DeleteSession {
                            target: crate::ipc::SessionRef::Id {
                                id: target_state.session_id,
                            },
                        },
                    )
                    .await?
                    else {
                        continue;
                    };
                    repl_note(
                        &mut live_repl,
                        &format!("\x1b[2m{}\x1b[0m", t("session deleted", "会话已删除")),
                    )?;
                    if deleted_active {
                        let Some(state) =
                            repl_fallback_session_state(paths, &mut live_repl).await?
                        else {
                            continue;
                        };
                        apply_repl_session_switch(
                            paths,
                            &config,
                            &state,
                            &mut active_session_id,
                            &mut history,
                            &mut live_repl,
                            &mut footer,
                            &mut cumulative_tokens,
                        )
                        .await?;
                    }
                }
                ReplSlashCommand::Workspace => {
                    let arg = command_args.trim();
                    if arg.is_empty() {
                        let Some(state) = repl_get_session_state(
                            paths,
                            &mut live_repl,
                            crate::ipc::SessionRef::Id {
                                id: active_session_id.clone(),
                            },
                        )
                        .await?
                        else {
                            continue;
                        };
                        let note = match state.workspace {
                            Some(workspace) => format!(
                                "\x1b[2m{}: {workspace}\x1b[0m\n",
                                t("session workspace", "会话工作目录")
                            ),
                            None => format!(
                                "\x1b[2m{}\x1b[0m\n",
                                t(
                                    "no workspace bound; using the client working directory",
                                    "未绑定工作目录；使用客户端当前目录"
                                )
                            ),
                        };
                        repl_note(&mut live_repl, &note)?;
                        continue;
                    }
                    if arg.eq_ignore_ascii_case("clear") {
                        if repl_ipc_admin(
                            paths,
                            &mut live_repl,
                            IpcCommand::SetWorkspace {
                                target: crate::ipc::SessionRef::Id {
                                    id: active_session_id.clone(),
                                },
                                path: None,
                            },
                        )
                        .await?
                        .is_some()
                        {
                            repl_note(
                                &mut live_repl,
                                &format!(
                                    "\x1b[2m{}\x1b[0m\n",
                                    t("workspace unbound", "已解绑工作目录")
                                ),
                            )?;
                        }
                        continue;
                    }
                    let path = match std::fs::canonicalize(expand_tilde(arg)) {
                        Ok(path) => path,
                        Err(error) => {
                            repl_note(
                                &mut live_repl,
                                &format!(
                                    "\x1b[31m{}: {arg} ({error})\x1b[0m\n",
                                    t("invalid workspace path", "无效的工作目录路径")
                                ),
                            )?;
                            continue;
                        }
                    };
                    if repl_ipc_admin(
                        paths,
                        &mut live_repl,
                        IpcCommand::SetWorkspace {
                            target: crate::ipc::SessionRef::Id {
                                id: active_session_id.clone(),
                            },
                            path: Some(path.clone()),
                        },
                    )
                    .await?
                    .is_some()
                    {
                        repl_note(
                            &mut live_repl,
                            &format!(
                                "\x1b[2m{}: {}\x1b[0m\n",
                                t("workspace bound", "已绑定工作目录"),
                                path.display()
                            ),
                        )?;
                    }
                }
                ReplSlashCommand::Usage => {
                    let snapshot = StateStore::new(paths)?.usage_snapshot()?;
                    let usage = footer.token_usage;
                    let context = Some((usage.session_tokens, usage.context_window));
                    repl_note(
                        &mut live_repl,
                        &format!("{}\n\n", usage_overview_text(&snapshot, context)),
                    )?;
                }
                ReplSlashCommand::Persona => match run_persona_picker(paths, command_args) {
                    Ok(true) => {
                        let _ =
                            repl_ipc_admin(paths, &mut live_repl, IpcCommand::ReloadConfig).await;
                        config = AppConfig::load(paths)?;
                        let session_config =
                            footer_config_for_session(paths, &config, &active_session_id);
                        let (state, _) =
                            repl_active_or_default_state(paths, &active_session_id).await?;
                        cumulative_tokens = state_cumulative(&state);
                        footer = ReplFooterStatus::from_config(
                            &session_config,
                            state.context_tokens,
                            cumulative_tokens,
                        );
                        let client = OpenAiCompatibleClient::from_config(&session_config, paths)?;
                        footer
                            .update_thinking_variant(client.thinking_variant_summary().as_deref());
                        footer.update_context_window(state.context_window);
                        live_repl.set_footer(footer.clone());
                        repl_note(
                            &mut live_repl,
                            &format!("{}\n", t("configuration reloaded", "配置已重新加载")),
                        )?;
                    }
                    Ok(false) => {}
                    Err(error) => {
                        repl_note(&mut live_repl, &format!("\x1b[31m{error:#}\x1b[0m\n"))?
                    }
                }
                ReplSlashCommand::Models => {
                    // Switches this session's pinned model; the change takes
                    // effect from the next turn without a daemon reload.
                    let argument = command_args.trim();
                    let result = run_models_for_session(
                        paths,
                        ModelsArgs {
                            target: (!argument.is_empty()).then(|| argument.to_string()),
                        },
                        Some(&active_session_id),
                    )
                    .await;
                    if let Err(error) = result {
                        repl_note(&mut live_repl, &format!("\x1b[31m{error:#}\x1b[0m\n"))?;
                        continue;
                    }
                    let session_config =
                        footer_config_for_session(paths, &config, &active_session_id);
                    let (state, _) =
                        repl_active_or_default_state(paths, &active_session_id).await?;
                    cumulative_tokens = state_cumulative(&state);
                    footer = ReplFooterStatus::from_config(
                        &session_config,
                        state.context_tokens,
                        cumulative_tokens,
                    );
                    let client = OpenAiCompatibleClient::from_config(&session_config, paths)?;
                    let thinking_summary = client.thinking_variant_summary();
                    footer.update_thinking_variant(thinking_summary.as_deref());
                    footer.update_context_window(state.context_window);
                    // Push the rebuilt footer into the live editor now; without
                    // this the on-screen model label stays stale until the next
                    // input event redraws the editor.
                    live_repl.set_footer(footer.clone());
                    repl_note(
                        &mut live_repl,
                        &format!(
                            "\x1b[2m{}\x1b[0m\n",
                            t(
                                "session model updated; takes effect from the next turn",
                                "会话模型已更新，下一轮生效"
                            )
                        ),
                    )?;
                }
                ReplSlashCommand::Config => {
                    crate::config_tui::run(paths)?;
                    let Some((_, _)) =
                        repl_ipc_admin(paths, &mut live_repl, IpcCommand::ReloadConfig).await?
                    else {
                        continue;
                    };
                    let refreshed = AppConfig::load(paths)?;
                    config = refreshed;
                    let (state, changed) =
                        repl_active_or_default_state(paths, &active_session_id).await?;
                    if changed {
                        apply_repl_session_switch(
                            paths,
                            &config,
                            &state,
                            &mut active_session_id,
                            &mut history,
                            &mut live_repl,
                            &mut footer,
                            &mut cumulative_tokens,
                        )
                        .await?;
                    }
                    cumulative_tokens = state_cumulative(&state);
                    footer = ReplFooterStatus::from_config(
                        &footer_config_for_session(paths, &config, &active_session_id),
                        state.context_tokens,
                        cumulative_tokens,
                    );
                    let client = OpenAiCompatibleClient::from_config(&config, paths)?;
                    let thinking_summary = client.thinking_variant_summary();
                    footer.update_thinking_variant(thinking_summary.as_deref());
                    footer.update_context_window(state.context_window);
                    live_repl.set_footer(footer.clone());
                    repl_note(
                        &mut live_repl,
                        &format!("{}\n", t("configuration reloaded", "配置已重新加载")),
                    )?;
                }
                ReplSlashCommand::Variant => {
                    if !crate::models_cache::is_loaded() {
                        repl_note(
                            &mut live_repl,
                            &format!(
                                "{}\n",
                                t(
                                    "model metadata is still loading; try /variant again shortly",
                                    "模型元数据仍在加载，请稍后重试 /variant"
                                )
                            ),
                        )?;
                        continue;
                    }
                    let selected = command_args.trim();
                    let mut client = OpenAiCompatibleClient::from_config(&config, paths)?;
                    match execute_variant(
                        paths,
                        &mut client,
                        (!selected.is_empty()).then_some(selected),
                        "/variant",
                    )? {
                        VariantOutcome::Updated => {
                            let Some((_, _)) =
                                repl_ipc_admin(paths, &mut live_repl, IpcCommand::ReloadConfig)
                                    .await?
                            else {
                                continue;
                            };
                            config = AppConfig::load(paths)?;
                            let (state, changed) =
                                repl_active_or_default_state(paths, &active_session_id).await?;
                            if changed {
                                apply_repl_session_switch(
                                    paths,
                                    &config,
                                    &state,
                                    &mut active_session_id,
                                    &mut history,
                                    &mut live_repl,
                                    &mut footer,
                                    &mut cumulative_tokens,
                                )
                                .await?;
                            }
                            cumulative_tokens = state_cumulative(&state);
                            footer = ReplFooterStatus::from_config(
                                &footer_config_for_session(paths, &config, &active_session_id),
                                state.context_tokens,
                                cumulative_tokens,
                            );
                            let thinking_summary = client.thinking_variant_summary();
                            footer.update_thinking_variant(thinking_summary.as_deref());
                            footer.update_context_window(state.context_window);
                            live_repl.set_footer(footer.clone());
                            repl_note(
                                &mut live_repl,
                                &format!("{}\n", t("thinking variants updated", "已更新思考档位")),
                            )?;
                        }
                        VariantOutcome::Cancelled => {}
                        VariantOutcome::Rejected(message) => {
                            repl_note(&mut live_repl, &format!("\x1b[31m{message}\x1b[0m"))?;
                        }
                    }
                }
                ReplSlashCommand::Undo => {
                    let Some((state, data)) = repl_ipc_admin(
                        paths,
                        &mut live_repl,
                        IpcCommand::Undo {
                            target: crate::ipc::SessionRef::Id {
                                id: active_session_id.clone(),
                            },
                        },
                    )
                    .await?
                    else {
                        continue;
                    };
                    let removed = data
                        .get("removed")
                        .and_then(serde_json::Value::as_u64)
                        .unwrap_or(0);
                    repl_note(
                        &mut live_repl,
                        &format!("{}: {removed}\n", t("undone messages", "已撤销消息数")),
                    )?;
                    if let Some(prompt) = data.get("prompt").and_then(serde_json::Value::as_str) {
                        live_repl.editor.input = prompt.to_string();
                        live_repl.editor.cursor = live_repl.editor.input.chars().count();
                        live_repl.editor.history_clean_index = None;
                    }
                    cumulative_tokens = state_cumulative(&state);
                    footer.update_session_tokens(state.context_tokens);
                    footer.update_context_window(state.context_window);
                    footer.update_cumulative_tokens(cumulative_tokens);
                }
                ReplSlashCommand::Pop => {
                    let count = match parse_repl_pop_count(command_args) {
                        Ok(count) => count,
                        Err(err) => {
                            repl_note(
                                &mut live_repl,
                                &format!("\x1b[31m{}: {err}\x1b[0m", t("error", "错误")),
                            )?;
                            continue;
                        }
                    };
                    let state_store = StateStore::new(paths)?.pinned(&active_session_id);
                    state_store.recover_stale_turns()?;
                    let candidates =
                        state_store.oldest_evictable_visible_turns(count.unwrap_or(usize::MAX))?;
                    let turn_ids = if count.is_some() {
                        candidates.into_iter().map(|turn| turn.turn_id).collect()
                    } else {
                        let all = state_store.oldest_evictable_visible_turns(usize::MAX)?;
                        if all.is_empty() {
                            repl_note(&mut live_repl, &repl_nothing_to_pop_text())?;
                            continue;
                        }
                        let Some(selected) = inline_pop_select(&all)? else {
                            continue;
                        };
                        all.into_iter()
                            .zip(selected)
                            .filter_map(|(turn, selected)| selected.then_some(turn.turn_id))
                            .collect()
                    };
                    let Some((state, data)) = repl_ipc_admin(
                        paths,
                        &mut live_repl,
                        IpcCommand::Pop {
                            target: crate::ipc::SessionRef::Id {
                                id: active_session_id.clone(),
                            },
                            turn_ids,
                        },
                    )
                    .await?
                    else {
                        continue;
                    };
                    let turns = data
                        .get("turns")
                        .and_then(serde_json::Value::as_u64)
                        .unwrap_or(0);
                    if turns > 0 {
                        let outcome = PopOutcome {
                            turns: turns as usize,
                            archived: data
                                .get("archived")
                                .and_then(serde_json::Value::as_bool)
                                .unwrap_or(false),
                        };
                        repl_note(&mut live_repl, &repl_pop_outcome_text(outcome))?;
                    } else {
                        repl_note(&mut live_repl, &repl_nothing_to_pop_text())?;
                    }
                    cumulative_tokens = state_cumulative(&state);
                    footer.update_session_tokens(state.context_tokens);
                    footer.update_context_window(state.context_window);
                    footer.update_cumulative_tokens(cumulative_tokens);
                }
                ReplSlashCommand::Compact => {
                    repl_note(
                        &mut live_repl,
                        &format!(
                            "\x1b[2m{}\x1b[0m",
                            t("compacting context…", "正在压缩上下文…")
                        ),
                    )?;
                    let Some((state, data)) = repl_ipc_admin(
                        paths,
                        &mut live_repl,
                        IpcCommand::Compact {
                            target: crate::ipc::SessionRef::Id {
                                id: active_session_id.clone(),
                            },
                        },
                    )
                    .await?
                    else {
                        continue;
                    };
                    if let Some(usage) = data
                        .get("usage")
                        .cloned()
                        .filter(|value| !value.is_null())
                        .map(serde_json::from_value::<Usage>)
                        .transpose()?
                    {
                        repl_note(
                            &mut live_repl,
                            &format!("\x1b[2m{}\x1b[0m\n", t("context compacted", "上下文已压缩")),
                        )?;
                        let result = ChatResult {
                            content: String::new(),
                            reasoning: None,
                            usage: Some(usage),
                            usage_estimated: data
                                .get("usage_estimated")
                                .and_then(serde_json::Value::as_bool)
                                .unwrap_or(false),
                            tool_calls: Vec::new(),
                            provider_id: None,
                            model: None,
                            finish_reason: None,
                            thinking_signature: None,
                            last_request_usage: None,
                            responses_continuation: None,
                        };
                        print_chat_token_usage(
                            &result,
                            config.display.show_token_usage,
                            state.context_tokens,
                            state.context_window,
                            state_cumulative(&state),
                        )?;
                    } else {
                        repl_note(
                            &mut live_repl,
                            &format!(
                                "\x1b[2m{}\x1b[0m\n",
                                t("nothing to compact", "没有可压缩的上下文")
                            ),
                        )?;
                    }
                    cumulative_tokens = state_cumulative(&state);
                    footer.update_session_tokens(state.context_tokens);
                    footer.update_context_window(state.context_window);
                    footer.update_cumulative_tokens(cumulative_tokens);
                }
                ReplSlashCommand::Reset => {
                    let Some((state, _)) = repl_ipc_admin(
                        paths,
                        &mut live_repl,
                        IpcCommand::ResetConversation {
                            target: crate::ipc::SessionRef::Id {
                                id: active_session_id.clone(),
                            },
                        },
                    )
                    .await?
                    else {
                        continue;
                    };
                    live_repl.editor.input.clear();
                    live_repl.editor.cursor = 0;
                    // The footer numbers are only half of it: the loop's own Σ
                    // accumulator has to go too, or the next config reload
                    // rebuilds the footer from the pre-reset total. The queue
                    // rows were deleted in the store, so the strip has to be
                    // reloaded rather than left showing them.
                    cumulative_tokens = TurnTokens::default();
                    footer.reset_token_usage(state.context_tokens, state.context_window);
                    reload_repl_queue(&mut live_repl, paths, &active_session_id)?;
                    repl_note(
                        &mut live_repl,
                        &format!(
                            "\x1b[2m{}\x1b[0m\n",
                            t("cleared current conversation history", "已清空当前会话历史")
                        ),
                    )?;
                }
                ReplSlashCommand::Wipe => {
                    repl_note(
                        &mut live_repl,
                        &format!("\x1b[2m{}\x1b[0m\n", wipe_summary()),
                    )?;
                    if !confirm_inline(&mut live_repl, t("wipe everything?", "确认全部抹掉？"))?
                    {
                        repl_note(
                            &mut live_repl,
                            &format!("\x1b[2m{}\x1b[0m\n", t("cancelled", "已取消")),
                        )?;
                        continue;
                    }
                    let Some((state, _)) =
                        repl_ipc_admin(paths, &mut live_repl, IpcCommand::WipePersona).await?
                    else {
                        continue;
                    };
                    live_repl.editor.input.clear();
                    live_repl.editor.cursor = 0;
                    cumulative_tokens = TurnTokens::default();
                    footer.reset_token_usage(state.context_tokens, state.context_window);
                    reload_repl_queue(&mut live_repl, paths, &active_session_id)?;
                    repl_note(
                        &mut live_repl,
                        &format!("\x1b[2m{}\x1b[0m\n", print_wipe_message()),
                    )?;
                }
            }
            continue;
        }
        if input.is_empty() {
            continue;
        }
        history.push(input.to_string());
        live_repl.editor.record_history(input);
        persist_repl_history_entry(paths, input);
        match try_run_remote_chat(
            paths,
            Some(&mut live_repl),
            input,
            None,
            false,
            mode,
            &images,
            Some(active_session_id.clone()),
            Some(&jobs_feed),
        )
        .await
        {
            Ok(Some(summary)) => {
                cumulative_tokens = summary.cumulative_tokens;
                footer.update_token_usage(
                    &summary.result,
                    summary.context_tokens,
                    summary.context_window,
                    cumulative_tokens,
                );
                // Refresh the job strip right away — a background command
                // spawned this turn must show up without waiting a poll.
                if let Ok((mut jobs, _, wake_runs)) = fetch_jobs_overview(paths).await {
                    retain_session_jobs(
                        &mut jobs,
                        jobs_shared.repl_session.lock().unwrap().as_deref(),
                    );
                    *jobs_shared.jobs.lock().unwrap() = jobs.clone();
                    *jobs_shared.wake_runs.lock().unwrap() = wake_runs;
                    live_repl.set_jobs(jobs);
                }
                live_repl.refresh_footer(footer.clone())?;
            }
            Ok(None) => bail!(
                "{}",
                t(
                    "the Miyu Web core stopped; start the REPL again to use direct mode",
                    "Miyu Web 核心已停止；请重新启动 REPL 以使用直连模式"
                )
            ),
            Err(err)
                if is_remote_turn_cancelled(&err)
                    || crate::question::is_question_cancelled(&err) =>
            {
                let frame = format!("\x1b[2m{}\x1b[0m\n\n", t("cancelled", "已取消"));
                live_repl.apply_output_frame(frame.as_bytes())?;
                // The interrupted turn still entered the context; refresh the
                // footer from the daemon's post-cancel state.
                if let Ok((state, _)) =
                    repl_active_or_default_state(paths, &active_session_id).await
                {
                    cumulative_tokens = state_cumulative(&state);
                    footer.update_session_tokens(state.context_tokens);
                    footer.update_cumulative_tokens(state_cumulative(&state));
                    footer.update_context_window(state.context_window);
                }
            }
            Err(err) => {
                let frame = format!("\x1b[31m{}: {err}\x1b[0m\n\n", t("error", "错误"));
                live_repl.apply_output_frame(frame.as_bytes())?;
                if let Ok((state, true)) =
                    repl_active_or_default_state(paths, &active_session_id).await
                {
                    apply_repl_session_switch(
                        paths,
                        &config,
                        &state,
                        &mut active_session_id,
                        &mut history,
                        &mut live_repl,
                        &mut footer,
                        &mut cumulative_tokens,
                    )
                    .await?;
                }
            }
        }
    }
    // Background commands follow their conversation's lifecycle: leaving the
    // REPL (exit/quit or Ctrl+C) terminates this session's running jobs.
    if let Ok((_, data)) = send_ipc_admin(
        paths,
        IpcCommand::StopSessionJobs {
            session_id: active_session_id.clone(),
        },
    )
    .await
    {
        let stopped = data
            .get("stopped")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0);
        if stopped > 0 {
            println!(
                "\x1b[2m{}\x1b[0m",
                if is_zh() {
                    format!("已停止 {stopped} 个后台命令")
                } else {
                    format!("stopped {stopped} background command(s)")
                }
            );
        }
    }
    Ok(())
}

fn direct_mode_requested() -> bool {
    std::env::var_os("MIYU_DIRECT").is_some_and(|value| value != "0")
}

async fn run_direct_repl(paths: &MiyuPaths, initial_mode: AgentMode) -> Result<()> {
    let _core_lease = ipc::acquire_direct_core(paths)?;
    initialize_models_cache(paths);
    let _cursor_restore = ReplCursorRestore;
    AppConfig::init_files(paths)?;
    let mut config = AppConfig::load_or_default(paths)?;
    tools::jobs::init(paths);
    let state = StateStore::new(paths)?;
    state.init_files()?;
    // Same lane as the remote REPL: resume where the last REPL was, not where
    // shell-hook happens to be pointing.
    let persona = config.active_persona_scope();
    if let Ok(Some(session_id)) = state.repl_session(&persona) {
        state.adopt_session(&session_id);
    } else {
        let _ = state.set_repl_session(&persona, &state.session_id());
    }
    apply_session_model_override(&state, &mut config);
    let memory_organizer = MemoryOrganizer::spawn()?;
    let memory_organizer_handle = memory_organizer.handle();
    memory_organizer_handle.wake(config.clone(), paths.clone(), state.clone());
    let mut client = OpenAiCompatibleClient::from_config(&config, paths)?;
    let mut mode = initial_mode;
    let mut input_history = load_repl_input_history(&state, paths)?;
    let mut prefill = None::<String>;
    let mut live_repl = None::<LiveReplTail>;

    crate::default_kb::check_update_if_due(paths).ok();
    if let Ok(Some(message)) = crate::default_kb::notice_if_update_available(paths) {
        println!("\x1b[2m{message}\x1b[0m");
    }
    let mut cumulative_tokens = state.session_cumulative_token_totals().unwrap_or_default();
    let mut show_shortcut_hint = true;
    let initial_registry =
        build_tool_registry(&config, paths, mode, crate::question_tui::available(false))?;
    let mut agent = Agent::new(
        config.clone(),
        paths,
        state.clone(),
        client.clone(),
        initial_registry,
        mode,
    )?;
    agent.set_memory_organizer(memory_organizer_handle);
    agent.prepare_for_turn()?;
    let mut footer = ReplFooterStatus::from_config(
        &config,
        agent.effective_context_tokens()?,
        TurnTokens::default(),
    );
    let thinking_summary = client.thinking_variant_summary();
    footer.update_thinking_variant(thinking_summary.as_deref());
    footer.update_context_window(agent.context_window());
    loop {
        let thinking_summary = client.thinking_variant_summary();
        footer.update_thinking_variant(thinking_summary.as_deref());
        let next_input = if let Some(live) = live_repl.as_mut() {
            live.set_footer(footer.clone());
            let input = match read_live_repl_input(live, paths, &JobsFeed::Local, None)? {
                LiveReplOutcome::Exit | LiveReplOutcome::FollowWake { .. } => None,
                // Direct mode owns its jobs in-process, so stop them here
                // rather than through the daemon.
                LiveReplOutcome::StopJobs => {
                    for job in crate::tools::jobs::overview() {
                        if job.running {
                            let _ = crate::tools::jobs::stop_job(&job.job_id).await;
                        }
                    }
                    continue;
                }
                LiveReplOutcome::Submit(next_mode, input, images) => {
                    Some((next_mode, input, images))
                }
            };
            // The user moved on: finished background commands count as
            // reported in direct mode (no daemon wake exists here).
            for job in crate::tools::jobs::overview() {
                if !job.running {
                    crate::tools::jobs::acknowledge(&job.job_id);
                }
            }
            input
        } else {
            read_repl_input(
                paths,
                mode,
                prefill.take(),
                &input_history,
                &footer,
                show_shortcut_hint,
            )?
        };
        let (input, pasted_images) = match next_input {
            Some((new_mode, input, pasted_images)) => {
                mode = new_mode;
                (input, pasted_images)
            }
            None => break,
        };
        let input = input.trim();
        let (command_input, command_args) = split_repl_command(input);
        let command = resolve_repl_command(command_input);
        let command_args_empty = command_args.trim().is_empty();
        if input.eq_ignore_ascii_case("exit")
            || input.eq_ignore_ascii_case("quit")
            || (command.eq_ignore_ascii_case("/exit") && command_args_empty)
        {
            break;
        }
        if command.eq_ignore_ascii_case("/help") && command_args_empty {
            print_repl_help();
            continue;
        }
        if command.eq_ignore_ascii_case("/usage") && command_args_empty {
            let snapshot = state.usage_snapshot()?;
            let context_tokens = agent.effective_context_tokens()?;
            let context = Some((context_tokens, agent.context_window()));
            println!("{}", usage_overview_text(&snapshot, context));
            if let Some(window) = agent.context_window() {
                println!(
                    "{}",
                    compact_watermark_text(context_tokens as usize, window, &config.context)
                );
            }
            println!();
            continue;
        }
        if command.eq_ignore_ascii_case("/persona") {
            match run_persona_picker(paths, command_args) {
                Ok(true) => {
                    reload_repl_config(paths, &state, &mut config, &mut client)?;
                    footer = ReplFooterStatus::from_config(
                        &config,
                        agent.effective_context_tokens()?,
                        cumulative_tokens,
                    );
                    let thinking_summary = client.thinking_variant_summary();
                    footer.update_thinking_variant(thinking_summary.as_deref());
                    let registry = build_tool_registry(
                        &config,
                        paths,
                        mode,
                        crate::question_tui::available(false),
                    )?;
                    agent.reload_config(config.clone(), client.clone())?;
                    agent.switch_mode(mode, registry);
                    footer.update_context_window(agent.context_window());
                    println!("{}", t("configuration reloaded", "配置已重新加载"));
                }
                Ok(false) => {}
                Err(error) => println!("\x1b[31m{error:#}\x1b[0m"),
            }
            println!();
            continue;
        }
        if command.eq_ignore_ascii_case("/models") {
            let argument = command_args.trim();
            let repl_session_id = state.session_id();
            run_models_for_session(
                paths,
                ModelsArgs {
                    target: (!argument.is_empty()).then(|| argument.to_string()),
                },
                Some(&repl_session_id),
            )
            .await?;
            reload_repl_config(paths, &state, &mut config, &mut client)?;
            footer = ReplFooterStatus::from_config(
                &config,
                agent.effective_context_tokens()?,
                cumulative_tokens,
            );
            let thinking_summary = client.thinking_variant_summary();
            footer.update_thinking_variant(thinking_summary.as_deref());
            let registry =
                build_tool_registry(&config, paths, mode, crate::question_tui::available(false))?;
            agent.reload_config(config.clone(), client.clone())?;
            agent.switch_mode(mode, registry);
            footer.update_context_window(agent.context_window());
            if let Some(live) = live_repl.as_mut() {
                live.set_footer(footer.clone());
            }
            println!("{}", t("configuration reloaded", "配置已重新加载"));
            println!();
            continue;
        }
        if command.eq_ignore_ascii_case("/config") && command_args_empty {
            crate::config_tui::run(paths)?;
            reload_repl_config(paths, &state, &mut config, &mut client)?;
            footer = ReplFooterStatus::from_config(
                &config,
                agent.effective_context_tokens()?,
                cumulative_tokens,
            );
            let thinking_summary = client.thinking_variant_summary();
            footer.update_thinking_variant(thinking_summary.as_deref());
            let registry =
                build_tool_registry(&config, paths, mode, crate::question_tui::available(false))?;
            agent.reload_config(config.clone(), client.clone())?;
            agent.switch_mode(mode, registry);
            footer.update_context_window(agent.context_window());
            if let Some(live) = live_repl.as_mut() {
                live.set_footer(footer.clone());
            }
            println!("{}", t("configuration reloaded", "配置已重新加载"));
            println!();
            continue;
        }
        if command.eq_ignore_ascii_case("/variant") {
            if !crate::models_cache::is_loaded() {
                println!(
                    "{}\n",
                    t(
                        "model metadata is still loading; try /variant again shortly",
                        "模型元数据仍在加载，请稍后重试 /variant"
                    )
                );
                continue;
            }
            let selected = command_args.trim();
            match execute_variant(
                paths,
                &mut client,
                (!selected.is_empty()).then_some(selected),
                "/variant",
            )? {
                VariantOutcome::Updated => {
                    let thinking_summary = client.thinking_variant_summary();
                    footer.update_thinking_variant(thinking_summary.as_deref());
                    agent.replace_client(client.clone());
                    print_variant_updated();
                }
                VariantOutcome::Cancelled => {}
                VariantOutcome::Rejected(message) => {
                    eprintln!("\x1b[31m{message}\x1b[0m");
                }
            }
            continue;
        }
        if command.eq_ignore_ascii_case("/undo") && command_args_empty {
            let (removed, prompt) = state.undo_last_turn()?;
            footer.update_session_tokens(agent.effective_context_tokens()?);
            if removed > 0 && prompt.is_none() {
                println!("{}", t("context compaction undone", "已撤销上下文压缩"));
            } else {
                println!("{}: {removed}", t("undone messages", "已撤销消息数"));
            }
            if let Some(prompt) = prompt {
                if let Some(live) = live_repl.as_mut() {
                    live.editor.input = prompt;
                    live.editor.cursor = live.editor.input.chars().count();
                    live.editor.history_clean_index = None;
                } else {
                    prefill = Some(prompt);
                }
            }
            continue;
        }
        if command.eq_ignore_ascii_case("/pop") {
            let count = match parse_repl_pop_count(command_args) {
                Ok(count) => count,
                Err(err) => {
                    eprintln!("\x1b[31m{}: {err}\x1b[0m", t("error", "错误"));
                    continue;
                }
            };
            state.recover_stale_turns()?;
            match execute_pop(paths, &config, &state, count) {
                Ok(Some(outcome)) => {
                    print_pop_outcome(outcome);
                    footer.update_session_tokens(agent.effective_context_tokens()?);
                }
                Ok(None) => {}
                Err(err) => {
                    eprintln!("\x1b[31m{}: {err}\x1b[0m", t("error", "错误"));
                }
            }
            continue;
        }
        if command.eq_ignore_ascii_case("/compact") && command_args_empty {
            let reasoning_mode =
                render::ReasoningDisplayMode::from_config(&config.display.reasoning);
            let tool_call_mode =
                render::ToolCallDisplayMode::from_config(&config.display.tool_calls);
            let mut renderer = render::StreamRenderer::new(
                reasoning_mode,
                tool_call_mode,
                false,
                config.display.readable_tool_names,
                config.display.command_output_lines,
            );
            match agent
                .compact_now(|event| handle_agent_event(&mut renderer, event))
                .await
            {
                Ok(Some(result)) => {
                    renderer.finish()?;
                    if let Some(usage) = result.usage.as_ref() {
                        cumulative_tokens.add(TurnTokens::from_usage(Some(usage)));
                    }
                    footer.update_token_usage(
                        &result,
                        agent.effective_context_tokens()?,
                        agent.context_window(),
                        cumulative_tokens,
                    );
                    if config.display.show_token_usage {
                        print_chat_token_usage(
                            &result,
                            true,
                            agent.effective_context_tokens()?,
                            agent.context_window(),
                            cumulative_tokens,
                        )?;
                    }
                }
                Ok(None) => {
                    renderer.finish()?;
                    println!(
                        "\x1b[2m{}\x1b[0m",
                        t("nothing to compact", "没有可压缩的上下文")
                    );
                    footer.update_session_tokens(agent.effective_context_tokens()?);
                }
                Err(err) => {
                    renderer.finish()?;
                    eprintln!("\x1b[31m{}: {err}\x1b[0m", t("error", "错误"));
                }
            }
            continue;
        }
        if command.eq_ignore_ascii_case("/reset") && command_args.trim().is_empty() {
            run_reset(paths).await?;
            if let Some(live) = live_repl.as_mut() {
                live.queued.clear();
            }
            cumulative_tokens = TurnTokens::default();
            footer.reset_token_usage(agent.effective_context_tokens()?, agent.context_window());
            continue;
        }
        if command.eq_ignore_ascii_case("/wipe") {
            println!("{}", wipe_summary());
            if !confirm_stdin(t("wipe everything?", "确认全部抹掉？"))? {
                println!("{}", t("cancelled", "已取消"));
                continue;
            }
            run_wipe(paths, true).await?;
            if let Some(live) = live_repl.as_mut() {
                live.queued.clear();
            }
            agent.reset_memory()?;
            cumulative_tokens = TurnTokens::default();
            footer.reset_token_usage(agent.effective_context_tokens()?, agent.context_window());
            continue;
        }
        if input.is_empty() {
            continue;
        }
        input_history.push(input.to_string());
        persist_repl_history_entry(paths, input);
        if let Some(live) = live_repl.as_mut() {
            live.editor.record_history(input);
        }
        if agent.mode() != mode {
            let registry =
                build_tool_registry(&config, paths, mode, crate::question_tui::available(false))?;
            agent.switch_mode(mode, registry);
        }
        agent.prepare_for_turn()?;
        let reasoning_mode = render::ReasoningDisplayMode::from_config(&config.display.reasoning);
        let tool_call_mode = render::ToolCallDisplayMode::from_config(&config.display.tool_calls);
        let mut renderer = render::StreamRenderer::new(
            reasoning_mode,
            tool_call_mode,
            false,
            config.display.readable_tool_names,
            config.display.command_output_lines,
        );
        let control = AgentTurnControl::new(
            mode,
            build_tool_registry(
                &config,
                paths,
                AgentMode::Normal,
                crate::question_tui::available(false),
            )?,
            build_tool_registry(
                &config,
                paths,
                AgentMode::Chat,
                crate::question_tui::available(false),
            )?,
        );
        if live_repl.is_none() {
            live_repl = Some(LiveReplTail::new(
                mode,
                input_history.clone(),
                state.load_queued_prompts()?,
                footer.clone(),
            )?);
        }
        let live = live_repl.as_mut().expect("live REPL was initialized");
        let chat_result = run_live_agent_turn(
            live,
            paths,
            &state,
            &mut agent,
            LiveAgentInput {
                content: input,
                images: &pasted_images,
            },
            &control,
            &mut renderer,
        )
        .await;
        mode = live.mode();
        match chat_result {
            Ok(Some(result)) => {
                let context_window =
                    result_context_window(&config, &result).or(agent.context_window());
                let mut turn_tokens = TurnTokens::from_usage(result.usage.as_ref());
                if let Some(usage) = result.usage.as_ref() {
                    cumulative_tokens.add(TurnTokens::from_usage(Some(usage)));
                }
                let context_tokens = agent.effective_context_tokens()?;
                footer.update_token_usage(
                    &result,
                    context_tokens,
                    context_window,
                    cumulative_tokens,
                );
                let endpoint_variant = result.provider_id.as_deref().and_then(|provider_id| {
                    result
                        .model
                        .as_deref()
                        .and_then(|model| client.thinking_variant_for(provider_id, model))
                });
                if show_mixed_model_endpoint(&config, true) {
                    let provider = result.provider_id.as_deref().unwrap_or("-");
                    let model = result.model.as_deref().unwrap_or("-");
                    let frame = format!(
                        "\x1b[2m{}\x1b[0m\n",
                        mixed_model_endpoint_label(provider, model, endpoint_variant.as_deref())
                    );
                    live.apply_output_frame(frame.as_bytes())?;
                }
                match handle_live_post_turn_overflow(
                    live,
                    &agent,
                    &mut renderer,
                    context_tokens,
                    config.display.show_token_usage,
                    Some(&mut cumulative_tokens),
                )
                .await
                {
                    Ok(Some(compact_result)) => {
                        if let Some(usage) = compact_result.usage.as_ref() {
                            turn_tokens.add(TurnTokens::from_usage(Some(usage)));
                        }
                        footer.set_token_usage_with_cache(
                            turn_tokens,
                            agent.effective_context_tokens()?,
                            agent.context_window(),
                            cumulative_tokens,
                        );
                    }
                    Ok(None) => {
                        footer.update_session_tokens(agent.effective_context_tokens()?);
                    }
                    Err(err) => {
                        let frame = format!("\x1b[31m{}: {err}\x1b[0m\n", t("error", "错误"));
                        live.apply_output_frame(frame.as_bytes())?;
                        continue;
                    }
                }
                live.refresh_footer(footer.clone())?;
                show_shortcut_hint = false;
            }
            Ok(None) => {
                // An explicit cancel also withdraws the queued follow-ups;
                // reloading afterwards clears their bubbles.
                let _ = state.delete_queued_prompts();
                if let Some(live) = live_repl.as_mut() {
                    synchronized_terminal_update(CursorAfterUpdate::Shown, || {
                        live.reload_queue(&state)
                    })?;
                }
                // An interrupted turn is persisted and will be replayed into
                // the next request, so the context meter must reflect it.
                cumulative_tokens = state
                    .session_cumulative_token_totals()
                    .unwrap_or(cumulative_tokens);
                footer.update_session_tokens(agent.effective_context_tokens()?);
                footer.update_cumulative_tokens(cumulative_tokens);
            }
            Err(err) if crate::question::is_question_cancelled(&err) => {
                let _ = state.delete_queued_prompts();
                if let Some(live) = live_repl.as_mut() {
                    synchronized_terminal_update(CursorAfterUpdate::Shown, || {
                        live.reload_queue(&state)
                    })?;
                }
                cumulative_tokens = state
                    .session_cumulative_token_totals()
                    .unwrap_or(cumulative_tokens);
                footer.update_session_tokens(agent.effective_context_tokens()?);
                footer.update_cumulative_tokens(cumulative_tokens);
                continue;
            }
            Err(err) => {
                if let Some(live) = live_repl.as_mut() {
                    let frame = format!("\x1b[31m{}: {err}\x1b[0m\n", t("error", "错误"));
                    live.apply_output_frame(frame.as_bytes())?;
                    synchronized_terminal_update(CursorAfterUpdate::Shown, || {
                        live.reload_queue(&state)
                    })?;
                }
                continue;
            }
        }
    }
    state.discard_queued_prompts()?;
    // Background jobs are children of this REPL process; never leave them
    // running once the host is gone.
    tools::jobs::shutdown_all();
    Ok(())
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

/// The footer/status display must reflect the session's pinned model pool,
/// not just the global config.
fn footer_config_for_session(paths: &MiyuPaths, config: &AppConfig, session_id: &str) -> AppConfig {
    let mut config = config.clone();
    if let Ok(Some(models)) =
        StateStore::new(paths).and_then(|store| store.session_model_override(session_id))
    {
        config.active_provider_models = Some(models);
    }
    config
}

/// Direct (daemon-less) sessions read their pinned model pool straight from
/// the state store; daemon-run turns get the same treatment in the turn task.
fn apply_session_model_override(state: &StateStore, config: &mut AppConfig) {
    match state.session_model_override(&state.session_id()) {
        Ok(Some(models)) => config.active_provider_models = Some(models),
        Ok(None) => {}
        Err(error) => tracing::warn!(
            error = %error,
            "{}",
            t(
                "loading the session model override failed",
                "读取会话模型覆盖失败"
            )
        ),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct VariantMenuItem {
    provider_id: String,
    model: String,
    options: Vec<VariantMenuOption>,
    selected: usize,
    cursor: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct VariantMenuOption {
    label: String,
    value: Option<String>,
}

impl VariantMenuItem {
    fn from_options(options: &ThinkingVariantOptions) -> Self {
        let mut variants = vec![VariantMenuOption {
            label: "default".to_string(),
            value: None,
        }];
        variants.extend(options.variants.iter().map(|variant| VariantMenuOption {
            label: if variant == "default" {
                "default (variant)".to_string()
            } else {
                variant.clone()
            },
            value: Some(variant.clone()),
        }));
        let selected = options
            .selected
            .as_ref()
            .and_then(|selected| {
                variants
                    .iter()
                    .position(|variant| variant.value.as_ref() == Some(selected))
            })
            .unwrap_or(0);
        Self {
            provider_id: options.provider_id.clone(),
            model: options.model.clone(),
            options: variants,
            selected,
            cursor: selected,
        }
    }

    fn selection(&self) -> (String, String, Option<String>) {
        (
            self.provider_id.clone(),
            self.model.clone(),
            self.options[self.selected].value.clone(),
        )
    }

    fn check_cursor(&mut self) {
        self.selected = self.cursor;
    }
}

fn inline_variant_select(
    options: &[ThinkingVariantOptions],
) -> Result<Option<Vec<(String, String, Option<String>)>>> {
    let mut items = options
        .iter()
        .map(VariantMenuItem::from_options)
        .collect::<Vec<_>>();
    if items.is_empty() {
        return Ok(None);
    }
    if items.len() == 1 {
        return inline_single_variant_select(items.remove(0));
    }
    let max_options = items
        .iter()
        .map(|item| item.options.len())
        .max()
        .unwrap_or(1);
    let menu_lines = inline_fuzzy_lines(items.len().max(max_options));
    reserve_inline_fuzzy_space(menu_lines)?;
    let mut session = InlineRawMode::start()?;
    let mut active_column = 0usize;
    let mut model_index = 0usize;
    let mut model_scroll = 0usize;
    let mut variant_scroll = 0usize;
    let (_, cursor_y) = cursor::position().unwrap_or((0, menu_lines.saturating_sub(1)));
    let anchor_y = cursor_y.saturating_sub(menu_lines.saturating_sub(1));
    loop {
        let visible = menu_lines.saturating_sub(2) as usize;
        model_scroll = inline_fuzzy_scroll(model_index, model_scroll, visible.min(items.len()));
        let item = &items[model_index];
        variant_scroll =
            inline_fuzzy_scroll(item.cursor, variant_scroll, visible.min(item.options.len()));
        draw_inline_variant(
            &mut session.stdout,
            anchor_y,
            menu_lines,
            &items,
            active_column,
            model_index,
            model_scroll,
            variant_scroll,
        )?;
        if let Event::Key(KeyEvent {
            code, modifiers, ..
        }) = event::read()?
        {
            match code {
                KeyCode::Char('c') if modifiers.contains(KeyModifiers::CONTROL) => {
                    clear_inline_fuzzy(&mut session.stdout, anchor_y, menu_lines)?;
                    return Ok(None);
                }
                KeyCode::Esc | KeyCode::Char('q') => {
                    clear_inline_fuzzy(&mut session.stdout, anchor_y, menu_lines)?;
                    return Ok(None);
                }
                KeyCode::Enter => {
                    clear_inline_fuzzy(&mut session.stdout, anchor_y, menu_lines)?;
                    return Ok(Some(items.iter().map(VariantMenuItem::selection).collect()));
                }
                KeyCode::Left | KeyCode::Char('h') => active_column = 0,
                KeyCode::Right | KeyCode::Char('l') => active_column = 1,
                KeyCode::Up | KeyCode::Char('k') if active_column == 0 => {
                    model_index = model_index.saturating_sub(1);
                    variant_scroll = 0;
                }
                KeyCode::Down | KeyCode::Char('j') if active_column == 0 => {
                    model_index = (model_index + 1).min(items.len() - 1);
                    variant_scroll = 0;
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    items[model_index].cursor = items[model_index].cursor.saturating_sub(1);
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    let last = items[model_index].options.len() - 1;
                    items[model_index].cursor = (items[model_index].cursor + 1).min(last);
                }
                KeyCode::Tab if active_column == 1 => {
                    items[model_index].check_cursor();
                }
                _ => {}
            }
        }
    }
}

fn inline_single_variant_select(
    mut item: VariantMenuItem,
) -> Result<Option<Vec<(String, String, Option<String>)>>> {
    let menu_lines = inline_fuzzy_lines(item.options.len());
    reserve_inline_fuzzy_space(menu_lines)?;
    let mut session = InlineRawMode::start()?;
    let mut scroll = 0usize;
    let (_, cursor_y) = cursor::position().unwrap_or((0, menu_lines.saturating_sub(1)));
    let anchor_y = cursor_y.saturating_sub(menu_lines.saturating_sub(1));
    loop {
        let visible = menu_lines.saturating_sub(2) as usize;
        scroll = inline_fuzzy_scroll(item.cursor, scroll, visible.min(item.options.len()));
        draw_inline_single_variant(&mut session.stdout, anchor_y, menu_lines, &item, scroll)?;
        if let Event::Key(KeyEvent {
            code, modifiers, ..
        }) = event::read()?
        {
            match code {
                KeyCode::Char('c') if modifiers.contains(KeyModifiers::CONTROL) => {
                    clear_inline_fuzzy(&mut session.stdout, anchor_y, menu_lines)?;
                    return Ok(None);
                }
                KeyCode::Esc | KeyCode::Char('q') => {
                    clear_inline_fuzzy(&mut session.stdout, anchor_y, menu_lines)?;
                    return Ok(None);
                }
                KeyCode::Enter => {
                    clear_inline_fuzzy(&mut session.stdout, anchor_y, menu_lines)?;
                    return Ok(Some(vec![item.selection()]));
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    item.cursor = item.cursor.saturating_sub(1);
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    item.cursor = (item.cursor + 1).min(item.options.len() - 1);
                }
                KeyCode::Tab => item.check_cursor(),
                _ => {}
            }
        }
    }
}

fn draw_inline_single_variant(
    stdout: &mut io::Stdout,
    anchor_y: u16,
    menu_lines: u16,
    item: &VariantMenuItem,
    scroll: usize,
) -> Result<()> {
    let (cols, _) = terminal::size().unwrap_or((80, 24));
    let bar = inline_fuzzy_bar();
    let available = (cols as usize).saturating_sub(visible_width(&bar)).max(1);
    let width = single_variant_content_width(item).min(available);
    let visible = menu_lines.saturating_sub(2) as usize;
    queue!(stdout, Hide)?;
    for row in 0..menu_lines {
        queue!(
            stdout,
            MoveTo(0, anchor_y + row),
            Clear(ClearType::CurrentLine)
        )?;
    }
    queue!(
        stdout,
        MoveTo(0, anchor_y),
        Print(&bar),
        Print(variant_menu_header(
            t("Thinking variant", "思考档位"),
            true,
            width,
        )),
    )?;
    for row in 0..visible {
        let index = scroll + row;
        let line = item.options.get(index).map_or_else(
            || " ".repeat(width),
            |variant| {
                variant_menu_cell(
                    &variant.label,
                    index == item.cursor,
                    index == item.cursor,
                    Some(index == item.selected),
                    width,
                )
            },
        );
        queue!(
            stdout,
            MoveTo(0, anchor_y + row as u16 + 1),
            Print(&bar),
            Print(line),
        )?;
    }
    queue!(
        stdout,
        MoveTo(0, anchor_y + menu_lines.saturating_sub(1)),
        Print(&bar),
        Print(format!(
            "\x1b[2m{}\x1b[0m",
            truncate_visible_width(
                t(
                    "j/k move · Tab select · Enter confirm · Esc/q cancel",
                    "j/k 移动 · Tab 勾选 · Enter 确认 · Esc/q 取消"
                ),
                available,
            )
        ))
    )?;
    stdout.flush()?;
    Ok(())
}

fn single_variant_content_width(item: &VariantMenuItem) -> usize {
    item.options
        .iter()
        .map(|option| visible_width(&option.label).saturating_add(6))
        .chain(std::iter::once(visible_width(t(
            "Thinking variant",
            "思考档位",
        ))))
        .max()
        .unwrap_or(1)
}

fn draw_inline_variant(
    stdout: &mut io::Stdout,
    anchor_y: u16,
    menu_lines: u16,
    items: &[VariantMenuItem],
    active_column: usize,
    model_index: usize,
    model_scroll: usize,
    variant_scroll: usize,
) -> Result<()> {
    let (cols, _) = terminal::size().unwrap_or((80, 24));
    let bar = inline_fuzzy_bar();
    let width = (cols as usize).saturating_sub(visible_width(&bar)).max(1);
    let separator = if width >= 3 { " │ " } else { "" };
    let available = width.saturating_sub(visible_width(separator));
    let (left_width, right_width) = variant_menu_column_widths(items, available);
    let visible = menu_lines.saturating_sub(2) as usize;
    queue!(stdout, Hide)?;
    for row in 0..menu_lines {
        queue!(
            stdout,
            MoveTo(0, anchor_y + row),
            Clear(ClearType::CurrentLine)
        )?;
    }
    queue!(
        stdout,
        MoveTo(0, anchor_y),
        Print(&bar),
        Print(variant_menu_header(
            t("Provider / Model", "Provider / 模型"),
            active_column == 0,
            left_width,
        )),
        Print(format!("\x1b[2m{separator}\x1b[0m")),
        Print(variant_menu_header(
            t("Thinking variant", "思考档位"),
            active_column == 1,
            right_width,
        )),
    )?;
    let variants = &items[model_index];
    for row in 0..visible {
        let left_index = model_scroll + row;
        let right_index = variant_scroll + row;
        let left = items.get(left_index).map_or_else(
            || " ".repeat(left_width),
            |item| {
                variant_menu_cell(
                    &format!("{} / {}", item.provider_id, item.model),
                    active_column == 0 && left_index == model_index,
                    left_index == model_index,
                    None,
                    left_width,
                )
            },
        );
        let right = variants.options.get(right_index).map_or_else(
            || " ".repeat(right_width),
            |variant| {
                variant_menu_cell(
                    &variant.label,
                    active_column == 1 && right_index == variants.cursor,
                    right_index == variants.cursor,
                    Some(right_index == variants.selected),
                    right_width,
                )
            },
        );
        queue!(
            stdout,
            MoveTo(0, anchor_y + row as u16 + 1),
            Print(&bar),
            Print(left),
            Print(format!("\x1b[2m{separator}\x1b[0m")),
            Print(right),
        )?;
    }
    queue!(
        stdout,
        MoveTo(0, anchor_y + menu_lines.saturating_sub(1)),
        Print(&bar),
        Print(format!(
            "\x1b[2m{}\x1b[0m",
            truncate_visible_width(
                t(
                    "h/l switch · j/k move · Tab select · Enter confirm · Esc/q cancel",
                    "h/l 切栏 · j/k 移动 · Tab 勾选 · Enter 确认 · Esc/q 取消"
                ),
                width,
            )
        ))
    )?;
    stdout.flush()?;
    Ok(())
}

fn variant_menu_column_widths(items: &[VariantMenuItem], available: usize) -> (usize, usize) {
    if available == 0 {
        return (0, 0);
    }
    if available == 1 {
        return (1, 0);
    }
    let left_needed = items
        .iter()
        .map(|item| {
            visible_width(&format!("{} / {}", item.provider_id, item.model)).saturating_add(2)
        })
        .chain(std::iter::once(visible_width(t(
            "Provider / Model",
            "Provider / 模型",
        ))))
        .max()
        .unwrap_or(1);
    let right_needed = items
        .iter()
        .flat_map(|item| item.options.iter())
        .map(|option| visible_width(&option.label).saturating_add(6))
        .chain(std::iter::once(visible_width(t(
            "Thinking variant",
            "思考档位",
        ))))
        .max()
        .unwrap_or(1);
    if left_needed.saturating_add(right_needed) <= available {
        return (left_needed, right_needed);
    }
    let total_needed = left_needed.saturating_add(right_needed).max(1);
    let left = available
        .saturating_mul(left_needed)
        .saturating_div(total_needed)
        .clamp(1, available - 1);
    (left, available - left)
}

fn variant_menu_header(label: &str, active: bool, width: usize) -> String {
    let label = pad_visible_width(&truncate_visible_width(label, width), width);
    if active {
        format!("\x1b[1m\x1b[35m{label}\x1b[0m")
    } else {
        format!("\x1b[1m{label}\x1b[0m")
    }
}

fn variant_menu_cell(
    label: &str,
    focused: bool,
    highlighted: bool,
    checked: Option<bool>,
    width: usize,
) -> String {
    let marker = if highlighted { "›" } else { " " };
    let check = match checked {
        Some(true) => "[*] ",
        Some(false) => "[ ] ",
        None => "",
    };
    let line = pad_visible_width(
        &truncate_visible_width(&format!("{marker} {check}{label}"), width),
        width,
    );
    if focused {
        format!("\x1b[1m\x1b[35m{line}\x1b[0m")
    } else if checked == Some(true) {
        format!("\x1b[1m\x1b[32m{line}\x1b[0m")
    } else if highlighted {
        format!("\x1b[1m{line}\x1b[0m")
    } else {
        format!("\x1b[2m{line}\x1b[0m")
    }
}

fn pad_visible_width(value: &str, width: usize) -> String {
    format!(
        "{value}{}",
        " ".repeat(width.saturating_sub(visible_width(value)))
    )
}

fn split_repl_command(input: &str) -> (&str, &str) {
    let Some((command, args)) = input.split_once(char::is_whitespace) else {
        return (input, "");
    };
    (command, args)
}

const REPL_HISTORY_CAP: usize = 200;

fn repl_history_file(paths: &MiyuPaths) -> PathBuf {
    paths.state_dir.join("repl-history.jsonl")
}

/// Prompt history that survives /reset and restarts: a global append-only
/// file, capped on load. Conversation resets delete turns, so the file is
/// the durable source; the turns-derived list only seeds sessions that
/// predate it.
fn load_persistent_repl_history(paths: &MiyuPaths) -> Vec<String> {
    let Ok(content) = std::fs::read_to_string(repl_history_file(paths)) else {
        return Vec::new();
    };
    let mut entries = content
        .lines()
        .filter_map(|line| serde_json::from_str::<String>(line).ok())
        .filter(|entry| !entry.trim().is_empty())
        .collect::<Vec<_>>();
    if entries.len() > REPL_HISTORY_CAP {
        entries = entries.split_off(entries.len() - REPL_HISTORY_CAP);
        // Opportunistic rewrite keeps the file from growing without bound.
        let rewritten = entries
            .iter()
            .filter_map(|entry| serde_json::to_string(entry).ok())
            .collect::<Vec<_>>()
            .join("\n");
        let _ = std::fs::write(repl_history_file(paths), rewritten + "\n");
    }
    entries
}

fn persist_repl_history_entry(paths: &MiyuPaths, entry: &str) {
    let entry = entry.trim();
    if entry.is_empty() {
        return;
    }
    let Ok(line) = serde_json::to_string(entry) else {
        return;
    };
    let path = repl_history_file(paths);
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

fn load_repl_input_history(state: &StateStore, paths: &MiyuPaths) -> Result<Vec<String>> {
    let mut merged: Vec<String> = state
        .load_conversation()?
        .into_iter()
        .filter(|entry| entry.role == "user" && !entry.content.trim().is_empty())
        .map(|entry| strip_terminal_control_sequences(&entry.content))
        .filter(|content| !content.trim().is_empty())
        .collect();
    for entry in load_persistent_repl_history(paths) {
        if !merged.contains(&entry) {
            merged.push(entry);
        }
    }
    Ok(merged)
}

fn print_repl_help() {
    println!("{}", t("commands:", "命令:"));
    let width = REPL_COMMAND_TABLE
        .iter()
        .map(|spec| {
            spec.name.len()
                + if spec.arg_hint.is_empty() {
                    0
                } else {
                    spec.arg_hint.len() + 1
                }
        })
        .max()
        .unwrap_or(0);
    for spec in REPL_COMMAND_TABLE {
        let invocation = if spec.arg_hint.is_empty() {
            spec.name.to_string()
        } else {
            format!("{} {}", spec.name, spec.arg_hint)
        };
        println!("  {invocation:<width$}  {}", t(spec.help_en, spec.help_zh));
    }
    println!("{}", t("keys:", "快捷键:"));
    println!(
        "  Tab         {}",
        t(
            "cycle NORMAL/CHAT, or complete slash commands",
            "循环切换 普通/闲聊，或补全斜杠菜单"
        )
    );
    println!("  Enter       {}", t("send message", "发送消息"));
    println!("  Shift+Enter {}", t("insert newline", "插入换行"));
    println!(
        "  Ctrl+J      {}",
        t(
            "insert newline, same as Shift+Enter",
            "插入换行，与 Shift+Enter 相同"
        )
    );
    println!(
        "  Ctrl+V      {}",
        t(
            "paste image or text from clipboard",
            "从剪贴板粘贴图片或文本"
        )
    );
    println!("  Ctrl+L      {}", t("clear screen", "清屏"));
    println!(
        "  Up/Down     {}",
        t("browse input history", "切换输入历史")
    );
    println!(
        "  Esc Esc     {}",
        t("interrupt running reply", "中断当前回复")
    );
    println!(
        "  Ctrl+C      {}",
        t(
            "clear the draft, else interrupt the reply, else stop background tasks, else exit",
            "先清空输入；输入为空则中断回复；再无回复则停止后台任务；都没有则退出"
        )
    );
    println!("  Ctrl+D      {}", t("exit", "退出"));
}

struct LiveReplEditor {
    mode: AgentMode,
    input: String,
    cursor: usize,
    history: Vec<String>,
    history_index: usize,
    history_clean_index: Option<usize>,
    is_pasted: bool,
    pasted_images: Vec<Option<crate::clipboard::PastedImage>>,
    pasted_texts: Vec<Option<PastedText>>,
    escape_armed_until: Option<Instant>,
    /// Whether the terminal window currently has focus, per the terminal's own
    /// focus reporting. Starts `true`: a terminal that never reports focus
    /// leaves this pinned, and notifications stay quiet rather than firing on
    /// every turn.
    focused: bool,
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

enum LiveEditorAction {
    None,
    Redraw,
    ClearScreen,
    EmptySubmit,
    Submit(LiveSubmission),
    Interrupt,
    Exit,
}

impl LiveReplEditor {
    fn new(mode: AgentMode, history: Vec<String>) -> Self {
        let history_index = history.len();
        Self {
            mode,
            input: String::new(),
            cursor: 0,
            history,
            history_index,
            history_clean_index: None,
            is_pasted: false,
            pasted_images: Vec::new(),
            pasted_texts: Vec::new(),
            escape_armed_until: None,
            focused: true,
        }
    }

    fn clear(&mut self) {
        self.input.clear();
        self.cursor = 0;
        self.history_clean_index = None;
        self.is_pasted = false;
        self.pasted_images.clear();
        self.pasted_texts.clear();
        self.escape_armed_until = None;
    }

    fn submit(&mut self) -> Option<LiveSubmission> {
        let display_content = strip_terminal_control_sequences(&self.input);
        let content = expand_pasted_text_placeholders(&display_content, &self.pasted_texts);
        let content = content.trim().to_string();
        if content.is_empty() {
            return None;
        }
        let display_content = display_content.trim().to_string();
        let images = std::mem::take(&mut self.pasted_images);
        self.input.clear();
        self.cursor = 0;
        self.history_clean_index = None;
        self.is_pasted = false;
        self.pasted_texts.clear();
        Some(LiveSubmission {
            content,
            display_content,
            images,
        })
    }

    fn record_history(&mut self, content: &str) {
        self.history.push(content.to_string());
        self.history_index = self.history.len();
    }

    fn handle_event(
        &mut self,
        event: Event,
        paths: &MiyuPaths,
        allow_interrupt: bool,
    ) -> Result<LiveEditorAction> {
        let is_escape = matches!(
            &event,
            Event::Key(KeyEvent {
                code: KeyCode::Esc,
                ..
            })
        );
        if !is_escape {
            self.escape_armed_until = None;
        }
        match event {
            Event::Key(KeyEvent {
                kind: KeyEventKind::Release,
                ..
            }) => return Ok(LiveEditorAction::None),
            Event::Resize(_, _) => return Ok(LiveEditorAction::Redraw),
            // Focus reporting gates notifications: no popup while you are
            // looking at the window.
            Event::FocusGained => {
                self.focused = true;
                return Ok(LiveEditorAction::None);
            }
            Event::FocusLost => {
                self.focused = false;
                return Ok(LiveEditorAction::None);
            }
            Event::Paste(text) => {
                insert_pasted_text_at_cursor(
                    &mut self.input,
                    &mut self.cursor,
                    text,
                    &mut self.pasted_texts,
                );
                self.history_clean_index = None;
                self.is_pasted = true;
                return Ok(LiveEditorAction::Redraw);
            }
            Event::Key(KeyEvent {
                code, modifiers, ..
            }) => match code {
                KeyCode::Tab => {
                    if self.input.starts_with('/') {
                        if let Some(completed) = complete_repl_command(&self.input) {
                            self.input = completed.to_string();
                            self.cursor = self.input.chars().count();
                            self.history_clean_index = None;
                        }
                    } else {
                        self.mode = match self.mode {
                            AgentMode::Normal => AgentMode::Chat,
                            AgentMode::Chat => AgentMode::Normal,
                        };
                    }
                }
                KeyCode::Esc => {
                    // Esc never clears typed input (Ctrl+C does that); it
                    // only arms the double-press interrupt while a reply is
                    // running.
                    if allow_interrupt {
                        if self
                            .escape_armed_until
                            .is_some_and(|deadline| Instant::now() < deadline)
                        {
                            self.escape_armed_until = None;
                            return Ok(LiveEditorAction::Interrupt);
                        }
                        self.escape_armed_until = Some(Instant::now() + Duration::from_secs(2));
                    }
                }
                KeyCode::Left => {
                    if let Some((start, _)) = placeholder_at_cursor(&self.input, self.cursor) {
                        self.cursor = start;
                    } else {
                        self.cursor = self.cursor.saturating_sub(1);
                    }
                }
                KeyCode::Right => {
                    if let Some((_, end)) = placeholder_at_cursor(&self.input, self.cursor) {
                        self.cursor = end;
                    } else {
                        self.cursor = (self.cursor + 1).min(self.input.chars().count());
                    }
                }
                KeyCode::Home => self.cursor = 0,
                KeyCode::End => self.cursor = self.input.chars().count(),
                KeyCode::Up => {
                    if !self.history.is_empty()
                        && repl_should_browse_history(
                            &self.input,
                            &self.history,
                            self.history_clean_index,
                        )
                    {
                        if self.input.is_empty() {
                            self.history_index = self.history.len();
                        }
                        self.history_index = self.history_index.saturating_sub(1);
                        self.input = self
                            .history
                            .get(self.history_index)
                            .cloned()
                            .unwrap_or_default();
                        self.cursor = self.input.chars().count();
                        self.history_clean_index = Some(self.history_index);
                        self.is_pasted = false;
                        self.pasted_images.clear();
                        self.pasted_texts.clear();
                    } else {
                        self.cursor = repl_move_cursor_vertical("  ", &self.input, self.cursor, -1);
                    }
                }
                KeyCode::Down => {
                    if repl_history_is_clean(&self.input, &self.history, self.history_clean_index) {
                        if self.history_index + 1 < self.history.len() {
                            self.history_index += 1;
                            self.input = self
                                .history
                                .get(self.history_index)
                                .cloned()
                                .unwrap_or_default();
                            self.cursor = self.input.chars().count();
                            self.history_clean_index = Some(self.history_index);
                        } else {
                            self.history_index = self.history.len();
                            self.input.clear();
                            self.cursor = 0;
                            self.history_clean_index = None;
                        }
                        self.is_pasted = false;
                        self.pasted_images.clear();
                        self.pasted_texts.clear();
                    } else {
                        self.cursor = repl_move_cursor_vertical("  ", &self.input, self.cursor, 1);
                    }
                }
                KeyCode::Enter if modifiers.contains(KeyModifiers::SHIFT) => {
                    // Shift+Enter 与 Ctrl+J 相同：在光标处插入换行，不提交
                    insert_newline_at_cursor(&mut self.input, &mut self.cursor);
                    self.history_clean_index = None;
                    self.is_pasted = false;
                }
                KeyCode::Enter => {
                    return Ok(self
                        .submit()
                        .map(LiveEditorAction::Submit)
                        .unwrap_or(LiveEditorAction::EmptySubmit));
                }
                KeyCode::Char('j') if modifiers.contains(KeyModifiers::CONTROL) => {
                    insert_newline_at_cursor(&mut self.input, &mut self.cursor);
                    self.history_clean_index = None;
                    self.is_pasted = false;
                }
                KeyCode::Char('c')
                    if modifiers.contains(KeyModifiers::CONTROL)
                        && !modifiers.contains(KeyModifiers::SHIFT) =>
                {
                    if self.input.is_empty() {
                        return Ok(LiveEditorAction::Interrupt);
                    }
                    self.clear();
                }
                KeyCode::Char('d')
                    if modifiers.contains(KeyModifiers::CONTROL) && self.input.is_empty() =>
                {
                    return Ok(LiveEditorAction::Exit);
                }
                KeyCode::Char('w') if modifiers.contains(KeyModifiers::CONTROL) => {
                    if let Some((start, end)) =
                        placeholder_before_or_at_cursor(&self.input, self.cursor)
                    {
                        clear_placeholder_payload(
                            &self.input,
                            start,
                            end,
                            &mut self.pasted_images,
                            &mut self.pasted_texts,
                        );
                        remove_range_chars(&mut self.input, start, end);
                        self.cursor = start;
                    } else {
                        remove_word_before_cursor(&mut self.input, &mut self.cursor);
                    }
                    self.history_clean_index = None;
                    self.is_pasted = false;
                }
                KeyCode::Backspace => {
                    if self.cursor > 0 {
                        if let Some((start, end)) =
                            placeholder_before_or_at_cursor(&self.input, self.cursor)
                        {
                            clear_placeholder_payload(
                                &self.input,
                                start,
                                end,
                                &mut self.pasted_images,
                                &mut self.pasted_texts,
                            );
                            remove_range_chars(&mut self.input, start, end);
                            self.cursor = start;
                        } else {
                            remove_char_before_cursor(&mut self.input, &mut self.cursor);
                        }
                        self.history_clean_index = None;
                    }
                    self.is_pasted = false;
                }
                KeyCode::Delete => {
                    if let Some((start, end)) =
                        placeholder_after_or_at_cursor(&self.input, self.cursor)
                    {
                        clear_placeholder_payload(
                            &self.input,
                            start,
                            end,
                            &mut self.pasted_images,
                            &mut self.pasted_texts,
                        );
                        remove_range_chars(&mut self.input, start, end);
                    } else {
                        remove_char_at_cursor(&mut self.input, self.cursor);
                    }
                    self.history_clean_index = None;
                    self.is_pasted = false;
                }
                KeyCode::Char('c' | 'C')
                    if modifiers.contains(KeyModifiers::CONTROL)
                        && modifiers.contains(KeyModifiers::SHIFT) =>
                {
                    if let Some(selected) =
                        placeholder_text_near_cursor(&self.input, self.cursor, &self.pasted_texts)
                    {
                        let _ = crate::clipboard::write_clipboard_text(&selected)?;
                    }
                }
                KeyCode::Char('v') if modifiers.contains(KeyModifiers::CONTROL) => {
                    self.paste_clipboard(paths)?;
                }
                KeyCode::Char('l') if modifiers.contains(KeyModifiers::CONTROL) => {
                    return Ok(LiveEditorAction::ClearScreen);
                }
                KeyCode::Char(ch) if !modifiers.contains(KeyModifiers::CONTROL) => {
                    if !is_disallowed_control_char(ch) {
                        if let Some((_, end)) = placeholder_at_cursor(&self.input, self.cursor) {
                            self.cursor = end;
                        }
                        insert_char_at_cursor(&mut self.input, &mut self.cursor, ch);
                        self.history_clean_index = None;
                    }
                    self.is_pasted = false;
                }
                _ => return Ok(LiveEditorAction::None),
            },
            _ => return Ok(LiveEditorAction::None),
        }
        Ok(LiveEditorAction::Redraw)
    }

    fn paste_clipboard(&mut self, paths: &MiyuPaths) -> Result<()> {
        match crate::clipboard::read_clipboard() {
            Ok(crate::clipboard::ClipboardContent::Image(image)) => {
                let index = self.pasted_images.len() + 1;
                let placeholder = match image.write_temp_file(&paths.cache_dir, index) {
                    Ok(path) => {
                        let filename = path
                            .file_name()
                            .and_then(|name| name.to_str())
                            .unwrap_or("image");
                        format!("[Image {index}: {filename}]")
                    }
                    Err(_) => format!("[Image {index}]"),
                };
                insert_str_at_cursor(&mut self.input, &mut self.cursor, &placeholder);
                self.pasted_images
                    .push(Some(crate::clipboard::PastedImage::Binary(image)));
                self.is_pasted = false;
            }
            Ok(crate::clipboard::ClipboardContent::ImagePath(path)) => {
                let index = self.pasted_images.len() + 1;
                let filename = std::path::Path::new(&path)
                    .file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or("image");
                insert_str_at_cursor(
                    &mut self.input,
                    &mut self.cursor,
                    &format!("[Image {index}: {filename}]"),
                );
                self.pasted_images
                    .push(Some(crate::clipboard::PastedImage::Path(path)));
                self.is_pasted = false;
            }
            Ok(crate::clipboard::ClipboardContent::TextPath(path)) => {
                insert_str_at_cursor(&mut self.input, &mut self.cursor, &path);
                self.is_pasted = false;
            }
            _ => {
                if let Ok(Some(text)) = crate::clipboard::read_clipboard_text() {
                    insert_pasted_text_at_cursor(
                        &mut self.input,
                        &mut self.cursor,
                        text,
                        &mut self.pasted_texts,
                    );
                    self.is_pasted = true;
                }
            }
        }
        self.history_clean_index = None;
        Ok(())
    }
}

struct LiveReplTail {
    editor: LiveReplEditor,
    queued: Vec<QueuedPrompt>,
    pending_chunks: Vec<ChatStreamChunk>,
    footer: ReplFooterStatus,
    jobs: Vec<crate::tools::jobs::JobOverview>,
    job_spinner: usize,
    output_cursor: (u16, u16),
    tail_start: u16,
    tail_rows: u16,
    input_cursor: (u16, u16),
    rendered: bool,
    external_output_active: bool,
    raw_mode_handoff: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct LiveTailPlacement {
    output_row: u16,
    tail_start: u16,
    overflow: u16,
    anchored: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TerminalFrameLayout {
    cursor: (u16, u16),
    occupied_bottom: Option<u16>,
}

struct TerminalFrameTracker {
    columns: usize,
    bottom_margin: Option<usize>,
    cursor_col: usize,
    cursor_row: usize,
    saved_cursor: (usize, usize, bool),
    pending_wrap: bool,
    pending_text: String,
    occupied_bottom: Option<usize>,
}

impl TerminalFrameTracker {
    fn new(start: (u16, u16), columns: u16, bottom_margin: Option<u16>) -> Self {
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
            occupied_bottom: None,
        }
    }

    fn finish(mut self) -> TerminalFrameLayout {
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

    fn flush_text(&mut self) {
        if self.pending_text.is_empty() {
            return;
        }
        let text = std::mem::take(&mut self.pending_text);
        for grapheme in text.graphemes(true) {
            self.print_width(UnicodeWidthStr::width(grapheme));
        }
    }

    fn print_width(&mut self, width: usize) {
        if width == 0 {
            return;
        }
        if self.pending_wrap || self.cursor_col.saturating_add(width) > self.columns {
            self.cursor_col = 0;
            self.index();
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
    }

    fn index(&mut self) {
        if self
            .bottom_margin
            .is_some_and(|bottom| self.cursor_row >= bottom)
        {
            return;
        }
        self.cursor_row = self.cursor_row.saturating_add(1);
    }

    fn move_down(&mut self, count: usize) {
        self.pending_wrap = false;
        self.cursor_row = self.cursor_row.saturating_add(count);
        if let Some(bottom) = self.bottom_margin {
            self.cursor_row = self.cursor_row.min(bottom);
        }
    }

    fn move_up(&mut self, count: usize) {
        self.pending_wrap = false;
        self.cursor_row = self.cursor_row.saturating_sub(count);
    }

    fn move_right(&mut self, count: usize) {
        self.pending_wrap = false;
        self.cursor_col = self
            .cursor_col
            .saturating_add(count)
            .min(self.columns.saturating_sub(1));
    }

    fn move_left(&mut self, count: usize) {
        self.pending_wrap = false;
        self.cursor_col = self.cursor_col.saturating_sub(count);
    }

    fn set_row(&mut self, row: usize) {
        self.pending_wrap = false;
        self.cursor_row = row;
        if let Some(bottom) = self.bottom_margin {
            self.cursor_row = self.cursor_row.min(bottom);
        }
    }

    fn set_col(&mut self, col: usize) {
        self.pending_wrap = false;
        self.cursor_col = col.min(self.columns.saturating_sub(1));
    }

    fn param(params: &VteParams, index: usize, default: usize) -> usize {
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

fn terminal_frame_layout(
    frame: &[u8],
    start: (u16, u16),
    columns: u16,
    bottom_margin: Option<u16>,
) -> TerminalFrameLayout {
    let mut parser = VteParser::new();
    let mut tracker = TerminalFrameTracker::new(start, columns, bottom_margin);
    parser.advance(&mut tracker, frame);
    tracker.finish()
}

fn live_frame_output_bottom(frame_margin: u16, layout: TerminalFrameLayout) -> Option<u16> {
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

#[derive(Clone, Copy)]
enum CursorAfterUpdate {
    Preserve,
    Shown,
    Hidden,
}

fn synchronized_terminal_update<T>(
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
fn live_tail_placement(
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
fn live_tail_next_start(current_start: u16, desired_tail: u16, max_tail: u16) -> u16 {
    if current_start >= max_tail {
        max_tail
    } else {
        desired_tail.min(max_tail)
    }
}

fn max_live_tail_start(terminal_rows: u16, tail_rows: u16) -> u16 {
    terminal_rows
        .max(1)
        .saturating_sub(1)
        .saturating_sub(tail_rows)
}

impl LiveReplTail {
    fn new(
        mode: AgentMode,
        history: Vec<String>,
        queued: Vec<QueuedPrompt>,
        footer: ReplFooterStatus,
    ) -> Result<Self> {
        Ok(Self {
            editor: LiveReplEditor::new(mode, history),
            queued,
            pending_chunks: Vec::new(),
            footer,
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

    fn mode(&self) -> AgentMode {
        self.editor.mode
    }

    fn set_footer(&mut self, footer: ReplFooterStatus) {
        self.footer = footer;
    }

    /// Replaces the footer and redraws the live editor immediately when it is
    /// already on screen. Without the redraw, token/context updates remain
    /// invisible until the next input event causes the editor to render.
    /// Update the background-command strip; returns true when a redraw is
    /// needed (content changed, or spinners/timers must advance).
    fn set_jobs(&mut self, jobs: Vec<crate::tools::jobs::JobOverview>) -> bool {
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
    fn tick_job_strip(&mut self) -> Result<()> {
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
        let start = self.tail_start.saturating_add(self.tail_rows).saturating_sub(rows);
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

    fn refresh_footer(&mut self, footer: ReplFooterStatus) -> Result<()> {
        self.set_footer(footer);
        if self.rendered {
            synchronized_terminal_update(CursorAfterUpdate::Shown, || self.redraw())?;
        }
        Ok(())
    }

    fn suspend(&mut self) -> Result<()> {
        if !self.rendered {
            return Ok(());
        }
        let mut stdout = io::stdout();
        let (_, terminal_rows) = terminal::size().unwrap_or((80, 24));
        for offset in 0..self.tail_rows {
            let row = self.tail_start.saturating_add(offset);
            if row >= terminal_rows {
                break;
            }
            queue!(stdout, MoveTo(0, row), Clear(ClearType::CurrentLine))?;
        }
        queue!(stdout, MoveTo(self.output_cursor.0, self.output_cursor.1))?;
        stdout.flush()?;
        self.rendered = false;
        Ok(())
    }

    fn resume(&mut self) -> Result<()> {
        self.resume_at(cursor_position_or(self.output_cursor))
    }

    fn resume_at(&mut self, (output_col, output_row): (u16, u16)) -> Result<()> {
        let (cols, terminal_rows) = terminal::size().unwrap_or((80, 24));
        let terminal_rows = terminal_rows.max(1);
        let editor_rows = repl_input_rendered_rows(
            &self.editor.input,
            self.editor.is_pasted,
            false,
            usize::from(cols),
        );
        let mut queue_lines =
            queued_prompt_lines(&self.queued, self.editor.mode, usize::from(cols));
        let queue_gap = u16::from(!queue_lines.is_empty());
        let max_queue_rows = terminal_rows.saturating_sub(editor_rows).saturating_sub(3) as usize;
        if queue_lines.len() > max_queue_rows {
            let omitted = queue_lines.len() - max_queue_rows.saturating_sub(1);
            let mut clipped = vec![format!(
                "\x1b[2m… {}\x1b[0m",
                if is_zh() {
                    format!("已隐藏 {omitted} 行排队内容")
                } else {
                    format!("{omitted} queued lines hidden")
                }
            )];
            let keep = max_queue_rows.saturating_sub(1);
            clipped.extend(queue_lines.split_off(queue_lines.len().saturating_sub(keep)));
            queue_lines = clipped;
        }
        let job_lines = background_job_lines(&self.jobs, self.job_spinner, usize::from(cols));
        let job_rows = job_lines.len().min(u16::MAX as usize) as u16;
        let total_rows = 1u16
            .saturating_add(queue_lines.len().min(u16::MAX as usize) as u16)
            .saturating_add(queue_gap)
            .saturating_add(editor_rows)
            .saturating_add(job_rows);
        // Derived from what is on screen rather than stored: the tail was
        // pinned to the bottom exactly when its bottom edge sat on the last
        // usable row. `suspend()` leaves both values untouched, so they are
        // still the previous frame's truth here, and a terminal resize simply
        // falls back to natural placement.
        let was_anchored = self.tail_rows > 0
            && self.tail_start.saturating_add(self.tail_rows) == terminal_rows.saturating_sub(1);
        let placement = live_tail_placement(
            output_col,
            output_row,
            total_rows,
            terminal_rows,
            was_anchored,
        );
        if placement.overflow > 0 {
            let mut stdout = io::stdout();
            queue!(stdout, MoveTo(0, terminal_rows.saturating_sub(1)))?;
            for _ in 0..placement.overflow {
                queue!(stdout, Print("\n"))?;
            }
            stdout.flush()?;
        }
        let output_row = placement.output_row;
        let tail_start = placement.tail_start;

        let mut stdout = io::stdout();
        queue!(stdout, MoveTo(0, tail_start), Clear(ClearType::CurrentLine))?;
        let mut row = tail_start.saturating_add(1);
        for line in &queue_lines {
            queue!(
                stdout,
                MoveTo(0, row),
                Clear(ClearType::CurrentLine),
                Print(line)
            )?;
            row = row.saturating_add(1);
        }
        if !queue_lines.is_empty() {
            queue!(stdout, MoveTo(0, row), Clear(ClearType::CurrentLine))?;
            row = row.saturating_add(1);
        }
        stdout.flush()?;

        let mut input_row = row;
        let mut rendered_rows = 0u16;
        render_repl_input_with_footer(
            &mut stdout,
            &mut input_row,
            &mut rendered_rows,
            self.editor.mode,
            &self.editor.input,
            self.editor.cursor,
            self.editor.is_pasted,
            &self.footer,
            false,
        )?;
        // The editor is back on screen: the cursor must be visible no
        // matter which path hid it (e.g. a question prompt suspended the
        // editor with the cursor hidden and then exited early). This is
        // the single convergence point for every editor redraw, so an
        // unconditional Show here prevents a permanently invisible cursor.
        self.input_cursor = cursor_position_or(self.input_cursor);
        if !job_lines.is_empty() {
            let mut stdout = io::stdout();
            let mut job_row = input_row.saturating_add(rendered_rows);
            for line in &job_lines {
                queue!(
                    stdout,
                    MoveTo(0, job_row),
                    Clear(ClearType::CurrentLine),
                    Print(line)
                )?;
                job_row = job_row.saturating_add(1);
            }
            queue!(stdout, MoveTo(self.input_cursor.0, self.input_cursor.1))?;
            stdout.flush()?;
        }
        execute!(io::stdout(), crossterm::cursor::Show)?;
        self.output_cursor = (output_col, output_row);
        self.tail_start = tail_start;
        self.tail_rows = total_rows;
        self.rendered = true;
        Ok(())
    }

    fn apply_output_frame(&mut self, frame: &[u8]) -> Result<()> {
        if frame.is_empty() {
            return Ok(());
        }
        if !self.rendered {
            io::stdout().write_all(frame)?;
            io::stdout().flush()?;
            self.output_cursor = cursor_position_or(self.output_cursor);
            return Ok(());
        }

        let (columns, terminal_rows) = terminal::size().unwrap_or((80, 24));
        let terminal_rows = terminal_rows.max(1);
        let unbounded = terminal_frame_layout(frame, self.output_cursor, columns, None);
        let natural_tail = unbounded
            .cursor
            .1
            .saturating_add(u16::from(unbounded.cursor.0 > 0));
        let occupied_tail = unbounded
            .occupied_bottom
            .map(|row| row.saturating_add(1))
            .unwrap_or(0);
        let desired_tail = natural_tail.max(occupied_tail);
        let max_tail = max_live_tail_start(terminal_rows, self.tail_rows);
        let next_tail = live_tail_next_start(self.tail_start, desired_tail, max_tail);
        let shift = i32::from(next_tail) - i32::from(self.tail_start);
        let frame_margin = if shift < 0 {
            self.tail_start
        } else {
            next_tail
        };
        let output_bottom = live_frame_output_bottom(frame_margin, unbounded);
        let leading_scroll = output_bottom
            .map(|bottom| self.output_cursor.1.saturating_sub(bottom))
            .unwrap_or(0);
        let frame_start = if let Some(bottom) = output_bottom.filter(|_| leading_scroll > 0) {
            (0, bottom)
        } else {
            self.output_cursor
        };
        let bounded = terminal_frame_layout(frame, frame_start, columns, output_bottom);

        let mut transaction = Vec::with_capacity(frame.len().saturating_add(96));
        if shift > 0 {
            queue!(
                transaction,
                MoveTo(0, self.tail_start.saturating_add(1)),
                Print(format!("\x1b[{shift}L"))
            )?;
        }
        if let Some(bottom) = output_bottom {
            queue!(
                transaction,
                Print(format!("\x1b[1;{}r", bottom.saturating_add(1)))
            )?;
        }
        if let Some(bottom) = output_bottom.filter(|_| leading_scroll > 0) {
            queue!(transaction, MoveTo(0, bottom))?;
            for _ in 0..leading_scroll {
                queue!(transaction, Print("\n"))?;
            }
        }
        queue!(transaction, MoveTo(frame_start.0, frame_start.1))?;
        transaction.extend_from_slice(frame);
        queue!(transaction, Print("\x1b[r"))?;
        if shift < 0 {
            queue!(
                transaction,
                MoveTo(0, next_tail.saturating_add(1)),
                Print(format!("\x1b[{}M", -shift))
            )?;
        }
        let input_row = (i32::from(self.input_cursor.1) + shift)
            .clamp(0, i32::from(terminal_rows.saturating_sub(1))) as u16;
        queue!(transaction, MoveTo(self.input_cursor.0, input_row))?;
        let mut stdout = io::stdout();
        stdout.write_all(&transaction)?;
        stdout.flush()?;

        self.output_cursor = bounded.cursor;
        self.tail_start = next_tail;
        self.input_cursor.1 = input_row;
        Ok(())
    }

    fn apply_renderer_frame(&mut self, renderer: &mut render::StreamRenderer) -> Result<()> {
        let frame = renderer.take_output_frame();
        self.apply_output_frame(&frame)
    }

    fn redraw(&mut self) -> Result<()> {
        let output_cursor = self.output_cursor;
        self.suspend()?;
        self.resume_at(output_cursor)
    }

    fn clear_screen(&mut self) -> Result<()> {
        self.suspend()?;
        let mut stdout = io::stdout();
        execute!(stdout, Clear(ClearType::All), MoveTo(0, 0))?;
        self.output_cursor = (0, 0);
        self.tail_start = 0;
        self.tail_rows = 0;
        self.resume_at((0, 0))
    }

    fn enqueue(&mut self, prompt: QueuedPrompt) -> Result<()> {
        let output_cursor = self.output_cursor;
        self.suspend()?;
        self.append_queued(prompt);
        self.resume_at(output_cursor)
    }

    fn append_queued(&mut self, prompt: QueuedPrompt) {
        self.queued.push(prompt);
        self.queued.sort_by_key(|prompt| prompt.seq);
    }

    fn queue_stream_chunk(&mut self, chunk: ChatStreamChunk) {
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

    fn flush_pending_chunks(&mut self, renderer: &mut render::StreamRenderer) -> Result<()> {
        for chunk in std::mem::take(&mut self.pending_chunks) {
            renderer.write_chunk(chunk)?;
        }
        Ok(())
    }

    fn discard_pending_chunks(&mut self) {
        self.pending_chunks.clear();
    }

    fn tick_spinner(&mut self, renderer: &mut render::StreamRenderer) -> Result<()> {
        self.flush_pending_chunks(renderer)?;
        renderer.tick_spinner()?;
        self.apply_renderer_frame(renderer)
    }

    fn commit_submission(&mut self, submission: &LiveSubmission) -> Result<()> {
        self.suspend()?;
        write_committed_user_messages(
            &[(submission.display_content.as_str(), self.editor.mode)],
            true,
        )?;
        self.output_cursor = cursor_position_or(self.output_cursor);
        Ok(())
    }

    fn commit_empty_submission(&mut self) -> Result<()> {
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
    fn show_background_report(&mut self, report: &BackgroundReport) -> Result<()> {
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
            queue!(stdout, Print(format!("{}\r\n", render::render_markdown_line(line))))?;
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
    fn drop_queued(&mut self, prompt_ids: &[String]) -> Result<()> {
        let ids = prompt_ids.iter().collect::<std::collections::HashSet<_>>();
        if !self.queued.iter().any(|prompt| ids.contains(&prompt.prompt_id)) {
            return Ok(());
        }
        let output_cursor = self.output_cursor;
        self.suspend()?;
        self.queued
            .retain(|prompt| !ids.contains(&prompt.prompt_id));
        self.resume_at(output_cursor)
    }

    fn consume_queued(&mut self, prompt_ids: &[String], mode: AgentMode) -> Result<()> {
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

    fn reload_queue(&mut self, state: &StateStore) -> Result<()> {
        let output_cursor = self.output_cursor;
        self.suspend()?;
        self.queued = state.load_queued_prompts()?;
        self.resume_at(output_cursor)
    }
}

fn repl_input_rendered_rows(
    input: &str,
    is_pasted: bool,
    show_shortcut_hint: bool,
    cols: usize,
) -> u16 {
    let suggestions = repl_command_suggestions(input);
    let lines = repl_input_lines(input);
    let display_lines =
        repl_visible_input_lines("  ", &lines, REPL_MAX_VISIBLE_INPUT_ROWS, is_pasted);
    let input_rows = repl_wrapped_input_rows_for_cols("  ", &display_lines, cols)
        .len()
        .max(1)
        .min(u16::MAX as usize) as u16;
    input_rows.saturating_add(if show_shortcut_hint && suggestions.is_empty() {
        4
    } else {
        3
    })
}

const JOB_SPINNER_FRAMES: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

fn format_job_duration(seconds: u64) -> String {
    if seconds >= 3600 {
        format!("{}h {:02}m", seconds / 3600, (seconds % 3600) / 60)
    } else if seconds >= 60 {
        format!("{}m {:02}s", seconds / 60, seconds % 60)
    } else {
        format!("{seconds}s")
    }
}

/// Status strip under the footer: a leading blank line, then one line per
/// background command with a blank line between entries. Timers are
/// right-aligned to the terminal width.
fn background_job_lines(
    jobs: &[crate::tools::jobs::JobOverview],
    spinner_phase: usize,
    cols: usize,
) -> Vec<String> {
    if jobs.is_empty() {
        return Vec::new();
    }
    let kind_label = |job: &crate::tools::jobs::JobOverview| {
        if job.kind == "subagent" {
            crate::i18n::text("agent", "子代理")
        } else {
            crate::i18n::text("cmd", "命令")
        }
    };
    // Pad kinds to one column so mixed command/subagent rows keep their ids
    // and titles vertically aligned.
    let kind_col = jobs
        .iter()
        .map(|job| visible_width(kind_label(job)))
        .max()
        .unwrap_or(0);
    let mut lines = vec![String::new()];
    for job in jobs.iter() {
        let marker = JOB_SPINNER_FRAMES[spinner_phase % JOB_SPINNER_FRAMES.len()];
        let kind_word = kind_label(job);
        let kind_pad = " ".repeat(kind_col.saturating_sub(visible_width(kind_word)));
        let mut left = format!("{marker} {kind_word}{kind_pad} {} · {}", job.job_id, job.title);
        let timer = format_job_duration(job.runtime_seconds);
        let timer_width = visible_width(&timer);
        // Never exceed the terminal width: a wrapped strip line would shift
        // the whole tail and flicker.
        let max_left = cols.saturating_sub(timer_width).saturating_sub(2);
        while visible_width(&left) > max_left && !left.is_empty() {
            left.pop();
        }
        let left_width = visible_width(&left);
        let pad = cols
            .saturating_sub(left_width)
            .saturating_sub(timer_width)
            .max(1);
        lines.push(format!(
            "\x1b[2m{left}{}{timer}\x1b[0m",
            " ".repeat(pad)
        ));
    }
    lines
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
    if messages.is_empty() {
        return Ok(());
    }
    let mut stdout = io::stdout();
    let col = cursor_col_or(0);
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

/// Strips the bracketed prefix off a background-job wake headline, leaving
/// `子代理完成 82bea3 · 标题`. The older `[后台命令完成] ` spelling still shows
/// up in sessions recorded before the rename.
fn job_wake_headline(headline: &str) -> &str {
    headline
        .strip_prefix("[后台任务完成] ")
        .or_else(|| headline.strip_prefix("[后台命令完成] "))
        .unwrap_or(headline)
}

/// Fires a desktop notification unless the REPL window has focus.
///
/// `focused` is `None` when there is no live tail — a one-shot `miyu ask` has
/// no window to be away from, so it stays quiet.
fn notify_if_unfocused(config: &AppConfig, focused: Option<bool>, title: &str, body: &str) {
    if !config.notifications.enabled || focused != Some(false) {
        return;
    }
    crate::notify::notify(title, &crate::notify::clip_body(body, 120));
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
        if replay.is_job_wake {
            // A background job woke the session; live rendering shows a dim
            // `⚙` notice, never a user bubble. Mirror that.
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

struct LiveRawMode {
    show_cursor_on_drop: bool,
    restore_terminal_on_drop: bool,
    keyboard_enhancement: KeyboardEnhancementState,
}

struct ReplCursorRestore;

impl Drop for ReplCursorRestore {
    fn drop(&mut self) {
        // 1. 会话级兜底：恢复括号粘贴与光标
        // 2. 再关闭 raw mode；键盘增强由 LiveRawMode / 局部输入作用域负责 Pop
        let _ = execute!(io::stdout(), DisableBracketedPaste, DisableFocusChange, Show);
        let _ = terminal::disable_raw_mode();
    }
}

impl LiveRawMode {
    /// 进入 live REPL 的 raw 输入模式，并尽量启用键盘增强协议。
    ///
    /// 参数: 无
    ///
    /// 返回:
    /// - 成功时返回会在 Drop 时恢复终端的守卫对象
    fn start() -> Result<Self> {
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
            show_cursor_on_drop: true,
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
    fn adopt() -> Self {
        Self {
            show_cursor_on_drop: true,
            restore_terminal_on_drop: true,
            keyboard_enhancement: KeyboardEnhancementState::assume_active(),
        }
    }

    fn keep_cursor_hidden(&mut self) {
        self.show_cursor_on_drop = false;
    }

    fn handoff(&mut self) {
        self.restore_terminal_on_drop = false;
        // handoff 后由下一段 LiveRawMode::adopt 继续持有键盘增强状态
        self.keyboard_enhancement = KeyboardEnhancementState::default();
    }
}

fn enable_live_raw_mode() -> Result<()> {
    terminal::enable_raw_mode()?;
    if let Err(error) = restore_live_output_processing() {
        let _ = terminal::disable_raw_mode();
        return Err(error);
    }
    Ok(())
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

impl Drop for LiveRawMode {
    fn drop(&mut self) {
        if !self.restore_terminal_on_drop {
            return;
        }
        let mut stdout = io::stdout();
        if self.show_cursor_on_drop {
            let _ = execute!(stdout, DisableBracketedPaste, DisableFocusChange, Show);
        } else {
            let _ = execute!(stdout, DisableBracketedPaste, DisableFocusChange);
        }
        // 1. 先 Pop 键盘增强协议
        // 2. 再退出 raw mode
        self.keyboard_enhancement.disable(&mut stdout);
        let _ = terminal::disable_raw_mode();
    }
}

/// Shared feed state between the remote REPL and its IPC poll thread.
#[derive(Default)]
struct SharedJobsFeed {
    /// The owning REPL's current session — strip snapshots are filtered to
    /// it (daemon "current session" can drift from the REPL's after /new).
    repl_session: std::sync::Mutex<Option<String>>,
    jobs: std::sync::Mutex<Vec<crate::tools::jobs::JobOverview>>,
    /// Rendered wake-turn reports waiting to be printed into the scrollback.
    reports: std::sync::Mutex<Vec<BackgroundReport>>,
    /// Latest session Σ read straight from the store. Background subagents
    /// bill to the session that launched them, but they finish long after the
    /// turn that spawned them published its totals — without this the footer
    /// sat on a stale Σ until the user happened to send another prompt.
    cumulative: std::sync::Mutex<Option<TurnTokens>>,
    /// Active daemon-initiated wake runs: (run_id, session_id, label).
    wake_runs: std::sync::Mutex<Vec<(String, String, String)>>,
    /// Wake runs already attached to (never re-follow), and turn ids that
    /// were rendered live (their DB report must not print again).
    followed_runs: std::sync::Mutex<std::collections::HashSet<String>>,
    rendered_turns: std::sync::Mutex<std::collections::HashSet<String>>,
}

#[derive(Clone)]
struct BackgroundReport {
    turn_id: String,
    headline: String,
    reply: String,
}

/// Session isolation for the strip: keep only `session`'s jobs (sessionless
/// jobs stay visible as a legacy fallback; `None` session shows everything).
fn retain_session_jobs(jobs: &mut Vec<crate::tools::jobs::JobOverview>, session: Option<&str>) {
    if let Some(session) = session {
        jobs.retain(|job| {
            job.session_id.is_none() || job.session_id.as_deref() == Some(session)
        });
    }
}

/// Source of background-command snapshots for the idle status strip.
enum JobsFeed {
    /// Remote REPL: snapshots pushed by the IPC poll thread.
    Shared(std::sync::Arc<SharedJobsFeed>),
    /// Direct REPL: read the in-process registry.
    Local,
}

impl JobsFeed {
    fn current(&self) -> Vec<crate::tools::jobs::JobOverview> {
        match self {
            JobsFeed::Shared(shared) => shared.jobs.lock().unwrap().clone(),
            JobsFeed::Local => crate::tools::jobs::overview(),
        }
    }

    /// The store's current Σ for the REPL's session, or `None` when this feed
    /// has no store behind it.
    fn cumulative(&self) -> Option<TurnTokens> {
        match self {
            JobsFeed::Shared(shared) => *shared.cumulative.lock().unwrap(),
            JobsFeed::Local => None,
        }
    }

    fn take_reports(&self) -> Vec<BackgroundReport> {
        match self {
            JobsFeed::Shared(shared) => {
                let mut reports = shared.reports.lock().unwrap();
                let rendered = shared.rendered_turns.lock().unwrap();
                let taken = reports
                    .drain(..)
                    .filter(|report| !rendered.contains(&report.turn_id))
                    .collect();
                taken
            }
            JobsFeed::Local => Vec::new(),
        }
    }

    /// Next wake run in `session` that has not been followed yet; marks it
    /// followed so the caller attaches exactly once.
    fn claim_wake_run(&self, session: &str) -> Option<(String, String)> {
        let JobsFeed::Shared(shared) = self else {
            return None;
        };
        let wake_runs = shared.wake_runs.lock().unwrap();
        let mut followed = shared.followed_runs.lock().unwrap();
        for (run_id, run_session, label) in wake_runs.iter() {
            if run_session == session && !followed.contains(run_id) {
                followed.insert(run_id.clone());
                return Some((run_id.clone(), label.clone()));
            }
        }
        None
    }
}

/// Poll the daemon for background commands while the remote REPL idles:
/// 1s when commands are live, 3s when quiet — a unix-socket roundtrip
/// costs microseconds either way.
fn spawn_jobs_poll_thread(paths: MiyuPaths) -> std::sync::Arc<SharedJobsFeed> {
    let shared = std::sync::Arc::new(SharedJobsFeed::default());
    let feed = shared.clone();
    std::thread::spawn(move || {
        let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        else {
            return;
        };
        // Track per-session watermarks so wake replies print exactly once,
        // and never replay history from before this REPL started.
        let mut seen: std::collections::HashMap<String, i64> = std::collections::HashMap::new();
        // The store open can lose a race against daemon writes (SQLITE_BUSY);
        // retry every cycle instead of deciding at startup forever.
        let mut store: Option<StateStore> = None;
        loop {
            if store.is_none() {
                store = StateStore::new(&paths).ok();
            }
            let (jobs, session_id, wake_runs) = runtime
                .block_on(async {
                    tokio::time::timeout(
                        std::time::Duration::from_millis(500),
                        fetch_jobs_overview(&paths),
                    )
                    .await
                    .unwrap_or_else(|_| Ok((Vec::new(), None, Vec::new())))
                })
                .unwrap_or_default();
            let mut jobs = jobs;
            let repl_session = { feed.repl_session.lock().unwrap().clone() };
            retain_session_jobs(&mut jobs, repl_session.as_deref());
            *feed.jobs.lock().unwrap() = jobs;
            *feed.wake_runs.lock().unwrap() = wake_runs;
            if let (Some(store), Some(session)) = (store.as_ref(), repl_session.as_deref()) {
                if let Ok(totals) = store.pinned(session).session_cumulative_token_totals() {
                    *feed.cumulative.lock().unwrap() = Some(totals);
                }
            }
            if let (Some(store), Some(session_id)) = (store.as_ref(), session_id) {
                let watermark = match seen.entry(session_id.clone()) {
                    std::collections::hash_map::Entry::Occupied(entry) => *entry.get(),
                    std::collections::hash_map::Entry::Vacant(entry) => {
                        let latest = store.latest_turn_seq(&session_id).unwrap_or(0);
                        *entry.insert(latest)
                    }
                };
                if let Ok(rows) = store.background_report_replies_after(&session_id, watermark) {
                    for (seq, turn_id, display, reply) in rows {
                        seen.insert(session_id.clone(), seq);
                        if feed.rendered_turns.lock().unwrap().contains(&turn_id) {
                            continue;
                        }
                        feed.reports.lock().unwrap().push(BackgroundReport {
                            turn_id,
                            headline: display,
                            reply,
                        });
                    }
                }
            }
            std::thread::sleep(std::time::Duration::from_secs(1));
        }
    });
    shared
}

type JobsOverviewSnapshot = (
    Vec<crate::tools::jobs::JobOverview>,
    Option<String>,
    Vec<(String, String, String)>,
);

async fn fetch_jobs_overview(paths: &MiyuPaths) -> Result<JobsOverviewSnapshot> {
    let mut stream = ipc::connect(&paths.ipc_socket()).await?;
    ipc::send(&mut stream, &IpcRequest::new(IpcCommand::JobsOverview)).await?;
    match ipc::receive::<IpcFrame>(&mut stream).await? {
        Some(IpcFrame::AdminResult { state, data }) => {
            let wake_runs = data
                .get("wake_runs")
                .and_then(serde_json::Value::as_array)
                .map(|rows| {
                    rows.iter()
                        .filter_map(|row| {
                            Some((
                                row.get("run_id")?.as_str()?.to_string(),
                                row.get("session_id")?.as_str()?.to_string(),
                                row.get("label")
                                    .and_then(serde_json::Value::as_str)
                                    .unwrap_or_default()
                                    .to_string(),
                            ))
                        })
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            Ok((
                data.get("jobs")
                    .cloned()
                    .map(serde_json::from_value)
                    .transpose()
                    .unwrap_or_default()
                    .unwrap_or_default(),
                Some(state.session_id),
                wake_runs,
            ))
        }
        _ => Ok((Vec::new(), None, Vec::new())),
    }
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
    FollowWake { run_id: String, label: String },
    /// Ctrl+C on an empty line while this session has background work: stop
    /// the work and stay in the REPL. Pressing it again then exits.
    StopJobs,
}

fn read_live_repl_input(
    live: &mut LiveReplTail,
    paths: &MiyuPaths,
    jobs_feed: &JobsFeed,
    wake_session: Option<&str>,
) -> Result<LiveReplOutcome> {
    let mut raw = if std::mem::take(&mut live.raw_mode_handoff) {
        LiveRawMode::adopt()
    } else {
        LiveRawMode::start()?
    };
    if !live.rendered {
        synchronized_terminal_update(CursorAfterUpdate::Shown, || live.resume())?;
    }
    let mut last_key_at = Instant::now();
    loop {
        if !event::poll(std::time::Duration::from_millis(80))? {
            // Idle tick: structural changes redraw the whole tail; otherwise
            // only the strip repaints. While the user is actively typing the
            // animation pauses so the two repaint sources never interleave.
            for report in jobs_feed.take_reports() {
                synchronized_terminal_update(CursorAfterUpdate::Preserve, || {
                    live.show_background_report(&report)
                })?;
            }
            let typing = last_key_at.elapsed() < Duration::from_millis(350);
            if typing {
                continue;
            }
            if let Some(session) = wake_session {
                if let Some((run_id, label)) = jobs_feed.claim_wake_run(session) {
                    return Ok(LiveReplOutcome::FollowWake { run_id, label });
                }
            }
            let cumulative_changed = jobs_feed
                .cumulative()
                .is_some_and(|totals| live.footer.update_cumulative_tokens(totals));
            if live.set_jobs(jobs_feed.current()) || cumulative_changed {
                synchronized_terminal_update(CursorAfterUpdate::Preserve, || live.redraw())?;
            } else {
                live.tick_job_strip()?;
            }
            continue;
        }
        last_key_at = Instant::now();
        match live.editor.handle_event(event::read()?, paths, false)? {
            LiveEditorAction::None => {}
            LiveEditorAction::Redraw => {
                synchronized_terminal_update(CursorAfterUpdate::Preserve, || live.redraw())?
            }
            LiveEditorAction::ClearScreen => {
                synchronized_terminal_update(CursorAfterUpdate::Preserve, || live.clear_screen())?
            }
            LiveEditorAction::EmptySubmit => {
                synchronized_terminal_update(CursorAfterUpdate::Preserve, || {
                    live.commit_empty_submission()
                })?
            }
            LiveEditorAction::Submit(submission) => {
                let mode = live.mode();
                synchronized_terminal_update(CursorAfterUpdate::Hidden, || {
                    live.commit_submission(&submission)
                })?;
                raw.keep_cursor_hidden();
                return Ok(LiveReplOutcome::Submit(
                    mode,
                    submission.content,
                    submission.images,
                ));
            }
            // Ctrl+C rung 3: the draft was empty and no reply is running, but
            // this session still has background work — stop that before the
            // press is allowed to mean "quit". `live.jobs` holds only running
            // jobs of this session, refreshed on every idle tick. Ctrl+D
            // (`Exit`) always quits outright.
            LiveEditorAction::Interrupt if !live.jobs.is_empty() => {
                return Ok(LiveReplOutcome::StopJobs);
            }
            LiveEditorAction::Interrupt | LiveEditorAction::Exit => {
                synchronized_terminal_update(CursorAfterUpdate::Hidden, || live.suspend())?;
                return Ok(LiveReplOutcome::Exit);
            }
        }
    }
}

/// Attach to a daemon-initiated wake turn and render it live: streaming
/// content, reasoning, and tool activity, exactly like a user-started turn.
/// ESC detaches (the turn keeps running; the DB report is suppressed because
/// the turn was already rendered here). Typed submissions queue into the
/// wake turn as follow-ups.
async fn follow_wake_run(
    paths: &MiyuPaths,
    live: &mut LiveReplTail,
    run_id: &str,
    label: &str,
    jobs_feed: &JobsFeed,
    jobs_shared: &std::sync::Arc<SharedJobsFeed>,
) -> Result<()> {
    let config = AppConfig::load_or_default(paths)?;
    let mut stream = ipc::connect(&paths.ipc_socket()).await?;
    ipc::send(
        &mut stream,
        &IpcRequest::new(IpcCommand::FollowRun {
            run_id: run_id.to_string(),
        }),
    )
    .await?;
    let mut turn_id: Option<String> = match ipc::receive::<IpcFrame>(&mut stream).await? {
        Some(IpcFrame::Accepted { turn_id, .. }) => turn_id,
        // Run already finished — the DB report path will print it instead.
        _ => return Ok(()),
    };

    let mut renderer = render::StreamRenderer::new(
        render::ReasoningDisplayMode::from_config(&config.display.reasoning),
        render::ToolCallDisplayMode::from_config(&config.display.tool_calls),
        false,
        config.display.readable_tool_names,
        config.display.command_output_lines,
    );
    renderer.use_external_cursor_control();
    renderer.use_buffered_output();
    live.external_output_active = false;
    // Print the header straight into the scrollback (not the live frame):
    // it must survive the streaming render that follows.
    {
        live.suspend()?;
        let mut stdout = io::stdout();
        let header = if label.is_empty() {
            crate::i18n::text("⚙ background task finished", "⚙ 后台任务完成").to_string()
        } else {
            format!("⚙ {label}")
        };
        queue!(stdout, Print(format!("\x1b[2m{header}\x1b[0m\r\n\r\n")))?;
        stdout.flush()?;
        live.output_cursor = cursor_position_or(live.output_cursor);
        let output_cursor = live.output_cursor;
        live.resume_at(output_cursor)?;
    }
    renderer.start_waiting()?;
    live.apply_renderer_frame(&mut renderer)?;
    let mut raw = LiveRawMode::start()?;

    let mut spinner_tick = tokio::time::interval(Duration::from_millis(33));
    spinner_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    spinner_tick.tick().await;
    let mut input_tick = tokio::time::interval(Duration::from_millis(16));
    input_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    input_tick.tick().await;
    let mut follow_strip_tick: u32 = 0;

    'outer: loop {
        let recv = ipc::receive::<IpcFrame>(&mut stream);
        tokio::pin!(recv);
        let frame = loop {
            tokio::select! {
                biased;
                _ = input_tick.tick() => {
                    if !event::poll(Duration::ZERO)? {
                        continue;
                    }
                    match live.editor.handle_event(event::read()?, paths, true)? {
                        LiveEditorAction::None => {}
                        LiveEditorAction::Redraw if !live.external_output_active => {
                            synchronized_terminal_update(CursorAfterUpdate::Preserve, || {
                                live.redraw()
                            })?
                        }
                        LiveEditorAction::Redraw | LiveEditorAction::ClearScreen => {}
                        LiveEditorAction::EmptySubmit => {}
                        LiveEditorAction::Submit(submission) => {
                            let Some(target_turn) = turn_id.as_deref() else {
                                continue;
                            };
                            if let Ok(prompt) = persist_remote_queued_submission(
                                paths,
                                run_id,
                                target_turn,
                                &submission,
                            )
                            .await
                            {
                                live.editor.record_history(&submission.content);
                                synchronized_terminal_update(
                                    CursorAfterUpdate::Preserve,
                                    || live.enqueue(prompt),
                                )?;
                            }
                        }
                        LiveEditorAction::Interrupt | LiveEditorAction::Exit => {
                            // Detach only: the wake turn keeps running.
                            break 'outer;
                        }
                    }
                }
                frame = &mut recv => break frame?,
                _ = spinner_tick.tick() => {
                    // SpinnerTick 经 live 路径冲刷 chunk 缓冲，流式输出靠它。
                    handle_live_agent_event(live, &mut renderer, AgentEvent::SpinnerTick)?;
                    // 状态条是 live tail 的一部分，附着期间同样要持续刷新。
                    follow_strip_tick = follow_strip_tick.wrapping_add(1);
                    if follow_strip_tick % 8 == 0 && !live.external_output_active {
                        if live.set_jobs(jobs_feed.current()) {
                            synchronized_terminal_update(CursorAfterUpdate::Preserve, || {
                                live.redraw()
                            })?;
                        } else {
                            live.tick_job_strip()?;
                        }
                    }
                }
            }
        };
        let Some(IpcFrame::Event { kind, data, .. }) = frame else {
            break;
        };
        match kind.as_str() {
            "turn.started" => {
                turn_id = Some(ipc_text(&data, "turn_id").to_string());
            }
            "assistant.delta" => handle_live_agent_event(
                live,
                &mut renderer,
                AgentEvent::Chunk(ChatStreamChunk {
                    kind: crate::llm::ChatStreamKind::Content,
                    text: ipc_text(&data, "delta").to_string(),
                }),
            )?,
            "reasoning.delta" => handle_live_agent_event(
                live,
                &mut renderer,
                AgentEvent::Chunk(ChatStreamChunk {
                    kind: crate::llm::ChatStreamKind::Reasoning,
                    text: ipc_text(&data, "delta").to_string(),
                }),
            )?,
            "reasoning.start" => handle_live_agent_event(
                live,
                &mut renderer,
                AgentEvent::ReasoningStart {
                    received_at: Instant::now(),
                },
            )?,
            "reasoning.reset" => handle_live_agent_event(
                live,
                &mut renderer,
                AgentEvent::ReasoningReset {
                    received_at: Instant::now(),
                },
            )?,
            "reasoning.part_start" => handle_live_agent_event(
                live,
                &mut renderer,
                AgentEvent::ReasoningPartStart {
                    received_at: Instant::now(),
                },
            )?,
            "reasoning.part_end" => handle_live_agent_event(
                live,
                &mut renderer,
                AgentEvent::ReasoningPartEnd {
                    received_at: Instant::now(),
                },
            )?,
            "reasoning.title" => handle_live_agent_event(
                live,
                &mut renderer,
                AgentEvent::ReasoningTitle(ipc_text(&data, "title").to_string()),
            )?,
            "tool.preparing" => handle_live_agent_event(
                live,
                &mut renderer,
                AgentEvent::ToolPreparing {
                    name: ipc_text(&data, "name").to_string(),
                },
            )?,
            "tool.started" => handle_live_agent_event(
                live,
                &mut renderer,
                AgentEvent::ToolCall {
                    call_id: ipc_text(&data, "tool_id").to_string(),
                    name: ipc_text(&data, "name").to_string(),
                    arguments: ipc_text(&data, "arguments").to_string(),
                },
            )?,
            "tool.progress" => handle_live_agent_event(
                live,
                &mut renderer,
                AgentEvent::ToolProgress {
                    call_id: ipc_text(&data, "tool_id").to_string(),
                    name: ipc_text(&data, "name").to_string(),
                    message: ipc_text(&data, "message").to_string(),
                },
            )?,
            "tool.output" => handle_live_agent_event(
                live,
                &mut renderer,
                AgentEvent::CommandOutput {
                    call_id: ipc_text(&data, "tool_id").to_string(),
                    name: ipc_text(&data, "name").to_string(),
                    stream: if ipc_text(&data, "stream") == "stderr" {
                        tools::CommandOutputStream::Stderr
                    } else {
                        tools::CommandOutputStream::Stdout
                    },
                    chunk: ipc_text(&data, "output").as_bytes().to_vec(),
                },
            )?,
            "tool.finished" => handle_live_agent_event(
                live,
                &mut renderer,
                AgentEvent::ToolResult {
                    call_id: ipc_text(&data, "tool_id").to_string(),
                    name: ipc_text(&data, "name").to_string(),
                    ok: data
                        .get("ok")
                        .and_then(serde_json::Value::as_bool)
                        .unwrap_or(false),
                    output: ipc_text(&data, "output").to_string(),
                },
            )?,
            "queue.consumed" => {
                let prompt_ids: Vec<String> = data
                    .get("prompt_ids")
                    .and_then(serde_json::Value::as_array)
                    .map(|values| {
                        values
                            .iter()
                            .filter_map(|value| value.as_str().map(str::to_string))
                            .collect()
                    })
                    .unwrap_or_default();
                let consumed_mode = match ipc_text(&data, "mode") {
                    "chat" => AgentMode::Chat,
                    _ => AgentMode::Normal,
                };
                renderer.prepare_for_external_output()?;
                live.apply_renderer_frame(&mut renderer)?;
                synchronized_terminal_update(CursorAfterUpdate::Preserve, || {
                    live.suspend()?;
                    live.consume_queued(&prompt_ids, consumed_mode)
                })?;
            }
            "generation.superseded" => handle_live_agent_event(
                live,
                &mut renderer,
                AgentEvent::ReasoningReset {
                    received_at: Instant::now(),
                },
            )?,
            "run.completed" | "run.failed" | "run.cancelled" => {
                break;
            }
            _ => {}
        }
    }

    // Flush chunks still buffered when the terminal frame arrived — the
    // final content burst lands right before run.completed.
    live.flush_pending_chunks(&mut renderer)?;
    renderer.finish()?;
    live.apply_renderer_frame(&mut renderer)?;
    raw.handoff();
    live.raw_mode_handoff = true;
    // Suppress the duplicate DB report for a turn that was rendered live.
    if let Some(turn_id) = turn_id {
        jobs_shared
            .rendered_turns
            .lock()
            .unwrap()
            .insert(turn_id);
    }
    Ok(())
}

fn handle_live_agent_event(
    live: &mut LiveReplTail,
    renderer: &mut render::StreamRenderer,
    event: AgentEvent,
) -> Result<()> {
    let event = match event {
        AgentEvent::Chunk(chunk) => {
            live.queue_stream_chunk(chunk);
            return Ok(());
        }
        event => event,
    };
    if live.external_output_active && matches!(&event, AgentEvent::SpinnerTick) {
        return Ok(());
    }
    if matches!(&event, AgentEvent::SpinnerTick) {
        return live.tick_spinner(renderer);
    }
    match event {
        AgentEvent::PrepareForExternalOutput { ready } => {
            let result = (|| {
                live.flush_pending_chunks(renderer)?;
                renderer.prepare_for_external_output()?;
                live.apply_renderer_frame(renderer)?;
                synchronized_terminal_update(CursorAfterUpdate::Hidden, || live.suspend())?;
                live.external_output_active = true;
                Ok(())
            })();
            if result.is_ok() {
                let _ = ready.send(true);
            }
            result
        }
        AgentEvent::QueuedPromptsConsumed {
            prompt_ids, mode, ..
        } => {
            live.flush_pending_chunks(renderer)?;
            renderer.prepare_for_external_output()?;
            live.apply_renderer_frame(renderer)?;
            synchronized_terminal_update(CursorAfterUpdate::Preserve, || {
                live.suspend()?;
                live.consume_queued(&prompt_ids, mode)
            })
        }
        event => {
            let finishes_external_output =
                live.external_output_active && matches!(&event, AgentEvent::ToolResult { .. });
            if live.external_output_active && !finishes_external_output {
                handle_agent_event(renderer, event)?;
                return live.apply_renderer_frame(renderer);
            }
            let question = matches!(&event, AgentEvent::AskQuestion { .. });
            if question {
                live.flush_pending_chunks(renderer)?;
                renderer.prepare_for_external_output()?;
                live.apply_renderer_frame(renderer)?;
                synchronized_terminal_update(CursorAfterUpdate::Hidden, || live.suspend())?;
                handle_agent_event(renderer, event)?;
                // 问题面板只关闭 raw mode 与括号粘贴，键盘增强仍由外层 LiveRawMode 持有
                enable_live_raw_mode()?;
                execute!(io::stdout(), EnableBracketedPaste)?;
                synchronized_terminal_update(CursorAfterUpdate::Shown, || live.resume())?;
                return live.apply_renderer_frame(renderer);
            }
            live.flush_pending_chunks(renderer)?;
            handle_agent_event(renderer, event)?;
            live.apply_renderer_frame(renderer)?;
            if finishes_external_output {
                live.external_output_active = false;
                synchronized_terminal_update(CursorAfterUpdate::Shown, || live.resume())?;
            }
            Ok(())
        }
    }
}

async fn run_live_agent_turn(
    live: &mut LiveReplTail,
    paths: &MiyuPaths,
    state: &StateStore,
    agent: &mut Agent,
    input: LiveAgentInput<'_>,
    control: &AgentTurnControl,
    renderer: &mut render::StreamRenderer,
) -> Result<Option<crate::llm::ChatResult>> {
    renderer.use_external_cursor_control();
    renderer.use_buffered_output();
    let mut raw = if std::mem::take(&mut live.raw_mode_handoff) {
        LiveRawMode::adopt()
    } else {
        LiveRawMode::start()?
    };
    live.external_output_active = false;
    if !live.rendered {
        live.resume_at(live.output_cursor)?;
    }
    renderer.start_waiting()?;
    live.apply_renderer_frame(renderer)?;

    let result = {
        let live_cell = std::cell::RefCell::new(&mut *live);
        let renderer_cell = std::cell::RefCell::new(&mut *renderer);
        let chat = agent.chat_stream_with_control(input.content, input.images, control, |event| {
            handle_live_agent_event(
                &mut live_cell.borrow_mut(),
                &mut renderer_cell.borrow_mut(),
                event,
            )
        });
        tokio::pin!(chat);
        let mut input_tick = tokio::time::interval(Duration::from_millis(16));
        input_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        input_tick.tick().await;
        loop {
            tokio::select! {
                biased;
                _ = input_tick.tick() => {
                    if !event::poll(Duration::ZERO)? {
                        continue;
                    }
                    let event = event::read()?;
                    let mut live = live_cell.borrow_mut();
                    if matches!(
                        &event,
                        Event::Key(KeyEvent {
                            code: KeyCode::Enter,
                            kind,
                            ..
                        }) if *kind != KeyEventKind::Release
                    ) && live.editor.input.trim_start().starts_with('/')
                    {
                        if live.external_output_active {
                            continue;
                        }
                        let mut renderer = renderer_cell.borrow_mut();
                        live.flush_pending_chunks(&mut renderer)?;
                        renderer.write_system_message(t(
                            "REPL commands are available after the current reply finishes",
                            "当前回复结束后才能执行 REPL 命令",
                        ))?;
                        // write_system_message tears down the wait spinner;
                        // restart it so progress keeps rendering.
                        renderer.start_waiting()?;
                        live.apply_renderer_frame(&mut renderer)?;
                        continue;
                    }
                    let mode_before = live.mode();
                    match live.editor.handle_event(event, paths, true)? {
                        LiveEditorAction::None => {}
                        LiveEditorAction::Redraw if !live.external_output_active => {
                            synchronized_terminal_update(CursorAfterUpdate::Preserve, || {
                                live.redraw()
                            })?
                        }
                        LiveEditorAction::ClearScreen if !live.external_output_active => {
                            synchronized_terminal_update(CursorAfterUpdate::Preserve, || {
                                live.clear_screen()
                            })?
                        }
                        LiveEditorAction::Redraw | LiveEditorAction::ClearScreen => {}
                        LiveEditorAction::EmptySubmit => {}
                        LiveEditorAction::Submit(submission) => {
                            let prompt = persist_queued_submission(state, &submission)?;
                            live.editor.record_history(&submission.content);
                            if live.external_output_active {
                                live.append_queued(prompt);
                            } else {
                                synchronized_terminal_update(CursorAfterUpdate::Preserve, || {
                                    live.enqueue(prompt)
                                })?;
                            }
                        }
                        LiveEditorAction::Interrupt | LiveEditorAction::Exit => break Ok(None),
                    }
                    if live.mode() != mode_before {
                        control.set_mode(live.mode());
                    }
                },
                result = &mut chat => break result.map(Some),
            }
        }
    };

    if matches!(&result, Ok(None)) {
        live.discard_pending_chunks();
    }
    live.external_output_active = false;
    live.flush_pending_chunks(renderer)?;
    renderer.finish()?;
    live.apply_renderer_frame(renderer)?;
    raw.handoff();
    live.raw_mode_handoff = true;
    result
}

fn read_repl_input(
    paths: &MiyuPaths,
    mut mode: AgentMode,
    prefill: Option<String>,
    history: &[String],
    footer: &ReplFooterStatus,
    show_shortcut_hint: bool,
) -> Result<
    Option<(
        AgentMode,
        String,
        Vec<Option<crate::clipboard::PastedImage>>,
    )>,
> {
    let mut stdout = io::stdout();
    let mut input = strip_terminal_control_sequences(&prefill.unwrap_or_default());
    let mut cursor = input.chars().count();
    let mut history_index = history.len();
    let mut history_clean_index: Option<usize> = None;
    let plain_prefix = "  ";
    let cursor_col = cursor_col_or(0);
    if cursor_col != 0 {
        writeln!(stdout)?;
        stdout.flush()?;
    }
    terminal::enable_raw_mode()?;
    execute!(stdout, EnableBracketedPaste)?;
    let mut keyboard_enhancement = KeyboardEnhancementState::enable(&mut stdout);
    let mut input_row = cursor_row_or(0);
    let mut rendered_rows = 0u16;
    let mut is_pasted = false;
    let mut pasted_images: Vec<Option<crate::clipboard::PastedImage>> = Vec::new();
    let mut pasted_texts: Vec<Option<PastedText>> = Vec::new();
    // 1. 局部退出时统一恢复终端协议
    // 2. 避免多处 return 漏 Pop 键盘增强
    let restore_terminal = |stdout: &mut io::Stdout,
                            keyboard_enhancement: &mut KeyboardEnhancementState|
     -> Result<()> {
        execute!(stdout, DisableBracketedPaste)?;
        keyboard_enhancement.disable(stdout);
        terminal::disable_raw_mode()?;
        Ok(())
    };
    let render_repl_input = |stdout: &mut io::Stdout,
                             input_row: &mut u16,
                             rendered_rows: &mut u16,
                             mode: AgentMode,
                             input: &str,
                             cursor: usize,
                             is_pasted: bool| {
        render_repl_input_with_footer(
            stdout,
            input_row,
            rendered_rows,
            mode,
            input,
            cursor,
            is_pasted,
            footer,
            show_shortcut_hint,
        )
    };
    render_repl_input(
        &mut stdout,
        &mut input_row,
        &mut rendered_rows,
        mode,
        &input,
        cursor,
        is_pasted,
    )?;
    loop {
        match event::read()? {
            Event::Paste(text) => {
                insert_pasted_text_at_cursor(&mut input, &mut cursor, text, &mut pasted_texts);
                history_clean_index = None;
                is_pasted = true;
                render_repl_input(
                    &mut stdout,
                    &mut input_row,
                    &mut rendered_rows,
                    mode,
                    &input,
                    cursor,
                    is_pasted,
                )?;
            }
            Event::Key(KeyEvent {
                code, modifiers, ..
            }) => match code {
                KeyCode::Tab => {
                    if input.starts_with('/') {
                        if let Some(completed) = complete_repl_command(&input) {
                            input = completed.to_string();
                            cursor = input.chars().count();
                            history_clean_index = None;
                        }
                    } else {
                        mode = match mode {
                            AgentMode::Normal => AgentMode::Chat,
                            AgentMode::Chat => AgentMode::Normal,
                        };
                    }
                    is_pasted = false;
                    render_repl_input(
                        &mut stdout,
                        &mut input_row,
                        &mut rendered_rows,
                        mode,
                        &input,
                        cursor,
                        is_pasted,
                    )?;
                }
                KeyCode::Esc => {
                    input.clear();
                    cursor = 0;
                    history_clean_index = None;
                    is_pasted = false;
                    pasted_images.clear();
                    pasted_texts.clear();
                    render_repl_input(
                        &mut stdout,
                        &mut input_row,
                        &mut rendered_rows,
                        mode,
                        &input,
                        cursor,
                        is_pasted,
                    )?;
                }
                KeyCode::Left => {
                    if let Some((start, _)) = placeholder_at_cursor(&input, cursor) {
                        cursor = start;
                    } else {
                        cursor = cursor.saturating_sub(1);
                    }
                    render_repl_input(
                        &mut stdout,
                        &mut input_row,
                        &mut rendered_rows,
                        mode,
                        &input,
                        cursor,
                        is_pasted,
                    )?;
                }
                KeyCode::Right => {
                    if let Some((_, end)) = placeholder_at_cursor(&input, cursor) {
                        cursor = end;
                    } else {
                        cursor = (cursor + 1).min(input.chars().count());
                    }
                    render_repl_input(
                        &mut stdout,
                        &mut input_row,
                        &mut rendered_rows,
                        mode,
                        &input,
                        cursor,
                        is_pasted,
                    )?;
                }
                KeyCode::Home => {
                    cursor = 0;
                    render_repl_input(
                        &mut stdout,
                        &mut input_row,
                        &mut rendered_rows,
                        mode,
                        &input,
                        cursor,
                        is_pasted,
                    )?;
                }
                KeyCode::End => {
                    cursor = input.chars().count();
                    render_repl_input(
                        &mut stdout,
                        &mut input_row,
                        &mut rendered_rows,
                        mode,
                        &input,
                        cursor,
                        is_pasted,
                    )?;
                }
                KeyCode::Up => {
                    if !history.is_empty()
                        && repl_should_browse_history(&input, history, history_clean_index)
                    {
                        if input.is_empty() {
                            history_index = history.len();
                        }
                        history_index = history_index.saturating_sub(1);
                        input = history.get(history_index).cloned().unwrap_or_default();
                        cursor = input.chars().count();
                        history_clean_index = Some(history_index);
                        is_pasted = false;
                        pasted_images.clear();
                        pasted_texts.clear();
                    } else {
                        cursor = repl_move_cursor_vertical(&plain_prefix, &input, cursor, -1);
                    }
                    render_repl_input(
                        &mut stdout,
                        &mut input_row,
                        &mut rendered_rows,
                        mode,
                        &input,
                        cursor,
                        is_pasted,
                    )?;
                }
                KeyCode::Down => {
                    if repl_history_is_clean(&input, history, history_clean_index) {
                        if history_index + 1 < history.len() {
                            history_index += 1;
                            input = history.get(history_index).cloned().unwrap_or_default();
                            cursor = input.chars().count();
                            history_clean_index = Some(history_index);
                        } else {
                            history_index = history.len();
                            input.clear();
                            cursor = 0;
                            history_clean_index = None;
                        }
                        is_pasted = false;
                        pasted_images.clear();
                        pasted_texts.clear();
                    } else {
                        cursor = repl_move_cursor_vertical(&plain_prefix, &input, cursor, 1);
                    }
                    render_repl_input(
                        &mut stdout,
                        &mut input_row,
                        &mut rendered_rows,
                        mode,
                        &input,
                        cursor,
                        is_pasted,
                    )?;
                }
                KeyCode::Enter if modifiers.contains(KeyModifiers::SHIFT) => {
                    // Shift+Enter 与 Ctrl+J 相同：在光标处插入换行，不提交
                    insert_newline_at_cursor(&mut input, &mut cursor);
                    history_clean_index = None;
                    is_pasted = false;
                    render_repl_input(
                        &mut stdout,
                        &mut input_row,
                        &mut rendered_rows,
                        mode,
                        &input,
                        cursor,
                        is_pasted,
                    )?;
                }
                KeyCode::Enter => {
                    let submitted_echo = strip_terminal_control_sequences(&input);
                    input = expand_pasted_text_placeholders(&submitted_echo, &pasted_texts);
                    replace_repl_input_with_user_echo(
                        &mut stdout,
                        input_row,
                        rendered_rows,
                        mode,
                        &submitted_echo,
                    )?;
                    restore_terminal(&mut stdout, &mut keyboard_enhancement)?;
                    return Ok(Some((mode, input, pasted_images)));
                }
                KeyCode::Char('j') if modifiers.contains(KeyModifiers::CONTROL) => {
                    insert_newline_at_cursor(&mut input, &mut cursor);
                    history_clean_index = None;
                    is_pasted = false;
                    render_repl_input(
                        &mut stdout,
                        &mut input_row,
                        &mut rendered_rows,
                        mode,
                        &input,
                        cursor,
                        is_pasted,
                    )?;
                }
                KeyCode::Char('c')
                    if modifiers.contains(KeyModifiers::CONTROL)
                        && !modifiers.contains(KeyModifiers::SHIFT) =>
                {
                    if !input.is_empty() {
                        input.clear();
                        cursor = 0;
                        history_clean_index = None;
                        is_pasted = false;
                        pasted_images.clear();
                        pasted_texts.clear();
                        render_repl_input(
                            &mut stdout,
                            &mut input_row,
                            &mut rendered_rows,
                            mode,
                            &input,
                            cursor,
                            is_pasted,
                        )?;
                        continue;
                    }
                    move_after_repl_input(&mut stdout, input_row, rendered_rows)?;
                    restore_terminal(&mut stdout, &mut keyboard_enhancement)?;
                    return Ok(None);
                }
                KeyCode::Char('d')
                    if modifiers.contains(KeyModifiers::CONTROL) && input.is_empty() =>
                {
                    move_after_repl_input(&mut stdout, input_row, rendered_rows)?;
                    restore_terminal(&mut stdout, &mut keyboard_enhancement)?;
                    return Ok(None);
                }
                KeyCode::Char('l') if modifiers.contains(KeyModifiers::CONTROL) => {
                    queue!(stdout, Clear(ClearType::All), MoveTo(0, 0))?;
                    stdout.flush()?;
                    input_row = 0;
                    rendered_rows = 0;
                    render_repl_input(
                        &mut stdout,
                        &mut input_row,
                        &mut rendered_rows,
                        mode,
                        &input,
                        cursor,
                        is_pasted,
                    )?;
                }
                KeyCode::Char('w') if modifiers.contains(KeyModifiers::CONTROL) => {
                    if let Some((start, end)) = placeholder_before_or_at_cursor(&input, cursor) {
                        clear_placeholder_payload(
                            &input,
                            start,
                            end,
                            &mut pasted_images,
                            &mut pasted_texts,
                        );
                        remove_range_chars(&mut input, start, end);
                        cursor = start;
                    } else {
                        remove_word_before_cursor(&mut input, &mut cursor);
                    }
                    history_clean_index = None;
                    is_pasted = false;
                    render_repl_input(
                        &mut stdout,
                        &mut input_row,
                        &mut rendered_rows,
                        mode,
                        &input,
                        cursor,
                        is_pasted,
                    )?;
                }
                KeyCode::Backspace => {
                    if cursor > 0 {
                        if let Some((start, end)) = placeholder_before_or_at_cursor(&input, cursor)
                        {
                            clear_placeholder_payload(
                                &input,
                                start,
                                end,
                                &mut pasted_images,
                                &mut pasted_texts,
                            );
                            remove_range_chars(&mut input, start, end);
                            cursor = start;
                        } else {
                            remove_char_before_cursor(&mut input, &mut cursor);
                        }
                        history_clean_index = None;
                    }
                    is_pasted = false;
                    render_repl_input(
                        &mut stdout,
                        &mut input_row,
                        &mut rendered_rows,
                        mode,
                        &input,
                        cursor,
                        is_pasted,
                    )?;
                }
                KeyCode::Delete => {
                    if let Some((start, end)) = placeholder_after_or_at_cursor(&input, cursor) {
                        clear_placeholder_payload(
                            &input,
                            start,
                            end,
                            &mut pasted_images,
                            &mut pasted_texts,
                        );
                        remove_range_chars(&mut input, start, end);
                    } else {
                        remove_char_at_cursor(&mut input, cursor);
                    }
                    history_clean_index = None;
                    is_pasted = false;
                    render_repl_input(
                        &mut stdout,
                        &mut input_row,
                        &mut rendered_rows,
                        mode,
                        &input,
                        cursor,
                        is_pasted,
                    )?;
                }
                KeyCode::Char('c' | 'C')
                    if modifiers.contains(KeyModifiers::CONTROL)
                        && modifiers.contains(KeyModifiers::SHIFT) =>
                {
                    if let Some(selected) =
                        placeholder_text_near_cursor(&input, cursor, &pasted_texts)
                    {
                        let _ = crate::clipboard::write_clipboard_text(&selected)?;
                    }
                }
                KeyCode::Char('v') if modifiers.contains(KeyModifiers::CONTROL) => {
                    match crate::clipboard::read_clipboard() {
                        Ok(crate::clipboard::ClipboardContent::Image(img)) => {
                            let index = pasted_images.len() + 1;
                            let placeholder = match img.write_temp_file(&paths.cache_dir, index) {
                                Ok(path) => {
                                    let filename = path
                                        .file_name()
                                        .and_then(|n| n.to_str())
                                        .unwrap_or("image");
                                    format!("[Image {}: {}]", index, filename)
                                }
                                Err(_) => format!("[Image {}]", index),
                            };
                            insert_str_at_cursor(&mut input, &mut cursor, &placeholder);
                            history_clean_index = None;
                            pasted_images.push(Some(crate::clipboard::PastedImage::Binary(img)));
                            is_pasted = false;
                            render_repl_input(
                                &mut stdout,
                                &mut input_row,
                                &mut rendered_rows,
                                mode,
                                &input,
                                cursor,
                                is_pasted,
                            )?;
                        }
                        Ok(crate::clipboard::ClipboardContent::ImagePath(path)) => {
                            let index = pasted_images.len() + 1;
                            let filename = std::path::Path::new(&path)
                                .file_name()
                                .and_then(|n| n.to_str())
                                .unwrap_or("image");
                            let placeholder = format!("[Image {}: {}]", index, filename);
                            insert_str_at_cursor(&mut input, &mut cursor, &placeholder);
                            history_clean_index = None;
                            pasted_images.push(Some(crate::clipboard::PastedImage::Path(path)));
                            is_pasted = false;
                            render_repl_input(
                                &mut stdout,
                                &mut input_row,
                                &mut rendered_rows,
                                mode,
                                &input,
                                cursor,
                                is_pasted,
                            )?;
                        }
                        Ok(crate::clipboard::ClipboardContent::TextPath(path)) => {
                            insert_str_at_cursor(&mut input, &mut cursor, &path);
                            history_clean_index = None;
                            is_pasted = false;
                            render_repl_input(
                                &mut stdout,
                                &mut input_row,
                                &mut rendered_rows,
                                mode,
                                &input,
                                cursor,
                                is_pasted,
                            )?;
                        }
                        _ => {
                            if let Ok(Some(text)) = crate::clipboard::read_clipboard_text() {
                                insert_pasted_text_at_cursor(
                                    &mut input,
                                    &mut cursor,
                                    text,
                                    &mut pasted_texts,
                                );
                                history_clean_index = None;
                                is_pasted = true;
                                render_repl_input(
                                    &mut stdout,
                                    &mut input_row,
                                    &mut rendered_rows,
                                    mode,
                                    &input,
                                    cursor,
                                    is_pasted,
                                )?;
                            }
                        }
                    }
                }
                KeyCode::Char(ch) if !modifiers.contains(KeyModifiers::CONTROL) => {
                    if !is_disallowed_control_char(ch) {
                        if let Some((_, end)) = placeholder_at_cursor(&input, cursor) {
                            cursor = end;
                        }
                        insert_char_at_cursor(&mut input, &mut cursor, ch);
                        history_clean_index = None;
                    }
                    is_pasted = false;
                    render_repl_input(
                        &mut stdout,
                        &mut input_row,
                        &mut rendered_rows,
                        mode,
                        &input,
                        cursor,
                        is_pasted,
                    )?;
                }
                _ => {}
            },
            _ => {}
        }
    }
}

fn render_repl_input_with_footer(
    stdout: &mut io::Stdout,
    input_row: &mut u16,
    rendered_rows: &mut u16,
    mode: AgentMode,
    input: &str,
    cursor: usize,
    is_pasted: bool,
    footer: &ReplFooterStatus,
    show_shortcut_hint: bool,
) -> Result<()> {
    let suggestions = repl_command_suggestions(input);
    let lines = repl_input_lines(input);
    let prompt_prefix = input_prompt_bar(mode);
    let plain_prefix = "  ";
    let cols = terminal_cols();
    let display_lines = repl_visible_input_lines(
        &plain_prefix,
        &lines,
        REPL_MAX_VISIBLE_INPUT_ROWS,
        is_pasted,
    );
    let display_rows = repl_wrapped_input_rows_for_cols(&plain_prefix, &display_lines, cols);
    let display_rows: Vec<String> = display_rows
        .iter()
        .map(|line| colorize_repl_placeholders(line))
        .collect();
    let input_rows = display_rows.len().max(1).min(u16::MAX as usize) as u16;
    let show_hint = show_shortcut_hint && suggestions.is_empty();
    let current_rows = input_rows.saturating_add(if show_hint { 4 } else { 3 });
    let rows_to_clear = (*rendered_rows).max(current_rows).max(1);
    ensure_repl_space(stdout, input_row, rows_to_clear)?;
    for row_offset in 0..rows_to_clear {
        queue!(
            stdout,
            MoveTo(0, (*input_row).saturating_add(row_offset)),
            Clear(ClearType::CurrentLine)
        )?;
    }
    let mut row_offset = 0u16;
    queue!(stdout, MoveTo(0, *input_row), Print(&prompt_prefix))?;
    row_offset = row_offset.saturating_add(1);
    for line in &display_rows {
        let row = (*input_row).saturating_add(row_offset);
        queue!(stdout, MoveTo(0, row))?;
        queue!(stdout, Print(&prompt_prefix), Print(line))?;
        row_offset = row_offset.saturating_add(1);
    }
    queue!(
        stdout,
        MoveTo(0, (*input_row).saturating_add(row_offset)),
        Print(&prompt_prefix)
    )?;
    row_offset = row_offset.saturating_add(1);
    if !suggestions.is_empty() {
        let suggestion_width = cols.saturating_sub(visible_width(&prompt_prefix)).max(1);
        queue!(
            stdout,
            MoveTo(0, (*input_row).saturating_add(row_offset)),
            Print(&prompt_prefix),
            Print(format!(
                "\x1b[2m{}\x1b[0m",
                repl_command_suggestions_line(&suggestions, suggestion_width)
            ))
        )?;
    } else {
        queue!(
            stdout,
            MoveTo(0, (*input_row).saturating_add(row_offset)),
            Print(repl_footer_line(mode, footer, cols))
        )?;
        if show_hint {
            row_offset = row_offset.saturating_add(1);
            queue!(
                stdout,
                MoveTo(0, (*input_row).saturating_add(row_offset)),
                Print(repl_shortcut_hint_line(mode, cols))
            )?;
        }
    }
    let (cursor_col, cursor_row_offset) = if display_lines.len() == lines.len() {
        repl_cursor_position(&plain_prefix, input, cursor)
    } else {
        let last_line = display_lines.last().map(String::as_str).unwrap_or_default();
        let (col, _) = repl_cursor_position_for_line_for_cols(
            &plain_prefix,
            last_line,
            last_line.chars().count(),
            terminal_cols(),
        );
        (
            col,
            repl_prompt_rows(&plain_prefix, &display_lines).saturating_sub(1),
        )
    };
    queue!(
        stdout,
        MoveTo(
            cursor_col,
            (*input_row)
                .saturating_add(1)
                .saturating_add(cursor_row_offset)
        )
    )?;
    stdout.flush()?;
    *rendered_rows = current_rows;
    Ok(())
}

fn repl_visible_input_lines(
    prefix: &str,
    lines: &[String],
    max_rows: u16,
    is_pasted: bool,
) -> Vec<String> {
    let total_rows = repl_prompt_rows(prefix, lines);
    if total_rows <= max_rows || lines.len() <= 2 || !is_pasted {
        return lines.to_vec();
    }

    let omitted_lines = lines.len().saturating_sub(2);
    let omitted = if is_zh() {
        format!("... 已隐藏 {omitted_lines} 行粘贴内容 ...")
    } else {
        format!("... {omitted_lines} pasted lines hidden ...")
    };
    vec![lines[0].clone(), omitted, lines[lines.len() - 1].clone()]
}

fn ensure_repl_space(stdout: &mut io::Stdout, input_row: &mut u16, needed_rows: u16) -> Result<()> {
    let (_, term_rows) = terminal::size().unwrap_or((80, 24));
    let term_rows = term_rows.max(1);
    if (*input_row).saturating_add(needed_rows) < term_rows {
        return Ok(());
    }
    let overflow = (*input_row)
        .saturating_add(needed_rows)
        .saturating_sub(term_rows.saturating_sub(1));
    queue!(stdout, MoveTo(0, term_rows.saturating_sub(1)))?;
    for _ in 0..overflow {
        queue!(stdout, Print("\n"))?;
    }
    *input_row = (*input_row).saturating_sub(overflow);
    Ok(())
}

fn move_after_repl_input(
    stdout: &mut io::Stdout,
    input_row: u16,
    rendered_rows: u16,
) -> Result<()> {
    queue!(
        stdout,
        MoveTo(0, input_row.saturating_add(rendered_rows.max(1)))
    )?;
    stdout.flush()?;
    Ok(())
}

fn replace_repl_input_with_user_echo(
    stdout: &mut io::Stdout,
    input_row: u16,
    rendered_rows: u16,
    mode: AgentMode,
    input: &str,
) -> Result<()> {
    let cols = terminal_cols();
    let echo_lines = submitted_echo_lines(mode, input.trim_end(), cols);
    let echo_rows = echo_lines.len().min(u16::MAX as usize) as u16;
    let rows_to_clear = rendered_rows.max(echo_rows).max(1);
    for row_offset in 0..rows_to_clear {
        queue!(
            stdout,
            MoveTo(0, input_row.saturating_add(row_offset)),
            Clear(ClearType::CurrentLine)
        )?;
    }
    for (offset, line) in echo_lines.iter().enumerate() {
        queue!(
            stdout,
            MoveTo(
                0,
                input_row.saturating_add(offset.min(u16::MAX as usize) as u16)
            ),
            Print(line)
        )?;
    }
    queue!(
        stdout,
        MoveTo(0, input_row.saturating_add(echo_rows).saturating_add(1))
    )?;
    stdout.flush()?;
    Ok(())
}

fn submitted_echo_lines(mode: AgentMode, input: &str, cols: usize) -> Vec<String> {
    let max_text_width = cols.saturating_sub(3).max(1);
    let bar = submitted_echo_bar(mode);
    let mut output = Vec::new();
    output.push(bar.clone());
    for line in input.split('\n') {
        let mut chunks = wrap_visible_width(line, max_text_width);
        if chunks.is_empty() {
            chunks.push(String::new());
        }
        for chunk in chunks {
            output.push(format!("{bar} {}", colorize_repl_placeholders(&chunk)));
        }
    }
    output.push(bar);
    output
}

fn submitted_echo_bar(mode: AgentMode) -> String {
    match mode {
        AgentMode::Normal => "\x1b[1m\x1b[34m┃\x1b[0m".to_string(),
        AgentMode::Chat => "\x1b[1m\x1b[32m┃\x1b[0m".to_string(),
    }
}

fn input_prompt_bar(mode: AgentMode) -> String {
    format!("{} ", submitted_echo_bar(mode))
}

fn repl_shortcut_hint_line(mode: AgentMode, cols: usize) -> String {
    let bar = input_prompt_bar(mode);
    let text = t(
        "Tab switch mode; Shift+Enter newline; Ctrl+J newline; Ctrl+V paste clipboard",
        "Tab 切换模式；Shift+Enter 换行；Ctrl+J 换行；Ctrl+V 粘贴剪贴板",
    );
    let text_width = cols.saturating_sub(visible_width(&bar)).max(1);
    format!(
        "{bar}\x1b[2m{}\x1b[0m",
        truncate_visible_width(text, text_width)
    )
}

fn wrap_visible_width(value: &str, max_width: usize) -> Vec<String> {
    let mut lines = Vec::new();
    let mut current = String::new();
    let mut width = 0usize;
    for ch in value.chars() {
        let char_width = visible_width(&ch.to_string());
        if width > 0 && width.saturating_add(char_width) > max_width {
            lines.push(std::mem::take(&mut current));
            width = 0;
        }
        current.push(ch);
        width = width.saturating_add(char_width);
    }
    if !current.is_empty() {
        lines.push(current);
    }
    lines
}

fn repl_wrapped_input_rows_for_cols(prefix: &str, lines: &[String], cols: usize) -> Vec<String> {
    let max_width = repl_content_width_for_cols(prefix, cols);
    let mut rows = Vec::new();
    for line in lines {
        let mut current = String::new();
        let mut width = 0usize;
        for ch in line.chars() {
            let char_width = visible_width(&ch.to_string());
            if width > 0 && width.saturating_add(char_width) > max_width {
                rows.push(std::mem::take(&mut current));
                width = 0;
            }
            current.push(ch);
            width = width.saturating_add(char_width);
        }
        rows.push(current);
        if width > 0 && width % max_width == 0 {
            rows.push(String::new());
        }
    }
    if rows.is_empty() {
        rows.push(String::new());
    }
    rows
}

fn repl_cursor_position_for_line_for_cols(
    prefix: &str,
    line: &str,
    cursor: usize,
    cols: usize,
) -> (u16, u16) {
    let cols = cols.max(1);
    let prefix_width = repl_prefix_width_for_cols(prefix, cols);
    let content_width = repl_content_width_for_cols(prefix, cols);
    let mut col = 0usize;
    let mut row = 0usize;
    for ch in line.chars().take(cursor) {
        let char_width = visible_width(&ch.to_string()).max(1);
        if col > 0 && col.saturating_add(char_width) > content_width {
            row = row.saturating_add(1);
            col = 0;
        }
        col = col.saturating_add(char_width);
        if col >= content_width {
            row = row.saturating_add(1);
            col = 0;
        }
    }
    (
        prefix_width.saturating_add(col).min(u16::MAX as usize) as u16,
        row.min(u16::MAX as usize) as u16,
    )
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

fn repl_move_cursor_vertical(prefix: &str, input: &str, cursor: usize, direction: i32) -> usize {
    if input.is_empty() || direction == 0 {
        return cursor.min(input.chars().count());
    }
    repl_move_cursor_vertical_for_cols(prefix, input, cursor, direction, terminal_cols())
}

fn repl_move_cursor_vertical_for_cols(
    prefix: &str,
    input: &str,
    cursor: usize,
    direction: i32,
    cols: usize,
) -> usize {
    let positions = repl_cursor_layout_positions_for_cols(prefix, input, cols);
    let cursor = cursor.min(positions.len().saturating_sub(1));
    let (_, current_row, current_col) = positions[cursor];
    let last_row = positions.last().map(|(_, row, _)| *row).unwrap_or(0);
    let target_row = if direction < 0 {
        current_row.saturating_sub(1)
    } else {
        current_row.saturating_add(1).min(last_row)
    };
    if target_row == current_row {
        return cursor;
    }

    positions
        .iter()
        .filter(|(_, row, _)| *row == target_row)
        .min_by_key(|(index, _, col)| (col.abs_diff(current_col), usize::MAX - *index))
        .map(|(index, _, _)| *index)
        .unwrap_or(cursor)
}

fn repl_cursor_layout_positions_for_cols(
    prefix: &str,
    input: &str,
    cols: usize,
) -> Vec<(usize, usize, usize)> {
    let content_width = repl_content_width_for_cols(prefix, cols);
    let mut positions = Vec::with_capacity(input.chars().count() + 1);
    let mut row = 0usize;
    let mut col = 0usize;
    positions.push((0, row, col));
    for (index, ch) in input.chars().enumerate() {
        if ch == '\n' {
            row = row.saturating_add(1);
            col = 0;
            positions.push((index + 1, row, col));
            continue;
        }
        let char_width = visible_width(&ch.to_string()).max(1);
        if col > 0 && col.saturating_add(char_width) > content_width {
            row = row.saturating_add(1);
            col = 0;
        }
        col = col.saturating_add(char_width);
        if col >= content_width {
            row = row.saturating_add(1);
            col = 0;
        }
        positions.push((index + 1, row, col));
    }
    positions
}

fn repl_prompt_rows(prefix: &str, lines: &[String]) -> u16 {
    repl_prompt_rows_for_cols(prefix, lines, terminal_cols())
}

fn repl_cursor_position(prefix: &str, input: &str, cursor: usize) -> (u16, u16) {
    repl_cursor_position_for_cols(prefix, input, cursor, terminal_cols())
}

fn repl_line_rows_for_cols(prefix: &str, line: &str, cols: usize) -> u16 {
    let content_width = repl_content_width_for_cols(prefix, cols);
    let width = visible_width(line);
    (width / content_width + 1).min(u16::MAX as usize) as u16
}

fn repl_prefix_width_for_cols(prefix: &str, cols: usize) -> usize {
    visible_width(prefix).min(cols.max(1).saturating_sub(1))
}

fn repl_content_width_for_cols(prefix: &str, cols: usize) -> usize {
    cols.max(1)
        .saturating_sub(repl_prefix_width_for_cols(prefix, cols))
        .max(1)
}

fn repl_prompt_rows_for_cols(prefix: &str, lines: &[String], cols: usize) -> u16 {
    let cols = cols.max(1);
    let mut rows = 0usize;
    for line in lines {
        rows += repl_line_rows_for_cols(prefix, line, cols) as usize;
    }
    rows.max(1).min(u16::MAX as usize) as u16
}

fn repl_cursor_position_for_cols(
    prefix: &str,
    input: &str,
    cursor: usize,
    cols: usize,
) -> (u16, u16) {
    let cols = cols.max(1);
    let before_cursor = take_chars(input, cursor);
    let lines = repl_input_lines(&before_cursor);
    let last_index = lines.len().saturating_sub(1);
    let mut row_offset = 0usize;
    for (index, line) in lines.iter().enumerate() {
        if index == last_index {
            let (col, row) =
                repl_cursor_position_for_line_for_cols(prefix, line, line.chars().count(), cols);
            return (
                col,
                row_offset
                    .saturating_add(row as usize)
                    .min(u16::MAX as usize) as u16,
            );
        }
        row_offset += repl_line_rows_for_cols(prefix, line, cols) as usize;
    }
    (
        repl_prefix_width_for_cols(prefix, cols).min(u16::MAX as usize) as u16,
        0,
    )
}

fn insert_char_at_cursor(value: &mut String, cursor: &mut usize, ch: char) {
    let byte_index = byte_index_for_char(value, *cursor);
    value.insert(byte_index, ch);
    *cursor += 1;
}

fn insert_str_at_cursor(value: &mut String, cursor: &mut usize, text: &str) {
    let byte_index = byte_index_for_char(value, *cursor);
    value.insert_str(byte_index, text);
    *cursor += text.chars().count();
}

fn insert_newline_at_cursor(value: &mut String, cursor: &mut usize) {
    insert_char_at_cursor(value, cursor, '\n');
}

fn remove_char_before_cursor(value: &mut String, cursor: &mut usize) {
    let end = byte_index_for_char(value, *cursor);
    let start = byte_index_for_char(value, cursor.saturating_sub(1));
    value.replace_range(start..end, "");
    *cursor -= 1;
}

fn remove_word_before_cursor(value: &mut String, cursor: &mut usize) {
    if *cursor == 0 {
        return;
    }
    let chars = value.chars().collect::<Vec<_>>();
    let mut start = (*cursor).min(chars.len());
    while start > 0 && chars[start - 1].is_whitespace() {
        start -= 1;
    }
    while start > 0 && !chars[start - 1].is_whitespace() {
        start -= 1;
    }
    let byte_start = byte_index_for_char(value, start);
    let byte_end = byte_index_for_char(value, *cursor);
    value.replace_range(byte_start..byte_end, "");
    *cursor = start;
}

fn remove_char_at_cursor(value: &mut String, cursor: usize) {
    if cursor >= value.chars().count() {
        return;
    }
    let start = byte_index_for_char(value, cursor);
    let end = byte_index_for_char(value, cursor + 1);
    value.replace_range(start..end, "");
}

fn byte_index_for_char(value: &str, char_index: usize) -> usize {
    value
        .char_indices()
        .nth(char_index)
        .map(|(index, _)| index)
        .unwrap_or(value.len())
}

fn should_summarize_pasted_text(text: &str) -> bool {
    !text.is_empty()
        && (pasted_text_line_count(text) >= REPL_PASTE_PLACEHOLDER_MIN_LINES
            || text.chars().count() > REPL_PASTE_PLACEHOLDER_MIN_CHARS)
}

fn pasted_text_line_count(text: &str) -> usize {
    if text.is_empty() {
        0
    } else {
        text.chars().filter(|ch| *ch == '\n').count() + 1
    }
}

fn pasted_text_placeholder(index: usize, line_count: usize) -> String {
    if is_zh() {
        format!("[粘贴 {index}: ~{line_count} 行]")
    } else {
        format!("[Pasted {index}: ~{line_count} lines]")
    }
}

fn insert_pasted_text_at_cursor(
    input: &mut String,
    cursor: &mut usize,
    text: String,
    pasted_texts: &mut Vec<Option<PastedText>>,
) {
    let text = strip_terminal_control_sequences(&text);
    if should_summarize_pasted_text(&text) {
        let index = pasted_texts.len() + 1;
        let placeholder = pasted_text_placeholder(index, pasted_text_line_count(&text));
        insert_str_at_cursor(input, cursor, &placeholder);
        pasted_texts.push(Some(PastedText { text }));
    } else {
        insert_str_at_cursor(input, cursor, &text);
    }
}

fn find_repl_placeholders(input: &str) -> Vec<(usize, usize)> {
    let mut result = Vec::new();
    let chars: Vec<char> = input.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let prefix_len = if i + 7 <= chars.len()
            && chars[i..i + 7].iter().collect::<String>() == "[Image "
        {
            Some(7)
        } else if i + 8 <= chars.len() && chars[i..i + 8].iter().collect::<String>() == "[Pasted " {
            Some(8)
        } else if i + 4 <= chars.len() && chars[i..i + 4].iter().collect::<String>() == "[粘贴 " {
            Some(4)
        } else {
            None
        };

        if let Some(prefix_len) = prefix_len {
            let mut j = i + prefix_len;
            while j < chars.len() && chars[j].is_ascii_digit() {
                j += 1;
            }
            if j < chars.len() && chars[j] == ':' {
                j += 1;
                while j < chars.len() && chars[j] != ']' {
                    j += 1;
                }
                if j < chars.len() && chars[j] == ']' {
                    result.push((i, j + 1));
                    i = j + 1;
                    continue;
                }
            } else if j < chars.len() && chars[j] == ']' {
                result.push((i, j + 1));
                i = j + 1;
                continue;
            }
        }
        i += 1;
    }
    result
}

fn find_image_placeholders(input: &str) -> Vec<(usize, usize)> {
    find_repl_placeholders(input)
        .into_iter()
        .filter(|(start, end)| parse_image_placeholder_index(input, *start, *end).is_some())
        .collect()
}

fn find_pasted_text_placeholders(input: &str) -> Vec<(usize, usize, usize)> {
    find_repl_placeholders(input)
        .into_iter()
        .filter_map(|(start, end)| {
            parse_pasted_text_placeholder_index(input, start, end).map(|index| (start, end, index))
        })
        .collect()
}

fn placeholder_at_cursor(input: &str, cursor: usize) -> Option<(usize, usize)> {
    let placeholders = find_repl_placeholders(input);
    for (start, end) in &placeholders {
        if cursor > *start && cursor < *end {
            return Some((*start, *end));
        }
    }
    None
}

fn placeholder_before_cursor(input: &str, cursor: usize) -> Option<(usize, usize)> {
    let placeholders = find_repl_placeholders(input);
    for (start, end) in &placeholders {
        if *end == cursor {
            return Some((*start, *end));
        }
    }
    None
}

fn placeholder_before_or_at_cursor(input: &str, cursor: usize) -> Option<(usize, usize)> {
    placeholder_at_cursor(input, cursor).or_else(|| placeholder_before_cursor(input, cursor))
}

fn placeholder_after_cursor(input: &str, cursor: usize) -> Option<(usize, usize)> {
    let placeholders = find_repl_placeholders(input);
    for (start, end) in &placeholders {
        if *start == cursor {
            return Some((*start, *end));
        }
    }
    None
}

fn placeholder_after_or_at_cursor(input: &str, cursor: usize) -> Option<(usize, usize)> {
    placeholder_at_cursor(input, cursor).or_else(|| placeholder_after_cursor(input, cursor))
}

fn remove_range_chars(value: &mut String, char_start: usize, char_end: usize) {
    let byte_start = byte_index_for_char(value, char_start);
    let byte_end = byte_index_for_char(value, char_end);
    value.replace_range(byte_start..byte_end, "");
}

fn parse_image_placeholder_index(input: &str, char_start: usize, char_end: usize) -> Option<usize> {
    let chars: Vec<char> = input.chars().collect();
    let segment: String = chars[char_start..char_end].iter().collect();
    let after_prefix = segment.strip_prefix("[Image ")?;
    let num_str: String = after_prefix
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect();
    num_str.parse::<usize>().ok()
}

fn parse_pasted_text_placeholder_index(
    input: &str,
    char_start: usize,
    char_end: usize,
) -> Option<usize> {
    let chars: Vec<char> = input.chars().collect();
    let segment: String = chars[char_start..char_end].iter().collect();
    let after_prefix = segment
        .strip_prefix("[Pasted ")
        .or_else(|| segment.strip_prefix("[粘贴 "))?;
    let num_str: String = after_prefix
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect();
    num_str.parse::<usize>().ok()
}

fn clear_placeholder_payload(
    input: &str,
    start: usize,
    end: usize,
    pasted_images: &mut [Option<crate::clipboard::PastedImage>],
    pasted_texts: &mut [Option<PastedText>],
) {
    if let Some(n) = parse_image_placeholder_index(input, start, end) {
        if n > 0 && n <= pasted_images.len() {
            pasted_images[n - 1] = None;
        }
    }
    if let Some(n) = parse_pasted_text_placeholder_index(input, start, end) {
        if n > 0 && n <= pasted_texts.len() {
            pasted_texts[n - 1] = None;
        }
    }
}

fn expand_pasted_text_placeholders(input: &str, pasted_texts: &[Option<PastedText>]) -> String {
    let placeholders = find_pasted_text_placeholders(input);
    if placeholders.is_empty() {
        return input.to_string();
    }

    let chars: Vec<char> = input.chars().collect();
    let mut expanded = String::new();
    let mut last_end = 0;
    for (start, end, index) in placeholders {
        expanded.extend(&chars[last_end..start]);
        if index > 0 {
            if let Some(Some(pasted_text)) = pasted_texts.get(index - 1) {
                expanded.push_str(&pasted_text.text);
            } else {
                expanded.extend(&chars[start..end]);
            }
        } else {
            expanded.extend(&chars[start..end]);
        }
        last_end = end;
    }
    expanded.extend(&chars[last_end..]);
    expanded
}

fn placeholder_text_near_cursor(
    input: &str,
    cursor: usize,
    pasted_texts: &[Option<PastedText>],
) -> Option<String> {
    let (start, end) = placeholder_at_cursor(input, cursor)
        .or_else(|| placeholder_before_cursor(input, cursor))
        .or_else(|| placeholder_after_cursor(input, cursor))?;
    let index = parse_pasted_text_placeholder_index(input, start, end)?;
    pasted_texts
        .get(index.checked_sub(1)?)
        .and_then(Option::as_ref)
        .map(|pasted_text| pasted_text.text.clone())
}

fn take_chars(value: &str, count: usize) -> String {
    value.chars().take(count).collect()
}

fn terminal_cols() -> usize {
    terminal::size()
        .map(|(cols, _)| cols.max(1) as usize)
        .unwrap_or(80)
}

fn repl_input_lines(input: &str) -> Vec<String> {
    let normalized = strip_terminal_control_sequences(input)
        .replace("\r\n", "\n")
        .replace('\r', "\n");
    let mut lines = normalized
        .split('\n')
        .map(ToString::to_string)
        .collect::<Vec<_>>();
    if lines.is_empty() {
        lines.push(String::new());
    }
    lines
}

fn strip_terminal_control_sequences(input: &str) -> String {
    let mut output = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\x1b' {
            if chars.peek() == Some(&'[') {
                chars.next();
                for next in chars.by_ref() {
                    if ('@'..='~').contains(&next) {
                        break;
                    }
                }
            } else {
                chars.next();
            }
            continue;
        }
        if is_disallowed_control_char(ch) {
            continue;
        }
        output.push(ch);
    }
    output
}

fn is_disallowed_control_char(ch: char) -> bool {
    ch.is_control() && !matches!(ch, '\n' | '\t')
}

fn visible_width(value: &str) -> usize {
    let mut width = 0usize;
    let mut escape = false;
    for ch in value.chars() {
        if escape {
            if ch == 'm' {
                escape = false;
            }
            continue;
        }
        if ch == '\x1b' {
            escape = true;
        } else if (ch as u32) >= 0x2e80 {
            width += 2;
        } else {
            width += 1;
        }
    }
    width
}

fn colorize_repl_placeholders(line: &str) -> String {
    let placeholders = find_repl_placeholders(line);
    if placeholders.is_empty() {
        return line.to_string();
    }

    let chars: Vec<char> = line.chars().collect();
    let mut result = String::new();
    let mut last_end = 0;
    for (start, end) in placeholders {
        result.extend(&chars[last_end..start]);
        result.push_str("\x1b[35m");
        result.extend(&chars[start..end]);
        result.push_str("\x1b[0m");
        last_end = end;
    }
    result.extend(&chars[last_end..]);
    result
}

/// Identity of a REPL slash command, dispatched via `parse_repl_input`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReplSlashCommand {
    New,
    Session,
    Rename,
    Archive,
    Delete,
    Workspace,
    Models,
    Persona,
    Usage,
    Config,
    Variant,
    Undo,
    Pop,
    Compact,
    Reset,
    Wipe,
    History,
    Clear,
    Help,
    Exit,
}

struct ReplCommandSpec {
    name: &'static str,
    command: ReplSlashCommand,
    /// Argument hint rendered in /help, e.g. "[count]"; empty when the
    /// command takes no arguments (enforced at dispatch).
    arg_hint: &'static str,
    help_en: &'static str,
    help_zh: &'static str,
}

/// Single source of truth for REPL slash commands: drives Tab completion,
/// prefix resolution, /help output, and dispatch in the REPL loop.
const REPL_COMMAND_TABLE: &[ReplCommandSpec] = &[
    ReplCommandSpec {
        name: "/new",
        command: ReplSlashCommand::New,
        arg_hint: "[name]",
        help_en: "create a new session and switch to it",
        help_zh: "创建新会话并切换过去",
    },
    ReplCommandSpec {
        name: "/session",
        command: ReplSlashCommand::Session,
        arg_hint: "[name|index]",
        help_en: "list sessions, or switch to one (Ctrl+D deletes in the picker)",
        help_zh: "列出会话，或切换到指定会话（菜单内 Ctrl+D 删除）",
    },
    ReplCommandSpec {
        name: "/rename",
        command: ReplSlashCommand::Rename,
        arg_hint: "<name>",
        help_en: "rename the current session",
        help_zh: "重命名当前会话",
    },
    ReplCommandSpec {
        name: "/archive",
        command: ReplSlashCommand::Archive,
        arg_hint: "",
        help_en: "archive the current session",
        help_zh: "归档当前会话",
    },
    ReplCommandSpec {
        name: "/delete",
        command: ReplSlashCommand::Delete,
        arg_hint: "[name|index]",
        help_en: "delete a session (current by default)",
        help_zh: "删除会话（默认当前会话）",
    },
    ReplCommandSpec {
        name: "/workspace",
        command: ReplSlashCommand::Workspace,
        arg_hint: "[path|clear]",
        help_en: "show, bind, or unbind the session workspace",
        help_zh: "查看、绑定或解绑会话工作目录",
    },
    ReplCommandSpec {
        name: "/models",
        command: ReplSlashCommand::Models,
        arg_hint: "[index|provider/model|default]",
        help_en: "switch this session's model",
        help_zh: "切换当前会话使用的模型",
    },
    ReplCommandSpec {
        name: "/persona",
        command: ReplSlashCommand::Persona,
        arg_hint: "[name]",
        help_en: "switch the active persona",
        help_zh: "切换当前人格",
    },
    ReplCommandSpec {
        name: "/usage",
        command: ReplSlashCommand::Usage,
        arg_hint: "",
        help_en: "show token usage details",
        help_zh: "显示 Token 用量详情",
    },
    ReplCommandSpec {
        name: "/config",
        command: ReplSlashCommand::Config,
        arg_hint: "",
        help_en: "open configuration UI",
        help_zh: "打开配置界面",
    },
    ReplCommandSpec {
        name: "/variant",
        command: ReplSlashCommand::Variant,
        arg_hint: "[name]",
        help_en: "view or switch thinking level",
        help_zh: "查看或切换思考档位",
    },
    ReplCommandSpec {
        name: "/undo",
        command: ReplSlashCommand::Undo,
        arg_hint: "",
        help_en: "undo the last turn or context compaction",
        help_zh: "撤销上一轮或上下文压缩",
    },
    ReplCommandSpec {
        name: "/pop",
        command: ReplSlashCommand::Pop,
        arg_hint: "[count]",
        help_en: "pop selected turns or the oldest count from active context",
        help_zh: "从当前上下文弹出所选轮次或最旧的指定轮数",
    },
    ReplCommandSpec {
        name: "/compact",
        command: ReplSlashCommand::Compact,
        arg_hint: "",
        help_en: "compact current conversation context now",
        help_zh: "立即压缩当前会话上下文",
    },
    ReplCommandSpec {
        name: "/reset",
        command: ReplSlashCommand::Reset,
        arg_hint: "",
        help_en: "start this conversation over",
        help_zh: "重新开始当前会话",
    },
    ReplCommandSpec {
        name: "/wipe",
        command: ReplSlashCommand::Wipe,
        arg_hint: "",
        help_en: "erase memory, every conversation, group contexts and generated skills",
        help_zh: "抹掉记忆、所有会话内容、群聊上下文和自动技能",
    },
    ReplCommandSpec {
        name: "/history",
        command: ReplSlashCommand::History,
        arg_hint: "",
        help_en: "show recent conversation history",
        help_zh: "显示最近的会话历史",
    },
    ReplCommandSpec {
        name: "/clear",
        command: ReplSlashCommand::Clear,
        arg_hint: "",
        help_en: "clear the screen",
        help_zh: "清屏",
    },
    ReplCommandSpec {
        name: "/help",
        command: ReplSlashCommand::Help,
        arg_hint: "",
        help_en: "show this help",
        help_zh: "显示此帮助",
    },
    ReplCommandSpec {
        name: "/exit",
        command: ReplSlashCommand::Exit,
        arg_hint: "",
        help_en: "leave REPL",
        help_zh: "退出 REPL",
    },
];

fn repl_command_spec(command: ReplSlashCommand) -> &'static ReplCommandSpec {
    REPL_COMMAND_TABLE
        .iter()
        .find(|spec| spec.command == command)
        .expect("every ReplSlashCommand has a table entry")
}

/// Parsed REPL input: plain chat, a resolved slash command with its argument
/// string, or an unknown/ambiguous slash command.
enum ReplInput<'a> {
    Chat,
    Slash(ReplSlashCommand, &'a str),
    UnknownSlash(&'a str),
}

fn parse_repl_input(input: &str) -> ReplInput<'_> {
    if !input.starts_with('/') {
        return ReplInput::Chat;
    }
    let (name, args) = split_repl_command(input);
    let lowered = name.to_ascii_lowercase();
    if let Some(spec) = REPL_COMMAND_TABLE.iter().find(|spec| spec.name == lowered) {
        return ReplInput::Slash(spec.command, args);
    }
    let mut matches = REPL_COMMAND_TABLE
        .iter()
        .filter(|spec| spec.name.starts_with(&lowered));
    match (matches.next(), matches.next()) {
        (Some(spec), None) => ReplInput::Slash(spec.command, args),
        _ => ReplInput::UnknownSlash(name),
    }
}

fn repl_commands() -> Vec<&'static str> {
    REPL_COMMAND_TABLE.iter().map(|spec| spec.name).collect()
}

fn repl_command_suggestions(input: &str) -> Vec<&'static str> {
    if !input.starts_with('/') {
        return Vec::new();
    }
    repl_commands()
        .into_iter()
        .filter(|command| command.starts_with(input))
        .collect()
}

fn complete_repl_command(input: &str) -> Option<&'static str> {
    let suggestions = repl_command_suggestions(input);
    if suggestions.len() == 1 {
        suggestions.first().copied()
    } else {
        None
    }
}

fn resolve_repl_command<'a>(input: &'a str) -> &'a str {
    if input.starts_with('/') {
        if let Some(command) = complete_repl_command(input) {
            return command;
        }
    }
    input
}

fn repl_command_suggestions_line(suggestions: &[&str], max_width: usize) -> String {
    let line = if suggestions.len() == 1 {
        suggestions[0].to_string()
    } else {
        suggestions.join("  ")
    };
    truncate_visible_width(&line, max_width)
}

fn truncate_visible_width(value: &str, max_width: usize) -> String {
    if visible_width(value) <= max_width {
        return value.to_string();
    }
    if max_width <= 3 {
        return ".".repeat(max_width);
    }
    let mut output = String::new();
    let mut width = 0usize;
    let ellipsis_width = visible_width("...");
    let budget = max_width.saturating_sub(ellipsis_width);
    for ch in value.chars() {
        let ch_width = visible_width(&ch.to_string());
        if width.saturating_add(ch_width) > budget {
            break;
        }
        output.push(ch);
        width = width.saturating_add(ch_width);
    }
    output.push_str("...");
    output
}

#[cfg(test)]
mod repl_input_tests {
    use super::*;
    use crate::llm::ChatStreamKind;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    fn sample_pop_turn(status: TurnStatus) -> Turn {
        Turn {
            turn_id: "turn-1".to_string(),
            seq: 1,
            user_content: "first prompt line\nsecond prompt line".to_string(),
            display_content: "first prompt line\nsecond prompt line".to_string(),
            user_timestamp: "2026-07-19 10:42".to_string(),
            assistant_content: "first answer line\nsecond answer line".to_string(),
            assistant_reasoning: Some("private reasoning".to_string()),
            assistant_provider_id: None,
            assistant_model: None,
            assistant_timestamp: Some("2026-07-19 10:43".to_string()),
            status,
            tool_reports: vec!["hidden tool report".to_string()],
            question_exchanges: Vec::new(),
            followups: Vec::new(),
            attachments: Vec::new(),
            hidden: false,
            is_summary: false,
            owner_pid: None,
            token_total: 0,
            token_prompt: 0,
            token_cache_read: 0,
            token_usage_estimated: false,
            revision: 0,
            journal_events: Vec::new(),
            context_messages: Vec::new(),
        }
    }

    fn pop_test_paths(root: &std::path::Path) -> MiyuPaths {
        MiyuPaths {
            root_dir: root.to_path_buf(),
            config_dir: root.join("config"),
            config_file: root.join("config/config.jsonc"),
            skills_dir: root.join("config/skills"),
            data_dir: root.join("data"),
            cache_dir: root.join("cache"),
            state_dir: root.join("state"),
            pictures_dir: root.join("pictures"),
            fish_hook_file: root.join("fish/miyu.fish"),
            bash_hook_file: root.join("shell/bash-hook.sh"),
            zsh_hook_file: root.join("shell/zsh-hook.zsh"),
            scripts_dir: root.join("config/scripts"),
            system_scripts_dir: root.join("system/scripts"),
        }
    }

    #[test]
    fn terminal_frame_tracks_ansi_and_wide_graphemes() {
        let layout = terminal_frame_layout("\x1b[32mAB\x1b[0m\n中👨‍👩‍👧‍👦".as_bytes(), (3, 2), 12, None);

        assert_eq!(layout.cursor, (4, 3));
        assert_eq!(layout.occupied_bottom, Some(3));
    }

    #[test]
    fn terminal_frame_wraps_before_the_next_wide_grapheme() {
        let layout = terminal_frame_layout("中🙂".as_bytes(), (8, 1), 10, None);

        assert_eq!(layout.cursor, (2, 2));
        assert_eq!(layout.occupied_bottom, Some(2));
    }

    #[test]
    fn terminal_frame_applies_cursor_motion_without_losing_bottom_occupancy() {
        let layout = terminal_frame_layout(b"first\nsecond\x1b[1A\x1b[3G!", (0, 4), 20, None);

        assert_eq!(layout.cursor, (3, 4));
        assert_eq!(layout.occupied_bottom, Some(5));
    }

    #[test]
    fn terminal_frame_scroll_margin_keeps_cursor_above_live_input() {
        let layout = terminal_frame_layout("one\n二\nthree".as_bytes(), (0, 5), 20, Some(5));

        assert_eq!(layout.cursor, (5, 5));
        assert_eq!(layout.occupied_bottom, Some(5));
    }

    #[test]
    fn live_frame_uses_the_gap_only_for_a_terminating_newline() {
        let content = terminal_frame_layout(b"answer", (0, 5), 20, None);
        assert_eq!(live_frame_output_bottom(6, content), Some(5));

        let terminated = terminal_frame_layout(b"answer\n", (0, 5), 20, None);
        assert_eq!(live_frame_output_bottom(6, terminated), Some(6));
        let bounded = terminal_frame_layout(
            b"answer\n",
            (0, 5),
            20,
            live_frame_output_bottom(6, terminated),
        );
        assert_eq!(bounded.cursor, (0, 6));
        assert_eq!(bounded.occupied_bottom, Some(5));
    }

    #[test]
    fn models_is_the_cli_model_selector() {
        let matches = localized_command()
            .try_get_matches_from(["miyu", "models", "1"])
            .unwrap();
        let cli = Cli::from_arg_matches(&matches).unwrap();

        assert!(matches!(
            cli.command,
            Some(Command::Models(ModelsArgs { target: Some(ref target) })) if target == "1"
        ));
        let old_matches = localized_command()
            .try_get_matches_from(["miyu", "providers"])
            .unwrap();
        let old_cli = Cli::from_arg_matches(&old_matches).unwrap();
        assert!(old_cli.command.is_none());
        assert_eq!(old_cli.message, ["providers"]);
    }

    #[test]
    fn variant_is_a_cli_subcommand_with_an_optional_name() {
        let cli = parse_args(["miyu", "variant"].map(OsString::from).to_vec()).unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::Variant(VariantArgs { name: None }))
        ));

        let cli = parse_args(["miyu", "variant", "high"].map(OsString::from).to_vec()).unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::Variant(VariantArgs { name })) if name.as_deref() == Some("high")
        ));

        assert!(parse_args(
            ["miyu", "variant", "high", "extra"]
                .map(OsString::from)
                .to_vec()
        )
        .is_err());
    }

    #[tokio::test]
    async fn one_shot_turns_default_to_a_throwaway_session() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let paths = MiyuPaths {
            root_dir: root.to_path_buf(),
            config_dir: root.join("config"),
            config_file: root.join("config/config.jsonc"),
            skills_dir: root.join("config/skills"),
            data_dir: root.join("data"),
            cache_dir: root.join("cache"),
            state_dir: root.join("state"),
            pictures_dir: root.join("pictures"),
            fish_hook_file: root.join("fish"),
            bash_hook_file: root.join("bash"),
            zsh_hook_file: root.join("zsh"),
            scripts_dir: root.join("scripts"),
            system_scripts_dir: root.join("system-scripts"),
        };

        // Neither flag: `miyu ask` / `miyu '<message>'` must not touch a real
        // conversation. `--continue` opts back into the terminal session.
        // Both resolve without contacting the daemon.
        assert_eq!(
            one_shot_session(&paths, None, false).await.unwrap(),
            TurnSession::Ephemeral
        );
        assert_eq!(
            one_shot_session(&paths, None, true).await.unwrap(),
            TurnSession::Current
        );
    }

    #[test]
    fn continue_and_session_flags_are_mutually_exclusive() {
        let cli = parse_args(["miyu", "-c", "hello"].map(OsString::from).to_vec()).unwrap();
        assert!(cli.continue_session);
        assert_eq!(cli.message, vec!["hello".to_string()]);

        let cli = parse_args(
            ["miyu", "--session", "2", "hello"]
                .map(OsString::from)
                .to_vec(),
        )
        .unwrap();
        assert!(!cli.continue_session);
        assert_eq!(cli.session.as_deref(), Some("2"));

        assert!(parse_args(
            ["miyu", "-c", "--session", "2", "hello"]
                .map(OsString::from)
                .to_vec()
        )
        .is_err());
    }

    #[test]
    fn replayed_job_wake_turns_are_not_drawn_as_user_prompts() {
        let config = AppConfig::default();
        let wake = crate::state::TurnReplay {
            display_content: "[后台任务完成] 子代理完成 82bea3 · 后台测试A".to_string(),
            assistant_content: "跑完了。".to_string(),
            entries: Vec::new(),
            is_job_wake: true,
        };
        let typed = crate::state::TurnReplay {
            display_content: "帮我改一下 README".to_string(),
            assistant_content: "改好了。".to_string(),
            entries: Vec::new(),
            is_job_wake: false,
        };

        let frame = session_replay_frame(&[wake], AgentMode::Normal, &config, 80).unwrap();
        let frame = String::from_utf8_lossy(&frame);
        // Dim ⚙ notice with the bracketed prefix stripped, exactly like the
        // live path — never the user bubble's bar.
        assert!(frame.contains("⚙ 子代理完成 82bea3 · 后台测试A"));
        assert!(!frame.contains("[后台任务完成]"));
        assert!(!frame.contains(&submitted_echo_bar(AgentMode::Normal)));

        let frame = session_replay_frame(&[typed], AgentMode::Normal, &config, 80).unwrap();
        let frame = String::from_utf8_lossy(&frame);
        assert!(frame.contains(&submitted_echo_bar(AgentMode::Normal)));
        assert!(!frame.contains('⚙'));
    }

    #[test]
    fn picker_keys_reach_delete_only_through_a_modifier() {
        use crossterm::event::{KeyCode, KeyModifiers};
        let plain = KeyModifiers::NONE;
        let control = KeyModifiers::CONTROL;

        // Every printable character is search input, so a bare `d` must never
        // be a shortcut — deletion needs Ctrl+D (or the Delete key).
        assert_eq!(
            inline_select_key(KeyCode::Char('d'), plain, true),
            InlineSelectKey::Char('d')
        );
        assert_eq!(
            inline_select_key(KeyCode::Char('d'), control, true),
            InlineSelectKey::DeleteRequest
        );
        assert_eq!(
            inline_select_key(KeyCode::Delete, plain, true),
            InlineSelectKey::DeleteRequest
        );

        // Pickers that did not opt in stay exactly as they were.
        assert_eq!(
            inline_select_key(KeyCode::Char('d'), control, false),
            InlineSelectKey::Ignore
        );
        assert_eq!(
            inline_select_key(KeyCode::Delete, plain, false),
            InlineSelectKey::Ignore
        );

        assert_eq!(
            inline_select_key(KeyCode::Char('c'), control, true),
            InlineSelectKey::Cancel
        );
        assert_eq!(
            inline_select_key(KeyCode::Esc, plain, true),
            InlineSelectKey::Cancel
        );
        assert_eq!(
            inline_select_key(KeyCode::Enter, plain, true),
            InlineSelectKey::Accept
        );
        assert_eq!(
            inline_select_key(KeyCode::Char('j'), plain, true),
            InlineSelectKey::Down
        );
        assert_eq!(
            inline_select_key(KeyCode::Char('k'), plain, true),
            InlineSelectKey::Up
        );
    }

    #[test]
    fn web_is_a_cli_subcommand_with_local_server_options() {
        let cli = parse_args(
            ["miyu", "web", "--port", "4100"]
                .map(OsString::from)
                .to_vec(),
        )
        .unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::Web(WebArgs {
                port: 4100,
                bind: None,
                password: None,
                password_file: None,
                port_explicit: true,
            }))
        ));

        for arg in ["stop", "status", "restart", "--status", "--stop"] {
            assert!(parse_args(["miyu", "web", arg].map(OsString::from).to_vec()).is_err());
        }

        let cli = parse_args(["miyu", "web"].map(OsString::from).to_vec()).unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::Web(WebArgs {
                port: 8300,
                bind: None,
                password: None,
                password_file: None,
                port_explicit: false,
            }))
        ));

        let cli = parse_args(["miyu", "web", "-p"].map(OsString::from).to_vec()).unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::Web(WebArgs {
                password: Some(password),
                ..
            })) if password.is_empty()
        ));
        for args in [
            vec!["miyu", "web", "-p", "secret"],
            vec!["miyu", "web", "--password=secret"],
            vec!["miyu", "web", "-psecret"],
        ] {
            assert!(parse_args(args.into_iter().map(OsString::from).collect()).is_err());
        }

        let cli = parse_args(
            ["miyu", "web", "--password-file", "/tmp/miyu-password"]
                .map(OsString::from)
                .to_vec(),
        )
        .unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::Web(WebArgs {
                password: None,
                password_file: Some(path),
                ..
            })) if path == PathBuf::from("/tmp/miyu-password")
        ));

        assert!(parse_args(["miyu", "web", "--public"].map(OsString::from).to_vec(),).is_err());
    }

    #[test]
    fn web_password_is_materialized_as_a_private_file() {
        let temp = tempfile::tempdir().unwrap();
        let paths = pop_test_paths(temp.path());
        let args = WebArgs {
            port: 9400,
            bind: None,
            password: Some("very-secret".to_string()),
            password_file: None,
            port_explicit: false,
        };

        let launch = web_launch_config(&paths, &args).unwrap().unwrap();

        assert_eq!(launch.port, 9400);
        let password_file = launch.password_file.unwrap();
        let password_dir = paths.managed_web_password_dir();
        assert_eq!(password_file.parent(), Some(password_dir.as_path()));
        assert_eq!(
            std::fs::read_to_string(&password_file).unwrap(),
            "very-secret"
        );
        assert_eq!(
            std::fs::metadata(password_file)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }

    #[test]
    fn bare_web_does_not_override_the_persisted_launch_config() {
        let temp = tempfile::tempdir().unwrap();
        let paths = pop_test_paths(temp.path());
        let args = WebArgs {
            port: ipc::DEFAULT_WEB_PORT,
            bind: None,
            password: None,
            password_file: None,
            port_explicit: false,
        };

        assert!(web_launch_config(&paths, &args).unwrap().is_none());
    }

    #[test]
    fn explicit_password_file_is_copied_into_private_miyu_state() {
        let temp = tempfile::tempdir().unwrap();
        let paths = pop_test_paths(temp.path());
        let external = temp.path().join("external-password");
        std::fs::write(&external, "file-secret\n").unwrap();
        let args = WebArgs {
            port: ipc::DEFAULT_WEB_PORT,
            bind: None,
            password: None,
            password_file: Some(external.clone()),
            port_explicit: false,
        };

        let launch = web_launch_config(&paths, &args).unwrap().unwrap();
        let managed = launch.password_file.unwrap();
        assert_ne!(managed, external);
        let password_dir = paths.managed_web_password_dir();
        assert_eq!(managed.parent(), Some(password_dir.as_path()));
        assert_eq!(std::fs::read_to_string(managed).unwrap(), "file-secret");
    }

    #[test]
    fn daemon_owns_lifecycle_and_log_commands() {
        for (arg, expected) in [
            ("start", "start"),
            ("stop", "stop"),
            ("restart", "restart"),
            ("status", "status"),
        ] {
            let cli = parse_args(["miyu", "daemon", arg].map(OsString::from).to_vec()).unwrap();
            let actual = match cli.command {
                Some(Command::Daemon(DaemonArgs {
                    command: Some(DaemonCommand::Start),
                    ..
                })) => "start",
                Some(Command::Daemon(DaemonArgs {
                    command: Some(DaemonCommand::Stop),
                    ..
                })) => "stop",
                Some(Command::Daemon(DaemonArgs {
                    command: Some(DaemonCommand::Restart),
                    ..
                })) => "restart",
                Some(Command::Daemon(DaemonArgs {
                    command: Some(DaemonCommand::Status),
                    ..
                })) => "status",
                other => panic!("unexpected command: {other:?}"),
            };
            assert_eq!(actual, expected);
        }

        let cli = parse_args(["miyu", "daemon", "logs"].map(OsString::from).to_vec()).unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::Daemon(DaemonArgs {
                command: Some(DaemonCommand::Logs(DaemonLogsArgs { lines: None })),
                ..
            }))
        ));

        let cli = parse_args(
            ["miyu", "daemon", "logs", "-n", "25"]
                .map(OsString::from)
                .to_vec(),
        )
        .unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::Daemon(DaemonArgs {
                command: Some(DaemonCommand::Logs(DaemonLogsArgs { lines: Some(25) })),
                ..
            }))
        ));
    }

    #[test]
    fn reload_is_a_top_level_command() {
        let cli = parse_args(["miyu", "reload"].map(OsString::from).to_vec()).unwrap();
        assert!(matches!(cli.command, Some(Command::Reload)));
        assert!(parse_args(["miyu", "reload", "extra"].map(OsString::from).to_vec()).is_err());
    }

    #[test]
    fn config_reload_response_uses_codes_and_supports_legacy_busy_errors() {
        assert_eq!(
            validate_config_reload_response(Some(IpcFrame::coded_error(
                ipc::ErrorCode::Busy,
                "localized busy message",
            )))
            .unwrap(),
            ConfigReloadResponse::Busy
        );
        assert_eq!(
            validate_config_reload_response(Some(IpcFrame::error(ipc::ADMIN_BUSY_MESSAGE)))
                .unwrap(),
            ConfigReloadResponse::Busy
        );
        assert!(
            validate_config_reload_response(Some(IpcFrame::error("invalid configuration")))
                .is_err()
        );
    }

    #[tokio::test]
    async fn config_reload_retries_busy_responses_until_success() {
        let attempts = Arc::new(AtomicUsize::new(0));
        let observed = attempts.clone();

        retry_config_reload(4, Duration::ZERO, move || {
            let attempts = attempts.clone();
            async move {
                let attempt = attempts.fetch_add(1, Ordering::SeqCst) + 1;
                Ok(if attempt < 3 {
                    ConfigReloadResponse::Busy
                } else {
                    ConfigReloadResponse::Reloaded
                })
            }
        })
        .await
        .unwrap();

        assert_eq!(observed.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn config_reload_stops_after_the_attempt_limit() {
        let attempts = Arc::new(AtomicUsize::new(0));
        let observed = attempts.clone();

        let error = retry_config_reload(3, Duration::ZERO, move || {
            let attempts = attempts.clone();
            async move {
                attempts.fetch_add(1, Ordering::SeqCst);
                Ok(ConfigReloadResponse::Busy)
            }
        })
        .await
        .unwrap_err();

        assert_eq!(observed.load(Ordering::SeqCst), 3);
        assert!(error.to_string().contains('3'));
    }

    #[tokio::test]
    async fn config_reload_retries_coded_busy_frames_over_ipc() {
        let temp = tempfile::tempdir().unwrap();
        let socket = temp.path().join("reload.sock");
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let server = tokio::spawn(async move {
            for attempt in 1..=3 {
                let (mut stream, _) = listener.accept().await.unwrap();
                let request = ipc::receive::<IpcRequest>(&mut stream)
                    .await
                    .unwrap()
                    .unwrap();
                assert!(matches!(request.command, IpcCommand::ReloadConfig));
                let response = if attempt < 3 {
                    IpcFrame::coded_error(ipc::ErrorCode::Busy, ipc::ADMIN_BUSY_MESSAGE)
                } else {
                    IpcFrame::Ack
                };
                ipc::send(&mut stream, &response).await.unwrap();
            }
        });

        retry_config_reload(4, Duration::ZERO, || {
            request_config_reload_at(&socket, Duration::from_secs(1))
        })
        .await
        .unwrap();
        server.await.unwrap();
    }

    #[tokio::test]
    async fn config_reload_request_times_out_when_daemon_does_not_respond() {
        let temp = tempfile::tempdir().unwrap();
        let socket = temp.path().join("reload-timeout.sock");
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let request = ipc::receive::<IpcRequest>(&mut stream)
                .await
                .unwrap()
                .unwrap();
            assert!(matches!(request.command, IpcCommand::ReloadConfig));
            std::future::pending::<()>().await;
        });

        let error = request_config_reload_at(&socket, Duration::from_millis(100))
            .await
            .unwrap_err();
        assert!(error
            .downcast_ref::<tokio::time::error::Elapsed>()
            .is_some());
        server.abort();
        let _ = server.await;
    }

    #[test]
    fn daemon_accepts_a_port_and_defaults_to_start() {
        let cli = parse_args(
            ["miyu", "daemon", "--port", "9412"]
                .map(OsString::from)
                .to_vec(),
        )
        .unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::Daemon(DaemonArgs {
                port: Some(9412),
                command: None,
            }))
        ));

        let cli = parse_args(
            ["miyu", "daemon", "--port", "9412", "restart"]
                .map(OsString::from)
                .to_vec(),
        )
        .unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::Daemon(DaemonArgs {
                port: Some(9412),
                command: Some(DaemonCommand::Restart),
            }))
        ));

        let cli = parse_args(
            ["miyu", "daemon", "start", "--port", "9412"]
                .map(OsString::from)
                .to_vec(),
        )
        .unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::Daemon(DaemonArgs {
                port: Some(9412),
                command: Some(DaemonCommand::Start),
            }))
        ));

        assert!(parse_args(
            ["miyu", "daemon", "--password"]
                .map(OsString::from)
                .to_vec(),
        )
        .is_err());
    }

    #[test]
    fn daemon_web_urls_are_rendered_on_separate_aligned_lines() {
        let urls = vec![
            "http://127.0.0.1:8300".to_string(),
            "http://192.168.1.2:8300".to_string(),
        ];
        assert_eq!(
            daemon_web_status_lines("WebUI:", &urls),
            [
                "WebUI: http://127.0.0.1:8300",
                "       http://192.168.1.2:8300",
            ]
        );
        assert_eq!(
            daemon_web_status_lines("WebUI：", &urls),
            [
                "WebUI： http://127.0.0.1:8300",
                "        http://192.168.1.2:8300",
            ]
        );
    }

    #[test]
    fn daemon_log_formatter_parses_targets_and_preserves_multiline_content() {
        let parsed = parse_daemon_log_line(
            "2026-07-29T12:34:56.789Z  INFO miyu::qq: listener ready port=8090",
        )
        .unwrap();
        assert_eq!(parsed.level, "INFO");
        assert_eq!(parsed.module, "miyu::qq");
        assert_eq!(parsed.message, "listener ready port=8090");

        let rendered = format_daemon_log_line(
            "2026-07-29T12:34:56.789Z  INFO miyu::qq: listener ready port=8090",
            false,
        );
        assert!(!rendered.contains('\x1b'));
        assert!(rendered.ends_with("[INFO] [miyu::qq] listener ready port=8090"));
        assert_eq!(
            format_daemon_log_line("判断原因：保留这一行原有的内容", true),
            "判断原因：保留这一行原有的内容"
        );
    }

    #[test]
    fn daemon_log_formatter_supports_legacy_lines_and_tty_colors() {
        let legacy = "2026-07-29T12:34:56.789Z  WARN OneBot connection closed reason=timeout";
        let parsed = parse_daemon_log_line(legacy).unwrap();
        assert_eq!(parsed.module, "miyu");
        assert_eq!(parsed.message, "OneBot connection closed reason=timeout");

        let rendered = format_daemon_log_line(legacy, true);
        assert!(rendered.contains('\x1b'));
        assert!(rendered.contains("[WARN]"));
        assert!(rendered.ends_with("OneBot connection closed reason=timeout"));
    }

    #[test]
    fn daemon_log_formatter_colors_entire_active_reply_decisions() {
        let mut formatter = DaemonLogStreamFormatter::default();
        let mut reply = Vec::new();
        formatter
            .push(
                b"2026-07-29T12:34:56.789Z  INFO miyu::qq: \xe3\x80\x90\xe7\xbb\xad\xe8\x81\x8a\xe7\xaa\x97\xe5\x8f\xa3\xe5\x88\xa4\xe6\x96\xad\xef\xbc\x9a\xe5\x9b\x9e\xe5\xa4\x8d\xe3\x80\x91\n\xe7\xbb\x93\xe6\x9e\x9c\xef\xbc\x9a\xe5\x9b\x9e\xe5\xa4\x8d\n",
                true,
                &mut reply,
            )
            .unwrap();
        let reply = String::from_utf8(reply).unwrap();
        assert!(reply.lines().all(|line| line.contains('\x1b')));

        let mut no_reply = Vec::new();
        formatter
            .push(
                b"2026-07-29T12:34:57.789Z  INFO miyu::qq: \xe3\x80\x90\xe4\xb8\xbb\xe5\x8a\xa8\xe5\x9b\x9e\xe5\xa4\x8d\xe5\x88\xa4\xe6\x96\xad\xef\xbc\x9a\xe4\xb8\x8d\xe5\x9b\x9e\xe5\xa4\x8d\xe3\x80\x91\n\xe7\xbb\x93\xe6\x9e\x9c\xef\xbc\x9a\xe4\xb8\x8d\xe5\x9b\x9e\xe5\xa4\x8d\n",
                true,
                &mut no_reply,
            )
            .unwrap();
        let no_reply = String::from_utf8(no_reply).unwrap();
        assert!(no_reply.lines().all(|line| line.contains('\x1b')));
        assert_ne!(reply, no_reply);

        let mut reset = Vec::new();
        formatter
            .push(
                b"2026-07-29T12:34:58.789Z  INFO miyu::qq: listener ready\nplain continuation\n",
                false,
                &mut reset,
            )
            .unwrap();
        let reset = String::from_utf8(reset).unwrap();
        assert!(reset.ends_with("[INFO] [miyu::qq] listener ready\nplain continuation\n"));
    }

    #[test]
    fn daemon_log_formatter_recognizes_english_active_reply_decisions() {
        assert_eq!(
            active_reply_log_color("[Active reply decision: reply]\nResult: reply"),
            Some(Color::Green)
        );
        assert_eq!(
            active_reply_log_color("[Continuation decision: no reply]\nResult: no reply"),
            Some(Color::DarkGrey)
        );

        let mut color = None;
        let timestamp = format_daemon_log_line_with_state(
            "2026-07-29T12:34:56.789Z  INFO miyu::qq: ",
            true,
            &mut color,
        );
        assert!(timestamp.contains("[INFO]"));
        assert_eq!(color, None);
        let title =
            format_daemon_log_line_with_state("[Active reply decision: reply]", true, &mut color);
        assert_eq!(color, Some(Color::Green));
        assert!(title.contains('\x1b'));
        assert!(
            format_daemon_log_line_with_state("Result: reply", true, &mut color).contains('\x1b')
        );
    }

    #[test]
    fn daemon_log_stream_formatter_waits_for_complete_lines() {
        let mut formatter = DaemonLogStreamFormatter::default();
        let mut output = Vec::new();
        formatter
            .push(
                b"2026-07-29T12:34:56.789Z  INFO miyu::qq: part",
                false,
                &mut output,
            )
            .unwrap();
        assert!(output.is_empty());

        formatter
            .push(b"ial\n  continuation\nlast", false, &mut output)
            .unwrap();
        let rendered = String::from_utf8(output).unwrap();
        assert!(rendered.contains("[INFO] [miyu::qq] partial\n"));
        assert!(rendered.ends_with("  continuation\n"));

        let mut tail = Vec::new();
        formatter.finish(false, &mut tail).unwrap();
        assert_eq!(tail, b"last\n");
    }

    #[test]
    fn recent_daemon_logs_keep_multiline_order_across_rotated_files() {
        let temp = tempfile::tempdir().unwrap();
        let paths = pop_test_paths(temp.path());
        let logs_dir = paths.logs_dir();
        std::fs::create_dir_all(&logs_dir).unwrap();
        std::fs::write(
            logs_dir.join("miyu.2026-07-28.log"),
            "2026-07-28T12:00:00Z  INFO miyu::qq: old event\n  old continuation\n",
        )
        .unwrap();
        std::fs::write(
            logs_dir.join("miyu.2026-07-29.log"),
            "2026-07-29T12:00:00Z  WARN miyu::qq: new event\n  new continuation\n判断原因：保持多行\n",
        )
        .unwrap();

        let lines = recent_daemon_log_lines(&paths, 4).unwrap();
        assert_eq!(
            lines,
            [
                "  old continuation",
                "2026-07-29T12:00:00Z  WARN miyu::qq: new event",
                "  new continuation",
                "判断原因：保持多行",
            ]
        );
        let rendered = lines
            .iter()
            .map(|line| format_daemon_log_line(line, false))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(!rendered.contains('\x1b'));
        assert!(rendered.contains("[WARN] [miyu::qq] new event"));
        assert!(rendered.ends_with("  new continuation\n判断原因：保持多行"));
    }

    #[test]
    fn recent_daemon_logs_include_unstructured_daemon_stream_before_rotating_logs() {
        let temp = tempfile::tempdir().unwrap();
        let paths = pop_test_paths(temp.path());
        let logs_dir = paths.logs_dir();
        std::fs::create_dir_all(&logs_dir).unwrap();
        std::fs::write(logs_dir.join("daemon.log"), "startup banner\npanic: boom\n").unwrap();
        std::fs::write(
            logs_dir.join("miyu.2026-07-29.log"),
            "2026-07-29T12:00:00Z  INFO miyu::qq: listener ready\n",
        )
        .unwrap();

        let lines = recent_daemon_log_lines(&paths, 3).unwrap();
        assert_eq!(
            lines,
            [
                "startup banner",
                "panic: boom",
                "2026-07-29T12:00:00Z  INFO miyu::qq: listener ready",
            ]
        );
    }

    #[test]
    fn daemon_log_follow_cursor_starts_after_the_snapshot_for_each_source() {
        let temp = tempfile::tempdir().unwrap();
        let paths = pop_test_paths(temp.path());
        let logs_dir = paths.logs_dir();
        std::fs::create_dir_all(&logs_dir).unwrap();
        let fallback = logs_dir.join("daemon.log");
        let rotating = logs_dir.join("miyu.2026-07-29.log");
        std::fs::write(&fallback, b"before fallback\n").unwrap();
        std::fs::write(&rotating, b"before rotating\n").unwrap();

        let snapshot = recent_daemon_log_snapshot(&paths, 10).unwrap();
        assert_eq!(snapshot.lines, ["before fallback", "before rotating"]);
        let cursor = snapshot.cursor;
        assert_eq!(cursor.current, Some(rotating.clone()));
        assert_eq!(cursor.fallback, Some(fallback.clone()));
        assert_eq!(cursor.current_offset, 16);
        assert_eq!(cursor.fallback_offset, 16);

        let mut formatter = DaemonLogStreamFormatter::default();
        let mut output = Vec::new();
        std::fs::OpenOptions::new()
            .append(true)
            .open(&rotating)
            .unwrap()
            .write_all(b"after rotating\n")
            .unwrap();
        let mut offset = cursor.current_offset;
        assert!(
            write_daemon_log_delta(&rotating, &mut offset, &mut formatter, false, &mut output,)
                .unwrap()
        );
        formatter.finish(false, &mut output).unwrap();
        assert_eq!(String::from_utf8(output).unwrap(), "after rotating\n");
    }

    #[test]
    fn daemon_log_delta_avoids_duplicates_across_append_rotation_and_truncation() {
        fn append(path: &Path, bytes: &[u8]) {
            let mut file = std::fs::OpenOptions::new().append(true).open(path).unwrap();
            file.write_all(bytes).unwrap();
        }

        let temp = tempfile::tempdir().unwrap();
        let old = temp.path().join("miyu.2026-07-28.log");
        let current = temp.path().join("miyu.2026-07-29.log");
        std::fs::write(&old, b"old partial").unwrap();

        let mut formatter = DaemonLogStreamFormatter::default();
        let mut output = Vec::new();
        let mut old_offset = 0;
        assert!(
            write_daemon_log_delta(&old, &mut old_offset, &mut formatter, false, &mut output,)
                .unwrap()
        );
        assert!(output.is_empty());
        append(&old, b" completed\nold tail\n");
        assert!(
            write_daemon_log_delta(&old, &mut old_offset, &mut formatter, false, &mut output,)
                .unwrap()
        );
        formatter.finish(false, &mut output).unwrap();

        std::fs::write(
            &current,
            b"2026-07-29T12:00:00Z  INFO miyu::qq: first\n  continuation\n",
        )
        .unwrap();
        let mut offset = 0;
        assert!(
            write_daemon_log_delta(&current, &mut offset, &mut formatter, false, &mut output,)
                .unwrap()
        );
        assert!(
            !write_daemon_log_delta(&current, &mut offset, &mut formatter, false, &mut output,)
                .unwrap()
        );

        append(&current, b"2026-07-29T12:00:01Z  INFO miyu::qq: \xe7\xbe");
        assert!(
            write_daemon_log_delta(&current, &mut offset, &mut formatter, false, &mut output,)
                .unwrap()
        );
        append(&current, b"\xa4\xe8\x81\x8a\n");
        assert!(
            write_daemon_log_delta(&current, &mut offset, &mut formatter, false, &mut output,)
                .unwrap()
        );

        append(&current, b"dangling");
        assert!(
            write_daemon_log_delta(&current, &mut offset, &mut formatter, false, &mut output,)
                .unwrap()
        );
        std::fs::write(&current, b"reset\n").unwrap();
        assert!(
            write_daemon_log_delta(&current, &mut offset, &mut formatter, false, &mut output,)
                .unwrap()
        );
        assert_eq!(offset, 6);

        let rendered = String::from_utf8(output).unwrap();
        assert!(!rendered.contains('\x1b'));
        assert_eq!(rendered.matches("old partial completed").count(), 1);
        assert_eq!(rendered.matches("old tail").count(), 1);
        assert_eq!(rendered.matches("[INFO] [miyu::qq] first").count(), 1);
        assert_eq!(rendered.matches("  continuation").count(), 1);
        assert_eq!(rendered.matches("[INFO] [miyu::qq] 群聊").count(), 1);
        assert_eq!(rendered.matches("dangling").count(), 1);
        assert_eq!(rendered.matches("reset").count(), 1);
    }

    #[test]
    fn pop_is_a_cli_subcommand_with_an_optional_count() {
        let cli = parse_args(["miyu", "pop"].map(OsString::from).to_vec()).unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::Pop(PopArgs { count: None }))
        ));

        let cli = parse_args(["miyu", "pop", "3"].map(OsString::from).to_vec()).unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::Pop(PopArgs { count: Some(3) }))
        ));
        assert!(parse_args(["miyu", "pop", "0"].map(OsString::from).to_vec()).is_err());
        assert!(parse_args(["miyu", "pop", "nope"].map(OsString::from).to_vec()).is_err());
    }

    #[test]
    fn repl_pop_accepts_zero_or_one_positive_integer() {
        assert_eq!(parse_repl_pop_count("").unwrap(), None);
        assert_eq!(parse_repl_pop_count(" 3 ").unwrap(), Some(3));
        assert!(parse_repl_pop_count("0").is_err());
        assert!(parse_repl_pop_count("nope").is_err());
        assert!(parse_repl_pop_count("1 2").is_err());
    }

    #[test]
    fn counted_pop_removes_oldest_turns_and_caps_at_available_count() {
        let temp = tempfile::tempdir().unwrap();
        let paths = pop_test_paths(temp.path());
        let config = AppConfig::default();
        let state = StateStore::new(&paths).unwrap();
        for id in ["t1", "t2", "t3"] {
            state.start_turn(id, id, 999999).unwrap();
            state.complete_turn(id, "reply", None).unwrap();
        }

        let first = execute_pop(&paths, &config, &state, Some(2))
            .unwrap()
            .unwrap();
        assert_eq!(first.turns, 2);
        assert_eq!(
            state
                .load_visible_turns()
                .unwrap()
                .into_iter()
                .map(|turn| turn.turn_id)
                .collect::<Vec<_>>(),
            vec!["t3"]
        );

        let second = execute_pop(&paths, &config, &state, Some(99))
            .unwrap()
            .unwrap();
        assert_eq!(second.turns, 1);
        assert!(state.load_visible_turns().unwrap().is_empty());
    }

    #[test]
    fn pop_menu_uses_three_lines_without_context_metadata() {
        let turn = sample_pop_turn(TurnStatus::Completed);
        let lines = pop_menu_turn_lines(&turn, true, false, 80)
            .map(|line| strip_terminal_control_sequences(&line));

        assert_eq!(lines[0], "› [ ] 2026-07-19 10:42");
        assert!(lines[1].contains("first prompt line"));
        assert!(!lines[1].contains("second prompt line"));
        assert!(lines[2].contains("first answer line"));
        assert!(!lines[2].contains("second answer line"));
        let joined = lines.join(" ");
        assert!(!joined.contains("hidden tool report"));
        assert!(!joined.contains("private reasoning"));
        assert!(lines.iter().all(|line| visible_width(line) <= 80));
    }

    #[test]
    fn pop_menu_labels_an_interrupted_reply_without_showing_the_reminder() {
        let mut turn = sample_pop_turn(TurnStatus::Interrupted);
        turn.assistant_content = crate::state::interrupted_text().to_string();
        let lines = pop_menu_turn_lines(&turn, false, true, 80)
            .map(|line| strip_terminal_control_sequences(&line));

        assert!(lines[2].contains("中断") || lines[2].contains("interrupted"));
        assert!(!lines[2].contains("system-reminder"));
    }

    #[test]
    fn pop_menu_footer_has_controls_but_no_position_counter() {
        let help = strip_terminal_control_sequences(&pop_menu_help_line(120));
        assert!(help.contains("Tab"));
        assert!(help.contains("Enter"));
        assert!(!help.contains("3 / 8"));

        let header = strip_terminal_control_sequences(&pop_menu_header("", 2, 8, 80));
        assert!(header.contains("2 / 8"));
    }

    #[test]
    fn filtered_pop_turns_keep_oldest_first_order() {
        let matcher = SkimMatcherV2::default();
        let items = vec![
            "old matching prompt".to_string(),
            "middle unrelated".to_string(),
            "new matching prompt".to_string(),
        ];

        assert_eq!(pop_matches(&matcher, &items, "matching"), vec![0, 2]);
    }

    #[test]
    fn debug_is_a_global_cli_option() {
        for args in [
            &["miyu", "--debug", "models", "1"][..],
            &["miyu", "models", "--debug", "1"][..],
            &["miyu", "hello", "--debug"][..],
            &["miyu", "ask", "hello", "--debug"][..],
        ] {
            let cli = parse_args(args.iter().map(OsString::from).collect()).unwrap();
            assert!(cli.debug);
        }

        let cli = parse_args(["miyu", "hello", "--debug"].map(OsString::from).to_vec()).unwrap();
        assert_eq!(cli.message, ["hello"]);

        let cli = parse_args(["miyu", "--", "--debug"].map(OsString::from).to_vec()).unwrap();
        assert!(!cli.debug);
        assert_eq!(cli.message, ["--debug"]);
    }

    #[test]
    fn footer_reset_clears_turn_and_cumulative_tokens() {
        let config = AppConfig::default();
        let mut footer = ReplFooterStatus::from_config(
            &config,
            100,
            TurnTokens {
                total: 250,
                ..Default::default()
            },
        );
        footer.set_token_usage(
            50,
            100,
            Some(200_000),
            TurnTokens {
                total: 250,
                ..Default::default()
            },
        );

        footer.reset_token_usage(0, Some(200_000));

        assert_eq!(footer.token_usage.turn_tokens, 0);
        assert_eq!(footer.token_usage.session_tokens, 0);
        assert_eq!(footer.token_usage.context_window, Some(200_000));
        assert_eq!(footer.token_usage.cumulative_tokens, None);

        footer.reset_token_usage(0, None);
        assert_eq!(footer.token_usage.context_window, None);
    }

    #[test]
    fn footer_turn_completion_updates_the_rendered_token_accounting() {
        let config = AppConfig::default();
        let mut footer = ReplFooterStatus::from_config(&config, 0, TurnTokens::default());
        let result = ChatResult {
            content: "reply".to_string(),
            reasoning: None,
            usage: Some(Usage {
                prompt_tokens: 80,
                completion_tokens: 20,
                total_tokens: 100,
                ..Usage::default()
            }),
            usage_estimated: false,
            tool_calls: Vec::new(),
            provider_id: None,
            model: None,
            finish_reason: None,
            thinking_signature: None,
            last_request_usage: None,
            responses_continuation: None,
        };

        footer.update_token_usage(
            &result,
            240,
            Some(200_000),
            TurnTokens {
                total: 100,
                ..Default::default()
            },
        );

        assert_eq!(footer.token_usage.turn_tokens, 100);
        assert_eq!(footer.token_usage.session_tokens, 240);
        assert_eq!(footer.token_usage.cumulative_tokens, Some(100));
        assert_eq!(
            strip_terminal_control_sequences(&repl_footer_line(AgentMode::Normal, &footer, 80))
                .split_whitespace()
                .last(),
            Some("Σ100")
        );
    }

    #[test]
    fn an_idle_tick_only_redraws_when_the_cumulative_actually_moved() {
        let config = AppConfig::default();
        let mut footer = ReplFooterStatus::from_config(&config, 0, TurnTokens::default());
        let totals = TurnTokens {
            total: 32_808,
            prompt: 29_035,
            cache_read: 17_664,
        };
        assert!(footer.update_cumulative_tokens(totals));
        // The jobs poll republishes the same Σ every second; redrawing the
        // whole tail on each of those would fight the strip animation.
        assert!(!footer.update_cumulative_tokens(totals));

        // A background subagent finishing moves only the cache halves — the
        // total can stay put when its usage was estimated, so equality has to
        // consider all three.
        assert!(footer.update_cumulative_tokens(TurnTokens {
            cache_read: 20_000,
            ..totals
        }));
    }

    #[test]
    fn the_footer_leaves_the_per_turn_figure_to_the_token_line() {
        let config = AppConfig::default();
        let mut footer = ReplFooterStatus::from_config(&config, 0, TurnTokens::default());
        footer.set_token_usage_with_cache(
            TurnTokens {
                total: 21_224,
                prompt: 16_139,
                cache_read: 6_528,
            },
            21_700,
            Some(1_000_000),
            TurnTokens {
                total: 180_100,
                prompt: 47_538,
                cache_read: 11_392,
            },
        );

        let line =
            strip_terminal_control_sequences(&repl_footer_line(AgentMode::Normal, &footer, 80));
        // Two standing gauges only. Carrying the turn figure as well cost 14
        // columns and pushed the whole footer past 80.
        assert!(line.contains("21.7k/1M(2.2%)"), "{line}");
        assert!(line.contains("Σ180.1k(C24%)"), "{line}");
        assert!(!line.contains("21.2k"), "{line}");
        assert!(!line.contains("C40%"), "{line}");
        assert!(
            visible_width(&line) <= 80,
            "footer must fit 80 columns: {} — {line}",
            visible_width(&line)
        );
    }

    #[test]
    fn footer_variant_always_uses_the_fixed_primary_color() {
        let config = AppConfig::default();
        let mut footer = ReplFooterStatus::from_config(&config, 0, TurnTokens::default());
        footer.update_thinking_variant(Some("high"));

        for mode in [AgentMode::Normal, AgentMode::Chat] {
            let line = repl_footer_left(mode, &footer, 120);
            assert!(line.contains("\x1b[1m\x1b[34mhigh\x1b[0m"));
            assert_eq!(
                strip_terminal_control_sequences(&line),
                format!(
                    "{} · {} {} · high",
                    mode.label(),
                    footer.model,
                    footer.provider
                )
            );
        }
    }

    #[test]
    fn mixed_footer_uses_dim_provider_and_hides_global_variant() {
        let mut config = AppConfig::default();
        let provider = config
            .providers
            .iter_mut()
            .find(|provider| !provider.models.is_empty())
            .unwrap();
        let provider_id = provider.id.clone();
        let first_model = provider.models[0].clone();
        let second_model = "footer-second-model".to_string();
        provider.models.push(second_model.clone());
        config.active_provider_models = Some(vec![
            ActiveProviderModelConfig {
                provider_id: provider_id.clone(),
                model: first_model,
            },
            ActiveProviderModelConfig {
                provider_id,
                model: second_model,
            },
        ]);
        let mut footer = ReplFooterStatus::from_config(&config, 0, TurnTokens::default());
        footer.update_thinking_variant(Some("mixed"));

        let line = repl_footer_left(AgentMode::Normal, &footer, 120);

        assert_eq!(footer.provider, "mixed");
        assert!(footer.thinking.is_none());
        assert_eq!(
            strip_terminal_control_sequences(&line),
            format!(
                "{} · {} mixed",
                AgentMode::Normal.label(),
                t("Mixed", "混合")
            )
        );
        assert!(line.contains("\x1b[2mmixed\x1b[0m"));
        assert!(!line.contains(&primary_footer_text("mixed")));
    }

    #[test]
    fn committed_user_message_keeps_one_blank_line_before_output() {
        let output = committed_user_messages_text(&[("hello", AgentMode::Normal)], true, 80);

        assert_eq!(
            strip_terminal_control_sequences(&output),
            "\n┃\n┃ hello\n┃\n\n"
        );
    }

    #[test]
    fn queued_message_uses_full_height_bar_and_primary_status() {
        let prompt = QueuedPrompt {
            prompt_id: "q1".to_string(),
            seq: 1,
            content: "follow up".to_string(),
            display_content: "follow up".to_string(),
            attachments: Vec::new(),
            uploaded_attachments: Vec::new(),
            submitted_at: String::new(),
        };

        let normal = queued_prompt_lines(std::slice::from_ref(&prompt), AgentMode::Normal, 80);
        let chat = queued_prompt_lines(&[prompt], AgentMode::Chat, 80);

        assert_eq!(normal.len(), 4);
        assert_eq!(normal[0], submitted_echo_bar(AgentMode::Normal));
        assert_eq!(normal[2], submitted_echo_bar(AgentMode::Normal));
        assert!(normal[3].starts_with(&submitted_echo_bar(AgentMode::Normal)));
        assert!(normal[3].contains(&primary_footer_text(t("Queued", "排队中"))));
        assert!(chat
            .iter()
            .filter(|line| !line.is_empty())
            .all(|line| line.starts_with(&submitted_echo_bar(AgentMode::Chat))));
        assert_ne!(normal[0], chat[0]);
    }

    #[test]
    fn live_tail_moves_naturally_and_releases_after_output_shrinks() {
        assert_eq!(max_live_tail_start(6, 5), 0);
        assert_eq!(max_live_tail_start(24, 5), 18);
        assert_eq!(
            live_tail_placement(0, 4, 5, 24, false),
            LiveTailPlacement {
                output_row: 4,
                tail_start: 4,
                overflow: 0,
                anchored: false,
            }
        );
        assert_eq!(
            live_tail_placement(0, 20, 5, 24, false),
            LiveTailPlacement {
                output_row: 18,
                tail_start: 18,
                overflow: 2,
                anchored: true,
            }
        );
        assert_eq!(
            live_tail_placement(0, 6, 5, 24, false),
            LiveTailPlacement {
                output_row: 6,
                tail_start: 6,
                overflow: 0,
                anchored: false,
            }
        );
        assert_eq!(live_tail_placement(0, 6, 5, 30, false).tail_start, 6);
    }

    #[test]
    fn anchored_tail_stays_at_the_bottom_when_it_shrinks() {
        // A job strip pushed a bottom-anchored 5-row tail to 7 rows, scrolling
        // the screen twice, so output now ends at row 16. The strip goes away.
        let shrunk = live_tail_placement(0, 16, 5, 24, true);
        assert_eq!(
            shrunk,
            LiveTailPlacement {
                // Stays where the output really ended: the renderer's spinner
                // erases itself relative to this cursor.
                output_row: 16,
                tail_start: 18,
                overflow: 0,
                anchored: true,
            }
        );
        // Bottom edge back on the last usable row, where it was before.
        assert_eq!(shrunk.tail_start + 5, 24 - 1);

        // Without the anchor the tail hugs the output cursor as before: a
        // conversation that has not filled the screen is untouched.
        assert_eq!(
            live_tail_placement(0, 16, 5, 24, false),
            LiveTailPlacement {
                output_row: 16,
                tail_start: 16,
                overflow: 0,
                anchored: false,
            }
        );

        // Growing while anchored still scrolls rather than double-counting.
        assert_eq!(
            live_tail_placement(0, 18, 7, 24, true),
            LiveTailPlacement {
                output_row: 16,
                tail_start: 16,
                overflow: 2,
                anchored: true,
            }
        );
    }

    #[test]
    fn streaming_output_never_drags_an_anchored_tail_back_up() {
        // 24 rows, 5-row tail → anchored at 18. Output ends two rows above it
        // because a job strip just went away; the frame must leave the tail
        // alone and fill the gap instead of reclaiming those rows.
        let max_tail = max_live_tail_start(24, 5);
        assert_eq!(max_tail, 18);
        assert_eq!(live_tail_next_start(18, 16, max_tail), 18);
        // Still pinned once the gap is closed.
        assert_eq!(live_tail_next_start(18, 18, max_tail), 18);
        // And it never runs past the anchor.
        assert_eq!(live_tail_next_start(18, 21, max_tail), 18);

        // A tail that had not reached the bottom keeps following the output.
        assert_eq!(live_tail_next_start(10, 12, max_tail), 12);
        assert_eq!(live_tail_next_start(10, 8, max_tail), 8);
        assert_eq!(live_tail_next_start(10, 30, max_tail), 18);
    }

    #[test]
    fn live_editor_restores_clear_screen_and_double_escape_controls() {
        let temp = tempfile::tempdir().unwrap();
        let paths = pop_test_paths(temp.path());
        let escape = || Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        let mut editor = LiveReplEditor::new(AgentMode::Normal, Vec::new());
        editor.input = "draft".to_string();
        assert!(matches!(
            editor.handle_event(escape(), &paths, true).unwrap(),
            LiveEditorAction::Redraw
        ));
        // Arming the interrupt must not clear the typed draft.
        assert_eq!(editor.input, "draft");
        assert!(matches!(
            editor.handle_event(escape(), &paths, true).unwrap(),
            LiveEditorAction::Interrupt
        ));
        assert_eq!(editor.input, "draft");

        let clear = Event::Key(KeyEvent::new(KeyCode::Char('l'), KeyModifiers::CONTROL));
        assert!(matches!(
            editor.handle_event(clear, &paths, true).unwrap(),
            LiveEditorAction::ClearScreen
        ));

        assert!(matches!(
            editor.handle_event(escape(), &paths, false).unwrap(),
            LiveEditorAction::Redraw
        ));
        assert!(matches!(
            editor.handle_event(escape(), &paths, false).unwrap(),
            LiveEditorAction::Redraw
        ));
        // Esc no longer clears drafts anywhere; empty the editor manually
        // before asserting the empty-submit path.
        assert_eq!(editor.input, "draft");
        editor.clear();

        assert!(matches!(
            editor
                .handle_event(
                    Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
                    &paths,
                    false,
                )
                .unwrap(),
            LiveEditorAction::EmptySubmit
        ));
        assert!(editor.history.is_empty());

        editor.input = "/help".to_string();
        editor.cursor = editor.input.chars().count();
        assert!(matches!(
            editor
                .handle_event(
                    Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
                    &paths,
                    false,
                )
                .unwrap(),
            LiveEditorAction::Submit(_)
        ));
        assert!(editor.history.is_empty());
        editor.record_history("ordinary prompt");
        assert_eq!(editor.history, ["ordinary prompt"]);
    }

    #[test]
    fn live_editor_shift_enter_inserts_newline_without_submit() {
        let temp = tempfile::tempdir().unwrap();
        let paths = pop_test_paths(temp.path());
        let mut editor = LiveReplEditor::new(AgentMode::Normal, Vec::new());
        editor.input = "hello".to_string();
        editor.cursor = 5;
        assert!(matches!(
            editor
                .handle_event(
                    Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT)),
                    &paths,
                    false,
                )
                .unwrap(),
            LiveEditorAction::Redraw
        ));
        assert_eq!(editor.input, "hello\n");
        assert_eq!(editor.cursor, 6);

        assert!(matches!(
            editor
                .handle_event(
                    Event::Key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL)),
                    &paths,
                    false,
                )
                .unwrap(),
            LiveEditorAction::Redraw
        ));
        assert_eq!(editor.input, "hello\n\n");
        assert_eq!(editor.cursor, 7);
    }

    #[test]
    fn spinner_does_not_resume_tail_during_external_output() {
        let config = AppConfig::default();
        let mut live = LiveReplTail {
            editor: LiveReplEditor::new(AgentMode::Normal, Vec::new()),
            queued: Vec::new(),
            pending_chunks: Vec::new(),
            footer: ReplFooterStatus::from_config(&config, 0, TurnTokens::default()),
            output_cursor: (0, 0),
            tail_start: 0,
            tail_rows: 0,
            input_cursor: (0, 0),
            rendered: false,
            external_output_active: true,
            raw_mode_handoff: false,
            jobs: Vec::new(),
            job_spinner: 0,
        };
        let mut renderer = render::StreamRenderer::new(
            render::ReasoningDisplayMode::Hidden,
            render::ToolCallDisplayMode::Hidden,
            true,
            true,
            10,
        );

        handle_live_agent_event(&mut live, &mut renderer, AgentEvent::SpinnerTick).unwrap();

        assert!(live.external_output_active);
        assert!(!live.rendered);
    }

    #[test]
    fn live_tail_coalesces_adjacent_stream_chunks_and_can_discard_them() {
        let config = AppConfig::default();
        let mut live = LiveReplTail {
            editor: LiveReplEditor::new(AgentMode::Normal, Vec::new()),
            queued: Vec::new(),
            pending_chunks: Vec::new(),
            footer: ReplFooterStatus::from_config(&config, 0, TurnTokens::default()),
            output_cursor: (0, 0),
            tail_start: 0,
            tail_rows: 0,
            input_cursor: (0, 0),
            rendered: false,
            external_output_active: false,
            raw_mode_handoff: false,
            jobs: Vec::new(),
            job_spinner: 0,
        };

        for (kind, text) in [
            (ChatStreamKind::Reasoning, "one"),
            (ChatStreamKind::Reasoning, " two"),
            (ChatStreamKind::Content, "answer"),
            (ChatStreamKind::Content, " text"),
        ] {
            live.queue_stream_chunk(ChatStreamChunk {
                kind,
                text: text.to_string(),
            });
        }

        assert_eq!(live.pending_chunks.len(), 2);
        assert_eq!(live.pending_chunks[0].text, "one two");
        assert_eq!(live.pending_chunks[1].text, "answer text");
        live.discard_pending_chunks();
        assert!(live.pending_chunks.is_empty());
    }

    #[test]
    fn prompt_rows_wrap_at_terminal_width() {
        assert_eq!(repl_prompt_rows_for_cols("", &["1234567".into()], 10), 1);
        assert_eq!(repl_prompt_rows_for_cols("", &["1234567890".into()], 10), 2);
        assert_eq!(
            repl_prompt_rows_for_cols("", &["123".into(), "456".into()], 10),
            2
        );
    }

    #[test]
    fn cursor_position_wraps_at_terminal_width() {
        assert_eq!(repl_cursor_position_for_cols("", "1234567", 7, 10), (7, 0));
        assert_eq!(
            repl_cursor_position_for_cols("", "1234567890", 10, 10),
            (0, 1)
        );
        assert_eq!(repl_cursor_position_for_cols("", "123\n456", 7, 10), (3, 1));
        assert_eq!(repl_cursor_position_for_cols("", "1234567", 3, 10), (3, 0));
    }

    #[test]
    fn cursor_position_keeps_prefix_after_newline() {
        assert_eq!(repl_cursor_position_for_cols("  ", "123\n", 4, 10), (2, 1));
        assert_eq!(
            repl_cursor_position_for_cols("  ", "123\n456", 7, 10),
            (5, 1)
        );
    }

    #[test]
    fn prompt_rows_include_prefix_on_each_line() {
        assert_eq!(
            repl_prompt_rows_for_cols("  ", &["12".into(), "34".into()], 5),
            2
        );
        assert_eq!(
            repl_prompt_rows_for_cols("  ", &["123".into(), "34".into()], 5),
            3
        );
    }

    #[test]
    fn wrapped_input_rows_keep_prefix_outside_content_width() {
        assert_eq!(
            repl_wrapped_input_rows_for_cols("  ", &["123456789".into()], 10),
            vec!["12345678".to_string(), "9".to_string()]
        );
        assert_eq!(
            repl_wrapped_input_rows_for_cols("  ", &["12345678".into()], 10),
            vec!["12345678".to_string(), String::new()]
        );
        assert_eq!(
            repl_cursor_position_for_cols("  ", "12345678", 8, 10),
            (2, 1)
        );
    }

    #[test]
    fn history_browsing_requires_empty_or_clean_history_input() {
        let history = vec!["first".to_string(), "second".to_string()];

        assert!(repl_should_browse_history("", &history, None));
        assert!(repl_should_browse_history("second", &history, Some(1)));
        assert!(!repl_should_browse_history("draft", &history, None));
        assert!(!repl_should_browse_history(
            "second edited",
            &history,
            Some(1)
        ));
    }

    #[test]
    fn vertical_cursor_move_uses_soft_wrapped_rows() {
        assert_eq!(
            repl_move_cursor_vertical_for_cols("  ", "123456789", 9, -1, 10),
            1
        );
        assert_eq!(
            repl_move_cursor_vertical_for_cols("  ", "123456789", 1, 1, 10),
            9
        );
    }

    #[test]
    fn vertical_cursor_move_handles_explicit_newlines() {
        assert_eq!(
            repl_move_cursor_vertical_for_cols("  ", "abc\ndef", 6, -1, 20),
            2
        );
        assert_eq!(
            repl_move_cursor_vertical_for_cols("  ", "abc\ndef", 2, 1, 20),
            6
        );
    }

    #[test]
    fn vertical_cursor_move_handles_wide_chars_near_wrap() {
        assert_eq!(
            repl_cursor_position_for_cols("  ", "1234567你", 8, 11),
            (2, 1)
        );
        assert_eq!(
            repl_cursor_position_for_cols("  ", "12345678你", 9, 11),
            (4, 1)
        );
        assert_eq!(
            repl_move_cursor_vertical_for_cols("  ", "12345678你好", 9, -1, 11),
            2
        );
    }

    #[test]
    fn reset_is_a_repl_command() {
        assert!(repl_commands().contains(&"/reset"));
    }

    #[test]
    fn compact_is_a_repl_command() {
        assert!(repl_commands().contains(&"/compact"));
    }

    #[test]
    fn pop_is_a_repl_command_with_an_optional_count() {
        assert!(repl_commands().contains(&"/pop"));
        assert_eq!(split_repl_command("/pop 3"), ("/pop", "3"));
        // "/p" became ambiguous once /persona joined the table.
        assert_eq!(resolve_repl_command("/po"), "/pop");
        assert_eq!(resolve_repl_command("/pe"), "/persona");
    }

    #[test]
    fn usage_and_persona_are_repl_commands() {
        assert!(repl_commands().contains(&"/usage"));
        assert!(repl_commands().contains(&"/persona"));
        assert_eq!(resolve_repl_command("/us"), "/usage");
        assert_eq!(split_repl_command("/persona Alice.md"), ("/persona", "Alice.md"));
    }

    #[test]
    fn variant_is_a_repl_command_with_arguments() {
        assert!(repl_commands().contains(&"/variant"));
        assert_eq!(split_repl_command("/variant high"), ("/variant", "high"));
        assert_eq!(split_repl_command("/reset all"), ("/reset", "all"));
        assert_eq!(resolve_repl_command("/var"), "/variant");
    }

    #[test]
    fn variant_menu_checks_pending_selection_before_confirming() {
        let options = ThinkingVariantOptions {
            provider_id: "ririxin".to_string(),
            model: "deepseek-v4-flash".to_string(),
            variants: vec!["high".to_string(), "max".to_string()],
            selected: Some("high".to_string()),
        };
        let mut item = VariantMenuItem::from_options(&options);
        assert_eq!(
            item.options
                .iter()
                .map(|option| option.label.as_str())
                .collect::<Vec<_>>(),
            vec!["default", "high", "max"]
        );
        assert_eq!(item.selection().2.as_deref(), Some("high"));

        item.cursor = 2;
        assert_eq!(item.selection().2.as_deref(), Some("high"));
        item.check_cursor();
        assert_eq!(item.selection().2.as_deref(), Some("max"));
    }

    #[test]
    fn single_variant_menu_uses_content_width() {
        let item = VariantMenuItem::from_options(&ThinkingVariantOptions {
            provider_id: "ririxin".to_string(),
            model: "deepseek-v4-flash".to_string(),
            variants: vec!["high".to_string(), "max".to_string()],
            selected: None,
        });

        assert!(single_variant_content_width(&item) < 30);
    }

    #[test]
    fn mixed_variant_columns_do_not_fill_wide_terminal() {
        let items = ["myopencode", "myopencode6"]
            .into_iter()
            .map(|provider_id| {
                VariantMenuItem::from_options(&ThinkingVariantOptions {
                    provider_id: provider_id.to_string(),
                    model: "deepseek-v4-flash-free".to_string(),
                    variants: vec!["high".to_string(), "max".to_string()],
                    selected: None,
                })
            })
            .collect::<Vec<_>>();

        let (left, right) = variant_menu_column_widths(&items, 120);
        assert!(left + right < 80);
        assert!(left >= visible_width("myopencode6 / deepseek-v4-flash-free") + 2);
        assert!(right >= visible_width("[*] default") + 2);
    }

    #[test]
    fn mixed_endpoint_label_only_omits_unset_variant() {
        assert_eq!(
            mixed_model_endpoint_label("provider", "model", None),
            "provider / model"
        );
        assert_eq!(
            mixed_model_endpoint_label("provider", "model", Some("default")),
            "provider / model · default"
        );
        assert_eq!(
            mixed_model_endpoint_label("provider", "model", Some("high")),
            "provider / model · high"
        );
    }

    #[test]
    fn variant_menu_distinguishes_unset_from_default_effort() {
        let options = ThinkingVariantOptions {
            provider_id: "groq".to_string(),
            model: "qwen/qwen3-32b".to_string(),
            variants: vec!["none".to_string(), "default".to_string()],
            selected: Some("default".to_string()),
        };
        let item = VariantMenuItem::from_options(&options);

        assert_eq!(item.options[0].label, "default");
        assert_eq!(item.options[0].value, None);
        assert_eq!(item.options[2].label, "default (variant)");
        assert_eq!(item.options[2].value.as_deref(), Some("default"));
        assert_eq!(item.selected, 2);
        assert_eq!(item.selection().2.as_deref(), Some("default"));
    }

    #[test]
    fn explicit_variant_prefix_can_select_default_effort() {
        let argument = "variant:default";
        assert_eq!(argument.strip_prefix("variant:"), Some("default"));
        assert_ne!(argument, "default");
    }

    #[test]
    fn variant_name_resolution_handles_default_and_case_insensitive_names() {
        let available = vec!["low".to_string(), "high".to_string(), "default".to_string()];

        assert_eq!(
            resolve_variant_name("HIGH", &available).unwrap(),
            Some("high".into())
        );
        assert_eq!(resolve_variant_name("default", &available).unwrap(), None);
        assert_eq!(
            resolve_variant_name("variant:default", &available).unwrap(),
            Some("default".into())
        );
        assert!(resolve_variant_name("unknown", &available).is_err());
        assert!(resolve_variant_name("Variant:default", &available).is_err());
    }

    #[test]
    fn command_suggestions_are_prefixed_and_truncated() {
        let suggestions = repl_command_suggestions("/");
        let line = repl_command_suggestions_line(&suggestions, 24);
        assert!(line.starts_with("/new"));
        assert!(visible_width(&line) <= 24);

        let line = repl_command_suggestions_line(&["/compact"], 40);
        assert_eq!(line, "/compact");
    }

    #[test]
    fn truncation_respects_very_narrow_widths() {
        assert_eq!(truncate_visible_width("abcdef", 0), "");
        assert_eq!(truncate_visible_width("abcdef", 1), ".");
        assert_eq!(truncate_visible_width("abcdef", 2), "..");
        assert_eq!(truncate_visible_width("abcdef", 3), "...");
    }

    #[test]
    fn shortcut_hint_line_is_bar_aligned_and_truncated() {
        let line = repl_shortcut_hint_line(AgentMode::Normal, 24);
        assert!(strip_terminal_control_sequences(&line).contains("Tab"));
        assert!(visible_width(&line) <= 24);
    }

    #[test]
    fn inline_fuzzy_lines_are_bar_aligned_and_truncated() {
        let header = inline_fuzzy_header("big", 12);
        assert!(strip_terminal_control_sequences(&header).contains(t("Select", "选择模型")));
        assert!(visible_width(&header) <= 12);

        let item = inline_fuzzy_item_line("opencode Zen / big-pickle", true, false, 16);
        let item_plain = strip_terminal_control_sequences(&item);
        assert!(item_plain.starts_with("› [ ]"));
        assert!(item_plain.contains("open"));
        assert!(visible_width(&item) <= 16);

        let item = inline_fuzzy_item_line("opencode Zen / big-pickle", false, true, 18);
        let item_plain = strip_terminal_control_sequences(&item);
        assert!(item_plain.starts_with("  [*]"));
        assert!(item_plain.contains("opencode"));
        assert!(visible_width(&item) <= 18);

        let help = inline_fuzzy_help_line(40);
        let help_plain = strip_terminal_control_sequences(&help);
        assert!(help_plain.contains("j/k"));
        assert!(visible_width(&help) <= 40);
    }

    #[test]
    fn session_selection_defaults_to_the_current_entry() {
        let entry = |id: &str, is_current: bool| SessionListEntry {
            id: id.to_string(),
            name: id.to_string(),
            is_current,
            turns: 0,
            snippet: String::new(),
            workspace: None,
        };
        let entries = vec![entry("default", true), entry("active", false)];

        assert_eq!(session_initial_selection(&entries, Some("active")), 1);
        assert_eq!(session_initial_selection(&entries, None), 0);
        assert!(matches!(
            session_ref_from_index(&entries, 2),
            Some(crate::ipc::SessionRef::Id { id }) if id == "active"
        ));
        assert_eq!(session_initial_selection(&[entry("only", false)], None), 0);
        assert!(!session_list_line(1, &entries[0], false).contains('\x1b'));
    }

    #[test]
    fn wipe_is_its_own_command_not_a_suffix_on_reset() {
        // `/reset` and `/reset all` differed by one word and by everything
        // else: one starts a conversation over, the other erased memory, every
        // session and the generated skills. They answer under separate names
        // now, and `/wipe` is far enough from `/w…` prefixes to be typed on
        // purpose.
        assert!(matches!(
            parse_repl_input("/wipe"),
            ReplInput::Slash(ReplSlashCommand::Wipe, "")
        ));
        assert!(matches!(
            parse_repl_input("/reset"),
            ReplInput::Slash(ReplSlashCommand::Reset, "")
        ));
        assert!(matches!(
            parse_repl_input("/reset all"),
            ReplInput::Slash(ReplSlashCommand::Reset, "all")
        ));
    }

    #[test]
    fn partial_slash_command_resolves_unique_match() {
        assert_eq!(resolve_repl_command("/model"), "/models");
        assert_eq!(resolve_repl_command("/compa"), "/compact");
        assert_eq!(resolve_repl_command("/co"), "/co");
        assert_eq!(resolve_repl_command("hello"), "hello");
    }

    #[test]
    fn parse_repl_input_dispatches_by_table() {
        assert!(matches!(parse_repl_input("hello"), ReplInput::Chat));
        assert!(matches!(
            parse_repl_input("/models"),
            ReplInput::Slash(ReplSlashCommand::Models, "")
        ));
        // Unique prefix resolves.
        assert!(matches!(
            parse_repl_input("/compa"),
            ReplInput::Slash(ReplSlashCommand::Compact, "")
        ));
        // Exact match wins over ambiguous prefixes of longer names.
        assert!(matches!(
            parse_repl_input("/reset all"),
            ReplInput::Slash(ReplSlashCommand::Reset, "all")
        ));
        // Case-insensitive.
        assert!(matches!(
            parse_repl_input("/POP 3"),
            ReplInput::Slash(ReplSlashCommand::Pop, "3")
        ));
        // Ambiguous prefix stays unknown.
        assert!(matches!(
            parse_repl_input("/co"),
            ReplInput::UnknownSlash("/co")
        ));
        assert!(matches!(
            parse_repl_input("/nope"),
            ReplInput::UnknownSlash("/nope")
        ));
    }

    #[test]
    fn every_repl_slash_command_has_a_table_entry() {
        // repl_command_spec panics on a missing entry; touch every variant.
        for spec in REPL_COMMAND_TABLE {
            assert_eq!(repl_command_spec(spec.command).command, spec.command);
        }
    }

    #[test]
    fn drain_stdin_does_not_panic() {
        drain_stdin();
    }

    #[test]
    fn input_helpers_edit_at_cursor() {
        let mut input = "abcd".to_string();
        let mut cursor = 2;
        insert_char_at_cursor(&mut input, &mut cursor, '中');
        assert_eq!(input, "ab中cd");
        assert_eq!(cursor, 3);

        remove_char_before_cursor(&mut input, &mut cursor);
        assert_eq!(input, "abcd");
        assert_eq!(cursor, 2);

        remove_char_at_cursor(&mut input, cursor);
        assert_eq!(input, "abd");
        assert_eq!(cursor, 2);
    }

    #[test]
    fn input_helpers_remove_word_before_cursor() {
        let mut input = "hello world  ".to_string();
        let mut cursor = input.chars().count();
        remove_word_before_cursor(&mut input, &mut cursor);
        assert_eq!(input, "hello ");
        assert_eq!(cursor, 6);

        let mut input = "前面 中间 后面".to_string();
        let mut cursor = 6;
        remove_word_before_cursor(&mut input, &mut cursor);
        assert_eq!(input, "前面 后面");
        assert_eq!(cursor, 3);
    }

    #[test]
    fn input_helpers_insert_paste_at_cursor() {
        let mut input = "前后".to_string();
        let mut cursor = 1;
        insert_str_at_cursor(&mut input, &mut cursor, "中间");
        assert_eq!(input, "前中间后");
        assert_eq!(cursor, 3);
    }

    #[test]
    fn input_helpers_insert_newline_at_cursor() {
        let mut input = "前后".to_string();
        let mut cursor = 1;
        insert_newline_at_cursor(&mut input, &mut cursor);
        assert_eq!(input, "前\n后");
        assert_eq!(cursor, 2);
    }

    #[test]
    fn long_paste_visible_lines_are_collapsed() {
        let lines = (0..20)
            .map(|index| format!("line {index}"))
            .collect::<Vec<_>>();
        let visible = repl_visible_input_lines("[NORMAL] > ", &lines, 12, true);

        assert_eq!(visible.len(), 3);
        assert_eq!(visible[0], "line 0");
        assert!(visible[1].contains("18") || visible[1].contains("已隐藏 18"));
        assert_eq!(visible[2], "line 19");
        assert_eq!(lines.len(), 20);
    }

    #[test]
    fn long_paste_is_replaced_with_placeholder_and_expanded() {
        let text = "alpha\nbeta\ngamma".to_string();
        let placeholder = pasted_text_placeholder(1, pasted_text_line_count(&text));
        let input = format!("请分析 {placeholder}谢谢");
        let pasted_texts = vec![Some(PastedText { text: text.clone() })];

        assert!(should_summarize_pasted_text(&text));
        assert_eq!(
            expand_pasted_text_placeholders(&input, &pasted_texts),
            "请分析 alpha\nbeta\ngamma谢谢"
        );
    }

    #[test]
    fn short_paste_is_not_summarized() {
        assert!(!should_summarize_pasted_text("short paste"));
    }

    #[test]
    fn insert_pasted_text_summarizes_long_clipboard_text() {
        let mut input = "前后".to_string();
        let mut cursor = 1;
        let mut pasted_texts = Vec::new();

        insert_pasted_text_at_cursor(
            &mut input,
            &mut cursor,
            "alpha\nbeta\ngamma".to_string(),
            &mut pasted_texts,
        );

        assert!(
            input == "前[Pasted 1: ~3 lines]后" || input == "前[粘贴 1: ~3 行]后",
            "unexpected localized placeholder: {input}"
        );
        assert_eq!(pasted_texts.len(), 1);
        assert_eq!(cursor, input.chars().count() - 1);
    }

    #[test]
    fn pasted_placeholder_is_treated_as_atomic_token() {
        let input = "前[Pasted 1: ~3 lines] 后";
        assert_eq!(placeholder_at_cursor(input, 3), Some((1, 21)));
        assert_eq!(placeholder_before_cursor(input, 21), Some((1, 21)));
        assert_eq!(placeholder_after_cursor(input, 1), Some((1, 21)));
        assert_eq!(placeholder_before_or_at_cursor(input, 3), Some((1, 21)));
        assert_eq!(placeholder_after_or_at_cursor(input, 3), Some((1, 21)));
    }

    #[test]
    fn chinese_pasted_placeholder_is_supported() {
        let input = "前[粘贴 1: ~3 行] 后";
        let placeholder = find_pasted_text_placeholders(input);

        assert_eq!(placeholder, vec![(1, 13, 1)]);
        assert_eq!(placeholder_at_cursor(input, 3), Some((1, 13)));
        assert_eq!(placeholder_before_cursor(input, 13), Some((1, 13)));
        assert_eq!(placeholder_after_cursor(input, 1), Some((1, 13)));
    }

    #[test]
    fn colorizes_image_and_pasted_placeholders() {
        let colored = colorize_repl_placeholders("[Image 1] [Pasted 1: ~3 lines]");
        assert!(colored.contains("\x1b[35m[Image 1]\x1b[0m"));
        assert!(colored.contains("\x1b[35m[Pasted 1: ~3 lines]\x1b[0m"));
    }

    #[test]
    fn placeholder_text_near_cursor_expands_pasted_placeholder() {
        let input = "前[Pasted 1: ~3 lines]后";
        let pasted_texts = vec![Some(PastedText {
            text: "alpha\nbeta\ngamma".to_string(),
        })];

        assert_eq!(
            placeholder_text_near_cursor(input, 3, &pasted_texts),
            Some("alpha\nbeta\ngamma".to_string())
        );
    }

    #[test]
    fn strips_terminal_control_sequences_from_repl_text() {
        assert_eq!(
            strip_terminal_control_sequences("\x1b[E表情包\x1b[0m\x07 ok"),
            "表情包 ok"
        );
        assert_eq!(
            strip_terminal_control_sequences("line1\nline2\tend"),
            "line1\nline2\tend"
        );
    }

    #[test]
    fn repl_history_loads_user_messages_from_state() {
        let temp = tempfile::tempdir().unwrap();
        let paths = MiyuPaths {
            root_dir: PathBuf::new(),
            config_dir: PathBuf::new(),
            config_file: PathBuf::new(),
            skills_dir: PathBuf::new(),
            data_dir: PathBuf::new(),
            cache_dir: PathBuf::new(),
            state_dir: temp.path().to_path_buf(),
            pictures_dir: PathBuf::new(),
            fish_hook_file: PathBuf::new(),
            bash_hook_file: PathBuf::new(),
            zsh_hook_file: PathBuf::new(),
            scripts_dir: PathBuf::new(),
            system_scripts_dir: PathBuf::new(),
        };
        let state = StateStore::new(&paths).unwrap();
        state.start_turn("turn_1", "first", 999999).unwrap();
        state.complete_turn("turn_1", "reply", None).unwrap();
        state.start_turn("turn_2", "second", 999999).unwrap();

        assert_eq!(
            load_repl_input_history(&state, &paths).unwrap(),
            vec!["first".to_string(), "second".to_string()]
        );
    }
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

async fn run_kb(paths: &MiyuPaths, args: KbArgs) -> Result<()> {
    let config = AppConfig::load(paths)?;
    let kb = tools::knowledge_base::KnowledgeBase::new(config, paths.clone())?;
    match args.command {
        KbCommand::Add(args) => {
            let added = kb.add_path(&args.path).await?;
            for path in added {
                println!("{} {path}", t("added", "已添加"));
            }
        }
        KbCommand::List => {
            for file in kb.list()? {
                println!("{}\t{} {}", file.name, file.size_bytes, t("bytes", "字节"));
            }
        }
        KbCommand::Search(args) => {
            let query = args.query.join(" ");
            println!("{}", kb.search(&query, args.limit).await?);
        }
        KbCommand::Find(args) => {
            let query = args.query.join(" ");
            println!("{}", kb.find_by_name(&query, args.limit)?);
        }
        KbCommand::Read(args) => {
            println!("{}", kb.read_file(&args.file, args.start, args.lines)?);
        }
        KbCommand::Remove(args) => {
            kb.remove(&args.file)?;
            println!("{} {}", t("removed", "已移除"), args.file);
        }
        KbCommand::Reindex => {
            let files = kb.list()?;
            println!(
                "{}: {}",
                t(
                    "keyword index is rebuilt on demand; files tracked",
                    "关键词索引会按需重建；已跟踪文件数",
                ),
                files.len()
            );
        }
        KbCommand::Stats => {
            let mut stats = kb.stats()?;
            if let Some(object) = stats.as_object_mut() {
                if let Ok(status) = crate::default_kb::status(paths) {
                    object.insert(
                        "default_kb_update_available".to_string(),
                        serde_json::json!(status.has_update_notice),
                    );
                }
            }
            println!("{}", stats);
        }
        KbCommand::Embed(args) => match args.command {
            KbEmbedCommand::Reindex(args) => {
                kb.reindex_embeddings(args.quiet).await?;
            }
        },
    }
    Ok(())
}

async fn run_update_default_kb(paths: &MiyuPaths) -> Result<()> {
    let config = AppConfig::load_or_default(paths)?;
    let state = crate::default_kb::update(paths, &config, |stage| {
        let mut stderr = io::stderr().lock();
        let _ = write_default_kb_update_progress(&mut stderr, stage);
    })?;
    println!(
        "{}: {}",
        t("updated default knowledge base", "已更新默认知识库"),
        state.shorin_wiki_commit
    );
    Ok(())
}

fn write_default_kb_update_progress(
    output: &mut impl Write,
    stage: crate::default_kb::UpdateStage,
) -> io::Result<()> {
    writeln!(output, "[default-kb] {}", stage.message())?;
    output.flush()
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

fn run_memory(paths: &MiyuPaths, args: MemoryArgs) -> Result<()> {
    let config = AppConfig::load_or_default(paths)?;
    let store = MemoryStore::new(&config, paths);
    match args.command {
        MemoryCommand::Stats => println!("{}", store.stats()?),
        MemoryCommand::Reset(args) => {
            store.reset_all(args.include_skills)?;
            println!("{}", t("cleared assistant memory", "已清空助手记忆"));
        }
        MemoryCommand::Search(args) => {
            let query = join_message(args.query);
            let limit = args.limit.unwrap_or(10);
            println!("{}", store.recall_memories(&query, limit, args.forgotten)?);
        }
        MemoryCommand::Remember(args) => {
            let content = join_message(args.content);
            let id = store.remember_fact(&content, &args.source)?;
            println!("{}: {id}", t("remembered fact", "已记住事实"));
        }
    }
    Ok(())
}

fn run_skills(paths: &MiyuPaths, args: SkillsArgs) -> Result<()> {
    std::fs::create_dir_all(&paths.skills_dir)?;
    match args.command {
        SkillsCommand::List => {
            for name in skill_names(paths)? {
                let disabled = paths.skills_dir.join(&name).join(".disabled").exists();
                println!(
                    "{}{}",
                    name,
                    if disabled {
                        t(" [disabled]", " [已禁用]")
                    } else {
                        ""
                    }
                );
            }
        }
        SkillsCommand::Show(args) => {
            let path = skill_dir(paths, &args.name)?.join("SKILL.md");
            println!("{}", std::fs::read_to_string(path)?);
        }
        SkillsCommand::Enable(args) => {
            let marker = skill_dir(paths, &args.name)?.join(".disabled");
            if marker.exists() {
                std::fs::remove_file(marker)?;
            }
            println!("{}: {}", t("enabled skill", "已启用 skill"), args.name);
        }
        SkillsCommand::Disable(args) => {
            let marker = skill_dir(paths, &args.name)?.join(".disabled");
            std::fs::write(marker, "disabled\n")?;
            println!("{}: {}", t("disabled skill", "已禁用 skill"), args.name);
        }
        SkillsCommand::Remove(args) => {
            let dir = skill_dir(paths, &args.name)?;
            std::fs::remove_dir_all(dir)?;
            println!("{}: {}", t("removed skill", "已移除 skill"), args.name);
        }
        SkillsCommand::Stats => {
            let names = skill_names(paths)?;
            let disabled = names
                .iter()
                .filter(|name| paths.skills_dir.join(name).join(".disabled").exists())
                .count();
            println!(
                "{}",
                serde_json::json!({
                    "ok": true,
                    "skills_dir": paths.skills_dir.display().to_string(),
                    "skills": names.len(),
                    "disabled": disabled,
                    "enabled": names.len().saturating_sub(disabled),
                })
            );
        }
        SkillsCommand::Prune => {
            let mut removed = 0usize;
            for name in skill_names(paths)? {
                let dir = paths.skills_dir.join(&name);
                let raw = std::fs::read_to_string(dir.join("SKILL.md")).unwrap_or_default();
                if crate::skills::is_generated_skill(&raw) && dir.join(".disabled").exists() {
                    std::fs::remove_dir_all(dir)?;
                    removed += 1;
                }
            }
            println!("{}: {removed}", t("pruned skills", "已清理 skills"));
        }
    }
    Ok(())
}

fn skill_names(paths: &MiyuPaths) -> Result<Vec<String>> {
    let mut names = Vec::new();
    if !paths.skills_dir.exists() {
        return Ok(names);
    }
    for entry in std::fs::read_dir(&paths.skills_dir)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() && entry.path().join("SKILL.md").is_file() {
            names.push(entry.file_name().to_string_lossy().to_string());
        }
    }
    names.sort();
    Ok(names)
}

fn skill_dir(paths: &MiyuPaths, name: &str) -> Result<PathBuf> {
    let clean = name.trim();
    if clean.is_empty()
        || clean.contains('/')
        || clean.contains('\\')
        || clean == "."
        || clean == ".."
    {
        bail!("{}: {name}", t("invalid skill name", "无效 skill 名称"));
    }
    let dir = paths.skills_dir.join(clean);
    if !dir.join("SKILL.md").is_file() {
        bail!("{}: {name}", t("skill not found", "未找到 skill"));
    }
    Ok(dir)
}

async fn run_reset(paths: &MiyuPaths) -> Result<()> {
    let config = AppConfig::load_or_default(paths)?;
    let state = StateStore::new(paths)?;
    let memory = MemoryStore::new(&config, paths);
    state.reset_conversation()?;
    memory.clear_evicted_context()?;
    memory.clear_pending_events()?;
    tools::clear_aur_review_state(paths)?;
    Ok(())
}

fn wipe_summary() -> &'static str {
    t(
        "This erases everything Miyu has accumulated: memory, every conversation's contents, group-chat contexts, and auto-generated skills. It cannot be undone.",
        "这会抹掉 Miyu 积累的一切：记忆、所有会话的内容、群聊上下文、自动生成的技能。不可撤销。",
    )
}

async fn run_wipe(paths: &MiyuPaths, assume_yes: bool) -> Result<()> {
    if !assume_yes {
        if !io::stdin().is_terminal() {
            bail!(
                "{}",
                t(
                    "wipe needs a terminal to confirm; pass --yes to run it unattended",
                    "wipe 需要在终端确认；非交互场景请加 --yes"
                )
            );
        }
        println!("{}", wipe_summary());
        if !confirm_stdin(t("wipe everything?", "确认全部抹掉？"))? {
            println!("{}", t("cancelled", "已取消"));
            return Ok(());
        }
    }
    if ipc::daemon_info(paths).await.is_some() {
        send_ipc_admin(paths, IpcCommand::WipePersona).await?;
    } else {
        let config = AppConfig::load_or_default(paths)?;
        let state = StateStore::new(paths)?;
        let persona = config.active_persona_scope();
        let bindings = state.platform_session_bindings(&persona, "onebot")?;
        let plugins = crate::platforms::plugins::PlatformPluginRegistry::built_in()?;
        plugins
            .after_persona_reset(&crate::platforms::plugins::PlatformPersonaResetContext {
                config: &config,
                paths,
                bindings: &bindings,
            })
            .await?;
        state.reset_persona_contexts(&persona, "onebot")?;
        state.reset_conversation_usage()?;
        MemoryStore::new(&config, paths).reset_all(true)?;
        tools::clear_aur_review_state(paths)?;
    }
    println!("{}", print_wipe_message());
    Ok(())
}

fn print_wipe_message() -> &'static str {
    t(
        "erased all conversations, QQ contexts, memory, and generated skills for the current persona",
        "已抹掉当前人格的全部会话内容、QQ 上下文、记忆和自动技能",
    )
}

fn print_reset_message() {
    let message = t("cleared current conversation history", "已清空当前会话历史");
    println!("\x1b[2m{message}\x1b[0m\n");
}

fn join_message(parts: Vec<String>) -> String {
    parts.join(" ").trim().to_string()
}

pub(crate) fn build_tool_registry(
    config: &AppConfig,
    paths: &MiyuPaths,
    mode: AgentMode,
    interactive_questions: bool,
) -> Result<tools::ToolRegistry> {
    let mut registry = if config.tools.enabled {
        match mode {
            AgentMode::Normal => tools::builtin_registry(config, paths),
            AgentMode::Chat => tools::chat_registry(config, paths),
        }
    } else {
        tools::ToolRegistry::new()
    };
    if config.tools.enabled && config.skills.enabled {
        tools::register_skills(&mut registry, config, paths)?;
        if mode == AgentMode::Normal {
            tools::register_skill_authoring(&mut registry, config.clone(), paths.clone());
        }
    }
    if config.tools.enabled && interactive_questions {
        tools::register_ask_question(&mut registry);
    }
    tools::register_script_display_names(&registry);
    Ok(registry)
}

fn handle_agent_event(renderer: &mut render::StreamRenderer, event: AgentEvent) -> Result<()> {
    match event {
        AgentEvent::TurnStarted { .. } => Ok(()),
        AgentEvent::RawReasoning(_) => Ok(()),
        AgentEvent::FlushJournal => Ok(()),
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
        AgentEvent::ToolPreparing { name } => {
            renderer.write_tool_preparing(&name)?;
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

#[cfg(test)]
mod remote_tool_image_tests {
    use super::*;
    use image::{Delay, Frame, Rgba, RgbaImage};

    #[test]
    fn web_tool_image_event_exposes_asset_id_to_remote_cli() {
        let event = serde_json::json!({
            "run_id": "run-1",
            "name": "show_meme",
            "asset": { "id": "img-1", "mime": "image/gif" }
        });
        assert_eq!(remote_tool_image_asset_id(&event), Some("img-1"));
        assert_eq!(remote_tool_image_asset_id(&serde_json::json!({})), None);
    }

    #[test]
    fn ipc_command_response_distinguishes_errors_and_closed_connections() {
        assert!(validate_ipc_command_response(Some(IpcFrame::Ack)).is_ok());
        let rejected = validate_ipc_command_response(Some(IpcFrame::Error {
            code: None,
            message: "Miyu is busy with another operation".to_string(),
        }))
        .unwrap_err();
        assert!(rejected.to_string().contains("busy with another operation"));

        let closed = validate_ipc_command_response(None).unwrap_err();
        assert!(closed
            .to_string()
            .contains("closed the connection without a response"));

        let unexpected = validate_ipc_command_response(Some(IpcFrame::Accepted {
            turn_id: None,
            run_id: "run-test".to_string(),
        }))
        .unwrap_err();
        assert!(unexpected.to_string().contains("unexpected response"));
    }

    #[test]
    fn remote_gif_asset_is_converted_to_static_png() {
        let mut gif = Vec::new();
        {
            let mut encoder = image::codecs::gif::GifEncoder::new(&mut gif);
            encoder
                .encode_frames((0..2).map(|value| {
                    Frame::from_parts(
                        RgbaImage::from_pixel(32, 32, Rgba([value, 20, 40, 255])),
                        0,
                        0,
                        Delay::from_numer_denom_ms(100, 1),
                    )
                }))
                .unwrap();
        }
        let asset = crate::state::ImageAssetData {
            asset: crate::state::ImageAsset {
                asset_id: "img-gif".to_string(),
                turn_id: "turn-1".to_string(),
                tool_id: Some("tool-1".to_string()),
                mime: "image/gif".to_string(),
                width: 32,
                height: 32,
                alt: "animated meme".to_string(),
                created_at: "2026-01-01T00:00:00Z".to_string(),
            },
            bytes: gif,
        };
        let preview = remote_image_preview(&asset).unwrap();
        let bytes = std::fs::read(preview.path()).unwrap();
        assert!(bytes.starts_with(b"\x89PNG\r\n\x1a\n"));
        assert_eq!(image::load_from_memory(&bytes).unwrap().width(), 32);
    }
}
