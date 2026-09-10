//! daemon 运行时的共享状态与操作。
//!
//! 这些类型原本长在 `web.rs` 里，于是平台层（QQ 适配、素材、插件）要拿会话
//! 状态、事件总线、运行记录，就得反过来 `use crate::web`——把整个平台层钉死
//! 在 HTTP 服务上。它们和 HTTP 其实没关系：`DaemonState` 是进程级的会话与
//! 平台运行时，`EventHub` 是事件广播，`QuestionBroker` 是提问代理，
//! `ActorCommand` 是 actor 的指令集。Web 只是它们的**一个**消费者，IPC 与
//! 平台适配是另外两个。
//!
//! 所以下沉到这里：`web` 与 `platforms` 都往下引用，方向一致，循环消失。
//!
//! `ApiError` 也在这里——`validate_content` 的返回类型是它，分开会立刻长出
//! 一条 `runtime → web` 的新边。
mod actor;
mod dto;
mod error;
mod events;
mod questions;
mod run;
mod session_ops;
mod state;
mod stores;
mod turn_update;

pub(crate) use actor::*;
pub(crate) use dto::*;
pub(crate) use error::*;
pub(crate) use events::*;
pub(crate) use questions::*;
pub(crate) use run::*;
pub(crate) use session_ops::*;
pub(crate) use state::*;
pub(crate) use stores::*;
pub(crate) use turn_update::*;

use axum::http::StatusCode;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use rand::rngs::OsRng;
use rand::RngCore;
use std::time::Duration;

// ── validate_content ──
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

// ── random_token / random_id / safe_error_message ──
pub(crate) fn random_token(bytes: usize) -> String {
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

/// 把 glibc arena 里已释放的整段内存还给操作系统。大块临时解析（models.dev
/// 目录、整回合请求体）释放后 glibc 默认把页留在 arena 里，RSS 不降。
/// 非 glibc 分配器下是无害空转。
#[cfg(target_os = "linux")]
pub(crate) fn trim_process_memory() {
    unsafe {
        libc::malloc_trim(0);
    }
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn trim_process_memory() {}

// ── MAX_CONTENT_CHARS ──
/// 单回合正文上限。09-07 从 20,000 提到 200,000:程序驱动的 CLI(`--stdin`、
/// stdio)要喂整篇文档,2 万字符连一份中等长度的 README 都装不下。上限仍要
/// 有——IPC 帧 24 MiB、落库与估 token 都按它兜底。
pub(crate) const MAX_CONTENT_CHARS: usize = 200_000;

// ── EVENT_CAPACITY ──
pub(crate) const EVENT_CAPACITY: usize = 4096;

// ── LOGIN_WINDOW / LOGIN_ATTEMPT_LIMIT ──
pub(crate) const LOGIN_WINDOW: Duration = Duration::from_secs(60);
pub(crate) const LOGIN_ATTEMPT_LIMIT: u8 = 5;
/// 限流表最多跟踪多少个来源。满了先清过期条目，仍然满就拒绝新来源——
/// 表只增不删的话，IPv6 轮换能把它撑到几十上百 MB。
pub(crate) const MAX_TRACKED_LOGIN_PEERS: usize = 4_096;
