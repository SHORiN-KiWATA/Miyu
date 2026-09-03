//! MCP 桥对外吐的工具 schema 按上游模型方言整形。
//!
//! Miyu 的工具 schema 是给 OpenAI/Anthropic 线写的,那两家对 JSON Schema 来
//! 者不拒;Gemini 的 function declaration 校验严得多——`enum` 里有空串直接
//! 400(`properties[site].enum[4]: cannot be empty`,09-03 antigravity 中转首跑
//! 撞上),`type` 不能是数组,`additionalProperties`/`pattern`/`default` 这些键
//! 不认。整形只发生在桥上、只在拉起方点名 `MIYU_MCP_SCHEMA_DIALECT=gemini`
//! 时——工具自己的 schema 一个字不改,别的供应商照旧。

use serde_json::{json, Map, Value};

/// Gemini 认识的 schema 键;其余一律剔除。
const GEMINI_KEYS: &[&str] = &[
    "type",
    "description",
    "enum",
    "items",
    "properties",
    "required",
    "nullable",
    "format",
    "minimum",
    "maximum",
    "minItems",
    "maxItems",
];

pub(in crate::cli) fn shape_for_dialect(schema: Value, dialect: &str) -> Value {
    match dialect {
        "gemini" => gemini_compatible(schema),
        _ => schema,
    }
}

fn gemini_compatible(schema: Value) -> Value {
    let Value::Object(map) = schema else {
        return schema;
    };
    let mut out = Map::new();
    let mut nullable = map
        .get("nullable")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    for (key, value) in map {
        if !GEMINI_KEYS.contains(&key.as_str()) {
            continue;
        }
        match key.as_str() {
            "type" => {
                // 联合类型取第一个非 null 的;null 折成 nullable。
                let picked = match &value {
                    Value::Array(types) => {
                        if types.iter().any(|t| t.as_str() == Some("null")) {
                            nullable = true;
                        }
                        types
                            .iter()
                            .find(|t| t.as_str() != Some("null"))
                            .cloned()
                            .unwrap_or(Value::String("string".into()))
                    }
                    other => other.clone(),
                };
                out.insert(key, picked);
            }
            "enum" => {
                if let Value::Array(values) = value {
                    let kept: Vec<Value> = values
                        .into_iter()
                        .filter(|v| v.as_str().is_none_or(|s| !s.is_empty()))
                        .collect();
                    if !kept.is_empty() {
                        out.insert(key, Value::Array(kept));
                    }
                }
            }
            "properties" => {
                if let Value::Object(props) = value {
                    let shaped: Map<String, Value> = props
                        .into_iter()
                        .map(|(name, sub)| (name, gemini_compatible(sub)))
                        .collect();
                    // 空 properties 的 OBJECT 会被拒("should be non-empty");
                    // 干脆不带这个键,让上游按无参处理。
                    if !shaped.is_empty() {
                        out.insert(key, Value::Object(shaped));
                    }
                }
            }
            "items" => {
                out.insert(key, gemini_compatible(value));
            }
            "required" => {
                if let Value::Array(names) = &value {
                    if !names.is_empty() {
                        out.insert(key, value);
                    }
                }
            }
            "nullable" => {}
            _ => {
                out.insert(key, value);
            }
        }
    }
    // required 里引用的属性要真的存在(属性可能刚被剔掉)。
    if let Some(Value::Array(required)) = out.get("required").cloned() {
        let present = out
            .get("properties")
            .and_then(Value::as_object)
            .map(|props| {
                required
                    .into_iter()
                    .filter(|name| name.as_str().is_some_and(|n| props.contains_key(n)))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        if present.is_empty() {
            out.remove("required");
        } else {
            out.insert("required".into(), Value::Array(present));
        }
    }
    if nullable {
        out.insert("nullable".into(), json!(true));
    }
    Value::Object(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_enum_entries_and_unknown_keys_are_dropped() {
        let shaped = shape_for_dialect(
            json!({
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "site": { "type": "string", "enum": ["a", "", "b"], "default": "a", "pattern": "^a" },
                    "seat": { "type": ["string", "array"], "items": { "type": "string" } },
                    "flag": { "type": ["null", "boolean"] }
                },
                "required": ["site", "gone"]
            }),
            "gemini",
        );
        assert!(shaped.get("additionalProperties").is_none());
        assert_eq!(shaped["properties"]["site"]["enum"], json!(["a", "b"]));
        assert!(shaped["properties"]["site"].get("default").is_none());
        assert!(shaped["properties"]["site"].get("pattern").is_none());
        assert_eq!(shaped["properties"]["seat"]["type"], "string");
        assert_eq!(shaped["properties"]["flag"]["type"], "boolean");
        assert_eq!(shaped["properties"]["flag"]["nullable"], true);
        assert_eq!(shaped["required"], json!(["site"]));
    }

    #[test]
    fn empty_object_properties_are_omitted_and_other_dialects_untouched() {
        let raw = json!({ "type": "object", "properties": {}, "additionalProperties": false });
        let shaped = shape_for_dialect(raw.clone(), "gemini");
        assert_eq!(shaped, json!({ "type": "object" }));
        assert_eq!(shape_for_dialect(raw.clone(), ""), raw);
    }
}
