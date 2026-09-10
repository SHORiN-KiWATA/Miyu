//! 插件的开关与逐项设置。
//!
//! 插件的字段是**运行时才知道**的：`plugin_fields` 按插件 ID 生成表单描述，
//! `apply_plugin_fields` 再把用户填的值写回配置。加插件只用改这两处，不用碰
//! TUI 框架。

use crate::config_tui::*;

pub(in crate::config_tui) fn edit_plugins(
    stdout: &mut io::Stdout,
    config: &mut AppConfig,
) -> Result<()> {
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

pub(in crate::config_tui) fn draw_plugin_menu(
    stdout: &mut io::Stdout,
    config: &AppConfig,
    selected: usize,
) -> Result<()> {
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

pub(in crate::config_tui) fn plugin_row(
    state: &str,
    name: &str,
    description: &str,
    width: usize,
) -> String {
    let fixed = pad(state, 8) + &pad(name, 24);
    let remaining = width.saturating_sub(display_width(&fixed)).max(10);
    fixed + &truncate(description, remaining)
}

pub(in crate::config_tui) fn plugin_names() -> [(&'static str, &'static str, &'static str); 12] {
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

pub(in crate::config_tui) fn plugin_enabled(config: &AppConfig, index: usize) -> bool {
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
        9 => config.plugins.memory.enabled,
        10 => config.plugins.package_advisor.enabled,
        11 => config.plugins.api_quota.enabled,
        _ => false,
    }
}

pub(in crate::config_tui) fn toggle_plugin(config: &mut AppConfig, index: usize) {
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
        9 => config.plugins.memory.enabled = value,
        10 => config.plugins.package_advisor.enabled = value,
        11 => config.plugins.api_quota.enabled = value,
        _ => {}
    }
}

pub(in crate::config_tui) fn edit_plugin_detail(
    stdout: &mut io::Stdout,
    config: &mut AppConfig,
    index: usize,
) -> Result<()> {
    // api_quota 是 plugin_names() 的最后一项(下标 12):它有专门的账号
    // 管理界面,不走通用表单。
    if index == plugin_names().len() - 1 {
        return edit_api_quota(stdout, config);
    }
    let title = format!(" {}: {} ", t("PLUGIN", "插件"), plugin_names()[index].1);
    let mut fields = plugin_fields(config, index);
    if !run_form(stdout, &title, &mut fields)? {
        return Ok(());
    }
    apply_plugin_fields(config, index, &fields)
}

pub(in crate::config_tui) fn plugin_fields(config: &AppConfig, index: usize) -> Vec<Field> {
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
                t(
                    "Exa API Keys (optional; keyless free quota)",
                    "Exa API Keys（可留空用免费额度）",
                ),
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
        9 => {
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
        10 => vec![Field::boolean(
            t("Enabled", "启用"),
            config.plugins.package_advisor.enabled,
        )],
        11 => vec![Field::boolean(
            t("Enabled", "启用"),
            config.plugins.api_quota.enabled,
        )],
        _ => vec![Field::boolean(
            t("Enabled", "启用"),
            plugin_enabled(config, index),
        )],
    }
}

pub(in crate::config_tui) fn apply_plugin_fields(
    config: &mut AppConfig,
    index: usize,
    fields: &[Field],
) -> Result<()> {
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
        10 => {
            config.plugins.package_advisor.enabled = parse_bool_field(&fields[0].value)?;
        }
        11 => {
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
