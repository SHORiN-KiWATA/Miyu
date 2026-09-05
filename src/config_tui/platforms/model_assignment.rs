//! QQ · 模型分配：这条线上所有会用模型的地方收在一屏，每行指向一个池。
//!
//! 行由「槽位」声明驱动：平台自己的三个池，加上各插件声明的槽位（回复判定、
//! 好感度、入群审批）。这一屏只收集声明、按声明读写，不认识任何插件的内部
//! 结构；插件未启用时它的槽位不出现。会话专属配置不在这里，它按会话一条条
//! 配，留在原来的位置。
//!
//! 选择框是「池单选 + 模型多选」：上半区 继承 / 全局池 / 四档，下半区
//! 文本或多模态模型；两区互斥，勾一个模型就是「指定模型」，勾几个就是这一处
//! 专属的小池子。`d` 把一行清回它声明的缺省值。
//!
//! 界面上不出现 `inherit` / `global` / `lite` 这些配置 id，只按 locale 显示一个
//! 名字；「继承」一律写成「继承 xx 池」，说清继承到哪一层。
use crate::config::{ActiveProviderModelConfig, ModelPoolRef, ModelTier, ProviderModelChoice};
use crate::config_tui::*;

/// One model-consuming slot on a platform.
pub(in crate::config_tui) struct PoolSlot {
    pub(in crate::config_tui) label: &'static str,
    /// How the `inherit` value reads on this slot, e.g. "继承平台池": names the
    /// layer it resolves to, for the row summary and the picker.
    pub(in crate::config_tui) inherit_label: &'static str,
    /// Whether the parent *is* the global pool: then `global` duplicates
    /// `inherit` and the picker hides it.
    pub(in crate::config_tui) parent_is_global: bool,
    pub(in crate::config_tui) multimodal: bool,
    pub(in crate::config_tui) default: ModelPoolRef,
    pub(in crate::config_tui) visible: fn(&AppConfig) -> bool,
    pub(in crate::config_tui) get: fn(&AppConfig) -> ModelPoolRef,
    pub(in crate::config_tui) set: fn(&mut AppConfig, ModelPoolRef),
}

fn always(_: &AppConfig) -> bool {
    true
}

fn real_context_enabled(config: &AppConfig) -> bool {
    matches!(real_context_values(config), Ok((true, _)))
}

fn group_join_enabled(config: &AppConfig) -> bool {
    matches!(group_join_approval_values(config), Ok((true, _)))
}

fn judge_get(config: &AppConfig) -> ModelPoolRef {
    real_context_values(config)
        .map(|(_, settings)| settings.text_models)
        .unwrap_or_default()
}

fn judge_set(config: &mut AppConfig, value: ModelPoolRef) {
    if let Ok((enabled, mut settings)) = real_context_values(config) {
        settings.text_models = value;
        apply_real_context_values(config, enabled, &settings);
    }
}

fn affection_get(config: &AppConfig) -> ModelPoolRef {
    real_context_values(config)
        .map(|(_, settings)| settings.affection_text_models)
        .unwrap_or_default()
}

fn affection_set(config: &mut AppConfig, value: ModelPoolRef) {
    if let Ok((enabled, mut settings)) = real_context_values(config) {
        settings.affection_text_models = value;
        apply_real_context_values(config, enabled, &settings);
    }
}

fn group_join_get(config: &AppConfig) -> ModelPoolRef {
    group_join_approval_values(config)
        .map(|(_, settings)| settings.text_models)
        .unwrap_or_default()
}

fn group_join_set(config: &mut AppConfig, value: ModelPoolRef) {
    if let Ok((enabled, mut settings)) = group_join_approval_values(config) {
        settings.text_models = value;
        apply_group_join_approval_values(config, enabled, &settings);
    }
}

/// QQ 的槽位声明，按显示顺序。
pub(in crate::config_tui) fn qq_pool_slots() -> Vec<PoolSlot> {
    vec![
        PoolSlot {
            label: t("Platform text pool", "平台文本池"),
            inherit_label: inherits_global_pool_label(),
            parent_is_global: true,
            multimodal: false,
            default: ModelPoolRef::inherit(),
            visible: always,
            get: |config| config.platforms.qq.text_models.clone(),
            set: |config, value| config.platforms.qq.text_models = value,
        },
        PoolSlot {
            label: t("Platform multimodal pool", "平台多模态池"),
            inherit_label: inherits_global_pool_label(),
            parent_is_global: true,
            multimodal: true,
            default: ModelPoolRef::inherit(),
            visible: always,
            get: |config| config.platforms.qq.multimodal_models.clone(),
            set: |config, value| config.platforms.qq.multimodal_models = value,
        },
        PoolSlot {
            label: t("Non-whitelist text", "非白名单文本"),
            inherit_label: t("inherits platform pool", "继承平台池"),
            parent_is_global: false,
            multimodal: false,
            default: ModelPoolRef::inherit(),
            visible: always,
            get: |config| config.platforms.qq.non_whitelist_text_models.clone(),
            set: |config, value| config.platforms.qq.non_whitelist_text_models = value,
        },
        PoolSlot {
            label: t("Reply judge", "回复判定"),
            inherit_label: t("inherits conversation pool", "继承会话池"),
            parent_is_global: false,
            multimodal: false,
            default: ModelPoolRef::tier(ModelTier::Lite),
            visible: real_context_enabled,
            get: judge_get,
            set: judge_set,
        },
        PoolSlot {
            label: t("Affection", "好感度"),
            inherit_label: t("inherits reply judge", "继承回复判定"),
            parent_is_global: false,
            multimodal: false,
            default: ModelPoolRef::inherit(),
            visible: real_context_enabled,
            get: affection_get,
            set: affection_set,
        },
        PoolSlot {
            label: t("Group join approval", "入群审批"),
            inherit_label: t("inherits platform pool", "继承平台池"),
            parent_is_global: false,
            multimodal: false,
            default: ModelPoolRef::tier(ModelTier::Lite),
            visible: group_join_enabled,
            get: group_join_get,
            set: group_join_set,
        },
    ]
}

fn visible_slots(config: &AppConfig) -> Vec<PoolSlot> {
    qq_pool_slots()
        .into_iter()
        .filter(|slot| (slot.visible)(config))
        .collect()
}

/// QQ 表单里那一行的右侧摘要。
pub(in crate::config_tui) fn qq_model_assignment_label(config: &AppConfig) -> String {
    format!("{} {}", visible_slots(config).len(), t("items", "项"))
}

/// 一处引用的摘要：`继承平台池`、`全局池`、`便宜`、`deepseek-chat`、`3 个模型`。
/// 档位池空了会在运行时回退全局池，这里不重复说——分级模型池那一屏已经写着。
pub(in crate::config_tui) fn pool_ref_summary(
    _config: &AppConfig,
    pool: &ModelPoolRef,
    inherit_label: &str,
) -> String {
    if pool.is_inherit() {
        return inherit_label.to_string();
    }
    if pool.is_global() {
        return global_pool_label().to_string();
    }
    if let Some(tier) = pool.tier_ref() {
        return tier_hint(tier).to_string();
    }
    match pool.explicit_models() {
        Some([single]) => single.model.clone(),
        Some(entries) => format!("{} {}", entries.len(), t("models", "个模型")),
        None => inherit_label.to_string(),
    }
}

pub(in crate::config_tui) fn select_qq_model_assignment(
    stdout: &mut io::Stdout,
    config: &mut AppConfig,
) -> Result<()> {
    let mut selected = 0usize;
    loop {
        let slots = visible_slots(config);
        let width = slots
            .iter()
            .map(|slot| display_width(slot.label))
            .max()
            .unwrap_or(8);
        let options: Vec<String> = slots
            .iter()
            .map(|slot| {
                format!(
                    "{}: {}",
                    pad(slot.label, width),
                    pool_ref_summary(config, &(slot.get)(config), slot.inherit_label),
                )
            })
            .collect();
        draw_menu(
            stdout,
            t(" QQ · CONFIGURE MODELS ", " QQ · 配置模型 "),
            &options,
            selected,
            t(
                "[Enter]open [d]reset to default [j/k]move [q]back",
                "[Enter]打开 [d]恢复缺省 [j/k]移动 [q]返回",
            ),
        )?;
        match read_key()? {
            KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
            KeyCode::Up | KeyCode::Char('k') => selected = selected.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => {
                selected = (selected + 1).min(options.len().saturating_sub(1))
            }
            KeyCode::Enter => {
                if let Some(slot) = slots.get(selected) {
                    let mut value = (slot.get)(config);
                    select_pool_ref(stdout, config, slot, &mut value)?;
                    (slot.set)(config, value);
                }
            }
            KeyCode::Char('d') => {
                if let Some(slot) = slots.get(selected) {
                    (slot.set)(config, slot.default.clone());
                }
            }
            _ => {}
        }
    }
}

enum PickRow {
    Named(ModelPoolRef, String),
    Separator,
    Model(ProviderModelChoice),
}

/// 池单选 + 模型多选。上半区 Tab 选定一个池并清空模型区；下半区 Tab 勾选模型
/// 并清掉池区的圆点。Enter / q 确认。
pub(in crate::config_tui) fn select_pool_ref(
    stdout: &mut io::Stdout,
    config: &AppConfig,
    slot: &PoolSlot,
    value: &mut ModelPoolRef,
) -> Result<()> {
    let choices = if slot.multimodal {
        config.multimodal_provider_model_choices()
    } else {
        config.text_provider_model_choices()
    };
    let mut rows = vec![PickRow::Named(
        ModelPoolRef::inherit(),
        slot.inherit_label.to_string(),
    )];
    if !slot.parent_is_global {
        rows.push(PickRow::Named(
            ModelPoolRef::global(),
            if slot.multimodal {
                t("global multimodal pool", "全局多模态池")
            } else {
                t("global text pool", "全局文本池")
            }
            .to_string(),
        ));
    }
    if !slot.multimodal {
        for tier in ModelTier::ALL {
            rows.push(PickRow::Named(
                ModelPoolRef::tier(tier),
                tier_hint(tier).to_string(),
            ));
        }
    }
    rows.push(PickRow::Separator);
    rows.extend(choices.into_iter().map(PickRow::Model));
    let title = format!(
        " {} · {} ",
        slot.label.to_uppercase(),
        t("SELECT MODELS", "选择模型")
    );
    let mut selected = 0usize;
    loop {
        let options: Vec<String> = rows
            .iter()
            .map(|row| match row {
                PickRow::Named(candidate, label) => {
                    let marker = if candidate == value { "(•) " } else { "( ) " };
                    format!("{marker}{label}")
                }
                PickRow::Separator => format!("── {} ──", t("Models", "模型")),
                PickRow::Model(choice) => {
                    let checked = value.explicit_models().is_some_and(|entries| {
                        entries.iter().any(|entry| {
                            entry.provider_id == choice.provider_id && entry.model == choice.model
                        })
                    });
                    format!(
                        "{}{}",
                        if checked { "[*] " } else { "[ ] " },
                        choice.label()
                    )
                }
            })
            .collect();
        draw_menu(
            stdout,
            &title,
            &options,
            selected,
            t(
                "[Tab]select/add [j/k]move [Enter/q]confirm",
                "[Tab]选定/加入 [j/k]移动 [Enter/q]确认",
            ),
        )?;
        match read_key()? {
            KeyCode::Char('q') | KeyCode::Esc | KeyCode::Enter => return Ok(()),
            KeyCode::Up | KeyCode::Char('k') => {
                selected = selected.saturating_sub(1);
                if matches!(rows[selected], PickRow::Separator) {
                    selected = selected.saturating_sub(1);
                }
            }
            KeyCode::Down | KeyCode::Char('j') => {
                selected = (selected + 1).min(rows.len() - 1);
                if matches!(rows[selected], PickRow::Separator) {
                    selected = (selected + 1).min(rows.len() - 1);
                }
            }
            KeyCode::Tab => match &rows[selected] {
                PickRow::Named(candidate, _) => *value = candidate.clone(),
                PickRow::Separator => {}
                PickRow::Model(choice) => {
                    let mut entries = value
                        .explicit_models()
                        .map(<[_]>::to_vec)
                        .unwrap_or_default();
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
                    *value = if entries.is_empty() {
                        ModelPoolRef::inherit()
                    } else {
                        ModelPoolRef::models(entries)
                    };
                }
            },
            _ => {}
        }
    }
}

/// 插件页里保留的第二入口：同一个选择框，作用在插件自己的设置值上。
pub(in crate::config_tui) fn select_plugin_pool_ref(
    stdout: &mut io::Stdout,
    config: &AppConfig,
    label: &'static str,
    inherit_label: &'static str,
    default: ModelPoolRef,
    value: &mut ModelPoolRef,
) -> Result<()> {
    let slot = PoolSlot {
        label,
        inherit_label,
        parent_is_global: false,
        multimodal: false,
        default,
        visible: always,
        get: |_| ModelPoolRef::inherit(),
        set: |_, _| {},
    };
    select_pool_ref(stdout, config, &slot, value)
}
