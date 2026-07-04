use crate::i18n::text as t;
use crate::paths::MiyuPaths;
use anyhow::Result;

pub fn hook() -> &'static str {
    r#"function fish_command_not_found
    status is-interactive; or return 127

    set -l command $argv
    if test (count $command) -eq 0
        return 127
    end

    set -l text (string join ' ' -- $command)
    string match -qr '[\n\r]' -- $text; and return 127

    miyu --shell-intercept --shell fish -- $command 2>/dev/null
    return 127
end
"#
}

pub fn install(paths: &MiyuPaths) -> Result<()> {
    if let Some(parent) = paths.fish_hook_file.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&paths.fish_hook_file, hook())?;
    println!(
        "{}: {}",
        t("installed fish hook", "已安装 fish hook"),
        paths.fish_hook_file.display()
    );
    super::print_reload_hint("fish", &paths.fish_hook_file);
    Ok(())
}

pub fn uninstall(paths: &MiyuPaths) -> Result<bool> {
    let removed = match std::fs::remove_file(&paths.fish_hook_file) {
        Ok(()) => true,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => false,
        Err(err) => return Err(err.into()),
    };
    if removed {
        println!(
            "{}: fish",
            t("removed Miyu shell hook", "已移除 Miyu shell hook")
        );
    }
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn fish_hook_defines_command_not_found_handler() {
        let hook = hook();
        assert!(hook.contains("fish_command_not_found"));
        assert!(hook.contains("--shell fish"));
        assert!(hook.contains("return 127"));
    }

    #[test]
    fn fish_hook_does_not_filter_natural_language_symbols() {
        let hook = hook();
        assert!(!hook.contains("length -- $text) -le 120"));
        assert!(!hook.contains("[/\\"));
        assert!(!hook.contains("=|;&<>"));
    }

    #[test]
    fn uninstall_reports_only_existing_hook() {
        let temp = tempfile::tempdir().unwrap();
        let paths = MiyuPaths {
            config_dir: temp.path().to_path_buf(),
            config_file: temp.path().join("config.json"),
            secrets_file: temp.path().join("secrets.json"),
            skills_dir: temp.path().join("skills"),
            data_dir: temp.path().join("data"),
            cache_dir: temp.path().join("cache"),
            state_dir: temp.path().join("state"),
            pictures_dir: temp.path().join("pictures"),
            fish_hook_file: temp.path().join("miyu.fish"),
            bash_hook_file: temp.path().join("bash-hook.sh"),
            zsh_hook_file: temp.path().join("zsh-hook.zsh"),
            scripts_dir: temp.path().join("scripts"),
            system_scripts_dir: PathBuf::new(),
        };

        assert!(!uninstall(&paths).unwrap());
        std::fs::write(&paths.fish_hook_file, hook()).unwrap();
        assert!(uninstall(&paths).unwrap());
        assert!(!uninstall(&paths).unwrap());
    }
}
