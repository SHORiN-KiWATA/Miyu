//! 命令行参数定义。
//!
//! `extract_debug_flag` 在 clap 之前手工扫一遍 `--debug`：它要在日志初始化之前
//! 就生效，而那时命令行还没解析。

use crate::cli::*;
use std::path::PathBuf;

#[derive(Debug, Parser)]
#[command(name = "miyu", version, about = "Miyu CLI AI Agent")]
pub struct Cli {
    #[arg(long, global = true)]
    pub debug: bool,

    /// 纯文本输出(= `--output-format text --quiet`),保留给老脚本。
    #[arg(long)]
    pub stdout: bool,

    /// 一次性对话的回合选项;`miyu ask` 子命令上同样一套,子命令的赢。
    #[command(flatten)]
    pub turn: TurnOptions,

    #[arg(long, hide = true)]
    pub shell_intercept: bool,

    #[arg(long, hide = true)]
    pub shell_classify: bool,

    #[arg(long, hide = true)]
    pub shell: Option<String>,

    /// 从标准输入读正文(读到 EOF,不再受 5 秒探测限制),并入消息尾部。
    #[arg(long)]
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
    Compact(CompactArgs),
    Kb(KbArgs),
    /// Semantic embedding: status, installed models, rebuild vectors.
    Embed(EmbedArgs),
    Export(ExportArgs),
    Import(ImportArgs),
    UpdateDefaultKb,
    Memory(MemoryArgs),
    Skills(SkillsArgs),
    Reset(ResetArgs),
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
    /// 会话管理:list / new / show / delete / rename / clear / pop / compact / models / workspace
    Session(SessionArgs),
    /// 长驻协议模式:stdin 一行一请求(JSON),stdout 一行一事件;宿主软件把 Miyu 当后端用
    Stdio,
}

/// 一次性回合的选项。根命令与 `ask` 子命令各 flatten 一份,`merged` 合并。
///
/// 覆盖类参数(模型/窗口/提示词/记忆/工具)全部只对本回合生效、不落盘;
/// 会话类参数(`--session --create --mode`)只在建会话时决定模式。
#[derive(Debug, Args, Clone, Default)]
pub struct TurnOptions {
    /// 仅为本次命令指定目标会话(名称、编号或 id),不改变全局当前会话
    #[arg(long, value_name = "SESSION")]
    pub session: Option<String>,

    #[arg(short = 'c', long = "continue", conflicts_with = "session")]
    pub continue_session: bool,

    /// `--session` 指名的会话不存在时新建(名字即会话名)
    #[arg(long, requires = "session")]
    pub create: bool,

    /// 新建会话的模式(normal/dev);对已有会话传了报错
    #[arg(long, value_name = "MODE", value_parser = ["normal", "dev"])]
    pub mode: Option<String>,

    /// 本回合模型:provider/model、裸名或 `list-models` 序号;不落盘
    #[arg(long, value_name = "MODEL")]
    pub model: Option<String>,

    /// 本回合上下文窗口(token 数);不落盘
    #[arg(long, value_name = "TOKENS")]
    pub context_window: Option<usize>,

    /// 整体替换系统提示词(文本或 @文件);每次调用都是缓存冷启动
    #[arg(long, value_name = "TEXT|@FILE")]
    pub system_prompt: Option<String>,

    /// 追加在系统提示词末尾的宿主指令(文本或 @文件);每回合同一段则缓存稳定
    #[arg(long, value_name = "TEXT|@FILE")]
    pub append_system_prompt: Option<String>,

    /// 本回合不写长期记忆、日记与经历
    #[arg(long)]
    pub no_memory: bool,

    /// 工具白名单,逗号分隔
    #[arg(
        long,
        value_name = "NAMES",
        value_delimiter = ',',
        conflicts_with = "no_tools"
    )]
    pub tools: Option<Vec<String>>,

    /// 本回合不给任何工具
    #[arg(long)]
    pub no_tools: bool,

    /// 附件(图片/视频/PDF),可多次
    #[arg(long, value_name = "PATH")]
    pub image: Vec<PathBuf>,

    /// 本回合工作区;缺省为调用方当前目录
    #[arg(long, value_name = "DIR")]
    pub cwd: Option<PathBuf>,

    /// 输出格式:text(默认)、json(一行终态)、stream-json(逐事件一行)
    #[arg(long, value_name = "FORMAT", value_enum)]
    pub output_format: Option<OutputFormat>,

    /// text 模式下不打进度与工具行
    #[arg(long)]
    pub quiet: bool,

    /// 超时秒数;到点取消回合,退出码 124
    #[arg(long, value_name = "SECS")]
    pub timeout: Option<u64>,
}

impl TurnOptions {
    /// 子命令那份压在根命令那份之上:子命令给了的项赢,没给的沿用根命令。
    pub fn merged(self, over: TurnOptions) -> TurnOptions {
        TurnOptions {
            session: over.session.or(self.session),
            continue_session: over.continue_session || self.continue_session,
            create: over.create || self.create,
            mode: over.mode.or(self.mode),
            model: over.model.or(self.model),
            context_window: over.context_window.or(self.context_window),
            system_prompt: over.system_prompt.or(self.system_prompt),
            append_system_prompt: over.append_system_prompt.or(self.append_system_prompt),
            no_memory: over.no_memory || self.no_memory,
            tools: over.tools.or(self.tools),
            no_tools: over.no_tools || self.no_tools,
            image: if over.image.is_empty() {
                self.image
            } else {
                over.image
            },
            cwd: over.cwd.or(self.cwd),
            output_format: over.output_format.or(self.output_format),
            quiet: over.quiet || self.quiet,
            timeout: over.timeout.or(self.timeout),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, clap::ValueEnum)]
pub enum OutputFormat {
    #[default]
    Text,
    Json,
    #[value(name = "stream-json")]
    StreamJson,
}

#[derive(Debug, Args)]
pub struct MessageArgs {
    #[command(flatten)]
    pub turn: TurnOptions,

    /// 从标准输入读正文(读到 EOF),并入消息尾部
    #[arg(long = "stdin")]
    pub read_stdin: bool,

    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    pub message: Vec<String>,
}

/// `miyu session …`:程序驱动的会话管理面,全部映射到 daemon 的会话 IPC。
#[derive(Debug, Args)]
pub struct SessionArgs {
    #[command(subcommand)]
    pub command: SessionCommand,
}

#[derive(Debug, Subcommand)]
pub enum SessionCommand {
    /// 列出当前人格的会话(普通+开发模式);`--json` 直出
    List {
        #[arg(long)]
        json: bool,
    },
    /// 新建会话
    New {
        name: String,
        #[arg(long, value_name = "MODE", value_parser = ["normal", "dev"])]
        mode: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// 查看会话详情(模式、工作区、轮数、上下文占用)
    Show {
        target: String,
        #[arg(long)]
        json: bool,
    },
    /// 删除会话
    Delete {
        target: String,
        /// 跳过确认
        #[arg(long)]
        yes: bool,
    },
    /// 重命名
    Rename { target: String, name: String },
    /// 清空会话上下文(历史与队列),会话本身保留
    Clear { target: String },
    /// 把最旧 N 轮移出活跃上下文
    Pop {
        target: String,
        #[arg(value_parser = parse_positive_pop_count)]
        count: usize,
    },
    /// 压缩会话上下文
    Compact { target: String },
    /// 查看/设置会话的模型覆盖(`default` 恢复跟随全局池)
    Models {
        target: String,
        model: Option<String>,
    },
    /// 查看/绑定会话工作区;`--clear` 解绑
    Workspace {
        target: String,
        dir: Option<PathBuf>,
        #[arg(long, conflicts_with = "dir")]
        clear: bool,
    },
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

    /// 目标会话(名字、编号或 id);缺省为终端集成会话
    #[arg(long, value_name = "SESSION")]
    pub session: Option<String>,
}

#[derive(Debug, Args)]
pub struct ResetArgs {
    /// 目标会话(名字、编号或 id);缺省为终端集成会话
    #[arg(long, value_name = "SESSION")]
    pub session: Option<String>,
}

/// `miyu compact`:立即压缩一个会话的上下文。与 `reset`/`pop` 同形——缺省打
/// 终端集成会话,`--session` 才换目标;程序驱动的宿主用 `miyu session compact`。
#[derive(Debug, Args)]
pub struct CompactArgs {
    /// 目标会话(名字、编号或 id);缺省为终端集成会话
    #[arg(long, value_name = "SESSION")]
    pub session: Option<String>,
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
