//! Antigravity CLI 中转协议(`protocol = "antigravity"`)。
//!
//! 传输层是本机 `agy` 子进程的 stream-json 流,与 claude-code 线同构:CLI 用
//! 用户既有的 Google 登录态,Miyu 不经手凭据;工具循环的所有权在 agy 侧,Miyu
//! 工具经 `miyu mcp-serve` 桥挂进去,回合循环看到的永远是「一次请求、纯文本回
//! 来、没有 tool_calls」。
//!
//! 与 claude-code 的三处实质差异(09-03 实测,见
//! docs/plan/2026-09-03-antigravity-provider-design-review.md):
//! ①没有 `--system-prompt`——人格写成全局自定义代理
//! `~/.gemini/config/agents/miyu/agent.md`,正文整体替换 agy 默认指令,
//! `tools:` 白名单同时决定原生工具面(不写只得缩水集;列全 57 件会整轮出错);
//! ②没有 `--mcp-config`——桥只能全局注册在 `~/.gemini/config/mcp_config.json`,
//! 靠 agy 把自己的环境(MIYU_SESSION 等)原样继承给 MCP 子进程按会话分流;
//! ③续传目标丢失**不报错**而是静默新开会话,判据是 init 首行的 id 与请求不符。
//!
//! 会话续传复用 claude-code 的逐消息哈希链([`session`]):键本就带 provider
//! 维度,种子含系统提示词,「提示词变=新会话全量重放」与那条线语义一致。

mod setup;
mod stream;

use crate::llm::openai_compatible::claude_code::{host_tools_face, payload, scope_allows, session};
use crate::llm::openai_compatible::*;

pub(in crate::llm::openai_compatible) use setup::remove_conversation_files;

/// 客户端构造期解析好的运行时参数,端点间共享。
pub(in crate::llm::openai_compatible) struct AntigravityRuntime {
    pub(in crate::llm::openai_compatible) binary: PathBuf,
    /// agy 原生工具的模式作用域:off/dev/normal/all。
    pub(in crate::llm::openai_compatible) native_tools: String,
    /// Miyu 工具经 MCP 桥挂给 agy 的模式作用域:off/dev/normal/all。
    pub(in crate::llm::openai_compatible) miyu_tools: String,
    /// 桥工具按 eager 注册(原生名直调)还是走 agy 的懒加载。
    pub(in crate::llm::openai_compatible) miyu_tools_eager: bool,
    pub(in crate::llm::openai_compatible) idle_timeout: Duration,
    pub(in crate::llm::openai_compatible) print_timeout: Duration,
    /// agy 的用户配置根(`~/.gemini/config`):代理文件与 MCP 注册都落这里。
    /// 测试经 `MIYU_AGY_CONFIG_DIR` 改道,免得碰真实配置。
    pub(in crate::llm::openai_compatible) config_dir: PathBuf,
}

impl AntigravityRuntime {
    pub(in crate::llm::openai_compatible) fn from_config(config: &AppConfig) -> Self {
        let plugin = &config.plugins.antigravity;
        let binary = if plugin.binary.trim().is_empty() {
            PathBuf::from("agy")
        } else {
            PathBuf::from(plugin.binary.trim())
        };
        Self {
            binary,
            native_tools: plugin.native_tools.clone(),
            miyu_tools: plugin.miyu_tools.clone(),
            miyu_tools_eager: plugin.miyu_tools_eager,
            idle_timeout: Duration::from_secs(plugin.idle_timeout_seconds.max(30)),
            print_timeout: Duration::from_secs(plugin.print_timeout_seconds.max(60)),
            config_dir: setup::default_config_dir(),
        }
    }
}

/// 人格代理在 agy 侧的固定名字:内容按提示词改写(agy 每个进程都重读文件,
/// 实测无缓存),用户的 /agents 面板里只多这一条。
pub(in crate::llm::openai_compatible) const AGENT_NAME: &str = "miyu";

/// 全局 mcp_config.json 里桥条目的键。
pub(in crate::llm::openai_compatible) const MCP_SERVER_NAME: &str = "miyu";

/// `tools:` 白名单——「原生全开」的实际内容。取自默认代理自报的工具集里
/// **注册表实测认识**的名字(09-03:command_status/wait_5_seconds 不在注册表,
/// 列了会让整轮静默失败;browser_* 系需要浏览器上下文,列了整轮报错)。
/// 故意不列的两件:`ask_question`(无头下必被跳过,不列它模型就只剩桥版
/// `mcp_miyu_ask_question`)与 `generate_image`(原生生图落在 agy 自己的产物
/// 目录,不进 Miyu 的 tool.image 通道,用户看不到)。
pub(in crate::llm::openai_compatible) const NATIVE_TOOLS: &[&str] = &[
    "run_command",
    "view_file",
    "write_to_file",
    "replace_file_content",
    "find_by_name",
    "grep_search",
    "list_dir",
    "read_url_content",
    "search_web",
];

/// 两套工具同开时从桥里剔除的 Miyu 工具(与 agy 原生功能重复,原生在训练
/// 分布内且吃订阅额度,优先)。与 claude 线的差异:agy 没有 todowrite 对应物
/// (`manage_task` 管的是后台任务),所以不剔;`read`/`edit`/`task`/`job`/`alarm`
/// 不剔的理由同 claude 线(kb:/artifact: 域、daemon 常驻后台)。
pub(in crate::llm::openai_compatible) const BRIDGE_DUPLICATE_TOOLS: &[&str] =
    &["run_command", "web_search", "web_fetch", "glob", "grep"];

/// 中转环境事实(声明式,不写指令;常量字节保证提示词哈希稳定)。
const RELAY_ENVIRONMENT_NOTE: &str = "\n\n<relay-environment>\nThis session runs inside Miyu's relay: each turn is a fresh agy process that exits when the turn ends. Work backgrounded through the built-in tools (run_command background runs, manage_task, schedule, subagents) dies with the process, and its completion notifications never arrive. The built-in ask_question and generate_image tools are not wired to the user here.\n</relay-environment>";

/// miyu 工具桥在场时的补充事实。
const RELAY_MIYU_TOOLS_NOTE: &str = "\n<relay-environment-tools>\nThe mcp_miyu_ tools live in the persistent Miyu daemon and survive across turns: mcp_miyu_task runs a background subagent that wakes a follow-up turn when it finishes, mcp_miyu_job inspects or stops those, mcp_miyu_alarm schedules timed reminders, mcp_miyu_ask_question actually reaches the user and waits for the answer, and mcp_miyu_generate_image delivers the picture to the user.\n</relay-environment-tools>";

/// 续传目标在 agy 侧已不存在:agy 不报错,静默新开了别的会话。
#[derive(Debug)]
pub(super) struct ResumeTargetLost {
    pub(super) requested: String,
    pub(super) actual: String,
}

impl std::fmt::Display for ResumeTargetLost {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "agy did not resume conversation {} (it started {} instead)",
            self.requested, self.actual
        )
    }
}

impl std::error::Error for ResumeTargetLost {}

pub(super) fn resume_target_lost(error: &anyhow::Error) -> bool {
    error
        .chain()
        .any(|cause| cause.downcast_ref::<ResumeTargetLost>().is_some())
}

impl OpenAiCompatibleClient {
    pub(crate) async fn chat_antigravity_stream<F>(
        &self,
        messages: Vec<ChatMessage>,
        tools: Vec<ToolDefinition>,
        request_id: &str,
        on_chunk: &mut F,
    ) -> Result<ChatResult>
    where
        F: FnMut(ChatStreamChunk) -> Result<()>,
    {
        let runtime = self
            .antigravity
            .clone()
            .context("antigravity runtime was not initialized for this client")?;
        let model = self.provider.default_model.clone();
        let (system_prompt, conversation) = payload::split_system(messages);
        // 辅助请求(压缩摘要、标题、judge)一次一个会话,不参与续传匹配;
        // 用完顺手删掉 agy 侧转录,免得辅助请求把用户的会话列表刷满。
        let ephemeral = self.request_scope != "chat";
        let workdir = crate::tools::workspace::effective_workdir();
        let miyu_session = crate::tools::workspace::try_session();
        let miyu_session = miyu_session.as_deref();
        let host_tools = host_tools_face(miyu_session);
        // 工具面按双四档作用域装配(语义同 claude 线):subagent 作用域也给,
        // 纯文本辅助请求无工具。
        let tool_capable = matches!(self.request_scope, "chat" | "subagent");
        let native_on =
            tool_capable && scope_allows(&runtime.native_tools, self.claude_code_dev_mode);
        let miyu_on = tool_capable && scope_allows(&runtime.miyu_tools, self.claude_code_dev_mode);
        let agent_prompt = compose_agent_prompt(&system_prompt, native_on, miyu_on);
        let chain = session::prefix_chain(&self.provider.id, &model, &agent_prompt, &conversation);
        let resumable = if ephemeral {
            None
        } else {
            session::find_resumable(
                &self.provider.id,
                &model,
                miyu_session,
                host_tools,
                &chain,
                conversation.len(),
            )
        };
        // 人格代理与桥注册落盘(内容不变就不写)。桥的 eager 名单 = 本轮工具
        // 面减去与原生重复的;桥关着时名单为空,并且不给 MIYU_SESSION——
        // mcp-serve 见到守卫而没有会话就只应答空工具表。
        setup::ensure_agent_file(&runtime.config_dir, &agent_prompt, native_on)?;
        let eager_tools: Vec<String> = if miyu_on && runtime.miyu_tools_eager {
            tools
                .iter()
                .map(|tool| tool.function.name.clone())
                .filter(|name| !native_on || !BRIDGE_DUPLICATE_TOOLS.contains(&name.as_str()))
                .collect()
        } else {
            Vec::new()
        };
        setup::ensure_mcp_entry(&runtime.config_dir, &eager_tools)?;
        let env = relay_env(miyu_on, native_on, miyu_session);

        let (resume_id, covered) = match resumable.clone() {
            Some((id, len)) => (Some(id), len),
            None => (None, 0),
        };
        let payload = render_stdin_line(&conversation[covered..]);
        let args = self.antigravity_args(&runtime, &model, &workdir, resume_id.as_deref());
        crate::llm::request_log::record(
            &self.provider.id,
            &model,
            "antigravity",
            self.request_scope,
            &runtime.binary.display().to_string(),
            &json!({ "args": args, "stdin": payload, "conversation": conversation }),
        );
        let outcome = match stream::run_agy_turn(
            &runtime,
            &workdir,
            &args,
            &env,
            &payload,
            resume_id.as_deref(),
            request_id,
            on_chunk,
        )
        .await
        {
            Err(error) if resumable.is_some() && resume_target_lost(&error) => {
                // agy 侧会话没了(被清理/过期):忘掉映射,整段全量重放一次。
                // init 是流的首行、先于模型调用,所以上面那次几乎没花额度。
                tracing::warn!(
                    request_id,
                    error = %format!("{error:#}"),
                    "antigravity resume target is gone; replaying the full conversation in a fresh session"
                );
                if let Some((relay_session, _)) = &resumable {
                    session::forget_session(relay_session);
                }
                let payload = render_stdin_line(&conversation);
                let args = self.antigravity_args(&runtime, &model, &workdir, None);
                stream::run_agy_turn(
                    &runtime, &workdir, &args, &env, &payload, None, request_id, on_chunk,
                )
                .await?
            }
            other => other?,
        };
        if let Some(conversation_id) = &outcome.conversation_id {
            if ephemeral {
                setup::remove_conversation_files(conversation_id);
            } else {
                let content = outcome.result.content.clone();
                if !content.trim().is_empty() {
                    let predicted = ChatMessage::assistant(content, None);
                    let next_hash = session::extend_chain(chain[conversation.len()], &predicted);
                    session::record_session(
                        &self.provider.id,
                        &model,
                        miyu_session,
                        host_tools,
                        conversation.len() + 1,
                        next_hash,
                        conversation_id.clone(),
                    );
                }
            }
        }
        Ok(outcome.result)
    }

    fn antigravity_args(
        &self,
        runtime: &AntigravityRuntime,
        model: &str,
        workdir: &std::path::Path,
        resume: Option<&str>,
    ) -> Vec<String> {
        // `--print=`:print 旗标必须带参数(空串即可),否则它把下一个旗标吃成
        // 提示词。
        let mut args: Vec<String> = [
            "--print=",
            "--input-format",
            "stream-json",
            "--output-format",
            "stream-json",
            "--dangerously-skip-permissions",
        ]
        .into_iter()
        .map(str::to_string)
        .collect();
        args.push("--model".into());
        args.push(model.to_string());
        if let Some((_, variant)) = self.selected_reasoning_variant() {
            if let crate::models_cache::ReasoningSetting::Effort(effort) = variant.setting {
                args.push("--effort".into());
                args.push(effort);
            }
        }
        // 人格代理:恒挂。流侧校验 init.agent,没挂上视为错误(否则静默跑在
        // agy 自己 13.9k tok 的默认提示词上)。
        args.push("--agent".into());
        args.push(AGENT_NAME.into());
        // 原生 run_command 默认跑在 agy 的 scratch 目录,不是进程 cwd;只有
        // --add-dir 过的目录才是它的工作区。只加一个:加两个时 cwd 在两者间随机。
        args.push("--add-dir".into());
        args.push(workdir.display().to_string());
        args.push("--print-timeout".into());
        args.push(format!("{}s", runtime.print_timeout.as_secs()));
        if let Some(resume) = resume {
            args.push("--conversation".into());
            args.push(resume.to_string());
        }
        args
    }
}

/// 代理文件正文 = Miyu 系统提示词 + 中转环境事实。
fn compose_agent_prompt(system_prompt: &str, native_on: bool, miyu_on: bool) -> String {
    let mut prompt = system_prompt.to_string();
    if native_on || miyu_on {
        prompt.push_str(RELAY_ENVIRONMENT_NOTE);
        if miyu_on {
            prompt.push_str(RELAY_MIYU_TOOLS_NOTE);
        }
    }
    prompt
}

/// 给 agy 进程的环境:它会原样继承给 MCP 子进程(实测),所以桥的会话身份
/// 走这里而不是 mcp_config 的静态 env。MIYU_HOME/XDG_RUNTIME_DIR 本来就在
/// 我们自己的环境里,自然继承,不再显式塞(claude 线第六轮的「如实透传」教训
/// 在这里天然满足)。
fn relay_env(
    miyu_on: bool,
    native_on: bool,
    miyu_session: Option<&str>,
) -> Vec<(String, Option<String>)> {
    let mut env: Vec<(String, Option<String>)> = Vec::new();
    match (miyu_on, miyu_session) {
        (true, Some(session)) => {
            env.push(("MIYU_SESSION".into(), Some(session.to_string())));
            let origin = serde_json::to_string(&crate::tools::workspace::current_turn_origin())
                .unwrap_or_default();
            env.push(("MIYU_TURN_ORIGIN".into(), Some(origin)));
            // 桥吐的 schema 按 Gemini 方言整形(空 enum/联合类型/多余键会被 400)。
            env.push(("MIYU_MCP_SCHEMA_DIALECT".into(), Some("gemini".into())));
            env.push((
                "MIYU_MCP_EXCLUDE".into(),
                Some(if native_on {
                    BRIDGE_DUPLICATE_TOOLS.join(",")
                } else {
                    String::new()
                }),
            ));
        }
        _ => {
            // 桥关着:抹掉会话身份,守卫让 mcp-serve 只应答空工具表。
            env.push(("MIYU_SESSION".into(), None));
            env.push(("MIYU_TURN_ORIGIN".into(), None));
            env.push(("MIYU_MCP_EXCLUDE".into(), None));
            env.push(("MIYU_MCP_SCHEMA_DIALECT".into(), None));
        }
    }
    env
}

/// stdin 的单行载荷:agy 的 `{"event":"user","message":{"content":[…]}}`。
/// 内容块复用 claude 线的翻译(历史转写 + 活跃尾巴),agy 只收 text 块,
/// 图片块降级成占位文本。
fn render_stdin_line(delta: &[ChatMessage]) -> String {
    let blocks: Vec<Value> = payload::render_user_blocks(delta)
        .into_iter()
        .map(|block| {
            if block.get("type").and_then(Value::as_str) == Some("image") {
                json!({
                    "type": "text",
                    "text": "[image omitted: the antigravity relay accepts text only]"
                })
            } else {
                block
            }
        })
        .collect();
    let mut line = json!({
        "event": "user",
        "message": { "content": blocks }
    })
    .to_string();
    line.push('\n');
    line
}

#[cfg(test)]
mod bridge_dedup_tests {
    use super::{BRIDGE_DUPLICATE_TOOLS, NATIVE_TOOLS};

    /// 去重名单里的每个名字都必须真的是一件已注册的 Miyu 工具(改名会让
    /// 纯字符串名单静默失效,claude 线的 read_file/apply_patch 教训)。
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

    /// 白名单是给 agy 的原生名,与 Miyu 工具名撞上的只能是同名剔除项:同名
    /// 两源同时出现会让卡片没法区分。
    #[test]
    fn native_allowlist_does_not_collide_with_bridged_miyu_names() {
        for name in NATIVE_TOOLS {
            let miyu_has_it = name == &"run_command";
            assert!(
                !miyu_has_it || BRIDGE_DUPLICATE_TOOLS.contains(name),
                "{name} 同时是 agy 原生名与 Miyu 工具名,必须进去重名单"
            );
        }
    }
}
