//! 配置的读写与版本迁移。
//!
//! `load` 之后一定跟着 `migrate` + `normalize_*` + `validate`：磁盘上的配置可能
//! 是任意历史版本，也可能被手工编辑过。归一化改的是**表示**（补默认、去重、
//! 修引用），校验只判定合不合法，两者不混。
//!
//! `display_language_hint` 单独存在是因为它要在完整加载之前就用上——报错信息本
//! 身也要按语言显示。

use crate::config::*;

impl AppConfig {
    pub fn display_language_hint(paths: &MiyuPaths) -> Option<String> {
        let raw = std::fs::read_to_string(&paths.config_file).ok()?;
        let stripped = json_comments::StripComments::new(raw.as_bytes());
        let value: serde_json::Value = serde_json::from_reader(stripped).ok()?;
        value
            .get("display")?
            .get("language")?
            .as_str()
            .map(str::to_string)
    }

    pub fn memory_config(&self) -> &MemoryConfig {
        if self.memory != MemoryConfig::default() {
            &self.memory
        } else {
            &self.plugins.memory
        }
    }

    pub fn load(paths: &MiyuPaths) -> Result<Self> {
        // Platform multimodal routes may rely on cached models.dev
        // capabilities. Load the full cache before validation; callers can
        // compact it to their active configuration afterwards.
        crate::models_cache::try_load(paths);
        let raw = std::fs::read_to_string(&paths.config_file)
            .with_context(|| format!("failed to read {}", paths.config_file.display()))?;
        let stripped = json_comments::StripComments::new(raw.as_bytes());
        let mut config: Self = serde_json::from_reader(stripped)
            .with_context(|| format!("invalid JSONC in {}", paths.config_file.display()))?;
        config.migrate()?;
        config.normalize_builtin_providers();
        config.normalize_api_quota_accounts();
        config.normalize_managed_output_paths(paths);
        config.normalize_platform_model_routes();
        config.validate()?;
        config.validate_persona_files(paths)?;
        Ok(config)
    }

    pub fn load_or_default(paths: &MiyuPaths) -> Result<Self> {
        if paths.config_file.exists() {
            Self::load(paths)
        } else {
            Ok(Self::default())
        }
    }

    pub fn init_files(paths: &MiyuPaths) -> Result<()> {
        paths.create_dirs()?;
        if !paths.config_file.exists() {
            Self::default().save(paths)?;
        }
        // Dev 模式提示词:一行、可编辑、不混淆(与 Miyu 人格提示词的内嵌
        // 不可编辑形成对照)。缺失时写默认;用户改成什么都以文件为准。
        let dev_prompt = paths.config_dir.join(DEV_PROMPT_FILE);
        if !dev_prompt.exists() {
            std::fs::write(&dev_prompt, format!("{DEFAULT_DEV_SYSTEM_PROMPT}\n"))?;
        }
        Ok(())
    }

    pub fn save(&self, paths: &MiyuPaths) -> Result<()> {
        let mut config = self.clone();
        config.migrate()?;
        config.normalize_api_quota_accounts();
        config.normalize_platform_model_routes();
        // Also on save, not just on load: a value healed only in memory is
        // rewritten stale on the next write, so the file never recovers.
        config.normalize_managed_output_paths(paths);
        let effective_memory = config.memory_config().clone();
        config.plugins.memory = effective_memory;
        config.memory = MemoryConfig::default();
        config.validate()?;
        paths.create_dirs()?;
        if let Some(prompt) = config.system_prompt.take() {
            let prompt_file = config.system_prompt_path(paths);
            if let Some(parent) = prompt_file.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let prompt = prompt.trim_end();
            let content = if prompt.is_empty() {
                String::new()
            } else {
                format!("{prompt}\n")
            };
            std::fs::write(prompt_file, content)?;
        }
        if config
            .system_prompt_file
            .as_deref()
            .map(str::trim)
            .unwrap_or_default()
            .is_empty()
        {
            config.system_prompt_file = Some("system-prompt.md".to_string());
        }
        let raw = serde_json::to_string_pretty(&config)?;
        // 原子写:写回瞬间断电/崩溃不能让 config.json 留下截断的半个 JSON。
        let config_dir = paths
            .config_file
            .parent()
            .context("config file has no parent directory")?;
        let temp = tempfile::NamedTempFile::new_in(config_dir)?;
        std::fs::write(temp.path(), format!("{raw}\n"))?;
        temp.persist(&paths.config_file)?;
        Ok(())
    }

    pub(crate) fn migrate(&mut self) -> Result<()> {
        if self.config_version > CURRENT_CONFIG_VERSION {
            bail!(
                "unsupported config version {}; maximum supported version is {}",
                self.config_version,
                CURRENT_CONFIG_VERSION
            );
        }
        if self.config_version < 1 {
            for provider in &mut self.providers {
                if (provider.temperature - LEGACY_DEFAULT_TEMPERATURE).abs() < f32::EPSILON {
                    provider.temperature = default_temperature();
                }
            }
        }
        // The embedding model used to live under the knowledge base, which is
        // where it was first needed. It now also backs memory recall, and a
        // knowledge-base setting silently steering group-chat search is a trap
        // for whoever reads this next.
        if !self.embedding.is_configured() {
            let kb = &self.plugins.knowledge_base;
            if !kb.embedding_provider_id.trim().is_empty() && !kb.embedding_model.trim().is_empty()
            {
                self.embedding.provider_id = kb.embedding_provider_id.trim().to_string();
                self.embedding.model = kb.embedding_model.trim().to_string();
                if kb.embedding_timeout_seconds > 0 {
                    self.embedding.timeout_seconds = kb.embedding_timeout_seconds;
                }
                self.embedding.min_score = kb.semantic_min_score;
            }
        }
        self.config_version = CURRENT_CONFIG_VERSION;
        Ok(())
    }

    pub(crate) fn normalize_builtin_providers(&mut self) {
        for provider in ProviderConfig::default_templates() {
            if !self.providers.iter().any(|item| {
                item.id == provider.id
                    || provider.id == OPENCODE_PROVIDER_ID && item.is_opencode_zen()
            }) {
                self.providers.push(provider);
            }
        }
        // Claude Code 恒置顶(用户拍板的列表次序);存量配置若排在后面搬到最前。
        if let Some(position) = self
            .providers
            .iter()
            .position(ProviderConfig::is_claude_code)
        {
            if position != 0 {
                let provider = self.providers.remove(position);
                self.providers.insert(0, provider);
            }
        }
        // Antigravity 紧随 Claude Code 之后(同样是内置 CLI 中转)。
        if let Some(position) = self
            .providers
            .iter()
            .position(ProviderConfig::is_antigravity)
        {
            let target = usize::from(self.providers[0].is_claude_code());
            if position != target && target < self.providers.len() {
                let provider = self.providers.remove(position);
                self.providers.insert(target, provider);
            }
        }
        if self.active_provider == "opencodezen" {
            self.active_provider = OPENCODE_PROVIDER_ID.to_string();
        }
        for provider in &mut self.providers {
            if provider.is_legacy_default_anthropic_model() {
                provider.models.clear();
                provider.default_model.clear();
            }
        }
        if let Some(active_models) = &mut self.active_provider_models {
            for active in active_models {
                if active.provider_id == "opencodezen" {
                    active.provider_id = OPENCODE_PROVIDER_ID.to_string();
                }
            }
        }
        if let Some(active_models) = &mut self.active_multimodal_provider_models {
            for active in active_models {
                if active.provider_id == "opencodezen" {
                    active.provider_id = OPENCODE_PROVIDER_ID.to_string();
                }
            }
        }
        self.platforms
            .rename_provider_references("opencodezen", OPENCODE_PROVIDER_ID);
        self.prune_stale_active_provider_models();
        self.normalize_platform_model_routes();
        if self.plugins.vision.vision_provider_id == "opencodezen" {
            self.plugins.vision.vision_provider_id = OPENCODE_PROVIDER_ID.to_string();
        }
        if self
            .provider(None)
            .map(|provider| provider.default_model.trim().is_empty())
            .unwrap_or(true)
        {
            self.active_provider = OPENCODE_PROVIDER_ID.to_string();
        }
        if self
            .active_provider_models
            .as_ref()
            .is_some_and(Vec::is_empty)
        {
            self.active_provider_models = Some(vec![ActiveProviderModelConfig {
                provider_id: OPENCODE_PROVIDER_ID.to_string(),
                model: OPENCODE_DEFAULT_CHAT_MODEL.to_string(),
            }]);
        }
    }

    pub(crate) fn normalize_api_quota_accounts(&mut self) {
        normalize_api_quota_provider(&mut self.plugins.api_quota.deepseek);
        normalize_api_quota_provider(&mut self.plugins.api_quota.openrouter);
    }

    pub(crate) fn normalize_managed_output_paths(&mut self, paths: &MiyuPaths) {
        let Some(base) = directories::BaseDirs::new() else {
            return;
        };
        let documents = directories::UserDirs::new()
            .and_then(|dirs| dirs.document_dir().map(PathBuf::from))
            .unwrap_or_else(|| base.home_dir().join("Documents"));
        let pictures = std::env::var_os("XDG_PICTURES_DIR")
            .map(PathBuf::from)
            .or_else(|| {
                directories::UserDirs::new().and_then(|dirs| dirs.picture_dir().map(PathBuf::from))
            })
            .unwrap_or_else(|| base.home_dir().join("Pictures"));
        // The XDG data root is a legacy root too: an upgrade that ran while
        // `data_dir` still pointed at `~/.local/share/miyu` remapped these
        // fields onto it and persisted the result, so the value we now have to
        // heal is one this function itself wrote.
        let legacy_data = base.data_dir().join("miyu");
        if let Some((from, to)) = remap_managed_output_dir(
            &mut self.plugins.deep_research.output_dir,
            &[
                documents.join("Miyu"),
                documents.join("miyu"),
                legacy_data.join("documents"),
            ],
            &paths.data_dir.join("documents"),
            base.home_dir(),
        ) {
            relocate_managed_output(&from, &to);
        }
        if let Some((from, to)) = remap_managed_output_dir(
            &mut self.plugins.image_generation.output_dir,
            &[
                pictures.join("miyu"),
                pictures.join("Miyu"),
                legacy_data.join("pictures"),
            ],
            &paths.data_dir.join("pictures"),
            base.home_dir(),
        ) {
            relocate_managed_output(&from, &to);
        }
    }

    pub(crate) fn prune_stale_active_provider_models(&mut self) {
        if let Some(active_models) = &mut self.active_provider_models {
            active_models.retain(|active| active_model_exists(&self.providers, active));
        }
        if let Some(active_models) = &mut self.active_multimodal_provider_models {
            active_models.retain(|active| active_model_supports_image(&self.providers, active));
        }
    }

    pub fn validate(&self) -> Result<()> {
        if crate::i18n::UiLanguage::parse(&self.display.language).is_none() {
            bail!(
                "{}",
                crate::i18n::text(
                    "display.language must be 'auto', 'en', or 'zh'",
                    "display.language 必须是 'auto'、'en' 或 'zh'"
                )
            );
        }
        if self.active_provider.trim().is_empty() {
            bail!("active_provider cannot be empty");
        }
        if self.providers.is_empty() {
            bail!("at least one provider is required");
        }
        let mut provider_ids = HashSet::with_capacity(self.providers.len());
        for provider in &self.providers {
            if provider.id.trim().is_empty() {
                bail!("provider id cannot be empty");
            }
            if provider.id.trim() != provider.id {
                bail!(
                    "provider id must not contain surrounding whitespace: {}",
                    provider.id
                );
            }
            if !provider_ids.insert(provider.id.as_str()) {
                bail!("duplicate provider id: {}", provider.id);
            }
            // Claude Code / Antigravity 特殊供应商的传输层是本机 CLI 子进程,
            // 没有 URL 概念。
            if provider.base_url.trim().is_empty() && !provider.is_builtin_cli_provider() {
                bail!("provider {} base_url cannot be empty", provider.id);
            }
        }
        if !(0.1..=1.0).contains(&self.context.trim_at_ratio) {
            bail!("context.trim_at_ratio must be between 0.1 and 1.0");
        }
        if !(0.1..=1.0).contains(&self.context.compact_force_ratio) {
            bail!("context.compact_force_ratio must be between 0.1 and 1.0");
        }
        if self.context.compact_force_ratio < self.context.trim_at_ratio {
            bail!("context.compact_force_ratio must be >= context.trim_at_ratio");
        }
        if !(0.05..=1.0).contains(&self.context.compact_soft_ratio)
            || !(0.05..=1.0).contains(&self.context.compact_snip_ratio)
        {
            bail!("context.compact_soft_ratio and compact_snip_ratio must be between 0.05 and 1.0");
        }
        if self.context.compact_soft_ratio > self.context.compact_snip_ratio
            || self.context.compact_snip_ratio > self.context.trim_at_ratio
        {
            bail!("context watermarks must be ordered: compact_soft_ratio <= compact_snip_ratio <= trim_at_ratio <= compact_force_ratio");
        }
        if !(0.01..=0.9).contains(&self.context.trim_batch_ratio) {
            bail!("context.trim_batch_ratio must be between 0.01 and 0.9");
        }
        match self.context.on_overflow.as_str() {
            "pop" | "compact" => {}
            value => bail!("context.on_overflow must be 'pop' or 'compact', got: {value}"),
        }
        if self.display.repl_replay_turns > MAX_REPL_REPLAY_TURNS {
            bail!("display.repl_replay_turns must be between 0 and {MAX_REPL_REPLAY_TURNS}");
        }
        if self.display.command_output_lines > MAX_COMMAND_OUTPUT_LINES {
            bail!("display.command_output_lines must be between 0 and {MAX_COMMAND_OUTPUT_LINES}");
        }
        if self.plugins.print_image.width_percent == 0
            || self.plugins.print_image.width_percent > 100
        {
            bail!("plugins.print_image.width_percent must be between 1 and 100");
        }
        if self.plugins.print_image.height_percent == 0
            || self.plugins.print_image.height_percent > 100
        {
            bail!("plugins.print_image.height_percent must be between 1 and 100");
        }
        if self.plugins.web.max_results == 0 {
            bail!("plugins.web.max_results must be greater than 0");
        }
        match self.plugins.deep_research.thinking_depth.as_str() {
            "minimal" | "low" | "medium" | "high" | "xhigh" => {}
            value => bail!("plugins.deep_research.thinking_depth is invalid: {value}"),
        }
        match self.plugins.image_generation.provider_type.as_str() {
            "openai" | "rightcode" => {}
            value => bail!("plugins.image_generation.provider_type is invalid: {value}"),
        }
        match self.plugins.image_generation.default_aspect_ratio.as_str() {
            "自动" | "1:1" | "2:3" | "3:2" | "3:4" | "4:3" | "4:5" | "5:4" | "9:16" | "16:9"
            | "21:9" => {}
            value => bail!("plugins.image_generation.default_aspect_ratio is invalid: {value}"),
        }
        match self.plugins.image_generation.default_resolution.as_str() {
            "1K" | "2K" | "4K" => {}
            value => bail!("plugins.image_generation.default_resolution is invalid: {value}"),
        }
        if self.plugins.image_generation.timeout_seconds == 0 {
            bail!("plugins.image_generation.timeout_seconds must be greater than 0");
        }
        if self.plugins.knowledge_base.max_search_results == 0 {
            bail!("plugins.knowledge_base.max_search_results must be greater than 0");
        }
        if self.plugins.knowledge_base.max_read_lines == 0 {
            bail!("plugins.knowledge_base.max_read_lines must be greater than 0");
        }
        if self.plugins.knowledge_base.max_file_size_kb == 0 {
            bail!("plugins.knowledge_base.max_file_size_kb must be greater than 0");
        }
        if self.plugins.knowledge_base.semantic_chunk_chars < 128 {
            bail!("plugins.knowledge_base.semantic_chunk_chars must be at least 128");
        }
        if self.plugins.knowledge_base.semantic_chunk_overlap
            >= self.plugins.knowledge_base.semantic_chunk_chars
        {
            bail!("plugins.knowledge_base.semantic_chunk_overlap must be smaller than semantic_chunk_chars");
        }
        if self.plugins.knowledge_base.semantic_top_k == 0 {
            bail!("plugins.knowledge_base.semantic_top_k must be greater than 0");
        }
        if self.plugins.knowledge_base.embedding_timeout_seconds == 0 {
            bail!("plugins.knowledge_base.embedding_timeout_seconds must be greater than 0");
        }
        if !(0.0..=2.0).contains(&self.provider(None)?.temperature) {
            bail!("provider temperature must be between 0.0 and 2.0");
        }
        for provider in &self.providers {
            if provider.timeout_seconds == 0 {
                bail!(
                    "provider {} timeout_seconds must be greater than 0",
                    provider.id
                );
            }
            if !(0.0..=2.0).contains(&provider.temperature) {
                bail!(
                    "provider {} temperature must be between 0.0 and 2.0",
                    provider.id
                );
            }
            if provider.anthropic_max_tokens == 0 {
                bail!(
                    "provider {} anthropic_max_tokens must be greater than 0",
                    provider.id
                );
            }
        }
        for provider in &self.providers {
            for (model, cost) in &provider.model_costs {
                if cost.input < 0.0
                    || cost.output < 0.0
                    || cost.cache_read.is_some_and(|price| price < 0.0)
                {
                    bail!(
                        "provider {} model {model} price must be non-negative",
                        provider.id
                    );
                }
            }
        }
        if !(0.0..=1.0).contains(&self.plugins.memes.auto_send_probability) {
            bail!("plugins.memes.auto_send_probability must be between 0.0 and 1.0");
        }
        if self.plugins.memes.width_percent == 0 || self.plugins.memes.width_percent > 100 {
            bail!("plugins.memes.width_percent must be between 1 and 100");
        }
        if self.plugins.memes.height_percent == 0 || self.plugins.memes.height_percent > 100 {
            bail!("plugins.memes.height_percent must be between 1 and 100");
        }
        if self.plugins.memes.search_max_results == 0 || self.plugins.memes.search_max_results > 3 {
            bail!("plugins.memes.search_max_results must be between 1 and 3");
        }
        let mem = self.memory_config();
        if mem.forgetting_half_life_days <= 0.0 {
            bail!("memory.forgetting_half_life_days must be greater than 0");
        }
        if mem.forget_after_days == 0 {
            bail!("memory.forget_after_days must be greater than 0");
        }
        if !(2..=100).contains(&mem.diary_batch_size) {
            bail!("memory.diary_batch_size must be between 2 and 100");
        }
        if !(1..=3650).contains(&mem.short_diary_retention_days) {
            bail!("memory.short_diary_retention_days must be between 1 and 3650");
        }
        if !(1..=100).contains(&mem.diary_promotion_recalls) {
            bail!("memory.diary_promotion_recalls must be between 1 and 100");
        }
        if !(5..=600).contains(&mem.organizer_timeout_seconds) {
            bail!("memory.organizer_timeout_seconds must be between 5 and 600");
        }
        if !(0.0..=1.0).contains(&self.plugins.knowledge_base.semantic_min_score) {
            bail!("plugins.knowledge_base.semantic_min_score must be between 0.0 and 1.0");
        }
        validate_api_quota_accounts("deepseek", &self.plugins.api_quota.deepseek)?;
        validate_api_quota_accounts("openrouter", &self.plugins.api_quota.openrouter)?;
        self.validate_model_references()?;
        self.validate_global_multimodal_config()?;
        self.validate_platforms()?;
        self.provider(None)?;
        Ok(())
    }

    pub(crate) fn validate_model_references(&self) -> Result<()> {
        if let Some(pool) = &self.active_provider_models {
            if pool.is_empty() {
                bail!("at least one model endpoint must remain active");
            }
            validate_unique_existing_pool(&self.providers, "active text", pool, false)?;
        }
        let kb_provider = self.plugins.knowledge_base.embedding_provider_id.trim();
        if !kb_provider.is_empty() {
            self.provider(Some(kb_provider))?;
        }
        Ok(())
    }

    pub(crate) fn validate_global_multimodal_config(&self) -> Result<()> {
        if let Some(pool) = &self.active_multimodal_provider_models {
            validate_unique_existing_pool(&self.providers, "active multimodal", pool, true)?;
        }
        if self.plugins.vision.enabled && !self.plugins.vision.vision_provider_id.trim().is_empty()
        {
            self.vision_provider_choice()?;
        }
        Ok(())
    }
}
