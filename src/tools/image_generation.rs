use super::{vision, ToolRegistry, ToolSpec};
use crate::config::{AppConfig, ImageGenerationPluginConfig};
use anyhow::{bail, Context, Result};
use base64::Engine;
use chrono::Local;
use reqwest::Client;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::time::Duration;

pub fn register(registry: &mut ToolRegistry, config: AppConfig) {
    registry.register(ToolSpec::new(
        "generate_image",
        "Generate an image from a text prompt using the configured OpenAI or RightCode image API. Returns a local image path. In the final assistant response, always include the returned path so the user can reuse it. Do not call print_image after this tool unless the user explicitly asks to display/print/preview the image; if this tool returns printed=true, never call print_image for the same image.",
        json!({
            "type": "object",
            "properties": {
                "prompt": { "type": "string", "description": "Image generation prompt." },
                "aspect_ratio": { "type": "string", "enum": ["自动", "1:1", "2:3", "3:2", "3:4", "4:3", "4:5", "5:4", "9:16", "16:9", "21:9"], "description": "Optional aspect ratio override." },
                "resolution": { "type": "string", "enum": ["1K", "2K", "4K"], "description": "Optional resolution hint for RightCode." }
            },
            "required": ["prompt"],
            "additionalProperties": false
        }),
        move |args| {
            let config = config.clone();
            async move { generate_image(args, config).await }
        },
    ));
}

async fn generate_image(args: Value, config: AppConfig) -> Result<String> {
    let plugin = &config.plugins.image_generation;
    if !plugin.enabled {
        bail!("image generation plugin is disabled")
    }
    let prompt = args
        .get("prompt")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim();
    if prompt.is_empty() {
        bail!("prompt is required")
    }
    let aspect_ratio = args
        .get("aspect_ratio")
        .and_then(Value::as_str)
        .unwrap_or(&plugin.default_aspect_ratio)
        .trim();
    let resolution = args
        .get("resolution")
        .and_then(Value::as_str)
        .unwrap_or(&plugin.default_resolution)
        .trim();
    let bytes = request_image(plugin, prompt, aspect_ratio, resolution).await?;
    let path = save_image(plugin, prompt, &bytes)?;
    let print_error = if plugin.auto_print && config.plugins.print_image.enabled {
        vision::print_image_file(
            &path,
            vision::configured_print_size(&config.plugins.print_image),
        )
        .await
        .err()
        .map(|err| err.to_string())
    } else {
        None
    };
    let printed = plugin.auto_print && config.plugins.print_image.enabled && print_error.is_none();
    let path_text = path.display().to_string();
    Ok(json!({
        "status": "ok",
        "path": path_text,
        "final_response_must_include_path": path.display().to_string(),
        "bytes": bytes.len(),
        "printed": printed,
        "print_error": print_error,
        "assistant_instruction": if printed {
            "The generated image has already been printed in the terminal. In your final response, include the exact local image path from final_response_must_include_path. Do not call print_image again unless the user asks to print it again."
        } else {
            "The generated image was saved to disk. In your final response, include the exact local image path from final_response_must_include_path. Do not call print_image unless the user explicitly asked to display, print, render, preview, or show it."
        }
    })
    .to_string())
}

async fn request_image(
    plugin: &ImageGenerationPluginConfig,
    prompt: &str,
    aspect_ratio: &str,
    resolution: &str,
) -> Result<Vec<u8>> {
    let api_key = plugin
        .api_keys
        .iter()
        .map(String::as_str)
        .map(str::trim)
        .find(|key| !key.is_empty())
        .context("plugins.image_generation.api_keys is empty")?;
    let base = plugin.base_url.trim().trim_end_matches('/');
    if base.is_empty() {
        bail!("plugins.image_generation.base_url is empty")
    }
    let client = Client::builder()
        .timeout(Duration::from_secs(plugin.timeout_seconds))
        .build()?;
    let response = client
        .post(format!("{base}/v1/images/generations"))
        .bearer_auth(api_key)
        .json(&payload(plugin, prompt, aspect_ratio, resolution))
        .send()
        .await?;
    let status = response.status();
    if !status.is_success() {
        let text = response.text().await.unwrap_or_default();
        bail!("image API error ({status}): {}", preview(&text, 500));
    }
    let data: Value = response.json().await?;
    extract_image(&client, data).await
}

fn payload(
    plugin: &ImageGenerationPluginConfig,
    prompt: &str,
    aspect_ratio: &str,
    resolution: &str,
) -> Value {
    let rightcode = plugin.provider_type == "rightcode";
    let mut payload = json!({
        "model": plugin.model,
        "prompt": prompt,
        "n": 1,
    });
    if let Some(size) = resolve_size(plugin, aspect_ratio, resolution) {
        payload["size"] = Value::String(size);
    }
    if rightcode {
        if !aspect_ratio.is_empty() && aspect_ratio != "自动" {
            payload["aspect_ratio"] = Value::String(aspect_ratio.to_string());
        }
    } else if !looks_gpt_image_model(&plugin.model) {
        payload["response_format"] = Value::String("b64_json".to_string());
    }
    payload
}

async fn extract_image(client: &Client, response: Value) -> Result<Vec<u8>> {
    let data = response
        .get("data")
        .and_then(Value::as_array)
        .context("image response missing data array")?;
    let first = data.first().context("image response data is empty")?;
    if let Some(b64) = first.get("b64_json").and_then(Value::as_str) {
        return base64::engine::general_purpose::STANDARD
            .decode(b64)
            .context("failed to decode b64_json image");
    }
    if let Some(url) = first.get("url").and_then(Value::as_str) {
        let response = client.get(url).send().await?;
        let status = response.status();
        if !status.is_success() {
            bail!("failed to download generated image ({status})")
        }
        return Ok(response.bytes().await?.to_vec());
    }
    bail!("image response contains neither b64_json nor url")
}

fn save_image(plugin: &ImageGenerationPluginConfig, prompt: &str, bytes: &[u8]) -> Result<PathBuf> {
    let output_dir = expand_path(&plugin.output_dir);
    std::fs::create_dir_all(&output_dir)
        .with_context(|| format!("failed to create output dir {}", output_dir.display()))?;
    let filename = format!(
        "{}-{}.png",
        slug(prompt),
        Local::now().format("%Y%m%d-%H%M%S")
    );
    let path = output_dir.join(filename);
    std::fs::write(&path, bytes).with_context(|| format!("failed to write {}", path.display()))?;
    Ok(path)
}

fn resolve_size(
    plugin: &ImageGenerationPluginConfig,
    aspect_ratio: &str,
    resolution: &str,
) -> Option<String> {
    if plugin.provider_type == "rightcode" {
        return match (aspect_ratio, resolution) {
            ("1:1", _) => Some("1024x1024".to_string()),
            ("3:2", "2K" | "4K") => Some("2048x1365".to_string()),
            ("16:9", "2K" | "4K") => Some("2048x1152".to_string()),
            ("4:3", "2K" | "4K") => Some("2048x1536".to_string()),
            ("5:4", "2K" | "4K") => Some("2048x1638".to_string()),
            ("21:9", "2K" | "4K") => Some("2048x878".to_string()),
            ("2:3", "2K" | "4K") => Some("1365x2048".to_string()),
            ("3:4", "2K" | "4K") => Some("1536x2048".to_string()),
            ("9:16", "2K" | "4K") => Some("1152x2048".to_string()),
            ("4:5", "2K" | "4K") => Some("1638x2048".to_string()),
            ("3:2", _) => Some("1536x1024".to_string()),
            ("16:9", _) => Some("1536x864".to_string()),
            ("4:3", _) => Some("1365x1024".to_string()),
            ("5:4", _) => Some("1280x1024".to_string()),
            ("21:9", _) => Some("1536x658".to_string()),
            ("2:3", _) => Some("1024x1536".to_string()),
            ("3:4", _) => Some("1024x1365".to_string()),
            ("9:16", _) => Some("864x1536".to_string()),
            ("4:5", _) => Some("1024x1280".to_string()),
            _ => None,
        };
    }
    if looks_gpt_image_model(&plugin.model) {
        return Some(
            match aspect_ratio {
                "1:1" => "1024x1024",
                "2:3" | "3:4" | "9:16" | "4:5" => "1024x1536",
                "3:2" | "16:9" | "4:3" | "5:4" | "21:9" => "1536x1024",
                _ => "auto",
            }
            .to_string(),
        );
    }
    Some(
        match aspect_ratio {
            "2:3" | "3:4" | "9:16" | "4:5" => "1024x1792",
            "3:2" | "16:9" | "4:3" | "5:4" | "21:9" => "1792x1024",
            _ => "1024x1024",
        }
        .to_string(),
    )
}

fn looks_gpt_image_model(model: &str) -> bool {
    model.to_ascii_lowercase().contains("gpt-image")
}

fn expand_path(value: &str) -> PathBuf {
    if let Some(rest) = value.trim().strip_prefix("~/") {
        if let Some(home) = directories::BaseDirs::new().map(|dirs| dirs.home_dir().to_path_buf()) {
            return home.join(rest);
        }
    }
    let path = Path::new(value.trim());
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(path)
    }
}

fn slug(value: &str) -> String {
    let mut out = value
        .chars()
        .filter_map(|ch| {
            if ch.is_ascii_alphanumeric() {
                Some(ch.to_ascii_lowercase())
            } else if ch.is_ascii_whitespace() || matches!(ch, '-' | '_') {
                Some('-')
            } else {
                None
            }
        })
        .collect::<String>();
    while out.contains("--") {
        out = out.replace("--", "-");
    }
    let out = out.trim_matches('-');
    if out.is_empty() {
        "image".to_string()
    } else {
        out.chars().take(48).collect()
    }
}

fn preview(value: &str, limit: usize) -> String {
    let normalized = value.split_whitespace().collect::<Vec<_>>().join(" ");
    if normalized.chars().count() <= limit {
        normalized
    } else {
        format!("{}...", normalized.chars().take(limit).collect::<String>())
    }
}
