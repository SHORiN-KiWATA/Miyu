//! Background command jobs: spawn-and-forget shell processes with status
//! polling, bounded lifetimes, and orphan hygiene across restarts.
//!
//! Jobs live in the current process (daemon or direct REPL). A restart
//! terminates them — the ledger under the runtime dir lets the next
//! instance kill anything a crashed predecessor leaked. Completion invokes
//! an optional host hook (the daemon uses it to wake the model).

mod ledger;
mod output;
pub(crate) use ledger::*;
pub(crate) use output::*;

use super::{CommandOutputStream, ToolProgress, ToolRegistry, ToolSpec};
use crate::paths::MiyuPaths;
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::process::Command;

const STOP_GRACE: Duration = Duration::from_secs(5);
const STATUS_POLL: Duration = Duration::from_millis(250);

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum JobState {
    Running,
    Exited { code: Option<i32> },
    TimedOut,
    Stopped,
}

/// Human-facing Chinese label for a status string; tool outputs keep the
/// raw English label for the model.
pub fn status_display(status: &str) -> String {
    if !crate::i18n::is_zh() {
        return match status {
            "stopped" => "stopped".to_string(),
            "timed_out" => "timed out".to_string(),
            "exited(signal)" => "killed".to_string(),
            "exited(0)" => "done".to_string(),
            other => other
                .strip_prefix("exited(")
                .and_then(|rest| rest.strip_suffix(')'))
                .map(|code| format!("exit {code}"))
                .unwrap_or_else(|| other.to_string()),
        };
    }
    match status {
        "stopped" => "已中断".to_string(),
        "timed_out" => "已超时".to_string(),
        "exited(signal)" => "异常退出".to_string(),
        "exited(0)" => "完成".to_string(),
        other => other
            .strip_prefix("exited(")
            .and_then(|rest| rest.strip_suffix(')'))
            .map(|code| format!("退出码 {code}"))
            .unwrap_or_else(|| other.to_string()),
    }
}

impl JobState {
    fn label(&self) -> String {
        match self {
            JobState::Running => "running".to_string(),
            JobState::Exited { code: Some(code) } => format!("exited({code})"),
            JobState::Exited { code: None } => "exited(signal)".to_string(),
            JobState::TimedOut => "timed_out".to_string(),
            JobState::Stopped => "stopped".to_string(),
        }
    }

    fn is_terminal(&self) -> bool {
        !matches!(self, JobState::Running)
    }
}

/// What a background job actually is: an OS process group, or an in-process
/// detached subagent future.
#[derive(Clone)]
pub enum JobKind {
    Command { pid: u32 },
    Subagent { abort: tokio::task::AbortHandle },
}

#[derive(Clone)]
struct JobEntry {
    job_id: String,
    title: String,
    command: String,
    workspace: PathBuf,
    session_id: Option<Arc<str>>,
    /// 触发这轮回合的终端(shellhook/单次 CLI)。任务完成后 daemon 凭它把
    /// 跟进回复写回那个终端。
    origin_tty: Option<crate::ipc::OriginTty>,
    /// 平台回合里真实发起者的 user_id(如 QQ 号)。完成唤醒的合成回合凭它
    /// 继承发起者权限,而不是伪装成机器人自己(issue #29)。
    platform_sender: Option<String>,
    kind: JobKind,
    started_wall: SystemTime,
    started: Instant,
    finished: Option<Instant>,
    log_path: PathBuf,
    state: JobState,
}

/// Completion details handed to the host hook (daemon: model wake-up).
#[derive(Clone, Debug)]
pub struct JobCompletion {
    pub job_id: String,
    pub title: String,
    /// False when the model itself stopped the command — the host should
    /// clean up UI strips but not wake the model about it.
    pub wake_requested: bool,
    /// True for detached subagents (wording of the wake prompt differs).
    pub is_subagent: bool,
    pub command: String,
    pub workspace: PathBuf,
    pub session_id: Option<Arc<str>>,
    /// 触发终端指纹,见 [`crate::ipc::OriginTty`]。
    pub origin_tty: Option<crate::ipc::OriginTty>,
    /// 平台回合发起者的 user_id,唤醒合成事件用它还原身份(issue #29)。
    pub platform_sender: Option<String>,
    pub state_label: String,
    pub exit_code: Option<i32>,
    pub runtime_seconds: u64,
    pub log_path: PathBuf,
}

pub type CompletionHook = Arc<dyn Fn(JobCompletion) + Send + Sync>;
pub type StartedHook = Arc<dyn Fn(JobOverview) + Send + Sync>;

/// UI-facing snapshot of one job, for status strips and IPC polling.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct JobOverview {
    pub job_id: String,
    pub title: String,
    /// "command" or "subagent" — UIs word their labels by this.
    #[serde(default)]
    pub kind: String,
    /// Owning turn session; UIs only strip-display jobs of their own session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    pub status: String,
    pub running: bool,
    pub runtime_seconds: u64,
}

struct JobHost {
    paths: MiyuPaths,
}

fn jobs() -> &'static Mutex<HashMap<String, JobEntry>> {
    static JOBS: OnceLock<Mutex<HashMap<String, JobEntry>>> = OnceLock::new();
    JOBS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn host() -> &'static OnceLock<JobHost> {
    static HOST: OnceLock<JobHost> = OnceLock::new();
    &HOST
}

fn completion_hook() -> &'static Mutex<Option<CompletionHook>> {
    static HOOK: OnceLock<Mutex<Option<CompletionHook>>> = OnceLock::new();
    HOOK.get_or_init(|| Mutex::new(None))
}

fn started_hook() -> &'static Mutex<Option<StartedHook>> {
    static HOOK: OnceLock<Mutex<Option<StartedHook>>> = OnceLock::new();
    HOOK.get_or_init(|| Mutex::new(None))
}

/// Install the host started hook (daemon: publish job.started to UIs).
pub fn set_started_hook(hook: StartedHook) {
    *started_hook().lock().unwrap() = Some(hook);
}

impl JobEntry {
    /// 命令还是子代理。UI 的任务条与工具返回值共用同一份判定,免得两处措辞跑偏。
    fn kind_label(&self) -> &'static str {
        match self.kind {
            JobKind::Command { .. } => "command",
            JobKind::Subagent { .. } => "subagent",
        }
    }
}

fn overview_of(job: &JobEntry) -> JobOverview {
    JobOverview {
        job_id: job.job_id.clone(),
        title: job.title.clone(),
        kind: job.kind_label().to_string(),
        session_id: job.session_id.as_deref().map(str::to_string),
        status: job.state.label(),
        running: !job.state.is_terminal(),
        runtime_seconds: job
            .finished
            .unwrap_or_else(Instant::now)
            .duration_since(job.started)
            .as_secs(),
    }
}

/// Jobs the UI status strip should show: running only — finished commands
/// are reported by the wake follow-up, so a terminal chip carries no
/// information.
pub fn overview() -> Vec<JobOverview> {
    let mut jobs = jobs().lock().unwrap();
    prune_expired_terminal(&mut jobs);
    let mut rows = jobs
        .values()
        .filter(|job| !job.state.is_terminal())
        .collect::<Vec<_>>();
    rows.sort_by_key(|job| job.started_wall);
    rows.into_iter().map(overview_of).collect()
}

/// 完成且已报告的任务直接从注册表移除(验收 08-16 用户反馈:"做完了
/// 也不删除,一直留着占用后台"——此前只打标记,条目终身堆积)。日志
/// 文件留在磁盘,唤醒消息里带着 log_path,要翻旧账用 read。
pub fn acknowledge(job_id: &str) {
    let mut jobs = jobs().lock().unwrap();
    let terminal = jobs.get(job_id).is_some_and(|job| job.state.is_terminal());
    if terminal {
        jobs.remove(job_id);
    }
}

/// 兜底:唤醒没送达(宿主死亡/失败路径)的终态条目,完成满一小时后
/// 清出注册表——注册表只该装"还需要被看见"的任务。
const TERMINAL_RETENTION: Duration = Duration::from_secs(3600);

fn prune_expired_terminal(jobs: &mut HashMap<String, JobEntry>) {
    jobs.retain(|_, job| {
        !(job.state.is_terminal()
            && job
                .finished
                .is_some_and(|finished| finished.elapsed() > TERMINAL_RETENTION))
    });
}

/// Install the host completion hook (daemon: wake the model). Replaces any
/// previous hook; pass-through for the direct REPL which sets none.
pub fn set_completion_hook(hook: CompletionHook) {
    *completion_hook().lock().unwrap() = Some(hook);
}

/// One-time host init: remembers paths and sweeps ledger entries
/// left behind by dead predecessor processes.
pub fn init(paths: &MiyuPaths) {
    let _ = host().set(JobHost {
        paths: paths.clone(),
    });
    sweep_stale_jobs(paths);
    cleanup_old_logs(paths);
}

fn require_host() -> Result<&'static JobHost> {
    host()
        .get()
        .context("background jobs are not initialized in this process")
}

/// Terminate every job owned by this process; called on daemon shutdown
/// and direct-REPL exit so setsid'd children never outlive their host.
pub fn shutdown_all() {
    let running = jobs()
        .lock()
        .unwrap()
        .values()
        .filter(|job| job.state == JobState::Running)
        .map(|job| job.kind.clone())
        .collect::<Vec<_>>();
    let mut pids = Vec::new();
    for kind in &running {
        match kind {
            JobKind::Command { pid } => {
                signal_process_group(*pid, libc::SIGTERM);
                pids.push(*pid);
            }
            JobKind::Subagent { abort } => abort.abort(),
        }
    }
    if !pids.is_empty() {
        std::thread::sleep(Duration::from_millis(300));
        for pid in pids {
            if process_alive(pid) {
                signal_process_group(pid, libc::SIGKILL);
            }
        }
    }
    if let Some(host) = host().get() {
        sync_ledger(&host.paths);
    }
}

/// Spawn `command` detached in its own process group; stdout+stderr stream
/// into a log file. Returns the tool JSON for run_command.
pub async fn spawn_background(
    command: &str,
    title: Option<&str>,
    progress: &ToolProgress,
) -> Result<String> {
    let host = require_host()?;
    let title = title
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| value.chars().take(16).collect::<String>())
        .unwrap_or_else(|| {
            let mut fallback = command.chars().take(20).collect::<String>();
            if fallback.len() < command.len() {
                fallback.push('…');
            }
            fallback
        });
    let job_id = next_job_id();
    let dir = logs_dir(&host.paths);
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("failed to create job log dir {}", dir.display()))?;
    let log_path = dir.join(format!("{job_id}.log"));
    let log = std::fs::File::create(&log_path)
        .with_context(|| format!("failed to create job log {}", log_path.display()))?;
    let workspace = super::workspace::effective_workdir();
    let mut process = Command::new("sh");
    process
        .arg("-lc")
        .arg(command)
        .current_dir(&workspace)
        // 工具桥环境:后台脚本里的 `miyu tool-call` 也能以本会话身份执行。
        .envs(
            super::workspace::try_session()
                .map(|session| ("MIYU_SESSION".to_string(), session.to_string())),
        )
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::from(log.try_clone()?))
        .stderr(std::process::Stdio::from(log));
    process.process_group(0);
    super::sandbox::confine(&mut process);
    let mut child = process
        .spawn()
        .context("failed to spawn the background job")?;
    let pid = child.id().context("background job has no pid")?;
    let entry = JobEntry {
        job_id: job_id.clone(),
        title,
        command: command.to_string(),
        workspace: workspace.clone(),
        session_id: super::workspace::try_session(),
        origin_tty: super::workspace::current_origin_tty(),
        platform_sender: super::workspace::current_platform_sender(),
        kind: JobKind::Command { pid },
        started_wall: SystemTime::now(),
        started: Instant::now(),
        finished: None,
        log_path: log_path.clone(),
        state: JobState::Running,
    };
    let started = overview_of(&entry);
    jobs().lock().unwrap().insert(job_id.clone(), entry);
    sync_ledger(&host.paths);
    if let Some(hook) = started_hook().lock().unwrap().clone() {
        hook(started);
    }

    let reaper_job_id = job_id.clone();
    tokio::spawn(async move {
        // 后台任务不设运行时长上限:自然退出为准。泄漏保护由
        // sweep_stale_jobs(死进程清扫)与 job_stop 显式停止承担;
        // JobState::TimedOut 仅为兼容旧账本记录保留。
        let state = match child.wait().await {
            Ok(status) => match status.code() {
                Some(code) => JobState::Exited { code: Some(code) },
                None => JobState::Exited { code: None },
            },
            Err(_) => JobState::Exited { code: None },
        };
        finalize_job(&reaper_job_id, state, true);
    });

    // Surface the job id in the tool's visible output stream so the user
    // can see at a glance which job this call started.
    progress.report_command_output(
        CommandOutputStream::Stdout,
        format!(
            "{} {job_id}\n",
            crate::i18n::text("Running in background:", "已后台运行")
        )
        .into_bytes(),
    );

    Ok(serde_json::to_string_pretty(&json!({
        "ok": true,
        "job_id": job_id,
        "pid": pid,
        "log": log_path.display().to_string(),
        "note": "Background command running. You will be woken automatically when it finishes — do not poll job(action=status) to wait; query it only when you need interim logs. Never assume the result before completion."
    }))?)
}

/// Detach a subagent as a background job: allocate an id and log file,
/// register the entry, spawn the provided future, and finalize through the
/// same completion hook as background commands (same wake, strip, stop).
/// The builder receives (job_id, log_path) so the future can stream its
/// progress into the log that `job_status` reads.
pub async fn spawn_background_subagent<F>(
    title: Option<&str>,
    description: &str,
    progress: &ToolProgress,
    build: impl FnOnce(String, PathBuf) -> F,
) -> Result<String>
where
    F: std::future::Future<Output = JobState> + Send + 'static,
{
    let host = require_host()?;
    let job_id = next_job_id();
    let dir = logs_dir(&host.paths);
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("failed to create job log dir {}", dir.display()))?;
    let log_path = dir.join(format!("{job_id}.log"));
    std::fs::write(&log_path, b"")
        .with_context(|| format!("failed to create job log {}", log_path.display()))?;
    let title = title
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| value.chars().take(16).collect::<String>())
        .unwrap_or_else(|| {
            let mut fallback = description.chars().take(16).collect::<String>();
            if fallback.chars().count() < description.chars().count() {
                fallback.push('…');
            }
            fallback
        });
    let fut = build(job_id.clone(), log_path.clone());
    let reaper_job_id = job_id.clone();
    let handle = tokio::spawn(async move {
        let state = fut.await;
        finalize_job(&reaper_job_id, state, true);
    });
    let entry = JobEntry {
        job_id: job_id.clone(),
        title,
        command: description.to_string(),
        workspace: super::workspace::effective_workdir(),
        session_id: super::workspace::try_session(),
        origin_tty: super::workspace::current_origin_tty(),
        platform_sender: super::workspace::current_platform_sender(),
        kind: JobKind::Subagent {
            abort: handle.abort_handle(),
        },
        started_wall: SystemTime::now(),
        started: Instant::now(),
        finished: None,
        log_path: log_path.clone(),
        state: JobState::Running,
    };
    let started = overview_of(&entry);
    jobs().lock().unwrap().insert(job_id.clone(), entry);
    if let Some(hook) = started_hook().lock().unwrap().clone() {
        hook(started);
    }
    // Subagent detach note rides its own progress channel so it lands as the
    // block's ↳ subject line; CommandOutput is dropped for non-run_command.
    progress.report(format!(
        "__subagent_detach__{} {job_id}",
        crate::i18n::text("Running in background:", "已后台运行")
    ));
    Ok(serde_json::to_string_pretty(&json!({
        "ok": true,
        "kind": "background_subagent",
        "job_id": job_id,
        "log": log_path.display().to_string(),
        "note": "Subagent detached to the background. Query with job(action=status) (the log holds its progress); never assume its result before it finishes — you will be woken automatically when it completes."
    }))?)
}

fn finalize_job(job_id: &str, state: JobState, wake_requested: bool) {
    let completion = {
        let mut jobs = jobs().lock().unwrap();
        let Some(job) = jobs.get_mut(job_id) else {
            return;
        };
        if job.state.is_terminal() {
            return;
        }
        job.state = state.clone();
        job.finished = Some(Instant::now());
        JobCompletion {
            job_id: job.job_id.clone(),
            title: job.title.clone(),
            wake_requested,
            is_subagent: matches!(job.kind, JobKind::Subagent { .. }),
            command: job.command.clone(),
            workspace: job.workspace.clone(),
            session_id: job.session_id.clone(),
            origin_tty: job.origin_tty.clone(),
            platform_sender: job.platform_sender.clone(),
            state_label: state.label(),
            exit_code: match state {
                JobState::Exited { code } => code,
                _ => None,
            },
            runtime_seconds: job.started.elapsed().as_secs(),
            log_path: job.log_path.clone(),
        }
    };
    if let Some(host) = host().get() {
        sync_ledger(&host.paths);
    }
    let hook = completion_hook().lock().unwrap().clone();
    if let Some(hook) = hook {
        hook(completion);
    }
}

impl JobEntry {
    fn pid(&self) -> Option<u32> {
        match &self.kind {
            JobKind::Command { pid } => Some(*pid),
            JobKind::Subagent { .. } => None,
        }
    }
}

fn job_snapshot(job_id: &str) -> Option<JobEntry> {
    jobs().lock().unwrap().get(job_id).cloned()
}

/// Job ids a call is asking about: the `job_ids` array first, then a scalar
/// `job_id`, de-duplicated while keeping the caller's order. Shared by
/// `job_status` and `job_stop` — a plain `dedup()` only drops *adjacent*
/// repeats, so `["a","b","a"]` used to slip two `a`s through.
fn requested_job_ids(args: &Value) -> Vec<String> {
    let mut ids: Vec<String> = super::string_list(args.get("job_ids"));
    if let Some(single) = args
        .get("job_id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        ids.push(single.to_string());
    }
    let mut seen = std::collections::HashSet::new();
    ids.retain(|id| seen.insert(id.clone()));
    ids
}

/// Rejects ids owned by another session, mirroring the `all=true` escape
/// hatch both job tools offer.
fn ensure_jobs_visible(ids: &[String], current: Option<&str>, all: bool, verb: &str) -> Result<()> {
    let jobs = jobs().lock().unwrap();
    for id in ids {
        if let Some(job) = jobs.get(id) {
            if !job_visible(job.session_id.as_deref(), current, all) {
                bail!("background job {id} belongs to another session; pass all=true if you really need to {verb} it");
            }
        }
    }
    Ok(())
}

/// Session-scoped visibility: a tool call only sees jobs of its own turn
/// session unless it passes `all=true`.
///
/// **调用方在会话里、任务却没有会话** → 不可见（fail-closed）。以前这条走
/// `_ => true`，于是任何没带 session 的任务（台账恢复的旧任务、异常路径漏抓
/// 的）对**所有**会话可见——一个会话能 stop 掉另一个会话的东西。逃生门是
/// `all=true`，它本来就是给「我知道我在干什么」准备的。
///
/// 调用方**不在**会话里（测试、turn scope 外的直接调用）仍然全局可见：那里
/// 没有「本会话」可言，收紧只会让工具桥不可用。
fn job_visible(job_session: Option<&str>, current: Option<&str>, all: bool) -> bool {
    if all {
        return true;
    }
    match (current, job_session) {
        (Some(current), Some(session)) => current == session,
        (Some(_), None) => false,
        (None, _) => true,
    }
}

#[cfg(test)]
mod visibility_tests {
    use super::job_visible;

    /// 可见性是权限判定,不是显示偏好——`ensure_jobs_visible` 用它决定能不能
    /// stop 掉一个任务。所以「拿不准」必须是不可见。
    #[test]
    fn a_sessionless_job_is_invisible_from_inside_a_session() {
        // 同会话:看得见。
        assert!(job_visible(Some("s1"), Some("s1"), false));
        // 别的会话:看不见。
        assert!(!job_visible(Some("s2"), Some("s1"), false));
        // 任务没有会话,而我在某个会话里 —— 以前这里是 true,于是台账恢复的
        // 旧任务、异常路径漏抓 session 的任务对**所有**会话可见,一个会话能
        // stop 掉另一个会话的东西。
        assert!(!job_visible(None, Some("s1"), false));
        // all=true 是逃生门,上面三种一律放行。
        for job_session in [Some("s1"), Some("s2"), None] {
            assert!(job_visible(job_session, Some("s1"), true));
        }
        // 调用方不在会话里(测试、turn scope 外的工具桥):没有「本会话」可言,
        // 收紧只会让它不可用,保持全局可见。
        assert!(job_visible(Some("s1"), None, false));
        assert!(job_visible(None, None, false));
    }
}

/// Stop every running job bound to `session_id`; returns how many were
/// stopped. Used when the owning REPL exits — background commands follow
/// their conversation's lifecycle.
pub async fn stop_session_jobs(session_id: &str) -> usize {
    let targets = jobs()
        .lock()
        .unwrap()
        .values()
        .filter(|job| {
            job.state == JobState::Running && job.session_id.as_deref() == Some(session_id)
        })
        .map(|job| job.job_id.clone())
        .collect::<Vec<_>>();
    // Concurrent: serial stops used to add up, and with a stubborn child each
    // one held the caller for the whole grace period.
    let outcomes =
        futures_util::future::join_all(targets.iter().map(|job_id| stop_job(job_id))).await;
    outcomes.into_iter().filter(Result::is_ok).count()
}

/// Host-initiated stop (WebUI strip ✕ button); same semantics as job_stop.
pub async fn stop_job(job_id: &str) -> Result<()> {
    job_stop(json!({ "job_id": job_id })).await.map(|_| ())
}

async fn job_stop(args: Value) -> Result<String> {
    let ids = requested_job_ids(&args);
    if ids.is_empty() {
        bail!("provide at least one of job_id or job_ids; usage: job({{\"action\":\"stop\",\"job_ids\":[\"abc123\"]}})");
    }
    let all = args.get("all").and_then(Value::as_bool).unwrap_or(false);
    let current = super::workspace::try_session();
    ensure_jobs_visible(&ids, current.as_deref(), all, "stop")?;
    if ids.len() > 1 {
        // Concurrent for the same reason as `stop_session_jobs`; per-id errors
        // still stay per-id rather than aborting the batch.
        let outcomes = futures_util::future::join_all(ids.iter().map(|id| stop_one(id))).await;
        let results = ids
            .iter()
            .zip(outcomes)
            .map(|(id, outcome)| match outcome {
                Ok(status) => json!({ "job_id": id, "ok": true, "status": status }),
                Err(error) => json!({ "job_id": id, "ok": false, "error": error.to_string() }),
            })
            .collect::<Vec<_>>();
        return Ok(serde_json::to_string_pretty(&json!({
            "ok": true,
            "results": results,
        }))?);
    }
    let job_id = &ids[0];
    let job =
        job_snapshot(job_id).with_context(|| format!("background job {job_id} does not exist"))?;
    if job.state.is_terminal() {
        return Ok(serde_json::to_string_pretty(&json!({
            "ok": true,
            "job_id": job_id,
            "status": job.state.label(),
            "note": "the background task had already finished",
        }))?);
    }
    let status = stop_one(job_id).await?;
    Ok(serde_json::to_string_pretty(&json!({
        "ok": true,
        "job_id": job_id,
        "status": status,
    }))?)
}

/// Stop a single job; returns its resulting status label.
async fn stop_one(job_id: &str) -> Result<String> {
    let job =
        job_snapshot(job_id).with_context(|| format!("background job {job_id} does not exist"))?;
    if job.state.is_terminal() {
        return Ok(job.state.label());
    }
    // Mark terminal first so the reaper's own finalize becomes a no-op;
    // wake_requested=false tells the host to clean up without waking.
    finalize_job(job_id, JobState::Stopped, false);
    acknowledge(job_id);
    match &job.kind {
        JobKind::Command { pid } => {
            let pid = *pid;
            // SIGTERM goes out synchronously so a well-behaved child is
            // already dying when this returns. Only the grace period and the
            // SIGKILL escalation are detached — waiting them out inline is
            // what made Ctrl+C feel frozen, and it bought nothing: the job was
            // marked terminal above and has already left `overview()`.
            signal_process_group(pid, libc::SIGTERM);
            tokio::spawn(async move {
                let deadline = Instant::now() + STOP_GRACE;
                while process_alive(pid) && Instant::now() < deadline {
                    tokio::time::sleep(STATUS_POLL).await;
                }
                if process_alive(pid) {
                    signal_process_group(pid, libc::SIGKILL);
                }
            });
        }
        JobKind::Subagent { abort } => abort.abort(),
    }
    Ok("stopped".to_string())
}

/// 后台任务查询与停止合并成一件 `job`(08-17):同一个对象的两种操作。
///
/// 曾经还有一个只注册 status 的只读变体,注释说是给聊天/平台面用的——但从
/// 来没有任何地方调用它,受限平台注册表压根不含任何后台任务工具。一并删掉,
/// 别留着一段撒谎的死代码。
pub fn register_management(registry: &mut ToolRegistry) {
    registry.register(job_spec().writes().with_always_loaded(false));
}

fn job_spec() -> ToolSpec {
    let actions = json!(["status", "stop"]);
    let action_hint = "status inspects, stop terminates. Defaults to status.";
    let description = "Inspect background jobs. action=status with no other argument lists this session's jobs, each with recent_output (log tail) and log_size. For one job's incremental output pass job_id plus offset; for several pass job_ids (the log budget is split between them). action=stop terminates jobs (commands get SIGTERM then SIGKILL; subagents are aborted), single or by job_ids. Add all=true to reach other sessions.";
    ToolSpec::new(
        "job",
        description,
        json!({
            "type": "object",
            "properties": {
                "action": { "type": "string", "enum": actions, "description": action_hint },
                "all": { "type": "boolean", "description": "true reaches other sessions' jobs." },
                "job_id": { "type": "string", "description": "A single job id." },
                "job_ids": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Several job ids at once; for status the log budget is split between them."
                },
                "offset": { "type": "integer", "minimum": 0, "description": "status only: byte offset to resume reading the log from (use the previous next_offset)." }
            },
            "additionalProperties": false
        }),
        move |args| async move {
            match args.get("action").and_then(Value::as_str).unwrap_or("status") {
                "status" => job_status(args).await,
                "stop" => job_stop(args).await,
                other => bail!("unknown action: {other}; expected status or stop"),
            }
        },
    )
    .with_display_name("Background jobs")
}

#[cfg(test)]
mod tests;
