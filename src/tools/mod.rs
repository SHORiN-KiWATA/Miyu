mod alarm;
mod api_quota;
mod apply_patch;
mod archlinux;
mod artifact;
mod share_file;
pub use share_file::set_share_url_bases;
mod ask_question;
mod awacy_query;
mod calculator;
mod caniplayonlinux_query;
mod clipboard;
mod deep_research;
mod deepseek_status;
mod default_tools;
pub(crate) use default_tools::TOOL_SUMMARY_PREFIX;
mod diagnostics;
mod exchange_rate;
mod fcitx_wiki;
pub mod goal;
mod hash_codec;
mod html_conversion;
mod http_response;
mod image_generation;
pub mod jobs;
pub mod knowledge_base;
mod load_tools;
mod man;
mod mcp;
pub(crate) mod memes;
mod memory;
mod moegirl;
mod package_advisor;
mod patch_preview;
mod protondb_query;
mod registry;
mod scripts;
mod skills;
mod subagent_runner;
mod task;
mod todowrite;
pub(crate) use todowrite::{clear_session_todos, session_todos};
pub mod tool_descriptions;
pub(crate) mod usage_query;
pub mod vision;
mod weather;
mod web;
mod web_images;
pub mod workspace;
mod xuanxue;

use crate::agent::AgentMode;
use crate::config::AppConfig;
use crate::i18n::{is_zh, text as t};
use crate::paths::MiyuPaths;
use std::collections::HashMap;
use std::sync::RwLock;

#[allow(unused_imports)]
pub use registry::{
    empty_parameters, CommandOutputStream, GuardCtx, ToolFuture, ToolGuard, ToolPermission,
    ToolProgress, ToolProgressEvent, ToolRegistry, ToolSpec,
};
pub(crate) use scripts::rescan_scripts;
pub(crate) use skills::{apply_skill_refresh, prepare_skill_refresh};
pub use skills::{register_authoring as register_skill_authoring, register_skills};

/// 把「一串字符串」参数收成 Vec，容忍模型真会传的几种形状。
///
/// stub 加载模式下模型看到的只有一句摘要和宽松参数壳，没取契约就调用时很容
/// 易把数组写成「数组的 JSON 字符串」——实测 mimo-v2.5 在 `reference_images`
/// 上传的就是 `"[\"/path.png\"]"`。只认真数组会让这类调用**静默**失效：参数
/// 明明传了，行为却像没传，排查时要靠返回体里的计数才能发现。
///
/// 收下：真数组、单个字符串、字符串里装的 JSON 数组。空白项一律丢弃。
pub(crate) fn string_list(value: Option<&serde_json::Value>) -> Vec<String> {
    use serde_json::Value;
    let Some(value) = value else {
        return Vec::new();
    };
    match value {
        Value::Array(items) => items
            .iter()
            .filter_map(Value::as_str)
            .map(str::trim)
            .filter(|item| !item.is_empty())
            .map(str::to_string)
            .collect(),
        Value::String(text) => {
            let text = text.trim();
            if text.is_empty() {
                return Vec::new();
            }
            if text.starts_with('[') {
                if let Ok(parsed) = serde_json::from_str::<Value>(text) {
                    return string_list(Some(&parsed));
                }
            }
            vec![text.to_string()]
        }
        _ => Vec::new(),
    }
}

pub fn register_ask_question(registry: &mut ToolRegistry) {
    ask_question::register(registry);
}

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
    if let Some(skill) = name.strip_prefix("load_skill:") {
        return if is_zh() {
            format!("加载技能：{skill}")
        } else {
            format!("Load skill: {skill}")
        };
    }
    if let Some(tools) = name.strip_prefix("load_tools:") {
        let targets = tools
            .split(',')
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .collect::<Vec<_>>();
        let display = targets
            .iter()
            .map(|name| readable_load_target_name(name))
            .collect::<Vec<_>>()
            .join(if is_zh() { "、" } else { ", " });
        return if is_zh() {
            format!("加载：{display}")
        } else {
            format!("Load: {display}")
        };
    }
    if let Some(display_name) = builtin_readable_tool_name(name) {
        return display_name.to_string();
    }
    // `use_meme:search` / `task:xxx` 这类带 action 后缀的事件名，按基名取友好名。
    // 漏了这一步就一路落到最后的 `name.to_string()`，UI 上显示成裸的
    // `use_meme:search`——同一个工具有没有后缀，显示名不该差这么远。
    let base = crate::render::tool_event_base_name(name);
    if base != name {
        if let Some(display_name) = builtin_readable_tool_name(base) {
            return display_name.to_string();
        }
    }
    if let Ok(guard) = SCRIPT_DISPLAY_NAMES.read() {
        if let Some(map) = guard.as_ref() {
            if let Some(dn) = map.get(name) {
                return dn.clone();
            }
        }
    }
    name.to_string()
}

fn readable_load_target_name(name: &str) -> String {
    if let Some(group) = name.strip_prefix("group:") {
        return builtin_readable_group_name(group)
            .map(str::to_string)
            .unwrap_or_else(|| format!("group:{group}"));
    }
    readable_tool_name(name)
}

/// Phase text for the "still receiving arguments" hint, or `None` for tools
/// that stream too fast to be worth one.
///
/// The tool name is decoded from the stream well before its arguments finish,
/// so this is what keeps a multi-kilobyte patch or file write from looking
/// frozen. Deliberately a short list: flashing a hint for a `read_file` whose
/// arguments arrive in one chunk is noise. `task` is absent because subagents
/// already get their own timed block.
pub fn preparing_phase(name: &str) -> Option<&'static str> {
    Some(match name {
        "edit"
        | "artifact"
        | "kb"
        | "apply_patch"
        | "apply_artifact_patch"
        | "create_artifact"
        | "write_file"
        | "edit_file"
        | "edit_string" => t("Preparing edit", "准备编辑"),
        "run_command" => t("Preparing command", "准备执行"),
        // 批量删的参数是一整串路径,条数一多就是几百字节,正好落在
        // 「工具名已解码、参数还在流」的那个窗口里。
        "trash_path" => t("Preparing delete", "准备删除"),
        // A subagent brief is long, and its own timed block only appears once
        // the arguments have all arrived.
        "task" | "deep_research" | "claude_code" => t("Preparing task", "准备任务"),
        "ask_question" => t("Preparing question", "准备问题"),
        // 整张清单都在参数里,条目一多就是几百字节,和批量删是同一个窗口。
        "todowrite" => t("Preparing list", "准备清单"),
        _ => return None,
    })
}

/// 同一条消息里第二个及以后的工具调用用的提示。
///
/// 单看每个工具都不够"慢"到值得提示，但 N 个调用的参数是接连流完的，
/// 合起来的静默窗口和一次大 patch 一样长。此时具体是哪个工具已经不重要
/// 了——重要的是让用户知道后面还有。
pub fn batch_preparing_phase() -> &'static str {
    t("Preparing tools", "准备工具")
}

fn builtin_readable_tool_name(name: &str) -> Option<&'static str> {
    Some(match name {
        "run_command" => t("Run command", "运行命令"),
        "job" => t("Background jobs", "后台任务"),
        "edit" | "apply_patch" => t("Edit files", "编辑文件"),
        "kb" => t("Edit knowledge base", "编辑知识库"),
        "artifact" | "apply_artifact_patch" => t("Edit preview file", "修改预览文件"),
        "create_artifact" => t("Create preview file", "创建预览文件"),
        "read_artifact" => t("Read preview file", "读取预览文件"),
        "present_artifact" => t("Preview file", "预览文件"),
        "ask_question" => t("Ask user", "询问用户"),
        "task" => t("Subagent", "子代理"),
        "claude_code" => t("Claude Code", "Claude Code"),
        "read" | "read_file" => t("Read file", "读取文件"),
        "write_file" => t("Write file", "写入文件"),
        "edit_file" => t("Edit file", "编辑文件"),
        "edit_string" => t("Edit string", "字符串编辑"),
        "list_directory" => t("List directory", "列目录"),
        "create_directory" => t("Create directory", "创建目录"),
        "trash_path" => t("Move to trash", "移入回收站"),
        "glob" => t("Find files", "查找文件"),
        "grep" => t("Search text", "搜索文本"),
        "get_current_directory" => t("Current directory", "当前目录"),
        "get_current_time" => t("Current time", "当前时间"),
        "check_issue" => t("Check issue", "检查问题"),
        "check_os_info" => t("System information", "查看系统信息"),
        "read_clipboard" => t("Read clipboard", "读取剪贴板"),
        "web_search" => t("Web search", "网络搜索"),
        "web_fetch" => t("Fetch webpage", "读取网页"),
        "fcitx5_input_method_wiki_qurey" => t("Query Fcitx5 Wiki", "查询 Fcitx5 Wiki"),
        "search_web_images" => t("Search images", "搜索图片"),
        "share_file" => t("Share file", "分享文件"),
        "analyze_image" | "vision_analyze" => t("Visual analysis", "视觉分析"),
        "print_image" => t("Display image", "显示图片"),
        "generate_image" => t("Generate image", "生成图片"),
        "use_meme" => t("Meme", "表情包"),
        "manage_meme" => t("Manage memes", "管理表情包"),
        "deep_research" => t("Deep research", "深度研究"),
        "upload_knowledge_base_file" | "upload_text_to_knowledge_base" => {
            t("Import knowledge base", "导入知识库")
        }
        "read_knowledge_base_file" => t("Read knowledge base", "读取知识库"),
        "search_knowledge_base" => t("Search knowledge base", "搜索知识库"),
        "edit_knowledge_base_file" => t("Edit knowledge base", "编辑知识库"),
        "remove_knowledge_base_file" => t("Remove from knowledge base", "移除知识库"),
        "list_knowledge_base_files" => t("List knowledge base", "列出知识库"),
        "alarm" => t("Alarms", "闹钟"),
        "remember_fact" => t("Remember fact", "记录记忆"),
        "search_evicted_context" => t("Search old context", "搜索旧上下文"),
        "recall_memory" | "recall_memories" => t("Recall memories", "召回记忆"),
        "forget_memory" | "forget_memories" => t("Forget memories", "删除记忆"),
        "list_memory" | "list_memories" => t("List memories", "列出记忆"),
        "aur" => t("AUR query", "AUR 查询"),
        "archlinux_official_package_query" => t("Query Arch package", "查询 Arch 官方包"),
        "query_deepseek_status" => t("Check DeepSeek status", "查询 DeepSeek 状态"),
        "query_api_quota" => t("Query API quota", "查询大模型 API 额度"),
        "pacman_search" => t("Search packages", "搜索软件包"),
        "archwiki_query" => t("Query ArchWiki", "查询 ArchWiki"),
        "archlinux_news" => t("Arch news", "Arch 新闻"),
        "online_man" => t("Online manual", "在线手册"),
        "moegirl_query" | "query_moegirl" => t("Query Moegirlpedia", "查询萌娘百科"),
        "calculate" | "calculator" | "scientific_calculator" => {
            t("Scientific calculation", "科学计算")
        }
        "codec" => t("Encode/decode", "编解码"),
        "exchange_rate" | "get_exchange_rate" => t("Exchange rates", "汇率查询"),
        "weather" | "get_weather" => t("Weather", "天气查询"),
        "game_compat" => t("Game compatibility", "游戏兼容性"),
        "divine" => t("Divination", "占卜"),
        "divine:zhouyi" => t("I Ching", "六十四卦"),
        "divine:tarot" => t("Tarot", "塔罗牌"),
        "divine:fortune" => t("Fortune", "吉凶占"),
        "divine:dice" => t("Dice roll", "掷骰子"),
        "load_skill" => t("Load skill", "加载技能"),
        "manage_skill" => t("Manage skills", "管理技能"),
        "load_tools" => t("Load", "加载"),
        "manage_script" => t("Manage scripts", "管理脚本"),
        "todowrite" => t("Todo list", "任务列表"),
        "get_goal" => t("Goal", "读取目标"),
        "create_goal" => t("Set goal", "创建目标"),
        "update_goal" => t("Update goal", "更新目标"),
        "review_aur_package" => t("Review AUR package", "审查 AUR 包"),
        "install_aur_package" => t("Install AUR package", "安装 AUR 包"),
        "review_pkgbuild_directory" => t("Review PKGBUILD directory", "审查 PKGBUILD 目录"),
        "register_deep_research_topic_title" => t("Register research title", "注册研究标题"),
        "register_deep_research_reference" => t("Register reference", "注册引用来源"),
        "remove_deep_research_reference" => t("Remove reference", "移除引用来源"),
        _ => return None,
    })
}

fn builtin_readable_group_name(group: &str) -> Option<&'static str> {
    Some(match group {
        "acg" => t("ACG tools", "ACG 工具组"),
        "agent" => t("Subagent tools", "子代理工具组"),
        "alarms" => t("Alarm tools", "闹钟工具组"),
        "arch" => t("Arch / AUR tools", "Arch / AUR 工具组"),
        "dev" => t("Development tools", "开发修改工具组"),
        "dev-read" => t("Code search tools", "代码检索工具组"),
        "diagnostics" => t("Diagnostic tools", "诊断工具组"),
        "divination" => t("Divination tools", "玄学工具组"),
        "gaming" => t("Gaming tools", "游戏工具组"),
        "images" => t("Image tools", "图片工具组"),
        "knowledge" => t("Knowledge base tools", "知识库工具组"),
        "knowledge-admin" => t("Knowledge base management", "知识库管理工具组"),
        "linux-docs" => t("Linux documentation", "Linux 文档工具组"),
        "memory" => t("Memory tools", "记忆工具组"),
        "memes" => t("Meme tools", "表情包工具组"),
        "planning" => t("Planning tools", "任务规划工具组"),
        "research" => t("Research tools", "深度研究工具组"),
        "scripts" => t("Script tools", "脚本工具组"),
        "scripting" => t("Script management", "脚本管理工具组"),
        "shell" => t("Shell tools", "Shell 工具组"),
        "shopping" => t("Shopping tools", "购物工具组"),
        "skills" => t("Skill tools", "技能工具组"),
        "systeminfo" => t("System information", "系统信息工具组"),
        "utility" => t("Utility tools", "实用工具组"),
        "web" => t("Web tools", "联网工具组"),
        _ => return None,
    })
}

pub fn clear_aur_review_state(paths: &MiyuPaths) -> anyhow::Result<()> {
    package_advisor::clear_aur_review_state(paths)
}

/// AUR 装包互斥:review 与 install 不同轮,逼一次"给用户看过再装"的确认。
/// 原为 chat_with_tools 循环里的硬编码特判,迁入 guard 层后对所有分发路径
/// (主循环/子代理/工具桥)一致生效。
pub(crate) fn aur_review_install_guard() -> ToolGuard {
    std::sync::Arc::new(|tool, _args, ctx| {
        (tool.name == "install_aur_package"
            && ctx
                .used_tools
                .iter()
                .any(|name| name == "review_aur_package"))
        .then(|| {
            "install_aur_package cannot run in the same turn as review_aur_package; \
             ask the user to confirm installation first"
                .to_string()
        })
    })
}

/// run_command 命令拒绝子串(config.tools.command_deny)。命中即拒,
/// 防提示注入与模型手滑;拒绝以 tool error 回给模型,轮次存活。
pub(crate) fn command_deny_guard(patterns: Vec<String>) -> ToolGuard {
    std::sync::Arc::new(move |tool, args, _ctx| {
        if tool.name != "run_command" {
            return None;
        }
        let command = args.get("command").and_then(serde_json::Value::as_str)?;
        patterns
            .iter()
            .find(|pattern| !pattern.is_empty() && command.contains(pattern.as_str()))
            .map(|pattern| {
                format!("command contains the denied pattern `{pattern}` and was rejected")
            })
    })
}

fn install_builtin_guards(registry: &mut ToolRegistry, config: &AppConfig) {
    registry.add_guard(aur_review_install_guard());
    registry.add_guard(command_deny_guard(config.tools.command_deny.clone()));
}

pub fn builtin_registry(config: &AppConfig, paths: &MiyuPaths) -> ToolRegistry {
    let mut registry = ToolRegistry::new();
    registry.set_default_timeout_secs(config.tools.default_timeout_secs);
    install_builtin_guards(&mut registry, config);
    default_tools::register(
        &mut registry,
        config.skills.allow_command_execution,
        config,
        paths,
    );
    jobs::register_management(&mut registry);
    usage_query::register(
        &mut registry,
        paths.state_dir.join("usage-history.jsonl"),
        config.clone(),
    );
    // 编辑器只留 apply_patch(聚合增/改/删,diff 渲染载体);write_file/
    // edit_file/edit_string 与 dev 同步退场(验收四轮:normal 也去冗余)。
    apply_patch::register(&mut registry);
    todowrite::register(&mut registry, paths.clone());
    goal::register(&mut registry, paths.clone());
    alarm::register(&mut registry, paths.clone());
    clipboard::register(&mut registry, paths.clone());
    web::register_fetch(&mut registry);
    fcitx_wiki::register(&mut registry);
    weather::register(&mut registry);
    protondb_query::register(&mut registry, paths.clone());
    // 插件关就不注册:关掉的插件仍然常驻一份完整契约,是三个面都白背的
    // 纯浪费(08-17 实测 get_exchange_rate 311 字符)。
    if config.plugins.exchange_rate.enabled {
        exchange_rate::register(&mut registry, config.plugins.exchange_rate.clone());
    }
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
    if config.plugins.api_quota.enabled {
        api_quota::register(&mut registry, config.plugins.api_quota.clone());
    }
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
    if config.plugins.vision.enabled {
        vision::register(&mut registry, config.clone(), paths.clone(), true);
    }
    if config.plugins.image_generation.enabled {
        image_generation::register(&mut registry, config.clone(), paths.clone());
    }
    if config.plugins.knowledge_base.enabled {
        knowledge_base::register(&mut registry, config.clone(), paths.clone());
    }
    if config.plugins.package_advisor.enabled {
        package_advisor::register(&mut registry, paths.clone());
    }
    if config.plugins.diagnostics.enabled {
        diagnostics::register(&mut registry, config.clone());
    }
    if config.memory_config().enabled {
        memory::register(&mut registry, config.clone(), paths.clone());
    }
    // claude_code 委托工具已移除(08-21 用户裁定);中转供应商那条线不受影响。
    let task_tools = registry.clone();
    task::register(&mut registry, config.clone(), paths.clone(), task_tools);
    scripts::register(&mut registry, config, paths);
    if config.mcp.enabled {
        mcp::register(&mut registry, config.clone());
    }
    // load_tools 常驻注册(09-01):full 模式下调用它无害(返回契约文本),
    // 而会话中途从需加载模型切到完整模型时,历史里的 load_tools 调用记录
    // 必须仍然可执行,否则模型模仿历史会撞未知工具。
    load_tools::register(&mut registry);
    registry
}

pub fn register_webui_artifact_tools(
    registry: &mut ToolRegistry,
    paths: &MiyuPaths,
    session_id: &str,
) {
    artifact::register_webui(registry, paths, session_id);
}

/// WebUI 文件分享工具。与 artifact 演示区解耦，单独注册。
pub fn register_webui_share_tools(
    registry: &mut ToolRegistry,
    config: &AppConfig,
    store: crate::state::StateStore,
) {
    share_file::register_webui(registry, config, store);
}

pub fn webui_artifact_manifest(paths: &MiyuPaths, session_id: &str) -> anyhow::Result<String> {
    artifact::managed_manifest(paths, session_id)
}

pub(crate) fn rescope_platform_memory_tools(
    registry: &mut ToolRegistry,
    config: &AppConfig,
    paths: &MiyuPaths,
    context: &dyn crate::platform_types::PlatformToolContext,
    readonly: bool,
) {
    if !config.tools.enabled || !config.memory_config().enabled {
        return;
    }
    for name in ["remember_fact", "search_evicted_context", "recall_memories"] {
        registry.unregister(name);
    }
    let principal = context.principal().stable_key();
    let access = if context.is_admin() {
        crate::memory::MemoryAccess::Privileged
    } else {
        crate::memory::MemoryAccess::principal(principal.clone())
    };
    if readonly {
        memory::register_readonly_with_context(
            registry,
            config.clone(),
            paths.clone(),
            access,
            Some(principal),
            context.sender_display_name(),
        );
    } else {
        memory::register_with_context(
            registry,
            config.clone(),
            paths.clone(),
            access,
            Some(principal),
            context.sender_display_name(),
        );
    }
}

/// Stub loading mode (v7 §八点七): every lazy tool stays registered as a
/// permanently visible stub (real name + one-line summary + permissive
/// parameter shell), so the provider-visible tools array is byte-constant for
/// the whole session; full contracts are fetched on demand through
/// `load_tools` as a tool result that rides the conversation tail.
///
/// "hybrid"/"lazy"(按已加载集合增长声明数组的旧档)09-01 删除,历史配置值
/// 按「需加载」处理——它们同属懒加载家族,悄悄升成 full 会让旧配置的工具面
/// 字节数翻好几倍。
pub fn is_stub_loading_mode(mode: &str) -> bool {
    matches!(mode.trim(), "stub" | "hybrid" | "lazy")
}

/// 本次请求的有效工具加载模式,按候选模型池解析。
///
/// 单成员规则:模型级覆盖(`provider.model_tools_loading_mode`)优先,缺项回退
/// 全局 `tools.loading_mode`。池级规则:任一成员要求 full 则整池 full——
/// 一次请求只有一张工具面,而命中哪个模型(以及故障转移换给谁)是发送时才
/// 决定的,这张脸必须让池里任何成员都能用;full 全兼容,stub 只是省 token 的
/// 优化,「就高不就低」恒安全。多模态池并入候选:带图回合可能路由过去,
/// 工具面跟着请求走。
///
/// 背景(09-01):约束解码型供应商(实测 bigmodel glm-5.3-flash)把工具参数
/// 生成硬限制在声明 schema 内,空壳 stub 让它永远只能发 `{}`,契约文本在
/// 对话里也救不回(裸 API 8/8 复现)。给这类模型配模型级 full,其余照旧 stub。
pub fn effective_tools_loading_mode(config: &AppConfig) -> String {
    let canonical = |mode: &str| {
        if mode.trim() == "full" {
            "full"
        } else {
            "stub"
        }
    };
    let global = canonical(&config.tools.loading_mode);
    let candidates = config
        .active_provider_models
        .iter()
        .flatten()
        .chain(config.active_multimodal_provider_models.iter().flatten());
    let mut any = false;
    for entry in candidates {
        any = true;
        let mode = config
            .providers
            .iter()
            .find(|provider| provider.id == entry.provider_id)
            .and_then(|provider| provider.model_tools_loading_mode.get(&entry.model))
            .map(|mode| canonical(mode))
            .unwrap_or(global);
        if mode == "full" {
            return "full".to_string();
        }
    }
    if any { "stub" } else { global }.to_string()
}

/// Build/Dev 模式工具目录:极简开发形态,"模型可见表面积最小化"。
/// 只注册:run_command+后台任务管理、apply_patch(唯一编辑器,聚合
/// 增/改/删文件,存在意义=diff 渲染)、todo、goal、task 子代理、web
/// 检索与 MCP。读检索(read_file/glob/grep)、check_os_info、
/// trash_path、知识库、多余编辑器都不注册——bash+coreutils 干得更好
/// (验收三轮裁剪)。人格/娱乐/平台类一概不存在。记忆工具**注册**,
/// 但作用域切到保留人格 "dev" 的独立命名空间(`dev_scoped`)。
/// ask_question 等界面胶水与 normal 同路,由 daemon/CLI 按 surface 追加。
pub fn dev_registry(config: &AppConfig, paths: &MiyuPaths) -> ToolRegistry {
    let mut registry = ToolRegistry::new();
    registry.set_default_timeout_secs(config.tools.default_timeout_secs);
    install_builtin_guards(&mut registry, config);
    // 验收三轮裁剪定稿:凡 coreutils 干得更好的都不注册——读检索
    // (read_file/glob/grep)、check_os_info、trash_path、知识库全砍;
    // 编辑只留 apply_patch(Add/Update/Delete File 全聚合,留它是为了
    // diff 渲染),write_file/edit_file/edit_string 一并退场。
    default_tools::register_run_command(&mut registry, config.skills.allow_command_execution);
    jobs::register_management(&mut registry);
    apply_patch::register(&mut registry);
    todowrite::register(&mut registry, paths.clone());
    goal::register(&mut registry, paths.clone());
    web::register_fetch(&mut registry);
    if config.plugins.web.enabled {
        web::register(&mut registry, config.plugins.web.clone());
    }
    if config.plugins.vision.enabled {
        // 看图对 coding 是刚需(UI 截图排错、设计稿、测试产出的图表);
        // 聊天模型不带眼睛时由 vision 插件路由给专用视觉模型。
        vision::register(&mut registry, config.clone(), paths.clone(), true);
    }
    if config.memory_config().enabled {
        // dev 独立记忆:同一套工具,库切到保留人格 "dev" 的命名空间,
        // 与默认人格的记忆互不可见(验收问题一:开发模式也要有记忆)。
        memory::register(&mut registry, config.dev_scoped(), paths.clone());
    }
    let task_tools = registry.clone();
    task::register(&mut registry, config.clone(), paths.clone(), task_tools);
    if config.mcp.enabled {
        mcp::register(&mut registry, config.clone());
    }
    // load_tools 常驻注册(09-01):full 模式下调用它无害(返回契约文本),
    // 而会话中途从需加载模型切到完整模型时,历史里的 load_tools 调用记录
    // 必须仍然可执行,否则模型模仿历史会撞未知工具。
    load_tools::register(&mut registry);
    registry
}

/// Tools exposed to an untrusted messaging-platform conversation. This list
/// deliberately excludes shell, filesystem, local-image inspection, memory,
/// knowledge-base, MCP, scripts, and tools that persist arbitrary downloads.
/// `generate_image` is the one Writes exception: it only saves its own API
/// output under the plugin's output directory, never an arbitrary host path.
pub fn restricted_platform_registry(config: &AppConfig, paths: &MiyuPaths) -> ToolRegistry {
    let mut registry = ToolRegistry::new();
    registry.set_default_timeout_secs(config.tools.default_timeout_secs);
    web::register_fetch(&mut registry);
    weather::register(&mut registry);
    protondb_query::register(&mut registry, paths.clone());
    // 插件关就不注册:关掉的插件仍然常驻一份完整契约,是三个面都白背的
    // 纯浪费(08-17 实测 get_exchange_rate 311 字符)。
    if config.plugins.exchange_rate.enabled {
        exchange_rate::register(&mut registry, config.plugins.exchange_rate.clone());
    }
    xuanxue::register(&mut registry);
    moegirl::register(&mut registry);
    hash_codec::register(&mut registry);
    calculator::register(&mut registry);
    deepseek_status::register(&mut registry);
    if config.plugins.web.enabled {
        web::register(&mut registry, config.plugins.web.clone());
    }
    if config.plugins.memes.enabled {
        memes::register_chat(&mut registry, config.clone(), paths.clone());
    }
    if config.plugins.image_generation.enabled {
        image_generation::register(&mut registry, config.clone(), paths.clone());
        // 静态英文追加,所有平台会话字节一致,不影响本地注册表的描述。
        registry.amend_description(
            "generate_image",
            " In messaging-platform conversations at most one image is generated per user request; the limit is enforced automatically.",
        );
    }
    if config.skills.enabled {
        if let Err(error) = skills::register_skills(&mut registry, config, paths) {
            tracing::warn!(error = %error, "failed to register skills for restricted platform registry");
        }
    }
    // load_tools 常驻注册(09-01):full 模式下调用它无害(返回契约文本),
    // 而会话中途从需加载模型切到完整模型时,历史里的 load_tools 调用记录
    // 必须仍然可执行,否则模型模仿历史会撞未知工具。
    load_tools::register(&mut registry);
    registry
}

/// 按模式与配置组装工具注册表：REPL、daemon、WebUI、子代理都从这里拿。
///
/// 组装顺序有意义，不是随手排的：
///
/// 1. 先按模式选底座（`normal` 面向日常对话，`dev` 面向写代码），工具总开关
///    关掉时给一个空注册表而不是提前返回——调用方拿到的永远是同一个类型。
/// 2. 技能只在工具开着时注册；技能创作工具（`manage_skill`）只在 normal 模式
///    出现，dev 模式下模型该写代码不该写技能。
/// 3. `ask_question` 单独由调用方决定：daemon 与 WebUI 能弹面板，一次性
///    `miyu ask` 不能，所以它是参数而不是模式的函数。
/// 4. 最后登记脚本工具的显示名——这一步要在所有注册之后，否则新注册的脚本
///    在渲染层会显示成原始工具名。
///
/// 这个函数原本长在 `cli.rs` 里，于是 `web` 和 `tools` 都得反过来
/// `use crate::cli`，把两个底层模块钉死在最上层。它实际只依赖
/// tools/config/paths/agent，与 CLI 毫无关系，所以下沉到这里——拆分要断的
/// 两条边（`web→cli`、`tools→cli`）一次都断掉。
pub(crate) fn build_tool_registry(
    config: &AppConfig,
    paths: &MiyuPaths,
    mode: AgentMode,
    interactive_questions: bool,
) -> anyhow::Result<ToolRegistry> {
    let mut registry = if config.tools.enabled {
        match mode {
            AgentMode::Normal => builtin_registry(config, paths),
            AgentMode::Dev => dev_registry(config, paths),
        }
    } else {
        ToolRegistry::new()
    };
    if config.tools.enabled && config.skills.enabled {
        register_skills(&mut registry, config, paths)?;
        if mode == AgentMode::Normal {
            register_skill_authoring(&mut registry, config.clone(), paths.clone());
        }
    }
    if config.tools.enabled && interactive_questions {
        register_ask_question(&mut registry);
    }
    register_script_display_names(&registry);
    Ok(registry)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 数组参数要容忍模型真会传的形状。线上实测:mimo-v2.5 把
    /// `reference_images` 传成了 `"[\"/path.png\"]"`——一个被 JSON 编码成
    /// 字符串的数组。只认真数组会让 job_ids / user_ids / tags / groups 这类
    /// 参数一起静默失效,踢人工具取不到目标尤其危险。
    #[test]
    fn string_list_accepts_the_shapes_models_actually_send() {
        use serde_json::json;
        let one = vec!["a".to_string()];
        assert_eq!(string_list(Some(&json!(["a"]))), one);
        assert_eq!(string_list(Some(&json!("a"))), one);
        assert_eq!(string_list(Some(&json!(r#"["a"]"#))), one);
        assert_eq!(
            string_list(Some(&json!(["a", " b ", "", "  "]))),
            vec!["a".to_string(), "b".to_string()]
        );
        assert!(string_list(None).is_empty());
        assert!(string_list(Some(&json!([]))).is_empty());
        assert!(string_list(Some(&json!(""))).is_empty());
        assert!(string_list(Some(&json!(null))).is_empty());
        // 解不开的字符串按单条路径收下,不当成数组硬猜。
        assert_eq!(
            string_list(Some(&json!("[not json"))),
            vec!["[not json".to_string()]
        );
    }

    /// 有效加载模式按候选池取最保守:任一成员要 full 则整池 full——一次请求
    /// 只有一张工具面,命中与故障转移都在发送时才定,这张脸必须全员可用。
    /// 约束解码型模型(实测 bigmodel glm-5.3-flash)吃不下空壳 stub,是模型级
    /// full 覆盖存在的理由(09-01)。
    #[test]
    fn effective_loading_mode_takes_the_most_conservative_pool_member() {
        use crate::config::ActiveProviderModelConfig;
        let mut config = AppConfig::default();
        config.tools.loading_mode = "stub".to_string();
        let provider_id = config.providers[0].id.clone();
        let pick = |model: &str| ActiveProviderModelConfig {
            provider_id: provider_id.clone(),
            model: model.to_string(),
        };

        // 空池回退全局。
        config.active_provider_models = None;
        assert_eq!(effective_tools_loading_mode(&config), "stub");

        // 全员跟随全局(需加载)。
        config.active_provider_models = Some(vec![pick("lenient-a"), pick("lenient-b")]);
        assert_eq!(effective_tools_loading_mode(&config), "stub");

        // 混进一个模型级 full,整池升 full。
        config.providers[0]
            .model_tools_loading_mode
            .insert("locked".to_string(), "full".to_string());
        config
            .active_provider_models
            .as_mut()
            .unwrap()
            .push(pick("locked"));
        assert_eq!(effective_tools_loading_mode(&config), "full");

        // 多模态池同样参与候选(带图回合可能路由过去,工具面跟请求走)。
        config.active_provider_models = Some(vec![pick("lenient-a")]);
        config.active_multimodal_provider_models = Some(vec![pick("locked")]);
        assert_eq!(effective_tools_loading_mode(&config), "full");
        config.active_multimodal_provider_models = None;

        // 模型级覆盖压过全局:全局 full,钉死的单模型显式需加载 → stub。
        config.tools.loading_mode = "full".to_string();
        config.providers[0]
            .model_tools_loading_mode
            .insert("thrifty".to_string(), "stub".to_string());
        config.active_provider_models = Some(vec![pick("thrifty")]);
        assert_eq!(effective_tools_loading_mode(&config), "stub");

        // 已删档的 hybrid/lazy 旧值按需加载处理,不悄悄升 full。
        config.tools.loading_mode = "hybrid".to_string();
        config.active_provider_models = Some(vec![pick("lenient-a")]);
        assert_eq!(effective_tools_loading_mode(&config), "stub");
        assert!(is_stub_loading_mode("hybrid"));
        assert!(is_stub_loading_mode("lazy"));
    }

    /// 回归:dev 模式要有看图(vision_analyze),且随 vision 插件开关走。
    #[test]
    fn dev_registry_vision_follows_plugin_switch() {
        let paths = crate::paths::MiyuPaths::new().unwrap();
        let mut config = crate::config::AppConfig::default();
        let names = |registry: &ToolRegistry| -> Vec<String> {
            registry
                .definitions()
                .iter()
                .map(|d| d.function.name.clone())
                .collect()
        };
        assert!(names(&dev_registry(&config, &paths)).contains(&"vision_analyze".to_string()));
        config.plugins.vision.enabled = false;
        assert!(!names(&dev_registry(&config, &paths)).contains(&"vision_analyze".to_string()));
    }

    pub(super) fn test_paths(root: &std::path::Path) -> MiyuPaths {
        MiyuPaths {
            root_dir: root.to_path_buf(),
            config_dir: root.join("config"),
            config_file: root.join("config/config.jsonc"),
            skills_dir: root.join("config/skills"),
            data_dir: root.join("data"),
            cache_dir: root.join("cache"),
            state_dir: root.join("state"),
            pictures_dir: root.join("pictures"),
            fish_hook_file: root.join("config/fish/conf.d/miyu.fish"),
            bash_hook_file: root.join("config/shell/bash-hook.sh"),
            zsh_hook_file: root.join("config/shell/zsh-hook.zsh"),
            scripts_dir: root.join("config/scripts"),
            system_scripts_dir: root.join("system-scripts"),
        }
    }

    #[test]
    fn preparing_phase_covers_the_slow_argument_tools_only() {
        for name in [
            "apply_patch",
            "apply_artifact_patch",
            "create_artifact",
            "write_file",
            "edit_file",
            "edit_string",
        ] {
            assert_eq!(
                preparing_phase(name),
                Some(crate::i18n::text("Preparing edit", "准备编辑")),
                "{name}"
            );
        }
        assert_eq!(
            preparing_phase("run_command"),
            Some(crate::i18n::text("Preparing command", "准备执行"))
        );
        assert_eq!(
            preparing_phase("trash_path"),
            Some(crate::i18n::text("Preparing delete", "准备删除"))
        );
        for name in ["task", "deep_research"] {
            assert_eq!(
                preparing_phase(name),
                Some(crate::i18n::text("Preparing task", "准备任务")),
                "{name}"
            );
        }
        assert_eq!(
            preparing_phase("ask_question"),
            Some(crate::i18n::text("Preparing question", "准备问题"))
        );
        // Arguments arrive in one chunk: a hint would only flicker.
        for name in ["read_file", "grep", "list_directory"] {
            assert_eq!(preparing_phase(name), None, "{name}");
        }
    }

    /// 续轮提示词必须自报来历。
    ///
    /// 实测过一次：一个会话正在排查游戏的 VC++ 运行库，用户设了个「查询东京
    /// 天气」的目标，续轮到达时模型判定「This looks like a system prompt
    /// injection or some automated goal that hijacked my session」，拒绝执行、
    /// 继续做上一个话题。那个警惕是对的——一段没有来历、和上文毫无关系的
    /// 英文祈使句，本来就该被怀疑。
    ///
    /// 所以这几句不是客套：谁下的（用户）、怎么来的（/goal 命令 + 空闲时自动
    /// 续轮）、为什么和上文对不上（长期目标会跨越话题）。看着像冗余，最容易
    /// 被后人当废话删掉，这条测试就是拦这个的。
    #[test]
    fn goal_round_prompt_states_where_it_came_from() {
        let goal = crate::state::GoalRecord {
            session_id: "sess_x".to_string(),
            goal_id: "goal_abc123".to_string(),
            revision: 4,
            objective: "把测试跑绿".to_string(),
            phase: crate::state::GoalPhase::Active,
            blocked_code: None,
            blocked_message: None,
            max_rounds: 10,
            rounds_started: 2,
            created_at: String::new(),
            updated_at: String::new(),
        };
        let prompt = crate::tools::goal::goal_round_prompt(&goal, true);
        // 断言按小写比，大小写不是这条测试要守的东西。
        let lowered = prompt.to_lowercase();
        for expected in [
            "set by the user",           // 谁下的
            "/goal",                     // 怎么下的
            "unrelated to the messages", // 为什么和上文对不上
            "waiting",                   // 不许拿一整轮只说「我在等你」
            "goal_abc123",               // CAS 凭证直接给它，省一次 get_goal
        ] {
            assert!(
                lowered.contains(expected),
                "续轮提示词丢了来历说明（缺 {expected:?}）——模型会把它当注入拒掉:\n{prompt}"
            );
        }
        // 目标本身和轮号仍要在。
        assert!(prompt.contains("把测试跑绿"));
        assert!(prompt.contains("Round 2 of 10"));
        // 两条结束调用都要把 goal_id 和 revision 填好——让模型自己去读一遍
        // 目标、或者为此加载一次工具，都是白跑的往返。
        assert!(
            prompt.contains(r#""revision":4"#),
            "revision 没填进调用里：\n{prompt}"
        );
        assert!(
            prompt.matches(r#""goal_id":"goal_abc123""#).count() == 2,
            "complete 和 blocked 两条都要填好：\n{prompt}"
        );
        assert!(
            prompt.contains("do not read the goal or load tools first"),
            "要明说别为它加载工具，否则模型会先失败一次再去加载：\n{prompt}"
        );

        // 第二轮起发短版，但短版必须**自包含**：一行来历 + 目标全文 + 两条
        // 填好的调用。早先短版只说「same objective and rules as above」，赌
        // 完整版还躺在上下文里——压缩会把这个赌注折掉，目标被人改过它又指向
        // 旧文案，为此还得维护一套「下轮重发完整版」的脏标记。
        let short = crate::tools::goal::goal_round_prompt(&goal, false);
        assert!(short.contains("Round 2 of 10"));
        assert!(
            short.contains("set by the user") && short.contains("把测试跑绿"),
            "短版丢了来历或目标全文——压缩/编辑之后它就指向空气：\n{short}"
        );
        assert!(
            short.contains(r#""revision":4"#) && short.matches("update_goal").count() == 2,
            "短版仍要带两条填好的调用——revision 每轮可能变，不该让模型去回忆：\n{short}"
        );
        // 仍要比完整版短：短版逐轮追加，长散文只该在第一轮出现一次。
        assert!(
            short.len() < prompt.len(),
            "短版没短下来（{} vs {}）：\n{short}",
            short.len(),
            prompt.len()
        );
    }

    #[test]
    fn readable_names_cover_all_built_in_tools_and_groups() {
        let mut missing_tools = tool_descriptions::all()
            .keys()
            .filter(|name| builtin_readable_tool_name(name).is_none())
            .cloned()
            .collect::<Vec<_>>();
        missing_tools.sort();
        assert!(
            missing_tools.is_empty(),
            "missing tool names: {missing_tools:?}"
        );

        let mut missing_groups = tool_descriptions::group_names()
            .into_iter()
            .filter(|group| builtin_readable_group_name(group).is_none())
            .collect::<Vec<_>>();
        missing_groups.sort();
        assert!(
            missing_groups.is_empty(),
            "missing tool group names: {missing_groups:?}"
        );
    }

    #[test]
    fn ui_language_does_not_change_agent_tool_definitions() {
        let temp = tempfile::tempdir().unwrap();
        let paths = test_paths(temp.path());
        let mut english = AppConfig::default();
        english.display.language = "en".to_string();
        let mut chinese = english.clone();
        chinese.display.language = "zh".to_string();

        let english =
            serde_json::to_value(builtin_registry(&english, &paths).definitions()).unwrap();
        let chinese =
            serde_json::to_value(builtin_registry(&chinese, &paths).definitions()).unwrap();

        assert_eq!(english, chinese);
    }

    #[test]
    fn restricted_platform_registry_has_no_host_or_write_tools() {
        let temp = tempfile::tempdir().unwrap();
        let paths = test_paths(temp.path());
        let registry = restricted_platform_registry(&AppConfig::default(), &paths);
        let names = registry.tool_names();

        for forbidden in [
            "run_command",
            "read_file",
            "write_file",
            "apply_patch",
            "vision_analyze",
            "task",
        ] {
            assert!(!names.iter().any(|name| name == forbidden), "{forbidden}");
        }
        for name in names {
            // 两个明示的 Writes 例外：都只写自己插件的目录，碰不到主机文件。
            // generate_image 写图片输出目录；manage_meme 写人格的表情库。
            if name == "generate_image" || name == "manage_meme" {
                continue;
            }
            assert_eq!(
                registry.permission(&name).unwrap(),
                ToolPermission::ReadOnly
            );
        }
        assert!(registry.contains("load_skill"));
        assert!(registry.contains("load_tools"));
        // With the plugin enabled, image generation is exposed to platforms.
        let mut config = AppConfig::default();
        config.plugins.image_generation.enabled = true;
        let registry = restricted_platform_registry(&config, &paths);
        assert!(registry.contains("generate_image"));
        let visible = registry.lazy_definitions(&Default::default());
        assert!(visible
            .iter()
            .any(|definition| definition.function.name == "load_tools"));
        assert!(!visible
            .iter()
            .any(|definition| definition.function.name == "divine"));
    }

    #[test]
    fn skill_authoring_tools_are_normal_mode_only() {
        let temp = tempfile::tempdir().unwrap();
        let paths = test_paths(temp.path());
        let config = AppConfig::default();
        let normal =
            build_tool_registry(&config, &paths, crate::agent::AgentMode::Normal, false).unwrap();
        let dev =
            build_tool_registry(&config, &paths, crate::agent::AgentMode::Dev, false).unwrap();

        assert!(normal.contains("manage_skill"));
        assert!(!dev.contains("manage_skill"));
        assert!(normal.contains("load_skill"));
        assert!(dev.contains("load_skill"));
    }

    #[test]
    fn artifact_tools_are_only_added_by_the_webui_registration_step() {
        let temp = tempfile::tempdir().unwrap();
        let paths = test_paths(temp.path());
        let config = AppConfig::default();
        let mut registry = builtin_registry(&config, &paths);
        assert!(!registry.contains("present_artifact"));

        // Edit/Read 统一后 WebUI 附加的只剩发布动作;创建/读取/打补丁走
        // edit/read 的 artifact: 命名空间。
        register_webui_artifact_tools(&mut registry, &paths, "sess_webui");
        assert_eq!(
            registry.permission("present_artifact").unwrap(),
            ToolPermission::Presentation,
        );
        let definitions = registry.definitions();
        assert!(definitions
            .iter()
            .any(|definition| definition.function.name == "present_artifact"));
        assert!(!definitions
            .iter()
            .any(|definition| definition.function.name == "create_artifact"));
    }

    #[tokio::test]
    async fn restricted_platform_can_load_the_divination_group() {
        let temp = tempfile::tempdir().unwrap();
        let paths = test_paths(temp.path());
        let registry = restricted_platform_registry(&AppConfig::default(), &paths);

        let output = registry
            .call("load_tools", r#"{"names":["group:divination"]}"#)
            .await
            .unwrap();
        let loaded = output
            .lines()
            .find_map(|line| line.strip_prefix("loaded_tools:"))
            .expect("loaded_tools line");
        assert!(loaded.split(',').any(|name| name.trim() == "divine"));
    }
}

#[cfg(test)]
mod tier_schema_probe {
    /// Regression: the built-in description overlay
    /// (`descriptions/task.json`) wholesale replaces the task schema at
    /// register time — a param added only in code silently vanishes from
    /// what the LLM sees.
    #[test]
    fn task_definition_includes_tier() {
        let config = crate::config::AppConfig::default();
        let paths = crate::paths::MiyuPaths::new().unwrap();
        let registry = super::builtin_registry(&config, &paths);
        let defs = registry.definitions();
        let task = defs
            .iter()
            .find(|d| d.function.name == "task")
            .expect("task registered");
        let props = task.function.parameters.get("properties").unwrap();
        assert!(
            props.get("tier").is_some(),
            "tier missing: {}",
            task.function.parameters
        );
        // 全未配置=零追加(08-16 tools 瘦身:三行"未配置"是零信息,还把
        // 动态文本焊进 tools 数组);配置了档位才出现状态。
        assert!(!task.function.description.contains("cheap=["));
    }

    /// The description suffix lists the concrete models per tier pool.
    #[test]
    fn task_description_lists_configured_tier_models() {
        let mut config = crate::config::AppConfig::default();
        let provider_id = config.providers[0].id.clone();
        config.providers[0].models.push("mini-a".to_string());
        config.providers[0].models.push("mini-b".to_string());
        config
            .toggle_subagent_tier_model(crate::config::ModelTier::Cheap, &provider_id, "mini-a")
            .unwrap();
        config
            .toggle_subagent_tier_model(crate::config::ModelTier::Balanced, &provider_id, "mini-a")
            .unwrap();
        config
            .toggle_subagent_tier_model(crate::config::ModelTier::Balanced, &provider_id, "mini-b")
            .unwrap();
        let paths = crate::paths::MiyuPaths::new().unwrap();
        let registry = super::builtin_registry(&config, &paths);
        let defs = registry.definitions();
        let task = defs.iter().find(|d| d.function.name == "task").unwrap();
        assert!(task.function.description.contains("cheap=[mini-a]"));
        assert!(task
            .function
            .description
            .contains("balanced=[mini-a, mini-b]"));
        assert!(task.function.description.contains("strong=["));
    }

    /// 量尺：`cargo test --lib token_diet_baseline -- --ignored --nocapture`
    ///
    /// token 瘦身专项的基线：三套 registry 在 stub（默认发送形态）与 full
    /// （懒加载展开上限）两种形态下，发给 LLM 的 tools 数组的真实 o200k
    /// token 数，附逐工具排行。默认 AppConfig，不含平台插件回合注册的工具。
    #[test]
    #[ignore]
    fn token_diet_baseline_probe() {
        use crate::tools::tests::test_paths;
        use crate::tools::{
            builtin_registry, dev_registry, restricted_platform_registry, AppConfig,
        };
        let temp = tempfile::tempdir().unwrap();
        let paths = test_paths(temp.path());
        let config = AppConfig::default();
        for (label, registry) in [
            ("normal", builtin_registry(&config, &paths)),
            ("dev", dev_registry(&config, &paths)),
            ("restricted", restricted_platform_registry(&config, &paths)),
        ] {
            for (variant, defs) in [
                ("stub", registry.stub_definitions()),
                ("full", registry.definitions()),
            ] {
                let whole = serde_json::to_string(&defs).unwrap();
                let tokens = crate::token_counter::count(&whole);
                eprintln!(
                    "[{label}/{variant}] tools={} bytes={} tokens={}",
                    defs.len(),
                    whole.len(),
                    tokens
                );
                let mut rows: Vec<(String, usize, usize)> = defs
                    .iter()
                    .map(|d| {
                        let s = serde_json::to_string(d).unwrap();
                        (
                            d.function.name.clone(),
                            s.len(),
                            crate::token_counter::count(&s),
                        )
                    })
                    .collect();
                rows.sort_by_key(|r| std::cmp::Reverse(r.2));
                for (name, bytes, toks) in rows {
                    eprintln!("  {toks:>6} tok {bytes:>6} B  {name}");
                }
            }
        }
    }
}
