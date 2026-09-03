//! shell 集成。
//!
//! 装在 shell 里的钩子会把命令行内容交给 Miyu 判断：这是一条要执行的命令，
//! 还是一句想问 Miyu 的话？判断在毫秒级发生（人还按着回车），所以这条路要
//! 尽量短。剪贴板粘贴与占位符展开也在这里。

use crate::cli::*;

pub(in crate::cli) fn remove_shell_hooks(paths: &MiyuPaths) -> Result<()> {
    let removed = shell::fish::uninstall(paths)?;
    let removed = shell::bash::uninstall(paths)? || removed;
    let removed = shell::zsh::uninstall(paths)? || removed;
    if !removed {
        println!(
            "{}",
            t(
                "no installed Miyu shell hooks found",
                "未找到已安装的 Miyu shell hook"
            )
        );
    }
    Ok(())
}

/// 机器生成的长哈希名截成 8 位;其余原样返回。
///
/// 只截 stem,扩展名留着——解析端要靠它判断这是图片还是视频。
///
/// **只截"看起来是哈希"的名字**(≥16 位且全为 ASCII 字母数字),两个理由:
/// 一是 `&stem[..8]` 按**字节**切,遇到中文文件名会切在字符中间直接 panic
/// ——这个函数原先只见得到 `write_temp_file` 产出的十六进制名,08-27 接上
/// 剪贴板里的**任意用户文件名**之后就踩得到了;二是用户自己起的名字本来
/// 就有信息量,`我的录屏.mp4` 截成 `我的录屏` 只会更难认。
fn shorten_image_name(filename: &str) -> String {
    let Some((stem, ext)) = filename.rsplit_once('.') else {
        return filename.to_string();
    };
    let machine_generated = stem.len() >= 16 && stem.chars().all(|ch| ch.is_ascii_alphanumeric());
    if !machine_generated {
        return filename.to_string();
    }
    format!("{}.{}", &stem[..8], ext)
}

/// 在 `dir` 下建一条名为 `name` 的软链指向 `target`;已经指对了就不动。
fn refresh_image_link(dir: &std::path::Path, name: &str, target: &std::path::Path) -> Result<()> {
    let link = dir.join(name);
    let up_to_date = std::fs::read_link(&link)
        .map(|existing| existing == target)
        .unwrap_or(false);
    if up_to_date {
        return Ok(());
    }
    if link.exists() || link.is_symlink() {
        std::fs::remove_file(&link)?;
    }
    std::os::unix::fs::symlink(target, &link)?;
    Ok(())
}

fn short_image_link(path: &std::path::Path, filename: &str) -> Result<String> {
    let short_name = shorten_image_name(filename);
    if short_name == filename {
        return Ok(short_name);
    }
    let dir = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("no parent dir"))?;
    // 同目录内的相对目标:图片就在旁边,软链跟着目录一起被清理也不会悬空。
    refresh_image_link(dir, &short_name, std::path::Path::new(filename))?;
    Ok(short_name)
}

pub(in crate::cli) fn run_clipboard_paste(paths: &MiyuPaths) -> Result<()> {
    match crate::clipboard::read_clipboard() {
        Ok(crate::clipboard::ClipboardContent::Image(img)) => {
            let path = img.write_temp_file(&paths.cache_dir, 0)?;
            let filename = path.file_name().and_then(|n| n.to_str()).unwrap_or("image");
            // 占位符里的文件名是跨进程找回图片的唯一线索,删不得;但 32 位
            // 哈希名太吵,给它建个 8 位短名软链,占位符打短名,解析端按名
            // 拼路径照样能打开。同前缀撞名就覆盖软链——同目录会定期清理,
            // 真撞上也只是指向最新一张。
            let display_name = match short_image_link(&path, filename) {
                Ok(name) => name,
                Err(_) => filename.to_string(),
            };
            print!("[Image 1: {}]", display_name);
            io::stdout().flush()?;
            Ok(())
        }
        Ok(crate::clipboard::ClipboardContent::MediaPath(path)) => {
            let source = std::path::Path::new(&path);
            let filename = source
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("image");
            let dir = paths.cache_dir.join("clipboard_images");
            std::fs::create_dir_all(&dir)?;
            crate::clipboard::cleanup_clipboard_images(&dir);
            // 这条路(剪贴板里是图片**文件**而不是图片数据,例如从 QQ 或文件
            // 管理器复制)原先原样打出源文件名,而 QQ 的文件名正好是 32 位
            // 哈希,占位符就又长又吵——`Image` 分支早就截短了,这里漏了
            // (08-27 用户点名)。软链名与打印名必须一致:解析端就是拿占位符
            // 里的名字去 clipboard_images 下找文件的。
            let display_name = shorten_image_name(filename);
            // 目标是外部绝对路径,不能用相对目标。
            refresh_image_link(&dir, &display_name, source)?;
            // 视频用 Video 标签:图片视频共用同一条通路,但显示成 `[Image]` 会
            // 让人以为粘错了(08-28 用户点名)。
            let label = crate::cli::repl::placeholder::media_placeholder_label(&path);
            print!("[{label} 1: {display_name}]");
            io::stdout().flush()?;
            Ok(())
        }
        Ok(crate::clipboard::ClipboardContent::TextPath(path)) => {
            print!("{}", path);
            io::stdout().flush()?;
            Ok(())
        }
        Ok(crate::clipboard::ClipboardContent::Text(text)) => {
            if should_summarize_pasted_text(&text) {
                let index = shell_pasted_text_index(&paths.cache_dir, &text)?;
                let placeholder = pasted_text_placeholder(index, pasted_text_line_count(&text));
                print!("{}", placeholder);
            } else {
                print!("{}", text);
            }
            io::stdout().flush()?;
            Ok(())
        }
        _ => {
            std::process::exit(1);
        }
    }
}

pub(in crate::cli) fn shell_pasted_text_index(
    cache_dir: &std::path::Path,
    text: &str,
) -> Result<usize> {
    let dir = cache_dir.join("clipboard_texts");
    std::fs::create_dir_all(&dir)?;
    let mut index = 1;
    loop {
        let path = dir.join(format!("{index}.txt"));
        if !path.exists() {
            std::fs::write(path, text)?;
            return Ok(index);
        }
        index += 1;
    }
}

pub(in crate::cli) fn shell_message_from_input(
    use_stdin: bool,
    message: Vec<String>,
) -> Result<String> {
    if use_stdin {
        let mut input = String::new();
        io::stdin().read_to_string(&mut input)?;
        Ok(input)
    } else {
        Ok(join_message(message))
    }
}

pub(in crate::cli) fn run_shell_classify(shell_name: &str, message: &str) -> Result<()> {
    if !matches!(shell_name, "fish" | "bash" | "zsh") {
        std::process::exit(2);
    }
    if shell::is_shell_command(message, shell_name) {
        std::process::exit(0);
    }
    std::process::exit(1);
}

pub(in crate::cli) async fn run_shell_intercept(
    paths: &MiyuPaths,
    shell_name: &str,
    message: String,
) -> Result<()> {
    if !matches!(shell_name, "fish" | "bash" | "zsh") {
        bail!("{}: {shell_name}", t("unsupported shell", "不支持的 shell"));
    }
    if message.trim().is_empty() {
        bail!(
            "{}",
            t("not a natural language command", "不是自然语言命令")
        );
    }

    let message = expand_shell_pasted_text_placeholders(paths, &message)?;
    let (clean_message, pasted_images) = extract_image_placeholders(&message);

    let result = if pasted_images.is_empty() {
        // shell-hook keeps landing in the terminal session: that lane is the
        // whole point of typing natural language at the prompt.
        run_chat_with_options(
            paths,
            clean_message,
            None,
            false,
            AgentMode::Normal,
            TurnSession::Current,
        )
        .await
    } else {
        run_chat_with_images(paths, clean_message, pasted_images).await
    };
    drain_stdin();
    match result {
        // Ctrl+C 不是故障：暗一行「已取消」就够了，别顶着红色的「错误」。
        Err(err)
            if err
                .downcast_ref::<crate::cli::repl::session::RemoteTurnCancelled>()
                .is_some() =>
        {
            println!("\x1b[2m{}\x1b[0m", t("cancelled", "已取消"));
            Ok(())
        }
        // 其余错误这里不打印：往上返回后 `main.rs` 会打一次。以前这里先打
        // 一遍再返回 Err，同一句「错误: …」就会出现两次。
        other => other,
    }
}

pub(in crate::cli) fn expand_shell_pasted_text_placeholders(
    paths: &MiyuPaths,
    message: &str,
) -> Result<String> {
    let placeholders = find_pasted_text_placeholders(message);
    if placeholders.is_empty() {
        return Ok(message.to_string());
    }

    let chars: Vec<char> = message.chars().collect();
    let mut expanded = String::new();
    let mut last_end = 0;
    let dir = paths.cache_dir.join("clipboard_texts");
    for (start, end, index) in placeholders {
        expanded.extend(&chars[last_end..start]);
        let path = dir.join(format!("{index}.txt"));
        match std::fs::read_to_string(&path) {
            Ok(text) => expanded.push_str(&text),
            Err(_) => expanded.extend(&chars[start..end]),
        }
        last_end = end;
    }
    expanded.extend(&chars[last_end..]);
    Ok(expanded)
}

#[cfg(test)]
mod short_link_tests {
    use super::*;

    /// 截短只针对机器生成的哈希名。
    ///
    /// 中文等多字节文件名按字节切会落在字符中间,`&stem[..8]` 直接 panic
    /// ——这个函数原先只见得到十六进制名,接上剪贴板里的任意用户文件名之后
    /// 就踩得到了(08-27 自审)。
    #[test]
    fn shortening_leaves_human_filenames_alone() {
        // 多字节名放最前:按字节切会落在字符中间,退回旧写法这一行直接 panic
        // ("byte index 8 is not a char boundary"),失败信息一眼指向真正的危险。
        assert_eq!(shorten_image_name("视频文件名.mp4"), "视频文件名.mp4");
        assert_eq!(
            shorten_image_name("我的录屏片段合集.mp4"),
            "我的录屏片段合集.mp4"
        );
        // 哈希名照截。
        assert_eq!(
            shorten_image_name("0f4636c78f65d3639ece5a064b5ae753.png"),
            "0f4636c7.png"
        );
        assert_eq!(
            shorten_image_name("a5b2ee8e91cd08030cc51a44929a3523_720.jpg"),
            "a5b2ee8e91cd08030cc51a44929a3523_720.jpg",
            "带下划线的不算纯哈希,保持原样"
        );
        // 用户自己起的英文名同样有信息量,不截。
        assert_eq!(
            shorten_image_name("Windows 11 Tracks Everything.mp4"),
            "Windows 11 Tracks Everything.mp4"
        );
        // 没有扩展名、以及本来就短的,原样返回。
        assert_eq!(shorten_image_name("noext"), "noext");
        assert_eq!(shorten_image_name("a.png"), "a.png");
    }

    fn short_image_link_creates_idempotent_symlink() {
        let temp = tempfile::tempdir().unwrap();
        let filename = "552921ce0e9994cb6d412899365c87a1.png";
        let path = temp.path().join(filename);
        std::fs::write(&path, b"png").unwrap();
        let short = short_image_link(&path, filename).unwrap();
        assert_eq!(short, "552921ce.png");
        let link = temp.path().join(&short);
        assert_eq!(std::fs::read(&link).unwrap(), b"png");
        // 幂等:再来一次不报错、还指向同处。
        assert_eq!(short_image_link(&path, filename).unwrap(), "552921ce.png");
        // 短名原样返回。
        let short_file = temp.path().join("cat.png");
        std::fs::write(&short_file, b"x").unwrap();
        assert_eq!(short_image_link(&short_file, "cat.png").unwrap(), "cat.png");
    }
}
