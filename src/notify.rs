//! Best-effort desktop notifications.
//!
//! Shells out to whatever the platform provides rather than pulling in a
//! notification crate — same trade the clipboard backends make. Every failure
//! is swallowed: a machine without a notification daemon, a headless session,
//! or a sandbox that blocks spawning must never turn a notification into a
//! user-visible error.

use std::process::{Command, Stdio};

/// Spawns the notification and returns immediately. The child is detached; we
/// never wait on it, so a hung helper cannot stall a turn.
pub fn notify(title: &str, body: &str) {
    if cfg!(target_os = "macos") {
        // `display notification` takes AppleScript string literals, so quotes
        // and backslashes in model-authored text have to be escaped.
        let script = format!(
            "display notification \"{}\" with title \"{}\"",
            applescript_escape(body),
            applescript_escape(title)
        );
        spawn("osascript", &["-e", &script]);
        return;
    }
    if cfg!(target_os = "windows") {
        let script = format!(
            "[System.Reflection.Assembly]::LoadWithPartialName('System.Windows.Forms') | Out-Null; \
             $n = New-Object System.Windows.Forms.NotifyIcon; \
             $n.Icon = [System.Drawing.SystemIcons]::Information; \
             $n.Visible = $true; $n.ShowBalloonTip(5000, '{}', '{}', 'Info')",
            powershell_escape(title),
            powershell_escape(body)
        );
        spawn("powershell", &["-NoProfile", "-Command", &script]);
        return;
    }
    // Linux/BSD: notify-send is the de-facto interface, and `--` keeps a body
    // starting with a dash from being read as a flag.
    spawn("notify-send", &["-a", "Miyu", "--", title, body]);
}

/// 同 [`notify`],但在 Linux 上走 `notify-send -p`(打印通知 id)并用 `-r`
/// 替换上一条,让一串相关通知(「在听」→「收到」→ 回复)只占一个气泡。
/// 返回本条的 id 供下次替换;拿不到 id(旧版 notify-send 不认 `-p`、非 Linux)
/// 时退回普通通知并返回 None。
///
/// 会等 notify-send 退出(它发完就退,几十毫秒),调用方应放在专用线程里
/// 保序,别堵在回合上。
pub fn notify_replacing(title: &str, body: &str, previous: Option<u32>) -> Option<u32> {
    if !cfg!(target_os = "linux") {
        notify(title, body);
        return None;
    }
    let mut args: Vec<String> = vec!["-a".into(), "Miyu".into(), "-p".into()];
    if let Some(id) = previous {
        args.push("-r".into());
        args.push(id.to_string());
    }
    args.extend(["--".to_string(), title.to_string(), body.to_string()]);
    let output = Command::new("notify-send")
        .args(&args)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output();
    match output {
        Ok(output) if output.status.success() => String::from_utf8_lossy(&output.stdout)
            .trim()
            .parse::<u32>()
            .ok(),
        Ok(_) => {
            // 老版本不认 -p/-r:退回普通发送,链条断了也别丢通知。
            spawn("notify-send", &["-a", "Miyu", "--", title, body]);
            None
        }
        Err(_) => None,
    }
}

fn spawn(program: &str, args: &[&str]) {
    let _ = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
}

fn applescript_escape(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', " ")
}

fn powershell_escape(value: &str) -> String {
    value.replace('\'', "''").replace('\n', " ")
}

/// Clips a notification body to something a popup can actually show.
pub fn clip_body(value: &str, max_chars: usize) -> String {
    let single_line = value.split_whitespace().collect::<Vec<_>>().join(" ");
    if single_line.chars().count() <= max_chars {
        return single_line;
    }
    let kept: String = single_line.chars().take(max_chars).collect();
    format!("{kept}…")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escaping_keeps_helper_arguments_intact() {
        assert_eq!(applescript_escape(r#"a"b\c"#), r#"a\"b\\c"#);
        assert_eq!(applescript_escape("line\nbreak"), "line break");
        assert_eq!(powershell_escape("it's"), "it''s");
    }

    #[test]
    fn body_is_clipped_to_one_line() {
        assert_eq!(clip_body("  a\n  b  ", 10), "a b");
        assert_eq!(clip_body("abcdefghij", 5), "abcde…");
    }

    #[test]
    fn a_missing_backend_is_silent() {
        // The point of the module: no panic, no error, no blocking, even when
        // nothing on the machine can show a notification.
        spawn("miyu-nonexistent-notification-backend", &["x"]);
    }
}
