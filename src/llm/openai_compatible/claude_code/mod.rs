//! Claude Code CLI 中转协议(`protocol = "claude-code"`)。
//!
//! 传输层不是 HTTP,是本机 `claude` 子进程的 stream-json 双向流:CLI 用用户
//! 既有的订阅登录态,Miyu 不经手任何凭据。工具循环的所有权在 claude 侧——
//! Miyu 的工具经 `miyu mcp-serve` 桥挂进去(内层调用照走 daemon 的 guard 管
//! 线),所以这条线对 Miyu 的回合循环呈现为「一次请求、纯文本(+思考)回来、
//! 永远没有 tool_calls」。
//!
//! 作用域裁决、哈希链续传、载荷转写、子进程泵都在 [`cli_relay`];这里只剩
//! claude 特有的三样:命令行怎么拼(`--system-prompt` 整体替换、`--mcp-config`
//! 内联 JSON、`--resume`)、事件怎么解析([`stream`])、清空时删哪里的转录。

mod stream;

use crate::llm::openai_compatible::cli_relay::{
    self, payload, RelayOutcome, ResumePlan, ToolScopes,
};
use crate::llm::openai_compatible::*;

/// 客户端构造期解析好的运行时参数(binary/工具作用域/权限模式),端点间共享。
pub(in crate::llm::openai_compatible) struct ClaudeCodeRuntime {
    pub(in crate::llm::openai_compatible) binary: PathBuf,
    /// claude 原生工具(Bash/Edit/Read…)的模式作用域:off/dev/normal/all。
    pub(in crate::llm::openai_compatible) native_tools: String,
    /// Miyu 工具经 MCP 桥挂给 claude 的模式作用域:off/dev/normal/all。
    pub(in crate::llm::openai_compatible) miyu_tools: String,
    /// 原生工具开启时的 --permission-mode(无头模式没有交互审批)。
    pub(in crate::llm::openai_compatible) permission_mode: String,
    /// 每个 (provider\tmodel) 的 --autocompact 阈值:取 Miyu 的有效窗口值
    /// (显式配置→目录→默认 168k),夹到 CLI 接受的 100k–1M。claude 在这个
    /// 尺寸自压缩,会话 id 不变、续传不断——窗口语义单一来源是 Miyu 配置。
    pub(in crate::llm::openai_compatible) autocompact: HashMap<String, u64>,
    pub(in crate::llm::openai_compatible) idle_timeout: Duration,
    pub(in crate::llm::openai_compatible) prefer_subscription: bool,
}

impl ClaudeCodeRuntime {
    pub(in crate::llm::openai_compatible) fn from_config(config: &AppConfig) -> Self {
        let plugin = &config.plugins.claude_code;
        let binary = if plugin.binary.trim().is_empty() {
            PathBuf::from("claude")
        } else {
            PathBuf::from(plugin.binary.trim())
        };
        Self {
            binary,
            native_tools: plugin.native_tools.clone(),
            miyu_tools: plugin.miyu_tools.clone(),
            permission_mode: plugin.permission_mode.clone(),
            autocompact: HashMap::new(),
            idle_timeout: Duration::from_secs(plugin.idle_timeout_seconds.max(30)),
            prefer_subscription: plugin.prefer_subscription,
        }
    }
}

impl OpenAiCompatibleClient {
    pub(crate) async fn chat_claude_code_stream<F>(
        &self,
        messages: Vec<ChatMessage>,
        _tools: Vec<ToolDefinition>,
        request_id: &str,
        on_chunk: &mut F,
    ) -> Result<ChatResult>
    where
        F: FnMut(ChatStreamChunk) -> Result<()>,
    {
        let runtime = self
            .claude_code
            .clone()
            .context("claude-code runtime was not initialized for this client")?;
        let model = self.provider.default_model.clone();
        let (system_prompt, conversation) = payload::split_system(messages);
        // 一律用会话工作区(与 run_command 同源):原生工具在这里操作文件,
        // 无工具时 cwd 无关紧要。回合作用域外(测试/辅助)回退进程 cwd。
        let workdir = crate::tools::workspace::effective_workdir();
        let miyu_session = crate::tools::workspace::try_session();
        let miyu_session = miyu_session.as_deref();
        // 续传按工具面档位隔离:桥每轮按触发者身份重算工具面,两档共用一条
        // claude 会话会让清单逐轮增删,模型读成"工具掉线"(见 session 模块头)。
        let host_tools = cli_relay::host_tools_face(miyu_session);
        let scopes = cli_relay::tool_scopes(
            self.request_scope,
            &runtime.native_tools,
            &runtime.miyu_tools,
            self.claude_code_dev_mode,
        );
        let prompt = cli_relay::compose_prompt(
            &system_prompt,
            scopes,
            RELAY_ENVIRONMENT_NOTE,
            RELAY_MIYU_TOOLS_NOTE,
        );
        let mut plan = ResumePlan::new(
            &self.provider.id,
            &model,
            &prompt,
            conversation,
            self.request_scope,
            miyu_session,
            host_tools,
        );
        let mut outcome = self
            .claude_turn(
                &runtime, &model, &workdir, &prompt, scopes, &plan, request_id, on_chunk,
            )
            .await;
        if let Err(error) = &outcome {
            if plan.resume_id().is_some() && stream::resume_session_lost(error) {
                plan.resume_lost("claude-code", request_id, error);
                outcome = self
                    .claude_turn(
                        &runtime, &model, &workdir, &prompt, scopes, &plan, request_id, on_chunk,
                    )
                    .await;
            }
        }
        let outcome = outcome?;
        plan.record(&outcome);
        Ok(outcome.result)
    }

    #[allow(clippy::too_many_arguments)]
    async fn claude_turn<F>(
        &self,
        runtime: &ClaudeCodeRuntime,
        model: &str,
        workdir: &std::path::Path,
        prompt: &str,
        scopes: ToolScopes,
        plan: &ResumePlan,
        request_id: &str,
        on_chunk: &mut F,
    ) -> Result<RelayOutcome>
    where
        F: FnMut(ChatStreamChunk) -> Result<()>,
    {
        let payload = payload::render_user_payload(plan.delta());
        let args = self.claude_code_args(
            runtime,
            model,
            prompt,
            scopes,
            plan.resume_id(),
            plan.ephemeral(),
        );
        crate::llm::request_log::record(
            &self.provider.id,
            model,
            "claude-code",
            self.request_scope,
            &runtime.binary.display().to_string(),
            // conversation 是续传哈希链的原料,录下来才能诊断"为什么没命中"。
            &json!({ "args": args, "stdin": payload, "conversation": plan.conversation() }),
        );
        stream::run_claude_turn(runtime, workdir, &args, &payload, request_id, on_chunk).await
    }

    fn claude_code_args(
        &self,
        runtime: &ClaudeCodeRuntime,
        model: &str,
        prompt: &str,
        scopes: ToolScopes,
        resume: Option<&str>,
        ephemeral: bool,
    ) -> Vec<String> {
        let mut args: Vec<String> = [
            "-p",
            "--verbose",
            "--output-format",
            "stream-json",
            "--input-format",
            "stream-json",
            "--include-partial-messages",
        ]
        .into_iter()
        .map(str::to_string)
        .collect();
        args.push("--model".into());
        args.push(model.to_string());
        if let Some(window) = runtime
            .autocompact
            .get(&format!("{}\t{}", self.provider.id, model))
        {
            args.push("--autocompact".into());
            args.push(window.to_string());
        }
        // 思考档:Miyu 的 thinking-variant 选择映射到 CLI 的 --effort。
        if let Some((_, variant)) = self.selected_reasoning_variant() {
            if let crate::models_cache::ReasoningSetting::Effort(effort) = variant.setting {
                args.push("--effort".into());
                args.push(effort);
            }
        }
        // 整体替换默认系统提示词:人格/开发提示词原样过去,同时甩掉 Claude
        // Code 自带的 CLI 身份与 CLAUDE.md 注入。
        if !prompt.trim().is_empty() {
            args.push("--system-prompt".into());
            args.push(prompt.to_string());
        }
        if scopes.native_on {
            // claude 原生工具(训练分布内)开放;无头模式没有交互审批,
            // 权限模式决定 Bash 等是否可用(默认 bypassPermissions)。
            args.push("--permission-mode".into());
            args.push(runtime.permission_mode.clone());
        } else {
            args.push("--tools".into());
            args.push(String::new());
        }
        args.push("--strict-mcp-config".into());
        if scopes.miyu_on {
            // 两套同开时去重:与 claude 原生重复的 Miyu 工具剔除,原生优先
            // (用户拍板清单;load_skill/manage_skill 与 claude 的 Skill 内容
            // 不同,不算重复)。
            if let Some(mcp_config) = mcp_bridge_config(scopes.native_on) {
                args.push("--mcp-config".into());
                args.push(mcp_config);
                args.push("--allowedTools".into());
                args.push("mcp__miyu".into());
            }
        }
        if let Some(resume) = resume {
            args.push("--resume".into());
            args.push(resume.to_string());
        }
        if ephemeral {
            args.push("--no-session-persistence".into());
        }
        args
    }
}

/// 中转环境事实(声明式,不写指令;常量字节保证前缀稳定)。
const RELAY_ENVIRONMENT_NOTE: &str = "\n\n<relay-environment>\nThis session runs inside Miyu's relay: each turn is a fresh CLI process that exits when the turn ends. Work backgrounded through the built-in tools (Bash run_in_background, background Task) dies with the process, and its completion notifications never arrive.\n</relay-environment>";

/// miyu 工具桥在场时的补充事实。
const RELAY_MIYU_TOOLS_NOTE: &str = "\n<relay-environment-tools>\nThe mcp__miyu__ tools live in the persistent Miyu daemon and survive across turns: mcp__miyu__task runs a background subagent that wakes a follow-up turn when it finishes, mcp__miyu__job inspects or stops those, and mcp__miyu__alarm schedules timed reminders.\n</relay-environment-tools>";

/// 两套工具同开时从桥里剔除的 Miyu 工具(与 claude 原生功能重复,原生
/// 在训练分布内、优先)。task **不剔**:与原生 Task 语义不同——Miyu 子代理
/// 在 daemon 里作为后台任务运行、完成后唤醒开新轮跟进,与 job(查/停)成对。
/// job/alarm **不剔**:claude 自己的后台/定时机制
/// 活在单次进程里,中转每轮一进程、轮末即杀,活不过回合;Miyu 的 job 走
/// daemon 常驻 + 完成唤醒开新轮,才是这套架构下唯一能跟进的后台。
///
/// `read` / `edit` **不剔**,尽管它们看着就是原生 Read/Edit 的同义词:08-21
/// 三域合并之后这两件已经不只管文件,`read` 认 `kb:`(知识库)与
/// `artifact:`(WebUI 工作区)前缀、`edit` 对应地改这两处,原生工具够不着这
/// 两个域。名单里原来写的是它们改名前的 `read_file` / `apply_patch`,改名那
/// 天起就没匹配上任何工具——所以"两件重复工具一直挂在桥上"是既成事实,而
/// 不是回归;这里删掉死名字并留下判断,免得下一个人照着旧名字"修好"它,
/// 反手把 kb:/artifact: 从中转这条线上摘掉。
const BRIDGE_DUPLICATE_TOOLS: &[&str] = &[
    "run_command",
    "web_search",
    "web_fetch",
    "glob",
    "grep",
    "todowrite",
];

/// Miyu 工具经 MCP stdio 桥挂给 claude:`miyu mcp-serve` 打回 daemon,与
/// `miyu tool-call` 同一条会话→模式→registry 解析链。没有会话作用域(测试
/// /直连辅助请求)就不挂桥。claude 给 MCP server 的是洁净环境,home/runtime
/// 识别变量要显式带(如实透传,见 cli_relay::bridge_env_passthrough)。
fn mcp_bridge_config(exclude_duplicates: bool) -> Option<String> {
    let session = crate::tools::workspace::try_session()?;
    let exe = crate::paths::miyu_executable().ok()?;
    let origin = serde_json::to_string(&crate::tools::workspace::current_turn_origin()).ok()?;
    let mut env = serde_json::Map::new();
    env.insert("MIYU_SESSION".into(), json!(&*session));
    env.insert("MIYU_TURN_ORIGIN".into(), json!(origin));
    if exclude_duplicates {
        env.insert(
            "MIYU_MCP_EXCLUDE".into(),
            json!(BRIDGE_DUPLICATE_TOOLS.join(",")),
        );
    }
    for (key, value) in cli_relay::bridge_env_passthrough() {
        env.insert(key, json!(value));
    }
    Some(
        json!({
            "mcpServers": {
                "miyu": {
                    "command": exe,
                    "args": ["mcp-serve"],
                    "env": env,
                }
            }
        })
        .to_string(),
    )
}

/// 清空 Miyu 会话时的联动:尽力删除 claude 侧的会话转录
/// (`~/.claude/projects/<项目槽>/<会话id>.jsonl`)。
pub(in crate::llm::openai_compatible) fn remove_transcript(
    miyu_session: &str,
    claude_session: &str,
) {
    let Some(home) = std::env::var_os("HOME") else {
        return;
    };
    let projects = std::path::Path::new(&home).join(".claude").join("projects");
    let Ok(project_dirs) = std::fs::read_dir(&projects) else {
        return;
    };
    for project in project_dirs.flatten() {
        let transcript = project.path().join(format!("{claude_session}.jsonl"));
        if !transcript.exists() {
            continue;
        }
        match std::fs::remove_file(&transcript) {
            Ok(()) => tracing::info!(
                miyu_session,
                claude_session = %claude_session,
                "removed the claude-side transcript for a cleared Miyu session"
            ),
            Err(error) => tracing::warn!(
                %error,
                path = %transcript.display(),
                "failed to remove a claude-side transcript (best effort)"
            ),
        }
    }
}

#[cfg(test)]
mod bridge_dedup_tests {
    use super::BRIDGE_DUPLICATE_TOOLS;

    /// 去重名单里的每个名字都必须真的是一件已注册工具。名单是纯字符串,
    /// 改名不会让它报错——`read_file` / `apply_patch` 就是这么在 08-21 三域
    /// 合并里变成死条目、白挂了两件重复工具上桥的(09-01 转录取证)。
    #[test]
    fn every_deduplicated_name_is_a_real_tool() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let paths = crate::paths::MiyuPaths {
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
            system_scripts_dir: root.join("data/scripts"),
        };
        let mut config = crate::config::AppConfig::default();
        config.plugins.web.enabled = true;
        config.skills.allow_command_execution = true;
        let registry = crate::tools::builtin_registry(&config, &paths);
        for name in BRIDGE_DUPLICATE_TOOLS {
            assert!(
                registry.contains(name),
                "去重名单里的 {name} 不是任何已注册工具——多半是工具改名后忘了跟着改"
            );
        }
    }
}
