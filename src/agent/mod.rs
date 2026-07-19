mod compact;
mod conversation;
pub(crate) mod overflow;

use crate::clipboard::{ClipboardImage, PastedImage};
use crate::config::AppConfig;
use crate::llm::{
    ChatContent, ChatContentPart, ChatMessage, ChatResult, ChatStreamChunk, ChatStreamKind,
    ImageUrlContent, OpenAiCompatibleClient, Usage,
};
use crate::memory::{EvictedTurn, MemoryStore};
use crate::paths::MiyuPaths;
use crate::question::{
    answered_tool_output, unavailable_tool_output, QuestionCancelled, QuestionExchange,
    QuestionRequest, QuestionResponse,
};
use crate::render::wait_spinner::SPINNER_INTERVAL;
use crate::state::StateStore;
use crate::tools::{self, memes, vision, ToolPermission, ToolRegistry};
use anyhow::{bail, Result};
use chrono::Local;
use serde_json::Value;
use std::collections::BTreeSet;
use std::io::IsTerminal;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, oneshot};

const MAX_QUESTION_ROUNDS_PER_TURN: usize = 8;

pub struct PendingTurnGuard {
    state: StateStore,
    turn_id: String,
    completed: bool,
}

impl PendingTurnGuard {
    pub fn new(state: StateStore, turn_id: String) -> Self {
        Self {
            state,
            turn_id,
            completed: false,
        }
    }

    pub fn complete(
        mut self,
        content: &str,
        reasoning: Option<&str>,
        token_total: Option<u64>,
        token_usage_estimated: bool,
    ) -> Result<()> {
        self.state.complete_turn_with_usage(
            &self.turn_id,
            content,
            reasoning,
            token_total,
            token_usage_estimated,
        )?;
        self.completed = true;
        Ok(())
    }

    #[allow(dead_code)]
    pub fn interrupt(&mut self) -> Result<()> {
        if !self.completed {
            self.state.interrupt_turn(&self.turn_id)?;
            self.completed = true;
        }
        Ok(())
    }
}

impl Drop for PendingTurnGuard {
    fn drop(&mut self) {
        if !self.completed {
            let _ = self.state.interrupt_turn(&self.turn_id);
        }
    }
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum AgentMode {
    Normal,
    Plan,
    Chat,
}

impl AgentMode {
    pub fn label(self) -> &'static str {
        if crate::i18n::is_zh() {
            match self {
                Self::Normal => "普通",
                Self::Plan => "计划",
                Self::Chat => "闲聊",
            }
        } else {
            match self {
                Self::Normal => "NORMAL",
                Self::Plan => "PLAN",
                Self::Chat => "CHAT",
            }
        }
    }

    fn reminder(self) -> Option<&'static str> {
        match self {
            Self::Normal => None,
            Self::Plan => Some(crate::prompts::PLAN_REMINDER),
            Self::Chat => Some(crate::prompts::CHAT_REMINDER),
        }
    }
}

#[derive(Debug)]
pub enum AgentEvent {
    Chunk(ChatStreamChunk),
    ReasoningStart {
        received_at: Instant,
    },
    ReasoningReset {
        received_at: Instant,
    },
    ReasoningPartStart {
        received_at: Instant,
    },
    ReasoningPartEnd {
        received_at: Instant,
    },
    ReasoningTitle(String),
    ToolCall {
        name: String,
        arguments: String,
    },
    ToolResult {
        name: String,
        ok: bool,
        output: String,
    },
    ToolProgress {
        name: String,
        message: String,
    },
    CommandOutput {
        name: String,
        stream: tools::CommandOutputStream,
        chunk: Vec<u8>,
    },
    PrepareForExternalOutput {
        ready: oneshot::Sender<()>,
    },
    AskQuestion {
        request: QuestionRequest,
        responder: oneshot::Sender<QuestionResponse>,
    },
    SpinnerTick,
    CompactStart,
    CompactChunk(ChatStreamChunk),
    CompactEnd,
    PopStart,
    PopEnd,
}

fn emit_tool_progress<F>(
    on_event: &mut F,
    name: &str,
    progress: tools::ToolProgressEvent,
) -> Result<()>
where
    F: FnMut(AgentEvent) -> Result<()>,
{
    match progress {
        tools::ToolProgressEvent::Message(message) => on_event(AgentEvent::ToolProgress {
            name: name.to_string(),
            message,
        }),
        tools::ToolProgressEvent::PrepareForExternalOutput { ready } => {
            on_event(AgentEvent::PrepareForExternalOutput { ready })
        }
        tools::ToolProgressEvent::CommandOutput { stream, chunk } => {
            on_event(AgentEvent::CommandOutput {
                name: name.to_string(),
                stream,
                chunk,
            })
        }
    }
}

pub struct Agent {
    state: StateStore,
    client: OpenAiCompatibleClient,
    system_prompt: String,
    trim_at_ratio: f32,
    trim_batch_ratio: f32,
    tools_enabled: bool,
    max_tool_rounds: usize,
    tools: Arc<Mutex<ToolRegistry>>,
    memory: MemoryStore,
    mode: AgentMode,
    config: AppConfig,
    paths: MiyuPaths,
    on_overflow: String,
}

impl Agent {
    pub fn new(
        config: AppConfig,
        paths: &MiyuPaths,
        state: StateStore,
        client: OpenAiCompatibleClient,
        tools: ToolRegistry,
        mode: AgentMode,
    ) -> Result<Self> {
        let base_system_prompt = config.system_prompt(paths)?;
        if matches!(mode, AgentMode::Normal | AgentMode::Chat) {
            state.reset_if_prompt_changed(&base_system_prompt)?;
            state.recover_stale_turns()?;
        }
        let system_prompt = with_mode_reminder(base_system_prompt, mode);
        let tools_enabled = config.tools.enabled;
        let max_tool_rounds = config.tools.max_rounds;
        let memory = MemoryStore::new(&config, paths);
        memory.init()?;
        let on_overflow = config.context.on_overflow.clone();
        Ok(Self {
            state,
            client,
            system_prompt,
            trim_at_ratio: config.context.trim_at_ratio,
            trim_batch_ratio: config.context.trim_batch_ratio,
            tools_enabled,
            max_tool_rounds,
            tools: Arc::new(Mutex::new(tools)),
            memory,
            mode,
            config,
            paths: paths.clone(),
            on_overflow,
        })
    }

    pub fn prepare_for_turn(&mut self) -> Result<()> {
        let base_system_prompt = self.config.system_prompt(&self.paths)?;
        if matches!(self.mode, AgentMode::Normal | AgentMode::Chat) {
            self.state.reset_if_prompt_changed(&base_system_prompt)?;
            self.state.recover_stale_turns()?;
        }
        self.system_prompt = with_mode_reminder(base_system_prompt, self.mode);
        Ok(())
    }

    pub fn mode(&self) -> AgentMode {
        self.mode
    }

    pub fn context_window(&self) -> Option<usize> {
        self.client.context_window(&self.config).ok().flatten()
    }

    pub fn effective_context_tokens(&self) -> Result<u64> {
        let messages = self.chat_messages("", "")?;
        let mut tokens = overflow::estimate_messages_tokens(&messages) as u64;
        if self.tools_enabled {
            let loaded_tools = self.initial_loaded_tools(&messages)?;
            tokens = tokens.saturating_add(self.tool_definition_tokens(&loaded_tools) as u64);
        }
        Ok(tokens)
    }

    fn tool_definition_tokens(&self, loaded_tools: &BTreeSet<String>) -> usize {
        let tools = self.tools.lock().unwrap();
        let definitions = if tools::is_hybrid_loading_mode(&self.config.tools.loading_mode) {
            tools.lazy_definitions(loaded_tools)
        } else {
            tools.definitions()
        };
        estimate_tool_definition_tokens(&definitions)
    }

    fn trim_visible_context(&self) -> Result<Vec<crate::state::StoredConversationEntry>> {
        let Some(context_window) = self.context_window() else {
            return Ok(Vec::new());
        };
        let track_loaded_tool_sources = self.tools_enabled
            && self.config.tools.persist_loaded_tools
            && tools::is_hybrid_loading_mode(&self.config.tools.loading_mode);
        if track_loaded_tool_sources {
            self.effective_context_tokens()?;
        }
        let mut loaded_tool_sources = if track_loaded_tool_sources {
            Some(self.state.load_session_loaded_tools_with_sources()?)
        } else {
            None
        };
        let expected_loaded_tools = loaded_tool_sources.clone();
        let mut total = usize::try_from(self.effective_context_tokens()?).unwrap_or(usize::MAX);
        let trigger = (context_window as f32 * self.trim_at_ratio).max(1.0) as usize;
        if total < trigger {
            return Ok(Vec::new());
        }

        let target = (context_window as f32 * (1.0 - self.trim_batch_ratio)).max(1.0) as usize;
        let turns = self.state.load_visible_turns()?;
        let mut loaded_tool_tokens = loaded_tool_sources
            .as_ref()
            .map(|items| {
                self.tool_definition_tokens(
                    &items
                        .iter()
                        .map(|(name, _)| name.clone())
                        .collect::<BTreeSet<_>>(),
                )
            })
            .unwrap_or(0);
        let mut count = 0usize;
        for turn in turns
            .iter()
            .filter(|turn| !turn.is_summary && turn.status != crate::state::TurnStatus::Running)
        {
            if total <= target {
                break;
            }
            total = total.saturating_sub(turn_context_tokens(turn));
            if let Some(items) = loaded_tool_sources.as_mut() {
                items.retain(|(_, source)| source.as_deref() != Some(turn.turn_id.as_str()));
                let remaining = items
                    .iter()
                    .map(|(name, _)| name.clone())
                    .collect::<BTreeSet<_>>();
                let remaining_tokens = self.tool_definition_tokens(&remaining);
                if remaining_tokens <= loaded_tool_tokens {
                    total = total.saturating_sub(loaded_tool_tokens - remaining_tokens);
                } else {
                    total = total.saturating_add(remaining_tokens - loaded_tool_tokens);
                }
                loaded_tool_tokens = remaining_tokens;
            }
            count += 1;
        }
        let turns = self.state.oldest_evictable_visible_turns(count)?;
        archive_and_delete_visible_turns_checked(
            &self.state,
            &self.memory,
            &turns,
            expected_loaded_tools.as_deref(),
        )
    }

    pub fn switch_mode(&mut self, mode: AgentMode, tools: ToolRegistry) {
        self.mode = mode;
        self.tools = Arc::new(Mutex::new(tools));
    }

    pub fn replace_client(&mut self, client: OpenAiCompatibleClient) {
        self.client = client;
    }

    pub fn reload_config(
        &mut self,
        config: AppConfig,
        client: OpenAiCompatibleClient,
    ) -> Result<()> {
        self.config = config;
        self.client = client;
        self.tools_enabled = self.config.tools.enabled;
        self.max_tool_rounds = self.config.tools.max_rounds;
        self.trim_at_ratio = self.config.context.trim_at_ratio;
        self.trim_batch_ratio = self.config.context.trim_batch_ratio;
        self.on_overflow = self.config.context.on_overflow.clone();
        self.memory = MemoryStore::new(&self.config, &self.paths);
        self.memory.init()?;
        self.prepare_for_turn()
    }

    pub fn reset_memory(&mut self) -> Result<()> {
        self.memory = MemoryStore::new(&self.config, &self.paths);
        self.memory.init()?;
        Ok(())
    }

    pub async fn chat_stream<F>(&mut self, input: &str, on_event: F) -> Result<ChatResult>
    where
        F: FnMut(AgentEvent) -> Result<()>,
    {
        self.chat_stream_with_images(input, &[], on_event).await
    }

    pub async fn chat_stream_with_images<F>(
        &mut self,
        input: &str,
        images: &[Option<PastedImage>],
        on_event: F,
    ) -> Result<ChatResult>
    where
        F: FnMut(AgentEvent) -> Result<()>,
    {
        self.state.recover_stale_turns()?;
        self.trim_visible_context()?;
        let input = clean_user_visible_text(input);
        let binary_images: Vec<&ClipboardImage> = images
            .iter()
            .filter_map(|opt| match opt {
                Some(PastedImage::Binary(img)) => Some(img),
                _ => None,
            })
            .collect();
        let path_images: Vec<&str> = images
            .iter()
            .filter_map(|opt| match opt {
                Some(PastedImage::Path(p)) => Some(p.as_str()),
                _ => None,
            })
            .collect();
        let absolute_image_paths = resolve_pasted_image_paths(images, &self.paths);
        let temp_paths: Vec<String> = absolute_image_paths
            .iter()
            .filter_map(|path| path.clone())
            .collect();
        let input = rewrite_image_placeholders_with_paths(&input, &absolute_image_paths);
        let input = if !binary_images.is_empty() && !self.current_model_supports_vision() {
            self.describe_images_with_vision_provider(&input, &binary_images)
                .await?
        } else {
            input
        };
        let turn_id = format!(
            "turn_{}_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis())
                .unwrap_or(0),
            rand::random::<u16>()
        );
        self.state
            .start_turn(&turn_id, &input, std::process::id())?;
        let guard = PendingTurnGuard::new(self.state.clone(), turn_id.clone());
        let mut messages = self.chat_messages(&turn_id, &input)?;
        if !binary_images.is_empty() && self.current_model_supports_vision() {
            if let Some(last) = messages.last_mut() {
                if last.role == "user" {
                    let text = match &last.content {
                        Some(ChatContent::Text(t)) => t.clone(),
                        _ => String::new(),
                    };
                    let mut parts = vec![ChatContentPart::Text { text }];
                    for img in &binary_images {
                        parts.push(ChatContentPart::ImageUrl {
                            image_url: ImageUrlContent {
                                url: img.data_url(),
                            },
                        });
                    }
                    last.content = Some(ChatContent::Parts(parts));
                }
            }
        }
        if !temp_paths.is_empty() {
            let hint = if temp_paths.len() == 1 {
                format!(
                    "用户粘贴了 1 张剪贴板图片，已保存到临时文件：{}\n你可以使用 vision_analyze 工具对此图片进行更详细的分析。",
                    temp_paths[0]
                )
            } else {
                let list = temp_paths
                    .iter()
                    .enumerate()
                    .map(|(i, p)| format!("  [Image {}] {}", i + 1, p))
                    .collect::<Vec<_>>()
                    .join("\n");
                format!(
                    "用户粘贴了 {} 张剪贴板图片，已保存到临时文件：\n{}\n你可以使用 vision_analyze 工具对这些图片进行更详细的分析。",
                    temp_paths.len(),
                    list
                )
            };
            messages.push(ChatMessage::system(hint));
        }
        if !path_images.is_empty() {
            let list = path_images
                .iter()
                .enumerate()
                .map(|(i, p)| format!("  [Image {}] {}", i + 1, p))
                .collect::<Vec<_>>()
                .join("\n");
            let hint = format!(
                "用户粘贴了 {} 张本地图片路径：\n{}\n你可以使用 vision_analyze 工具读取并分析这些图片。",
                path_images.len(),
                list
            );
            messages.push(ChatMessage::system(hint));
        }
        if self.mode != AgentMode::Chat {
            if let Some(association) = self.memory.association(&input)? {
                messages.insert(
                    1,
                    ChatMessage::system(self.memory.format_association(&association)),
                );
            }
        }
        let mut on_event = on_event;
        if self.mode != AgentMode::Plan {
            if let Some(reminder) = memes::auto_meme_reminder(&self.config, &input) {
                messages.push(ChatMessage::system(reminder));
            }
        }
        let mut used_tools = Vec::new();
        let mut persisted_tool_reports = Vec::new();
        let result = self
            .chat_with_tools(
                &turn_id,
                &mut messages,
                &mut used_tools,
                &mut persisted_tool_reports,
                &mut on_event,
            )
            .await?;
        for (_, report) in persisted_tool_reports {
            self.state.append_persisted_context(&turn_id, &report)?;
        }
        let token_total = result.usage.as_ref().map(Usage::effective_total_tokens);
        guard.complete(
            &result.content,
            result.reasoning.as_deref(),
            token_total,
            result.usage_estimated,
        )?;
        self.memory.process_after_turn(&input, &result.content)?;
        if let Some(usage) = result.usage.clone() {
            self.state.add_usage(&usage)?;
        }
        Ok(result)
    }

    pub async fn handle_overflow_after_turn<F>(
        &self,
        context_tokens: u64,
        on_event: F,
    ) -> Result<Option<ChatResult>>
    where
        F: FnMut(AgentEvent) -> Result<()>,
    {
        let mut on_event = on_event;
        let Some(compact) = self.handle_overflow(context_tokens, &mut on_event).await? else {
            return Ok(None);
        };
        self.state.add_auxiliary_usage(&compact.usage)?;
        Ok(Some(ChatResult {
            content: String::new(),
            reasoning: None,
            usage: Some(compact.usage),
            usage_estimated: compact.usage_estimated,
            tool_calls: Vec::new(),
            provider_id: None,
            model: None,
        }))
    }

    pub async fn compact_now<F>(&self, on_event: F) -> Result<Option<ChatResult>>
    where
        F: FnMut(AgentEvent) -> Result<()>,
    {
        let mut on_event = on_event;
        let context_window = self.context_window().or_else(|| {
            if crate::models_cache::is_loaded() {
                return None;
            }
            crate::models_cache::refresh_blocking(&self.paths).ok()?;
            self.context_window()
        });
        let Some(context_window) = context_window else {
            let missing = self.client.models_without_context_window(&self.config);
            if missing.is_empty() {
                bail!("当前模型的上下文窗口尚未加载或未配置，无法压缩上下文");
            }
            bail!(
                "以下活动模型的上下文窗口尚未加载或未配置，无法压缩上下文：{}",
                missing.join(", ")
            );
        };
        let visible_count = self.state.load_visible_turns()?.len();
        if visible_count == 0 {
            return Ok(None);
        }
        let check = overflow::OverflowCheck::new(Some(context_window), self.trim_at_ratio, None);
        on_event(AgentEvent::CompactStart)?;
        let compactor = compact::Compactor::new(
            self.client.clone(),
            self.state.clone(),
            context_window,
            check.reserved_tokens,
        );
        let mut on_chunk = |chunk: ChatStreamChunk| on_event(AgentEvent::CompactChunk(chunk));
        let compact = match compactor.perform_compact(&mut on_chunk).await {
            Ok(result) => {
                on_event(AgentEvent::CompactEnd)?;
                result
            }
            Err(err) => {
                on_event(AgentEvent::CompactEnd)?;
                return Err(err);
            }
        };
        let Some(compact) = compact else {
            return Ok(None);
        };
        self.state.add_auxiliary_usage(&compact.usage)?;
        Ok(Some(ChatResult {
            content: String::new(),
            reasoning: None,
            usage: Some(compact.usage),
            usage_estimated: compact.usage_estimated,
            tool_calls: Vec::new(),
            provider_id: None,
            model: None,
        }))
    }

    async fn handle_overflow<F>(
        &self,
        context_tokens: u64,
        on_event: &mut F,
    ) -> Result<Option<compact::CompactResult>>
    where
        F: FnMut(AgentEvent) -> Result<()>,
    {
        let context_window = self.context_window();
        let check = overflow::OverflowCheck::new(context_window, self.trim_at_ratio, None);
        let context_tokens = usize::try_from(context_tokens).unwrap_or(usize::MAX);
        if !check.is_enabled() || !check.check_tokens(context_tokens) {
            return Ok(None);
        }
        let compact_result = match self.on_overflow.as_str() {
            "compact" => {
                let visible_count = self.state.load_visible_turns()?.len();
                if visible_count == 0 {
                    return Ok(None);
                }
                on_event(AgentEvent::CompactStart)?;
                let compactor = compact::Compactor::new(
                    self.client.clone(),
                    self.state.clone(),
                    context_window.unwrap(),
                    check.reserved_tokens,
                );
                let mut on_chunk =
                    |chunk: ChatStreamChunk| on_event(AgentEvent::CompactChunk(chunk));
                match compactor.perform_compact(&mut on_chunk).await {
                    Ok(result) => {
                        on_event(AgentEvent::CompactEnd)?;
                        result
                    }
                    Err(e) => {
                        on_event(AgentEvent::CompactEnd)?;
                        return Err(e);
                    }
                }
            }
            "pop" => {
                on_event(AgentEvent::PopStart)?;
                self.trim_visible_context()?;
                on_event(AgentEvent::PopEnd)?;
                None
            }
            _ => None,
        };
        Ok(compact_result)
    }

    fn current_model_supports_vision(&self) -> bool {
        let provider = match self.config.provider(None) {
            Ok(p) => p,
            Err(_) => return false,
        };
        match provider.supports_vision(&provider.default_model) {
            Some(true) => true,
            _ => false,
        }
    }

    async fn describe_images_with_vision_provider(
        &self,
        input: &str,
        images: &[&ClipboardImage],
    ) -> Result<String> {
        let vision_cfg = &self.config.plugins.vision;
        if !vision_cfg.enabled {
            return Ok(input.to_string());
        }
        let mut descriptions = Vec::new();
        for (i, img) in images.iter().enumerate() {
            let prompt = if input.trim().is_empty() {
                "请简洁描述这张图片，并指出重要细节。".to_string()
            } else {
                format!("用户消息：{input}\n\n请基于图片内容回答或描述图片，不要编造看不见的信息。")
            };
            match vision::analyze_image_url_with_prompt(
                &self.config,
                &self.paths,
                &img.data_url(),
                &prompt,
            )
            .await
            {
                Ok(desc) => {
                    descriptions.push(format!("[Image {} 的描述]\n{}", i + 1, desc.trim()));
                }
                Err(e) => {
                    descriptions.push(format!("[Image {} 识图失败: {}]", i + 1, e));
                }
            }
        }
        let combined = descriptions.join("\n\n");
        if input.trim().is_empty() {
            Ok(combined)
        } else {
            Ok(format!("{input}\n\n{combined}"))
        }
    }

    async fn chat_with_tools<F>(
        &self,
        current_turn_id: &str,
        messages: &mut Vec<ChatMessage>,
        used_tools: &mut Vec<String>,
        persisted_tool_reports: &mut Vec<(String, String)>,
        on_event: &mut F,
    ) -> Result<ChatResult>
    where
        F: FnMut(AgentEvent) -> Result<()>,
    {
        let mut tool_round = 0usize;
        let mut question_rounds = 0usize;
        let mut loaded_tools = self.initial_loaded_tools(messages)?;
        let mut usage_accumulator = UsageAccumulator::default();
        loop {
            let tool_limit_reached = self.max_tool_rounds > 0 && tool_round >= self.max_tool_rounds;

            if self.mode == AgentMode::Normal {
                let mut tools = self.tools.lock().unwrap();
                tools::rescan_scripts(&mut tools, &self.paths);
                tools::register_script_display_names(&tools);
            }

            let definitions = if self.tools_enabled && !tool_limit_reached {
                let tools = self.tools.lock().unwrap();
                if tools::is_hybrid_loading_mode(&self.config.tools.loading_mode) {
                    tools.lazy_definitions(&loaded_tools)
                } else {
                    tools.definitions()
                }
            } else {
                Vec::new()
            };

            on_event(AgentEvent::ReasoningStart {
                received_at: Instant::now(),
            })?;
            let (chunk_tx, mut chunk_rx) =
                tokio::sync::mpsc::unbounded_channel::<(ChatStreamChunk, Instant)>();
            let request_messages = messages.clone();
            let llm_future =
                self.client
                    .chat_stream(request_messages.clone(), definitions, move |chunk| {
                        let _ = chunk_tx.send((chunk, Instant::now()));
                        Ok(())
                    });
            tokio::pin!(llm_future);
            let mut spinner_interval = tokio::time::interval(SPINNER_INTERVAL);
            spinner_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            spinner_interval.tick().await;
            let mut reasoning_filter = ReasoningTitleFilter::default();
            let result = loop {
                tokio::select! {
                    result = &mut llm_future => {
                        break result?;
                    }
                    Some((chunk, received_at)) = chunk_rx.recv() => {
                        emit_filtered_chunk_at(chunk, received_at, &mut reasoning_filter, on_event)?;
                    }
                    _ = spinner_interval.tick() => {
                        on_event(AgentEvent::SpinnerTick)?;
                    }
                }
            };
            while let Ok((chunk, received_at)) = chunk_rx.try_recv() {
                emit_filtered_chunk_at(chunk, received_at, &mut reasoning_filter, on_event)?;
            }
            let (title, text) = reasoning_filter.finish();
            if let Some(title) = title {
                on_event(AgentEvent::ReasoningTitle(title))?;
            }
            if let Some(text) = text {
                on_event(AgentEvent::Chunk(ChatStreamChunk {
                    kind: ChatStreamKind::Reasoning,
                    text,
                }))?;
            }
            usage_accumulator.add_result(&result, &request_messages);
            if result.tool_calls.is_empty() || !self.tools_enabled {
                let mut result = result;
                if let Some(usage) = usage_accumulator.usage() {
                    result.usage = Some(usage);
                    result.usage_estimated = usage_accumulator.estimated;
                }
                return Ok(result);
            }
            if tool_limit_reached {
                let mut result = result;
                let warning = format!(
                    "工具调用已达到上限 {} 轮，未执行后续工具调用。可将 `tools.max_rounds` 设为 0 以允许无限工具调用。",
                    self.max_tool_rounds
                );
                let warning_chunk = if result.content.trim().is_empty() {
                    warning.clone()
                } else {
                    format!("\n\n{warning}")
                };
                result.content.push_str(&warning_chunk);
                on_event(AgentEvent::Chunk(ChatStreamChunk {
                    kind: ChatStreamKind::Content,
                    text: warning_chunk,
                }))?;
                result.tool_calls.clear();
                if let Some(usage) = usage_accumulator.usage() {
                    result.usage = Some(usage);
                    result.usage_estimated = usage_accumulator.estimated;
                }
                return Ok(result);
            }
            tool_round += 1;
            messages.push(ChatMessage::assistant(
                result.content.clone(),
                Some(result.tool_calls.clone()),
            ));
            let ask_question_enabled = self
                .tools
                .lock()
                .unwrap()
                .tool_names()
                .iter()
                .any(|name| name == "ask_question");
            let question_call_count = result
                .tool_calls
                .iter()
                .filter(|call| ask_question_enabled && call.function.name == "ask_question")
                .count();
            if question_call_count == 1 {
                question_rounds += 1;
            }
            let question_round_allowed =
                question_call_count == 1 && question_rounds <= MAX_QUESTION_ROUNDS_PER_TURN;
            let defer_sibling_tools = question_call_count == 1 && result.tool_calls.len() > 1;
            for call in result.tool_calls {
                let event_name = tool_event_name(&call.function.name, &call.function.arguments);
                on_event(AgentEvent::ToolCall {
                    name: event_name.clone(),
                    arguments: call.function.arguments.clone(),
                })?;
                if question_call_count > 1 {
                    let output = "tool error: only one ask_question call is allowed per tool batch; combine all questions into one call".to_string();
                    on_event(AgentEvent::ToolResult {
                        name: event_name.clone(),
                        ok: false,
                        output: output.clone(),
                    })?;
                    messages.push(ChatMessage::tool(call.id, output));
                    continue;
                }
                if defer_sibling_tools && call.function.name != "ask_question" {
                    let output = "tool error: deferred until the user answers ask_question; reissue this tool call after receiving the answer".to_string();
                    on_event(AgentEvent::ToolResult {
                        name: event_name.clone(),
                        ok: false,
                        output: output.clone(),
                    })?;
                    messages.push(ChatMessage::tool(call.id, output));
                    continue;
                }
                if ask_question_enabled && call.function.name == "ask_question" {
                    if !question_round_allowed {
                        let output = format!(
                            "tool error: ask_question exceeded the per-turn limit of {MAX_QUESTION_ROUNDS_PER_TURN}"
                        );
                        on_event(AgentEvent::ToolResult {
                            name: event_name.clone(),
                            ok: false,
                            output: output.clone(),
                        })?;
                        messages.push(ChatMessage::tool(call.id, output));
                        continue;
                    }
                    let request = match QuestionRequest::parse(&call.function.arguments) {
                        Ok(request) => request,
                        Err(err) => {
                            let output = format!("tool error: invalid ask_question request: {err}");
                            on_event(AgentEvent::ToolResult {
                                name: event_name.clone(),
                                ok: false,
                                output: output.clone(),
                            })?;
                            messages.push(ChatMessage::tool(call.id, output));
                            continue;
                        }
                    };
                    let (response_tx, response_rx) = oneshot::channel();
                    on_event(AgentEvent::AskQuestion {
                        request: request.clone(),
                        responder: response_tx,
                    })?;
                    let response = response_rx.await.unwrap_or(QuestionResponse::Cancelled);
                    let output = match response {
                        QuestionResponse::Answered(answers) => {
                            let exchange = QuestionExchange::new(request, answers)?;
                            self.state
                                .append_question_exchange(current_turn_id, &exchange)?;
                            answered_tool_output(&exchange)
                        }
                        QuestionResponse::Cancelled => return Err(QuestionCancelled.into()),
                        QuestionResponse::Unavailable(reason) => unavailable_tool_output(&reason),
                    };
                    messages.push(ChatMessage::tool(call.id, output.clone()));
                    on_event(AgentEvent::ToolResult {
                        name: event_name,
                        ok: true,
                        output,
                    })?;
                    continue;
                }
                used_tools.push(call.function.name.clone());
                {
                    let tools = self.tools.lock().unwrap();
                    if matches!(self.mode, AgentMode::Plan | AgentMode::Chat)
                        && tools.permission(&call.function.name)? != ToolPermission::ReadOnly
                    {
                        bail!(
                            "{} mode blocked non-read-only tool: {}",
                            self.mode.label(),
                            call.function.name
                        );
                    }
                    if tools::is_hybrid_loading_mode(&self.config.tools.loading_mode)
                        && call.function.name != "load_tools"
                        && tools.requires_lazy_load(&call.function.name, &loaded_tools)
                    {
                        if tools.can_auto_load_direct_call(&call.function.name) {
                            loaded_tools.insert(call.function.name.clone());
                            if self.config.tools.persist_loaded_tools {
                                self.state.add_session_loaded_tools(
                                    &[call.function.name.clone()],
                                    Some(current_turn_id),
                                )?;
                            }
                        } else {
                            let output = format!(
                                "tool error: 工具 `{}` 尚未加载。请先调用 load_tools，参数为 {{\"names\":[\"{}\"]}}。",
                                call.function.name,
                                call.function.name,
                            );
                            on_event(AgentEvent::ToolResult {
                                name: event_name.clone(),
                                ok: false,
                                output: output.clone(),
                            })?;
                            messages.push(ChatMessage::tool(call.id, output));
                            continue;
                        }
                    }
                }
                if call.function.name == "install_aur_package"
                    && used_tools.iter().any(|name| name == "review_aur_package")
                {
                    let output = "tool error: install_aur_package cannot run in the same turn as review_aur_package; ask the user to confirm installation first".to_string();
                    on_event(AgentEvent::ToolResult {
                        name: event_name.clone(),
                        ok: false,
                        output: output.clone(),
                    })?;
                    messages.push(ChatMessage::tool(call.id, output));
                    continue;
                }
                let (progress_tx, mut progress_rx) = mpsc::unbounded_channel();
                let tool_future = {
                    let tools = self.tools.lock().unwrap();
                    tools.call_with_progress_future(
                        &call.function.name,
                        &call.function.arguments,
                        progress_tx,
                    )
                };
                let tool_future = match tool_future {
                    Ok(f) => f,
                    Err(err) => {
                        let output = format!("tool error: {err}");
                        on_event(AgentEvent::ToolResult {
                            name: event_name.clone(),
                            ok: false,
                            output: output.clone(),
                        })?;
                        messages.push(ChatMessage::tool(call.id, output));
                        continue;
                    }
                };
                tokio::pin!(tool_future);
                let mut spinner_interval = tokio::time::interval(SPINNER_INTERVAL);
                spinner_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                spinner_interval.tick().await;
                let (output, tool_succeeded) = loop {
                    tokio::select! {
                        result = &mut tool_future => {
                            break match result {
                                Ok(output) => {
                                    while let Ok(progress) = progress_rx.try_recv() {
                                        emit_tool_progress(on_event, &event_name, progress)?;
                                    }
                                    (output, true)
                                }
                                Err(err) => {
                                    while let Ok(progress) = progress_rx.try_recv() {
                                        emit_tool_progress(on_event, &event_name, progress)?;
                                    }
                                    on_event(AgentEvent::ToolResult {
                                        name: event_name.clone(),
                                        ok: false,
                                        output: format!("tool error: {err}"),
                                    })?;
                                    (format!("tool error: {err}"), false)
                                }
                            };
                        }
                        Some(progress) = progress_rx.recv() => {
                            emit_tool_progress(on_event, &event_name, progress)?;
                        }
                        _ = spinner_interval.tick() => {
                            on_event(AgentEvent::SpinnerTick)?;
                        }
                    }
                };
                let clipboard_image = if tool_succeeded {
                    clipboard_binary_image_from_tool_result(&call.function.name, &output)
                } else {
                    None
                };
                messages.push(ChatMessage::tool(call.id, output.clone()));
                if tool_succeeded && call.function.name == "load_tools" {
                    let loaded = loaded_items_from_output(&output);
                    for name in &loaded.tools {
                        loaded_tools.insert(name.clone());
                    }
                    if self.config.tools.persist_loaded_tools {
                        self.state
                            .add_session_loaded_tools(&loaded.tools, Some(current_turn_id))?;
                        self.state
                            .add_session_loaded_targets(&loaded.targets, Some(current_turn_id))?;
                    }
                }
                if let Some(img) = clipboard_image {
                    let supports_vision = self.current_model_supports_vision();
                    let uses_vision_fallback =
                        !supports_vision && self.config.plugins.vision.enabled;
                    if !supports_vision {
                        let message = if self.config.plugins.vision.enabled {
                            if crate::i18n::is_zh() {
                                "视觉分析."
                            } else {
                                "Vision analysis."
                            }
                        } else if crate::i18n::is_zh() {
                            "当前模型不支持图片，且未启用视觉模型，无法分析剪贴板图片。"
                        } else {
                            "The current model does not support images and the vision plugin is disabled, so the clipboard image cannot be analyzed."
                        };
                        on_event(AgentEvent::ToolProgress {
                            name: event_name.clone(),
                            message: message.to_string(),
                        })?;
                    }
                    let image_message = if uses_vision_fallback {
                        let image_future = self.clipboard_image_message(img);
                        tokio::pin!(image_future);
                        let mut spinner_interval = tokio::time::interval(SPINNER_INTERVAL);
                        spinner_interval
                            .set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                        spinner_interval.tick().await;
                        let mut progress_interval =
                            tokio::time::interval(Duration::from_millis(900));
                        progress_interval
                            .set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                        progress_interval.tick().await;
                        let mut progress_tick = 0usize;
                        loop {
                            tokio::select! {
                                result = &mut image_future => {
                                    break result?;
                                }
                                _ = progress_interval.tick() => {
                                    progress_tick = progress_tick.wrapping_add(1);
                                    on_event(AgentEvent::ToolProgress {
                                        name: event_name.clone(),
                                        message: vision_analysis_progress(progress_tick),
                                    })?;
                                }
                                _ = spinner_interval.tick() => {
                                    on_event(AgentEvent::SpinnerTick)?;
                                }
                            }
                        }
                    } else {
                        self.clipboard_image_message(img).await?
                    };
                    if let Some(message) = image_message {
                        messages.push(message);
                    }
                }
                if tool_succeeded {
                    let result_ok = if call.function.name == "run_command" {
                        serde_json::from_str::<serde_json::Value>(&output)
                            .ok()
                            .and_then(|v| v.get("success").and_then(serde_json::Value::as_bool))
                            .unwrap_or(true)
                    } else {
                        true
                    };
                    on_event(AgentEvent::ToolResult {
                        name: event_name.clone(),
                        ok: result_ok,
                        output: output.clone(),
                    })?;
                    if let Some(report) =
                        extract_persistable_tool_report(&call.function.name, &output)
                    {
                        persisted_tool_reports.push((call.function.name.clone(), report));
                    }
                }
            }
            if question_round_allowed {
                tool_round = tool_round.saturating_sub(1);
            }
        }
    }

    fn initial_loaded_tools(&self, messages: &[ChatMessage]) -> Result<BTreeSet<String>> {
        if !self.config.tools.persist_loaded_tools {
            return Ok(BTreeSet::new());
        }
        let mut loaded = self.state.load_session_loaded_tools()?;
        if loaded.is_empty() {
            loaded = loaded_tools_from_messages(messages);
            if !loaded.is_empty() {
                let names = loaded.iter().cloned().collect::<Vec<_>>();
                self.state.add_session_loaded_tools(&names, None)?;
            }
        }
        if !loaded.is_empty() {
            let tools = self.tools.lock().unwrap();
            let available = tools.tool_names().into_iter().collect::<BTreeSet<_>>();
            loaded.retain(|name| available.contains(name));
        }
        Ok(loaded)
    }

    async fn clipboard_image_message(&self, img: ClipboardImage) -> Result<Option<ChatMessage>> {
        if self.current_model_supports_vision() {
            return Ok(Some(ChatMessage {
                role: "user".to_string(),
                content: Some(ChatContent::Parts(vec![ChatContentPart::ImageUrl {
                    image_url: ImageUrlContent {
                        url: img.data_url(),
                    },
                }])),
                tool_call_id: None,
                tool_calls: None,
            }));
        }

        let images = vec![&img];
        let description = self
            .describe_images_with_vision_provider("", &images)
            .await?;
        if description.trim().is_empty() {
            return Ok(None);
        }
        Ok(Some(ChatMessage::plain("user", description)))
    }

    fn chat_messages(
        &self,
        current_turn_id: &str,
        current_input: &str,
    ) -> Result<Vec<ChatMessage>> {
        let mut messages = vec![ChatMessage::system(self.system_prompt.clone())];
        if let Some(summary) = self.state.load_last_summary()? {
            messages.push(ChatMessage::system(format!(
                "<conversation-summary>\n{}\n</conversation-summary>",
                summary.assistant_content
            )));
        }
        let turns = self.state.load_visible_turns_excluding(current_turn_id)?;
        for turn in &turns {
            if turn.is_summary {
                continue;
            }
            messages.push(ChatMessage::plain("user", &turn.user_content));
            for exchange in &turn.question_exchanges {
                messages.push(ChatMessage::plain(
                    "assistant",
                    crate::question::assistant_exchange_text(exchange),
                ));
                messages.push(ChatMessage::plain(
                    "user",
                    crate::question::user_exchange_text(exchange),
                ));
            }
            messages.push(ChatMessage::plain("assistant", &turn.assistant_content));
            if !turn.tool_reports.is_empty() {
                messages.push(ChatMessage::system(private_tool_memory(&turn.tool_reports)));
            }
        }
        messages.push(ChatMessage::system(runtime_context(self.mode)));
        messages.push(ChatMessage::plain("user", current_input));
        Ok(messages)
    }
}

#[derive(Default)]
struct UsageAccumulator {
    prompt_tokens: u64,
    completion_tokens: u64,
    total_tokens: u64,
    has_usage: bool,
    estimated: bool,
}

impl UsageAccumulator {
    fn add_result(&mut self, result: &ChatResult, request_messages: &[ChatMessage]) {
        if let Some(usage) = &result.usage {
            self.add_usage(usage, false);
            return;
        }

        let prompt_tokens = overflow::estimate_messages_tokens(request_messages) as u64;
        let completion_tokens = estimate_result_tokens(result) as u64;
        self.add_usage(
            &Usage {
                prompt_tokens,
                completion_tokens,
                total_tokens: prompt_tokens.saturating_add(completion_tokens),
            },
            true,
        );
    }

    fn add_usage(&mut self, usage: &Usage, estimated: bool) {
        self.prompt_tokens = self.prompt_tokens.saturating_add(usage.prompt_tokens);
        self.completion_tokens = self
            .completion_tokens
            .saturating_add(usage.completion_tokens);
        let total = usage.effective_total_tokens();
        self.total_tokens = self.total_tokens.saturating_add(total);
        self.has_usage = true;
        self.estimated |= estimated;
    }

    fn usage(&self) -> Option<Usage> {
        self.has_usage.then_some(Usage {
            prompt_tokens: self.prompt_tokens,
            completion_tokens: self.completion_tokens,
            total_tokens: self.total_tokens,
        })
    }
}

fn estimate_result_tokens(result: &ChatResult) -> usize {
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

fn estimate_tool_definition_tokens(definitions: &[crate::llm::ToolDefinition]) -> usize {
    definitions
        .iter()
        .filter_map(|definition| serde_json::to_string(definition).ok())
        .map(|text| crate::token_estimate::estimate_tokens(&text))
        .sum()
}

fn extract_persistable_tool_report(tool_name: &str, output: &str) -> Option<String> {
    let field = match tool_name {
        "load_tools" => {
            return compact_loaded_tools_report(output)
                .map(|report| wrap_previous_tool_report(tool_name, &report))
        }
        "show_meme" => return compact_sent_meme_report(output),
        "remember_fact" => {
            return compact_remembered_fact_report(output)
                .map(|report| wrap_previous_tool_report(tool_name, &report))
        }
        "deep_research_linux_game_compatibility" => "final_report",
        "linux_input_method_diagnose" | "deep_diagnose" | "deep_research" => "final_answer",
        "task" => "result",
        _ => return None,
    };
    serde_json::from_str::<serde_json::Value>(output)
        .ok()
        .and_then(|value| {
            value
                .get(field)
                .and_then(serde_json::Value::as_str)
                .map(str::trim)
                .map(str::to_string)
        })
        .map(|report| wrap_previous_tool_report(tool_name, &report))
        .filter(|report| !report.is_empty())
}

fn wrap_previous_tool_report(tool_name: &str, report: &str) -> String {
    format!(
        "<previous_tool_report name=\"{tool_name}\">\n{}\n</previous_tool_report>",
        report.trim()
    )
}

fn private_tool_memory(reports: &[String]) -> String {
    format!(
        "<system-reminder>\n<private_tool_memory>\n这些是内部工具记忆，仅用于保持对话连续性。不要向用户复述、展示或引用这些标签。\n{}\n</private_tool_memory>\n</system-reminder>",
        reports
            .iter()
            .map(|report| report.trim())
            .filter(|report| !report.is_empty())
            .collect::<Vec<_>>()
            .join("\n")
    )
}

fn turn_context_tokens(turn: &crate::state::Turn) -> usize {
    let mut messages = vec![ChatMessage::plain("user", &turn.user_content)];
    for exchange in &turn.question_exchanges {
        messages.push(ChatMessage::plain(
            "assistant",
            crate::question::assistant_exchange_text(exchange),
        ));
        messages.push(ChatMessage::plain(
            "user",
            crate::question::user_exchange_text(exchange),
        ));
    }
    messages.push(ChatMessage::plain("assistant", &turn.assistant_content));
    if !turn.tool_reports.is_empty() {
        messages.push(ChatMessage::system(private_tool_memory(&turn.tool_reports)));
    }
    overflow::estimate_messages_tokens(&messages)
}

fn evicted_turn_entries(
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

fn archive_and_delete_visible_turns_checked(
    state: &StateStore,
    memory: &MemoryStore,
    turns: &[crate::state::Turn],
    expected_loaded_tools: Option<&[(String, Option<String>)]>,
) -> Result<Vec<crate::state::StoredConversationEntry>> {
    let (entries, evicted) = evicted_turn_entries(turns);
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

fn compact_remembered_fact_report(output: &str) -> Option<String> {
    let value = serde_json::from_str::<Value>(output).ok()?;
    if value.get("ok").and_then(Value::as_bool) != Some(true) {
        return None;
    }
    let content = value.get("content").and_then(Value::as_str)?.trim();
    if content.is_empty() {
        return None;
    }
    let mut report = serde_json::json!({
        "remembered_fact": {
            "content": content,
        }
    });
    if let Some(id) = value.get("id").and_then(Value::as_i64) {
        report["remembered_fact"]["id"] = serde_json::json!(id);
    }
    if let Some(source) = value.get("source").and_then(Value::as_str) {
        let source = source.trim();
        if !source.is_empty() {
            report["remembered_fact"]["source"] = serde_json::json!(source);
        }
    }
    serde_json::to_string(&report).ok()
}

fn compact_loaded_tools_report(output: &str) -> Option<String> {
    let value = serde_json::from_str::<Value>(output).ok()?;
    let names = value
        .get("loaded_tools")
        .and_then(Value::as_array)?
        .iter()
        .filter_map(|item| {
            item.as_str()
                .or_else(|| item.get("name").and_then(Value::as_str))
        })
        .map(str::to_string)
        .collect::<Vec<_>>();
    if names.is_empty() {
        return None;
    }
    serde_json::to_string(&serde_json::json!({ "loaded_tools": names })).ok()
}

#[derive(Default)]
struct LoadedItems {
    targets: Vec<String>,
    tools: Vec<String>,
}

fn loaded_items_from_output(output: &str) -> LoadedItems {
    let Ok(value) = serde_json::from_str::<Value>(output) else {
        return LoadedItems::default();
    };
    let targets = value
        .get("loaded_targets")
        .and_then(Value::as_array)
        .map(|items| string_array_items(items))
        .unwrap_or_default();
    let tools = value
        .get("loaded_tools")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| {
                    item.as_str()
                        .or_else(|| item.get("name").and_then(Value::as_str))
                })
                .map(str::trim)
                .filter(|name| !name.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    LoadedItems { targets, tools }
}

fn string_array_items(items: &[Value]) -> Vec<String> {
    items
        .iter()
        .filter_map(Value::as_str)
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(str::to_string)
        .collect()
}

fn compact_sent_meme_report(output: &str) -> Option<String> {
    const MAX_DESCRIPTION_CHARS: usize = 120;

    let value = serde_json::from_str::<Value>(output).ok()?;
    if value.get("success").and_then(Value::as_bool) != Some(true) {
        return None;
    }
    let id = value.get("id").and_then(Value::as_str)?.trim();
    if id.is_empty() {
        return None;
    }
    let description = value
        .get("description")
        .and_then(Value::as_str)
        .map(compact_one_line)
        .filter(|description| !description.is_empty())
        .map(|description| truncate_chars(&description, MAX_DESCRIPTION_CHARS));
    let id = xml_text_escape(id);
    match description {
        Some(description) => Some(format!(
            "<sent_meme>发送了一个表情包：id={}；description={}</sent_meme>",
            id,
            xml_text_escape(&description)
        )),
        None => Some(format!("<sent_meme>发送了一个表情包：id={id}</sent_meme>")),
    }
}

fn compact_one_line(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn truncate_chars(value: &str, max_chars: usize) -> String {
    let mut output = String::new();
    for (index, ch) in value.chars().enumerate() {
        if index >= max_chars {
            output.push('…');
            return output;
        }
        output.push(ch);
    }
    output
}

fn xml_text_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn loaded_tools_from_messages(messages: &[ChatMessage]) -> BTreeSet<String> {
    let mut loaded = BTreeSet::new();
    for message in messages {
        let Some(ChatContent::Text(text)) = message.content.as_ref() else {
            continue;
        };
        collect_loaded_tools_from_text(text, &mut loaded);
    }
    loaded
}

fn collect_loaded_tools_from_text(text: &str, loaded: &mut BTreeSet<String>) {
    let mut rest = text;
    let start_tag = "<previous_tool_report name=\"load_tools\">";
    let end_tag = "</previous_tool_report>";
    while let Some(start) = rest.find(start_tag) {
        let body_start = start + start_tag.len();
        let Some(end) = rest[body_start..].find(end_tag) else {
            break;
        };
        let body = &rest[body_start..body_start + end];
        if let Ok(value) = serde_json::from_str::<Value>(body.trim()) {
            if let Some(names) = value.get("loaded_tools").and_then(Value::as_array) {
                for name in names.iter().filter_map(Value::as_str) {
                    if !name.trim().is_empty() {
                        loaded.insert(name.trim().to_string());
                    }
                }
            }
        }
        rest = &rest[body_start + end + end_tag.len()..];
    }
}

fn tool_event_name(name: &str, arguments: &str) -> String {
    let Ok(args) = serde_json::from_str::<Value>(arguments) else {
        return name.to_string();
    };
    match name {
        "load_skill" => args
            .get("name")
            .and_then(Value::as_str)
            .map(|skill| format!("load_skill:{skill}"))
            .unwrap_or_else(|| name.to_string()),
        "load_tools" => args
            .get("names")
            .and_then(Value::as_array)
            .map(|names| {
                names
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .collect::<Vec<_>>()
                    .join(",")
            })
            .filter(|tools| !tools.is_empty())
            .map(|tools| format!("load_tools:{tools}"))
            .unwrap_or_else(|| name.to_string()),
        _ => name.to_string(),
    }
}

fn clipboard_binary_image_from_tool_result(
    tool_name: &str,
    output: &str,
) -> Option<ClipboardImage> {
    if tool_name != "read_clipboard" {
        return None;
    }
    let value = serde_json::from_str::<Value>(output).ok()?;
    if value.get("ok").and_then(Value::as_bool) != Some(true) {
        return None;
    }
    if value.get("kind").and_then(Value::as_str) != Some("clipboard") {
        return None;
    }
    if value.get("content_type").and_then(Value::as_str) != Some("image") {
        return None;
    }
    if value.get("source").and_then(Value::as_str) != Some("clipboard_binary") {
        return None;
    }
    let path = value.get("path").and_then(Value::as_str)?;
    let mime = value
        .get("mime")
        .and_then(Value::as_str)
        .unwrap_or("image/png")
        .to_string();
    let data = std::fs::read(path).ok()?;
    Some(ClipboardImage::new(mime, data))
}

fn resolve_pasted_image_paths(
    images: &[Option<PastedImage>],
    paths: &MiyuPaths,
) -> Vec<Option<String>> {
    images
        .iter()
        .enumerate()
        .map(|(i, image)| match image {
            Some(PastedImage::Binary(img)) => img
                .write_temp_file(&paths.cache_dir, i + 1)
                .ok()
                .map(|path| path.display().to_string()),
            Some(PastedImage::Path(path)) => Some(path.clone()),
            None => None,
        })
        .collect()
}

fn rewrite_image_placeholders_with_paths(input: &str, paths: &[Option<String>]) -> String {
    let mut output = String::new();
    let mut rest = input;
    while let Some(start) = rest.find("[Image ") {
        output.push_str(&rest[..start]);
        let after_start = &rest[start..];
        let Some(end) = after_start.find(']') else {
            output.push_str(after_start);
            return output;
        };
        let placeholder = &after_start[..=end];
        if let Some(index) = image_placeholder_index(placeholder) {
            if let Some(Some(path)) = paths.get(index - 1) {
                output.push_str(&format!("[Image {index}: {path}]"));
            } else {
                output.push_str(placeholder);
            }
        } else {
            output.push_str(placeholder);
        }
        rest = &after_start[end + 1..];
    }
    output.push_str(rest);
    output
}

fn image_placeholder_index(placeholder: &str) -> Option<usize> {
    let inner = placeholder
        .strip_prefix("[Image ")?
        .strip_suffix(']')?
        .trim_start();
    let num: String = inner.chars().take_while(|c| c.is_ascii_digit()).collect();
    let index = num.parse::<usize>().ok()?;
    (index > 0).then_some(index)
}

fn vision_analysis_progress(tick: usize) -> String {
    let dots = match tick % 3 {
        1 => ".",
        2 => "..",
        _ => "...",
    };
    if crate::i18n::is_zh() {
        format!("视觉分析{dots}")
    } else {
        format!("Vision analysis{dots}")
    }
}

fn with_mode_reminder(system_prompt: String, mode: AgentMode) -> String {
    let mut prompt = system_prompt;
    if let Some(reminder) = mode.reminder() {
        prompt.push_str("\n\n");
        prompt.push_str(reminder);
    }
    prompt
}

#[derive(Default)]
struct ReasoningTitleFilter {
    pending: String,
    decided: bool,
    trim_body_prefix: bool,
}

impl ReasoningTitleFilter {
    fn push(&mut self, text: &str) -> (Option<String>, Option<String>) {
        if self.decided {
            let text = if self.trim_body_prefix {
                let text = text.trim_start_matches(['\r', '\n']);
                if text.is_empty() {
                    return (None, None);
                }
                self.trim_body_prefix = false;
                text
            } else {
                text
            };
            return (None, (!text.is_empty()).then(|| text.to_string()));
        }
        self.pending.push_str(text);
        let trimmed = self.pending.trim_start();
        if "**".starts_with(trimmed) {
            return (None, None);
        }
        if let Some(body) = trimmed.strip_prefix("**") {
            let Some(close) = body.find("**") else {
                if trimmed.chars().count() <= 160 {
                    return (None, None);
                }
                return self.release_without_title();
            };
            let title = clean_reasoning_title(&body[..close]);
            let suffix = &body[close + 2..];
            if only_line_breaks(suffix) {
                return self.finish_decision(title, String::new());
            }
            if !suffix.starts_with("\n\n") && !suffix.starts_with("\r\n\r\n") {
                return self.release_without_title();
            }
            let rest = suffix.trim_start_matches(['\r', '\n']).to_string();
            return self.finish_decision(title, rest);
        }
        if possible_markdown_heading_prefix(trimmed) {
            return (None, None);
        }
        if let Some(title_start) = markdown_heading_content_start(trimmed) {
            let Some(end) = trimmed.find('\n') else {
                if trimmed.chars().count() <= 160 {
                    return (None, None);
                }
                return self.release_without_title();
            };
            let suffix = &trimmed[end + 1..];
            if only_line_breaks(suffix) {
                return (None, None);
            }
            let title = clean_reasoning_title(&trimmed[title_start..end]);
            let rest = suffix.trim_start_matches(['\r', '\n']).to_string();
            return self.finish_decision(title, rest);
        }
        self.release_without_title()
    }

    fn finish_decision(&mut self, title: String, rest: String) -> (Option<String>, Option<String>) {
        self.pending.clear();
        self.decided = true;
        self.trim_body_prefix = rest.is_empty();
        (
            (!title.is_empty()).then_some(title),
            (!rest.is_empty()).then_some(rest),
        )
    }

    fn release_without_title(&mut self) -> (Option<String>, Option<String>) {
        self.decided = true;
        (None, Some(std::mem::take(&mut self.pending)))
    }

    fn finish(&mut self) -> (Option<String>, Option<String>) {
        if self.pending.is_empty() {
            return (None, None);
        }
        self.decided = true;
        let pending = std::mem::take(&mut self.pending);
        let trimmed = pending.trim_start();
        if let Some(body) = trimmed.strip_prefix("**") {
            if let Some(close) = body.find("**") {
                let suffix = &body[close + 2..];
                if suffix.is_empty()
                    || ((suffix.starts_with("\n\n") || suffix.starts_with("\r\n\r\n"))
                        && only_line_breaks(suffix))
                {
                    let title = clean_reasoning_title(&body[..close]);
                    return ((!title.is_empty()).then_some(title), None);
                }
            }
        }
        if let Some(title_start) = markdown_heading_content_start(trimmed) {
            let title = clean_reasoning_title(&trimmed[title_start..]);
            return ((!title.is_empty()).then_some(title), None);
        }
        (None, Some(trimmed.to_string()))
    }
}

fn possible_markdown_heading_prefix(text: &str) -> bool {
    !text.is_empty() && text.len() <= 6 && text.bytes().all(|byte| byte == b'#')
}

fn only_line_breaks(text: &str) -> bool {
    text.bytes().all(|byte| matches!(byte, b'\r' | b'\n'))
}

fn markdown_heading_content_start(text: &str) -> Option<usize> {
    let hashes = text.bytes().take_while(|byte| *byte == b'#').count();
    if !(1..=6).contains(&hashes) {
        return None;
    }
    let rest = text.get(hashes..)?;
    let whitespace = rest
        .bytes()
        .take_while(|byte| matches!(*byte, b' ' | b'\t'))
        .count();
    (whitespace > 0).then_some(hashes + whitespace)
}

fn clean_reasoning_title(value: &str) -> String {
    let value = compact_one_line(value);
    let value = value.trim_matches(['*', '#', ' ', '\t', '.', '。', '!', '！', '?', '？']);
    truncate_chars(value, 80)
}

fn emit_filtered_chunk_at<F>(
    chunk: ChatStreamChunk,
    received_at: Instant,
    filter: &mut ReasoningTitleFilter,
    on_event: &mut F,
) -> Result<()>
where
    F: FnMut(AgentEvent) -> Result<()>,
{
    match chunk.kind {
        ChatStreamKind::ReasoningPartStart => {
            *filter = ReasoningTitleFilter::default();
            on_event(AgentEvent::ReasoningPartStart { received_at })?;
        }
        ChatStreamKind::ReasoningReset => {
            *filter = ReasoningTitleFilter::default();
            on_event(AgentEvent::ReasoningReset { received_at })?;
        }
        ChatStreamKind::ReasoningPartEnd => {
            let (title, text) = filter.finish();
            if let Some(title) = title {
                on_event(AgentEvent::ReasoningTitle(title))?;
            }
            if let Some(text) = text {
                on_event(AgentEvent::Chunk(ChatStreamChunk {
                    kind: ChatStreamKind::Reasoning,
                    text,
                }))?;
            }
            on_event(AgentEvent::ReasoningPartEnd { received_at })?;
        }
        ChatStreamKind::Reasoning => {
            let (title, text) = filter.push(&chunk.text);
            if let Some(title) = title {
                on_event(AgentEvent::ReasoningTitle(title))?;
            }
            if let Some(text) = text {
                on_event(AgentEvent::Chunk(ChatStreamChunk {
                    kind: ChatStreamKind::Reasoning,
                    text,
                }))?;
            }
        }
        _ => on_event(AgentEvent::Chunk(chunk))?,
    }
    Ok(())
}

#[cfg(test)]
fn emit_filtered_chunk<F>(
    chunk: ChatStreamChunk,
    filter: &mut ReasoningTitleFilter,
    on_event: &mut F,
) -> Result<()>
where
    F: FnMut(AgentEvent) -> Result<()>,
{
    emit_filtered_chunk_at(chunk, Instant::now(), filter, on_event)
}

#[cfg(test)]
fn parse_reasoning_title(reasoning: &str) -> (Option<String>, String) {
    parse_reasoning_title_chunks([reasoning])
}

#[cfg(test)]
fn parse_reasoning_title_chunks<'a>(
    chunks: impl IntoIterator<Item = &'a str>,
) -> (Option<String>, String) {
    let mut filter = ReasoningTitleFilter::default();
    let mut title = None;
    let mut output = String::new();
    for chunk in chunks {
        let (chunk_title, text) = filter.push(chunk);
        title = title.or(chunk_title);
        if let Some(text) = text {
            output.push_str(&text);
        }
    }
    let (finished_title, pending) = filter.finish();
    let title = title.or(finished_title);
    if let Some(pending) = pending {
        output.push_str(&pending);
    }
    (title, output)
}

fn runtime_context(mode: AgentMode) -> String {
    let cwd = std::env::current_dir()
        .map(|path| path.display().to_string())
        .unwrap_or_else(|_| "unknown".to_string());
    if mode == AgentMode::Chat {
        format!(
            "<runtime now=\"{}\" cwd=\"{}\" note=\"cwd is workspace context only; do not infer assistant identity from paths or project names\"/>",
            Local::now().format("%Y年%m月%d日 %A %H:%M"),
            xml_attr_escape(&cwd),
        )
    } else {
        let runtime = terminal_runtime_context();
        format!(
            "<runtime now=\"{}\" cwd=\"{}\" note=\"cwd is workspace context only; do not infer assistant identity from paths or project names\" {runtime}/>",
            Local::now().format("%Y年%m月%d日 %A %H:%M"),
            xml_attr_escape(&cwd),
        )
    }
}

fn terminal_runtime_context() -> String {
    let stdin_tty = std::io::stdin().is_terminal();
    let stdout_tty = std::io::stdout().is_terminal();
    let stderr_tty = std::io::stderr().is_terminal();
    let environment = if stdin_tty || stdout_tty || stderr_tty {
        if crate::i18n::is_zh() {
            "终端会话"
        } else {
            "terminal session"
        }
    } else if crate::i18n::is_zh() {
        "非交互或管道环境"
    } else {
        "non-interactive or piped environment"
    };
    let shell = std::env::var("SHELL")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| "unknown".to_string());
    let mut terminal_parts = Vec::new();
    for key in ["TERM_PROGRAM", "TERM", "COLORTERM"] {
        if let Ok(value) = std::env::var(key) {
            if !value.trim().is_empty() {
                terminal_parts.push(format!("{key}={value}"));
            }
        }
    }
    let terminal = if terminal_parts.is_empty() {
        "unknown".to_string()
    } else {
        terminal_parts.join(", ")
    };
    format!(
        "env=\"{}\" shell=\"{}\" terminal=\"{}\"",
        xml_attr_escape(environment),
        xml_attr_escape(&shell),
        xml_attr_escape(&terminal)
    )
}

fn xml_attr_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn clean_user_visible_text(input: &str) -> String {
    let mut output = input.to_string();
    for tag in ["system-reminder", "system_reminder"] {
        output = strip_tagged_sections(output, tag);
    }
    output
}

fn strip_tagged_sections(mut text: String, tag: &str) -> String {
    let open = format!("<{tag}");
    let close = format!("</{tag}>");
    while let Some(start) = text.find(&open) {
        let Some(relative_end) = text[start..].find(&close) else {
            text.replace_range(start.., "");
            break;
        };
        let end = start + relative_end + close.len();
        text.replace_range(start..end, "");
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AppConfig;
    use crate::paths::MiyuPaths;
    use crate::tools::{empty_parameters, ToolSpec};
    use std::path::PathBuf;

    #[test]
    fn strips_pasted_system_reminder_from_user_input() {
        let input = "继续<system-reminder>hidden</system-reminder> ok";

        assert_eq!(clean_user_visible_text(input), "继续 ok");
    }

    #[test]
    fn strips_unclosed_system_reminder_from_user_input() {
        let input = "继续<system_reminder>hidden";

        assert_eq!(clean_user_visible_text(input), "继续");
    }

    #[test]
    fn formats_dynamic_load_tool_names() {
        assert_eq!(
            tool_event_name("load_skill", r#"{"name":"web-search"}"#),
            "load_skill:web-search"
        );
        assert_eq!(
            tool_event_name("load_tools", r#"{"names":["get_weather","todoupdate"]}"#),
            "load_tools:get_weather,todoupdate"
        );
    }

    #[test]
    fn restores_loaded_tools_from_previous_tool_report() {
        let messages = vec![ChatMessage::plain(
            "assistant",
            "<previous_tool_report name=\"load_tools\">\n{\"loaded_tools\":[\"get_weather\",\"todoupdate\"]}\n</previous_tool_report>",
        )];
        let loaded = loaded_tools_from_messages(&messages);
        assert!(loaded.contains("get_weather"));
        assert!(loaded.contains("todoupdate"));
    }

    #[test]
    fn persists_loaded_tools_with_previous_tool_report_wrapper() {
        let output = serde_json::json!({
            "loaded_tools": [
                {"name": "get_weather"},
                {"name": "todoupdate"}
            ]
        })
        .to_string();

        assert_eq!(
            extract_persistable_tool_report("load_tools", &output).as_deref(),
            Some("<previous_tool_report name=\"load_tools\">\n{\"loaded_tools\":[\"get_weather\",\"todoupdate\"]}\n</previous_tool_report>")
        );
    }

    #[test]
    fn persists_compact_sent_meme_report() {
        let output = serde_json::json!({
            "success": true,
            "id": "sha256:abc123",
            "description": "猫猫\n开心 & <得意>",
            "unused": "ignored",
        })
        .to_string();

        assert_eq!(
            extract_persistable_tool_report("show_meme", &output).as_deref(),
            Some("<sent_meme>发送了一个表情包：id=sha256:abc123；description=猫猫 开心 &amp; &lt;得意&gt;</sent_meme>")
        );
    }

    #[test]
    fn sent_meme_report_allows_missing_description() {
        let output = serde_json::json!({
            "success": true,
            "id": "sha256:abc123",
        })
        .to_string();

        assert_eq!(
            extract_persistable_tool_report("show_meme", &output).as_deref(),
            Some("<sent_meme>发送了一个表情包：id=sha256:abc123</sent_meme>")
        );
    }

    #[test]
    fn sent_meme_report_skips_failed_result() {
        let output = serde_json::json!({
            "success": false,
            "id": "sha256:abc123",
            "description": "猫猫",
        })
        .to_string();

        assert!(extract_persistable_tool_report("show_meme", &output).is_none());
    }

    #[test]
    fn mode_reminder_does_not_inject_a_reasoning_title_protocol() {
        let prompt = with_mode_reminder("base".to_string(), AgentMode::Normal);
        assert_eq!(prompt, "base");
        assert!(!prompt.contains("<runtime"));

        let prompt = with_mode_reminder("base".to_string(), AgentMode::Plan);
        assert!(prompt.contains("base"));
        assert!(prompt.contains(crate::prompts::PLAN_REMINDER));
        assert!(!prompt.contains("<runtime"));
    }

    #[test]
    fn reasoning_title_filter_emits_completed_markdown_title_immediately() {
        let mut filter = ReasoningTitleFilter::default();
        assert_eq!(filter.push("**Preparing to"), (None, None));
        assert_eq!(
            filter.push(" call tools**"),
            (Some("Preparing to call tools".to_string()), None)
        );
        assert_eq!(filter.finish(), (None, None));
    }

    #[test]
    fn reasoning_title_filter_strips_delayed_blank_line_before_body() {
        let mut filter = ReasoningTitleFilter::default();
        assert_eq!(
            filter.push("**Preparing to call tools**\n"),
            (Some("Preparing to call tools".to_string()), None)
        );
        assert_eq!(
            filter.push("\nInspect the arguments."),
            (None, Some("Inspect the arguments.".to_string()))
        );
    }

    #[test]
    fn reasoning_title_filter_streams_plain_body_without_inventing_title() {
        let mut filter = ReasoningTitleFilter::default();
        assert_eq!(
            filter.push("The user is"),
            (None, Some("The user is".to_string()))
        );
        assert_eq!(
            filter.push(" asking what changed."),
            (None, Some(" asking what changed.".to_string()))
        );
        assert_eq!(
            filter.push(" Continue analysis."),
            (None, Some(" Continue analysis.".to_string()))
        );
        assert_eq!(filter.finish(), (None, None));
    }

    #[test]
    fn reasoning_title_filter_keeps_long_markdown_heading_text() {
        let title = "heading ".repeat(12);
        let text = format!("# {title}\n\nBody reasoning.");
        let mut filter = ReasoningTitleFilter::default();
        let (parsed_title, body) = filter.push(&text);

        assert!(parsed_title.is_some());
        assert_eq!(body.as_deref(), Some("Body reasoning."));
        assert_eq!(filter.finish(), (None, None));
    }

    #[test]
    fn reasoning_title_filter_extracts_markdown_action_heading() {
        assert_eq!(
            parse_reasoning_title(
                "**Planning response approach and title clipping**\n\nInspect the renderer."
            ),
            (
                Some("Planning response approach and title clipping".to_string()),
                "Inspect the renderer.".to_string()
            )
        );
    }

    #[test]
    fn reasoning_title_filter_keeps_ordinary_bold_text_in_body() {
        assert_eq!(
            parse_reasoning_title("**Important:** keep this in the body."),
            (None, "**Important:** keep this in the body.".to_string())
        );
    }

    #[test]
    fn reasoning_title_filter_matches_unsplit_input_at_every_character_boundary() {
        for text in [
            "**检查参数**\n\n\n继续分析。",
            "## 检查参数\n\n\n继续分析。",
            "**Checking arguments**\r\n\r\nContinue analysis.",
            "#include <stdio.h>",
        ] {
            let expected = parse_reasoning_title(text);
            for split in text
                .char_indices()
                .map(|(index, _)| index)
                .chain(std::iter::once(text.len()))
            {
                assert_eq!(
                    parse_reasoning_title_chunks([&text[..split], &text[split..]]),
                    expected,
                    "different result when split at byte {split} in {text:?}"
                );
            }
        }
    }

    #[test]
    fn reasoning_title_filter_does_not_show_incomplete_bold_title() {
        assert_eq!(
            parse_reasoning_title("**Incomplete title"),
            (None, "**Incomplete title".to_string())
        );
    }

    #[test]
    fn reasoning_title_filter_does_not_use_first_sentence_as_title() {
        assert_eq!(
            parse_reasoning_title("Designing the clipping helper. Keep the rest."),
            (
                None,
                "Designing the clipping helper. Keep the rest.".to_string()
            )
        );
    }

    #[test]
    fn reasoning_part_start_reopens_title_detection() {
        let mut filter = ReasoningTitleFilter::default();
        let mut titles = Vec::new();
        let mut reasoning = Vec::new();
        let mut on_event = |event| {
            match event {
                AgentEvent::ReasoningTitle(title) => titles.push(title),
                AgentEvent::Chunk(chunk) if chunk.kind == ChatStreamKind::Reasoning => {
                    reasoning.push(chunk.text);
                }
                _ => {}
            }
            Ok(())
        };

        emit_filtered_chunk(
            ChatStreamChunk {
                kind: ChatStreamKind::ReasoningPartStart,
                text: String::new(),
            },
            &mut filter,
            &mut on_event,
        )
        .unwrap();
        emit_filtered_chunk(
            ChatStreamChunk {
                kind: ChatStreamKind::Reasoning,
                text: "**First title**\n\nFirst body.".to_string(),
            },
            &mut filter,
            &mut on_event,
        )
        .unwrap();
        emit_filtered_chunk(
            ChatStreamChunk {
                kind: ChatStreamKind::ReasoningPartEnd,
                text: String::new(),
            },
            &mut filter,
            &mut on_event,
        )
        .unwrap();
        emit_filtered_chunk(
            ChatStreamChunk {
                kind: ChatStreamKind::ReasoningPartStart,
                text: String::new(),
            },
            &mut filter,
            &mut on_event,
        )
        .unwrap();
        emit_filtered_chunk(
            ChatStreamChunk {
                kind: ChatStreamKind::Reasoning,
                text: "**Second title**".to_string(),
            },
            &mut filter,
            &mut on_event,
        )
        .unwrap();

        assert_eq!(titles, vec!["First title", "Second title"]);
        assert_eq!(reasoning, vec!["First body."]);
    }

    #[test]
    fn reasoning_summary_finishes_before_answer_content() {
        let mut filter = ReasoningTitleFilter::default();
        let mut events = Vec::new();
        let mut on_event = |event| {
            events.push(match event {
                AgentEvent::ReasoningPartStart { .. } => "part-start".to_string(),
                AgentEvent::ReasoningTitle(title) => format!("title:{title}"),
                AgentEvent::Chunk(chunk) => format!("{:?}:{}", chunk.kind, chunk.text),
                AgentEvent::ReasoningPartEnd { .. } => "part-end".to_string(),
                _ => "other".to_string(),
            });
            Ok(())
        };

        for chunk in [
            ChatStreamChunk {
                kind: ChatStreamKind::ReasoningPartStart,
                text: String::new(),
            },
            ChatStreamChunk {
                kind: ChatStreamKind::Reasoning,
                text: "**Checking event order**\n\nSummary body.".to_string(),
            },
            ChatStreamChunk {
                kind: ChatStreamKind::ReasoningPartEnd,
                text: String::new(),
            },
            ChatStreamChunk {
                kind: ChatStreamKind::Content,
                text: "Answer.".to_string(),
            },
        ] {
            emit_filtered_chunk(chunk, &mut filter, &mut on_event).unwrap();
        }

        assert_eq!(
            events,
            [
                "part-start",
                "title:Checking event order",
                "Reasoning:Summary body.",
                "part-end",
                "Content:Answer.",
            ]
        );
    }

    #[test]
    fn reasoning_boundaries_preserve_chunk_receive_timestamps() {
        let mut filter = ReasoningTitleFilter::default();
        let started_at = Instant::now();
        let ended_at = started_at + Duration::from_millis(725);
        let mut boundaries = Vec::new();
        let mut on_event = |event| {
            match event {
                AgentEvent::ReasoningPartStart { received_at } => {
                    boundaries.push(("start", received_at));
                }
                AgentEvent::ReasoningPartEnd { received_at } => {
                    boundaries.push(("end", received_at));
                }
                _ => {}
            }
            Ok(())
        };

        emit_filtered_chunk_at(
            ChatStreamChunk {
                kind: ChatStreamKind::ReasoningPartStart,
                text: String::new(),
            },
            started_at,
            &mut filter,
            &mut on_event,
        )
        .unwrap();
        emit_filtered_chunk_at(
            ChatStreamChunk {
                kind: ChatStreamKind::ReasoningPartEnd,
                text: String::new(),
            },
            ended_at,
            &mut filter,
            &mut on_event,
        )
        .unwrap();

        assert_eq!(boundaries, [("start", started_at), ("end", ended_at)]);
    }

    #[test]
    fn reasoning_title_filter_does_not_treat_hash_include_as_heading() {
        assert_eq!(
            parse_reasoning_title("#include <stdio.h>"),
            (None, "#include <stdio.h>".to_string())
        );
    }

    #[test]
    fn runtime_context_contains_dynamic_runtime_only() {
        let context = runtime_context(AgentMode::Normal);
        assert!(context.starts_with("<runtime "));
        assert!(context.contains("now=\""));
        assert!(context.contains("cwd=\""));
    }

    #[test]
    fn effective_context_tokens_include_tool_definitions() {
        let temp = tempfile::tempdir().unwrap();
        let paths = test_paths(temp.path());
        let config = AppConfig::default();
        let state = StateStore::new(&paths).unwrap();
        state.init_files().unwrap();
        let client =
            OpenAiCompatibleClient::new(config.provider(None).unwrap(), &config, &paths).unwrap();
        let mut tools = ToolRegistry::new();
        tools.register(ToolSpec::new(
            "heavy_context_tool",
            "This tool has a deliberately long description so effective context includes tool definitions.",
            empty_parameters(),
            |_| async { Ok(String::new()) },
        ));
        let with_tools = Agent::new(
            config.clone(),
            &paths,
            state.clone(),
            client.clone(),
            tools,
            AgentMode::Normal,
        )
        .unwrap();
        let without_tools = Agent::new(
            AppConfig {
                tools: crate::config::ToolsConfig {
                    enabled: false,
                    ..config.tools.clone()
                },
                ..config
            },
            &paths,
            state,
            client,
            ToolRegistry::new(),
            AgentMode::Normal,
        )
        .unwrap();

        assert!(
            with_tools.effective_context_tokens().unwrap()
                > without_tools.effective_context_tokens().unwrap()
        );
    }

    #[test]
    fn overflow_check_tokens_triggers_at_threshold() {
        let check = overflow::OverflowCheck::new(Some(100_000), 0.9, None);
        assert!(!check.check_tokens(60_000));
        assert!(check.check_tokens(95_000));
    }

    #[test]
    fn overflow_check_disabled_when_no_window() {
        let check = overflow::OverflowCheck::new(None, 0.9, None);
        assert!(!check.is_enabled());
        assert!(!check.check_tokens(1_998_998));
    }

    #[test]
    fn overflow_check_estimate_triggers() {
        let check = overflow::OverflowCheck::new(Some(1_000), 0.9, None);
        let big_msg = ChatMessage::plain("user", &"token ".repeat(2_000));
        let small_msg = ChatMessage::plain("user", "hi");
        assert!(check.check_estimate(&[big_msg]));
        assert!(!check.check_estimate(&[small_msg]));
    }

    #[test]
    fn turn_context_tokens_match_sent_messages() {
        let mut turn = crate::state::Turn {
            turn_id: "t1".to_string(),
            seq: 1,
            user_content: "question".to_string(),
            user_timestamp: String::new(),
            assistant_content: "answer".to_string(),
            assistant_reasoning: Some("hidden reasoning ".repeat(1_000)),
            assistant_timestamp: None,
            status: crate::state::TurnStatus::Completed,
            tool_reports: Vec::new(),
            question_exchanges: Vec::new(),
            hidden: false,
            is_summary: false,
            owner_pid: None,
            token_total: 0,
            token_usage_estimated: false,
        };
        let without_reports = turn_context_tokens(&turn);
        turn.assistant_reasoning = None;
        assert_eq!(turn_context_tokens(&turn), without_reports);

        turn.tool_reports.push("persisted tool result".to_string());
        assert!(turn_context_tokens(&turn) > without_reports);
    }

    #[test]
    fn trim_visible_context_keeps_summary_and_removes_oldest_turn() {
        let temp = tempfile::tempdir().unwrap();
        let paths = test_paths(temp.path());
        let config = AppConfig {
            tools: crate::config::ToolsConfig {
                enabled: false,
                ..AppConfig::default().tools
            },
            ..AppConfig::default()
        };
        let state = StateStore::new(&paths).unwrap();
        state.init_files().unwrap();
        let client =
            OpenAiCompatibleClient::new(config.provider(None).unwrap(), &config, &paths).unwrap();
        let mut agent = Agent::new(
            config,
            &paths,
            state.clone(),
            client,
            ToolRegistry::new(),
            AgentMode::Normal,
        )
        .unwrap();
        state
            .insert_summary_turn(&"summary ".repeat(2_000), None, true)
            .unwrap();
        for id in ["t1", "t2"] {
            state
                .start_turn(id, &format!("{id} {}", "question ".repeat(2_000)), 999999)
                .unwrap();
            state
                .complete_turn(id, &"answer ".repeat(2_000), None)
                .unwrap();
        }
        agent.trim_at_ratio = 1.0;
        let context_window = agent.effective_context_tokens().unwrap() as usize;
        let choice = agent.config.active_provider_model_choices().remove(0);
        agent
            .config
            .providers
            .iter_mut()
            .find(|provider| provider.id == choice.provider_id)
            .unwrap()
            .model_context_window
            .insert(choice.model, context_window);
        assert_eq!(agent.context_window(), Some(context_window));

        let evicted = agent.trim_visible_context().unwrap();

        assert!(!evicted.is_empty());
        let visible = state.load_visible_turns().unwrap();
        assert_eq!(visible.len(), 2);
        assert!(visible[0].is_summary);
        assert_eq!(visible[1].turn_id, "t2");
    }

    #[test]
    fn trim_accounts_for_tool_definitions_unloaded_with_a_popped_turn() {
        let temp = tempfile::tempdir().unwrap();
        let paths = test_paths(temp.path());
        let mut config = AppConfig::default();
        config.tools.loading_mode = "hybrid".to_string();
        let state = StateStore::new(&paths).unwrap();
        state.init_files().unwrap();
        let client =
            OpenAiCompatibleClient::new(config.provider(None).unwrap(), &config, &paths).unwrap();
        let mut tools = ToolRegistry::new();
        tools.register(
            ToolSpec::new(
                "heavy_context_tool",
                "heavy context ".repeat(20_000),
                empty_parameters(),
                |_| async { Ok(String::new()) },
            )
            .with_always_loaded(false),
        );
        let mut agent = Agent::new(
            config,
            &paths,
            state.clone(),
            client,
            tools,
            AgentMode::Normal,
        )
        .unwrap();
        for id in ["t1", "t2"] {
            state.start_turn(id, id, 999999).unwrap();
            state.complete_turn(id, "reply", None).unwrap();
        }
        state
            .add_session_loaded_tools(&["heavy_context_tool".to_string()], Some("t1"))
            .unwrap();
        agent.trim_at_ratio = 1.0;
        agent.trim_batch_ratio = 0.5;
        let context_window = agent.effective_context_tokens().unwrap() as usize;
        let choice = agent.config.active_provider_model_choices().remove(0);
        agent
            .config
            .providers
            .iter_mut()
            .find(|provider| provider.id == choice.provider_id)
            .unwrap()
            .model_context_window
            .insert(choice.model, context_window);

        agent.trim_visible_context().unwrap();

        let visible = state.load_visible_turns().unwrap();
        assert_eq!(visible.len(), 1);
        assert_eq!(visible[0].turn_id, "t2");
        assert!(state.load_session_loaded_tools().unwrap().is_empty());
    }

    #[test]
    fn trim_ignores_stale_loaded_tool_sources_when_persistence_is_disabled() {
        let temp = tempfile::tempdir().unwrap();
        let paths = test_paths(temp.path());
        let mut config = AppConfig::default();
        config.tools.loading_mode = "hybrid".to_string();
        config.tools.persist_loaded_tools = false;
        let state = StateStore::new(&paths).unwrap();
        state.init_files().unwrap();
        let client =
            OpenAiCompatibleClient::new(config.provider(None).unwrap(), &config, &paths).unwrap();
        let mut tools = ToolRegistry::new();
        tools.register(
            ToolSpec::new(
                "stale_heavy_tool",
                "stale heavy context ".repeat(20_000),
                empty_parameters(),
                |_| async { Ok(String::new()) },
            )
            .with_always_loaded(false),
        );
        let mut agent = Agent::new(
            config,
            &paths,
            state.clone(),
            client,
            tools,
            AgentMode::Normal,
        )
        .unwrap();
        for id in ["t1", "t2"] {
            state.start_turn(id, id, 999999).unwrap();
            state.complete_turn(id, "reply", None).unwrap();
        }
        state
            .add_session_loaded_tools(&["stale_heavy_tool".to_string()], Some("t1"))
            .unwrap();
        agent.trim_at_ratio = 1.0;
        agent.trim_batch_ratio = 0.5;
        let context_window = agent.effective_context_tokens().unwrap() as usize;
        let choice = agent.config.active_provider_model_choices().remove(0);
        agent
            .config
            .providers
            .iter_mut()
            .find(|provider| provider.id == choice.provider_id)
            .unwrap()
            .model_context_window
            .insert(choice.model, context_window);

        agent.trim_visible_context().unwrap();

        assert!(state.load_visible_turns().unwrap().is_empty());
    }

    #[test]
    fn explicit_pop_archives_context_content_but_not_reasoning() {
        let temp = tempfile::tempdir().unwrap();
        let paths = test_paths(temp.path());
        let config = AppConfig::default();
        let state = StateStore::new(&paths).unwrap();
        state.start_turn("t1", "promptonlyalpha", 999999).unwrap();
        state
            .complete_turn("t1", "answeronlybeta", Some("reasoningonlyquasar"))
            .unwrap();
        state
            .append_persisted_context("t1", "toolonlygamma")
            .unwrap();
        let memory = MemoryStore::new(&config, &paths);
        let turns = state.oldest_evictable_visible_turns(1).unwrap();

        archive_and_delete_visible_turns(&state, &memory, &turns).unwrap();

        assert!(state.load_visible_turns().unwrap().is_empty());
        for query in ["promptonlyalpha", "answeronlybeta", "toolonlygamma"] {
            assert!(
                !memory.search_evicted_context(query, 10).unwrap()["results"]
                    .as_array()
                    .unwrap()
                    .is_empty()
            );
        }
        assert!(memory
            .search_evicted_context("reasoningonlyquasar", 10)
            .unwrap()["results"]
            .as_array()
            .unwrap()
            .is_empty());
    }

    #[test]
    fn explicit_pop_still_deletes_when_evicted_context_archiving_is_disabled() {
        let temp = tempfile::tempdir().unwrap();
        let paths = test_paths(temp.path());
        let mut config = AppConfig::default();
        config.memory.evicted_context_enabled = false;
        let state = StateStore::new(&paths).unwrap();
        state.start_turn("t1", "unarchived-marker", 999999).unwrap();
        state.complete_turn("t1", "reply", None).unwrap();
        let memory = MemoryStore::new(&config, &paths);
        let turns = state.oldest_evictable_visible_turns(1).unwrap();

        archive_and_delete_visible_turns(&state, &memory, &turns).unwrap();

        assert!(state.load_visible_turns().unwrap().is_empty());
        assert!(memory
            .search_evicted_context("unarchived-marker", 10)
            .unwrap()["results"]
            .as_array()
            .unwrap()
            .is_empty());
    }

    #[test]
    fn explicit_pop_does_not_archive_a_turn_removed_before_commit() {
        let temp = tempfile::tempdir().unwrap();
        let paths = test_paths(temp.path());
        let config = AppConfig::default();
        let state = StateStore::new(&paths).unwrap();
        state
            .start_turn("t1", "stale-archive-quasar", 999999)
            .unwrap();
        state.complete_turn("t1", "reply", None).unwrap();
        let turns = state.oldest_evictable_visible_turns(1).unwrap();
        state.delete_visible_turns(&["t1".to_string()]).unwrap();
        let memory = MemoryStore::new(&config, &paths);

        assert!(archive_and_delete_visible_turns(&state, &memory, &turns).is_err());

        assert!(memory
            .search_evicted_context("stale-archive-quasar", 10)
            .unwrap()["results"]
            .as_array()
            .unwrap()
            .is_empty());
    }

    #[test]
    fn failed_concurrent_pop_preserves_archive_from_the_successful_pop() {
        let temp = tempfile::tempdir().unwrap();
        let paths = test_paths(temp.path());
        let config = AppConfig::default();
        let state = StateStore::new(&paths).unwrap();
        state
            .start_turn("t1", "successful-pop-quasar", 999999)
            .unwrap();
        state.complete_turn("t1", "reply", None).unwrap();
        let turns = state.oldest_evictable_visible_turns(1).unwrap();
        let memory = MemoryStore::new(&config, &paths);

        archive_and_delete_visible_turns(&state, &memory, &turns).unwrap();
        assert!(archive_and_delete_visible_turns(&state, &memory, &turns).is_err());

        assert!(!memory
            .search_evicted_context("successful-pop-quasar", 10)
            .unwrap()["results"]
            .as_array()
            .unwrap()
            .is_empty());
    }

    #[test]
    fn explicit_pop_removes_new_archive_when_the_turn_still_exists_hidden() {
        let temp = tempfile::tempdir().unwrap();
        let paths = test_paths(temp.path());
        let config = AppConfig::default();
        let state = StateStore::new(&paths).unwrap();
        state
            .start_turn("t1", "hidden-stale-quasar", 999999)
            .unwrap();
        state.complete_turn("t1", "reply", None).unwrap();
        let turns = state.oldest_evictable_visible_turns(1).unwrap();
        state
            .replace_visible_with_summary(turns[0].seq, &["t1".to_string()], "summary", None, false)
            .unwrap();
        let memory = MemoryStore::new(&config, &paths);

        assert!(archive_and_delete_visible_turns(&state, &memory, &turns).is_err());

        assert!(memory
            .search_evicted_context("hidden-stale-quasar", 10)
            .unwrap()["results"]
            .as_array()
            .unwrap()
            .is_empty());
    }

    fn test_paths(root: &std::path::Path) -> MiyuPaths {
        MiyuPaths {
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
}
