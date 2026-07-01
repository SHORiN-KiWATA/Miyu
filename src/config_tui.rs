use crate::config::{AppConfig, ProviderConfig};
use crate::default_kb;
use crate::default_models::{OPENCODE_DEFAULT_VISION_MODEL, OPENCODE_PROVIDER_ID};
use crate::paths::MiyuPaths;
use anyhow::{bail, Result};
use crossterm::cursor::{Hide, MoveTo, Show};
use crossterm::event::{self, Event, KeyCode, KeyEvent};
use crossterm::style::{Attribute, Print, SetAttribute};
use crossterm::terminal::{self, Clear, ClearType, EnterAlternateScreen, LeaveAlternateScreen};
use crossterm::{execute, queue};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::io::{self, Write};
use std::path::PathBuf;
use std::process::Command;
use std::sync::mpsc::{self, Receiver};
use std::time::Duration;

pub fn run(paths: &MiyuPaths) -> Result<()> {
    AppConfig::init_files(paths)?;
    let config = AppConfig::load_or_default(paths)?;
    TerminalSession::start()?.run(paths, config)
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

    fn run(mut self, paths: &MiyuPaths, mut config: AppConfig) -> Result<()> {
        let result = run_main_menu(&mut self.stdout, paths, &mut config);
        execute!(self.stdout, Show, LeaveAlternateScreen)?;
        terminal::disable_raw_mode()?;
        let _ = result?;
        Ok(())
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
) -> Result<bool> {
    let mut selected = 0usize;
    loop {
        let active = active_label(config);
        let options = [
            format!("激活配置 (当前: {active})"),
            "供应商和模型".to_string(),
            "插件配置".to_string(),
            "自定义提示词".to_string(),
            "全局参数设置".to_string(),
            "保存并退出".to_string(),
        ];
        let status = default_kb::status(paths)
            .ok()
            .filter(|status| status.has_update_notice)
            .map(|_| "默认知识库需要更新，运行 miyu update-default-kb")
            .unwrap_or("");
        draw_menu(stdout, " MIYU CONFIG ", &options, selected, status)?;

        match read_key()? {
            KeyCode::Char('q') | KeyCode::Esc => return Ok(false),
            KeyCode::Up | KeyCode::Char('k') => selected = selected.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => selected = (selected + 1).min(options.len() - 1),
            KeyCode::Enter => match selected {
                0 => select_active_provider(stdout, config)?,
                1 => ProviderBrowser::new(config).run(stdout)?,
                2 => edit_plugins(stdout, config)?,
                3 => edit_custom_prompts(stdout, paths, config)?,
                4 => edit_settings(stdout, config)?,
                5 => {
                    config.save(paths)?;
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
    draw_box(stdout, x, y, width, height, " PLUGINS ")?;
    queue!(
        stdout,
        MoveTo(x + 2, y + 1),
        Print("[Space]启用/禁用 [Enter]配置 [j/k]移动 [q]返回")
    )?;
    queue!(
        stdout,
        MoveTo(x + 2, y + 3),
        SetAttribute(Attribute::Bold),
        Print(pad(
            &plugin_row("状态", "插件", "说明", width.saturating_sub(4) as usize),
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
            "[ON]"
        } else {
            "[OFF]"
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
        ("web", "网络搜索", "搜索 API 与脚本 fallback"),
        ("deep_research", "深度研究", "长任务研究并输出 Markdown"),
        ("vision", "识图", "图片理解和终端预览"),
        ("image_generation", "生图", "文本生成图片"),
        ("web_images", "搜图", "网络图片搜索、下载与审核"),
        ("print_image", "打印图片", "终端图片打印尺寸"),
        ("memes", "表情包", "人格表情库与发送尺寸"),
        ("knowledge_base", "知识库", "本地文件检索与语义索引"),
        ("archlinux", "Arch Linux", "AUR 状态与 ArchWiki 查询"),
        ("man", "在线手册", "在线 man 手册搜索与读取"),
        ("memory", "记忆", "长期记忆与联想"),
        ("package_advisor", "AUR 审查", "PKGBUILD/AUR 安全审查"),
        (
            "linux_game_compatibility",
            "Linux 游戏兼容",
            "Proton/反作弊/兼容性查询",
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
        12 => config.plugins.linux_game_compatibility.enabled,
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
        12 => config.plugins.linux_game_compatibility.enabled = value,
        _ => {}
    }
}

fn edit_plugin_detail(stdout: &mut io::Stdout, config: &mut AppConfig, index: usize) -> Result<()> {
    let title = format!(" PLUGIN: {} ", plugin_names()[index].1);
    let mut fields = plugin_fields(config, index);
    if !run_form(stdout, &title, &mut fields)? {
        return Ok(());
    }
    apply_plugin_fields(config, index, &fields)
}

fn plugin_fields(config: &AppConfig, index: usize) -> Vec<Field> {
    match index {
        0 => vec![
            Field::boolean("启用", config.plugins.web.enabled),
            Field::textarea(
                "Tavily API Keys",
                config.plugins.web.tavily_api_keys.join("\n"),
            ),
            Field::textarea(
                "Firecrawl API Keys",
                config.plugins.web.firecrawl_api_keys.join("\n"),
            ),
            Field::textarea(
                "AnySearch API Keys",
                config.plugins.web.anysearch_api_keys.join("\n"),
            ),
            Field::new("SearXNG URL", config.plugins.web.searxng_base_url.clone()),
        ],
        1 => vec![
            Field::boolean("启用", config.plugins.deep_research.enabled),
            Field::new("输出目录", config.plugins.deep_research.output_dir.clone()),
            Field::new(
                "思考深度",
                config.plugins.deep_research.thinking_depth.clone(),
            )
            .choices(&["minimal", "low", "medium", "high", "xhigh"]),
            Field::new(
                "最大审视修正次数",
                config
                    .plugins
                    .deep_research
                    .max_review_revisions
                    .to_string(),
            ),
            Field::new(
                "每轮工具步数",
                config
                    .plugins
                    .deep_research
                    .max_tool_steps_per_round
                    .to_string(),
            ),
            Field::new(
                "最终字数上限",
                config
                    .plugins
                    .deep_research
                    .max_final_answer_chars
                    .to_string(),
            ),
            Field::new(
                "工具超时秒数",
                config
                    .plugins
                    .deep_research
                    .tool_call_timeout_seconds
                    .to_string(),
            ),
            Field::boolean("显示过程进度", config.plugins.deep_research.show_progress),
        ],
        2 => vec![
            Field::boolean("启用", config.plugins.vision.enabled),
            Field::boolean(
                "优先当前多模态模型",
                config.plugins.vision.prefer_current_multimodal_model,
            ),
            Field::new("识图 Provider/模型", vision_provider_value(config))
                .choices_owned(provider_model_choice_values(config, true)),
        ],
        3 => vec![
            Field::boolean("启用", config.plugins.image_generation.enabled),
            Field::new(
                "生图 API 类型",
                config.plugins.image_generation.provider_type.clone(),
            )
            .choices(&["openai", "rightcode"]),
            Field::new("Base URL", config.plugins.image_generation.base_url.clone()),
            Field::textarea(
                "API Keys",
                config.plugins.image_generation.api_keys.join("\n"),
            ),
            Field::new("模型", config.plugins.image_generation.model.clone()),
            Field::new(
                "默认宽高比",
                config.plugins.image_generation.default_aspect_ratio.clone(),
            )
            .choices(&[
                "自动", "1:1", "2:3", "3:2", "3:4", "4:3", "4:5", "5:4", "9:16", "16:9", "21:9",
            ]),
            Field::new(
                "默认分辨率",
                config.plugins.image_generation.default_resolution.clone(),
            )
            .choices(&["1K", "2K", "4K"]),
            Field::new(
                "输出目录",
                config.plugins.image_generation.output_dir.clone(),
            ),
            Field::boolean("完成后打印", config.plugins.image_generation.auto_print),
            Field::new(
                "超时秒数",
                config.plugins.image_generation.timeout_seconds.to_string(),
            ),
        ],
        4 => vec![
            Field::boolean("启用", config.plugins.web_images.enabled),
            Field::boolean(
                "视觉模型审核",
                config.plugins.web_images.vision_screening_enabled,
            ),
            Field::new(
                "数量上限",
                config.plugins.web_images.max_results.to_string(),
            ),
            Field::boolean("安全搜索", config.plugins.web_images.safe_search),
            Field::boolean("自动预览", config.plugins.web_images.auto_preview),
            Field::new(
                "默认预览数量",
                config.plugins.web_images.preview_count.to_string(),
            ),
            Field::new(
                "最大下载 MB",
                config.plugins.web_images.max_download_mb.to_string(),
            ),
            Field::new(
                "超时秒数",
                config.plugins.web_images.timeout_seconds.to_string(),
            ),
        ],
        5 => vec![
            Field::boolean("启用", config.plugins.print_image.enabled),
            Field::new(
                "打印宽度百分比",
                config.plugins.print_image.width_percent.to_string(),
            ),
            Field::new(
                "打印高度百分比",
                config.plugins.print_image.height_percent.to_string(),
            ),
        ],
        6 => vec![
            Field::boolean("启用", config.plugins.memes.enabled),
            Field::new(
                "发送宽度百分比",
                config.plugins.memes.width_percent.to_string(),
            ),
            Field::new(
                "发送高度百分比",
                config.plugins.memes.height_percent.to_string(),
            ),
            Field::new("最大图片 MB", config.plugins.memes.max_image_mb.to_string()),
            Field::boolean("允许 GIF 动画", config.plugins.memes.allow_gif_animation),
            Field::boolean("自动发送", config.plugins.memes.auto_send_enabled),
            Field::new(
                "自动发送概率",
                config.plugins.memes.auto_send_probability.to_string(),
            ),
            Field::new(
                "自动发送最低置信度",
                config.plugins.memes.auto_send_min_confidence.to_string(),
            ),
        ],
        7 => vec![
            Field::boolean("启用", config.plugins.knowledge_base.enabled),
            Field::new("知识库目录", config.plugins.knowledge_base.data_dir.clone()),
            Field::new(
                "搜索最大结果数",
                config.plugins.knowledge_base.max_search_results.to_string(),
            ),
            Field::new(
                "片段上下文字数",
                config
                    .plugins
                    .knowledge_base
                    .snippet_context_chars
                    .to_string(),
            ),
            Field::new(
                "同窗匹配范围",
                config
                    .plugins
                    .knowledge_base
                    .proximity_window_chars
                    .to_string(),
            ),
            Field::new(
                "读取最大行数",
                config.plugins.knowledge_base.max_read_lines.to_string(),
            ),
            Field::new(
                "最大文件 KB",
                config.plugins.knowledge_base.max_file_size_kb.to_string(),
            ),
            Field::boolean(
                "允许 AI 上传",
                config.plugins.knowledge_base.upload_tool_enabled,
            ),
            Field::boolean(
                "启用 Embedding",
                config.plugins.knowledge_base.embedding_enabled,
            ),
            Field::new(
                "Embedding Provider/模型",
                kb_embedding_provider_value(config),
            )
            .choices_owned(provider_model_choice_values(config, false))
            .empty_choice_label("未配置 Embedding"),
            Field::new(
                "语义块大小",
                config
                    .plugins
                    .knowledge_base
                    .semantic_chunk_chars
                    .to_string(),
            ),
            Field::new(
                "语义块重叠",
                config
                    .plugins
                    .knowledge_base
                    .semantic_chunk_overlap
                    .to_string(),
            ),
            Field::new(
                "语义候选数",
                config.plugins.knowledge_base.semantic_top_k.to_string(),
            ),
            Field::new(
                "语义最低分",
                config.plugins.knowledge_base.semantic_min_score.to_string(),
            ),
            Field::new(
                "关键词强命中阈值",
                config
                    .plugins
                    .knowledge_base
                    .keyword_strong_score_threshold
                    .to_string(),
            ),
            Field::new(
                "Embedding 超时秒数",
                config
                    .plugins
                    .knowledge_base
                    .embedding_timeout_seconds
                    .to_string(),
            ),
        ],
        8 => vec![Field::boolean("启用", config.plugins.archlinux.enabled)],
        9 => vec![Field::boolean("启用", config.plugins.man.enabled)],
        10 => vec![
            Field::boolean("启用", config.plugins.memory.enabled),
            Field::boolean(
                "上下文弹出缓存",
                config.plugins.memory.evicted_context_enabled,
            ),
            Field::boolean("联想启用", config.plugins.memory.association_enabled),
            Field::boolean("自动日记", config.plugins.memory.auto_diary_enabled),
            Field::boolean("自动知识记忆", config.plugins.memory.auto_fact_enabled),
            Field::new(
                "联想知识条数",
                config.plugins.memory.association_facts.to_string(),
            ),
            Field::new(
                "联想事件条数",
                config.plugins.memory.association_episodes.to_string(),
            ),
            Field::new(
                "联想字符上限",
                config.plugins.memory.association_max_chars.to_string(),
            ),
            Field::boolean("遗忘启用", config.plugins.memory.forgetting_enabled),
            Field::new(
                "遗忘半衰期天",
                config.plugins.memory.forgetting_half_life_days.to_string(),
            ),
            Field::new(
                "遗忘最低强度",
                config.plugins.memory.forgetting_min_strength.to_string(),
            ),
            Field::new(
                "回忆增强强度",
                config.plugins.memory.forgetting_review_boost.to_string(),
            ),
        ],
        11 => vec![Field::boolean(
            "启用",
            config.plugins.package_advisor.enabled,
        )],
        12 => vec![Field::boolean(
            "启用",
            config.plugins.linux_game_compatibility.enabled,
        )],
        _ => vec![Field::boolean("启用", plugin_enabled(config, index))],
    }
}

fn apply_plugin_fields(config: &mut AppConfig, index: usize, fields: &[Field]) -> Result<()> {
    match index {
        0 => {
            config.plugins.web.enabled = parse_bool_field(&fields[0].value)?;
            config.plugins.web.tavily_api_keys = parse_key_list(&fields[1].value);
            config.plugins.web.firecrawl_api_keys = parse_key_list(&fields[2].value);
            config.plugins.web.anysearch_api_keys = parse_key_list(&fields[3].value);
            config.plugins.web.searxng_base_url =
                fields[4].value.trim().trim_end_matches('/').to_string();
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
            config.plugins.web_images.vision_screening_enabled =
                parse_bool_field(&fields[1].value)?;
            config.plugins.web_images.max_results =
                fields[2].value.trim().parse::<usize>()?.clamp(1, 10);
            config.plugins.web_images.safe_search = parse_bool_field(&fields[3].value)?;
            config.plugins.web_images.auto_preview = parse_bool_field(&fields[4].value)?;
            config.plugins.web_images.preview_count =
                fields[5].value.trim().parse::<usize>()?.min(5);
            config.plugins.web_images.max_download_mb =
                fields[6].value.trim().parse::<f64>()?.clamp(0.1, 50.0);
            config.plugins.web_images.timeout_seconds =
                fields[7].value.trim().parse::<u64>()?.clamp(5, 120);
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
            config.plugins.memes.allow_gif_animation = parse_bool_field(&fields[4].value)?;
            config.plugins.memes.auto_send_enabled = parse_bool_field(&fields[5].value)?;
            config.plugins.memes.auto_send_probability =
                fields[6].value.trim().parse::<f32>()?.clamp(0.0, 1.0);
            config.plugins.memes.auto_send_min_confidence =
                fields[7].value.trim().parse::<f32>()?.clamp(0.0, 1.0);
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
            config.plugins.memory.enabled = parse_bool_field(&fields[0].value)?;
            config.plugins.memory.evicted_context_enabled = parse_bool_field(&fields[1].value)?;
            config.plugins.memory.association_enabled = parse_bool_field(&fields[2].value)?;
            config.plugins.memory.auto_diary_enabled = parse_bool_field(&fields[3].value)?;
            config.plugins.memory.auto_fact_enabled = parse_bool_field(&fields[4].value)?;
            config.plugins.memory.auto_skill_enabled = false;
            config.plugins.memory.association_facts = fields[5].value.trim().parse::<usize>()?;
            config.plugins.memory.association_episodes = fields[6].value.trim().parse::<usize>()?;
            config.plugins.memory.association_max_chars =
                fields[7].value.trim().parse::<usize>()?;
            config.plugins.memory.forgetting_enabled = parse_bool_field(&fields[8].value)?;
            config.plugins.memory.forgetting_half_life_days =
                fields[9].value.trim().parse::<f64>()?;
            config.plugins.memory.forgetting_min_strength =
                fields[10].value.trim().parse::<f64>()?;
            config.plugins.memory.forgetting_review_boost =
                fields[11].value.trim().parse::<f64>()?;
        }
        11 => {
            config.plugins.package_advisor.enabled = parse_bool_field(&fields[0].value)?;
        }
        12 => {
            config.plugins.linux_game_compatibility.enabled = parse_bool_field(&fields[0].value)?;
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
        let options = [format!("AI 人格 (当前: {persona})"), "用户身份".to_string()];
        draw_menu(
            stdout,
            " CUSTOM PROMPTS ",
            &options,
            selected,
            "[Enter]选择 [q]返回",
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
    std::fs::create_dir_all(config.prompts_dir_path(paths))?;
    let mut selected = 0usize;
    loop {
        let personas = list_personas(paths, config)?;
        let mut options = Vec::with_capacity(personas.len() + 1);
        let default_marker = if config.prompt.active_persona.trim().is_empty() {
            "* "
        } else {
            "  "
        };
        options.push(format!("{default_marker}Miyu"));
        options.extend(personas.iter().map(|name| {
            let display = persona_display_name(name);
            if *name == config.prompt.active_persona {
                format!("* {display}")
            } else {
                format!("  {display}")
            }
        }));
        selected = selected.min(options.len().saturating_sub(1));
        draw_menu(
            stdout,
            " AI 人格 ",
            &options,
            selected,
            "[Tab]激活 [Enter]编辑 [a]新增 [d]删除 [j/k]移动 [q]返回",
        )?;
        match read_key()? {
            KeyCode::Esc | KeyCode::Char('q') => return Ok(()),
            KeyCode::Up | KeyCode::Char('k') => selected = selected.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => selected = (selected + 1).min(options.len() - 1),
            KeyCode::Tab => {
                config.prompt.active_persona = if selected == 0 {
                    String::new()
                } else {
                    personas.get(selected - 1).cloned().unwrap_or_default()
                };
            }
            KeyCode::Char('a') => {
                if let Some(name) = new_persona(stdout, paths, config)? {
                    config.prompt.active_persona = name;
                }
            }
            KeyCode::Enter if selected > 0 => {
                if let Some(name) = personas.get(selected - 1) {
                    if let Some(new_name) = edit_persona(stdout, paths, config, name)? {
                        move_persona_scope(paths, config, name, &new_name)?;
                        if config.prompt.active_persona == *name {
                            config.prompt.active_persona = new_name;
                        }
                    }
                }
            }
            KeyCode::Char('d') if selected > 0 => {
                if let Some(name) = personas.get(selected - 1) {
                    let path = config.persona_path(paths, name);
                    if path.exists() {
                        std::fs::remove_file(path)?;
                    }
                    remove_persona_scope(paths, config, name)?;
                    if config.prompt.active_persona == *name {
                        config.prompt.active_persona.clear();
                    }
                    selected = selected.saturating_sub(1);
                }
            }
            _ => {}
        }
    }
}

fn new_persona(
    stdout: &mut io::Stdout,
    paths: &MiyuPaths,
    config: &AppConfig,
) -> Result<Option<String>> {
    edit_prompt_file_form(
        stdout,
        " NEW PERSONA ",
        None,
        String::new(),
        |name, content| write_persona(paths, config, name, content),
    )
}

fn edit_persona(
    stdout: &mut io::Stdout,
    paths: &MiyuPaths,
    config: &AppConfig,
    current_name: &str,
) -> Result<Option<String>> {
    let content = read_persona(paths, config, current_name)?;
    edit_prompt_file_form(
        stdout,
        " EDIT PERSONA ",
        Some(current_name),
        content,
        |name, content| {
            if name != current_name {
                let old_path = config.persona_path(paths, current_name);
                if old_path.exists() {
                    std::fs::remove_file(old_path)?;
                }
            }
            write_persona(paths, config, name, content)
        },
    )
}

fn move_persona_scope(
    paths: &MiyuPaths,
    config: &AppConfig,
    old_name: &str,
    new_name: &str,
) -> Result<()> {
    if old_name == new_name {
        return Ok(());
    }
    move_dir_if_exists(
        config.persona_memory_data_dir(paths, old_name),
        config.persona_memory_data_dir(paths, new_name),
    )?;
    move_dir_if_exists(
        config.persona_memory_state_dir(paths, old_name),
        config.persona_memory_state_dir(paths, new_name),
    )?;
    move_dir_if_exists(
        config.persona_skills_dir(paths, old_name),
        config.persona_skills_dir(paths, new_name),
    )?;
    Ok(())
}

fn remove_persona_scope(paths: &MiyuPaths, config: &AppConfig, name: &str) -> Result<()> {
    remove_dir_if_exists(config.persona_memory_data_dir(paths, name))?;
    remove_dir_if_exists(config.persona_memory_state_dir(paths, name))?;
    remove_dir_if_exists(config.persona_skills_dir(paths, name))?;
    Ok(())
}

fn move_dir_if_exists(from: PathBuf, to: PathBuf) -> Result<()> {
    if !from.exists() {
        return Ok(());
    }
    if let Some(parent) = to.parent() {
        std::fs::create_dir_all(parent)?;
    }
    if to.exists() {
        std::fs::remove_dir_all(&to)?;
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
        options.push(format!("{default_marker}不使用用户身份"));
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
            " 用户身份 ",
            &options,
            selected,
            "[Tab]激活 [Enter]编辑 [a]新增 [d]删除 [j/k]移动 [q]返回",
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
        " NEW IDENTITY ",
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
        " EDIT IDENTITY ",
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
    let mut fields = vec![
        Field::new(
            "名称",
            current_name
                .map(persona_display_name)
                .unwrap_or("")
                .to_string(),
        ),
        Field::textarea("内容", content),
    ];
    if !run_form(stdout, title, &mut fields)? {
        return Ok(None);
    }
    let name = sanitize_persona_name(&fields[0].value)?;
    write(&name, &fields[1].value)?;
    Ok(Some(name))
}

fn list_personas(paths: &MiyuPaths, config: &AppConfig) -> Result<Vec<String>> {
    list_markdown_files(&config.prompts_dir_path(paths))
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
        bail!("persona name cannot be empty");
    }
    name.push_str(".md");
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
        .split([',', '\n', '\r'])
        .map(str::trim)
        .filter(|item| !item.is_empty())
        .map(str::to_string)
        .collect()
}

struct ProviderBrowser<'a> {
    config: &'a mut AppConfig,
    active_col: usize,
    provider_idx: usize,
    org_idx: usize,
    model_idx: usize,
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
    fn new(config: &'a mut AppConfig) -> Self {
        Self {
            config,
            active_col: 0,
            provider_idx: 0,
            org_idx: 0,
            model_idx: 0,
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
                self.refresh_models();
            }
            1 => {
                self.org_idx = self.org_idx.saturating_sub(1);
                self.rebuild_models();
            }
            2 => self.model_idx = self.model_idx.saturating_sub(1),
            _ => {}
        }
    }

    fn move_down(&mut self) {
        match self.active_col {
            0 => {
                self.provider_idx =
                    (self.provider_idx + 1).min(self.config.providers.len().saturating_sub(1));
                self.refresh_models();
            }
            1 => {
                self.org_idx = (self.org_idx + 1).min(self.orgs.len().saturating_sub(1));
                self.rebuild_models();
            }
            2 => self.model_idx = (self.model_idx + 1).min(self.models.len().saturating_sub(1)),
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
            self.status = "正在获取模型列表...".to_string();
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
                self.status = format!("已获取 {} 个模型", models.len());
                self.raw_models = models;
            }
            Err(err) => {
                self.status = format_status_line(&format!("获取模型失败: {err}"));
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
    }

    fn add_provider(&mut self, stdout: &mut io::Stdout) -> Result<()> {
        if let Some(provider) = edit_provider_form(stdout, ProviderConfig::new_openai_compatible())?
        {
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
        if self.config.active_provider == removed.id {
            self.config.active_provider = self
                .config
                .providers
                .first()
                .map(|provider| provider.id.clone())
                .unwrap_or_default();
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
                    if let Some(provider) = edit_provider_form(stdout, provider)? {
                        let old_id = self.config.providers[self.provider_idx].id.clone();
                        self.config.providers[self.provider_idx] = provider.clone();
                        if self.config.active_provider == old_id {
                            self.config.active_provider = provider.id.clone();
                        }
                        self.refresh_models();
                    }
                }
            }
            2 => {
                if let (Some(provider), Some(model)) = (
                    self.config.providers.get_mut(self.provider_idx),
                    self.models.get(self.model_idx).cloned(),
                ) {
                    if edit_model_form(stdout, provider, &model.full)? {
                        self.config.active_provider = provider.id.clone();
                        self.status = format!("已更新模型设置: {}", model.full);
                    }
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
        if let (Some(provider), Some(model)) = (
            self.config.providers.get_mut(self.provider_idx),
            self.models.get(self.model_idx),
        ) {
            if let Some(index) = provider.models.iter().position(|item| item == &model.full) {
                provider.models.remove(index);
                if provider.default_model == model.full {
                    provider.default_model = provider.models.first().cloned().unwrap_or_default();
                }
                self.status = format!("已取消激活模型: {}", model.full);
            } else {
                provider.models.push(model.full.clone());
                if provider.default_model.trim().is_empty() {
                    provider.default_model = model.full.clone();
                }
                self.status = format!("已激活模型: {}", model.full);
            }
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
                let current = self
                    .config
                    .providers
                    .get(self.provider_idx)
                    .map(|provider| provider.default_model == model.full)
                    .unwrap_or(false);
                let active = self
                    .config
                    .providers
                    .get(self.provider_idx)
                    .map(|provider| provider.models.iter().any(|item| item == &model.full))
                    .unwrap_or(false);
                if current && active {
                    format!("{} [current active]", model.name)
                } else if current {
                    format!("{} [current]", model.name)
                } else if active {
                    format!("{} [active]", model.name)
                } else {
                    model.name.clone()
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
            " PROVIDERS ",
            &providers,
            self.provider_idx,
            self.active_col == 0,
        )?;
        draw_column(
            stdout,
            inner_x + left_w + 1,
            inner_y,
            mid_w,
            inner_h,
            " ORG ",
            &self.orgs,
            self.org_idx,
            self.active_col == 1,
        )?;
        let title = if self.filter.is_empty() {
            " MODELS ".to_string()
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
            self.active_col == 2,
        )?;
        let help = if self.filter_mode {
            format!("搜索: {}_  [Enter]确认 [Esc]取消", self.filter)
        } else {
            "[h/l]切栏 [j/k]移动 [Tab]激活模型 [Enter]模型设置 [/]搜索 [r]刷新 [a]添加 [d]删除 [q]返回"
                .to_string()
        };
        let status = if self.loading {
            self.status.to_string()
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
    let choices = config.provider_model_choices();
    if choices.is_empty() {
        message(stdout, "没有可用 Provider，请先添加。")?;
        return Ok(());
    }
    let mut selected = choices
        .iter()
        .position(|choice| {
            config
                .provider(None)
                .map(|provider| {
                    provider.id == choice.provider_id && provider.default_model == choice.model
                })
                .unwrap_or(false)
        })
        .unwrap_or(0);
    loop {
        let options = choices
            .iter()
            .map(|choice| choice.label())
            .collect::<Vec<_>>();
        draw_menu(
            stdout,
            " SELECT PROVIDER/MODEL ",
            &options,
            selected,
            "[Enter]选择 [q]返回",
        )?;
        match read_key()? {
            KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
            KeyCode::Up | KeyCode::Char('k') => selected = selected.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => selected = (selected + 1).min(options.len() - 1),
            KeyCode::Enter => {
                config.set_active_provider_model(
                    &choices[selected].provider_id,
                    &choices[selected].model,
                )?;
                return Ok(());
            }
            _ => {}
        }
    }
}

fn edit_provider_form(
    stdout: &mut io::Stdout,
    provider: ProviderConfig,
) -> Result<Option<ProviderConfig>> {
    let current_context_chars = provider
        .model_context_chars
        .get(&provider.default_model)
        .copied()
        .unwrap_or_default();
    let mut fields = vec![
        Field::new("配置 ID", provider.id.clone()),
        Field::new("显示名称", provider.display_name.clone()),
        Field::new("Base URL", provider.base_url.clone()),
        Field::new("协议", provider.protocol.clone()).choices(&[
            "auto",
            "openai-chat",
            "openai-responses",
            "anthropic",
        ]),
        Field::new(
            "API Key 或 $env:NAME",
            provider.api_key.clone().unwrap_or_default(),
        ),
        Field::new("当前模型", provider.default_model.clone()),
        Field::new("模型上下文字符数", current_context_chars.to_string()),
        Field::new("超时秒数", provider.timeout_seconds.to_string()),
        Field::new("Temperature", provider.temperature.to_string()),
    ];
    if !run_form(stdout, " EDIT PROVIDER ", &mut fields)? {
        return Ok(None);
    }
    let default_model = fields[5].value.trim().to_string();
    let mut model_context_chars = provider.model_context_chars.clone();
    match fields[6].value.trim().parse::<usize>().unwrap_or_default() {
        0 => {
            model_context_chars.remove(&default_model);
        }
        value => {
            model_context_chars.insert(default_model.clone(), value);
        }
    }
    let mut models = provider.models.clone();
    if !default_model.trim().is_empty() && !models.iter().any(|item| item == &default_model) {
        models.push(default_model.clone());
    }
    Ok(Some(ProviderConfig {
        id: fields[0].value.trim().to_string(),
        display_name: fields[1].value.trim().to_string(),
        base_url: normalize_base_url(&fields[2].value),
        protocol: fields[3].value.trim().to_string(),
        api_key: Some(fields[4].value.trim().to_string()).filter(|value| !value.is_empty()),
        models,
        model_context_chars,
        default_model,
        timeout_seconds: fields[7].value.trim().parse().unwrap_or(60),
        temperature: fields[8].value.trim().parse().unwrap_or(0.7),
    }))
}

fn edit_model_form(
    stdout: &mut io::Stdout,
    provider: &mut ProviderConfig,
    model: &str,
) -> Result<bool> {
    let active = provider.models.iter().any(|item| item == model);
    let current = provider.default_model == model;
    let context_chars = provider
        .model_context_chars
        .get(model)
        .copied()
        .unwrap_or_default();
    let mut fields = vec![
        Field::boolean("激活模型", active),
        Field::boolean("设为当前模型", current),
        Field::new("模型上下文字符数", context_chars.to_string()),
    ];
    if !run_form(stdout, " EDIT MODEL ", &mut fields)? {
        return Ok(false);
    }
    let active = parse_bool_field(&fields[0].value)?;
    let current = parse_bool_field(&fields[1].value)?;
    if active {
        if !provider.models.iter().any(|item| item == model) {
            provider.models.push(model.to_string());
        }
    } else {
        provider.models.retain(|item| item != model);
    }
    if current || provider.default_model == model && !active {
        provider.default_model = if active {
            model.to_string()
        } else {
            provider.models.first().cloned().unwrap_or_default()
        };
        if !provider.default_model.is_empty()
            && !provider
                .models
                .iter()
                .any(|item| item == &provider.default_model)
        {
            provider.models.push(provider.default_model.clone());
        }
    }
    match fields[2].value.trim().parse::<usize>().unwrap_or_default() {
        0 => {
            provider.model_context_chars.remove(model);
        }
        value => {
            provider
                .model_context_chars
                .insert(model.to_string(), value);
        }
    }
    Ok(true)
}

fn edit_settings(stdout: &mut io::Stdout, config: &mut AppConfig) -> Result<()> {
    let mut fields = vec![
        Field::boolean("工具启用", config.tools.enabled),
        Field::new("工具最大轮数", config.tools.max_rounds.to_string()),
        Field::boolean("Skills 启用", config.skills.enabled),
        Field::boolean("允许执行命令", config.skills.allow_command_execution),
        Field::new("显示思考过程", config.display.reasoning.clone())
            .choices(&["summary", "full", "hidden"]),
        Field::new("显示工具调用信息", config.display.tool_calls.clone())
            .choices(&["summary", "full", "hidden"]),
        Field::boolean("工具名可读显示", config.display.readable_tool_names),
    ];
    if run_form(stdout, " GLOBAL SETTINGS ", &mut fields)? {
        config.tools.enabled = parse_bool_field(&fields[0].value)?;
        config.tools.max_rounds = fields[1].value.trim().parse::<usize>()?;
        config.skills.enabled = parse_bool_field(&fields[2].value)?;
        config.skills.allow_command_execution = parse_bool_field(&fields[3].value)?;
        config.display.reasoning = fields[4].value.trim().to_string();
        config.display.tool_calls = fields[5].value.trim().to_string();
        config.display.readable_tool_names = parse_bool_field(&fields[6].value)?;
    }
    Ok(())
}

fn parse_bool_field(value: &str) -> Result<bool> {
    match value.trim().to_ascii_lowercase().as_str() {
        "true" | "yes" | "y" | "1" | "on" | "启用" | "是" => Ok(true),
        "false" | "no" | "n" | "0" | "off" | "禁用" | "否" => Ok(false),
        value => bail!("invalid boolean value: {value}"),
    }
}

struct FcitxState {
    last_state: Option<char>,
}

impl FcitxState {
    fn new() -> Self {
        run_fcitx5_remote("-c");
        Self {
            last_state: Some('1'),
        }
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

fn run_form(stdout: &mut io::Stdout, title: &str, fields: &mut [Field]) -> Result<bool> {
    let mut selected = 0usize;
    let mut editing = false;
    let mut fcitx = FcitxState::new();
    let mut cursors = fields
        .iter()
        .map(|field| field.value.chars().count())
        .collect::<Vec<_>>();
    loop {
        draw_form(stdout, title, fields, selected, editing, &cursors)?;
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
            KeyCode::Enter if !editing && !fields[selected].choices.is_empty() => {
                fields[selected].value = select_choice(
                    stdout,
                    fields[selected].label,
                    &fields[selected].value,
                    &fields[selected].choices,
                    fields[selected].empty_choice_label,
                )?;
                cursors[selected] = fields[selected].value.chars().count();
            }
            KeyCode::Enter if !editing && fields[selected].textarea => {
                edit_textarea(stdout, &mut fields[selected].value)?;
                return Ok(true);
            }
            KeyCode::Enter if !editing && !fields[selected].boolean => {
                fcitx.enter_editing();
                editing = true;
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
            KeyCode::Backspace if editing && cursors[selected] > 0 => {
                remove_char_before_cursor(&mut fields[selected].value, &mut cursors[selected]);
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
    let options = ["true".to_string(), "false".to_string()];
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
) -> Result<String> {
    let mut selected = choices.iter().position(|item| item == current).unwrap_or(0);
    loop {
        let options = choices
            .iter()
            .map(|choice| choice_label(choice, empty_label))
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

fn choice_label(choice: &str, empty_label: &str) -> String {
    if choice.is_empty() {
        empty_label.to_string()
    } else if let Some((provider, model)) = choice.split_once('\t') {
        format!("{provider} / {model}")
    } else {
        choice.to_string()
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
        eprintln!("failed to open editor: {err}");
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
    for (index, option) in options.iter().enumerate() {
        queue!(stdout, MoveTo(x + 2, y + index as u16 + 2))?;
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

fn menu_help(status: &str) -> &str {
    if status.is_empty() {
        "[j/k]移动 [Enter]选择 [q]返回"
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

#[allow(clippy::too_many_arguments)]
fn draw_column(
    stdout: &mut io::Stdout,
    x: u16,
    y: u16,
    width: u16,
    height: u16,
    title: &str,
    items: &[String],
    selected: usize,
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
    let start = selected.saturating_sub(visible_rows.saturating_sub(1));
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

fn draw_form(
    stdout: &mut io::Stdout,
    title: &str,
    fields: &[Field],
    selected: usize,
    editing: bool,
    cursors: &[usize],
) -> Result<()> {
    let (cols, rows) = terminal::size()?;
    let width = cols.saturating_sub(8).clamp(48, 96);
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
        Print("[j/k]移动 [Enter]编辑/打开编辑器 [s]确认 [q]取消")
    )?;
    let mut cursor = None;
    for (index, field) in fields.iter().enumerate() {
        let row_y = y + index as u16 + 3;
        queue!(stdout, MoveTo(x + 2, row_y))?;
        let marker = if index == selected { ">" } else { " " };
        let value = if field.textarea && field.value.is_empty() {
            "(Enter 打开 $EDITOR)".to_string()
        } else if !field.choices.is_empty() && field.value.is_empty() {
            field.empty_choice_label.to_string()
        } else if !field.choices.is_empty() {
            choice_label(&field.value, field.empty_choice_label)
        } else {
            truncate(&field.value.replace('\n', " "), 70)
        };
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
    let button_y = y + fields.len() as u16 + 4;
    draw_form_button(
        stdout,
        x + 2,
        button_y,
        " 保存 ",
        selected == fields.len() && !editing,
    )?;
    draw_form_button(
        stdout,
        x + 14,
        button_y,
        " 取消 ",
        selected == fields.len() + 1 && !editing,
    )?;

    let mode = if editing {
        "编辑中，Enter/Esc 结束编辑"
    } else {
        "导航中，Enter 选择当前项"
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
        Print("按任意键继续")
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
        if let Event::Key(KeyEvent { code, .. }) = event::read()? {
            return Ok(Some(code));
        }
    }
}

fn active_label(config: &AppConfig) -> String {
    config
        .provider(None)
        .map(|provider| format!("{} / {}", provider.display_name, provider.default_model))
        .unwrap_or_else(|_| "未配置".to_string())
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
    boolean: bool,
    choices: Vec<String>,
    empty_choice_label: &'static str,
}

impl Field {
    fn new(label: &'static str, value: String) -> Self {
        Self {
            label,
            value,
            textarea: false,
            boolean: false,
            choices: Vec::new(),
            empty_choice_label: "使用当前 Provider",
        }
    }

    fn boolean(label: &'static str, value: bool) -> Self {
        Self {
            label,
            value: value.to_string(),
            textarea: false,
            boolean: true,
            choices: Vec::new(),
            empty_choice_label: "使用当前 Provider",
        }
    }

    fn textarea(label: &'static str, value: String) -> Self {
        Self {
            label,
            value,
            textarea: true,
            boolean: false,
            choices: Vec::new(),
            empty_choice_label: "使用当前 Provider",
        }
    }

    fn choices(mut self, choices: &[&str]) -> Self {
        self.choices = choices.iter().map(|item| item.to_string()).collect();
        self
    }

    fn choices_owned(mut self, choices: Vec<String>) -> Self {
        self.choices = choices;
        self
    }

    fn empty_choice_label(mut self, label: &'static str) -> Self {
        self.empty_choice_label = label;
        self
    }
}
