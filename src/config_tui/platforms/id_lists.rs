//! ID 与关键词列表的编辑。
//!
//! 这些列表决定谁能指挥 Miyu、哪些词会触发它，所以解析严格：
//! `parse_positive_id` 拒绝一切不是正整数的东西，宁可让用户重填，也不能把
//! 「填错的那一行」静默丢掉——静默丢掉意味着权限没生效而用户以为生效了。

use crate::config_tui::*;

pub(in crate::config_tui) fn parse_positive_id(value: &str) -> std::result::Result<i64, String> {
    value
        .trim()
        .parse::<i64>()
        .ok()
        .filter(|id| *id > 0)
        .ok_or_else(|| {
            t(
                "QQ/group id must be a positive integer.",
                "QQ 号/群号必须是正整数。",
            )
            .to_string()
        })
}

pub(in crate::config_tui) fn parse_id_lines(value: &str) -> std::result::Result<Vec<i64>, String> {
    let mut parsed = Vec::new();
    for (index, line) in value.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let id = parse_positive_id(line)
            .map_err(|error| format!("{} {}: {error}", t("Line", "第"), index + 1))?;
        if !parsed.contains(&id) {
            parsed.push(id);
        }
    }
    Ok(parsed)
}

pub(in crate::config_tui) fn prompt_single_id(
    stdout: &mut io::Stdout,
    item_label: &str,
    current: Option<i64>,
) -> Result<Option<i64>> {
    let action = if current.is_some() {
        t("Edit", "编辑")
    } else {
        t("Add", "新增")
    };
    let title = format!(" {action} {item_label} ");
    let Some(value) = edit_inline_value(
        stdout,
        &title,
        &current.map(|id| id.to_string()).unwrap_or_default(),
        false,
    )?
    else {
        return Ok(None);
    };
    match parse_positive_id(&value) {
        Ok(id) => Ok(Some(id)),
        Err(error) => {
            message(stdout, &error)?;
            Ok(None)
        }
    }
}

/// 管理员列表:每项 QQ 号 + 别名(别名给终端发消息工具的 `to` 列表用)。
/// 第一项是主管理员。
pub(in crate::config_tui) fn edit_qq_admin_list(
    stdout: &mut io::Stdout,
    title: &'static str,
    ids: &mut Vec<i64>,
    aliases: &mut std::collections::BTreeMap<String, String>,
) -> Result<()> {
    let mut selected = 0usize;
    loop {
        let mut options = vec![t("+ Add", "+ 新增").to_string()];
        options.extend(ids.iter().enumerate().map(|(index, id)| {
            let alias = aliases.get(&id.to_string()).map(String::as_str).unwrap_or("");
            let primary = if index == 0 { t(" (primary)", "(主管理员)") } else { "" };
            if alias.is_empty() {
                format!("{id}{primary}")
            } else {
                format!("{id}  {alias}{primary}")
            }
        }));
        draw_menu(
            stdout,
            title,
            &options,
            selected,
            t(
                "[Enter]add/edit [Delete]remove [j/k]move [q]back",
                "[Enter]新增/编辑 [Delete]删除 [j/k]移动 [q]返回",
            ),
        )?;
        match read_key()? {
            KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
            KeyCode::Up | KeyCode::Char('k') => selected = selected.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => selected = (selected + 1).min(options.len() - 1),
            KeyCode::Enter => {
                let index = if selected == 0 { None } else { Some(selected - 1) };
                let current_id = index.and_then(|i| ids.get(i).copied());
                let current_alias = current_id
                    .and_then(|id| aliases.get(&id.to_string()).cloned())
                    .unwrap_or_default();
                let mut fields = vec![
                    Field::new(
                        t("QQ id", "QQ 号"),
                        current_id.map(|id| id.to_string()).unwrap_or_default(),
                    ),
                    Field::new(
                        t("Alias (recipient name shown to the AI)", "别名(AI 发消息时的收件人名)"),
                        current_alias,
                    ),
                ];
                if !run_form_editing(stdout, t(" ADMINISTRATOR ", " 管理员 "), &mut fields)? {
                    continue;
                }
                let id = match parse_positive_id(&fields[0].value) {
                    Ok(id) => id,
                    Err(error) => {
                        message(stdout, &error)?;
                        continue;
                    }
                };
                let alias = fields[1].value.trim().to_string();
                if ids
                    .iter()
                    .enumerate()
                    .any(|(other, item)| Some(other) != index && *item == id)
                {
                    message(stdout, t("That id already exists.", "该号码已存在。"))?;
                    continue;
                }
                match index {
                    Some(i) => {
                        if let Some(old) = ids.get(i).copied() {
                            aliases.remove(&old.to_string());
                        }
                        ids[i] = id;
                    }
                    None => ids.push(id),
                }
                if alias.is_empty() {
                    aliases.remove(&id.to_string());
                } else {
                    aliases.insert(id.to_string(), alias);
                }
            }
            KeyCode::Delete | KeyCode::Backspace if selected >= 1 => {
                let removed = ids.remove(selected - 1);
                aliases.remove(&removed.to_string());
                selected = selected.min(ids.len());
            }
            _ => {}
        }
    }
}

pub(in crate::config_tui) fn edit_qq_id_list(
    stdout: &mut io::Stdout,
    title: &'static str,
    item_label: &'static str,
    ids: &mut Vec<i64>,
) -> Result<()> {
    let mut selected = 0usize;
    loop {
        let mut options = vec![
            t("+ Add one", "+ 新增一项").to_string(),
            t("+ Add multiple", "+ 批量新增").to_string(),
        ];
        options.extend(ids.iter().map(ToString::to_string));
        draw_menu(
            stdout,
            title,
            &options,
            selected,
            t(
                "[Enter]add/edit [Delete]remove [j/k]move [q]back",
                "[Enter]新增/编辑 [Delete]删除 [j/k]移动 [q]返回",
            ),
        )?;
        match read_key()? {
            KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
            KeyCode::Up | KeyCode::Char('k') => selected = selected.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => selected = (selected + 1).min(options.len() - 1),
            KeyCode::Enter if selected == 0 => {
                if let Some(id) = prompt_single_id(stdout, item_label, None)? {
                    if !ids.contains(&id) {
                        ids.push(id);
                    }
                }
            }
            KeyCode::Enter if selected == 1 => {
                let mut value = String::new();
                loop {
                    edit_textarea(stdout, &mut value)?;
                    match parse_id_lines(&value) {
                        Ok(additions) => {
                            for id in additions {
                                if !ids.contains(&id) {
                                    ids.push(id);
                                }
                            }
                            break;
                        }
                        Err(error) => message(stdout, &error.to_string())?,
                    }
                }
            }
            KeyCode::Enter => {
                let index = selected - 2;
                if let Some(id) = prompt_single_id(stdout, item_label, ids.get(index).copied())? {
                    if ids
                        .iter()
                        .enumerate()
                        .any(|(other, item)| other != index && *item == id)
                    {
                        message(stdout, t("That id already exists.", "该号码已存在。"))?;
                    } else if let Some(item) = ids.get_mut(index) {
                        *item = id;
                    }
                }
            }
            KeyCode::Delete | KeyCode::Backspace if selected >= 2 => {
                ids.remove(selected - 2);
                selected = selected.min(ids.len() + 1);
            }
            _ => {}
        }
    }
}

pub(in crate::config_tui) fn parse_keyword_lines(
    value: &str,
) -> std::result::Result<Vec<String>, String> {
    let mut parsed = Vec::new();
    for (index, line) in value.lines().enumerate() {
        let keyword = line.trim();
        if keyword.is_empty() {
            continue;
        }
        if keyword.chars().count() > 128 || keyword.chars().any(char::is_control) {
            return Err(format!(
                "{} {}: {}",
                t("Line", "第"),
                index + 1,
                t("keyword is invalid or too long", "关键词无效或过长")
            ));
        }
        if !parsed.iter().any(|item| item == keyword) {
            parsed.push(keyword.to_string());
        }
    }
    Ok(parsed)
}

pub(in crate::config_tui) fn edit_keyword_list(
    stdout: &mut io::Stdout,
    keywords: &mut Vec<String>,
) -> Result<()> {
    let mut selected = 0usize;
    loop {
        let mut options = vec![
            t("+ Add one", "+ 新增一项").to_string(),
            t("+ Add multiple", "+ 批量新增").to_string(),
        ];
        options.extend(keywords.iter().cloned());
        draw_menu(
            stdout,
            t(" GROUP WAKE KEYWORDS ", " 群聊触发关键词 "),
            &options,
            selected,
            "",
        )?;
        match read_key()? {
            KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
            KeyCode::Up | KeyCode::Char('k') => selected = selected.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => selected = (selected + 1).min(options.len() - 1),
            KeyCode::Enter if selected == 0 => {
                if let Some(value) =
                    edit_inline_value(stdout, t(" ADD KEYWORD ", " 新增关键词 "), "", false)?
                {
                    match parse_keyword_lines(&value) {
                        Ok(additions) if additions.len() == 1 => {
                            let keyword = additions.into_iter().next().unwrap();
                            if !keywords.contains(&keyword) {
                                keywords.push(keyword);
                            }
                        }
                        _ => message(
                            stdout,
                            t("Enter exactly one valid keyword.", "请输入一个有效关键词。"),
                        )?,
                    }
                }
            }
            KeyCode::Enter if selected == 1 => {
                let mut value = String::new();
                loop {
                    edit_textarea(stdout, &mut value)?;
                    match parse_keyword_lines(&value) {
                        Ok(additions) => {
                            for keyword in additions {
                                if !keywords.contains(&keyword) {
                                    keywords.push(keyword);
                                }
                            }
                            break;
                        }
                        Err(error) => message(stdout, &error.to_string())?,
                    }
                }
            }
            KeyCode::Enter => {
                let index = selected - 2;
                if let Some(value) = edit_inline_value(
                    stdout,
                    t(" EDIT KEYWORD ", " 编辑关键词 "),
                    &keywords[index],
                    false,
                )? {
                    match parse_keyword_lines(&value) {
                        Ok(values) if values.len() == 1 => {
                            let value = values[0].clone();
                            if keywords
                                .iter()
                                .enumerate()
                                .any(|(other, item)| other != index && item == &value)
                            {
                                message(
                                    stdout,
                                    t("That keyword already exists.", "该关键词已存在。"),
                                )?;
                            } else {
                                keywords[index] = value;
                            }
                        }
                        _ => message(
                            stdout,
                            t("Enter exactly one valid keyword.", "请输入一个有效关键词。"),
                        )?,
                    }
                }
            }
            KeyCode::Delete | KeyCode::Backspace if selected >= 2 => {
                keywords.remove(selected - 2);
                selected = selected.min(keywords.len() + 1);
            }
            _ => {}
        }
    }
}

pub(in crate::config_tui) fn format_id_list(ids: &[i64]) -> String {
    ids.iter()
        .map(i64::to_string)
        .collect::<Vec<_>>()
        .join(", ")
}

pub(in crate::config_tui) fn parse_id_list(value: &str) -> Result<Vec<i64>> {
    value
        .split([',', ' ', '\u{3000}', ';', '\n', '\r', '\t'])
        .map(str::trim)
        .filter(|item| !item.is_empty())
        .map(|item| {
            let id = item.parse::<i64>().map_err(|_| {
                anyhow::anyhow!(t("invalid id: {}", "无效的号码：{}").replace("{}", item))
            })?;
            if id <= 0 {
                bail!(t(
                    "QQ and group ids must be positive",
                    "QQ 号和群号必须为正数"
                ));
            }
            Ok(id)
        })
        .collect()
}
