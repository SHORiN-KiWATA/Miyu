//! REPL 的输入编辑器。
//!
//! 一个多行输入框：光标移动、换行、粘贴、历史翻阅，以及「这一行在终端上占
//! 几行」的换行账。它只管**编辑状态**，怎么画到屏幕上是活动区的事。

// 编辑器还用着一批留在 cli::mod 的辅助（光标插入、粘贴占位、命令补全等）。
// 它们本身也该继续往下拆，但那是后面的步骤——先用 super::* 把编译撑住，
// 免得一次改动横跨太多文件而无法定位问题。
use crate::cli::*;

/// 这个会话的上键历史。
///
/// 三个来源按「从旧到新」拼起来——上键是从**末尾**往回走的，顺序错了最近敲的
/// 就不在第一格：
///
/// 1. 分会话之前的全局文件（只读的老数据）
/// 2. 本会话的对话记录里用户说过的话
/// 3. 本会话的历史文件（`/reset` 删掉 turn 之后它仍在，是持久真相）
pub(in crate::cli) fn load_repl_input_history(
    state: &StateStore,
    paths: &MiyuPaths,
) -> Result<Vec<ReplHistoryEntry>> {
    let session_id = state.session_id();
    let mut merged: Vec<ReplHistoryEntry> =
        read_repl_history_file(&legacy_repl_history_file(paths));
    let conversation = state
        .load_conversation()?
        .into_iter()
        .filter(|entry| entry.role == "user" && !entry.content.trim().is_empty())
        .map(|entry| strip_terminal_control_sequences(&entry.content))
        .filter(|content| !content.trim().is_empty());
    for content in conversation {
        merge_history_entry(&mut merged, ReplHistoryEntry::plain(&content));
    }
    // 历史文件的条目带占位符载荷,和对话记录里同一次提交(展开全文)合成一条。
    for entry in load_persistent_repl_history(paths, &session_id) {
        merge_history_entry(&mut merged, entry);
    }
    Ok(merged)
}

/// 从磁盘补齐本会话的历史，返回是否真的多出了条目。
///
/// 历史以前只在 REPL 启动时读一次（`load_repl_input_history`），此后整个循环
/// **没有任何重新加载的路径**。于是两个 REPL 同时开着时，先开的那个永远看不到
/// 后来在另一个窗口敲的东西。上一轮排查按「先敲完再开第二个」测，走的是启动
/// 加载那条路，当然是通的，所以没能复现——测错了顺序。
///
/// 只在「从空输入框开始翻」时调用：翻到一半重载会让 `history_index` 错位。
pub(in crate::cli) fn refresh_repl_input_history(
    history: &mut Vec<ReplHistoryEntry>,
    paths: &MiyuPaths,
    session_id: &str,
) -> bool {
    let mut added = false;
    for entry in load_persistent_repl_history(paths, session_id) {
        added |= merge_history_entry(history, entry);
    }
    added
}

/// 把一条历史放回输入框:占位符照旧是占位符,载荷跟着回来。
pub(in crate::cli) fn restore_history_entry(
    entry: &ReplHistoryEntry,
    input: &mut String,
    cursor: &mut usize,
    pasted_images: &mut Vec<Option<crate::clipboard::PastedImage>>,
    pasted_texts: &mut Vec<Option<PastedText>>,
    raw_pasted_lines: &mut usize,
) {
    *input = entry.display.clone();
    *cursor = input.chars().count();
    *pasted_images = entry.pasted_images();
    *pasted_texts = entry.pasted_texts();
    // 载荷都在占位符后面,输入框里没有生粘贴的行。
    *raw_pasted_lines = 0;
}

pub(in crate::cli) struct LiveReplEditor {
    pub(in crate::cli) mode: AgentMode,
    pub(in crate::cli) input: String,
    pub(in crate::cli) cursor: usize,
    pub(in crate::cli) history: Vec<ReplHistoryEntry>,
    pub(in crate::cli) history_index: usize,
    pub(in crate::cli) history_clean_index: Option<usize>,
    /// 当前缓冲区里由**生粘贴**带进来的行数(折成占位符的不算)。输入区的
    /// 整体折叠只在这种内容占满缓冲区时才发生,见 `repl_visible_input_lines`。
    pub(in crate::cli) raw_pasted_lines: usize,
    pub(in crate::cli) pasted_images: Vec<Option<crate::clipboard::PastedImage>>,
    pub(in crate::cli) pasted_texts: Vec<Option<PastedText>>,
    pub(in crate::cli) escape_armed_until: Option<Instant>,
    /// Whether the terminal window currently has focus, per the terminal's own
    /// focus reporting. Starts `true`: a terminal that never reports focus
    /// leaves this pinned, and notifications stay quiet rather than firing on
    /// every turn.
    pub(in crate::cli) focused: bool,
}

pub(in crate::cli) enum LiveEditorAction {
    None,
    Redraw,
    ClearScreen,
    EmptySubmit,
    Submit(LiveSubmission),
    Interrupt,
    Exit,
}

impl LiveReplEditor {
    pub(in crate::cli) fn new(mode: AgentMode, history: Vec<ReplHistoryEntry>) -> Self {
        let history_index = history.len();
        Self {
            mode,
            input: String::new(),
            cursor: 0,
            history,
            history_index,
            history_clean_index: None,
            raw_pasted_lines: 0,
            pasted_images: Vec::new(),
            pasted_texts: Vec::new(),
            escape_armed_until: None,
            focused: true,
        }
    }

    pub(in crate::cli) fn clear(&mut self) {
        self.input.clear();
        self.cursor = 0;
        self.history_clean_index = None;
        self.raw_pasted_lines = 0;
        self.pasted_images.clear();
        self.pasted_texts.clear();
        self.escape_armed_until = None;
    }

    pub(in crate::cli) fn submit(&mut self) -> Option<LiveSubmission> {
        let display_content = strip_terminal_control_sequences(&self.input);
        let content = expand_pasted_text_placeholders(&display_content, &self.pasted_texts);
        let content = content.trim().to_string();
        if content.is_empty() {
            return None;
        }
        let display_content = display_content.trim().to_string();
        let images = std::mem::take(&mut self.pasted_images);
        let pasted_texts = std::mem::take(&mut self.pasted_texts);
        self.input.clear();
        self.cursor = 0;
        self.history_clean_index = None;
        self.raw_pasted_lines = 0;
        Some(LiveSubmission {
            content,
            display_content,
            images,
            pasted_texts,
        })
    }

    pub(in crate::cli) fn record_history(&mut self, entry: ReplHistoryEntry) {
        push_history_capped(&mut self.history, entry);
        self.history_index = self.history.len();
    }

    fn recall_history_entry(&mut self, index: usize) {
        let entry = self.history.get(index).cloned().unwrap_or_default();
        restore_history_entry(
            &entry,
            &mut self.input,
            &mut self.cursor,
            &mut self.pasted_images,
            &mut self.pasted_texts,
            &mut self.raw_pasted_lines,
        );
        self.history_clean_index = Some(index);
    }

    pub(in crate::cli) fn handle_event(
        &mut self,
        event: Event,
        paths: &MiyuPaths,
        allow_interrupt: bool,
    ) -> Result<LiveEditorAction> {
        let is_escape = matches!(
            &event,
            Event::Key(KeyEvent {
                code: KeyCode::Esc,
                ..
            })
        );
        if !is_escape {
            self.escape_armed_until = None;
        }
        match event {
            Event::Key(KeyEvent {
                kind: KeyEventKind::Release,
                ..
            }) => return Ok(LiveEditorAction::None),
            Event::Resize(_, _) => return Ok(LiveEditorAction::Redraw),
            // Focus reporting gates notifications: no popup while you are
            // looking at the window.
            Event::FocusGained => {
                self.focused = true;
                return Ok(LiveEditorAction::None);
            }
            Event::FocusLost => {
                self.focused = false;
                return Ok(LiveEditorAction::None);
            }
            Event::Paste(text) => {
                let raw_lines = insert_pasted_text_at_cursor(
                    &mut self.input,
                    &mut self.cursor,
                    text,
                    &mut self.pasted_texts,
                );
                self.history_clean_index = None;
                self.raw_pasted_lines = self.raw_pasted_lines.saturating_add(raw_lines);
                return Ok(LiveEditorAction::Redraw);
            }
            Event::Key(KeyEvent {
                code, modifiers, ..
            }) => match code {
                KeyCode::Tab => {
                    if self.input.starts_with('/') {
                        if let Some(completed) = complete_repl_command(&self.input) {
                            self.input = completed.to_string();
                            self.cursor = self.input.chars().count();
                            self.history_clean_index = None;
                        }
                    } else {
                        // 会话模式创建时定死:Tab 切换已随闲聊模式一并删除
                        // (中途换模式=系统提示词换血=全量缓存作废)。
                    }
                }
                KeyCode::Esc => {
                    // Esc never clears typed input (Ctrl+C does that); it
                    // only arms the double-press interrupt while a reply is
                    // running.
                    if allow_interrupt {
                        if self
                            .escape_armed_until
                            .is_some_and(|deadline| Instant::now() < deadline)
                        {
                            self.escape_armed_until = None;
                            return Ok(LiveEditorAction::Interrupt);
                        }
                        self.escape_armed_until = Some(Instant::now() + Duration::from_secs(2));
                    }
                }
                // Ctrl+方向 必须排在裸方向之前:match 从上往下取第一个命中的
                // 分支,裸 `KeyCode::Left` 不看修饰键,写在前面会把 Ctrl 组合
                // 一起吃掉。
                KeyCode::Left if modifiers.contains(KeyModifiers::CONTROL) => {
                    self.cursor = word_start_before_cursor(&self.input, self.cursor);
                }
                KeyCode::Right if modifiers.contains(KeyModifiers::CONTROL) => {
                    self.cursor = word_end_after_cursor(&self.input, self.cursor);
                }
                KeyCode::Left => {
                    if let Some((start, _)) = placeholder_at_cursor(&self.input, self.cursor) {
                        self.cursor = start;
                    } else {
                        self.cursor = self.cursor.saturating_sub(1);
                    }
                }
                KeyCode::Right => {
                    if let Some((_, end)) = placeholder_at_cursor(&self.input, self.cursor) {
                        self.cursor = end;
                    } else {
                        self.cursor = (self.cursor + 1).min(self.input.chars().count());
                    }
                }
                KeyCode::Home => self.cursor = 0,
                KeyCode::End => self.cursor = self.input.chars().count(),
                KeyCode::Up => {
                    if !self.history.is_empty()
                        && repl_should_browse_history(
                            &self.input,
                            &self.history,
                            self.history_clean_index,
                        )
                    {
                        if self.input.is_empty() {
                            self.history_index = self.history.len();
                        }
                        self.history_index = self.history_index.saturating_sub(1);
                        self.recall_history_entry(self.history_index);
                    } else {
                        self.cursor = repl_move_cursor_vertical("  ", &self.input, self.cursor, -1);
                    }
                }
                KeyCode::Down => {
                    if repl_history_is_clean(&self.input, &self.history, self.history_clean_index) {
                        if self.history_index + 1 < self.history.len() {
                            self.history_index += 1;
                            self.recall_history_entry(self.history_index);
                        } else {
                            self.history_index = self.history.len();
                            self.input.clear();
                            self.cursor = 0;
                            self.history_clean_index = None;
                            self.raw_pasted_lines = 0;
                            self.pasted_images.clear();
                            self.pasted_texts.clear();
                        }
                    } else {
                        self.cursor = repl_move_cursor_vertical("  ", &self.input, self.cursor, 1);
                    }
                }
                KeyCode::Enter if modifiers.contains(KeyModifiers::SHIFT) => {
                    // Shift+Enter 与 Ctrl+J 相同：在光标处插入换行，不提交
                    insert_newline_at_cursor(&mut self.input, &mut self.cursor);
                    self.history_clean_index = None;
                    self.raw_pasted_lines = 0;
                }
                KeyCode::Enter => {
                    return Ok(self
                        .submit()
                        .map(LiveEditorAction::Submit)
                        .unwrap_or(LiveEditorAction::EmptySubmit));
                }
                KeyCode::Char('j') if modifiers.contains(KeyModifiers::CONTROL) => {
                    insert_newline_at_cursor(&mut self.input, &mut self.cursor);
                    self.history_clean_index = None;
                    self.raw_pasted_lines = 0;
                }
                KeyCode::Char('c')
                    if modifiers.contains(KeyModifiers::CONTROL)
                        && !modifiers.contains(KeyModifiers::SHIFT) =>
                {
                    if self.input.is_empty() {
                        return Ok(LiveEditorAction::Interrupt);
                    }
                    self.clear();
                }
                KeyCode::Char('d')
                    if modifiers.contains(KeyModifiers::CONTROL) && self.input.is_empty() =>
                {
                    return Ok(LiveEditorAction::Exit);
                }
                KeyCode::Char('w') if modifiers.contains(KeyModifiers::CONTROL) => {
                    remove_word_before_cursor(
                        &mut self.input,
                        &mut self.cursor,
                        &mut self.pasted_images,
                        &mut self.pasted_texts,
                    );
                    self.history_clean_index = None;
                    self.raw_pasted_lines = 0;
                }
                KeyCode::Backspace => {
                    if self.cursor > 0 {
                        if let Some((start, end)) =
                            placeholder_before_or_at_cursor(&self.input, self.cursor)
                        {
                            clear_placeholder_payload(
                                &self.input,
                                start,
                                end,
                                &mut self.pasted_images,
                                &mut self.pasted_texts,
                            );
                            remove_range_chars(&mut self.input, start, end);
                            self.cursor = start;
                        } else {
                            remove_char_before_cursor(&mut self.input, &mut self.cursor);
                        }
                        self.history_clean_index = None;
                    }
                    self.raw_pasted_lines = 0;
                }
                KeyCode::Delete => {
                    if let Some((start, end)) =
                        placeholder_after_or_at_cursor(&self.input, self.cursor)
                    {
                        clear_placeholder_payload(
                            &self.input,
                            start,
                            end,
                            &mut self.pasted_images,
                            &mut self.pasted_texts,
                        );
                        remove_range_chars(&mut self.input, start, end);
                    } else {
                        remove_char_at_cursor(&mut self.input, self.cursor);
                    }
                    self.history_clean_index = None;
                    self.raw_pasted_lines = 0;
                }
                KeyCode::Char('c' | 'C')
                    if modifiers.contains(KeyModifiers::CONTROL)
                        && modifiers.contains(KeyModifiers::SHIFT) =>
                {
                    if let Some(selected) =
                        placeholder_text_near_cursor(&self.input, self.cursor, &self.pasted_texts)
                    {
                        let _ = crate::clipboard::write_clipboard_text(&selected)?;
                    }
                }
                KeyCode::Char('v') if modifiers.contains(KeyModifiers::CONTROL) => {
                    self.paste_clipboard(paths)?;
                }
                KeyCode::Char('l') if modifiers.contains(KeyModifiers::CONTROL) => {
                    return Ok(LiveEditorAction::ClearScreen);
                }
                KeyCode::Char(ch) if !modifiers.contains(KeyModifiers::CONTROL) => {
                    if !is_disallowed_control_char(ch) {
                        if let Some((_, end)) = placeholder_at_cursor(&self.input, self.cursor) {
                            self.cursor = end;
                        }
                        insert_char_at_cursor(&mut self.input, &mut self.cursor, ch);
                        self.history_clean_index = None;
                    }
                    self.raw_pasted_lines = 0;
                }
                _ => return Ok(LiveEditorAction::None),
            },
            _ => return Ok(LiveEditorAction::None),
        }
        Ok(LiveEditorAction::Redraw)
    }

    pub(in crate::cli) fn paste_clipboard(&mut self, paths: &MiyuPaths) -> Result<()> {
        match crate::clipboard::read_clipboard() {
            Ok(crate::clipboard::ClipboardContent::Image(image)) => {
                let index = self.pasted_images.len() + 1;
                // 占位符只认序号,文件名不进输入框(模型侧路径另拼)。
                let _ = image.write_temp_file(&paths.cache_dir, index);
                let placeholder = format!("[Image {index}]");
                insert_str_at_cursor(&mut self.input, &mut self.cursor, &placeholder);
                self.pasted_images
                    .push(Some(crate::clipboard::PastedImage::Binary(image)));
                self.raw_pasted_lines = 0;
            }
            Ok(crate::clipboard::ClipboardContent::MediaPath(path)) => {
                let index = self.pasted_images.len() + 1;
                let label = media_placeholder_label(&path);
                insert_str_at_cursor(
                    &mut self.input,
                    &mut self.cursor,
                    &format!("[{label} {index}]"),
                );
                self.pasted_images
                    .push(Some(crate::clipboard::PastedImage::Path(path)));
                self.raw_pasted_lines = 0;
            }
            Ok(crate::clipboard::ClipboardContent::TextPath(path)) => {
                insert_str_at_cursor(&mut self.input, &mut self.cursor, &path);
                self.raw_pasted_lines = 0;
            }
            _ => {
                if let Ok(Some(text)) = crate::clipboard::read_clipboard_text() {
                    let raw_lines = insert_pasted_text_at_cursor(
                        &mut self.input,
                        &mut self.cursor,
                        text,
                        &mut self.pasted_texts,
                    );
                    self.raw_pasted_lines = self.raw_pasted_lines.saturating_add(raw_lines);
                }
            }
        }
        self.history_clean_index = None;
        Ok(())
    }
}

pub(in crate::cli) fn repl_input_rendered_rows(
    input: &str,
    raw_pasted_lines: usize,
    show_shortcut_hint: bool,
    cols: usize,
) -> u16 {
    let suggestions = repl_command_suggestions(input);
    let lines = repl_input_lines(input);
    let display_lines =
        repl_visible_input_lines("  ", &lines, REPL_MAX_VISIBLE_INPUT_ROWS, raw_pasted_lines);
    let input_rows = repl_wrapped_input_rows_for_cols("  ", &display_lines, cols)
        .len()
        .max(1)
        .min(u16::MAX as usize) as u16;
    input_rows.saturating_add(if show_shortcut_hint && suggestions.is_empty() {
        4
    } else {
        3
    })
}

pub(in crate::cli) fn repl_input_lines(input: &str) -> Vec<String> {
    let normalized = strip_terminal_control_sequences(input)
        .replace("\r\n", "\n")
        .replace('\r', "\n");
    let mut lines = normalized
        .split('\n')
        .map(ToString::to_string)
        .collect::<Vec<_>>();
    if lines.is_empty() {
        lines.push(String::new());
    }
    lines
}

/// 判定一行输入是命令还是聊天。**只认完整命令名**——不命中就是聊天。
///
/// 两条边界都是踩出来的：
///
/// 一、回车不做前缀展开。以前 `/n 什么的` 唯一前缀命中 `/new [name]`，静默建
/// 了个叫「什么的」的会话；`/d 3` 命中 `/delete [name|index]`，静默删掉 3 号
/// 会话。用户想说的只是普通句子，代价却是数据没了。前缀展开留给 Tab
/// （`complete_repl_command`）——那里用户看得见展开结果，能反悔。
///
/// 二、不命中回落聊天而不是报「未知命令」。`/home/shorin/x 这是什么` 是完全
/// 正常的一句话，以前整行被丢弃、输入框也被清空。平台侧早就是这个语义
/// （`platforms/commands.rs`：未注册名返回 `None`，继续当普通聊天），REPL 现在
/// 对齐。代价是打错的命令（`/rest`）会发给模型，可接受——模型会告诉你。
pub(in crate::cli) fn parse_repl_input(input: &str) -> ReplInput<'_> {
    if !input.starts_with('/') {
        return ReplInput::Chat;
    }
    let (name, args) = split_repl_command(input);
    let lowered = name.to_ascii_lowercase();
    match REPL_COMMAND_TABLE.iter().find(|spec| spec.name == lowered) {
        Some(spec) => ReplInput::Slash(spec.command, args),
        None => ReplInput::Chat,
    }
}
