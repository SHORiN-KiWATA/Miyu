mod anthropic;
mod antigravity;
mod builder;
mod chat;
mod chat_consume;
mod claude_code;
mod cli_relay;
mod codex;
mod dsml;
mod endpoints;
mod errors;
mod lower;
mod protocol;
mod sse;
mod variants;
mod wire;
use antigravity::AntigravityRuntime;
use claude_code::ClaudeCodeRuntime;
pub(crate) use cli_relay::forget_relay_sessions;
use codex::CodexRuntime;
use dsml::*;
use endpoints::*;
use errors::*;
use lower::*;
pub use protocol::ThinkingVariantOptions;
use protocol::*;
pub(crate) use protocol::{thinking_variant_options_for_model, ThinkingVariantPreferences};
use sse::*;
use wire::*;

use super::{
    ChatMessage, ChatResult, ChatStreamChunk, ChatStreamKind, ResponsesContinuation, ToolCall,
    ToolCallFunction, ToolDefinition, Usage,
};
use crate::config::{AppConfig, ProviderConfig};
use crate::default_models::OPENCODE_ZEN_BASE_URL;
use crate::i18n::text as t;
use crate::models_cache::{self, ModelReasoningInfo, ReasoningSetting, ReasoningVariant};
use crate::paths::MiyuPaths;
use anyhow::{bail, Context, Result};
use futures_util::{Stream, StreamExt};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use std::collections::{BTreeSet, HashMap, HashSet};
use std::fs::{File, OpenOptions};
use std::io::{ErrorKind, Write};
use std::os::fd::AsRawFd;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

static TOOL_CALL_COUNTER: AtomicU64 = AtomicU64::new(0);
static LLM_REQUEST_COUNTER: AtomicU64 = AtomicU64::new(0);
static LLM_SCHEDULER: LazyLock<Mutex<LlmScheduler>> =
    LazyLock::new(|| Mutex::new(LlmScheduler::default()));

fn gen_tool_call_id() -> String {
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let n = TOOL_CALL_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("call_{ts}_{n}")
}

fn gen_llm_request_id() -> String {
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let n = LLM_REQUEST_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("llm_{ts}_{n}")
}

#[derive(Clone)]
pub struct OpenAiCompatibleClient {
    client: Client,
    provider: ProviderConfig,
    api_key: String,
    endpoints: Arc<Vec<LlmEndpoint>>,
    thinking_variants: HashMap<String, String>,
    reasoning_visibility: ReasoningVisibility,
    /// True when partial output never reaches a person mid-request — platform
    /// turns buffer a round and post it as one message. Nothing is committed
    /// until the round ends, so a dropped stream can be retried invisibly.
    buffered_delivery: bool,
    detailed_reasoning_summary: bool,
    request_timeouts: Option<RequestTimeouts>,
    /// Per-clone completion cap. Auxiliary callers (compaction summaries)
    /// clone the client and set this so a runaway summary cannot eat the
    /// window; None leaves the provider default untouched.
    max_tokens_override: Option<u32>,
    continuation_health: ResponsesContinuationHealth,
    /// Scope tag for the per-request cache accounting log ("chat", "qq-judge",
    /// "compact", …). Auxiliary callers override it via `with_request_scope`
    /// so cache stats separate the main conversation from side channels.
    request_scope: &'static str,
    /// claude-code 协议的运行时参数;端点池里没有该协议的端点时为 None。
    claude_code: Option<Arc<ClaudeCodeRuntime>>,
    /// antigravity 协议的运行时参数;端点池里没有该协议的端点时为 None。
    antigravity: Option<Arc<AntigravityRuntime>>,
    /// codex 协议的运行时参数;端点池里没有该协议的端点时为 None。
    codex: Option<Arc<CodexRuntime>>,
    /// 本会话是否 dev 模式(Agent 构造时置位),claude-code 的双四档工具
    /// 作用域(native_tools/miyu_tools)按它判定。
    claude_code_dev_mode: bool,
}

#[derive(Clone, Copy)]
struct RequestTimeouts {
    response_header: Duration,
    stream_idle: Duration,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ReasoningVisibility {
    Hidden,
    Summary,
    Full,
}

impl OpenAiCompatibleClient {}

#[cfg(test)]
mod tests;
