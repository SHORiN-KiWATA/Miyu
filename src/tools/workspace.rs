//! Per-turn workspace context. Tools resolve their working directory from a
//! task-local set by the running turn, so concurrently running turns with
//! different workspaces never interfere (std::env::current_dir is process
//! global). Outside a turn scope (direct CLI mode, tests) it falls back to
//! the process working directory.

use std::future::Future;
use std::path::PathBuf;

tokio::task_local! {
    static TURN_WORKSPACE: PathBuf;
    static TURN_SESSION: std::sync::Arc<str>;
    // 当前是否在子代理循环里跑。子代理的模型和主池可能不同,且子代理循环不做
    // 主回合那套 inline 媒体接力(把图塞进下一条消息的 content parts)——所以
    // 子代理里的 vision_analyze 一律走旁路转写(拿到的是文字,任何模型都能吃),
    // 不走 inline 寄存,否则图片被寄存却没人取,子代理只看到一个 ref 标记。
    static IN_SUBAGENT: bool;
}

/// Runs `future` marked as executing inside a subagent loop.
pub async fn with_subagent<F: Future>(future: F) -> F::Output {
    IN_SUBAGENT.scope(true, future).await
}

/// True when the current task is running inside a subagent loop.
pub fn in_subagent() -> bool {
    IN_SUBAGENT.try_with(|flag| *flag).unwrap_or(false)
}

/// Runs `future` with the given session id as the ambient turn session.
/// Subagents spawned inside the turn read it to link their audit sessions to
/// the parent.
pub async fn with_session<F: Future>(session_id: std::sync::Arc<str>, future: F) -> F::Output {
    TURN_SESSION.scope(session_id, future).await
}

/// The ambient turn session, if inside a turn scope.
pub fn try_session() -> Option<std::sync::Arc<str>> {
    TURN_SESSION.try_with(|session| session.clone()).ok()
}

/// Runs `future` with the given workspace as the ambient turn workspace.
pub async fn with_workspace<F: Future>(workspace: PathBuf, future: F) -> F::Output {
    TURN_WORKSPACE.scope(workspace, future).await
}

/// The directory tools should operate in: the ambient turn workspace, or the
/// process working directory outside a turn scope.
pub fn effective_workdir() -> PathBuf {
    try_workspace()
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| PathBuf::from("."))
}

/// The ambient turn workspace, if inside a turn scope.
pub fn try_workspace() -> Option<PathBuf> {
    TURN_WORKSPACE.try_with(|workspace| workspace.clone()).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn effective_workdir_returns_scoped_workspace() {
        let workspace = PathBuf::from("/tmp/miyu-turn-workspace");
        let seen = with_workspace(workspace.clone(), async { effective_workdir() }).await;
        assert_eq!(seen, workspace);
    }

    #[tokio::test]
    async fn effective_workdir_falls_back_to_process_cwd_outside_scope() {
        assert_eq!(try_workspace(), None);
        let cwd = std::env::current_dir().expect("process cwd");
        assert_eq!(effective_workdir(), cwd);
    }

    #[tokio::test]
    async fn workspace_visible_inside_select_nested_future() {
        let workspace = PathBuf::from("/tmp/miyu-select-workspace");
        let seen = with_workspace(workspace.clone(), async {
            let work = async { effective_workdir() };
            tokio::pin!(work);
            tokio::select! {
                result = &mut work => result,
                _ = std::future::ready(()) , if false => unreachable!(),
            }
        })
        .await;
        assert_eq!(seen, workspace);
    }
}

tokio::task_local! {
    /// 触发本回合的终端身份(shellhook/单次 CLI),供后台任务捕获,
    /// 完成后把跟进回复写回原终端。
    static ORIGIN_TTY: Option<crate::ipc::OriginTty>;
}

pub async fn with_origin_tty<F>(origin: Option<crate::ipc::OriginTty>, future: F) -> F::Output
where
    F: std::future::Future,
{
    ORIGIN_TTY.scope(origin, future).await
}

pub fn current_origin_tty() -> Option<crate::ipc::OriginTty> {
    ORIGIN_TTY.try_with(|origin| origin.clone()).ok().flatten()
}

tokio::task_local! {
    /// 触发本回合的平台侧真实发起者(如 QQ user_id)。后台任务 spawn 时捕获,
    /// 完成唤醒的合成回合凭它继承发起者的身份与权限;不继承的话合成事件只能
    /// 伪装成机器人自己,is_admin=false 会把工具表降级成受限集合(issue #29)。
    static PLATFORM_SENDER: Option<String>;
}

pub async fn with_platform_sender<F>(sender: Option<String>, future: F) -> F::Output
where
    F: std::future::Future,
{
    PLATFORM_SENDER.scope(sender, future).await
}

pub fn current_platform_sender() -> Option<String> {
    PLATFORM_SENDER
        .try_with(|sender| sender.clone())
        .ok()
        .flatten()
}

/// 本回合的发起来源(dsh goal 权限模型的 Miyu 化:不扫会话事件,发起方
/// 在起回合时如实声明)。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TurnOrigin {
    /// 人类输入(REPL/WebUI/平台真实消息)。
    Human,
    /// 后台任务完成唤醒的合成轮。
    JobWake,
    /// 目标驱动器自动开的续轮。
    ///
    /// 带着认领时的 (goal_id, revision, round)。工具层据此判定「本轮恰好是
    /// 当前目标的那一轮」——只有这种轮才允许模型自己报完成/受阻;别的自动轮
    /// (比如任务唤醒)不行,拿着旧轮号的更不行。
    GoalRound {
        goal_id: String,
        revision: i64,
        round: i64,
    },
}

tokio::task_local! {
    static TURN_ORIGIN: TurnOrigin;
}

pub async fn with_turn_origin<F>(origin: TurnOrigin, future: F) -> F::Output
where
    F: std::future::Future,
{
    TURN_ORIGIN.scope(origin, future).await
}

/// 缺省 Human:直连 CLI/测试没有包装层,而那里敲键盘的就是人。
pub fn current_turn_origin() -> TurnOrigin {
    TURN_ORIGIN
        .try_with(|origin| origin.clone())
        .unwrap_or(TurnOrigin::Human)
}

tokio::task_local! {
    /// 工具桥递归深度:回合内 run_command 起的脚本经 `miyu tool-call` 打回
    /// daemon 再执行工具,若那个工具又是 run_command……深度护栏防无限套娃。
    static BRIDGE_DEPTH: u32;
}

pub const MAX_BRIDGE_DEPTH: u32 = 2;

pub async fn with_bridge_depth<F>(depth: u32, future: F) -> F::Output
where
    F: std::future::Future,
{
    BRIDGE_DEPTH.scope(depth, future).await
}

pub fn current_bridge_depth() -> u32 {
    BRIDGE_DEPTH.try_with(|depth| *depth).unwrap_or(0)
}

/// 平台回合的生图配额。计数器挂在 turn future 的 task-local 上而不是共享
/// 注册表里:注册表在配置缓存中跨 turn 复用,放那里会让会话之间互相污染。
pub struct ImageGenLimit {
    per_request: usize,
    remaining: std::sync::atomic::AtomicUsize,
}

impl ImageGenLimit {
    pub fn new(per_request: usize) -> std::sync::Arc<Self> {
        std::sync::Arc::new(Self {
            per_request,
            remaining: std::sync::atomic::AtomicUsize::new(per_request),
        })
    }

    fn try_acquire(&self) -> bool {
        use std::sync::atomic::Ordering;
        loop {
            let remaining = self.remaining.load(Ordering::Acquire);
            let Some(next) = remaining.checked_sub(1) else {
                return false;
            };
            if self
                .remaining
                .compare_exchange(remaining, next, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                return true;
            }
        }
    }

    fn refund(&self) {
        use std::sync::atomic::Ordering;
        loop {
            let remaining = self.remaining.load(Ordering::Acquire);
            let next = (remaining + 1).min(self.per_request);
            if self
                .remaining
                .compare_exchange(remaining, next, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                return;
            }
        }
    }

    fn reset(&self) {
        self.remaining
            .store(self.per_request, std::sync::atomic::Ordering::Release);
    }
}

tokio::task_local! {
    /// `None` 或未设置 = 本地会话(REPL/WebUI/测试),不限流。
    static IMAGE_GEN_LIMIT: Option<std::sync::Arc<ImageGenLimit>>;
}

pub async fn with_image_gen_limit<F: Future>(
    limit: Option<std::sync::Arc<ImageGenLimit>>,
    future: F,
) -> F::Output {
    IMAGE_GEN_LIMIT.scope(limit, future).await
}

/// 申请一次生图配额。true = 放行。平台回合按 task-local 计数,其余无限。
pub fn try_allow_image() -> bool {
    IMAGE_GEN_LIMIT
        .try_with(|limit| {
            limit
                .as_ref()
                .map(|limit| limit.try_acquire())
                .unwrap_or(true)
        })
        .unwrap_or(true)
}

/// 生图请求失败时退还配额,让同一请求内的重试仍然可行;成功的生成不退。
pub fn refund_image_gen_allowance() {
    let _ = IMAGE_GEN_LIMIT.try_with(|limit| {
        if let Some(limit) = limit {
            limit.refund();
        }
    });
}

/// 排队 follow-up 被消费 = 用户更新了请求,配额重置。非平台回合是 no-op。
pub fn reset_image_gen_limit() {
    let _ = IMAGE_GEN_LIMIT.try_with(|limit| {
        if let Some(limit) = limit {
            limit.reset();
        }
    });
}

#[cfg(test)]
mod image_limit_tests {
    use super::*;

    #[tokio::test]
    async fn platform_limit_allows_one_then_blocks() {
        with_image_gen_limit(Some(ImageGenLimit::new(1)), async {
            assert!(try_allow_image());
            assert!(!try_allow_image());
        })
        .await;
    }

    #[tokio::test]
    async fn queued_prompt_reset_restores_allowance() {
        with_image_gen_limit(Some(ImageGenLimit::new(1)), async {
            assert!(try_allow_image());
            assert!(!try_allow_image());
            reset_image_gen_limit();
            assert!(try_allow_image());
            assert!(!try_allow_image());
        })
        .await;
    }

    #[tokio::test]
    async fn failed_generation_refund_keeps_retry_possible() {
        with_image_gen_limit(Some(ImageGenLimit::new(1)), async {
            assert!(try_allow_image());
            refund_image_gen_allowance();
            assert!(try_allow_image());
            // 退还不会超过配额上限。
            refund_image_gen_allowance();
            refund_image_gen_allowance();
            assert!(try_allow_image());
            assert!(!try_allow_image());
        })
        .await;
    }

    #[tokio::test]
    async fn local_turns_without_task_local_are_unlimited() {
        assert!(try_allow_image());
        assert!(try_allow_image());
        reset_image_gen_limit();
        refund_image_gen_allowance();
        assert!(try_allow_image());
    }

    #[tokio::test]
    async fn exempt_platform_turn_with_none_is_unlimited() {
        with_image_gen_limit(None, async {
            assert!(try_allow_image());
            assert!(try_allow_image());
        })
        .await;
    }

    #[tokio::test]
    async fn subagent_flag_is_scoped() {
        assert!(!in_subagent(), "外层不该带子代理标记");
        with_subagent(async {
            assert!(in_subagent(), "with_subagent 里应为真");
        })
        .await;
        assert!(!in_subagent(), "作用域外恢复");
    }
}
