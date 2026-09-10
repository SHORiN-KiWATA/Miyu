//! 工具与子代理的活动摘要。

use super::shared::*;
use crate::render::*;

#[test]
fn completed_tools_are_committed_per_call_instead_of_aggregated() {
    let mut renderer = StreamRenderer::new(
        ReasoningDisplayMode::Summary,
        ToolCallDisplayMode::Summary,
        false,
        true,
        10,
    );
    renderer.live_summary = false;

    renderer
        .write_tool_call("web_search", r#"{"query":"first subject"}"#)
        .unwrap();
    assert_eq!(
        renderer.tool_summary_text(),
        format!(
            "~ {}×1 {}\n  ↳ first subject",
            t("Web search", "网络搜索"),
            t("running", "运行中")
        )
    );
    renderer
        .write_tool_result("web_search", true, "{}")
        .unwrap();
    assert!(renderer.tool_stats.is_empty());

    renderer
        .write_tool_call("web_search", r#"{"query":"second subject"}"#)
        .unwrap();
    assert_eq!(
        renderer.tool_summary_text(),
        format!(
            "~ {}×1 {}\n  ↳ second subject",
            t("Web search", "网络搜索"),
            t("running", "运行中")
        )
    );
}

#[test]
fn tool_summary_uses_spinner_and_updates_subagent_elapsed_time() {
    let mut renderer = StreamRenderer::new(
        ReasoningDisplayMode::Summary,
        ToolCallDisplayMode::Summary,
        false,
        true,
        10,
    );
    renderer.live_summary = true;

    renderer
        .write_tool_call(
            "task",
            r#"{"description":"确认工作区环境","prompt":"details"}"#,
        )
        .unwrap();

    assert!(renderer.wait_spinner.is_some());
    assert!(!renderer.summary_line_active);
    assert_eq!(
        renderer.tool_summary_text(),
        format!(
            "~ {}×1 {} · 0s\n  ↳ 确认工作区环境",
            t("Subagent", "子代理"),
            t("running", "运行中")
        )
    );
    renderer.tool_stats.get_mut("task").unwrap().started_at =
        Some(std::time::Instant::now() - std::time::Duration::from_secs(2));
    renderer.tick_spinner().unwrap();
    assert_eq!(
        renderer.tool_summary_text(),
        format!(
            "~ {}×1 {} · 2s\n  ↳ 确认工作区环境",
            t("Subagent", "子代理"),
            t("running", "运行中")
        )
    );
}

#[test]
fn subagent_summary_keeps_current_internal_tool_without_raw_reasoning() {
    let mut renderer = StreamRenderer::new(
        ReasoningDisplayMode::Full,
        ToolCallDisplayMode::Summary,
        false,
        true,
        10,
    );
    renderer.live_summary = false;
    renderer
        .write_tool_call(
            "task",
            r#"{"description":"查询磁盘占用","prompt":"details"}"#,
        )
        .unwrap();
    renderer
        .write_tool_progress("task", "工具 #2：运行命令 · du -sh /home/shorin/* 运行中")
        .unwrap();
    renderer
        .write_tool_progress("task", "__subagent_reasoning__private analysis")
        .unwrap();

    let summary = renderer.tool_summary_text();
    assert!(summary.contains("↳ 查询磁盘占用"));
    assert!(summary.contains("↳ 工具 #2：运行命令 · du -sh /home/shorin/* 运行中"));
    assert!(!summary.contains("private analysis"));
    assert_eq!(renderer.subagent_mode, None);
}

#[test]
fn external_output_clears_every_active_summary_row() {
    let mut renderer = StreamRenderer::new(
        ReasoningDisplayMode::Summary,
        ToolCallDisplayMode::Summary,
        false,
        true,
        10,
    );
    renderer.live_summary = true;
    renderer.summary_line_active = true;
    renderer.summary_lines_active = 2;

    renderer.prepare_for_external_output().unwrap();

    assert!(!renderer.summary_line_active);
    assert_eq!(renderer.summary_lines_active, 0);
}

#[test]
fn tool_status_prefers_running_for_single_active_call() {
    let stats = ToolStats {
        calls: 1,
        ok: 0,
        error: 0,
        subject: None,
        progress: None,
        final_progress: None,
        ..ToolStats::default()
    };
    assert_eq!(
        tool_status_text("grep", &stats, false),
        format!("grep×1 {}", t("running", "运行中"))
    );
}

#[test]
fn tool_status_uses_simple_single_success() {
    let stats = ToolStats {
        calls: 1,
        ok: 1,
        error: 0,
        subject: None,
        progress: None,
        final_progress: None,
        ..ToolStats::default()
    };
    assert_eq!(tool_status_text("grep", &stats, false), "grep×1 ok");
}

#[test]
fn detached_subagents_drop_the_meaningless_elapsed_timer() {
    let finished = ToolStats {
        calls: 1,
        ok: 1,
        elapsed: Some(std::time::Duration::from_secs(12)),
        ..ToolStats::default()
    };
    assert_eq!(
        tool_status_text("子代理", &finished, true),
        "子代理×1 ok · 12s"
    );

    // Handing off to the background returns immediately, so the timer only
    // ever read `0s` — which looked like the work had finished instantly.
    let detached = ToolStats {
        calls: 1,
        ok: 1,
        elapsed: Some(std::time::Duration::from_millis(3)),
        detached: true,
        ..ToolStats::default()
    };
    assert_eq!(tool_status_text("子代理", &detached, true), "子代理×1 ok");
}

#[test]
fn tool_status_subagent_tool_keeps_count_suffix() {
    let stats = ToolStats {
        calls: 1,
        ok: 0,
        error: 0,
        subject: None,
        progress: None,
        final_progress: None,
        ..ToolStats::default()
    };
    assert_eq!(
        tool_status_text("deep_research", &stats, true),
        format!("deep_research×1 {}", t("running", "运行中"))
    );
    let stats = ToolStats {
        calls: 1,
        ok: 1,
        error: 0,
        subject: None,
        progress: None,
        final_progress: None,
        ..ToolStats::default()
    };
    assert_eq!(
        tool_status_text("deep_research", &stats, true),
        "deep_research×1 ok"
    );
}

#[test]
fn subagent_status_shows_live_and_frozen_elapsed_time() {
    let running = ToolStats {
        calls: 1,
        started_at: Some(std::time::Instant::now() - std::time::Duration::from_secs(68)),
        ..ToolStats::default()
    };
    assert_eq!(
        tool_status_text("task", &running, true),
        format!("task×1 {} · 1m 08s", t("running", "运行中"))
    );
    assert_eq!(
        tool_status_text("task", &running, false),
        format!("task×1 {}", t("running", "运行中"))
    );

    let completed = ToolStats {
        calls: 1,
        ok: 1,
        elapsed: Some(std::time::Duration::from_secs(3_720)),
        ..ToolStats::default()
    };
    assert_eq!(
        tool_status_text("deep_research", &completed, true),
        "deep_research×1 ok · 1h 02m"
    );
}

#[test]
fn elapsed_time_formats_seconds_minutes_and_hours() {
    assert_eq!(format_elapsed(std::time::Duration::from_secs(5)), "5s");
    assert_eq!(format_elapsed(std::time::Duration::from_secs(65)), "1m 05s");
    assert_eq!(
        format_elapsed(std::time::Duration::from_secs(7_380)),
        "2h 03m"
    );
}

#[test]
fn full_mode_subagent_result_uses_elapsed_status_and_clears_timer() {
    let mut renderer = StreamRenderer::new(
        ReasoningDisplayMode::Summary,
        ToolCallDisplayMode::Full,
        false,
        true,
        10,
    );
    renderer.live_summary = false;
    renderer
        .write_tool_call("task", r#"{"description":"计时","prompt":"details"}"#)
        .unwrap();
    renderer.tool_stats.get_mut("task").unwrap().started_at =
        Some(std::time::Instant::now() - std::time::Duration::from_secs(5));

    renderer.write_tool_result("task", true, "{}").unwrap();

    assert!(!renderer.tool_stats.contains_key("task"));
    assert_eq!(
        tool_result_status("ok", Some(std::time::Duration::from_secs(5))),
        "ok · 5s"
    );
}

#[test]
fn tool_summary_suppresses_subagent_reasoning_even_when_reasoning_is_full() {
    let mut renderer = StreamRenderer::new(
        ReasoningDisplayMode::Full,
        ToolCallDisplayMode::Summary,
        false,
        true,
        10,
    );
    renderer.live_summary = false;
    renderer
        .write_tool_call("task", r#"{"description":"分析问题","prompt":"details"}"#)
        .unwrap();

    renderer
        .write_tool_progress("task", "__subagent_reasoning__Inspecting state")
        .unwrap();

    let stats = renderer.tool_stats.get("task").unwrap();
    assert_eq!(stats.calls, 1);
    assert!(stats.started_at.is_some());
    assert_eq!(renderer.subagent_mode, None);
}

#[test]
fn tool_summary_keeps_final_subagent_stats() {
    let mut renderer = StreamRenderer::new(
        ReasoningDisplayMode::Summary,
        ToolCallDisplayMode::Summary,
        true,
        true,
        10,
    );
    renderer.tool_stats.insert(
        "deep_research".to_string(),
        ToolStats {
            calls: 1,
            ok: 1,
            error: 0,
            subject: None,
            progress: None,
            final_progress: Some("工具调用 1 次　消耗词元 2.3K".to_string()),
            ..ToolStats::default()
        },
    );

    assert_eq!(
        renderer.tool_summary_text(),
        format!(
            "~ {}×1 ok\n  ✓ 工具调用 1 次　消耗词元 2.3K",
            t("Deep research", "深度研究")
        )
    );
}

#[test]
fn task_summary_omits_tool_prefix() {
    let mut renderer = StreamRenderer::new(
        ReasoningDisplayMode::Summary,
        ToolCallDisplayMode::Summary,
        true,
        true,
        10,
    );
    renderer.tool_stats.insert(
        "task".to_string(),
        ToolStats {
            calls: 1,
            ok: 0,
            error: 0,
            subject: Some("定位活动摘要渲染链路".to_string()),
            progress: None,
            final_progress: None,
            ..ToolStats::default()
        },
    );

    let header = format!("~ {}×1 {}", t("Subagent", "子代理"), t("running", "运行中"));
    assert_eq!(renderer.tool_summary_header(), header);
    assert_eq!(
        renderer.tool_summary_text(),
        format!("{header}\n  ↳ 定位活动摘要渲染链路")
    );
}

#[test]
fn parallel_subagents_render_stacked_blocks() {
    let mut renderer = StreamRenderer::new(
        ReasoningDisplayMode::Summary,
        ToolCallDisplayMode::Summary,
        true,
        true,
        10,
    );
    for (name, subject, progress) in [
        ("task:任务A", "任务A", Some("工具 #1: 运行命令")),
        ("task:任务B", "任务B", None),
        ("task:任务C", "任务C", Some("正在搜索")),
    ] {
        renderer.tool_stats.insert(
            name.to_string(),
            ToolStats {
                calls: 1,
                ok: 0,
                error: 0,
                subject: Some(subject.to_string()),
                progress: progress.map(str::to_string),
                final_progress: None,
                ..ToolStats::default()
            },
        );
    }
    let (phase, sub) = renderer.tool_summary_live();
    // Block mode: no shared phase line — every subagent is its own block.
    assert_eq!(phase, "");
    let sub = sub.expect("stacked blocks present");
    let marker = wait_spinner::BLOCK_MARKER;
    let lines: Vec<&str> = sub.lines().collect();
    // Each running block header carries the spinner marker; its own
    // progress follows; blank lines separate blocks. The redundant
    // subject line (same as the description in the header) is dropped.
    assert!(lines[0].starts_with(marker) && lines[0].contains("任务A"));
    assert_eq!(lines[1], "  ↳ 工具 #1: 运行命令");
    assert_eq!(lines[2], "");
    assert!(lines[3].starts_with(marker) && lines[3].contains("任务B"));
    assert_eq!(lines[4], "");
    assert!(lines[5].starts_with(marker) && lines[5].contains("任务C"));
    assert_eq!(lines[6], "  ↳ 正在搜索");
    assert_eq!(lines.len(), 7);
}

#[test]
fn live_blocks_freeze_settled_subagents_in_place() {
    let mut renderer = StreamRenderer::new(
        ReasoningDisplayMode::Summary,
        ToolCallDisplayMode::Summary,
        true,
        true,
        10,
    );
    renderer.tool_stats.insert(
        "task:任务A".to_string(),
        ToolStats {
            calls: 1,
            subject: Some("任务A".to_string()),
            progress: Some("正在搜索".to_string()),
            ..ToolStats::default()
        },
    );
    renderer.tool_stats.insert(
        "task:任务B".to_string(),
        ToolStats {
            calls: 1,
            ok: 1,
            subject: Some("任务B".to_string()),
            final_progress: Some("工具调用 1 次".to_string()),
            ..ToolStats::default()
        },
    );
    let (phase, sub) = renderer.tool_summary_live();
    assert_eq!(phase, "");
    let sub = sub.expect("blocks present");
    let marker = wait_spinner::BLOCK_MARKER;
    let lines: Vec<&str> = sub.lines().collect();
    // Running block keeps its animated marker + indented live progress…
    assert!(lines[0].starts_with(marker) && lines[0].contains("任务A"));
    assert_eq!(lines[1], "  ↳ 正在搜索");
    assert_eq!(lines[2], "");
    // …while the settled block drops the spinner glyph from its header;
    // detail lines stay two columns in, matching the committed layout.
    assert!(lines[3].starts_with("~ ") && lines[3].contains("任务B"));
    assert!(lines[3].contains("ok"));
    assert_eq!(lines[4], "  ✓ 工具调用 1 次");
    assert_eq!(lines.len(), 5);
}

#[test]
fn committed_summary_keeps_block_headers_when_one_subagent_finishes() {
    let mut renderer = StreamRenderer::new(
        ReasoningDisplayMode::Summary,
        ToolCallDisplayMode::Summary,
        true,
        true,
        10,
    );
    renderer.tool_stats.insert(
        "task:任务A".to_string(),
        ToolStats {
            calls: 1,
            subject: Some("任务A".to_string()),
            ..ToolStats::default()
        },
    );
    renderer.tool_stats.insert(
        "task:任务B".to_string(),
        ToolStats {
            calls: 1,
            ok: 1,
            subject: Some("任务B".to_string()),
            final_progress: Some("工具调用 1 次".to_string()),
            ..ToolStats::default()
        },
    );
    let text = renderer.tool_summary_text();
    let lines: Vec<&str> = text.lines().collect();
    // Each block keeps its own "~" header; a blank line separates blocks.
    assert!(lines[0].starts_with("~ ") && lines[0].contains("任务A"));
    assert_eq!(lines[1], "");
    assert!(lines[2].starts_with("~ ") && lines[2].contains("任务B"));
    assert_eq!(lines[3], "  ✓ 工具调用 1 次");
    assert_eq!(lines.len(), 4);
}

#[test]
fn all_subagent_summaries_use_activity_prefix() {
    for name in ["task", "deep_research"] {
        let mut renderer = StreamRenderer::new(
            ReasoningDisplayMode::Summary,
            ToolCallDisplayMode::Summary,
            true,
            true,
            10,
        );
        renderer.tool_stats.insert(
            name.to_string(),
            ToolStats {
                calls: 1,
                ok: 0,
                error: 0,
                subject: None,
                progress: None,
                final_progress: None,
                ..ToolStats::default()
            },
        );

        assert_eq!(
            renderer.tool_summary_header(),
            format!(
                "~ {}×1 {}",
                readable_tool_name(name),
                t("running", "运行中")
            )
        );
    }
}

#[test]
fn load_tools_keeps_targets_on_the_status_line() {
    let mut renderer = StreamRenderer::new(
        ReasoningDisplayMode::Summary,
        ToolCallDisplayMode::Summary,
        false,
        true,
        10,
    );
    renderer.tool_stats.insert(
        "load_tools:web_search,get_weather".to_string(),
        ToolStats {
            calls: 1,
            ok: 1,
            subject: Some("网络搜索、天气查询".to_string()),
            ..ToolStats::default()
        },
    );

    assert_eq!(
        renderer.tool_summary_text(),
        format!("~ {}×1 ok · 网络搜索、天气查询", t("Load", "加载"))
    );
    assert!(!renderer.tool_summary_text().contains("\n↳"));
}

#[test]
fn tool_status_counts_mixed_multiple_calls() {
    let stats = ToolStats {
        calls: 3,
        ok: 1,
        error: 1,
        subject: None,
        progress: None,
        final_progress: None,
        ..ToolStats::default()
    };
    assert_eq!(
        tool_status_text("grep", &stats, false),
        format!("grep×3 {}:1 ok:1 err:1", t("running", "运行中"))
    );
}

#[test]
fn trash_subject_counts_items_but_names_a_lone_path() {
    assert_eq!(
        tool_subject("trash_path", r#"{"paths":["/tmp/a","/tmp/b","/tmp/c"]}"#).as_deref(),
        Some(t("3 items", "3 项"))
    );
    // 只删一个时路径比「1 项」有用。
    assert_eq!(
        tool_subject("trash_path", r#"{"paths":["/tmp/only.txt"]}"#).as_deref(),
        Some("/tmp/only.txt")
    );
    assert_eq!(tool_subject("trash_path", r#"{"paths":[]}"#), None);
}

/// 失败清单自带 `✗`,不能再被套一个 `✓` 变成「✓ ✗ 权限不足」。
#[test]
fn final_progress_keeps_lines_that_carry_their_own_marker() {
    let renderer = StreamRenderer::new(
        ReasoningDisplayMode::Summary,
        ToolCallDisplayMode::Summary,
        false,
        true,
        10,
    );
    let stats = ToolStats {
        calls: 1,
        ok: 1,
        final_progress: Some("✗ /etc/hosts  权限不足\n收尾完成".to_string()),
        ..Default::default()
    };
    let lines = renderer.tool_block_lines("trash_path", &stats, false);
    assert!(
        lines
            .iter()
            .any(|line| line.trim() == "✗ /etc/hosts  权限不足"),
        "失败行应原样保留：{lines:?}"
    );
    assert!(
        lines.iter().any(|line| line.trim() == "✓ 收尾完成"),
        "普通行仍要套 ✓：{lines:?}"
    );
}

#[test]
fn tool_subject_extracts_safe_operation_targets() {
    assert_eq!(
        tool_subject("web_search", r#"{"query":"OpenCode 工具摘要"}"#).as_deref(),
        Some("OpenCode 工具摘要")
    );
    assert_eq!(
        tool_subject(
            "task",
            r#"{"description":"定位渲染链路","prompt":"private details"}"#
        )
        .as_deref(),
        Some("定位渲染链路")
    );
    assert_eq!(
        tool_subject("grep", r#"{"pattern":"ToolStats","path":"src"}"#).as_deref(),
        Some("ToolStats · src")
    );
    assert_eq!(
        tool_subject("run_command", r#"{"command":"du -sh /home/shorin/*"}"#).as_deref(),
        Some("du -sh /home/shorin/*")
    );
    let expected_load_tools_subject = format!(
        "{}{}{}",
        t("Web search", "网络搜索"),
        t(", ", "、"),
        t("Exchange rates", "汇率查询")
    );
    assert_eq!(
        tool_subject(
            "load_tools:web_search,get_exchange_rate",
            r#"{"names":["web_search","get_exchange_rate"]}"#
        )
        .as_deref(),
        Some(expected_load_tools_subject.as_str())
    );
}

#[test]
fn read_file_subject_shows_the_page_range() {
    assert_eq!(
        tool_subject("read_file", r#"{"path":"/tmp/a.rs"}"#).as_deref(),
        Some("/tmp/a.rs")
    );
    assert_eq!(
        tool_subject(
            "read_file",
            r#"{"path":"/tmp/a.rs","offset":2001,"limit":2000}"#
        )
        .as_deref(),
        Some("/tmp/a.rs (L2001-4000)")
    );
    assert_eq!(
        tool_subject("read_file", r#"{"path":"/tmp/a.rs","limit":500}"#).as_deref(),
        Some("/tmp/a.rs (L1-500)")
    );
    assert_eq!(
        tool_subject("read_file", r#"{"path":"/tmp/a.rs","offset":300}"#).as_deref(),
        Some("/tmp/a.rs (L300+)")
    );
}

#[test]
fn tool_subject_redacts_urls_and_ignores_unknown_arguments() {
    let subject = tool_subject(
        "web_fetch",
        r#"{"url":"https://user:secret@example.com/path?token=hidden#fragment"}"#,
    )
    .unwrap();
    assert_eq!(subject, "https://example.com/path");
    assert!(!subject.contains("secret"));
    assert!(!subject.contains("token"));
    assert_eq!(
        tool_subject("mcp_unknown", r#"{"password":"hidden","query":"private"}"#),
        None
    );
    assert_eq!(
        tool_subject(
            "web_search",
            r#"{"query":"查找 token=super-secret, Rust 文档"}"#
        )
        .as_deref(),
        Some("查找 token=[redacted], Rust 文档")
    );
    assert_eq!(
        safe_inline_subject(r#"请求 {"token":"super-secret"}"#).as_deref(),
        Some(r#"请求 {"token":"[redacted]"}"#)
    );
    assert_eq!(
        safe_inline_subject("Authorization Bearer super-secret").as_deref(),
        Some("Authorization [redacted]")
    );
    assert_eq!(
        safe_inline_subject("curl --password hunter2 https://example.com").as_deref(),
        Some("curl --password [redacted] https://example.com")
    );
    assert_eq!(
        safe_inline_subject("Bearer ghp_super-secret next").as_deref(),
        Some("Bearer [redacted] next")
    );
    assert_eq!(
        safe_inline_subject("curl --password\nhunter2 https://example.com").as_deref(),
        Some("curl --password [redacted] https://example.com")
    );
    assert_eq!(
        safe_inline_subject("Bearer\nghp_super-secret next").as_deref(),
        Some("Bearer [redacted] next")
    );
    assert_eq!(
        safe_inline_subject("AWS_SECRET_ACCESS_KEY=super-secret command").as_deref(),
        Some("AWS_SECRET_ACCESS_KEY=[redacted]")
    );
    assert_eq!(
        safe_inline_subject("AWS_ACCESS_KEY_ID=AKIAEXAMPLE command").as_deref(),
        Some("AWS_ACCESS_KEY_ID=[redacted]")
    );
    assert_eq!(
        safe_inline_subject("password hunter2").as_deref(),
        Some("password [redacted]")
    );
}

#[test]
fn tool_subject_is_single_line_and_terminal_safe() {
    let subject = tool_subject("web_search", "{\"query\":\"safe\\ntext\\u001b[2J\"}").unwrap();
    assert_eq!(subject, "safe text");
}

#[test]
fn only_the_show_action_of_use_meme_is_silent() {
    assert!(is_silent_tool("use_meme:show"));
    // use_meme 里只有 show 静默
    assert!(!is_silent_tool("use_meme:search"));
    assert!(!is_silent_tool("use_meme"));
    assert!(!is_silent_tool("manage_meme"));
}

#[test]
fn readable_tool_names_translate_known_tools_and_fallback_unknown() {
    for (name, english, chinese) in [
        ("deep_research", "Deep research", "深度研究"),
        ("read_file", "Read file", "读取文件"),
        ("check_issue", "Check issue", "检查问题"),
        ("check_os_info", "System information", "查看系统信息"),
        ("get_exchange_rate", "Exchange rates", "汇率查询"),
        ("vision_analyze", "Visual analysis", "视觉分析"),
        ("use_meme", "Meme", "表情包"),
        ("manage_meme", "Manage memes", "管理表情包"),
        ("task", "Subagent", "子代理"),
        (
            "upload_text_to_knowledge_base",
            "Import knowledge base",
            "导入知识库",
        ),
        (
            "search_evicted_context",
            "Search old context",
            "搜索旧上下文",
        ),
        ("aur", "AUR query", "AUR 查询"),
        ("install_aur_package", "Install AUR package", "安装 AUR 包"),
        ("manage_skill", "Manage skills", "管理技能"),
        ("recall_memories", "Recall memories", "召回记忆"),
    ] {
        assert_eq!(readable_tool_name(name), t(english, chinese), "{name}");
    }
    assert_eq!(readable_tool_name("custom_skill"), "custom_skill");
}

#[test]
fn summary_styles_distinguish_reasoning_from_tools() {
    assert_eq!(
        style_summary_text("工具", SummaryStyle::Tool),
        "\x1b[2m工具\x1b[0m"
    );
    assert_eq!(
        style_summary_text("思考", SummaryStyle::Reasoning),
        "\x1b[38;5;10m思考\x1b[0m"
    );
}

#[test]
fn ordinary_activity_summaries_have_one_blank_line_without_leading_gap() {
    let mut output = Vec::new();
    write_activity_summary(&mut output, "思考摘要", SummaryStyle::Reasoning).unwrap();
    write_activity_summary(&mut output, "~ 工具×1 ok", SummaryStyle::Tool).unwrap();
    let output = strip_ansi_for_test(&String::from_utf8(output).unwrap());

    assert_eq!(output, "思考摘要\n\n~ 工具×1 ok\n\n");
    assert!(!output.starts_with('\n'));
}

#[test]
fn reasoning_summary_reserves_one_blank_line_before_subagent_activity() {
    let mut output = Vec::new();
    write_activity_summary(
        &mut output,
        "思考 · 59 词元 · 2.5s",
        SummaryStyle::Reasoning,
    )
    .unwrap();
    write!(output, "~ Linux 游戏兼容性调查×1 运行中").unwrap();
    let output = strip_ansi_for_test(&String::from_utf8(output).unwrap());

    assert_eq!(
        output,
        "思考 · 59 词元 · 2.5s\n\n~ Linux 游戏兼容性调查×1 运行中"
    );
}

#[test]
fn reasoning_live_text_updates_title_tokens_and_precise_elapsed_time() {
    let mut renderer = StreamRenderer::new(
        ReasoningDisplayMode::Summary,
        ToolCallDisplayMode::Summary,
        true,
        true,
        10,
    );
    renderer.reasoning_title = Some("The user is asking \"你确定\"".to_string());
    renderer.record_reasoning_text("Inspecting the current implementation.");
    renderer.reasoning_started_at =
        Some(std::time::Instant::now() - std::time::Duration::from_millis(11_700));

    let expected = crate::token_estimate::estimate_tokens(&renderer.reasoning_text);
    let title_separator = t(": ", "：");
    assert_eq!(
        renderer.reasoning_live_text(),
        format!(
            "{}{title_separator}The user is asking \"你确定\" · {expected} {} · 11.7s",
            t("thinking", "思考"),
            t("tokens", "词元")
        )
    );
}

#[test]
fn tool_preparing_announces_every_slow_argument_tool() {
    let phase_for_batch = |name: &str, batch: bool| {
        let mut renderer = StreamRenderer::new(
            ReasoningDisplayMode::Summary,
            ToolCallDisplayMode::Summary,
            false,
            true,
            10,
        );
        renderer.use_external_cursor_control();
        renderer.use_buffered_output();
        // No TTY under test, so the spinner degrades to a summary line —
        // which is gated on the same flag a real terminal would set.
        renderer.live_summary = true;
        renderer.write_tool_preparing(name, batch).unwrap();
        String::from_utf8_lossy(&renderer.take_output_frame()).into_owned()
    };
    let phase_for = |name: &str| phase_for_batch(name, false);

    // apply_artifact_patch used to fall through the label match and render
    // nothing even though the backend announced it.
    for name in ["apply_patch", "apply_artifact_patch", "write_file"] {
        let phase = phase_for(name);
        assert!(
            phase.contains(t("~ Preparing edit", "~ 准备编辑")),
            "{name}"
        );
        // Dim tool palette, not the green the model's thinking uses: a
        // tool is starting up here.
        assert!(phase.contains("\x1b[2m"), "{name}");
        assert!(!phase.contains("\x1b[38;5;10m"), "{name}");
    }
    assert!(phase_for("run_command").contains(t("~ Preparing command", "~ 准备执行")));
    assert!(phase_for("trash_path").contains(t("~ Preparing delete", "~ 准备删除")));
    assert!(phase_for("todowrite").contains(t("~ Preparing list", "~ 准备清单")));
    assert!(phase_for("read_file").is_empty());

    // 同一条消息里的第 2+ 个调用:每个工具单看都不够慢,但参数接连流完
    // 的静默窗口和一次大 patch 一样长,所以退到通用提示而不是空白。
    assert!(phase_for_batch("read_file", true).contains(t("~ Preparing tools", "~ 准备工具")));
    // 工具自己的提示更具体,批量时也不该被通用的顶掉。
    assert!(phase_for_batch("run_command", true).contains(t("~ Preparing command", "~ 准备执行")));
}

/// Regression: the hint above is announced mid-turn, when a reasoning
/// spinner is already up and earlier tools have filled `tool_stats`. Every
/// tick re-derives the phase from renderer state, so pushing the text into
/// the spinner was not enough — the tool summary overwrote it inside the
/// very `tick_spinner` that `ensure_waiting_phase` performs, and the hint
/// never reached the screen for anything except `ask_question` (which has
/// its own sticky flag).
#[test]
fn tool_preparing_survives_the_tick_that_re_derives_the_phase() {
    let mut renderer = StreamRenderer::new(
        ReasoningDisplayMode::Summary,
        ToolCallDisplayMode::Summary,
        false,
        true,
        10,
    );
    renderer.use_external_cursor_control();
    renderer.use_buffered_output();
    renderer.live_summary = true;
    // Mid-turn state: the model has been thinking and already ran a tool.
    renderer.reasoning_started_at =
        Some(std::time::Instant::now() - std::time::Duration::from_secs(30));
    renderer.tool_stats_entry("read_file").calls += 1;

    renderer.write_tool_preparing("run_command", false).unwrap();
    assert!(renderer
        .waiting_phase_text()
        .starts_with(t("~ Preparing command · ", "~ 准备执行 · ")));
    renderer.last_tick = None;
    renderer.tick_spinner().unwrap();
    assert!(
        renderer
            .waiting_phase_text()
            .contains(t("Preparing command", "准备执行")),
        "a tick must not hand the spinner back to the tool summary"
    );

    // The arguments arrived: the hint steps aside for the tool summary.
    renderer.write_tool_call("run_command", "{}").unwrap();
    assert!(renderer.tool_preparing.is_none());
    assert!(!renderer
        .waiting_phase_text()
        .contains(t("Preparing command", "准备执行")));
}

/// claude-code 原生 Bash 与 run_command 同属命令家族:进 CommandLiveDisplay
/// (带色命令行+输出尾巴),而不是通用的「Bash×1 ok」摘要行——用户点名两者
/// 的命令颜色与输出块必须一致。
#[test]
fn native_bash_routes_through_the_command_display() {
    let mut renderer = StreamRenderer::new(
        ReasoningDisplayMode::Summary,
        ToolCallDisplayMode::Summary,
        false,
        true,
        10,
    );
    renderer.use_external_cursor_control();
    renderer.use_buffered_output();
    renderer.live_summary = true;
    renderer
        .write_tool_call("Bash", r#"{"command":"echo relay-proof"}"#)
        .unwrap();
    let display = renderer
        .command_display
        .as_ref()
        .expect("Bash must open a live command display, not a summary line");
    assert_eq!(display.command, "echo relay-proof");
    renderer
        .write_command_output(
            "Bash",
            crate::tools::CommandOutputStream::Stdout,
            b"relay-proof\n",
        )
        .unwrap();
    renderer
        .write_tool_result("Bash", true, "relay-proof")
        .unwrap();
    assert!(renderer.command_display.is_none());
    let frame = String::from_utf8_lossy(&renderer.take_output_frame()).to_string();
    assert!(
        frame.contains("echo relay-proof"),
        "the committed block must carry the command line: {frame:?}"
    );
    assert!(
        frame.contains("relay-proof"),
        "the committed block must carry the captured output: {frame:?}"
    );
    assert!(
        renderer.tool_stats.is_empty(),
        "Bash must not also land in the generic tool summary"
    );
}
