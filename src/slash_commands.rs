//! 斜杠命令的单一事实来源。
//!
//! 命令名、参数提示、帮助文案、以及「这一行输入是不是命令」的判定都在这里。
//! 原本长在 `cli::repl::commands` 里，是 `pub(in crate::cli)` 的——WebUI 够
//! 不到，只能自己再维护一份清单，两份迟早分叉（加一条命令忘了改另一边）。
//! 提到 crate 级之后 CLI 与 WebUI 同源，`GET /api/commands` 直接从这张表出。
//!
//! 命名避开 `commands`：`platforms::commands` 已经占了那个名字，同名子模块
//! 遮蔽上层兄弟模块在这个仓库里踩过三次（AGENTS.md 3.3 坑 3）。
//!
//! 渲染归 CLI：`print_repl_help` 与 `repl_command_suggestions_line` 要 println
//! 和终端宽度，留在 `cli::repl::commands`。

pub(crate) fn split_repl_command(input: &str) -> (&str, &str) {
    let Some((command, args)) = input.split_once(char::is_whitespace) else {
        return (input, "");
    };
    (command, args)
}
/// Identity of a REPL slash command, dispatched via `parse_repl_input`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReplSlashCommand {
    New,
    Session,
    Rename,
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
    Stt,
    Goal,
    Reset,
    ResetMemory,
    Wipe,
    History,
    Clear,
    Help,
    Exit,
}

pub(crate) struct ReplCommandSpec {
    pub(crate) name: &'static str,
    pub(crate) command: ReplSlashCommand,
    /// Argument hint rendered in /help, e.g. "[count]"; empty when the
    /// command takes no arguments (enforced at dispatch).
    pub(crate) arg_hint: &'static str,
    pub(crate) help_en: &'static str,
    pub(crate) help_zh: &'static str,
    /// 这条命令在 WebUI 的输入框里出不出现（`GET /api/commands` 按它过滤）。
    ///
    /// 绝大多数命令**不**开：WebUI 早就有对应的 GUI 入口（侧栏切会话、设置面板
    /// 改模型/人格、新建按钮），再给一个命令是两条路做同一件事，还得各自维护
    /// 一套确认弹窗。开的是 WebUI 里**没有别的办法做到**的那几件。
    pub(crate) web: bool,
}

impl ReplCommandSpec {
    pub(crate) fn help(&self) -> &'static str {
        if crate::i18n::is_zh() {
            self.help_zh
        } else {
            self.help_en
        }
    }
}

/// WebUI 输入框认得的命令。CLI 认得全部，两边同源于 `REPL_COMMAND_TABLE`。
pub(crate) fn web_commands() -> Vec<&'static ReplCommandSpec> {
    REPL_COMMAND_TABLE.iter().filter(|spec| spec.web).collect()
}

/// Single source of truth for REPL slash commands: drives Tab completion,
/// prefix resolution, /help output, and dispatch in the REPL loop.
pub(crate) const REPL_COMMAND_TABLE: &[ReplCommandSpec] = &[
    ReplCommandSpec {
        name: "/new",
        command: ReplSlashCommand::New,
        arg_hint: "[name]",
        help_en: "create a new session and switch to it",
        help_zh: "创建新会话并切换过去",
        web: false,
    },
    ReplCommandSpec {
        name: "/session",
        command: ReplSlashCommand::Session,
        arg_hint: "[name|index]",
        help_en: "list sessions, or switch to one (Ctrl+D deletes in the picker)",
        help_zh: "列出会话，或切换到指定会话（菜单内 Ctrl+D 删除）",
        web: false,
    },
    ReplCommandSpec {
        name: "/rename",
        command: ReplSlashCommand::Rename,
        arg_hint: "<name>",
        help_en: "rename the current session",
        help_zh: "重命名当前会话",
        web: false,
    },
    ReplCommandSpec {
        name: "/delete",
        command: ReplSlashCommand::Delete,
        arg_hint: "[name|index]",
        help_en: "delete a session (current by default)",
        help_zh: "删除会话（默认当前会话）",
        web: false,
    },
    ReplCommandSpec {
        name: "/workspace",
        command: ReplSlashCommand::Workspace,
        arg_hint: "[path|clear]",
        help_en: "show, bind, or unbind the session workspace",
        help_zh: "查看、绑定或解绑会话工作目录",
        web: false,
    },
    ReplCommandSpec {
        name: "/models",
        command: ReplSlashCommand::Models,
        arg_hint: "[index|provider/model|default]",
        help_en: "switch this session's model",
        help_zh: "切换当前会话使用的模型",
        web: false,
    },
    ReplCommandSpec {
        name: "/persona",
        command: ReplSlashCommand::Persona,
        arg_hint: "[name]",
        help_en: "switch the active persona",
        help_zh: "切换当前人格",
        web: false,
    },
    ReplCommandSpec {
        name: "/usage",
        command: ReplSlashCommand::Usage,
        arg_hint: "",
        help_en: "show token usage details",
        help_zh: "显示 Token 用量详情",
        web: false,
    },
    ReplCommandSpec {
        name: "/config",
        command: ReplSlashCommand::Config,
        arg_hint: "",
        help_en: "open configuration UI",
        help_zh: "打开配置界面",
        web: false,
    },
    ReplCommandSpec {
        name: "/variant",
        command: ReplSlashCommand::Variant,
        arg_hint: "[name]",
        help_en: "view or switch thinking level",
        help_zh: "查看或切换思考档位",
        web: false,
    },
    ReplCommandSpec {
        name: "/undo",
        command: ReplSlashCommand::Undo,
        arg_hint: "",
        help_en: "undo the last turn or context compaction",
        help_zh: "撤销上一轮或上下文压缩",
        web: false,
    },
    ReplCommandSpec {
        name: "/pop",
        command: ReplSlashCommand::Pop,
        arg_hint: "[count]",
        help_en: "pop selected turns or the oldest count from active context",
        help_zh: "从当前上下文弹出所选轮次或最旧的指定轮数",
        // WebUI 只有按数量的形态（交互式挑选依赖终端）。
        web: true,
    },
    ReplCommandSpec {
        name: "/compact",
        command: ReplSlashCommand::Compact,
        arg_hint: "",
        help_en: "compact current conversation context now",
        help_zh: "立即压缩当前会话上下文",
        web: true,
    },
    ReplCommandSpec {
        name: "/stt",
        command: ReplSlashCommand::Stt,
        arg_hint: "",
        help_en:
            "dictate with the microphone: speech goes into the input box; Esc stops (needs voice enabled)",
        help_zh: "用麦克风听写：说的话填进输入框(需开启语音功能);Esc 停止,回车发送",
        web: false,
    },
    ReplCommandSpec {
        name: "/goal",
        command: ReplSlashCommand::Goal,
        arg_hint: "[目标|edit <新目标>|pause|resume|clear]",
        help_en: "give the session a long task and let it keep working on it by itself",
        help_zh: "交代一件长活，之后它会自己一轮轮做下去；不带参数看进度",
        web: true,
    },
    ReplCommandSpec {
        name: "/reset",
        command: ReplSlashCommand::Reset,
        arg_hint: "",
        help_en: "start this conversation over",
        help_zh: "重新开始当前会话",
        web: true,
    },
    ReplCommandSpec {
        name: "/reset-memory",
        command: ReplSlashCommand::ResetMemory,
        arg_hint: "",
        help_en: "erase this mode's long-term memory",
        help_zh: "清空当前模式的长期记忆",
        web: true,
    },
    ReplCommandSpec {
        name: "/wipe",
        command: ReplSlashCommand::Wipe,
        arg_hint: "",
        help_en: "erase memory, every conversation, group contexts and generated skills",
        help_zh: "抹掉记忆、所有会话内容、群聊上下文和自动技能",
        web: false,
    },
    ReplCommandSpec {
        name: "/history",
        command: ReplSlashCommand::History,
        arg_hint: "",
        help_en: "show recent conversation history",
        help_zh: "显示最近的会话历史",
        web: false,
    },
    ReplCommandSpec {
        name: "/clear",
        command: ReplSlashCommand::Clear,
        arg_hint: "",
        help_en: "clear the screen",
        help_zh: "清屏",
        web: false,
    },
    ReplCommandSpec {
        name: "/help",
        command: ReplSlashCommand::Help,
        arg_hint: "",
        help_en: "show this help",
        help_zh: "显示此帮助",
        web: false,
    },
    ReplCommandSpec {
        name: "/exit",
        command: ReplSlashCommand::Exit,
        arg_hint: "",
        help_en: "leave REPL",
        help_zh: "退出 REPL",
        web: false,
    },
];

pub(crate) fn repl_command_spec(command: ReplSlashCommand) -> &'static ReplCommandSpec {
    REPL_COMMAND_TABLE
        .iter()
        .find(|spec| spec.command == command)
        .expect("every ReplSlashCommand has a table entry")
}
/// Parsed REPL input: plain chat, a resolved slash command with its argument
/// string, or an unknown/ambiguous slash command.
/// 一行输入的归类。**没有「未知命令」这一类**——`/` 开头但不命中命令表的输入
/// 就是聊天（见 `parse_repl_input` 的文档）。
pub(crate) enum ReplInput<'a> {
    Chat,
    Slash(ReplSlashCommand, &'a str),
}

pub(crate) fn repl_commands() -> Vec<&'static str> {
    REPL_COMMAND_TABLE.iter().map(|spec| spec.name).collect()
}

pub(crate) fn repl_command_suggestions(input: &str) -> Vec<&'static str> {
    if !input.starts_with('/') {
        return Vec::new();
    }
    repl_commands()
        .into_iter()
        .filter(|command| command.starts_with(input))
        .collect()
}

pub(crate) fn complete_repl_command(input: &str) -> Option<&'static str> {
    let suggestions = repl_command_suggestions(input);
    if suggestions.len() == 1 {
        suggestions.first().copied()
    } else {
        None
    }
}

/// 命令表里有没有这个**完整**名字。执行前的唯一判定入口——直连道的 if 链和
/// 泄漏守门都问它，不再走前缀展开（理由见 `parse_repl_input`）。
pub(crate) fn is_repl_command(name: &str) -> bool {
    REPL_COMMAND_TABLE
        .iter()
        .any(|spec| spec.name.eq_ignore_ascii_case(name))
}
