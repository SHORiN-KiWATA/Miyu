//! 内置 Codex(OpenAI codex CLI)特殊供应商的专用编辑表单。
//!
//! 与另两条 CLI 线的表单同构、不共享字段:没有 HTTP 概念,只有启用总开关
//! 与 CLI 中转设置(落盘在 plugins.codex)。

use crate::config_tui::*;

const TOOL_SCOPES: &[&str] = &["off", "dev", "normal", "all"];
const SANDBOX_MODES: &[&str] = &["danger-full-access", "workspace-write", "read-only"];

pub(in crate::config_tui) fn edit_codex_provider_form(
    stdout: &mut io::Stdout,
    provider: ProviderConfig,
    plugin: &mut crate::config::CodexPluginConfig,
) -> Result<Option<ProviderConfig>> {
    let mut fields = vec![
        Field::new(
            t("Enabled (Codex relay)", "启用(中转 Codex)"),
            provider.enabled.to_string(),
        )
        .choices(&["true", "false"]),
        Field::new(t("Display name", "显示名称"), provider.display_name.clone()),
        Field::new(
            t("codex binary (empty = PATH)", "codex 可执行文件(空=PATH)"),
            plugin.binary.clone(),
        ),
        Field::new(
            t("codex native tools scope", "codex 原生工具作用域"),
            plugin.native_tools.clone(),
        )
        .choices(TOOL_SCOPES),
        Field::new(
            t(
                "Miyu tools via MCP bridge scope",
                "Miyu 工具挂给 codex 的作用域",
            ),
            plugin.miyu_tools.clone(),
        )
        .choices(TOOL_SCOPES),
        Field::new(t("Sandbox mode", "沙箱模式"), plugin.sandbox_mode.clone())
            .choices(SANDBOX_MODES),
        Field::new(
            t(
                "Ignore user config.toml",
                "忽略用户自己的 ~/.codex/config.toml",
            ),
            plugin.ignore_user_config.to_string(),
        )
        .choices(&["true", "false"]),
        Field::new(
            t("Stream idle watchdog (seconds)", "流空闲看门狗(秒)"),
            plugin.idle_timeout_seconds.to_string(),
        ),
    ];
    loop {
        if !run_form(stdout, t(" EDIT CODEX ", " 编辑 Codex "), &mut fields)? {
            return Ok(None);
        }
        let enabled = match parse_bool_field(&fields[0].value) {
            Ok(value) => value,
            Err(error) => {
                message(stdout, &format!("{error:#}"))?;
                continue;
            }
        };
        let ignore_user_config = match parse_bool_field(&fields[6].value) {
            Ok(value) => value,
            Err(error) => {
                message(stdout, &format!("{error:#}"))?;
                continue;
            }
        };
        plugin.binary = fields[2].value.trim().to_string();
        plugin.native_tools = normalize_scope(&fields[3].value);
        plugin.miyu_tools = normalize_scope(&fields[4].value);
        let sandbox = fields[5].value.trim().to_string();
        plugin.sandbox_mode = if SANDBOX_MODES.contains(&sandbox.as_str()) {
            sandbox
        } else {
            "danger-full-access".to_string()
        };
        plugin.ignore_user_config = ignore_user_config;
        plugin.idle_timeout_seconds = fields[7].value.trim().parse().unwrap_or(300);
        let mut updated = provider.clone();
        updated.enabled = enabled;
        let display_name = fields[1].value.trim();
        updated.display_name = if display_name.is_empty() {
            "Codex".to_string()
        } else {
            display_name.to_string()
        };
        return Ok(Some(updated));
    }
}

fn normalize_scope(value: &str) -> String {
    let value = value.trim().to_ascii_lowercase();
    if TOOL_SCOPES.contains(&value.as_str()) {
        value
    } else {
        "off".to_string()
    }
}
