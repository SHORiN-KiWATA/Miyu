//! 供应商与模型的浏览、选择、标签推断。
//!
//! `ProviderBrowser` 会去拉供应商的模型列表，所以要能在拉取失败、超时、返回
//! 格式不对时都继续可用——配置界面不该因为网络问题打不开。
//!
//! `auto_configure_model_tags` 按模型名推断能力标签（视觉、嵌入、思考），只是
//! 预填，用户改了以他为准。

use crate::config_tui::*;

pub(in crate::config_tui) struct ProviderBrowser<'a> {
    pub(in crate::config_tui) paths: &'a MiyuPaths,
    pub(in crate::config_tui) config: &'a mut AppConfig,
    pub(in crate::config_tui) thinking_variants: &'a mut ThinkingVariantPreferences,
    pub(in crate::config_tui) active_col: usize,
    pub(in crate::config_tui) provider_idx: usize,
    pub(in crate::config_tui) provider_scroll: usize,
    pub(in crate::config_tui) org_idx: usize,
    pub(in crate::config_tui) org_scroll: usize,
    pub(in crate::config_tui) model_idx: usize,
    pub(in crate::config_tui) model_scroll: usize,
    pub(in crate::config_tui) filter: String,
    pub(in crate::config_tui) filter_mode: bool,
    pub(in crate::config_tui) raw_models: Vec<String>,
    pub(in crate::config_tui) orgs: Vec<String>,
    pub(in crate::config_tui) models: Vec<ModelEntry>,
    pub(in crate::config_tui) status: String,
    pub(in crate::config_tui) loading: bool,
    pub(in crate::config_tui) fetch_seq: u64,
    /// 删供应商牵连很广（连带清掉它在各个池子与路由里的每一处引用），
    /// 手滑一下要能退回来。
    pub(in crate::config_tui) undo: ConfigUndo,
    pub(in crate::config_tui) fetch_rx: Option<Receiver<FetchResult>>,
}

pub(in crate::config_tui) type FetchResult = (u64, Result<Vec<String>, String>);

pub(in crate::config_tui) fn format_status_line(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[derive(Clone)]
pub(in crate::config_tui) struct ModelEntry {
    pub(in crate::config_tui) name: String,
    pub(in crate::config_tui) full: String,
}

impl ModelEntry {
    pub(in crate::config_tui) fn new(name: &str, full: &str) -> Self {
        Self {
            name: name.to_string(),
            full: full.to_string(),
        }
    }
}

/// `cli_binary`:内置 CLI 供应商列模型要跑的二进制(见 `cli_catalog`);
/// HTTP 供应商忽略。
pub(crate) fn fetch_models(
    provider: &ProviderConfig,
    cli_binary: Option<&str>,
) -> Result<Vec<String>> {
    if provider.is_builtin_cli_provider() {
        // 本机 CLI 后端没有 /models HTTP 端点:目录问 CLI 要(失败就报错),
        // 再并上配置里手工加的名字。只返回 `provider.models` 的话,用户一旦
        // 只激活一个模型,下次进来就只剩那一个可选(09-03)。
        return crate::config_tui::cli_catalog::builtin_cli_catalog(provider, cli_binary);
    }
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

/// 该模型在 models.dev 目录里的条目(读磁盘全量目录,没有就联网取一次)。
pub(in crate::config_tui) fn catalog_entry(
    paths: &MiyuPaths,
    provider: &ProviderConfig,
    model: &str,
) -> Option<crate::models_cache::ModelCatalogEntry> {
    crate::models_cache::describe_models(paths, &provider.id, &provider.base_url, &[model.to_string()])
        .pop()
}

/// 从目录自动同步模型元数据:输入模态、上下文窗口,只补空缺不覆盖手填
/// (09-04,与 WebUI 「从目录补全」同一语义)。价格不落盘——运行时本来就按
/// 目录价估算,编辑表单里只把目录价显示出来。
pub(in crate::config_tui) fn auto_configure_model_tags(
    paths: &MiyuPaths,
    provider: &mut ProviderConfig,
    model: &str,
) {
    let needs_modalities = !provider.model_modalities.contains_key(model);
    let needs_window = !provider.model_context_window.contains_key(model);
    if !needs_modalities && !needs_window {
        return;
    }
    let Some(entry) = catalog_entry(paths, provider, model) else {
        return;
    };
    if needs_modalities {
        if let Some(modalities) = entry.modalities.filter(|modalities| !modalities.is_empty()) {
            provider
                .model_modalities
                .insert(model.to_string(), modalities);
        }
    }
    if needs_window {
        if let Some(window) = entry.context_window.filter(|window| *window > 0) {
            provider
                .model_context_window
                .insert(model.to_string(), window as usize);
        }
    }
}

pub(in crate::config_tui) fn models_url(base_url: &str) -> String {
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
pub(in crate::config_tui) struct ModelsResponse {
    pub(in crate::config_tui) data: Vec<ModelInfo>,
}

#[derive(Deserialize)]
pub(in crate::config_tui) struct ModelInfo {
    pub(in crate::config_tui) id: String,
}

pub(in crate::config_tui) fn select_active_provider(
    stdout: &mut io::Stdout,
    config: &mut AppConfig,
) -> Result<()> {
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
    let mut undo = ConfigUndo::default();
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
            &format!(
                "{}{}",
                t(
                    "[Tab]activate/deactivate [Enter/q]confirm [d]remove",
                    "[Tab]激活/取消 [Enter/q]确认 [d]移除",
                ),
                undo.hint()
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
                undo.record(config);
                let choice = choices[selected].clone();
                config.remove_active_provider_model(&choice.provider_id, &choice.model)?;
                choices = config.text_provider_model_choices();
                if choices.is_empty() {
                    // 列表空了就退不回来了(界面没东西可画),所以当场撤销并说明
                    undo.undo(config);
                    message(
                        stdout,
                        t(
                            "That was the last model; removal was undone.",
                            "这是最后一个模型，已撤销该删除。",
                        ),
                    )?;
                    choices = config.text_provider_model_choices();
                }
                selected = selected.min(choices.len().saturating_sub(1));
            }
            KeyCode::Char('u') => {
                if undo.undo(config) {
                    choices = config.text_provider_model_choices();
                    selected = selected.min(choices.len().saturating_sub(1));
                }
            }
            _ => {}
        }
    }
}

pub(in crate::config_tui) fn model_is_embedding(provider: &ProviderConfig, model: &str) -> bool {
    AppConfig::model_is_embedding(provider, model)
}

pub(in crate::config_tui) fn embedding_model_label(config: &AppConfig) -> String {
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

pub(in crate::config_tui) fn edit_embedding_model(
    stdout: &mut io::Stdout,
    config: &mut AppConfig,
) -> Result<()> {
    // 候选每轮从 config 重建：删除和撤销都改的是 config 本身，重建比两边各维护
    // 一份再想办法同步简单，也不会漏。
    fn embedding_candidates(config: &AppConfig) -> Vec<(String, String)> {
        let mut candidates = Vec::new();
        for provider in &config.providers {
            for model in &provider.models {
                if model_is_embedding(provider, model) {
                    candidates.push((provider.id.clone(), model.clone()));
                }
            }
        }
        candidates
    }

    if embedding_candidates(config).is_empty() {
        message(
            stdout,
            t(
                "No embedding models yet. Mark one in Providers and models -> Edit model.",
                "还没有语义模型。请在「供应商和模型」->「编辑模型」里把某个模型标记为语义模型。",
            ),
        )?;
        return Ok(());
    }
    let mut selected = embedding_candidates(config)
        .iter()
        .position(|(provider, model)| {
            provider == config.embedding.provider_id.trim()
                && model == config.embedding.model.trim()
        })
        .unwrap_or(0);
    let mut undo = ConfigUndo::default();
    loop {
        let candidates = embedding_candidates(config);
        // 尾部两项不是模型，删除键要挡住它们
        let mut options: Vec<String> = candidates
            .iter()
            .map(|(provider, model)| format!("{provider}/{model}"))
            .collect();
        options.push(t("Advanced settings", "高级设置").to_string());
        options.push(t("Clear selection", "清除选择").to_string());
        selected = selected.min(options.len() - 1);
        draw_menu(
            stdout,
            t(" EMBEDDING MODEL ", " EMBEDDING 模型 "),
            &options,
            selected,
            &format!(
                "{}{}",
                t(
                    "[Enter]select [j/k]move [d]remove [q]back",
                    "[Enter]选择 [j/k]移动 [d]移除 [q]返回",
                ),
                undo.hint()
            ),
        )?;
        match read_key()? {
            KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
            KeyCode::Up | KeyCode::Char('k') => selected = selected.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => selected = (selected + 1).min(options.len() - 1),
            KeyCode::Char('d') if selected < candidates.len() => {
                undo.record(config);
                let (provider, model) = candidates[selected].clone();
                config.remove_active_provider_model(&provider, &model)?;
                if embedding_candidates(config).is_empty() {
                    // 空列表没法继续画，当场撤销并说明
                    undo.undo(config);
                    message(
                        stdout,
                        t(
                            "That was the last embedding model; removal was undone.",
                            "这是最后一个语义模型，已撤销该删除。",
                        ),
                    )?;
                }
            }
            KeyCode::Char('u') => {
                undo.undo(config);
            }
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

pub(in crate::config_tui) fn edit_embedding_advanced(
    stdout: &mut io::Stdout,
    config: &mut AppConfig,
) -> Result<()> {
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

pub(in crate::config_tui) fn subagent_tiers_label(config: &AppConfig) -> String {
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

pub(in crate::config_tui) fn tier_display_name(tier: crate::config::ModelTier) -> &'static str {
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
pub(in crate::config_tui) fn select_subagent_tiers(
    stdout: &mut io::Stdout,
    config: &mut AppConfig,
) -> Result<()> {
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
pub(in crate::config_tui) fn select_subagent_tier_models(
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

pub(in crate::config_tui) fn select_model_pool(
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

pub(in crate::config_tui) fn edit_provider_form(
    stdout: &mut io::Stdout,
    provider: ProviderConfig,
) -> Result<Option<ProviderConfig>> {
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
        // claude-code 不在这份下拉里:它是内置特殊供应商的内部协议标识,
        // 不暴露成用户可选概念(那个供应商走自己的专用表单)。
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
        // 「当前模型」不在这张表里。这张表只管「这个供应商怎么连」——地址、
        // 协议、密钥、超时;「用哪个模型」是模型菜单的事(模型列的 [*] 勾选与
        // 「配置文本模型」的池子)。两处都能改一个值,用户改完哪边生效说不清。
        Field::new(
            t("Timeout (seconds)", "超时秒数"),
            provider.timeout_seconds.to_string(),
        ),
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

        // 温度与上下文窗口都是按模型的事,归模型菜单管;供应商表单
        // 不再放这两项(验收:曾牵连全部模型)。当前模型同理,已移走。
        let timeout = fields[5].value.trim().parse().unwrap_or(60);

        let extra_body = match parse_extra_body(&fields[6].value) {
            Ok(extra_body) => extra_body,
            Err(error) => {
                message(stdout, &error)?;
                continue;
            }
        };

        // models 原样带过去。以前这里有一段「把表单里填的模型补进 models」——
        // 表单不再收模型,它就只剩「保存时顺手改一下模型列表」这个与用户操作
        // 无关的副作用了。真要修 default_model ∉ models,那是
        // `prune_model_references` 的活。
        // 所有验证通过，返回新的 ProviderConfig
        return Ok(Some(ProviderConfig {
            id: fields[0].value.trim().to_string(),
            display_name: fields[1].value.trim().to_string(),
            base_url: normalize_base_url(&fields[2].value),
            enabled: provider.enabled,
            protocol: fields[3].value.trim().to_string(),
            api_key: Some(fields[4].value.trim().to_string()).filter(|value| !value.is_empty()),
            models: provider.models.clone(),
            model_context_window: provider.model_context_window.clone(),
            model_temperature: provider.model_temperature.clone(),
            model_tools_loading_mode: provider.model_tools_loading_mode.clone(),
            model_modalities: provider.model_modalities.clone(),
            tool_result_media: provider.tool_result_media,
            model_costs: provider.model_costs.clone(),
            default_model: provider.default_model.clone(),
            timeout_seconds: timeout,
            temperature: provider.temperature,
            anthropic_max_tokens: provider.anthropic_max_tokens,
            extra_body,
        }));
    }
}

pub(in crate::config_tui) fn parse_extra_body(
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

pub(in crate::config_tui) fn edit_model_form(
    stdout: &mut io::Stdout,
    paths: &MiyuPaths,
    provider: &mut ProviderConfig,
    model: &str,
    thinking_variants: &mut ThinkingVariantPreferences,
) -> Result<bool> {
    // 目录信息只用来预填与提示;真正落盘的仍是表单里保存的值。
    let catalog = catalog_entry(paths, provider, model);
    let context_window = provider
        .model_context_window
        .get(model)
        .copied()
        .or_else(|| {
            catalog
                .as_ref()
                .and_then(|entry| entry.context_window)
                .map(|window| window as usize)
        })
        .unwrap_or_default();
    let catalog_price_label: &'static str = match catalog.as_ref().and_then(|entry| entry.cost.as_ref()) {
        Some(cost) => Box::leak(
            format!(
                "{} ${}/{}",
                t("catalogue", "目录价"),
                trim_price(cost.input),
                trim_price(cost.output)
            )
            .into_boxed_str(),
        ),
        None => t("catalogue", "目录价"),
    };
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
        Field::new(
            t(
                "Temperature (empty = provider default)",
                "Temperature (留空=供应商默认)",
            ),
            provider
                .model_temperature
                .get(model)
                .map(|value| value.to_string())
                .unwrap_or_default(),
        ),
        Field::new(
            t(
                "Price currency (empty = models.dev)",
                "价格货币 (留空=用 models.dev 目录价)",
            ),
            currency_value,
        )
        .choices(&["", "USD", "CNY"])
        .empty_choice_label(catalog_price_label),
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
        // 按模型工具加载模式覆盖:约束解码型模型(实测 bigmodel glm-5.3-flash)
        // 把参数生成硬限制在声明 schema 内,吃不下空壳 stub,要单独配完整。
        // 池级解析取最保守(tools::effective_tools_loading_mode)。
        Field::new(
            t("Tool loading mode", "工具加载模式"),
            provider
                .model_tools_loading_mode
                .get(model)
                .map(|mode| crate::config_tui::settings::normalize_tools_loading_mode(mode))
                .unwrap_or_default(),
        )
        .choices(&["", "full", "stub"])
        .empty_choice_label(t("inherit global", "跟随全局")),
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
        match fields[4].value.trim().parse::<f32>() {
            Ok(value) => {
                provider.model_temperature.insert(model.to_string(), value);
            }
            Err(_) => {
                provider.model_temperature.remove(model);
            }
        }
        match fields[9].value.trim() {
            "" => {
                provider.model_tools_loading_mode.remove(model);
            }
            mode => {
                provider.model_tools_loading_mode.insert(
                    model.to_string(),
                    crate::config_tui::settings::normalize_tools_loading_mode(mode),
                );
            }
        }
        return Ok(true);
    }
}

fn trim_price(value: f64) -> String {
    let text = format!("{value:.4}");
    text.trim_end_matches('0').trim_end_matches('.').to_string()
}

pub(in crate::config_tui) fn thinking_variant_field(
    options: &ThinkingVariantOptions,
    stored: Option<&str>,
) -> Field {
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

pub(in crate::config_tui) fn provider_model_choice_values(
    config: &AppConfig,
    include_current: bool,
) -> Vec<String> {
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

pub(in crate::config_tui) fn vision_provider_model_choice_values(
    config: &AppConfig,
) -> Vec<String> {
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

pub(in crate::config_tui) fn active_multimodal_label(config: &AppConfig) -> String {
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

pub(in crate::config_tui) fn modality_field_value(
    provider: &ProviderConfig,
    model: &str,
) -> String {
    provider
        .input_modalities(model)
        .unwrap_or_else(|| vec!["text".to_string()])
        .join(", ")
}

pub(in crate::config_tui) fn parse_modalities(value: &str) -> Vec<String> {
    value
        .split(|ch| ch == ',' || ch == '，' || ch == '\n' || ch == '\r')
        .map(str::trim)
        .filter(|item| !item.is_empty())
        .map(str::to_string)
        .collect()
}

pub(in crate::config_tui) fn has_modality(value: &str, modality: &str) -> bool {
    parse_modalities(value).iter().any(|item| item == modality)
}

pub(in crate::config_tui) fn select_active_multimodal_provider(
    stdout: &mut io::Stdout,
    config: &mut AppConfig,
) -> Result<()> {
    let mut choices = config.multimodal_provider_model_choices();
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
    let mut undo = ConfigUndo::default();
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
            &format!(
                "{}{}",
                t(
                    "[Tab]activate/deactivate [Enter/q]confirm [d]remove",
                    "[Tab]激活/取消 [Enter/q]确认 [d]移除",
                ),
                undo.hint()
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
            // 和文本模型列表一样的语义:把这个模型从供应商里整条删掉,连带
            // 清掉它在各个池子/路由里的引用。服务端下架了模型之后,配置里
            // 那条残留是没别的地方删得掉的。
            KeyCode::Char('d') => {
                undo.record(config);
                let choice = choices[selected].clone();
                config.remove_active_provider_model(&choice.provider_id, &choice.model)?;
                choices = config.multimodal_provider_model_choices();
                if choices.is_empty() {
                    message(
                        stdout,
                        t(
                            "The last multimodal model was removed.",
                            "已移除最后一个多模态模型。",
                        ),
                    )?;
                    return Ok(());
                }
                selected = selected.min(choices.len().saturating_sub(1));
            }
            KeyCode::Char('u') => {
                if undo.undo(config) {
                    choices = config.multimodal_provider_model_choices();
                    selected = selected.min(choices.len().saturating_sub(1));
                }
            }
            _ => {}
        }
    }
}

pub(in crate::config_tui) fn vision_provider_value(config: &AppConfig) -> String {
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

pub(in crate::config_tui) fn kb_embedding_provider_value(config: &AppConfig) -> String {
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

pub(in crate::config_tui) fn parse_provider_model_choice(value: &str) -> (String, String) {
    let value = value.trim();
    if value.is_empty() {
        return (String::new(), String::new());
    }
    if let Some((provider, model)) = value.split_once('\t') {
        return (provider.trim().to_string(), model.trim().to_string());
    }
    (value.to_string(), String::new())
}
