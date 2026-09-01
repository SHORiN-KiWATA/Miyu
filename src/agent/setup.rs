//! Agent 的构造与逐回合配置。
//!
//! `set_*` 那一族是「这一回合的外部条件」：平台身份、上下文图片、记忆开关。
//! 分开设而不是塞进构造函数，是因为它们来自不同的调用方，而且不是每回合都有。
//!
//! `start_cache_keepalive` 定期发极小请求续上供应商的前缀缓存——缓存有 TTL，
//! 过期就从 token 0 重算。

use crate::agent::*;

impl Agent {
    pub fn new(
        config: AppConfig,
        paths: &MiyuPaths,
        state: StateStore,
        client: OpenAiCompatibleClient,
        tools: ToolRegistry,
        mode: AgentMode,
    ) -> Result<Self> {
        Self::new_for_audience(
            config,
            paths,
            state,
            client,
            tools,
            mode,
            PromptAudience::Owner,
        )
    }

    pub(crate) fn new_for_audience(
        config: AppConfig,
        paths: &MiyuPaths,
        state: StateStore,
        client: OpenAiCompatibleClient,
        tools: ToolRegistry,
        mode: AgentMode,
        prompt_audience: PromptAudience,
    ) -> Result<Self> {
        // Construction is side-effect free (aside from idempotent memory
        // init) so concurrent turns can each build their own Agent; startup
        // maintenance (prompt-change reset, stale-turn recovery) lives in
        // `prepare_for_turn`.
        // dev 有自己的记忆/技能(=切人格语义):把 config 的人格指针换成
        // 保留人格 "dev",此后 MemoryStore/skills 派生目录全部随之隔离。
        let config = if mode == AgentMode::Dev {
            config.dev_scoped()
        } else {
            config
        };
        // claude-code 中转的双四档工具作用域按会话模式裁决;其他协议无感。
        let client = client.with_claude_code_dev_mode(mode == AgentMode::Dev);
        let base_system_prompt = mode_system_prompt(&config, paths, mode, prompt_audience)?;
        let system_prompt = with_memory_preamble(
            with_host_environment(
                with_mode_reminder(base_system_prompt, mode),
                prompt_audience,
                paths,
                mode,
            ),
            config.memory_config().enabled,
        );
        let tools_enabled = config.tools.enabled;
        let max_tool_rounds = config.tools.max_rounds;
        // Dev 无人格:预设对话整套跳过。
        let preset_dialogs = if mode == AgentMode::Dev {
            Vec::new()
        } else {
            persona_hint::load_dialogs(&config, paths, &config.active_persona_scope())
        };
        let memory = MemoryStore::new(&config, paths);
        memory.init()?;
        let (memory_database_id, memory_generation) = memory.identity()?;
        let memory_origin = MemoryOrigin::local(state.session_id().to_string());
        let on_overflow = config.context.on_overflow.clone();
        Ok(Self {
            state,
            client,
            system_prompt,
            runtime_system_context: Vec::new(),
            turn_system_context: Vec::new(),
            memory_content: None,
            suppress_session_history: false,
            trim_at_ratio: config.context.trim_at_ratio,
            trim_batch_ratio: config.context.trim_batch_ratio,
            tools_enabled,
            max_tool_rounds,
            tools: Arc::new(Mutex::new(tools)),
            memory,
            memory_organizer: None,
            memory_origin,
            memory_database_id,
            memory_generation,
            mode,
            prompt_audience,
            config,
            paths: paths.clone(),
            on_overflow,
            turn_display_content: None,
            attachment_run_id: None,
            image_platform: None,
            image_platform_label: None,
            platform_context: None,
            context_images: Vec::new(),
            context_files: Vec::new(),
            persona_reminder: None,
            preset_dialogs,
            last_request_snapshot: None,
            pending_remote_tool_calls: std::sync::Mutex::new(Vec::new()),
            last_request_endpoint: None,
            keepalive_cancel: None,
            consecutive_compacts: std::sync::atomic::AtomicU32::new(0),
            compact_stuck: std::sync::atomic::AtomicBool::new(false),
            last_compact_max_seq: std::sync::atomic::AtomicI64::new(-1),
            rapid_compacts: std::sync::atomic::AtomicU32::new(0),
            soft_notice_sent: std::sync::atomic::AtomicBool::new(false),
            spinner_interval: crate::render::wait_spinner::SPINNER_INTERVAL,
        })
    }

    /// daemon 内跑的回合调用：SpinnerTick 出不了进程（event_map 丢弃，
    /// REPL/一次性会话的动画由 CLI 本地定时器驱动），只剩 journal 尾部
    /// 冲刷的兜底作用，降到 200ms。终端直连（CLI direct）不要调，动画
    /// 帧率靠 40ms。
    pub(crate) fn with_headless_pacing(mut self) -> Self {
        self.spinner_interval = std::time::Duration::from_millis(200);
        self
    }

    /// Stops the idle cache-keepalive loop (called whenever a new request is
    /// about to change the context, and before dropping the agent).
    /// 测试用：塞一份请求快照，好让 `start_cache_keepalive` 真的起得来
    /// （它没有快照就直接返回）。
    #[cfg(test)]
    pub(in crate::agent) fn seed_request_snapshot_for_test(&mut self) {
        self.last_request_snapshot = Some((vec![ChatMessage::system("probe")], Vec::new()));
    }

    /// 测试用：拿到取消标志，好在 `Agent` 被丢掉之后验证它确实被翻了。
    #[cfg(test)]
    pub(in crate::agent) fn keepalive_cancel_flag(
        &self,
    ) -> Option<Arc<std::sync::atomic::AtomicBool>> {
        self.keepalive_cancel.clone()
    }

    pub fn cancel_cache_keepalive(&mut self) {
        if let Some(cancel) = self.keepalive_cancel.take() {
            cancel.store(true, std::sync::atomic::Ordering::Release);
        }
    }

    /// Starts the idle keepalive loop for the last request prefix. No-op when
    /// disabled or when no snapshot exists.
    pub(in crate::agent) fn start_cache_keepalive(&mut self) {
        self.cancel_cache_keepalive();
        let interval = self.config.cache.keepalive_seconds;
        if interval == 0 {
            return;
        }
        let Some((messages, tools)) = self.last_request_snapshot.clone() else {
            return;
        };
        let endpoint_hint = self.last_request_endpoint.clone();
        let max_pings = self.config.cache.keepalive_max_pings;
        let cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
        self.keepalive_cancel = Some(cancel.clone());
        let client = self.client.clone();
        let state = self.state.clone();
        let usage_source = self.usage_source().to_string();
        tokio::spawn(async move {
            for ping in 0..max_pings {
                tokio::time::sleep(std::time::Duration::from_secs(interval)).await;
                if cancel.load(std::sync::atomic::Ordering::Acquire) {
                    return;
                }
                match client
                    .cache_keepalive(messages.clone(), tools.clone(), endpoint_hint.as_ref())
                    .await
                {
                    Ok(Some(usage)) => {
                        tracing::info!(
                            ping = ping + 1,
                            prompt_tokens = usage.prompt_tokens,
                            cache_read = usage.cache_read_tokens,
                            "cache keepalive ping"
                        );
                        let meta = crate::state::UsageMeta {
                            source: &usage_source,
                            provider: Some(client.provider_id()),
                            model: None,
                            kind: None,
                        };
                        let _ = state.add_auxiliary_usage(&usage, meta);
                    }
                    Ok(None) => return, // protocol without keepalive support
                    Err(error) => {
                        tracing::warn!(error = %error, "cache keepalive ping failed");
                        return;
                    }
                }
            }
        });
    }

    pub(in crate::agent) fn usage_source(&self) -> &str {
        self.platform_context
            .as_ref()
            .map(|context| context.conversation.platform.as_str())
            .unwrap_or("agent")
    }

    pub fn prepare_for_turn(&mut self) -> Result<()> {
        let effective_system_prompt =
            mode_system_prompt(&self.config, &self.paths, self.mode, self.prompt_audience)?;
        {
            let fingerprint_prompt = match self.mode {
                AgentMode::Dev => effective_system_prompt.clone(),
                AgentMode::Normal => self.config.base_system_prompt(&self.paths)?,
            };
            let compatible_previous = matches!(self.prompt_audience, PromptAudience::Owner)
                .then_some(effective_system_prompt.as_str());
            self.state.reset_if_prompt_changed_with_compatible(
                &fingerprint_prompt,
                compatible_previous,
            )?;
            self.state.recover_stale_turns()?;
            self.maybe_cold_resume_prune()?;
        }
        self.system_prompt = with_memory_preamble(
            with_host_environment(
                with_runtime_system_context(
                    with_mode_reminder(effective_system_prompt, self.mode),
                    &self.runtime_system_context,
                ),
                self.prompt_audience,
                &self.paths,
                self.mode,
            ),
            self.config.memory_config().enabled,
        );
        Ok(())
    }

    pub fn set_runtime_system_context(&mut self, context: Vec<String>) -> Result<()> {
        self.runtime_system_context = context
            .into_iter()
            .map(|item| item.trim().to_string())
            .filter(|item| !item.is_empty())
            .collect();
        self.refresh_system_prompt()
    }

    /// Per-message transport blocks that ride the turn tail (after the user
    /// message) instead of the system prompt. No prompt refresh needed: they
    /// are consumed at message-assembly time.
    /// Raw input for the memory diary; `None` falls back to the turn content.
    pub fn set_memory_content(&mut self, content: Option<String>) {
        self.memory_content = content.filter(|text| !text.trim().is_empty());
    }

    pub fn set_turn_system_context(&mut self, context: Vec<String>) {
        self.turn_system_context = context
            .into_iter()
            .map(|item| item.trim().to_string())
            .filter(|item| !item.is_empty())
            .collect();
    }

    pub(crate) fn set_memory_writes_enabled(&mut self, enabled: bool) {
        self.memory.set_writes_enabled(enabled);
    }

    pub(crate) fn set_memory_organizer(&mut self, organizer: MemoryOrganizerHandle) {
        self.memory_organizer = Some(organizer);
    }

    pub(crate) fn set_memory_origin(&mut self, origin: MemoryOrigin) {
        self.memory_origin = origin;
    }

    pub(crate) fn set_memory_request_context(
        &mut self,
        access: MemoryAccess,
        writer_principal: Option<String>,
        writer_display_name: impl Into<String>,
    ) {
        self.memory
            .set_request_context(access, writer_principal, writer_display_name);
    }

    pub(crate) fn set_image_platform(&mut self, platform: &str, display_name: &str) {
        let platform = platform
            .chars()
            .filter(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_'))
            .collect::<String>();
        self.image_platform = (!platform.is_empty()).then_some(platform);
        self.image_platform_label = self.image_platform.as_ref().and_then(|_| {
            (!display_name.trim().is_empty()).then(|| display_name.trim().to_string())
        });
    }

    pub(crate) fn set_platform_context_images(
        &mut self,
        context: Arc<PlatformTurnContext>,
        images: Vec<PlatformContextImageRef>,
    ) {
        self.platform_context = Some(context);
        self.context_images = images;
    }

    pub(crate) fn set_platform_context_files(
        &mut self,
        context: Arc<PlatformTurnContext>,
        files: Vec<PlatformContextFileRef>,
    ) {
        self.platform_context = Some(context.clone());
        self.context_files = files.clone();
        if self.tools_enabled {
            let mut tools = self.tools.lock().unwrap();
            crate::platforms::file_reader::register(&mut tools, context, files);
        }
    }

    pub fn set_turn_persistence(
        &mut self,
        display_content: String,
        attachment_run_id: Option<String>,
    ) {
        self.turn_display_content = Some(display_content);
        self.attachment_run_id = attachment_run_id;
    }

    pub fn set_session_history_suppressed(&mut self, suppressed: bool) {
        self.suppress_session_history = suppressed;
    }

    /// Rebuilds the system prompt for the current mode without running
    /// turn-entry maintenance. Used for mid-turn mode switches, where
    /// `reset_if_prompt_changed` must never fire (it would wipe the very
    /// turn that is running).
    pub(in crate::agent) fn refresh_system_prompt(&mut self) -> Result<()> {
        let base_system_prompt =
            mode_system_prompt(&self.config, &self.paths, self.mode, self.prompt_audience)?;
        self.system_prompt = with_memory_preamble(
            with_host_environment(
                with_runtime_system_context(
                    with_mode_reminder(base_system_prompt, self.mode),
                    &self.runtime_system_context,
                ),
                self.prompt_audience,
                &self.paths,
                self.mode,
            ),
            self.config.memory_config().enabled,
        );
        Ok(())
    }

    pub fn mode(&self) -> AgentMode {
        self.mode
    }

    pub fn context_window(&self) -> Option<usize> {
        self.client.context_window(&self.config).ok().flatten()
    }

    /// 上面那个数是不是猜的。猜的时候 footer 不能拿它算百分比。
    pub fn context_window_assumed(&self) -> bool {
        matches!(
            self.client
                .context_window_with_source(&self.config)
                .ok()
                .flatten(),
            Some((_, crate::config::ContextWindowSource::Assumed))
        )
    }

    pub fn effective_context_tokens(&self) -> Result<u64> {
        let (messages, _) = self.chat_messages("", "")?;
        let mut tokens = overflow::estimate_messages_tokens(&messages) as u64;
        if self.tools_enabled {
            tokens = tokens.saturating_add(self.tool_definition_tokens() as u64);
        }
        Ok(tokens)
    }

    /// Session-scoped lifetime token total (Σ in the footer): keeps growing
    /// across compactions, resets to zero with the session history. The old
    /// global usage.json figure lives on in /usage as the global overview.
    pub fn conversation_usage_tokens(&self) -> Result<u64> {
        self.state.session_cumulative_tokens()
    }

    /// Same Σ with the prompt and cache-read halves its cache rate needs.
    pub fn conversation_usage_token_totals(&self) -> Result<TurnTokens> {
        self.state.session_cumulative_token_totals()
    }

    pub(in crate::agent) fn tool_definition_tokens(&self) -> usize {
        let tools = self.tools.lock().unwrap();
        let definitions =
            if tools::is_stub_loading_mode(&tools::effective_tools_loading_mode(&self.config)) {
                tools.stub_definitions()
            } else {
                tools.definitions()
            };
        estimate_tool_definition_tokens(&definitions)
    }

    pub fn switch_mode(&mut self, mode: AgentMode, tools: ToolRegistry) {
        self.mode = mode;
        self.tools = Arc::new(Mutex::new(tools));
        // 预设对话跟人格走:Normal↔Dev 切换后必须重算,否则 Dev 带着
        // 人格 dialogs(违反"Dev 无人格"),Dev→Normal 则永远没有。
        self.refresh_preset_dialogs();
    }

    pub(in crate::agent) fn refresh_preset_dialogs(&mut self) {
        // Dev 无人格:预设对话整套跳过(与构造期一致)。
        self.preset_dialogs = if self.mode == AgentMode::Dev {
            Vec::new()
        } else {
            persona_hint::load_dialogs(
                &self.config,
                &self.paths,
                &self.config.active_persona_scope(),
            )
        };
    }

    pub fn replace_client(&mut self, client: OpenAiCompatibleClient) {
        self.client = client;
    }

    pub(crate) fn cloned_client(&self) -> OpenAiCompatibleClient {
        self.client.clone()
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
        let (access, writer_principal, writer_display_name) = self.memory.request_context();
        self.memory = MemoryStore::new(&self.config, &self.paths).with_request_context(
            access,
            writer_principal,
            writer_display_name,
        );
        self.memory.init()?;
        (self.memory_database_id, self.memory_generation) = self.memory.identity()?;
        self.refresh_preset_dialogs();
        self.prepare_for_turn()
    }

    /// 平台(QQ 等)回合的工具轮数上限(platforms.max_tool_rounds,默认 32,
    /// 0=不限):平台回合失控时没人守在终端里按停——真机 web_search 同
    /// query 222 连就是这么烧起来的。
    pub fn cap_tool_rounds_for_platform(&mut self) {
        let cap = self.config.platforms.max_tool_rounds;
        if cap > 0 {
            self.max_tool_rounds = cap;
        }
    }
}
