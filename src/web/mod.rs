use crate::agent::{
    archive_and_delete_visible_turns, Agent, AgentEvent, AgentMode, AgentTurnControl,
};
use crate::args::WebArgs;
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
use crate::question::{self, QuestionAnswers};
// daemon 运行时的共享状态已下沉到 runtime：web 只是它的消费者之一，IPC 与
// 平台适配是另外两个。放在 web 里会让平台层反过来依赖 HTTP 服务。
mod actor;
mod assets;
mod attachments;
mod bridge_progress;
mod bridge_question;
mod commands_api;
mod config_api;
mod dashboards;
mod dto;
mod event_map;
mod goal_driver;
mod persona;
mod prompt_files;
mod qq_history;
mod security;
mod server;
mod session_cmds;
mod sessions;
mod shared_files;
#[cfg(test)]
mod tests;
mod tty;
mod turns;
// 叫 ipc_server 而不是 ipc：`web::ipc` 会把 `crate::ipc` 遮住，本文件里几十处
// `ipc::send` 会突然解析到子模块上——编译期就报，但报错信息（找不到 send）
// 离真正的原因很远。
mod ipc_server;

use actor::*;
use assets::*;
use attachments::*;
use bridge_progress::*;
use bridge_question::*;
use commands_api::*;
use config_api::*;
use dashboards::kb::*;
use dashboards::memes::*;
use dashboards::memory::*;
use dashboards::qq::*;
use dto::*;
use event_map::*;
use goal_driver::*;
use ipc_server::*;
use persona::*;
use prompt_files::*;
use qq_history::*;
use security::*;
pub(crate) use server::run;
use server::*;
use session_cmds::*;
use sessions::*;
use shared_files::*;
use tty::*;
use turns::*;

use crate::runtime::{
    cold_context, enqueue_turn_update, finish_run, random_id, random_token, release_admin,
    reset_platform_persona_state, safe_error_message, validate_content, ActorCommand, AdminFailure,
    AnswerFailure, ApiError, ContextSnapshot, DaemonState, EventHub, EventRecord, IpcRunGuard,
    LoginFailure, ManagerState, PlatformPersonaResetError, PromptDocument, PromptDocuments,
    QuestionBroker, RedoWebPrompt, RunInfo, RunOperation, SafeQueuedPrompt, SafeUserAttachment,
    ThinkingVariantUpdate, TurnEngineState, TurnResourceCache, TurnUpdateMode, TurnUpdateRequest,
    WebAuth,
};
use crate::state::{
    ArtifactAsset, ImageAsset, PlatformPluginScopeKey, QueuedPrompt, StateStore, Turn,
    TurnFollowup, TurnStatus, UsageSnapshot, UserAttachment, USER_ATTACHMENT_KIND_FILE,
    USER_ATTACHMENT_KIND_IMAGE, USER_ATTACHMENT_KIND_TEXT,
};
use crate::tools::build_tool_registry;
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
use base64::Engine;
use futures_util::stream::{self, Stream};
use futures_util::StreamExt;
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
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::sync::{broadcast, mpsc, oneshot, Semaphore};
use tokio::task::JoinHandle as TokioJoinHandle;

use crate::platforms::{self, PlatformRuntime};

const JSON_BODY_LIMIT: usize = 4 * 1024 * 1024;
const PERSONA_ASSET_LIMIT: usize = 8 * 1024 * 1024;
const MAX_PROMPT_DOCUMENT_CHARS: usize = 200_000;
const MAX_PROMPT_DOCUMENTS: usize = 128;

const INDEX_HTML: &str = include_str!("../../web/index.html");
const STYLES_CSS: &str = include_str!("../../web/styles.css");
const APP_JS: &str = include_str!("../../web/app.js");
// 斜杠命令层单独一个文件:app.js 已经 9500 行,再往里长就找不到东西了。
const COMMANDS_JS: &str = include_str!("../../web/commands.js");
const LIGHTBOX_JS: &str = include_str!("../../web/lightbox.js");
const TODOS_JS: &str = include_str!("../../web/todos.js");
// 文件分享面板:独立文件,与 artifact 演示区无关。
const SHARED_JS: &str = include_str!("../../web/shared.js");
// 插件 dashboard 脚本走 assets.rs 的 DASH_SCRIPTS 静态表,加面板只改那一行。
// KaTeX 0.18.4(vendored):公式渲染;字体只带 woff2(css 里 woff2 列首,
// 现代浏览器不会去请求 woff/ttf 回退项)。
const KATEX_JS: &str = include_str!("../../web/vendor/katex/katex.min.js");
const KATEX_CSS: &str = include_str!("../../web/vendor/katex/katex.min.css");
static KATEX_FONTS: &[(&str, &[u8])] = &[
    (
        "KaTeX_AMS-Regular.woff2",
        include_bytes!("../../web/vendor/katex/fonts/KaTeX_AMS-Regular.woff2"),
    ),
    (
        "KaTeX_Caligraphic-Bold.woff2",
        include_bytes!("../../web/vendor/katex/fonts/KaTeX_Caligraphic-Bold.woff2"),
    ),
    (
        "KaTeX_Caligraphic-Regular.woff2",
        include_bytes!("../../web/vendor/katex/fonts/KaTeX_Caligraphic-Regular.woff2"),
    ),
    (
        "KaTeX_Fraktur-Bold.woff2",
        include_bytes!("../../web/vendor/katex/fonts/KaTeX_Fraktur-Bold.woff2"),
    ),
    (
        "KaTeX_Fraktur-Regular.woff2",
        include_bytes!("../../web/vendor/katex/fonts/KaTeX_Fraktur-Regular.woff2"),
    ),
    (
        "KaTeX_Main-Bold.woff2",
        include_bytes!("../../web/vendor/katex/fonts/KaTeX_Main-Bold.woff2"),
    ),
    (
        "KaTeX_Main-BoldItalic.woff2",
        include_bytes!("../../web/vendor/katex/fonts/KaTeX_Main-BoldItalic.woff2"),
    ),
    (
        "KaTeX_Main-Italic.woff2",
        include_bytes!("../../web/vendor/katex/fonts/KaTeX_Main-Italic.woff2"),
    ),
    (
        "KaTeX_Main-Regular.woff2",
        include_bytes!("../../web/vendor/katex/fonts/KaTeX_Main-Regular.woff2"),
    ),
    (
        "KaTeX_Math-BoldItalic.woff2",
        include_bytes!("../../web/vendor/katex/fonts/KaTeX_Math-BoldItalic.woff2"),
    ),
    (
        "KaTeX_Math-Italic.woff2",
        include_bytes!("../../web/vendor/katex/fonts/KaTeX_Math-Italic.woff2"),
    ),
    (
        "KaTeX_SansSerif-Bold.woff2",
        include_bytes!("../../web/vendor/katex/fonts/KaTeX_SansSerif-Bold.woff2"),
    ),
    (
        "KaTeX_SansSerif-Italic.woff2",
        include_bytes!("../../web/vendor/katex/fonts/KaTeX_SansSerif-Italic.woff2"),
    ),
    (
        "KaTeX_SansSerif-Regular.woff2",
        include_bytes!("../../web/vendor/katex/fonts/KaTeX_SansSerif-Regular.woff2"),
    ),
    (
        "KaTeX_Script-Regular.woff2",
        include_bytes!("../../web/vendor/katex/fonts/KaTeX_Script-Regular.woff2"),
    ),
    (
        "KaTeX_Size1-Regular.woff2",
        include_bytes!("../../web/vendor/katex/fonts/KaTeX_Size1-Regular.woff2"),
    ),
    (
        "KaTeX_Size2-Regular.woff2",
        include_bytes!("../../web/vendor/katex/fonts/KaTeX_Size2-Regular.woff2"),
    ),
    (
        "KaTeX_Size3-Regular.woff2",
        include_bytes!("../../web/vendor/katex/fonts/KaTeX_Size3-Regular.woff2"),
    ),
    (
        "KaTeX_Size4-Regular.woff2",
        include_bytes!("../../web/vendor/katex/fonts/KaTeX_Size4-Regular.woff2"),
    ),
    (
        "KaTeX_Typewriter-Regular.woff2",
        include_bytes!("../../web/vendor/katex/fonts/KaTeX_Typewriter-Regular.woff2"),
    ),
];
// 这两张是 `pics/` 里原图的**显示尺寸副本**，不是原图。原图 1254×1254 和
// 3344×1882，而 WebUI 里头像只显示 38/64 px、看板图最大 330×178 px——浏览器
// 解码是按像素数来的，原图会占掉 30 MiB GPU 纹理去画两个缩略图，还让二进制
// 多背 7.2 MiB。降到 256×256 和 1280×720（2x DPR 仍有富余）后纹理 3.7 MiB。
// 原图留在 `pics/` 不动：README、终端演示、外部链接还在引用。
// 重新生成见 `scripts/gen_web_assets.py`。
const MIYU_LOGO: &[u8] = include_bytes!("../../web/assets/miyu-logo.png");
const MIYU_WALLPAPER: &[u8] = include_bytes!("../../web/assets/miyuwallpaper.png");

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

// ── spawn_actor ──

impl DaemonState {
    pub(crate) fn for_test_with_actor(
        paths: MiyuPaths,
        web_port: u16,
    ) -> Result<(Self, std::thread::JoinHandle<Result<()>>)> {
        let state_store = StateStore::new(&paths)?;
        let config = AppConfig::default();
        let context = cold_context(&config, &paths, &state_store)?;
        let manager = Arc::new(Mutex::new(ManagerState {
            config: config.clone(),
            active_runs: HashMap::new(),
            admin_busy: false,
            context,
            persona_session_ids: HashMap::new(),
            runs_changed: Arc::new(tokio::sync::Notify::new()),
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
