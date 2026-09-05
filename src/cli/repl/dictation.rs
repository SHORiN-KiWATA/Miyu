//! REPL 听写(`/stt`):认领 daemon 的一条听写流,识别文本在输入循环的
//! 空闲 tick 里取出来填进编辑框(或直接提交)。
//!
//! REPL 的输入循环是同步阻塞的(裸 poll stdin),tokio 任务在它期间不会被
//! 调度,所以 IPC 连接跑在自己的 std 线程 + current_thread runtime 上,事件经
//! std mpsc 交回主线程——和 jobs 的轮询线程同一套路。

use crate::cli::*;
use std::sync::mpsc;
use std::sync::Mutex;
use tokio::sync::oneshot;

pub(in crate::cli) enum DictationEvent {
    Utterance(String),
    Ended,
    Error(String),
}

struct Session {
    events: mpsc::Receiver<DictationEvent>,
    stop: Option<oneshot::Sender<()>>,
    auto_submit: bool,
}

static ACTIVE: Mutex<Option<Session>> = Mutex::new(None);

pub(in crate::cli) fn is_active() -> bool {
    ACTIVE.lock().unwrap().is_some()
}

/// 开始听写。已在听写则无操作(返回 false)。
pub(in crate::cli) fn start(paths: &MiyuPaths, auto_submit: bool) -> bool {
    let mut active = ACTIVE.lock().unwrap();
    if active.is_some() {
        return false;
    }
    let (tx, rx) = mpsc::channel();
    let (stop_tx, stop_rx) = oneshot::channel();
    let socket = paths.ipc_socket();
    let spawned = std::thread::Builder::new()
        .name("miyu-repl-dictation".into())
        .spawn(move || {
            if let Err(error) = relay(&socket, &tx, stop_rx) {
                let _ = tx.send(DictationEvent::Error(format!("{error:#}")));
            }
            let _ = tx.send(DictationEvent::Ended);
        });
    if spawned.is_err() {
        return false;
    }
    *active = Some(Session {
        events: rx,
        stop: Some(stop_tx),
        auto_submit,
    });
    true
}

/// 停止听写(连接一断 daemon 就释放认领并恢复唤醒)。
pub(in crate::cli) fn stop() {
    if let Some(mut session) = ACTIVE.lock().unwrap().take() {
        if let Some(stop) = session.stop.take() {
            let _ = stop.send(());
        }
    }
}

/// 空闲 tick 调用:取出积压事件。Ended/Error 之后会话自动清除。
/// 返回 (事件, 是否直接提交)。
pub(in crate::cli) fn poll() -> Vec<(DictationEvent, bool)> {
    let mut out = Vec::new();
    let mut guard = ACTIVE.lock().unwrap();
    let Some(session) = guard.as_ref() else {
        return out;
    };
    let auto_submit = session.auto_submit;
    let mut ended = false;
    while let Ok(event) = session.events.try_recv() {
        if matches!(event, DictationEvent::Ended | DictationEvent::Error(_)) {
            ended = true;
        }
        out.push((event, auto_submit));
        if ended {
            break;
        }
    }
    if ended {
        *guard = None;
    }
    out
}

fn relay(
    socket: &std::path::Path,
    tx: &mpsc::Sender<DictationEvent>,
    mut stop: oneshot::Receiver<()>,
) -> Result<()> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async move {
        let mut stream = ipc::connect(socket)
            .await
            .context(t("Miyu daemon is not running", "Miyu daemon 未运行"))?;
        ipc::send(&mut stream, &IpcRequest::new(IpcCommand::StartDictation)).await?;
        match ipc::receive::<IpcFrame>(&mut stream).await? {
            Some(IpcFrame::Ack) => {}
            Some(IpcFrame::Error { message, .. }) => bail!("{message}"),
            other => bail!("unexpected reply to StartDictation: {other:?}"),
        }
        loop {
            // stop 到达就直接放弃连接:daemon 看到断开即释放听写。
            let frame = tokio::select! {
                frame = ipc::receive::<IpcFrame>(&mut stream) => frame?,
                _ = &mut stop => return Ok(()),
            };
            match frame {
                Some(IpcFrame::Event { kind, data, .. }) => match kind.as_str() {
                    "voice.dictation" => {
                        let text = data
                            .get("text")
                            .and_then(serde_json::Value::as_str)
                            .unwrap_or_default()
                            .trim()
                            .to_string();
                        if !text.is_empty() && tx.send(DictationEvent::Utterance(text)).is_err() {
                            return Ok(());
                        }
                    }
                    "voice.dictation_ended" => return Ok(()),
                    _ => {}
                },
                Some(IpcFrame::Error { message, .. }) => bail!("{message}"),
                Some(_) => {}
                None => return Ok(()),
            }
        }
    })
}
