//! 内置 CLI 供应商的模型目录。
//!
//! 它们没有 /models HTTP 端点,但 CLI 自己能列:`agy models`(TSV,
//! `slug<TAB>显示名`)、`codex debug models`(JSON,`models[].slug`,
//! `visibility: hide` 的不列)。目录就问 CLI 要;CLI 不在、超时或输出不认识
//! 一律报错让用户看见(09-03 裁定:不悄悄退回快照——预置表只在首次创建
//! 供应商时当模板用)。配置里手工加的名字并进目录,去重保序。`claude` 没有
//! 列模型的子命令(`--model` 只认 fable/opus/sonnet/haiku 别名或完整名),
//! 它的目录就是那张别名表。

use crate::config::{AppConfig, ProviderConfig};
use anyhow::{bail, Context, Result};
use std::io::Read;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// agy 要联网取目录,给足时间;超时就退回预置表,不让 TUI 干等。
const CLI_LIST_TIMEOUT: Duration = Duration::from_secs(20);

/// 该供应商列模型要跑的二进制;`None` = 没有对应的 CLI 子命令。
pub(crate) fn builtin_cli_binary(config: &AppConfig, provider: &ProviderConfig) -> Option<String> {
    let pick = |configured: &str, fallback: &str| {
        let configured = configured.trim();
        if configured.is_empty() {
            fallback.to_string()
        } else {
            configured.to_string()
        }
    };
    if provider.is_antigravity() {
        Some(pick(&config.plugins.antigravity.binary, "agy"))
    } else if provider.is_codex() {
        Some(pick(&config.plugins.codex.binary, "codex"))
    } else {
        None
    }
}

/// 目录 = CLI 实时列表(claude:别名表)∪ 配置里手工加的。CLI 失败即失败。
pub(in crate::config_tui) fn builtin_cli_catalog(
    provider: &ProviderConfig,
    binary: Option<&str>,
) -> Result<Vec<String>> {
    let mut catalog = match binary {
        Some(binary) => {
            let models = live_catalog(provider, binary)?;
            if models.is_empty() {
                bail!("{binary} listed no models");
            }
            models
        }
        None => provider
            .preset_model_catalog()
            .iter()
            .map(|name| name.to_string())
            .collect(),
    };
    for name in &provider.models {
        if !catalog.iter().any(|known| known == name) {
            catalog.push(name.clone());
        }
    }
    Ok(catalog)
}

fn live_catalog(provider: &ProviderConfig, binary: &str) -> Result<Vec<String>> {
    if provider.is_antigravity() {
        let stdout = run_with_timeout(binary, &["models"], CLI_LIST_TIMEOUT)?;
        Ok(parse_agy_models(&stdout))
    } else if provider.is_codex() {
        let stdout = run_with_timeout(binary, &["debug", "models"], CLI_LIST_TIMEOUT)?;
        parse_codex_models(&stdout)
    } else {
        bail!("this CLI has no model listing command")
    }
}

/// `agy models`:每行 `slug<TAB>显示名`;首行 "Fetching available models..."
/// 之类的提示没有 TAB,自然被跳过。
pub(crate) fn parse_agy_models(stdout: &str) -> Vec<String> {
    stdout
        .lines()
        .filter_map(|line| line.split_once('\t'))
        .map(|(slug, _)| slug.trim().to_string())
        .filter(|slug| !slug.is_empty())
        .collect()
}

/// `codex debug models`:`{"models":[{"slug":…,"visibility":"list"|"hide",…}]}`。
pub(crate) fn parse_codex_models(stdout: &str) -> Result<Vec<String>> {
    let start = stdout
        .find('{')
        .context("codex debug models printed no JSON")?;
    let value: serde_json::Value =
        serde_json::from_str(stdout[start..].trim()).context("codex debug models JSON")?;
    let models = value
        .get("models")
        .and_then(serde_json::Value::as_array)
        .context("codex debug models: missing models array")?;
    Ok(models
        .iter()
        .filter(|model| model.get("visibility").and_then(serde_json::Value::as_str) != Some("hide"))
        .filter_map(|model| model.get("slug").and_then(serde_json::Value::as_str))
        .map(str::to_string)
        .collect())
}

/// 跑子进程收 stdout,超时就杀。stdout 在独立线程里读:codex 的输出有几百
/// KB,超过管道缓冲,不边读边等会死锁。
fn run_with_timeout(binary: &str, args: &[&str], timeout: Duration) -> Result<String> {
    let mut child = Command::new(binary)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| format!("failed to start {binary}"))?;
    let mut stdout = child.stdout.take().context("stdout pipe")?;
    let reader = std::thread::spawn(move || {
        let mut buffer = String::new();
        let _ = stdout.read_to_string(&mut buffer);
        buffer
    });
    let started = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if started.elapsed() > timeout {
            let _ = child.kill();
            let _ = child.wait();
            bail!(
                "{binary} {} timed out after {}s",
                args.join(" "),
                timeout.as_secs()
            );
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    let output = reader.join().unwrap_or_default();
    if !status.success() {
        bail!("{binary} {} exited with {status}", args.join(" "));
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agy_listing_skips_the_banner_and_keeps_slugs() {
        let out = "Fetching available models...\ngemini-3.8-flash-high\tGemini 3.8 Flash (High)\nclaude-sonnet-4-6\tClaude Sonnet 4.6 (Thinking)\n\n";
        assert_eq!(
            parse_agy_models(out),
            vec![
                "gemini-3.8-flash-high".to_string(),
                "claude-sonnet-4-6".to_string()
            ]
        );
    }

    #[test]
    fn codex_listing_drops_hidden_models_and_tolerates_a_banner() {
        let out = "warming up\n{\"models\":[{\"slug\":\"gpt-reserve\",\"visibility\":\"hide\"},{\"slug\":\"gpt-5.6-luna\",\"visibility\":\"list\"},{\"slug\":\"gpt-5.5\"}]}";
        assert_eq!(
            parse_codex_models(out).unwrap(),
            vec!["gpt-5.6-luna".to_string(), "gpt-5.5".to_string()]
        );
        assert!(parse_codex_models("nothing here").is_err());
    }

    #[test]
    fn a_missing_cli_is_an_error_not_a_silent_preset() {
        let mut provider = ProviderConfig::antigravity_template();
        provider.models = vec!["custom-alias".to_string()];
        let error = builtin_cli_catalog(&provider, Some("/nonexistent/agy-binary")).unwrap_err();
        assert!(error.to_string().contains("failed to start"));
        // claude 没有列模型命令:目录就是别名表,并上手工加的。
        let mut claude = ProviderConfig::claude_code_template();
        claude.models = vec!["claude-fable-5".to_string()];
        let config = AppConfig::default();
        assert!(builtin_cli_binary(&config, &claude).is_none());
        let models = builtin_cli_catalog(&claude, None).unwrap();
        assert_eq!(models.len(), claude.preset_model_catalog().len() + 1);
        assert_eq!(models.last().map(String::as_str), Some("claude-fable-5"));
    }
}

/// 真机探针:`cargo test --lib live_cli_catalog -- --ignored --nocapture`。
#[cfg(test)]
mod live_probe {
    use super::*;

    #[test]
    #[ignore]
    fn live_cli_catalog() {
        let config = AppConfig::default();
        for provider in [
            ProviderConfig::antigravity_template(),
            ProviderConfig::codex_template(),
        ] {
            let binary = builtin_cli_binary(&config, &provider);
            match builtin_cli_catalog(&provider, binary.as_deref()) {
                Ok(models) => eprintln!(
                    "{}: {} models: {}",
                    provider.id,
                    models.len(),
                    models.join(", ")
                ),
                Err(error) => eprintln!("{}: ERROR {error:#}", provider.id),
            }
        }
    }
}
