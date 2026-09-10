//! daemon 的启动、停止、状态与配置热重载。
//!
//! 热重载走 IPC 而不是信号：要能拿到「重载成功了没、哪一项没通过校验」的回
//! 答。`retry_config_reload` 会重试，因为 daemon 可能正忙在一个回合里。
//!
//! `daemon_process_alive` 有两个实现（按平台），判活不能只看 PID 文件——进程
//! 挂了文件还在。

use crate::cli::*;

pub(in crate::cli) const RELOAD_MAX_ATTEMPTS: usize = 12;

pub(in crate::cli) const RELOAD_RETRY_INTERVAL: Duration = Duration::from_secs(5);

pub(in crate::cli) const RELOAD_RESPONSE_TIMEOUT: Duration = Duration::from_secs(60);

pub(in crate::cli) async fn run_web(paths: &MiyuPaths, args: WebArgs) -> Result<()> {
    if let Some(info) = ipc::daemon_info(paths).await {
        if info.build_id == ipc::BUILD_ID {
            if args.port_explicit || args.bind.is_some() {
                bail!(
                    "{}",
                    t(
                        "the running Miyu daemon already owns Web settings; restart it to change them",
                        "当前 Miyu daemon 已接管 Web 设置；如需修改请先重启 daemon"
                    )
                );
            }
            for url in daemon_web_access_urls(&info) {
                println!("Miyu WebUI: {url}");
            }
            return Ok(());
        }
    }

    let launch = web_launch_config(paths, &args)?;
    let info = ipc::ensure_daemon(paths, launch.as_ref()).await?;
    for url in daemon_web_access_urls(&info) {
        println!("Miyu WebUI: {url}");
    }
    Ok(())
}

pub(in crate::cli) fn web_launch_config(
    paths: &MiyuPaths,
    args: &WebArgs,
) -> Result<Option<ipc::DaemonLaunchConfig>> {
    if !args.port_explicit && args.bind.is_none() {
        return Ok(None);
    }
    // 09-11 起 WebUI 的口令不再从命令行来:首次访问用内置口令建管理员账号,
    // 之后只认账号;launch 状态里的 password_file 是旧字段,留着只为读得懂旧文件。
    let _ = paths;
    Ok(Some(ipc::DaemonLaunchConfig {
        port: args.port,
        password_file: None,
        bind: args.bind,
    }))
}

pub(in crate::cli) fn daemon_web_access_urls(info: &ipc::DaemonInfo) -> Vec<String> {
    let bind = info
        .web_bind
        .unwrap_or(std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED));
    ipc::web_access_urls_for(bind, info.web_port)
}

pub(in crate::cli) async fn run_daemon_command(paths: &MiyuPaths, args: DaemonArgs) -> Result<()> {
    let command = args.command.unwrap_or(DaemonCommand::Start);
    if args.port.is_some() && !matches!(command, DaemonCommand::Start | DaemonCommand::Restart) {
        bail!(
            "{}",
            t(
                "--port only applies to daemon start or restart",
                "--port 仅适用于 daemon start 或 restart"
            )
        );
    }

    match command {
        DaemonCommand::Start => {
            let launch = args
                .port
                .map(|port| ipc::daemon_launch_config_with_port(paths, port))
                .transpose()?;
            if launch.is_some()
                && ipc::daemon_info(paths)
                    .await
                    .is_some_and(|info| info.build_id == ipc::BUILD_ID)
            {
                bail!(
                    "{}",
                    t(
                        "the running Miyu daemon already owns Web settings; use `miyu daemon restart` to change the port",
                        "当前 Miyu daemon 已接管 Web 设置；如需修改端口请使用 `miyu daemon restart`"
                    )
                );
            }
            ipc::ensure_daemon(paths, launch.as_ref()).await?;
            let refreshed = MiyuPaths::new()?;
            print_daemon_status(&refreshed).await
        }
        DaemonCommand::Stop => stop_daemon(paths).await,
        DaemonCommand::Restart => {
            let pending_launch = if let Some(port) = args.port {
                Some(ipc::daemon_launch_config_with_port(paths, port)?)
            } else {
                match ipc::daemon_info(paths).await {
                    Some(info) => ipc::recover_daemon_launch_if_missing(paths, info.pid)?,
                    None => None,
                }
            };
            if let Err(error) = stop_daemon(paths).await {
                if let Some(launch) = &pending_launch {
                    ipc::discard_daemon_launch_candidate(paths, launch);
                }
                return Err(error);
            };
            let refreshed = match MiyuPaths::new() {
                Ok(paths) => paths,
                Err(error) => {
                    if let Some(launch) = &pending_launch {
                        ipc::discard_daemon_launch_candidate(paths, launch);
                    }
                    return Err(error);
                }
            };
            if let Err(error) = ipc::ensure_daemon(&refreshed, pending_launch.as_ref()).await {
                if let Some(launch) = &pending_launch {
                    ipc::discard_daemon_launch_candidate(&refreshed, launch);
                }
                return Err(error);
            }
            print_daemon_status(&refreshed).await
        }
        DaemonCommand::Status => print_daemon_status(paths).await,
        DaemonCommand::Logs(args) => run_daemon_logs(paths, args).await,
    }
}

pub(in crate::cli) async fn stop_daemon(paths: &MiyuPaths) -> Result<()> {
    match ipc::daemon_info(paths).await {
        Some(info) => {
            ipc::shutdown_daemon(paths, &info).await?;
            println!("{}", t("Miyu daemon stopped", "Miyu daemon 已停止"));
        }
        None => println!("{}", t("Miyu daemon is not running", "Miyu daemon 未运行")),
    }
    // info 文件只指向最后一次 start 的进程:历史上多次 start 互相覆盖,会留
    // 下仍占 8300 的孤儿 daemon——stop 谎报成功,后续测试全打在旧代码上,新
    // daemon 被挤去随机端口(08-20 一天咬了两次)。正常关停后按进程表扫一遍
    // 同一 home 的 `__daemon` 残留,全部清掉。
    let swept = sweep_stray_daemons(paths).await;
    if swept > 0 {
        if is_zh() {
            println!("已清理 {swept} 个残留 daemon 进程(历史 start 覆盖登记留下的孤儿)");
        } else {
            println!("cleaned up {swept} stray daemon process(es) left by overwritten launches");
        }
    }
    Ok(())
}

/// 扫描并终止属于本 home 的 `__daemon` 残留进程。归属按 /proc 环境里的
/// MIYU_HOME 判定(未设 = 默认 ~/.miyu),隔离测试环境的 daemon 不受波及。
async fn sweep_stray_daemons(paths: &MiyuPaths) -> usize {
    let my_root = paths.root_dir.clone();
    let default_root =
        std::env::var_os("HOME").map(|home| std::path::PathBuf::from(home).join(".miyu"));
    let mut strays = Vec::new();
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return 0;
    };
    for entry in entries.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<i32>().ok())
        else {
            continue;
        };
        if pid == std::process::id() as i32 {
            continue;
        }
        let Ok(cmdline) = std::fs::read(entry.path().join("cmdline")) else {
            continue;
        };
        let mut parts = cmdline.split(|byte| *byte == 0);
        let is_daemon = parts.next().is_some_and(|argv0| argv0.ends_with(b"miyu"))
            && parts.next() == Some(b"__daemon");
        if !is_daemon {
            continue;
        }
        let Ok(environ) = std::fs::read(entry.path().join("environ")) else {
            continue;
        };
        let home = environ
            .split(|byte| *byte == 0)
            .find_map(|pair| pair.strip_prefix(b"MIYU_HOME="))
            .map(|value| std::path::PathBuf::from(String::from_utf8_lossy(value).to_string()))
            .or_else(|| default_root.clone());
        if home.as_deref() != Some(my_root.as_path()) {
            continue;
        }
        unsafe {
            libc::kill(pid, libc::SIGTERM);
        }
        strays.push(pid);
    }
    if strays.is_empty() {
        return 0;
    }
    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    for pid in &strays {
        if std::fs::metadata(format!("/proc/{pid}")).is_ok() {
            unsafe {
                libc::kill(*pid, libc::SIGKILL);
            }
        }
    }
    strays.len()
}

pub(in crate::cli) async fn print_daemon_status(paths: &MiyuPaths) -> Result<()> {
    let Some(info) = ipc::daemon_info(paths).await else {
        println!("{}", t("Miyu daemon: stopped", "Miyu daemon：已停止"));
        return Ok(());
    };
    let (_, data) = send_ipc_admin(paths, IpcCommand::GetStatus).await?;
    println!(
        "{} {} (PID {})",
        t("Miyu daemon:", "Miyu daemon："),
        t("running", "运行中"),
        info.pid,
    );
    for line in daemon_web_status_lines(t("WebUI:", "WebUI："), &daemon_web_access_urls(&info)) {
        println!("{line}");
    }
    let engine = data
        .pointer("/runtime/turn_engine")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("ready");
    println!("{} {}", t("Turn engine:", "TurnEngine："), engine);

    let qq = data.pointer("/platforms/qq");
    let enabled = qq
        .and_then(|value| value.get("enabled"))
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    if !enabled {
        println!(
            "{} {}",
            t("Tencent QQ:", "腾讯 QQ："),
            t("disabled", "未启用")
        );
        return Ok(());
    }
    let port = qq
        .and_then(|value| value.get("listen_port"))
        .and_then(serde_json::Value::as_u64)
        .unwrap_or_default();
    let accounts = qq
        .and_then(|value| value.get("connected_accounts"))
        .and_then(serde_json::Value::as_array)
        .map(|values| {
            values
                .iter()
                .filter_map(serde_json::Value::as_i64)
                .map(|value| value.to_string())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let connection = if accounts.is_empty() {
        t("not connected", "尚未连接").to_string()
    } else {
        format!("{}: {}", t("connected", "已连接"), accounts.join(", "))
    };
    println!(
        "{} ws://localhost:{port}/ws · {connection}",
        t("Tencent QQ:", "腾讯 QQ：")
    );
    Ok(())
}

pub(in crate::cli) fn daemon_web_status_lines(label: &str, urls: &[String]) -> Vec<String> {
    let Some((first, remaining)) = urls.split_first() else {
        return vec![label.to_string()];
    };
    let indent = " ".repeat(visible_width(label).saturating_add(1));
    std::iter::once(format!("{label} {first}"))
        .chain(remaining.iter().map(|url| format!("{indent}{url}")))
        .collect()
}

/// `miyu daemon logs request`:监控期间开启录制,滚动打印每个出网请求
/// 的摘要行;完整请求体在 JSONL 文件里(整段 prompt 打终端没法看)。
/// Ctrl+C 退出时关闭录制——开关是 daemon 进程级内存位,不落配置。
pub(in crate::cli) async fn run_request_monitor(paths: &MiyuPaths) -> Result<()> {
    if ipc::daemon_info(paths).await.is_none() {
        bail!(
            "{}",
            t(
                "the daemon is not running; start it first (miyu daemon start)",
                "daemon 未运行;先 miyu daemon start"
            )
        );
    }
    let (_, data) = send_ipc_admin(paths, IpcCommand::SetRequestLogging { enabled: true }).await?;
    let file = data
        .get("file")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| {
            paths
                .logs_dir()
                .join("requests-<date>.jsonl")
                .display()
                .to_string()
        });
    println!(
        "{}",
        t(
            "request recording is ON; full bodies append to:",
            "出网请求录制已开启;完整请求体实时追加到:"
        )
    );
    println!("  {file}");
    println!(
        "[2m{}[0m",
        t(
            "monitoring (one summary line per request) · Ctrl+C stops and turns recording off",
            "实时监控中(每请求一行摘要) · Ctrl+C 停止并关闭录制"
        )
    );
    let path = std::path::PathBuf::from(&file);
    let mut offset = std::fs::metadata(&path).map(|meta| meta.len()).unwrap_or(0);
    let mut carry = String::new();
    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => break,
            _ = tokio::time::sleep(Duration::from_millis(300)) => {}
        }
        let Ok(meta) = std::fs::metadata(&path) else {
            continue;
        };
        if meta.len() < offset {
            offset = 0; // 跨日换文件或被清空
        }
        if meta.len() == offset {
            continue;
        }
        use std::io::{Read as _, Seek as _};
        let Ok(mut handle) = std::fs::File::open(&path) else {
            continue;
        };
        if handle.seek(std::io::SeekFrom::Start(offset)).is_err() {
            continue;
        }
        let mut chunk = String::new();
        if handle.read_to_string(&mut chunk).is_err() {
            continue;
        }
        offset = meta.len();
        carry.push_str(&chunk);
        while let Some(newline) = carry.find('\n') {
            let line: String = carry.drain(..=newline).collect();
            let Ok(entry) = serde_json::from_str::<serde_json::Value>(line.trim()) else {
                continue;
            };
            let text = |key: &str| {
                entry
                    .get(key)
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("?")
            };
            let body = entry.get("body");
            let messages = body
                .and_then(|body| body.get("messages").or_else(|| body.get("input")))
                .and_then(serde_json::Value::as_array)
                .map(|items| items.len());
            let size_kb = line.len() as f64 / 1024.0;
            let stamp = text("ts").get(11..19).unwrap_or("--:--:--").to_string();
            println!(
                "{stamp}  {}/{}  {}  scope={}  {:.1}KB{}",
                text("provider"),
                text("model"),
                text("kind"),
                text("scope"),
                size_kb,
                messages
                    .map(|count| format!("  messages={count}"))
                    .unwrap_or_default(),
            );
        }
    }
    let _ = send_ipc_admin(paths, IpcCommand::SetRequestLogging { enabled: false }).await;
    println!(
        "
{}
  {file}",
        t(
            "recording is OFF; inspect full bodies with jq:",
            "录制已关闭;用 jq 查看完整请求体:"
        )
    );
    Ok(())
}

pub(in crate::cli) fn tail_file_lines_with_end(
    path: &Path,
    limit: usize,
) -> Result<(Vec<String>, u64)> {
    const CHUNK: usize = 8192;
    let mut file = std::fs::File::open(path)?;
    let mut position = file.seek(SeekFrom::End(0))?;
    let end_offset = position;
    let mut bytes = Vec::new();
    let mut newline_count = 0usize;
    while position > 0 && newline_count <= limit {
        let read_len = usize::try_from(position.min(CHUNK as u64)).unwrap_or(CHUNK);
        position -= read_len as u64;
        file.seek(SeekFrom::Start(position))?;
        let mut chunk = vec![0_u8; read_len];
        file.read_exact(&mut chunk)?;
        newline_count += chunk.iter().filter(|byte| **byte == b'\n').count();
        chunk.extend(bytes);
        bytes = chunk;
    }
    let text = String::from_utf8_lossy(&bytes);
    let mut lines = text.lines().map(str::to_string).collect::<Vec<_>>();
    if lines.len() > limit {
        lines.drain(..lines.len() - limit);
    }
    Ok((lines, end_offset))
}

#[cfg(unix)]
pub(in crate::cli) fn daemon_process_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    let result = unsafe { libc::kill(pid as libc::pid_t, 0) };
    result == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(not(unix))]
pub(in crate::cli) fn daemon_process_alive(_pid: u32) -> bool {
    // The IPC probe remains the authoritative check on platforms without a
    // portable process-existence primitive.
    true
}

pub(in crate::cli) async fn reload_daemon_if_running(paths: &MiyuPaths) -> Result<()> {
    if ipc::daemon_info(paths).await.is_some() {
        retry_config_reload(RELOAD_MAX_ATTEMPTS, RELOAD_RETRY_INTERVAL, || {
            request_config_reload(paths)
        })
        .await
        .with_context(|| {
            t(
                "configuration was saved, but the running daemon did not reload it",
                "配置已保存，但正在运行的 daemon 未能重新加载配置",
            )
        })?;
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::cli) enum ConfigReloadResponse {
    Reloaded,
    Busy,
}

pub(in crate::cli) fn validate_config_reload_response(
    frame: Option<IpcFrame>,
) -> Result<ConfigReloadResponse> {
    if let Some(IpcFrame::Error { code, message }) = &frame {
        if *code == Some(ipc::ErrorCode::Busy)
            || (code.is_none() && message == ipc::ADMIN_BUSY_MESSAGE)
        {
            return Ok(ConfigReloadResponse::Busy);
        }
    }
    validate_ipc_command_response(frame)?;
    Ok(ConfigReloadResponse::Reloaded)
}

pub(in crate::cli) async fn request_config_reload(
    paths: &MiyuPaths,
) -> Result<ConfigReloadResponse> {
    request_config_reload_at(&paths.ipc_socket(), RELOAD_RESPONSE_TIMEOUT).await
}

pub(in crate::cli) async fn request_config_reload_at(
    socket: &Path,
    response_timeout: Duration,
) -> Result<ConfigReloadResponse> {
    tokio::time::timeout(response_timeout, async {
        let mut stream = ipc::connect(socket).await?;
        ipc::send(&mut stream, &IpcRequest::new(IpcCommand::ReloadConfig)).await?;
        validate_config_reload_response(ipc::receive::<IpcFrame>(&mut stream).await?)
    })
    .await
    .with_context(|| {
        t(
            "timed out waiting for Miyu daemon to reload configuration",
            "等待 Miyu daemon 重新加载配置超时",
        )
    })?
}

pub(in crate::cli) async fn run_reload(paths: &MiyuPaths) -> Result<()> {
    if ipc::daemon_info(paths).await.is_none() {
        bail!("{}", t("Miyu daemon is not running", "Miyu daemon 未运行"));
    }
    retry_config_reload(RELOAD_MAX_ATTEMPTS, RELOAD_RETRY_INTERVAL, || {
        request_config_reload(paths)
    })
    .await?;
    println!("{}", t("configuration reloaded", "配置已重新加载"));
    Ok(())
}

pub(in crate::cli) async fn retry_config_reload<F, Fut>(
    max_attempts: usize,
    retry_interval: Duration,
    mut request_reload: F,
) -> Result<()>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<ConfigReloadResponse>>,
{
    if max_attempts == 0 {
        bail!("reload must allow at least one attempt");
    }

    for attempt in 1..=max_attempts {
        match request_reload().await? {
            ConfigReloadResponse::Reloaded => return Ok(()),
            ConfigReloadResponse::Busy if attempt < max_attempts => {
                let seconds = retry_interval.as_secs();
                let message = if is_zh() {
                    format!(
                        "Miyu daemon 正忙；将在 {seconds} 秒后重试配置重载（{attempt}/{max_attempts}）"
                    )
                } else {
                    format!(
                        "Miyu daemon is busy; retrying configuration reload in {seconds} seconds ({attempt}/{max_attempts})"
                    )
                };
                eprintln!("{message}");
                tokio::time::sleep(retry_interval).await;
            }
            ConfigReloadResponse::Busy => {
                let message = if is_zh() {
                    format!("Miyu daemon 在 {max_attempts} 次配置重载尝试后仍然忙碌")
                } else {
                    format!(
                        "Miyu daemon remained busy after {max_attempts} configuration reload attempts"
                    )
                };
                bail!("{message}");
            }
        }
    }
    unreachable!("reload loop always returns on its final attempt")
}
