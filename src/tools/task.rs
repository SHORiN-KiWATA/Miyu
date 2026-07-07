use super::subagent_runner::{ProgressMode, SubagentProgress, SubagentRunner, SubagentStats};
use super::{ToolRegistry, ToolSpec};
use crate::config::AppConfig;
use crate::i18n::{is_zh, text as t};
use crate::llm::OpenAiCompatibleClient;
use crate::paths::MiyuPaths;
use anyhow::{bail, Result};
use serde_json::{json, Value};
use std::time::Duration;

const EXPLORE_SYSTEM_PROMPT: &str = include_str!("../prompts/subagent-explore.md");
const GENERAL_SYSTEM_PROMPT: &str = include_str!("../prompts/subagent-general.md");

const EXPLORE_ALLOWED: &[&str] = &[
    "read_file",
    "glob",
    "grep",
    "check_os_info",
    "read_clipboard",
    "web_fetch",
    "web_search",
];

const GENERAL_EXCLUDED: &[&str] = &[
    "task",
    "task_agent",
    "deep_research",
    "linux_input_method_diagnose",
    "deep_diagnose",
    "deep_research_linux_game_compatibility",
    "load_skill",
    "set_alarm",
    "list_alarms",
    "cancel_alarm",
    "search_meme",
    "show_meme",
    "add_meme",
    "update_meme",
    "delete_meme",
    "generate_image",
    "print_image",
    "search_web_images",
    "xuanxue_pick",
    "xuanxue_divine",
    "draw_zhouyi_hexagram",
    "draw_tarot_card",
    "draw_fortune_lot",
    "roll_dice",
];

const EXPLORE_MAX_STEPS: usize = 30;
const GENERAL_MAX_STEPS: usize = 50;
const EXPLORE_TOOL_TIMEOUT: u64 = 60;
const GENERAL_TOOL_TIMEOUT: u64 = 120;
const EXPLORE_TOTAL_TIMEOUT: u64 = 300;
const GENERAL_TOTAL_TIMEOUT: u64 = 600;

#[derive(Clone)]
struct TaskContext {
    config: AppConfig,
    paths: MiyuPaths,
    tools: ToolRegistry,
}

#[derive(Clone, Copy, PartialEq)]
enum SubagentType {
    Explore,
    General,
}

impl SubagentType {
    fn from_str(s: &str) -> Self {
        match s {
            "explore" => Self::Explore,
            _ => Self::General,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Explore => "explore",
            Self::General => "general",
        }
    }

    fn system_prompt(self) -> &'static str {
        match self {
            Self::Explore => EXPLORE_SYSTEM_PROMPT,
            Self::General => GENERAL_SYSTEM_PROMPT,
        }
    }

    fn default_max_steps(self) -> usize {
        match self {
            Self::Explore => EXPLORE_MAX_STEPS,
            Self::General => GENERAL_MAX_STEPS,
        }
    }

    fn tool_timeout(self) -> u64 {
        match self {
            Self::Explore => EXPLORE_TOOL_TIMEOUT,
            Self::General => GENERAL_TOOL_TIMEOUT,
        }
    }

    fn total_timeout(self) -> u64 {
        match self {
            Self::Explore => EXPLORE_TOTAL_TIMEOUT,
            Self::General => GENERAL_TOTAL_TIMEOUT,
        }
    }
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
        t(
            "Launch a subagent to handle a complex task independently. The subagent has its own system prompt, tool set, and LLM loop, and returns its final text to the main agent.",
            "启动子代理独立处理复杂任务。子代理有独立的系统提示、工具集和 LLM 循环，完成后返回最终文本给主 agent。",
        ),
        json!({
            "type": "object",
            "properties": {
                "description": {
                    "type": "string",
                    "description": t("Short task description for progress display.", "简短任务描述，用于进度展示。")
                },
                "prompt": {
                    "type": "string",
                    "description": t("Detailed task prompt. Must include full context, goals, and output requirements since the subagent has no access to the main agent's conversation history.", "详细任务提示。必须包含完整的上下文、目标和输出要求，因为子代理无法访问主 agent 的对话历史。")
                },
                "subagent_type": {
                    "type": "string",
                    "enum": ["explore", "general"],
                    "description": t("Subagent type. explore: read-only search for codebase exploration and info gathering; general: multi-step tasks with file read/write and command execution. Defaults to general.", "子代理类型。explore：只读搜索，适合代码库探索和信息收集；general：通用多步任务，可读写文件和运行命令。默认 general。"),
                    "default": "general"
                },
                "max_steps": {
                    "type": "integer",
                    "description": t("Optional. Override the subagent's tool call budget. explore defaults to 30, general defaults to 50.", "可选。覆盖子代理的工具调用预算上限。explore 默认 30，general 默认 50。")
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

async fn run_task(
    args: Value,
    context: TaskContext,
    progress: crate::tools::ToolProgress,
) -> Result<String> {
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
    let sa_type = SubagentType::from_str(
        args.get("subagent_type")
            .and_then(Value::as_str)
            .unwrap_or("general"),
    );
    let max_steps = args
        .get("max_steps")
        .and_then(Value::as_u64)
        .map(|v| v as usize)
        .unwrap_or_else(|| sa_type.default_max_steps());
    let tool_timeout = sa_type.tool_timeout();
    let total_timeout = sa_type.total_timeout();

    let mode = ProgressMode::from_config(&context.config);
    let enabled = context.config.plugins.deep_research.show_progress;
    let sa_progress = SubagentProgress::new(progress, mode, enabled);

    sa_progress.report(if is_zh() {
        format!("{}（{}）：{}", t("subagent", "子代理"), sa_type.label(), description)
    } else {
        format!("subagent ({}): {}", sa_type.label(), description)
    });

    let client = OpenAiCompatibleClient::from_config(&context.config, &context.paths)?;
    let tools = match sa_type {
        SubagentType::Explore => context.tools.clone_filtered(EXPLORE_ALLOWED),
        SubagentType::General => context.tools.clone(),
    };

    let runner = SubagentRunner::new(client, sa_type.system_prompt(), tools, sa_progress)
        .max_steps(max_steps)
        .timeout_seconds(tool_timeout)
        .excluded_tools(if sa_type == SubagentType::General {
            GENERAL_EXCLUDED
        } else {
            &[]
        });

    let (result, stats) = match tokio::time::timeout(
        Duration::from_secs(total_timeout),
        runner.run(&prompt),
    )
    .await
    {
        Ok(Ok((result, stats))) => (result, stats),
        Ok(Err(err)) => {
            return Ok(serde_json::to_string_pretty(&json!({
                "ok": false,
                "kind": "task",
                "subagent_type": sa_type.label(),
                "description": description,
                "state": "error",
                "error": err.to_string(),
                "stats": SubagentStats::default().public(),
            }))?);
        }
        Err(_) => {
            return Ok(serde_json::to_string_pretty(&json!({
                "ok": false,
                "kind": "task",
                "subagent_type": sa_type.label(),
                "description": description,
                "state": "timeout",
                "error": format!("subagent timed out after {total_timeout}s"),
                "stats": SubagentStats::default().public(),
            }))?);
        }
    };

    let state = if stats.budget_reached {
        "budget_reached"
    } else {
        "completed"
    };

    let final_text = result.content.trim().to_string();

    Ok(serde_json::to_string_pretty(&json!({
        "ok": true,
        "kind": "task",
        "subagent_type": sa_type.label(),
        "description": description,
        "state": state,
        "result": final_text,
        "stats": stats.public(),
    }))?)
}
