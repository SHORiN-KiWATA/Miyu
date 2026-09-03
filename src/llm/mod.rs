mod cache_log;
mod openai_compatible;
pub(crate) mod provider_capabilities;
pub mod request_log;

pub(crate) use openai_compatible::{
    forget_relay_sessions, remove_antigravity_relay_files, thinking_variant_options_for_model,
    ThinkingVariantPreferences,
};
pub use openai_compatible::{OpenAiCompatibleClient, ThinkingVariantOptions};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<ChatContent>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCall>>,
    /// DeepSeek thinking mode requires the `reasoning_content` KEY to be present
    /// on assistant tool_calls turns of subsequent requests (empty string is
    /// accepted; a missing key is a 400). Only serialized when `Some`; the
    /// provider adapter strips it for endpoints that do not understand it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_content: Option<String>,
    /// Anthropic extended-thinking signature captured from the stream; needed to
    /// rebuild the `thinking` block when replaying the assistant turn within the
    /// same tool loop. Never serialized into OpenAI-style JSON directly.
    #[serde(default, skip_serializing, skip_deserializing)]
    pub thinking_signature: Option<String>,
    /// Marks a block built by [`ChatMessage::turn_context`]: the transient tail
    /// that gets fossilized into the turn record. Not part of the wire format
    /// and not persisted — it only has to survive from construction until the
    /// tail is sliced off, and reloaded fossils are replayed as stored.
    #[serde(default, skip_serializing, skip_deserializing)]
    pub transient_context: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ChatContent {
    Text(String),
    Parts(Vec<ChatContentPart>),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ChatContentPart {
    #[serde(rename = "text")]
    Text { text: String },
    #[serde(rename = "image_url")]
    ImageUrl { image_url: ImageUrlContent },
    /// 视频输入(08-22):OpenRouter/Qwen 系 openai-chat 约定
    /// `{"type":"video_url","video_url":{"url":…}}`,仅视频能力模型接受。
    #[serde(rename = "video_url")]
    VideoUrl { video_url: VideoUrlContent },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImageUrlContent {
    pub url: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VideoUrlContent {
    pub url: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub function: ToolCallFunction,
}

/// 工具调用参数在发上线之前必须是合法 JSON **对象**。
///
/// 非法就换成 `{}`：那次调用本来就废了（工具侧会报自己的解析错误），把半截
/// JSON 发出去只会让整条请求被上游拒掉。要求"对象"而不只是"合法 JSON"——
/// `5`、`"x"`、`[1,2]` 也是合法 JSON，但两家 wire 格式都要求对象。
pub(crate) fn wire_safe_tool_arguments(arguments: &str) -> String {
    let trimmed = arguments.trim();
    if serde_json::from_str::<serde_json::Value>(trimmed).is_ok_and(|value| value.is_object()) {
        return trimmed.to_string();
    }
    "{}".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCallFunction {
    pub name: String,
    pub arguments: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ToolDefinition {
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub function: FunctionDefinition,
}

#[derive(Debug, Clone, Serialize)]
pub struct FunctionDefinition {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
}

impl ChatMessage {
    fn base(role: impl Into<String>) -> Self {
        Self {
            role: role.into(),
            content: None,
            tool_call_id: None,
            tool_calls: None,
            reasoning_content: None,
            thinking_signature: None,
            transient_context: false,
        }
    }

    pub fn system(content: impl Into<String>) -> Self {
        Self {
            content: Some(ChatContent::Text(content.into())),
            ..Self::base("system")
        }
    }

    /// A context block that rides inside the conversation rather than heading
    /// it: runtime stamp, associative memory, hints, fossilized transient tail.
    ///
    /// Deliberately the `user` role, not `system`. Provider chat templates
    /// gather every `system` message to the front of the rendered prompt, so
    /// adding one *anywhere* mid-conversation shifts that front block and
    /// invalidates the whole prefix cache behind it. Measured against DeepSeek
    /// with a byte-identical prefix, single variable, same instant:
    ///
    /// | second request appends      | prefix cache hit |
    /// |----------------------------|------------------|
    /// | assistant + user           | 99%              |
    /// | system + assistant + user  | 0%               |
    /// | user(same text) + assistant + user | 99%      |
    ///
    /// Position made no difference — a `system` message appended at the very
    /// end killed it just the same. The blocks carry their own XML-ish framing
    /// (`<runtime …/>`, `<system-reminder>`, `<associative-memory>`), so the
    /// model still reads them as context rather than as the user speaking.
    pub fn turn_context(content: impl Into<String>) -> Self {
        Self {
            content: Some(ChatContent::Text(content.into())),
            transient_context: true,
            ..Self::base("user")
        }
    }

    pub fn assistant(content: impl Into<String>, tool_calls: Option<Vec<ToolCall>>) -> Self {
        let text = content.into();
        // 参数在进消息的这一刻就必须是合法 JSON 对象。模型偶尔把 native
        // tool_call 只写了个头就截断(实测 mimo-v2.5 的
        // `{"action": "mute", "duration_seconds": `),那一串一旦随消息发出去,
        // OpenAI→Anthropic 的代理解析不了就整条 HTTP 500,而且它会被化石化
        // 进历史,之后每轮回放都炸——会话从此永久不可用。
        //
        // 收口放在构造函数里而不是各个调用点:08-17 按调用点补了两轮,
        // 主循环补完漏了子代理,历史回放补完漏了回合内的活体路径。这里是
        // tool_calls 进入消息的唯一入口,补一次就全覆盖。
        let tool_calls = tool_calls.map(|calls| {
            calls
                .into_iter()
                .map(|mut call| {
                    call.function.arguments = wire_safe_tool_arguments(&call.function.arguments);
                    call
                })
                .collect::<Vec<_>>()
        });
        let has_tool_calls = tool_calls.as_ref().map(|c| !c.is_empty()).unwrap_or(false);
        let content = if text.trim().is_empty() && has_tool_calls {
            // Keep an explicit empty string so the `content` key stays present on
            // tool_calls turns: some strict gateways 400 on a missing key.
            Some(ChatContent::Text(String::new()))
        } else {
            Some(ChatContent::Text(text))
        };
        Self {
            content,
            tool_calls,
            ..Self::base("assistant")
        }
    }

    pub fn tool(tool_call_id: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            content: Some(ChatContent::Text(content.into())),
            tool_call_id: Some(tool_call_id.into()),
            ..Self::base("tool")
        }
    }

    pub fn plain(role: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            content: Some(ChatContent::Text(content.into())),
            ..Self::base(role)
        }
    }

    pub fn user_parts(parts: Vec<ChatContentPart>) -> Self {
        Self {
            content: Some(ChatContent::Parts(parts)),
            ..Self::base("user")
        }
    }

    pub fn user_with_image(text: impl Into<String>, image_url: impl Into<String>) -> Self {
        Self {
            content: Some(ChatContent::Parts(vec![
                ChatContentPart::Text { text: text.into() },
                ChatContentPart::ImageUrl {
                    image_url: ImageUrlContent {
                        url: image_url.into(),
                    },
                },
            ])),
            ..Self::base("user")
        }
    }

    pub fn user_with_video(text: impl Into<String>, video_url: impl Into<String>) -> Self {
        Self {
            content: Some(ChatContent::Parts(vec![
                ChatContentPart::Text { text: text.into() },
                ChatContentPart::VideoUrl {
                    video_url: VideoUrlContent {
                        url: video_url.into(),
                    },
                },
            ])),
            ..Self::base("user")
        }
    }
}

fn u64_is_zero(value: &u64) -> bool {
    *value == 0
}

fn bool_is_false(value: &bool) -> bool {
    !*value
}

/// OpenAI-style `usage.prompt_tokens_details`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PromptTokensDetails {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cached_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write_tokens: Option<u64>,
}

/// OpenAI-style `usage.completion_tokens_details`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CompletionTokensDetails {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_tokens: Option<u64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Usage {
    #[serde(default)]
    pub prompt_tokens: u64,
    #[serde(default)]
    pub completion_tokens: u64,
    #[serde(default)]
    pub total_tokens: u64,
    /// DeepSeek reports cache accounting at the top level of `usage`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_cache_hit_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_cache_miss_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_tokens_details: Option<PromptTokensDetails>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completion_tokens_details: Option<CompletionTokensDetails>,
    /// Normalized invariant across providers: prompt_tokens = total input,
    /// cache_read_tokens ⊆ prompt_tokens, cache_write_tokens is the portion
    /// written to cache where the provider reports it (Anthropic/OpenAI 5.6+).
    #[serde(default, skip_serializing_if = "u64_is_zero")]
    pub cache_read_tokens: u64,
    #[serde(default, skip_serializing_if = "u64_is_zero")]
    pub cache_write_tokens: u64,
    #[serde(default, skip_serializing_if = "u64_is_zero")]
    pub reasoning_tokens: u64,
    /// True when the provider reported any cache accounting for this request.
    /// Distinguishes "0 cached" from "provider does not report caching" so
    /// hit-rate stats do not treat DeepSeek cold requests as unsupported.
    #[serde(default, skip_serializing_if = "bool_is_false")]
    pub cache_reported: bool,
}

impl Usage {
    pub fn effective_total_tokens(&self) -> u64 {
        if self.total_tokens > 0 {
            self.total_tokens
        } else {
            self.prompt_tokens.saturating_add(self.completion_tokens)
        }
    }

    /// Fold provider-specific raw fields into the normalized cache columns.
    /// Idempotent; call once wherever a provider usage payload enters Miyu.
    pub fn normalize_cache_fields(&mut self) {
        if let Some(hit) = self.prompt_cache_hit_tokens {
            self.cache_read_tokens = self.cache_read_tokens.max(hit);
            self.cache_reported = true;
        }
        if self.prompt_cache_miss_tokens.is_some() {
            self.cache_reported = true;
        }
        if let Some(details) = &self.prompt_tokens_details {
            if let Some(cached) = details.cached_tokens {
                self.cache_read_tokens = self.cache_read_tokens.max(cached);
                self.cache_reported = true;
            }
            if let Some(write) = details.cache_write_tokens {
                self.cache_write_tokens = self.cache_write_tokens.max(write);
                self.cache_reported = true;
            }
        }
        if let Some(details) = &self.completion_tokens_details {
            if let Some(reasoning) = details.reasoning_tokens {
                self.reasoning_tokens = self.reasoning_tokens.max(reasoning);
            }
        }
        // Guard the invariant instead of papering over a broken adapter: a
        // cache_read larger than the whole prompt means the mapping is wrong.
        if self.cache_read_tokens > self.prompt_tokens && self.prompt_tokens > 0 {
            self.cache_read_tokens = self.prompt_tokens;
        }
    }

    pub fn uncached_prompt_tokens(&self) -> u64 {
        self.prompt_tokens.saturating_sub(self.cache_read_tokens)
    }
}

/// The slice of a turn's usage that is worth persisting. `total` alone cannot
/// express a cache hit rate: a hit is an input-side property — output tokens
/// only enter the prompt on the *next* turn — so the rate needs `prompt` as
/// its denominator, and both halves have to survive into the database for the
/// cumulative figure to mean anything.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TurnTokens {
    pub total: u64,
    pub prompt: u64,
    pub cache_read: u64,
}

impl TurnTokens {
    pub fn from_usage(usage: Option<&Usage>) -> Self {
        usage
            .map(|usage| Self {
                total: usage.effective_total_tokens(),
                prompt: usage.prompt_tokens,
                cache_read: usage.cache_read_tokens,
            })
            .unwrap_or_default()
    }

    pub fn add(&mut self, other: TurnTokens) {
        self.total = self.total.saturating_add(other.total);
        self.prompt = self.prompt.saturating_add(other.prompt);
        self.cache_read = self.cache_read.saturating_add(other.cache_read);
    }
}

#[derive(Debug, Clone)]
pub(crate) struct ResponsesContinuation {
    pub(crate) response_id: String,
    pub(crate) endpoint_id: String,
}

#[derive(Debug, Clone)]
pub struct ChatResult {
    pub content: String,
    pub reasoning: Option<String>,
    pub usage: Option<Usage>,
    pub usage_estimated: bool,
    pub tool_calls: Vec<ToolCall>,
    pub provider_id: Option<String>,
    pub model: Option<String>,
    /// Provider finish reason ("stop" / "length" / "tool_calls" / ...). A
    /// "length" stop with pending tool calls means the arguments may be
    /// silently truncated; the agent refuses to execute them in that case.
    pub finish_reason: Option<String>,
    /// Anthropic extended-thinking signature for this assistant turn, needed
    /// to replay the thinking block on the next request of the same tool loop.
    pub thinking_signature: Option<String>,
    /// Raw usage of the FINAL request of the turn (when `usage` holds the
    /// turn-accumulated sum). Its prompt+completion is the true provider-side
    /// context size, used for the context meter instead of a local estimate.
    pub last_request_usage: Option<Usage>,
    pub(crate) responses_continuation: Option<Box<ResponsesContinuation>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChatStreamKind {
    Content,
    Reasoning,
    ReasoningReset,
    ReasoningPartStart,
    ReasoningPartEnd,
    ToolCall,
    /// 中转(claude-code)侧闭环执行的工具调用開始:text 是
    /// `{id,name,input}` JSON,回合层翻成标准 tool.started 卡片。
    RemoteToolStarted,
    /// 同上的收口:text 是 `{id,name,ok,output}` JSON → tool.finished。
    RemoteToolFinished,
}

#[derive(Debug, Clone)]
pub struct ChatStreamChunk {
    pub kind: ChatStreamKind,
    pub text: String,
}

/// Provider-agnostic context-overflow classifier (compact-and-retry passive
/// trigger). Exclusions are checked before matches: Bedrock-style throttling
/// text ("Too many tokens, please wait") overlaps the overflow wording, and a
/// rate-limit misread would turn a transient 429 into a destructive
/// compaction. Patterns are the fixed-substring core of the pi/opencode
/// regex sets (DeepSeek / OpenAI / Anthropic / gateway variants).
pub fn is_context_overflow_message(message: &str) -> bool {
    let lower = message.to_lowercase();
    const EXCLUSIONS: &[&str] = &["rate limit", "too many requests", "throttling"];
    if EXCLUSIONS.iter().any(|pattern| lower.contains(pattern)) {
        return false;
    }
    const PATTERNS: &[&str] = &[
        "prompt is too long", // Anthropic
        "request_too_large",  // Anthropic HTTP 413
        "request entity too large",
        "input is too long for requested model", // Bedrock
        "exceeds the context window",            // OpenAI
        "maximum context length",                // OpenAI-compatible / gateways
        "reduce the length of the messages",     // Groq
        "context window exceeds limit",          // MiniMax
        "exceeded model token limit",            // Kimi
        "but the configured context size",       // DeepSeek
        "model_context_window_exceeded",         // z.ai
        "context_length_exceeded",               // OpenAI error code
        "context length exceeded",
        "prompt too long",                    // Ollama
        "greater than the context length",    // LM Studio
        "exceeds the available context size", // llama.cpp
        "too many tokens",                    // generic fallback
        "token limit exceeded",               // generic fallback
    ];
    PATTERNS.iter().any(|pattern| lower.contains(pattern))
}

pub fn is_context_overflow_error(error: &anyhow::Error) -> bool {
    is_context_overflow_message(&format!("{error:#}"))
}

/// Responses 续传不被上游支持的签名错误:第二步只发增量(带
/// previous_response_id),而上游没有服务端会话状态,就找不到
/// function_call_output 对应的 function_call。窄匹配上游原文,
/// 命中即触发"清续传+全量重发+持久记 false"自愈(任务#16)。
///
/// 三种措辞(issue #32:只认第一种时,newapi 网关转发的 OpenAI 官方变体
/// "…for function call output with call_id …"漏网,同一请求重试三次后
/// 直接报错,自愈从未触发):
/// - "No tool call found for tool output"(部分网关)
/// - "No tool call found for function call output"(OpenAI 官方措辞)
/// - 自家探测 bail("OpenAI Responses continuation is not supported"):
///   能力探测已知不支持却还带着续传,同样该走清续传全量重发。
pub fn is_responses_continuation_unsupported_error(error: &anyhow::Error) -> bool {
    let text = format!("{error:#}");
    text.contains("No tool call found for tool output")
        || text.contains("No tool call found for function call output")
        || text.contains("OpenAI Responses continuation is not supported")
}

#[cfg(test)]
mod tool_argument_tests {
    use super::{ChatMessage, ToolCall, ToolCallFunction};

    fn call(arguments: &str) -> ToolCall {
        ToolCall {
            id: "c1".to_string(),
            kind: "function".to_string(),
            function: ToolCallFunction {
                name: "qq_group_manage".to_string(),
                arguments: arguments.to_string(),
            },
        }
    }

    /// 半截 JSON 不能随消息发上线。实测 mimo-v2.5 会把一次 native tool_call
    /// 只写个头就截断,那一串发出去上游直接 500,而且会被化石化进历史,之后
    /// 每轮回放都炸。收口在构造函数里,主循环/子代理/历史回放/中断重建全覆盖。
    #[test]
    fn assistant_messages_never_carry_unparseable_tool_arguments() {
        for broken in [
            r#"{"action": "mute", "duration_seconds": "#,
            "",
            "   ",
            "not json",
            "{",
            // 合法 JSON 但不是对象:两家 wire 格式都要求对象。
            "5",
            r#""text""#,
            "null",
            "[1,2]",
        ] {
            let message = ChatMessage::assistant("", Some(vec![call(broken)]));
            let calls = message.tool_calls.unwrap();
            assert_eq!(calls[0].function.arguments, "{}", "{broken:?}");
        }
    }

    /// 合法参数原样通过(只去首尾空白),别把好调用改坏。
    #[test]
    fn valid_tool_arguments_pass_through_untouched() {
        for good in [r#"{"a":1}"#, "{}", r#"{"nested":{"b":[1,2]}}"#] {
            let message = ChatMessage::assistant("", Some(vec![call(good)]));
            assert_eq!(message.tool_calls.unwrap()[0].function.arguments, good);
        }
        let message = ChatMessage::assistant("", Some(vec![call("  {\"a\":1}  ")]));
        assert_eq!(
            message.tool_calls.unwrap()[0].function.arguments,
            r#"{"a":1}"#
        );
    }
}

#[cfg(test)]
mod overflow_classifier_tests {
    use super::is_context_overflow_message;

    #[test]
    fn matches_provider_overflow_messages() {
        for msg in [
            "400 This model's maximum context length is 65536 tokens",
            "prompt is too long: 210000 tokens > 200000 maximum",
            "The prompt has 131500 tokens, but the configured context size is 131072 tokens",
            "error code context_length_exceeded",
            "Input validation error: input is too long for requested model",
            "Please reduce the length of the messages or completion",
        ] {
            assert!(is_context_overflow_message(msg), "should match: {msg}");
        }
    }

    #[test]
    fn rate_limit_wording_is_excluded_before_matching() {
        for msg in [
            "ThrottlingException: Too many tokens, please wait before trying again",
            "429 rate limit exceeded, maximum context length notwithstanding",
            "Too Many Requests",
        ] {
            assert!(!is_context_overflow_message(msg), "must not match: {msg}");
        }
    }

    #[test]
    fn unrelated_errors_do_not_match() {
        assert!(!is_context_overflow_message("connection reset by peer"));
        assert!(!is_context_overflow_message("invalid api key"));
    }
}

#[cfg(test)]
mod continuation_signature_tests {
    /// issue #32:三种措辞都要命中——只认网关变体时 OpenAI 官方措辞
    /// ("…for function call output with call_id …")漏网,自愈从未触发。
    #[test]
    fn continuation_unsupported_matches_all_known_wordings() {
        let hit = |text: &str| {
            super::is_responses_continuation_unsupported_error(&anyhow::anyhow!("{}", text))
        };
        assert!(hit(
            "status_code=400, No tool call found for tool output with id x"
        ));
        assert!(hit(
            "status_code=400, No tool call found for function call output with call_id call_AhcSn"
        ));
        assert!(hit(
            "no LLM provider/model endpoint succeeded: - newapi / gpt: OpenAI Responses continuation is not supported by this provider"
        ));
        assert!(!hit("upstream returned HTTP 400: bad request"));
    }
}
