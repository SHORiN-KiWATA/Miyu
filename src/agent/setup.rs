//! Agent 的构造与逐回合配置。
//!
//! `set_*` 那一族是「这一回合的外部条件」：平台身份、上下文图片、记忆开关。
//! 分开设而不是塞进构造函数，是因为它们来自不同的调用方，而且不是每回合都有。
//!
//! `start_cache_keepalive` 定期发极小请求续上供应商的前缀缓存——缓存有 TTL，
//! 过期就从 token 0 重算。

use crate::agent::*;
use crate::config::PersonaManifest;

/// 会按会话状态增删的工具(dev 专用):建表时判据还不存在,原件抓在
/// `Agent` 上,每回合由 `apply_situational_tools` 决定挂不挂。
const SITUATIONAL_TOOLS: &[&str] = &["load_tools"];

fn situational_tool_specs(
    tools: &ToolRegistry,
    mode: AgentMode,
) -> Vec<Arc<crate::tools::ToolSpec>> {
    if mode != AgentMode::Dev {
        return Vec::new();
    }
    SITUATIONAL_TOOLS
        .iter()
        .filter_map(|name| tools.shared(name))
        .collect()
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
        // dev 走保留人格 "dev" 的作用域:记忆整套在这里关掉(09-09),派生
        // 目录也随之隔离,免得日后重开时读到默认人格的库。
        let config = if mode == AgentMode::Dev {
            config.dev_scoped()
        } else {
            config
        };
        // claude-code 中转的双四档工具作用域按会话模式裁决;其他协议无感。
        let client = client.with_claude_code_dev_mode(mode == AgentMode::Dev);
        // opencode Zen 的会话头按这个走:一次对话对应服务端一个会话。
        let client = client.with_zen_session(&state.session_id());
        let base_system_prompt = mode_system_prompt(
            &config,
            paths,
            mode,
            prompt_audience,
            user_profile_applies(prompt_audience, false),
        )?;
        let system_prompt = with_memory_preamble(
            with_host_environment(
                with_mode_reminder(base_system_prompt, mode),
                prompt_audience,
                paths,
                &config,
                mode,
                false,
            ),
            PersonaManifest::load(&config, paths, &config.active_persona_scope())
                .memory_enabled(&config),
        );
        let tools_enabled = config.tools.enabled;
        let max_tool_rounds = config.tools.max_rounds;
        // Dev 无人格:预设对话整套跳过。
        let preset_dialogs = if mode == AgentMode::Dev {
            Vec::new()
        } else {
            persona_hint::load_dialogs(&config, paths, &config.active_persona_scope())
        };
        let situational_tools = situational_tool_specs(&tools, mode);
        // 会话标记与 memory_origin 同源:日记记的和事实记的必须是同一个
        // 会话,否则会话级重置只清得掉一半。
        let memory_origin = MemoryOrigin::local(state.session_id().to_string());
        let memory = MemoryStore::new(&config, paths).with_session_id(&memory_origin.session_id);
        // 记忆关着就不建库(dev 走这条):`init`/`identity` 都会顺手创建
        // 库文件并跑一次衰减,而这条路上没有任何东西会去读它。库身份只被
        // 写日记/redo 的一致性校验用,那两处在关闭时先一步早退。
        // 记忆子系统按 persona 清单构造:清单关着就不建库、不注入、不写日记。
        let manifest = PersonaManifest::load(&config, paths, &config.active_persona_scope());
        let memory_enabled = manifest.memory_enabled(&config);
        let (memory_database_id, memory_generation) = if memory_enabled {
            memory.init()?;
            memory.identity()?
        } else {
            (String::new(), 0)
        };
        let on_overflow = config.context.on_overflow.clone();
        Ok(Self {
            state,
            client,
            system_prompt,
            runtime_system_context: Vec::new(),
            turn_system_context: Vec::new(),
            system_prompt_override: None,
            context_window_override: None,
            memory_content: None,
            suppress_session_history: false,
            trim_at_ratio: config.context.trim_at_ratio,
            trim_batch_ratio: config.context.trim_batch_ratio,
            tools_enabled,
            max_tool_rounds,
            tools: Arc::new(Mutex::new(tools)),
            situational_tools,
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
        // (档案进不进由 user_profile_applies 决定:平台回合不进。)
        let mode_prompt = mode_system_prompt(
            &self.config,
            &self.paths,
            self.mode,
            self.prompt_audience,
            user_profile_applies(self.prompt_audience, self.platform_context.is_some()),
        )?;
        {
            // 指纹永远按人格/模式提示词算,不看整体替换的覆盖:覆盖是回合级
            // 瞬态,进指纹会让每个带覆盖的回合都翻转一次指纹文件。
            let fingerprint_prompt = match self.mode {
                AgentMode::Dev => mode_prompt.clone(),
                AgentMode::Normal => self.config.base_system_prompt(&self.paths)?,
            };
            let compatible_previous = matches!(self.prompt_audience, PromptAudience::Owner)
                .then_some(mode_prompt.as_str());
            self.state.reset_if_prompt_changed_with_compatible(
                &fingerprint_prompt,
                compatible_previous,
            )?;
            self.state.recover_stale_turns()?;
        }
        self.system_prompt = self.assemble_system_prompt(mode_prompt);
        self.apply_situational_tools();
        Ok(())
    }

    /// 情境化工具的回合级增删(dev 专用,09-09)。
    ///
    /// 判据要会话状态,而工具表在 daemon 启动时就建好了,只能在每回合装配
    /// 时决定。工具目录是缓存前缀的一部分,改它=整条前缀作废,所以判据
    /// 必须挑在「本轮前缀反正已经断了」的时刻才翻转。
    ///
    /// `load_tools`:full 档下模型压根看不见它,也就不会调;留着的唯一理由
    /// 是历史里已有调用记录(会话中途从需加载模型切过来)时它不能变成未知
    /// 工具——而换档本身就换掉了整个工具面。
    ///
    /// 摘掉的工具原件留在 `situational_tools` 里:REPL 的 Agent 跨回合复用,
    /// 判据翻真时得原样放回去,而 spec 里裹着闭包,重建不如留原件。
    fn apply_situational_tools(&self) {
        if self.mode != AgentMode::Dev || self.situational_tools.is_empty() {
            return;
        }
        let stub_mode =
            tools::is_stub_loading_mode(&tools::effective_tools_loading_mode(&self.config));
        // 判据取不到就当「保留」:少一件工具只是省字节,凭空少一件却会让
        // 模型照着历史撞未知工具,两种错的代价不对称。
        let session_loaded_anything = self
            .state
            .load_session_loaded_tools()
            .map(|loaded| !loaded.is_empty())
            .unwrap_or(true);
        let mut registry = self.tools.lock().unwrap();
        for spec in &self.situational_tools {
            let keep = match spec.name.as_str() {
                "load_tools" => stub_mode || session_loaded_anything,
                _ => true,
            };
            if keep {
                registry.register_shared(spec.clone());
            } else {
                registry.unregister(&spec.name);
            }
        }
    }

    /// 人格/模式提示词之上的固定叠加顺序(顺序即缓存前缀,见 prompt 模块头)。
    /// 有整体替换覆盖时:覆盖文本顶掉模式提示词、模式提醒与属主主机环境块;
    /// 运行时追加段与记忆前言照旧。
    fn assemble_system_prompt(&self, mode_prompt: String) -> String {
        match &self.system_prompt_override {
            Some(override_prompt) => with_memory_preamble(
                with_runtime_system_context(override_prompt.clone(), &self.runtime_system_context),
                self.config.memory_config().enabled,
            ),
            None => with_memory_preamble(
                with_host_environment(
                    with_runtime_system_context(
                        with_mode_reminder(mode_prompt, self.mode),
                        &self.runtime_system_context,
                    ),
                    self.prompt_audience,
                    &self.paths,
                    &self.config,
                    self.mode,
                    self.platform_context.is_some(),
                ),
                self.config.memory_config().enabled,
            ),
        }
    }

    /// 程序驱动 CLI 的整体替换提示词;`prepare_for_turn` 之前调用才生效。
    pub(crate) fn set_system_prompt_override(&mut self, prompt: String) {
        let prompt = prompt.trim().to_string();
        self.system_prompt_override = (!prompt.is_empty()).then_some(prompt);
    }

    /// 程序驱动 CLI 的本回合上下文窗口;0 视作不覆盖。
    pub(crate) fn set_context_window_override(&mut self, window: usize) {
        self.context_window_override = (window > 0).then_some(window);
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
        self.memory.set_session_id(&origin.session_id);
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
        let mode_prompt = mode_system_prompt(
            &self.config,
            &self.paths,
            self.mode,
            self.prompt_audience,
            user_profile_applies(self.prompt_audience, self.platform_context.is_some()),
        )?;
        self.system_prompt = self.assemble_system_prompt(mode_prompt);
        Ok(())
    }

    pub fn mode(&self) -> AgentMode {
        self.mode
    }

    pub fn context_window(&self) -> Option<usize> {
        if let Some(window) = self.context_window_override {
            return Some(window);
        }
        self.client.context_window(&self.config).ok().flatten()
    }

    /// 上面那个数是不是猜的。猜的时候 footer 不能拿它算百分比。
    pub fn context_window_assumed(&self) -> bool {
        if self.context_window_override.is_some() {
            return false;
        }
        matches!(
            self.client
                .context_window_with_source(&self.config)
                .ok()
                .flatten(),
            Some((_, crate::config::ContextWindowSource::Assumed))
        )
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
        // 情境化工具的原件跟着新表走:两张表是分别建的,拿旧表的 Arc 去
        // 新表上放回,等于把上一模式的工具塞进这一模式。
        self.situational_tools = situational_tool_specs(&tools, mode);
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
        self.memory = MemoryStore::new(&self.config, &self.paths)
            .with_request_context(access, writer_principal, writer_display_name)
            .with_session_id(&self.memory_origin.session_id);
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

/// 属主/成员档案进不进系统提示词:终端(Owner)与 WebUI(External 且没有平台
/// 上下文)进;QQ 等平台回合与内部回合不进。
pub(in crate::agent) fn user_profile_applies(
    audience: PromptAudience,
    platform_turn: bool,
) -> bool {
    !platform_turn && matches!(audience, PromptAudience::Owner | PromptAudience::External)
}
