use super::subagent_runner::{ProgressMode, SubagentProgress, SubagentRunner, SubagentStats};
use super::{ToolRegistry, ToolSpec};
use crate::agent::AgentMode;
use crate::config::{AppConfig, ModelTier};
use crate::llm::OpenAiCompatibleClient;
use crate::paths::MiyuPaths;
use anyhow::{bail, Result};
use serde_json::{json, Value};

const SUBAGENT_SYSTEM_PROMPT: &str = include_str!("../prompts/subagent-general.md");

/// dev 子代理的系统提示词由三段拼成:用户的 dev 提示词(与 dev 会话同一份
/// 真相源)、主机环境块、这一句交付约定。三段都是同一会话内的常量,拼出的
/// 前缀字节稳定,多次 dev 子代理之间照样命中供应商缓存。
///
/// 约定只留一句:主体布置任务时会把目标写进 prompt,但「回话对象是主 agent
/// 而不是用户、没有第二轮」这件事它自己看不出来——dev 提示词里也没有。
const SUBAGENT_DEV_CONTRACT: &str = "Your reply goes back to the agent that delegated this task, not to a user, and there is no second round: finish the work yourself and end with what you did, what the result was, and anything the caller must know.";

/// 子代理不再分类(08-17):任务由主体布置,工具就沿用主体的目录。
/// 原来的 explore 是一份硬白名单(read_file/glob/grep/check_os_info/
/// read_clipboard/web_fetch/web_search),而 dev 目录根本不注册前五个——
/// dev 下的 explore 只剩 web 两件套,描述却还在承诺 7 个工具。分类本身
/// 就是这类漂移的来源,连同 275 字符的 subagent_type 参数一起退场。
///
/// 递归防护保留:这份排除表继续把 subagent/deep_research、技能创作、闹钟和
/// 娱乐类工具挡在子代理之外。
pub(in crate::tools) const SUBAGENT_EXCLUDED: &[&str] = &[
    "subagent",
    // 09-11 改名前的旧名,按名匹配的排除表留着不花钱。
    "task",
    "task_agent",
    "send_subagent_message",
    "deep_research",
    "load_skill",
    "manage_skill",
    "alarm",
    "use_meme",
    "manage_meme",
    "generate_image",
    "print_image",
    "search_web_images",
    "divine",
];

const SUBAGENT_TOOL_TIMEOUT: u64 = 120;

#[derive(Clone)]
struct SubagentContext {
    config: AppConfig,
    paths: MiyuPaths,
    tools: ToolRegistry,
}

pub fn register(
    registry: &mut ToolRegistry,
    config: AppConfig,
    paths: MiyuPaths,
    tools: ToolRegistry,
) {
    let context = SubagentContext {
        config,
        paths,
        tools,
    };
    registry.register(ToolSpec::new_with_progress(
        "subagent",
        "Launch a subagent to handle a complex task independently. The subagent has its own system prompt, tool set, and LLM loop, and returns its final text to the main agent. Set dev=true for coding work.",
        json!({
            "type": "object",
            "properties": {
                "description": {
                    "type": "string",
                    "description": "Short task description for progress display."
                },
                "prompt": {
                    "type": "string",
                    "description": "Detailed task prompt. Must include full context, goals, and output requirements since the subagent has no access to the main agent's conversation history."
                },
                "dev": {
                    "type": "boolean",
                    "description": "Run the subagent in development mode: the development system prompt plus a minimal coding tool set. Turn it on for every coding task."
                },
                "max_steps": {
                    "type": "integer",
                    "description": "Optional tool-call budget. Unlimited by default: the subagent ends when the task is done. Set a number only when you want a hard cap."
                },
                "background": {
                    "type": "boolean",
                    "description": "Run the subagent detached in the background: returns a job_id immediately; check with job(action=status) (its log holds live progress) and you are woken automatically on completion. Use for long research/tasks that should not block the conversation."
                },
                "resume_id": {
                    "type": "string",
                    "description": "Optional. When a previous task failed with a resume_id in its error, pass it here to continue that subagent from its last completed tool round instead of starting over (process-local; lost on restart)."
                },
                "tier": {
                    "type": "string",
                    "enum": ["lite", "cheap", "standard", "flagship"],
                    "description": "Optional model tier by task difficulty: lite for trivial lookups and formatting, cheap for simple tool-using work, standard for regular multi-step work (default), flagship for hard reasoning. Every tier has the full tool set; an unconfigured tier falls back to the main model."
                }
            },
            "required": ["description", "prompt"],
            "additionalProperties": false
        }),
        move |args, progress| {
            let context = context.clone();
            async move { run_subagent(args, context, progress).await }
        },
    ).writes());

    // 给正在运行的后台子代理发一条 follow-up 排队指令(像给主会话排队消息),
    // 子代理下一步开始前取走、并入对话——用于运行途中调整任务目标。
    registry.register(ToolSpec::new(
        "send_subagent_message",
        "Queue a follow-up instruction to a RUNNING background subagent (one you started with task(background=true)). It works like queuing a message to the main agent mid-run: the subagent picks it up before its next step, so you can steer or adjust its goal while it works. Pass the job_id from the background task's result. Only works while that subagent is still running.",
        json!({
            "type": "object",
            "properties": {
                "job_id": {
                    "type": "string",
                    "description": "The background subagent's job_id, from the task(background=true) result."
                },
                "message": {
                    "type": "string",
                    "description": "The follow-up instruction to inject into the running subagent."
                }
            },
            "required": ["job_id", "message"],
            "additionalProperties": false
        }),
        move |args| async move { send_subagent_message(args) },
    ));
}

fn send_subagent_message(args: Value) -> Result<String> {
    let job_id = args
        .get("job_id")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim()
        .to_string();
    let message = args
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim()
        .to_string();
    if job_id.is_empty() {
        bail!("job_id is required (the background subagent's id from the task result)");
    }
    if message.is_empty() {
        bail!("message is required");
    }
    if crate::tools::subagent_runner::deliver_to_subagent(&job_id, &message) {
        Ok(serde_json::to_string_pretty(&json!({
            "ok": true,
            "job_id": job_id,
            "queued": message,
            "note": "The subagent will incorporate this before its next step."
        }))?)
    } else {
        let running = crate::tools::subagent_runner::running_subagent_ids();
        let hint = if running.is_empty() {
            "no background subagent is running right now".to_string()
        } else {
            format!("running background subagents: {}", running.join(", "))
        };
        bail!(
            "no running background subagent with job_id '{job_id}' (it may have already finished). {hint}"
        )
    }
}

#[derive(Clone)]
struct SubagentParams {
    description: String,
    prompt: String,
    resume_id: Option<String>,
    max_steps: usize,
    tier: ModelTier,
    dev: bool,
}

/// Session linkage captured while still inside the turn scope — a detached
/// background subagent loses the task-locals, so the audit anchor must be
/// resolved before spawning.
#[derive(Clone)]
struct AuditAnchor {
    parent: Option<String>,
    persona: String,
}

fn parse_params(args: &Value) -> Result<SubagentParams> {
    let description = args
        .get("description")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim()
        .to_string();
    if description.is_empty() {
        bail!("description is required");
    }
    let prompt = args
        .get("prompt")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim()
        .to_string();
    if prompt.is_empty() {
        bail!("prompt is required");
    }
    let resume_id = args
        .get("resume_id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .map(str::to_string);
    // 0 = 不限步数(runner 语义):默认让子代理自然结束,预算仅在调用方
    // 显式给出 max_steps 时生效。
    let max_steps = args
        .get("max_steps")
        .and_then(Value::as_u64)
        .map(|v| v as usize)
        .unwrap_or(0);
    let tier = args
        .get("tier")
        .and_then(Value::as_str)
        .and_then(ModelTier::from_str)
        .unwrap_or(ModelTier::Standard);
    let dev = args.get("dev").and_then(Value::as_bool).unwrap_or(false);
    Ok(SubagentParams {
        description,
        prompt,
        resume_id,
        max_steps,
        tier,
        dev,
    })
}

async fn run_subagent(
    args: Value,
    context: SubagentContext,
    progress: crate::tools::ToolProgress,
) -> Result<String> {
    let params = parse_params(&args)?;
    let anchor = AuditAnchor {
        parent: crate::tools::workspace::try_session().map(|session| session.to_string()),
        persona: context.config.active_persona_scope(),
    };
    if args
        .get("background")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        return spawn_background(context, params, anchor, progress).await;
    }
    // 前台子代理阻塞在本次调用里,主体无从中途插话,不开收件箱(None)。
    Ok(run_core(context, progress, params, anchor, None)
        .await?
        .output)
}

/// 一次子代理运行的结果。
///
/// `state` 以前是后台路径把 `output` 当 JSON 反解出来的——而 08-21 的
/// token-diet 把成功路径改成了纯文本,那次反解从此永远失败、悄悄退化成
/// "completed",`budget_reached` 被当成正常完成上报。现在直接带出来。
struct SubagentRun {
    output: String,
    state: &'static str,
}

/// 回合作用域(沙盒策略、工作区、会话身份)不跟着 `tokio::spawn` 走:后台
/// 子代理起在一条新任务上,task-local 到那边全是空的。后果不是显示问题
/// ——成员回合的后台子代理会跑在 Landlock 之外,工具的工作目录也退回
/// daemon 的 cwd。在还看得见的地方抓下来,进了后台原样套回去。
async fn with_turn_scope<F>(
    sandbox: Option<std::sync::Arc<crate::tools::sandbox::SandboxPolicy>>,
    workspace: Option<std::path::PathBuf>,
    session: Option<std::sync::Arc<str>>,
    future: F,
) -> F::Output
where
    F: std::future::Future + Send + 'static,
    F::Output: Send + 'static,
{
    let mut future: std::pin::Pin<Box<dyn std::future::Future<Output = F::Output> + Send>> =
        Box::pin(future);
    if let Some(session) = session {
        future = Box::pin(crate::tools::workspace::with_session(session, future));
    }
    if let Some(workspace) = workspace {
        future = Box::pin(crate::tools::workspace::with_workspace(workspace, future));
    }
    // `None` 也照样套:显式「这一段没有策略」与回合里的语义一致。
    crate::tools::sandbox::with_sandbox(sandbox, future).await
}

/// Detach the subagent run behind the shared background-job registry: its
/// progress streams into the job log, and completion goes through the same
/// wake path as background commands.
async fn spawn_background(
    context: SubagentContext,
    params: SubagentParams,
    anchor: AuditAnchor,
    progress: crate::tools::ToolProgress,
) -> Result<String> {
    let description = params.description.clone();
    // 后台子代理起在 tokio::spawn 的新任务上,回合的 task-local(工作区/会话/
    // 沙盒)到那儿全空了:相对路径退回 daemon 的 cwd、Landlock 失效、且
    // mcp_bridge_config 因 try_session()=None 返回 None(claude-code 拿不到 Miyu 桥)。
    // 在还处于父回合作用域的此刻抓下来,由 with_turn_scope 在 spawn 里套回去。
    let sandbox = crate::tools::sandbox::current_sandbox();
    let workspace = crate::tools::workspace::try_workspace();
    let session = crate::tools::workspace::try_session();
    crate::tools::jobs::spawn_background_subagent(
        None,
        &description,
        &progress,
        move |job_id, log_path| async move {
            let bridge = spawn_subagent_log_bridge(job_id.clone(), log_path.clone());
            // 后台子代理:用后台任务 id 作收件箱键,主体可用 send_subagent_message
            // 中途投递 follow-up;主体从后台返回里拿到这个 job_id。工作区/会话/沙盒
            // 由 with_turn_scope 套回(见上)。
            let run = with_turn_scope(
                sandbox,
                workspace,
                session,
                run_core(context, bridge, params, anchor, Some(job_id.clone())),
            )
            .await;
            let state_label = match &run {
                Ok(run) => run.state,
                Err(_) => "error",
            };
            let tail = match &run {
                Ok(run) => format!(
                    "\n{}\n{}\n",
                    crate::tools::jobs::SUBAGENT_RESULT_MARKER,
                    run.output
                ),
                Err(error) => format!("\n{}\n{error}\n", crate::tools::jobs::SUBAGENT_ERROR_MARKER),
            };
            let _ = std::fs::OpenOptions::new()
                .append(true)
                .open(&log_path)
                .and_then(|mut file| {
                    use std::io::Write as _;
                    file.write_all(tail.as_bytes())
                });
            tracing::debug!(job_id = %job_id, state = %state_label, "background subagent finished");
            match state_label {
                "completed" | "budget_reached" => {
                    crate::tools::jobs::JobState::Exited { code: Some(0) }
                }
                "timeout" => crate::tools::jobs::JobState::TimedOut,
                _ => crate::tools::jobs::JobState::Exited { code: None },
            }
        },
    )
    .await
}

/// Bridge a detached subagent's progress stream into its job log so
/// `job_status` reads live progress the same way it reads command output.
fn spawn_subagent_log_bridge(
    job_id: String,
    log_path: std::path::PathBuf,
) -> crate::tools::ToolProgress {
    let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        while let Some(event) = receiver.recv().await {
            let crate::tools::ToolProgressEvent::Message(message) = event else {
                continue;
            };
            // 原始标记上 SSE(网页端据 job_id 渲染子过程流,与前台子代理工具行
            // 同款);人读的行落任务日志(job status 读它)。
            crate::tools::jobs::publish_job_progress(&job_id, &message);
            let line = readable_subagent_log_line(&message);
            if line.is_empty() {
                continue;
            }
            let _ = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&log_path)
                .and_then(|mut file| {
                    use std::io::Write as _;
                    writeln!(file, "{line}")
                });
        }
    });
    crate::tools::ToolProgress::new(sender)
}

fn readable_subagent_log_line(message: &str) -> String {
    if let Some(text) = message.strip_prefix("__subagent_reasoning__") {
        let text = text.trim();
        if text.is_empty() {
            return String::new();
        }
        return format!("[思考] {text}");
    }
    if let Some(text) = message.strip_prefix("__subtool_call__") {
        return format!("[工具] {}", text.trim());
    }
    if let Some(text) = message.strip_prefix("__subtool_result__") {
        return format!("[结果] {}", text.trim());
    }
    if let Some(text) = message.strip_prefix("__subagent_stats__") {
        return format!("[统计] {}", text.trim());
    }
    message.trim().to_string()
}

/// dev 子代理的系统提示词。
///
/// 第一段是用户自己的 dev 提示词(`dev-prompt.md`,与 dev 会话读同一份),
/// 改它对子代理同时生效。第二段是主体也在用的主机环境块——子代理没有
/// 每轮瞬态尾巴,工作目录只能从这里知道,否则第一步永远浪费在 `pwd` 上。
/// 末尾是那句交付约定。
///
/// 三段在一个会话里都是常量(工作目录跟着会话工作区走),多次 dev 子代理
/// 之间前缀缓存照样命中。
fn build_dev_system_prompt(config: &AppConfig, paths: &MiyuPaths) -> Result<String> {
    let mut prompt = config.dev_system_prompt(paths)?;
    prompt.push_str("\n\n");
    prompt.push_str(&crate::agent::prompt::host_environment_for(config, paths));
    prompt.push_str(&format!(
        "\n<runtime cwd=\"{}\"/>",
        crate::host_info::xml_attr_escape(
            &crate::tools::workspace::effective_workdir()
                .display()
                .to_string()
        )
    ));
    prompt.push_str("\n\n");
    prompt.push_str(SUBAGENT_DEV_CONTRACT);
    Ok(prompt)
}

async fn run_core(
    context: SubagentContext,
    progress: crate::tools::ToolProgress,
    params: SubagentParams,
    anchor: AuditAnchor,
    inbox_id: Option<String>,
) -> Result<SubagentRun> {
    let SubagentParams {
        description,
        prompt,
        resume_id,
        max_steps,
        tier,
        dev,
    } = params;
    let tool_timeout = SUBAGENT_TOOL_TIMEOUT;

    // WebUI 回合(既非终端、也非平台:没有 origin tty、没有平台 sender)一律用
    // Full 档发子过程标记(思考 + 结构化工具调用/结果),网页端据此把展开后的
    // 子过程时间线画成「思考+工具流」——和主智能体过程区同款(09-11 用户要求)。
    // 网页端默认收起这些,静息态不吵;终端/平台仍按 display.tool_calls 配置,
    // 免得 Summary 档的终端用户突然被子代理的全量嵌套刷屏。
    let is_webui_turn = crate::tools::workspace::current_origin_tty().is_none()
        && crate::tools::workspace::current_platform_sender().is_none();
    let mode = if is_webui_turn {
        ProgressMode::Full
    } else {
        ProgressMode::from_config(&context.config)
    };
    let enabled = context.config.plugins.deep_research.show_progress;
    let sa_progress = SubagentProgress::new(progress, mode, enabled);

    // dev 子代理 = 开发模式的三件套,与 dev 会话同源:保留人格 "dev" 的
    // 作用域(记忆整套关)、那份 core_only 的工具面、以及中转线的 dev 工具
    // 作用域。少任何一件都会漂移成「名字叫 dev、其实是普通子代理」。
    let config = if dev {
        context.config.dev_scoped()
    } else {
        context.config.clone()
    };

    // Tier routing: the tier's pool gets its own load-balanced client;
    // an unconfigured pool silently uses the main model pool, and a
    // configured-but-unusable pool falls back with a notice returned to
    // the calling agent (not printed to the user). The fallback contract
    // lives in `from_tier` so auxiliary roles share it byte for byte.
    let routed = OpenAiCompatibleClient::from_tier(&config, &context.paths, tier)?;
    let tier_notice = routed.notice;
    let model_choice = routed.model_choice;
    let client = routed
        .client
        .with_request_scope("subagent")
        .with_claude_code_dev_mode(dev)
        .for_subagent_output(mode == ProgressMode::Full);
    // 普通子代理沿用主体目录:任务是主体布置的,分类只会让"承诺的工具"
    // 与"实际注册的工具"漂移(dev 下的旧 explore 就是这么坏掉的)。
    // dev 子代理反过来:它的任务与主体人格无关,拿的就是 dev 会话那张面,
    // 现造而不是注册时造——注册发生在 `compose_registry` 里,在那儿造 dev
    // 面会自己套自己。
    let tools = if dev {
        crate::tools::build_tool_registry(&config, &context.paths, AgentMode::Dev, false)?
    } else {
        context.tools.clone()
    };

    let system_prompt = if dev {
        build_dev_system_prompt(&config, &context.paths)?
    } else {
        SUBAGENT_SYSTEM_PROMPT.to_string()
    };

    let runner = SubagentRunner::new(client, system_prompt, tools, sa_progress)
        .max_steps(max_steps)
        .timeout_seconds(tool_timeout)
        .excluded_tools(SUBAGENT_EXCLUDED)
        .inbox_id(inbox_id.clone());

    // 后台子代理开收件箱:主体可在运行途中投递 follow-up(见 subagent_runner)。
    // 用 drop guard 关箱,覆盖所有退出路径(正常返回 / `?` 早退 / panic)。
    struct InboxGuard(Option<String>);
    impl Drop for InboxGuard {
        fn drop(&mut self) {
            if let Some(id) = &self.0 {
                crate::tools::subagent_runner::close_subagent_inbox(id);
            }
        }
    }
    if let Some(id) = &inbox_id {
        crate::tools::subagent_runner::open_subagent_inbox(id);
    }
    let _inbox_guard = InboxGuard(inbox_id.clone());

    // 子代理不设总时长上限:它自然结束于任务完成或步数预算;逐工具超时
    // (tool_timeout)仍然兜底单步挂死。
    // 标记「在子代理里」:vision_analyze 据此走旁路转写而非 inline 寄存
    // (子代理循环不接力 inline 媒体,见 workspace::in_subagent)。
    let (result, stats) = match crate::tools::workspace::with_subagent(
        runner.run_with_resume(&prompt, resume_id.as_deref()),
    )
    .await
    {
        Ok((result, stats)) => (result, stats),
        Err(err) => {
            let output = serde_json::to_string_pretty(&json!({
                "ok": false,
                "kind": "subagent",
                "tier": tier.label(),
                "tier_notice": tier_notice,
                "description": description,
                "state": "error",
                "error": err.to_string(),
                "stats": SubagentStats::default().public(),
            }))?;
            record_subagent_audit(
                &context,
                &anchor,
                &description,
                &prompt,
                &output,
                None,
                &model_choice,
            );
            return Ok(SubagentRun {
                output,
                state: "error",
            });
        }
    };

    let state = if stats.budget_reached {
        "budget_reached"
    } else {
        "completed"
    };

    let final_text = result.content.trim().to_string();

    // 08-21 token-diet:成功路径改文本形态——子代理结论不再被 JSON 转义
    // (换行/引号转义在长结论上是实打实的浪费)。result: 之后到结尾都是
    // 结论本体,tool_report.rs 的持久化提取按此约定解析;错误路径保留
    // ok:false JSON(成败判定的结构即功能)。
    let mut output = format!("subagent {state} (tier {}): {description}\n", tier.label());
    if let Some(notice) = &tier_notice {
        output.push_str(notice);
        output.push('\n');
    }
    output.push_str(&format!(
        "stats: {}\n",
        serde_json::to_string(&stats.public())?
    ));
    output.push_str("result:\n");
    output.push_str(&final_text);
    // Prefer the endpoint that actually produced the final reply (pools
    // load-balance, so the representative pool entry may differ).
    let model_choice = match (&result.provider_id, &result.model) {
        (Some(provider_id), Some(model)) => Some((provider_id.clone(), model.clone())),
        _ => model_choice,
    };
    record_subagent_audit(
        &context,
        &anchor,
        &description,
        &prompt,
        &output,
        Some(&stats),
        &model_choice,
    );
    Ok(SubagentRun { output, state })
}

/// Persists an audit session for a subagent run: a hidden `kind='subagent'`
/// session linked to the parent turn's session, holding one turn (prompt →
/// result JSON) plus the model identity and token usage on the session row.
/// Best-effort: audit failures never fail the task itself.
fn record_subagent_audit(
    context: &SubagentContext,
    anchor: &AuditAnchor,
    description: &str,
    prompt: &str,
    output: &str,
    stats: Option<&SubagentStats>,
    model_choice: &Option<(String, String)>,
) {
    let outcome = (|| -> Result<()> {
        let store = crate::state::StateStore::new(&context.paths)?;
        let parent = anchor.parent.clone();
        let persona = anchor.persona.clone();
        let name: String = description.chars().take(40).collect();
        let record = store.create_session(&persona, &name, "subagent", parent.as_deref())?;
        let pinned = store.pinned(&record.session_id);
        let turn_id = format!(
            "sat_{}_{:08x}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|duration| duration.as_millis())
                .unwrap_or(0),
            rand::random::<u32>()
        );
        pinned.start_turn(&turn_id, prompt, std::process::id())?;
        pinned.complete_turn(&turn_id, output, None)?;
        let (provider_id, model) = match model_choice.as_ref() {
            Some((provider_id, model)) => (Some(provider_id.as_str()), Some(model.as_str())),
            None => (None, None),
        };
        let context_window = match (provider_id, model) {
            (Some(provider), Some(model)) => context
                .config
                .context_window_for_provider_model(provider, model)
                .ok()
                .flatten()
                .map(|window| window as i64),
            _ => None,
        };
        let (prompt_tokens, completion_tokens, total_tokens, cache_read_tokens) = match stats {
            Some(stats) => (
                stats.prompt_tokens as i64,
                stats.completion_tokens as i64,
                stats.total_tokens.max(stats.token_estimate) as i64,
                stats.cache_read_tokens as i64,
            ),
            None => (0, 0, 0, 0),
        };
        store.record_subagent_usage(
            &record.session_id,
            provider_id,
            model,
            context_window,
            prompt_tokens,
            completion_tokens,
            total_tokens,
            cache_read_tokens,
        )
    })();
    if let Err(error) = outcome {
        tracing::warn!(error = %error, "{}", crate::i18n::text("failed to record subagent audit session", "记录子代理审计会话失败"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_paths(root: &std::path::Path) -> MiyuPaths {
        crate::tools::tests::test_paths(root)
    }

    #[test]
    fn dev_flag_defaults_to_off_and_parses() {
        let base = json!({"description": "d", "prompt": "p"});
        assert!(!parse_params(&base).unwrap().dev);
        let mut with_dev = base.clone();
        with_dev["dev"] = json!(true);
        assert!(parse_params(&with_dev).unwrap().dev);
    }

    /// dev 子代理的系统提示词是三段拼起来的,少任何一段它都得先浪费一轮
    /// 去问「我在哪、说给谁听」。
    #[test]
    fn dev_system_prompt_carries_the_dev_prompt_host_block_and_contract() {
        let temp = tempfile::tempdir().unwrap();
        let paths = test_paths(temp.path());
        let config = AppConfig::default();
        let prompt = build_dev_system_prompt(&config, &paths).unwrap();
        assert!(
            prompt.starts_with(crate::config::DEFAULT_DEV_SYSTEM_PROMPT),
            "{prompt}"
        );
        assert!(prompt.contains("<host-environment"), "{prompt}");
        assert!(prompt.contains("<runtime cwd="), "{prompt}");
        assert!(prompt.ends_with(SUBAGENT_DEV_CONTRACT), "{prompt}");
    }

    /// 同一个会话里连开两个 dev 子代理,系统提示词必须逐字节相同——不然
    /// 每一个都是一次冷前缀。
    #[test]
    fn dev_system_prompt_is_byte_stable_within_a_session() {
        let temp = tempfile::tempdir().unwrap();
        let paths = test_paths(temp.path());
        let config = AppConfig::default();
        assert_eq!(
            build_dev_system_prompt(&config, &paths).unwrap(),
            build_dev_system_prompt(&config, &paths).unwrap()
        );
    }

    /// 递归防护:dev 子代理拿的是 dev 会话那张面,而那张面里也注册着
    /// `subagent`——排除表必须认得新名,否则子代理能自己再开子代理。
    #[test]
    fn subagent_excludes_itself_by_its_current_name() {
        assert!(SUBAGENT_EXCLUDED.contains(&"subagent"));
    }

    /// 后台子代理起在 `tokio::spawn` 的新任务上,回合的 task-local 到那儿
    /// 全空了:成员的后台子代理会因此跑在 Landlock 之外,工具的工作目录
    /// 也退回 daemon 的 cwd。这条钉住「抓下来再套回去」。
    #[tokio::test]
    async fn background_scope_is_carried_across_the_spawn() {
        let workspace = std::path::PathBuf::from("/tmp/miyu-subagent-scope");
        let session: std::sync::Arc<str> = "sess_probe".into();
        let (bare, restored) = crate::tools::workspace::with_workspace(
            workspace.clone(),
            crate::tools::workspace::with_session(session.clone(), async {
                let carried_workspace = crate::tools::workspace::try_workspace();
                let carried_session = crate::tools::workspace::try_session();
                tokio::spawn(async move {
                    let bare = (
                        crate::tools::workspace::try_workspace(),
                        crate::tools::workspace::try_session(),
                    );
                    let restored =
                        with_turn_scope(None, carried_workspace, carried_session, async {
                            (
                                crate::tools::workspace::try_workspace(),
                                crate::tools::workspace::try_session(),
                            )
                        })
                        .await;
                    (bare, restored)
                })
                .await
                .unwrap()
            }),
        )
        .await;
        assert_eq!(bare, (None, None), "裸 spawn 本就看不见回合作用域");
        assert_eq!(restored, (Some(workspace), Some(session)));
    }
}
