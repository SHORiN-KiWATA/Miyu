//! 真实感插件（real_context）的设置。
//!
//! 这个插件的可调项最多，因为它管的是「像不像人」——回复时机、克制程度、话题
//! 延续、好感度、身份认知。全部拆成独立小编辑器而不是一个巨型表单，是因为它们
//! 之间没有依赖，用户通常只想改其中一项。
//!
//! `real_context_value` / `real_context_bool` 是统一的读值口子：这些设置存在
//! 插件的自由 JSON 里，不是强类型字段。

mod affection;
mod emotion;
mod identity;
mod reply;
pub(in crate::config_tui) use affection::*;
pub(in crate::config_tui) use emotion::*;
pub(in crate::config_tui) use identity::*;
pub(in crate::config_tui) use reply::*;

use crate::config_tui::*;

pub(in crate::config_tui) fn real_context_values(
    config: &AppConfig,
) -> Result<(bool, RealContextPluginSettings)> {
    let Some(instance) = config.platforms.qq.plugins.get(REAL_CONTEXT_PLUGIN_ID) else {
        return Ok((true, RealContextPluginSettings::default()));
    };
    Ok((
        instance.enabled_or(true),
        RealContextPluginSettings::from_instance(instance)?,
    ))
}

pub(in crate::config_tui) fn apply_real_context_values(
    config: &mut AppConfig,
    enabled: bool,
    settings: &RealContextPluginSettings,
) {
    let instance = config
        .platforms
        .qq
        .plugins
        .entry(REAL_CONTEXT_PLUGIN_ID.to_string())
        .or_default();
    instance.enabled = (!enabled).then_some(false);
    merge_real_context_settings(instance, settings);
}

pub(in crate::config_tui) fn edit_real_context(
    stdout: &mut io::Stdout,
    paths: &MiyuPaths,
    config: &mut AppConfig,
) -> Result<()> {
    let (mut enabled, mut settings) = real_context_values(config)?;
    let mut selected = 0usize;
    loop {
        let state = if enabled {
            t("enabled", "已启用")
        } else {
            t("disabled", "未启用")
        };
        let options = vec![
            format!("{}: {state}", t("Plugin", "插件状态")),
            format!(
                "{}: {}",
                t("Text model pool", "文本模型池"),
                real_context_model_pool_summary(settings.text_models.as_deref())
            ),
            format!(
                "{}: {}",
                t("Reply context window", "回复上下文消息数"),
                settings.reply_context_window
            ),
            t("Group member information", "群成员信息查询").to_string(),
            t("Active reply judgement", "主动回复判断").to_string(),
            t("Quote, mention, and reactions", "引用艾特和贴表情").to_string(),
            t("Safety checks", "违规判断").to_string(),
            t("Affection and relationship", "好感度与关系").to_string(),
            t("Emotion state", "情绪状态").to_string(),
            t("Identity mappings", "识人映射").to_string(),
        ];
        draw_menu(
            stdout,
            t(" GROUP REAL CONTEXT ", " 群聊真实上下文回复 "),
            &options,
            selected,
            "",
        )?;
        match read_key()? {
            KeyCode::Char('q') | KeyCode::Esc => {
                settings.normalize();
                let mut candidate = config.clone();
                apply_real_context_values(&mut candidate, enabled, &settings);
                if let Err(error) = candidate.validate() {
                    message(stdout, &error.to_string())?;
                } else {
                    apply_real_context_values(config, enabled, &settings);
                    return Ok(());
                }
            }
            KeyCode::Up | KeyCode::Char('k') => selected = selected.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => selected = (selected + 1).min(options.len() - 1),
            KeyCode::Enter => match selected {
                0 => enabled = select_bool(stdout, t("Plugin", "插件状态"), enabled)?,
                1 => select_real_context_model_pool(stdout, config, &mut settings.text_models)?,
                2 => edit_real_context_number(
                    stdout,
                    t("Reply context window", "回复上下文消息数"),
                    settings.reply_context_window,
                    &mut settings,
                    |candidate, value| candidate.reply_context_window = value,
                )?,
                3 => edit_real_context_history(stdout, &mut settings)?,
                4 => match StateStore::new(paths) {
                    Ok(state) => edit_real_context_active_reply(stdout, &state, &mut settings)?,
                    Err(error) => message(
                        stdout,
                        &format!(
                            "{}: {error}",
                            t("Unable to open persistent state", "无法打开持久状态数据库")
                        ),
                    )?,
                },
                5 => edit_real_context_reply_target(stdout, &mut settings)?,
                6 => edit_real_context_moderation(stdout, &mut settings)?,
                7 => edit_real_context_affection(stdout, config, &mut settings)?,
                8 => edit_real_context_emotion(stdout, &mut settings)?,
                9 => edit_real_context_identities(stdout, &mut settings)?,
                _ => {}
            },
            _ => {}
        }
    }
}

pub(in crate::config_tui) fn edit_real_context_history(
    stdout: &mut io::Stdout,
    settings: &mut RealContextPluginSettings,
) -> Result<()> {
    loop {
        let mut fields = vec![Field::new(
            t(
                "Maximum group member search results",
                "群成员搜索工具最大返回数量",
            ),
            settings.group_member_search_max_results.to_string(),
        )];
        if !run_form(
            stdout,
            t(" GROUP MEMBER INFORMATION ", " 群成员信息查询 "),
            &mut fields,
        )? {
            return Ok(());
        }
        let mut candidate = settings.clone();
        let parsed = (|| -> std::result::Result<(), String> {
            candidate.group_member_search_max_results = real_context_value(&fields, 0)?;
            candidate.validate().map_err(|error| error.to_string())
        })();
        match parsed {
            Ok(()) => {
                *settings = candidate;
                return Ok(());
            }
            Err(error) => message(stdout, &error)?,
        }
    }
}

pub(in crate::config_tui) fn real_context_bool(
    fields: &[Field],
    index: usize,
) -> std::result::Result<bool, String> {
    parse_bool_field(&fields[index].value).map_err(|error| error.to_string())
}

pub(in crate::config_tui) fn real_context_value<T>(
    fields: &[Field],
    index: usize,
) -> std::result::Result<T, String>
where
    T: std::str::FromStr,
{
    fields[index]
        .value
        .trim()
        .parse()
        .map_err(|_| t("Invalid value.", "数值无效。").to_string())
}

pub(in crate::config_tui) fn edit_real_context_number<T>(
    stdout: &mut io::Stdout,
    label: &'static str,
    current: T,
    settings: &mut RealContextPluginSettings,
    assign: impl Fn(&mut RealContextPluginSettings, T),
) -> Result<()>
where
    T: Copy + ToString + std::str::FromStr,
{
    loop {
        let Some(raw) = edit_inline_value(stdout, label, &current.to_string(), false)? else {
            return Ok(());
        };
        let value = match raw.trim().parse() {
            Ok(value) => value,
            Err(_) => {
                message(stdout, t("Invalid value.", "数值无效。"))?;
                continue;
            }
        };
        let mut candidate = settings.clone();
        assign(&mut candidate, value);
        match candidate.validate() {
            Ok(()) => {
                *settings = candidate;
                return Ok(());
            }
            Err(error) => message(stdout, &error.to_string())?,
        }
    }
}

pub(in crate::config_tui) fn real_context_media_mode_label(value: &str) -> &'static str {
    match value {
        "off" => t("Off", "不记录"),
        "metadata" => t("Metadata", "保留元数据"),
        _ => t("Placeholder", "仅占位"),
    }
}

pub(in crate::config_tui) fn real_context_media_mode_value(value: &str) -> Option<&'static str> {
    match value.trim() {
        "off" | "Off" | "不记录" => Some("off"),
        "placeholder" | "Placeholder" | "仅占位" => Some("placeholder"),
        "metadata" | "Metadata" | "保留元数据" => Some("metadata"),
        _ => None,
    }
}

pub(in crate::config_tui) fn real_context_model_pool_summary(
    pool: Option<&[ActiveProviderModelConfig]>,
) -> String {
    match pool {
        None | Some([]) => t("inherit platform", "继承平台池").to_string(),
        Some(entries) => route_pool_summary(Some(entries), PlatformModelPoolInheritance::Platform),
    }
}

pub(in crate::config_tui) fn select_real_context_model_pool(
    stdout: &mut io::Stdout,
    config: &AppConfig,
    pool: &mut Option<Vec<ActiveProviderModelConfig>>,
) -> Result<()> {
    select_model_pool(
        stdout,
        config.text_provider_model_choices(),
        pool,
        false,
        t(" REAL-CONTEXT TEXT MODELS ", " 真实上下文文本模型 "),
        t("Inherit QQ platform model pool", "继承 QQ 平台模型池"),
    )
}
