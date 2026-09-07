//! `miyu stdio`:长驻协议模式。stdin 一行一请求(JSON),stdout 一行一事件。
//!
//! 宿主软件起一个进程常驻,多会话多回合并发。daemon 侧是「一连接一回合」,
//! 所以这里是个 fan-in 分发器:每条 `message` 另开一条 IPC 连接跑回合,
//! N 条帧流合并成一条 stdout,靠宿主给的 `id` 区分;`cancel`/`answer` 另开
//! 连接发。daemon 零改动。
//!
//! 入站(每行一个对象,`type` 分流):
//! - `message`:`{id, content, session?, create?, mode?, images?, cwd?,
//!   timeout?, overrides?: {model, context_window, system_prompt,
//!   append_system_prompt, no_memory, tools, no_tools}}`
//! - `answer`:`{id, question_id, answer}`,answer 为字符串 / 字符串数组 /
//!   二维数组(多选)
//! - `cancel`:`{id}`
//! - `session`:`{id, op, …}`,op 同 `miyu session` 子命令
//! - `ping`:`{id}`
//!
//! stdin EOF 或 Ctrl+C:取消所有在跑的回合,等它们收尾后退出。

use crate::cli::args::TurnOptions;
use crate::cli::exit_code::{exit_code_for, usage_error, EXIT_SESSION_NOT_FOUND, EXIT_USAGE};
use crate::cli::output::event::{ErrorKind, PublicEvent};
use crate::cli::output::turn_client::{
    answer_question, run_turn, QuestionPolicy, TurnOutcome, TurnRequest,
};
use crate::cli::repl::session::discard_ephemeral_session;
use crate::cli::session_cmds::session_op_json;
use crate::cli::turn_request::{build_overrides, resolve_turn_session};
use crate::ipc;
use crate::paths::MiyuPaths;
use anyhow::Result;
use serde::Deserialize;
use serde_json::Value;
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
use tokio::sync::{mpsc, watch};

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Inbound {
    Message(MessageRequest),
    Answer {
        id: String,
        question_id: String,
        answer: Value,
    },
    Cancel {
        id: String,
    },
    Session {
        id: String,
        op: String,
        #[serde(flatten)]
        args: Value,
    },
    Ping {
        id: String,
    },
}

#[derive(Debug, Deserialize)]
struct MessageRequest {
    id: String,
    content: String,
    #[serde(default)]
    session: Option<String>,
    #[serde(default)]
    create: bool,
    #[serde(default)]
    mode: Option<String>,
    #[serde(default)]
    images: Vec<PathBuf>,
    #[serde(default)]
    cwd: Option<PathBuf>,
    /// 秒。
    #[serde(default)]
    timeout: Option<u64>,
    #[serde(default)]
    overrides: Option<InboundOverrides>,
}

#[derive(Debug, Default, Deserialize)]
struct InboundOverrides {
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    context_window: Option<usize>,
    #[serde(default)]
    system_prompt: Option<String>,
    #[serde(default)]
    append_system_prompt: Option<String>,
    #[serde(default)]
    no_memory: bool,
    #[serde(default)]
    tools: Option<Vec<String>>,
    #[serde(default)]
    no_tools: bool,
}

impl MessageRequest {
    /// 与命令行 `ask` 共用同一套解析(`turn_request`),所以两条路上模型名、
    /// 会话规则、报错措辞一致。
    fn turn_options(&self) -> TurnOptions {
        let overrides = self.overrides.as_ref();
        TurnOptions {
            session: self.session.clone(),
            continue_session: false,
            create: self.create,
            mode: self.mode.clone(),
            model: overrides.and_then(|o| o.model.clone()),
            context_window: overrides.and_then(|o| o.context_window),
            system_prompt: overrides.and_then(|o| o.system_prompt.clone()),
            append_system_prompt: overrides.and_then(|o| o.append_system_prompt.clone()),
            no_memory: overrides.is_some_and(|o| o.no_memory),
            tools: overrides.and_then(|o| o.tools.clone()),
            no_tools: overrides.is_some_and(|o| o.no_tools),
            image: self.images.clone(),
            cwd: self.cwd.clone(),
            output_format: None,
            quiet: false,
            timeout: self.timeout,
        }
    }
}

/// stdout 单写者:所有任务把整行交给它,行与行永不交错。
#[derive(Clone)]
struct Outbound(mpsc::UnboundedSender<String>);

impl Outbound {
    fn emit(&self, id: Option<&str>, event: &PublicEvent) {
        let _ = self.0.send(event.to_line(id));
    }

    fn error(&self, id: Option<&str>, kind: ErrorKind, message: impl Into<String>) {
        self.emit(
            id,
            &PublicEvent::Error {
                kind,
                message: message.into(),
                session_id: None,
            },
        );
    }
}

fn error_kind_for(error: &anyhow::Error) -> ErrorKind {
    match exit_code_for(error) {
        EXIT_USAGE => ErrorKind::Usage,
        EXIT_SESSION_NOT_FOUND => ErrorKind::SessionNotFound,
        _ => ErrorKind::TurnFailed,
    }
}

/// `answer` 字段的三种写法 → 每题一组选择。
fn parse_answers(value: &Value) -> Result<crate::question::QuestionAnswers> {
    match value {
        Value::String(text) => Ok(vec![vec![text.clone()]]),
        Value::Array(items) if items.iter().all(Value::is_string) => Ok(items
            .iter()
            .map(|item| vec![item.as_str().unwrap_or_default().to_string()])
            .collect()),
        Value::Array(items) => items
            .iter()
            .map(|item| {
                item.as_array()
                    .map(|choices| {
                        choices
                            .iter()
                            .map(|choice| choice.as_str().unwrap_or_default().to_string())
                            .collect()
                    })
                    .ok_or_else(|| usage_error("answer must be a string or an array"))
            })
            .collect(),
        _ => Err(usage_error("answer must be a string or an array")),
    }
}

struct RunningTurn {
    cancel: watch::Sender<bool>,
    task: tokio::task::JoinHandle<()>,
}

async fn start_message(
    paths: MiyuPaths,
    request: MessageRequest,
    out: Outbound,
) -> Option<RunningTurn> {
    let id = request.id.clone();
    let options = request.turn_options();
    let overrides = match build_overrides(&paths, &options) {
        Ok(overrides) => overrides,
        Err(error) => {
            out.error(Some(&id), error_kind_for(&error), error.to_string());
            return None;
        }
    };
    let session = match resolve_turn_session(&paths, &options).await {
        Ok(session) => session,
        Err(error) => {
            out.error(Some(&id), error_kind_for(&error), error.to_string());
            return None;
        }
    };
    let (cancel_tx, cancel_rx) = watch::channel(false);
    let turn = TurnRequest {
        content: request.content,
        session_id: session.session_id.clone(),
        images: request.images,
        cwd: request.cwd,
        overrides,
        timeout: request.timeout.map(Duration::from_secs),
    };
    let task = tokio::spawn(async move {
        let emit_out = out.clone();
        let emit_id = id.clone();
        let outcome = run_turn(
            &paths,
            turn,
            QuestionPolicy::External,
            Some(cancel_rx),
            |event| emit_out.emit(Some(&emit_id), &event),
        )
        .await;
        if session.ephemeral {
            if let Some(session_id) = session.session_id.as_deref() {
                discard_ephemeral_session(&paths, session_id).await;
            }
        }
        match outcome {
            Ok(TurnOutcome::Completed(done)) => out.emit(Some(&id), &done),
            Ok(TurnOutcome::Failed {
                kind,
                message,
                session_id,
            }) => out.emit(
                Some(&id),
                &PublicEvent::Error {
                    kind,
                    message,
                    session_id,
                },
            ),
            Err(error) => out.error(Some(&id), ErrorKind::Disconnected, error.to_string()),
        }
    });
    Some(RunningTurn {
        cancel: cancel_tx,
        task,
    })
}

pub(in crate::cli) async fn run_stdio(paths: &MiyuPaths) -> Result<()> {
    let info = ipc::ensure_daemon(paths, None).await?;
    let paths = MiyuPaths::new()?;
    let (line_tx, mut line_rx) = mpsc::unbounded_channel::<String>();
    let writer = tokio::spawn(async move {
        let mut stdout = tokio::io::stdout();
        while let Some(line) = line_rx.recv().await {
            if stdout.write_all(line.as_bytes()).await.is_err() {
                break;
            }
            if stdout.write_all(b"\n").await.is_err() {
                break;
            }
            let _ = stdout.flush().await;
        }
    });
    let out = Outbound(line_tx);
    out.emit(
        None,
        &PublicEvent::Ready {
            daemon_pid: info.pid,
            build_id: info.build_id.clone(),
        },
    );

    let mut running: HashMap<String, RunningTurn> = HashMap::new();
    let mut lines = tokio::io::BufReader::new(tokio::io::stdin()).lines();
    loop {
        // 收割已结束的回合,别让表无限长。
        running.retain(|_, turn| !turn.task.is_finished());
        let line = tokio::select! {
            line = lines.next_line() => match line {
                Ok(Some(line)) => line,
                Ok(None) => break,
                Err(_) => break,
            },
            _ = tokio::signal::ctrl_c() => break,
        };
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let inbound: Inbound = match serde_json::from_str(line) {
            Ok(inbound) => inbound,
            Err(error) => {
                out.error(None, ErrorKind::Usage, format!("invalid request: {error}"));
                continue;
            }
        };
        match inbound {
            Inbound::Message(request) => {
                let id = request.id.clone();
                if running.contains_key(&id) {
                    out.error(Some(&id), ErrorKind::Usage, "request id is already running");
                    continue;
                }
                if let Some(turn) = start_message(paths.clone(), request, out.clone()).await {
                    running.insert(id, turn);
                }
            }
            Inbound::Answer {
                id,
                question_id,
                answer,
            } => {
                let result = match parse_answers(&answer) {
                    Ok(answers) => answer_question(&paths, question_id, answers).await,
                    Err(error) => Err(error),
                };
                match result {
                    Ok(()) => out.emit(
                        Some(&id),
                        &PublicEvent::Result {
                            ok: true,
                            data: Value::Null,
                        },
                    ),
                    Err(error) => out.error(Some(&id), error_kind_for(&error), error.to_string()),
                }
            }
            Inbound::Cancel { id } => match running.get(&id) {
                Some(turn) => {
                    let _ = turn.cancel.send(true);
                }
                None => out.error(Some(&id), ErrorKind::Usage, "no running turn with this id"),
            },
            Inbound::Session { id, op, args } => match session_op_json(&paths, &op, &args).await {
                Ok(data) => out.emit(Some(&id), &PublicEvent::Result { ok: true, data }),
                Err(error) => out.error(Some(&id), error_kind_for(&error), error.to_string()),
            },
            Inbound::Ping { id } => out.emit(Some(&id), &PublicEvent::Pong),
        }
    }

    // EOF:取消所有在跑的回合,给它们几秒收尾(cancel 事件要发出去)。
    for turn in running.values() {
        let _ = turn.cancel.send(true);
    }
    let shutdown = async {
        for (_, turn) in running.drain() {
            let _ = turn.task.await;
        }
    };
    let _ = tokio::time::timeout(Duration::from_secs(5), shutdown).await;
    drop(out);
    let _ = writer.await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_message_with_overrides() {
        let line = r#"{"type":"message","id":"r1","content":"hi","session":"翻译","create":true,
            "overrides":{"model":"x/y","no_memory":true,"tools":["read"]},"timeout":30}"#;
        let inbound: Inbound = serde_json::from_str(line).unwrap();
        let Inbound::Message(request) = inbound else {
            panic!("expected message");
        };
        let options = request.turn_options();
        assert_eq!(options.session.as_deref(), Some("翻译"));
        assert!(options.create);
        assert_eq!(options.model.as_deref(), Some("x/y"));
        assert!(options.no_memory);
        assert_eq!(options.tools, Some(vec!["read".to_string()]));
        assert_eq!(options.timeout, Some(30));
    }

    #[test]
    fn session_request_keeps_extra_fields_as_args() {
        let inbound: Inbound = serde_json::from_str(
            r#"{"type":"session","id":"s1","op":"rename","target":"a","new_name":"b"}"#,
        )
        .unwrap();
        let Inbound::Session { op, args, .. } = inbound else {
            panic!("expected session");
        };
        assert_eq!(op, "rename");
        assert_eq!(args["target"], "a");
        assert_eq!(args["new_name"], "b");
    }

    #[test]
    fn answers_accept_three_shapes() {
        assert_eq!(
            parse_answers(&json!("yes")).unwrap(),
            vec![vec!["yes".to_string()]]
        );
        assert_eq!(
            parse_answers(&json!(["a", "b"])).unwrap(),
            vec![vec!["a".to_string()], vec!["b".to_string()]]
        );
        assert_eq!(
            parse_answers(&json!([["a", "b"]])).unwrap(),
            vec![vec!["a".to_string(), "b".to_string()]]
        );
        assert!(parse_answers(&json!(1)).is_err());
    }
}
