//! 平台（QQ 等）的接入配置。
//!
//! 这里的 ID 列表编辑（`parse_id_lines`、`prompt_single_id`）都做严格校验：这
//! 些值最终会决定谁能指挥 Miyu，填错一个数字就是把权限给了别人。
//!
//! 模型路由（`select_platform_model_routes`）让不同会话走不同的模型池，摘要函
//! 数（`*_summary`、`*_label`）只是把配置压成菜单里一行看得懂的字。

mod id_lists;
mod routes;
pub(in crate::config_tui) use id_lists::*;
pub(in crate::config_tui) use routes::*;

use crate::config_tui::*;

pub(in crate::config_tui) fn platforms_label(config: &AppConfig) -> String {
    if config.platforms.qq.enabled {
        t("Tencent QQ enabled", "腾讯 QQ 已启用").to_string()
    } else {
        t("disabled", "未启用").to_string()
    }
}

pub(in crate::config_tui) fn select_platforms(
    stdout: &mut io::Stdout,
    paths: &MiyuPaths,
    config: &mut AppConfig,
) -> Result<()> {
    let mut selected = 0usize;
    loop {
        let state = if config.platforms.qq.enabled {
            t("enabled", "已启用")
        } else {
            t("disabled", "未启用")
        };
        let max_rounds_label = if config.platforms.max_tool_rounds == 0 {
            t("unlimited", "不限").to_string()
        } else {
            config.platforms.max_tool_rounds.to_string()
        };
        let options = vec![
            format!("{}: {state}", t("Tencent QQ", "腾讯 QQ")),
            format!(
                "{}: {}",
                t("Command trigger prefix", "命令触发前缀"),
                config.platforms.command_prefix
            ),
            t("Command list", "命令列表").to_string(),
            format!(
                "{}: {max_rounds_label}",
                t("Max tool rounds per turn", "最大工具轮数")
            ),
            format!(
                "{}: {}",
                t(
                    "Allow the AI to message platforms from the terminal",
                    "允许 AI 从终端发消息到通讯平台"
                ),
                if config.platforms.terminal_outreach {
                    t("true", "true")
                } else {
                    t("false", "false")
                }
            ),
        ];
        draw_menu(
            stdout,
            t(" IM PLATFORMS ", " 接入通讯平台 "),
            &options,
            selected,
            t(
                "[Enter]configure [j/k]move [q]back",
                "[Enter]配置 [j/k]移动 [q]返回",
            ),
        )?;
        match read_key()? {
            KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
            KeyCode::Up | KeyCode::Char('k') => selected = selected.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => selected = (selected + 1).min(options.len() - 1),
            KeyCode::Enter => match selected {
                0 => edit_qq(stdout, paths, config)?,
                1 => edit_platform_command_prefix(stdout, config)?,
                2 => select_platform_commands(stdout, config)?,
                3 => edit_platform_max_tool_rounds(stdout, config)?,
                4 => config.platforms.terminal_outreach = !config.platforms.terminal_outreach,
                _ => {}
            },
            _ => {}
        }
    }
}

/// 平台回合的工具轮数上限(0=不限;默认 32)。web_search 同 query 222 连
/// 事故后的那道闸,可按需放宽或收紧。
pub(in crate::config_tui) fn edit_platform_max_tool_rounds(
    stdout: &mut io::Stdout,
    config: &mut AppConfig,
) -> Result<()> {
    if let Some(value) = edit_u16_value(
        stdout,
        t(" MAX TOOL ROUNDS PER TURN ", " 最大工具轮数(0=不限) "),
        u16::try_from(config.platforms.max_tool_rounds).unwrap_or(u16::MAX),
    )? {
        config.platforms.max_tool_rounds = usize::from(value);
    }
    Ok(())
}

pub(in crate::config_tui) fn edit_platform_command_prefix(
    stdout: &mut io::Stdout,
    config: &mut AppConfig,
) -> Result<()> {
    let Some(value) = edit_inline_value(
        stdout,
        t(" COMMAND TRIGGER PREFIX ", " 命令触发前缀 "),
        &config.platforms.command_prefix,
        false,
    )?
    else {
        return Ok(());
    };
    let value = value.trim();
    if value.is_empty()
        || value.chars().count() > MAX_PLATFORM_COMMAND_PREFIX_CHARS
        || value
            .chars()
            .any(|character| character.is_whitespace() || character.is_control())
    {
        message(
            stdout,
            t(
                "The prefix must be 1-32 characters and cannot contain whitespace.",
                "前缀必须为 1 到 32 个字符，且不能包含空白字符。",
            ),
        )?;
    } else {
        config.platforms.command_prefix = value.to_string();
    }
    Ok(())
}

pub(in crate::config_tui) fn platform_command_permission_label(
    permission: PlatformCommandPermission,
) -> &'static str {
    match permission {
        PlatformCommandPermission::Everyone => t("Everyone", "所有人"),
        PlatformCommandPermission::AdminOnly => t("Administrators only", "仅管理员"),
    }
}

pub(in crate::config_tui) fn select_platform_commands(
    stdout: &mut io::Stdout,
    config: &mut AppConfig,
) -> Result<()> {
    let mut selected = 0usize;
    loop {
        let options = commands::BUILTIN_COMMANDS
            .iter()
            .map(|command| {
                let permission = config
                    .platforms
                    .command_permission(command.id, command.default_permission);
                format!(
                    "{}: {}",
                    command.id,
                    platform_command_permission_label(permission)
                )
            })
            .collect::<Vec<_>>();
        draw_menu(
            stdout,
            t(" PLATFORM COMMANDS ", " 命令列表 "),
            &options,
            selected,
            t(
                "[Enter]set permission [j/k]move [q]back",
                "[Enter]设置权限 [j/k]移动 [q]返回",
            ),
        )?;
        match read_key()? {
            KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
            KeyCode::Up | KeyCode::Char('k') => selected = selected.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => selected = (selected + 1).min(options.len() - 1),
            KeyCode::Enter => {
                edit_platform_command_permission(
                    stdout,
                    config,
                    &commands::BUILTIN_COMMANDS[selected],
                )?;
            }
            _ => {}
        }
    }
}

pub(in crate::config_tui) fn edit_platform_command_permission(
    stdout: &mut io::Stdout,
    config: &mut AppConfig,
    command: &PlatformCommandDescriptor,
) -> Result<()> {
    let permissions = [
        PlatformCommandPermission::Everyone,
        PlatformCommandPermission::AdminOnly,
    ];
    let current = config
        .platforms
        .command_permission(command.id, command.default_permission);
    let mut selected = permissions
        .iter()
        .position(|permission| *permission == current)
        .unwrap_or(0);
    loop {
        let options = permissions
            .iter()
            .map(|permission| platform_command_permission_label(*permission).to_string())
            .collect::<Vec<_>>();
        let title = format!(" {} · {} ", t("COMMAND PERMISSION", "命令权限"), command.id);
        draw_menu(stdout, &title, &options, selected, "")?;
        match read_key()? {
            KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
            KeyCode::Up | KeyCode::Char('k') => selected = selected.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => {
                selected = (selected + 1).min(permissions.len() - 1)
            }
            KeyCode::Enter => {
                config.platforms.set_command_permission(
                    command.id,
                    permissions[selected],
                    command.default_permission,
                );
                return Ok(());
            }
            _ => {}
        }
    }
}

pub(in crate::config_tui) fn enabled_label(value: bool) -> &'static str {
    if value {
        t("enabled", "已启用")
    } else {
        t("disabled", "已禁用")
    }
}

pub(in crate::config_tui) fn edit_qq(
    stdout: &mut io::Stdout,
    paths: &MiyuPaths,
    config: &mut AppConfig,
) -> Result<()> {
    let mut selected = 0usize;
    loop {
        let qq = &config.platforms.qq;
        let parallel = qq.session_parallel;
        let mut options = vec![
            format!(
                "{}: {}",
                t("Enabled", "是否启用"),
                enabled_label(qq.enabled)
            ),
            format!(
                "{}: {}",
                t("Text model pool", "文本模型池"),
                qq_pool_summary(qq.text_models.as_deref())
            ),
            format!(
                "{}: {}",
                t("Multimodal model pool", "多模态模型池"),
                qq_pool_summary(qq.multimodal_models.as_deref())
            ),
            format!(
                "{}: {}",
                t("Reverse WebSocket port", "反向 WebSocket 端口"),
                qq.reverse_ws_port
            ),
            format!(
                "{}: {}",
                t("Reverse WebSocket token", "反向 WebSocket 验证 Token"),
                if qq.access_token.is_empty() {
                    t("empty", "未设置")
                } else {
                    "********"
                }
            ),
            format!(
                "{}: {}",
                t("User identification", "用户识别"),
                enabled_label(qq.user_identification)
            ),
            format!(
                "{}: {}",
                t("Show group name", "显示群名称"),
                enabled_label(qq.show_group_name)
            ),
            format!(
                "{}: {}",
                t("Write persona memory", "写入人格记忆"),
                enabled_label(qq.memory.write_enabled)
            ),
            format!(
                "{}: {}",
                t(
                    "Administrator QQ ids allowed to use the terminal",
                    "允许使用终端的管理员 QQ 号"
                ),
                qq.admin_users.len()
            ),
            format!(
                "{}: {}",
                t(
                    "Allow non-admin computer access",
                    "是否允许非管理员使用电脑"
                ),
                enabled_label(qq.allow_non_admin_host_tools)
            ),
            format!(
                "{}: {}",
                t(
                    "Send intermediate messages in group chats",
                    "群聊是否输出中间消息"
                ),
                enabled_label(qq.group_intermediate_messages)
            ),
            format!(
                "{}: {}",
                t(
                    "Send intermediate messages in private chats",
                    "私聊是否输出中间消息"
                ),
                enabled_label(qq.private_intermediate_messages)
            ),
            format!(
                "{}: {}",
                t("Private whitelist", "私聊白名单"),
                qq.private_chats.whitelist.len()
            ),
            format!(
                "{}: {}",
                t("Non-whitelist model pool", "非白名单模型池"),
                route_pool_summary(
                    qq.non_whitelist_text_models.as_deref(),
                    PlatformModelPoolInheritance::Platform,
                )
            ),
            format!(
                "{}: {}",
                t(
                    "Only private whitelist can add friends",
                    "仅私聊白名单能加好友"
                ),
                enabled_label(qq.private_chats.friend_requests_require_private_whitelist)
            ),
            format!(
                "{}: {}",
                t("Allow non-whitelist private chats", "是否允许非白名单私聊"),
                enabled_label(qq.private_chats.allow_non_whitelist)
            ),
            format!(
                "{}: {}",
                t("Non-whitelist private rate limit", "非白名单私聊限流"),
                rate_limit_label(qq.private_chats.non_whitelist_rate_limit)
            ),
            format!(
                "{}: {}",
                t("Group whitelist", "群聊白名单"),
                qq.group_chats.whitelist.len()
            ),
            format!(
                "{}: {}",
                t("Additional group wake keywords", "额外群聊触发关键词"),
                qq.group_chats.trigger_keywords.len()
            ),
            format!(
                "{}: {}",
                t("Whitelist-group rate limit", "白名单群聊限流"),
                rate_limit_label(qq.group_chats.whitelist_rate_limit)
            ),
            format!(
                "{}: {}",
                t("Allow non-whitelist groups", "是否允许非白名单群聊"),
                enabled_label(qq.group_chats.allow_non_whitelist)
            ),
            format!(
                "{}: {}",
                t("Non-whitelist-group rate limit", "非白名单群聊限流"),
                rate_limit_label(qq.group_chats.non_whitelist_rate_limit)
            ),
            format!(
                "{}: {}",
                t("In-conversation parallelism", "会话内并行"),
                enabled_label(qq.session_parallel)
            ),
        ];
        // 串行时并行数无处可用,整项不出现(08-26 用户裁定):串行下被挡住的
        // 消息由「多少秒多少条」限流决定丢弃,不归这里管。
        if qq.session_parallel {
            options.push(format!(
                "{}: {}",
                t("In-conversation parallel turns", "会话内并行数量"),
                session_limits_label(qq.session_limits)
            ));
        }
        options.extend([
            format!(
                "{}: {}",
                t("Private/group conversation settings", "私聊/群聊专属配置"),
                qq.conversations.len()
            ),
            t("QQ plugins", "QQ 插件配置").to_string(),
            t("Advanced settings", "高级设置").to_string(),
        ]);
        draw_menu(
            stdout,
            t(" TENCENT QQ ", " 腾讯 QQ "),
            &options,
            selected,
            "",
        )?;
        let key = read_key()?;
        match key {
            KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
            KeyCode::Up | KeyCode::Char('k') => selected = selected.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => selected = (selected + 1).min(options.len() - 1),
            KeyCode::Enter | KeyCode::Char(' ') => match selected {
                0 => config.platforms.qq.enabled = !config.platforms.qq.enabled,
                1 if matches!(key, KeyCode::Enter) => select_qq_model_pool(stdout, config, false)?,
                2 if matches!(key, KeyCode::Enter) => select_qq_model_pool(stdout, config, true)?,
                3 if matches!(key, KeyCode::Enter) => {
                    if let Some(value) = edit_u16_value(
                        stdout,
                        t("Reverse WebSocket port", "反向 WebSocket 端口"),
                        config.platforms.qq.reverse_ws_port,
                    )? {
                        if value == 0 {
                            message(
                                stdout,
                                t(
                                    "Port must be between 1 and 65535.",
                                    "端口必须在 1 到 65535 之间。",
                                ),
                            )?;
                        } else {
                            config.platforms.qq.reverse_ws_port = value;
                        }
                    }
                }
                4 if matches!(key, KeyCode::Enter) => edit_qq_token(stdout, config)?,
                5 => {
                    config.platforms.qq.user_identification =
                        !config.platforms.qq.user_identification
                }
                6 => config.platforms.qq.show_group_name = !config.platforms.qq.show_group_name,
                7 => {
                    config.platforms.qq.memory.write_enabled =
                        !config.platforms.qq.memory.write_enabled
                }
                8 if matches!(key, KeyCode::Enter) => edit_qq_admin_list(
                    stdout,
                    t(
                        " TERMINAL-ENABLED ADMINISTRATORS ",
                        " 允许使用终端的管理员 QQ 号 ",
                    ),
                    &mut config.platforms.qq.admin_users,
                    &mut config.platforms.qq.admin_aliases,
                )?,
                9 => {
                    config.platforms.qq.allow_non_admin_host_tools =
                        !config.platforms.qq.allow_non_admin_host_tools
                }
                10 => {
                    config.platforms.qq.group_intermediate_messages =
                        !config.platforms.qq.group_intermediate_messages
                }
                11 => {
                    config.platforms.qq.private_intermediate_messages =
                        !config.platforms.qq.private_intermediate_messages
                }
                12 if matches!(key, KeyCode::Enter) => edit_qq_id_list(
                    stdout,
                    t(" PRIVATE WHITELIST ", " 私聊白名单 "),
                    t("QQ id", "QQ 号"),
                    &mut config.platforms.qq.private_chats.whitelist,
                )?,
                13 if matches!(key, KeyCode::Enter) => {
                    select_non_whitelist_model_pool(stdout, config)?
                }
                14 => {
                    config
                        .platforms
                        .qq
                        .private_chats
                        .friend_requests_require_private_whitelist = !config
                        .platforms
                        .qq
                        .private_chats
                        .friend_requests_require_private_whitelist
                }
                15 => {
                    config.platforms.qq.private_chats.allow_non_whitelist =
                        !config.platforms.qq.private_chats.allow_non_whitelist
                }
                16 if matches!(key, KeyCode::Enter) => {
                    edit_platform_rate_limit(
                        stdout,
                        &mut config.platforms.qq.private_chats.non_whitelist_rate_limit,
                    )?;
                }
                17 if matches!(key, KeyCode::Enter) => edit_qq_id_list(
                    stdout,
                    t(" GROUP WHITELIST ", " 群聊白名单 "),
                    t("Group id", "群号"),
                    &mut config.platforms.qq.group_chats.whitelist,
                )?,
                18 if matches!(key, KeyCode::Enter) => edit_keyword_list(
                    stdout,
                    &mut config.platforms.qq.group_chats.trigger_keywords,
                )?,
                19 if matches!(key, KeyCode::Enter) => {
                    edit_platform_rate_limit(
                        stdout,
                        &mut config.platforms.qq.group_chats.whitelist_rate_limit,
                    )?;
                }
                20 => {
                    config.platforms.qq.group_chats.allow_non_whitelist =
                        !config.platforms.qq.group_chats.allow_non_whitelist
                }
                21 if matches!(key, KeyCode::Enter) => {
                    edit_platform_rate_limit(
                        stdout,
                        &mut config.platforms.qq.group_chats.non_whitelist_rate_limit,
                    )?;
                }
                // 光标就停在开关这一行(22),"并行数量"排在它之后——关掉并行
                // 时那一项消失也不会把光标落到不存在的行上,无需再钳制。
                22 => {
                    config.platforms.qq.session_parallel = !config.platforms.qq.session_parallel;
                }
                23 if parallel && matches!(key, KeyCode::Enter) => {
                    edit_platform_session_limits(stdout, &mut config.platforms.qq.session_limits)?
                }
                // 尾部三项随"并行数量"是否出现整体顺延一位。
                index if index == 24 - usize::from(!parallel) && matches!(key, KeyCode::Enter) => {
                    select_platform_model_routes(stdout, paths, config)?
                }
                index if index == 25 - usize::from(!parallel) && matches!(key, KeyCode::Enter) => {
                    select_platform_plugins(stdout, paths, config)?
                }
                index if index == 26 - usize::from(!parallel) && matches!(key, KeyCode::Enter) => {
                    edit_qq_advanced(stdout, config)?
                }
                _ => {}
            },
            _ => {}
        }
    }
}

pub(in crate::config_tui) fn session_limits_label(limits: PlatformSessionLimits) -> String {
    format!(
        "{} {} + {} {}",
        limits.running,
        t("running", "运行"),
        limits.queued,
        t("queued", "等待")
    )
}

pub(in crate::config_tui) fn edit_platform_session_limits(
    stdout: &mut io::Stdout,
    limits: &mut PlatformSessionLimits,
) -> Result<()> {
    let mut fields = vec![
        Field::new(
            t("Running turns", "并行运行数量"),
            limits.running.to_string(),
        ),
        Field::new(t("Queued turns", "等待队列数量"), limits.queued.to_string()),
    ];
    if !run_form_editing(
        stdout,
        t(" CONVERSATION CONCURRENCY ", " 会话并发 "),
        &mut fields,
    )? {
        return Ok(());
    }
    let running = fields[0].value.trim().parse::<usize>()?;
    let queued = fields[1].value.trim().parse::<usize>()?;
    if !(1..=MAX_PLATFORM_SESSION_RUNNING).contains(&running)
        || queued > MAX_PLATFORM_SESSION_QUEUED
    {
        message(
            stdout,
            t(
                "Concurrency values are outside the supported range.",
                "并发数值超出支持范围。",
            ),
        )?;
        return Ok(());
    }
    *limits = PlatformSessionLimits { running, queued };
    Ok(())
}

pub(in crate::config_tui) fn rate_limit_label(limit: PlatformRateLimit) -> String {
    if limit.max_messages == 0 {
        return t("unlimited", "不限").to_string();
    }
    format!(
        "{} / {} {}",
        limit.max_messages,
        limit.window_seconds,
        t("seconds", "秒")
    )
}

/// Both numbers live on one form, the way `edit_platform_session_limits`
/// already does it. The menu row above already renders "N / M 秒", so routing
/// Enter through a two-item submenu only restated that summary before letting
/// anyone type — two keypresses to reach a field that was never in doubt.
pub(in crate::config_tui) fn edit_platform_rate_limit(
    stdout: &mut io::Stdout,
    limit: &mut PlatformRateLimit,
) -> Result<()> {
    let mut fields = vec![
        Field::new(
            t(
                "Maximum messages (0 = unlimited)",
                "窗口内消息上限（0 = 不限）",
            ),
            limit.max_messages.to_string(),
        ),
        Field::new(
            t("Window seconds (1-86400)", "窗口秒数（1-86400）"),
            limit.window_seconds.to_string(),
        ),
    ];
    if !run_form_editing(stdout, t(" RATE LIMIT ", " 限流配置 "), &mut fields)? {
        return Ok(());
    }
    let (Ok(max_messages), Ok(window_seconds)) = (
        fields[0].value.trim().parse::<u32>(),
        fields[1].value.trim().parse::<u32>(),
    ) else {
        message(stdout, t("Invalid number.", "数值无效。"))?;
        return Ok(());
    };
    if !(1..=86_400).contains(&window_seconds) {
        message(
            stdout,
            t(
                "Window seconds must be between 1 and 86400.",
                "窗口秒数必须在 1 到 86400 之间。",
            ),
        )?;
        return Ok(());
    }
    *limit = PlatformRateLimit {
        max_messages,
        window_seconds,
    };
    Ok(())
}

pub(in crate::config_tui) fn edit_qq_token(
    stdout: &mut io::Stdout,
    config: &mut AppConfig,
) -> Result<()> {
    if let Some(value) = edit_inline_value(
        stdout,
        t(" REVERSE WEBSOCKET TOKEN ", " 反向 WebSocket 验证 Token "),
        &config.platforms.qq.access_token,
        true,
    )? {
        config.platforms.qq.access_token = value.trim().to_string();
    }
    Ok(())
}

pub(in crate::config_tui) fn edit_qq_advanced(
    stdout: &mut io::Stdout,
    config: &mut AppConfig,
) -> Result<()> {
    let qq = &config.platforms.qq;
    let mut fields = vec![
        Field::new(
            t(
                "Asset base URL (empty = automatic)",
                "文件访问基础 URL（空 = 自动推导）",
            ),
            qq.asset_base_url.clone(),
        ),
        Field::new(
            t(
                "Max reply chars per message (0 = no split)",
                "单条回复最大字数（0 = 不分段）",
            ),
            qq.max_reply_chars.to_string(),
        ),
        Field::new(
            t(
                "Group overflow (compact / pop)",
                "群聊上下文溢出策略（compact 摘要 / pop 丢弃最旧）",
            ),
            qq.group_context.on_overflow.clone(),
        ),
        Field::new(
            t(
                "Group trim batch (0-1, share released per trim)",
                "群聊单次丢弃比例（0-1，一次让出的窗口占比）",
            ),
            qq.group_context.trim_batch_ratio.to_string(),
        ),
    ];
    if run_form(stdout, t(" QQ ADVANCED ", " QQ 高级设置 "), &mut fields)? {
        config.platforms.qq.asset_base_url =
            fields[0].value.trim().trim_end_matches('/').to_string();
        let overflow = fields[2].value.trim().to_ascii_lowercase();
        if !matches!(overflow.as_str(), "compact" | "pop") {
            return Err(anyhow::anyhow!(t(
                "Group overflow must be compact or pop.",
                "群聊溢出策略只能是 compact 或 pop。"
            )));
        }
        let batch: f32 = fields[3].value.trim().parse().map_err(|_| {
            anyhow::anyhow!(t("Invalid group trim batch.", "群聊单次丢弃比例无效。"))
        })?;
        if !(0.0..1.0).contains(&batch) {
            return Err(anyhow::anyhow!(t(
                "Group trim batch must be between 0 and 1.",
                "群聊单次丢弃比例必须在 0 与 1 之间。"
            )));
        }
        config.platforms.qq.group_context.on_overflow = overflow;
        config.platforms.qq.group_context.trim_batch_ratio = batch;
        config.platforms.qq.max_reply_chars = fields[1].value.trim().parse().map_err(|_| {
            anyhow::anyhow!(t("Invalid maximum reply length.", "单条回复最大字数无效。"))
        })?;
    }
    Ok(())
}
