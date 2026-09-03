//! 回合弹出与会话重置。
//!
//! `pop` 把最近若干轮从上下文里摘掉——不是删除，是归档到「弹出上下文」，之
//! 后还能用 `search_evicted_context` 找回来。`reset` 换一个新会话，`wipe` 才
//! 是真的清空。
//!
//! 菜单那半边要在几行之内让人认出「这是哪一轮」：时间、用户说了什么、助手回
//! 了什么摘要，还要标出被打断的回合——那种回合摘要里全是半截话，不标出来会
//! 让人以为选错了。

use crate::cli::repl::width::*;
use crate::cli::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::cli) struct PopOutcome {
    pub(in crate::cli) turns: usize,
    pub(in crate::cli) archived: bool,
}

pub(in crate::cli) fn run_pop(paths: &MiyuPaths, args: PopArgs) -> Result<()> {
    let config = AppConfig::load_or_default(paths)?;
    let state = StateStore::new(paths)?;
    state.recover_stale_turns()?;
    if let Some(outcome) = execute_pop(paths, &config, &state, args.count)? {
        print_pop_outcome(outcome);
    }
    Ok(())
}

/// Pop while the daemon owns the core: candidates are selected locally
/// (read-only), but the mutation goes through IPC so the daemon stays the
/// single writer.
pub(in crate::cli) async fn run_pop_via_daemon(paths: &MiyuPaths, args: PopArgs) -> Result<()> {
    let state = StateStore::new(paths)?;
    let turn_ids: Vec<String> = match args.count {
        Some(count) => {
            validate_pop_count(count)?;
            state
                .oldest_evictable_visible_turns(count)?
                .into_iter()
                .map(|turn| turn.turn_id)
                .collect()
        }
        None => {
            if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
                bail!(
                    "{}",
                    t(
                        "interactive pop requires a terminal; use `miyu pop <count>`",
                        "交互 pop 需要终端；请使用 `miyu pop <数量>`",
                    )
                );
            }
            let limit = usize::try_from(i64::MAX).unwrap_or(usize::MAX);
            let candidates = state.oldest_evictable_visible_turns(limit)?;
            if candidates.is_empty() {
                print_nothing_to_pop();
                return Ok(());
            }
            let Some(selected) = inline_pop_select(&candidates)? else {
                return Ok(());
            };
            candidates
                .into_iter()
                .zip(selected)
                .filter_map(|(turn, selected)| selected.then_some(turn.turn_id))
                .collect()
        }
    };
    if turn_ids.is_empty() {
        print_nothing_to_pop();
        return Ok(());
    }
    let (_, data) = send_ipc_admin(
        paths,
        IpcCommand::Pop {
            target: crate::ipc::SessionRef::Current,
            turn_ids,
        },
    )
    .await?;
    let turns = data
        .get("turns")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);
    if turns > 0 {
        print_pop_outcome(PopOutcome {
            turns: turns as usize,
            archived: data
                .get("archived")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false),
        });
    } else {
        print_nothing_to_pop();
    }
    Ok(())
}

pub(in crate::cli) fn execute_pop(
    paths: &MiyuPaths,
    config: &AppConfig,
    state: &StateStore,
    count: Option<usize>,
) -> Result<Option<PopOutcome>> {
    let turns = match count {
        Some(count) => {
            validate_pop_count(count)?;
            state.oldest_evictable_visible_turns(count)?
        }
        None => {
            if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
                bail!(
                    "{}",
                    t(
                        "interactive pop requires a terminal; use `miyu pop <count>`",
                        "交互 pop 需要终端；请使用 `miyu pop <数量>`",
                    )
                );
            }
            let limit = usize::try_from(i64::MAX).unwrap_or(usize::MAX);
            let candidates = state.oldest_evictable_visible_turns(limit)?;
            if candidates.is_empty() {
                print_nothing_to_pop();
                return Ok(None);
            }
            let Some(selected) = inline_pop_select(&candidates)? else {
                return Ok(None);
            };
            let selected = candidates
                .into_iter()
                .zip(selected)
                .filter_map(|(turn, selected)| selected.then_some(turn))
                .collect::<Vec<_>>();
            if selected.is_empty() {
                return Ok(None);
            }
            selected
        }
    };
    if turns.is_empty() {
        print_nothing_to_pop();
        return Ok(None);
    }

    let memory = MemoryStore::new(config, paths);
    archive_and_delete_visible_turns(state, &memory, &turns)?;
    let memory_config = config.memory_config();
    Ok(Some(PopOutcome {
        turns: turns.len(),
        archived: memory_config.enabled && memory_config.evicted_context_enabled,
    }))
}

pub(in crate::cli) fn validate_pop_count(count: usize) -> Result<usize> {
    if count == 0 {
        bail!(
            "{}",
            t("pop count must be greater than zero", "pop 数量必须大于 0")
        );
    }
    Ok(count)
}

pub(in crate::cli) fn parse_positive_pop_count(value: &str) -> std::result::Result<usize, String> {
    let count = value.parse::<usize>().map_err(|_| {
        t(
            "pop count must be a positive integer",
            "pop 数量必须是正整数",
        )
        .to_string()
    })?;
    if count == 0 {
        return Err(t("pop count must be greater than zero", "pop 数量必须大于 0").to_string());
    }
    Ok(count)
}

pub(in crate::cli) fn parse_repl_pop_count(args: &str) -> Result<Option<usize>> {
    let mut parts = args.split_whitespace();
    let Some(value) = parts.next() else {
        return Ok(None);
    };
    if parts.next().is_some() {
        bail!(
            "{}",
            t("usage: /pop [positive integer]", "用法：/pop [正整数]")
        );
    }
    let count = parse_positive_pop_count(value).map_err(anyhow::Error::msg)?;
    validate_pop_count(count).map(Some)
}

pub(in crate::cli) fn print_pop_outcome(outcome: PopOutcome) {
    let message = if is_zh() {
        if outcome.archived {
            format!("已弹出 {} 轮 · 已归档", outcome.turns)
        } else {
            format!(
                "已弹出 {} 轮 · 未归档（弹出上下文归档已关闭）",
                outcome.turns
            )
        }
    } else {
        let turns = if outcome.turns == 1 { "turn" } else { "turns" };
        if outcome.archived {
            format!("popped {} {turns} · archived", outcome.turns)
        } else {
            format!(
                "popped {} {turns} · not archived (evicted-context archiving is disabled)",
                outcome.turns
            )
        }
    };
    println!("\x1b[2m{message}\x1b[0m\n");
}

pub(in crate::cli) fn print_nothing_to_pop() {
    println!(
        "\x1b[2m{}\x1b[0m\n",
        t(
            "no conversation turns are available to pop",
            "没有可弹出的上下文轮次"
        )
    );
}

pub(in crate::cli) fn inline_pop_select(turns: &[Turn]) -> Result<Option<Vec<bool>>> {
    let menu_lines = inline_pop_lines(turns.len());
    let visible_items = menu_lines.saturating_sub(2) as usize / 3;
    reserve_inline_fuzzy_space(menu_lines)?;
    let mut session = InlineRawMode::start()?;
    let matcher = SkimMatcherV2::default();
    let search_items = turns.iter().map(pop_search_text).collect::<Vec<_>>();
    let mut active = vec![false; turns.len()];
    let mut query = String::new();
    let mut selected = 0usize;
    let mut scroll = 0usize;
    let (_, cursor_y) = cursor::position().unwrap_or((0, menu_lines.saturating_sub(1)));
    let anchor_y = cursor_y.saturating_sub(menu_lines.saturating_sub(1));
    loop {
        let matches = pop_matches(&matcher, &search_items, &query);
        if selected >= matches.len() {
            selected = matches.len().saturating_sub(1);
        }
        let visible = matches.len().min(visible_items);
        scroll = inline_fuzzy_scroll(selected, scroll, visible);
        draw_inline_pop(
            &mut session.stdout,
            anchor_y,
            menu_lines,
            turns,
            &matches,
            selected,
            scroll,
            &active,
            &query,
        )?;
        if let Event::Key(KeyEvent {
            code, modifiers, ..
        }) = event::read()?
        {
            match code {
                KeyCode::Char('c') if modifiers.contains(KeyModifiers::CONTROL) => {
                    clear_inline_fuzzy(&mut session.stdout, anchor_y, menu_lines)?;
                    return Ok(None);
                }
                KeyCode::Esc => {
                    clear_inline_fuzzy(&mut session.stdout, anchor_y, menu_lines)?;
                    return Ok(None);
                }
                KeyCode::Char('q') => {
                    clear_inline_fuzzy(&mut session.stdout, anchor_y, menu_lines)?;
                    return Ok(None);
                }
                KeyCode::Enter => {
                    clear_inline_fuzzy(&mut session.stdout, anchor_y, menu_lines)?;
                    return Ok(Some(active));
                }
                KeyCode::Tab => {
                    if let Some(index) = matches.get(selected) {
                        if let Some(value) = active.get_mut(*index) {
                            *value = !*value;
                        }
                    }
                }
                KeyCode::Up | KeyCode::Char('k') => selected = selected.saturating_sub(1),
                KeyCode::Down | KeyCode::Char('j') => {
                    selected = (selected + 1).min(matches.len().saturating_sub(1));
                }
                KeyCode::Backspace => {
                    query.pop();
                    selected = 0;
                    scroll = 0;
                }
                KeyCode::Char(ch) if !modifiers.contains(KeyModifiers::CONTROL) => {
                    query.push(ch);
                    selected = 0;
                    scroll = 0;
                }
                _ => {}
            }
        }
    }
}

pub(in crate::cli) fn pop_matches(
    matcher: &SkimMatcherV2,
    items: &[String],
    query: &str,
) -> Vec<usize> {
    items
        .iter()
        .enumerate()
        .filter_map(|(index, item)| {
            (query.trim().is_empty() || matcher.fuzzy_match(item, query).is_some()).then_some(index)
        })
        .collect()
}

#[allow(clippy::too_many_arguments)]
pub(in crate::cli) fn draw_inline_pop(
    stdout: &mut io::Stdout,
    anchor_y: u16,
    menu_lines: u16,
    turns: &[Turn],
    matches: &[usize],
    selected: usize,
    scroll: usize,
    active: &[bool],
    query: &str,
) -> Result<()> {
    let (cols, _) = terminal::size().unwrap_or((80, 24));
    let bar = inline_fuzzy_bar();
    let width = (cols as usize).saturating_sub(visible_width(&bar)).max(1);
    let visible_items = menu_lines.saturating_sub(2) as usize / 3;
    queue!(stdout, Hide)?;
    for row in 0..menu_lines {
        queue!(
            stdout,
            MoveTo(0, anchor_y + row),
            Clear(ClearType::CurrentLine)
        )?;
    }
    queue!(
        stdout,
        MoveTo(0, anchor_y),
        Print(&bar),
        Print(pop_menu_header(
            query,
            active.iter().filter(|selected| **selected).count(),
            turns.len(),
            width,
        )),
    )?;
    if matches.is_empty() {
        queue!(
            stdout,
            MoveTo(0, anchor_y + 1),
            Print(&bar),
            Print(format!("\x1b[2m{}\x1b[0m", t("no matches", "没有匹配项")))
        )?;
    } else {
        for (row, item_index) in matches.iter().skip(scroll).take(visible_items).enumerate() {
            let focused = scroll + row == selected;
            let checked = active.get(*item_index).copied().unwrap_or(false);
            let lines = pop_menu_turn_lines(&turns[*item_index], focused, checked, width);
            for (line_offset, line) in lines.into_iter().enumerate() {
                queue!(
                    stdout,
                    MoveTo(0, anchor_y + 1 + row as u16 * 3 + line_offset as u16),
                    Print(&bar),
                    Print(line)
                )?;
            }
        }
    }
    queue!(
        stdout,
        MoveTo(0, anchor_y + menu_lines.saturating_sub(1)),
        Print(&bar),
        Print(pop_menu_help_line(width))
    )?;
    stdout.flush()?;
    Ok(())
}

pub(in crate::cli) fn pop_menu_header(
    query: &str,
    selected: usize,
    total: usize,
    width: usize,
) -> String {
    let title = if query.trim().is_empty() {
        t("Pop context", "弹出上下文").to_string()
    } else {
        format!(
            "{} · {}: {}",
            t("Pop context", "弹出上下文"),
            t("Search", "搜索"),
            query.trim()
        )
    };
    let count = if is_zh() {
        format!("已选 {selected} / {total}")
    } else {
        format!("selected {selected} / {total}")
    };
    let count_width = visible_width(&count);
    if count_width >= width {
        return format!("\x1b[2m{}\x1b[0m", truncate_visible_width(&count, width));
    }
    let title_width = width.saturating_sub(count_width + 1);
    let title = truncate_visible_width(&title, title_width);
    let gap = width
        .saturating_sub(visible_width(&title).saturating_add(count_width))
        .max(1);
    format!(
        "\x1b[1m{title}\x1b[0m{}\x1b[2m{count}\x1b[0m",
        " ".repeat(gap)
    )
}

pub(in crate::cli) fn pop_menu_turn_lines(
    turn: &Turn,
    focused: bool,
    checked: bool,
    width: usize,
) -> [String; 3] {
    let cursor = if focused { "›" } else { " " };
    let marker = if checked { "[*]" } else { "[ ]" };
    let lines = [
        format!(
            "{cursor} {marker} {}",
            pop_menu_timestamp(&turn.user_timestamp)
        ),
        format!(
            "      {}{}",
            t("You: ", "你："),
            pop_menu_summary(&turn.user_content)
        ),
        format!(
            "      {}{}",
            t("AI: ", "AI："),
            pop_menu_assistant_summary(turn)
        ),
    ];
    lines.map(|line| {
        let line = truncate_visible_width(&line, width);
        if focused {
            format!("\x1b[1m\x1b[35m{line}\x1b[0m")
        } else if checked {
            format!("\x1b[1m\x1b[32m{line}\x1b[0m")
        } else {
            format!("\x1b[2m{line}\x1b[0m")
        }
    })
}

pub(in crate::cli) fn pop_menu_timestamp(timestamp: &str) -> String {
    DateTime::parse_from_rfc3339(timestamp)
        .map(|timestamp| {
            timestamp
                .with_timezone(&Local)
                .format("%Y-%m-%d %H:%M")
                .to_string()
        })
        .unwrap_or_else(|_| pop_menu_summary(timestamp))
}

pub(in crate::cli) fn pop_menu_assistant_summary(turn: &Turn) -> String {
    if turn.status == TurnStatus::Interrupted {
        t("(reply interrupted)", "（回复已中断）").to_string()
    } else {
        pop_menu_summary(&turn.assistant_content)
    }
}

pub(in crate::cli) fn pop_menu_summary(content: &str) -> String {
    strip_terminal_control_sequences(content)
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or_else(|| t("(empty)", "（空）"))
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

pub(in crate::cli) fn pop_search_text(turn: &Turn) -> String {
    format!(
        "{} {} {}",
        pop_menu_timestamp(&turn.user_timestamp),
        pop_menu_summary(&turn.user_content),
        pop_menu_assistant_summary(turn)
    )
}

pub(in crate::cli) fn pop_menu_help_line(width: usize) -> String {
    let line = t(
        "Up/Down or j/k move · Tab toggle · Enter pop · Esc/q cancel",
        "↑/↓ 或 j/k 移动 · Tab 勾选 · Enter 弹出 · Esc/q 取消",
    );
    format!("\x1b[2m{}\x1b[0m", truncate_visible_width(line, width))
}

pub(in crate::cli) fn inline_pop_lines(item_count: usize) -> u16 {
    let (_, terminal_rows) = terminal::size().unwrap_or((80, 24));
    let available_items = terminal_rows.saturating_sub(2).saturating_div(3).max(1) as usize;
    let visible_items = item_count.min(5).min(available_items).max(1);
    (visible_items as u16).saturating_mul(3).saturating_add(2)
}

/// Remote-REPL text equivalent of `print_pop_outcome` (which stays
/// println-based for the direct REPL and one-shot `miyu pop`).
pub(in crate::cli) fn repl_pop_outcome_text(outcome: PopOutcome) -> String {
    let message = if is_zh() {
        if outcome.archived {
            format!("已弹出 {} 轮 · 已归档", outcome.turns)
        } else {
            format!(
                "已弹出 {} 轮 · 未归档（弹出上下文归档已关闭）",
                outcome.turns
            )
        }
    } else {
        let turns = if outcome.turns == 1 { "turn" } else { "turns" };
        if outcome.archived {
            format!("popped {} {turns} · archived", outcome.turns)
        } else {
            format!(
                "popped {} {turns} · not archived (evicted-context archiving is disabled)",
                outcome.turns
            )
        }
    };
    format!("\x1b[2m{message}\x1b[0m\n")
}

/// Remote-REPL text equivalent of `print_nothing_to_pop`.
pub(in crate::cli) fn repl_nothing_to_pop_text() -> String {
    format!(
        "\x1b[2m{}\x1b[0m\n",
        t(
            "no conversation turns are available to pop",
            "没有可弹出的上下文轮次"
        )
    )
}

pub(in crate::cli) async fn run_reset(paths: &MiyuPaths) -> Result<()> {
    let config = AppConfig::load_or_default(paths)?;
    let state = StateStore::new(paths)?;
    let memory = MemoryStore::new(&config, paths);
    state.reset_conversation()?;
    crate::llm::forget_relay_sessions(&state.session_id());
    memory.clear_evicted_context()?;
    memory.clear_pending_events()?;
    tools::clear_aur_review_state(paths)?;
    Ok(())
}

/// `miyu reset-memory`:清空当前人格的长期记忆。daemon 在跑走 IPC,
/// 否则本地直清。
///
/// 不再二次确认:清的只是长期记忆(事实/日记/经历),会话历史、技能和知识库
/// 都不动,和 `/wipe` 那种不可逆的整体抹除不是一个量级。确认弹窗还顺带把
/// 这条命令钉死在终端上——非交互调用只能拿到"需要在终端确认"的报错。
pub(in crate::cli) async fn run_reset_memory_command(paths: &MiyuPaths) -> Result<()> {
    if ipc::daemon_info(paths).await.is_some() {
        send_ipc_admin(paths, IpcCommand::ResetMemory { mode: None }).await?;
    } else {
        let config = AppConfig::load_or_default(paths)?;
        MemoryStore::new(&config, paths).reset_all(false)?;
    }
    println!("{}", t("long-term memory erased", "长期记忆已清空"));
    Ok(())
}

pub(in crate::cli) fn wipe_summary() -> &'static str {
    t(
        "This erases everything Miyu has accumulated: memory, every conversation's contents, group-chat contexts, and auto-generated skills. It cannot be undone.",
        "这会抹掉 Miyu 积累的一切：记忆、所有会话的内容、群聊上下文、自动生成的技能。不可撤销。",
    )
}

pub(in crate::cli) async fn run_wipe(paths: &MiyuPaths, assume_yes: bool) -> Result<()> {
    if !assume_yes {
        if !io::stdin().is_terminal() {
            bail!(
                "{}",
                t(
                    "wipe needs a terminal to confirm; pass --yes to run it unattended",
                    "wipe 需要在终端确认；非交互场景请加 --yes"
                )
            );
        }
        println!("{}", wipe_summary());
        if !confirm_stdin(t("wipe everything?", "确认全部抹掉？"))? {
            println!("{}", t("cancelled", "已取消"));
            return Ok(());
        }
    }
    if ipc::daemon_info(paths).await.is_some() {
        send_ipc_admin(paths, IpcCommand::WipePersona).await?;
    } else {
        let config = AppConfig::load_or_default(paths)?;
        let state = StateStore::new(paths)?;
        let persona = config.active_persona_scope();
        let bindings = state.platform_session_bindings(&persona, "onebot")?;
        let plugins = crate::platforms::plugins::PlatformPluginRegistry::built_in()?;
        plugins
            .after_persona_reset(&crate::platforms::plugins::PlatformPersonaResetContext {
                config: &config,
                paths,
                bindings: &bindings,
            })
            .await?;
        let cleared_sessions = state.reset_persona_contexts(&persona, "onebot")?;
        for session_id in &cleared_sessions {
            crate::llm::forget_relay_sessions(session_id);
        }
        state.reset_conversation_usage()?;
        MemoryStore::new(&config, paths).reset_all(true)?;
        tools::clear_aur_review_state(paths)?;
    }
    println!("{}", print_wipe_message());
    Ok(())
}

pub(in crate::cli) fn print_wipe_message() -> &'static str {
    t(
        "erased all conversations, QQ contexts, memory, and generated skills for the current persona",
        "已抹掉当前人格的全部会话内容、QQ 上下文、记忆和自动技能",
    )
}

pub(in crate::cli) fn print_reset_message() {
    let message = t("cleared current conversation history", "已清空当前会话历史");
    println!("\x1b[2m{message}\x1b[0m\n");
}
