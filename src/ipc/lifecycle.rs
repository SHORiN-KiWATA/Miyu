//! daemon 的启动、探活与关停。
//!
//! `ensure_daemon` 是幂等的：已经在跑就直接用，没在跑就拉起来并等它就绪。
//! 抢启动靠 `acquire_starter` 的文件锁——两个终端同时开会各起一个 daemon。
//!
//! 判活不能只看 PID 文件（`daemon_process_matches` 还要比对进程身份），因为
//! PID 会被复用；`restart_stale_daemon` 处理的是版本对不上的旧 daemon。

use crate::ipc::*;

pub const DAEMON_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Clone, Debug)]
pub struct DaemonInfo {
    pub pid: u32,
    pub web_port: u16,
    pub web_public: bool,
    pub web_bind: Option<std::net::IpAddr>,
    pub build_id: String,
    pub protocol_version: u16,
}

#[derive(Clone, Copy, Debug)]
pub struct DaemonProcessIdentity {
    pub(crate) pid: u32,
    #[cfg(target_os = "linux")]
    pub(crate) start_time: Option<u64>,
}

pub struct DirectCoreLease {
    pub(crate) lock_file: File,
}

pub struct WebCoreLease {
    pub(crate) lock_file: File,
    pub(crate) socket_path: PathBuf,
}

pub(crate) struct StarterLease {
    pub(crate) lock_file: File,
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

pub(crate) fn prepare_runtime_dir(paths: &MiyuPaths) -> Result<()> {
    let runtime_dir = paths.runtime_dir();
    std::fs::create_dir_all(&runtime_dir)?;
    std::fs::set_permissions(&runtime_dir, std::fs::Permissions::from_mode(0o700))?;
    Ok(())
}

pub(crate) fn acquire_direct_core_at(lock_path: PathBuf) -> Result<DirectCoreLease> {
    Ok(DirectCoreLease {
        lock_file: acquire_lock(lock_path)?,
    })
}

pub(crate) fn acquire_lock(lock_path: PathBuf) -> Result<File> {
    let lock_file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(lock_path)?;
    let result = unsafe { libc::flock(lock_file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if result != 0 {
        bail!(
            "{}",
            crate::i18n::text(
                "another Miyu core (the daemon or another direct REPL) holds this home; stop it (miyu daemon stop) or drop MIYU_DIRECT to attach to the daemon",
                "另一个 Miyu 核心(daemon 或另一个直连 REPL)正占用本机身份;直连模式与它互斥——先 miyu daemon stop,或去掉 MIYU_DIRECT 改为连接 daemon"
            )
        );
    }
    Ok(lock_file)
}

pub(crate) fn unlock(lock_file: &File) {
    unsafe {
        libc::flock(lock_file.as_raw_fd(), libc::LOCK_UN);
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

pub(crate) async fn ping_daemon(path: &Path, protocol_version: u16) -> Option<Frame> {
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
    // daemon 的 stdout/stderr 全进 daemon.log(见 start_daemon_process)。
    // 它是所有启动共用的追加文件,所以失败时不能 tail 固定行数——daemon 若
    // 死在任何输出之前,尾部拿到的是上一次**成功**启动的 "Miyu WebUI: …",
    // 用户会以为起来了。记下 spawn 前的字节长度,只读这之后新增的部分。
    let log_offset = daemon_log_len(&active_paths);
    let mut child = match start_daemon_process(&active_paths, &launch) {
        Ok(child) => child,
        Err(error) => {
            abandon_daemon_launch_candidate(&active_paths, &launch);
            return Err(error);
        }
    };
    let ready_timeout = daemon_ready_timeout();
    let deadline = tokio::time::Instant::now() + ready_timeout;
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
                bail!(
                    "Miyu daemon exited before becoming ready ({status}){}",
                    daemon_log_since(&active_paths, log_offset)
                );
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
            bail!(
                "Miyu daemon did not become ready within {} seconds (override with {DAEMON_READY_TIMEOUT_ENV}){}",
                ready_timeout.as_secs(),
                daemon_log_since(&active_paths, log_offset)
            );
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// daemon 就绪窗口的环境变量覆盖(秒)。默认 8 秒:daemon 自己的启动路径已经
/// 把慢步骤(MCP 列举)限在 3 秒内放行(见 `startup_context`),8 秒够用;
/// 留这个口子给机器特别慢、或想看清 daemon 到底卡在哪一步的人。
pub const DAEMON_READY_TIMEOUT_ENV: &str = "MIYU_DAEMON_READY_TIMEOUT_SECS";
const DEFAULT_DAEMON_READY_TIMEOUT: Duration = Duration::from_secs(8);

fn daemon_ready_timeout() -> Duration {
    std::env::var(DAEMON_READY_TIMEOUT_ENV)
        .ok()
        .and_then(|value| value.trim().parse::<u64>().ok())
        .filter(|secs| *secs > 0)
        .map(Duration::from_secs)
        .unwrap_or(DEFAULT_DAEMON_READY_TIMEOUT)
}

/// Shuts down a daemon left over from an older build so the caller can spawn
/// one matching the current binary.
pub(crate) async fn restart_stale_daemon(paths: &MiyuPaths, info: &DaemonInfo) -> Result<()> {
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
pub(crate) fn daemon_process_matches(process: DaemonProcessIdentity) -> bool {
    let Some((state, start_time)) = linux_process_state(process.pid) else {
        return false;
    };
    state != 'Z'
        && process
            .start_time
            .is_none_or(|expected| expected == start_time)
}

#[cfg(all(unix, not(target_os = "linux")))]
pub(crate) fn daemon_process_matches(process: DaemonProcessIdentity) -> bool {
    if process.pid == 0 {
        return false;
    }
    let result = unsafe { libc::kill(process.pid as libc::pid_t, 0) };
    result == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(not(unix))]
pub(crate) fn daemon_process_matches(_process: DaemonProcessIdentity) -> bool {
    false
}

#[cfg(target_os = "linux")]
pub(crate) fn linux_process_state(pid: u32) -> Option<(char, u64)> {
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

pub(crate) fn acquire_starter(paths: &MiyuPaths) -> Result<StarterLease> {
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

fn daemon_log_path(paths: &MiyuPaths) -> PathBuf {
    paths.logs_dir().join("daemon.log")
}

pub(in crate::ipc) fn daemon_log_len(paths: &MiyuPaths) -> u64 {
    std::fs::metadata(daemon_log_path(paths))
        .map(|meta| meta.len())
        .unwrap_or(0)
}

/// 本次启动往 daemon.log 里写了什么。只读 `offset` 之后新增的字节——共用
/// 追加文件,读全文或 tail 固定行数都会把别人的输出算到这次头上。
///
/// 08-29 用户反馈:所有需要 daemon 的命令都只吐 "exit status: 1",真正的原因
/// (`database disk image is malformed`)躺在日志里没人看得到。
pub(in crate::ipc) fn daemon_log_since(paths: &MiyuPaths, offset: u64) -> String {
    use std::io::{Read, Seek, SeekFrom};
    const MAX_BYTES: usize = 4096;
    let path = daemon_log_path(paths);
    let appended = (|| -> std::io::Result<String> {
        let mut file = std::fs::File::open(&path)?;
        let len = file.metadata()?.len();
        if len <= offset {
            return Ok(String::new());
        }
        // 只保留末尾一段:启动噪音可能很长,用户要的是最后那几句。
        let start = offset.max(len.saturating_sub(MAX_BYTES as u64));
        file.seek(SeekFrom::Start(start))?;
        let mut buffer = Vec::new();
        file.take(MAX_BYTES as u64).read_to_end(&mut buffer)?;
        Ok(String::from_utf8_lossy(&buffer).into_owned())
    })()
    .unwrap_or_default();
    let appended = appended.trim();
    if appended.is_empty() {
        // 一个字都没写就死了。指路比沉默强。
        return format!("\n(daemon 没有留下任何输出；日志：{})", path.display());
    }
    format!("\ndaemon 日志（{}）：\n{appended}", path.display())
}

pub(crate) fn start_daemon_process(
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
    let executable = crate::paths::miyu_executable()
        .context("resolving the Miyu executable to spawn the daemon")?;
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

pub(crate) fn spawn_daemon_reaper(mut child: std::process::Child) {
    // Reap the daemon when it eventually exits: long-lived parents (the
    // REPL) would otherwise accumulate a zombie per spawned daemon.
    std::thread::spawn(move || {
        let _ = child.wait();
    });
}

pub(crate) fn append_daemon_process_args(
    command: &mut std::process::Command,
    launch: &DaemonLaunchConfig,
) {
    command.arg("--port").arg(launch.port.to_string());
    if let Some(path) = &launch.password_file {
        command.arg("--password-file").arg(path);
    }
    if let Some(bind) = &launch.bind {
        command.arg("--bind").arg(bind.to_string());
    }
}
