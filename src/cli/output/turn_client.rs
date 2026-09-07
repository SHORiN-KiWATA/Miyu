//! 程序驱动形态的回合客户端:一条 IPC 连接跑一个回合,把事件翻成对外事件。
//!
//! 与 `one_shot.rs`(终端渲染那条)并列而不共用——那条路的循环里揉着
//! 终端光标、raw mode、footer 转轮;这里只有帧进、JSON 出。`miyu ask
//! --output-format json|stream-json` 与 `miyu stdio` 都走这里。
//!
//! daemon 侧是「一连接一回合」:取消、答问都得另开连接发,这里照办。

use crate::cli::output::event::{ErrorKind, PublicEvent};
use crate::cli::repl::session::send_ipc_command;
use crate::ipc::{self, Command as IpcCommand, Frame as IpcFrame, Request as IpcRequest};
use crate::paths::MiyuPaths;
use anyhow::{bail, Result};
use serde_json::Value;
use std::path::PathBuf;
use std::time::{Duration, Instant};

pub struct TurnRequest {
    pub content: String,
    /// None = daemon 的当前会话。
    pub session_id: Option<String>,
    pub images: Vec<PathBuf>,
    pub cwd: Option<PathBuf>,
    pub overrides: Option<ipc::TurnOverrides>,
    pub timeout: Option<Duration>,
}

/// 她提问时怎么办。一次性调用没有回答通道,直接关掉让回合继续;stdio 模式
/// 把问题交给宿主,由分发器收到 `answer` 后另开连接回。
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum QuestionPolicy {
    AutoClose,
    External,
}

pub enum TurnOutcome {
    /// 携带 `PublicEvent::Done`。
    Completed(PublicEvent),
    Failed {
        kind: ErrorKind,
        message: String,
        session_id: Option<String>,
    },
}

impl TurnOutcome {
    pub fn failed(kind: ErrorKind, message: impl Into<String>, session_id: Option<String>) -> Self {
        TurnOutcome::Failed {
            kind,
            message: message.into(),
            session_id,
        }
    }
}

/// 外部取消信号(stdio 的 `cancel` / EOF)。一次性调用传 None,自己盯 Ctrl+C。
pub type CancelSignal = tokio::sync::watch::Receiver<bool>;

fn text(data: &Value, key: &str) -> Option<String> {
    data.get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

fn image_attachments(images: &[PathBuf]) -> Result<Vec<Option<ipc::ImageAttachment>>> {
    images
        .iter()
        .map(|path| {
            // daemon 的工作目录不是调用方的:必须绝对路径。
            let absolute = std::fs::canonicalize(path).map_err(|error| {
                crate::cli::exit_code::usage_error(format!(
                    "{}: {} ({error})",
                    crate::i18n::text("image not found", "找不到图片"),
                    path.display()
                ))
            })?;
            Ok(Some(ipc::ImageAttachment::Path {
                path: absolute.to_string_lossy().into_owned(),
            }))
        })
        .collect()
}

/// 跑一个回合。事件按到达顺序交给 `emit`;返回终态。连接层错误(daemon
/// 没了)走 Err,回合层失败走 `TurnOutcome::Failed`。
pub async fn run_turn(
    paths: &MiyuPaths,
    request: TurnRequest,
    policy: QuestionPolicy,
    mut cancel: Option<CancelSignal>,
    mut emit: impl FnMut(PublicEvent),
) -> Result<TurnOutcome> {
    let started_at = Instant::now();
    let images = image_attachments(&request.images)?;
    let mut stream = ipc::connect(&paths.ipc_socket()).await?;
    ipc::send(
        &mut stream,
        &IpcRequest::new(IpcCommand::StartTurn {
            content: request.content,
            mode: "normal".to_string(),
            images,
            cwd: request.cwd.or_else(|| std::env::current_dir().ok()),
            session_id: request.session_id.clone(),
            origin_tty: None,
            overrides: request.overrides,
        }),
    )
    .await?;
    let Some(first) = ipc::receive::<IpcFrame>(&mut stream).await? else {
        bail!("Miyu core closed the connection before accepting the turn");
    };
    let run_id = match first {
        IpcFrame::Accepted { run_id, .. } => run_id,
        IpcFrame::Error { message, .. } => {
            let kind = if message.contains("session") && message.contains("not found") {
                ErrorKind::SessionNotFound
            } else {
                ErrorKind::TurnFailed
            };
            return Ok(TurnOutcome::failed(kind, message, request.session_id));
        }
        other => bail!("Miyu core returned an unexpected response: {other:?}"),
    };

    let deadline = request.timeout.map(|timeout| started_at + timeout);
    let mut session_id = request.session_id.clone();
    let mut content = String::new();
    let cancel_and_finish = |kind: ErrorKind, message: &str, session_id: Option<String>| {
        let run_id = run_id.clone();
        let message = message.to_string();
        async move {
            let _ = send_ipc_command(paths, IpcCommand::Cancel { run_id }).await;
            Ok(TurnOutcome::failed(kind, message, session_id))
        }
    };
    loop {
        let frame = tokio::select! {
            biased;
            frame = ipc::receive::<IpcFrame>(&mut stream) => frame?,
            _ = async {
                match cancel.as_mut() {
                    Some(cancel) => {
                        while !*cancel.borrow() {
                            if cancel.changed().await.is_err() {
                                break;
                            }
                        }
                    }
                    None => std::future::pending::<()>().await,
                }
            } => {
                return cancel_and_finish(ErrorKind::Cancelled, "cancelled", session_id).await;
            }
            _ = async {
                match deadline {
                    Some(deadline) => tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)).await,
                    None => std::future::pending::<()>().await,
                }
            } => {
                return cancel_and_finish(ErrorKind::Timeout, "turn timed out", session_id).await;
            }
            _ = tokio::signal::ctrl_c(), if cancel.is_none() => {
                return cancel_and_finish(ErrorKind::Cancelled, "cancelled", session_id).await;
            }
        };
        let Some(frame) = frame else {
            return Ok(TurnOutcome::failed(
                ErrorKind::Disconnected,
                "Miyu core disconnected during the turn",
                session_id,
            ));
        };
        let (kind, data) = match frame {
            IpcFrame::Event { kind, data, .. } => (kind, data),
            IpcFrame::Error { message, .. } => {
                return Ok(TurnOutcome::failed(
                    ErrorKind::TurnFailed,
                    message,
                    session_id,
                ));
            }
            _ => continue,
        };
        match kind.as_str() {
            "run.started" => {
                if let Some(id) = text(&data, "session_id") {
                    session_id = Some(id);
                }
            }
            "turn.started" => emit(PublicEvent::Started {
                session_id: session_id.clone().unwrap_or_default(),
                run_id: run_id.clone(),
                turn_id: text(&data, "turn_id"),
            }),
            "assistant.delta" => {
                if let Some(delta) = data.get("delta").and_then(Value::as_str) {
                    content.push_str(delta);
                }
                if let Some(event) = PublicEvent::from_ipc(&kind, &data) {
                    emit(event);
                }
            }
            "generation.superseded" => content.clear(),
            "question.requested" => {
                if let Some(event) = PublicEvent::from_ipc(&kind, &data) {
                    emit(event);
                }
                if policy == QuestionPolicy::AutoClose {
                    let _ = send_ipc_command(
                        paths,
                        IpcCommand::CloseQuestion {
                            question_id: text(&data, "question_id").unwrap_or_default(),
                        },
                    )
                    .await;
                }
            }
            "run.completed" => {
                let session_id = text(&data, "session_id").or(session_id).unwrap_or_default();
                let context_u64 = |key: &str| data.get(key).and_then(Value::as_u64);
                return Ok(TurnOutcome::Completed(PublicEvent::Done {
                    session_id,
                    run_id: run_id.clone(),
                    // daemon 随终态发最终正文;老 daemon 没这个字段就用累加的。
                    text: text(&data, "content").unwrap_or(content),
                    usage: data.get("usage").cloned().filter(|value| !value.is_null()),
                    usage_estimated: data
                        .get("usage_estimated")
                        .and_then(Value::as_bool)
                        .unwrap_or(false),
                    model: text(&data, "model"),
                    provider_id: text(&data, "provider_id"),
                    context_tokens: context_u64("context_tokens").unwrap_or_default(),
                    context_window: context_u64("context_window"),
                    elapsed_ms: started_at.elapsed().as_millis() as u64,
                }));
            }
            "run.failed" => {
                return Ok(TurnOutcome::failed(
                    ErrorKind::TurnFailed,
                    text(&data, "message").unwrap_or_else(|| "turn failed".to_string()),
                    text(&data, "session_id").or(session_id),
                ));
            }
            "run.cancelled" => {
                return Ok(TurnOutcome::failed(
                    ErrorKind::Cancelled,
                    "cancelled",
                    text(&data, "session_id").or(session_id),
                ));
            }
            _ => {
                if let Some(event) = PublicEvent::from_ipc(&kind, &data) {
                    emit(event);
                }
            }
        }
    }
}

/// 另开连接回答一个问题(stdio 的 `answer`)。
pub async fn answer_question(
    paths: &MiyuPaths,
    question_id: String,
    answers: crate::question::QuestionAnswers,
) -> Result<()> {
    send_ipc_command(
        paths,
        IpcCommand::AnswerQuestion {
            question_id,
            answers,
        },
    )
    .await
}
