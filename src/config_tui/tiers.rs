//! 分级模型池：四个档位池的成员编辑，以及旁路请求（会话标题 / 日记整理 /
//! 深度研究）指向哪一档。
//!
//! 一屏平铺：四档在上、旁路在下，每行 Enter 打开子菜单——档位行打开与文本池
//! 一样的多选框，旁路行打开单选（四档 + 全局池）。`d` 只对旁路行生效：清掉显式
//! 值，回到代码内置的缺省档。通讯平台的池不在这里，它们各归各的平台菜单。
//!
//! 界面上档位只按 locale 显示一个名字（中文「轻量」/ 英文 `lite`），配置文件里
//! 存的仍是 `ModelTier::label()` 那套英文 id。
use crate::config::{AuxRole, ModelTier};
use crate::config_tui::*;

pub(in crate::config_tui) fn tier_display_name(tier: ModelTier) -> &'static str {
    tier_hint(tier)
}

/// 档位在当前 locale 下的显示名：只说「什么档」，不说工具——档位不影响工具集。
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
        AuxRole::DeepResearch => t("Deep research", "深度研究"),
    }
}

/// 「空池 = 继承全局池」的统一措辞，档位行和各处池引用共用。
pub(in crate::config_tui) fn inherits_global_pool_label() -> &'static str {
    t("inherits global pool", "继承全局池")
}

/// 显式指向全局池时的措辞。
pub(in crate::config_tui) fn global_pool_label() -> &'static str {
    t("global pool", "全局池")
}

/// 池成员摘要；空池就是继承全局池。
pub(in crate::config_tui) fn tier_pool_summary(config: &AppConfig, tier: ModelTier) -> String {
    let pool = config.tier_choices(tier);
    if pool.is_empty() {
        inherits_global_pool_label().to_string()
    } else {
        pool.iter()
            .map(|choice| choice.model.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    }
}

/// 旁路行右列：档位显示名；显式 global 就写全局池。档位池空了会在运行时回退
/// 全局池，这一层不再重复说——档位行自己已经写着「继承全局池」。
pub(in crate::config_tui) fn aux_role_summary(config: &AppConfig, role: AuxRole) -> String {
    match config.model_tiers.role_tier(role) {
        None => global_pool_label().to_string(),
        Some(tier) => tier_hint(tier).to_string(),
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
        // 两组共用一个左列宽度，冒号才能对齐；按显示宽度算，中文双宽不会错位。
        let name_width = ModelTier::ALL
            .iter()
            .map(|tier| display_width(tier_hint(*tier)))
            .chain(
                AuxRole::ALL
                    .iter()
                    .map(|role| display_width(aux_role_label(*role))),
            )
            .max()
            .unwrap_or(8);
        let mut options: Vec<String> = ModelTier::ALL
            .iter()
            .map(|tier| {
                format!(
                    "{}: {}",
                    pad(tier_hint(*tier), name_width),
                    tier_pool_summary(config, *tier),
                )
            })
            .collect();
        options.push("─".repeat(44));
        options.extend(AuxRole::ALL.iter().map(|role| {
            format!(
                "{}: {}",
                pad(aux_role_label(*role), name_width),
                aux_role_summary(config, *role),
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
    let title = format!(" {} · {} ", t("TIER POOL", "档位池"), tier_hint(tier));
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

/// 旁路请求的档位单选：四档 + 全局池。选定即写入显式值（含 global）。
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
        .map(|tier| tier_hint(*tier).to_string())
        .collect();
    options.push(t("global text pool", "全局文本池").to_string());
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
