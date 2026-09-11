//! 工具报告的压缩与回读。
//!
//! 工具输出要长期留在上下文里，但原样留会把窗口吃光。这里把它压成一行摘要
//! （`compact_one_line`），只保留后续回合真正会用到的东西：加载过的工具名、
//! 记住的事实、发过的表情。
//!
//! `loaded_tools_from_messages` 是反向操作——从历史消息里把「这个会话已经加载
//! 过哪些工具」读回来，避免重复加载。

use crate::agent::*;

pub(in crate::agent) fn compact_remembered_fact_report(output: &str) -> Option<String> {
    let value = serde_json::from_str::<Value>(output).ok()?;
    if value.get("ok").and_then(Value::as_bool) != Some(true) {
        return None;
    }
    let content = value.get("content").and_then(Value::as_str)?.trim();
    if content.is_empty() {
        return None;
    }
    let mut report = serde_json::json!({
        "remembered_fact": {
            "content": content,
        }
    });
    if let Some(id) = value.get("id").and_then(Value::as_i64) {
        report["remembered_fact"]["id"] = serde_json::json!(id);
    }
    if let Some(source) = value.get("source").and_then(Value::as_str) {
        let source = source.trim();
        if !source.is_empty() {
            report["remembered_fact"]["source"] = serde_json::json!(source);
        }
    }
    serde_json::to_string(&report).ok()
}

pub(in crate::agent) fn compact_loaded_tools_report(output: &str) -> Option<String> {
    let names = loaded_items_from_output(output).tools;
    if names.is_empty() {
        return None;
    }
    serde_json::to_string(&serde_json::json!({ "loaded_tools": names })).ok()
}

#[derive(Default)]
pub(in crate::agent) struct LoadedItems {
    pub(in crate::agent) targets: Vec<String>,
    pub(in crate::agent) tools: Vec<String>,
}

pub(in crate::agent) fn loaded_items_from_output(output: &str) -> LoadedItems {
    let Ok(value) = serde_json::from_str::<Value>(output) else {
        // 08-21 token-diet 文本形态:头部 "loaded_targets: a, b" /
        // "loaded_tools: a, b" 两行;契约正文从首个 "### " 起,不参与解析。
        // 旧 JSON 形态(历史回放)继续走下面的原路径。
        let mut items = LoadedItems::default();
        for line in output.lines() {
            if line.starts_with("### ") {
                break;
            }
            if let Some(rest) = line.strip_prefix("loaded_targets:") {
                items.targets = split_comma_names(rest);
            } else if let Some(rest) = line.strip_prefix("loaded_tools:") {
                items.tools = split_comma_names(rest);
            }
        }
        return items;
    };
    let targets = value
        .get("loaded_targets")
        .and_then(Value::as_array)
        .map(|items| string_array_items(items))
        .unwrap_or_default();
    let tools = value
        .get("loaded_tools")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| {
                    item.as_str()
                        .or_else(|| item.get("name").and_then(Value::as_str))
                })
                .map(str::trim)
                .filter(|name| !name.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    LoadedItems { targets, tools }
}

fn split_comma_names(rest: &str) -> Vec<String> {
    rest.split(',')
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(str::to_string)
        .collect()
}

pub(in crate::agent) fn string_array_items(items: &[Value]) -> Vec<String> {
    items
        .iter()
        .filter_map(Value::as_str)
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(str::to_string)
        .collect()
}

pub(in crate::agent) fn compact_sent_meme_report(output: &str) -> Option<String> {
    const MAX_DESCRIPTION_CHARS: usize = 120;

    // 08-21 token-diet 文本形态:"sent meme {id}" 或 "sent meme {id}: {desc}"。
    // 旧 JSON 形态(历史回放)走下面的原路径。
    let (id, description): (String, Option<String>) =
        if let Some(rest) = output.trim().strip_prefix("sent meme ") {
            match rest.split_once(": ") {
                Some((id, description)) => (id.to_string(), Some(description.to_string())),
                None => (rest.to_string(), None),
            }
        } else {
            let value = serde_json::from_str::<Value>(output).ok()?;
            if value.get("success").and_then(Value::as_bool) != Some(true) {
                return None;
            }
            let id = value.get("id").and_then(Value::as_str)?.trim().to_string();
            let description = value
                .get("description")
                .and_then(Value::as_str)
                .map(str::to_string);
            (id, description)
        };
    let id = id.trim();
    if id.is_empty() {
        return None;
    }
    let description = description
        .as_deref()
        .map(compact_one_line)
        .filter(|description| !description.is_empty())
        .map(|description| truncate_chars(&description, MAX_DESCRIPTION_CHARS));
    let id = xml_text_escape(id);
    match description {
        Some(description) => Some(format!(
            "<sent_meme>发送了一个表情包：id={}；description={}</sent_meme>",
            id,
            xml_text_escape(&description)
        )),
        None => Some(format!("<sent_meme>发送了一个表情包：id={id}</sent_meme>")),
    }
}

pub(in crate::agent) fn compact_one_line(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

pub(in crate::agent) fn truncate_chars(value: &str, max_chars: usize) -> String {
    let mut output = String::new();
    for (index, ch) in value.chars().enumerate() {
        if index >= max_chars {
            output.push('…');
            return output;
        }
        output.push(ch);
    }
    output
}

pub(in crate::agent) fn xml_text_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

pub(in crate::agent) fn loaded_tools_from_messages(messages: &[ChatMessage]) -> BTreeSet<String> {
    let mut loaded = BTreeSet::new();
    for message in messages {
        let Some(ChatContent::Text(text)) = message.content.as_ref() else {
            continue;
        };
        collect_loaded_tools_from_text(text, &mut loaded);
    }
    loaded
}

pub(in crate::agent) fn collect_loaded_tools_from_text(text: &str, loaded: &mut BTreeSet<String>) {
    let mut rest = text;
    let start_tag = "<previous_tool_report name=\"load_tools\">";
    let end_tag = "</previous_tool_report>";
    while let Some(start) = rest.find(start_tag) {
        let body_start = start + start_tag.len();
        let Some(end) = rest[body_start..].find(end_tag) else {
            break;
        };
        let body = &rest[body_start..body_start + end];
        if let Ok(value) = serde_json::from_str::<Value>(body.trim()) {
            if let Some(names) = value.get("loaded_tools").and_then(Value::as_array) {
                for name in names.iter().filter_map(Value::as_str) {
                    if !name.trim().is_empty() {
                        loaded.insert(name.trim().to_string());
                    }
                }
            }
        }
        rest = &rest[body_start + end + end_tag.len()..];
    }
}

pub(in crate::agent) fn tool_event_name(name: &str, arguments: &str) -> String {
    let Ok(args) = serde_json::from_str::<Value>(arguments) else {
        return name.to_string();
    };
    match name {
        "load_skill" => args
            .get("name")
            .and_then(Value::as_str)
            .map(|skill| format!("load_skill:{skill}"))
            .unwrap_or_else(|| name.to_string()),
        "load_tools" => args
            .get("names")
            .and_then(Value::as_array)
            .map(|names| {
                names
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .collect::<Vec<_>>()
                    .join(",")
            })
            .filter(|tools| !tools.is_empty())
            .map(|tools| format!("load_tools:{tools}"))
            .unwrap_or_else(|| name.to_string()),
        // use_meme 有两个动作:show 要静默(图片自己会打出来),search 要照常
        // 显示摘要。渲染层只拿得到名字,所以把 action 编进事件名。
        // divine 四种占法都叫"占卜"毫无区分度(08-22 用户点名):method 编进
        // 事件名,渲染层按 divine:zhouyi/tarot/fortune/dice 取各自的显示名。
        "divine" => args
            .get("method")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|method| !method.is_empty())
            .map(|method| format!("divine:{method}"))
            .unwrap_or_else(|| name.to_string()),
        "use_meme" => args
            .get("action")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|action| !action.is_empty())
            .map(|action| format!("use_meme:{action}"))
            .unwrap_or_else(|| name.to_string()),
        // Each subagent gets a distinct event name so concurrent subagent
        // calls render as separate status lines instead of one aggregated
        // counter.
        "subagent" => args
            .get("description")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|description| !description.is_empty())
            .map(|description| {
                let truncated: String = description.chars().take(32).collect();
                format!("subagent:{truncated}")
            })
            .unwrap_or_else(|| name.to_string()),
        _ => name.to_string(),
    }
}
