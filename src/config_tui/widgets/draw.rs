//! 菜单与框线的绘制，以及宽度计算。
//!
//! 宽度全部按**显示宽度**算（`display_width`）：中文双宽、emoji 更宽，按字符数
//! 算的框一遇中文就错位。`pad` / `truncate` 同理。

use crate::config_tui::*;

pub(in crate::config_tui) fn draw_menu(
    stdout: &mut io::Stdout,
    title: &str,
    options: &[String],
    selected: usize,
    status: &str,
) -> Result<()> {
    let (cols, rows) = terminal::size()?;
    // 按显示宽度定框宽:按字符数算,中文行会被框截成「…」(09-06 音色列表)。
    let content_w = options
        .iter()
        .map(|option| display_width(option))
        .max()
        .unwrap_or(20)
        .max(display_width(title))
        .max(display_width(menu_help(status)))
        + 6;
    let width = (content_w as u16).min(cols.saturating_sub(4)).max(56);
    let height = (options.len() as u16 + 5)
        .min(rows.saturating_sub(2))
        .max(7);
    let x = cols.saturating_sub(width) / 2;
    let y = rows.saturating_sub(height) / 2;
    let visible_rows = height.saturating_sub(4).max(1) as usize;
    let window = menu_window(options.len(), selected, visible_rows);

    queue!(stdout, Clear(ClearType::All))?;
    draw_box(stdout, x, y, width, height, title)?;
    queue!(
        stdout,
        MoveTo(x + 2, y + height - 1),
        SetAttribute(Attribute::Dim),
        Print(truncate(
            menu_help(status),
            width.saturating_sub(4) as usize
        )),
        SetAttribute(Attribute::Reset)
    )?;
    for (row, index) in window.enumerate() {
        let option = &options[index];
        queue!(stdout, MoveTo(x + 2, y + row as u16 + 2))?;
        if index == selected {
            queue!(
                stdout,
                SetAttribute(Attribute::Reverse),
                Print(pad(option, width.saturating_sub(4) as usize)),
                SetAttribute(Attribute::Reset)
            )?;
        } else {
            queue!(stdout, Print(pad(option, width.saturating_sub(4) as usize)))?;
        }
    }
    stdout.flush()?;
    Ok(())
}

pub(in crate::config_tui) fn draw_menu_with_editing(
    stdout: &mut io::Stdout,
    title: &str,
    options: &[String],
    selected: usize,
    status: &str,
    editing: Option<(usize, &str, &str, usize)>,
) -> Result<()> {
    let (cols, rows) = terminal::size()?;
    let editing_width = editing
        .map(|(_, label, value, _)| {
            format!("{label}: {value}")
                .chars()
                .count()
                .saturating_add(2)
        })
        .unwrap_or(0);
    let content_w = options
        .iter()
        .map(|option| option.chars().count())
        .max()
        .unwrap_or(20)
        .max(title.chars().count())
        .max(menu_help(status).chars().count())
        .max(editing_width)
        + 6;
    let width = (content_w as u16).min(cols.saturating_sub(4)).max(56);
    let height = (options.len() as u16 + 5)
        .min(rows.saturating_sub(2))
        .max(7);
    let x = cols.saturating_sub(width) / 2;
    let y = rows.saturating_sub(height) / 2;
    let visible_rows = height.saturating_sub(4).max(1) as usize;
    let window = menu_window(options.len(), selected, visible_rows);
    let inner_width = width.saturating_sub(4) as usize;

    queue!(stdout, Clear(ClearType::All))?;
    draw_box(stdout, x, y, width, height, title)?;
    let footer = if editing.is_some() {
        t(
            "[Enter]save [Esc]cancel [Left/Right/Home/End]edit",
            "[Enter]保存 [Esc]取消 [Left/Right/Home/End]编辑",
        )
    } else {
        menu_help(status)
    };
    queue!(
        stdout,
        MoveTo(x + 2, y + height - 1),
        SetAttribute(Attribute::Dim),
        Print(truncate(footer, inner_width)),
        SetAttribute(Attribute::Reset)
    )?;
    for (row, index) in window.enumerate() {
        queue!(stdout, MoveTo(x + 2, y + row as u16 + 2))?;
        if let Some((edit_index, label, value, cursor)) = editing {
            if edit_index == index {
                let prefix = format!("> {label}: ");
                let before_cursor = format!("{prefix}{}", take_chars(value, cursor));
                let line = format!("{prefix}{value}");
                queue!(
                    stdout,
                    SetAttribute(Attribute::Reverse),
                    Print(pad(&line, inner_width)),
                    SetAttribute(Attribute::Reset),
                    MoveTo(
                        x + 2 + display_width(&truncate(&before_cursor, inner_width)) as u16,
                        y + row as u16 + 2,
                    ),
                    Show,
                )?;
                continue;
            }
        }
        if index == selected {
            queue!(
                stdout,
                SetAttribute(Attribute::Reverse),
                Print(pad(&options[index], inner_width)),
                SetAttribute(Attribute::Reset)
            )?;
        } else {
            queue!(stdout, Print(pad(&options[index], inner_width)))?;
        }
    }
    stdout.flush()?;
    Ok(())
}

pub(in crate::config_tui) fn menu_window(
    item_count: usize,
    selected: usize,
    visible_rows: usize,
) -> std::ops::Range<usize> {
    if item_count == 0 || visible_rows == 0 {
        return 0..0;
    }
    let visible_rows = visible_rows.min(item_count);
    let selected = selected.min(item_count - 1);
    let start = selected
        .saturating_sub(visible_rows / 2)
        .min(item_count - visible_rows);
    start..start + visible_rows
}

pub(in crate::config_tui) fn menu_help(status: &str) -> &str {
    if status.is_empty() {
        t(
            "[j/k]move [Enter]select [q]back",
            "[j/k]移动 [Enter]选择 [q]返回",
        )
    } else {
        status
    }
}

pub(in crate::config_tui) fn draw_box(
    stdout: &mut io::Stdout,
    x: u16,
    y: u16,
    width: u16,
    height: u16,
    title: &str,
) -> Result<()> {
    queue!(
        stdout,
        MoveTo(x, y),
        Print(format!(
            "┌{}┐",
            "─".repeat(width.saturating_sub(2) as usize)
        ))
    )?;
    for row in 1..height.saturating_sub(1) {
        queue!(
            stdout,
            MoveTo(x, y + row),
            Print(format!(
                "│{}│",
                " ".repeat(width.saturating_sub(2) as usize)
            ))
        )?;
    }
    queue!(
        stdout,
        MoveTo(x, y + height.saturating_sub(1)),
        Print(format!(
            "└{}┘",
            "─".repeat(width.saturating_sub(2) as usize)
        ))
    )?;
    queue!(
        stdout,
        MoveTo(x + 2, y),
        SetAttribute(Attribute::Bold),
        Print(title),
        SetAttribute(Attribute::Reset)
    )?;
    Ok(())
}

pub(in crate::config_tui) fn draw_column(
    stdout: &mut io::Stdout,
    x: u16,
    y: u16,
    width: u16,
    height: u16,
    title: &str,
    items: &[String],
    selected: usize,
    scroll: usize,
    active: bool,
) -> Result<()> {
    let attr = if active {
        Attribute::Reverse
    } else {
        Attribute::Bold
    };
    queue!(
        stdout,
        MoveTo(x, y),
        SetAttribute(attr),
        Print(pad(&truncate(title, width as usize), width as usize)),
        SetAttribute(Attribute::Reset)
    )?;
    let visible_rows = height.saturating_sub(2) as usize;
    let start = column_scroll(selected, scroll, visible_rows);
    for row in 0..visible_rows {
        let index = start + row;
        if index >= items.len() {
            break;
        }
        queue!(stdout, MoveTo(x, y + row as u16 + 1))?;
        let line = truncate(&items[index], width as usize);
        if index == selected {
            queue!(
                stdout,
                SetAttribute(Attribute::Reverse),
                Print(pad(&line, width as usize)),
                SetAttribute(Attribute::Reset)
            )?;
        } else {
            queue!(stdout, Print(pad(&line, width as usize)))?;
        }
    }
    Ok(())
}

pub(in crate::config_tui) fn column_visible_rows() -> usize {
    terminal::size()
        .map(|(_, rows)| rows.saturating_sub(4) as usize)
        .unwrap_or(1)
}

pub(in crate::config_tui) fn column_scroll(
    selected: usize,
    scroll: usize,
    visible_rows: usize,
) -> usize {
    if visible_rows == 0 {
        return 0;
    }
    if selected < scroll {
        selected
    } else if selected >= scroll + visible_rows {
        selected + 1 - visible_rows
    } else {
        scroll
    }
}

pub(in crate::config_tui) fn insert_char_at_cursor(
    value: &mut String,
    cursor: &mut usize,
    ch: char,
) {
    let byte_index = byte_index_for_char(value, *cursor);
    value.insert(byte_index, ch);
    *cursor += 1;
}

pub(in crate::config_tui) fn remove_char_before_cursor(value: &mut String, cursor: &mut usize) {
    let end = byte_index_for_char(value, *cursor);
    let start = byte_index_for_char(value, cursor.saturating_sub(1));
    value.replace_range(start..end, "");
    *cursor -= 1;
}

pub(in crate::config_tui) fn remove_char_at_cursor(value: &mut String, cursor: usize) {
    if cursor >= value.chars().count() {
        return;
    }
    let start = byte_index_for_char(value, cursor);
    let end = byte_index_for_char(value, cursor + 1);
    value.replace_range(start..end, "");
}

pub(in crate::config_tui) fn byte_index_for_char(value: &str, char_index: usize) -> usize {
    value
        .char_indices()
        .nth(char_index)
        .map(|(index, _)| index)
        .unwrap_or(value.len())
}

pub(in crate::config_tui) fn take_chars(value: &str, count: usize) -> String {
    value.chars().take(count).collect()
}

pub(in crate::config_tui) fn message(stdout: &mut io::Stdout, text: &str) -> Result<()> {
    queue!(
        stdout,
        Clear(ClearType::All),
        MoveTo(0, 0),
        Print(text),
        MoveTo(0, 2),
        Print(t("Press any key to continue", "按任意键继续"))
    )?;
    stdout.flush()?;
    let _ = read_key()?;
    Ok(())
}

pub(in crate::config_tui) fn read_key() -> Result<KeyCode> {
    read_key_with_timeout(None).map(|key| key.expect("blocking read should return a key"))
}

pub(in crate::config_tui) fn read_key_with_timeout(
    timeout: Option<Duration>,
) -> Result<Option<KeyCode>> {
    loop {
        if let Some(timeout) = timeout {
            if !event::poll(timeout)? {
                return Ok(None);
            }
        }
        if let Event::Key(KeyEvent { code, .. }) = event::read()? {
            return Ok(Some(code));
        }
    }
}

pub(in crate::config_tui) fn active_label(config: &AppConfig) -> String {
    match config.active_provider_model_choices().as_slice() {
        [] => t("Not configured", "未配置").to_string(),
        [choice] => format!("{} / {}", choice.provider_name, choice.model),
        _ => t("Mixed", "混合").to_string(),
    }
}

pub(in crate::config_tui) fn truncate(value: &str, max: usize) -> String {
    if display_width(value) <= max {
        return value.to_string();
    }
    let mut width = 0usize;
    let mut output = String::new();
    let ellipsis_width = 1usize;
    for ch in value.chars() {
        let char_width = display_width(&ch.to_string());
        if width + char_width + ellipsis_width > max {
            break;
        }
        output.push(ch);
        width += char_width;
    }
    output.push('…');
    output
}

pub(in crate::config_tui) fn display_width(value: &str) -> usize {
    value
        .chars()
        .map(|ch| match ch {
            '\u{1100}'..='\u{115F}'
            | '\u{2329}'..='\u{232A}'
            | '\u{2E80}'..='\u{A4CF}'
            | '\u{AC00}'..='\u{D7A3}'
            | '\u{F900}'..='\u{FAFF}'
            | '\u{FE10}'..='\u{FE19}'
            | '\u{FE30}'..='\u{FE6F}'
            | '\u{FF00}'..='\u{FF60}'
            | '\u{FFE0}'..='\u{FFE6}' => 2,
            _ => 1,
        })
        .sum()
}

pub(in crate::config_tui) fn pad(value: &str, width: usize) -> String {
    let value = truncate(value, width);
    let len = display_width(&value);
    if len >= width {
        value
    } else {
        format!("{value}{}", " ".repeat(width - len))
    }
}
