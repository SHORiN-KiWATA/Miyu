use super::{ToolRegistry, ToolSpec};
use crate::config::{AppConfig, DiagnosticsPluginConfig};
use anyhow::{bail, Result};
use serde::Serialize;
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use tokio::process::Command;
use tokio::time::{timeout, Duration};

pub fn register(registry: &mut ToolRegistry, config: AppConfig) {
    registry.register(ToolSpec::new(
        "check_issue",
        "Collect read-only diagnostic evidence for a concrete local issue. This tool gathers facts only; it does not diagnose, rank root causes, or recommend fixes.",
        json!({
            "type": "object",
            "properties": {
                "query": { "type": "string", "description": "Original user issue. Used for auto area/target inference." },
                "area": { "type": "string", "enum": ["auto", "system", "app", "input_method", "display", "audio", "package", "gpu", "network", "storage"], "description": "Evidence collection area." },
                "target": { "type": "string", "description": "Optional app, process, command, package, or subsystem target." },
                "symptom": { "type": "string", "description": "Optional symptom label." },
                "depth": { "type": "string", "enum": ["quick", "normal", "full"], "description": "Probe depth." },
                "recent_minutes": { "type": "integer", "description": "Recent log window in minutes, clamped to 1..1440." },
                "platform": { "type": "string", "enum": ["auto", "linux", "macos"], "description": "Platform override. Prefer auto." },
                "allow_launch_probe": { "type": "boolean", "description": "For app/input_method evidence only: explicitly allow launching target to sample runtime facts. Defaults to false." },
                "launch_timeout_seconds": { "type": "integer", "description": "Seconds to wait after launch probe before sampling pids. Defaults to 3, max 15." }
            },
            "required": [],
            "additionalProperties": false
        }),
        move |args| {
            let config = config.clone();
            async move { check_issue(args, config.plugins.diagnostics.clone()).await }
        },
    ));
}

#[derive(Debug, Clone)]
struct CheckIssueArgs {
    query: Option<String>,
    area: Area,
    target: Option<String>,
    symptom: Option<String>,
    depth: Depth,
    recent_minutes: u64,
    platform: PlatformArg,
    allow_launch_probe: bool,
    launch_timeout_seconds: u64,
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
enum Area {
    System,
    App,
    InputMethod,
    Display,
    Audio,
    Package,
    Gpu,
    Network,
    Storage,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum Depth {
    Quick,
    Normal,
    Full,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PlatformArg {
    Auto,
    Linux,
    Macos,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum Platform {
    Linux,
    Macos,
    Unsupported,
}

#[derive(Debug, Serialize)]
struct EvidenceReport {
    ok: bool,
    kind: &'static str,
    platform: Platform,
    query: Option<String>,
    area: Area,
    target: Option<String>,
    symptom: Option<String>,
    depth: Depth,
    facts: BTreeMap<String, Value>,
    checks: Vec<Check>,
    logs: Vec<LogExcerpt>,
    missing_evidence: Vec<String>,
    safety_notes: Vec<String>,
    recommended_next_probes: Vec<String>,
}

#[derive(Debug, Serialize)]
struct Check {
    id: String,
    status: CheckStatus,
    detail: String,
    evidence: Vec<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "snake_case")]
enum CheckStatus {
    Ok,
    Warn,
    Error,
    Unknown,
}

#[derive(Debug, Serialize)]
struct LogExcerpt {
    source: String,
    message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum InputToolkit {
    ElectronChromium,
    ElectronX11,
    ElectronWayland,
    Gtk,
    Qt,
    Sdl,
    Java,
    X11Legacy,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum DisplayMode {
    X11,
    XWayland,
    WaylandNative,
    Unknown,
}

#[derive(Debug, Serialize)]
struct InputMethodProfile {
    toolkit: InputToolkit,
    display_mode: DisplayMode,
    runtime_observed: bool,
    command_line: Option<String>,
    desktop_exec: Option<String>,
    target_env: Option<BTreeMap<String, String>>,
    loaded_input_modules: Vec<String>,
    available_input_modules: Vec<String>,
    immodule_cache: Vec<ImmoduleCacheEntry>,
    wayland_protocol: WaylandProtocolInfo,
    locale_info: LocaleInfo,
    path_status: InputMethodPathStatus,
}

#[derive(Debug, Clone, Serialize)]
struct ImmoduleCacheEntry {
    so_path: String,
    module_name: String,
    locales: String,
}

#[derive(Debug, Serialize)]
struct WaylandProtocolInfo {
    compositor_supports_text_input_v3: bool,
    fcitx5_wayland_frontend_loaded: bool,
    wayland_info_available: bool,
}

#[derive(Debug, Serialize)]
struct LocaleInfo {
    target_lang: Option<String>,
    target_lc_ctype: Option<String>,
    available_locales: Vec<String>,
    locale_valid: bool,
}

#[derive(Debug, Serialize)]
struct InputMethodPathStatus {
    paths: Vec<NamedPathCheck>,
    overall: String,
}

#[derive(Debug, Serialize)]
struct NamedPathCheck {
    name: String,
    status: String,
    evidence: Vec<String>,
    missing: Vec<String>,
}

#[derive(Debug)]
struct ProbeOutput {
    status: Option<i32>,
    stdout: String,
    stderr: String,
    timed_out: bool,
}

async fn check_issue(args: Value, config: DiagnosticsPluginConfig) -> Result<String> {
    if !config.enabled {
        bail!("diagnostics plugin is disabled");
    }
    let args = parse_args(args)?;
    let platform = detect_platform(args.platform);
    let mut report = EvidenceReport {
        ok: true,
        kind: "diagnostic_evidence",
        platform,
        query: args.query.clone(),
        area: args.area,
        target: args.target.clone(),
        symptom: args.symptom.clone(),
        depth: args.depth,
        facts: BTreeMap::new(),
        checks: Vec::new(),
        logs: Vec::new(),
        missing_evidence: Vec::new(),
        safety_notes: vec![
            "check_issue uses fixed read-only probes and does not diagnose or apply fixes"
                .to_string(),
        ],
        recommended_next_probes: Vec::new(),
    };

    match platform {
        Platform::Linux => collect_linux_evidence(&args, &config, &mut report).await,
        Platform::Macos => collect_macos_evidence(&args, &config, &mut report).await,
        Platform::Unsupported => {
            report.ok = false;
            report.checks.push(Check {
                id: "platform.supported".to_string(),
                status: CheckStatus::Error,
                detail: "only linux and macos are supported by check_issue".to_string(),
                evidence: vec![std::env::consts::OS.to_string()],
            });
        }
    }
    Ok(serde_json::to_string_pretty(&report)?)
}

fn parse_args(args: Value) -> Result<CheckIssueArgs> {
    let query = optional_string(&args, "query", 500);
    let mut target = optional_string(&args, "target", 160);
    let symptom = optional_string(&args, "symptom", 200);
    let area_raw = args
        .get("area")
        .or_else(|| args.get("mode"))
        .and_then(Value::as_str)
        .unwrap_or("auto")
        .trim();
    let area = if area_raw == "auto" {
        let inferred = infer_area(query.as_deref(), target.as_deref())?;
        if target.is_none() {
            target = infer_target(query.as_deref().unwrap_or_default());
        }
        inferred
    } else {
        parse_area(area_raw)?
    };
    Ok(CheckIssueArgs {
        query,
        area,
        target,
        symptom,
        depth: parse_depth(
            args.get("depth")
                .and_then(Value::as_str)
                .unwrap_or("normal"),
        )?,
        recent_minutes: args
            .get("recent_minutes")
            .and_then(Value::as_u64)
            .unwrap_or(30)
            .clamp(1, 1440),
        platform: parse_platform_arg(
            args.get("platform")
                .and_then(Value::as_str)
                .unwrap_or("auto"),
        )?,
        allow_launch_probe: args
            .get("allow_launch_probe")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        launch_timeout_seconds: args
            .get("launch_timeout_seconds")
            .and_then(Value::as_u64)
            .unwrap_or(3)
            .clamp(1, 15),
    })
}

fn optional_string(args: &Value, name: &str, max_chars: usize) -> Option<String> {
    args.get(name)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| value.chars().take(max_chars).collect())
}

fn infer_area(query: Option<&str>, target: Option<&str>) -> Result<Area> {
    let text = query.unwrap_or_default();
    let lower = text.to_ascii_lowercase();
    if contains_any(
        text,
        &["输入法", "打不了中文", "候选框", "拼音", "fcitx", "ibus"],
    ) || contains_any(&lower, &["ime", "input method", "fcitx", "ibus"])
    {
        Ok(Area::InputMethod)
    } else if contains_any(text, &["没声音", "声音", "麦克风", "耳机"])
        || contains_any(&lower, &["audio", "sound", "pipewire", "wireplumber"])
    {
        Ok(Area::Audio)
    } else if contains_any(text, &["屏幕分享", "黑屏", "截图", "录屏", "显示器"])
        || contains_any(
            &lower,
            &["display", "screen", "wayland", "xwayland", "portal"],
        )
    {
        Ok(Area::Display)
    } else if contains_any(text, &["更新", "安装包", "依赖", "包管理"])
        || contains_any(
            &lower,
            &["pacman", "yay", "paru", "aur", "apt", "dnf", "brew"],
        )
    {
        Ok(Area::Package)
    } else if contains_any(text, &["显卡", "驱动", "独显", "核显"])
        || contains_any(&lower, &["gpu", "nvidia", "amd", "mesa", "vulkan"])
    {
        Ok(Area::Gpu)
    } else if contains_any(text, &["网络", "联网", "断网", "网卡", "wifi"])
        || contains_any(&lower, &["network", "internet", "wifi", "dns"])
    {
        Ok(Area::Network)
    } else if contains_any(text, &["磁盘", "硬盘", "空间", "挂载", "btrfs"])
        || contains_any(&lower, &["disk", "storage", "mount", "filesystem"])
    {
        Ok(Area::Storage)
    } else if target.is_some()
        || contains_any(text, &["打不开", "启动不了", "闪退", "崩溃", "报错"])
        || contains_any(&lower, &["crash", "cannot start", "won't open", "not open"])
    {
        Ok(Area::App)
    } else if text.trim().is_empty() {
        bail!("area is auto but query is empty; provide query or structured area")
    } else {
        Ok(Area::System)
    }
}

fn infer_target(text: &str) -> Option<String> {
    let lower = text.to_ascii_lowercase();
    for (needle, target) in [
        ("opencode", "opencode"),
        ("linuxqq", "qq"),
        ("qq", "qq"),
        ("微信", "wechat"),
        ("wechat", "wechat"),
        ("steam", "steam"),
        ("firefox", "firefox"),
        ("chrome", "chrome"),
        ("chromium", "chromium"),
        ("vscode", "code"),
        ("code", "code"),
    ] {
        if lower.contains(needle) || text.contains(needle) {
            return Some(target.to_string());
        }
    }
    None
}

fn parse_area(value: &str) -> Result<Area> {
    match value.trim() {
        "system" => Ok(Area::System),
        "app" => Ok(Area::App),
        "input_method" => Ok(Area::InputMethod),
        "display" => Ok(Area::Display),
        "audio" => Ok(Area::Audio),
        "package" | "package_update" => Ok(Area::Package),
        "gpu" => Ok(Area::Gpu),
        "network" => Ok(Area::Network),
        "storage" => Ok(Area::Storage),
        _ => bail!("unsupported diagnostic area: {value}"),
    }
}

fn parse_depth(value: &str) -> Result<Depth> {
    match value.trim() {
        "quick" => Ok(Depth::Quick),
        "normal" => Ok(Depth::Normal),
        "full" => Ok(Depth::Full),
        _ => bail!("unsupported diagnostic depth: {value}"),
    }
}

fn parse_platform_arg(value: &str) -> Result<PlatformArg> {
    match value.trim() {
        "auto" => Ok(PlatformArg::Auto),
        "linux" => Ok(PlatformArg::Linux),
        "macos" => Ok(PlatformArg::Macos),
        _ => bail!("unsupported diagnostic platform: {value}"),
    }
}

fn detect_platform(arg: PlatformArg) -> Platform {
    match arg {
        PlatformArg::Linux => Platform::Linux,
        PlatformArg::Macos => Platform::Macos,
        PlatformArg::Auto => match std::env::consts::OS {
            "linux" => Platform::Linux,
            "macos" => Platform::Macos,
            _ => Platform::Unsupported,
        },
    }
}

async fn collect_linux_evidence(
    args: &CheckIssueArgs,
    config: &DiagnosticsPluginConfig,
    report: &mut EvidenceReport,
) {
    linux_system_facts(config, report).await;
    match args.area {
        Area::System => linux_basic_checks(config, report).await,
        Area::App => linux_app_evidence(args, config, report).await,
        Area::InputMethod => linux_input_method_evidence(args, config, report).await,
        Area::Display => linux_display_evidence(args, config, report).await,
        Area::Audio => linux_audio_evidence(args, config, report).await,
        Area::Package => linux_package_evidence(args, config, report).await,
        Area::Gpu => linux_gpu_evidence(config, report).await,
        Area::Network => linux_network_evidence(config, report).await,
        Area::Storage => linux_storage_evidence(config, report).await,
    }
}

async fn collect_macos_evidence(
    args: &CheckIssueArgs,
    config: &DiagnosticsPluginConfig,
    report: &mut EvidenceReport,
) {
    fact_env(report, "env.shell", "SHELL");
    fact_env(report, "env.term", "TERM");
    fact_env(report, "env.lang", "LANG");
    let sw_vers = run_command(config, "sw_vers", &[], 2).await;
    push_log_if_stdout(report, "sw_vers", &sw_vers);
    if matches!(args.area, Area::App | Area::InputMethod) {
        if let Some(target) = args.target.as_deref() {
            command_exists_check(config, report, target).await;
            process_check(config, report, target).await;
        } else {
            report
                .missing_evidence
                .push("target app was not provided".to_string());
        }
    }
}

async fn linux_system_facts(config: &DiagnosticsPluginConfig, report: &mut EvidenceReport) {
    for key in [
        "SHELL",
        "TERM",
        "LANG",
        "XDG_SESSION_TYPE",
        "XDG_CURRENT_DESKTOP",
        "DESKTOP_SESSION",
        "WAYLAND_DISPLAY",
        "DISPLAY",
        "GTK_IM_MODULE",
        "QT_IM_MODULE",
        "QT_IM_MODULES",
        "XMODIFIERS",
        "SDL_IM_MODULE",
    ] {
        fact_env(report, &format!("env.{key}"), key);
    }
    if let Ok(text) = std::fs::read_to_string("/etc/os-release") {
        if let Some(name) = os_release_value(&text, "PRETTY_NAME") {
            report
                .facts
                .insert("os.pretty_name".to_string(), json!(name));
        }
    }
    let uname = run_command(config, "uname", &["-a"], 2).await;
    if !uname.stdout.trim().is_empty() {
        report
            .facts
            .insert("kernel.uname".to_string(), json!(uname.stdout.trim()));
    }
}

async fn linux_basic_checks(config: &DiagnosticsPluginConfig, report: &mut EvidenceReport) {
    for command in ["systemctl", "journalctl", "loginctl", "ip", "df"] {
        command_exists_check(config, report, command).await;
    }
}

async fn linux_app_evidence(
    args: &CheckIssueArgs,
    config: &DiagnosticsPluginConfig,
    report: &mut EvidenceReport,
) {
    let Some(target) = args.target.as_deref() else {
        report
            .missing_evidence
            .push("target app was not provided".to_string());
        return;
    };
    command_exists_check(config, report, target).await;
    process_check(config, report, target).await;
    if let Some(path) = command_path(config, target).await {
        report
            .facts
            .insert("app.command_path".to_string(), json!(path.clone()));
        package_owner(config, report, &path).await;
        app_probe_version(config, report, target).await;
    }
    recent_logs(args, config, report, &[target, "error", "failed"]).await;
}

async fn linux_input_method_evidence(
    args: &CheckIssueArgs,
    config: &DiagnosticsPluginConfig,
    report: &mut EvidenceReport,
) {
    for name in ["fcitx5", "ibus-daemon"] {
        process_check(config, report, name).await;
    }
    command_exists_check(config, report, "fcitx5-remote").await;
    if command_path(config, "fcitx5-remote").await.is_some() {
        let output = run_command(config, "fcitx5-remote", &[], 2).await;
        report.checks.push(Check {
            id: "input_method.fcitx5_remote".to_string(),
            status: if output.status == Some(0) {
                CheckStatus::Ok
            } else {
                CheckStatus::Warn
            },
            detail: "fcitx5-remote status probe".to_string(),
            evidence: compact_evidence(&output),
        });
    }

    let wayland_protocol = probe_wayland_protocol(config, report).await;

    let available_modules = scan_available_input_modules();
    report.facts.insert(
        "input_method.available_modules".to_string(),
        json!(available_modules.clone()),
    );

    let immodule_cache = read_gtk_immodule_cache();
    report.facts.insert(
        "input_method.immodule_cache".to_string(),
        json!(immodule_cache.clone()),
    );

    let Some(target) = args.target.as_deref() else {
        report.missing_evidence.push("target app was not provided; cannot check app toolkit, target environment, loaded .so modules, or path status".to_string());
        return;
    };
    let mut pids = process_check(config, report, target).await;
    if pids.is_empty() && args.allow_launch_probe {
        pids = launch_probe_target(args, config, report, target).await;
    }
    if pids.is_empty() {
        report.missing_evidence.push(format!(
            "target app {target} is not running; runtime environment and loaded .so modules are unavailable"
        ));
        report.recommended_next_probes.push(format!(
            "start {target}, then rerun check_issue with area=input_method and target={target}"
        ));
    }
    let target_env = pids.first().and_then(|pid| read_process_input_env(*pid));
    let loaded_modules = read_loaded_input_modules(&pids);

    let locale_info = probe_locale_info(config, report, &target_env).await;

    let socket_display_mode = probe_display_mode_via_sockets(config, report, &pids).await;

    let profile = build_input_method_profile(
        config,
        report,
        target,
        &pids,
        target_env,
        loaded_modules,
        available_modules,
        immodule_cache,
        wayland_protocol,
        locale_info,
        socket_display_mode,
    )
    .await;
    report
        .facts
        .insert("input_method.profile".to_string(), json!(profile));
    recent_logs(
        args,
        config,
        report,
        &[target, "fcitx", "ibus", "qt", "gtk", "xwayland"],
    )
    .await;
}

async fn probe_wayland_protocol(
    config: &DiagnosticsPluginConfig,
    report: &mut EvidenceReport,
) -> WaylandProtocolInfo {
    let wayland_info_output = run_command(config, "wayland-info", &[], 3).await;
    let wayland_info_available = wayland_info_output.status.is_some();
    let compositor_supports_text_input_v3 = wayland_info_output
        .stdout
        .contains("zwp_text_input_manager_v3");
    report.checks.push(Check {
        id: "input_method.wayland_text_input_v3".to_string(),
        status: if compositor_supports_text_input_v3 {
            CheckStatus::Ok
        } else {
            CheckStatus::Unknown
        },
        detail: "wayland-info: compositor text-input-v3 protocol support".to_string(),
        evidence: compact_evidence(&wayland_info_output),
    });

    let fcitx5_pids = process_ids(config, "fcitx5").await;
    let fcitx5_maps = fcitx5_pids
        .first()
        .and_then(|pid| std::fs::read_to_string(format!("/proc/{pid}/maps")).ok())
        .unwrap_or_default();
    let fcitx5_wayland_frontend_loaded = fcitx5_maps
        .lines()
        .any(|line| line.contains("libwaylandim.so") || line.contains("libwayland.so"));

    WaylandProtocolInfo {
        compositor_supports_text_input_v3,
        fcitx5_wayland_frontend_loaded,
        wayland_info_available,
    }
}

async fn probe_locale_info(
    config: &DiagnosticsPluginConfig,
    report: &mut EvidenceReport,
    target_env: &Option<BTreeMap<String, String>>,
) -> LocaleInfo {
    let target_lang = target_env.as_ref().and_then(|env| env.get("LANG").cloned());
    let target_lc_ctype = target_env
        .as_ref()
        .and_then(|env| env.get("LC_CTYPE").or_else(|| env.get("LC_ALL")).cloned());

    let locale_a_output = run_command(config, "locale", &["-a"], 2).await;
    let available_locales: Vec<String> = locale_a_output
        .stdout
        .lines()
        .map(|line| line.trim().to_string())
        .filter(|line| !line.is_empty())
        .collect();
    report.facts.insert(
        "input_method.available_locales".to_string(),
        json!(available_locales.clone()),
    );

    let check_locale = target_lc_ctype
        .as_deref()
        .or(target_lang.as_deref())
        .unwrap_or("C");
    let locale_valid = check_locale != "C"
        && check_locale != "POSIX"
        && available_locales.iter().any(|loc| {
            loc == check_locale
                || loc.eq_ignore_ascii_case(check_locale)
                || check_locale
                    .split('.')
                    .next()
                    .is_some_and(|prefix| loc.split('.').next() == Some(prefix))
        });

    LocaleInfo {
        target_lang,
        target_lc_ctype,
        available_locales,
        locale_valid,
    }
}

async fn probe_display_mode_via_sockets(
    config: &DiagnosticsPluginConfig,
    report: &mut EvidenceReport,
    pids: &[u32],
) -> DisplayMode {
    if pids.is_empty() {
        return DisplayMode::Unknown;
    }
    let ss_output = run_command(config, "ss", &["-xp"], 3).await;
    let unix_text = std::fs::read_to_string("/proc/net/unix").unwrap_or_default();

    let x11_inodes: BTreeSet<String> = unix_text
        .lines()
        .filter(|line| line.contains("X11-unix"))
        .filter_map(|line| line.split_whitespace().nth(7).map(|s| s.to_string()))
        .collect();
    let wayland_inodes: BTreeSet<String> = unix_text
        .lines()
        .filter(|line| line.contains("wayland"))
        .filter_map(|line| line.split_whitespace().nth(7).map(|s| s.to_string()))
        .collect();

    let mut has_x11 = false;
    let mut has_wayland = false;
    for pid in pids.iter().take(8) {
        let pid_str = format!("pid={pid}");
        for line in ss_output.stdout.lines() {
            if !line.contains(&pid_str) {
                continue;
            }
            let parts: Vec<&str> = line.split_whitespace().collect();
            for part in &parts {
                if part.chars().all(|c| c.is_ascii_digit()) && !part.is_empty() {
                    if x11_inodes.contains(*part) {
                        has_x11 = true;
                    }
                    if wayland_inodes.contains(*part) {
                        has_wayland = true;
                    }
                }
            }
        }
    }

    report.facts.insert(
        "input_method.socket_display_mode".to_string(),
        json!({
            "has_x11_socket": has_x11,
            "has_wayland_socket": has_wayland,
        }),
    );

    match (has_x11, has_wayland) {
        (true, _) => DisplayMode::XWayland,
        (false, true) => DisplayMode::WaylandNative,
        (false, false) => DisplayMode::Unknown,
    }
}

fn read_gtk_immodule_cache() -> Vec<ImmoduleCacheEntry> {
    let mut entries = Vec::new();
    for cache_path in [
        "/usr/lib/gtk-3.0/3.0.0/immodules.cache",
        "/usr/lib/gtk-4.0/4.0.0/immodules.cache",
    ] {
        let Ok(text) = std::fs::read_to_string(cache_path) else {
            continue;
        };
        for line in text.lines() {
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() < 5 {
                continue;
            }
            let so_path = parts[0].trim_matches('"').to_string();
            let module_name = parts[1].trim_matches('"').to_string();
            let locales = parts
                .get(4)
                .map(|s| s.trim_matches('"').to_string())
                .unwrap_or_default();
            entries.push(ImmoduleCacheEntry {
                so_path,
                module_name,
                locales,
            });
        }
    }
    entries
}

async fn build_input_method_profile(
    config: &DiagnosticsPluginConfig,
    report: &mut EvidenceReport,
    target: &str,
    pids: &[u32],
    target_env: Option<BTreeMap<String, String>>,
    loaded_modules: Vec<String>,
    available_modules: Vec<String>,
    immodule_cache: Vec<ImmoduleCacheEntry>,
    wayland_protocol: WaylandProtocolInfo,
    locale_info: LocaleInfo,
    socket_display_mode: DisplayMode,
) -> InputMethodProfile {
    let command_line = pids.first().and_then(|pid| read_proc_cmdline(*pid));
    let desktop_exec = linux_desktop_exec_for_target(target);
    let command_path = command_path(config, target).await;
    let package_probe = command_path
        .as_deref()
        .and_then(|path| package_probe_for_command(config, path, target));
    if let Some(probe) = &package_probe {
        report
            .facts
            .insert("input_method.package_probe".to_string(), json!(probe));
    }
    let evidence_text = [
        target.to_string(),
        command_line.clone().unwrap_or_default(),
        desktop_exec.clone().unwrap_or_default(),
        command_path.unwrap_or_default(),
        package_probe.unwrap_or_default(),
    ]
    .join(" ");
    let raw_toolkit = infer_input_toolkit(&evidence_text);
    let display_mode = infer_display_mode(
        &evidence_text,
        target_env.as_ref(),
        socket_display_mode,
        &loaded_modules,
    );
    let toolkit = refine_electron_toolkit(raw_toolkit, display_mode);
    let path_status = input_method_path_status(
        toolkit,
        display_mode,
        !pids.is_empty() && command_line.is_some(),
        target_env.as_ref(),
        &loaded_modules,
        &available_modules,
        &immodule_cache,
        &wayland_protocol,
        &locale_info,
    );
    InputMethodProfile {
        toolkit,
        display_mode,
        runtime_observed: !pids.is_empty() && command_line.is_some(),
        command_line,
        desktop_exec,
        target_env,
        loaded_input_modules: loaded_modules,
        available_input_modules: available_modules,
        immodule_cache,
        wayland_protocol,
        locale_info,
        path_status,
    }
}

fn refine_electron_toolkit(toolkit: InputToolkit, display_mode: DisplayMode) -> InputToolkit {
    match toolkit {
        InputToolkit::ElectronChromium => match display_mode {
            DisplayMode::WaylandNative => InputToolkit::ElectronWayland,
            DisplayMode::X11 | DisplayMode::XWayland => InputToolkit::ElectronX11,
            DisplayMode::Unknown => InputToolkit::ElectronX11,
        },
        other => other,
    }
}

fn input_method_path_status(
    toolkit: InputToolkit,
    display_mode: DisplayMode,
    runtime_observed: bool,
    env: Option<&BTreeMap<String, String>>,
    loaded_modules: &[String],
    available_modules: &[String],
    immodule_cache: &[ImmoduleCacheEntry],
    wayland_protocol: &WaylandProtocolInfo,
    locale_info: &LocaleInfo,
) -> InputMethodPathStatus {
    let mut paths = Vec::new();
    let mut evidence = Vec::new();
    let mut missing = Vec::new();

    evidence.push(format!("toolkit={toolkit:?}"));
    evidence.push(format!("display_mode={display_mode:?}"));
    if !runtime_observed {
        missing.push("runtime process evidence".to_string());
    }
    if toolkit == InputToolkit::Unknown {
        missing.push("app toolkit/framework evidence".to_string());
    }
    paths.push(NamedPathCheck {
        name: "app_adapter".to_string(),
        status: if missing.is_empty() {
            "confirmed"
        } else {
            "unknown"
        }
        .to_string(),
        evidence,
        missing,
    });

    let relevant_paths = relevant_input_paths(toolkit);
    for path_name in &relevant_paths {
        let check = check_single_path(
            path_name,
            env,
            loaded_modules,
            available_modules,
            immodule_cache,
            wayland_protocol,
            locale_info,
        );
        paths.push(check);
    }

    let any_confirmed = paths.iter().skip(1).any(|p| p.status == "confirmed");
    let all_incomplete = paths
        .iter()
        .skip(1)
        .all(|p| p.status == "missing" || p.status == "unknown");
    let overall = if any_confirmed {
        "path_evidence_complete".to_string()
    } else if all_incomplete {
        "path_evidence_incomplete".to_string()
    } else {
        "path_evidence_partial".to_string()
    };

    InputMethodPathStatus { paths, overall }
}

fn relevant_input_paths(toolkit: InputToolkit) -> Vec<&'static str> {
    match toolkit {
        InputToolkit::Gtk | InputToolkit::Qt | InputToolkit::Sdl | InputToolkit::X11Legacy => {
            vec!["wayland_protocol", "toolkit_module", "xim"]
        }
        InputToolkit::ElectronX11 => vec!["gtk_module", "xim"],
        InputToolkit::ElectronWayland => vec!["wayland_protocol", "gtk_module"],
        InputToolkit::ElectronChromium => vec!["wayland_protocol", "gtk_module", "xim"],
        InputToolkit::Java => vec!["xim"],
        InputToolkit::Unknown => vec![
            "wayland_protocol",
            "gtk_module",
            "qt_module",
            "sdl_module",
            "xim",
        ],
    }
}

#[allow(clippy::too_many_arguments)]
fn check_single_path(
    path_name: &str,
    env: Option<&BTreeMap<String, String>>,
    loaded_modules: &[String],
    available_modules: &[String],
    immodule_cache: &[ImmoduleCacheEntry],
    wayland_protocol: &WaylandProtocolInfo,
    locale_info: &LocaleInfo,
) -> NamedPathCheck {
    let loaded = |needles: &[&str]| loaded_module_evidence(loaded_modules, needles);
    let available = |needles: &[&str]| available_module_evidence(available_modules, needles);

    match path_name {
        "wayland_protocol" => {
            let mut ev = Vec::new();
            let mut miss = Vec::new();
            if wayland_protocol.compositor_supports_text_input_v3 {
                ev.push("compositor supports zwp_text_input_manager_v3".to_string());
            } else {
                miss.push("compositor text-input-v3 protocol support".to_string());
            }
            if wayland_protocol.fcitx5_wayland_frontend_loaded {
                ev.push("fcitx5 loaded libwaylandim.so (wayland frontend)".to_string());
            } else {
                miss.push("fcitx5 wayland frontend (libwaylandim.so)".to_string());
            }
            let status = if wayland_protocol.compositor_supports_text_input_v3
                && wayland_protocol.fcitx5_wayland_frontend_loaded
            {
                "confirmed"
            } else if wayland_protocol.compositor_supports_text_input_v3
                || wayland_protocol.fcitx5_wayland_frontend_loaded
            {
                "configured"
            } else {
                "missing"
            };
            NamedPathCheck {
                name: "wayland_protocol".to_string(),
                status: status.to_string(),
                evidence: ev,
                missing: miss,
            }
        }
        "gtk_module" | "toolkit_module" => {
            let mut ev = Vec::new();
            let mut miss = Vec::new();

            if let Some(env) = env {
                if path_name == "gtk_module" {
                    if let Some(value) = env.get("GTK_IM_MODULE").filter(|v| !v.trim().is_empty()) {
                        ev.push(format!("GTK_IM_MODULE={value}"));
                    }
                }
            }

            if let Some(item) = loaded(&["im-fcitx", "im-wayland", "im-xim", "im-ibus"]) {
                ev.push(item);
                NamedPathCheck {
                    name: path_name.to_string(),
                    status: "confirmed".to_string(),
                    evidence: ev,
                    missing: miss,
                }
            } else if let Some(item) = available(&["im-fcitx", "im-wayland", "im-xim", "im-ibus"]) {
                ev.push(format!("available_on_disk={item}"));
                let locale_match =
                    check_immodule_locale(path_name, "fcitx", immodule_cache, locale_info);
                if !locale_match.is_empty() {
                    ev.push(locale_match);
                }
                NamedPathCheck {
                    name: path_name.to_string(),
                    status: "configured".to_string(),
                    evidence: ev,
                    missing: miss,
                }
            } else {
                miss.push("GTK input module .so (neither loaded nor on disk)".to_string());
                NamedPathCheck {
                    name: path_name.to_string(),
                    status: "missing".to_string(),
                    evidence: ev,
                    missing: miss,
                }
            }
        }
        "qt_module" => {
            let mut ev = Vec::new();
            let mut miss = Vec::new();
            if let Some(env) = env {
                if let Some(value) = env.get("QT_IM_MODULE").filter(|v| !v.trim().is_empty()) {
                    ev.push(format!("QT_IM_MODULE={value}"));
                }
                if let Some(value) = env.get("QT_IM_MODULES").filter(|v| !v.trim().is_empty()) {
                    ev.push(format!("QT_IM_MODULES={value}"));
                }
            }
            if let Some(item) = loaded(&["platforminputcontext", "libfcitx", "libibus"]) {
                ev.push(item);
                NamedPathCheck {
                    name: "qt_module".to_string(),
                    status: "confirmed".to_string(),
                    evidence: ev,
                    missing: miss,
                }
            } else if let Some(item) = available(&["platforminputcontext", "fcitx"]) {
                ev.push(format!("available_on_disk={item}"));
                NamedPathCheck {
                    name: "qt_module".to_string(),
                    status: "configured".to_string(),
                    evidence: ev,
                    missing: miss,
                }
            } else {
                miss.push("Qt platforminputcontext .so evidence".to_string());
                NamedPathCheck {
                    name: "qt_module".to_string(),
                    status: "missing".to_string(),
                    evidence: ev,
                    missing: miss,
                }
            }
        }
        "sdl_module" => {
            let mut ev = Vec::new();
            let mut miss = Vec::new();
            if let Some(env) = env {
                if let Some(value) = env.get("SDL_IM_MODULE").filter(|v| !v.trim().is_empty()) {
                    ev.push(format!("SDL_IM_MODULE={value}"));
                }
            }
            if let Some(item) = loaded(&["libfcitx", "libibus", "sdl"]) {
                ev.push(item);
                NamedPathCheck {
                    name: "sdl_module".to_string(),
                    status: "confirmed".to_string(),
                    evidence: ev,
                    missing: miss,
                }
            } else {
                miss.push("SDL input bridge .so evidence".to_string());
                NamedPathCheck {
                    name: "sdl_module".to_string(),
                    status: "missing".to_string(),
                    evidence: ev,
                    missing: miss,
                }
            }
        }
        "xim" => {
            let mut ev = Vec::new();
            let mut miss = Vec::new();
            if let Some(env) = env {
                if let Some(value) = env.get("XMODIFIERS").filter(|v| !v.trim().is_empty()) {
                    ev.push(format!("XMODIFIERS={value}"));
                }
            }
            let xim_env_ok = env_has(env.unwrap_or(&BTreeMap::new()), "XMODIFIERS", "@im=fcitx");
            if !xim_env_ok {
                miss.push("XMODIFIERS=@im=fcitx not set in target env".to_string());
            }
            if !locale_info.locale_valid {
                let loc = locale_info
                    .target_lc_ctype
                    .as_deref()
                    .or(locale_info.target_lang.as_deref())
                    .unwrap_or("C");
                miss.push(format!(
                    "locale '{loc}' is C/POSIX or not in locale -a; XIM may not activate"
                ));
            }
            if let Some(item) = loaded(&["im-xim", "libx11", "libxim"]) {
                ev.push(item);
                NamedPathCheck {
                    name: "xim".to_string(),
                    status: if xim_env_ok && locale_info.locale_valid {
                        "confirmed"
                    } else {
                        "configured"
                    }
                    .to_string(),
                    evidence: ev,
                    missing: miss,
                }
            } else if let Some(item) = available(&["im-xim"]) {
                ev.push(format!("available_on_disk={item}"));
                let locale_match =
                    check_immodule_locale("gtk_module", "xim", immodule_cache, locale_info);
                if !locale_match.is_empty() {
                    ev.push(locale_match);
                }
                NamedPathCheck {
                    name: "xim".to_string(),
                    status: if xim_env_ok && locale_info.locale_valid {
                        "configured"
                    } else {
                        "missing"
                    }
                    .to_string(),
                    evidence: ev,
                    missing: miss,
                }
            } else {
                miss.push("im-xim.so not found on disk".to_string());
                NamedPathCheck {
                    name: "xim".to_string(),
                    status: "missing".to_string(),
                    evidence: ev,
                    missing: miss,
                }
            }
        }
        _ => NamedPathCheck {
            name: path_name.to_string(),
            status: "unknown".to_string(),
            evidence: vec![],
            missing: vec!["unknown path name".to_string()],
        },
    }
}

fn check_immodule_locale(
    _path_name: &str,
    module_name: &str,
    immodule_cache: &[ImmoduleCacheEntry],
    locale_info: &LocaleInfo,
) -> String {
    let target_locale = locale_info
        .target_lc_ctype
        .as_deref()
        .or(locale_info.target_lang.as_deref())
        .unwrap_or("C");
    let locale_prefix = target_locale
        .split(|c: char| c == '.' || c == '_')
        .next()
        .unwrap_or("");
    let locale_lang = locale_prefix.split('_').next().unwrap_or("");

    for entry in immodule_cache {
        if !entry.module_name.contains(module_name) {
            continue;
        }
        let locales = &entry.locales;
        if locales.contains('*') {
            return format!(
                "immodule_cache: {} matches any locale (*)",
                entry.module_name
            );
        }
        let matches = locales.split(':').any(|loc| {
            loc == locale_prefix
                || loc == target_locale
                || loc == locale_lang
                || (loc.len() == 2 && locale_lang == loc)
        });
        return if matches {
            format!(
                "immodule_cache: {} locale '{}' matches target '{}'",
                entry.module_name, locales, target_locale
            )
        } else {
            format!(
                "immodule_cache: {} locale '{}' does NOT match target '{}'",
                entry.module_name, locales, target_locale
            )
        };
    }
    String::new()
}

async fn linux_display_evidence(
    args: &CheckIssueArgs,
    config: &DiagnosticsPluginConfig,
    report: &mut EvidenceReport,
) {
    for service in [
        "xdg-desktop-portal.service",
        "pipewire.service",
        "wireplumber.service",
    ] {
        systemd_user_active_check(config, report, service).await;
    }
    process_check(config, report, "Xwayland").await;
    linux_gpu_evidence(config, report).await;
    recent_logs(
        args,
        config,
        report,
        &["portal", "pipewire", "wireplumber", "wayland", "xwayland"],
    )
    .await;
}

async fn linux_audio_evidence(
    args: &CheckIssueArgs,
    config: &DiagnosticsPluginConfig,
    report: &mut EvidenceReport,
) {
    for service in [
        "pipewire.service",
        "wireplumber.service",
        "pipewire-pulse.service",
    ] {
        systemd_user_active_check(config, report, service).await;
    }
    command_exists_check(config, report, "wpctl").await;
    if command_path(config, "wpctl").await.is_some() {
        let output = run_command(config, "wpctl", &["status"], 3).await;
        push_log_if_stdout(report, "wpctl status", &output);
    }
    recent_logs(
        args,
        config,
        report,
        &["pipewire", "wireplumber", "pulse", "audio"],
    )
    .await;
}

async fn linux_package_evidence(
    args: &CheckIssueArgs,
    config: &DiagnosticsPluginConfig,
    report: &mut EvidenceReport,
) {
    for command in ["pacman", "yay", "paru"] {
        command_exists_check(config, report, command).await;
    }
    report.facts.insert(
        "package.pacman_db_lock_exists".to_string(),
        json!(Path::new("/var/lib/pacman/db.lck").exists()),
    );
    recent_logs(
        args,
        config,
        report,
        &["pacman", "error", "failed", "warning"],
    )
    .await;
}

async fn linux_gpu_evidence(config: &DiagnosticsPluginConfig, report: &mut EvidenceReport) {
    command_exists_check(config, report, "lspci").await;
    if command_path(config, "lspci").await.is_some() {
        let output = run_command(config, "lspci", &["-nnk"], 4).await;
        let gpu = extract_lspci_gpu_blocks(&output.stdout);
        if !gpu.is_empty() {
            report.facts.insert("gpu.lspci".to_string(), json!(gpu));
        }
    }
    command_exists_check(config, report, "nvidia-smi").await;
}

async fn linux_network_evidence(config: &DiagnosticsPluginConfig, report: &mut EvidenceReport) {
    for command in ["ip", "resolvectl", "ping"] {
        command_exists_check(config, report, command).await;
    }
    if command_path(config, "ip").await.is_some() {
        let output = run_command(config, "ip", &["-brief", "addr"], 3).await;
        push_log(
            report,
            "ip -brief addr",
            &mask_network_addresses(&output.stdout),
        );
    }
    if command_path(config, "resolvectl").await.is_some() {
        let output = run_command(config, "resolvectl", &["status"], 3).await;
        push_log_if_stdout(report, "resolvectl status", &output);
    }
}

async fn linux_storage_evidence(config: &DiagnosticsPluginConfig, report: &mut EvidenceReport) {
    command_exists_check(config, report, "df").await;
    if command_path(config, "df").await.is_some() {
        let output = run_command(config, "df", &["-hT"], 3).await;
        push_log_if_stdout(report, "df -hT", &output);
    }
    command_exists_check(config, report, "btrfs").await;
}

async fn command_exists_check(
    config: &DiagnosticsPluginConfig,
    report: &mut EvidenceReport,
    name: &str,
) {
    let path = command_path(config, name).await;
    report.checks.push(Check {
        id: format!("command.{name}.exists"),
        status: if path.is_some() {
            CheckStatus::Ok
        } else {
            CheckStatus::Unknown
        },
        detail: if path.is_some() {
            format!("{name} is available")
        } else {
            format!("{name} is not available")
        },
        evidence: path.into_iter().collect(),
    });
}

async fn process_check(
    config: &DiagnosticsPluginConfig,
    report: &mut EvidenceReport,
    name: &str,
) -> Vec<u32> {
    let output = run_command(config, "pgrep", &["-af", name], 2).await;
    let matches = filtered_process_matches(&output.stdout, name);
    report.checks.push(Check {
        id: format!("process.{name}.running"),
        status: if matches.is_empty() {
            CheckStatus::Unknown
        } else {
            CheckStatus::Ok
        },
        detail: if matches.is_empty() {
            format!("no process matching {name} was found")
        } else {
            format!("process matching {name} is running")
        },
        evidence: if matches.is_empty() {
            Vec::new()
        } else {
            vec![clip(&matches.join("\n"), 1_000)]
        },
    });
    matches
        .iter()
        .filter_map(|line| line.split_whitespace().next()?.parse::<u32>().ok())
        .collect()
}

async fn launch_probe_target(
    args: &CheckIssueArgs,
    config: &DiagnosticsPluginConfig,
    report: &mut EvidenceReport,
    target: &str,
) -> Vec<u32> {
    if !safe_command_name(target) {
        report
            .missing_evidence
            .push("launch probe skipped because target command name is not safe".to_string());
        return Vec::new();
    }
    let before = process_ids(config, target).await;
    let spawn = Command::new(target)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    let Ok(child) = spawn else {
        report
            .missing_evidence
            .push(format!("failed to launch {target} for runtime sampling"));
        return Vec::new();
    };
    tokio::time::sleep(Duration::from_secs(args.launch_timeout_seconds)).await;
    let after = process_ids(config, target).await;
    let new_pids = after
        .iter()
        .copied()
        .filter(|pid| !before.contains(pid))
        .collect::<Vec<_>>();
    report.facts.insert(
        "launch_probe".to_string(),
        json!({"target": target, "launched_pid": child.id(), "pids_before": before, "pids_after": after, "new_pids": new_pids}),
    );
    if new_pids.is_empty() {
        after
    } else {
        new_pids
    }
}

async fn process_ids(config: &DiagnosticsPluginConfig, name: &str) -> Vec<u32> {
    let output = run_command(config, "pgrep", &["-af", name], 2).await;
    filtered_process_matches(&output.stdout, name)
        .iter()
        .filter_map(|line| line.split_whitespace().next()?.parse::<u32>().ok())
        .collect()
}

fn filtered_process_matches(output: &str, name: &str) -> Vec<String> {
    let name_lower = name.to_ascii_lowercase();
    let mut matches = output
        .lines()
        .filter(|line| {
            let lower = line.to_ascii_lowercase();
            lower.contains(&name_lower)
                && !lower.contains("pgrep -af")
                && !lower.contains("/usr/bin/bash -c")
                && !lower.contains("/bin/sh -c")
                && !line_starts_with_pid(line, std::process::id())
        })
        .map(ToString::to_string)
        .collect::<Vec<_>>();
    matches.sort();
    matches
}

fn line_starts_with_pid(line: &str, pid: u32) -> bool {
    line.split_whitespace()
        .next()
        .and_then(|value| value.parse::<u32>().ok())
        == Some(pid)
}

fn read_process_input_env(pid: u32) -> Option<BTreeMap<String, String>> {
    let raw = std::fs::read(format!("/proc/{pid}/environ")).ok()?;
    let mut picked = BTreeMap::new();
    for item in raw.split(|byte| *byte == 0) {
        let entry = String::from_utf8_lossy(item);
        let Some((key, value)) = entry.split_once('=') else {
            continue;
        };
        if matches!(
            key,
            "GTK_IM_MODULE"
                | "QT_IM_MODULE"
                | "QT_IM_MODULES"
                | "XMODIFIERS"
                | "SDL_IM_MODULE"
                | "GLFW_IM_MODULE"
                | "XDG_SESSION_TYPE"
                | "WAYLAND_DISPLAY"
                | "DISPLAY"
                | "LANG"
                | "LC_ALL"
                | "LC_CTYPE"
        ) {
            picked.insert(key.to_string(), redact(value));
        }
    }
    Some(picked)
}

fn read_proc_cmdline(pid: u32) -> Option<String> {
    let raw = std::fs::read(format!("/proc/{pid}/cmdline")).ok()?;
    let parts = raw
        .split(|byte| *byte == 0)
        .filter(|part| !part.is_empty())
        .map(|part| redact(String::from_utf8_lossy(part)))
        .collect::<Vec<_>>();
    (!parts.is_empty()).then(|| parts.join(" "))
}

fn read_loaded_input_modules(pids: &[u32]) -> Vec<String> {
    let mut modules = BTreeSet::new();
    for pid in pids.iter().take(8) {
        let Ok(text) = std::fs::read_to_string(format!("/proc/{pid}/maps")) else {
            continue;
        };
        for line in text.lines() {
            if let Some(path) = input_module_path_from_maps_line(line) {
                modules.insert(format!("pid {pid}: {path}"));
            }
        }
    }
    modules.into_iter().take(80).collect()
}

fn input_module_path_from_maps_line(line: &str) -> Option<String> {
    let path = line.split_whitespace().last()?;
    let lower = path.to_ascii_lowercase();
    let is_input_module = lower.contains("/immodules/")
        || lower.contains("im-fcitx")
        || lower.contains("im-xim")
        || lower.contains("im-ibus")
        || lower.contains("im-wayland")
        || lower.contains("platforminputcontext")
        || lower.contains("libibus")
        || lower.contains("libfcitx");
    (is_input_module && (lower.ends_with(".so") || lower.contains(".so."))).then(|| redact(path))
}

fn scan_available_input_modules() -> Vec<String> {
    let mut modules = BTreeSet::new();
    for root in ["/usr/lib", "/usr/lib64", "/app/lib"] {
        scan_available_input_modules_under(Path::new(root), 0, &mut modules);
    }
    modules.into_iter().take(120).collect()
}

fn scan_available_input_modules_under(dir: &Path, depth: usize, modules: &mut BTreeSet<String>) {
    if depth > 5 || modules.len() >= 120 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten().take(300) {
        let path = entry.path();
        let text = path.display().to_string();
        let lower = text.to_ascii_lowercase();
        if path.is_dir() {
            if lower.contains("gtk")
                || lower.contains("immodules")
                || lower.contains("qt")
                || lower.contains("fcitx")
                || lower.contains("ibus")
            {
                scan_available_input_modules_under(&path, depth + 1, modules);
            }
        } else if input_module_file_name(&lower) {
            modules.insert(redact(&text));
        }
    }
}

fn input_module_file_name(lower_path: &str) -> bool {
    (lower_path.contains("/immodules/")
        || lower_path.contains("im-fcitx")
        || lower_path.contains("im-xim")
        || lower_path.contains("im-ibus")
        || lower_path.contains("im-wayland")
        || lower_path.contains("platforminputcontext"))
        && (lower_path.ends_with(".so") || lower_path.contains(".so."))
}

fn linux_desktop_exec_for_target(target: &str) -> Option<String> {
    let mut dirs = Vec::new();
    if let Ok(home) = std::env::var("HOME") {
        dirs.push(PathBuf::from(home).join(".local/share/applications"));
    }
    dirs.push(PathBuf::from("/usr/share/applications"));
    for dir in dirs {
        let Ok(entries) = std::fs::read_dir(dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|value| value.to_str()) != Some("desktop") {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            let exec = text.lines().find_map(|line| line.strip_prefix("Exec="));
            if path
                .file_stem()
                .and_then(|value| value.to_str())
                .is_some_and(|value| value.eq_ignore_ascii_case(target))
                || exec.is_some_and(|line| command_mentions_target(line, target))
            {
                return exec.map(redact);
            }
        }
    }
    None
}

fn command_mentions_target(line: &str, target: &str) -> bool {
    line.split(|ch: char| ch.is_whitespace() || ch == '/' || ch == '=')
        .any(|part| part == target)
}

fn infer_input_toolkit(text: &str) -> InputToolkit {
    let lower = text.to_ascii_lowercase();
    if lower.contains("qt_im_module")
        || lower.contains("platforminputcontext")
        || lower.contains("libqt")
    {
        InputToolkit::Qt
    } else if lower.contains("electron")
        || lower.contains("chromium")
        || lower.contains("chrome-sandbox")
        || lower.contains("steamwebhelper")
        || lower.contains("--ozone-platform")
        || lower.contains("linuxqq")
    {
        InputToolkit::ElectronChromium
    } else if lower.contains("gtk") || lower.contains("gdk") || lower.contains("immodules") {
        InputToolkit::Gtk
    } else if lower.contains("sdl") {
        InputToolkit::Sdl
    } else if lower.contains("java") {
        InputToolkit::Java
    } else if lower.contains("x11") || lower.contains("xlib") {
        InputToolkit::X11Legacy
    } else {
        InputToolkit::Unknown
    }
}

fn infer_display_mode(
    text: &str,
    env: Option<&BTreeMap<String, String>>,
    socket_mode: DisplayMode,
    loaded_modules: &[String],
) -> DisplayMode {
    let lower = text.to_ascii_lowercase();
    let has_ozone_wayland = lower.contains("--ozone-platform=wayland");

    if has_ozone_wayland {
        return DisplayMode::WaylandNative;
    }

    if socket_mode == DisplayMode::XWayland || socket_mode == DisplayMode::X11 {
        return DisplayMode::XWayland;
    }
    if socket_mode == DisplayMode::WaylandNative {
        return DisplayMode::WaylandNative;
    }

    let has_im_wayland = loaded_modules
        .iter()
        .any(|m| m.to_ascii_lowercase().contains("im-wayland"));
    if has_im_wayland {
        return DisplayMode::WaylandNative;
    }

    if let Some(env) = env {
        let has_wayland = env.get("WAYLAND_DISPLAY").is_some();
        let has_display = env.get("DISPLAY").is_some();
        return match (has_wayland, has_display) {
            (true, false) => DisplayMode::WaylandNative,
            (false, true) => DisplayMode::X11,
            (true, true) => DisplayMode::XWayland,
            _ => DisplayMode::Unknown,
        };
    }

    DisplayMode::Unknown
}

fn env_has(env: &BTreeMap<String, String>, key: &str, expected: &str) -> bool {
    env.get(key)
        .map(|value| value == expected || value.split(';').any(|item| item.trim() == expected))
        .unwrap_or(false)
}

fn loaded_module_evidence(loaded_modules: &[String], needles: &[&str]) -> Option<String> {
    loaded_modules.iter().find_map(|module| {
        let lower = module.to_ascii_lowercase();
        needles
            .iter()
            .any(|needle| lower.contains(&needle.to_ascii_lowercase()))
            .then(|| format!("runtime_loaded_module={module}"))
    })
}

fn available_module_evidence(available_modules: &[String], needles: &[&str]) -> Option<String> {
    available_modules.iter().find_map(|module| {
        let lower = module.to_ascii_lowercase();
        needles
            .iter()
            .any(|needle| lower.contains(&needle.to_ascii_lowercase()))
            .then(|| module.to_string())
    })
}

async fn systemd_user_active_check(
    config: &DiagnosticsPluginConfig,
    report: &mut EvidenceReport,
    service: &str,
) {
    let output = run_command(config, "systemctl", &["--user", "is-active", service], 2).await;
    report.checks.push(Check {
        id: format!("systemd_user.{service}.active"),
        status: if output.status == Some(0) {
            CheckStatus::Ok
        } else {
            CheckStatus::Warn
        },
        detail: format!("systemctl --user is-active {service}"),
        evidence: compact_evidence(&output),
    });
}

async fn app_probe_version(
    config: &DiagnosticsPluginConfig,
    report: &mut EvidenceReport,
    command: &str,
) {
    if !safe_command_name(command) {
        return;
    }
    let output = run_command(config, command, &["--version"], 2).await;
    push_log_if_stdout(report, &format!("{command} --version"), &output);
}

async fn package_owner(config: &DiagnosticsPluginConfig, report: &mut EvidenceReport, path: &str) {
    if command_path(config, "pacman").await.is_none() {
        return;
    }
    let output = run_command(config, "pacman", &["-Qo", path], 3).await;
    push_log_if_stdout(report, "pacman -Qo", &output);
}

fn package_probe_for_command(
    config: &DiagnosticsPluginConfig,
    command_path: &str,
    target: &str,
) -> Option<String> {
    let owner = std::process::Command::new("pacman")
        .args(["-Qo", command_path])
        .output()
        .ok()
        .filter(|output| output.status.success())?;
    let owner_text = String::from_utf8_lossy(&owner.stdout);
    let package = package_name_from_pacman_owner(&owner_text)?;
    if !safe_command_name(&package) {
        return None;
    }
    let output = std::process::Command::new("pacman")
        .args(["-Ql", &package])
        .output()
        .ok()
        .filter(|output| output.status.success())?;
    let mut lines = vec![format!("package={package}"), format!("target={target}")];
    lines.extend(
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter(|line| package_probe_line(line))
            .take(80)
            .map(ToString::to_string),
    );
    Some(redact(&clip(
        &lines.join("\n"),
        config.max_stdout_chars.min(4_000),
    )))
}

fn package_name_from_pacman_owner(text: &str) -> Option<String> {
    let parts = text.split_whitespace().collect::<Vec<_>>();
    if let Some(index) = parts.iter().position(|part| *part == "by" || *part == "由") {
        return parts.get(index + 1).map(|value| value.to_string());
    }
    None
}

fn package_probe_line(line: &str) -> bool {
    let lower = line.to_ascii_lowercase();
    lower.contains("libgtk")
        || lower.contains("libgdk")
        || lower.contains("libqt")
        || lower.contains("platforminputcontext")
        || lower.contains("immodules")
        || lower.contains("electron")
        || lower.contains("chrome")
        || lower.ends_with(".desktop")
        || lower.contains("/bin/")
}

async fn recent_logs(
    args: &CheckIssueArgs,
    config: &DiagnosticsPluginConfig,
    report: &mut EvidenceReport,
    needles: &[&str],
) {
    if args.depth == Depth::Quick || command_path(config, "journalctl").await.is_none() {
        return;
    }
    let since = format!("-{}min", args.recent_minutes);
    let output = run_command(
        config,
        "journalctl",
        &["--user", "--since", &since, "--no-pager", "-n", "200"],
        5,
    )
    .await;
    let text = output
        .stdout
        .lines()
        .filter(|line| {
            let lower = line.to_ascii_lowercase();
            needles
                .iter()
                .any(|needle| lower.contains(&needle.to_ascii_lowercase()))
        })
        .take(80)
        .collect::<Vec<_>>()
        .join("\n");
    push_log(report, "journalctl --user recent filtered", &text);
}

async fn command_path(config: &DiagnosticsPluginConfig, command: &str) -> Option<String> {
    if !safe_command_name(command) {
        return None;
    }
    let output = run_command(config, "which", &[command], 2).await;
    (output.status == Some(0))
        .then(|| {
            output
                .stdout
                .lines()
                .next()
                .unwrap_or_default()
                .trim()
                .to_string()
        })
        .filter(|value| !value.is_empty())
}

async fn run_command(
    config: &DiagnosticsPluginConfig,
    command: &str,
    args: &[&str],
    timeout_seconds: u64,
) -> ProbeOutput {
    if !safe_command_name(command) {
        return ProbeOutput {
            status: None,
            stdout: String::new(),
            stderr: String::new(),
            timed_out: false,
        };
    }
    let result = timeout(
        Duration::from_secs(timeout_seconds.min(config.command_timeout_seconds).max(1)),
        Command::new(command)
            .args(args)
            .stdin(Stdio::null())
            .output(),
    )
    .await;
    match result {
        Ok(Ok(output)) => ProbeOutput {
            status: output.status.code(),
            stdout: clip(
                &String::from_utf8_lossy(&output.stdout),
                config.max_stdout_chars,
            ),
            stderr: clip(
                &String::from_utf8_lossy(&output.stderr),
                config.max_stderr_chars,
            ),
            timed_out: false,
        },
        Ok(Err(err)) => ProbeOutput {
            status: None,
            stdout: String::new(),
            stderr: err.to_string(),
            timed_out: false,
        },
        Err(_) => ProbeOutput {
            status: None,
            stdout: String::new(),
            stderr: String::new(),
            timed_out: true,
        },
    }
}

fn fact_env(report: &mut EvidenceReport, key: &str, env: &str) {
    if let Ok(value) = std::env::var(env) {
        if !value.trim().is_empty() {
            report.facts.insert(key.to_string(), json!(redact(&value)));
        }
    }
}

fn push_log_if_stdout(report: &mut EvidenceReport, source: &str, output: &ProbeOutput) {
    if !output.stdout.trim().is_empty() {
        push_log(report, source, &output.stdout);
    }
}

fn push_log(report: &mut EvidenceReport, source: &str, message: &str) {
    if !message.trim().is_empty() {
        report.logs.push(LogExcerpt {
            source: source.to_string(),
            message: clip(message, 2_000),
        });
    }
}

fn compact_evidence(output: &ProbeOutput) -> Vec<String> {
    let mut evidence = Vec::new();
    if let Some(status) = output.status {
        evidence.push(format!("exit={status}"));
    }
    if !output.stdout.trim().is_empty() {
        evidence.push(format!("stdout={}", clip(&output.stdout, 800)));
    }
    if !output.stderr.trim().is_empty() {
        evidence.push(format!("stderr={}", clip(&output.stderr, 800)));
    }
    if output.timed_out {
        evidence.push("timed_out=true".to_string());
    }
    evidence
}

fn extract_lspci_gpu_blocks(text: &str) -> Vec<String> {
    text.lines()
        .filter(|line| {
            let lower = line.to_ascii_lowercase();
            lower.contains("vga")
                || lower.contains("3d controller")
                || lower.contains("display controller")
                || lower.contains("kernel driver in use")
        })
        .take(80)
        .map(redact)
        .collect()
}

fn os_release_value(text: &str, key: &str) -> Option<String> {
    text.lines().find_map(|line| {
        let (name, value) = line.split_once('=')?;
        (name == key).then(|| value.trim_matches('"').to_string())
    })
}

fn mask_network_addresses(text: &str) -> String {
    text.split_whitespace()
        .map(|part| {
            if part.contains('.') && part.chars().any(|ch| ch.is_ascii_digit()) {
                "<ipv4>".to_string()
            } else if part.contains(':') && part.chars().any(|ch| ch.is_ascii_hexdigit()) {
                "<ipv6-or-mac>".to_string()
            } else {
                part.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn redact(value: impl AsRef<str>) -> String {
    let home = std::env::var("HOME").unwrap_or_default();
    if home.is_empty() {
        value.as_ref().to_string()
    } else {
        value.as_ref().replace(&home, "$HOME")
    }
}

fn clip(value: &str, max_chars: usize) -> String {
    let value = value.trim();
    if value.chars().count() <= max_chars {
        value.to_string()
    } else {
        format!(
            "{}...",
            value
                .chars()
                .take(max_chars.saturating_sub(3))
                .collect::<String>()
        )
    }
}

fn contains_any(text: &str, needles: &[&str]) -> bool {
    needles.iter().any(|needle| text.contains(needle))
}

fn safe_command_name(value: &str) -> bool {
    !value.trim().is_empty()
        && value
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.' | '+'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn check_issue_infers_input_method_area() {
        let args = parse_args(json!({"query": "QQ 打不了中文", "target": "qq"})).unwrap();
        assert!(matches!(args.area, Area::InputMethod));
    }

    #[test]
    fn input_method_path_needs_runtime_evidence() {
        let status = input_method_path_status(
            InputToolkit::Unknown,
            DisplayMode::Unknown,
            false,
            None,
            &[],
            &[],
            &[],
            &WaylandProtocolInfo {
                compositor_supports_text_input_v3: false,
                fcitx5_wayland_frontend_loaded: false,
                wayland_info_available: false,
            },
            &LocaleInfo {
                target_lang: None,
                target_lc_ctype: None,
                available_locales: vec![],
                locale_valid: false,
            },
        );
        assert_eq!(status.overall, "path_evidence_incomplete");
    }
}
