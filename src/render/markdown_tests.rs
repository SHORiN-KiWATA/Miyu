use super::*;
use crate::render::style::{
    BOLD_STYLE, CODE_BLOCK_BG, CODE_BLOCK_FRAME_STYLE, CODE_FUNCTION_STYLE, CODE_KEYWORD_STYLE,
    CODE_TOKEN_RESET, HEADER_STYLE, IMAGE_STYLE, INLINE_CODE_STYLE, ITALIC_STYLE, PRIMARY_STYLE,
    RESET, STRIKE_STYLE, TERTIARY_STYLE, URL_STYLE,
};
use std::sync::Mutex;

static ASSET_STUB_LOCK: Mutex<()> = Mutex::new(());

#[test]
fn streams_only_complete_lines() {
    let mut renderer = MarkdownStreamRenderer::new();
    assert_eq!(renderer.push("**bo"), "");
    assert_eq!(
        renderer.push("ld**\n"),
        format!("{BOLD_STYLE}bold{RESET}\n")
    );
}

#[test]
fn flushes_partial_final_line() {
    let mut renderer = MarkdownStreamRenderer::new();
    assert_eq!(renderer.push("# Title"), "");
    assert_eq!(renderer.flush(), format!("{HEADER_STYLE}# Title{RESET}\n"));
}

#[test]
fn headings_use_one_color_and_distinct_prefix_lengths() {
    assert_eq!(
        render_markdown_line("# One"),
        format!("{HEADER_STYLE}# One{RESET}")
    );
    assert_eq!(
        render_markdown_line("## Two"),
        format!("{HEADER_STYLE}## Two{RESET}")
    );
    assert_eq!(
        render_markdown_line("### Three"),
        format!("{HEADER_STYLE}### Three{RESET}")
    );
    assert_eq!(
        render_markdown_line("###### Six"),
        format!("{HEADER_STYLE}###### Six{RESET}")
    );
}

#[test]
fn list_markers_use_tertiary_color() {
    assert!(render_markdown_line("- item").contains(&format!("{TERTIARY_STYLE}-{RESET}")));
    assert!(render_markdown_line("1. item").contains(&format!("{TERTIARY_STYLE}1.{RESET}")));
}

#[test]
fn buffers_tables_until_non_table_line() {
    let mut renderer = MarkdownStreamRenderer::new();
    assert_eq!(renderer.push("| a | b |\n"), "");
    let header = renderer.push("| - | - |\n");
    assert!(header.contains("\x1b[1ma\x1b[0m"));
    let row = renderer.push("| 1 | 2 |\n");
    assert!(row.contains("1"));
    let output = renderer.push("done\n");
    assert!(output.contains('└'));
    assert!(!output.contains('+'));
    assert!(output.ends_with("done\n"));
}

#[test]
fn blockquote_is_visually_distinct() {
    let mut renderer = MarkdownStreamRenderer::new();
    let output = renderer.push(">> quoted\n");
    assert!(output.contains("\x1b[32m| \x1b[0m\x1b[32m| \x1b[0m"));
    assert!(output.contains("\x1b[32mquoted\x1b[0m"));
    assert!(!output.contains("48;5;236"));
}

#[test]
fn code_block_has_label_and_readable_content() {
    let mut renderer = MarkdownStreamRenderer::new();
    let output = renderer.push("```rust\nfn main() {}\n```\n");
    assert!(output.contains("-- code rust"));
    assert!(!output.contains(",-- code rust"));
    assert!(!output.contains("\x1b[2m|\x1b[0m"));
    assert!(output.contains(&format!(
        "{CODE_BLOCK_BG}{CODE_KEYWORD_STYLE}fn{CODE_TOKEN_RESET}"
    )));
    assert!(output.contains(&format!("{CODE_FUNCTION_STYLE}main{CODE_TOKEN_RESET}")));
    assert!(output.contains(&format!("{CODE_BLOCK_FRAME_STYLE}-- code rust -")));
    assert!(output.contains(&format!(
        "{CODE_BLOCK_FRAME_STYLE}{}{RESET}",
        "-".repeat(24)
    )));
    assert!(!output.contains("`--"));
}

#[test]
fn code_block_content_has_default_color() {
    let mut renderer = MarkdownStreamRenderer::new();
    let output = renderer.push("```\nXMODIFIERS \"@im=fcitx\"\n```\n");
    assert!(output.contains(&format!(
        "{CODE_BLOCK_BG}XMODIFIERS \"@im=fcitx\"{}{RESET}",
        " ".repeat(2)
    )));
    assert!(!output.contains("\x1b[33mXMODIFIERS"));
}

#[test]
fn code_block_variables_use_primary_color() {
    let mut renderer = MarkdownStreamRenderer::new();
    let output = renderer.push("```rust\nlet msg = String::from(\"hi\");\n```\n");
    assert!(output.contains(&format!("{PRIMARY_STYLE}msg{CODE_TOKEN_RESET}")));
}

#[test]
fn code_block_background_uses_longest_line_width() {
    let mut renderer = MarkdownStreamRenderer::new();
    let output = renderer.push("```\nshort\nlonger line\n```\n");
    assert!(output.contains(&format!("{CODE_BLOCK_BG}short{}{RESET}", " ".repeat(19))));
    assert!(output.contains(&format!(
        "{CODE_BLOCK_BG}longer line{}{RESET}",
        " ".repeat(13)
    )));
    assert!(output.contains(&format!(
        "{CODE_BLOCK_FRAME_STYLE}{}{RESET}",
        "-".repeat(24)
    )));
    assert!(!output.contains("48;5;236"));
}

#[test]
fn renders_more_inline_markdown() {
    let output = render_inline(
        "*i* ~~gone~~ [site](https://example.com) <https://example.org> ![pic](https://img)",
    );
    assert!(output.contains(&format!("{ITALIC_STYLE}i{RESET}")));
    assert!(output.contains(&format!("{STRIKE_STYLE}gone{RESET}")));
    assert!(output.contains(&format!("<{URL_STYLE}https://example.com{RESET}>")));
    assert!(output.contains(&format!(
        "\x1b[4m<{URL_STYLE}https://example.org{RESET}>{RESET}"
    )));
    assert!(output.contains(&format!(
        "{IMAGE_STYLE}[image: pic]{RESET}({URL_STYLE}https://img{RESET})"
    )));
    assert!(!output.contains("\x1b[35mimage\x1b[0m"));
}

#[test]
fn renders_inline_code_at_start_of_bullet() {
    let output = render_markdown_line("- `read_file` — 读文件内容");
    assert!(output.contains(&format!("{INLINE_CODE_STYLE}read_file\x1b[0m")));
    assert!(output.contains("— 读文件内容"));
}

#[test]
fn renders_multiple_inline_code_spans_in_bullet_with_chinese_text() {
    let output = render_markdown_line(
        "- `~/.config/Thunar/` - 里面有 `accels.scm`（快捷键绑定）和 `uca.xml`（自定义右键菜单）",
    );
    assert!(output.contains(&format!("{INLINE_CODE_STYLE}~/.config/Thunar/\x1b[0m")));
    assert!(output.contains(&format!("{INLINE_CODE_STYLE}accels.scm\x1b[0m")));
    assert!(output.contains(&format!("{INLINE_CODE_STYLE}uca.xml\x1b[0m")));
    assert!(!output.contains('`'));
}

#[test]
fn renders_inline_code_when_stream_chunks_split_backticks() {
    let mut renderer = MarkdownStreamRenderer::new();
    assert_eq!(renderer.push("- `~/.config/Thu"), "");
    let output = renderer.push("nar/` - 里面有 `accels.scm`\n");
    assert!(output.contains(&format!("{INLINE_CODE_STYLE}~/.config/Thunar/\x1b[0m")));
    assert!(output.contains(&format!("{INLINE_CODE_STYLE}accels.scm\x1b[0m")));
    assert!(!output.contains('`'));
}

#[test]
fn keeps_identifier_underscores_literal() {
    let output = render_inline("GTK_IM_MODULE and _italic_");
    assert!(output.contains("GTK_IM_MODULE"));
    assert!(output.contains(&format!("{ITALIC_STYLE}italic{RESET}")));
    assert!(!output.contains("GTK\x1b[3mIM\x1b[0mMODULE"));
    assert_eq!(render_inline("abc_def_ghi"), "abc_def_ghi");
}

#[test]
fn renders_inline_math_formulas_visibly() {
    let _guard = ASSET_STUB_LOCK.lock().unwrap();
    std::env::set_var("MIYU_RENDER_ASSET_TEST_STUB", "1");
    let output = render_inline("inline $E=mc^2$ and display $$a^2+b^2=c^2$$");
    std::env::remove_var("MIYU_RENDER_ASSET_TEST_STUB");
    assert!(output.contains("inline "));
    assert!(output.contains(" and display "));
    assert!(output.contains("[inline math rendering skipped]"));
    assert!(!output.contains("$E=mc^2$"));
    assert!(!output.contains("$$a^2+b^2=c^2$$"));
}

#[test]
fn renders_multiline_math_blocks_as_assets() {
    let _guard = ASSET_STUB_LOCK.lock().unwrap();
    std::env::set_var("MIYU_RENDER_ASSET_TEST_STUB", "1");
    let mut renderer = MarkdownStreamRenderer::new();
    let output = renderer.push("$$\na^2 + b^2 = c^2\n$$\n");
    std::env::remove_var("MIYU_RENDER_ASSET_TEST_STUB");
    assert!(output.contains("[math]"));
    assert!(output.contains("[asset rendering skipped]"));
    assert!(!output.contains("\x1b[36m$$\x1b[0m"));
}

#[test]
fn renders_mermaid_blocks_as_assets() {
    let _guard = ASSET_STUB_LOCK.lock().unwrap();
    std::env::set_var("MIYU_RENDER_ASSET_TEST_STUB", "1");
    let mut renderer = MarkdownStreamRenderer::new();
    let output = renderer.push("```mermaid\ngraph TD\nA --> B\n```\n");
    std::env::remove_var("MIYU_RENDER_ASSET_TEST_STUB");
    assert!(output.contains("[mermaid]"));
    assert!(output.contains("[asset rendering skipped]"));
    assert!(!output.contains("-- code mermaid"));
}

#[test]
fn renders_selected_inline_html_tags() {
    let output = render_inline("<u>under</u> H<sub>2</sub> x<sup>2</sup><br>next");
    assert!(output.contains("\x1b[4munder\x1b[0m"));
    assert!(output.contains("H\x1b[2m2\x1b[0m"));
    assert!(output.contains("x\x1b[1m2\x1b[0m"));
    assert!(output.contains("\nnext"));
}

#[test]
fn horizontal_rule_uses_terminal_width_fallback() {
    let output = render_markdown_line("---");
    assert!(output.starts_with("\x1b[2m"));
    assert!(output.ends_with("\x1b[0m"));
    assert!(table::visible_width(&output) >= 16);
}

#[test]
fn supports_table_alignment_markers() {
    let mut renderer = MarkdownStreamRenderer::new();
    let output = renderer.push("| left | mid | right |\n| :--- | :---: | ---: |\n| a | b | c |\n");
    let output = format!("{output}{}", renderer.flush());
    assert!(output.contains('┌'));
    assert!(!output.contains('+'));
    assert!(!output.contains(":---"));
    assert!(output.contains("\x1b[1mleft\x1b[0m"));
}

#[test]
fn does_not_buffer_plain_lines_with_pipes_as_tables() {
    let mut renderer = MarkdownStreamRenderer::new();
    let output = renderer.push("echo hi | wc -l\nnext\n");
    assert!(output.contains("echo hi | wc -l\nnext\n"));
}
