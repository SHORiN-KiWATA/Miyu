use super::PlatformTurnContext;
use crate::platform_types::PlatformContextFileRef;
use crate::tools::{ToolRegistry, ToolSpec};
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Upper bound on how much of one platform file is read into model context.
const MAX_PLATFORM_FILE_READ_BYTES: usize = 128 * 1024;
/// Files with these extensions are considered readable text without a magic
/// probe. The content still has to decode as UTF-8 and contain no NUL bytes.
const TEXT_EXTENSIONS: &[&str] = &[
    "txt", "md", "markdown", "html", "htm", "json", "jsonc", "log", "ini", "conf", "config", "csv",
    "xml", "yaml", "yml", "toml", "c", "h", "cpp", "cc", "hpp", "rs", "py", "js", "mjs", "ts",
    "tsx", "jsx", "sh", "fish", "bash", "zsh", "css", "java", "go", "rb", "lua", "sql",
];
/// Extensions that must never be decoded into prompt text, even when the file
/// happens to contain a short UTF-8 run inside a container or binary payload.
const BINARY_EXTENSIONS: &[&str] = &[
    "zip", "7z", "tar", "gz", "xz", "bz2", "zst", "rar", "exe", "dll", "so", "dylib", "bin", "png",
    "jpg", "jpeg", "gif", "webp", "mp3", "mp4", "webm", "mkv", "flac", "ogg", "wav", "pdf", "doc",
    "docx", "xls", "xlsx", "ppt", "pptx", "apk", "deb", "rpm", "pkg",
];

struct FileReaderState {
    context: Arc<PlatformTurnContext>,
    files: Vec<PlatformContextFileRef>,
}

pub(crate) fn register(
    registry: &mut ToolRegistry,
    context: Arc<PlatformTurnContext>,
    files: Vec<PlatformContextFileRef>,
) {
    let state = Arc::new(FileReaderState { context, files });
    let description = if state.context.conversation.kind
        == crate::platform_types::ConversationKind::Group
    {
        "Read a text file uploaded to the current QQ group. `file` must be a file id from the visible chat history (e.g. file_<message_id>_1). Compressed archives, executables, and other binary formats are rejected; videos and image files must go through vision_analyze with the same file id instead; text is capped at 128 KiB per call."
    } else {
        "Read a text file uploaded through the current QQ/platform conversation. `file` is either a file id from the visible chat history (e.g. file_<message_id>_1) or an absolute path Miyu already downloaded under its platform_files cache. Compressed archives, executables, and other binary formats are rejected; videos and image files must go through vision_analyze with the same file id instead; text is capped at 128 KiB per call."
    };
    registry.register(
        ToolSpec::new(
            "read_platform_file",
            description,
            json!({
                "type": "object",
                "properties": {
                    "file": {
                        "type": "string",
                        "description": "File id from chat history, or a local path already under platform_files."
                    }
                },
                "required": ["file"],
                "additionalProperties": false
            }),
            move |arguments| {
                let state = state.clone();
                async move { read(arguments, state).await }
            },
        )
        .with_display_name("Read uploaded file"),
    );
}

async fn read(arguments: Value, state: Arc<FileReaderState>) -> Result<String> {
    let object = arguments
        .as_object()
        .context("arguments must be an object containing only file")?;
    if object.len() != 1 || !object.contains_key("file") {
        bail!("only file may be specified");
    }
    let raw = arguments
        .get("file")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .context("file is required")?;

    let downloaded = if let Some(file_ref) = state.files.iter().find(|file| file.id == raw) {
        // 视频/图片不下载:这条工具只出文本,下了也读不了。直接指到看图工具,
        // 省一次最长 200MB 的下载。
        if let Some(hint) = visual_file_hint(&file_ref.file_name, raw) {
            bail!(hint)
        }
        Some(state.context.fetch_platform_file(file_ref).await?)
    } else if raw.starts_with("file_") {
        let available = state
            .files
            .iter()
            .map(|file| file.id.clone())
            .collect::<Vec<_>>();
        if available.is_empty() {
            bail!("this turn has no platform file ids; use an absolute path under platform_files")
        }
        bail!(
            "file id `{raw}` is not attached to the current platform turn; available: {}",
            available.join(", ")
        )
    } else if state.context.conversation.kind == crate::platform_types::ConversationKind::Group {
        bail!("group files must be referenced by their file_... id from chat history")
    } else {
        None
    };

    let path = match downloaded {
        Some(file) => {
            let path = validate_cached_path(&file.path, &state.context.paths.cache_dir)?;
            read_platform_text(&path, &file.name, file.size)?
        }
        None => {
            let path = expand_home(Path::new(raw));
            let path = validate_cached_path(&path, &state.context.paths.cache_dir)?;
            let name = path
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("platform file")
                .to_string();
            let size = std::fs::metadata(&path)
                .map(|metadata| metadata.len())
                .unwrap_or(0);
            read_platform_text(&path, &name, size)?
        }
    };
    Ok(path)
}

fn validate_cached_path(path: &Path, cache_dir: &Path) -> Result<PathBuf> {
    let root = cache_dir.join("platform_files").join("qq");
    let root = root
        .canonicalize()
        .with_context(|| format!("platform file cache is unavailable: {}", root.display()))?;
    let path = path
        .canonicalize()
        .with_context(|| format!("platform file does not exist: {}", path.display()))?;
    if !path.starts_with(&root) {
        bail!("only files under {} may be read", root.display());
    }
    Ok(path)
}

fn expand_home(path: &Path) -> PathBuf {
    let Some(raw) = path.to_str() else {
        return path.to_path_buf();
    };
    if let Some(rest) = raw.strip_prefix("~/") {
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home).join(rest);
        }
    }
    path.to_path_buf()
}

/// 视频与图片文件不是文本:返回一句指向 `vision_analyze` 的提示,`reference`
/// 是模型该原样传过去的引用(文件 id 或本地路径)。
fn visual_file_hint(name: &str, reference: &str) -> Option<String> {
    let extension = Path::new(name)
        .extension()
        .and_then(|extension| extension.to_str())
        .map(str::to_ascii_lowercase)?;
    let kind = if crate::tools::vision::video_mime(name).is_some() {
        "a video"
    } else if matches!(extension.as_str(), "png" | "jpg" | "jpeg" | "gif" | "webp") {
        "an image"
    } else {
        return None;
    };
    Some(format!(
        "`{name}` is {kind}, not text; watch it with vision_analyze (image=\"{reference}\") instead"
    ))
}

fn read_platform_text(path: &Path, name: &str, size: u64) -> Result<String> {
    if let Some(hint) = visual_file_hint(name, &path.display().to_string()) {
        bail!(hint)
    }
    let extension = path
        .extension()
        .and_then(|extension| extension.to_str())
        .map(str::to_ascii_lowercase);
    let binary_extension = extension
        .as_deref()
        .is_some_and(|extension| BINARY_EXTENSIONS.contains(&extension));
    if binary_extension {
        bail!("`{name}` is a binary or compressed format; Miyu cannot read it as text");
    }

    let file = std::fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mut bytes = Vec::new();
    file.take((MAX_PLATFORM_FILE_READ_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .with_context(|| format!("reading {}", path.display()))?;
    let truncated = bytes.len() > MAX_PLATFORM_FILE_READ_BYTES;
    if truncated {
        bytes.truncate(MAX_PLATFORM_FILE_READ_BYTES);
        if let Err(error) = std::str::from_utf8(&bytes) {
            bytes.truncate(error.valid_up_to());
        }
    }
    if bytes.iter().any(|byte| *byte == 0) {
        bail!("`{name}` looks binary and cannot be read as text");
    }
    let text = std::str::from_utf8(&bytes)
        .with_context(|| format!("`{name}` is not valid UTF-8 text"))?
        .to_string();
    let explicit_text = extension
        .as_deref()
        .is_some_and(|extension| TEXT_EXTENSIONS.contains(&extension));
    if !explicit_text && !looks_like_text(&bytes) {
        bail!("`{name}` does not look like a text file; refusing to read it as text");
    }

    let mut output = format!("File: {name} ({} bytes)\n", size);
    if truncated {
        output.push_str(&format!("[showing the first {} bytes]\n", bytes.len()));
    }
    output.push_str("\n");
    output.push_str(text.trim_end());
    Ok(output)
}

fn looks_like_text(bytes: &[u8]) -> bool {
    let printable = bytes
        .iter()
        .filter(|byte| byte.is_ascii_graphic() || matches!(byte, b' ' | b'\n' | b'\r' | b'\t'))
        .count();
    bytes.is_empty() || printable * 100 / bytes.len() >= 95
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_file(name: &str, bytes: &[u8]) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(name);
        std::fs::write(&path, bytes).unwrap();
        (dir, path)
    }

    #[test]
    fn reads_utf8_text_and_reports_size() {
        let (_dir, path) = write_file("report.txt", "hello\nworld".as_bytes());
        let output = read_platform_text(&path, "report.txt", 11).unwrap();
        assert!(output.contains("File: report.txt (11 bytes)"));
        assert!(output.contains("hello\nworld"));
    }

    #[test]
    fn rejects_binary_extensions_before_decoding() {
        let (_dir, path) = write_file("evil.zip", b"PK\x03\x04 not really text");
        let error = read_platform_text(&path, "evil.zip", 22).unwrap_err();
        assert!(error.to_string().contains("binary or compressed"));
    }

    #[test]
    fn videos_and_images_point_at_the_vision_tool() {
        let (_dir, path) = write_file("clip.mp4", b"\x00\x00\x00\x18ftypmp42");
        let error = read_platform_text(&path, "clip.mp4", 12).unwrap_err();
        let text = error.to_string();
        assert!(text.contains("is a video"), "{text}");
        assert!(text.contains("vision_analyze"), "{text}");
        assert!(text.contains("clip.mp4"), "{text}");
        assert_eq!(
            visual_file_hint("photo.PNG", "file_1_1").unwrap(),
            "`photo.PNG` is an image, not text; watch it with vision_analyze (image=\"file_1_1\") instead"
        );
        assert!(visual_file_hint("notes.txt", "file_1_1").is_none());
        assert!(visual_file_hint("evil.zip", "file_1_1").is_none());
    }

    #[test]
    fn rejects_invalid_utf8_text() {
        let (_dir, path) = write_file("bad.txt", b"\xff\xfe\x00");
        let error = read_platform_text(&path, "bad.txt", 3).unwrap_err();
        assert!(error.to_string().contains("UTF-8") || error.to_string().contains("binary"));
    }

    #[test]
    fn truncates_large_files() {
        let text = "界".repeat(MAX_PLATFORM_FILE_READ_BYTES + 10);
        let (_dir, path) = write_file("large.txt", text.as_bytes());
        let output = read_platform_text(&path, "large.txt", text.len() as u64).unwrap();
        assert!(output.contains("first 131070 bytes") || output.contains("first 131071 bytes"));
    }

    #[test]
    fn unknown_extensions_require_text_sniffing() {
        let (_dir, path) = write_file("README", b"plain text");
        let output = read_platform_text(&path, "README", 10).unwrap();
        assert!(output.contains("plain text"));
    }
}
