use crate::agent::{
    archive_and_delete_visible_turns, Agent, AgentEvent, AgentMode, AgentTurnControl,
};
use crate::cli::{build_tool_registry, WebArgs};
use crate::config::{ActiveProviderModelConfig, AppConfig, PromptAudience};
use crate::i18n::text as t;
use crate::ipc::{
    self, Command as IpcCommand, Frame as IpcFrame, ImageAttachment, Request as IpcRequest,
};
use crate::llm::{
    thinking_variant_options_for_model, ChatResult, ChatStreamKind, OpenAiCompatibleClient,
    ThinkingVariantOptions, ThinkingVariantPreferences, Usage,
};
use crate::memory::{
    MemoryAccess, MemoryOrganizer, MemoryOrganizerHandle, MemoryOrigin, MemoryStore,
};
use crate::paths::MiyuPaths;
use crate::question::{self, QuestionAnswers, QuestionRequest, QuestionResponse};
use crate::state::{
    ArtifactAsset, ImageAsset, PlatformPluginScopeKey, QueuedPrompt, StateStore, Turn,
    TurnFollowup, TurnStatus, UsageSnapshot, UserAttachment,
};
use crate::tools::{self, CommandOutputStream};
use anyhow::{bail, Context, Result};
use axum::body::Bytes;
use axum::extract::{ConnectInfo, DefaultBodyLimit, Path, Query, State};
use axum::http::header::{
    CACHE_CONTROL, CONTENT_DISPOSITION, CONTENT_LENGTH, CONTENT_SECURITY_POLICY, CONTENT_TYPE,
    COOKIE, HOST, ORIGIN, REFERRER_POLICY, RETRY_AFTER, SET_COOKIE, X_CONTENT_TYPE_OPTIONS,
};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, patch, post, put};
use axum::{Json, Router};
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use futures_util::stream::{self, Stream};
use futures_util::StreamExt;
use rand::rngs::OsRng;
use rand::RngCore;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet, VecDeque};
use std::convert::Infallible;
use std::future::IntoFuture;
use std::io::{self, IsTerminal, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path as FilePath, PathBuf};
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use tokio::io::AsyncWriteExt;
use tokio::sync::{broadcast, mpsc, oneshot, Semaphore};
use tokio::task::JoinHandle as TokioJoinHandle;

use crate::platforms::{self, PlatformRuntime};

const JSON_BODY_LIMIT: usize = 4 * 1024 * 1024;
const PERSONA_ASSET_LIMIT: usize = 8 * 1024 * 1024;
const ATTACHMENT_BODY_LIMIT: usize = 10 * 1024 * 1024;
const MAX_ATTACHMENT_TOTAL_BYTES: u64 = 32 * 1024 * 1024;
const MAX_TEXT_ATTACHMENT_BYTES: usize = 1024 * 1024;
const MAX_ATTACHMENTS_PER_MESSAGE: usize = 12;
const DEFAULT_BOARD_TITLE: &str = "今天想聊些什么？";
const DEFAULT_BOARD_SUBTITLE: &str = "从一个问题、计划或此刻的想法开始。";
const DEFAULT_STARTER_PROMPTS: [&str; 4] = [
    "查询今天的天气",
    "分析一个问题",
    "发表情包打个招呼吧",
    "搜索一张图片",
];
const MAX_CONTENT_CHARS: usize = 20_000;
const MAX_PROMPT_DOCUMENT_CHARS: usize = 200_000;
const MAX_PROMPT_DOCUMENTS: usize = 128;
const MAX_SECRET_CHARS: usize = 100_000;
const MAX_THINKING_VARIANT_UPDATES: usize = 64;
const EVENT_CAPACITY: usize = 4096;
const AUTH_COOKIE: &str = "miyu_session";
const LOGIN_WINDOW: Duration = Duration::from_secs(60);
const LOGIN_ATTEMPT_LIMIT: u8 = 5;

const INDEX_HTML: &str = include_str!("../web/index.html");
const STYLES_CSS: &str = include_str!("../web/styles.css");
const APP_JS: &str = include_str!("../web/app.js");
const MIYU_LOGO: &[u8] = include_bytes!("../pics/miyu-logo.png");
const MIYU_WALLPAPER: &[u8] = include_bytes!("../pics/miyuwallpaper.png");

#[derive(Clone)]
pub(crate) struct DaemonState {
    auth: WebAuth,
    boot_id: Arc<str>,
    pub(crate) web_port: u16,
    web_public: bool,
    web_bind: IpAddr,
    pub(crate) paths: MiyuPaths,
    pub(crate) manager: Arc<Mutex<ManagerState>>,
    pub(crate) state_store: StateStore,
    pub(crate) events: EventHub,
    pub(crate) questions: QuestionBroker,
    pub(crate) actor_tx: mpsc::UnboundedSender<ActorCommand>,
    shutdown_tx: broadcast::Sender<()>,
    turn_engine: TurnEngineState,
    pub(crate) platforms: PlatformRuntime,
}

#[cfg(test)]
impl DaemonState {
    pub(crate) fn for_test(paths: MiyuPaths, web_port: u16) -> Result<Self> {
        let state_store = StateStore::new(&paths)?;
        let config = AppConfig::default();
        let context = cold_context(&config, &state_store)?;
        let manager = Arc::new(Mutex::new(ManagerState {
            config,
            active_runs: HashMap::new(),
            admin_busy: false,
            context,
            persona_session_ids: HashMap::new(),
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

    pub(crate) fn for_test_with_actor(
        paths: MiyuPaths,
        web_port: u16,
    ) -> Result<(Self, std::thread::JoinHandle<Result<()>>)> {
        let state_store = StateStore::new(&paths)?;
        let config = AppConfig::default();
        let context = cold_context(&config, &state_store)?;
        let manager = Arc::new(Mutex::new(ManagerState {
            config: config.clone(),
            active_runs: HashMap::new(),
            admin_busy: false,
            context,
            persona_session_ids: HashMap::new(),
        }));
        let events = EventHub::new();
        let questions = QuestionBroker::new();
        let turn_engine = TurnEngineState::default();
        let (actor_tx, actor_join) = spawn_actor(
            config,
            paths.clone(),
            state_store.clone(),
            manager.clone(),
            events.clone(),
            questions.clone(),
            turn_engine.clone(),
            None,
        )?;
        let (shutdown_tx, _shutdown_rx) = broadcast::channel(1);
        Ok((
            Self {
                auth: WebAuth::new(None),
                boot_id: Arc::from("boot-test"),
                web_port,
                web_public: false,
            web_bind: IpAddr::V4(Ipv4Addr::LOCALHOST),
                paths,
                manager,
                state_store,
                events,
                questions,
                actor_tx,
                shutdown_tx,
                turn_engine,
                platforms: PlatformRuntime::new()?,
            },
            actor_join,
        ))
    }
}

#[derive(Clone, Default)]
struct TurnEngineState(Arc<AtomicU8>);

impl TurnEngineState {
    const COLD: u8 = 0;
    const INITIALIZING: u8 = 1;
    const READY: u8 = 2;
    const FAILED: u8 = 3;

    fn set(&self, state: u8) {
        self.0.store(state, Ordering::Release);
    }

    fn is_ready(&self) -> bool {
        self.0.load(Ordering::Acquire) == Self::READY
    }

    fn label(&self) -> &'static str {
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
struct TurnResources {
    client: OpenAiCompatibleClient,
    normal_tools: tools::ToolRegistry,
    plan_tools: tools::ToolRegistry,
    chat_tools: tools::ToolRegistry,
    restricted_tools: tools::ToolRegistry,
}

const MAX_CACHED_TURN_RESOURCE_CONFIGS: usize = 16;

struct TurnResourceCache {
    entries: HashMap<[u8; 32], Arc<TurnResources>>,
    order: VecDeque<[u8; 32]>,
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
    fn clear(&mut self) {
        self.entries.clear();
        self.order.clear();
    }

    fn key(config: &AppConfig) -> Result<[u8; 32]> {
        let encoded =
            serde_json::to_vec(config).context("serializing effective turn configuration")?;
        Ok(*blake3::hash(&encoded).as_bytes())
    }

    fn get_or_build(
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
            tools::restricted_platform_registry(config, paths)
        } else {
            tools::ToolRegistry::new()
        };
        tools::register_script_display_names(&restricted_tools);
        let resources = Arc::new(TurnResources {
            client: OpenAiCompatibleClient::from_config(config, paths)?,
            normal_tools: build_tool_registry(config, paths, AgentMode::Normal, false)?,
            plan_tools: build_tool_registry(config, paths, AgentMode::Plan, false)?,
            chat_tools: build_tool_registry(config, paths, AgentMode::Chat, false)?,
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

#[derive(Clone)]
struct WebAuth {
    password_digest: Option<[u8; 32]>,
    sessions: Arc<Mutex<HashSet<String>>>,
    attempts: Arc<Mutex<HashMap<IpAddr, LoginAttempt>>>,
}

#[derive(Clone, Copy)]
struct LoginAttempt {
    window_started: Instant,
    failures: u8,
}

#[derive(Debug, Clone, Copy)]
enum LoginFailure {
    Invalid,
    RateLimited,
}

impl WebAuth {
    fn new(password: Option<&str>) -> Self {
        let password_digest = password.map(|password| {
            let mut digest = Sha256::new();
            digest.update(password.as_bytes());
            digest.finalize().into()
        });
        Self {
            password_digest,
            sessions: Arc::new(Mutex::new(HashSet::new())),
            attempts: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    fn required(&self) -> bool {
        self.password_digest.is_some()
    }

    fn is_authenticated(&self, supplied: Option<&str>) -> bool {
        if !self.required() {
            return true;
        }
        supplied.is_some_and(|token| self.sessions.lock().unwrap().contains(token))
    }

    fn login(&self, peer: IpAddr, password: &str) -> std::result::Result<String, LoginFailure> {
        let Some(expected) = self.password_digest else {
            return Ok(String::new());
        };
        let now = Instant::now();
        {
            let mut attempts = self.attempts.lock().unwrap();
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
        sessions.insert(token.clone());
        if sessions.len() > 64 {
            sessions.clear();
            sessions.insert(token.clone());
        }
        Ok(token)
    }
}

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
    pub(crate) platform_followup: Option<Arc<platforms::PlatformFollowupRun>>,
    pub(crate) operation: RunOperation,
    /// True for daemon-initiated background-command wake turns; lets REPL
    /// clients discover and attach to them for live rendering.
    pub(crate) job_wake: bool,
    /// Display label for wake turns: "<job_id> · <title>".
    pub(crate) job_wake_label: Option<String>,
}

#[derive(Clone, Debug)]
pub(crate) enum RunOperation {
    Create,
    Redo { turn_id: String, input_id: String },
}

impl RunOperation {
    fn name(&self) -> &'static str {
        match self {
            Self::Create => "create",
            Self::Redo { .. } => "redo",
        }
    }

    fn turn_id(&self) -> Option<&str> {
        match self {
            Self::Create => None,
            Self::Redo { turn_id, .. } => Some(turn_id),
        }
    }

    fn input_id(&self) -> Option<&str> {
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
    pub(crate) context: ContextSnapshot,
    persona_session_ids: HashMap<String, String>,
}

impl ManagerState {
    /// A run currently executing in the given session, if any (most callers
    /// only need one representative — e.g. the WebUI compat field).
    fn run_in_session(&self, session_id: &str) -> Option<&String> {
        self.active_runs
            .iter()
            .find(|(_, info)| &*info.session_id == session_id)
            .map(|(run_id, _)| run_id)
    }

    fn session_has_runs(&self, session_id: &str) -> bool {
        self.active_runs
            .values()
            .any(|info| &*info.session_id == session_id)
    }

    pub(crate) fn session_has_redo(&self, session_id: &str) -> bool {
        self.active_runs.values().any(|info| {
            &*info.session_id == session_id && matches!(info.operation, RunOperation::Redo { .. })
        })
    }

    fn session_runs_match_audience(&self, session_id: &str, audience: PromptAudience) -> bool {
        let mut runs = self
            .active_runs
            .values()
            .filter(|info| &*info.session_id == session_id);
        runs.next().is_some_and(|first| {
            first.audience == audience && runs.all(|info| info.audience == audience)
        })
    }
}

#[derive(Clone, Copy, Debug, Serialize)]
pub(crate) struct ContextSnapshot {
    pub(crate) tokens: u64,
    pub(crate) window: Option<usize>,
    pub(crate) cumulative_tokens: u64,
    pub(crate) cumulative_prompt_tokens: u64,
    pub(crate) cumulative_cache_read_tokens: u64,
}

pub(crate) enum ActorCommand {
    StartTurn {
        run_id: String,
        session_id: Arc<str>,
        content: String,
        display_content: String,
        attachment_run_id: Option<String>,
        mode: AgentMode,
        images: Vec<Option<ImageAttachment>>,
        cwd: Option<std::path::PathBuf>,
        audience: PromptAudience,
        /// Platform-only per-turn overrides. CLI/WebUI turns leave this empty.
        profile: Option<platforms::TurnProfile>,
        cancel: tokio::sync::watch::Receiver<bool>,
    },
    RedoTurn {
        run_id: String,
        session_id: Arc<str>,
        candidate: crate::state::RedoCandidate,
        prompts: Vec<RedoWebPrompt>,
        mode: AgentMode,
        cancel: tokio::sync::watch::Receiver<bool>,
    },
    SetModels {
        models: Vec<ActiveProviderModelConfig>,
        reply: oneshot::Sender<std::result::Result<(), AdminFailure>>,
    },
    SetThinkingVariants {
        updates: Vec<ThinkingVariantUpdate>,
        reply: oneshot::Sender<std::result::Result<(), AdminFailure>>,
    },
    ApplyConfig {
        config: Box<AppConfig>,
        prompts: PromptDocuments,
        reset_conversation: bool,
        reply: oneshot::Sender<std::result::Result<(), AdminFailure>>,
    },
    ResetConversation {
        session_id: Arc<str>,
        reply: oneshot::Sender<std::result::Result<(), AdminFailure>>,
    },
    ResetPersonaState {
        config: Box<AppConfig>,
        reply: oneshot::Sender<std::result::Result<(), AdminFailure>>,
    },
    ClearSessionContent {
        session_id: Arc<str>,
        reply: oneshot::Sender<std::result::Result<(), AdminFailure>>,
    },
    SwitchSession {
        session_id: String,
        release_reservation: bool,
        reply: oneshot::Sender<std::result::Result<(), AdminFailure>>,
    },
    Undo {
        session_id: Arc<str>,
        reply: oneshot::Sender<std::result::Result<Value, AdminFailure>>,
    },
    Pop {
        session_id: Arc<str>,
        turn_ids: Vec<String>,
        reply: oneshot::Sender<std::result::Result<Value, AdminFailure>>,
    },
    Compact {
        session_id: Arc<str>,
        reply: oneshot::Sender<std::result::Result<Value, AdminFailure>>,
    },
    Shutdown,
}

#[derive(Debug)]
pub(crate) enum AdminFailure {
    Invalid(String),
    Internal(String),
}

#[derive(Debug)]
pub(crate) enum PlatformSessionResetError {
    Busy,
    Unavailable,
    Internal(String),
}

#[derive(Debug)]
pub(crate) enum PlatformPersonaResetError {
    Busy,
    Unavailable,
    Internal(String),
}

#[derive(Clone, Debug)]
pub(crate) struct EventRecord {
    pub(crate) id: u64,
    pub(crate) kind: String,
    pub(crate) data: String,
}

#[derive(Clone)]
pub(crate) struct EventHub {
    inner: Arc<Mutex<EventHubInner>>,
    sender: broadcast::Sender<EventRecord>,
}

struct EventHubInner {
    next_id: u64,
    records: VecDeque<EventRecord>,
}

pub(crate) struct EventSubscription {
    pub(crate) pending: VecDeque<EventRecord>,
    pub(crate) receiver: broadcast::Receiver<EventRecord>,
}

impl EventHub {
    fn new() -> Self {
        let (sender, _) = broadcast::channel(EVENT_CAPACITY);
        Self {
            inner: Arc::new(Mutex::new(EventHubInner {
                next_id: 1,
                records: VecDeque::with_capacity(EVENT_CAPACITY),
            })),
            sender,
        }
    }

    pub(crate) fn publish(&self, kind: impl Into<String>, data: Value) -> u64 {
        let mut inner = self.inner.lock().unwrap();
        let id = inner.next_id;
        inner.next_id = inner.next_id.saturating_add(1);
        let record = EventRecord {
            id,
            kind: kind.into(),
            data: serde_json::to_string(&data)
                .unwrap_or_else(|_| "{\"error\":\"event serialization failed\"}".to_string()),
        };
        if inner.records.len() == EVENT_CAPACITY {
            inner.records.pop_front();
        }
        inner.records.push_back(record.clone());
        let _ = self.sender.send(record);
        id
    }

    pub(crate) fn latest_id(&self) -> u64 {
        self.inner.lock().unwrap().next_id.saturating_sub(1)
    }

    pub(crate) fn subscribe_after(&self, after: u64) -> EventSubscription {
        let mut inner = self.inner.lock().unwrap();
        let receiver = self.sender.subscribe();
        let pending = replay_records(&mut inner, after);
        EventSubscription { pending, receiver }
    }

    pub(crate) fn replay_after(&self, after: u64) -> VecDeque<EventRecord> {
        replay_records(&mut self.inner.lock().unwrap(), after)
    }
}

fn replay_records(inner: &mut EventHubInner, after: u64) -> VecDeque<EventRecord> {
    if after > inner.next_id.saturating_sub(1) {
        return resync_record(inner);
    }
    let Some(oldest) = inner.records.front().map(|record| record.id) else {
        return VecDeque::new();
    };
    if after < oldest.saturating_sub(1) {
        return resync_record(inner);
    }
    inner
        .records
        .iter()
        .filter(|record| record.id > after)
        .cloned()
        .collect()
}

fn resync_record(inner: &mut EventHubInner) -> VecDeque<EventRecord> {
    let id = inner.next_id;
    inner.next_id = inner.next_id.saturating_add(1);
    VecDeque::from([EventRecord {
        id,
        kind: "resync_required".to_string(),
        data: json!({ "latest_event_id": id }).to_string(),
    }])
}

#[derive(Clone)]
pub(crate) struct QuestionBroker {
    pending: Arc<Mutex<HashMap<String, PendingQuestion>>>,
}

struct PendingQuestion {
    run_id: String,
    request: QuestionRequest,
    responder: oneshot::Sender<QuestionResponse>,
}

#[derive(Debug)]
enum AnswerFailure {
    NotFound,
    Invalid(String),
    Gone,
}

impl QuestionBroker {
    fn new() -> Self {
        Self {
            pending: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    fn insert(
        &self,
        run_id: &str,
        request: QuestionRequest,
        responder: oneshot::Sender<QuestionResponse>,
    ) -> String {
        let mut pending = self.pending.lock().unwrap();
        loop {
            let question_id = random_id("question", 18);
            if !pending.contains_key(&question_id) {
                pending.insert(
                    question_id.clone(),
                    PendingQuestion {
                        run_id: run_id.to_string(),
                        request,
                        responder,
                    },
                );
                return question_id;
            }
        }
    }

    fn answer<F>(
        &self,
        question_id: &str,
        answers: QuestionAnswers,
        before_resume: F,
    ) -> std::result::Result<(), AnswerFailure>
    where
        F: FnOnce(&str, &QuestionAnswers),
    {
        let mut all_pending = self.pending.lock().unwrap();
        let request = all_pending
            .get(question_id)
            .map(|pending| pending.request.clone())
            .ok_or(AnswerFailure::NotFound)?;
        let answers = normalize_answers(&request, answers).map_err(AnswerFailure::Invalid)?;
        let pending = all_pending
            .remove(question_id)
            .ok_or(AnswerFailure::NotFound)?;
        let run_id = pending.run_id;
        if pending.responder.is_closed() {
            return Err(AnswerFailure::Gone);
        }
        before_resume(&run_id, &answers);
        pending
            .responder
            .send(QuestionResponse::Answered(answers.clone()))
            .map_err(|_| AnswerFailure::Gone)?;
        Ok(())
    }

    fn close<F>(
        &self,
        question_id: &str,
        before_resume: F,
    ) -> std::result::Result<(), AnswerFailure>
    where
        F: FnOnce(&str),
    {
        let mut all_pending = self.pending.lock().unwrap();
        let pending = all_pending
            .remove(question_id)
            .ok_or(AnswerFailure::NotFound)?;
        let run_id = pending.run_id;
        if pending.responder.is_closed() {
            return Err(AnswerFailure::Gone);
        }
        before_resume(&run_id);
        pending
            .responder
            .send(QuestionResponse::Closed)
            .map_err(|_| AnswerFailure::Gone)?;
        Ok(())
    }

    fn cancel_run(&self, run_id: &str) {
        let cancelled = {
            let mut pending = self.pending.lock().unwrap();
            let ids = pending
                .iter()
                .filter(|(_, question)| question.run_id == run_id)
                .map(|(id, _)| id.clone())
                .collect::<Vec<_>>();
            ids.into_iter()
                .filter_map(|id| pending.remove(&id))
                .collect::<Vec<_>>()
        };
        for pending in cancelled {
            let _ = pending.responder.send(QuestionResponse::Cancelled);
        }
    }
}

struct RunEventMapper {
    run_id: String,
    events: EventHub,
    questions: QuestionBroker,
    state_store: StateStore,
    manager: Arc<Mutex<ManagerState>>,
    turn_id: Option<String>,
    active_tools: Vec<ActiveTool>,
    queue_ingress: Option<Arc<crate::agent::QueueIngressBarrier>>,
    operation: &'static str,
    redo_input_id: Option<String>,
    redo_display_content: Option<String>,
    command_output_lines: usize,
}

struct ActiveTool {
    id: String,
    name: String,
    display_name: String,
    command_output: Option<crate::render::CommandOutputTail>,
}

impl RunEventMapper {
    fn new(
        run_id: String,
        events: EventHub,
        questions: QuestionBroker,
        state_store: StateStore,
        manager: Arc<Mutex<ManagerState>>,
        queue_ingress: Option<Arc<crate::agent::QueueIngressBarrier>>,
        operation: &'static str,
        redo_input_id: Option<String>,
        redo_display_content: Option<String>,
        command_output_lines: usize,
    ) -> Self {
        Self {
            run_id,
            events,
            questions,
            state_store,
            manager,
            turn_id: None,
            active_tools: Vec::new(),
            queue_ingress,
            operation,
            redo_input_id,
            redo_display_content,
            command_output_lines,
        }
    }

    fn publish(&self, kind: &str, data: Value) {
        self.events.publish(kind, data);
    }

    fn next_tool(&self, call_id: String, event_name: String) -> ActiveTool {
        let name = real_tool_name(&event_name).to_string();
        let display_name = tools::readable_tool_name(&event_name);
        ActiveTool {
            id: call_id,
            command_output: (name == "run_command")
                .then(|| crate::render::CommandOutputTail::new(self.command_output_lines)),
            name,
            display_name,
        }
    }

    fn handle(&mut self, event: AgentEvent) {
        match event {
            AgentEvent::TurnStarted { turn_id } => {
                self.turn_id = Some(turn_id.clone());
                if let Some(run) = self
                    .manager
                    .lock()
                    .unwrap()
                    .active_runs
                    .get_mut(&self.run_id)
                {
                    run.turn_id = Some(turn_id.clone());
                    run.queue_target = Some(self.state_store.queue_target(turn_id.clone()));
                }
                self.publish(
                    "turn.started",
                    json!({
                        "run_id": self.run_id,
                        "turn_id": turn_id,
                        "operation": self.operation,
                        "input_id": self.redo_input_id,
                        "display_content": self.redo_display_content,
                    }),
                );
            }
            AgentEvent::RawReasoning(_) => {}
            AgentEvent::FlushJournal => {}
            AgentEvent::Chunk(chunk) => match chunk.kind {
                ChatStreamKind::Content => self.publish(
                    "assistant.delta",
                    json!({ "run_id": self.run_id, "delta": chunk.text }),
                ),
                ChatStreamKind::Reasoning => self.publish(
                    "reasoning.delta",
                    json!({ "run_id": self.run_id, "delta": chunk.text }),
                ),
                _ => {}
            },
            AgentEvent::ReasoningStart { .. } => {
                self.publish("reasoning.start", json!({ "run_id": self.run_id }))
            }
            AgentEvent::ReasoningReset { .. } => {
                self.publish("reasoning.reset", json!({ "run_id": self.run_id }))
            }
            AgentEvent::ReasoningPartStart { .. } => {
                self.publish("reasoning.part_start", json!({ "run_id": self.run_id }))
            }
            AgentEvent::ReasoningPartEnd { .. } => {
                self.publish("reasoning.part_end", json!({ "run_id": self.run_id }))
            }
            AgentEvent::ReasoningTitle(title) => self.publish(
                "reasoning.title",
                json!({ "run_id": self.run_id, "title": title }),
            ),
            AgentEvent::ToolCall {
                call_id,
                name,
                arguments,
            } => {
                if let Some(queue_ingress) = self.queue_ingress.as_ref() {
                    queue_ingress.tool_started(&call_id);
                }
                let tool = self.next_tool(call_id, name);
                self.publish(
                    "tool.started",
                    json!({
                        "run_id": self.run_id,
                        "tool_id": tool.id,
                        "name": tool.name,
                        "display_name": tool.display_name,
                        "arguments": arguments,
                    }),
                );
                self.active_tools.push(tool);
            }
            // `name` is the raw tool name, matching `tool.started` — it used to
            // be the readable one here alone, which is an easy way to wire a
            // consumer to the wrong field. `tool_name` stays as an alias for
            // browsers still running a cached asset.
            AgentEvent::ToolPreparing { name } => self.publish(
                "tool.preparing",
                json!({
                    "run_id": self.run_id,
                    "name": &name,
                    "tool_name": &name,
                    "display_name": tools::readable_tool_name(&name),
                    // Sent so the WebUI label tracks the backend list instead
                    // of keeping its own copy in sync.
                    "phase": tools::preparing_phase(&name),
                }),
            ),
            AgentEvent::ToolProgress {
                call_id,
                name,
                message,
            } => {
                let (tool_id, tool_name) = self.tool_identity(&call_id, &name);
                self.publish(
                    "tool.progress",
                    json!({
                        "run_id": self.run_id,
                        "tool_id": tool_id,
                        "name": tool_name,
                        "message": message,
                    }),
                );
            }
            AgentEvent::CommandOutput {
                call_id,
                name,
                stream,
                chunk,
            } => {
                let stream_name = match stream {
                    CommandOutputStream::Stdout => "stdout",
                    CommandOutputStream::Stderr => "stderr",
                };
                let (tool_id, tool_name, preview) = if let Some(tool) =
                    self.active_tools.iter_mut().find(|tool| tool.id == call_id)
                {
                    let preview = tool.command_output.as_mut().map(|output| {
                        output.push(stream, &chunk);
                        output.preview()
                    });
                    (tool.id.clone(), tool.name.clone(), preview)
                } else {
                    (call_id.clone(), real_tool_name(&name).to_string(), None)
                };
                self.publish(
                    "tool.output",
                    json!({
                        "run_id": self.run_id,
                        "tool_id": tool_id,
                        "name": tool_name,
                        "stream": stream_name,
                        "output": String::from_utf8_lossy(&chunk),
                        "preview": preview,
                    }),
                );
            }
            AgentEvent::ToolResult {
                call_id,
                name,
                ok,
                output,
            } => {
                if let Some(queue_ingress) = self.queue_ingress.as_ref() {
                    queue_ingress.tool_finished(&call_id);
                }
                let mut tool = self
                    .active_tools
                    .iter()
                    .position(|tool| tool.id == call_id)
                    .map(|index| self.active_tools.remove(index))
                    .unwrap_or_else(|| self.next_tool(call_id, name));
                let preview = tool.command_output.as_mut().map(|output| {
                    output.finalize();
                    output.preview()
                });
                self.publish(
                    "tool.finished",
                    json!({
                        "run_id": self.run_id,
                        "tool_id": tool.id,
                        "name": tool.name,
                        "display_name": tool.display_name,
                        "ok": ok,
                        "output": output,
                        "preview": preview,
                    }),
                );
            }
            AgentEvent::PrepareForExternalOutput { ready } => {
                let _ = ready.send(false);
            }
            AgentEvent::Image {
                call_id,
                name,
                path,
                alt,
            } => {
                let (tool_id, tool_name) = self.tool_identity(&call_id, &name);
                let hide_caption = tool_name == "show_meme";
                let Some(turn_id) = self.turn_id.as_deref() else {
                    self.publish(
                        "tool.image",
                        json!({
                            "run_id": self.run_id,
                            "tool_id": tool_id,
                            "name": tool_name,
                            "error": "image could not be associated with the current turn",
                        }),
                    );
                    return;
                };
                match self
                    .state_store
                    .save_image_asset(turn_id, Some(&tool_id), &path, &alt)
                {
                    Ok(asset) => self.publish(
                        "tool.image",
                        json!({
                            "run_id": self.run_id,
                            "tool_id": tool_id,
                            "name": tool_name,
                            "asset": SafeImageAsset::from_asset(asset, hide_caption),
                        }),
                    ),
                    Err(error) => {
                        tracing::warn!(
                            run_id = %self.run_id,
                            tool = %tool_name,
                            error = %error,
                            "{}",
                            t("failed to persist a WebUI image", "WebUI 图像保存失败")
                        );
                        self.publish(
                            "tool.image",
                            json!({
                                "run_id": self.run_id,
                                "tool_id": tool_id,
                                "name": tool_name,
                                "error": "image could not be added to the WebUI",
                            }),
                        );
                    }
                }
            }
            AgentEvent::Artifact {
                call_id,
                name,
                path,
                title,
            } => {
                let (tool_id, tool_name) = self.tool_identity(&call_id, &name);
                let Some(turn_id) = self.turn_id.as_deref() else {
                    self.publish(
                        "tool.artifact",
                        json!({
                            "run_id": self.run_id,
                            "tool_id": tool_id,
                            "name": tool_name,
                            "error": "artifact could not be associated with the current turn",
                        }),
                    );
                    return;
                };
                match self
                    .state_store
                    .save_artifact_asset(turn_id, Some(&tool_id), &path, &title)
                {
                    Ok(asset) => self.publish(
                        "tool.artifact",
                        json!({
                            "run_id": self.run_id,
                            "tool_id": tool_id,
                            "name": tool_name,
                            "artifact": SafeArtifactAsset::from(asset),
                        }),
                    ),
                    Err(error) => {
                        tracing::warn!(run_id = %self.run_id, tool = %tool_name, error = %error, "failed to persist a WebUI artifact");
                        self.publish(
                            "tool.artifact",
                            json!({
                                "run_id": self.run_id,
                                "tool_id": tool_id,
                                "name": tool_name,
                                "error": "file could not be added to the WebUI preview",
                            }),
                        );
                    }
                }
            }
            AgentEvent::AskQuestion {
                call_id,
                request,
                responder,
            } => {
                let question_id = self
                    .questions
                    .insert(&self.run_id, request.clone(), responder);
                let (tool_id, tool_name) = self.tool_identity(&call_id, "ask_question");
                self.publish(
                    "question.requested",
                    json!({
                        "run_id": self.run_id,
                        "question_id": question_id,
                        "tool_id": tool_id,
                        "name": tool_name,
                        "questions": request.questions,
                    }),
                );
            }
            AgentEvent::QueuedPromptsConsumed {
                prompt_ids,
                mode,
                provider_id,
                model,
            } => self.publish(
                "queue.consumed",
                json!({
                    "run_id": self.run_id,
                    "prompt_ids": prompt_ids,
                    "mode": mode_name(mode),
                    "provider_id": provider_id,
                    "model": model,
                }),
            ),
            AgentEvent::GenerationSuperseded { prompt_ids } => self.publish(
                "generation.superseded",
                json!({
                    "run_id": self.run_id,
                    "turn_id": self.turn_id,
                    "prompt_ids": prompt_ids,
                }),
            ),
            AgentEvent::SpinnerTick => {}
            AgentEvent::CompactStart => {
                self.publish("context.compact_start", json!({ "run_id": self.run_id }))
            }
            AgentEvent::CompactChunk(chunk) => self.publish(
                "context.compact_delta",
                json!({ "run_id": self.run_id, "delta": chunk.text }),
            ),
            AgentEvent::CompactEnd => {
                self.publish("context.compact_end", json!({ "run_id": self.run_id }))
            }
            AgentEvent::PopStart => {
                self.publish("context.pop_start", json!({ "run_id": self.run_id }))
            }
            AgentEvent::PopEnd => self.publish("context.pop_end", json!({ "run_id": self.run_id })),
            AgentEvent::Notice { text } => self.publish(
                "context.notice",
                json!({ "run_id": self.run_id, "text": text }),
            ),
        }
    }

    fn tool_identity(&self, call_id: &str, fallback: &str) -> (String, String) {
        self.active_tools
            .iter()
            .find(|tool| tool.id == call_id)
            .map(|tool| (tool.id.clone(), tool.name.clone()))
            .unwrap_or_else(|| (call_id.to_string(), real_tool_name(fallback).to_string()))
    }
}

#[derive(Debug)]
pub(crate) struct ApiError {
    status: StatusCode,
    pub(crate) message: String,
}

impl ApiError {
    fn new(status: StatusCode, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
        }
    }

    fn internal(error: impl std::fmt::Display) -> Self {
        tracing::error!(error = %error, "{}", t("WebUI request failed", "WebUI 请求失败"));
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, "internal server error")
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (
            self.status,
            Json(json!({ "error": { "message": self.message } })),
        )
            .into_response()
    }
}

#[derive(Default, Deserialize)]
struct EventsQuery {
    #[serde(default)]
    after: u64,
}

#[derive(Deserialize)]
struct AttachmentQuery {
    session_id: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CreateTurnRequest {
    content: String,
    mode: String,
    #[serde(default)]
    attachment_ids: Vec<String>,
    /// Target session; defaults to the global current session. The turn runs
    /// there without moving the current pointer (per-view WebUI sessions).
    #[serde(default)]
    session_id: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct QueuePromptRequest {
    content: String,
    run_id: String,
    turn_id: String,
    #[serde(default)]
    attachment_ids: Vec<String>,
    /// Target session; defaults to the global current session.
    #[serde(default)]
    session_id: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TurnUpdateMode {
    Followup,
    Supersede,
}

pub(crate) struct TurnUpdateRequest {
    pub(crate) run_id: String,
    pub(crate) turn_id: String,
    pub(crate) session_id: Option<Arc<str>>,
    pub(crate) audience: PromptAudience,
    pub(crate) content: String,
    pub(crate) display_content: String,
    pub(crate) attachments: Vec<crate::state::QueuedPromptAttachment>,
    pub(crate) uploaded_attachment_ids: Vec<String>,
    pub(crate) mode: TurnUpdateMode,
}

pub(crate) struct TurnUpdateReceipt {
    pub(crate) run_id: String,
    pub(crate) turn_id: String,
    pub(crate) session_id: Arc<str>,
    pub(crate) prompt: QueuedPrompt,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RedoTurnRequest {
    expected_revision: i64,
    input_id: String,
    #[serde(default)]
    content: Option<String>,
    mode: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AnswerQuestionRequest {
    answers: QuestionAnswers,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SetModelsRequest {
    models: Vec<ActiveProviderModelConfig>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ThinkingVariantUpdate {
    provider_id: String,
    model: String,
    selected: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SetThinkingVariantsRequest {
    updates: Vec<ThinkingVariantUpdate>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LoginRequest {
    password: String,
}

const QQ_GROUP_MANAGEMENT_PLUGIN_ID: &str = "qq_group_management";
const QQ_GROUP_MANAGEMENT_PLATFORM: &str = "onebot";

#[derive(Deserialize)]
struct QqGroupHistoryQuery {
    account_id: String,
    group_id: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct QqGroupHistoryClearRequest {
    account_id: String,
    group_id: String,
    kind: String,
}

fn qq_group_scope(
    account_id: &str,
    group_id: &str,
) -> std::result::Result<PlatformPluginScopeKey, ApiError> {
    if !valid_qq_id(account_id) || !valid_qq_id(group_id) {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "account_id and group_id must be numeric QQ ids",
        ));
    }
    Ok(PlatformPluginScopeKey {
        plugin_id: QQ_GROUP_MANAGEMENT_PLUGIN_ID.to_string(),
        platform: QQ_GROUP_MANAGEMENT_PLATFORM.to_string(),
        account_id: account_id.to_string(),
        conversation_kind: "group".to_string(),
        conversation_id: group_id.to_string(),
    })
}

fn valid_qq_id(value: &str) -> bool {
    (5..=12).contains(&value.len()) && value.bytes().all(|byte| byte.is_ascii_digit())
}

async fn qq_group_history_http(
    State(state): State<DaemonState>,
    headers: HeaderMap,
    Query(query): Query<QqGroupHistoryQuery>,
) -> std::result::Result<Response, ApiError> {
    require_auth(&headers, &state)?;
    let scope = qq_group_scope(&query.account_id, &query.group_id)?;
    let offenders = state
        .state_store
        .plugin_get_json::<Value>(&scope, "offender_history")
        .map_err(ApiError::internal)?
        .unwrap_or_else(|| json!({}));
    let kicks = state
        .state_store
        .plugin_get_json::<Value>(&scope, "kick_history")
        .map_err(ApiError::internal)?
        .unwrap_or_else(|| json!([]));
    let connected_accounts = state
        .platforms
        .onebot
        .lock()
        .unwrap()
        .connected_accounts()
        .into_iter()
        .map(|account| account.to_string())
        .collect::<Vec<_>>();
    Ok(Json(json!({
        "account_id": query.account_id,
        "group_id": query.group_id,
        "offenders": offenders.clone(),
        "kicks": kicks.clone(),
        "offender_history": offenders,
        "kick_history": kicks,
        "connected_accounts": connected_accounts,
    }))
    .into_response())
}

async fn qq_group_history_clear_http(
    State(state): State<DaemonState>,
    headers: HeaderMap,
    Json(request): Json<QqGroupHistoryClearRequest>,
) -> std::result::Result<Response, ApiError> {
    require_mutation(&headers, &state)?;
    let scope = qq_group_scope(&request.account_id, &request.group_id)?;
    let key = match request.kind.as_str() {
        "offenders" => "offender_history",
        "kicks" => "kick_history",
        _ => {
            return Err(ApiError::new(
                StatusCode::BAD_REQUEST,
                "kind must be offenders or kicks",
            ))
        }
    };
    state
        .state_store
        .plugin_delete_key(&scope, key)
        .map_err(ApiError::internal)?;
    Ok(Json(json!({ "ok": true })).into_response())
}

async fn qq_group_offender_delete_http(
    State(state): State<DaemonState>,
    headers: HeaderMap,
    Path(user_id): Path<String>,
    Query(query): Query<QqGroupHistoryQuery>,
) -> std::result::Result<Response, ApiError> {
    require_mutation(&headers, &state)?;
    if !valid_qq_id(&user_id) {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "user_id must be a numeric QQ id",
        ));
    }
    let scope = qq_group_scope(&query.account_id, &query.group_id)?;
    state
        .state_store
        .plugin_update_json::<HashMap<String, Value>, _>(&scope, "offender_history", |current| {
            let mut records = current.unwrap_or_default();
            records.remove(&user_id);
            Ok(if records.is_empty() {
                None
            } else {
                Some(records)
            })
        })
        .map_err(ApiError::internal)?;
    Ok(Json(json!({ "ok": true })).into_response())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct UpdateConfigRequest {
    config: Value,
    #[serde(default)]
    secrets: HashMap<String, SecretMutation>,
    prompts: PromptDocuments,
    #[serde(default)]
    reset_conversation: bool,
}

#[derive(Deserialize)]
#[serde(tag = "action", content = "value", rename_all = "snake_case")]
enum SecretMutation {
    Set(String),
    Clear,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PromptDocuments {
    #[serde(default)]
    personas: Vec<PromptDocument>,
    #[serde(default)]
    identities: Vec<PromptDocument>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct PromptDocument {
    name: String,
    content: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    avatar_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    board_image_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    board_title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    board_subtitle: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    starter_prompts: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    original_name: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
struct PersonaMetadata {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    avatar_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    board_image_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    board_title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    board_subtitle: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    starter_prompts: Option<Vec<String>>,
}

#[derive(Serialize)]
struct ConfigResponse {
    config: Value,
    secret_states: HashMap<String, bool>,
    prompts: PromptDocuments,
    models: Vec<SafeModel>,
    multimodal_models: Vec<SafeModel>,
    display: WebDisplayConfig,
    context: ContextSnapshot,
    persona: PersonaIdentity,
}

#[derive(Clone, Debug, Serialize)]
struct PersonaIdentity {
    name: String,
    avatar_url: Option<String>,
    board_image_url: Option<String>,
    board_title: String,
    board_subtitle: String,
    starter_prompts: Vec<String>,
}

#[derive(Serialize)]
struct BootstrapResponse {
    version: &'static str,
    boot_id: String,
    latest_event_id: u64,
    active_run_id: Option<String>,
    running_turn_id: Option<String>,
    external_queue_available: bool,
    turns: Vec<SafeTurn>,
    queued_prompts: Vec<SafeQueuedPrompt>,
    models: Vec<SafeModel>,
    display: WebDisplayConfig,
    context: ContextSnapshot,
    usage: SafeUsageSnapshot,
    capabilities: Capabilities,
    sessions: Vec<Value>,
    current_session_id: String,
    /// Every turn currently running, across all sessions.
    runs: Vec<Value>,
    persona: PersonaIdentity,
    redo_candidate: Option<SafeRedoCandidate>,
}

#[derive(Serialize)]
struct Capabilities {
    multi_conversation: bool,
    attachments: bool,
    queue: bool,
    redo: bool,
}

#[derive(Clone, Serialize)]
struct WebDisplayConfig {
    reasoning: String,
    tool_calls: String,
    readable_tool_names: bool,
    command_output_lines: usize,
    mixed_model_endpoint_display: String,
    show_mixed_model_endpoint: bool,
}

#[derive(Clone, Serialize)]
pub(crate) struct SafeQueuedPrompt {
    id: String,
    content: String,
    submitted_at: String,
    attachments: Vec<SafeUserAttachment>,
}

#[derive(Serialize)]
struct SafeModel {
    provider_id: String,
    provider_name: String,
    model: String,
    active: bool,
}

#[derive(Serialize)]
struct SafeTurn {
    id: String,
    seq: i64,
    status: &'static str,
    active_context: bool,
    user_content: String,
    assistant_content: String,
    assistant_reasoning: Option<String>,
    provider_id: Option<String>,
    model: Option<String>,
    user_timestamp: String,
    assistant_timestamp: Option<String>,
    token_total: u64,
    token_prompt: u64,
    token_cache_read: u64,
    token_usage_estimated: bool,
    question_exchanges: Vec<crate::question::QuestionExchange>,
    followups: Vec<SafeFollowup>,
    assets: Vec<SafeImageAsset>,
    artifacts: Vec<SafeArtifactAsset>,
    attachments: Vec<SafeUserAttachment>,
    revision: i64,
}

#[derive(Serialize)]
struct SafeRedoCandidate {
    turn_id: String,
    revision: i64,
    input_id: String,
    input_kind: &'static str,
    content: String,
}

impl From<crate::state::RedoCandidate> for SafeRedoCandidate {
    fn from(candidate: crate::state::RedoCandidate) -> Self {
        Self {
            turn_id: candidate.turn_id,
            revision: candidate.revision,
            input_id: candidate.input_id,
            input_kind: match candidate.input_kind {
                crate::state::RedoInputKind::Initial => "initial",
                crate::state::RedoInputKind::Followup => "followup",
            },
            content: candidate.display_content,
        }
    }
}

#[derive(Serialize)]
struct SafeFollowup {
    id: String,
    content: String,
    submitted_at: String,
    preceding_assistant_content: Option<String>,
    preceding_assistant_reasoning: Option<String>,
    provider_id: Option<String>,
    model: Option<String>,
    attachments: Vec<SafeUserAttachment>,
}

#[derive(Clone, Serialize)]
struct SafeUserAttachment {
    id: String,
    url: String,
    name: String,
    mime: String,
    kind: String,
    size: u64,
    width: u32,
    height: u32,
}

#[derive(Serialize)]
struct SafeImageAsset {
    id: String,
    url: String,
    mime: String,
    width: u32,
    height: u32,
    alt: String,
    hide_caption: bool,
}

#[derive(Clone, Serialize)]
struct SafeArtifactAsset {
    id: String,
    url: String,
    name: String,
    mime: String,
    kind: String,
    type_label: String,
    size: u64,
    updated_at: String,
}

#[derive(Serialize)]
struct SafeUsageSnapshot {
    requests: u64,
    prompt_tokens: u64,
    completion_tokens: u64,
    total_tokens: u64,
    conversation_tokens: u64,
    cache_read_tokens: u64,
    cache_write_tokens: u64,
    reasoning_tokens: u64,
    last_usage: Option<Usage>,
    last_conversation_usage: Option<Usage>,
}

#[derive(Serialize)]
struct ModelResponse {
    models: Vec<SafeModel>,
    display: WebDisplayConfig,
    context: ContextSnapshot,
}

#[derive(Serialize)]
struct ThinkingVariantsResponse {
    options: Vec<ThinkingVariantOptions>,
}

pub async fn run(paths: MiyuPaths, args: WebArgs) -> Result<()> {
    let password = resolve_web_password(&args)?;
    AppConfig::init_files(&paths)?;
    let config = AppConfig::load_or_default(&paths)?;
    tools::jobs::init(&paths);
    let state_store = StateStore::new(&paths)?;
    state_store.init_files()?;
    let persona = config.active_persona_scope();
    state_store.adopt_sessions_for_persona(&persona)?;
    ensure_local_current_session(&state_store, &persona)?;
    // Subagent audit sessions are kept for a week, cleaned at startup and
    // then daily while the daemon runs. One-shot `ask` sessions delete
    // themselves as their turn ends, so the hour-old survivors swept here are
    // strictly orphans from a client that died mid-turn.
    const SUBAGENT_AUDIT_RETENTION_DAYS: i64 = 7;
    const ASK_SESSION_RETENTION_HOURS: i64 = 1;
    let _ = state_store.delete_subagent_sessions_older_than(SUBAGENT_AUDIT_RETENTION_DAYS);
    let _ = state_store.delete_ask_sessions_older_than(ASK_SESSION_RETENTION_HOURS);
    {
        let store = state_store.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(24 * 60 * 60));
            interval.tick().await;
            loop {
                interval.tick().await;
                let _ = store.delete_subagent_sessions_older_than(SUBAGENT_AUDIT_RETENTION_DAYS);
                let _ = store.delete_ask_sessions_older_than(ASK_SESSION_RETENTION_HOURS);
            }
        });
    }
    let context = cold_context(&config, &state_store)?;

    // Default binds loopback only so an unauthenticated WebUI is never
    // exposed to the LAN; `--bind 0.0.0.0` opts back in explicitly. Access
    // URLs matching the effective bind are printed below.
    let bind_ip = args.bind.unwrap_or(IpAddr::V4(Ipv4Addr::LOCALHOST));
    let listener = match tokio::net::TcpListener::bind(SocketAddr::new(bind_ip, args.port)).await {
        Ok(listener) => listener,
        Err(error)
            if args.port == ipc::DEFAULT_WEB_PORT
                && error.kind() == std::io::ErrorKind::AddrInUse =>
        {
            tracing::warn!(
                requested_port = args.port,
                "{}",
                t(
                    "Miyu WebUI default port is occupied; selecting an ephemeral port",
                    "Miyu WebUI 默认端口已被占用；将选择临时端口"
                )
            );
            tokio::net::TcpListener::bind(SocketAddr::new(bind_ip, 0))
                .await
                .context("binding Miyu WebUI to an ephemeral fallback port")?
        }
        Err(error) => {
            return Err(error)
                .with_context(|| format!("binding Miyu WebUI to {bind_ip}:{}", args.port));
        }
    };
    let port = listener.local_addr()?.port();
    let boot_id: Arc<str> = random_id("boot", 18).into();
    let events = EventHub::new();
    let questions = QuestionBroker::new();
    let manager = Arc::new(Mutex::new(ManagerState {
        config: config.clone(),
        active_runs: HashMap::new(),
        admin_busy: false,
        context,
        persona_session_ids: HashMap::from([(
            config.active_persona_scope(),
            state_store.session_id().to_string(),
        )]),
    }));
    let turn_engine = TurnEngineState::default();
    let memory_organizer = MemoryOrganizer::spawn()?;
    let memory_organizer_handle = memory_organizer.handle();
    memory_organizer_handle.wake(config.clone(), paths.clone(), state_store.clone());
    let (actor_tx, actor_join) = spawn_actor(
        config,
        paths.clone(),
        state_store.clone(),
        manager.clone(),
        events.clone(),
        questions.clone(),
        turn_engine.clone(),
        Some(memory_organizer_handle),
    )?;
    let (shutdown_tx, mut shutdown_rx) = broadcast::channel(1);
    let state = DaemonState {
        auth: WebAuth::new(password.as_deref()),
        boot_id,
        web_port: port,
        web_public: !bind_ip.is_loopback(),
        web_bind: bind_ip,
        paths,
        manager,
        state_store,
        events,
        questions,
        actor_tx: actor_tx.clone(),
        shutdown_tx,
        turn_engine,
        platforms: PlatformRuntime::new()?,
    };
    let initial_qq = state.manager.lock().unwrap().config.platforms.qq.clone();
    state
        .platforms
        .qq_listener
        .prepare(&state, None, &initial_qq)
        .await?
        .commit();
    let (ipc_lease, ipc_task) = start_ipc_server(&state)?;
    install_background_job_hook(&state);
    let app = router(state.clone());
    let urls = ipc::web_access_urls_for(bind_ip, port);
    for url in &urls {
        println!("Miyu WebUI: {url}");
    }
    std::io::stdout().flush().ok();

    let serve_result = {
        let server = axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .into_future();
        tokio::pin!(server);
        tokio::select! {
            result = &mut server => result,
            _ = shutdown_signal() => Ok(()),
            _ = shutdown_rx.recv() => Ok(()),
        }
    };
    let _ = actor_tx.send(ActorCommand::Shutdown);
    tools::jobs::shutdown_all();
    state.platforms.qq_listener.shutdown(&state).await;
    ipc_task.abort();
    let _ = ipc_task.await;
    let actor_result = tokio::task::spawn_blocking(move || actor_join.join())
        .await
        .context("joining WebUI actor task")?
        .map_err(|_| anyhow::anyhow!("WebUI actor thread panicked"))?;
    memory_organizer.shutdown();
    drop(ipc_lease);
    serve_result.context("serving Miyu WebUI")?;
    actor_result
}

/// Old WebUI versions could make a platform-owned conversation the global
/// current session. Repair that pointer before constructing the local agent
/// so QQ history can never become the WebUI/CLI startup conversation.
fn ensure_local_current_session(state_store: &StateStore, persona: &str) -> Result<()> {
    let current_session_id = state_store.session_id();
    if is_available_local_session(state_store, &current_session_id, persona)? {
        return Ok(());
    }

    let target_session_id = match state_store
        .list_local_sessions(persona, false)?
        .into_iter()
        .next()
    {
        Some(overview) => overview.record.session_id,
        None => {
            state_store
                .create_session(persona, "", "user", None)?
                .session_id
        }
    };
    state_store.switch_session(&target_session_id)
}

fn is_available_local_session(
    state_store: &StateStore,
    session_id: &str,
    persona: &str,
) -> Result<bool> {
    let usable = state_store
        .session_record(session_id)?
        .is_some_and(|record| {
            record.persona == persona && record.kind == "user" && !record.archived
        });
    Ok(usable && !state_store.is_platform_session(session_id)?)
}

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
        if !self.finished {
            // Client disconnected mid-turn: cancel its run.
            if let Some(info) = self.manager.lock().unwrap().active_runs.get(&self.run_id) {
                info.request_cancel();
            }
        }
    }
}

fn start_ipc_server(
    state: &DaemonState,
) -> Result<(crate::ipc::WebCoreLease, TokioJoinHandle<()>)> {
    let lease = ipc::acquire_web_core(&state.paths)
        .context("another Miyu core is already running or starting")?;
    let socket_path = state.paths.ipc_socket();
    let listener = tokio::net::UnixListener::bind(&socket_path)
        .with_context(|| format!("binding Miyu IPC socket at {}", socket_path.display()))?;
    std::fs::set_permissions(&socket_path, std::fs::Permissions::from_mode(0o600))?;

    let server_state = state.clone();
    let permits = Arc::new(Semaphore::new(32));
    let task = tokio::spawn(async move {
        loop {
            let (stream, _) = match listener.accept().await {
                Ok(connection) => connection,
                Err(error) => {
                    tracing::warn!(
                        error = %error,
                        "{}",
                        t("Miyu IPC listener stopped", "Miyu IPC 监听器已停止")
                    );
                    break;
                }
            };
            let permit = match permits.clone().acquire_owned().await {
                Ok(permit) => permit,
                Err(_) => break,
            };
            let connection_state = server_state.clone();
            tokio::spawn(async move {
                let _permit = permit;
                if let Err(error) = handle_ipc_connection(connection_state, stream).await {
                    tracing::debug!(
                        error = %error,
                        "{}",
                        t(
                            "Miyu IPC connection closed with an error",
                            "Miyu IPC 连接因错误关闭"
                        )
                    );
                }
            });
        }
    });
    Ok((lease, task))
}

async fn handle_ipc_connection(
    state: DaemonState,
    mut stream: tokio::net::UnixStream,
) -> Result<()> {
    let Some(request) = tokio::time::timeout(
        Duration::from_secs(5),
        ipc::receive::<IpcRequest>(&mut stream),
    )
    .await
    .context("timed out waiting for a Miyu IPC request")??
    else {
        return Ok(());
    };
    if request.version != ipc::PROTOCOL_VERSION
        && !matches!(&request.command, IpcCommand::Ping | IpcCommand::Shutdown)
    {
        ipc::send(
            &mut stream,
            &IpcFrame::error(format!(
                "unsupported IPC protocol version {}; expected {}",
                request.version,
                ipc::PROTOCOL_VERSION
            )),
        )
        .await?;
        return Ok(());
    }

    match request.command {
        IpcCommand::Ping => {
            ipc::send(
                &mut stream,
                &IpcFrame::Ready {
                    pid: std::process::id(),
                    web_port: state.web_port,
                    web_public: state.web_public,
                    web_bind: Some(state.web_bind),
                    build_id: ipc::BUILD_ID.to_string(),
                },
            )
            .await?;
        }
        IpcCommand::Shutdown => {
            ipc::send(&mut stream, &IpcFrame::Ack).await?;
            let _ = state.shutdown_tx.send(());
        }
        IpcCommand::JobsOverview => {
            let wake_runs = {
                let manager = state.manager.lock().unwrap();
                manager
                    .active_runs
                    .iter()
                    .filter(|(_, info)| info.job_wake)
                    .map(|(run_id, info)| {
                        json!({
                            "run_id": run_id,
                            "session_id": &*info.session_id,
                            "label": info.job_wake_label,
                        })
                    })
                    .collect::<Vec<_>>()
            };
            ipc::send(
                &mut stream,
                &IpcFrame::AdminResult {
                    state: session_state(&state.manager, &state.state_store)?,
                    data: json!({ "jobs": tools::jobs::overview(), "wake_runs": wake_runs }),
                },
            )
            .await?;
        }
        IpcCommand::FollowRun { run_id } => {
            follow_run(&state, &mut stream, run_id).await?;
        }
        IpcCommand::StopSessionJobs { session_id } => {
            let stopped = tools::jobs::stop_session_jobs(&session_id).await;
            state
                .events
                .publish("job.acknowledged", json!({ "session_id": session_id }));
            ipc::send(
                &mut stream,
                &IpcFrame::AdminResult {
                    state: session_state(&state.manager, &state.state_store)?,
                    data: json!({ "stopped": stopped }),
                },
            )
            .await?;
        }
        IpcCommand::GetStatus => {
            let qq_enabled = state.manager.lock().unwrap().config.platforms.qq.enabled;
            let qq_port = state.platforms.qq_listener.active_port();
            let connected_accounts = state.platforms.onebot.lock().unwrap().connected_accounts();
            ipc::send(
                &mut stream,
                &IpcFrame::AdminResult {
                    state: session_state(&state.manager, &state.state_store)?,
                    data: json!({
                        "runtime": {
                            "turn_engine": state.turn_engine.label(),
                        },
                        "platforms": {
                            "qq": {
                                "enabled": qq_enabled,
                                "listen_port": qq_port,
                                "connected_accounts": connected_accounts,
                            }
                        }
                    }),
                },
            )
            .await?;
        }
        IpcCommand::GetReplSession => {
            let persona = active_persona_scope(&state);
            let store = &state.state_store;
            // A stale pointer (session deleted or archived elsewhere) must not
            // strand the REPL: fall back to the terminal session and heal the
            // pointer so the next start is a plain read.
            let session_id = store
                .repl_session(&persona)
                .ok()
                .flatten()
                .unwrap_or_else(|| store.session_id().to_string());
            let target = ipc::SessionRef::Id { id: session_id };
            let session_id = match resolve_available_local_session_ref(&state, &target) {
                Ok(record) => record.session_id,
                Err(_) => store.session_id().to_string(),
            };
            let _ = store.set_repl_session(&persona, &session_id);
            ipc::send(
                &mut stream,
                &IpcFrame::AdminResult {
                    state: session_state_for(&state, &session_id)?,
                    data: json!({}),
                },
            )
            .await?;
        }
        IpcCommand::GetSessionState { target } => {
            let record = match resolve_available_local_session_ref(&state, &target) {
                Ok(record) => record,
                Err(message) => {
                    ipc::send(&mut stream, &IpcFrame::error(message)).await?;
                    return Ok(());
                }
            };
            ipc::send(
                &mut stream,
                &IpcFrame::AdminResult {
                    state: session_state_for(&state, &record.session_id)?,
                    data: json!({}),
                },
            )
            .await?;
        }
        IpcCommand::ReloadConfig => {
            let current_config = state.manager.lock().unwrap().config.clone();
            let next_config = match AppConfig::load_or_default(&state.paths) {
                Ok(config) => config,
                Err(error) => {
                    ipc::send(
                        &mut stream,
                        &IpcFrame::error(format!(
                            "invalid configuration: {}",
                            safe_error_message(error)
                        )),
                    )
                    .await?;
                    return Ok(());
                }
            };
            let prompts = match read_prompt_documents(&next_config, &state.paths) {
                Ok(prompts) => prompts,
                Err(error) => {
                    ipc::send(&mut stream, &IpcFrame::error(safe_error_message(error))).await?;
                    return Ok(());
                }
            };
            let qq_listener = match state
                .platforms
                .qq_listener
                .prepare(
                    &state,
                    Some(&current_config.platforms.qq),
                    &next_config.platforms.qq,
                )
                .await
            {
                Ok(listener) => listener,
                Err(error) => {
                    ipc::send(
                        &mut stream,
                        &IpcFrame::error(format!(
                            "Tencent QQ listener configuration failed: {}",
                            safe_error_message(error)
                        )),
                    )
                    .await?;
                    return Ok(());
                }
            };
            // Light reservation: reloading is allowed while turns run. Running
            // turns keep the config snapshot they started with; new turns pick
            // up the reloaded config. Persona layout changes interrupt running
            // turns inside the ApplyConfig handler instead of failing here.
            if let Err(error) = reserve_admin_light(&state.manager) {
                ipc::send(
                    &mut stream,
                    &IpcFrame::coded_error(ipc::ErrorCode::Busy, error.message),
                )
                .await?;
                return Ok(());
            }
            let (reply, receiver) = oneshot::channel();
            if state
                .actor_tx
                .send(ActorCommand::ApplyConfig {
                    config: Box::new(next_config),
                    prompts,
                    reset_conversation: false,
                    reply,
                })
                .is_err()
            {
                release_admin(&state.manager);
                ipc::send(
                    &mut stream,
                    &IpcFrame::error("Miyu core worker is unavailable"),
                )
                .await?;
                return Ok(());
            }
            match receiver.await {
                Ok(Ok(())) => {
                    qq_listener.commit();
                    match session_state(&state.manager, &state.state_store) {
                        Ok(session) => {
                            ipc::send(
                                &mut stream,
                                &IpcFrame::AdminResult {
                                    state: session,
                                    data: json!({}),
                                },
                            )
                            .await?
                        }
                        Err(error) => {
                            ipc::send(&mut stream, &IpcFrame::error(safe_error_message(error)))
                                .await?
                        }
                    }
                }
                Ok(Err(AdminFailure::Invalid(message) | AdminFailure::Internal(message))) => {
                    ipc::send(&mut stream, &IpcFrame::error(message)).await?
                }
                Err(_) => {
                    release_admin(&state.manager);
                    ipc::send(
                        &mut stream,
                        &IpcFrame::error("Miyu core stopped while reloading configuration"),
                    )
                    .await?
                }
            }
        }
        IpcCommand::ResetConversation { target } => {
            let target_record = match resolve_available_local_session_ref(&state, &target) {
                Ok(record) => record,
                Err(message) => {
                    ipc::send(&mut stream, &IpcFrame::error(message)).await?;
                    return Ok(());
                }
            };
            let session_id: Arc<str> = target_record.session_id.into();
            reserve_admin_for_session(&state.manager, &session_id)
                .map_err(|error| anyhow::anyhow!(error.message))?;
            let (reply, receiver) = oneshot::channel();
            if state
                .actor_tx
                .send(ActorCommand::ResetConversation {
                    session_id: session_id.clone(),
                    reply,
                })
                .is_err()
            {
                release_admin(&state.manager);
                anyhow::bail!("Miyu core worker is unavailable");
            }
            match receiver.await {
                Ok(Ok(())) => {
                    ipc::send(
                        &mut stream,
                        &IpcFrame::AdminResult {
                            state: session_state_for(&state, &session_id)?,
                            data: json!({}),
                        },
                    )
                    .await?
                }
                Ok(Err(AdminFailure::Invalid(message) | AdminFailure::Internal(message))) => {
                    ipc::send(&mut stream, &IpcFrame::error(message)).await?
                }
                Err(_) => {
                    release_admin(&state.manager);
                    anyhow::bail!("Miyu core stopped while resetting the conversation");
                }
            }
        }
        IpcCommand::WipePersona => {
            let config = state.manager.lock().unwrap().config.clone();
            let current = state.state_store.session_id().to_string();
            match reset_platform_persona_state(&state, &config).await {
                Ok(sessions) => {
                    ipc::send(
                        &mut stream,
                        &IpcFrame::AdminResult {
                            state: session_state_for(&state, &current)?,
                            data: json!({ "sessions": sessions }),
                        },
                    )
                    .await?;
                }
                Err(PlatformPersonaResetError::Busy) => {
                    ipc::send(
                        &mut stream,
                        &IpcFrame::coded_error(ipc::ErrorCode::Busy, ipc::ADMIN_BUSY_MESSAGE),
                    )
                    .await?;
                }
                Err(PlatformPersonaResetError::Unavailable) => {
                    anyhow::bail!("Miyu core worker is unavailable");
                }
                Err(PlatformPersonaResetError::Internal(message)) => {
                    ipc::send(&mut stream, &IpcFrame::error(message)).await?;
                }
            }
        }
        IpcCommand::Undo { target } => {
            let record = match resolve_available_local_session_ref(&state, &target) {
                Ok(record) => record,
                Err(message) => {
                    ipc::send(&mut stream, &IpcFrame::error(message)).await?;
                    return Ok(());
                }
            };
            let session_id: Arc<str> = record.session_id.into();
            reserve_admin_for_session(&state.manager, &session_id)
                .map_err(|error| anyhow::anyhow!(error.message))?;
            let (reply, receiver) = oneshot::channel();
            if state
                .actor_tx
                .send(ActorCommand::Undo {
                    session_id: session_id.clone(),
                    reply,
                })
                .is_err()
            {
                release_admin(&state.manager);
                anyhow::bail!("Miyu core worker is unavailable");
            }
            match receiver.await {
                Ok(Ok(data)) => {
                    ipc::send(
                        &mut stream,
                        &IpcFrame::AdminResult {
                            state: session_state_for(&state, &session_id)?,
                            data,
                        },
                    )
                    .await?
                }
                Ok(Err(AdminFailure::Invalid(message) | AdminFailure::Internal(message))) => {
                    ipc::send(&mut stream, &IpcFrame::error(message)).await?
                }
                Err(_) => {
                    release_admin(&state.manager);
                    anyhow::bail!("Miyu core stopped while undoing the conversation");
                }
            }
        }
        IpcCommand::Pop { target, turn_ids } => {
            let record = match resolve_available_local_session_ref(&state, &target) {
                Ok(record) => record,
                Err(message) => {
                    ipc::send(&mut stream, &IpcFrame::error(message)).await?;
                    return Ok(());
                }
            };
            let session_id: Arc<str> = record.session_id.into();
            reserve_admin_for_session(&state.manager, &session_id)
                .map_err(|error| anyhow::anyhow!(error.message))?;
            let (reply, receiver) = oneshot::channel();
            if state
                .actor_tx
                .send(ActorCommand::Pop {
                    session_id: session_id.clone(),
                    turn_ids,
                    reply,
                })
                .is_err()
            {
                release_admin(&state.manager);
                anyhow::bail!("Miyu core worker is unavailable");
            }
            match receiver.await {
                Ok(Ok(data)) => {
                    ipc::send(
                        &mut stream,
                        &IpcFrame::AdminResult {
                            state: session_state_for(&state, &session_id)?,
                            data,
                        },
                    )
                    .await?
                }
                Ok(Err(AdminFailure::Invalid(message) | AdminFailure::Internal(message))) => {
                    ipc::send(&mut stream, &IpcFrame::error(message)).await?
                }
                Err(_) => {
                    release_admin(&state.manager);
                    anyhow::bail!("Miyu core stopped while popping the conversation");
                }
            }
        }
        IpcCommand::Compact { target } => {
            let record = match resolve_available_local_session_ref(&state, &target) {
                Ok(record) => record,
                Err(message) => {
                    ipc::send(&mut stream, &IpcFrame::error(message)).await?;
                    return Ok(());
                }
            };
            let session_id: Arc<str> = record.session_id.into();
            reserve_admin_for_session(&state.manager, &session_id)
                .map_err(|error| anyhow::anyhow!(error.message))?;
            let (reply, receiver) = oneshot::channel();
            if state
                .actor_tx
                .send(ActorCommand::Compact {
                    session_id: session_id.clone(),
                    reply,
                })
                .is_err()
            {
                release_admin(&state.manager);
                anyhow::bail!("Miyu core worker is unavailable");
            }
            match receiver.await {
                Ok(Ok(data)) => {
                    ipc::send(
                        &mut stream,
                        &IpcFrame::AdminResult {
                            state: session_state_for(&state, &session_id)?,
                            data,
                        },
                    )
                    .await?
                }
                Ok(Err(AdminFailure::Invalid(message) | AdminFailure::Internal(message))) => {
                    ipc::send(&mut stream, &IpcFrame::error(message)).await?
                }
                Err(_) => {
                    release_admin(&state.manager);
                    anyhow::bail!("Miyu core stopped while compacting the conversation");
                }
            }
        }
        IpcCommand::StartTurn {
            content,
            mode,
            images,
            cwd,
            session_id,
        } => {
            handle_ipc_turn(&state, &mut stream, content, mode, images, cwd, session_id).await?;
        }
        IpcCommand::QueueTurnUpdate {
            run_id,
            turn_id,
            content,
            display_content,
            images,
            supersede,
        } => {
            let attachments = images
                .into_iter()
                .flatten()
                .map(|image| match image {
                    ImageAttachment::Binary { mime, data } => {
                        crate::state::QueuedPromptAttachment::Binary {
                            mime,
                            data_base64: base64::engine::general_purpose::STANDARD.encode(data),
                        }
                    }
                    ImageAttachment::Path { path } => {
                        crate::state::QueuedPromptAttachment::Path { path }
                    }
                })
                .collect();
            match enqueue_turn_update(
                &state,
                TurnUpdateRequest {
                    run_id,
                    turn_id,
                    session_id: None,
                    audience: PromptAudience::Owner,
                    content,
                    display_content,
                    attachments,
                    uploaded_attachment_ids: Vec::new(),
                    mode: if supersede {
                        TurnUpdateMode::Supersede
                    } else {
                        TurnUpdateMode::Followup
                    },
                },
            ) {
                Ok(receipt) => {
                    ipc::send(
                        &mut stream,
                        &IpcFrame::TurnUpdateAccepted {
                            run_id: receipt.run_id,
                            turn_id: receipt.turn_id,
                            prompt_id: receipt.prompt.prompt_id,
                            seq: receipt.prompt.seq,
                            submitted_at: receipt.prompt.submitted_at,
                        },
                    )
                    .await?;
                }
                Err(error) => {
                    ipc::send(&mut stream, &IpcFrame::error(error.to_string())).await?;
                }
            }
        }
        IpcCommand::Cancel { run_id } => {
            let cancelled = {
                let manager = state.manager.lock().unwrap();
                manager
                    .active_runs
                    .get(&run_id)
                    .map(RunInfo::request_cancel)
            };
            if cancelled.is_some() {
                ipc::send(&mut stream, &IpcFrame::Ack).await?;
            } else {
                ipc::send(&mut stream, &IpcFrame::error("active run not found")).await?;
            }
        }
        IpcCommand::CloseQuestion { question_id } => {
            let _ = state.questions.close(&question_id, |run_id| {
                state.events.publish(
                    "question.closed",
                    json!({
                        "run_id": run_id,
                        "question_id": question_id,
                    }),
                );
            });
            ipc::send(&mut stream, &IpcFrame::Ack).await?
        }
        IpcCommand::AnswerQuestion {
            question_id,
            answers,
        } => match state
            .questions
            .answer(&question_id, answers, |run_id, answers| {
                state.events.publish(
                    "question.answered",
                    json!({
                        "run_id": run_id,
                        "question_id": question_id,
                        "answers": answers,
                    }),
                );
            }) {
            Ok(()) => ipc::send(&mut stream, &IpcFrame::Ack).await?,
            Err(error) => {
                let message = match error {
                    AnswerFailure::NotFound => "pending question not found".to_string(),
                    AnswerFailure::Invalid(message) => message,
                    AnswerFailure::Gone => "pending question is no longer active".to_string(),
                };
                ipc::send(&mut stream, &IpcFrame::error(message)).await?;
            }
        },
        session_command => match handle_session_command(&state, session_command).await {
            Ok(data) => {
                ipc::send(
                    &mut stream,
                    &IpcFrame::AdminResult {
                        state: session_state(&state.manager, &state.state_store)?,
                        data,
                    },
                )
                .await?
            }
            Err(message) => ipc::send(&mut stream, &IpcFrame::error(message)).await?,
        },
    }
    Ok(())
}

/// Handles the session-management IPC commands. Returns the `AdminResult`
/// payload on success or a user-facing error message.
async fn handle_session_command(
    state: &DaemonState,
    command: IpcCommand,
) -> std::result::Result<Value, String> {
    let store = &state.state_store;
    let persona = active_persona_scope(state);
    match command {
        IpcCommand::ListSessions { include_archived } => {
            let current = store.session_id();
            let sessions = store
                .list_local_sessions(&persona, include_archived)
                .map_err(|error| safe_error_message(&error))?;
            let sessions: Vec<Value> = sessions
                .iter()
                .map(|overview| session_overview_json(overview, &current))
                .collect();
            Ok(json!({ "current": &*current, "sessions": sessions }))
        }
        IpcCommand::CreateSession { name, switch, kind } => {
            // Whitelisted: `ask` is the only non-user kind a client may mint,
            // and it is deliberately unswitchable — subagent audit sessions and
            // anything else stay daemon-internal.
            let kind = match kind.as_deref() {
                None | Some(crate::state::USER_SESSION_KIND) => crate::state::USER_SESSION_KIND,
                Some(crate::state::ASK_SESSION_KIND) if !switch => crate::state::ASK_SESSION_KIND,
                Some(_) => {
                    return Err(t("unsupported session kind", "不支持的会话类型").to_string())
                }
            };
            // No explicit name: leave it empty; the session is auto-named
            // from the first prompt when its first turn completes.
            let name = name.map(|name| name.trim().to_string()).unwrap_or_default();
            let record = store
                .create_session(&persona, &name, kind, None)
                .map_err(|error| safe_error_message(&error))?;
            if kind == crate::state::USER_SESSION_KIND {
                state.events.publish(
                    "session.created",
                    json!({ "session_id": record.session_id, "name": record.name }),
                );
            }
            if switch {
                switch_session_via_actor(state, record.session_id.clone()).await?;
            }
            Ok(json!({ "session": session_record_json(&record) }))
        }
        IpcCommand::SetReplSession { target } => {
            let record = resolve_available_local_session_ref(state, &target)?;
            store
                .set_repl_session(&persona, &record.session_id)
                .map_err(|error| safe_error_message(&error))?;
            Ok(json!({ "session": session_record_json(&record) }))
        }
        IpcCommand::SwitchSession { target } => {
            let record = resolve_local_session_ref(state, &target)?;
            if record.archived {
                store
                    .set_session_archived(&record.session_id, false)
                    .map_err(|error| safe_error_message(&error))?;
            }
            switch_session_via_actor(state, record.session_id.clone()).await?;
            Ok(json!({ "session": session_record_json(&record) }))
        }
        IpcCommand::RenameSession { target, name } => {
            let record = resolve_local_session_ref(state, &target)?;
            let name = name.trim();
            if name.is_empty() {
                return Err(t("session name cannot be empty", "会话名称不能为空").to_string());
            }
            store
                .rename_session(&record.session_id, name)
                .map_err(|error| safe_error_message(&error))?;
            state.events.publish(
                "session.renamed",
                json!({ "session_id": record.session_id, "name": name }),
            );
            Ok(json!({}))
        }
        IpcCommand::ArchiveSession { target, archived } => {
            let record = resolve_local_session_ref(state, &target)?;
            if archived
                && state
                    .manager
                    .lock()
                    .unwrap()
                    .session_has_runs(&record.session_id)
            {
                return Err(t(
                    "the session has a reply in progress",
                    "该会话有回复正在进行",
                )
                .to_string());
            }
            if archived && &*store.session_id() == record.session_id.as_str() {
                let fallback = fallback_session_id(state, &record.session_id)?;
                switch_session_via_actor(state, fallback).await?;
            }
            store
                .set_session_archived(&record.session_id, archived)
                .map_err(|error| safe_error_message(&error))?;
            state.events.publish(
                "session.archived",
                json!({ "session_id": record.session_id, "archived": archived }),
            );
            Ok(json!({}))
        }
        IpcCommand::DeleteSession { target } => {
            // Accepts `ask` too: a one-shot turn deletes its own session here.
            let record = resolve_local_session_ref_with_kinds(state, &target, TURN_TARGET_KINDS)?;
            reserve_admin_for_session(&state.manager, &record.session_id)
                .map_err(|error| error.message)?;
            if &*store.session_id() == record.session_id.as_str() {
                let fallback = match fallback_session_id(state, &record.session_id) {
                    Ok(fallback) => fallback,
                    Err(error) => {
                        release_admin(&state.manager);
                        return Err(error);
                    }
                };
                if let Err(error) = switch_session_via_actor_reserved(state, fallback).await {
                    release_admin(&state.manager);
                    return Err(error);
                }
            }
            let result = store
                .delete_session(&record.session_id)
                .map_err(|error| safe_error_message(&error));
            release_admin(&state.manager);
            result?;
            state.events.publish(
                "session.deleted",
                json!({ "session_id": record.session_id }),
            );
            Ok(json!({}))
        }
        IpcCommand::SetWorkspace { target, path } => {
            let record = resolve_local_session_ref(state, &target)?;
            let workspace = match path {
                Some(path) => {
                    if !path.is_dir() {
                        return Err(format!(
                            "{}: {}",
                            t("workspace is not a directory", "workspace 不是目录"),
                            path.display()
                        ));
                    }
                    Some(path.to_string_lossy().into_owned())
                }
                None => None,
            };
            store
                .set_session_workspace(&record.session_id, workspace.as_deref())
                .map_err(|error| safe_error_message(&error))?;
            state.events.publish(
                "session.updated",
                json!({ "session_id": record.session_id, "workspace": workspace }),
            );
            Ok(json!({}))
        }
        IpcCommand::SetSessionModels { target, models } => {
            let record = resolve_local_session_ref(state, &target)?;
            let models = (!models.is_empty()).then_some(models);
            if let Some(models) = &models {
                let choices = {
                    let manager = state.manager.lock().unwrap();
                    manager.config.text_provider_model_choices()
                };
                for model in models {
                    if !choices.iter().any(|choice| {
                        choice.provider_id == model.provider_id && choice.model == model.model
                    }) {
                        return Err(format!(
                            "{}{}/{}",
                            t("unknown model: ", "未知模型："),
                            model.provider_id,
                            model.model
                        ));
                    }
                }
            }
            store
                .set_session_model_override(&record.session_id, models.as_deref())
                .map_err(|error| safe_error_message(&error))?;
            state.events.publish(
                "session.updated",
                json!({
                    "session_id": record.session_id,
                    "model_override": models,
                }),
            );
            Ok(json!({ "session_id": record.session_id }))
        }
        _ => Err("unsupported session command".to_string()),
    }
}

fn active_persona_scope(state: &DaemonState) -> String {
    state.manager.lock().unwrap().config.active_persona_scope()
}

fn session_api_error(message: String) -> ApiError {
    ApiError::new(StatusCode::BAD_REQUEST, message)
}

fn require_local_web_session(
    state: &DaemonState,
    session_id: &str,
) -> std::result::Result<crate::state::SessionRecord, ApiError> {
    let record = state
        .state_store
        .session_record(session_id)
        .map_err(ApiError::internal)?;
    let is_platform = state
        .state_store
        .is_platform_session(session_id)
        .map_err(ApiError::internal)?;
    match record {
        Some(record)
            if !is_platform
                && record.kind == "user"
                && record.persona == active_persona_scope(state) =>
        {
            Ok(record)
        }
        _ => Err(ApiError::new(StatusCode::NOT_FOUND, "session not found")),
    }
}

#[derive(Deserialize)]
struct SessionsQuery {
    #[serde(default)]
    include_archived: bool,
}

async fn list_sessions_http(
    State(state): State<DaemonState>,
    headers: HeaderMap,
    Query(query): Query<SessionsQuery>,
) -> std::result::Result<Response, ApiError> {
    require_auth(&headers, &state)?;
    let current = state.state_store.session_id();
    let persona = active_persona_scope(&state);
    let sessions = state
        .state_store
        .list_local_sessions(&persona, query.include_archived)
        .map_err(ApiError::internal)?;
    let sessions = sessions
        .iter()
        .map(|overview| session_overview_json(overview, &current))
        .collect::<Vec<_>>();
    let data = json!({ "current": &*current, "sessions": sessions });
    Ok(Json(data).into_response())
}

#[derive(Deserialize)]
struct CreateSessionRequest {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    switch: bool,
}

#[derive(Deserialize)]
struct ResetConversationRequest {
    session_id: Option<String>,
}

async fn create_session_http(
    State(state): State<DaemonState>,
    headers: HeaderMap,
    Json(request): Json<CreateSessionRequest>,
) -> std::result::Result<Response, ApiError> {
    require_mutation(&headers, &state)?;
    let data = handle_session_command(
        &state,
        IpcCommand::CreateSession {
            name: request.name,
            switch: request.switch,
            kind: None,
        },
    )
    .await
    .map_err(session_api_error)?;
    Ok((StatusCode::CREATED, Json(data)).into_response())
}

async fn activate_session_http(
    State(state): State<DaemonState>,
    headers: HeaderMap,
    Path(session_id): Path<String>,
) -> std::result::Result<Response, ApiError> {
    require_mutation(&headers, &state)?;
    require_local_web_session(&state, &session_id)?;
    let data = handle_session_command(
        &state,
        IpcCommand::SwitchSession {
            target: ipc::SessionRef::Id { id: session_id },
        },
    )
    .await
    .map_err(session_api_error)?;
    Ok(Json(data).into_response())
}

#[derive(Deserialize)]
struct UpdateSessionRequest {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    archived: Option<bool>,
    /// `Some("")` unbinds the workspace; a non-empty value binds it.
    #[serde(default)]
    workspace: Option<String>,
}

async fn update_session_http(
    State(state): State<DaemonState>,
    headers: HeaderMap,
    Path(session_id): Path<String>,
    Json(request): Json<UpdateSessionRequest>,
) -> std::result::Result<Response, ApiError> {
    require_mutation(&headers, &state)?;
    require_local_web_session(&state, &session_id)?;
    let target = || ipc::SessionRef::Id {
        id: session_id.clone(),
    };
    if let Some(name) = request.name {
        handle_session_command(
            &state,
            IpcCommand::RenameSession {
                target: target(),
                name,
            },
        )
        .await
        .map_err(session_api_error)?;
    }
    if let Some(archived) = request.archived {
        handle_session_command(
            &state,
            IpcCommand::ArchiveSession {
                target: target(),
                archived,
            },
        )
        .await
        .map_err(session_api_error)?;
    }
    if let Some(workspace) = request.workspace {
        let path = (!workspace.trim().is_empty()).then(|| std::path::PathBuf::from(workspace));
        handle_session_command(
            &state,
            IpcCommand::SetWorkspace {
                target: target(),
                path,
            },
        )
        .await
        .map_err(session_api_error)?;
    }
    Ok(Json(json!({})).into_response())
}

/// Read-only snapshot of one session's conversation for per-view browsing:
/// turns, queued follow-ups, and its currently running turns. Does not touch
/// the global current-session pointer.
async fn session_turns_http(
    State(state): State<DaemonState>,
    headers: HeaderMap,
    Path(session_id): Path<String>,
) -> std::result::Result<Response, ApiError> {
    require_auth(&headers, &state)?;
    require_local_web_session(&state, &session_id)?;
    let store = state.state_store.pinned(&session_id);
    let mut assets_by_turn = HashMap::<String, Vec<ImageAsset>>::new();
    for asset in store.load_image_assets().map_err(ApiError::internal)? {
        assets_by_turn
            .entry(asset.turn_id.clone())
            .or_default()
            .push(asset);
    }
    let mut artifacts_by_turn = HashMap::<String, Vec<ArtifactAsset>>::new();
    for artifact in store.load_artifact_assets().map_err(ApiError::internal)? {
        artifacts_by_turn
            .entry(artifact.turn_id.clone())
            .or_default()
            .push(artifact);
    }
    let turns: Vec<SafeTurn> = store
        .load_turns()
        .map_err(ApiError::internal)?
        .into_iter()
        .filter(|turn| !turn.is_summary)
        .map(|turn| {
            let assets = assets_by_turn.remove(&turn.turn_id).unwrap_or_default();
            let artifacts = artifacts_by_turn.remove(&turn.turn_id).unwrap_or_default();
            SafeTurn::from_turn(turn, assets, artifacts)
        })
        .collect();
    let running_target = store
        .running_turn_queue_target()
        .map_err(ApiError::internal)?;
    let queued_prompts: Vec<SafeQueuedPrompt> = match running_target.as_ref() {
        Some(target) => store
            .load_queued_prompts_for_target(target)
            .map_err(ApiError::internal)?,
        None => Vec::new(),
    }
    .into_iter()
    .map(SafeQueuedPrompt::from)
    .collect();
    let runs: Vec<Value> = state
        .manager
        .lock()
        .unwrap()
        .active_runs
        .iter()
        .filter(|(_, info)| &*info.session_id == session_id.as_str())
        .map(|(run_id, info)| {
            json!({
                "run_id": run_id,
                "session_id": &*info.session_id,
                "mode": mode_name(info.mode),
                "operation": info.operation.name(),
                "turn_id": info.operation.turn_id(),
                "input_id": info.operation.input_id(),
            })
        })
        .collect();
    let redo_candidate = if runs.is_empty() {
        store
            .redo_candidate()
            .map_err(ApiError::internal)?
            .map(SafeRedoCandidate::from)
    } else {
        None
    };
    let mut response = Json(json!({
        "session_id": session_id,
        "turns": turns,
        "queued_prompts": queued_prompts,
        "running_turn_id": running_target.as_ref().map(|target| target.turn_id.as_str()),
        "runs": runs,
        "redo_candidate": redo_candidate,
    }))
    .into_response();
    response
        .headers_mut()
        .insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    Ok(response)
}

async fn delete_session_http(
    State(state): State<DaemonState>,
    headers: HeaderMap,
    Path(session_id): Path<String>,
) -> std::result::Result<Response, ApiError> {
    require_mutation(&headers, &state)?;
    require_local_web_session(&state, &session_id)?;
    let data = handle_session_command(
        &state,
        IpcCommand::DeleteSession {
            target: ipc::SessionRef::Id { id: session_id },
        },
    )
    .await
    .map_err(session_api_error)?;
    Ok(Json(data).into_response())
}

fn resolve_local_session_ref(
    state: &DaemonState,
    target: &ipc::SessionRef,
) -> std::result::Result<crate::state::SessionRecord, String> {
    resolve_local_session_ref_with_kinds(state, target, &[crate::state::USER_SESSION_KIND])
}

/// Same, but for the two callers that must also reach one-shot `ask` sessions
/// (running their turn, then deleting them). `SessionRef::Name` still cannot
/// find those — the DB lookup filters to user sessions — so only the client
/// holding the freshly minted id can address one.
fn resolve_local_session_ref_with_kinds(
    state: &DaemonState,
    target: &ipc::SessionRef,
    kinds: &[&str],
) -> std::result::Result<crate::state::SessionRecord, String> {
    let store = &state.state_store;
    let persona = active_persona_scope(state);
    let record = match target {
        ipc::SessionRef::Current => store
            .session_record(&store.session_id())
            .map_err(|error| safe_error_message(&error))?,
        ipc::SessionRef::Id { id } => store
            .session_record(id)
            .map_err(|error| safe_error_message(&error))?,
        ipc::SessionRef::Name { name } => store
            .find_local_session_by_name(&persona, name)
            .map_err(|error| safe_error_message(&error))?,
    };
    let Some(record) = record else {
        return Err(t("session not found", "找不到该会话").to_string());
    };
    let is_platform = store
        .is_platform_session(&record.session_id)
        .map_err(|error| safe_error_message(&error))?;
    if record.persona != persona || !kinds.contains(&record.kind.as_str()) || is_platform {
        return Err(t("session not found", "找不到该会话").to_string());
    }
    Ok(record)
}

fn resolve_available_local_session_ref(
    state: &DaemonState,
    target: &ipc::SessionRef,
) -> std::result::Result<crate::state::SessionRecord, String> {
    let record = resolve_local_session_ref(state, target)?;
    if record.archived {
        return Err(t("session is archived", "会话已归档").to_string());
    }
    Ok(record)
}

/// Turn targets and deletions additionally accept one-shot `ask` sessions.
const TURN_TARGET_KINDS: &[&str] = &[
    crate::state::USER_SESSION_KIND,
    crate::state::ASK_SESSION_KIND,
];

/// Most recently updated other unarchived user session, or a fresh default
/// session when none is left.
fn fallback_session_id(state: &DaemonState, exclude: &str) -> std::result::Result<String, String> {
    let persona = active_persona_scope(state);
    let sessions = state
        .state_store
        .list_local_sessions(&persona, false)
        .map_err(|error| safe_error_message(&error))?;
    if let Some(overview) = sessions
        .iter()
        .find(|overview| overview.record.session_id != exclude)
    {
        return Ok(overview.record.session_id.clone());
    }
    let record = state
        .state_store
        .create_session(&persona, t("Default session", "默认会话"), "user", None)
        .map_err(|error| safe_error_message(&error))?;
    state.events.publish(
        "session.created",
        json!({ "session_id": record.session_id, "name": record.name }),
    );
    Ok(record.session_id)
}

async fn switch_session_via_actor(
    state: &DaemonState,
    session_id: String,
) -> std::result::Result<(), String> {
    reserve_admin_light(&state.manager).map_err(|error| error.message)?;
    let (reply, receiver) = oneshot::channel();
    if state
        .actor_tx
        .send(ActorCommand::SwitchSession {
            session_id,
            release_reservation: true,
            reply,
        })
        .is_err()
    {
        release_admin(&state.manager);
        return Err("Miyu core worker is unavailable".to_string());
    }
    match receiver.await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(AdminFailure::Invalid(message) | AdminFailure::Internal(message))) => Err(message),
        Err(_) => {
            release_admin(&state.manager);
            Err("Miyu core stopped while switching sessions".to_string())
        }
    }
}

async fn switch_session_via_actor_reserved(
    state: &DaemonState,
    session_id: String,
) -> std::result::Result<(), String> {
    let (reply, receiver) = oneshot::channel();
    if state
        .actor_tx
        .send(ActorCommand::SwitchSession {
            session_id,
            release_reservation: false,
            reply,
        })
        .is_err()
    {
        return Err("Miyu core worker is unavailable".to_string());
    }
    match receiver.await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(AdminFailure::Invalid(message) | AdminFailure::Internal(message))) => Err(message),
        Err(_) => Err("Miyu core stopped while switching sessions".to_string()),
    }
}

fn session_record_json(record: &crate::state::SessionRecord) -> Value {
    json!({
        "session_id": record.session_id,
        "name": record.name,
        "kind": record.kind,
        "workspace": record.workspace,
        "archived": record.archived,
        "created_at": record.created_at,
        "updated_at": record.updated_at,
    })
}

fn session_overview_json(overview: &crate::state::SessionOverview, current: &str) -> Value {
    let mut value = session_record_json(&overview.record);
    value["turn_count"] = json!(overview.turn_count);
    value["last_user_content"] = json!(overview.last_user_content);
    value["is_current"] = json!(overview.record.session_id == current);
    value
}

/// Resolves an optional turn-target session id: validates existence and that
/// it is a user or one-shot session; `None` falls back to the global current
/// session.
fn resolve_turn_session(
    state: &DaemonState,
    session_id: Option<String>,
) -> std::result::Result<Arc<str>, String> {
    match session_id {
        None => Ok(state.state_store.session_id()),
        Some(session_id) => {
            let record = resolve_local_session_ref_with_kinds(
                state,
                &ipc::SessionRef::Id { id: session_id },
                TURN_TARGET_KINDS,
            )?;
            if record.archived {
                return Err(t("session is archived", "会话已归档").to_string());
            }
            Ok(record.session_id.into())
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn handle_ipc_turn(
    state: &DaemonState,
    stream: &mut tokio::net::UnixStream,
    content: String,
    mode: String,
    images: Vec<Option<ImageAttachment>>,
    cwd: Option<std::path::PathBuf>,
    session_id: Option<String>,
) -> Result<()> {
    let content = match validate_content(content) {
        Ok(content) => content,
        Err(error) => {
            ipc::send(stream, &IpcFrame::error(error.message)).await?;
            return Ok(());
        }
    };
    let mode = match parse_mode(&mode) {
        Ok(mode) => mode,
        Err(error) => {
            ipc::send(stream, &IpcFrame::error(error.message)).await?;
            return Ok(());
        }
    };
    // Turns run in parallel — several may be active at once, including in
    // the same session (placeholder semantics). The only rejection is a
    // transient admin mutation window.
    let run_id = random_id("run", 18);
    let session_id = match resolve_turn_session(state, session_id) {
        Ok(session_id) => session_id,
        Err(message) => {
            ipc::send(stream, &IpcFrame::error(message)).await?;
            return Ok(());
        }
    };
    let (cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
    let busy = {
        let mut manager = state.manager.lock().unwrap();
        if manager.admin_busy {
            true
        } else {
            manager.active_runs.insert(
                run_id.clone(),
                RunInfo {
                    session_id: session_id.clone(),
                    mode,
                    audience: PromptAudience::Owner,
                    cancel: cancel_tx,
                    turn_id: None,
                    queue_target: None,
                    supersede: Arc::new(crate::agent::TurnSupersedeSignal::default()),
                    platform_followup: None,
                    operation: RunOperation::Create,
                    job_wake: false,
                job_wake_label: None,
                },
            );
            false
        }
    };
    if busy {
        ipc::send(
            stream,
            &IpcFrame::coded_error(ipc::ErrorCode::Busy, ipc::ADMIN_BUSY_MESSAGE),
        )
        .await?;
        return Ok(());
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
            mode,
            images,
            cwd,
            audience: PromptAudience::Owner,
            profile: None,
            cancel: cancel_rx,
        })
        .is_err()
    {
        finish_run(&state.manager, &run_id, None);
        ipc::send(stream, &IpcFrame::error("Miyu core worker is unavailable")).await?;
        return Ok(());
    }
    let mut run_guard = IpcRunGuard {
        manager: state.manager.clone(),
        run_id: run_id.clone(),
        finished: false,
    };
    ipc::send(
        stream,
        &IpcFrame::Accepted {
            run_id: run_id.clone(),
            turn_id: None,
        },
    )
    .await?;

    let mut last_id = after;
    loop {
        let record = if let Some(record) = subscription.pending.pop_front() {
            record
        } else {
            match subscription.receiver.recv().await {
                Ok(record) => record,
                Err(broadcast::error::RecvError::Lagged(_)) => {
                    subscription.pending = state.events.replay_after(last_id);
                    continue;
                }
                Err(broadcast::error::RecvError::Closed) => break,
            }
        };
        if record.kind == "resync_required" {
            ipc::send(
                stream,
                &IpcFrame::error("Miyu core event history was exhausted; the turn was cancelled"),
            )
            .await?;
            break;
        }
        last_id = record.id;
        let Ok(data) = serde_json::from_str::<Value>(&record.data) else {
            continue;
        };
        if data.get("run_id").and_then(Value::as_str) != Some(run_id.as_str()) {
            continue;
        }
        let terminal = matches!(
            record.kind.as_str(),
            "run.completed" | "run.failed" | "run.cancelled"
        );
        ipc::send(
            stream,
            &IpcFrame::Event {
                id: record.id,
                kind: record.kind,
                data,
            },
        )
        .await?;
        if terminal {
            run_guard.finish();
            break;
        }
    }
    Ok(())
}


/// Attach a client to an already-running turn (background-command wake):
/// forwards its event frames until terminal, without owning the run.
async fn follow_run(
    state: &DaemonState,
    stream: &mut tokio::net::UnixStream,
    run_id: String,
) -> Result<()> {
    let mut subscription = state.events.subscribe_after(state.events.latest_id());
    let run_state = {
        let manager = state.manager.lock().unwrap();
        manager
            .active_runs
            .get(&run_id)
            .map(|info| info.turn_id.clone())
    };
    let Some(turn_id) = run_state else {
        ipc::send(stream, &IpcFrame::error("run is not active")).await?;
        return Ok(());
    };
    ipc::send(
        stream,
        &IpcFrame::Accepted {
            run_id: run_id.clone(),
            turn_id,
        },
    )
    .await?;
    let mut last_id = 0u64;
    loop {
        let record = if let Some(record) = subscription.pending.pop_front() {
            record
        } else {
            match subscription.receiver.recv().await {
                Ok(record) => record,
                Err(broadcast::error::RecvError::Lagged(_)) => {
                    subscription.pending = state.events.replay_after(last_id);
                    continue;
                }
                Err(broadcast::error::RecvError::Closed) => break,
            }
        };
        if record.kind == "resync_required" {
            ipc::send(
                stream,
                &IpcFrame::error("Miyu core event history was exhausted"),
            )
            .await?;
            break;
        }
        last_id = record.id;
        let Ok(data) = serde_json::from_str::<Value>(&record.data) else {
            continue;
        };
        if data.get("run_id").and_then(Value::as_str) != Some(run_id.as_str()) {
            // The run may have finished before we saw a frame; stop when it
            // is no longer active and nothing more will arrive for it.
            if !state.manager.lock().unwrap().active_runs.contains_key(&run_id) {
                break;
            }
            continue;
        }
        let terminal = matches!(
            record.kind.as_str(),
            "run.completed" | "run.failed" | "run.cancelled"
        );
        ipc::send(
            stream,
            &IpcFrame::Event {
                id: record.id,
                kind: record.kind,
                data,
            },
        )
        .await?;
        if terminal {
            break;
        }
    }
    Ok(())
}

fn router(state: DaemonState) -> Router {
    Router::new()
        .route("/", get(index_asset))
        .route("/styles.css", get(styles_asset))
        .route("/theme.css", get(theme_css))
        .route("/app.js", get(app_asset))
        .route("/assets/miyu-logo.png", get(logo_asset))
        .route("/assets/miyuwallpaper.png", get(wallpaper_asset))
        .route("/api/health", get(health))
        .route("/api/auth/login", post(auth_login))
        .route("/api/bootstrap", get(bootstrap))
        .route("/api/persona/avatar", get(persona_avatar))
        .route(
            "/api/persona/assets",
            post(upload_persona_asset).layer(DefaultBodyLimit::max(PERSONA_ASSET_LIMIT)),
        )
        .route("/api/config", get(get_config).put(update_config))
        .route(
            "/api/qq-group-management/history",
            get(qq_group_history_http),
        )
        .route(
            "/api/qq-group-management/history/clear",
            post(qq_group_history_clear_http),
        )
        .route(
            "/api/qq-group-management/offenders/{user_id}",
            delete(qq_group_offender_delete_http),
        )
        .route("/api/events", get(events))
        .route("/api/assets/{asset_id}", get(image_asset))
        .route("/api/artifacts/{asset_id}", get(artifact_asset))
        .route(
            "/api/attachments",
            post(upload_user_attachment).layer(DefaultBodyLimit::max(ATTACHMENT_BODY_LIMIT)),
        )
        .route(
            "/api/attachments/{attachment_id}",
            get(user_attachment).delete(delete_user_attachment),
        )
        .route(
            "/api/platform-assets/{token}",
            get(platforms::platform_asset),
        )
        .route(
            "/api/sessions",
            get(list_sessions_http).post(create_session_http),
        )
        .route(
            "/api/sessions/{session_id}",
            patch(update_session_http).delete(delete_session_http),
        )
        .route(
            "/api/sessions/{session_id}/activate",
            post(activate_session_http),
        )
        .route("/api/sessions/{session_id}/turns", get(session_turns_http))
        .route(
            "/api/sessions/{session_id}/models",
            get(get_session_models_http).put(set_session_models_http),
        )
        .route(
            "/api/sessions/{session_id}/turns/{turn_id}/redo",
            post(redo_turn),
        )
        .route("/api/turns", post(create_turn))
        .route("/api/queue", post(queue_prompt))
        .route(
            "/api/runs/{run_id}/turns/{turn_id}/queue/{prompt_id}",
            delete(remove_queue_prompt),
        )
        .route("/api/runs/{run_id}/cancel", post(cancel_run))
        .route("/api/questions/{question_id}", delete(close_question))
        .route("/api/questions/{question_id}/answer", post(answer_question))
        .route("/api/models/active", put(set_models))
        .route(
            "/api/models/thinking-variants",
            get(get_thinking_variants).put(set_thinking_variants),
        )
        .route("/api/conversation/reset", post(reset_conversation))
        .route("/api/jobs", get(list_jobs_http))
        .route("/api/jobs/{job_id}", delete(stop_job_http))
        // OneBot v11 reverse-WS endpoint: NapCat connects here as a WS
        // client. Gated by platforms.qq config, not web auth.
        .route("/ws", get(platforms::onebot::onebot_ws_on_web_port))
        // Backward-compatible endpoint used by earlier Miyu releases.
        .route(
            "/onebot/v11/ws",
            get(platforms::onebot::onebot_ws_on_web_port),
        )
        .layer(DefaultBodyLimit::max(JSON_BODY_LIMIT))
        .with_state(state)
}

/// Strong validator shared by all build-embedded assets: the BUILD_ID
/// changes on any frontend edit (build.rs rerun triggers), so a 304 can
/// never pin a stale file.
fn build_etag() -> &'static HeaderValue {
    static ETAG_VALUE: std::sync::LazyLock<HeaderValue> = std::sync::LazyLock::new(|| {
        HeaderValue::from_str(concat!("\"", env!("MIYU_BUILD_ID"), "\""))
            .expect("build id forms a valid header value")
    });
    &ETAG_VALUE
}

fn embedded_asset(
    headers: &HeaderMap,
    content: &'static [u8],
    content_type: &'static str,
) -> Response {
    if headers
        .get(axum::http::header::IF_NONE_MATCH)
        .is_some_and(|value| value == build_etag())
    {
        let mut response = StatusCode::NOT_MODIFIED.into_response();
        response
            .headers_mut()
            .insert(axum::http::header::ETAG, build_etag().clone());
        return response;
    }
    let mut response = finish_asset_response(content.into_response(), content_type);
    response
        .headers_mut()
        .insert(axum::http::header::ETAG, build_etag().clone());
    response
}

async fn index_asset(headers: HeaderMap) -> Response {
    // Version the asset references so browsers and intermediaries can never
    // serve a stale app.js/styles.css after an upgrade.
    static VERSIONED_INDEX: std::sync::LazyLock<String> = std::sync::LazyLock::new(|| {
        INDEX_HTML
            .replace("href=\"/styles.css\"", concat!("href=\"/styles.css?v=", env!("MIYU_BUILD_ID"), "\""))
            .replace("src=\"/app.js\"", concat!("src=\"/app.js?v=", env!("MIYU_BUILD_ID"), "\""))
    });
    embedded_asset(&headers, VERSIONED_INDEX.as_bytes(), "text/html; charset=utf-8")
}

async fn styles_asset(headers: HeaderMap) -> Response {
    embedded_asset(&headers, STYLES_CSS.as_bytes(), "text/css; charset=utf-8")
}

/// Optional MD3 token override generated by matugen from the wallpaper.
/// Read from disk on every request (the file is tiny and regenerated at any
/// time); 404 when absent so the WebUI falls back to the built-in palette.
async fn theme_css(State(state): State<DaemonState>) -> Response {
    let path = state.paths.config_dir.join("webui-theme.css");
    match tokio::fs::read(&path).await {
        Ok(bytes) => finish_asset_response(bytes.into_response(), "text/css; charset=utf-8"),
        Err(_) => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn app_asset(headers: HeaderMap) -> Response {
    embedded_asset(&headers, APP_JS.as_bytes(), "application/javascript; charset=utf-8")
}

async fn logo_asset(headers: HeaderMap) -> Response {
    embedded_asset(&headers, MIYU_LOGO, "image/png")
}

async fn wallpaper_asset(headers: HeaderMap) -> Response {
    embedded_asset(&headers, MIYU_WALLPAPER, "image/png")
}

async fn persona_avatar(
    State(state): State<DaemonState>,
    headers: HeaderMap,
    Query(query): Query<std::collections::HashMap<String, String>>,
) -> std::result::Result<Response, ApiError> {
    require_auth(&headers, &state)?;
    let (config, prompts) = {
        let manager = state.manager.lock().unwrap();
        let prompts =
            read_prompt_documents(&manager.config, &state.paths).map_err(ApiError::internal)?;
        (manager.config.clone(), prompts)
    };
    let path = if let Some(path) = query.get("path").filter(|p| !p.is_empty()) {
        managed_persona_asset_path(&state.paths, path).ok_or_else(|| {
            ApiError::new(
                StatusCode::BAD_REQUEST,
                "invalid managed persona asset path",
            )
        })?
    } else if query.contains_key("board") {
        active_persona_board_path(&config, &prompts, &state.paths)
            .ok_or_else(|| ApiError::new(StatusCode::NOT_FOUND, "persona board image not found"))?
    } else if let Some(path) = active_persona_avatar_path(&config, &prompts, &state.paths) {
        path
    } else {
        return Err(ApiError::new(
            StatusCode::NOT_FOUND,
            "persona avatar not found",
        ));
    };
    if path.starts_with(state.paths.persona_avatars_dir()) {
        validate_managed_persona_asset_file(&state.paths, &path)
            .map_err(|_| ApiError::new(StatusCode::NOT_FOUND, "persona avatar not found"))?;
    }
    let bytes = tokio::fs::read(&path)
        .await
        .map_err(|_| ApiError::new(StatusCode::NOT_FOUND, "persona avatar not found"))?;
    if bytes.len() > 8 * 1024 * 1024 {
        return Err(ApiError::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            "persona avatar is too large",
        ));
    }
    let format = image::guess_format(&bytes)
        .map_err(|_| ApiError::new(StatusCode::NOT_FOUND, "persona avatar is not an image"))?;
    let mime = match format {
        image::ImageFormat::Png => "image/png",
        image::ImageFormat::Jpeg => "image/jpeg",
        image::ImageFormat::Gif => "image/gif",
        image::ImageFormat::WebP => "image/webp",
        image::ImageFormat::Bmp => "image/bmp",
        _ => {
            return Err(ApiError::new(
                StatusCode::NOT_FOUND,
                "persona avatar format is unsupported",
            ))
        }
    };
    let mut response = bytes.into_response();
    response
        .headers_mut()
        .insert(CONTENT_TYPE, HeaderValue::from_static(mime));
    response
        .headers_mut()
        .insert(CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    response
        .headers_mut()
        .insert(X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    Ok(response)
}

async fn upload_persona_asset(
    State(state): State<DaemonState>,
    headers: HeaderMap,
    body: Bytes,
) -> std::result::Result<Json<Value>, ApiError> {
    require_mutation(&headers, &state)?;
    if body.is_empty() {
        return Err(ApiError::new(StatusCode::BAD_REQUEST, "image is empty"));
    }
    if body.len() > PERSONA_ASSET_LIMIT {
        return Err(ApiError::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            "persona image is too large",
        ));
    }
    let format = image::guess_format(&body)
        .map_err(|_| ApiError::new(StatusCode::BAD_REQUEST, "unsupported image format"))?;
    let extension = match format {
        image::ImageFormat::Png => "png",
        image::ImageFormat::Jpeg => "jpg",
        image::ImageFormat::Gif => "gif",
        image::ImageFormat::WebP => "webp",
        image::ImageFormat::Bmp => "bmp",
        _ => {
            return Err(ApiError::new(
                StatusCode::BAD_REQUEST,
                "unsupported image format",
            ))
        }
    };
    let hash = format!("{:x}", Sha256::digest(&body));
    let relative = format!("persona-avatars/{hash}.{extension}");
    let directory = state.paths.persona_avatars_dir();
    let destination = directory.join(format!("{hash}.{extension}"));
    tokio::fs::create_dir_all(&directory)
        .await
        .map_err(ApiError::internal)?;
    let directory_metadata = tokio::fs::symlink_metadata(&directory)
        .await
        .map_err(ApiError::internal)?;
    if directory_metadata.file_type().is_symlink() || !directory_metadata.is_dir() {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "persona asset directory is unsafe",
        ));
    }
    store_persona_asset(&directory, &destination, &hash, &body).await?;
    let config = state.manager.lock().unwrap().config.clone();
    if let Ok(prompts) = read_prompt_documents(&config, &state.paths) {
        cleanup_persona_assets(&state.paths, &prompts, &prompts);
    }
    Ok(Json(json!({
        "path": relative,
        "preview_url": format!("/api/persona/avatar?path={relative}"),
    })))
}

async fn store_persona_asset(
    directory: &FilePath,
    destination: &FilePath,
    expected_hash: &str,
    body: &[u8],
) -> std::result::Result<(), ApiError> {
    let replace_corrupt = match tokio::fs::symlink_metadata(destination).await {
        Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => {
            match verify_persona_asset_hash(destination, expected_hash).await {
                Ok(()) => return Ok(()),
                Err(error) if error.status == StatusCode::CONFLICT => true,
                Err(error) => return Err(error),
            }
        }
        Ok(_) => {
            return Err(ApiError::new(
                StatusCode::CONFLICT,
                "persona asset destination is unsafe",
            ));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(error) => return Err(ApiError::internal(error)),
    };

    let temporary = directory.join(format!(
        ".upload-{}-{:016x}",
        std::process::id(),
        rand::random::<u64>()
    ));
    let mut file = tokio::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&temporary)
        .await
        .map_err(ApiError::internal)?;
    let write_result = async {
        file.write_all(body).await?;
        file.sync_all().await?;
        if replace_corrupt {
            tokio::fs::rename(&temporary, destination).await
        } else {
            tokio::fs::hard_link(&temporary, destination).await
        }
    }
    .await;
    match write_result {
        Ok(()) => {
            let _ = tokio::fs::remove_file(&temporary).await;
            let directory = tokio::fs::File::open(directory)
                .await
                .map_err(ApiError::internal)?;
            directory.sync_all().await.map_err(ApiError::internal)?;
            Ok(())
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            let _ = tokio::fs::remove_file(&temporary).await;
            verify_persona_asset_hash(destination, expected_hash).await
        }
        Err(error) => {
            let _ = tokio::fs::remove_file(&temporary).await;
            Err(ApiError::internal(error))
        }
    }
}

async fn verify_persona_asset_hash(
    path: &FilePath,
    expected_hash: &str,
) -> std::result::Result<(), ApiError> {
    let bytes = tokio::fs::read(path).await.map_err(ApiError::internal)?;
    if bytes.len() > PERSONA_ASSET_LIMIT || format!("{:x}", Sha256::digest(&bytes)) != expected_hash
    {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "persona asset cache entry is corrupted",
        ));
    }
    Ok(())
}

fn text_asset(content: &'static str, content_type: &'static str) -> Response {
    asset_response(content.as_bytes(), content_type)
}

fn binary_asset(content: &'static [u8], content_type: &'static str) -> Response {
    asset_response(content, content_type)
}

fn asset_response(content: &'static [u8], content_type: &'static str) -> Response {
    finish_asset_response(content.into_response(), content_type)
}

fn finish_asset_response(mut response: Response, content_type: &'static str) -> Response {
    response
        .headers_mut()
        .insert(CONTENT_TYPE, HeaderValue::from_static(content_type));
    response
        .headers_mut()
        .insert(CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    response
        .headers_mut()
        .insert(X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    response.headers_mut().insert(
        CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(
            "default-src 'self'; img-src 'self'; style-src 'self'; script-src 'self'; connect-src 'self'; base-uri 'none'; frame-ancestors 'none'; form-action 'self'",
        ),
    );
    response
        .headers_mut()
        .insert(REFERRER_POLICY, HeaderValue::from_static("no-referrer"));
    response
}

async fn auth_login(
    State(state): State<DaemonState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(request): Json<LoginRequest>,
) -> std::result::Result<Response, ApiError> {
    if !origin_is_allowed(&headers) {
        return Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "request origin is not allowed",
        ));
    }
    if !state.auth.required() {
        return Ok(StatusCode::NO_CONTENT.into_response());
    }
    if request.password.chars().count() > 1_024 {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "password is too long",
        ));
    }
    let session = match state.auth.login(peer.ip(), &request.password) {
        Ok(session) => session,
        Err(LoginFailure::Invalid) => {
            return Err(ApiError::new(StatusCode::UNAUTHORIZED, "invalid password"));
        }
        Err(LoginFailure::RateLimited) => {
            let mut response = ApiError::new(
                StatusCode::TOO_MANY_REQUESTS,
                "too many login attempts; try again shortly",
            )
            .into_response();
            response
                .headers_mut()
                .insert(RETRY_AFTER, HeaderValue::from_static("60"));
            return Ok(response);
        }
    };
    let cookie =
        format!("{AUTH_COOKIE}={session}; HttpOnly; SameSite=Strict; Path=/; Max-Age=86400");
    let mut response = StatusCode::NO_CONTENT.into_response();
    response.headers_mut().insert(
        SET_COOKIE,
        HeaderValue::from_str(&cookie).map_err(ApiError::internal)?,
    );
    response
        .headers_mut()
        .insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    Ok(response)
}

fn resolve_web_password(args: &WebArgs) -> Result<Option<String>> {
    let password = if let Some(path) = &args.password_file {
        let contents = std::fs::read_to_string(path)
            .with_context(|| format!("reading WebUI password file: {}", path.display()))?;
        Some(contents.trim_end_matches(['\r', '\n']).to_string())
    } else {
        match &args.password {
            Some(password) if !password.is_empty() => Some(password.clone()),
            Some(_) if io::stdin().is_terminal() => {
                Some(rpassword::prompt_password("WebUI password: ")?)
            }
            Some(_) => {
                anyhow::bail!("-p requires an interactive terminal or an explicit password value")
            }
            None => None,
        }
    };
    if let Some(password) = &password {
        if password.is_empty() {
            anyhow::bail!("WebUI password cannot be empty");
        }
        if password.chars().count() > 1_024 {
            anyhow::bail!("WebUI password cannot exceed 1,024 characters");
        }
    }
    Ok(password)
}

async fn health() -> Json<Value> {
    Json(json!({
        "status": "ready",
        "version": env!("CARGO_PKG_VERSION"),
    }))
}

async fn bootstrap(
    State(state): State<DaemonState>,
    headers: HeaderMap,
) -> std::result::Result<Response, ApiError> {
    require_auth(&headers, &state)?;
    let metadata_config = state.manager.lock().unwrap().config.clone();
    crate::models_cache::ensure_active_metadata(&state.paths, &metadata_config);
    state
        .state_store
        .recover_stale_turns()
        .map_err(ApiError::internal)?;
    let current_session = state.state_store.session_id();
    let (config, active_run_id, runs, context) = {
        let manager = state.manager.lock().unwrap();
        let runs: Vec<Value> = manager
            .active_runs
            .iter()
            .map(|(run_id, info)| {
                json!({
                    "run_id": run_id,
                    "session_id": &*info.session_id,
                    "mode": mode_name(info.mode),
                    "operation": info.operation.name(),
                    "turn_id": info.operation.turn_id(),
                    "input_id": info.operation.input_id(),
                })
            })
            .collect();
        (
            manager.config.clone(),
            manager.run_in_session(&current_session).cloned(),
            runs,
            manager.context,
        )
    };
    let running_target = state
        .state_store
        .running_turn_queue_target()
        .map_err(ApiError::internal)?;
    let external_target = active_run_id
        .is_none()
        .then_some(running_target.as_ref())
        .flatten();
    let mut assets_by_turn = HashMap::<String, Vec<ImageAsset>>::new();
    for asset in state
        .state_store
        .load_image_assets()
        .map_err(ApiError::internal)?
    {
        assets_by_turn
            .entry(asset.turn_id.clone())
            .or_default()
            .push(asset);
    }
    let mut artifacts_by_turn = HashMap::<String, Vec<ArtifactAsset>>::new();
    for artifact in state
        .state_store
        .load_artifact_assets()
        .map_err(ApiError::internal)?
    {
        artifacts_by_turn
            .entry(artifact.turn_id.clone())
            .or_default()
            .push(artifact);
    }
    let turns = state
        .state_store
        .load_turns()
        .map_err(ApiError::internal)?
        .into_iter()
        .filter(|turn| !turn.is_summary)
        .map(|turn| {
            let assets = assets_by_turn.remove(&turn.turn_id).unwrap_or_default();
            let artifacts = artifacts_by_turn.remove(&turn.turn_id).unwrap_or_default();
            SafeTurn::from_turn(turn, assets, artifacts)
        })
        .collect();
    let usage = state
        .state_store
        .usage_snapshot()
        .map_err(ApiError::internal)?
        .into();
    let queued_prompts = match external_target {
        Some(target) => state
            .state_store
            .load_queued_prompts_for_target(target)
            .map_err(ApiError::internal)?,
        None => state
            .state_store
            .load_queued_prompts()
            .map_err(ApiError::internal)?,
    }
    .into_iter()
    .map(SafeQueuedPrompt::from)
    .collect();
    let running_turn_id = running_target.as_ref().map(|target| target.turn_id.clone());
    let external_queue_available = external_target
        .is_some_and(|target| target.queue_session_id.is_some() && target.owner_pid.is_some());
    let current_session_id = state.state_store.session_id().to_string();
    let sessions = state
        .state_store
        .list_local_sessions(&config.active_persona_scope(), false)
        .map_err(ApiError::internal)?
        .iter()
        .map(|overview| session_overview_json(overview, &current_session_id))
        .collect();
    let persona = persona_identity(
        &config,
        &read_prompt_documents(&config, &state.paths).map_err(ApiError::internal)?,
    );
    let redo_candidate = if active_run_id.is_none() {
        state
            .state_store
            .redo_candidate()
            .map_err(ApiError::internal)?
            .map(SafeRedoCandidate::from)
    } else {
        None
    };
    let mut response = Json(BootstrapResponse {
        version: env!("CARGO_PKG_VERSION"),
        boot_id: state.boot_id.to_string(),
        latest_event_id: state.events.latest_id(),
        active_run_id,
        running_turn_id,
        external_queue_available,
        turns,
        queued_prompts,
        models: safe_models(&config),
        display: web_display_config(&config),
        context,
        usage,
        capabilities: Capabilities {
            multi_conversation: true,
            attachments: true,
            queue: true,
            redo: true,
        },
        sessions,
        current_session_id,
        runs,
        persona,
        redo_candidate,
    })
    .into_response();
    response
        .headers_mut()
        .insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    Ok(response)
}

async fn get_config(
    State(state): State<DaemonState>,
    headers: HeaderMap,
) -> std::result::Result<Response, ApiError> {
    require_auth(&headers, &state)?;
    let (config, context) = {
        let manager = state.manager.lock().unwrap();
        (manager.config.clone(), manager.context)
    };
    let mut response = Json(config_response(&config, context, &state.paths)?).into_response();
    response
        .headers_mut()
        .insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    Ok(response)
}

async fn update_config(
    State(state): State<DaemonState>,
    headers: HeaderMap,
    Json(request): Json<UpdateConfigRequest>,
) -> std::result::Result<Json<ConfigResponse>, ApiError> {
    require_mutation(&headers, &state)?;

    let current = state.manager.lock().unwrap().config.clone();
    let current_prompts =
        read_prompt_documents(&current, &state.paths).map_err(ApiError::internal)?;
    let mut candidate: AppConfig = serde_json::from_value(request.config).map_err(|error| {
        ApiError::new(
            StatusCode::BAD_REQUEST,
            format!("invalid configuration: {}", safe_error_message(error)),
        )
    })?;
    reconcile_qq_persona_references(&mut candidate, &request.prompts);
    candidate.normalize_platform_model_routes();
    restore_config_secrets(&mut candidate, &current, &request.secrets)?;
    validate_config_candidate(&candidate)?;
    validate_prompt_documents(&candidate, &request.prompts)?;
    let qq_listener = state
        .platforms
        .qq_listener
        .prepare(&state, Some(&current.platforms.qq), &candidate.platforms.qq)
        .await
        .map_err(|error| {
            ApiError::new(
                StatusCode::BAD_REQUEST,
                format!(
                    "Tencent QQ listener configuration failed: {}",
                    safe_error_message(error)
                ),
            )
        })?;
    let requested_prompts = request.prompts.clone();
    // Allowed while turns run: the ApplyConfig handler interrupts running
    // turns only for persona layout changes; everything else hot-applies.
    reserve_admin_light(&state.manager)?;
    let (reply, receiver) = oneshot::channel();
    if state
        .actor_tx
        .send(ActorCommand::ApplyConfig {
            config: Box::new(candidate),
            prompts: request.prompts,
            reset_conversation: false,
            reply,
        })
        .is_err()
    {
        release_admin(&state.manager);
        return Err(ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "agent worker is unavailable",
        ));
    }
    match receiver.await {
        Ok(Ok(())) => qq_listener.commit(),
        Ok(Err(AdminFailure::Invalid(message))) => {
            return Err(ApiError::new(StatusCode::BAD_REQUEST, message));
        }
        Ok(Err(AdminFailure::Internal(message))) => {
            tracing::error!(
                error = %message,
                "{}",
                t("WebUI configuration update failed", "WebUI 配置更新失败")
            );
            return Err(ApiError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                safe_error_message(&message),
            ));
        }
        Err(_) => {
            release_admin(&state.manager);
            return Err(ApiError::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "agent worker stopped before updating the configuration",
            ));
        }
    }
    cleanup_persona_assets(&state.paths, &current_prompts, &requested_prompts);
    let manager = state.manager.lock().unwrap();
    Ok(Json(config_response(
        &manager.config,
        manager.context,
        &state.paths,
    )?))
}

fn cleanup_persona_assets(
    paths: &MiyuPaths,
    previous: &PromptDocuments,
    current: &PromptDocuments,
) {
    let directory = paths.persona_avatars_dir();
    let Ok(entries) = std::fs::read_dir(&directory) else {
        return;
    };
    let referenced = |prompts: &PromptDocuments| {
        prompts
            .personas
            .iter()
            .flat_map(|document| {
                [
                    document.avatar_path.as_deref(),
                    document.board_image_path.as_deref(),
                ]
            })
            .flatten()
            .filter_map(|path| resolve_persona_asset_path(paths, path))
            .filter_map(|path| {
                path.strip_prefix(&directory)
                    .ok()
                    .map(|relative| relative.to_string_lossy().to_string())
            })
            .collect::<HashSet<_>>()
    };
    let previous = referenced(previous);
    let current = referenced(current);
    let now = std::time::SystemTime::now();
    for entry in entries.flatten() {
        let file_type = match entry.file_type() {
            Ok(file_type) => file_type,
            Err(_) => continue,
        };
        if !file_type.is_file() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        let stale = entry
            .metadata()
            .and_then(|metadata| metadata.modified())
            .ok()
            .and_then(|modified| now.duration_since(modified).ok())
            .is_some_and(|age| age >= std::time::Duration::from_secs(24 * 60 * 60));
        if name.starts_with(".upload-") {
            if stale {
                let _ = std::fs::remove_file(entry.path());
            }
            continue;
        }
        let bytes = name.as_bytes();
        let managed_name = bytes.len() >= 68
            && bytes[64] == b'.'
            && bytes[..64].iter().all(u8::is_ascii_hexdigit)
            && matches!(&bytes[65..], b"png" | b"jpg" | b"gif" | b"webp" | b"bmp");
        if !managed_name || current.contains(&name) {
            continue;
        }
        let old_reference = previous.contains(&name);
        if old_reference || stale {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

async fn image_asset(
    State(state): State<DaemonState>,
    headers: HeaderMap,
    Path(asset_id): Path<String>,
) -> std::result::Result<Response, ApiError> {
    require_auth(&headers, &state)?;
    if asset_id.len() > 96
        || asset_id.is_empty()
        || !asset_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
    {
        return Err(ApiError::new(
            StatusCode::NOT_FOUND,
            "image asset not found",
        ));
    }
    let Some(asset) = state
        .state_store
        .load_image_asset(&asset_id)
        .map_err(ApiError::internal)?
    else {
        return Err(ApiError::new(
            StatusCode::NOT_FOUND,
            "image asset not found",
        ));
    };
    let mut response = asset.bytes.into_response();
    response.headers_mut().insert(
        CONTENT_TYPE,
        HeaderValue::from_str(&asset.asset.mime).map_err(ApiError::internal)?,
    );
    response.headers_mut().insert(
        CACHE_CONTROL,
        HeaderValue::from_static("private, max-age=86400"),
    );
    response
        .headers_mut()
        .insert(X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    Ok(response)
}

async fn artifact_asset(
    State(state): State<DaemonState>,
    headers: HeaderMap,
    Path(asset_id): Path<String>,
) -> std::result::Result<Response, ApiError> {
    require_auth(&headers, &state)?;
    if asset_id.len() > 96
        || asset_id.is_empty()
        || !asset_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
    {
        return Err(ApiError::new(StatusCode::NOT_FOUND, "artifact not found"));
    }
    let Some(artifact) = state
        .state_store
        .load_artifact_asset(&asset_id)
        .map_err(ApiError::internal)?
    else {
        return Err(ApiError::new(StatusCode::NOT_FOUND, "artifact not found"));
    };
    let inline = matches!(
        artifact.asset.kind.as_str(),
        "markdown" | "text" | "code" | "json" | "pdf" | "html"
    );
    let disposition = format!(
        "{}; filename*=UTF-8''{}",
        if inline { "inline" } else { "attachment" },
        urlencoding::encode(&artifact.asset.file_name)
    );
    let mut response = artifact.bytes.into_response();
    response.headers_mut().insert(
        CONTENT_TYPE,
        HeaderValue::from_str(&artifact.asset.mime).map_err(ApiError::internal)?,
    );
    response.headers_mut().insert(
        CONTENT_DISPOSITION,
        HeaderValue::from_str(&disposition).map_err(ApiError::internal)?,
    );
    response
        .headers_mut()
        .insert(CACHE_CONTROL, HeaderValue::from_static("private, no-cache"));
    response
        .headers_mut()
        .insert(X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    if artifact.asset.kind == "html" {
        response.headers_mut().insert(
            CONTENT_SECURITY_POLICY,
            HeaderValue::from_static(
                "sandbox; default-src 'none'; style-src 'unsafe-inline'; img-src data: blob:",
            ),
        );
    }
    Ok(response)
}

async fn upload_user_attachment(
    State(state): State<DaemonState>,
    headers: HeaderMap,
    Query(query): Query<AttachmentQuery>,
    body: Bytes,
) -> std::result::Result<Json<SafeUserAttachment>, ApiError> {
    require_mutation(&headers, &state)?;
    let session_id =
        resolve_turn_session(&state, Some(query.session_id)).map_err(session_api_error)?;
    if body.is_empty() || body.len() > ATTACHMENT_BODY_LIMIT {
        return Err(ApiError::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            "attachment must be between 1 byte and 10 MiB",
        ));
    }
    let encoded_name = headers
        .get("x-miyu-filename")
        .and_then(|value| value.to_str().ok())
        .ok_or_else(|| ApiError::new(StatusCode::BAD_REQUEST, "attachment filename is required"))?;
    let decoded_name = urlencoding::decode(encoded_name)
        .map_err(|_| ApiError::new(StatusCode::BAD_REQUEST, "attachment filename is invalid"))?;
    let file_name = sanitize_attachment_file_name(&decoded_name)?;
    let (kind, mime, width, height) = inspect_user_attachment(&file_name, &body)?;
    let attachment = UserAttachment {
        attachment_id: random_id("att", 24),
        file_name,
        mime,
        kind,
        size_bytes: body.len() as u64,
        width,
        height,
        created_at: chrono::Utc::now().to_rfc3339(),
    };
    let store = state.state_store.pinned(&session_id);
    store
        .purge_stale_user_attachments()
        .map_err(ApiError::internal)?;
    store
        .save_user_attachment(&attachment, &body)
        .map_err(ApiError::internal)?;
    Ok(Json(SafeUserAttachment::from(attachment)))
}

async fn user_attachment(
    State(state): State<DaemonState>,
    headers: HeaderMap,
    Path(attachment_id): Path<String>,
) -> std::result::Result<Response, ApiError> {
    require_auth(&headers, &state)?;
    validate_attachment_id(&attachment_id)?;
    let Some(attachment) = state
        .state_store
        .load_user_attachment_by_id(&attachment_id)
        .map_err(ApiError::internal)?
    else {
        return Err(ApiError::new(StatusCode::NOT_FOUND, "attachment not found"));
    };
    let inline = attachment.attachment.kind == "image";
    let mut response = attachment.bytes.into_response();
    let content_type = if inline {
        attachment.attachment.mime.as_str()
    } else {
        "application/octet-stream"
    };
    response.headers_mut().insert(
        CONTENT_TYPE,
        HeaderValue::from_str(content_type).map_err(ApiError::internal)?,
    );
    response.headers_mut().insert(
        CONTENT_LENGTH,
        HeaderValue::from_str(&attachment.attachment.size_bytes.to_string())
            .map_err(ApiError::internal)?,
    );
    response.headers_mut().insert(
        CONTENT_DISPOSITION,
        attachment_content_disposition(&attachment.attachment.file_name, inline)?,
    );
    response.headers_mut().insert(
        CACHE_CONTROL,
        HeaderValue::from_static("private, max-age=86400"),
    );
    response
        .headers_mut()
        .insert(X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    Ok(response)
}

async fn delete_user_attachment(
    State(state): State<DaemonState>,
    headers: HeaderMap,
    Query(query): Query<AttachmentQuery>,
    Path(attachment_id): Path<String>,
) -> std::result::Result<StatusCode, ApiError> {
    require_mutation(&headers, &state)?;
    validate_attachment_id(&attachment_id)?;
    let session_id =
        resolve_turn_session(&state, Some(query.session_id)).map_err(session_api_error)?;
    let deleted = state
        .state_store
        .pinned(&session_id)
        .delete_staged_user_attachment(&attachment_id)
        .map_err(ApiError::internal)?;
    if !deleted {
        return Err(ApiError::new(StatusCode::NOT_FOUND, "attachment not found"));
    }
    Ok(StatusCode::NO_CONTENT)
}

fn validate_attachment_id(attachment_id: &str) -> std::result::Result<(), ApiError> {
    if attachment_id.len() <= 96
        && !attachment_id.is_empty()
        && attachment_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
    {
        return Ok(());
    }
    Err(ApiError::new(StatusCode::NOT_FOUND, "attachment not found"))
}

fn sanitize_attachment_file_name(value: &str) -> std::result::Result<String, ApiError> {
    let name = value
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or("")
        .trim()
        .chars()
        .filter(|character| !character.is_control())
        .take(180)
        .collect::<String>();
    if name.is_empty() || name == "." || name == ".." {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "attachment filename is invalid",
        ));
    }
    Ok(name)
}

fn inspect_user_attachment(
    file_name: &str,
    bytes: &[u8],
) -> std::result::Result<(String, String, u32, u32), ApiError> {
    if let Ok(reader) = image::ImageReader::new(std::io::Cursor::new(bytes)).with_guessed_format() {
        if let Some(format) = reader.format() {
            if matches!(
                format,
                image::ImageFormat::Png
                    | image::ImageFormat::Jpeg
                    | image::ImageFormat::WebP
                    | image::ImageFormat::Gif
            ) {
                let (width, height) = reader.into_dimensions().map_err(|_| {
                    ApiError::new(StatusCode::BAD_REQUEST, "attachment image is invalid")
                })?;
                if width == 0
                    || height == 0
                    || width > 40_000
                    || height > 40_000
                    || u64::from(width) * u64::from(height) > 40_000_000
                {
                    return Err(ApiError::new(
                        StatusCode::BAD_REQUEST,
                        "attachment image dimensions are outside the safety limit",
                    ));
                }
                return Ok((
                    "image".to_string(),
                    format.to_mime_type().to_string(),
                    width,
                    height,
                ));
            }
        }
    }
    if bytes.len() > MAX_TEXT_ATTACHMENT_BYTES {
        return Err(ApiError::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            "text attachment exceeds the 1 MiB limit",
        ));
    }
    std::str::from_utf8(bytes).map_err(|_| {
        ApiError::new(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "attachment is not UTF-8 text",
        )
    })?;
    let extension = FilePath::new(file_name)
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    const TEXT_EXTENSIONS: &[&str] = &[
        "txt", "md", "markdown", "json", "jsonl", "csv", "tsv", "log", "rs", "js", "jsx", "ts",
        "tsx", "py", "go", "java", "c", "cc", "cpp", "h", "hpp", "cs", "rb", "php", "swift", "kt",
        "kts", "sh", "bash", "zsh", "fish", "toml", "yaml", "yml", "xml", "html", "css", "scss",
        "sql",
    ];
    if !TEXT_EXTENSIONS.contains(&extension.as_str()) {
        return Err(ApiError::new(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "unsupported attachment type",
        ));
    }
    let mime = match extension.as_str() {
        "md" | "markdown" => "text/markdown",
        "json" | "jsonl" => "application/json",
        "csv" => "text/csv",
        "html" => "text/html",
        "css" => "text/css",
        _ => "text/plain",
    };
    Ok(("text".to_string(), mime.to_string(), 0, 0))
}

fn attachment_content_disposition(
    file_name: &str,
    inline: bool,
) -> std::result::Result<HeaderValue, ApiError> {
    let fallback = file_name
        .chars()
        .filter(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '.' | '-' | '_')
        })
        .take(80)
        .collect::<String>();
    let fallback = if fallback.is_empty() {
        "attachment"
    } else {
        &fallback
    };
    let disposition = if inline { "inline" } else { "attachment" };
    let value = format!(
        "{disposition}; filename=\"{fallback}\"; filename*=UTF-8''{}",
        urlencoding::encode(file_name)
    );
    HeaderValue::from_str(&value).map_err(ApiError::internal)
}

async fn events(
    State(state): State<DaemonState>,
    headers: HeaderMap,
    Query(query): Query<EventsQuery>,
) -> std::result::Result<Sse<impl Stream<Item = std::result::Result<Event, Infallible>>>, ApiError>
{
    require_auth(&headers, &state)?;
    let header_after = headers
        .get("last-event-id")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(0);
    let after = query.after.max(header_after);
    let subscription = state.events.subscribe_after(after);
    let stream_state = SseStreamState {
        pending: subscription.pending,
        receiver: subscription.receiver,
        events: state.events,
        last_id: after,
    };
    let events = stream::unfold(stream_state, |mut state| async move {
        loop {
            if let Some(record) = state.pending.pop_front() {
                if record.kind == "resync_required" {
                    state.last_id = record.id;
                    return Some((Ok(record_to_sse(record)), state));
                }
                if record.id <= state.last_id {
                    continue;
                }
                state.last_id = record.id;
                return Some((Ok(record_to_sse(record)), state));
            }
            match state.receiver.recv().await {
                Ok(record) if record.id > state.last_id => {
                    state.last_id = record.id;
                    return Some((Ok(record_to_sse(record)), state));
                }
                Ok(_) => {}
                Err(broadcast::error::RecvError::Lagged(_)) => {
                    state.pending = state.events.replay_after(state.last_id);
                }
                Err(broadcast::error::RecvError::Closed) => return None,
            }
        }
    });
    let ready =
        stream::once(async { Ok::<Event, Infallible>(Event::default().comment("connected")) });
    let stream = ready.chain(events);
    Ok(Sse::new(stream).keep_alive(
        KeepAlive::new()
            .interval(Duration::from_secs(15))
            .text("keepalive"),
    ))
}

struct SseStreamState {
    pending: VecDeque<EventRecord>,
    receiver: broadcast::Receiver<EventRecord>,
    events: EventHub,
    last_id: u64,
}

fn record_to_sse(record: EventRecord) -> Event {
    Event::default()
        .id(record.id.to_string())
        .event(record.kind)
        .data(record.data)
}

pub(crate) fn enqueue_turn_update(
    state: &DaemonState,
    request: TurnUpdateRequest,
) -> Result<TurnUpdateReceipt> {
    let manager = state.manager.lock().unwrap();
    if manager.admin_busy {
        bail!("{}", ipc::ADMIN_BUSY_MESSAGE);
    }
    let run = manager
        .active_runs
        .get(&request.run_id)
        .context("active run not found")?;
    if run.audience != request.audience {
        bail!("the active reply belongs to a different request source");
    }
    if request
        .session_id
        .as_deref()
        .is_some_and(|session_id| session_id != &*run.session_id)
    {
        bail!("the active reply belongs to a different conversation");
    }
    if run.turn_id.as_deref() != Some(request.turn_id.as_str()) {
        bail!("the active run no longer owns the requested turn");
    }
    let target = run
        .queue_target
        .clone()
        .context("the active turn is not ready to accept follow-up messages")?;
    if target.turn_id != request.turn_id {
        bail!("the active run queue target changed");
    }
    let session_id = run.session_id.clone();
    let supersede = run.supersede.clone();
    let prompt_id = random_id("queued", 18);
    let store = state.state_store.pinned(&session_id);
    store.recover_stale_turns()?;
    let prompt = store.enqueue_prompt_for_target_with_uploads(
        &target,
        &prompt_id,
        &request.content,
        &request.display_content,
        &request.attachments,
        &request.uploaded_attachment_ids,
    )?;
    if request.mode == TurnUpdateMode::Supersede {
        supersede.trigger();
    }
    state.events.publish(
        "queue.added",
        json!({
            "session_id": &*session_id,
            "run_id": request.run_id,
            "turn_id": request.turn_id,
            "mode": match request.mode {
                TurnUpdateMode::Followup => "followup",
                TurnUpdateMode::Supersede => "supersede",
            },
            "prompt": SafeQueuedPrompt::from(prompt.clone()),
        }),
    );
    Ok(TurnUpdateReceipt {
        run_id: request.run_id,
        turn_id: request.turn_id,
        session_id,
        prompt,
    })
}

struct PreparedWebAttachments {
    content: String,
    images: Vec<Option<ImageAttachment>>,
}

pub(crate) struct RedoWebPrompt {
    prompt_id: String,
    content: String,
    display_content: String,
    images: Vec<Option<ImageAttachment>>,
}

fn prepare_web_attachments(
    store: &StateStore,
    display_content: &str,
    attachment_ids: &[String],
) -> std::result::Result<PreparedWebAttachments, ApiError> {
    if attachment_ids.len() > MAX_ATTACHMENTS_PER_MESSAGE {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            format!("a message can include at most {MAX_ATTACHMENTS_PER_MESSAGE} attachments"),
        ));
    }
    let unique = attachment_ids.iter().collect::<HashSet<_>>();
    if unique.len() != attachment_ids.len()
        || attachment_ids
            .iter()
            .any(|id| validate_attachment_id(id).is_err())
    {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "attachment ids are invalid",
        ));
    }
    let attachments = store
        .load_staged_user_attachments(attachment_ids)
        .map_err(|error| ApiError::new(StatusCode::BAD_REQUEST, error.to_string()))?;
    prepare_web_attachment_data(display_content, attachments)
}

fn prepare_web_attachment_data(
    display_content: &str,
    attachments: Vec<crate::state::UserAttachmentData>,
) -> std::result::Result<PreparedWebAttachments, ApiError> {
    if attachments.len() > MAX_ATTACHMENTS_PER_MESSAGE {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            format!("a message can include at most {MAX_ATTACHMENTS_PER_MESSAGE} attachments"),
        ));
    }
    let total_bytes = attachments
        .iter()
        .map(|attachment| attachment.attachment.size_bytes)
        .sum::<u64>();
    if total_bytes > MAX_ATTACHMENT_TOTAL_BYTES {
        return Err(ApiError::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            "attachments exceed the 32 MiB per-message limit",
        ));
    }
    let mut content = if display_content.is_empty() {
        "请查看附件。".to_string()
    } else {
        display_content.to_string()
    };
    let mut images = Vec::new();
    for attachment in attachments {
        if attachment.attachment.kind == "image" {
            images.push(Some(ImageAttachment::Binary {
                mime: attachment.attachment.mime,
                data: attachment.bytes,
            }));
            continue;
        }
        let text = std::str::from_utf8(&attachment.bytes)
            .map_err(|_| ApiError::new(StatusCode::BAD_REQUEST, "text attachment is not UTF-8"))?;
        let name = escape_attachment_attribute(&attachment.attachment.file_name);
        let mime = escape_attachment_attribute(&attachment.attachment.mime);
        content.push_str(&format!(
            "\n\n<user-attachment name=\"{name}\" mime=\"{mime}\">\n{text}\n</user-attachment>"
        ));
    }
    Ok(PreparedWebAttachments { content, images })
}

fn escape_attachment_attribute(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn validate_message_content(
    content: String,
    has_attachments: bool,
) -> std::result::Result<String, ApiError> {
    if content.trim().is_empty() && has_attachments {
        return Ok(String::new());
    }
    validate_content(content)
}

async fn redo_turn(
    State(state): State<DaemonState>,
    headers: HeaderMap,
    Path((session_id, turn_id)): Path<(String, String)>,
    Json(request): Json<RedoTurnRequest>,
) -> std::result::Result<Response, ApiError> {
    require_mutation(&headers, &state)?;
    require_local_web_session(&state, &session_id)?;
    let mode = parse_mode(&request.mode)?;
    let store = state.state_store.pinned_for_turn(&session_id);
    let candidate = store
        .redo_candidate()
        .map_err(ApiError::internal)?
        .ok_or_else(|| ApiError::new(StatusCode::CONFLICT, "the last input cannot be redone"))?;
    if candidate.turn_id != turn_id
        || candidate.input_id != request.input_id
        || candidate.revision != request.expected_revision
    {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "the conversation changed before redo could start",
        ));
    }

    let mut prompts = Vec::new();
    match candidate.input_kind {
        crate::state::RedoInputKind::Initial => {
            let attachments = store
                .load_user_attachment_data_for_turn(&turn_id)
                .map_err(ApiError::internal)?;
            let display_content = validate_message_content(
                request
                    .content
                    .unwrap_or_else(|| candidate.display_content.clone()),
                !attachments.is_empty(),
            )?;
            let prepared = prepare_web_attachment_data(&display_content, attachments)?;
            prompts.push(RedoWebPrompt {
                prompt_id: candidate.input_id.clone(),
                content: prepared.content,
                display_content,
                images: prepared.images,
            });
        }
        crate::state::RedoInputKind::Followup => {
            let batch = store
                .load_redo_batch_prompts(&turn_id, &candidate.batch_prompt_ids)
                .map_err(ApiError::internal)?;
            for prompt in batch {
                if !prompt.attachments.is_empty() {
                    return Err(ApiError::new(
                        StatusCode::CONFLICT,
                        "this follow-up uses non-durable attachments and cannot be redone",
                    ));
                }
                let attachments = store
                    .load_user_attachment_data_for_prompt(&prompt.prompt_id)
                    .map_err(ApiError::internal)?;
                let display_content = if prompt.prompt_id == candidate.input_id {
                    validate_message_content(
                        request
                            .content
                            .clone()
                            .unwrap_or_else(|| prompt.display_content.clone()),
                        !attachments.is_empty(),
                    )?
                } else {
                    prompt.display_content
                };
                let prepared = prepare_web_attachment_data(&display_content, attachments)?;
                prompts.push(RedoWebPrompt {
                    prompt_id: prompt.prompt_id,
                    content: prepared.content,
                    display_content,
                    images: prepared.images,
                });
            }
        }
    }

    let run_id = random_id("run", 18);
    let (cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
    {
        let mut manager = state.manager.lock().unwrap();
        if manager.admin_busy || manager.session_has_runs(&session_id) {
            return Err(ApiError::new(
                StatusCode::CONFLICT,
                "Miyu is busy in this conversation",
            ));
        }
        manager.active_runs.insert(
            run_id.clone(),
            RunInfo {
                session_id: session_id.clone().into(),
                mode,
                audience: PromptAudience::External,
                cancel: cancel_tx,
                turn_id: Some(turn_id.clone()),
                queue_target: None,
                supersede: Arc::new(crate::agent::TurnSupersedeSignal::default()),
                platform_followup: None,
                operation: RunOperation::Redo {
                    turn_id: turn_id.clone(),
                    input_id: candidate.input_id.clone(),
                },
                job_wake: false,
                job_wake_label: None,
            },
        );
    }
    if state
        .actor_tx
        .send(ActorCommand::RedoTurn {
            run_id: run_id.clone(),
            session_id: session_id.into(),
            candidate,
            prompts,
            mode,
            cancel: cancel_rx,
        })
        .is_err()
    {
        finish_run(&state.manager, &run_id, None);
        return Err(ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "agent worker is unavailable",
        ));
    }
    Ok((
        StatusCode::ACCEPTED,
        Json(json!({
            "run_id": run_id,
            "turn_id": turn_id,
            "operation": "redo",
        })),
    )
        .into_response())
}

fn unique_run_target(
    manager: &ManagerState,
    session_id: &str,
    audience: PromptAudience,
) -> Option<(String, String)> {
    let mut runs = manager.active_runs.iter().filter(|(_, run)| {
        &*run.session_id == session_id && run.audience == audience && run.turn_id.is_some()
    });
    let (run_id, run) = runs.next()?;
    if runs.next().is_some() {
        return None;
    }
    Some((run_id.clone(), run.turn_id.clone()?))
}

async fn create_turn(
    State(state): State<DaemonState>,
    headers: HeaderMap,
    Json(request): Json<CreateTurnRequest>,
) -> std::result::Result<Response, ApiError> {
    require_mutation(&headers, &state)?;
    let attachment_ids = request.attachment_ids;
    let display_content = validate_message_content(request.content, !attachment_ids.is_empty())?;
    let mode = parse_mode(&request.mode)?;
    let session_id = resolve_turn_session(&state, request.session_id).map_err(session_api_error)?;
    state
        .state_store
        .recover_stale_turns()
        .map_err(ApiError::internal)?;
    // A running turn in the *target* session gets the message as a queued
    // follow-up (composer tray UX); other sessions run in parallel.
    let target_store = state.state_store.pinned(&session_id);
    let prepared = prepare_web_attachments(&target_store, &display_content, &attachment_ids)?;
    if target_store
        .has_running_turns()
        .map_err(ApiError::internal)?
        && state
            .manager
            .lock()
            .unwrap()
            .session_runs_match_audience(&session_id, PromptAudience::External)
    {
        let (run_id, turn_id) = unique_run_target(
            &state.manager.lock().unwrap(),
            &session_id,
            PromptAudience::External,
        )
        .ok_or_else(|| {
            ApiError::new(
                StatusCode::CONFLICT,
                "the running turn is not ready or is ambiguous",
            )
        })?;
        let receipt = enqueue_turn_update(
            &state,
            TurnUpdateRequest {
                run_id: run_id.clone(),
                turn_id: turn_id.clone(),
                session_id: Some(session_id.clone()),
                audience: PromptAudience::External,
                content: prepared.content,
                display_content,
                attachments: Vec::new(),
                uploaded_attachment_ids: attachment_ids,
                mode: TurnUpdateMode::Followup,
            },
        )
        .map_err(|error| ApiError::new(StatusCode::CONFLICT, error.to_string()))?;
        let prompt = SafeQueuedPrompt::from(receipt.prompt);
        return Ok((
            StatusCode::ACCEPTED,
            Json(json!({
                "queued": true,
                "prompt": prompt,
                "run_id": receipt.run_id,
                "running_turn_id": receipt.turn_id,
            })),
        )
            .into_response());
    }
    let run_id = random_id("run", 18);
    let (cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
    {
        let mut manager = state.manager.lock().unwrap();
        if manager.admin_busy || manager.session_has_runs(&session_id) {
            return Err(ApiError::new(
                StatusCode::CONFLICT,
                "Miyu is busy in this conversation",
            ));
        }
        manager.active_runs.insert(
            run_id.clone(),
            RunInfo {
                session_id: session_id.clone(),
                mode,
                audience: PromptAudience::External,
                cancel: cancel_tx,
                turn_id: None,
                queue_target: None,
                supersede: Arc::new(crate::agent::TurnSupersedeSignal::default()),
                platform_followup: None,
                operation: RunOperation::Create,
                job_wake: false,
                job_wake_label: None,
            },
        );
    }
    if let Err(error) = target_store.reserve_user_attachments(&attachment_ids, &run_id) {
        finish_run(&state.manager, &run_id, None);
        return Err(ApiError::new(StatusCode::BAD_REQUEST, error.to_string()));
    }
    if state
        .actor_tx
        .send(ActorCommand::StartTurn {
            run_id: run_id.clone(),
            session_id,
            display_content,
            content: prepared.content,
            attachment_run_id: (!attachment_ids.is_empty()).then_some(run_id.clone()),
            mode,
            images: prepared.images,
            cwd: None,
            audience: PromptAudience::External,
            profile: None,
            cancel: cancel_rx,
        })
        .is_err()
    {
        let _ = target_store.release_user_attachments_for_run(&run_id);
        finish_run(&state.manager, &run_id, None);
        return Err(ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "agent worker is unavailable",
        ));
    }
    Ok((StatusCode::ACCEPTED, Json(json!({ "run_id": run_id }))).into_response())
}

async fn queue_prompt(
    State(state): State<DaemonState>,
    headers: HeaderMap,
    Json(request): Json<QueuePromptRequest>,
) -> std::result::Result<Response, ApiError> {
    require_mutation(&headers, &state)?;
    let attachment_ids = request.attachment_ids;
    let display_content = validate_message_content(request.content, !attachment_ids.is_empty())?;
    let session_id = resolve_turn_session(&state, request.session_id).map_err(session_api_error)?;
    let store = state.state_store.pinned(&session_id);
    let prepared = prepare_web_attachments(&store, &display_content, &attachment_ids)?;
    let receipt = enqueue_turn_update(
        &state,
        TurnUpdateRequest {
            run_id: request.run_id,
            turn_id: request.turn_id,
            session_id: Some(session_id),
            audience: PromptAudience::External,
            content: prepared.content,
            display_content,
            attachments: Vec::new(),
            uploaded_attachment_ids: attachment_ids,
            mode: TurnUpdateMode::Followup,
        },
    )
    .map_err(|error| ApiError::new(StatusCode::CONFLICT, error.to_string()))?;
    let safe = SafeQueuedPrompt::from(receipt.prompt);
    Ok((StatusCode::ACCEPTED, Json(safe)).into_response())
}

async fn remove_queue_prompt(
    State(state): State<DaemonState>,
    headers: HeaderMap,
    Path((run_id, turn_id, prompt_id)): Path<(String, String, String)>,
) -> std::result::Result<StatusCode, ApiError> {
    require_mutation(&headers, &state)?;
    if prompt_id.len() > 96
        || prompt_id.is_empty()
        || !prompt_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
    {
        return Err(ApiError::new(
            StatusCode::NOT_FOUND,
            "queued prompt not found",
        ));
    }
    let manager = state.manager.lock().unwrap();
    let run = manager
        .active_runs
        .get(&run_id)
        .ok_or_else(|| ApiError::new(StatusCode::NOT_FOUND, "queued prompt target not found"))?;
    if run.audience != PromptAudience::External || run.turn_id.as_deref() != Some(&turn_id) {
        return Err(ApiError::new(
            StatusCode::NOT_FOUND,
            "queued prompt target not found",
        ));
    }
    let target = run
        .queue_target
        .clone()
        .ok_or_else(|| ApiError::new(StatusCode::CONFLICT, "the active turn is not ready"))?;
    let session_id = run.session_id.clone();
    drop(manager);
    let removed = state
        .state_store
        .pinned(&session_id)
        .remove_queued_prompt_for_target(&target, &prompt_id)
        .map_err(ApiError::internal)?;
    if !removed {
        return Err(ApiError::new(
            StatusCode::NOT_FOUND,
            "queued prompt not found",
        ));
    };
    state.events.publish(
        "queue.removed",
        json!({
            "session_id": &*session_id,
            "run_id": run_id,
            "turn_id": turn_id,
            "prompt_id": prompt_id,
        }),
    );
    Ok(StatusCode::NO_CONTENT)
}

async fn list_jobs_http(
    State(state): State<DaemonState>,
    headers: HeaderMap,
) -> std::result::Result<Response, ApiError> {
    require_auth(&headers, &state)?;
    Ok(Json(json!({ "jobs": tools::jobs::overview() })).into_response())
}

async fn stop_job_http(
    State(state): State<DaemonState>,
    headers: HeaderMap,
    Path(job_id): Path<String>,
) -> std::result::Result<Response, ApiError> {
    require_mutation(&headers, &state)?;
    tools::jobs::stop_job(&job_id)
        .await
        .map_err(|error| ApiError::new(StatusCode::NOT_FOUND, safe_error_message(&error)))?;
    tools::jobs::acknowledge(&job_id);
    state
        .events
        .publish("job.acknowledged", json!({ "job_id": job_id }));
    Ok(StatusCode::NO_CONTENT.into_response())
}

async fn cancel_run(
    State(state): State<DaemonState>,
    headers: HeaderMap,
    Path(run_id): Path<String>,
) -> std::result::Result<Response, ApiError> {
    require_mutation(&headers, &state)?;
    let cancelled = {
        let manager = state.manager.lock().unwrap();
        manager
            .active_runs
            .get(&run_id)
            .map(RunInfo::request_cancel)
    };
    if cancelled.is_none() {
        return Err(ApiError::new(StatusCode::NOT_FOUND, "active run not found"));
    }
    Ok((
        StatusCode::ACCEPTED,
        Json(json!({
            "run_id": run_id,
            "cancellation_requested": true,
        })),
    )
        .into_response())
}

async fn answer_question(
    State(state): State<DaemonState>,
    headers: HeaderMap,
    Path(question_id): Path<String>,
    Json(request): Json<AnswerQuestionRequest>,
) -> std::result::Result<StatusCode, ApiError> {
    require_mutation(&headers, &state)?;
    match state
        .questions
        .answer(&question_id, request.answers, |run_id, answers| {
            state.events.publish(
                "question.answered",
                json!({
                    "run_id": run_id,
                    "question_id": question_id,
                    "answers": answers,
                }),
            );
        }) {
        Ok(()) => {}
        Err(AnswerFailure::NotFound) => {
            return Err(ApiError::new(
                StatusCode::NOT_FOUND,
                "pending question not found",
            ));
        }
        Err(AnswerFailure::Invalid(message)) => {
            return Err(ApiError::new(StatusCode::BAD_REQUEST, message));
        }
        Err(AnswerFailure::Gone) => {
            return Err(ApiError::new(
                StatusCode::CONFLICT,
                "the question is no longer awaiting an answer",
            ));
        }
    }
    Ok(StatusCode::NO_CONTENT)
}

async fn close_question(
    State(state): State<DaemonState>,
    headers: HeaderMap,
    Path(question_id): Path<String>,
) -> std::result::Result<StatusCode, ApiError> {
    require_mutation(&headers, &state)?;
    match state.questions.close(&question_id, |run_id| {
        state.events.publish(
            "question.closed",
            json!({
                "run_id": run_id,
                "question_id": question_id,
            }),
        );
    }) {
        Ok(()) => {}
        Err(AnswerFailure::NotFound) => {
            return Err(ApiError::new(
                StatusCode::NOT_FOUND,
                "pending question not found",
            ));
        }
        Err(AnswerFailure::Gone) => {
            return Err(ApiError::new(
                StatusCode::CONFLICT,
                "the question is no longer awaiting an answer",
            ));
        }
        Err(AnswerFailure::Invalid(_)) => unreachable!("closing a question has no answer payload"),
    }
    Ok(StatusCode::NO_CONTENT)
}

async fn get_thinking_variants(
    State(state): State<DaemonState>,
    headers: HeaderMap,
) -> std::result::Result<Response, ApiError> {
    require_auth(&headers, &state)?;
    let config = state.manager.lock().unwrap().config.clone();
    let options =
        active_thinking_variant_options(&config, &state.paths).map_err(ApiError::internal)?;
    let mut response = Json(ThinkingVariantsResponse { options }).into_response();
    response
        .headers_mut()
        .insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    Ok(response)
}

async fn set_thinking_variants(
    State(state): State<DaemonState>,
    headers: HeaderMap,
    Json(request): Json<SetThinkingVariantsRequest>,
) -> std::result::Result<Json<ThinkingVariantsResponse>, ApiError> {
    require_mutation(&headers, &state)?;
    let updates = validate_thinking_variant_updates(request.updates)?;
    reserve_admin(&state.manager)?;
    let (reply, receiver) = oneshot::channel();
    if state
        .actor_tx
        .send(ActorCommand::SetThinkingVariants { updates, reply })
        .is_err()
    {
        release_admin(&state.manager);
        return Err(ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "agent worker is unavailable",
        ));
    }
    match receiver.await {
        Ok(Ok(())) => {}
        Ok(Err(AdminFailure::Invalid(message))) => {
            return Err(ApiError::new(StatusCode::BAD_REQUEST, message));
        }
        Ok(Err(AdminFailure::Internal(message))) => {
            tracing::error!(
                error = %message,
                "{}",
                t(
                    "WebUI thinking variant update failed",
                    "WebUI 思考程度更新失败"
                )
            );
            return Err(ApiError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                safe_error_message(&message),
            ));
        }
        Err(_) => {
            release_admin(&state.manager);
            return Err(ApiError::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "agent worker stopped before updating the thinking variant",
            ));
        }
    }
    let config = state.manager.lock().unwrap().config.clone();
    let options =
        active_thinking_variant_options(&config, &state.paths).map_err(ApiError::internal)?;
    Ok(Json(ThinkingVariantsResponse { options }))
}

#[derive(Deserialize)]
struct SetSessionModelsRequest {
    /// Empty clears the override so the session follows the global pool.
    #[serde(default)]
    models: Vec<ActiveProviderModelConfig>,
}

#[derive(Serialize)]
struct SessionModelsResponse {
    model_override: Option<Vec<ActiveProviderModelConfig>>,
}

async fn get_session_models_http(
    State(state): State<DaemonState>,
    headers: HeaderMap,
    Path(session_id): Path<String>,
) -> std::result::Result<Json<SessionModelsResponse>, ApiError> {
    require_auth(&headers, &state)?;
    let record = require_local_web_session(&state, &session_id)?;
    let model_override = state
        .state_store
        .session_model_override(&record.session_id)
        .map_err(ApiError::internal)?;
    Ok(Json(SessionModelsResponse { model_override }))
}

async fn set_session_models_http(
    State(state): State<DaemonState>,
    headers: HeaderMap,
    Path(session_id): Path<String>,
    Json(request): Json<SetSessionModelsRequest>,
) -> std::result::Result<Json<SessionModelsResponse>, ApiError> {
    require_mutation(&headers, &state)?;
    let record = require_local_web_session(&state, &session_id)?;
    let models = (!request.models.is_empty()).then(|| request.models);
    if let Some(models) = &models {
        let choices = {
            let manager = state.manager.lock().unwrap();
            manager.config.text_provider_model_choices()
        };
        for model in models {
            if !choices.iter().any(|choice| {
                choice.provider_id == model.provider_id && choice.model == model.model
            }) {
                return Err(ApiError::new(
                    StatusCode::BAD_REQUEST,
                    format!("unknown model: {}/{}", model.provider_id, model.model),
                ));
            }
        }
    }
    state
        .state_store
        .set_session_model_override(&record.session_id, models.as_deref())
        .map_err(ApiError::internal)?;
    state.events.publish(
        "session.updated",
        json!({ "session_id": record.session_id, "model_override": models }),
    );
    Ok(Json(SessionModelsResponse {
        model_override: models,
    }))
}

async fn set_models(
    State(state): State<DaemonState>,
    headers: HeaderMap,
    Json(request): Json<SetModelsRequest>,
) -> std::result::Result<Json<ModelResponse>, ApiError> {
    require_mutation(&headers, &state)?;
    let models = validate_model_selection(request.models)?;
    reserve_admin_light(&state.manager)?;
    let (reply, receiver) = oneshot::channel();
    if state
        .actor_tx
        .send(ActorCommand::SetModels { models, reply })
        .is_err()
    {
        release_admin(&state.manager);
        return Err(ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "agent worker is unavailable",
        ));
    }
    match receiver.await {
        Ok(Ok(())) => {}
        Ok(Err(AdminFailure::Invalid(message))) => {
            return Err(ApiError::new(StatusCode::BAD_REQUEST, message));
        }
        Ok(Err(AdminFailure::Internal(message))) => {
            tracing::error!(
                error = %message,
                "{}",
                t("WebUI model update failed", "WebUI 模型更新失败")
            );
            return Err(ApiError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                safe_error_message(&message),
            ));
        }
        Err(_) => {
            release_admin(&state.manager);
            return Err(ApiError::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "agent worker stopped before updating the model",
            ));
        }
    }
    let manager = state.manager.lock().unwrap();
    Ok(Json(ModelResponse {
        models: safe_models(&manager.config),
        display: web_display_config(&manager.config),
        context: manager.context,
    }))
}

async fn reset_conversation(
    State(state): State<DaemonState>,
    headers: HeaderMap,
    Json(request): Json<ResetConversationRequest>,
) -> std::result::Result<StatusCode, ApiError> {
    require_mutation(&headers, &state)?;
    let session_id = request
        .session_id
        .unwrap_or_else(|| state.state_store.session_id().to_string());
    require_local_web_session(&state, &session_id)?;
    let store = state.state_store.pinned(&session_id);
    if store.has_running_turns().map_err(ApiError::internal)? {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "a conversation turn is already running",
        ));
    }
    reserve_admin_for_session(&state.manager, &session_id)?;
    let (reply, receiver) = oneshot::channel();
    if state
        .actor_tx
        .send(ActorCommand::ResetConversation {
            session_id: session_id.into(),
            reply,
        })
        .is_err()
    {
        release_admin(&state.manager);
        return Err(ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "agent worker is unavailable",
        ));
    }
    match receiver.await {
        Ok(Ok(())) => Ok(StatusCode::NO_CONTENT),
        Ok(Err(AdminFailure::Invalid(message))) => {
            Err(ApiError::new(StatusCode::CONFLICT, message))
        }
        Ok(Err(AdminFailure::Internal(message))) => {
            tracing::error!(
                error = %message,
                "{}",
                t("WebUI conversation reset failed", "WebUI 对话重置失败")
            );
            Err(ApiError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                safe_error_message(&message),
            ))
        }
        Err(_) => {
            release_admin(&state.manager);
            Err(ApiError::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "agent worker stopped before resetting the conversation",
            ))
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn spawn_actor(
    config: AppConfig,
    paths: MiyuPaths,
    state_store: StateStore,
    manager: Arc<Mutex<ManagerState>>,
    events: EventHub,
    questions: QuestionBroker,
    turn_engine: TurnEngineState,
    memory_organizer: Option<MemoryOrganizerHandle>,
) -> Result<(mpsc::UnboundedSender<ActorCommand>, JoinHandle<Result<()>>)> {
    let (sender, receiver) = mpsc::unbounded_channel();
    let join = std::thread::Builder::new()
        .name("miyu-daemon-core".to_string())
        // tiktoken 词元计数器首次初始化会走 fancy_regex/regex_automata 的深递归
        // 编译，debug 构建栈帧大，默认 2MB 线程栈会溢出（release 勉强够用）
        .stack_size(16 * 1024 * 1024)
        .spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .context("building daemon core runtime")?;
            // Turns are spawned as local tasks so several can run
            // concurrently on this thread (they are IO-bound); LocalSet
            // avoids imposing Send on the agent futures.
            let local = tokio::task::LocalSet::new();
            runtime.block_on(local.run_until(actor_loop(
                config,
                paths,
                state_store,
                manager,
                events,
                questions,
                turn_engine,
                memory_organizer,
                receiver,
            )));
            Ok(())
        })
        .context("starting daemon core thread")?;
    Ok((sender, join))
}

#[allow(clippy::too_many_arguments)]
async fn actor_loop(
    mut config: AppConfig,
    paths: MiyuPaths,
    state_store: StateStore,
    manager: Arc<Mutex<ManagerState>>,
    events: EventHub,
    questions: QuestionBroker,
    turn_engine: TurnEngineState,
    memory_organizer: Option<MemoryOrganizerHandle>,
    mut receiver: mpsc::UnboundedReceiver<ActorCommand>,
) {
    let mut agent: Option<Agent> = None;
    let resource_cache = Arc::new(Mutex::new(TurnResourceCache::default()));
    while let Some(command) = receiver.recv().await {
        match command {
            ActorCommand::StartTurn {
                run_id,
                session_id,
                content,
                display_content,
                attachment_run_id,
                mode,
                images,
                cwd,
                audience,
                profile,
                cancel,
            } => {
                // Stale-turn recovery is owner-pid safe. Prompt maintenance is
                // performed after per-turn platform overrides are applied.
                let _ = state_store.recover_stale_turns();
                let store = state_store.pinned_for_turn(&session_id);
                // Per-turn workspace: a workspace bound to the session wins,
                // otherwise the calling client's cwd, otherwise the daemon
                // process cwd. The resolved path scopes the whole turn task.
                let workspace = store
                    .session_record(&session_id)
                    .ok()
                    .flatten()
                    .and_then(|record| record.workspace.map(std::path::PathBuf::from))
                    .filter(|path| path.is_dir())
                    .or_else(|| cwd.filter(|path| path.is_dir()))
                    .or_else(|| std::env::current_dir().ok())
                    .unwrap_or_else(|| std::path::PathBuf::from("."));
                let task = run_turn_task(
                    config.clone(),
                    paths.clone(),
                    store,
                    state_store.clone(),
                    manager.clone(),
                    events.clone(),
                    questions.clone(),
                    run_id,
                    session_id.clone(),
                    TurnTaskInput::Create {
                        content,
                        display_content,
                        attachment_run_id,
                        images,
                    },
                    mode,
                    audience,
                    profile,
                    cancel,
                    resource_cache.clone(),
                    turn_engine.clone(),
                    memory_organizer.clone(),
                );
                tokio::task::spawn_local(crate::tools::workspace::with_workspace(
                    workspace,
                    crate::tools::workspace::with_session(session_id, task),
                ));
            }
            ActorCommand::RedoTurn {
                run_id,
                session_id,
                candidate,
                prompts,
                mode,
                cancel,
            } => {
                let _ = state_store.recover_stale_turns();
                let store = state_store.pinned_for_turn(&session_id);
                let workspace = store
                    .session_record(&session_id)
                    .ok()
                    .flatten()
                    .and_then(|record| record.workspace.map(std::path::PathBuf::from))
                    .filter(|path| path.is_dir())
                    .or_else(|| std::env::current_dir().ok())
                    .unwrap_or_else(|| std::path::PathBuf::from("."));
                let task = run_turn_task(
                    config.clone(),
                    paths.clone(),
                    store,
                    state_store.clone(),
                    manager.clone(),
                    events.clone(),
                    questions.clone(),
                    run_id,
                    session_id.clone(),
                    TurnTaskInput::Redo { candidate, prompts },
                    mode,
                    PromptAudience::External,
                    None,
                    cancel,
                    resource_cache.clone(),
                    turn_engine.clone(),
                    memory_organizer.clone(),
                );
                tokio::task::spawn_local(crate::tools::workspace::with_workspace(
                    workspace,
                    crate::tools::workspace::with_session(session_id, task),
                ));
            }
            ActorCommand::SetModels { models, reply } => {
                let result = rebuild_for_models(
                    &mut agent,
                    &mut config,
                    &paths,
                    &state_store,
                    &manager,
                    &models,
                );
                if result.is_ok() {
                    resource_cache.lock().unwrap().clear();
                    turn_engine.set(if agent.is_some() {
                        TurnEngineState::READY
                    } else {
                        TurnEngineState::COLD
                    });
                }
                release_admin(&manager);
                let _ = reply.send(result);
            }
            ActorCommand::SetThinkingVariants { updates, reply } => {
                let result = apply_thinking_variant_updates(&mut agent, &config, &paths, &updates);
                if result.is_ok() {
                    resource_cache.lock().unwrap().clear();
                }
                release_admin(&manager);
                let _ = reply.send(result);
            }
            ActorCommand::ApplyConfig {
                config: next_config,
                prompts,
                reset_conversation,
                reply,
            } => {
                // Persona layout changes migrate or delete session state that
                // running turns may be standing on, so those interrupt every
                // running turn before applying ("save after interrupting").
                // All other changes hot-apply: running turns keep the config
                // snapshot they cloned at start and later turns use the new
                // configuration.
                if config_change_requires_interrupt(&config, &next_config, &paths, &prompts) {
                    for info in manager.lock().unwrap().active_runs.values() {
                        info.request_cancel();
                    }
                    for _ in 0..100 {
                        if manager.lock().unwrap().active_runs.is_empty() {
                            break;
                        }
                        tokio::time::sleep(Duration::from_millis(50)).await;
                    }
                }
                let result = rebuild_for_config(
                    &mut agent,
                    &mut config,
                    &paths,
                    &state_store,
                    &manager,
                    &events,
                    *next_config,
                    &prompts,
                    reset_conversation,
                );
                if result.is_ok() {
                    resource_cache.lock().unwrap().clear();
                    turn_engine.set(if agent.is_some() {
                        TurnEngineState::READY
                    } else {
                        TurnEngineState::COLD
                    });
                    if let Some(handle) = memory_organizer.as_ref() {
                        handle.wake(config.clone(), paths.clone(), state_store.clone());
                    }
                }
                release_admin(&manager);
                let _ = reply.send(result);
            }
            ActorCommand::ResetConversation { session_id, reply } => {
                let result = reset_actor_conversation(
                    &mut agent,
                    &config,
                    &paths,
                    &state_store,
                    &manager,
                    &events,
                    &session_id,
                );
                release_admin(&manager);
                let _ = reply.send(result);
            }
            ActorCommand::ResetPersonaState {
                config: reset_config,
                reply,
            } => {
                let result = reset_actor_persona_state(
                    &mut agent,
                    &config,
                    &reset_config,
                    &paths,
                    &state_store,
                    &manager,
                    &events,
                );
                if result.is_ok() {
                    resource_cache.lock().unwrap().clear();
                }
                release_admin(&manager);
                let _ = reply.send(result);
            }
            ActorCommand::ClearSessionContent { session_id, reply } => {
                let result = clear_actor_session_content(
                    &mut agent,
                    &config,
                    &state_store,
                    &manager,
                    &session_id,
                );
                release_admin(&manager);
                let _ = reply.send(result);
            }
            ActorCommand::SwitchSession {
                session_id,
                release_reservation,
                reply,
            } => {
                let result = switch_actor_session(
                    agent.as_ref(),
                    &config,
                    &state_store,
                    &manager,
                    &events,
                    &session_id,
                );
                if release_reservation {
                    release_admin(&manager);
                }
                let _ = reply.send(result);
            }
            ActorCommand::Shutdown => {
                // Cancel every running turn, then drain briefly so they can
                // persist their interrupted state before the runtime drops.
                for info in manager.lock().unwrap().active_runs.values() {
                    info.request_cancel();
                }
                for _ in 0..100 {
                    if manager.lock().unwrap().active_runs.is_empty() {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
                break;
            }
            ActorCommand::Undo { session_id, reply } => {
                let result = (|| -> std::result::Result<Value, AdminFailure> {
                    let store = state_store.pinned(&session_id);
                    let (removed, prompt) = store
                        .undo_last_turn()
                        .map_err(|error| AdminFailure::Internal(safe_error_message(&error)))?;
                    if &*state_store.session_id() == &*session_id {
                        manager.lock().unwrap().context =
                            actor_context(&agent, &config, &state_store).map_err(|error| {
                                AdminFailure::Internal(safe_error_message(&error))
                            })?;
                    }
                    Ok(json!({ "removed": removed, "prompt": prompt }))
                })();
                release_admin(&manager);
                let _ = reply.send(result);
            }
            ActorCommand::Pop {
                session_id,
                turn_ids,
                reply,
            } => {
                let result = (|| -> std::result::Result<Value, AdminFailure> {
                    if turn_ids.is_empty() {
                        return Ok(json!({ "turns": 0, "archived": false }));
                    }
                    let store = state_store.pinned(&session_id);
                    let turns = store
                        .oldest_evictable_visible_turns(usize::MAX)
                        .map_err(|error| AdminFailure::Internal(safe_error_message(&error)))?;
                    let selected = turns
                        .into_iter()
                        .filter(|turn| turn_ids.iter().any(|id| id == &turn.turn_id))
                        .collect::<Vec<_>>();
                    if selected.len() != turn_ids.len() {
                        return Err(AdminFailure::Invalid(
                            "one or more conversation turns are no longer available".to_string(),
                        ));
                    }
                    let memory = MemoryStore::new(&config, &paths);
                    let memory_config = config.memory_config();
                    archive_and_delete_visible_turns(&store, &memory, &selected)
                        .map_err(|error| AdminFailure::Internal(safe_error_message(&error)))?;
                    if &*state_store.session_id() == &*session_id {
                        manager.lock().unwrap().context =
                            actor_context(&agent, &config, &state_store).map_err(|error| {
                                AdminFailure::Internal(safe_error_message(&error))
                            })?;
                    }
                    let data = json!({
                        "turns": selected.len(),
                        "archived": memory_config.enabled && memory_config.evicted_context_enabled
                    });
                    let mut event_data = data.clone();
                    event_data["session_id"] = json!(&*session_id);
                    events.publish("conversation.pop", event_data);
                    Ok(data)
                })();
                release_admin(&manager);
                let _ = reply.send(result);
            }
            ActorCommand::Compact { session_id, reply } => {
                let result = async {
                    let updates_default = &*state_store.session_id() == &*session_id;
                    let compact = if updates_default {
                        let agent = ensure_actor_agent(
                            &mut agent,
                            &config,
                            &paths,
                            &state_store,
                            &turn_engine,
                        )?;
                        let compact = agent
                            .compact_now(|_| Ok(()))
                            .await
                            .map_err(|error| AdminFailure::Internal(safe_error_message(&error)))?;
                        manager.lock().unwrap().context = current_context(agent)
                            .map_err(|error| AdminFailure::Internal(safe_error_message(&error)))?;
                        compact
                    } else {
                        let store = state_store.pinned(&session_id);
                        let target_agent = build_actor_agent(&config, &paths, &store)
                            .map_err(|error| AdminFailure::Internal(safe_error_message(&error)))?;
                        target_agent
                            .compact_now(|_| Ok(()))
                            .await
                            .map_err(|error| AdminFailure::Internal(safe_error_message(&error)))?
                    };
                    Ok::<Value, AdminFailure>(json!({
                        "compacted": compact.is_some(),
                        "usage": compact.as_ref().and_then(|result| result.usage.clone()),
                        "usage_estimated": compact
                            .as_ref()
                            .map(|result| result.usage_estimated)
                            .unwrap_or(false)
                    }))
                }
                .await;
                release_admin(&manager);
                let _ = reply.send(result);
            }
        }
    }
}

#[cfg(target_os = "linux")]
fn trim_process_memory() {
    unsafe {
        libc::malloc_trim(0);
    }
}

#[cfg(not(target_os = "linux"))]
fn trim_process_memory() {}

struct AttachmentRunGuard {
    store: StateStore,
    run_id: Option<String>,
}

enum TurnTaskInput {
    Create {
        content: String,
        display_content: String,
        attachment_run_id: Option<String>,
        images: Vec<Option<ImageAttachment>>,
    },
    Redo {
        candidate: crate::state::RedoCandidate,
        prompts: Vec<RedoWebPrompt>,
    },
}

fn into_pasted_images(
    images: Vec<Option<ImageAttachment>>,
) -> Vec<Option<crate::clipboard::PastedImage>> {
    images
        .into_iter()
        .map(|image| {
            image.map(|image| match image {
                ImageAttachment::Binary { mime, data } => crate::clipboard::PastedImage::Binary(
                    crate::clipboard::ClipboardImage::new(mime, data),
                ),
                ImageAttachment::Path { path } => crate::clipboard::PastedImage::Path(path),
            })
        })
        .collect()
}

impl AttachmentRunGuard {
    fn new(store: StateStore, run_id: Option<String>) -> Self {
        Self { store, run_id }
    }
}

impl Drop for AttachmentRunGuard {
    fn drop(&mut self) {
        if let Some(run_id) = self.run_id.as_deref() {
            let _ = self.store.release_user_attachments_for_run(run_id);
        }
    }
}

/// Executes one turn as a self-contained task. Multiple turn tasks run
/// concurrently on the actor's LocalSet — each with its own Agent, a
/// StateStore pinned to the turn's session, and an independent cancel signal.
#[allow(clippy::too_many_arguments)]
async fn run_turn_task(
    mut config: AppConfig,
    paths: MiyuPaths,
    store: StateStore,
    base_store: StateStore,
    manager: Arc<Mutex<ManagerState>>,
    events: EventHub,
    questions: QuestionBroker,
    run_id: String,
    session_id: Arc<str>,
    input: TurnTaskInput,
    mode: AgentMode,
    audience: PromptAudience,
    profile: Option<platforms::TurnProfile>,
    mut cancel: tokio::sync::watch::Receiver<bool>,
    resource_cache: Arc<Mutex<TurnResourceCache>>,
    turn_engine: TurnEngineState,
    memory_organizer: Option<MemoryOrganizerHandle>,
) {
    let attachment_run_id = match &input {
        TurnTaskInput::Create {
            attachment_run_id, ..
        } => attachment_run_id.clone(),
        TurnTaskInput::Redo { .. } => None,
    };
    let _attachment_guard = AttachmentRunGuard::new(base_store.clone(), attachment_run_id.clone());
    if let Some(profile) = &profile {
        if let Some(active_persona) = &profile.active_persona {
            config.prompt.active_persona.clone_from(active_persona);
        }
        if let Some(models) = &profile.text_models {
            config.active_provider_models = Some(models.clone());
        }
        // Groups drop whole turns instead of summarising: a compaction would
        // fold the structured group log into prose and every
        // `回复引用: msg=…` in the surviving turns would point at nothing.
        if let Some(group_context) = &profile.group_context {
            if !group_context.on_overflow.trim().is_empty() {
                config.context.on_overflow = group_context.on_overflow.trim().to_string();
            }
            if group_context.trim_batch_ratio > 0.0 {
                config.context.trim_batch_ratio = group_context.trim_batch_ratio;
            }
        }
        if let Some(models) = &profile.multimodal_models {
            config.active_multimodal_provider_models = Some(models.clone());
            // A conversation-specific multimodal pool is an explicit
            // override of the global vision plugin's single-model choice.
            config.plugins.vision.vision_provider_id.clear();
            config.plugins.vision.vision_model.clear();
        }
    }
    // Local sessions (REPL/WebUI/shell hook) may pin their own model pool.
    // Platform turns were already routed through the platform pools above.
    if profile
        .as_ref()
        .is_none_or(|profile| profile.text_models.is_none())
    {
        match base_store.session_model_override(&session_id) {
            Ok(Some(models)) => config.active_provider_models = Some(models),
            Ok(None) => {}
            Err(error) => tracing::warn!(
                error = %error,
                session_id = &*session_id,
                "{}",
                t(
                    "loading the session model override failed",
                    "读取会话模型覆盖失败"
                )
            ),
        }
    }
    let manager = &manager;
    let events = &events;
    let questions = &questions;
    let run_id = run_id.as_str();
    let operation = match &input {
        TurnTaskInput::Create { .. } => "create",
        TurnTaskInput::Redo { .. } => "redo",
    };
    events.publish(
        "run.started",
        json!({
            "run_id": run_id,
            "session_id": &*session_id,
            "mode": mode_name(mode),
            "operation": operation,
        }),
    );
    let title_seed: String = match &input {
        TurnTaskInput::Create { content, .. } => content.chars().take(80).collect(),
        TurnTaskInput::Redo { candidate, .. } => {
            candidate.display_content.chars().take(80).collect()
        }
    };
    let warming = !turn_engine.is_ready();
    if warming {
        turn_engine.set(TurnEngineState::INITIALIZING);
    }
    let setup = (|| -> Result<(Agent, AgentTurnControl)> {
        let platform_context = profile
            .as_ref()
            .and_then(|profile| profile.platform.as_deref());
        let local_webui = is_local_webui_request(audience, profile.is_some());
        let resources = resource_cache
            .lock()
            .map_err(|_| anyhow::anyhow!("turn resource cache is poisoned"))?
            .get_or_build(&config, &paths)?;
        let restricted = platform_context.is_some_and(|context| !context.host_tools_allowed());
        let mut normal_tools = if restricted {
            resources.restricted_tools.clone()
        } else {
            resources.normal_tools.clone()
        };
        let mut plan_tools = if restricted {
            resources.restricted_tools.clone()
        } else {
            resources.plan_tools.clone()
        };
        let mut chat_tools = if restricted {
            resources.restricted_tools.clone()
        } else {
            resources.chat_tools.clone()
        };
        if !restricted {
            if let Some(context) = platform_context {
                tools::rescope_platform_memory_tools(
                    &mut normal_tools,
                    &config,
                    &paths,
                    context,
                    false,
                );
                tools::rescope_platform_memory_tools(
                    &mut plan_tools,
                    &config,
                    &paths,
                    context,
                    true,
                );
            }
        }
        if local_webui && config.tools.enabled {
            tools::register_webui_artifact_tools(&mut normal_tools, &paths, &session_id);
            tools::register_webui_artifact_tools(&mut plan_tools, &paths, &session_id);
        }
        if profile
            .as_ref()
            .is_some_and(|profile| !profile.memory_write_enabled)
        {
            normal_tools.unregister("remember_fact");
            plan_tools.unregister("remember_fact");
            chat_tools.unregister("remember_fact");
        }
        if platform_context.is_none() && config.tools.enabled {
            tools::register_ask_question(&mut normal_tools);
            tools::register_ask_question(&mut plan_tools);
            tools::register_ask_question(&mut chat_tools);
        }
        if config.tools.enabled {
            if let Some(context) = profile
                .as_ref()
                .and_then(|profile| profile.platform.clone())
            {
                platforms::register_platform_tools(&mut normal_tools, context.clone());
                platforms::register_platform_tools(&mut plan_tools, context.clone());
                platforms::register_platform_tools(&mut chat_tools, context);
            }
        }
        let active_tools = match mode {
            AgentMode::Normal => normal_tools.clone(),
            AgentMode::Plan => plan_tools.clone(),
            AgentMode::Chat => chat_tools.clone(),
        };
        let mut agent = Agent::new_for_audience(
            config.clone(),
            &paths,
            store.clone(),
            // A platform turn buffers a whole round and posts it as one
            // message, so a stream that dies mid-round showed the group
            // nothing and can be retried on another endpoint — or the same
            // one — without anybody seeing a false start.
            resources
                .client
                .clone()
                .with_buffered_delivery(platform_context.is_some()),
            active_tools,
            mode,
            audience,
        )?;
        let mut runtime_system_context = profile
            .as_ref()
            .map(|profile| profile.system_context.clone())
            .unwrap_or_default();
        let mut turn_system_context = profile
            .as_ref()
            .map(|profile| profile.turn_system_context.clone())
            .unwrap_or_default();
        if local_webui && matches!(mode, AgentMode::Normal | AgentMode::Plan) {
            let manifest = tools::webui_artifact_manifest(&paths, &session_id)
                .unwrap_or_else(|_| "（Artifact 清单暂时不可用）".to_string());
            // v7 Phase 2.1: the manifest changes whenever artifacts change, so
            // it rides the turn tail; only the static policy stays in the
            // system prompt.
            turn_system_context.push(format!(
                "<artifact-workspace>\n{manifest}\n使用 read_artifact 和 apply_artifact_patch 按文件名操作已有 Artifact；不要用 glob 搜索托管目录，也不要猜测 ~/.miyu 路径。\n</artifact-workspace>"
            ));
            runtime_system_context.push(
                "<artifact-policy>\n\
                你正在 Miyu WebUI 中工作，并且拥有 Artifact 展示工具。\n\
                - 当用户明确要求报告、文档、网页、表格、数据文件、独立代码文件或其他可下载成品时，必须创建或展示 Artifact。\n\
                - 对由你直接编写的文本交付物，优先调用 create_artifact；filename 必须带正确扩展名。\n\
                - 对命令或其他工具已经生成的文件，调用 present_artifact。\n\
                - 更新已有 Artifact 时先使用 read_artifact，再使用 apply_artifact_patch 做局部修改；补丁路径只写 Artifact 文件名。除非用户明确要求完全重写，否则不要用 create_artifact 覆盖全文。\n\
                - 内容完成并自检后再发布。普通项目源码修改、配置修改、测试夹具和简短回答不要发布为 Artifact。\n\
                - Artifact 是回答的一部分；发布成功后再用简短文字告知用户。\n\
                </artifact-policy>"
                    .to_string(),
            );
        }
        if !runtime_system_context.is_empty() {
            agent.set_runtime_system_context(runtime_system_context)?;
        }
        if !turn_system_context.is_empty() {
            agent.set_turn_system_context(turn_system_context);
        }
        if let Some(profile) = &profile {
            agent.set_memory_writes_enabled(profile.memory_write_enabled);
            agent.set_memory_content(profile.memory_content.clone());
            agent.set_session_history_suppressed(profile.suppress_session_history);
            if let Some(namespace) = profile.image_cache_namespace.as_deref() {
                agent.set_image_platform(
                    namespace,
                    profile.image_source_label.as_deref().unwrap_or(namespace),
                );
            }
            if let Some(context) = profile.platform.as_deref() {
                let principal = context.principal().stable_key();
                agent.set_memory_request_context(
                    if context.is_admin {
                        MemoryAccess::Privileged
                    } else {
                        MemoryAccess::principal(principal.clone())
                    },
                    Some(principal),
                    context.sender_display_name.clone(),
                );
                agent.set_memory_origin(MemoryOrigin {
                    kind: "platform".to_string(),
                    platform: context.conversation.platform.clone(),
                    account_id: context.conversation.account_id.clone(),
                    conversation_kind: context.conversation.kind.as_str().to_string(),
                    conversation_id: context.conversation.conversation_id.clone(),
                    sender_id: context.sender_id.clone(),
                    sender_display_name: context.sender_display_name.clone(),
                    session_id: session_id.to_string(),
                    message_id: context
                        .inbound_event()
                        .map(|event| event.message_id.clone())
                        .unwrap_or_default(),
                });
            }
            if let Some(context) = profile.platform.clone() {
                agent.set_platform_context_images(context, profile.context_images.clone());
            }
        }
        if let Some(organizer) = memory_organizer.clone() {
            agent.set_memory_organizer(organizer);
        }
        agent.prepare_for_turn()?;
        let mut control = AgentTurnControl::new(mode, normal_tools, plan_tools, chat_tools);
        if let Some(signal) = manager
            .lock()
            .unwrap()
            .active_runs
            .get(run_id)
            .map(|run| run.supersede.clone())
        {
            control.set_supersede_signal(signal);
        }
        if let Some(ingress) = profile
            .as_ref()
            .and_then(|profile| profile.followup.as_ref())
            .map(|followup| followup.ingress())
        {
            control.set_queue_ingress(ingress);
        }
        Ok((agent, control))
    })();
    let (mut agent, control) = match setup {
        Ok(setup) => {
            turn_engine.set(TurnEngineState::READY);
            setup
        }
        Err(error) => {
            if warming {
                turn_engine.set(TurnEngineState::FAILED);
            }
            questions.cancel_run(run_id);
            finish_run(manager, run_id, None);
            let message = safe_error_message(&error);
            tracing::error!(
                run_id,
                error = %error,
                "{}",
                t("WebUI agent run setup failed", "WebUI 智能体运行初始化失败")
            );
            events.publish(
                "run.failed",
                json!({ "run_id": run_id, "session_id": &*session_id, "message": message }),
            );
            return;
        }
    };
    if let TurnTaskInput::Create {
        display_content, ..
    } = &input
    {
        agent.set_turn_persistence(display_content.clone(), attachment_run_id);
    }
    // The daemon-wide context snapshot tracks the *current* session; a turn
    // for another session must not overwrite it.
    let updates_context = || *base_store.session_id() == *session_id;
    let agent = &mut agent;
    let (redo_input_id, redo_display_content) = match &input {
        TurnTaskInput::Redo { candidate, prompts } => (
            Some(candidate.input_id.clone()),
            prompts.last().map(|prompt| prompt.display_content.clone()),
        ),
        TurnTaskInput::Create { .. } => (None, None),
    };

    let mapper = Arc::new(Mutex::new(RunEventMapper::new(
        run_id.to_string(),
        events.clone(),
        questions.clone(),
        store.clone(),
        manager.clone(),
        profile
            .as_ref()
            .and_then(|profile| profile.followup.as_ref())
            .map(|followup| followup.ingress()),
        operation,
        redo_input_id,
        redo_display_content,
        config.display.command_output_lines,
    )));
    let chat_outcome = match input {
        TurnTaskInput::Create {
            content, images, ..
        } => {
            let callback_mapper = mapper.clone();
            let images = into_pasted_images(images);
            let chat = agent.chat_stream_with_control(&content, &images, &control, move |event| {
                callback_mapper.lock().unwrap().handle(event);
                Ok(())
            });
            tokio::pin!(chat);
            loop {
                tokio::select! {
                    biased;
                    result = &mut chat => break TurnOutcome::Finished(result),
                    changed = cancel.changed() => {
                        if changed.is_err() || *cancel.borrow() {
                            questions.cancel_run(run_id);
                            break TurnOutcome::Cancelled;
                        }
                    }
                }
            }
        }
        TurnTaskInput::Redo { candidate, prompts } => {
            let callback_mapper = mapper.clone();
            let prompts = prompts
                .into_iter()
                .map(|prompt| crate::agent::RedoPromptInput {
                    prompt_id: prompt.prompt_id,
                    content: prompt.content,
                    display_content: prompt.display_content,
                    images: into_pasted_images(prompt.images),
                })
                .collect();
            let chat =
                agent.redo_stream_with_control(&candidate, prompts, &control, move |event| {
                    callback_mapper.lock().unwrap().handle(event);
                    Ok(())
                });
            tokio::pin!(chat);
            loop {
                tokio::select! {
                    biased;
                    result = &mut chat => break TurnOutcome::Finished(result),
                    changed = cancel.changed() => {
                        if changed.is_err() || *cancel.borrow() {
                            questions.cancel_run(run_id);
                            break TurnOutcome::Cancelled;
                        }
                    }
                }
            }
        }
    };

    let result = match chat_outcome {
        TurnOutcome::Cancelled => {
            drop_cancelled_queue(&store, events, run_id, &session_id);
            finish_cancelled_run(
                manager,
                events,
                agent,
                run_id,
                &session_id,
                updates_context(),
            );
            finish_turn_task(&config, &paths, &store, &title_seed, events, false);
            return;
        }
        TurnOutcome::Finished(Err(error)) if question::is_question_cancelled(&error) => {
            questions.cancel_run(run_id);
            drop_cancelled_queue(&store, events, run_id, &session_id);
            finish_cancelled_run(
                manager,
                events,
                agent,
                run_id,
                &session_id,
                updates_context(),
            );
            finish_turn_task(&config, &paths, &store, &title_seed, events, false);
            return;
        }
        TurnOutcome::Finished(Err(error)) => {
            finish_failed_run(
                manager,
                events,
                questions,
                agent,
                run_id,
                &session_id,
                updates_context(),
                &error,
            );
            finish_turn_task(&config, &paths, &store, &title_seed, events, false);
            return;
        }
        TurnOutcome::Finished(Ok(result)) => result,
    };

    questions.cancel_run(run_id);
    let context_tokens = match agent.effective_context_tokens() {
        Ok(tokens) => tokens,
        Err(error) => {
            finish_completed_with_context_error(
                manager,
                events,
                agent,
                run_id,
                &session_id,
                updates_context(),
                &result,
                &error,
            );
            finish_turn_task(&config, &paths, &store, &title_seed, events, true);
            return;
        }
    };
    let overflow_outcome = {
        let callback_mapper = mapper;
        let overflow = agent.handle_overflow_after_turn(context_tokens, move |event| {
            callback_mapper.lock().unwrap().handle(event);
            Ok(())
        });
        tokio::pin!(overflow);
        loop {
            tokio::select! {
                biased;
                result = &mut overflow => break OverflowOutcome::Finished(result),
                changed = cancel.changed() => {
                    if changed.is_err() || *cancel.borrow() {
                        break OverflowOutcome::Cancelled;
                    }
                }
            }
        }
    };
    match overflow_outcome {
        OverflowOutcome::Cancelled => {
            drop_cancelled_queue(&store, events, run_id, &session_id);
            let context =
                current_context(agent).unwrap_or_else(|_| manager.lock().unwrap().context);
            finish_run(manager, run_id, updates_context().then_some(context));
            publish_completed(events, run_id, &session_id, &result, context);
            finish_turn_task(&config, &paths, &store, &title_seed, events, true);
            return;
        }
        OverflowOutcome::Finished(Err(error)) => {
            finish_completed_with_context_error(
                manager,
                events,
                agent,
                run_id,
                &session_id,
                updates_context(),
                &result,
                &error,
            );
            finish_turn_task(&config, &paths, &store, &title_seed, events, true);
            return;
        }
        OverflowOutcome::Finished(Ok(_)) => {}
    }
    let context = match current_context(agent) {
        Ok(context) => context,
        Err(error) => {
            finish_completed_with_context_error(
                manager,
                events,
                agent,
                run_id,
                &session_id,
                updates_context(),
                &result,
                &error,
            );
            finish_turn_task(&config, &paths, &store, &title_seed, events, true);
            return;
        }
    };
    finish_run(manager, run_id, updates_context().then_some(context));
    publish_completed(events, run_id, &session_id, &result, context);
    finish_turn_task(&config, &paths, &store, &title_seed, events, true);
}

/// Shared per-turn cleanup: auto-naming, activity timestamp, queue-identity
/// cleanup, and allocator trimming. `store` is the turn's pinned store, so
/// session-scoped operations hit the turn's own session.
fn finish_turn_task(
    config: &AppConfig,
    paths: &MiyuPaths,
    store: &StateStore,
    title_seed: &str,
    events: &EventHub,
    completed: bool,
) {
    if completed {
        if let Some(fallback) = maybe_auto_name_session(store, events, title_seed) {
            spawn_session_title_refinement(config, paths, store, events, fallback, title_seed);
        }
        let _ = store.touch_session(&store.session_id());
    }
    let _ = store.discard_queued_prompts();
    trim_process_memory();
}

/// Best-effort AI pass over the truncated default session name: ask the
/// main model pool for a concise title and apply it only if the
/// auto-generated name is still in place (a user rename wins). Runs
/// detached on the actor's LocalSet — never blocks the turn.
fn spawn_session_title_refinement(
    config: &AppConfig,
    paths: &MiyuPaths,
    store: &StateStore,
    events: &EventHub,
    fallback: String,
    seed: &str,
) {
    let Ok(client) = OpenAiCompatibleClient::from_config(config, paths) else {
        return;
    };
    let store = store.clone();
    let events = events.clone();
    let seed = seed.to_string();
    tokio::task::spawn_local(async move {
        let session_id = store.session_id();
        let prompt = format!(
            "为下面这条用户消息生成一个简洁的会话标题。要求：不超过 16 个字，             概括主题，只输出标题本身，不要引号、句号或任何解释。

用户消息：{seed}"
        );
        let result = client
            .chat_stream(
                vec![
                    crate::llm::ChatMessage::system("你是会话标题生成器，只输出标题本身。"),
                    crate::llm::ChatMessage::plain("user", prompt),
                ],
                Vec::new(),
                |_| Ok(()),
            )
            .await;
        let Ok(result) = result else { return };
        let title = sanitize_session_title(&result.content);
        if title.is_empty() {
            return;
        }
        let Ok(Some(record)) = store.session_record(&session_id) else {
            return;
        };
        if record.name != fallback {
            return;
        }
        if store.rename_session(&record.session_id, &title).is_ok() {
            events.publish(
                "session.renamed",
                json!({ "session_id": record.session_id, "name": title }),
            );
        }
        if let Some(usage) = result.usage.as_ref() {
            let _ = store.add_auxiliary_usage(usage);
        }
    });
}

/// Cleans an LLM-generated title down to a single short line: first line
/// only, surrounding quotes/punctuation stripped, clipped to 20 chars.
fn sanitize_session_title(raw: &str) -> String {
    let cleaned = raw
        .trim()
        .lines()
        .next()
        .unwrap_or("")
        .trim()
        .trim_matches(|c: char| {
            matches!(
                c,
                '"' | '\''
                    | '“'
                    | '”'
                    | '‘'
                    | '’'
                    | '「'
                    | '」'
                    | '《'
                    | '》'
                    | '。'
                    | '.'
                    | '，'
                    | ','
            )
        })
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    cleaned.chars().take(20).collect()
}

pub(crate) enum TurnOutcome {
    Finished(Result<ChatResult>),
    Cancelled,
}

enum OverflowOutcome {
    Finished(Result<Option<ChatResult>>),
    Cancelled,
}

fn active_thinking_variant_options(
    config: &AppConfig,
    paths: &MiyuPaths,
) -> Result<Vec<ThinkingVariantOptions>> {
    crate::models_cache::ensure_active_metadata(paths, config);
    let preferences = ThinkingVariantPreferences::load(paths);
    config
        .active_provider_model_choices()
        .into_iter()
        .map(|choice| {
            let provider = config.provider(Some(&choice.provider_id))?;
            Ok(thinking_variant_options_for_model(
                provider,
                &choice.model,
                preferences.selected(&choice.provider_id, &choice.model),
            ))
        })
        .collect()
}

fn apply_thinking_variant_updates(
    agent: &mut Option<Agent>,
    config: &AppConfig,
    paths: &MiyuPaths,
    updates: &[ThinkingVariantUpdate],
) -> std::result::Result<(), AdminFailure> {
    let options = active_thinking_variant_options(config, paths)
        .map_err(|error| AdminFailure::Internal(safe_error_message(error)))?;
    for update in updates {
        let option = options
            .iter()
            .find(|option| option.provider_id == update.provider_id && option.model == update.model)
            .ok_or_else(|| {
                AdminFailure::Invalid(format!(
                    "inactive model: {} / {}",
                    update.provider_id, update.model
                ))
            })?;
        if let Some(selected) = &update.selected {
            if !option.variants.iter().any(|variant| variant == selected) {
                return Err(AdminFailure::Invalid(format!(
                    "thinking variant is unavailable for {} / {}: {}",
                    update.provider_id, update.model, selected
                )));
            }
        }
    }

    let selections = updates
        .iter()
        .map(|update| {
            (
                update.provider_id.clone(),
                update.model.clone(),
                update.selected.clone(),
            )
        })
        .collect::<Vec<_>>();
    let next_client = agent
        .as_ref()
        .map(|current| {
            let mut client = current.cloned_client();
            client
                .set_thinking_variants(&selections)
                .map_err(|error| AdminFailure::Invalid(safe_error_message(error)))?;
            Ok(client)
        })
        .transpose()?;

    let mut preferences = ThinkingVariantPreferences::load(paths);
    for update in updates {
        preferences.set(&update.provider_id, &update.model, update.selected.clone());
    }
    preferences
        .save(paths)
        .map_err(|error| AdminFailure::Internal(safe_error_message(error)))?;

    if let (Some(agent), Some(client)) = (agent.as_mut(), next_client) {
        agent.replace_client(client);
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn rebuild_for_models(
    agent: &mut Option<Agent>,
    config: &mut AppConfig,
    paths: &MiyuPaths,
    state_store: &StateStore,
    manager: &Arc<Mutex<ManagerState>>,
    models: &[ActiveProviderModelConfig],
) -> std::result::Result<(), AdminFailure> {
    let mut next_config = config.clone();
    next_config
        .set_active_provider_models(models)
        .map_err(|error| AdminFailure::Invalid(safe_error_message(&error)))?;
    if next_config.active_provider_models == config.active_provider_models {
        return Ok(());
    }
    let next_agent = if agent.is_some() {
        crate::models_cache::ensure_active_metadata(paths, &next_config);
        let client = OpenAiCompatibleClient::from_config(&next_config, paths)
            .map_err(|error| AdminFailure::Internal(safe_error_message(&error)))?;
        let registry = build_tool_registry(&next_config, paths, AgentMode::Normal, true)
            .map_err(|error| AdminFailure::Internal(safe_error_message(&error)))?;
        Some(
            Agent::new(
                next_config.clone(),
                paths,
                state_store.clone(),
                client,
                registry,
                AgentMode::Normal,
            )
            .map_err(|error| AdminFailure::Internal(safe_error_message(&error)))?,
        )
    } else {
        None
    };
    let context = next_agent
        .as_ref()
        .map_or_else(|| cold_context(&next_config, state_store), current_context)
        .map_err(|error| AdminFailure::Internal(safe_error_message(&error)))?;
    next_config
        .save(paths)
        .map_err(|error| AdminFailure::Internal(safe_error_message(&error)))?;
    *agent = next_agent;
    *config = next_config.clone();
    let mut manager = manager.lock().unwrap();
    manager.config = next_config;
    manager.context = context;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn session_for_persona(
    state_store: &StateStore,
    manager: &Arc<Mutex<ManagerState>>,
    persona: &str,
) -> Result<String> {
    if let Some(session_id) = state_store.persona_current_session(persona)? {
        if is_available_local_session(state_store, &session_id, persona)? {
            return Ok(session_id);
        }
    }
    let remembered = manager
        .lock()
        .unwrap()
        .persona_session_ids
        .get(persona)
        .cloned();
    if let Some(session_id) = remembered {
        if is_available_local_session(state_store, &session_id, persona)? {
            return Ok(session_id);
        }
    }
    if let Some(overview) = state_store
        .list_local_sessions(persona, false)?
        .into_iter()
        .next()
    {
        return Ok(overview.record.session_id);
    }
    Ok(state_store
        .create_session(persona, "", "user", None)?
        .session_id)
}

#[allow(clippy::too_many_arguments)]
fn rebuild_for_config(
    agent: &mut Option<Agent>,
    config: &mut AppConfig,
    paths: &MiyuPaths,
    state_store: &StateStore,
    manager: &Arc<Mutex<ManagerState>>,
    events: &EventHub,
    next_config: AppConfig,
    prompts: &PromptDocuments,
    reset_conversation: bool,
) -> std::result::Result<(), AdminFailure> {
    let _ = reset_conversation;
    let mut next_config = next_config;
    // Models removed from the text models must leave the tier pools too.
    next_config.prune_subagent_tiers();
    let previous_prompts = read_prompt_documents(config, paths)
        .map_err(|error| AdminFailure::Internal(safe_error_message(error)))?;
    let persona_changes = persona_document_changes(&previous_prompts, prompts);
    let mut persona_db_guard = PersonaDbRenameGuard::new(state_store.clone(), &persona_changes)
        .map_err(|error| AdminFailure::Internal(safe_error_message(error)))?;
    let previous_scope = config.active_persona_scope();
    let next_scope = next_config.active_persona_scope();
    let migrated_previous_scope = persona_changes
        .iter()
        .find_map(|(old_name, new_name)| {
            (crate::config::persona_scope_name(old_name) == previous_scope)
                .then(|| new_name.as_deref().map(crate::config::persona_scope_name))
                .flatten()
        })
        .unwrap_or_else(|| previous_scope.clone());
    let persona_changed = migrated_previous_scope != next_scope;
    let previous_session_id = state_store.session_id().to_string();
    let target_session_id = if persona_changed {
        session_for_persona(state_store, manager, &next_scope)
            .map_err(|error| AdminFailure::Internal(safe_error_message(error)))?
    } else {
        previous_session_id.clone()
    };
    if persona_changed {
        state_store
            .set_persona_current_session(&migrated_previous_scope, &previous_session_id)
            .map_err(|error| AdminFailure::Internal(safe_error_message(error)))?;
    }
    let target_state_store = if persona_changed {
        state_store.pinned(&target_session_id)
    } else {
        state_store.clone()
    };
    let prompt_backups =
        apply_prompt_documents(config, &next_config, &previous_prompts, prompts, paths)
            .map_err(|error| AdminFailure::Internal(safe_error_message(error)))?;
    let scope_backups = match apply_persona_scope_changes(
        config,
        &next_config,
        &previous_prompts,
        prompts,
        paths,
    ) {
        Ok(backups) => backups,
        Err(error) => {
            restore_file_backups(&prompt_backups);
            return Err(AdminFailure::Internal(safe_error_message(error)));
        }
    };
    let config_backup = FileBackup {
        path: paths.config_file.clone(),
        content: std::fs::read(&paths.config_file).ok(),
    };
    let system_prompt_backup = next_config.system_prompt.as_ref().map(|_| FileBackup {
        path: next_config.system_prompt_path(paths),
        content: std::fs::read(next_config.system_prompt_path(paths)).ok(),
    });

    let build_agent = || -> Result<Agent> {
        crate::models_cache::ensure_active_metadata(paths, &next_config);
        let client = OpenAiCompatibleClient::from_config(&next_config, paths)?;
        let registry = build_tool_registry(&next_config, paths, AgentMode::Normal, true)?;
        Agent::new(
            next_config.clone(),
            paths,
            target_state_store.clone(),
            client,
            registry,
            AgentMode::Normal,
        )
    };
    let next_agent = if agent.is_some() {
        match build_agent() {
            Ok(agent) => Some(agent),
            Err(error) => {
                restore_file_backups(&prompt_backups);
                restore_persona_scope_backups(&scope_backups);
                return Err(AdminFailure::Invalid(safe_error_message(error)));
            }
        }
    } else {
        None
    };
    let context = match next_agent.as_ref().map_or_else(
        || cold_context(&next_config, &target_state_store),
        current_context,
    ) {
        Ok(context) => context,
        Err(error) => {
            restore_file_backups(&prompt_backups);
            restore_persona_scope_backups(&scope_backups);
            return Err(AdminFailure::Invalid(safe_error_message(error)));
        }
    };
    if let Err(error) = next_config.save(paths) {
        restore_file_backups(&prompt_backups);
        restore_persona_scope_backups(&scope_backups);
        restore_file_backups(std::slice::from_ref(&config_backup));
        if let Some(backup) = &system_prompt_backup {
            restore_file_backups(std::slice::from_ref(backup));
        }
        return Err(AdminFailure::Internal(safe_error_message(error)));
    }

    if persona_changed {
        if let Err(error) = state_store.switch_session(&target_session_id) {
            restore_file_backups(&prompt_backups);
            restore_persona_scope_backups(&scope_backups);
            restore_file_backups(std::slice::from_ref(&config_backup));
            if let Some(backup) = &system_prompt_backup {
                restore_file_backups(std::slice::from_ref(backup));
            }
            return Err(AdminFailure::Internal(safe_error_message(error)));
        }
        if let Err(error) = state_store.set_persona_current_session(&next_scope, &target_session_id)
        {
            let _ = state_store.switch_session(&previous_session_id);
            restore_file_backups(&prompt_backups);
            restore_persona_scope_backups(&scope_backups);
            restore_file_backups(std::slice::from_ref(&config_backup));
            if let Some(backup) = &system_prompt_backup {
                restore_file_backups(std::slice::from_ref(backup));
            }
            return Err(AdminFailure::Internal(safe_error_message(error)));
        }
    }

    *agent = next_agent;
    *config = next_config.clone();
    let mut manager = manager.lock().unwrap();
    let migrated_session_ids = persona_changes
        .iter()
        .filter_map(|(old_name, new_name)| {
            let old_scope = crate::config::persona_scope_name(old_name);
            let new_scope = new_name.as_deref().map(crate::config::persona_scope_name)?;
            manager
                .persona_session_ids
                .remove(&old_scope)
                .map(|session_id| (new_scope, session_id))
        })
        .collect::<Vec<_>>();
    manager.persona_session_ids.extend(migrated_session_ids);
    if persona_changed {
        manager
            .persona_session_ids
            .insert(migrated_previous_scope, previous_session_id);
        manager
            .persona_session_ids
            .insert(next_scope, target_session_id.clone());
    }
    manager.config = next_config;
    manager.context = context;
    drop(manager);
    if persona_changed {
        events.publish(
            "session.current_changed",
            json!({ "session_id": target_session_id }),
        );
    }
    persona_db_guard.commit();
    finalize_persona_scope_backups(&scope_backups);
    for (old_name, new_name) in &persona_changes {
        if new_name.is_none() {
            if let Err(error) =
                state_store.delete_persona_scope(&crate::config::persona_scope_name(old_name))
            {
                tracing::warn!(
                    %error,
                    %old_name,
                    "{}",
                    t(
                        "deleted persona state cleanup failed",
                        "已删除角色的状态清理失败"
                    )
                );
            }
        }
    }
    Ok(())
}

/// Auto-names a still-unnamed session from its first prompt once a turn has
/// run in it. Explicit names (given at creation or via rename) are never
/// overwritten.
fn maybe_auto_name_session(
    state_store: &StateStore,
    events: &EventHub,
    seed: &str,
) -> Option<String> {
    let session_id = state_store.session_id();
    let record = state_store.session_record(&session_id).ok().flatten()?;
    if !record.name.trim().is_empty() {
        return None;
    }
    let title = session_title_from_prompt(seed);
    if title.is_empty() {
        return None;
    }
    if state_store
        .rename_session(&record.session_id, &title)
        .is_ok()
    {
        events.publish(
            "session.renamed",
            json!({ "session_id": record.session_id, "name": title }),
        );
        return Some(title);
    }
    None
}

fn session_title_from_prompt(prompt: &str) -> String {
    let cleaned = prompt
        .trim()
        .lines()
        .next()
        .unwrap_or("")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let mut title: String = cleaned.chars().take(20).collect();
    if cleaned.chars().count() > 20 {
        title.push('…');
    }
    title
}

fn switch_actor_session(
    agent: Option<&Agent>,
    config: &AppConfig,
    state_store: &StateStore,
    manager: &Arc<Mutex<ManagerState>>,
    events: &EventHub,
    session_id: &str,
) -> std::result::Result<(), AdminFailure> {
    // Notes: switching deliberately does not touch updated_at (viewing must
    // not reorder the session list), and runs no turn-entry maintenance —
    // switching is allowed while turns are running, so a prompt-change reset
    // here could wipe a session mid-turn.
    let switch = || -> Result<ContextSnapshot> {
        state_store.switch_session(session_id)?;
        agent.map_or_else(|| cold_context(config, state_store), current_context)
    };
    let context = switch().map_err(|error| AdminFailure::Internal(safe_error_message(&error)))?;
    let mut manager_state = manager.lock().unwrap();
    manager_state.context = context;
    let persona_scope = manager_state.config.active_persona_scope();
    manager_state
        .persona_session_ids
        .insert(persona_scope.clone(), session_id.to_string());
    drop(manager_state);
    state_store
        .set_persona_current_session(&persona_scope, session_id)
        .map_err(|error| AdminFailure::Internal(safe_error_message(error)))?;
    events.publish(
        "session.current_changed",
        json!({ "session_id": session_id }),
    );
    Ok(())
}

fn reset_actor_conversation(
    agent: &mut Option<Agent>,
    config: &AppConfig,
    paths: &MiyuPaths,
    state_store: &StateStore,
    manager: &Arc<Mutex<ManagerState>>,
    events: &EventHub,
    session_id: &str,
) -> std::result::Result<(), AdminFailure> {
    // "Reset" means the conversation starts over, so everything scoped to it
    // goes: history, per-session usage, and the recall caches that only make
    // sense against that history. This used to be gated on a flag that was
    // really asking "did the caller address the session as `Current`?" — an
    // implementation detail of each frontend, which left `/reset` and the
    // WebUI clearing strictly less than `miyu reset`. Platform sessions never
    // reach this command (both entry points reject them) and clear themselves
    // through `ClearSessionContent`, so there is nothing left for a flag to
    // protect.
    let mut reset = || -> Result<Option<ContextSnapshot>> {
        let store = state_store.pinned(session_id);
        store.clear_session_content()?;
        store.reset_conversation_usage()?;
        let memory = MemoryStore::new(config, paths);
        memory.clear_evicted_context()?;
        memory.clear_pending_events()?;
        tools::clear_aur_review_state(paths)?;
        if &*state_store.session_id() == session_id {
            if let Some(agent) = agent.as_mut() {
                agent.reset_memory()?;
                agent.prepare_for_turn()?;
                current_context(agent).map(Some)
            } else {
                cold_context(config, &store).map(Some)
            }
        } else {
            Ok(None)
        }
    };
    let context = reset().map_err(|error| AdminFailure::Internal(safe_error_message(&error)))?;
    if let Some(context) = context {
        manager.lock().unwrap().context = context;
    }
    events.publish("conversation.reset", json!({ "session_id": session_id }));
    Ok(())
}

fn reset_actor_persona_state(
    agent: &mut Option<Agent>,
    daemon_config: &AppConfig,
    reset_config: &AppConfig,
    paths: &MiyuPaths,
    state_store: &StateStore,
    manager: &Arc<Mutex<ManagerState>>,
    events: &EventHub,
) -> std::result::Result<(), AdminFailure> {
    let mut reset = || -> Result<ContextSnapshot> {
        let persona = reset_config.active_persona_scope();
        state_store.reset_persona_contexts(&persona, "onebot")?;
        MemoryStore::new(reset_config, paths).reset_all(true)?;
        if persona != daemon_config.active_persona_scope() {
            return Ok(manager.lock().unwrap().context);
        }
        tools::clear_aur_review_state(paths)?;
        state_store.reset_conversation_usage()?;
        if let Some(agent) = agent.as_mut() {
            agent.reset_memory()?;
            agent.prepare_for_turn()?;
            current_context(agent)
        } else {
            cold_context(daemon_config, state_store)
        }
    };
    let context = reset().map_err(|error| AdminFailure::Internal(safe_error_message(&error)))?;
    manager.lock().unwrap().context = context;
    events.publish(
        "conversation.reset",
        json!({ "scope": "persona", "persona": reset_config.active_persona_scope() }),
    );
    Ok(())
}

fn clear_actor_session_content(
    agent: &mut Option<Agent>,
    config: &AppConfig,
    state_store: &StateStore,
    manager: &Arc<Mutex<ManagerState>>,
    session_id: &str,
) -> std::result::Result<(), AdminFailure> {
    let store = state_store.pinned(session_id);
    store
        .clear_session_content()
        .map_err(|error| AdminFailure::Internal(safe_error_message(error)))?;

    // Platform sessions normally never become the daemon's current local
    // session. Keep the in-memory context coherent if a legacy binding points
    // at that session, without clearing persona-wide memory or usage totals.
    if &*state_store.session_id() == session_id {
        let context = if let Some(agent) = agent.as_mut() {
            agent
                .reset_memory()
                .and_then(|()| agent.prepare_for_turn())
                .and_then(|()| current_context(agent))
        } else {
            cold_context(config, &store)
        }
        .map_err(|error| AdminFailure::Internal(safe_error_message(error)))?;
        manager.lock().unwrap().context = context;
    }
    Ok(())
}

/// Background-job completions wake the model so it can follow up on the
/// result autonomously. Local sessions get a real turn (or a queued
/// followup when the session is mid-turn); platform-bound sessions get a
/// plain-text broadcast into the conversation — a self-initiated platform
/// turn would need synthetic sender semantics the plugins aren't built for.
fn install_background_job_hook(state: &DaemonState) {
    let started_state = state.clone();
    tools::jobs::set_started_hook(Arc::new(move |overview| {
        started_state
            .events
            .publish("job.started", json!({ "job": overview }));
    }));
    let hook_state = state.clone();
    tools::jobs::set_completion_hook(Arc::new(move |completion| {
        let state = hook_state.clone();
        tokio::spawn(async move {
            handle_job_completion(state, completion).await;
        });
    }));
}

async fn handle_job_completion(state: DaemonState, completion: tools::jobs::JobCompletion) {
    state.events.publish(
        "job.finished",
        json!({
            "job_id": completion.job_id,
            "title": completion.title,
            "status": completion.state_label,
            "runtime_seconds": completion.runtime_seconds,
        }),
    );
    if !completion.wake_requested {
        // The model stopped this command itself; clean the strips quietly.
        tools::jobs::acknowledge(&completion.job_id);
        state
            .events
            .publish("job.acknowledged", json!({ "job_id": completion.job_id }));
        return;
    }
    let command_short = completion.command.chars().take(120).collect::<String>();
    let mut pending_wake_run: Option<String> = None;
    if let Some(session_id) = completion.session_id.clone() {
        match state.state_store.is_platform_session(&session_id) {
            Ok(true) => {
                wake_platform_session_for_job(&state, &session_id, &completion).await;
            }
            Ok(false) => {
                pending_wake_run =
                    wake_local_session_for_job(&state, session_id, &completion, &command_short);
            }
            Err(error) => {
                tracing::warn!(
                    job_id = %completion.job_id,
                    error = %error,
                    "failed to resolve the session of a finished background command"
                );
            }
        }
    }
    // Keep the finished job visible in UI strips until its wake turn is done
    // (the report is what replaces the strip line); everything else clears
    // right away.
    if let Some(run_id) = pending_wake_run {
        let deadline = std::time::Instant::now() + Duration::from_secs(600);
        while std::time::Instant::now() < deadline {
            let still_running = state
                .manager
                .lock()
                .unwrap()
                .active_runs
                .contains_key(&run_id);
            if !still_running {
                break;
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }
    tools::jobs::acknowledge(&completion.job_id);
    state
        .events
        .publish("job.acknowledged", json!({ "job_id": completion.job_id }));
}

fn wake_local_session_for_job(
    state: &DaemonState,
    session_id: Arc<str>,
    completion: &tools::jobs::JobCompletion,
    command_short: &str,
) -> Option<String> {
    let noun = if completion.is_subagent {
        "后台子代理"
    } else {
        "后台命令"
    };
    let action_hint = if completion.is_subagent {
        "先用 job_status 读取日志（结尾的「子代理结果」段是最终结论，必要时用 offset 分页），然后向用户简要汇报；如果失败，指出原因并给出建议。"
    } else {
        "先用 job_status 读取输出（必要时用 offset 分页），然后向用户简要汇报结果；如果命令失败，指出原因并给出建议。"
    };
    let content = format!(
        "<background-job-report>{noun}「{}」已执行完毕，请自主跟进：\n\
         - job_id: {}\n- 任务: {}\n- 状态: {}（运行 {} 秒）\n\
         {action_hint}这是系统自动触发的跟进，不是用户消息。\
         </background-job-report>",
        completion.title,
        completion.job_id,
        command_short,
        completion.state_label,
        completion.runtime_seconds
    );
    let display_content = format!(
        "[后台任务完成] {}完成 {} · {}",
        if completion.is_subagent { "子代理" } else { "命令" },
        completion.job_id,
        completion.title
    );

    // Mid-turn session: ride the queue so the model reacts within the
    // running reply instead of colliding with it.
    let queued = {
        let manager = state.manager.lock().unwrap();
        manager
            .active_runs
            .iter()
            .find(|(_, info)| &*info.session_id == &*session_id)
            .map(|(run_id, info)| {
                (
                    run_id.clone(),
                    info.queue_target.clone(),
                    info.audience,
                )
            })
    };
    if let Some((run_id, queue_target, audience)) = queued {
        let Some(target) = queue_target else {
            // Turn is still starting; report on the next completion poll
            // rather than racing its queue setup.
            tracing::debug!(job_id = %completion.job_id, "job wake skipped: turn starting");
            return None;
        };
        let request = TurnUpdateRequest {
            run_id,
            turn_id: target.turn_id,
            session_id: Some(session_id.clone()),
            audience,
            content,
            display_content,
            attachments: Vec::new(),
            uploaded_attachment_ids: Vec::new(),
            mode: TurnUpdateMode::Followup,
        };
        if let Err(error) = enqueue_turn_update(state, request) {
            tracing::debug!(
                job_id = %completion.job_id,
                error = %error,
                "job wake could not join the running turn"
            );
        }
        return None;
    }

    let run_id = random_id("run", 18);
    let (cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
    {
        let mut manager = state.manager.lock().unwrap();
        if manager.admin_busy {
            tracing::debug!(job_id = %completion.job_id, "job wake skipped: admin busy");
            return None;
        }
        manager.active_runs.insert(
            run_id.clone(),
            RunInfo {
                session_id: session_id.clone(),
                mode: AgentMode::Normal,
                audience: PromptAudience::Owner,
                cancel: cancel_tx,
                turn_id: None,
                queue_target: None,
                supersede: Arc::new(crate::agent::TurnSupersedeSignal::default()),
                platform_followup: None,
                operation: RunOperation::Create,
                job_wake: true,
                job_wake_label: Some(format!(
                    "{}完成 {} · {}",
                    if completion.is_subagent { "子代理" } else { "命令" },
                    completion.job_id,
                    completion.title
                )),
            },
        );
    }
    if state
        .actor_tx
        .send(ActorCommand::StartTurn {
            run_id: run_id.clone(),
            session_id,
            content,
            display_content,
            attachment_run_id: None,
            mode: AgentMode::Normal,
            images: Vec::new(),
            cwd: Some(completion.workspace.clone()),
            audience: PromptAudience::Owner,
            profile: None,
            cancel: cancel_rx,
        })
        .is_err()
    {
        finish_run(&state.manager, &run_id, None);
        return None;
    }
    Some(run_id)
}

async fn wake_platform_session_for_job(
    state: &DaemonState,
    session_id: &Arc<str>,
    completion: &tools::jobs::JobCompletion,
) {
    let persona = state
        .manager
        .lock()
        .unwrap()
        .config
        .active_persona_scope();
    let binding = state
        .state_store
        .platform_session_bindings(&persona, "onebot")
        .ok()
        .and_then(|bindings| {
            bindings
                .into_iter()
                .find(|binding| binding.session_id == **session_id)
        });
    let Some(binding) = binding else {
        tracing::debug!(job_id = %completion.job_id, "job wake skipped: no platform binding");
        return;
    };
    let noun = if completion.is_subagent {
        "后台子代理"
    } else {
        "后台命令"
    };
    let content = format!(
        "<background-job-report>{noun}「{}」已执行完毕：\n- job_id: {}\n- 任务: {}\n- 状态: {}（运行 {} 秒）\n\
         请用 job_status 查看输出，并把结果自然地发到会话里。这是系统自动触发的跟进，不是用户消息。\
         </background-job-report>",
        completion.title,
        completion.job_id,
        completion.command.chars().take(200).collect::<String>(),
        completion.state_label,
        completion.runtime_seconds
    );
    if let Err(error) = crate::platforms::onebot::wake_conversation_for_job(
        state,
        &binding.key.account_id,
        &binding.key.conversation_kind,
        &binding.key.conversation_id,
        content,
    )
    .await
    {
        tracing::warn!(
            job_id = %completion.job_id,
            error = %error,
            "failed to wake the model for a background command in QQ"
        );
    }
}

/// An explicit cancel withdraws the follow-ups still queued behind the
/// reply: the user aborted the exchange, so folding them into context would
/// keep answering messages they no longer want processed. Published before
/// `run.cancelled` so clients still draining the event stream can clear
/// their queue bubbles.
fn drop_cancelled_queue(store: &StateStore, events: &EventHub, run_id: &str, session_id: &str) {
    match store.delete_queued_prompts() {
        Ok(prompt_ids) => {
            for prompt_id in prompt_ids {
                events.publish(
                    "queue.removed",
                    json!({
                        "session_id": session_id,
                        "run_id": run_id,
                        "prompt_id": prompt_id,
                    }),
                );
            }
        }
        Err(error) => {
            tracing::warn!(
                run_id,
                error = %error,
                "{}",
                t(
                    "failed to drop queued prompts for a cancelled turn",
                    "无法丢弃已取消回复的排队消息"
                )
            );
        }
    }
}

fn finish_cancelled_run(
    manager: &Arc<Mutex<ManagerState>>,
    events: &EventHub,
    agent: &Agent,
    run_id: &str,
    session_id: &str,
    updates_context: bool,
) {
    let context = current_context(agent).ok().filter(|_| updates_context);
    let mut payload = json!({ "run_id": run_id, "session_id": session_id });
    if let Some(context) = &context {
        // The interrupted turn is persisted into the context; keep the client
        // context meters honest instead of leaving them at the pre-turn value.
        payload["context_tokens"] = json!(context.tokens);
        payload["context_window"] = json!(context.window);
        payload["cumulative_tokens"] = json!(context.cumulative_tokens);
        payload["cumulative_prompt_tokens"] = json!(context.cumulative_prompt_tokens);
        payload["cumulative_cache_read_tokens"] = json!(context.cumulative_cache_read_tokens);
    }
    finish_run(manager, run_id, context);
    events.publish("run.cancelled", payload);
}

#[allow(clippy::too_many_arguments)]
fn finish_failed_run(
    manager: &Arc<Mutex<ManagerState>>,
    events: &EventHub,
    questions: &QuestionBroker,
    agent: &Agent,
    run_id: &str,
    session_id: &str,
    updates_context: bool,
    error: &anyhow::Error,
) {
    questions.cancel_run(run_id);
    let context = current_context(agent).ok().filter(|_| updates_context);
    finish_run(manager, run_id, context);
    let message = safe_error_message(error);
    tracing::error!(
        run_id,
        error = %error,
        "{}",
        t("WebUI agent run failed", "WebUI 智能体运行失败")
    );
    events.publish(
        "run.failed",
        json!({ "run_id": run_id, "session_id": session_id, "message": message }),
    );
}

#[allow(clippy::too_many_arguments)]
fn finish_completed_with_context_error(
    manager: &Arc<Mutex<ManagerState>>,
    events: &EventHub,
    agent: &Agent,
    run_id: &str,
    session_id: &str,
    updates_context: bool,
    result: &ChatResult,
    error: &anyhow::Error,
) {
    let message = safe_error_message(error);
    tracing::error!(
        run_id,
        error = %error,
        "{}",
        t(
            "WebUI post-turn context maintenance failed",
            "WebUI 回合后上下文维护失败"
        )
    );
    events.publish(
        "context.error",
        json!({ "run_id": run_id, "session_id": session_id, "message": message }),
    );
    let context = current_context(agent).unwrap_or_else(|_| manager.lock().unwrap().context);
    finish_run(manager, run_id, updates_context.then_some(context));
    publish_completed(events, run_id, session_id, result, context);
}

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
    }
}

fn publish_completed(
    events: &EventHub,
    run_id: &str,
    session_id: &str,
    result: &ChatResult,
    context: ContextSnapshot,
) {
    // Always the local estimate of the persisted context: provider-reported
    // request usage measures what this turn consumed, not what the context
    // holds now — the two diverge after post-turn compaction/pruning, and
    // the footer meter must refresh with those rewrites.
    let context_tokens = context.tokens;
    events.publish(
        "run.completed",
        json!({
            "run_id": run_id,
            "session_id": session_id,
            "usage": result.usage,
            "usage_estimated": result.usage_estimated,
            "provider_id": result.provider_id,
            "model": result.model,
            "context_tokens": context_tokens,
            "context_window": context.window,
            "cumulative_tokens": context.cumulative_tokens,
            "cumulative_prompt_tokens": context.cumulative_prompt_tokens,
            "cumulative_cache_read_tokens": context.cumulative_cache_read_tokens,
        }),
    );
}

fn current_context(agent: &Agent) -> Result<ContextSnapshot> {
    let cumulative = agent.conversation_usage_token_totals()?;
    Ok(ContextSnapshot {
        tokens: agent.effective_context_tokens()?,
        window: agent.context_window(),
        cumulative_tokens: cumulative.total,
        cumulative_prompt_tokens: cumulative.prompt,
        cumulative_cache_read_tokens: cumulative.cache_read,
    })
}

fn build_actor_agent(config: &AppConfig, paths: &MiyuPaths, state: &StateStore) -> Result<Agent> {
    let mut agent = build_session_agent(config, paths, state)?;
    agent.prepare_for_turn()?;
    Ok(agent)
}

fn build_session_agent(config: &AppConfig, paths: &MiyuPaths, state: &StateStore) -> Result<Agent> {
    crate::models_cache::ensure_active_metadata(paths, config);
    let client = OpenAiCompatibleClient::from_config(config, paths)?;
    let registry = build_tool_registry(config, paths, AgentMode::Normal, true)?;
    Agent::new(
        config.clone(),
        paths,
        state.clone(),
        client,
        registry,
        AgentMode::Normal,
    )
}

fn ensure_actor_agent<'a>(
    agent: &'a mut Option<Agent>,
    config: &AppConfig,
    paths: &MiyuPaths,
    state: &StateStore,
    turn_engine: &TurnEngineState,
) -> std::result::Result<&'a mut Agent, AdminFailure> {
    if agent.is_none() {
        turn_engine.set(TurnEngineState::INITIALIZING);
        match build_actor_agent(config, paths, state) {
            Ok(next) => {
                *agent = Some(next);
                turn_engine.set(TurnEngineState::READY);
            }
            Err(error) => {
                turn_engine.set(TurnEngineState::FAILED);
                return Err(AdminFailure::Internal(safe_error_message(error)));
            }
        }
    }
    Ok(agent.as_mut().expect("actor agent was initialized"))
}

fn actor_context(
    agent: &Option<Agent>,
    config: &AppConfig,
    state: &StateStore,
) -> Result<ContextSnapshot> {
    agent
        .as_ref()
        .map_or_else(|| cold_context(config, state), current_context)
}

fn cold_context(config: &AppConfig, state_store: &StateStore) -> Result<ContextSnapshot> {
    let cumulative = state_store.session_cumulative_token_totals()?;
    Ok(ContextSnapshot {
        tokens: 0,
        window: config.active_context_window()?,
        cumulative_tokens: cumulative.total,
        cumulative_prompt_tokens: cumulative.prompt,
        cumulative_cache_read_tokens: cumulative.cache_read,
    })
}

fn session_state(
    manager: &Arc<Mutex<ManagerState>>,
    state_store: &StateStore,
) -> Result<ipc::SessionState> {
    let context = manager.lock().unwrap().context;
    let session_id = state_store.session_id();
    let record = state_store.session_record(&session_id)?;
    Ok(ipc::SessionState {
        context_tokens: context.tokens,
        context_window: context.window,
        cumulative_tokens: context.cumulative_tokens,
        cumulative_prompt_tokens: context.cumulative_prompt_tokens,
        cumulative_cache_read_tokens: context.cumulative_cache_read_tokens,
        session_id: session_id.to_string(),
        session_name: record
            .as_ref()
            .map(|record| record.name.clone())
            .unwrap_or_default(),
        workspace: record.and_then(|record| record.workspace),
    })
}

fn session_state_for(state: &DaemonState, session_id: &str) -> Result<ipc::SessionState> {
    let record = state
        .state_store
        .session_record(session_id)?
        .with_context(|| format!("session not found: {session_id}"))?;
    let current_session_id = state.state_store.session_id();
    let context = if &*current_session_id == session_id {
        state.manager.lock().unwrap().context
    } else {
        let config = state.manager.lock().unwrap().config.clone();
        let store = state.state_store.pinned(session_id);
        current_context(&build_session_agent(&config, &state.paths, &store)?)?
    };
    Ok(ipc::SessionState {
        context_tokens: context.tokens,
        context_window: context.window,
        cumulative_tokens: context.cumulative_tokens,
        cumulative_prompt_tokens: context.cumulative_prompt_tokens,
        cumulative_cache_read_tokens: context.cumulative_cache_read_tokens,
        session_id: record.session_id,
        session_name: record.name,
        workspace: record.workspace,
    })
}

/// Global admin reservation (config/model changes): requires that no turn is
/// running in any session.
fn reserve_admin(manager: &Arc<Mutex<ManagerState>>) -> std::result::Result<(), ApiError> {
    let mut manager = manager.lock().unwrap();
    if !manager.active_runs.is_empty() || manager.admin_busy {
        return Err(ApiError::new(StatusCode::CONFLICT, ipc::ADMIN_BUSY_MESSAGE));
    }
    manager.admin_busy = true;
    Ok(())
}

/// Per-session admin reservation (reset/undo/pop/compact/delete/archive):
/// only the target session must be idle; turns in other sessions keep
/// running.
fn reserve_admin_for_session(
    manager: &Arc<Mutex<ManagerState>>,
    session_id: &str,
) -> std::result::Result<(), ApiError> {
    let mut manager = manager.lock().unwrap();
    if manager.admin_busy || manager.session_has_runs(session_id) {
        return Err(ApiError::new(StatusCode::CONFLICT, ipc::ADMIN_BUSY_MESSAGE));
    }
    manager.admin_busy = true;
    Ok(())
}

pub(crate) async fn clear_platform_session_content(
    state: &DaemonState,
    session_id: Arc<str>,
) -> std::result::Result<(), PlatformSessionResetError> {
    state
        .state_store
        .recover_stale_turns()
        .map_err(|error| PlatformSessionResetError::Internal(safe_error_message(error)))?;
    {
        let mut manager = state.manager.lock().unwrap();
        if manager.admin_busy || manager.session_has_runs(&session_id) {
            return Err(PlatformSessionResetError::Busy);
        }
        manager.admin_busy = true;
    }

    let target = state.state_store.pinned(&session_id);
    match target.has_running_turns() {
        Ok(false) => {}
        Ok(true) => {
            release_admin(&state.manager);
            return Err(PlatformSessionResetError::Busy);
        }
        Err(error) => {
            release_admin(&state.manager);
            return Err(PlatformSessionResetError::Internal(safe_error_message(
                error,
            )));
        }
    }

    let (reply, receiver) = oneshot::channel();
    if state
        .actor_tx
        .send(ActorCommand::ClearSessionContent { session_id, reply })
        .is_err()
    {
        release_admin(&state.manager);
        return Err(PlatformSessionResetError::Unavailable);
    }
    match receiver.await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(AdminFailure::Invalid(message) | AdminFailure::Internal(message))) => {
            Err(PlatformSessionResetError::Internal(message))
        }
        Err(_) => {
            release_admin(&state.manager);
            Err(PlatformSessionResetError::Unavailable)
        }
    }
}

pub(crate) async fn reset_platform_persona_state(
    state: &DaemonState,
    config: &AppConfig,
) -> std::result::Result<usize, PlatformPersonaResetError> {
    let persona = config.active_persona_scope();
    let session_ids = state
        .state_store
        .persona_reset_session_ids(&persona, "onebot")
        .map_err(|error| PlatformPersonaResetError::Internal(safe_error_message(error)))?;
    let bindings = state
        .state_store
        .platform_session_bindings(&persona, "onebot")
        .map_err(|error| PlatformPersonaResetError::Internal(safe_error_message(error)))?;
    let targets = session_ids
        .iter()
        .map(String::as_str)
        .collect::<std::collections::HashSet<_>>();

    {
        let mut manager = state.manager.lock().unwrap();
        if manager.admin_busy {
            return Err(PlatformPersonaResetError::Busy);
        }
        manager.admin_busy = true;
        for run in manager
            .active_runs
            .values()
            .filter(|run| targets.contains(&*run.session_id))
        {
            run.request_cancel();
        }
    }

    let tickets = session_ids
        .iter()
        .map(|session_id| state.platforms.preempt_session_turns(session_id))
        .collect::<Vec<_>>();
    let leases = match tokio::time::timeout(Duration::from_secs(10), async {
        let mut leases = Vec::with_capacity(tickets.len());
        for ticket in tickets {
            leases.push(ticket.acquire().await.expect("exclusive platform ticket"));
        }
        leases
    })
    .await
    {
        Ok(leases) => leases,
        Err(_) => {
            release_admin(&state.manager);
            return Err(PlatformPersonaResetError::Busy);
        }
    };

    for _ in 0..200 {
        let running = state
            .manager
            .lock()
            .unwrap()
            .active_runs
            .values()
            .any(|run| targets.contains(&*run.session_id));
        if !running {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    if state
        .manager
        .lock()
        .unwrap()
        .active_runs
        .values()
        .any(|run| targets.contains(&*run.session_id))
    {
        drop(leases);
        release_admin(&state.manager);
        return Err(PlatformPersonaResetError::Busy);
    }

    let plugins = match state.platforms.plugins() {
        Ok(plugins) => plugins,
        Err(error) => {
            drop(leases);
            release_admin(&state.manager);
            return Err(PlatformPersonaResetError::Internal(safe_error_message(
                error,
            )));
        }
    };
    let reset_context = crate::platforms::plugins::PlatformPersonaResetContext {
        config,
        paths: &state.paths,
        bindings: &bindings,
    };
    if let Err(error) = plugins.after_persona_reset(&reset_context).await {
        drop(leases);
        release_admin(&state.manager);
        return Err(PlatformPersonaResetError::Internal(safe_error_message(
            error,
        )));
    }

    let (reply, receiver) = oneshot::channel();
    if state
        .actor_tx
        .send(ActorCommand::ResetPersonaState {
            config: Box::new(config.clone()),
            reply,
        })
        .is_err()
    {
        drop(leases);
        release_admin(&state.manager);
        return Err(PlatformPersonaResetError::Unavailable);
    }
    let result = match receiver.await {
        Ok(Ok(())) => Ok(session_ids.len()),
        Ok(Err(AdminFailure::Invalid(message) | AdminFailure::Internal(message))) => {
            Err(PlatformPersonaResetError::Internal(message))
        }
        Err(_) => {
            release_admin(&state.manager);
            Err(PlatformPersonaResetError::Unavailable)
        }
    };
    drop(leases);
    result
}

/// Light admin reservation (session/model updates): serializes against other
/// admin operations but is allowed while turns are running.
fn reserve_admin_light(manager: &Arc<Mutex<ManagerState>>) -> std::result::Result<(), ApiError> {
    let mut manager = manager.lock().unwrap();
    if manager.admin_busy {
        return Err(ApiError::new(StatusCode::CONFLICT, ipc::ADMIN_BUSY_MESSAGE));
    }
    manager.admin_busy = true;
    Ok(())
}

fn require_no_running_turn(state_store: &StateStore) -> std::result::Result<(), ApiError> {
    if state_store
        .has_any_running_turns()
        .map_err(ApiError::internal)?
    {
        Err(ApiError::new(
            StatusCode::CONFLICT,
            "a conversation turn is already running",
        ))
    } else {
        Ok(())
    }
}

fn release_admin(manager: &Arc<Mutex<ManagerState>>) {
    manager.lock().unwrap().admin_busy = false;
}

fn config_response(
    config: &AppConfig,
    context: ContextSnapshot,
    paths: &MiyuPaths,
) -> std::result::Result<ConfigResponse, ApiError> {
    let mut redacted = config.clone();
    let mut secret_states = HashMap::new();
    for (index, provider) in redacted.providers.iter_mut().enumerate() {
        secret_states.insert(
            format!("providers.{index}.api_key"),
            provider
                .api_key
                .as_deref()
                .is_some_and(|value| !value.trim().is_empty()),
        );
        provider.api_key = None;
    }
    redact_secret_list(
        &mut secret_states,
        "plugins.web.tavily_api_keys",
        &mut redacted.plugins.web.tavily_api_keys,
    );
    redact_secret_list(
        &mut secret_states,
        "plugins.web.firecrawl_api_keys",
        &mut redacted.plugins.web.firecrawl_api_keys,
    );
    redact_secret_list(
        &mut secret_states,
        "plugins.web.anysearch_api_keys",
        &mut redacted.plugins.web.anysearch_api_keys,
    );
    redact_secret_list(
        &mut secret_states,
        "plugins.web.exa_api_keys",
        &mut redacted.plugins.web.exa_api_keys,
    );
    secret_states.insert(
        "plugins.exchange_rate.api_key".to_string(),
        !redacted.plugins.exchange_rate.api_key.trim().is_empty(),
    );
    redacted.plugins.exchange_rate.api_key.clear();
    secret_states.insert(
        "platforms.qq.access_token".to_string(),
        !redacted.platforms.qq.access_token.trim().is_empty(),
    );
    redacted.platforms.qq.access_token.clear();
    redact_secret_list(
        &mut secret_states,
        "plugins.image_generation.api_keys",
        &mut redacted.plugins.image_generation.api_keys,
    );
    redact_api_quota_provider(
        &mut secret_states,
        "plugins.api_quota.deepseek",
        &mut redacted.plugins.api_quota.deepseek,
    );
    redact_api_quota_provider(
        &mut secret_states,
        "plugins.api_quota.openrouter",
        &mut redacted.plugins.api_quota.openrouter,
    );
    let mut config_value = serde_json::to_value(&redacted).map_err(ApiError::internal)?;
    if let Value::Object(config_object) = &mut config_value {
        config_object.insert(
            "memory".to_string(),
            serde_json::to_value(redacted.memory_config()).map_err(ApiError::internal)?,
        );
    }
    let prompts = read_prompt_documents(config, paths).map_err(ApiError::internal)?;
    let persona = persona_identity(config, &prompts);
    Ok(ConfigResponse {
        config: config_value,
        secret_states,
        prompts,
        models: safe_models(config),
        multimodal_models: safe_multimodal_models(config),
        display: web_display_config(config),
        context,
        persona,
    })
}

fn persona_identity(config: &AppConfig, prompts: &PromptDocuments) -> PersonaIdentity {
    let active = config.prompt.active_persona.trim();
    if active.is_empty() {
        return PersonaIdentity {
            name: "Miyu".to_string(),
            avatar_url: Some("/assets/miyu-logo.png".to_string()),
            board_image_url: Some("/assets/miyuwallpaper.png".to_string()),
            board_title: DEFAULT_BOARD_TITLE.to_string(),
            board_subtitle: DEFAULT_BOARD_SUBTITLE.to_string(),
            starter_prompts: DEFAULT_STARTER_PROMPTS.map(str::to_string).to_vec(),
        };
    }
    let document = prompts
        .personas
        .iter()
        .find(|document| document.name == active);
    let avatar_url = document
        .and_then(|document| document.avatar_path.as_deref())
        .filter(|path| !path.trim().is_empty())
        .and_then(|_| Some("/api/persona/avatar".to_string()));
    let board_image_url = if document
        .and_then(|document| document.board_image_path.as_deref())
        .is_some_and(|path| !path.trim().is_empty())
    {
        Some("/api/persona/avatar?board=1".to_string())
    } else {
        None
    };
    let board_title = document
        .and_then(|document| document.board_title.as_deref())
        .filter(|value| !value.trim().is_empty())
        .unwrap_or(DEFAULT_BOARD_TITLE)
        .to_string();
    let board_subtitle = document
        .and_then(|document| document.board_subtitle.as_deref())
        .filter(|value| !value.trim().is_empty())
        .unwrap_or(DEFAULT_BOARD_SUBTITLE)
        .to_string();
    let configured_prompts = document.and_then(|document| document.starter_prompts.as_deref());
    let starter_prompts = DEFAULT_STARTER_PROMPTS
        .iter()
        .enumerate()
        .map(|(index, fallback)| {
            configured_prompts
                .and_then(|values| values.get(index))
                .filter(|value| !value.trim().is_empty())
                .map_or_else(|| (*fallback).to_string(), Clone::clone)
        })
        .collect();
    PersonaIdentity {
        name: active.strip_suffix(".md").unwrap_or(active).to_string(),
        avatar_url,
        board_image_url,
        board_title,
        board_subtitle,
        starter_prompts,
    }
}

fn active_persona_avatar_path(
    config: &AppConfig,
    prompts: &PromptDocuments,
    paths: &MiyuPaths,
) -> Option<PathBuf> {
    let active = config.prompt.active_persona.trim();
    if active.is_empty() {
        return None;
    }
    let value = prompts
        .personas
        .iter()
        .find(|document| document.name == active)
        .and_then(|document| document.avatar_path.as_deref())?;
    resolve_persona_asset_path(paths, value)
}

fn active_persona_board_path(
    config: &AppConfig,
    prompts: &PromptDocuments,
    paths: &MiyuPaths,
) -> Option<PathBuf> {
    let active = config.prompt.active_persona.trim();
    let value = prompts
        .personas
        .iter()
        .find(|document| document.name == active)
        .and_then(|document| document.board_image_path.as_deref())?;
    resolve_persona_asset_path(paths, value)
}

fn resolve_persona_asset_path(paths: &MiyuPaths, value: &str) -> Option<PathBuf> {
    let value = value.trim();
    if persona_asset_uses_managed_namespace(value) {
        return managed_persona_asset_path(paths, value);
    }
    let path = PathBuf::from(value);
    if let Some(path) = paths.migrated_resource_path(&path) {
        return Some(path);
    }
    Some(if path.is_absolute() {
        path
    } else {
        paths.config_dir.join(path)
    })
}

fn managed_persona_asset_path(paths: &MiyuPaths, value: &str) -> Option<PathBuf> {
    let value = value.trim();
    if value.contains('\\') || value.chars().any(char::is_control) {
        return None;
    }
    let mut components = std::path::Path::new(value).components();
    while matches!(
        components.clone().next(),
        Some(std::path::Component::CurDir)
    ) {
        components.next();
    }
    if !matches!(components.next(), Some(std::path::Component::Normal(name)) if name == "persona-avatars")
    {
        return None;
    }
    let mut normalized = PathBuf::new();
    for component in components {
        match component {
            std::path::Component::Normal(component) => normalized.push(component),
            _ => return None,
        }
    }
    if normalized.as_os_str().is_empty() {
        return None;
    }
    Some(paths.persona_avatars_dir().join(normalized))
}

fn persona_asset_uses_managed_namespace(value: &str) -> bool {
    std::path::Path::new(value)
        .components()
        .find(|component| !matches!(component, std::path::Component::CurDir))
        .is_some_and(|component| {
            matches!(component, std::path::Component::Normal(name) if name == "persona-avatars")
        })
}

fn validate_managed_persona_asset_file(paths: &MiyuPaths, path: &FilePath) -> Result<()> {
    let root_path = paths.persona_avatars_dir();
    let root_metadata = std::fs::symlink_metadata(&root_path)?;
    if root_metadata.file_type().is_symlink() || !root_metadata.is_dir() {
        bail!("managed persona asset directory is unsafe");
    }
    let root = std::fs::canonicalize(root_path)?;
    let canonical = std::fs::canonicalize(path)?;
    if !canonical.starts_with(&root) || !std::fs::metadata(&canonical)?.is_file() {
        bail!("managed persona asset escapes its resource directory");
    }
    Ok(())
}

fn redact_secret_list(states: &mut HashMap<String, bool>, key: &str, values: &mut Vec<String>) {
    states.insert(
        key.to_string(),
        values.iter().any(|value| !value.trim().is_empty()),
    );
    values.clear();
}

fn redact_api_quota_provider(
    states: &mut HashMap<String, bool>,
    prefix: &str,
    provider: &mut crate::config::ApiQuotaProviderConfig,
) {
    if provider.accounts.is_empty() {
        provider
            .accounts
            .push(crate::config::ApiQuotaAccountConfig {
                id: "account-1".to_string(),
                name: "默认账号".to_string(),
                api_key: provider.api_key.clone(),
            });
    } else if !provider.api_key.trim().is_empty() && provider.accounts[0].api_key.trim().is_empty()
    {
        provider.accounts[0].api_key = provider.api_key.clone();
    }
    provider.api_key.clear();
    let mut used_ids = HashSet::with_capacity(provider.accounts.len());
    for (index, account) in provider.accounts.iter_mut().enumerate() {
        if account.id.trim().is_empty() || !used_ids.insert(account.id.clone()) {
            let mut number = index + 1;
            loop {
                let candidate = format!("account-{number}");
                if used_ids.insert(candidate.clone()) {
                    account.id = candidate;
                    break;
                }
                number += 1;
            }
        }
    }
    for (index, account) in provider.accounts.iter_mut().enumerate() {
        let key = format!("{prefix}.accounts.{index}.api_key");
        states.insert(key, !account.api_key.trim().is_empty());
        account.api_key.clear();
    }
}

fn restore_config_secrets(
    candidate: &mut AppConfig,
    current: &AppConfig,
    mutations: &HashMap<String, SecretMutation>,
) -> std::result::Result<(), ApiError> {
    let mut recognized = HashSet::new();
    for (index, provider) in candidate.providers.iter_mut().enumerate() {
        let key = format!("providers.{index}.api_key");
        recognized.insert(key.clone());
        let existing = current
            .providers
            .iter()
            .find(|item| item.id == provider.id)
            .and_then(|item| item.api_key.clone());
        provider.api_key = match mutations.get(&key) {
            Some(SecretMutation::Set(value)) => normalize_single_secret(value, &key)?,
            Some(SecretMutation::Clear) => None,
            None => existing,
        };
    }

    restore_secret_list(
        candidate,
        current,
        mutations,
        &mut recognized,
        "plugins.web.tavily_api_keys",
        |config| &mut config.plugins.web.tavily_api_keys,
        |config| &config.plugins.web.tavily_api_keys,
    )?;
    restore_secret_list(
        candidate,
        current,
        mutations,
        &mut recognized,
        "plugins.web.firecrawl_api_keys",
        |config| &mut config.plugins.web.firecrawl_api_keys,
        |config| &config.plugins.web.firecrawl_api_keys,
    )?;
    restore_secret_list(
        candidate,
        current,
        mutations,
        &mut recognized,
        "plugins.web.anysearch_api_keys",
        |config| &mut config.plugins.web.anysearch_api_keys,
        |config| &config.plugins.web.anysearch_api_keys,
    )?;
    restore_secret_list(
        candidate,
        current,
        mutations,
        &mut recognized,
        "plugins.web.exa_api_keys",
        |config| &mut config.plugins.web.exa_api_keys,
        |config| &config.plugins.web.exa_api_keys,
    )?;
    restore_secret_list(
        candidate,
        current,
        mutations,
        &mut recognized,
        "plugins.image_generation.api_keys",
        |config| &mut config.plugins.image_generation.api_keys,
        |config| &config.plugins.image_generation.api_keys,
    )?;

    let exchange_key = "plugins.exchange_rate.api_key";
    recognized.insert(exchange_key.to_string());
    candidate.plugins.exchange_rate.api_key = match mutations.get(exchange_key) {
        Some(SecretMutation::Set(value)) => {
            normalize_single_secret(value, exchange_key)?.unwrap_or_default()
        }
        Some(SecretMutation::Clear) => String::new(),
        None => current.plugins.exchange_rate.api_key.clone(),
    };

    restore_api_quota_provider(
        &mut candidate.plugins.api_quota.deepseek,
        &current.plugins.api_quota.deepseek,
        mutations,
        &mut recognized,
        "plugins.api_quota.deepseek",
    )?;
    restore_api_quota_provider(
        &mut candidate.plugins.api_quota.openrouter,
        &current.plugins.api_quota.openrouter,
        mutations,
        &mut recognized,
        "plugins.api_quota.openrouter",
    )?;

    let onebot_token_key = "platforms.qq.access_token";
    recognized.insert(onebot_token_key.to_string());
    candidate.platforms.qq.access_token = match mutations.get(onebot_token_key) {
        Some(SecretMutation::Set(value)) => {
            normalize_single_secret(value, onebot_token_key)?.unwrap_or_default()
        }
        Some(SecretMutation::Clear) => String::new(),
        None => current.platforms.qq.access_token.clone(),
    };

    if let Some(key) = mutations.keys().find(|key| !recognized.contains(*key)) {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            format!("unknown secret field: {key}"),
        ));
    }
    Ok(())
}

fn restore_api_quota_provider(
    candidate: &mut crate::config::ApiQuotaProviderConfig,
    current: &crate::config::ApiQuotaProviderConfig,
    mutations: &HashMap<String, SecretMutation>,
    recognized: &mut HashSet<String>,
    prefix: &str,
) -> std::result::Result<(), ApiError> {
    for (index, account) in candidate.accounts.iter_mut().enumerate() {
        let key = format!("{prefix}.accounts.{index}.api_key");
        recognized.insert(key.clone());
        let mut existing = current
            .accounts
            .iter()
            .find(|item| !account.id.is_empty() && item.id == account.id)
            .or_else(|| {
                current
                    .accounts
                    .iter()
                    .find(|item| item.id.is_empty() && item.name == account.name)
            })
            .map(|item| item.api_key.clone())
            .or_else(|| {
                (index == 0 && current.accounts.is_empty()).then(|| current.api_key.clone())
            })
            .unwrap_or_default();
        if existing.is_empty() && index == 0 && !current.api_key.trim().is_empty() {
            existing = current.api_key.clone();
        }
        account.api_key = match mutations.get(&key) {
            Some(SecretMutation::Set(value)) => {
                normalize_single_secret(value, &key)?.unwrap_or_default()
            }
            Some(SecretMutation::Clear) => String::new(),
            None => existing,
        };
    }
    candidate.api_key.clear();
    Ok(())
}

fn restore_secret_list<Mut, Ref>(
    candidate: &mut AppConfig,
    current: &AppConfig,
    mutations: &HashMap<String, SecretMutation>,
    recognized: &mut HashSet<String>,
    key: &str,
    candidate_values: Mut,
    current_values: Ref,
) -> std::result::Result<(), ApiError>
where
    Mut: FnOnce(&mut AppConfig) -> &mut Vec<String>,
    Ref: FnOnce(&AppConfig) -> &Vec<String>,
{
    recognized.insert(key.to_string());
    *candidate_values(candidate) = match mutations.get(key) {
        Some(SecretMutation::Set(value)) => parse_secret_list(value, key)?,
        Some(SecretMutation::Clear) => Vec::new(),
        None => current_values(current).clone(),
    };
    Ok(())
}

fn normalize_single_secret(
    value: &str,
    field: &str,
) -> std::result::Result<Option<String>, ApiError> {
    validate_secret_text(value, field)?;
    Ok(Some(value.trim().to_string()).filter(|value| !value.is_empty()))
}

fn parse_secret_list(value: &str, field: &str) -> std::result::Result<Vec<String>, ApiError> {
    validate_secret_text(value, field)?;
    Ok(value
        .split(|character| matches!(character, ',' | '\n' | '\r'))
        .map(str::trim)
        .filter(|item| !item.is_empty())
        .map(str::to_string)
        .collect())
}

fn validate_secret_text(value: &str, field: &str) -> std::result::Result<(), ApiError> {
    if value.chars().count() > MAX_SECRET_CHARS
        || value
            .chars()
            .any(|character| character.is_control() && !matches!(character, '\n' | '\r' | '\t'))
    {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            format!("{field} is invalid"),
        ));
    }
    Ok(())
}

fn validate_config_candidate(config: &AppConfig) -> std::result::Result<(), ApiError> {
    config.validate().map_err(|error| {
        ApiError::new(
            StatusCode::BAD_REQUEST,
            format!("invalid configuration: {}", safe_error_message(error)),
        )
    })?;
    let mut provider_ids = HashSet::with_capacity(config.providers.len());
    for provider in &config.providers {
        if !provider_ids.insert(provider.id.trim()) {
            return Err(ApiError::new(
                StatusCode::BAD_REQUEST,
                format!("duplicate provider id: {}", provider.id),
            ));
        }
    }
    if let Some(active) = &config.active_provider_models {
        let mut checked = config.clone();
        checked
            .set_active_provider_models(active)
            .map_err(|error| ApiError::new(StatusCode::BAD_REQUEST, safe_error_message(error)))?;
    }
    if let Some(active) = &config.active_multimodal_provider_models {
        let choices = config.multimodal_provider_model_choices();
        let mut seen = HashSet::with_capacity(active.len());
        for model in active {
            if !seen.insert((&model.provider_id, &model.model))
                || !choices.iter().any(|choice| {
                    choice.provider_id == model.provider_id && choice.model == model.model
                })
            {
                return Err(ApiError::new(
                    StatusCode::BAD_REQUEST,
                    format!(
                        "invalid multimodal provider/model: {} / {}",
                        model.provider_id, model.model
                    ),
                ));
            }
        }
    }
    Ok(())
}

fn validate_prompt_documents(
    config: &AppConfig,
    prompts: &PromptDocuments,
) -> std::result::Result<(), ApiError> {
    validate_prompt_document_list("persona", &prompts.personas)?;
    validate_prompt_document_list("identity", &prompts.identities)?;
    let mut persona_scopes = HashMap::<String, &str>::new();
    for document in &prompts.personas {
        if document.name.eq_ignore_ascii_case("system-prompt.md") {
            return Err(ApiError::new(
                StatusCode::BAD_REQUEST,
                "system-prompt.md is reserved and cannot be used as a persona",
            ));
        }
        let scope = crate::config::persona_scope_name(&document.name);
        if let Some(existing) = persona_scopes.insert(scope.clone(), &document.name) {
            return Err(ApiError::new(
                StatusCode::BAD_REQUEST,
                format!(
                    "persona names map to the same persistent scope: {existing} and {} ({scope})",
                    document.name
                ),
            ));
        }
    }
    if !config.prompt.active_persona.trim().is_empty()
        && !prompts
            .personas
            .iter()
            .any(|document| document.name == config.prompt.active_persona)
    {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "the active persona does not exist",
        ));
    }
    for route in &config.platforms.qq.conversations {
        let Some(name) = route.persona.custom_name() else {
            continue;
        };
        if !prompts
            .personas
            .iter()
            .any(|document| document.name == name)
        {
            return Err(ApiError::new(
                StatusCode::BAD_REQUEST,
                format!("QQ conversation persona does not exist: {name}"),
            ));
        }
    }
    if !config.prompt.active_identity.trim().is_empty()
        && !prompts
            .identities
            .iter()
            .any(|document| document.name == config.prompt.active_identity)
    {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "the active identity does not exist",
        ));
    }
    Ok(())
}

fn reconcile_qq_persona_references(config: &mut AppConfig, prompts: &PromptDocuments) {
    let renames = prompts
        .personas
        .iter()
        .filter_map(|document| {
            document
                .original_name
                .as_deref()
                .filter(|original| *original != document.name)
                .map(|original| (original.to_string(), document.name.clone()))
        })
        .collect::<HashMap<_, _>>();
    for route in &mut config.platforms.qq.conversations {
        let Some(current) = route.persona.custom_name() else {
            continue;
        };
        if let Some(next) = renames.get(current) {
            route.persona = crate::config::PlatformPersonaOverride::Custom { name: next.clone() };
        }
    }
}

fn validate_prompt_document_list(
    kind: &str,
    documents: &[PromptDocument],
) -> std::result::Result<(), ApiError> {
    if documents.len() > MAX_PROMPT_DOCUMENTS {
        return Err(ApiError::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            format!("at most {MAX_PROMPT_DOCUMENTS} {kind} documents are allowed"),
        ));
    }
    let mut names = HashSet::with_capacity(documents.len());
    let mut original_names = HashSet::with_capacity(documents.len());
    for document in documents {
        validate_prompt_document_name(&document.name, kind)?;
        if !names.insert(document.name.as_str()) {
            return Err(ApiError::new(
                StatusCode::BAD_REQUEST,
                format!("duplicate {kind} document: {}", document.name),
            ));
        }
        if document.content.chars().count() > MAX_PROMPT_DOCUMENT_CHARS
            || document.content.contains('\0')
        {
            return Err(ApiError::new(
                StatusCode::PAYLOAD_TOO_LARGE,
                format!("{kind} document is too large: {}", document.name),
            ));
        }
        for (field, value) in [
            ("avatar", document.avatar_path.as_deref()),
            ("board image", document.board_image_path.as_deref()),
        ] {
            if value.is_some_and(|path| {
                path.len() > 4_096 || path.contains('\0') || path.trim() != path
            }) {
                return Err(ApiError::new(
                    StatusCode::BAD_REQUEST,
                    format!("invalid {kind} {field} path: {}", document.name),
                ));
            }
        }
        for (field, value) in [
            ("board title", document.board_title.as_deref()),
            ("board subtitle", document.board_subtitle.as_deref()),
        ] {
            if value.is_some_and(|text| {
                text.chars().count() > 200 || text.chars().any(char::is_control)
            }) {
                return Err(ApiError::new(
                    StatusCode::BAD_REQUEST,
                    format!("invalid {kind} {field}: {}", document.name),
                ));
            }
        }
        if let Some(prompts) = document.starter_prompts.as_deref() {
            if prompts.len() > 4
                || prompts
                    .iter()
                    .any(|text| text.chars().count() > 200 || text.chars().any(char::is_control))
            {
                return Err(ApiError::new(
                    StatusCode::BAD_REQUEST,
                    format!("invalid {kind} starter prompts: {}", document.name),
                ));
            }
        }
        if let Some(original) = document.original_name.as_deref() {
            validate_prompt_document_name(original, kind)?;
            if !original_names.insert(original) {
                return Err(ApiError::new(
                    StatusCode::BAD_REQUEST,
                    format!("duplicate original {kind} document: {original}"),
                ));
            }
        }
    }
    Ok(())
}

fn validate_prompt_document_name(name: &str, kind: &str) -> std::result::Result<(), ApiError> {
    let valid = name == name.trim()
        && name.ends_with(".md")
        && name.len() <= 240
        && name.len() > 3
        && !name.starts_with('.')
        && !name.contains(['/', '\\'])
        && !name.chars().any(char::is_control)
        && FilePath::new(name)
            .file_name()
            .and_then(|value| value.to_str())
            == Some(name);
    if !valid {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            format!("invalid {kind} document name: {name}"),
        ));
    }
    Ok(())
}

fn read_prompt_documents(config: &AppConfig, paths: &MiyuPaths) -> Result<PromptDocuments> {
    Ok(PromptDocuments {
        personas: read_prompt_document_dir(&config.prompts_dir_path(paths), true)?,
        identities: read_prompt_document_dir(&config.identities_dir_path(paths), false)?,
    })
}

fn read_prompt_document_dir(
    dir: &FilePath,
    with_avatar_metadata: bool,
) -> Result<Vec<PromptDocument>> {
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut documents = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        if !name.ends_with(".md") {
            continue;
        }
        if with_avatar_metadata && name.eq_ignore_ascii_case("system-prompt.md") {
            continue;
        }
        let content = std::fs::read_to_string(entry.path())?;
        let metadata = with_avatar_metadata
            .then(|| read_prompt_metadata(&entry.path()))
            .flatten()
            .unwrap_or_default();
        documents.push(PromptDocument {
            original_name: Some(name.clone()),
            name,
            content,
            avatar_path: metadata.avatar_path,
            board_image_path: metadata.board_image_path,
            board_title: metadata.board_title,
            board_subtitle: metadata.board_subtitle,
            starter_prompts: metadata.starter_prompts,
        });
    }
    documents.sort_by(|left, right| left.name.cmp(&right.name));
    Ok(documents)
}

fn read_prompt_metadata(path: &FilePath) -> Option<PersonaMetadata> {
    let sidecar = path.with_extension("json");
    let raw = std::fs::read_to_string(sidecar).ok()?;
    serde_json::from_str(&raw).ok()
}

fn prompt_configuration_changed(current: &AppConfig, candidate: &AppConfig) -> bool {
    serde_json::to_value(&current.prompt).ok() != serde_json::to_value(&candidate.prompt).ok()
        || current.system_prompt_file != candidate.system_prompt_file
        || current.system_prompt != candidate.system_prompt
}

fn prompt_documents_changed(current: &PromptDocuments, candidate: &PromptDocuments) -> bool {
    canonical_prompt_documents(&current.personas) != canonical_prompt_documents(&candidate.personas)
        || canonical_prompt_documents(&current.identities)
            != canonical_prompt_documents(&candidate.identities)
}

fn canonical_prompt_documents(documents: &[PromptDocument]) -> Vec<(String, String)> {
    let mut values = documents
        .iter()
        .map(|document| (document.name.clone(), document.content.clone()))
        .collect::<Vec<_>>();
    values.sort_by(|left, right| left.0.cmp(&right.0));
    values
}

struct FileBackup {
    path: PathBuf,
    content: Option<Vec<u8>>,
}

struct PersonaScopeBackup {
    original: PathBuf,
    staged: PathBuf,
    destination: Option<PathBuf>,
}

struct PersonaDbRenameGuard {
    state: StateStore,
    renames: Vec<(String, String)>,
    committed: bool,
}

impl PersonaDbRenameGuard {
    fn new(state: StateStore, changes: &[(String, Option<String>)]) -> Result<Self> {
        let renames = changes
            .iter()
            .filter_map(|(old_name, new_name)| {
                let new_name = new_name.as_deref()?;
                let old_scope = crate::config::persona_scope_name(old_name);
                let new_scope = crate::config::persona_scope_name(new_name);
                (old_scope != new_scope).then_some((old_scope, new_scope))
            })
            .collect::<Vec<_>>();
        migrate_persona_db_scopes(&state, &renames)?;
        Ok(Self {
            state,
            renames,
            committed: false,
        })
    }

    fn commit(&mut self) {
        self.committed = true;
    }
}

impl Drop for PersonaDbRenameGuard {
    fn drop(&mut self) {
        if self.committed || self.renames.is_empty() {
            return;
        }
        let reverse = self
            .renames
            .iter()
            .map(|(old, new)| (new.clone(), old.clone()))
            .collect::<Vec<_>>();
        let _ = migrate_persona_db_scopes(&self.state, &reverse);
    }
}

fn migrate_persona_db_scopes(state: &StateStore, renames: &[(String, String)]) -> Result<()> {
    let staged = renames
        .iter()
        .map(|(old, new)| {
            (
                old.clone(),
                format!("persona-migration-{}", random_token(18)),
                new.clone(),
            )
        })
        .collect::<Vec<_>>();
    for (staged_count, (old, temporary, _)) in staged.iter().enumerate() {
        if let Err(error) = state.rename_persona_scope(old, temporary) {
            for (old, temporary, _) in staged[..staged_count].iter().rev() {
                let _ = state.rename_persona_scope(temporary, old);
            }
            return Err(error);
        }
    }
    for (finalized, (_, temporary, new)) in staged.iter().enumerate() {
        if let Err(error) = state.rename_persona_scope(temporary, new) {
            for (_, temporary, new) in staged[..finalized].iter().rev() {
                let _ = state.rename_persona_scope(new, temporary);
            }
            for (old, temporary, _) in staged.iter().rev() {
                let _ = state.rename_persona_scope(temporary, old);
            }
            return Err(error);
        }
    }
    Ok(())
}

fn apply_prompt_documents(
    current_config: &AppConfig,
    next_config: &AppConfig,
    current: &PromptDocuments,
    next: &PromptDocuments,
    paths: &MiyuPaths,
) -> Result<Vec<FileBackup>> {
    let mut mutations = HashMap::<PathBuf, Option<Vec<u8>>>::new();
    collect_prompt_file_mutations(
        &current.personas,
        &next.personas,
        &current_config.prompts_dir_path(paths),
        &next_config.prompts_dir_path(paths),
        &mut mutations,
        true,
    );
    collect_prompt_file_mutations(
        &current.identities,
        &next.identities,
        &current_config.identities_dir_path(paths),
        &next_config.identities_dir_path(paths),
        &mut mutations,
        false,
    );
    let backups = mutations
        .keys()
        .map(|path| FileBackup {
            path: path.clone(),
            content: std::fs::read(path).ok(),
        })
        .collect::<Vec<_>>();
    for (path, content) in mutations {
        let result = if let Some(content) = content {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&path, content)
        } else if path.exists() {
            std::fs::remove_file(&path)
        } else {
            Ok(())
        };
        if let Err(error) = result {
            restore_file_backups(&backups);
            return Err(error.into());
        }
    }
    Ok(backups)
}

fn apply_persona_scope_changes(
    current_config: &AppConfig,
    next_config: &AppConfig,
    current: &PromptDocuments,
    next: &PromptDocuments,
    paths: &MiyuPaths,
) -> Result<Vec<PersonaScopeBackup>> {
    let changes = persona_document_changes(current, next);
    let mut backups = Vec::new();
    let stage_result = (|| -> Result<()> {
        for (change_index, (old_name, new_name)) in changes.iter().enumerate() {
            let old_paths = [
                current_config.persona_memory_data_dir(paths, old_name),
                current_config.persona_memory_state_dir(paths, old_name),
                current_config.persona_skills_dir(paths, old_name),
            ];
            let new_paths = new_name.as_ref().map(|name| {
                [
                    next_config.persona_memory_data_dir(paths, name),
                    next_config.persona_memory_state_dir(paths, name),
                    next_config.persona_skills_dir(paths, name),
                ]
            });
            for (scope_index, original) in old_paths.into_iter().enumerate() {
                if !original.exists() {
                    continue;
                }
                let parent = original
                    .parent()
                    .context("persona scope path has no parent")?;
                let staged = parent.join(format!(
                    ".miyu-web-scope-{}-{change_index}-{scope_index}",
                    random_token(10)
                ));
                std::fs::rename(&original, &staged)?;
                backups.push(PersonaScopeBackup {
                    original,
                    staged,
                    destination: new_paths.as_ref().map(|paths| paths[scope_index].clone()),
                });
            }
        }
        Ok(())
    })();
    if let Err(error) = stage_result {
        restore_persona_scope_backups(&backups);
        return Err(error);
    }

    let result = (|| -> Result<()> {
        for backup in &backups {
            let Some(destination) = &backup.destination else {
                continue;
            };
            if destination.exists() {
                anyhow::bail!(
                    "persona scope destination already exists: {}",
                    destination.display()
                );
            }
            if let Some(parent) = destination.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::rename(&backup.staged, destination)?;
        }
        Ok(())
    })();
    if let Err(error) = result {
        restore_persona_scope_backups(&backups);
        return Err(error);
    }
    Ok(backups)
}

/// True when applying `next` cannot safely coexist with running turns:
/// persona renames/deletions and active-persona switches migrate or delete
/// the session state those turns are using. Everything else hot-applies.
fn config_change_requires_interrupt(
    current: &AppConfig,
    next: &AppConfig,
    paths: &MiyuPaths,
    next_prompts: &PromptDocuments,
) -> bool {
    let Ok(previous_prompts) = read_prompt_documents(current, paths) else {
        // The safe direction: interrupt when the current prompt layout cannot
        // be read to prove the change is turn-safe.
        return true;
    };
    if !persona_document_changes(&previous_prompts, next_prompts).is_empty() {
        return true;
    }
    current.active_persona_scope() != next.active_persona_scope()
}

fn persona_document_changes(
    current: &PromptDocuments,
    next: &PromptDocuments,
) -> Vec<(String, Option<String>)> {
    let mut changes = Vec::new();
    for document in &current.personas {
        let represented = next.personas.iter().find(|next_document| {
            next_document.original_name.as_deref() == Some(document.name.as_str())
                || next_document.original_name.is_none() && next_document.name == document.name
        });
        match represented {
            Some(next_document) if next_document.name != document.name => {
                changes.push((document.name.clone(), Some(next_document.name.clone())));
            }
            None => changes.push((document.name.clone(), None)),
            _ => {}
        }
    }
    changes
}

fn restore_persona_scope_backups(backups: &[PersonaScopeBackup]) {
    for backup in backups.iter().rev() {
        if let Some(destination) = &backup.destination {
            if destination.exists() && !backup.staged.exists() {
                let _ = std::fs::rename(destination, &backup.staged);
            }
        }
        if backup.staged.exists() {
            if let Some(parent) = backup.original.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let _ = std::fs::rename(&backup.staged, &backup.original);
        }
    }
}

fn finalize_persona_scope_backups(backups: &[PersonaScopeBackup]) {
    for backup in backups {
        if backup.destination.is_none() && backup.staged.exists() {
            let _ = std::fs::remove_dir_all(&backup.staged);
        }
    }
}

fn collect_prompt_file_mutations(
    current: &[PromptDocument],
    next: &[PromptDocument],
    current_dir: &FilePath,
    next_dir: &FilePath,
    mutations: &mut HashMap<PathBuf, Option<Vec<u8>>>,
    with_avatar_metadata: bool,
) {
    for document in next {
        let content = document.content.trim_end();
        let content = if content.is_empty() {
            Vec::new()
        } else {
            format!("{content}\n").into_bytes()
        };
        mutations.insert(next_dir.join(&document.name), Some(content));
        if with_avatar_metadata {
            let metadata_path = next_dir.join(&document.name).with_extension("json");
            let metadata = PersonaMetadata {
                avatar_path: document
                    .avatar_path
                    .as_deref()
                    .map(str::trim)
                    .filter(|path| !path.is_empty())
                    .map(str::to_string),
                board_image_path: document
                    .board_image_path
                    .as_deref()
                    .map(str::trim)
                    .filter(|path| !path.is_empty())
                    .map(str::to_string),
                board_title: document
                    .board_title
                    .as_deref()
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .map(str::to_string),
                board_subtitle: document
                    .board_subtitle
                    .as_deref()
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .map(str::to_string),
                starter_prompts: document.starter_prompts.clone(),
            };
            let metadata = if metadata.avatar_path.is_none()
                && metadata.board_image_path.is_none()
                && metadata.board_title.is_none()
                && metadata.board_subtitle.is_none()
                && metadata.starter_prompts.is_none()
            {
                None
            } else {
                Some(
                    serde_json::to_vec_pretty(&metadata)
                        .expect("serializing persona metadata cannot fail"),
                )
            };
            mutations.insert(
                metadata_path,
                metadata.map(|mut bytes| {
                    bytes.push(b'\n');
                    bytes
                }),
            );
        }
    }
    for document in current {
        let represented = next.iter().find(|next_document| {
            next_document.original_name.as_deref() == Some(document.name.as_str())
                || next_document.original_name.is_none() && next_document.name == document.name
        });
        let old_path = current_dir.join(&document.name);
        let retained_at_same_path = represented
            .map(|next_document| next_dir.join(&next_document.name) == old_path)
            .unwrap_or(false);
        if !retained_at_same_path {
            mutations.entry(old_path).or_insert(None);
            if with_avatar_metadata {
                mutations
                    .entry(current_dir.join(&document.name).with_extension("json"))
                    .or_insert(None);
            }
        }
    }
}

fn restore_file_backups(backups: &[FileBackup]) {
    for backup in backups {
        restore_optional_file(&backup.path, backup.content.as_deref());
    }
}

fn restore_optional_file(path: &FilePath, content: Option<&[u8]>) {
    if let Some(content) = content {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::write(path, content);
    } else if path.exists() {
        let _ = std::fs::remove_file(path);
    }
}

fn safe_models(config: &AppConfig) -> Vec<SafeModel> {
    config
        .provider_model_choices()
        .into_iter()
        .map(|choice| SafeModel {
            active: config.is_active_provider_model(&choice.provider_id, &choice.model),
            provider_id: choice.provider_id,
            provider_name: choice.provider_name,
            model: choice.model,
        })
        .collect()
}

fn web_display_config(config: &AppConfig) -> WebDisplayConfig {
    let mixed_model_endpoint_display = config.display.mixed_model_endpoint_display.clone();
    WebDisplayConfig {
        reasoning: config.display.reasoning.clone(),
        tool_calls: config.display.tool_calls.clone(),
        readable_tool_names: config.display.readable_tool_names,
        command_output_lines: config.display.command_output_lines,
        show_mixed_model_endpoint: config.active_provider_model_choices().len() > 1
            && matches!(mixed_model_endpoint_display.as_str(), "interactive" | "all"),
        mixed_model_endpoint_display,
    }
}

fn safe_multimodal_models(config: &AppConfig) -> Vec<SafeModel> {
    config
        .multimodal_provider_model_choices()
        .into_iter()
        .map(|choice| SafeModel {
            active: config.is_active_multimodal_provider_model(&choice.provider_id, &choice.model),
            provider_id: choice.provider_id,
            provider_name: choice.provider_name,
            model: choice.model,
        })
        .collect()
}

impl SafeTurn {
    fn from_turn(turn: Turn, assets: Vec<ImageAsset>, artifacts: Vec<ArtifactAsset>) -> Self {
        let assets = assets
            .into_iter()
            .map(|asset| {
                let hide_caption = meme_asset_caption_hidden(&asset, &turn.tool_reports);
                SafeImageAsset::from_asset(asset, hide_caption)
            })
            .collect();
        Self {
            id: turn.turn_id,
            seq: turn.seq,
            status: match turn.status {
                TurnStatus::Running => "running",
                TurnStatus::Completed => "completed",
                TurnStatus::Interrupted => "interrupted",
            },
            active_context: !turn.hidden,
            user_content: turn.display_content,
            assistant_content: redact_internal_assistant_text(&turn.assistant_content),
            assistant_reasoning: turn
                .assistant_reasoning
                .map(|reasoning| redact_internal_assistant_text(&reasoning)),
            provider_id: turn.assistant_provider_id,
            model: turn.assistant_model,
            user_timestamp: turn.user_timestamp,
            assistant_timestamp: turn.assistant_timestamp,
            token_total: turn.token_total,
            token_prompt: turn.token_prompt,
            token_cache_read: turn.token_cache_read,
            token_usage_estimated: turn.token_usage_estimated,
            question_exchanges: turn.question_exchanges,
            followups: turn.followups.into_iter().map(SafeFollowup::from).collect(),
            assets,
            artifacts: artifacts.into_iter().map(SafeArtifactAsset::from).collect(),
            attachments: turn
                .attachments
                .into_iter()
                .map(SafeUserAttachment::from)
                .collect(),
            revision: turn.revision,
        }
    }
}

impl From<ArtifactAsset> for SafeArtifactAsset {
    fn from(asset: ArtifactAsset) -> Self {
        Self {
            url: format!("/api/artifacts/{}", asset.asset_id),
            id: asset.asset_id,
            name: asset.file_name,
            mime: asset.mime,
            kind: asset.kind,
            type_label: artifact_type_label(&asset.source_key),
            size: asset.size_bytes,
            updated_at: asset.updated_at,
        }
    }
}

fn artifact_type_label(source_key: &str) -> String {
    let extension = FilePath::new(source_key)
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or_default()
        .to_ascii_uppercase();
    match extension.as_str() {
        "MARKDOWN" => "MD".to_string(),
        "HTML" | "HTM" => "HTML".to_string(),
        "JSONL" => "JSONL".to_string(),
        "JSON" => "JSON".to_string(),
        "PDF" => "PDF".to_string(),
        value if value.len() <= 6 && !value.is_empty() => value.to_string(),
        _ => "FILE".to_string(),
    }
}

impl SafeImageAsset {
    fn from_asset(asset: ImageAsset, hide_caption: bool) -> Self {
        Self {
            url: format!("/api/assets/{}", asset.asset_id),
            id: asset.asset_id,
            mime: asset.mime,
            width: asset.width,
            height: asset.height,
            alt: asset.alt,
            hide_caption,
        }
    }
}

impl From<ImageAsset> for SafeImageAsset {
    fn from(asset: ImageAsset) -> Self {
        Self::from_asset(asset, false)
    }
}

fn meme_asset_caption_hidden(asset: &ImageAsset, reports: &[String]) -> bool {
    const MAX_DESCRIPTION_CHARS: usize = 120;

    let description = asset.alt.split_whitespace().collect::<Vec<_>>().join(" ");
    if description.is_empty() {
        return false;
    }
    let mut characters = description.chars();
    let mut compact = characters
        .by_ref()
        .take(MAX_DESCRIPTION_CHARS)
        .collect::<String>();
    if characters.next().is_some() {
        compact.push('…');
    }
    let escaped = compact
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;");
    let marker = format!("description={escaped}</sent_meme>");
    reports
        .iter()
        .any(|report| report.starts_with("<sent_meme>") && report.contains(&marker))
}

impl From<TurnFollowup> for SafeFollowup {
    fn from(followup: TurnFollowup) -> Self {
        Self {
            id: followup.prompt_id,
            content: followup.display_content,
            submitted_at: followup.submitted_at,
            preceding_assistant_content: followup
                .preceding_assistant_content
                .map(|content| redact_internal_assistant_text(&content)),
            preceding_assistant_reasoning: followup
                .preceding_assistant_reasoning
                .map(|reasoning| redact_internal_assistant_text(&reasoning)),
            provider_id: followup.preceding_assistant_provider_id,
            model: followup.preceding_assistant_model,
            attachments: followup
                .uploaded_attachments
                .into_iter()
                .map(SafeUserAttachment::from)
                .collect(),
        }
    }
}

impl From<QueuedPrompt> for SafeQueuedPrompt {
    fn from(prompt: QueuedPrompt) -> Self {
        Self {
            id: prompt.prompt_id,
            content: prompt.display_content,
            submitted_at: prompt.submitted_at,
            attachments: prompt
                .uploaded_attachments
                .into_iter()
                .map(SafeUserAttachment::from)
                .collect(),
        }
    }
}

impl From<UserAttachment> for SafeUserAttachment {
    fn from(attachment: UserAttachment) -> Self {
        Self {
            url: format!("/api/attachments/{}", attachment.attachment_id),
            id: attachment.attachment_id,
            name: attachment.file_name,
            mime: attachment.mime,
            kind: attachment.kind,
            size: attachment.size_bytes,
            width: attachment.width,
            height: attachment.height,
        }
    }
}

impl From<UsageSnapshot> for SafeUsageSnapshot {
    fn from(usage: UsageSnapshot) -> Self {
        Self {
            requests: usage.requests,
            prompt_tokens: usage.prompt_tokens,
            completion_tokens: usage.completion_tokens,
            total_tokens: usage.total_tokens,
            conversation_tokens: usage.conversation_tokens,
            cache_read_tokens: usage.cache_read_tokens,
            cache_write_tokens: usage.cache_write_tokens,
            reasoning_tokens: usage.reasoning_tokens,
            last_usage: usage.last_usage,
            last_conversation_usage: usage.last_conversation_usage,
        }
    }
}

fn redact_internal_assistant_text(value: &str) -> String {
    value
        .replace(crate::state::pending_placeholder(), "")
        .replace(crate::state::interrupted_text(), "")
}

fn normalize_answers(
    request: &QuestionRequest,
    mut answers: QuestionAnswers,
) -> std::result::Result<QuestionAnswers, String> {
    for answer in &mut answers {
        for value in answer {
            *value = value.trim().to_string();
            if value.chars().any(char::is_control) {
                return Err("answers cannot contain control characters".to_string());
            }
        }
    }
    question::validate_answers(request, &answers).map_err(|error| safe_error_message(&error))?;
    Ok(answers)
}

pub(crate) fn validate_content(content: String) -> std::result::Result<String, ApiError> {
    let content = content.trim().to_string();
    if content.is_empty() {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "content cannot be empty",
        ));
    }
    if content.chars().count() > MAX_CONTENT_CHARS {
        return Err(ApiError::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            format!("content cannot exceed {MAX_CONTENT_CHARS} characters"),
        ));
    }
    if content
        .chars()
        .any(|character| character.is_control() && !matches!(character, '\n' | '\r' | '\t'))
    {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "content contains unsupported control characters",
        ));
    }
    Ok(content)
}

fn validate_short_field(
    value: String,
    field: &str,
    max_chars: usize,
) -> std::result::Result<String, ApiError> {
    let value = value.trim().to_string();
    if value.is_empty() {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            format!("{field} cannot be empty"),
        ));
    }
    if value.chars().count() > max_chars || value.chars().any(char::is_control) {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            format!("{field} is invalid"),
        ));
    }
    Ok(value)
}

fn validate_model_selection(
    models: Vec<ActiveProviderModelConfig>,
) -> std::result::Result<Vec<ActiveProviderModelConfig>, ApiError> {
    if models.is_empty() {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "at least one model endpoint must remain active",
        ));
    }
    if models.len() > 64 {
        return Err(ApiError::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            "at most 64 model endpoints can be active",
        ));
    }
    let mut seen = HashSet::with_capacity(models.len());
    let mut validated = Vec::with_capacity(models.len());
    for model in models {
        let provider_id = validate_short_field(model.provider_id, "provider_id", 200)?;
        let model = validate_short_field(model.model, "model", 500)?;
        if !seen.insert((provider_id.clone(), model.clone())) {
            return Err(ApiError::new(
                StatusCode::BAD_REQUEST,
                "duplicate provider/model selection",
            ));
        }
        validated.push(ActiveProviderModelConfig { provider_id, model });
    }
    Ok(validated)
}

fn validate_thinking_variant_updates(
    updates: Vec<ThinkingVariantUpdate>,
) -> std::result::Result<Vec<ThinkingVariantUpdate>, ApiError> {
    if updates.is_empty() {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "at least one thinking variant update is required",
        ));
    }
    if updates.len() > MAX_THINKING_VARIANT_UPDATES {
        return Err(ApiError::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            format!("at most {MAX_THINKING_VARIANT_UPDATES} thinking variants can be updated"),
        ));
    }

    let mut seen = HashSet::with_capacity(updates.len());
    let mut validated = Vec::with_capacity(updates.len());
    for update in updates {
        let provider_id = validate_short_field(update.provider_id, "provider_id", 200)?;
        let model = validate_short_field(update.model, "model", 500)?;
        let selected = update
            .selected
            .map(|selected| validate_short_field(selected, "selected", 200))
            .transpose()?;
        if !seen.insert((provider_id.clone(), model.clone())) {
            return Err(ApiError::new(
                StatusCode::BAD_REQUEST,
                "duplicate provider/model thinking variant update",
            ));
        }
        validated.push(ThinkingVariantUpdate {
            provider_id,
            model,
            selected,
        });
    }
    Ok(validated)
}

fn parse_mode(mode: &str) -> std::result::Result<AgentMode, ApiError> {
    match mode {
        "normal" => Ok(AgentMode::Normal),
        "plan" => Ok(AgentMode::Plan),
        "chat" => Ok(AgentMode::Chat),
        _ => Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "mode must be normal, plan, or chat",
        )),
    }
}

fn mode_name(mode: AgentMode) -> &'static str {
    match mode {
        AgentMode::Normal => "normal",
        AgentMode::Plan => "plan",
        AgentMode::Chat => "chat",
    }
}

fn is_local_webui_request(audience: PromptAudience, has_turn_profile: bool) -> bool {
    audience == PromptAudience::External && !has_turn_profile
}

fn real_tool_name(event_name: &str) -> &str {
    if event_name.starts_with("load_skill:") {
        "load_skill"
    } else if event_name.starts_with("load_tools:") {
        "load_tools"
    } else {
        event_name
    }
}

fn require_auth(headers: &HeaderMap, state: &DaemonState) -> std::result::Result<(), ApiError> {
    if state
        .auth
        .is_authenticated(cookie_value(headers, AUTH_COOKIE))
    {
        Ok(())
    } else {
        Err(ApiError::new(
            StatusCode::UNAUTHORIZED,
            "authentication required",
        ))
    }
}

fn require_mutation(headers: &HeaderMap, state: &DaemonState) -> std::result::Result<(), ApiError> {
    require_auth(headers, state)?;
    if origin_is_allowed(headers) {
        Ok(())
    } else {
        Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "request origin is not allowed",
        ))
    }
}

fn cookie_value<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    for header in headers.get_all(COOKIE) {
        let Ok(header) = header.to_str() else {
            continue;
        };
        for pair in header.split(';') {
            let Some((key, value)) = pair.trim().split_once('=') else {
                continue;
            };
            if key.trim() == name {
                return Some(value.trim());
            }
        }
    }
    None
}

fn origin_is_allowed(headers: &HeaderMap) -> bool {
    let mut origins = headers.get_all(ORIGIN).iter();
    let Some(origin) = origins.next() else {
        return true;
    };
    if origins.next().is_some() {
        return false;
    }
    let Some(host) = headers.get(HOST).and_then(|host| host.to_str().ok()) else {
        return false;
    };
    let expected = format!("http://{host}");
    origin.to_str().is_ok_and(|origin| origin == expected)
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    let mut difference = left.len() ^ right.len();
    let length = left.len().max(right.len());
    for index in 0..length {
        let left = left.get(index).copied().unwrap_or(0);
        let right = right.get(index).copied().unwrap_or(0);
        difference |= usize::from(left ^ right);
    }
    difference == 0
}

fn random_token(bytes: usize) -> String {
    let mut buffer = vec![0u8; bytes];
    OsRng.fill_bytes(&mut buffer);
    URL_SAFE_NO_PAD.encode(buffer)
}

pub(crate) fn random_id(prefix: &str, bytes: usize) -> String {
    format!("{prefix}_{}", random_token(bytes))
}

pub(crate) fn safe_error_message(error: impl std::fmt::Display) -> String {
    let message = error
        .to_string()
        .chars()
        .filter(|character| !character.is_control() || *character == '\n' || *character == '\t')
        .take(1000)
        .collect::<String>();
    if message.trim().is_empty() {
        "operation failed".to_string()
    } else {
        message
    }
}

async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::question::{QuestionOption, QuestionPrompt};
    use crate::state::PlatformSessionBindingKey;

    #[test]
    fn artifact_tools_are_scoped_to_local_webui_requests() {
        assert!(is_local_webui_request(PromptAudience::External, false));
        assert!(!is_local_webui_request(PromptAudience::Owner, false));
        assert!(!is_local_webui_request(PromptAudience::External, true));
    }

    fn test_paths(root: &FilePath) -> MiyuPaths {
        MiyuPaths {
            root_dir: root.to_path_buf(),
            config_dir: root.join("config"),
            config_file: root.join("config/config.jsonc"),
            skills_dir: root.join("config/skills"),
            data_dir: root.join("data"),
            cache_dir: root.join("cache"),
            state_dir: root.join("state"),
            pictures_dir: root.join("pictures"),
            fish_hook_file: root.join("fish"),
            bash_hook_file: root.join("bash"),
            zsh_hook_file: root.join("zsh"),
            scripts_dir: root.join("scripts"),
            system_scripts_dir: root.join("system-scripts"),
        }
    }

    #[test]
    fn managed_persona_assets_use_the_resource_directory_and_reject_escape() {
        let temp = tempfile::tempdir().unwrap();
        let mut paths = test_paths(temp.path());
        paths.skills_dir = paths.data_dir.join("skills");
        paths.scripts_dir = paths.data_dir.join("scripts");

        assert_eq!(
            managed_persona_asset_path(&paths, "persona-avatars/avatar.png"),
            Some(paths.data_dir.join("persona-avatars/avatar.png"))
        );
        assert!(managed_persona_asset_path(&paths, "/etc/passwd").is_none());
        assert!(managed_persona_asset_path(&paths, "persona-avatars/../secret").is_none());
        assert_eq!(
            managed_persona_asset_path(&paths, "persona-avatars/nested/file.png"),
            Some(paths.data_dir.join("persona-avatars/nested/file.png"))
        );
        assert_eq!(
            resolve_persona_asset_path(&paths, "./persona-avatars/avatar.png"),
            Some(paths.data_dir.join("persona-avatars/avatar.png"))
        );
        assert!(resolve_persona_asset_path(&paths, "persona-avatars/../../secret").is_none());
        assert_eq!(
            resolve_persona_asset_path(&paths, "avatars/custom.png"),
            Some(paths.config_dir.join("avatars/custom.png"))
        );
        assert_eq!(
            resolve_persona_asset_path(&paths, "scripts/images/custom.png"),
            Some(paths.data_dir.join("scripts/images/custom.png"))
        );
        assert_eq!(
            resolve_persona_asset_path(
                &paths,
                &paths
                    .config_dir
                    .join("persona-avatars/absolute.png")
                    .display()
                    .to_string(),
            ),
            Some(paths.data_dir.join("persona-avatars/absolute.png"))
        );
    }

    #[tokio::test]
    async fn persona_asset_store_is_atomic_and_rejects_corrupt_cache_entries() {
        let temp = tempfile::tempdir().unwrap();
        let directory = temp.path().join("persona-avatars");
        std::fs::create_dir_all(&directory).unwrap();
        let body = b"persona asset";
        let hash = format!("{:x}", Sha256::digest(body));
        let destination = directory.join(format!("{hash}.png"));

        store_persona_asset(&directory, &destination, &hash, body)
            .await
            .unwrap();
        store_persona_asset(&directory, &destination, &hash, body)
            .await
            .unwrap();
        assert_eq!(std::fs::read(&destination).unwrap(), body);

        std::fs::write(&destination, b"corrupt").unwrap();
        store_persona_asset(&directory, &destination, &hash, body)
            .await
            .unwrap();
        assert_eq!(std::fs::read(&destination).unwrap(), body);
    }

    #[test]
    fn persona_asset_cleanup_normalizes_managed_reference_paths() {
        fn prompts(path: String) -> PromptDocuments {
            PromptDocuments {
                personas: vec![PromptDocument {
                    name: "Persona.md".to_string(),
                    content: String::new(),
                    avatar_path: Some(path),
                    board_image_path: None,
                    board_title: None,
                    board_subtitle: None,
                    starter_prompts: None,
                    original_name: None,
                }],
                identities: Vec::new(),
            }
        }

        let temp = tempfile::tempdir().unwrap();
        let mut paths = test_paths(temp.path());
        paths.skills_dir = paths.data_dir.join("skills");
        let directory = paths.persona_avatars_dir();
        std::fs::create_dir_all(&directory).unwrap();
        let name = format!("{}.png", "a".repeat(64));
        let asset = directory.join(&name);
        std::fs::write(&asset, "image").unwrap();

        cleanup_persona_assets(
            &paths,
            &prompts(format!("persona-avatars/{name}")),
            &prompts(format!("./persona-avatars/{name}")),
        );
        assert!(asset.is_file());
    }

    #[test]
    fn system_prompt_resource_is_not_exposed_as_a_persona_document() {
        let temp = tempfile::tempdir().unwrap();
        let mut paths = test_paths(temp.path());
        paths.skills_dir = paths.data_dir.join("skills");
        let prompts = paths.prompts_dir();
        std::fs::create_dir_all(&prompts).unwrap();
        std::fs::write(prompts.join("system-prompt.md"), "fallback").unwrap();
        std::fs::write(prompts.join("Persona.md"), "persona").unwrap();

        let documents = read_prompt_documents(&AppConfig::default(), &paths).unwrap();
        assert_eq!(documents.personas.len(), 1);
        assert_eq!(documents.personas[0].name, "Persona.md");
    }

    #[cfg(unix)]
    #[test]
    fn managed_persona_asset_validation_rejects_symlink_escape() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let mut paths = test_paths(temp.path());
        paths.skills_dir = paths.data_dir.join("skills");
        let directory = paths.persona_avatars_dir();
        std::fs::create_dir_all(&directory).unwrap();
        let outside = temp.path().join("outside.png");
        std::fs::write(&outside, "image").unwrap();
        let managed = directory.join("avatar.png");
        symlink(&outside, &managed).unwrap();

        assert!(validate_managed_persona_asset_file(&paths, &managed).is_err());
    }

    fn test_daemon_with_actor(
        root: &FilePath,
    ) -> (DaemonState, std::thread::JoinHandle<Result<()>>) {
        DaemonState::for_test_with_actor(test_paths(root), 8300).unwrap()
    }

    #[tokio::test]
    async fn one_shot_sessions_are_mintable_runnable_and_deletable_but_nothing_else() {
        let temp = tempfile::tempdir().unwrap();
        let state = DaemonState::for_test(test_paths(temp.path()), 8300).unwrap();
        let persona = active_persona_scope(&state);
        state
            .state_store
            .adopt_sessions_for_persona(&persona)
            .unwrap();
        let terminal = state.state_store.session_id().to_string();

        let data = handle_session_command(
            &state,
            IpcCommand::CreateSession {
                name: Some("一次性对话".to_string()),
                switch: false,
                kind: Some(crate::state::ASK_SESSION_KIND.to_string()),
            },
        )
        .await
        .unwrap();
        let ask_id = data["session"]["session_id"].as_str().unwrap().to_string();

        // Minting it must not move the terminal lane, and it must not surface
        // in the session list.
        assert_eq!(&*state.state_store.session_id(), terminal.as_str());
        let listed = handle_session_command(
            &state,
            IpcCommand::ListSessions {
                include_archived: true,
            },
        )
        .await
        .unwrap();
        assert!(listed["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .all(|session| session["session_id"] != ask_id.as_str()));

        // A turn may target it; switching to it may not.
        assert_eq!(
            resolve_turn_session(&state, Some(ask_id.clone())).unwrap(),
            ask_id.clone().into()
        );
        assert!(handle_session_command(
            &state,
            IpcCommand::SwitchSession {
                target: ipc::SessionRef::Id { id: ask_id.clone() },
            },
        )
        .await
        .is_err());

        // Other kinds are not mintable over IPC, and `ask` may not be created
        // as the session to switch into.
        assert!(handle_session_command(
            &state,
            IpcCommand::CreateSession {
                name: None,
                switch: false,
                kind: Some("subagent".to_string()),
            },
        )
        .await
        .is_err());
        assert!(handle_session_command(
            &state,
            IpcCommand::CreateSession {
                name: None,
                switch: true,
                kind: Some(crate::state::ASK_SESSION_KIND.to_string()),
            },
        )
        .await
        .is_err());

        // Deleting it is the teardown a one-shot turn performs.
        handle_session_command(
            &state,
            IpcCommand::DeleteSession {
                target: ipc::SessionRef::Id { id: ask_id.clone() },
            },
        )
        .await
        .unwrap();
        assert!(state.state_store.session_record(&ask_id).unwrap().is_none());
        assert!(resolve_turn_session(&state, Some(ask_id)).is_err());
    }

    #[tokio::test]
    async fn repl_session_lane_resumes_and_heals_without_moving_the_terminal_lane() {
        let temp = tempfile::tempdir().unwrap();
        let state = DaemonState::for_test(test_paths(temp.path()), 8300).unwrap();
        let persona = active_persona_scope(&state);
        state
            .state_store
            .adopt_sessions_for_persona(&persona)
            .unwrap();
        let terminal = state.state_store.session_id().to_string();
        let repl = state
            .state_store
            .create_session(&persona, "repl lane", crate::state::USER_SESSION_KIND, None)
            .unwrap();

        handle_session_command(
            &state,
            IpcCommand::SetReplSession {
                target: ipc::SessionRef::Id {
                    id: repl.session_id.clone(),
                },
            },
        )
        .await
        .unwrap();
        assert_eq!(
            state.state_store.repl_session(&persona).unwrap().as_deref(),
            Some(repl.session_id.as_str())
        );
        assert_eq!(&*state.state_store.session_id(), terminal.as_str());

        // A deleted REPL session must not strand the next REPL: the pointer
        // falls back to the terminal session and is healed in place.
        state.state_store.delete_session(&repl.session_id).unwrap();
        assert!(state.state_store.repl_session(&persona).unwrap().is_none());

        // One-shot sessions are not a valid REPL lane either.
        let ask = state
            .state_store
            .create_session(&persona, "一次性对话", crate::state::ASK_SESSION_KIND, None)
            .unwrap();
        assert!(handle_session_command(
            &state,
            IpcCommand::SetReplSession {
                target: ipc::SessionRef::Id {
                    id: ask.session_id.clone(),
                },
            },
        )
        .await
        .is_err());
    }

    #[tokio::test]
    async fn ipc_session_list_excludes_platform_owned_sessions() {
        let temp = tempfile::tempdir().unwrap();
        let state = DaemonState::for_test(test_paths(temp.path()), 8300).unwrap();
        let persona = active_persona_scope(&state);
        state
            .state_store
            .adopt_sessions_for_persona(&persona)
            .unwrap();
        let local = state
            .state_store
            .create_session(&persona, "local", "user", None)
            .unwrap();
        let platform = state
            .state_store
            .create_session(&persona, "platform", "user", None)
            .unwrap();
        state
            .state_store
            .bind_platform_session(
                &PlatformSessionBindingKey {
                    platform: "onebot".to_string(),
                    account_id: "10000".to_string(),
                    conversation_kind: "group".to_string(),
                    conversation_id: "20000".to_string(),
                    participant_id: None,
                    persona: persona.clone(),
                },
                &platform.session_id,
            )
            .unwrap();

        let data = handle_session_command(
            &state,
            IpcCommand::ListSessions {
                include_archived: false,
            },
        )
        .await
        .unwrap();
        let ids = data["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|session| session["session_id"].as_str())
            .collect::<Vec<_>>();

        assert!(ids.contains(&local.session_id.as_str()));
        assert!(!ids.contains(&platform.session_id.as_str()));
    }

    #[test]
    fn target_session_state_does_not_move_the_default_session() {
        let temp = tempfile::tempdir().unwrap();
        let state = DaemonState::for_test(test_paths(temp.path()), 8300).unwrap();
        let persona = active_persona_scope(&state);
        state
            .state_store
            .adopt_sessions_for_persona(&persona)
            .unwrap();
        let default_session_id = state.state_store.session_id();
        let local = state
            .state_store
            .create_session(&persona, "repl local", "user", None)
            .unwrap();

        let snapshot = session_state_for(&state, &local.session_id).unwrap();

        assert_eq!(snapshot.session_id, local.session_id);
        assert_eq!(&*state.state_store.session_id(), &*default_session_id);
    }

    #[tokio::test]
    async fn creating_a_repl_session_does_not_move_the_default_session() {
        let temp = tempfile::tempdir().unwrap();
        let state = DaemonState::for_test(test_paths(temp.path()), 8300).unwrap();
        let persona = active_persona_scope(&state);
        state
            .state_store
            .adopt_sessions_for_persona(&persona)
            .unwrap();
        let default_session_id = state.state_store.session_id();

        let data = handle_session_command(
            &state,
            IpcCommand::CreateSession {
                name: Some("repl local".to_string()),
                switch: false,
                kind: None,
            },
        )
        .await
        .unwrap();

        assert_ne!(
            data["session"]["session_id"].as_str(),
            Some(default_session_id.as_ref())
        );
        assert_eq!(&*state.state_store.session_id(), &*default_session_id);
    }

    #[tokio::test]
    async fn actor_undo_is_scoped_to_the_requested_session() {
        let temp = tempfile::tempdir().unwrap();
        let (state, actor_join) = test_daemon_with_actor(temp.path());
        let persona = active_persona_scope(&state);
        state
            .state_store
            .adopt_sessions_for_persona(&persona)
            .unwrap();
        let default_session_id = state.state_store.session_id();
        let default_store = state.state_store.pinned(&default_session_id);
        default_store
            .start_turn("default-turn", "default", std::process::id())
            .unwrap();
        default_store
            .complete_turn("default-turn", "default reply", None)
            .unwrap();
        let local = state
            .state_store
            .create_session(&persona, "repl local", "user", None)
            .unwrap();
        let local_store = state.state_store.pinned(&local.session_id);
        local_store
            .start_turn("local-turn", "local", std::process::id())
            .unwrap();
        local_store
            .complete_turn("local-turn", "local reply", None)
            .unwrap();

        let (reply, receiver) = oneshot::channel();
        state
            .actor_tx
            .send(ActorCommand::Undo {
                session_id: local.session_id.clone().into(),
                reply,
            })
            .unwrap();
        receiver.await.unwrap().unwrap();

        assert!(local_store.load_turns().unwrap().is_empty());
        assert_eq!(default_store.load_turns().unwrap().len(), 1);
        assert_eq!(&*state.state_store.session_id(), &*default_session_id);
        state.actor_tx.send(ActorCommand::Shutdown).unwrap();
        actor_join.join().unwrap().unwrap();
    }

    #[test]
    fn local_session_resolution_rejects_platform_ids_and_prefers_local_names() {
        let temp = tempfile::tempdir().unwrap();
        let state = DaemonState::for_test(test_paths(temp.path()), 8300).unwrap();
        let persona = active_persona_scope(&state);
        state
            .state_store
            .adopt_sessions_for_persona(&persona)
            .unwrap();
        let local = state
            .state_store
            .create_session(&persona, "shared", "user", None)
            .unwrap();
        let platform = state
            .state_store
            .create_session(&persona, "shared", "user", None)
            .unwrap();
        state
            .state_store
            .bind_platform_session(
                &PlatformSessionBindingKey {
                    platform: "onebot".to_string(),
                    account_id: "10000".to_string(),
                    conversation_kind: "private".to_string(),
                    conversation_id: "20000".to_string(),
                    participant_id: Some("20000".to_string()),
                    persona,
                },
                &platform.session_id,
            )
            .unwrap();

        let resolved = resolve_local_session_ref(
            &state,
            &ipc::SessionRef::Name {
                name: "SHARED".to_string(),
            },
        )
        .unwrap();
        assert_eq!(resolved.session_id, local.session_id);
        assert!(resolve_local_session_ref(
            &state,
            &ipc::SessionRef::Id {
                id: platform.session_id,
            },
        )
        .is_err());
    }

    #[test]
    fn attachment_validation_accepts_utf8_code_and_rejects_unknown_binary() {
        let (kind, mime, width, height) =
            inspect_user_attachment("main.rs", b"fn main() {}\n").unwrap();
        assert_eq!(kind, "text");
        assert_eq!(mime, "text/plain");
        assert_eq!((width, height), (0, 0));
        assert!(inspect_user_attachment("payload.bin", &[0xff, 0xfe, 0xfd]).is_err());
        assert!(inspect_user_attachment("notes.exe", b"plain text").is_err());
    }

    #[test]
    fn attachment_download_header_preserves_utf8_filename() {
        let value = attachment_content_disposition("报告 1.md", false)
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();
        assert!(value.starts_with("attachment;"));
        assert!(value.contains("filename*=UTF-8''%E6%8A%A5%E5%91%8A%201.md"));
    }

    #[tokio::test]
    async fn config_reload_applies_disk_config() {
        let temp = tempfile::tempdir().unwrap();
        let (state, actor_join) = test_daemon_with_actor(temp.path());
        let mut next_config = state.manager.lock().unwrap().config.clone();
        next_config.display.show_token_usage = !next_config.display.show_token_usage;
        let expected = next_config.display.show_token_usage;
        next_config.save(&state.paths).unwrap();

        let (mut client, server) = tokio::net::UnixStream::pair().unwrap();
        let server_state = state.clone();
        let task = tokio::spawn(async move { handle_ipc_connection(server_state, server).await });
        ipc::send(&mut client, &IpcRequest::new(IpcCommand::ReloadConfig))
            .await
            .unwrap();
        let response = ipc::receive::<IpcFrame>(&mut client)
            .await
            .unwrap()
            .unwrap();

        assert!(matches!(response, IpcFrame::AdminResult { .. }));
        task.await.unwrap().unwrap();
        let manager = state.manager.lock().unwrap();
        assert_eq!(manager.config.display.show_token_usage, expected);
        assert!(!manager.admin_busy);
        drop(manager);

        state.actor_tx.send(ActorCommand::Shutdown).unwrap();
        actor_join.join().unwrap().unwrap();
    }

    #[tokio::test]
    async fn failed_config_reload_preserves_the_candidate_file() {
        let temp = tempfile::tempdir().unwrap();
        let state = DaemonState::for_test(test_paths(temp.path()), 8300).unwrap();
        let runtime_value = state
            .manager
            .lock()
            .unwrap()
            .config
            .display
            .show_token_usage;
        let mut candidate = state.manager.lock().unwrap().config.clone();
        candidate.display.show_token_usage = !runtime_value;
        candidate.save(&state.paths).unwrap();

        let (mut client, server) = tokio::net::UnixStream::pair().unwrap();
        let server_state = state.clone();
        let task = tokio::spawn(async move { handle_ipc_connection(server_state, server).await });
        ipc::send(&mut client, &IpcRequest::new(IpcCommand::ReloadConfig))
            .await
            .unwrap();
        let response = ipc::receive::<IpcFrame>(&mut client)
            .await
            .unwrap()
            .unwrap();

        assert!(matches!(
            response,
            IpcFrame::Error {
                code: None,
                message,
            } if message.contains("worker is unavailable")
        ));
        task.await.unwrap().unwrap();
        assert_eq!(
            AppConfig::load(&state.paths)
                .unwrap()
                .display
                .show_token_usage,
            !runtime_value
        );
        let manager = state.manager.lock().unwrap();
        assert_eq!(manager.config.display.show_token_usage, runtime_value);
        assert!(!manager.admin_busy);
    }

    #[tokio::test]
    async fn busy_config_reload_returns_an_error_frame() {
        let temp = tempfile::tempdir().unwrap();
        let (state, actor_join) = test_daemon_with_actor(temp.path());
        state
            .manager
            .lock()
            .unwrap()
            .config
            .save(&state.paths)
            .unwrap();
        // Running turns no longer block a reload (they keep their own config
        // snapshot); only a concurrent admin operation does.
        state.manager.lock().unwrap().admin_busy = true;

        let (mut client, server) = tokio::net::UnixStream::pair().unwrap();
        let server_state = state.clone();
        let task = tokio::spawn(async move { handle_ipc_connection(server_state, server).await });
        ipc::send(&mut client, &IpcRequest::new(IpcCommand::ReloadConfig))
            .await
            .unwrap();
        let response = ipc::receive::<IpcFrame>(&mut client)
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(
            response,
            IpcFrame::Error {
                code: Some(ipc::ErrorCode::Busy),
                message,
            } if message.contains("busy with another operation")
        ));
        task.await.unwrap().unwrap();

        state.manager.lock().unwrap().admin_busy = false;
        state.actor_tx.send(ActorCommand::Shutdown).unwrap();
        actor_join.join().unwrap().unwrap();
    }

    #[tokio::test]
    async fn config_reload_succeeds_and_keeps_turns_running() {
        let temp = tempfile::tempdir().unwrap();
        let (state, actor_join) = test_daemon_with_actor(temp.path());
        state
            .manager
            .lock()
            .unwrap()
            .config
            .save(&state.paths)
            .unwrap();
        let (cancel, cancel_rx) = tokio::sync::watch::channel(false);
        state.manager.lock().unwrap().active_runs.insert(
            "hot-reload-run".to_string(),
            RunInfo {
                session_id: state.state_store.session_id().into(),
                mode: AgentMode::Normal,
                audience: PromptAudience::External,
                cancel,
                turn_id: None,
                queue_target: None,
                supersede: Arc::new(crate::agent::TurnSupersedeSignal::default()),
                platform_followup: None,
                operation: RunOperation::Create,
                job_wake: false,
                job_wake_label: None,
            },
        );

        let (mut client, server) = tokio::net::UnixStream::pair().unwrap();
        let server_state = state.clone();
        let task = tokio::spawn(async move { handle_ipc_connection(server_state, server).await });
        ipc::send(&mut client, &IpcRequest::new(IpcCommand::ReloadConfig))
            .await
            .unwrap();
        let response = ipc::receive::<IpcFrame>(&mut client)
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(response, IpcFrame::AdminResult { .. }));
        task.await.unwrap().unwrap();

        // A turn-safe reload neither cancels nor waits out the running turn.
        assert!(!*cancel_rx.borrow());
        {
            let manager = state.manager.lock().unwrap();
            assert!(manager.active_runs.contains_key("hot-reload-run"));
            assert!(!manager.admin_busy);
        }

        state.manager.lock().unwrap().active_runs.clear();
        state.actor_tx.send(ActorCommand::Shutdown).unwrap();
        actor_join.join().unwrap().unwrap();
    }

    #[tokio::test]
    async fn set_session_models_ipc_pins_and_clears_the_override() {
        let temp = tempfile::tempdir().unwrap();
        let state = DaemonState::for_test(test_paths(temp.path()), 8300).unwrap();
        let choice = state
            .manager
            .lock()
            .unwrap()
            .config
            .text_provider_model_choices()
            .first()
            .cloned()
            .expect("the default config configures at least one model");
        let persona = active_persona_scope(&state);
        let record = state
            .state_store
            .create_session(&persona, "", "user", None)
            .unwrap();
        let target = ipc::SessionRef::Id {
            id: record.session_id.clone(),
        };

        handle_session_command(
            &state,
            IpcCommand::SetSessionModels {
                target: target.clone(),
                models: vec![crate::config::ActiveProviderModelConfig {
                    provider_id: choice.provider_id.clone(),
                    model: choice.model.clone(),
                }],
            },
        )
        .await
        .unwrap();
        let session_id = record.session_id.clone();
        let stored = state
            .state_store
            .session_model_override(&session_id)
            .unwrap()
            .expect("the override is stored");
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].provider_id, choice.provider_id);
        assert_eq!(stored[0].model, choice.model);

        // Unknown models are rejected and leave the override untouched.
        let error = handle_session_command(
            &state,
            IpcCommand::SetSessionModels {
                target: target.clone(),
                models: vec![crate::config::ActiveProviderModelConfig {
                    provider_id: "no-such-provider".to_string(),
                    model: "no-such-model".to_string(),
                }],
            },
        )
        .await
        .unwrap_err();
        assert!(error.contains("no-such-provider"));
        assert!(state
            .state_store
            .session_model_override(&session_id)
            .unwrap()
            .is_some());

        // An empty list clears the override (follow the global pool again).
        handle_session_command(
            &state,
            IpcCommand::SetSessionModels {
                target,
                models: Vec::new(),
            },
        )
        .await
        .unwrap();
        assert!(state
            .state_store
            .session_model_override(&session_id)
            .unwrap()
            .is_none());
    }

    #[test]
    fn qq_group_history_scope_and_offender_deletion_are_isolated() {
        let temp = tempfile::tempdir().unwrap();
        let store = StateStore::new(&test_paths(temp.path())).unwrap();
        let scope = qq_group_scope("123456", "234567").unwrap();
        store
            .plugin_put_json(
                &scope,
                "offender_history",
                &json!({
                    "345678": { "user_id": "345678", "ban_count": 2 },
                    "456789": { "user_id": "456789", "ban_count": 1 }
                }),
            )
            .unwrap();
        store
            .plugin_update_json::<HashMap<String, Value>, _>(
                &scope,
                "offender_history",
                |current| {
                    let mut records = current.unwrap_or_default();
                    records.remove("345678");
                    Ok(Some(records))
                },
            )
            .unwrap();
        let remaining = store
            .plugin_get_json::<HashMap<String, Value>>(&scope, "offender_history")
            .unwrap()
            .unwrap();
        assert!(!remaining.contains_key("345678"));
        assert!(remaining.contains_key("456789"));
        assert_eq!(scope.platform, "onebot");
        assert_eq!(scope.conversation_kind, "group");
    }

    #[tokio::test]
    async fn platform_session_reset_is_serialized_per_target_session() {
        let temp = tempfile::tempdir().unwrap();
        let (state, actor_join) = test_daemon_with_actor(temp.path());
        let target = state
            .state_store
            .create_session("miyu", "qq target", "user", None)
            .unwrap();
        let other = state
            .state_store
            .create_session("miyu", "other", "user", None)
            .unwrap();
        let target_store = state.state_store.pinned(&target.session_id);
        target_store
            .start_turn("before_reset", "hello", std::process::id())
            .unwrap();
        target_store
            .complete_turn("before_reset", "world", None)
            .unwrap();

        let (other_cancel, _other_cancel_rx) = tokio::sync::watch::channel(false);
        state.manager.lock().unwrap().active_runs.insert(
            "other_run".to_string(),
            RunInfo {
                session_id: other.session_id.clone().into(),
                mode: AgentMode::Normal,
                audience: PromptAudience::Internal,
                cancel: other_cancel,
                turn_id: None,
                queue_target: None,
                supersede: Arc::new(crate::agent::TurnSupersedeSignal::default()),
                platform_followup: None,
                operation: RunOperation::Create,
                job_wake: false,
                job_wake_label: None,
            },
        );
        assert!(
            clear_platform_session_content(&state, target.session_id.clone().into())
                .await
                .is_ok()
        );
        assert!(target_store.load_turns().unwrap().is_empty());
        assert!(!state.manager.lock().unwrap().admin_busy);

        target_store
            .start_turn("must_survive", "still here", std::process::id())
            .unwrap();
        target_store
            .complete_turn("must_survive", "answer", None)
            .unwrap();
        let (target_cancel, _target_cancel_rx) = tokio::sync::watch::channel(false);
        state.manager.lock().unwrap().active_runs.insert(
            "target_run".to_string(),
            RunInfo {
                session_id: target.session_id.clone().into(),
                mode: AgentMode::Normal,
                audience: PromptAudience::External,
                cancel: target_cancel,
                turn_id: None,
                queue_target: None,
                supersede: Arc::new(crate::agent::TurnSupersedeSignal::default()),
                platform_followup: None,
                operation: RunOperation::Create,
                job_wake: false,
                job_wake_label: None,
            },
        );
        assert!(matches!(
            clear_platform_session_content(&state, target.session_id.clone().into()).await,
            Err(PlatformSessionResetError::Busy)
        ));
        assert_eq!(target_store.load_turns().unwrap().len(), 1);
        assert!(!state.manager.lock().unwrap().admin_busy);

        state.manager.lock().unwrap().active_runs.clear();
        target_store
            .start_turn("database_running", "working", std::process::id())
            .unwrap();
        assert!(matches!(
            clear_platform_session_content(&state, target.session_id.clone().into()).await,
            Err(PlatformSessionResetError::Busy)
        ));
        assert!(!state.manager.lock().unwrap().admin_busy);
        target_store.interrupt_turn("database_running").unwrap();

        state.actor_tx.send(ActorCommand::Shutdown).unwrap();
        actor_join.join().unwrap().unwrap();
        assert!(matches!(
            clear_platform_session_content(&state, target.session_id.into()).await,
            Err(PlatformSessionResetError::Unavailable)
        ));
        assert!(!state.manager.lock().unwrap().admin_busy);
    }

    #[test]
    fn startup_repairs_a_platform_owned_current_session() {
        let temp = tempfile::tempdir().unwrap();
        let store = StateStore::new(&test_paths(temp.path())).unwrap();
        store.adopt_sessions_for_persona("miyu").unwrap();
        let qq_session = store
            .create_session("miyu", "QQ group 20000", "user", None)
            .unwrap();
        store
            .bind_platform_session(
                &PlatformSessionBindingKey {
                    platform: "onebot".to_string(),
                    account_id: "10000".to_string(),
                    conversation_kind: "group".to_string(),
                    conversation_id: "20000".to_string(),
                    participant_id: None,
                    persona: "miyu".to_string(),
                },
                &qq_session.session_id,
            )
            .unwrap();
        store.switch_session(&qq_session.session_id).unwrap();

        ensure_local_current_session(&store, "miyu").unwrap();

        let repaired = store.session_id();
        assert_ne!(&*repaired, qq_session.session_id);
        assert!(!store.is_platform_session(&repaired).unwrap());
        assert_eq!(
            store.session_record(&repaired).unwrap().unwrap().persona,
            "miyu"
        );
    }

    #[test]
    fn actor_commands_keep_large_configuration_off_the_inline_queue_item() {
        assert!(std::mem::size_of::<ActorCommand>() <= 512);
    }

    #[test]
    fn prompt_sidecar_reads_avatar_path_without_touching_prompt_content() {
        let temp = tempfile::tempdir().unwrap();
        let prompt = temp.path().join("Alice.md");
        std::fs::write(&prompt, "You are Alice.\n").unwrap();
        std::fs::write(
            temp.path().join("Alice.json"),
            r#"{"avatar_path":"avatars/alice.png","board_image_path":"persona-avatars/board.png","board_title":"欢迎","board_subtitle":"从这里开始","starter_prompts":["天气","问题"]}"#,
        )
        .unwrap();

        let documents = read_prompt_document_dir(temp.path(), true).unwrap();
        assert_eq!(documents.len(), 1);
        assert_eq!(documents[0].name, "Alice.md");
        assert_eq!(documents[0].content, "You are Alice.\n");
        assert_eq!(
            documents[0].avatar_path.as_deref(),
            Some("avatars/alice.png")
        );
        assert_eq!(
            documents[0].board_image_path.as_deref(),
            Some("persona-avatars/board.png")
        );
        assert_eq!(documents[0].board_title.as_deref(), Some("欢迎"));
        assert_eq!(documents[0].starter_prompts.as_ref().map(Vec::len), Some(2));
    }

    #[test]
    fn malformed_prompt_sidecar_falls_back_to_no_avatar() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join("Alice.md"), "prompt").unwrap();
        std::fs::write(temp.path().join("Alice.json"), "not json").unwrap();

        let documents = read_prompt_document_dir(temp.path(), true).unwrap();
        assert_eq!(documents[0].avatar_path, None);
    }

    #[test]
    fn persona_file_mutations_include_avatar_sidecar() {
        let temp = tempfile::tempdir().unwrap();
        let mut mutations = HashMap::new();
        let documents = vec![PromptDocument {
            name: "Alice.md".to_string(),
            content: "prompt".to_string(),
            avatar_path: Some("avatars/alice.png".to_string()),
            board_image_path: None,
            board_title: None,
            board_subtitle: None,
            starter_prompts: None,
            original_name: None,
        }];
        collect_prompt_file_mutations(
            &[],
            &documents,
            temp.path(),
            temp.path(),
            &mut mutations,
            true,
        );

        let metadata = mutations
            .get(&temp.path().join("Alice.json"))
            .and_then(Option::as_deref)
            .unwrap();
        let metadata: Value = serde_json::from_slice(metadata).unwrap();
        assert_eq!(metadata["avatar_path"], "avatars/alice.png");
    }

    #[test]
    fn persona_identity_uses_default_and_custom_values() {
        let mut config = AppConfig::default();
        let prompts = PromptDocuments::default();
        let default = persona_identity(&config, &prompts);
        assert_eq!(default.name, "Miyu");
        assert_eq!(default.avatar_url.as_deref(), Some("/assets/miyu-logo.png"));

        config.prompt.active_persona = "Alice.md".to_string();
        let prompts = PromptDocuments {
            personas: vec![PromptDocument {
                name: "Alice.md".to_string(),
                content: "prompt".to_string(),
                avatar_path: Some("avatars/alice.png".to_string()),
                board_image_path: None,
                board_title: None,
                board_subtitle: None,
                starter_prompts: None,
                original_name: None,
            }],
            identities: Vec::new(),
        };
        let custom = persona_identity(&config, &prompts);
        assert_eq!(custom.name, "Alice");
        assert_eq!(custom.avatar_url.as_deref(), Some("/api/persona/avatar"));
    }

    #[test]
    fn sanitize_session_title_cleans_llm_output() {
        assert_eq!(sanitize_session_title("「东京天气查询」"), "东京天气查询");
        assert_eq!(
            sanitize_session_title("\"Arch Linux 新闻\"\n第二行忽略"),
            "Arch Linux 新闻"
        );
        assert_eq!(sanitize_session_title("  标题。  "), "标题");
        assert_eq!(sanitize_session_title(""), "");
        // Overlong output clips to 20 chars.
        let long = "很长的标题".repeat(10);
        assert_eq!(sanitize_session_title(&long).chars().count(), 20);
    }

    fn manager_with_run(
        run_id: &str,
    ) -> (Arc<Mutex<ManagerState>>, tokio::sync::watch::Receiver<bool>) {
        let (cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
        let manager = Arc::new(Mutex::new(ManagerState {
            config: AppConfig::default(),
            active_runs: HashMap::from([(
                run_id.to_string(),
                RunInfo {
                    session_id: "default".into(),
                    mode: AgentMode::Normal,
                    audience: PromptAudience::Owner,
                    cancel: cancel_tx,
                    turn_id: None,
                    queue_target: None,
                    supersede: Arc::new(crate::agent::TurnSupersedeSignal::default()),
                    platform_followup: None,
                    operation: RunOperation::Create,
                    job_wake: false,
                job_wake_label: None,
                },
            )]),
            admin_busy: false,
            context: ContextSnapshot {
                tokens: 0,
                window: None,
                cumulative_tokens: 0,
                cumulative_prompt_tokens: 0,
                cumulative_cache_read_tokens: 0,
            },
            persona_session_ids: HashMap::new(),
        }));
        (manager, cancel_rx)
    }

    #[test]
    fn active_turn_queue_never_crosses_prompt_audiences() {
        let (manager, _cancel_rx) = manager_with_run("owner_run");
        let manager = manager.lock().unwrap();

        assert!(manager.session_runs_match_audience("default", PromptAudience::Owner));
        assert!(!manager.session_runs_match_audience("default", PromptAudience::External));
        assert!(!manager.session_runs_match_audience("missing", PromptAudience::Owner));
    }

    #[test]
    fn light_admin_reservation_allows_running_turns_and_serializes_mutations() {
        let (manager, _cancel_rx) = manager_with_run("active_run");

        assert!(reserve_admin(&manager).is_err());
        assert!(reserve_admin_light(&manager).is_ok());
        assert!(reserve_admin_light(&manager).is_err());
        assert_eq!(manager.lock().unwrap().active_runs.len(), 1);

        release_admin(&manager);
        assert!(reserve_admin_light(&manager).is_ok());
        release_admin(&manager);
    }

    #[test]
    fn turn_updates_are_routed_to_the_exact_run_and_turn() {
        let temp = tempfile::tempdir().unwrap();
        let state = DaemonState::for_test(test_paths(temp.path()), 8300).unwrap();
        let session_id = state.state_store.session_id();
        let first_store = state.state_store.pinned_for_turn(&session_id);
        let second_store = state.state_store.pinned_for_turn(&session_id);
        first_store
            .start_turn("turn-first", "first", std::process::id())
            .unwrap();
        second_store
            .start_turn("turn-second", "second", std::process::id())
            .unwrap();
        let mut manager = state.manager.lock().unwrap();
        for (run_id, turn_id, store) in [
            ("run-first", "turn-first", &first_store),
            ("run-second", "turn-second", &second_store),
        ] {
            let (cancel, _cancel_rx) = tokio::sync::watch::channel(false);
            manager.active_runs.insert(
                run_id.to_string(),
                RunInfo {
                    session_id: session_id.clone(),
                    mode: AgentMode::Normal,
                    audience: PromptAudience::External,
                    cancel,
                    turn_id: Some(turn_id.to_string()),
                    queue_target: Some(store.queue_target(turn_id)),
                    supersede: Arc::new(crate::agent::TurnSupersedeSignal::default()),
                    platform_followup: None,
                    operation: RunOperation::Create,
                    job_wake: false,
                job_wake_label: None,
                },
            );
        }
        drop(manager);

        enqueue_turn_update(
            &state,
            TurnUpdateRequest {
                run_id: "run-first".to_string(),
                turn_id: "turn-first".to_string(),
                session_id: Some(session_id.clone()),
                audience: PromptAudience::External,
                content: "follow first".to_string(),
                display_content: "follow first".to_string(),
                attachments: Vec::new(),
                uploaded_attachment_ids: Vec::new(),
                mode: TurnUpdateMode::Followup,
            },
        )
        .unwrap();

        assert_eq!(first_store.load_queued_prompts().unwrap().len(), 1);
        assert!(second_store.load_queued_prompts().unwrap().is_empty());
        assert!(enqueue_turn_update(
            &state,
            TurnUpdateRequest {
                run_id: "run-first".to_string(),
                turn_id: "turn-second".to_string(),
                session_id: Some(session_id),
                audience: PromptAudience::External,
                content: "wrong target".to_string(),
                display_content: "wrong target".to_string(),
                attachments: Vec::new(),
                uploaded_attachment_ids: Vec::new(),
                mode: TurnUpdateMode::Followup,
            },
        )
        .is_err());
    }

    #[test]
    fn dropped_ipc_turn_cancels_its_core_run() {
        let (manager, cancel_rx) = manager_with_run("run_test");
        drop(IpcRunGuard {
            manager,
            run_id: "run_test".to_string(),
            finished: false,
        });
        assert!(*cancel_rx.borrow());
    }

    #[test]
    fn completed_ipc_turn_does_not_send_a_late_cancel() {
        let (manager, cancel_rx) = manager_with_run("run_test");
        let mut guard = IpcRunGuard {
            manager,
            run_id: "run_test".to_string(),
            finished: false,
        };
        guard.finish();
        drop(guard);
        assert!(!*cancel_rx.borrow());
    }

    #[test]
    fn assistant_sentinels_are_never_exposed() {
        assert_eq!(
            redact_internal_assistant_text(crate::state::pending_placeholder()),
            ""
        );
        assert_eq!(
            redact_internal_assistant_text(crate::state::interrupted_text()),
            ""
        );
        let combined = format!("before {} after", crate::state::interrupted_text());
        let redacted = redact_internal_assistant_text(&combined);
        assert_eq!(redacted, "before  after");
        assert!(!redacted.contains("system-reminder"));
    }

    #[test]
    fn persisted_meme_assets_hide_their_descriptive_caption() {
        let asset = ImageAsset {
            asset_id: "img_test".to_string(),
            turn_id: "turn_test".to_string(),
            tool_id: Some("tool_test".to_string()),
            mime: "image/png".to_string(),
            width: 64,
            height: 64,
            alt: "猫猫 开心 & <得意>".to_string(),
            created_at: "2026-01-01T00:00:00Z".to_string(),
        };
        let reports = vec![
            "<sent_meme>发送了一个表情包：id=sha256:test；description=猫猫 开心 &amp; &lt;得意&gt;</sent_meme>"
                .to_string(),
        ];

        assert!(meme_asset_caption_hidden(&asset, &reports));
        assert!(!meme_asset_caption_hidden(
            &asset,
            &["normal tool output".to_string()]
        ));
    }

    #[test]
    fn cookie_parser_matches_an_exact_cookie_name() {
        let mut headers = HeaderMap::new();
        headers.insert(
            COOKIE,
            HeaderValue::from_static("other=1; miyu_session=secret-token; suffix=2"),
        );
        assert_eq!(cookie_value(&headers, AUTH_COOKIE), Some("secret-token"));
        assert_eq!(cookie_value(&headers, "session"), None);
    }

    #[test]
    fn origin_check_accepts_absent_or_current_host_origin() {
        let mut headers = HeaderMap::new();
        assert!(origin_is_allowed(&headers));
        headers.insert(HOST, HeaderValue::from_static("192.168.1.20:4096"));
        headers.insert(ORIGIN, HeaderValue::from_static("http://127.0.0.1:4096"));
        assert!(!origin_is_allowed(&headers));
        headers.insert(ORIGIN, HeaderValue::from_static("http://192.168.1.20:4096"));
        assert!(origin_is_allowed(&headers));
        headers.append(ORIGIN, HeaderValue::from_static("http://192.168.1.20:4096"));
        assert!(!origin_is_allowed(&headers));
    }

    #[test]
    fn optional_password_auth_issues_server_side_sessions_and_limits_failures() {
        let disabled = WebAuth::new(None);
        assert!(disabled.is_authenticated(None));

        let auth = WebAuth::new(Some("correct horse"));
        let peer = IpAddr::V4(Ipv4Addr::LOCALHOST);
        assert!(!auth.is_authenticated(None));
        assert!(matches!(
            auth.login(peer, "wrong"),
            Err(LoginFailure::Invalid)
        ));
        let token = auth.login(peer, "correct horse").unwrap();
        assert!(auth.is_authenticated(Some(&token)));

        let limited = WebAuth::new(Some("secret"));
        for _ in 0..LOGIN_ATTEMPT_LIMIT {
            assert!(matches!(
                limited.login(peer, "wrong"),
                Err(LoginFailure::Invalid)
            ));
        }
        assert!(matches!(
            limited.login(peer, "secret"),
            Err(LoginFailure::RateLimited)
        ));
    }

    #[test]
    fn model_selection_rejects_empty_and_duplicate_pools() {
        assert!(validate_model_selection(Vec::new()).is_err());
        let model = ActiveProviderModelConfig {
            provider_id: "provider".to_string(),
            model: "model".to_string(),
        };
        assert!(validate_model_selection(vec![model.clone()]).is_ok());
        assert!(validate_model_selection(vec![model.clone(), model]).is_err());
    }

    #[test]
    fn thinking_variant_validation_distinguishes_model_default_and_named_default() {
        let updates = validate_thinking_variant_updates(vec![
            ThinkingVariantUpdate {
                provider_id: " provider ".to_string(),
                model: "model-one".to_string(),
                selected: None,
            },
            ThinkingVariantUpdate {
                provider_id: "provider".to_string(),
                model: "model-two".to_string(),
                selected: Some(" default ".to_string()),
            },
        ])
        .unwrap();
        assert_eq!(updates[0].provider_id, "provider");
        assert_eq!(updates[0].selected, None);
        assert_eq!(updates[1].selected.as_deref(), Some("default"));

        assert!(validate_thinking_variant_updates(vec![
            ThinkingVariantUpdate {
                provider_id: "provider".to_string(),
                model: "model".to_string(),
                selected: None,
            },
            ThinkingVariantUpdate {
                provider_id: " provider ".to_string(),
                model: " model ".to_string(),
                selected: Some("high".to_string()),
            },
        ])
        .is_err());
    }

    #[test]
    fn thinking_variant_updates_validate_before_persisting_and_can_clear_a_selection() {
        let temp = tempfile::tempdir().unwrap();
        let paths = test_paths(temp.path());
        let config = AppConfig::default();
        let choice = config
            .active_provider_model_choices()
            .into_iter()
            .next()
            .unwrap();
        let mut preferences = ThinkingVariantPreferences::load(&paths);
        preferences.set(
            &choice.provider_id,
            &choice.model,
            Some("previous-selection".to_string()),
        );
        preferences.save(&paths).unwrap();

        let mut agent = None;
        let invalid = ThinkingVariantUpdate {
            provider_id: choice.provider_id.clone(),
            model: choice.model.clone(),
            selected: Some("definitely-not-a-real-variant".to_string()),
        };
        assert!(matches!(
            apply_thinking_variant_updates(&mut agent, &config, &paths, &[invalid]),
            Err(AdminFailure::Invalid(_))
        ));
        assert_eq!(
            ThinkingVariantPreferences::load(&paths).selected(&choice.provider_id, &choice.model),
            Some("previous-selection")
        );

        let clear = ThinkingVariantUpdate {
            provider_id: choice.provider_id.clone(),
            model: choice.model.clone(),
            selected: None,
        };
        apply_thinking_variant_updates(&mut agent, &config, &paths, &[clear]).unwrap();
        assert_eq!(
            ThinkingVariantPreferences::load(&paths).selected(&choice.provider_id, &choice.model),
            None
        );
    }

    #[test]
    fn config_response_never_serializes_secret_values() {
        let mut config = AppConfig::default();
        config.providers[0].api_key = Some("provider-secret".to_string());
        config.plugins.web.tavily_api_keys = vec!["tavily-secret".to_string()];
        config.plugins.exchange_rate.api_key = "exchange-secret".to_string();
        config.plugins.image_generation.api_keys = vec!["image-secret".to_string()];
        config.plugins.api_quota.deepseek.api_key = "deepseek-secret".to_string();
        config.plugins.api_quota.openrouter.api_key = "openrouter-secret".to_string();
        let paths = tempfile::tempdir().unwrap();
        let paths = MiyuPaths {
            root_dir: paths.path().to_path_buf(),
            config_dir: paths.path().join("config"),
            config_file: paths.path().join("config/config.jsonc"),
            skills_dir: paths.path().join("config/skills"),
            data_dir: paths.path().join("data"),
            cache_dir: paths.path().join("cache"),
            state_dir: paths.path().join("state"),
            pictures_dir: paths.path().join("pictures"),
            fish_hook_file: paths.path().join("fish"),
            bash_hook_file: paths.path().join("bash"),
            zsh_hook_file: paths.path().join("zsh"),
            scripts_dir: paths.path().join("scripts"),
            system_scripts_dir: paths.path().join("system-scripts"),
        };
        let response = config_response(
            &config,
            ContextSnapshot {
                tokens: 0,
                window: None,
                cumulative_tokens: 0,
                cumulative_prompt_tokens: 0,
                cumulative_cache_read_tokens: 0,
            },
            &paths,
        )
        .unwrap();
        let serialized = serde_json::to_string(&response).unwrap();
        assert!(!serialized.contains("provider-secret"));
        assert!(!serialized.contains("tavily-secret"));
        assert!(!serialized.contains("exchange-secret"));
        assert!(!serialized.contains("image-secret"));
        assert!(!serialized.contains("deepseek-secret"));
        assert!(!serialized.contains("openrouter-secret"));
        assert_eq!(response.secret_states["providers.0.api_key"], true);
        assert_eq!(response.secret_states["plugins.web.tavily_api_keys"], true);
        assert_eq!(
            response.secret_states["plugins.api_quota.deepseek.accounts.0.api_key"],
            true
        );
        assert_eq!(
            response.secret_states["plugins.api_quota.openrouter.accounts.0.api_key"],
            true
        );
        assert!(response.config.get("memory").is_some());
    }

    #[test]
    fn omitted_provider_secret_does_not_follow_array_position_after_rename() {
        let mut current = AppConfig::default();
        current.providers[0].id = "first".to_string();
        current.providers[0].api_key = Some("first-secret".to_string());
        let mut candidate = current.clone();
        candidate.providers[0].id = "renamed".to_string();
        candidate.providers[0].api_key = None;
        restore_config_secrets(&mut candidate, &current, &HashMap::new()).unwrap();
        assert_eq!(candidate.providers[0].api_key, None);
    }

    #[test]
    fn explicit_secret_clear_removes_a_provider_key() {
        let mut current = AppConfig::default();
        current.providers[0].api_key = Some("secret".to_string());
        let mut candidate = current.clone();
        candidate.providers[0].api_key = None;
        let mutations = HashMap::from([("providers.0.api_key".to_string(), SecretMutation::Clear)]);
        restore_config_secrets(&mut candidate, &current, &mutations).unwrap();
        assert_eq!(candidate.providers[0].api_key, None);
    }

    #[test]
    fn api_quota_secrets_are_preserved_set_and_cleared() {
        let mut current = AppConfig::default();
        current.plugins.api_quota.deepseek.api_key = "deepseek-old".to_string();
        current.plugins.api_quota.openrouter.api_key = "openrouter-old".to_string();
        let mut candidate = current.clone();
        candidate.plugins.api_quota.deepseek.accounts =
            vec![crate::config::ApiQuotaAccountConfig {
                id: "account-1".to_string(),
                name: "默认账号".to_string(),
                api_key: String::new(),
            }];
        candidate.plugins.api_quota.openrouter.accounts =
            vec![crate::config::ApiQuotaAccountConfig {
                id: "account-1".to_string(),
                name: "默认账号".to_string(),
                api_key: String::new(),
            }];
        candidate.plugins.api_quota.deepseek.api_key.clear();
        candidate.plugins.api_quota.openrouter.api_key.clear();

        restore_config_secrets(&mut candidate, &current, &HashMap::new()).unwrap();
        assert_eq!(
            candidate.plugins.api_quota.deepseek.accounts[0].api_key,
            "deepseek-old"
        );
        assert_eq!(
            candidate.plugins.api_quota.openrouter.accounts[0].api_key,
            "openrouter-old"
        );

        let mutations = HashMap::from([
            (
                "plugins.api_quota.deepseek.accounts.0.api_key".to_string(),
                SecretMutation::Set("deepseek-new".to_string()),
            ),
            (
                "plugins.api_quota.openrouter.accounts.0.api_key".to_string(),
                SecretMutation::Clear,
            ),
        ]);
        restore_config_secrets(&mut candidate, &current, &mutations).unwrap();
        assert_eq!(
            candidate.plugins.api_quota.deepseek.accounts[0].api_key,
            "deepseek-new"
        );
        assert!(candidate.plugins.api_quota.openrouter.accounts[0]
            .api_key
            .is_empty());
    }

    #[test]
    fn api_quota_account_ids_prevent_deleted_key_reuse() {
        let mut current = AppConfig::default();
        current.plugins.api_quota.deepseek.accounts[0] = crate::config::ApiQuotaAccountConfig {
            id: "old-id".to_string(),
            name: "账号 2".to_string(),
            api_key: "old-secret".to_string(),
        };
        let mut candidate = current.clone();
        candidate.plugins.api_quota.deepseek.accounts[0] = crate::config::ApiQuotaAccountConfig {
            id: "new-id".to_string(),
            name: "账号 2".to_string(),
            api_key: String::new(),
        };

        restore_config_secrets(&mut candidate, &current, &HashMap::new()).unwrap();
        assert!(candidate.plugins.api_quota.deepseek.accounts[0]
            .api_key
            .is_empty());

        candidate.plugins.api_quota.deepseek.accounts[0].id = "old-id".to_string();
        candidate.plugins.api_quota.deepseek.accounts[0].name = "重命名账号".to_string();
        restore_config_secrets(&mut candidate, &current, &HashMap::new()).unwrap();
        assert_eq!(
            candidate.plugins.api_quota.deepseek.accounts[0].api_key,
            "old-secret"
        );
    }

    #[test]
    fn stale_event_cursor_receives_resync_marker() {
        let events = EventHub::new();
        for index in 0..=EVENT_CAPACITY {
            events.publish("test", json!({ "index": index }));
        }
        let replay = events.replay_after(0);
        assert_eq!(replay.len(), 1);
        assert_eq!(replay[0].kind, "resync_required");
        assert_eq!(replay[0].id, events.latest_id());
        let next = events.publish("after-resync", json!({}));
        assert!(next > replay[0].id);
    }

    #[test]
    fn replay_after_cursor_is_ordered_and_exclusive() {
        let events = EventHub::new();
        events.publish("one", json!({}));
        events.publish("two", json!({}));
        events.publish("three", json!({}));
        let replay = events.replay_after(1);
        assert_eq!(
            replay.iter().map(|record| record.id).collect::<Vec<_>>(),
            vec![2, 3]
        );
    }

    #[test]
    fn future_event_cursor_requests_resync_after_server_restart() {
        let events = EventHub::new();
        let replay = events.replay_after(42);
        assert_eq!(replay.len(), 1);
        assert_eq!(replay[0].kind, "resync_required");
    }

    #[test]
    fn answer_validation_trims_values_and_rejects_control_characters() {
        let request = sample_question();
        assert_eq!(
            normalize_answers(&request, vec![vec!["  All  ".to_string()]]).unwrap(),
            vec![vec!["All".to_string()]]
        );
        assert!(normalize_answers(&request, vec![vec!["bad\nanswer".to_string()]]).is_err());
    }

    #[test]
    fn invalid_answer_keeps_question_pending() {
        let broker = QuestionBroker::new();
        let (responder, mut response) = oneshot::channel();
        let question_id = broker.insert("run_test", sample_question(), responder);
        let invalid = broker.answer(&question_id, vec![Vec::new()], |_, _| {
            panic!("invalid answer must not be published")
        });
        assert!(matches!(invalid, Err(AnswerFailure::Invalid(_))));
        assert!(broker.pending.lock().unwrap().contains_key(&question_id));

        broker
            .answer(
                &question_id,
                vec![vec![" All ".to_string()]],
                |run_id, answers| {
                    assert_eq!(run_id, "run_test");
                    assert_eq!(answers, &vec![vec!["All".to_string()]]);
                },
            )
            .unwrap();
        assert!(matches!(
            response.try_recv().unwrap(),
            QuestionResponse::Answered(answers) if answers == vec![vec!["All".to_string()]]
        ));
    }

    #[test]
    fn closed_question_responder_does_not_publish_an_answer() {
        let broker = QuestionBroker::new();
        let (responder, response) = oneshot::channel();
        drop(response);
        let question_id = broker.insert("run_test", sample_question(), responder);
        let mut published = false;
        let result = broker.answer(&question_id, vec![vec!["All".to_string()]], |_, _| {
            published = true
        });
        assert!(matches!(result, Err(AnswerFailure::Gone)));
        assert!(!published);
    }

    #[test]
    fn closing_question_resumes_run_without_answers() {
        let broker = QuestionBroker::new();
        let (responder, mut response) = oneshot::channel();
        let question_id = broker.insert("run_test", sample_question(), responder);
        let mut resumed_run = None;

        broker
            .close(&question_id, |run_id| {
                assert!(response.try_recv().is_err());
                resumed_run = Some(run_id.to_string())
            })
            .unwrap();

        assert_eq!(resumed_run.as_deref(), Some("run_test"));
        assert!(matches!(
            response.try_recv().unwrap(),
            QuestionResponse::Closed
        ));
        assert!(!broker.pending.lock().unwrap().contains_key(&question_id));
    }

    #[test]
    fn closed_question_receiver_does_not_publish_close_event() {
        let broker = QuestionBroker::new();
        let (responder, response) = oneshot::channel();
        drop(response);
        let question_id = broker.insert("run_test", sample_question(), responder);
        let mut published = false;

        let result = broker.close(&question_id, |_| published = true);

        assert!(matches!(result, Err(AnswerFailure::Gone)));
        assert!(!published);
    }

    fn sample_question() -> QuestionRequest {
        QuestionRequest {
            questions: vec![QuestionPrompt {
                header: "Scope".to_string(),
                question: "Which scope?".to_string(),
                options: vec![QuestionOption {
                    label: "All".to_string(),
                    description: String::new(),
                }],
                multiple: false,
                custom: true,
            }],
        }
    }

    #[test]
    fn content_limit_counts_characters() {
        assert!(validate_content("x".repeat(MAX_CONTENT_CHARS)).is_ok());
        let error = validate_content("界".repeat(MAX_CONTENT_CHARS + 1)).unwrap_err();
        assert_eq!(error.status, StatusCode::PAYLOAD_TOO_LARGE);
    }
    #[test]
    fn web_persona_rename_updates_qq_routes_and_deletion_is_rejected() {
        let mut config = AppConfig::default();
        config
            .platforms
            .qq
            .conversations
            .push(crate::config::PlatformModelRoute {
                conversation: crate::config::PlatformConversationConfig {
                    kind: crate::config::PlatformConversationKind::Group,
                    id: "42".to_string(),
                },
                persona: crate::config::PlatformPersonaOverride::Custom {
                    name: "Old.md".to_string(),
                },
                text_models_inheritance: crate::config::PlatformModelPoolInheritance::Platform,
                text_models: None,
                multimodal_models_inheritance:
                    crate::config::PlatformModelPoolInheritance::Platform,
                multimodal_models: None,
                extra_prompt: String::new(),
                session_limits: None,
            });
        let renamed: PromptDocuments = serde_json::from_value(json!({
            "personas": [{
                "name": "New.md",
                "content": "persona",
                "original_name": "Old.md"
            }],
            "identities": []
        }))
        .unwrap();

        reconcile_qq_persona_references(&mut config, &renamed);
        assert_eq!(
            config.platforms.qq.conversations[0].persona.custom_name(),
            Some("New.md")
        );
        assert!(validate_prompt_documents(&config, &renamed).is_ok());
        assert!(validate_prompt_documents(&config, &PromptDocuments::default()).is_err());
    }

    #[test]
    fn web_persona_renames_use_the_original_reference_snapshot() {
        let route = |id: &str, persona: &str| crate::config::PlatformModelRoute {
            conversation: crate::config::PlatformConversationConfig {
                kind: crate::config::PlatformConversationKind::Group,
                id: id.to_string(),
            },
            persona: crate::config::PlatformPersonaOverride::Custom {
                name: persona.to_string(),
            },
            text_models_inheritance: crate::config::PlatformModelPoolInheritance::Platform,
            text_models: None,
            multimodal_models_inheritance: crate::config::PlatformModelPoolInheritance::Platform,
            multimodal_models: None,
            extra_prompt: String::new(),
            session_limits: None,
        };
        let mut config = AppConfig::default();
        config.platforms.qq.conversations = vec![route("1", "A.md"), route("2", "B.md")];
        let prompts: PromptDocuments = serde_json::from_value(json!({
            "personas": [
                {"name": "B.md", "content": "A", "original_name": "A.md"},
                {"name": "C.md", "content": "B", "original_name": "B.md"}
            ],
            "identities": []
        }))
        .unwrap();

        reconcile_qq_persona_references(&mut config, &prompts);

        assert_eq!(
            config.platforms.qq.conversations[0].persona.custom_name(),
            Some("B.md")
        );
        assert_eq!(
            config.platforms.qq.conversations[1].persona.custom_name(),
            Some("C.md")
        );
    }

    #[test]
    fn web_rejects_persona_names_with_colliding_persistent_scopes() {
        let prompts: PromptDocuments = serde_json::from_value(json!({
            "personas": [
                {"name": "A B.md", "content": "first"},
                {"name": "A@B.md", "content": "second"}
            ],
            "identities": []
        }))
        .unwrap();

        assert!(validate_prompt_documents(&AppConfig::default(), &prompts).is_err());
    }

    #[test]
    fn web_persona_scope_batch_migration_supports_swaps() {
        let temp = tempfile::tempdir().unwrap();
        let paths = test_paths(temp.path());
        let store = StateStore::new(&paths).unwrap();
        let first = store.create_session("a", "first", "user", None).unwrap();
        let second = store.create_session("b", "second", "user", None).unwrap();

        migrate_persona_db_scopes(
            &store,
            &[
                ("a".to_string(), "b".to_string()),
                ("b".to_string(), "a".to_string()),
            ],
        )
        .unwrap();

        assert_eq!(
            store
                .session_record(&first.session_id)
                .unwrap()
                .unwrap()
                .persona,
            "b"
        );
        assert_eq!(
            store
                .session_record(&second.session_id)
                .unwrap()
                .unwrap()
                .persona,
            "a"
        );
    }
}
