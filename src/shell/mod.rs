pub mod fish;

pub fn looks_like_natural_language(input: &str) -> bool {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return false;
    }
    if trimmed.chars().count() > 120 || trimmed.contains('\n') || trimmed.contains('\r') {
        return false;
    }
    if starts_like_shell_fragment(trimmed) || contains_shell_syntax(trimmed) {
        return false;
    }
    trimmed.chars().any(|char| !char.is_ascii()) || trimmed.split_whitespace().count() > 1
}

fn starts_like_shell_fragment(input: &str) -> bool {
    let mut chars = input.chars();
    let Some(first) = chars.next() else {
        return true;
    };
    if matches!(first, '-' | '#' | '.' | '/' | '~') || first.is_ascii_digit() {
        return true;
    }
    false
}

fn contains_shell_syntax(input: &str) -> bool {
    input.chars().any(|ch| {
        matches!(
            ch,
            '/' | '\\'
                | '='
                | '|'
                | ';'
                | '&'
                | '<'
                | '>'
                | '$'
                | '`'
                | '('
                | ')'
                | '{'
                | '}'
                | '['
                | ']'
                | '*'
        )
    })
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
    fn rejects_pasted_list_and_shell_syntax() {
        assert!(!looks_like_natural_language(
            "- archlinux zen内核怎么安装驱动"
        ));
        assert!(!looks_like_natural_language("1. 我家离洗车店只有50M"));
        assert!(!looks_like_natural_language("GTK_IM_MODULE=fcitx"));
        assert!(!looks_like_natural_language("./target/release/miyu 查询"));
    }
}
