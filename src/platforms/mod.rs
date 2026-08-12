//! IM platform bridges.
//!
//! This module is the platform-neutral core: turn driving against the
//! agent actor, session resolution, rate limiting and reply shaping.
//! Each protocol lives in its own submodule (`onebot` = NapCat / QQ);
//! later platforms (Telegram, QQ official, WeChat) add submodules and
//! reuse everything here without touching the web core.

mod access_control;
mod assets;
pub(crate) mod avatar;
pub(crate) mod commands;
pub(crate) mod onebot;
pub(crate) mod plugins;
mod tool;
mod types;

pub(crate) use types::{
    BotGroupRole, BotSendAvailability, ConversationKind, ForwardNode, OutboundBody,
    OutboundMessage, OutboundOrigin, OutboundSegment, PartialSendError, PlatformAdapter,
    PlatformContextImageRef, PlatformConversation, PlatformGroupMember, PlatformImageData,
    PlatformInboundEvent, PlatformInboundEventKind, PlatformInboundMedia, PlatformMediaKind,
    PlatformMention, PlatformMessageInfo, PlatformMessagePosition, PlatformPrincipal,
    ResponseTarget, SendReceipt, TriggerDecision,
};

use crate::agent::{AgentMode, QueueIngressBarrier, QueueIngressReservation};
use crate::config::{
    ActiveProviderModelConfig, AppConfig, PlatformRateLimit, PlatformSessionLimits, PromptAudience,
};
use crate::i18n::{text_for, Locale};
use crate::ipc::ImageAttachment;
use crate::paths::MiyuPaths;
use crate::state::{PlatformSessionBindingKey, StateStore};
use crate::web::{random_id, validate_content, ActorCommand, DaemonState, IpcRunGuard, RunInfo};
use anyhow::{anyhow, bail, Context, Result};
use futures_util::StreamExt;
use serde_json::Value;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::{Duration, Instant};
use tokio::sync::broadcast;

/// How long a delivered image stays deduplicated for its conversation.
/// Auto-attached reply images (generate_image / search_web_images) must not
/// be sent twice when a turn is retried or recovered after an interrupted
/// send; an explicit "send it again" goes through send_message_to_user,
/// which is not filtered by this.
/// Kept short: it only needs to span a recovery turn, and a genuine
/// "send that one again" outside the window must still work.
const RECENT_IMAGE_TTL: Duration = Duration::from_secs(5 * 60);
const RECENT_IMAGE_CONVERSATIONS: usize = 64;
const RECENT_IMAGES_PER_CONVERSATION: usize = 32;

type RecentImageLedger = HashMap<String, Vec<(blake3::Hash, Instant)>>;

fn recent_images() -> &'static Mutex<RecentImageLedger> {
    static LEDGER: OnceLock<Mutex<RecentImageLedger>> = OnceLock::new();
    LEDGER.get_or_init(|| Mutex::new(HashMap::new()))
}

fn record_recent_conversation_images(scope_key: &str, digests: &[blake3::Hash]) {
    let now = Instant::now();
    let mut ledger = recent_images().lock().unwrap();
    ledger.retain(|_, entries| {
        entries.retain(|(_, at)| now.duration_since(*at) < RECENT_IMAGE_TTL);
        !entries.is_empty()
    });
    let entries = ledger.entry(scope_key.to_string()).or_default();
    for digest in digests {
        entries.retain(|(known, _)| known != digest);
        entries.push((*digest, now));
    }
    if entries.len() > RECENT_IMAGES_PER_CONVERSATION {
        let excess = entries.len() - RECENT_IMAGES_PER_CONVERSATION;
        entries.drain(..excess);
    }
    if ledger.len() > RECENT_IMAGE_CONVERSATIONS {
        // Bound the ledger even when every conversation stays inside the TTL.
        let oldest = ledger
            .iter()
            .filter_map(|(key, entries)| {
                entries.last().map(|(_, at)| (*at, key.clone()))
            })
            .min()
            .map(|(_, key)| key);
        if let Some(key) = oldest {
            ledger.remove(&key);
        }
    }
}

fn recent_conversation_images(scope_key: &str) -> Vec<blake3::Hash> {
    let now = Instant::now();
    recent_images()
        .lock()
        .unwrap()
        .get(scope_key)
        .map(|entries| {
            entries
                .iter()
                .filter(|(_, at)| now.duration_since(*at) < RECENT_IMAGE_TTL)
                .map(|(digest, _)| *digest)
                .collect()
        })
        .unwrap_or_default()
}

/// Hard ceiling for one platform-driven turn; beyond this the run is
/// cancelled so a wedged turn cannot pin the bridge task forever.
const PLATFORM_TURN_TIMEOUT: Duration = Duration::from_secs(30 * 60);
const RATE_PRUNE_INTERVAL: Duration = Duration::from_secs(60);
const MAX_CONCURRENT_PLATFORM_TURNS: usize = 16;
const PLATFORM_TOOL_LOG_MAX_CHARS: usize = 2_400;
const PLATFORM_REPLY_LOG_MAX_CHARS: usize = 1_200;
const MESSAGE_ACTIVITY_SCOPE_SOFT_LIMIT: usize = 512;
const MESSAGE_ACTIVITY_SEEN_LIMIT: usize = 4_096;
const MESSAGE_ACTIVITY_MAX_ID_BYTES: usize = 256;

#[derive(Clone, Default)]
pub(crate) struct MessageActivityRegistry {
    entries: Arc<Mutex<HashMap<String, Weak<MessageActivity>>>>,
}

#[derive(Clone)]
pub(crate) struct MessageActivityHandle(Arc<MessageActivity>);

struct MessageActivity {
    state: Mutex<MessageActivityState>,
}

#[derive(Default)]
struct MessageActivityState {
    total_messages: u64,
    sender_messages: HashMap<String, u64>,
    seen_messages: HashMap<String, SeenMessage>,
}

#[derive(Clone, Copy)]
struct SeenMessage {
    position: PlatformMessagePosition,
    received_at: Instant,
}

impl MessageActivityRegistry {
    pub(crate) fn observe(
        &self,
        scope: &str,
        message_id: &str,
        sender_id: &str,
        received_at: Instant,
    ) -> (MessageActivityHandle, PlatformMessagePosition, Instant) {
        let activity = {
            let mut entries = self.entries.lock().unwrap();
            if entries.len() >= MESSAGE_ACTIVITY_SCOPE_SOFT_LIMIT && !entries.contains_key(scope) {
                entries.retain(|_, activity| activity.strong_count() > 0);
            }
            match entries.get(scope).and_then(Weak::upgrade) {
                Some(activity) => activity,
                None => {
                    let activity = Arc::new(MessageActivity {
                        state: Mutex::new(MessageActivityState::default()),
                    });
                    entries.insert(scope.to_string(), Arc::downgrade(&activity));
                    activity
                }
            }
        };
        let handle = MessageActivityHandle(activity);
        let (position, received_at) = handle.observe(message_id, sender_id, received_at);
        (handle, position, received_at)
    }
}

impl MessageActivityHandle {
    fn observe(
        &self,
        message_id: &str,
        sender_id: &str,
        received_at: Instant,
    ) -> (PlatformMessagePosition, Instant) {
        let mut state = self.0.state.lock().unwrap();
        let track_id = !message_id.is_empty() && message_id.len() <= MESSAGE_ACTIVITY_MAX_ID_BYTES;
        if track_id {
            if let Some(seen) = state.seen_messages.get(message_id) {
                return (seen.position, seen.received_at);
            }
        }
        state.total_messages = state.total_messages.saturating_add(1);
        let total_messages = state.total_messages;
        let sender_messages = {
            let count = state
                .sender_messages
                .entry(sender_id.to_string())
                .or_default();
            *count = count.saturating_add(1);
            *count
        };
        let position = PlatformMessagePosition {
            total_messages,
            sender_messages,
        };
        if track_id {
            if state.seen_messages.len() >= MESSAGE_ACTIVITY_SEEN_LIMIT {
                state.seen_messages.clear();
            }
            state.seen_messages.insert(
                message_id.to_string(),
                SeenMessage {
                    position,
                    received_at,
                },
            );
        }
        (position, received_at)
    }

    fn position_for(&self, sender_id: &str) -> PlatformMessagePosition {
        let state = self.0.state.lock().unwrap();
        PlatformMessagePosition {
            total_messages: state.total_messages,
            sender_messages: state.sender_messages.get(sender_id).copied().unwrap_or(0),
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct AdaptiveResponseTargetPolicy {
    position: Option<PlatformMessagePosition>,
    received_at: Instant,
    quote_after_other_messages: u64,
    mention_after: Duration,
}

impl AdaptiveResponseTargetPolicy {
    pub(crate) fn new(
        position: Option<PlatformMessagePosition>,
        received_at: Instant,
        quote_after_other_messages: u64,
        mention_after_seconds: u64,
    ) -> Self {
        Self {
            position,
            received_at,
            quote_after_other_messages,
            mention_after: Duration::from_secs(mention_after_seconds),
        }
    }

    fn resolve(
        self,
        mut target: ResponseTarget,
        current: Option<PlatformMessagePosition>,
        now: Instant,
    ) -> Option<ResponseTarget> {
        let other_messages = self.position.zip(current).map(|(start, current)| {
            let total = current.total_messages.saturating_sub(start.total_messages);
            let same_sender = current
                .sender_messages
                .saturating_sub(start.sender_messages);
            total.saturating_sub(same_sender)
        });
        if target.quote {
            target.quote = self.quote_after_other_messages == 0
                || other_messages.is_some_and(|count| count >= self.quote_after_other_messages);
        }
        if target.mention {
            // Unknown activity preserves the original time-only mention behavior.
            target.mention = now
                .checked_duration_since(self.received_at)
                .unwrap_or_default()
                >= self.mention_after
                && other_messages.is_none_or(|count| count > 0);
        }
        target.is_effective().then_some(target)
    }
}

#[derive(Clone)]
struct PendingResponseTarget {
    target: ResponseTarget,
    policy: Option<AdaptiveResponseTargetPolicy>,
}

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
        }
    }

    async fn acquire_session_turn(
        &self,
        session_id: &str,
        limits: PlatformSessionLimits,
    ) -> std::result::Result<SessionTurnLease, SessionTurnAcquireError> {
        self.session_turn_ticket(session_id, limits).acquire().await
    }

    pub(crate) fn preempt_session_turns(&self, session_id: &str) -> SessionTurnTicket {
        let mut ticket = self.session_turn_ticket(session_id, PlatformSessionLimits::default());
        ticket.generation = ticket
            .state
            .generation
            .fetch_add(1, Ordering::AcqRel)
            .wrapping_add(1);
        ticket.exclusive = true;
        ticket.state.preempting.store(true, Ordering::Release);
        ticket
    }

    pub(crate) fn queued_session_turns(&self, session_id: &str) -> usize {
        let locks = self.session_turn_locks.lock().unwrap();
        locks
            .get(session_id)
            .and_then(Weak::upgrade)
            .map(|state| state.waiting.load(Ordering::Acquire))
            .unwrap_or(0)
    }
}

struct SessionTurnState {
    slots: Arc<tokio::sync::Semaphore>,
    gate: Arc<tokio::sync::RwLock<()>>,
    waiting: AtomicUsize,
    max_queued: usize,
    preempting: AtomicBool,
    preemption_changed: tokio::sync::Notify,
    generation: AtomicU64,
}

impl SessionTurnState {
    fn new(limits: PlatformSessionLimits) -> Self {
        Self {
            slots: Arc::new(tokio::sync::Semaphore::new(limits.running)),
            gate: Arc::new(tokio::sync::RwLock::new(())),
            waiting: AtomicUsize::new(0),
            max_queued: limits.queued,
            preempting: AtomicBool::new(false),
            preemption_changed: tokio::sync::Notify::new(),
            generation: AtomicU64::new(0),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SessionTurnAcquireError {
    Full,
    Closed,
}

pub(crate) struct SessionTurnTicket {
    states: Arc<Mutex<HashMap<String, Weak<SessionTurnState>>>>,
    session_id: String,
    state: Arc<SessionTurnState>,
    generation: u64,
    exclusive: bool,
}

impl SessionTurnTicket {
    pub(crate) async fn acquire(
        self,
    ) -> std::result::Result<SessionTurnLease, SessionTurnAcquireError> {
        let (guard, permit) = if self.exclusive {
            (
                SessionTurnGuard::Write(self.state.gate.clone().write_owned().await),
                None,
            )
        } else {
            let permit = match self.state.slots.clone().try_acquire_owned() {
                Ok(permit) => permit,
                Err(tokio::sync::TryAcquireError::Closed) => {
                    return Err(SessionTurnAcquireError::Closed)
                }
                Err(tokio::sync::TryAcquireError::NoPermits) => {
                    if self
                        .state
                        .waiting
                        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |waiting| {
                            (waiting < self.state.max_queued).then_some(waiting + 1)
                        })
                        .is_err()
                    {
                        return Err(SessionTurnAcquireError::Full);
                    }
                    let acquired = self.state.slots.clone().acquire_owned().await;
                    self.state.waiting.fetch_sub(1, Ordering::AcqRel);
                    acquired.map_err(|_| SessionTurnAcquireError::Closed)?
                }
            };
            while self.state.preempting.load(Ordering::Acquire) {
                let changed = self.state.preemption_changed.notified();
                if !self.state.preempting.load(Ordering::Acquire) {
                    break;
                }
                changed.await;
            }
            (
                SessionTurnGuard::Read(self.state.gate.clone().read_owned().await),
                Some(permit),
            )
        };
        Ok(SessionTurnLease {
            guard: Some(guard),
            permit,
            states: self.states,
            session_id: self.session_id,
            state: self.state,
            generation: self.generation,
            exclusive: self.exclusive,
        })
    }
}

enum SessionTurnGuard {
    Read(tokio::sync::OwnedRwLockReadGuard<()>),
    Write(tokio::sync::OwnedRwLockWriteGuard<()>),
}

pub(crate) struct SessionTurnLease {
    guard: Option<SessionTurnGuard>,
    permit: Option<tokio::sync::OwnedSemaphorePermit>,
    states: Arc<Mutex<HashMap<String, Weak<SessionTurnState>>>>,
    session_id: String,
    state: Arc<SessionTurnState>,
    generation: u64,
    exclusive: bool,
}

impl SessionTurnLease {
    fn is_valid(&self) -> bool {
        self.state.generation.load(Ordering::Acquire) == self.generation
    }
}

impl Drop for SessionTurnLease {
    fn drop(&mut self) {
        // Release the session before removing its registry entry. Otherwise a
        // new arrival could create a second lock during this guard's drop.
        self.guard.take();
        self.permit.take();
        if self.exclusive {
            self.state.preempting.store(false, Ordering::Release);
            self.state.preemption_changed.notify_waiters();
        }
        let mut states = self.states.lock().unwrap();
        if Arc::strong_count(&self.state) == 1
            && states
                .get(&self.session_id)
                .is_some_and(|registered| Weak::ptr_eq(registered, &Arc::downgrade(&self.state)))
        {
            states.remove(&self.session_id);
        }
    }
}

pub(crate) use assets::platform_asset;

#[derive(Clone)]
pub(crate) struct TurnProfile {
    pub(crate) active_persona: Option<String>,
    pub(crate) text_models: Option<Vec<ActiveProviderModelConfig>>,
    pub(crate) multimodal_models: Option<Vec<ActiveProviderModelConfig>>,
    pub(crate) system_context: Vec<String>,
    /// Per-message transport context (sender identity JSON, message ids, …).
    /// Rendered as a tail system message after the user turn instead of being
    /// folded into the system prompt, so the stable prefix stays byte-identical
    /// across turns (v7 Phase 2.1).
    pub(crate) turn_system_context: Vec<String>,
    /// Raw input snapshot for the memory diary (pre-plugin content); `None`
    /// keeps the agent's default of recording the turn content as-is.
    pub(crate) memory_content: Option<String>,
    pub(crate) context_images: Vec<PlatformContextImageRef>,
    pub(crate) platform: Option<Arc<PlatformTurnContext>>,
    pub(crate) image_cache_namespace: Option<String>,
    pub(crate) image_source_label: Option<String>,
    pub(crate) memory_write_enabled: bool,
    /// Structured platform history replaces ambiguous core user/assistant
    /// replay for shared conversations such as QQ groups.
    pub(crate) suppress_session_history: bool,
    /// Group overflow handling; `None` inherits the global `context` settings.
    pub(crate) group_context: Option<crate::config::PlatformGroupContextConfig>,
    pub(crate) followup: Option<Arc<PlatformFollowupRun>>,
}

impl Default for TurnProfile {
    fn default() -> Self {
        Self {
            active_persona: None,
            text_models: None,
            multimodal_models: None,
            system_context: Vec::new(),
            turn_system_context: Vec::new(),
            memory_content: None,
            context_images: Vec::new(),
            platform: None,
            image_cache_namespace: None,
            image_source_label: None,
            memory_write_enabled: true,
            suppress_session_history: false,
            group_context: None,
            followup: None,
        }
    }
}

pub(crate) struct PlatformFollowupRun {
    pub(crate) conversation: PlatformConversation,
    pub(crate) sender_id: String,
    pub(crate) context: Arc<PlatformTurnContext>,
    ingress: Arc<QueueIngressBarrier>,
    enqueue: tokio::sync::Mutex<()>,
    started: Instant,
}

impl PlatformFollowupRun {
    pub(crate) fn new(context: Arc<PlatformTurnContext>) -> Arc<Self> {
        Arc::new(Self {
            conversation: context.conversation.clone(),
            sender_id: context.sender_id.clone(),
            context,
            ingress: Arc::new(QueueIngressBarrier::default()),
            enqueue: tokio::sync::Mutex::new(()),
            started: Instant::now(),
        })
    }

    pub(crate) fn ingress(&self) -> Arc<QueueIngressBarrier> {
        self.ingress.clone()
    }

    pub(crate) fn try_reserve(&self) -> Option<QueueIngressReservation> {
        self.ingress.try_reserve()
    }

    pub(crate) async fn lock_enqueue(&self) -> tokio::sync::MutexGuard<'_, ()> {
        self.enqueue.lock().await
    }

    pub(crate) fn started(&self) -> Instant {
        self.started
    }

    pub(crate) fn close(&self) {
        self.ingress.close();
    }
}

pub(crate) struct PlatformTurnContext {
    pub(crate) conversation: PlatformConversation,
    pub(crate) sender_id: String,
    pub(crate) sender_display_name: String,
    pub(crate) is_admin: bool,
    pub(crate) config: AppConfig,
    pub(crate) paths: MiyuPaths,
    pub(crate) state_store: StateStore,
    adapter: Arc<dyn PlatformAdapter>,
    plugins: Arc<plugins::PlatformPluginRegistry>,
    config_manager: Option<Weak<Mutex<crate::web::ManagerState>>>,
    inbound_event: Option<Arc<PlatformInboundEvent>>,
    message_activity: Option<MessageActivityHandle>,
    response_target: Mutex<Option<PendingResponseTarget>>,
    group_member_cache: Mutex<HashMap<String, PlatformGroupMember>>,
    plugin_values: Mutex<BTreeMap<String, Value>>,
    delivered_image_digests: Mutex<HashSet<blake3::Hash>>,
    reply_rate_available: AtomicBool,
    pending_final_reply_suppression: AtomicBool,
    pending_prior_reply_suppression: AtomicBool,
}

impl PlatformTurnContext {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        conversation: PlatformConversation,
        sender_id: String,
        sender_display_name: String,
        is_admin: bool,
        config: AppConfig,
        paths: MiyuPaths,
        state_store: StateStore,
        adapter: Arc<dyn PlatformAdapter>,
        plugins: Arc<plugins::PlatformPluginRegistry>,
    ) -> Self {
        Self {
            conversation,
            sender_id,
            sender_display_name,
            is_admin,
            config,
            paths,
            state_store,
            adapter,
            plugins,
            config_manager: None,
            inbound_event: None,
            message_activity: None,
            response_target: Mutex::new(None),
            group_member_cache: Mutex::new(HashMap::new()),
            plugin_values: Mutex::new(BTreeMap::new()),
            delivered_image_digests: Mutex::new(HashSet::new()),
            reply_rate_available: AtomicBool::new(true),
            pending_final_reply_suppression: AtomicBool::new(false),
            pending_prior_reply_suppression: AtomicBool::new(false),
        }
    }

    pub(crate) fn with_inbound_event(mut self, event: PlatformInboundEvent) -> Self {
        self.inbound_event = Some(Arc::new(event));
        self
    }

    pub(crate) fn with_message_activity(mut self, activity: MessageActivityHandle) -> Self {
        self.message_activity = Some(activity);
        self
    }

    pub(crate) fn with_config_manager(
        mut self,
        manager: Arc<Mutex<crate::web::ManagerState>>,
    ) -> Self {
        self.config_manager = Some(Arc::downgrade(&manager));
        self
    }

    pub(crate) fn with_current_config<T>(&self, read: impl FnOnce(&AppConfig) -> T) -> T {
        match self.config_manager.as_ref().and_then(Weak::upgrade) {
            Some(manager) => read(&manager.lock().unwrap().config),
            None => read(&self.config),
        }
    }

    pub(crate) fn inbound_event(&self) -> Option<&PlatformInboundEvent> {
        self.inbound_event.as_deref()
    }

    pub(crate) fn principal(&self) -> PlatformPrincipal {
        PlatformPrincipal {
            platform: self.conversation.platform.clone(),
            account_id: self.conversation.account_id.clone(),
            user_id: self.sender_id.clone(),
        }
    }

    pub(crate) fn set_response_target(&self, target: Option<ResponseTarget>) {
        let target = target.filter(ResponseTarget::is_effective);
        let mut pending = self.response_target.lock().unwrap();
        match target {
            Some(target)
                if pending
                    .as_ref()
                    .is_some_and(|existing| existing.target == target) =>
            {
                pending.as_mut().expect("target exists").target = target;
            }
            Some(target) => {
                *pending = Some(PendingResponseTarget {
                    target,
                    policy: None,
                });
            }
            None => *pending = None,
        }
    }

    pub(crate) fn set_adaptive_response_target(
        &self,
        target: Option<ResponseTarget>,
        policy: AdaptiveResponseTargetPolicy,
    ) {
        let mut pending = self.response_target.lock().unwrap();
        let explicit_mentions = pending
            .as_ref()
            .map(|pending| pending.target.explicit_mention_user_ids.clone())
            .filter(|mentions| !mentions.is_empty());
        let target = target.filter(ResponseTarget::is_effective);
        *pending = match (target, explicit_mentions) {
            (Some(mut target), Some(mentions)) => {
                target.mention = false;
                target.explicit_mention_user_ids = mentions;
                Some(PendingResponseTarget {
                    target,
                    policy: Some(policy),
                })
            }
            (Some(target), None) => Some(PendingResponseTarget {
                target,
                policy: Some(policy),
            }),
            (None, Some(mentions)) => Some(PendingResponseTarget {
                target: ResponseTarget {
                    message_id: String::new(),
                    user_id: String::new(),
                    quote: false,
                    mention: false,
                    explicit_mention_user_ids: mentions,
                },
                policy: None,
            }),
            (None, None) => None,
        };
    }

    pub(crate) fn response_target(&self) -> Option<ResponseTarget> {
        self.response_target
            .lock()
            .unwrap()
            .as_ref()
            .map(|pending| pending.target.clone())
    }

    pub(crate) fn set_explicit_response_mentions(&self, user_ids: Vec<String>) {
        if user_ids.is_empty() {
            return;
        }
        let mut pending = self.response_target.lock().unwrap();
        if let Some(pending) = pending.as_mut() {
            pending.target.mention = false;
            pending.target.explicit_mention_user_ids = user_ids;
        } else {
            *pending = Some(PendingResponseTarget {
                target: ResponseTarget {
                    message_id: String::new(),
                    user_id: String::new(),
                    quote: false,
                    mention: false,
                    explicit_mention_user_ids: user_ids,
                },
                policy: None,
            });
        }
    }

    pub(crate) fn set_plugin_value(&self, key: impl Into<String>, value: Value) {
        self.plugin_values.lock().unwrap().insert(key.into(), value);
    }

    pub(crate) fn remove_plugin_value(&self, key: &str) {
        self.plugin_values.lock().unwrap().remove(key);
    }

    pub(crate) fn plugin_value(&self, key: &str) -> Option<Value> {
        self.plugin_values.lock().unwrap().get(key).cloned()
    }

    pub(crate) fn set_reply_rate_available(&self, available: bool) {
        self.reply_rate_available
            .store(available, Ordering::Release);
    }

    pub(crate) fn reply_rate_available(&self) -> bool {
        self.reply_rate_available.load(Ordering::Acquire)
    }

    pub(crate) fn plugin_enabled(&self, id: &str, default_enabled: bool) -> bool {
        self.config
            .platforms
            .qq
            .plugins
            .get(id)
            .and_then(|plugin| plugin.enabled)
            .unwrap_or(default_enabled)
    }

    pub(crate) fn host_tools_allowed(&self) -> bool {
        if self.is_admin {
            return true;
        }
        self.conversation.kind == ConversationKind::Private
            && self.config.platforms.qq.allow_non_admin_host_tools
            && self.sender_id.parse::<i64>().ok().is_some_and(|sender| {
                self.config
                    .platforms
                    .qq
                    .private_chats
                    .whitelist
                    .contains(&sender)
                    || access_control::has_dynamic_access(
                        &self.state_store,
                        &self.conversation.account_id,
                        access_control::AccessPermission::PrivateWhitelist,
                        &self.sender_id,
                    )
            })
    }

    pub(crate) async fn handle_command(&self, text: &str) -> Option<OutboundMessage> {
        self.plugins.handle_command(self, text).await
    }

    pub(crate) async fn prepare_turn(&self, content: String) -> plugins::PlatformTurnInput {
        let mut input = plugins::PlatformTurnInput {
            memory_content: content.clone(),
            content,
            system_context: Vec::new(),
            turn_system_context: Vec::new(),
            context_images: Vec::new(),
        };
        self.plugins.before_turn(self, &mut input).await;
        input
    }

    pub(crate) async fn observe_inbound(&self, event: &PlatformInboundEvent) {
        self.plugins.observe_inbound(self, event).await;
    }

    pub(crate) fn accept_followup(&self, event: &PlatformInboundEvent) {
        self.plugins.accept_followup(self, event);
    }

    pub(crate) fn preempt_inbound(&self, event: &PlatformInboundEvent) -> bool {
        self.plugins.preempt_inbound(self, event)
    }

    pub(crate) async fn confirm_supersede(&self, event: &PlatformInboundEvent) {
        self.plugins.confirm_supersede(self, event).await;
    }

    pub(crate) fn turn_is_superseded(&self) -> bool {
        self.plugins.turn_is_superseded(self)
    }

    pub(crate) fn turn_started(&self, cancel: tokio::sync::watch::Sender<bool>) {
        self.plugins.turn_started(self, cancel);
    }

    pub(crate) async fn after_turn_aborted(&self) {
        self.plugins.after_turn_aborted(self).await;
    }

    pub(crate) async fn decide_trigger(
        &self,
        event: &PlatformInboundEvent,
        decision: &mut TriggerDecision,
    ) {
        self.plugins.decide_trigger(self, event, decision).await;
    }

    pub(crate) async fn after_session_reset(&self) -> Result<()> {
        self.plugins.after_session_reset(self).await
    }

    pub(crate) async fn send(&self, mut message: OutboundMessage) -> Result<SendReceipt> {
        if matches!(
            message.origin,
            OutboundOrigin::FinalReply | OutboundOrigin::IntermediateReply | OutboundOrigin::Tool
        ) && message_is_parenthetical_only(&message)
        {
            tracing::info!(
                platform = %self.conversation.platform,
                conversation_kind = self.conversation.kind.as_str(),
                conversation_id = %self.conversation.conversation_id,
                "{}",
                crate::i18n::text(
                    "suppressed a parenthetical-only model reply",
                    "已抑制仅含括号内容的模型回复",
                )
            );
            return Ok(SendReceipt::default());
        }
        let reserved_target = if message.response_target.is_none()
            && matches!(
                message.origin,
                OutboundOrigin::FinalReply | OutboundOrigin::Tool
            ) {
            self.response_target.lock().unwrap().take()
        } else {
            None
        };
        if let Some(target) = reserved_target.as_ref() {
            message.response_target = Some(target.target.clone());
        }
        let mut prepared = self.plugins.before_send(self, message).await;
        if let Some(target) = reserved_target.as_ref() {
            let current = self
                .message_activity
                .as_ref()
                .map(|activity| activity.position_for(&target.target.user_id));
            let resolved = target
                .policy
                .and_then(|policy| policy.resolve(target.target.clone(), current, Instant::now()))
                .or_else(|| target.policy.is_none().then(|| target.target.clone()));
            apply_resolved_response_target(
                &mut prepared.primary,
                &target.target,
                resolved.as_ref(),
            );
            if let Some(fallback) = prepared.fallback.as_mut() {
                apply_resolved_response_target(fallback, &target.target, resolved.as_ref());
            }
        }
        let primary = prepared.primary;
        let delivered = match self.adapter.send(primary.clone()).await {
            Ok(receipt) => Ok((primary, receipt, true)),
            Err(error) => {
                let (partially_delivered, response_target_delivered) =
                    self.record_partial_delivery(&error);
                match (partially_delivered, prepared.fallback) {
                    (true, _) => {
                        // `miyu::qq` and not the module default: these are
                        // delivery outcomes an operator reads next to the
                        // "回复已投递" lines, and every other target is filtered
                        // to ERROR unless MIYU_LOG says otherwise (see
                        // `logging::init`), which kept this whole branch
                        // invisible in the QQ log.
                        tracing::warn!(
                            target: "miyu::qq",
                            error = %error,
                            "{}",
                            crate::i18n::text(
                                "platform message partially succeeded; skipped the full fallback to avoid duplicate delivery",
                                "平台消息部分发送成功；为避免重复投递，已跳过完整回退消息",
                            )
                        );
                        Err((error, response_target_delivered))
                    }
                    (false, Some(fallback)) => {
                        tracing::warn!(target: "miyu::qq", error = %error, "{}", crate::i18n::text("transformed platform message failed; sending fallback", "转换后的平台消息发送失败；正在发送回退消息"));
                        match self.adapter.send(fallback.clone()).await {
                            Ok(receipt) => Ok((fallback, receipt, false)),
                            Err(error) => {
                                let (_, response_target_delivered) =
                                    self.record_partial_delivery(&error);
                                Err((error, response_target_delivered))
                            }
                        }
                    }
                    (false, None) => Err((error, false)),
                }
            }
        };
        let (delivered_message, receipt, transformed_primary_succeeded) = match delivered {
            Ok(delivered) => delivered,
            Err((error, response_target_delivered)) => {
                if !response_target_delivered {
                    if let Some(target) = reserved_target {
                        self.restore_response_target(target);
                    }
                }
                return Err(error);
            }
        };
        self.record_delivered_images(&receipt);
        self.plugins
            .after_send(self, &delivered_message, &receipt)
            .await;
        for message in prepared.after_success {
            let history_text = outbound_text_for_history(&message);
            match self.adapter.send(message).await {
                Ok(receipt) => {
                    self.record_delivered_images(&receipt);
                    let message_id = receipt
                        .message_ids
                        .first()
                        .map(String::as_str)
                        .unwrap_or("");
                    self.plugins
                        .record_external_bot_message(self, message_id, &history_text)
                        .await;
                }
                Err(error) => {
                    let _ = self.record_partial_delivery(&error);
                    tracing::warn!(target: "miyu::qq", error = %error, "{}", crate::i18n::text("platform plugin follow-up send failed", "平台插件后续消息发送失败"));
                }
            }
        }
        if prepared.suppress_final_reply
            && transformed_primary_succeeded
            && delivered_message.origin == OutboundOrigin::Tool
        {
            self.pending_final_reply_suppression
                .store(true, Ordering::Release);
            if prepared.suppress_prior_reply {
                self.pending_prior_reply_suppression
                    .store(true, Ordering::Release);
            }
        }
        Ok(receipt)
    }

    pub(crate) async fn send_bypass_plugins(
        &self,
        message: OutboundMessage,
    ) -> Result<SendReceipt> {
        let history_text = outbound_text_for_history(&message);
        match self.adapter.send(message).await {
            Ok(receipt) => {
                self.record_delivered_images(&receipt);
                let message_id = receipt
                    .message_ids
                    .first()
                    .map(String::as_str)
                    .unwrap_or("");
                self.plugins
                    .record_external_bot_message(self, message_id, &history_text)
                    .await;
                Ok(receipt)
            }
            Err(error) => {
                let _ = self.record_partial_delivery(&error);
                Err(error)
            }
        }
    }

    fn record_delivered_images(&self, receipt: &SendReceipt) {
        if receipt.image_digests.is_empty() {
            return;
        }
        self.delivered_image_digests
            .lock()
            .unwrap()
            .extend(receipt.image_digests.iter().copied());
        record_recent_conversation_images(&self.conversation.scope_key(), &receipt.image_digests);
    }

    fn record_partial_delivery(&self, error: &anyhow::Error) -> (bool, bool) {
        let Some(partial) = error.downcast_ref::<PartialSendError>() else {
            return (false, false);
        };
        self.record_delivered_images(partial.receipt());
        (
            partial.receipt().has_delivery(),
            partial.receipt().response_target_delivered,
        )
    }

    fn restore_response_target(&self, target: PendingResponseTarget) {
        let mut available = self.response_target.lock().unwrap();
        match available.as_mut() {
            Some(current)
                if current.target.explicit_mention_user_ids.is_empty()
                    && !target.target.explicit_mention_user_ids.is_empty() =>
            {
                current.target.mention = false;
                current.target.explicit_mention_user_ids = target.target.explicit_mention_user_ids;
            }
            Some(_) => {}
            None => *available = Some(target),
        }
    }

    pub(crate) fn delivered_image_digests(&self) -> HashSet<blake3::Hash> {
        let mut digests = self.delivered_image_digests.lock().unwrap().clone();
        digests.extend(recent_conversation_images(&self.conversation.scope_key()));
        digests
    }

    pub(crate) async fn bot_display_name(&self) -> Result<String> {
        self.adapter.bot_display_name().await
    }

    pub(crate) async fn bot_send_availability(&self) -> types::BotSendAvailability {
        match self.adapter.bot_send_availability().await {
            Ok(availability) => availability,
            Err(error) => {
                tracing::debug!(error = %error, "{}", crate::i18n::text("platform bot send availability lookup failed", "平台机器人发送可用性查询失败"));
                types::BotSendAvailability::Unknown
            }
        }
    }

    pub(crate) async fn set_message_reaction(
        &self,
        message_id: &str,
        reaction_id: &str,
        active: bool,
    ) -> Result<()> {
        self.adapter
            .set_message_reaction(message_id, reaction_id, active)
            .await
    }

    pub(crate) fn schedule_message_reaction_removal(
        &self,
        message_id: String,
        reaction_id: String,
        delay: Duration,
    ) -> tokio::task::AbortHandle {
        let adapter = self.adapter.clone();
        tokio::spawn(async move {
            tokio::time::sleep(delay).await;
            if let Err(error) = adapter
                .set_message_reaction(&message_id, &reaction_id, false)
                .await
            {
                tracing::debug!(
                    error = %error,
                    %message_id,
                    %reaction_id,
                    "{}",
                    crate::i18n::text(
                        "expired platform reaction could not be removed",
                        "无法移除已过期的平台表情回应",
                    )
                );
            }
        })
        .abort_handle()
    }

    pub(crate) async fn message_info(
        &self,
        message_id: &str,
    ) -> Result<Option<PlatformMessageInfo>> {
        self.adapter.message_info(message_id).await
    }

    pub(crate) fn message_images_task(
        &self,
        message_id: String,
    ) -> futures_util::future::BoxFuture<'static, Result<Vec<PlatformImageData>>> {
        let adapter = self.adapter.clone();
        Box::pin(async move { adapter.message_images(&message_id).await })
    }

    pub(crate) async fn group_members(&self) -> Result<Vec<PlatformGroupMember>> {
        let members = self.adapter.group_members().await?;
        self.group_member_cache.lock().unwrap().extend(
            members
                .iter()
                .cloned()
                .map(|member| (member.user_id.clone(), member)),
        );
        Ok(members)
    }

    pub(crate) async fn group_member(&self, user_id: &str) -> Result<Option<PlatformGroupMember>> {
        if let Some(member) = self
            .group_member_cache
            .lock()
            .unwrap()
            .get(user_id)
            .cloned()
        {
            return Ok(Some(member));
        }
        let member = self.adapter.group_member(user_id).await?;
        if let Some(member) = member.as_ref() {
            self.group_member_cache
                .lock()
                .unwrap()
                .insert(member.user_id.clone(), member.clone());
        }
        Ok(member)
    }

    /// Membership as the server sees it *now*, skipping both the per-turn cache
    /// and the platform's roster cache. Destructive actions validate through
    /// this so a member who already left is refused here instead of failing
    /// deep inside the bridge.
    pub(crate) async fn group_member_fresh(
        &self,
        user_id: &str,
    ) -> Result<Option<PlatformGroupMember>> {
        let member = self.adapter.group_member_fresh(user_id).await?;
        let mut cache = self.group_member_cache.lock().unwrap();
        match member.as_ref() {
            Some(member) => {
                cache.insert(member.user_id.clone(), member.clone());
            }
            None => {
                cache.remove(user_id);
            }
        }
        Ok(member)
    }

    /// Drops a member from the per-turn cache — used when a leave/kick notice
    /// arrives so later lookups in the same turn cannot resurrect them.
    pub(crate) fn forget_group_member(&self, user_id: &str) {
        self.group_member_cache.lock().unwrap().remove(user_id);
    }

    pub(crate) async fn bot_group_role(&self) -> types::BotGroupRole {
        self.adapter
            .bot_group_role()
            .await
            .unwrap_or(types::BotGroupRole::Unknown)
    }

    pub(crate) async fn delete_message(&self, message_id: &str) -> Result<()> {
        self.adapter.delete_message(message_id).await
    }

    pub(crate) async fn set_group_ban(&self, user_id: &str, duration_seconds: u64) -> Result<()> {
        self.adapter.set_group_ban(user_id, duration_seconds).await
    }

    pub(crate) async fn set_group_kick(
        &self,
        user_id: &str,
        reject_add_request: bool,
    ) -> Result<()> {
        self.adapter
            .set_group_kick(user_id, reject_add_request)
            .await
    }

    pub(crate) async fn set_group_special_title(
        &self,
        user_id: &str,
        special_title: &str,
        duration_seconds: i64,
    ) -> Result<()> {
        self.adapter
            .set_group_special_title(user_id, special_title, duration_seconds)
            .await
    }

    pub(crate) async fn record_external_bot_message(&self, message_id: &str, text: &str) {
        self.plugins
            .record_external_bot_message(self, message_id, text)
            .await;
    }

    pub(crate) fn take_final_reply_suppression(&self) -> bool {
        let suppress = self
            .pending_final_reply_suppression
            .swap(false, Ordering::AcqRel);
        self.pending_prior_reply_suppression
            .store(false, Ordering::Release);
        suppress
    }

    pub(crate) fn take_final_reply_suppression_start(&self, text_len: usize) -> Option<usize> {
        if !self
            .pending_final_reply_suppression
            .swap(false, Ordering::AcqRel)
        {
            return None;
        }
        let suppress_prior = self
            .pending_prior_reply_suppression
            .swap(false, Ordering::AcqRel);
        Some(if suppress_prior { 0 } else { text_len })
    }
}

fn apply_resolved_response_target(
    message: &mut OutboundMessage,
    original: &ResponseTarget,
    resolved: Option<&ResponseTarget>,
) {
    if message.response_target.as_ref() == Some(original) {
        message.response_target = resolved.cloned();
    }
}

fn message_is_parenthetical_only(message: &OutboundMessage) -> bool {
    let OutboundBody::Segments(segments) = &message.body else {
        return false;
    };
    let mut text = String::new();
    for segment in segments {
        match segment {
            OutboundSegment::Markdown(part) | OutboundSegment::Text(part) => text.push_str(part),
            OutboundSegment::Mention(_) => {}
            OutboundSegment::ImageBytes { .. }
            | OutboundSegment::ImagePath { .. }
            | OutboundSegment::FilePath { .. } => return false,
        }
    }
    let text = text.trim();
    if text.is_empty() || !text.starts_with('（') || !text.ends_with('）') {
        return false;
    }
    let mut depth = 0_u32;
    let mut chars = text.chars().peekable();
    while let Some(character) = chars.next() {
        match character {
            '（' => depth = depth.saturating_add(1),
            '）' => {
                let Some(next_depth) = depth.checked_sub(1) else {
                    return false;
                };
                depth = next_depth;
                if depth == 0 && chars.peek().is_some() {
                    return false;
                }
            }
            _ if depth == 0 => return false,
            _ => {}
        }
    }
    depth == 0
}

fn outbound_text_for_history(message: &OutboundMessage) -> String {
    fn append(parts: &mut Vec<String>, segments: &[OutboundSegment]) {
        for segment in segments {
            match segment {
                OutboundSegment::Markdown(text) | OutboundSegment::Text(text) => {
                    if !text.trim().is_empty() {
                        parts.push(text.clone());
                    }
                }
                OutboundSegment::Mention(user_id) => parts.push(format!("@{user_id}")),
                OutboundSegment::ImageBytes { .. }
                | OutboundSegment::ImagePath { .. }
                | OutboundSegment::FilePath { .. } => {}
            }
        }
    }

    let mut parts = Vec::new();
    match &message.body {
        OutboundBody::Segments(segments) => append(&mut parts, segments),
        OutboundBody::Forward(nodes) => {
            for node in nodes {
                append(&mut parts, &node.segments);
            }
        }
    }
    parts.join("\n").trim().to_string()
}

pub(crate) fn register_platform_tools(
    registry: &mut crate::tools::ToolRegistry,
    context: Arc<PlatformTurnContext>,
) {
    tool::register(registry, context.clone());
    context.plugins.register_tools(registry, context.clone());
}

/// Per-conversation fixed-window rate limiter shared by all platforms.
pub(crate) struct RateWindow {
    last_prune: Instant,
    conversations: HashMap<String, SenderWindow>,
}

struct SenderWindow {
    window_start: Instant,
    window: Duration,
    count: u32,
    notified: bool,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum RateDecision {
    Allow,
    /// Over quota and already warned this window.
    DropSilently,
    /// Over quota for the first time this window: send one notice.
    DropWithNotice,
}

impl RateWindow {
    pub(crate) fn new() -> Self {
        Self {
            last_prune: Instant::now(),
            conversations: HashMap::new(),
        }
    }

    pub(crate) fn check(&mut self, conversation: &str, limit: PlatformRateLimit) -> RateDecision {
        self.check_at(Instant::now(), conversation, limit)
    }

    pub(crate) fn available(&mut self, conversation: &str, limit: PlatformRateLimit) -> bool {
        self.available_at(Instant::now(), conversation, limit)
    }

    fn available_at(&mut self, now: Instant, conversation: &str, limit: PlatformRateLimit) -> bool {
        self.prune_at(now);
        if limit.max_messages == 0 {
            return true;
        }
        let configured_window = Duration::from_secs(u64::from(limit.window_seconds));
        self.conversations.get(conversation).is_none_or(|entry| {
            entry.window != configured_window
                || now.duration_since(entry.window_start) >= configured_window
                || entry.count < limit.max_messages
        })
    }

    fn check_at(
        &mut self,
        now: Instant,
        conversation: &str,
        limit: PlatformRateLimit,
    ) -> RateDecision {
        self.prune_at(now);
        if limit.max_messages == 0 {
            return RateDecision::Allow;
        }
        let configured_window = Duration::from_secs(u64::from(limit.window_seconds));
        let entry = self
            .conversations
            .entry(conversation.to_string())
            .or_insert(SenderWindow {
                window_start: now,
                window: configured_window,
                count: 0,
                notified: false,
            });
        if entry.window != configured_window
            || now.duration_since(entry.window_start) >= configured_window
        {
            *entry = SenderWindow {
                window_start: now,
                window: configured_window,
                count: 0,
                notified: false,
            };
        }
        if entry.count < limit.max_messages {
            entry.count += 1;
            return RateDecision::Allow;
        }
        if entry.notified {
            return RateDecision::DropSilently;
        }
        entry.notified = true;
        RateDecision::DropWithNotice
    }

    fn prune_at(&mut self, now: Instant) {
        if now.duration_since(self.last_prune) >= RATE_PRUNE_INTERVAL {
            self.last_prune = now;
            self.conversations.retain(|_, entry| {
                now.checked_duration_since(entry.window_start)
                    .is_some_and(|elapsed| elapsed < entry.window)
            });
        }
    }
}

/// Finds or creates the dedicated user session for a stable external
/// conversation identity. The visible session name can be edited freely;
/// routing never depends on it after the binding has been created.
pub(crate) fn resolve_platform_session(
    state: &DaemonState,
    conversation: &PlatformConversation,
    persona: &str,
    participant_id: Option<String>,
    name: &str,
    legacy_name: Option<&str>,
) -> Result<Arc<str>> {
    let key = PlatformSessionBindingKey {
        platform: conversation.platform.clone(),
        account_id: conversation.account_id.clone(),
        conversation_kind: conversation.kind.as_str().to_string(),
        conversation_id: conversation.conversation_id.clone(),
        participant_id,
        persona: persona.to_string(),
    };
    if let Some(session_id) = state.state_store.find_platform_session_binding(&key)? {
        let record = state
            .state_store
            .session_record(&session_id)?
            .with_context(|| format!("bound platform session is missing: {session_id}"))?;
        if record.archived {
            state
                .state_store
                .set_session_archived(&record.session_id, false)?;
        }
        return Ok(record.session_id.into());
    }

    // Adopt the pre-binding name only when it identifies exactly one session.
    // If multiple bot accounts race for the same legacy name, the first bind
    // wins and every later account gets a fresh, correctly isolated session.
    let mut candidates = state
        .state_store
        .list_sessions(&persona, true)?
        .into_iter()
        .filter(|overview| {
            overview.record.kind == "user"
                && (overview.record.name == name
                    || legacy_name.is_some_and(|legacy| overview.record.name == legacy))
        })
        .map(|overview| overview.record)
        .collect::<Vec<_>>();
    if candidates.len() == 1 {
        let record = candidates.pop().expect("length checked");
        match state
            .state_store
            .claim_platform_session(&key, &record.session_id)
        {
            Ok(session_id) if session_id == record.session_id => {
                if record.archived {
                    state
                        .state_store
                        .set_session_archived(&record.session_id, false)?;
                }
                return Ok(record.session_id.into());
            }
            Ok(session_id) => return Ok(session_id.into()),
            Err(error) => {
                tracing::warn!(error = %error, session_id = %record.session_id, "{}", crate::i18n::text("legacy platform session could not be bound", "无法绑定旧版平台会话"));
                if let Some(session_id) = state.state_store.find_platform_session_binding(&key)? {
                    return Ok(session_id.into());
                }
            }
        }
    } else if candidates.len() > 1 {
        tracing::warn!(
            name,
            "{}",
            crate::i18n::text(
                "legacy platform session name is ambiguous; creating a new session",
                "旧版平台会话名称存在歧义；正在创建新会话",
            )
        );
    }

    let (record, created) = state
        .state_store
        .create_or_get_platform_session(&key, name)?;
    if record.archived {
        state
            .state_store
            .set_session_archived(&record.session_id, false)?;
    }
    if created {
        state.events.publish(
            "session.created",
            serde_json::json!({
                "session_id": record.session_id,
                "name": record.name,
                "platform": conversation.platform,
                "account_id": conversation.account_id,
                "conversation_kind": conversation.kind.as_str(),
                "conversation_id": conversation.conversation_id,
            }),
        );
    }
    Ok(record.session_id.into())
}

pub(crate) struct TurnOutcome {
    pub(crate) run_id: String,
    pub(crate) text: String,
    pub(crate) provider_id: Option<String>,
    pub(crate) model: Option<String>,
    /// Image asset ids published during the turn (`tool.image` events);
    /// bridges load the bytes and re-send them platform-natively.
    pub(crate) image_assets: Vec<String>,
    /// Byte ranges produced after confirmed direct long-image tool sends.
    /// Direct-send acknowledgements are removed from the final fallback text.
    pub(crate) suppressed_reply_ranges: Vec<(usize, usize)>,
    /// The last response segment was delivered by a successful direct tool
    /// send, so an otherwise empty platform reply must not add a placeholder.
    pub(crate) final_reply_already_sent: bool,
}

#[derive(Default)]
struct ReplySuppression {
    ranges: Vec<(usize, usize)>,
    open_at: Option<usize>,
    final_reply_already_sent: bool,
}

impl ReplySuppression {
    fn direct_send_succeeded(&mut self, text_len: usize) {
        self.open_at = Some(
            self.open_at
                .map_or(text_len, |existing| existing.min(text_len)),
        );
        self.final_reply_already_sent = true;
    }

    fn model_started(&mut self) {
        self.ranges.clear();
        // A direct tool send answers the same prompt across its model
        // continuation, so suppress that continuation from its first byte.
        self.open_at = self.final_reply_already_sent.then_some(0);
    }

    fn queued_prompt_consumed(&mut self) {
        self.ranges.clear();
        self.open_at = None;
        self.final_reply_already_sent = false;
    }

    fn finish(mut self, text_len: usize) -> (Vec<(usize, usize)>, bool) {
        self.close_range(text_len);
        (self.ranges, self.final_reply_already_sent)
    }

    fn close_range(&mut self, text_len: usize) {
        if let Some(start) = self.open_at.take() {
            if start < text_len {
                self.ranges.push((start, text_len));
            }
        }
    }

    /// Ranges to cut when the current round's text is flushed mid-turn as an
    /// intermediate reply. Leaves the state untouched so the `model_started`
    /// reset that follows keeps its existing semantics.
    fn round_ranges(&self, text_len: usize) -> Vec<(usize, usize)> {
        let mut ranges = self.ranges.clone();
        if let Some(start) = self.open_at {
            if start < text_len {
                ranges.push((start, text_len));
            }
        }
        ranges
    }
}

pub(crate) fn cut_suppressed_ranges(text: &str, ranges: &[(usize, usize)]) -> String {
    if ranges.is_empty() {
        return text.to_string();
    }
    let mut result = String::with_capacity(text.len());
    let mut cursor = 0;
    for &(start, end) in ranges {
        let start = start.clamp(cursor, text.len());
        let end = end.clamp(start, text.len());
        let (Some(prefix), Some(_suppressed)) = (text.get(cursor..start), text.get(start..end))
        else {
            continue;
        };
        result.push_str(prefix);
        cursor = end;
    }
    if let Some(suffix) = text.get(cursor..) {
        result.push_str(suffix);
    }
    result
}

fn start_model_reply(text: &mut String, suppression: &mut ReplySuppression) {
    text.clear();
    suppression.model_started();
}

/// Sends the just-finished model round as its own platform message. The
/// round's direct-send suppression ranges still apply, so text a tool already
/// delivered is not repeated.
async fn flush_intermediate_reply(
    context: &PlatformTurnContext,
    text: &str,
    suppression: &ReplySuppression,
) {
    if context.turn_is_superseded() {
        return;
    }
    let visible = cut_suppressed_ranges(text, &suppression.round_ranges(text.len()));
    let visible = visible.trim();
    if visible.is_empty() {
        return;
    }
    match context
        .send(OutboundMessage::markdown(
            OutboundOrigin::IntermediateReply,
            visible.to_string(),
        ))
        .await
    {
        Ok(_) => tracing::info!(
            target: "miyu::qq",
            chars = visible.chars().count(),
            "{}",
            crate::i18n::text(
                "sent an intermediate platform reply",
                "已发送平台中间消息",
            )
        ),
        Err(error) => tracing::warn!(
            target: "miyu::qq",
            error = %error,
            "{}",
            crate::i18n::text(
                "sending an intermediate platform reply failed",
                "发送平台中间消息失败",
            )
        ),
    }
}

fn format_platform_tool_payload(payload: &str) -> String {
    format_platform_tool_payload_for(payload, crate::i18n::locale())
}

fn format_platform_tool_payload_for(payload: &str, locale: Locale) -> String {
    let sanitized = sanitize_platform_log_text(payload.trim());
    let text = sanitized.as_str();
    if text.chars().count() > PLATFORM_TOOL_LOG_MAX_CHARS {
        return truncate_platform_tool_log(text, locale);
    }
    let formatted = serde_json::from_str::<Value>(text)
        .ok()
        .and_then(|value| serde_json::to_string_pretty(&value).ok())
        .unwrap_or_else(|| text.to_string());
    truncate_platform_tool_log(&formatted, locale)
}

fn truncate_platform_tool_log(text: &str, locale: Locale) -> String {
    truncate_platform_log(text, PLATFORM_TOOL_LOG_MAX_CHARS, locale)
}

fn truncate_platform_reply_log(text: &str) -> String {
    truncate_platform_reply_log_for(text, crate::i18n::locale())
}

fn truncate_platform_reply_log_for(text: &str, locale: Locale) -> String {
    sanitize_platform_log_text(&truncate_platform_log(
        text,
        PLATFORM_REPLY_LOG_MAX_CHARS,
        locale,
    ))
}

fn sanitize_platform_log_text(text: &str) -> String {
    let mut sanitized = String::with_capacity(text.len());
    for character in text.chars() {
        match character {
            '\n' | '\t' => sanitized.push(character),
            character if character.is_control() => sanitized.extend(character.escape_default()),
            character => sanitized.push(character),
        }
    }
    sanitized
}

fn truncate_platform_log(text: &str, max_chars: usize, locale: Locale) -> String {
    let total = text.chars().count();
    if total <= max_chars {
        return text.to_string();
    }
    let omitted = total - max_chars;
    format!(
        "{}\n{}",
        text.chars().take(max_chars).collect::<String>(),
        if locale == Locale::Zh {
            format!("... 已截断 {omitted} 字符 ...")
        } else {
            format!("... truncated {omitted} characters ...")
        }
    )
}

fn format_platform_final_reply_log(
    outcome: &TurnOutcome,
    context: &PlatformTurnContext,
    reply_text: &str,
    image_count: usize,
) -> String {
    format_platform_final_reply_log_for(
        outcome,
        context,
        reply_text,
        image_count,
        crate::i18n::locale(),
    )
}

fn format_platform_final_reply_log_for(
    outcome: &TurnOutcome,
    context: &PlatformTurnContext,
    reply_text: &str,
    image_count: usize,
    locale: Locale,
) -> String {
    let endpoint = match (
        outcome
            .provider_id
            .as_deref()
            .filter(|value| !value.is_empty()),
        outcome.model.as_deref().filter(|value| !value.is_empty()),
    ) {
        (Some(provider), Some(model)) => format!("{provider} / {model}"),
        (Some(provider), None) => provider.to_string(),
        (None, Some(model)) => model.to_string(),
        (None, None) => text_for(locale, "unknown", "未知").to_string(),
    };
    let endpoint = sanitize_platform_log_text(&endpoint);
    let body = if reply_text.trim().is_empty() {
        if outcome.final_reply_already_sent {
            text_for(
                locale,
                "[reply was sent directly by a tool]",
                "[回复已由工具直接发送]",
            )
            .to_string()
        } else if image_count > 0 {
            if locale == Locale::Zh {
                format!("[无文本，发送 {image_count} 张图片]")
            } else {
                format!("[no text; sent {image_count} images]")
            }
        } else {
            text_for(locale, "[empty reply]", "[空回复]").to_string()
        }
    } else {
        truncate_platform_reply_log_for(reply_text.trim(), locale)
    };
    let conversation_kind = match (locale, context.conversation.kind) {
        (Locale::Zh, ConversationKind::Group) => "群聊",
        (Locale::Zh, ConversationKind::Private) => "私聊",
        (_, kind) => kind.as_str(),
    };
    if locale == Locale::Zh {
        format!(
            "【AI 最终回复】\n运行：{}\n会话：{} {}（机器人账号 {}）\n模型：{}\n内容：\n{}",
            outcome.run_id,
            conversation_kind,
            context.conversation.conversation_id,
            context.conversation.account_id,
            endpoint,
            body
        )
    } else {
        format!(
            "[AI final reply]\nRun: {}\nConversation: {} {} (bot account {})\nModel: {}\nContent:\n{}",
            outcome.run_id,
            conversation_kind,
            context.conversation.conversation_id,
            context.conversation.account_id,
            endpoint,
            body
        )
    }
}

fn format_platform_tool_name(name: &str, display_name: Option<&str>) -> String {
    display_name
        .filter(|display_name| *display_name != name)
        .map(sanitize_platform_tool_label)
        .unwrap_or_else(|| sanitize_platform_tool_label(name))
}

fn sanitize_platform_tool_label(value: &str) -> String {
    let compact = sanitize_platform_log_text(value)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if compact.is_empty() {
        "unknown".to_string()
    } else {
        compact.chars().take(128).collect()
    }
}

fn format_platform_tool_started_log(run_id: &str, data: &Value) -> String {
    format_platform_tool_started_log_for(run_id, data, crate::i18n::locale())
}

fn format_platform_tool_started_log_for(run_id: &str, data: &Value, locale: Locale) -> String {
    let tool_id = data
        .get("tool_id")
        .and_then(Value::as_str)
        .unwrap_or_else(|| text_for(locale, "unknown", "未知"));
    let name = data
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or_else(|| text_for(locale, "unknown", "未知"));
    let display_name = data.get("display_name").and_then(Value::as_str);
    let arguments = data
        .get("arguments")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let tool_name = sanitize_platform_tool_label(name);
    let display_name = format_platform_tool_name(name, display_name);
    let arguments = format_platform_tool_payload_for(arguments, locale);
    if locale == Locale::Zh {
        let mut lines = vec![
            format!("【工具：{tool_name}】"),
            format!("运行：{run_id}"),
            format!("调用 ID：{tool_id}"),
        ];
        if display_name != tool_name {
            lines.push(format!("显示名称：{display_name}"));
        }
        lines.push(format!("参数：\n{arguments}"));
        lines.join("\n")
    } else {
        let mut lines = vec![
            format!("[Tool: {tool_name}]"),
            format!("Run: {run_id}"),
            format!("Call ID: {tool_id}"),
        ];
        if display_name != tool_name {
            lines.push(format!("Display name: {display_name}"));
        }
        lines.push(format!("Arguments:\n{arguments}"));
        lines.join("\n")
    }
}

fn format_platform_tool_finished_log(run_id: &str, data: &Value) -> String {
    format_platform_tool_finished_log_for(run_id, data, crate::i18n::locale())
}

fn format_platform_tool_finished_log_for(run_id: &str, data: &Value, locale: Locale) -> String {
    let tool_id = data
        .get("tool_id")
        .and_then(Value::as_str)
        .unwrap_or_else(|| text_for(locale, "unknown", "未知"));
    let name = data
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or_else(|| text_for(locale, "unknown", "未知"));
    let display_name = data.get("display_name").and_then(Value::as_str);
    let ok = data.get("ok").and_then(Value::as_bool).unwrap_or(false);
    let output = data
        .get("output")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let tool_name = sanitize_platform_tool_label(name);
    let display_name = format_platform_tool_name(name, display_name);
    let output = format_platform_tool_payload_for(output, locale);
    if locale == Locale::Zh {
        let mut lines = vec![
            format!("【工具结果：{tool_name}】"),
            format!("运行：{run_id}"),
            format!("调用 ID：{tool_id}"),
        ];
        if display_name != tool_name {
            lines.push(format!("显示名称：{display_name}"));
        }
        lines.push(format!("状态：{}", if ok { "成功" } else { "失败" }));
        lines.push(format!("结果：\n{output}"));
        lines.join("\n")
    } else {
        let mut lines = vec![
            format!("[Tool result: {tool_name}]"),
            format!("Run: {run_id}"),
            format!("Call ID: {tool_id}"),
        ];
        if display_name != tool_name {
            lines.push(format!("Display name: {display_name}"));
        }
        lines.push(format!("Status: {}", if ok { "success" } else { "failed" }));
        lines.push(format!("Result:\n{output}"));
        lines.join("\n")
    }
}

pub(crate) enum TurnDispatch {
    Completed(TurnOutcome),
    Failed(String),
}

/// Drives one agent turn for an inbound IM message and waits for the
/// final result. Mirrors `handle_ipc_turn`, minus the client stream.
pub(crate) async fn run_platform_turn(
    state: &DaemonState,
    session_id: Arc<str>,
    content: String,
    images: Vec<Option<ImageAttachment>>,
    mut profile: TurnProfile,
) -> Result<TurnDispatch> {
    let content = validate_content(content).map_err(|error| anyhow!(error.message))?;
    state.state_store.recover_stale_turns()?;

    let _global_permit = state
        .platforms
        .turn_permits
        .clone()
        .acquire_owned()
        .await
        .map_err(|_| anyhow!("the platform turn scheduler is closed"))?;

    let run_id = random_id("run", 18);
    let (cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
    let platform_context = profile.platform.clone();
    let intermediate_replies = platform_context.as_ref().is_some_and(|context| {
        let qq = &context.config.platforms.qq;
        match context.conversation.kind {
            ConversationKind::Group => qq.group_intermediate_messages,
            ConversationKind::Private => qq.private_intermediate_messages,
        }
    });
    let platform_followup = platform_context
        .as_ref()
        .map(|context| PlatformFollowupRun::new(context.clone()));
    profile.followup = platform_followup.clone();
    {
        let mut manager = state.manager.lock().unwrap();
        if manager.admin_busy {
            bail!("Miyu is busy with another operation");
        }
        manager.active_runs.insert(
            run_id.clone(),
            RunInfo {
                session_id: session_id.clone(),
                mode: AgentMode::Normal,
                audience: PromptAudience::External,
                cancel: cancel_tx.clone(),
                turn_id: None,
                queue_target: None,
                supersede: Arc::new(crate::agent::TurnSupersedeSignal::default()),
                platform_followup,
                operation: crate::web::RunOperation::Create,
                job_wake: false,
                job_wake_label: None,
            },
        );
    }
    if let Some(context) = platform_context.as_ref() {
        context.turn_started(cancel_tx);
    }
    if platform_context
        .as_ref()
        .is_some_and(|context| context.turn_is_superseded())
    {
        crate::web::finish_run(&state.manager, &run_id, None);
        return Ok(TurnDispatch::Failed(
            crate::i18n::text("the turn was superseded", "本轮已被新消息覆盖").to_string(),
        ));
    }
    let after = state.events.latest_id();
    let mut subscription = state.events.subscribe_after(after);
    if state
        .actor_tx
        .send(ActorCommand::StartTurn {
            run_id: run_id.clone(),
            session_id,
            display_content: content.clone(),
            content,
            attachment_run_id: None,
            mode: AgentMode::Normal,
            images,
            cwd: None,
            audience: PromptAudience::External,
            profile: Some(profile),
            cancel: cancel_rx,
        })
        .is_err()
    {
        crate::web::finish_run(&state.manager, &run_id, None);
        bail!("Miyu core worker is unavailable");
    }
    // Cancels the run if this task dies before the turn settles.
    let mut run_guard = IpcRunGuard {
        manager: state.manager.clone(),
        run_id: run_id.clone(),
        finished: false,
    };

    let deadline = tokio::time::Instant::now() + PLATFORM_TURN_TIMEOUT;
    let mut text = String::new();
    let mut image_assets = Vec::new();
    let mut reply_suppression = ReplySuppression::default();
    let mut last_id = after;
    let dispatch = loop {
        let record = if let Some(record) = subscription.pending.pop_front() {
            record
        } else {
            match tokio::time::timeout_at(deadline, subscription.receiver.recv()).await {
                Err(_) => {
                    break TurnDispatch::Failed(
                        crate::i18n::text("the reply timed out", "回复超时，本轮已取消")
                            .to_string(),
                    );
                }
                Ok(Ok(record)) => record,
                Ok(Err(broadcast::error::RecvError::Lagged(_))) => {
                    subscription.pending = state.events.replay_after(last_id);
                    continue;
                }
                Ok(Err(broadcast::error::RecvError::Closed)) => {
                    break TurnDispatch::Failed(
                        crate::i18n::text("Miyu core stopped", "Miyu 核心已停止").to_string(),
                    );
                }
            }
        };
        if record.kind == "resync_required" {
            break TurnDispatch::Failed(
                crate::i18n::text(
                    "event history was exhausted; the turn was cancelled",
                    "事件缓冲耗尽，本轮已取消",
                )
                .to_string(),
            );
        }
        last_id = record.id;
        let Ok(data) = serde_json::from_str::<Value>(&record.data) else {
            continue;
        };
        if data.get("run_id").and_then(Value::as_str) != Some(run_id.as_str()) {
            continue;
        }
        match record.kind.as_str() {
            "reasoning.start" => {
                if intermediate_replies {
                    if let Some(context) = platform_context.as_ref() {
                        flush_intermediate_reply(context, &text, &reply_suppression).await;
                    }
                }
                start_model_reply(&mut text, &mut reply_suppression);
            }
            "assistant.delta" => {
                if let Some(delta) = data.get("delta").and_then(Value::as_str) {
                    text.push_str(delta);
                }
            }
            "generation.superseded" => {
                text.clear();
                reply_suppression.model_started();
            }
            "tool.started" => {
                let readable = format_platform_tool_started_log(&run_id, &data);
                tracing::info!(target: "miyu::qq", "\n{readable}");
            }
            "tool.image" => {
                if let Some(id) = data
                    .get("asset")
                    .and_then(|asset| asset.get("id"))
                    .and_then(Value::as_str)
                {
                    image_assets.push(id.to_string());
                }
            }
            "tool.finished" => {
                let readable = format_platform_tool_finished_log(&run_id, &data);
                tracing::info!(target: "miyu::qq", "\n{readable}");
                let suppression_start = platform_context
                    .as_ref()
                    .and_then(|context| context.take_final_reply_suppression_start(text.len()));
                if let Some(start) = suppression_start {
                    reply_suppression.direct_send_succeeded(start);
                }
            }
            "queue.consumed" => {
                // Flush before the suppression reset below: the flushed text
                // still needs the direct-send ranges of the round it came
                // from, and the next round answers the newly consumed prompt.
                if intermediate_replies {
                    if let Some(context) = platform_context.as_ref() {
                        flush_intermediate_reply(context, &text, &reply_suppression).await;
                    }
                    text.clear();
                }
                reply_suppression.queued_prompt_consumed();
            }
            "run.completed" => {
                run_guard.finish();
                let (suppressed_reply_ranges, final_reply_already_sent) =
                    reply_suppression.finish(text.len());
                break TurnDispatch::Completed(TurnOutcome {
                    run_id: run_id.clone(),
                    text,
                    provider_id: data
                        .get("provider_id")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                    model: data
                        .get("model")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                    image_assets,
                    suppressed_reply_ranges,
                    final_reply_already_sent,
                });
            }
            "run.failed" => {
                run_guard.finish();
                let message = data
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown error")
                    .to_string();
                break TurnDispatch::Failed(message);
            }
            "run.cancelled" => {
                run_guard.finish();
                break TurnDispatch::Failed(
                    crate::i18n::text("the turn was cancelled", "本轮被取消了").to_string(),
                );
            }
            _ => {}
        }
    };
    Ok(dispatch)
}

/// Strips markdown decoration for plain-text IM surfaces (QQ renders no
/// markup). Deliberately conservative: fenced code bodies are kept
/// verbatim, single `*` stays (could be math), lists and newlines stay.
pub(crate) fn markdown_to_plain(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut in_fence = false;
    for line in text.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            in_fence = !in_fence;
            continue;
        }
        if in_fence {
            out.push_str(line);
            out.push('\n');
            continue;
        }
        let stripped = if trimmed.starts_with('#') {
            trimmed.trim_start_matches('#').trim_start()
        } else if let Some(rest) = trimmed.strip_prefix("> ") {
            rest
        } else {
            line
        };
        out.push_str(&strip_inline_markup(stripped));
        out.push('\n');
    }
    out.trim_end().to_string()
}

/// Removes `**`, `__`, backticks and rewrites `[text](url)` → `text (url)`.
fn strip_inline_markup(line: &str) -> String {
    let chars: Vec<char> = line.chars().collect();
    let mut out = String::with_capacity(line.len());
    let mut i = 0;
    while i < chars.len() {
        match chars[i] {
            '*' if chars.get(i + 1) == Some(&'*') => i += 2,
            '_' if chars.get(i + 1) == Some(&'_') => i += 2,
            '`' => i += 1,
            '[' => {
                // Try [text](url); anything else is emitted verbatim.
                let close = chars[i + 1..].iter().position(|&c| c == ']');
                let parsed = close.and_then(|offset| {
                    let close = i + 1 + offset;
                    if chars.get(close + 1) == Some(&'(') {
                        let end = chars[close + 2..].iter().position(|&c| c == ')');
                        end.map(|len| {
                            let text: String = chars[i + 1..close].iter().collect();
                            let url: String = chars[close + 2..close + 2 + len].iter().collect();
                            (close + 2 + len + 1, text, url)
                        })
                    } else {
                        None
                    }
                });
                match parsed {
                    Some((next, text, url)) => {
                        out.push_str(&text);
                        if !url.is_empty() && url != text {
                            out.push_str(" (");
                            out.push_str(&url);
                            out.push(')');
                        }
                        i = next;
                    }
                    None => {
                        out.push('[');
                        i += 1;
                    }
                }
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    out
}

/// Splits an over-long reply on paragraph, then line, then raw char
/// boundaries. Char-based so CJK never gets cut mid-codepoint.
pub(crate) fn split_reply(text: &str, max_chars: usize) -> Vec<String> {
    let text = text.trim();
    if text.is_empty() {
        return Vec::new();
    }
    if max_chars == 0 || text.chars().count() <= max_chars {
        return vec![text.to_string()];
    }
    let mut chunks = Vec::new();
    let mut current = String::new();
    let mut current_chars = 0;
    let flush = |current: &mut String, current_chars: &mut usize, chunks: &mut Vec<String>| {
        let piece = current.trim();
        if !piece.is_empty() {
            chunks.push(piece.to_string());
        }
        current.clear();
        *current_chars = 0;
    };
    for paragraph in text.split("\n\n") {
        let unit_chars = paragraph.chars().count();
        if unit_chars > max_chars {
            flush(&mut current, &mut current_chars, &mut chunks);
            // Oversized paragraph: pack by lines, hard-split huge lines.
            for line in paragraph.lines() {
                let line_chars = line.chars().count();
                if line_chars > max_chars {
                    flush(&mut current, &mut current_chars, &mut chunks);
                    let mut buffer = String::new();
                    let mut count = 0;
                    for c in line.chars() {
                        buffer.push(c);
                        count += 1;
                        if count == max_chars {
                            chunks.push(buffer.clone());
                            buffer.clear();
                            count = 0;
                        }
                    }
                    if !buffer.trim().is_empty() {
                        chunks.push(buffer.trim().to_string());
                    }
                    continue;
                }
                if current_chars + line_chars + 1 > max_chars {
                    flush(&mut current, &mut current_chars, &mut chunks);
                }
                if !current.is_empty() {
                    current.push('\n');
                    current_chars += 1;
                }
                current.push_str(line);
                current_chars += line_chars;
            }
            flush(&mut current, &mut current_chars, &mut chunks);
            continue;
        }
        if current_chars + unit_chars + 2 > max_chars {
            flush(&mut current, &mut current_chars, &mut chunks);
        }
        if !current.is_empty() {
            current.push_str("\n\n");
            current_chars += 2;
        }
        current.push_str(paragraph);
        current_chars += unit_chars;
    }
    flush(&mut current, &mut current_chars, &mut chunks);
    chunks
}

/// Sniffs the mime type of downloaded image bytes by magic numbers.
pub(crate) fn sniff_image_mime(bytes: &[u8]) -> &'static str {
    if bytes.starts_with(&[0x89, b'P', b'N', b'G']) {
        "image/png"
    } else if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        "image/jpeg"
    } else if bytes.starts_with(b"GIF8") {
        "image/gif"
    } else if bytes.len() > 11 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        "image/webp"
    } else {
        "image/png"
    }
}

/// Downloads a URL with a byte cap enforced while streaming, so an
/// oversized (or length-less) body can never balloon memory.
pub(crate) async fn download_capped(
    client: &reqwest::Client,
    url: &str,
    max_bytes: usize,
    timeout: Duration,
) -> Result<(Vec<u8>, Option<String>)> {
    let response = client
        .get(url)
        .timeout(timeout)
        .send()
        .await
        .with_context(|| format!("requesting {url}"))?
        .error_for_status()
        .with_context(|| format!("downloading {url}"))?;
    if let Some(length) = response.content_length() {
        if length as usize > max_bytes {
            bail!(
                "the file is larger than the {}MB limit",
                max_bytes / 1024 / 1024
            );
        }
    }
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string);
    let mut bytes = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.with_context(|| format!("reading {url}"))?;
        if bytes.len() + chunk.len() > max_bytes {
            bail!(
                "the file is larger than the {}MB limit",
                max_bytes / 1024 / 1024
            );
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok((bytes, content_type))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::paths::MiyuPaths;
    use futures_util::future::BoxFuture;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

    /// Regression: an auto-attached reply image delivered in one turn must
    /// stay suppressed for the recovery turn that follows an interrupted
    /// send — that replay is what duplicated pictures in QQ groups.
    #[test]
    fn delivered_images_stay_deduplicated_across_turns_per_conversation() {
        let first = blake3::hash(b"generated-picture");
        let second = blake3::hash(b"another-picture");
        let scope = "onebot:1:group:duplicate-image-regression";
        let other_scope = "onebot:1:group:unrelated";

        assert!(recent_conversation_images(scope).is_empty());
        record_recent_conversation_images(scope, &[first]);
        assert_eq!(recent_conversation_images(scope), vec![first]);
        // Other conversations are unaffected.
        assert!(recent_conversation_images(other_scope).is_empty());

        // Re-recording keeps one entry per digest.
        record_recent_conversation_images(scope, &[first, second]);
        let mut seen = recent_conversation_images(scope);
        seen.sort_by_key(|digest| digest.as_bytes().to_vec());
        let mut expected = vec![first, second];
        expected.sort_by_key(|digest| digest.as_bytes().to_vec());
        assert_eq!(seen, expected);
    }

    struct SuppressingToolPlugin;

    impl plugins::PlatformPlugin for SuppressingToolPlugin {
        fn descriptor(&self) -> plugins::PluginDescriptor {
            plugins::PluginDescriptor {
                id: "test_suppress",
                priority: 1,
                default_enabled: true,
            }
        }

        fn before_send<'a>(
            &'a self,
            _context: &'a PlatformTurnContext,
            message: OutboundMessage,
        ) -> BoxFuture<'a, Result<plugins::PreparedSend>> {
            Box::pin(async move {
                Ok(plugins::PreparedSend {
                    primary: message.clone(),
                    after_success: Vec::new(),
                    fallback: Some(message),
                    suppress_final_reply: true,
                    suppress_prior_reply: false,
                })
            })
        }
    }

    struct CountingAdapter {
        calls: AtomicUsize,
        fail_first: bool,
        messages: Mutex<Vec<OutboundMessage>>,
        group_members: Vec<PlatformGroupMember>,
    }

    struct PartialFailureAdapter {
        calls: AtomicUsize,
        digest: blake3::Hash,
        response_target_delivered: bool,
    }

    impl PlatformAdapter for CountingAdapter {
        fn send<'a>(&'a self, message: OutboundMessage) -> BoxFuture<'a, Result<SendReceipt>> {
            Box::pin(async move {
                let image_digests = match &message.body {
                    OutboundBody::Segments(segments) => segments
                        .iter()
                        .filter_map(|segment| match segment {
                            OutboundSegment::ImageBytes { data, .. } => Some(blake3::hash(data)),
                            _ => None,
                        })
                        .collect(),
                    OutboundBody::Forward(_) => Vec::new(),
                };
                self.messages.lock().unwrap().push(message);
                let call = self.calls.fetch_add(1, AtomicOrdering::Relaxed);
                if self.fail_first && call == 0 {
                    anyhow::bail!("injected primary failure");
                }
                Ok(SendReceipt {
                    delivered_parts: 1,
                    image_digests,
                    ..SendReceipt::default()
                })
            })
        }

        fn bot_display_name<'a>(&'a self) -> BoxFuture<'a, Result<String>> {
            Box::pin(async { Ok("Miyu".to_string()) })
        }

        fn group_members<'a>(&'a self) -> BoxFuture<'a, Result<Vec<PlatformGroupMember>>> {
            let members = self.group_members.clone();
            Box::pin(async move { Ok(members) })
        }
    }

    impl PlatformAdapter for PartialFailureAdapter {
        fn send<'a>(&'a self, _message: OutboundMessage) -> BoxFuture<'a, Result<SendReceipt>> {
            Box::pin(async move {
                self.calls.fetch_add(1, AtomicOrdering::Relaxed);
                Err(anyhow::Error::new(PartialSendError::new(
                    anyhow::anyhow!("injected failure after partial delivery"),
                    SendReceipt {
                        delivered_parts: 1,
                        image_digests: vec![self.digest],
                        response_target_delivered: self.response_target_delivered,
                        ..SendReceipt::default()
                    },
                )))
            })
        }

        fn bot_display_name<'a>(&'a self) -> BoxFuture<'a, Result<String>> {
            Box::pin(async { Ok("Miyu".to_string()) })
        }
    }

    fn test_paths(root: &std::path::Path) -> MiyuPaths {
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
            system_scripts_dir: PathBuf::new(),
        }
    }

    fn test_group_members() -> Vec<PlatformGroupMember> {
        ["20000", "30000", "40000", "50000"]
            .into_iter()
            .map(|user_id| PlatformGroupMember {
                group_id: "20000".to_string(),
                user_id: user_id.to_string(),
                nickname: format!("member-{user_id}"),
                card: String::new(),
                role: "member".to_string(),
                title: String::new(),
                joined_at: 0,
                last_active_at: 0,
            })
            .collect()
    }

    #[test]
    fn parenthetical_only_filter_ignores_mentions_but_preserves_real_content() {
        let filtered = OutboundMessage::segments(
            OutboundOrigin::FinalReply,
            vec![
                OutboundSegment::Mention("123".to_string()),
                OutboundSegment::Text("  （这个消息与我无关）  ".to_string()),
            ],
        );
        assert!(message_is_parenthetical_only(&filtered));

        let nested = OutboundMessage::text(OutboundOrigin::FinalReply, "（外层（说明））");
        assert!(message_is_parenthetical_only(&nested));
        let two = OutboundMessage::text(OutboundOrigin::FinalReply, "（动作）（说明）");
        assert!(!message_is_parenthetical_only(&two));
        let sentence = OutboundMessage::text(OutboundOrigin::FinalReply, "你好（说明）");
        assert!(!message_is_parenthetical_only(&sentence));
        let media = OutboundMessage::segments(
            OutboundOrigin::FinalReply,
            vec![
                OutboundSegment::Text("（图片）".to_string()),
                OutboundSegment::ImageBytes {
                    mime: "image/png".to_string(),
                    data: Arc::from([1_u8]),
                    alt: String::new(),
                },
            ],
        );
        assert!(!message_is_parenthetical_only(&media));
    }

    #[tokio::test]
    async fn parenthetical_only_model_reply_never_reaches_the_adapter() {
        let (_temp, context, adapter) = test_turn_context(false);
        context
            .send(OutboundMessage::segments(
                OutboundOrigin::FinalReply,
                vec![
                    OutboundSegment::Mention("123".to_string()),
                    OutboundSegment::Text("（无视）".to_string()),
                ],
            ))
            .await
            .unwrap();
        assert_eq!(adapter.calls.load(AtomicOrdering::Relaxed), 0);

        context
            .send(OutboundMessage::text(
                OutboundOrigin::FinalReply,
                "正常回复（补充）",
            ))
            .await
            .unwrap();
        assert_eq!(adapter.calls.load(AtomicOrdering::Relaxed), 1);
    }

    fn test_turn_context(
        fail_first: bool,
    ) -> (tempfile::TempDir, PlatformTurnContext, Arc<CountingAdapter>) {
        let temp = tempfile::tempdir().unwrap();
        let paths = test_paths(temp.path());
        let adapter = Arc::new(CountingAdapter {
            calls: AtomicUsize::new(0),
            fail_first,
            messages: Mutex::new(Vec::new()),
            group_members: test_group_members(),
        });
        // Unique conversation per context: the delivered-image ledger is
        // process-global and keyed by conversation, so two test contexts
        // sharing an id would observe each other's deliveries.
        static NEXT_CONVERSATION: AtomicUsize = AtomicUsize::new(0);
        let conversation_id = format!(
            "20000-{}",
            NEXT_CONVERSATION.fetch_add(1, AtomicOrdering::Relaxed)
        );
        let context = PlatformTurnContext::new(
            PlatformConversation {
                platform: "onebot".to_string(),
                account_id: "10000".to_string(),
                kind: ConversationKind::Private,
                conversation_id,
            },
            "20000".to_string(),
            "tester".to_string(),
            false,
            AppConfig::default(),
            paths.clone(),
            StateStore::new(&paths).unwrap(),
            adapter.clone(),
            Arc::new(plugins::PlatformPluginRegistry::new(vec![Arc::new(
                SuppressingToolPlugin,
            )])),
        );
        (temp, context, adapter)
    }

    #[tokio::test]
    async fn intermediate_flush_sends_round_text_once() {
        let (_temp, context, adapter) = test_turn_context(false);
        let suppression = ReplySuppression::default();
        flush_intermediate_reply(&context, "第一轮的说明。", &suppression).await;
        let messages = adapter.messages.lock().unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].origin, OutboundOrigin::IntermediateReply);
        let OutboundBody::Segments(segments) = &messages[0].body else {
            panic!("intermediate reply must be a normal message");
        };
        assert!(matches!(
            segments.as_slice(),
            [OutboundSegment::Markdown(text)] if text == "第一轮的说明。"
        ));
    }

    #[tokio::test]
    async fn intermediate_flush_skips_empty_and_cuts_direct_send_ranges() {
        let (_temp, context, adapter) = test_turn_context(false);

        // Nothing to say: no message goes out.
        flush_intermediate_reply(&context, "   ", &ReplySuppression::default()).await;
        assert!(adapter.messages.lock().unwrap().is_empty());

        // The model continuation after a confirmed direct tool send is
        // suppressed, so only the part before the send is flushed.
        let text = "前半部分。已被工具直发的确认。";
        let mut suppression = ReplySuppression::default();
        suppression.direct_send_succeeded("前半部分。".len());
        flush_intermediate_reply(&context, text, &suppression).await;
        let messages = adapter.messages.lock().unwrap();
        assert_eq!(messages.len(), 1);
        let OutboundBody::Segments(segments) = &messages[0].body else {
            panic!("intermediate reply must be a normal message");
        };
        assert!(matches!(
            segments.as_slice(),
            [OutboundSegment::Markdown(text)] if text == "前半部分。"
        ));
    }

    #[test]
    fn rate_window_allows_then_drops_with_single_notice() {
        let mut window = RateWindow::new();
        let start = Instant::now();
        let limit = PlatformRateLimit {
            max_messages: 3,
            window_seconds: 60,
        };
        for _ in 0..3 {
            assert_eq!(
                window.check_at(start, "group:1", limit),
                RateDecision::Allow
            );
        }
        assert_eq!(
            window.check_at(start, "group:1", limit),
            RateDecision::DropWithNotice
        );
        assert_eq!(
            window.check_at(start, "group:1", limit),
            RateDecision::DropSilently
        );
        // Another conversation is unaffected by the first group's quota.
        assert_eq!(
            window.check_at(start, "group:2", limit),
            RateDecision::Allow
        );
        // The window resets after a minute.
        let later = start + Duration::from_secs(61);
        assert_eq!(
            window.check_at(later, "group:1", limit),
            RateDecision::Allow
        );
    }

    #[test]
    fn rate_availability_preflight_never_consumes_quota() {
        let mut window = RateWindow::new();
        let start = Instant::now();
        let limit = PlatformRateLimit {
            max_messages: 1,
            window_seconds: 60,
        };
        assert!(window.available_at(start, "group:1", limit));
        assert!(window.available_at(start, "group:1", limit));
        assert_eq!(
            window.check_at(start, "group:1", limit),
            RateDecision::Allow
        );
        assert!(!window.available_at(start, "group:1", limit));
        assert_eq!(
            window.check_at(start, "group:1", limit),
            RateDecision::DropWithNotice
        );
    }

    #[test]
    fn rate_windows_are_independent_and_support_three_minute_quotas() {
        let mut window = RateWindow::new();
        let start = Instant::now();
        let limit = PlatformRateLimit {
            max_messages: 1,
            window_seconds: 180,
        };
        assert_eq!(
            window.check_at(start, "private:1", limit),
            RateDecision::Allow
        );
        assert_eq!(
            window.check_at(start + Duration::from_secs(30), "private:2", limit),
            RateDecision::Allow
        );
        assert_eq!(
            window.check_at(start + Duration::from_secs(179), "private:1", limit),
            RateDecision::DropWithNotice
        );
        assert_eq!(
            window.check_at(start + Duration::from_secs(180), "private:1", limit),
            RateDecision::Allow
        );
    }

    #[test]
    fn rate_window_zero_is_unlimited() {
        let mut unlimited = RateWindow::new();
        let start = Instant::now();
        let limit = PlatformRateLimit::default();
        for i in 0..100 {
            assert_eq!(
                unlimited.check_at(start, &format!("group:{i}"), limit),
                RateDecision::Allow
            );
        }
    }

    #[test]
    fn markdown_to_plain_strips_decoration_keeps_content() {
        let input = "# 标题\n\n**加粗** 与 `代码` 和 [链接](https://a.b)\n\n```rust\nlet x = 1; // **不动**\n```\n\n- 列表项\n> 引用";
        let plain = markdown_to_plain(input);
        assert_eq!(
            plain,
            "标题\n\n加粗 与 代码 和 链接 (https://a.b)\n\nlet x = 1; // **不动**\n\n- 列表项\n引用"
        );
    }

    #[test]
    fn markdown_link_edge_cases() {
        assert_eq!(strip_inline_markup("[a](b"), "[a](b");
        assert_eq!(strip_inline_markup("纯 [文本] 括号"), "纯 [文本] 括号");
        // Identical text/url collapses to one copy.
        assert_eq!(
            strip_inline_markup("[https://x.y](https://x.y)"),
            "https://x.y"
        );
    }

    #[test]
    fn split_reply_paragraph_line_and_hard_boundaries() {
        assert_eq!(split_reply("短", 10), vec!["短"]);
        assert!(split_reply("  ", 10).is_empty());
        // 0 disables splitting.
        let long = "a".repeat(50);
        assert_eq!(split_reply(&long, 0), vec![long.clone()]);

        let text = "第一段落。\n\n第二段落。";
        let chunks = split_reply(text, 6);
        assert_eq!(chunks, vec!["第一段落。", "第二段落。"]);

        // CJK hard split never panics and keeps every char.
        let cjk = "汉".repeat(25);
        let chunks = split_reply(&cjk, 10);
        assert_eq!(chunks.len(), 3);
        assert_eq!(chunks.join(""), cjk);
    }

    #[test]
    fn sniff_image_mime_by_magic() {
        assert_eq!(sniff_image_mime(&[0x89, b'P', b'N', b'G', 0]), "image/png");
        assert_eq!(sniff_image_mime(&[0xFF, 0xD8, 0xFF, 0xE0]), "image/jpeg");
        assert_eq!(sniff_image_mime(b"GIF89a"), "image/gif");
        assert_eq!(sniff_image_mime(b"RIFF\0\0\0\0WEBPVP8 "), "image/webp");
        assert_eq!(sniff_image_mime(b"????"), "image/png");
    }

    #[test]
    fn message_activity_counts_other_senders_and_deduplicates_events() {
        let registry = MessageActivityRegistry::default();
        let now = Instant::now();
        let (activity, start, _) = registry.observe("onebot:1:group:2", "m1", "alice", now);
        assert_eq!(start.total_messages, 1);
        assert_eq!(start.sender_messages, 1);

        let (_, first_other, first_received_at) =
            registry.observe("onebot:1:group:2", "m2", "bob", now);
        let (_, duplicate, duplicate_received_at) = registry.observe(
            "onebot:1:group:2",
            "m2",
            "bob",
            now + Duration::from_secs(10),
        );
        assert_eq!(duplicate, first_other);
        assert_eq!(duplicate_received_at, first_received_at);
        registry.observe("onebot:1:group:2", "m3", "alice", now);

        let current = activity.position_for("alice");
        assert_eq!(current.total_messages, 3);
        assert_eq!(current.sender_messages, 2);
        let other_messages = current
            .total_messages
            .saturating_sub(start.total_messages)
            .saturating_sub(
                current
                    .sender_messages
                    .saturating_sub(start.sender_messages),
            );
        assert_eq!(other_messages, 1);

        let (_, isolated, _) = registry.observe("onebot:1:group:3", "m4", "bob", now);
        assert_eq!(isolated.total_messages, 1);
    }

    #[test]
    fn adaptive_response_target_uses_independent_inclusive_boundaries() {
        let now = Instant::now();
        let start = PlatformMessagePosition {
            total_messages: 10,
            sender_messages: 2,
        };
        let target = ResponseTarget {
            message_id: "message-1".to_string(),
            user_id: "alice".to_string(),
            quote: true,
            mention: true,
            explicit_mention_user_ids: Vec::new(),
        };
        let policy = AdaptiveResponseTargetPolicy::new(Some(start), now, 5, 15);

        let before_both = policy.resolve(
            target.clone(),
            Some(PlatformMessagePosition {
                total_messages: 15,
                sender_messages: 3,
            }),
            now + Duration::from_secs(14),
        );
        assert!(before_both.is_none());

        let quote_only = policy
            .resolve(
                target.clone(),
                Some(PlatformMessagePosition {
                    total_messages: 15,
                    sender_messages: 2,
                }),
                now + Duration::from_secs(14),
            )
            .unwrap();
        assert!(quote_only.quote);
        assert!(!quote_only.mention);

        let mention_only = policy
            .resolve(
                target.clone(),
                Some(PlatformMessagePosition {
                    total_messages: 15,
                    sender_messages: 3,
                }),
                now + Duration::from_secs(15),
            )
            .unwrap();
        assert!(!mention_only.quote);
        assert!(mention_only.mention);

        let both = policy
            .resolve(
                target,
                Some(PlatformMessagePosition {
                    total_messages: 15,
                    sender_messages: 2,
                }),
                now + Duration::from_secs(15),
            )
            .unwrap();
        assert!(both.quote);
        assert!(both.mention);
    }

    #[test]
    fn adaptive_response_target_mention_uses_known_message_activity() {
        let now = Instant::now();
        let start = PlatformMessagePosition {
            total_messages: 10,
            sender_messages: 2,
        };
        let target = ResponseTarget {
            message_id: "message-1".to_string(),
            user_id: "alice".to_string(),
            quote: false,
            mention: true,
            explicit_mention_user_ids: Vec::new(),
        };
        let policy = AdaptiveResponseTargetPolicy::new(Some(start), now, 5, 15);
        let same_sender_message = PlatformMessagePosition {
            total_messages: 11,
            sender_messages: 3,
        };
        let other_sender_message = PlatformMessagePosition {
            total_messages: 11,
            sender_messages: 2,
        };
        let cases = [
            ("before threshold without messages", Some(start), 14, false),
            ("at threshold without messages", Some(start), 15, false),
            (
                "at threshold after same sender",
                Some(same_sender_message),
                15,
                false,
            ),
            (
                "before threshold after other sender",
                Some(other_sender_message),
                14,
                false,
            ),
            (
                "at threshold after other sender",
                Some(other_sender_message),
                15,
                true,
            ),
            ("before threshold with unknown activity", None, 14, false),
            ("at threshold with unknown activity", None, 15, true),
        ];

        for (case, current, elapsed_seconds, expected) in cases {
            let mention = policy
                .resolve(
                    target.clone(),
                    current,
                    now + Duration::from_secs(elapsed_seconds),
                )
                .is_some_and(|target| target.mention);
            assert_eq!(mention, expected, "{case}");
        }
    }

    #[tokio::test]
    async fn direct_final_suppression_requires_primary_send_success() {
        let (_temp, success, _adapter) = test_turn_context(false);
        success
            .send(OutboundMessage::text(OutboundOrigin::Tool, "sent"))
            .await
            .unwrap();
        assert!(success.take_final_reply_suppression());
        assert!(!success.take_final_reply_suppression());

        let (_temp, fallback, _adapter) = test_turn_context(true);
        fallback
            .send(OutboundMessage::text(OutboundOrigin::Tool, "fallback"))
            .await
            .unwrap();
        assert!(!fallback.take_final_reply_suppression());
    }

    #[tokio::test]
    async fn delivery_ledger_records_only_confirmed_images() {
        let bytes: Arc<[u8]> = Arc::from([1_u8, 2, 3]);
        let digest = blake3::hash(&bytes);
        let image_message = || {
            OutboundMessage::segments(
                OutboundOrigin::Tool,
                vec![OutboundSegment::ImageBytes {
                    mime: "image/png".to_string(),
                    data: bytes.clone(),
                    alt: String::new(),
                }],
            )
        };

        let (_temp, success, _adapter) = test_turn_context(false);
        success.send(image_message()).await.unwrap();
        assert!(success.delivered_image_digests().contains(&digest));

        let (_temp, mut failed, _adapter) = test_turn_context(true);
        failed.plugins = Arc::new(plugins::PlatformPluginRegistry::default());
        assert!(failed.send(image_message()).await.is_err());
        assert!(failed.delivered_image_digests().is_empty());
    }

    #[tokio::test]
    async fn partial_delivery_is_recorded_without_sending_a_full_fallback() {
        let temp = tempfile::tempdir().unwrap();
        let paths = test_paths(temp.path());
        let digest = blake3::hash(&[1_u8, 2, 3]);
        let adapter = Arc::new(PartialFailureAdapter {
            calls: AtomicUsize::new(0),
            digest,
            response_target_delivered: false,
        });
        let context = PlatformTurnContext::new(
            PlatformConversation {
                platform: "onebot".to_string(),
                account_id: "10000".to_string(),
                kind: ConversationKind::Private,
                conversation_id: "20000".to_string(),
            },
            "20000".to_string(),
            "tester".to_string(),
            false,
            AppConfig::default(),
            paths.clone(),
            StateStore::new(&paths).unwrap(),
            adapter.clone(),
            Arc::new(plugins::PlatformPluginRegistry::new(vec![Arc::new(
                SuppressingToolPlugin,
            )])),
        );

        let result = context
            .send(OutboundMessage::segments(
                OutboundOrigin::Tool,
                vec![OutboundSegment::ImageBytes {
                    mime: "image/png".to_string(),
                    data: Arc::from([1_u8, 2, 3]),
                    alt: String::new(),
                }],
            ))
            .await;

        assert!(result.is_err());
        assert_eq!(adapter.calls.load(AtomicOrdering::Relaxed), 1);
        assert!(context.delivered_image_digests().contains(&digest));
    }

    #[tokio::test]
    async fn response_target_is_consumed_once_and_survives_primary_fallback() {
        let (_temp, context, adapter) = test_turn_context(true);
        let target = ResponseTarget {
            message_id: "message-9".to_string(),
            user_id: "user-4".to_string(),
            quote: true,
            mention: true,
            explicit_mention_user_ids: Vec::new(),
        };
        context.set_response_target(Some(target.clone()));

        context
            .send(OutboundMessage::text(OutboundOrigin::Tool, "first"))
            .await
            .unwrap();
        context
            .send(OutboundMessage::text(OutboundOrigin::FinalReply, "second"))
            .await
            .unwrap();

        let messages = adapter.messages.lock().unwrap();
        assert_eq!(messages.len(), 3);
        assert_eq!(messages[0].response_target, Some(target.clone()));
        assert_eq!(messages[1].response_target, Some(target));
        assert_eq!(messages[2].response_target, None);
        assert_eq!(context.response_target(), None);
    }

    #[tokio::test]
    async fn partially_delivered_response_target_is_not_restored() {
        let temp = tempfile::tempdir().unwrap();
        let paths = test_paths(temp.path());
        let adapter = Arc::new(PartialFailureAdapter {
            calls: AtomicUsize::new(0),
            digest: blake3::hash(&[1_u8]),
            response_target_delivered: true,
        });
        let context = PlatformTurnContext::new(
            PlatformConversation {
                platform: "onebot".to_string(),
                account_id: "10000".to_string(),
                kind: ConversationKind::Group,
                conversation_id: "20000".to_string(),
            },
            "20000".to_string(),
            "tester".to_string(),
            false,
            AppConfig::default(),
            paths.clone(),
            StateStore::new(&paths).unwrap(),
            adapter,
            Arc::new(plugins::PlatformPluginRegistry::default()),
        );
        context.set_explicit_response_mentions(vec!["30000".to_string()]);

        assert!(context
            .send(OutboundMessage::text(OutboundOrigin::FinalReply, "first"))
            .await
            .is_err());
        assert!(context.response_target().is_none());
    }

    #[test]
    fn failed_older_send_merges_mentions_into_a_newer_response_target() {
        let (_temp, context, _adapter) = test_turn_context(false);
        context.set_explicit_response_mentions(vec!["30000".to_string()]);
        let reserved = context
            .response_target
            .lock()
            .unwrap()
            .take()
            .expect("explicit target exists");
        context.set_adaptive_response_target(
            Some(ResponseTarget {
                message_id: "message-2".to_string(),
                user_id: "20000".to_string(),
                quote: true,
                mention: true,
                explicit_mention_user_ids: Vec::new(),
            }),
            AdaptiveResponseTargetPolicy::new(None, Instant::now(), 1, 1),
        );

        context.restore_response_target(reserved);

        assert_eq!(
            context.response_target(),
            Some(ResponseTarget {
                message_id: "message-2".to_string(),
                user_id: "20000".to_string(),
                quote: true,
                mention: false,
                explicit_mention_user_ids: vec!["30000".to_string()],
            })
        );
    }

    #[tokio::test]
    async fn adaptive_response_target_is_identical_on_primary_and_fallback() {
        let (_temp, mut context, adapter) = test_turn_context(true);
        let registry = MessageActivityRegistry::default();
        let (activity, start, _) =
            registry.observe("onebot:1:group:2", "m1", "alice", Instant::now());
        for index in 0..5 {
            registry.observe(
                "onebot:1:group:2",
                &format!("other-{index}"),
                "bob",
                Instant::now(),
            );
        }
        context.message_activity = Some(activity);
        let target = ResponseTarget {
            message_id: "m1".to_string(),
            user_id: "alice".to_string(),
            quote: true,
            mention: true,
            explicit_mention_user_ids: Vec::new(),
        };
        context.set_adaptive_response_target(
            Some(target.clone()),
            AdaptiveResponseTargetPolicy::new(
                Some(start),
                Instant::now().checked_sub(Duration::from_secs(15)).unwrap(),
                5,
                15,
            ),
        );
        // The OneBot trigger pipeline writes the final static decision after
        // the plugin has selected its adaptive policy; the matching target
        // must not discard that policy.
        context.set_response_target(Some(target.clone()));

        context
            .send(OutboundMessage::text(OutboundOrigin::Tool, "answer"))
            .await
            .unwrap();

        let messages = adapter.messages.lock().unwrap();
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].response_target, Some(target.clone()));
        assert_eq!(messages[1].response_target, Some(target));
    }

    #[tokio::test]
    async fn session_turns_are_fifo_and_lock_entries_are_reclaimed() {
        let runtime = PlatformRuntime::new().unwrap();
        let limits = PlatformSessionLimits {
            running: 1,
            queued: 2,
        };
        let first = runtime
            .acquire_session_turn("session-a", limits)
            .await
            .unwrap();
        let (order_tx, mut order_rx) = tokio::sync::mpsc::unbounded_channel();

        let second_runtime = runtime.clone();
        let second_tx = order_tx.clone();
        let second = tokio::spawn(async move {
            let _lease = second_runtime
                .acquire_session_turn("session-a", limits)
                .await
                .unwrap();
            second_tx.send(2).unwrap();
        });
        while runtime
            .session_turn_locks
            .lock()
            .unwrap()
            .get("session-a")
            .map(Weak::strong_count)
            .unwrap_or(0)
            < 2
        {
            tokio::task::yield_now().await;
        }

        let third_runtime = runtime.clone();
        let third = tokio::spawn(async move {
            let _lease = third_runtime
                .acquire_session_turn("session-a", limits)
                .await
                .unwrap();
            order_tx.send(3).unwrap();
        });
        while runtime
            .session_turn_locks
            .lock()
            .unwrap()
            .get("session-a")
            .map(Weak::strong_count)
            .unwrap_or(0)
            < 3
        {
            tokio::task::yield_now().await;
        }

        drop(first);
        assert_eq!(order_rx.recv().await, Some(2));
        assert_eq!(order_rx.recv().await, Some(3));
        second.await.unwrap();
        third.await.unwrap();
        assert!(runtime.session_turn_locks.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn session_turn_limits_bound_running_and_waiting_work() {
        let runtime = PlatformRuntime::new().unwrap();
        let limits = PlatformSessionLimits {
            running: 4,
            queued: 8,
        };
        let mut running = Vec::new();
        for _ in 0..4 {
            running.push(
                runtime
                    .acquire_session_turn("bounded", limits)
                    .await
                    .unwrap(),
            );
        }
        let mut queued = Vec::new();
        for _ in 0..8 {
            let runtime = runtime.clone();
            queued.push(tokio::spawn(async move {
                runtime
                    .acquire_session_turn("bounded", limits)
                    .await
                    .unwrap()
            }));
        }
        loop {
            let waiting = runtime
                .session_turn_locks
                .lock()
                .unwrap()
                .get("bounded")
                .and_then(Weak::upgrade)
                .map(|state| state.waiting.load(Ordering::Acquire))
                .unwrap_or_default();
            if waiting == 8 {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert!(matches!(
            runtime.acquire_session_turn("bounded", limits).await,
            Err(SessionTurnAcquireError::Full)
        ));
        drop(running);
        for task in queued {
            drop(task.await.unwrap());
        }
        assert!(runtime.session_turn_locks.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn session_preemption_invalidates_old_waiters_but_not_new_arrivals() {
        let runtime = PlatformRuntime::new().unwrap();
        let limits = PlatformSessionLimits {
            running: 1,
            queued: 8,
        };
        let first = runtime
            .acquire_session_turn("session-a", limits)
            .await
            .unwrap();
        let old_ticket = runtime.session_turn_ticket("session-a", limits);
        let (order_tx, mut order_rx) = tokio::sync::mpsc::unbounded_channel();
        let (started_tx, mut started_rx) = tokio::sync::mpsc::unbounded_channel();

        let old_tx = order_tx.clone();
        let old_started = started_tx.clone();
        let old = tokio::spawn(async move {
            old_started.send("old").unwrap();
            let lease = old_ticket.acquire().await.unwrap();
            old_tx.send(("old", lease.is_valid())).unwrap();
        });
        assert_eq!(started_rx.recv().await, Some("old"));

        let command_ticket = runtime.preempt_session_turns("session-a");
        assert!(!first.is_valid());
        let command_tx = order_tx.clone();
        let command_started = started_tx.clone();
        let command = tokio::spawn(async move {
            command_started.send("command").unwrap();
            let lease = command_ticket.acquire().await.unwrap();
            command_tx.send(("command", lease.is_valid())).unwrap();
        });
        assert_eq!(started_rx.recv().await, Some("command"));

        let new_ticket = runtime.session_turn_ticket("session-a", limits);
        let new = tokio::spawn(async move {
            let lease = new_ticket.acquire().await.unwrap();
            order_tx.send(("new", lease.is_valid())).unwrap();
        });

        drop(first);
        assert_eq!(order_rx.recv().await, Some(("command", true)));
        assert_eq!(order_rx.recv().await, Some(("old", false)));
        assert_eq!(order_rx.recv().await, Some(("new", true)));
        old.await.unwrap();
        command.await.unwrap();
        new.await.unwrap();
        assert!(runtime.session_turn_locks.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn different_platform_sessions_do_not_block_each_other() {
        let runtime = PlatformRuntime::new().unwrap();
        let limits = PlatformSessionLimits {
            running: 1,
            queued: 1,
        };
        let _first = runtime
            .acquire_session_turn("session-a", limits)
            .await
            .unwrap();
        let independent = tokio::time::timeout(
            Duration::from_secs(1),
            runtime.acquire_session_turn("session-b", limits),
        )
        .await;
        assert!(independent.is_ok());
    }

    #[tokio::test]
    async fn running_platform_turn_does_not_block_an_independent_dispatch() {
        let daemon_temp = tempfile::tempdir().unwrap();
        let state = DaemonState::for_test(test_paths(daemon_temp.path()), 8300).unwrap();
        let session = state
            .state_store
            .create_session("miyu", "queued platform test", "user", None)
            .unwrap();
        state
            .state_store
            .pinned(&session.session_id)
            .start_turn("running-platform-turn", "first", std::process::id())
            .unwrap();

        let error = match run_platform_turn(
            &state,
            Arc::from(session.session_id.as_str()),
            "must stay separate".to_string(),
            Vec::new(),
            TurnProfile::default(),
        )
        .await
        {
            Err(error) => error,
            Ok(_) => panic!("independent platform turn should reach the unavailable worker"),
        };

        assert!(error.to_string().contains("worker is unavailable"));
        assert!(state
            .state_store
            .pinned(&session.session_id)
            .load_queued_prompts()
            .unwrap()
            .is_empty());
    }

    #[test]
    fn direct_send_without_later_prompt_covers_an_empty_final_reply() {
        let mut suppression = ReplySuppression::default();
        suppression.direct_send_succeeded(8);
        let (ranges, already_sent) = suppression.finish(8);
        assert!(ranges.is_empty());
        assert!(already_sent);
    }

    #[test]
    fn model_round_boundary_keeps_only_the_latest_visible_text() {
        let mut text = String::new();
        let mut suppression = ReplySuppression::default();

        start_model_reply(&mut text, &mut suppression);
        text.push_str("text before tool");
        start_model_reply(&mut text, &mut suppression);
        text.push_str("final tool follow-up");

        assert_eq!(text, "final tool follow-up");
        assert_eq!(suppression.finish(text.len()), (Vec::new(), false));
    }

    #[test]
    fn ordinary_single_round_reply_is_unchanged() {
        let mut text = String::new();
        let mut suppression = ReplySuppression::default();

        start_model_reply(&mut text, &mut suppression);
        text.push_str("ordinary single round");

        assert_eq!(text, "ordinary single round");
        assert_eq!(suppression.finish(text.len()), (Vec::new(), false));
    }

    #[test]
    fn platform_tool_payload_pretty_prints_small_json() {
        assert_eq!(
            format_platform_tool_payload_for(r#"{"query":"Miyu","limit":2}"#, Locale::Zh),
            "{\n  \"limit\": 2,\n  \"query\": \"Miyu\"\n}"
        );
    }

    #[test]
    fn platform_tool_payload_truncates_on_unicode_boundaries() {
        let payload = "喵".repeat(PLATFORM_TOOL_LOG_MAX_CHARS + 1);
        let formatted = format_platform_tool_payload_for(&payload, Locale::Zh);
        let (kept, notice) = formatted.split_once('\n').unwrap();

        assert_eq!(kept.chars().count(), PLATFORM_TOOL_LOG_MAX_CHARS);
        assert!(kept.chars().all(|character| character == '喵'));
        assert_eq!(notice, "... 已截断 1 字符 ...");
    }

    #[test]
    fn platform_reply_log_truncates_on_unicode_boundaries() {
        let payload = "喵".repeat(PLATFORM_REPLY_LOG_MAX_CHARS + 7);
        let formatted = truncate_platform_reply_log_for(&payload, Locale::Zh);
        let kept = formatted.lines().next().unwrap();

        assert_eq!(kept.chars().count(), PLATFORM_REPLY_LOG_MAX_CHARS);
        assert!(formatted.ends_with("... 已截断 7 字符 ..."));
        assert_eq!(
            truncate_platform_reply_log_for("safe\u{1b}[31m", Locale::Zh),
            "safe\\u{1b}[31m"
        );
    }

    #[test]
    fn platform_tool_logs_include_correlation_and_result_details() {
        let started = format_platform_tool_started_log_for(
            "run_123",
            &serde_json::json!({
                "tool_id": "run_123_tool_2",
                "name": "web_search",
                "display_name": "网页搜索",
                "arguments": "{\"query\":\"Miyu\"}"
            }),
            Locale::Zh,
        );
        assert!(started.starts_with("【工具：web_search】\n运行：run_123"));
        assert!(started.contains("调用 ID：run_123_tool_2"));
        assert!(started.contains("显示名称：网页搜索"));
        assert!(started.contains("\"query\": \"Miyu\""));

        let finished = format_platform_tool_finished_log_for(
            "run_123",
            &serde_json::json!({
                "tool_id": "run_123_tool_2",
                "name": "web_search",
                "display_name": "网页搜索",
                "ok": false,
                "output": "request timed out"
            }),
            Locale::Zh,
        );
        assert!(finished.starts_with("【工具结果：web_search】\n运行：run_123"));
        assert!(finished.contains("调用 ID：run_123_tool_2"));
        assert!(finished.contains("显示名称：网页搜索"));
        assert!(finished.contains("状态：失败"));
        assert!(finished.ends_with("结果：\nrequest timed out"));

        let english = format_platform_tool_finished_log_for(
            "run_123",
            &serde_json::json!({
                "tool_id": "run_123_tool_2",
                "name": "web_search",
                "ok": true,
                "output": "done"
            }),
            Locale::En,
        );
        assert!(english.starts_with("[Tool result: web_search]\nRun: run_123"));
        assert!(english.contains("Status: success"));

        let sanitized = format_platform_tool_finished_log_for(
            "run_123",
            &serde_json::json!({
                "tool_id": "run_123_tool_2",
                "name": "web_search\nforged",
                "ok": true,
                "output": "safe\u{1b}[31m"
            }),
            Locale::En,
        );
        assert!(sanitized.starts_with("[Tool result: web_search forged]"));
        assert!(sanitized.ends_with("Result:\nsafe\\u{1b}[31m"));
    }

    #[test]
    fn platform_final_reply_log_is_bilingual() {
        let (_temp, context, _adapter) = test_turn_context(false);
        let outcome = TurnOutcome {
            run_id: "run_123".to_string(),
            text: "hello".to_string(),
            provider_id: Some("provider".to_string()),
            model: Some("model".to_string()),
            image_assets: Vec::new(),
            suppressed_reply_ranges: Vec::new(),
            final_reply_already_sent: false,
        };

        let chinese =
            format_platform_final_reply_log_for(&outcome, &context, "你好", 0, Locale::Zh);
        assert!(chinese.starts_with("【AI 最终回复】\n运行：run_123"));
        assert!(chinese.contains("模型：provider / model"));

        let english =
            format_platform_final_reply_log_for(&outcome, &context, "hello", 0, Locale::En);
        assert!(english.starts_with("[AI final reply]\nRun: run_123"));
        assert!(english.contains("Model: provider / model"));
    }

    #[test]
    fn direct_send_suppresses_the_next_model_round() {
        let mut text = String::new();
        let mut suppression = ReplySuppression::default();
        start_model_reply(&mut text, &mut suppression);
        text.push_str("text before tool");
        suppression.direct_send_succeeded(text.len());

        start_model_reply(&mut text, &mut suppression);
        text.push_str("duplicate confirmation");
        let (ranges, already_sent) = suppression.finish(text.len());

        assert_eq!(ranges, vec![(0, text.len())]);
        assert!(already_sent);
    }

    #[test]
    fn queued_followup_resets_prior_direct_send_suppression() {
        let mut text = String::new();
        let mut suppression = ReplySuppression::default();
        start_model_reply(&mut text, &mut suppression);
        suppression.direct_send_succeeded(0);
        start_model_reply(&mut text, &mut suppression);
        text.push_str("reply before queued follow-up");
        suppression.queued_prompt_consumed();

        start_model_reply(&mut text, &mut suppression);
        text.push_str("queued follow-up answer");

        assert_eq!(text, "queued follow-up answer");
        assert_eq!(suppression.finish(text.len()), (Vec::new(), false));
    }

    #[test]
    fn host_tools_follow_admin_and_private_whitelist_policy() {
        let (_temp, mut context, _adapter) = test_turn_context(false);
        assert!(!context.host_tools_allowed());
        context.is_admin = true;
        assert!(context.host_tools_allowed());

        context.is_admin = false;
        context.config.platforms.qq.allow_non_admin_host_tools = true;
        assert!(!context.host_tools_allowed());
        let dynamic_key = access_control::global_grant_key(
            access_control::AccessPermission::PrivateWhitelist,
            "20000".to_string(),
        );
        let actor = crate::state::PlatformAccessActor {
            platform: "onebot".to_string(),
            account_id: "10000".to_string(),
            user_id: "42".to_string(),
            conversation_kind: "private".to_string(),
            conversation_id: "42".to_string(),
            message_id: "message-1".to_string(),
        };
        context
            .state_store
            .add_platform_access_grant(&dynamic_key, &actor)
            .unwrap();
        assert!(context.host_tools_allowed());
        context
            .state_store
            .remove_platform_access_grant(&dynamic_key, &actor)
            .unwrap();
        assert!(!context.host_tools_allowed());
        context
            .config
            .platforms
            .qq
            .private_chats
            .whitelist
            .push(20_000);
        assert!(context.host_tools_allowed());

        context.conversation.kind = ConversationKind::Group;
        assert!(!context.host_tools_allowed());
    }

    #[test]
    fn untrusted_send_tool_schema_does_not_expose_local_attachments() {
        let (_temp, context, _adapter) = test_turn_context(false);
        let mut registry = crate::tools::ToolRegistry::new();
        register_platform_tools(&mut registry, Arc::new(context));
        let parameters = &registry.get("send_message_to_user").unwrap().parameters;

        assert!(parameters["properties"].get("text").is_some());
        assert!(parameters["properties"].get("images").is_none());
        assert!(parameters["properties"].get("files").is_none());
    }

    #[test]
    fn multi_mention_tool_is_only_registered_for_group_turns() {
        let (_private_temp, private, _adapter) = test_turn_context(false);
        let mut private_tools = crate::tools::ToolRegistry::new();
        register_platform_tools(&mut private_tools, Arc::new(private));
        assert!(private_tools.get("qq_mention_users").is_none());

        let (_group_temp, mut group, _adapter) = test_turn_context(false);
        group.conversation.kind = ConversationKind::Group;
        let mut group_tools = crate::tools::ToolRegistry::new();
        register_platform_tools(&mut group_tools, Arc::new(group));
        assert!(group_tools.get("qq_mention_user").is_none());
        let tool = group_tools.get("qq_mention_users").unwrap();
        assert_eq!(tool.parameters["required"], serde_json::json!(["user_ids"]));
        assert_eq!(tool.parameters["additionalProperties"], false);
        assert_eq!(tool.parameters["properties"]["user_ids"]["minItems"], 1);
        assert_eq!(tool.parameters["properties"]["user_ids"]["maxItems"], 32);
        assert_eq!(
            tool.parameters["properties"]["user_ids"]["items"]["pattern"],
            "^[1-9][0-9]{4,11}$"
        );
    }

    #[tokio::test]
    async fn multi_mention_tool_overrides_automatic_mention_without_sending_an_extra_message() {
        let (_temp, mut context, adapter) = test_turn_context(false);
        context.conversation.kind = ConversationKind::Group;
        let context = Arc::new(context);
        context.set_response_target(Some(ResponseTarget {
            message_id: "message-1".to_string(),
            user_id: "20000".to_string(),
            quote: true,
            mention: true,
            explicit_mention_user_ids: Vec::new(),
        }));
        let mut registry = crate::tools::ToolRegistry::new();
        register_platform_tools(&mut registry, context.clone());

        registry
            .call("qq_mention_users", r#"{"user_ids":["50000"]}"#)
            .await
            .unwrap();
        let output = registry
            .call(
                "qq_mention_users",
                r#"{"user_ids":["30000","40000","30000"]}"#,
            )
            .await
            .unwrap();
        let output: Value = serde_json::from_str(&output).unwrap();
        assert_eq!(output["user_ids"], serde_json::json!(["30000", "40000"]));
        assert_eq!(adapter.calls.load(AtomicOrdering::Relaxed), 0);
        assert_eq!(
            context.response_target(),
            Some(ResponseTarget {
                message_id: "message-1".to_string(),
                user_id: "20000".to_string(),
                quote: true,
                mention: false,
                explicit_mention_user_ids: vec!["30000".to_string(), "40000".to_string()],
            })
        );

        context
            .send(OutboundMessage::text(OutboundOrigin::FinalReply, "你好"))
            .await
            .unwrap();
        assert_eq!(adapter.calls.load(AtomicOrdering::Relaxed), 1);
        assert!(context.response_target().is_none());
        let messages = adapter.messages.lock().unwrap();
        assert_eq!(
            messages[0].response_target,
            Some(ResponseTarget {
                message_id: "message-1".to_string(),
                user_id: "20000".to_string(),
                quote: true,
                mention: false,
                explicit_mention_user_ids: vec!["30000".to_string(), "40000".to_string()],
            })
        );
    }

    #[tokio::test]
    async fn multi_mention_tool_preserves_the_adaptive_quote_policy() {
        let (_temp, mut context, adapter) = test_turn_context(false);
        context.conversation.kind = ConversationKind::Group;
        let context = Arc::new(context);
        context.set_adaptive_response_target(
            Some(ResponseTarget {
                message_id: "message-1".to_string(),
                user_id: "20000".to_string(),
                quote: true,
                mention: true,
                explicit_mention_user_ids: Vec::new(),
            }),
            AdaptiveResponseTargetPolicy::new(None, Instant::now(), 1, 0),
        );
        let mut registry = crate::tools::ToolRegistry::new();
        register_platform_tools(&mut registry, context.clone());

        registry
            .call("qq_mention_users", r#"{"user_ids":["30000"]}"#)
            .await
            .unwrap();
        context.set_adaptive_response_target(
            Some(ResponseTarget {
                message_id: "message-2".to_string(),
                user_id: "20000".to_string(),
                quote: true,
                mention: true,
                explicit_mention_user_ids: Vec::new(),
            }),
            AdaptiveResponseTargetPolicy::new(None, Instant::now(), 1, 0),
        );
        context
            .send(OutboundMessage::text(OutboundOrigin::FinalReply, "你好"))
            .await
            .unwrap();

        let messages = adapter.messages.lock().unwrap();
        assert_eq!(
            messages[0].response_target,
            Some(ResponseTarget {
                message_id: "message-2".to_string(),
                user_id: "20000".to_string(),
                quote: false,
                mention: false,
                explicit_mention_user_ids: vec!["30000".to_string()],
            })
        );
    }

    #[tokio::test]
    async fn multi_mention_tool_rejects_invalid_or_excessive_targets() {
        let (_temp, mut context, adapter) = test_turn_context(false);
        context.conversation.kind = ConversationKind::Group;
        let mut registry = crate::tools::ToolRegistry::new();
        register_platform_tools(&mut registry, Arc::new(context));

        let error = registry
            .call("qq_mention_users", r#"{"user_ids":["all"]}"#)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("5-12 digit QQ ID"));

        let error = registry
            .call("qq_mention_users", r#"{"user_ids":["+30000"]}"#)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("5-12 digit QQ ID"));

        let error = registry
            .call("qq_mention_users", r#"{"user_ids":[" 30000 "]}"#)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("5-12 digit QQ ID"));

        let error = registry
            .call(
                "qq_mention_users",
                r#"{"user_ids":["30000"],"group_id":"99999"}"#,
            )
            .await
            .unwrap_err();
        assert!(error.to_string().contains("only user_ids"));

        let error = registry
            .call("qq_mention_users", r#"{"user_ids":["60000"]}"#)
            .await
            .unwrap_err();
        assert!(error
            .to_string()
            .contains("not members of the current group"));

        let error = registry
            .call("qq_mention_users", r#"{"user_ids":[]}"#)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("at least one QQ ID"));

        let user_ids = (1..=33).map(|id| id.to_string()).collect::<Vec<_>>();
        let arguments = serde_json::json!({ "user_ids": user_ids }).to_string();
        let error = registry
            .call("qq_mention_users", &arguments)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("at most 32 users"));
        assert_eq!(adapter.calls.load(AtomicOrdering::Relaxed), 0);
    }

    fn built_in_test_context(
        kind: ConversationKind,
    ) -> (tempfile::TempDir, Arc<PlatformTurnContext>) {
        let temp = tempfile::tempdir().unwrap();
        let paths = test_paths(temp.path());
        let adapter = Arc::new(CountingAdapter {
            calls: AtomicUsize::new(0),
            fail_first: false,
            messages: Mutex::new(Vec::new()),
            group_members: test_group_members(),
        });
        let context = PlatformTurnContext::new(
            PlatformConversation {
                platform: "onebot".to_string(),
                account_id: "10000".to_string(),
                kind,
                conversation_id: "20000".to_string(),
            },
            "20000".to_string(),
            "tester".to_string(),
            false,
            AppConfig::default(),
            paths.clone(),
            StateStore::new(&paths).unwrap(),
            adapter,
            Arc::new(plugins::PlatformPluginRegistry::built_in().unwrap()),
        );
        (temp, Arc::new(context))
    }

    #[tokio::test]
    async fn one_recall_tool_is_registered_for_every_qq_turn() {
        let (_private_temp, private) = built_in_test_context(ConversationKind::Private);
        private.prepare_turn("test".to_string()).await;
        let mut private_tools = crate::tools::ToolRegistry::new();
        register_platform_tools(&mut private_tools, private);
        assert!(private_tools.get("qq_withdraw_message").is_some());

        let (_group_temp, group) = built_in_test_context(ConversationKind::Group);
        group.prepare_turn("test".to_string()).await;
        let mut group_tools = crate::tools::ToolRegistry::new();
        register_platform_tools(&mut group_tools, group);
        assert!(group_tools.get("qq_withdraw_message").is_some());

        let (_member_temp, member_group) = built_in_test_context(ConversationKind::Group);
        member_group.set_plugin_value(
            "qq_group_management.bot_role",
            Value::String("member".to_string()),
        );
        let mut member_tools = crate::tools::ToolRegistry::new();
        register_platform_tools(&mut member_tools, member_group);
        assert!(member_tools.get("qq_withdraw_message").is_some());
    }
}
