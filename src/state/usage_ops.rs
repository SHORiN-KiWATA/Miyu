//! 用量的累计、落盘与统计。
//!
//! 用量分两条：当前回合的（内存里累加）和历史的（落盘）。子代理的用量要算进
//! 发起它的会话（`record_subagent_usage`），否则「这次对话花了多少」是错的。

use crate::state::*;

impl StateStore {
    #[allow(clippy::too_many_arguments)]
    pub fn record_subagent_usage(
        &self,
        session_id: &str,
        provider_id: Option<&str>,
        model: Option<&str>,
        context_window: Option<i64>,
        prompt_tokens: i64,
        completion_tokens: i64,
        total_tokens: i64,
        cache_read_tokens: i64,
    ) -> Result<()> {
        self.conv_db.record_subagent_usage(
            session_id,
            provider_id,
            model,
            context_window,
            prompt_tokens,
            completion_tokens,
            total_tokens,
            cache_read_tokens,
        )
    }

    pub fn reset_conversation_usage(&self) -> Result<()> {
        usage::reset_conversation(&self.usage_file())
    }

    pub fn add_usage(&self, usage: &Usage, meta: UsageMeta<'_>) -> Result<()> {
        self.init_files()?;
        usage::add_usage(&self.usage_file(), usage)?;
        self.record_usage_history(usage, meta, false);
        Ok(())
    }

    pub fn add_auxiliary_usage(&self, usage: &Usage, meta: UsageMeta<'_>) -> Result<()> {
        self.init_files()?;
        usage::add_auxiliary_usage(&self.usage_file(), usage)?;
        self.record_usage_history(usage, meta, true);
        Ok(())
    }

    /// 历史明细落账失败只告警:usage.json 累计是正账,明细缺一行不该
    /// 让整个回合报错。
    pub(crate) fn record_usage_history(&self, usage: &Usage, meta: UsageMeta<'_>, aux: bool) {
        if let Err(error) = usage::record_usage_for_account(
            &self.usage_history_file(),
            usage,
            meta,
            aux,
            &self.usage_account,
        ) {
            tracing::warn!(error = %error, "recording usage history failed");
        }
    }

    /// 清空逐次调用明细(usage-history.jsonl)。累计正账 usage.json 不动——
    /// 那是"一生用了多少"的唯一来源,统计页要的只是明细派生的图表与记录。
    pub fn clear_usage_history(&self) -> Result<()> {
        let path = self.usage_history_file();
        if path.exists() {
            std::fs::remove_file(&path)
                .with_context(|| format!("clearing usage history {}", path.display()))?;
        }
        Ok(())
    }

    pub fn usage_history_file(&self) -> PathBuf {
        self.state_dir.join(usage::USAGE_HISTORY_FILE)
    }

    /// 供应商改名后同步用量账本;见 [`usage::rename_provider`]。
    pub fn rename_usage_provider(&self, old: &str, new: &str) -> Result<usize> {
        usage::rename_provider(&self.usage_history_file(), old, new)
    }

    /// `config` 提供时按 models.dev 单价做计费估算;None 则费用字段全零。
    pub fn usage_stats(
        &self,
        range: UsageRange,
        config: Option<&crate::config::AppConfig>,
    ) -> Result<usage::UsageStats> {
        self.usage_stats_for_account(range, config, None)
    }

    /// `account` 为 Some 时只统计该账号(空串 = 管理员/遗留);None = 全部并按人拆分。
    pub fn usage_stats_for_account(
        &self,
        range: UsageRange,
        config: Option<&crate::config::AppConfig>,
        account: Option<&str>,
    ) -> Result<usage::UsageStats> {
        let path = self.usage_history_file();
        match config {
            Some(config) => {
                let price = crate::models_cache::pricing_resolver(config);
                usage::usage_stats_for_account(&path, range, &price, account)
            }
            None => usage::usage_stats_for_account(&path, range, &|_, _| None, account),
        }
    }

    pub fn usage_details(
        &self,
        limit: usize,
        src: Option<&str>,
        model: Option<&str>,
        config: Option<&crate::config::AppConfig>,
    ) -> Result<Vec<usage::UsageRecord>> {
        self.usage_details_for_account(limit, src, model, config, None)
    }

    pub fn usage_details_for_account(
        &self,
        limit: usize,
        src: Option<&str>,
        model: Option<&str>,
        config: Option<&crate::config::AppConfig>,
        account: Option<&str>,
    ) -> Result<Vec<usage::UsageRecord>> {
        let path = self.usage_history_file();
        match config {
            Some(config) => {
                let price = crate::models_cache::pricing_resolver(config);
                usage::usage_details_for_account(&path, limit, src, model, &price, account)
            }
            None => {
                usage::usage_details_for_account(&path, limit, src, model, &|_, _| None, account)
            }
        }
    }

    #[allow(dead_code)]
    pub fn usage_snapshot(&self) -> Result<UsageSnapshot> {
        usage::snapshot(&self.usage_file())
    }

    /// Lifetime token total of the current session (survives compaction,
    /// zeroed by /reset). This is the Σ shown in the REPL/WebUI footer.
    pub fn session_cumulative_tokens(&self) -> Result<u64> {
        self.conv_db.session_token_total(&self.session())
    }

    /// Same Σ, plus the prompt and cache-read halves the cumulative cache rate
    /// is computed from.
    pub fn session_cumulative_token_totals(&self) -> Result<TurnTokens> {
        self.conv_db.session_token_totals(&self.session())
    }

    pub fn clear_last_usage(&self) -> Result<()> {
        usage::clear_last_usage(&self.usage_file())
    }
}
