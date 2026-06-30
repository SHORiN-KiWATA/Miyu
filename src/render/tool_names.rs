/// 返回面向用户展示的工具名称。
///
/// 参数:
/// - `name`: 工具原始名称
///
/// 返回:
/// - 本地化后的工具名称，未知工具返回原名
pub(crate) fn readable_tool_name(name: &str) -> &str {
    match name {
        "run_command" => "运行命令",
        "task_agent" => "创建子任务",
        "read_file" => "读取文件",
        "write_file" => "写入文件",
        "edit_file" => "编辑文件",
        "list_directory" => "列目录",
        "create_directory" => "创建目录",
        "trash_path" => "移入回收站",
        "find_files" | "glob" => "查找文件",
        "search_text" | "grep" => "搜索文本",
        "get_current_directory" => "当前目录",
        "get_current_time" => "当前时间",
        "inspect_issue" => "检查问题",
        "check_os_info" => "查看系统信息",
        "web_search" => "网页搜索",
        "web_fetch" => "读取网页",
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
        "pacman_search" => "搜索软件包",
        "archwiki_query" => "查询 ArchWiki",
        "online_man_search" | "man_search" => "搜索在线手册",
        "online_man_get_page" | "man_read" => "读取在线手册",
        "moegirl_query" => "查询萌娘百科",
        "calculate" | "calculator" => "计算",
        "calculate_hash" => "计算哈希",
        "decode_encoded_text" => "解码文本",
        "exchange_rate" | "get_exchange_rate" => "汇率查询",
        "weather" | "get_weather" => "天气查询",
        "xuanxue_pick" => "玄学选择",
        "xuanxue_divine" => "玄学占卜",
        "draw_zhouyi_hexagram" => "周易起卦",
        "draw_tarot_card" => "抽塔罗牌",
        "draw_fortune_lot" => "抽签",
        "load_skill" => "加载技能",
        "review_aur_package" => "审查 AUR 包",
        "install_aur_package" => "安装 AUR 包",
        "review_pkgbuild_directory" => "审查 PKGBUILD 目录",
        "linux_game_compatibility" => "查询 Linux 游戏兼容性",
        _ => name,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn readable_tool_names_translate_known_tools_and_fallback_unknown() {
        assert_eq!(readable_tool_name("deep_research"), "深度研究");
        assert_eq!(readable_tool_name("read_file"), "读取文件");
        assert_eq!(readable_tool_name("inspect_issue"), "检查问题");
        assert_eq!(readable_tool_name("check_os_info"), "查看系统信息");
        assert_eq!(readable_tool_name("get_weather"), "天气查询");
        assert_eq!(readable_tool_name("get_exchange_rate"), "汇率查询");
        assert_eq!(readable_tool_name("draw_zhouyi_hexagram"), "周易起卦");
        assert_eq!(readable_tool_name("draw_tarot_card"), "抽塔罗牌");
        assert_eq!(readable_tool_name("draw_fortune_lot"), "抽签");
        assert_eq!(readable_tool_name("vision_analyze"), "分析图片");
        assert_eq!(readable_tool_name("search_meme"), "搜索表情包");
        assert_eq!(readable_tool_name("show_meme"), "发送表情");
        assert_eq!(readable_tool_name("add_meme"), "添加表情包");
        assert_eq!(readable_tool_name("task_agent"), "创建子任务");
        assert_eq!(
            readable_tool_name("upload_text_to_knowledge_base"),
            "导入知识库"
        );
        assert_eq!(readable_tool_name("search_evicted_context"), "搜索旧上下文");
        assert_eq!(readable_tool_name("recall_past_events"), "回忆往事");
        assert_eq!(readable_tool_name("aur_check_status"), "查询 AUR 状态");
        assert_eq!(readable_tool_name("online_man_search"), "搜索在线手册");
        assert_eq!(readable_tool_name("online_man_get_page"), "读取在线手册");
        assert_eq!(readable_tool_name("install_aur_package"), "安装 AUR 包");
        assert_eq!(
            readable_tool_name("search_knowledge_base_by_name"),
            "按名称搜索知识库"
        );
        assert_eq!(readable_tool_name("recall_memories"), "召回记忆");
        assert_eq!(readable_tool_name("custom_skill"), "custom_skill");
    }
}
