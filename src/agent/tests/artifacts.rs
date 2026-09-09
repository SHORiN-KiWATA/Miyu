//! 产物识别与工具报告的持久化。

use crate::agent::*;

#[test]
fn artifact_delivery_detection_is_conservative() {
    assert!(artifact_delivery_requested(&[ChatMessage::plain(
        "user",
        "生成一个 Linux 游玩报告，保存为 Markdown 文件",
    )]));
    assert!(artifact_delivery_requested(&[ChatMessage::plain(
        "user",
        "create a standalone HTML file",
    )]));
    assert!(!artifact_delivery_requested(&[ChatMessage::plain(
        "user",
        "修改 src/main.rs 修复这个错误",
    )]));
}

#[test]
fn artifact_candidates_only_include_new_files() {
    let created = artifact_candidate_paths(
        "write_file",
        r#"{"ok":true,"created":true,"path":"report.md"}"#,
    );
    assert_eq!(created.len(), 1);
    assert!(artifact_candidate_paths(
        "write_file",
        r#"{"ok":true,"created":false,"path":"src/main.rs"}"#,
    )
    .is_empty());
    assert!(artifact_candidate_paths(
        "apply_patch",
        r#"{"ok":true,"files":[{"path":"report.md","operation":"update"}]}"#,
    )
    .is_empty());
}

#[test]
fn artifact_tool_report_keeps_cross_turn_filename_memory() {
    let report = extract_persistable_tool_report(
        "apply_artifact_patch",
        r#"{"ok":true,"files":[{"path":"report.md","operation":"update"}]}"#,
    )
    .unwrap();
    assert!(report.contains("report.md"));
    assert!(!report.contains("/home/test"));
}

#[test]
fn formats_dynamic_load_tool_names() {
    assert_eq!(
        tool_event_name("load_skill", r#"{"name":"web-search"}"#),
        "load_skill:web-search"
    );
    assert_eq!(
        tool_event_name("load_tools", r#"{"names":["get_weather","todoupdate"]}"#),
        "load_tools:get_weather,todoupdate"
    );
}

#[test]
fn restores_loaded_tools_from_previous_tool_report() {
    let messages = vec![ChatMessage::plain(
        "assistant",
        "<previous_tool_report name=\"load_tools\">\n{\"loaded_tools\":[\"get_weather\",\"todoupdate\"]}\n</previous_tool_report>",
    )];
    let loaded = loaded_tools_from_messages(&messages);
    assert!(loaded.contains("get_weather"));
    assert!(loaded.contains("todoupdate"));
}

#[test]
fn persists_loaded_tools_with_previous_tool_report_wrapper() {
    let output = serde_json::json!({
        "loaded_tools": [
            {"name": "get_weather"},
            {"name": "todoupdate"}
        ]
    })
    .to_string();

    assert_eq!(
        extract_persistable_tool_report("load_tools", &output).as_deref(),
        Some("<previous_tool_report name=\"load_tools\">\n{\"loaded_tools\":[\"get_weather\",\"todoupdate\"]}\n</previous_tool_report>")
    );
}

#[test]
fn tool_footprint_extracts_paths_and_memories() {
    let fp = tool_call_footprint("read_file", r#"{"path":"/tmp/a.txt"}"#).unwrap();
    assert!(fp.read.contains("/tmp/a.txt"));
    let fp = tool_call_footprint(
        "edit_string",
        r#"{"path":"b.rs","old_string":"x","new_string":"y"}"#,
    )
    .unwrap();
    assert!(fp.modified.contains("b.rs"));
    // stub-mode wrapped arguments unwrap
    let fp = tool_call_footprint(
        "write_file",
        r#"{"arguments":{"path":"c.md","content":"hi"}}"#,
    )
    .unwrap();
    assert!(fp.modified.contains("c.md"));
    let fp = tool_call_footprint("remember_fact", r#"{"content":"用户住在杭州"}"#).unwrap();
    assert!(fp.memories.contains("用户住在杭州"));
    assert!(tool_call_footprint("bash", r#"{"command":"ls"}"#).is_none());
}

#[test]
fn tool_footprint_reads_edit_patch_headers() {
    let patch = "*** Begin Patch\n*** Add File: /tmp/new.rs\n+x\n*** Update File: src/lib.rs\n@@ a\n-x\n+y\n*** Delete File: old.rs\n*** End Patch";
    let fp = tool_call_footprint(
        "edit",
        &serde_json::json!({ "patchText": patch }).to_string(),
    )
    .expect("edit patch must produce a footprint");
    assert!(fp.read.is_empty());
    assert!(fp.modified.contains("/tmp/new.rs"));
    assert!(fp.modified.contains("src/lib.rs"));
    assert!(fp.modified.contains("old.rs"));
}

#[test]
fn tool_footprint_skips_patch_domain_prefixes() {
    // kb:/artifact: 不是文件系统路径，别把它们塞进 modified。
    let patch = "*** Begin Patch\n*** Add File: kb:notes/a.md\n+x\n*** Add File: artifact:report.md\n+y\n*** End Patch";
    assert!(tool_call_footprint(
        "edit",
        &serde_json::json!({ "patchText": patch }).to_string()
    )
    .is_none());
}

#[test]
fn persists_compact_sent_meme_report() {
    let output = serde_json::json!({
        "success": true,
        "id": "sha256:abc123",
        "description": "猫猫\n开心 & <得意>",
        "unused": "ignored",
    })
    .to_string();

    assert_eq!(
        extract_persistable_tool_report("use_meme:show", &output).as_deref(),
        Some("<sent_meme>发送了一个表情包：id=sha256:abc123；description=猫猫 开心 &amp; &lt;得意&gt;</sent_meme>")
    );
}

#[test]
fn sent_meme_report_allows_missing_description() {
    let output = serde_json::json!({
        "success": true,
        "id": "sha256:abc123",
    })
    .to_string();

    assert_eq!(
        extract_persistable_tool_report("use_meme:show", &output).as_deref(),
        Some("<sent_meme>发送了一个表情包：id=sha256:abc123</sent_meme>")
    );
}

#[test]
fn sent_meme_report_skips_failed_result() {
    let output = serde_json::json!({
        "success": false,
        "id": "sha256:abc123",
        "description": "猫猫",
    })
    .to_string();

    assert!(extract_persistable_tool_report("use_meme:show", &output).is_none());
}
