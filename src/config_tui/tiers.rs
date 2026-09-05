//! 分级模型池：四个档位池的成员编辑，以及旁路请求（会话标题 / 日记整理 /
//! deep_research）指向哪一档。
//!
//! 一屏平铺：四档在上、旁路在下，每行 Enter 打开子菜单——档位行打开与文本池
//! 一样的多选框，旁路行打开单选（四档 + global）。`d` 只对旁路行生效：清掉显式
//! 值，回到代码内置的缺省档。通讯平台的池不在这里，它们各归各的平台菜单。
use crate::config::{AuxRole, ModelTier, GLOBAL_POOL_LABEL};
use crate::config_tui::*;

pub(in crate::config_tui) fn tier_display_name(tier: ModelTier) -> &'static str {
    tier.label()
}

/// 档位定位的一句话：只说「什么档」，不说工具——档位不影响工具集。
pub(in crate::config_tui) fn tier_hint(tier: ModelTier) -> &'static str {
    match tier {
        ModelTier::Lite => t("lite", "轻量"),
        ModelTier::Cheap => t("cheap", "便宜"),
        ModelTier::Standard => t("standard", "普通"),
        ModelTier::Flagship => t("flagship", "旗舰"),
    }
}

pub(in crate::config_tui) fn aux_role_label(role: AuxRole) -> &'static str {
    match role {
        AuxRole::SessionTitle => t("Session title", "会话标题"),
        AuxRole::MemoryOrganizer => t("Diary organizer", "日记整理"),
        AuxRole::DeepResearch => "deep_research",
    }
}

/// 池成员摘要；空池明说会回退到全局文本池。
pub(in crate::config_tui) fn tier_pool_summary(config: &AppConfig, tier: ModelTier) -> String {
    let pool = config.tier_choices(tier);
    if pool.is_empty() {
        t(
            "not configured, falls back to global pool",
            "未配置, 回退全局池",
        )
        .to_string()
    } else {
        pool.iter()
            .map(|choice| choice.model.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    }
}

/// 旁路行右列：档位名，档位空了追加回退说明；显式 global 就写 global。
pub(in crate::config_tui) fn aux_role_summary(config: &AppConfig, role: AuxRole) -> String {
    match config.model_tiers.role_tier(role) {
        None => GLOBAL_POOL_LABEL.to_string(),
        Some(tier) if config.tier_choices(tier).is_empty() => format!(
            "{} ({})",
            tier.label(),
            t(
                "not configured, falls back to global pool",
                "未配置, 回退全局池"
            )
        ),
        Some(tier) => tier.label().to_string(),
    }
}

const SEPARATOR_ROW: usize = ModelTier::ALL.len();

fn row_count() -> usize {
    ModelTier::ALL.len() + 1 + AuxRole::ALL.len()
}

fn step(selected: usize, delta: isize) -> usize {
    let last = row_count() - 1;
    let mut next = (selected as isize + delta).clamp(0, last as isize) as usize;
    if next == SEPARATOR_ROW {
        next = (next as isize + delta.signum()).clamp(0, last as isize) as usize;
    }
    next
}

pub(in crate::config_tui) fn select_model_tiers(
    stdout: &mut io::Stdout,
    config: &mut AppConfig,
) -> Result<()> {
    let mut selected = 0usize;
    loop {
        let name_width = ModelTier::ALL
            .iter()
            .map(|tier| tier.label().chars().count())
            .max()
            .unwrap_or(8);
        let mut options: Vec<String> = ModelTier::ALL
            .iter()
            .map(|tier| {
                format!(
                    "{:<width$} ({}): {}",
                    tier.label(),
                    tier_hint(*tier),
                    tier_pool_summary(config, *tier),
                    width = name_width
                )
            })
            .collect();
        options.push("─".repeat(44));
        let role_width = AuxRole::ALL
            .iter()
            .map(|role| aux_role_label(*role).chars().count())
            .max()
            .unwrap_or(8);
        options.extend(AuxRole::ALL.iter().map(|role| {
            format!(
                "{:<width$}: {}",
                aux_role_label(*role),
                aux_role_summary(config, *role),
                width = role_width
            )
        }));
        draw_menu(
            stdout,
            t(" TIERED MODEL POOLS ", " 分级模型池 "),
            &options,
            selected,
            t(
                "[Enter]open [d]reset to default [j/k]move [q]back",
                "[Enter]打开 [d]恢复缺省 [j/k]移动 [q]返回",
            ),
        )?;
        match read_key()? {
            KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
            KeyCode::Up | KeyCode::Char('k') => selected = step(selected, -1),
            KeyCode::Down | KeyCode::Char('j') => selected = step(selected, 1),
            KeyCode::Enter if selected < SEPARATOR_ROW => {
                select_tier_models(stdout, config, ModelTier::ALL[selected])?
            }
            KeyCode::Enter if selected > SEPARATOR_ROW => {
                let role = AuxRole::ALL[selected - SEPARATOR_ROW - 1];
                select_aux_role_tier(stdout, config, role)?
            }
            KeyCode::Char('d') if selected > SEPARATOR_ROW => {
                let role = AuxRole::ALL[selected - SEPARATOR_ROW - 1];
                config.model_tiers.reset_role(role);
            }
            _ => {}
        }
    }
}

/// Model multi-select for one tier pool, mirroring the text-model picker:
/// candidates are the configured text models, Tab toggles membership.
pub(in crate::config_tui) fn select_tier_models(
    stdout: &mut io::Stdout,
    config: &mut AppConfig,
    tier: ModelTier,
) -> Result<()> {
    let choices = config.text_provider_model_choices();
    if choices.is_empty() {
        message(
            stdout,
            t(
                "No text models are configured. Add models under Providers and models first.",
                "没有可用的文本模型，请先在供应商和模型里添加模型。",
            ),
        )?;
        return Ok(());
    }
    let mut selected = 0usize;
    let title = format!(" {} · {} ", t("TIER POOL", "档位池"), tier.label());
    loop {
        let options = choices
            .iter()
            .map(|choice| {
                let marker = if config.is_tier_model(tier, &choice.provider_id, &choice.model) {
                    "[*] "
                } else {
                    "[ ] "
                };
                format!("{marker}{}", choice.label())
            })
            .collect::<Vec<_>>();
        draw_menu(
            stdout,
            &title,
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
            KeyCode::Tab => {
                let choice = choices[selected].clone();
                config.toggle_tier_model(tier, &choice.provider_id, &choice.model)?;
            }
            _ => {}
        }
    }
}

/// 旁路请求的档位单选：四档 + global。选定即写入显式值（含 global）。
pub(in crate::config_tui) fn select_aux_role_tier(
    stdout: &mut io::Stdout,
    config: &mut AppConfig,
    role: AuxRole,
) -> Result<()> {
    let current = config.model_tiers.role_tier(role);
    let mut selected = match current {
        Some(tier) => ModelTier::ALL.iter().position(|t| *t == tier).unwrap_or(0),
        None => ModelTier::ALL.len(),
    };
    let title = format!(
        " {} · {} ",
        aux_role_label(role).to_uppercase(),
        t("SELECT TIER", "选择档位")
    );
    let mut options: Vec<String> = ModelTier::ALL
        .iter()
        .map(|tier| format!("{:<9} ({})", tier.label(), tier_hint(*tier)))
        .collect();
    options.push(format!(
        "{:<9} ({})",
        GLOBAL_POOL_LABEL,
        t("global text pool", "全局文本池")
    ));
    loop {
        draw_menu(
            stdout,
            &title,
            &options,
            selected,
            t(
                "[Enter]select [j/k]move [q]cancel",
                "[Enter]选定 [j/k]移动 [q]取消",
            ),
        )?;
        match read_key()? {
            KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
            KeyCode::Up | KeyCode::Char('k') => selected = selected.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => selected = (selected + 1).min(options.len() - 1),
            KeyCode::Enter => {
                let tier = ModelTier::ALL.get(selected).copied();
                config.model_tiers.set_role(role, tier);
                return Ok(());
            }
            _ => {}
        }
    }
}
