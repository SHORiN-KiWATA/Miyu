//! 系统提示词的拼装。
//!
//! 每个 `with_*` 都是往提示词上**追加**一段，从不往中间插——顺序即缓存前缀，
//! 换了顺序等于全 miss。`host_environment_is_byte_stable_across_prompt_rebuilds`
//! 那条测试守的就是这一点：同一份配置重建两次，字节必须完全一致。
//!
//! `with_host_environment` 的主机路径、渲染能力、语音协议只对属主开放：主机
//! 路径不该出现在群聊人格的提示词里。风格锁例外——它与受众无关,外部受众也
//! 带(追加在末尾,属主分支字节顺序不变)。

use crate::agent::*;

pub(in crate::agent) fn with_mode_reminder(system_prompt: String, mode: AgentMode) -> String {
    let mut prompt = system_prompt;
    if let Some(reminder) = mode.reminder() {
        prompt.push_str("\n\n");
        prompt.push_str(reminder);
    }
    prompt
}

pub(in crate::agent) fn with_runtime_system_context(
    mut system_prompt: String,
    context: &[String],
) -> String {
    for item in context
        .iter()
        .map(String::as_str)
        .filter(|item| !item.is_empty())
    {
        system_prompt.push_str("\n\n");
        system_prompt.push_str(item);
    }
    system_prompt
}

/// 模式选提示词源:Dev=一行可编辑开发提示词(无人格全家、无用户身份,
/// 极简原则);Normal=人格提示词(按 audience 附用户档案)。
/// `with_user_profile`:属主档案进不进提示词——终端/WebUI 回合进,通讯平台
/// 回合不进(阶段 6:成员的 WebUI 回合带的是成员自己的档案)。
pub(in crate::agent) fn mode_system_prompt(
    config: &AppConfig,
    paths: &MiyuPaths,
    mode: AgentMode,
    audience: PromptAudience,
    with_user_profile: bool,
) -> Result<String> {
    match mode {
        AgentMode::Dev => config.dev_system_prompt(paths),
        AgentMode::Normal => config.system_prompt_with(paths, audience, with_user_profile),
    }
}

/// 联想记忆块的前言常量上提到 system 提示词(08-17)。
///
/// 它逐字不变,却随每个 `<associative-memory>` 块重发一次:实测终端长会话
/// 42 块共 2,142 字符(占该块总量 6.5%),QQ 群会话 240 块共 28,410 字符
/// (占 31.8%)。放进 system 说一次,块里只留会变的部分。
///
/// Dev 会话一个字都不发:`dev_scoped()` 把记忆整套关掉(09-09),
/// `memory_enabled` 在那条路上恒为假。
pub(in crate::agent) fn with_memory_preamble(
    mut system_prompt: String,
    memory_enabled: bool,
) -> String {
    if !memory_enabled {
        return system_prompt;
    }
    system_prompt.push_str("\n\n");
    // 进 system 提示词=模型可见面,恒英文,不随 UI locale 变。
    system_prompt.push_str(
        "<associative-memory> blocks hold memories recalled from the current input. Do not treat the people in them as the current user, and do not imitate the recorded dialogue as a style example. A block that names a principal only contains public knowledge plus that principal's own memories; a stable principal is what identifies a person — nicknames and message text never reassign a memory's owner.",
    );
    system_prompt
}

/// 工具期风格锁(08-23 工具体制 A/B 实测 n=12/臂:探针全过 5/12→8/12,无换行
/// 6/12→10/12)。所有人格会话共用,dev 不带。
pub(in crate::agent) const STYLE_LOCK: &str = "\n\n<style-lock>Stay in character across tool calls. Tool results are working material; they are not a reason to switch into an assistant reporting tone.</style-lock>";

pub(in crate::agent) fn with_host_environment(
    mut system_prompt: String,
    audience: PromptAudience,
    paths: &MiyuPaths,
    mode: AgentMode,
) -> String {
    if audience == PromptAudience::External {
        // 风格锁与受众无关(09-10 分层架构阶段 2):它守的是「工具循环后别切
        // 播报腔」,QQ 群里同样需要。此前它只是顺手放进了属主分支,等于让
        // 场所替人格做了决定。属主提示词的字节顺序不动(零冷启动),外部
        // 受众追加在末尾——风格锁的位置有 A/B 背书(08-23),末尾就是那个位。
        // Internal(判官、子代理)不是人格,不加。
        if mode != AgentMode::Dev {
            system_prompt.push_str(STYLE_LOCK);
        }
        return system_prompt;
    }
    if audience != PromptAudience::Owner {
        return system_prompt;
    }
    system_prompt.push_str("\n\n");
    system_prompt.push_str(&crate::host_info::host_environment_block(&paths.root_dir));
    // 渲染能力说明(仅 owner 会话):终端与 WebUI 都支持 LaTeX。
    // 不放人格提示词里——QQ 等平台的排版能力不同,不该看到这段。
    // dev 也不带:极简原则,编码任务用不上排版说明(验收 08-16 解剖)。
    if mode != AgentMode::Dev {
        // 工具期风格锁:模型进工具循环后切播报腔是 OOC 主场景(AstrBot 4.6
        // 同款思路)。08-23 工具体制 A/B 实测 n=12/臂:探针全过 5/12→8/12,
        // 无换行 6/12→10/12,其余指标不降。
        system_prompt.push_str(STYLE_LOCK);
        system_prompt.push_str(
            "\n\nWrite math in LaTeX. Block formulas (`$$…$$` on their own paragraph) render as typeset images; inline `$…$` becomes Unicode math text. Never hand-build formulas from bare Unicode or ASCII.",
        );
        // 语音协议是常量,所有 owner 会话共用:语音会话不换系统提示词,缓存
        // 前缀与别的会话一致;只有被 <voice_input> 包裹的用户消息才触发
        // <speak> 块,打字的会话不会多吐一个字。
        system_prompt.push_str("\n\n");
        system_prompt.push_str(VOICE_PROTOCOL);
    }
    system_prompt
}

/// 每轮瞬态尾巴里唯一的运行时事实：时间 + 工作目录。
///
/// 其余字段全部退场（08-17 实测）：`env`/`shell`/`terminal` 读的是 **daemon
/// 进程**的 stdio 与环境变量，而回合跑在 daemon 里——stdin 恒为 /dev/null,
/// 于是 `env` 永远报"非交互"; `TERM`/`SHELL` 是 daemon 启动那一刻冻结的值,
/// 与真正的客户端终端无关(实测 daemon=xterm-kitty 而客户端=tmux-256color)。
/// 三个字段既是错的又占 91 字符。`note` 那句身份守卫同样删除。
///
/// 时间格式:终端小时级、平台分钟级——同粒度内整块字节不变,配合"变了才
/// 注入"的投影(见 `chat_messages`)。ISO 日期比中文日期短,星期用三字母。
pub(in crate::agent) fn runtime_context(mode: AgentMode, platform: bool) -> String {
    // 时区随时间一起给(%:z 固定 6 字符,同粒度内字节稳定):模型换算
    // 绝对时间/跨时区事件时不用再猜本机时区。带 UTC 前缀写成
    // "UTC+09:00"——裸偏移量容易被当成时间的一部分读(08-26 用户点名)。
    if platform {
        return format!(
            "<runtime now=\"{}\"/>",
            Local::now().format("%Y-%m-%d %a %H:%M UTC%:z")
        );
    }
    let cwd = crate::tools::workspace::effective_workdir()
        .display()
        .to_string();
    let _ = mode;
    format!(
        "<runtime now=\"{}\" cwd=\"{}\"/>",
        Local::now().format("%Y-%m-%d %a %H:00 UTC%:z"),
        xml_attr_escape(&cwd),
    )
}

/// 语音对话协议(见 `web::voice_tts`):模型在 `<speak>` 块里给可朗读的口语版。
pub(crate) const VOICE_PROTOCOL: &str = "<voice-protocol>用户消息被 <voice_input> 包裹时是语音对话:正文照常回答;末尾另起一个 <speak> 块,写两三句能直接读出来的口语版(不含路径、代码、链接、Markdown、表格)。用户消息没有这个标记时绝不输出 <speak>。</voice-protocol>";

pub(in crate::agent) fn clean_user_visible_text(input: &str) -> String {
    let mut output = input.to_string();
    for tag in ["system-reminder", "system_reminder"] {
        output = strip_tagged_sections(output, tag);
    }
    output
}

/// 给 crate 内别处(语音播报)复用的标签剥离。
pub(crate) fn prompt_strip_tagged(text: String, tag: &str) -> String {
    strip_tagged_sections(text, tag)
}

pub(in crate::agent) fn strip_tagged_sections(mut text: String, tag: &str) -> String {
    let open = format!("<{tag}");
    let close = format!("</{tag}>");
    while let Some(start) = text.find(&open) {
        let Some(relative_end) = text[start..].find(&close) else {
            text.replace_range(start.., "");
            break;
        };
        let end = start + relative_end + close.len();
        text.replace_range(start..end, "");
    }
    text
}
