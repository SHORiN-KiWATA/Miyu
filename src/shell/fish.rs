use crate::i18n::text as t;
use crate::paths::MiyuPaths;
use anyhow::Result;

pub fn hook() -> &'static str {
    r#"function __miyu_paste
    set -l output (miyu --clipboard-paste 2>/dev/null)
    if test $status -eq 0; and test -n "$output"
        if not set -q __miyu_image_counter
            set -g __miyu_image_counter 0
        end
        set __miyu_image_counter (math $__miyu_image_counter + 1)
        set output (string replace "Image 1" "Image $__miyu_image_counter" -- $output)
        commandline -i -- $output
        commandline -f repaint
    else
        fish_clipboard_paste
    end
end

bind \cv __miyu_paste

function __miyu_insert_newline
    commandline -f expand-abbr
    commandline -i \n
end

bind ctrl-j __miyu_insert_newline
bind \cj __miyu_insert_newline
bind -M insert ctrl-j __miyu_insert_newline
bind -M insert \cj __miyu_insert_newline

function __miyu_wrap_fish_prompt
    functions -q __miyu_original_fish_prompt; and return
    functions -q fish_prompt; or fish_prompt >/dev/null 2>/dev/null
    functions -q fish_prompt; or return

    functions -c fish_prompt __miyu_original_fish_prompt
    function fish_prompt
        if set -q __miyu_pending_buffer
            printf '\e[?25l'
        end
        __miyu_original_fish_prompt
    end
end

function __miyu_replay_buffer
    set -l buffer $argv[1]
    set -l lines (string split \n -- "$buffer")
    if test (count $lines) -gt 0
        set -l prompt (fish_prompt | string collect -N)
        set -l prompt_lines (string split \n -- "$prompt")
        set -l prompt_col (math (string length --visible -- "$prompt_lines[-1]") + 1)
        printf '\e[?25l'
        printf '\e[1A\e[%sG' $prompt_col
        if not set -q fish_color_error; or not set_color $fish_color_error 2>/dev/null
            set_color red
        end
        printf '%s\n' "$lines[1]"
        for line in $lines[2..-1]
            printf '  %s\n' "$line"
        end
        set_color normal
    end
end

function __miyu_restore_cursor
    printf '\e[?25h'
    set -e __miyu_cursor_hidden
end

function __miyu_on_prompt --on-event fish_prompt
    set -q __miyu_pending_buffer; or return

    set -l buffer $__miyu_pending_buffer
    set -e __miyu_pending_buffer
    set -e __miyu_image_counter

    trap __miyu_restore_cursor INT TERM EXIT
    __miyu_replay_buffer "$buffer"
    printf '%s' "$buffer" | miyu --shell-intercept --shell fish --stdin
    set -l miyu_status $status
    trap - INT TERM EXIT
    __miyu_restore_cursor
    return $miyu_status
end

function __miyu_execute_or_continue
    commandline --is-valid
    set -l valid_status $status
    if test $valid_status -eq 2
        commandline -i \n
        commandline -f repaint
    else
        set -e __miyu_image_counter
        commandline -f execute
    end
end

function __miyu_buffer_is_multiline
    test (string split \n -- "$argv[1]" | count) -gt 1
end

function __miyu_multiline_has_unknown_command
    set -l buffer $argv[1]
    for line in (string split \n -- "$buffer")
        set -l trimmed (string trim -- "$line")
        if test -z "$trimmed"; or string match -q '#*' -- "$trimmed"
            continue
        end

        set -l tokens (string split -n ' ' -- "$trimmed")
        while test (count $tokens) -gt 0
            set -l token $tokens[1]
            if string match -qr '^[A-Za-z_][A-Za-z0-9_]*=' -- "$token"
                set -e tokens[1]
                continue
            end
            break
        end
        set -l command $tokens[1]
        test -n "$command"; or continue
        type -q -- "$command"; or return 0
    end
    return 1
end

function __miyu_accept_line
    status is-interactive; or return

    commandline -f expand-abbr
    set -l buffer (commandline -b | string collect -N)
    set -l trimmed (string trim -- "$buffer")
    if test -z "$trimmed"
        __miyu_execute_or_continue
        return
    end

    if not __miyu_buffer_is_multiline "$buffer"
        __miyu_execute_or_continue
        return
    end

    if not __miyu_multiline_has_unknown_command "$buffer"
        __miyu_execute_or_continue
        return
    end

    set -e __miyu_image_counter
    __miyu_wrap_fish_prompt
    set -g __miyu_cursor_hidden 1
    history append -- "$buffer"
    set -g __miyu_pending_buffer "$buffer"
    commandline -b -- ""
    printf '\e[?25l'
    commandline -f execute
end

bind enter __miyu_accept_line
bind \r __miyu_accept_line
bind -M insert enter __miyu_accept_line
bind -M insert \r __miyu_accept_line

function fish_command_not_found
    status is-interactive; or return 127

    set -e __miyu_image_counter

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
    fn fish_hook_defines_paste_binding() {
        let hook = hook();
        assert!(hook.contains("__miyu_paste"));
        assert!(hook.contains("bind \\cv __miyu_paste"));
        assert!(hook.contains("miyu --clipboard-paste"));
    }

    #[test]
    fn fish_hook_defines_enter_binding() {
        let hook = hook();
        assert!(hook.contains("__miyu_accept_line"));
        assert!(hook.contains("__miyu_wrap_fish_prompt"));
        assert!(hook.contains("functions -c fish_prompt __miyu_original_fish_prompt"));
        assert!(hook.contains("if set -q __miyu_pending_buffer"));
        assert!(hook.contains("__miyu_replay_buffer"));
        assert!(hook.contains("__miyu_on_prompt --on-event fish_prompt"));
        assert!(!hook.contains("        fish_prompt\n"));
        assert!(hook.contains("string length --visible"));
        assert!(hook.contains("printf '\\e[?25l'"));
        assert!(hook.contains("printf '\\e[1A\\e[%sG' $prompt_col"));
        assert!(hook.contains("not set_color $fish_color_error 2>/dev/null"));
        assert!(hook.contains("set_color normal"));
        assert!(hook.contains("printf '\\e[?25h'"));
        assert!(hook.contains("set -g __miyu_cursor_hidden 1"));
        assert!(hook.contains("set -e __miyu_cursor_hidden"));
        assert!(hook.contains("return $miyu_status"));
        assert!(hook.contains("__miyu_execute_or_continue"));
        assert!(hook.contains("__miyu_buffer_is_multiline"));
        assert!(hook.contains("test (string split \\n -- \"$argv[1]\" | count) -gt 1"));
        assert!(hook.contains("__miyu_multiline_has_unknown_command"));
        assert!(hook.contains("type -q -- \"$command\"; or return 0"));
        assert!(hook.contains("set -g __miyu_pending_buffer \"$buffer\""));
        assert!(hook.contains("history append -- \"$buffer\""));
        assert!(hook.contains("commandline -b -- \"\""));
        assert!(hook.contains("commandline -f execute"));
        assert!(hook.contains("commandline -f expand-abbr"));
        assert!(hook.contains("string match -qr '^[A-Za-z_][A-Za-z0-9_]*='"));
        assert!(!hook.contains("cancel-commandline"));
        assert!(hook.contains("commandline -b | string collect -N"));
        assert!(hook.contains("--shell-intercept --shell fish --stdin"));
        assert!(hook.contains("bind enter __miyu_accept_line"));
        assert!(hook.contains("bind \\r __miyu_accept_line"));
        assert!(hook.contains("bind ctrl-j __miyu_insert_newline"));
        assert!(hook.contains("bind -M insert enter __miyu_accept_line"));
        assert!(hook.contains("bind -M insert ctrl-j __miyu_insert_newline"));
    }

    #[test]
    fn fish_hook_resets_image_counter_on_command_not_found() {
        let hook = hook();
        assert!(hook.contains("set -e __miyu_image_counter"));
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
