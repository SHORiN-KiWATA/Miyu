#![cfg_attr(test, allow(dead_code))]
//! 本地嵌入跑在独立子进程里，与长图渲染 worker 同一套思路。
//!
//! 理由是**隔离与归零**：ONNX Runtime 一旦 dlopen 进 daemon 就永远在了，用完
//! drop 模型也还剩 24 MB（09-05 实测）；子进程空闲超时退出后 daemon 一个字节
//! 不多占，OOM/panic 也带不走主进程。
//!
//! 协议：父→子 长度前缀 JSON `{"texts":[...]}`；子→父 `status u8` + 向量的
//! 小端 f32 原始字节。两侧都对帧大小设上限——读一个来自管道的长度就分配
//! 那么多内存是标准的崩法。子进程启动后先回一帧握手（成功=dims，失败=原因），
//! 父进程因此能把「没装 onnxruntime」这种错误原样说给用户。

use super::local::{find_runtime_lib, LocalEncoder, RUNTIME_LIB_ENV};
use super::manifest::{load_model_dir, LocalModel};
use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::io;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::process::{Child, ChildStdin, ChildStdout};
use tokio::sync::Mutex;

pub(crate) const WORKER_ENV: &str = "MIYU_INTERNAL_EMBEDDING_WORKER";
pub(crate) const WORKER_ARG: &str = "__embedding-worker";
const MODEL_DIR_ENV: &str = "MIYU_EMBEDDING_WORKER_MODEL_DIR";
const RUNTIME_LIB_WORKER_ENV: &str = "MIYU_EMBEDDING_WORKER_RUNTIME_LIB";
const IDLE_SECS_ENV: &str = "MIYU_EMBEDDING_WORKER_IDLE_SECS";

const MAX_REQUEST_FRAME_BYTES: usize = 8 * 1024 * 1024;
const MAX_RESPONSE_BYTES: usize = 64 * 1024 * 1024;
const MAX_ERROR_BYTES: usize = 64 * 1024;
const MAX_TEXTS_PER_REQUEST: usize = 256;
/// 09-05 实测:int8 bge-small 单线程逐条 12 ms/条、峰值 129 MB;1 GiB 是给
/// 意外留的天花板,不是预算。debug 二进制未优化,放宽。
#[cfg(not(debug_assertions))]
const WORKER_ADDRESS_SPACE_LIMIT: u64 = 1024 * 1024 * 1024;
#[cfg(debug_assertions)]
const WORKER_ADDRESS_SPACE_LIMIT: u64 = 4 * 1024 * 1024 * 1024;
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(30);
const REQUEST_TIMEOUT_BASE: Duration = Duration::from_secs(20);
const REQUEST_TIMEOUT_PER_TEXT: Duration = Duration::from_millis(500);
/// A worker that failed to start is not retried on every turn; the cause
/// (missing runtime, broken model file) does not fix itself within seconds.
const FAILURE_COOLDOWN: Duration = Duration::from_secs(5 * 60);

#[derive(Debug, Serialize, Deserialize)]
struct EmbedRequest {
    texts: Vec<String>,
}

pub(crate) fn embedding_worker_requested() -> bool {
    std::env::var_os(WORKER_ENV).as_deref() == Some(std::ffi::OsStr::new("1"))
        && std::env::args_os().nth(1).as_deref() == Some(std::ffi::OsStr::new(WORKER_ARG))
}

/// Entry point of the child process.
pub(crate) async fn run_embedding_worker() -> Result<()> {
    apply_address_space_limit()?;
    let mut output = tokio::io::stdout();
    let mut input = tokio::io::stdin();
    let idle = std::env::var(IDLE_SECS_ENV)
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .map(Duration::from_secs)
        .unwrap_or(Duration::from_secs(600));
    let loaded = (|| -> Result<LocalEncoder> {
        let model_dir = std::env::var_os(MODEL_DIR_ENV)
            .map(PathBuf::from)
            .context("embedding worker started without a model directory")?;
        let runtime_lib = std::env::var_os(RUNTIME_LIB_WORKER_ENV)
            .map(PathBuf::from)
            .context("embedding worker started without a runtime library path")?;
        let model = load_model_dir(&model_dir)?;
        LocalEncoder::load(&model, &runtime_lib)
    })();
    serve(&mut input, &mut output, idle, loaded).await
}

/// The worker's protocol loop over arbitrary streams: handshake first (dims
/// on success, the load error otherwise), then one response per request
/// until the peer closes or `idle` passes without a request. Split from the
/// process entry point so the framing is testable over an in-memory pipe.
pub(super) async fn serve<R, W>(
    input: &mut R,
    output: &mut W,
    idle: Duration,
    loaded: Result<LocalEncoder>,
) -> Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut encoder = match loaded {
        Ok(encoder) => {
            write_ok_header(output, 0, encoder.dims()).await?;
            output.flush().await?;
            encoder
        }
        Err(error) => {
            write_error(output, &format!("{error:#}")).await?;
            output.flush().await?;
            return Ok(());
        }
    };
    loop {
        let payload =
            match tokio::time::timeout(idle, read_frame(input, MAX_REQUEST_FRAME_BYTES)).await {
                Err(_) => return Ok(()),
                Ok(Ok(Some(payload))) => payload,
                Ok(Ok(None)) => return Ok(()),
                Ok(Err(error)) => return Err(error),
            };
        let result = serde_json::from_slice::<EmbedRequest>(&payload)
            .context("decoding the embedding request")
            .and_then(|request| {
                if request.texts.len() > MAX_TEXTS_PER_REQUEST {
                    bail!("embedding request has too many texts");
                }
                let mut vectors = Vec::with_capacity(request.texts.len());
                for text in &request.texts {
                    vectors.push(encoder.encode(text)?);
                }
                Ok(vectors)
            });
        match result {
            Ok(vectors) => {
                let dims = vectors.first().map(Vec::len).unwrap_or(encoder.dims());
                write_ok_header(output, vectors.len(), dims).await?;
                let mut bytes = Vec::with_capacity(vectors.len() * dims * 4);
                for vector in &vectors {
                    bytes.extend_from_slice(&super::vectors::vector_to_blob(vector));
                }
                output.write_all(&bytes).await?;
            }
            Err(error) => write_error(output, &format!("{error:#}")).await?,
        }
        output.flush().await?;
    }
}

#[cfg(unix)]
fn apply_address_space_limit() -> Result<()> {
    let limit = libc::rlimit {
        rlim_cur: WORKER_ADDRESS_SPACE_LIMIT as libc::rlim_t,
        rlim_max: WORKER_ADDRESS_SPACE_LIMIT as libc::rlim_t,
    };
    if unsafe { libc::setrlimit(libc::RLIMIT_AS, &limit) } != 0 {
        return Err(io::Error::last_os_error()).context("limiting embedding worker address space");
    }
    Ok(())
}

#[cfg(not(unix))]
fn apply_address_space_limit() -> Result<()> {
    Ok(())
}

// ---- framing -------------------------------------------------------------

async fn write_frame<W: AsyncWrite + Unpin>(writer: &mut W, payload: &[u8]) -> Result<()> {
    if payload.len() > MAX_REQUEST_FRAME_BYTES {
        bail!("embedding request exceeds the {MAX_REQUEST_FRAME_BYTES}-byte limit");
    }
    let length = u32::try_from(payload.len()).context("embedding request is too large")?;
    writer.write_all(&length.to_be_bytes()).await?;
    writer.write_all(payload).await?;
    Ok(())
}

async fn read_frame<R: AsyncRead + Unpin>(reader: &mut R, limit: usize) -> Result<Option<Vec<u8>>> {
    let mut length = [0_u8; 4];
    match reader.read_exact(&mut length).await {
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(error) => return Err(error.into()),
    }
    let length = u32::from_be_bytes(length) as usize;
    if length > limit {
        bail!("embedding frame exceeds the {limit}-byte limit");
    }
    let mut payload = vec![0_u8; length];
    reader.read_exact(&mut payload).await?;
    Ok(Some(payload))
}

async fn write_ok_header<W: AsyncWrite + Unpin>(
    writer: &mut W,
    count: usize,
    dims: usize,
) -> Result<()> {
    writer.write_all(&[0]).await?;
    writer.write_all(&(count as u32).to_be_bytes()).await?;
    writer.write_all(&(dims as u32).to_be_bytes()).await?;
    Ok(())
}

async fn write_error<W: AsyncWrite + Unpin>(writer: &mut W, message: &str) -> Result<()> {
    let mut message = message.to_string();
    if message.len() > MAX_ERROR_BYTES {
        let mut end = MAX_ERROR_BYTES;
        while !message.is_char_boundary(end) {
            end -= 1;
        }
        message.truncate(end);
    }
    writer.write_all(&[1]).await?;
    writer
        .write_all(&(message.len() as u32).to_be_bytes())
        .await?;
    writer.write_all(message.as_bytes()).await?;
    Ok(())
}

#[derive(Debug)]
pub(super) enum ExchangeError {
    /// The pipe broke or the child answered garbage: restart it.
    Transport(anyhow::Error),
    /// The child ran fine and rejected this request: keep it alive.
    Rejected(String),
}

pub(super) async fn read_response<R: AsyncRead + Unpin>(
    reader: &mut R,
) -> std::result::Result<(usize, Vec<Vec<f32>>), ExchangeError> {
    let mut status = [0_u8; 1];
    reader
        .read_exact(&mut status)
        .await
        .map_err(|error| ExchangeError::Transport(error.into()))?;
    let mut word = [0_u8; 4];
    match status[0] {
        0 => {
            reader
                .read_exact(&mut word)
                .await
                .map_err(|error| ExchangeError::Transport(error.into()))?;
            let count = u32::from_be_bytes(word) as usize;
            reader
                .read_exact(&mut word)
                .await
                .map_err(|error| ExchangeError::Transport(error.into()))?;
            let dims = u32::from_be_bytes(word) as usize;
            let bytes = count
                .checked_mul(dims)
                .and_then(|n| n.checked_mul(4))
                .ok_or_else(|| ExchangeError::Transport(anyhow!("embedding response overflow")))?;
            if bytes > MAX_RESPONSE_BYTES {
                return Err(ExchangeError::Transport(anyhow!(
                    "embedding response exceeds the {MAX_RESPONSE_BYTES}-byte limit"
                )));
            }
            let mut payload = vec![0_u8; bytes];
            reader
                .read_exact(&mut payload)
                .await
                .map_err(|error| ExchangeError::Transport(error.into()))?;
            let vectors = payload
                .chunks_exact(dims.max(1) * 4)
                .map(|chunk| super::vectors::vector_from_blob(chunk).unwrap_or_default())
                .take(count)
                .collect();
            Ok((dims, vectors))
        }
        1 => {
            reader
                .read_exact(&mut word)
                .await
                .map_err(|error| ExchangeError::Transport(error.into()))?;
            let length = u32::from_be_bytes(word) as usize;
            if length > MAX_ERROR_BYTES {
                return Err(ExchangeError::Transport(anyhow!(
                    "embedding error frame is too large"
                )));
            }
            let mut message = vec![0_u8; length];
            reader
                .read_exact(&mut message)
                .await
                .map_err(|error| ExchangeError::Transport(error.into()))?;
            Err(ExchangeError::Rejected(
                String::from_utf8_lossy(&message).into_owned(),
            ))
        }
        other => Err(ExchangeError::Transport(anyhow!(
            "embedding worker sent an unknown status byte {other}"
        ))),
    }
}

// ---- parent side ---------------------------------------------------------

struct WorkerProcess {
    child: Child,
    stdin: ChildStdin,
    stdout: ChildStdout,
    /// Model dir + runtime lib the child was started with; a config change
    /// that moves either replaces the child instead of feeding it stale work.
    key: String,
}

#[derive(Default)]
struct WorkerSlot {
    process: Option<WorkerProcess>,
    idle_task: Option<tokio::task::JoinHandle<()>>,
    generation: u64,
    last_failure: Option<(Instant, String, String)>,
}

impl WorkerSlot {
    fn cancel_idle_timer(&mut self) {
        if let Some(task) = self.idle_task.take() {
            task.abort();
        }
    }
}

fn slot() -> Arc<Mutex<WorkerSlot>> {
    static SLOT: OnceLock<Arc<Mutex<WorkerSlot>>> = OnceLock::new();
    SLOT.get_or_init(|| Arc::new(Mutex::new(WorkerSlot::default())))
        .clone()
}

pub(crate) fn runtime_lib_or_hint() -> Result<PathBuf> {
    find_runtime_lib().ok_or_else(|| {
        let searched = super::local::candidate_runtime_libs()
            .iter()
            .map(|path| path.display().to_string())
            .collect::<Vec<_>>()
            .join(", ");
        anyhow!(
            "ONNX Runtime library not found; install it (Arch: onnxruntime-cpu) or set {RUNTIME_LIB_ENV} (searched: {searched})"
        )
    })
}

async fn spawn_worker(
    model: &LocalModel,
    runtime_lib: &PathBuf,
    idle: Duration,
    key: String,
) -> Result<(WorkerProcess, usize)> {
    let executable = crate::paths::miyu_executable()?;
    let mut command = tokio::process::Command::new(&executable);
    command
        .arg(WORKER_ARG)
        .env(WORKER_ENV, "1")
        .env(MODEL_DIR_ENV, &model.dir)
        .env(RUNTIME_LIB_WORKER_ENV, runtime_lib)
        .env(IDLE_SECS_ENV, idle.as_secs().to_string())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true);
    let mut child = command.spawn().with_context(|| {
        format!(
            "starting the embedding worker ({}); if Miyu was upgraded or rebuilt while running, restart the daemon",
            executable.display()
        )
    })?;
    let stdin = child
        .stdin
        .take()
        .context("embedding worker stdin was not piped")?;
    let mut stdout = child
        .stdout
        .take()
        .context("embedding worker stdout was not piped")?;
    let handshake = tokio::time::timeout(HANDSHAKE_TIMEOUT, read_response(&mut stdout)).await;
    let dims = match handshake {
        Ok(Ok((dims, _))) => dims,
        Ok(Err(ExchangeError::Rejected(message))) => {
            let _ = child.kill().await;
            bail!("{message}");
        }
        Ok(Err(ExchangeError::Transport(error))) => {
            let _ = child.kill().await;
            return Err(error).context("embedding worker did not complete its handshake");
        }
        Err(_) => {
            let _ = child.kill().await;
            bail!(
                "embedding worker took longer than {}s to load the model",
                HANDSHAKE_TIMEOUT.as_secs()
            );
        }
    };
    Ok((
        WorkerProcess {
            child,
            stdin,
            stdout,
            key,
        },
        dims,
    ))
}

async fn stop_worker(mut worker: WorkerProcess) {
    let _ = worker.child.kill().await;
    let _ = worker.child.wait().await;
}

async fn exchange(
    worker: &mut WorkerProcess,
    texts: &[String],
) -> std::result::Result<Vec<Vec<f32>>, ExchangeError> {
    exchange_io(&mut worker.stdin, &mut worker.stdout, texts).await
}

/// One request/response over arbitrary streams (the client half of [`serve`]).
pub(super) async fn exchange_io<W, R>(
    writer: &mut W,
    reader: &mut R,
    texts: &[String],
) -> std::result::Result<Vec<Vec<f32>>, ExchangeError>
where
    W: AsyncWrite + Unpin,
    R: AsyncRead + Unpin,
{
    let payload = serde_json::to_vec(&EmbedRequest {
        texts: texts.to_vec(),
    })
    .map_err(|error| ExchangeError::Transport(error.into()))?;
    write_frame(writer, &payload)
        .await
        .map_err(ExchangeError::Transport)?;
    writer
        .flush()
        .await
        .map_err(|error| ExchangeError::Transport(error.into()))?;
    let (_, vectors) = read_response(reader).await?;
    if vectors.len() != texts.len() {
        return Err(ExchangeError::Transport(anyhow!(
            "embedding worker returned {} vectors for {} texts",
            vectors.len(),
            texts.len()
        )));
    }
    Ok(vectors)
}

fn recycle(slot: &mut WorkerSlot, worker: WorkerProcess, idle: Duration) {
    slot.process = Some(worker);
    slot.generation = slot.generation.wrapping_add(1);
    let generation = slot.generation;
    let weak = Arc::downgrade(&self::slot());
    slot.idle_task = Some(tokio::spawn(async move {
        tokio::time::sleep(idle).await;
        let Some(shared) = weak.upgrade() else {
            return;
        };
        let mut slot = shared.lock().await;
        if slot.generation != generation {
            return;
        }
        if let Some(worker) = slot.process.take() {
            stop_worker(worker).await;
        }
        slot.idle_task.take();
    }));
}

/// Under `cargo test` the current executable is the test harness, so spawning
/// "ourselves" would talk to the wrong program; tests encode in-process
/// instead (the process form is covered by testkit/embedding). Load failures
/// still surface as `Err`, which is what the degradation tests exercise.
#[cfg(test)]
pub(crate) async fn embed_via_worker(
    model: &LocalModel,
    _idle: Duration,
    texts: &[String],
) -> Result<Vec<Vec<f32>>> {
    static ENCODER: OnceLock<std::sync::Mutex<Option<(String, LocalEncoder)>>> = OnceLock::new();
    let runtime_lib = runtime_lib_or_hint()?;
    let key = format!("{}|{}", model.dir.display(), runtime_lib.display());
    let mut guard = ENCODER.get_or_init(Default::default).lock().unwrap();
    if guard.as_ref().map(|(k, _)| k != &key).unwrap_or(true) {
        *guard = Some((key, LocalEncoder::load(model, &runtime_lib)?));
    }
    let (_, encoder) = guard.as_mut().expect("encoder was just loaded");
    texts.iter().map(|text| encoder.encode(text)).collect()
}

/// Embed through the (lazily started) worker. Errors are plain `Result`s so
/// callers degrade to keyword search; the message names the actual cause.
#[cfg(not(test))]
pub(crate) async fn embed_via_worker(
    model: &LocalModel,
    idle: Duration,
    texts: &[String],
) -> Result<Vec<Vec<f32>>> {
    if texts.is_empty() {
        return Ok(Vec::new());
    }
    if texts.len() > MAX_TEXTS_PER_REQUEST {
        let mut all = Vec::with_capacity(texts.len());
        for chunk in texts.chunks(MAX_TEXTS_PER_REQUEST) {
            all.extend(Box::pin(embed_via_worker(model, idle, chunk)).await?);
        }
        return Ok(all);
    }
    let shared = slot();
    let mut slot = shared.lock().await;
    slot.cancel_idle_timer();
    let runtime_lib = match runtime_lib_or_hint() {
        Ok(path) => path,
        Err(error) => {
            if let Some(worker) = slot.process.take() {
                stop_worker(worker).await;
            }
            return Err(error);
        }
    };
    let key = format!("{}|{}", model.dir.display(), runtime_lib.display());
    if slot
        .process
        .as_ref()
        .is_some_and(|worker| worker.key != key)
    {
        if let Some(worker) = slot.process.take() {
            stop_worker(worker).await;
        }
    }
    if slot.process.is_none() {
        if let Some((at, failed_key, message)) = &slot.last_failure {
            if *failed_key == key && at.elapsed() < FAILURE_COOLDOWN {
                bail!(
                    "{message} (retry in {}s)",
                    (FAILURE_COOLDOWN - at.elapsed()).as_secs()
                );
            }
        }
    }
    let timeout = REQUEST_TIMEOUT_BASE + REQUEST_TIMEOUT_PER_TEXT * texts.len() as u32;
    for attempt in 0..2 {
        let mut worker = match slot.process.take() {
            Some(worker) => worker,
            None => match spawn_worker(model, &runtime_lib, idle, key.clone()).await {
                Ok((worker, _dims)) => {
                    slot.last_failure = None;
                    worker
                }
                Err(error) => {
                    let message = format!("{error:#}");
                    slot.last_failure = Some((Instant::now(), key.clone(), message.clone()));
                    bail!("{message}");
                }
            },
        };
        match tokio::time::timeout(timeout, exchange(&mut worker, texts)).await {
            Ok(Ok(vectors)) => {
                recycle(&mut slot, worker, idle);
                return Ok(vectors);
            }
            Ok(Err(ExchangeError::Rejected(message))) => {
                recycle(&mut slot, worker, idle);
                bail!("embedding worker rejected the request: {message}");
            }
            Ok(Err(ExchangeError::Transport(error))) => {
                stop_worker(worker).await;
                if attempt == 1 {
                    return Err(error).context("embedding worker communication failed");
                }
            }
            Err(_) => {
                stop_worker(worker).await;
                bail!(
                    "embedding worker exceeded its {}-second timeout",
                    timeout.as_secs()
                );
            }
        }
    }
    unreachable!("embedding worker retry loop always returns")
}

/// Stop a running worker now (config reload, `embed status` cleanup).
pub(crate) async fn shutdown_worker() {
    let shared = slot();
    let mut slot = shared.lock().await;
    slot.cancel_idle_timer();
    if let Some(worker) = slot.process.take() {
        stop_worker(worker).await;
    }
}
