use anyhow::{bail, Context, Result};
use base64::{engine::general_purpose, Engine as _};
use crossterm::terminal;
use std::fs;
use std::path::Path;
use std::process::{Command, Stdio};

const KITTY_CHUNK_SIZE: usize = 4096;

/// 将图片渲染为当前终端可显示的文本。
///
/// 参数:
/// - `path`: 图片文件路径
///
/// 返回:
/// - 终端图片协议文本或 chafa 文本输出
pub(crate) fn render_terminal_image(path: &Path) -> Result<String> {
    if supports_kitty_graphics() {
        return render_kitty_image(path);
    }
    if supports_iterm_inline_image() {
        return render_iterm_image(path);
    }
    render_chafa_image(path)
}

/// 判断当前终端是否支持 Kitty 图形协议。
///
/// 返回:
/// - 是否支持 Kitty 图形协议
fn supports_kitty_graphics() -> bool {
    std::env::var_os("KITTY_WINDOW_ID").is_some()
        || std::env::var("TERM")
            .map(|term| term.contains("xterm-kitty"))
            .unwrap_or(false)
        || std::env::var("TERM_PROGRAM")
            .map(|program| program == "WezTerm")
            .unwrap_or(false)
}

/// 判断当前终端是否支持 iTerm2 图片协议。
///
/// 返回:
/// - 是否支持 iTerm2 图片协议
fn supports_iterm_inline_image() -> bool {
    std::env::var("TERM_PROGRAM")
        .map(|program| matches!(program.as_str(), "iTerm.app" | "WezTerm"))
        .unwrap_or(false)
}

/// 使用 Kitty 图形协议渲染图片。
///
/// 参数:
/// - `path`: 图片文件路径
///
/// 返回:
/// - Kitty 图形协议转义序列
fn render_kitty_image(path: &Path) -> Result<String> {
    let bytes =
        fs::read(path).with_context(|| format!("failed to read image {}", path.display()))?;
    let encoded = general_purpose::STANDARD.encode(bytes);
    let mut output = String::new();
    let mut chunks = encoded.as_bytes().chunks(KITTY_CHUNK_SIZE).peekable();
    if let Some(first) = chunks.next() {
        let more = if chunks.peek().is_some() { 1 } else { 0 };
        output.push_str(&format!(
            "\x1b_Gf=100,a=T,m={more};{}\x1b\\",
            String::from_utf8_lossy(first)
        ));
    }
    while let Some(chunk) = chunks.next() {
        let more = if chunks.peek().is_some() { 1 } else { 0 };
        output.push_str(&format!(
            "\x1b_Gm={more};{}\x1b\\",
            String::from_utf8_lossy(chunk)
        ));
    }
    output.push('\n');
    Ok(output)
}

/// 使用 iTerm2 图片协议渲染图片。
///
/// 参数:
/// - `path`: 图片文件路径
///
/// 返回:
/// - iTerm2 图片协议转义序列
fn render_iterm_image(path: &Path) -> Result<String> {
    let bytes =
        fs::read(path).with_context(|| format!("failed to read image {}", path.display()))?;
    let encoded = general_purpose::STANDARD.encode(bytes);
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("image.png");
    let name = general_purpose::STANDARD.encode(name.as_bytes());
    Ok(format!(
        "\x1b]1337;File=inline=1;name={name}:{encoded}\x07\n"
    ))
}

/// 使用 chafa 降级渲染图片。
///
/// 参数:
/// - `path`: 图片文件路径
///
/// 返回:
/// - chafa 输出的终端文本
fn render_chafa_image(path: &Path) -> Result<String> {
    let mut command = Command::new("chafa");
    if let Some(size) = chafa_size() {
        command.arg("--size").arg(size);
    }
    command.arg("--fg-only").arg("--threshold").arg("0.75");
    let output = command
        .arg(path)
        .stdin(Stdio::null())
        .output()
        .with_context(|| "failed to run chafa; install chafa or use a terminal image protocol")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!(
            "chafa exited with status {}: {}",
            output.status,
            stderr.trim()
        );
    }
    let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
    if !text.ends_with('\n') {
        text.push('\n');
    }
    Ok(text)
}

/// 计算 chafa 图片显示尺寸。
///
/// 返回:
/// - chafa `--size` 参数
fn chafa_size() -> Option<String> {
    let (cols, rows) = terminal::size().ok()?;
    let width = cols.clamp(20, 120);
    let height = (rows / 2).clamp(8, 40);
    Some(format!("{width}x{height}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_iterm_terminal_program() {
        std::env::set_var("TERM_PROGRAM", "iTerm.app");
        assert!(supports_iterm_inline_image());
        std::env::remove_var("TERM_PROGRAM");
    }
}
