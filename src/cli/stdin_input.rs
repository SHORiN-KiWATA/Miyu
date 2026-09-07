//! 从管道读输入。
//!
//! `miyu < file` 或 `cmd | miyu` 时把 stdin 并进提示词。有字符上限与超时
//! （`STDIN_MAX_CHARS` / `STDIN_TIMEOUT_SECS`）——管道可能永远不关，也可能吐出
//! 几个 G。

use crate::cli::*;

pub(in crate::cli) fn drain_stdin() {
    use std::os::fd::AsRawFd;

    let stdin = io::stdin();
    if !stdin.is_terminal() {
        return;
    }
    let fd = stdin.as_raw_fd();
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 {
        return;
    }
    if unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return;
    }

    let mut handle = stdin.lock();
    let mut buffer = [0_u8; 4096];
    loop {
        match handle.read(&mut buffer) {
            Ok(0) => break,
            Ok(_) => continue,
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => break,
            Err(_) => break,
        }
    }

    let _ = unsafe { libc::fcntl(fd, libc::F_SETFL, flags) };
}

pub(in crate::cli) const STDIN_MAX_CHARS: usize = 50_000;

pub(in crate::cli) const STDIN_TIMEOUT_SECS: u64 = 5;

pub(in crate::cli) async fn append_stdin_if_piped(message: String) -> String {
    if io::stdin().is_terminal() {
        return message;
    }
    // The reader thread bounds itself with poll() deadlines instead of being
    // abandoned by an outer timeout: a thread stuck in a blocking read(0)
    // would make the tokio runtime hang forever on shutdown (the process
    // then never exits when stdin is a never-closing pipe).
    let read_result = tokio::task::spawn_blocking(|| -> std::io::Result<String> {
        use std::os::fd::AsRawFd;
        let stdin = std::io::stdin();
        let fd = stdin.as_raw_fd();
        let mut buf: Vec<u8> = Vec::new();
        let deadline =
            std::time::Instant::now() + std::time::Duration::from_secs(STDIN_TIMEOUT_SECS);
        loop {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() || buf.len() >= STDIN_MAX_CHARS {
                break;
            }
            let mut pollfd = libc::pollfd {
                fd,
                events: libc::POLLIN,
                revents: 0,
            };
            let timeout_ms = remaining.as_millis().min(i32::MAX as u128) as i32;
            let ready = unsafe { libc::poll(&mut pollfd, 1, timeout_ms) };
            if ready <= 0 {
                break;
            }
            let mut chunk = [0u8; 8192];
            let count = unsafe { libc::read(fd, chunk.as_mut_ptr().cast(), chunk.len()) };
            if count < 0 {
                let error = std::io::Error::last_os_error();
                if error.kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(error);
            }
            if count == 0 {
                break;
            }
            buf.extend_from_slice(&chunk[..count as usize]);
        }
        buf.truncate(STDIN_MAX_CHARS);
        Ok(String::from_utf8_lossy(&buf).into_owned())
    })
    .await;

    let stdin_content = match read_result {
        Ok(Ok(content)) if !content.trim().is_empty() => content.trim().to_string(),
        _ => return message,
    };

    if message.is_empty() {
        stdin_content
    } else {
        format!("{message}\n\n---\n(stdin)\n{stdin_content}")
    }
}

/// Expands a leading `~` or `~/…` to the user's home directory.
pub(in crate::cli) fn expand_tilde(path: &str) -> PathBuf {
    if let Some(home) = std::env::var_os("HOME") {
        if path == "~" {
            return PathBuf::from(home);
        }
        if let Some(rest) = path.strip_prefix("~/") {
            return PathBuf::from(home).join(rest);
        }
    }
    PathBuf::from(path)
}

/// `--stdin`:显式要求读标准输入,读到 EOF 为止(宿主喂长文档),超过上限
/// 报错而不是静默截断。与 `append_stdin_if_piped` 的 5 秒探测是两回事。
/// 与 daemon 的回合正文上限同源:超了在客户端就报用法错误,别送过去吃一个
/// 回合失败。
pub(in crate::cli) const STDIN_EXPLICIT_MAX_CHARS: usize = crate::runtime::MAX_CONTENT_CHARS;

pub(in crate::cli) fn append_stdin_to_eof(message: String) -> anyhow::Result<String> {
    let mut input = String::new();
    io::stdin().read_to_string(&mut input)?;
    if input.chars().count() + message.chars().count() > STDIN_EXPLICIT_MAX_CHARS {
        return Err(crate::cli::exit_code::usage_error(format!(
            "{} ({STDIN_EXPLICIT_MAX_CHARS})",
            crate::i18n::text("stdin exceeds the character limit", "标准输入超过字符上限")
        )));
    }
    let input = input.trim();
    if input.is_empty() {
        return Ok(message);
    }
    if message.is_empty() {
        return Ok(input.to_string());
    }
    Ok(format!("{message}\n\n---\n(stdin)\n{input}"))
}
