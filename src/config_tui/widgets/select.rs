//! 选择器：布尔、单选、多选。
//!
//! 选项的显示文本要走本地化（`localized_choice_label`），但**存回配置的是内部
//! 值**——把显示文本存进配置，换了语言就读不出来了。

use crate::config_tui::*;

pub(in crate::config_tui) fn select_bool(
    stdout: &mut io::Stdout,
    label: &str,
    current: bool,
) -> Result<bool> {
    let mut selected = if current { 0 } else { 1 };
    let options = [
        boolean_label(true).to_string(),
        boolean_label(false).to_string(),
    ];
    loop {
        draw_menu(stdout, label, &options, selected, "")?;
        match read_key()? {
            KeyCode::Esc | KeyCode::Char('q') => return Ok(current),
            KeyCode::Up | KeyCode::Char('k') => selected = selected.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => selected = (selected + 1).min(options.len() - 1),
            KeyCode::Enter => return Ok(selected == 0),
            _ => {}
        }
    }
}

pub(in crate::config_tui) fn select_choice(
    stdout: &mut io::Stdout,
    label: &str,
    current: &str,
    choices: &[String],
    empty_label: &'static str,
    raw_choice_labels: bool,
) -> Result<String> {
    let mut selected = choices.iter().position(|item| item == current).unwrap_or(0);
    loop {
        let options = choices
            .iter()
            .map(|choice| choice_display_label(choice, empty_label, raw_choice_labels))
            .collect::<Vec<_>>();
        draw_menu(stdout, label, &options, selected, "")?;
        match read_key()? {
            KeyCode::Esc | KeyCode::Char('q') => return Ok(current.to_string()),
            KeyCode::Up | KeyCode::Char('k') => selected = selected.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => selected = (selected + 1).min(choices.len() - 1),
            KeyCode::Enter => return Ok(choices[selected].clone()),
            _ => {}
        }
    }
}

pub(in crate::config_tui) fn select_multi_choice(
    stdout: &mut io::Stdout,
    label: &str,
    current: &str,
    choices: &[String],
) -> Result<String> {
    let mut selected = 0usize;
    let mut active = choices
        .iter()
        .map(|choice| has_modality(current, choice))
        .collect::<Vec<_>>();
    loop {
        let options = choices
            .iter()
            .zip(&active)
            .map(|(choice, active)| {
                format!(
                    "{} {}",
                    if *active { "[*]" } else { "[ ]" },
                    choice_label(choice, "")
                )
            })
            .collect::<Vec<_>>();
        draw_menu(
            stdout,
            label,
            &options,
            selected,
            t(
                "[Tab]select/deselect [Enter/q]confirm",
                "[Tab]选择/取消 [Enter/q]确认",
            ),
        )?;
        match read_key()? {
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Enter => {
                return Ok(choices
                    .iter()
                    .zip(active)
                    .filter_map(|(choice, active)| active.then(|| choice.clone()))
                    .collect::<Vec<_>>()
                    .join(", "))
            }
            KeyCode::Up | KeyCode::Char('k') => selected = selected.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => selected = (selected + 1).min(choices.len() - 1),
            KeyCode::Tab | KeyCode::Char(' ') => active[selected] = !active[selected],
            _ => {}
        }
    }
}

pub(in crate::config_tui) fn choice_label(choice: &str, empty_label: &str) -> String {
    if choice.is_empty() {
        empty_label.to_string()
    } else if let Some((provider, model)) = choice.split_once('\t') {
        format!("{provider} / {model}")
    } else if let Some(label) = localized_choice_label(choice, is_zh()) {
        label.to_string()
    } else {
        choice.to_string()
    }
}

pub(in crate::config_tui) fn choice_display_label(
    choice: &str,
    empty_label: &str,
    raw: bool,
) -> String {
    if choice.is_empty() {
        empty_label.to_string()
    } else if raw {
        choice.to_string()
    } else {
        choice_label(choice, empty_label)
    }
}

pub(in crate::config_tui) fn boolean_label(value: bool) -> &'static str {
    if value {
        t("Enabled", "启用")
    } else {
        t("Disabled", "禁用")
    }
}

pub(in crate::config_tui) fn localized_choice_label(value: &str, zh: bool) -> Option<&'static str> {
    if let Some(label) = language_choice_label(value, zh) {
        return Some(label);
    }
    match (value.trim(), zh) {
        ("normal", false) => Some("Normal mode (normal)"),
        ("normal", true) => Some("普通模式（normal）"),
        ("dev", false) => Some("Dev mode (dev)"),
        ("dev", true) => Some("开发模式（dev）"),
        ("minimal", false) => Some("Minimal"),
        ("minimal", true) => Some("最低"),
        ("low", false) => Some("Low"),
        ("low", true) => Some("低"),
        ("medium", false) => Some("Medium"),
        ("medium", true) => Some("中"),
        ("high", false) => Some("High"),
        ("high", true) => Some("高"),
        ("xhigh", false) => Some("Extra high"),
        ("xhigh", true) => Some("极高"),
        ("global", false) => Some("Global"),
        ("global", true) => Some("全球"),
        ("mainland", false) => Some("Mainland China"),
        ("mainland", true) => Some("中国大陆"),
        ("summary", false) => Some("Summary"),
        ("summary", true) => Some("摘要"),
        ("full", false) => Some("Full"),
        ("full", true) => Some("完整"),
        ("hidden", false) => Some("Hidden"),
        ("hidden", true) => Some("隐藏"),
        ("stub", false) => Some("Load on demand"),
        ("stub", true) => Some("需加载"),
        ("off", false) => Some("Off"),
        ("off", true) => Some("关"),
        ("interactive", false) => Some("Interactive only"),
        ("interactive", true) => Some("仅交互模式"),
        ("all", false) => Some("All modes"),
        ("all", true) => Some("全部模式"),
        ("pop", false) => Some("Remove oldest"),
        ("pop", true) => Some("弹出旧消息"),
        ("compact", false) => Some("Compact context"),
        ("compact", true) => Some("压缩上下文"),
        ("text", false) => Some("Text"),
        ("text", true) => Some("文本"),
        ("image", false) => Some("Image"),
        ("image", true) => Some("图片"),
        ("audio", false) => Some("Audio"),
        ("audio", true) => Some("音频"),
        ("video", false) => Some("Video"),
        ("video", true) => Some("视频"),
        ("pdf", false) => Some("PDF"),
        ("pdf", true) => Some("PDF"),
        ("自动", false) => Some("Auto"),
        ("自动", true) => Some("自动"),
        _ => None,
    }
}
