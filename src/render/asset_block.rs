use crate::render::style::{ASSET_ERROR_STYLE, RESET, SECONDARY_STYLE};
use crate::render::terminal_image;
use anyhow::{bail, Context, Result};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use tempfile::TempDir;

#[derive(Clone, Copy)]
enum AssetKind {
    Mermaid,
    Math,
}

impl AssetKind {
    /// 返回资产类型展示名称。
    ///
    /// 返回:
    /// - 展示名称
    fn label(self) -> &'static str {
        match self {
            Self::Mermaid => "mermaid",
            Self::Math => "math",
        }
    }
}

/// 判断代码块语言是否需要渲染为图片资产。
///
/// 参数:
/// - `lang`: Markdown 代码块语言
///
/// 返回:
/// - 是否需要走图片资产渲染
pub(crate) fn is_asset_language(lang: &str) -> bool {
    asset_kind_from_lang(lang).is_some()
}

/// 渲染 Markdown 资产代码块。
///
/// 参数:
/// - `lang`: Markdown 代码块语言
/// - `lines`: 代码块内容行
///
/// 返回:
/// - 终端图片协议文本或错误提示
pub(crate) fn render_asset_block(lang: &str, lines: &[String]) -> String {
    let Some(kind) = asset_kind_from_lang(lang) else {
        return render_error("asset", "unsupported asset language");
    };
    render_asset(kind, &lines.join("\n"))
}

/// 渲染 `$$` 数学块。
///
/// 参数:
/// - `lines`: 数学公式内容行
///
/// 返回:
/// - 终端图片协议文本或错误提示
pub(crate) fn render_math_block(lines: &[String]) -> String {
    render_asset(AssetKind::Math, &lines.join("\n"))
}

/// 解析资产代码块类型。
///
/// 参数:
/// - `lang`: Markdown 代码块语言
///
/// 返回:
/// - 资产类型
fn asset_kind_from_lang(lang: &str) -> Option<AssetKind> {
    match lang.trim().to_ascii_lowercase().as_str() {
        "mermaid" | "mmd" => Some(AssetKind::Mermaid),
        "math" | "latex" | "tex" => Some(AssetKind::Math),
        _ => None,
    }
}

/// 渲染单个图片资产。
///
/// 参数:
/// - `kind`: 资产类型
/// - `source`: 原始内容
///
/// 返回:
/// - 终端图片协议文本或错误提示
fn render_asset(kind: AssetKind, source: &str) -> String {
    if source.trim().is_empty() {
        return render_error(kind.label(), "content is empty");
    }
    if test_stub_enabled() {
        return render_success(kind.label(), "[asset rendering skipped]\n".to_string());
    }
    match render_asset_inner(kind, source) {
        Ok(rendered) => render_success(kind.label(), rendered),
        Err(error) => render_error(kind.label(), &error.to_string()),
    }
}

/// 生成资产临时图片并转换为终端图片文本。
///
/// 参数:
/// - `kind`: 资产类型
/// - `source`: 原始内容
///
/// 返回:
/// - 终端图片协议文本或 chafa 文本输出
fn render_asset_inner(kind: AssetKind, source: &str) -> Result<String> {
    let temp_dir = tempfile::tempdir().context("failed to create temporary render directory")?;
    let image = match kind {
        AssetKind::Mermaid => render_mermaid_image(source, &temp_dir)?,
        AssetKind::Math => render_math_image(source, &temp_dir)?,
    };
    terminal_image::render_terminal_image(&image)
}

/// 使用 Mermaid CLI 生成图表图片。
///
/// 参数:
/// - `source`: Mermaid 源码
/// - `temp_dir`: 临时目录
///
/// 返回:
/// - 生成的 PNG 路径
fn render_mermaid_image(source: &str, temp_dir: &TempDir) -> Result<PathBuf> {
    let input = temp_dir.path().join("diagram.mmd");
    let output = temp_dir.path().join("diagram.png");
    fs::write(&input, source).with_context(|| format!("failed to write {}", input.display()))?;
    let mut command = Command::new("mmdc");
    command
        .arg("-i")
        .arg(&input)
        .arg("-o")
        .arg(&output)
        .arg("--quiet")
        .arg("-b")
        .arg("transparent")
        .stdin(Stdio::null());
    run_command(command, "mmdc")?;
    ensure_file_exists(&output)?;
    Ok(output)
}

/// 生成数学公式图片。
///
/// 参数:
/// - `source`: 数学公式源码
/// - `temp_dir`: 临时目录
///
/// 返回:
/// - 生成的 PNG 路径
fn render_math_image(source: &str, temp_dir: &TempDir) -> Result<PathBuf> {
    if let Some(output) = try_render_typst_math(source, temp_dir)? {
        return Ok(output);
    }
    let svg = temp_dir.path().join("formula.svg");
    let output = temp_dir.path().join("formula.png");
    fs::write(&svg, build_math_svg(source))
        .with_context(|| format!("failed to write {}", svg.display()))?;
    convert_svg_to_png(&svg, &output)?;
    ensure_file_exists(&output)?;
    Ok(output)
}

/// 优先尝试用 Typst 渲染数学公式。
///
/// 参数:
/// - `source`: 数学公式源码
/// - `temp_dir`: 临时目录
///
/// 返回:
/// - 成功时返回图片路径，未启用时返回空
fn try_render_typst_math(source: &str, temp_dir: &TempDir) -> Result<Option<PathBuf>> {
    if !command_available("typst") {
        return Ok(None);
    }
    let input = temp_dir.path().join("formula.typ");
    let output = temp_dir.path().join("formula.png");
    let content = format!(
        "#set page(width: auto, height: auto, margin: 12pt, fill: rgb(\"111827\"))\n#set text(fill: rgb(\"d7e3ff\"), size: 18pt)\n$ {} $\n",
        source.trim()
    );
    fs::write(&input, content).with_context(|| format!("failed to write {}", input.display()))?;
    let mut command = Command::new("typst");
    command
        .arg("compile")
        .arg(&input)
        .arg(&output)
        .stdin(Stdio::null());
    if run_command(command, "typst").is_ok() && output.is_file() {
        return Ok(Some(output));
    }
    Ok(None)
}

/// 将 SVG 公式图片转换为 PNG。
///
/// 参数:
/// - `svg`: SVG 输入路径
/// - `output`: PNG 输出路径
///
/// 返回:
/// - 转换是否成功
fn convert_svg_to_png(svg: &Path, output: &Path) -> Result<()> {
    let mut rsvg = Command::new("rsvg-convert");
    rsvg.arg("-f")
        .arg("png")
        .arg("-o")
        .arg(output)
        .arg(svg)
        .stdin(Stdio::null());
    if run_command(rsvg, "rsvg-convert").is_ok() {
        return Ok(());
    }
    let mut magick = Command::new("magick");
    magick.arg(svg).arg(output).stdin(Stdio::null());
    run_command(magick, "magick")
}

/// 构建数学公式降级 SVG。
///
/// 参数:
/// - `source`: 数学公式源码
///
/// 返回:
/// - SVG 文本
fn build_math_svg(source: &str) -> String {
    let lines = wrap_math_lines(source.trim(), 72);
    let max_chars = lines
        .iter()
        .map(|line| line.chars().count())
        .max()
        .unwrap_or(1);
    let width = (max_chars * 11 + 48).clamp(240, 1200);
    let height = (lines.len() * 30 + 32).clamp(72, 1200);
    let mut text = String::new();
    for (index, line) in lines.iter().enumerate() {
        let y = 38 + index * 30;
        text.push_str(&format!(
            r##"<text x="24" y="{y}" font-family="DejaVu Sans Mono, monospace" font-size="22" fill="#d7e3ff">{}</text>"##,
            escape_xml(line)
        ));
    }
    format!(
        r##"<svg xmlns="http://www.w3.org/2000/svg" width="{width}" height="{height}" viewBox="0 0 {width} {height}"><rect width="100%" height="100%" rx="12" fill="#111827"/>{text}</svg>"##
    )
}

/// 按字符数量拆分数学公式行。
///
/// 参数:
/// - `source`: 原始公式
/// - `limit`: 每行最大字符数
///
/// 返回:
/// - 拆分后的公式行
fn wrap_math_lines(source: &str, limit: usize) -> Vec<String> {
    let mut lines = Vec::new();
    for raw_line in source.lines() {
        let mut current = String::new();
        for ch in raw_line.chars() {
            current.push(ch);
            if current.chars().count() >= limit {
                lines.push(std::mem::take(&mut current));
            }
        }
        if !current.is_empty() {
            lines.push(current);
        }
    }
    if lines.is_empty() {
        lines.push(source.to_string());
    }
    lines
}

/// 转义 XML 文本。
///
/// 参数:
/// - `text`: 原始文本
///
/// 返回:
/// - 可安全写入 XML 的文本
fn escape_xml(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

/// 执行外部渲染命令。
///
/// 参数:
/// - `command`: 待执行命令
/// - `name`: 命令名称
///
/// 返回:
/// - 命令是否执行成功
fn run_command(mut command: Command, name: &str) -> Result<()> {
    let output = command
        .output()
        .with_context(|| format!("failed to run {name}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        let message = if stderr.trim().is_empty() {
            stdout.trim()
        } else {
            stderr.trim()
        };
        bail!("{name} exited with status {}: {message}", output.status);
    }
    Ok(())
}

/// 判断命令是否存在。
///
/// 参数:
/// - `name`: 命令名称
///
/// 返回:
/// - 命令是否可执行
fn command_available(name: &str) -> bool {
    Command::new(name)
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok()
}

/// 确认输出文件已经生成。
///
/// 参数:
/// - `path`: 输出文件路径
///
/// 返回:
/// - 文件是否存在
fn ensure_file_exists(path: &Path) -> Result<()> {
    if path.is_file() {
        Ok(())
    } else {
        bail!("renderer did not create {}", path.display())
    }
}

/// 渲染成功标签和图片文本。
///
/// 参数:
/// - `label`: 资产类型标签
/// - `rendered`: 图片渲染文本
///
/// 返回:
/// - 带标签的图片文本
fn render_success(label: &str, rendered: String) -> String {
    format!("{SECONDARY_STYLE}[{label}]{RESET}\n{rendered}")
}

/// 渲染资产错误提示。
///
/// 参数:
/// - `label`: 资产类型标签
/// - `message`: 错误信息
///
/// 返回:
/// - 带样式的错误提示
fn render_error(label: &str, message: &str) -> String {
    format!("{ASSET_ERROR_STYLE}[{label} render failed: {message}]{RESET}\n")
}

/// 判断测试替身是否开启。
///
/// 返回:
/// - 是否开启测试替身
fn test_stub_enabled() -> bool {
    cfg!(test) && std::env::var_os("MIYU_RENDER_ASSET_TEST_STUB").is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_asset_languages() {
        assert!(is_asset_language("mermaid"));
        assert!(is_asset_language("math"));
        assert!(is_asset_language("latex"));
        assert!(!is_asset_language("rust"));
    }

    #[test]
    fn math_svg_escapes_formula_text() {
        let svg = build_math_svg(r#"a < b && c > "d""#);
        assert!(svg.contains("&lt;"));
        assert!(svg.contains("&gt;"));
        assert!(svg.contains("&quot;"));
    }
}
