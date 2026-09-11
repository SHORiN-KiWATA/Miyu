//! 把图片打到终端上。
//!
//! 尺寸按终端宽高的百分比算（`configured_print_size`），模型可以覆盖但要限幅
//! （`requested_print_size`）——一张占满整屏的图会把上下文全推走。

use crate::tools::vision::*;

pub fn register_print(registry: &mut ToolRegistry, config: AppConfig) {
    if !config.plugins.print_image.enabled {
        return;
    }
    registry.register(ToolSpec::new_with_progress(
        "print_image",
        "Print/render a local image directly in the current terminal output. Use this when the user asks to show, print, render, or preview an image, or when you need to inspect an image visually in the terminal before answering.",
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
        move |args, progress| {
            let print_config = config.plugins.print_image.clone();
            async move { print_image(args, &print_config, progress).await }
        },
    ));
}

pub(crate) async fn print_image(
    args: Value,
    print_config: &PrintImagePluginConfig,
    progress: crate::tools::ToolProgress,
) -> Result<String> {
    let image = args
        .get("image")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim();
    if image.is_empty() {
        bail!("{}", "image is required")
    }
    let path = expand_path(image);
    crate::tools::sandbox::guard_read(&path)?;
    let metadata = std::fs::metadata(&path)
        .with_context(|| format!("{} {}", "failed to stat image", path.display()))?;
    if !metadata.is_file() {
        bail!("{}: {}", "image path is not a file", path.display())
    }
    // 模型显式要的尺寸要随事件带走:daemon 模式下真正画图的是终端那一侧,
    // 这里 print_image_file 的参数它看不见。
    progress.report_sized_image(
        path.clone(),
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("image"),
        requested_print_size(&args),
    );
    if progress.prepare_for_external_output().await {
        print_image_file(&path, print_size(&args, print_config)).await?;
        Ok(format!("printed image in terminal: {}", path.display()))
    } else {
        // 无终端会话(WebUI/桥/平台)里谎报 "printed in terminal" 会让模型
        // 以为用户看到了图(08-21 生图 bug 的帮凶);说真话:图已 emit,
        // 由宿主决定怎么展示。
        Ok(format!(
            "image emitted to the host for display: {}",
            path.display()
        ))
    }
}

pub async fn print_image_file(path: &Path, size: Option<String>) -> Result<()> {
    // 不再自带前导空行:所有调用方都紧跟 prepare_for_external_output,
    // 摘要冻结(write_activity_summary)已留了一个空行,这里再空一行就是
    // 图片上方空两行(用户 08-20 实测点名)。
    if crossterm::terminal::is_raw_mode_enabled().unwrap_or(false)
        && crate::terminal::kitty::is_native_kitty_terminal()
        && crate::terminal::kitty::supports_path(path)
    {
        crate::terminal::kitty::print(path, size.as_deref())?;
        println!();
        io::stdout().flush()?;
        return Ok(());
    }
    let mut command = Command::new("chafa");
    if crossterm::terminal::is_raw_mode_enabled().unwrap_or(false) {
        command.args(["--probe", "off", "--relative", "off"]);
    }
    if let Some(size) = size {
        command.arg("--size").arg(size);
    }
    command.kill_on_drop(true);
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
    println!();
    io::stdout().flush()?;
    Ok(())
}

pub fn configured_print_size(print_config: &PrintImagePluginConfig) -> Option<String> {
    let (cols, rows) = crossterm::terminal::size().ok()?;
    let width = ((cols as u32 * print_config.width_percent as u32) / 100).max(1);
    let height = ((rows as u32 * print_config.height_percent as u32) / 100).max(1);
    Some(format!("{}x{}", width.min(300), height.min(200)))
}

/// 模型显式要的尺寸，没要就是 None。
///
/// 和 `configured_print_size` 分开是因为两者只能在不同的地方解析：百分比
/// 依赖 `crossterm::terminal::size()`，daemon 量到的不是用户的终端，只能
/// 在 CLI 那侧算；而显式值只有 daemon 手里的工具参数才知道，必须随事件带
/// 过去，否则模型写了 width 也会被无声吃掉。
pub fn requested_print_size(args: &Value) -> Option<String> {
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
            .map(str::to_string),
        (width, 0) => Some(format!("{width}x")),
        (0, height) => Some(format!("x{height}")),
        (width, height) => Some(format!("{width}x{height}")),
    }
}

pub(crate) fn print_size(args: &Value, print_config: &PrintImagePluginConfig) -> Option<String> {
    requested_print_size(args).or_else(|| configured_print_size(print_config))
}
