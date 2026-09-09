//! clap 帮助文本的中文化。
//!
//! clap 本身不支持多语言，所以这里在构建好的 `Command` 上逐个子命令替换描述与
//! 模板。做法笨但可控：不用维护一份平行的命令定义，加新参数时忘了翻译也只是
//! 显示英文，不会不一致到出错。

use crate::cli::*;

pub(in crate::cli) fn localized_command() -> clap::Command {
    let mut command = Cli::command();
    command = command
        .about(t("Miyu AI assistant", "Miyu AI 助手"))
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
    // 终端无缝集成组在根帮助里以静态段单独成节(这些子命令已 hide,
    // 不进 {subcommands});最后设置以免被上面的通用中文模板覆盖。
    command = command.help_template(root_help_template());
    command
}

pub(in crate::cli) fn root_help_template() -> String {
    let shell_block = t(
        "  fish-init          Integrate with fish; then chat in natural language directly in the terminal
  bash-init          Integrate with bash
  zsh-init           Integrate with zsh
  remove-shell-hook  Safely remove installed Miyu shell hooks
  models             Switch the terminal session's model (-g edits the global pool)
  variant            Switch the terminal session model's thinking level
  history            Show conversation history
  reset              Clear the terminal-integration session context
  reset-memory       Erase the long-term memory this terminal session produced
  reset-all-memory   Erase this persona's entire long-term memory
  pop                Move conversation turns out of active context
  compact            Compact the terminal-integration session context now",
        "  fish-init          集成到 fish，集成后可在终端直接使用自然语言交流
  bash-init          集成到 bash
  zsh-init           集成到 zsh
  remove-shell-hook  安全删除已安装的 Miyu shell hook
  models             修改终端集成会话的模型（-g 改全局模型池）
  variant            切换终端集成会话模型的思考档位
  history            显示会话历史
  reset              清除终端集成会话上下文
  reset-memory       清空本次终端会话记下的长期记忆
  reset-all-memory   清空当前人格的全部长期记忆
  pop                将对话轮次移出当前上下文
  compact            立即压缩终端集成会话上下文",
    );
    if is_zh() {
        format!(
            "{{about}}

用法: {{usage}}

命令:
{{subcommands}}

终端无缝集成相关：
{shell_block}

参数:
{{positionals}}
选项:
{{options}}
{{after-help}}"
        )
    } else {
        format!(
            "{{about}}

Usage: {{usage}}

Commands:
{{subcommands}}

Terminal integration:
{shell_block}

Arguments:
{{positionals}}
Options:
{{options}}
{{after-help}}"
        )
    }
}

pub(in crate::cli) fn apply_localized_help_flags(
    mut command: clap::Command,
    root: bool,
) -> clap::Command {
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

pub(in crate::cli) fn apply_chinese_help_template(mut command: clap::Command) -> clap::Command {
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

pub(in crate::cli) fn localize_top_args(command: clap::Command) -> clap::Command {
    localize_turn_options(command)
        .mut_arg("stdin", |arg| {
            arg.help(t(
                "Read the message body from stdin up to EOF (no probe timeout)",
                "从标准输入读正文(读到 EOF,不受探测超时限制)",
            ))
        })
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
                "Send the message into the terminal-integration session instead of a throwaway one-shot chat",
                "把消息发进终端集成会话，而不是用完即弃的一次性对话",
            ))
        })
        .mut_arg("message", |arg| {
            arg.help(t(
                "Message to send; omitted to enter REPL",
                "要发送的消息；省略则进入 REPL",
            ))
        })
}

pub(in crate::cli) fn localize_subcommands(mut command: clap::Command) -> clap::Command {
    let descriptions = [
        (
            "session",
            "Manage sessions: list / new / show / delete / rename / clear / pop / compact / models / workspace",
            "会话管理:list / new / show / delete / rename / clear / pop / compact / models / workspace",
        ),
        (
            "stdio",
            "Long-running protocol mode: one JSON request per stdin line, one JSON event per stdout line",
            "长驻协议模式:stdin 一行一请求(JSON),stdout 一行一事件;宿主软件把 Miyu 当后端用",
        ),
        (
            "ask",
            "Send one message to the assistant as a one-shot chat",
            "向助手发送一条消息，一次性对话",
        ),
        (
            "normal",
            "Enter the normal-mode REPL (full persona abilities)",
            "进入普通模式 REPL（人格全能力）",
        ),
        (
            "dev",
            "Enter the dev-mode REPL (minimal coding form, no persona)",
            "进入开发模式 REPL（极简编码形态，无人格）",
        ),
        (
            "tool-call",
            "Tool bridge: call this session's AI tools from the command line",
            "工具桥：以本会话身份调用 AI 工具",
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
        ("config", "Configure via the TUI", "使用 TUI 进行配置"),
        ("reload", "Reload configuration", "重新加载配置"),
        (
            "models",
            "Switch the terminal-integration session's model",
            "修改终端集成会话的模型",
        ),
        ("list-models", "List available models", "列出可用模型"),
        (
            "variant",
            "Switch the terminal session model's thinking level",
            "切换终端集成会话模型的思考档位",
        ),
        (
            "fish-init",
            "Integrate with fish so you can chat in natural language directly in the terminal",
            "集成到 fish，集成后可在终端直接使用自然语言交流。",
        ),
        ("bash-init", "Integrate with bash", "集成到 bash"),
        ("zsh-init", "Integrate with zsh", "集成到 zsh"),
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
        (
            "compact",
            "Compact the terminal-integration session context now",
            "立即压缩终端集成会话上下文",
        ),
        ("kb", "Manage the knowledge base", "管理知识库"),
        (
            "update-default-kb",
            "Update Miyu default knowledge base",
            "更新 Miyu 默认知识库",
        ),
        ("memory", "Manage assistant memory", "管理记忆"),
        ("skills", "Manage assistant skills", "管理助手 skills"),
        (
            "reset",
            "Clear the terminal-integration session context",
            "清除终端集成会话上下文",
        ),
        (
            "reset-memory",
            "Erase the long-term memory this terminal session produced",
            "清空本次终端会话记下的长期记忆",
        ),
        (
            "reset-all-memory",
            "Erase this persona's entire long-term memory",
            "清空当前人格的全部长期记忆",
        ),
        (
            "wipe",
            "Erase all conversation history, memory, group contexts and their artifacts",
            "抹掉所有会话历史、记忆、群聊上下文和其产物",
        ),
        ("web", "Open the local Miyu WebUI", "访问本地 Miyu WebUI"),
        (
            "daemon",
            "Manage the unified Miyu background service",
            "管理 Miyu 统一后台服务",
        ),
        (
            "export",
            "Export configuration into a portable archive",
            "导出配置，把当前配置打包成可移植归档",
        ),
        ("import", "Import configuration", "导入配置"),
    ];
    for (name, en, zh) in descriptions {
        command = command.mut_subcommand(name, |subcommand| subcommand.about(t(en, zh)));
    }
    // 终端无缝集成组:从 {subcommands} 里藏掉,根帮助模板里以静态段
    // 单独成节(clap 不支持子命令分组);`miyu <cmd> -h` 不受影响。
    for name in [
        "fish-init",
        "bash-init",
        "zsh-init",
        "remove-shell-hook",
        "models",
        "variant",
        "history",
        "reset",
        "reset-memory",
        "reset-all-memory",
        "pop",
        "compact",
    ] {
        command = command.mut_subcommand(name, |subcommand| subcommand.hide(true));
    }
    for (index, name) in [
        "init",
        "config",
        "normal",
        "dev",
        "daemon",
        "web",
        "tool-call",
        "ask",
        "session",
        "stdio",
        "list-models",
        "export",
        "import",
        "kb",
        "memory",
        "skills",
        "update-default-kb",
        "wipe",
        "paths",
        "reload",
    ]
    .into_iter()
    .enumerate()
    {
        command = command.mut_subcommand(name, move |subcommand| subcommand.display_order(index));
    }
    command = command
        .mut_subcommand("ask", localize_ask_command)
        .mut_subcommand("session", localize_session_command)
        .mut_subcommand("models", localize_models_command)
        .mut_subcommand("variant", localize_variant_command)
        .mut_subcommand("history", localize_history_command)
        .mut_subcommand("pop", localize_pop_command)
        .mut_subcommand("reset", |command| {
            command.mut_arg("session", |arg| {
                arg.help(t(
                    "Target session (name, list number, or id); defaults to the terminal session",
                    "目标会话(名字、编号或 id);缺省为终端集成会话",
                ))
            })
        })
        .mut_subcommand("compact", |command| {
            command.mut_arg("session", |arg| {
                arg.help(t(
                    "Target session (name, list number, or id); defaults to the terminal session",
                    "目标会话(名字、编号或 id);缺省为终端集成会话",
                ))
            })
        })
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

pub(in crate::cli) fn localize_export_command(command: clap::Command) -> clap::Command {
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
            arg.help(t("Include chat-platform history", "包含通讯平台的聊天历史"))
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

pub(in crate::cli) fn localize_import_command(command: clap::Command) -> clap::Command {
    command
        .mut_arg("archive", |arg| {
            arg.help(t(
                "Archive produced by `miyu export`",
                "miyu export 生成的归档",
            ))
        })
        .mut_arg("force", |arg| {
            arg.help(t(
                "Overwrite existing data (the current installation is backed up first)",
                "覆盖已有数据（覆盖前会先备份当前安装）",
            ))
        })
}

pub(in crate::cli) fn localize_ask_command(command: clap::Command) -> clap::Command {
    localize_turn_options(command)
        .mut_arg("message", |arg| {
            arg.help(t("Message to send", "要发送的消息"))
        })
        .mut_arg("read_stdin", |arg| {
            arg.help(t(
                "Read the message body from stdin up to EOF (no probe timeout)",
                "从标准输入读正文(读到 EOF,不受探测超时限制)",
            ))
        })
}

/// 根命令与 `ask` 各 flatten 一份 `TurnOptions`,帮助文案共用这里。
/// `mut_arg` 的名字必须存在(否则 clap panic),所以这里只写 TurnOptions 的字段。
pub(in crate::cli) fn localize_turn_options(command: clap::Command) -> clap::Command {
    command
        .mut_arg("session", |arg| {
            arg.help(t(
                "Target session (name, list number, or id) for this command only",
                "仅本次命令使用的目标会话(名字、编号或 id)",
            ))
        })
        .mut_arg("continue_session", |arg| {
            arg.help(t(
                "Send the message into the terminal-integration session instead of a throwaway one-shot chat",
                "把消息发进终端集成会话,而不是用完即弃的一次性对话",
            ))
        })
        .mut_arg("create", |arg| {
            arg.help(t(
                "Create the --session if it does not exist (its name is the session name)",
                "--session 指名的会话不存在时新建(名字即会话名)",
            ))
        })
        .mut_arg("mode", |arg| {
            arg.help(t(
                "Mode for a newly created session (normal/dev); rejected for existing sessions",
                "新建会话的模式(normal/dev);对已有会话传了报错",
            ))
        })
        .mut_arg("model", |arg| {
            arg.help(t(
                "Model for this turn only (provider/model, bare name, or list-models index); not persisted",
                "本回合模型(provider/model、裸名或 list-models 序号);不落盘",
            ))
        })
        .mut_arg("context_window", |arg| {
            arg.help(t(
                "Context window in tokens for this turn only; not persisted",
                "本回合上下文窗口(token 数);不落盘",
            ))
        })
        .mut_arg("system_prompt", |arg| {
            arg.help(t(
                "Replace the system prompt (text or @file); every call is a cache cold start",
                "整体替换系统提示词(文本或 @文件);每次调用都是缓存冷启动",
            ))
        })
        .mut_arg("append_system_prompt", |arg| {
            arg.help(t(
                "Append host instructions after the persona prompt (text or @file); cache-stable when repeated",
                "在人格提示词后追加宿主指令(文本或 @文件);每回合同一段则缓存稳定",
            ))
        })
        .mut_arg("no_memory", |arg| {
            arg.help(t(
                "Do not write long-term memory, diary, or episodes for this turn",
                "本回合不写长期记忆、日记与经历",
            ))
        })
        .mut_arg("tools", |arg| {
            arg.help(t(
                "Tool allowlist, comma-separated",
                "工具白名单,逗号分隔",
            ))
        })
        .mut_arg("no_tools", |arg| {
            arg.help(t("Give this turn no tools at all", "本回合不给任何工具"))
        })
        .mut_arg("image", |arg| {
            arg.help(t("Attach an image (repeatable)", "附图,可多次"))
        })
        .mut_arg("cwd", |arg| {
            arg.help(t(
                "Workspace for this turn; defaults to the caller's current directory",
                "本回合工作区;缺省为调用方当前目录",
            ))
        })
        .mut_arg("output_format", |arg| {
            arg.help(t(
                "Output format: text (default), json (one final line), stream-json (one event per line)",
                "输出格式:text(默认)、json(一行终态)、stream-json(逐事件一行)",
            ))
        })
        .mut_arg("quiet", |arg| {
            arg.help(t(
                "Text mode: no progress or tool lines",
                "text 模式下不打进度与工具行",
            ))
        })
        .mut_arg("timeout", |arg| {
            arg.help(t(
                "Seconds before the turn is cancelled (exit 124); json/stream-json/stdio only",
                "超时秒数,到点取消回合(退出码 124);仅 json/stream-json/stdio",
            ))
        })
}

pub(in crate::cli) fn localize_session_command(command: clap::Command) -> clap::Command {
    let subs = [
        (
            "list",
            "List this persona's sessions (normal + dev); --json for raw output",
            "列出当前人格的会话(普通+开发模式);--json 直出",
        ),
        ("new", "Create a session", "新建会话"),
        (
            "show",
            "Session details (mode, workspace, turns, context usage)",
            "会话详情(模式、工作区、轮数、上下文占用)",
        ),
        ("delete", "Delete a session", "删除会话"),
        ("rename", "Rename a session", "重命名会话"),
        (
            "clear",
            "Clear a session's context (history and queue); the session stays",
            "清空会话上下文(历史与队列),会话本身保留",
        ),
        (
            "pop",
            "Move the oldest N turns out of the active context",
            "把最旧 N 轮移出活跃上下文",
        ),
        ("compact", "Compact a session's context", "压缩会话上下文"),
        (
            "models",
            "Show or set the session's model override (`default` follows the global pool)",
            "查看/设置会话模型覆盖(`default` 恢复跟随全局池)",
        ),
        (
            "workspace",
            "Show or bind the session workspace; --clear unbinds",
            "查看/绑定会话工作区;--clear 解绑",
        ),
    ];
    let mut command = command;
    for (name, en, zh) in subs {
        command = command.mut_subcommand(name, |sub| sub.about(t(en, zh)));
    }
    command
}

pub(in crate::cli) fn localize_models_command(command: clap::Command) -> clap::Command {
    command.mut_arg("target", |arg| {
        arg.help(t(
            "List index, provider/model, or 'default' to follow the global pool",
            "模型列表序号、供应商/模型名，或 default 恢复跟随全局模型池",
        ))
    })
}

pub(in crate::cli) fn localize_variant_command(command: clap::Command) -> clap::Command {
    command.mut_arg("name", |arg| {
        arg.help(t(
            "Thinking level to select; omit to choose interactively",
            "要选择的思考档位；省略则进入交互选择",
        ))
    })
}

pub(in crate::cli) fn localize_history_command(command: clap::Command) -> clap::Command {
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

pub(in crate::cli) fn localize_pop_command(command: clap::Command) -> clap::Command {
    command
        .mut_arg("count", |arg| {
            arg.help(t(
                "Number of oldest turns to pop; omit to select interactively",
                "要弹出的最旧轮次数；省略则进入交互多选",
            ))
        })
        .mut_arg("session", |arg| {
            arg.help(t(
                "Target session (name, list number, or id); defaults to the terminal session",
                "目标会话(名字、编号或 id);缺省为终端集成会话",
            ))
        })
}

pub(in crate::cli) fn localize_config_command(command: clap::Command) -> clap::Command {
    command
        .mut_subcommand("validate", |subcommand| {
            subcommand.about(t("Validate configuration", "校验配置"))
        })
        .mut_subcommand("paths", |subcommand| {
            subcommand.about(t("Show configuration paths", "显示配置路径"))
        })
}

pub(in crate::cli) fn localize_web_command(command: clap::Command) -> clap::Command {
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

pub(in crate::cli) fn localize_daemon_command(mut command: clap::Command) -> clap::Command {
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

pub(in crate::cli) fn localize_kb_command(mut command: clap::Command) -> clap::Command {
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

pub(in crate::cli) fn localize_memory_command(mut command: clap::Command) -> clap::Command {
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

pub(in crate::cli) fn localize_skills_command(mut command: clap::Command) -> clap::Command {
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
