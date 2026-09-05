//! 平台的模型路由与人格覆盖。
//!
//! 路由把「哪个会话」映射到「哪个模型池」。池的编辑要区分「继承上层」和「空
//! 池」——两者语义不同（见 `config::platform` 的 `PlatformModelPoolInheritance`），
//! 界面上也得让用户能表达出来。

use crate::config_tui::*;

pub(in crate::config_tui) fn select_platform_model_routes(
    stdout: &mut io::Stdout,
    paths: &MiyuPaths,
    config: &mut AppConfig,
) -> Result<()> {
    let mut selected = 0usize;
    loop {
        let mut options = Vec::with_capacity(config.platforms.qq.conversations.len() + 1);
        options.push(t("+ Add conversation", "+ 新增会话配置").to_string());
        options.extend(
            config
                .platforms
                .qq
                .conversations
                .iter()
                .map(platform_model_route_label),
        );
        selected = selected.min(options.len().saturating_sub(1));
        draw_menu(
            stdout,
            t(" QQ CONVERSATIONS ", " 私聊/群聊专属配置 "),
            &options,
            selected,
            t(
                "[Enter]add/edit [d]delete [j/k]move [q]back",
                "[Enter]新增/编辑 [d]删除 [j/k]移动 [q]返回",
            ),
        )?;
        match read_key()? {
            KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
            KeyCode::Up | KeyCode::Char('k') => selected = selected.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => {
                selected = (selected + 1).min(options.len().saturating_sub(1));
            }
            KeyCode::Enter if selected == 0 => {
                edit_platform_model_route(stdout, paths, config, None)?
            }
            KeyCode::Enter => edit_platform_model_route(stdout, paths, config, Some(selected - 1))?,
            KeyCode::Char('d') | KeyCode::Delete if selected > 0 => {
                config.platforms.qq.conversations.remove(selected - 1);
                selected = selected.min(config.platforms.qq.conversations.len());
            }
            _ => {}
        }
    }
}

pub(in crate::config_tui) fn platform_model_route_label(route: &PlatformModelRoute) -> String {
    let kind = match route.conversation.kind {
        PlatformConversationKind::Private => t("private", "私聊"),
        PlatformConversationKind::Group => t("group", "群聊"),
    };
    let text = route_pool_summary(route.text_models.as_deref(), route.text_models_inheritance);
    let multimodal = route_pool_summary(
        route.multimodal_models.as_deref(),
        route.multimodal_models_inheritance,
    );
    let prompt = if route.extra_prompt.is_empty() {
        t("none", "无")
    } else {
        t("set", "已设置")
    };
    let persona = platform_persona_summary(&route.persona);
    format!(
        "{kind} {} · {}:{persona} · {}:{text} {}:{multimodal} · {}:{prompt}",
        route.conversation.id,
        t("persona", "人格"),
        t("text", "文本"),
        t("media", "多模态"),
        t("prompt", "提示词")
    )
}

pub(in crate::config_tui) fn edit_platform_model_route(
    stdout: &mut io::Stdout,
    paths: &MiyuPaths,
    config: &mut AppConfig,
    route_index: Option<usize>,
) -> Result<()> {
    let mut route = route_index
        .and_then(|index| config.platforms.qq.conversations.get(index).cloned())
        .unwrap_or_else(|| PlatformModelRoute {
            conversation: PlatformConversationConfig {
                kind: PlatformConversationKind::Private,
                id: String::new(),
            },
            persona: PlatformPersonaOverride::Inherit,
            text_models_inheritance: PlatformModelPoolInheritance::Platform,
            text_models: None,
            multimodal_models_inheritance: PlatformModelPoolInheritance::Platform,
            multimodal_models: None,
            extra_prompt: String::new(),
            session_limits: None,
        });
    let mut selected = 0usize;
    loop {
        let kind_label = platform_conversation_kind_label(route.conversation.kind);
        let id_label = platform_conversation_id_label(route.conversation.kind);
        let options = [
            format!("{}: {}", t("Conversation type", "会话类型"), kind_label,),
            format!(
                "{id_label}: {}",
                if route.conversation.id.is_empty() {
                    t("not set", "未设置")
                } else {
                    route.conversation.id.as_str()
                },
            ),
            format!(
                "{}: {}",
                t("Override AI persona", "覆盖 AI 人格"),
                platform_persona_summary(&route.persona)
            ),
            format!(
                "{}: {}",
                t("Text model pool", "文本模型池"),
                route_pool_summary(route.text_models.as_deref(), route.text_models_inheritance)
            ),
            format!(
                "{}: {}",
                t("Multimodal model pool", "多模态模型池"),
                route_pool_summary(
                    route.multimodal_models.as_deref(),
                    route.multimodal_models_inheritance,
                )
            ),
            format!(
                "{}: {}",
                t("Extra prompt", "额外提示词"),
                if route.extra_prompt.is_empty() {
                    t("none", "未设置")
                } else {
                    t("set", "已设置")
                }
            ),
            format!(
                "{}: {}",
                t("Override concurrency settings", "覆盖并发配置"),
                route
                    .session_limits
                    .map(session_limits_label)
                    .unwrap_or_else(|| t("inherit", "继承").to_string())
            ),
        ];
        draw_menu(
            stdout,
            t(" EDIT QQ CONVERSATION ", " 编辑 QQ 会话配置 "),
            &options,
            selected,
            "",
        )?;
        match read_key()? {
            KeyCode::Char('q') | KeyCode::Esc => {
                route.normalize();
                if let Err(error) = config.validate_platform_model_route(&route) {
                    if route_index.is_none() {
                        return Ok(());
                    }
                    message(stdout, &error.to_string())?;
                    continue;
                }
                if config
                    .platforms
                    .qq
                    .conversations
                    .iter()
                    .enumerate()
                    .any(|(index, existing)| {
                        Some(index) != route_index && existing.identity() == route.identity()
                    })
                {
                    message(
                        stdout,
                        t(
                            "A configuration for this QQ conversation already exists.",
                            "该 QQ 会话的配置已存在。",
                        ),
                    )?;
                    continue;
                }
                match route_index {
                    Some(index) => config.platforms.qq.conversations[index] = route,
                    None => config.platforms.upsert_model_route(route),
                }
                return Ok(());
            }
            KeyCode::Up | KeyCode::Char('k') => selected = selected.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => selected = (selected + 1).min(options.len() - 1),
            KeyCode::Enter => match selected {
                0 => select_platform_conversation_kind(stdout, &mut route.conversation.kind)?,
                1 => {
                    let title = format!(" {id_label} ");
                    if let Some(value) =
                        edit_inline_value(stdout, &title, &route.conversation.id, false)?
                    {
                        route.conversation.id = value.trim().to_string();
                    }
                }
                2 => edit_platform_personas(stdout, paths, config, &mut route.persona)?,
                3 => select_platform_route_models(
                    stdout,
                    config,
                    &mut route.text_models,
                    &mut route.text_models_inheritance,
                    false,
                )?,
                4 => select_platform_route_models(
                    stdout,
                    config,
                    &mut route.multimodal_models,
                    &mut route.multimodal_models_inheritance,
                    true,
                )?,
                5 => edit_conversation_extra_prompt(stdout, &mut route.extra_prompt)?,
                6 => {
                    let enabled = select_bool(
                        stdout,
                        t("Override QQ concurrency", "覆盖 QQ 并发配置"),
                        route.session_limits.is_some(),
                    )?;
                    if enabled {
                        let limits = route
                            .session_limits
                            .get_or_insert(config.platforms.qq.session_limits);
                        edit_platform_session_limits(stdout, limits)?;
                    } else {
                        route.session_limits = None;
                    }
                }
                _ => {}
            },
            _ => {}
        }
    }
}

pub(in crate::config_tui) fn platform_conversation_kind_label(
    kind: PlatformConversationKind,
) -> &'static str {
    match kind {
        PlatformConversationKind::Private => t("Private chat", "私聊"),
        PlatformConversationKind::Group => t("Group chat", "群聊"),
    }
}

pub(in crate::config_tui) fn platform_conversation_id_label(
    kind: PlatformConversationKind,
) -> &'static str {
    match kind {
        PlatformConversationKind::Private => t("QQ id", "QQ 号"),
        PlatformConversationKind::Group => t("Group id", "群号"),
    }
}

pub(in crate::config_tui) fn platform_persona_summary(persona: &PlatformPersonaOverride) -> String {
    match persona {
        PlatformPersonaOverride::Inherit => {
            t("inherit current persona", "继承当前人格").to_string()
        }
        PlatformPersonaOverride::Miyu => "Miyu".to_string(),
        PlatformPersonaOverride::Custom { name } => persona_display_name(name).to_string(),
    }
}

pub(in crate::config_tui) fn edit_platform_personas(
    stdout: &mut io::Stdout,
    paths: &MiyuPaths,
    config: &mut AppConfig,
    persona: &mut PlatformPersonaOverride,
) -> Result<()> {
    if let Some(updated) = manage_personas(
        stdout,
        paths,
        config,
        PersonaMenuTarget::Platform(persona.clone()),
    )? {
        *persona = updated;
    }
    Ok(())
}

pub(in crate::config_tui) fn select_platform_conversation_kind(
    stdout: &mut io::Stdout,
    kind: &mut PlatformConversationKind,
) -> Result<()> {
    let choices = [
        platform_conversation_kind_label(PlatformConversationKind::Private).to_string(),
        platform_conversation_kind_label(PlatformConversationKind::Group).to_string(),
    ];
    let current = platform_conversation_kind_label(*kind);
    let selected = select_choice(
        stdout,
        t("Conversation type", "会话类型"),
        current,
        &choices,
        "",
        false,
    )?;
    *kind = if selected == choices[1] {
        PlatformConversationKind::Group
    } else {
        PlatformConversationKind::Private
    };
    Ok(())
}

pub(in crate::config_tui) fn edit_conversation_extra_prompt(
    stdout: &mut io::Stdout,
    prompt: &mut String,
) -> Result<()> {
    edit_textarea(stdout, prompt)?;
    Ok(())
}

pub(in crate::config_tui) fn route_pool_summary(
    pool: Option<&[ActiveProviderModelConfig]>,
    inheritance: PlatformModelPoolInheritance,
) -> String {
    match pool {
        None | Some([]) if inheritance == PlatformModelPoolInheritance::Global => {
            t("inherit global", "继承全局池").to_string()
        }
        None | Some([]) => t("inherit platform", "继承 QQ 平台池").to_string(),
        Some(entries) if entries.len() == 1 => {
            format!("{} / {}", entries[0].provider_id, entries[0].model)
        }
        Some(entries) => format!("{} {}", entries.len(), t("models", "个模型")),
    }
}

pub(in crate::config_tui) fn select_platform_route_models(
    stdout: &mut io::Stdout,
    config: &AppConfig,
    pool: &mut Option<Vec<ActiveProviderModelConfig>>,
    inheritance: &mut PlatformModelPoolInheritance,
    multimodal: bool,
) -> Result<()> {
    let choices = if multimodal {
        config.multimodal_provider_model_choices()
    } else {
        config.text_provider_model_choices()
    };
    let mut selected = 0usize;
    loop {
        let mut options = Vec::with_capacity(choices.len() + 2);
        let inherit_platform_marker = if pool.as_ref().is_none_or(Vec::is_empty)
            && *inheritance == PlatformModelPoolInheritance::Platform
        {
            "[*] "
        } else {
            "[ ] "
        };
        options.push(format!(
            "{inherit_platform_marker}{}",
            t("Inherit QQ platform model pool", "继承 QQ 平台模型池")
        ));
        let inherit_global_marker = if pool.as_ref().is_none_or(Vec::is_empty)
            && *inheritance == PlatformModelPoolInheritance::Global
        {
            "[*] "
        } else {
            "[ ] "
        };
        options.push(format!(
            "{inherit_global_marker}{}",
            if multimodal {
                t(
                    "Inherit global multimodal model pool",
                    "继承全局多模态模型池",
                )
            } else {
                t("Inherit global model pool", "继承全局模型池")
            }
        ));
        options.extend(choices.iter().map(|choice| {
            let active = pool.as_ref().is_some_and(|entries| {
                entries.iter().any(|entry| {
                    entry.provider_id == choice.provider_id && entry.model == choice.model
                })
            });
            let marker = if active { "[*] " } else { "[ ] " };
            format!("{marker}{}", choice.label())
        }));
        draw_menu(
            stdout,
            if multimodal {
                t(" SESSION MULTIMODAL MODELS ", " 会话多模态模型 ")
            } else {
                t(" SESSION TEXT MODELS ", " 会话文本模型 ")
            },
            &options,
            selected,
            t(
                "[Tab]add/remove [Enter/q]confirm",
                "[Tab]加入/移出 [Enter/q]确认",
            ),
        )?;
        match read_key()? {
            KeyCode::Char('q') | KeyCode::Esc | KeyCode::Enter => return Ok(()),
            KeyCode::Up | KeyCode::Char('k') => selected = selected.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => selected = (selected + 1).min(options.len() - 1),
            KeyCode::Tab if selected == 0 => {
                *pool = None;
                *inheritance = PlatformModelPoolInheritance::Platform;
            }
            KeyCode::Tab if selected == 1 => {
                *pool = None;
                *inheritance = PlatformModelPoolInheritance::Global;
            }
            KeyCode::Tab => {
                *inheritance = PlatformModelPoolInheritance::Platform;
                let choice = &choices[selected - 2];
                let entries = pool.get_or_insert_with(Vec::new);
                if let Some(index) = entries.iter().position(|entry| {
                    entry.provider_id == choice.provider_id && entry.model == choice.model
                }) {
                    entries.remove(index);
                } else {
                    entries.push(ActiveProviderModelConfig {
                        provider_id: choice.provider_id.clone(),
                        model: choice.model.clone(),
                    });
                }
                if entries.is_empty() {
                    *pool = None;
                }
            }
            _ => {}
        }
    }
}
