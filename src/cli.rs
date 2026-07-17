use crate::agent::{Agent, AgentEvent, AgentMode};
use crate::config::{ActiveProviderModelConfig, AppConfig};
use crate::i18n::{is_zh, text as t};
use crate::llm::{OpenAiCompatibleClient, ThinkingVariantOptions};
use crate::memory::MemoryStore;
use crate::paths::MiyuPaths;
use crate::render;
use crate::shell;
use crate::state::StateStore;
use crate::tools;
use anyhow::{bail, Result};
use clap::{Arg, ArgAction, Args, CommandFactory, FromArgMatches, Parser, Subcommand};
use crossterm::cursor::{self, Hide, MoveTo, Show};
use crossterm::event::{
    self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode, KeyEvent, KeyModifiers,
};
use crossterm::style::Print;
use crossterm::terminal::{self, Clear, ClearType};
use crossterm::{execute, queue};
use fuzzy_matcher::skim::SkimMatcherV2;
use fuzzy_matcher::FuzzyMatcher;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io::Cursor;
use std::io::{self, IsTerminal, Read, Write};
use std::path::PathBuf;
use std::time::Duration;

const REPL_MAX_VISIBLE_INPUT_ROWS: u16 = 12;
const REPL_PASTE_PLACEHOLDER_MIN_LINES: usize = 3;
const REPL_PASTE_PLACEHOLDER_MIN_CHARS: usize = 150;
#[derive(Clone, Debug)]
struct PastedText {
    text: String,
}

#[derive(Clone, Debug)]
struct ReplFooterStatus {
    provider: String,
    model: String,
    thinking: Option<String>,
    token_usage: ReplTokenUsage,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct ThinkingVariantPreferences {
    #[serde(default)]
    selected: HashMap<String, String>,
}

#[derive(Clone, Copy, Debug)]
struct ReplTokenUsage {
    turn_tokens: u64,
    session_tokens: u64,
    context_window: Option<usize>,
    cumulative_tokens: Option<u64>,
}

impl ReplFooterStatus {
    fn from_config(
        config: &AppConfig,
        session_tokens: u64,
        cumulative_tokens: Option<u64>,
    ) -> Self {
        let active = config.active_provider_model_choices();
        let (provider_id, model) = match active.as_slice() {
            [] => ("-".to_string(), "None".to_string()),
            [choice] => (choice.provider_id.clone(), choice.model.clone()),
            _ => (String::new(), "Mixed".to_string()),
        };

        Self {
            model: if model == "Mixed" || model == "None" {
                model
            } else {
                short_model_name(&model, &provider_id)
            },
            provider: provider_id,
            thinking: None,
            token_usage: ReplTokenUsage {
                turn_tokens: 0,
                session_tokens,
                context_window: config.active_context_window().ok().flatten(),
                cumulative_tokens,
            },
        }
    }

    fn update_token_usage(
        &mut self,
        result: &crate::llm::ChatResult,
        session_tokens: u64,
        context_window: Option<usize>,
        cumulative_tokens: Option<u64>,
    ) {
        if let Some(usage) = &result.usage {
            self.set_token_usage(
                render::usage_total(usage),
                session_tokens,
                context_window,
                cumulative_tokens,
            );
        }
    }

    fn set_token_usage(
        &mut self,
        turn_tokens: u64,
        session_tokens: u64,
        context_window: Option<usize>,
        cumulative_tokens: Option<u64>,
    ) {
        self.token_usage = ReplTokenUsage {
            turn_tokens,
            session_tokens,
            context_window,
            cumulative_tokens,
        };
    }

    fn update_session_tokens(&mut self, session_tokens: u64) {
        self.token_usage.session_tokens = session_tokens;
    }

    fn update_context_window(&mut self, context_window: Option<usize>) {
        self.token_usage.context_window = context_window;
    }

    fn reset_token_usage(&mut self, session_tokens: u64, context_window: Option<usize>) {
        self.token_usage = ReplTokenUsage {
            turn_tokens: 0,
            session_tokens,
            context_window,
            cumulative_tokens: None,
        };
    }

    fn update_thinking_variant(&mut self, variant: Option<&str>) {
        self.thinking = variant.map(str::to_string);
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

fn repl_footer_line(mode: AgentMode, footer: &ReplFooterStatus, cols: usize) -> String {
    let cols = cols.max(1);
    let bar = input_prompt_bar(mode);
    let bar_width = visible_width(&bar);
    let usage = footer.token_usage;
    let right_plain = render::format_token_usage_inline(
        usage.turn_tokens,
        usage.session_tokens,
        usage.context_window,
        usage.cumulative_tokens,
    );
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
    let provider = format!("\x1b[2m{}\x1b[0m", footer.provider);
    let mode = colored_footer_mode_label(mode);
    let full = repl_footer_left_parts(&mode, &footer.model, Some(&provider), thinking);
    if visible_width(&full) <= width {
        return full;
    }

    let compact = repl_footer_left_parts(&mode, &footer.model, None, thinking);
    if visible_width(&compact) <= width {
        return compact;
    }

    let fixed_width = visible_width(&mode)
        .saturating_add(if thinking.is_empty() {
            0
        } else {
            1 + thinking.len()
        })
        .saturating_add(1);
    let model_budget = width.saturating_sub(fixed_width).max(1);
    let model = truncate_display(&footer.model, model_budget);
    repl_footer_left_parts(&mode, &model, None, thinking)
}

fn repl_footer_left_parts(
    mode: &str,
    model: &str,
    provider: Option<&str>,
    thinking: &str,
) -> String {
    let mut parts = vec![mode.to_string(), model.to_string()];
    if let Some(provider) = provider.filter(|provider| !provider.is_empty()) {
        parts.push(provider.to_string());
    }
    if !thinking.is_empty() {
        parts.push(thinking.to_string());
    }
    parts.join(" ")
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
    match mode {
        AgentMode::Normal => "\x1b[1m\x1b[34mNormal\x1b[0m".to_string(),
        AgentMode::Plan => "\x1b[1m\x1b[35mPlan\x1b[0m".to_string(),
        AgentMode::Chat => "\x1b[1m\x1b[32mChat\x1b[0m".to_string(),
    }
}

#[derive(Debug, Parser)]
#[command(name = "miyu", version, about = "Miyu CLI AI Agent")]
pub struct Cli {
    #[arg(long)]
    pub plan: bool,

    #[arg(long)]
    pub stdout: bool,

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
    let matches = localized_command().get_matches();
    Cli::from_arg_matches(&matches).unwrap_or_else(|err| err.exit())
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
            .after_help("提示：不带参数进入 REPL；直接输入消息会发送一次对话。使用 MIYU_LANG=en_US 可切换英文。")
            .disable_help_subcommand(true);
    } else {
        command = command
            .after_help(
                "Tip: run without arguments to enter the REPL; pass MESSAGE to send one chat turn. Set MIYU_LANG=zh_CN for Chinese.",
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
        .mut_arg("plan", |arg| {
            arg.help(t("Run in read-only planning mode", "使用只读计划模式运行"))
        })
        .mut_arg("stdout", |arg| {
            arg.help(t(
                "Plain output mode (no colors, no TUI); pipe-friendly for stdout redirection",
                "纯文本输出模式（无颜色、无 TUI）；适合管道重定向",
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
            "Create default config and state files",
            "创建默认配置和状态文件",
        ),
        (
            "paths",
            "Show app config, data, and cache paths",
            "显示应用配置、数据和缓存路径",
        ),
        ("config", "Open or manage configuration", "打开或管理配置"),
        ("models", "List or switch models", "列出或切换模型"),
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
            "Clear current conversation history",
            "清空当前会话历史",
        ),
    ];
    for (name, en, zh) in descriptions {
        command = command.mut_subcommand(name, |subcommand| subcommand.about(t(en, zh)));
    }
    command = command
        .mut_subcommand("ask", localize_ask_command)
        .mut_subcommand("models", localize_models_command)
        .mut_subcommand("history", localize_history_command)
        .mut_subcommand("kb", localize_kb_command)
        .mut_subcommand("memory", localize_memory_command)
        .mut_subcommand("skills", localize_skills_command)
        .mut_subcommand("config", localize_config_command)
        .mut_subcommand("reset", localize_reset_command);
    command
}

fn localize_ask_command(command: clap::Command) -> clap::Command {
    command.mut_arg("message", |arg| {
        arg.help(t("Message to send", "要发送的消息"))
    })
}

fn localize_models_command(command: clap::Command) -> clap::Command {
    command.mut_arg("index", |arg| {
        arg.help(t("Model list index to activate", "要激活的模型列表序号"))
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

fn localize_config_command(command: clap::Command) -> clap::Command {
    command
        .mut_subcommand("validate", |subcommand| {
            subcommand.about(t("Validate configuration", "校验配置"))
        })
        .mut_subcommand("paths", |subcommand| {
            subcommand.about(t("Show configuration paths", "显示配置路径"))
        })
}

fn localize_reset_command(command: clap::Command) -> clap::Command {
    command.mut_arg("scope", |arg| {
        arg.help(t(
            "all also clears long-term memory",
            "all 同时清空长期记忆",
        ))
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
    Ask(MessageArgs),
    Init,
    Paths,
    Config(ConfigArgs),
    Models(ModelsArgs),
    FishInit,
    BashInit,
    ZshInit,
    RemoveShellHook,
    History(HistoryArgs),
    Kb(KbArgs),
    UpdateDefaultKb,
    Memory(MemoryArgs),
    Skills(SkillsArgs),
    Reset(ResetArgs),
}

#[derive(Debug, Args)]
pub struct MessageArgs {
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    pub message: Vec<String>,
}

#[derive(Debug, Args)]
pub struct ResetArgs {
    pub scope: Option<String>,
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
pub struct ModelsArgs {
    pub index: Option<usize>,
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

pub async fn run(cli: Cli) -> Result<()> {
    if cli.shell_classify {
        let shell_name = cli.shell.as_deref().unwrap_or("fish");
        let message = shell_message_from_input(cli.stdin, cli.message)?;
        return run_shell_classify(shell_name, &message);
    }

    let paths = MiyuPaths::new()?;
    let mode = if cli.plan {
        AgentMode::Plan
    } else {
        AgentMode::Normal
    };

    crate::models_cache::try_load(&paths);
    crate::models_cache::spawn_background_refresh(paths.clone());

    if cli.shell_intercept {
        let shell_name = cli.shell.as_deref().unwrap_or("fish");
        let message = shell_message_from_input(cli.stdin, cli.message)?;
        return run_shell_intercept(&paths, shell_name, message).await;
    }

    if cli.clipboard_paste {
        return run_clipboard_paste(&paths);
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
        )
    {
        run_init(&paths, InitKind::FirstRun)?;
    }

    match cli.command {
        Some(Command::AlarmWorker(args)) => run_alarm_worker(args),
        Some(Command::Tool(args)) => run_tool(&paths, mode, args).await,
        Some(Command::Ask(args)) => {
            run_chat_with_options(&paths, join_message(args.message), None, cli.stdout, mode).await
        }
        Some(Command::Init) => run_init(&paths, InitKind::Explicit),
        Some(Command::Paths) => {
            paths.print();
            Ok(())
        }
        Some(Command::Config(args)) => run_config(&paths, args).await,
        Some(Command::Models(args)) => run_models(&paths, args),
        Some(Command::FishInit) => shell::fish::install(&paths),
        Some(Command::BashInit) => shell::bash::install(&paths),
        Some(Command::ZshInit) => shell::zsh::install(&paths),
        Some(Command::RemoveShellHook) => remove_shell_hooks(&paths),
        Some(Command::History(args)) => run_history(&paths, args),
        Some(Command::Kb(args)) => run_kb(&paths, args).await,
        Some(Command::UpdateDefaultKb) => run_update_default_kb(&paths).await,
        Some(Command::Memory(args)) => run_memory(&paths, args),
        Some(Command::Skills(args)) => run_skills(&paths, args),
        Some(Command::Reset(args)) => run_reset(&paths, args.scope.as_deref()),
        None => {
            let message = join_message(cli.message);
            if message.is_empty() && io::stdin().is_terminal() {
                run_repl(&paths, mode).await
            } else {
                run_chat_with_options(&paths, message, None, cli.stdout, mode).await
            }
        }
    }
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
        std::thread::sleep(Duration::from_millis(420));
        prompt_shell_init_menu(paths)?;
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

fn prompt_shell_init_menu(paths: &MiyuPaths) -> Result<()> {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        return Ok(());
    }
    println!("{}", t("Integrate with shell?", "是否集成到 shell？"));
    println!(
        "{}\n",
        t(
            "After integration, you can chat in natural language directly in the terminal.",
            "集成后可在终端直接使用自然语言交流。"
        )
    );
    match select_shell_hook()? {
        Some("fish") => shell::fish::install(paths),
        Some("bash") => shell::bash::install(paths),
        Some("zsh") => shell::zsh::install(paths),
        _ => Ok(()),
    }
}

fn select_shell_hook() -> Result<Option<&'static str>> {
    let options = [
        (t("Skip", "跳过"), None),
        ("fish", Some("fish")),
        ("bash", Some("bash")),
        ("zsh", Some("zsh")),
    ];
    let detected = shell::current_parent_shell();
    let mut selected = detected
        .as_deref()
        .and_then(|shell| options.iter().position(|(_, value)| *value == Some(shell)))
        .unwrap_or(0);
    let mut stdout = io::stdout();
    let (_, menu_row) = cursor::position()?;
    execute!(stdout, Hide)?;
    struct ShellMenuGuard;
    impl Drop for ShellMenuGuard {
        fn drop(&mut self) {
            let _ = terminal::disable_raw_mode();
            let _ = execute!(io::stdout(), Show);
        }
    }
    let _guard = ShellMenuGuard;
    loop {
        queue!(
            stdout,
            MoveTo(0, menu_row),
            Clear(ClearType::FromCursorDown)
        )?;
        for (index, (label, _)) in options.iter().enumerate() {
            if index == selected {
                queue!(stdout, Print(format!("> {label}\n")))?;
            } else {
                queue!(stdout, Print(format!("  {label}\n")))?;
            }
        }
        queue!(
            stdout,
            Print(format!(
                "\n\x1b[2m{}\x1b[0m",
                t(
                    "Up/Down or j/k to choose, Enter to confirm, Esc/q to skip",
                    "↑/↓ 或 j/k 选择，Enter 确认，Esc/q 跳过"
                )
            ))
        )?;
        stdout.flush()?;
        terminal::enable_raw_mode()?;
        let key = read_shell_menu_key();
        terminal::disable_raw_mode()?;
        match key? {
            KeyCode::Esc | KeyCode::Char('q') => {
                execute!(stdout, Show)?;
                return Ok(None);
            }
            KeyCode::Up | KeyCode::Char('k') => selected = selected.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => selected = (selected + 1).min(options.len() - 1),
            KeyCode::Enter => {
                execute!(stdout, Show)?;
                return Ok(options[selected].1);
            }
            _ => {}
        }
    }
}

fn read_shell_menu_key() -> Result<KeyCode> {
    loop {
        if let Event::Key(KeyEvent { code, .. }) = event::read()? {
            return Ok(code);
        }
    }
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
    let paths = alarm_worker_paths(args.state_dir);
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
    std::fs::create_dir_all(&paths.state_dir)?;
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(crate::alarm::alarm_log_file(paths))?;
    file.write_all(line.as_bytes())?;
    Ok(())
}

fn alarm_worker_paths(state_dir: PathBuf) -> MiyuPaths {
    MiyuPaths {
        config_dir: PathBuf::new(),
        config_file: PathBuf::new(),
        skills_dir: PathBuf::new(),
        data_dir: PathBuf::new(),
        cache_dir: PathBuf::new(),
        state_dir,
        pictures_dir: PathBuf::new(),
        fish_hook_file: PathBuf::new(),
        bash_hook_file: PathBuf::new(),
        zsh_hook_file: PathBuf::new(),
        scripts_dir: PathBuf::new(),
        system_scripts_dir: PathBuf::new(),
    }
}

fn run_models(paths: &MiyuPaths, args: ModelsArgs) -> Result<()> {
    let mut config = AppConfig::load(paths)?;
    let choices = config.text_provider_model_choices();
    if choices.is_empty() {
        bail!(
            "{}",
            t(
                "no active provider models; configure or activate a model first",
                "没有已激活的 provider 模型；请先配置或激活模型",
            )
        );
    }
    if let Some(index) = args.index {
        if index == 0 || index > choices.len() {
            bail!(
                "{}: {index}",
                t("provider index out of range", "provider 序号超出范围")
            );
        }
        let choice = &choices[index - 1];
        let provider_id = choice.provider_id.clone();
        let model = choice.model.clone();
        let label = choice.label();
        config.set_active_provider_model(&provider_id, &model)?;
        config.save(paths)?;
        println!(
            "{}: {index}. {label}",
            t("active provider", "当前 provider")
        );
        return Ok(());
    }
    if io::stdout().is_terminal() && io::stdin().is_terminal() {
        let active = choices
            .iter()
            .map(|choice| config.is_active_provider_model(&choice.provider_id, &choice.model))
            .collect::<Vec<_>>();
        if let Some(active) = inline_fuzzy_select(
            &choices
                .iter()
                .map(|choice| choice.label())
                .collect::<Vec<_>>(),
            active,
        )? {
            config.active_provider_models = Some(
                choices
                    .iter()
                    .zip(active)
                    .filter_map(|(choice, active)| {
                        active.then(|| ActiveProviderModelConfig {
                            provider_id: choice.provider_id.clone(),
                            model: choice.model.clone(),
                        })
                    })
                    .collect(),
            );
            config.save(paths)?;
            println!("{}", t("active provider models updated", "已更新激活模型"));
        }
        return Ok(());
    }
    for (index, choice) in choices.iter().enumerate() {
        let marker = if config.is_active_provider_model(&choice.provider_id, &choice.model) {
            "[*]"
        } else {
            "[ ]"
        };
        println!("{marker} {}. {}", index + 1, choice.label());
    }
    Ok(())
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

async fn run_config(paths: &MiyuPaths, args: ConfigArgs) -> Result<()> {
    match args.command {
        Some(ConfigCommand::Validate) => {
            AppConfig::load(paths)?;
            println!(
                "{}: {}",
                t("config is valid", "配置有效"),
                paths.config_file.display()
            );
            Ok(())
        }
        Some(ConfigCommand::Paths) => {
            paths.print();
            Ok(())
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
            Ok(())
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
                std::os::unix::fs::symlink(&path, &link_path)?;
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
        run_chat_with_options(paths, clean_message, None, false, AgentMode::Normal).await
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
    AppConfig::init_files(paths)?;
    let config = AppConfig::load_or_default(paths)?;
    let state = StateStore::new(paths)?;
    state.init_files()?;
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
    let mut cumulative_tokens = result.usage.as_ref().map(render::usage_total).unwrap_or(0);
    let context_tokens = agent.effective_context_tokens()?;
    print_chat_token_usage(
        &result,
        show_token_usage,
        context_tokens,
        result_context_window(&display_config, &result).or(agent.context_window()),
        Some(cumulative_tokens),
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
            Some(cumulative_tokens),
        )?;
    }
    Ok(())
}

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

async fn append_stdin_if_piped(message: String) -> String {
    if io::stdin().is_terminal() {
        return message;
    }
    let read_result = tokio::time::timeout(
        std::time::Duration::from_secs(STDIN_TIMEOUT_SECS),
        tokio::task::spawn_blocking(|| {
            let mut buf = String::new();
            let mut stdin = std::io::stdin().lock();
            let mut limited = (&mut stdin).take(STDIN_MAX_CHARS as u64);
            limited.read_to_string(&mut buf).map(|_| buf)
        }),
    )
    .await;

    let stdin_content = match read_result {
        Ok(Ok(Ok(content))) if !content.trim().is_empty() => content.trim().to_string(),
        _ => return message,
    };

    if message.is_empty() {
        stdin_content
    } else {
        format!("{message}\n\n---\n(stdin)\n{stdin_content}")
    }
}

async fn run_chat_with_options(
    paths: &MiyuPaths,
    message: String,
    show_reasoning: Option<bool>,
    plain: bool,
    mode: AgentMode,
) -> Result<()> {
    let message = append_stdin_if_piped(message).await;
    if message.is_empty() {
        return run_repl(paths, mode).await;
    }
    AppConfig::init_files(paths)?;
    let config = AppConfig::load_or_default(paths)?;
    let state = StateStore::new(paths)?;
    state.init_files()?;
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
    let mut cumulative_tokens = result.usage.as_ref().map(render::usage_total).unwrap_or(0);
    let context_tokens = agent.effective_context_tokens()?;
    print_chat_token_usage(
        &result,
        show_token_usage,
        context_tokens,
        result_context_window(&display_config, &result).or(agent.context_window()),
        Some(cumulative_tokens),
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
            Some(cumulative_tokens),
        )?;
    }
    Ok(())
}

fn print_chat_token_usage(
    result: &crate::llm::ChatResult,
    enabled: bool,
    session_token_total: u64,
    context_window: Option<usize>,
    cumulative_tokens: Option<u64>,
) -> Result<()> {
    if enabled {
        if let Some(usage) = &result.usage {
            let turn_tokens = render::usage_total(usage);
            render::print_token_usage(
                turn_tokens,
                session_token_total,
                context_window,
                cumulative_tokens,
                result.usage_estimated,
            )?;
        }
    }
    Ok(())
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
    cumulative_tokens: Option<&mut u64>,
) -> Result<Option<crate::llm::ChatResult>> {
    let compact_result = agent
        .handle_overflow_after_turn(context_tokens, |event| handle_agent_event(renderer, event))
        .await?;
    renderer.finish()?;
    if let Some(compact_result) = compact_result {
        let mut cumulative_display = None;
        if let Some(total) = cumulative_tokens {
            if let Some(usage) = compact_result.usage.as_ref() {
                *total = total.saturating_add(render::usage_total(usage));
                cumulative_display = Some(*total);
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

async fn run_repl(paths: &MiyuPaths, initial_mode: AgentMode) -> Result<()> {
    AppConfig::init_files(paths)?;
    let mut config = AppConfig::load_or_default(paths)?;
    let state = StateStore::new(paths)?;
    state.init_files()?;
    let mut client = OpenAiCompatibleClient::from_config(&config, paths)?;
    restore_thinking_variant(paths, &mut client);
    let mut mode = initial_mode;
    let mut input_history = load_repl_input_history(&state)?;
    let mut prefill = None::<String>;

    crate::default_kb::check_update_if_due(paths).ok();
    if let Ok(Some(message)) = crate::default_kb::notice_if_update_available(paths) {
        println!("\x1b[2m{message}\x1b[0m");
    }
    let mut cumulative_tokens = 0u64;
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
    let mut footer =
        ReplFooterStatus::from_config(&config, agent.effective_context_tokens()?, None);
    let thinking_summary = client.thinking_variant_summary();
    footer.update_thinking_variant(thinking_summary.as_deref());
    footer.update_context_window(agent.context_window());
    loop {
        let thinking_summary = client.thinking_variant_summary();
        footer.update_thinking_variant(thinking_summary.as_deref());
        let (input, pasted_images) = match read_repl_input(
            paths,
            mode,
            prefill.take(),
            &input_history,
            &footer,
            show_shortcut_hint,
        )? {
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
        if command.eq_ignore_ascii_case("/models") && command_args_empty {
            run_models(paths, ModelsArgs { index: None })?;
            reload_repl_config(paths, &mut config, &mut client)?;
            restore_thinking_variant(paths, &mut client);
            footer = ReplFooterStatus::from_config(
                &config,
                agent.effective_context_tokens()?,
                (cumulative_tokens > 0).then_some(cumulative_tokens),
            );
            let thinking_summary = client.thinking_variant_summary();
            footer.update_thinking_variant(thinking_summary.as_deref());
            let registry =
                build_tool_registry(&config, paths, mode, crate::question_tui::available(false))?;
            agent.reload_config(config.clone(), client.clone())?;
            agent.switch_mode(mode, registry);
            footer.update_context_window(agent.context_window());
            println!("{}", t("configuration reloaded", "配置已重新加载"));
            println!();
            continue;
        }
        if command.eq_ignore_ascii_case("/config") && command_args_empty {
            crate::config_tui::run(paths)?;
            reload_repl_config(paths, &mut config, &mut client)?;
            restore_thinking_variant(paths, &mut client);
            footer = ReplFooterStatus::from_config(
                &config,
                agent.effective_context_tokens()?,
                (cumulative_tokens > 0).then_some(cumulative_tokens),
            );
            let thinking_summary = client.thinking_variant_summary();
            footer.update_thinking_variant(thinking_summary.as_deref());
            let registry =
                build_tool_registry(&config, paths, mode, crate::question_tui::available(false))?;
            agent.reload_config(config.clone(), client.clone())?;
            agent.switch_mode(mode, registry);
            footer.update_context_window(agent.context_window());
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
            if selected.is_empty() {
                let options = client.thinking_variant_options();
                let Some(selections) = inline_variant_select(&options)? else {
                    continue;
                };
                client.set_thinking_variants(&selections)?;
            } else {
                if client.thinking_variant_options().len() != 1 {
                    eprintln!(
                        "\x1b[31m{}\x1b[0m",
                        t(
                            "multiple models are active; use /variant and configure them in the TUI",
                            "当前激活了多个模型；请使用 /variant 在 TUI 中分别设置"
                        )
                    );
                    continue;
                }
                let explicit_variant = selected.strip_prefix("variant:");
                let variant =
                    if explicit_variant.is_none() && selected.eq_ignore_ascii_case("default") {
                        None
                    } else {
                        let selected = explicit_variant.unwrap_or(selected);
                        let available = client.available_thinking_variants();
                        match available
                            .iter()
                            .find(|candidate| candidate.eq_ignore_ascii_case(selected))
                        {
                            Some(candidate) => Some(candidate.clone()),
                            None => {
                                eprintln!(
                                    "\x1b[31m{}: {selected}\x1b[0m",
                                    t("unknown thinking variant", "未知思考档位")
                                );
                                continue;
                            }
                        }
                    };
                client.set_thinking_variant(variant)?;
            }
            save_thinking_variant(paths, &client)?;
            let thinking_summary = client.thinking_variant_summary();
            footer.update_thinking_variant(thinking_summary.as_deref());
            agent.replace_client(client.clone());
            println!("{}\n", t("thinking variants updated", "已更新思考档位"));
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
            prefill = prompt;
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
                        cumulative_tokens =
                            cumulative_tokens.saturating_add(render::usage_total(usage));
                    }
                    footer.update_token_usage(
                        &result,
                        agent.effective_context_tokens()?,
                        agent.context_window(),
                        (cumulative_tokens > 0).then_some(cumulative_tokens),
                    );
                    if config.display.show_token_usage {
                        print_chat_token_usage(
                            &result,
                            true,
                            agent.effective_context_tokens()?,
                            agent.context_window(),
                            (cumulative_tokens > 0).then_some(cumulative_tokens),
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
            run_reset(paths, None)?;
            input_history.clear();
            cumulative_tokens = 0;
            footer.reset_token_usage(agent.effective_context_tokens()?, agent.context_window());
            continue;
        }
        if command.eq_ignore_ascii_case("/reset") && command_args.trim().eq_ignore_ascii_case("all")
        {
            run_reset(paths, Some("all"))?;
            input_history.clear();
            agent.reset_memory()?;
            cumulative_tokens = 0;
            footer.reset_token_usage(agent.effective_context_tokens()?, agent.context_window());
            continue;
        }
        if input.is_empty() {
            continue;
        }
        input_history.push(input.to_string());
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
        renderer.start_waiting()?;
        let chat_result = {
            let renderer_cell = std::cell::RefCell::new(&mut renderer);
            let chat = agent.chat_stream_with_images(input, &pasted_images, |event| {
                handle_agent_event(&mut *renderer_cell.borrow_mut(), event)
            });
            tokio::pin!(chat);
            let mut spinner_tick = tokio::time::interval(render::wait_spinner::SPINNER_INTERVAL);
            spinner_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            spinner_tick.tick().await;
            loop {
                tokio::select! {
                    result = &mut chat => break result.map(Some),
                    signal = tokio::signal::ctrl_c() => {
                        signal?;
                        break Ok(None);
                    }
                    _ = spinner_tick.tick() => {
                        renderer_cell.borrow_mut().tick_spinner()?;
                    }
                }
            }
        };
        renderer.finish()?;
        match chat_result {
            Ok(Some(result)) => {
                let context_window =
                    result_context_window(&config, &result).or(agent.context_window());
                let mut turn_tokens = result.usage.as_ref().map(render::usage_total).unwrap_or(0);
                if let Some(usage) = result.usage.as_ref() {
                    cumulative_tokens =
                        cumulative_tokens.saturating_add(render::usage_total(usage));
                }
                let context_tokens = agent.effective_context_tokens()?;
                footer.update_token_usage(
                    &result,
                    context_tokens,
                    context_window,
                    (cumulative_tokens > 0).then_some(cumulative_tokens),
                );
                let endpoint_variant = result.provider_id.as_deref().and_then(|provider_id| {
                    result
                        .model
                        .as_deref()
                        .and_then(|model| client.thinking_variant_for(provider_id, model))
                });
                print_mixed_model_endpoint(
                    show_mixed_model_endpoint(&config, true),
                    &result,
                    endpoint_variant.as_deref(),
                );
                match handle_post_turn_overflow(
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
                            turn_tokens = turn_tokens.saturating_add(render::usage_total(usage));
                        }
                        footer.set_token_usage(
                            turn_tokens,
                            agent.effective_context_tokens()?,
                            agent.context_window(),
                            (cumulative_tokens > 0).then_some(cumulative_tokens),
                        );
                    }
                    Ok(None) => {
                        footer.update_session_tokens(agent.effective_context_tokens()?);
                    }
                    Err(err) => {
                        eprintln!("\x1b[31m{}: {err}\x1b[0m", t("error", "错误"));
                        continue;
                    }
                }
                show_shortcut_hint = false;
            }
            Ok(None) => {}
            Err(err) if crate::question::is_question_cancelled(&err) => {
                footer.update_session_tokens(agent.effective_context_tokens()?);
                continue;
            }
            Err(err) => {
                eprintln!("\x1b[31m{}: {err}\x1b[0m", t("error", "错误"));
                continue;
            }
        }
    }
    Ok(())
}

fn reload_repl_config(
    paths: &MiyuPaths,
    config: &mut AppConfig,
    client: &mut OpenAiCompatibleClient,
) -> Result<()> {
    *config = AppConfig::load(paths)?;
    *client = OpenAiCompatibleClient::from_config(config, paths)?;
    Ok(())
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

fn thinking_variant_preferences_file(paths: &MiyuPaths) -> PathBuf {
    paths.state_dir.join("thinking-variants.json")
}

fn thinking_variant_key(provider_id: &str, model: &str) -> String {
    format!("{provider_id}\t{model}")
}

fn load_thinking_variant_preferences(paths: &MiyuPaths) -> ThinkingVariantPreferences {
    let path = thinking_variant_preferences_file(paths);
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

fn restore_thinking_variant(paths: &MiyuPaths, client: &mut OpenAiCompatibleClient) {
    let preferences = load_thinking_variant_preferences(paths);
    let selections = client
        .endpoint_model_preferences()
        .into_iter()
        .filter_map(|options| {
            let (provider_id, model) = options;
            let selected = preferences
                .selected
                .get(&thinking_variant_key(&provider_id, &model))?;
            Some((provider_id, model, selected.clone()))
        })
        .collect::<Vec<_>>();
    client.restore_thinking_variants(&selections);
}

fn save_thinking_variant(paths: &MiyuPaths, client: &OpenAiCompatibleClient) -> Result<()> {
    let path = thinking_variant_preferences_file(paths);
    let mut preferences = load_thinking_variant_preferences(paths);
    for options in client.thinking_variant_options() {
        let key = thinking_variant_key(&options.provider_id, &options.model);
        if let Some(variant) = options.selected {
            preferences.selected.insert(key, variant.to_string());
        } else {
            preferences.selected.remove(&key);
        }
    }
    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("thinking variant state path has no parent"))?;
    std::fs::create_dir_all(parent)?;
    let mut temp = tempfile::NamedTempFile::new_in(parent)?;
    temp.write_all(serde_json::to_string_pretty(&preferences)?.as_bytes())?;
    temp.persist(path).map_err(|error| error.error)?;
    Ok(())
}

fn load_repl_input_history(state: &StateStore) -> Result<Vec<String>> {
    Ok(state
        .load_conversation()?
        .into_iter()
        .filter(|entry| entry.role == "user" && !entry.content.trim().is_empty())
        .map(|entry| strip_terminal_control_sequences(&entry.content))
        .filter(|content| !content.trim().is_empty())
        .collect())
}

fn print_repl_help() {
    println!("{}", t("commands:", "命令:"));
    println!(
        "  /models     {}",
        t("quickly switch model", "快速切换模型")
    );
    println!(
        "  /config     {}",
        t("open configuration UI", "打开配置界面")
    );
    println!(
        "  /variant [name] {}",
        t("view or switch thinking level", "查看或切换思考档位")
    );
    println!(
        "  /undo       {}",
        t(
            "undo the last turn or context compaction",
            "撤销上一轮或上下文压缩"
        )
    );
    println!(
        "  /compact   {}",
        t(
            "compact current conversation context now",
            "立即压缩当前会话上下文"
        )
    );
    println!(
        "  /reset [all] {}",
        t(
            "clear current conversation history; all also clears memory",
            "清空当前会话历史；all 同时清空记忆"
        )
    );
    println!("  /help       {}", t("show this help", "显示此帮助"));
    println!("  /exit       {}", t("leave REPL", "退出 REPL"));
    println!("{}", t("keys:", "快捷键:"));
    println!(
        "  Tab         {}",
        t(
            "cycle NORMAL/PLAN/CHAT, or complete slash commands",
            "循环切换 普通/计划/闲聊，或补全斜杠菜单"
        )
    );
    println!("  Enter       {}", t("send message", "发送消息"));
    println!("  Ctrl+J      {}", t("insert newline", "插入换行"));
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
    let (cursor_col, _) = cursor::position()?;
    if cursor_col != 0 {
        writeln!(stdout)?;
        stdout.flush()?;
    }
    terminal::enable_raw_mode()?;
    execute!(stdout, EnableBracketedPaste)?;
    let (_, mut input_row) = cursor::position()?;
    let mut rendered_rows = 0u16;
    let mut is_pasted = false;
    let mut pasted_images: Vec<Option<crate::clipboard::PastedImage>> = Vec::new();
    let mut pasted_texts: Vec<Option<PastedText>> = Vec::new();
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
                            AgentMode::Normal => AgentMode::Plan,
                            AgentMode::Plan => AgentMode::Chat,
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
                    execute!(stdout, DisableBracketedPaste)?;
                    terminal::disable_raw_mode()?;
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
                    execute!(stdout, DisableBracketedPaste)?;
                    terminal::disable_raw_mode()?;
                    return Ok(None);
                }
                KeyCode::Char('d')
                    if modifiers.contains(KeyModifiers::CONTROL) && input.is_empty() =>
                {
                    move_after_repl_input(&mut stdout, input_row, rendered_rows)?;
                    execute!(stdout, DisableBracketedPaste)?;
                    terminal::disable_raw_mode()?;
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
        AgentMode::Plan => "\x1b[1m\x1b[35m┃\x1b[0m".to_string(),
        AgentMode::Chat => "\x1b[1m\x1b[32m┃\x1b[0m".to_string(),
    }
}

fn input_prompt_bar(mode: AgentMode) -> String {
    format!("{} ", submitted_echo_bar(mode))
}

fn repl_shortcut_hint_line(mode: AgentMode, cols: usize) -> String {
    let bar = input_prompt_bar(mode);
    let text = t(
        "Tab switch mode; Ctrl+J newline; Ctrl+V paste clipboard",
        "Tab 切换模式；Ctrl+J 换行；Ctrl+V 粘贴剪贴板",
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

fn repl_commands() -> [&'static str; 8] {
    [
        "/models", "/config", "/variant", "/undo", "/compact", "/reset", "/help", "/exit",
    ]
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

    #[test]
    fn models_is_the_cli_model_selector() {
        let matches = localized_command()
            .try_get_matches_from(["miyu", "models", "1"])
            .unwrap();
        let cli = Cli::from_arg_matches(&matches).unwrap();

        assert!(matches!(
            cli.command,
            Some(Command::Models(ModelsArgs { index: Some(1) }))
        ));
        let old_matches = localized_command()
            .try_get_matches_from(["miyu", "providers"])
            .unwrap();
        let old_cli = Cli::from_arg_matches(&old_matches).unwrap();
        assert!(old_cli.command.is_none());
        assert_eq!(old_cli.message, ["providers"]);
    }

    #[test]
    fn footer_reset_clears_turn_and_cumulative_tokens() {
        let config = AppConfig::default();
        let mut footer = ReplFooterStatus::from_config(&config, 100, Some(250));
        footer.set_token_usage(50, 100, Some(200_000), Some(250));

        footer.reset_token_usage(0, Some(200_000));

        assert_eq!(footer.token_usage.turn_tokens, 0);
        assert_eq!(footer.token_usage.session_tokens, 0);
        assert_eq!(footer.token_usage.context_window, Some(200_000));
        assert_eq!(footer.token_usage.cumulative_tokens, None);

        footer.reset_token_usage(0, None);
        assert_eq!(footer.token_usage.context_window, None);
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
    fn command_suggestions_are_prefixed_and_truncated() {
        let suggestions = repl_command_suggestions("/");
        let line = repl_command_suggestions_line(&suggestions, 24);
        assert!(line.starts_with("/models"));
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
        assert!(strip_terminal_control_sequences(&header).contains("选择模型"));
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
    fn partial_slash_command_resolves_unique_match() {
        assert_eq!(resolve_repl_command("/model"), "/models");
        assert_eq!(resolve_repl_command("/compa"), "/compact");
        assert_eq!(resolve_repl_command("/co"), "/co");
        assert_eq!(resolve_repl_command("hello"), "hello");
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
            load_repl_input_history(&state).unwrap(),
            vec!["first".to_string(), "second".to_string()]
        );
    }
}

fn run_history(paths: &MiyuPaths, args: HistoryArgs) -> Result<()> {
    let state = StateStore::new(paths)?;
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
    let state = crate::default_kb::update(paths, &config)?;
    println!(
        "{}: {}",
        t("updated default knowledge base", "已更新默认知识库"),
        state.shorin_wiki_commit
    );
    Ok(())
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
                if raw.contains("generated_by: miyu") && dir.join(".disabled").exists() {
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

fn run_reset(paths: &MiyuPaths, scope: Option<&str>) -> Result<()> {
    let all = match scope {
        None => false,
        Some("all") => true,
        Some(scope) => bail!("{}: {scope}", t("unknown reset scope", "未知 reset 范围")),
    };
    let config = AppConfig::load_or_default(paths)?;
    StateStore::new(paths)?.reset_conversation()?;
    let memory = MemoryStore::new(&config, paths);
    if all {
        memory.reset_all(false)?;
    } else {
        memory.clear_evicted_context()?;
        memory.clear_pending_events()?;
    }
    tools::clear_aur_review_state(paths)?;
    let message = if all {
        t(
            "cleared current conversation history and all memory",
            "已清空当前会话历史与全部记忆",
        )
    } else {
        t("cleared current conversation history", "已清空当前会话历史")
    };
    println!("\x1b[2m{message}\x1b[0m\n");
    Ok(())
}

fn join_message(parts: Vec<String>) -> String {
    parts.join(" ").trim().to_string()
}

fn build_tool_registry(
    config: &AppConfig,
    paths: &MiyuPaths,
    mode: AgentMode,
    interactive_questions: bool,
) -> Result<tools::ToolRegistry> {
    let mut registry = if config.tools.enabled {
        match mode {
            AgentMode::Normal => tools::builtin_registry(config, paths),
            AgentMode::Plan => tools::readonly_registry(config, paths),
            AgentMode::Chat => tools::chat_registry(config, paths),
        }
    } else {
        tools::ToolRegistry::new()
    };
    if config.tools.enabled && config.skills.enabled && mode != AgentMode::Chat {
        tools::register_skills(&mut registry, config, paths)?;
    }
    if config.tools.enabled && interactive_questions {
        tools::register_ask_question(&mut registry);
    }
    tools::register_script_display_names(&registry);
    Ok(registry)
}

fn handle_agent_event(renderer: &mut render::StreamRenderer, event: AgentEvent) -> Result<()> {
    match event {
        AgentEvent::Chunk(chunk) => {
            renderer.write_chunk(chunk)?;
            renderer.tick_spinner()
        }
        AgentEvent::ToolCall { name, arguments } => {
            renderer.write_tool_call(&name, &arguments)?;
            renderer.tick_spinner()
        }
        AgentEvent::ToolResult { name, ok, output } => {
            renderer.write_tool_result(&name, ok, &output)?;
            renderer.tick_spinner()
        }
        AgentEvent::ToolProgress { name, message } => {
            renderer.write_tool_progress(&name, &message)?;
            renderer.tick_spinner()
        }
        AgentEvent::CommandOutput {
            name,
            stream,
            chunk,
        } => {
            renderer.write_command_output(&name, stream, &chunk)?;
            renderer.tick_spinner()
        }
        AgentEvent::PrepareForExternalOutput { ready } => {
            renderer.prepare_for_external_output()?;
            let _ = ready.send(());
            Ok(())
        }
        AgentEvent::AskQuestion { request, responder } => {
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
        AgentEvent::SpinnerTick => renderer.tick_spinner(),
        AgentEvent::CompactStart => {
            renderer.write_system_message("正在压缩上下文...")?;
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
    }
}
