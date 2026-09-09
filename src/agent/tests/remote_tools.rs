//! 中转线(claude-code / codex / agy)工具活动的收集:remote 轮与 footprint。

use crate::agent::turn_loop::record_remote_tool_chunk;
use crate::llm::{ChatStreamChunk, ChatStreamKind};
use crate::state::ToolFlowCall;
use std::sync::Mutex;

fn chunk(kind: ChatStreamKind, value: serde_json::Value) -> ChatStreamChunk {
    ChatStreamChunk {
        kind,
        text: value.to_string(),
    }
}

/// 中转轮的 footprint 在 Finished 且成功时才记,用 Started 存下的名字与参数算;
/// 失败的调用不记(与本地工具同一口径)。改前这条路根本不产出 footprint。
#[test]
fn remote_tool_footprint_lands_on_successful_finish() {
    let pending: Mutex<Vec<ToolFlowCall>> = Mutex::new(Vec::new());

    let started = chunk(
        ChatStreamKind::RemoteToolStarted,
        serde_json::json!({
            "id": "t1",
            "name": "Edit",
            "input": { "file_path": "src/lib.rs", "old_string": "a", "new_string": "b" }
        }),
    );
    assert!(
        record_remote_tool_chunk(&started, &pending).is_none(),
        "nothing is known about success at start"
    );

    let finished = chunk(
        ChatStreamKind::RemoteToolFinished,
        serde_json::json!({ "id": "t1", "name": "Edit", "ok": true, "output": "done" }),
    );
    let delta =
        record_remote_tool_chunk(&finished, &pending).expect("successful edit leaves a footprint");
    assert!(delta.modified.contains("src/lib.rs"));
    assert!(delta.read.is_empty());
    assert_eq!(pending.lock().unwrap()[0].output, "done");
}

#[test]
fn remote_tool_footprint_skips_failures_and_unknown_tools() {
    let pending: Mutex<Vec<ToolFlowCall>> = Mutex::new(Vec::new());
    for (id, name, input) in [
        ("f1", "Write", serde_json::json!({ "file_path": "gone.rs" })),
        ("b1", "Bash", serde_json::json!({ "command": "ls" })),
    ] {
        let started = chunk(
            ChatStreamKind::RemoteToolStarted,
            serde_json::json!({ "id": id, "name": name, "input": input }),
        );
        record_remote_tool_chunk(&started, &pending);
    }
    let failed = chunk(
        ChatStreamKind::RemoteToolFinished,
        serde_json::json!({ "id": "f1", "name": "Write", "ok": false, "output": "denied" }),
    );
    assert!(record_remote_tool_chunk(&failed, &pending).is_none());
    assert_eq!(pending.lock().unwrap()[0].output, "tool error: denied");

    let bash = chunk(
        ChatStreamKind::RemoteToolFinished,
        serde_json::json!({ "id": "b1", "name": "Bash", "ok": true, "output": "a b" }),
    );
    assert!(record_remote_tool_chunk(&bash, &pending).is_none());
}
