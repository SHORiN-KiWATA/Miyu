pub mod bash;
pub mod fish;
pub mod powershell;
pub mod zsh;

use crate::i18n::text as t;
use std::env;
use std::fs;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

pub fn print_reload_hint(shell: &str, hook_file: &Path) {
    let source = match shell {
        "fish" => format!("source {}", fish_quote(hook_file)),
        "bash" | "zsh" => format!("source {}", shell_quote(hook_file)),
        _ => return,
    };
    if current_parent_shell().as_deref() == Some(shell) {
        println!(
            "{}: {}",
            t(
                "run this in the current terminal to load it now",
                "在当前终端运行此命令可立即加载"
            ),
            source
        );
    } else {
        println!(
            "{}",
            t(
                "open a new matching shell session for the hook to take effect",
                "新开对应 shell 会话后 hook 将生效"
            )
        );
    }
}

pub fn current_parent_shell() -> Option<String> {
    let mut pid = std::process::id();
    for _ in 0..8 {
        let parent = parent_pid(pid)?;
        let name = process_name(parent)?;
        if matches!(name.as_str(), "fish" | "bash" | "zsh") {
            return Some(name);
        }
        pid = parent;
    }
    None
}

fn parent_pid(pid: u32) -> Option<u32> {
    #[cfg(not(unix))]
    {
        let _ = pid;
        return None;
    }
    #[cfg(unix)]
    {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        let after_name = stat.rsplit_once(") ")?.1;
        after_name.split_whitespace().nth(1)?.parse().ok()
    }
}

fn process_name(pid: u32) -> Option<String> {
    #[cfg(not(unix))]
    {
        let _ = pid;
        return None;
    }
    #[cfg(unix)]
    {
        std::fs::read_to_string(format!("/proc/{pid}/comm"))
            .ok()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
    }
}

fn shell_quote(path: &Path) -> String {
    format!("'{}'", path.display().to_string().replace('\'', "'\\''"))
}

fn fish_quote(path: &Path) -> String {
    format!(
        "'{}'",
        path.display()
            .to_string()
            .replace('\\', "\\\\")
            .replace('\'', "\\'")
    )
}

#[cfg(test)]
pub fn looks_like_natural_language(input: &str) -> bool {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return false;
    }
    !trimmed.contains('\n') && !trimmed.contains('\r')
}

pub fn is_shell_command(input: &str, shell_name: &str) -> bool {
    let parsed = if shell_name == "powershell" {
        first_powershell_command_token_with_rest(input)
    } else {
        first_command_token_with_rest(input)
    };
    let Some((command, rest)) = parsed else {
        return false;
    };
    if ambiguous_command_tail_looks_like_message(&command, rest) {
        return false;
    }
    is_shell_keyword_or_builtin(&command, shell_name)
        || is_explicit_command_path(&command)
        || command_exists_in_path(&command)
}

fn first_powershell_command_token_with_rest(input: &str) -> Option<(String, &str)> {
    let mut index = 0;
    loop {
        while let Some(ch) = input.get(index..)?.chars().next() {
            if !ch.is_whitespace() {
                break;
            }
            index += ch.len_utf8();
        }
        let rest = input.get(index..)?;
        if let Some(after_operator) = rest.strip_prefix('&') {
            index = input.len() - after_operator.len();
            continue;
        }
        if let Some(after_operator) = rest.strip_prefix('.') {
            if after_operator
                .chars()
                .next()
                .is_some_and(char::is_whitespace)
            {
                index = input.len() - after_operator.len();
                continue;
            }
        }
        break;
    }

    let mut token = String::new();
    let mut quote = None;
    let mut escaped = false;
    let mut consumed = input.len();

    for (relative, ch) in input[index..].char_indices() {
        let absolute = index + relative;
        if escaped {
            token.push(ch);
            escaped = false;
            continue;
        }
        if ch == '`' {
            escaped = true;
            continue;
        }
        if let Some(active_quote) = quote {
            if ch == active_quote {
                quote = None;
            } else {
                token.push(ch);
            }
            continue;
        }
        if matches!(ch, '\'' | '"') {
            quote = Some(ch);
            continue;
        }
        if ch.is_whitespace() || matches!(ch, ';' | '|' | '&' | '<' | '>') {
            consumed = absolute + ch.len_utf8();
            break;
        }
        token.push(ch);
    }

    if token.is_empty() {
        None
    } else {
        Some((token, input.get(consumed..).unwrap_or("")))
    }
}

fn first_command_token_with_rest(input: &str) -> Option<(String, &str)> {
    let mut offset = 0;
    while let Some(token) = next_fish_like_token(input, &mut offset) {
        if is_env_assignment(&token) {
            continue;
        }
        return Some((token, input.get(offset..).unwrap_or("")));
    }
    None
}

fn ambiguous_command_tail_looks_like_message(command: &str, rest: &str) -> bool {
    if !matches!(
        command,
        "time" | "test" | "date" | "which" | "type" | "command" | "history"
    ) {
        return false;
    }
    let rest = rest.trim();
    !rest.is_empty()
        && rest
            .chars()
            .any(|ch| ch == '?' || ch == '？' || is_cjk_char(ch))
}

fn is_cjk_char(ch: char) -> bool {
    matches!(
        ch,
        '\u{3400}'..='\u{4DBF}'
            | '\u{4E00}'..='\u{9FFF}'
            | '\u{F900}'..='\u{FAFF}'
            | '\u{20000}'..='\u{2A6DF}'
            | '\u{2A700}'..='\u{2B73F}'
            | '\u{2B740}'..='\u{2B81F}'
            | '\u{2B820}'..='\u{2CEAF}'
    )
}

fn next_fish_like_token(input: &str, offset: &mut usize) -> Option<String> {
    let mut index = *offset;
    loop {
        let rest = input.get(index..)?;
        let Some(ch) = rest.chars().next() else {
            *offset = input.len();
            return None;
        };
        if ch.is_whitespace() {
            index += ch.len_utf8();
            continue;
        }
        if ch == '#' {
            index += ch.len_utf8();
            while let Some(next) = input.get(index..).and_then(|rest| rest.chars().next()) {
                index += next.len_utf8();
                if next == '\n' || next == '\r' {
                    break;
                }
            }
            continue;
        }
        break;
    }

    let mut token = String::new();
    let mut in_single = false;
    let mut in_double = false;
    let mut escaped = false;
    let mut consumed = input.len();

    for (relative, ch) in input[index..].char_indices() {
        let absolute = index + relative;
        if escaped {
            token.push(ch);
            escaped = false;
            continue;
        }
        if ch == '\\' && !in_single {
            escaped = true;
            continue;
        }
        if ch == '\'' && !in_double {
            in_single = !in_single;
            continue;
        }
        if ch == '"' && !in_single {
            in_double = !in_double;
            continue;
        }
        if !in_single
            && !in_double
            && (ch.is_whitespace() || matches!(ch, ';' | '|' | '&' | '<' | '>'))
        {
            consumed = absolute + ch.len_utf8();
            if token.is_empty() {
                token.push(ch);
            }
            break;
        }
        token.push(ch);
    }

    *offset = consumed;
    if token.is_empty() {
        None
    } else {
        Some(token)
    }
}

fn is_env_assignment(token: &str) -> bool {
    let Some((name, _)) = token.split_once('=') else {
        return false;
    };
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    (first == '_' || first.is_ascii_alphabetic())
        && chars.all(|ch| ch == '_' || ch.is_ascii_alphanumeric())
}

fn is_shell_keyword_or_builtin(command: &str, shell_name: &str) -> bool {
    let common = matches!(
        command,
        "alias"
            | "bg"
            | "break"
            | "builtin"
            | "case"
            | "cd"
            | "command"
            | "continue"
            | "else"
            | "echo"
            | "end"
            | "exec"
            | "exit"
            | "false"
            | "fg"
            | "for"
            | "function"
            | "functions"
            | "history"
            | "if"
            | "jobs"
            | "ls"
            | "not"
            | "or"
            | "and"
            | "read"
            | "return"
            | "set"
            | "source"
            | "status"
            | "switch"
            | "test"
            | "time"
            | "true"
            | "while"
    );
    common
        || (shell_name == "fish"
            && matches!(
                command,
                "abbr"
                    | "argparse"
                    | "begin"
                    | "bind"
                    | "block"
                    | "contains"
                    | "count"
                    | "disown"
                    | "emit"
                    | "eval"
                    | "math"
                    | "random"
                    | "string"
                    | "type"
                    | "ulimit"
            ))
}

fn is_explicit_command_path(command: &str) -> bool {
    command.starts_with('/')
        || command.starts_with('\\')
        || command.starts_with("./")
        || command.starts_with(".\\")
        || command.starts_with("../")
        || command.starts_with("..\\")
        || command.starts_with("~/")
        || (command.len() >= 3
            && command.as_bytes()[1] == b':'
            && matches!(command.as_bytes()[2], b'\\' | b'/'))
}

fn command_exists_in_path(command: &str) -> bool {
    if command.is_empty() || command.contains('/') || command.contains('\\') {
        return false;
    }
    let Some(paths) = env::var_os("PATH") else {
        return false;
    };
    env::split_paths(&paths).any(|dir| {
        if is_executable_file(&dir.join(command)) {
            return true;
        }
        #[cfg(windows)]
        {
            let extensions =
                env::var("PATHEXT").unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD;.PS1".to_string());
            return extensions
                .split(';')
                .filter(|extension| !extension.is_empty())
                .any(|extension| is_executable_file(&dir.join(format!("{command}{extension}"))));
        }
        #[cfg(not(windows))]
        false
    })
}

fn is_executable_file(path: &Path) -> bool {
    let Ok(metadata) = fs::metadata(path) else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    return metadata.permissions().mode() & 0o111 != 0;
    #[cfg(windows)]
    return true;
    #[cfg(not(any(unix, windows)))]
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_safe_natural_language() {
        assert!(looks_like_natural_language("帮我查一下 niri 输入法"));
        assert!(looks_like_natural_language(
            "why is fcitx candidate window small"
        ));
    }

    #[test]
    fn accepts_command_not_found_text_without_syntax_filtering() {
        assert!(looks_like_natural_language(
            "这样写可以吗？假设我们输入一个字母`x`"
        ));
        assert!(looks_like_natural_language(
            "我好像在输入里加一个左斜杠就会导致输入不被传给miyu/对吗？"
        ));
        assert!(looks_like_natural_language(
            "软件需要适配 Wayland 的 `text-input` 协议，输入法要支持 $GTK_IM_MODULE 吗？"
        ));
        assert!(looks_like_natural_language(
            "GTK_IM_MODULE=fcitx 是什么意思？"
        ));
        assert!(looks_like_natural_language(
            "./target/release/miyu 查询为什么失败？"
        ));
    }

    #[test]
    fn rejects_empty_or_multiline_text() {
        assert!(!looks_like_natural_language(""));
        assert!(!looks_like_natural_language("   "));
        assert!(!looks_like_natural_language("第一行\n第二行"));
    }

    #[test]
    fn classifies_commands_as_shell() {
        assert!(is_shell_command("echo hi", "fish"));
        assert!(is_shell_command("cd /tmp", "fish"));
        assert!(is_shell_command("FOO=bar cargo check", "fish"));
        assert!(is_shell_command("# comment\nls", "fish"));
        assert!(is_shell_command("./target/release/miyu hi", "fish"));
        assert!(is_shell_command("for item in a b", "fish"));
        assert!(is_shell_command("time cargo check", "fish"));
        assert!(is_shell_command(
            r".\not-installed-yet.ps1 --help",
            "powershell"
        ));
        assert!(is_shell_command(
            r"C:\Tools\not-installed-yet.exe --help",
            "powershell"
        ));
        assert!(is_shell_command(
            r"& '.\not installed yet.ps1' --help",
            "powershell"
        ));
    }

    #[test]
    fn classifies_messages_as_miyu() {
        assert!(!is_shell_command("你觉得 a;b 是什么意思", "fish"));
        assert!(!is_shell_command("解释 <tag> 是什么", "fish"));
        assert!(!is_shell_command("第一行\n第二行", "fish"));
        assert!(!is_shell_command("# note\n解释一下这个问题", "fish"));
        assert!(!is_shell_command("time 是什么命令？", "fish"));
        assert!(!is_shell_command(
            "this-command-probably-does-not-exist",
            "fish"
        ));
        assert!(!is_shell_command(
            "GTK_IM_MODULE=fcitx 是什么意思？",
            "fish"
        ));
    }
}
