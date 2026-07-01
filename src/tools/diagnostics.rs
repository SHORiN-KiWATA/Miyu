use super::{ToolRegistry, ToolSpec};
use crate::config::{AppConfig, DiagnosticsPluginConfig};
use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::process::Stdio;
use std::time::Duration;
use tokio::process::Command;
use tokio::time::timeout;

pub fn register(registry: &mut ToolRegistry, config: AppConfig) {
    registry.register(ToolSpec::new(
        "inspect_issue",
        "Collect read-only local facts for a reported computer issue. Covers app startup, input method, display or screen sharing, audio, package updates, GPU or driver, network, storage, and general system context. This only gathers evidence; it does not diagnose or produce final advice. After using it, combine the result with knowledge base, memory, and web search/fetch when needed. It does not modify the system.",
        json!({
            "type": "object",
            "properties": {
                "query": { "type": "string", "description": "Optional original user request. Used only as fallback when mode is auto or omitted." },
                "mode": { "type": "string", "enum": ["auto", "system", "app", "input_method", "display", "audio", "package_update", "gpu", "network", "storage"], "description": "Probe mode. Use auto only when passing query and no structured mode is obvious." },
                "target": { "type": "string", "description": "Optional target app, process, command, or subsystem, for example qq or opencode." },
                "symptom": { "type": "string", "description": "Optional symptom such as cannot_start, app_cannot_input_chinese, no_audio, screen_share_failed." },
                "depth": { "type": "string", "enum": ["quick", "normal", "full"], "description": "Probe depth. Start with quick or normal; full may run slower probes." },
                "recent_minutes": { "type": "integer", "description": "Recent log window in minutes, clamped to 1..1440." },
                "platform": { "type": "string", "enum": ["auto", "linux", "macos"], "description": "Platform override. Prefer auto." },
                "allow_launch_probe": { "type": "boolean", "description": "For app/input_method diagnosis only: explicitly allow launching the target if it is not already running, so runtime process facts can be sampled. Defaults to false." },
                "launch_timeout_seconds": { "type": "integer", "description": "Seconds to wait after an allowed launch probe before sampling pids. Defaults to 3, max 15." }
            },
            "required": [],
            "additionalProperties": false
        }),
        move |args| {
            let config = config.clone();
            async move { inspect_issue(args, config.plugins.diagnostics.clone()).await }
        },
    ));
}

#[derive(Debug, Clone)]
struct DiagnoseArgs {
    query: Option<String>,
    mode: Mode,
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
enum Mode {
    System,
    App,
    InputMethod,
    Display,
    Audio,
    PackageUpdate,
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

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
enum Platform {
    Linux,
    Macos,
    Unsupported,
}

#[derive(Debug, Serialize)]
struct DiagnosticReport {
    ok: bool,
    platform: Platform,
    query: Option<String>,
    mode: Mode,
    target: Option<String>,
    symptom: Option<String>,
    depth: Depth,
    summary: String,
    facts: BTreeMap<String, Value>,
    checks: Vec<Check>,
    logs: Vec<LogExcerpt>,
    findings: Vec<Finding>,
    hypotheses: Vec<Hypothesis>,
    confidence: Option<Confidence>,
    evidence_notes: Vec<String>,
    missing_evidence: Vec<String>,
    next_questions: Vec<String>,
    output_instruction: String,
}

#[derive(Debug, Serialize)]
struct Check {
    id: String,
    status: CheckStatus,
    detail: String,
    evidence: Vec<String>,
}

#[derive(Debug, PartialEq, Eq, Serialize)]
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

#[derive(Debug, Serialize)]
struct Finding {
    severity: Severity,
    title: String,
    evidence: String,
}

#[derive(Debug, Serialize)]
struct Hypothesis {
    title: String,
    rationale: String,
    required_evidence: Vec<String>,
}

#[derive(Debug, Serialize)]
struct Confidence {
    score: u8,
    label: String,
    threshold: u8,
    can_conclude: bool,
    answer_tone: String,
    required_language: Vec<String>,
    forbidden_language: Vec<String>,
    reason: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum InputToolkit {
    ElectronChromium,
    Gtk,
    Qt,
    Sdl,
    Java,
    X11Legacy,
    Unknown,
}

#[derive(Debug, Clone, Serialize)]
struct InputMethodAppProfile {
    toolkit: InputToolkit,
    display_backend: String,
    runtime_observed: bool,
    loaded_input_modules: Vec<String>,
    command_line: Option<String>,
    desktop_exec: Option<String>,
    evidence_text: String,
    electron_uses_wayland: bool,
    electron_wayland_ime: bool,
}

#[derive(Debug, Serialize)]
struct InputMethodPathReport {
    core_model: String,
    app_adaptation: PathVerdict,
    environment_module_path: PathVerdict,
    wayland_protocol_path: PathVerdict,
    decision_rule: String,
}

#[derive(Debug, Serialize)]
struct InputMethodQuestions {
    backend_question: BackendQuestion,
    toolkit_question: ToolkitQuestion,
    module_question: ModuleQuestion,
    environment_question: EnvironmentQuestion,
    answer_rule: String,
}

#[derive(Debug, Serialize)]
struct BackendQuestion {
    backend: String,
    text_input_support: FactStatus,
    evidence: Vec<String>,
    missing_evidence: Vec<String>,
}

#[derive(Debug, Serialize)]
struct ToolkitQuestion {
    toolkit: String,
    status: FactStatus,
    evidence: Vec<String>,
    missing_evidence: Vec<String>,
}

#[derive(Debug, Serialize)]
struct ModuleQuestion {
    confirmed_modules: Vec<InputModuleFact>,
    unsupported_modules: Vec<InputModuleFact>,
    unknown_modules: Vec<InputModuleFact>,
    gtk_immodule_cache: Vec<GtkImModuleCacheEntry>,
    gtk_requested_module: Option<GtkRequestedModuleReport>,
    gtk_locale_selected_modules: Vec<GtkImModuleCacheEntry>,
    rule: String,
}

#[derive(Debug, Serialize)]
struct InputModuleFact {
    module: String,
    status: FactStatus,
    evidence: Vec<String>,
    activation_conditions: Vec<String>,
    missing_evidence: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct GtkImModuleCacheEntry {
    module: String,
    path: String,
    locales: Vec<String>,
    source: String,
}

#[derive(Debug, Serialize)]
struct GtkRequestedModuleReport {
    requested: String,
    present_in_cache: bool,
    matching_entries: Vec<GtkImModuleCacheEntry>,
    evidence: Vec<String>,
}

#[derive(Debug, Serialize)]
struct EnvironmentQuestion {
    current_shell_env: BTreeMap<String, String>,
    desktop_exec_env: BTreeMap<String, String>,
    target_process_env: Option<BTreeMap<String, String>>,
    missing_evidence: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum FactStatus {
    Confirmed,
    Unknown,
}

#[derive(Debug, Serialize)]
struct PathVerdict {
    status: String,
    evidence: Vec<String>,
    missing: Vec<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "snake_case")]
enum Severity {
    Medium,
    High,
}

#[derive(Debug)]
struct ProbeOutput {
    status: Option<i32>,
    stdout: String,
    stderr: String,
    error: Option<String>,
    timed_out: bool,
}

async fn inspect_issue(args: Value, config: DiagnosticsPluginConfig) -> Result<String> {
    if !config.enabled {
        bail!("diagnostics plugin is disabled");
    }
    let args = parse_args(args)?;
    let platform = detect_platform(args.platform);
    let mut report = DiagnosticReport {
        ok: true,
        platform,
        query: args.query.clone(),
        mode: args.mode,
        target: args.target.clone(),
        symptom: args.symptom.clone(),
        depth: args.depth,
        summary: String::new(),
        facts: BTreeMap::new(),
        checks: Vec::new(),
        logs: Vec::new(),
        findings: Vec::new(),
        hypotheses: Vec::new(),
        confidence: None,
        evidence_notes: Vec::new(),
        missing_evidence: Vec::new(),
        next_questions: Vec::new(),
        output_instruction: "Use these facts as evidence, not as an automatic verdict. For input-method issues, answer whether the app backend/toolkit, required input module/protocol, and target process environment form a complete path. Do not claim Electron/Chromium/AppImage/Flatpak behavior from generic knowledge without concrete app/runtime evidence. If runtime facts are missing, ask for/perform an allowed launch probe instead of guessing. Use fcitx5_input_method_wiki_qurey for Fcitx rules when needed.".to_string(),
    };
    match platform {
        Platform::Linux => run_linux_plan(&args, &config, &mut report).await,
        Platform::Macos => run_macos_plan(&args, &config, &mut report).await,
        Platform::Unsupported => {
            report.ok = false;
            report.summary = "unsupported platform".to_string();
            report.checks.push(Check {
                id: "platform.supported".to_string(),
                status: CheckStatus::Error,
                detail: "only linux and macos are supported by diagnostics".to_string(),
                evidence: vec![std::env::consts::OS.to_string()],
            });
        }
    }
    finalize_summary(&mut report);
    Ok(serde_json::to_string_pretty(&report)?)
}

fn parse_args(args: Value) -> Result<DiagnoseArgs> {
    let query = optional_string(&args, "query", 500);
    let mode_raw = args
        .get("mode")
        .and_then(Value::as_str)
        .unwrap_or("auto")
        .trim();
    let mut target = optional_string(&args, "target", 160);
    let mut symptom = optional_string(&args, "symptom", 200);
    let mode = if mode_raw == "auto" {
        let inferred =
            infer_probe_request(query.as_deref(), target.as_deref(), symptom.as_deref())?;
        if target.is_none() {
            target = inferred.target;
        }
        if symptom.is_none() {
            symptom = inferred.symptom;
        }
        inferred.mode
    } else {
        parse_mode(mode_raw)?
    };
    let depth = parse_depth(
        args.get("depth")
            .and_then(Value::as_str)
            .unwrap_or("normal")
            .trim(),
    )?;
    let recent_minutes = args
        .get("recent_minutes")
        .and_then(Value::as_u64)
        .unwrap_or(30)
        .clamp(1, 1440);
    let platform = parse_platform_arg(
        args.get("platform")
            .and_then(Value::as_str)
            .unwrap_or("auto")
            .trim(),
    )?;
    let allow_launch_probe = args
        .get("allow_launch_probe")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let launch_timeout_seconds = args
        .get("launch_timeout_seconds")
        .and_then(Value::as_u64)
        .unwrap_or(3)
        .clamp(1, 15);
    Ok(DiagnoseArgs {
        query,
        mode,
        target,
        symptom,
        depth,
        recent_minutes,
        platform,
        allow_launch_probe,
        launch_timeout_seconds,
    })
}

fn optional_string(args: &Value, name: &str, max_chars: usize) -> Option<String> {
    args.get(name)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| value.chars().take(max_chars).collect())
}

struct InferredProbeRequest {
    mode: Mode,
    target: Option<String>,
    symptom: Option<String>,
}

fn infer_probe_request(
    query: Option<&str>,
    target: Option<&str>,
    symptom: Option<&str>,
) -> Result<InferredProbeRequest> {
    let text = query.unwrap_or_default().trim();
    let lower = text.to_ascii_lowercase();
    let mode = if contains_any(
        text,
        &[
            "输入法",
            "打不了中文",
            "候选框",
            "fcitx",
            "fcitx5",
            "ibus",
            "拼音",
        ],
    ) || contains_any(&lower, &["ime", "input method"])
    {
        Mode::InputMethod
    } else if contains_any(text, &["没声音", "声音", "麦克风", "耳机", "音频"])
        || contains_any(
            &lower,
            &["audio", "sound", "microphone", "pipewire", "wireplumber"],
        )
    {
        Mode::Audio
    } else if contains_any(
        text,
        &["屏幕分享", "黑屏", "截图", "录屏", "显示器", "窗口", "闪屏"],
    ) || contains_any(
        &lower,
        &["display", "screen", "wayland", "xwayland", "portal"],
    ) {
        Mode::Display
    } else if contains_any(text, &["更新", "安装包", "依赖", "滚挂", "包管理"])
        || contains_any(
            &lower,
            &["pacman", "yay", "paru", "aur", "dnf", "apt", "brew"],
        )
    {
        Mode::PackageUpdate
    } else if contains_any(text, &["显卡", "驱动", "独显", "核显"])
        || contains_any(&lower, &["gpu", "nvidia", "amd", "mesa", "vulkan"])
    {
        Mode::Gpu
    } else if contains_any(
        text,
        &["网络", "联网", "断网", "dns", "网卡", "wifi", "wi-fi"],
    ) || contains_any(&lower, &["network", "internet", "wifi", "wi-fi", "dns"])
    {
        Mode::Network
    } else if contains_any(text, &["磁盘", "硬盘", "空间", "挂载", "btrfs", "快照"])
        || contains_any(&lower, &["disk", "storage", "mount", "btrfs", "filesystem"])
    {
        Mode::Storage
    } else if target.is_some()
        || contains_any(text, &["打不开", "启动不了", "闪退", "崩溃", "报错"])
        || contains_any(&lower, &["crash", "cannot start", "won't open", "not open"])
    {
        Mode::App
    } else if text.is_empty() {
        bail!("mode is auto but query is empty; provide query or a structured mode")
    } else {
        Mode::System
    };
    Ok(InferredProbeRequest {
        mode,
        target: target
            .map(ToString::to_string)
            .or_else(|| infer_target(text)),
        symptom: symptom
            .map(ToString::to_string)
            .or_else(|| infer_symptom(text, mode)),
    })
}

fn contains_any(text: &str, needles: &[&str]) -> bool {
    needles.iter().any(|needle| text.contains(needle))
}

fn infer_target(text: &str) -> Option<String> {
    let lower = text.to_ascii_lowercase();
    for (needle, target) in [
        ("opencode", "opencode"),
        ("open code", "opencode"),
        ("linuxqq", "qq"),
        ("qq", "qq"),
        ("微信", "wechat"),
        ("wechat", "wechat"),
        ("steam", "steam"),
        ("firefox", "firefox"),
        ("chrome", "chrome"),
        ("chromium", "chromium"),
        ("wps", "wps"),
        ("vscode", "code"),
        ("code", "code"),
    ] {
        if lower.contains(needle) || text.contains(needle) {
            return Some(target.to_string());
        }
    }
    None
}

fn infer_symptom(text: &str, mode: Mode) -> Option<String> {
    let lower = text.to_ascii_lowercase();
    match mode {
        Mode::InputMethod => Some("app_cannot_input_chinese".to_string()),
        Mode::App
            if contains_any(text, &["打不开", "启动不了", "闪退", "崩溃"])
                || contains_any(&lower, &["crash", "cannot start", "won't open", "not open"]) =>
        {
            Some("cannot_start".to_string())
        }
        Mode::Audio => Some("audio_problem".to_string()),
        Mode::Display => Some("display_problem".to_string()),
        Mode::Network => Some("network_problem".to_string()),
        Mode::PackageUpdate => Some("package_update_problem".to_string()),
        Mode::Storage => Some("storage_problem".to_string()),
        Mode::Gpu => Some("gpu_problem".to_string()),
        _ => None,
    }
}

fn parse_mode(value: &str) -> Result<Mode> {
    match value {
        "system" => Ok(Mode::System),
        "app" => Ok(Mode::App),
        "input_method" => Ok(Mode::InputMethod),
        "display" => Ok(Mode::Display),
        "audio" => Ok(Mode::Audio),
        "package_update" => Ok(Mode::PackageUpdate),
        "gpu" => Ok(Mode::Gpu),
        "network" => Ok(Mode::Network),
        "storage" => Ok(Mode::Storage),
        _ => bail!("unsupported diagnostic mode: {value}"),
    }
}

fn parse_depth(value: &str) -> Result<Depth> {
    match value {
        "quick" => Ok(Depth::Quick),
        "normal" => Ok(Depth::Normal),
        "full" => Ok(Depth::Full),
        _ => bail!("unsupported diagnostic depth: {value}"),
    }
}

fn parse_platform_arg(value: &str) -> Result<PlatformArg> {
    match value {
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

async fn run_linux_plan(
    args: &DiagnoseArgs,
    config: &DiagnosticsPluginConfig,
    report: &mut DiagnosticReport,
) {
    linux_system_facts(config, report).await;
    match args.mode {
        Mode::System => linux_system_checks(config, report).await,
        Mode::App => linux_app_checks(args, config, report).await,
        Mode::InputMethod => linux_input_method_checks(args, config, report).await,
        Mode::Display => linux_display_checks(args, config, report).await,
        Mode::Audio => linux_audio_checks(args, config, report).await,
        Mode::PackageUpdate => linux_package_checks(args, config, report).await,
        Mode::Gpu => linux_gpu_checks(config, report).await,
        Mode::Network => linux_network_checks(config, report).await,
        Mode::Storage => linux_storage_checks(config, report).await,
    }
}

async fn run_macos_plan(
    args: &DiagnoseArgs,
    config: &DiagnosticsPluginConfig,
    report: &mut DiagnosticReport,
) {
    macos_system_facts(config, report).await;
    match args.mode {
        Mode::System => macos_system_checks(config, report).await,
        Mode::App => macos_app_checks(args, config, report).await,
        Mode::InputMethod => macos_input_method_checks(args, config, report).await,
        Mode::Display => macos_display_checks(config, report).await,
        Mode::Audio => macos_audio_checks(config, report).await,
        Mode::PackageUpdate => macos_package_checks(config, report).await,
        Mode::Network => macos_network_checks(config, report).await,
        Mode::Storage => macos_storage_checks(config, report).await,
        Mode::Gpu => macos_display_checks(config, report).await,
    }
}

async fn linux_system_facts(config: &DiagnosticsPluginConfig, report: &mut DiagnosticReport) {
    fact_env(report, "env.shell", "SHELL");
    fact_env(report, "env.term", "TERM");
    fact_env(report, "env.lang", "LANG");
    for key in [
        "XDG_SESSION_TYPE",
        "XDG_CURRENT_DESKTOP",
        "DESKTOP_SESSION",
        "WAYLAND_DISPLAY",
        "DISPLAY",
        "GTK_IM_MODULE",
        "QT_IM_MODULE",
        "XMODIFIERS",
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

async fn linux_system_checks(config: &DiagnosticsPluginConfig, report: &mut DiagnosticReport) {
    for command in ["systemctl", "journalctl", "loginctl", "ip", "df"] {
        command_exists_check(config, report, command).await;
    }
}

async fn linux_app_checks(
    args: &DiagnoseArgs,
    config: &DiagnosticsPluginConfig,
    report: &mut DiagnosticReport,
) {
    let Some(target) = args.target.as_deref() else {
        report
            .next_questions
            .push("which app should I probe?".to_string());
        return;
    };
    match command_path(config, target).await {
        Some(path) => {
            report
                .facts
                .insert("app.command_path".to_string(), json!(path.clone()));
            report.checks.push(Check {
                id: "app.command_exists".to_string(),
                status: CheckStatus::Ok,
                detail: format!("{target} exists in PATH"),
                evidence: vec![path.clone()],
            });
            app_probe_version(config, report, target).await;
            app_probe_help(config, report, target).await;
            linux_package_owner(config, report, &path).await;
            node_runtime_if_relevant(config, report, target, &path).await;
        }
        None => {
            report.checks.push(Check {
                id: "app.command_exists".to_string(),
                status: CheckStatus::Error,
                detail: format!("{target} was not found in PATH"),
                evidence: Vec::new(),
            });
            report.findings.push(Finding {
                severity: Severity::High,
                title: format!("{target} is not available in the current PATH"),
                evidence: "command -v returned no path".to_string(),
            });
        }
    }
    linux_recent_logs(args, config, report, &[target, "node", "error", "failed"]).await;
}

async fn linux_input_method_checks(
    args: &DiagnoseArgs,
    config: &DiagnosticsPluginConfig,
    report: &mut DiagnosticReport,
) {
    linux_input_method_config_facts(report);
    linux_wayland_compositor_checks(config, report).await;
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
    if let Some(target) = args.target.as_deref() {
        let mut pids = process_check(config, report, target).await;
        if pids.is_empty() {
            if args.allow_launch_probe {
                pids = launch_probe_target(args, config, report, target).await;
            }
            if pids.is_empty() {
                report.missing_evidence.push(format!(
                    "target app {target} is not running; cannot inspect runtime backend, actual environment, or Wayland/XWayland state"
                ));
                report.evidence_notes.push(
                    "desktop file and package-level evidence are weaker than runtime process evidence"
                        .to_string(),
                );
            }
        }
        linux_app_input_env(report, target, &pids);
        let loaded_modules = read_loaded_input_modules(&pids);
        if !loaded_modules.is_empty() {
            report.facts.insert(
                "input_method.runtime_loaded_modules".to_string(),
                json!(loaded_modules.clone()),
            );
        }
        let available_modules = scan_available_input_modules();
        if !available_modules.is_empty() {
            report.facts.insert(
                "input_method.available_modules".to_string(),
                json!(available_modules),
            );
        }
        let gtk_cache_entries = scan_gtk_immodule_cache_entries();
        if !gtk_cache_entries.is_empty() {
            report.facts.insert(
                "input_method.gtk_immodule_cache".to_string(),
                json!(gtk_cache_entries),
            );
        }
        let profile =
            linux_input_method_app_profile(config, report, target, &pids, loaded_modules).await;
        linux_input_method_profile_checks(report, target, profile.as_ref());
        linux_input_method_path_report(report, profile.as_ref());
        linux_fcitx_package_checks(config, report).await;
        linux_recent_logs(
            args,
            config,
            report,
            &[target, "fcitx", "ibus", "qt", "gtk", "xwayland"],
        )
        .await;
    } else {
        report
            .missing_evidence
            .push("target app was not provided; cannot compare app toolkit, backend, and per-process environment".to_string());
    }
    linux_input_method_env_checks(report);
    finalize_input_method_confidence(report);
}

async fn linux_display_checks(
    args: &DiagnoseArgs,
    config: &DiagnosticsPluginConfig,
    report: &mut DiagnosticReport,
) {
    for service in [
        "xdg-desktop-portal.service",
        "pipewire.service",
        "wireplumber.service",
    ] {
        systemd_user_active_check(config, report, service).await;
    }
    process_check(config, report, "Xwayland").await;
    linux_gpu_checks(config, report).await;
    linux_recent_logs(
        args,
        config,
        report,
        &["portal", "pipewire", "wireplumber", "wayland", "xwayland"],
    )
    .await;
}

async fn linux_audio_checks(
    args: &DiagnoseArgs,
    config: &DiagnosticsPluginConfig,
    report: &mut DiagnosticReport,
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
        if !output.stdout.trim().is_empty() {
            report.logs.push(LogExcerpt {
                source: "wpctl status".to_string(),
                message: clip(&output.stdout, 2_000),
            });
        }
    }
    linux_recent_logs(
        args,
        config,
        report,
        &["pipewire", "wireplumber", "pulse", "audio"],
    )
    .await;
}

async fn linux_package_checks(
    args: &DiagnoseArgs,
    config: &DiagnosticsPluginConfig,
    report: &mut DiagnosticReport,
) {
    command_exists_check(config, report, "pacman").await;
    if std::path::Path::new("/var/lib/pacman/db.lck").exists() {
        report.findings.push(Finding {
            severity: Severity::High,
            title: "pacman database lock exists".to_string(),
            evidence: "/var/lib/pacman/db.lck exists".to_string(),
        });
    }
    if command_path(config, "pacman").await.is_some() {
        let output = run_command(config, "pacman", &["-Q", "archlinux-keyring"], 3).await;
        if output.status == Some(0) && !output.stdout.trim().is_empty() {
            report.facts.insert(
                "package.archlinux_keyring".to_string(),
                json!(output.stdout.trim()),
            );
        }
    }
    linux_recent_logs(
        args,
        config,
        report,
        &["pacman", "error", "failed", "warning"],
    )
    .await;
}

async fn linux_gpu_checks(config: &DiagnosticsPluginConfig, report: &mut DiagnosticReport) {
    command_exists_check(config, report, "lspci").await;
    if command_path(config, "lspci").await.is_some() {
        let output = run_command(config, "lspci", &["-nnk"], 4).await;
        let gpu_lines = extract_lspci_gpu_blocks(&output.stdout);
        if !gpu_lines.is_empty() {
            report
                .facts
                .insert("gpu.lspci".to_string(), json!(gpu_lines));
        }
    }
    command_exists_check(config, report, "nvidia-smi").await;
}

async fn linux_network_checks(config: &DiagnosticsPluginConfig, report: &mut DiagnosticReport) {
    for command in ["ip", "resolvectl", "ping"] {
        command_exists_check(config, report, command).await;
    }
    if command_path(config, "ip").await.is_some() {
        let output = run_command(config, "ip", &["-brief", "addr"], 3).await;
        if !output.stdout.trim().is_empty() {
            report.logs.push(LogExcerpt {
                source: "ip -brief addr".to_string(),
                message: clip(&mask_network_addresses(&output.stdout), 2_000),
            });
        }
    }
    if command_path(config, "resolvectl").await.is_some() {
        let output = run_command(config, "resolvectl", &["status"], 3).await;
        if !output.stdout.trim().is_empty() {
            report.logs.push(LogExcerpt {
                source: "resolvectl status".to_string(),
                message: clip(&output.stdout, 2_000),
            });
        }
    }
}

async fn linux_storage_checks(config: &DiagnosticsPluginConfig, report: &mut DiagnosticReport) {
    command_exists_check(config, report, "df").await;
    if command_path(config, "df").await.is_some() {
        let output = run_command(config, "df", &["-hT"], 3).await;
        if !output.stdout.trim().is_empty() {
            report.logs.push(LogExcerpt {
                source: "df -hT".to_string(),
                message: clip(&output.stdout, 2_000),
            });
        }
    }
    command_exists_check(config, report, "btrfs").await;
}

async fn macos_system_facts(config: &DiagnosticsPluginConfig, report: &mut DiagnosticReport) {
    fact_env(report, "env.shell", "SHELL");
    fact_env(report, "env.term", "TERM");
    fact_env(report, "env.lang", "LANG");
    let sw_vers = run_command(config, "sw_vers", &[], 2).await;
    if !sw_vers.stdout.trim().is_empty() {
        report
            .facts
            .insert("os.sw_vers".to_string(), json!(sw_vers.stdout.trim()));
    }
    let arch = run_command(config, "uname", &["-m"], 2).await;
    if !arch.stdout.trim().is_empty() {
        report
            .facts
            .insert("hardware.arch".to_string(), json!(arch.stdout.trim()));
    }
}

async fn macos_system_checks(config: &DiagnosticsPluginConfig, report: &mut DiagnosticReport) {
    for command in ["sw_vers", "launchctl", "log", "system_profiler", "df"] {
        command_exists_check(config, report, command).await;
    }
}

async fn macos_app_checks(
    args: &DiagnoseArgs,
    config: &DiagnosticsPluginConfig,
    report: &mut DiagnosticReport,
) {
    let Some(target) = args.target.as_deref() else {
        report
            .next_questions
            .push("which app should I probe?".to_string());
        return;
    };
    match command_path(config, target).await {
        Some(path) => {
            report
                .facts
                .insert("app.command_path".to_string(), json!(path.clone()));
            report.checks.push(Check {
                id: "app.command_exists".to_string(),
                status: CheckStatus::Ok,
                detail: format!("{target} exists in PATH"),
                evidence: vec![path.clone()],
            });
            app_probe_version(config, report, target).await;
            app_probe_help(config, report, target).await;
            macos_quarantine_check(config, report, &path).await;
            macos_codesign_check(config, report, &path).await;
            node_runtime_if_relevant(config, report, target, &path).await;
        }
        None => {
            report.checks.push(Check {
                id: "app.command_exists".to_string(),
                status: CheckStatus::Error,
                detail: format!("{target} was not found in PATH"),
                evidence: Vec::new(),
            });
        }
    }
    macos_recent_logs(args, config, report, &[target, "error", "failed"]).await;
}

async fn macos_input_method_checks(
    args: &DiagnoseArgs,
    config: &DiagnosticsPluginConfig,
    report: &mut DiagnosticReport,
) {
    let output = run_command(
        config,
        "defaults",
        &["read", "com.apple.HIToolbox", "AppleSelectedInputSources"],
        3,
    )
    .await;
    if !output.stdout.trim().is_empty() {
        report.logs.push(LogExcerpt {
            source: "AppleSelectedInputSources".to_string(),
            message: clip(&output.stdout, 2_000),
        });
    }
    if let Some(target) = args.target.as_deref() {
        process_check(config, report, target).await;
        macos_recent_logs(args, config, report, &[target, "InputMethodKit", "TIS"]).await;
    }
}

async fn macos_display_checks(config: &DiagnosticsPluginConfig, report: &mut DiagnosticReport) {
    system_profiler_check(
        config,
        report,
        "SPDisplaysDataType",
        "display.system_profiler",
    )
    .await;
}

async fn macos_audio_checks(config: &DiagnosticsPluginConfig, report: &mut DiagnosticReport) {
    system_profiler_check(config, report, "SPAudioDataType", "audio.system_profiler").await;
}

async fn macos_package_checks(config: &DiagnosticsPluginConfig, report: &mut DiagnosticReport) {
    for command in ["brew", "port", "nix"] {
        command_exists_check(config, report, command).await;
    }
}

async fn macos_network_checks(config: &DiagnosticsPluginConfig, report: &mut DiagnosticReport) {
    for command in ["ifconfig", "scutil", "networksetup"] {
        command_exists_check(config, report, command).await;
    }
    if command_path(config, "scutil").await.is_some() {
        let output = run_command(config, "scutil", &["--dns"], 3).await;
        if !output.stdout.trim().is_empty() {
            report.logs.push(LogExcerpt {
                source: "scutil --dns".to_string(),
                message: clip(&output.stdout, 2_000),
            });
        }
    }
}

async fn macos_storage_checks(config: &DiagnosticsPluginConfig, report: &mut DiagnosticReport) {
    command_exists_check(config, report, "df").await;
    if command_path(config, "df").await.is_some() {
        let output = run_command(config, "df", &["-h"], 3).await;
        if !output.stdout.trim().is_empty() {
            report.logs.push(LogExcerpt {
                source: "df -h".to_string(),
                message: clip(&output.stdout, 2_000),
            });
        }
    }
}

async fn command_exists_check(
    config: &DiagnosticsPluginConfig,
    report: &mut DiagnosticReport,
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
    report: &mut DiagnosticReport,
    name: &str,
) -> Vec<u32> {
    let output = run_command(config, "pgrep", &["-af", name], 2).await;
    let matches = filtered_process_matches(&output.stdout, name);
    let found = output.status == Some(0) && !matches.is_empty();
    report.checks.push(Check {
        id: format!("process.{name}.running"),
        status: if found {
            CheckStatus::Ok
        } else {
            CheckStatus::Unknown
        },
        detail: if found {
            format!("process matching {name} is running")
        } else {
            format!("no process matching {name} was found")
        },
        evidence: if found {
            vec![clip(&matches.join("\n"), 1_000)]
        } else {
            Vec::new()
        },
    });
    matches
        .iter()
        .filter_map(|line| line.split_whitespace().next()?.parse::<u32>().ok())
        .collect()
}

fn filtered_process_matches(output: &str, name: &str) -> Vec<String> {
    let mut matches = output
        .lines()
        .filter(|line| {
            let lower = line.to_ascii_lowercase();
            lower.contains(&name.to_ascii_lowercase())
                && !lower.contains("pgrep -af")
                && !lower.contains("/usr/bin/bash -c")
                && !lower.contains("/bin/sh -c")
                && !line_starts_with_pid(line, std::process::id())
        })
        .map(ToString::to_string)
        .collect::<Vec<_>>();
    matches.sort_by_key(|line| std::cmp::Reverse(process_match_score(line, name)));
    matches
}

fn line_starts_with_pid(line: &str, pid: u32) -> bool {
    line.split_whitespace()
        .next()
        .and_then(|value| value.parse::<u32>().ok())
        == Some(pid)
}

async fn launch_probe_target(
    args: &DiagnoseArgs,
    config: &DiagnosticsPluginConfig,
    report: &mut DiagnosticReport,
    target: &str,
) -> Vec<u32> {
    if !safe_command_name(target) {
        report.checks.push(Check {
            id: "input_method.launch_probe".to_string(),
            status: CheckStatus::Warn,
            detail: "launch probe skipped because target command name is not safe".to_string(),
            evidence: vec![target.to_string()],
        });
        return Vec::new();
    }
    let before = process_ids(config, target).await;
    let spawn = Command::new(target)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    let Ok(child) = spawn else {
        report.checks.push(Check {
            id: "input_method.launch_probe".to_string(),
            status: CheckStatus::Warn,
            detail: format!("failed to launch {target} for runtime sampling"),
            evidence: vec![spawn.err().map(|err| err.to_string()).unwrap_or_default()],
        });
        return Vec::new();
    };
    let child_pid = child.id().unwrap_or_default();
    tokio::time::sleep(Duration::from_secs(args.launch_timeout_seconds)).await;
    let after = process_ids(config, target).await;
    let new_pids = after
        .iter()
        .copied()
        .filter(|pid| !before.contains(pid))
        .collect::<Vec<_>>();
    report.facts.insert(
        "input_method.launch_probe".to_string(),
        json!({
            "target": target,
            "launched_pid": child_pid,
            "waited_seconds": args.launch_timeout_seconds,
            "pids_before": before,
            "pids_after": after,
            "new_pids": new_pids,
            "note": "explicit allow_launch_probe=true; existing user processes were not killed",
        }),
    );
    report.checks.push(Check {
        id: "input_method.launch_probe".to_string(),
        status: if new_pids.is_empty() {
            CheckStatus::Unknown
        } else {
            CheckStatus::Ok
        },
        detail: format!("launched {target} for runtime input-method sampling"),
        evidence: vec![format!("new_pids={new_pids:?}")],
    });
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

fn process_match_score(line: &str, name: &str) -> usize {
    let lower = line.to_ascii_lowercase();
    let name = name.to_ascii_lowercase();
    let mut score = 0usize;
    if lower.contains(&format!("/{name} ")) || lower.ends_with(&format!("/{name}")) {
        score += 100;
    }
    if lower.contains(&format!(" {name} ")) || lower.ends_with(&format!(" {name}")) {
        score += 50;
    }
    if lower.contains("--type=zygote") || lower.contains("--type=renderer") {
        score = score.saturating_sub(30);
    }
    if lower.contains("clipsync") || lower.contains("helper") {
        score = score.saturating_sub(20);
    }
    if lower.contains("/tmp/.mount_") {
        score = score.saturating_sub(10);
    }
    score
}

fn linux_app_input_env(report: &mut DiagnosticReport, target: &str, pids: &[u32]) {
    let Some(pid) = pids.first() else {
        return;
    };
    let path = format!("/proc/{pid}/environ");
    let Ok(raw) = std::fs::read(&path) else {
        report.checks.push(Check {
            id: "input_method.app_env".to_string(),
            status: CheckStatus::Unknown,
            detail: format!("could not read environment for {target} pid {pid}"),
            evidence: Vec::new(),
        });
        return;
    };
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
                | "XMODIFIERS"
                | "SDL_IM_MODULE"
                | "GLFW_IM_MODULE"
                | "XDG_SESSION_TYPE"
                | "WAYLAND_DISPLAY"
                | "DISPLAY"
                | "LANG"
                | "LC_ALL"
                | "LC_CTYPE"
                | "LC_MESSAGES"
        ) {
            picked.insert(key.to_string(), redact(value));
        }
    }
    let qt_ok = picked.get("QT_IM_MODULE").map(String::as_str) == Some("fcitx");
    report
        .facts
        .insert("input_method.app_env".to_string(), json!(picked));
    report.checks.push(Check {
        id: "input_method.app_env_qt_im_module".to_string(),
        status: if qt_ok {
            CheckStatus::Ok
        } else {
            CheckStatus::Warn
        },
        detail: format!("checked input method environment for {target} pid {pid}"),
        evidence: vec![format!(
            "QT_IM_MODULE={}",
            if qt_ok {
                "fcitx"
            } else {
                "missing-or-different"
            }
        )],
    });
}

async fn linux_input_method_app_profile(
    config: &DiagnosticsPluginConfig,
    report: &mut DiagnosticReport,
    target: &str,
    pids: &[u32],
    loaded_input_modules: Vec<String>,
) -> Option<InputMethodAppProfile> {
    let command_line = pids.first().and_then(|pid| read_proc_cmdline(*pid));
    let desktop_exec = linux_desktop_exec_for_target(target);
    let command_path = command_path(config, target).await;
    if let Some(path) = command_path.as_deref() {
        report
            .facts
            .insert("input_method.app_command_path".to_string(), json!(path));
    }
    let wrapper_probe = command_path.as_deref().and_then(read_wrapper_probe);
    if let Some(probe) = &wrapper_probe {
        report
            .facts
            .insert("input_method.app_wrapper_probe".to_string(), json!(probe));
    }
    let package_probe = command_path
        .as_deref()
        .and_then(|path| package_probe_for_command(config, path, target));
    if let Some(probe) = &package_probe {
        report
            .facts
            .insert("input_method.app_package_probe".to_string(), json!(probe));
    }
    let flatpak_probe = flatpak_probe_for_target(target, command_path.as_deref());
    if let Some(probe) = &flatpak_probe {
        report
            .facts
            .insert("input_method.app_flatpak_probe".to_string(), json!(probe));
    }
    let combined = [
        target.to_string(),
        command_line.clone().unwrap_or_default(),
        desktop_exec.clone().unwrap_or_default(),
        command_path.clone().unwrap_or_default(),
        wrapper_probe.clone().unwrap_or_default(),
        package_probe.clone().unwrap_or_default(),
        flatpak_probe.clone().unwrap_or_default(),
    ]
    .join(" ");
    let mut profile = InputMethodAppProfile {
        toolkit: infer_input_toolkit(&combined, command_path.as_deref()),
        display_backend: infer_display_backend(&combined),
        runtime_observed: !pids.is_empty() && command_line.is_some(),
        loaded_input_modules,
        evidence_text: combined.clone(),
        electron_uses_wayland: electron_uses_wayland(&combined),
        electron_wayland_ime: electron_wayland_ime_enabled(&combined),
        command_line,
        desktop_exec,
    };
    if profile.display_backend == "unknown" {
        profile.display_backend = infer_display_backend_from_env(report);
    }
    report
        .facts
        .insert("input_method.app_profile".to_string(), json!(&profile));
    Some(profile)
}

fn linux_input_method_profile_checks(
    report: &mut DiagnosticReport,
    target: &str,
    profile: Option<&InputMethodAppProfile>,
) {
    let Some(profile) = profile else {
        report
            .missing_evidence
            .push("target process profile was unavailable".to_string());
        return;
    };
    report.checks.push(Check {
        id: "input_method.app_toolkit".to_string(),
        status: if profile.toolkit == InputToolkit::Unknown {
            CheckStatus::Unknown
        } else {
            CheckStatus::Ok
        },
        detail: format!("inferred toolkit for {target}"),
        evidence: vec![format!("{:?}", profile.toolkit)],
    });
    if !profile.runtime_observed {
        report.hypotheses.push(Hypothesis {
            title: format!("{target} input method path cannot be confirmed without runtime evidence"),
            rationale: "A desktop file or command name can suggest how the app might start, but input method diagnosis depends on the actual running process backend, environment, and loaded modules.".to_string(),
            required_evidence: vec![
                format!("start {target} and rerun input_method diagnostics"),
                format!("read /proc/<{target}-pid>/environ"),
                format!("read /proc/<{target}-pid>/cmdline"),
                "confirm whether the focused window is native Wayland or XWayland".to_string(),
            ],
        });
        return;
    }
    if profile.toolkit == InputToolkit::ElectronChromium {
        let status = if profile.display_backend == "wayland" {
            if profile.electron_wayland_ime {
                CheckStatus::Ok
            } else {
                CheckStatus::Warn
            }
        } else {
            CheckStatus::Unknown
        };
        report.checks.push(Check {
            id: "input_method.electron_ozone_wayland_ime".to_string(),
            status,
            detail: "checked Electron/Chromium Wayland input method flags".to_string(),
            evidence: vec![format!(
                "backend={}, ozone_wayland={}, wayland_ime={}",
                profile.display_backend,
                profile.electron_uses_wayland,
                profile.electron_wayland_ime
            )],
        });
        if profile.display_backend != "wayland" {
            report.hypotheses.push(Hypothesis {
                title: "Electron/Chromium may be using the traditional module path".to_string(),
                rationale: "Runtime evidence did not show a native Wayland backend. Official Fcitx Wayland guidance treats XWayland Electron/Chromium as a traditional X11-style path, where GTK input modules and XIM may both be relevant depending on the concrete app build/runtime.".to_string(),
                required_evidence: vec![
                    "confirm backend with focused-window tooling or process environment".to_string(),
                    "confirm GTK_IM_MODULE and XMODIFIERS in the target process environment".to_string(),
                    "confirm LANG/LC_CTYPE is valid for XIM when relying on XMODIFIERS".to_string(),
                    "confirm the app actually uses Electron/Chromium from package/runtime evidence".to_string(),
                ],
            });
        } else if !profile.electron_wayland_ime {
            report.hypotheses.push(Hypothesis {
                title: "Electron/Chromium Wayland text-input path may be incomplete".to_string(),
                rationale: "The app appears to use Wayland, but command line evidence does not include Wayland IM flags.".to_string(),
                required_evidence: vec![
                    "confirm the app supports Wayland text-input".to_string(),
                    "confirm compositor input-method/text-input protocol support".to_string(),
                    "test with --enable-wayland-ime or documented app-specific flags".to_string(),
                ],
            });
        }
    }
}

fn linux_input_method_env_checks(report: &mut DiagnosticReport) {
    let session = std::env::var("XDG_SESSION_TYPE").unwrap_or_default();
    let desktop = std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default();
    let xmodifiers = std::env::var("XMODIFIERS").unwrap_or_default();
    let qt_im_module = std::env::var("QT_IM_MODULE").unwrap_or_default();
    let qt_im_modules = std::env::var("QT_IM_MODULES").unwrap_or_default();
    let gtk_im_module = std::env::var("GTK_IM_MODULE").unwrap_or_default();
    let wayland =
        session.eq_ignore_ascii_case("wayland") || std::env::var_os("WAYLAND_DISPLAY").is_some();

    report.checks.push(Check {
        id: "input_method.xmodifiers".to_string(),
        status: if xmodifiers == "@im=fcitx" {
            CheckStatus::Ok
        } else {
            CheckStatus::Warn
        },
        detail: "checked XMODIFIERS for X11/XWayland applications".to_string(),
        evidence: vec![format!(
            "XMODIFIERS={}",
            if xmodifiers.is_empty() {
                "<unset>"
            } else {
                &xmodifiers
            }
        )],
    });
    if xmodifiers != "@im=fcitx" {
        report.findings.push(Finding {
            severity: Severity::Medium,
            title: "XMODIFIERS is not set for fcitx".to_string(),
            evidence: "X11 and XWayland applications usually need XMODIFIERS=@im=fcitx."
                .to_string(),
        });
    }
    if wayland && gtk_im_module == "fcitx" {
        report.findings.push(Finding {
            severity: Severity::Medium,
            title: "GTK_IM_MODULE is globally forced to fcitx in a Wayland session".to_string(),
            evidence: "Fcitx recommends leaving GTK_IM_MODULE unset for modern GTK3/4 Wayland apps and using GTK config files or per-app overrides for legacy/XWayland apps.".to_string(),
        });
    }
    let desktop_lower = desktop.to_ascii_lowercase();
    let qt_has_fallback = qt_im_modules
        .split(';')
        .map(str::trim)
        .any(|item| item == "fcitx" || item == "ibus");
    if wayland && !desktop_lower.contains("kde") && qt_im_module != "fcitx" && !qt_has_fallback {
        report.findings.push(Finding {
            severity: Severity::Medium,
            title: "Qt input method fallback is not configured for this Wayland desktop".to_string(),
            evidence: "On GNOME/wlroots-style compositors, Qt5 and some Qt6 apps often need QT_IM_MODULE=fcitx or QT_IM_MODULES=\"wayland;fcitx;ibus\".".to_string(),
        });
    }
}

fn linux_input_method_path_report(
    report: &mut DiagnosticReport,
    profile: Option<&InputMethodAppProfile>,
) {
    let questions = input_method_questions(report, profile);
    let app_adaptation = app_adaptation_verdict(profile);
    let environment_module_path = environment_module_verdict(report, profile);
    let wayland_protocol_path = wayland_protocol_verdict(report, profile);
    report
        .facts
        .insert("input_method.questions".to_string(), json!(questions));
    report.facts.insert(
        "input_method.path_report".to_string(),
        json!(InputMethodPathReport {
            core_model: "Input path requires app adapter + module/protocol availability + matching target environment. Runtime-loaded modules are strong evidence, but a configured path can be enough when all links are verified.".to_string(),
            app_adaptation,
            environment_module_path,
            wayland_protocol_path,
            decision_rule: "Diagnose by proving or disproving the specific path, not by app name. If no path is complete, name the missing link and the evidence needed to verify it.".to_string(),
        }),
    );
}

fn input_method_questions(
    report: &DiagnosticReport,
    profile: Option<&InputMethodAppProfile>,
) -> InputMethodQuestions {
    InputMethodQuestions {
        backend_question: backend_question(report, profile),
        toolkit_question: toolkit_question(profile),
        module_question: module_question(report, profile),
        environment_question: environment_question(report, profile),
        answer_rule: "Answer these facts before diagnosing. If any answer is unknown, keep the final diagnosis uncertain and collect more evidence. Do not turn unknown modules into possible modules.".to_string(),
    }
}

fn backend_question(
    report: &DiagnosticReport,
    profile: Option<&InputMethodAppProfile>,
) -> BackendQuestion {
    let mut evidence = Vec::new();
    let mut missing_evidence = Vec::new();
    let backend = profile
        .map(|item| item.display_backend.clone())
        .unwrap_or_else(|| "unknown".to_string());
    if let Some(profile) = profile {
        if let Some(command_line) = &profile.command_line {
            evidence.push(format!("cmdline={command_line}"));
        }
        if let Some(app_env) = report.facts.get("input_method.app_env") {
            let has_wayland_display = app_env.get("WAYLAND_DISPLAY").is_some();
            let has_display = app_env.get("DISPLAY").is_some();
            if has_wayland_display {
                evidence.push("target_process_env has WAYLAND_DISPLAY".to_string());
            }
            if has_display {
                evidence.push("target_process_env has DISPLAY".to_string());
            }
            if has_wayland_display && has_display {
                missing_evidence.push("window-level backend evidence; WAYLAND_DISPLAY+DISPLAY only proves mixed session environment, not native Wayland".to_string());
            }
        }
        if !profile.runtime_observed {
            missing_evidence.push("runtime process backend evidence".to_string());
        }
    } else {
        missing_evidence.push("target app profile".to_string());
    }
    let text_input_support = match profile.map(|item| item.toolkit) {
        Some(InputToolkit::Gtk)
            if backend == "wayland" && profile.is_some_and(|item| item.runtime_observed) =>
        {
            FactStatus::Confirmed
        }
        _ => FactStatus::Unknown,
    };
    if backend == "wayland" && text_input_support == FactStatus::Unknown {
        missing_evidence.push("fact evidence that the app supports Wayland text-input".to_string());
    }
    BackendQuestion {
        backend,
        text_input_support,
        evidence,
        missing_evidence,
    }
}

fn toolkit_question(profile: Option<&InputMethodAppProfile>) -> ToolkitQuestion {
    let Some(profile) = profile else {
        return ToolkitQuestion {
            toolkit: "unknown".to_string(),
            status: FactStatus::Unknown,
            evidence: Vec::new(),
            missing_evidence: vec!["runtime/package evidence for toolkit framework".to_string()],
        };
    };
    let mut evidence = Vec::new();
    if let Some(command_line) = &profile.command_line {
        evidence.push(format!("cmdline={command_line}"));
    }
    if let Some(exec) = &profile.desktop_exec {
        evidence.push(format!("desktop_exec={exec}"));
    }
    if !profile.evidence_text.trim().is_empty() {
        evidence.push(format!(
            "evidence_text={}",
            clip(&profile.evidence_text, 1_200)
        ));
    }
    let mut missing_evidence = Vec::new();
    let status = if profile.toolkit == InputToolkit::Unknown {
        missing_evidence.push("toolkit/framework evidence".to_string());
        FactStatus::Unknown
    } else if profile.runtime_observed {
        FactStatus::Confirmed
    } else {
        missing_evidence.push("runtime confirmation for toolkit/framework".to_string());
        FactStatus::Unknown
    };
    ToolkitQuestion {
        toolkit: format!("{:?}", profile.toolkit),
        status,
        evidence,
        missing_evidence,
    }
}

fn module_question(
    report: &DiagnosticReport,
    profile: Option<&InputMethodAppProfile>,
) -> ModuleQuestion {
    let gtk_cache = gtk_cache_entries_from_report(report);
    let environment = environment_question(report, profile);
    let requested = environment
        .target_process_env
        .as_ref()
        .and_then(|env| env.get("GTK_IM_MODULE"))
        .cloned()
        .or_else(|| environment.desktop_exec_env.get("GTK_IM_MODULE").cloned())
        .or_else(|| environment.current_shell_env.get("GTK_IM_MODULE").cloned());
    let locale = environment
        .target_process_env
        .as_ref()
        .and_then(locale_from_env)
        .or_else(|| locale_from_env(&environment.desktop_exec_env))
        .or_else(|| locale_from_env(&environment.current_shell_env));
    let gtk_requested_module = requested
        .as_deref()
        .map(|requested| gtk_requested_module_report(requested, &gtk_cache));
    let gtk_locale_selected_modules = locale
        .as_deref()
        .map(|locale| gtk_locale_selected_modules(locale, &gtk_cache))
        .unwrap_or_default();
    let mut confirmed_modules = Vec::new();
    let unsupported_modules = Vec::new();
    let mut unknown_modules = Vec::new();
    for module in [
        "gtk-im-module",
        "qt-im-module",
        "xim",
        "sdl-im-module",
        "wayland-text-input",
    ] {
        let fact = input_module_fact(report, profile, module);
        match fact.status {
            FactStatus::Confirmed => confirmed_modules.push(fact),
            FactStatus::Unknown => unknown_modules.push(fact),
        }
    }
    ModuleQuestion {
        confirmed_modules,
        unsupported_modules,
        unknown_modules,
        gtk_immodule_cache: gtk_cache,
        gtk_requested_module,
        gtk_locale_selected_modules,
        rule: "No possible/likely module is allowed without concrete evidence. Loaded modules prove runtime use; environment variables only prove activation requests. A configured module path can be confirmed when app toolkit, module availability, and target environment all line up.".to_string(),
    }
}

fn input_module_fact(
    report: &DiagnosticReport,
    profile: Option<&InputMethodAppProfile>,
    module: &str,
) -> InputModuleFact {
    let mut evidence = Vec::new();
    let mut missing_evidence = Vec::new();
    let activation_conditions = module_activation_conditions(module);
    let toolkit = profile
        .map(|item| item.toolkit)
        .unwrap_or(InputToolkit::Unknown);
    let runtime_observed = profile.map(|item| item.runtime_observed).unwrap_or(false);
    let loaded_modules = profile
        .map(|item| item.loaded_input_modules.as_slice())
        .unwrap_or(&[]);
    let app_env = report.facts.get("input_method.app_env");
    let has_env = |key: &str, expected: &str| {
        app_env
            .and_then(|value| value.get(key))
            .and_then(Value::as_str)
            .map(|value| value == expected || value.split(';').any(|item| item.trim() == expected))
            .unwrap_or(false)
    };
    let status = match module {
        "gtk-im-module"
            if toolkit == InputToolkit::Gtk || toolkit == InputToolkit::ElectronChromium =>
        {
            if let Some(loaded) = loaded_module_evidence(
                loaded_modules,
                &[
                    "/immodules/",
                    "im-fcitx",
                    "im-xim",
                    "im-ibus",
                    "im-wayland",
                    "im-cedilla",
                ],
            ) {
                evidence.push(loaded);
                if has_env("GTK_IM_MODULE", "fcitx") {
                    evidence.push(
                        "target_process_env GTK_IM_MODULE=fcitx activation request".to_string(),
                    );
                }
                FactStatus::Confirmed
            } else if has_env("GTK_IM_MODULE", "fcitx")
                && gtk_module_available(report, &["fcitx", "im-fcitx"])
            {
                evidence
                    .push("target_process_env GTK_IM_MODULE=fcitx activation request".to_string());
                evidence.push("GTK fcitx module is available from immodule cache or module files (path configured; not runtime-loaded evidence)".to_string());
                FactStatus::Confirmed
            } else {
                if !runtime_observed {
                    missing_evidence.push("runtime process evidence".to_string());
                }
                if has_env("GTK_IM_MODULE", "fcitx") {
                    evidence.push("target_process_env GTK_IM_MODULE=fcitx activation request (not module-load evidence)".to_string());
                }
                missing_evidence.push("loaded GTK input module evidence from /proc/<pid>/maps, e.g. im-fcitx.so/im-xim.so/im-ibus.so/im-wayland.so/im-cedilla.so".to_string());
                missing_evidence.push(
                    "GTK immodules locale activation evidence (LANG/LC_CTYPE and immodules cache/rules)"
                        .to_string(),
                );
                FactStatus::Unknown
            }
        }
        "qt-im-module" if toolkit == InputToolkit::Qt => {
            if let Some(loaded) = loaded_module_evidence(
                loaded_modules,
                &["platforminputcontext", "libfcitx", "libibus"],
            ) {
                evidence.push(loaded);
                if has_env("QT_IM_MODULE", "fcitx") || has_env("QT_IM_MODULES", "fcitx") {
                    evidence.push("target_process_env has Qt fcitx activation request".to_string());
                }
                FactStatus::Confirmed
            } else if (has_env("QT_IM_MODULE", "fcitx") || has_env("QT_IM_MODULES", "fcitx"))
                && available_module_evidence(report, &["platforminputcontext", "fcitx"]).is_some()
            {
                evidence.push("target_process_env has Qt fcitx activation request".to_string());
                evidence.push("Qt fcitx platform input context appears available (path configured; not runtime-loaded evidence)".to_string());
                FactStatus::Confirmed
            } else {
                if !runtime_observed {
                    missing_evidence.push("runtime process evidence".to_string());
                }
                missing_evidence.push("loaded Qt platforminputcontext/libfcitx/libibus evidence from /proc/<pid>/maps".to_string());
                FactStatus::Unknown
            }
        }
        "xim" => {
            if let Some(loaded) =
                loaded_module_evidence(loaded_modules, &["im-xim", "libx11", "libxim"])
            {
                evidence.push(loaded);
                if has_env("XMODIFIERS", "@im=fcitx") {
                    evidence.push(
                        "target_process_env XMODIFIERS=@im=fcitx activation request".to_string(),
                    );
                }
                FactStatus::Confirmed
            } else if has_env("XMODIFIERS", "@im=fcitx") && xim_path_configured(report, profile) {
                evidence
                    .push("target_process_env XMODIFIERS=@im=fcitx activation request".to_string());
                evidence.push("X11/XWayland-compatible backend plus valid locale evidence (XIM path configured; not runtime-loaded evidence)".to_string());
                FactStatus::Confirmed
            } else {
                if !runtime_observed {
                    missing_evidence.push("runtime process evidence".to_string());
                }
                if has_env("XMODIFIERS", "@im=fcitx") {
                    evidence.push("target_process_env XMODIFIERS=@im=fcitx activation request (not module-load evidence)".to_string());
                }
                missing_evidence.push("loaded XIM evidence from /proc/<pid>/maps, e.g. im-xim.so or X11/XIM library evidence".to_string());
                FactStatus::Unknown
            }
        }
        "sdl-im-module" if toolkit == InputToolkit::Sdl => {
            if let Some(loaded) =
                loaded_module_evidence(loaded_modules, &["libfcitx", "libibus", "sdl"])
            {
                evidence.push(loaded);
                if has_env("SDL_IM_MODULE", "fcitx") {
                    evidence.push(
                        "target_process_env SDL_IM_MODULE=fcitx activation request".to_string(),
                    );
                }
                FactStatus::Confirmed
            } else {
                if !runtime_observed {
                    missing_evidence.push("runtime process evidence".to_string());
                }
                missing_evidence.push(
                    "loaded SDL input method bridge evidence from /proc/<pid>/maps".to_string(),
                );
                FactStatus::Unknown
            }
        }
        "wayland-text-input" => {
            if runtime_observed
                && profile.is_some_and(|item| item.display_backend == "wayland")
                && toolkit == InputToolkit::Gtk
            {
                evidence.push("runtime backend is wayland and toolkit is GTK".to_string());
                FactStatus::Confirmed
            } else {
                missing_evidence.push(
                    "runtime Wayland backend plus app text-input support evidence".to_string(),
                );
                FactStatus::Unknown
            }
        }
        _ => {
            missing_evidence.push("toolkit/module adapter evidence".to_string());
            FactStatus::Unknown
        }
    };
    InputModuleFact {
        module: module.to_string(),
        status,
        evidence,
        activation_conditions,
        missing_evidence,
    }
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

fn gtk_module_available(report: &DiagnosticReport, needles: &[&str]) -> bool {
    let cache_match = gtk_cache_entries_from_report(report).iter().any(|entry| {
        let haystack = format!("{} {}", entry.module, entry.path).to_ascii_lowercase();
        needles
            .iter()
            .any(|needle| haystack.contains(&needle.to_ascii_lowercase()))
    });
    cache_match || available_module_evidence(report, needles).is_some()
}

fn available_module_evidence(report: &DiagnosticReport, needles: &[&str]) -> Option<String> {
    let available = report
        .facts
        .get("input_method.available_modules")
        .and_then(Value::as_array)
        .and_then(|items| {
            items.iter().filter_map(Value::as_str).find_map(|item| {
                let lower = item.to_ascii_lowercase();
                needles
                    .iter()
                    .any(|needle| lower.contains(&needle.to_ascii_lowercase()))
                    .then(|| item.to_string())
            })
        });
    available.or_else(|| {
        report
            .facts
            .get("input_method.app_flatpak_probe")
            .and_then(Value::as_str)
            .and_then(|probe| {
                probe.lines().find_map(|line| {
                    let lower = line.to_ascii_lowercase();
                    needles
                        .iter()
                        .any(|needle| lower.contains(&needle.to_ascii_lowercase()))
                        .then(|| line.to_string())
                })
            })
    })
}

fn xim_path_configured(report: &DiagnosticReport, profile: Option<&InputMethodAppProfile>) -> bool {
    let backend_ok = profile
        .map(|item| {
            item.display_backend == "x11_or_xwayland" || item.display_backend == "unknown_mixed_env"
        })
        .unwrap_or(false);
    backend_ok && target_locale_supports_xim(report)
}

fn target_locale_supports_xim(report: &DiagnosticReport) -> bool {
    report
        .facts
        .get("input_method.app_env")
        .and_then(Value::as_object)
        .and_then(|env| {
            ["LC_ALL", "LC_CTYPE", "LANG"].iter().find_map(|key| {
                env.get(*key)
                    .and_then(Value::as_str)
                    .filter(|value| !value.trim().is_empty())
            })
        })
        .map(|locale| {
            let locale = locale.to_ascii_lowercase();
            locale != "c" && locale != "posix"
        })
        .unwrap_or(false)
}

fn module_activation_conditions(module: &str) -> Vec<String> {
    match module {
        "gtk-im-module" => vec![
            "GTK app or Chromium/Electron XWayland GTK path".to_string(),
            "GTK_IM_MODULE=fcitx or GTK settings/XSettings select fcitx".to_string(),
            "If GTK_IM_MODULE is unset, GTK may select an input method module from immodules rules using locale categories such as LC_CTYPE/LANG; this must be verified from runtime locale and module cache/documentation before concluding".to_string(),
        ],
        "qt-im-module" => vec![
            "Qt app with matching platform input context plugin".to_string(),
            "QT_IM_MODULE=fcitx or QT_IM_MODULES includes fcitx".to_string(),
        ],
        "xim" => vec![
            "X11/XWayland/Xlib-compatible input path; Fcitx Wiki says X11 apps under XWayland are nearly the same as normal X11 for input-method setup".to_string(),
            "XMODIFIERS=@im=fcitx".to_string(),
            "LANG/LC_CTYPE is a valid installed locale and is not C/POSIX".to_string(),
            "Concrete app/toolkit evidence or behavior test shows it can call XIM; do not deny XIM only because the app is Electron/Chromium/AppImage".to_string(),
        ],
        "sdl-im-module" => vec![
            "SDL app with SDL IM module support".to_string(),
            "SDL_IM_MODULE=fcitx".to_string(),
        ],
        "wayland-text-input" => vec![
            "native Wayland app supports text-input".to_string(),
            "compositor supports text-input and input-method bridge".to_string(),
            "input method framework is connected through input-method path".to_string(),
        ],
        _ => Vec::new(),
    }
}

fn environment_question(
    report: &DiagnosticReport,
    profile: Option<&InputMethodAppProfile>,
) -> EnvironmentQuestion {
    let keys = [
        "GTK_IM_MODULE",
        "QT_IM_MODULE",
        "QT_IM_MODULES",
        "XMODIFIERS",
        "SDL_IM_MODULE",
        "GLFW_IM_MODULE",
        "WAYLAND_DISPLAY",
        "DISPLAY",
        "XDG_SESSION_TYPE",
        "LANG",
        "LC_ALL",
        "LC_CTYPE",
        "LC_MESSAGES",
    ];
    let mut current_shell_env = BTreeMap::new();
    for key in keys {
        if let Ok(value) = std::env::var(key) {
            if !value.trim().is_empty() {
                current_shell_env.insert(key.to_string(), redact(&value));
            }
        }
    }
    let mut desktop_exec_env = BTreeMap::new();
    if let Some(exec) = profile.and_then(|item| item.desktop_exec.as_deref()) {
        desktop_exec_env = parse_env_assignments(exec);
    }
    let target_process_env = report
        .facts
        .get("input_method.app_env")
        .and_then(Value::as_object)
        .map(|map| {
            map.iter()
                .filter_map(|(key, value)| {
                    value.as_str().map(|value| (key.clone(), value.to_string()))
                })
                .collect::<BTreeMap<_, _>>()
        });
    let mut missing_evidence = Vec::new();
    if target_process_env.is_none() {
        missing_evidence.push("target process environment".to_string());
    }
    EnvironmentQuestion {
        current_shell_env,
        desktop_exec_env,
        target_process_env,
        missing_evidence,
    }
}

fn parse_env_assignments(exec: &str) -> BTreeMap<String, String> {
    let mut values = BTreeMap::new();
    let mut saw_env = false;
    for part in exec.split_whitespace() {
        if part == "env" {
            saw_env = true;
            continue;
        }
        if !saw_env {
            continue;
        }
        let Some((key, value)) = part.split_once('=') else {
            break;
        };
        if key.chars().all(|ch| ch.is_ascii_uppercase() || ch == '_') {
            values.insert(key.to_string(), redact(value));
        }
    }
    values
}

fn app_adaptation_verdict(profile: Option<&InputMethodAppProfile>) -> PathVerdict {
    let Some(profile) = profile else {
        return PathVerdict {
            status: "unknown".to_string(),
            evidence: Vec::new(),
            missing: vec!["target app profile".to_string()],
        };
    };
    let mut evidence = vec![format!("toolkit={:?}", profile.toolkit)];
    if let Some(command_line) = &profile.command_line {
        evidence.push(format!("cmdline={command_line}"));
    }
    if let Some(exec) = &profile.desktop_exec {
        evidence.push(format!("desktop_exec={exec}"));
    }
    let mut missing = Vec::new();
    if !profile.runtime_observed {
        missing.push("runtime process evidence".to_string());
    }
    if profile.toolkit == InputToolkit::Unknown {
        missing.push("toolkit/input module adapter evidence".to_string());
    }
    PathVerdict {
        status: if missing.is_empty() {
            "supported_or_likely"
        } else {
            "unknown"
        }
        .to_string(),
        evidence,
        missing,
    }
}

fn environment_module_verdict(
    report: &DiagnosticReport,
    profile: Option<&InputMethodAppProfile>,
) -> PathVerdict {
    let app_env = report.facts.get("input_method.app_env");
    let mut evidence = Vec::new();
    let mut missing = Vec::new();
    for key in [
        "GTK_IM_MODULE",
        "QT_IM_MODULE",
        "QT_IM_MODULES",
        "XMODIFIERS",
        "SDL_IM_MODULE",
        "GLFW_IM_MODULE",
    ] {
        let process_value = app_env
            .and_then(|value| value.get(key))
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty());
        if let Some(value) = process_value {
            evidence.push(format!("{key}={value}"));
        } else if let Ok(value) = std::env::var(key) {
            if !value.trim().is_empty() {
                evidence.push(format!("current_shell:{key}={value}"));
            }
        }
    }
    if app_env.is_none() {
        missing.push("target process environment".to_string());
    }
    if profile.is_some_and(|item| item.toolkit == InputToolkit::Unknown) {
        missing.push("which input method module the app can load".to_string());
    }
    let has_fcitx_env = evidence
        .iter()
        .any(|item| item.contains("fcitx") || item.contains("@im=fcitx"));
    PathVerdict {
        status: if has_fcitx_env && missing.is_empty() {
            "configured"
        } else if has_fcitx_env {
            "partially_configured"
        } else {
            "unknown_or_missing"
        }
        .to_string(),
        evidence,
        missing,
    }
}

fn wayland_protocol_verdict(
    report: &DiagnosticReport,
    profile: Option<&InputMethodAppProfile>,
) -> PathVerdict {
    let mut evidence = Vec::new();
    let mut missing = Vec::new();
    if let Some(profile) = profile {
        evidence.push(format!("app_backend={}", profile.display_backend));
        if profile.display_backend != "wayland" {
            missing.push("native Wayland app backend".to_string());
        }
        if !profile.runtime_observed {
            missing.push("runtime backend evidence".to_string());
        }
        match profile.toolkit {
            InputToolkit::Gtk => {
                evidence.push("GTK3/4 commonly supports text-input-v3".to_string())
            }
            InputToolkit::Qt => missing.push("Qt version and text-input support".to_string()),
            InputToolkit::ElectronChromium => {
                missing.push("Electron/Chromium text-input support and IM flags".to_string())
            }
            InputToolkit::Unknown => missing.push("app text-input support".to_string()),
            _ => missing.push("app text-input support".to_string()),
        }
    } else {
        missing.push("target app profile".to_string());
    }
    if let Some(hint) = report
        .facts
        .get("input_method.compositor_support_hint")
        .and_then(Value::as_str)
    {
        evidence.push(format!("compositor_hint={hint}"));
        if hint.contains("unknown") || hint.contains("verify") || hint.contains("vary") {
            missing
                .push("confirmed compositor text-input/input-method protocol support".to_string());
        }
    } else {
        missing.push("compositor protocol support".to_string());
    }
    PathVerdict {
        status: if missing.is_empty() {
            "possible"
        } else {
            "unconfirmed"
        }
        .to_string(),
        evidence,
        missing,
    }
}

fn linux_input_method_config_facts(report: &mut DiagnosticReport) {
    for (fact, path) in [
        ("input_method.config.etc_environment", "/etc/environment"),
        ("input_method.config.gtk2", "$HOME/.gtkrc-2.0"),
        (
            "input_method.config.gtk3",
            "$HOME/.config/gtk-3.0/settings.ini",
        ),
        (
            "input_method.config.gtk4",
            "$HOME/.config/gtk-4.0/settings.ini",
        ),
    ] {
        if let Some(text) = read_config_probe(path) {
            report.facts.insert(fact.to_string(), json!(redact(&text)));
        }
    }
    if let Some(entries) = read_environment_d_entries() {
        report.facts.insert(
            "input_method.config.environment_d".to_string(),
            json!(entries),
        );
    }
}

async fn linux_wayland_compositor_checks(
    config: &DiagnosticsPluginConfig,
    report: &mut DiagnosticReport,
) {
    for name in [
        "kwin_wayland",
        "gnome-shell",
        "sway",
        "Hyprland",
        "niri",
        "weston",
    ] {
        process_check(config, report, name).await;
    }
    let desktop = std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default();
    let compositor = infer_compositor_from_desktop(&desktop);
    report.facts.insert(
        "input_method.compositor_support_hint".to_string(),
        json!(wayland_compositor_support_hint(&compositor)),
    );
}

fn finalize_input_method_confidence(report: &mut DiagnosticReport) {
    let mut score = input_method_confidence_score(report);
    if report
        .missing_evidence
        .iter()
        .any(|item| item.contains("not running") || item.contains("runtime"))
    {
        score = score.min(70);
    }
    if !report.facts.contains_key("input_method.questions") {
        score = score.min(75);
    }
    let threshold = 95;
    let can_conclude = score >= threshold && report.missing_evidence.is_empty();
    let label = if can_conclude {
        "high"
    } else if score >= 70 {
        "medium"
    } else {
        "low"
    };
    if score < threshold
        && !report
            .missing_evidence
            .iter()
            .any(|item| item.contains("target app"))
    {
        report.missing_evidence.push(
            "more evidence is needed: app adapter/module support, target process environment variables, and Wayland text-input/input-method protocol support".to_string(),
        );
    }
    report.confidence = Some(Confidence {
        score,
        label: label.to_string(),
        threshold,
        can_conclude,
        answer_tone: if can_conclude { "certain" } else { "uncertain" }.to_string(),
        required_language: if can_conclude {
            Vec::new()
        } else {
            vec![
                "不确定".to_string(),
                "可能".to_string(),
                "也许".to_string(),
                "目前只能判断".to_string(),
                "还缺证据".to_string(),
            ]
        },
        forbidden_language: if can_conclude {
            Vec::new()
        } else {
            vec![
                "根因是".to_string(),
                "说明".to_string(),
                "就是".to_string(),
                "确定是".to_string(),
            ]
        },
        reason: "estimated from input method daemon state, session/compositor facts, target app profile, environment variables, packages, and logs".to_string(),
    });
}

fn read_proc_cmdline(pid: u32) -> Option<String> {
    let raw = std::fs::read(format!("/proc/{pid}/cmdline")).ok()?;
    let parts = raw
        .split(|byte| *byte == 0)
        .filter(|part| !part.is_empty())
        .map(|part| redact(&String::from_utf8_lossy(part)))
        .collect::<Vec<_>>();
    (!parts.is_empty()).then(|| parts.join(" "))
}

fn read_loaded_input_modules(pids: &[u32]) -> Vec<String> {
    let mut modules = BTreeMap::new();
    for pid in pids.iter().take(8) {
        let Ok(text) = std::fs::read_to_string(format!("/proc/{pid}/maps")) else {
            continue;
        };
        for line in text.lines() {
            if let Some(path) = input_module_path_from_maps_line(line) {
                modules.insert(path.clone(), format!("pid {pid}: {path}"));
            }
        }
    }
    modules.into_values().take(80).collect()
}

fn input_module_path_from_maps_line(line: &str) -> Option<String> {
    let path = line.split_whitespace().last()?;
    let lower = path.to_ascii_lowercase();
    let is_input_module = lower.contains("/immodules/")
        || lower.contains("im-fcitx")
        || lower.contains("im-xim")
        || lower.contains("im-ibus")
        || lower.contains("im-wayland")
        || lower.contains("im-cedilla")
        || lower.contains("platforminputcontext")
        || lower.contains("libibus")
        || lower.contains("libfcitx");
    if is_input_module && (lower.ends_with(".so") || lower.contains(".so.")) {
        Some(redact(path))
    } else {
        None
    }
}

fn scan_available_input_modules() -> Vec<String> {
    let mut modules = BTreeMap::new();
    for root in ["/usr/lib", "/usr/lib64", "/app/lib"] {
        scan_available_input_modules_under(std::path::Path::new(root), 0, &mut modules);
    }
    modules.into_values().take(120).collect()
}

fn scan_gtk_immodule_cache_entries() -> Vec<GtkImModuleCacheEntry> {
    let mut entries = Vec::new();
    let cache_paths = gtk_immodule_cache_paths();
    for path in cache_paths {
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        entries.extend(parse_gtk_immodule_cache(
            &text,
            &redact(&path.display().to_string()),
        ));
        if entries.len() >= 160 {
            break;
        }
    }
    entries.truncate(160);
    entries
}

fn gtk_immodule_cache_paths() -> Vec<std::path::PathBuf> {
    let mut paths = Vec::new();
    for root in ["/usr/lib", "/usr/lib64", "/app/lib"] {
        collect_gtk_immodule_cache_paths(std::path::Path::new(root), 0, &mut paths);
    }
    paths.truncate(80);
    paths
}

fn collect_gtk_immodule_cache_paths(
    dir: &std::path::Path,
    depth: usize,
    paths: &mut Vec<std::path::PathBuf>,
) {
    if depth > 8 || paths.len() >= 80 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten().take(300) {
        let path = entry.path();
        let lower = path.display().to_string().to_ascii_lowercase();
        if path.is_dir() {
            if lower.contains("gtk") || lower.contains("steam") || lower.contains("runtime") {
                collect_gtk_immodule_cache_paths(&path, depth + 1, paths);
            }
        } else if lower.ends_with("immodules.cache") {
            paths.push(path);
        }
    }
}

fn parse_gtk_immodule_cache(text: &str, source: &str) -> Vec<GtkImModuleCacheEntry> {
    let mut entries = Vec::new();
    let lines = text.lines().collect::<Vec<_>>();
    let mut index = 0usize;
    while index < lines.len() {
        let line = lines[index].trim();
        if !line.starts_with('"') || !line.contains(".so") {
            index += 1;
            continue;
        }
        let path = line.trim_matches('"').to_string();
        let mut metadata_index = index + 1;
        while metadata_index < lines.len() && lines[metadata_index].trim().is_empty() {
            metadata_index += 1;
        }
        if metadata_index >= lines.len() {
            break;
        }
        let metadata = parse_quoted_fields(lines[metadata_index]);
        if metadata.is_empty() {
            index += 1;
            continue;
        }
        let module = metadata.first().cloned().unwrap_or_default();
        let locales = metadata
            .last()
            .map(|value| {
                value
                    .split(':')
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        entries.push(GtkImModuleCacheEntry {
            module,
            path: redact(&path),
            locales,
            source: source.to_string(),
        });
        index = metadata_index + 1;
    }
    entries
}

fn parse_quoted_fields(line: &str) -> Vec<String> {
    let mut fields = Vec::new();
    let mut current = String::new();
    let mut in_quote = false;
    for ch in line.chars() {
        match ch {
            '"' if in_quote => {
                fields.push(current.clone());
                current.clear();
                in_quote = false;
            }
            '"' => in_quote = true,
            _ if in_quote => current.push(ch),
            _ => {}
        }
    }
    fields
}

fn gtk_cache_entries_from_report(report: &DiagnosticReport) -> Vec<GtkImModuleCacheEntry> {
    report
        .facts
        .get("input_method.gtk_immodule_cache")
        .and_then(|value| serde_json::from_value(value.clone()).ok())
        .unwrap_or_default()
}

fn gtk_requested_module_report(
    requested: &str,
    entries: &[GtkImModuleCacheEntry],
) -> GtkRequestedModuleReport {
    let matching_entries = entries
        .iter()
        .filter(|entry| {
            entry.module == requested
                || entry
                    .path
                    .to_ascii_lowercase()
                    .contains(&format!("im-{requested}"))
        })
        .cloned()
        .collect::<Vec<_>>();
    let present_in_cache = !matching_entries.is_empty();
    GtkRequestedModuleReport {
        requested: requested.to_string(),
        present_in_cache,
        evidence: if present_in_cache {
            matching_entries
                .iter()
                .map(|entry| {
                    format!(
                        "{} -> {} locales={}",
                        entry.source,
                        entry.path,
                        entry.locales.join(":")
                    )
                })
                .collect()
        } else {
            vec![format!("requested GTK_IM_MODULE={requested}, but no matching entry was found in collected GTK immodule cache")]
        },
        matching_entries,
    }
}

fn gtk_locale_selected_modules(
    locale: &str,
    entries: &[GtkImModuleCacheEntry],
) -> Vec<GtkImModuleCacheEntry> {
    entries
        .iter()
        .filter(|entry| gtk_entry_matches_locale(entry, locale))
        .cloned()
        .collect()
}

fn gtk_entry_matches_locale(entry: &GtkImModuleCacheEntry, locale: &str) -> bool {
    let locale_lower = locale.to_ascii_lowercase();
    let lang = locale_lower
        .split(['_', '.', '@', '-'])
        .next()
        .unwrap_or_default();
    entry.locales.iter().any(|item| {
        let item = item.to_ascii_lowercase();
        item == "*" || item == locale_lower || item == lang
    })
}

fn locale_from_env(env: &BTreeMap<String, String>) -> Option<String> {
    for key in ["LC_ALL", "LC_CTYPE", "LANG"] {
        if let Some(value) = env.get(key).filter(|value| !value.trim().is_empty()) {
            return Some(value.clone());
        }
    }
    None
}

fn scan_available_input_modules_under(
    dir: &std::path::Path,
    depth: usize,
    modules: &mut BTreeMap<String, String>,
) {
    if depth > 5 || modules.len() >= 120 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten().take(300) {
        let path = entry.path();
        let path_text = path.display().to_string();
        let lower = path_text.to_ascii_lowercase();
        if path.is_dir() {
            if lower.contains("gtk")
                || lower.contains("immodules")
                || lower.contains("qt")
                || lower.contains("fcitx")
                || lower.contains("ibus")
            {
                scan_available_input_modules_under(&path, depth + 1, modules);
            }
            continue;
        }
        if input_module_file_name(&lower) {
            modules.insert(path_text.clone(), redact(&path_text));
        }
    }
}

fn input_module_file_name(lower_path: &str) -> bool {
    (lower_path.contains("/immodules/")
        || lower_path.contains("im-fcitx")
        || lower_path.contains("im-xim")
        || lower_path.contains("im-ibus")
        || lower_path.contains("im-wayland")
        || lower_path.contains("im-cedilla")
        || lower_path.contains("platforminputcontext"))
        && (lower_path.ends_with(".so") || lower_path.contains(".so."))
}

fn read_wrapper_probe(path: &str) -> Option<String> {
    let metadata = std::fs::metadata(path).ok()?;
    if metadata.len() > 512 * 1024 {
        return None;
    }
    let text = std::fs::read_to_string(path).ok()?;
    let picked = text
        .lines()
        .take(120)
        .filter(|line| {
            let lower = line.to_ascii_lowercase();
            lower.contains("exec ")
                || lower.contains("qt_im_module")
                || lower.contains("gtk_im_module")
                || lower.contains("xmodifiers")
                || lower.contains("sdl_im_module")
                || lower.contains("electron")
                || lower.contains("chrome")
                || lower.contains("appimage")
                || lower.contains("flatpak")
        })
        .take(40)
        .collect::<Vec<_>>()
        .join("\n");
    (!picked.trim().is_empty()).then(|| redact(&picked))
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
    let text = lines.join("\n");
    let clipped = clip(&text, config.max_stdout_chars.min(4_000));
    Some(redact(&clipped))
}

fn package_name_from_pacman_owner(text: &str) -> Option<String> {
    let parts = text.split_whitespace().collect::<Vec<_>>();
    if let Some(index) = parts.iter().position(|part| *part == "by") {
        return parts.get(index + 1).map(|value| value.to_string());
    }
    if let Some(index) = parts.iter().position(|part| *part == "由") {
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
        || lower.contains("cef")
        || lower.contains("appimage")
        || lower.ends_with(".desktop")
        || lower.contains("/bin/")
        || lower.contains("/sbin/")
}

fn flatpak_probe_for_target(target: &str, command_path: Option<&str>) -> Option<String> {
    let app_id = flatpak_app_id_for_target(target, command_path)?;
    let app_location = flatpak_location(&app_id)?;
    let metadata = std::fs::read_to_string(format!("{app_location}/metadata")).ok();
    let runtime_ref = metadata
        .as_deref()
        .and_then(|text| metadata_value(text, "Application", "runtime"));
    let runtime_location = runtime_ref.as_deref().and_then(flatpak_runtime_location);
    let info = std::process::Command::new("flatpak")
        .args(["info", &app_id])
        .output()
        .ok()
        .filter(|output| output.status.success())?;
    let info_text = redact(&String::from_utf8_lossy(&info.stdout));
    let picked = info_text
        .lines()
        .filter(|line| {
            let lower = line.to_ascii_lowercase();
            lower.contains("id")
                || lower.contains("标识")
                || lower.contains("runtime")
                || lower.contains("运行时")
                || lower.contains("sdk")
                || lower.contains("branch")
                || lower.contains("分支")
        })
        .take(40)
        .collect::<Vec<_>>()
        .join("\n");
    let permissions = std::process::Command::new("flatpak")
        .args(["info", "--show-permissions", &app_id])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| redact(&String::from_utf8_lossy(&output.stdout)))
        .unwrap_or_default();
    let mut roots = vec![format!("{app_location}/files")];
    if let Some(runtime_location) = &runtime_location {
        roots.push(format!("{runtime_location}/files"));
    }
    let modules = scan_flatpak_input_modules(&roots);
    Some(format!(
        "flatpak_app_id={app_id}\napp_location={}\nruntime_ref={}\nruntime_location={}\n{}\n{}\n{}",
        redact(&app_location),
        runtime_ref.unwrap_or_default(),
        runtime_location
            .map(|path| redact(&path))
            .unwrap_or_default(),
        picked,
        flatpak_permission_probe(&permissions),
        modules.join("\n")
    ))
}

fn flatpak_location(app_id: &str) -> Option<String> {
    let output = std::process::Command::new("flatpak")
        .args(["info", "--show-location", app_id])
        .output()
        .ok()
        .filter(|output| output.status.success())?;
    Some(String::from_utf8_lossy(&output.stdout).trim().to_string())
        .filter(|value| !value.is_empty())
}

fn flatpak_runtime_location(runtime_ref: &str) -> Option<String> {
    let mut parts = runtime_ref.split('/');
    let runtime_id = parts.next()?;
    let _arch = parts.next()?;
    let branch = parts.next()?;
    flatpak_location(&format!("{runtime_id}//{branch}")).or_else(|| flatpak_location(runtime_id))
}

fn metadata_value(text: &str, section: &str, key: &str) -> Option<String> {
    let mut in_section = false;
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            in_section = trimmed == format!("[{section}]");
            continue;
        }
        if in_section {
            let Some((name, value)) = trimmed.split_once('=') else {
                continue;
            };
            if name == key {
                return Some(value.to_string());
            }
        }
    }
    None
}

fn flatpak_permission_probe(text: &str) -> String {
    text.lines()
        .filter(|line| {
            let lower = line.to_ascii_lowercase();
            lower.contains("sockets=")
                || lower.contains("session bus")
                || lower.contains("org.fcitx")
                || lower.contains("environment")
                || lower.contains("qt_im_module")
                || lower.contains("gtk_im_module")
                || lower.contains("xmodifiers")
        })
        .take(80)
        .collect::<Vec<_>>()
        .join("\n")
}

fn scan_flatpak_input_modules(roots: &[String]) -> Vec<String> {
    let mut modules = BTreeMap::new();
    for root in roots {
        scan_flatpak_input_modules_under(std::path::Path::new(root), 0, &mut modules);
    }
    modules.into_values().take(160).collect()
}

fn scan_flatpak_input_modules_under(
    dir: &std::path::Path,
    depth: usize,
    modules: &mut BTreeMap<String, String>,
) {
    if depth > 8 || modules.len() >= 160 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten().take(500) {
        let path = entry.path();
        let text = path.display().to_string();
        let lower = text.to_ascii_lowercase();
        if path.is_dir() {
            if lower.contains("plugin")
                || lower.contains("gtk")
                || lower.contains("qt")
                || lower.contains("lib")
                || lower.contains("immodules")
                || lower.contains("platforminputcontext")
            {
                scan_flatpak_input_modules_under(&path, depth + 1, modules);
            }
            continue;
        }
        if lower.ends_with("immodules.cache")
            || lower.contains("platforminputcontexts")
            || lower.contains("immodules/im-")
            || lower.contains("libfcitx")
            || lower.contains("libibus")
            || lower.contains("libqt") && lower.contains("gui")
            || lower.contains("libgtk")
        {
            modules.insert(text.clone(), format!("flatpak_module={}", redact(&text)));
        }
    }
}

fn flatpak_app_id_for_target(target: &str, command_path: Option<&str>) -> Option<String> {
    if target.contains('.') {
        return Some(target.to_string());
    }
    command_path
        .and_then(|path| path.rsplit('/').next())
        .filter(|name| name.contains('.'))
        .map(ToString::to_string)
        .or_else(|| {
            let output = std::process::Command::new("flatpak")
                .args(["list", "--app", "--columns=application,name"])
                .output()
                .ok()?;
            String::from_utf8_lossy(&output.stdout)
                .lines()
                .find_map(|line| {
                    let lower = line.to_ascii_lowercase();
                    let app_id = line.split_whitespace().next()?;
                    lower
                        .contains(&target.to_ascii_lowercase())
                        .then(|| app_id.to_string())
                })
        })
}

fn linux_desktop_exec_for_target(target: &str) -> Option<String> {
    let mut dirs = Vec::new();
    if let Ok(home) = std::env::var("HOME") {
        dirs.push(format!("{home}/.local/share/applications"));
    }
    dirs.push("/usr/share/applications".to_string());
    for dir in dirs {
        let Ok(entries) = std::fs::read_dir(dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|value| value.to_str()) != Some("desktop") {
                continue;
            }
            let file_stem_matches = path
                .file_stem()
                .and_then(|value| value.to_str())
                .map(|value| value.eq_ignore_ascii_case(target))
                .unwrap_or(false);
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            let exec = desktop_exec_line(&text);
            if file_stem_matches
                || exec
                    .as_deref()
                    .is_some_and(|line| command_mentions_target(line, target))
            {
                return exec.map(|line| redact(&line));
            }
        }
    }
    None
}

fn desktop_exec_line(text: &str) -> Option<String> {
    text.lines()
        .find_map(|line| line.strip_prefix("Exec=").map(ToString::to_string))
}

fn command_mentions_target(line: &str, target: &str) -> bool {
    line.split(|ch: char| ch.is_whitespace() || ch == '/' || ch == '=')
        .any(|part| part == target)
}

fn infer_input_toolkit(text: &str, command_path: Option<&str>) -> InputToolkit {
    let lower = format!("{} {}", text, command_path.unwrap_or_default()).to_ascii_lowercase();
    let qt_env = lower.contains("qt_im_module") || lower.contains("qt_im_modules");
    let qt_platform = lower.contains("org.kde.platform") || lower.contains("platforminputcontext");
    let strong_chromium = lower.contains("electron")
        || lower.contains("chromium")
        || lower.contains("chrome-sandbox")
        || lower.contains("chrome_crashpad")
        || lower.contains("steamwebhelper")
        || lower.contains("google-chrome")
        || lower.contains("--ozone-platform")
        || lower.contains("linuxqq")
        || lower.contains("code ")
        || lower.ends_with(" code");
    if qt_env {
        InputToolkit::Qt
    } else if strong_chromium {
        InputToolkit::ElectronChromium
    } else if qt_platform {
        InputToolkit::Qt
    } else if lower.contains("gtk") || lower.contains("gnome") || lower.contains("libadwaita") {
        InputToolkit::Gtk
    } else if lower.contains("qt") || lower.contains("qpa") || lower.contains("kde") {
        InputToolkit::Qt
    } else if lower.contains("sdl") {
        InputToolkit::Sdl
    } else if lower.contains("java") || lower.contains("jdk") || lower.contains("jre") {
        InputToolkit::Java
    } else if lower.contains("xterm") || lower.contains("rxvt") || lower.contains("xlib") {
        InputToolkit::X11Legacy
    } else {
        InputToolkit::Unknown
    }
}

fn infer_display_backend(text: &str) -> String {
    let lower = text.to_ascii_lowercase();
    if lower.contains("--ozone-platform=wayland")
        || lower.contains("wayland") && !lower.contains("xwayland")
    {
        "wayland".to_string()
    } else if lower.contains("--ozone-platform=x11")
        || lower.contains("xwayland")
        || lower.contains("xcb")
        || lower.contains("x11")
    {
        "x11_or_xwayland".to_string()
    } else {
        "unknown".to_string()
    }
}

fn infer_display_backend_from_env(report: &DiagnosticReport) -> String {
    let app_env = report.facts.get("input_method.app_env");
    let has_wayland = app_env
        .and_then(|value| value.get("WAYLAND_DISPLAY"))
        .and_then(Value::as_str)
        .map(|value| !value.is_empty())
        .unwrap_or(false);
    let has_display = app_env
        .and_then(|value| value.get("DISPLAY"))
        .and_then(Value::as_str)
        .map(|value| !value.is_empty())
        .unwrap_or(false);
    if has_wayland {
        if has_display {
            "unknown_mixed_env".to_string()
        } else {
            "wayland".to_string()
        }
    } else if has_display {
        "x11_or_xwayland".to_string()
    } else {
        "unknown".to_string()
    }
}

fn electron_uses_wayland(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    lower.contains("--ozone-platform=wayland") || lower.contains("wayland")
}

fn electron_wayland_ime_enabled(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    lower.contains("--enable-wayland-ime") || lower.contains("--gtk-version=4")
}

fn read_config_probe(path: &str) -> Option<String> {
    let path = expand_home(path);
    let text = std::fs::read_to_string(path).ok()?;
    let picked = text
        .lines()
        .filter(|line| {
            let lower = line.to_ascii_lowercase();
            lower.contains("im_module")
                || lower.contains("im-module")
                || lower.contains("xmodifiers")
                || lower.contains("fcitx")
                || lower.contains("ibus")
        })
        .take(20)
        .collect::<Vec<_>>()
        .join("\n");
    (!picked.trim().is_empty()).then_some(picked)
}

fn read_environment_d_entries() -> Option<Vec<String>> {
    let home = std::env::var("HOME").ok()?;
    let dir = format!("{home}/.config/environment.d");
    let mut entries = Vec::new();
    for entry in std::fs::read_dir(dir).ok()?.flatten() {
        let path = entry.path();
        if path.extension().and_then(|value| value.to_str()) != Some("conf") {
            continue;
        }
        if let Ok(text) = std::fs::read_to_string(&path) {
            if let Some(probe) = read_environment_text_probe(&text) {
                entries.push(format!(
                    "{}:\n{}",
                    redact(&path.display().to_string()),
                    redact(&probe)
                ));
            }
        }
    }
    (!entries.is_empty()).then_some(entries)
}

fn read_environment_text_probe(text: &str) -> Option<String> {
    let picked = text
        .lines()
        .filter(|line| {
            line.contains("GTK_IM_MODULE")
                || line.contains("QT_IM_MODULE")
                || line.contains("QT_IM_MODULES")
                || line.contains("XMODIFIERS")
                || line.contains("SDL_IM_MODULE")
                || line.starts_with("LANG=")
                || line.starts_with("LC_ALL=")
                || line.starts_with("LC_CTYPE=")
                || line.starts_with("LC_MESSAGES=")
        })
        .take(20)
        .collect::<Vec<_>>()
        .join("\n");
    (!picked.trim().is_empty()).then_some(picked)
}

fn expand_home(path: &str) -> String {
    if let Some(rest) = path.strip_prefix("$HOME/") {
        if let Ok(home) = std::env::var("HOME") {
            return format!("{home}/{rest}");
        }
    }
    path.to_string()
}

fn infer_compositor_from_desktop(desktop: &str) -> String {
    let lower = desktop.to_ascii_lowercase();
    if lower.contains("kde") || lower.contains("plasma") {
        "kde_plasma".to_string()
    } else if lower.contains("gnome") {
        "gnome".to_string()
    } else if lower.contains("sway") {
        "sway".to_string()
    } else if lower.contains("hyprland") {
        "hyprland".to_string()
    } else if lower.contains("niri") {
        "niri".to_string()
    } else {
        "unknown".to_string()
    }
}

fn wayland_compositor_support_hint(compositor: &str) -> String {
    match compositor {
        "kde_plasma" => "KDE Plasma generally supports text-input-v2/v3 and input-method-v1; launch fcitx5 from Virtual Keyboard for protocol path.".to_string(),
        "gnome" => "GNOME uses text-input-v3 and an ibus-dbus style frontend path; fcitx5 can replace ibus-daemon when autostarted.".to_string(),
        "sway" => "Sway 1.10+ supports text-input-v3 with zwp_input_method_v2; older versions may need fcitx modules.".to_string(),
        "hyprland" | "niri" => "wlroots-style compositors vary; check text-input-v3 and input-method protocol support, and keep fcitx modules as fallback.".to_string(),
        _ => "unknown compositor; verify text-input and input-method protocol support before trusting native Wayland IM path.".to_string(),
    }
}

fn input_method_confidence_score(report: &DiagnosticReport) -> u8 {
    let mut score = 45u8;
    if report.checks.iter().any(|check| {
        check.id == "process.fcitx5.running" && matches!(check.status, CheckStatus::Ok)
    }) {
        score = score.saturating_add(10);
    }
    if report.facts.contains_key("input_method.app_env") {
        score = score.saturating_add(12);
    }
    if report.facts.contains_key("input_method.app_profile") {
        score = score.saturating_add(15);
    }
    if report
        .checks
        .iter()
        .any(|check| check.id == "input_method.xmodifiers")
    {
        score = score.saturating_add(6);
    }
    if report.facts.contains_key("input_method.questions") {
        score = score.saturating_add(10);
    }
    if report
        .facts
        .contains_key("input_method.compositor_support_hint")
    {
        score = score.saturating_add(4);
    }
    if report
        .logs
        .iter()
        .any(|log| log.source.contains("journalctl"))
    {
        score = score.saturating_add(6);
    }
    if report
        .checks
        .iter()
        .any(|check| matches!(check.status, CheckStatus::Error))
    {
        score = score.saturating_sub(10);
    }
    score.min(99)
}

async fn linux_fcitx_package_checks(
    config: &DiagnosticsPluginConfig,
    report: &mut DiagnosticReport,
) {
    if command_path(config, "pacman").await.is_none() {
        return;
    }
    for package in ["fcitx5", "fcitx5-qt", "fcitx5-gtk"] {
        let output = run_command(config, "pacman", &["-Q", package], 2).await;
        report.checks.push(Check {
            id: format!("input_method.package.{package}"),
            status: if output.status == Some(0) {
                CheckStatus::Ok
            } else {
                CheckStatus::Warn
            },
            detail: if output.status == Some(0) {
                format!("{package} is installed")
            } else {
                format!("{package} is not confirmed installed")
            },
            evidence: compact_evidence(&output),
        });
    }
}

async fn systemd_user_active_check(
    config: &DiagnosticsPluginConfig,
    report: &mut DiagnosticReport,
    service: &str,
) {
    if command_path(config, "systemctl").await.is_none() {
        return;
    }
    let output = run_command(config, "systemctl", &["--user", "is-active", service], 2).await;
    let active = output.stdout.trim() == "active";
    report.checks.push(Check {
        id: format!("systemd_user.{service}.active"),
        status: if active {
            CheckStatus::Ok
        } else {
            CheckStatus::Warn
        },
        detail: format!("{service} is {}", output.stdout.trim()),
        evidence: compact_evidence(&output),
    });
}

async fn app_probe_version(
    config: &DiagnosticsPluginConfig,
    report: &mut DiagnosticReport,
    target: &str,
) {
    let output = run_command(config, target, &["--version"], 3).await;
    report.checks.push(Check {
        id: "app.version_probe".to_string(),
        status: if output.status == Some(0) {
            CheckStatus::Ok
        } else {
            CheckStatus::Warn
        },
        detail: format!("ran {target} --version"),
        evidence: compact_evidence(&output),
    });
}

async fn app_probe_help(
    config: &DiagnosticsPluginConfig,
    report: &mut DiagnosticReport,
    target: &str,
) {
    let output = run_command(config, target, &["--help"], 3).await;
    report.checks.push(Check {
        id: "app.help_probe".to_string(),
        status: if output.status == Some(0) {
            CheckStatus::Ok
        } else {
            CheckStatus::Warn
        },
        detail: format!("ran {target} --help"),
        evidence: compact_evidence(&output),
    });
    if output.status != Some(0) && !output.stderr.trim().is_empty() {
        report.findings.push(Finding {
            severity: Severity::High,
            title: format!("{target} returned an error during startup probe"),
            evidence: clip(&output.stderr, 1_000),
        });
    }
}

async fn linux_package_owner(
    config: &DiagnosticsPluginConfig,
    report: &mut DiagnosticReport,
    path: &str,
) {
    if command_path(config, "pacman").await.is_none() {
        return;
    }
    let output = run_command(config, "pacman", &["-Qo", path], 3).await;
    if output.status == Some(0) {
        report
            .facts
            .insert("app.package_owner".to_string(), json!(output.stdout.trim()));
    }
}

async fn node_runtime_if_relevant(
    config: &DiagnosticsPluginConfig,
    report: &mut DiagnosticReport,
    target: &str,
    path: &str,
) {
    let lower = format!("{} {}", target, path).to_ascii_lowercase();
    if !(lower.contains("node") || lower.contains("npm") || lower.contains("opencode")) {
        return;
    }
    for command in ["node", "npm", "pnpm", "bun"] {
        command_exists_check(config, report, command).await;
        if command_path(config, command).await.is_some() {
            let output = run_command(config, command, &["--version"], 3).await;
            report.facts.insert(
                format!("runtime.{command}.version"),
                json!(clip(output.stdout.trim(), 200)),
            );
        }
    }
}

async fn linux_recent_logs(
    args: &DiagnoseArgs,
    config: &DiagnosticsPluginConfig,
    report: &mut DiagnosticReport,
    keywords: &[&str],
) {
    if args.depth == Depth::Quick || command_path(config, "journalctl").await.is_none() {
        return;
    }
    let since = format!("-{}min", args.recent_minutes);
    let output = run_command(
        config,
        "journalctl",
        &["--user", "--since", &since, "--no-pager", "-n", "300"],
        5,
    )
    .await;
    push_filtered_log(report, "journalctl --user", &output.stdout, keywords);
}

async fn macos_recent_logs(
    args: &DiagnoseArgs,
    config: &DiagnosticsPluginConfig,
    report: &mut DiagnosticReport,
    keywords: &[&str],
) {
    if args.depth == Depth::Quick || command_path(config, "log").await.is_none() {
        return;
    }
    let last = format!("{}m", args.recent_minutes);
    let predicate = keywords
        .iter()
        .map(|keyword| format!("eventMessage CONTAINS[c] '{}'", keyword.replace('\'', "")))
        .collect::<Vec<_>>()
        .join(" OR ");
    let output = run_command(
        config,
        "log",
        &[
            "show",
            "--last",
            &last,
            "--style",
            "compact",
            "--predicate",
            &predicate,
        ],
        6,
    )
    .await;
    push_filtered_log(report, "log show", &output.stdout, keywords);
}

fn push_filtered_log(report: &mut DiagnosticReport, source: &str, text: &str, keywords: &[&str]) {
    let mut lines = Vec::new();
    for line in text.lines() {
        let lower = line.to_ascii_lowercase();
        if keywords
            .iter()
            .any(|keyword| lower.contains(&keyword.to_ascii_lowercase()))
        {
            lines.push(line);
        }
        if lines.len() >= 20 {
            break;
        }
    }
    if !lines.is_empty() {
        report.logs.push(LogExcerpt {
            source: source.to_string(),
            message: clip(&lines.join("\n"), 4_000),
        });
    }
}

async fn macos_quarantine_check(
    config: &DiagnosticsPluginConfig,
    report: &mut DiagnosticReport,
    path: &str,
) {
    if command_path(config, "xattr").await.is_none() {
        return;
    }
    let output = run_command(config, "xattr", &["-p", "com.apple.quarantine", path], 2).await;
    if output.status == Some(0) && !output.stdout.trim().is_empty() {
        report.findings.push(Finding {
            severity: Severity::Medium,
            title: "target has macOS quarantine attribute".to_string(),
            evidence: output.stdout.trim().to_string(),
        });
    }
}

async fn macos_codesign_check(
    config: &DiagnosticsPluginConfig,
    report: &mut DiagnosticReport,
    path: &str,
) {
    if command_path(config, "codesign").await.is_none() {
        return;
    }
    let output = run_command(config, "codesign", &["--verify", "--verbose", path], 4).await;
    report.checks.push(Check {
        id: "macos.codesign.verify".to_string(),
        status: if output.status == Some(0) {
            CheckStatus::Ok
        } else {
            CheckStatus::Warn
        },
        detail: "codesign verification probe".to_string(),
        evidence: compact_evidence(&output),
    });
}

async fn system_profiler_check(
    config: &DiagnosticsPluginConfig,
    report: &mut DiagnosticReport,
    data_type: &str,
    source: &str,
) {
    if command_path(config, "system_profiler").await.is_none() {
        return;
    }
    let output = run_command(config, "system_profiler", &[data_type], 8).await;
    if !output.stdout.trim().is_empty() {
        report.logs.push(LogExcerpt {
            source: source.to_string(),
            message: clip(&output.stdout, 4_000),
        });
    }
}

async fn command_path(config: &DiagnosticsPluginConfig, command: &str) -> Option<String> {
    if !safe_command_name(command) {
        return None;
    }
    let script = format!("command -v {}", shell_escape(command));
    let output = run_command(config, "sh", &["-c", &script], 2).await;
    if output.status == Some(0) && !output.stdout.trim().is_empty() {
        return Some(output.stdout.trim().to_string());
    }
    let output = run_command(config, "which", &[command], 2).await;
    (output.status == Some(0) && !output.stdout.trim().is_empty())
        .then(|| output.stdout.trim().to_string())
}

async fn run_command(
    config: &DiagnosticsPluginConfig,
    program: &str,
    args: &[&str],
    timeout_seconds: u64,
) -> ProbeOutput {
    let seconds = timeout_seconds
        .max(1)
        .min(config.command_timeout_seconds.max(1));
    let mut command = Command::new(program);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            return ProbeOutput {
                status: None,
                stdout: String::new(),
                stderr: String::new(),
                error: Some(error.to_string()),
                timed_out: false,
            };
        }
    };
    match timeout(Duration::from_secs(seconds), child.wait_with_output()).await {
        Ok(Ok(output)) => ProbeOutput {
            status: output.status.code(),
            stdout: redact(&clip(
                &String::from_utf8_lossy(&output.stdout),
                config.max_stdout_chars,
            )),
            stderr: redact(&clip(
                &String::from_utf8_lossy(&output.stderr),
                config.max_stderr_chars,
            )),
            error: None,
            timed_out: false,
        },
        Ok(Err(error)) => ProbeOutput {
            status: None,
            stdout: String::new(),
            stderr: String::new(),
            error: Some(error.to_string()),
            timed_out: false,
        },
        Err(_) => ProbeOutput {
            status: None,
            stdout: String::new(),
            stderr: String::new(),
            error: Some(format!("command timed out after {seconds}s")),
            timed_out: true,
        },
    }
}

fn compact_evidence(output: &ProbeOutput) -> Vec<String> {
    let mut evidence = Vec::new();
    if !output.stdout.trim().is_empty() {
        evidence.push(clip(output.stdout.trim(), 1_000));
    }
    if !output.stderr.trim().is_empty() {
        evidence.push(clip(output.stderr.trim(), 1_000));
    }
    if let Some(error) = &output.error {
        evidence.push(error.clone());
    }
    if output.timed_out && output.error.is_none() {
        evidence.push("command timed out".to_string());
    }
    evidence
}

fn fact_env(report: &mut DiagnosticReport, fact: &str, key: &str) {
    if let Ok(value) = std::env::var(key) {
        if !value.trim().is_empty() {
            report.facts.insert(fact.to_string(), json!(redact(&value)));
        }
    }
}

fn os_release_value(text: &str, key: &str) -> Option<String> {
    for line in text.lines() {
        let Some((name, value)) = line.split_once('=') else {
            continue;
        };
        if name == key {
            return Some(value.trim_matches('"').to_string());
        }
    }
    None
}

fn extract_lspci_gpu_blocks(text: &str) -> String {
    let mut blocks = Vec::new();
    let mut current = Vec::new();
    for line in text.lines() {
        let starts_device = line
            .chars()
            .next()
            .map(|ch| ch.is_ascii_hexdigit())
            .unwrap_or(false);
        if starts_device && !current.is_empty() {
            maybe_push_gpu_block(&mut blocks, &current);
            current.clear();
        }
        current.push(line.to_string());
    }
    if !current.is_empty() {
        maybe_push_gpu_block(&mut blocks, &current);
    }
    blocks.join("\n\n")
}

fn maybe_push_gpu_block(blocks: &mut Vec<String>, block: &[String]) {
    let header = block.first().map(String::as_str).unwrap_or_default();
    let lower = header.to_ascii_lowercase();
    if lower.contains("vga compatible controller")
        || lower.contains("3d controller")
        || lower.contains("display controller")
    {
        blocks.push(block.join("\n"));
    }
}

fn finalize_summary(report: &mut DiagnosticReport) {
    if !report.summary.is_empty() {
        return;
    }
    let errors = report
        .checks
        .iter()
        .filter(|check| matches!(check.status, CheckStatus::Error))
        .count();
    let warnings = report
        .checks
        .iter()
        .filter(|check| matches!(check.status, CheckStatus::Warn))
        .count();
    report.summary = if errors > 0 {
        format!(
            "context collection completed with {errors} error check(s) and {warnings} warning check(s)"
        )
    } else if warnings > 0 {
        format!("context collection completed with {warnings} warning check(s)")
    } else {
        "context collection completed without obvious errors in the selected probes".to_string()
    };
}

fn safe_command_name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 160
        && value
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.' | '+' | '/'))
}

fn shell_escape(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn clip(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        text.to_string()
    } else {
        format!(
            "{}\n...[truncated]",
            text.chars().take(max_chars).collect::<String>()
        )
    }
}

fn redact(text: &str) -> String {
    let home = std::env::var("HOME").unwrap_or_default();
    let user = std::env::var("USER").unwrap_or_default();
    let mut output = text.to_string();
    if !home.is_empty() {
        output = output.replace(&home, "$HOME");
    }
    if !user.is_empty() {
        output = output.replace(&format!("/{user}/"), "/$USER/");
        output = output.replace(&format!("{user}@"), "$USER@");
    }
    for marker in [
        "TOKEN=",
        "API_KEY=",
        "PASSWORD=",
        "SECRET=",
        "ACCESS_TOKEN=",
        "AUTH=",
    ] {
        output = redact_after_marker(&output, marker);
    }
    output
}

fn redact_after_marker(input: &str, marker: &str) -> String {
    let mut output = String::new();
    for line in input.lines() {
        if let Some(pos) = line.find(marker) {
            output.push_str(&line[..pos + marker.len()]);
            output.push_str("[REDACTED]\n");
        } else {
            output.push_str(line);
            output.push('\n');
        }
    }
    output.trim_end_matches('\n').to_string()
}

fn mask_network_addresses(text: &str) -> String {
    text.lines()
        .map(|line| {
            line.split_whitespace()
                .map(|token| {
                    if token.contains('/') && (token.contains('.') || token.contains(':')) {
                        "[ip/prefix]"
                    } else {
                        token
                    }
                })
                .collect::<Vec<_>>()
                .join(" ")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn infers_electron_wayland_profile_from_ozone_flags() {
        let command = "linuxqq --enable-features=UseOzonePlatform --ozone-platform=wayland --enable-wayland-ime --wayland-text-input-version=3";

        assert_eq!(
            infer_input_toolkit(command, Some("/usr/bin/linuxqq")),
            InputToolkit::ElectronChromium
        );
        assert_eq!(infer_display_backend(command), "wayland");
        assert!(electron_uses_wayland(command));
        assert!(electron_wayland_ime_enabled(command));
    }

    #[test]
    fn infers_electron_xwayland_without_wayland_ime() {
        let command = "env GTK_IM_MODULE=fcitx linuxqq --ozone-platform=x11";

        assert_eq!(
            infer_input_toolkit(command, Some("/usr/bin/linuxqq")),
            InputToolkit::ElectronChromium
        );
        assert_eq!(infer_display_backend(command), "x11_or_xwayland");
        assert!(!electron_uses_wayland(command));
        assert!(!electron_wayland_ime_enabled(command));
    }

    #[test]
    fn infers_wechat_appimage_as_qt_from_wrapper_evidence() {
        let evidence = "exec /opt/wechat-appimage/wechat-appimage.AppImage export QT_IM_MODULE=fcitx package=wechat-appimage";

        assert_eq!(
            infer_input_toolkit(evidence, Some("/usr/bin/wechat")),
            InputToolkit::Qt
        );
    }

    #[test]
    fn infers_flatpak_kde_app_as_qt_from_runtime_evidence() {
        let evidence = "flatpak_app_id=org.kde.ghostwriter 运行时: org.kde.Platform/x86_64/6.10";

        assert_eq!(
            infer_input_toolkit(
                evidence,
                Some("/var/lib/flatpak/exports/bin/org.kde.ghostwriter")
            ),
            InputToolkit::Qt
        );
    }

    #[test]
    fn chrome_shims_do_not_override_chromium_evidence() {
        let evidence = "/opt/google/chrome/chrome /opt/google/chrome/chrome-sandbox /opt/google/chrome/libqt6_shim.so";

        assert_eq!(
            infer_input_toolkit(evidence, Some("/usr/bin/google-chrome-stable")),
            InputToolkit::ElectronChromium
        );
    }

    #[test]
    fn process_filter_ignores_probe_shell_commands() {
        let output = "123 /usr/bin/bash -c pgrep -af 'ghostwriter|org.kde.ghostwriter'\n456 /app/bin/ghostwriter\n";

        let matches = filtered_process_matches(output, "ghostwriter");

        assert_eq!(matches, vec!["456 /app/bin/ghostwriter".to_string()]);
    }

    #[test]
    fn parses_flatpak_metadata_runtime() {
        let metadata = "[Application]\nname=org.kde.ghostwriter\nruntime=org.kde.Platform/x86_64/6.10\ncommand=ghostwriter\n";

        assert_eq!(
            metadata_value(metadata, "Application", "runtime").as_deref(),
            Some("org.kde.Platform/x86_64/6.10")
        );
    }

    #[test]
    fn flatpak_probe_contributes_qt_fcitx_module_evidence() {
        let mut report = DiagnosticReport {
            ok: true,
            platform: Platform::Linux,
            query: None,
            mode: Mode::InputMethod,
            target: Some("ghostwriter".to_string()),
            symptom: None,
            depth: Depth::Normal,
            summary: String::new(),
            facts: BTreeMap::new(),
            checks: Vec::new(),
            logs: Vec::new(),
            findings: Vec::new(),
            hypotheses: Vec::new(),
            confidence: None,
            evidence_notes: Vec::new(),
            missing_evidence: Vec::new(),
            next_questions: Vec::new(),
            output_instruction: String::new(),
        };
        report.facts.insert(
            "input_method.app_flatpak_probe".to_string(),
            json!("flatpak_module=/runtime/files/lib/plugins/platforminputcontexts/libfcitx5platforminputcontextplugin.so"),
        );

        assert!(available_module_evidence(&report, &["platforminputcontext", "fcitx"]).is_some());
    }

    #[test]
    fn extracts_only_input_method_environment_lines() {
        let text = "PATH=/usr/bin\nXMODIFIERS=@im=fcitx\nQT_IM_MODULES=wayland;fcitx;ibus\nSECRET=hidden\n";

        let probe = read_environment_text_probe(text).unwrap();

        assert!(probe.contains("XMODIFIERS=@im=fcitx"));
        assert!(probe.contains("QT_IM_MODULES=wayland;fcitx;ibus"));
        assert!(!probe.contains("PATH="));
        assert!(!probe.contains("SECRET="));
    }

    #[test]
    fn confidence_increases_with_app_evidence() {
        let mut report = DiagnosticReport {
            ok: true,
            platform: Platform::Linux,
            query: None,
            mode: Mode::InputMethod,
            target: Some("linuxqq".to_string()),
            symptom: None,
            depth: Depth::Normal,
            summary: String::new(),
            facts: BTreeMap::new(),
            checks: Vec::new(),
            logs: Vec::new(),
            findings: Vec::new(),
            hypotheses: Vec::new(),
            confidence: None,
            evidence_notes: Vec::new(),
            missing_evidence: Vec::new(),
            next_questions: Vec::new(),
            output_instruction: String::new(),
        };
        report.facts.insert(
            "input_method.app_env".to_string(),
            json!({ "DISPLAY": ":0", "GTK_IM_MODULE": "fcitx" }),
        );
        report.facts.insert(
            "input_method.app_profile".to_string(),
            json!({ "toolkit": "electron_chromium" }),
        );
        report.facts.insert(
            "input_method.compositor_support_hint".to_string(),
            json!("hint"),
        );
        report.facts.insert(
            "input_method.questions".to_string(),
            json!({
                "backend_question": { "backend": "x11_or_xwayland" },
                "toolkit_question": { "toolkit": "ElectronChromium" },
                "module_question": { "confirmed_modules": [{"module": "gtk-im-module"}] },
                "environment_question": { "target_process_env": {"GTK_IM_MODULE": "fcitx"} }
            }),
        );
        report.facts.insert(
            "input_method.path_report".to_string(),
            json!({
                "app_adaptation": { "status": "supported_or_likely" },
                "environment_module_path": { "status": "configured" },
                "wayland_protocol_path": { "status": "possible" }
            }),
        );
        report.checks.push(Check {
            id: "process.fcitx5.running".to_string(),
            status: CheckStatus::Ok,
            detail: String::new(),
            evidence: Vec::new(),
        });
        report.checks.push(Check {
            id: "input_method.xmodifiers".to_string(),
            status: CheckStatus::Ok,
            detail: String::new(),
            evidence: Vec::new(),
        });

        finalize_input_method_confidence(&mut report);

        let confidence = report.confidence.unwrap();
        assert!(confidence.score >= 95);
        assert!(confidence.can_conclude);
        assert_ne!(confidence.label, "low");
    }

    #[test]
    fn unrunning_target_keeps_runtime_claims_unconfirmed() {
        let mut report = DiagnosticReport {
            ok: true,
            platform: Platform::Linux,
            query: None,
            mode: Mode::InputMethod,
            target: Some("steam".to_string()),
            symptom: None,
            depth: Depth::Normal,
            summary: String::new(),
            facts: BTreeMap::new(),
            checks: Vec::new(),
            logs: Vec::new(),
            findings: Vec::new(),
            hypotheses: Vec::new(),
            confidence: None,
            evidence_notes: Vec::new(),
            missing_evidence: vec![
                "target app steam is not running; cannot inspect runtime backend".to_string(),
            ],
            next_questions: Vec::new(),
            output_instruction: String::new(),
        };
        let profile = InputMethodAppProfile {
            toolkit: InputToolkit::Unknown,
            display_backend: "unknown".to_string(),
            runtime_observed: false,
            loaded_input_modules: Vec::new(),
            evidence_text: String::new(),
            command_line: None,
            desktop_exec: Some("env SDL_IM_MODULE=fcitx /usr/bin/steam %U".to_string()),
            electron_uses_wayland: false,
            electron_wayland_ime: false,
        };

        linux_input_method_profile_checks(&mut report, "steam", Some(&profile));
        linux_input_method_path_report(&mut report, Some(&profile));
        finalize_input_method_confidence(&mut report);

        assert!(report.findings.is_empty());
        assert!(!report.hypotheses.is_empty());
        assert!(report.facts.contains_key("input_method.questions"));
        assert!(report.facts.contains_key("input_method.path_report"));
        let confidence = report.confidence.unwrap();
        assert!(confidence.score <= 70);
        assert!(!confidence.can_conclude);
        assert_eq!(confidence.answer_tone, "uncertain");
    }

    #[test]
    fn path_report_has_three_core_branches() {
        let mut report = DiagnosticReport {
            ok: true,
            platform: Platform::Linux,
            query: None,
            mode: Mode::InputMethod,
            target: Some("gtk-demo".to_string()),
            symptom: None,
            depth: Depth::Normal,
            summary: String::new(),
            facts: BTreeMap::new(),
            checks: Vec::new(),
            logs: Vec::new(),
            findings: Vec::new(),
            hypotheses: Vec::new(),
            confidence: None,
            evidence_notes: Vec::new(),
            missing_evidence: Vec::new(),
            next_questions: Vec::new(),
            output_instruction: String::new(),
        };
        let profile = InputMethodAppProfile {
            toolkit: InputToolkit::Gtk,
            display_backend: "wayland".to_string(),
            runtime_observed: true,
            loaded_input_modules: vec![
                "pid 1: /usr/lib/gtk-3.0/3.0.0/immodules/im-fcitx.so".to_string()
            ],
            evidence_text: String::new(),
            command_line: Some("gtk-demo".to_string()),
            desktop_exec: None,
            electron_uses_wayland: false,
            electron_wayland_ime: false,
        };

        linux_input_method_path_report(&mut report, Some(&profile));

        let path_report = report.facts.get("input_method.path_report").unwrap();
        assert!(report.facts.contains_key("input_method.questions"));
        assert!(path_report.get("app_adaptation").is_some());
        assert!(path_report.get("environment_module_path").is_some());
        assert!(path_report.get("wayland_protocol_path").is_some());
    }

    #[test]
    fn modules_without_evidence_are_unknown_not_possible() {
        let report = DiagnosticReport {
            ok: true,
            platform: Platform::Linux,
            query: None,
            mode: Mode::InputMethod,
            target: Some("steam".to_string()),
            symptom: None,
            depth: Depth::Normal,
            summary: String::new(),
            facts: BTreeMap::new(),
            checks: Vec::new(),
            logs: Vec::new(),
            findings: Vec::new(),
            hypotheses: Vec::new(),
            confidence: None,
            evidence_notes: Vec::new(),
            missing_evidence: Vec::new(),
            next_questions: Vec::new(),
            output_instruction: String::new(),
        };
        let profile = InputMethodAppProfile {
            toolkit: InputToolkit::Unknown,
            display_backend: "unknown".to_string(),
            runtime_observed: false,
            loaded_input_modules: Vec::new(),
            evidence_text: String::new(),
            command_line: None,
            desktop_exec: Some("env SDL_IM_MODULE=fcitx /usr/bin/steam %U".to_string()),
            electron_uses_wayland: false,
            electron_wayland_ime: false,
        };

        let modules = module_question(&report, Some(&profile));

        assert!(modules.confirmed_modules.is_empty());
        assert!(modules.unsupported_modules.is_empty());
        assert!(modules
            .unknown_modules
            .iter()
            .any(|item| item.module == "sdl-im-module"));
        assert!(modules.rule.contains("No possible/likely"));
    }

    #[test]
    fn parses_desktop_exec_environment_assignments() {
        let values =
            parse_env_assignments("env GTK_IM_MODULE=fcitx SDL_IM_MODULE=fcitx /usr/bin/steam %U");

        assert_eq!(
            values.get("GTK_IM_MODULE").map(String::as_str),
            Some("fcitx")
        );
        assert_eq!(
            values.get("SDL_IM_MODULE").map(String::as_str),
            Some("fcitx")
        );
        assert!(!values.contains_key("/usr/bin/steam"));
    }

    #[test]
    fn environment_probe_keeps_locale_variables() {
        let text = "PATH=/usr/bin\nLC_CTYPE=en_US.UTF-8\nLANG=zh_CN.UTF-8\nGTK_IM_MODULE=xim\n";

        let probe = read_environment_text_probe(text).unwrap();

        assert!(probe.contains("LC_CTYPE=en_US.UTF-8"));
        assert!(probe.contains("LANG=zh_CN.UTF-8"));
        assert!(probe.contains("GTK_IM_MODULE=xim"));
        assert!(!probe.contains("PATH="));
    }

    #[test]
    fn gtk_activation_conditions_require_locale_evidence() {
        let conditions = module_activation_conditions("gtk-im-module");

        assert!(conditions.iter().any(|item| item.contains("LC_CTYPE")));
        assert!(conditions.iter().any(|item| item.contains("locale")));
    }

    #[test]
    fn mixed_wayland_and_display_env_is_not_native_wayland_evidence() {
        let mut report = DiagnosticReport {
            ok: true,
            platform: Platform::Linux,
            query: None,
            mode: Mode::InputMethod,
            target: Some("app".to_string()),
            symptom: None,
            depth: Depth::Normal,
            summary: String::new(),
            facts: BTreeMap::new(),
            checks: Vec::new(),
            logs: Vec::new(),
            findings: Vec::new(),
            hypotheses: Vec::new(),
            confidence: None,
            evidence_notes: Vec::new(),
            missing_evidence: Vec::new(),
            next_questions: Vec::new(),
            output_instruction: String::new(),
        };
        report.facts.insert(
            "input_method.app_env".to_string(),
            json!({ "WAYLAND_DISPLAY": "wayland-1", "DISPLAY": ":0" }),
        );

        assert_eq!(infer_display_backend_from_env(&report), "unknown_mixed_env");
    }

    #[test]
    fn confirms_module_only_from_loaded_module_evidence() {
        let mut report = DiagnosticReport {
            ok: true,
            platform: Platform::Linux,
            query: None,
            mode: Mode::InputMethod,
            target: Some("app".to_string()),
            symptom: None,
            depth: Depth::Normal,
            summary: String::new(),
            facts: BTreeMap::new(),
            checks: Vec::new(),
            logs: Vec::new(),
            findings: Vec::new(),
            hypotheses: Vec::new(),
            confidence: None,
            evidence_notes: Vec::new(),
            missing_evidence: Vec::new(),
            next_questions: Vec::new(),
            output_instruction: String::new(),
        };
        report.facts.insert(
            "input_method.app_env".to_string(),
            json!({ "GTK_IM_MODULE": "fcitx", "XMODIFIERS": "@im=fcitx" }),
        );
        let profile = InputMethodAppProfile {
            toolkit: InputToolkit::Gtk,
            display_backend: "x11_or_xwayland".to_string(),
            runtime_observed: true,
            loaded_input_modules: vec![
                "pid 7: /usr/lib/gtk-3.0/3.0.0/immodules/im-xim.so".to_string()
            ],
            evidence_text: String::new(),
            command_line: Some("app".to_string()),
            desktop_exec: None,
            electron_uses_wayland: false,
            electron_wayland_ime: false,
        };

        let modules = module_question(&report, Some(&profile));

        assert!(modules
            .confirmed_modules
            .iter()
            .any(|item| item.module == "gtk-im-module"));
    }

    #[test]
    fn env_only_does_not_confirm_gtk_module() {
        let mut report = DiagnosticReport {
            ok: true,
            platform: Platform::Linux,
            query: None,
            mode: Mode::InputMethod,
            target: Some("app".to_string()),
            symptom: None,
            depth: Depth::Normal,
            summary: String::new(),
            facts: BTreeMap::new(),
            checks: Vec::new(),
            logs: Vec::new(),
            findings: Vec::new(),
            hypotheses: Vec::new(),
            confidence: None,
            evidence_notes: Vec::new(),
            missing_evidence: Vec::new(),
            next_questions: Vec::new(),
            output_instruction: String::new(),
        };
        report.facts.insert(
            "input_method.app_env".to_string(),
            json!({ "GTK_IM_MODULE": "fcitx" }),
        );
        let profile = InputMethodAppProfile {
            toolkit: InputToolkit::Gtk,
            display_backend: "x11_or_xwayland".to_string(),
            runtime_observed: true,
            loaded_input_modules: Vec::new(),
            evidence_text: String::new(),
            command_line: Some("app".to_string()),
            desktop_exec: None,
            electron_uses_wayland: false,
            electron_wayland_ime: false,
        };

        let modules = module_question(&report, Some(&profile));

        assert!(modules.confirmed_modules.is_empty());
        assert!(modules
            .unknown_modules
            .iter()
            .any(|item| item.module == "gtk-im-module"));
    }

    #[test]
    fn gtk_path_can_be_confirmed_without_loaded_module_when_chain_is_configured() {
        let mut report = DiagnosticReport {
            ok: true,
            platform: Platform::Linux,
            query: None,
            mode: Mode::InputMethod,
            target: Some("app".to_string()),
            symptom: None,
            depth: Depth::Normal,
            summary: String::new(),
            facts: BTreeMap::new(),
            checks: Vec::new(),
            logs: Vec::new(),
            findings: Vec::new(),
            hypotheses: Vec::new(),
            confidence: None,
            evidence_notes: Vec::new(),
            missing_evidence: Vec::new(),
            next_questions: Vec::new(),
            output_instruction: String::new(),
        };
        report.facts.insert(
            "input_method.app_env".to_string(),
            json!({ "GTK_IM_MODULE": "fcitx" }),
        );
        report.facts.insert(
            "input_method.gtk_immodule_cache".to_string(),
            json!(parse_gtk_immodule_cache(
                "\"/usr/lib/gtk-3.0/3.0.0/immodules/im-fcitx5.so\"\n\"fcitx\" \"Fcitx\" \"gtk30\" \"/locale\" \"zh:ja:ko:*\"\n",
                "test-cache",
            )),
        );
        let profile = InputMethodAppProfile {
            toolkit: InputToolkit::Gtk,
            display_backend: "x11_or_xwayland".to_string(),
            runtime_observed: true,
            loaded_input_modules: Vec::new(),
            evidence_text: String::new(),
            command_line: Some("app".to_string()),
            desktop_exec: None,
            electron_uses_wayland: false,
            electron_wayland_ime: false,
        };

        let modules = module_question(&report, Some(&profile));

        let gtk = modules
            .confirmed_modules
            .iter()
            .find(|item| item.module == "gtk-im-module")
            .unwrap();
        assert!(gtk
            .evidence
            .iter()
            .any(|item| item.contains("path configured")));
    }

    #[test]
    fn xim_path_can_be_confirmed_without_loaded_module_when_chain_is_configured() {
        let mut report = DiagnosticReport {
            ok: true,
            platform: Platform::Linux,
            query: None,
            mode: Mode::InputMethod,
            target: Some("app".to_string()),
            symptom: None,
            depth: Depth::Normal,
            summary: String::new(),
            facts: BTreeMap::new(),
            checks: Vec::new(),
            logs: Vec::new(),
            findings: Vec::new(),
            hypotheses: Vec::new(),
            confidence: None,
            evidence_notes: Vec::new(),
            missing_evidence: Vec::new(),
            next_questions: Vec::new(),
            output_instruction: String::new(),
        };
        report.facts.insert(
            "input_method.app_env".to_string(),
            json!({ "XMODIFIERS": "@im=fcitx", "LC_CTYPE": "en_US.UTF-8" }),
        );
        let profile = InputMethodAppProfile {
            toolkit: InputToolkit::ElectronChromium,
            display_backend: "x11_or_xwayland".to_string(),
            runtime_observed: true,
            loaded_input_modules: Vec::new(),
            evidence_text: String::new(),
            command_line: Some("app".to_string()),
            desktop_exec: None,
            electron_uses_wayland: false,
            electron_wayland_ime: false,
        };

        let modules = module_question(&report, Some(&profile));

        assert!(modules
            .confirmed_modules
            .iter()
            .any(|item| item.module == "xim"));
    }

    #[test]
    fn parses_gtk_immodule_cache_locale_rules() {
        let cache = r#"
"/runtime/lib/gtk-3.0/3.0.0/immodules/im-xim.so"
"xim" "X Input Method" "gtk30" "/runtime/share/locale" "ko:ja:th:zh"
"/runtime/lib/gtk-3.0/3.0.0/immodules/im-cedilla.so"
"cedilla" "Cedilla" "gtk30" "/runtime/share/locale" "az:ca:co:fr:gv:oc:pt:sq:tr:wa"
"#;

        let entries = parse_gtk_immodule_cache(cache, "test-cache");

        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].module, "xim");
        assert!(entries[0].locales.contains(&"zh".to_string()));
        assert!(!gtk_entry_matches_locale(&entries[0], "en_US.UTF-8"));
        assert!(gtk_entry_matches_locale(&entries[0], "zh_CN.UTF-8"));
    }

    #[test]
    fn reports_requested_gtk_module_absent_from_cache() {
        let entries = parse_gtk_immodule_cache(
            "\"/runtime/immodules/im-xim.so\"\n\"xim\" \"XIM\" \"gtk30\" \"/locale\" \"ko:ja:th:zh\"\n",
            "test-cache",
        );

        let report = gtk_requested_module_report("fcitx", &entries);

        assert!(!report.present_in_cache);
        assert!(report.evidence[0].contains("no matching entry"));
    }

    #[test]
    fn module_question_includes_gtk_cache_and_locale_selection() {
        let mut report = DiagnosticReport {
            ok: true,
            platform: Platform::Linux,
            query: None,
            mode: Mode::InputMethod,
            target: Some("app".to_string()),
            symptom: None,
            depth: Depth::Normal,
            summary: String::new(),
            facts: BTreeMap::new(),
            checks: Vec::new(),
            logs: Vec::new(),
            findings: Vec::new(),
            hypotheses: Vec::new(),
            confidence: None,
            evidence_notes: Vec::new(),
            missing_evidence: Vec::new(),
            next_questions: Vec::new(),
            output_instruction: String::new(),
        };
        report.facts.insert(
            "input_method.app_env".to_string(),
            json!({ "GTK_IM_MODULE": "fcitx", "LC_CTYPE": "zh_CN.UTF-8" }),
        );
        report.facts.insert(
            "input_method.gtk_immodule_cache".to_string(),
            json!(parse_gtk_immodule_cache(
                "\"/runtime/immodules/im-xim.so\"\n\"xim\" \"XIM\" \"gtk30\" \"/locale\" \"ko:ja:th:zh\"\n",
                "test-cache",
            )),
        );
        let profile = InputMethodAppProfile {
            toolkit: InputToolkit::Gtk,
            display_backend: "x11_or_xwayland".to_string(),
            runtime_observed: true,
            loaded_input_modules: Vec::new(),
            evidence_text: String::new(),
            command_line: Some("app".to_string()),
            desktop_exec: None,
            electron_uses_wayland: false,
            electron_wayland_ime: false,
        };

        let modules = module_question(&report, Some(&profile));

        assert!(modules.gtk_requested_module.unwrap().requested == "fcitx");
        assert!(modules
            .gtk_locale_selected_modules
            .iter()
            .any(|entry| entry.module == "xim"));
    }
}
