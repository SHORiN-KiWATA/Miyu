mod alarm;
mod archlinux;
mod calculator;
mod caniplayonlinux_query;
mod deep_diagnose;
mod deep_research;
mod deepseek_status;
mod default_tools;
mod diagnostics;
mod edit_replace;
mod exchange_rate;
mod fcitx_wiki;
mod hash_codec;
mod image_generation;
pub mod knowledge_base;
mod linux_game;
mod man;
pub(crate) mod memes;
mod memory;
mod moegirl;
mod package_advisor;
mod protondb_query;
mod registry;
mod scripts;
mod skills;
mod subagent_runner;
mod task;
mod todowrite;
pub mod vision;
mod weather;
mod web;
mod web_images;
mod write;
mod xuanxue;

use crate::config::AppConfig;
use crate::paths::MiyuPaths;
use std::collections::HashMap;
use std::sync::RwLock;

#[allow(unused_imports)]
pub use registry::{empty_parameters, ToolPermission, ToolProgress, ToolRegistry, ToolSpec};
pub use skills::{register_skills, skills_prompt};
pub(crate) use scripts::rescan_scripts;

static SCRIPT_DISPLAY_NAMES: RwLock<Option<HashMap<String, String>>> = RwLock::new(None);

pub fn register_script_display_names(registry: &ToolRegistry) {
    let mut map = HashMap::new();
    for name in registry.tool_names() {
        if let Some(dn) = registry.display_name(&name) {
            map.insert(name, dn);
        }
    }
    *SCRIPT_DISPLAY_NAMES.write().unwrap() = Some(map);
}

pub fn readable_tool_name(name: &str) -> String {
    if let Ok(guard) = SCRIPT_DISPLAY_NAMES.read() {
        if let Some(map) = guard.as_ref() {
            if let Some(dn) = map.get(name) {
                return dn.clone();
            }
        }
    }
    builtin_readable_tool_name(name).to_string()
}

fn builtin_readable_tool_name(name: &str) -> String {
    let result: &str = match name {
        "run_command" => "运行命令",
        "task" => "子代理任务",
        "read_file" => "读取文件",
        "write_file" => "写入文件",
        "edit_file" => "编辑文件",
        "edit_string" => "字符串编辑",
        "list_directory" => "列目录",
        "create_directory" => "创建目录",
        "trash_path" => "移入回收站",
        "find_files" | "glob" => "查找文件",
        "search_text" | "grep" => "搜索文本",
        "get_current_directory" => "当前目录",
        "get_current_time" => "当前时间",
        "check_issue" => "检查问题",
        "check_os_info" => "查看系统信息",
        "web_search" => "网页搜索",
        "web_fetch" => "读取网页",
        "fcitx5_input_method_wiki_qurey" => "查询 Fcitx5 Wiki",
        "search_web_images" => "搜索图片",
        "analyze_image" | "vision_analyze" => "分析图片",
        "print_image" => "显示图片",
        "generate_image" => "生成图片",
        "search_meme" => "搜索表情包",
        "show_meme" => "发送表情",
        "add_meme" => "添加表情包",
        "update_meme" => "更新表情包",
        "delete_meme" => "删除表情包",
        "deep_research" => "深度研究",
        "deep_diagnose" | "linux_input_method_diagnose" => "输入法诊断",
        "upload_knowledge_base_file" | "upload_text_to_knowledge_base" => "导入知识库",
        "read_knowledge_base_file" => "读取知识库",
        "search_knowledge_base" => "搜索知识库",
        "search_knowledge_base_by_name" => "按名称搜索知识库",
        "edit_knowledge_base_file" => "编辑知识库",
        "remove_knowledge_base_file" => "移除知识库",
        "list_knowledge_base_files" => "列出知识库",
        "set_alarm" => "设置闹钟",
        "list_alarms" => "列出闹钟",
        "cancel_alarm" => "取消闹钟",
        "remember_fact" => "记录记忆",
        "search_evicted_context" => "搜索旧上下文",
        "recall_past_events" => "回忆往事",
        "recall_memory" | "recall_memories" => "召回记忆",
        "forget_memory" | "forget_memories" => "删除记忆",
        "list_memory" | "list_memories" => "列出记忆",
        "aur_search_packages" => "搜索 AUR",
        "aur_get_package_info" => "查看 AUR 包",
        "aur_check_status" => "查询 AUR 状态",
        "archlinux_official_package_query" => "查询 Arch 官方包",
        "query_deepseek_status" => "查询 DeepSeek 状态",
        "pacman_search" => "搜索软件包",
        "archwiki_query" => "查询 ArchWiki",
        "archlinux_news" => "Arch 新闻",
        "online_man_search" | "man_search" => "搜索在线手册",
        "online_man_get_page" | "man_read" => "读取在线手册",
        "moegirl_query" => "查询萌娘百科",
        "calculate" | "calculator" | "scientific_calculator" => "科学计算",
        "calculate_hash" => "计算哈希",
        "decode_encoded_text" => "解码文本",
        "exchange_rate" | "get_exchange_rate" => "汇率查询",
        "weather" | "get_weather" => "天气查询",
        "query_caniplayonlinux" => "查询是否能在Linux上玩",
        "protondb_query" => "查询 ProtonDB",
        "xuanxue_pick" => "玄学选择",
        "xuanxue_divine" => "玄学占卜",
        "draw_zhouyi_hexagram" => "周易起卦",
        "draw_tarot_card" => "抽塔罗牌",
        "draw_fortune_lot" => "吉凶占",
        "roll_dice" => "掷骰子",
        "load_skill" => "加载技能",
        "register_script" => "注册脚本",
        "unregister_script" => "注销脚本",
        "list_scripts" => "列出脚本",
        "todowrite" => "任务列表",
        "todoupdate" => "更新任务",
        "review_aur_package" => "审查 AUR 包",
        "install_aur_package" => "安装 AUR 包",
        "review_pkgbuild_directory" => "审查 PKGBUILD 目录",
        "deep_research_linux_game_compatibility" => "Linux 游戏兼容性调查",
        "gather_linux_game_compatibility_signals" => "收集游戏兼容性",
        "register_linux_game_evidence" => "登记兼容性证据",
        "register_deep_research_topic_title" => "注册研究标题",
        "register_deep_research_reference" => "注册引用来源",
        "remove_deep_research_reference" => "移除引用来源",
        _ => name,
    };
    result.to_string()
}

pub fn clear_aur_review_state(paths: &MiyuPaths) -> anyhow::Result<()> {
    package_advisor::clear_aur_review_state(paths)
}

pub fn builtin_registry(config: &AppConfig, paths: &MiyuPaths) -> ToolRegistry {
    let mut registry = ToolRegistry::new();
    default_tools::register(&mut registry, true);
    write::register(&mut registry);
    edit_replace::register(&mut registry);
    todowrite::register(&mut registry);
    alarm::register(&mut registry, paths.clone());
    web::register_fetch(&mut registry);
    fcitx_wiki::register(&mut registry);
    weather::register(&mut registry);
    caniplayonlinux_query::register(&mut registry);
    protondb_query::register(&mut registry);
    exchange_rate::register(&mut registry, config.plugins.exchange_rate.clone());
    xuanxue::register(&mut registry);
    if config.plugins.archlinux.enabled {
        archlinux::register(&mut registry, paths);
    }
    if config.plugins.man.enabled {
        man::register(&mut registry);
    }
    moegirl::register(&mut registry);
    hash_codec::register(&mut registry);
    calculator::register(&mut registry);
    deepseek_status::register(&mut registry);
    vision::register_print(&mut registry, config.clone());
    if config.plugins.memes.enabled {
        memes::register(&mut registry, config.clone(), paths.clone());
    }
    if config.plugins.web.enabled {
        web::register(&mut registry, config.plugins.web.clone());
    }
    if config.plugins.web_images.enabled {
        web_images::register(&mut registry, config.clone(), paths.clone(), true);
    }
    if config.plugins.deep_research.enabled {
        let research_tools = registry.clone();
        deep_research::register(&mut registry, config.clone(), paths.clone(), research_tools);
    }
    if config.plugins.deep_diagnose.enabled {
        let diagnosis_tools = registry.clone();
        deep_diagnose::register(
            &mut registry,
            config.clone(),
            paths.clone(),
            diagnosis_tools,
        );
    }
    if config.plugins.vision.enabled {
        vision::register(&mut registry, config.clone(), paths.clone(), true);
    }
    if config.plugins.image_generation.enabled {
        image_generation::register(&mut registry, config.clone());
    }
    if config.plugins.knowledge_base.enabled {
        knowledge_base::register(&mut registry, config.clone(), paths.clone());
    }
    if config.plugins.package_advisor.enabled {
        package_advisor::register(&mut registry, paths.clone());
    }
    if config.plugins.deep_research_linux_game_compatibility.enabled {
        let game_tools = registry.clone();
        linux_game::register(&mut registry, config.clone(), paths.clone(), game_tools);
    }
    if config.plugins.diagnostics.enabled {
        diagnostics::register(&mut registry, config.clone());
    }
    if config.memory_config().enabled {
        memory::register(&mut registry, config.clone(), paths.clone());
    }
    let task_tools = registry.clone();
    task::register(&mut registry, config.clone(), paths.clone(), task_tools);
    scripts::register(&mut registry, paths);
    registry
}

pub fn readonly_registry(config: &AppConfig, paths: &MiyuPaths) -> ToolRegistry {
    let mut registry = ToolRegistry::new();
    default_tools::register_readonly(&mut registry);
    web::register_fetch(&mut registry);
    fcitx_wiki::register(&mut registry);
    caniplayonlinux_query::register(&mut registry);
    protondb_query::register(&mut registry);
    if config.plugins.archlinux.enabled {
        archlinux::register(&mut registry, paths);
    }
    if config.plugins.man.enabled {
        man::register(&mut registry);
    }
    if config.plugins.web.enabled {
        web::register(&mut registry, config.plugins.web.clone());
    }
    if config.plugins.web_images.enabled {
        web_images::register(&mut registry, config.clone(), paths.clone(), false);
    }
    if config.plugins.knowledge_base.enabled {
        knowledge_base::register_readonly(&mut registry, config.clone(), paths.clone());
    }
    if config.plugins.package_advisor.enabled {
        package_advisor::register(&mut registry, paths.clone());
    }
    if config.plugins.deep_research_linux_game_compatibility.enabled {
        let game_tools = registry.clone();
        linux_game::register(&mut registry, config.clone(), paths.clone(), game_tools);
    }
    if config.plugins.diagnostics.enabled {
        diagnostics::register(&mut registry, config.clone());
    }
    if config.memory_config().enabled {
        memory::register_readonly(&mut registry, config.clone(), paths.clone());
    }
    registry
}
