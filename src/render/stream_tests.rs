use super::*;

#[test]
fn tool_status_prefers_running_for_single_active_call() {
    let stats = ToolStats {
        calls: 1,
        ok: 0,
        error: 0,
        progress: None,
    };
    let output = tool_status_text("deep_research", &stats);
    assert!(matches!(
        output.as_str(),
        "deep_research×1 运行中" | "deep_research×1 running"
    ));
}

#[test]
fn tool_status_uses_simple_single_success() {
    let stats = ToolStats {
        calls: 1,
        ok: 1,
        error: 0,
        progress: None,
    };
    assert_eq!(
        tool_status_text("deep_research", &stats),
        "deep_research×1 ok"
    );
}

#[test]
fn tool_status_counts_mixed_multiple_calls() {
    let stats = ToolStats {
        calls: 3,
        ok: 1,
        error: 1,
        progress: None,
    };
    let output = tool_status_text("grep", &stats);
    assert!(matches!(
        output.as_str(),
        "grep×3 运行中:1 ok:1 err:1" | "grep×3 running:1 ok:1 err:1"
    ));
}

#[test]
fn summary_styles_distinguish_reasoning_from_tools() {
    assert_eq!(
        style_summary_text("工具", SummaryStyle::Tool),
        "\x1b[2m工具\x1b[0m"
    );
    assert_eq!(
        style_summary_text("思考", SummaryStyle::Reasoning),
        "\x1b[2m\x1b[36m思考\x1b[0m"
    );
}
