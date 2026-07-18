use super::{ToolRegistry, ToolSpec};
use anyhow::{bail, Result};
use serde_json::{json, Value};
use std::collections::BTreeSet;

const BASE_DESCRIPTION: &str = "按需加载工具、脚本或工具组的完整说明和参数 schema。请从 <available_load_targets> 中选择 <name>，并使用 {\"names\":[\"名称\"]} 加载。type=tool/script 表示加载单个工具；type=group 表示加载该组所有未加载工具。<unregistered_scripts> 中的文件尚未注册为工具，不能直接加载或调用；需要先读取对应路径并使用 register_script 注册。";

pub fn register(registry: &mut ToolRegistry) {
    registry.register(
        ToolSpec::new(
            "load_tools",
            BASE_DESCRIPTION,
            json!({
                "type": "object",
                "properties": {
                    "names": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "要加载的工具、脚本或工具组名称列表。只允许填写 available_load_targets 中的 name，例如 web_search 或 group:gaming。"
                    }
                },
                "required": ["names"],
                "additionalProperties": false
            }),
            |_args| async {
                bail!("load_tools must be executed through the active tool registry")
            },
        )
        .with_display_name("加载"),
    );
}

pub(super) fn dynamic_description(registry: &ToolRegistry, loaded: &BTreeSet<String>) -> String {
    format!(
        "{BASE_DESCRIPTION}\n\n{}\n{}",
        registry.load_targets_xml(loaded),
        unregistered_scripts_xml(registry),
    )
}

pub(super) fn execute(args: Value, registry: &ToolRegistry) -> Result<String> {
    let names = args
        .get("names")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow::anyhow!("names array is required"))?;
    if names.is_empty() {
        bail!("names must not be empty");
    }

    let requested = names
        .iter()
        .filter_map(|value| value.as_str().map(str::trim).map(str::to_string))
        .filter(|value| !value.is_empty())
        .collect::<Vec<_>>();
    let (loaded_targets, loaded_tools) =
        registry.expand_load_targets(&requested, &BTreeSet::new())?;

    if loaded_tools.is_empty() {
        bail!("names must contain at least one loadable tool, script, or group");
    }

    Ok(serde_json::to_string_pretty(&json!({
        "loaded_targets": loaded_targets,
        "loaded_tools": loaded_tools,
        "note": "loaded"
    }))?)
}

fn unregistered_scripts_xml(registry: &ToolRegistry) -> String {
    let items = registry
        .unregistered_scripts()
        .iter()
        .map(|script| {
            format!(
                "  <script>\n    <name>{}</name>\n    <path>{}</path>\n  </script>",
                xml_escape(&script.name),
                xml_escape(&script.path),
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    format!("<unregistered_scripts>\n{items}\n</unregistered_scripts>")
}

fn xml_escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

#[cfg(test)]
mod tests {
    use super::super::tool_descriptions::LoadPolicy;
    use super::*;

    #[test]
    fn description_separates_builtin_scripts_and_unregistered_files() {
        let mut registry = ToolRegistry::new();
        register(&mut registry);
        registry.register(
            ToolSpec::new(
                "custom_builtin",
                "Built in",
                json!({"type":"object","properties":{}}),
                |_| async { Ok(String::new()) },
            )
            .with_always_loaded(false),
        );
        registry
            .replace_script_tools(
                vec![ToolSpec::new(
                    "lazy_script",
                    "Lazy script",
                    json!({"type":"object","properties":{"query":{"type":"string"}}}),
                    |_| async { Ok(String::new()) },
                )
                .script()
                .with_always_loaded(false)],
                vec![super::super::registry::UnregisteredScript {
                    name: "unknown_script".to_string(),
                    path: "unknown-script".to_string(),
                }],
            )
            .unwrap();

        let description = dynamic_description(&registry, &BTreeSet::new());
        assert!(description.contains("<available_load_targets>"));
        assert!(description.contains("custom_builtin"));
        assert!(description.contains("lazy_script"));
        assert!(description.contains("<unregistered_scripts>"));
        assert!(description.contains("unknown-script"));
    }

    #[tokio::test]
    async fn registry_loads_dynamic_script_definition() {
        let mut registry = ToolRegistry::new();
        register(&mut registry);
        registry
            .replace_script_tools(
                vec![ToolSpec::new(
                    "lazy_script",
                    "Lazy script",
                    json!({"type":"object","properties":{}}),
                    |_| async { Ok(String::new()) },
                )
                .script()
                .with_always_loaded(false)],
                Vec::new(),
            )
            .unwrap();

        let output = registry
            .call("load_tools", r#"{"names":["lazy_script"]}"#)
            .await
            .unwrap();
        assert!(output.contains("lazy_script"));
        assert!(output.contains("\"loaded_tools\""));
    }

    #[tokio::test]
    async fn registry_loads_group_target_definition() {
        let mut registry = ToolRegistry::new();
        register(&mut registry);
        registry.register(
            ToolSpec::new(
                "protondb_query",
                "Query ProtonDB compatibility",
                json!({"type":"object","properties":{"game":{"type":"string"}}}),
                |_| async { Ok(String::new()) },
            )
            .with_always_loaded(false)
            .with_load_policy(LoadPolicy::Group)
            .with_groups(vec!["gaming".to_string()]),
        );

        let description = dynamic_description(&registry, &BTreeSet::new());
        assert!(description.contains("group:gaming"));
        assert!(description.contains("protondb_query"));

        let output = registry
            .call("load_tools", r#"{"names":["group:gaming"]}"#)
            .await
            .unwrap();
        let value: Value = serde_json::from_str(&output).unwrap();
        assert_eq!(
            value["loaded_targets"].as_array().unwrap()[0]
                .as_str()
                .unwrap(),
            "group:gaming"
        );
        assert_eq!(
            value["loaded_tools"].as_array().unwrap()[0]
                .as_str()
                .unwrap(),
            "protondb_query"
        );
    }
}
