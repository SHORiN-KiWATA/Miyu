use super::{ToolRegistry, ToolSpec};
use crate::clipboard::ClipboardContent;
use crate::i18n::text as t;
use crate::paths::MiyuPaths;
use anyhow::{bail, Result};
use serde_json::{json, Value};
use std::env;
use std::path::Path;

#[derive(Clone, Copy, Eq, PartialEq)]
enum PreferredType {
    Auto,
    Text,
    Image,
}

pub fn register(registry: &mut ToolRegistry, paths: MiyuPaths) {
    registry.register(ToolSpec::new(
        "read_clipboard",
        t(
            "Read the current Linux clipboard and automatically detect whether it contains text, an image, or a copied local file path. Supports wl-paste, xclip, or xsel.",
            "读取当前 Linux 剪贴板，并自动判断其中是文本、图片还是复制的本地文件路径。支持 wl-paste、xclip 或 xsel。",
        ),
        json!({
            "type": "object",
            "properties": {
                "preferred_type": {
                    "type": "string",
                    "enum": ["auto", "text", "image"],
                    "description": t("Legacy preference hint. The tool still auto-detects image/file-path content first to avoid reading binary clipboard data as text. Defaults to auto.", "兼容用的偏好提示。工具仍会优先自动识别图片和文件路径，避免把二进制剪贴板内容当文本读取。默认 auto。"),
                    "default": "auto"
                }
            },
            "additionalProperties": false
        }),
        move |args| {
            let paths = paths.clone();
            async move { read_clipboard_tool(args, paths) }
        },
    ));
}

fn read_clipboard_tool(args: Value, paths: MiyuPaths) -> Result<String> {
    if std::env::consts::OS != "linux" {
        return Ok(serde_json::to_string_pretty(&json!({
            "ok": false,
            "kind": "clipboard",
            "error": t("read_clipboard currently only supports Linux", "read_clipboard 当前仅支持 Linux"),
        }))?);
    }

    let preferred = PreferredType::from_args(&args)?;
    if !has_required_backend(preferred) {
        return Ok(serde_json::to_string_pretty(&json!({
            "ok": false,
            "kind": "clipboard",
            "error": t(
                "No supported clipboard backend found. Install wl-clipboard, xclip, or xsel.",
                "未检测到可用的剪贴板读取工具，请安装 wl-clipboard、xclip 或 xsel。"
            ),
            "required_tools": required_tools(preferred),
        }))?);
    }

    match preferred {
        PreferredType::Text => detected_text_result(paths),
        PreferredType::Image => detected_text_result(paths),
        PreferredType::Auto => auto_result(paths),
    }
}

impl PreferredType {
    fn from_args(args: &Value) -> Result<Self> {
        match args
            .get("preferred_type")
            .and_then(Value::as_str)
            .unwrap_or("auto")
            .trim()
        {
            "" | "auto" => Ok(Self::Auto),
            "text" => Ok(Self::Text),
            "image" => Ok(Self::Image),
            other => bail!("unsupported preferred_type: {other}"),
        }
    }
}

fn auto_result(paths: MiyuPaths) -> Result<String> {
    detected_text_result(paths)
}

fn detected_text_result(paths: MiyuPaths) -> Result<String> {
    match crate::clipboard::read_clipboard()? {
        ClipboardContent::Image(img) => image_binary_result(img, &paths),
        ClipboardContent::ImagePath(path) => image_path_result(path),
        ClipboardContent::TextPath(path) => text_path_result(path),
        ClipboardContent::Text(text) => text_content_result(text),
        ClipboardContent::None => text_result(),
    }
}

fn text_content_result(text: String) -> Result<String> {
    Ok(serde_json::to_string_pretty(&json!({
        "ok": true,
        "kind": "clipboard",
        "content_type": "text",
        "text": text,
        "bytes": text.len(),
    }))?)
}

fn text_result() -> Result<String> {
    if let Some(text) = crate::clipboard::read_clipboard_text()? {
        return Ok(serde_json::to_string_pretty(&json!({
            "ok": true,
            "kind": "clipboard",
            "content_type": "text",
            "text": text,
            "bytes": text.len(),
        }))?);
    }
    empty_result()
}

fn image_binary_result(img: crate::clipboard::ClipboardImage, paths: &MiyuPaths) -> Result<String> {
    let mime = img.mime.clone();
    let bytes = img.data.len();
    let path = img.write_temp_file(&paths.cache_dir, 0)?;
    Ok(serde_json::to_string_pretty(&json!({
        "ok": true,
        "kind": "clipboard",
        "content_type": "image",
        "source": "clipboard_binary",
        "path": path.display().to_string(),
        "mime": mime,
        "bytes": bytes,
        "multimodal_available": true,
    }))?)
}

fn image_path_result(path: String) -> Result<String> {
    Ok(serde_json::to_string_pretty(&json!({
        "ok": true,
        "kind": "clipboard",
        "content_type": "image_path",
        "source": "clipboard_path",
        "path": path,
    }))?)
}

fn text_path_result(path: String) -> Result<String> {
    Ok(serde_json::to_string_pretty(&json!({
        "ok": true,
        "kind": "clipboard",
        "content_type": "text_path",
        "source": "clipboard_path",
        "path": path,
    }))?)
}

fn empty_result() -> Result<String> {
    Ok(serde_json::to_string_pretty(&json!({
        "ok": true,
        "kind": "clipboard",
        "content_type": "empty",
    }))?)
}

fn has_required_backend(preferred: PreferredType) -> bool {
    match preferred {
        PreferredType::Image => command_exists("wl-paste") || command_exists("xclip"),
        PreferredType::Auto | PreferredType::Text => {
            command_exists("wl-paste") || command_exists("xclip") || command_exists("xsel")
        }
    }
}

fn required_tools(preferred: PreferredType) -> Vec<&'static str> {
    match preferred {
        PreferredType::Image => vec!["wl-clipboard", "xclip"],
        PreferredType::Auto | PreferredType::Text => vec!["wl-clipboard", "xclip", "xsel"],
    }
}

fn command_exists(name: &str) -> bool {
    let Some(path) = env::var_os("PATH") else {
        return false;
    };
    env::split_paths(&path).any(|dir| is_executable(&dir.join(name)))
}

fn is_executable(path: &Path) -> bool {
    let Ok(metadata) = std::fs::metadata(path) else {
        return false;
    };
    metadata.is_file()
}
