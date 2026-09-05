//! 命令行参数定义。
//!
//! `extract_debug_flag` 在 clap 之前手工扫一遍 `--debug`：它要在日志初始化之前
//! 就生效，而那时命令行还没解析。

use crate::cli::*;

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

pub(in crate::cli) fn parse_args(mut args: Vec<OsString>) -> std::result::Result<Cli, clap::Error> {
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

pub(in crate::cli) fn extract_debug_flag(args: &mut Vec<OsString>) -> bool {
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
    /// 用麦克风说一句话,识别成文字后当作消息发送(需开启语音功能)
    Stt,
    /// 让语音前端立刻进入收听状态,不用喊唤醒词(给桌面快捷键用)
    Listen,
    /// 语音会话与播报管理:say / reset / history / status
    Voice(VoiceArgs),
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
    /// Semantic embedding: status, installed models, rebuild vectors.
    Embed(EmbedArgs),
    Export(ExportArgs),
    Import(ImportArgs),
    UpdateDefaultKb,
    Memory(MemoryArgs),
    Skills(SkillsArgs),
    Reset,
    #[command(name = "reset-memory")]
    ResetMemoryCli,
    Wipe(WipeArgs),
    Web(WebArgs),
    Daemon(DaemonArgs),
    /// 进入普通模式 REPL(人格全能力)
    Normal,
    /// 进入开发模式 REPL(极简编码形态,无人格)
    Dev,
    /// 工具桥:以当前会话身份调用一个结构化工具(供 run_command 脚本编排)
    #[command(name = "tool-call")]
    ToolCallCmd(ToolCallArgs),
    /// MCP stdio 工具桥(claude-code 供应商内部使用,由 claude 拉起)
    #[command(name = "mcp-serve", hide = true)]
    McpServe,
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

    /// `request`:开启出网请求录制并实时监控;Ctrl+C 停止并关闭录制
    #[arg(value_name = "TOPIC")]
    pub topic: Option<String>,
}

#[derive(Debug, Args)]
pub struct ToolArgs {
    pub name: String,
    pub arguments: Option<String>,
}

/// 工具桥:以本会话身份(MIYU_SESSION)调用结构化工具。--list 列出的即
/// 本会话可调用的集合;内层调用在 daemon 侧的会话工作区执行,不继承本
/// shell 的环境变量与当前目录,跨工具传数据走参数 JSON 或文件。
#[derive(Debug, Args)]
pub struct ToolCallArgs {
    /// 工具名(--list 时可省略)
    pub name: Option<String>,
    /// 参数 JSON(便捷位置参数;脚本里推荐 --stdin 免引号地狱)
    pub arguments: Option<String>,
    /// 从标准输入读参数 JSON(跨 shell 安全,PowerShell 也能用)
    #[arg(long = "stdin")]
    pub args_stdin: bool,
    /// 从文件读参数 JSON
    #[arg(long)]
    pub args_file: Option<std::path::PathBuf>,
    /// 列出本会话当前可调用的工具(名称+显示名)
    #[arg(long)]
    pub list: bool,
    /// 打印指定工具的完整合同(描述+参数 schema)
    #[arg(long)]
    pub describe: bool,
}

#[derive(Debug, Args)]
pub struct ConfigArgs {
    #[command(subcommand)]
    pub command: Option<ConfigCommand>,
}

#[derive(Debug, Args)]
pub struct VoiceArgs {
    #[command(subcommand)]
    pub command: VoiceCommand,
}

#[derive(Debug, Subcommand)]
pub enum VoiceCommand {
    /// 用当前 TTS 配置合成并播出一句话(试听)
    Say { text: String },
    /// 清空唤醒对话的专属会话(下次唤醒重新开始)
    Reset,
    /// 打印语音会话最近的对话
    History {
        /// 最多打印多少轮
        #[arg(long, default_value_t = 10)]
        limit: usize,
    },
    /// 语音前端状态
    Status,
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
pub struct ModelsArgs {
    /// 1-based list index, `provider/model`, a bare model name, or
    /// `default` to follow the global active pool again.
    pub target: Option<String>,

    /// 改的是全局激活模型池，而不是当前终端集成会话的覆盖。
    /// 全局池是所有没有单独覆盖的会话（含 WebUI 与通讯平台）的默认来源。
    #[arg(short = 'g', long = "global")]
    pub global: bool,
}

#[derive(Debug, Args)]
pub struct VariantArgs {
    pub name: Option<String>,
}

#[derive(Debug, Subcommand)]
pub enum ConfigCommand {
    Validate,
    Paths,
    #[command(hide = true)]
    PromptSource,
}
