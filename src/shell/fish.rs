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
    test (string length -- $text) -le 120; or return 127
    string match -qr '[\n\r]' -- $text; and return 127
    string match -qr '^\s*([-#./~0-9]|[[:digit:]]+[.)])' -- $text; and return 127
    string match -qr '[/\\=|;&<>$`(){}\[\]*]' -- $text; and return 127

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
    println!("installed fish hook: {}", paths.fish_hook_file.display());
    Ok(())
}
