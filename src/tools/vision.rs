use super::{ToolRegistry, ToolSpec};
use crate::config::{AppConfig, PrintImagePluginConfig, ProviderConfig, VisionPluginConfig};
use crate::llm::{ChatMessage, OpenAiCompatibleClient};
use crate::paths::MiyuPaths;
use anyhow::{bail, Context, Result};
use base64::Engine;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use tokio::process::Command;

const MAX_IMAGE_BYTES: usize = 10 * 1024 * 1024;

pub fn register(
    registry: &mut ToolRegistry,
    config: AppConfig,
    paths: MiyuPaths,
    register_analyze: bool,
) {
    if !register_analyze {
        return;
    }
    registry.register(ToolSpec::new(
        "vision_analyze",
        "Analyze an image using the current multimodal model or a configured vision provider. Supports local image paths and http(s) image URLs.",
        json!({
            "type": "object",
            "properties": {
                "image": { "type": "string", "description": "Local image path or http(s) image URL." },
                "prompt": { "type": "string", "description": "Question or instruction for image analysis. Defaults to a concise description." }
            },
            "required": ["image"],
            "additionalProperties": false
        }),
        move |args| {
            let config = config.clone();
            let paths = paths.clone();
            async move { analyze_image(args, config, paths).await }
        },
    ));
}

pub fn register_print(registry: &mut ToolRegistry, config: AppConfig) {
    if !config.plugins.print_image.enabled {
        return;
    }
    registry.register(ToolSpec::new(
        "print_image",
        "Print/render a local image directly in the current terminal output using chafa. Use this when the user asks to show, print, render, or preview an image, or when you need to inspect an image visually in the terminal before answering.",
        json!({
            "type": "object",
            "properties": {
                "image": { "type": "string", "description": "Local image path." },
                "size": { "type": "string", "description": "Optional chafa size, e.g. 80x40. Use this or width/height to avoid oversized output." },
                "width": { "type": "integer", "description": "Optional output width in terminal cells, e.g. 80." },
                "height": { "type": "integer", "description": "Optional output height in terminal cells, e.g. 40." }
            },
            "required": ["image"],
            "additionalProperties": false
        }),
        move |args| {
            let print_config = config.plugins.print_image.clone();
            async move { print_image(args, &print_config).await }
        },
    ));
}

async fn print_image(args: Value, print_config: &PrintImagePluginConfig) -> Result<String> {
    let image = args
        .get("image")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim();
    if image.is_empty() {
        bail!("image is required")
    }
    let path = expand_path(image);
    let metadata = std::fs::metadata(&path)
        .with_context(|| format!("failed to stat image {}", path.display()))?;
    if !metadata.is_file() {
        bail!("image path is not a file: {}", path.display())
    }
    print_image_file(&path, print_size(&args, print_config)).await?;
    Ok(format!("printed image in terminal: {}", path.display()))
}

pub async fn print_image_file(path: &Path, size: Option<String>) -> Result<()> {
    let mut command = Command::new("chafa");
    if let Some(size) = size {
        command.arg("--size").arg(size);
    }
    let status = command
        .arg(path)
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .await
        .with_context(|| "failed to run chafa; install chafa or disable terminal image printing")?;
    if !status.success() {
        bail!("chafa exited with status {status}")
    }
    Ok(())
}

pub fn configured_print_size(print_config: &PrintImagePluginConfig) -> Option<String> {
    let (cols, rows) = crossterm::terminal::size().ok()?;
    let width = ((cols as u32 * print_config.width_percent as u32) / 100).max(1);
    let height = ((rows as u32 * print_config.height_percent as u32) / 100).max(1);
    Some(format!("{}x{}", width.min(300), height.min(200)))
}

fn print_size(args: &Value, print_config: &PrintImagePluginConfig) -> Option<String> {
    let width = args
        .get("width")
        .and_then(Value::as_u64)
        .unwrap_or(0)
        .min(300);
    let height = args
        .get("height")
        .and_then(Value::as_u64)
        .unwrap_or(0)
        .min(200);
    match (width, height) {
        (0, 0) => args
            .get("size")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
            .or_else(|| configured_print_size(print_config)),
        (width, 0) => Some(format!("{width}x")),
        (0, height) => Some(format!("x{height}")),
        (width, height) => Some(format!("{width}x{height}")),
    }
}

async fn analyze_image(args: Value, config: AppConfig, paths: MiyuPaths) -> Result<String> {
    let vision = &config.plugins.vision;
    if !vision.enabled {
        bail!("vision plugin is disabled")
    }
    let image = args
        .get("image")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim();
    if image.is_empty() {
        bail!("image is required")
    }
    let prompt = args
        .get("prompt")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .unwrap_or("请简洁描述这张图片，并指出重要细节。")
        .trim();
    let image_url = if image.starts_with("http://") || image.starts_with("https://") {
        image.to_string()
    } else {
        local_image_data_url(image)?
    };
    let provider = vision_provider(&config, vision)?;
    let client = OpenAiCompatibleClient::new(&provider, &config, &paths)?;
    let result = client
        .chat_stream(
            vec![
                ChatMessage::system(
                    "你是 Miyu 的识图助手。请基于图片内容回答，不要编造看不见的信息。",
                ),
                ChatMessage::user_with_image(prompt, image_url),
            ],
            Vec::new(),
            |_| Ok(()),
        )
        .await?;
    if result.content.trim().is_empty() {
        bail!("vision model returned empty response")
    }
    Ok(result.content)
}

fn vision_provider(config: &AppConfig, vision: &VisionPluginConfig) -> Result<ProviderConfig> {
    let mut provider = if !vision.vision_provider_id.trim().is_empty() {
        config
            .provider(Some(vision.vision_provider_id.trim()))?
            .clone()
    } else {
        config.provider(None)?.clone()
    };
    if !vision.vision_model.trim().is_empty() {
        provider.default_model = vision.vision_model.trim().to_string();
    }
    Ok(provider)
}

fn local_image_data_url(value: &str) -> Result<String> {
    let path = expand_path(value);
    let metadata = std::fs::metadata(&path)
        .with_context(|| format!("failed to stat image {}", path.display()))?;
    if !metadata.is_file() {
        bail!("image path is not a file: {}", path.display())
    }
    if metadata.len() as usize > MAX_IMAGE_BYTES {
        bail!("image too large: {} bytes", metadata.len())
    }
    let bytes =
        std::fs::read(&path).with_context(|| format!("failed to read image {}", path.display()))?;
    let mime = mime_from_path(&path)?;
    let encoded = base64::engine::general_purpose::STANDARD.encode(bytes);
    Ok(format!("data:{mime};base64,{encoded}"))
}

fn expand_path(value: &str) -> PathBuf {
    let value = value.trim();
    if let Some(rest) = value.strip_prefix("~/") {
        if let Some(home) = directories::BaseDirs::new().map(|dirs| dirs.home_dir().to_path_buf()) {
            return home.join(rest);
        }
    }
    let path = Path::new(value);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(path)
    }
}

fn mime_from_path(path: &Path) -> Result<&'static str> {
    match path
        .extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase()
        .as_str()
    {
        "jpg" | "jpeg" => Ok("image/jpeg"),
        "png" => Ok("image/png"),
        "webp" => Ok("image/webp"),
        "gif" => Ok("image/gif"),
        value => {
            bail!("unsupported image extension: {value}; supported: jpg, jpeg, png, webp, gif")
        }
    }
}
