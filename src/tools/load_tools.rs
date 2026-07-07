use super::{tool_descriptions, ToolRegistry, ToolSpec};
use anyhow::{bail, Result};
use serde_json::{json, Value};
use std::collections::BTreeSet;

pub fn register(registry: &mut ToolRegistry) {
    let allowed_tools = registry.tool_names().into_iter().collect::<BTreeSet<_>>();
    let description = format!(
        "按需加载内置工具的完整说明和参数 schema。加载后的内容会作为工具结果保留在当前对话上下文中\n\n{}",
        available_tools_xml(&allowed_tools)
    );
    registry.register(
        ToolSpec::new(
            "load_tools",
            description,
            json!({
                "type": "object",
                "properties": {
                    "names": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "使用前必须需要先加载的工具的列表。"
                    }
                },
                "required": ["names"]
            }),
            move |args| {
                let allowed_tools = allowed_tools.clone();
                async move { load_tools(args, &allowed_tools) }
            },
        )
        .with_display_name("加载工具"),
    );
}

fn load_tools(args: Value, allowed_tools: &BTreeSet<String>) -> Result<String> {
    let names = args
        .get("names")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow::anyhow!("names array is required"))?;
    if names.is_empty() {
        bail!("names must not be empty");
    }

    let mut loaded = Vec::new();
    for value in names {
        let name = value
            .as_str()
            .unwrap_or_default()
            .trim();
        if name.is_empty() {
            continue;
        }
        if !allowed_tools.contains(name) {
            bail!("tool is not available in the current mode: {name}");
        }
        let Some(desc) = tool_descriptions::get(name) else {
            bail!("unknown tool: {name}");
        };
        loaded.push(json!({
            "name": desc.name,
            "display_name": desc.display_name,
            "description": desc.description,
            "parameters": desc.parameters,
            "permission": desc.permission,
        }));
    }

    Ok(serde_json::to_string_pretty(&json!({
        "loaded_tools": loaded,
        "note": "这些工具的完整定义已加载到当前对话上下文；后续可以直接按对应 name 调用。"
    }))?)
}

fn available_tools_xml(allowed_tools: &BTreeSet<String>) -> String {
    let items = tool_descriptions::on_demand_descriptions()
        .iter()
        .filter(|desc| allowed_tools.contains(&desc.name))
        .map(|desc| {
            format!(
                "  <tool>\n    <name>{}</name>\n    <display_name>{}</display_name>\n    <description>{}</description>\n  </tool>",
                xml_escape(&desc.name),
                xml_escape(&desc.display_name),
                xml_escape(&desc.description)
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    format!("<available_tools>\n{items}\n</available_tools>")
}

fn xml_escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}
