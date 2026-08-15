use crate::config::{
    merge_real_context_settings, ActiveProviderModelConfig, ApiQuotaAccountConfig,
    ApiQuotaProviderConfig, AppConfig, PlatformCommandPermission, PlatformConversationConfig,
    PlatformConversationKind, PlatformModelPoolInheritance, PlatformModelRoute,
    PlatformPersonaOverride, PlatformRateLimit, PlatformSessionLimits, ProviderConfig,
    ProviderModelChoice, QqMemeCollectorPluginSettings, QqMessageHistoryPluginSettings,
    RealContextIdentityMapping, RealContextPluginSettings, MAX_COMMAND_OUTPUT_LINES,
    MAX_PLATFORM_COMMAND_PREFIX_CHARS, MAX_PLATFORM_SESSION_QUEUED, MAX_PLATFORM_SESSION_RUNNING,
    MAX_REPL_REPLAY_TURNS,
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
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind};
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
                    config.save(paths)?;
                    thinking_variants.save(paths)?;
                    return Ok(true);
                }
                return Ok(false);
            }
            KeyCode::Up | KeyCode::Char('k') => selected = selected.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => selected = (selected + 1).min(options.len() - 1),
            KeyCode::Enter => match selected {
                0 => ProviderBrowser::new(paths, config, thinking_variants).run(stdout)?,
                1 => select_active_provider(stdout, config)?,
                2 => select_active_multimodal_provider(stdout, config)?,
                3 => edit_embedding_model(stdout, config)?,
                4 => select_subagent_tiers(stdout, config)?,
                5 => edit_plugins(stdout, config)?,
                6 => edit_custom_prompts(stdout, paths, config)?,
                7 => select_platforms(stdout, paths, config)?,
                8 => edit_settings(stdout, config)?,
                9 => {
                    config.save(paths)?;
                    thinking_variants.save(paths)?;
                    return Ok(true);
                }
                _ => {}
            },
            _ => {}
        }
    }
}

fn edit_plugins(stdout: &mut io::Stdout, config: &mut AppConfig) -> Result<()> {
    let mut selected = 0usize;
    loop {
        let count = plugin_names().len();
        draw_plugin_menu(stdout, config, selected)?;
        match read_key()? {
            KeyCode::Esc | KeyCode::Char('q') => return Ok(()),
            KeyCode::Up | KeyCode::Char('k') => selected = selected.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => selected = (selected + 1).min(count - 1),
            KeyCode::Char(' ') => toggle_plugin(config, selected),
            KeyCode::Enter | KeyCode::Char('i') => edit_plugin_detail(stdout, config, selected)?,
            _ => {}
        }
    }
}

fn draw_plugin_menu(stdout: &mut io::Stdout, config: &AppConfig, selected: usize) -> Result<()> {
    let (cols, rows) = terminal::size()?;
    let width = cols.saturating_sub(4).max(60);
    let height = rows.saturating_sub(2).max(10);
    let x = 2;
    let y = 1;
    queue!(stdout, Clear(ClearType::All))?;
    draw_box(stdout, x, y, width, height, t(" PLUGINS ", " 插件 "))?;
    queue!(
        stdout,
        MoveTo(x + 2, y + 1),
        Print(t(
            "[Space]enable/disable [Enter]configure [j/k]move [q]back",
            "[Space]启用/禁用 [Enter]配置 [j/k]移动 [q]返回",
        ))
    )?;
    queue!(
        stdout,
        MoveTo(x + 2, y + 3),
        SetAttribute(Attribute::Bold),
        Print(pad(
            &plugin_row(
                t("Status", "状态"),
                t("Plugin", "插件"),
                t("Description", "说明"),
                width.saturating_sub(4) as usize,
            ),
            width.saturating_sub(4) as usize,
        )),
        SetAttribute(Attribute::Reset)
    )?;
    let plugins = plugin_names();
    let visible_rows = height.saturating_sub(6) as usize;
    let start = selected.saturating_sub(visible_rows.saturating_sub(1));
    for row in 0..visible_rows {
        let index = start + row;
        if index >= plugins.len() {
            break;
        }
        let (_, name, description) = plugins[index];
        let state = if plugin_enabled(config, index) {
            t("[ON]", "[开]")
        } else {
            t("[OFF]", "[关]")
        };
        let line = plugin_row(state, name, description, width.saturating_sub(4) as usize);
        queue!(stdout, MoveTo(x + 2, y + row as u16 + 4))?;
        if index == selected {
            queue!(
                stdout,
                SetAttribute(Attribute::Reverse),
                Print(pad(&line, width.saturating_sub(4) as usize)),
                SetAttribute(Attribute::Reset)
            )?;
        } else {
            queue!(stdout, Print(pad(&line, width.saturating_sub(4) as usize)))?;
        }
    }
    stdout.flush()?;
    Ok(())
}

fn plugin_row(state: &str, name: &str, description: &str, width: usize) -> String {
    let fixed = pad(state, 8) + &pad(name, 24);
    let remaining = width.saturating_sub(display_width(&fixed)).max(10);
    fixed + &truncate(description, remaining)
}

fn plugin_names() -> [(&'static str, &'static str, &'static str); 13] {
    [
        (
            "web",
            t("Web search", "网络搜索"),
            t(
                "Search APIs with script fallback",
                "搜索 API 与脚本 fallback",
            ),
        ),
        (
            "deep_research",
            t("Deep research", "深度研究"),
            t(
                "Run long research tasks and output Markdown",
                "长任务研究并输出 Markdown",
            ),
        ),
        (
            "vision",
            t("Vision", "识图"),
            t(
                "Image understanding and terminal preview",
                "图片理解和终端预览",
            ),
        ),
        (
            "image_generation",
            t("Image generation", "生图"),
            t("Generate images from text", "文本生成图片"),
        ),
        (
            "web_images",
            t("Image search", "搜图"),
            t(
                "Search, download, and review web images",
                "网络图片搜索、下载与审核",
            ),
        ),
        (
            "print_image",
            t("Print image", "打印图片"),
            t("Terminal image print size", "终端图片打印尺寸"),
        ),
        (
            "memes",
            t("Memes", "表情包"),
            t("Persona meme library and send size", "人格表情库与发送尺寸"),
        ),
        (
            "knowledge_base",
            t("Knowledge base", "知识库"),
            t(
                "Local file search and semantic index",
                "本地文件检索与语义索引",
            ),
        ),
        (
            "archlinux",
            "Arch Linux",
            t("AUR status and ArchWiki lookup", "AUR 状态与 ArchWiki 查询"),
        ),
        (
            "man",
            t("Online manuals", "在线手册"),
            t(
                "Search and read online man pages",
                "在线 man 手册搜索与读取",
            ),
        ),
        (
            "memory",
            t("Memory", "记忆"),
            t("Long-term memory and association", "长期记忆与联想"),
        ),
        (
            "package_advisor",
            t("AUR review", "AUR 审查"),
            t("PKGBUILD/AUR security review", "PKGBUILD/AUR 安全审查"),
        ),
        (
            "api_quota",
            t("LLM API quota", "大模型额度查询"),
            t(
                "Query DeepSeek and OpenRouter API quota",
                "查询 DeepSeek 与 OpenRouter API 额度",
            ),
        ),
    ]
}

fn plugin_enabled(config: &AppConfig, index: usize) -> bool {
    match index {
        0 => config.plugins.web.enabled,
        1 => config.plugins.deep_research.enabled,
        2 => config.plugins.vision.enabled,
        3 => config.plugins.image_generation.enabled,
        4 => config.plugins.web_images.enabled,
        5 => config.plugins.print_image.enabled,
        6 => config.plugins.memes.enabled,
        7 => config.plugins.knowledge_base.enabled,
        8 => config.plugins.archlinux.enabled,
        9 => config.plugins.man.enabled,
        10 => config.plugins.memory.enabled,
        11 => config.plugins.package_advisor.enabled,
        12 => config.plugins.api_quota.enabled,
        _ => false,
    }
}

fn toggle_plugin(config: &mut AppConfig, index: usize) {
    let value = !plugin_enabled(config, index);
    match index {
        0 => config.plugins.web.enabled = value,
        1 => config.plugins.deep_research.enabled = value,
        2 => config.plugins.vision.enabled = value,
        3 => config.plugins.image_generation.enabled = value,
        4 => config.plugins.web_images.enabled = value,
        5 => config.plugins.print_image.enabled = value,
        6 => config.plugins.memes.enabled = value,
        7 => config.plugins.knowledge_base.enabled = value,
        8 => config.plugins.archlinux.enabled = value,
        9 => config.plugins.man.enabled = value,
        10 => config.plugins.memory.enabled = value,
        11 => config.plugins.package_advisor.enabled = value,
        12 => config.plugins.api_quota.enabled = value,
        _ => {}
    }
}

fn edit_plugin_detail(stdout: &mut io::Stdout, config: &mut AppConfig, index: usize) -> Result<()> {
    if index == 13 {
        return edit_api_quota(stdout, config);
    }
    let title = format!(" {}: {} ", t("PLUGIN", "插件"), plugin_names()[index].1);
    let mut fields = plugin_fields(config, index);
    if !run_form(stdout, &title, &mut fields)? {
        return Ok(());
    }
    apply_plugin_fields(config, index, &fields)
}

fn edit_api_quota(stdout: &mut io::Stdout, config: &mut AppConfig) -> Result<()> {
    let mut selected = 0usize;
    loop {
        let options = [
            format!(
                "DeepSeek ({})",
                configured_count(&config.plugins.api_quota.deepseek)
            ),
            format!(
                "OpenRouter ({})",
                configured_count(&config.plugins.api_quota.openrouter)
            ),
        ];
        draw_menu(
            stdout,
            t(" LLM API QUOTA ", " 大模型额度查询 "),
            &options,
            selected,
            t("[Enter]configure [q]back", "[Enter]配置 [q]返回"),
        )?;
        match read_key()? {
            KeyCode::Esc | KeyCode::Char('q') => return Ok(()),
            KeyCode::Up | KeyCode::Char('k') => selected = selected.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => selected = (selected + 1).min(1),
            KeyCode::Enter | KeyCode::Char('i') => {
                if selected == 0 {
                    edit_api_quota_accounts(
                        stdout,
                        "DeepSeek",
                        &mut config.plugins.api_quota.deepseek,
                    )?;
                } else {
                    edit_api_quota_accounts(
                        stdout,
                        "OpenRouter",
                        &mut config.plugins.api_quota.openrouter,
                    )?;
                }
            }
            _ => {}
        }
    }
}

fn edit_api_quota_accounts(
    stdout: &mut io::Stdout,
    name: &str,
    config: &mut ApiQuotaProviderConfig,
) -> Result<()> {
    if config.accounts.is_empty() {
        config.accounts.push(ApiQuotaAccountConfig {
            id: "account-1".to_string(),
            name: "默认账号".to_string(),
            api_key: std::mem::take(&mut config.api_key),
        });
    }
    let mut selected = 0usize;
    loop {
        let mut options = config
            .accounts
            .iter()
            .map(|account| {
                format!(
                    "{} ({})",
                    account.name,
                    if account.api_key.trim().is_empty() {
                        t("not configured", "未配置")
                    } else {
                        t("configured", "已配置")
                    }
                )
            })
            .collect::<Vec<_>>();
        options.push(t("New account", "新建账号").to_string());
        selected = selected.min(options.len().saturating_sub(1));
        draw_menu(
            stdout,
            &format!(" {name} "),
            &options,
            selected,
            if name == "DeepSeek" {
                t(
                    "[Enter]edit [n]new [d]delete",
                    "[Enter]编辑 [n]新建 [d]删除",
                )
            } else {
                t(
                    "[Enter]edit [n]new [d]delete [q]back",
                    "[Enter]编辑 [n]新建 [d]删除 [q]返回",
                )
            },
        )?;
        match read_key()? {
            KeyCode::Esc | KeyCode::Char('q') => return Ok(()),
            KeyCode::Up | KeyCode::Char('k') => selected = selected.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => {
                selected = (selected + 1).min(options.len().saturating_sub(1))
            }
            KeyCode::Char('n') => {
                if config.accounts.len() < 32 && add_api_quota_account(stdout, config, name)? {
                    selected = config.accounts.len().saturating_sub(1);
                }
            }
            KeyCode::Char('d') if selected < config.accounts.len() => {
                if confirm_api_quota_delete(stdout, &config.accounts[selected].name)? {
                    if config.accounts.len() == 1 {
                        config.accounts[0].name = "默认账号".to_string();
                        config.accounts[0].api_key.clear();
                    } else {
                        config.accounts.remove(selected);
                        selected = selected.min(config.accounts.len() - 1);
                    }
                }
            }
            KeyCode::Enter | KeyCode::Char('i') if selected < config.accounts.len() => {
                let _ = edit_api_quota_account(stdout, name, &mut config.accounts[selected])?;
            }
            KeyCode::Enter | KeyCode::Char('i') => {
                if config.accounts.len() < 32 && add_api_quota_account(stdout, config, name)? {
                    selected = config.accounts.len().saturating_sub(1);
                }
            }
            _ => {}
        }
    }
}

/// true = save and exit, false = discard and exit. A choice is mandatory:
/// `q`/`Esc` are ignored so an accidental key press cannot lose edits.
fn confirm_save_on_exit(stdout: &mut io::Stdout) -> Result<bool> {
    let options = [
        t("Save", "保存").to_string(),
        t("Discard", "不保存").to_string(),
    ];
    let mut selected = 0usize;
    loop {
        draw_menu(
            stdout,
            t(" SAVE EDITED CHANGES? ", " 是否保存已编辑内容 "),
            &options,
            selected,
            t("[j/k]move [Enter]confirm", "[j/k]移动 [Enter]确认"),
        )?;
        match read_key()? {
            KeyCode::Up | KeyCode::Char('k') => selected = selected.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => selected = (selected + 1).min(1),
            KeyCode::Enter => return Ok(selected == 0),
            _ => {}
        }
    }
}

fn confirm_api_quota_delete(stdout: &mut io::Stdout, account: &str) -> Result<bool> {
    let options = [
        t("Cancel", "取消").to_string(),
        format!("{}: {account}", t("Delete", "删除")),
    ];
    let mut selected = 0usize;
    loop {
        draw_menu(
            stdout,
            t(" DELETE ACCOUNT ", " 删除账号 "),
            &options,
            selected,
            t("[Enter]confirm [q]cancel", "[Enter]确认 [q]取消"),
        )?;
        match read_key()? {
            KeyCode::Esc | KeyCode::Char('q') => return Ok(false),
            KeyCode::Up | KeyCode::Char('k') => selected = selected.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => selected = (selected + 1).min(1),
            KeyCode::Enter => return Ok(selected == 1),
            _ => {}
        }
    }
}

fn edit_api_quota_account(
    stdout: &mut io::Stdout,
    provider: &str,
    account: &mut ApiQuotaAccountConfig,
) -> Result<bool> {
    let mut fields = vec![
        Field::new(t("Account name", "账号名称"), account.name.clone()),
        Field::new("API Key", account.api_key.clone()).sensitive(),
    ];
    if run_form(stdout, &format!(" {provider} "), &mut fields)? {
        account.name = fields[0].value.trim().to_string();
        if account.name.is_empty() {
            account.name = "默认账号".to_string();
        }
        account.api_key = fields[1].value.trim().to_string();
        return Ok(true);
    }
    Ok(false)
}

fn add_api_quota_account(
    stdout: &mut io::Stdout,
    config: &mut ApiQuotaProviderConfig,
    provider: &str,
) -> Result<bool> {
    let name = next_api_quota_account_name(config);
    let id = next_api_quota_account_id(config);
    config.accounts.push(ApiQuotaAccountConfig {
        id,
        name,
        api_key: String::new(),
    });
    let index = config.accounts.len() - 1;
    if edit_api_quota_account(stdout, provider, &mut config.accounts[index])? {
        Ok(true)
    } else {
        config.accounts.pop();
        Ok(false)
    }
}

fn next_api_quota_account_id(_config: &ApiQuotaProviderConfig) -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT_ID: AtomicU64 = AtomicU64::new(1);
    let sequence = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!("account-{nanos}-{sequence}")
}

fn next_api_quota_account_name(config: &ApiQuotaProviderConfig) -> String {
    let mut number = 2usize;
    loop {
        let candidate = format!("账号 {number}");
        if config
            .accounts
            .iter()
            .all(|account| account.name != candidate)
        {
            return candidate;
        }
        number += 1;
    }
}

fn configured_count(config: &ApiQuotaProviderConfig) -> String {
    let count = if config.accounts.is_empty() {
        usize::from(!config.api_key.trim().is_empty())
    } else {
        config
            .accounts
            .iter()
            .filter(|account| !account.api_key.trim().is_empty())
            .count()
    };
    if is_zh() {
        format!("{count} 个已配置")
    } else {
        format!("{count} configured")
    }
}

fn plugin_fields(config: &AppConfig, index: usize) -> Vec<Field> {
    match index {
        0 => vec![
            Field::boolean(t("Enabled", "启用"), config.plugins.web.enabled),
            Field::new(
                t("Results per request", "每次返回数量"),
                config.plugins.web.max_results.to_string(),
            ),
            Field::textarea(
                "Tavily API Keys",
                config.plugins.web.tavily_api_keys.join("\n"),
            )
            .sensitive(),
            Field::textarea(
                "Firecrawl API Keys",
                config.plugins.web.firecrawl_api_keys.join("\n"),
            )
            .sensitive(),
            Field::textarea(
                "AnySearch API Keys",
                config.plugins.web.anysearch_api_keys.join("\n"),
            )
            .sensitive(),
            Field::textarea(
                t("Exa API Keys (optional; keyless free quota)", "Exa API Keys（可留空用免费额度）"),
                config.plugins.web.exa_api_keys.join("\n"),
            )
            .sensitive(),
            Field::new("SearXNG URL", config.plugins.web.searxng_base_url.clone()),
        ],
        1 => vec![
            Field::boolean(t("Enabled", "启用"), config.plugins.deep_research.enabled),
            Field::new(
                t("Output directory", "输出目录"),
                config.plugins.deep_research.output_dir.clone(),
            ),
            Field::new(
                t("Thinking depth", "思考深度"),
                config.plugins.deep_research.thinking_depth.clone(),
            )
            .choices(&["minimal", "low", "medium", "high", "xhigh"]),
            Field::new(
                t("Maximum review revisions", "最大审视修正次数"),
                config
                    .plugins
                    .deep_research
                    .max_review_revisions
                    .to_string(),
            ),
            Field::new(
                t("Tool steps per round", "每轮工具步数"),
                config
                    .plugins
                    .deep_research
                    .max_tool_steps_per_round
                    .to_string(),
            ),
            Field::new(
                t("Final answer character limit", "最终字数上限"),
                config
                    .plugins
                    .deep_research
                    .max_final_answer_chars
                    .to_string(),
            ),
            Field::new(
                t("Tool timeout (seconds)", "工具超时秒数"),
                config
                    .plugins
                    .deep_research
                    .tool_call_timeout_seconds
                    .to_string(),
            ),
            Field::boolean(
                t("Show progress", "显示过程进度"),
                config.plugins.deep_research.show_progress,
            ),
        ],
        2 => vec![
            Field::boolean(t("Enabled", "启用"), config.plugins.vision.enabled),
            Field::boolean(
                t(
                    "Prefer current chat model for images",
                    "优先使用当前对话模型识图",
                ),
                config.plugins.vision.prefer_current_multimodal_model,
            ),
            Field::new(
                t("Vision provider/model", "识图 Provider/模型"),
                vision_provider_value(config),
            )
            .choices_owned(vision_provider_model_choice_values(config)),
            Field::new(
                t("Response header timeout (seconds)", "响应头超时秒数"),
                config
                    .plugins
                    .vision
                    .response_header_timeout_seconds
                    .to_string(),
            ),
            Field::new(
                t("Stream idle timeout (seconds)", "流空闲超时秒数"),
                config
                    .plugins
                    .vision
                    .stream_idle_timeout_seconds
                    .to_string(),
            ),
            Field::new(
                t("Per-image timeout (seconds)", "单图总超时秒数"),
                config.plugins.vision.image_timeout_seconds.to_string(),
            ),
        ],
        3 => vec![
            Field::boolean(
                t("Enabled", "启用"),
                config.plugins.image_generation.enabled,
            ),
            Field::new(
                t("Image API type", "生图 API 类型"),
                config.plugins.image_generation.provider_type.clone(),
            )
            .choices(&["openai", "rightcode"]),
            Field::new("Base URL", config.plugins.image_generation.base_url.clone()),
            Field::textarea(
                "API Keys",
                config.plugins.image_generation.api_keys.join("\n"),
            )
            .sensitive(),
            Field::new(
                t("Model", "模型"),
                config.plugins.image_generation.model.clone(),
            ),
            Field::new(
                t("Default aspect ratio", "默认宽高比"),
                config.plugins.image_generation.default_aspect_ratio.clone(),
            )
            .choices(&[
                "自动", "1:1", "2:3", "3:2", "3:4", "4:3", "4:5", "5:4", "9:16", "16:9", "21:9",
            ]),
            Field::new(
                t("Default resolution", "默认分辨率"),
                config.plugins.image_generation.default_resolution.clone(),
            )
            .choices(&["1K", "2K", "4K"]),
            Field::new(
                t("Output directory", "输出目录"),
                config.plugins.image_generation.output_dir.clone(),
            ),
            Field::boolean(
                t("Print when complete", "完成后打印"),
                config.plugins.image_generation.auto_print,
            ),
            Field::new(
                t("Timeout (seconds)", "超时秒数"),
                config.plugins.image_generation.timeout_seconds.to_string(),
            ),
        ],
        4 => vec![
            Field::boolean(t("Enabled", "启用"), config.plugins.web_images.enabled),
            Field::new(
                t("Search source mode", "搜索来源模式"),
                config.plugins.web_images.source_mode.clone(),
            )
            .choices(&["auto", "global", "mainland"]),
            Field::boolean(
                t("Vision model review", "视觉模型审核"),
                config.plugins.web_images.vision_screening_enabled,
            ),
            Field::new(
                t("Maximum results", "数量上限"),
                config.plugins.web_images.max_results.to_string(),
            ),
            Field::boolean(
                t("Safe search", "安全搜索"),
                config.plugins.web_images.safe_search,
            ),
            Field::boolean(
                t("Automatic preview", "自动预览"),
                config.plugins.web_images.auto_preview,
            ),
            Field::new(
                t("Default preview count", "默认预览数量"),
                config.plugins.web_images.preview_count.to_string(),
            ),
            Field::new(
                t("Maximum download (MB)", "最大下载 MB"),
                config.plugins.web_images.max_download_mb.to_string(),
            ),
            Field::new(
                t("Timeout (seconds)", "超时秒数"),
                config.plugins.web_images.timeout_seconds.to_string(),
            ),
        ],
        5 => vec![
            Field::boolean(t("Enabled", "启用"), config.plugins.print_image.enabled),
            Field::new(
                t("Print width percent", "打印宽度百分比"),
                config.plugins.print_image.width_percent.to_string(),
            ),
            Field::new(
                t("Print height percent", "打印高度百分比"),
                config.plugins.print_image.height_percent.to_string(),
            ),
        ],
        6 => vec![
            Field::boolean(t("Enabled", "启用"), config.plugins.memes.enabled),
            Field::new(
                t("Send width percent", "发送宽度百分比"),
                config.plugins.memes.width_percent.to_string(),
            ),
            Field::new(
                t("Send height percent", "发送高度百分比"),
                config.plugins.memes.height_percent.to_string(),
            ),
            Field::new(
                t("Maximum image size (MB)", "最大图片 MB"),
                config.plugins.memes.max_image_mb.to_string(),
            ),
            Field::new(
                t("Maximum search results", "搜索最大结果数"),
                config.plugins.memes.search_max_results.to_string(),
            ),
            Field::boolean(
                t("Allow animated GIFs", "允许 GIF 动画"),
                config.plugins.memes.allow_gif_animation,
            ),
            Field::boolean(
                t("Suggest memes automatically", "自动提示发送表情"),
                config.plugins.memes.auto_send_enabled,
            ),
            Field::boolean(
                t(
                    "Suggest memes automatically on platforms",
                    "通讯平台自动提示发送表情",
                ),
                config.plugins.memes.auto_send_platform_enabled,
            ),
            Field::new(
                t(
                    "Automatic meme suggestion probability",
                    "自动提示发送表情概率",
                ),
                config.plugins.memes.auto_send_probability.to_string(),
            ),
        ],
        7 => vec![
            Field::boolean(t("Enabled", "启用"), config.plugins.knowledge_base.enabled),
            Field::new(
                t("Knowledge base directory", "知识库目录"),
                config.plugins.knowledge_base.data_dir.clone(),
            ),
            Field::new(
                t("Maximum search results", "搜索最大结果数"),
                config.plugins.knowledge_base.max_search_results.to_string(),
            ),
            Field::new(
                t("Snippet context characters", "片段上下文字数"),
                config
                    .plugins
                    .knowledge_base
                    .snippet_context_chars
                    .to_string(),
            ),
            Field::new(
                t("Proximity window characters", "同窗匹配范围"),
                config
                    .plugins
                    .knowledge_base
                    .proximity_window_chars
                    .to_string(),
            ),
            Field::new(
                t("Maximum lines to read", "读取最大行数"),
                config.plugins.knowledge_base.max_read_lines.to_string(),
            ),
            Field::new(
                t("Maximum file size (KB)", "最大文件 KB"),
                config.plugins.knowledge_base.max_file_size_kb.to_string(),
            ),
            Field::boolean(
                t("Allow AI uploads", "允许 AI 上传"),
                config.plugins.knowledge_base.upload_tool_enabled,
            ),
            Field::boolean(
                t("Enable embedding", "启用 Embedding"),
                config.plugins.knowledge_base.embedding_enabled,
            ),
            Field::new(
                t("Embedding provider/model", "Embedding Provider/模型"),
                kb_embedding_provider_value(config),
            )
            .choices_owned(provider_model_choice_values(config, false))
            .empty_choice_label(t("Embedding not configured", "未配置 Embedding")),
            Field::new(
                t("Semantic chunk size", "语义块大小"),
                config
                    .plugins
                    .knowledge_base
                    .semantic_chunk_chars
                    .to_string(),
            ),
            Field::new(
                t("Semantic chunk overlap", "语义块重叠"),
                config
                    .plugins
                    .knowledge_base
                    .semantic_chunk_overlap
                    .to_string(),
            ),
            Field::new(
                t("Semantic candidates", "语义候选数"),
                config.plugins.knowledge_base.semantic_top_k.to_string(),
            ),
            Field::new(
                t("Minimum semantic score", "语义最低分"),
                config.plugins.knowledge_base.semantic_min_score.to_string(),
            ),
            Field::new(
                t("Strong keyword match threshold", "关键词强命中阈值"),
                config
                    .plugins
                    .knowledge_base
                    .keyword_strong_score_threshold
                    .to_string(),
            ),
            Field::new(
                t("Embedding timeout (seconds)", "Embedding 超时秒数"),
                config
                    .plugins
                    .knowledge_base
                    .embedding_timeout_seconds
                    .to_string(),
            ),
        ],
        8 => vec![Field::boolean(
            t("Enabled", "启用"),
            config.plugins.archlinux.enabled,
        )],
        9 => vec![Field::boolean(
            t("Enabled", "启用"),
            config.plugins.man.enabled,
        )],
        10 => {
            let memory = config.memory_config();
            vec![
                Field::boolean(t("Enabled", "启用"), memory.enabled),
                Field::boolean(
                    t("Evicted context cache", "上下文弹出缓存"),
                    memory.evicted_context_enabled,
                ),
                Field::boolean(
                    t("Enable association", "联想启用"),
                    memory.association_enabled,
                ),
                Field::boolean(t("Automatic diary", "自动日记"), memory.auto_diary_enabled),
                Field::boolean(
                    t("Automatic fact memory", "自动知识记忆"),
                    memory.auto_fact_enabled,
                ),
                Field::new(
                    t("Diary batch size", "日记整理轮数"),
                    memory.diary_batch_size.to_string(),
                ),
                Field::new(
                    t("Short diary retention days", "短期日记保留天数"),
                    memory.short_diary_retention_days.to_string(),
                ),
                Field::new(
                    t("Diary promotion recalls", "日记长期化召回次数"),
                    memory.diary_promotion_recalls.to_string(),
                ),
                Field::new(
                    t("Organizer timeout seconds", "记忆整理超时秒数"),
                    memory.organizer_timeout_seconds.to_string(),
                ),
                Field::new(
                    t("Associated facts", "联想知识条数"),
                    memory.association_facts.to_string(),
                ),
                Field::new(
                    t("Associated events", "联想事件条数"),
                    memory.association_episodes.to_string(),
                ),
                Field::new(
                    t("Association character limit", "联想字符上限"),
                    memory.association_max_chars.to_string(),
                ),
                Field::boolean(
                    t("Enable forgetting", "遗忘启用"),
                    memory.forgetting_enabled,
                ),
                Field::new(
                    t("Forgetting half-life (days)", "遗忘半衰期天"),
                    memory.forgetting_half_life_days.to_string(),
                ),
                Field::new(
                    t("Minimum forgetting strength", "遗忘最低强度"),
                    memory.forgetting_min_strength.to_string(),
                ),
                Field::new(
                    t("Recall boost strength", "回忆增强强度"),
                    memory.forgetting_review_boost.to_string(),
                ),
                Field::boolean(
                    t("Association dedup", "联想跨回合去重"),
                    memory.association_dedup,
                ),
            ]
        }
        11 => vec![Field::boolean(
            t("Enabled", "启用"),
            config.plugins.package_advisor.enabled,
        )],
        12 => vec![Field::boolean(
            t("Enabled", "启用"),
            config.plugins.api_quota.enabled,
        )],
        _ => vec![Field::boolean(
            t("Enabled", "启用"),
            plugin_enabled(config, index),
        )],
    }
}

fn apply_plugin_fields(config: &mut AppConfig, index: usize, fields: &[Field]) -> Result<()> {
    match index {
        0 => {
            config.plugins.web.enabled = parse_bool_field(&fields[0].value)?;
            config.plugins.web.max_results = fields[1].value.trim().parse::<usize>()?.clamp(1, 10);
            config.plugins.web.tavily_api_keys = parse_key_list(&fields[2].value);
            config.plugins.web.firecrawl_api_keys = parse_key_list(&fields[3].value);
            config.plugins.web.anysearch_api_keys = parse_key_list(&fields[4].value);
            config.plugins.web.exa_api_keys = parse_key_list(&fields[5].value);
            config.plugins.web.searxng_base_url =
                fields[6].value.trim().trim_end_matches('/').to_string();
        }
        1 => {
            config.plugins.deep_research.enabled = parse_bool_field(&fields[0].value)?;
            config.plugins.deep_research.output_dir = fields[1].value.trim().to_string();
            config.plugins.deep_research.thinking_depth = fields[2].value.trim().to_string();
            config.plugins.deep_research.max_review_revisions = fields[3].value.trim().parse()?;
            config.plugins.deep_research.max_tool_steps_per_round =
                fields[4].value.trim().parse()?;
            config.plugins.deep_research.max_final_answer_chars = fields[5].value.trim().parse()?;
            config.plugins.deep_research.tool_call_timeout_seconds =
                fields[6].value.trim().parse()?;
            config.plugins.deep_research.show_progress = parse_bool_field(&fields[7].value)?;
        }
        2 => {
            config.plugins.vision.enabled = parse_bool_field(&fields[0].value)?;
            config.plugins.vision.prefer_current_multimodal_model =
                parse_bool_field(&fields[1].value)?;
            let (provider_id, model) = parse_provider_model_choice(&fields[2].value);
            config.plugins.vision.vision_provider_id = provider_id;
            config.plugins.vision.vision_model = model;
            config.plugins.vision.response_header_timeout_seconds =
                fields[3].value.trim().parse::<u64>()?.max(1);
            config.plugins.vision.stream_idle_timeout_seconds =
                fields[4].value.trim().parse::<u64>()?.max(1);
            config.plugins.vision.image_timeout_seconds =
                fields[5].value.trim().parse::<u64>()?.max(1);
        }
        3 => {
            config.plugins.image_generation.enabled = parse_bool_field(&fields[0].value)?;
            config.plugins.image_generation.provider_type = fields[1].value.trim().to_string();
            config.plugins.image_generation.base_url =
                fields[2].value.trim().trim_end_matches('/').to_string();
            config.plugins.image_generation.api_keys = parse_key_list(&fields[3].value);
            config.plugins.image_generation.model = fields[4].value.trim().to_string();
            config.plugins.image_generation.default_aspect_ratio =
                fields[5].value.trim().to_string();
            config.plugins.image_generation.default_resolution = fields[6].value.trim().to_string();
            config.plugins.image_generation.output_dir = fields[7].value.trim().to_string();
            config.plugins.image_generation.auto_print = parse_bool_field(&fields[8].value)?;
            config.plugins.image_generation.timeout_seconds = fields[9].value.trim().parse()?;
        }
        4 => {
            config.plugins.web_images.enabled = parse_bool_field(&fields[0].value)?;
            config.plugins.web_images.source_mode = match fields[1].value.trim() {
                "auto" | "global" | "mainland" => fields[1].value.trim().to_string(),
                other => {
                    if is_zh() {
                        anyhow::bail!("未知搜图来源模式: {other}")
                    } else {
                        anyhow::bail!("Unknown image search source mode: {other}")
                    }
                }
            };
            config.plugins.web_images.vision_screening_enabled =
                parse_bool_field(&fields[2].value)?;
            config.plugins.web_images.max_results =
                fields[3].value.trim().parse::<usize>()?.clamp(1, 10);
            config.plugins.web_images.safe_search = parse_bool_field(&fields[4].value)?;
            config.plugins.web_images.auto_preview = parse_bool_field(&fields[5].value)?;
            config.plugins.web_images.preview_count =
                fields[6].value.trim().parse::<usize>()?.min(5);
            config.plugins.web_images.max_download_mb =
                fields[7].value.trim().parse::<f64>()?.clamp(0.1, 50.0);
            config.plugins.web_images.timeout_seconds =
                fields[8].value.trim().parse::<u64>()?.clamp(5, 120);
        }
        5 => {
            config.plugins.print_image.enabled = parse_bool_field(&fields[0].value)?;
            config.plugins.print_image.width_percent = fields[1].value.trim().parse::<u8>()?;
            config.plugins.print_image.height_percent = fields[2].value.trim().parse::<u8>()?;
        }
        6 => {
            config.plugins.memes.enabled = parse_bool_field(&fields[0].value)?;
            config.plugins.memes.width_percent =
                fields[1].value.trim().parse::<u8>()?.clamp(1, 100);
            config.plugins.memes.height_percent =
                fields[2].value.trim().parse::<u8>()?.clamp(1, 100);
            config.plugins.memes.max_image_mb =
                fields[3].value.trim().parse::<u64>()?.clamp(1, 100);
            config.plugins.memes.search_max_results =
                fields[4].value.trim().parse::<usize>()?.clamp(1, 3);
            config.plugins.memes.allow_gif_animation = parse_bool_field(&fields[5].value)?;
            config.plugins.memes.auto_send_enabled = parse_bool_field(&fields[6].value)?;
            config.plugins.memes.auto_send_platform_enabled = parse_bool_field(&fields[7].value)?;
            config.plugins.memes.auto_send_probability =
                fields[8].value.trim().parse::<f32>()?.clamp(0.0, 1.0);
        }
        7 => {
            config.plugins.knowledge_base.enabled = parse_bool_field(&fields[0].value)?;
            config.plugins.knowledge_base.data_dir = fields[1].value.trim().to_string();
            config.plugins.knowledge_base.max_search_results = fields[2].value.trim().parse()?;
            config.plugins.knowledge_base.snippet_context_chars = fields[3].value.trim().parse()?;
            config.plugins.knowledge_base.proximity_window_chars =
                fields[4].value.trim().parse()?;
            config.plugins.knowledge_base.max_read_lines = fields[5].value.trim().parse()?;
            config.plugins.knowledge_base.max_file_size_kb = fields[6].value.trim().parse()?;
            config.plugins.knowledge_base.upload_tool_enabled = parse_bool_field(&fields[7].value)?;
            config.plugins.knowledge_base.embedding_enabled = parse_bool_field(&fields[8].value)?;
            let (provider_id, model) = parse_provider_model_choice(&fields[9].value);
            config.plugins.knowledge_base.embedding_provider_id = provider_id;
            config.plugins.knowledge_base.embedding_model = model;
            config.plugins.knowledge_base.semantic_chunk_chars = fields[10].value.trim().parse()?;
            config.plugins.knowledge_base.semantic_chunk_overlap =
                fields[11].value.trim().parse()?;
            config.plugins.knowledge_base.semantic_top_k = fields[12].value.trim().parse()?;
            config.plugins.knowledge_base.semantic_min_score = fields[13].value.trim().parse()?;
            config.plugins.knowledge_base.keyword_strong_score_threshold =
                fields[14].value.trim().parse()?;
            config.plugins.knowledge_base.embedding_timeout_seconds =
                fields[15].value.trim().parse()?;
        }
        8 => {
            config.plugins.archlinux.enabled = parse_bool_field(&fields[0].value)?;
        }
        9 => {
            config.plugins.man.enabled = parse_bool_field(&fields[0].value)?;
        }
        10 => {
            config.memory = crate::config::MemoryConfig::default();
            config.plugins.memory.enabled = parse_bool_field(&fields[0].value)?;
            config.plugins.memory.evicted_context_enabled = parse_bool_field(&fields[1].value)?;
            config.plugins.memory.association_enabled = parse_bool_field(&fields[2].value)?;
            config.plugins.memory.auto_diary_enabled = parse_bool_field(&fields[3].value)?;
            config.plugins.memory.auto_fact_enabled = parse_bool_field(&fields[4].value)?;
            config.plugins.memory.auto_skill_enabled = false;
            config.plugins.memory.diary_batch_size =
                fields[5].value.trim().parse::<usize>()?.clamp(2, 100);
            config.plugins.memory.short_diary_retention_days =
                fields[6].value.trim().parse::<u64>()?.clamp(1, 3650);
            config.plugins.memory.diary_promotion_recalls =
                fields[7].value.trim().parse::<u64>()?.clamp(1, 100);
            config.plugins.memory.organizer_timeout_seconds =
                fields[8].value.trim().parse::<u64>()?.clamp(5, 600);
            config.plugins.memory.association_facts = fields[9].value.trim().parse::<usize>()?;
            config.plugins.memory.association_episodes =
                fields[10].value.trim().parse::<usize>()?;
            config.plugins.memory.association_max_chars =
                fields[11].value.trim().parse::<usize>()?;
            config.plugins.memory.forgetting_enabled = parse_bool_field(&fields[12].value)?;
            config.plugins.memory.forgetting_half_life_days =
                fields[13].value.trim().parse::<f64>()?;
            config.plugins.memory.forgetting_min_strength =
                fields[14].value.trim().parse::<f64>()?;
            config.plugins.memory.forgetting_review_boost =
                fields[15].value.trim().parse::<f64>()?;
            config.plugins.memory.association_dedup = parse_bool_field(&fields[16].value)?;
        }
        11 => {
            config.plugins.package_advisor.enabled = parse_bool_field(&fields[0].value)?;
        }
        12 => {
            config.plugins.api_quota.enabled = parse_bool_field(&fields[0].value)?;
        }
        _ => {
            let value = parse_bool_field(&fields[0].value)?;
            if plugin_enabled(config, index) != value {
                toggle_plugin(config, index);
            }
        }
    }
    Ok(())
}

fn edit_custom_prompts(
    stdout: &mut io::Stdout,
    paths: &MiyuPaths,
    config: &mut AppConfig,
) -> Result<()> {
    let mut selected = 0usize;
    loop {
        let persona = if config.prompt.active_persona.trim().is_empty() {
            "Miyu".to_string()
        } else {
            persona_display_name(&config.prompt.active_persona).to_string()
        };
        let options = [
            format!(
                "{} ({}: {persona})",
                t("AI persona", "AI 人格"),
                t("Current", "当前")
            ),
            t("User identity", "用户身份").to_string(),
        ];
        draw_menu(
            stdout,
            t(" CUSTOM PROMPTS ", " 自定义提示词 "),
            &options,
            selected,
            t("[Enter]select [q]back", "[Enter]选择 [q]返回"),
        )?;
        match read_key()? {
            KeyCode::Esc | KeyCode::Char('q') => return Ok(()),
            KeyCode::Up | KeyCode::Char('k') => selected = selected.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => selected = (selected + 1).min(options.len() - 1),
            KeyCode::Enter if selected == 0 => edit_personas(stdout, paths, config)?,
            KeyCode::Enter if selected == 1 => edit_identities(stdout, paths, config)?,
            _ => {}
        }
    }
}

fn edit_personas(stdout: &mut io::Stdout, paths: &MiyuPaths, config: &mut AppConfig) -> Result<()> {
    manage_personas(stdout, paths, config, PersonaMenuTarget::Global)?;
    Ok(())
}

enum PersonaMenuTarget {
    Global,
    Platform(PlatformPersonaOverride),
}

impl PersonaMenuTarget {
    fn custom_offset(&self) -> usize {
        match self {
            Self::Global => 1,
            Self::Platform(_) => 2,
        }
    }

    fn is_miyu(&self, config: &AppConfig) -> bool {
        match self {
            Self::Global => config.prompt.active_persona.trim().is_empty(),
            Self::Platform(persona) => matches!(persona, PlatformPersonaOverride::Miyu),
        }
    }

    fn custom_name<'a>(&'a self, config: &'a AppConfig) -> Option<&'a str> {
        match self {
            Self::Global => (!config.prompt.active_persona.trim().is_empty())
                .then_some(config.prompt.active_persona.as_str()),
            Self::Platform(persona) => persona.custom_name(),
        }
    }

    fn activate_inherit(&mut self) {
        if let Self::Platform(persona) = self {
            *persona = PlatformPersonaOverride::Inherit;
        }
    }

    fn activate_miyu(&mut self, config: &mut AppConfig) {
        match self {
            Self::Global => config.prompt.active_persona.clear(),
            Self::Platform(persona) => *persona = PlatformPersonaOverride::Miyu,
        }
    }

    fn activate_custom(&mut self, config: &mut AppConfig, name: String) {
        match self {
            Self::Global => config.prompt.active_persona = name,
            Self::Platform(persona) => *persona = PlatformPersonaOverride::Custom { name },
        }
    }

    fn rename_custom(&mut self, old_name: &str, new_name: &str) {
        if let Self::Platform(persona) = self {
            if persona.custom_name() == Some(old_name) {
                *persona = PlatformPersonaOverride::Custom {
                    name: new_name.to_string(),
                };
            }
        }
    }

    fn pending_reference_count(&self, name: &str) -> usize {
        usize::from(matches!(
            self,
            Self::Platform(PlatformPersonaOverride::Custom { name: current }) if current == name
        ))
    }

    fn into_platform(self) -> Option<PlatformPersonaOverride> {
        match self {
            Self::Global => None,
            Self::Platform(persona) => Some(persona),
        }
    }
}

fn manage_personas(
    stdout: &mut io::Stdout,
    paths: &MiyuPaths,
    config: &mut AppConfig,
    mut target: PersonaMenuTarget,
) -> Result<Option<PlatformPersonaOverride>> {
    std::fs::create_dir_all(config.prompts_dir_path(paths))?;
    let mut selected = 0usize;
    loop {
        let personas = list_personas(paths, config)?;
        let custom_offset = target.custom_offset();
        let mut options = Vec::with_capacity(personas.len() + custom_offset);
        if let PersonaMenuTarget::Platform(persona) = &target {
            options.push(format!(
                "{}{}",
                if persona.is_inherit() { "* " } else { "  " },
                t("Inherit current persona", "继承当前人格")
            ));
        }
        options.push(format!(
            "{}Miyu",
            if target.is_miyu(config) { "* " } else { "  " }
        ));
        options.extend(personas.iter().map(|name| {
            let display = persona_display_name(name);
            if target.custom_name(config) == Some(name.as_str()) {
                format!("* {display}")
            } else {
                format!("  {display}")
            }
        }));
        selected = selected.min(options.len().saturating_sub(1));
        draw_menu(
            stdout,
            match &target {
                PersonaMenuTarget::Global => t(" AI PERSONA ", " AI 人格 "),
                PersonaMenuTarget::Platform(_) => {
                    t(" QQ CONVERSATION PERSONA ", " QQ 会话 AI 人格 ")
                }
            },
            &options,
            selected,
            t(
                "[Tab]activate [Enter]edit [a]add [d]delete [j/k]move [q]back",
                "[Tab]激活 [Enter]编辑 [a]新增 [d]删除 [j/k]移动 [q]返回",
            ),
        )?;
        match read_key()? {
            KeyCode::Esc | KeyCode::Char('q') => return Ok(target.into_platform()),
            KeyCode::Up | KeyCode::Char('k') => selected = selected.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => selected = (selected + 1).min(options.len() - 1),
            KeyCode::Tab => {
                if matches!(&target, PersonaMenuTarget::Platform(_)) && selected == 0 {
                    target.activate_inherit();
                } else if selected + 1 == custom_offset {
                    target.activate_miyu(config);
                } else if let Some(name) = personas.get(selected.saturating_sub(custom_offset)) {
                    target.activate_custom(config, name.clone());
                }
            }
            KeyCode::Char('a') => {
                if let Some(name) = new_persona(stdout, paths, config)? {
                    target.activate_custom(config, name);
                }
            }
            KeyCode::Enter if selected >= custom_offset => {
                if let Some(name) = personas.get(selected - custom_offset) {
                    if let Some(values) = edit_persona(stdout, paths, config, name)? {
                        apply_persona_edit(paths, config, name, &values.name, &values.content)?;
                        write_persona_aux(
                            paths,
                            config,
                            &crate::config::persona_scope_name(&values.name),
                            &values.hint,
                            &values.dialogs,
                        )?;
                        target.rename_custom(name, &values.name);
                    }
                }
            }
            // 默认 Miyu 人格本体只读,但防失忆提示与预设对话是独立文件
            // (hints/default.md、dialogs/default.md),回车打开精简表单。
            KeyCode::Enter if selected + 1 == custom_offset => {
                edit_miyu_persona_extras(stdout, paths, config)?;
            }
            KeyCode::Char('d') if selected >= custom_offset => {
                if let Some(name) = personas.get(selected - custom_offset) {
                    let persisted = AppConfig::load_or_default(paths)?;
                    let references = config
                        .platforms
                        .persona_reference_count(name)
                        .max(persisted.platforms.persona_reference_count(name))
                        .max(target.pending_reference_count(name));
                    if references > 0 {
                        message(
                            stdout,
                            &if is_zh() {
                                format!(
                                    "该人格仍被 {references} 个 QQ 会话配置引用，请先解除引用。"
                                )
                            } else {
                                format!(
                                    "This persona is still used by {references} QQ conversation configuration(s). Remove those references first."
                                )
                            },
                        )?;
                        continue;
                    }
                    apply_persona_delete(paths, config, persisted, name)?;
                    selected = selected.saturating_sub(1);
                }
            }
            _ => {}
        }
    }
}

fn apply_persona_edit(
    paths: &MiyuPaths,
    config: &mut AppConfig,
    old_name: &str,
    new_name: &str,
    content: &str,
) -> Result<()> {
    ensure_persona_name_available(paths, config, new_name, Some(old_name))?;
    if old_name == new_name {
        return write_persona(paths, config, new_name, content);
    }

    let old_path = config.persona_path(paths, old_name);
    let new_path = config.persona_path(paths, new_name);
    let old_content = std::fs::read(&old_path)?;
    let mut persisted = AppConfig::load_or_default(paths)?;
    let state = crate::state::StateStore::new(paths)?;
    write_persona(paths, config, new_name, content)?;
    if let Err(error) = move_persona_scope(paths, config, old_name, new_name) {
        let _ = std::fs::remove_file(&new_path);
        return Err(error);
    }

    let old_scope = crate::config::persona_scope_name(old_name);
    let new_scope = crate::config::persona_scope_name(new_name);
    if let Err(error) = state.rename_persona_scope(&old_scope, &new_scope) {
        let _ = move_persona_scope(paths, config, new_name, old_name);
        let _ = std::fs::remove_file(&new_path);
        return Err(error);
    }
    if let Err(error) = std::fs::remove_file(&old_path) {
        let _ = state.rename_persona_scope(&new_scope, &old_scope);
        let _ = move_persona_scope(paths, config, new_name, old_name);
        let _ = std::fs::remove_file(&new_path);
        return Err(error.into());
    }

    persisted
        .platforms
        .rename_persona_references(old_name, new_name);
    if persisted.prompt.active_persona == old_name {
        persisted.prompt.active_persona = new_name.to_string();
    }
    if let Err(error) = persisted.save(paths) {
        let _ = std::fs::write(&old_path, old_content);
        let _ = std::fs::remove_file(&new_path);
        let _ = state.rename_persona_scope(&new_scope, &old_scope);
        let _ = move_persona_scope(paths, config, new_name, old_name);
        return Err(error);
    }

    config
        .platforms
        .rename_persona_references(old_name, new_name);
    if config.prompt.active_persona == old_name {
        config.prompt.active_persona = new_name.to_string();
    }
    Ok(())
}

fn apply_persona_delete(
    paths: &MiyuPaths,
    config: &mut AppConfig,
    mut persisted: AppConfig,
    name: &str,
) -> Result<()> {
    if persisted.prompt.active_persona == name {
        persisted.prompt.active_persona.clear();
        persisted.save(paths)?;
    }
    let scope = crate::config::persona_scope_name(name);
    crate::state::StateStore::new(paths)?.delete_persona_scope(&scope)?;
    let path = config.persona_path(paths, name);
    if path.exists() {
        std::fs::remove_file(path)?;
    }
    remove_persona_scope(paths, config, name)?;
    if config.prompt.active_persona == name {
        config.prompt.active_persona.clear();
    }
    Ok(())
}

struct PersonaFormValues {
    name: String,
    content: String,
    hint: String,
    dialogs: String,
}

/// 人格附属文件现值:防失忆提示(hints/<scope>.md)与预设对话
/// (dialogs/<scope>.md)。
fn persona_aux_values(paths: &MiyuPaths, config: &AppConfig, scope: &str) -> (String, String) {
    let hint =
        std::fs::read_to_string(crate::persona_hint::manual_hint_path(config, paths, scope))
            .map(|text| text.trim().to_string())
            .unwrap_or_default();
    let dialogs = crate::persona_hint::dialogs_raw(config, paths, scope);
    (hint, dialogs)
}

/// 附属文件落盘:非空写入,空则删除(清空提示=回到自动蒸馏,清空
/// 对话=不注入)。
fn write_persona_aux(
    paths: &MiyuPaths,
    config: &AppConfig,
    scope: &str,
    hint: &str,
    dialogs: &str,
) -> Result<()> {
    let targets = [
        (
            crate::persona_hint::manual_hint_path(config, paths, scope),
            hint,
        ),
        (
            crate::persona_hint::dialogs_path(config, paths, scope),
            dialogs,
        ),
    ];
    for (path, value) in targets {
        let value = value.trim();
        if value.is_empty() {
            if path.exists() {
                std::fs::remove_file(&path)?;
            }
        } else {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&path, format!("{value}\n"))?;
        }
    }
    Ok(())
}

fn persona_aux_fields(hint: String, dialogs: String, miyu: bool) -> Vec<Field> {
    let (hint_label, dialogs_label) = if miyu {
        (
            t(
                "Anti-amnesia reminder (empty = built-in default)",
                "防失忆提示(清空=恢复内置默认)",
            ),
            t(
                "Preset dialogs (user:/assistant: lines; empty = built-in default)",
                "预设对话(user:/assistant: 开头的行,清空=恢复内置默认)",
            ),
        )
    } else {
        (
            t(
                "Anti-amnesia reminder (empty = auto distill)",
                "防失忆提示(留空=自动蒸馏)",
            ),
            t(
                "Preset dialogs (lines starting with user:/assistant:)",
                "预设对话(user:/assistant: 开头的行)",
            ),
        )
    };
    vec![
        Field::textarea(hint_label, hint),
        Field::textarea(dialogs_label, dialogs),
    ]
}

fn new_persona(
    stdout: &mut io::Stdout,
    paths: &MiyuPaths,
    config: &AppConfig,
) -> Result<Option<String>> {
    let mut fields = vec![
        Field::new(t("Name", "名称"), String::new()),
        Field::textarea(t("Content", "内容"), String::new()),
    ];
    fields.extend(persona_aux_fields(String::new(), String::new(), false));
    if !run_form(stdout, t(" NEW PERSONA ", " 新建人格 "), &mut fields)? {
        return Ok(None);
    }
    let name = sanitize_persona_name(&fields[0].value)?;
    ensure_persona_name_available(paths, config, &name, None)?;
    write_persona(paths, config, &name, &fields[1].value)?;
    write_persona_aux(
        paths,
        config,
        &crate::config::persona_scope_name(&name),
        &fields[2].value,
        &fields[3].value,
    )?;
    Ok(Some(name))
}

fn edit_persona(
    stdout: &mut io::Stdout,
    paths: &MiyuPaths,
    config: &AppConfig,
    current_name: &str,
) -> Result<Option<PersonaFormValues>> {
    let content = read_persona(paths, config, current_name)?;
    let (hint, dialogs) = persona_aux_values(
        paths,
        config,
        &crate::config::persona_scope_name(current_name),
    );
    let mut fields = vec![
        Field::new(
            t("Name", "名称"),
            persona_display_name(current_name).to_string(),
        ),
        Field::textarea(t("Content", "内容"), content),
    ];
    fields.extend(persona_aux_fields(hint, dialogs, false));
    if !run_form(stdout, t(" EDIT PERSONA ", " 编辑人格 "), &mut fields)? {
        return Ok(None);
    }
    let name = sanitize_persona_name(&fields[0].value)?;
    Ok(Some(PersonaFormValues {
        name,
        content: fields[1].value.clone(),
        hint: fields[2].value.clone(),
        dialogs: fields[3].value.clone(),
    }))
}

/// 默认 Miyu 人格:本体只读,回车只编辑附属的防失忆提示与预设对话
/// (scope 固定为 default)。
fn edit_miyu_persona_extras(
    stdout: &mut io::Stdout,
    paths: &MiyuPaths,
    config: &AppConfig,
) -> Result<()> {
    let (hint, dialogs) = crate::persona_hint::miyu_aux_prefill(config, paths);
    let mut fields = persona_aux_fields(hint, dialogs, true);
    if !run_form(stdout, t(" MIYU EXTRAS ", " Miyu 人格附加 "), &mut fields)? {
        return Ok(());
    }
    write_persona_aux(paths, config, "default", &fields[0].value, &fields[1].value)
}

fn ensure_persona_name_available(
    paths: &MiyuPaths,
    config: &AppConfig,
    candidate: &str,
    current: Option<&str>,
) -> Result<()> {
    let candidate_scope = crate::config::persona_scope_name(candidate);
    for existing in list_personas(paths, config)? {
        if current == Some(existing.as_str()) {
            continue;
        }
        if existing == candidate {
            bail!(
                "{}",
                t(
                    "A persona with this name already exists.",
                    "同名人格已存在。"
                )
            );
        }
        if crate::config::persona_scope_name(&existing) == candidate_scope {
            bail!(
                "{}",
                t(
                    "This persona name conflicts with another persona's persistent scope.",
                    "该人格名称与另一个人格的持久化作用域冲突。",
                )
            );
        }
    }
    Ok(())
}

fn move_persona_scope(
    paths: &MiyuPaths,
    config: &AppConfig,
    old_name: &str,
    new_name: &str,
) -> Result<()> {
    if old_name == new_name
        || crate::config::persona_scope_name(old_name)
            == crate::config::persona_scope_name(new_name)
    {
        return Ok(());
    }
    let moves = [
        (
            config.persona_memory_data_dir(paths, old_name),
            config.persona_memory_data_dir(paths, new_name),
        ),
        (
            config.persona_memory_state_dir(paths, old_name),
            config.persona_memory_state_dir(paths, new_name),
        ),
        (
            config.persona_skills_dir(paths, old_name),
            config.persona_skills_dir(paths, new_name),
        ),
    ];
    if let Some((_, target)) = moves
        .iter()
        .find(|(source, target)| source.exists() && target.exists())
    {
        bail!(
            "persona scope destination already exists: {}",
            target.display()
        );
    }
    let mut completed = Vec::new();
    for (source, target) in moves {
        if let Err(error) = move_dir_if_exists(source.clone(), target.clone()) {
            for (from, to) in completed.into_iter().rev() {
                let _ = move_dir_if_exists(to, from);
            }
            return Err(error);
        }
        if target.exists() {
            completed.push((source, target));
        }
    }
    let old_scope = crate::config::persona_scope_name(old_name);
    let new_scope = crate::config::persona_scope_name(new_name);
    let file_moves = [
        (
            crate::persona_hint::manual_hint_path(config, paths, &old_scope),
            crate::persona_hint::manual_hint_path(config, paths, &new_scope),
        ),
        (
            crate::persona_hint::dialogs_path(config, paths, &old_scope),
            crate::persona_hint::dialogs_path(config, paths, &new_scope),
        ),
    ];
    for (source, target) in file_moves {
        if source.exists() && !target.exists() {
            std::fs::rename(&source, &target)?;
        }
    }
    Ok(())
}

fn remove_persona_scope(paths: &MiyuPaths, config: &AppConfig, name: &str) -> Result<()> {
    remove_dir_if_exists(config.persona_memory_data_dir(paths, name))?;
    remove_dir_if_exists(config.persona_memory_state_dir(paths, name))?;
    remove_dir_if_exists(config.persona_skills_dir(paths, name))?;
    let scope = crate::config::persona_scope_name(name);
    for path in [
        crate::persona_hint::manual_hint_path(config, paths, &scope),
        crate::persona_hint::dialogs_path(config, paths, &scope),
    ] {
        if path.exists() {
            std::fs::remove_file(path)?;
        }
    }
    Ok(())
}

fn move_dir_if_exists(from: PathBuf, to: PathBuf) -> Result<()> {
    if !from.exists() {
        return Ok(());
    }
    if let Some(parent) = to.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::rename(from, to)?;
    Ok(())
}

fn remove_dir_if_exists(path: PathBuf) -> Result<()> {
    if path.exists() {
        std::fs::remove_dir_all(path)?;
    }
    Ok(())
}

fn edit_identities(
    stdout: &mut io::Stdout,
    paths: &MiyuPaths,
    config: &mut AppConfig,
) -> Result<()> {
    std::fs::create_dir_all(config.identities_dir_path(paths))?;
    let mut selected = 0usize;
    loop {
        let identities = list_identities(paths, config)?;
        let mut options = Vec::with_capacity(identities.len() + 1);
        let default_marker = if config.prompt.active_identity.trim().is_empty() {
            "* "
        } else {
            "  "
        };
        options.push(format!(
            "{default_marker}{}",
            t("Do not use a user identity", "不使用用户身份")
        ));
        options.extend(identities.iter().map(|name| {
            let display = persona_display_name(name);
            if *name == config.prompt.active_identity {
                format!("* {display}")
            } else {
                format!("  {display}")
            }
        }));
        selected = selected.min(options.len().saturating_sub(1));
        draw_menu(
            stdout,
            t(" USER IDENTITY ", " 用户身份 "),
            &options,
            selected,
            t(
                "[Tab]activate [Enter]edit [a]add [d]delete [j/k]move [q]back",
                "[Tab]激活 [Enter]编辑 [a]新增 [d]删除 [j/k]移动 [q]返回",
            ),
        )?;
        match read_key()? {
            KeyCode::Esc | KeyCode::Char('q') => return Ok(()),
            KeyCode::Up | KeyCode::Char('k') => selected = selected.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => selected = (selected + 1).min(options.len() - 1),
            KeyCode::Tab => {
                config.prompt.active_identity = if selected == 0 {
                    String::new()
                } else {
                    identities.get(selected - 1).cloned().unwrap_or_default()
                };
            }
            KeyCode::Char('a') => {
                if let Some(name) = new_identity(stdout, paths, config)? {
                    config.prompt.active_identity = name;
                }
            }
            KeyCode::Enter if selected > 0 => {
                if let Some(name) = identities.get(selected - 1) {
                    if let Some(new_name) = edit_identity(stdout, paths, config, name)? {
                        if config.prompt.active_identity == *name {
                            config.prompt.active_identity = new_name;
                        }
                    }
                }
            }
            KeyCode::Char('d') if selected > 0 => {
                if let Some(name) = identities.get(selected - 1) {
                    let path = config.identity_path(paths, name);
                    if path.exists() {
                        std::fs::remove_file(path)?;
                    }
                    if config.prompt.active_identity == *name {
                        config.prompt.active_identity.clear();
                    }
                    selected = selected.saturating_sub(1);
                }
            }
            _ => {}
        }
    }
}

fn new_identity(
    stdout: &mut io::Stdout,
    paths: &MiyuPaths,
    config: &AppConfig,
) -> Result<Option<String>> {
    edit_prompt_file_form(
        stdout,
        t(" NEW IDENTITY ", " 新建用户身份 "),
        None,
        String::new(),
        |name, content| write_identity(paths, config, name, content),
    )
}

fn edit_identity(
    stdout: &mut io::Stdout,
    paths: &MiyuPaths,
    config: &AppConfig,
    current_name: &str,
) -> Result<Option<String>> {
    let content = read_identity(paths, config, current_name)?;
    edit_prompt_file_form(
        stdout,
        t(" EDIT IDENTITY ", " 编辑用户身份 "),
        Some(current_name),
        content,
        |name, content| {
            if name != current_name {
                let old_path = config.identity_path(paths, current_name);
                if old_path.exists() {
                    std::fs::remove_file(old_path)?;
                }
            }
            write_identity(paths, config, name, content)
        },
    )
}

fn list_identities(paths: &MiyuPaths, config: &AppConfig) -> Result<Vec<String>> {
    list_markdown_files(&config.identities_dir_path(paths))
}

fn read_identity(paths: &MiyuPaths, config: &AppConfig, name: &str) -> Result<String> {
    let path = config.identity_path(paths, name);
    if path.exists() {
        Ok(std::fs::read_to_string(path)?)
    } else {
        Ok(String::new())
    }
}

fn write_identity(paths: &MiyuPaths, config: &AppConfig, name: &str, content: &str) -> Result<()> {
    let path = config.identity_path(paths, name);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, format_text_file(content))?;
    Ok(())
}

fn edit_prompt_file_form<F>(
    stdout: &mut io::Stdout,
    title: &str,
    current_name: Option<&str>,
    content: String,
    write: F,
) -> Result<Option<String>>
where
    F: FnOnce(&str, &str) -> Result<()>,
{
    let Some((name, content)) = edit_prompt_file_values(stdout, title, current_name, content)?
    else {
        return Ok(None);
    };
    write(&name, &content)?;
    Ok(Some(name))
}

fn edit_prompt_file_values(
    stdout: &mut io::Stdout,
    title: &str,
    current_name: Option<&str>,
    content: String,
) -> Result<Option<(String, String)>> {
    let mut fields = vec![
        Field::new(
            t("Name", "名称"),
            current_name
                .map(persona_display_name)
                .unwrap_or("")
                .to_string(),
        ),
        Field::textarea(t("Content", "内容"), content),
    ];
    if !run_form(stdout, title, &mut fields)? {
        return Ok(None);
    }
    let name = sanitize_persona_name(&fields[0].value)?;
    Ok(Some((name, fields[1].value.clone())))
}

fn list_personas(paths: &MiyuPaths, config: &AppConfig) -> Result<Vec<String>> {
    let mut names = list_markdown_files(&config.prompts_dir_path(paths))?;
    names.retain(|name| !name.eq_ignore_ascii_case("system-prompt.md"));
    Ok(names)
}

fn list_markdown_files(dir: &std::path::Path) -> Result<Vec<String>> {
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut names = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        if entry.file_type()?.is_file() {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.ends_with(".md") {
                names.push(name);
            }
        }
    }
    names.sort();
    Ok(names)
}

fn read_persona(paths: &MiyuPaths, config: &AppConfig, name: &str) -> Result<String> {
    let path = config.persona_path(paths, name);
    if path.exists() {
        Ok(std::fs::read_to_string(path)?)
    } else {
        Ok(String::new())
    }
}

fn write_persona(paths: &MiyuPaths, config: &AppConfig, name: &str, content: &str) -> Result<()> {
    let path = config.persona_path(paths, name);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, format_text_file(content))?;
    Ok(())
}

fn sanitize_persona_name(value: &str) -> Result<String> {
    let mut name = value
        .trim()
        .trim_end_matches(".md")
        .replace(['/', '\\'], "-");
    if name.is_empty() {
        bail!("{}", t("Persona name cannot be empty", "人格名称不能为空"));
    }
    name.push_str(".md");
    if name.eq_ignore_ascii_case("system-prompt.md") {
        bail!(
            "{}",
            t(
                "system-prompt.md is reserved",
                "system-prompt.md 是保留文件名"
            )
        );
    }
    Ok(name)
}

fn persona_display_name(name: &str) -> &str {
    name.strip_suffix(".md").unwrap_or(name)
}

fn format_text_file(content: &str) -> String {
    let content = content.trim_end();
    if content.is_empty() {
        String::new()
    } else {
        format!("{content}\n")
    }
}

fn parse_key_list(value: &str) -> Vec<String> {
    value
        .split(|ch| ch == ',' || ch == '\n' || ch == '\r')
        .map(str::trim)
        .filter(|item| !item.is_empty())
        .map(str::to_string)
        .collect()
}

struct ProviderBrowser<'a> {
    paths: &'a MiyuPaths,
    config: &'a mut AppConfig,
    thinking_variants: &'a mut ThinkingVariantPreferences,
    active_col: usize,
    provider_idx: usize,
    provider_scroll: usize,
    org_idx: usize,
    org_scroll: usize,
    model_idx: usize,
    model_scroll: usize,
    filter: String,
    filter_mode: bool,
    raw_models: Vec<String>,
    orgs: Vec<String>,
    models: Vec<ModelEntry>,
    status: String,
    loading: bool,
    fetch_seq: u64,
    fetch_rx: Option<Receiver<FetchResult>>,
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
            let (tx, rx) = mpsc::channel();
            self.fetch_rx = Some(rx);
            self.loading = true;
            self.status = t("Fetching model list...", "正在获取模型列表...").to_string();
            std::thread::spawn(move || {
                let result = fetch_models(&provider).map_err(|err| err.to_string());
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
        let removed = self.config.providers.remove(self.provider_idx);
        self.config.remove_provider_references(&removed.id);
        self.provider_idx = self
            .provider_idx
            .min(self.config.providers.len().saturating_sub(1));
        self.refresh_models();
    }

    fn select_or_edit(&mut self, stdout: &mut io::Stdout) -> Result<()> {
        match self.active_col {
            0 => {
                if let Some(provider) = self.config.providers.get(self.provider_idx).cloned() {
                    if let Some(provider) = edit_provider_form(stdout, provider)? {
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
        let providers = self
            .config
            .providers
            .iter()
            .map(|provider| {
                let active = if provider.id == self.config.active_provider {
                    "* "
                } else {
                    "  "
                };
                format!("{active}{}", provider.display_name)
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
            t(
                "[h/l]column [j/k]move [Tab]activate model [Enter]model settings [/]search [r]refresh [a]add [d]delete [q]back",
                "[h/l]切栏 [j/k]移动 [Tab]激活模型 [Enter]模型设置 [/]搜索 [r]刷新 [a]添加 [d]删除 [q]返回",
            )
            .to_string()
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

type FetchResult = (u64, Result<Vec<String>, String>);

fn format_status_line(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[derive(Clone)]
struct ModelEntry {
    name: String,
    full: String,
}

impl ModelEntry {
    fn new(name: &str, full: &str) -> Self {
        Self {
            name: name.to_string(),
            full: full.to_string(),
        }
    }
}

fn fetch_models(provider: &ProviderConfig) -> Result<Vec<String>> {
    let api_key = provider.api_key.as_deref().unwrap_or_default();
    let mut api_key = if let Some(env_name) = api_key.strip_prefix("$env:") {
        std::env::var(env_name).unwrap_or_default()
    } else {
        api_key.to_string()
    };
    if api_key.is_empty() && provider.is_opencode_zen() {
        api_key = "public".to_string();
    }
    let url = models_url(&provider.base_url);
    let mut request = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(provider.timeout_seconds))
        .build()?
        .get(url)
        .header("Accept", "application/json")
        .header("User-Agent", "miyu-config");
    if !api_key.is_empty() {
        request = request.bearer_auth(api_key);
    }
    let response = request.send()?;
    let status = response.status();
    let body = response.text()?;
    if !status.is_success() {
        bail!("{status}: {body}");
    }
    let parsed: ModelsResponse = serde_json::from_str(&body)?;
    Ok(parsed
        .data
        .into_iter()
        .map(|model| model.id)
        .filter(|id| !id.is_empty())
        .collect())
}

fn auto_configure_model_tags(paths: &MiyuPaths, provider: &mut ProviderConfig, model: &str) {
    if provider.model_modalities.contains_key(model) {
        return;
    }
    if let Some(modalities) =
        crate::models_cache::input_modalities_blocking(paths, &provider.id, model)
            .filter(|modalities| !modalities.is_empty())
    {
        provider
            .model_modalities
            .insert(model.to_string(), modalities);
    }
}

fn models_url(base_url: &str) -> String {
    let mut url = base_url.trim().trim_end_matches('/').to_string();
    if url.ends_with("/chat/completions") {
        url.truncate(url.len() - "/chat/completions".len());
    }
    if url.ends_with("/v1") {
        format!("{url}/models")
    } else {
        format!("{url}/v1/models")
    }
}

#[derive(Deserialize)]
struct ModelsResponse {
    data: Vec<ModelInfo>,
}

#[derive(Deserialize)]
struct ModelInfo {
    id: String,
}

fn select_active_provider(stdout: &mut io::Stdout, config: &mut AppConfig) -> Result<()> {
    let mut choices = config.text_provider_model_choices();
    if choices.is_empty() {
        message(
            stdout,
            t(
                "No text models are selected. Activate one with Tab under Providers and models first.",
                "没有已勾选的文本模型，请先在供应商和模型里用 Tab 激活模型。",
            ),
        )?;
        return Ok(());
    }
    let mut selected = choices
        .iter()
        .position(|choice| config.is_active_provider_model(&choice.provider_id, &choice.model))
        .unwrap_or(0);
    loop {
        let options = choices
            .iter()
            .map(|choice| {
                let marker = if config.is_active_provider_model(&choice.provider_id, &choice.model)
                {
                    "[*] "
                } else {
                    "[ ] "
                };
                format!("{marker}{}", choice.label())
            })
            .collect::<Vec<_>>();
        draw_menu(
            stdout,
            t(" SELECT TEXT MODEL ", " 选择文本模型 "),
            &options,
            selected,
            t(
                "[Tab]activate/deactivate [Enter/q]confirm [d]remove",
                "[Tab]激活/取消 [Enter/q]确认 [d]移除",
            ),
        )?;
        match read_key()? {
            KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
            KeyCode::Up | KeyCode::Char('k') => selected = selected.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => selected = (selected + 1).min(options.len() - 1),
            KeyCode::Enter => return Ok(()),
            KeyCode::Tab => {
                let choice = choices[selected].clone();
                config.toggle_active_provider_model(&choice.provider_id, &choice.model)?;
            }
            KeyCode::Char('d') => {
                let choice = choices[selected].clone();
                config.remove_active_provider_model(&choice.provider_id, &choice.model)?;
                choices = config.text_provider_model_choices();
                if choices.is_empty() {
                    message(
                        stdout,
                        t(
                            "The active model was removed; no models are currently available.",
                            "已移除激活模型，当前没有可用模型。",
                        ),
                    )?;
                    return Ok(());
                }
                selected = selected.min(choices.len().saturating_sub(1));
            }
            _ => {}
        }
    }
}

use crate::config::EMBEDDING_MODALITY;

fn model_is_embedding(provider: &ProviderConfig, model: &str) -> bool {
    AppConfig::model_is_embedding(provider, model)
}

fn embedding_model_label(config: &AppConfig) -> String {
    if config.embedding.is_configured() {
        format!(
            "{}/{}",
            config.embedding.provider_id.trim(),
            config.embedding.model.trim()
        )
    } else {
        t("not set", "未设置").to_string()
    }
}

fn edit_embedding_model(stdout: &mut io::Stdout, config: &mut AppConfig) -> Result<()> {
    let mut candidates: Vec<(String, String)> = Vec::new();
    for provider in &config.providers {
        for model in &provider.models {
            if model_is_embedding(provider, model) {
                candidates.push((provider.id.clone(), model.clone()));
            }
        }
    }
    if candidates.is_empty() {
        message(
            stdout,
            t(
                "No embedding models yet. Mark one in Providers and models -> Edit model.",
                "还没有语义模型。请在「供应商和模型」->「编辑模型」里把某个模型标记为语义模型。",
            ),
        )?;
        return Ok(());
    }
    let mut options: Vec<String> = candidates
        .iter()
        .map(|(provider, model)| format!("{provider}/{model}"))
        .collect();
    options.push(t("Advanced settings", "高级设置").to_string());
    options.push(t("Clear selection", "清除选择").to_string());
    let mut selected = candidates
        .iter()
        .position(|(provider, model)| {
            provider == config.embedding.provider_id.trim()
                && model == config.embedding.model.trim()
        })
        .unwrap_or(0);
    loop {
        draw_menu(
            stdout,
            t(" EMBEDDING MODEL ", " EMBEDDING 模型 "),
            &options,
            selected,
            t(
                "[Enter]select [j/k]move [q]back",
                "[Enter]选择 [j/k]移动 [q]返回",
            ),
        )?;
        match read_key()? {
            KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
            KeyCode::Up | KeyCode::Char('k') => selected = selected.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => selected = (selected + 1).min(options.len() - 1),
            KeyCode::Enter => {
                if selected == options.len() - 1 {
                    config.embedding.provider_id.clear();
                    config.embedding.model.clear();
                    return Ok(());
                }
                if selected == options.len() - 2 {
                    edit_embedding_advanced(stdout, config)?;
                    continue;
                }
                let (provider, model) = candidates[selected].clone();
                config.embedding.provider_id = provider;
                config.embedding.model = model;
                return Ok(());
            }
            _ => {}
        }
    }
}

fn edit_embedding_advanced(stdout: &mut io::Stdout, config: &mut AppConfig) -> Result<()> {
    let mut fields = vec![
        Field::new(
            t("Request timeout (seconds)", "请求超时（秒）"),
            config.embedding.timeout_seconds.to_string(),
        ),
        Field::new(
            t("Similarity floor (0-1)", "相似度下限（0-1）"),
            config.embedding.min_score.to_string(),
        ),
    ];
    if !run_form(
        stdout,
        t(" EMBEDDING ADVANCED ", " EMBEDDING 高级设置 "),
        &mut fields,
    )? {
        return Ok(());
    }
    let timeout: u64 = fields[0]
        .value
        .trim()
        .parse()
        .map_err(|_| anyhow::anyhow!(t("Invalid timeout.", "超时数值无效。")))?;
    let score: f32 = fields[1]
        .value
        .trim()
        .parse()
        .map_err(|_| anyhow::anyhow!(t("Invalid similarity floor.", "相似度下限无效。")))?;
    if timeout == 0 {
        return Err(anyhow::anyhow!(t(
            "Timeout must be positive.",
            "超时必须大于 0。"
        )));
    }
    if !(0.0..=1.0).contains(&score) {
        return Err(anyhow::anyhow!(t(
            "Similarity floor must be between 0 and 1.",
            "相似度下限必须在 0 与 1 之间。"
        )));
    }
    config.embedding.timeout_seconds = timeout;
    config.embedding.min_score = score;
    Ok(())
}

fn subagent_tiers_label(config: &AppConfig) -> String {
    let counts = crate::config::ModelTier::ALL.map(|tier| config.subagent_tier_choices(tier).len());
    if counts.iter().all(|count| *count == 0) {
        t("not configured", "未配置").to_string()
    } else {
        format!(
            "cheap:{} balanced:{} strong:{}",
            counts[0], counts[1], counts[2]
        )
    }
}

fn tier_display_name(tier: crate::config::ModelTier) -> &'static str {
    use crate::config::ModelTier;
    match tier {
        ModelTier::Cheap => "cheap",
        ModelTier::Balanced => "balanced",
        ModelTier::Strong => "strong",
    }
}

/// Tier pool overview: pick a tier, then toggle models for it. Subagents
/// choose a tier by task complexity; unconfigured pools fall back to the
/// main model pool.
fn select_subagent_tiers(stdout: &mut io::Stdout, config: &mut AppConfig) -> Result<()> {
    use crate::config::ModelTier;
    let mut selected = 0usize;
    loop {
        let options = ModelTier::ALL
            .iter()
            .map(|tier| {
                let pool = config.subagent_tier_choices(*tier);
                let summary = if pool.is_empty() {
                    t("fallback to main model", "回退主模型").to_string()
                } else {
                    pool.iter()
                        .map(|choice| choice.model.clone())
                        .collect::<Vec<_>>()
                        .join(", ")
                };
                let hint = match tier {
                    ModelTier::Cheap => t("simple tasks", "简单任务"),
                    ModelTier::Balanced => t("normal tasks", "普通任务"),
                    ModelTier::Strong => t("complex tasks", "复杂任务"),
                };
                format!("{} ({hint}): {summary}", tier_display_name(*tier))
            })
            .collect::<Vec<_>>();
        draw_menu(
            stdout,
            t(" SUBAGENT TIER POOLS ", " 子代理档位池 "),
            &options,
            selected,
            t(
                "[Enter]configure tier [j/k]move [q]back",
                "[Enter]配置该档位 [j/k]移动 [q]返回",
            ),
        )?;
        match read_key()? {
            KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
            KeyCode::Up | KeyCode::Char('k') => selected = selected.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => selected = (selected + 1).min(options.len() - 1),
            KeyCode::Enter => {
                select_subagent_tier_models(stdout, config, ModelTier::ALL[selected])?
            }
            _ => {}
        }
    }
}

/// Model multi-select for one tier pool, mirroring the text-model picker:
/// candidates are the configured text models, Tab toggles membership.
fn select_subagent_tier_models(
    stdout: &mut io::Stdout,
    config: &mut AppConfig,
    tier: crate::config::ModelTier,
) -> Result<()> {
    let choices = config.text_provider_model_choices();
    if choices.is_empty() {
        message(
            stdout,
            t(
                "No text models are configured. Add models under Providers and models first.",
                "没有可用的文本模型，请先在供应商和模型里添加模型。",
            ),
        )?;
        return Ok(());
    }
    let mut selected = 0usize;
    let title = format!(
        " {} · {} ",
        t("TIER POOL", "档位池"),
        tier_display_name(tier)
    );
    loop {
        let options = choices
            .iter()
            .map(|choice| {
                let marker =
                    if config.is_subagent_tier_model(tier, &choice.provider_id, &choice.model) {
                        "[*] "
                    } else {
                        "[ ] "
                    };
                format!("{marker}{}", choice.label())
            })
            .collect::<Vec<_>>();
        draw_menu(
            stdout,
            &title,
            &options,
            selected,
            t(
                "[Tab]add/remove [Enter/q]confirm",
                "[Tab]加入/移出 [Enter/q]确认",
            ),
        )?;
        match read_key()? {
            KeyCode::Char('q') | KeyCode::Esc | KeyCode::Enter => return Ok(()),
            KeyCode::Up | KeyCode::Char('k') => selected = selected.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => selected = (selected + 1).min(options.len() - 1),
            KeyCode::Tab => {
                let choice = choices[selected].clone();
                config.toggle_subagent_tier_model(tier, &choice.provider_id, &choice.model)?;
            }
            _ => {}
        }
    }
}

fn platforms_label(config: &AppConfig) -> String {
    if config.platforms.qq.enabled {
        t("Tencent QQ enabled", "腾讯 QQ 已启用").to_string()
    } else {
        t("disabled", "未启用").to_string()
    }
}

fn select_platforms(
    stdout: &mut io::Stdout,
    paths: &MiyuPaths,
    config: &mut AppConfig,
) -> Result<()> {
    let mut selected = 0usize;
    loop {
        let state = if config.platforms.qq.enabled {
            t("enabled", "已启用")
        } else {
            t("disabled", "未启用")
        };
        let options = vec![
            format!("{}: {state}", t("Tencent QQ", "腾讯 QQ")),
            format!(
                "{}: {}",
                t("Command trigger prefix", "命令触发前缀"),
                config.platforms.command_prefix
            ),
            t("Command list", "命令列表").to_string(),
        ];
        draw_menu(
            stdout,
            t(" IM PLATFORMS ", " 接入通讯平台 "),
            &options,
            selected,
            t(
                "[Enter]configure [j/k]move [q]back",
                "[Enter]配置 [j/k]移动 [q]返回",
            ),
        )?;
        match read_key()? {
            KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
            KeyCode::Up | KeyCode::Char('k') => selected = selected.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => selected = (selected + 1).min(options.len() - 1),
            KeyCode::Enter => match selected {
                0 => edit_qq(stdout, paths, config)?,
                1 => edit_platform_command_prefix(stdout, config)?,
                2 => select_platform_commands(stdout, config)?,
                _ => {}
            },
            _ => {}
        }
    }
}

fn edit_platform_command_prefix(stdout: &mut io::Stdout, config: &mut AppConfig) -> Result<()> {
    let Some(value) = edit_inline_value(
        stdout,
        t(" COMMAND TRIGGER PREFIX ", " 命令触发前缀 "),
        &config.platforms.command_prefix,
        false,
    )?
    else {
        return Ok(());
    };
    let value = value.trim();
    if value.is_empty()
        || value.chars().count() > MAX_PLATFORM_COMMAND_PREFIX_CHARS
        || value
            .chars()
            .any(|character| character.is_whitespace() || character.is_control())
    {
        message(
            stdout,
            t(
                "The prefix must be 1-32 characters and cannot contain whitespace.",
                "前缀必须为 1 到 32 个字符，且不能包含空白字符。",
            ),
        )?;
    } else {
        config.platforms.command_prefix = value.to_string();
    }
    Ok(())
}

fn platform_command_permission_label(permission: PlatformCommandPermission) -> &'static str {
    match permission {
        PlatformCommandPermission::Everyone => t("Everyone", "所有人"),
        PlatformCommandPermission::AdminOnly => t("Administrators only", "仅管理员"),
    }
}

fn select_platform_commands(stdout: &mut io::Stdout, config: &mut AppConfig) -> Result<()> {
    let mut selected = 0usize;
    loop {
        let options = commands::BUILTIN_COMMANDS
            .iter()
            .map(|command| {
                let permission = config
                    .platforms
                    .command_permission(command.id, command.default_permission);
                format!(
                    "{}: {}",
                    command.id,
                    platform_command_permission_label(permission)
                )
            })
            .collect::<Vec<_>>();
        draw_menu(
            stdout,
            t(" PLATFORM COMMANDS ", " 命令列表 "),
            &options,
            selected,
            t(
                "[Enter]set permission [j/k]move [q]back",
                "[Enter]设置权限 [j/k]移动 [q]返回",
            ),
        )?;
        match read_key()? {
            KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
            KeyCode::Up | KeyCode::Char('k') => selected = selected.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => selected = (selected + 1).min(options.len() - 1),
            KeyCode::Enter => {
                edit_platform_command_permission(
                    stdout,
                    config,
                    &commands::BUILTIN_COMMANDS[selected],
                )?;
            }
            _ => {}
        }
    }
}

fn edit_platform_command_permission(
    stdout: &mut io::Stdout,
    config: &mut AppConfig,
    command: &PlatformCommandDescriptor,
) -> Result<()> {
    let permissions = [
        PlatformCommandPermission::Everyone,
        PlatformCommandPermission::AdminOnly,
    ];
    let current = config
        .platforms
        .command_permission(command.id, command.default_permission);
    let mut selected = permissions
        .iter()
        .position(|permission| *permission == current)
        .unwrap_or(0);
    loop {
        let options = permissions
            .iter()
            .map(|permission| platform_command_permission_label(*permission).to_string())
            .collect::<Vec<_>>();
        let title = format!(" {} · {} ", t("COMMAND PERMISSION", "命令权限"), command.id);
        draw_menu(stdout, &title, &options, selected, "")?;
        match read_key()? {
            KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
            KeyCode::Up | KeyCode::Char('k') => selected = selected.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => {
                selected = (selected + 1).min(permissions.len() - 1)
            }
            KeyCode::Enter => {
                config.platforms.set_command_permission(
                    command.id,
                    permissions[selected],
                    command.default_permission,
                );
                return Ok(());
            }
            _ => {}
        }
    }
}

fn enabled_label(value: bool) -> &'static str {
    if value {
        t("enabled", "已启用")
    } else {
        t("disabled", "已禁用")
    }
}

fn edit_qq(stdout: &mut io::Stdout, paths: &MiyuPaths, config: &mut AppConfig) -> Result<()> {
    let mut selected = 0usize;
    loop {
        let qq = &config.platforms.qq;
        let options = vec![
            format!(
                "{}: {}",
                t("Enabled", "是否启用"),
                enabled_label(qq.enabled)
            ),
            format!(
                "{}: {}",
                t("Text model pool", "文本模型池"),
                qq_pool_summary(qq.text_models.as_deref())
            ),
            format!(
                "{}: {}",
                t("Multimodal model pool", "多模态模型池"),
                qq_pool_summary(qq.multimodal_models.as_deref())
            ),
            format!(
                "{}: {}",
                t("Reverse WebSocket port", "反向 WebSocket 端口"),
                qq.reverse_ws_port
            ),
            format!(
                "{}: {}",
                t("Reverse WebSocket token", "反向 WebSocket 验证 Token"),
                if qq.access_token.is_empty() {
                    t("empty", "未设置")
                } else {
                    "********"
                }
            ),
            format!(
                "{}: {}",
                t("User identification", "用户识别"),
                enabled_label(qq.user_identification)
            ),
            format!(
                "{}: {}",
                t("Show group name", "显示群名称"),
                enabled_label(qq.show_group_name)
            ),
            format!(
                "{}: {}",
                t("Write persona memory", "写入人格记忆"),
                enabled_label(qq.memory.write_enabled)
            ),
            format!(
                "{}: {}",
                t(
                    "Administrator QQ ids allowed to use the terminal",
                    "允许使用终端的管理员 QQ 号"
                ),
                qq.admin_users.len()
            ),
            format!(
                "{}: {}",
                t(
                    "Allow non-admin computer access",
                    "是否允许非管理员使用电脑"
                ),
                enabled_label(qq.allow_non_admin_host_tools)
            ),
            format!(
                "{}: {}",
                t(
                    "Send intermediate messages in group chats",
                    "群聊是否输出中间消息"
                ),
                enabled_label(qq.group_intermediate_messages)
            ),
            format!(
                "{}: {}",
                t(
                    "Send intermediate messages in private chats",
                    "私聊是否输出中间消息"
                ),
                enabled_label(qq.private_intermediate_messages)
            ),
            format!(
                "{}: {}",
                t("Private whitelist", "私聊白名单"),
                qq.private_chats.whitelist.len()
            ),
            format!(
                "{}: {}",
                t("Non-whitelist model pool", "非白名单模型池"),
                route_pool_summary(
                    qq.non_whitelist_text_models.as_deref(),
                    PlatformModelPoolInheritance::Platform,
                )
            ),
            format!(
                "{}: {}",
                t(
                    "Only private whitelist can add friends",
                    "仅私聊白名单能加好友"
                ),
                enabled_label(qq.private_chats.friend_requests_require_private_whitelist)
            ),
            format!(
                "{}: {}",
                t("Allow non-whitelist private chats", "是否允许非白名单私聊"),
                enabled_label(qq.private_chats.allow_non_whitelist)
            ),
            format!(
                "{}: {}",
                t("Non-whitelist private rate limit", "非白名单私聊限流"),
                rate_limit_label(qq.private_chats.non_whitelist_rate_limit)
            ),
            format!(
                "{}: {}",
                t("Group whitelist", "群聊白名单"),
                qq.group_chats.whitelist.len()
            ),
            format!(
                "{}: {}",
                t("Additional group wake keywords", "额外群聊触发关键词"),
                qq.group_chats.trigger_keywords.len()
            ),
            format!(
                "{}: {}",
                t("Whitelist-group rate limit", "白名单群聊限流"),
                rate_limit_label(qq.group_chats.whitelist_rate_limit)
            ),
            format!(
                "{}: {}",
                t("Allow non-whitelist groups", "是否允许非白名单群聊"),
                enabled_label(qq.group_chats.allow_non_whitelist)
            ),
            format!(
                "{}: {}",
                t("Non-whitelist-group rate limit", "非白名单群聊限流"),
                rate_limit_label(qq.group_chats.non_whitelist_rate_limit)
            ),
            format!(
                "{}: {}",
                t("Conversation concurrency", "会话并发"),
                session_limits_label(qq.session_limits)
            ),
            format!(
                "{}: {}",
                t("Private/group conversation settings", "私聊/群聊专属配置"),
                qq.conversations.len()
            ),
            t("QQ plugins", "QQ 插件配置").to_string(),
            t("Advanced settings", "高级设置").to_string(),
        ];
        draw_menu(
            stdout,
            t(" TENCENT QQ ", " 腾讯 QQ "),
            &options,
            selected,
            "",
        )?;
        let key = read_key()?;
        match key {
            KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
            KeyCode::Up | KeyCode::Char('k') => selected = selected.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => selected = (selected + 1).min(options.len() - 1),
            KeyCode::Enter | KeyCode::Char(' ') => match selected {
                0 => config.platforms.qq.enabled = !config.platforms.qq.enabled,
                1 if matches!(key, KeyCode::Enter) => select_qq_model_pool(stdout, config, false)?,
                2 if matches!(key, KeyCode::Enter) => select_qq_model_pool(stdout, config, true)?,
                3 if matches!(key, KeyCode::Enter) => {
                    if let Some(value) = edit_u16_value(
                        stdout,
                        t("Reverse WebSocket port", "反向 WebSocket 端口"),
                        config.platforms.qq.reverse_ws_port,
                    )? {
                        if value == 0 {
                            message(
                                stdout,
                                t(
                                    "Port must be between 1 and 65535.",
                                    "端口必须在 1 到 65535 之间。",
                                ),
                            )?;
                        } else {
                            config.platforms.qq.reverse_ws_port = value;
                        }
                    }
                }
                4 if matches!(key, KeyCode::Enter) => edit_qq_token(stdout, config)?,
                5 => {
                    config.platforms.qq.user_identification =
                        !config.platforms.qq.user_identification
                }
                6 => config.platforms.qq.show_group_name = !config.platforms.qq.show_group_name,
                7 => {
                    config.platforms.qq.memory.write_enabled =
                        !config.platforms.qq.memory.write_enabled
                }
                8 if matches!(key, KeyCode::Enter) => edit_qq_id_list(
                    stdout,
                    t(
                        " TERMINAL-ENABLED ADMINISTRATORS ",
                        " 允许使用终端的管理员 QQ 号 ",
                    ),
                    t("QQ id", "QQ 号"),
                    &mut config.platforms.qq.admin_users,
                )?,
                9 => {
                    config.platforms.qq.allow_non_admin_host_tools =
                        !config.platforms.qq.allow_non_admin_host_tools
                }
                10 => {
                    config.platforms.qq.group_intermediate_messages =
                        !config.platforms.qq.group_intermediate_messages
                }
                11 => {
                    config.platforms.qq.private_intermediate_messages =
                        !config.platforms.qq.private_intermediate_messages
                }
                12 if matches!(key, KeyCode::Enter) => edit_qq_id_list(
                    stdout,
                    t(" PRIVATE WHITELIST ", " 私聊白名单 "),
                    t("QQ id", "QQ 号"),
                    &mut config.platforms.qq.private_chats.whitelist,
                )?,
                13 if matches!(key, KeyCode::Enter) => {
                    select_non_whitelist_model_pool(stdout, config)?
                }
                14 => {
                    config
                        .platforms
                        .qq
                        .private_chats
                        .friend_requests_require_private_whitelist = !config
                        .platforms
                        .qq
                        .private_chats
                        .friend_requests_require_private_whitelist
                }
                15 => {
                    config.platforms.qq.private_chats.allow_non_whitelist =
                        !config.platforms.qq.private_chats.allow_non_whitelist
                }
                16 if matches!(key, KeyCode::Enter) => {
                    edit_platform_rate_limit(
                        stdout,
                        &mut config.platforms.qq.private_chats.non_whitelist_rate_limit,
                    )?;
                }
                17 if matches!(key, KeyCode::Enter) => edit_qq_id_list(
                    stdout,
                    t(" GROUP WHITELIST ", " 群聊白名单 "),
                    t("Group id", "群号"),
                    &mut config.platforms.qq.group_chats.whitelist,
                )?,
                18 if matches!(key, KeyCode::Enter) => edit_keyword_list(
                    stdout,
                    &mut config.platforms.qq.group_chats.trigger_keywords,
                )?,
                19 if matches!(key, KeyCode::Enter) => {
                    edit_platform_rate_limit(
                        stdout,
                        &mut config.platforms.qq.group_chats.whitelist_rate_limit,
                    )?;
                }
                20 => {
                    config.platforms.qq.group_chats.allow_non_whitelist =
                        !config.platforms.qq.group_chats.allow_non_whitelist
                }
                21 if matches!(key, KeyCode::Enter) => {
                    edit_platform_rate_limit(
                        stdout,
                        &mut config.platforms.qq.group_chats.non_whitelist_rate_limit,
                    )?;
                }
                22 if matches!(key, KeyCode::Enter) => {
                    edit_platform_session_limits(stdout, &mut config.platforms.qq.session_limits)?
                }
                23 if matches!(key, KeyCode::Enter) => {
                    select_platform_model_routes(stdout, paths, config)?
                }
                24 if matches!(key, KeyCode::Enter) => {
                    select_platform_plugins(stdout, paths, config)?
                }
                25 if matches!(key, KeyCode::Enter) => edit_qq_advanced(stdout, config)?,
                _ => {}
            },
            _ => {}
        }
    }
}

fn session_limits_label(limits: PlatformSessionLimits) -> String {
    format!(
        "{} {} + {} {}",
        limits.running,
        t("running", "运行"),
        limits.queued,
        t("queued", "等待")
    )
}

fn edit_platform_session_limits(
    stdout: &mut io::Stdout,
    limits: &mut PlatformSessionLimits,
) -> Result<()> {
    let mut fields = vec![
        Field::new(
            t("Running turns", "并行运行数量"),
            limits.running.to_string(),
        ),
        Field::new(t("Queued turns", "等待队列数量"), limits.queued.to_string()),
    ];
    if !run_form_editing(
        stdout,
        t(" CONVERSATION CONCURRENCY ", " 会话并发 "),
        &mut fields,
    )? {
        return Ok(());
    }
    let running = fields[0].value.trim().parse::<usize>()?;
    let queued = fields[1].value.trim().parse::<usize>()?;
    if !(1..=MAX_PLATFORM_SESSION_RUNNING).contains(&running)
        || queued > MAX_PLATFORM_SESSION_QUEUED
    {
        message(
            stdout,
            t(
                "Concurrency values are outside the supported range.",
                "并发数值超出支持范围。",
            ),
        )?;
        return Ok(());
    }
    *limits = PlatformSessionLimits { running, queued };
    Ok(())
}

fn rate_limit_label(limit: PlatformRateLimit) -> String {
    if limit.max_messages == 0 {
        return t("unlimited", "不限").to_string();
    }
    format!(
        "{} / {} {}",
        limit.max_messages,
        limit.window_seconds,
        t("seconds", "秒")
    )
}

/// Both numbers live on one form, the way `edit_platform_session_limits`
/// already does it. The menu row above already renders "N / M 秒", so routing
/// Enter through a two-item submenu only restated that summary before letting
/// anyone type — two keypresses to reach a field that was never in doubt.
fn edit_platform_rate_limit(stdout: &mut io::Stdout, limit: &mut PlatformRateLimit) -> Result<()> {
    let mut fields = vec![
        Field::new(
            t(
                "Maximum messages (0 = unlimited)",
                "窗口内消息上限（0 = 不限）",
            ),
            limit.max_messages.to_string(),
        ),
        Field::new(
            t("Window seconds (1-86400)", "窗口秒数（1-86400）"),
            limit.window_seconds.to_string(),
        ),
    ];
    if !run_form_editing(stdout, t(" RATE LIMIT ", " 限流配置 "), &mut fields)? {
        return Ok(());
    }
    let (Ok(max_messages), Ok(window_seconds)) = (
        fields[0].value.trim().parse::<u32>(),
        fields[1].value.trim().parse::<u32>(),
    ) else {
        message(stdout, t("Invalid number.", "数值无效。"))?;
        return Ok(());
    };
    if !(1..=86_400).contains(&window_seconds) {
        message(
            stdout,
            t(
                "Window seconds must be between 1 and 86400.",
                "窗口秒数必须在 1 到 86400 之间。",
            ),
        )?;
        return Ok(());
    }
    *limit = PlatformRateLimit {
        max_messages,
        window_seconds,
    };
    Ok(())
}

fn edit_u16_value(
    stdout: &mut io::Stdout,
    label: &'static str,
    current: u16,
) -> Result<Option<u16>> {
    let mut fields = vec![Field::new(label, current.to_string())];
    if !run_form_editing(stdout, t(" EDIT VALUE ", " 编辑数值 "), &mut fields)? {
        return Ok(None);
    }
    match fields[0].value.trim().parse() {
        Ok(value) => Ok(Some(value)),
        Err(_) => {
            message(stdout, t("Invalid number.", "数值无效。"))?;
            Ok(None)
        }
    }
}

fn edit_qq_token(stdout: &mut io::Stdout, config: &mut AppConfig) -> Result<()> {
    if let Some(value) = edit_inline_value(
        stdout,
        t(" REVERSE WEBSOCKET TOKEN ", " 反向 WebSocket 验证 Token "),
        &config.platforms.qq.access_token,
        true,
    )? {
        config.platforms.qq.access_token = value.trim().to_string();
    }
    Ok(())
}

fn parse_positive_id(value: &str) -> std::result::Result<i64, String> {
    value
        .trim()
        .parse::<i64>()
        .ok()
        .filter(|id| *id > 0)
        .ok_or_else(|| {
            t(
                "QQ/group id must be a positive integer.",
                "QQ 号/群号必须是正整数。",
            )
            .to_string()
        })
}

fn parse_id_lines(value: &str) -> std::result::Result<Vec<i64>, String> {
    let mut parsed = Vec::new();
    for (index, line) in value.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let id = parse_positive_id(line)
            .map_err(|error| format!("{} {}: {error}", t("Line", "第"), index + 1))?;
        if !parsed.contains(&id) {
            parsed.push(id);
        }
    }
    Ok(parsed)
}

fn prompt_single_id(
    stdout: &mut io::Stdout,
    item_label: &str,
    current: Option<i64>,
) -> Result<Option<i64>> {
    let action = if current.is_some() {
        t("Edit", "编辑")
    } else {
        t("Add", "新增")
    };
    let title = format!(" {action} {item_label} ");
    let Some(value) = edit_inline_value(
        stdout,
        &title,
        &current.map(|id| id.to_string()).unwrap_or_default(),
        false,
    )?
    else {
        return Ok(None);
    };
    match parse_positive_id(&value) {
        Ok(id) => Ok(Some(id)),
        Err(error) => {
            message(stdout, &error)?;
            Ok(None)
        }
    }
}

fn edit_qq_id_list(
    stdout: &mut io::Stdout,
    title: &'static str,
    item_label: &'static str,
    ids: &mut Vec<i64>,
) -> Result<()> {
    let mut selected = 0usize;
    loop {
        let mut options = vec![
            t("+ Add one", "+ 新增一项").to_string(),
            t("+ Add multiple", "+ 批量新增").to_string(),
        ];
        options.extend(ids.iter().map(ToString::to_string));
        draw_menu(
            stdout,
            title,
            &options,
            selected,
            t(
                "[Enter]add/edit [Delete]remove [j/k]move [q]back",
                "[Enter]新增/编辑 [Delete]删除 [j/k]移动 [q]返回",
            ),
        )?;
        match read_key()? {
            KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
            KeyCode::Up | KeyCode::Char('k') => selected = selected.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => selected = (selected + 1).min(options.len() - 1),
            KeyCode::Enter if selected == 0 => {
                if let Some(id) = prompt_single_id(stdout, item_label, None)? {
                    if !ids.contains(&id) {
                        ids.push(id);
                    }
                }
            }
            KeyCode::Enter if selected == 1 => {
                let mut value = String::new();
                loop {
                    edit_textarea(stdout, &mut value)?;
                    match parse_id_lines(&value) {
                        Ok(additions) => {
                            for id in additions {
                                if !ids.contains(&id) {
                                    ids.push(id);
                                }
                            }
                            break;
                        }
                        Err(error) => message(stdout, &error.to_string())?,
                    }
                }
            }
            KeyCode::Enter => {
                let index = selected - 2;
                if let Some(id) = prompt_single_id(stdout, item_label, ids.get(index).copied())? {
                    if ids
                        .iter()
                        .enumerate()
                        .any(|(other, item)| other != index && *item == id)
                    {
                        message(stdout, t("That id already exists.", "该号码已存在。"))?;
                    } else if let Some(item) = ids.get_mut(index) {
                        *item = id;
                    }
                }
            }
            KeyCode::Delete | KeyCode::Backspace if selected >= 2 => {
                ids.remove(selected - 2);
                selected = selected.min(ids.len() + 1);
            }
            _ => {}
        }
    }
}

fn parse_keyword_lines(value: &str) -> std::result::Result<Vec<String>, String> {
    let mut parsed = Vec::new();
    for (index, line) in value.lines().enumerate() {
        let keyword = line.trim();
        if keyword.is_empty() {
            continue;
        }
        if keyword.chars().count() > 128 || keyword.chars().any(char::is_control) {
            return Err(format!(
                "{} {}: {}",
                t("Line", "第"),
                index + 1,
                t("keyword is invalid or too long", "关键词无效或过长")
            ));
        }
        if !parsed.iter().any(|item| item == keyword) {
            parsed.push(keyword.to_string());
        }
    }
    Ok(parsed)
}

fn edit_keyword_list(stdout: &mut io::Stdout, keywords: &mut Vec<String>) -> Result<()> {
    let mut selected = 0usize;
    loop {
        let mut options = vec![
            t("+ Add one", "+ 新增一项").to_string(),
            t("+ Add multiple", "+ 批量新增").to_string(),
        ];
        options.extend(keywords.iter().cloned());
        draw_menu(
            stdout,
            t(" GROUP WAKE KEYWORDS ", " 群聊触发关键词 "),
            &options,
            selected,
            "",
        )?;
        match read_key()? {
            KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
            KeyCode::Up | KeyCode::Char('k') => selected = selected.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => selected = (selected + 1).min(options.len() - 1),
            KeyCode::Enter if selected == 0 => {
                if let Some(value) =
                    edit_inline_value(stdout, t(" ADD KEYWORD ", " 新增关键词 "), "", false)?
                {
                    match parse_keyword_lines(&value) {
                        Ok(additions) if additions.len() == 1 => {
                            let keyword = additions.into_iter().next().unwrap();
                            if !keywords.contains(&keyword) {
                                keywords.push(keyword);
                            }
                        }
                        _ => message(
                            stdout,
                            t("Enter exactly one valid keyword.", "请输入一个有效关键词。"),
                        )?,
                    }
                }
            }
            KeyCode::Enter if selected == 1 => {
                let mut value = String::new();
                loop {
                    edit_textarea(stdout, &mut value)?;
                    match parse_keyword_lines(&value) {
                        Ok(additions) => {
                            for keyword in additions {
                                if !keywords.contains(&keyword) {
                                    keywords.push(keyword);
                                }
                            }
                            break;
                        }
                        Err(error) => message(stdout, &error.to_string())?,
                    }
                }
            }
            KeyCode::Enter => {
                let index = selected - 2;
                if let Some(value) = edit_inline_value(
                    stdout,
                    t(" EDIT KEYWORD ", " 编辑关键词 "),
                    &keywords[index],
                    false,
                )? {
                    match parse_keyword_lines(&value) {
                        Ok(values) if values.len() == 1 => {
                            let value = values[0].clone();
                            if keywords
                                .iter()
                                .enumerate()
                                .any(|(other, item)| other != index && item == &value)
                            {
                                message(
                                    stdout,
                                    t("That keyword already exists.", "该关键词已存在。"),
                                )?;
                            } else {
                                keywords[index] = value;
                            }
                        }
                        _ => message(
                            stdout,
                            t("Enter exactly one valid keyword.", "请输入一个有效关键词。"),
                        )?,
                    }
                }
            }
            KeyCode::Delete | KeyCode::Backspace if selected >= 2 => {
                keywords.remove(selected - 2);
                selected = selected.min(keywords.len() + 1);
            }
            _ => {}
        }
    }
}

fn edit_qq_advanced(stdout: &mut io::Stdout, config: &mut AppConfig) -> Result<()> {
    let qq = &config.platforms.qq;
    let mut fields = vec![
        Field::new(
            t(
                "Asset base URL (empty = automatic)",
                "文件访问基础 URL（空 = 自动推导）",
            ),
            qq.asset_base_url.clone(),
        ),
        Field::new(
            t(
                "Max reply chars per message (0 = no split)",
                "单条回复最大字数（0 = 不分段）",
            ),
            qq.max_reply_chars.to_string(),
        ),
        Field::new(
            t(
                "Group overflow (compact / pop)",
                "群聊上下文溢出策略（compact 摘要 / pop 丢弃最旧）",
            ),
            qq.group_context.on_overflow.clone(),
        ),
        Field::new(
            t(
                "Group trim batch (0-1, share released per trim)",
                "群聊单次丢弃比例（0-1，一次让出的窗口占比）",
            ),
            qq.group_context.trim_batch_ratio.to_string(),
        ),
    ];
    if run_form(stdout, t(" QQ ADVANCED ", " QQ 高级设置 "), &mut fields)? {
        config.platforms.qq.asset_base_url =
            fields[0].value.trim().trim_end_matches('/').to_string();
        let overflow = fields[2].value.trim().to_ascii_lowercase();
        if !matches!(overflow.as_str(), "compact" | "pop") {
            return Err(anyhow::anyhow!(t(
                "Group overflow must be compact or pop.",
                "群聊溢出策略只能是 compact 或 pop。"
            )));
        }
        let batch: f32 = fields[3].value.trim().parse().map_err(|_| {
            anyhow::anyhow!(t("Invalid group trim batch.", "群聊单次丢弃比例无效。"))
        })?;
        if !(0.0..1.0).contains(&batch) {
            return Err(anyhow::anyhow!(t(
                "Group trim batch must be between 0 and 1.",
                "群聊单次丢弃比例必须在 0 与 1 之间。"
            )));
        }
        config.platforms.qq.group_context.on_overflow = overflow;
        config.platforms.qq.group_context.trim_batch_ratio = batch;
        config.platforms.qq.max_reply_chars = fields[1].value.trim().parse().map_err(|_| {
            anyhow::anyhow!(t("Invalid maximum reply length.", "单条回复最大字数无效。"))
        })?;
    }
    Ok(())
}

fn select_platform_model_routes(
    stdout: &mut io::Stdout,
    paths: &MiyuPaths,
    config: &mut AppConfig,
) -> Result<()> {
    let mut selected = 0usize;
    loop {
        let mut options = Vec::with_capacity(config.platforms.qq.conversations.len() + 1);
        options.push(t("+ Add conversation", "+ 新增会话配置").to_string());
        options.extend(
            config
                .platforms
                .qq
                .conversations
                .iter()
                .map(platform_model_route_label),
        );
        selected = selected.min(options.len().saturating_sub(1));
        draw_menu(
            stdout,
            t(" QQ CONVERSATIONS ", " 私聊/群聊专属配置 "),
            &options,
            selected,
            t(
                "[Enter]add/edit [d]delete [j/k]move [q]back",
                "[Enter]新增/编辑 [d]删除 [j/k]移动 [q]返回",
            ),
        )?;
        match read_key()? {
            KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
            KeyCode::Up | KeyCode::Char('k') => selected = selected.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => {
                selected = (selected + 1).min(options.len().saturating_sub(1));
            }
            KeyCode::Enter if selected == 0 => {
                edit_platform_model_route(stdout, paths, config, None)?
            }
            KeyCode::Enter => edit_platform_model_route(stdout, paths, config, Some(selected - 1))?,
            KeyCode::Char('d') | KeyCode::Delete if selected > 0 => {
                config.platforms.qq.conversations.remove(selected - 1);
                selected = selected.min(config.platforms.qq.conversations.len());
            }
            _ => {}
        }
    }
}

fn platform_model_route_label(route: &PlatformModelRoute) -> String {
    let kind = match route.conversation.kind {
        PlatformConversationKind::Private => t("private", "私聊"),
        PlatformConversationKind::Group => t("group", "群聊"),
    };
    let text = route_pool_summary(route.text_models.as_deref(), route.text_models_inheritance);
    let multimodal = route_pool_summary(
        route.multimodal_models.as_deref(),
        route.multimodal_models_inheritance,
    );
    let prompt = if route.extra_prompt.is_empty() {
        t("none", "无")
    } else {
        t("set", "已设置")
    };
    let persona = platform_persona_summary(&route.persona);
    format!(
        "{kind} {} · {}:{persona} · {}:{text} {}:{multimodal} · {}:{prompt}",
        route.conversation.id,
        t("persona", "人格"),
        t("text", "文本"),
        t("media", "多模态"),
        t("prompt", "提示词")
    )
}

fn edit_platform_model_route(
    stdout: &mut io::Stdout,
    paths: &MiyuPaths,
    config: &mut AppConfig,
    route_index: Option<usize>,
) -> Result<()> {
    let mut route = route_index
        .and_then(|index| config.platforms.qq.conversations.get(index).cloned())
        .unwrap_or_else(|| PlatformModelRoute {
            conversation: PlatformConversationConfig {
                kind: PlatformConversationKind::Private,
                id: String::new(),
            },
            persona: PlatformPersonaOverride::Inherit,
            text_models_inheritance: PlatformModelPoolInheritance::Platform,
            text_models: None,
            multimodal_models_inheritance: PlatformModelPoolInheritance::Platform,
            multimodal_models: None,
            extra_prompt: String::new(),
            session_limits: None,
        });
    let mut selected = 0usize;
    loop {
        let kind_label = platform_conversation_kind_label(route.conversation.kind);
        let id_label = platform_conversation_id_label(route.conversation.kind);
        let options = [
            format!("{}: {}", t("Conversation type", "会话类型"), kind_label,),
            format!(
                "{id_label}: {}",
                if route.conversation.id.is_empty() {
                    t("not set", "未设置")
                } else {
                    route.conversation.id.as_str()
                },
            ),
            format!(
                "{}: {}",
                t("Override AI persona", "覆盖 AI 人格"),
                platform_persona_summary(&route.persona)
            ),
            format!(
                "{}: {}",
                t("Text model pool", "文本模型池"),
                route_pool_summary(route.text_models.as_deref(), route.text_models_inheritance)
            ),
            format!(
                "{}: {}",
                t("Multimodal model pool", "多模态模型池"),
                route_pool_summary(
                    route.multimodal_models.as_deref(),
                    route.multimodal_models_inheritance,
                )
            ),
            format!(
                "{}: {}",
                t("Extra prompt", "额外提示词"),
                if route.extra_prompt.is_empty() {
                    t("none", "未设置")
                } else {
                    t("set", "已设置")
                }
            ),
            format!(
                "{}: {}",
                t("Override concurrency settings", "覆盖并发配置"),
                route
                    .session_limits
                    .map(session_limits_label)
                    .unwrap_or_else(|| t("inherit", "继承").to_string())
            ),
        ];
        draw_menu(
            stdout,
            t(" EDIT QQ CONVERSATION ", " 编辑 QQ 会话配置 "),
            &options,
            selected,
            "",
        )?;
        match read_key()? {
            KeyCode::Char('q') | KeyCode::Esc => {
                route.normalize();
                if let Err(error) = config.validate_platform_model_route(&route) {
                    if route_index.is_none() {
                        return Ok(());
                    }
                    message(stdout, &error.to_string())?;
                    continue;
                }
                if config
                    .platforms
                    .qq
                    .conversations
                    .iter()
                    .enumerate()
                    .any(|(index, existing)| {
                        Some(index) != route_index && existing.identity() == route.identity()
                    })
                {
                    message(
                        stdout,
                        t(
                            "A configuration for this QQ conversation already exists.",
                            "该 QQ 会话的配置已存在。",
                        ),
                    )?;
                    continue;
                }
                match route_index {
                    Some(index) => config.platforms.qq.conversations[index] = route,
                    None => config.platforms.upsert_model_route(route),
                }
                return Ok(());
            }
            KeyCode::Up | KeyCode::Char('k') => selected = selected.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => selected = (selected + 1).min(options.len() - 1),
            KeyCode::Enter => match selected {
                0 => select_platform_conversation_kind(stdout, &mut route.conversation.kind)?,
                1 => {
                    let title = format!(" {id_label} ");
                    if let Some(value) =
                        edit_inline_value(stdout, &title, &route.conversation.id, false)?
                    {
                        route.conversation.id = value.trim().to_string();
                    }
                }
                2 => edit_platform_personas(stdout, paths, config, &mut route.persona)?,
                3 => select_platform_route_models(
                    stdout,
                    config,
                    &mut route.text_models,
                    &mut route.text_models_inheritance,
                    false,
                )?,
                4 => select_platform_route_models(
                    stdout,
                    config,
                    &mut route.multimodal_models,
                    &mut route.multimodal_models_inheritance,
                    true,
                )?,
                5 => edit_conversation_extra_prompt(stdout, &mut route.extra_prompt)?,
                6 => {
                    let enabled = select_bool(
                        stdout,
                        t("Override QQ concurrency", "覆盖 QQ 并发配置"),
                        route.session_limits.is_some(),
                    )?;
                    if enabled {
                        let limits = route
                            .session_limits
                            .get_or_insert(config.platforms.qq.session_limits);
                        edit_platform_session_limits(stdout, limits)?;
                    } else {
                        route.session_limits = None;
                    }
                }
                _ => {}
            },
            _ => {}
        }
    }
}

fn platform_conversation_kind_label(kind: PlatformConversationKind) -> &'static str {
    match kind {
        PlatformConversationKind::Private => t("Private chat", "私聊"),
        PlatformConversationKind::Group => t("Group chat", "群聊"),
    }
}

fn platform_conversation_id_label(kind: PlatformConversationKind) -> &'static str {
    match kind {
        PlatformConversationKind::Private => t("QQ id", "QQ 号"),
        PlatformConversationKind::Group => t("Group id", "群号"),
    }
}

fn platform_persona_summary(persona: &PlatformPersonaOverride) -> String {
    match persona {
        PlatformPersonaOverride::Inherit => {
            t("inherit current persona", "继承当前人格").to_string()
        }
        PlatformPersonaOverride::Miyu => "Miyu".to_string(),
        PlatformPersonaOverride::Custom { name } => persona_display_name(name).to_string(),
    }
}

fn edit_platform_personas(
    stdout: &mut io::Stdout,
    paths: &MiyuPaths,
    config: &mut AppConfig,
    persona: &mut PlatformPersonaOverride,
) -> Result<()> {
    if let Some(updated) = manage_personas(
        stdout,
        paths,
        config,
        PersonaMenuTarget::Platform(persona.clone()),
    )? {
        *persona = updated;
    }
    Ok(())
}

fn select_platform_conversation_kind(
    stdout: &mut io::Stdout,
    kind: &mut PlatformConversationKind,
) -> Result<()> {
    let choices = [
        platform_conversation_kind_label(PlatformConversationKind::Private).to_string(),
        platform_conversation_kind_label(PlatformConversationKind::Group).to_string(),
    ];
    let current = platform_conversation_kind_label(*kind);
    let selected = select_choice(
        stdout,
        t("Conversation type", "会话类型"),
        current,
        &choices,
        "",
        false,
    )?;
    *kind = if selected == choices[1] {
        PlatformConversationKind::Group
    } else {
        PlatformConversationKind::Private
    };
    Ok(())
}

fn edit_conversation_extra_prompt(stdout: &mut io::Stdout, prompt: &mut String) -> Result<()> {
    edit_textarea(stdout, prompt)?;
    Ok(())
}

fn route_pool_summary(
    pool: Option<&[ActiveProviderModelConfig]>,
    inheritance: PlatformModelPoolInheritance,
) -> String {
    match pool {
        None | Some([]) if inheritance == PlatformModelPoolInheritance::Global => {
            t("inherit global", "继承全局池").to_string()
        }
        None | Some([]) => t("inherit platform", "继承 QQ 平台池").to_string(),
        Some(entries) if entries.len() == 1 => {
            format!("{} / {}", entries[0].provider_id, entries[0].model)
        }
        Some(entries) => format!("{} {}", entries.len(), t("models", "个模型")),
    }
}

fn qq_pool_summary(pool: Option<&[ActiveProviderModelConfig]>) -> String {
    match pool {
        None | Some([]) => t("inherit global", "继承全局").to_string(),
        Some(entries) => route_pool_summary(Some(entries), PlatformModelPoolInheritance::Platform),
    }
}

fn select_platform_route_models(
    stdout: &mut io::Stdout,
    config: &AppConfig,
    pool: &mut Option<Vec<ActiveProviderModelConfig>>,
    inheritance: &mut PlatformModelPoolInheritance,
    multimodal: bool,
) -> Result<()> {
    let choices = if multimodal {
        config.multimodal_provider_model_choices()
    } else {
        config.text_provider_model_choices()
    };
    let mut selected = 0usize;
    loop {
        let mut options = Vec::with_capacity(choices.len() + 2);
        let inherit_platform_marker = if pool.as_ref().is_none_or(Vec::is_empty)
            && *inheritance == PlatformModelPoolInheritance::Platform
        {
            "[*] "
        } else {
            "[ ] "
        };
        options.push(format!(
            "{inherit_platform_marker}{}",
            t("Inherit QQ platform model pool", "继承 QQ 平台模型池")
        ));
        let inherit_global_marker = if pool.as_ref().is_none_or(Vec::is_empty)
            && *inheritance == PlatformModelPoolInheritance::Global
        {
            "[*] "
        } else {
            "[ ] "
        };
        options.push(format!(
            "{inherit_global_marker}{}",
            if multimodal {
                t(
                    "Inherit global multimodal model pool",
                    "继承全局多模态模型池",
                )
            } else {
                t("Inherit global model pool", "继承全局模型池")
            }
        ));
        options.extend(choices.iter().map(|choice| {
            let active = pool.as_ref().is_some_and(|entries| {
                entries.iter().any(|entry| {
                    entry.provider_id == choice.provider_id && entry.model == choice.model
                })
            });
            let marker = if active { "[*] " } else { "[ ] " };
            format!("{marker}{}", choice.label())
        }));
        draw_menu(
            stdout,
            if multimodal {
                t(" SESSION MULTIMODAL MODELS ", " 会话多模态模型 ")
            } else {
                t(" SESSION TEXT MODELS ", " 会话文本模型 ")
            },
            &options,
            selected,
            t(
                "[Tab]add/remove [Enter/q]confirm",
                "[Tab]加入/移出 [Enter/q]确认",
            ),
        )?;
        match read_key()? {
            KeyCode::Char('q') | KeyCode::Esc | KeyCode::Enter => return Ok(()),
            KeyCode::Up | KeyCode::Char('k') => selected = selected.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => selected = (selected + 1).min(options.len() - 1),
            KeyCode::Tab if selected == 0 => {
                *pool = None;
                *inheritance = PlatformModelPoolInheritance::Platform;
            }
            KeyCode::Tab if selected == 1 => {
                *pool = None;
                *inheritance = PlatformModelPoolInheritance::Global;
            }
            KeyCode::Tab => {
                *inheritance = PlatformModelPoolInheritance::Platform;
                let choice = &choices[selected - 2];
                let entries = pool.get_or_insert_with(Vec::new);
                if let Some(index) = entries.iter().position(|entry| {
                    entry.provider_id == choice.provider_id && entry.model == choice.model
                }) {
                    entries.remove(index);
                } else {
                    entries.push(ActiveProviderModelConfig {
                        provider_id: choice.provider_id.clone(),
                        model: choice.model.clone(),
                    });
                }
                if entries.is_empty() {
                    *pool = None;
                }
            }
            _ => {}
        }
    }
}

fn select_qq_model_pool(
    stdout: &mut io::Stdout,
    config: &mut AppConfig,
    multimodal: bool,
) -> Result<()> {
    let choices = if multimodal {
        config.multimodal_provider_model_choices()
    } else {
        config.text_provider_model_choices()
    };
    let title = if multimodal {
        t(" QQ MULTIMODAL MODELS ", " QQ 多模态模型 ")
    } else {
        t(" QQ TEXT MODELS ", " QQ 文本模型 ")
    };
    let inherit = if multimodal {
        t(
            "Inherit global multimodal model pool",
            "继承全局多模态模型池",
        )
    } else {
        t("Inherit global model pool", "继承全局模型池")
    };
    select_model_pool(
        stdout,
        choices,
        if multimodal {
            &mut config.platforms.qq.multimodal_models
        } else {
            &mut config.platforms.qq.text_models
        },
        multimodal,
        title,
        inherit,
    )
}

fn select_non_whitelist_model_pool(stdout: &mut io::Stdout, config: &mut AppConfig) -> Result<()> {
    let choices = config.text_provider_model_choices();
    select_model_pool(
        stdout,
        choices,
        &mut config.platforms.qq.non_whitelist_text_models,
        false,
        t(" NON-WHITELIST TEXT MODELS ", " 非白名单模型池 "),
        t("Inherit QQ platform model pool", "继承 QQ 平台模型池"),
    )
}

fn select_model_pool(
    stdout: &mut io::Stdout,
    choices: Vec<ProviderModelChoice>,
    pool: &mut Option<Vec<ActiveProviderModelConfig>>,
    _multimodal: bool,
    title: &str,
    inherit_label: &str,
) -> Result<()> {
    let mut selected = 0usize;
    loop {
        let mut options = Vec::with_capacity(choices.len() + 1);
        let inherit_marker = if pool.as_ref().is_none_or(Vec::is_empty) {
            "[*] "
        } else {
            "[ ] "
        };
        options.push(format!("{inherit_marker}{inherit_label}"));
        options.extend(choices.iter().map(|choice| {
            let active = pool.as_ref().is_some_and(|entries| {
                entries.iter().any(|entry| {
                    entry.provider_id == choice.provider_id && entry.model == choice.model
                })
            });
            format!("{}{}", if active { "[*] " } else { "[ ] " }, choice.label())
        }));
        draw_menu(
            stdout,
            title,
            &options,
            selected,
            t(
                "[Tab]add/remove [Enter/q]confirm",
                "[Tab]加入/移出 [Enter/q]确认",
            ),
        )?;
        match read_key()? {
            KeyCode::Char('q') | KeyCode::Esc | KeyCode::Enter => return Ok(()),
            KeyCode::Up | KeyCode::Char('k') => selected = selected.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => selected = (selected + 1).min(options.len() - 1),
            KeyCode::Tab if selected == 0 => *pool = None,
            KeyCode::Tab => {
                let choice = &choices[selected - 1];
                let entries = pool.get_or_insert_with(Vec::new);
                if let Some(index) = entries.iter().position(|entry| {
                    entry.provider_id == choice.provider_id && entry.model == choice.model
                }) {
                    entries.remove(index);
                } else {
                    entries.push(ActiveProviderModelConfig {
                        provider_id: choice.provider_id.clone(),
                        model: choice.model.clone(),
                    });
                }
                if entries.is_empty() {
                    *pool = None;
                }
            }
            _ => {}
        }
    }
}

const REPLY_PROCESSOR_PLUGIN_ID: &str = "reply_processor";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
struct ReplyProcessorSettingsForm {
    default_enabled: bool,
    threshold: usize,
    mode: String,
    followup_mention: bool,
    strip_period: bool,
    theme: String,
    max_height: u32,
    font_size: u32,
    code_font_size: u32,
    padding: u32,
    context_notice: bool,
    ttl_hours: u64,
    max_records: usize,
    send_tool_intercept: bool,
    font: String,
    title_font: String,
    code_font: String,
    emoji_font: String,
}

impl Default for ReplyProcessorSettingsForm {
    fn default() -> Self {
        Self {
            default_enabled: true,
            threshold: 200,
            mode: "image".to_string(),
            followup_mention: true,
            strip_period: true,
            theme: "paper".to_string(),
            max_height: 2600,
            font_size: 36,
            code_font_size: 30,
            padding: 64,
            context_notice: true,
            ttl_hours: 24,
            max_records: 3,
            send_tool_intercept: true,
            font: String::new(),
            title_font: String::new(),
            code_font: String::new(),
            emoji_font: String::new(),
        }
    }
}

fn select_platform_plugins(
    stdout: &mut io::Stdout,
    paths: &MiyuPaths,
    config: &mut AppConfig,
) -> Result<()> {
    let mut selected = 0usize;
    loop {
        let reply_enabled = config
            .platforms
            .qq
            .plugins
            .get(REPLY_PROCESSOR_PLUGIN_ID)
            .map(|plugin| plugin.enabled_or(true))
            .unwrap_or(true);
        let reply_state = if reply_enabled {
            t("enabled", "已启用")
        } else {
            t("disabled", "未启用")
        };
        let real_context_enabled = config
            .platforms
            .qq
            .plugins
            .get(REAL_CONTEXT_PLUGIN_ID)
            .map(|plugin| plugin.enabled_or(true))
            .unwrap_or(true);
        let real_context_state = if real_context_enabled {
            t("enabled", "已启用")
        } else {
            t("disabled", "未启用")
        };
        let message_history_enabled = config
            .platforms
            .qq
            .plugins
            .get(QQ_MESSAGE_HISTORY_PLUGIN_ID)
            .map(|plugin| plugin.enabled_or(true))
            .unwrap_or(true);
        let message_history_state = if message_history_enabled {
            t("enabled", "已启用")
        } else {
            t("disabled", "未启用")
        };
        let meme_collector_enabled = config
            .platforms
            .qq
            .plugins
            .get(QQ_MEME_COLLECTOR_PLUGIN_ID)
            .map(|plugin| plugin.enabled_or(true))
            .unwrap_or(true);
        let meme_collector_state = if meme_collector_enabled {
            t("enabled", "已启用")
        } else {
            t("disabled", "未启用")
        };
        let options = [
            format!("{}: {reply_state}", t("Reply processor", "回复处理")),
            format!(
                "{}: {real_context_state}",
                t("Group real-context replies", "群聊真实上下文回复")
            ),
            format!(
                "{}: {message_history_state}",
                t("QQ text message history", "QQ 纯文字消息历史")
            ),
            format!(
                "{}: {meme_collector_state}",
                t("QQ meme pocket", "QQ 表情口袋")
            ),
        ];
        draw_menu(
            stdout,
            t(" TENCENT QQ PLUGINS ", " QQ 插件配置 "),
            &options,
            selected,
            t(
                "[Enter]configure [j/k]move [q]back",
                "[Enter]配置 [j/k]移动 [q]返回",
            ),
        )?;
        match read_key()? {
            KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
            KeyCode::Up | KeyCode::Char('k') => selected = selected.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => selected = (selected + 1).min(options.len() - 1),
            KeyCode::Enter => match selected {
                0 => edit_reply_processor(stdout, config)?,
                1 => edit_real_context(stdout, paths, config)?,
                2 => edit_message_history(stdout, config)?,
                3 => edit_meme_collector(stdout, config)?,
                _ => {}
            },
            _ => {}
        }
    }
}

fn edit_message_history(stdout: &mut io::Stdout, config: &mut AppConfig) -> Result<()> {
    let instance = config
        .platforms
        .qq
        .plugins
        .get(QQ_MESSAGE_HISTORY_PLUGIN_ID);
    let enabled = instance.map(|value| value.enabled_or(true)).unwrap_or(true);
    let settings = instance
        .map(QqMessageHistoryPluginSettings::from_instance)
        .transpose()?
        .unwrap_or_default();
    let mut fields = vec![
        Field::boolean(t("Plugin", "插件状态"), enabled),
        Field::new(
            t(
                "Maximum query results (0 = safety limit)",
                "查询工具单次最多返回（0=安全页上限）",
            ),
            settings.history_search_max_results.to_string(),
        ),
        Field::new(
            t("Query safety page limit", "查询安全页上限"),
            settings.history_safe_page_limit.to_string(),
        ),
        Field::boolean(
            t(
                "Allow administrators to access other conversations",
                "允许管理员访问其他会话",
            ),
            settings.allow_cross_conversation_search,
        ),
    ];
    if !run_form(
        stdout,
        t(" QQ TEXT MESSAGE HISTORY ", " QQ 纯文字消息历史 "),
        &mut fields,
    )? {
        return Ok(());
    }
    let enabled = fields[0].value.parse::<bool>()?;
    let settings = QqMessageHistoryPluginSettings {
        history_search_max_results: fields[1].value.trim().parse()?,
        history_safe_page_limit: fields[2].value.trim().parse()?,
        allow_cross_conversation_search: fields[3].value.parse()?,
    };
    settings.validate()?;
    let mut candidate = config.clone();
    let instance = candidate
        .platforms
        .qq
        .plugins
        .entry(QQ_MESSAGE_HISTORY_PLUGIN_ID.to_string())
        .or_default();
    instance.enabled = (!enabled).then_some(false);
    instance.settings.insert(
        "history_search_max_results".to_string(),
        serde_json::json!(settings.history_search_max_results),
    );
    instance.settings.insert(
        "history_safe_page_limit".to_string(),
        serde_json::json!(settings.history_safe_page_limit),
    );
    instance.settings.insert(
        "allow_cross_conversation_search".to_string(),
        serde_json::json!(settings.allow_cross_conversation_search),
    );
    candidate.normalize_platform_model_routes();
    candidate.validate()?;
    *config = candidate;
    Ok(())
}

fn edit_meme_collector(stdout: &mut io::Stdout, config: &mut AppConfig) -> Result<()> {
    let instance = config.platforms.qq.plugins.get(QQ_MEME_COLLECTOR_PLUGIN_ID);
    let enabled = instance.map(|value| value.enabled_or(true)).unwrap_or(true);
    let settings = instance
        .map(QqMemeCollectorPluginSettings::from_instance)
        .transpose()?
        .unwrap_or_default();
    let mut fields = vec![
        Field::boolean(t("Plugin", "插件状态"), enabled),
        Field::new(
            t("Collection probability (0..1)", "收图概率（0..1）"),
            settings.collect_probability.to_string(),
        ),
        Field::new(
            t("Maximum images per message", "每条消息最多图片数"),
            settings.max_images_per_message.to_string(),
        ),
        Field::boolean(
            t(
                "Allow non-admin save meme tool",
                "允许非管理员使用存表情工具",
            ),
            settings.allow_non_admin_save_tool,
        ),
    ];
    if !run_form(stdout, t(" QQ MEME POCKET ", " QQ 表情口袋 "), &mut fields)? {
        return Ok(());
    }
    let enabled = fields[0].value.parse::<bool>()?;
    let collect_probability = fields[1].value.trim().parse::<f64>()?;
    let max_images_per_message = fields[2].value.trim().parse::<usize>()?;
    let allow_non_admin_save_tool = fields[3].value.parse::<bool>()?;
    let mut candidate = config.clone();
    let instance = candidate
        .platforms
        .qq
        .plugins
        .entry(QQ_MEME_COLLECTOR_PLUGIN_ID.to_string())
        .or_default();
    instance.enabled = (!enabled).then_some(false);
    instance.settings.insert(
        "collect_probability".to_string(),
        serde_json::json!(collect_probability),
    );
    instance.settings.insert(
        "max_images_per_message".to_string(),
        serde_json::json!(max_images_per_message),
    );
    instance.settings.insert(
        "allow_non_admin_save_tool".to_string(),
        serde_json::json!(allow_non_admin_save_tool),
    );
    if let Err(error) = candidate.validate() {
        message(stdout, &error.to_string())?;
        return Ok(());
    }
    *config = candidate;
    Ok(())
}

fn real_context_values(config: &AppConfig) -> Result<(bool, RealContextPluginSettings)> {
    let Some(instance) = config.platforms.qq.plugins.get(REAL_CONTEXT_PLUGIN_ID) else {
        return Ok((true, RealContextPluginSettings::default()));
    };
    Ok((
        instance.enabled_or(true),
        RealContextPluginSettings::from_instance(instance)?,
    ))
}

fn apply_real_context_values(
    config: &mut AppConfig,
    enabled: bool,
    settings: &RealContextPluginSettings,
) {
    let instance = config
        .platforms
        .qq
        .plugins
        .entry(REAL_CONTEXT_PLUGIN_ID.to_string())
        .or_default();
    instance.enabled = (!enabled).then_some(false);
    merge_real_context_settings(instance, settings);
}

fn edit_real_context(
    stdout: &mut io::Stdout,
    paths: &MiyuPaths,
    config: &mut AppConfig,
) -> Result<()> {
    let (mut enabled, mut settings) = real_context_values(config)?;
    let mut selected = 0usize;
    loop {
        let state = if enabled {
            t("enabled", "已启用")
        } else {
            t("disabled", "未启用")
        };
        let options = vec![
            format!("{}: {state}", t("Plugin", "插件状态")),
            format!(
                "{}: {}",
                t("Text model pool", "文本模型池"),
                real_context_model_pool_summary(settings.text_models.as_deref())
            ),
            format!(
                "{}: {}",
                t("Reply context window", "回复上下文消息数"),
                settings.reply_context_window
            ),
            t("Group member information", "群成员信息查询").to_string(),
            t("Active reply judgement", "主动回复判断").to_string(),
            t("Quote, mention, and reactions", "引用艾特和贴表情").to_string(),
            t("Safety checks", "违规判断").to_string(),
            t("Affection and relationship", "好感度与关系").to_string(),
            t("Identity mappings", "识人映射").to_string(),
        ];
        draw_menu(
            stdout,
            t(" GROUP REAL CONTEXT ", " 群聊真实上下文回复 "),
            &options,
            selected,
            "",
        )?;
        match read_key()? {
            KeyCode::Char('q') | KeyCode::Esc => {
                settings.normalize();
                let mut candidate = config.clone();
                apply_real_context_values(&mut candidate, enabled, &settings);
                if let Err(error) = candidate.validate() {
                    message(stdout, &error.to_string())?;
                } else {
                    apply_real_context_values(config, enabled, &settings);
                    return Ok(());
                }
            }
            KeyCode::Up | KeyCode::Char('k') => selected = selected.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => selected = (selected + 1).min(options.len() - 1),
            KeyCode::Enter => match selected {
                0 => enabled = select_bool(stdout, t("Plugin", "插件状态"), enabled)?,
                1 => select_real_context_model_pool(stdout, config, &mut settings.text_models)?,
                2 => edit_real_context_number(
                    stdout,
                    t("Reply context window", "回复上下文消息数"),
                    settings.reply_context_window,
                    &mut settings,
                    |candidate, value| candidate.reply_context_window = value,
                )?,
                3 => edit_real_context_history(stdout, &mut settings)?,
                4 => match StateStore::new(paths) {
                    Ok(state) => edit_real_context_active_reply(stdout, &state, &mut settings)?,
                    Err(error) => message(
                        stdout,
                        &format!(
                            "{}: {error}",
                            t("Unable to open persistent state", "无法打开持久状态数据库")
                        ),
                    )?,
                },
                5 => edit_real_context_reply_target(stdout, &mut settings)?,
                6 => edit_real_context_moderation(stdout, &mut settings)?,
                7 => edit_real_context_affection(stdout, config, &mut settings)?,
                8 => edit_real_context_identities(stdout, &mut settings)?,
                _ => {}
            },
            _ => {}
        }
    }
}

fn edit_real_context_history(
    stdout: &mut io::Stdout,
    settings: &mut RealContextPluginSettings,
) -> Result<()> {
    loop {
        let mut fields = vec![Field::new(
            t(
                "Maximum group member search results",
                "群成员搜索工具最大返回数量",
            ),
            settings.group_member_search_max_results.to_string(),
        )];
        if !run_form(
            stdout,
            t(" GROUP MEMBER INFORMATION ", " 群成员信息查询 "),
            &mut fields,
        )? {
            return Ok(());
        }
        let mut candidate = settings.clone();
        let parsed = (|| -> std::result::Result<(), String> {
            candidate.group_member_search_max_results = real_context_value(&fields, 0)?;
            candidate.validate().map_err(|error| error.to_string())
        })();
        match parsed {
            Ok(()) => {
                *settings = candidate;
                return Ok(());
            }
            Err(error) => message(stdout, &error)?,
        }
    }
}

fn edit_real_context_active_reply(
    stdout: &mut io::Stdout,
    state: &StateStore,
    settings: &mut RealContextPluginSettings,
) -> Result<()> {
    let mut selected = 0usize;
    loop {
        let skip_list_summary = active_judgement_skip_ids(state)
            .map(|ids| ids.len().to_string())
            .unwrap_or_else(|_| t("unavailable", "不可用").to_string());
        let options = vec![
            format!(
                "{}: {}",
                t("Scoring and restraint", "评分与克制"),
                boolean_label(settings.active_reply_enable)
            ),
            format!(
                "{}: {}",
                t("Inherit persona during judgement", "判断时继承人格"),
                boolean_label(settings.judge_include_persona)
            ),
            format!(
                "{}: {}",
                t("Custom prompt", "自定义提示词"),
                if settings.judge_persona_prompt.trim().is_empty() {
                    t("none", "未设置")
                } else {
                    t("set", "已设置")
                }
            ),
            format!(
                "{}: {}",
                t("Random judgement probability", "随机进入判断的概率"),
                settings.active_judge_probability
            ),
            format!(
                "{}: {}",
                t("Reply threshold", "回复阈值"),
                settings.reply_threshold
            ),
            format!(
                "{}: {}",
                t("Skip image-only messages", "跳过纯图片消息"),
                boolean_label(settings.skip_pure_image_active_judge)
            ),
            format!(
                "{}: {}",
                t("QQ ids that skip active judgement", "跳过主动判断的 QQ 号"),
                skip_list_summary
            ),
            format!(
                "{}: {}",
                t(
                    "New message supersedes pending judgement",
                    "新消息覆盖待判断消息",
                ),
                boolean_label(settings.active_reply_supersede_enable)
            ),
            format!(
                "{}: {}",
                t("Supersede window (seconds)", "覆盖窗口（秒）"),
                settings.active_reply_supersede_window_seconds
            ),
            format!(
                "{}: {}",
                t("Reply restraint", "回复克制"),
                boolean_label(settings.reply_restraint_enable)
            ),
            format!(
                "{}: {}",
                t("Restraint recovery (minutes)", "克制恢复时间（分钟）"),
                settings.reply_restraint_recover_minutes
            ),
            format!(
                "{}: {}",
                t("Restraint strength", "克制强度"),
                real_context_restraint_label(&settings.reply_restraint_strength)
            ),
            format!(
                "{}: {}",
                t("Restraint multiplier", "克制倍率"),
                settings.reply_restraint_multiplier
            ),
            t("Continuation window", "续聊窗口").to_string(),
            t("Trigger methods", "触发方式").to_string(),
            t("Concurrency and weights", "并发与权重").to_string(),
            format!(
                "{}: {}",
                t("Judge context window", "判断上下文消息数"),
                settings.judge_context_window
            ),
        ];
        draw_menu(
            stdout,
            t(" ACTIVE REPLY JUDGEMENT ", " 主动回复判断 "),
            &options,
            selected,
            "",
        )?;
        match read_key()? {
            KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
            KeyCode::Up | KeyCode::Char('k') => selected = selected.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => selected = (selected + 1).min(options.len() - 1),
            KeyCode::Enter => match selected {
                0 => {
                    settings.active_reply_enable = select_bool(
                        stdout,
                        t("Scoring and restraint", "评分与克制"),
                        settings.active_reply_enable,
                    )?
                }
                1 => {
                    settings.judge_include_persona = select_bool(
                        stdout,
                        t("Inherit persona during judgement", "判断时继承人格"),
                        settings.judge_include_persona,
                    )?
                }
                2 => edit_textarea(stdout, &mut settings.judge_persona_prompt)?,
                3 => edit_real_context_number(
                    stdout,
                    t("Random judgement probability", "随机进入判断的概率"),
                    settings.active_judge_probability,
                    settings,
                    |candidate, value| candidate.active_judge_probability = value,
                )?,
                4 => edit_real_context_number(
                    stdout,
                    t("Reply threshold", "回复阈值"),
                    settings.reply_threshold,
                    settings,
                    |candidate, value| candidate.reply_threshold = value,
                )?,
                5 => {
                    settings.skip_pure_image_active_judge = select_bool(
                        stdout,
                        t("Skip image-only messages", "跳过纯图片消息"),
                        settings.skip_pure_image_active_judge,
                    )?
                }
                6 => {
                    edit_active_judgement_skip_ids(stdout, state)?;
                }
                7 => {
                    settings.active_reply_supersede_enable = select_bool(
                        stdout,
                        t(
                            "New message supersedes pending judgement",
                            "新消息覆盖待判断消息",
                        ),
                        settings.active_reply_supersede_enable,
                    )?
                }
                8 => edit_real_context_number(
                    stdout,
                    t("Supersede window (seconds)", "覆盖窗口（秒）"),
                    settings.active_reply_supersede_window_seconds,
                    settings,
                    |candidate, value| candidate.active_reply_supersede_window_seconds = value,
                )?,
                9 => {
                    settings.reply_restraint_enable = select_bool(
                        stdout,
                        t("Reply restraint", "回复克制"),
                        settings.reply_restraint_enable,
                    )?
                }
                10 => edit_real_context_number(
                    stdout,
                    t("Restraint recovery (minutes)", "克制恢复时间（分钟）"),
                    settings.reply_restraint_recover_minutes,
                    settings,
                    |candidate, value| candidate.reply_restraint_recover_minutes = value,
                )?,
                11 => edit_real_context_restraint_strength(stdout, settings)?,
                12 => edit_real_context_number(
                    stdout,
                    t("Restraint multiplier", "克制倍率"),
                    settings.reply_restraint_multiplier,
                    settings,
                    |candidate, value| candidate.reply_restraint_multiplier = value,
                )?,
                13 => edit_real_context_continuation(stdout, settings)?,
                14 => edit_real_context_triggers(stdout, settings)?,
                15 => edit_real_context_judge_advanced(stdout, settings)?,
                16 => edit_real_context_number(
                    stdout,
                    t("Judge context window", "判断上下文消息数"),
                    settings.judge_context_window,
                    settings,
                    |candidate, value| candidate.judge_context_window = value,
                )?,
                _ => {}
            },
            _ => {}
        }
    }
}

fn edit_active_judgement_skip_ids(stdout: &mut io::Stdout, state: &StateStore) -> Result<()> {
    let original = match active_judgement_skip_ids(state) {
        Ok(ids) => ids,
        Err(error) => {
            message(
                stdout,
                &format!(
                    "{}: {error}",
                    t(
                        "Unable to read the active judgement skip list",
                        "无法读取主动判断跳过名单"
                    )
                ),
            )?;
            return Ok(());
        }
    };
    let mut edited = original.clone();
    edit_qq_id_list(
        stdout,
        t(" ACTIVE JUDGEMENT SKIP QQ IDS ", " 跳过主动判断的 QQ 号 "),
        t("QQ id", "QQ 号"),
        &mut edited,
    )?;
    if let Err(error) = apply_active_judgement_skip_editor_changes(state, &original, &edited) {
        message(
            stdout,
            &format!(
                "{}: {error}",
                t(
                    "Unable to update the active judgement skip list",
                    "无法更新主动判断跳过名单"
                )
            ),
        )?;
    }
    Ok(())
}

fn edit_real_context_restraint_strength(
    stdout: &mut io::Stdout,
    settings: &mut RealContextPluginSettings,
) -> Result<()> {
    loop {
        let mut fields = vec![Field::new(
            t("Restraint strength", "克制强度"),
            real_context_restraint_label(&settings.reply_restraint_strength).to_string(),
        )
        .choices(&[t("Light", "轻度"), t("Medium", "中度"), t("Strong", "强烈")])];
        if !run_form(stdout, t(" RESTRAINT STRENGTH ", " 克制强度 "), &mut fields)? {
            return Ok(());
        }
        let mut candidate = settings.clone();
        let parsed = (|| -> std::result::Result<(), String> {
            candidate.reply_restraint_strength = real_context_restraint_value(&fields[0].value)
                .ok_or_else(|| t("Invalid restraint strength.", "克制强度无效。").to_string())?
                .to_string();
            candidate.validate().map_err(|error| error.to_string())
        })();
        match parsed {
            Ok(()) => {
                *settings = candidate;
                return Ok(());
            }
            Err(error) => message(stdout, &error)?,
        }
    }
}

fn edit_real_context_judge_advanced(
    stdout: &mut io::Stdout,
    settings: &mut RealContextPluginSettings,
) -> Result<()> {
    loop {
        let mut fields = vec![
            Field::new(
                t("Timeout (seconds)", "判断超时（秒）"),
                settings.judge_timeout_seconds.to_string(),
            ),
            Field::new(
                t("Endpoint timeout (seconds)", "单模型超时（秒）"),
                settings.judge_endpoint_timeout_seconds.to_string(),
            ),
            Field::new(
                t(
                    "Global concurrency wait timeout (seconds)",
                    "全局判断并发等待超时（秒）",
                ),
                settings.judge_queue_wait_timeout_seconds.to_string(),
            ),
            Field::new(
                t("Maximum concurrency", "最大并发数"),
                settings.judge_max_concurrency.to_string(),
            ),
            Field::new(
                t("Maximum retries", "最大重试次数"),
                settings.judge_max_retries.to_string(),
            ),
            Field::new(
                t("Relevance weight", "相关性权重"),
                settings.judge_relevance_weight.to_string(),
            ),
            Field::new(
                t("Willingness weight", "意愿权重"),
                settings.judge_willingness_weight.to_string(),
            ),
            Field::new(
                t("Social weight", "社交适合度权重"),
                settings.judge_social_weight.to_string(),
            ),
            Field::new(
                t("Timing weight", "时机权重"),
                settings.judge_timing_weight.to_string(),
            ),
            Field::new(
                t("Continuity weight", "连续性权重"),
                settings.judge_continuity_weight.to_string(),
            ),
            Field::boolean(
                t("Use judgement recommendation", "采用判断建议加减分"),
                settings.judge_should_reply_adjust_enable,
            ),
            Field::new(
                t("Recommended-reply boost", "建议回复加分"),
                settings.judge_should_reply_boost_score.to_string(),
            ),
            Field::new(
                t("Recommended-silence penalty", "建议不回复减分"),
                settings.judge_should_reply_penalty_score.to_string(),
            ),
        ];
        if !run_form(
            stdout,
            t(" JUDGEMENT ADVANCED ", " 主动判断高级设置 "),
            &mut fields,
        )? {
            return Ok(());
        }
        let mut candidate = settings.clone();
        let parsed = (|| -> std::result::Result<(), String> {
            candidate.judge_timeout_seconds = real_context_value(&fields, 0)?;
            candidate.judge_endpoint_timeout_seconds = real_context_value(&fields, 1)?;
            candidate.judge_queue_wait_timeout_seconds = real_context_value(&fields, 2)?;
            candidate.judge_max_concurrency = real_context_value(&fields, 3)?;
            candidate.judge_max_retries = real_context_value(&fields, 4)?;
            candidate.judge_relevance_weight = real_context_value(&fields, 5)?;
            candidate.judge_willingness_weight = real_context_value(&fields, 6)?;
            candidate.judge_social_weight = real_context_value(&fields, 7)?;
            candidate.judge_timing_weight = real_context_value(&fields, 8)?;
            candidate.judge_continuity_weight = real_context_value(&fields, 9)?;
            candidate.judge_should_reply_adjust_enable = real_context_bool(&fields, 10)?;
            candidate.judge_should_reply_boost_score = real_context_value(&fields, 11)?;
            candidate.judge_should_reply_penalty_score = real_context_value(&fields, 12)?;
            candidate.validate().map_err(|error| error.to_string())
        })();
        match parsed {
            Ok(()) => {
                *settings = candidate;
                return Ok(());
            }
            Err(error) => message(stdout, &error)?,
        }
    }
}

fn edit_real_context_triggers(
    stdout: &mut io::Stdout,
    settings: &mut RealContextPluginSettings,
) -> Result<()> {
    loop {
        let mut fields = vec![
            Field::boolean(
                t("Take over direct triggers", "接管直接触发"),
                settings.takeover_direct_trigger_enable,
            ),
            Field::new(
                t("Direct-trigger boost", "直接触发加分"),
                settings.takeover_direct_trigger_boost_score.to_string(),
            ),
            Field::boolean(
                t(
                    "Privileged users skip group active judgement",
                    "管理员和私聊白名单跳过群聊主动回复判断",
                ),
                settings.privileged_direct_trigger_skip_active_judgement,
            ),
        ];
        if !run_form(stdout, t(" TRIGGER METHODS ", " 触发方式 "), &mut fields)? {
            return Ok(());
        }
        let mut candidate = settings.clone();
        let parsed = (|| -> std::result::Result<(), String> {
            candidate.takeover_direct_trigger_enable = real_context_bool(&fields, 0)?;
            candidate.takeover_direct_trigger_boost_score = real_context_value(&fields, 1)?;
            candidate.privileged_direct_trigger_skip_active_judgement =
                real_context_bool(&fields, 2)?;
            candidate.validate().map_err(|error| error.to_string())
        })();
        match parsed {
            Ok(()) => {
                *settings = candidate;
                return Ok(());
            }
            Err(error) => message(stdout, &error)?,
        }
    }
}

fn edit_real_context_continuation(
    stdout: &mut io::Stdout,
    settings: &mut RealContextPluginSettings,
) -> Result<()> {
    loop {
        let mut fields = vec![
            Field::boolean(
                t("Natural continuation", "自然续聊"),
                settings.continuation_enable,
            ),
            Field::new(
                t("Continuation window (seconds)", "续聊窗口（秒）"),
                settings.continuation_window_seconds.to_string(),
            ),
            Field::new(
                t("Continuation boost", "续聊加分"),
                settings.continuation_boost_score.to_string(),
            ),
        ];
        if !run_form(
            stdout,
            t(" CONTINUATION WINDOW ", " 续聊窗口 "),
            &mut fields,
        )? {
            return Ok(());
        }
        let mut candidate = settings.clone();
        let parsed = (|| -> std::result::Result<(), String> {
            candidate.continuation_enable = real_context_bool(&fields, 0)?;
            candidate.continuation_window_seconds = real_context_value(&fields, 1)?;
            candidate.continuation_boost_score = real_context_value(&fields, 2)?;
            candidate.validate().map_err(|error| error.to_string())
        })();
        match parsed {
            Ok(()) => {
                *settings = candidate;
                return Ok(());
            }
            Err(error) => message(stdout, &error)?,
        }
    }
}

fn edit_real_context_reply_target(
    stdout: &mut io::Stdout,
    settings: &mut RealContextPluginSettings,
) -> Result<()> {
    let mut selected = 0usize;
    loop {
        let options = vec![
            format!(
                "{}: {}",
                t("Target the replied-to user", "定向回复对象"),
                boolean_label(settings.reply_target_enable)
            ),
            format!(
                "{}: {}",
                t("Quote target message", "引用目标消息"),
                boolean_label(settings.reply_target_quote_enable)
            ),
            format!(
                "{}: {}",
                t(
                    "Quote after intervening messages from others",
                    "和原消息间隔几条消息则引用"
                ),
                settings.reply_target_quote_after_other_messages
            ),
            format!(
                "{}: {}",
                t("Mention target user", "艾特目标用户"),
                boolean_label(settings.reply_target_mention_enable)
            ),
            format!(
                "{}: {}",
                t("Mention after elapsed seconds", "回复时间超过多少秒则艾特"),
                settings.reply_target_mention_after_seconds
            ),
            format!(
                "{}: {}",
                t(
                    "React after an active reply is accepted",
                    "确认主动回复后贴表情"
                ),
                boolean_label(settings.active_reply_reaction_enable)
            ),
            format!(
                "{}: {}",
                t("Active-reply reaction id", "主动回复贴的表情ID"),
                settings
                    .active_reply_reaction_emoji_ids
                    .first()
                    .copied()
                    .unwrap_or_default()
            ),
            format!(
                "{}: {}",
                t("Reaction cleanup timeout (seconds)", "表情清理超时（秒）"),
                settings.active_reply_reaction_timeout_seconds
            ),
        ];
        draw_menu(
            stdout,
            t(" QUOTE, MENTION, AND REACTIONS ", " 引用艾特和贴表情 "),
            &options,
            selected,
            "",
        )?;
        match read_key()? {
            KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
            KeyCode::Up | KeyCode::Char('k') => selected = selected.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => selected = (selected + 1).min(options.len() - 1),
            KeyCode::Enter => match selected {
                0 => {
                    settings.reply_target_enable = select_bool(
                        stdout,
                        t("Target the replied-to user", "定向回复对象"),
                        settings.reply_target_enable,
                    )?
                }
                1 => {
                    settings.reply_target_quote_enable = select_bool(
                        stdout,
                        t("Quote target message", "引用目标消息"),
                        settings.reply_target_quote_enable,
                    )?
                }
                2 => edit_real_context_number(
                    stdout,
                    t(
                        "Quote after intervening messages from others",
                        "和原消息间隔几条消息则引用",
                    ),
                    settings.reply_target_quote_after_other_messages,
                    settings,
                    |candidate, value| candidate.reply_target_quote_after_other_messages = value,
                )?,
                3 => {
                    settings.reply_target_mention_enable = select_bool(
                        stdout,
                        t("Mention target user", "艾特目标用户"),
                        settings.reply_target_mention_enable,
                    )?
                }
                4 => edit_real_context_number(
                    stdout,
                    t("Mention after elapsed seconds", "回复时间超过多少秒则艾特"),
                    settings.reply_target_mention_after_seconds,
                    settings,
                    |candidate, value| candidate.reply_target_mention_after_seconds = value,
                )?,
                5 => {
                    settings.active_reply_reaction_enable = select_bool(
                        stdout,
                        t(
                            "React after an active reply is accepted",
                            "确认主动回复后贴表情",
                        ),
                        settings.active_reply_reaction_enable,
                    )?
                }
                6 => {
                    let current = settings
                        .active_reply_reaction_emoji_ids
                        .first()
                        .copied()
                        .unwrap_or_default();
                    edit_real_context_number(
                        stdout,
                        t("Active-reply reaction id", "主动回复贴的表情ID"),
                        current,
                        settings,
                        |candidate, value| candidate.active_reply_reaction_emoji_ids = vec![value],
                    )?;
                }
                7 => edit_real_context_number(
                    stdout,
                    t("Reaction cleanup timeout (seconds)", "表情清理超时（秒）"),
                    settings.active_reply_reaction_timeout_seconds,
                    settings,
                    |candidate, value| candidate.active_reply_reaction_timeout_seconds = value,
                )?,
                _ => {}
            },
            _ => {}
        }
    }
}

fn edit_real_context_moderation(
    stdout: &mut io::Stdout,
    settings: &mut RealContextPluginSettings,
) -> Result<()> {
    let mut selected = 0usize;
    loop {
        let options = vec![
            format!(
                "{}: {}",
                t("Moderation", "违规判断"),
                boolean_label(settings.moderation_enable)
            ),
            format!(
                "{}: {}",
                t("Keyword precheck", "关键词触发初判"),
                boolean_label(settings.moderation_keyword_trigger_enable)
            ),
            format!(
                "{}: {}",
                t("Moderation keywords", "违规初判关键词"),
                settings.moderation_keywords.len()
            ),
            format!(
                "{}: {}",
                t("Moderation rules prompt", "违规规则提示词"),
                if settings.moderation_custom_rules.is_empty() {
                    t("none", "未设置")
                } else {
                    t("set", "已设置")
                }
            ),
            format!(
                "{}: {}",
                t("Minimum severity", "判断违规的阈值"),
                settings.moderation_min_severity
            ),
            format!(
                "{}: {}",
                t("Moderation timeout (seconds)", "违规判断超时"),
                settings.moderation_timeout_seconds
            ),
            format!(
                "{}: {}",
                t("Decode Base64 text", "Base64 违规初判"),
                boolean_label(settings.base64_moderation_enable)
            ),
            format!(
                "{}: {}",
                t("Minimum Base64 length", "Base64 最短长度"),
                settings.base64_moderation_min_chars
            ),
            format!(
                "{}: {}",
                t("Maximum decoded characters", "Base64 最大解码字符数"),
                settings.base64_moderation_max_decoded_chars
            ),
            format!(
                "{}: {}",
                t("Minimum printable ratio", "Base64 最低可打印比例"),
                settings.base64_moderation_min_printable_ratio
            ),
        ];
        draw_menu(
            stdout,
            t(" SAFETY CHECKS ", " 违规判断 "),
            &options,
            selected,
            "",
        )?;
        match read_key()? {
            KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
            KeyCode::Up | KeyCode::Char('k') => selected = selected.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => selected = (selected + 1).min(options.len() - 1),
            KeyCode::Enter if selected == 0 => {
                settings.moderation_enable = select_bool(
                    stdout,
                    t("Moderation", "违规判断"),
                    settings.moderation_enable,
                )?
            }
            KeyCode::Enter if selected == 1 => {
                settings.moderation_keyword_trigger_enable = select_bool(
                    stdout,
                    t("Keyword precheck", "关键词触发初判"),
                    settings.moderation_keyword_trigger_enable,
                )?
            }
            KeyCode::Enter if selected == 2 => edit_real_context_string_lines(
                stdout,
                t(" MODERATION KEYWORDS ", " 违规初判关键词 "),
                &mut settings.moderation_keywords,
                256,
            )?,
            KeyCode::Enter if selected == 3 => {
                edit_textarea(stdout, &mut settings.moderation_custom_rules)?
            }
            KeyCode::Enter if selected == 4 => edit_real_context_number(
                stdout,
                t("Minimum severity", "判断违规的阈值"),
                settings.moderation_min_severity,
                settings,
                |candidate, value| candidate.moderation_min_severity = value,
            )?,
            KeyCode::Enter if selected == 5 => edit_real_context_number(
                stdout,
                t("Moderation timeout (seconds)", "违规判断超时"),
                settings.moderation_timeout_seconds,
                settings,
                |candidate, value| candidate.moderation_timeout_seconds = value,
            )?,
            KeyCode::Enter if selected == 6 => {
                settings.base64_moderation_enable = select_bool(
                    stdout,
                    t("Decode Base64 text", "Base64 违规初判"),
                    settings.base64_moderation_enable,
                )?
            }
            KeyCode::Enter if selected == 7 => edit_real_context_number(
                stdout,
                t("Minimum Base64 length", "Base64 最短长度"),
                settings.base64_moderation_min_chars,
                settings,
                |candidate, value| candidate.base64_moderation_min_chars = value,
            )?,
            KeyCode::Enter if selected == 8 => edit_real_context_number(
                stdout,
                t("Maximum decoded characters", "Base64 最大解码字符数"),
                settings.base64_moderation_max_decoded_chars,
                settings,
                |candidate, value| candidate.base64_moderation_max_decoded_chars = value,
            )?,
            KeyCode::Enter if selected == 9 => edit_real_context_number(
                stdout,
                t("Minimum printable ratio", "Base64 最低可打印比例"),
                settings.base64_moderation_min_printable_ratio,
                settings,
                |candidate, value| candidate.base64_moderation_min_printable_ratio = value,
            )?,
            _ => {}
        }
    }
}

fn edit_real_context_affection(
    stdout: &mut io::Stdout,
    _config: &AppConfig,
    settings: &mut RealContextPluginSettings,
) -> Result<()> {
    let mut selected = 0usize;
    loop {
        let options = [
            format!(
                "{}: {}",
                t("Affection system", "好感度系统"),
                boolean_label(settings.affection_enable)
            ),
            format!(
                "{}: {}",
                t(
                    "Judge affection changes after replies",
                    "回复后判断好感度变化",
                ),
                boolean_label(settings.affection_update_enable)
            ),
            t("Score and limits", "分值与限制").to_string(),
            t("Relationship prompts", "关系提示词").to_string(),
            format!(
                "{}: {}",
                t("Top-tier QQ IDs", "允许到达最高挡位的 QQ 号"),
                settings.affection_unlimited_user_ids.len()
            ),
        ];
        draw_menu(
            stdout,
            t(" AFFECTION AND RELATIONSHIP ", " 好感度与关系 "),
            &options,
            selected,
            "",
        )?;
        match read_key()? {
            KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
            KeyCode::Up | KeyCode::Char('k') => selected = selected.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => selected = (selected + 1).min(options.len() - 1),
            KeyCode::Enter => match selected {
                0 => {
                    settings.affection_enable = select_bool(
                        stdout,
                        t("Affection system", "好感度系统"),
                        settings.affection_enable,
                    )?;
                }
                1 => {
                    settings.affection_update_enable = select_bool(
                        stdout,
                        t(
                            "Judge affection changes after replies",
                            "回复后判断好感度变化",
                        ),
                        settings.affection_update_enable,
                    )?;
                }
                2 => edit_real_context_affection_values(stdout, settings)?,
                3 => edit_real_context_affection_prompts(stdout, settings)?,
                4 => {
                    let mut raw = settings
                        .affection_unlimited_user_ids
                        .iter()
                        .map(i64::to_string)
                        .collect::<Vec<_>>()
                        .join("\n");
                    edit_textarea(stdout, &mut raw)?;
                    match parse_id_list(&raw) {
                        Ok(ids) => settings.affection_unlimited_user_ids = ids,
                        Err(error) => message(stdout, &error.to_string())?,
                    }
                }
                _ => {}
            },
            _ => {}
        }
    }
}

fn edit_real_context_affection_values(
    stdout: &mut io::Stdout,
    settings: &mut RealContextPluginSettings,
) -> Result<()> {
    loop {
        let mut fields = vec![
            Field::new(
                t("Initial score", "首次互动默认好感度"),
                settings.affection_initial_score.to_string(),
            ),
            Field::new(
                t("Minimum score", "好感度下限"),
                settings.affection_min_score.to_string(),
            ),
            Field::new(
                t("Global maximum score", "全局最高好感度"),
                settings.affection_max_score.to_string(),
            ),
            Field::new(
                t("Regular-user maximum", "普通用户最高好感度"),
                settings.affection_regular_max_score.to_string(),
            ),
            Field::new(
                t("Reply bias minimum", "主动回复最低加值"),
                settings.affection_bias_min.to_string(),
            ),
            Field::new(
                t("Reply bias maximum", "主动回复最高加值"),
                settings.affection_bias_max.to_string(),
            ),
            Field::new(
                t("Gain pivot", "好感增益拐点"),
                settings.affection_gain_pivot.to_string(),
            ),
            Field::new(
                t("Delta scale", "好感变化倍率"),
                settings.affection_delta_scale.to_string(),
            ),
            Field::new(
                t("Single-change minimum", "单次变化下限"),
                settings.affection_delta_min.to_string(),
            ),
            Field::new(
                t("Single-change maximum", "单次变化上限"),
                settings.affection_delta_max.to_string(),
            ),
            Field::new(
                t("Confidence threshold", "变化置信度阈值"),
                settings.affection_update_confidence_threshold.to_string(),
            ),
            Field::new(
                t(
                    "Daily gain limit (0 = unlimited)",
                    "单日正向上限（0 = 不限）",
                ),
                settings.affection_daily_gain_limit.to_string(),
            ),
            Field::new(
                t(
                    "Daily loss limit (0 = unlimited)",
                    "单日负向上限（0 = 不限）",
                ),
                settings.affection_daily_loss_limit.to_string(),
            ),
            Field::boolean(
                t("Automatic tags", "自动标签"),
                settings.affection_auto_tag_enable,
            ),
            Field::new(
                t("Maximum tags (0 = unlimited)", "标签上限（0 = 不限）"),
                settings.affection_max_tags.to_string(),
            ),
            Field::new(
                t("Recent events in prompt", "注入提示词的近期变化条数"),
                settings.affection_recent_events_for_prompt.to_string(),
            ),
            Field::new(
                t(
                    "Update timeout (seconds; 0 = unlimited)",
                    "更新超时（秒；0 = 不限）",
                ),
                settings.affection_update_timeout_seconds.to_string(),
            ),
        ];
        if !run_form(
            stdout,
            t(" AFFECTION SCORE AND LIMITS ", " 好感度分值与限制 "),
            &mut fields,
        )? {
            return Ok(());
        }
        let mut candidate = settings.clone();
        let parsed = (|| -> std::result::Result<(), String> {
            candidate.affection_initial_score = real_context_value(&fields, 0)?;
            candidate.affection_min_score = real_context_value(&fields, 1)?;
            candidate.affection_max_score = real_context_value(&fields, 2)?;
            candidate.affection_regular_max_score = real_context_value(&fields, 3)?;
            candidate.affection_bias_min = real_context_value(&fields, 4)?;
            candidate.affection_bias_max = real_context_value(&fields, 5)?;
            candidate.affection_gain_pivot = real_context_value(&fields, 6)?;
            candidate.affection_delta_scale = real_context_value(&fields, 7)?;
            candidate.affection_delta_min = real_context_value(&fields, 8)?;
            candidate.affection_delta_max = real_context_value(&fields, 9)?;
            candidate.affection_update_confidence_threshold = real_context_value(&fields, 10)?;
            candidate.affection_daily_gain_limit = real_context_value(&fields, 11)?;
            candidate.affection_daily_loss_limit = real_context_value(&fields, 12)?;
            candidate.affection_auto_tag_enable = real_context_bool(&fields, 13)?;
            candidate.affection_max_tags = real_context_value(&fields, 14)?;
            candidate.affection_recent_events_for_prompt = real_context_value(&fields, 15)?;
            candidate.affection_update_timeout_seconds = real_context_value(&fields, 16)?;
            candidate.validate().map_err(|error| error.to_string())
        })();
        match parsed {
            Ok(()) => {
                *settings = candidate;
                return Ok(());
            }
            Err(error) => message(stdout, &error)?,
        }
    }
}

fn edit_real_context_affection_prompts(
    stdout: &mut io::Stdout,
    settings: &mut RealContextPluginSettings,
) -> Result<()> {
    let prompts = [
        (
            t("Estranged", "刻意疏远"),
            &mut settings.affection_prompt_estranged,
        ),
        (t("Cold", "冷漠"), &mut settings.affection_prompt_cold),
        (t("Neutral", "中立"), &mut settings.affection_prompt_neutral),
        (t("Known", "认识"), &mut settings.affection_prompt_known),
        (t("Friend", "好友"), &mut settings.affection_prompt_friend),
        (t("Trusted", "信任"), &mut settings.affection_prompt_trusted),
        (t("Close", "亲近"), &mut settings.affection_prompt_close),
    ];
    let mut selected = 0usize;
    loop {
        let options = prompts
            .iter()
            .map(|(label, value)| {
                format!(
                    "{label}: {}",
                    if value.is_empty() {
                        t("unset", "未设置")
                    } else {
                        t("set", "已设置")
                    }
                )
            })
            .collect::<Vec<_>>();
        draw_menu(
            stdout,
            t(" AFFECTION RELATIONSHIP PROMPTS ", " 好感度关系提示词 "),
            &options,
            selected,
            "",
        )?;
        match read_key()? {
            KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
            KeyCode::Up | KeyCode::Char('k') => selected = selected.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => selected = (selected + 1).min(options.len() - 1),
            KeyCode::Enter => edit_textarea(stdout, prompts[selected].1)?,
            _ => {}
        }
    }
}

fn edit_real_context_identities(
    stdout: &mut io::Stdout,
    settings: &mut RealContextPluginSettings,
) -> Result<()> {
    let mut selected = 0usize;
    loop {
        let mut options = vec![
            t("+ Add one", "+ 新增一项").to_string(),
            t("+ Add multiple", "+ 批量新增").to_string(),
        ];
        options.extend(
            settings
                .identity_mappings
                .iter()
                .map(|mapping| format!("{} -> {}", mapping.nickname, mapping.user_id)),
        );
        selected = selected.min(options.len() - 1);
        draw_menu(
            stdout,
            t(" IDENTITY MAPPINGS ", " 识人映射 "),
            &options,
            selected,
            t(
                "[Enter]configure [Delete]remove [j/k]move [q]back",
                "[Enter]配置 [Delete]删除 [j/k]移动 [q]返回",
            ),
        )?;
        match read_key()? {
            KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
            KeyCode::Up | KeyCode::Char('k') => selected = selected.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => selected = (selected + 1).min(options.len() - 1),
            KeyCode::Enter if selected == 0 => {
                if let Some(mapping) = prompt_real_context_identity(stdout, None)? {
                    upsert_real_context_identity(&mut settings.identity_mappings, mapping);
                }
            }
            KeyCode::Enter if selected == 1 => {
                let mut raw = format!(
                    "# {}",
                    t(
                        "one per line: nickname<Tab>QQ-id",
                        "每行一项：昵称<Tab>QQ号"
                    )
                );
                edit_textarea(stdout, &mut raw)?;
                match parse_real_context_identity_lines(&raw) {
                    Ok(mappings) => {
                        for mapping in mappings {
                            upsert_real_context_identity(&mut settings.identity_mappings, mapping);
                        }
                    }
                    Err(error) => message(stdout, &error)?,
                }
            }
            KeyCode::Enter => {
                let index = selected - 2;
                if let Some(mapping) = prompt_real_context_identity(
                    stdout,
                    settings.identity_mappings.get(index).cloned(),
                )? {
                    if settings
                        .identity_mappings
                        .iter()
                        .enumerate()
                        .any(|(other, item)| other != index && item.nickname == mapping.nickname)
                    {
                        message(stdout, t("That nickname already exists.", "该昵称已存在。"))?;
                    } else if let Some(item) = settings.identity_mappings.get_mut(index) {
                        *item = mapping;
                    }
                }
            }
            KeyCode::Delete | KeyCode::Backspace if selected >= 2 => {
                settings.identity_mappings.remove(selected - 2);
                selected = selected.min(settings.identity_mappings.len() + 1);
            }
            _ => {}
        }
    }
}

fn prompt_real_context_identity(
    stdout: &mut io::Stdout,
    current: Option<RealContextIdentityMapping>,
) -> Result<Option<RealContextIdentityMapping>> {
    let mut fields = vec![
        Field::new(
            t("Protected nickname", "受保护昵称"),
            current
                .as_ref()
                .map(|mapping| mapping.nickname.clone())
                .unwrap_or_default(),
        ),
        Field::new(
            t("Expected QQ id", "对应 QQ 号"),
            current
                .as_ref()
                .map(|mapping| mapping.user_id.to_string())
                .unwrap_or_default(),
        ),
    ];
    if !run_form(
        stdout,
        t(" IDENTITY MAPPING ", " 编辑识人映射 "),
        &mut fields,
    )? {
        return Ok(None);
    }
    let nickname = fields[0].value.trim();
    if nickname.is_empty()
        || nickname.chars().count() > 128
        || nickname.chars().any(char::is_control)
    {
        message(
            stdout,
            t(
                "Nickname must be 1-128 characters without control characters.",
                "昵称必须为 1 到 128 个字符，且不能包含控制字符。",
            ),
        )?;
        return Ok(None);
    }
    let user_id = match parse_positive_id(&fields[1].value) {
        Ok(user_id) => user_id,
        Err(error) => {
            message(stdout, &error)?;
            return Ok(None);
        }
    };
    Ok(Some(RealContextIdentityMapping {
        nickname: nickname.to_string(),
        user_id,
    }))
}

fn upsert_real_context_identity(
    mappings: &mut Vec<RealContextIdentityMapping>,
    mapping: RealContextIdentityMapping,
) {
    if let Some(existing) = mappings
        .iter_mut()
        .find(|existing| existing.nickname == mapping.nickname)
    {
        *existing = mapping;
    } else {
        mappings.push(mapping);
    }
}

fn parse_real_context_identity_lines(
    raw: &str,
) -> std::result::Result<Vec<RealContextIdentityMapping>, String> {
    let mut mappings = Vec::new();
    for (index, line) in raw.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((nickname, user_id)) = line.rsplit_once('\t').or_else(|| line.rsplit_once('='))
        else {
            return Err(format!(
                "{} {}: {}",
                t("Line", "第"),
                index + 1,
                t("use nickname<Tab>QQ-id", "请使用 昵称<Tab>QQ号 格式")
            ));
        };
        let nickname = nickname.trim();
        if nickname.is_empty()
            || nickname.chars().count() > 128
            || nickname.chars().any(char::is_control)
        {
            return Err(format!(
                "{} {}: {}",
                t("Line", "第"),
                index + 1,
                t("invalid nickname", "昵称无效")
            ));
        }
        let user_id = parse_positive_id(user_id)?;
        if mappings
            .iter()
            .any(|mapping: &RealContextIdentityMapping| mapping.nickname == nickname)
        {
            return Err(format!(
                "{} {}: {}",
                t("Line", "第"),
                index + 1,
                t("duplicate nickname", "昵称重复")
            ));
        }
        mappings.push(RealContextIdentityMapping {
            nickname: nickname.to_string(),
            user_id,
        });
    }
    Ok(mappings)
}

fn edit_real_context_string_lines(
    stdout: &mut io::Stdout,
    _title: &'static str,
    values: &mut Vec<String>,
    maximum_chars: usize,
) -> Result<()> {
    let mut raw = values.join("\n");
    edit_textarea(stdout, &mut raw)?;
    match parse_real_context_string_lines(&raw, maximum_chars) {
        Ok(parsed) => *values = parsed,
        Err(error) => message(stdout, &error)?,
    }
    Ok(())
}

fn parse_real_context_string_lines(
    raw: &str,
    maximum_chars: usize,
) -> std::result::Result<Vec<String>, String> {
    let mut values = Vec::new();
    for (index, line) in raw.lines().enumerate() {
        let value = line.trim();
        if value.is_empty() {
            continue;
        }
        if value.chars().count() > maximum_chars || value.chars().any(char::is_control) {
            return Err(format!(
                "{} {}: {}",
                t("Line", "第"),
                index + 1,
                t("value is invalid or too long", "内容无效或过长")
            ));
        }
        if !values.iter().any(|existing| existing == value) {
            values.push(value.to_string());
        }
    }
    Ok(values)
}

fn real_context_bool(fields: &[Field], index: usize) -> std::result::Result<bool, String> {
    parse_bool_field(&fields[index].value).map_err(|error| error.to_string())
}

fn real_context_value<T>(fields: &[Field], index: usize) -> std::result::Result<T, String>
where
    T: std::str::FromStr,
{
    fields[index]
        .value
        .trim()
        .parse()
        .map_err(|_| t("Invalid value.", "数值无效。").to_string())
}

fn edit_real_context_number<T>(
    stdout: &mut io::Stdout,
    label: &'static str,
    current: T,
    settings: &mut RealContextPluginSettings,
    assign: impl Fn(&mut RealContextPluginSettings, T),
) -> Result<()>
where
    T: Copy + ToString + std::str::FromStr,
{
    loop {
        let Some(raw) = edit_inline_value(stdout, label, &current.to_string(), false)? else {
            return Ok(());
        };
        let value = match raw.trim().parse() {
            Ok(value) => value,
            Err(_) => {
                message(stdout, t("Invalid value.", "数值无效。"))?;
                continue;
            }
        };
        let mut candidate = settings.clone();
        assign(&mut candidate, value);
        match candidate.validate() {
            Ok(()) => {
                *settings = candidate;
                return Ok(());
            }
            Err(error) => message(stdout, &error.to_string())?,
        }
    }
}

fn real_context_media_mode_label(value: &str) -> &'static str {
    match value {
        "off" => t("Off", "不记录"),
        "metadata" => t("Metadata", "保留元数据"),
        _ => t("Placeholder", "仅占位"),
    }
}

fn real_context_media_mode_value(value: &str) -> Option<&'static str> {
    match value.trim() {
        "off" | "Off" | "不记录" => Some("off"),
        "placeholder" | "Placeholder" | "仅占位" => Some("placeholder"),
        "metadata" | "Metadata" | "保留元数据" => Some("metadata"),
        _ => None,
    }
}

fn real_context_restraint_label(value: &str) -> &'static str {
    match value {
        "light" => t("Light", "轻度"),
        "strong" => t("Strong", "强烈"),
        _ => t("Medium", "中度"),
    }
}

fn real_context_restraint_value(value: &str) -> Option<&'static str> {
    match value.trim() {
        "light" | "Light" | "轻度" => Some("light"),
        "medium" | "Medium" | "中度" => Some("medium"),
        "strong" | "Strong" | "强烈" => Some("strong"),
        _ => None,
    }
}

fn real_context_model_pool_summary(pool: Option<&[ActiveProviderModelConfig]>) -> String {
    match pool {
        None | Some([]) => t("inherit platform", "继承平台池").to_string(),
        Some(entries) => route_pool_summary(Some(entries), PlatformModelPoolInheritance::Platform),
    }
}

fn select_real_context_model_pool(
    stdout: &mut io::Stdout,
    config: &AppConfig,
    pool: &mut Option<Vec<ActiveProviderModelConfig>>,
) -> Result<()> {
    select_model_pool(
        stdout,
        config.text_provider_model_choices(),
        pool,
        false,
        t(" REAL-CONTEXT TEXT MODELS ", " 真实上下文文本模型 "),
        t("Inherit QQ platform model pool", "继承 QQ 平台模型池"),
    )
}

fn reply_processor_values(config: &AppConfig) -> Result<(bool, ReplyProcessorSettingsForm)> {
    let Some(instance) = config.platforms.qq.plugins.get(REPLY_PROCESSOR_PLUGIN_ID) else {
        return Ok((true, ReplyProcessorSettingsForm::default()));
    };
    let settings = serde_json::from_value(serde_json::Value::Object(instance.settings.clone()))?;
    Ok((instance.enabled_or(true), settings))
}

fn apply_reply_processor_values(
    config: &mut AppConfig,
    enabled: bool,
    settings: &ReplyProcessorSettingsForm,
) -> Result<()> {
    let serialized = serde_json::to_value(settings)?;
    let serde_json::Value::Object(known_settings) = serialized else {
        bail!("reply processor settings must serialize as an object");
    };
    let instance = config
        .platforms
        .qq
        .plugins
        .entry(REPLY_PROCESSOR_PLUGIN_ID.to_string())
        .or_default();
    instance.enabled = (!enabled).then_some(false);
    for (key, value) in known_settings {
        instance.settings.insert(key, value);
    }
    Ok(())
}

fn edit_reply_processor(stdout: &mut io::Stdout, config: &mut AppConfig) -> Result<()> {
    let (mut plugin_enabled, mut settings) = reply_processor_values(config)?;
    loop {
        let mode_choices = vec![
            reply_processor_mode_label("image"),
            reply_processor_mode_label("forward"),
        ];
        let mut fields = vec![
            Field::boolean(t("Plugin enabled", "启用插件"), plugin_enabled),
            Field::boolean(
                t("Enabled for new conversations", "新会话默认启用"),
                settings.default_enabled,
            ),
            Field::new(
                t("Long reply threshold (characters)", "长回复阈值（字符）"),
                settings.threshold.to_string(),
            ),
            Field::new(
                t("Long reply processing mode", "长回复处理模式"),
                reply_processor_mode_label(&settings.mode),
            )
            .choices_owned(mode_choices)
            .raw_choice_labels(),
            Field::boolean(
                t("Mention sender after forwarding", "转发后艾特发起者"),
                settings.followup_mention,
            ),
            Field::boolean(
                t("Strip trailing Chinese period", "移除末尾中文句号"),
                settings.strip_period,
            ),
            Field::new(t("Image theme", "长图主题"), settings.theme.clone())
                .choices(&["paper", "light", "dark"]),
            Field::new(
                t("Image maximum height", "长图最大高度"),
                settings.max_height.to_string(),
            ),
            Field::new(
                t("Body font size", "正文字号"),
                settings.font_size.to_string(),
            ),
            Field::new(
                t("Code font size", "代码字号"),
                settings.code_font_size.to_string(),
            ),
            Field::new(t("Image padding", "长图边距"), settings.padding.to_string()),
            Field::boolean(
                t("Add image context notice", "注入长图上下文提示"),
                settings.context_notice,
            ),
            Field::new(
                t("Context notice TTL (hours)", "上下文提示保留小时"),
                settings.ttl_hours.to_string(),
            ),
            Field::new(
                t("Maximum context records", "上下文提示最大条数"),
                settings.max_records.to_string(),
            ),
            Field::boolean(
                t("Intercept send-message tool", "接管发送消息工具"),
                settings.send_tool_intercept,
            ),
            Field::new(
                t(
                    "Body font file path (empty = bundled default)",
                    "正文字体文件路径（空 = 内置默认字体）",
                ),
                settings.font.clone(),
            ),
            Field::new(
                t(
                    "Title font file path (empty = body font)",
                    "标题字体文件路径（空 = 跟随正文字体）",
                ),
                settings.title_font.clone(),
            ),
            Field::new(
                t(
                    "Code font file path (empty = bundled default)",
                    "代码字体文件路径（空 = 内置默认字体）",
                ),
                settings.code_font.clone(),
            ),
            Field::new(
                t(
                    "Emoji font file path (empty = bundled default)",
                    "Emoji 字体文件路径（空 = 内置默认字体）",
                ),
                settings.emoji_font.clone(),
            ),
        ];
        run_form_without_buttons(stdout, t(" REPLY PROCESSOR ", " 回复处理 "), &mut fields)?;
        plugin_enabled = parse_bool_field(&fields[0].value)?;
        settings = match parse_reply_processor_fields(&fields) {
            Ok(settings) => settings,
            Err(error) => {
                message(stdout, &error)?;
                continue;
            }
        };
        apply_reply_processor_values(config, plugin_enabled, &settings)?;
        return Ok(());
    }
}

fn parse_reply_processor_fields(
    fields: &[Field],
) -> std::result::Result<ReplyProcessorSettingsForm, String> {
    let bool_at =
        |index: usize| parse_bool_field(&fields[index].value).map_err(|error| error.to_string());
    let mode = reply_processor_mode_value(&fields[3].value)
        .map(str::to_string)
        .unwrap_or_else(|| fields[3].value.trim().to_string());
    let settings = ReplyProcessorSettingsForm {
        default_enabled: bool_at(1)?,
        threshold: parse_reply_processor_value(fields, 2, t("threshold", "阈值"))?,
        mode,
        followup_mention: bool_at(4)?,
        strip_period: bool_at(5)?,
        theme: fields[6].value.trim().to_string(),
        max_height: parse_reply_processor_value(fields, 7, t("maximum height", "最大高度"))?,
        font_size: parse_reply_processor_value(fields, 8, t("font size", "字号"))?,
        code_font_size: parse_reply_processor_value(fields, 9, t("code font size", "代码字号"))?,
        padding: parse_reply_processor_value(fields, 10, t("padding", "边距"))?,
        context_notice: bool_at(11)?,
        ttl_hours: parse_reply_processor_value(fields, 12, "TTL")?,
        max_records: parse_reply_processor_value(fields, 13, t("maximum records", "最大条数"))?,
        send_tool_intercept: bool_at(14)?,
        font: fields[15].value.trim().to_string(),
        title_font: fields[16].value.trim().to_string(),
        code_font: fields[17].value.trim().to_string(),
        emoji_font: fields[18].value.trim().to_string(),
    };
    validate_reply_processor_settings(&settings)?;
    Ok(settings)
}

fn reply_processor_mode_label(value: &str) -> String {
    match value.trim() {
        "image" => t("Convert to image", "转图片"),
        "forward" => t("Merged forward", "合并转发"),
        value => value,
    }
    .to_string()
}

fn reply_processor_mode_value(value: &str) -> Option<&'static str> {
    match value.trim() {
        "image" | "Convert to image" | "转图片" => Some("image"),
        "forward" | "Merged forward" | "合并转发" => Some("forward"),
        _ => None,
    }
}

fn parse_reply_processor_value<T>(
    fields: &[Field],
    index: usize,
    label: &str,
) -> std::result::Result<T, String>
where
    T: std::str::FromStr,
{
    fields[index]
        .value
        .trim()
        .parse()
        .map_err(|_| format!("{}: {label}", t("Invalid value", "无效值")))
}

fn validate_reply_processor_settings(
    settings: &ReplyProcessorSettingsForm,
) -> std::result::Result<(), String> {
    if settings.threshold == 0 || settings.threshold > 100_000 {
        return Err(t(
            "Threshold must be between 1 and 100000.",
            "阈值必须在 1 到 100000 之间。",
        )
        .to_string());
    }
    if !matches!(settings.mode.as_str(), "image" | "forward") {
        return Err(t(
            "Mode must be Convert to image or Merged forward.",
            "模式必须是转图片或合并转发。",
        )
        .to_string());
    }
    if !matches!(settings.theme.as_str(), "paper" | "light" | "dark") {
        return Err(t(
            "Theme must be paper, light, or dark.",
            "主题必须是 paper、light 或 dark。",
        )
        .to_string());
    }
    if !(1000..=5000).contains(&settings.max_height) {
        return Err(t(
            "Image maximum height must be between 1000 and 5000.",
            "长图最大高度必须在 1000 到 5000 之间。",
        )
        .to_string());
    }
    if !(24..=56).contains(&settings.font_size) || !(20..=46).contains(&settings.code_font_size) {
        return Err(t(
            "Body font size must be 24-56 and code font size must be 20-46.",
            "正文字号必须为 24-56，代码字号必须为 20-46。",
        )
        .to_string());
    }
    if !(36..=120).contains(&settings.padding) {
        return Err(t(
            "Image padding must be between 36 and 120.",
            "长图边距必须在 36 到 120 之间。",
        )
        .to_string());
    }
    if !(1..=168).contains(&settings.ttl_hours) || !(1..=10).contains(&settings.max_records) {
        return Err(t(
            "Context TTL must be 1-168 hours and maximum records must be 1-10.",
            "上下文保留时间必须为 1-168 小时，最大条数必须为 1-10。",
        )
        .to_string());
    }
    Ok(())
}

fn format_id_list(ids: &[i64]) -> String {
    ids.iter()
        .map(i64::to_string)
        .collect::<Vec<_>>()
        .join(", ")
}

fn parse_id_list(value: &str) -> Result<Vec<i64>> {
    value
        .split([',', ' ', '\u{3000}', ';', '\n', '\r', '\t'])
        .map(str::trim)
        .filter(|item| !item.is_empty())
        .map(|item| {
            let id = item.parse::<i64>().map_err(|_| {
                anyhow::anyhow!(t("invalid id: {}", "无效的号码：{}").replace("{}", item))
            })?;
            if id <= 0 {
                bail!(t(
                    "QQ and group ids must be positive",
                    "QQ 号和群号必须为正数"
                ));
            }
            Ok(id)
        })
        .collect()
}

fn edit_provider_form(
    stdout: &mut io::Stdout,
    provider: ProviderConfig,
) -> Result<Option<ProviderConfig>> {
    let current_context_window = provider
        .model_context_window
        .get(&provider.default_model)
        .copied()
        .unwrap_or_default();

    // 将 extra_body 格式化为 JSON 字符串，方便编辑
    let extra_body_string = provider
        .extra_body
        .as_ref()
        .and_then(|v| serde_json::to_string_pretty(v).ok())
        .unwrap_or_default();

    let mut fields = vec![
        Field::new(t("Configuration ID", "配置 ID"), provider.id.clone()),
        Field::new(t("Display name", "显示名称"), provider.display_name.clone()),
        Field::new("Base URL", provider.base_url.clone()),
        Field::new(t("Protocol", "协议"), provider.protocol.clone()).choices(&[
            "auto",
            "openai-chat",
            "openai-responses",
            "anthropic",
        ]),
        Field::new(
            t("API Key or $env:NAME", "API Key 或 $env:NAME"),
            provider.api_key.clone().unwrap_or_default(),
        )
        .sensitive(),
        Field::new(
            t("Current model", "当前模型"),
            provider.default_model.clone(),
        ),
        Field::new(
            t(
                "Model context window (tokens, 0=auto)",
                "模型上下文窗口 (tokens, 0=自动)",
            ),
            current_context_window.to_string(),
        ),
        Field::new(
            t("Timeout (seconds)", "超时秒数"),
            provider.timeout_seconds.to_string(),
        ),
        Field::new("Temperature", provider.temperature.to_string()),
        Field::textarea(
            t("Extra request body (JSON)", "额外请求体 (JSON)"),
            extra_body_string,
        ),
    ];

    // 循环直到用户取消或输入合法 JSON 对象
    loop {
        if !run_form(stdout, t(" EDIT PROVIDER ", " 编辑供应商 "), &mut fields)? {
            return Ok(None);
        }

        // 提取各个字段的值（索引保持不变）
        let default_model = fields[5].value.trim().to_string();
        let model_context_window_raw = fields[6].value.trim().parse::<usize>().unwrap_or_default();
        let timeout = fields[7].value.trim().parse().unwrap_or(60);
        let temperature = fields[8].value.trim().parse().unwrap_or(1.0);

        let extra_body = match parse_extra_body(&fields[9].value) {
            Ok(extra_body) => extra_body,
            Err(error) => {
                message(stdout, &error)?;
                continue;
            }
        };

        // 构建 model_context_window
        let mut model_context_window = provider.model_context_window.clone();
        match model_context_window_raw {
            0 => {
                model_context_window.remove(&default_model);
            }
            value => {
                model_context_window.insert(default_model.clone(), value);
            }
        }

        let mut models = provider.models.clone();
        if !default_model.trim().is_empty() && !models.iter().any(|item| item == &default_model) {
            models.push(default_model.clone());
        }

        // 所有验证通过，返回新的 ProviderConfig
        return Ok(Some(ProviderConfig {
            id: fields[0].value.trim().to_string(),
            display_name: fields[1].value.trim().to_string(),
            base_url: normalize_base_url(&fields[2].value),
            protocol: fields[3].value.trim().to_string(),
            api_key: Some(fields[4].value.trim().to_string()).filter(|value| !value.is_empty()),
            models,
            model_context_window,
            model_modalities: provider.model_modalities.clone(),
            model_costs: provider.model_costs.clone(),
            default_model,
            timeout_seconds: timeout,
            temperature,
            anthropic_max_tokens: provider.anthropic_max_tokens,
            extra_body,
        }));
    }
}

fn parse_extra_body(
    value: &str,
) -> std::result::Result<Option<serde_json::Map<String, serde_json::Value>>, String> {
    let value = value.trim();
    if value.is_empty() {
        return Ok(None);
    }
    match serde_json::from_str::<serde_json::Value>(value) {
        Ok(serde_json::Value::Object(object)) => Ok(Some(object)),
        Ok(_) => Err(t(
            "The extra request body must be a JSON object (for example {\"key\": \"value\"})",
            "额外请求体必须是 JSON 对象 (如 {\"key\": \"value\"})",
        )
        .to_string()),
        Err(error) => Err(if is_zh() {
            format!("无效 JSON: {error}")
        } else {
            format!("Invalid JSON: {error}")
        }),
    }
}

fn edit_model_form(
    stdout: &mut io::Stdout,
    provider: &mut ProviderConfig,
    model: &str,
    thinking_variants: &mut ThinkingVariantPreferences,
) -> Result<bool> {
    let context_window = provider
        .model_context_window
        .get(model)
        .copied()
        .unwrap_or_default();
    let stored_variant = thinking_variants
        .selected(&provider.id, model)
        .filter(|selected| !selected.trim().is_empty())
        .map(str::to_string);
    let variant_options =
        thinking_variant_options_for_model(provider, model, stored_variant.as_deref());
    let initial_variant = stored_variant.clone();
    let cost = provider.model_costs.get(model).copied();
    let currency_value = cost
        .map(|cost| match cost.currency {
            crate::config::CostCurrency::Usd => "USD",
            crate::config::CostCurrency::Cny => "CNY",
        })
        .unwrap_or("")
        .to_string();
    let price_text = |value: Option<f64>| value.map(|v| v.to_string()).unwrap_or_default();
    let mut fields = vec![
        Field::modalities(
            t("Supported input", "支持输入"),
            modality_field_value(provider, model),
        ),
        Field::boolean(
            t("Is an embedding model", "这是语义模型吗"),
            model_is_embedding(provider, model),
        ),
        Field::new(
            t(
                "Model context window (tokens, 0=auto)",
                "模型上下文窗口 (tokens, 0=自动)",
            ),
            context_window.to_string(),
        ),
        thinking_variant_field(&variant_options, stored_variant.as_deref()),
        Field::new("Temperature", provider.temperature.to_string()),
        Field::new(
            t(
                "Price currency (empty = models.dev)",
                "价格货币 (留空=用 models.dev 目录价)",
            ),
            currency_value,
        )
        .choices(&["", "USD", "CNY"])
        .empty_choice_label(t("catalogue", "目录价")),
        Field::new(
            t("Input price / 1M tokens", "输入价 / 1M tokens"),
            price_text(cost.map(|c| c.input)),
        ),
        Field::new(
            t("Output price / 1M tokens", "输出价 / 1M tokens"),
            price_text(cost.map(|c| c.output)),
        ),
        Field::new(
            t(
                "Cache-hit price / 1M (empty = input price)",
                "缓存命中价 / 1M (留空=按输入价)",
            ),
            price_text(cost.and_then(|c| c.cache_read)),
        ),
    ];
    loop {
        if !run_form(stdout, t(" EDIT MODEL ", " 编辑模型 "), &mut fields)? {
            return Ok(false);
        }
        // 价格:选了货币才生效;三个价按所选货币记,估算时统一折 USD。
        match fields[5].value.trim() {
            "" => {
                provider.model_costs.remove(model);
            }
            currency => {
                let parse = |value: &str| -> Option<f64> {
                    let value = value.trim();
                    if value.is_empty() {
                        return None;
                    }
                    value.parse::<f64>().ok().filter(|price| *price >= 0.0)
                };
                let (input, output) = match (parse(&fields[6].value), parse(&fields[7].value)) {
                    (Some(input), Some(output)) => (input, output),
                    _ => {
                        message(
                            stdout,
                            t(
                                "Input and output prices are required non-negative numbers",
                                "输入价与输出价必须是非负数字",
                            ),
                        )?;
                        continue;
                    }
                };
                let cache_read = match (fields[8].value.trim().is_empty(), parse(&fields[8].value))
                {
                    (true, _) => None,
                    (false, Some(price)) => Some(price),
                    (false, None) => {
                        message(
                            stdout,
                            t(
                                "Cache-hit price must be a non-negative number",
                                "缓存命中价必须是非负数字",
                            ),
                        )?;
                        continue;
                    }
                };
                provider.model_costs.insert(
                    model.to_string(),
                    crate::config::ModelCostConfig {
                        currency: if currency == "CNY" {
                            crate::config::CostCurrency::Cny
                        } else {
                            crate::config::CostCurrency::Usd
                        },
                        input,
                        output,
                        cache_read,
                    },
                );
            }
        }
        let mut modalities = parse_modalities(&fields[0].value);
        modalities.retain(|item| item != EMBEDDING_MODALITY);
        if parse_bool_field(&fields[1].value)? {
            modalities.push(EMBEDDING_MODALITY.to_string());
        }
        provider
            .model_modalities
            .insert(model.to_string(), modalities);
        match fields[2].value.trim().parse::<usize>().unwrap_or_default() {
            0 => {
                provider.model_context_window.remove(model);
            }
            value => {
                provider
                    .model_context_window
                    .insert(model.to_string(), value);
            }
        }
        let selected_variant =
            (!fields[3].value.trim().is_empty()).then(|| fields[3].value.trim().to_string());
        if selected_variant != initial_variant {
            thinking_variants.set(&provider.id, model, selected_variant);
        }
        provider.temperature = fields[4].value.trim().parse().unwrap_or(1.0);
        return Ok(true);
    }
}

fn thinking_variant_field(options: &ThinkingVariantOptions, stored: Option<&str>) -> Field {
    let mut choices = Vec::with_capacity(options.variants.len() + 2);
    choices.push(String::new());
    if let Some(stored) = stored.filter(|stored| {
        !stored.is_empty() && !options.variants.iter().any(|variant| variant == *stored)
    }) {
        choices.push(stored.to_string());
    }
    choices.extend(options.variants.iter().cloned());
    Field::new(
        t("Thinking variant", "思考程度"),
        stored.unwrap_or_default().to_string(),
    )
    .choices_owned(choices)
    .raw_choice_labels()
    .empty_choice_label("default")
}

fn edit_settings(stdout: &mut io::Stdout, config: &mut AppConfig) -> Result<()> {
    let language = language_choice_value(&config.display.language).unwrap_or("auto");
    let mut fields = vec![
        Field::boolean(t("Enable tools", "工具启用"), config.tools.enabled),
        Field::new(
            t("Maximum tool rounds", "工具最大轮数"),
            config.tools.max_rounds.to_string(),
        ),
        Field::new(
            t("Tool loading mode", "工具加载模式"),
            config.tools.loading_mode.clone(),
        )
        .choices(&["full", "hybrid", "stub"]),
        Field::boolean(
            t("Remember loaded tools", "记住已加载工具"),
            config.tools.persist_loaded_tools,
        ),
        Field::boolean(t("Enable skills", "Skills 启用"), config.skills.enabled),
        Field::boolean(
            t("Allow command execution", "允许执行命令"),
            config.skills.allow_command_execution,
        ),
        Field::new(t("Interface language", "界面语言"), language.to_string())
            .choices(&["auto", "en", "zh"]),
        Field::new(
            t("Show reasoning", "显示思考过程"),
            config.display.reasoning.clone(),
        )
        .choices(&["summary", "full", "hidden"]),
        Field::new(
            t("Show tool call details", "显示工具调用信息"),
            config.display.tool_calls.clone(),
        )
        .choices(&["summary", "full", "hidden"]),
        Field::new(
            t("Command output lines", "命令输出显示行数"),
            config.display.command_output_lines.to_string(),
        ),
        Field::boolean(
            t("Readable tool names", "工具名可读显示"),
            config.display.readable_tool_names,
        ),
        Field::boolean(
            t(
                "Show token usage in shell conversations",
                "Shell 无缝对话显示 Token 计数",
            ),
            config.display.show_token_usage,
        ),
        Field::new(
            t(
                "Show current provider/model in Mixed mode",
                "Mixed 时显示本次供应商/模型",
            ),
            parse_mixed_endpoint_display(&config.display.mixed_model_endpoint_display),
        )
        .choices(&["off", "interactive", "all"]),
        Field::new(
            t("When context reaches its limit", "上下文到达上限后"),
            config.context.on_overflow.clone(),
        )
        .choices(&["compact", "pop"]),
        // Appended rather than inserted: the read-back below is positional.
        Field::new(
            t("Turns replayed when reopening the REPL", "重开 REPL 回放的轮数"),
            config.display.repl_replay_turns.to_string(),
        ),
    ];
    // The read-back below is by index, so an insert in the middle silently
    // writes every later value into the wrong setting. This catches that in
    // debug builds; new fields go on the end.
    debug_assert_eq!(
        fields.len(),
        15,
        "global settings fields changed: update the positional read-back below"
    );
    run_form_without_buttons(stdout, t(" GLOBAL SETTINGS ", " 全局设置 "), &mut fields)?;
    config.tools.enabled = parse_bool_field(&fields[0].value)?;
    config.tools.max_rounds = fields[1].value.trim().parse::<usize>()?;
    config.tools.loading_mode = normalize_tools_loading_mode(&fields[2].value);
    config.tools.persist_loaded_tools = parse_bool_field(&fields[3].value)?;
    config.skills.enabled = parse_bool_field(&fields[4].value)?;
    config.skills.allow_command_execution = parse_bool_field(&fields[5].value)?;
    config.display.language = language_choice_value(&fields[6].value)
        .unwrap_or("auto")
        .to_string();
    config.display.reasoning = fields[7].value.trim().to_string();
    config.display.tool_calls = fields[8].value.trim().to_string();
    config.display.command_output_lines = fields[9]
        .value
        .trim()
        .parse::<usize>()?
        .min(MAX_COMMAND_OUTPUT_LINES);
    config.display.readable_tool_names = parse_bool_field(&fields[10].value)?;
    config.display.show_token_usage = parse_bool_field(&fields[11].value)?;
    config.display.mixed_model_endpoint_display = parse_mixed_endpoint_display(&fields[12].value);
    config.context.on_overflow = fields[13].value.trim().to_string();
    config.display.repl_replay_turns = fields[14]
        .value
        .trim()
        .parse::<usize>()?
        .min(MAX_REPL_REPLAY_TURNS);
    Ok(())
}

fn language_choice_label(value: &str, zh: bool) -> Option<&'static str> {
    match (value.trim(), zh) {
        ("auto", false) => Some("Auto"),
        ("auto", true) => Some("自动"),
        ("en", false) => Some("English"),
        ("en", true) => Some("英语"),
        ("zh", false) => Some("Simplified Chinese"),
        ("zh", true) => Some("简体中文"),
        _ => None,
    }
}

fn language_choice_value(value: &str) -> Option<&'static str> {
    match value.trim() {
        "auto" | "Auto" | "自动" => Some("auto"),
        "en" | "English" | "英语" => Some("en"),
        "zh" | "Simplified Chinese" | "简体中文" => Some("zh"),
        _ => None,
    }
}

fn parse_mixed_endpoint_display(value: &str) -> String {
    match value.trim() {
        "关" | "Off" | "off" => "off".to_string(),
        "全部模式" | "All modes" | "all" => "all".to_string(),
        _ => "interactive".to_string(),
    }
}

fn normalize_tools_loading_mode(value: &str) -> String {
    match value.trim() {
        "lazy" => "hybrid".to_string(),
        value => value.to_string(),
    }
}

fn parse_bool_field(value: &str) -> Result<bool> {
    match value.trim().to_ascii_lowercase().as_str() {
        "true" | "yes" | "y" | "1" | "on" | "启用" | "是" => Ok(true),
        "false" | "no" | "n" | "0" | "off" | "禁用" | "否" => Ok(false),
        value => {
            if is_zh() {
                bail!("无效的布尔值: {value}")
            } else {
                bail!("Invalid boolean value: {value}")
            }
        }
    }
}

struct FcitxState {
    last_state: Option<char>,
}

impl FcitxState {
    fn new() -> Self {
        let last_state = fcitx5_state();
        run_fcitx5_remote("-c");
        Self { last_state }
    }

    fn enter_editing(&mut self) {
        if self.last_state == Some('2') {
            run_fcitx5_remote("-o");
        }
    }

    fn leave_editing(&mut self) {
        self.last_state = fcitx5_state();
        run_fcitx5_remote("-c");
    }
}

fn fcitx5_state() -> Option<char> {
    let output = Command::new("fcitx5-remote").output().ok()?;
    output.stdout.first().copied().map(char::from)
}

fn run_fcitx5_remote(arg: &str) {
    let _ = Command::new("fcitx5-remote").arg(arg).spawn();
}

fn edit_inline_value(
    stdout: &mut io::Stdout,
    title: &str,
    current: &str,
    sensitive: bool,
) -> Result<Option<String>> {
    let mut value = current.to_string();
    let mut cursor = value.chars().count();
    let mut fcitx = FcitxState::new();
    fcitx.enter_editing();
    loop {
        draw_inline_editor(stdout, title, &value, cursor, sensitive)?;
        match read_key()? {
            KeyCode::Esc => {
                fcitx.leave_editing();
                execute!(stdout, Hide)?;
                return Ok(None);
            }
            KeyCode::Enter => {
                fcitx.leave_editing();
                execute!(stdout, Hide)?;
                return Ok(Some(value));
            }
            KeyCode::Left => cursor = cursor.saturating_sub(1),
            KeyCode::Right => cursor = (cursor + 1).min(value.chars().count()),
            KeyCode::Home => cursor = 0,
            KeyCode::End => cursor = value.chars().count(),
            KeyCode::Backspace if cursor > 0 => remove_char_before_cursor(&mut value, &mut cursor),
            KeyCode::Delete => remove_char_at_cursor(&mut value, cursor),
            KeyCode::Char(ch) => insert_char_at_cursor(&mut value, &mut cursor, ch),
            _ => {}
        }
    }
}

fn draw_inline_editor(
    stdout: &mut io::Stdout,
    title: &str,
    value: &str,
    cursor: usize,
    sensitive: bool,
) -> Result<()> {
    let (cols, rows) = terminal::size()?;
    let width = 72_u16.min(cols.saturating_sub(2)).max(12);
    let height = rows.clamp(1, 6);
    let x = cols.saturating_sub(width) / 2;
    let y = rows.saturating_sub(height) / 2;
    let capacity = width.saturating_sub(4) as usize;
    let chars = value.chars().collect::<Vec<_>>();
    let cursor = cursor.min(chars.len());
    let start = cursor
        .saturating_sub(capacity.saturating_sub(1))
        .min(chars.len().saturating_sub(capacity));
    let end = (start + capacity).min(chars.len());
    let visible = if sensitive {
        "*".repeat(end.saturating_sub(start))
    } else {
        chars[start..end].iter().collect::<String>()
    };

    queue!(stdout, Hide, Clear(ClearType::All))?;
    draw_box(stdout, x, y, width, height, title)?;
    queue!(
        stdout,
        MoveTo(x + 2, y + 2),
        Print(pad(&visible, capacity)),
        MoveTo(x + 2, y + 4),
        SetAttribute(Attribute::Dim),
        Print(truncate(
            t("[Enter]save  [Esc]cancel", "[Enter]保存  [Esc]取消"),
            capacity,
        )),
        SetAttribute(Attribute::Reset),
        MoveTo(
            x + 2 + u16::try_from(cursor.saturating_sub(start)).unwrap_or(u16::MAX),
            y + 2,
        ),
        Show,
    )?;
    stdout.flush()?;
    Ok(())
}

fn run_form(stdout: &mut io::Stdout, title: &str, fields: &mut [Field]) -> Result<bool> {
    run_form_from(stdout, title, fields, false)
}

/// `start_editing` puts the caret in the first field straight away, for forms
/// reached from a menu row that already showed the value: the row said what it
/// was, Enter said "change it", so a second Enter to begin typing is a keypress
/// that asks a question nobody had.
fn run_form_editing(stdout: &mut io::Stdout, title: &str, fields: &mut [Field]) -> Result<bool> {
    run_form_from(stdout, title, fields, true)
}

fn run_form_from(
    stdout: &mut io::Stdout,
    title: &str,
    fields: &mut [Field],
    start_editing: bool,
) -> Result<bool> {
    let mut selected = 0usize;
    let mut fcitx = FcitxState::new();
    // Only a plain text field can be typed into directly; the others open
    // their own picker on Enter, so landing "inside" them would mean typing
    // free text where a choice was expected.
    let mut editing = start_editing
        && fields.first().is_some_and(|field| {
            !field.boolean && !field.textarea && !field.modalities && field.choices.is_empty()
        });
    if editing {
        fcitx.enter_editing();
    }
    let mut cursors = fields
        .iter()
        .map(|field| field.value.chars().count())
        .collect::<Vec<_>>();
    loop {
        draw_form(stdout, title, fields, selected, editing, &cursors, true)?;
        match read_key()? {
            KeyCode::Esc if editing => {
                fcitx.leave_editing();
                editing = false;
            }
            KeyCode::Esc | KeyCode::Char('q') if !editing => return Ok(false),
            KeyCode::Enter if editing => {
                fcitx.leave_editing();
                editing = false;
            }
            KeyCode::Enter if !editing && selected == fields.len() => return Ok(true),
            KeyCode::Enter if !editing && selected == fields.len() + 1 => return Ok(false),
            KeyCode::Enter if !editing && fields[selected].boolean => {
                let value = select_bool(
                    stdout,
                    fields[selected].label,
                    parse_bool_field(&fields[selected].value)?,
                )?;
                fields[selected].value = value.to_string();
                cursors[selected] = fields[selected].value.chars().count();
            }
            KeyCode::Enter if !editing && fields[selected].modalities => {
                fields[selected].value = select_multi_choice(
                    stdout,
                    fields[selected].label,
                    &fields[selected].value,
                    &["text", "image", "audio", "video", "pdf"]
                        .iter()
                        .map(|item| item.to_string())
                        .collect::<Vec<_>>(),
                )?;
                cursors[selected] = fields[selected].value.chars().count();
            }
            KeyCode::Enter if !editing && !fields[selected].choices.is_empty() => {
                fields[selected].value = select_choice(
                    stdout,
                    fields[selected].label,
                    &fields[selected].value,
                    &fields[selected].choices,
                    fields[selected].empty_choice_label,
                    fields[selected].raw_choice_labels,
                )?;
                cursors[selected] = fields[selected].value.chars().count();
            }
            KeyCode::Enter if !editing && fields[selected].textarea => {
                edit_textarea(stdout, &mut fields[selected].value)?;
                cursors[selected] = fields[selected].value.chars().count();
                if !fields[selected].sensitive {
                    return Ok(true);
                }
            }
            KeyCode::Enter if !editing => {
                if !fields[selected].boolean {
                    fcitx.enter_editing();
                    editing = true;
                }
            }
            KeyCode::Char('s') if !editing => return Ok(true),
            KeyCode::Up | KeyCode::Char('k') if !editing => selected = selected.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') if !editing => {
                selected = (selected + 1).min(fields.len() + 1)
            }
            KeyCode::Left | KeyCode::Char('h') if !editing && selected == fields.len() + 1 => {
                selected = fields.len()
            }
            KeyCode::Right | KeyCode::Char('l') if !editing && selected == fields.len() => {
                selected = fields.len() + 1
            }
            KeyCode::Left if editing => cursors[selected] = cursors[selected].saturating_sub(1),
            KeyCode::Right if editing => {
                cursors[selected] =
                    (cursors[selected] + 1).min(fields[selected].value.chars().count())
            }
            KeyCode::Home if editing => cursors[selected] = 0,
            KeyCode::End if editing => cursors[selected] = fields[selected].value.chars().count(),
            KeyCode::Backspace if editing => {
                if cursors[selected] > 0 {
                    remove_char_before_cursor(&mut fields[selected].value, &mut cursors[selected]);
                }
            }
            KeyCode::Delete if editing => {
                remove_char_at_cursor(&mut fields[selected].value, cursors[selected])
            }
            KeyCode::Char(char) if editing => {
                insert_char_at_cursor(&mut fields[selected].value, &mut cursors[selected], char)
            }
            _ => {}
        }
    }
}

fn run_form_without_buttons(
    stdout: &mut io::Stdout,
    title: &str,
    fields: &mut [Field],
) -> Result<()> {
    let mut selected = 0usize;
    let mut editing = false;
    let mut fcitx = FcitxState::new();
    let mut cursors = fields
        .iter()
        .map(|field| field.value.chars().count())
        .collect::<Vec<_>>();
    loop {
        draw_form(stdout, title, fields, selected, editing, &cursors, false)?;
        match read_key()? {
            KeyCode::Esc if editing => {
                fcitx.leave_editing();
                editing = false;
            }
            KeyCode::Esc | KeyCode::Char('q') if !editing => return Ok(()),
            KeyCode::Enter if editing => {
                fcitx.leave_editing();
                editing = false;
            }
            KeyCode::Enter if !editing && fields[selected].boolean => {
                let value = select_bool(
                    stdout,
                    fields[selected].label,
                    parse_bool_field(&fields[selected].value)?,
                )?;
                fields[selected].value = value.to_string();
                cursors[selected] = fields[selected].value.chars().count();
            }
            KeyCode::Enter if !editing && fields[selected].modalities => {
                fields[selected].value = select_multi_choice(
                    stdout,
                    fields[selected].label,
                    &fields[selected].value,
                    &["text", "image", "audio", "video", "pdf"]
                        .iter()
                        .map(|item| item.to_string())
                        .collect::<Vec<_>>(),
                )?;
                cursors[selected] = fields[selected].value.chars().count();
            }
            KeyCode::Enter if !editing && !fields[selected].choices.is_empty() => {
                fields[selected].value = select_choice(
                    stdout,
                    fields[selected].label,
                    &fields[selected].value,
                    &fields[selected].choices,
                    fields[selected].empty_choice_label,
                    fields[selected].raw_choice_labels,
                )?;
                cursors[selected] = fields[selected].value.chars().count();
            }
            KeyCode::Enter if !editing && fields[selected].textarea => {
                edit_textarea(stdout, &mut fields[selected].value)?;
                cursors[selected] = fields[selected].value.chars().count();
                if !fields[selected].sensitive {
                    return Ok(());
                }
            }
            KeyCode::Enter if !editing => {
                if !fields[selected].boolean {
                    fcitx.enter_editing();
                    editing = true;
                }
            }
            KeyCode::Up | KeyCode::Char('k') if !editing => selected = selected.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') if !editing => {
                selected = (selected + 1).min(fields.len().saturating_sub(1))
            }
            KeyCode::Left if editing => cursors[selected] = cursors[selected].saturating_sub(1),
            KeyCode::Right if editing => {
                cursors[selected] =
                    (cursors[selected] + 1).min(fields[selected].value.chars().count())
            }
            KeyCode::Home if editing => cursors[selected] = 0,
            KeyCode::End if editing => cursors[selected] = fields[selected].value.chars().count(),
            KeyCode::Backspace if editing => {
                if cursors[selected] > 0 {
                    remove_char_before_cursor(&mut fields[selected].value, &mut cursors[selected]);
                }
            }
            KeyCode::Delete if editing => {
                remove_char_at_cursor(&mut fields[selected].value, cursors[selected])
            }
            KeyCode::Char(char) if editing => {
                insert_char_at_cursor(&mut fields[selected].value, &mut cursors[selected], char)
            }
            _ => {}
        }
    }
}

fn select_bool(stdout: &mut io::Stdout, label: &str, current: bool) -> Result<bool> {
    let mut selected = if current { 0 } else { 1 };
    let options = [
        boolean_label(true).to_string(),
        boolean_label(false).to_string(),
    ];
    loop {
        draw_menu(stdout, label, &options, selected, "")?;
        match read_key()? {
            KeyCode::Esc | KeyCode::Char('q') => return Ok(current),
            KeyCode::Up | KeyCode::Char('k') => selected = selected.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => selected = (selected + 1).min(options.len() - 1),
            KeyCode::Enter => return Ok(selected == 0),
            _ => {}
        }
    }
}

fn select_choice(
    stdout: &mut io::Stdout,
    label: &str,
    current: &str,
    choices: &[String],
    empty_label: &'static str,
    raw_choice_labels: bool,
) -> Result<String> {
    let mut selected = choices.iter().position(|item| item == current).unwrap_or(0);
    loop {
        let options = choices
            .iter()
            .map(|choice| choice_display_label(choice, empty_label, raw_choice_labels))
            .collect::<Vec<_>>();
        draw_menu(stdout, label, &options, selected, "")?;
        match read_key()? {
            KeyCode::Esc | KeyCode::Char('q') => return Ok(current.to_string()),
            KeyCode::Up | KeyCode::Char('k') => selected = selected.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => selected = (selected + 1).min(choices.len() - 1),
            KeyCode::Enter => return Ok(choices[selected].clone()),
            _ => {}
        }
    }
}

fn select_multi_choice(
    stdout: &mut io::Stdout,
    label: &str,
    current: &str,
    choices: &[String],
) -> Result<String> {
    let mut selected = 0usize;
    let mut active = choices
        .iter()
        .map(|choice| has_modality(current, choice))
        .collect::<Vec<_>>();
    loop {
        let options = choices
            .iter()
            .zip(&active)
            .map(|(choice, active)| {
                format!(
                    "{} {}",
                    if *active { "[*]" } else { "[ ]" },
                    choice_label(choice, "")
                )
            })
            .collect::<Vec<_>>();
        draw_menu(
            stdout,
            label,
            &options,
            selected,
            t(
                "[Tab]select/deselect [Enter/q]confirm",
                "[Tab]选择/取消 [Enter/q]确认",
            ),
        )?;
        match read_key()? {
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Enter => {
                return Ok(choices
                    .iter()
                    .zip(active)
                    .filter_map(|(choice, active)| active.then(|| choice.clone()))
                    .collect::<Vec<_>>()
                    .join(", "))
            }
            KeyCode::Up | KeyCode::Char('k') => selected = selected.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => selected = (selected + 1).min(choices.len() - 1),
            KeyCode::Tab | KeyCode::Char(' ') => active[selected] = !active[selected],
            _ => {}
        }
    }
}

fn choice_label(choice: &str, empty_label: &str) -> String {
    if choice.is_empty() {
        empty_label.to_string()
    } else if let Some((provider, model)) = choice.split_once('\t') {
        format!("{provider} / {model}")
    } else if let Some(label) = localized_choice_label(choice, is_zh()) {
        label.to_string()
    } else {
        choice.to_string()
    }
}

fn choice_display_label(choice: &str, empty_label: &str, raw: bool) -> String {
    if choice.is_empty() {
        empty_label.to_string()
    } else if raw {
        choice.to_string()
    } else {
        choice_label(choice, empty_label)
    }
}

fn boolean_label(value: bool) -> &'static str {
    if value {
        t("Enabled", "启用")
    } else {
        t("Disabled", "禁用")
    }
}

fn localized_choice_label(value: &str, zh: bool) -> Option<&'static str> {
    if let Some(label) = language_choice_label(value, zh) {
        return Some(label);
    }
    match (value.trim(), zh) {
        ("minimal", false) => Some("Minimal"),
        ("minimal", true) => Some("最低"),
        ("low", false) => Some("Low"),
        ("low", true) => Some("低"),
        ("medium", false) => Some("Medium"),
        ("medium", true) => Some("中"),
        ("high", false) => Some("High"),
        ("high", true) => Some("高"),
        ("xhigh", false) => Some("Extra high"),
        ("xhigh", true) => Some("极高"),
        ("global", false) => Some("Global"),
        ("global", true) => Some("全球"),
        ("mainland", false) => Some("Mainland China"),
        ("mainland", true) => Some("中国大陆"),
        ("summary", false) => Some("Summary"),
        ("summary", true) => Some("摘要"),
        ("full", false) => Some("Full"),
        ("full", true) => Some("完整"),
        ("hidden", false) => Some("Hidden"),
        ("hidden", true) => Some("隐藏"),
        ("hybrid", false) => Some("Hybrid"),
        ("hybrid", true) => Some("混合"),
        ("stub", false) => Some("Stub"),
        ("stub", true) => Some("精简常驻"),
        ("off", false) => Some("Off"),
        ("off", true) => Some("关"),
        ("interactive", false) => Some("Interactive only"),
        ("interactive", true) => Some("仅交互模式"),
        ("all", false) => Some("All modes"),
        ("all", true) => Some("全部模式"),
        ("pop", false) => Some("Remove oldest"),
        ("pop", true) => Some("弹出旧消息"),
        ("compact", false) => Some("Compact context"),
        ("compact", true) => Some("压缩上下文"),
        ("text", false) => Some("Text"),
        ("text", true) => Some("文本"),
        ("image", false) => Some("Image"),
        ("image", true) => Some("图片"),
        ("audio", false) => Some("Audio"),
        ("audio", true) => Some("音频"),
        ("video", false) => Some("Video"),
        ("video", true) => Some("视频"),
        ("pdf", false) => Some("PDF"),
        ("pdf", true) => Some("PDF"),
        ("自动", false) => Some("Auto"),
        ("自动", true) => Some("自动"),
        _ => None,
    }
}

fn provider_model_choice_values(config: &AppConfig, include_current: bool) -> Vec<String> {
    let mut choices = vec![String::new()];
    if include_current {
        choices.push(format!(
            "{OPENCODE_PROVIDER_ID}\t{OPENCODE_DEFAULT_VISION_MODEL}"
        ));
    }
    choices.extend(
        config
            .provider_model_choices()
            .into_iter()
            .map(|choice| choice.value()),
    );
    choices
}

fn vision_provider_model_choice_values(config: &AppConfig) -> Vec<String> {
    let mut choices = vec![
        String::new(),
        format!("{OPENCODE_PROVIDER_ID}\t{OPENCODE_DEFAULT_VISION_MODEL}"),
    ];
    choices.extend(
        config
            .multimodal_provider_model_choices()
            .into_iter()
            .map(|choice| choice.value()),
    );
    choices.sort();
    choices.dedup();
    choices
}

fn active_multimodal_label(config: &AppConfig) -> String {
    let choices = config.active_multimodal_provider_model_choices();
    if choices.is_empty() {
        format!(
            "{} / {}",
            OPENCODE_PROVIDER_ID, OPENCODE_DEFAULT_VISION_MODEL
        )
    } else if choices.len() == 1 {
        choices[0].label()
    } else {
        t("Mixed", "混合").to_string()
    }
}

fn modality_field_value(provider: &ProviderConfig, model: &str) -> String {
    provider
        .input_modalities(model)
        .unwrap_or_else(|| vec!["text".to_string()])
        .join(", ")
}

fn parse_modalities(value: &str) -> Vec<String> {
    value
        .split(|ch| ch == ',' || ch == '，' || ch == '\n' || ch == '\r')
        .map(str::trim)
        .filter(|item| !item.is_empty())
        .map(str::to_string)
        .collect()
}

fn has_modality(value: &str, modality: &str) -> bool {
    parse_modalities(value).iter().any(|item| item == modality)
}

fn select_active_multimodal_provider(
    stdout: &mut io::Stdout,
    config: &mut AppConfig,
) -> Result<()> {
    let choices = config.multimodal_provider_model_choices();
    if choices.is_empty() {
        message(
            stdout,
            t(
                "No models support image input. Configure Supported input under Edit model first.",
                "没有支持图片输入的模型，请先在编辑模型里配置支持输入。",
            ),
        )?;
        return Ok(());
    }
    let mut selected = choices
        .iter()
        .position(|choice| {
            config.is_active_multimodal_provider_model(&choice.provider_id, &choice.model)
        })
        .unwrap_or(0);
    loop {
        let options = choices
            .iter()
            .map(|choice| {
                let marker = if config
                    .is_active_multimodal_provider_model(&choice.provider_id, &choice.model)
                {
                    "[*] "
                } else {
                    "[ ] "
                };
                format!("{marker}{}", choice.label())
            })
            .collect::<Vec<_>>();
        draw_menu(
            stdout,
            t(" SELECT MULTIMODAL MODEL ", " 选择多模态模型 "),
            &options,
            selected,
            t(
                "[Tab]activate/deactivate [Enter/q]confirm",
                "[Tab]激活/取消 [Enter/q]确认",
            ),
        )?;
        match read_key()? {
            KeyCode::Char('q') | KeyCode::Esc | KeyCode::Enter => return Ok(()),
            KeyCode::Up | KeyCode::Char('k') => selected = selected.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => selected = (selected + 1).min(options.len() - 1),
            KeyCode::Tab => {
                let choice = choices[selected].clone();
                config
                    .toggle_active_multimodal_provider_model(&choice.provider_id, &choice.model)?;
            }
            _ => {}
        }
    }
}

fn vision_provider_value(config: &AppConfig) -> String {
    let vision = &config.plugins.vision;
    if vision.vision_provider_id.trim().is_empty() {
        format!("{OPENCODE_PROVIDER_ID}\t{OPENCODE_DEFAULT_VISION_MODEL}")
    } else if vision.vision_model.trim().is_empty() {
        config
            .provider(Some(vision.vision_provider_id.trim()))
            .map(|provider| format!("{}\t{}", provider.id, provider.default_model))
            .unwrap_or_else(|_| vision.vision_provider_id.clone())
    } else {
        format!("{}\t{}", vision.vision_provider_id, vision.vision_model)
    }
}

fn kb_embedding_provider_value(config: &AppConfig) -> String {
    let kb = &config.plugins.knowledge_base;
    if kb.embedding_provider_id.trim().is_empty() {
        String::new()
    } else if kb.embedding_model.trim().is_empty() {
        config
            .provider(Some(kb.embedding_provider_id.trim()))
            .map(|provider| format!("{}\t{}", provider.id, provider.default_model))
            .unwrap_or_else(|_| kb.embedding_provider_id.clone())
    } else {
        format!("{}\t{}", kb.embedding_provider_id, kb.embedding_model)
    }
}

fn parse_provider_model_choice(value: &str) -> (String, String) {
    let value = value.trim();
    if value.is_empty() {
        return (String::new(), String::new());
    }
    if let Some((provider, model)) = value.split_once('\t') {
        return (provider.trim().to_string(), model.trim().to_string());
    }
    (value.to_string(), String::new())
}

fn edit_textarea(stdout: &mut io::Stdout, value: &mut String) -> Result<()> {
    execute!(
        stdout,
        Show,
        LeaveAlternateScreen,
        Clear(ClearType::All),
        MoveTo(0, 0)
    )?;
    stdout.flush()?;
    terminal::disable_raw_mode()?;
    let mut file = tempfile::NamedTempFile::new()?;
    file.write_all(value.as_bytes())?;
    let path = file.path().to_path_buf();
    let editor = std::env::var("EDITOR").unwrap_or_else(|_| "vim".to_string());
    let status = Command::new(&editor)
        .arg(&path)
        .status()
        .or_else(|_| Command::new("nano").arg(&path).status());
    if let Err(err) = status {
        if is_zh() {
            eprintln!("无法打开编辑器: {err}");
        } else {
            eprintln!("Failed to open editor: {err}");
        }
    }
    *value = std::fs::read_to_string(&path)?.trim().to_string();
    terminal::enable_raw_mode()?;
    execute!(stdout, EnterAlternateScreen, Clear(ClearType::All), Hide)?;
    Ok(())
}

fn draw_menu(
    stdout: &mut io::Stdout,
    title: &str,
    options: &[String],
    selected: usize,
    status: &str,
) -> Result<()> {
    let (cols, rows) = terminal::size()?;
    let content_w = options
        .iter()
        .map(|option| option.chars().count())
        .max()
        .unwrap_or(20)
        .max(title.chars().count())
        .max(menu_help(status).chars().count())
        + 6;
    let width = (content_w as u16).min(cols.saturating_sub(4)).max(56);
    let height = (options.len() as u16 + 5)
        .min(rows.saturating_sub(2))
        .max(7);
    let x = cols.saturating_sub(width) / 2;
    let y = rows.saturating_sub(height) / 2;
    let visible_rows = height.saturating_sub(4).max(1) as usize;
    let window = menu_window(options.len(), selected, visible_rows);

    queue!(stdout, Clear(ClearType::All))?;
    draw_box(stdout, x, y, width, height, title)?;
    queue!(
        stdout,
        MoveTo(x + 2, y + height - 1),
        SetAttribute(Attribute::Dim),
        Print(truncate(
            menu_help(status),
            width.saturating_sub(4) as usize
        )),
        SetAttribute(Attribute::Reset)
    )?;
    for (row, index) in window.enumerate() {
        let option = &options[index];
        queue!(stdout, MoveTo(x + 2, y + row as u16 + 2))?;
        if index == selected {
            queue!(
                stdout,
                SetAttribute(Attribute::Reverse),
                Print(pad(option, width.saturating_sub(4) as usize)),
                SetAttribute(Attribute::Reset)
            )?;
        } else {
            queue!(stdout, Print(pad(option, width.saturating_sub(4) as usize)))?;
        }
    }
    stdout.flush()?;
    Ok(())
}

fn menu_window(item_count: usize, selected: usize, visible_rows: usize) -> std::ops::Range<usize> {
    if item_count == 0 || visible_rows == 0 {
        return 0..0;
    }
    let visible_rows = visible_rows.min(item_count);
    let selected = selected.min(item_count - 1);
    let start = selected
        .saturating_sub(visible_rows / 2)
        .min(item_count - visible_rows);
    start..start + visible_rows
}

fn menu_help(status: &str) -> &str {
    if status.is_empty() {
        t(
            "[j/k]move [Enter]select [q]back",
            "[j/k]移动 [Enter]选择 [q]返回",
        )
    } else {
        status
    }
}

fn draw_box(
    stdout: &mut io::Stdout,
    x: u16,
    y: u16,
    width: u16,
    height: u16,
    title: &str,
) -> Result<()> {
    queue!(
        stdout,
        MoveTo(x, y),
        Print(format!(
            "┌{}┐",
            "─".repeat(width.saturating_sub(2) as usize)
        ))
    )?;
    for row in 1..height.saturating_sub(1) {
        queue!(
            stdout,
            MoveTo(x, y + row),
            Print(format!(
                "│{}│",
                " ".repeat(width.saturating_sub(2) as usize)
            ))
        )?;
    }
    queue!(
        stdout,
        MoveTo(x, y + height.saturating_sub(1)),
        Print(format!(
            "└{}┘",
            "─".repeat(width.saturating_sub(2) as usize)
        ))
    )?;
    queue!(
        stdout,
        MoveTo(x + 2, y),
        SetAttribute(Attribute::Bold),
        Print(title),
        SetAttribute(Attribute::Reset)
    )?;
    Ok(())
}

fn draw_column(
    stdout: &mut io::Stdout,
    x: u16,
    y: u16,
    width: u16,
    height: u16,
    title: &str,
    items: &[String],
    selected: usize,
    scroll: usize,
    active: bool,
) -> Result<()> {
    let attr = if active {
        Attribute::Reverse
    } else {
        Attribute::Bold
    };
    queue!(
        stdout,
        MoveTo(x, y),
        SetAttribute(attr),
        Print(pad(&truncate(title, width as usize), width as usize)),
        SetAttribute(Attribute::Reset)
    )?;
    let visible_rows = height.saturating_sub(2) as usize;
    let start = column_scroll(selected, scroll, visible_rows);
    for row in 0..visible_rows {
        let index = start + row;
        if index >= items.len() {
            break;
        }
        queue!(stdout, MoveTo(x, y + row as u16 + 1))?;
        let line = truncate(&items[index], width as usize);
        if index == selected {
            queue!(
                stdout,
                SetAttribute(Attribute::Reverse),
                Print(pad(&line, width as usize)),
                SetAttribute(Attribute::Reset)
            )?;
        } else {
            queue!(stdout, Print(pad(&line, width as usize)))?;
        }
    }
    Ok(())
}

fn column_visible_rows() -> usize {
    terminal::size()
        .map(|(_, rows)| rows.saturating_sub(4) as usize)
        .unwrap_or(1)
}

fn column_scroll(selected: usize, scroll: usize, visible_rows: usize) -> usize {
    if visible_rows == 0 {
        return 0;
    }
    if selected < scroll {
        selected
    } else if selected >= scroll + visible_rows {
        selected + 1 - visible_rows
    } else {
        scroll
    }
}

fn draw_form(
    stdout: &mut io::Stdout,
    title: &str,
    fields: &[Field],
    selected: usize,
    editing: bool,
    cursors: &[usize],
    show_buttons: bool,
) -> Result<()> {
    let (cols, rows) = terminal::size()?;
    let width = cols.saturating_sub(8).min(96).max(48);
    let height = (fields.len() as u16 + 8)
        .min(rows.saturating_sub(4))
        .max(10);
    let x = cols.saturating_sub(width) / 2;
    let y = rows.saturating_sub(height) / 2;
    queue!(stdout, Clear(ClearType::All))?;
    draw_box(stdout, x, y, width, height, title)?;
    queue!(
        stdout,
        MoveTo(x + 2, y + 1),
        Print(if show_buttons {
            t(
                "[j/k]move [Enter]edit/open editor [s]confirm [q]back",
                "[j/k]移动 [Enter]编辑/打开编辑器 [s]确认 [q]返回",
            )
        } else {
            t(
                "[j/k]move [Enter]edit/open editor [q]back",
                "[j/k]移动 [Enter]编辑/打开编辑器 [q]返回",
            )
        })
    )?;
    let mut cursor = None;
    for (index, field) in fields.iter().enumerate() {
        let row_y = y + index as u16 + 3;
        queue!(stdout, MoveTo(x + 2, row_y))?;
        let marker = if index == selected { ">" } else { " " };
        let value = field_display_value(field, index == selected && editing);
        let prefix = format!("{marker} {}: ", field.label);
        let line = truncate(
            &format!("{prefix}{value}"),
            width.saturating_sub(4) as usize,
        );
        if index == selected && !editing {
            queue!(
                stdout,
                SetAttribute(Attribute::Reverse),
                Print(pad(&line, width.saturating_sub(4) as usize)),
                SetAttribute(Attribute::Reset)
            )?;
        } else {
            queue!(stdout, Print(pad(&line, width.saturating_sub(4) as usize)))?;
        }
        if index == selected && editing {
            let cursor_text = take_chars(&field.value.replace('\n', " "), cursors[index]);
            let cursor_x = x
                + 2
                + display_width(&prefix) as u16
                + display_width(&truncate(&cursor_text, width.saturating_sub(4) as usize)) as u16;
            cursor = Some((cursor_x.min(x + width.saturating_sub(3)), row_y));
        }
    }
    if show_buttons {
        let button_y = y + fields.len() as u16 + 4;
        draw_form_button(
            stdout,
            x + 2,
            button_y,
            t(" Save ", " 保存 "),
            selected == fields.len() && !editing,
        )?;
        draw_form_button(
            stdout,
            x + 14,
            button_y,
            t(" Back ", " 返回 "),
            selected == fields.len() + 1 && !editing,
        )?;
    }

    let mode = if editing {
        t(
            "Editing; Enter/Esc finishes editing",
            "编辑中，Enter/Esc 结束编辑",
        )
    } else if show_buttons {
        t(
            "Navigating; Enter selects the current item",
            "导航中，Enter 选择当前项",
        )
    } else {
        t(
            "Navigating; Enter selects the current item; [q]back",
            "导航中，Enter 选择当前项，[q]返回",
        )
    };
    queue!(
        stdout,
        MoveTo(x + 2, y + height.saturating_sub(1)),
        Print(truncate(mode, width.saturating_sub(4) as usize))
    )?;
    if let Some((x, y)) = cursor {
        queue!(stdout, Show, MoveTo(x, y))?;
    } else {
        queue!(stdout, Hide)?;
    }
    stdout.flush()?;
    Ok(())
}

fn field_display_value(field: &Field, reveal_sensitive: bool) -> String {
    if field.textarea && field.value.is_empty() {
        t("(Enter opens $EDITOR)", "(Enter 打开 $EDITOR)").to_string()
    } else if field.sensitive && !field.value.is_empty() && !reveal_sensitive {
        if field.textarea {
            if is_zh() {
                format!("[已配置 {} 项]", parse_key_list(&field.value).len())
            } else {
                format!("[{} configured]", parse_key_list(&field.value).len())
            }
        } else {
            "********".to_string()
        }
    } else if !field.choices.is_empty() && field.value.is_empty() {
        field.empty_choice_label.to_string()
    } else if !field.choices.is_empty() {
        choice_display_label(
            &field.value,
            field.empty_choice_label,
            field.raw_choice_labels,
        )
    } else if field.boolean {
        match parse_bool_field(&field.value) {
            Ok(value) => boolean_label(value).to_string(),
            Err(_) => field.value.clone(),
        }
    } else if field.modalities {
        parse_modalities(&field.value)
            .iter()
            .map(|value| choice_label(value, ""))
            .collect::<Vec<_>>()
            .join(", ")
    } else {
        truncate(&field.value.replace('\n', " "), 70)
    }
}

fn draw_form_button(
    stdout: &mut io::Stdout,
    x: u16,
    y: u16,
    label: &str,
    selected: bool,
) -> Result<()> {
    queue!(stdout, MoveTo(x, y))?;
    if selected {
        queue!(
            stdout,
            SetAttribute(Attribute::Reverse),
            Print(label),
            SetAttribute(Attribute::Reset)
        )?;
    } else {
        queue!(stdout, Print(label))?;
    }
    Ok(())
}

fn insert_char_at_cursor(value: &mut String, cursor: &mut usize, ch: char) {
    let byte_index = byte_index_for_char(value, *cursor);
    value.insert(byte_index, ch);
    *cursor += 1;
}

fn remove_char_before_cursor(value: &mut String, cursor: &mut usize) {
    let end = byte_index_for_char(value, *cursor);
    let start = byte_index_for_char(value, cursor.saturating_sub(1));
    value.replace_range(start..end, "");
    *cursor -= 1;
}

fn remove_char_at_cursor(value: &mut String, cursor: usize) {
    if cursor >= value.chars().count() {
        return;
    }
    let start = byte_index_for_char(value, cursor);
    let end = byte_index_for_char(value, cursor + 1);
    value.replace_range(start..end, "");
}

fn byte_index_for_char(value: &str, char_index: usize) -> usize {
    value
        .char_indices()
        .nth(char_index)
        .map(|(index, _)| index)
        .unwrap_or(value.len())
}

fn take_chars(value: &str, count: usize) -> String {
    value.chars().take(count).collect()
}

fn message(stdout: &mut io::Stdout, text: &str) -> Result<()> {
    queue!(
        stdout,
        Clear(ClearType::All),
        MoveTo(0, 0),
        Print(text),
        MoveTo(0, 2),
        Print(t("Press any key to continue", "按任意键继续"))
    )?;
    stdout.flush()?;
    let _ = read_key()?;
    Ok(())
}

fn read_key() -> Result<KeyCode> {
    read_key_with_timeout(None).map(|key| key.expect("blocking read should return a key"))
}

fn read_key_with_timeout(timeout: Option<Duration>) -> Result<Option<KeyCode>> {
    loop {
        if let Some(timeout) = timeout {
            if !event::poll(timeout)? {
                return Ok(None);
            }
        }
        if let Event::Key(key) = event::read()? {
            if key.kind != KeyEventKind::Release {
                return Ok(Some(key.code));
            }
        }
    }
}

fn active_label(config: &AppConfig) -> String {
    match config.active_provider_model_choices().as_slice() {
        [] => t("Not configured", "未配置").to_string(),
        [choice] => format!("{} / {}", choice.provider_name, choice.model),
        _ => t("Mixed", "混合").to_string(),
    }
}

fn normalize_base_url(value: &str) -> String {
    let mut url = value.trim().trim_end_matches('/').to_string();
    if url.ends_with("/chat/completions") {
        url.truncate(url.len() - "/chat/completions".len());
    }
    url
}

fn truncate(value: &str, max: usize) -> String {
    if display_width(value) <= max {
        return value.to_string();
    }
    let mut width = 0usize;
    let mut output = String::new();
    let ellipsis_width = 1usize;
    for ch in value.chars() {
        let char_width = display_width(&ch.to_string());
        if width + char_width + ellipsis_width > max {
            break;
        }
        output.push(ch);
        width += char_width;
    }
    output.push('…');
    output
}

fn display_width(value: &str) -> usize {
    value
        .chars()
        .map(|ch| match ch {
            '\u{1100}'..='\u{115F}'
            | '\u{2329}'..='\u{232A}'
            | '\u{2E80}'..='\u{A4CF}'
            | '\u{AC00}'..='\u{D7A3}'
            | '\u{F900}'..='\u{FAFF}'
            | '\u{FE10}'..='\u{FE19}'
            | '\u{FE30}'..='\u{FE6F}'
            | '\u{FF00}'..='\u{FF60}'
            | '\u{FFE0}'..='\u{FFE6}' => 2,
            _ => 1,
        })
        .sum()
}

fn pad(value: &str, width: usize) -> String {
    let value = truncate(value, width);
    let len = display_width(&value);
    if len >= width {
        value
    } else {
        format!("{value}{}", " ".repeat(width - len))
    }
}

struct Field {
    label: &'static str,
    value: String,
    textarea: bool,
    sensitive: bool,
    boolean: bool,
    modalities: bool,
    choices: Vec<String>,
    empty_choice_label: &'static str,
    raw_choice_labels: bool,
}

impl Field {
    fn new(label: &'static str, value: String) -> Self {
        Self {
            label,
            value,
            textarea: false,
            sensitive: false,
            boolean: false,
            modalities: false,
            choices: Vec::new(),
            empty_choice_label: t("Use current provider", "使用当前 Provider"),
            raw_choice_labels: false,
        }
    }

    fn boolean(label: &'static str, value: bool) -> Self {
        Self {
            label,
            value: value.to_string(),
            textarea: false,
            sensitive: false,
            boolean: true,
            modalities: false,
            choices: Vec::new(),
            empty_choice_label: t("Use current provider", "使用当前 Provider"),
            raw_choice_labels: false,
        }
    }

    fn textarea(label: &'static str, value: String) -> Self {
        Self {
            label,
            value,
            textarea: true,
            sensitive: false,
            boolean: false,
            modalities: false,
            choices: Vec::new(),
            empty_choice_label: t("Use current provider", "使用当前 Provider"),
            raw_choice_labels: false,
        }
    }

    fn choices(mut self, choices: &[&str]) -> Self {
        self.choices = choices.iter().map(|item| item.to_string()).collect();
        self
    }

    fn sensitive(mut self) -> Self {
        self.sensitive = true;
        self
    }

    fn modalities(label: &'static str, value: String) -> Self {
        Self {
            label,
            value,
            textarea: false,
            sensitive: false,
            boolean: false,
            modalities: true,
            choices: Vec::new(),
            empty_choice_label: t("Use current provider", "使用当前 Provider"),
            raw_choice_labels: false,
        }
    }

    fn choices_owned(mut self, choices: Vec<String>) -> Self {
        self.choices = choices;
        self
    }

    fn empty_choice_label(mut self, label: &'static str) -> Self {
        self.empty_choice_label = label;
        self
    }

    fn raw_choice_labels(mut self) -> Self {
        self.raw_choice_labels = true;
        self
    }
}

#[cfg(test)]
mod tests {
    use super::{
        apply_real_context_values, apply_reply_processor_values, choice_display_label,
        field_display_value, language_choice_label, language_choice_value, menu_window,
        parse_extra_body, parse_id_lines, parse_id_list, parse_keyword_lines,
        parse_real_context_identity_lines, parse_real_context_string_lines,
        platform_conversation_id_label, platform_conversation_kind_label, platform_persona_summary,
        real_context_values, reply_processor_mode_label, reply_processor_mode_value,
        reply_processor_values, route_pool_summary, t, thinking_variant_field,
        validate_reply_processor_settings, vision_provider_model_choice_values, Field,
        PersonaMenuTarget, ReplyProcessorSettingsForm, REPLY_PROCESSOR_PLUGIN_ID,
    };
    use crate::config::{
        AppConfig, PlatformConversationKind, PlatformModelPoolInheritance, PlatformPersonaOverride,
        PlatformPluginInstanceConfig, RealContextPluginSettings, REAL_CONTEXT_PLUGIN_ID,
    };
    use crate::llm::ThinkingVariantOptions;

    #[test]
    fn sensitive_field_is_masked_until_actively_edited() {
        let field = Field::new("API Key", "secret-key".to_string()).sensitive();

        assert_eq!(field_display_value(&field, false), "********");
        assert_eq!(field_display_value(&field, true), "secret-key");
    }

    #[test]
    fn empty_sensitive_field_remains_empty() {
        let field = Field::new("API Key", String::new()).sensitive();

        assert_eq!(field_display_value(&field, false), "");
    }

    #[test]
    fn thinking_variant_field_uses_raw_model_options_and_default_choice() {
        let field = thinking_variant_field(
            &ThinkingVariantOptions {
                provider_id: "provider".to_string(),
                model: "model".to_string(),
                variants: vec!["default".to_string(), "high".to_string()],
                selected: Some("default".to_string()),
            },
            Some("default"),
        );

        assert_eq!(field.label, t("Thinking variant", "思考程度"));
        assert_eq!(field.value, "default");
        assert_eq!(field.choices, vec!["", "default", "high"]);
        assert!(field.raw_choice_labels);
        assert_eq!(choice_display_label("high", "", true), "high");
        assert_eq!(field.empty_choice_label, "default");

        let unsupported = thinking_variant_field(
            &ThinkingVariantOptions {
                provider_id: "provider".to_string(),
                model: "plain-model".to_string(),
                variants: Vec::new(),
                selected: None,
            },
            None,
        );
        assert_eq!(unsupported.choices, vec![""]);
        assert_eq!(field_display_value(&unsupported, false), "default");

        let stale = thinking_variant_field(
            &ThinkingVariantOptions {
                provider_id: "provider".to_string(),
                model: "changed-model".to_string(),
                variants: Vec::new(),
                selected: None,
            },
            Some("high"),
        );
        assert_eq!(stale.value, "high");
        assert_eq!(stale.choices, vec!["", "high"]);
        assert_eq!(field_display_value(&stale, false), "high");
    }

    #[test]
    fn sensitive_textarea_displays_configured_item_count() {
        let field = Field::textarea("API Keys", "first\n\nsecond, third".to_string()).sensitive();

        assert_eq!(
            field_display_value(&field, false),
            t("[3 configured]", "[已配置 3 项]")
        );
    }

    #[test]
    fn empty_sensitive_textarea_keeps_editor_placeholder() {
        let field = Field::textarea("API Keys", String::new()).sensitive();

        assert_eq!(
            field_display_value(&field, false),
            t("(Enter opens $EDITOR)", "(Enter 打开 $EDITOR)")
        );
    }

    #[test]
    fn language_choices_have_locale_specific_labels() {
        assert_eq!(language_choice_label("auto", false), Some("Auto"));
        assert_eq!(language_choice_label("en", false), Some("English"));
        assert_eq!(
            language_choice_label("zh", false),
            Some("Simplified Chinese")
        );
        assert_eq!(language_choice_label("auto", true), Some("自动"));
        assert_eq!(language_choice_label("en", true), Some("英语"));
        assert_eq!(language_choice_label("zh", true), Some("简体中文"));
    }

    #[test]
    fn language_choice_labels_map_to_stable_values() {
        for value in ["auto", "Auto", "自动"] {
            assert_eq!(language_choice_value(value), Some("auto"));
        }
        for value in ["en", "English", "英语"] {
            assert_eq!(language_choice_value(value), Some("en"));
        }
        for value in ["zh", "Simplified Chinese", "简体中文"] {
            assert_eq!(language_choice_value(value), Some("zh"));
        }
        assert_eq!(language_choice_value("unsupported"), None);
    }

    #[test]
    fn menu_window_keeps_selection_visible_for_long_lists() {
        assert_eq!(menu_window(100, 0, 5), 0..5);
        assert_eq!(menu_window(100, 50, 5), 48..53);
        assert_eq!(menu_window(100, 99, 5), 95..100);
        assert_eq!(menu_window(3, 2, 10), 0..3);
        assert_eq!(menu_window(0, 0, 5), 0..0);
    }

    #[test]
    fn extra_body_parser_accepts_only_json_objects() {
        for input in ["true", "\"hello\"", "[1, 2, 3]", "{invalid"] {
            assert!(parse_extra_body(input).is_err());
        }

        let parsed = parse_extra_body(r#"{"enable_thinking":false}"#)
            .unwrap()
            .unwrap();
        assert_eq!(parsed["enable_thinking"], false);
        assert!(parse_extra_body("  ").unwrap().is_none());
    }

    #[test]
    fn reply_processor_defaults_match_platform_contract() {
        let config = AppConfig::default();
        let (enabled, settings) = reply_processor_values(&config).unwrap();

        assert!(enabled);
        assert!(settings.default_enabled);
        assert_eq!(settings.threshold, 200);
        assert_eq!(settings.mode, "image");
        assert!(settings.followup_mention);
        assert!(settings.strip_period);
        assert_eq!(settings.theme, "paper");
        assert_eq!(settings.max_height, 2600);
        assert_eq!(settings.font_size, 36);
        assert_eq!(settings.code_font_size, 30);
        assert_eq!(settings.padding, 64);
        assert!(settings.context_notice);
        assert_eq!(settings.ttl_hours, 24);
        assert_eq!(settings.max_records, 3);
        assert!(settings.send_tool_intercept);
        assert!(settings.font.is_empty());
        assert!(settings.title_font.is_empty());
        assert!(settings.code_font.is_empty());
        assert!(settings.emoji_font.is_empty());
    }

    #[test]
    fn reply_processor_mode_labels_preserve_config_values() {
        assert_eq!(
            reply_processor_mode_label("image"),
            t("Convert to image", "转图片")
        );
        assert_eq!(
            reply_processor_mode_label("forward"),
            t("Merged forward", "合并转发")
        );
        assert_eq!(reply_processor_mode_value("转图片"), Some("image"));
        assert_eq!(
            reply_processor_mode_value("Merged forward"),
            Some("forward")
        );
        assert_eq!(reply_processor_mode_value("unsupported"), None);
    }

    #[test]
    fn reply_processor_settings_use_generic_map_and_preserve_unknown_keys() {
        let mut config = AppConfig::default();
        let mut instance = PlatformPluginInstanceConfig {
            enabled: Some(false),
            ..PlatformPluginInstanceConfig::default()
        };
        instance
            .settings
            .insert("future_option".to_string(), serde_json::json!({"value": 1}));
        config
            .platforms
            .qq
            .plugins
            .insert(REPLY_PROCESSOR_PLUGIN_ID.to_string(), instance);
        let settings = ReplyProcessorSettingsForm {
            threshold: 512,
            mode: "forward".to_string(),
            ..ReplyProcessorSettingsForm::default()
        };

        apply_reply_processor_values(&mut config, true, &settings).unwrap();

        let instance = &config.platforms.qq.plugins[REPLY_PROCESSOR_PLUGIN_ID];
        assert_eq!(instance.enabled, None);
        assert_eq!(instance.settings["threshold"], 512);
        assert_eq!(instance.settings["mode"], "forward");
        assert_eq!(instance.settings["future_option"]["value"], 1);
        let (enabled, reparsed) = reply_processor_values(&config).unwrap();
        assert!(enabled);
        assert_eq!(reparsed, settings);
    }

    #[test]
    fn reply_processor_range_validation_rejects_unsafe_render_settings() {
        assert!(validate_reply_processor_settings(&ReplyProcessorSettingsForm::default()).is_ok());
        assert!(
            validate_reply_processor_settings(&ReplyProcessorSettingsForm {
                threshold: 0,
                ..ReplyProcessorSettingsForm::default()
            })
            .is_err()
        );
        assert!(
            validate_reply_processor_settings(&ReplyProcessorSettingsForm {
                max_height: 999,
                ..ReplyProcessorSettingsForm::default()
            })
            .is_err()
        );
        assert!(
            validate_reply_processor_settings(&ReplyProcessorSettingsForm {
                ttl_hours: 169,
                ..ReplyProcessorSettingsForm::default()
            })
            .is_err()
        );
    }

    #[test]
    fn real_context_settings_use_generic_map_and_preserve_unknown_keys() {
        let mut config = AppConfig::default();
        let mut instance = PlatformPluginInstanceConfig::default();
        instance
            .settings
            .insert("future_option".to_string(), serde_json::json!({"value": 1}));
        config
            .platforms
            .qq
            .plugins
            .insert(REAL_CONTEXT_PLUGIN_ID.to_string(), instance);
        let settings = RealContextPluginSettings {
            reply_threshold: 0.9,
            reply_context_window: 42,
            judge_persona_prompt: "judge persona".to_string(),
            ..RealContextPluginSettings::default()
        };

        apply_real_context_values(&mut config, false, &settings);

        let instance = &config.platforms.qq.plugins[REAL_CONTEXT_PLUGIN_ID];
        assert_eq!(instance.enabled, Some(false));
        assert_eq!(instance.settings["reply_threshold"], 0.9);
        assert_eq!(instance.settings["reply_context_window"], 42);
        assert_eq!(instance.settings["judge_persona_prompt"], "judge persona");
        assert_eq!(instance.settings["future_option"]["value"], 1);
        let (enabled, reparsed) = real_context_values(&config).unwrap();
        assert!(!enabled);
        assert_eq!(reparsed, settings);
    }

    #[test]
    fn real_context_batch_parsers_are_line_based_and_deduplicated() {
        let mappings =
            parse_real_context_identity_lines("# 昵称<Tab>QQ号\nMiyu\t123\n小羽 = 456").unwrap();
        assert_eq!(mappings.len(), 2);
        assert_eq!(mappings[0].nickname, "Miyu");
        assert_eq!(mappings[0].user_id, 123);
        assert!(parse_real_context_identity_lines("Miyu\t123\nMiyu\t456").is_err());
        assert!(parse_real_context_identity_lines("Miyu 123").is_err());

        assert_eq!(
            parse_real_context_string_lines("晚安\n 晚安 \nMiyu", 128).unwrap(),
            vec!["晚安", "Miyu"]
        );
    }

    #[test]
    fn route_pool_and_id_helpers_express_inheritance_and_positive_ids() {
        assert_eq!(
            route_pool_summary(None, PlatformModelPoolInheritance::Platform),
            t("inherit platform", "继承 QQ 平台池")
        );
        assert_eq!(
            route_pool_summary(Some(&[]), PlatformModelPoolInheritance::Platform),
            t("inherit platform", "继承 QQ 平台池")
        );
        assert_eq!(
            route_pool_summary(None, PlatformModelPoolInheritance::Global),
            t("inherit global", "继承全局池")
        );
        assert_eq!(parse_id_list("123, 456").unwrap(), vec![123, 456]);
        assert!(parse_id_list("0").is_err());
        assert!(parse_id_list("-1").is_err());
        assert_eq!(parse_id_lines("123\n456\n123\n").unwrap(), vec![123, 456]);
        assert!(parse_id_lines("123\ninvalid\n456").is_err());
        assert_eq!(
            parse_keyword_lines("Miyu\n 小羽 \nMiyu").unwrap(),
            vec!["Miyu", "小羽"]
        );
    }

    #[test]
    fn qq_batch_inputs_are_line_based_trimmed_and_deduplicated() {
        assert_eq!(
            parse_id_lines(" 123 \r\n\r\n456\n123\n").unwrap(),
            vec![123, 456]
        );
        assert!(parse_id_lines("123,456").is_err());
        assert_eq!(
            parse_keyword_lines(" Miyu \r\n\r\n小羽\nMiyu\n").unwrap(),
            vec!["Miyu", "小羽"]
        );
    }

    #[test]
    fn qq_conversation_labels_are_localized_and_id_label_tracks_type() {
        assert_eq!(
            platform_conversation_kind_label(PlatformConversationKind::Private),
            t("Private chat", "私聊")
        );
        assert_eq!(
            platform_conversation_kind_label(PlatformConversationKind::Group),
            t("Group chat", "群聊")
        );
        assert_eq!(
            platform_conversation_id_label(PlatformConversationKind::Private),
            t("QQ id", "QQ 号")
        );
        assert_eq!(
            platform_conversation_id_label(PlatformConversationKind::Group),
            t("Group id", "群号")
        );
    }

    #[test]
    fn qq_conversation_persona_summary_distinguishes_inheritance_and_miyu() {
        assert_eq!(
            platform_persona_summary(&PlatformPersonaOverride::Inherit),
            t("inherit current persona", "继承当前人格")
        );
        assert_eq!(
            platform_persona_summary(&PlatformPersonaOverride::Miyu),
            "Miyu"
        );
        assert_eq!(
            platform_persona_summary(&PlatformPersonaOverride::Custom {
                name: "Group.md".to_string()
            }),
            "Group"
        );
    }

    #[test]
    fn qq_persona_menu_target_isolated_from_global_persona_and_tracks_renames() {
        let mut config = AppConfig::default();
        config.prompt.active_persona = "Global.md".to_string();
        let mut target = PersonaMenuTarget::Platform(PlatformPersonaOverride::Inherit);

        assert_eq!(target.custom_offset(), 2);
        target.activate_custom(&mut config, "Session.md".to_string());
        assert_eq!(config.prompt.active_persona, "Global.md");
        assert_eq!(target.custom_name(&config), Some("Session.md"));
        assert_eq!(target.pending_reference_count("Session.md"), 1);

        target.rename_custom("Session.md", "Renamed.md");
        assert_eq!(target.custom_name(&config), Some("Renamed.md"));
        assert_eq!(target.pending_reference_count("Session.md"), 0);
        assert_eq!(target.pending_reference_count("Renamed.md"), 1);

        target.activate_miyu(&mut config);
        assert!(target.is_miyu(&config));
        assert_eq!(config.prompt.active_persona, "Global.md");
        target.activate_inherit();
        assert!(matches!(
            target,
            PersonaMenuTarget::Platform(PlatformPersonaOverride::Inherit)
        ));
    }

    #[test]
    fn global_persona_menu_target_preserves_activation_behavior() {
        let mut config = AppConfig::default();
        let mut target = PersonaMenuTarget::Global;

        assert_eq!(target.custom_offset(), 1);
        assert!(target.is_miyu(&config));
        target.activate_custom(&mut config, "Global.md".to_string());
        assert_eq!(target.custom_name(&config), Some("Global.md"));
        assert_eq!(target.pending_reference_count("Global.md"), 0);

        target.activate_miyu(&mut config);
        assert!(config.prompt.active_persona.is_empty());
        assert!(target.is_miyu(&config));
    }

    #[test]
    fn explicit_vision_choices_only_include_image_capable_models() {
        let mut config = AppConfig::default();
        let provider = &mut config.providers[0];
        provider.models = vec!["text-only".to_string(), "vision".to_string()];
        provider
            .model_modalities
            .insert("text-only".to_string(), vec!["text".to_string()]);
        provider.model_modalities.insert(
            "vision".to_string(),
            vec!["text".to_string(), "image".to_string()],
        );
        let provider_id = provider.id.clone();

        let choices = vision_provider_model_choice_values(&config);

        assert!(choices.contains(&format!("{provider_id}\tvision")));
        assert!(!choices.contains(&format!("{provider_id}\ttext-only")));
    }
}
