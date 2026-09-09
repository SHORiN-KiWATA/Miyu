//! 运行记录与管理锁。
//!
//! 一个 run 是「一次占用会话的工作」——回合、压缩、管理操作都算。
//! `ManagerState` 记着当前活跃的 run，`IpcRunGuard` 负责在客户端断线时
//! 不误杀 daemon 里跑着的回合。
// 兄弟模块的类型互相引用（DaemonState 持有 EventHub、run 记录引用
// ManagerState 等），统一从 mod.rs 的再导出取，免得每个文件维护一份
// 交叉导入清单。
use crate::agent::AgentMode;
use crate::config::{AppConfig, PromptAudience};
use serde::Serialize;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

// ── RunInfo / RunOperation / ManagerState / ContextSnapshot ──
/// A turn currently executing in the daemon.
pub(crate) struct RunInfo {
    pub(crate) session_id: Arc<str>,
    pub(crate) mode: AgentMode,
    pub(crate) audience: PromptAudience,
    /// Signals cancellation to the turn task; the task selects on the
    /// paired receiver.
    pub(crate) cancel: tokio::sync::watch::Sender<bool>,
    pub(crate) turn_id: Option<String>,
    pub(crate) queue_target: Option<crate::state::RunningTurnQueueTarget>,
    pub(crate) supersede: Arc<crate::agent::TurnSupersedeSignal>,
    pub(crate) platform_followup: Option<Arc<crate::platforms::PlatformFollowupRun>>,
    pub(crate) operation: RunOperation,
    /// True for daemon-initiated background-command wake turns; lets REPL
    /// clients discover and attach to them for live rendering.
    pub(crate) job_wake: bool,
    /// 本回合的发起来源(goal 权限与取消语义用,见 workspace::TurnOrigin)。
    pub(crate) turn_origin: crate::tools::workspace::TurnOrigin,
    /// Display label for wake turns: "<job_id> · <title>".
    pub(crate) job_wake_label: Option<String>,
}

#[derive(Clone, Debug)]
pub(crate) enum RunOperation {
    Create,
    Redo { turn_id: String, input_id: String },
}

impl RunOperation {
    pub(crate) fn name(&self) -> &'static str {
        match self {
            Self::Create => "create",
            Self::Redo { .. } => "redo",
        }
    }

    pub(crate) fn turn_id(&self) -> Option<&str> {
        match self {
            Self::Create => None,
            Self::Redo { turn_id, .. } => Some(turn_id),
        }
    }

    pub(crate) fn input_id(&self) -> Option<&str> {
        match self {
            Self::Create => None,
            Self::Redo { input_id, .. } => Some(input_id),
        }
    }
}

impl RunInfo {
    pub(crate) fn request_cancel(&self) {
        if let Some(followup) = self.platform_followup.as_ref() {
            followup.close();
        }
        let _ = self.cancel.send(true);
    }
}

pub(crate) struct ManagerState {
    pub(crate) config: AppConfig,
    /// Concurrently running turns, keyed by run id. Turns run in parallel —
    /// including several in the same session (placeholder semantics) — so
    /// this replaces the old single `active_run_id`.
    pub(crate) active_runs: HashMap<String, RunInfo>,
    pub(crate) admin_busy: bool,
    /// 本次 admin 预约限定的会话。`None` = 全局预约（改配置/换模型这类，
    /// 必须挡住所有人）；`Some(id)` = 只挡这一个会话。
    ///
    /// 压缩/pop/undo/清空重写的是**单个会话**的消息数组，危险只对那个会话
    /// 成立。以前它们和全局操作共用一个布尔，于是压一个会话就把所有会话的
    /// 新回合全拒了（09-09 实况：一次 compact 期间所有会话报 busy）。
    pub(crate) admin_session: Option<String>,
    pub(crate) context: ContextSnapshot,
    pub(crate) persona_session_ids: HashMap<String, String>,
    /// 每当有 run 从 `active_runs` 移除时通知一次。等「某个/某些 run 结束」
    /// 的循环靠它事件化，免掉定周期拿全局锁轮询。
    pub(crate) runs_changed: Arc<tokio::sync::Notify>,
}

impl ManagerState {
    /// A run currently executing in the given session, if any (most callers
    /// only need one representative — e.g. the WebUI compat field).
    pub(crate) fn run_in_session(&self, session_id: &str) -> Option<&String> {
        self.active_runs
            .iter()
            .find(|(_, info)| &*info.session_id == session_id)
            .map(|(run_id, _)| run_id)
    }

    /// 当前的 admin 预约是否挡住这个会话：全局预约挡所有人，会话级预约
    /// 只挡它自己。
    pub(crate) fn admin_blocks_session(&self, session_id: &str) -> bool {
        self.admin_busy
            && self
                .admin_session
                .as_deref()
                .is_none_or(|scoped| scoped == session_id)
    }

    pub(crate) fn session_has_runs(&self, session_id: &str) -> bool {
        self.active_runs
            .values()
            .any(|info| &*info.session_id == session_id)
    }

    pub(crate) fn session_has_redo(&self, session_id: &str) -> bool {
        self.active_runs.values().any(|info| {
            &*info.session_id == session_id && matches!(info.operation, RunOperation::Redo { .. })
        })
    }

    pub(crate) fn session_runs_match_audience(
        &self,
        session_id: &str,
        audience: PromptAudience,
    ) -> bool {
        let mut runs = self
            .active_runs
            .values()
            .filter(|info| &*info.session_id == session_id);
        runs.next().is_some_and(|first| {
            first.audience == audience && runs.all(|info| info.audience == audience)
        })
    }

    /// 这个会话正在跑的是不是目标续轮。
    ///
    /// 用户在续轮跑着的时候发消息，应当排进那一轮（回合循环在每个工具边界
    /// 都会取排队的输入），而不是撞一个 409 让他自己重发——续轮是机器自己
    /// 开的，人开口理应优先。续轮的 audience 是 `Owner`，和 WebUI 发消息走
    /// 的 `External` 对不上，所以排队那条路要单独认它。
    pub(crate) fn session_runs_are_goal_rounds(&self, session_id: &str) -> bool {
        let mut runs = self
            .active_runs
            .values()
            .filter(|info| &*info.session_id == session_id)
            .peekable();
        runs.peek().is_some()
            && runs.all(|info| {
                matches!(
                    info.turn_origin,
                    crate::tools::workspace::TurnOrigin::GoalRound { .. }
                )
            })
    }
}

#[derive(Clone, Copy, Debug, Serialize)]
pub(crate) struct ContextSnapshot {
    pub(crate) tokens: u64,
    pub(crate) window: Option<usize>,
    /// `window` 是猜的还是有出处的。见 `ContextWindowSource`——猜的那个数跟具体
    /// 模型无关，footer 不能拿它算百分比。
    pub(crate) window_assumed: bool,
    pub(crate) cumulative_tokens: u64,
    pub(crate) cumulative_prompt_tokens: u64,
    pub(crate) cumulative_cache_read_tokens: u64,
}

// ── IpcRunGuard ──
pub(crate) struct IpcRunGuard {
    pub(crate) manager: Arc<Mutex<ManagerState>>,
    pub(crate) run_id: String,
    pub(crate) finished: bool,
}

impl IpcRunGuard {
    pub(crate) fn finish(&mut self) {
        self.finished = true;
    }
}

impl Drop for IpcRunGuard {
    fn drop(&mut self) {
        // dsh 语义(验收):回合归 daemon 所有,前端断线只是观众离席——
        // 不取消。曾经这里在客户端断开时砍掉 run,REPL 一关回合就死;
        // 现在 run 由 actor 跑到终态,finish_run 在完成路径里自行清理,
        // 断线客户端留下的只是一个没人看的事件流。guard 保留为挂点
        // (显式取消仍走 IpcCommand::Cancel)。
        let _ = self.finished;
    }
}

// ── finish_run ──
pub(crate) fn finish_run(
    manager: &Arc<Mutex<ManagerState>>,
    run_id: &str,
    context: Option<ContextSnapshot>,
) {
    let mut manager = manager.lock().unwrap();
    if let Some(context) = context {
        manager.context = context;
    }
    if let Some(run) = manager.active_runs.remove(run_id) {
        if let Some(followup) = run.platform_followup {
            followup.close();
        }
        manager.runs_changed.notify_waiters();
    }
}

// ── release_admin ──
pub(crate) fn release_admin(manager: &Arc<Mutex<ManagerState>>) {
    let mut manager = manager.lock().unwrap();
    manager.admin_busy = false;
    manager.admin_session = None;
}
