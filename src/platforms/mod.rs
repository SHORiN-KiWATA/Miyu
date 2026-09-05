//! IM platform bridges.
//!
//! This module is the platform-neutral core: turn driving against the
//! agent actor, session resolution, rate limiting and reply shaping.
//! Each protocol lives in its own submodule (`onebot` = NapCat / QQ);
//! later platforms (Telegram, QQ official, WeChat) add submodules and
//! reuse everything here without touching the web core.

mod activity;
mod inflight;
mod live_turns;
mod logging;
mod reply;
mod scheduling;
mod turn_context;
mod turn_order;
mod turn_run;
pub(crate) use activity::*;
pub(crate) use logging::*;
pub(crate) use reply::*;
pub(crate) use scheduling::*;
pub(crate) use turn_context::*;
pub(crate) use turn_order::*;
pub(crate) use turn_run::*;
mod access_control;
mod assets;
pub(crate) mod avatar;
pub(crate) mod commands;
pub(crate) mod file_reader;
pub(crate) mod onebot;
pub(crate) mod plugins;
mod tool;
// 平台层的纯数据类型下沉到 crate::platform_types：tools / memory / agent 都
// 要用它们（图片引用、主体身份、会话标识），但不该为此依赖整个平台运行时。
// 这里原样再导出，`platforms::PlatformPrincipal` 这类写法一个字都不用改。
mod tool_context;

pub(crate) use crate::platform_types::{
    BotGroupRole, BotSendAvailability, ConversationKind, ForwardNode, OutboundBody,
    OutboundMessage, OutboundOrigin, OutboundSegment, PartialSendError, PlatformAdapter,
    PlatformContextFileRef, PlatformContextImageRef, PlatformConversation, PlatformFileDownload,
    PlatformGroupMember, PlatformImageData, PlatformInboundEvent, PlatformInboundEventKind,
    PlatformInboundMedia, PlatformMediaKind, PlatformMention, PlatformMessageInfo,
    PlatformMessagePosition, PlatformPrincipal, ResponseTarget, SendReceipt, TriggerDecision,
};

use crate::agent::{AgentMode, QueueIngressBarrier, QueueIngressReservation};
use crate::config::{
    ActiveProviderModelConfig, AppConfig, PlatformRateLimit, PlatformSessionLimits, PromptAudience,
};
use crate::i18n::{text_for, Locale};
use crate::ipc::ImageAttachment;
use crate::paths::MiyuPaths;
use crate::runtime::{
    random_id, validate_content, ActorCommand, DaemonState, IpcRunGuard, RunInfo,
};
use crate::state::{PlatformSessionBindingKey, StateStore};
use anyhow::{anyhow, bail, Context, Result};
use futures_util::StreamExt;
use serde_json::Value;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::{Duration, Instant};
use tokio::sync::broadcast;

/// Shared state for all IM bridges, hung off `DaemonState`. Cheap to clone;
/// everything inside is reference counted.
#[derive(Clone)]
pub(crate) struct PlatformRuntime {
    http: Arc<OnceLock<std::result::Result<reqwest::Client, String>>>,
    pub(crate) onebot: Arc<Mutex<onebot::ConnectionRegistry>>,
    pub(crate) qq_listener: onebot::QqListenerManager,
    pub(crate) rate: Arc<Mutex<RateWindow>>,
    plugins: Arc<OnceLock<std::result::Result<Arc<plugins::PlatformPluginRegistry>, String>>>,
    pub(crate) assets: assets::AssetLeaseStore,
    pub(crate) turn_permits: Arc<tokio::sync::Semaphore>,
    pub(crate) file_store_lock: Arc<tokio::sync::Mutex<()>>,
    pub(crate) message_activity: MessageActivityRegistry,
    session_turn_locks: Arc<Mutex<HashMap<String, Weak<SessionTurnState>>>>,
    /// 到达顺序闸,按会话标识分表(见 `turn_order`)。与 session 锁分开,是
    /// 因为它要在 session id 查出来之前就登记。
    pub(crate) turn_order: TurnOrderRegistry,
}

impl PlatformRuntime {
    pub(crate) fn new() -> Result<Self> {
        Ok(Self {
            http: Arc::new(OnceLock::new()),
            onebot: Arc::new(Mutex::new(onebot::ConnectionRegistry::default())),
            qq_listener: onebot::QqListenerManager::default(),
            rate: Arc::new(Mutex::new(RateWindow::new())),
            plugins: Arc::new(OnceLock::new()),
            assets: assets::AssetLeaseStore::new(),
            turn_permits: Arc::new(tokio::sync::Semaphore::new(MAX_CONCURRENT_PLATFORM_TURNS)),
            file_store_lock: Arc::new(tokio::sync::Mutex::new(())),
            message_activity: MessageActivityRegistry::default(),
            session_turn_locks: Arc::new(Mutex::new(HashMap::new())),
            turn_order: TurnOrderRegistry::default(),
        })
    }

    pub(crate) fn http_client(&self) -> Result<reqwest::Client> {
        self.http
            .get_or_init(|| {
                reqwest::Client::builder()
                    .connect_timeout(Duration::from_secs(10))
                    .build()
                    .map_err(|error| error.to_string())
            })
            .as_ref()
            .cloned()
            .map_err(|error| anyhow!("building the IM platform HTTP client: {error}"))
    }

    pub(crate) fn plugins(&self) -> Result<Arc<plugins::PlatformPluginRegistry>> {
        self.plugins
            .get_or_init(|| {
                plugins::PlatformPluginRegistry::built_in()
                    .map(Arc::new)
                    .map_err(|error| error.to_string())
            })
            .as_ref()
            .cloned()
            .map_err(|error| anyhow!("building the IM platform plugin registry: {error}"))
    }

    fn session_turn_ticket(
        &self,
        session_id: &str,
        limits: PlatformSessionLimits,
    ) -> SessionTurnTicket {
        let state = {
            let mut locks = self.session_turn_locks.lock().unwrap();
            match locks.get(session_id).and_then(Weak::upgrade) {
                Some(state) => state,
                None => {
                    let state = Arc::new(SessionTurnState::new(limits));
                    locks.insert(session_id.to_string(), Arc::downgrade(&state));
                    state
                }
            }
        };
        SessionTurnTicket {
            session_id: session_id.to_string(),
            generation: state.generation.load(Ordering::Acquire),
            state,
            states: self.session_turn_locks.clone(),
            exclusive: false,
            order_slot: None,
        }
    }

    /// 用一个**已登记的**到达顺序位建票。顺序位在派发链最前面就拿到手
    /// (`turn_order.enter`),这里只是把它交给票据保管。
    ///
    /// 票据本身**必须在主动回复判断之前建**:它快照 generation,判断期间发生
    /// 的覆盖要能让这张票失效。真正阻塞的是随后的 `acquire()`。
    pub(crate) fn session_turn_ticket_in_order(
        &self,
        session_id: &str,
        limits: PlatformSessionLimits,
        order_slot: TurnOrderSlot,
    ) -> SessionTurnTicket {
        let mut ticket = self.session_turn_ticket(session_id, limits);
        ticket.order_slot = Some(order_slot);
        ticket
    }

    async fn acquire_session_turn(
        &self,
        session_id: &str,
        limits: PlatformSessionLimits,
    ) -> std::result::Result<SessionTurnLease, SessionTurnAcquireError> {
        self.session_turn_ticket(session_id, limits).acquire().await
    }

    /// `limits` 只在这条会话尚无调度状态时生效(建状态用)。传解析后的值,
    /// 别传 default——串行体制下用 default 建出来的状态带着 8 个并行位,
    /// 后续回合会顺着它偷跑(08-26 会话内并行开关)。
    pub(crate) fn preempt_session_turns(
        &self,
        session_id: &str,
        limits: PlatformSessionLimits,
    ) -> SessionTurnTicket {
        let mut ticket = self.session_turn_ticket(session_id, limits);
        ticket.generation = ticket
            .state
            .generation
            .fetch_add(1, Ordering::AcqRel)
            .wrapping_add(1);
        ticket.exclusive = true;
        ticket.state.preempting.store(true, Ordering::Release);
        ticket
    }

    /// 这条会话上还压着多少回合。取顺序闸的登记数而不是 `waiting`:判断挪到
    /// 抢名额之前以后,同一时刻最多只有一条卡在信号量上,`waiting` 恒 0 或 1,
    /// 拿它报数会让 `/stop` 把八条积压说成一条(08-26 二轮审查)。
    pub(crate) fn queued_turns(&self, conversation_scope: &str) -> usize {
        self.turn_order.backlog(conversation_scope)
    }
}

/// 把一个 registry 收敛成平台回合该有的样子:非管理员会话换成受限底座,
/// 管理员会话保留底座但摘掉 owner 专属工具、并把记忆工具作用域化到该会话。
///
/// 平台回合(turns/task.rs)与 MCP 桥(web/session_cmds.rs)两条路必须给出
/// 同一套工具面——桥曾经直接发 owner 面全量 registry,非管理员群友经它就
/// 能调 run_command 与 claude_code(08-26 审查)。收在这里,两边不再各写一遍。
/// `restricted_base` 给调用方复用已建好的受限底座(回合路径每轮都要用,
/// 现建一次不划算);传 None 就地建一个。
pub(crate) fn apply_platform_turn_scope(
    registry: &mut crate::tools::ToolRegistry,
    config: &crate::config::AppConfig,
    paths: &crate::paths::MiyuPaths,
    context: &PlatformTurnContext,
    restricted_base: Option<&crate::tools::ToolRegistry>,
) {
    if !context.host_tools_allowed() {
        *registry = match restricted_base {
            Some(base) => base.clone(),
            None => crate::tools::restricted_platform_registry(config, paths),
        };
    } else {
        crate::tools::rescope_platform_memory_tools(registry, config, paths, context, false);
    }
    // claude_code 只属于本机 owner 面(§09):订阅额度与本机代理权限不跟
    // 平台身份走,管理员会话也一并摘掉。该委托工具 08-21 已删除,这行是它
    // 万一回归时的常备闸——当下不生效,也无法被测试钉住。
    registry.unregister("claude_code");
    // 扬声器与「发到 QQ」都是本机 owner 面的东西:QQ 聊天通常是远程的,
    // 从群里让电脑开口没有意义;从 QQ 会话再发 QQ 更是绕圈(那边有
    // send_message_to_user)。管理员会话也一并摘掉。
    registry.unregister("speak");
    registry.unregister("send_qq_message");
}

pub(crate) use assets::platform_asset;
pub(crate) use live_turns::{live_turn_context, LiveTurnGuard};

#[cfg(test)]
pub(crate) mod tests;
