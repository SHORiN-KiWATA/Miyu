//! 输入编辑：光标、换行、粘贴、历史。

// 被测的东西散在 cli::mod 与 repl 的兄弟模块里，这里全都要够到。
use super::shared::*;
use crate::cli::repl::editor::*;
use crate::cli::*;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

/// 回归：daemon 模式下终端图片印成满屏。
///
/// 真正画图的是终端这一侧，而尺寸此前只给表情包工具算，别的工具一律
/// 传 None——`parse_size(None)` 的语义正是「铺满整个终端」。
#[test]
fn every_tool_image_gets_a_size_not_just_memes() {
    let config = crate::config::AppConfig::default();
    // 没有一个工具可以拿着 None 去调 print_image_file。
    for name in [
        "generate_image",
        "print_image",
        "search_web_images",
        "use_meme",
        "",
    ] {
        assert!(
            remote_tool_image_size(name, "", &config).is_some(),
            "{name} 没拿到尺寸，会印成满屏"
        );
    }
    // 模型显式要的尺寸优先，且不被百分比覆盖。
    assert_eq!(
        remote_tool_image_size("print_image", "40x12", &config),
        Some("40x12".to_string())
    );
    // 空白不算「要了」，仍走配置百分比。
    assert_ne!(
        remote_tool_image_size("print_image", "   ", &config),
        Some("   ".to_string())
    );
}

/// 回归(PR#31):会话内历史无上限,常开 REPL 线性增长。
#[test]
fn repl_history_is_capped() {
    let mut history = Vec::new();
    for i in 0..(REPL_HISTORY_LIMIT + 100) {
        push_history_capped(&mut history, &format!("entry-{i}"));
    }
    assert_eq!(history.len(), REPL_HISTORY_LIMIT);
    assert_eq!(history.first().map(String::as_str), Some("entry-100"));
    assert_eq!(history.last().map(String::as_str), Some("entry-599"));
}

#[test]
fn models_is_the_cli_model_selector() {
    let matches = localized_command()
        .try_get_matches_from(["miyu", "models", "1"])
        .unwrap();
    let cli = Cli::from_arg_matches(&matches).unwrap();

    assert!(matches!(
        cli.command,
        Some(Command::Models(ModelsArgs { target: Some(ref target), global: false })) if target == "1"
    ));
    let old_matches = localized_command()
        .try_get_matches_from(["miyu", "providers"])
        .unwrap();
    let old_cli = Cli::from_arg_matches(&old_matches).unwrap();
    assert!(old_cli.command.is_none());
    assert_eq!(old_cli.message, ["providers"]);
}

#[tokio::test]
async fn one_shot_turns_default_to_a_throwaway_session() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let paths = MiyuPaths {
        root_dir: root.to_path_buf(),
        config_dir: root.join("config"),
        config_file: root.join("config/config.jsonc"),
        skills_dir: root.join("config/skills"),
        data_dir: root.join("data"),
        cache_dir: root.join("cache"),
        state_dir: root.join("state"),
        pictures_dir: root.join("pictures"),
        fish_hook_file: root.join("fish"),
        bash_hook_file: root.join("bash"),
        zsh_hook_file: root.join("zsh"),
        scripts_dir: root.join("scripts"),
        system_scripts_dir: root.join("system-scripts"),
    };

    // Neither flag: `miyu ask` / `miyu '<message>'` must not touch a real
    // conversation. `--continue` opts back into the terminal session.
    // Both resolve without contacting the daemon.
    assert_eq!(
        one_shot_session(&paths, None, false).await.unwrap(),
        TurnSession::Ephemeral
    );
    assert_eq!(
        one_shot_session(&paths, None, true).await.unwrap(),
        TurnSession::Current
    );
}

#[tokio::test]
async fn config_reload_retries_busy_responses_until_success() {
    let attempts = Arc::new(AtomicUsize::new(0));
    let observed = attempts.clone();

    retry_config_reload(4, Duration::ZERO, move || {
        let attempts = attempts.clone();
        async move {
            let attempt = attempts.fetch_add(1, Ordering::SeqCst) + 1;
            Ok(if attempt < 3 {
                ConfigReloadResponse::Busy
            } else {
                ConfigReloadResponse::Reloaded
            })
        }
    })
    .await
    .unwrap();

    assert_eq!(observed.load(Ordering::SeqCst), 3);
}

#[tokio::test]
async fn config_reload_stops_after_the_attempt_limit() {
    let attempts = Arc::new(AtomicUsize::new(0));
    let observed = attempts.clone();

    let error = retry_config_reload(3, Duration::ZERO, move || {
        let attempts = attempts.clone();
        async move {
            attempts.fetch_add(1, Ordering::SeqCst);
            Ok(ConfigReloadResponse::Busy)
        }
    })
    .await
    .unwrap_err();

    assert_eq!(observed.load(Ordering::SeqCst), 3);
    assert!(error.to_string().contains('3'));
}

#[tokio::test]
async fn config_reload_retries_coded_busy_frames_over_ipc() {
    let temp = tempfile::tempdir().unwrap();
    let socket = temp.path().join("reload.sock");
    let listener = tokio::net::UnixListener::bind(&socket).unwrap();
    let server = tokio::spawn(async move {
        for attempt in 1..=3 {
            let (mut stream, _) = listener.accept().await.unwrap();
            let request = ipc::receive::<IpcRequest>(&mut stream)
                .await
                .unwrap()
                .unwrap();
            assert!(matches!(request.command, IpcCommand::ReloadConfig));
            let response = if attempt < 3 {
                IpcFrame::coded_error(ipc::ErrorCode::Busy, ipc::ADMIN_BUSY_MESSAGE)
            } else {
                IpcFrame::Ack
            };
            ipc::send(&mut stream, &response).await.unwrap();
        }
    });

    retry_config_reload(4, Duration::ZERO, || {
        request_config_reload_at(&socket, Duration::from_secs(1))
    })
    .await
    .unwrap();
    server.await.unwrap();
}

#[tokio::test]
async fn config_reload_request_times_out_when_daemon_does_not_respond() {
    let temp = tempfile::tempdir().unwrap();
    let socket = temp.path().join("reload-timeout.sock");
    let listener = tokio::net::UnixListener::bind(&socket).unwrap();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let request = ipc::receive::<IpcRequest>(&mut stream)
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(request.command, IpcCommand::ReloadConfig));
        std::future::pending::<()>().await;
    });

    let error = request_config_reload_at(&socket, Duration::from_millis(100))
        .await
        .unwrap_err();
    assert!(error
        .downcast_ref::<tokio::time::error::Elapsed>()
        .is_some());
    server.abort();
    let _ = server.await;
}

/// Ctrl+←/→ 按词跳。以前这两个分支不看修饰键，Ctrl 组合和裸方向键一样只挪
/// 一个字符。
#[test]
fn ctrl_arrows_jump_by_word() {
    let temp = tempfile::tempdir().unwrap();
    let paths = pop_test_paths(temp.path());
    let ctrl_left = || Event::Key(KeyEvent::new(KeyCode::Left, KeyModifiers::CONTROL));
    let ctrl_right = || Event::Key(KeyEvent::new(KeyCode::Right, KeyModifiers::CONTROL));
    let mut editor = LiveReplEditor::new(AgentMode::Normal, Vec::new());

    // ASCII：三个词，从行尾往回跳到每个词首。
    editor.input = "cargo test --all".to_string();
    editor.cursor = editor.input.chars().count();
    editor.handle_event(ctrl_left(), &paths, false).unwrap();
    assert_eq!(editor.cursor, 11, "应停在 --all 的词首");
    editor.handle_event(ctrl_left(), &paths, false).unwrap();
    assert_eq!(editor.cursor, 6, "应停在 test 的词首");
    editor.handle_event(ctrl_left(), &paths, false).unwrap();
    assert_eq!(editor.cursor, 0);
    // 到行首后再按不越界。
    editor.handle_event(ctrl_left(), &paths, false).unwrap();
    assert_eq!(editor.cursor, 0);

    // 往右跳到每个词尾，到行尾后不越界。
    editor.handle_event(ctrl_right(), &paths, false).unwrap();
    assert_eq!(editor.cursor, 5);
    editor.handle_event(ctrl_right(), &paths, false).unwrap();
    assert_eq!(editor.cursor, 10);
    editor.handle_event(ctrl_right(), &paths, false).unwrap();
    assert_eq!(editor.cursor, 16);
    editor.handle_event(ctrl_right(), &paths, false).unwrap();
    assert_eq!(editor.cursor, 16);

    // 连续空格算一段：跨过去，不在中间停。
    editor.input = "a    b".to_string();
    editor.cursor = 6;
    editor.handle_event(ctrl_left(), &paths, false).unwrap();
    assert_eq!(editor.cursor, 5);
    editor.handle_event(ctrl_left(), &paths, false).unwrap();
    assert_eq!(editor.cursor, 0);

    // 中英混排：CJK 没有空格，整段是一个词。
    editor.input = "查一下 tokio 的文档".to_string();
    editor.cursor = editor.input.chars().count();
    editor.handle_event(ctrl_left(), &paths, false).unwrap();
    assert_eq!(editor.cursor, 10, "应停在「的文档」词首");
    editor.handle_event(ctrl_left(), &paths, false).unwrap();
    assert_eq!(editor.cursor, 4, "应停在 tokio 词首");
    editor.handle_event(ctrl_left(), &paths, false).unwrap();
    assert_eq!(editor.cursor, 0);
}

/// 占位符自带空格，按空白分词一定会切进它中段。跳词必须整块跨过去——
/// 光标落进占位符内部，后续删除/渲染都会拿到半截文本。
#[test]
fn ctrl_arrows_treat_placeholders_as_atomic() {
    let temp = tempfile::tempdir().unwrap();
    let paths = pop_test_paths(temp.path());
    let ctrl_left = || Event::Key(KeyEvent::new(KeyCode::Left, KeyModifiers::CONTROL));
    let ctrl_right = || Event::Key(KeyEvent::new(KeyCode::Right, KeyModifiers::CONTROL));
    let mut editor = LiveReplEditor::new(AgentMode::Normal, Vec::new());

    let input = "看 [Pasted 1: ~3 lines] 这段";
    let placeholders = find_repl_placeholders(input);
    assert_eq!(placeholders.len(), 1, "样例里应当只有一个占位符");
    let (start, end) = placeholders[0];

    editor.input = input.to_string();
    editor.cursor = editor.input.chars().count();
    editor.handle_event(ctrl_left(), &paths, false).unwrap();
    assert!(
        editor.cursor <= start || editor.cursor >= end,
        "光标 {} 落进了占位符 {start}..{end} 内部",
        editor.cursor
    );
    editor.handle_event(ctrl_left(), &paths, false).unwrap();
    assert!(
        editor.cursor <= start || editor.cursor >= end,
        "光标 {} 落进了占位符 {start}..{end} 内部",
        editor.cursor
    );

    editor.cursor = 0;
    for _ in 0..4 {
        editor.handle_event(ctrl_right(), &paths, false).unwrap();
        assert!(
            editor.cursor <= start || editor.cursor >= end,
            "光标 {} 落进了占位符 {start}..{end} 内部",
            editor.cursor
        );
    }
}

#[test]
fn live_editor_restores_clear_screen_and_double_escape_controls() {
    let temp = tempfile::tempdir().unwrap();
    let paths = pop_test_paths(temp.path());
    let escape = || Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    let mut editor = LiveReplEditor::new(AgentMode::Normal, Vec::new());
    editor.input = "draft".to_string();
    assert!(matches!(
        editor.handle_event(escape(), &paths, true).unwrap(),
        LiveEditorAction::Redraw
    ));
    // Arming the interrupt must not clear the typed draft.
    assert_eq!(editor.input, "draft");
    assert!(matches!(
        editor.handle_event(escape(), &paths, true).unwrap(),
        LiveEditorAction::Interrupt
    ));
    assert_eq!(editor.input, "draft");

    let clear = Event::Key(KeyEvent::new(KeyCode::Char('l'), KeyModifiers::CONTROL));
    assert!(matches!(
        editor.handle_event(clear, &paths, true).unwrap(),
        LiveEditorAction::ClearScreen
    ));

    assert!(matches!(
        editor.handle_event(escape(), &paths, false).unwrap(),
        LiveEditorAction::Redraw
    ));
    assert!(matches!(
        editor.handle_event(escape(), &paths, false).unwrap(),
        LiveEditorAction::Redraw
    ));
    // Esc no longer clears drafts anywhere; empty the editor manually
    // before asserting the empty-submit path.
    assert_eq!(editor.input, "draft");
    editor.clear();

    assert!(matches!(
        editor
            .handle_event(
                Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
                &paths,
                false,
            )
            .unwrap(),
        LiveEditorAction::EmptySubmit
    ));
    assert!(editor.history.is_empty());

    editor.input = "/help".to_string();
    editor.cursor = editor.input.chars().count();
    assert!(matches!(
        editor
            .handle_event(
                Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
                &paths,
                false,
            )
            .unwrap(),
        LiveEditorAction::Submit(_)
    ));
    assert!(editor.history.is_empty());
    editor.record_history("ordinary prompt");
    assert_eq!(editor.history, ["ordinary prompt"]);
}

#[test]
fn live_editor_shift_enter_inserts_newline_without_submit() {
    let temp = tempfile::tempdir().unwrap();
    let paths = pop_test_paths(temp.path());
    let mut editor = LiveReplEditor::new(AgentMode::Normal, Vec::new());
    editor.input = "hello".to_string();
    editor.cursor = 5;
    assert!(matches!(
        editor
            .handle_event(
                Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT)),
                &paths,
                false,
            )
            .unwrap(),
        LiveEditorAction::Redraw
    ));
    assert_eq!(editor.input, "hello\n");
    assert_eq!(editor.cursor, 6);

    assert!(matches!(
        editor
            .handle_event(
                Event::Key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL)),
                &paths,
                false,
            )
            .unwrap(),
        LiveEditorAction::Redraw
    ));
    assert_eq!(editor.input, "hello\n\n");
    assert_eq!(editor.cursor, 7);
}

#[test]
fn prompt_rows_wrap_at_terminal_width() {
    assert_eq!(repl_prompt_rows_for_cols("", &["1234567".into()], 10), 1);
    assert_eq!(repl_prompt_rows_for_cols("", &["1234567890".into()], 10), 2);
    assert_eq!(
        repl_prompt_rows_for_cols("", &["123".into(), "456".into()], 10),
        2
    );
}

#[test]
fn cursor_position_wraps_at_terminal_width() {
    assert_eq!(repl_cursor_position_for_cols("", "1234567", 7, 10), (7, 0));
    assert_eq!(
        repl_cursor_position_for_cols("", "1234567890", 10, 10),
        (0, 1)
    );
    assert_eq!(repl_cursor_position_for_cols("", "123\n456", 7, 10), (3, 1));
    assert_eq!(repl_cursor_position_for_cols("", "1234567", 3, 10), (3, 0));
}

#[test]
fn cursor_position_keeps_prefix_after_newline() {
    assert_eq!(repl_cursor_position_for_cols("  ", "123\n", 4, 10), (2, 1));
    assert_eq!(
        repl_cursor_position_for_cols("  ", "123\n456", 7, 10),
        (5, 1)
    );
}

#[test]
fn prompt_rows_include_prefix_on_each_line() {
    assert_eq!(
        repl_prompt_rows_for_cols("  ", &["12".into(), "34".into()], 5),
        2
    );
    assert_eq!(
        repl_prompt_rows_for_cols("  ", &["123".into(), "34".into()], 5),
        3
    );
}

#[test]
fn wrapped_input_rows_keep_prefix_outside_content_width() {
    assert_eq!(
        repl_wrapped_input_rows_for_cols("  ", &["123456789".into()], 10),
        vec!["12345678".to_string(), "9".to_string()]
    );
    assert_eq!(
        repl_wrapped_input_rows_for_cols("  ", &["12345678".into()], 10),
        vec!["12345678".to_string(), String::new()]
    );
    assert_eq!(
        repl_cursor_position_for_cols("  ", "12345678", 8, 10),
        (2, 1)
    );
}

#[test]
fn history_browsing_requires_empty_or_clean_history_input() {
    let history = vec!["first".to_string(), "second".to_string()];

    assert!(repl_should_browse_history("", &history, None));
    assert!(repl_should_browse_history("second", &history, Some(1)));
    assert!(!repl_should_browse_history("draft", &history, None));
    assert!(!repl_should_browse_history(
        "second edited",
        &history,
        Some(1)
    ));
}

#[test]
fn vertical_cursor_move_uses_soft_wrapped_rows() {
    assert_eq!(
        repl_move_cursor_vertical_for_cols("  ", "123456789", 9, -1, 10),
        1
    );
    assert_eq!(
        repl_move_cursor_vertical_for_cols("  ", "123456789", 1, 1, 10),
        9
    );
}

#[test]
fn vertical_cursor_move_handles_explicit_newlines() {
    assert_eq!(
        repl_move_cursor_vertical_for_cols("  ", "abc\ndef", 6, -1, 20),
        2
    );
    assert_eq!(
        repl_move_cursor_vertical_for_cols("  ", "abc\ndef", 2, 1, 20),
        6
    );
}

#[test]
fn vertical_cursor_move_handles_wide_chars_near_wrap() {
    assert_eq!(
        repl_cursor_position_for_cols("  ", "1234567你", 8, 11),
        (2, 1)
    );
    assert_eq!(
        repl_cursor_position_for_cols("  ", "12345678你", 9, 11),
        (4, 1)
    );
    assert_eq!(
        repl_move_cursor_vertical_for_cols("  ", "12345678你好", 9, -1, 11),
        2
    );
}

#[test]
fn drain_stdin_does_not_panic() {
    drain_stdin();
}

#[test]
fn input_helpers_edit_at_cursor() {
    let mut input = "abcd".to_string();
    let mut cursor = 2;
    insert_char_at_cursor(&mut input, &mut cursor, '中');
    assert_eq!(input, "ab中cd");
    assert_eq!(cursor, 3);

    remove_char_before_cursor(&mut input, &mut cursor);
    assert_eq!(input, "abcd");
    assert_eq!(cursor, 2);

    remove_char_at_cursor(&mut input, cursor);
    assert_eq!(input, "abd");
    assert_eq!(cursor, 2);
}

#[test]
fn input_helpers_remove_word_before_cursor() {
    let mut nothing_pasted = (Vec::new(), Vec::new());
    let mut input = "hello world  ".to_string();
    let mut cursor = input.chars().count();
    remove_word_before_cursor(
        &mut input,
        &mut cursor,
        &mut nothing_pasted.0,
        &mut nothing_pasted.1,
    );
    assert_eq!(input, "hello ");
    assert_eq!(cursor, 6);

    let mut input = "前面 中间 后面".to_string();
    let mut cursor = 6;
    remove_word_before_cursor(
        &mut input,
        &mut cursor,
        &mut nothing_pasted.0,
        &mut nothing_pasted.1,
    );
    assert_eq!(input, "前面 后面");
    assert_eq!(cursor, 3);
}

/// Ctrl+W 必须把占位符整块删掉，**隔着空白时也一样**。
///
/// 占位符自带空格（`[Pasted 1: ~3 lines]`），按空白分词会切进它中段，只删掉
/// ` lines]`，屏幕上留下 `[Pasted 1: ~3` 这种再也解析不回去的残片；更糟的是
/// 载荷还挂在数组里，提交时被展开——用户看到的和发出去的对不上。
#[test]
fn ctrl_w_removes_a_placeholder_whole_even_across_whitespace() {
    let paste = |lines: usize| {
        Some(PastedText {
            text: "x\n".repeat(lines),
        })
    };

    // 隔着一个空格：以前在这里切进中段。
    let mut input = "看 [Pasted 1: ~3 lines] ".to_string();
    let mut cursor = input.chars().count();
    let mut images = Vec::new();
    let mut texts = vec![paste(3)];
    remove_word_before_cursor(&mut input, &mut cursor, &mut images, &mut texts);
    assert_eq!(input, "看 ", "占位符必须整块消失，不留残片");
    assert_eq!(cursor, input.chars().count());
    assert!(texts[0].is_none(), "载荷没清掉，提交时会被展开回来");

    // 紧贴其后：原本就该整块删，别改坏。
    let mut input = "看 [Pasted 1: ~3 lines]".to_string();
    let mut cursor = input.chars().count();
    let mut texts = vec![paste(3)];
    remove_word_before_cursor(&mut input, &mut cursor, &mut Vec::new(), &mut texts);
    assert_eq!(input, "看 ");
    assert!(texts[0].is_none());

    // 两个占位符贴在一起时一次只删一个：占位符就是一个词，Ctrl+W 删一个词。
    let mut input = "[Image 1][Image 2]".to_string();
    let mut cursor = input.chars().count();
    let mut images = vec![None, None];
    remove_word_before_cursor(&mut input, &mut cursor, &mut images, &mut Vec::new());
    assert_eq!(input, "[Image 1]");
    assert_eq!(cursor, 9);
    remove_word_before_cursor(&mut input, &mut cursor, &mut images, &mut Vec::new());
    assert_eq!(input, "");
    assert_eq!(cursor, 0);

    // 占位符后面还有普通词时，先删那个词，别误伤占位符。
    let mut input = "[Pasted 1: ~3 lines] 这段".to_string();
    let mut cursor = input.chars().count();
    let mut texts = vec![paste(3)];
    remove_word_before_cursor(&mut input, &mut cursor, &mut Vec::new(), &mut texts);
    assert_eq!(input, "[Pasted 1: ~3 lines] ");
    assert!(texts[0].is_some(), "没碰到的占位符，载荷不能被清掉");
}

#[test]
fn input_helpers_insert_paste_at_cursor() {
    let mut input = "前后".to_string();
    let mut cursor = 1;
    insert_str_at_cursor(&mut input, &mut cursor, "中间");
    assert_eq!(input, "前中间后");
    assert_eq!(cursor, 3);
}

#[test]
fn input_helpers_insert_newline_at_cursor() {
    let mut input = "前后".to_string();
    let mut cursor = 1;
    insert_newline_at_cursor(&mut input, &mut cursor);
    assert_eq!(input, "前\n后");
    assert_eq!(cursor, 2);
}

#[test]
fn long_paste_visible_lines_are_collapsed() {
    let lines = (0..20)
        .map(|index| format!("line {index}"))
        .collect::<Vec<_>>();
    // 整段都是粘进来的原文:这才是折叠该管的场景。
    let visible = repl_visible_input_lines("[NORMAL] > ", &lines, 12, 20);

    assert_eq!(visible.len(), 3);
    assert_eq!(visible[0], "line 0");
    assert!(visible[1].contains("18") || visible[1].contains("已隐藏 18"));
    assert_eq!(visible[2], "line 19");
    assert_eq!(lines.len(), 20);
}

/// 已经敲了十几行之后再粘一小段,不该把用户自己写的内容折起来(08-26 实测:
/// 折叠判据原本是"上一个动作是不是粘贴",随便再打个字又恢复)。
#[test]
fn a_short_paste_does_not_collapse_hand_typed_input() {
    let mut lines = (0..18)
        .map(|index| format!("第 {index} 行是我自己敲的"))
        .collect::<Vec<_>>();
    lines.push("/home/shorin/Downloads/1.png".to_string());

    // 生粘贴只带进来 1 行,缓冲区绝大部分是手敲的 → 不折。
    let visible = repl_visible_input_lines("  ", &lines, 12, 1);
    assert_eq!(visible.len(), lines.len(), "短粘贴不该折叠手敲内容");
    assert_eq!(visible, lines);

    // 一行都不是粘进来的,同样不折。
    assert_eq!(repl_visible_input_lines("  ", &lines, 12, 0), lines);
}

#[test]
fn long_paste_is_replaced_with_placeholder_and_expanded() {
    let text = "alpha\nbeta\ngamma".to_string();
    let placeholder = pasted_text_placeholder(1, pasted_text_line_count(&text));
    let input = format!("请分析 {placeholder}谢谢");
    let pasted_texts = vec![Some(PastedText { text: text.clone() })];

    assert!(should_summarize_pasted_text(&text));
    assert_eq!(
        expand_pasted_text_placeholders(&input, &pasted_texts),
        "请分析 alpha\nbeta\ngamma谢谢"
    );
}

#[test]
fn short_paste_is_not_summarized() {
    assert!(!should_summarize_pasted_text("short paste"));
}

#[test]
fn insert_pasted_text_summarizes_long_clipboard_text() {
    let mut input = "前后".to_string();
    let mut cursor = 1;
    let mut pasted_texts = Vec::new();

    insert_pasted_text_at_cursor(
        &mut input,
        &mut cursor,
        "alpha\nbeta\ngamma".to_string(),
        &mut pasted_texts,
    );

    assert!(
        input == "前[Pasted 1: ~3 lines]后" || input == "前[粘贴 1: ~3 行]后",
        "unexpected localized placeholder: {input}"
    );
    assert_eq!(pasted_texts.len(), 1);
    assert_eq!(cursor, input.chars().count() - 1);
}

#[test]
fn pasted_placeholder_is_treated_as_atomic_token() {
    let input = "前[Pasted 1: ~3 lines] 后";
    assert_eq!(placeholder_at_cursor(input, 3), Some((1, 21)));
    assert_eq!(placeholder_before_cursor(input, 21), Some((1, 21)));
    assert_eq!(placeholder_after_cursor(input, 1), Some((1, 21)));
    assert_eq!(placeholder_before_or_at_cursor(input, 3), Some((1, 21)));
    assert_eq!(placeholder_after_or_at_cursor(input, 3), Some((1, 21)));
}

#[test]
fn chinese_pasted_placeholder_is_supported() {
    let input = "前[粘贴 1: ~3 行] 后";
    let placeholder = find_pasted_text_placeholders(input);

    assert_eq!(placeholder, vec![(1, 13, 1)]);
    assert_eq!(placeholder_at_cursor(input, 3), Some((1, 13)));
    assert_eq!(placeholder_before_cursor(input, 13), Some((1, 13)));
    assert_eq!(placeholder_after_cursor(input, 1), Some((1, 13)));
}

#[test]
fn colorizes_image_and_pasted_placeholders() {
    let colored = colorize_repl_placeholders("[Image 1] [Pasted 1: ~3 lines]");
    assert!(colored.contains("\x1b[35m[Image 1]\x1b[0m"));
    assert!(colored.contains("\x1b[35m[Pasted 1: ~3 lines]\x1b[0m"));
}

#[test]
fn placeholder_text_near_cursor_expands_pasted_placeholder() {
    let input = "前[Pasted 1: ~3 lines]后";
    let pasted_texts = vec![Some(PastedText {
        text: "alpha\nbeta\ngamma".to_string(),
    })];

    assert_eq!(
        placeholder_text_near_cursor(input, 3, &pasted_texts),
        Some("alpha\nbeta\ngamma".to_string())
    );
}

#[test]
fn strips_terminal_control_sequences_from_repl_text() {
    assert_eq!(
        strip_terminal_control_sequences("\x1b[E表情包\x1b[0m\x07 ok"),
        "表情包 ok"
    );
    assert_eq!(
        strip_terminal_control_sequences("line1\nline2\tend"),
        "line1\nline2\tend"
    );
}

/// 只需要 state_dir 的 fixture：历史文件、状态库都落在它下面。
fn state_only_paths(root: &std::path::Path) -> MiyuPaths {
    MiyuPaths {
        root_dir: PathBuf::new(),
        config_dir: PathBuf::new(),
        config_file: PathBuf::new(),
        skills_dir: PathBuf::new(),
        data_dir: PathBuf::new(),
        cache_dir: PathBuf::new(),
        state_dir: root.to_path_buf(),
        pictures_dir: PathBuf::new(),
        fish_hook_file: PathBuf::new(),
        bash_hook_file: PathBuf::new(),
        zsh_hook_file: PathBuf::new(),
        scripts_dir: PathBuf::new(),
        system_scripts_dir: PathBuf::new(),
    }
}

#[test]
fn repl_history_loads_user_messages_from_state() {
    let temp = tempfile::tempdir().unwrap();
    let paths = state_only_paths(temp.path());
    let state = StateStore::new(&paths).unwrap();
    state.start_turn("turn_1", "first", 999999).unwrap();
    state.complete_turn("turn_1", "reply", None).unwrap();
    state.start_turn("turn_2", "second", 999999).unwrap();

    assert_eq!(
        load_repl_input_history(&state, &paths).unwrap(),
        vec!["first".to_string(), "second".to_string()]
    );
}

/// 两个 REPL 同时开着时，后敲的内容要能被先开的那个翻出来。
///
/// 历史以前只在启动时读一次，之后整个循环没有重新加载的路径——先开的那个
/// REPL 的 `Vec` 就此冻结。上一轮排查按「先敲完再开第二个」测，走的是启动
/// 加载那条路，当然是通的，所以没复现。
#[test]
fn a_second_repl_sees_what_the_first_one_just_typed() {
    let temp = tempfile::tempdir().unwrap();
    let paths = state_only_paths(temp.path());

    // REPL#2 先开：拿到一份当时的快照。
    let mut second_repl_history = vec!["老条目".to_string()];

    // REPL#1 后敲了两条（提交前就落盘，与生产路径一致）。
    persist_repl_history_entry(&paths, "sess_a", "cargo test --all");
    persist_repl_history_entry(&paths, "sess_a", "看一下 tokio 的文档");

    // REPL#2 按上键 → 现读一次。
    assert!(refresh_repl_input_history(
        &mut second_repl_history,
        &paths,
        "sess_a"
    ));
    assert_eq!(
        second_repl_history,
        vec![
            "老条目".to_string(),
            "cargo test --all".to_string(),
            "看一下 tokio 的文档".to_string(),
        ],
        "最近敲的必须在末尾——上键是从末尾往回走的"
    );

    // 再刷一次没有新东西，不该重复追加。
    assert!(!refresh_repl_input_history(
        &mut second_repl_history,
        &paths,
        "sess_a"
    ));
    assert_eq!(second_repl_history.len(), 3);
}

/// 历史按会话隔离：别的会话敲的东西不该出现在这个会话的上键里。
#[test]
fn repl_history_does_not_leak_across_sessions() {
    let temp = tempfile::tempdir().unwrap();
    let paths = state_only_paths(temp.path());

    persist_repl_history_entry(&paths, "sess_a", "只属于 A");
    persist_repl_history_entry(&paths, "sess_b", "只属于 B");

    let mut a = Vec::new();
    refresh_repl_input_history(&mut a, &paths, "sess_a");
    assert_eq!(a, vec!["只属于 A".to_string()]);

    let mut b = Vec::new();
    refresh_repl_input_history(&mut b, &paths, "sess_b");
    assert_eq!(b, vec!["只属于 B".to_string()]);
}

/// 分会话之前的全局文件仍然读得到——直接丢掉用户会觉得「历史没了」。
/// 它排在最前面：里面的都是更早的输入，上键从末尾走，最近的要在后面。
#[test]
fn legacy_global_history_is_still_reachable() {
    let temp = tempfile::tempdir().unwrap();
    let paths = state_only_paths(temp.path());
    std::fs::create_dir_all(&paths.state_dir).unwrap();
    std::fs::write(
        paths.state_dir.join("repl-history.jsonl"),
        "\"分会话之前敲的\"\n",
    )
    .unwrap();
    persist_repl_history_entry(&paths, "default", "分会话之后敲的");

    let state = StateStore::new(&paths).unwrap();
    assert_eq!(
        load_repl_input_history(&state, &paths).unwrap(),
        vec!["分会话之前敲的".to_string(), "分会话之后敲的".to_string()]
    );
}

/// 视频粘贴要显示成 `[Video N]` 而不是 `[Image N]`(08-28 用户点名"怎么视频
/// 粘贴进去还是图片的占位符")。两种标签共用同一条解析通路。
#[test]
fn video_placeholders_carry_their_own_label() {
    // 定位:两种前缀都要被认出来。
    let input = "看这个 [Image 1] 和 [Video 2] 还有 [Video 10: clip.mp4]";
    assert_eq!(find_image_placeholders(input).len(), 3);

    // 序号解析对两种标签一致,多位数也不能丢。
    let spots = find_image_placeholders(input);
    let idx = |n: usize| {
        parse_image_placeholder_index(input, spots[n].0, spots[n].1).expect("能解析出序号")
    };
    assert_eq!(idx(0), 1);
    assert_eq!(idx(1), 2);
    assert_eq!(idx(2), 10);

    // 标签按文件类型挑。
    assert_eq!(media_placeholder_label("/tmp/a.mp4"), "Video");
    assert_eq!(media_placeholder_label("/tmp/a.mkv"), "Video");
    assert_eq!(media_placeholder_label("/tmp/a.png"), "Image");
    assert_eq!(media_placeholder_label("/tmp/a.pdf"), "PDF");
}

/// `[PDF ` 只有 5 位,而前缀匹配一度写死 7("[Image "/"[Video " 恰好都是 7)。
/// 写死的那版会把 PDF 占位符整条漏掉:光标能走进去、上色不生效、路径改写
/// 也认不出来。
#[test]
fn pdf_placeholders_are_found_despite_shorter_prefix() {
    let input = "看 [PDF 1] 和 [PDF 2: /tmp/b.pdf] 还有 [Image 3]";
    let spots = find_repl_placeholders(input);
    let chars: Vec<char> = input.chars().collect();
    let slice = |n: usize| chars[spots[n].0..spots[n].1].iter().collect::<String>();

    assert_eq!(spots.len(), 3, "三个占位符都要认出来");
    assert_eq!(slice(0), "[PDF 1]");
    assert_eq!(slice(1), "[PDF 2: /tmp/b.pdf]");
    assert_eq!(slice(2), "[Image 3]");

    // 序号照旧解析得出,PDF 与图片走同一套改写。
    let idx = |n: usize| {
        parse_image_placeholder_index(input, spots[n].0, spots[n].1).expect("能解析出序号")
    };
    assert_eq!(idx(0), 1);
    assert_eq!(idx(1), 2);
    assert_eq!(idx(2), 3);
}
