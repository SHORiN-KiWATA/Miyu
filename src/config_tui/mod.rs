mod antigravity_form;
mod claude_code_form;
mod cli_catalog;
mod codex_form;
mod personas;
mod platforms;
mod plugin_settings;
mod plugins;
mod providers;
mod quota;
mod real_context;
mod scheduled_messages;
mod settings;
mod undo;
mod widgets;
use antigravity_form::*;
use claude_code_form::*;
use codex_form::*;
use personas::*;
use platforms::*;
use plugin_settings::*;
use plugins::*;
use providers::*;
use quota::*;
use real_context::*;
use scheduled_messages::*;
use settings::*;
use undo::*;
use widgets::*;

use crate::config::{
    merge_group_join_approval_settings, merge_real_context_settings, ActiveProviderModelConfig,
    ApiQuotaAccountConfig, ApiQuotaProviderConfig, AppConfig, PlatformCommandPermission,
    PlatformConversationConfig, PlatformConversationKind, PlatformModelPoolInheritance,
    PlatformModelRoute, PlatformPersonaOverride, PlatformRateLimit, PlatformSessionLimits,
    ProviderConfig, ProviderModelChoice, QqGroupJoinApprovalGroupConfig,
    QqGroupJoinApprovalPluginSettings, QqMemeCollectorPluginSettings,
    QqMessageHistoryPluginSettings, RealContextIdentityMapping, RealContextPluginSettings,
    MAX_COMMAND_OUTPUT_LINES, MAX_PLATFORM_COMMAND_PREFIX_CHARS, MAX_PLATFORM_SESSION_QUEUED,
    MAX_PLATFORM_SESSION_RUNNING, MAX_REPL_REPLAY_TURNS, QQ_GROUP_JOIN_APPROVAL_PLUGIN_ID,
    QQ_MEME_COLLECTOR_PLUGIN_ID, QQ_MESSAGE_HISTORY_PLUGIN_ID, REAL_CONTEXT_PLUGIN_ID,
};
use crate::default_models::{OPENCODE_DEFAULT_VISION_MODEL, OPENCODE_PROVIDER_ID};
use crate::i18n::{is_zh, text as t};
use crate::llm::{
    thinking_variant_options_for_model, ThinkingVariantOptions, ThinkingVariantPreferences,
};
use crate::paths::MiyuPaths;
use crate::platforms::commands::{self, PlatformCommandDescriptor};
use crate::platforms::plugins::{
    active_judgement_skip_ids, apply_active_judgement_skip_editor_changes,
};
use crate::state::StateStore;
use anyhow::{bail, Result};
use crossterm::cursor::{Hide, MoveTo, Show};
use crossterm::event::{self, Event, KeyCode, KeyEvent};
use crossterm::style::{Attribute, Print, SetAttribute};
use crossterm::terminal::{self, Clear, ClearType, EnterAlternateScreen, LeaveAlternateScreen};
use crossterm::{execute, queue};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io::{self, Write};
use std::path::PathBuf;
use std::process::Command;
use std::sync::mpsc::{self, Receiver};
use std::time::Duration;

pub fn run(paths: &MiyuPaths) -> Result<bool> {
    AppConfig::init_files(paths)?;
    crate::models_cache::try_load(paths);
    crate::models_cache::spawn_background_refresh(paths.clone());
    let config = AppConfig::load_or_default(paths)?;
    let thinking_variants = ThinkingVariantPreferences::load(paths);
    TerminalSession::start()?.run(paths, config, thinking_variants)
}

struct TerminalSession {
    stdout: io::Stdout,
}

impl TerminalSession {
    fn start() -> Result<Self> {
        terminal::enable_raw_mode()?;
        // 独立 `miyu config` 没有 REPL 的挂断看门狗;不发 SIGHUP 的断开
        // (tmux kill-pane、SSH 掉线)会让 crossterm 对 HUP fd 全速自旋。
        crate::cli::spawn_hangup_watchdog();
        let mut stdout = io::stdout();
        execute!(stdout, EnterAlternateScreen, Hide)?;
        Ok(Self { stdout })
    }

    fn run(
        mut self,
        paths: &MiyuPaths,
        mut config: AppConfig,
        mut thinking_variants: ThinkingVariantPreferences,
    ) -> Result<bool> {
        let result = run_main_menu(&mut self.stdout, paths, &mut config, &mut thinking_variants);
        execute!(self.stdout, Show, LeaveAlternateScreen)?;
        terminal::disable_raw_mode()?;
        result
    }
}

impl Drop for TerminalSession {
    fn drop(&mut self) {
        let _ = execute!(self.stdout, Show, LeaveAlternateScreen);
        let _ = terminal::disable_raw_mode();
    }
}

fn run_main_menu(
    stdout: &mut io::Stdout,
    paths: &MiyuPaths,
    config: &mut AppConfig,
    thinking_variants: &mut ThinkingVariantPreferences,
) -> Result<bool> {
    // Detects edits on quit; sub-menus mutate `config` in place without any
    // dirty flag of their own.
    let pristine_config = serde_json::to_string(config).ok();
    let mut selected = 0usize;
    loop {
        let active = active_label(config);
        let multimodal = active_multimodal_label(config);
        let options = [
            t("Providers and models", "供应商和模型").to_string(),
            format!(
                "{} ({}: {active})",
                t("Configure text model", "配置文本模型"),
                t("Current", "当前")
            ),
            format!(
                "{} ({}: {multimodal})",
                t("Configure multimodal model", "配置多模态模型"),
                t("Current", "当前")
            ),
            format!(
                "{} ({}: {})",
                t("Configure embedding model", "配置 Embedding 模型"),
                t("Current", "当前"),
                embedding_model_label(config)
            ),
            format!(
                "{} ({})",
                t("Configure subagent tier pools", "配置子代理档位池"),
                subagent_tiers_label(config)
            ),
            t("Plugins", "插件配置").to_string(),
            t("Custom prompts", "自定义提示词").to_string(),
            format!(
                "{} ({})",
                t("IM platforms", "接入通讯平台"),
                platforms_label(config)
            ),
            t("Global settings", "全局参数设置").to_string(),
            t("Save and exit", "保存并退出").to_string(),
        ];
        draw_menu(
            stdout,
            t(" MIYU CONFIG ", " MIYU 配置 "),
            &options,
            selected,
            "",
        )?;

        match read_key()? {
            KeyCode::Char('q') | KeyCode::Esc => {
                let dirty = thinking_variants.is_dirty()
                    || serde_json::to_string(config).ok() != pristine_config;
                if !dirty {
                    return Ok(false);
                }
                if confirm_save_on_exit(stdout)? {
                    match config.save(paths) {
                        Ok(()) => {
                            thinking_variants.save(paths)?;
                            return Ok(true);
                        }
                        Err(error) => {
                            // 保存失败(如校验不过)不能崩出:崩出会丢掉本次
                            // 全部内存修改,留在菜单让用户改完再存。
                            show_tui_error(stdout, &error)?;
                            continue;
                        }
                    }
                }
                return Ok(false);
            }
            KeyCode::Up | KeyCode::Char('k') => selected = selected.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => selected = (selected + 1).min(options.len() - 1),
            KeyCode::Enter => {
                let outcome = match selected {
                    0 => ProviderBrowser::new(paths, config, thinking_variants).run(stdout),
                    1 => select_active_provider(stdout, config),
                    2 => select_active_multimodal_provider(stdout, config),
                    3 => edit_embedding_model(stdout, config),
                    4 => select_subagent_tiers(stdout, config),
                    5 => edit_plugins(stdout, config),
                    6 => edit_custom_prompts(stdout, paths, config),
                    7 => select_platforms(stdout, paths, config),
                    8 => edit_settings(stdout, config),
                    9 => match config.save(paths) {
                        Ok(()) => {
                            thinking_variants.save(paths)?;
                            return Ok(true);
                        }
                        Err(error) => Err(error),
                    },
                    _ => Ok(()),
                };
                if let Err(error) = outcome {
                    // 子界面的表单解析/保存错误只作废当次输入,config 的
                    // 内存态还在;显示错误后回主菜单,不让 TUI 整个崩出。
                    show_tui_error(stdout, &error)?;
                }
            }
            _ => {}
        }
    }
}

impl<'a> ProviderBrowser<'a> {
    fn new(
        paths: &'a MiyuPaths,
        config: &'a mut AppConfig,
        thinking_variants: &'a mut ThinkingVariantPreferences,
    ) -> Self {
        Self {
            paths,
            config,
            thinking_variants,
            active_col: 0,
            provider_idx: 0,
            provider_scroll: 0,
            org_idx: 0,
            org_scroll: 0,
            model_idx: 0,
            model_scroll: 0,
            filter: String::new(),
            filter_mode: false,
            raw_models: Vec::new(),
            orgs: Vec::new(),
            models: Vec::new(),
            status: String::new(),
            loading: false,
            fetch_seq: 0,
            fetch_rx: None,
            undo: ConfigUndo::default(),
        }
    }

    fn run(mut self, stdout: &mut io::Stdout) -> Result<()> {
        self.refresh_models();
        loop {
            self.poll_fetch_result();
            self.draw(stdout)?;
            match read_key_with_timeout(if self.loading {
                Some(Duration::from_millis(100))
            } else {
                None
            })? {
                None => continue,
                Some(key) => match key {
                    key if self.filter_mode => self.handle_filter_key(key),
                    KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
                    KeyCode::Left | KeyCode::Char('h') => self.move_left(),
                    KeyCode::Right | KeyCode::Char('l') => self.move_right(),
                    KeyCode::Up | KeyCode::Char('k') => self.move_up(),
                    KeyCode::Down | KeyCode::Char('j') => self.move_down(),
                    KeyCode::Char('/') => {
                        self.filter_mode = true;
                        self.filter.clear();
                        self.rebuild_models();
                    }
                    KeyCode::Char('r') => self.refresh_models(),
                    KeyCode::Char('a') => self.add_provider(stdout)?,
                    KeyCode::Char('d') => self.delete_provider(),
                    KeyCode::Char('u') => self.undo_delete(),
                    KeyCode::Tab if self.active_col == 2 => self.toggle_model_activation(),
                    KeyCode::Enter | KeyCode::Char('i') => self.select_or_edit(stdout)?,
                    _ => {}
                },
            }
        }
    }

    fn handle_filter_key(&mut self, key: KeyCode) {
        match key {
            KeyCode::Esc => {
                self.filter_mode = false;
                self.filter.clear();
            }
            KeyCode::Enter => self.filter_mode = false,
            KeyCode::Backspace => {
                self.filter.pop();
            }
            KeyCode::Char(ch) => self.filter.push(ch),
            _ => {}
        }
        self.rebuild_models();
    }

    fn move_left(&mut self) {
        self.active_col = self.active_col.saturating_sub(1);
    }

    fn move_right(&mut self) {
        self.active_col = (self.active_col + 1).min(2);
    }

    fn move_up(&mut self) {
        match self.active_col {
            0 => {
                self.provider_idx = self.provider_idx.saturating_sub(1);
                self.provider_scroll = column_scroll(
                    self.provider_idx,
                    self.provider_scroll,
                    column_visible_rows(),
                );
                self.refresh_models();
            }
            1 => {
                self.org_idx = self.org_idx.saturating_sub(1);
                self.org_scroll =
                    column_scroll(self.org_idx, self.org_scroll, column_visible_rows());
                self.rebuild_models();
            }
            2 => {
                self.model_idx = self.model_idx.saturating_sub(1);
                self.model_scroll =
                    column_scroll(self.model_idx, self.model_scroll, column_visible_rows());
            }
            _ => {}
        }
    }

    fn move_down(&mut self) {
        match self.active_col {
            0 => {
                self.provider_idx =
                    (self.provider_idx + 1).min(self.config.providers.len().saturating_sub(1));
                self.provider_scroll = column_scroll(
                    self.provider_idx,
                    self.provider_scroll,
                    column_visible_rows(),
                );
                self.refresh_models();
            }
            1 => {
                self.org_idx = (self.org_idx + 1).min(self.orgs.len().saturating_sub(1));
                self.org_scroll =
                    column_scroll(self.org_idx, self.org_scroll, column_visible_rows());
                self.rebuild_models();
            }
            2 => {
                self.model_idx = (self.model_idx + 1).min(self.models.len().saturating_sub(1));
                self.model_scroll =
                    column_scroll(self.model_idx, self.model_scroll, column_visible_rows());
            }
            _ => {}
        }
    }

    fn refresh_models(&mut self) {
        self.provider_idx = self
            .provider_idx
            .min(self.config.providers.len().saturating_sub(1));
        self.raw_models.clear();
        self.orgs = vec!["All".to_string()];
        self.models.clear();
        self.fetch_seq += 1;
        if let Some(provider) = self.config.providers.get(self.provider_idx).cloned() {
            let seq = self.fetch_seq;
            let cli_binary = cli_catalog::builtin_cli_binary(&self.config, &provider);
            let (tx, rx) = mpsc::channel();
            self.fetch_rx = Some(rx);
            self.loading = true;
            self.status = t("Fetching model list...", "正在获取模型列表...").to_string();
            std::thread::spawn(move || {
                let result =
                    fetch_models(&provider, cli_binary.as_deref()).map_err(|err| err.to_string());
                let _ = tx.send((seq, result));
            });
        } else {
            self.fetch_rx = None;
            self.loading = false;
            self.status.clear();
        }
        self.org_idx = 0;
        self.model_idx = 0;
        self.org_scroll = 0;
        self.model_scroll = 0;
    }

    fn poll_fetch_result(&mut self) {
        let Some(rx) = &self.fetch_rx else {
            return;
        };
        let Ok((seq, result)) = rx.try_recv() else {
            return;
        };
        if seq != self.fetch_seq {
            return;
        }
        self.loading = false;
        self.fetch_rx = None;
        match result {
            Ok(models) => {
                self.status = if is_zh() {
                    format!("已获取 {} 个模型", models.len())
                } else {
                    format!("Fetched {} models", models.len())
                };
                self.raw_models = models;
            }
            Err(err) => {
                let status = if is_zh() {
                    format!("获取模型失败: {err}")
                } else {
                    format!("Failed to fetch models: {err}")
                };
                self.status = format_status_line(&status);
                self.raw_models.clear();
            }
        }
        self.rebuild_models();
    }

    fn rebuild_models(&mut self) {
        let filter = self.filter.to_ascii_lowercase();
        let mut grouped: BTreeMap<String, Vec<ModelEntry>> = BTreeMap::new();
        for model in &self.raw_models {
            if !filter.is_empty() && !model.to_ascii_lowercase().contains(&filter) {
                continue;
            }
            let org = model
                .split_once('/')
                .map(|(org, _)| org)
                .unwrap_or("All")
                .to_string();
            let name = model
                .split_once('/')
                .map(|(_, name)| name)
                .unwrap_or(model)
                .to_string();
            grouped
                .entry("All".to_string())
                .or_default()
                .push(ModelEntry::new(model, model));
            if org != "All" {
                grouped
                    .entry(org)
                    .or_default()
                    .push(ModelEntry::new(&name, model));
            }
        }
        self.orgs = grouped.keys().cloned().collect();
        if self.orgs.is_empty() {
            self.orgs.push("All".to_string());
        }
        self.org_idx = self.org_idx.min(self.orgs.len().saturating_sub(1));
        self.models = grouped.remove(&self.orgs[self.org_idx]).unwrap_or_default();
        self.model_idx = self.model_idx.min(self.models.len().saturating_sub(1));
        self.org_scroll = column_scroll(self.org_idx, self.org_scroll, column_visible_rows());
        self.model_scroll = column_scroll(self.model_idx, self.model_scroll, column_visible_rows());
    }

    fn add_provider(&mut self, stdout: &mut io::Stdout) -> Result<()> {
        if let Some(provider) = edit_provider_form(stdout, ProviderConfig::new_custom())? {
            self.config.upsert_provider(provider);
            self.provider_idx = self.config.providers.len().saturating_sub(1);
            self.refresh_models();
        }
        Ok(())
    }

    fn delete_provider(&mut self) {
        if self.config.providers.is_empty() {
            return;
        }
        if self
            .config
            .providers
            .get(self.provider_idx)
            .is_some_and(ProviderConfig::is_builtin_cli_provider)
        {
            // 内置供应商删了下次加载也会被重新注入,徒增困惑;要停用走编辑
            // 表单里的启用开关。
            self.status = t(
                "This built-in CLI provider cannot be deleted; disable it in its edit form instead.",
                "内置 CLI 供应商不可删除;要停用请在编辑表单里关掉启用开关。",
            )
            .to_string();
            return;
        }
        self.undo.record(self.config);
        let removed = self.config.providers.remove(self.provider_idx);
        self.config.remove_provider_references(&removed.id);
        self.provider_idx = self
            .provider_idx
            .min(self.config.providers.len().saturating_sub(1));
        self.refresh_models();
    }

    /// 退回上一步。分步的:连按几次就退几步（上限见 `ConfigUndo`）。
    fn undo_delete(&mut self) {
        if !self.undo.undo(self.config) {
            return;
        }
        self.provider_idx = self
            .provider_idx
            .min(self.config.providers.len().saturating_sub(1));
        self.refresh_models();
    }

    fn select_or_edit(&mut self, stdout: &mut io::Stdout) -> Result<()> {
        match self.active_col {
            0 => {
                if let Some(provider) = self.config.providers.get(self.provider_idx).cloned() {
                    // 内置 Claude Code 走专用表单:没有 HTTP 概念,只有启用
                    // 总开关与 CLI 中转设置。
                    let edited = if provider.is_claude_code() {
                        edit_claude_code_provider_form(
                            stdout,
                            provider,
                            &mut self.config.plugins.claude_code,
                        )?
                    } else if provider.is_antigravity() {
                        edit_antigravity_provider_form(
                            stdout,
                            provider,
                            &mut self.config.plugins.antigravity,
                        )?
                    } else if provider.is_codex() {
                        edit_codex_provider_form(stdout, provider, &mut self.config.plugins.codex)?
                    } else {
                        edit_provider_form(stdout, provider)?
                    };
                    if let Some(provider) = edited {
                        let old_id = self.config.providers[self.provider_idx].id.clone();
                        self.config.providers[self.provider_idx] = provider.clone();
                        if self.config.active_provider == old_id {
                            self.config.active_provider = provider.id.clone();
                        }
                        if old_id != provider.id {
                            self.config
                                .rename_provider_references(&old_id, &provider.id);
                            self.thinking_variants
                                .rename_provider(&old_id, &provider.id);
                        }
                        self.refresh_models();
                    }
                }
            }
            2 => {
                let mut model_updated = false;
                if let Some(model) = self.models.get(self.model_idx).cloned() {
                    if let Some(provider) = self.config.providers.get_mut(self.provider_idx) {
                        auto_configure_model_tags(self.paths, provider, &model.full);
                    }
                    if let Some(provider) = self.config.providers.get_mut(self.provider_idx) {
                        if edit_model_form(stdout, provider, &model.full, self.thinking_variants)? {
                            self.config.active_provider = provider.id.clone();
                            model_updated = true;
                            self.status = if is_zh() {
                                format!("已更新模型设置: {}", model.full)
                            } else {
                                format!("Updated model settings: {}", model.full)
                            };
                        }
                    }
                }
                if model_updated {
                    self.config.prune_model_references();
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn toggle_model_activation(&mut self) {
        if self.active_col != 2 {
            return;
        }
        let mut removed = None;
        if let (Some(provider), Some(model)) = (
            self.config.providers.get_mut(self.provider_idx),
            self.models.get(self.model_idx),
        ) {
            if let Some(index) = provider.models.iter().position(|item| item == &model.full) {
                let provider_id = provider.id.clone();
                let model = model.full.clone();
                provider.models.remove(index);
                if provider.default_model == model {
                    provider.default_model = provider.models.first().cloned().unwrap_or_default();
                }
                self.status = if is_zh() {
                    format!("已取消激活模型: {model}")
                } else {
                    format!("Deactivated model: {model}")
                };
                removed = Some((provider_id, model));
            } else {
                provider.models.push(model.full.clone());
                auto_configure_model_tags(self.paths, provider, &model.full);
                if provider.default_model.trim().is_empty() {
                    provider.default_model = model.full.clone();
                }
                self.status = if is_zh() {
                    format!("已激活模型: {}", model.full)
                } else {
                    format!("Activated model: {}", model.full)
                };
            }
        }
        if let Some((provider_id, model)) = removed {
            self.config
                .remove_active_model_references(&provider_id, &model);
        }
    }

    fn draw(&self, stdout: &mut io::Stdout) -> Result<()> {
        let (cols, rows) = terminal::size()?;
        let inner_x = 0;
        let inner_y = 0;
        let inner_w = cols;
        let inner_h = rows.saturating_sub(2);
        let left_w = inner_w.saturating_mul(28).saturating_div(100).max(20);
        let mid_w = inner_w.saturating_mul(22).saturating_div(100).max(16);
        let right_w = inner_w
            .saturating_sub(left_w)
            .saturating_sub(mid_w)
            .saturating_sub(2)
            .max(18);
        // 不再给 active_provider 打星号。这个菜单只回答「有哪些供应商、各自有
        // 哪些模型可用」;「现在用谁」由「配置文本模型」那个池子决定。星号标的
        // 是 `active_provider`——它现在只是 `provider(None)` 的兜底,在这里显示
        // 会让人以为在这一列按一下就能换模型。
        let providers = self
            .config
            .providers
            .iter()
            .map(|provider| {
                if provider.enabled {
                    format!("  {}", provider.display_name)
                } else {
                    // 目前只有内置 Claude Code 会处于未启用态,标出来免得
                    // 用户找不到"为什么模型列表里没有它"。
                    format!(
                        "  {}{}",
                        provider.display_name,
                        t(" (disabled)", "(未启用)")
                    )
                }
            })
            .collect::<Vec<_>>();
        let models = self
            .models
            .iter()
            .map(|model| {
                let active = self
                    .config
                    .providers
                    .get(self.provider_idx)
                    .map(|provider| provider.models.iter().any(|item| item == &model.full))
                    .unwrap_or(false);
                format!("{} {}", if active { "[*]" } else { "[ ]" }, model.name)
            })
            .collect::<Vec<_>>();
        let orgs = self
            .orgs
            .iter()
            .map(|org| {
                if org == "All" {
                    t("All", "全部").to_string()
                } else {
                    org.clone()
                }
            })
            .collect::<Vec<_>>();

        queue!(stdout, Clear(ClearType::All))?;
        draw_column(
            stdout,
            inner_x,
            inner_y,
            left_w,
            inner_h,
            t(" PROVIDERS ", " 供应商 "),
            &providers,
            self.provider_idx,
            self.provider_scroll,
            self.active_col == 0,
        )?;
        draw_column(
            stdout,
            inner_x + left_w + 1,
            inner_y,
            mid_w,
            inner_h,
            t(" ORGANIZATION ", " 组织 "),
            &orgs,
            self.org_idx,
            self.org_scroll,
            self.active_col == 1,
        )?;
        let title = if self.filter.is_empty() {
            t(" MODELS ", " 模型 ").to_string()
        } else if is_zh() {
            format!(" 模型 /{} ", self.filter)
        } else {
            format!(" MODELS /{} ", self.filter)
        };
        draw_column(
            stdout,
            inner_x + left_w + mid_w + 2,
            inner_y,
            right_w,
            inner_h,
            &title,
            &models,
            self.model_idx,
            self.model_scroll,
            self.active_col == 2,
        )?;
        let help = if self.filter_mode {
            if is_zh() {
                format!("搜索: {}_  [Enter]确认 [Esc]取消", self.filter)
            } else {
                format!("Search: {}_  [Enter]confirm [Esc]cancel", self.filter)
            }
        } else {
            format!(
                "{}{}",
                t(
                    "[h/l]column [j/k]move [Tab]activate model [Enter]model settings [/]search [r]refresh [a]add [d]delete [q]back",
                    "[h/l]切栏 [j/k]移动 [Tab]激活模型 [Enter]模型设置 [/]搜索 [r]刷新 [a]添加 [d]删除 [q]返回",
                ),
                self.undo.hint()
            )
        };
        let status = if self.loading {
            format!("{}", self.status)
        } else {
            self.status.clone()
        };
        queue!(
            stdout,
            MoveTo(0, rows.saturating_sub(2)),
            Clear(ClearType::CurrentLine),
            Print(truncate(&status, cols as usize))
        )?;
        queue!(
            stdout,
            MoveTo(0, rows.saturating_sub(1)),
            Clear(ClearType::CurrentLine),
            Print(truncate(&help, cols as usize))
        )?;
        stdout.flush()?;
        Ok(())
    }
}

use crate::config::EMBEDDING_MODALITY;

#[cfg(test)]
mod tests;
