//! 从终端读一次 REPL 输入。
//!
//! 这一层负责把按键事件变成一次提交：读键、分发给编辑器、处理粘贴与括号粘贴
//! 模式、在需要时重绘输入区。它是编辑器（纯状态）与终端（真实字节）之间的桥。

// 输入层还用着一批留在 cli::mod 的辅助，以及编辑器与宽度计算。
use crate::cli::repl::editor::*;
use crate::cli::repl::width::*;
use crate::cli::*;

pub(in crate::cli) fn read_live_repl_input(
    live: &mut LiveReplTail,
    paths: &MiyuPaths,
    jobs_feed: &JobsFeed,
    // 这个 REPL 的会话：唤醒回合按它认领，输入历史也按它刷新。
    repl_session: Option<&str>,
) -> Result<LiveReplOutcome> {
    let _raw_mode = if std::mem::take(&mut live.raw_mode_handoff) {
        LiveRawMode::adopt()
    } else {
        LiveRawMode::start()?
    };
    if !live.rendered {
        synchronized_terminal_update(CursorAfterUpdate::Shown, || live.resume())?;
    }
    let mut last_key_at = Instant::now();
    loop {
        // 等待权自持:PTY 死亡后 crossterm 的 poll 会在内部对 HUP fd
        // 无限自旋、永不返回(实测),所以不能把"等 80ms"交给它——用裸
        // poll 等待并率先识别挂断,有输入就绪时才让 crossterm 取事件。
        let mut pollfd = libc::pollfd {
            fd: libc::STDIN_FILENO,
            events: libc::POLLIN,
            revents: 0,
        };
        let ready = unsafe { libc::poll(&mut pollfd, 1, 80) };
        if ready == 1 && (pollfd.revents & (libc::POLLHUP | libc::POLLERR | libc::POLLNVAL)) != 0 {
            return Ok(LiveReplOutcome::Exit);
        }
        // 就绪判定必须问 crossterm(它的内部缓冲对裸 poll 不可见):
        // 一次 read 会把 fd 里的字节全部吞进内部缓冲,只看 fd 会让
        // 积压按键滞留到下一次按键或超时才放行,打字手感直接变卡。
        let has_input = event::poll(Duration::ZERO)?;
        if !has_input {
            // Idle tick: structural changes redraw the whole tail; otherwise
            // only the strip repaints. While the user is actively typing the
            // animation pauses so the two repaint sources never interleave.
            for report in jobs_feed.take_reports() {
                synchronized_terminal_update(CursorAfterUpdate::Preserve, || {
                    live.show_background_report(&report)
                })?;
            }
            // 听写:识别出的句子填进编辑框(或按配置直接提交)。
            for (event, auto_submit) in crate::cli::repl::dictation::poll() {
                use crate::cli::repl::dictation::DictationEvent;
                match event {
                    DictationEvent::Utterance(text) if auto_submit => {
                        live.editor.input = text;
                        live.editor.cursor = live.editor.input.chars().count();
                        if let Some(submission) = live.editor.submit() {
                            let mode = live.mode();
                            synchronized_terminal_update(CursorAfterUpdate::Shown, || {
                                live.commit_submission(&submission)
                            })?;
                            return Ok(LiveReplOutcome::Submit(
                                mode,
                                submission.content,
                                submission.images,
                            ));
                        }
                    }
                    DictationEvent::Utterance(text) => {
                        if !live.editor.input.is_empty()
                            && !live.editor.input.ends_with(char::is_whitespace)
                        {
                            live.editor.input.push(' ');
                        }
                        live.editor.input.push_str(&text);
                        live.editor.cursor = live.editor.input.chars().count();
                        synchronized_terminal_update(CursorAfterUpdate::Preserve, || {
                            live.redraw()
                        })?;
                    }
                    DictationEvent::Ended => {
                        repl_note(
                            live,
                            &format!("\x1b[2m{}\x1b[0m\n", t("dictation ended", "听写结束")),
                        )?;
                    }
                    DictationEvent::Error(message) => {
                        repl_note(
                            live,
                            &format!(
                                "\x1b[31m{}: {message}\x1b[0m\n",
                                t("dictation failed", "听写失败")
                            ),
                        )?;
                    }
                }
            }
            let typing = last_key_at.elapsed() < Duration::from_millis(350);
            if typing {
                continue;
            }
            if let Some(session) = repl_session {
                if let Some((run_id, label)) = jobs_feed.claim_wake_run(session) {
                    return Ok(LiveReplOutcome::FollowWake { run_id, label });
                }
            }
            let cumulative_changed = jobs_feed
                .cumulative()
                .is_some_and(|totals| live.footer.update_cumulative_tokens(totals));
            if live.set_jobs(jobs_feed.current()) || cumulative_changed {
                synchronized_terminal_update(CursorAfterUpdate::Preserve, || live.redraw())?;
            } else {
                live.tick_job_strip()?;
            }
            continue;
        }
        // 抽干本轮就绪的全部事件再回到等待,粘贴/快速输入不积压。
        while event::poll(Duration::ZERO)? {
            // read 前再验挂断:HUP 的 fd 会让 poll 报就绪却读不出事件,
            // 直接 read 就掉进 crossterm 的自旋。
            if terminal_hangup() {
                return Ok(LiveReplOutcome::Exit);
            }
            last_key_at = Instant::now();
            let event = event::read()?;
            // 听写中 Esc = 停止听写,已听写的文字留在编辑框里(编辑框有内容时
            // 打不出 /stt,所以停止不能靠命令)。
            if crate::cli::repl::dictation::is_active()
                && matches!(
                    &event,
                    Event::Key(KeyEvent {
                        code: KeyCode::Esc,
                        kind,
                        ..
                    }) if *kind != KeyEventKind::Release
                )
            {
                crate::cli::repl::dictation::stop();
                repl_note(
                    live,
                    &format!("\x1b[2m{}\x1b[0m\n", t("dictation stopped", "听写已停止")),
                )?;
                synchronized_terminal_update(CursorAfterUpdate::Preserve, || live.redraw())?;
                continue;
            }
            // 上键开始翻历史之前，先把别的 REPL 刚落盘的输入补进来。历史只在
            // 启动时读一次，两个 REPL 同时开着时先开的那个原本永远看不到后开
            // 的那个敲了什么（见 `refresh_repl_input_history`）。
            //
            // 只在输入框为空、也就是「从头开始翻」时刷：翻到一半重载会让
            // history_index 指错行。
            if live.editor.input.is_empty()
                && matches!(
                    &event,
                    Event::Key(KeyEvent {
                        code: KeyCode::Up,
                        kind,
                        ..
                    }) if *kind != KeyEventKind::Release
                )
            {
                if let Some(session) = repl_session {
                    if refresh_repl_input_history(&mut live.editor.history, paths, session) {
                        live.editor.history_index = live.editor.history.len();
                    }
                }
            }
            match live.editor.handle_event(event, paths, false)? {
                LiveEditorAction::None => {}
                LiveEditorAction::Redraw => {
                    synchronized_terminal_update(CursorAfterUpdate::Preserve, || live.redraw())?
                }
                LiveEditorAction::ClearScreen => {
                    synchronized_terminal_update(CursorAfterUpdate::Preserve, || {
                        live.clear_screen()
                    })?
                }
                LiveEditorAction::EmptySubmit => {
                    synchronized_terminal_update(CursorAfterUpdate::Preserve, || {
                        live.commit_empty_submission()
                    })?
                }
                LiveEditorAction::Submit(submission) => {
                    // 回车发送即结束听写,不让麦克风继续往下一条消息里灌字。
                    crate::cli::repl::dictation::stop();
                    // `/goal edit`（无参数）在提交前原地变身成可编辑的
                    // 「/goal edit <当前目标>」，不回显、不产生任何输出。
                    if submission.content.trim() == "/goal edit"
                        && crate::cli::repl::session::prefill_goal_edit_input(
                            paths,
                            repl_session,
                            live,
                        )
                    {
                        synchronized_terminal_update(CursorAfterUpdate::Preserve, || {
                            live.redraw()
                        })?;
                        continue;
                    }
                    let mode = live.mode();
                    // 回显和活动区重画在一个同步块里完成:光标不在左下角
                    // 落脚,kitty 的 cursor_trail 就没有东西可画(见 commit_submission)。
                    synchronized_terminal_update(CursorAfterUpdate::Shown, || {
                        live.commit_submission(&submission)
                    })?;
                    return Ok(LiveReplOutcome::Submit(
                        mode,
                        submission.content,
                        submission.images,
                    ));
                }
                // Ctrl+C rung 3: the draft was empty and no reply is running, but
                // this session still has background work — stop that before the
                // press is allowed to mean "quit". `live.jobs` holds only running
                // jobs of this session, refreshed on every idle tick. Ctrl+D
                // (`Exit`) always quits outright.
                LiveEditorAction::Interrupt if !live.jobs.is_empty() => {
                    return Ok(LiveReplOutcome::StopJobs);
                }
                LiveEditorAction::Interrupt | LiveEditorAction::Exit => {
                    synchronized_terminal_update(CursorAfterUpdate::Hidden, || live.suspend())?;
                    return Ok(LiveReplOutcome::Exit);
                }
            }
        }
    }
}

pub(in crate::cli) fn read_repl_input(
    paths: &MiyuPaths,
    mode: AgentMode,
    prefill: Option<String>,
    history: &[String],
    footer: &ReplFooterStatus,
    show_shortcut_hint: bool,
) -> Result<
    Option<(
        AgentMode,
        String,
        Vec<Option<crate::clipboard::PastedImage>>,
    )>,
> {
    let mut stdout = io::stdout();
    let mut input = strip_terminal_control_sequences(&prefill.unwrap_or_default());
    let mut cursor = input.chars().count();
    let mut history_index = history.len();
    let mut history_clean_index: Option<usize> = None;
    let plain_prefix = "  ";
    let cursor_col = cursor_col_or(0);
    if cursor_col != 0 {
        writeln!(stdout)?;
        stdout.flush()?;
    }
    terminal::enable_raw_mode()?;
    spawn_hangup_watchdog();
    execute!(stdout, EnableBracketedPaste)?;
    let mut keyboard_enhancement = KeyboardEnhancementState::enable(&mut stdout);
    let mut input_row = cursor_row_or(0);
    let mut rendered_rows = 0u16;
    let mut raw_pasted_lines = 0usize;
    let mut pasted_images: Vec<Option<crate::clipboard::PastedImage>> = Vec::new();
    let mut pasted_texts: Vec<Option<PastedText>> = Vec::new();
    // 1. 局部退出时统一恢复终端协议
    // 2. 避免多处 return 漏 Pop 键盘增强
    let restore_terminal = |stdout: &mut io::Stdout,
                            keyboard_enhancement: &mut KeyboardEnhancementState|
     -> Result<()> {
        execute!(stdout, DisableBracketedPaste)?;
        keyboard_enhancement.disable(stdout);
        terminal::disable_raw_mode()?;
        Ok(())
    };
    let render_repl_input = |stdout: &mut io::Stdout,
                             input_row: &mut u16,
                             rendered_rows: &mut u16,
                             mode: AgentMode,
                             input: &str,
                             cursor: usize,
                             raw_pasted_lines: usize| {
        render_repl_input_with_footer(
            stdout,
            input_row,
            rendered_rows,
            mode,
            input,
            cursor,
            raw_pasted_lines,
            footer,
            show_shortcut_hint,
        )
    };
    render_repl_input(
        &mut stdout,
        &mut input_row,
        &mut rendered_rows,
        mode,
        &input,
        cursor,
        raw_pasted_lines,
    )?;
    loop {
        match event::read()? {
            Event::Paste(text) => {
                let raw_lines =
                    insert_pasted_text_at_cursor(&mut input, &mut cursor, text, &mut pasted_texts);
                history_clean_index = None;
                raw_pasted_lines = raw_pasted_lines.saturating_add(raw_lines);
                render_repl_input(
                    &mut stdout,
                    &mut input_row,
                    &mut rendered_rows,
                    mode,
                    &input,
                    cursor,
                    raw_pasted_lines,
                )?;
            }
            Event::Key(KeyEvent {
                code, modifiers, ..
            }) => match code {
                KeyCode::Tab => {
                    if input.starts_with('/') {
                        if let Some(completed) = complete_repl_command(&input) {
                            input = completed.to_string();
                            cursor = input.chars().count();
                            history_clean_index = None;
                        }
                    } else {
                        // 会话模式创建时定死:Tab 切换已随闲聊模式一并删除。
                    }
                    raw_pasted_lines = 0;
                    render_repl_input(
                        &mut stdout,
                        &mut input_row,
                        &mut rendered_rows,
                        mode,
                        &input,
                        cursor,
                        raw_pasted_lines,
                    )?;
                }
                KeyCode::Esc => {
                    input.clear();
                    cursor = 0;
                    history_clean_index = None;
                    raw_pasted_lines = 0;
                    pasted_images.clear();
                    pasted_texts.clear();
                    render_repl_input(
                        &mut stdout,
                        &mut input_row,
                        &mut rendered_rows,
                        mode,
                        &input,
                        cursor,
                        raw_pasted_lines,
                    )?;
                }
                KeyCode::Left => {
                    if let Some((start, _)) = placeholder_at_cursor(&input, cursor) {
                        cursor = start;
                    } else {
                        cursor = cursor.saturating_sub(1);
                    }
                    render_repl_input(
                        &mut stdout,
                        &mut input_row,
                        &mut rendered_rows,
                        mode,
                        &input,
                        cursor,
                        raw_pasted_lines,
                    )?;
                }
                KeyCode::Right => {
                    if let Some((_, end)) = placeholder_at_cursor(&input, cursor) {
                        cursor = end;
                    } else {
                        cursor = (cursor + 1).min(input.chars().count());
                    }
                    render_repl_input(
                        &mut stdout,
                        &mut input_row,
                        &mut rendered_rows,
                        mode,
                        &input,
                        cursor,
                        raw_pasted_lines,
                    )?;
                }
                KeyCode::Home => {
                    cursor = 0;
                    render_repl_input(
                        &mut stdout,
                        &mut input_row,
                        &mut rendered_rows,
                        mode,
                        &input,
                        cursor,
                        raw_pasted_lines,
                    )?;
                }
                KeyCode::End => {
                    cursor = input.chars().count();
                    render_repl_input(
                        &mut stdout,
                        &mut input_row,
                        &mut rendered_rows,
                        mode,
                        &input,
                        cursor,
                        raw_pasted_lines,
                    )?;
                }
                KeyCode::Up => {
                    if !history.is_empty()
                        && repl_should_browse_history(&input, history, history_clean_index)
                    {
                        if input.is_empty() {
                            history_index = history.len();
                        }
                        history_index = history_index.saturating_sub(1);
                        input = history.get(history_index).cloned().unwrap_or_default();
                        cursor = input.chars().count();
                        history_clean_index = Some(history_index);
                        raw_pasted_lines = 0;
                        pasted_images.clear();
                        pasted_texts.clear();
                    } else {
                        cursor = repl_move_cursor_vertical(&plain_prefix, &input, cursor, -1);
                    }
                    render_repl_input(
                        &mut stdout,
                        &mut input_row,
                        &mut rendered_rows,
                        mode,
                        &input,
                        cursor,
                        raw_pasted_lines,
                    )?;
                }
                KeyCode::Down => {
                    if repl_history_is_clean(&input, history, history_clean_index) {
                        if history_index + 1 < history.len() {
                            history_index += 1;
                            input = history.get(history_index).cloned().unwrap_or_default();
                            cursor = input.chars().count();
                            history_clean_index = Some(history_index);
                        } else {
                            history_index = history.len();
                            input.clear();
                            cursor = 0;
                            history_clean_index = None;
                        }
                        raw_pasted_lines = 0;
                        pasted_images.clear();
                        pasted_texts.clear();
                    } else {
                        cursor = repl_move_cursor_vertical(&plain_prefix, &input, cursor, 1);
                    }
                    render_repl_input(
                        &mut stdout,
                        &mut input_row,
                        &mut rendered_rows,
                        mode,
                        &input,
                        cursor,
                        raw_pasted_lines,
                    )?;
                }
                KeyCode::Enter if modifiers.contains(KeyModifiers::SHIFT) => {
                    // Shift+Enter 与 Ctrl+J 相同：在光标处插入换行，不提交
                    insert_newline_at_cursor(&mut input, &mut cursor);
                    history_clean_index = None;
                    raw_pasted_lines = 0;
                    render_repl_input(
                        &mut stdout,
                        &mut input_row,
                        &mut rendered_rows,
                        mode,
                        &input,
                        cursor,
                        raw_pasted_lines,
                    )?;
                }
                KeyCode::Enter => {
                    let submitted_echo = strip_terminal_control_sequences(&input);
                    input = expand_pasted_text_placeholders(&submitted_echo, &pasted_texts);
                    replace_repl_input_with_user_echo(
                        &mut stdout,
                        input_row,
                        rendered_rows,
                        mode,
                        &submitted_echo,
                    )?;
                    restore_terminal(&mut stdout, &mut keyboard_enhancement)?;
                    return Ok(Some((mode, input, pasted_images)));
                }
                KeyCode::Char('j') if modifiers.contains(KeyModifiers::CONTROL) => {
                    insert_newline_at_cursor(&mut input, &mut cursor);
                    history_clean_index = None;
                    raw_pasted_lines = 0;
                    render_repl_input(
                        &mut stdout,
                        &mut input_row,
                        &mut rendered_rows,
                        mode,
                        &input,
                        cursor,
                        raw_pasted_lines,
                    )?;
                }
                KeyCode::Char('c')
                    if modifiers.contains(KeyModifiers::CONTROL)
                        && !modifiers.contains(KeyModifiers::SHIFT) =>
                {
                    if !input.is_empty() {
                        input.clear();
                        cursor = 0;
                        history_clean_index = None;
                        raw_pasted_lines = 0;
                        pasted_images.clear();
                        pasted_texts.clear();
                        render_repl_input(
                            &mut stdout,
                            &mut input_row,
                            &mut rendered_rows,
                            mode,
                            &input,
                            cursor,
                            raw_pasted_lines,
                        )?;
                        continue;
                    }
                    move_after_repl_input(&mut stdout, input_row, rendered_rows)?;
                    restore_terminal(&mut stdout, &mut keyboard_enhancement)?;
                    return Ok(None);
                }
                KeyCode::Char('d')
                    if modifiers.contains(KeyModifiers::CONTROL) && input.is_empty() =>
                {
                    move_after_repl_input(&mut stdout, input_row, rendered_rows)?;
                    restore_terminal(&mut stdout, &mut keyboard_enhancement)?;
                    return Ok(None);
                }
                KeyCode::Char('l') if modifiers.contains(KeyModifiers::CONTROL) => {
                    queue!(stdout, Clear(ClearType::All), MoveTo(0, 0))?;
                    stdout.flush()?;
                    input_row = 0;
                    rendered_rows = 0;
                    render_repl_input(
                        &mut stdout,
                        &mut input_row,
                        &mut rendered_rows,
                        mode,
                        &input,
                        cursor,
                        raw_pasted_lines,
                    )?;
                }
                KeyCode::Char('w') if modifiers.contains(KeyModifiers::CONTROL) => {
                    remove_word_before_cursor(
                        &mut input,
                        &mut cursor,
                        &mut pasted_images,
                        &mut pasted_texts,
                    );
                    history_clean_index = None;
                    raw_pasted_lines = 0;
                    render_repl_input(
                        &mut stdout,
                        &mut input_row,
                        &mut rendered_rows,
                        mode,
                        &input,
                        cursor,
                        raw_pasted_lines,
                    )?;
                }
                KeyCode::Backspace => {
                    if cursor > 0 {
                        if let Some((start, end)) = placeholder_before_or_at_cursor(&input, cursor)
                        {
                            clear_placeholder_payload(
                                &input,
                                start,
                                end,
                                &mut pasted_images,
                                &mut pasted_texts,
                            );
                            remove_range_chars(&mut input, start, end);
                            cursor = start;
                        } else {
                            remove_char_before_cursor(&mut input, &mut cursor);
                        }
                        history_clean_index = None;
                    }
                    raw_pasted_lines = 0;
                    render_repl_input(
                        &mut stdout,
                        &mut input_row,
                        &mut rendered_rows,
                        mode,
                        &input,
                        cursor,
                        raw_pasted_lines,
                    )?;
                }
                KeyCode::Delete => {
                    if let Some((start, end)) = placeholder_after_or_at_cursor(&input, cursor) {
                        clear_placeholder_payload(
                            &input,
                            start,
                            end,
                            &mut pasted_images,
                            &mut pasted_texts,
                        );
                        remove_range_chars(&mut input, start, end);
                    } else {
                        remove_char_at_cursor(&mut input, cursor);
                    }
                    history_clean_index = None;
                    raw_pasted_lines = 0;
                    render_repl_input(
                        &mut stdout,
                        &mut input_row,
                        &mut rendered_rows,
                        mode,
                        &input,
                        cursor,
                        raw_pasted_lines,
                    )?;
                }
                KeyCode::Char('c' | 'C')
                    if modifiers.contains(KeyModifiers::CONTROL)
                        && modifiers.contains(KeyModifiers::SHIFT) =>
                {
                    if let Some(selected) =
                        placeholder_text_near_cursor(&input, cursor, &pasted_texts)
                    {
                        let _ = crate::clipboard::write_clipboard_text(&selected)?;
                    }
                }
                KeyCode::Char('v') if modifiers.contains(KeyModifiers::CONTROL) => {
                    match crate::clipboard::read_clipboard() {
                        Ok(crate::clipboard::ClipboardContent::Image(img)) => {
                            let index = pasted_images.len() + 1;
                            // 占位符只认序号,文件名纯属显示噪音(模型侧路径
                            // 由 rewrite_image_placeholders_with_paths 另拼)。
                            let _ = img.write_temp_file(&paths.cache_dir, index);
                            let placeholder = format!("[Image {}]", index);
                            insert_str_at_cursor(&mut input, &mut cursor, &placeholder);
                            history_clean_index = None;
                            pasted_images.push(Some(crate::clipboard::PastedImage::Binary(img)));
                            raw_pasted_lines = 0;
                            render_repl_input(
                                &mut stdout,
                                &mut input_row,
                                &mut rendered_rows,
                                mode,
                                &input,
                                cursor,
                                raw_pasted_lines,
                            )?;
                        }
                        Ok(crate::clipboard::ClipboardContent::MediaPath(path)) => {
                            let index = pasted_images.len() + 1;
                            let label = media_placeholder_label(&path);
                            let placeholder = format!("[{label} {index}]");
                            insert_str_at_cursor(&mut input, &mut cursor, &placeholder);
                            history_clean_index = None;
                            pasted_images.push(Some(crate::clipboard::PastedImage::Path(path)));
                            raw_pasted_lines = 0;
                            render_repl_input(
                                &mut stdout,
                                &mut input_row,
                                &mut rendered_rows,
                                mode,
                                &input,
                                cursor,
                                raw_pasted_lines,
                            )?;
                        }
                        Ok(crate::clipboard::ClipboardContent::TextPath(path)) => {
                            insert_str_at_cursor(&mut input, &mut cursor, &path);
                            history_clean_index = None;
                            raw_pasted_lines = 0;
                            render_repl_input(
                                &mut stdout,
                                &mut input_row,
                                &mut rendered_rows,
                                mode,
                                &input,
                                cursor,
                                raw_pasted_lines,
                            )?;
                        }
                        _ => {
                            if let Ok(Some(text)) = crate::clipboard::read_clipboard_text() {
                                let raw_lines = insert_pasted_text_at_cursor(
                                    &mut input,
                                    &mut cursor,
                                    text,
                                    &mut pasted_texts,
                                );
                                history_clean_index = None;
                                raw_pasted_lines = raw_pasted_lines.saturating_add(raw_lines);
                                render_repl_input(
                                    &mut stdout,
                                    &mut input_row,
                                    &mut rendered_rows,
                                    mode,
                                    &input,
                                    cursor,
                                    raw_pasted_lines,
                                )?;
                            }
                        }
                    }
                }
                KeyCode::Char(ch) if !modifiers.contains(KeyModifiers::CONTROL) => {
                    if !is_disallowed_control_char(ch) {
                        if let Some((_, end)) = placeholder_at_cursor(&input, cursor) {
                            cursor = end;
                        }
                        insert_char_at_cursor(&mut input, &mut cursor, ch);
                        history_clean_index = None;
                    }
                    raw_pasted_lines = 0;
                    render_repl_input(
                        &mut stdout,
                        &mut input_row,
                        &mut rendered_rows,
                        mode,
                        &input,
                        cursor,
                        raw_pasted_lines,
                    )?;
                }
                _ => {}
            },
            _ => {}
        }
    }
}

pub(in crate::cli) fn render_repl_input_with_footer(
    stdout: &mut io::Stdout,
    input_row: &mut u16,
    rendered_rows: &mut u16,
    mode: AgentMode,
    input: &str,
    cursor: usize,
    raw_pasted_lines: usize,
    footer: &ReplFooterStatus,
    show_shortcut_hint: bool,
) -> Result<Option<u16>> {
    let suggestions = repl_command_suggestions(input);
    let lines = repl_input_lines(input);
    let prompt_prefix = input_prompt_bar(mode);
    let plain_prefix = "  ";
    let cols = terminal_cols();
    let display_lines = repl_visible_input_lines(
        &plain_prefix,
        &lines,
        REPL_MAX_VISIBLE_INPUT_ROWS,
        raw_pasted_lines,
    );
    let display_rows = repl_wrapped_input_rows_for_cols(&plain_prefix, &display_lines, cols);
    let display_rows: Vec<String> = display_rows
        .iter()
        .map(|line| colorize_repl_placeholders(line))
        .collect();
    let input_rows = display_rows.len().max(1).min(u16::MAX as usize) as u16;
    let show_hint = show_shortcut_hint && suggestions.is_empty();
    let current_rows = input_rows.saturating_add(if show_hint { 4 } else { 3 });
    let rows_to_clear = (*rendered_rows).max(current_rows).max(1);
    ensure_repl_space(stdout, input_row, rows_to_clear)?;
    for row_offset in 0..rows_to_clear {
        queue!(
            stdout,
            MoveTo(0, (*input_row).saturating_add(row_offset)),
            Clear(ClearType::CurrentLine)
        )?;
    }
    let mut row_offset = 0u16;
    let footer_row;
    queue!(stdout, MoveTo(0, *input_row), Print(&prompt_prefix))?;
    row_offset = row_offset.saturating_add(1);
    for line in &display_rows {
        let row = (*input_row).saturating_add(row_offset);
        queue!(stdout, MoveTo(0, row))?;
        queue!(stdout, Print(&prompt_prefix), Print(line))?;
        row_offset = row_offset.saturating_add(1);
    }
    queue!(
        stdout,
        MoveTo(0, (*input_row).saturating_add(row_offset)),
        Print(&prompt_prefix)
    )?;
    row_offset = row_offset.saturating_add(1);
    if !suggestions.is_empty() {
        let suggestion_width = cols.saturating_sub(visible_width(&prompt_prefix)).max(1);
        queue!(
            stdout,
            MoveTo(0, (*input_row).saturating_add(row_offset)),
            Print(&prompt_prefix),
            Print(format!(
                "\x1b[2m{}\x1b[0m",
                repl_command_suggestions_line(&suggestions, suggestion_width)
            ))
        )?;
        footer_row = None;
    } else {
        footer_row = Some((*input_row).saturating_add(row_offset));
        queue!(
            stdout,
            MoveTo(0, (*input_row).saturating_add(row_offset)),
            Print(repl_footer_line(mode, footer, cols))
        )?;
        if show_hint {
            row_offset = row_offset.saturating_add(1);
            queue!(
                stdout,
                MoveTo(0, (*input_row).saturating_add(row_offset)),
                Print(repl_shortcut_hint_line(mode, cols))
            )?;
        }
    }
    let (cursor_col, cursor_row_offset) = if display_lines.len() == lines.len() {
        repl_cursor_position(&plain_prefix, input, cursor)
    } else {
        let last_line = display_lines.last().map(String::as_str).unwrap_or_default();
        let (col, _) = repl_cursor_position_for_line_for_cols(
            &plain_prefix,
            last_line,
            last_line.chars().count(),
            terminal_cols(),
        );
        (
            col,
            repl_prompt_rows(&plain_prefix, &display_lines).saturating_sub(1),
        )
    };
    queue!(
        stdout,
        MoveTo(
            cursor_col,
            (*input_row)
                .saturating_add(1)
                .saturating_add(cursor_row_offset)
        )
    )?;
    stdout.flush()?;
    *rendered_rows = current_rows;
    Ok(footer_row)
}

pub(in crate::cli) fn move_after_repl_input(
    stdout: &mut io::Stdout,
    input_row: u16,
    rendered_rows: u16,
) -> Result<()> {
    queue!(
        stdout,
        MoveTo(0, input_row.saturating_add(rendered_rows.max(1)))
    )?;
    stdout.flush()?;
    Ok(())
}

pub(in crate::cli) fn replace_repl_input_with_user_echo(
    stdout: &mut io::Stdout,
    input_row: u16,
    rendered_rows: u16,
    mode: AgentMode,
    input: &str,
) -> Result<()> {
    let cols = terminal_cols();
    let echo_lines = submitted_echo_lines(mode, input.trim_end(), cols);
    let echo_rows = echo_lines.len().min(u16::MAX as usize) as u16;
    let rows_to_clear = rendered_rows.max(echo_rows).max(1);
    for row_offset in 0..rows_to_clear {
        queue!(
            stdout,
            MoveTo(0, input_row.saturating_add(row_offset)),
            Clear(ClearType::CurrentLine)
        )?;
    }
    for (offset, line) in echo_lines.iter().enumerate() {
        queue!(
            stdout,
            MoveTo(
                0,
                input_row.saturating_add(offset.min(u16::MAX as usize) as u16)
            ),
            Print(line)
        )?;
    }
    queue!(
        stdout,
        MoveTo(0, input_row.saturating_add(echo_rows).saturating_add(1))
    )?;
    stdout.flush()?;
    Ok(())
}
