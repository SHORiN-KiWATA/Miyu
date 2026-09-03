//! OpenAI Codex CLI 中转协议(`protocol = "codex"`)。
//!
//! 传输层是本机 `codex exec --json` 子进程的 JSONL 事件流:CLI 用用户既有的
//! ChatGPT 登录态,Miyu 不经手凭据。与另两条 CLI 线同构(骨架在 [`cli_relay`]),
//! codex 特有的三样(09-03 实测,agy 1.1.24 同日对照):
//! ①任何配置都能经 `-c key=value` **逐进程**注入——人格走
//! `model_instructions_file`(整体替换内置指令,rollout 的 base_instructions
//! 实证)、`project_doc_max_bytes=0` 关掉 AGENTS.md、MCP 桥条目也逐进程给,
//! 不碰用户的 `~/.codex/config.toml`(`--ignore-user-config`);
//! ②stdin 就是提示词正文,没有结构化输入;
//! ③续传 `codex exec resume <id>`,目标丢失会报 `no rollout found`(先于鉴权)。

mod stream;

use crate::llm::openai_compatible::cli_relay::{
    self, payload, RelayOutcome, ResumePlan, ToolScopes,
};
use crate::llm::openai_compatible::*;

/// 客户端构造期解析好的运行时参数,端点间共享。
pub(in crate::llm::openai_compatible) struct CodexRuntime {
    pub(in crate::llm::openai_compatible) binary: PathBuf,
    /// codex 原生工具(shell/apply_patch/web_search)的模式作用域:off/dev/normal/all。
    pub(in crate::llm::openai_compatible) native_tools: String,
    /// Miyu 工具经 MCP 桥挂给 codex 的模式作用域:off/dev/normal/all。
    pub(in crate::llm::openai_compatible) miyu_tools: String,
    /// codex 沙箱:read-only / workspace-write / danger-full-access。
    pub(in crate::llm::openai_compatible) sandbox_mode: String,
    /// 不加载用户自己的 `~/.codex/config.toml`(登录态仍从 CODEX_HOME 读):
    /// 用户的 MCP 服务器/规则/模型偏好不会混进中转的工具面。
    pub(in crate::llm::openai_compatible) ignore_user_config: bool,
    pub(in crate::llm::openai_compatible) idle_timeout: Duration,
    /// 指令文件落盘目录(按提示词哈希命名,内容不变不写)。
    pub(in crate::llm::openai_compatible) instructions_dir: PathBuf,
}

impl CodexRuntime {
    pub(in crate::llm::openai_compatible) fn from_config(config: &AppConfig) -> Self {
        let plugin = &config.plugins.codex;
        let binary = if plugin.binary.trim().is_empty() {
            PathBuf::from("codex")
        } else {
            PathBuf::from(plugin.binary.trim())
        };
        Self {
            binary,
            native_tools: plugin.native_tools.clone(),
            miyu_tools: plugin.miyu_tools.clone(),
            sandbox_mode: plugin.sandbox_mode.clone(),
            ignore_user_config: plugin.ignore_user_config,
            idle_timeout: Duration::from_secs(plugin.idle_timeout_seconds.max(30)),
            instructions_dir: default_instructions_dir(),
        }
    }
}

/// `<state>/relay/codex`;拿不到 Miyu 路径时退到临时目录。
fn default_instructions_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("MIYU_CODEX_INSTRUCTIONS_DIR") {
        return PathBuf::from(dir);
    }
    crate::paths::MiyuPaths::new()
        .map(|paths| paths.state_dir.join("relay").join("codex"))
        .unwrap_or_else(|_| std::env::temp_dir().join("miyu-codex"))
}

/// 两套工具同开时从桥里剔除的 Miyu 工具。codex 原生有 shell(=run_command)、
/// web_search 与 update_plan(=todowrite);glob/grep 它靠 shell 里的 rg 做,
/// 没有独立工具,Miyu 的照留;read/edit/task/job/alarm 的保留理由同 claude 线。
pub(in crate::llm::openai_compatible) const BRIDGE_DUPLICATE_TOOLS: &[&str] =
    &["run_command", "web_search", "todowrite"];

/// 中转环境事实(声明式;常量字节保证提示词哈希稳定)。
const RELAY_ENVIRONMENT_NOTE: &str = "\n\n<relay-environment>\nThis session runs inside Miyu's relay: each turn is a fresh codex process that exits when the turn ends. Anything backgrounded through the built-in shell dies with the process.\n</relay-environment>";

/// miyu 工具桥在场时的补充事实(codex 给 MCP 工具的名字是 `miyu__<name>`… 实际
/// 前缀由 codex 决定,这里按服务器名描述)。
const RELAY_MIYU_TOOLS_NOTE: &str = "\n<relay-environment-tools>\nThe tools from the `miyu` MCP server live in the persistent Miyu daemon and survive across turns: its task tool runs a background subagent that wakes a follow-up turn when it finishes, job inspects or stops those, alarm schedules timed reminders, ask_question reaches the user and waits for the answer, and generate_image delivers the picture to the user.\n</relay-environment-tools>";

impl OpenAiCompatibleClient {
    pub(crate) async fn chat_codex_stream<F>(
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
            .codex
            .clone()
            .context("codex runtime was not initialized for this client")?;
        let model = self.provider.default_model.clone();
        let (system_prompt, conversation) = payload::split_system(messages);
        let workdir = crate::tools::workspace::effective_workdir();
        let miyu_session = crate::tools::workspace::try_session();
        let miyu_session = miyu_session.as_deref();
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
        let instructions = ensure_instructions_file(&runtime.instructions_dir, &prompt)?;
        let overrides = self.codex_overrides(&runtime, scopes, miyu_session, &instructions);
        let mut outcome = self
            .codex_turn(
                &runtime, &model, &workdir, &overrides, &plan, request_id, on_chunk,
            )
            .await;
        if let Err(error) = &outcome {
            if plan.resume_id().is_some() && stream::resume_lost(error) {
                plan.resume_lost("codex", request_id, error);
                outcome = self
                    .codex_turn(
                        &runtime, &model, &workdir, &overrides, &plan, request_id, on_chunk,
                    )
                    .await;
            }
        }
        let outcome = outcome?;
        plan.record(&outcome);
        Ok(outcome.result)
    }

    #[allow(clippy::too_many_arguments)]
    async fn codex_turn<F>(
        &self,
        runtime: &CodexRuntime,
        model: &str,
        workdir: &std::path::Path,
        overrides: &[String],
        plan: &ResumePlan,
        request_id: &str,
        on_chunk: &mut F,
    ) -> Result<RelayOutcome>
    where
        F: FnMut(ChatStreamChunk) -> Result<()>,
    {
        let payload = render_prompt(plan.delta());
        let args = codex_args(
            runtime,
            model,
            workdir,
            overrides,
            plan.resume_id(),
            plan.ephemeral(),
        );
        crate::llm::request_log::record(
            &self.provider.id,
            model,
            "codex",
            self.request_scope,
            &runtime.binary.display().to_string(),
            &json!({ "args": args, "stdin": payload, "conversation": plan.conversation() }),
        );
        stream::run_codex_turn(
            runtime,
            workdir,
            &args,
            &payload,
            plan.resume_id(),
            request_id,
            on_chunk,
        )
        .await
    }

    /// 逐进程的 `-c` 配置覆盖(TOML 值)。
    fn codex_overrides(
        &self,
        runtime: &CodexRuntime,
        scopes: ToolScopes,
        miyu_session: Option<&str>,
        instructions: &std::path::Path,
    ) -> Vec<String> {
        let mut overrides = vec![
            // 人格整体替换内置指令;工作区里的 AGENTS.md 不进上下文。
            format!(
                "model_instructions_file={}",
                toml_string(&instructions.display().to_string())
            ),
            "project_doc_max_bytes=0".to_string(),
            // 无头模式没有审批交互;思考摘要要,终端的思考通道靠它。
            "approval_policy=\"never\"".to_string(),
            "model_reasoning_summary=\"auto\"".to_string(),
            "hide_agent_reasoning=false".to_string(),
        ];
        if let Some((_, variant)) = self.selected_reasoning_variant() {
            if let crate::models_cache::ReasoningSetting::Effort(effort) = variant.setting {
                overrides.push(format!("model_reasoning_effort={}", toml_string(&effort)));
            }
        }
        if !scopes.native_on {
            // codex 没有工具白名单,只能按功能开关尽力关:shell 与网页搜索。
            overrides.push("features.shell_tool=false".to_string());
            overrides.push("features.unified_exec=false".to_string());
            overrides.push("web_search=\"disabled\"".to_string());
        }
        if scopes.miyu_on {
            if let Some(entry) = bridge_overrides(scopes.native_on, miyu_session) {
                overrides.extend(entry);
            }
        }
        if !runtime.sandbox_mode.trim().is_empty() && runtime.sandbox_mode != "danger-full-access" {
            overrides.push(format!(
                "sandbox_mode={}",
                toml_string(runtime.sandbox_mode.trim())
            ));
        }
        overrides
    }
}

fn codex_args(
    runtime: &CodexRuntime,
    model: &str,
    workdir: &std::path::Path,
    overrides: &[String],
    resume: Option<&str>,
    ephemeral: bool,
) -> Vec<String> {
    let mut args: Vec<String> = vec![
        "exec".into(),
        "--json".into(),
        "--skip-git-repo-check".into(),
        "-C".into(),
        workdir.display().to_string(),
        "-m".into(),
        model.to_string(),
    ];
    if runtime.ignore_user_config {
        args.push("--ignore-user-config".into());
    }
    if runtime.sandbox_mode.trim() == "danger-full-access" {
        args.push("--dangerously-bypass-approvals-and-sandbox".into());
    }
    if ephemeral {
        args.push("--ephemeral".into());
    }
    for item in overrides {
        args.push("-c".into());
        args.push(item.clone());
    }
    if let Some(resume) = resume {
        args.push("resume".into());
        args.push(resume.to_string());
    }
    // `-` = 提示词从 stdin 读。
    args.push("-".into());
    args
}

/// 指令文件按内容哈希命名:同一份提示词只落一次盘。旧文件只清**一小时前**
/// 的:目录是进程级共享的,别的会话/辅助请求(不同人格、不带环境事实)刚写的
/// 那份可能正被一个还没拉起的 codex 引用,立刻删会让它对着不存在的文件启动
/// (评审 09-03)。
pub(in crate::llm::openai_compatible) fn ensure_instructions_file(
    dir: &std::path::Path,
    prompt: &str,
) -> Result<PathBuf> {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    prompt.hash(&mut hasher);
    let name = format!("instructions-{:016x}.md", hasher.finish());
    let path = dir.join(&name);
    if std::fs::read_to_string(&path).ok().as_deref() == Some(prompt) {
        return Ok(path);
    }
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, prompt).with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, &path).with_context(|| format!("renaming into {}", path.display()))?;
    if let Ok(entries) = std::fs::read_dir(dir) {
        let stale_after = std::time::Duration::from_secs(60 * 60);
        for entry in entries.flatten() {
            let file = entry.file_name();
            let file = file.to_string_lossy();
            if !(file.starts_with("instructions-") && file.ends_with(".md")) || file == name {
                continue;
            }
            let stale = entry
                .metadata()
                .and_then(|meta| meta.modified())
                .ok()
                .and_then(|modified| modified.elapsed().ok())
                .is_some_and(|age| age > stale_after);
            if stale {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }
    Ok(path)
}

/// MCP 桥条目(逐进程 `-c`):codex 给 MCP 服务器的环境要显式列——会话身份、
/// 去重名单、home/runtime 识别变量(如实透传)。没有会话作用域就不挂桥。
fn bridge_overrides(exclude_duplicates: bool, miyu_session: Option<&str>) -> Option<Vec<String>> {
    let session = miyu_session?;
    let exe = crate::paths::miyu_executable().ok()?;
    let origin = serde_json::to_string(&crate::tools::workspace::current_turn_origin()).ok()?;
    let mut env: Vec<(String, String)> = vec![
        ("MIYU_SESSION".into(), session.to_string()),
        ("MIYU_TURN_ORIGIN".into(), origin),
    ];
    if exclude_duplicates {
        env.push(("MIYU_MCP_EXCLUDE".into(), BRIDGE_DUPLICATE_TOOLS.join(",")));
    }
    env.extend(cli_relay::bridge_env_passthrough());
    let env_table = env
        .iter()
        .map(|(key, value)| format!("{key}={}", toml_string(value)))
        .collect::<Vec<_>>()
        .join(",");
    Some(vec![
        format!(
            "mcp_servers.miyu.command={}",
            toml_string(&exe.display().to_string())
        ),
        "mcp_servers.miyu.args=[\"mcp-serve\"]".to_string(),
        format!("mcp_servers.miyu.env={{{env_table}}}"),
        // 桥问答要等人回答,默认 60s 等不起。
        "mcp_servers.miyu.tool_timeout_sec=1800".to_string(),
        "mcp_servers.miyu.startup_timeout_sec=30".to_string(),
    ])
}

/// TOML 基本字符串:双引号、反斜杠、控制字符转义。
pub(in crate::llm::openai_compatible) fn toml_string(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for ch in value.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04X}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// stdin 提示词正文:历史转写块 + 活跃尾巴,纯文本。codex 只从 stdin 收文本;
/// 图片可经 `-i` 传文件(第二阶段),此处先降级成占位。
fn render_prompt(delta: &[ChatMessage]) -> String {
    let mut parts: Vec<String> = Vec::new();
    for block in payload::render_user_blocks(delta) {
        match block.get("type").and_then(Value::as_str) {
            Some("text") => {
                if let Some(text) = block.get("text").and_then(Value::as_str) {
                    if !text.is_empty() {
                        parts.push(text.to_string());
                    }
                }
            }
            Some("image") => {
                parts.push("[image omitted: the codex relay accepts text only]".into())
            }
            _ => {}
        }
    }
    let mut text = parts.join("\n\n");
    if text.trim().is_empty() {
        text = "(continue)".into();
    }
    text.push('\n');
    text
}

/// 清空 Miyu 会话时的联动:删 codex 侧的 rollout
/// (`$CODEX_HOME/sessions/YYYY/MM/DD/rollout-<时间>-<thread_id>.jsonl`)。
pub(in crate::llm::openai_compatible) fn remove_rollout(thread_id: &str) {
    if thread_id.is_empty()
        || !thread_id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-')
    {
        return;
    }
    let home = std::env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".codex")));
    let Some(home) = home else {
        return;
    };
    let sessions = home.join("sessions");
    let suffix = format!("-{thread_id}.jsonl");
    let mut removed = false;
    for year in std::fs::read_dir(&sessions).into_iter().flatten().flatten() {
        for month in std::fs::read_dir(year.path())
            .into_iter()
            .flatten()
            .flatten()
        {
            for day in std::fs::read_dir(month.path())
                .into_iter()
                .flatten()
                .flatten()
            {
                for file in std::fs::read_dir(day.path())
                    .into_iter()
                    .flatten()
                    .flatten()
                {
                    let name = file.file_name();
                    if name.to_string_lossy().ends_with(&suffix) {
                        removed |= std::fs::remove_file(file.path()).is_ok();
                    }
                }
            }
        }
    }
    if removed {
        tracing::info!(
            thread_id,
            "removed the codex-side rollout for a cleared Miyu session"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn toml_strings_escape_quotes_and_backslashes() {
        assert_eq!(toml_string(r#"a"b\c"#), r#""a\"b\\c""#);
        assert_eq!(toml_string("x\ny"), "\"x\\ny\"");
    }

    #[test]
    fn instructions_file_is_content_addressed_and_pruned() {
        let dir = tempfile::tempdir().unwrap();
        let one = ensure_instructions_file(dir.path(), "one").unwrap();
        assert_eq!(std::fs::read_to_string(&one).unwrap(), "one");
        let again = ensure_instructions_file(dir.path(), "one").unwrap();
        assert_eq!(one, again);
        let two = ensure_instructions_file(dir.path(), "two").unwrap();
        assert!(
            one.exists(),
            "刚写的指令文件可能正被别的回合引用,不能立刻删"
        );
        assert!(two.exists());
        // 一小时前的才清。
        let old = std::time::SystemTime::now() - std::time::Duration::from_secs(2 * 60 * 60);
        std::fs::File::options()
            .write(true)
            .open(&one)
            .unwrap()
            .set_modified(old)
            .unwrap();
        ensure_instructions_file(dir.path(), "three").unwrap();
        assert!(!one.exists(), "过期的旧指令文件应被清掉");
        assert!(two.exists());
    }

    #[test]
    fn rollout_cleanup_refuses_path_like_ids() {
        remove_rollout("../x");
        remove_rollout("");
    }

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
                "去重名单里的 {name} 不是任何已注册工具"
            );
        }
    }
}
