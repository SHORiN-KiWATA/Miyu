//! 几个独立插件的设置界面。
//!
//! 回复处理器、入群审核、消息历史、表情收集——共性是都有自己的一套 JSON 设置，
//! 所以都遵循同一个三段式：`*_values` 读出来、表单编辑、`apply_*_values` 写回。
//!
//! `validate_reply_processor_settings` 单独存在是因为它的字段互相约束（比如某
//! 个模式下另一项必填），表单本身校验不了。

use crate::config::{ModelPoolRef, ModelTier};
use crate::config_tui::*;

pub(in crate::config_tui) const REPLY_PROCESSOR_PLUGIN_ID: &str = "reply_processor";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub(in crate::config_tui) struct ReplyProcessorSettingsForm {
    pub(in crate::config_tui) default_enabled: bool,
    pub(in crate::config_tui) threshold: usize,
    pub(in crate::config_tui) mode: String,
    pub(in crate::config_tui) followup_mention: bool,
    pub(in crate::config_tui) strip_period: bool,
    pub(in crate::config_tui) theme: String,
    pub(in crate::config_tui) max_height: u32,
    pub(in crate::config_tui) font_size: u32,
    pub(in crate::config_tui) code_font_size: u32,
    pub(in crate::config_tui) padding: u32,
    pub(in crate::config_tui) context_notice: bool,
    pub(in crate::config_tui) ttl_hours: u64,
    pub(in crate::config_tui) max_records: usize,
    pub(in crate::config_tui) send_tool_intercept: bool,
    pub(in crate::config_tui) font: String,
    pub(in crate::config_tui) title_font: String,
    pub(in crate::config_tui) code_font: String,
    pub(in crate::config_tui) emoji_font: String,
}

impl Default for ReplyProcessorSettingsForm {
    fn default() -> Self {
        Self {
            default_enabled: true,
            threshold: 200,
            mode: "image".to_string(),
            followup_mention: true,
            strip_period: true,
            theme: "paper".to_string(),
            max_height: 2600,
            font_size: 36,
            code_font_size: 30,
            padding: 64,
            context_notice: true,
            ttl_hours: 24,
            max_records: 3,
            send_tool_intercept: true,
            font: String::new(),
            title_font: String::new(),
            code_font: String::new(),
            emoji_font: String::new(),
        }
    }
}

pub(in crate::config_tui) fn select_platform_plugins(
    stdout: &mut io::Stdout,
    paths: &MiyuPaths,
    config: &mut AppConfig,
) -> Result<()> {
    let mut selected = 0usize;
    loop {
        let reply_enabled = config
            .platforms
            .qq
            .plugins
            .get(REPLY_PROCESSOR_PLUGIN_ID)
            .map(|plugin| plugin.enabled_or(true))
            .unwrap_or(true);
        let reply_state = if reply_enabled {
            t("enabled", "已启用")
        } else {
            t("disabled", "未启用")
        };
        let real_context_enabled = config
            .platforms
            .qq
            .plugins
            .get(REAL_CONTEXT_PLUGIN_ID)
            .map(|plugin| plugin.enabled_or(true))
            .unwrap_or(true);
        let real_context_state = if real_context_enabled {
            t("enabled", "已启用")
        } else {
            t("disabled", "未启用")
        };
        let message_history_enabled = config
            .platforms
            .qq
            .plugins
            .get(QQ_MESSAGE_HISTORY_PLUGIN_ID)
            .map(|plugin| plugin.enabled_or(true))
            .unwrap_or(true);
        let message_history_state = if message_history_enabled {
            t("enabled", "已启用")
        } else {
            t("disabled", "未启用")
        };
        let meme_collector_enabled = config
            .platforms
            .qq
            .plugins
            .get(QQ_MEME_COLLECTOR_PLUGIN_ID)
            .map(|plugin| plugin.enabled_or(true))
            .unwrap_or(true);
        let meme_collector_state = if meme_collector_enabled {
            t("enabled", "已启用")
        } else {
            t("disabled", "未启用")
        };
        let group_join_approval_enabled = config
            .platforms
            .qq
            .plugins
            .get(QQ_GROUP_JOIN_APPROVAL_PLUGIN_ID)
            .map(|plugin| plugin.enabled_or(true))
            .unwrap_or(true);
        let group_join_approval_state = if group_join_approval_enabled {
            t("enabled", "已启用")
        } else {
            t("disabled", "未启用")
        };
        let scheduled_messages_enabled = config
            .platforms
            .qq
            .plugins
            .get(crate::config::QQ_SCHEDULED_MESSAGES_PLUGIN_ID)
            .map(|plugin| plugin.enabled_or(false))
            .unwrap_or(false);
        let scheduled_messages_state = if scheduled_messages_enabled {
            t("enabled", "已启用")
        } else {
            t("disabled", "未启用")
        };
        let options = [
            format!("{}: {reply_state}", t("Reply processor", "回复处理")),
            format!(
                "{}: {real_context_state}",
                t("Group real-context replies", "群聊真实上下文回复")
            ),
            format!(
                "{}: {message_history_state}",
                t("QQ text message history", "QQ 纯文字消息历史")
            ),
            format!(
                "{}: {meme_collector_state}",
                t("QQ meme pocket", "QQ 表情口袋")
            ),
            format!(
                "{}: {group_join_approval_state}",
                t("Group join approval", "入群审批")
            ),
            format!(
                "{}: {scheduled_messages_state}",
                t("Scheduled messages", "定时消息")
            ),
        ];
        draw_menu(
            stdout,
            t(" TENCENT QQ PLUGINS ", " QQ 插件配置 "),
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
                0 => edit_reply_processor(stdout, config)?,
                1 => edit_real_context(stdout, paths, config)?,
                2 => edit_message_history(stdout, config)?,
                3 => edit_meme_collector(stdout, config)?,
                4 => edit_group_join_approval(stdout, config)?,
                5 => edit_scheduled_messages(stdout, config)?,
                _ => {}
            },
            _ => {}
        }
    }
}

pub(in crate::config_tui) fn group_join_approval_values(
    config: &AppConfig,
) -> Result<(bool, QqGroupJoinApprovalPluginSettings)> {
    let Some(instance) = config
        .platforms
        .qq
        .plugins
        .get(QQ_GROUP_JOIN_APPROVAL_PLUGIN_ID)
    else {
        return Ok((true, QqGroupJoinApprovalPluginSettings::default()));
    };
    Ok((
        instance.enabled_or(true),
        QqGroupJoinApprovalPluginSettings::from_instance(instance)?,
    ))
}

pub(in crate::config_tui) fn apply_group_join_approval_values(
    config: &mut AppConfig,
    enabled: bool,
    settings: &QqGroupJoinApprovalPluginSettings,
) {
    let instance = config
        .platforms
        .qq
        .plugins
        .entry(QQ_GROUP_JOIN_APPROVAL_PLUGIN_ID.to_string())
        .or_default();
    instance.enabled = (!enabled).then_some(false);
    merge_group_join_approval_settings(instance, settings);
}

pub(in crate::config_tui) fn group_join_approval_group_label(
    group: &QqGroupJoinApprovalGroupConfig,
) -> String {
    format!(
        "{} · {}",
        group.group_id,
        if group.approve_condition.is_empty() {
            t("not set", "未设置")
        } else {
            t("set", "已设置")
        }
    )
}

pub(in crate::config_tui) fn edit_group_join_approval_groups(
    stdout: &mut io::Stdout,
    settings: &mut QqGroupJoinApprovalPluginSettings,
) -> Result<()> {
    let mut selected = 0usize;
    loop {
        let mut options = vec![t("+ Add one", "+ 新增一项").to_string()];
        options.extend(settings.groups.iter().map(group_join_approval_group_label));
        selected = selected.min(options.len().saturating_sub(1));
        draw_menu(
            stdout,
            t(" GROUP JOIN APPROVAL CONDITIONS ", " 分群审批条件 "),
            &options,
            selected,
            t(
                "[Enter]configure [Delete]remove [j/k]move [q]back",
                "[Enter]配置 [Delete]删除 [j/k]移动 [q]返回",
            ),
        )?;
        match read_key()? {
            KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
            KeyCode::Up | KeyCode::Char('k') => selected = selected.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => {
                selected = (selected + 1).min(options.len().saturating_sub(1))
            }
            KeyCode::Enter if selected == 0 => {
                if let Some(group) = prompt_group_join_approval_group(stdout, None)? {
                    upsert_group_join_approval_group(&mut settings.groups, group);
                    settings.normalize();
                }
            }
            KeyCode::Enter => {
                let index = selected - 1;
                if let Some(group) =
                    prompt_group_join_approval_group(stdout, settings.groups.get(index).cloned())?
                {
                    upsert_group_join_approval_group(&mut settings.groups, group);
                    settings.normalize();
                }
            }
            KeyCode::Delete | KeyCode::Backspace if selected >= 1 => {
                settings.groups.remove(selected - 1);
                selected = selected.min(settings.groups.len());
            }
            _ => {}
        }
    }
}

pub(in crate::config_tui) fn prompt_group_join_approval_group(
    stdout: &mut io::Stdout,
    current: Option<QqGroupJoinApprovalGroupConfig>,
) -> Result<Option<QqGroupJoinApprovalGroupConfig>> {
    let current = current.unwrap_or(QqGroupJoinApprovalGroupConfig {
        group_id: 0,
        approve_condition: String::new(),
    });
    let mut fields = vec![
        Field::new(
            t("Group id", "群号"),
            if current.group_id > 0 {
                current.group_id.to_string()
            } else {
                String::new()
            },
        ),
        Field::textarea(
            t("Approval condition", "通过条件"),
            current.approve_condition.clone(),
        ),
    ];
    if !run_form_editing(
        stdout,
        t(" GROUP JOIN APPROVAL CONDITION ", " 编辑入群审批条件 "),
        &mut fields,
    )? {
        return Ok(None);
    }
    let group_id = match parse_positive_id(&fields[0].value) {
        Ok(id) => id,
        Err(error) => {
            message(stdout, &error)?;
            return Ok(None);
        }
    };
    let approve_condition = fields[1].value.trim().to_string();
    if approve_condition.is_empty() {
        message(
            stdout,
            t(
                "The approval condition cannot be empty.",
                "通过条件不能为空。",
            ),
        )?;
        return Ok(None);
    }
    Ok(Some(QqGroupJoinApprovalGroupConfig {
        group_id,
        approve_condition,
    }))
}

pub(in crate::config_tui) fn upsert_group_join_approval_group(
    groups: &mut Vec<QqGroupJoinApprovalGroupConfig>,
    group: QqGroupJoinApprovalGroupConfig,
) {
    if let Some(existing) = groups
        .iter_mut()
        .find(|existing| existing.group_id == group.group_id)
    {
        *existing = group;
    } else {
        groups.push(group);
    }
}

pub(in crate::config_tui) fn edit_group_join_approval(
    stdout: &mut io::Stdout,
    config: &mut AppConfig,
) -> Result<()> {
    let (mut enabled, mut settings) = group_join_approval_values(config)?;
    let mut selected = 0usize;
    let mut editing: Option<(usize, String, usize)> = None;
    loop {
        let state = if enabled {
            t("enabled", "已启用")
        } else {
            t("disabled", "未启用")
        };
        let labels = [
            t("Plugin", "插件状态"),
            t("Approval timeout seconds", "审批超时秒数"),
            t("Parse retry count", "解析失败重试次数"),
            t("Text model pool", "文本模型池"),
            t("Group approval conditions", "分群审批条件"),
        ];
        let options = vec![
            format!("{}: {state}", labels[0]),
            format!("{}: {}", labels[1], settings.timeout_seconds),
            format!("{}: {}", labels[2], settings.max_retries),
            format!(
                "{}: {}",
                labels[3],
                pool_ref_summary(config, &settings.text_models, t("default text", "默认文本"))
            ),
            format!(
                "{}: {} {}",
                labels[4],
                settings.groups.len(),
                t("groups", "个群")
            ),
        ];
        draw_menu_with_editing(
            stdout,
            t(" GROUP JOIN APPROVAL ", " 入群审批 "),
            &options,
            selected,
            "",
            editing
                .as_ref()
                .map(|(index, value, cursor)| (*index, labels[*index], value.as_str(), *cursor)),
        )?;
        let key = read_key()?;
        if let Some((_, value, cursor)) = editing.as_mut() {
            match key {
                KeyCode::Esc => editing = None,
                KeyCode::Enter => {
                    let (index, value, _) = editing.take().unwrap();
                    let value = value.trim().to_string();
                    match index {
                        1 => match value.parse::<u64>() {
                            Ok(parsed) if (1..=3_600).contains(&parsed) => {
                                settings.timeout_seconds = parsed;
                            }
                            _ => message(
                                stdout,
                                t(
                                    "Timeout must be between 1 and 3600 seconds.",
                                    "超时秒数必须在 1 到 3600 之间。",
                                ),
                            )?,
                        },
                        2 => match value.parse::<usize>() {
                            Ok(parsed) if parsed <= 3 => settings.max_retries = parsed,
                            _ => message(
                                stdout,
                                t(
                                    "Retry count must be between 0 and 3.",
                                    "重试次数必须在 0 到 3 之间。",
                                ),
                            )?,
                        },
                        _ => {}
                    }
                }
                KeyCode::Left => *cursor = cursor.saturating_sub(1),
                KeyCode::Right => *cursor = (*cursor + 1).min(value.chars().count()),
                KeyCode::Home => *cursor = 0,
                KeyCode::End => *cursor = value.chars().count(),
                KeyCode::Backspace => {
                    if *cursor > 0 {
                        remove_char_before_cursor(value, cursor);
                    }
                }
                KeyCode::Delete => remove_char_at_cursor(value, *cursor),
                KeyCode::Char(character) => insert_char_at_cursor(value, cursor, character),
                _ => {}
            }
            continue;
        }
        match key {
            KeyCode::Char('q') | KeyCode::Esc => {
                settings.normalize();
                let mut candidate = config.clone();
                apply_group_join_approval_values(&mut candidate, enabled, &settings);
                candidate.normalize_platform_model_routes();
                if let Err(error) = candidate.validate() {
                    message(stdout, &error.to_string())?;
                    continue;
                }
                apply_group_join_approval_values(config, enabled, &settings);
                config.normalize_platform_model_routes();
                return Ok(());
            }
            KeyCode::Up | KeyCode::Char('k') => selected = selected.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => selected = (selected + 1).min(options.len() - 1),
            KeyCode::Enter => match selected {
                0 => enabled = select_bool(stdout, t("Plugin", "插件状态"), enabled)?,
                1 => {
                    let value = settings.timeout_seconds.to_string();
                    let cursor = value.chars().count();
                    editing = Some((1, value, cursor));
                }
                2 => {
                    let value = settings.max_retries.to_string();
                    let cursor = value.chars().count();
                    editing = Some((2, value, cursor));
                }
                3 => select_plugin_pool_ref(
                    stdout,
                    config,
                    t("Group join approval", "入群审批"),
                    t("default text", "默认文本"),
                    ModelPoolRef::tier(ModelTier::Lite),
                    &mut settings.text_models,
                )?,
                4 => edit_group_join_approval_groups(stdout, &mut settings)?,
                _ => {}
            },
            _ => {}
        }
    }
}

pub(in crate::config_tui) fn edit_message_history(
    stdout: &mut io::Stdout,
    config: &mut AppConfig,
) -> Result<()> {
    let instance = config
        .platforms
        .qq
        .plugins
        .get(QQ_MESSAGE_HISTORY_PLUGIN_ID);
    let enabled = instance.map(|value| value.enabled_or(true)).unwrap_or(true);
    let settings = instance
        .map(QqMessageHistoryPluginSettings::from_instance)
        .transpose()?
        .unwrap_or_default();
    let mut fields = vec![
        Field::boolean(t("Plugin", "插件状态"), enabled),
        Field::new(
            t(
                "Maximum query results (0 = safety limit)",
                "查询工具单次最多返回（0=安全页上限）",
            ),
            settings.history_search_max_results.to_string(),
        ),
        Field::new(
            t("Query safety page limit", "查询安全页上限"),
            settings.history_safe_page_limit.to_string(),
        ),
        Field::boolean(
            t(
                "Allow administrators to access other conversations",
                "允许管理员访问其他会话",
            ),
            settings.allow_cross_conversation_search,
        ),
    ];
    if !run_form(
        stdout,
        t(" QQ TEXT MESSAGE HISTORY ", " QQ 纯文字消息历史 "),
        &mut fields,
    )? {
        return Ok(());
    }
    let enabled = fields[0].value.parse::<bool>()?;
    let settings = QqMessageHistoryPluginSettings {
        history_search_max_results: fields[1].value.trim().parse()?,
        history_safe_page_limit: fields[2].value.trim().parse()?,
        allow_cross_conversation_search: fields[3].value.parse()?,
    };
    settings.validate()?;
    let mut candidate = config.clone();
    let instance = candidate
        .platforms
        .qq
        .plugins
        .entry(QQ_MESSAGE_HISTORY_PLUGIN_ID.to_string())
        .or_default();
    instance.enabled = (!enabled).then_some(false);
    instance.settings.insert(
        "history_search_max_results".to_string(),
        serde_json::json!(settings.history_search_max_results),
    );
    instance.settings.insert(
        "history_safe_page_limit".to_string(),
        serde_json::json!(settings.history_safe_page_limit),
    );
    instance.settings.insert(
        "allow_cross_conversation_search".to_string(),
        serde_json::json!(settings.allow_cross_conversation_search),
    );
    candidate.normalize_platform_model_routes();
    candidate.validate()?;
    *config = candidate;
    Ok(())
}

pub(in crate::config_tui) fn edit_meme_collector(
    stdout: &mut io::Stdout,
    config: &mut AppConfig,
) -> Result<()> {
    let instance = config.platforms.qq.plugins.get(QQ_MEME_COLLECTOR_PLUGIN_ID);
    let enabled = instance.map(|value| value.enabled_or(true)).unwrap_or(true);
    let settings = instance
        .map(QqMemeCollectorPluginSettings::from_instance)
        .transpose()?
        .unwrap_or_default();
    let mut fields = vec![
        Field::boolean(t("Plugin", "插件状态"), enabled),
        Field::new(
            t("Collection probability (0..1)", "收图概率（0..1）"),
            settings.collect_probability.to_string(),
        ),
        Field::new(
            t("Maximum images per message", "每条消息最多图片数"),
            settings.max_images_per_message.to_string(),
        ),
        Field::boolean(
            t(
                "Allow non-admin save meme tool",
                "允许非管理员使用存表情工具",
            ),
            settings.allow_non_admin_save_tool,
        ),
    ];
    if !run_form(stdout, t(" QQ MEME POCKET ", " QQ 表情口袋 "), &mut fields)? {
        return Ok(());
    }
    let enabled = fields[0].value.parse::<bool>()?;
    let collect_probability = fields[1].value.trim().parse::<f64>()?;
    let max_images_per_message = fields[2].value.trim().parse::<usize>()?;
    let allow_non_admin_save_tool = fields[3].value.parse::<bool>()?;
    let mut candidate = config.clone();
    let instance = candidate
        .platforms
        .qq
        .plugins
        .entry(QQ_MEME_COLLECTOR_PLUGIN_ID.to_string())
        .or_default();
    instance.enabled = (!enabled).then_some(false);
    instance.settings.insert(
        "collect_probability".to_string(),
        serde_json::json!(collect_probability),
    );
    instance.settings.insert(
        "max_images_per_message".to_string(),
        serde_json::json!(max_images_per_message),
    );
    instance.settings.insert(
        "allow_non_admin_save_tool".to_string(),
        serde_json::json!(allow_non_admin_save_tool),
    );
    if let Err(error) = candidate.validate() {
        message(stdout, &error.to_string())?;
        return Ok(());
    }
    *config = candidate;
    Ok(())
}

pub(in crate::config_tui) fn reply_processor_values(
    config: &AppConfig,
) -> Result<(bool, ReplyProcessorSettingsForm)> {
    let Some(instance) = config.platforms.qq.plugins.get(REPLY_PROCESSOR_PLUGIN_ID) else {
        return Ok((true, ReplyProcessorSettingsForm::default()));
    };
    let settings = serde_json::from_value(serde_json::Value::Object(instance.settings.clone()))?;
    Ok((instance.enabled_or(true), settings))
}

pub(in crate::config_tui) fn apply_reply_processor_values(
    config: &mut AppConfig,
    enabled: bool,
    settings: &ReplyProcessorSettingsForm,
) -> Result<()> {
    let serialized = serde_json::to_value(settings)?;
    let serde_json::Value::Object(known_settings) = serialized else {
        bail!("reply processor settings must serialize as an object");
    };
    let instance = config
        .platforms
        .qq
        .plugins
        .entry(REPLY_PROCESSOR_PLUGIN_ID.to_string())
        .or_default();
    instance.enabled = (!enabled).then_some(false);
    for (key, value) in known_settings {
        instance.settings.insert(key, value);
    }
    Ok(())
}

pub(in crate::config_tui) fn edit_reply_processor(
    stdout: &mut io::Stdout,
    config: &mut AppConfig,
) -> Result<()> {
    let (mut plugin_enabled, mut settings) = reply_processor_values(config)?;
    loop {
        let mode_choices = vec![
            reply_processor_mode_label("image"),
            reply_processor_mode_label("forward"),
        ];
        let mut fields = vec![
            Field::boolean(t("Plugin enabled", "启用插件"), plugin_enabled),
            Field::boolean(
                t("Enabled for new conversations", "新会话默认启用"),
                settings.default_enabled,
            ),
            Field::new(
                t("Long reply threshold (characters)", "长回复阈值（字符）"),
                settings.threshold.to_string(),
            ),
            Field::new(
                t("Long reply processing mode", "长回复处理模式"),
                reply_processor_mode_label(&settings.mode),
            )
            .choices_owned(mode_choices)
            .raw_choice_labels(),
            Field::boolean(
                t("Mention sender after forwarding", "转发后艾特发起者"),
                settings.followup_mention,
            ),
            Field::boolean(
                t("Strip trailing Chinese period", "移除末尾中文句号"),
                settings.strip_period,
            ),
            Field::new(t("Image theme", "长图主题"), settings.theme.clone())
                .choices(&["paper", "light", "dark"]),
            Field::new(
                t("Image maximum height", "长图最大高度"),
                settings.max_height.to_string(),
            ),
            Field::new(
                t("Body font size", "正文字号"),
                settings.font_size.to_string(),
            ),
            Field::new(
                t("Code font size", "代码字号"),
                settings.code_font_size.to_string(),
            ),
            Field::new(t("Image padding", "长图边距"), settings.padding.to_string()),
            Field::boolean(
                t("Add image context notice", "注入长图上下文提示"),
                settings.context_notice,
            ),
            Field::new(
                t("Context notice TTL (hours)", "上下文提示保留小时"),
                settings.ttl_hours.to_string(),
            ),
            Field::new(
                t("Maximum context records", "上下文提示最大条数"),
                settings.max_records.to_string(),
            ),
            Field::boolean(
                t("Intercept send-message tool", "接管发送消息工具"),
                settings.send_tool_intercept,
            ),
            Field::new(
                t(
                    "Body font file path (empty = bundled default)",
                    "正文字体文件路径（空 = 内置默认字体）",
                ),
                settings.font.clone(),
            ),
            Field::new(
                t(
                    "Title font file path (empty = body font)",
                    "标题字体文件路径（空 = 跟随正文字体）",
                ),
                settings.title_font.clone(),
            ),
            Field::new(
                t(
                    "Code font file path (empty = bundled default)",
                    "代码字体文件路径（空 = 内置默认字体）",
                ),
                settings.code_font.clone(),
            ),
            Field::new(
                t(
                    "Emoji font file path (empty = bundled default)",
                    "Emoji 字体文件路径（空 = 内置默认字体）",
                ),
                settings.emoji_font.clone(),
            ),
        ];
        run_form_without_buttons(stdout, t(" REPLY PROCESSOR ", " 回复处理 "), &mut fields)?;
        plugin_enabled = parse_bool_field(&fields[0].value)?;
        settings = match parse_reply_processor_fields(&fields) {
            Ok(settings) => settings,
            Err(error) => {
                message(stdout, &error)?;
                continue;
            }
        };
        apply_reply_processor_values(config, plugin_enabled, &settings)?;
        return Ok(());
    }
}

pub(in crate::config_tui) fn parse_reply_processor_fields(
    fields: &[Field],
) -> std::result::Result<ReplyProcessorSettingsForm, String> {
    let bool_at =
        |index: usize| parse_bool_field(&fields[index].value).map_err(|error| error.to_string());
    let mode = reply_processor_mode_value(&fields[3].value)
        .map(str::to_string)
        .unwrap_or_else(|| fields[3].value.trim().to_string());
    let settings = ReplyProcessorSettingsForm {
        default_enabled: bool_at(1)?,
        threshold: parse_reply_processor_value(fields, 2, t("threshold", "阈值"))?,
        mode,
        followup_mention: bool_at(4)?,
        strip_period: bool_at(5)?,
        theme: fields[6].value.trim().to_string(),
        max_height: parse_reply_processor_value(fields, 7, t("maximum height", "最大高度"))?,
        font_size: parse_reply_processor_value(fields, 8, t("font size", "字号"))?,
        code_font_size: parse_reply_processor_value(fields, 9, t("code font size", "代码字号"))?,
        padding: parse_reply_processor_value(fields, 10, t("padding", "边距"))?,
        context_notice: bool_at(11)?,
        ttl_hours: parse_reply_processor_value(fields, 12, "TTL")?,
        max_records: parse_reply_processor_value(fields, 13, t("maximum records", "最大条数"))?,
        send_tool_intercept: bool_at(14)?,
        font: fields[15].value.trim().to_string(),
        title_font: fields[16].value.trim().to_string(),
        code_font: fields[17].value.trim().to_string(),
        emoji_font: fields[18].value.trim().to_string(),
    };
    validate_reply_processor_settings(&settings)?;
    Ok(settings)
}

pub(in crate::config_tui) fn reply_processor_mode_label(value: &str) -> String {
    match value.trim() {
        "image" => t("Convert to image", "转图片"),
        "forward" => t("Merged forward", "合并转发"),
        value => value,
    }
    .to_string()
}

pub(in crate::config_tui) fn reply_processor_mode_value(value: &str) -> Option<&'static str> {
    match value.trim() {
        "image" | "Convert to image" | "转图片" => Some("image"),
        "forward" | "Merged forward" | "合并转发" => Some("forward"),
        _ => None,
    }
}

pub(in crate::config_tui) fn parse_reply_processor_value<T>(
    fields: &[Field],
    index: usize,
    label: &str,
) -> std::result::Result<T, String>
where
    T: std::str::FromStr,
{
    fields[index]
        .value
        .trim()
        .parse()
        .map_err(|_| format!("{}: {label}", t("Invalid value", "无效值")))
}

pub(in crate::config_tui) fn validate_reply_processor_settings(
    settings: &ReplyProcessorSettingsForm,
) -> std::result::Result<(), String> {
    if settings.threshold == 0 || settings.threshold > 100_000 {
        return Err(t(
            "Threshold must be between 1 and 100000.",
            "阈值必须在 1 到 100000 之间。",
        )
        .to_string());
    }
    if !matches!(settings.mode.as_str(), "image" | "forward") {
        return Err(t(
            "Mode must be Convert to image or Merged forward.",
            "模式必须是转图片或合并转发。",
        )
        .to_string());
    }
    if !matches!(settings.theme.as_str(), "paper" | "light" | "dark") {
        return Err(t(
            "Theme must be paper, light, or dark.",
            "主题必须是 paper、light 或 dark。",
        )
        .to_string());
    }
    if !(1000..=5000).contains(&settings.max_height) {
        return Err(t(
            "Image maximum height must be between 1000 and 5000.",
            "长图最大高度必须在 1000 到 5000 之间。",
        )
        .to_string());
    }
    if !(24..=56).contains(&settings.font_size) || !(20..=46).contains(&settings.code_font_size) {
        return Err(t(
            "Body font size must be 24-56 and code font size must be 20-46.",
            "正文字号必须为 24-56，代码字号必须为 20-46。",
        )
        .to_string());
    }
    if !(36..=120).contains(&settings.padding) {
        return Err(t(
            "Image padding must be between 36 and 120.",
            "长图边距必须在 36 到 120 之间。",
        )
        .to_string());
    }
    if !(1..=168).contains(&settings.ttl_hours) || !(1..=10).contains(&settings.max_records) {
        return Err(t(
            "Context TTL must be 1-168 hours and maximum records must be 1-10.",
            "上下文保留时间必须为 1-168 小时，最大条数必须为 1-10。",
        )
        .to_string());
    }
    Ok(())
}
