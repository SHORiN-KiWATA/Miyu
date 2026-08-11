use crate::paths::MiyuPaths;
use crate::question::QuestionAnswers;
use anyhow::{bail, Context, Result};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use serde_json::Value;
#[cfg(target_os = "linux")]
use std::ffi::OsString;
#[cfg(target_os = "linux")]
use std::os::unix::ffi::OsStringExt;
use std::path::Path;
use std::{
    fs::File, fs::OpenOptions, os::fd::AsRawFd, os::unix::fs::OpenOptionsExt,
    os::unix::fs::PermissionsExt, os::unix::process::CommandExt, path::PathBuf, process::Stdio,
    time::Duration,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

pub const PROTOCOL_VERSION: u16 = 3;
pub const DEFAULT_WEB_PORT: u16 = 8300;
pub const DAEMON_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(15);
pub const ADMIN_BUSY_MESSAGE: &str = "Miyu is busy with another operation";
const MAX_FRAME_BYTES: usize = 24 * 1024 * 1024;

/// Unique id of this build, stamped by build.rs. A daemon whose build id
/// differs from the client's is restarted transparently so a rebuild never
/// keeps serving stale code.
pub const BUILD_ID: &str = env!("MIYU_BUILD_ID");

/// Access URLs for the WebUI: loopback plus every local IPv4 address.
/// Shared between the daemon (startup banner) and the CLI (`miyu web` /
/// `--status` output).
pub fn web_access_urls(port: u16) -> Vec<String> {
    web_access_urls_for(std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED), port)
}

/// Access URLs honoring the daemon's actual bind address: a loopback bind
/// yields only the localhost URL, a concrete interface bind yields only
/// that address, and an unspecified bind enumerates every local IPv4.
pub fn web_access_urls_for(bind: std::net::IpAddr, port: u16) -> Vec<String> {
    if bind.is_loopback() {
        return vec![format!("http://127.0.0.1:{port}")];
    }
    if !bind.is_unspecified() {
        return vec![format!("http://{bind}:{port}")];
    }
    let mut addresses = std::collections::BTreeSet::new();
    addresses.insert(std::net::Ipv4Addr::LOCALHOST);
    if let Ok(interfaces) = if_addrs::get_if_addrs() {
        for interface in interfaces {
            if let if_addrs::IfAddr::V4(address) = interface.addr {
                if !address.ip.is_unspecified() {
                    addresses.insert(address.ip);
                }
            }
        }
    }
    addresses
        .into_iter()
        .map(|address| format!("http://{address}:{port}"))
        .collect()
}

#[derive(Clone, Debug)]
pub struct DaemonInfo {
    pub pid: u32,
    pub web_port: u16,
    pub web_public: bool,
    pub web_bind: Option<std::net::IpAddr>,
    pub build_id: String,
    pub protocol_version: u16,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct DaemonLaunchConfig {
    pub port: u16,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub password_file: Option<PathBuf>,
    /// WebUI bind address; `None` binds loopback only (the secure default).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bind: Option<std::net::IpAddr>,
}

impl Default for DaemonLaunchConfig {
    fn default() -> Self {
        Self {
            port: DEFAULT_WEB_PORT,
            password_file: None,
            bind: None,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct DaemonProcessIdentity {
    pid: u32,
    #[cfg(target_os = "linux")]
    start_time: Option<u64>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SessionState {
    pub context_tokens: u64,
    pub context_window: Option<usize>,
    pub cumulative_tokens: u64,
    /// Prompt and cache-read halves behind Σ's cache rate. Defaulted so a REPL
    /// talking to an older daemon degrades to "no cache reported" instead of
    /// failing to parse the state frame.
    #[serde(default)]
    pub cumulative_prompt_tokens: u64,
    #[serde(default)]
    pub cumulative_cache_read_tokens: u64,
    #[serde(default)]
    pub session_id: String,
    #[serde(default)]
    pub session_name: String,
    #[serde(default)]
    pub workspace: Option<String>,
}

/// Reference to a chat session in IPC commands.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SessionRef {
    /// The daemon's current session.
    Current,
    /// A session by exact id.
    Id { id: String },
    /// A user session of the active persona by (case-insensitive) name.
    Name { name: String },
}

pub struct DirectCoreLease {
    lock_file: File,
}

pub struct WebCoreLease {
    lock_file: File,
    socket_path: PathBuf,
}

struct StarterLease {
    lock_file: File,
}

impl Drop for DirectCoreLease {
    fn drop(&mut self) {
        unlock(&self.lock_file);
    }
}

impl Drop for WebCoreLease {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.socket_path);
        unlock(&self.lock_file);
    }
}

impl Drop for StarterLease {
    fn drop(&mut self) {
        unlock(&self.lock_file);
    }
}

pub fn acquire_direct_core(paths: &MiyuPaths) -> Result<DirectCoreLease> {
    prepare_runtime_dir(paths)?;
    acquire_direct_core_at(paths.ipc_lock())
}

pub fn acquire_web_core(paths: &MiyuPaths) -> Result<WebCoreLease> {
    prepare_runtime_dir(paths)?;
    let lock_file = acquire_lock(paths.ipc_lock())?;
    let socket_path = paths.ipc_socket();
    if socket_path.exists() {
        std::fs::remove_file(&socket_path)?;
    }
    Ok(WebCoreLease {
        lock_file,
        socket_path,
    })
}

fn prepare_runtime_dir(paths: &MiyuPaths) -> Result<()> {
    let runtime_dir = paths.runtime_dir();
    std::fs::create_dir_all(&runtime_dir)?;
    std::fs::set_permissions(&runtime_dir, std::fs::Permissions::from_mode(0o700))?;
    Ok(())
}

fn acquire_direct_core_at(lock_path: PathBuf) -> Result<DirectCoreLease> {
    Ok(DirectCoreLease {
        lock_file: acquire_lock(lock_path)?,
    })
}

fn acquire_lock(lock_path: PathBuf) -> Result<File> {
    let lock_file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(lock_path)?;
    let result = unsafe { libc::flock(lock_file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if result != 0 {
        bail!("Miyu Web core is starting or temporarily unavailable");
    }
    Ok(lock_file)
}

fn unlock(lock_file: &File) {
    unsafe {
        libc::flock(lock_file.as_raw_fd(), libc::LOCK_UN);
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Request {
    pub version: u16,
    #[serde(flatten)]
    pub command: Command,
}

impl Request {
    pub fn new(command: Command) -> Self {
        Self {
            version: PROTOCOL_VERSION,
            command,
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "command", rename_all = "snake_case")]
pub enum Command {
    Ping,
    Shutdown,
    ReloadConfig,
    GetStatus,
    /// Lightweight poll for the REPL background-command status strip.
    JobsOverview,
    /// Attach to a running daemon-initiated turn (background-command wake)
    /// and stream its event frames until it finishes.
    FollowRun {
        run_id: String,
    },
    /// Stop all running background commands of a session (REPL exit).
    StopSessionJobs {
        session_id: String,
    },
    GetSessionState {
        target: SessionRef,
    },
    /// Re-initializes one conversation: history, queue, per-session usage and
    /// the recall caches that belong to it. Only ever sent by the first-party
    /// frontends (CLI, REPL, WebUI) — platform sessions are rejected upstream
    /// and clear themselves through `ClearSessionContent`.
    ResetConversation {
        target: SessionRef,
    },
    /// Erases everything the persona accumulated: memory, every session's
    /// contents, group-chat contexts and auto-generated skills. Configuration
    /// is untouched. Irreversible; every frontend confirms before sending it.
    WipePersona,
    Undo {
        target: SessionRef,
    },
    Pop {
        target: SessionRef,
        turn_ids: Vec<String>,
    },
    Compact {
        target: SessionRef,
    },
    StartTurn {
        content: String,
        mode: String,
        #[serde(default)]
        images: Vec<Option<ImageAttachment>>,
        /// Client working directory; used as the turn workspace when the
        /// target session has none bound.
        #[serde(default)]
        cwd: Option<std::path::PathBuf>,
        /// Target session id. Defaults to the global current session; when
        /// set, the turn runs there without moving the current pointer.
        #[serde(default)]
        session_id: Option<String>,
    },
    QueueTurnUpdate {
        run_id: String,
        turn_id: String,
        content: String,
        display_content: String,
        #[serde(default)]
        images: Vec<Option<ImageAttachment>>,
        #[serde(default)]
        supersede: bool,
    },
    Cancel {
        run_id: String,
    },
    AnswerQuestion {
        question_id: String,
        answers: QuestionAnswers,
    },
    /// Resolve a question without an answer, when the client cannot present it
    /// at all. Distinct from `Cancel`: the turn keeps going and the tool that
    /// asked simply learns nobody answered.
    CloseQuestion {
        question_id: String,
    },
    ListSessions {
        #[serde(default)]
        include_archived: bool,
    },
    CreateSession {
        #[serde(default)]
        name: Option<String>,
        #[serde(default)]
        switch: bool,
        /// `user` (default) or `ask`; anything else is rejected. `ask` sessions
        /// back one-shot turns and stay out of every listing.
        #[serde(default)]
        kind: Option<String>,
    },
    /// Session the REPL was last on. Falls back to the current session and
    /// heals the stored pointer when it has gone stale.
    GetReplSession,
    SetReplSession {
        target: SessionRef,
    },
    SwitchSession {
        target: SessionRef,
    },
    RenameSession {
        target: SessionRef,
        name: String,
    },
    ArchiveSession {
        target: SessionRef,
        archived: bool,
    },
    DeleteSession {
        target: SessionRef,
    },
    SetWorkspace {
        target: SessionRef,
        #[serde(default)]
        path: Option<std::path::PathBuf>,
    },
    /// Pins the target session to its own model pool. An empty list clears
    /// the override so the session follows the global active pool again.
    SetSessionModels {
        target: SessionRef,
        #[serde(default)]
        models: Vec<crate::config::ActiveProviderModelConfig>,
    },
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ImageAttachment {
    Binary { mime: String, data: Vec<u8> },
    Path { path: String },
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Frame {
    Ready {
        pid: u32,
        #[serde(default)]
        web_port: u16,
        #[serde(default)]
        web_public: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        web_bind: Option<std::net::IpAddr>,
        #[serde(default)]
        build_id: String,
    },
    Accepted {
        run_id: String,
        /// Present when attaching to an already-running turn (FollowRun).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        turn_id: Option<String>,
    },
    TurnUpdateAccepted {
        run_id: String,
        turn_id: String,
        prompt_id: String,
        seq: i64,
        submitted_at: String,
    },
    Event {
        id: u64,
        kind: String,
        data: Value,
    },
    Ack,
    AdminResult {
        state: SessionState,
        data: Value,
    },
    Error {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        code: Option<ErrorCode>,
        message: String,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    Busy,
    #[serde(other)]
    Unknown,
}

impl Frame {
    pub fn error(message: impl Into<String>) -> Self {
        Self::Error {
            code: None,
            message: message.into(),
        }
    }

    pub fn coded_error(code: ErrorCode, message: impl Into<String>) -> Self {
        Self::Error {
            code: Some(code),
            message: message.into(),
        }
    }
}

pub async fn connect(path: &Path) -> Result<UnixStream> {
    UnixStream::connect(path)
        .await
        .with_context(|| format!("connecting to Miyu core at {}", path.display()))
}

pub async fn daemon_info(paths: &MiyuPaths) -> Option<DaemonInfo> {
    let socket = paths.ipc_socket();
    let frame = ping_daemon(&socket, PROTOCOL_VERSION).await?;
    match frame {
        Frame::Ready {
            pid,
            web_port,
            web_public,
            web_bind,
            build_id,
        } => Some(DaemonInfo {
            pid,
            web_port,
            web_public,
            web_bind,
            build_id,
            protocol_version: PROTOCOL_VERSION,
        }),
        Frame::Error { message, .. } => {
            let protocol_version = expected_protocol_version(&message)?;
            let Frame::Ready {
                pid,
                web_port,
                web_public,
                web_bind,
                build_id,
            } = ping_daemon(&socket, protocol_version).await?
            else {
                return None;
            };
            Some(DaemonInfo {
                pid,
                web_port,
                web_public,
                web_bind,
                build_id,
                protocol_version,
            })
        }
        _ => None,
    }
}

async fn ping_daemon(path: &Path, protocol_version: u16) -> Option<Frame> {
    let mut stream = tokio::time::timeout(Duration::from_millis(250), connect(path))
        .await
        .ok()?
        .ok()?;
    send(
        &mut stream,
        &Request {
            version: protocol_version,
            command: Command::Ping,
        },
    )
    .await
    .ok()?;
    tokio::time::timeout(Duration::from_millis(250), receive::<Frame>(&mut stream))
        .await
        .ok()?
        .ok()?
}

fn expected_protocol_version(message: &str) -> Option<u16> {
    message
        .strip_prefix("unsupported IPC protocol version ")?
        .rsplit_once("; expected ")?
        .1
        .parse()
        .ok()
}

pub fn stage_managed_web_password(paths: &MiyuPaths, password: &str) -> Result<PathBuf> {
    validate_web_password(password)?;
    let path = paths.managed_web_password_dir().join(format!(
        "password-{}-{}",
        std::process::id(),
        rand::random::<u64>()
    ));
    write_private_state(&path, password.as_bytes())
        .with_context(|| format!("saving WebUI password to {}", path.display()))?;
    Ok(path)
}

pub fn stage_web_password_file(paths: &MiyuPaths, source: &Path) -> Result<PathBuf> {
    let contents = std::fs::read_to_string(source)
        .with_context(|| format!("reading WebUI password file: {}", source.display()))?;
    stage_managed_web_password(paths, contents.trim_end_matches(['\r', '\n']))
}

fn validate_web_password(password: &str) -> Result<()> {
    if password.is_empty() {
        bail!("WebUI password cannot be empty");
    }
    if password.chars().count() > 1_024 {
        bail!("WebUI password cannot exceed 1,024 characters");
    }
    Ok(())
}

fn try_load_daemon_launch_config(paths: &MiyuPaths) -> Result<Option<DaemonLaunchConfig>> {
    let path = paths.daemon_launch_state_file();
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).with_context(|| format!("reading {}", path.display())),
    };
    serde_json::from_slice(&bytes)
        .map(Some)
        .with_context(|| format!("parsing daemon launch state at {}", path.display()))
}

fn load_daemon_launch_config(paths: &MiyuPaths) -> Result<DaemonLaunchConfig> {
    let mut config = try_load_daemon_launch_config(paths)?.unwrap_or_default();
    let Some(password_file) = config.password_file.as_ref() else {
        return Ok(config);
    };
    if password_file.exists() {
        return Ok(config);
    }
    let Some(name) = password_file.file_name() else {
        return Ok(config);
    };
    let migrated = paths.managed_web_password_dir().join(name);
    if password_file
        .parent()
        .and_then(Path::file_name)
        .is_some_and(|parent| parent == "web-passwords")
        && migrated.exists()
    {
        config.password_file = Some(migrated);
    } else if name == "web-password" {
        let old_managed = paths.state_dir.join(name);
        if old_managed.exists() {
            config.password_file = Some(stage_web_password_file(paths, &old_managed)?);
        }
    }
    Ok(config)
}

pub(crate) fn daemon_launch_config_with_port(
    paths: &MiyuPaths,
    port: u16,
) -> Result<DaemonLaunchConfig> {
    let mut config = load_daemon_launch_config(paths)?;
    config.port = port;
    Ok(config)
}

fn save_daemon_launch_config(paths: &MiyuPaths, config: &DaemonLaunchConfig) -> Result<()> {
    let path = paths.daemon_launch_state_file();
    let mut bytes = serde_json::to_vec(config)?;
    bytes.push(b'\n');
    write_private_state(&path, &bytes)
        .with_context(|| format!("saving daemon launch state to {}", path.display()))
}

fn commit_daemon_launch_config(paths: &MiyuPaths, config: &DaemonLaunchConfig) -> Result<()> {
    let previous = try_load_daemon_launch_config(paths)?;
    save_daemon_launch_config(paths, config)?;
    if let Some(old_password) = previous.and_then(|value| value.password_file) {
        if config.password_file.as_ref() != Some(&old_password) {
            remove_managed_password(paths, &old_password);
        }
    }
    Ok(())
}

fn abandon_daemon_launch_candidate(paths: &MiyuPaths, config: &DaemonLaunchConfig) {
    let persisted_password = try_load_daemon_launch_config(paths)
        .ok()
        .flatten()
        .and_then(|value| value.password_file);
    if let Some(candidate) = &config.password_file {
        if persisted_password.as_ref() != Some(candidate) {
            remove_managed_password(paths, candidate);
        }
    }
}

fn remove_managed_password(paths: &MiyuPaths, path: &Path) {
    if path.parent() == Some(paths.managed_web_password_dir().as_path()) {
        let _ = std::fs::remove_file(path);
    }
}

fn remap_managed_password(
    config: &mut DaemonLaunchConfig,
    previous: &MiyuPaths,
    current: &MiyuPaths,
) {
    let Some(path) = config.password_file.as_ref() else {
        return;
    };
    if path.parent() == Some(previous.managed_web_password_dir().as_path()) {
        if let Some(name) = path.file_name() {
            config.password_file = Some(current.managed_web_password_dir().join(name));
        }
    }
}

fn write_private_state(path: &Path, contents: &[u8]) -> Result<()> {
    let parent = path.parent().context("Miyu state file has no parent")?;
    std::fs::create_dir_all(parent)?;
    std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))?;
    let temporary = parent.join(format!(
        ".{}.tmp-{}-{}",
        path.file_name().unwrap_or_default().to_string_lossy(),
        std::process::id(),
        rand::random::<u64>()
    ));
    let result = (|| -> Result<()> {
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(&temporary)?;
        std::io::Write::write_all(&mut file, contents)?;
        file.sync_all()?;
        std::fs::rename(&temporary, path)?;
        let directory_sync = File::open(parent).and_then(|directory| directory.sync_all());
        finish_private_state_commit(parent, directory_sync)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result
}

fn finish_private_state_commit(parent: &Path, directory_sync: std::io::Result<()>) -> Result<()> {
    if let Err(error) = directory_sync {
        tracing::warn!(
            directory = %parent.display(),
            error = %error,
            "Miyu state file was committed, but syncing its parent directory failed"
        );
    }
    Ok(())
}

#[cfg(target_os = "linux")]
enum LegacyPassword {
    File(PathBuf),
    Inline(String),
}

#[cfg(target_os = "linux")]
struct LegacyDaemonArgs {
    port: u16,
    password: Option<LegacyPassword>,
}

#[cfg(target_os = "linux")]
fn parse_legacy_daemon_cmdline(cmdline: &[u8]) -> Result<LegacyDaemonArgs> {
    let args = cmdline
        .split(|byte| *byte == 0)
        .filter(|arg| !arg.is_empty())
        .collect::<Vec<_>>();
    let mut parsed = LegacyDaemonArgs {
        port: DEFAULT_WEB_PORT,
        password: None,
    };
    let mut index = 0;
    while index < args.len() {
        let arg = args[index];
        if arg == b"--port" {
            index += 1;
            let value = args
                .get(index)
                .context("legacy daemon --port has no value")?;
            parsed.port = parse_legacy_port(value)?;
        } else if let Some(value) = arg.strip_prefix(b"--port=") {
            parsed.port = parse_legacy_port(value)?;
        } else if arg == b"--password-file" {
            index += 1;
            let value = args
                .get(index)
                .context("legacy daemon --password-file has no value")?;
            parsed.password = Some(LegacyPassword::File(PathBuf::from(OsString::from_vec(
                value.to_vec(),
            ))));
        } else if let Some(value) = arg.strip_prefix(b"--password-file=") {
            parsed.password = Some(LegacyPassword::File(PathBuf::from(OsString::from_vec(
                value.to_vec(),
            ))));
        } else if arg == b"--password" {
            index += 1;
            let value = args
                .get(index)
                .context("legacy daemon --password has no value")?;
            parsed.password = Some(LegacyPassword::Inline(parse_legacy_password(value)?));
        } else if let Some(value) = arg.strip_prefix(b"--password=") {
            parsed.password = Some(LegacyPassword::Inline(parse_legacy_password(value)?));
        }
        index += 1;
    }
    Ok(parsed)
}

#[cfg(target_os = "linux")]
fn parse_legacy_port(value: &[u8]) -> Result<u16> {
    std::str::from_utf8(value)
        .context("legacy daemon port is not UTF-8")?
        .parse()
        .context("legacy daemon port is invalid")
}

#[cfg(target_os = "linux")]
fn parse_legacy_password(value: &[u8]) -> Result<String> {
    String::from_utf8(value.to_vec()).context("legacy daemon password is not UTF-8")
}

#[cfg(target_os = "linux")]
fn recover_legacy_daemon_launch(paths: &MiyuPaths, pid: u32) -> Result<DaemonLaunchConfig> {
    let cmdline = std::fs::read(format!("/proc/{pid}/cmdline"))
        .context("reading legacy Miyu daemon arguments")?;
    let cwd = std::fs::read_link(format!("/proc/{pid}/cwd"))
        .context("reading legacy Miyu daemon working directory")?;
    recover_legacy_daemon_launch_from_cmdline(paths, &cmdline, Some(&cwd))
}

#[cfg(target_os = "linux")]
fn recover_legacy_daemon_launch_from_cmdline(
    paths: &MiyuPaths,
    cmdline: &[u8],
    cwd: Option<&Path>,
) -> Result<DaemonLaunchConfig> {
    let parsed = parse_legacy_daemon_cmdline(cmdline)?;
    let password_file = match parsed.password {
        Some(LegacyPassword::File(path)) => {
            let path = if path.is_relative() {
                cwd.context("legacy daemon password file is relative but its cwd is unavailable")?
                    .join(path)
            } else {
                path
            };
            Some(stage_web_password_file(paths, &path)?)
        }
        Some(LegacyPassword::Inline(password)) => {
            Some(stage_managed_web_password(paths, &password)?)
        }
        None => None,
    };
    Ok(DaemonLaunchConfig {
        port: parsed.port,
        password_file,
        // Legacy daemons predate --bind and listened on 0.0.0.0; recovery
        // now leaves the (secure) loopback default in place.
        bind: None,
    })
}

#[cfg(not(target_os = "linux"))]
fn recover_legacy_daemon_launch(_paths: &MiyuPaths, _pid: u32) -> Result<DaemonLaunchConfig> {
    Ok(DaemonLaunchConfig::default())
}

pub fn recover_daemon_launch_if_missing(
    paths: &MiyuPaths,
    pid: u32,
) -> Result<Option<DaemonLaunchConfig>> {
    if try_load_daemon_launch_config(paths)?.is_some() {
        Ok(None)
    } else {
        recover_legacy_daemon_launch(paths, pid).map(Some)
    }
}

pub fn discard_daemon_launch_candidate(paths: &MiyuPaths, config: &DaemonLaunchConfig) {
    abandon_daemon_launch_candidate(paths, config);
}

pub async fn ensure_daemon(
    paths: &MiyuPaths,
    requested: Option<&DaemonLaunchConfig>,
) -> Result<DaemonInfo> {
    let mut active_paths = paths.clone();
    let mut pending_launch = requested.cloned();
    let mut current = daemon_info(&active_paths).await;
    if current.is_none() {
        let previous_paths = active_paths.clone();
        active_paths = match MiyuPaths::new().context("refreshing Miyu paths before daemon startup")
        {
            Ok(paths) => paths,
            Err(error) => {
                if let Some(launch) = &pending_launch {
                    abandon_daemon_launch_candidate(&previous_paths, launch);
                }
                return Err(error);
            }
        };
        if let Some(launch) = &mut pending_launch {
            remap_managed_password(launch, &previous_paths, &active_paths);
        }
        current = daemon_info(&active_paths).await;
    }
    if let Some(info) = current {
        if info.build_id == BUILD_ID {
            if let Some(launch) = &pending_launch {
                abandon_daemon_launch_candidate(&active_paths, launch);
            }
            return Ok(info);
        }
        if pending_launch.is_none() {
            pending_launch = recover_daemon_launch_if_missing(&active_paths, info.pid)?;
        }
        let previous_paths = active_paths.clone();
        if let Err(error) = restart_stale_daemon(&active_paths, &info).await {
            if let Some(launch) = &pending_launch {
                abandon_daemon_launch_candidate(&active_paths, launch);
            }
            return Err(error);
        }
        active_paths = match MiyuPaths::new().context("refreshing Miyu paths after daemon shutdown")
        {
            Ok(paths) => paths,
            Err(error) => {
                if let Some(launch) = &pending_launch {
                    abandon_daemon_launch_candidate(&previous_paths, launch);
                }
                return Err(error);
            }
        };
        if let Some(launch) = &mut pending_launch {
            remap_managed_password(launch, &previous_paths, &active_paths);
        }
    }
    let _starter = loop {
        let starter = acquire_starter(&active_paths)?;
        let Some(info) = daemon_info(&active_paths).await else {
            break starter;
        };
        if info.build_id == BUILD_ID {
            if let Some(launch) = &pending_launch {
                abandon_daemon_launch_candidate(&active_paths, launch);
            }
            return Ok(info);
        }
        if pending_launch.is_none() {
            pending_launch = recover_daemon_launch_if_missing(&active_paths, info.pid)?;
        }
        let previous_paths = active_paths.clone();
        if let Err(error) = restart_stale_daemon(&active_paths, &info).await {
            if let Some(launch) = &pending_launch {
                abandon_daemon_launch_candidate(&active_paths, launch);
            }
            return Err(error);
        }
        drop(starter);
        active_paths = match MiyuPaths::new().context("refreshing Miyu paths after daemon shutdown")
        {
            Ok(paths) => paths,
            Err(error) => {
                if let Some(launch) = &pending_launch {
                    abandon_daemon_launch_candidate(&previous_paths, launch);
                }
                return Err(error);
            }
        };
        if let Some(launch) = &mut pending_launch {
            remap_managed_password(launch, &previous_paths, &active_paths);
        }
    };
    let launch = pending_launch
        .map(Ok)
        .unwrap_or_else(|| load_daemon_launch_config(&active_paths))?;
    let mut child = match start_daemon_process(&active_paths, &launch) {
        Ok(child) => child,
        Err(error) => {
            abandon_daemon_launch_candidate(&active_paths, &launch);
            return Err(error);
        }
    };
    let deadline = tokio::time::Instant::now() + Duration::from_secs(8);
    loop {
        if let Some(info) = daemon_info(&active_paths).await {
            if let Err(error) = commit_daemon_launch_config(&active_paths, &launch) {
                let _ = child.kill();
                let _ = child.wait();
                abandon_daemon_launch_candidate(&active_paths, &launch);
                return Err(error);
            }
            spawn_daemon_reaper(child);
            return Ok(info);
        }
        match child.try_wait().context("checking Miyu daemon process") {
            Ok(Some(status)) => {
                abandon_daemon_launch_candidate(&active_paths, &launch);
                bail!("Miyu daemon exited before becoming ready ({status})");
            }
            Ok(None) => {}
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                abandon_daemon_launch_candidate(&active_paths, &launch);
                return Err(error);
            }
        }
        if tokio::time::Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            abandon_daemon_launch_candidate(&active_paths, &launch);
            bail!("Miyu daemon did not become ready within 8 seconds");
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Shuts down a daemon left over from an older build so the caller can spawn
/// one matching the current binary.
async fn restart_stale_daemon(paths: &MiyuPaths, info: &DaemonInfo) -> Result<()> {
    shutdown_daemon(paths, info)
        .await
        .context("waiting for the outdated Miyu daemon to stop")
}

pub async fn shutdown_daemon(paths: &MiyuPaths, info: &DaemonInfo) -> Result<()> {
    let process = daemon_process_identity(info.pid);
    let mut stream = connect(&paths.ipc_socket()).await?;
    send(
        &mut stream,
        &Request {
            version: info.protocol_version,
            command: Command::Shutdown,
        },
    )
    .await?;
    let _ = receive::<Frame>(&mut stream).await;
    wait_for_daemon_exit(process, DAEMON_SHUTDOWN_TIMEOUT).await
}

pub fn daemon_process_identity(pid: u32) -> DaemonProcessIdentity {
    DaemonProcessIdentity {
        pid,
        #[cfg(target_os = "linux")]
        start_time: linux_process_state(pid).map(|(_, start_time)| start_time),
    }
}

pub async fn wait_for_daemon_exit(process: DaemonProcessIdentity, timeout: Duration) -> Result<()> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if !daemon_process_matches(process) {
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            bail!(
                "Miyu daemon PID {} did not stop within {} seconds",
                process.pid,
                timeout.as_secs()
            );
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

#[cfg(target_os = "linux")]
fn daemon_process_matches(process: DaemonProcessIdentity) -> bool {
    let Some((state, start_time)) = linux_process_state(process.pid) else {
        return false;
    };
    state != 'Z'
        && process
            .start_time
            .is_none_or(|expected| expected == start_time)
}

#[cfg(all(unix, not(target_os = "linux")))]
fn daemon_process_matches(process: DaemonProcessIdentity) -> bool {
    if process.pid == 0 {
        return false;
    }
    let result = unsafe { libc::kill(process.pid as libc::pid_t, 0) };
    result == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(not(unix))]
fn daemon_process_matches(_process: DaemonProcessIdentity) -> bool {
    false
}

#[cfg(target_os = "linux")]
fn linux_process_state(pid: u32) -> Option<(char, u64)> {
    if pid == 0 {
        return None;
    }
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let fields = stat
        .rsplit_once(") ")?
        .1
        .split_whitespace()
        .collect::<Vec<_>>();
    let state = fields.first()?.chars().next()?;
    // `fields[0]` is procfs field 3 (state); starttime is field 22.
    let start_time = fields.get(19)?.parse().ok()?;
    Some((state, start_time))
}

fn acquire_starter(paths: &MiyuPaths) -> Result<StarterLease> {
    prepare_runtime_dir(paths)?;
    let lock_file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(paths.daemon_start_lock())?;
    let result = unsafe { libc::flock(lock_file.as_raw_fd(), libc::LOCK_EX) };
    if result != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(StarterLease { lock_file })
}

fn start_daemon_process(
    paths: &MiyuPaths,
    launch: &DaemonLaunchConfig,
) -> Result<std::process::Child> {
    std::fs::create_dir_all(paths.logs_dir())?;
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(paths.logs_dir().join("daemon.log"))?;
    // The daemon is this very binary re-executed with a hidden subcommand,
    // so a single installed file is always sufficient.
    let executable =
        std::env::current_exe().context("resolving the Miyu executable to spawn the daemon")?;
    let mut command = std::process::Command::new(executable);
    command.arg("__daemon");
    append_daemon_process_args(&mut command, launch);
    command
        .stdin(Stdio::null())
        .stdout(Stdio::from(log.try_clone()?))
        .stderr(Stdio::from(log));
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    command.spawn().context("starting Miyu daemon")
}

fn spawn_daemon_reaper(mut child: std::process::Child) {
    // Reap the daemon when it eventually exits: long-lived parents (the
    // REPL) would otherwise accumulate a zombie per spawned daemon.
    std::thread::spawn(move || {
        let _ = child.wait();
    });
}

fn append_daemon_process_args(command: &mut std::process::Command, launch: &DaemonLaunchConfig) {
    command.arg("--port").arg(launch.port.to_string());
    if let Some(path) = &launch.password_file {
        command.arg("--password-file").arg(path);
    }
    if let Some(bind) = &launch.bind {
        command.arg("--bind").arg(bind.to_string());
    }
}

pub async fn send<T: Serialize>(stream: &mut UnixStream, value: &T) -> Result<()> {
    let bytes = serde_json::to_vec(value)?;
    if bytes.len() > MAX_FRAME_BYTES {
        bail!("IPC frame exceeds the 24 MiB limit");
    }
    stream.write_u32(bytes.len() as u32).await?;
    stream.write_all(&bytes).await?;
    stream.flush().await?;
    Ok(())
}

pub async fn receive<T: DeserializeOwned>(stream: &mut UnixStream) -> Result<Option<T>> {
    let length = match stream.read_u32().await {
        Ok(length) => length as usize,
        Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    if length == 0 || length > MAX_FRAME_BYTES {
        bail!("invalid IPC frame length: {length}");
    }
    let mut bytes = vec![0; length];
    stream.read_exact(&mut bytes).await?;
    Ok(Some(serde_json::from_slice(&bytes)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_protocol_is_explicitly_versioned() {
        let value = serde_json::to_value(Request::new(Command::Ping)).unwrap();
        assert_eq!(value["version"], PROTOCOL_VERSION);
        assert_eq!(value["command"], "ping");
    }

    #[test]
    fn ready_frame_exposes_daemon_web_state() {
        let value = serde_json::to_value(Frame::Ready {
            pid: 42,
            web_port: 4096,
            web_public: false,
            web_bind: None,
            build_id: "test-build".to_string(),
        })
        .unwrap();
        assert_eq!(value["type"], "ready");
        assert_eq!(value["pid"], 42);
        assert_eq!(value["web_port"], 4096);
        assert_eq!(value["web_public"].as_bool(), Some(false));
    }

    #[test]
    fn error_frames_keep_backward_compatibility_and_expose_codes() {
        let legacy: Frame = serde_json::from_value(serde_json::json!({
            "type": "error",
            "message": ADMIN_BUSY_MESSAGE,
        }))
        .unwrap();
        assert!(matches!(
            legacy,
            Frame::Error {
                code: None,
                message,
            } if message == ADMIN_BUSY_MESSAGE
        ));

        let coded =
            serde_json::to_value(Frame::coded_error(ErrorCode::Busy, ADMIN_BUSY_MESSAGE)).unwrap();
        assert_eq!(coded["type"], "error");
        assert_eq!(coded["code"], "busy");
        assert_eq!(coded["message"], ADMIN_BUSY_MESSAGE);

        let future: Frame = serde_json::from_value(serde_json::json!({
            "type": "error",
            "code": "future_error",
            "message": "future failure",
        }))
        .unwrap();
        assert!(matches!(
            future,
            Frame::Error {
                code: Some(ErrorCode::Unknown),
                ..
            }
        ));
    }

    #[test]
    fn daemon_process_prefers_the_default_web_port_unless_overridden() {
        let mut default = std::process::Command::new("miyu");
        append_daemon_process_args(&mut default, &DaemonLaunchConfig::default());
        let default_args = default
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert_eq!(default_args, ["--port", "8300"]);

        let supplied = DaemonLaunchConfig {
            port: 9400,
            password_file: Some(PathBuf::from("/private/password")),
            bind: None,
        };
        let mut overridden = std::process::Command::new("miyu");
        append_daemon_process_args(&mut overridden, &supplied);
        let overridden_args = overridden
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert_eq!(
            overridden_args,
            ["--port", "9400", "--password-file", "/private/password"]
        );
        assert!(overridden_args.iter().all(|arg| !arg.contains("secret")));
    }

    fn test_paths(root: &Path) -> MiyuPaths {
        MiyuPaths {
            root_dir: root.to_path_buf(),
            config_dir: root.join("config"),
            config_file: root.join("config/config.jsonc"),
            skills_dir: root.join("config/skills"),
            data_dir: root.join("data"),
            cache_dir: root.join("cache"),
            state_dir: root.join("state"),
            pictures_dir: root.join("pictures"),
            fish_hook_file: root.join("fish/miyu.fish"),
            bash_hook_file: root.join("shell/bash-hook.sh"),
            zsh_hook_file: root.join("shell/zsh-hook.zsh"),
            scripts_dir: root.join("config/scripts"),
            system_scripts_dir: root.join("system/scripts"),
        }
    }

    #[test]
    fn daemon_launch_state_contains_only_port_and_password_file() {
        let temp = tempfile::tempdir().unwrap();
        let paths = test_paths(temp.path());
        let password_path = stage_managed_web_password(&paths, "very-secret").unwrap();
        let config = DaemonLaunchConfig {
            port: 9400,
            password_file: Some(password_path.clone()),
            bind: None,
        };
        save_daemon_launch_config(&paths, &config).unwrap();

        assert_eq!(load_daemon_launch_config(&paths).unwrap(), config);
        let state = std::fs::read_to_string(paths.daemon_launch_state_file()).unwrap();
        assert!(!state.contains("very-secret"));
        assert_eq!(
            std::fs::metadata(password_path)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        assert_eq!(
            std::fs::metadata(paths.daemon_launch_state_file())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }

    #[test]
    fn directory_sync_failure_after_rename_keeps_the_commit_successful() {
        let error = std::io::Error::new(std::io::ErrorKind::Other, "injected sync failure");

        assert!(finish_private_state_commit(Path::new("/private/state"), Err(error)).is_ok());
    }

    #[test]
    fn bare_launch_restores_the_saved_port_and_password_file() {
        let temp = tempfile::tempdir().unwrap();
        let paths = test_paths(temp.path());
        let password = stage_managed_web_password(&paths, "saved-secret").unwrap();
        let saved = DaemonLaunchConfig {
            port: 9412,
            password_file: Some(password.clone()),
            bind: None,
        };
        save_daemon_launch_config(&paths, &saved).unwrap();

        let restored = load_daemon_launch_config(&paths).unwrap();
        assert_eq!(restored, saved);
        let mut command = std::process::Command::new("miyu");
        append_daemon_process_args(&mut command, &restored);
        assert_eq!(
            command.get_args().collect::<Vec<_>>(),
            [
                std::ffi::OsStr::new("--port"),
                std::ffi::OsStr::new("9412"),
                std::ffi::OsStr::new("--password-file"),
                password.as_os_str(),
            ]
        );
    }

    #[test]
    fn port_override_preserves_the_saved_password_file() {
        let temp = tempfile::tempdir().unwrap();
        let paths = test_paths(temp.path());
        let password = stage_managed_web_password(&paths, "saved-secret").unwrap();
        let saved = DaemonLaunchConfig {
            port: 8300,
            password_file: Some(password.clone()),
            bind: None,
        };
        save_daemon_launch_config(&paths, &saved).unwrap();

        let overridden = daemon_launch_config_with_port(&paths, 9412).unwrap();

        assert_eq!(overridden.port, 9412);
        assert_eq!(overridden.password_file, Some(password));
    }

    #[test]
    fn persisted_launch_state_takes_precedence_over_proc_recovery() {
        let temp = tempfile::tempdir().unwrap();
        let paths = test_paths(temp.path());
        let saved = DaemonLaunchConfig {
            port: 9412,
            password_file: None,
            bind: None,
        };
        save_daemon_launch_config(&paths, &saved).unwrap();

        assert!(recover_daemon_launch_if_missing(&paths, u32::MAX)
            .unwrap()
            .is_none());
        assert_eq!(load_daemon_launch_config(&paths).unwrap(), saved);
    }

    #[test]
    fn abandoned_password_candidate_does_not_replace_the_committed_password() {
        let temp = tempfile::tempdir().unwrap();
        let paths = test_paths(temp.path());
        let old_password = stage_managed_web_password(&paths, "old-secret").unwrap();
        let old_launch = DaemonLaunchConfig {
            port: 8300,
            password_file: Some(old_password.clone()),
            bind: None,
        };
        commit_daemon_launch_config(&paths, &old_launch).unwrap();

        let candidate = stage_managed_web_password(&paths, "new-secret").unwrap();
        let failed_launch = DaemonLaunchConfig {
            port: 9400,
            password_file: Some(candidate.clone()),
            bind: None,
        };
        abandon_daemon_launch_candidate(&paths, &failed_launch);

        assert_eq!(load_daemon_launch_config(&paths).unwrap(), old_launch);
        assert_eq!(std::fs::read_to_string(old_password).unwrap(), "old-secret");
        assert!(!candidate.exists());
    }

    #[test]
    fn committing_a_new_password_cleans_the_previous_managed_file() {
        let temp = tempfile::tempdir().unwrap();
        let paths = test_paths(temp.path());
        let old_password = stage_managed_web_password(&paths, "old-secret").unwrap();
        commit_daemon_launch_config(
            &paths,
            &DaemonLaunchConfig {
                port: 8300,
                password_file: Some(old_password.clone()),
            bind: None,
            },
        )
        .unwrap();
        let new_password = stage_managed_web_password(&paths, "new-secret").unwrap();
        let new_launch = DaemonLaunchConfig {
            port: 9400,
            password_file: Some(new_password.clone()),
            bind: None,
        };

        commit_daemon_launch_config(&paths, &new_launch).unwrap();

        assert!(!old_password.exists());
        assert_eq!(std::fs::read_to_string(new_password).unwrap(), "new-secret");
        assert_eq!(load_daemon_launch_config(&paths).unwrap(), new_launch);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn legacy_inline_password_and_port_are_recovered_into_managed_state() {
        let temp = tempfile::tempdir().unwrap();
        let paths = test_paths(temp.path());
        let cmdline = b"/usr/bin/miyu\0__daemon\0--port\09412\0--password=legacy-secret\0";

        let recovered = recover_legacy_daemon_launch_from_cmdline(&paths, cmdline, None).unwrap();

        assert_eq!(recovered.port, 9412);
        let password = recovered.password_file.unwrap();
        let password_dir = paths.managed_web_password_dir();
        assert_eq!(password.parent(), Some(password_dir.as_path()));
        assert_eq!(std::fs::read_to_string(password).unwrap(), "legacy-secret");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn legacy_relative_password_file_is_copied_from_the_daemon_cwd() {
        let temp = tempfile::tempdir().unwrap();
        let paths = test_paths(temp.path());
        std::fs::write(temp.path().join("external-password"), "file-secret\n").unwrap();
        let cmdline = b"miyu\0__daemon\0--port=9500\0--password-file\0external-password\0";

        let recovered =
            recover_legacy_daemon_launch_from_cmdline(&paths, cmdline, Some(temp.path())).unwrap();

        assert_eq!(recovered.port, 9500);
        let password = recovered.password_file.unwrap();
        assert_ne!(password, temp.path().join("external-password"));
        assert_eq!(std::fs::read_to_string(password).unwrap(), "file-secret");
    }

    #[test]
    fn admin_commands_round_trip_with_explicit_state() {
        let request = Request::new(Command::ResetConversation {
            target: SessionRef::Id {
                id: "sess_local".to_string(),
            },
        });
        let value = serde_json::to_value(request).unwrap();
        assert_eq!(value["command"], "reset_conversation");
        assert_eq!(value["target"]["kind"], "id");
        assert_eq!(value["target"]["id"], "sess_local");
        assert_eq!(PROTOCOL_VERSION, 3);

        let frame = Frame::AdminResult {
            state: SessionState {
                context_tokens: 12,
                context_window: Some(1000),
                cumulative_tokens: 34,
                cumulative_prompt_tokens: 20,
                cumulative_cache_read_tokens: 10,
                session_id: "default".to_string(),
                session_name: "默认会话".to_string(),
                workspace: None,
            },
            data: serde_json::json!({"ok": true}),
        };
        let value = serde_json::to_value(frame).unwrap();
        assert_eq!(value["type"], "admin_result");
        assert_eq!(value["state"]["cumulative_tokens"], 34);
    }

    #[test]
    fn parses_protocol_version_from_daemon_rejection() {
        assert_eq!(
            expected_protocol_version("unsupported IPC protocol version 3; expected 2"),
            Some(2)
        );
        assert_eq!(expected_protocol_version("unrelated error"), None);
    }

    #[tokio::test]
    async fn framed_protocol_round_trips_over_unix_socket() {
        let (mut left, mut right) = UnixStream::pair().unwrap();
        let request = Request::new(Command::StartTurn {
            content: "hello".to_string(),
            mode: "normal".to_string(),
            images: vec![Some(ImageAttachment::Binary {
                mime: "image/png".to_string(),
                data: vec![1, 2, 3],
            })],
            cwd: Some(std::path::PathBuf::from("/tmp/workdir")),
            session_id: Some("sess_test".to_string()),
        });
        let writer = tokio::spawn(async move { send(&mut left, &request).await });
        let received = receive::<Request>(&mut right).await.unwrap().unwrap();
        writer.await.unwrap().unwrap();

        assert_eq!(received.version, PROTOCOL_VERSION);
        match received.command {
            Command::StartTurn {
                content,
                mode,
                images,
                cwd,
                session_id,
            } => {
                assert_eq!(content, "hello");
                assert_eq!(mode, "normal");
                assert_eq!(images.len(), 1);
                assert_eq!(cwd, Some(std::path::PathBuf::from("/tmp/workdir")));
                assert_eq!(session_id.as_deref(), Some("sess_test"));
            }
            _ => panic!("unexpected command"),
        }
    }

    #[tokio::test]
    async fn oversized_frame_is_rejected_before_writing() {
        let (mut left, _right) = UnixStream::pair().unwrap();
        let request = Request::new(Command::StartTurn {
            content: "x".repeat(MAX_FRAME_BYTES),
            mode: "normal".to_string(),
            images: Vec::new(),
            cwd: None,
            session_id: None,
        });
        assert!(send(&mut left, &request).await.is_err());
    }

    #[test]
    fn direct_core_lease_is_exclusive() {
        let temp = tempfile::tempdir().unwrap();
        let lock = temp.path().join("core.lock");
        let first = acquire_direct_core_at(lock.clone()).unwrap();
        assert!(acquire_direct_core_at(lock.clone()).is_err());
        drop(first);
        assert!(acquire_direct_core_at(lock).is_ok());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn daemon_identity_uses_linux_process_start_time() {
        let identity = daemon_process_identity(std::process::id());
        assert!(identity.start_time.is_some());
        assert!(daemon_process_matches(identity));
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn daemon_exit_wait_tracks_the_process_instead_of_ipc_files() {
        let mut child = std::process::Command::new("sh")
            .args(["-c", "sleep 0.1"])
            .spawn()
            .unwrap();
        let identity = daemon_process_identity(child.id());
        assert!(daemon_process_matches(identity));

        wait_for_daemon_exit(identity, Duration::from_secs(2))
            .await
            .unwrap();
        child.wait().unwrap();
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn daemon_exit_wait_times_out_while_the_same_process_is_alive() {
        let identity = daemon_process_identity(std::process::id());
        let error = wait_for_daemon_exit(identity, Duration::from_millis(30))
            .await
            .unwrap_err();
        assert!(error.to_string().contains(&std::process::id().to_string()));
    }
}
