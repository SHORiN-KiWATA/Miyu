//! footer 计量与活动区（live tail）的渲染。

// 被测的东西散在 cli::mod 与 repl 的兄弟模块里，这里全都要够到。
use crate::cli::repl::editor::*;
use crate::cli::repl::layout::terminal_frame_layout_with_scrolls;
use crate::cli::repl::tail::{
    live_frame_output_bottom, live_tail_next_start, live_tail_placement, max_live_tail_start,
    queue_lifted_frame, FrameScroll, LiveTailPlacement,
};
use crate::cli::repl::width::*;
use crate::cli::*;
use crate::llm::ChatStreamKind;
#[test]
fn terminal_frame_tracks_ansi_and_wide_graphemes() {
    let layout = terminal_frame_layout("\x1b[32mAB\x1b[0m\n中👨‍👩‍👧‍👦".as_bytes(), (3, 2), 12, None);

    assert_eq!(layout.cursor, (4, 3));
    assert_eq!(layout.occupied_bottom, Some(3));
}

#[test]
fn terminal_frame_wraps_before_the_next_wide_grapheme() {
    let layout = terminal_frame_layout("中🙂".as_bytes(), (8, 1), 10, None);

    assert_eq!(layout.cursor, (2, 2));
    assert_eq!(layout.occupied_bottom, Some(2));
}

#[test]
fn terminal_frame_applies_cursor_motion_without_losing_bottom_occupancy() {
    let layout = terminal_frame_layout(b"first\nsecond\x1b[1A\x1b[3G!", (0, 4), 20, None);

    assert_eq!(layout.cursor, (3, 4));
    assert_eq!(layout.occupied_bottom, Some(5));
}

#[test]
fn terminal_frame_scroll_margin_keeps_cursor_above_live_input() {
    let layout = terminal_frame_layout("one\n二\nthree".as_bytes(), (0, 5), 20, Some(5));

    assert_eq!(layout.cursor, (5, 5));
    assert_eq!(layout.occupied_bottom, Some(5));
}

#[test]
fn live_frame_uses_the_gap_only_for_a_terminating_newline() {
    let content = terminal_frame_layout(b"answer", (0, 5), 20, None);
    assert_eq!(live_frame_output_bottom(6, content), Some(5));

    let terminated = terminal_frame_layout(b"answer\n", (0, 5), 20, None);
    assert_eq!(live_frame_output_bottom(6, terminated), Some(6));
    let bounded = terminal_frame_layout(
        b"answer\n",
        (0, 5),
        20,
        live_frame_output_bottom(6, terminated),
    );
    assert_eq!(bounded.cursor, (0, 6));
    assert_eq!(bounded.occupied_bottom, Some(5));
}

#[test]
fn replayed_job_wake_turns_are_not_drawn_as_user_prompts() {
    let config = AppConfig::default();
    let wake = crate::state::TurnReplay {
        display_content: "[后台任务完成] 子代理完成 82bea3 · 后台测试A".to_string(),
        assistant_content: "跑完了。".to_string(),
        entries: Vec::new(),
        is_synthetic: true,
    };
    let typed = crate::state::TurnReplay {
        display_content: "帮我改一下 README".to_string(),
        assistant_content: "改好了。".to_string(),
        entries: Vec::new(),
        is_synthetic: false,
    };

    let frame = session_replay_frame(&[wake], AgentMode::Normal, &config, 80).unwrap();
    let frame = String::from_utf8_lossy(&frame);
    // Dim ⚙ notice with the bracketed prefix stripped, exactly like the
    // live path — never the user bubble's bar.
    assert!(frame.contains("⚙ 子代理完成 82bea3 · 后台测试A"));
    assert!(!frame.contains("[后台任务完成]"));
    assert!(!frame.contains(&submitted_echo_bar(AgentMode::Normal)));

    let frame = session_replay_frame(&[typed], AgentMode::Normal, &config, 80).unwrap();
    let frame = String::from_utf8_lossy(&frame);
    assert!(frame.contains(&submitted_echo_bar(AgentMode::Normal)));
    assert!(!frame.contains('⚙'));
}

#[test]
fn pop_menu_footer_has_controls_but_no_position_counter() {
    let help = strip_terminal_control_sequences(&pop_menu_help_line(120));
    assert!(help.contains("Tab"));
    assert!(help.contains("Enter"));
    assert!(!help.contains("3 / 8"));

    let header = strip_terminal_control_sequences(&pop_menu_header("", 2, 8, 80));
    assert!(header.contains("2 / 8"));
}

#[test]
fn footer_reset_clears_turn_and_cumulative_tokens() {
    let config = AppConfig::default();
    let mut footer = ReplFooterStatus::from_config(
        &config,
        100,
        TurnTokens {
            total: 250,
            ..Default::default()
        },
    );
    footer.set_token_usage(
        50,
        100,
        Some(200_000),
        TurnTokens {
            total: 250,
            ..Default::default()
        },
    );

    footer.reset_token_usage(0, Some(200_000));

    assert_eq!(footer.token_usage.turn_tokens, 0);
    assert_eq!(footer.token_usage.session_tokens, 0);
    assert_eq!(footer.token_usage.context_window, Some(200_000));
    assert_eq!(footer.token_usage.cumulative_tokens, None);

    footer.reset_token_usage(0, None);
    assert_eq!(footer.token_usage.context_window, None);
}

#[test]
fn footer_turn_completion_updates_the_rendered_token_accounting() {
    let config = AppConfig::default();
    let mut footer = ReplFooterStatus::from_config(&config, 0, TurnTokens::default());
    let result = ChatResult {
        content: "reply".to_string(),
        reasoning: None,
        usage: Some(Usage {
            prompt_tokens: 80,
            completion_tokens: 20,
            total_tokens: 100,
            ..Usage::default()
        }),
        usage_estimated: false,
        tool_calls: Vec::new(),
        provider_id: None,
        model: None,
        finish_reason: None,
        thinking_signature: None,
        last_request_usage: None,
        responses_continuation: None,
    };

    footer.update_token_usage(
        &result,
        240,
        Some(200_000),
        TurnTokens {
            total: 100,
            ..Default::default()
        },
    );

    assert_eq!(footer.token_usage.turn_tokens, 100);
    assert_eq!(footer.token_usage.session_tokens, 240);
    assert_eq!(footer.token_usage.cumulative_tokens, Some(100));
    assert_eq!(
        strip_terminal_control_sequences(&repl_footer_line(AgentMode::Normal, &footer, 80))
            .split_whitespace()
            .last(),
        Some("Σ100")
    );
}

#[test]
fn an_idle_tick_only_redraws_when_the_cumulative_actually_moved() {
    let config = AppConfig::default();
    let mut footer = ReplFooterStatus::from_config(&config, 0, TurnTokens::default());
    let totals = TurnTokens {
        total: 32_808,
        prompt: 29_035,
        cache_read: 17_664,
    };
    assert!(footer.update_cumulative_tokens(totals));
    // The jobs poll republishes the same Σ every second; redrawing the
    // whole tail on each of those would fight the strip animation.
    assert!(!footer.update_cumulative_tokens(totals));

    // A background subagent finishing moves only the cache halves — the
    // total can stay put when its usage was estimated, so equality has to
    // consider all three.
    assert!(footer.update_cumulative_tokens(TurnTokens {
        cache_read: 20_000,
        ..totals
    }));
}

#[test]
fn the_footer_leaves_the_per_turn_figure_to_the_token_line() {
    let config = AppConfig::default();
    let mut footer = ReplFooterStatus::from_config(&config, 0, TurnTokens::default());
    footer.set_token_usage_with_cache(
        TurnTokens {
            total: 21_224,
            prompt: 16_139,
            cache_read: 6_528,
        },
        21_700,
        Some(1_000_000),
        TurnTokens {
            total: 180_100,
            prompt: 47_538,
            cache_read: 11_392,
        },
    );

    let line = strip_terminal_control_sequences(&repl_footer_line(AgentMode::Normal, &footer, 80));
    // Two standing gauges only. Carrying the turn figure as well cost 14
    // columns and pushed the whole footer past 80.
    assert!(line.contains("21.7k/1M(2.2%)"), "{line}");
    assert!(line.contains("Σ180.1k(C24%)"), "{line}");
    assert!(!line.contains("21.2k"), "{line}");
    assert!(!line.contains("C40%"), "{line}");
    assert!(
        visible_width(&line) <= 80,
        "footer must fit 80 columns: {} — {line}",
        visible_width(&line)
    );
}

#[test]
fn footer_variant_always_uses_the_fixed_primary_color() {
    let config = AppConfig::default();
    let mut footer = ReplFooterStatus::from_config(&config, 0, TurnTokens::default());
    footer.update_thinking_variant(Some("high"));

    for mode in [AgentMode::Normal, AgentMode::Dev] {
        let line = repl_footer_left(mode, &footer, 120);
        assert!(line.contains("\x1b[1m\x1b[34mhigh\x1b[0m"));
        assert_eq!(
            strip_terminal_control_sequences(&line),
            format!(
                "{} · {} {} · high",
                mode.label(),
                footer.model,
                footer.provider
            )
        );
    }
}

#[test]
fn mixed_footer_uses_dim_provider_and_hides_global_variant() {
    let mut config = AppConfig::default();
    let provider = config
        .providers
        .iter_mut()
        .find(|provider| !provider.models.is_empty())
        .unwrap();
    let provider_id = provider.id.clone();
    let first_model = provider.models[0].clone();
    let second_model = "footer-second-model".to_string();
    provider.models.push(second_model.clone());
    config.active_provider_models = Some(vec![
        ActiveProviderModelConfig {
            provider_id: provider_id.clone(),
            model: first_model,
        },
        ActiveProviderModelConfig {
            provider_id,
            model: second_model,
        },
    ]);
    let mut footer = ReplFooterStatus::from_config(&config, 0, TurnTokens::default());
    footer.update_thinking_variant(Some("mixed"));

    let line = repl_footer_left(AgentMode::Normal, &footer, 120);

    assert_eq!(footer.provider, "mixed");
    assert!(footer.thinking.is_none());
    assert_eq!(
        strip_terminal_control_sequences(&line),
        format!(
            "{} · {} mixed",
            AgentMode::Normal.label(),
            t("Mixed", "混合")
        )
    );
    assert!(line.contains("\x1b[2mmixed\x1b[0m"));
    assert!(!line.contains(&primary_footer_text("mixed")));
}

#[test]
fn committed_user_message_keeps_one_blank_line_before_output() {
    let output = committed_user_messages_text(&[("hello", AgentMode::Normal)], true, 80);

    assert_eq!(
        strip_terminal_control_sequences(&output),
        "\n┃\n┃ hello\n┃\n\n"
    );
}

#[test]
fn queued_message_uses_full_height_bar_and_primary_status() {
    let prompt = QueuedPrompt {
        prompt_id: "q1".to_string(),
        seq: 1,
        content: "follow up".to_string(),
        display_content: "follow up".to_string(),
        attachments: Vec::new(),
        uploaded_attachments: Vec::new(),
        submitted_at: String::new(),
    };

    let normal = queued_prompt_lines(std::slice::from_ref(&prompt), AgentMode::Normal, 80);
    let chat = queued_prompt_lines(&[prompt], AgentMode::Dev, 80);

    assert_eq!(normal.len(), 4);
    assert_eq!(normal[0], submitted_echo_bar(AgentMode::Normal));
    assert_eq!(normal[2], submitted_echo_bar(AgentMode::Normal));
    assert!(normal[3].starts_with(&submitted_echo_bar(AgentMode::Normal)));
    assert!(normal[3].contains(&primary_footer_text(t("Queued", "排队中"))));
    assert!(chat
        .iter()
        .filter(|line| !line.is_empty())
        .all(|line| line.starts_with(&submitted_echo_bar(AgentMode::Dev))));
    assert_ne!(normal[0], chat[0]);
}

#[test]
fn live_tail_moves_naturally_and_releases_after_output_shrinks() {
    assert_eq!(max_live_tail_start(6, 5), 0);
    assert_eq!(max_live_tail_start(24, 5), 18);
    assert_eq!(
        live_tail_placement(0, 4, 5, 24, false),
        LiveTailPlacement {
            output_row: 4,
            tail_start: 4,
            overflow: 0,
            anchored: false,
        }
    );
    assert_eq!(
        live_tail_placement(0, 20, 5, 24, false),
        LiveTailPlacement {
            output_row: 18,
            tail_start: 18,
            overflow: 2,
            anchored: true,
        }
    );
    assert_eq!(
        live_tail_placement(0, 6, 5, 24, false),
        LiveTailPlacement {
            output_row: 6,
            tail_start: 6,
            overflow: 0,
            anchored: false,
        }
    );
    assert_eq!(live_tail_placement(0, 6, 5, 30, false).tail_start, 6);
}

#[test]
fn anchored_tail_stays_at_the_bottom_when_it_shrinks() {
    // A job strip pushed a bottom-anchored 5-row tail to 7 rows, scrolling
    // the screen twice, so output now ends at row 16. The strip goes away.
    let shrunk = live_tail_placement(0, 16, 5, 24, true);
    assert_eq!(
        shrunk,
        LiveTailPlacement {
            // Stays where the output really ended: the renderer's spinner
            // erases itself relative to this cursor.
            output_row: 16,
            tail_start: 18,
            overflow: 0,
            anchored: true,
        }
    );
    // Bottom edge back on the last usable row, where it was before.
    assert_eq!(shrunk.tail_start + 5, 24 - 1);

    // Without the anchor the tail hugs the output cursor as before: a
    // conversation that has not filled the screen is untouched.
    assert_eq!(
        live_tail_placement(0, 16, 5, 24, false),
        LiveTailPlacement {
            output_row: 16,
            tail_start: 16,
            overflow: 0,
            anchored: false,
        }
    );

    // Growing while anchored still scrolls rather than double-counting.
    assert_eq!(
        live_tail_placement(0, 18, 7, 24, true),
        LiveTailPlacement {
            output_row: 16,
            tail_start: 16,
            overflow: 2,
            anchored: true,
        }
    );
}

#[test]
fn streaming_output_never_drags_an_anchored_tail_back_up() {
    // 24 rows, 5-row tail → anchored at 18. Output ends two rows above it
    // because a job strip just went away; the frame must leave the tail
    // alone and fill the gap instead of reclaiming those rows.
    let max_tail = max_live_tail_start(24, 5);
    assert_eq!(max_tail, 18);
    assert_eq!(live_tail_next_start(18, 16, max_tail), 18);
    // Still pinned once the gap is closed.
    assert_eq!(live_tail_next_start(18, 18, max_tail), 18);
    // And it never runs past the anchor.
    assert_eq!(live_tail_next_start(18, 21, max_tail), 18);

    // A tail that had not reached the bottom keeps following the output.
    assert_eq!(live_tail_next_start(10, 12, max_tail), 12);
    assert_eq!(live_tail_next_start(10, 8, max_tail), 8);
    assert_eq!(live_tail_next_start(10, 30, max_tail), 18);
}

#[test]
fn spinner_does_not_resume_tail_during_external_output() {
    let config = AppConfig::default();
    let mut live = LiveReplTail {
        editor: LiveReplEditor::new(AgentMode::Normal, Vec::new()),
        queued: Vec::new(),
        pending_chunks: Vec::new(),
        footer: ReplFooterStatus::from_config(&config, 0, TurnTokens::default()),
        round_base_footer: None,
        footer_offset: None,
        footer_spinner_last: None,
        output_cursor: (0, 0),
        tail_start: 0,
        tail_rows: 0,
        input_cursor: (0, 0),
        rendered: false,
        external_output_active: true,
        raw_mode_handoff: false,
        jobs: Vec::new(),
        job_spinner: 0,
    };
    let mut renderer = render::StreamRenderer::new(
        render::ReasoningDisplayMode::Hidden,
        render::ToolCallDisplayMode::Hidden,
        true,
        true,
        10,
    );

    handle_live_agent_event(&mut live, &mut renderer, AgentEvent::SpinnerTick).unwrap();

    assert!(live.external_output_active);
    assert!(!live.rendered);
}

#[test]
fn live_tail_coalesces_adjacent_stream_chunks_and_can_discard_them() {
    let config = AppConfig::default();
    let mut live = LiveReplTail {
        editor: LiveReplEditor::new(AgentMode::Normal, Vec::new()),
        queued: Vec::new(),
        pending_chunks: Vec::new(),
        footer: ReplFooterStatus::from_config(&config, 0, TurnTokens::default()),
        round_base_footer: None,
        footer_offset: None,
        footer_spinner_last: None,
        output_cursor: (0, 0),
        tail_start: 0,
        tail_rows: 0,
        input_cursor: (0, 0),
        rendered: false,
        external_output_active: false,
        raw_mode_handoff: false,
        jobs: Vec::new(),
        job_spinner: 0,
    };

    for (kind, text) in [
        (ChatStreamKind::Reasoning, "one"),
        (ChatStreamKind::Reasoning, " two"),
        (ChatStreamKind::Content, "answer"),
        (ChatStreamKind::Content, " text"),
    ] {
        live.queue_stream_chunk(ChatStreamChunk {
            kind,
            text: text.to_string(),
        });
    }

    assert_eq!(live.pending_chunks.len(), 2);
    assert_eq!(live.pending_chunks[0].text, "one two");
    assert_eq!(live.pending_chunks[1].text, "answer text");
    live.discard_pending_chunks();
    assert!(live.pending_chunks.is_empty());
}

// ---- kitty 图片残影:帧的滚动点与「整屏滚 + 插回」的切段 ----

#[test]
fn terminal_frame_records_where_each_bottom_scroll_happens() {
    let (layout, scrolls) = terminal_frame_layout_with_scrolls(b"ab\ncd\nef", (0, 5), 20, Some(5));

    assert_eq!(layout.cursor, (2, 5));
    assert_eq!(
        scrolls,
        vec![
            FrameScroll {
                end: 3,
                col_after: 0
            },
            FrameScroll {
                end: 6,
                col_after: 0
            },
        ]
    );
}

#[test]
fn terminal_frame_attributes_a_wrap_scroll_to_the_wrapping_grapheme() {
    // 4 列:abcd 填满一行,e 折到下一行——顶在页底就是一次滚动,记在 e 自己
    // 的结束偏移(5)上,滚完光标停在第 1 列。
    let (_, scrolls) = terminal_frame_layout_with_scrolls(b"abcdef", (0, 5), 4, Some(5));
    assert_eq!(
        scrolls,
        vec![FrameScroll {
            end: 5,
            col_after: 1
        }]
    );

    // 折行紧跟换行:两次滚动必须落在不同偏移,否则帧切不开。
    let (_, scrolls) = terminal_frame_layout_with_scrolls(b"abcde\n", (0, 5), 4, Some(5));
    assert_eq!(
        scrolls,
        vec![
            FrameScroll {
                end: 5,
                col_after: 1
            },
            FrameScroll {
                end: 6,
                col_after: 0
            },
        ]
    );

    // 宽字符与多字节:「中」占 2 列,折行后光标在第 2 列,偏移是它 3 个字节的末尾。
    let (_, scrolls) = terminal_frame_layout_with_scrolls("abc中".as_bytes(), (0, 5), 4, Some(5));
    assert_eq!(
        scrolls,
        vec![FrameScroll {
            end: 6,
            col_after: 2
        }]
    );
}

#[test]
fn terminal_frame_without_bottom_margin_never_scrolls() {
    let (_, scrolls) = terminal_frame_layout_with_scrolls(b"a\nb\nc\n", (0, 5), 20, None);
    assert!(scrolls.is_empty());
}

fn move_to(col: u16, row: u16) -> String {
    format!("\x1b[{};{}H", row + 1, col + 1)
}

#[test]
fn lifted_frame_scrolls_the_whole_screen_and_pushes_the_tail_back() {
    // 页底第 5 行,光标已经在页底,帧 "a\nb\nc" 要滚两次。
    let frame = b"a\nb\nc";
    let (_, scrolls) = terminal_frame_layout_with_scrolls(frame, (0, 5), 20, Some(5));
    let mut transaction = Vec::new();
    queue_lifted_frame(&mut transaction, frame, (0, 5), 5, 0, &scrolls, 10).unwrap();

    let expected = format!(
        "{}{}\n\n{}\x1b[2L{}{}{}\x1b[r{}{}{}\x1b[r",
        "\x1b[r",
        move_to(0, 9),
        move_to(0, 4),
        "\x1b[1;6r",
        move_to(0, 3),
        "a\nb\n",
        "\x1b[1;6r",
        move_to(0, 5),
        "c",
    );
    assert_eq!(String::from_utf8(transaction).unwrap(), expected);
}

#[test]
fn lifted_frame_lifts_leading_scroll_without_consuming_the_frame() {
    // 光标掉到了页底之下两行:先整屏抬两行,帧再从页底写起,帧本身不滚。
    let frame = b"tail";
    let mut transaction = Vec::new();
    queue_lifted_frame(&mut transaction, frame, (0, 5), 5, 2, &[], 10).unwrap();

    let expected = format!(
        "{}{}\n\n{}\x1b[2L{}{}tail\x1b[r",
        "\x1b[r",
        move_to(0, 9),
        move_to(0, 4),
        "\x1b[1;6r",
        move_to(0, 5),
    );
    assert_eq!(String::from_utf8(transaction).unwrap(), expected);
}

#[test]
fn lifted_frame_is_cut_into_pieces_when_taller_than_the_page() {
    // 页底第 2 行(页只有 3 行),帧要滚 3 次:先抬 2 行写两段,再抬 1 行写一段,
    // 末尾不滚的部分直接写。三段拼起来必须正好是整个帧,一个字节不多不少。
    let frame = b"a\nb\nc\nd";
    let (_, scrolls) = terminal_frame_layout_with_scrolls(frame, (0, 2), 20, Some(2));
    assert_eq!(scrolls.len(), 3);
    let mut transaction = Vec::new();
    queue_lifted_frame(&mut transaction, frame, (0, 2), 2, 0, &scrolls, 10).unwrap();
    let text = String::from_utf8(transaction).unwrap();

    assert_eq!(text.matches("\x1b[2L").count(), 1);
    assert_eq!(text.matches("\x1b[1L").count(), 1);
    let pieces: Vec<&str> = text
        .split("\x1b[1;3r")
        .skip(1)
        .map(|piece| {
            let start = piece.find('H').map(|i| i + 1).unwrap_or(0);
            let end = piece.find("\x1b[r").unwrap_or(piece.len());
            &piece[start..end]
        })
        .collect();
    assert_eq!(pieces, vec!["a\nb\n", "c\n", "d"]);
}

#[test]
fn lifted_frame_falls_back_to_the_scroll_region_on_a_one_row_page() {
    // 光标在第 0 行还要滚:抬不了,整段交给受限区滚动。
    let frame = b"a\nb";
    let (_, scrolls) = terminal_frame_layout_with_scrolls(frame, (0, 0), 20, Some(0));
    let mut transaction = Vec::new();
    queue_lifted_frame(&mut transaction, frame, (0, 0), 0, 0, &scrolls, 10).unwrap();
    assert_eq!(
        String::from_utf8(transaction).unwrap(),
        format!("\x1b[1;1r{}a\nb\x1b[r", move_to(0, 0))
    );
}
