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
#[cfg(unix)]
use std::os::fd::AsRawFd;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

static TOOL_CALL_COUNTER: AtomicU64 = AtomicU64::new(0);
static LLM_REQUEST_COUNTER: AtomicU64 = AtomicU64::new(0);
static LLM_SCHEDULER: LazyLock<Mutex<LlmScheduler>> =
    LazyLock::new(|| Mutex::new(LlmScheduler::default()));

const TRANSPORT_RETRY_DELAY: Duration = Duration::from_millis(250);
const MAX_SEND_ATTEMPTS: usize = 3;
/// Attempts a request gets before giving up, however few endpoints exist. With
/// several endpoints these are failovers; with one they are plain retries.
const MIN_ENDPOINT_ATTEMPTS: usize = 3;
#[cfg(not(test))]
const HTTP_STATUS_RETRY_INITIAL_DELAY: Duration = Duration::from_secs(2);
#[cfg(test)]
const HTTP_STATUS_RETRY_INITIAL_DELAY: Duration = Duration::from_millis(10);
#[cfg(not(test))]
const HTTP_STATUS_RETRY_MAX_DELAY: Duration = Duration::from_secs(120);
#[cfg(test)]
const HTTP_STATUS_RETRY_MAX_DELAY: Duration = Duration::from_millis(120);

const CHAT_RESERVED_BODY_KEYS: &[&str] = &[
    "model",
    "messages",
    "temperature",
    "stream",
    "stream_options",
    "tools",
    "chat_template_kwargs",
];
const RESPONSES_RESERVED_BODY_KEYS: &[&str] = &[
    "model",
    "input",
    "instructions",
    "previous_response_id",
    "stream",
    "tools",
    "reasoning",
    "temperature",
];
const ANTHROPIC_RESERVED_BODY_KEYS: &[&str] = &[
    "model",
    "system",
    "messages",
    "tools",
    "stream",
    "max_tokens",
    "temperature",
    "thinking",
];

fn sanitize_extra_body(
    extra: Option<Map<String, Value>>,
    reserved_keys: &[&str],
) -> Option<Map<String, Value>> {
    let mut extra = extra?;
    for key in reserved_keys {
        extra.remove(*key);
    }
    (!extra.is_empty()).then_some(extra)
}

fn merge_extra_body(
    base: Option<Map<String, Value>>,
    overlay: Option<Map<String, Value>>,
) -> Option<Map<String, Value>> {
    let mut base = base.unwrap_or_default();
    for (key, value) in overlay.unwrap_or_default() {
        match base.get_mut(&key) {
            Some(existing) => merge_json_value(existing, value),
            None => {
                base.insert(key, value);
            }
        }
    }
    (!base.is_empty()).then_some(base)
}

fn merge_json_value(base: &mut Value, overlay: Value) {
    if let (Some(base), Some(overlay)) = (base.as_object_mut(), overlay.as_object()) {
        for (key, value) in overlay {
            match base.get_mut(key) {
                Some(existing) => merge_json_value(existing, value.clone()),
                None => {
                    base.insert(key.clone(), value.clone());
                }
            }
        }
    } else {
        *base = overlay;
    }
}

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TransportFailureKind {
    Connect,
    Timeout,
    Other,
}

impl std::fmt::Display for TransportFailureKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Connect => "connect",
            Self::Timeout => "timeout",
            Self::Other => "request",
        })
    }
}

fn retryable_transport_failure(kind: TransportFailureKind) -> bool {
    kind == TransportFailureKind::Connect
}

fn retryable_http_status(status: u16) -> bool {
    (500..=599).contains(&status)
}

fn http_status_retry_delay(attempt: usize) -> Duration {
    HTTP_STATUS_RETRY_INITIAL_DELAY
        .saturating_mul(1 << attempt.saturating_sub(1).min(6))
        .min(HTTP_STATUS_RETRY_MAX_DELAY)
}

#[derive(Debug)]
struct TransportFailure {
    stage: &'static str,
    kind: TransportFailureKind,
}

impl std::fmt::Display for TransportFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} transport failed ({})", self.stage, self.kind)
    }
}

impl std::error::Error for TransportFailure {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HttpFailureKind {
    Status,
    Authentication,
    RateLimit,
    EndpointUnavailable,
    EndpointIncompatible,
    InvalidRequest,
}

impl std::fmt::Display for HttpFailureKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Status => "status",
            Self::Authentication => "authentication",
            Self::RateLimit => "rate_limit",
            Self::EndpointUnavailable => "endpoint_unavailable",
            Self::EndpointIncompatible => "endpoint_incompatible",
            Self::InvalidRequest => "invalid_request",
        })
    }
}

#[derive(Debug)]
struct HttpStatusFailure {
    status: u16,
    kind: HttpFailureKind,
}

impl HttpStatusFailure {
    fn classify(status: u16, body: &str) -> Self {
        let kind = match status {
            401 | 403 => HttpFailureKind::Authentication,
            429 => HttpFailureKind::RateLimit,
            408 | 500..=599 => HttpFailureKind::Status,
            _ => classify_provider_error_body(body).unwrap_or(HttpFailureKind::Status),
        };
        Self { status, kind }
    }
}

fn classify_provider_error_body(body: &str) -> Option<HttpFailureKind> {
    let structured = serde_json::from_str::<Value>(body).ok();
    let error = structured
        .as_ref()
        .and_then(|value| value.get("error"))
        .or(structured.as_ref());
    let mut signals = Vec::with_capacity(3);
    if let Some(error) = error {
        for field in ["code", "type", "status", "message"] {
            if let Some(value) = error.get(field).and_then(Value::as_str) {
                signals.push(normalize_error_signal(value));
            }
        }
    }
    if signals.is_empty() {
        signals.push(normalize_error_signal(body));
    }

    for signal in &signals {
        if contains_any(
            signal,
            &[
                "invalid_api_key",
                "incorrect_api_key",
                "authentication",
                "unauthorized",
                "forbidden",
                "permission_denied",
            ],
        ) {
            return Some(HttpFailureKind::Authentication);
        }
    }
    for signal in &signals {
        if contains_any(
            signal,
            &["rate_limit", "ratelimit", "quota", "too_many_requests"],
        ) {
            return Some(HttpFailureKind::RateLimit);
        }
    }
    for signal in &signals {
        if contains_any(
            signal,
            &[
                "model_not_found",
                "model_not_available",
                "model_unavailable",
                "unsupported_model",
                "deployment_not_found",
                "model_access_denied",
                "no_available_provider",
                "provider_unavailable",
                "upstream_request_failed",
                "service_unavailable",
                "overloaded",
            ],
        ) {
            return Some(HttpFailureKind::EndpointUnavailable);
        }
    }
    for signal in &signals {
        if contains_any(
            signal,
            &[
                "context_length",
                "context_window",
                "max_tokens",
                "unsupported_parameter",
                "unknown_parameter",
                "unsupported_feature",
                "not_supported",
            ],
        ) {
            return Some(HttpFailureKind::EndpointIncompatible);
        }
    }
    for signal in &signals {
        if contains_any(
            signal,
            &[
                "invalid_request",
                "invalid_argument",
                "malformed",
                "validation_error",
            ],
        ) {
            return Some(HttpFailureKind::InvalidRequest);
        }
    }
    None
}

fn normalize_error_signal(value: &str) -> String {
    let mut normalized = String::with_capacity(value.len());
    let mut separator = false;
    let bytes = value.as_bytes();
    for (index, byte) in bytes.iter().copied().enumerate() {
        if byte.is_ascii_alphanumeric() {
            let previous = index
                .checked_sub(1)
                .and_then(|index| bytes.get(index))
                .copied();
            let next = bytes.get(index + 1).copied();
            let camel_case_boundary = byte.is_ascii_uppercase()
                && previous.is_some_and(|previous| {
                    previous.is_ascii_lowercase()
                        || previous.is_ascii_digit()
                        || (previous.is_ascii_uppercase()
                            && next.is_some_and(|next_byte| next_byte.is_ascii_lowercase()))
                });
            if camel_case_boundary && !separator && !normalized.is_empty() {
                normalized.push('_');
            }
            normalized.push((byte as char).to_ascii_lowercase());
            separator = false;
        } else if !separator && !normalized.is_empty() {
            normalized.push('_');
            separator = true;
        }
    }
    if normalized.ends_with('_') {
        normalized.pop();
    }
    normalized
}

fn contains_any(value: &str, needles: &[&str]) -> bool {
    needles.iter().any(|needle| value.contains(needle))
}

impl std::fmt::Display for HttpStatusFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "upstream returned HTTP {}", self.status)
    }
}

impl std::error::Error for HttpStatusFailure {}

fn format_error_chain(error: &(dyn std::error::Error + 'static)) -> String {
    let mut message = error.to_string();
    let mut source = error.source();
    while let Some(error) = source {
        message.push_str(": ");
        message.push_str(&error.to_string());
        source = error.source();
    }
    message
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProviderProtocol {
    Auto,
    OpenAiChat,
    OpenAiResponses,
    Anthropic,
}

impl ProviderProtocol {
    fn from_provider(provider: &ProviderConfig) -> Result<Self> {
        match provider.protocol.trim().to_ascii_lowercase().as_str() {
            "" | "auto" => Ok(Self::Auto),
            "openai-chat" => Ok(Self::OpenAiChat),
            "openai-responses" => Ok(Self::OpenAiResponses),
            "anthropic" | "anthropic-messages" | "claude" | "claude-messages" => {
                Ok(Self::Anthropic)
            }
            protocol => bail!("unsupported provider protocol: {protocol}"),
        }
    }
}

fn effective_protocol(provider: &ProviderConfig, model: &str) -> Result<ProviderProtocol> {
    match ProviderProtocol::from_provider(provider)? {
        ProviderProtocol::Auto if provider_looks_anthropic(provider) => {
            Ok(ProviderProtocol::Anthropic)
        }
        ProviderProtocol::Auto if uses_openai_responses(model) => {
            Ok(ProviderProtocol::OpenAiResponses)
        }
        ProviderProtocol::Auto => Ok(ProviderProtocol::OpenAiChat),
        protocol => Ok(protocol),
    }
}

fn uses_openai_responses(model: &str) -> bool {
    let model = model.to_ascii_lowercase();
    model.starts_with("gpt-5")
        || model.starts_with("o1")
        || model.starts_with("o3")
        || model.starts_with("o4")
}

fn is_openrouter_provider(provider: &ProviderConfig) -> bool {
    provider.id.eq_ignore_ascii_case("openrouter")
        || provider
            .base_url
            .to_ascii_lowercase()
            .contains("openrouter.ai")
}

fn uses_enable_thinking(provider: &ProviderConfig, info: &ModelReasoningInfo) -> bool {
    info.provider_npm.as_deref() == Some("@ai-sdk/alibaba")
        || provider.id.to_ascii_lowercase().contains("alibaba")
        || provider
            .base_url
            .to_ascii_lowercase()
            .contains("dashscope.aliyuncs.com")
}

fn anthropic_reasoning_budget(max_tokens: u32, requested: u64) -> Option<u64> {
    (max_tokens > 1024 && requested < u64::from(max_tokens)).then_some(requested)
}

fn supported_reasoning_variants(provider: &ProviderConfig, model: &str) -> Vec<ReasoningVariant> {
    let Some(info) = models_cache::reasoning_info(&provider.id, model) else {
        return Vec::new();
    };
    info.variants
        .iter()
        .filter(|variant| reasoning_variant_supported(provider, model, &info, variant))
        .cloned()
        .collect()
}

fn reasoning_variant_supported(
    provider: &ProviderConfig,
    model: &str,
    info: &ModelReasoningInfo,
    variant: &ReasoningVariant,
) -> bool {
    let Ok(protocol) = effective_protocol(provider, model) else {
        return false;
    };
    reasoning_variant_supported_for_protocol(provider, info, variant, protocol)
}

fn reasoning_variant_supported_for_protocol(
    provider: &ProviderConfig,
    info: &ModelReasoningInfo,
    variant: &ReasoningVariant,
    protocol: ProviderProtocol,
) -> bool {
    match protocol {
        ProviderProtocol::OpenAiResponses => matches!(
            variant.setting,
            ReasoningSetting::Effort(_) | ReasoningSetting::Toggle(_) | ReasoningSetting::Disabled
        ),
        ProviderProtocol::Anthropic => match variant.setting {
            ReasoningSetting::BudgetTokens(budget) => {
                anthropic_reasoning_budget(provider.anthropic_max_tokens, budget).is_some()
            }
            _ => true,
        },
        ProviderProtocol::OpenAiChat | ProviderProtocol::Auto => {
            let npm = info.provider_npm.as_deref().unwrap_or_default();
            if is_openrouter_provider(provider) || npm == "@openrouter/ai-sdk-provider" {
                matches!(
                    variant.setting,
                    ReasoningSetting::Effort(_) | ReasoningSetting::BudgetTokens(_)
                )
            } else if matches!(variant.setting, ReasoningSetting::Effort(_)) {
                true
            } else if uses_enable_thinking(provider, info) {
                matches!(variant.setting, ReasoningSetting::Toggle(_))
            } else {
                false
            }
        }
    }
}

fn thinking_variant_key(provider_id: &str, model: &str) -> String {
    format!("{provider_id}\t{model}")
}

fn rename_thinking_variant_entries<T>(
    entries: &mut HashMap<String, T>,
    old_id: &str,
    new_id: &str,
) {
    let prefix = format!("{old_id}\t");
    let renamed = entries
        .keys()
        .filter_map(|key| {
            key.strip_prefix(&prefix)
                .map(|model| (key.clone(), thinking_variant_key(new_id, model)))
        })
        .collect::<Vec<_>>();
    for (old_key, new_key) in renamed {
        if let Some(value) = entries.remove(&old_key) {
            entries.insert(new_key, value);
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(crate) struct ThinkingVariantPreferences {
    #[serde(default)]
    selected: HashMap<String, String>,
    #[serde(skip)]
    changes: HashMap<String, Option<String>>,
    #[serde(skip)]
    provider_renames: Vec<(String, String)>,
}

fn thinking_variant_preferences_file(paths: &MiyuPaths) -> PathBuf {
    paths.state_dir.join("thinking-variants.json")
}

fn lock_thinking_variant_preferences(paths: &MiyuPaths) -> Result<File> {
    let lock_path = paths.state_dir.join("thinking-variants.lock");
    let lock = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .open(&lock_path)
        .with_context(|| {
            format!(
                "failed to open thinking variant lock: {}",
                lock_path.display()
            )
        })?;
    if !crate::sys::flock_lock_ex(&lock, false)? {
        return Err(std::io::Error::last_os_error()).with_context(|| {
            format!(
                "failed to lock thinking variant file: {}",
                lock_path.display()
            )
        });
    }
    Ok(lock)
}

fn load_thinking_variant_preferences(paths: &MiyuPaths) -> ThinkingVariantPreferences {
    ThinkingVariantPreferences::load(paths)
}

impl ThinkingVariantPreferences {
    pub(crate) fn load(paths: &MiyuPaths) -> Self {
        Self::load_for_update(paths).unwrap_or_default()
    }

    fn load_for_update(paths: &MiyuPaths) -> Result<Self> {
        let path = thinking_variant_preferences_file(paths);
        match std::fs::read_to_string(&path) {
            Ok(text) => serde_json::from_str(&text).with_context(|| {
                format!("failed to parse thinking variant state: {}", path.display())
            }),
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(Self::default()),
            Err(error) => Err(error).with_context(|| {
                format!("failed to read thinking variant state: {}", path.display())
            }),
        }
    }

    pub(crate) fn selected(&self, provider_id: &str, model: &str) -> Option<&str> {
        self.selected
            .get(&thinking_variant_key(provider_id, model))
            .map(String::as_str)
    }

    pub(crate) fn set(&mut self, provider_id: &str, model: &str, selected: Option<String>) {
        let key = thinking_variant_key(provider_id, model);
        let selected = selected.filter(|value| !value.trim().is_empty());
        if self.selected.get(&key).map(String::as_str) == selected.as_deref() {
            return;
        }
        if let Some(selected) = &selected {
            self.selected.insert(key.clone(), selected.clone());
        } else {
            self.selected.remove(&key);
        }
        self.changes.insert(key, selected);
    }

    pub(crate) fn rename_provider(&mut self, old_id: &str, new_id: &str) {
        if old_id == new_id {
            return;
        }
        rename_thinking_variant_entries(&mut self.selected, old_id, new_id);
        rename_thinking_variant_entries(&mut self.changes, old_id, new_id);
        self.provider_renames
            .push((old_id.to_string(), new_id.to_string()));
    }

    /// True when `save` would write anything to disk.
    pub(crate) fn is_dirty(&self) -> bool {
        !self.changes.is_empty() || !self.provider_renames.is_empty()
    }

    pub(crate) fn save(&self, paths: &MiyuPaths) -> Result<()> {
        if self.changes.is_empty() && self.provider_renames.is_empty() {
            return Ok(());
        }

        let path = thinking_variant_preferences_file(paths);
        let parent = path
            .parent()
            .ok_or_else(|| anyhow::anyhow!("thinking variant state path has no parent"))?;
        std::fs::create_dir_all(parent)?;
        let _lock = lock_thinking_variant_preferences(paths)?;
        let mut persisted = Self::load_for_update(paths)?;
        for (old_id, new_id) in &self.provider_renames {
            rename_thinking_variant_entries(&mut persisted.selected, old_id, new_id);
        }
        for (key, selected) in &self.changes {
            if let Some(selected) = selected {
                persisted.selected.insert(key.clone(), selected.clone());
            } else {
                persisted.selected.remove(key);
            }
        }
        let mut temp = tempfile::NamedTempFile::new_in(parent)?;
        temp.write_all(serde_json::to_string_pretty(&persisted)?.as_bytes())?;
        temp.persist(path).map_err(|error| error.error)?;
        Ok(())
    }
}

fn chat_variant_body(
    provider: &ProviderConfig,
    info: &ModelReasoningInfo,
    setting: ReasoningSetting,
) -> Option<Map<String, Value>> {
    let npm = info.provider_npm.as_deref().unwrap_or_default();
    match setting {
        ReasoningSetting::Effort(effort)
            if is_openrouter_provider(provider) || npm == "@openrouter/ai-sdk-provider" =>
        {
            Some(
                json!({ "reasoning": { "effort": effort } })
                    .as_object()?
                    .clone(),
            )
        }
        ReasoningSetting::BudgetTokens(budget)
            if is_openrouter_provider(provider) || npm == "@openrouter/ai-sdk-provider" =>
        {
            Some(
                json!({ "reasoning": { "max_tokens": budget } })
                    .as_object()?
                    .clone(),
            )
        }
        ReasoningSetting::Effort(effort) => {
            Some(json!({ "reasoning_effort": effort }).as_object()?.clone())
        }
        ReasoningSetting::Toggle(enabled) if uses_enable_thinking(provider, info) => {
            Some(json!({ "enable_thinking": enabled }).as_object()?.clone())
        }
        _ => None,
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ThinkingVariantOptions {
    pub provider_id: String,
    pub model: String,
    pub variants: Vec<String>,
    pub selected: Option<String>,
}

pub(crate) fn thinking_variant_options_for_model(
    provider: &ProviderConfig,
    model: &str,
    selected: Option<&str>,
) -> ThinkingVariantOptions {
    let variants = supported_reasoning_variants(provider, model)
        .into_iter()
        .map(|variant| variant.id)
        .collect::<Vec<_>>();
    let selected = selected
        .filter(|selected| variants.iter().any(|variant| variant == *selected))
        .map(str::to_string);
    ThinkingVariantOptions {
        provider_id: provider.id.clone(),
        model: model.to_string(),
        variants,
        selected,
    }
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
    /// Scope tag for the per-request cache accounting log ("chat", "qq-judge",
    /// "compact", …). Auxiliary callers override it via `with_request_scope`
    /// so cache stats separate the main conversation from side channels.
    request_scope: &'static str,
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

#[derive(Clone)]
struct LlmEndpoint {
    client: Client,
    provider: ProviderConfig,
    api_key: String,
    key_index: usize,
}

impl LlmEndpoint {
    fn id(&self) -> String {
        endpoint_id(
            &self.provider.id,
            &self.provider.default_model,
            self.key_index,
        )
    }
}

#[derive(Default)]
struct LlmScheduler {
    cursor: usize,
    cooldowns: HashMap<String, Instant>,
}

impl LlmScheduler {
    fn ordered_indices(&mut self, endpoints: &[LlmEndpoint]) -> Vec<usize> {
        let available = endpoints
            .iter()
            .enumerate()
            .filter_map(|(index, endpoint)| self.is_ready(&endpoint.id()).then_some(index))
            .collect::<Vec<_>>();
        if available.is_empty() {
            return Vec::new();
        }
        let start = self.cursor % available.len();
        self.cursor = self.cursor.wrapping_add(1);
        rotate_from(available, start)
    }

    fn is_ready(&mut self, id: &str) -> bool {
        match self.cooldowns.get(id).copied() {
            Some(until) if until > Instant::now() => false,
            Some(_) => {
                self.cooldowns.remove(id);
                true
            }
            None => true,
        }
    }

    /// The endpoint whose cooldown lifts first, for the single probe sent when
    /// every endpoint is cooling down. `None` sorts ahead of any deadline, so
    /// an endpoint with no cooldown recorded wins outright.
    fn soonest_ready_index(&self, endpoints: &[LlmEndpoint]) -> Option<usize> {
        endpoints
            .iter()
            .enumerate()
            .min_by_key(|(_, endpoint)| self.cooldowns.get(&endpoint.id()).copied())
            .map(|(index, _)| index)
    }

    fn mark_success(&mut self, id: &str) {
        self.cooldowns.remove(id);
    }

    fn mark_failure(&mut self, id: String, duration: Duration) {
        self.cooldowns.insert(id, Instant::now() + duration);
    }
}

fn rotate_from<T>(mut items: Vec<T>, start: usize) -> Vec<T> {
    items.rotate_left(start);
    items
}

fn endpoint_id(provider_id: &str, model: &str, key_index: usize) -> String {
    format!("{provider_id}\t{model}\t{key_index}")
}

fn ordered_endpoint_indices(endpoints: &[LlmEndpoint]) -> Vec<usize> {
    LLM_SCHEDULER
        .lock()
        .map(|mut scheduler| scheduler.ordered_indices(endpoints))
        .unwrap_or_else(|_| (0..endpoints.len()).collect())
}

fn soonest_ready_endpoint_index(endpoints: &[LlmEndpoint]) -> Option<usize> {
    LLM_SCHEDULER
        .lock()
        .ok()
        .and_then(|scheduler| scheduler.soonest_ready_index(endpoints))
        .or_else(|| (!endpoints.is_empty()).then_some(0))
}

fn mark_endpoint_success(endpoint: &LlmEndpoint) {
    if let Ok(mut scheduler) = LLM_SCHEDULER.lock() {
        scheduler.mark_success(&endpoint.id());
    }
}

fn mark_endpoint_failure(endpoint: &LlmEndpoint, error: &anyhow::Error) -> Option<Duration> {
    let duration = cooldown_for_error(error)?;
    let mut scheduler = LLM_SCHEDULER.lock().ok()?;
    scheduler.mark_failure(endpoint.id(), duration);
    Some(duration)
}

fn cooldown_for_status(status: u16) -> Option<Duration> {
    match status {
        401 | 403 | 429 => Some(Duration::from_secs(600)),
        408 | 500..=599 => Some(Duration::from_secs(120)),
        _ => None,
    }
}

fn cooldown_for_error(error: &anyhow::Error) -> Option<Duration> {
    if let Some(failure) = error.downcast_ref::<HttpStatusFailure>() {
        return match failure.kind {
            HttpFailureKind::Authentication | HttpFailureKind::RateLimit => {
                Some(Duration::from_secs(600))
            }
            HttpFailureKind::EndpointUnavailable => Some(Duration::from_secs(120)),
            HttpFailureKind::EndpointIncompatible | HttpFailureKind::InvalidRequest => None,
            HttpFailureKind::Status => cooldown_for_status(failure.status),
        };
    }
    if error.downcast_ref::<TransportFailure>().is_some() {
        return Some(Duration::from_secs(120));
    }
    error
        .downcast_ref::<reqwest::Error>()
        .filter(|error| error.is_connect() || error.is_timeout())
        .map(|_| Duration::from_secs(120))
}

fn endpoint_failover_allowed(error: &anyhow::Error) -> bool {
    !error
        .downcast_ref::<HttpStatusFailure>()
        .is_some_and(|failure| failure.kind == HttpFailureKind::InvalidRequest)
}

/// Whether the *same* endpoint may be tried again inside one request. A 429 or
/// a rejected key is a verdict on that provider/model/key, not a moment in
/// time: the retries `MIN_ENDPOINT_ATTEMPTS` pads in would fire back-to-back
/// with no backoff and spend more of a quota that already said no — which on a
/// shared free tier is what exhausted it. Failover to a *different* endpoint is
/// unaffected; that is `endpoint_failover_allowed`'s job.
fn same_endpoint_retry_allowed(error: &anyhow::Error) -> bool {
    !error
        .downcast_ref::<HttpStatusFailure>()
        .is_some_and(|failure| {
            matches!(
                failure.kind,
                HttpFailureKind::Authentication | HttpFailureKind::RateLimit
            )
        })
}

fn endpoint_client(provider: &ProviderConfig) -> Result<Client> {
    // Auxiliary callers (judge/affection/organizer) rebuild their client per
    // call; without this cache every judge run pays fresh TLS setup and loses
    // connection reuse. Keyed by every input the builder consumes, so a config
    // edit that changes the timeout naturally mints a new client; the map is
    // bounded by the number of distinct providers. `reqwest::Client` is an Arc
    // handle — clones share one pool.
    static CLIENTS: std::sync::OnceLock<std::sync::Mutex<HashMap<(String, u64), Client>>> =
        std::sync::OnceLock::new();
    let timeout = provider.timeout_seconds.clamp(5, 30);
    let key = (provider.id.clone(), timeout);
    let mut cache = CLIENTS
        .get_or_init(|| std::sync::Mutex::new(HashMap::new()))
        .lock()
        .unwrap();
    if let Some(client) = cache.get(&key) {
        return Ok(client.clone());
    }
    let client = Client::builder()
        .connect_timeout(Duration::from_secs(timeout))
        .build()
        .with_context(|| format!("building HTTP client for provider {}", provider.id))?;
    cache.insert(key, client.clone());
    Ok(client)
}

fn llm_endpoints(config: &AppConfig, paths: &MiyuPaths) -> Result<Vec<LlmEndpoint>> {
    let mut endpoints = Vec::new();
    let mut errors = Vec::new();
    for choice in config.active_provider_model_choices() {
        let mut provider = config.provider(Some(&choice.provider_id))?.clone();
        provider.default_model = choice.model;
        let client = endpoint_client(&provider)?;
        match provider.resolved_api_keys(paths) {
            Ok(keys) => {
                for key in keys {
                    endpoints.push(LlmEndpoint {
                        client: client.clone(),
                        provider: provider.clone(),
                        api_key: key.value,
                        key_index: key.index,
                    });
                }
            }
            Err(err) => errors.push(format!(
                "{} / {}: {err}",
                provider.id, provider.default_model
            )),
        }
    }
    if endpoints.is_empty() {
        bail!(
            "no active provider/model endpoint is configured:\n- {}",
            errors.join("\n- ")
        )
    }
    Ok(endpoints)
}

impl OpenAiCompatibleClient {
    /// 当前主 provider id,用量历史记账用(具体模型以 ChatResult 为准)。
    pub fn provider_id(&self) -> &str {
        &self.provider.id
    }

    pub fn from_config(config: &AppConfig, paths: &MiyuPaths) -> Result<Self> {
        super::cache_log::configure(paths, &config.cache);
        let endpoints = llm_endpoints(config, paths)?;
        let first = endpoints
            .first()
            .with_context(|| "no active provider/model endpoint is configured")?;
        let mut client = Self {
            client: first.client.clone(),
            provider: first.provider.clone(),
            api_key: first.api_key.clone(),
            endpoints: Arc::new(endpoints),
            thinking_variants: HashMap::new(),
            reasoning_visibility: reasoning_visibility(config),
            buffered_delivery: false,
            detailed_reasoning_summary: reasoning_summary_is_detailed(config),
            request_timeouts: None,
            max_tokens_override: None,
            request_scope: "chat",
        };
        client.restore_saved_thinking_variants(paths);
        Ok(client)
    }

    /// Builds a client over an explicit provider/model pool (e.g. a
    /// subagent tier pool). Requests load-balance across the pool through
    /// the shared endpoint scheduler, exactly like the main model pool.
    pub fn from_choices(
        config: &AppConfig,
        paths: &MiyuPaths,
        choices: &[crate::config::ProviderModelChoice],
    ) -> Result<Self> {
        super::cache_log::configure(paths, &config.cache);
        let mut endpoints = Vec::new();
        let mut errors = Vec::new();
        for choice in choices {
            let mut provider = match config.provider(Some(&choice.provider_id)) {
                Ok(provider) => provider.clone(),
                Err(err) => {
                    errors.push(format!("{} / {}: {err}", choice.provider_id, choice.model));
                    continue;
                }
            };
            provider.default_model = choice.model.clone();
            let client = endpoint_client(&provider)?;
            match provider.resolved_api_keys(paths) {
                Ok(keys) => {
                    for key in keys {
                        endpoints.push(LlmEndpoint {
                            client: client.clone(),
                            provider: provider.clone(),
                            api_key: key.value,
                            key_index: key.index,
                        });
                    }
                }
                Err(err) => errors.push(format!(
                    "{} / {}: {err}",
                    provider.id, provider.default_model
                )),
            }
        }
        let first = match endpoints.first() {
            Some(first) => first,
            None => bail!(
                "no usable endpoint in the model pool:\n- {}",
                errors.join("\n- ")
            ),
        };
        let mut client = Self {
            client: first.client.clone(),
            provider: first.provider.clone(),
            api_key: first.api_key.clone(),
            endpoints: Arc::new(endpoints),
            thinking_variants: HashMap::new(),
            reasoning_visibility: reasoning_visibility(config),
            buffered_delivery: false,
            detailed_reasoning_summary: reasoning_summary_is_detailed(config),
            request_timeouts: None,
            max_tokens_override: None,
            request_scope: "chat",
        };
        client.restore_saved_thinking_variants(paths);
        Ok(client)
    }

    pub fn new(provider: &ProviderConfig, config: &AppConfig, paths: &MiyuPaths) -> Result<Self> {
        if provider.default_model.trim().is_empty() {
            bail!(
                "{}: {}",
                t(
                    "provider has no active model; select a model before chatting",
                    "provider 没有当前模型；请先选择模型再聊天",
                ),
                provider.id
            );
        }
        let client = endpoint_client(provider)?;
        let key = provider
            .resolved_api_keys(paths)?
            .into_iter()
            .next()
            .with_context(|| format!("missing API key for provider {}", provider.id))?;
        let endpoint = LlmEndpoint {
            client: client.clone(),
            provider: provider.clone(),
            api_key: key.value.clone(),
            key_index: key.index,
        };
        let mut client = Self {
            client,
            provider: provider.clone(),
            api_key: key.value,
            endpoints: Arc::new(vec![endpoint]),
            thinking_variants: HashMap::new(),
            reasoning_visibility: reasoning_visibility(config),
            buffered_delivery: false,
            detailed_reasoning_summary: reasoning_summary_is_detailed(config),
            request_timeouts: None,
            max_tokens_override: None,
            request_scope: "chat",
        };
        client.restore_saved_thinking_variants(paths);
        Ok(client)
    }

    pub fn context_window(&self, config: &AppConfig) -> Result<Option<usize>> {
        let choices = self.endpoint_model_choices();
        let mut windows = Vec::with_capacity(choices.len());
        for (provider_id, model) in choices {
            let Some(window) = config.context_window_for_provider_model(&provider_id, &model)?
            else {
                return Ok(None);
            };
            windows.push(window);
        }
        Ok(windows.into_iter().min())
    }

    /// Marks a client whose caller collects output and delivers it in one
    /// piece. A truncated stream can then be retried without the person
    /// seeing the false start.
    pub fn with_buffered_delivery(mut self, buffered: bool) -> Self {
        self.buffered_delivery = buffered;
        self
    }

    pub fn for_subagent_output(mut self, full: bool) -> Self {
        self.reasoning_visibility = if full {
            ReasoningVisibility::Full
        } else {
            ReasoningVisibility::Hidden
        };
        self.detailed_reasoning_summary = full;
        self
    }

    pub fn with_request_timeouts(
        mut self,
        response_header: Duration,
        stream_idle: Duration,
    ) -> Self {
        self.request_timeouts = Some(RequestTimeouts {
            response_header: response_header.max(Duration::from_millis(1)),
            stream_idle: stream_idle.max(Duration::from_millis(1)),
        });
        self
    }

    pub fn models_without_context_window(&self, config: &AppConfig) -> Vec<String> {
        self.endpoint_model_choices()
            .into_iter()
            .filter(|(provider_id, model)| {
                config
                    .context_window_for_provider_model(provider_id, model)
                    .ok()
                    .flatten()
                    .is_none()
            })
            .map(|(provider_id, model)| format!("{provider_id} / {model}"))
            .collect()
    }

    fn endpoint_model_choices(&self) -> BTreeSet<(String, String)> {
        self.endpoints
            .iter()
            .map(|endpoint| {
                (
                    endpoint.provider.id.clone(),
                    endpoint.provider.default_model.clone(),
                )
            })
            .collect()
    }

    fn with_endpoint(&self, endpoint: &LlmEndpoint) -> Self {
        Self {
            client: endpoint.client.clone(),
            provider: endpoint.provider.clone(),
            api_key: endpoint.api_key.clone(),
            endpoints: self.endpoints.clone(),
            thinking_variants: self.thinking_variants.clone(),
            reasoning_visibility: self.reasoning_visibility,
            buffered_delivery: self.buffered_delivery,
            detailed_reasoning_summary: self.detailed_reasoning_summary,
            request_timeouts: self.request_timeouts,
            max_tokens_override: self.max_tokens_override,
            request_scope: self.request_scope,
        }
    }

    /// Returns a clone whose chat completions are capped at `max_tokens`.
    pub fn with_request_scope(mut self, scope: &'static str) -> Self {
        self.request_scope = scope;
        self
    }

    pub fn with_max_tokens(&self, max_tokens: u32) -> Self {
        let mut clone = self.clone();
        clone.max_tokens_override = Some(max_tokens.max(1));
        clone
    }

    pub fn available_thinking_variants(&self) -> Vec<String> {
        let options = self.thinking_variant_options();
        (options.len() == 1)
            .then(|| options[0].variants.clone())
            .unwrap_or_default()
    }

    pub fn set_thinking_variant(&mut self, variant: Option<String>) -> Result<()> {
        let options = self.thinking_variant_options();
        if options.len() != 1 {
            bail!("a model must be specified when multiple models are active");
        }
        let option = &options[0];
        self.set_thinking_variants(&[(option.provider_id.clone(), option.model.clone(), variant)])
    }

    pub fn set_thinking_variants(
        &mut self,
        selections: &[(String, String, Option<String>)],
    ) -> Result<()> {
        let options = self.thinking_variant_options();
        for (provider_id, model, selected) in selections {
            let option = options
                .iter()
                .find(|option| option.provider_id == *provider_id && option.model == *model)
                .ok_or_else(|| anyhow::anyhow!("inactive model: {provider_id} / {model}"))?;
            if let Some(selected) = selected {
                if !option.variants.iter().any(|variant| variant == selected) {
                    bail!(
                        "thinking variant is unavailable for {provider_id} / {model}: {selected}"
                    );
                }
            }
        }
        for (provider_id, model, selected) in selections {
            let key = thinking_variant_key(provider_id, model);
            if let Some(selected) = selected.as_ref().filter(|value| !value.trim().is_empty()) {
                self.thinking_variants.insert(key, selected.clone());
            } else {
                self.thinking_variants.remove(&key);
            }
        }
        Ok(())
    }

    pub fn restore_thinking_variants(&mut self, selections: &[(String, String, String)]) {
        let active = self.endpoint_model_preferences();
        for (provider_id, model, selected) in selections {
            if active.iter().any(|(active_provider, active_model)| {
                active_provider == provider_id && active_model == model
            }) {
                self.thinking_variants
                    .insert(thinking_variant_key(provider_id, model), selected.clone());
            }
        }
    }

    fn restore_saved_thinking_variants(&mut self, paths: &MiyuPaths) {
        let preferences = load_thinking_variant_preferences(paths);
        let selections = self
            .endpoint_model_preferences()
            .into_iter()
            .filter_map(|(provider_id, model)| {
                let selected = preferences
                    .selected(&provider_id, &model)
                    .map(str::to_string)?;
                Some((provider_id, model, selected))
            })
            .collect::<Vec<_>>();
        self.restore_thinking_variants(&selections);
    }

    pub fn save_thinking_variants(&self, paths: &MiyuPaths) -> Result<()> {
        let mut preferences = load_thinking_variant_preferences(paths);
        for (provider_id, model) in self.endpoint_model_preferences() {
            let key = thinking_variant_key(&provider_id, &model);
            preferences.set(
                &provider_id,
                &model,
                self.thinking_variants.get(&key).cloned(),
            );
        }
        preferences.save(paths)
    }

    pub fn thinking_variant_options(&self) -> Vec<ThinkingVariantOptions> {
        self.endpoint_model_preferences()
            .into_iter()
            .filter_map(|(provider_id, model)| {
                let provider = &self
                    .endpoints
                    .iter()
                    .find(|endpoint| {
                        endpoint.provider.id == provider_id
                            && endpoint.provider.default_model == model
                    })?
                    .provider;
                let selected = self
                    .thinking_variants
                    .get(&thinking_variant_key(&provider_id, &model))
                    .map(String::as_str);
                Some(thinking_variant_options_for_model(
                    provider, &model, selected,
                ))
            })
            .collect()
    }

    pub fn thinking_variant_summary(&self) -> Option<String> {
        let options = self.thinking_variant_options();
        let mut variants = options.iter().map(|option| option.selected.as_deref());
        let first = variants.next()?;
        if variants.all(|variant| variant == first) {
            first.map(str::to_string)
        } else {
            Some("mixed".to_string())
        }
    }

    pub fn thinking_variant_for(&self, provider_id: &str, model: &str) -> Option<String> {
        self.thinking_variant_options()
            .into_iter()
            .find(|options| options.provider_id == provider_id && options.model == model)
            .and_then(|options| options.selected)
    }

    pub fn endpoint_model_preferences(&self) -> Vec<(String, String)> {
        self.endpoints
            .iter()
            .map(|endpoint| {
                (
                    endpoint.provider.id.clone(),
                    endpoint.provider.default_model.clone(),
                )
            })
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect()
    }

    fn selected_reasoning_variant(&self) -> Option<(ModelReasoningInfo, ReasoningVariant)> {
        let id = self.selected_thinking_variant_id()?;
        let info = models_cache::reasoning_info(&self.provider.id, &self.provider.default_model)?;
        let variant = info
            .variants
            .iter()
            .find(|candidate| candidate.id.as_str() == id)
            .cloned()?;
        reasoning_variant_supported(
            &self.provider,
            &self.provider.default_model,
            &info,
            &variant,
        )
        .then_some((info, variant))
    }

    fn selected_thinking_variant_id(&self) -> Option<&str> {
        self.thinking_variants
            .get(&thinking_variant_key(
                &self.provider.id,
                &self.provider.default_model,
            ))
            .map(String::as_str)
    }

    fn chat_variant_extra_body(&self) -> Option<Map<String, Value>> {
        let (info, variant) = self.selected_reasoning_variant()?;
        chat_variant_body(&self.provider, &info, variant.setting)
    }

    fn responses_reasoning(&self) -> Option<ResponsesReasoning> {
        let summary = self.responses_reasoning_summary();
        let Some((_, variant)) = self.selected_reasoning_variant() else {
            return Some(default_responses_reasoning(summary));
        };
        match variant.setting {
            ReasoningSetting::Effort(effort) => Some(ResponsesReasoning {
                effort: Some(effort),
                summary: Some(summary.to_string()),
            }),
            ReasoningSetting::Toggle(true) => Some(default_responses_reasoning(summary)),
            ReasoningSetting::Toggle(false) | ReasoningSetting::Disabled => None,
            ReasoningSetting::BudgetTokens(_) => Some(default_responses_reasoning(summary)),
        }
    }

    fn responses_reasoning_summary(&self) -> &'static str {
        if self.detailed_reasoning_summary {
            "detailed"
        } else {
            "auto"
        }
    }

    fn anthropic_variant(
        &self,
        thinking_enabled: bool,
    ) -> (Option<Value>, Option<Map<String, Value>>) {
        if !thinking_enabled {
            return (None, None);
        }
        let Some((_, variant)) = self.selected_reasoning_variant() else {
            return (Some(anthropic_thinking_config()), None);
        };
        match variant.setting {
            ReasoningSetting::Effort(effort) => (
                Some(anthropic_thinking_config()),
                Some(
                    json!({ "output_config": { "effort": effort } })
                        .as_object()
                        .unwrap()
                        .clone(),
                ),
            ),
            ReasoningSetting::Toggle(true) => (Some(anthropic_thinking_config()), None),
            ReasoningSetting::Toggle(false) | ReasoningSetting::Disabled => (None, None),
            ReasoningSetting::BudgetTokens(budget) => {
                let budget = anthropic_reasoning_budget(self.provider.anthropic_max_tokens, budget)
                    .expect("unsupported Anthropic budget variant should be filtered");
                (
                    Some(json!({ "type": "enabled", "budget_tokens": budget })),
                    None,
                )
            }
        }
    }

    pub async fn chat_stream<F>(
        &self,
        messages: Vec<ChatMessage>,
        tools: Vec<ToolDefinition>,
        mut on_chunk: F,
    ) -> Result<ChatResult>
    where
        F: FnMut(ChatStreamChunk) -> Result<()>,
    {
        self.chat_stream_inner(messages, tools, None, false, &mut on_chunk)
            .await
    }

    pub(crate) async fn chat_stream_with_continuation<F>(
        &self,
        messages: Vec<ChatMessage>,
        tools: Vec<ToolDefinition>,
        continuation: Option<&ResponsesContinuation>,
        mut on_chunk: F,
    ) -> Result<ChatResult>
    where
        F: FnMut(ChatStreamChunk) -> Result<()>,
    {
        self.chat_stream_inner(messages, tools, continuation, false, &mut on_chunk)
            .await
    }

    /// Runs an internal completion without exposing partial output. Since no
    /// chunk is committed to a user, a failed endpoint can be safely replaced
    /// even after it emitted an incomplete response.
    pub async fn chat_buffered(
        &self,
        messages: Vec<ChatMessage>,
        tools: Vec<ToolDefinition>,
    ) -> Result<ChatResult> {
        self.chat_stream_inner(messages, tools, None, true, &mut |_| Ok(()))
            .await
    }

    /// Cache keepalive ping (v7 DeepSeek 高命中策略): re-sends the exact
    /// prompt prefix of the last live request as a non-streaming
    /// max_tokens=1 completion so best-effort provider caches keep the deep
    /// prefix alive between user turns. The messages/tools serialization goes
    /// through the same path as live chat, so the server-rendered prompt is
    /// byte-identical (measured: extra body params like max_tokens do not
    /// affect the provider prefix cache key). Returns the reported usage, or
    /// None when the selected endpoint speaks a protocol where the ping does
    /// not apply (Anthropic / OpenAI Responses).
    pub async fn cache_keepalive(
        &self,
        messages: Vec<ChatMessage>,
        tools: Vec<ToolDefinition>,
    ) -> Result<Option<Usage>> {
        let endpoints = self.endpoints.as_ref();
        let order = ordered_endpoint_indices(endpoints);
        let index = order.first().copied().unwrap_or(0);
        let endpoint = endpoints
            .get(index)
            .context("no LLM endpoint configured for cache keepalive")?;
        let client = self.with_endpoint(endpoint);
        if client.uses_openai_responses() || client.uses_anthropic_messages() {
            return Ok(None);
        }
        client.cache_keepalive_single(messages, tools).await
    }

    async fn cache_keepalive_single(
        &self,
        messages: Vec<ChatMessage>,
        tools: Vec<ToolDefinition>,
    ) -> Result<Option<Usage>> {
        let request_id = gen_llm_request_id();
        let extra_body = merge_extra_body(
            sanitize_extra_body(self.provider.extra_body.clone(), CHAT_RESERVED_BODY_KEYS),
            self.chat_variant_extra_body(),
        );
        let messages = prepare_chat_messages_for_provider(&self.provider, messages);
        let request = ChatRequest {
            model: self.provider.default_model.clone(),
            messages,
            temperature: self.provider.temperature,
            stream: false,
            stream_options: None,
            max_tokens: Some(1),
            tools: (!tools.is_empty()).then_some(tools),
            chat_template_kwargs: taotoken_glm_chat_template_kwargs(&self.provider),
            extra_body,
        };
        let url = format!(
            "{}/chat/completions",
            self.provider.base_url.trim_end_matches('/')
        );
        let response = self
            .send_chat_completion_request(&url, &request, &request_id, "chat.cache_keepalive")
            .await?;
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        if !status.is_success() {
            bail!("cache keepalive ping failed with HTTP {status}: {body}");
        }
        let value: serde_json::Value = serde_json::from_str(&body)
            .with_context(|| "cache keepalive response was not valid JSON")?;
        let usage = value
            .get("usage")
            .cloned()
            .and_then(|usage| serde_json::from_value::<Usage>(usage).ok())
            .map(|mut usage| {
                usage.normalize_cache_fields();
                usage
            });
        Ok(usage)
    }

    async fn chat_stream_inner<F>(
        &self,
        messages: Vec<ChatMessage>,
        tools: Vec<ToolDefinition>,
        continuation: Option<&ResponsesContinuation>,
        buffered: bool,
        on_chunk: &mut F,
    ) -> Result<ChatResult>
    where
        F: FnMut(ChatStreamChunk) -> Result<()>,
    {
        let request_id = gen_llm_request_id();
        let endpoints = self.endpoints.as_ref();
        let mut errors = Vec::new();
        let mut order = if let Some(continuation) = continuation {
            let index = endpoints
                .iter()
                .position(|endpoint| endpoint.id() == continuation.endpoint_id)
                .with_context(|| {
                    format!(
                        "Responses continuation endpoint is no longer available: {}",
                        continuation.endpoint_id
                    )
                })?;
            vec![index]
        } else {
            ordered_endpoint_indices(endpoints)
        };
        // Every endpoint is cooling down. Refusing outright would strand a
        // single-endpoint user for the whole cooldown, so one probe still goes
        // out — but exactly one, and to whichever endpoint recovers first.
        // Refilling the pool here (and then padding it below) meant a rate
        // limit cost three requests per turn *for the entire cooldown*, which
        // made the cooldown worse than useless.
        let probe_only = order.is_empty();
        if probe_only {
            tracing::warn!(
                request_id,
                endpoint_count = endpoints.len(),
                all_endpoints_cooling_down = true,
                "{}",
                t(
                    "All LLM endpoints are cooling down; sending a single probe",
                    "所有 LLM 端点均在冷却；仅发送一次探测请求"
                )
            );
            order = soonest_ready_endpoint_index(endpoints)
                .into_iter()
                .collect();
        }
        // A dropped stream or a 5xx is a moment in time, not a verdict on the
        // endpoint. Tying the number of attempts to the number of configured
        // endpoints meant someone with a single model got no retry at all,
        // which is backwards: they are the ones with nowhere else to go. Pad
        // the attempt list by cycling so every setup gets the same budget.
        // Errors that a retry cannot fix still stop on the first attempt —
        // `endpoint_failover_allowed` returns before the next one is tried,
        // and `same_endpoint_retry_allowed` skips the padded repeats of an
        // endpoint that answered 429/401.
        if !probe_only && !order.is_empty() && order.len() < MIN_ENDPOINT_ATTEMPTS {
            let cycle: Vec<usize> = order.clone();
            while order.len() < MIN_ENDPOINT_ATTEMPTS {
                order.extend(cycle.iter().copied());
            }
            order.truncate(MIN_ENDPOINT_ATTEMPTS);
        }
        tracing::debug!(
            request_id,
            endpoint_count = order.len(),
            message_count = messages.len(),
            tool_count = tools.len(),
            continued = continuation.is_some(),
            "{}",
            t("LLM request started", "LLM 请求已开始")
        );
        let mut exhausted: Vec<String> = Vec::new();
        for (attempt, index) in order.into_iter().enumerate() {
            let endpoint = &endpoints[index];
            if exhausted.contains(&endpoint.id()) {
                continue;
            }
            let client = self.with_endpoint(endpoint);
            if attempt > 0 {
                on_chunk(ChatStreamChunk {
                    kind: ChatStreamKind::ReasoningReset,
                    text: String::new(),
                })?;
            }
            let started = Instant::now();
            tracing::debug!(
                request_id,
                attempt = attempt + 1,
                provider = %endpoint.provider.id,
                model = %endpoint.provider.default_model,
                key_index = endpoint.key_index + 1,
                "{}",
                t("LLM endpoint attempt started", "LLM 端点尝试已开始")
            );
            let mut attempt_committed = false;
            let result = {
                let buffered = buffered || self.buffered_delivery;
                let mut attempt_on_chunk = |chunk: ChatStreamChunk| {
                    if !buffered {
                        attempt_committed |=
                            stream_chunk_commits_attempt(&chunk, client.reasoning_visibility);
                    }
                    on_chunk(chunk)
                };
                client
                    .chat_stream_single(
                        messages.clone(),
                        tools.clone(),
                        continuation.map(|continuation| continuation.response_id.as_str()),
                        &request_id,
                        &mut attempt_on_chunk,
                    )
                    .await
            };
            match result {
                Ok(mut result) => {
                    result.provider_id = Some(endpoint.provider.id.clone());
                    result.model = Some(endpoint.provider.default_model.clone());
                    if let Some(next) = result.responses_continuation.as_mut() {
                        next.endpoint_id = endpoint.id();
                    }
                    mark_endpoint_success(endpoint);
                    super::cache_log::record(
                        self.request_scope,
                        &endpoint.provider.id,
                        &endpoint.provider.default_model,
                        endpoint.key_index,
                        &request_id,
                        result.usage.as_ref(),
                    );
                    tracing::debug!(
                        request_id,
                        attempt = attempt + 1,
                        provider = %endpoint.provider.id,
                        model = %endpoint.provider.default_model,
                        elapsed_ms = started.elapsed().as_millis(),
                        "{}",
                        t("LLM endpoint succeeded", "LLM 端点请求成功")
                    );
                    return Ok(result);
                }
                Err(err) => {
                    let cooldown = mark_endpoint_failure(endpoint, &err);
                    let endpoint_cooling_down = cooldown.is_some();
                    let cooldown_seconds = cooldown.map(|duration| duration.as_secs()).unwrap_or(0);
                    if let Some(failure) = err.downcast_ref::<TransportFailure>() {
                        tracing::error!(
                            request_id,
                            attempt = attempt + 1,
                            provider = %endpoint.provider.id,
                            model = %endpoint.provider.default_model,
                            stage = failure.stage,
                            transport_kind = %failure.kind,
                            endpoint_cooling_down,
                            cooldown_seconds,
                            elapsed_ms = started.elapsed().as_millis(),
                            error = %format!("{err:#}"),
                            "{}",
                            t("LLM endpoint transport failure", "LLM 端点传输失败")
                        );
                    } else if let Some(failure) = err.downcast_ref::<HttpStatusFailure>() {
                        tracing::error!(
                            request_id,
                            attempt = attempt + 1,
                            provider = %endpoint.provider.id,
                            model = %endpoint.provider.default_model,
                            status = failure.status,
                            failure_kind = %failure.kind,
                            endpoint_cooling_down,
                            cooldown_seconds,
                            elapsed_ms = started.elapsed().as_millis(),
                            "{}",
                            t("LLM endpoint HTTP failure", "LLM 端点 HTTP 请求失败")
                        );
                    } else {
                        tracing::error!(
                            request_id,
                            attempt = attempt + 1,
                            provider = %endpoint.provider.id,
                            model = %endpoint.provider.default_model,
                            endpoint_cooling_down,
                            cooldown_seconds,
                            elapsed_ms = started.elapsed().as_millis(),
                            error = %format!("{err:#}"),
                            "{}",
                            t(
                                "LLM endpoint failed outside the HTTP send stage",
                                "LLM 端点在 HTTP 发送阶段之外失败"
                            )
                        );
                    }
                    let message = format!("{err:#}");
                    errors.push(format!(
                        "{} / {} key#{}: {message}",
                        endpoint.provider.id,
                        endpoint.provider.default_model,
                        endpoint.key_index + 1
                    ));
                    if !same_endpoint_retry_allowed(&err) {
                        exhausted.push(endpoint.id());
                    }
                    if attempt_committed {
                        return Err(err.context(
                            "LLM stream failed after emitting output; endpoint failover was suppressed",
                        ));
                    }
                    if !endpoint_failover_allowed(&err) {
                        return Err(err.context(
                            "LLM request was rejected; endpoint failover was suppressed",
                        ));
                    }
                }
            }
        }
        bail!(
            "no LLM provider/model endpoint succeeded (request {request_id}):\n- {}",
            errors.join("\n- ")
        )
    }

    async fn chat_stream_single<F>(
        &self,
        messages: Vec<ChatMessage>,
        tools: Vec<ToolDefinition>,
        previous_response_id: Option<&str>,
        request_id: &str,
        on_chunk: &mut F,
    ) -> Result<ChatResult>
    where
        F: FnMut(ChatStreamChunk) -> Result<()>,
    {
        let protocol = ProviderProtocol::from_provider(&self.provider)?;
        let uses_responses = protocol == ProviderProtocol::OpenAiResponses
            || (protocol == ProviderProtocol::Auto && self.uses_openai_responses());
        if previous_response_id.is_some() && !uses_responses {
            bail!("Responses continuation endpoint no longer uses the Responses protocol");
        }
        if protocol == ProviderProtocol::Anthropic
            || (protocol == ProviderProtocol::Auto && self.uses_anthropic_messages())
        {
            return self
                .chat_anthropic_stream(messages, tools, request_id, on_chunk)
                .await;
        }
        if uses_responses {
            if let Some(result) = self
                .chat_responses_stream(
                    messages.clone(),
                    tools.clone(),
                    previous_response_id,
                    request_id,
                    on_chunk,
                )
                .await?
            {
                return Ok(result);
            }
            if previous_response_id.is_some() {
                bail!("OpenAI Responses continuation is not supported by this provider");
            }
            if protocol == ProviderProtocol::OpenAiResponses {
                bail!("OpenAI Responses protocol is not supported by this provider");
            }
            if let Some((info, variant)) = self.selected_reasoning_variant() {
                if !reasoning_variant_supported_for_protocol(
                    &self.provider,
                    &info,
                    &variant,
                    ProviderProtocol::OpenAiChat,
                ) {
                    bail!(
                        "thinking variant '{}' cannot be applied after falling back from OpenAI Responses to Chat Completions",
                        variant.id
                    );
                }
            }
        }
        let extra_body = merge_extra_body(
            sanitize_extra_body(self.provider.extra_body.clone(), CHAT_RESERVED_BODY_KEYS),
            self.chat_variant_extra_body(),
        );
        let messages = prepare_chat_messages_for_provider(&self.provider, messages);
        let mut request = ChatRequest {
            model: self.provider.default_model.clone(),
            messages,
            temperature: self.provider.temperature,
            stream: true,
            stream_options: Some(ChatStreamOptions {
                include_usage: true,
            }),
            max_tokens: self.max_tokens_override,
            tools: (!tools.is_empty()).then_some(tools),
            chat_template_kwargs: taotoken_glm_chat_template_kwargs(&self.provider),
            extra_body,
        };
        let url = format!(
            "{}/chat/completions",
            self.provider.base_url.trim_end_matches('/')
        );
        let mut response = self
            .send_chat_completion_request(&url, &request, request_id, "chat.send")
            .await?;
        let mut status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            if non_stream_quota_fallback_candidate(status.as_u16(), &body) {
                let mut retry = request.clone();
                retry.stream = false;
                retry.stream_options = None;
                let response = self
                    .send_chat_completion_request(
                        &url,
                        &retry,
                        request_id,
                        "chat.retry_without_streaming",
                    )
                    .await?;
                let retry_status = response.status();
                if retry_status.is_success() {
                    tracing::info!(
                        request_id,
                        provider = %self.provider.id,
                        model = %self.provider.default_model,
                        "{}",
                        t(
                            "streaming quota was unavailable; non-streaming compatibility retry succeeded",
                            "流式配额不可用；非流式兼容重试成功"
                        )
                    );
                    return self
                        .consume_chat_completion_response(response, on_chunk)
                        .await;
                }
                let retry_body = response.text().await.unwrap_or_default();
                tracing::debug!(
                    request_id,
                    status = retry_status.as_u16(),
                    "{}",
                    t(
                        "non-streaming quota compatibility retry returned an HTTP error",
                        "非流式配额兼容重试返回 HTTP 错误"
                    )
                );
                return self.bail_chat_completion_failure(retry_status.as_u16(), &retry_body);
            }
            if stream_options_unsupported(status.as_u16(), &body) {
                request.stream_options = None;
                response = self
                    .send_chat_completion_request(
                        &url,
                        &request,
                        request_id,
                        "chat.retry_without_stream_options",
                    )
                    .await?;
                status = response.status();
                if status.is_success() {
                    return self
                        .consume_chat_completion_stream(response, on_chunk)
                        .await;
                }
                let body = response.text().await.unwrap_or_default();
                if let Some(result) = self
                    .try_zen_chat_completion_compat_retry(
                        &url,
                        &request,
                        status.as_u16(),
                        &body,
                        request_id,
                        on_chunk,
                    )
                    .await?
                {
                    return Ok(result);
                }
                return self.bail_chat_completion_failure(status.as_u16(), &body);
            }
            if let Some(result) = self
                .try_zen_chat_completion_compat_retry(
                    &url,
                    &request,
                    status.as_u16(),
                    &body,
                    request_id,
                    on_chunk,
                )
                .await?
            {
                return Ok(result);
            }
            return self.bail_chat_completion_failure(status.as_u16(), &body);
        }

        self.consume_chat_completion_stream(response, on_chunk)
            .await
    }

    async fn send_chat_completion_request(
        &self,
        url: &str,
        request: &ChatRequest,
        request_id: &str,
        stage: &'static str,
    ) -> Result<reqwest::Response> {
        self.send_with_transport_retry(request_id, stage, || {
            self.client
                .post(url)
                .bearer_auth(&self.api_key)
                .json(request)
        })
        .await
    }

    async fn send_with_transport_retry<F>(
        &self,
        request_id: &str,
        stage: &'static str,
        mut build_request: F,
    ) -> Result<reqwest::Response>
    where
        F: FnMut() -> reqwest::RequestBuilder,
    {
        let mut connect_retry_used = false;
        let mut attempt = 0usize;
        loop {
            attempt = attempt.saturating_add(1);
            let started = Instant::now();
            let send = build_request().send();
            let response = if let Some(timeouts) = self.request_timeouts {
                match tokio::time::timeout(timeouts.response_header, send).await {
                    Ok(response) => response,
                    Err(_) => {
                        tracing::warn!(
                            request_id,
                            stage,
                            attempt,
                            timeout_seconds = timeouts.response_header.as_secs(),
                            "{}",
                            t("LLM response header timed out", "LLM 响应头等待超时")
                        );
                        return Err(anyhow::anyhow!(
                            "LLM response header timed out after {} seconds",
                            timeouts.response_header.as_secs()
                        )
                        .context(TransportFailure {
                            stage,
                            kind: TransportFailureKind::Timeout,
                        }));
                    }
                }
            } else {
                send.await
            };
            match response {
                Ok(response) => {
                    let status = response.status().as_u16();
                    let retryable_status = retryable_http_status(status);
                    let will_retry = retryable_status && attempt < MAX_SEND_ATTEMPTS;
                    tracing::debug!(
                        request_id,
                        stage,
                        attempt,
                        status,
                        will_retry,
                        elapsed_ms = started.elapsed().as_millis(),
                        "{}",
                        t(
                            "LLM HTTP response headers received",
                            "已收到 LLM HTTP 响应头"
                        )
                    );
                    if will_retry {
                        let delay = http_status_retry_delay(attempt);
                        tracing::warn!(
                            request_id,
                            stage,
                            attempt,
                            status,
                            retry_delay_ms = delay.as_millis(),
                            "{}",
                            t(
                                "LLM HTTP request returned a transient server error",
                                "LLM HTTP 请求返回临时服务器错误"
                            )
                        );
                        let _ = response.bytes().await;
                        tokio::time::sleep(delay).await;
                        continue;
                    }
                    if retryable_status {
                        let body = response.text().await.unwrap_or_default();
                        return Err(anyhow::anyhow!(
                            "LLM HTTP request failed after {attempt} attempts: {body}"
                        )
                        .context(HttpStatusFailure::classify(status, &body)));
                    }
                    return Ok(response);
                }
                Err(error) => {
                    let kind = if error.is_connect() {
                        TransportFailureKind::Connect
                    } else if error.is_timeout() {
                        TransportFailureKind::Timeout
                    } else {
                        TransportFailureKind::Other
                    };
                    let will_retry = attempt < MAX_SEND_ATTEMPTS
                        && !connect_retry_used
                        && retryable_transport_failure(kind);
                    connect_retry_used |= will_retry;
                    let error = error.without_url();
                    tracing::warn!(
                        request_id,
                        stage,
                        attempt,
                        transport_kind = %kind,
                        will_retry,
                        elapsed_ms = started.elapsed().as_millis(),
                        error = %format_error_chain(&error),
                        "{}",
                        t("LLM HTTP transport attempt failed", "LLM HTTP 传输尝试失败")
                    );
                    if will_retry {
                        tokio::time::sleep(TRANSPORT_RETRY_DELAY).await;
                        continue;
                    }
                    return Err(anyhow::Error::new(error).context(TransportFailure { stage, kind }));
                }
            }
        }
    }

    async fn next_response_chunk<S, T>(
        &self,
        stream: &mut S,
        stage: &'static str,
    ) -> Result<Option<T>>
    where
        S: Stream<Item = std::result::Result<T, reqwest::Error>> + Unpin,
    {
        let next = if let Some(timeouts) = self.request_timeouts {
            match tokio::time::timeout(timeouts.stream_idle, stream.next()).await {
                Ok(next) => next,
                Err(_) => {
                    return Err(anyhow::anyhow!(
                        "LLM response stream was idle for {} seconds",
                        timeouts.stream_idle.as_secs()
                    )
                    .context(TransportFailure {
                        stage,
                        kind: TransportFailureKind::Timeout,
                    }));
                }
            }
        } else {
            stream.next().await
        };
        next.transpose().map_err(|error| {
            anyhow::Error::new(error).context(TransportFailure {
                stage,
                kind: TransportFailureKind::Other,
            })
        })
    }

    async fn try_zen_chat_completion_compat_retry<F>(
        &self,
        url: &str,
        request: &ChatRequest,
        status: u16,
        body: &str,
        request_id: &str,
        on_chunk: &mut F,
    ) -> Result<Option<ChatResult>>
    where
        F: FnMut(ChatStreamChunk) -> Result<()>,
    {
        if !zen_upstream_failed(&self.provider, status, body) {
            return Ok(None);
        }

        let mut retries = Vec::new();
        if request.stream_options.is_some() {
            let mut retry = request.clone();
            retry.stream_options = None;
            retries.push(retry);
        }
        if request.tools.is_some() {
            let mut retry = request.clone();
            retry.stream_options = None;
            retry.tools = None;
            retries.push(retry);
        }

        for (attempt, retry) in retries.into_iter().enumerate() {
            let response = self
                .send_chat_completion_request(
                    url,
                    &retry,
                    request_id,
                    "chat.zen_compatibility_retry",
                )
                .await?;
            let status = response.status();
            if status.is_success() {
                return self
                    .consume_chat_completion_stream(response, on_chunk)
                    .await
                    .map(Some);
            }
            tracing::debug!(
                request_id,
                attempt = attempt + 1,
                status = status.as_u16(),
                "{}",
                t(
                    "Zen compatibility retry returned an HTTP error",
                    "Zen 兼容重试返回 HTTP 错误"
                )
            );
            let _ = response.text().await;
        }

        Ok(None)
    }

    async fn consume_chat_completion_stream<F>(
        &self,
        response: reqwest::Response,
        on_chunk: &mut F,
    ) -> Result<ChatResult>
    where
        F: FnMut(ChatStreamChunk) -> Result<()>,
    {
        let dsml = dsml_enabled_for(&self.provider);
        let mut buffer = Utf8LineBuffer::default();
        let mut content = String::new();
        let mut content_emitted = 0usize;
        let mut reasoning = String::new();
        let mut reasoning_emitted = 0usize;
        let mut reasoning_part_active = false;
        let mut finish_reason = None;
        let mut usage = None;
        let mut tool_calls = ToolCallAccumulator::default();
        let mut stream = response.bytes_stream();
        while let Some(chunk) = self.next_response_chunk(&mut stream, "chat.stream").await? {
            for line in buffer.push(&chunk)? {
                if let Some(done) = handle_sse_line(
                    &line,
                    &mut content,
                    &mut content_emitted,
                    &mut reasoning,
                    &mut reasoning_emitted,
                    &mut reasoning_part_active,
                    &mut finish_reason,
                    &mut usage,
                    &mut tool_calls,
                    &mut *on_chunk,
                )? {
                    if done {
                        return finalize_stream_result(
                            content,
                            reasoning,
                            usage,
                            tool_calls.finish(),
                            dsml,
                        );
                    }
                }
            }
        }
        for line in buffer.finish()? {
            let _ = handle_sse_line(
                &line,
                &mut content,
                &mut content_emitted,
                &mut reasoning,
                &mut reasoning_emitted,
                &mut reasoning_part_active,
                &mut finish_reason,
                &mut usage,
                &mut tool_calls,
                &mut *on_chunk,
            )?;
        }
        // Reaching here means the socket closed without `[DONE]` — the loop
        // above returns early on that marker. A provider that ends this way
        // still has to have said it was finished somewhere, and `finish_reason`
        // is the only other place it can say so (llama.cpp's Responses
        // endpoint, for one, never sends `[DONE]`). With neither signal the
        // response is a truncated fragment, and returning it as a completed
        // turn is how an empty reply reaches the user with nothing logged.
        //
        // Reported as a transport failure so the existing machinery retries it
        // across endpoints and resets the partial reasoning already streamed.
        // Retrying is safe here: tool calls execute after this returns, so a
        // truncated turn has run nothing yet.
        if finish_reason.is_none() {
            return Err(anyhow::anyhow!(t(
                "the response stream ended before the model said it was done",
                "模型还没说完，响应流就提前结束了"
            ))
            .context(TransportFailure {
                stage: "chat.stream",
                kind: TransportFailureKind::Other,
            }));
        }
        flush_buffer(
            &reasoning,
            &mut reasoning_emitted,
            ChatStreamKind::Reasoning,
            &mut *on_chunk,
            true,
        )?;
        flush_buffer(
            &content,
            &mut content_emitted,
            ChatStreamKind::Content,
            &mut *on_chunk,
            true,
        )?;
        tracing::debug!(
            provider = %self.provider.id,
            model = %self.provider.default_model,
            finish_reason = finish_reason.as_deref(),
            content_chars = content.chars().count(),
            reasoning_chars = reasoning.chars().count(),
            tool_call_count = tool_calls.calls.len(),
            "{}",
            t("Chat completions stream reached EOF", "聊天补全流已到达 EOF")
        );
        let mut result =
            finalize_stream_result(content, reasoning, usage, tool_calls.finish(), dsml)?;
        result.finish_reason = finish_reason;
        if reasoning_part_active {
            on_chunk(ChatStreamChunk {
                kind: ChatStreamKind::ReasoningPartEnd,
                text: String::new(),
            })?;
        }
        Ok(result)
    }

    async fn consume_chat_completion_response<F>(
        &self,
        response: reqwest::Response,
        on_chunk: &mut F,
    ) -> Result<ChatResult>
    where
        F: FnMut(ChatStreamChunk) -> Result<()>,
    {
        const MAX_RESPONSE_BYTES: usize = 16 * 1024 * 1024;

        if response
            .content_length()
            .is_some_and(|length| length > MAX_RESPONSE_BYTES as u64)
        {
            bail!("non-streaming chat response exceeds the 16 MiB limit");
        }
        let mut bytes = Vec::new();
        let mut stream = response.bytes_stream();
        while let Some(chunk) = self
            .next_response_chunk(&mut stream, "chat.response")
            .await?
        {
            if bytes.len().saturating_add(chunk.len()) > MAX_RESPONSE_BYTES {
                bail!("non-streaming chat response exceeds the 16 MiB limit");
            }
            bytes.extend_from_slice(&chunk);
        }
        let response: ChatCompletionResponse =
            serde_json::from_slice(&bytes).with_context(|| {
                format!(
                    "{}: {}",
                    t(
                        "invalid non-streaming chat completions response",
                        "无效的非流式聊天响应",
                    ),
                    clean_plain_text(String::from_utf8_lossy(&bytes).to_string())
                )
            })?;
        if let Some(error) = response.error {
            bail!(
                "{}: {}",
                t(
                    "non-streaming chat completions returned an error",
                    "非流式聊天响应返回错误"
                ),
                provider_error_text(&error)
            );
        }
        let choice = response
            .choices
            .into_iter()
            .next()
            .context("non-streaming chat response contained no choices")?;
        let mut tool_calls = ToolCallAccumulator::default();
        let reasoning = delta_reasoning_text(&choice.message).unwrap_or_default();
        if !reasoning.is_empty() {
            on_chunk(ChatStreamChunk {
                kind: ChatStreamKind::ReasoningPartStart,
                text: String::new(),
            })?;
            on_chunk(ChatStreamChunk {
                kind: ChatStreamKind::Reasoning,
                text: reasoning.clone(),
            })?;
            on_chunk(ChatStreamChunk {
                kind: ChatStreamKind::ReasoningPartEnd,
                text: String::new(),
            })?;
        }
        let content = choice.message.content.unwrap_or_default();
        if !content.is_empty() {
            on_chunk(ChatStreamChunk {
                kind: ChatStreamKind::Content,
                text: content.clone(),
            })?;
        }
        for tool_call in choice.message.tool_calls {
            if let Some(name) = tool_calls.push(tool_call) {
                on_chunk(ChatStreamChunk {
                    kind: ChatStreamKind::ToolCall,
                    text: name,
                })?;
            }
        }
        tracing::debug!(
            provider = %self.provider.id,
            model = %self.provider.default_model,
            finish_reason = choice.finish_reason.as_deref(),
            content_chars = content.chars().count(),
            reasoning_chars = reasoning.chars().count(),
            tool_call_count = tool_calls.calls.len(),
            "{}",
            t(
                "Non-streaming chat completions response consumed",
                "非流式聊天补全响应已处理"
            )
        );
        let mut result = finalize_stream_result(
            content,
            reasoning,
            response.usage,
            tool_calls.finish(),
            dsml_enabled_for(&self.provider),
        )?;
        result.finish_reason = choice.finish_reason;
        Ok(result)
    }

    fn bail_chat_completion_failure<T>(&self, status: u16, body: &str) -> Result<T> {
        let hint = claude_protocol_hint(&self.provider);
        Err(anyhow::anyhow!(
            "{} ({}): {}{}",
            t("chat completions stream request failed", "聊天流式请求失败",),
            status,
            body,
            hint
        )
        .context(HttpStatusFailure::classify(status, body)))
    }

    async fn chat_anthropic_stream<F>(
        &self,
        messages: Vec<ChatMessage>,
        tools: Vec<ToolDefinition>,
        request_id: &str,
        on_chunk: &mut F,
    ) -> Result<ChatResult>
    where
        F: FnMut(ChatStreamChunk) -> Result<()>,
    {
        let mut response = self
            .send_anthropic_request(
                &self.anthropic_request(messages.clone(), tools.clone(), true),
                request_id,
                "anthropic.send",
            )
            .await?;
        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            let sent_thinking_blocks = messages.iter().any(|message| {
                message.thinking_signature.is_some()
                    && message
                        .reasoning_content
                        .as_deref()
                        .is_some_and(|text| !text.trim().is_empty())
            });
            if sent_thinking_blocks && anthropic_thinking_unsupported(status.as_u16(), &body) {
                // The request already carried well-formed thinking blocks, so a
                // thinking-shaped 400 here is a protocol bug on our side, not a
                // capability gap. Surface it instead of silently downgrading the
                // whole tool loop (double request per round + split cache).
                return Err(anyhow::anyhow!(
                    "{} ({status}): {body}",
                    t(
                        "anthropic messages stream rejected replayed thinking blocks",
                        "Anthropic Messages 拒绝了回传的 thinking 块"
                    )
                )
                .context(HttpStatusFailure::classify(status.as_u16(), &body)));
            }
            if anthropic_thinking_unsupported(status.as_u16(), &body) {
                response = self
                    .send_anthropic_request(
                        &self.anthropic_request(messages, tools, false),
                        request_id,
                        "anthropic.retry_without_thinking",
                    )
                    .await?;
                let status = response.status();
                if status.is_success() {
                    return self.consume_anthropic_stream(response, on_chunk).await;
                }
                let body = response.text().await.unwrap_or_default();
                return Err(anyhow::anyhow!(
                    "{} ({status}): {body}",
                    t(
                        "anthropic messages stream request failed",
                        "Anthropic Messages 流式请求失败"
                    )
                )
                .context(HttpStatusFailure::classify(status.as_u16(), &body)));
            }
            return Err(anyhow::anyhow!(
                "{} ({status}): {body}",
                t(
                    "anthropic messages stream request failed",
                    "Anthropic Messages 流式请求失败"
                )
            )
            .context(HttpStatusFailure::classify(status.as_u16(), &body)));
        }

        self.consume_anthropic_stream(response, on_chunk).await
    }
    fn anthropic_request(
        &self,
        messages: Vec<ChatMessage>,
        tools: Vec<ToolDefinition>,
        thinking: bool,
    ) -> AnthropicRequest {
        let (variant_thinking, variant_extra) = self.anthropic_variant(thinking);
        let extra_body = merge_extra_body(
            sanitize_extra_body(
                self.provider.extra_body.clone(),
                ANTHROPIC_RESERVED_BODY_KEYS,
            ),
            variant_extra,
        );
        AnthropicRequest {
            model: self.provider.default_model.clone(),
            system: lower_anthropic_system(&messages),
            messages: lower_anthropic_messages(messages),
            tools: (!tools.is_empty()).then(|| lower_anthropic_tools(tools)),
            stream: true,
            max_tokens: self
                .max_tokens_override
                .map(|cap| cap.min(self.provider.anthropic_max_tokens))
                .unwrap_or(self.provider.anthropic_max_tokens),
            temperature: Some(self.provider.temperature),
            thinking: variant_thinking,
            extra_body,
        }
    }

    async fn send_anthropic_request(
        &self,
        request: &AnthropicRequest,
        request_id: &str,
        stage: &'static str,
    ) -> Result<reqwest::Response> {
        let url = format!("{}/messages", self.provider.base_url.trim_end_matches('/'));
        self.send_with_transport_retry(request_id, stage, || {
            self.client
                .post(&url)
                .header("x-api-key", &self.api_key)
                .header("anthropic-version", "2023-06-01")
                .json(request)
        })
        .await
    }

    async fn consume_anthropic_stream<F>(
        &self,
        response: reqwest::Response,
        on_chunk: &mut F,
    ) -> Result<ChatResult>
    where
        F: FnMut(ChatStreamChunk) -> Result<()>,
    {
        let dsml = dsml_enabled_for(&self.provider);
        let mut state = AnthropicStreamState::default();
        let mut buffer = SseDataBuffer::default();
        let mut stream = response.bytes_stream();
        while let Some(chunk) = self
            .next_response_chunk(&mut stream, "anthropic.stream")
            .await?
        {
            for data in buffer.push(&chunk)? {
                if handle_anthropic_sse_data(&data, &mut state, &mut *on_chunk)? {
                    let signature = state.thinking_signature.take();
                    let mut result = finalize_stream_result(
                        state.content,
                        state.reasoning,
                        state.usage,
                        state.tool_calls.finish(),
                        dsml,
                    )?;
                    result.thinking_signature = signature;
                    return Ok(result);
                }
            }
        }
        for data in buffer.finish()? {
            let _ = handle_anthropic_sse_data(&data, &mut state, &mut *on_chunk)?;
        }
        flush_buffer(
            &state.reasoning,
            &mut state.reasoning_emitted,
            ChatStreamKind::Reasoning,
            &mut *on_chunk,
            true,
        )?;
        flush_buffer(
            &state.content,
            &mut state.content_emitted,
            ChatStreamKind::Content,
            &mut *on_chunk,
            true,
        )?;
        let reasoning_part_active = state.reasoning_part_active;
        let signature = state.thinking_signature.take();
        let mut result = finalize_stream_result(
            state.content,
            state.reasoning,
            state.usage,
            state.tool_calls.finish(),
            dsml,
        )?;
        result.thinking_signature = signature;
        if reasoning_part_active {
            on_chunk(ChatStreamChunk {
                kind: ChatStreamKind::ReasoningPartEnd,
                text: String::new(),
            })?;
        }
        Ok(result)
    }

    async fn chat_responses_stream<F>(
        &self,
        messages: Vec<ChatMessage>,
        tools: Vec<ToolDefinition>,
        previous_response_id: Option<&str>,
        request_id: &str,
        on_chunk: &mut F,
    ) -> Result<Option<ChatResult>>
    where
        F: FnMut(ChatStreamChunk) -> Result<()>,
    {
        let extra_body = sanitize_extra_body(
            self.provider.extra_body.clone(),
            RESPONSES_RESERVED_BODY_KEYS,
        );
        let store_disabled = extra_body
            .as_ref()
            .and_then(|body| body.get("store"))
            .and_then(Value::as_bool)
            == Some(false);
        if store_disabled && !tools.is_empty() {
            bail!("OpenAI Responses tools require response storage; remove store=false or disable tools");
        }
        if previous_response_id.is_some() && store_disabled {
            bail!("OpenAI Responses tool continuation requires response storage; remove store=false or disable tools");
        }
        let request = ResponsesRequest {
            model: self.provider.default_model.clone(),
            input: lower_responses_messages(messages),
            instructions: None,
            previous_response_id: previous_response_id.map(str::to_string),
            stream: true,
            tools: (!tools.is_empty()).then(|| lower_responses_tools(tools)),
            reasoning: self.responses_reasoning(),
            temperature: Some(self.provider.temperature),
            extra_body,
        };
        let reasoning_effort = request
            .reasoning
            .as_ref()
            .and_then(|reasoning| reasoning.effort.as_deref())
            .unwrap_or("disabled");
        let reasoning_summary = request
            .reasoning
            .as_ref()
            .and_then(|reasoning| reasoning.summary.as_deref())
            .unwrap_or("disabled");
        tracing::debug!(
            request_id,
            provider = %self.provider.id,
            model = %self.provider.default_model,
            reasoning_effort,
            reasoning_summary,
            "{}",
            t("Responses request configured", "Responses 请求配置完成")
        );
        let url = format!("{}/responses", self.provider.base_url.trim_end_matches('/'));
        let response = self
            .send_with_transport_retry(request_id, "responses.send", || {
                self.client
                    .post(&url)
                    .bearer_auth(&self.api_key)
                    .json(&request)
            })
            .await?;
        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            if responses_unsupported(status.as_u16(), &body) {
                return Ok(None);
            }
            return Err(anyhow::anyhow!(
                "{} ({status}): {body}",
                t("responses stream request failed", "Responses 流式请求失败")
            )
            .context(HttpStatusFailure::classify(status.as_u16(), &body)));
        }

        let dsml = dsml_enabled_for(&self.provider);
        let mut buffer = Utf8LineBuffer::default();
        let mut content = String::new();
        let mut content_emitted = 0usize;
        let mut reasoning = String::new();
        let mut reasoning_emitted = 0usize;
        let mut reasoning_part_active = false;
        let mut usage = None;
        let mut content_started = false;
        let mut output_text_delta_parts = HashSet::new();
        let mut refusal_delta_parts = HashSet::new();
        let mut response_id = None;
        let mut tool_calls = ResponsesToolAccumulator::default();
        let mut stream = response.bytes_stream();
        while let Some(chunk) = self
            .next_response_chunk(&mut stream, "responses.stream")
            .await?
        {
            for line in buffer.push(&chunk)? {
                if handle_responses_sse_line(
                    &line,
                    &mut content,
                    &mut content_emitted,
                    &mut reasoning,
                    &mut reasoning_emitted,
                    &mut reasoning_part_active,
                    &mut usage,
                    &mut content_started,
                    &mut output_text_delta_parts,
                    &mut refusal_delta_parts,
                    &mut response_id,
                    &mut tool_calls,
                    &mut *on_chunk,
                )? {
                    return finalize_responses_stream_result(
                        content,
                        reasoning,
                        usage,
                        tool_calls.finish(),
                        dsml,
                        response_id,
                        store_disabled,
                    )
                    .map(Some);
                }
            }
        }
        let mut terminal_event_received = false;
        for line in buffer.finish()? {
            if handle_responses_sse_line(
                &line,
                &mut content,
                &mut content_emitted,
                &mut reasoning,
                &mut reasoning_emitted,
                &mut reasoning_part_active,
                &mut usage,
                &mut content_started,
                &mut output_text_delta_parts,
                &mut refusal_delta_parts,
                &mut response_id,
                &mut tool_calls,
                &mut *on_chunk,
            )? {
                terminal_event_received = true;
                break;
            }
        }
        if !terminal_event_received {
            bail!("OpenAI Responses stream ended before a terminal event");
        }
        finalize_responses_stream_result(
            content,
            reasoning,
            usage,
            tool_calls.finish(),
            dsml,
            response_id,
            store_disabled,
        )
        .map(Some)
    }

    fn uses_openai_responses(&self) -> bool {
        let model = self.provider.default_model.to_ascii_lowercase();
        model.starts_with("gpt-5")
            || model.starts_with("o1")
            || model.starts_with("o3")
            || model.starts_with("o4")
    }

    fn uses_anthropic_messages(&self) -> bool {
        provider_looks_anthropic(&self.provider)
    }
}

fn stream_chunk_commits_attempt(
    chunk: &ChatStreamChunk,
    reasoning_visibility: ReasoningVisibility,
) -> bool {
    (chunk.kind == ChatStreamKind::ReasoningPartEnd
        && reasoning_visibility != ReasoningVisibility::Hidden)
        || chunk.kind == ChatStreamKind::ToolCall
        || (chunk.kind == ChatStreamKind::Content && !chunk.text.is_empty())
        || (reasoning_visibility == ReasoningVisibility::Full
            && chunk.kind == ChatStreamKind::Reasoning
            && !chunk.text.is_empty())
}

fn reasoning_visibility(config: &AppConfig) -> ReasoningVisibility {
    match config
        .display
        .reasoning
        .trim()
        .to_ascii_lowercase()
        .as_str()
    {
        "hidden" => ReasoningVisibility::Hidden,
        "full" => ReasoningVisibility::Full,
        _ => ReasoningVisibility::Summary,
    }
}

fn reasoning_summary_is_detailed(config: &AppConfig) -> bool {
    config.display.reasoning.trim().eq_ignore_ascii_case("full")
}

fn provider_looks_anthropic(provider: &ProviderConfig) -> bool {
    let id = provider.id.to_ascii_lowercase();
    let display_name = provider.display_name.to_ascii_lowercase();
    let base_url = provider.base_url.to_ascii_lowercase();
    id == "anthropic"
        || id == "claude"
        || id.contains("anthropic")
        || display_name.contains("anthropic")
        || base_url.contains("api.anthropic.com")
        || base_url.contains("anthropic.com/v1")
}

fn provider_looks_claude_related(provider: &ProviderConfig) -> bool {
    let id = provider.id.to_ascii_lowercase();
    let display_name = provider.display_name.to_ascii_lowercase();
    let base_url = provider.base_url.to_ascii_lowercase();
    let model = provider.default_model.to_ascii_lowercase();
    provider_looks_anthropic(provider)
        || id.contains("claude")
        || display_name.contains("claude")
        || model.starts_with("claude")
        || base_url.contains("claude")
}

fn claude_protocol_hint(provider: &ProviderConfig) -> &'static str {
    let protocol = provider.protocol.trim();
    if (protocol.is_empty()
        || protocol.eq_ignore_ascii_case("auto")
        || protocol.eq_ignore_ascii_case("openai-chat"))
        && provider_looks_claude_related(provider)
        && !provider_looks_anthropic(provider)
    {
        return "\nHint: if this provider is the official Anthropic Claude API, set provider protocol to anthropic and base_url to https://api.anthropic.com/v1. If it is an OpenAI-compatible Claude proxy, keep openai-chat/auto.";
    }
    ""
}

fn anthropic_thinking_config() -> Value {
    json!({ "type": "adaptive", "display": "summarized" })
}

fn anthropic_thinking_unsupported(status: u16, body: &str) -> bool {
    if status != 400 && status != 422 {
        return false;
    }
    let body = body.to_ascii_lowercase();
    body.contains("thinking")
        && (body.contains("unsupported")
            || body.contains("not supported")
            || body.contains("unknown")
            || body.contains("invalid")
            || body.contains("unrecognized"))
}

fn responses_unsupported(status: u16, body: &str) -> bool {
    if status == 404 || status == 405 {
        return true;
    }
    if status != 400 {
        return false;
    }
    let body = body.to_ascii_lowercase();
    body.contains("unsupported")
        || body.contains("not supported")
        || body.contains("unknown parameter")
        || body.contains("invalid endpoint")
        || body.contains("not found")
}

fn stream_options_unsupported(status: u16, body: &str) -> bool {
    if status != 400 && status != 422 {
        return false;
    }
    let body = body.to_ascii_lowercase();
    body.contains("stream_options")
        && (body.contains("unsupported")
            || body.contains("not supported")
            || body.contains("unknown")
            || body.contains("unrecognized")
            || body.contains("invalid")
            || body.contains("extra"))
}

fn non_stream_quota_fallback_candidate(status: u16, body: &str) -> bool {
    status == 429 && body.to_ascii_lowercase().contains("insufficient_quota")
}

fn zen_upstream_failed(provider: &ProviderConfig, status: u16, body: &str) -> bool {
    status == 400
        && provider.base_url.trim_end_matches('/') == OPENCODE_ZEN_BASE_URL
        && body
            .to_ascii_lowercase()
            .contains("upstream request failed")
}

#[derive(Debug, Clone, Serialize)]
struct ChatRequest {
    model: String,
    messages: Vec<ChatMessage>,
    temperature: f32,
    stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    stream_options: Option<ChatStreamOptions>,
    /// Only set by cache-keepalive pings; normal chat leaves the provider
    /// default in place.
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<ToolDefinition>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    chat_template_kwargs: Option<ChatTemplateKwargs>,
    #[serde(flatten)]
    #[serde(skip_serializing_if = "Option::is_none")]
    extra_body: Option<Map<String, Value>>,
}

#[derive(Debug, Clone, Serialize)]
struct ChatStreamOptions {
    include_usage: bool,
}

#[derive(Debug, Serialize)]
struct ResponsesRequest {
    model: String,
    input: Vec<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    instructions: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    previous_response_id: Option<String>,
    stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<Value>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning: Option<ResponsesReasoning>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
    #[serde(flatten)]
    #[serde(skip_serializing_if = "Option::is_none")]
    extra_body: Option<Map<String, Value>>,
}

#[derive(Debug, Serialize)]
struct ResponsesReasoning {
    #[serde(skip_serializing_if = "Option::is_none")]
    effort: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    summary: Option<String>,
}

fn default_responses_reasoning(summary: &str) -> ResponsesReasoning {
    ResponsesReasoning {
        effort: Some("medium".to_string()),
        summary: Some(summary.to_string()),
    }
}

#[derive(Debug, Serialize)]
struct AnthropicRequest {
    model: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    system: Option<String>,
    messages: Vec<AnthropicMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<AnthropicTool>>,
    stream: bool,
    max_tokens: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    thinking: Option<Value>,
    #[serde(flatten)]
    #[serde(skip_serializing_if = "Option::is_none")]
    extra_body: Option<Map<String, Value>>,
}

#[derive(Debug, Serialize)]
struct AnthropicMessage {
    role: String,
    content: Vec<AnthropicContentBlock>,
}

#[derive(Debug, Serialize)]
#[serde(tag = "type")]
enum AnthropicContentBlock {
    #[serde(rename = "text")]
    Text { text: String },
    #[serde(rename = "image")]
    Image { source: AnthropicImageSource },
    #[serde(rename = "tool_use")]
    ToolUse {
        id: String,
        name: String,
        input: Value,
    },
    #[serde(rename = "tool_result")]
    ToolResult {
        tool_use_id: String,
        content: String,
    },
    /// Extended-thinking block replayed on assistant tool_use turns. Anthropic
    /// 400s a thinking-enabled tool loop whose assistant turns omit the block.
    #[serde(rename = "thinking")]
    Thinking { thinking: String, signature: String },
}

#[derive(Debug, Serialize)]
#[serde(tag = "type")]
enum AnthropicImageSource {
    #[serde(rename = "base64")]
    Base64 { media_type: String, data: String },
    #[serde(rename = "url")]
    Url { url: String },
}

#[derive(Debug, Serialize)]
struct AnthropicTool {
    name: String,
    description: String,
    input_schema: Value,
}

#[derive(Debug, Clone, Serialize)]
struct ChatTemplateKwargs {
    enable_thinking: bool,
}

/// DeepSeek thinking mode 400s an assistant tool_calls turn whose
/// `reasoning_content` KEY is absent from the request JSON, while many other
/// OpenAI-compatible gateways reject the unknown field outright. Send the key
/// only to providers known to understand it and strip it everywhere else, so
/// the transport copy stays byte-identical to the pre-A17 shape on unrelated
/// endpoints (prompt-cache prefix preserved).
fn provider_accepts_reasoning_content(provider: &ProviderConfig) -> bool {
    let haystack = format!(
        "{} {} {}",
        provider.id.to_ascii_lowercase(),
        provider.base_url.to_ascii_lowercase(),
        provider.default_model.to_ascii_lowercase()
    );
    ["deepseek", "glm-", "zhipu", "bigmodel", "kimi", "moonshot"]
        .iter()
        .any(|needle| haystack.contains(needle))
}

fn prepare_chat_messages_for_provider(
    provider: &ProviderConfig,
    mut messages: Vec<ChatMessage>,
) -> Vec<ChatMessage> {
    if !provider_accepts_reasoning_content(provider) {
        for message in &mut messages {
            message.reasoning_content = None;
        }
    }
    messages
}

fn taotoken_glm_chat_template_kwargs(provider: &ProviderConfig) -> Option<ChatTemplateKwargs> {
    let base_url = provider.base_url.to_ascii_lowercase();
    let model = provider.default_model.to_ascii_lowercase();
    if base_url.contains("taotoken.net") && model.starts_with("glm") {
        Some(ChatTemplateKwargs {
            enable_thinking: true,
        })
    } else {
        None
    }
}

fn lower_responses_messages(messages: Vec<ChatMessage>) -> Vec<Value> {
    messages
        .into_iter()
        .flat_map(|message| match message.role.as_str() {
            "system" => vec![json!({"role": "system", "content": chat_content_text(message.content)})],
            "user" => vec![json!({"role": "user", "content": lower_responses_user_content(message.content)})],
            "assistant" => lower_responses_assistant_message(message),
            "tool" => vec![json!({"type": "function_call_output", "call_id": message.tool_call_id.unwrap_or_default(), "output": chat_content_text(message.content)})],
            role => vec![json!({"role": role, "content": chat_content_text(message.content)})],
        })
        .collect()
}

fn lower_responses_assistant_message(message: ChatMessage) -> Vec<Value> {
    let mut items = Vec::new();
    let text = chat_content_text(message.content);
    if !text.trim().is_empty() {
        items.push(json!({"role": "assistant", "content": text}));
    }
    if let Some(tool_calls) = message.tool_calls {
        items.extend(tool_calls.into_iter().map(|call| {
            json!({
                "type": "function_call",
                "call_id": call.id,
                "name": call.function.name,
                "arguments": call.function.arguments,
            })
        }));
    }
    items
}

fn lower_responses_user_content(content: Option<super::ChatContent>) -> Vec<Value> {
    match content {
        Some(super::ChatContent::Parts(parts)) => parts
            .into_iter()
            .map(|part| match part {
                super::ChatContentPart::Text { text } => {
                    json!({"type": "input_text", "text": text})
                }
                super::ChatContentPart::ImageUrl { image_url } => {
                    json!({"type": "input_image", "image_url": image_url.url})
                }
            })
            .collect(),
        Some(super::ChatContent::Text(text)) => vec![json!({"type": "input_text", "text": text})],
        None => vec![json!({"type": "input_text", "text": ""})],
    }
}

fn chat_content_text(content: Option<super::ChatContent>) -> String {
    match content {
        Some(super::ChatContent::Text(text)) => text,
        Some(super::ChatContent::Parts(parts)) => parts
            .into_iter()
            .filter_map(|part| match part {
                super::ChatContentPart::Text { text } => Some(text),
                super::ChatContentPart::ImageUrl { .. } => None,
            })
            .collect::<Vec<_>>()
            .join(""),
        None => String::new(),
    }
}

fn lower_responses_tools(tools: Vec<ToolDefinition>) -> Vec<Value> {
    tools
        .into_iter()
        .map(|tool| {
            json!({
                "type": "function",
                "name": tool.function.name,
                "description": tool.function.description,
                "parameters": openai_tool_input_schema(tool.function.parameters),
                "strict": false,
            })
        })
        .collect()
}

fn lower_anthropic_system(messages: &[ChatMessage]) -> Option<String> {
    messages
        .iter()
        .take_while(|message| message.role == "system")
        .map(|message| chat_content_text_ref(message.content.as_ref()))
        .filter(|text| !text.trim().is_empty())
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_string()
        .into_non_empty()
}

fn lower_anthropic_messages(messages: Vec<ChatMessage>) -> Vec<AnthropicMessage> {
    let mut output = Vec::new();
    let mut skipped_initial_system = true;
    for message in messages {
        if skipped_initial_system && message.role == "system" {
            continue;
        }
        skipped_initial_system = false;
        match message.role.as_str() {
            "user" => output.push(AnthropicMessage {
                role: "user".to_string(),
                content: lower_anthropic_user_content(message.content),
            }),
            "assistant" => output.push(AnthropicMessage {
                role: "assistant".to_string(),
                content: lower_anthropic_assistant_content(message),
            }),
            "tool" => output.push(AnthropicMessage {
                role: "user".to_string(),
                content: vec![AnthropicContentBlock::ToolResult {
                    tool_use_id: message.tool_call_id.unwrap_or_default(),
                    content: chat_content_text(message.content),
                }],
            }),
            "system" => output.push(AnthropicMessage {
                role: "user".to_string(),
                content: vec![AnthropicContentBlock::Text {
                    text: wrap_system_update(chat_content_text(message.content)),
                }],
            }),
            _ => output.push(AnthropicMessage {
                role: "user".to_string(),
                content: vec![AnthropicContentBlock::Text {
                    text: chat_content_text(message.content),
                }],
            }),
        }
    }
    output
}

fn lower_anthropic_user_content(content: Option<super::ChatContent>) -> Vec<AnthropicContentBlock> {
    match content {
        Some(super::ChatContent::Parts(parts)) => parts
            .into_iter()
            .filter_map(|part| match part {
                super::ChatContentPart::Text { text } => Some(AnthropicContentBlock::Text { text }),
                super::ChatContentPart::ImageUrl { image_url } => {
                    lower_anthropic_image_url(&image_url.url)
                }
            })
            .collect(),
        Some(super::ChatContent::Text(text)) => vec![AnthropicContentBlock::Text { text }],
        None => vec![AnthropicContentBlock::Text {
            text: String::new(),
        }],
    }
}

fn lower_anthropic_image_url(url: &str) -> Option<AnthropicContentBlock> {
    if url.starts_with("http://") || url.starts_with("https://") {
        return Some(AnthropicContentBlock::Image {
            source: AnthropicImageSource::Url {
                url: url.to_string(),
            },
        });
    }
    let data = url.strip_prefix("data:")?;
    let (media_type, base64) = data.split_once(";base64,")?;
    Some(AnthropicContentBlock::Image {
        source: AnthropicImageSource::Base64 {
            media_type: media_type.to_string(),
            data: base64.to_string(),
        },
    })
}

fn lower_anthropic_assistant_content(message: ChatMessage) -> Vec<AnthropicContentBlock> {
    let mut content = Vec::new();
    let has_tool_calls = message
        .tool_calls
        .as_ref()
        .is_some_and(|calls| !calls.is_empty());
    if has_tool_calls {
        if let (Some(signature), Some(thinking)) = (
            message.thinking_signature.as_ref(),
            message.reasoning_content.as_ref(),
        ) {
            if !thinking.trim().is_empty() && !signature.trim().is_empty() {
                content.push(AnthropicContentBlock::Thinking {
                    thinking: thinking.clone(),
                    signature: signature.clone(),
                });
            }
        }
    }
    let text = chat_content_text(message.content);
    if !text.trim().is_empty() {
        content.push(AnthropicContentBlock::Text { text });
    }
    if let Some(tool_calls) = message.tool_calls {
        content.extend(
            tool_calls
                .into_iter()
                .map(|call| AnthropicContentBlock::ToolUse {
                    id: call.id,
                    name: call.function.name,
                    input: serde_json::from_str(&call.function.arguments)
                        .unwrap_or_else(|_| json!({})),
                }),
        );
    }
    if content.is_empty() {
        content.push(AnthropicContentBlock::Text {
            text: String::new(),
        });
    }
    content
}

fn lower_anthropic_tools(tools: Vec<ToolDefinition>) -> Vec<AnthropicTool> {
    tools
        .into_iter()
        .map(|tool| AnthropicTool {
            name: tool.function.name,
            description: tool.function.description,
            input_schema: tool.function.parameters,
        })
        .collect()
}

fn wrap_system_update(text: String) -> String {
    format!(
        "<system-update>\n{}\n</system-update>",
        text.replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
    )
}

trait IntoNonEmpty {
    fn into_non_empty(self) -> Option<String>;
}

impl IntoNonEmpty for String {
    fn into_non_empty(self) -> Option<String> {
        (!self.trim().is_empty()).then_some(self)
    }
}

fn chat_content_text_ref(content: Option<&super::ChatContent>) -> String {
    match content {
        Some(super::ChatContent::Text(text)) => text.clone(),
        Some(super::ChatContent::Parts(parts)) => parts
            .iter()
            .filter_map(|part| match part {
                super::ChatContentPart::Text { text } => Some(text.clone()),
                super::ChatContentPart::ImageUrl { .. } => None,
            })
            .collect::<Vec<_>>()
            .join(""),
        None => String::new(),
    }
}

fn openai_tool_input_schema(schema: Value) -> Value {
    let flattened = flatten_top_level_any_of(schema);
    let normalized = remove_null_any_of(flattened);
    if normalized.is_object() {
        normalized
    } else {
        json!({"type": "object"})
    }
}

fn flatten_top_level_any_of(schema: Value) -> Value {
    let Some(object) = schema.as_object() else {
        return json!({"type": "object"});
    };
    let Some(variants) = object.get("anyOf").and_then(Value::as_array) else {
        let mut cloned = object.clone();
        cloned.insert("type".to_string(), Value::String("object".to_string()));
        return Value::Object(cloned);
    };
    let mut properties = serde_json::Map::new();
    for variant in variants.iter().filter_map(Value::as_object) {
        if let Some(variant_properties) = variant.get("properties").and_then(Value::as_object) {
            for (key, value) in variant_properties {
                properties.insert(key.clone(), value.clone());
            }
        }
    }
    let mut flattened = object
        .iter()
        .filter(|(key, _)| key.as_str() != "anyOf")
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect::<serde_json::Map<_, _>>();
    flattened.insert("type".to_string(), Value::String("object".to_string()));
    flattened.insert("properties".to_string(), Value::Object(properties));
    flattened.insert("additionalProperties".to_string(), Value::Bool(false));
    Value::Object(flattened)
}

fn remove_null_any_of(value: Value) -> Value {
    match value {
        Value::Array(items) => Value::Array(items.into_iter().map(remove_null_any_of).collect()),
        Value::Object(mut object) => {
            let any_of = object.remove("anyOf");
            let mut object = object
                .into_iter()
                .map(|(key, value)| (key, remove_null_any_of(value)))
                .collect::<serde_json::Map<_, _>>();
            let Some(Value::Array(variants)) = any_of else {
                return Value::Object(object);
            };
            let variants = variants
                .into_iter()
                .filter(|variant| variant.get("type").and_then(Value::as_str) != Some("null"))
                .map(remove_null_any_of)
                .collect::<Vec<_>>();
            if variants.len() == 1 {
                if let Some(variant_object) =
                    variants.first().and_then(|item| item.as_object().cloned())
                {
                    object.extend(variant_object);
                    return Value::Object(object);
                }
            }
            object.insert("anyOf".to_string(), Value::Array(variants));
            Value::Object(object)
        }
        value => value,
    }
}

#[derive(Debug, Deserialize)]
struct ChatStreamResponse {
    #[serde(default, deserialize_with = "null_as_default")]
    choices: Vec<ChatStreamChoice>,
    #[serde(default, deserialize_with = "null_as_default")]
    usage: Option<Usage>,
    #[serde(default, deserialize_with = "null_as_default")]
    error: Option<Value>,
}

#[derive(Debug, Deserialize)]
struct ChatCompletionResponse {
    #[serde(default, deserialize_with = "null_as_default")]
    choices: Vec<ChatCompletionChoice>,
    #[serde(default, deserialize_with = "null_as_default")]
    usage: Option<Usage>,
    #[serde(default, deserialize_with = "null_as_default")]
    error: Option<Value>,
}

#[derive(Debug, Deserialize)]
struct ChatCompletionChoice {
    #[serde(default, deserialize_with = "null_as_default")]
    finish_reason: Option<String>,
    #[serde(default)]
    message: ChatChoiceMessage,
}

#[derive(Debug, Deserialize)]
struct ChatStreamChoice {
    #[serde(default, deserialize_with = "null_as_default")]
    finish_reason: Option<String>,
    #[serde(default)]
    delta: ChatChoiceMessage,
}

#[derive(Debug, Default, Deserialize)]
struct ChatChoiceMessage {
    #[serde(default, deserialize_with = "null_as_default")]
    content: Option<String>,
    #[serde(default, deserialize_with = "null_as_default")]
    reasoning_content: Option<String>,
    #[serde(default, deserialize_with = "null_as_default")]
    reasoning: Option<String>,
    #[serde(default, deserialize_with = "null_as_default")]
    thinking: Option<String>,
    #[serde(default, deserialize_with = "null_as_default")]
    thinking_content: Option<String>,
    #[serde(default, deserialize_with = "null_as_default")]
    reasoning_text: Option<String>,
    #[serde(default, deserialize_with = "null_as_default")]
    reasoning_details: Option<serde_json::Value>,
    #[serde(default, deserialize_with = "null_as_default")]
    tool_calls: Vec<ToolCallDelta>,
}

fn null_as_default<'de, D, T>(deserializer: D) -> std::result::Result<T, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Default + Deserialize<'de>,
{
    Ok(Option::<T>::deserialize(deserializer)?.unwrap_or_default())
}

#[derive(Debug, Default, Deserialize)]
struct ToolCallDelta {
    #[serde(default)]
    index: usize,
    #[serde(default, deserialize_with = "null_as_default")]
    id: Option<String>,
    #[serde(rename = "type", default, deserialize_with = "null_as_default")]
    kind: Option<String>,
    #[serde(default)]
    function: ToolCallFunctionDelta,
}

#[derive(Debug, Default, Deserialize)]
struct ToolCallFunctionDelta {
    #[serde(default, deserialize_with = "null_as_default")]
    name: Option<String>,
    #[serde(default, deserialize_with = "null_as_default")]
    arguments: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ResponsesStreamEvent {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default, deserialize_with = "null_as_default")]
    delta: Option<String>,
    #[serde(default, deserialize_with = "null_as_default")]
    arguments: Option<String>,
    #[serde(default, deserialize_with = "null_as_default")]
    name: Option<String>,
    #[serde(default, deserialize_with = "null_as_default")]
    refusal: Option<String>,
    #[serde(default, deserialize_with = "null_as_default")]
    text: Option<String>,
    #[serde(default, deserialize_with = "null_as_default")]
    content_index: Option<usize>,
    #[serde(default, deserialize_with = "null_as_default")]
    item_id: Option<String>,
    #[serde(default, deserialize_with = "null_as_default")]
    item: Option<ResponsesStreamItem>,
    #[serde(default, deserialize_with = "null_as_default")]
    response: Option<ResponsesStreamResponse>,
}

#[derive(Debug, Deserialize)]
struct ResponsesStreamItem {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default, deserialize_with = "null_as_default")]
    id: Option<String>,
    #[serde(default, deserialize_with = "null_as_default")]
    call_id: Option<String>,
    #[serde(default, deserialize_with = "null_as_default")]
    name: Option<String>,
    #[serde(default, deserialize_with = "null_as_default")]
    arguments: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ResponsesStreamResponse {
    #[serde(default, deserialize_with = "null_as_default")]
    id: Option<String>,
    #[serde(default, deserialize_with = "null_as_default")]
    usage: Option<ResponsesUsage>,
    #[serde(default, deserialize_with = "null_as_default")]
    incomplete_details: Option<ResponsesIncompleteDetails>,
}

#[derive(Debug, Deserialize)]
struct ResponsesIncompleteDetails {
    #[serde(default, deserialize_with = "null_as_default")]
    reason: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ResponsesUsage {
    #[serde(default)]
    input_tokens: u64,
    #[serde(default)]
    output_tokens: u64,
    #[serde(default)]
    total_tokens: u64,
    #[serde(default)]
    input_tokens_details: Option<ResponsesInputTokenDetails>,
    #[serde(default)]
    output_tokens_details: Option<ResponsesOutputTokenDetails>,
}

#[derive(Debug, Deserialize)]
struct ResponsesInputTokenDetails {
    #[serde(default)]
    cached_tokens: Option<u64>,
    #[serde(default)]
    cache_write_tokens: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct ResponsesOutputTokenDetails {
    #[serde(default)]
    reasoning_tokens: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct AnthropicStreamEvent {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default, deserialize_with = "null_as_default")]
    index: Option<usize>,
    #[serde(default, deserialize_with = "null_as_default")]
    message: Option<AnthropicStreamMessage>,
    #[serde(default, deserialize_with = "null_as_default")]
    content_block: Option<AnthropicStreamBlock>,
    #[serde(default, deserialize_with = "null_as_default")]
    delta: Option<AnthropicStreamDelta>,
    #[serde(default, deserialize_with = "null_as_default")]
    usage: Option<AnthropicUsage>,
    #[serde(default, deserialize_with = "null_as_default")]
    error: Option<AnthropicStreamError>,
}

#[derive(Debug, Deserialize)]
struct AnthropicStreamMessage {
    #[serde(default, deserialize_with = "null_as_default")]
    usage: Option<AnthropicUsage>,
}

#[derive(Debug, Deserialize)]
struct AnthropicStreamBlock {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default, deserialize_with = "null_as_default")]
    id: Option<String>,
    #[serde(default, deserialize_with = "null_as_default")]
    name: Option<String>,
    #[serde(default, deserialize_with = "null_as_default")]
    text: Option<String>,
    #[serde(default, deserialize_with = "null_as_default")]
    thinking: Option<String>,
}

#[derive(Debug, Deserialize)]
struct AnthropicStreamDelta {
    #[serde(rename = "type", default, deserialize_with = "null_as_default")]
    kind: Option<String>,
    #[serde(default, deserialize_with = "null_as_default")]
    text: Option<String>,
    #[serde(default, deserialize_with = "null_as_default")]
    thinking: Option<String>,
    #[serde(default, deserialize_with = "null_as_default")]
    partial_json: Option<String>,
    #[serde(default, deserialize_with = "null_as_default")]
    signature: Option<String>,
}

#[derive(Debug, Deserialize)]
struct AnthropicUsage {
    #[serde(default)]
    input_tokens: u64,
    #[serde(default)]
    output_tokens: u64,
    #[serde(default)]
    cache_read_input_tokens: Option<u64>,
    #[serde(default)]
    cache_creation_input_tokens: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct AnthropicStreamError {
    #[serde(rename = "type", default, deserialize_with = "null_as_default")]
    kind: Option<String>,
    #[serde(default, deserialize_with = "null_as_default")]
    message: Option<String>,
}

#[derive(Default)]
struct AnthropicStreamState {
    content: String,
    content_emitted: usize,
    reasoning: String,
    reasoning_emitted: usize,
    reasoning_part_active: bool,
    thinking_signature: Option<String>,
    usage: Option<Usage>,
    tool_calls: AnthropicToolAccumulator,
}

/// Upper bound on streamed tool calls per response. Indices come from the
/// upstream stream verbatim; without a cap a single malformed chunk (e.g.
/// index 2^30) makes the accumulator allocate gigabytes. Chunks addressing
/// an index beyond the cap are dropped.
const MAX_STREAM_TOOL_CALLS: usize = 128;
const MAX_STREAM_TOOL_ARGUMENT_BYTES: usize = 4 * 1024 * 1024;
const MAX_STREAM_LINE_BYTES: usize = 4 * 1024 * 1024;
const MAX_STREAM_RESPONSE_BYTES: usize = 64 * 1024 * 1024;

fn append_bounded(target: &mut String, text: &str, limit: usize) {
    let remaining = limit.saturating_sub(target.len());
    if remaining == 0 {
        return;
    }
    let mut end = text.len().min(remaining);
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    target.push_str(&text[..end]);
}

fn bounded_stream_string(mut value: String, limit: usize) -> String {
    if value.len() <= limit {
        return value;
    }
    let mut end = limit;
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    value.truncate(end);
    value
}

#[derive(Debug, Default)]
struct AnthropicToolAccumulator {
    calls: Vec<PartialToolCall>,
}

impl AnthropicToolAccumulator {
    fn start(&mut self, index: usize, block: AnthropicStreamBlock) -> Option<String> {
        if index >= MAX_STREAM_TOOL_CALLS {
            return None;
        }
        while self.calls.len() <= index {
            self.calls.push(PartialToolCall::default());
        }
        let call = &mut self.calls[index];
        call.id = block.id.unwrap_or_else(|| format!("tool-{index}"));
        call.kind = "function".to_string();
        call.name = block.name.unwrap_or_default();
        (!call.name.is_empty()).then(|| call.name.clone())
    }

    fn append_arguments(&mut self, index: usize, text: String) {
        if index >= MAX_STREAM_TOOL_CALLS {
            return;
        }
        while self.calls.len() <= index {
            self.calls.push(PartialToolCall::default());
        }
        append_bounded(
            &mut self.calls[index].arguments,
            &text,
            MAX_STREAM_TOOL_ARGUMENT_BYTES,
        );
    }

    fn finish(self) -> Vec<ToolCall> {
        self.calls
            .into_iter()
            .filter(|call| !call.name.trim().is_empty())
            .map(|call| {
                let id = if call.id.is_empty() {
                    gen_tool_call_id()
                } else {
                    call.id
                };
                ToolCall {
                    id,
                    kind: if call.kind.is_empty() {
                        "function".to_string()
                    } else {
                        call.kind
                    },
                    function: ToolCallFunction {
                        name: call.name,
                        arguments: call.arguments,
                    },
                }
            })
            .collect()
    }
}

#[derive(Debug, Default)]
struct ResponsesToolAccumulator {
    calls: Vec<PartialResponsesToolCall>,
}

#[derive(Debug, Default)]
struct PartialResponsesToolCall {
    item_id: String,
    call: PartialToolCall,
}

impl ResponsesToolAccumulator {
    fn start(&mut self, item: ResponsesStreamItem) -> Option<String> {
        if item.kind != "function_call" || self.calls.len() >= MAX_STREAM_TOOL_CALLS {
            return None;
        }
        let item_id = item.id.unwrap_or_default();
        let name = item.name.unwrap_or_default();
        self.calls.push(PartialResponsesToolCall {
            call: PartialToolCall {
                id: item.call_id.unwrap_or_else(|| item_id.clone()),
                kind: "function".to_string(),
                name: name.clone(),
                arguments: bounded_stream_string(
                    item.arguments.unwrap_or_default(),
                    MAX_STREAM_TOOL_ARGUMENT_BYTES,
                ),
            },
            item_id,
        });
        (!name.is_empty()).then_some(name)
    }

    fn append_arguments(&mut self, item_id: Option<String>, delta: String) {
        if let Some(item_id) = item_id {
            if let Some(partial) = self.calls.iter_mut().find(|call| call.item_id == item_id) {
                append_bounded(
                    &mut partial.call.arguments,
                    &delta,
                    MAX_STREAM_TOOL_ARGUMENT_BYTES,
                );
                return;
            }
            return;
        }
        if let Some(partial) = self.calls.last_mut() {
            append_bounded(
                &mut partial.call.arguments,
                &delta,
                MAX_STREAM_TOOL_ARGUMENT_BYTES,
            );
        }
    }

    fn finish_item(&mut self, item: ResponsesStreamItem) {
        if item.kind != "function_call" {
            return;
        }
        let item_id = item.id.unwrap_or_default();
        let call_id = item.call_id.unwrap_or_default();
        let existing = self.calls.iter_mut().find(|partial| {
            (!item_id.is_empty() && partial.item_id == item_id)
                || (item_id.is_empty() && !call_id.is_empty() && partial.call.id == call_id)
        });
        if let Some(partial) = existing {
            if !call_id.is_empty() {
                partial.call.id = call_id;
            }
            if let Some(name) = item.name {
                partial.call.name = name;
            }
            if let Some(arguments) = item.arguments {
                partial.call.arguments =
                    bounded_stream_string(arguments, MAX_STREAM_TOOL_ARGUMENT_BYTES);
            }
        } else {
            let _ = self.start(ResponsesStreamItem {
                kind: "function_call".to_string(),
                id: (!item_id.is_empty()).then_some(item_id),
                call_id: (!call_id.is_empty()).then_some(call_id),
                name: item.name,
                arguments: item.arguments,
            });
        }
    }

    fn finish_arguments(
        &mut self,
        item_id: Option<String>,
        name: Option<String>,
        arguments: Option<String>,
    ) {
        let Some(item_id) = item_id else {
            return;
        };
        let Some(partial) = self.calls.iter_mut().find(|call| call.item_id == item_id) else {
            return;
        };
        if let Some(name) = name {
            partial.call.name = name;
        }
        if let Some(arguments) = arguments {
            partial.call.arguments =
                bounded_stream_string(arguments, MAX_STREAM_TOOL_ARGUMENT_BYTES);
        }
    }

    fn finish(self) -> Vec<ToolCall> {
        self.calls
            .into_iter()
            .map(|partial| partial.call)
            .filter(|call| !call.name.trim().is_empty())
            .map(|call| {
                let id = if call.id.is_empty() {
                    gen_tool_call_id()
                } else {
                    call.id
                };
                ToolCall {
                    id,
                    kind: call.kind,
                    function: ToolCallFunction {
                        name: call.name,
                        arguments: call.arguments,
                    },
                }
            })
            .collect()
    }
}

#[derive(Debug, Default)]
struct ToolCallAccumulator {
    calls: Vec<PartialToolCall>,
}

#[derive(Debug, Default)]
struct PartialToolCall {
    id: String,
    kind: String,
    name: String,
    arguments: String,
}

impl ToolCallAccumulator {
    fn push(&mut self, delta: ToolCallDelta) -> Option<String> {
        if delta.index >= MAX_STREAM_TOOL_CALLS {
            return None;
        }
        while self.calls.len() <= delta.index {
            self.calls.push(PartialToolCall::default());
        }
        let call = &mut self.calls[delta.index];
        let name_updated = delta.function.name.is_some();
        if let Some(id) = delta.id {
            call.id = id;
        }
        if let Some(kind) = delta.kind {
            call.kind = kind;
        }
        if let Some(name) = delta.function.name {
            // Some gateways resend the complete function name on every delta
            // instead of streaming fragments; blind appending would build
            // "use_tooluse_tool…". Treat an exact repeat (or a full-name replay
            // that extends the current prefix) as a replacement, and only
            // append genuine fragments.
            if call.name.is_empty() {
                append_bounded(&mut call.name, &name, 16 * 1024);
            } else if name == call.name {
                // full-name replay, ignore
            } else if name.starts_with(&call.name) {
                call.name.clear();
                append_bounded(&mut call.name, &name, 16 * 1024);
            } else {
                append_bounded(&mut call.name, &name, 16 * 1024);
            }
        }
        if let Some(arguments) = delta.function.arguments {
            append_bounded(
                &mut call.arguments,
                &arguments,
                MAX_STREAM_TOOL_ARGUMENT_BYTES,
            );
        }
        (name_updated && !call.name.is_empty()).then(|| call.name.clone())
    }

    fn finish(self) -> Vec<ToolCall> {
        self.calls
            .into_iter()
            .filter(|call| !call.name.trim().is_empty())
            .map(|call| {
                let id = if call.id.is_empty() {
                    gen_tool_call_id()
                } else {
                    call.id
                };
                ToolCall {
                    id,
                    kind: if call.kind.is_empty() {
                        "function".to_string()
                    } else {
                        call.kind
                    },
                    function: ToolCallFunction {
                        name: call.name,
                        arguments: call.arguments,
                    },
                }
            })
            .collect()
    }
}

#[derive(Default)]
struct Utf8LineBuffer {
    buffer: Vec<u8>,
    received_bytes: usize,
}

impl Utf8LineBuffer {
    fn push(&mut self, bytes: &[u8]) -> Result<Vec<String>> {
        if self.received_bytes.saturating_add(bytes.len()) > MAX_STREAM_RESPONSE_BYTES {
            bail!("streaming response exceeded {MAX_STREAM_RESPONSE_BYTES} bytes");
        }
        if self.buffer.len().saturating_add(bytes.len()) > MAX_STREAM_LINE_BYTES {
            bail!("streaming response line exceeded {MAX_STREAM_LINE_BYTES} bytes");
        }
        self.received_bytes += bytes.len();
        self.buffer.extend_from_slice(bytes);
        let mut lines = Vec::new();
        while let Some(index) = self.buffer.iter().position(|byte| *byte == b'\n') {
            let mut line = self.buffer.drain(..=index).collect::<Vec<_>>();
            if line.last() == Some(&b'\n') {
                line.pop();
            }
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            lines.push(
                std::str::from_utf8(&line)
                    .context("invalid utf-8 in streaming response")?
                    .to_string(),
            );
        }
        Ok(lines)
    }

    fn finish(mut self) -> Result<Vec<String>> {
        if self.buffer.iter().all(|byte| byte.is_ascii_whitespace()) {
            return Ok(Vec::new());
        }
        if self.buffer.last() == Some(&b'\r') {
            self.buffer.pop();
        }
        Ok(vec![std::str::from_utf8(&self.buffer)
            .context("invalid utf-8 in streaming response")?
            .to_string()])
    }
}

#[derive(Default)]
struct SseDataBuffer {
    lines: Utf8LineBuffer,
    data_lines: Vec<String>,
}

impl SseDataBuffer {
    fn push(&mut self, bytes: &[u8]) -> Result<Vec<String>> {
        let mut events = Vec::new();
        for line in self.lines.push(bytes)? {
            if let Some(event) = self.push_line(&line) {
                events.push(event);
            }
        }
        Ok(events)
    }

    fn finish(mut self) -> Result<Vec<String>> {
        let mut events = Vec::new();
        for line in std::mem::take(&mut self.lines).finish()? {
            if let Some(event) = self.push_line(&line) {
                events.push(event);
            }
        }
        if !self.data_lines.is_empty() {
            events.push(self.data_lines.join("\n"));
        }
        Ok(events)
    }

    fn push_line(&mut self, line: &str) -> Option<String> {
        if line.is_empty() {
            if self.data_lines.is_empty() {
                return None;
            }
            return Some(std::mem::take(&mut self.data_lines).join("\n"));
        }
        if let Some(data) = line.strip_prefix("data:") {
            self.data_lines.push(data.trim_start().to_string());
        }
        None
    }
}

fn clean_response_content(content: String) -> (String, Option<String>) {
    split_tagged_reasoning(clean_plain_text(content))
}

fn is_empty_error(value: &Value) -> bool {
    match value {
        Value::String(text) => text.trim().is_empty(),
        Value::Object(fields) => fields.is_empty(),
        Value::Null => true,
        _ => false,
    }
}

fn provider_error_text(value: &Value) -> String {
    value
        .get("message")
        .and_then(Value::as_str)
        .or_else(|| {
            value
                .get("error")
                .and_then(|error| error.get("message"))
                .and_then(Value::as_str)
        })
        .map(|message| clean_plain_text(message.to_string()))
        .unwrap_or_else(|| clean_plain_text(value.to_string()))
}

fn split_tagged_reasoning(content: String) -> (String, Option<String>) {
    match split_tag_pair(content, "think").or_else(|content| split_tag_pair(content, "thinking")) {
        Ok(result) => result,
        Err(content) => (content, None),
    }
}

fn split_tag_pair(
    content: String,
    tag: &str,
) -> std::result::Result<(String, Option<String>), String> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let Some(start) = content.find(&open) else {
        return Err(content);
    };
    let reasoning_start = start + open.len();
    let Some(relative_end) = content[reasoning_start..].find(&close) else {
        return Ok((content, None));
    };
    let end = reasoning_start + relative_end;
    let reasoning = content[reasoning_start..end].trim().to_string();
    let mut visible = String::new();
    visible.push_str(content[..start].trim_end());
    visible.push_str(content[end + close.len()..].trim_start());
    Ok((
        visible.trim().to_string(),
        (!reasoning.is_empty()).then_some(reasoning),
    ))
}

fn handle_sse_line<F>(
    line: &str,
    content: &mut String,
    content_emitted: &mut usize,
    reasoning: &mut String,
    reasoning_emitted: &mut usize,
    reasoning_part_active: &mut bool,
    finish_reason: &mut Option<String>,
    usage: &mut Option<Usage>,
    tool_calls: &mut ToolCallAccumulator,
    on_chunk: &mut F,
) -> Result<Option<bool>>
where
    F: FnMut(ChatStreamChunk) -> Result<()>,
{
    let Some(data) = line.strip_prefix("data:").map(str::trim) else {
        return Ok(None);
    };
    if data == "[DONE]" {
        flush_buffer(
            reasoning,
            reasoning_emitted,
            ChatStreamKind::Reasoning,
            on_chunk,
            true,
        )?;
        if *reasoning_part_active {
            on_chunk(ChatStreamChunk {
                kind: ChatStreamKind::ReasoningPartEnd,
                text: String::new(),
            })?;
            *reasoning_part_active = false;
        }
        flush_buffer(
            content,
            content_emitted,
            ChatStreamKind::Content,
            on_chunk,
            true,
        )?;
        tracing::debug!(
            finish_reason = finish_reason.as_deref(),
            content_chars = content.chars().count(),
            reasoning_chars = reasoning.chars().count(),
            tool_call_count = tool_calls.calls.len(),
            "{}",
            t(
                "Chat completions stream received DONE",
                "聊天补全流已收到 DONE"
            )
        );
        return Ok(Some(true));
    }
    let response: ChatStreamResponse = serde_json::from_str(data).with_context(|| {
        format!(
            "{}: {}",
            t(
                "invalid chat completions stream response",
                "无效的聊天流式响应",
            ),
            clean_plain_text(data.to_string())
        )
    })?;
    // An empty `error` is not one: some gateways send `{"error":""}` alongside
    // the terminal usage event, and failing the turn over it would turn a
    // normal completion into a spurious error.
    if let Some(error) = response.error.filter(|error| !is_empty_error(error)) {
        bail!(
            "{}: {}",
            t(
                "chat completions stream returned an error",
                "聊天流式响应返回错误"
            ),
            provider_error_text(&error)
        );
    }
    if let Some(next_usage) = response.usage {
        *usage = Some(next_usage);
    }
    for choice in response.choices {
        // An empty string is "absent", not an end signal: some gateways send
        // `"finish_reason": ""` on ordinary chunks.
        if let Some(next_finish_reason) = choice.finish_reason.filter(|reason| !reason.is_empty()) {
            tracing::debug!(
                finish_reason = %next_finish_reason,
                "{}",
                t(
                    "Chat completions stream finish reason received",
                    "已收到聊天补全流结束原因"
                )
            );
            *finish_reason = Some(next_finish_reason);
        }
        let delta = choice.delta;
        if let Some(text) = delta_reasoning_text(&delta) {
            if !*reasoning_part_active {
                if !reasoning.is_empty() && !reasoning.ends_with("\n\n") {
                    reasoning.push_str("\n\n");
                }
                on_chunk(ChatStreamChunk {
                    kind: ChatStreamKind::ReasoningPartStart,
                    text: String::new(),
                })?;
                *reasoning_part_active = true;
            }
            push_buffered_chunk(
                reasoning,
                reasoning_emitted,
                ChatStreamKind::Reasoning,
                text,
                on_chunk,
            )?;
        }
        if let Some(text) = delta.content {
            if !text.is_empty() {
                if *reasoning_part_active {
                    flush_buffer(
                        reasoning,
                        reasoning_emitted,
                        ChatStreamKind::Reasoning,
                        on_chunk,
                        true,
                    )?;
                    on_chunk(ChatStreamChunk {
                        kind: ChatStreamKind::ReasoningPartEnd,
                        text: String::new(),
                    })?;
                    *reasoning_part_active = false;
                }
                push_buffered_chunk(
                    content,
                    content_emitted,
                    ChatStreamKind::Content,
                    text,
                    on_chunk,
                )?;
            }
        }
        for tool_call in delta.tool_calls {
            if *reasoning_part_active {
                flush_buffer(
                    reasoning,
                    reasoning_emitted,
                    ChatStreamKind::Reasoning,
                    on_chunk,
                    true,
                )?;
                on_chunk(ChatStreamChunk {
                    kind: ChatStreamKind::ReasoningPartEnd,
                    text: String::new(),
                })?;
                *reasoning_part_active = false;
            }
            if let Some(name) = tool_calls.push(tool_call) {
                on_chunk(ChatStreamChunk {
                    kind: ChatStreamKind::ToolCall,
                    text: name,
                })?;
            }
        }
    }
    Ok(Some(false))
}

fn handle_responses_sse_line<F>(
    line: &str,
    content: &mut String,
    content_emitted: &mut usize,
    reasoning: &mut String,
    reasoning_emitted: &mut usize,
    reasoning_part_active: &mut bool,
    usage: &mut Option<Usage>,
    content_started: &mut bool,
    output_text_delta_parts: &mut HashSet<(String, usize)>,
    refusal_delta_parts: &mut HashSet<(String, usize)>,
    response_id: &mut Option<String>,
    tool_calls: &mut ResponsesToolAccumulator,
    on_chunk: &mut F,
) -> Result<bool>
where
    F: FnMut(ChatStreamChunk) -> Result<()>,
{
    let Some(data) = line.strip_prefix("data:").map(str::trim) else {
        return Ok(false);
    };
    if data == "[DONE]" {
        flush_buffer(
            reasoning,
            reasoning_emitted,
            ChatStreamKind::Reasoning,
            on_chunk,
            true,
        )?;
        if *reasoning_part_active {
            on_chunk(ChatStreamChunk {
                kind: ChatStreamKind::ReasoningPartEnd,
                text: String::new(),
            })?;
            *reasoning_part_active = false;
        }
        flush_buffer(
            content,
            content_emitted,
            ChatStreamKind::Content,
            on_chunk,
            true,
        )?;
        return Ok(true);
    }
    let event: ResponsesStreamEvent = serde_json::from_str(data).with_context(|| {
        format!(
            "{}: {}",
            t(
                "invalid responses stream event",
                "无效的 Responses 流式事件"
            ),
            clean_plain_text(data.to_string())
        )
    })?;
    if let Some(id) = event
        .response
        .as_ref()
        .and_then(|response| response.id.as_deref())
        .filter(|id| !id.trim().is_empty())
    {
        *response_id = Some(id.to_string());
    }
    if event.kind.starts_with("response.reasoning")
        || matches!(
            event.kind.as_str(),
            "response.output_item.added" | "response.completed" | "response.incomplete"
        )
    {
        let item_kind = event.item.as_ref().map(|item| item.kind.as_str());
        let delta_chars = event.delta.as_deref().map(|delta| delta.chars().count());
        let reasoning_tokens = event
            .response
            .as_ref()
            .and_then(|response| response.usage.as_ref())
            .and_then(|usage| usage.output_tokens_details.as_ref())
            .and_then(|details| details.reasoning_tokens);
        tracing::debug!(
            event_type = %event.kind,
            item_kind = ?item_kind,
            delta_chars = ?delta_chars,
            reasoning_tokens = ?reasoning_tokens,
            "{}",
            t("Responses stream milestone", "Responses 流关键节点")
        );
    }
    let content_part_key = (
        event.item_id.clone().unwrap_or_default(),
        event.content_index.unwrap_or_default(),
    );
    match event.kind.as_str() {
        "response.output_text.delta"
        | "response.output_text.done"
        | "response.refusal.delta"
        | "response.refusal.done" => {
            let text = match event.kind.as_str() {
                "response.output_text.delta" => {
                    let text = event.delta.unwrap_or_default();
                    if !text.is_empty() {
                        output_text_delta_parts.insert(content_part_key.clone());
                    }
                    text
                }
                "response.output_text.done"
                    if !output_text_delta_parts.contains(&content_part_key) =>
                {
                    event.text.unwrap_or_default()
                }
                "response.output_text.done" => String::new(),
                "response.refusal.delta" => {
                    let text = event.delta.unwrap_or_default();
                    if !text.is_empty() {
                        refusal_delta_parts.insert(content_part_key.clone());
                    }
                    text
                }
                "response.refusal.done" if !refusal_delta_parts.contains(&content_part_key) => {
                    event.refusal.unwrap_or_default()
                }
                "response.refusal.done" => String::new(),
                _ => String::new(),
            };
            if text.is_empty() {
                return Ok(false);
            }
            if *reasoning_part_active {
                flush_buffer(
                    reasoning,
                    reasoning_emitted,
                    ChatStreamKind::Reasoning,
                    on_chunk,
                    true,
                )?;
                on_chunk(ChatStreamChunk {
                    kind: ChatStreamKind::ReasoningPartEnd,
                    text: String::new(),
                })?;
                *reasoning_part_active = false;
            }
            *content_started = true;
            push_buffered_chunk(
                content,
                content_emitted,
                ChatStreamKind::Content,
                text,
                on_chunk,
            )?;
        }
        "response.reasoning_text.delta"
        | "response.reasoning_summary.delta"
        | "response.reasoning_summary_text.delta" => {
            if let Some(text) = event.delta {
                if !*reasoning_part_active {
                    if !reasoning.is_empty() && !reasoning.ends_with("\n\n") {
                        reasoning.push_str("\n\n");
                    }
                    on_chunk(ChatStreamChunk {
                        kind: ChatStreamKind::ReasoningPartStart,
                        text: String::new(),
                    })?;
                    *reasoning_part_active = true;
                }
                push_buffered_chunk(
                    reasoning,
                    reasoning_emitted,
                    ChatStreamKind::Reasoning,
                    text,
                    on_chunk,
                )?;
            }
        }
        "response.reasoning_text.done"
        | "response.reasoning_summary.done"
        | "response.reasoning_summary_text.done" => {
            flush_buffer(
                reasoning,
                reasoning_emitted,
                ChatStreamKind::Reasoning,
                on_chunk,
                true,
            )?;
            if *reasoning_part_active {
                on_chunk(ChatStreamChunk {
                    kind: ChatStreamKind::ReasoningPartEnd,
                    text: String::new(),
                })?;
                *reasoning_part_active = false;
            }
            if !*content_started && !reasoning.trim().is_empty() {
                *content_started = true;
                on_chunk(ChatStreamChunk {
                    kind: ChatStreamKind::Content,
                    text: String::new(),
                })?;
            }
        }
        "response.output_item.added" => {
            if *reasoning_part_active {
                flush_buffer(
                    reasoning,
                    reasoning_emitted,
                    ChatStreamKind::Reasoning,
                    on_chunk,
                    true,
                )?;
                on_chunk(ChatStreamChunk {
                    kind: ChatStreamKind::ReasoningPartEnd,
                    text: String::new(),
                })?;
                *reasoning_part_active = false;
            }
            if let Some(item) = event.item {
                if let Some(name) = tool_calls.start(item) {
                    on_chunk(ChatStreamChunk {
                        kind: ChatStreamKind::ToolCall,
                        text: name,
                    })?;
                }
            }
        }
        "response.reasoning_summary_part.added" => {
            if *reasoning_part_active {
                flush_buffer(
                    reasoning,
                    reasoning_emitted,
                    ChatStreamKind::Reasoning,
                    on_chunk,
                    true,
                )?;
                on_chunk(ChatStreamChunk {
                    kind: ChatStreamKind::ReasoningPartEnd,
                    text: String::new(),
                })?;
            }
            if !reasoning.is_empty() && !reasoning.ends_with("\n\n") {
                reasoning.push_str("\n\n");
            }
            on_chunk(ChatStreamChunk {
                kind: ChatStreamKind::ReasoningPartStart,
                text: String::new(),
            })?;
            *reasoning_part_active = true;
        }
        "response.reasoning_summary_part.done" => {
            flush_buffer(
                reasoning,
                reasoning_emitted,
                ChatStreamKind::Reasoning,
                on_chunk,
                true,
            )?;
            if *reasoning_part_active {
                on_chunk(ChatStreamChunk {
                    kind: ChatStreamKind::ReasoningPartEnd,
                    text: String::new(),
                })?;
                *reasoning_part_active = false;
            }
        }
        "response.function_call_arguments.delta" => {
            if let Some(delta) = event.delta {
                tool_calls.append_arguments(event.item_id, delta);
            }
        }
        "response.function_call_arguments.done" => {
            tool_calls.finish_arguments(event.item_id, event.name, event.arguments);
        }
        "response.output_item.done" => {
            if let Some(item) = event.item {
                tool_calls.finish_item(item);
            }
        }
        "response.completed" => {
            if let Some(next_usage) = event.response.and_then(|response| response.usage) {
                let total_tokens = if next_usage.total_tokens > 0 {
                    next_usage.total_tokens
                } else {
                    next_usage
                        .input_tokens
                        .saturating_add(next_usage.output_tokens)
                };
                let input_details = next_usage.input_tokens_details.as_ref();
                let cache_read = input_details.and_then(|details| details.cached_tokens);
                let cache_write = input_details.and_then(|details| details.cache_write_tokens);
                let reasoning_tokens = next_usage
                    .output_tokens_details
                    .as_ref()
                    .and_then(|details| details.reasoning_tokens)
                    .unwrap_or(0);
                *usage = Some(Usage {
                    prompt_tokens: next_usage.input_tokens,
                    completion_tokens: next_usage.output_tokens,
                    total_tokens,
                    cache_read_tokens: cache_read.unwrap_or(0),
                    cache_write_tokens: cache_write.unwrap_or(0),
                    reasoning_tokens,
                    cache_reported: cache_read.is_some() || cache_write.is_some(),
                    ..Usage::default()
                });
            }
            flush_buffer(
                reasoning,
                reasoning_emitted,
                ChatStreamKind::Reasoning,
                on_chunk,
                true,
            )?;
            if *reasoning_part_active {
                on_chunk(ChatStreamChunk {
                    kind: ChatStreamKind::ReasoningPartEnd,
                    text: String::new(),
                })?;
                *reasoning_part_active = false;
            }
            flush_buffer(
                content,
                content_emitted,
                ChatStreamKind::Content,
                on_chunk,
                true,
            )?;
            return Ok(true);
        }
        "response.incomplete" => {
            let reason = event
                .response
                .as_ref()
                .and_then(|response| response.incomplete_details.as_ref())
                .and_then(|details| details.reason.as_deref())
                .unwrap_or("unknown");
            bail!("OpenAI Responses response was incomplete: {reason}");
        }
        "error" | "response.failed" => {
            bail!(
                "OpenAI Responses stream failed: {}",
                clean_plain_text(data.to_string())
            );
        }
        _ => {}
    }
    Ok(false)
}

fn handle_anthropic_sse_data<F>(
    data: &str,
    state: &mut AnthropicStreamState,
    on_chunk: &mut F,
) -> Result<bool>
where
    F: FnMut(ChatStreamChunk) -> Result<()>,
{
    if data == "[DONE]" {
        flush_anthropic_state(state, on_chunk)?;
        return Ok(true);
    }
    let event: AnthropicStreamEvent = serde_json::from_str(data).with_context(|| {
        format!(
            "{}: {}",
            t(
                "invalid anthropic messages stream event",
                "无效的 Anthropic Messages 流式事件"
            ),
            clean_plain_text(data.to_string())
        )
    })?;
    match event.kind.as_str() {
        "message_start" => {
            if let Some(usage) = event.message.and_then(|message| message.usage) {
                merge_anthropic_usage(&mut state.usage, usage);
            }
        }
        "content_block_start" => {
            if let Some(block) = event.content_block {
                match block.kind.as_str() {
                    "tool_use" | "server_tool_use" => {
                        if let Some(index) = event.index {
                            if let Some(name) = state.tool_calls.start(index, block) {
                                on_chunk(ChatStreamChunk {
                                    kind: ChatStreamKind::ToolCall,
                                    text: name,
                                })?;
                            }
                        }
                    }
                    "text" => {
                        if state.reasoning_part_active {
                            flush_buffer(
                                &mut state.reasoning,
                                &mut state.reasoning_emitted,
                                ChatStreamKind::Reasoning,
                                on_chunk,
                                true,
                            )?;
                            on_chunk(ChatStreamChunk {
                                kind: ChatStreamKind::ReasoningPartEnd,
                                text: String::new(),
                            })?;
                            state.reasoning_part_active = false;
                        }
                        if let Some(text) = block.text {
                            push_buffered_chunk(
                                &mut state.content,
                                &mut state.content_emitted,
                                ChatStreamKind::Content,
                                text,
                                on_chunk,
                            )?;
                        }
                    }
                    "thinking" => {
                        if state.reasoning_part_active {
                            flush_buffer(
                                &mut state.reasoning,
                                &mut state.reasoning_emitted,
                                ChatStreamKind::Reasoning,
                                on_chunk,
                                true,
                            )?;
                            on_chunk(ChatStreamChunk {
                                kind: ChatStreamKind::ReasoningPartEnd,
                                text: String::new(),
                            })?;
                        }
                        if !state.reasoning.is_empty() && !state.reasoning.ends_with("\n\n") {
                            state.reasoning.push_str("\n\n");
                        }
                        on_chunk(ChatStreamChunk {
                            kind: ChatStreamKind::ReasoningPartStart,
                            text: String::new(),
                        })?;
                        state.reasoning_part_active = true;
                        if let Some(text) = block.thinking {
                            push_buffered_chunk(
                                &mut state.reasoning,
                                &mut state.reasoning_emitted,
                                ChatStreamKind::Reasoning,
                                text,
                                on_chunk,
                            )?;
                        }
                    }
                    _ => {}
                }
            }
        }
        "content_block_delta" => {
            if let Some(delta) = event.delta {
                match delta.kind.as_deref() {
                    Some("text_delta") => {
                        if state.reasoning_part_active {
                            flush_buffer(
                                &mut state.reasoning,
                                &mut state.reasoning_emitted,
                                ChatStreamKind::Reasoning,
                                on_chunk,
                                true,
                            )?;
                            on_chunk(ChatStreamChunk {
                                kind: ChatStreamKind::ReasoningPartEnd,
                                text: String::new(),
                            })?;
                            state.reasoning_part_active = false;
                        }
                        if let Some(text) = delta.text {
                            push_buffered_chunk(
                                &mut state.content,
                                &mut state.content_emitted,
                                ChatStreamKind::Content,
                                text,
                                on_chunk,
                            )?;
                        }
                    }
                    Some("thinking_delta") => {
                        if let Some(text) = delta.thinking {
                            if !state.reasoning_part_active {
                                if !state.reasoning.is_empty() && !state.reasoning.ends_with("\n\n")
                                {
                                    state.reasoning.push_str("\n\n");
                                }
                                on_chunk(ChatStreamChunk {
                                    kind: ChatStreamKind::ReasoningPartStart,
                                    text: String::new(),
                                })?;
                                state.reasoning_part_active = true;
                            }
                            push_buffered_chunk(
                                &mut state.reasoning,
                                &mut state.reasoning_emitted,
                                ChatStreamKind::Reasoning,
                                text,
                                on_chunk,
                            )?;
                        }
                    }
                    Some("input_json_delta") => {
                        if let (Some(index), Some(text)) = (event.index, delta.partial_json) {
                            state.tool_calls.append_arguments(index, text);
                        }
                    }
                    Some("signature_delta") => {
                        state.thinking_signature = delta.signature;
                    }
                    _ => {}
                }
            }
        }
        "content_block_stop" => {
            if state.reasoning_part_active {
                flush_buffer(
                    &mut state.reasoning,
                    &mut state.reasoning_emitted,
                    ChatStreamKind::Reasoning,
                    on_chunk,
                    true,
                )?;
                on_chunk(ChatStreamChunk {
                    kind: ChatStreamKind::ReasoningPartEnd,
                    text: String::new(),
                })?;
                state.reasoning_part_active = false;
            }
        }
        "message_delta" => {
            if let Some(usage) = event.usage {
                merge_anthropic_usage(&mut state.usage, usage);
            }
            flush_anthropic_state(state, on_chunk)?;
        }
        "message_stop" => {
            flush_anthropic_state(state, on_chunk)?;
            return Ok(true);
        }
        "error" => {
            let message = event
                .error
                .map(|error| match (error.kind, error.message) {
                    (Some(kind), Some(message)) => format!("{kind}: {message}"),
                    (Some(kind), None) => kind,
                    (None, Some(message)) => message,
                    (None, None) => "Anthropic Messages stream error".to_string(),
                })
                .unwrap_or_else(|| "Anthropic Messages stream error".to_string());
            bail!("{message}");
        }
        _ => {}
    }
    Ok(false)
}

fn flush_anthropic_state<F>(state: &mut AnthropicStreamState, on_chunk: &mut F) -> Result<()>
where
    F: FnMut(ChatStreamChunk) -> Result<()>,
{
    flush_buffer(
        &state.reasoning,
        &mut state.reasoning_emitted,
        ChatStreamKind::Reasoning,
        on_chunk,
        true,
    )?;
    if state.reasoning_part_active {
        on_chunk(ChatStreamChunk {
            kind: ChatStreamKind::ReasoningPartEnd,
            text: String::new(),
        })?;
        state.reasoning_part_active = false;
    }
    flush_buffer(
        &state.content,
        &mut state.content_emitted,
        ChatStreamKind::Content,
        on_chunk,
        true,
    )
}

fn merge_anthropic_usage(current: &mut Option<Usage>, usage: AnthropicUsage) {
    let previous = current.take().unwrap_or_default();
    let cache_read = usage
        .cache_read_input_tokens
        .unwrap_or(previous.cache_read_tokens);
    let cache_write = usage
        .cache_creation_input_tokens
        .unwrap_or(previous.cache_write_tokens);
    // Anthropic's `input_tokens` excludes both cache reads and cache writes;
    // normalize to the cross-provider invariant `prompt = uncached + read + write`
    // so context accounting does not collapse once cache_control is in play.
    let prompt_tokens = if usage.input_tokens > 0 || cache_read > 0 || cache_write > 0 {
        usage
            .input_tokens
            .saturating_add(cache_read)
            .saturating_add(cache_write)
    } else {
        previous.prompt_tokens
    };
    let completion_tokens = if usage.output_tokens > 0 {
        usage.output_tokens
    } else {
        previous.completion_tokens
    };
    *current = Some(Usage {
        prompt_tokens,
        completion_tokens,
        total_tokens: prompt_tokens.saturating_add(completion_tokens),
        cache_read_tokens: cache_read,
        cache_write_tokens: cache_write,
        cache_reported: previous.cache_reported
            || usage.cache_read_input_tokens.is_some()
            || usage.cache_creation_input_tokens.is_some(),
        reasoning_tokens: previous.reasoning_tokens,
        ..Usage::default()
    });
}

fn delta_reasoning_text(delta: &ChatChoiceMessage) -> Option<String> {
    delta
        .reasoning_content
        .clone()
        .or_else(|| delta.reasoning.clone())
        .or_else(|| delta.thinking.clone())
        .or_else(|| delta.thinking_content.clone())
        .or_else(|| delta.reasoning_text.clone())
        .or_else(|| reasoning_details_text(delta.reasoning_details.as_ref()))
}

fn reasoning_details_text(value: Option<&serde_json::Value>) -> Option<String> {
    let value = value?;
    if let Some(text) = value.as_str() {
        return Some(text.to_string());
    }
    if let Some(array) = value.as_array() {
        let text = array
            .iter()
            .filter_map(|item| {
                item.get("text")
                    .or_else(|| item.get("content"))
                    .and_then(serde_json::Value::as_str)
            })
            .collect::<Vec<_>>()
            .join("");
        return (!text.is_empty()).then_some(text);
    }
    value
        .get("text")
        .or_else(|| value.get("content"))
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
}

fn push_buffered_chunk<F>(
    target: &mut String,
    emitted: &mut usize,
    kind: ChatStreamKind,
    text: String,
    on_chunk: &mut F,
) -> Result<()>
where
    F: FnMut(ChatStreamChunk) -> Result<()>,
{
    if text.is_empty() {
        return Ok(());
    }
    target.push_str(&text);
    flush_buffer(target, emitted, kind, on_chunk, false)
}

fn flush_buffer<F>(
    target: &str,
    emitted: &mut usize,
    kind: ChatStreamKind,
    on_chunk: &mut F,
    final_flush: bool,
) -> Result<()>
where
    F: FnMut(ChatStreamChunk) -> Result<()>,
{
    while *emitted < target.len() {
        let remaining = &target[*emitted..];
        if starts_hidden_prefix(remaining) {
            if let Some(end) = hidden_end_after(target, *emitted) {
                *emitted = end;
                continue;
            }
            if final_flush {
                *emitted = target.len();
            }
            return Ok(());
        }
        let hidden_start = hidden_start_after(target, *emitted);
        let mut safe_end = hidden_start.unwrap_or(target.len());
        if hidden_start.is_none() && !final_flush {
            safe_end =
                safe_end.saturating_sub(partial_hidden_suffix_len(&target[*emitted..safe_end]));
        }
        if safe_end <= *emitted {
            return Ok(());
        }
        let text = target[*emitted..safe_end].to_string();
        *emitted = safe_end;
        if !text.is_empty() {
            on_chunk(ChatStreamChunk { kind, text })?;
        }
    }
    Ok(())
}

fn finalize_responses_stream_result(
    content: String,
    reasoning: String,
    usage: Option<Usage>,
    tool_calls: Vec<ToolCall>,
    dsml_enabled: bool,
    response_id: Option<String>,
    store_disabled: bool,
) -> Result<ChatResult> {
    let mut result = finalize_stream_result(content, reasoning, usage, tool_calls, dsml_enabled)?;
    if result.tool_calls.is_empty() {
        return Ok(result);
    }
    if store_disabled {
        bail!(
            "OpenAI Responses returned tool calls, but store=false prevents stateful continuation"
        );
    }
    let response_id = response_id
        .filter(|id| !id.trim().is_empty())
        .context("OpenAI Responses returned tool calls without a response ID")?;
    result.responses_continuation = Some(Box::new(ResponsesContinuation {
        response_id,
        endpoint_id: String::new(),
    }));
    Ok(result)
}

fn finalize_stream_result(
    content: String,
    reasoning: String,
    usage: Option<Usage>,
    tool_calls: Vec<ToolCall>,
    dsml_enabled: bool,
) -> Result<ChatResult> {
    let usage = usage.map(|mut usage| {
        usage.normalize_cache_fields();
        if usage.cache_reported {
            // v7 Release 1 observability: one absolute-value line per request,
            // à la Reasonix ("in N (M cached / K new)"). Percentages mislead
            // when a turn adds lots of fresh content, so none are shown.
            tracing::info!(
                prompt_tokens = usage.prompt_tokens,
                cache_read = usage.cache_read_tokens,
                cache_write = usage.cache_write_tokens,
                fresh = usage.uncached_prompt_tokens(),
                "prompt cache accounting"
            );
        }
        usage
    });
    let content = clean_plain_text(content);
    let (content, mut dsml_tool_calls) = if dsml_enabled {
        extract_dsml_tool_calls(content)
    } else {
        (content, Vec::new())
    };
    let content = if dsml_enabled {
        strip_orphaned_dsml_tags(content)
    } else {
        content
    };
    let reasoning = clean_plain_text(reasoning);
    let (reasoning, reasoning_dsml_tool_calls) = if dsml_enabled {
        extract_dsml_tool_calls(reasoning)
    } else {
        (reasoning, Vec::new())
    };
    let reasoning = if dsml_enabled {
        strip_orphaned_dsml_tags(reasoning)
    } else {
        reasoning
    };
    dsml_tool_calls.extend(reasoning_dsml_tool_calls);
    let (content, tag_reasoning) = clean_response_content(content);
    let reasoning = if reasoning.trim().is_empty() {
        tag_reasoning
    } else {
        Some(reasoning)
    };
    let tool_calls = if dsml_tool_calls.is_empty() {
        tool_calls
    } else {
        dsml_tool_calls
    };
    if content.trim().is_empty()
        && !reasoning
            .as_ref()
            .is_some_and(|text| !text.trim().is_empty())
        && tool_calls.is_empty()
    {
        bail!(
            "{}",
            t(
                "chat completions stream response was empty",
                "聊天流式响应为空",
            )
        );
    }
    Ok(ChatResult {
        content,
        reasoning: reasoning.filter(|text| !text.trim().is_empty()),
        usage,
        usage_estimated: false,
        tool_calls,
        provider_id: None,
        model: None,
        finish_reason: None,
        thinking_signature: None,
        last_request_usage: None,
        responses_continuation: None,
    })
}

fn dsml_enabled_for(provider: &ProviderConfig) -> bool {
    let base_url = provider.base_url.to_ascii_lowercase();
    let model = provider.default_model.to_ascii_lowercase();
    base_url.contains("taotoken.net") && model.starts_with("glm")
}

const DSML_ANY_PREFIX: &str = "<｜｜DSML";
const DSML_PREFIX: &str = "<｜｜DSML｜｜tool_calls";
const DSML_END: &str = "</｜｜DSML｜｜tool_calls>";
const SYSTEM_REMINDER_PREFIX: &str = "<system-reminder";
const SYSTEM_REMINDER_UNDERSCORE_PREFIX: &str = "<system_reminder";

fn hidden_start_after(target: &str, offset: usize) -> Option<usize> {
    [
        target[offset..].find(DSML_ANY_PREFIX),
        target[offset..].find(SYSTEM_REMINDER_PREFIX),
        target[offset..].find(SYSTEM_REMINDER_UNDERSCORE_PREFIX),
    ]
    .into_iter()
    .flatten()
    .map(|index| offset + index)
    .min()
}

fn starts_hidden_prefix(value: &str) -> bool {
    DSML_ANY_PREFIX.starts_with(value)
        || SYSTEM_REMINDER_PREFIX.starts_with(value)
        || SYSTEM_REMINDER_UNDERSCORE_PREFIX.starts_with(value)
        || value.starts_with(DSML_ANY_PREFIX)
        || value.starts_with(SYSTEM_REMINDER_PREFIX)
        || value.starts_with(SYSTEM_REMINDER_UNDERSCORE_PREFIX)
}

fn partial_hidden_suffix_len(value: &str) -> usize {
    let max_len = value.len().min(
        DSML_ANY_PREFIX
            .len()
            .max(SYSTEM_REMINDER_PREFIX.len())
            .max(SYSTEM_REMINDER_UNDERSCORE_PREFIX.len()),
    );
    for len in (1..=max_len).rev() {
        if !value.is_char_boundary(value.len() - len) {
            continue;
        }
        let suffix = &value[value.len() - len..];
        if DSML_ANY_PREFIX.starts_with(suffix)
            || SYSTEM_REMINDER_PREFIX.starts_with(suffix)
            || SYSTEM_REMINDER_UNDERSCORE_PREFIX.starts_with(suffix)
        {
            return len;
        }
    }
    0
}

fn hidden_end_after(target: &str, offset: usize) -> Option<usize> {
    let remaining = &target[offset..];
    if remaining.starts_with(DSML_ANY_PREFIX) {
        return remaining
            .find(DSML_END)
            .map(|index| offset + index + DSML_END.len());
    }
    for tag in ["system-reminder", "system_reminder"] {
        let open_prefix = format!("<{tag}");
        if remaining.starts_with(&open_prefix) {
            let close = format!("</{tag}>");
            return remaining
                .find(&close)
                .map(|index| offset + index + close.len());
        }
    }
    None
}

fn extract_dsml_tool_calls(mut content: String) -> (String, Vec<ToolCall>) {
    let mut calls = Vec::new();
    let mut index = 0usize;
    while let Some(start) = content.find(DSML_PREFIX) {
        let tag_end = content[start..]
            .find('>')
            .map(|offset| start + offset + 1)
            .unwrap_or(start + DSML_PREFIX.len());
        let body_start = tag_end;
        let Some(relative_end) = content[body_start..].find(DSML_END) else {
            content.replace_range(start.., "");
            break;
        };
        let end = body_start + relative_end;
        let block = content[body_start..end].to_string();
        calls.extend(parse_dsml_block(&block, &mut index));
        content.replace_range(start..end + DSML_END.len(), "");
    }
    (content.trim().to_string(), calls)
}

fn strip_orphaned_dsml_tags(mut content: String) -> String {
    content = content.replace(DSML_END, "");
    content = content.replace(DSML_PREFIX, "");
    content = content.replace("</｜｜DSML｜｜invoke>", "");
    content = content.replace("<｜｜DSML｜｜invoke", "");
    content = content.replace("</｜｜DSML｜｜parameter>", "");
    content = content.replace("<｜｜DSML｜｜parameter", "");
    content.trim().to_string()
}

fn parse_dsml_block(block: &str, index: &mut usize) -> Vec<ToolCall> {
    let mut calls = Vec::new();
    let mut rest = block;
    while let Some(start) = rest.find("<｜｜DSML｜｜invoke") {
        rest = &rest[start..];
        let Some(tag_end) = rest.find('>') else {
            break;
        };
        let tag = &rest[..tag_end];
        let Some(name) = attr_value(tag, "name") else {
            rest = &rest[tag_end..];
            continue;
        };
        let body_start = tag_end + 1;
        let Some(relative_end) = rest[body_start..].find("</｜｜DSML｜｜invoke>") else {
            break;
        };
        let body = &rest[body_start..body_start + relative_end];
        let arguments = parse_dsml_arguments(body);
        *index += 1;
        calls.push(ToolCall {
            id: format!("dsml-tool-call-{index}"),
            kind: "function".to_string(),
            function: ToolCallFunction {
                name,
                arguments: arguments.to_string(),
            },
        });
        rest = &rest[body_start + relative_end + "</｜｜DSML｜｜invoke>".len()..];
    }
    calls
}

fn parse_dsml_arguments(body: &str) -> serde_json::Value {
    let mut map = serde_json::Map::new();
    let mut rest = body;
    while let Some(start) = rest.find("<｜｜DSML｜｜parameter") {
        rest = &rest[start..];
        let Some(tag_end) = rest.find('>') else {
            break;
        };
        let tag = &rest[..tag_end];
        let Some(name) = attr_value(tag, "name") else {
            rest = &rest[tag_end..];
            continue;
        };
        let value_start = tag_end + 1;
        let Some(relative_end) = rest[value_start..].find("</｜｜DSML｜｜parameter>") else {
            break;
        };
        let raw_value = rest[value_start..value_start + relative_end].trim();
        map.insert(name, parse_dsml_value(raw_value));
        rest = &rest[value_start + relative_end + "</｜｜DSML｜｜parameter>".len()..];
    }
    serde_json::Value::Object(map)
}

fn parse_dsml_value(value: &str) -> serde_json::Value {
    let trimmed = value.trim();
    if let Ok(value) = serde_json::from_str(trimmed) {
        return value;
    }
    if let Ok(value) = trimmed.parse::<i64>() {
        return serde_json::Value::Number(value.into());
    }
    serde_json::Value::String(trimmed.trim_matches('"').to_string())
}

fn attr_value(tag: &str, name: &str) -> Option<String> {
    let pattern = format!("{name}=\"");
    let start = tag.find(&pattern)? + pattern.len();
    let end = tag[start..].find('"')?;
    Some(tag[start..start + end].to_string())
}

fn clean_plain_text(mut text: String) -> String {
    for tag in ["system-reminder", "system_reminder"] {
        text = strip_tagged_sections(text, tag);
    }
    text = text.replace("<system-reminder>", "");
    text = text.replace("</system-reminder>", "");
    text = text.replace("<system_reminder>", "");
    text = text.replace("</system_reminder>", "");
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::{ChatContent, ChatContentPart, ImageUrlContent};
    use std::sync::atomic::AtomicUsize;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    #[derive(Debug)]
    struct ResponsesTestOutput {
        content: String,
        chunks: Vec<ChatStreamChunk>,
        response_id: Option<String>,
        terminal: bool,
    }

    fn run_responses_test_events(lines: &[&str]) -> Result<ResponsesTestOutput> {
        let mut content = String::new();
        let mut content_emitted = 0usize;
        let mut reasoning = String::new();
        let mut reasoning_emitted = 0usize;
        let mut reasoning_part_active = false;
        let mut usage = None;
        let mut content_started = false;
        let mut output_text_delta_parts = HashSet::new();
        let mut refusal_delta_parts = HashSet::new();
        let mut response_id = None;
        let mut tool_calls = ResponsesToolAccumulator::default();
        let mut chunks = Vec::new();
        let mut terminal = false;
        let mut on_chunk = |chunk| {
            chunks.push(chunk);
            Ok(())
        };
        for line in lines {
            terminal = handle_responses_sse_line(
                line,
                &mut content,
                &mut content_emitted,
                &mut reasoning,
                &mut reasoning_emitted,
                &mut reasoning_part_active,
                &mut usage,
                &mut content_started,
                &mut output_text_delta_parts,
                &mut refusal_delta_parts,
                &mut response_id,
                &mut tool_calls,
                &mut on_chunk,
            )?;
            if terminal {
                break;
            }
        }
        Ok(ResponsesTestOutput {
            content,
            chunks,
            response_id,
            terminal,
        })
    }

    #[test]
    fn tool_call_accumulators_drop_out_of_range_indices() {
        // A malformed upstream chunk with a huge index must not make the
        // accumulator allocate gigabytes (regression: 160GB VmSize).
        let mut acc = ToolCallAccumulator::default();
        let huge = ToolCallDelta {
            index: 1 << 30,
            id: Some("x".to_string()),
            kind: None,
            function: ToolCallFunctionDelta {
                name: Some("evil".to_string()),
                arguments: None,
            },
        };
        assert!(acc.push(huge).is_none());
        assert!(acc.calls.is_empty());
        let ok = ToolCallDelta {
            index: 0,
            id: Some("a".to_string()),
            kind: None,
            function: ToolCallFunctionDelta {
                name: Some("fine".to_string()),
                arguments: Some("{}".to_string()),
            },
        };
        assert!(acc.push(ok).is_some());
        assert_eq!(acc.calls.len(), 1);

        let mut anthropic = AnthropicToolAccumulator::default();
        assert!(anthropic
            .start(
                usize::MAX,
                AnthropicStreamBlock {
                    kind: "tool_use".to_string(),
                    id: Some("x".to_string()),
                    name: Some("evil".to_string()),
                    text: None,
                    thinking: None,
                },
            )
            .is_none());
        anthropic.append_arguments(1 << 30, "{}".to_string());
        assert!(anthropic.calls.is_empty());
    }

    #[test]
    fn stream_chunk_accepts_null_tool_calls() {
        let raw = r#"{"choices":[{"delta":{"content":"在","tool_calls":null}}]}"#;
        let parsed: ChatStreamResponse = serde_json::from_str(raw).unwrap();

        assert_eq!(parsed.choices.len(), 1);
        assert_eq!(parsed.choices[0].delta.content.as_deref(), Some("在"));
        assert!(parsed.choices[0].delta.tool_calls.is_empty());
    }

    #[test]
    fn stream_chunk_accepts_taotoken_glm_nulls() {
        let raw = r#"{"created":1782742568,"usage":null,"model":"glm_for_coding","id":"9981f6121a31494387131c61bd2ad7a2","choices":[{"finish_reason":null,"matched_stop":null,"delta":{"role":null,"tool_calls":null,"content":"在","reasoning_content":null},"index":0,"logprobs":null}],"object":"chat.completion.chunk"}"#;
        let parsed: ChatStreamResponse = serde_json::from_str(raw).unwrap();

        assert!(parsed.usage.is_none());
        assert_eq!(parsed.choices.len(), 1);
        assert!(parsed.choices[0].finish_reason.is_none());
        assert_eq!(parsed.choices[0].delta.content.as_deref(), Some("在"));
        assert!(parsed.choices[0].delta.reasoning_content.is_none());
        assert!(parsed.choices[0].delta.tool_calls.is_empty());
    }

    #[test]
    fn stream_chunk_emits_glm_reasoning_content() {
        let mut content = String::new();
        let mut content_emitted = 0usize;
        let mut reasoning = String::new();
        let mut reasoning_emitted = 0usize;
        let mut reasoning_part_active = false;
        let mut finish_reason = None;
        let mut usage = None;
        let mut tool_calls = ToolCallAccumulator::default();
        let mut chunks = Vec::new();
        let mut on_chunk = |chunk| {
            chunks.push(chunk);
            Ok(())
        };

        handle_sse_line(
            r#"data: {"choices":[{"finish_reason":"length","delta":{"reasoning_content":"先想一下","content":"","tool_calls":null}}]}"#,
            &mut content,
            &mut content_emitted,
            &mut reasoning,
            &mut reasoning_emitted,
            &mut reasoning_part_active,
            &mut finish_reason,
            &mut usage,
            &mut tool_calls,
            &mut on_chunk,
        )
        .unwrap();

        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].kind, ChatStreamKind::ReasoningPartStart);
        assert_eq!(chunks[1].kind, ChatStreamKind::Reasoning);
        assert_eq!(chunks[1].text, "先想一下");
        assert_eq!(finish_reason.as_deref(), Some("length"));
    }

    #[test]
    fn chat_stream_announces_question_tool_before_arguments() {
        let mut content = String::new();
        let mut content_emitted = 0usize;
        let mut reasoning = String::new();
        let mut reasoning_emitted = 0usize;
        let mut reasoning_part_active = false;
        let mut finish_reason = None;
        let mut usage = None;
        let mut tool_calls = ToolCallAccumulator::default();
        let mut chunks = Vec::new();
        let mut on_chunk = |chunk| {
            chunks.push(chunk);
            Ok(())
        };

        handle_sse_line(
            r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"ask_question","arguments":""}}]}}]}"#,
            &mut content,
            &mut content_emitted,
            &mut reasoning,
            &mut reasoning_emitted,
            &mut reasoning_part_active,
            &mut finish_reason,
            &mut usage,
            &mut tool_calls,
            &mut on_chunk,
        )
        .unwrap();

        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].kind, ChatStreamKind::ToolCall);
        assert_eq!(chunks[0].text, "ask_question");
    }

    #[test]
    fn chat_stream_surfaces_sse_error_objects() {
        let mut content = String::new();
        let mut content_emitted = 0usize;
        let mut reasoning = String::new();
        let mut reasoning_emitted = 0usize;
        let mut reasoning_part_active = false;
        let mut finish_reason = None;
        let mut usage = None;
        let mut tool_calls = ToolCallAccumulator::default();

        let error = handle_sse_line(
            r#"data: {"error":{"message":"upstream generation timed out"}}"#,
            &mut content,
            &mut content_emitted,
            &mut reasoning,
            &mut reasoning_emitted,
            &mut reasoning_part_active,
            &mut finish_reason,
            &mut usage,
            &mut tool_calls,
            &mut |_| Ok(()),
        )
        .unwrap_err();

        assert!(format!("{error:#}").contains("upstream generation timed out"));
    }

    #[test]
    fn reasoning_only_stream_result_is_preserved() {
        let result = finalize_stream_result(
            String::new(),
            "完整思考内容".to_string(),
            None,
            Vec::new(),
            false,
        )
        .unwrap();

        assert!(result.content.is_empty());
        assert_eq!(result.reasoning.as_deref(), Some("完整思考内容"));
    }

    #[test]
    fn fully_empty_stream_result_is_rejected() {
        let error = finalize_stream_result(String::new(), String::new(), None, Vec::new(), false)
            .unwrap_err();

        let message = format!("{error:#}");
        assert!(message.contains("流式响应为空") || message.contains("stream response was empty"));
    }

    #[test]
    fn sse_buffer_preserves_utf8_split_across_byte_chunks() {
        let line = r#"data: {"choices":[{"delta":{"content":"等","tool_calls":null}}]}"#;
        let split = line.find("等").unwrap() + 1;
        let mut buffer = Utf8LineBuffer::default();

        assert!(buffer.push(&line.as_bytes()[..split]).unwrap().is_empty());
        let lines = buffer.push(&line.as_bytes()[split..]).unwrap();

        assert!(lines.is_empty());
        assert_eq!(buffer.finish().unwrap(), vec![line]);
    }

    #[test]
    fn previous_lossy_chunk_decode_corrupts_split_utf8() {
        let text = "等";
        let mut decoded = String::new();

        decoded.push_str(&String::from_utf8_lossy(&text.as_bytes()[..1]));
        decoded.push_str(&String::from_utf8_lossy(&text.as_bytes()[1..]));

        assert_eq!(decoded, "���");
    }

    #[test]
    fn taotoken_glm_request_enables_thinking() {
        let mut provider = test_provider("taotoken", "https://taotoken.net/api/v1");
        provider.default_model = "glm_for_coding".to_string();

        assert!(taotoken_glm_chat_template_kwargs(&provider)
            .is_some_and(|kwargs| kwargs.enable_thinking));
    }

    #[test]
    fn non_taotoken_glm_request_keeps_default_body() {
        let mut provider = test_provider("local", "http://localhost:11434/v1");
        provider.default_model = "glm-5".to_string();

        assert!(taotoken_glm_chat_template_kwargs(&provider).is_none());
    }

    #[test]
    fn chat_request_includes_stream_usage_options() {
        let request = ChatRequest {
            model: "model".to_string(),
            messages: vec![ChatMessage::plain("user", "hi")],
            temperature: 0.0,
            stream: true,
            stream_options: Some(ChatStreamOptions {
                include_usage: true,
            }),
            max_tokens: None,
            tools: None,
            chat_template_kwargs: None,
            extra_body: None,
        };

        let value = serde_json::to_value(request).unwrap();

        assert_eq!(value["stream_options"]["include_usage"], true);
    }

    #[test]
    fn stream_options_unsupported_detects_retryable_error() {
        assert!(stream_options_unsupported(
            400,
            "unknown parameter: stream_options"
        ));
        assert!(stream_options_unsupported(
            422,
            "stream_options is not supported"
        ));
        assert!(!stream_options_unsupported(403, "stream_options forbidden"));
        assert!(!stream_options_unsupported(400, "invalid api key"));
    }

    #[test]
    fn quota_compatibility_retry_is_narrowly_scoped() {
        assert!(non_stream_quota_fallback_candidate(
            429,
            r#"{"error":{"code":"insufficient_quota"}}"#
        ));
        assert!(!non_stream_quota_fallback_candidate(
            429,
            r#"{"error":{"code":"rate_limit_exceeded"}}"#
        ));
        assert!(!non_stream_quota_fallback_candidate(
            400,
            r#"{"error":{"code":"insufficient_quota"}}"#
        ));
    }

    #[test]
    fn zen_upstream_failed_detects_opencode_zen_compat_error() {
        let provider = test_provider("myopencode", OPENCODE_ZEN_BASE_URL);

        assert!(zen_upstream_failed(
            &provider,
            400,
            r#"{"error":{"message":"Error from provider (Console): Upstream request failed"}}"#,
        ));
        assert!(!zen_upstream_failed(
            &provider,
            401,
            "Upstream request failed"
        ));
        assert!(!zen_upstream_failed(
            &test_provider("other", "https://example.com/v1"),
            400,
            "Upstream request failed"
        ));
    }

    #[test]
    fn openai_gpt5_uses_responses_api() {
        let mut provider = test_provider("openai", "https://api.openai.com/v1");
        provider.default_model = "gpt-5.5".to_string();
        let client = test_client(provider);

        assert!(client.uses_openai_responses());
    }

    #[test]
    fn openai_compatible_gpt5_tries_responses_api() {
        let mut provider = test_provider("taotoken", "https://taotoken.net/api/v1");
        provider.default_model = "gpt-5.5".to_string();
        let client = test_client(provider);

        assert!(client.uses_openai_responses());
    }

    #[test]
    fn auto_protocol_uses_anthropic_for_official_provider() {
        let provider = test_provider("anthropic", "https://api.anthropic.com/v1");
        let client = test_client(provider);

        assert!(client.uses_anthropic_messages());
    }

    #[test]
    fn auto_protocol_keeps_openai_compatible_claude_proxy() {
        let mut provider = test_provider("openrouter", "https://openrouter.ai/api/v1");
        provider.default_model = "anthropic/claude-sonnet-4-5".to_string();
        let client = test_client(provider);

        assert!(!client.uses_anthropic_messages());
    }

    #[test]
    fn responses_unsupported_allows_chat_fallback() {
        assert!(responses_unsupported(404, "not found"));
        assert!(responses_unsupported(400, "unsupported endpoint"));
        assert!(!responses_unsupported(401, "invalid api key"));
    }

    #[test]
    fn openai_tool_schema_flattens_top_level_any_of() {
        let schema = json!({
            "anyOf": [
                {"type":"object","properties":{"path":{"type":"string"}},"required":["path"]},
                {"type":"object","properties":{"resource":{"anyOf":[{"type":"string"},{"type":"null"}]}},"required":["resource"]}
            ]
        });

        let normalized = openai_tool_input_schema(schema);

        assert_eq!(normalized["type"], "object");
        assert_eq!(normalized["additionalProperties"], false);
        assert_eq!(normalized["properties"]["path"]["type"], "string");
        assert_eq!(normalized["properties"]["resource"]["type"], "string");
        assert!(normalized.get("anyOf").is_none());
    }

    #[test]
    fn responses_assistant_history_uses_easy_input_message() {
        let input = lower_responses_messages(vec![ChatMessage::assistant("prior answer", None)]);

        assert_eq!(
            input,
            vec![json!({"role": "assistant", "content": "prior answer"})]
        );
    }

    #[test]
    fn responses_stream_emits_reasoning_and_content() {
        let mut content = String::new();
        let mut content_emitted = 0usize;
        let mut reasoning = String::new();
        let mut reasoning_emitted = 0usize;
        let mut reasoning_part_active = false;
        let mut usage = None;
        let mut content_started = false;
        let mut output_text_delta_parts = HashSet::new();
        let mut refusal_delta_parts = HashSet::new();
        let mut response_id = None;
        let mut tool_calls = ResponsesToolAccumulator::default();
        let mut chunks = Vec::new();
        let mut on_chunk = |chunk| {
            chunks.push(chunk);
            Ok(())
        };

        handle_responses_sse_line(
            r#"data: {"type":"response.reasoning_summary_text.delta","item_id":"rs_1","delta":"思考"}"#,
            &mut content,
            &mut content_emitted,
            &mut reasoning,
            &mut reasoning_emitted,
            &mut reasoning_part_active,
            &mut usage,
            &mut content_started,
            &mut output_text_delta_parts,
            &mut refusal_delta_parts,
            &mut response_id,
            &mut tool_calls,
            &mut on_chunk,
        )
        .unwrap();
        handle_responses_sse_line(
            r#"data: {"type":"response.output_text.delta","item_id":"msg_1","delta":""}"#,
            &mut content,
            &mut content_emitted,
            &mut reasoning,
            &mut reasoning_emitted,
            &mut reasoning_part_active,
            &mut usage,
            &mut content_started,
            &mut output_text_delta_parts,
            &mut refusal_delta_parts,
            &mut response_id,
            &mut tool_calls,
            &mut on_chunk,
        )
        .unwrap();
        handle_responses_sse_line(
            r#"data: {"type":"response.output_text.delta","item_id":"msg_1","delta":"答案"}"#,
            &mut content,
            &mut content_emitted,
            &mut reasoning,
            &mut reasoning_emitted,
            &mut reasoning_part_active,
            &mut usage,
            &mut content_started,
            &mut output_text_delta_parts,
            &mut refusal_delta_parts,
            &mut response_id,
            &mut tool_calls,
            &mut on_chunk,
        )
        .unwrap();

        assert_eq!(chunks.len(), 4);
        assert_eq!(chunks[0].kind, ChatStreamKind::ReasoningPartStart);
        assert_eq!(chunks[1].kind, ChatStreamKind::Reasoning);
        assert_eq!(chunks[1].text, "思考");
        assert_eq!(chunks[2].kind, ChatStreamKind::ReasoningPartEnd);
        assert_eq!(chunks[3].kind, ChatStreamKind::Content);
        assert_eq!(chunks[3].text, "答案");
    }

    #[test]
    fn responses_reasoning_done_emits_content_boundary() {
        let mut content = String::new();
        let mut content_emitted = 0usize;
        let mut reasoning = String::new();
        let mut reasoning_emitted = 0usize;
        let mut reasoning_part_active = false;
        let mut usage = None;
        let mut content_started = false;
        let mut output_text_delta_parts = HashSet::new();
        let mut refusal_delta_parts = HashSet::new();
        let mut response_id = None;
        let mut tool_calls = ResponsesToolAccumulator::default();
        let mut chunks = Vec::new();
        let mut on_chunk = |chunk| {
            chunks.push(chunk);
            Ok(())
        };

        for line in [
            r#"data: {"type":"response.reasoning_summary_text.delta","item_id":"rs_1","delta":"思考"}"#,
            r#"data: {"type":"response.reasoning_summary_text.done","item_id":"rs_1"}"#,
            r#"data: {"type":"response.output_text.delta","item_id":"msg_1","delta":"答案"}"#,
            r#"data: {"type":"response.reasoning_summary_text.delta","item_id":"rs_1","delta":"晚到"}"#,
        ] {
            handle_responses_sse_line(
                line,
                &mut content,
                &mut content_emitted,
                &mut reasoning,
                &mut reasoning_emitted,
                &mut reasoning_part_active,
                &mut usage,
                &mut content_started,
                &mut output_text_delta_parts,
                &mut refusal_delta_parts,
                &mut response_id,
                &mut tool_calls,
                &mut on_chunk,
            )
            .unwrap();
        }

        assert_eq!(chunks.len(), 7);
        assert_eq!(chunks[0].kind, ChatStreamKind::ReasoningPartStart);
        assert_eq!(chunks[1].kind, ChatStreamKind::Reasoning);
        assert_eq!(chunks[1].text, "思考");
        assert_eq!(chunks[2].kind, ChatStreamKind::ReasoningPartEnd);
        assert_eq!(chunks[3].kind, ChatStreamKind::Content);
        assert!(chunks[3].text.is_empty());
        assert_eq!(chunks[4].kind, ChatStreamKind::Content);
        assert_eq!(chunks[4].text, "答案");
        assert_eq!(chunks[5].kind, ChatStreamKind::ReasoningPartStart);
        assert_eq!(chunks[6].kind, ChatStreamKind::Reasoning);
        assert_eq!(chunks[6].text, "\n\n晚到");
        assert_eq!(reasoning, "思考\n\n晚到");
    }

    #[test]
    fn responses_stream_preserves_multiple_reasoning_summary_parts() {
        let mut content = String::new();
        let mut content_emitted = 0usize;
        let mut reasoning = String::new();
        let mut reasoning_emitted = 0usize;
        let mut reasoning_part_active = false;
        let mut usage = None;
        let mut content_started = false;
        let mut output_text_delta_parts = HashSet::new();
        let mut refusal_delta_parts = HashSet::new();
        let mut response_id = None;
        let mut tool_calls = ResponsesToolAccumulator::default();
        let mut chunks = Vec::new();
        let mut on_chunk = |chunk| {
            chunks.push(chunk);
            Ok(())
        };

        for line in [
            r#"data: {"type":"response.reasoning_summary_part.added","item_id":"rs_1","summary_index":0}"#,
            r#"data: {"type":"response.reasoning_summary_text.delta","item_id":"rs_1","summary_index":0,"delta":"**Planning response**"}"#,
            r#"data: {"type":"response.reasoning_summary_part.done","item_id":"rs_1","summary_index":0}"#,
            r#"data: {"type":"response.reasoning_summary_part.added","item_id":"rs_1","summary_index":1}"#,
            r#"data: {"type":"response.reasoning_summary_text.delta","item_id":"rs_1","summary_index":1,"delta":"**Designing helper**"}"#,
            r#"data: {"type":"response.reasoning_summary_part.done","item_id":"rs_1","summary_index":1}"#,
        ] {
            handle_responses_sse_line(
                line,
                &mut content,
                &mut content_emitted,
                &mut reasoning,
                &mut reasoning_emitted,
                &mut reasoning_part_active,
                &mut usage,
                &mut content_started,
                &mut output_text_delta_parts,
                &mut refusal_delta_parts,
                &mut response_id,
                &mut tool_calls,
                &mut on_chunk,
            )
            .unwrap();
        }

        let kinds = chunks.iter().map(|chunk| chunk.kind).collect::<Vec<_>>();
        assert_eq!(
            kinds,
            vec![
                ChatStreamKind::ReasoningPartStart,
                ChatStreamKind::Reasoning,
                ChatStreamKind::ReasoningPartEnd,
                ChatStreamKind::ReasoningPartStart,
                ChatStreamKind::Reasoning,
                ChatStreamKind::ReasoningPartEnd,
            ]
        );
        assert_eq!(reasoning, "**Planning response**\n\n**Designing helper**");
    }

    #[test]
    fn stream_filter_skips_split_system_reminder() {
        let mut content = String::new();
        let mut emitted = 0usize;
        let mut chunks = Vec::new();
        let mut on_chunk = |chunk| {
            chunks.push(chunk);
            Ok(())
        };

        push_buffered_chunk(
            &mut content,
            &mut emitted,
            ChatStreamKind::Content,
            "hello <system-rem".to_string(),
            &mut on_chunk,
        )
        .unwrap();
        push_buffered_chunk(
            &mut content,
            &mut emitted,
            ChatStreamKind::Content,
            "inder>hidden</system-reminder> world".to_string(),
            &mut on_chunk,
        )
        .unwrap();

        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].text, "hello ");
        assert_eq!(chunks[1].text, " world");
    }

    #[test]
    fn stream_filter_skips_underscore_system_reminder() {
        let mut content = String::new();
        let mut emitted = 0usize;
        let mut chunks = Vec::new();
        let mut on_chunk = |chunk| {
            chunks.push(chunk);
            Ok(())
        };

        push_buffered_chunk(
            &mut content,
            &mut emitted,
            ChatStreamKind::Content,
            "a<system_reminder>hidden</system_reminder>b".to_string(),
            &mut on_chunk,
        )
        .unwrap();

        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].text, "a");
        assert_eq!(chunks[1].text, "b");
    }

    #[test]
    fn responses_stream_collects_tool_calls() {
        let mut content = String::new();
        let mut content_emitted = 0usize;
        let mut reasoning = String::new();
        let mut reasoning_emitted = 0usize;
        let mut reasoning_part_active = false;
        let mut usage = None;
        let mut content_started = false;
        let mut output_text_delta_parts = HashSet::new();
        let mut refusal_delta_parts = HashSet::new();
        let mut response_id = None;
        let mut tool_calls = ResponsesToolAccumulator::default();
        let mut on_chunk = |_| Ok(());

        for line in [
            r#"data: {"type":"response.output_item.added","item":{"type":"function_call","id":"item_1","call_id":"call_1","name":"calc","arguments":""}}"#,
            r#"data: {"type":"response.function_call_arguments.delta","item_id":"item_1","delta":"{\"x\":"}"#,
            r#"data: {"type":"response.function_call_arguments.delta","item_id":"item_1","delta":"1}"}"#,
            r#"data: {"type":"response.output_item.done","item":{"type":"function_call","id":"item_1","call_id":"call_1","name":"calc","arguments":"{\"x\":1}"}}"#,
        ] {
            handle_responses_sse_line(
                line,
                &mut content,
                &mut content_emitted,
                &mut reasoning,
                &mut reasoning_emitted,
                &mut reasoning_part_active,
                &mut usage,
                &mut content_started,
                &mut output_text_delta_parts,
                &mut refusal_delta_parts,
                &mut response_id,
                &mut tool_calls,
                &mut on_chunk,
            )
            .unwrap();
        }

        let calls = tool_calls.finish();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].id, "call_1");
        assert_eq!(calls[0].function.name, "calc");
        assert_eq!(calls[0].function.arguments, r#"{"x":1}"#);
    }

    #[test]
    fn responses_stream_announces_question_tool_when_item_starts() {
        let mut content = String::new();
        let mut content_emitted = 0usize;
        let mut reasoning = String::new();
        let mut reasoning_emitted = 0usize;
        let mut reasoning_part_active = false;
        let mut usage = None;
        let mut content_started = false;
        let mut output_text_delta_parts = HashSet::new();
        let mut refusal_delta_parts = HashSet::new();
        let mut response_id = None;
        let mut tool_calls = ResponsesToolAccumulator::default();
        let mut chunks = Vec::new();
        let mut on_chunk = |chunk| {
            chunks.push(chunk);
            Ok(())
        };

        handle_responses_sse_line(
            r#"data: {"type":"response.output_item.added","item":{"type":"function_call","id":"item_1","call_id":"call_1","name":"ask_question","arguments":""}}"#,
            &mut content,
            &mut content_emitted,
            &mut reasoning,
            &mut reasoning_emitted,
            &mut reasoning_part_active,
            &mut usage,
            &mut content_started,
            &mut output_text_delta_parts,
            &mut refusal_delta_parts,
            &mut response_id,
            &mut tool_calls,
            &mut on_chunk,
        )
        .unwrap();

        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].kind, ChatStreamKind::ToolCall);
        assert_eq!(chunks[0].text, "ask_question");
    }

    #[test]
    fn responses_tool_arguments_follow_output_item_ids() {
        let mut tool_calls = ResponsesToolAccumulator::default();
        for (item_id, call_id, name) in [
            ("item_a", "call_a", "first"),
            ("item_b", "call_b", "second"),
        ] {
            tool_calls.start(ResponsesStreamItem {
                kind: "function_call".to_string(),
                id: Some(item_id.to_string()),
                call_id: Some(call_id.to_string()),
                name: Some(name.to_string()),
                arguments: Some(String::new()),
            });
        }

        tool_calls.append_arguments(Some("item_a".to_string()), "{\"a\":".to_string());
        tool_calls.append_arguments(Some("item_b".to_string()), "{\"b\":2}".to_string());
        tool_calls.append_arguments(Some("item_a".to_string()), "1}".to_string());
        tool_calls.append_arguments(Some("unknown".to_string()), "ignored".to_string());

        let calls = tool_calls.finish();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].id, "call_a");
        assert_eq!(calls[0].function.arguments, r#"{"a":1}"#);
        assert_eq!(calls[1].id, "call_b");
        assert_eq!(calls[1].function.arguments, r#"{"b":2}"#);
    }

    #[test]
    fn responses_stream_surfaces_refusal_text() {
        let output = run_responses_test_events(&[
            r#"data: {"type":"response.created","response":{"id":"resp_refusal"}}"#,
            r#"data: {"type":"response.refusal.delta","item_id":"msg_1","delta":"Cannot "}"#,
            r#"data: {"type":"response.refusal.delta","item_id":"msg_1","delta":"help"}"#,
            r#"data: {"type":"response.refusal.done","item_id":"msg_1","refusal":"Cannot help"}"#,
            r#"data: {"type":"response.completed","response":{"id":"resp_refusal"}}"#,
        ])
        .unwrap();

        assert!(output.terminal);
        assert_eq!(output.content, "Cannot help");
        assert_eq!(output.response_id.as_deref(), Some("resp_refusal"));
        assert_eq!(
            output
                .chunks
                .iter()
                .filter(|chunk| chunk.kind == ChatStreamKind::Content)
                .map(|chunk| chunk.text.as_str())
                .collect::<String>(),
            "Cannot help"
        );
    }

    #[test]
    fn responses_stream_accepts_done_only_refusal() {
        let output = run_responses_test_events(&[
            r#"data: {"type":"response.refusal.done","item_id":"msg_1","refusal":"Cannot help"}"#,
            r#"data: {"type":"response.completed","response":{"id":"resp_refusal"}}"#,
        ])
        .unwrap();

        assert_eq!(output.content, "Cannot help");
    }

    #[test]
    fn responses_stream_accepts_done_only_output_text() {
        let output = run_responses_test_events(&[
            r#"data: {"type":"response.output_text.delta","item_id":"msg_1","delta":""}"#,
            r#"data: {"type":"response.output_text.done","item_id":"msg_1","text":"final text"}"#,
            r#"data: {"type":"response.output_text.done","item_id":"msg_2","text":" second"}"#,
            r#"data: {"type":"response.completed","response":{"id":"resp_text"}}"#,
        ])
        .unwrap();

        assert_eq!(output.content, "final text second");
    }

    #[test]
    fn responses_incomplete_is_not_a_successful_terminal_event() {
        let error = run_responses_test_events(&[r#"data: {"type":"response.incomplete","response":{"id":"resp_incomplete","incomplete_details":{"reason":"max_output_tokens"}}}"#])
            .unwrap_err();

        assert!(error.to_string().contains("max_output_tokens"), "{error:#}");
    }

    #[test]
    fn responses_tool_calls_require_stateful_continuation() {
        let tool_call = ToolCall {
            id: "call_1".to_string(),
            kind: "function".to_string(),
            function: ToolCallFunction {
                name: "calc".to_string(),
                arguments: "{}".to_string(),
            },
        };

        let store_error = finalize_responses_stream_result(
            String::new(),
            String::new(),
            None,
            vec![tool_call.clone()],
            false,
            Some("resp_1".to_string()),
            true,
        )
        .unwrap_err();
        assert!(store_error.to_string().contains("store=false"));

        let id_error = finalize_responses_stream_result(
            String::new(),
            String::new(),
            None,
            vec![tool_call],
            false,
            None,
            false,
        )
        .unwrap_err();
        assert!(id_error.to_string().contains("without a response ID"));
    }

    #[tokio::test]
    async fn responses_store_false_rejects_tools_before_sending() {
        let mut provider = test_provider("responses-store-test", "http://127.0.0.1:1/v1");
        provider.protocol = "openai-responses".to_string();
        provider.default_model = "gpt-5".to_string();
        provider.extra_body = json!({"store": false}).as_object().cloned();
        let client = test_client(provider);
        let tools = vec![ToolDefinition {
            kind: "function",
            function: crate::llm::FunctionDefinition {
                name: "calc".to_string(),
                description: "calculate".to_string(),
                parameters: json!({"type": "object", "properties": {}}),
            },
        }];

        let error = client
            .chat_responses_stream(
                vec![ChatMessage::plain("user", "hi")],
                tools,
                None,
                "request-test",
                &mut |_| Ok(()),
            )
            .await
            .unwrap_err();

        assert!(error.to_string().contains("remove store=false"));
    }

    #[test]
    fn protocol_config_accepts_explicit_anthropic() {
        let mut provider = test_provider("anthropic", "https://api.anthropic.com/v1");
        provider.protocol = "anthropic".to_string();

        assert_eq!(
            ProviderProtocol::from_provider(&provider).unwrap(),
            ProviderProtocol::Anthropic
        );
    }

    #[test]
    fn protocol_config_accepts_anthropic_aliases() {
        let mut provider = test_provider("anthropic", "https://api.anthropic.com/v1");

        for protocol in ["anthropic-messages", "claude", "claude-messages"] {
            provider.protocol = protocol.to_string();
            assert_eq!(
                ProviderProtocol::from_provider(&provider).unwrap(),
                ProviderProtocol::Anthropic
            );
        }
    }

    #[test]
    fn anthropic_lowering_keeps_remote_image_urls() {
        let content = lower_anthropic_user_content(Some(ChatContent::Parts(vec![
            ChatContentPart::ImageUrl {
                image_url: ImageUrlContent {
                    url: "https://example.com/image.png".to_string(),
                },
            },
            ChatContentPart::Text {
                text: "describe".to_string(),
            },
        ])));
        let json = serde_json::to_value(content).unwrap();

        assert_eq!(json[0]["type"], "image");
        assert_eq!(json[0]["source"]["type"], "url");
        assert_eq!(json[0]["source"]["url"], "https://example.com/image.png");
        assert_eq!(json[1]["text"], "describe");
    }

    #[test]
    fn anthropic_stream_waits_for_message_stop() {
        let mut state = AnthropicStreamState::default();
        let mut on_chunk = |_| Ok(());

        let done = handle_anthropic_sse_data(
            r#"{"type":"message_delta","usage":{"input_tokens":3,"output_tokens":2},"delta":{"stop_reason":"end_turn"}}"#,
            &mut state,
            &mut on_chunk,
        )
        .unwrap();
        assert!(!done);

        let done =
            handle_anthropic_sse_data(r#"{"type":"message_stop"}"#, &mut state, &mut on_chunk)
                .unwrap();
        assert!(done);
    }

    #[test]
    fn official_anthropic_template_sets_messages_protocol() {
        let provider = ProviderConfig::default_anthropic();

        assert_eq!(provider.id, "anthropic");
        assert_eq!(provider.protocol, "anthropic");
        assert_eq!(provider.base_url, "https://api.anthropic.com/v1");
        assert_eq!(provider.api_key.as_deref(), Some("$env:ANTHROPIC_API_KEY"));
        assert!(provider.models.is_empty());
        assert!(provider.default_model.is_empty());
    }

    #[test]
    fn anthropic_request_enables_adaptive_summarized_thinking_by_default() {
        let mut provider = test_provider("anthropic", "https://api.anthropic.com/v1");
        provider.default_model = "claude-sonnet-4-5".to_string();
        let client = test_client(provider);

        let request =
            client.anthropic_request(vec![ChatMessage::plain("user", "hi")], Vec::new(), true);
        let json = serde_json::to_value(request).unwrap();

        assert_eq!(json["thinking"]["type"], "adaptive");
        assert_eq!(json["thinking"]["display"], "summarized");
    }

    #[test]
    fn anthropic_request_can_disable_thinking_for_fallback() {
        let mut provider = test_provider("anthropic", "https://api.anthropic.com/v1");
        provider.default_model = "claude-sonnet-4-5".to_string();
        let client = test_client(provider);

        let request =
            client.anthropic_request(vec![ChatMessage::plain("user", "hi")], Vec::new(), false);
        let json = serde_json::to_value(request).unwrap();

        assert!(json.get("thinking").is_none());
    }

    #[test]
    fn anthropic_thinking_unsupported_detects_retryable_errors() {
        assert!(anthropic_thinking_unsupported(
            400,
            "invalid request: thinking is not supported by this model"
        ));
        assert!(anthropic_thinking_unsupported(
            422,
            "unknown parameter: thinking"
        ));
        assert!(!anthropic_thinking_unsupported(401, "invalid api key"));
        assert!(!anthropic_thinking_unsupported(
            400,
            "max_tokens is too low"
        ));
    }

    #[test]
    fn anthropic_stream_emits_reasoning_content_and_usage() {
        let mut state = AnthropicStreamState::default();
        let mut chunks = Vec::new();
        let mut on_chunk = |chunk| {
            chunks.push(chunk);
            Ok(())
        };

        for data in [
            r#"{"type":"message_start","message":{"usage":{"input_tokens":3,"output_tokens":0}}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"想"}}"#,
            r#"{"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"答"}}"#,
            r#"{"type":"message_delta","usage":{"output_tokens":2},"delta":{"stop_reason":"end_turn"}}"#,
            r#"{"type":"message_stop"}"#,
        ] {
            let done = handle_anthropic_sse_data(data, &mut state, &mut on_chunk).unwrap();
            if data.contains("message_stop") {
                assert!(done);
            }
        }

        assert_eq!(chunks.len(), 4);
        assert_eq!(chunks[0].kind, ChatStreamKind::ReasoningPartStart);
        assert_eq!(chunks[1].kind, ChatStreamKind::Reasoning);
        assert_eq!(chunks[1].text, "想");
        assert_eq!(chunks[2].kind, ChatStreamKind::ReasoningPartEnd);
        assert_eq!(chunks[3].kind, ChatStreamKind::Content);
        assert_eq!(chunks[3].text, "答");
        let usage = state.usage.unwrap();
        assert_eq!(usage.prompt_tokens, 3);
        assert_eq!(usage.completion_tokens, 2);
        assert_eq!(usage.total_tokens, 5);
    }

    #[test]
    fn anthropic_stream_accepts_thinking_signature_delta() {
        let mut state = AnthropicStreamState::default();
        let mut on_chunk = |_| Ok(());

        handle_anthropic_sse_data(
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"sig_123"}}"#,
            &mut state,
            &mut on_chunk,
        )
        .unwrap();

        assert_eq!(state.thinking_signature.as_deref(), Some("sig_123"));
        assert!(state.reasoning.is_empty());
    }

    #[test]
    fn anthropic_stream_separates_multiple_thinking_blocks() {
        let mut state = AnthropicStreamState::default();
        let mut chunks = Vec::new();
        let mut on_chunk = |chunk| {
            chunks.push(chunk);
            Ok(())
        };

        for data in [
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":"Planning"}}"#,
            r#"{"type":"content_block_stop","index":0}"#,
            r#"{"type":"content_block_start","index":1,"content_block":{"type":"thinking","thinking":"Designing"}}"#,
            r#"{"type":"content_block_stop","index":1}"#,
        ] {
            handle_anthropic_sse_data(data, &mut state, &mut on_chunk).unwrap();
        }

        assert_eq!(state.reasoning, "Planning\n\nDesigning");
        assert_eq!(
            chunks.iter().map(|chunk| chunk.kind).collect::<Vec<_>>(),
            vec![
                ChatStreamKind::ReasoningPartStart,
                ChatStreamKind::Reasoning,
                ChatStreamKind::ReasoningPartEnd,
                ChatStreamKind::ReasoningPartStart,
                ChatStreamKind::Reasoning,
                ChatStreamKind::ReasoningPartEnd,
            ]
        );
    }

    #[test]
    fn anthropic_stream_collects_tool_calls() {
        let mut state = AnthropicStreamState::default();
        let mut on_chunk = |_| Ok(());

        for data in [
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_1","name":"calc","input":{}}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"x\":"}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"1}"}}"#,
        ] {
            handle_anthropic_sse_data(data, &mut state, &mut on_chunk).unwrap();
        }

        let calls = state.tool_calls.finish();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].id, "toolu_1");
        assert_eq!(calls[0].function.name, "calc");
        assert_eq!(calls[0].function.arguments, r#"{"x":1}"#);
    }

    #[test]
    fn anthropic_stream_announces_question_tool_when_block_starts() {
        let mut state = AnthropicStreamState::default();
        let mut chunks = Vec::new();
        let mut on_chunk = |chunk| {
            chunks.push(chunk);
            Ok(())
        };

        handle_anthropic_sse_data(
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_1","name":"ask_question","input":{}}}"#,
            &mut state,
            &mut on_chunk,
        )
        .unwrap();

        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].kind, ChatStreamKind::ToolCall);
        assert_eq!(chunks[0].text, "ask_question");
    }

    #[tokio::test]
    async fn transport_connect_failure_is_retried_once() {
        let unavailable = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let unavailable_addr = unavailable.local_addr().unwrap();
        drop(unavailable);

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let available_url = format!("http://{}/ok", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            read_http_headers(&mut stream).await;
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok")
                .await
                .unwrap();
        });

        let client = test_client(test_provider("test", "http://example.invalid/v1"));
        let unavailable_url = format!("http://{unavailable_addr}/unavailable");
        let mut builds = 0;
        let response = client
            .send_with_transport_retry("request-test", "chat.send", || {
                builds += 1;
                client.client.get(if builds == 1 {
                    &unavailable_url
                } else {
                    &available_url
                })
            })
            .await
            .unwrap();

        assert_eq!(builds, 2);
        assert_eq!(response.text().await.unwrap(), "ok");
        server.await.unwrap();
    }

    #[tokio::test]
    async fn transient_http_server_errors_are_retried() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/retry", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            for status in [500, 503, 200] {
                let (mut stream, _) = listener.accept().await.unwrap();
                read_http_headers(&mut stream).await;
                let reason = if status == 200 {
                    "OK"
                } else {
                    "Internal Server Error"
                };
                let body = if status == 200 { "ok" } else { "error" };
                let response = format!(
                    "HTTP/1.1 {status} {reason}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                stream.write_all(response.as_bytes()).await.unwrap();
            }
        });

        let client = test_client(test_provider("test", "http://example.invalid/v1"));
        let mut builds = 0;
        let response = client
            .send_with_transport_retry("request-test", "chat.send", || {
                builds += 1;
                client.client.get(&url)
            })
            .await
            .unwrap();

        assert_eq!(builds, 3);
        assert_eq!(response.status().as_u16(), 200);
        assert_eq!(response.text().await.unwrap(), "ok");
        server.await.unwrap();
    }

    #[tokio::test]
    async fn persistent_http_server_errors_stop_after_three_attempts() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/retry", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            for _ in 0..MAX_SEND_ATTEMPTS {
                let (mut stream, _) = listener.accept().await.unwrap();
                read_http_headers(&mut stream).await;
                stream
                    .write_all(
                        b"HTTP/1.1 500 Internal Server Error\r\nContent-Length: 5\r\nConnection: close\r\n\r\nerror",
                    )
                    .await
                    .unwrap();
            }
        });

        let client = test_client(test_provider("test", "http://example.invalid/v1"));
        let mut builds = 0;
        let error = tokio::time::timeout(
            Duration::from_secs(1),
            client.send_with_transport_retry("request-test", "chat.send", || {
                builds += 1;
                client.client.get(&url)
            }),
        )
        .await
        .expect("persistent 5xx retries did not stop")
        .unwrap_err();

        assert_eq!(builds, MAX_SEND_ATTEMPTS);
        let failure = error.downcast_ref::<HttpStatusFailure>().unwrap();
        assert_eq!(failure.status, 500);
        server.await.unwrap();
    }

    #[tokio::test]
    async fn response_header_timeout_stops_a_stalled_endpoint() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/v1", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            read_http_headers(&mut stream).await;
            tokio::time::sleep(Duration::from_millis(100)).await;
        });

        let mut provider = test_provider("header-timeout-test", &url);
        provider.protocol = "openai-chat".to_string();
        let client = test_client(provider)
            .with_request_timeouts(Duration::from_millis(20), Duration::from_secs(1));
        let error = client
            .chat_stream(vec![ChatMessage::plain("user", "hi")], Vec::new(), |_| {
                Ok(())
            })
            .await
            .unwrap_err();

        let message = format!("{error:#}");
        assert!(message.contains("response header timed out"), "{message}");
        server.await.unwrap();
    }

    #[tokio::test]
    async fn response_header_timeout_fails_over_to_the_next_endpoint() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/v1", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (mut stalled, _) = listener.accept().await.unwrap();
            read_http_headers(&mut stalled).await;
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(100)).await;
                drop(stalled);
            });
            let (mut healthy, _) = listener.accept().await.unwrap();
            read_http_headers(&mut healthy).await;
            write_http_sse_response(
                &mut healthy,
                concat!(
                    "data: {\"choices\":[{\"delta\":{\"content\":\"fallback\"}}]}\n\n",
                    "data: [DONE]\n\n"
                ),
            )
            .await;
        });

        let mut first = test_provider("header-timeout-first", &url);
        first.protocol = "openai-chat".to_string();
        let mut second = test_provider("header-timeout-second", &url);
        second.protocol = "openai-chat".to_string();
        let http_client = reqwest::Client::new();
        let endpoints = vec![
            LlmEndpoint {
                client: http_client.clone(),
                provider: first.clone(),
                api_key: "first".to_string(),
                key_index: 0,
            },
            LlmEndpoint {
                client: http_client.clone(),
                provider: second,
                api_key: "second".to_string(),
                key_index: 0,
            },
        ];
        let client = OpenAiCompatibleClient {
            client: http_client,
            provider: first,
            api_key: "first".to_string(),
            endpoints: Arc::new(endpoints),
            thinking_variants: HashMap::new(),
            reasoning_visibility: ReasoningVisibility::Hidden,
            buffered_delivery: false,
            detailed_reasoning_summary: false,
            request_timeouts: Some(RequestTimeouts {
                response_header: Duration::from_millis(20),
                stream_idle: Duration::from_secs(1),
            }),
            max_tokens_override: None,
            request_scope: "chat",
        };

        let result = client
            .chat_buffered(vec![ChatMessage::plain("user", "hi")], Vec::new())
            .await
            .unwrap();
        assert_eq!(result.content, "fallback");
        server.await.unwrap();
    }

    #[tokio::test]
    async fn stream_idle_timeout_stops_a_stalled_endpoint() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/v1", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            read_http_headers(&mut stream).await;
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n",
                )
                .await
                .unwrap();
            tokio::time::sleep(Duration::from_millis(100)).await;
        });

        let mut provider = test_provider("stream-idle-test", &url);
        provider.protocol = "openai-chat".to_string();
        let client = test_client(provider)
            .with_request_timeouts(Duration::from_secs(1), Duration::from_millis(20));
        let error = client
            .chat_stream(vec![ChatMessage::plain("user", "hi")], Vec::new(), |_| {
                Ok(())
            })
            .await
            .unwrap_err();

        let message = format!("{error:#}");
        assert!(message.contains("response stream was idle"), "{message}");
        server.await.unwrap();
    }

    /// Writes an SSE body and then hangs up without `[DONE]`, the way a proxy
    /// that drops the connection mid-generation does.
    async fn write_truncated_sse_response(stream: &mut tokio::net::TcpStream, body: &str) {
        // No Content-Length and no terminating chunk: the peer sees the socket
        // close, which is exactly the "graceful close mid-stream" case.
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n{body}"
        );
        stream.write_all(response.as_bytes()).await.unwrap();
        stream.flush().await.unwrap();
        stream.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn a_stream_that_stops_before_any_end_signal_is_not_a_completion() {
        // The failure this reproduces: the model was still emitting reasoning
        // when the connection went away, so there is no content, no tool call,
        // no `[DONE]` and no finish_reason. Accepting that as a finished turn
        // is what made a QQ reply vanish silently.
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/v1", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            for _ in 0..MAX_SEND_ATTEMPTS {
                let Ok((mut stream, _)) = listener.accept().await else {
                    return;
                };
                read_http_headers(&mut stream).await;
                write_truncated_sse_response(
                    &mut stream,
                    "data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"在想第一步\"}}]}\n\n",
                )
                .await;
            }
        });

        let mut provider = test_provider("truncated-stream-test", &url);
        provider.protocol = "openai-chat".to_string();
        provider.default_model = "test-model".to_string();
        let client = test_client(provider);

        let outcome = client
            .chat_stream(vec![ChatMessage::plain("user", "hi")], Vec::new(), |_| {
                Ok(())
            })
            .await;

        let error = outcome.expect_err("a truncated stream must not read as a finished turn");
        let message = format!("{error:#}");
        assert!(
            message.contains("ended before") || message.contains("提前结束"),
            "the error should name the truncation: {message}"
        );
        server.abort();
    }

    #[tokio::test]
    async fn an_empty_error_field_does_not_fail_the_turn() {
        // Some gateways send `{"error":""}` next to the terminal usage event.
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/v1", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            read_http_headers(&mut stream).await;
            write_http_sse_response(
                &mut stream,
                concat!(
                    "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n",
                    "data: {\"error\":\"\",\"choices\":[{\"finish_reason\":\"stop\",\"delta\":{}}]}\n\n",
                    "data: [DONE]\n\n"
                ),
            )
            .await;
        });

        let mut provider = test_provider("empty-error-test", &url);
        provider.protocol = "openai-chat".to_string();
        provider.default_model = "test-model".to_string();
        let client = test_client(provider);

        let result = client
            .chat_stream(vec![ChatMessage::plain("user", "hi")], Vec::new(), |_| {
                Ok(())
            })
            .await
            .expect("an empty error field is not an error");
        assert_eq!(result.content, "hi");
        server.await.unwrap();
    }

    #[tokio::test]
    async fn a_real_error_field_still_fails_the_turn() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/v1", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            read_http_headers(&mut stream).await;
            write_http_sse_response(
                &mut stream,
                concat!(
                    "data: {\"error\":{\"message\":\"上游炸了\"},\"choices\":[]}\n\n",
                    "data: [DONE]\n\n"
                ),
            )
            .await;
        });

        let mut provider = test_provider("real-error-test", &url);
        provider.protocol = "openai-chat".to_string();
        provider.default_model = "test-model".to_string();
        let client = test_client(provider);

        let error = client
            .chat_stream(vec![ChatMessage::plain("user", "hi")], Vec::new(), |_| {
                Ok(())
            })
            .await
            .expect_err("an in-band error must not be dressed up as a completion");
        assert!(format!("{error:#}").contains("上游炸了"));
        server.abort();
    }

    #[tokio::test]
    async fn a_lone_endpoint_still_gets_retried() {
        // Attempts used to equal endpoints, so the person with a single model
        // — the one with nowhere else to go — got no retry at all.
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/v1", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            // First connection dies mid-stream; the second answers properly.
            let (mut stream, _) = listener.accept().await.unwrap();
            read_http_headers(&mut stream).await;
            write_truncated_sse_response(
                &mut stream,
                "data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"想了一半\"}}]}\n\n",
            )
            .await;

            let (mut stream, _) = listener.accept().await.unwrap();
            read_http_headers(&mut stream).await;
            write_http_sse_response(
                &mut stream,
                concat!(
                    "data: {\"choices\":[{\"delta\":{\"content\":\"第二次成功\"}}]}\n\n",
                    "data: [DONE]\n\n"
                ),
            )
            .await;
        });

        let mut provider = test_provider("lone-endpoint-test", &url);
        provider.protocol = "openai-chat".to_string();
        provider.default_model = "test-model".to_string();
        let client = test_client(provider);
        assert_eq!(client.endpoints.len(), 1, "the point is a single endpoint");

        let result = client
            .chat_stream(vec![ChatMessage::plain("user", "hi")], Vec::new(), |_| {
                Ok(())
            })
            .await
            .expect("a single endpoint should still be retried");
        assert_eq!(result.content, "第二次成功");
        server.await.unwrap();
    }

    #[tokio::test]
    async fn buffered_delivery_lets_a_committed_attempt_be_retried() {
        // A platform turn collects a whole round before posting it, so content
        // streamed before the drop reached nobody and retrying is invisible.
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/v1", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            read_http_headers(&mut stream).await;
            // Content, not just reasoning: this is what used to pin the turn
            // to the failed attempt.
            write_truncated_sse_response(
                &mut stream,
                "data: {\"choices\":[{\"delta\":{\"content\":\"半句\"}}]}\n\n",
            )
            .await;

            let (mut stream, _) = listener.accept().await.unwrap();
            read_http_headers(&mut stream).await;
            write_http_sse_response(
                &mut stream,
                concat!(
                    "data: {\"choices\":[{\"delta\":{\"content\":\"完整回复\"}}]}\n\n",
                    "data: [DONE]\n\n"
                ),
            )
            .await;
        });

        let mut provider = test_provider("buffered-delivery-test", &url);
        provider.protocol = "openai-chat".to_string();
        provider.default_model = "test-model".to_string();
        let client = test_client(provider).with_buffered_delivery(true);

        let result = client
            .chat_stream(vec![ChatMessage::plain("user", "hi")], Vec::new(), |_| {
                Ok(())
            })
            .await
            .expect("buffered delivery means the false start was never seen");
        assert_eq!(result.content, "完整回复");
        server.await.unwrap();
    }

    #[tokio::test]
    async fn a_stream_that_ends_on_finish_reason_alone_is_a_completion() {
        // Some OpenAI-compatible servers never send `[DONE]` (llama.cpp's
        // Responses endpoint, for one). A finish_reason is end enough.
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/v1", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            read_http_headers(&mut stream).await;
            write_truncated_sse_response(
                &mut stream,
                concat!(
                    "data: {\"choices\":[{\"delta\":{\"content\":\"done thinking\"}}]}\n\n",
                    "data: {\"choices\":[{\"finish_reason\":\"stop\",\"delta\":{}}]}\n\n"
                ),
            )
            .await;
        });

        let mut provider = test_provider("no-done-marker-test", &url);
        provider.protocol = "openai-chat".to_string();
        provider.default_model = "test-model".to_string();
        let client = test_client(provider);

        let result = client
            .chat_stream(vec![ChatMessage::plain("user", "hi")], Vec::new(), |_| {
                Ok(())
            })
            .await
            .expect("finish_reason without [DONE] is a normal completion");
        assert_eq!(result.content, "done thinking");
        server.await.unwrap();
    }

    #[tokio::test]
    async fn endpoint_accepts_reasoning_only_completion() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/v1", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            read_http_headers(&mut stream).await;
            let body = concat!(
                "data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"partial reasoning\"}}]}\n\n",
                "data: {\"choices\":[{\"finish_reason\":\"length\",\"delta\":{}}]}\n\n",
                "data: [DONE]\n\n"
            );
            write_http_sse_response(&mut stream, body).await;
        });

        let mut provider = test_provider("reasoning-only-test", &url);
        provider.protocol = "openai-chat".to_string();
        provider.default_model = "test-model".to_string();
        let client = test_client(provider);
        let mut chunks = Vec::new();

        let result = client
            .chat_stream(
                vec![ChatMessage::plain("user", "hi")],
                Vec::new(),
                |chunk| {
                    chunks.push(chunk);
                    Ok(())
                },
            )
            .await
            .unwrap();

        assert!(result.content.is_empty());
        assert_eq!(result.reasoning.as_deref(), Some("partial reasoning"));
        assert_eq!(
            chunks.iter().map(|chunk| chunk.kind).collect::<Vec<_>>(),
            vec![
                ChatStreamKind::ReasoningPartStart,
                ChatStreamKind::Reasoning,
                ChatStreamKind::ReasoningPartEnd,
            ]
        );
        server.await.unwrap();
    }

    #[tokio::test]
    async fn responses_stream_rejects_eof_without_terminal_event() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/v1", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            read_http_headers(&mut stream).await;
            write_http_sse_response(
                &mut stream,
                concat!(
                    "data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_partial\"}}\n\n",
                    "data: {\"type\":\"response.output_text.delta\",\"item_id\":\"msg_1\",\"delta\":\"partial\"}\n\n"
                ),
            )
            .await;
        });

        let mut provider = test_provider("responses-eof-test", &url);
        provider.protocol = "openai-responses".to_string();
        provider.default_model = "gpt-5".to_string();
        let client = test_client(provider);

        let error = client
            .chat_stream(vec![ChatMessage::plain("user", "hi")], Vec::new(), |_| {
                Ok(())
            })
            .await
            .unwrap_err();

        assert!(
            format!("{error:#}").contains("before a terminal event"),
            "{error:#}"
        );
        server.await.unwrap();
    }

    #[tokio::test]
    async fn responses_continuation_is_pinned_to_its_original_endpoint() {
        let first_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let first_url = format!("http://{}/v1", first_listener.local_addr().unwrap());
        let second_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let second_url = format!("http://{}/v1", second_listener.local_addr().unwrap());
        let first_server = tokio::spawn(async move {
            tokio::time::timeout(Duration::from_millis(200), first_listener.accept())
                .await
                .is_ok()
        });
        let second_server = tokio::spawn(async move {
            let (mut first, _) = second_listener.accept().await.unwrap();
            read_http_headers(&mut first).await;
            write_http_sse_response(
                &mut first,
                concat!(
                    "data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_1\"}}\n\n",
                    "data: {\"type\":\"response.output_item.added\",\"item\":{\"type\":\"function_call\",\"id\":\"item_1\",\"call_id\":\"call_1\",\"name\":\"calc\",\"arguments\":\"{}\"}}\n\n",
                    "data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"function_call\",\"id\":\"item_1\",\"call_id\":\"call_1\",\"name\":\"calc\",\"arguments\":\"{}\"}}\n\n",
                    "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\"}}\n\n"
                ),
            )
            .await;

            let (mut second, _) = second_listener.accept().await.unwrap();
            read_http_headers(&mut second).await;
            write_http_sse_response(
                &mut second,
                concat!(
                    "data: {\"type\":\"response.output_text.delta\",\"item_id\":\"msg_2\",\"delta\":\"continued\"}\n\n",
                    "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_2\"}}\n\n"
                ),
            )
            .await;
        });

        let mut first_provider = test_provider("responses-shared", &first_url);
        first_provider.protocol = "openai-responses".to_string();
        first_provider.default_model = "gpt-5".to_string();
        let mut original_provider = test_provider("responses-shared", &second_url);
        original_provider.protocol = "openai-responses".to_string();
        original_provider.default_model = "gpt-5".to_string();
        let http_client = reqwest::Client::new();
        let original_endpoint = LlmEndpoint {
            client: http_client.clone(),
            provider: original_provider.clone(),
            api_key: "second".to_string(),
            key_index: 1,
        };
        let initial_client = OpenAiCompatibleClient {
            client: http_client.clone(),
            provider: original_provider.clone(),
            api_key: "second".to_string(),
            endpoints: Arc::new(vec![original_endpoint.clone()]),
            thinking_variants: HashMap::new(),
            reasoning_visibility: ReasoningVisibility::Summary,
            buffered_delivery: false,
            detailed_reasoning_summary: false,
            request_timeouts: None,
            max_tokens_override: None,
            request_scope: "chat",
        };
        let initial_result = initial_client
            .chat_stream(vec![ChatMessage::plain("user", "hi")], Vec::new(), |_| {
                Ok(())
            })
            .await
            .unwrap();
        let continuation = initial_result
            .responses_continuation
            .as_deref()
            .unwrap()
            .clone();
        assert_eq!(continuation.endpoint_id, original_endpoint.id());

        let endpoints = vec![
            LlmEndpoint {
                client: http_client.clone(),
                provider: first_provider.clone(),
                api_key: "first".to_string(),
                key_index: 0,
            },
            original_endpoint,
        ];
        let client = OpenAiCompatibleClient {
            client: http_client,
            provider: first_provider,
            api_key: "first".to_string(),
            endpoints: Arc::new(endpoints),
            thinking_variants: HashMap::new(),
            reasoning_visibility: ReasoningVisibility::Summary,
            buffered_delivery: false,
            detailed_reasoning_summary: false,
            request_timeouts: None,
            max_tokens_override: None,
            request_scope: "chat",
        };

        let result = client
            .chat_stream_with_continuation(
                vec![ChatMessage::tool("call_1", "tool result")],
                Vec::new(),
                Some(&continuation),
                |_| Ok(()),
            )
            .await
            .unwrap();

        assert_eq!(result.content, "continued");
        assert_eq!(result.provider_id.as_deref(), Some("responses-shared"));
        assert!(
            !first_server.await.unwrap(),
            "continuation used another endpoint"
        );
        second_server.await.unwrap();
    }

    #[tokio::test]
    async fn insufficient_streaming_quota_falls_back_to_non_streaming_once() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/v1", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (mut first, _) = listener.accept().await.unwrap();
            read_http_headers(&mut first).await;
            let quota = r#"{"error":{"message":"quota","code":"insufficient_quota"}}"#;
            let response = format!(
                "HTTP/1.1 429 Too Many Requests\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                quota.len(),
                quota
            );
            first.write_all(response.as_bytes()).await.unwrap();

            let (mut second, _) = listener.accept().await.unwrap();
            read_http_headers(&mut second).await;
            let body = r#"{"choices":[{"finish_reason":"stop","message":{"reasoning_content":"think","content":"answer"}}],"usage":{"prompt_tokens":3,"completion_tokens":2,"total_tokens":5}}"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            second.write_all(response.as_bytes()).await.unwrap();
        });

        let mut provider = test_provider("quota-fallback-test", &url);
        provider.protocol = "openai-chat".to_string();
        provider.default_model = "test-model".to_string();
        let client = test_client(provider);
        let mut chunks = Vec::new();
        let result = client
            .chat_stream(
                vec![ChatMessage::plain("user", "hi")],
                Vec::new(),
                |chunk| {
                    chunks.push(chunk);
                    Ok(())
                },
            )
            .await
            .unwrap();

        assert_eq!(result.content, "answer");
        assert_eq!(result.reasoning.as_deref(), Some("think"));
        assert_eq!(result.usage.unwrap().total_tokens, 5);
        assert_eq!(
            chunks.iter().map(|chunk| chunk.kind).collect::<Vec<_>>(),
            vec![
                ChatStreamKind::ReasoningPartStart,
                ChatStreamKind::Reasoning,
                ChatStreamKind::ReasoningPartEnd,
                ChatStreamKind::Content,
            ]
        );
        server.await.unwrap();
    }

    #[tokio::test]
    async fn endpoint_failover_resets_partial_reasoning_before_retry() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/v1", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let bodies = [
                concat!(
                    "data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"old\"}}]}\n\n",
                    "data: {\"error\":{\"message\":\"upstream stream failed\"}}\n\n"
                ),
                concat!(
                    "data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"new\"}}]}\n\n",
                    "data: {\"choices\":[{\"delta\":{\"content\":\"answer\"}}]}\n\n",
                    "data: [DONE]\n\n"
                ),
            ];
            for body in bodies {
                let (mut stream, _) = listener.accept().await.unwrap();
                read_http_headers(&mut stream).await;
                write_http_sse_response(&mut stream, body).await;
            }
        });

        let mut first = test_provider("failover-first-test", &url);
        first.protocol = "openai-chat".to_string();
        first.default_model = "test-model".to_string();
        let mut second = test_provider("failover-second-test", &url);
        second.protocol = "openai-chat".to_string();
        second.default_model = "test-model".to_string();
        let first_client = reqwest::Client::new();
        let second_client = reqwest::Client::new();
        let endpoints = vec![
            LlmEndpoint {
                client: first_client.clone(),
                provider: first.clone(),
                api_key: "first".to_string(),
                key_index: 0,
            },
            LlmEndpoint {
                client: second_client,
                provider: second,
                api_key: "second".to_string(),
                key_index: 0,
            },
        ];
        let client = OpenAiCompatibleClient {
            client: first_client,
            provider: first,
            api_key: "first".to_string(),
            endpoints: Arc::new(endpoints),
            thinking_variants: HashMap::new(),
            reasoning_visibility: ReasoningVisibility::Summary,
            buffered_delivery: false,
            detailed_reasoning_summary: false,
            request_timeouts: None,
            max_tokens_override: None,
            request_scope: "chat",
        };
        let mut chunks = Vec::new();

        let result = client
            .chat_stream(
                vec![ChatMessage::plain("user", "hi")],
                Vec::new(),
                |chunk| {
                    chunks.push(chunk);
                    Ok(())
                },
            )
            .await
            .unwrap();

        assert_eq!(result.reasoning.as_deref(), Some("new"));
        assert_eq!(result.content, "answer");
        assert_eq!(
            chunks.iter().map(|chunk| chunk.kind).collect::<Vec<_>>(),
            vec![
                ChatStreamKind::ReasoningPartStart,
                ChatStreamKind::Reasoning,
                ChatStreamKind::ReasoningReset,
                ChatStreamKind::ReasoningPartStart,
                ChatStreamKind::Reasoning,
                ChatStreamKind::ReasoningPartEnd,
                ChatStreamKind::Content,
            ]
        );
        server.await.unwrap();
    }

    #[tokio::test]
    async fn buffered_completion_fails_over_after_partial_content() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/v1", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let bodies = [
                concat!(
                    "data: {\"choices\":[{\"delta\":{\"content\":\"incomplete\"}}]}\n\n",
                    "data: {\"error\":{\"message\":\"upstream stream failed\"}}\n\n"
                ),
                concat!(
                    "data: {\"choices\":[{\"delta\":{\"content\":\"answer\"}}]}\n\n",
                    "data: [DONE]\n\n"
                ),
            ];
            for body in bodies {
                let (mut stream, _) = listener.accept().await.unwrap();
                read_http_headers(&mut stream).await;
                write_http_sse_response(&mut stream, body).await;
            }
        });

        let mut first = test_provider("buffered-first-test", &url);
        first.protocol = "openai-chat".to_string();
        let mut second = test_provider("buffered-second-test", &url);
        second.protocol = "openai-chat".to_string();
        let http_client = reqwest::Client::new();
        let endpoints = vec![
            LlmEndpoint {
                client: http_client.clone(),
                provider: first.clone(),
                api_key: "first".to_string(),
                key_index: 0,
            },
            LlmEndpoint {
                client: http_client.clone(),
                provider: second,
                api_key: "second".to_string(),
                key_index: 0,
            },
        ];
        let client = OpenAiCompatibleClient {
            client: http_client,
            provider: first,
            api_key: "first".to_string(),
            endpoints: Arc::new(endpoints),
            thinking_variants: HashMap::new(),
            reasoning_visibility: ReasoningVisibility::Summary,
            buffered_delivery: false,
            detailed_reasoning_summary: false,
            request_timeouts: None,
            max_tokens_override: None,
            request_scope: "chat",
        };

        let result = client
            .chat_buffered(vec![ChatMessage::plain("user", "hi")], Vec::new())
            .await
            .unwrap();
        assert_eq!(result.content, "answer");
        server.await.unwrap();
    }

    #[tokio::test]
    async fn endpoint_client_reuses_one_tcp_connection() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/reuse", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            for _ in 0..2 {
                read_http_headers(&mut stream).await;
                stream
                    .write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: keep-alive\r\n\r\nok",
                    )
                    .await
                    .unwrap();
            }
        });

        let client = test_client(test_provider("test", "http://example.invalid/v1"));
        for request_id in ["request-one", "request-two"] {
            let endpoint_client = client.with_endpoint(&client.endpoints[0]);
            let response = tokio::time::timeout(
                Duration::from_secs(2),
                endpoint_client.send_with_transport_retry(request_id, "chat.send", || {
                    endpoint_client.client.get(&url)
                }),
            )
            .await
            .expect("request timed out instead of reusing the connection")
            .unwrap();
            assert_eq!(response.text().await.unwrap(), "ok");
        }
        tokio::time::timeout(Duration::from_secs(2), server)
            .await
            .expect("server did not observe two requests on one connection")
            .unwrap();
    }

    #[tokio::test]
    async fn transport_error_keeps_source_chain_without_url() {
        let unavailable = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = unavailable.local_addr().unwrap();
        drop(unavailable);
        let url = format!("http://{addr}/secret?api_key=do-not-log");
        let client = test_client(test_provider("test", "http://example.invalid/v1"));

        let error = client
            .send_with_transport_retry("request-test", "chat.send", || client.client.get(&url))
            .await
            .unwrap_err();
        let rendered = format!("{error:#}");

        assert!(rendered.contains("chat.send transport failed (connect)"));
        assert!(rendered.contains("error sending request"));
        assert!(!rendered.contains("api_key"));
        assert!(!rendered.contains("do-not-log"));
    }

    #[test]
    fn typed_failures_drive_endpoint_cooldowns() {
        let rate_limit = anyhow::anyhow!("provider body")
            .context(HttpStatusFailure::classify(429, "provider body"));
        let quota = anyhow::anyhow!("provider body")
            .context(HttpStatusFailure::classify(400, "quota exceeded"));
        let invalid_key = anyhow::anyhow!("provider body")
            .context(HttpStatusFailure::classify(400, "invalid api key"));
        let transport = anyhow::anyhow!("socket source").context(TransportFailure {
            stage: "chat.send",
            kind: TransportFailureKind::Connect,
        });
        let protocol = anyhow::anyhow!("invalid response shape");

        assert_eq!(
            cooldown_for_error(&rate_limit),
            Some(Duration::from_secs(600))
        );
        assert_eq!(cooldown_for_error(&quota), Some(Duration::from_secs(600)));
        assert_eq!(
            cooldown_for_error(&invalid_key),
            Some(Duration::from_secs(600))
        );
        assert_eq!(
            cooldown_for_error(&transport),
            Some(Duration::from_secs(120))
        );
        assert_eq!(cooldown_for_error(&protocol), None);
    }

    #[test]
    fn structured_provider_errors_drive_failure_semantics() {
        let rate_limit = HttpStatusFailure::classify(
            400,
            r#"{"error":{"type":"rate_limit_error","code":"rate_limit_exceeded"}}"#,
        );
        let invalid_key = HttpStatusFailure::classify(
            400,
            r#"{"error":{"type":"authentication_error","code":"invalid_api_key"}}"#,
        );
        let unavailable_model = HttpStatusFailure::classify(
            400,
            r#"{"error":{"type":"invalid_request_error","code":"model_not_available"}}"#,
        );
        let incompatible_endpoint = HttpStatusFailure::classify(
            400,
            r#"{"error":{"type":"invalid_request_error","message":"Unknown parameter: tools"}}"#,
        );
        let invalid_request = HttpStatusFailure::classify(
            400,
            r#"{"error":{"type":"invalid_request_error","message":"Malformed request body"}}"#,
        );
        let google_invalid_request = HttpStatusFailure::classify(
            400,
            r#"{"error":{"status":"InvalidArgument","message":"request rejected"}}"#,
        );
        let azure_missing_deployment = HttpStatusFailure::classify(
            400,
            r#"{"error":{"code":"DeploymentNotFound","message":"missing"}}"#,
        );
        let unknown = HttpStatusFailure::classify(400, r#"{"error":{"message":"failed"}}"#);

        assert_eq!(rate_limit.kind, HttpFailureKind::RateLimit);
        assert_eq!(invalid_key.kind, HttpFailureKind::Authentication);
        assert_eq!(unavailable_model.kind, HttpFailureKind::EndpointUnavailable);
        assert_eq!(
            incompatible_endpoint.kind,
            HttpFailureKind::EndpointIncompatible
        );
        assert_eq!(invalid_request.kind, HttpFailureKind::InvalidRequest);
        assert_eq!(google_invalid_request.kind, HttpFailureKind::InvalidRequest);
        assert_eq!(
            azure_missing_deployment.kind,
            HttpFailureKind::EndpointUnavailable
        );
        assert_eq!(unknown.kind, HttpFailureKind::Status);

        assert!(endpoint_failover_allowed(&anyhow::Error::new(
            incompatible_endpoint
        )));
        let invalid_request = anyhow::Error::new(invalid_request);
        assert_eq!(cooldown_for_error(&invalid_request), None);
        assert!(!endpoint_failover_allowed(&invalid_request));
        assert!(endpoint_failover_allowed(&anyhow::Error::new(unknown)));
    }

    #[test]
    fn scheduler_skips_cooling_endpoints_and_reports_an_exhausted_pool() {
        let first = test_client(test_provider(
            "scheduler-first",
            "http://example.invalid/v1",
        ));
        let second = test_client(test_provider(
            "scheduler-second",
            "http://example.invalid/v1",
        ));
        let endpoints = vec![first.endpoints[0].clone(), second.endpoints[0].clone()];
        let mut scheduler = LlmScheduler::default();

        scheduler.mark_failure(endpoints[0].id(), Duration::from_secs(60));
        assert_eq!(scheduler.ordered_indices(&endpoints), vec![1]);

        scheduler.mark_failure(endpoints[1].id(), Duration::from_secs(60));
        assert!(scheduler.ordered_indices(&endpoints).is_empty());

        scheduler.mark_success(&endpoints[0].id());
        assert_eq!(scheduler.ordered_indices(&endpoints), vec![0]);
    }

    #[tokio::test]
    async fn invalid_request_does_not_fail_over_to_another_endpoint() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/v1", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            read_http_headers(&mut stream).await;
            let body =
                r#"{"error":{"type":"invalid_request_error","message":"Malformed request body"}}"#;
            let response = format!(
                "HTTP/1.1 400 Bad Request\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            stream.write_all(response.as_bytes()).await.unwrap();
            tokio::time::timeout(Duration::from_millis(100), listener.accept())
                .await
                .is_err()
        });

        let mut first = test_provider("invalid-request-first", &url);
        first.protocol = "openai-chat".to_string();
        let mut second = test_provider("invalid-request-second", &url);
        second.protocol = "openai-chat".to_string();
        let http_client = reqwest::Client::new();
        let endpoints = vec![
            LlmEndpoint {
                client: http_client.clone(),
                provider: first.clone(),
                api_key: "first".to_string(),
                key_index: 0,
            },
            LlmEndpoint {
                client: http_client.clone(),
                provider: second,
                api_key: "second".to_string(),
                key_index: 0,
            },
        ];
        let client = OpenAiCompatibleClient {
            client: http_client,
            provider: first,
            api_key: "first".to_string(),
            endpoints: Arc::new(endpoints),
            thinking_variants: HashMap::new(),
            reasoning_visibility: ReasoningVisibility::Summary,
            buffered_delivery: false,
            detailed_reasoning_summary: false,
            request_timeouts: None,
            max_tokens_override: None,
            request_scope: "chat",
        };

        let error = client
            .chat_stream(vec![ChatMessage::plain("user", "hi")], Vec::new(), |_| {
                Ok(())
            })
            .await
            .unwrap_err();

        assert!(format!("{error:#}").contains("endpoint failover was suppressed"));
        assert!(
            server.await.unwrap(),
            "a second endpoint received the request"
        );
    }

    #[test]
    fn only_connect_failures_are_retried() {
        assert!(retryable_transport_failure(TransportFailureKind::Connect));
        assert!(!retryable_transport_failure(TransportFailureKind::Timeout));
        assert!(!retryable_transport_failure(TransportFailureKind::Other));
        assert!(retryable_http_status(500));
        assert!(retryable_http_status(599));
        assert!(!retryable_http_status(429));
        assert!(!retryable_http_status(400));
    }

    #[test]
    fn http_status_retry_delay_caps_at_configured_maximum() {
        assert_eq!(http_status_retry_delay(1), Duration::from_millis(10));
        assert_eq!(http_status_retry_delay(2), Duration::from_millis(20));
        assert_eq!(http_status_retry_delay(3), Duration::from_millis(40));
        assert_eq!(http_status_retry_delay(4), Duration::from_millis(80));
        assert_eq!(http_status_retry_delay(5), Duration::from_millis(120));
        assert_eq!(
            http_status_retry_delay(usize::MAX),
            Duration::from_millis(120)
        );
    }

    #[test]
    fn endpoint_failover_stops_after_irreversible_stream_output() {
        let reasoning = ChatStreamChunk {
            kind: ChatStreamKind::Reasoning,
            text: "partial".to_string(),
        };
        assert!(!stream_chunk_commits_attempt(
            &reasoning,
            ReasoningVisibility::Hidden
        ));
        assert!(!stream_chunk_commits_attempt(
            &reasoning,
            ReasoningVisibility::Summary
        ));
        assert!(stream_chunk_commits_attempt(
            &reasoning,
            ReasoningVisibility::Full
        ));
        assert!(!stream_chunk_commits_attempt(
            &ChatStreamChunk {
                kind: ChatStreamKind::Content,
                text: String::new(),
            },
            ReasoningVisibility::Full,
        ));
        let reasoning_end = ChatStreamChunk {
            kind: ChatStreamKind::ReasoningPartEnd,
            text: String::new(),
        };
        assert!(!stream_chunk_commits_attempt(
            &reasoning_end,
            ReasoningVisibility::Hidden
        ));
        assert!(stream_chunk_commits_attempt(
            &reasoning_end,
            ReasoningVisibility::Summary
        ));
        for chunk in [
            ChatStreamChunk {
                kind: ChatStreamKind::Content,
                text: "answer".to_string(),
            },
            ChatStreamChunk {
                kind: ChatStreamKind::ToolCall,
                text: "ask_question".to_string(),
            },
        ] {
            assert!(stream_chunk_commits_attempt(
                &chunk,
                ReasoningVisibility::Hidden
            ));
        }
    }

    #[test]
    fn reasoning_failover_visibility_only_follows_reasoning_display() {
        let mut config = AppConfig::default();
        assert_eq!(reasoning_visibility(&config), ReasoningVisibility::Summary);

        config.display.reasoning = " full ".to_string();
        assert_eq!(reasoning_visibility(&config), ReasoningVisibility::Full);

        config.display.reasoning = "hidden".to_string();
        config.display.tool_calls = "FULL".to_string();
        assert_eq!(reasoning_visibility(&config), ReasoningVisibility::Hidden);
    }

    #[test]
    fn responses_summary_uses_auto_and_full_uses_detailed() {
        let mut config = AppConfig::default();
        assert!(!reasoning_summary_is_detailed(&config));

        config.display.reasoning = " FULL ".to_string();
        assert!(reasoning_summary_is_detailed(&config));

        let provider = test_provider("openai", "https://api.openai.com/v1");
        let mut client = test_client(provider);
        let reasoning = client.responses_reasoning().unwrap();
        assert_eq!(reasoning.summary.as_deref(), Some("auto"));

        client.detailed_reasoning_summary = true;
        let reasoning = client.responses_reasoning().unwrap();
        assert_eq!(reasoning.summary.as_deref(), Some("detailed"));
    }

    #[test]
    fn subagent_output_visibility_follows_tool_detail_mode() {
        let provider = test_provider("openai", "https://api.openai.com/v1");
        let hidden = test_client(provider.clone()).for_subagent_output(false);
        assert_eq!(hidden.reasoning_visibility, ReasoningVisibility::Hidden);
        assert!(!hidden.detailed_reasoning_summary);

        let full = test_client(provider).for_subagent_output(true);
        assert_eq!(full.reasoning_visibility, ReasoningVisibility::Full);
        assert!(full.detailed_reasoning_summary);
    }

    async fn read_http_headers(stream: &mut tokio::net::TcpStream) {
        let mut request = Vec::new();
        let mut byte = [0u8; 1];
        while !request.ends_with(b"\r\n\r\n") {
            let read = stream.read(&mut byte).await.unwrap();
            assert_ne!(read, 0, "connection closed before request headers");
            request.push(byte[0]);
        }
    }

    async fn write_http_sse_response(stream: &mut tokio::net::TcpStream, body: &str) {
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        stream.write_all(response.as_bytes()).await.unwrap();
    }

    /// Serves an endless stream of opencode-zen-shaped 429s, counting hits.
    /// The listener is bound before the task is spawned: `#[tokio::test]` runs
    /// a current-thread runtime, so handing the address back over a blocking
    /// channel would deadlock the only thread that could serve it.
    async fn spawn_rate_limited_endpoint() -> (String, Arc<AtomicUsize>, tokio::task::JoinHandle<()>)
    {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/v1", listener.local_addr().unwrap());
        let hits = Arc::new(AtomicUsize::new(0));
        let counter = hits.clone();
        let server = tokio::spawn(async move {
            loop {
                let (mut stream, _) = listener.accept().await.unwrap();
                read_http_headers(&mut stream).await;
                counter.fetch_add(1, Ordering::SeqCst);
                let body = concat!(
                    r#"{"type":"error","error":{"type":"FreeUsageLimitError","#,
                    r#""message":"Error from provider (Console): Rate limit exceeded."}}"#
                );
                stream
                    .write_all(
                        format!(
                            "HTTP/1.1 429 Too Many Requests\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                            body.len()
                        )
                        .as_bytes(),
                    )
                    .await
                    .unwrap();
            }
        });
        (url, hits, server)
    }

    fn rate_limit_test_endpoint(id: &str, url: &str) -> LlmEndpoint {
        let mut provider = test_provider(id, url);
        provider.protocol = "openai-chat".to_string();
        provider.default_model = "big-pickle".to_string();
        LlmEndpoint {
            client: reqwest::Client::new(),
            provider,
            api_key: "public".to_string(),
            key_index: 0,
        }
    }

    fn client_over(endpoints: Vec<LlmEndpoint>) -> OpenAiCompatibleClient {
        let first = endpoints[0].clone();
        OpenAiCompatibleClient {
            client: first.client.clone(),
            provider: first.provider.clone(),
            api_key: first.api_key.clone(),
            endpoints: Arc::new(endpoints),
            thinking_variants: HashMap::new(),
            reasoning_visibility: ReasoningVisibility::Summary,
            buffered_delivery: false,
            detailed_reasoning_summary: false,
            request_timeouts: None,
            max_tokens_override: None,
            request_scope: "chat",
        }
    }

    #[tokio::test]
    async fn a_rate_limited_endpoint_costs_one_request_per_turn_not_three() {
        // Regression: `MIN_ENDPOINT_ATTEMPTS` padded the attempt list by
        // cycling the only endpoint, so a single 429 fired three back-to-back
        // requests with no backoff — and the 600s cooldown then refilled the
        // whole pool, repeating the triple every turn for the entire cooldown.
        let (url, hits, server) = spawn_rate_limited_endpoint().await;
        let client = client_over(vec![rate_limit_test_endpoint(
            "rate-limit-single-endpoint-test",
            &url,
        )]);

        for turn in 1..=3 {
            let before = hits.load(Ordering::SeqCst);
            let error = client
                .chat_stream(vec![ChatMessage::plain("user", "hi")], Vec::new(), |_| {
                    Ok(())
                })
                .await
                .unwrap_err();
            assert!(
                format!("{error:#}").contains("429"),
                "turn {turn} did not surface the rate limit: {error:#}"
            );
            assert_eq!(
                hits.load(Ordering::SeqCst) - before,
                1,
                "turn {turn} spent more than one request on a rate-limited endpoint"
            );
        }

        server.abort();
    }

    #[tokio::test]
    async fn a_rate_limited_endpoint_still_fails_over_to_a_different_one() {
        // The same-endpoint suppression must not cost cross-endpoint failover:
        // each distinct endpoint is still tried exactly once.
        let (first_url, first_hits, first_server) = spawn_rate_limited_endpoint().await;
        let (second_url, second_hits, second_server) = spawn_rate_limited_endpoint().await;
        let client = client_over(vec![
            rate_limit_test_endpoint("rate-limit-failover-first-test", &first_url),
            rate_limit_test_endpoint("rate-limit-failover-second-test", &second_url),
        ]);

        let error = client
            .chat_stream(vec![ChatMessage::plain("user", "hi")], Vec::new(), |_| {
                Ok(())
            })
            .await
            .unwrap_err();

        assert_eq!(first_hits.load(Ordering::SeqCst), 1);
        assert_eq!(second_hits.load(Ordering::SeqCst), 1);
        let rendered = format!("{error:#}");
        for id in [
            "rate-limit-failover-first-test",
            "rate-limit-failover-second-test",
        ] {
            assert!(
                rendered.contains(id),
                "{id} missing from the failure report: {rendered}"
            );
        }

        first_server.abort();
        second_server.abort();
    }

    fn test_client(provider: ProviderConfig) -> OpenAiCompatibleClient {
        let client = reqwest::Client::new();
        let endpoint = LlmEndpoint {
            client: client.clone(),
            provider: provider.clone(),
            api_key: "test".to_string(),
            key_index: 0,
        };
        OpenAiCompatibleClient {
            client,
            provider,
            api_key: "test".to_string(),
            endpoints: Arc::new(vec![endpoint]),
            thinking_variants: HashMap::new(),
            reasoning_visibility: ReasoningVisibility::Summary,
            buffered_delivery: false,
            detailed_reasoning_summary: false,
            request_timeouts: None,
            max_tokens_override: None,
            request_scope: "chat",
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
            system_scripts_dir: root.join("system/scripts"),
        }
    }

    fn test_provider(id: &str, base_url: &str) -> ProviderConfig {
        ProviderConfig {
            id: id.to_string(),
            display_name: id.to_string(),
            base_url: base_url.to_string(),
            protocol: "auto".to_string(),
            api_key: None,
            models: Vec::new(),
            model_context_window: std::collections::HashMap::new(),
            model_modalities: std::collections::HashMap::new(),
            model_costs: std::collections::HashMap::new(),
            default_model: String::new(),
            timeout_seconds: 60,
            temperature: 1.0,
            anthropic_max_tokens: 4096,
            extra_body: None,
        }
    }

    #[test]
    fn client_constructors_restore_saved_thinking_variants() {
        let temp = tempfile::tempdir().unwrap();
        let paths = test_paths(temp.path());
        std::fs::create_dir_all(&paths.state_dir).unwrap();

        let mut provider = test_provider("custom", "https://example.com/v1");
        provider.default_model = "reasoning-model".to_string();
        provider.models = vec![provider.default_model.clone()];
        provider.api_key = Some("test-key".to_string());
        let preferences = ThinkingVariantPreferences {
            selected: HashMap::from([(
                thinking_variant_key(&provider.id, &provider.default_model),
                "high".to_string(),
            )]),
            ..ThinkingVariantPreferences::default()
        };
        std::fs::write(
            thinking_variant_preferences_file(&paths),
            serde_json::to_string(&preferences).unwrap(),
        )
        .unwrap();

        let config = AppConfig {
            active_provider: provider.id.clone(),
            active_provider_models: None,
            providers: vec![provider.clone()],
            ..AppConfig::default()
        };

        let configured = OpenAiCompatibleClient::from_config(&config, &paths).unwrap();
        assert_eq!(configured.selected_thinking_variant_id(), Some("high"));

        let direct = OpenAiCompatibleClient::new(&provider, &config, &paths).unwrap();
        assert_eq!(direct.selected_thinking_variant_id(), Some("high"));
    }

    #[test]
    fn saving_thinking_variants_preserves_inactive_models_and_clears_unset_active_model() {
        let temp = tempfile::tempdir().unwrap();
        let paths = test_paths(temp.path());
        std::fs::create_dir_all(&paths.state_dir).unwrap();
        let inactive_key = thinking_variant_key("inactive", "old-model");
        let active_key = thinking_variant_key("custom", "reasoning-model");
        let preferences = ThinkingVariantPreferences {
            selected: HashMap::from([(inactive_key.clone(), "max".to_string())]),
            ..ThinkingVariantPreferences::default()
        };
        std::fs::write(
            thinking_variant_preferences_file(&paths),
            serde_json::to_string(&preferences).unwrap(),
        )
        .unwrap();

        let mut provider = test_provider("custom", "https://example.com/v1");
        provider.default_model = "reasoning-model".to_string();
        let mut client = test_client(provider);
        client
            .thinking_variants
            .insert(active_key.clone(), "high".to_string());
        client.save_thinking_variants(&paths).unwrap();

        let saved = load_thinking_variant_preferences(&paths);
        assert_eq!(
            saved.selected.get(&inactive_key).map(String::as_str),
            Some("max")
        );
        assert_eq!(
            saved.selected.get(&active_key).map(String::as_str),
            Some("high")
        );

        client.thinking_variants.remove(&active_key);
        client.save_thinking_variants(&paths).unwrap();
        let saved = load_thinking_variant_preferences(&paths);
        assert_eq!(
            saved.selected.get(&inactive_key).map(String::as_str),
            Some("max")
        );
        assert!(!saved.selected.contains_key(&active_key));
    }

    #[test]
    fn staged_thinking_variant_update_merges_only_the_edited_inactive_model() {
        let temp = tempfile::tempdir().unwrap();
        let paths = test_paths(temp.path());
        let mut staged = ThinkingVariantPreferences::load(&paths);
        staged.set("future-provider", "future-model", Some("high".to_string()));

        std::fs::create_dir_all(&paths.state_dir).unwrap();
        let concurrent_key = thinking_variant_key("other-provider", "other-model");
        let concurrent = ThinkingVariantPreferences {
            selected: HashMap::from([(concurrent_key.clone(), "max".to_string())]),
            ..ThinkingVariantPreferences::default()
        };
        std::fs::write(
            thinking_variant_preferences_file(&paths),
            serde_json::to_string(&concurrent).unwrap(),
        )
        .unwrap();

        staged.save(&paths).unwrap();

        let saved = ThinkingVariantPreferences::load(&paths);
        assert_eq!(
            saved.selected("future-provider", "future-model"),
            Some("high")
        );
        assert_eq!(
            saved.selected.get(&concurrent_key).map(String::as_str),
            Some("max")
        );
    }

    #[test]
    fn malformed_thinking_variant_state_is_not_overwritten() {
        let temp = tempfile::tempdir().unwrap();
        let paths = test_paths(temp.path());
        std::fs::create_dir_all(&paths.state_dir).unwrap();
        let path = thinking_variant_preferences_file(&paths);
        std::fs::write(&path, "{not-json").unwrap();
        let mut preferences = ThinkingVariantPreferences::load(&paths);
        preferences.set("provider", "model", Some("high".to_string()));

        let error = preferences.save(&paths).unwrap_err();

        assert!(format!("{error:#}").contains("failed to parse thinking variant state"));
        assert_eq!(std::fs::read_to_string(path).unwrap(), "{not-json");
    }

    #[test]
    fn thinking_variant_preferences_follow_provider_renames() {
        let temp = tempfile::tempdir().unwrap();
        let paths = test_paths(temp.path());
        std::fs::create_dir_all(&paths.state_dir).unwrap();
        let preferences = ThinkingVariantPreferences {
            selected: HashMap::from([
                (thinking_variant_key("old", "first"), "high".to_string()),
                (thinking_variant_key("old", "second"), "max".to_string()),
                (thinking_variant_key("other", "first"), "low".to_string()),
            ]),
            ..ThinkingVariantPreferences::default()
        };
        std::fs::write(
            thinking_variant_preferences_file(&paths),
            serde_json::to_string(&preferences).unwrap(),
        )
        .unwrap();
        let mut preferences = ThinkingVariantPreferences::load(&paths);

        preferences.set("old", "second", Some("low".to_string()));
        preferences.rename_provider("old", "new");
        let mut concurrent = ThinkingVariantPreferences::load(&paths);
        concurrent.set("old", "first", Some("medium".to_string()));
        concurrent.set("old", "second", Some("high".to_string()));
        concurrent.set("old", "late", Some("medium".to_string()));
        concurrent.save(&paths).unwrap();
        preferences.save(&paths).unwrap();

        let saved = ThinkingVariantPreferences::load(&paths);
        assert_eq!(saved.selected("new", "first"), Some("medium"));
        assert_eq!(saved.selected("new", "second"), Some("low"));
        assert_eq!(saved.selected("new", "late"), Some("medium"));
        assert_eq!(saved.selected("other", "first"), Some("low"));
        assert_eq!(saved.selected("old", "first"), None);
        assert_eq!(saved.selected("old", "second"), None);
        assert_eq!(saved.selected("old", "late"), None);
    }

    #[test]
    fn provider_rename_replays_when_the_initial_variant_snapshot_was_empty() {
        let temp = tempfile::tempdir().unwrap();
        let paths = test_paths(temp.path());
        let mut renaming = ThinkingVariantPreferences::load(&paths);
        renaming.rename_provider("old", "new");

        let mut concurrent = ThinkingVariantPreferences::load(&paths);
        concurrent.set("old", "late", Some("high".to_string()));
        concurrent.save(&paths).unwrap();
        renaming.save(&paths).unwrap();

        let saved = ThinkingVariantPreferences::load(&paths);
        assert_eq!(saved.selected("new", "late"), Some("high"));
        assert_eq!(saved.selected("old", "late"), None);
    }

    #[test]
    fn concurrent_thinking_variant_updates_keep_distinct_models() {
        let temp = tempfile::tempdir().unwrap();
        let paths = test_paths(temp.path());
        let barrier = Arc::new(std::sync::Barrier::new(2));
        let handles = ["first", "second"].map(|model| {
            let paths = paths.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                let mut preferences = ThinkingVariantPreferences::load(&paths);
                preferences.set("provider", model, Some("high".to_string()));
                barrier.wait();
                preferences.save(&paths).unwrap();
            })
        });
        for handle in handles {
            handle.join().unwrap();
        }

        let saved = ThinkingVariantPreferences::load(&paths);
        assert_eq!(saved.selected("provider", "first"), Some("high"));
        assert_eq!(saved.selected("provider", "second"), Some("high"));
    }

    #[test]
    fn reasoning_variants_use_current_wire_protocol_mapping() {
        let info = ModelReasoningInfo {
            provider_npm: Some("@openrouter/ai-sdk-provider".to_string()),
            variants: Vec::new(),
        };
        let effort = ReasoningVariant {
            id: "high".to_string(),
            setting: ReasoningSetting::Effort("high".to_string()),
        };
        let budget = ReasoningVariant {
            id: "max".to_string(),
            setting: ReasoningSetting::BudgetTokens(8000),
        };
        let provider = test_provider("openrouter", "https://openrouter.ai/api/v1");
        assert!(reasoning_variant_supported(
            &provider,
            "test-model",
            &info,
            &effort
        ));
        assert!(reasoning_variant_supported(
            &provider,
            "test-model",
            &info,
            &budget
        ));

        let unknown_info = ModelReasoningInfo {
            provider_npm: Some("@unknown/provider".to_string()),
            variants: Vec::new(),
        };
        let unknown = test_provider("proxy", "https://proxy.example/v1");
        assert!(reasoning_variant_supported(
            &unknown,
            "test-model",
            &unknown_info,
            &effort
        ));
        assert!(!reasoning_variant_supported(
            &unknown,
            "test-model",
            &unknown_info,
            &budget
        ));

        let alibaba = test_provider("alibaba-token-plan", "https://example.com/v1");
        let toggle = ReasoningVariant {
            id: "on".to_string(),
            setting: ReasoningSetting::Toggle(true),
        };
        assert!(reasoning_variant_supported(
            &alibaba,
            "test-model",
            &unknown_info,
            &toggle
        ));

        assert!(reasoning_variant_supported(
            &unknown,
            "gpt-5-mini",
            &unknown_info,
            &toggle
        ));
        assert!(!reasoning_variant_supported(
            &unknown,
            "gpt-4.1",
            &unknown_info,
            &toggle
        ));
        assert!(!reasoning_variant_supported_for_protocol(
            &unknown,
            &unknown_info,
            &toggle,
            ProviderProtocol::OpenAiChat
        ));
    }

    #[test]
    fn anthropic_budget_is_bounded_by_max_tokens() {
        assert_eq!(anthropic_reasoning_budget(4096, 2048), Some(2048));
        assert_eq!(anthropic_reasoning_budget(4096, 32_000), None);
        assert_eq!(anthropic_reasoning_budget(1024, 32_000), None);
    }

    #[test]
    fn custom_openai_compatible_provider_uses_reasoning_effort() {
        let mut provider = test_provider("ririxin", "https://token.sensenova.cn/v1");
        provider.default_model = "deepseek-v4-flash".to_string();
        let info = ModelReasoningInfo {
            provider_npm: Some("@ai-sdk/openai-compatible".to_string()),
            variants: Vec::new(),
        };

        let body = chat_variant_body(
            &provider,
            &info,
            ReasoningSetting::Effort("high".to_string()),
        )
        .unwrap();
        assert_eq!(body["reasoning_effort"], "high");
        assert!(body.get("reasoning").is_none());
    }

    #[test]
    fn mixed_client_keeps_variants_per_provider_and_model() {
        let mut first = test_provider("ririxin", "https://token.sensenova.cn/v1");
        first.default_model = "deepseek-v4-flash".to_string();
        let mut second = test_provider("opencode", "https://opencode.ai/zen/v1");
        second.default_model = "mimo-v2.5-free".to_string();
        let first_client = reqwest::Client::new();
        let second_client = reqwest::Client::new();
        let endpoints = vec![
            LlmEndpoint {
                client: first_client.clone(),
                provider: first.clone(),
                api_key: "first".to_string(),
                key_index: 0,
            },
            LlmEndpoint {
                client: second_client,
                provider: second,
                api_key: "second".to_string(),
                key_index: 0,
            },
        ];
        let mut client = OpenAiCompatibleClient {
            client: first_client,
            provider: first,
            api_key: "first".to_string(),
            endpoints: Arc::new(endpoints),
            thinking_variants: HashMap::from([(
                thinking_variant_key("ririxin", "deepseek-v4-flash"),
                "high".to_string(),
            )]),
            reasoning_visibility: ReasoningVisibility::Summary,
            buffered_delivery: false,
            detailed_reasoning_summary: false,
            request_timeouts: None,
            max_tokens_override: None,
            request_scope: "chat",
        };

        let first_endpoint = client.with_endpoint(&client.endpoints[0]);
        let second_endpoint = client.with_endpoint(&client.endpoints[1]);
        assert_eq!(first_endpoint.selected_thinking_variant_id(), Some("high"));
        assert_eq!(second_endpoint.selected_thinking_variant_id(), None);
        client.thinking_variants.insert(
            thinking_variant_key("opencode", "mimo-v2.5-free"),
            "max".to_string(),
        );
        let second_endpoint = client.with_endpoint(&client.endpoints[1]);
        assert_eq!(second_endpoint.selected_thinking_variant_id(), Some("max"));
        assert_eq!(first_endpoint.selected_thinking_variant_id(), Some("high"));
    }

    #[test]
    fn variant_extra_body_merges_nested_reasoning_fields() {
        let base = json!({ "reasoning": { "exclude": true }, "custom": 1 })
            .as_object()
            .cloned();
        let variant = json!({ "reasoning": { "effort": "high" } })
            .as_object()
            .cloned();

        let merged = merge_extra_body(base, variant).unwrap();
        assert_eq!(merged["reasoning"]["exclude"], true);
        assert_eq!(merged["reasoning"]["effort"], "high");
        assert_eq!(merged["custom"], 1);
    }

    #[test]
    fn test_chat_request_extra_body_flatten() {
        use serde_json::json;

        let extra = json!({
            "model": "override",
            "messages": [],
            "enable_thinking": false,
            "custom_param": "value"
        })
        .as_object()
        .cloned();

        let request = ChatRequest {
            model: "gpt-4".to_string(),
            messages: vec![ChatMessage::plain("user", "Hello")],
            temperature: 0.7,
            stream: true,
            stream_options: Some(ChatStreamOptions {
                include_usage: true,
            }),
            max_tokens: None,
            tools: None,
            chat_template_kwargs: None,
            extra_body: sanitize_extra_body(extra, CHAT_RESERVED_BODY_KEYS),
        };

        let serialized = serde_json::to_string(&request).unwrap();
        let value = serde_json::to_value(&request).unwrap();

        assert_eq!(value["enable_thinking"], false);
        assert_eq!(value["custom_param"], "value");
        assert_eq!(value["model"], "gpt-4");
        let temp = value["temperature"].as_f64().unwrap();
        assert!((temp - 0.7).abs() < 1e-6);
        assert!(value.get("extra_body").is_none());
        assert_eq!(serialized.matches("\"model\":").count(), 1);
        assert_eq!(serialized.matches("\"messages\":").count(), 1);
    }

    #[test]
    fn test_responses_request_extra_body_flatten() {
        use serde_json::json;

        let extra = json!({
            "input": [],
            "previous_response_id": "wrong",
            "reasoning": {"effort": "high"},
            "reasoning_effort": "high",
            "parallel_tool_calls": false
        })
        .as_object()
        .cloned();

        let request = ResponsesRequest {
            model: "gpt-5".to_string(),
            input: vec![json!({"role": "user", "content": "Hello"})],
            instructions: None,
            previous_response_id: Some("resp_good".to_string()),
            stream: true,
            tools: None,
            reasoning: Some(ResponsesReasoning {
                effort: Some("medium".to_string()),
                summary: Some("concise".to_string()),
            }),
            temperature: Some(0.5),
            extra_body: sanitize_extra_body(extra, RESPONSES_RESERVED_BODY_KEYS),
        };

        let serialized = serde_json::to_string(&request).unwrap();
        let value = serde_json::to_value(&request).unwrap();

        assert_eq!(value["reasoning_effort"], "high");
        assert_eq!(value["parallel_tool_calls"], false);
        assert_eq!(value["model"], "gpt-5");
        assert_eq!(value["previous_response_id"], "resp_good");
        assert_eq!(value["reasoning"]["effort"], "medium");
        assert_eq!(value["temperature"], 0.5);
        assert!(value.get("extra_body").is_none());
        assert_eq!(serialized.matches("\"input\":").count(), 1);
        assert_eq!(serialized.matches("\"previous_response_id\":").count(), 1);
        assert_eq!(serialized.matches("\"reasoning\":").count(), 1);
    }

    #[test]
    fn test_anthropic_request_extra_body_flatten() {
        use serde_json::json;

        let extra = json!({
            "system": "override",
            "max_tokens": 1,
            "thinking": {"type": "disabled"},
            "metadata": {"user_id": "123"}
        })
        .as_object()
        .cloned();
        let mut provider = test_provider("anthropic", "https://api.anthropic.com/v1");
        provider.default_model = "claude-3-opus".to_string();
        provider.extra_body = extra;
        let client = test_client(provider);
        let request = client.anthropic_request(
            vec![
                ChatMessage::plain("system", "You are helpful"),
                ChatMessage::plain("user", "Hello"),
            ],
            Vec::new(),
            true,
        );

        let serialized = serde_json::to_string(&request).unwrap();
        let value = serde_json::to_value(&request).unwrap();

        assert_eq!(value["metadata"]["user_id"], "123");
        assert_eq!(value["system"], "You are helpful");
        assert_eq!(value["thinking"]["type"], "adaptive");
        assert_eq!(value["model"], "claude-3-opus");
        assert_eq!(value["max_tokens"], 4096);
        assert!(value.get("extra_body").is_none());
        assert_eq!(serialized.matches("\"system\":").count(), 1);
        assert_eq!(serialized.matches("\"max_tokens\":").count(), 1);
        assert_eq!(serialized.matches("\"thinking\":").count(), 1);
    }

    #[test]
    fn extra_body_reserved_keys_match_each_protocol() {
        for reserved in [
            CHAT_RESERVED_BODY_KEYS,
            RESPONSES_RESERVED_BODY_KEYS,
            ANTHROPIC_RESERVED_BODY_KEYS,
        ] {
            let mut extra = serde_json::Map::new();
            for key in reserved {
                extra.insert((*key).to_string(), serde_json::json!("override"));
            }
            extra.insert("custom".to_string(), serde_json::json!("keep"));

            let sanitized = sanitize_extra_body(Some(extra), reserved).unwrap();
            assert_eq!(sanitized.len(), 1);
            assert_eq!(sanitized["custom"], "keep");
        }
    }
}

fn strip_tagged_sections(mut text: String, tag: &str) -> String {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let open_prefix = format!("<{tag}");
    loop {
        let Some(start) = text.find(&open_prefix) else {
            break;
        };
        let content_start = text[start..]
            .find('>')
            .map(|offset| start + offset + 1)
            .unwrap_or(start + open.len());
        let Some(relative_end) = text[content_start..].find(&close) else {
            text.replace_range(start.., "");
            break;
        };
        let end = content_start + relative_end + close.len();
        text.replace_range(start..end, "");
    }
    text
}
