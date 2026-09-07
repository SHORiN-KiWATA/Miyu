//! 对外的 JSON 事件 schema(`--output-format json|stream-json` 与 `miyu stdio`)。
//!
//! 内部 AgentEvent / IPC 事件是给自家渲染器用的,字段随时会改;这里是**契约**:
//! 每行一个对象,`v` 版本号只增不减,类型只加不删、字段只加不改。纯 UI 事件
//! (spinner、排队光标)不出门。
//!
//! 与 IPC 事件的翻译在 [`PublicEvent::from_ipc`],和 `one_shot.rs` 的
//! IPC→AgentEvent 表并列——加 IPC 事件时两边都要过一遍。

use serde::Serialize;
use serde_json::Value;

pub const PUBLIC_EVENT_VERSION: u8 = 1;

/// 一行 JSON 的外壳:`{"v":1,"id":"…","type":"…",…}`。`id` 是宿主给的
/// 请求号(一次性调用没有则省略)。
#[derive(Debug, Serialize)]
pub struct Envelope<'a> {
    pub v: u8,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<&'a str>,
    #[serde(flatten)]
    pub event: &'a PublicEvent,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PublicEvent {
    /// stdio 模式就绪(daemon 已连上)。
    Ready {
        daemon_pid: u32,
        build_id: String,
    },
    /// 回合被受理。`turn_id` 在 daemon 落库后才有,可能晚于 started。
    Started {
        session_id: String,
        run_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        turn_id: Option<String>,
    },
    Text {
        delta: String,
    },
    Reasoning {
        delta: String,
    },
    Tool {
        phase: ToolPhase,
        call_id: String,
        name: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        arguments: Option<Value>,
        #[serde(skip_serializing_if = "Option::is_none")]
        ok: Option<bool>,
        #[serde(skip_serializing_if = "Option::is_none")]
        output: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        stream: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        message: Option<String>,
    },
    /// 工具产出的图片;`asset_id` 可用 `miyu tool-call` 或状态库取回。
    Image {
        call_id: String,
        name: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        asset_id: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        mime: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        alt: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
    /// 她在问宿主。stdio 模式用 `answer` 回;一次性调用自动关闭。
    Question {
        question_id: String,
        questions: Value,
    },
    Notice {
        level: NoticeLevel,
        message: String,
    },
    /// 一次模型请求结束后的计量快照。
    Usage {
        usage: Value,
        #[serde(skip_serializing_if = "Option::is_none")]
        model: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        provider_id: Option<String>,
        estimated: bool,
    },
    /// 回合完成。`text` 是最终正文(daemon 随终态发的,不靠 delta 累加)。
    Done {
        session_id: String,
        run_id: String,
        text: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        usage: Option<Value>,
        usage_estimated: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        model: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        provider_id: Option<String>,
        context_tokens: u64,
        #[serde(skip_serializing_if = "Option::is_none")]
        context_window: Option<u64>,
        elapsed_ms: u64,
    },
    Error {
        kind: ErrorKind,
        message: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        session_id: Option<String>,
    },
    /// 会话操作等非回合请求的结果。
    Result {
        ok: bool,
        data: Value,
    },
    Pong,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ToolPhase {
    Start,
    Progress,
    Output,
    End,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum NoticeLevel {
    Info,
    Warning,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ErrorKind {
    Usage,
    SessionNotFound,
    TurnFailed,
    Cancelled,
    Timeout,
    Disconnected,
}

impl ErrorKind {
    pub fn exit_code(self) -> i32 {
        use crate::cli::exit_code::*;
        match self {
            ErrorKind::Usage => EXIT_USAGE,
            ErrorKind::SessionNotFound => EXIT_SESSION_NOT_FOUND,
            ErrorKind::TurnFailed | ErrorKind::Disconnected => EXIT_FAILURE,
            ErrorKind::Cancelled => EXIT_CANCELLED,
            ErrorKind::Timeout => EXIT_TIMEOUT,
        }
    }
}

fn text(data: &Value, key: &str) -> String {
    data.get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

fn optional_text(data: &Value, key: &str) -> Option<String> {
    data.get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

impl PublicEvent {
    pub fn to_line(&self, id: Option<&str>) -> String {
        let envelope = Envelope {
            v: PUBLIC_EVENT_VERSION,
            id,
            event: self,
        };
        // 事件都是自家类型,序列化不会失败;真失败了也不能让宿主收到半行。
        serde_json::to_string(&envelope).unwrap_or_else(|error| {
            format!(
                "{{\"v\":{PUBLIC_EVENT_VERSION},\"type\":\"error\",\"kind\":\"turn_failed\",\"message\":{}}}",
                Value::String(format!("event serialization failed: {error}"))
            )
        })
    }

    /// IPC 事件 → 对外事件。回合终态(run.completed/failed/cancelled)由
    /// 回合运行器自己收,这里返回 None;不认识的也 None。
    pub fn from_ipc(kind: &str, data: &Value) -> Option<PublicEvent> {
        Some(match kind {
            "assistant.delta" => PublicEvent::Text {
                delta: text(data, "delta"),
            },
            "reasoning.delta" => PublicEvent::Reasoning {
                delta: text(data, "delta"),
            },
            "tool.started" => PublicEvent::Tool {
                phase: ToolPhase::Start,
                call_id: text(data, "tool_id"),
                name: text(data, "name"),
                arguments: data.get("arguments").cloned(),
                ok: None,
                output: None,
                stream: None,
                message: None,
            },
            "tool.progress" => PublicEvent::Tool {
                phase: ToolPhase::Progress,
                call_id: text(data, "tool_id"),
                name: text(data, "name"),
                arguments: None,
                ok: None,
                output: None,
                stream: None,
                message: optional_text(data, "message"),
            },
            "tool.output" => PublicEvent::Tool {
                phase: ToolPhase::Output,
                call_id: text(data, "tool_id"),
                name: text(data, "name"),
                arguments: None,
                ok: None,
                output: Some(text(data, "output")),
                stream: optional_text(data, "stream"),
                message: None,
            },
            "tool.finished" => PublicEvent::Tool {
                phase: ToolPhase::End,
                call_id: text(data, "tool_id"),
                name: text(data, "name"),
                arguments: None,
                ok: data.get("ok").and_then(Value::as_bool),
                output: Some(text(data, "output")),
                stream: None,
                message: None,
            },
            "tool.image" => {
                let asset = data.get("asset");
                PublicEvent::Image {
                    call_id: text(data, "tool_id"),
                    name: text(data, "name"),
                    asset_id: asset.and_then(|asset| optional_text(asset, "id")),
                    mime: asset.and_then(|asset| optional_text(asset, "mime")),
                    alt: asset.and_then(|asset| optional_text(asset, "alt")),
                    error: optional_text(data, "error"),
                }
            }
            "question.requested" => PublicEvent::Question {
                question_id: text(data, "question_id"),
                questions: data
                    .get("questions")
                    .cloned()
                    .unwrap_or(Value::Array(Vec::new())),
            },
            "context.notice" => PublicEvent::Notice {
                level: NoticeLevel::Info,
                message: text(data, "text"),
            },
            "context.compact_start" => PublicEvent::Notice {
                level: NoticeLevel::Info,
                message: "context compaction started".to_string(),
            },
            "context.compact_end" => PublicEvent::Notice {
                level: NoticeLevel::Info,
                message: "context compaction finished".to_string(),
            },
            "chat.round_usage" => PublicEvent::Usage {
                usage: data.get("usage").cloned().unwrap_or(Value::Null),
                model: optional_text(data, "model"),
                provider_id: optional_text(data, "provider_id"),
                estimated: data
                    .get("estimated")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
            },
            _ => return None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn envelope_carries_version_and_optional_id() {
        let line = PublicEvent::Text { delta: "hi".into() }.to_line(Some("r1"));
        let value: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(value["v"], 1);
        assert_eq!(value["id"], "r1");
        assert_eq!(value["type"], "text");
        assert_eq!(value["delta"], "hi");
        let line = PublicEvent::Pong.to_line(None);
        let value: Value = serde_json::from_str(&line).unwrap();
        assert!(value.get("id").is_none());
        assert_eq!(value["type"], "pong");
    }

    #[test]
    fn translates_tool_lifecycle() {
        let start = PublicEvent::from_ipc(
            "tool.started",
            &json!({"tool_id":"t1","name":"read","arguments":{"path":"a"}}),
        )
        .unwrap();
        assert_eq!(
            start,
            PublicEvent::Tool {
                phase: ToolPhase::Start,
                call_id: "t1".into(),
                name: "read".into(),
                arguments: Some(json!({"path":"a"})),
                ok: None,
                output: None,
                stream: None,
                message: None,
            }
        );
        let end = PublicEvent::from_ipc(
            "tool.finished",
            &json!({"tool_id":"t1","name":"read","ok":true,"output":"x"}),
        )
        .unwrap();
        let line = end.to_line(None);
        let value: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(value["phase"], "end");
        assert_eq!(value["ok"], true);
        assert!(value.get("arguments").is_none());
    }

    #[test]
    fn terminal_and_ui_events_stay_internal() {
        assert!(PublicEvent::from_ipc("run.completed", &json!({})).is_none());
        assert!(PublicEvent::from_ipc("run.failed", &json!({})).is_none());
        assert!(PublicEvent::from_ipc("queue.consumed", &json!({})).is_none());
        assert!(PublicEvent::from_ipc("turn.started", &json!({})).is_none());
    }

    #[test]
    fn image_event_reads_asset_block() {
        let event = PublicEvent::from_ipc(
            "tool.image",
            &json!({"tool_id":"t2","name":"draw","asset":{"id":"a1","mime":"image/png","alt":"cat"}}),
        )
        .unwrap();
        let value: Value = serde_json::from_str(&event.to_line(None)).unwrap();
        assert_eq!(value["asset_id"], "a1");
        assert_eq!(value["mime"], "image/png");
        assert_eq!(value["alt"], "cat");
    }
}
