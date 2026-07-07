mod compact;
mod conversation;
mod overflow;

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
use std::time::{Duration, Instant};
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
    Yolo,
    Plan,
}

impl AgentMode {
    pub fn label(self) -> &'static str {
        match self {
            Self::Yolo => "YOLO",
            Self::Plan => "PLAN",
        }
    }

    fn reminder(self) -> &'static str {
        match self {
            Self::Yolo => crate::prompts::YOLO_REMINDER,
            Self::Plan => crate::prompts::PLAN_REMINDER,
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
    ExternalOutput,
    SpinnerTick,
    MemeSelectPhase,
    CompactStart,
    CompactChunk(ChatStreamChunk),
    CompactEnd,
    PopStart,
    PopEnd {
        popped_count: usize,
    },
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
        if mode == AgentMode::Yolo {
            state.reset_if_prompt_changed(&base_system_prompt)?;
            state.recover_stale_turns()?;
        }
        let system_prompt = with_current_time(base_system_prompt, mode);
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
        let temp_paths: Vec<String> = if !binary_images.is_empty() {
            binary_images
                .iter()
                .enumerate()
                .filter_map(|(i, img)| {
                    img.write_temp_file(&self.paths.cache_dir, i)
                        .ok()
                        .map(|p| p.display().to_string())
                })
                .collect()
        } else {
            Vec::new()
        };
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
        self.state.start_turn(&turn_id, &input, std::process::id())?;
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
        if let Some(association) = self.memory.association(&input)? {
            messages.insert(
                1,
                ChatMessage::system(self.memory.format_association(&association)),
            );
        }
        let mut on_event = on_event;
        let meme_future =
            memes::plan_auto_meme_before_reply(&self.config, &self.paths, &self.client, &input);
        tokio::pin!(meme_future);
        let mut spinner_interval = tokio::time::interval(SPINNER_INTERVAL);
        spinner_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        spinner_interval.tick().await;
        let meme_start = Instant::now();
        let mut meme_phase_switched = false;
        let auto_meme_plan = loop {
            tokio::select! {
                result = &mut meme_future => {
                    break result?;
                }
                _ = spinner_interval.tick() => {
                    if !meme_phase_switched
                        && meme_start.elapsed() >= Duration::from_millis(150)
                    {
                        on_event(AgentEvent::MemeSelectPhase)?;
                        meme_phase_switched = true;
                    }
                    on_event(AgentEvent::SpinnerTick)?;
                }
            }
        };
        if let Some(plan) = &auto_meme_plan {
            messages.push(ChatMessage::system(plan.reminder.clone()));
        }
        let mut used_tools = Vec::new();
        let mut persisted_tool_reports = Vec::new();
        let result = self
            .chat_with_tools(
                &mut messages,
                &mut used_tools,
                &mut persisted_tool_reports,
                &mut on_event,
            )
            .await?;
        if let Some(plan) = auto_meme_plan {
            on_event(AgentEvent::ExternalOutput)?;
            memes::render_auto_meme(&self.config, &self.paths, &plan.event).await?;
            memes::record_auto_meme_event(&self.config, &self.paths, &plan.event)?;
        }
        for (tool_name, report) in persisted_tool_reports {
            self.state
                .append_tool_report_context(&turn_id, &tool_name, &report)?;
        }
        let token_total = result.usage.as_ref().map(Usage::effective_total_tokens);
        guard.complete(
            &result.content,
            result.reasoning.as_deref(),
            token_total,
            result.usage_estimated,
        )?;
        self.memory.process_after_turn(&input, &result.content)?;
        if let Some(usage) = &result.usage {
            self.state.add_usage(usage)?;
            self.handle_overflow(usage, &mut on_event).await?;
        }
        Ok(result)
    }

    async fn handle_overflow<F>(&self, usage: &Usage, on_event: &mut F) -> Result<()>
    where
        F: FnMut(AgentEvent) -> Result<()>,
    {
        let check = overflow::OverflowCheck::new(
            self.context_window,
            self.trim_at_ratio,
            None,
        );
        if !check.is_enabled() || !check.check_usage(usage) {
            return Ok(());
        }
        match self.on_overflow.as_str() {
            "compact" => {
                let visible_count = self.state.load_visible_turns()?.len();
                if visible_count == 0 {
                    return Ok(());
                }
                on_event(AgentEvent::CompactStart)?;
                let compactor = compact::Compactor::new(
                    self.client.clone(),
                    self.state.clone(),
                    self.context_window.unwrap(),
                    check.reserved_tokens,
                );
                let mut on_chunk = |chunk: ChatStreamChunk| {
                    on_event(AgentEvent::CompactChunk(chunk))
                };
                match compactor.perform_compact(&mut on_chunk).await {
                    Ok(_) => {
                        on_event(AgentEvent::CompactEnd)?;
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
                on_event(AgentEvent::PopEnd {
                    popped_count: evicted_turns.len(),
                })?;
            }
            _ => {}
        }
        Ok(())
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
        messages: &mut Vec<ChatMessage>,
        used_tools: &mut Vec<String>,
        persisted_tool_reports: &mut Vec<(String, String)>,
        on_event: &mut F,
    ) -> Result<ChatResult>
    where
        F: FnMut(AgentEvent) -> Result<()>,
    {
        let mut tool_round = 0usize;
        let mut loaded_tools = loaded_tools_from_messages(messages);
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
                });
            }
            tool_round += 1;

            {
                let mut tools = self.tools.lock().unwrap();
                tools::rescan_scripts(&mut tools, &self.paths);
            }

            let definitions = if self.tools_enabled {
                let tools = self.tools.lock().unwrap();
                if self.config.tools.loading_mode == "lazy" {
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
            let llm_future = self
                .client
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
                    if self.mode == AgentMode::Plan
                        && tools.permission(&call.function.name)? != ToolPermission::ReadOnly
                    {
                        bail!(
                            "Plan mode blocked non-read-only tool: {}",
                            call.function.name
                        );
                    }
                    if self.config.tools.loading_mode == "lazy"
                        && call.function.name != "load_tools"
                        && tools.requires_lazy_load(&call.function.name, &loaded_tools)
                    {
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
                    for name in tool_names_arg(&call.function.arguments) {
                        loaded_tools.insert(name);
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

    fn chat_messages(&self, current_turn_id: &str, current_input: &str) -> Result<Vec<ChatMessage>> {
        let mut messages = vec![ChatMessage::system(self.system_prompt.clone())];
        if let Some(summary) = memes::last_auto_meme_reminder(&self.config, &self.paths)? {
            messages.push(ChatMessage::system(summary));
        }
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
            for report in &turn.tool_reports {
                messages.push(ChatMessage::plain("assistant", report));
            }
        }
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
        let total = if usage.total_tokens > 0 {
            usage.total_tokens
        } else {
            usage.prompt_tokens.saturating_add(usage.completion_tokens)
        };
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
    let mut text = String::new();
    text.push_str(&result.content);
    if let Some(reasoning) = &result.reasoning {
        text.push_str(reasoning);
    }
    for call in &result.tool_calls {
        text.push_str(&call.function.name);
        text.push_str(&call.function.arguments);
    }
    overflow::estimate_tokens(&text)
}

fn extract_persistable_tool_report(tool_name: &str, output: &str) -> Option<String> {
    let field = match tool_name {
        "load_tools" => return compact_loaded_tools_report(output),
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
        .filter(|report| !report.is_empty())
}

fn compact_loaded_tools_report(output: &str) -> Option<String> {
    let value = serde_json::from_str::<Value>(output).ok()?;
    let names = value
        .get("loaded_tools")
        .and_then(Value::as_array)?
        .iter()
        .filter_map(|item| item.get("name").and_then(Value::as_str))
        .map(str::to_string)
        .collect::<Vec<_>>();
    if names.is_empty() {
        return None;
    }
    serde_json::to_string(&serde_json::json!({ "loaded_tools": names })).ok()
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

fn tool_names_arg(arguments: &str) -> Vec<String> {
    serde_json::from_str::<Value>(arguments)
        .ok()
        .and_then(|args| args.get("names").and_then(Value::as_array).cloned())
        .unwrap_or_default()
        .into_iter()
        .filter_map(|value| value.as_str().map(str::trim).map(str::to_string))
        .filter(|value| !value.is_empty())
        .collect()
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
    Some(ClipboardImage { mime, data })
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

fn with_current_time(system_prompt: String, mode: AgentMode) -> String {
    let cwd = std::env::current_dir()
        .map(|path| path.display().to_string())
        .unwrap_or_else(|_| "unknown".to_string());
    let runtime = terminal_runtime_context();
    format!(
        "{system_prompt}\n\n<system-reminder>\n当前系统时间：{}。用户询问当前时间时，优先使用这里的时间，不需要调用命令查询。\n当前工作目录：{cwd}。涉及相对路径、当前项目、文件操作时优先以此为准。\n{runtime}\n</system-reminder>\n\n{}",
        Local::now().format("%Y年%m月%d日 %H:%M"),
        mode.reminder()
    )
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
    if crate::i18n::is_zh() {
        format!("当前运行环境：{environment}。当前 shell：{shell}。当前终端标识：{terminal}。")
    } else {
        format!("Current runtime environment: {environment}. Current shell: {shell}. Terminal identifiers: {terminal}.")
    }
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
    use crate::llm::Usage;

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
        let big_msg = ChatMessage::plain("user", &"x".repeat(4_000));
        let small_msg = ChatMessage::plain("user", "hi");
        assert!(check.check_estimate(&[big_msg]));
        assert!(!check.check_estimate(&[small_msg]));
    }
}
