//! 内置 Antigravity(agy CLI)特殊供应商的专用编辑表单。
//!
//! 与 Claude Code 表单同构、不共享字段:没有 HTTP 概念,只有启用总开关与
//! CLI 中转设置(落盘在 plugins.antigravity)。没有权限模式一栏——agy 无头模式
//! 只有「全放行」一种能干活的姿态(不放行时命令被拒且 result 仍报成功)。

use crate::config_tui::*;

const TOOL_SCOPES: &[&str] = &["off", "dev", "normal", "all"];

pub(in crate::config_tui) fn edit_antigravity_provider_form(
    stdout: &mut io::Stdout,
    provider: ProviderConfig,
    plugin: &mut crate::config::AntigravityPluginConfig,
) -> Result<Option<ProviderConfig>> {
    let mut fields = vec![
        Field::new(
            t("Enabled (Antigravity relay)", "启用(中转 Antigravity)"),
            provider.enabled.to_string(),
        )
        .choices(&["true", "false"]),
        Field::new(t("Display name", "显示名称"), provider.display_name.clone()),
        Field::new(
            t("agy binary (empty = PATH)", "agy 可执行文件(空=PATH)"),
            plugin.binary.clone(),
        ),
        Field::new(
            t("agy native tools scope", "agy 原生工具作用域"),
            plugin.native_tools.clone(),
        )
        .choices(TOOL_SCOPES),
        Field::new(
            t(
                "Miyu tools via MCP bridge scope",
                "Miyu 工具挂给 agy 的作用域",
            ),
            plugin.miyu_tools.clone(),
        )
        .choices(TOOL_SCOPES),
        Field::new(
            t(
                "Register bridged tools eagerly",
                "桥工具 eager 注册(原生名直调)",
            ),
            plugin.miyu_tools_eager.to_string(),
        )
        .choices(&["true", "false"]),
        Field::new(
            t("Stream idle watchdog (seconds)", "流空闲看门狗(秒)"),
            plugin.idle_timeout_seconds.to_string(),
        ),
        Field::new(
            t(
                "agy --print-timeout (seconds)",
                "agy 整轮上限 --print-timeout(秒)",
            ),
            plugin.print_timeout_seconds.to_string(),
        ),
    ];
    loop {
        if !run_form(
            stdout,
            t(" EDIT ANTIGRAVITY ", " 编辑 Antigravity "),
            &mut fields,
        )? {
            return Ok(None);
        }
        let enabled = match parse_bool_field(&fields[0].value) {
            Ok(value) => value,
            Err(error) => {
                message(stdout, &format!("{error:#}"))?;
                continue;
            }
        };
        let eager = match parse_bool_field(&fields[5].value) {
            Ok(value) => value,
            Err(error) => {
                message(stdout, &format!("{error:#}"))?;
                continue;
            }
        };
        plugin.binary = fields[2].value.trim().to_string();
        plugin.native_tools = normalize_scope(&fields[3].value);
        plugin.miyu_tools = normalize_scope(&fields[4].value);
        plugin.miyu_tools_eager = eager;
        plugin.idle_timeout_seconds = fields[6].value.trim().parse().unwrap_or(300);
        plugin.print_timeout_seconds = fields[7].value.trim().parse().unwrap_or(24 * 60 * 60);
        let mut updated = provider.clone();
        updated.enabled = enabled;
        let display_name = fields[1].value.trim();
        updated.display_name = if display_name.is_empty() {
            "Antigravity".to_string()
        } else {
            display_name.to_string()
        };
        return Ok(Some(updated));
    }
}

/// 手输的作用域值归一到四档;认不出的按 off 兜底(与运行时判定一致)。
fn normalize_scope(value: &str) -> String {
    let value = value.trim().to_ascii_lowercase();
    if TOOL_SCOPES.contains(&value.as_str()) {
        value
    } else {
        "off".to_string()
    }
}
