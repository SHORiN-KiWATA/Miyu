//! 压后重建材料：候选选择、预算裁决、渲染稳定性、转录导出。

use crate::agent::compact_extras::*;
use crate::state::{ToolFlowCall, ToolFlowRound, Turn, TurnStatus};
use std::collections::HashSet;
use std::path::{Path, PathBuf};

fn call(name: &str, arguments: &str) -> ToolFlowCall {
    ToolFlowCall {
        id: format!("c-{name}-{}", arguments.len()),
        name: name.to_string(),
        arguments: arguments.to_string(),
        output: "ok".to_string(),
    }
}

fn turn(seq: i64, user: &str, calls: Vec<ToolFlowCall>) -> Turn {
    Turn {
        turn_id: format!("t{seq}"),
        seq,
        user_content: user.to_string(),
        display_content: user.to_string(),
        user_timestamp: "2026-09-09T00:00:00Z".to_string(),
        assistant_content: format!("reply {seq}"),
        assistant_reasoning: None,
        assistant_provider_id: None,
        assistant_model: None,
        assistant_timestamp: None,
        status: TurnStatus::Completed,
        tool_reports: Vec::new(),
        tool_flow: if calls.is_empty() {
            Vec::new()
        } else {
            vec![ToolFlowRound {
                remote: false,
                assistant_content: String::new(),
                assistant_reasoning: None,
                calls,
            }]
        },
        question_exchanges: Vec::new(),
        followups: Vec::new(),
        attachments: Vec::new(),
        hidden: false,
        is_summary: false,
        owner_pid: None,
        token_total: 0,
        token_prompt: 0,
        token_cache_read: 0,
        token_usage_estimated: false,
        revision: 0,
        journal_events: Vec::new(),
        context_messages: Vec::new(),
    }
}

fn policy(dir: &Path, workdir: &Path) -> CompactExtrasPolicy {
    CompactExtrasPolicy {
        restore_files: 5,
        restore_file_tokens: 4000,
        restore_total_tokens: 24000,
        export_transcript: true,
        read_tool_available: true,
        transcript_dir: dir.join("compact").join("session"),
        exclude_root: None,
        workdir: workdir.to_path_buf(),
    }
}

fn args(path: &str) -> String {
    serde_json::json!({ "path": path }).to_string()
}

#[test]
fn touched_files_orders_by_recency_and_dedups() {
    let workdir = PathBuf::from("/work");
    let turns = vec![
        turn(1, "one", vec![call("read", &args("a.rs"))]),
        turn(2, "two", vec![call("write_file", &args("b.rs"))]),
        turn(3, "three", vec![call("read", &args("a.rs"))]),
    ];
    let refs: Vec<&Turn> = turns.iter().collect();

    let all = touched_files(&refs, &workdir, false);
    assert_eq!(
        all,
        vec![PathBuf::from("/work/a.rs"), PathBuf::from("/work/b.rs")],
        "most recently touched first, deduplicated by resolved path"
    );

    let reads = touched_files(&refs, &workdir, true);
    assert_eq!(reads, vec![PathBuf::from("/work/a.rs")]);
}

#[test]
fn stub_shell_arguments_are_unwrapped() {
    let workdir = PathBuf::from("/work");
    let wrapped = serde_json::json!({ "arguments": { "path": "x.rs" } }).to_string();
    let turns = vec![turn(1, "one", vec![call("read", &wrapped)])];
    let refs: Vec<&Turn> = turns.iter().collect();
    assert_eq!(
        touched_files(&refs, &workdir, false),
        vec![PathBuf::from("/work/x.rs")]
    );
}

#[test]
fn unknown_tools_and_blank_paths_are_ignored() {
    let workdir = PathBuf::from("/work");
    let turns = vec![turn(
        1,
        "one",
        vec![
            call("run_command", &args("a.rs")),
            call("read", &args("   ")),
            call("read", "not json"),
        ],
    )];
    let refs: Vec<&Turn> = turns.iter().collect();
    assert!(touched_files(&refs, &workdir, false).is_empty());
}

#[test]
fn restore_applies_per_file_and_total_caps() {
    let temp = tempfile::tempdir().unwrap();
    let work = temp.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    std::fs::write(work.join("small.rs"), "fn main() {}\nlet x = 1;\n").unwrap();
    std::fs::write(work.join("huge.rs"), "x".repeat(200_000)).unwrap();
    std::fs::write(work.join("mid.rs"), "y\n".repeat(400)).unwrap();

    let mut policy = policy(temp.path(), &work);
    // huge.rs 200KB 超单文件上限（4000 tok × 8 字节预判）→ too large；
    // mid.rs 只有 800 字节、在单文件上限内，但顶不进剩下的总预算 → over budget。
    policy.restore_file_tokens = 4000;
    policy.restore_total_tokens = 100;

    let turns = vec![turn(
        1,
        "one",
        vec![
            call("read", &args("mid.rs")),
            call("read", &args("huge.rs")),
            call("read", &args("small.rs")),
        ],
    )];
    let refs: Vec<&Turn> = turns.iter().collect();
    let extras = build_compact_extras(&policy, "s1", &refs, &[], None, None);

    let by_name = |needle: &str| {
        extras
            .restored
            .iter()
            .find(|file| file.path.ends_with(needle))
            .unwrap_or_else(|| panic!("{needle} missing from {:?}", extras.restored))
    };
    let small = by_name("small.rs");
    assert_eq!(small.status, RestoreStatus::Included);
    assert_eq!(small.lines, 2);
    assert!(small.content.starts_with("1: fn main() {}\n"));
    assert_eq!(by_name("huge.rs").status, RestoreStatus::TooLarge);
    assert!(by_name("huge.rs").content.is_empty());
    assert_eq!(by_name("mid.rs").status, RestoreStatus::OverBudget);
    assert!(by_name("mid.rs").content.is_empty());
    assert_eq!(extras.included_files(), 1);
}

#[test]
fn restore_skips_tail_reads_excluded_root_and_missing() {
    let temp = tempfile::tempdir().unwrap();
    let work = temp.path().join("work");
    let root = temp.path().join("miyu-home");
    std::fs::create_dir_all(&work).unwrap();
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(work.join("kept.rs"), "kept\n").unwrap();
    std::fs::write(work.join("in-tail.rs"), "tail\n").unwrap();
    std::fs::write(root.join("persona.md"), "persona\n").unwrap();
    std::fs::write(work.join("binary.bin"), [b'a', 0u8, b'b']).unwrap();

    let mut policy = policy(temp.path(), &work);
    policy.exclude_root = Some(root.clone());
    policy.export_transcript = false;

    let fold = vec![turn(
        1,
        "one",
        vec![
            call("read", &args("in-tail.rs")),
            call("read", &args(root.join("persona.md").to_str().unwrap())),
            call("read", &args("gone.rs")),
            call("read", &args("binary.bin")),
            call("read", &args("kept.rs")),
        ],
    )];
    let tail = vec![turn(2, "two", vec![call("read", &args("in-tail.rs"))])];
    let fold_refs: Vec<&Turn> = fold.iter().collect();
    let tail_refs: Vec<&Turn> = tail.iter().collect();

    let extras = build_compact_extras(&policy, "s1", &fold_refs, &tail_refs, None, None);
    let paths: Vec<&str> = extras
        .restored
        .iter()
        .map(|file| file.path.as_str())
        .collect();
    assert_eq!(paths.len(), 1, "unexpected restores: {paths:?}");
    assert!(paths[0].ends_with("kept.rs"));
    assert!(extras.transcripts.is_empty());
}

#[test]
fn restore_is_off_when_the_file_budget_is_zero() {
    let temp = tempfile::tempdir().unwrap();
    let work = temp.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    std::fs::write(work.join("a.rs"), "a\n").unwrap();
    let mut policy = policy(temp.path(), &work);
    policy.restore_files = 0;
    policy.export_transcript = false;

    let fold = vec![turn(1, "one", vec![call("read", &args("a.rs"))])];
    let refs: Vec<&Turn> = fold.iter().collect();
    let extras = build_compact_extras(&policy, "s1", &refs, &[], None, None);
    assert!(extras.is_empty());
    assert!(extras.render().is_empty());
}

#[test]
fn render_is_deterministic_and_english() {
    let extras = CompactExtras {
        transcripts: vec![
            "/state/fold-2.md".to_string(),
            "/state/fold-1.md".to_string(),
        ],
        restored: vec![
            RestoredFile {
                path: "/work/a.rs".to_string(),
                status: RestoreStatus::Included,
                lines: 2,
                content: "1: a\n2: b\n".to_string(),
            },
            RestoredFile {
                path: "/work/big \"quoted\".rs".to_string(),
                status: RestoreStatus::TooLarge,
                lines: 0,
                content: String::new(),
            },
        ],
        read_hint: true,
    };
    let first = extras.render();
    assert_eq!(first, extras.render(), "rendering must be byte-stable");
    assert!(first.contains("<restored-files>"));
    assert!(first.contains("<file path=\"/work/a.rs\" lines=\"2\">\n1: a\n2: b\n</file>"));
    assert!(first.contains("status=\"too large\""));
    assert!(first.contains("&quot;quoted&quot;"), "{first}");
    assert!(first.contains("<compact-transcript>"));
    assert!(first.contains("Earlier folds: /state/fold-1.md."));
    assert!(first.contains("the read tool"));

    let mut quiet = extras.clone();
    quiet.read_hint = false;
    let rendered = quiet.render();
    assert!(!rendered.contains("read tool"), "{rendered}");
    assert!(rendered.contains("too large or over budget."));

    let single = CompactExtras {
        transcripts: vec!["/state/fold-2.md".to_string()],
        ..Default::default()
    };
    assert!(!single.render().contains("Earlier folds"));
}

#[test]
fn export_transcript_writes_previous_summary_and_turns() {
    let temp = tempfile::tempdir().unwrap();
    let work = temp.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    let mut policy = policy(temp.path(), &work);
    policy.restore_files = 0;

    let fold = vec![turn(
        1,
        "遗嘱：端口写死 7043",
        vec![call("read", &args("a.rs"))],
    )];
    let refs: Vec<&Turn> = fold.iter().collect();
    let extras = build_compact_extras(
        &policy,
        "quiz",
        &refs,
        &[],
        None,
        Some("## Standing Facts\n- 代号 HOSHIZORA-7"),
    );

    assert_eq!(extras.transcripts.len(), 1);
    let body = std::fs::read_to_string(&extras.transcripts[0]).unwrap();
    assert!(body.contains("## Previous checkpoint summary"));
    assert!(body.contains("代号 HOSHIZORA-7"));
    assert!(body.contains("遗嘱：端口写死 7043"));
    assert!(body.contains("#### read"));
}

#[test]
fn transcript_chain_keeps_the_previous_paths_newest_first() {
    let temp = tempfile::tempdir().unwrap();
    let work = temp.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    let mut policy = policy(temp.path(), &work);
    policy.restore_files = 0;

    let fold = vec![turn(1, "one", vec![])];
    let refs: Vec<&Turn> = fold.iter().collect();
    let previous = CompactExtras {
        transcripts: (1..=5).map(|i| format!("/old/fold-{i}.md")).collect(),
        ..Default::default()
    };
    let extras = build_compact_extras(&policy, "s1", &refs, &[], Some(&previous), None);

    assert_eq!(extras.transcripts.len(), 5, "chain is capped");
    assert!(extras.transcripts[0].contains("compact/session/fold-"));
    assert_eq!(extras.transcripts[1], "/old/fold-1.md");
    assert_eq!(extras.transcripts[4], "/old/fold-4.md");
}

#[test]
fn safe_path_segment_strips_separators() {
    assert_eq!(safe_path_segment("qq:group/123"), "qq_group_123");
    assert_eq!(safe_path_segment(&"x".repeat(80)).len(), 48);
}

#[test]
fn touched_files_ignores_remote_rounds() {
    let workdir = PathBuf::from("/work");
    let mut only = turn(1, "one", vec![call("read", &args("a.rs"))]);
    only.tool_flow[0].remote = true;
    let turns = vec![only];
    let refs: Vec<&Turn> = turns.iter().collect();
    assert!(touched_files(&refs, &workdir, false).is_empty());
}

#[test]
fn skip_set_uses_resolved_paths() {
    let workdir = PathBuf::from("/work");
    let turns = vec![turn(1, "one", vec![call("read", &args("./a.rs"))])];
    let refs: Vec<&Turn> = turns.iter().collect();
    let resolved = touched_files(&refs, &workdir, false);
    let skip: HashSet<PathBuf> = resolved.iter().cloned().collect();
    assert!(skip.contains(&PathBuf::from("/work/./a.rs")));
}
