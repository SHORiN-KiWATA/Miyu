mod dashboard;
mod header;
mod index;
mod manage;
mod refresh;
pub(crate) use dashboard::*;
pub(crate) use header::*;
pub(crate) use index::*;
pub(crate) use manage::*;
pub(crate) use refresh::*;

use super::registry::{ScriptScope, UnregisteredScript};
use super::{ToolPermission, ToolProgress, ToolRegistry, ToolSpec, ToolTrust};
use crate::i18n::is_zh;
use crate::paths::MiyuPaths;
use crate::tools::tool_descriptions::LoadPolicy;
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use tokio::process::Command;

const SCRIPT_TIMEOUT_SECS: u64 = 120;
const MAX_SCRIPT_OUTPUT_CHARS: usize = 20_000;
/// `MIYU_ARGS_JSON` 的上限。Linux 单个环境变量 128KB 封顶,留一半余量;超了
/// 只走 stdin——stdin 那份永远在,环境变量只是让脚本少写一段读管道的代码。
const MAX_ARGS_ENV_BYTES: usize = 64 * 1024;

pub fn register(registry: &mut ToolRegistry, config: &crate::config::AppConfig, paths: &MiyuPaths) {
    // 内置脚本装在 <system>/personas/default/ 下,自定义人格天然扫不到——
    // 别人换上自定义人格拿到纯净状态(09-01)。覆盖链与四层细节见
    // script_scan_roots。启动这一次也走指纹路径,后续回合只在目录变了才重扫。
    match prepare_script_refresh(None, config, paths) {
        Ok(Some(snapshot)) => apply_script_refresh(registry, paths, snapshot),
        Ok(None) => {}
        Err(error) => {
            tracing::warn!(error = %error, "failed to scan Miyu script directories during tool registration");
        }
    }
    register_script_tools(registry, config.clone(), paths.clone());
}

/// 不可信场所的脚本面:只收头部写了 `Trust: external` 的脚本,不给 manage_script。
/// 范围记在注册表上,热刷新走同一条 replace_script_tools 时照样过滤。
pub fn register_external(
    registry: &mut ToolRegistry,
    config: &crate::config::AppConfig,
    paths: &MiyuPaths,
) {
    registry.set_script_scope(ScriptScope::ExternalOnly);
    match prepare_script_refresh(None, config, paths) {
        Ok(Some(snapshot)) => apply_script_refresh(registry, paths, snapshot),
        Ok(None) => {}
        Err(error) => {
            tracing::warn!(error = %error, "failed to scan Miyu script directories for the external registry");
        }
    }
}

/// 脚本 stdout 里的附件行:`MIYU-IMAGE: <路径> | <说明>`。整行从输出里摘掉,
/// 图片交给投递层(终端内联/WebUI/QQ 各自渲染),说明可省。相对路径按
/// 脚本缓存目录解析。
pub(crate) const IMAGE_MARKER: &str = "MIYU-IMAGE:";

pub(crate) fn split_attachment_lines(
    stdout: &str,
    cache_dir: &Path,
) -> (String, Vec<(PathBuf, String)>) {
    let mut kept = Vec::new();
    let mut images = Vec::new();
    for line in stdout.lines() {
        let Some(rest) = line.trim_start().strip_prefix(IMAGE_MARKER) else {
            kept.push(line);
            continue;
        };
        let (path, alt) = rest
            .split_once('|')
            .map(|(path, alt)| (path.trim(), alt.trim()))
            .unwrap_or((rest.trim(), ""));
        if path.is_empty() {
            continue;
        }
        let path = Path::new(path);
        let path = if path.is_absolute() {
            path.to_path_buf()
        } else {
            cache_dir.join(path)
        };
        images.push((path, alt.to_string()));
    }
    (kept.join("\n"), images)
}

async fn run_script(
    path_str: &str,
    scripts_dir: &Path,
    cache_dir: &Path,
    args: &Value,
    timeout_secs: u64,
    argv: ArgvMode,
    progress: &ToolProgress,
) -> Result<String> {
    let script_path = resolve_script_path(path_str, scripts_dir);

    if !script_path.is_file() {
        bail!("script not found: {}", script_path.display());
    }

    let args_json = serde_json::to_string(args).unwrap_or_default();
    let stdin_input = match args.get("stdin").and_then(Value::as_str) {
        Some(text) if !text.is_empty() => text.to_string(),
        _ => args_json.clone(),
    };

    let mut command = Command::new(&script_path);
    // argv=flags:参数同时展成 --key=value(header.rs 的 ArgvMode),脚本用
    // argparse/getopt 即可;stdin 那份照发,两路内容一致。
    if argv == ArgvMode::Flags {
        command.args(argv_flags(args));
    }
    // 脚本的中间产物(登录 profile、会话快照、查询票据、二维码图)统一收进
    // Miyu 自己的缓存目录,`miyu wipe` 清 ~/.miyu 时一并带走,不在用户的
    // ~/.cache 下散落一堆。脚本单独拿到终端跑时这个变量不存在,退回 XDG
    // 默认——两种用法各自有各自的登录态,互不覆盖。
    command.env("MIYU_SCRIPT_CACHE_DIR", cache_dir);
    if args_json.len() <= MAX_ARGS_ENV_BYTES {
        command.env("MIYU_ARGS_JSON", &args_json);
    }
    command.stdin(Stdio::piped());
    command.stdout(Stdio::piped());
    command.stderr(Stdio::piped());
    command.kill_on_drop(true);

    let mut child = command.spawn()?;
    let stdin_pipe = child.stdin.take();

    // Collect with a hard per-stream cap: wait_with_output() buffers
    // without bounds, so a runaway script could exhaust memory.
    // stdin 写入必须在 timeout 之内且与读取并发:脚本不读 stdin 且输入
    // 超过管道缓冲时 write_all 永久 pending,放在超时外整个 future 就
    // 永远不返回,kill_on_drop 也无从生效。
    let stdout_pipe = child.stdout.take();
    let stderr_pipe = child.stderr.take();
    let (status, stdout_bytes, stderr_bytes) =
        tokio::time::timeout(std::time::Duration::from_secs(timeout_secs), async {
            let write_stdin = async {
                if let Some(mut stdin) = stdin_pipe {
                    if !stdin_input.is_empty() {
                        use tokio::io::AsyncWriteExt;
                        let _ = stdin.write_all(stdin_input.as_bytes()).await;
                    }
                    // drop 关闭写端,脚本读 stdin 时拿到 EOF。
                }
            };
            let (_, stdout_bytes, stderr_bytes, status) = tokio::join!(
                write_stdin,
                read_capped_stream(stdout_pipe),
                read_capped_stream(stderr_pipe),
                child.wait(),
            );
            status.map(|status| (status, stdout_bytes, stderr_bytes))
        })
        .await
        .map_err(|_| anyhow::anyhow!("script timed out after {timeout_secs}s"))??;

    let stdout = String::from_utf8_lossy(&stdout_bytes);
    let stderr = String::from_utf8_lossy(&stderr_bytes);
    let (stdout, images) = split_attachment_lines(stdout.trim(), cache_dir);
    for (path, alt) in images {
        if path.is_file() {
            progress.report_image(path, alt);
        } else {
            tracing::warn!(path = %path.display(), "script reported an image that does not exist");
        }
    }
    let stdout = clip_output(stdout.trim());
    let stderr = clip_output(stderr.trim());

    Ok(serde_json::to_string_pretty(&json!({
        "success": status.success(),
        "exit_code": status.code(),
        "stdout": stdout,
        "stderr": stderr,
    }))?)
}

/// `{"query":"x","limit":5,"json":true,"dry":false}` → `--query=x --limit=5 --json`。
/// 键按字典序,输出确定;`=` 连写让负数和以 `-` 开头的值也能被 argparse 收下;
/// true 只给旗标,false/null 省略;数组与对象给紧凑 JSON 字符串。`stdin` 是
/// 泛化 schema 的透传口,不算参数。
pub(crate) fn argv_flags(args: &Value) -> Vec<String> {
    let Some(object) = args.as_object() else {
        return Vec::new();
    };
    let mut keys: Vec<&String> = object.keys().collect();
    keys.sort();
    let mut flags = Vec::new();
    for key in keys {
        if key == "stdin" {
            continue;
        }
        match &object[key] {
            Value::Null | Value::Bool(false) => {}
            Value::Bool(true) => flags.push(format!("--{key}")),
            Value::String(text) => flags.push(format!("--{key}={text}")),
            Value::Number(number) => flags.push(format!("--{key}={number}")),
            other => flags.push(format!("--{key}={other}")),
        }
    }
    flags
}

/// Drains a child stream, keeping at most 8MB in memory.
async fn read_capped_stream(reader: Option<impl tokio::io::AsyncRead + Unpin>) -> Vec<u8> {
    use tokio::io::AsyncReadExt;
    const CAP: usize = 8 * 1024 * 1024;
    let Some(mut reader) = reader else {
        return Vec::new();
    };
    let mut output = Vec::new();
    let mut truncated = false;
    let mut buffer = [0u8; 8192];
    loop {
        match reader.read(&mut buffer).await {
            Ok(0) | Err(_) => break,
            Ok(read) => {
                let remaining = CAP.saturating_sub(output.len());
                if remaining == 0 {
                    truncated = true;
                    continue;
                }
                let take = read.min(remaining);
                if take < read {
                    truncated = true;
                }
                output.extend_from_slice(&buffer[..take]);
            }
        }
    }
    if truncated {
        output.extend_from_slice(b"\n[truncated at 8MB]");
    }
    output
}

fn clip_output(value: &str) -> String {
    if value.chars().count() <= MAX_SCRIPT_OUTPUT_CHARS {
        value.to_string()
    } else {
        format!(
            "{}\n...[{} {MAX_SCRIPT_OUTPUT_CHARS} {}]",
            value
                .chars()
                .take(MAX_SCRIPT_OUTPUT_CHARS)
                .collect::<String>(),
            "truncated to",
            "chars"
        )
    }
}

pub(crate) fn make_executable(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(path)?.permissions();
        perms.set_mode(perms.mode() | 0o111);
        std::fs::set_permissions(path, perms)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests;
