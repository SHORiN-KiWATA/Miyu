//! OneBot v11 bridge (NapCat / QQ).
//!
//! NapCat connects to Miyu as a reverse-WebSocket client
//! (`GET /ws` on the existing web server; `/onebot/v11/ws` remains an
//! alias). Inbound `message`
//! events run agent turns via the platform-neutral core in the parent
//! module; replies go back as `send_private_msg` / `send_group_msg`
//! frames on the same socket. Query-style API calls (file URL lookup)
//! use an echo-to-oneshot table. Sends are acknowledged before plugin
//! success hooks run, so transformations can safely persist delivery state.

mod adapter;
mod admission;
mod builtin_commands;
mod caches;
mod connection;
mod dispatch;
mod files;
mod forward;
mod group_join;
mod identity;
mod images;
mod inbound;
mod notices;
mod outbound;
mod proactive;
mod send;
mod turn;
use adapter::*;
use admission::*;
use builtin_commands::*;
use caches::*;
use connection::*;
use dispatch::*;
// 这三样 onebot 之外也要用（platforms 持有注册表与监听器，web 复用端口），
// 子模块本身是私有的，得显式再导出一层
pub(crate) use caches::cached_group_name;
pub(crate) use connection::{onebot_ws_on_web_port, ConnectionRegistry, QqListenerManager};
use files::*;
use group_join::*;
use identity::*;
use images::*;
use inbound::*;
use notices::*;
use outbound::*;
pub(crate) use proactive::send_direct_text;
pub(crate) use turn::wake_conversation_for_job;
use turn::*;

use super::access_control::{has_dynamic_access, AccessPermission};
use super::{
    commands, download_capped, markdown_to_plain, resolve_platform_session, run_platform_turn,
    sniff_image_mime, split_reply, BotGroupRole, BotSendAvailability, ConversationKind,
    ForwardNode, OutboundBody, OutboundMessage, OutboundOrigin, OutboundSegment, PartialSendError,
    PlatformAdapter, PlatformContextFileRef, PlatformConversation, PlatformFileDownload,
    PlatformFollowupRun, PlatformGroupMember, PlatformImageData, PlatformInboundEvent,
    PlatformInboundEventKind, PlatformInboundMedia, PlatformMediaKind, PlatformMention,
    PlatformMessageInfo, PlatformMessagePosition, PlatformPrincipal, PlatformTurnContext,
    RateDecision, ResponseTarget, SendReceipt, TriggerDecision, TurnDispatch, TurnProfile,
};
use crate::config::{
    AppConfig, OneBotConfig, PlatformConversationKind, PlatformRateLimit,
    QqGroupJoinApprovalPluginSettings, RealContextPluginSettings, QQ_GROUP_JOIN_APPROVAL_PLUGIN_ID,
    REAL_CONTEXT_PLUGIN_ID,
};
use crate::i18n::text as t;
use crate::ipc::ImageAttachment;
use crate::llm::{ChatMessage, OpenAiCompatibleClient};
use crate::paths::MiyuPaths;
use crate::runtime::{
    clear_platform_session_content, enqueue_turn_update, random_id, reset_platform_persona_state,
    safe_error_message, DaemonState, PlatformPersonaResetError, PlatformSessionResetError,
    TurnUpdateMode, TurnUpdateRequest,
};
use crate::state::{QueuedPromptAttachment, StateStore, UsageMeta};
use anyhow::{bail, Context, Result};
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{ConnectInfo, State};
use axum::http::{
    header::{AUTHORIZATION, HOST},
    HeaderMap, StatusCode,
};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use futures_util::future::{join_all, BoxFuture};
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicI64, Ordering as AtomicOrdering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{mpsc, oneshot, watch, Semaphore};
use tokio::task::JoinHandle;

const MAX_BASE64_FILE_BYTES: usize = 16 * 1024 * 1024;

static LAST_INGRESS_ORDER: AtomicI64 = AtomicI64::new(0);

static GROUP_NAME_CACHE: OnceLock<Mutex<GroupNameCache>> = OnceLock::new();

static MENTION_NAME_CACHE: OnceLock<Mutex<MentionNameCache>> = OnceLock::new();

static GROUP_ROLE_CACHE: OnceLock<Mutex<GroupRoleCache>> = OnceLock::new();

static GROUP_MUTE_CACHE: OnceLock<Mutex<GroupMuteCache>> = OnceLock::new();

// ---------------------------------------------------------------------------
// Connection registry
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// WebSocket endpoint
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Inbound message pipeline
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Outbound
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests;
