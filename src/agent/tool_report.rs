//! 工具报告的提取、压缩与私有记忆。
//!
//! 工具输出要长期留在上下文里，但原样留会把窗口吃光。这里把它压成能长期携带的
//! 形态：只保留后续回合真正会用到的东西。
//!
//! 「私有记忆」是给 Miyu 自己看的那一份（`private_tool_memory`），头尾各留一段
//! （`PRIVATE_*_HEAD/TAIL_CHARS`）——中间截掉，因为有用的信息通常在两头。

use crate::agent::*;

/// 一次工具调用碰过的文件是读还是写。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::agent) enum PathAccess {
    Read,
    Write,
}

/// 解析工具调用的真实参数。stub 懒工具把真参数包在 `arguments` 壳里，先拆壳。
fn tool_arguments(arguments: &str) -> Option<serde_json::Value> {
    let mut args: serde_json::Value = serde_json::from_str(arguments).ok()?;
    if let Some(inner) = args.get("arguments") {
        if inner.is_object() {
            args = inner.clone();
        }
    }
    Some(args)
}

/// 单路径参数:Miyu/agy 是 `path`(agy 的 AbsolutePath/TargetFile 在流层已
/// 归一成 path),claude 原生工具是 `file_path`,NotebookEdit 是 `notebook_path`。
fn path_arg(args: &serde_json::Value) -> Option<String> {
    ["path", "file_path", "notebook_path"]
        .iter()
        .filter_map(|key| args.get(key)?.as_str())
        .map(str::trim)
        .find(|path| !path.is_empty())
        .map(str::to_string)
}

/// codex 的 `file_change` 事件折成 `edit` 时带整组 `paths`(`path` 只是第一个)。
fn paths_arg(args: &serde_json::Value) -> Vec<String> {
    args.get("paths")
        .and_then(serde_json::Value::as_array)
        .map(|paths| {
            paths
                .iter()
                .filter_map(serde_json::Value::as_str)
                .map(str::trim)
                .filter(|path| !path.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// 补丁工具把路径藏在 `patchText` 的头部，而不是 `path` 参数里。08-21 工具面
/// 统一后现网写文件工具叫 `edit`（`write_file`/`edit_string` 是旧名），只认
/// `path` 会把它的每次改动都漏掉。
fn patch_paths(args: &serde_json::Value) -> Vec<(PathAccess, String)> {
    let Some(raw) = args
        .get("patchText")
        .or_else(|| args.get("patch_text"))
        .and_then(serde_json::Value::as_str)
    else {
        return Vec::new();
    };
    let mut paths = Vec::new();
    for line in raw.lines() {
        for prefix in [
            "*** Add File:",
            "*** Update File:",
            "*** Delete File:",
            "*** Move to:",
        ] {
            let Some(value) = line
                .strip_prefix(prefix)
                .map(str::trim)
                .filter(|value| !value.is_empty())
            else {
                continue;
            };
            // kb:/artifact: 是知识库与产物域，不是文件系统路径。
            if !value.starts_with("kb:") && !value.starts_with("artifact:") {
                paths.push((PathAccess::Write, value.to_string()));
            }
        }
    }
    paths
}

/// 一次工具调用碰过的全部文件路径。`path` 参数类工具直接读；`edit`/`apply_patch`
/// 走 `patchText` 的补丁头。
///
/// 中转线的名字也在这里认(09-10):三条线的工具调用全走 `RemoteToolStarted`,
/// 名字是 CLI 那头的——claude 原生 `Read`/`Edit`/`Write`/`MultiEdit`/
/// `NotebookEdit`,agy 的 `view_file`/`write_to_file`/`replace_file_content`,
/// codex 把 `file_change` 折成 `edit` + `paths` 数组。取证前活库 42 个
/// remote 轮 footprint 全空,`<modified-files>` 与回灌在中转线上从没有过内容。
pub(in crate::agent) fn tool_call_paths(name: &str, arguments: &str) -> Vec<(PathAccess, String)> {
    let Some(args) = tool_arguments(arguments) else {
        return Vec::new();
    };
    let access = match name {
        "read" | "read_file" | "Read" | "view_file" => PathAccess::Read,
        "write_file"
        | "edit_file"
        | "edit_string"
        | "Edit"
        | "Write"
        | "MultiEdit"
        | "NotebookEdit"
        | "write_to_file"
        | "replace_file_content"
        | "multi_replace_file_content" => PathAccess::Write,
        "edit" | "apply_patch" => {
            let mut paths = patch_paths(&args);
            if paths.is_empty() {
                paths = paths_arg(&args)
                    .into_iter()
                    .map(|path| (PathAccess::Write, path))
                    .collect();
            }
            if paths.is_empty() {
                paths = path_arg(&args)
                    .map(|path| vec![(PathAccess::Write, path)])
                    .unwrap_or_default();
            }
            return paths;
        }
        _ => return Vec::new(),
    };
    path_arg(&args)
        .map(|path| vec![(access, path)])
        .unwrap_or_default()
}

/// Deterministic footprint extraction at tool-execution time: the only point
/// where tool arguments still exist (completed turns don't persist them).
pub(in crate::agent) fn tool_call_footprint(
    name: &str,
    arguments: &str,
) -> Option<crate::state::ToolFootprint> {
    let mut footprint = crate::state::ToolFootprint::default();
    if name == "remember_fact" {
        let args = tool_arguments(arguments)?;
        let content = args.get("content")?.as_str()?.trim();
        if content.is_empty() {
            return None;
        }
        let mut label: String = content.chars().take(80).collect();
        if content.chars().count() > 80 {
            label.push('…');
        }
        footprint.memories.insert(label);
        return Some(footprint);
    }
    for (access, path) in tool_call_paths(name, arguments) {
        match access {
            PathAccess::Read => {
                footprint.read.insert(path);
            }
            PathAccess::Write => {
                footprint.modified.insert(path);
            }
        }
    }
    (!footprint.is_empty()).then_some(footprint)
}

pub(in crate::agent) fn extract_persistable_tool_report(
    tool_name: &str,
    output: &str,
) -> Option<String> {
    let field = match tool_name {
        "artifact" | "create_artifact" | "apply_artifact_patch" | "present_artifact" => {
            return compact_artifact_tool_report(tool_name, output)
                .map(|report| wrap_previous_tool_report(tool_name, &report))
        }
        // 统一 edit 只有 artifact: 命名空间的输出(operation=apply_artifact_patch)
        // 需要长期报告;文件系统/kb 的编辑不留。
        "edit" => {
            let is_artifact = serde_json::from_str::<serde_json::Value>(output)
                .ok()
                .and_then(|value| {
                    value
                        .get("operation")
                        .and_then(|operation| operation.as_str().map(str::to_string))
                })
                .is_some_and(|operation| operation == "apply_artifact_patch");
            if is_artifact {
                return compact_artifact_tool_report(tool_name, output)
                    .map(|report| wrap_previous_tool_report(tool_name, &report));
            }
            return None;
        }
        "load_tools" => {
            return compact_loaded_tools_report(output)
                .map(|report| wrap_previous_tool_report(tool_name, &report))
        }
        "use_meme:show" => return compact_sent_meme_report(output),
        "remember_fact" => {
            return compact_remembered_fact_report(output)
                .map(|report| wrap_previous_tool_report(tool_name, &report))
        }
        "deep_research_linux_game_compatibility" => "final_report",
        "linux_input_method_diagnose" | "deep_diagnose" | "deep_research" => "final_answer",
        // 08-21 token-diet 文本形态:"result:" 行之后全是子代理结论本体;
        // 旧 JSON 形态(历史回放)与错误路径(ok:false JSON)走下面的原路径。
        "task" => {
            if let Some((_, report)) = output.split_once("\nresult:\n") {
                let report = report.trim();
                if !report.is_empty() {
                    return Some(wrap_previous_tool_report(tool_name, report));
                }
            }
            "result"
        }
        _ => return None,
    };
    serde_json::from_str::<serde_json::Value>(output)
        .ok()
        .and_then(|value| {
            value
                .get(field)
                .and_then(serde_json::Value::as_str)
                .map(str::trim)
                .map(str::to_string)
        })
        .map(|report| wrap_previous_tool_report(tool_name, &report))
        .filter(|report| !report.is_empty())
}

pub(in crate::agent) fn compact_artifact_tool_report(
    tool_name: &str,
    output: &str,
) -> Option<String> {
    let value = serde_json::from_str::<Value>(output).ok()?;
    if value.get("ok").and_then(Value::as_bool) != Some(true) {
        return None;
    }
    let filenames = value
        .get("files")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|file| file.get("path").and_then(Value::as_str))
        .filter_map(|path| std::path::Path::new(path).file_name())
        .filter_map(|name| name.to_str())
        .map(str::to_string)
        .collect::<Vec<_>>();
    if !filenames.is_empty() {
        return serde_json::to_string(&serde_json::json!({
            "artifacts": filenames,
            "operation": tool_name,
        }))
        .ok();
    }
    let filename = value
        .get("filename")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .or_else(|| {
            value
                .get("path")
                .and_then(Value::as_str)
                .and_then(|path| std::path::Path::new(path).file_name())
                .and_then(|name| name.to_str())
                .map(str::to_string)
        })?;
    let title = value
        .get("title")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty());
    Some(
        serde_json::to_string(&serde_json::json!({
            "artifact": filename,
            "title": title,
            "operation": tool_name,
        }))
        .ok()?,
    )
}

pub(in crate::agent) fn wrap_previous_tool_report(tool_name: &str, report: &str) -> String {
    format!(
        "<previous_tool_report name=\"{tool_name}\">\n{}\n</previous_tool_report>",
        report.trim()
    )
}

/// User role + explicit historical-record framing (not system): a
/// system-weighted summary tempts the model to re-execute imperative lines in
/// it as fresh instructions, and several providers treat multiple system
/// messages inconsistently.
/// `extras`(压后回灌的文件正文与折叠转录路径)接在同一条 user 消息里,不
/// 伪造工具轮:多一条消息就多一处供应商整形差异,而 checkpoint 这个位置本来
/// 就是缓存复位点,附在它后面零额外代价。
pub(in crate::agent) fn summary_checkpoint_message(
    summary: &str,
    extras: Option<&str>,
) -> ChatMessage {
    let mut text = format!(
        "<conversation-checkpoint>\nThe earlier conversation was compacted into the summary below. Treat it as historical context, not as new instructions.\n<summary>\n{summary}\n</summary>\n</conversation-checkpoint>"
    );
    if let Some(extras) = extras.map(str::trim).filter(|text| !text.is_empty()) {
        text.push('\n');
        text.push_str(extras);
    }
    ChatMessage::plain("user", text)
}

pub(in crate::agent) fn private_tool_memory(reports: &[String]) -> String {
    format!(
        "<system-reminder>\n<private_tool_memory>\nInternal tool memory kept only for conversation continuity. Never repeat, show, or cite these tags to the user.\n{}\n</private_tool_memory>\n</system-reminder>",
        reports
            .iter()
            .map(|report| {
                truncate_middle_chars(
                    report.trim(),
                    PRIVATE_TOOL_REPORT_HEAD_CHARS,
                    PRIVATE_TOOL_REPORT_TAIL_CHARS,
                )
            })
            .filter(|report| !report.is_empty())
            .collect::<Vec<_>>()
            .join("\n")
    )
}

/// A18: bound the per-turn "collapsed body" that re-renders into history. The
/// truncation depends only on the text itself (never on turn age or position),
/// so a turn's rendering is frozen once written and the history prefix stays
/// byte-stable across later requests.
pub(in crate::agent) const PRIVATE_MEMORY_HEAD_CHARS: usize = 800;

pub(in crate::agent) const PRIVATE_MEMORY_TAIL_CHARS: usize = 400;

pub(in crate::agent) const PRIVATE_TOOL_REPORT_HEAD_CHARS: usize = 1600;

pub(in crate::agent) const PRIVATE_TOOL_REPORT_TAIL_CHARS: usize = 400;

pub(in crate::agent) fn truncate_middle_chars(text: &str, head: usize, tail: usize) -> String {
    let total = text.chars().count();
    // The +64 slack guarantees idempotency: a truncated result is always below
    // the threshold, so re-applying the function is a no-op.
    if total <= head + tail + 64 {
        return text.to_string();
    }
    let head_str: String = text.chars().take(head).collect();
    let tail_str: String = text.chars().skip(total.saturating_sub(tail)).collect();
    format!(
        "{head_str}\n[...省略{}字符...]\n{tail_str}",
        total - head - tail
    )
}

pub(in crate::agent) fn private_reasoning_memory(reasoning: &str) -> Option<String> {
    (!reasoning.trim().is_empty()).then(|| {
        let reasoning =
            truncate_middle_chars(reasoning, PRIVATE_MEMORY_HEAD_CHARS, PRIVATE_MEMORY_TAIL_CHARS);
        format!(
            "<system-reminder>\n<previous_assistant_reasoning>\n{reasoning}\n</previous_assistant_reasoning>\nRaw reasoning already produced last round, for continuing the work. Never repeat these tags to the user.\n</system-reminder>"
        )
    })
}

/// dsh 式外溢替换文案的预算自洽拼装:先按最坏情况预扣提示文案的字节数,
/// 预览用剩余额度头尾对半(字符边界安全);连提示都放不下返回 None(放弃
/// 外溢保留原文——替换永不比原文更大)。
pub(in crate::agent) fn spill_replacement(
    output: &str,
    cap: usize,
    locator: &str,
) -> Option<String> {
    fn notice(omitted: usize, locator: &str) -> String {
        format!(
            "\n\n({omitted} bytes omitted. Full result saved at: {locator} — page through it with read offset/limit, or search it with rg via run_command.)"
        )
    }
    fn cut_at_boundary(text: &str, mut at: usize) -> usize {
        while at > 0 && !text.is_char_boundary(at) {
            at -= 1;
        }
        at
    }
    // 预扣:提示文案按最坏情况(省略数取全长的位数上界) + 头尾之间的 \n…\n 分隔符。
    let reserve = notice(output.len(), locator).len() + "\n…\n".len();
    if reserve >= cap {
        return None;
    }
    let budget = cap - reserve;
    let head_end = cut_at_boundary(output, budget / 2);
    let mut tail_start = output.len().saturating_sub(budget - budget / 2);
    while tail_start < output.len() && !output.is_char_boundary(tail_start) {
        tail_start += 1;
    }
    if tail_start <= head_end {
        return None;
    }
    let omitted = tail_start - head_end;
    Some(format!(
        "{}\n…\n{}{}",
        &output[..head_end],
        &output[tail_start..],
        notice(omitted, locator)
    ))
}

/// 完成时从本回合的实况消息尾段推导结构化工具流。以 messages 为唯一真相
/// (dsh "reconstructable requests":模型可见 ⟺ 可持久重建):assistant 带
/// tool_calls 即开一轮,其后按 call id 认领 role:"tool" 输出。被 length 截断
/// 拒执行的调用照录——它们的错误文案同样是模型看到的字节。任何悬空调用
/// (无输出)补占位,回放绝不发"无应答的 tool_calls"(provider 会 400)。
/// 历史工具结果分级剪枝(08-17,抄 dsh-compaction-tool-result-pruner)。
///
/// 超预算的工具输出在**落库那一刻**改写成「头 + 省略标记 + 尾」,活体请求
/// 仍然拿到完整输出(模型这一轮需要它)。选落库而不是回放:落库时这条结果
/// 就在上下文末尾,前缀只在末尾分叉一次,代价是一个尾巴;放到回放侧改则
/// 每次回放都可能在历史中段分叉。改写是幂等的——第二次扫过不会再变。
pub(in crate::agent) fn prune_tool_output(
    output: &str,
    threshold: usize,
    head: usize,
    tail: usize,
) -> String {
    // 预算不自洽(头+尾不比阈值小)时原样返回:否则下面的减法会下溢,而且
    // "剪枝"结果可能比原文还长。调用方也拦一道,这里是第二道。
    if threshold == 0 || head + tail >= threshold || output.chars().count() <= threshold {
        return output.to_string();
    }
    let chars: Vec<char> = output.chars().collect();
    let omitted = chars.len() - head - tail;
    let marker = format!("\n…[{} {omitted} {}]\n", "omitted", "chars from the middle");
    let mut pruned = String::new();
    pruned.extend(&chars[..head]);
    pruned.push_str(&marker);
    pruned.extend(&chars[chars.len() - tail..]);
    pruned
}

pub(in crate::agent) fn prune_tool_flow(
    flow: &mut [crate::state::ToolFlowRound],
    context: &crate::config::ContextConfig,
) {
    let (threshold, head, tail) = (
        context.tool_result_prune_chars,
        context.tool_result_prune_head_chars,
        context.tool_result_prune_tail_chars,
    );
    if threshold == 0 || head + tail >= threshold {
        return;
    }
    for round in flow.iter_mut() {
        for call in round.calls.iter_mut() {
            call.output = prune_tool_output(&call.output, threshold, head, tail);
        }
    }
}

/// 回放视图:连续同签名(整轮的 名字+参数 序列相同)的轮只保留第一轮。
/// 复读轮是端点故障窗口的毒料,原样回放会教模型继续复读——08-24 取证:
/// 一个群会话积累 122 个历史工具调用、111 个纯重复(60×同一 web_search),
/// in-context 模式锁死后温度 0.6 也会产出逐字节相同的补全。确定性折叠
/// =字节稳定;上线是一次计划内冷启动,此后该会话前缀反而大幅变短。
pub(in crate::agent) fn replay_rounds(
    flow: &[crate::state::ToolFlowRound],
) -> Vec<&crate::state::ToolFlowRound> {
    let mut kept: Vec<&crate::state::ToolFlowRound> = Vec::new();
    let mut previous: Option<Vec<(&str, &str)>> = None;
    for round in flow.iter().filter(|round| !round.remote) {
        let signature: Vec<(&str, &str)> = round
            .calls
            .iter()
            .map(|call| (call.name.as_str(), call.arguments.as_str()))
            .collect();
        if !signature.is_empty() && previous.as_ref() == Some(&signature) {
            continue;
        }
        previous = Some(signature);
        kept.push(round);
    }
    kept
}

pub(in crate::agent) fn derive_tool_flow(
    messages: &[ChatMessage],
    live_start: usize,
) -> Vec<crate::state::ToolFlowRound> {
    let mut rounds: Vec<crate::state::ToolFlowRound> = Vec::new();
    for message in &messages[live_start.min(messages.len())..] {
        if message.role == "assistant" {
            if let Some(calls) = message
                .tool_calls
                .as_ref()
                .filter(|calls| !calls.is_empty())
            {
                rounds.push(crate::state::ToolFlowRound {
                    remote: false,
                    assistant_content: chat_message_text(message).unwrap_or_default(),
                    assistant_reasoning: message
                        .reasoning_content
                        .clone()
                        .filter(|reasoning| !reasoning.is_empty()),
                    calls: calls
                        .iter()
                        .map(|call| crate::state::ToolFlowCall {
                            id: call.id.clone(),
                            name: call.function.name.clone(),
                            arguments: call.function.arguments.clone(),
                            output: String::new(),
                            started_ms: None,
                            finished_ms: None,
                        })
                        .collect(),
                });
            }
        } else if message.role == "tool" {
            if let (Some(call_id), Some(round)) = (message.tool_call_id.as_ref(), rounds.last_mut())
            {
                if let Some(call) = round
                    .calls
                    .iter_mut()
                    .find(|call| &call.id == call_id && call.output.is_empty())
                {
                    call.output = chat_message_text(message).unwrap_or_default();
                    if let Some((started, finished)) = message.tool_span_ms {
                        call.started_ms = Some(started);
                        call.finished_ms = Some(finished);
                    }
                }
            }
        }
    }
    for round in &mut rounds {
        for call in &mut round.calls {
            if call.output.is_empty() {
                call.output = "(tool result unavailable)".to_string();
            }
        }
    }
    rounds
}
