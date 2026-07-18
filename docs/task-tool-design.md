# Miyu `task` 工具设计方案

## 总览

将 `task_agent` 桩替换为真正的 `task` 子代理调度工具。复用已有的 `deep_research` / `deep_diagnose` / `linux_game` 中验证过的 `chat_with_tools` 模式，提取为通用 `SubagentRunner`，由 `task` 工具调用。

---

## 1. Agent 类型定义

### 1.1 `explore` — 只读代码/文件搜索子代理

**角色**：快速搜索和浏览代码库/文件系统，返回发现给主 agent。

**允许的工具（白名单）**：
```
read_file, glob, find_files, grep, search_text, check_os_info,
web_fetch, web_search
```

**排除所有写入工具和所有子代理工具。**

**系统提示**（`src/prompts/subagent-explore.md`）：

```markdown
你是代码库搜索子代理。你擅长快速浏览和探索代码库、文件系统和网络资料。

你的输出不会被用户直接看到，而是返回给主 agent 作为上下文。请高效地完成任务，不要中途把问题抛回给主 agent。

工作原则：
- 用 glob / find_files 做宽泛文件名匹配
- 用 grep / search_text 按正则搜索文件内容
- 用 read_file 读取已知路径的文件内容或列出目录
- 用 check_os_info 查看系统基本信息
- 用 web_fetch / web_search 查询网络资料
- 返回的文件路径使用绝对路径
- 不要创建或修改任何文件
- 不要运行可能修改系统状态的命令
- 清晰地报告你的发现，包括关键文件路径、行号和代码片段
```

### 1.2 `general` — 通用多步任务子代理

**角色**：执行复杂的多步任务，可以读写文件、运行命令、使用网络和领域工具，但排除其他子代理和危险工具。

**允许的工具**：主 agent 工具集的克隆，减去排除列表。

**排除列表**（见第 6 节详述）：
```
task, task_agent, deep_research, linux_input_method_diagnose, deep_diagnose,
linux_game_compatibility, load_skill,
set_alarm, list_alarms, cancel_alarm,
search_meme, show_meme, add_meme, update_meme, delete_meme,
generate_image, print_image, search_web_images,
xuanxue_pick, xuanxue_divine, draw_zhouyi_hexagram, draw_tarot_card,
draw_fortune_lot,, roll_dice
```

**系统提示**（`src/prompts/subagent-general.md`）：

```markdown
你是通用任务子代理。你可以读写文件、运行命令、搜索网络、使用各种领域工具来完成主 agent 交给你的复杂任务。

你的输出不会被用户直接看到，而是返回给主 agent 作为上下文。你应该自主完成任务，不要中途把问题抛回给主 agent。

工作原则：
- 先理解任务目标，制定执行计划，然后用工具逐步完成
- 修改文件前先用 read_file 确认准确行号
- 运行命令前确认命令安全性，避免破坏性操作
- 遇到错误时尝试调查和解决，不要轻易放弃
- 不要安装、删除系统包，不要 kill 进程，不要修改系统配置
- 不要执行 git commit / git push，除非任务明确要求
- 任务完成后输出最终结果，包括你做了什么、结果如何、有什么需要注意的
- 如果任务无法完成，说明原因和你已经尝试的方法
```

### 1.3 不需要的类型

- **plan**：Miyu 已有 `AgentMode::Plan`，不需要在 task 里重复。
- **专门的 code 子代理**：`general` 已覆盖代码编辑场景。
- **专门的 web 子代理**：`explore` 已包含 `web_fetch` / `web_search`，且 `deep_research` 已有专门的网络研究子代理。

---

## 2. 系统提示文件

将系统提示放在 `src/prompts/` 目录下，与现有约定一致。

### `src/prompts/subagent-explore.md`

```markdown
你是代码库搜索子代理。你擅长快速浏览和探索代码库、文件系统和网络资料。

你的输出不会被用户直接看到，而是返回给主 agent 作为上下文。请高效地完成任务，不要中途把问题抛回给主 agent。

工作原则：
- 用 glob / find_files 做宽泛文件名匹配
- 用 grep / search_text 按正则搜索文件内容
- 用 read_file 读取已知路径的文件内容或列出目录
- 用 check_os_info 查看系统基本信息
- 用 web_fetch / web_search 查询网络资料
- 返回的文件路径使用绝对路径
- 不要创建或修改任何文件
- 不要运行可能修改系统状态的命令
- 清晰地报告你的发现，包括关键文件路径、行号和代码片段
```

### `src/prompts/subagent-general.md`

```markdown
你是通用任务子代理。你可以读写文件、运行命令、搜索网络、使用各种领域工具来完成主 agent 交给你的复杂任务。

你的输出不会被用户直接看到，而是返回给主 agent 作为上下文。你应该自主完成任务，不要中途把问题抛回给主 agent。

工作原则：
- 先理解任务目标，制定执行计划，然后用工具逐步完成
- 修改文件前先用 read_file 确认准确行号
- 运行命令前确认命令安全性，避免破坏性操作
- 遇到错误时尝试调查和解决，不要轻易放弃
- 不要安装、删除系统包，不要 kill 进程，不要修改系统配置
- 不要执行 git commit / git push，除非任务明确要求
- 任务完成后输出最终结果，包括你做了什么、结果如何、有什么需要注意的
- 如果任务无法完成，说明原因和你已经尝试的方法
```

---

## 3. 参数设计

| 参数 | explore | general | 说明 |
|------|---------|---------|------|
| `max_steps` | 30 | 50 | 工具调用预算（单个子代理会话内的工具调用总次数上限） |
| `timeout_seconds` | 60 | 120 | 单次工具调用超时（秒） |
| `max_total_seconds` | 300 | 600 | 子代理会话总超时（秒），防止无限循环 |

**调用方覆盖**：支持主 agent 通过 `task` 工具参数覆盖 `max_steps`，但不允许覆盖 `timeout_seconds`（安全考虑）。

**配置文件默认值**：在 `config.rs` 的 `ToolsConfig` 中新增：

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolsConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub max_rounds: usize,
    #[serde(default = "default_task_max_steps_explore")]
    pub task_max_steps_explore: usize,
    #[serde(default = "default_task_max_steps_general")]
    pub task_max_steps_general: usize,
}

fn default_task_max_steps_explore() -> usize { 30 }
fn default_task_max_steps_general() -> usize { 50 }
```

---

## 4. `task` 工具 JSON Schema

```json
{
  "type": "object",
  "properties": {
    "description": {
      "type": "string",
      "description": "简短任务描述（10-80 字），用于进度展示和日志。"
    },
    "prompt": {
      "type": "string",
      "description": "详细任务提示。应包含完整的任务上下文、目标和输出要求，因为子代理没有主 agent 的对话历史。"
    },
    "subagent_type": {
      "type": "string",
      "enum": ["explore", "general"],
      "description": "子代理类型。explore：只读搜索，适合代码库探索和信息收集；general：通用多步任务，可读写文件和运行命令。默认 general。",
      "default": "general"
    },
    "max_steps": {
      "type": "integer",
      "description": "可选。覆盖子代理的工具调用预算上限。explore 默认 30，general 默认 50。"
    }
  },
  "required": ["description", "prompt"],
  "additionalProperties": false
}
```

**不实现 `task_id`**：Miyu 没有持久化 session 系统，子代理是一次性的。如果未来需要恢复，可以再扩展。

---

## 5. 返回值格式

采用 JSON 格式（与 Miyu 其他工具一致，如 `deep_research`、`linux_game_compatibility`）：

```json
{
  "ok": true,
  "kind": "task",
  "subagent_type": "general",
  "description": "重构 auth 模块",
  "state": "completed",
  "result": "子代理最终文本...",
  "stats": {
    "tool_calls": 12,
    "tool_ok": 10,
    "tool_errors": 2,
    "steps_used": 12,
    "max_steps": 50
  }
}
```

**`state` 枚举值**：
- `completed`：子代理正常完成（无 tool_calls 时停止）
- `budget_reached`：工具预算耗尽，子代理被强制终止并输出最终总结
- `timeout`：会话总超时
- `error`：子代理运行出错

**不使用 XML 格式的原因**：
1. Miyu 所有其他工具返回 JSON（`deep_research`、`deep_diagnose`、`linux_game` 都是 `serde_json::to_string_pretty(&json!({...}))`）
2. 主 agent 的 `extract_persistable_tool_report` 已经按 JSON 字段提取 `final_answer` / `final_report`
3. JSON 结构化字段（`stats`、`state`）比 XML 更容易让 LLM 理解和消费

**主 agent 消费方式**：
- `result` 字段包含子代理最终文本，主 agent 读取后作为上下文回复用户
- 在 `extract_persistable_tool_report` 中新增 `"task" => "result"` 映射，使子代理结果被持久化到对话上下文

---

## 6. 工具排除列表

### 6.1 永远排除（所有子代理类型）

防止递归调用和功能重叠：

```rust
const ALWAYS_EXCLUDED: &[&str] = &[
    "task",
    "task_agent",
    "deep_research",
    "linux_input_method_diagnose",
    "deep_diagnose",
    "linux_game_compatibility",
];
```

### 6.2 `explore` 类型 — 白名单模式

只保留只读工具，其余全部排除：

```rust
const EXPLORE_ALLOWED: &[&str] = &[
    "read_file",
    "glob",
    "find_files",
    "grep",
    "search_text",
    "check_os_info",
    "web_fetch",
    "web_search",
];
```

实现方式：从主 registry 克隆后，删除所有不在白名单中的工具。

```rust
fn build_explore_registry(source: &ToolRegistry) -> ToolRegistry {
    let mut registry = ToolRegistry::new();
    for name in EXPLORE_ALLOWED {
        if let Some(spec) = source.tools.get(name) {
            registry.register(spec.clone());
        }
    }
    registry
}
```

注意：`explore` 的 `run_command` 用的是 `register_readonly` 版本（只允许只读命令），但由于白名单中不包含 `run_command`，explore 子代理完全没有 shell 命令能力。这是有意为之——explore 应该用专用工具（`grep`、`glob`、`read_file`）而不是 shell。

### 6.3 `general` 类型 — 黑名单模式

从主 registry 克隆后删除排除列表中的工具：

```rust
const GENERAL_EXCLUDED: &[&str] = &[
    // 永远排除
    "task",
    "task_agent",
    "deep_research",
    "linux_input_method_diagnose",
    "deep_diagnose",
    "linux_game_compatibility",
    // 技能加载（子代理不应加载技能）
    "load_skill",
    // 闹钟（用户交互工具，子代理不应设置）
    "set_alarm",
    "list_alarms",
    "cancel_alarm",
    // 表情包（用户交互工具）
    "search_meme",
    "show_meme",
    "add_meme",
    "update_meme",
    "delete_meme",
    // 图片生成/显示（用户交互工具，消耗资源）
    "generate_image",
    "print_image",
    "search_web_images",
    // 玄学/娱乐（与编程任务无关）
    "xuanxue_pick",
    "xuanxue_divine",
    "draw_zhouyi_hexagram",
    "draw_tarot_card",
    "draw_fortune_lot",
    "roll_dice",
];
```

**排除理由**：
- 子代理工具：防递归
- `load_skill`：子代理不应动态加载技能改变自身行为
- 闹钟/表情包/图片生成/玄学：这些是面向用户的交互工具，子代理的输出返回给主 agent，不应直接面向用户产生副作用
- `install_aur_package` / `review_aur_package` / `review_pkgbuild_directory`：**不排除**，因为 general 子代理可能需要审查 AUR 包作为任务的一部分
- `analyze_image` / `vision_analyze`：**不排除**，general 子代理可能需要分析图片
- `remember_fact` / `recall_memory` 等：**不排除**，general 子代理可能需要查询记忆
- 知识库工具：**不排除**，general 子代理可能需要搜索知识库

---

## 7. 实现架构

### 7.1 新增文件

```
src/tools/subagent.rs       — task 工具注册 + SubagentRunner
src/prompts/subagent-explore.md
src/prompts/subagent-general.md
```

### 7.2 `SubagentRunner` 核心结构

```rust
// src/tools/subagent.rs

use super::{readable_tool_name, ToolProgress, ToolRegistry, ToolSpec};
use crate::config::AppConfig;
use crate::i18n::{is_zh, text as t};
use crate::llm::{
    ChatMessage, ChatResult, ChatStreamChunk, ChatStreamKind, OpenAiCompatibleClient, Usage,
};
use crate::paths::MiyuPaths;
use anyhow::{bail, Result};
use serde_json::{json, Value};
use std::time::{Duration, Instant};

const EXPLORE_SYSTEM_PROMPT: &str = include_str!("../prompts/subagent-explore.md");
const GENERAL_SYSTEM_PROMPT: &str = include_str!("../prompts/subagent-general.md");

const ALWAYS_EXCLUDED: &[&str] = &[
    "task",
    "task_agent",
    "deep_research",
    "linux_input_method_diagnose",
    "deep_diagnose",
    "linux_game_compatibility",
];

const EXPLORE_ALLOWED: &[&str] = &[
    "read_file",
    "glob",
    "find_files",
    "grep",
    "search_text",
    "check_os_info",
    "web_fetch",
    "web_search",
];

const GENERAL_EXCLUDED: &[&str] = &[
    "task",
    "task_agent",
    "deep_research",
    "linux_input_method_diagnose",
    "deep_diagnose",
    "linux_game_compatibility",
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
```

### 7.3 注册函数

```rust
pub fn register(
    registry: &mut ToolRegistry,
    config: AppConfig,
    paths: MiyuPaths,
    tools: ToolRegistry,
) {
    let context = SubagentContext { config, paths, tools };
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
                    "description": t(
                        "Short task description (10-80 chars) for progress display.",
                        "简短任务描述（10-80 字），用于进度展示。"
                    )
                },
                "prompt": {
                    "type": "string",
                    "description": t(
                        "Detailed task prompt. Must include full context, goals, and output requirements since the subagent has no access to the main agent's conversation history.",
                        "详细任务提示。必须包含完整的上下文、目标和输出要求，因为子代理无法访问主 agent 的对话历史。"
                    )
                },
                "subagent_type": {
                    "type": "string",
                    "enum": ["explore", "general"],
                    "description": t(
                        "Subagent type. explore: read-only search for codebase exploration and info gathering; general: multi-step tasks with file read/write and command execution. Defaults to general.",
                        "子代理类型。explore：只读搜索，适合代码库探索和信息收集；general：通用多步任务，可读写文件和运行命令。默认 general。"
                    ),
                    "default": "general"
                },
                "max_steps": {
                    "type": "integer",
                    "description": t(
                        "Optional. Override the subagent's tool call budget. explore defaults to 30, general defaults to 50.",
                        "可选。覆盖子代理的工具调用预算上限。explore 默认 30，general 默认 50。"
                    )
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
```

### 7.4 核心运行函数

```rust
#[derive(Clone)]
struct SubagentContext {
    config: AppConfig,
    paths: MiyuPaths,
    tools: ToolRegistry,
}

#[derive(Clone, Copy, Eq, PartialEq)]
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

    fn build_registry(self, source: &ToolRegistry) -> ToolRegistry {
        match self {
            Self::Explore => {
                let mut registry = ToolRegistry::new();
                for name in EXPLORE_ALLOWED {
                    // 直接访问 source 的 tools HashMap
                    // 需要在 ToolRegistry 上暴露一个 getter 或用 definitions_except
                }
                registry
            }
            Self::General => {
                // 克隆 source，然后用 definitions_except 排除
                source.clone() // 然后在 chat_with_tools 中用 definitions_except(GENERAL_EXCLUDED)
            }
        }
    }
}
```

> **注意**：`explore` 白名单模式需要 `ToolRegistry` 暴露按名获取 `ToolSpec` 的方法。当前 `tools` 字段是私有的。有两个方案：
> - **方案 A**：在 `ToolRegistry` 上新增 `pub fn get(&self, name: &str) -> Option<&ToolSpec>` 和 `pub fn clone_filtered(&self, allowed: &[&str]) -> ToolRegistry`
> - **方案 B**：explore 也用黑名单模式，排除所有不在白名单中的工具——但这需要知道所有工具名，维护成本高
> 
> **推荐方案 A**，在 `registry.rs` 中新增：
> ```rust
> pub fn get(&self, name: &str) -> Option<&ToolSpec> {
>     self.tools.get(name)
> }
> 
> pub fn clone_filtered(&self, allowed: &[&str]) -> ToolRegistry {
>     let mut registry = ToolRegistry::new();
>     for name in allowed {
>         if let Some(spec) = self.tools.get(*name) {
>             registry.register(spec.clone());
>         }
>     }
>     registry
> }
> ```

### 7.5 `run_task` 函数

```rust
async fn run_task(
    args: Value,
    context: SubagentContext,
    progress: ToolProgress,
) -> Result<String> {
    let description = required(&args, "description")?;
    let prompt = required(&args, "prompt")?;
    let subagent_type = SubagentType::from_str(
        args.get("subagent_type")
            .and_then(Value::as_str)
            .unwrap_or("general"),
    );
    let max_steps = args
        .get("max_steps")
        .and_then(Value::as_u64)
        .map(|v| v as usize)
        .unwrap_or_else(|| subagent_type.default_max_steps());
    let tool_timeout = subagent_type.tool_timeout();
    let total_timeout = subagent_type.total_timeout();

    progress.report(format!(
        "{}：{}",
        t("subagent", "子代理"),
        description
    ));

    let client = OpenAiCompatibleClient::from_config(&context.config, &context.paths)?;
    let tools = subagent_type.build_registry(&context.tools);
    let system_prompt = subagent_type.system_prompt();

    let start = Instant::now();
    let mut stats = SubagentStats::default();
    let messages = vec![
        ChatMessage::system(system_prompt),
        ChatMessage::plain("user", prompt.clone()),
    ];

    let (result, state) = match tokio::time::timeout(
        Duration::from_secs(total_timeout),
        chat_with_tools(
            &client,
            messages,
            tools,
            subagent_type,
            max_steps,
            tool_timeout,
            &progress,
            &mut stats,
        ),
    )
    .await
    {
        Ok(Ok(result)) => (result, "completed"),
        Ok(Err(_)) => (String::new(), "error"),
        Err(_) => (String::new(), "timeout"),
    };

    // 如果 result 为空但 state 是 completed，说明可能是 budget_reached
    let state = if state == "completed" && stats.budget_reached {
        "budget_reached"
    } else {
        state
    };

    let final_text = result.trim().to_string();
    stats.add_usage_or_estimate(result.usage.as_ref(), &[system_prompt, &prompt, &final_text]);

    Ok(serde_json::to_string_pretty(&json!({
        "ok": state == "completed" || state == "budget_reached",
        "kind": "task",
        "subagent_type": match subagent_type {
            SubagentType::Explore => "explore",
            SubagentType::General => "general",
        },
        "description": description,
        "state": state,
        "result": final_text,
        "stats": stats.public(),
    }))?)
}
```

### 7.6 `chat_with_tools` 函数

复用 `deep_diagnose` / `linux_game` 的模式，统一为：

```rust
async fn chat_with_tools(
    client: &OpenAiCompatibleClient,
    mut messages: Vec<ChatMessage>,
    tools: ToolRegistry,
    subagent_type: SubagentType,
    max_steps: usize,
    timeout_seconds: u64,
    progress: &ToolProgress,
    stats: &mut SubagentStats,
) -> Result<ChatResult> {
    let excluded = match subagent_type {
        SubagentType::Explore => &[][..], // explore 的 registry 已经是白名单构建的
        SubagentType::General => GENERAL_EXCLUDED,
    };
    let definitions = tools.definitions_except(excluded);
    let mut steps = 0usize;

    loop {
        // 预算检查
        if max_steps > 0 && steps >= max_steps {
            stats.budget_reached = true;
            messages.push(ChatMessage::plain("user", finalization_prompt()));
            let result = client
                .chat_stream(messages, Vec::new(), |chunk: ChatStreamChunk| {
                    if chunk.kind == ChatStreamKind::Reasoning {
                        progress.report(format!("__subagent_reasoning__{}", chunk.text));
                    }
                    Ok(())
                })
                .await?;
            stats.add_usage_or_estimate(result.usage.as_ref(), &[&result.content]);
            return Ok(result);
        }

        let result = client
            .chat_stream(messages.clone(), definitions.clone(), |chunk: ChatStreamChunk| {
                if chunk.kind == ChatStreamKind::Reasoning {
                    progress.report(format!("__subagent_reasoning__{}", chunk.text));
                }
                Ok(())
            })
            .await?;
        stats.add_usage_or_estimate(result.usage.as_ref(), &[]);

        if result.tool_calls.is_empty() {
            return Ok(result);
        }

        // 推入 assistant 消息
        if !result.content.trim().is_empty() {
            messages.push(ChatMessage::assistant(result.content.clone(), None));
        }

        let mut transcript = Vec::new();
        for call in result.tool_calls {
            if max_steps > 0 && steps >= max_steps {
                transcript.push(render_internal_tool_result(
                    &call.function.name,
                    &call.function.arguments,
                    false,
                    "tool skipped: subagent tool budget reached",
                ));
                continue;
            }
            steps += 1;
            stats.tool_calls += 1;

            // 进度报告
            if is_zh() {
                progress.report(format!(
                    "工具 #{steps}：{} 运行中",
                    readable_tool_name(&call.function.name)
                ));
            } else {
                progress.report(format!("tool #{steps}: {} running", call.function.name));
            }
            progress.report(format!(
                "__subtool_call__{}",
                json!({"name": call.function.name, "args": call.function.arguments})
            ));

            let (output, ok) = match tokio::time::timeout(
                Duration::from_secs(timeout_seconds.max(5)),
                tools.call(&call.function.name, &call.function.arguments),
            )
            .await
            {
                Ok(Ok(output)) => (output, true),
                Ok(Err(err)) => (format!("tool error: {err}"), false),
                Err(_) => (
                    format!("tool error: {} timed out after {timeout_seconds}s", call.function.name),
                    false,
                ),
            };

            if ok {
                stats.tool_ok += 1;
            } else {
                stats.tool_errors += 1;
            }

            if is_zh() {
                progress.report(format!(
                    "工具 #{steps}：{} ok",
                    readable_tool_name(&call.function.name)
                ));
            } else {
                progress.report(format!("tool #{steps}: {} ok", call.function.name));
            }
            progress.report(format!(
                "__subtool_result__{}",
                json!({"name": call.function.name, "ok": ok, "output": output})
            ));

            transcript.push(render_internal_tool_result(
                &call.function.name,
                &call.function.arguments,
                ok,
                &output,
            ));
        }

        if !transcript.is_empty() {
            messages.push(ChatMessage::plain(
                "user",
                render_internal_tool_transcript(&transcript, steps, max_steps),
            ));
        }
    }
}

fn finalization_prompt() -> &'static str {
    "<tool_budget_reached>工具预算已用尽。不要再请求工具。请只基于上面的任务描述和已执行工具结果输出最终结果；缺少信息的地方明确说明。</tool_budget_reached>"
}

fn render_internal_tool_transcript(results: &[String], steps: usize, max_steps: usize) -> String {
    format!(
        "<subagent_tool_transcript>\n说明：以下是已执行完成的内部工具调用结果，不是新的用户请求。请基于这些观察继续完成任务；如信息已经足够，请输出最终结果。\ntool_budget: {steps}/{max_steps}\n{}\n</subagent_tool_transcript>",
        results.join("\n")
    )
}

fn render_internal_tool_result(name: &str, arguments: &str, ok: bool, output: &str) -> String {
    format!(
        "<tool_result name=\"{}\" ok=\"{}\">\narguments_json:\n```json\n{}\n```\noutput:\n```text\n{}\n```\n</tool_result>",
        name, ok, arguments.trim(), clip_inline(output, 6000)
    )
}
```

### 7.7 `SubagentStats`

复用 `deep_diagnose` / `linux_game` 中的统计结构：

```rust
#[derive(Default)]
struct SubagentStats {
    tool_calls: usize,
    tool_ok: usize,
    tool_errors: usize,
    budget_reached: bool,
    prompt_tokens: u64,
    completion_tokens: u64,
    total_tokens: u64,
    token_estimate: u64,
    token_estimate_method: TokenEstimateMethod,
}

// add_usage_or_estimate / public / token_estimate_method_label 
// 与 deep_diagnose.rs 中的实现完全一致
```

### 7.8 辅助函数

```rust
fn required(args: &Value, key: &str) -> Result<String> {
    let value = args.get(key).and_then(Value::as_str).unwrap_or_default().trim();
    if value.is_empty() {
        bail!("missing required argument: {key}")
    }
    Ok(value.to_string())
}

fn clip_inline(value: &str, max_chars: usize) -> String {
    let value = value.split_whitespace().collect::<Vec<_>>().join(" ");
    if value.chars().count() <= max_chars {
        value
    } else {
        format!("{}...", value.chars().take(max_chars.saturating_sub(3)).collect::<String>())
    }
}
```

---

## 8. 集成修改

### 8.1 `src/tools/mod.rs`

```rust
mod subagent;
```

在 `builtin_registry` 函数中，将 `task_agent` 替换为 `task`：

```rust
pub fn builtin_registry(config: &AppConfig, paths: &MiyuPaths) -> ToolRegistry {
    let mut registry = ToolRegistry::new();
    // 注意：不再调用 default_tools::register（它注册 task_agent）
    // 而是手动注册 default tools 除了 task_agent
    default_tools::register(&mut registry, true);
    // ... 其他工具注册 ...
    
    // 注册 task 工具（替换 task_agent）
    let subagent_tools = registry.clone();
    subagent::register(&mut registry, config.clone(), paths.clone(), subagent_tools);
    
    registry
}
```

**注意**：`default_tools::register` 会注册 `task_agent`。有两个选择：
- **方案 A**：从 `default_tools::register` 中移除 `task_agent` 注册，在 `builtin_registry` 中注册 `task`
- **方案 B**：在 `default_tools::register` 中将 `task_agent` 替换为调用 `subagent::register`

**推荐方案 A**，因为 `task` 需要访问 `AppConfig`、`MiyuPaths` 和 `ToolRegistry`，而 `default_tools::register` 签名只有 `&mut ToolRegistry` 和 `bool`。

具体修改 `default_tools.rs`：

```rust
pub fn register(registry: &mut ToolRegistry, allow_command_execution: bool) {
    register_readonly(registry);
    registry.register(ToolSpec::new(
        "run_command",
        // ... 保持不变 ...
    ).writes());
    // 移除 task_agent 注册
    registry.register(ToolSpec::new(
        "edit_file",
        // ... 保持不变 ...
    ).writes());
    registry.register(ToolSpec::new(
        "trash_path",
        // ... 保持不变 ...
    ).writes());
}
```

### 8.2 `src/tools/registry.rs`

新增 `get` 和 `clone_filtered` 方法：

```rust
impl ToolRegistry {
    // ... 现有方法 ...

    pub fn get(&self, name: &str) -> Option<&ToolSpec> {
        self.tools.get(name)
    }

    pub fn clone_filtered(&self, allowed: &[&str]) -> ToolRegistry {
        let mut registry = ToolRegistry::new();
        for name in allowed {
            if let Some(spec) = self.tools.get(*name) {
                registry.register(spec.clone());
            }
        }
        registry
    }
}
```

### 8.3 `src/agent/mod.rs`

在 `extract_persistable_tool_report` 中新增 `task` 映射：

```rust
fn extract_persistable_tool_report(tool_name: &str, output: &str) -> Option<String> {
    let field = match tool_name {
        "linux_game_compatibility" => "final_report",
        "linux_input_method_diagnose" | "deep_diagnose" | "deep_research" => "final_answer",
        "task" => "result",  // 新增
        _ => return None,
    };
    // ... 保持不变 ...
}
```

### 8.4 `src/prompts.rs`

新增常量：

```rust
pub const SUBAGENT_EXPLORE_PROMPT: &str = include_str!("prompts/subagent-explore.md");
pub const SUBAGENT_GENERAL_PROMPT: &str = include_str!("prompts/subagent-general.md");
```

### 8.5 `src/tools/mod.rs` — `readable_tool_name`

更新映射：

```rust
pub fn readable_tool_name(name: &str) -> &str {
    match name {
        // ...
        "task" => "子代理",  // 替换 "task_agent" => "创建子任务"
        // ...
        _ => name,
    }
}
```

### 8.6 `src/config.rs` — 可选配置

在 `ToolsConfig` 中新增可选配置字段（带默认值，不影响现有配置文件）：

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolsConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub max_rounds: usize,
    #[serde(default = "default_task_max_steps_explore")]
    pub task_max_steps_explore: usize,
    #[serde(default = "default_task_max_steps_general")]
    pub task_max_steps_general: usize,
}

fn default_task_max_steps_explore() -> usize { 30 }
fn default_task_max_steps_general() -> usize { 50 }
```

在 `Default` 实现中：

```rust
impl Default for ToolsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_rounds: 0,
            task_max_steps_explore: 30,
            task_max_steps_general: 50,
        }
    }
}
```

---

## 9. 完整工具排除矩阵

| 工具名 | explore | general | 排除原因 |
|--------|---------|---------|----------|
| `run_command` | ❌ | ✅ | explore 无 shell；general 可用 |
| `read_file` | ✅ | ✅ | 只读 |
| `glob` / `find_files` | ✅ | ✅ | 只读 |
| `grep` / `search_text` | ✅ | ✅ | 只读 |
| `edit_file` | ❌ | ✅ | 写入，general 需要 |
| `trash_path` | ❌ | ✅ | 写入，general 需要 |
| `check_os_info` | ✅ | ✅ | 只读 |
| `task` / `task_agent` | ❌ | ❌ | 递归 |
| `deep_research` | ❌ | ❌ | 递归 |
| `linux_input_method_diagnose` / `deep_diagnose` | ❌ | ❌ | 递归 |
| `linux_game_compatibility` | ❌ | ❌ | 递归 |
| `web_search` | ✅ | ✅ | 只读 |
| `web_fetch` | ✅ | ✅ | 只读 |
| `load_skill` | ❌ | ❌ | 子代理不应加载技能 |
| `set_alarm` / `list_alarms` / `cancel_alarm` | ❌ | ❌ | 用户交互副作用 |
| `search_meme` / `show_meme` / `add_meme` / `update_meme` / `delete_meme` | ❌ | ❌ | 用户交互副作用 |
| `generate_image` | ❌ | ❌ | 资源消耗 + 用户交互 |
| `print_image` | ❌ | ❌ | 用户交互 |
| `search_web_images` | ❌ | ❌ | 用户交互 |
| `xuanxue_pick` / `xuanxue_divine` / `draw_zhouyi_hexagram` / `draw_tarot_card` / `draw_fortune_lot` / `roll_dice` | ❌ | ❌ | 与编程任务无关 |
| `analyze_image` / `vision_analyze` | ❌ | ✅ | general 可能需要 |
| `fcitx5_input_method_wiki_qurey` | ❌ | ✅ | general 可能需要 |
| `weather` / `get_weather` | ❌ | ✅ | general 可能需要 |
| `exchange_rate` / `get_exchange_rate` | ❌ | ✅ | general 可能需要 |
| `moegirl_query` | ❌ | ✅ | general 可能需要 |
| `archlinux_official_package_query` / `aur_search_packages` / `aur_get_package_info` / `aur_check_status` / `pacman_search` / `archwiki_query` | ❌ | ✅ | general 可能需要 |
| `online_man_search` / `man_search` / `online_man_get_page` / `man_read` | ❌ | ✅ | general 可能需要 |
| `calculate` / `calculator` | ❌ | ✅ | general 可能需要 |
| `calculate_hash` / `decode_encoded_text` | ❌ | ✅ | general 可能需要 |
| `query_deepseek_status` | ❌ | ✅ | general 可能需要 |
| `protondb_query` | ❌ | ✅ | general 可能需要 |
| `review_aur_package` / `install_aur_package` / `review_pkgbuild_directory` | ❌ | ✅ | general 可能需要 |
| `gather_linux_game_compatibility_signals` / `register_linux_game_evidence` | ❌ | ❌ | 子代理专属工具 |
| `remember_fact` / `recall_memory` / `recall_memories` / `forget_memory` / `forget_memories` / `list_memory` / `list_memories` / `recall_past_events` / `search_evicted_context` | ❌ | ✅ | general 可能需要查询记忆 |
| `upload_knowledge_base_file` / `upload_text_to_knowledge_base` / `read_knowledge_base_file` / `search_knowledge_base` / `search_knowledge_base_by_name` / `edit_knowledge_base_file` / `remove_knowledge_base_file` / `list_knowledge_base_files` | ❌ | ✅ | general 可能需要 |
| `register_deep_research_topic_title` / `register_deep_research_reference` / `remove_deep_research_reference` | ❌ | ❌ | deep_research 专属工具 |
| `linux_input_method_diagnose` 相关诊断工具 | ❌ | ❌ | 诊断子代理专属 |

---

## 10. 实现检查清单

- [ ] `src/prompts/subagent-explore.md` — 创建
- [ ] `src/prompts/subagent-general.md` — 创建
- [ ] `src/prompts.rs` — 新增 `SUBAGENT_EXPLORE_PROMPT` / `SUBAGENT_GENERAL_PROMPT` 常量
- [ ] `src/tools/registry.rs` — 新增 `get()` / `clone_filtered()` 方法
- [ ] `src/tools/subagent.rs` — 创建，包含完整实现
- [ ] `src/tools/mod.rs` — 新增 `mod subagent;`，更新 `readable_tool_name`，更新 `builtin_registry`
- [ ] `src/tools/default_tools.rs` — 从 `register()` 中移除 `task_agent` 注册
- [ ] `src/agent/mod.rs` — 在 `extract_persistable_tool_report` 中新增 `"task" => "result"`
- [ ] `src/config.rs` — 在 `ToolsConfig` 中新增 `task_max_steps_explore` / `task_max_steps_general`（可选）
- [ ] `cargo build` 通过
- [ ] `cargo test` 通过
- [ ] `cargo clippy` 通过
