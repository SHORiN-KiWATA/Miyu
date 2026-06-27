use super::{ToolRegistry, ToolSpec};
use anyhow::{bail, Result};
use serde_json::{json, Value};

pub fn register(registry: &mut ToolRegistry) {
    registry.register(ToolSpec::new(
        "get_weather",
        "Query current weather via wttr.in. Use for weather questions. Location can be city name, airport code, or empty for auto-detected location.",
        json!({
            "type": "object",
            "properties": {
                "location": { "type": "string", "description": "City/location, for example Tokyo or Beijing. Empty means auto-detect." }
            },
            "additionalProperties": false
        }),
        |args| async move { get_weather(args).await },
    ));
}

async fn get_weather(args: Value) -> Result<String> {
    let location = args
        .get("location")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();
    let path = if location.is_empty() {
        String::new()
    } else {
        format!("/{}", urlencoding::encode(location))
    };
    let url = format!("https://wttr.in{path}?format=%C+%t+%w+%l");
    let text = reqwest::Client::new()
        .get(url)
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    let text = text.trim();
    if text.is_empty() {
        bail!("weather response was empty");
    }
    Ok(format!(
        "current weather(condition,temperature,wind,location): {text}"
    ))
}
