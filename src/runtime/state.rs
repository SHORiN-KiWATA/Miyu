//! 进程级会话与平台运行时状态。
//!
//! `DaemonState` 是整个 daemon 的根：配置管理器、状态库、事件总线、提问
//! 代理、actor 通道、平台运行时都挂在它上面。Web、IPC、平台适配三条路都
//! 从这里取。
// 兄弟模块的类型互相引用（DaemonState 持有 EventHub、run 记录引用
// ManagerState 等），统一从 mod.rs 的再导出取，免得每个文件维护一份
// 交叉导入清单。
use super::*;
use crate::agent::AgentMode;
use crate::config::AppConfig;
use crate::llm::OpenAiCompatibleClient;
use crate::paths::MiyuPaths;
use crate::platforms::PlatformRuntime;
use crate::state::StateStore;
use crate::tools::build_tool_registry;
use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, VecDeque};
use std::net::IpAddr;
// 只有 `for_test` 用得到，不加 cfg 的话 lib 构建会报未使用
#[cfg(test)]
use std::net::Ipv4Addr;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::{broadcast, mpsc};

// ── DaemonState / TurnEngineState / TurnResources 与其缓存 ──
#[derive(Clone)]
pub(crate) struct DaemonState {
    pub(crate) auth: WebAuth,
    pub(crate) boot_id: Arc<str>,
    pub(crate) web_port: u16,
    pub(crate) web_public: bool,
    pub(crate) web_bind: IpAddr,
    pub(crate) paths: MiyuPaths,
    pub(crate) manager: Arc<Mutex<ManagerState>>,
    pub(crate) state_store: StateStore,
    pub(crate) events: EventHub,
    pub(crate) questions: QuestionBroker,
    pub(crate) actor_tx: mpsc::UnboundedSender<ActorCommand>,
    pub(crate) shutdown_tx: broadcast::Sender<()>,
    pub(crate) turn_engine: TurnEngineState,
    pub(crate) platforms: PlatformRuntime,
}

#[cfg(test)]
impl DaemonState {
    pub(crate) fn for_test(paths: MiyuPaths, web_port: u16) -> Result<Self> {
        let state_store = StateStore::new(&paths)?;
        let config = AppConfig::default();
        let context = cold_context(&config, &paths, &state_store)?;
        let manager = Arc::new(Mutex::new(ManagerState {
            config,
            active_runs: HashMap::new(),
            admin_busy: false,
            context,
            persona_session_ids: HashMap::new(),
            runs_changed: Arc::new(tokio::sync::Notify::new()),
        }));
        let (actor_tx, _actor_rx) = mpsc::unbounded_channel();
        let (shutdown_tx, _shutdown_rx) = broadcast::channel(1);
        Ok(Self {
            auth: WebAuth::new(None),
            boot_id: Arc::from("boot-test"),
            web_port,
            web_public: false,
            web_bind: IpAddr::V4(Ipv4Addr::LOCALHOST),
            paths,
            manager,
            state_store,
            events: EventHub::new(),
            questions: QuestionBroker::new(),
            actor_tx,
            shutdown_tx,
            turn_engine: TurnEngineState::default(),
            platforms: PlatformRuntime::new()?,
        })
    }
}

#[derive(Clone, Default)]
pub(crate) struct TurnEngineState(Arc<AtomicU8>);

impl TurnEngineState {
    pub(crate) const COLD: u8 = 0;
    pub(crate) const INITIALIZING: u8 = 1;
    pub(crate) const READY: u8 = 2;
    pub(crate) const FAILED: u8 = 3;

    pub(crate) fn set(&self, state: u8) {
        self.0.store(state, Ordering::Release);
    }

    pub(crate) fn is_ready(&self) -> bool {
        self.0.load(Ordering::Acquire) == Self::READY
    }

    pub(crate) fn label(&self) -> &'static str {
        match self.0.load(Ordering::Acquire) {
            Self::INITIALIZING => "initializing",
            Self::READY => "ready",
            Self::FAILED => "failed",
            _ => "cold",
        }
    }
}

/// Expensive per-turn dependencies are initialized on first use and shared
/// by subsequent turns. The cache is keyed by the effective configuration so
/// a QQ conversation-specific model pool gets its own client/tool snapshot.
/// Configuration reloads clear the cache before the next request.
pub(crate) struct TurnResources {
    pub(crate) client: OpenAiCompatibleClient,
    pub(crate) normal_tools: crate::tools::ToolRegistry,
    pub(crate) dev_tools: crate::tools::ToolRegistry,
    pub(crate) restricted_tools: crate::tools::ToolRegistry,
}

// 8 而不是更多：每份 TurnResources 带三套完整工具注册表（实测 0.7–1.8 MB），
// 满槽就是十几 MB 的悬崖；miss 的代价只是几 ms 的重建。
pub(crate) const MAX_CACHED_TURN_RESOURCE_CONFIGS: usize = 8;

pub(crate) struct TurnResourceCache {
    pub(crate) entries: HashMap<[u8; 32], Arc<TurnResources>>,
    pub(crate) order: VecDeque<[u8; 32]>,
}

impl Default for TurnResourceCache {
    fn default() -> Self {
        Self {
            entries: HashMap::new(),
            order: VecDeque::new(),
        }
    }
}

impl TurnResourceCache {
    pub(crate) fn clear(&mut self) {
        self.entries.clear();
        self.order.clear();
    }

    pub(crate) fn key(config: &AppConfig) -> Result<[u8; 32]> {
        let encoded =
            serde_json::to_vec(config).context("serializing effective turn configuration")?;
        Ok(*blake3::hash(&encoded).as_bytes())
    }

    pub(crate) fn get_or_build(
        &mut self,
        config: &AppConfig,
        paths: &MiyuPaths,
    ) -> Result<Arc<TurnResources>> {
        let key = Self::key(config)?;
        if let Some(resources) = self.entries.get(&key).cloned() {
            self.order.retain(|entry| *entry != key);
            self.order.push_back(key);
            return Ok(resources);
        }

        crate::models_cache::ensure_active_metadata(paths, config);
        let restricted_tools = if config.tools.enabled {
            crate::tools::restricted_platform_registry(config, paths)
        } else {
            crate::tools::ToolRegistry::new()
        };
        crate::tools::register_script_display_names(&restricted_tools);
        let resources = Arc::new(TurnResources {
            client: OpenAiCompatibleClient::from_config(config, paths)?,
            normal_tools: build_tool_registry(config, paths, AgentMode::Normal, false)?,
            dev_tools: build_tool_registry(config, paths, AgentMode::Dev, false)?,
            restricted_tools,
        });

        if self.entries.len() >= MAX_CACHED_TURN_RESOURCE_CONFIGS {
            if let Some(oldest) = self.order.pop_front() {
                self.entries.remove(&oldest);
            }
        }
        self.order.push_back(key);
        self.entries.insert(key, resources.clone());
        Ok(resources)
    }
}

// ── WebAuth 登录节流（DaemonState 持有它，只能一起走） ──
#[derive(Clone)]
pub(crate) struct WebAuth {
    pub(crate) password_digest: Option<[u8; 32]>,
    /// 按登录先后有序:超限淘汰最旧令牌,而不是把全部在用会话一起登出。
    pub(crate) sessions: Arc<Mutex<Vec<String>>>,
    pub(crate) attempts: Arc<Mutex<HashMap<IpAddr, LoginAttempt>>>,
}

#[derive(Clone, Copy)]
pub(crate) struct LoginAttempt {
    pub(crate) window_started: Instant,
    pub(crate) failures: u8,
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum LoginFailure {
    Invalid,
    RateLimited,
}

impl WebAuth {
    pub(crate) fn new(password: Option<&str>) -> Self {
        let password_digest = password.map(|password| {
            let mut digest = Sha256::new();
            digest.update(password.as_bytes());
            digest.finalize().into()
        });
        Self {
            password_digest,
            sessions: Arc::new(Mutex::new(Vec::new())),
            attempts: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub(crate) fn required(&self) -> bool {
        self.password_digest.is_some()
    }

    pub(crate) fn is_authenticated(&self, supplied: Option<&str>) -> bool {
        if !self.required() {
            return true;
        }
        supplied.is_some_and(|token| {
            self.sessions
                .lock()
                .unwrap()
                .iter()
                .any(|existing| existing == token)
        })
    }

    /// 限流表的键。IPv6 按 **/64 前缀**归并。
    ///
    /// 按完整地址记的话，攻击者拿一个 /64（2⁶⁴ 个地址，随手轮换）就能让
    /// `attempts` 无限增长——这张表只增不删，是暴露在网络侧的内存耗尽入口。
    /// 而 /64 在现实里就是同一个来源，业界限流也按它算，归并之后轮换地址
    /// 不再有意义，限流本身还更严。
    ///
    /// 代价：同一个 /64 下的两个人共用一个计数桶。个人助手的 Web 界面里
    /// 这是可接受的取舍，也是标准做法。
    fn attempt_key(peer: IpAddr) -> IpAddr {
        match peer {
            IpAddr::V4(_) => peer,
            IpAddr::V6(address) => {
                let mut octets = address.octets();
                octets[8..].fill(0);
                IpAddr::V6(std::net::Ipv6Addr::from(octets))
            }
        }
    }

    /// 限流表当前跟踪了多少个来源。给测试量「有没有无限涨」用。
    #[cfg(test)]
    pub(crate) fn tracked_login_peers(&self) -> usize {
        self.attempts.lock().unwrap().len()
    }

    pub(crate) fn login(
        &self,
        peer: IpAddr,
        password: &str,
    ) -> std::result::Result<String, LoginFailure> {
        let Some(expected) = self.password_digest else {
            return Ok(String::new());
        };
        let now = Instant::now();
        let peer = Self::attempt_key(peer);
        {
            let mut attempts = self.attempts.lock().unwrap();
            // 窗口过了的条目留着没意义(下面本来就会重置计数),顺手清掉。
            // 只清过期的:还在窗口内的条目一旦被淘汰,计数就归零了,那等于
            // 给攻击者一条「刷满表把自己的记录挤掉」的绕过路径。
            if attempts.len() >= MAX_TRACKED_LOGIN_PEERS {
                attempts.retain(|_, entry| now.duration_since(entry.window_started) < LOGIN_WINDOW);
            }
            // 清完还是满的,说明这么多来源正在同时失败。此时不再收新条目——
            // 宁可让新来源走没有计数的路径,也不能把正在被限的记录挤掉。
            if attempts.len() >= MAX_TRACKED_LOGIN_PEERS && !attempts.contains_key(&peer) {
                return Err(LoginFailure::RateLimited);
            }
            let entry = attempts.entry(peer).or_insert(LoginAttempt {
                window_started: now,
                failures: 0,
            });
            if now.duration_since(entry.window_started) >= LOGIN_WINDOW {
                entry.window_started = now;
                entry.failures = 0;
            }
            if entry.failures >= LOGIN_ATTEMPT_LIMIT {
                return Err(LoginFailure::RateLimited);
            }
        }

        let mut digest = Sha256::new();
        digest.update(password.as_bytes());
        let supplied: [u8; 32] = digest.finalize().into();
        if !constant_time_eq(&supplied, &expected) {
            let mut attempts = self.attempts.lock().unwrap();
            if let Some(entry) = attempts.get_mut(&peer) {
                entry.failures = entry.failures.saturating_add(1);
            }
            return Err(LoginFailure::Invalid);
        }

        let token = random_token(32);
        let mut sessions = self.sessions.lock().unwrap();
        sessions.push(token.clone());
        // 第 65 个登录淘汰最旧的一个令牌;此前是 sessions.clear() 全员登出。
        if sessions.len() > 64 {
            sessions.remove(0);
        }
        Ok(token)
    }
}

// ── cold_context ──
/// 手里没有活的 `Agent` 时，这个会话的上下文快照。
///
/// `tokens` 曾经硬编码 0。daemon 冷启动后 `manager.context` 就是这份快照，
/// 而 `session_state_for` 对「当前会话」直接读它、不重算——于是重进 REPL 的
/// 首帧 footer 显示 `0/168k`，要等第一次对话 `finish_run` 写回真实值才恢复。
/// 会话明明有几万 token 的历史，屏幕上却是 0。
///
/// 真实值算不出来是因为它依赖整条组装链：系统提示词、人格、记忆、工具目录。
/// 所以这里临时建一个 Agent 现算，与 `session_state_for` 对**非当前**会话本
/// 来就走的那条路同一套口径——两条路算出来的数必须一样，否则切个会话数字就
/// 跳。建 Agent 的开销（注册表 + 提示词组装）那条路每次 IPC 都在付，这里
/// 只在没有活 Agent 时付一次。
///
/// 建不出来（配置坏了、供应商没配）不能让整个 daemon 起不来：退回 0 并把
/// 累计值照常带出去，footer 少一个数字好过起不来。
pub(crate) fn cold_context(
    config: &AppConfig,
    paths: &MiyuPaths,
    state_store: &StateStore,
) -> Result<ContextSnapshot> {
    let tokens = cold_context_tokens(config, paths, state_store).unwrap_or(0);
    cold_context_with_tokens(config, state_store, tokens)
}

/// 启动路径专用的 `cold_context`(09-04 issue #36)。
///
/// 现算 token 要建整套工具注册表,里面含 MCP tools/list:某个 MCP server
/// 不可达时这一步要等到列举超时,而 CLI 只给 daemon 8 秒就绪窗口——过去
/// 这条链跑在 IPC 起监听之前,一个死 server 就让整个 daemon(WebUI、QQ)
/// 起不来。
///
/// 所以启动时只等 [`STARTUP_CONTEXT_BUDGET`]:算得快(常态)照旧拿真数,
/// REPL 连上第一帧 footer 就是对的;算得慢就先用 0 占位放 daemon 就绪,
/// 真数继续在线程里算,算完从返回的 receiver 送出,由调用方回填。
pub(crate) const STARTUP_CONTEXT_BUDGET: Duration = Duration::from_secs(3);

pub(crate) fn startup_context(
    config: &AppConfig,
    paths: &MiyuPaths,
    state_store: &StateStore,
) -> Result<(ContextSnapshot, Option<Receiver<Result<ContextSnapshot>>>)> {
    let (tx, rx) = std::sync::mpsc::channel();
    {
        let config = config.clone();
        let paths = paths.clone();
        let state_store = state_store.clone();
        std::thread::Builder::new()
            .name("miyu-startup-context".to_string())
            .spawn(move || {
                let _ = tx.send(cold_context(&config, &paths, &state_store));
            })
            .context("spawning startup context worker")?;
    }
    match rx.recv_timeout(STARTUP_CONTEXT_BUDGET) {
        Ok(context) => Ok((context?, None)),
        Err(RecvTimeoutError::Timeout) => {
            tracing::warn!(
                budget_secs = STARTUP_CONTEXT_BUDGET.as_secs(),
                "startup context calculation is slow (unreachable MCP server?); \
                 daemon goes ready with a placeholder and backfills later"
            );
            Ok((cold_context_with_tokens(config, state_store, 0)?, Some(rx)))
        }
        Err(RecvTimeoutError::Disconnected) => {
            bail!("startup context worker exited without a result")
        }
    }
}

fn cold_context_with_tokens(
    config: &AppConfig,
    state_store: &StateStore,
    tokens: u64,
) -> Result<ContextSnapshot> {
    let cumulative = state_store.session_cumulative_token_totals()?;
    let window = config.active_context_window_with_source()?;
    Ok(ContextSnapshot {
        tokens,
        window: window.map(|(value, _)| value),
        window_assumed: matches!(
            window,
            Some((_, crate::config::ContextWindowSource::Assumed))
        ),
        cumulative_tokens: cumulative.total,
        cumulative_prompt_tokens: cumulative.prompt,
        cumulative_cache_read_tokens: cumulative.cache_read,
    })
}

/// 现算一次这个会话的上下文 token。失败返回 None，由调用方决定怎么退。
fn cold_context_tokens(
    config: &AppConfig,
    paths: &MiyuPaths,
    state_store: &StateStore,
) -> Option<u64> {
    crate::models_cache::ensure_active_metadata(paths, config);
    let client = OpenAiCompatibleClient::from_config(config, paths).ok()?;
    let registry = build_tool_registry(config, paths, AgentMode::Normal, true).ok()?;
    let agent = crate::agent::Agent::new(
        config.clone(),
        paths,
        state_store.clone(),
        client,
        registry,
        AgentMode::Normal,
    )
    .ok()?;
    agent.effective_context_tokens().ok()
}
