use crate::i18n::text as t;
use crate::paths::MiyuPaths;
use anyhow::Result;
use directories::BaseDirs;
use std::path::{Path, PathBuf};

const BEGIN_MARKER: &str = "# >>> miyu nushell hook >>>";
const END_MARKER: &str = "# <<< miyu nushell hook <<<";

pub fn hook() -> &'static str {
    r#"# Send the current Nushell command line to Miyu with Alt+M.
# Enter remains untouched so native commands, history, and input editing keep
# using Nushell's normal Reedline flow.
def __miyu_send_buffer [] {
    let message = (commandline)
    if ($message | str trim | is-empty) {
        return
    }

    commandline edit --replace ""
    $message | ^miyu --shell-intercept --shell nu --stdin
}

$env.config.keybindings = (
    $env.config.keybindings
    | where name != "miyu_send_buffer"
    | append {
        name: miyu_send_buffer
        modifier: alt
        keycode: char_m
        mode: [emacs vi_insert vi_normal]
        event: {
            send: executehostcommand
            cmd: "__miyu_send_buffer"
        }
    }
)
"#
}

pub fn install(paths: &MiyuPaths) -> Result<()> {
    let hook_file = paths.nu_hook_file();
    let config_file = nushell_config_file();
    install_to(&hook_file, &config_file)?;

    println!(
        "{}: {}",
        t("installed nushell hook", "已安装 nushell hook"),
        hook_file.display()
    );
    println!("{}: {}", t("updated", "已更新"), config_file.display());
    super::print_reload_hint("nu", &hook_file);
    Ok(())
}

pub fn uninstall(paths: &MiyuPaths) -> Result<bool> {
    let hook_file = paths.nu_hook_file();
    let config_file = nushell_config_file();
    let removed_block = remove_source_block(&config_file)?;
    let removed_file = remove_file_if_exists(&hook_file)?;
    let removed = removed_block || removed_file;
    if removed {
        println!(
            "{}: nushell",
            t("removed Miyu shell hook", "已移除 Miyu shell hook")
        );
    }
    Ok(removed)
}

fn nushell_config_file() -> PathBuf {
    BaseDirs::new()
        .map(|base| base.config_dir().join("nushell/config.nu"))
        .unwrap_or_else(|| PathBuf::from(".config/nushell/config.nu"))
}

fn install_to(hook_file: &Path, config_file: &Path) -> Result<()> {
    if let Some(parent) = hook_file.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(hook_file, hook())?;
    append_source_block(config_file, hook_file)
}

fn append_source_block(config_file: &Path, hook_file: &Path) -> Result<()> {
    let existing = std::fs::read_to_string(config_file).unwrap_or_default();
    if existing.contains(BEGIN_MARKER) && existing.contains(END_MARKER) {
        return Ok(());
    }
    if let Some(parent) = config_file.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let mut updated = existing;
    if !updated.is_empty() && !updated.ends_with('\n') {
        updated.push('\n');
    }
    if !updated.is_empty() {
        updated.push('\n');
    }
    updated.push_str(BEGIN_MARKER);
    updated.push('\n');
    updated.push_str("source ");
    updated.push_str(&nu_quote(hook_file));
    updated.push('\n');
    updated.push_str(END_MARKER);
    updated.push('\n');
    std::fs::write(config_file, updated)?;
    Ok(())
}

fn remove_source_block(config_file: &Path) -> Result<bool> {
    let Ok(existing) = std::fs::read_to_string(config_file) else {
        return Ok(false);
    };
    let Some(begin_index) = existing.find(BEGIN_MARKER) else {
        return Ok(false);
    };
    let Some(end_relative) = existing[begin_index..].find(END_MARKER) else {
        return Ok(false);
    };

    let mut start_index = begin_index;
    if start_index > 0 && existing.as_bytes().get(start_index - 1) == Some(&b'\n') {
        start_index -= 1;
    }
    let mut end_index = begin_index + end_relative + END_MARKER.len();
    if existing.as_bytes().get(end_index) == Some(&b'\r') {
        end_index += 1;
    }
    if existing.as_bytes().get(end_index) == Some(&b'\n') {
        end_index += 1;
    }

    let mut updated = String::new();
    updated.push_str(&existing[..start_index]);
    updated.push_str(&existing[end_index..]);
    std::fs::write(config_file, updated)?;
    Ok(true)
}

fn remove_file_if_exists(path: &Path) -> Result<bool> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(true),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(err) => Err(err.into()),
    }
}

fn nu_quote(path: &Path) -> String {
    serde_json::to_string(&path.to_string_lossy()).expect("serializing a path string cannot fail")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hook_uses_alt_m_without_rebinding_enter() {
        let generated = hook();
        assert!(generated.contains("modifier: alt"));
        assert!(generated.contains("keycode: char_m"));
        assert!(generated.contains("commandline"));
        assert!(generated.contains("commandline edit --replace \"\""));
        assert!(generated.contains("--shell-intercept --shell nu --stdin"));
        assert!(generated.contains("where name != \"miyu_send_buffer\""));
        assert!(!generated.contains("keycode: enter"));
        assert!(!generated.contains("command_not_found"));
        assert!(!generated.contains("history import"));
    }

    #[test]
    fn install_is_idempotent_and_preserves_existing_config() {
        let temp = tempfile::tempdir().unwrap();
        let hook_file = temp.path().join("miyu/shell/nu-hook.nu");
        let config_file = temp.path().join("nushell/config.nu");
        std::fs::create_dir_all(config_file.parent().unwrap()).unwrap();
        std::fs::write(&config_file, "$env.TEST = 'kept'\n").unwrap();

        install_to(&hook_file, &config_file).unwrap();
        install_to(&hook_file, &config_file).unwrap();

        let config = std::fs::read_to_string(&config_file).unwrap();
        assert!(config.contains("$env.TEST = 'kept'"));
        assert_eq!(config.matches(BEGIN_MARKER).count(), 1);
        assert_eq!(config.matches(END_MARKER).count(), 1);
        assert!(config.contains(&nu_quote(&hook_file)));
        assert_eq!(std::fs::read_to_string(&hook_file).unwrap(), hook());
    }

    #[test]
    fn uninstall_removes_only_the_managed_block_and_hook() {
        let temp = tempfile::tempdir().unwrap();
        let hook_file = temp.path().join("miyu/shell/nu-hook.nu");
        let config_file = temp.path().join("nushell/config.nu");
        std::fs::create_dir_all(config_file.parent().unwrap()).unwrap();
        std::fs::write(&config_file, "before\nafter\n").unwrap();
        install_to(&hook_file, &config_file).unwrap();

        assert!(remove_source_block(&config_file).unwrap());
        assert!(remove_file_if_exists(&hook_file).unwrap());
        assert_eq!(
            std::fs::read_to_string(&config_file).unwrap(),
            "before\nafter\n"
        );
        assert!(!remove_source_block(&config_file).unwrap());
        assert!(!remove_file_if_exists(&hook_file).unwrap());
    }

    #[test]
    fn nu_quote_handles_spaces_quotes_and_non_ascii_paths() {
        let quoted = nu_quote(Path::new("/tmp/Miyu 配置/it's/hook.nu"));
        assert_eq!(quoted, "\"/tmp/Miyu 配置/it's/hook.nu\"");
    }
}
