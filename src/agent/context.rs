//! 上下文组装：哪些消息进请求、以什么顺序、占多少 token。
//!
//! **这是字节纯度的核心区**。供应商的前缀缓存只比字节：从第一个不同的字节起
//! 后面全部作废。所以这里的每个函数都要满足两条——同样的输入必然产出同样的
//! 字节；新增内容一律追加在尾部，不许插进已有内容中间。
//!
//! 「化石」（fossil）是这里的关键概念：临时上下文（记忆片段、平台身份、模式提
//! 醒）在被送出去一次之后就固化成一条历史消息，后续回合原样回放。这样做的原因
//! 正是缓存——如果每轮都重新生成这些块，它们的措辞和顺序稍有变化就会打掉整条
//! 前缀。`replay_fossil` / `fossil_context_messages` 就是这套机制的两端。
//!
//! 裁剪（`prune_*`、`spill_replacement`、`truncate_middle_chars`）动的是**尾部**
//! 已经放不下的部分，动不到前缀，所以不影响命中。

use crate::agent::*;

/// The fossilizable prefix of a transient tail: the contiguous run of
/// system-role text messages. Stops at the first non-system or non-text
/// message so redo checkpoints (which append loop messages) never leak
/// assistant/tool content into the fossil record.
/// Marks turn-context blocks that are standing advisories about recent state
/// (not about the current message): only these may be skipped when an
/// identical copy is already visible in a replayed fossil. Producers opt in
/// by using this prefix (reply_processor's long-reply conversion notice).
pub(in crate::agent) const STANDING_ADVISORY_PREFIX: &str = "[SystemInfo:";

/// True when `block`'s exact text already appears inside a user-role message
/// of the request being built (a fossilized turn tail replayed from an earlier
/// turn). Stops standing notices from re-fossilizing identical bytes every
/// turn; a block whose content changed no longer matches and is sent again.
pub(in crate::agent) fn turn_context_block_visible(messages: &[ChatMessage], block: &str) -> bool {
    messages.iter().any(|message| {
        message.role == "user"
            && matches!(
                message.content.as_ref(),
                Some(ChatContent::Text(text)) if text.contains(block)
            )
    })
}

/// Collects the associative-memory entry lines already visible in the request
/// being built. Fossilized blocks replay as `user` messages whose text starts
/// with the block tag, so matching on that prefix picks up exactly the earlier
/// injections (legacy `system` fossils are re-roled to `user` before this
/// point). Matching whole rendered lines means an updated memory — new content
/// or date — no longer matches and gets injected again.
pub(in crate::agent) fn visible_association_lines(messages: &[ChatMessage]) -> HashSet<&str> {
    let mut seen = HashSet::new();
    for message in messages {
        if message.role != "user" {
            continue;
        }
        let Some(ChatContent::Text(text)) = message.content.as_ref() else {
            continue;
        };
        if !text.starts_with("<associative-memory") {
            continue;
        }
        for line in text.lines() {
            if line.starts_with("- [") {
                seen.insert(line.trim_end());
            }
        }
    }
    seen
}

/// replay_start 之前最近一条非瞬态 user 消息=本轮真实用户输入的下标。
pub(in crate::agent) fn live_user_index(
    messages: &[ChatMessage],
    replay_start: usize,
) -> Option<usize> {
    let end = replay_start.min(messages.len());
    (0..end).rev().find(|&index| {
        let message = &messages[index];
        message.role == "user" && !message.transient_context
    })
}

/// 已拼装消息里最近一条以 `prefix` 开头的 user 侧文本(倒序首个)。
/// 不检查 transient 标志:回放化石反序列化后该标志会丢(serde skip),
/// 而以 `<runtime ` 开头的用户输入不存在——按内容前缀即可唯一识别。
pub(in crate::agent) fn last_fossil_with_prefix<'a>(
    messages: &'a [ChatMessage],
    prefix: &str,
) -> Option<&'a str> {
    messages.iter().rev().find_map(|message| {
        if message.role != "user" {
            return None;
        }
        match message.content.as_ref() {
            Some(ChatContent::Text(text)) if text.starts_with(prefix) => Some(text.as_str()),
            _ => None,
        }
    })
}

#[cfg(test)]
mod projection_tests {
    use crate::agent::*;

    #[test]
    fn runtime_projection_skips_byte_identical_fossil() {
        let stamp = "<runtime now=\"2026年08月16日 Sunday 00时\" cwd=\"/x\"/>";
        let mut messages = vec![
            ChatMessage::system("s"),
            ChatMessage::plain("user", "hi"),
            ChatMessage::turn_context(stamp.to_string()),
            ChatMessage::assistant("ok".to_string(), None),
        ];
        assert_eq!(last_fossil_with_prefix(&messages, "<runtime "), Some(stamp));
        // 变化才注入:相同→跳过,不同→追加。
        messages.push(ChatMessage::turn_context(
            "<runtime now=\"2026年08月16日 Sunday 01时\" cwd=\"/x\"/>".to_string(),
        ));
        assert_ne!(last_fossil_with_prefix(&messages, "<runtime "), Some(stamp));
    }
}

pub(in crate::agent) fn fossil_context_messages(tail: &[ChatMessage]) -> Vec<ChatMessage> {
    // Keyed on the explicit marker rather than the role: these blocks now ride
    // as `user` messages (see `ChatMessage::turn_context`), which is
    // indistinguishable by role from a real user turn.
    tail.iter()
        .take_while(|message| {
            message.transient_context
                && matches!(message.content.as_ref(), Some(ChatContent::Text(_)))
        })
        .cloned()
        .collect()
}

/// Fossils written before the role change are stored as `system`. Replaying
/// them verbatim would keep re-poisoning the prefix for the rest of the
/// session, so they are re-roled on the way out: one cold start at the upgrade
/// boundary, byte-stable forever after.
pub(in crate::agent) fn replay_fossil(message: &ChatMessage) -> ChatMessage {
    if message.role != "system" {
        return message.clone();
    }
    let mut message = message.clone();
    message.role = "user".to_string();
    message.transient_context = true;
    message
}

pub(in crate::agent) fn replace_request_mode_context(
    messages: &mut [ChatMessage],
    system_prompt: &str,
    mode: AgentMode,
    platform: bool,
) {
    if let Some(system) = messages.first_mut() {
        *system = ChatMessage::system(system_prompt);
    }
    // runtime 块不再原地改写:已发出的块是前缀的一部分,改它等于把本轮尾巴
    // 的前缀掰断——CLI 中转线上 followup 的第二次请求因此整段全量重放
    // (09-04 seq 220 实证)。新的 runtime 由 followup 路径按"变了才追加"
    // 的规矩挂在 followup 正文之后,与首轮注入同一口径。
    let _ = (mode, platform);
}

pub(in crate::agent) fn continuation_system_prompt(system_prompt: &str, mode: AgentMode) -> String {
    let mode = match mode {
        AgentMode::Normal => "normal",
        AgentMode::Dev => "dev",
    };
    format!(
        "<mode-update active=\"{mode}\">This supersedes all earlier mode-specific instructions.</mode-update>\n\n{system_prompt}"
    )
}

pub(in crate::agent) fn estimate_result_tokens(result: &ChatResult) -> usize {
    let mut tokens = crate::token_estimate::estimate_tokens(&result.content);
    if let Some(reasoning) = &result.reasoning {
        tokens = tokens.saturating_add(crate::token_estimate::estimate_tokens(reasoning));
    }
    for call in &result.tool_calls {
        tokens = tokens.saturating_add(crate::token_estimate::estimate_tokens(&call.function.name));
        tokens = tokens.saturating_add(crate::token_estimate::estimate_tokens(
            &call.function.arguments,
        ));
    }
    tokens.max(1)
}

/// 工具目录 token 估算的记忆表。
///
/// 工具目录是**字节稳定**的（那是缓存契约的一部分），而这个估算每轮至少跑
/// 三次（`setup.rs` 的上下文总账、`history.rs` 的两处装载决策）。实测估一遍
/// 61 个工具 / 63 KB 要 **38.3 ms**，而「序列化+哈希」——也就是判断「还是不是
/// 同一份目录」的固定成本——只要 1.6 ms。省掉 96%。
///
/// 键里带上序列化后的总长度，不只是哈希：FxHash 快但雪崩性一般，多一个长度
/// 维度让碰撞必须两项同时撞上。真撞了后果也只是 token **估**值偏一点（这个
/// 数只用于溢出记账，不参与请求组装），不会动到字节纯度。
///
/// 表有上限：不同模式/注册面各是一个键（hybrid 档在世时每种「已装载工具
/// 子集」还各占一个，09-01 删档后键少了，上限留着防御）。满了整表清空——
/// 重建一次 38 ms，而不是留个逐出策略在这儿养 bug。
static TOOL_TOKEN_CACHE: std::sync::LazyLock<
    std::sync::Mutex<rustc_hash::FxHashMap<(u64, usize), usize>>,
> = std::sync::LazyLock::new(|| std::sync::Mutex::new(rustc_hash::FxHashMap::default()));

const TOOL_TOKEN_CACHE_CAP: usize = 64;

pub(in crate::agent) fn estimate_tool_definition_tokens(
    definitions: &[crate::llm::ToolDefinition],
) -> usize {
    use std::hash::{Hash, Hasher};

    let mut hasher = rustc_hash::FxHasher::default();
    let mut length = 0usize;
    let texts = definitions
        .iter()
        .filter_map(|definition| serde_json::to_string(definition).ok())
        .inspect(|text| {
            text.hash(&mut hasher);
            length += text.len();
        })
        .collect::<Vec<_>>();
    let key = (hasher.finish(), length);

    if let Some(&cached) = TOOL_TOKEN_CACHE.lock().unwrap().get(&key) {
        return cached;
    }
    let total = texts
        .iter()
        .map(|text| crate::token_estimate::estimate_tokens(text))
        .sum();
    let mut cache = TOOL_TOKEN_CACHE.lock().unwrap();
    if cache.len() >= TOOL_TOKEN_CACHE_CAP {
        cache.clear();
    }
    cache.insert(key, total);
    total
}

pub(in crate::agent) fn push_assistant_context_messages(
    messages: &mut Vec<ChatMessage>,
    content: &str,
    reasoning: Option<&str>,
    force_assistant_message: bool,
) {
    push_assistant_message_with_reasoning(
        messages,
        content.to_string(),
        reasoning,
        None,
        None,
        force_assistant_message,
    );
}

pub(in crate::agent) fn push_assistant_message_with_reasoning(
    messages: &mut Vec<ChatMessage>,
    content: String,
    reasoning: Option<&str>,
    thinking_signature: Option<&str>,
    tool_calls: Option<Vec<ToolCall>>,
    force_assistant_message: bool,
) {
    let has_tool_calls = tool_calls.as_ref().is_some_and(|calls| !calls.is_empty());
    if has_tool_calls {
        // A17: DeepSeek thinking mode requires the `reasoning_content` KEY on
        // assistant tool_calls turns of the live tool loop (an empty string is
        // accepted, a missing key is a 400). Carry it on the assistant message
        // itself; the provider adapter strips it for endpoints that do not
        // understand the field and rebuilds the Anthropic thinking block from
        // the signature where present.
        let mut message = ChatMessage::assistant(content, tool_calls);
        message.reasoning_content = Some(reasoning.unwrap_or_default().to_string());
        message.thinking_signature = thinking_signature.map(str::to_string);
        messages.push(message);
        return;
    }
    // 跨轮思考回放退役(验收 08-16):正常完成轮的正式回复已承载结论,
    // 思维链副本纯属冗余——官方语义 reasoning 是轮内产物(普通轮回传被
    // API 忽略),dsh 同款丢弃。中断恢复不走这里:journal 专道
    // (interrupted_turn_replay_messages)仍原样重放中断前的思考。
    let _ = reasoning;
    if force_assistant_message || !content.trim().is_empty() {
        messages.push(ChatMessage::assistant(content, None));
    }
}

pub(in crate::agent) fn turn_context_tokens(turn: &crate::state::Turn) -> usize {
    let mut messages = vec![ChatMessage::plain("user", &turn.user_content)];
    // Fossilized transient tail is replayed with the turn, so count it.
    messages.extend(turn.context_messages.iter().cloned());
    // 与 push_history_turn 同步:有结构化 flow 的回合问答对不再回放。
    // remote 轮(中转侧工具活动)不回放,判定与 push_history_turn 同步。
    let has_native_flow = turn.tool_flow.iter().any(|round| !round.remote);
    let replay_exchanges: &[crate::question::QuestionExchange] = if !has_native_flow {
        &turn.question_exchanges
    } else {
        &[]
    };
    for exchange in replay_exchanges {
        messages.push(ChatMessage::plain(
            "assistant",
            crate::question::assistant_exchange_text(exchange),
        ));
        messages.push(ChatMessage::plain(
            "user",
            crate::question::user_exchange_text(exchange),
        ));
    }
    for followup in &turn.followups {
        push_assistant_context_messages(
            &mut messages,
            followup
                .preceding_assistant_content
                .as_deref()
                .unwrap_or_default(),
            followup.preceding_assistant_reasoning.as_deref(),
            false,
        );
        messages.push(ChatMessage::plain("user", &followup.content));
        messages.extend(followup.context_messages());
    }
    // 与 push_history_turn 同步:工具轮以原生 tool_calls + tool 输出回放,
    // 漏计 tool_flow 会让 trim/压缩预算对工具密集回合失真数十倍。
    for round in replay_rounds(&turn.tool_flow) {
        push_assistant_message_with_reasoning(
            &mut messages,
            round.assistant_content.clone(),
            round.assistant_reasoning.as_deref(),
            None,
            Some(
                round
                    .calls
                    .iter()
                    .map(|call| ToolCall {
                        id: call.id.clone(),
                        kind: "function".to_string(),
                        function: ToolCallFunction {
                            name: call.name.clone(),
                            arguments: call.arguments.clone(),
                        },
                    })
                    .collect(),
            ),
            false,
        );
        for call in &round.calls {
            messages.push(ChatMessage::tool(call.id.clone(), call.output.clone()));
        }
    }
    push_assistant_context_messages(
        &mut messages,
        &turn.assistant_content,
        turn.assistant_reasoning.as_deref(),
        true,
    );
    // 与 push_history_turn 同步:reports 压扁只在无结构化 flow 时回放。
    if !has_native_flow && !turn.tool_reports.is_empty() {
        messages.push(ChatMessage::turn_context(private_tool_memory(
            &turn.tool_reports,
        )));
    }
    overflow::estimate_messages_tokens(&messages)
}

pub(in crate::agent) fn followup_assistant_replay_content(
    followup: &crate::state::TurnFollowup,
) -> Option<&str> {
    followup
        .preceding_assistant_content
        .as_deref()
        .filter(|content| !content.trim().is_empty())
        .or_else(|| {
            followup
                .preceding_assistant_reasoning
                .as_deref()
                .filter(|reasoning| !reasoning.trim().is_empty())
        })
}

pub(in crate::agent) fn interrupted_turn_replay_messages(
    agent: &Agent,
    turn: &crate::state::Turn,
) -> Vec<ChatMessage> {
    let mut messages = Vec::new();
    messages.push(ChatMessage::turn_context(
        "<interrupted-turn-recovery>The previous reply was interrupted. Below is the model output and tool progress that had already been persisted before the interruption; do not re-run tools that already completed — continue handling the current user request from this content.</interrupted-turn-recovery>",
    ));

    // A redo revision only journals the new branch. Preserve the already
    // committed clarification/follow-up prefix from the turn row before
    // replaying the new branch's events.
    let replayed_prompt_ids = turn
        .journal_events
        .iter()
        .filter(|event| event.kind == "queued_prompts_consumed")
        .flat_map(|event| {
            event
                .text_payload
                .as_deref()
                .and_then(|payload| serde_json::from_str::<Vec<String>>(payload).ok())
                .unwrap_or_default()
        })
        .collect::<HashSet<_>>();
    if turn.revision > 0 {
        let prefix_question_count = turn
            .journal_events
            .iter()
            .find(|event| event.kind == "redo_prefix_question_count")
            .and_then(|event| event.text_payload.as_deref())
            .and_then(|count| count.parse::<usize>().ok())
            .unwrap_or_else(|| {
                let branch_answers = turn
                    .journal_events
                    .iter()
                    .filter(|event| {
                        event.kind == "tool_result"
                            && event.name.as_deref() == Some("ask_question")
                            && event
                                .text_payload
                                .as_deref()
                                .and_then(|payload| serde_json::from_str::<Value>(payload).ok())
                                .and_then(|payload| {
                                    payload
                                        .get("status")
                                        .and_then(Value::as_str)
                                        .map(|status| status == "answered")
                                })
                                .unwrap_or(false)
                    })
                    .count();
                turn.question_exchanges.len().saturating_sub(branch_answers)
            });
        for exchange in turn.question_exchanges.iter().take(prefix_question_count) {
            messages.push(ChatMessage::plain(
                "assistant",
                crate::question::assistant_exchange_text(exchange),
            ));
            messages.push(ChatMessage::plain(
                "user",
                crate::question::user_exchange_text(exchange),
            ));
        }
        for followup in &turn.followups {
            if replayed_prompt_ids.contains(&followup.prompt_id) {
                continue;
            }
            push_assistant_context_messages(
                &mut messages,
                followup
                    .preceding_assistant_content
                    .as_deref()
                    .unwrap_or_default(),
                followup.preceding_assistant_reasoning.as_deref(),
                false,
            );
            messages.push(agent.followup_user_message(followup));
            messages.extend(followup.context_messages().iter().map(replay_fossil));
        }
    }

    let mut assistant_text = String::new();
    let mut assistant_reasoning = String::new();
    let mut pending_calls = Vec::<ToolCall>::new();
    let mut open_calls = Vec::<ToolCall>::new();
    let mut progress = HashMap::<String, String>::new();
    let mut command_tail = HashMap::<String, Vec<u8>>::new();

    for event in &turn.journal_events {
        match event.kind.as_str() {
            "assistant_content" => {
                if let Some(text) = &event.text_payload {
                    assistant_text.push_str(text);
                }
            }
            "assistant_reasoning" => {
                if let Some(text) = &event.text_payload {
                    assistant_reasoning.push_str(text);
                }
            }
            "reasoning_reset" => assistant_reasoning.clear(),
            "tool_call" => {
                let Some(call_id) = event.call_id.clone() else {
                    continue;
                };
                let Some(name) = event.name.as_deref() else {
                    continue;
                };
                pending_calls.push(ToolCall {
                    id: call_id,
                    kind: "function".to_string(),
                    function: ToolCallFunction {
                        name: replay_tool_function_name(name),
                        arguments: event.text_payload.clone().unwrap_or_default(),
                    },
                });
            }
            "tool_result" => {
                open_calls.extend(flush_interrupted_assistant(
                    &mut messages,
                    &mut assistant_reasoning,
                    &mut assistant_text,
                    &mut pending_calls,
                ));
                if let Some(call_id) = &event.call_id {
                    // 空的 tool 结果同样不可回放:多数上游把它当协议错误。
                    let output = event
                        .text_payload
                        .as_deref()
                        .map(str::trim)
                        .filter(|text| !text.is_empty())
                        .unwrap_or("(no output)");
                    messages.push(ChatMessage::tool(call_id, truncate_chars(output, 48_000)));
                    open_calls.retain(|call| call.id != *call_id);
                    progress.remove(call_id);
                    command_tail.remove(call_id);
                }
            }
            "tool_progress" => {
                if let Some(call_id) = &event.call_id {
                    progress.insert(
                        call_id.clone(),
                        truncate_chars(event.text_payload.as_deref().unwrap_or_default(), 4_000),
                    );
                }
            }
            "command_stdout" | "command_stderr" => {
                if let Some(call_id) = &event.call_id {
                    let tail = command_tail.entry(call_id.clone()).or_default();
                    if let Some(bytes) = &event.blob_payload {
                        tail.extend_from_slice(bytes);
                        const MAX_COMMAND_TAIL: usize = 8 * 1024;
                        if tail.len() > MAX_COMMAND_TAIL {
                            let start = tail.len() - MAX_COMMAND_TAIL;
                            tail.drain(..start);
                        }
                    }
                }
            }
            "queued_prompts_consumed" => {
                open_calls.extend(flush_interrupted_assistant(
                    &mut messages,
                    &mut assistant_reasoning,
                    &mut assistant_text,
                    &mut pending_calls,
                ));
                append_interrupted_tool_results(
                    &mut messages,
                    &mut open_calls,
                    &mut progress,
                    &mut command_tail,
                );
                let prompt_ids = event
                    .text_payload
                    .as_deref()
                    .and_then(|payload| serde_json::from_str::<Vec<String>>(payload).ok())
                    .unwrap_or_default();
                for prompt_id in prompt_ids {
                    if let Some(followup) = turn
                        .followups
                        .iter()
                        .find(|followup| followup.prompt_id == prompt_id)
                    {
                        messages.push(agent.followup_user_message(followup));
                    }
                }
            }
            _ => {}
        }
    }

    open_calls.extend(flush_interrupted_assistant(
        &mut messages,
        &mut assistant_reasoning,
        &mut assistant_text,
        &mut pending_calls,
    ));
    append_interrupted_tool_results(
        &mut messages,
        &mut open_calls,
        &mut progress,
        &mut command_tail,
    );
    messages
}

pub(in crate::agent) fn flush_interrupted_assistant(
    messages: &mut Vec<ChatMessage>,
    assistant_reasoning: &mut String,
    assistant_text: &mut String,
    pending_calls: &mut Vec<ToolCall>,
) -> Vec<ToolCall> {
    if assistant_reasoning.trim().is_empty()
        && assistant_text.trim().is_empty()
        && pending_calls.is_empty()
    {
        return Vec::new();
    }
    if !assistant_reasoning.trim().is_empty() {
        if let Some(reasoning) = private_reasoning_memory(assistant_reasoning) {
            messages.push(ChatMessage::turn_context(reasoning));
        }
    }
    assistant_reasoning.clear();
    let text = std::mem::take(assistant_text);
    let calls = std::mem::take(pending_calls);
    let replay_calls = (!calls.is_empty()).then(|| calls.clone());
    messages.push(ChatMessage::assistant(text, replay_calls));
    calls
}

pub(in crate::agent) fn append_interrupted_tool_results(
    messages: &mut Vec<ChatMessage>,
    open_calls: &mut Vec<ToolCall>,
    progress: &mut HashMap<String, String>,
    command_tail: &mut HashMap<String, Vec<u8>>,
) {
    for call in std::mem::take(open_calls) {
        let mut output =
            "tool execution was interrupted before a final result was persisted".to_string();
        if let Some(message) = progress.remove(&call.id) {
            output.push_str("\nlast progress: ");
            output.push_str(&message);
        }
        if let Some(bytes) = command_tail.remove(&call.id) {
            let tail = String::from_utf8_lossy(&bytes);
            if !tail.trim().is_empty() {
                output.push_str("\nlast command output:\n");
                output.push_str(&truncate_chars(&tail, 8_000));
            }
        }
        messages.push(ChatMessage::tool(call.id, output));
    }
}

pub(in crate::agent) fn replay_tool_function_name(name: &str) -> String {
    match name.split_once(':').map(|(prefix, _)| prefix) {
        Some("load_skill") | Some("load_tools") | Some("task") => {
            name.split(':').next().unwrap_or(name).to_string()
        }
        _ => name.to_string(),
    }
}

pub(in crate::agent) fn redo_checkpoint_payload(
    messages: &[ChatMessage],
    replay_start: usize,
    base_tool_reports: &[String],
    pending_tool_reports: &[(String, String)],
    tool_rounds: usize,
    question_rounds: usize,
) -> TurnRedoCheckpointPayload {
    let mut prefix_tool_reports = Vec::with_capacity(
        base_tool_reports
            .len()
            .saturating_add(pending_tool_reports.len()),
    );
    prefix_tool_reports.extend(base_tool_reports.iter().cloned());
    prefix_tool_reports.extend(
        pending_tool_reports
            .iter()
            .map(|(_, report)| report.clone()),
    );
    TurnRedoCheckpointPayload {
        replay_messages: messages.get(replay_start..).unwrap_or_default().to_vec(),
        prefix_tool_reports,
        tool_rounds,
        question_rounds,
        loaded_items: Vec::new(),
        prefix_question_count: 0,
        prefix_image_asset_ids: Vec::new(),
        prefix_artifact_asset_ids: Vec::new(),
    }
}

pub(in crate::agent) fn evicted_turn_entries(
    turns: &[crate::state::Turn],
) -> (Vec<crate::state::StoredConversationEntry>, Vec<EvictedTurn>) {
    let mut entries = Vec::new();
    let mut evicted = Vec::new();
    for turn in turns {
        entries.push(crate::state::StoredConversationEntry {
            timestamp: turn.user_timestamp.clone(),
            role: "user".to_string(),
            content: turn.user_content.clone(),
            reasoning: None,
        });
        evicted.push(EvictedTurn {
            source_id: format!("{}:user", turn.turn_id),
            timestamp: turn.user_timestamp.clone(),
            role: "user".to_string(),
            content: turn.user_content.clone(),
            ..EvictedTurn::default()
        });

        for (index, exchange) in turn.question_exchanges.iter().enumerate() {
            let timestamp = exchange.answered_at.clone();
            let assistant_content = crate::question::assistant_exchange_text(exchange);
            entries.push(crate::state::StoredConversationEntry {
                timestamp: timestamp.clone(),
                role: "assistant_clarification".to_string(),
                content: assistant_content.clone(),
                reasoning: None,
            });
            evicted.push(EvictedTurn {
                source_id: format!("{}:question:{index}", turn.turn_id),
                timestamp: timestamp.clone(),
                role: "assistant".to_string(),
                content: assistant_content,
                ..EvictedTurn::default()
            });
            let user_content = crate::question::user_exchange_text(exchange);
            entries.push(crate::state::StoredConversationEntry {
                timestamp: timestamp.clone(),
                role: "user_clarification".to_string(),
                content: user_content.clone(),
                reasoning: None,
            });
            evicted.push(EvictedTurn {
                source_id: format!("{}:answer:{index}", turn.turn_id),
                timestamp,
                role: "user".to_string(),
                content: user_content,
                ..EvictedTurn::default()
            });
        }

        for followup in &turn.followups {
            if followup_assistant_replay_content(followup).is_some() {
                let content = followup
                    .preceding_assistant_content
                    .clone()
                    .unwrap_or_default();
                entries.push(crate::state::StoredConversationEntry {
                    timestamp: followup.submitted_at.clone(),
                    role: "assistant".to_string(),
                    content: content.clone(),
                    reasoning: followup.preceding_assistant_reasoning.clone(),
                });
                evicted.push(EvictedTurn {
                    source_id: format!("{}:before:{}", turn.turn_id, followup.prompt_id),
                    timestamp: followup.submitted_at.clone(),
                    role: "assistant".to_string(),
                    content,
                    ..EvictedTurn::default()
                });
            }
            entries.push(crate::state::StoredConversationEntry {
                timestamp: followup.submitted_at.clone(),
                role: "user".to_string(),
                content: followup.content.clone(),
                reasoning: None,
            });
            evicted.push(EvictedTurn {
                source_id: format!("{}:followup:{}", turn.turn_id, followup.prompt_id),
                timestamp: followup.submitted_at.clone(),
                role: "user".to_string(),
                content: followup.content.clone(),
                ..EvictedTurn::default()
            });
        }

        let timestamp = turn.assistant_timestamp.clone().unwrap_or_default();
        entries.push(crate::state::StoredConversationEntry {
            timestamp: timestamp.clone(),
            role: "assistant".to_string(),
            content: turn.assistant_content.clone(),
            reasoning: turn.assistant_reasoning.clone(),
        });
        evicted.push(EvictedTurn {
            source_id: format!("{}:assistant", turn.turn_id),
            timestamp: timestamp.clone(),
            role: "assistant".to_string(),
            content: turn.assistant_content.clone(),
            ..EvictedTurn::default()
        });

        for (index, report) in turn.tool_reports.iter().enumerate() {
            entries.push(crate::state::StoredConversationEntry {
                timestamp: timestamp.clone(),
                role: "assistant".to_string(),
                content: report.clone(),
                reasoning: None,
            });
            evicted.push(EvictedTurn {
                source_id: format!("{}:tool:{index}", turn.turn_id),
                timestamp: timestamp.clone(),
                role: "assistant".to_string(),
                content: report.clone(),
                ..EvictedTurn::default()
            });
        }
    }
    (entries, evicted)
}

pub(crate) fn archive_and_delete_visible_turns(
    state: &StateStore,
    memory: &MemoryStore,
    turns: &[crate::state::Turn],
) -> Result<Vec<crate::state::StoredConversationEntry>> {
    archive_and_delete_visible_turns_checked(state, memory, turns, None)
}

pub(in crate::agent) fn archive_and_delete_visible_turns_checked(
    state: &StateStore,
    memory: &MemoryStore,
    turns: &[crate::state::Turn],
    expected_loaded_tools: Option<&[(String, Option<String>)]>,
) -> Result<Vec<crate::state::StoredConversationEntry>> {
    let (entries, mut evicted) = evicted_turn_entries(turns);
    memory.apply_evicted_ownership(&mut evicted);
    let turn_ids = turns
        .iter()
        .map(|turn| turn.turn_id.clone())
        .collect::<Vec<_>>();
    if let Some(archive_db) = memory.prepare_evicted_context_db()? {
        state.archive_and_delete_visible_turns(
            &archive_db,
            &evicted,
            &turn_ids,
            expected_loaded_tools,
        )?;
    } else if expected_loaded_tools.is_some() {
        state.delete_visible_turns_checked(&turn_ids, expected_loaded_tools)?;
    } else {
        state.delete_visible_turns(&turn_ids)?;
    }
    Ok(entries)
}

/// The transient runtime stamp that rides the turn tail.
///
/// `platform` strips everything a chat message cannot use. A QQ turn has no
/// working directory, no shell and no terminal — those attributes were pure
/// scaffolding there, and they were re-sent at full price on every single
/// turn (285 chars against a ~45-char timestamp).
/// 距最近一次防失忆提醒化石过去了多少个可见轮;None=历史里没有提醒。
pub(in crate::agent) fn turns_since_reminder_fossil(
    state: &crate::state::StateStore,
    current_turn_id: &str,
) -> Result<Option<usize>> {
    let turns = state.load_visible_turns_excluding(current_turn_id)?;
    let mut since = None;
    for turn in &turns {
        if turn.is_summary || turn.status == crate::state::TurnStatus::Running {
            continue;
        }
        let has_reminder = turn.context_messages.iter().any(|fossil| {
            matches!(
                fossil.content.as_ref(),
                Some(ChatContent::Text(text)) if text.starts_with("<persona-reminder>")
            )
        });
        if has_reminder {
            since = Some(0);
        } else if let Some(count) = since.as_mut() {
            *count += 1;
        }
    }
    Ok(since)
}

#[cfg(test)]
mod tool_token_cache_tests {
    use super::*;
    use crate::llm::{FunctionDefinition, ToolDefinition};

    fn definition(name: &str, description: &str) -> ToolDefinition {
        ToolDefinition {
            kind: "function",
            function: FunctionDefinition {
                name: name.to_string(),
                description: description.to_string(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {},
                    "additionalProperties": false,
                }),
            },
        }
    }

    /// 缓存不能改答案：命中和未命中必须是同一个数。
    #[test]
    fn the_cache_returns_what_a_fresh_computation_would() {
        let definitions = vec![
            definition("alpha", "第一个工具，描述得长一点好让 token 数不至于是 1"),
            definition("beta", "第二个工具，同样写长一些"),
        ];
        let uncached: usize = definitions
            .iter()
            .filter_map(|definition| serde_json::to_string(definition).ok())
            .map(|text| crate::token_estimate::estimate_tokens(&text))
            .sum();

        let first = estimate_tool_definition_tokens(&definitions);
        let second = estimate_tool_definition_tokens(&definitions);
        assert_eq!(first, uncached);
        assert_eq!(second, uncached, "第二次（命中缓存）跟第一次不一致");
    }

    /// 目录变了就必须重算——lazy 模式下每装载一个工具就是一份新目录。
    #[test]
    fn a_different_catalogue_gets_a_different_answer() {
        let small = vec![definition("only", "就一个工具")];
        let large = vec![
            definition("only", "就一个工具"),
            definition("extra", "又装载了一个，描述还挺长的，token 数应该明显变多"),
        ];
        assert!(
            estimate_tool_definition_tokens(&large) > estimate_tool_definition_tokens(&small),
            "目录变大了 token 估算却没变"
        );
    }

    /// 表满了清空，不能无限涨。
    #[test]
    fn the_cache_stays_bounded() {
        for index in 0..TOOL_TOKEN_CACHE_CAP * 2 + 5 {
            let definitions = vec![definition(&format!("tool{index}"), "描述")];
            estimate_tool_definition_tokens(&definitions);
        }
        assert!(TOOL_TOKEN_CACHE.lock().unwrap().len() <= TOOL_TOKEN_CACHE_CAP);
    }
}
