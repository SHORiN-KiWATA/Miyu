use super::subagent_runner::{ProgressMode, SubagentProgress, SubagentRunner, SubagentStats};
use super::{ToolRegistry, ToolSpec};
use crate::config::{AppConfig, ModelTier};
use crate::llm::OpenAiCompatibleClient;
use crate::paths::MiyuPaths;
use anyhow::{bail, Result};
use serde_json::{json, Value};

const SUBAGENT_SYSTEM_PROMPT: &str = include_str!("../prompts/subagent-general.md");

/// 子代理不再分类(08-17):任务由主体布置,工具就沿用主体的目录。
/// 原来的 explore 是一份硬白名单(read_file/glob/grep/check_os_info/
/// read_clipboard/web_fetch/web_search),而 dev 目录根本不注册前五个——
/// dev 下的 explore 只剩 web 两件套,描述却还在承诺 7 个工具。分类本身
/// 就是这类漂移的来源,连同 275 字符的 subagent_type 参数一起退场。
///
/// 递归防护保留:这份排除表继续把 task/deep_research、技能创作、闹钟和
/// 娱乐类工具挡在子代理之外。
pub(in crate::tools) const SUBAGENT_EXCLUDED: &[&str] = &[
    "task",
    "task_agent",
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
struct TaskContext {
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
    let context = TaskContext {
        config,
        paths,
        tools,
    };
    registry.register(ToolSpec::new_with_progress(
        "task",
        "Launch a subagent to handle a complex task independently. The subagent has its own system prompt, tool set, and LLM loop, and returns its final text to the main agent.",
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
            async move { run_task(args, context, progress).await }
        },
    ).writes());
}

#[derive(Clone)]
struct TaskParams {
    description: String,
    prompt: String,
    resume_id: Option<String>,
    max_steps: usize,
    tier: ModelTier,
}

/// Session linkage captured while still inside the turn scope — a detached
/// background subagent loses the task-locals, so the audit anchor must be
/// resolved before spawning.
#[derive(Clone)]
struct AuditAnchor {
    parent: Option<String>,
    persona: String,
}

fn parse_task_params(args: &Value) -> Result<TaskParams> {
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
    Ok(TaskParams {
        description,
        prompt,
        resume_id,
        max_steps,
        tier,
    })
}

async fn run_task(
    args: Value,
    context: TaskContext,
    progress: crate::tools::ToolProgress,
) -> Result<String> {
    let params = parse_task_params(&args)?;
    let anchor = AuditAnchor {
        parent: crate::tools::workspace::try_session().map(|session| session.to_string()),
        persona: context.config.active_persona_scope(),
    };
    if args
        .get("background")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        return spawn_background_task(context, params, anchor, progress).await;
    }
    Ok(run_task_core(context, progress, params, anchor)
        .await?
        .output)
}

/// 一次子代理运行的结果。
///
/// `state` 以前是后台路径把 `output` 当 JSON 反解出来的——而 08-21 的
/// token-diet 把成功路径改成了纯文本,那次反解从此永远失败、悄悄退化成
/// "completed",`budget_reached` 被当成正常完成上报。现在直接带出来。
struct TaskRun {
    output: String,
    state: &'static str,
}

/// Detach the subagent run behind the shared background-job registry: its
/// progress streams into the job log, and completion goes through the same
/// wake path as background commands.
async fn spawn_background_task(
    context: TaskContext,
    params: TaskParams,
    anchor: AuditAnchor,
    progress: crate::tools::ToolProgress,
) -> Result<String> {
    let description = params.description.clone();
    crate::tools::jobs::spawn_background_subagent(
        None,
        &description,
        &progress,
        move |job_id, log_path| async move {
            let bridge = spawn_subagent_log_bridge(log_path.clone());
            let run = run_task_core(context, bridge, params, anchor).await;
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
fn spawn_subagent_log_bridge(log_path: std::path::PathBuf) -> crate::tools::ToolProgress {
    let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        while let Some(event) = receiver.recv().await {
            let crate::tools::ToolProgressEvent::Message(message) = event else {
                continue;
            };
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

async fn run_task_core(
    context: TaskContext,
    progress: crate::tools::ToolProgress,
    params: TaskParams,
    anchor: AuditAnchor,
) -> Result<TaskRun> {
    let TaskParams {
        description,
        prompt,
        resume_id,
        max_steps,
        tier,
    } = params;
    let tool_timeout = SUBAGENT_TOOL_TIMEOUT;

    let mode = ProgressMode::from_config(&context.config);
    let enabled = context.config.plugins.deep_research.show_progress;
    let sa_progress = SubagentProgress::new(progress, mode, enabled);

    // Tier routing: the tier's pool gets its own load-balanced client;
    // an unconfigured pool silently uses the main model pool, and a
    // configured-but-unusable pool falls back with a notice returned to
    // the calling agent (not printed to the user). The fallback contract
    // lives in `from_tier` so auxiliary roles share it byte for byte.
    let routed = OpenAiCompatibleClient::from_tier(&context.config, &context.paths, tier)?;
    let tier_notice = routed.notice;
    let model_choice = routed.model_choice;
    let client = routed
        .client
        .with_request_scope("subagent")
        .for_subagent_output(mode == ProgressMode::Full);
    // 工具沿用主体目录:子代理的任务是主体布置的,分类只会让"承诺的工具"
    // 与"实际注册的工具"漂移(dev 下的旧 explore 就是这么坏掉的)。
    let tools = context.tools.clone();

    let runner = SubagentRunner::new(client, SUBAGENT_SYSTEM_PROMPT, tools, sa_progress)
        .max_steps(max_steps)
        .timeout_seconds(tool_timeout)
        .excluded_tools(SUBAGENT_EXCLUDED);

    // 子代理不设总时长上限:它自然结束于任务完成或步数预算;逐工具超时
    // (tool_timeout)仍然兜底单步挂死。
    let (result, stats) = match runner.run_with_resume(&prompt, resume_id.as_deref()).await {
        Ok((result, stats)) => (result, stats),
        Err(err) => {
            let output = serde_json::to_string_pretty(&json!({
                "ok": false,
                "kind": "task",
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
            return Ok(TaskRun {
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
    let mut output = format!("task {state} (tier {}): {description}\n", tier.label());
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
    Ok(TaskRun { output, state })
}

/// Persists an audit session for a subagent run: a hidden `kind='subagent'`
/// session linked to the parent turn's session, holding one turn (prompt →
/// result JSON) plus the model identity and token usage on the session row.
/// Best-effort: audit failures never fail the task itself.
fn record_subagent_audit(
    context: &TaskContext,
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
