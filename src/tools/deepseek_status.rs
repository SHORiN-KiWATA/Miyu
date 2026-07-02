use super::{ToolRegistry, ToolSpec};
use anyhow::{bail, Result};
use chrono::{FixedOffset, Utc};
use serde_json::{json, Value};
use tokio::process::Command;

const DEEPSEEK_STATUS_URL: &str = "https://status.deepseek.com/";

pub fn register(registry: &mut ToolRegistry) {
    registry.register(ToolSpec::new(
        "query_deepseek_status",
        "Query DeepSeek service status from the official status page.",
        json!({
            "type": "object",
            "properties": {
                "include_incidents": { "type": "boolean", "description": "Whether to include recent incidents, default true." },
                "max_incidents": { "type": "integer", "description": "Maximum recent incidents to return, 1-20, default 5." }
            },
            "additionalProperties": false
        }),
        |args| async move { query_deepseek_status(args).await },
    ));
}

async fn query_deepseek_status(args: Value) -> Result<String> {
    let include_incidents = args
        .get("include_incidents")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    let max_incidents = args
        .get("max_incidents")
        .and_then(Value::as_u64)
        .unwrap_or(5)
        .clamp(1, 20) as usize;
    let html = fetch_status_html().await?;
    let raw = parse_status_page(&html)?;
    Ok(serde_json::to_string_pretty(&build_response(
        &raw,
        include_incidents,
        max_incidents,
    ))?)
}

async fn fetch_status_html() -> Result<String> {
    match fetch_status_html_with_curl().await {
        Ok(html) => Ok(html),
        Err(curl_err) => match fetch_status_html_with_reqwest().await {
            Ok(html) => Ok(html),
            Err(reqwest_err) => bail!(
                "failed to fetch DeepSeek status page; curl error: {curl_err}; reqwest error: {reqwest_err}"
            ),
        },
    }
}

async fn fetch_status_html_with_curl() -> Result<String> {
    let output = Command::new("curl")
        .args(["-4", "-fsSL", "--max-time", "20", DEEPSEEK_STATUS_URL])
        .output()
        .await?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        bail!("curl exited with status {}: {stderr}", output.status)
    }
    Ok(String::from_utf8(output.stdout)?)
}

async fn fetch_status_html_with_reqwest() -> Result<String> {
    Ok(reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()?
        .get(DEEPSEEK_STATUS_URL)
        .header(reqwest::header::ACCEPT, "text/html")
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?)
}

fn parse_status_page(html: &str) -> Result<Value> {
    let target_keys = [
        "initialPageConfig",
        "active_changes",
        "initialCalendarData",
        "component_uptimes",
    ];
    let mut collected = serde_json::Map::new();
    let marker = "self.__next_f.push([1,\"";
    let mut offset = 0usize;
    while let Some(relative_start) = html[offset..].find(marker) {
        let content_start = offset + relative_start + marker.len();
        if let Some(raw) = next_push_payload(html, content_start) {
            if let Ok(decoded) = serde_json::from_str::<String>(&format!("\"{raw}\"")) {
                if let Some((_, data_str)) = decoded.split_once(':') {
                    let data = serde_json::from_str::<Value>(data_str)
                        .unwrap_or_else(|_| Value::String(data_str.to_string()));
                    extract_keys(&data, &target_keys, &mut collected);
                }
            }
        }
        offset = content_start;
    }
    if collected.is_empty() {
        bail!("failed to parse DeepSeek status page data")
    }
    Ok(Value::Object(collected))
}

fn next_push_payload(html: &str, start: usize) -> Option<&str> {
    let bytes = html.as_bytes();
    let mut pos = start;
    while pos < bytes.len() {
        if bytes[pos] == b'\\' && pos + 1 < bytes.len() {
            pos += 2;
            continue;
        }
        if bytes[pos] == b'"' && html[pos + 1..].starts_with("])") {
            return Some(&html[start..pos]);
        }
        pos += 1;
    }
    None
}

fn extract_keys(
    value: &Value,
    target_keys: &[&str],
    collected: &mut serde_json::Map<String, Value>,
) {
    match value {
        Value::Object(map) => {
            for key in target_keys {
                if !collected.contains_key(*key) {
                    if let Some(value) = map.get(*key) {
                        collected.insert((*key).to_string(), value.clone());
                    }
                }
            }
            for value in map.values() {
                extract_keys(value, target_keys, collected);
            }
        }
        Value::Array(items) => {
            for item in items {
                extract_keys(item, target_keys, collected);
            }
        }
        _ => {}
    }
}

fn build_response(raw: &Value, include_incidents: bool, max_incidents: usize) -> Value {
    let page_config = raw.get("initialPageConfig").unwrap_or(&Value::Null);
    let active_changes = raw
        .get("active_changes")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let component_uptimes = raw
        .get("component_uptimes")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut components = page_config
        .get("components")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .map(|component| {
                    let id = component.get("component_id").and_then(Value::as_str).unwrap_or("");
                    let uptime = component_uptimes.iter().find_map(|uptime| {
                        (uptime.get("component_id").and_then(Value::as_str) == Some(id))
                            .then(|| uptime.get("uptime").cloned())
                            .flatten()
                    });
                    json!({
                        "id": id,
                        "name": component.get("name").and_then(Value::as_str).unwrap_or(""),
                        "description": component.get("description").and_then(Value::as_str).unwrap_or(""),
                        "status": "operational",
                        "uptime_30d_percent": uptime,
                    })
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    for change in &active_changes {
        let change_status = change
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        let affected_ids = change
            .get("affected_components")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| item.get("component_id").and_then(Value::as_str))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        for component in &mut components {
            let Some(id) = component.get("id").and_then(Value::as_str) else {
                continue;
            };
            if affected_ids.contains(&id) {
                component["status"] = Value::String(change_status.to_string());
            }
        }
    }
    let active_count = active_changes.len();
    let (overall_status, status_readable) = if active_count == 0 {
        (
            "operational".to_string(),
            "All Systems Operational".to_string(),
        )
    } else {
        let worst = active_changes.iter().max_by_key(|change| {
            severity_rank(change.get("status").and_then(Value::as_str).unwrap_or(""))
        });
        let status = worst
            .and_then(|change| change.get("status").and_then(Value::as_str))
            .unwrap_or("unknown");
        (
            status.to_string(),
            format!(
                "{} - {active_count} active incident(s)",
                title_status(status)
            ),
        )
    };
    let recent_incidents = if include_incidents {
        raw.get("initialCalendarData")
            .and_then(|value| value.get("changes"))
            .and_then(Value::as_array)
            .map(|changes| {
                changes
                    .iter()
                    .take(max_incidents)
                    .map(format_incident)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default()
    } else {
        Vec::new()
    };
    json!({
        "success": true,
        "queried_at": now_cn_iso(),
        "overall_status": overall_status,
        "status_readable": status_readable,
        "active_incidents_count": active_count,
        "components": components,
        "recent_incidents": recent_incidents,
    })
}

fn format_incident(change: &Value) -> Value {
    let started = change.get("start_at_seconds").and_then(Value::as_f64);
    let resolved = change.get("close_at_seconds").and_then(Value::as_f64);
    let duration = match (started, resolved) {
        (Some(started), Some(resolved)) => Some((resolved - started) as i64),
        _ => None,
    };
    json!({
        "id": change.get("change_id"),
        "title": change.get("title").and_then(Value::as_str).unwrap_or(""),
        "type": change.get("type").and_then(Value::as_str).unwrap_or("incident"),
        "status": change.get("status").and_then(Value::as_str).unwrap_or(""),
        "started_at": started.and_then(epoch_to_cn_iso),
        "resolved_at": resolved.and_then(epoch_to_cn_iso),
        "duration_seconds": duration,
        "affected_components": change.get("affected_components").and_then(Value::as_array).map(|items| items.iter().map(|item| item.get("name").or_else(|| item.get("component_name")).and_then(Value::as_str).unwrap_or("")).collect::<Vec<_>>()).unwrap_or_default(),
        "timeline": change.get("updates").and_then(Value::as_array).map(|items| items.iter().map(|item| json!({
            "time": item.get("at_seconds").and_then(Value::as_f64).and_then(epoch_to_cn_iso),
            "status": item.get("status").and_then(Value::as_str).unwrap_or(""),
            "description": item.get("description").and_then(Value::as_str).unwrap_or(""),
        })).collect::<Vec<_>>()).unwrap_or_default(),
    })
}

fn severity_rank(status: &str) -> u8 {
    match status {
        "full_outage" => 4,
        "partial_outage" => 3,
        "degraded" => 2,
        "maintenance" => 1,
        _ => 0,
    }
}

fn title_status(status: &str) -> String {
    status
        .split('_')
        .map(|part| {
            let mut chars = part.chars();
            match chars.next() {
                Some(first) => format!("{}{}", first.to_ascii_uppercase(), chars.as_str()),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn now_cn_iso() -> String {
    let offset = FixedOffset::east_opt(8 * 3600).expect("valid fixed offset");
    Utc::now().with_timezone(&offset).to_rfc3339()
}

fn epoch_to_cn_iso(seconds: f64) -> Option<String> {
    let offset = FixedOffset::east_opt(8 * 3600)?;
    chrono::DateTime::from_timestamp(seconds as i64, 0)
        .map(|time| time.with_timezone(&offset).to_rfc3339())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_next_payload_data() {
        let data = json!({"initialPageConfig":{"components":[]}}).to_string();
        let encoded = serde_json::to_string(&format!("1:{data}")).unwrap();
        let html = format!("self.__next_f.push([1,{encoded}])");
        let parsed = parse_status_page(&html).unwrap();
        assert!(parsed.get("initialPageConfig").is_some());
    }
}
