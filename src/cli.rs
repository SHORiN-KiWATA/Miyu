use crate::agent::{Agent, AgentEvent, AgentMode};
use crate::config::AppConfig;
use crate::i18n::{is_zh, text as t};
use crate::llm::OpenAiCompatibleClient;
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
use crossterm::style::{Attribute, Print, SetAttribute};
use crossterm::terminal::{self, Clear, ClearType};
use crossterm::{execute, queue};
use fuzzy_matcher::skim::SkimMatcherV2;
use fuzzy_matcher::FuzzyMatcher;
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
    pub shell: Option<String>,

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
        (
            "providers",
            "List or switch provider/model",
            "列出或切换 provider/模型",
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
        .mut_subcommand("providers", localize_providers_command)
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

fn localize_providers_command(command: clap::Command) -> clap::Command {
    command.mut_arg("index", |arg| {
        arg.help(t(
            "Provider/model list index to activate",
            "要激活的 provider/模型列表序号",
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
    Providers(ProvidersArgs),
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
pub struct ProvidersArgs {
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
        let message = join_message(cli.message);
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
        Some(Command::Providers(args)) => run_providers(&paths, args),
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
    let registry = build_tool_registry(&config, paths, mode)?;
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
        secrets_file: PathBuf::new(),
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

fn run_providers(paths: &MiyuPaths, args: ProvidersArgs) -> Result<()> {
    let mut config = AppConfig::load(paths)?;
    let choices = config.provider_model_choices();
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
        if let Some(index) = inline_fuzzy_select(
            &choices
                .iter()
                .map(|choice| choice.label())
                .collect::<Vec<_>>(),
        )? {
            let choice = &choices[index];
            let provider_id = choice.provider_id.clone();
            let model = choice.model.clone();
            let label = choice.label();
            config.set_active_provider_model(&provider_id, &model)?;
            config.save(paths)?;
            println!(
                "{}: {}. {label}",
                t("active provider", "当前 provider"),
                index + 1
            );
        }
        return Ok(());
    }
    for (index, choice) in choices.iter().enumerate() {
        let active = config
            .provider(None)
            .map(|provider| {
                provider.id == choice.provider_id && provider.default_model == choice.model
            })
            .unwrap_or(false);
        let marker = if active { "*" } else { " " };
        println!("{marker} {}. {}", index + 1, choice.label());
    }
    Ok(())
}

fn inline_fuzzy_select(items: &[String]) -> Result<Option<usize>> {
    let menu_lines = inline_fuzzy_lines(items.len());
    reserve_inline_fuzzy_space(menu_lines)?;
    let mut session = InlineRawMode::start()?;
    let matcher = SkimMatcherV2::default();
    let mut query = String::new();
    let mut selected = 0usize;
    let (_, cursor_y) = cursor::position().unwrap_or((0, menu_lines.saturating_sub(1)));
    let anchor_y = cursor_y.saturating_sub(menu_lines.saturating_sub(1));
    loop {
        let matches = fuzzy_matches(&matcher, items, &query);
        if selected >= matches.len() {
            selected = matches.len().saturating_sub(1);
        }
        draw_inline_fuzzy(
            &mut session.stdout,
            anchor_y,
            menu_lines,
            &query,
            items,
            &matches,
            selected,
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
                    return Ok(matches.get(selected).map(|(_, index)| *index));
                }
                KeyCode::Up | KeyCode::Char('k') => selected = selected.saturating_sub(1),
                KeyCode::Down | KeyCode::Char('j') => {
                    selected = (selected + 1).min(matches.len().saturating_sub(1));
                }
                KeyCode::Backspace => {
                    query.pop();
                    selected = 0;
                }
                KeyCode::Char(ch) if !modifiers.contains(KeyModifiers::CONTROL) => {
                    query.push(ch);
                    selected = 0;
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
) -> Result<()> {
    let (cols, _) = terminal::size().unwrap_or((80, 24));
    let width = cols.saturating_sub(2).max(24) as usize;
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
        Print(truncate_display(&format!("> {query}"), width)),
    )?;
    if matches.is_empty() {
        queue!(
            stdout,
            MoveTo(0, anchor_y + 1),
            Print(t("  no matches", "  没有匹配项"))
        )?;
    } else {
        for (row, (_, item_index)) in matches.iter().take(visible).enumerate() {
            let marker = if row == selected { ">" } else { " " };
            let line = truncate_display(&format!("{marker} {}", items[*item_index]), width);
            queue!(stdout, MoveTo(0, anchor_y + row as u16 + 1))?;
            if row == selected {
                queue!(
                    stdout,
                    SetAttribute(Attribute::Reverse),
                    Print(line),
                    SetAttribute(Attribute::Reset)
                )?;
            } else {
                queue!(stdout, Print(line))?;
            }
        }
    }
    queue!(
        stdout,
        MoveTo(0, anchor_y + menu_lines.saturating_sub(1)),
        Print(truncate_display(
            t(
                "[type] search  [j/k] move  [enter] select  [esc/q] cancel",
                "[输入] 搜索  [j/k] 移动  [enter] 选择  [esc/q] 取消",
            ),
            width
        ))
    )?;
    stdout.flush()?;
    Ok(())
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
        _ => {
            std::process::exit(1);
        }
    }
}

async fn run_shell_intercept(paths: &MiyuPaths, shell_name: &str, message: String) -> Result<()> {
    if !matches!(shell_name, "fish" | "bash" | "zsh") {
        bail!("{}: {shell_name}", t("unsupported shell", "不支持的 shell"));
    }
    if message.is_empty() || !shell::looks_like_natural_language(&message) {
        bail!(
            "{}",
            t("not a natural language command", "不是自然语言命令")
        );
    }

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
    let registry = build_tool_registry(&config, paths, AgentMode::Normal)?;
    let reasoning_mode = render::ReasoningDisplayMode::from_config(&config.display.reasoning);
    let tool_call_mode = render::ToolCallDisplayMode::from_config(&config.display.tool_calls);
    let readable_tool_names = config.display.readable_tool_names;
    let show_token_usage = config.display.show_token_usage;
    let mut agent = Agent::new(
        config,
        paths,
        state.clone(),
        client,
        registry,
        AgentMode::Normal,
    )?;
    let mut renderer =
        render::StreamRenderer::new(reasoning_mode, tool_call_mode, false, readable_tool_names);
    renderer.start_waiting()?;
    let result = agent
        .chat_stream_with_images(&message, &pasted_images, |event| {
            handle_agent_event(&mut renderer, event)
        })
        .await;
    renderer.finish()?;
    let result = result?;
    print_chat_token_usage(&result, show_token_usage, state.token_total()?)?;
    handle_post_turn_overflow(&agent, &mut renderer, &result, show_token_usage, &state).await?;
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
    let registry = build_tool_registry(&config, paths, mode)?;
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
    let show_token_usage = config.display.show_token_usage && !plain;
    let mut agent = Agent::new(config, paths, state.clone(), client, registry, mode)?;
    let mut renderer =
        render::StreamRenderer::new(reasoning_mode, tool_call_mode, plain, readable_tool_names);
    renderer.start_waiting()?;
    let result = agent
        .chat_stream(&message, |event| handle_agent_event(&mut renderer, event))
        .await;
    renderer.finish()?;
    let result = result?;
    print_chat_token_usage(&result, show_token_usage, state.token_total()?)?;
    handle_post_turn_overflow(&agent, &mut renderer, &result, show_token_usage, &state).await?;
    Ok(())
}

fn print_chat_token_usage(
    result: &crate::llm::ChatResult,
    enabled: bool,
    session_token_total: u64,
) -> Result<()> {
    if enabled {
        if let Some(usage) = &result.usage {
            let turn_tokens = render::usage_total(usage);
            render::print_token_usage(turn_tokens, session_token_total, result.usage_estimated)?;
        }
    }
    Ok(())
}

async fn handle_post_turn_overflow(
    agent: &Agent,
    renderer: &mut render::StreamRenderer,
    result: &crate::llm::ChatResult,
    show_token_usage: bool,
    state: &StateStore,
) -> Result<()> {
    let Some(usage) = result.usage.as_ref() else {
        return Ok(());
    };
    let compact_result = agent
        .handle_overflow_after_turn(usage, |event| handle_agent_event(renderer, event))
        .await?;
    renderer.finish()?;
    if let Some(compact_result) = compact_result {
        print_chat_token_usage(&compact_result, show_token_usage, state.token_total()?)?;
    }
    Ok(())
}

async fn run_repl(paths: &MiyuPaths, initial_mode: AgentMode) -> Result<()> {
    AppConfig::init_files(paths)?;
    let mut config = AppConfig::load_or_default(paths)?;
    let state = StateStore::new(paths)?;
    state.init_files()?;
    let mut client = OpenAiCompatibleClient::from_config(&config, paths)?;
    let mut mode = initial_mode;
    let mut input_history = load_repl_input_history(&state)?;
    let mut prefill = None::<String>;

    println!(
        "\x1b[2m{}\x1b[0m",
        t(
            "Tab cycles NORMAL/PLAN/CHAT; Enter sends; Ctrl+J inserts newline; Ctrl+V pastes clipboard",
            "Tab 循环切换 普通/计划/闲聊；Enter 发送；Ctrl+J 换行；Ctrl+V 粘贴剪贴板",
        )
    );
    crate::default_kb::check_update_if_due(paths).ok();
    if let Ok(Some(message)) = crate::default_kb::notice_if_update_available(paths) {
        println!("\x1b[2m{message}\x1b[0m");
    }
    loop {
        let (input, pasted_images) = match read_repl_input(mode, prefill.take(), &input_history)? {
            Some((new_mode, input, pasted_images)) => {
                mode = new_mode;
                (input, pasted_images)
            }
            None => break,
        };
        let input = input.trim();
        if input.eq_ignore_ascii_case("exit")
            || input.eq_ignore_ascii_case("quit")
            || input.eq_ignore_ascii_case("/exit")
        {
            break;
        }
        if input.eq_ignore_ascii_case("/help") {
            print_repl_help();
            continue;
        }
        if input.eq_ignore_ascii_case("/plan") {
            mode = AgentMode::Plan;
            println!("{}: {}", t("mode", "模式"), mode.label());
            continue;
        }
        if input.eq_ignore_ascii_case("/chat") {
            mode = AgentMode::Chat;
            println!("{}: {}", t("mode", "模式"), mode.label());
            continue;
        }
        if input.eq_ignore_ascii_case("/normal") {
            mode = AgentMode::Normal;
            println!("{}: {}", t("mode", "模式"), mode.label());
            continue;
        }
        if input.eq_ignore_ascii_case("/providers") {
            run_providers(paths, ProvidersArgs { index: None })?;
            reload_repl_config(paths, &mut config, &mut client)?;
            println!("{}", t("configuration reloaded", "配置已重新加载"));
            continue;
        }
        if input.eq_ignore_ascii_case("/config") {
            crate::config_tui::run(paths)?;
            reload_repl_config(paths, &mut config, &mut client)?;
            println!("{}", t("configuration reloaded", "配置已重新加载"));
            continue;
        }
        if input.eq_ignore_ascii_case("/undo") {
            let (removed, prompt) = state.undo_last_turn()?;
            println!("{}: {removed}", t("undone messages", "已撤销消息数"));
            prefill = prompt;
            continue;
        }
        if input.eq_ignore_ascii_case("/reset") {
            run_reset(paths, None)?;
            input_history.clear();
            continue;
        }
        if input.eq_ignore_ascii_case("/reset all") {
            run_reset(paths, Some("all"))?;
            input_history.clear();
            continue;
        }
        if input.is_empty() {
            continue;
        }
        input_history.push(input.to_string());
        let registry = build_tool_registry(&config, paths, mode)?;
        let mut agent = Agent::new(
            config.clone(),
            paths,
            state.clone(),
            client.clone(),
            registry,
            mode,
        )?;
        let reasoning_mode = render::ReasoningDisplayMode::from_config(&config.display.reasoning);
        let tool_call_mode = render::ToolCallDisplayMode::from_config(&config.display.tool_calls);
        let mut renderer = render::StreamRenderer::new(
            reasoning_mode,
            tool_call_mode,
            false,
            config.display.readable_tool_names,
        );
        renderer.start_waiting()?;
        let chat_result = {
            let chat = agent.chat_stream_with_images(input, &pasted_images, |event| {
                handle_agent_event(&mut renderer, event)
            });
            tokio::pin!(chat);
            tokio::select! {
                result = &mut chat => result.map(Some),
                signal = tokio::signal::ctrl_c() => {
                    signal?;
                    Ok(None)
                }
            }
        };
        renderer.finish()?;
        match chat_result {
            Ok(Some(result)) => {
                print_chat_token_usage(
                    &result,
                    config.display.show_token_usage,
                    state.token_total()?,
                )?;
                if let Err(err) = handle_post_turn_overflow(
                    &agent,
                    &mut renderer,
                    &result,
                    config.display.show_token_usage,
                    &state,
                )
                .await
                {
                    eprintln!("\x1b[31m{}: {err}\x1b[0m", t("error", "错误"));
                    continue;
                }
            }
            Ok(None) => {}
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
        "  /providers  {}",
        t("switch provider or model", "切换 provider 或模型")
    );
    println!(
        "  /config     {}",
        t("open configuration UI", "打开配置界面")
    );
    println!(
        "  /plan       {}",
        t("switch to read-only planning mode", "切换到只读计划模式")
    );
    println!(
        "  /normal     {}",
        t("switch to normal mode", "切换到普通模式")
    );
    println!(
        "  /chat       {}",
        t(
            "switch to lightweight chat mode with web and memes only",
            "切换到仅开放网络和表情的轻量闲聊模式"
        )
    );
    println!(
        "  /undo       {}",
        t(
            "remove last turn and restore prompt",
            "撤销上一轮并恢复输入"
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
    mut mode: AgentMode,
    prefill: Option<String>,
    history: &[String],
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
                    if !history.is_empty() {
                        history_index = history_index.saturating_sub(1);
                        input = history.get(history_index).cloned().unwrap_or_default();
                        cursor = input.chars().count();
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
                }
                KeyCode::Down => {
                    if history_index + 1 < history.len() {
                        history_index += 1;
                        input = history.get(history_index).cloned().unwrap_or_default();
                    } else {
                        history_index = history.len();
                        input.clear();
                    }
                    cursor = input.chars().count();
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
                KeyCode::Enter => {
                    input = strip_terminal_control_sequences(&input);
                    input = expand_pasted_text_placeholders(&input, &pasted_texts);
                    move_after_repl_input(&mut stdout, input_row, rendered_rows)?;
                    execute!(stdout, DisableBracketedPaste)?;
                    terminal::disable_raw_mode()?;
                    return Ok(Some((mode, input, pasted_images)));
                }
                KeyCode::Char('j') if modifiers.contains(KeyModifiers::CONTROL) => {
                    insert_newline_at_cursor(&mut input, &mut cursor);
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
                            let placeholder = format!("[Image {}]", index);
                            insert_str_at_cursor(&mut input, &mut cursor, &placeholder);
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

fn render_repl_input(
    stdout: &mut io::Stdout,
    input_row: &mut u16,
    rendered_rows: &mut u16,
    mode: AgentMode,
    input: &str,
    cursor: usize,
    is_pasted: bool,
) -> Result<()> {
    let suggestions = repl_command_suggestions(input);
    let lines = repl_input_lines(input);
    let prompt_prefix = format!("{} > ", colored_mode_label(mode));
    let plain_prefix = format!("[{}] > ", mode.label());
    let display_lines = repl_visible_input_lines(
        &plain_prefix,
        &lines,
        REPL_MAX_VISIBLE_INPUT_ROWS,
        is_pasted,
    );
    let display_lines: Vec<String> = display_lines
        .iter()
        .map(|line| colorize_repl_placeholders(line))
        .collect();
    let current_rows = repl_render_rows(&plain_prefix, &display_lines, !suggestions.is_empty());
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
    for (index, line) in display_lines.iter().enumerate() {
        let row = (*input_row).saturating_add(row_offset);
        queue!(stdout, MoveTo(0, row))?;
        if index == 0 {
            queue!(stdout, Print(&prompt_prefix), Print(line))?;
            row_offset = row_offset.saturating_add(repl_line_rows(&plain_prefix, line));
        } else {
            queue!(stdout, Print(line))?;
            row_offset = row_offset.saturating_add(repl_line_rows("", line));
        }
    }
    if !suggestions.is_empty() {
        let suggestion_row =
            (*input_row).saturating_add(repl_prompt_rows(&plain_prefix, &display_lines));
        queue!(
            stdout,
            MoveTo(0, suggestion_row),
            Print(format!("\x1b[2m{}\x1b[0m", suggestions.join("  ")))
        )?;
    }
    let (cursor_col, cursor_row_offset) = if display_lines.len() == lines.len() {
        repl_cursor_position(&plain_prefix, input, cursor)
    } else {
        let last_line = display_lines.last().map(String::as_str).unwrap_or_default();
        let col = (visible_width(last_line) % terminal_cols()) as u16;
        (
            col,
            repl_prompt_rows(&plain_prefix, &display_lines).saturating_sub(1),
        )
    };
    queue!(
        stdout,
        MoveTo(cursor_col, (*input_row).saturating_add(cursor_row_offset))
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

fn repl_render_rows(prefix: &str, lines: &[String], has_suggestions: bool) -> u16 {
    repl_prompt_rows_for_cols(prefix, lines, terminal_cols()) + u16::from(has_suggestions)
}

fn repl_prompt_rows(prefix: &str, lines: &[String]) -> u16 {
    repl_prompt_rows_for_cols(prefix, lines, terminal_cols())
}

fn repl_cursor_position(prefix: &str, input: &str, cursor: usize) -> (u16, u16) {
    repl_cursor_position_for_cols(prefix, input, cursor, terminal_cols())
}

fn repl_line_rows(prefix: &str, line: &str) -> u16 {
    repl_line_rows_for_cols(prefix, line, terminal_cols())
}

fn repl_line_rows_for_cols(prefix: &str, line: &str, cols: usize) -> u16 {
    let cols = cols.max(1);
    let width = visible_width(prefix) + visible_width(line);
    (width / cols + 1).min(u16::MAX as usize) as u16
}

fn repl_prompt_rows_for_cols(prefix: &str, lines: &[String], cols: usize) -> u16 {
    let cols = cols.max(1);
    let mut rows = 0usize;
    for (index, line) in lines.iter().enumerate() {
        rows += repl_line_rows_for_cols(if index == 0 { prefix } else { "" }, line, cols) as usize;
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
        let width = if index == 0 {
            visible_width(prefix) + visible_width(line)
        } else {
            visible_width(line)
        };
        if index == last_index {
            return (
                (width % cols).min(u16::MAX as usize) as u16,
                (row_offset + width / cols).min(u16::MAX as usize) as u16,
            );
        }
        row_offset += width / cols + 1;
    }
    (visible_width(prefix).min(u16::MAX as usize) as u16, 0)
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

fn colored_mode_label(mode: AgentMode) -> String {
    let label = mode.label();
    match mode {
        AgentMode::Normal => format!("\x1b[1m\x1b[34m[{label}]\x1b[0m"),
        AgentMode::Plan => format!("\x1b[1m\x1b[36m[{label}]\x1b[0m"),
        AgentMode::Chat => format!("\x1b[1m\x1b[32m[{label}]\x1b[0m"),
    }
}

fn repl_commands() -> [&'static str; 9] {
    [
        "/providers",
        "/config",
        "/plan",
        "/normal",
        "/chat",
        "/undo",
        "/reset",
        "/help",
        "/exit",
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

#[cfg(test)]
mod repl_input_tests {
    use super::*;

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
    fn reset_is_a_repl_command() {
        assert!(repl_commands().contains(&"/reset"));
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
            secrets_file: PathBuf::new(),
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
        println!("{} {}", entry.timestamp, entry.role);
        if entry.role == "assistant" {
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
    println!(
        "{}",
        if all {
            t(
                "cleared current conversation history and all memory",
                "已清空当前会话历史与全部记忆",
            )
        } else {
            t("cleared current conversation history", "已清空当前会话历史")
        }
    );
    Ok(())
}

fn join_message(parts: Vec<String>) -> String {
    parts.join(" ").trim().to_string()
}

fn build_tool_registry(
    config: &AppConfig,
    paths: &MiyuPaths,
    mode: AgentMode,
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
        AgentEvent::ExternalOutput => renderer.prepare_for_external_output(),
        AgentEvent::SpinnerTick => renderer.tick_spinner(),
        AgentEvent::MemeSelectPhase => {
            renderer.set_meme_select_phase()?;
            renderer.tick_spinner()
        }
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
