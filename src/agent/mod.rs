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
use crate::render::wait_spinner::SPINNER_INTERVAL;
use crate::state::StateStore;
use crate::tools::{self, memes, vision, ToolPermission, ToolRegistry};
use anyhow::{bail, Result};
use chrono::Local;
use serde_json::Value;
use std::collections::BTreeSet;
use std::io::IsTerminal;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::mpsc;

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

#[derive(Debug, Clone)]
pub enum AgentEvent {
    Chunk(ChatStreamChunk),
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
    SpinnerTick,
    CompactStart,
    CompactChunk(ChatStreamChunk),
    CompactEnd,
    PopStart,
    PopEnd,
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
    context_window: Option<usize>,
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
        let context_window = config.active_context_window()?;
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
            context_window,
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
        self.context_window
    }

    pub fn effective_context_tokens(&self) -> Result<u64> {
        let messages = self.chat_messages("", "")?;
        let mut tokens = overflow::estimate_messages_tokens(&messages) as u64;
        if self.tools_enabled {
            let loaded_tools = self.initial_loaded_tools(&messages)?;
            let definitions = {
                let tools = self.tools.lock().unwrap();
                if tools::is_hybrid_loading_mode(&self.config.tools.loading_mode) {
                    tools.lazy_definitions(&loaded_tools)
                } else {
                    tools.definitions()
                }
            };
            tokens = tokens.saturating_add(estimate_tool_definition_tokens(&definitions) as u64);
        }
        Ok(tokens)
    }

    pub fn switch_mode(&mut self, mode: AgentMode, tools: ToolRegistry) {
        self.mode = mode;
        self.tools = Arc::new(Mutex::new(tools));
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
        self.context_window = self.config.active_context_window()?;
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
        self.state.mark_interrupted_turn_if_needed()?;
        let evicted = if let Some(window) = self.context_window {
            self.state.trim_visible_to_token_budget(
                window,
                self.trim_at_ratio,
                self.trim_batch_ratio,
            )?
        } else {
            Vec::new()
        };
        let evicted = evicted
            .into_iter()
            .map(|entry| EvictedTurn {
                timestamp: entry.timestamp,
                role: entry.role,
                content: entry.content,
            })
            .collect::<Vec<_>>();
        self.memory.remember_evicted_turns(&evicted)?;
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
        usage: &Usage,
        on_event: F,
    ) -> Result<Option<ChatResult>>
    where
        F: FnMut(AgentEvent) -> Result<()>,
    {
        let mut on_event = on_event;
        let Some(compact) = self.handle_overflow(usage, &mut on_event).await? else {
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
        let Some(context_window) = self.context_window else {
            bail!("当前模型未配置上下文窗口，无法压缩上下文");
        };
        let visible_count = self.state.load_visible_turns()?.len();
        if visible_count == 0 {
            return Ok(None);
        }
        let check = overflow::OverflowCheck::new(self.context_window, self.trim_at_ratio, None);
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
        usage: &Usage,
        on_event: &mut F,
    ) -> Result<Option<compact::CompactResult>>
    where
        F: FnMut(AgentEvent) -> Result<()>,
    {
        let check = overflow::OverflowCheck::new(self.context_window, self.trim_at_ratio, None);
        if !check.is_enabled() || !check.check_usage(usage) {
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
                    self.context_window.unwrap(),
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
                let evicted = self.state.trim_visible_to_token_budget(
                    self.context_window.unwrap(),
                    self.trim_at_ratio,
                    self.trim_batch_ratio,
                )?;
                let evicted_turns: Vec<EvictedTurn> = evicted
                    .into_iter()
                    .map(|entry| EvictedTurn {
                        timestamp: entry.timestamp,
                        role: entry.role,
                        content: entry.content,
                    })
                    .collect();
                self.memory.remember_evicted_turns(&evicted_turns)?;
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
        let mut loaded_tools = self.initial_loaded_tools(messages)?;
        let mut usage_accumulator = UsageAccumulator::default();
        loop {
            if self.max_tool_rounds > 0 && tool_round >= self.max_tool_rounds {
                let content = format!(
                    "工具调用已达到上限 {} 轮，已停止继续调用。可将 `tools.max_rounds` 设为 0 以允许无限工具调用。",
                    self.max_tool_rounds
                );
                on_event(AgentEvent::Chunk(ChatStreamChunk {
                    kind: ChatStreamKind::Content,
                    text: content.clone(),
                }))?;
                let usage = usage_accumulator.usage();
                return Ok(ChatResult {
                    content,
                    reasoning: None,
                    usage,
                    usage_estimated: usage_accumulator.estimated,
                    tool_calls: Vec::new(),
                    provider_id: None,
                    model: None,
                });
            }
            tool_round += 1;

            if self.mode == AgentMode::Normal {
                let mut tools = self.tools.lock().unwrap();
                tools::rescan_scripts(&mut tools, &self.paths);
                tools::register_script_display_names(&tools);
            }

            let definitions = if self.tools_enabled {
                let tools = self.tools.lock().unwrap();
                if tools::is_hybrid_loading_mode(&self.config.tools.loading_mode) {
                    tools.lazy_definitions(&loaded_tools)
                } else {
                    tools.definitions()
                }
            } else {
                Vec::new()
            };

            let (chunk_tx, mut chunk_rx) =
                tokio::sync::mpsc::unbounded_channel::<ChatStreamChunk>();
            let request_messages = messages.clone();
            let llm_future =
                self.client
                    .chat_stream(request_messages.clone(), definitions, move |chunk| {
                        let _ = chunk_tx.send(chunk);
                        Ok(())
                    });
            tokio::pin!(llm_future);
            let mut spinner_interval = tokio::time::interval(SPINNER_INTERVAL);
            spinner_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            spinner_interval.tick().await;
            let result = loop {
                tokio::select! {
                    result = &mut llm_future => {
                        break result?;
                    }
                    Some(chunk) = chunk_rx.recv() => {
                        on_event(AgentEvent::Chunk(chunk))?;
                    }
                    _ = spinner_interval.tick() => {
                        on_event(AgentEvent::SpinnerTick)?;
                    }
                }
            };
            while let Ok(chunk) = chunk_rx.try_recv() {
                on_event(AgentEvent::Chunk(chunk))?;
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
            messages.push(ChatMessage::assistant(
                result.content.clone(),
                Some(result.tool_calls.clone()),
            ));
            for call in result.tool_calls {
                let event_name = tool_event_name(&call.function.name, &call.function.arguments);
                used_tools.push(call.function.name.clone());
                on_event(AgentEvent::ToolCall {
                    name: event_name.clone(),
                    arguments: call.function.arguments.clone(),
                })?;
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
                                    while let Ok(message) = progress_rx.try_recv() {
                                        on_event(AgentEvent::ToolProgress {
                                            name: event_name.clone(),
                                            message,
                                        })?;
                                    }
                                    (output, true)
                                }
                                Err(err) => {
                                    while let Ok(message) = progress_rx.try_recv() {
                                        on_event(AgentEvent::ToolProgress {
                                            name: event_name.clone(),
                                            message,
                                        })?;
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
                        Some(message) = progress_rx.recv() => {
                            on_event(AgentEvent::ToolProgress {
                                name: event_name.clone(),
                                message,
                            })?;
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
                    on_event(AgentEvent::ToolResult {
                        name: event_name.clone(),
                        ok: true,
                        output: output.clone(),
                    })?;
                    if let Some(report) =
                        extract_persistable_tool_report(&call.function.name, &output)
                    {
                        persisted_tool_reports.push((call.function.name.clone(), report));
                    }
                }
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
    use crate::llm::Usage;
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
    fn mode_reminder_keeps_runtime_out_of_stable_prompt() {
        let prompt = with_mode_reminder("base".to_string(), AgentMode::Normal);
        assert_eq!(prompt, "base");
        assert!(!prompt.contains("<runtime"));

        let prompt = with_mode_reminder("base".to_string(), AgentMode::Plan);
        assert!(prompt.contains("base"));
        assert!(prompt.contains(crate::prompts::PLAN_REMINDER));
        assert!(!prompt.contains("<runtime"));
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
    fn overflow_check_usage_triggers_at_threshold() {
        let check = overflow::OverflowCheck::new(Some(100_000), 0.9, None);
        assert!(!check.check_usage(&Usage {
            prompt_tokens: 50_000,
            completion_tokens: 10_000,
            total_tokens: 60_000,
        }));
        assert!(check.check_usage(&Usage {
            prompt_tokens: 85_000,
            completion_tokens: 10_000,
            total_tokens: 95_000,
        }));
    }

    #[test]
    fn overflow_check_disabled_when_no_window() {
        let check = overflow::OverflowCheck::new(None, 0.9, None);
        assert!(!check.is_enabled());
        assert!(!check.check_usage(&Usage {
            prompt_tokens: 999_999,
            completion_tokens: 999_999,
            total_tokens: 1_998_998,
        }));
    }

    #[test]
    fn overflow_check_estimate_triggers() {
        let check = overflow::OverflowCheck::new(Some(1_000), 0.9, None);
        let big_msg = ChatMessage::plain("user", &"token ".repeat(2_000));
        let small_msg = ChatMessage::plain("user", "hi");
        assert!(check.check_estimate(&[big_msg]));
        assert!(!check.check_estimate(&[small_msg]));
    }

    fn test_paths(root: &std::path::Path) -> MiyuPaths {
        MiyuPaths {
            config_dir: root.join("config"),
            config_file: root.join("config/config.jsonc"),
            secrets_file: root.join("config/secrets.jsonc"),
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
