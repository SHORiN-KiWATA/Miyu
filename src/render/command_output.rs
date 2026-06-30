use crate::i18n::text as t;
use anyhow::Result;
use serde_json::Value;
use std::io::{self, Write};

/// 写入普通工具参数或输出块。
///
/// 参数:
/// - `stdout`: 标准输出句柄
/// - `label`: 输出块标签
/// - `payload`: 原始工具载荷
///
/// 返回:
/// - 写入是否成功
pub(crate) fn write_tool_payload(
    stdout: &mut io::Stdout,
    label: &str,
    payload: &str,
) -> Result<()> {
    let formatted = format_tool_payload(payload);
    writeln!(stdout, "\x1b[2m{label}:\x1b[0m")?;
    for line in formatted.lines() {
        writeln!(stdout, "\x1b[2m  {line}\x1b[0m")?;
    }
    Ok(())
}

/// 写入命令调用块。
///
/// 参数:
/// - `stdout`: 标准输出句柄
/// - `arguments`: 工具调用参数
///
/// 返回:
/// - 写入是否成功
pub(crate) fn write_command_block(stdout: &mut io::Stdout, arguments: &str) -> Result<()> {
    let parsed = serde_json::from_str::<Value>(arguments).ok();
    let command = parsed
        .as_ref()
        .and_then(|value| value.get("command"))
        .and_then(Value::as_str)
        .unwrap_or(arguments)
        .trim();
    writeln!(stdout, "\x1b[2m,-- {}\x1b[0m", t("command", "命令"))?;
    writeln!(stdout, "\x1b[33m$ {command}\x1b[0m")?;
    writeln!(stdout, "\x1b[2m`--\x1b[0m")?;
    Ok(())
}

/// 按命令执行结果写入 stdout 和 stderr 块。
///
/// 参数:
/// - `stdout`: 标准输出句柄
/// - `output`: 命令执行结果 JSON
///
/// 返回:
/// - 写入是否成功
pub(crate) fn write_command_result_blocks(stdout: &mut io::Stdout, output: &str) -> Result<()> {
    let Some(result) = parse_command_result(output) else {
        return write_tool_payload(stdout, t("output", "输出"), output);
    };
    if !result.stdout.trim().is_empty() {
        write_fenced_block(stdout, t("output", "输出"), &result.stdout)?;
    }
    if !result.stderr.trim().is_empty() {
        let label = result
            .exit_code
            .map(|code| format!("err exit {code}"))
            .unwrap_or_else(|| "err".to_string());
        write_fenced_block(stdout, &label, &result.stderr)?;
    } else if !result.success {
        let label = result
            .exit_code
            .map(|code| format!("err exit {code}"))
            .unwrap_or_else(|| "err".to_string());
        write_fenced_block(
            stdout,
            &label,
            t(
                "command failed without stderr",
                "命令失败，但没有 stderr 输出",
            ),
        )?;
    }
    Ok(())
}

/// 写入命令失败摘要块。
///
/// 参数:
/// - `stdout`: 标准输出句柄
/// - `output`: 命令执行结果 JSON
///
/// 返回:
/// - 写入是否成功
pub(crate) fn write_command_error_block(stdout: &mut io::Stdout, output: &str) -> Result<()> {
    let Some(result) = parse_command_result(output) else {
        return write_fenced_block(stdout, "err", output);
    };
    if result.success {
        return Ok(());
    }
    let label = result
        .exit_code
        .map(|code| format!("err exit {code}"))
        .unwrap_or_else(|| "err".to_string());
    let message = if result.stderr.trim().is_empty() {
        result.stdout.as_str()
    } else {
        result.stderr.as_str()
    };
    write_fenced_block(stdout, &label, message)
}

/// 写入带边界的文本块。
///
/// 参数:
/// - `stdout`: 标准输出句柄
/// - `label`: 文本块标签
/// - `text`: 文本内容
///
/// 返回:
/// - 写入是否成功
fn write_fenced_block(stdout: &mut io::Stdout, label: &str, text: &str) -> Result<()> {
    writeln!(stdout, "\x1b[2m,-- {label}\x1b[0m")?;
    for line in truncate_chars(text.trim(), 2400).lines() {
        writeln!(stdout, "\x1b[33m{line}\x1b[0m")?;
    }
    writeln!(stdout, "\x1b[2m`--\x1b[0m")?;
    Ok(())
}

struct CommandResult {
    success: bool,
    exit_code: Option<i64>,
    stdout: String,
    stderr: String,
}

/// 解析命令工具返回的 JSON 结果。
///
/// 参数:
/// - `output`: 原始 JSON 文本
///
/// 返回:
/// - 解析后的命令结果，解析失败时返回空
fn parse_command_result(output: &str) -> Option<CommandResult> {
    let value = serde_json::from_str::<Value>(output.trim()).ok()?;
    Some(CommandResult {
        success: value.get("success")?.as_bool()?,
        exit_code: value.get("exit_code").and_then(Value::as_i64),
        stdout: value
            .get("stdout")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        stderr: value
            .get("stderr")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
    })
}

/// 格式化工具载荷并限制长度。
///
/// 参数:
/// - `payload`: 原始工具载荷
///
/// 返回:
/// - 格式化后的文本
fn format_tool_payload(payload: &str) -> String {
    let text = payload.trim();
    let formatted = serde_json::from_str::<Value>(text)
        .ok()
        .and_then(|value| serde_json::to_string_pretty(&value).ok())
        .unwrap_or_else(|| text.to_string());
    truncate_chars(&formatted, 2400)
}

/// 按字符数量截断文本。
///
/// 参数:
/// - `text`: 原始文本
/// - `max_chars`: 最大字符数
///
/// 返回:
/// - 截断后的文本
fn truncate_chars(text: &str, max_chars: usize) -> String {
    let total = text.chars().count();
    if total <= max_chars {
        return text.to_string();
    }
    let omitted = total - max_chars;
    format!(
        "{}\n... {} {omitted} {} ...",
        text.chars().take(max_chars).collect::<String>(),
        t("truncated", "已截断"),
        t("chars", "字符")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_command_result_json() {
        let result = parse_command_result(
            r#"{"success":false,"exit_code":1,"stdout":"unused","stderr":"not found"}"#,
        )
        .unwrap();
        assert!(!result.success);
        assert_eq!(result.exit_code, Some(1));
        assert_eq!(result.stdout, "unused");
        assert_eq!(result.stderr, "not found");
    }
}
