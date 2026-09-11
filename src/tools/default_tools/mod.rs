mod command;
mod files;
mod sysinfo;
use command::*;
// claude_code 复用同一套进程组击杀语义,不再抄一份。
pub(crate) use files::*;
use sysinfo::*;

use super::{CommandOutputStream, ToolProgress, ToolRegistry, ToolSpec};
use crate::host_info::{parse_macos_system_version, read_small_file};
use crate::tools::patch_preview::write_with_patch_preview;
use anyhow::{bail, Result};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use tokio::io::AsyncReadExt;
use tokio::process::Command;

/// 进度消息前缀:带它的内容是「本次调用的最终摘要」,由渲染层原样按行展示,
/// 而不是当成一闪而过的进度。回收站用它交代失败清单。
pub(crate) const TOOL_SUMMARY_PREFIX: &str = "__tool_summary__";

pub fn register(
    registry: &mut ToolRegistry,
    allow_command_execution: bool,
    config: &crate::config::AppConfig,
    paths: &crate::paths::MiyuPaths,
) {
    register_readonly(registry, config, paths);
    register_run_command(registry, allow_command_execution);
    registry.register(ToolSpec::new_with_progress(
        "trash_path",
        "Move files, directories, or symlinks to the system Trash instead of permanently deleting them. Pass every path in one call — one call per path floods the transcript. Use this when the user asks to delete/remove/clean up local paths; do not use rm unless explicitly requested.",
        json!({"type":"object","properties":{"paths":{"type":"array","items":{"type":"string"},"minItems":1,"description": "Paths to move to Trash. Absolute, workspace-relative, and ~/ paths are all accepted."}},"required":["paths"],"additionalProperties":false}),
        |args, progress| async move { trash_paths(args, progress) },
    ).writes());
}

/// `run_command` 单独可注册:dev 模式只挂它(+后台任务管理),不连带
/// coreutils 可替代的读写全家(验收三轮裁剪)。
pub fn register_run_command(registry: &mut ToolRegistry, allow_command_execution: bool) {
    registry.register(ToolSpec::new_with_progress(
        "run_command",
        "Run a shell command in the workspace when skills.allow_command_execution is enabled. Set background=true for long-running commands (builds, dev servers): it returns a job_id immediately; poll with job(action=status) and stop with job(action=stop).",
        json!({"type":"object","properties":{"command":{"type":"string","description": "Command to run."},"timeout_seconds":{"type":"integer","description": "Optional timeout in seconds (1-120, default 30). Ignored when background=true."},"background":{"type":"boolean","description": "Run detached as a background command and return a short job_id immediately."},"title":{"type":"string","description": "Short display title (<=16 chars) for the background command."}},"required":["command"],"additionalProperties":false}),
        move |args, progress| async move {
            run_command(args, allow_command_execution, progress).await
        },
    ).writes());
}

/// 只读工具集。计划模式移除后这里不再注册 `run_command`——它在
/// `register` 里紧接着就会被可写版覆盖,留着只是一份读不到的死描述。
pub fn register_readonly(
    registry: &mut ToolRegistry,
    config: &crate::config::AppConfig,
    paths: &crate::paths::MiyuPaths,
) {
    registry.register(ToolSpec::new(
        "check_os_info",
        "Check basic read-only OS, shell, desktop session, kernel, host, and package-manager context. For concrete Linux input method issues, load the linux-input-method-diagnose skill.",
        json!({"type":"object","properties":{},"additionalProperties":false}),
        |_| async move { check_os_info() },
    ));
    // 08-21 Edit/Read 统一:read_file 更名 read,并认 `artifact:`/`kb:` 前缀
    // (Artifact 库与知识库的读取工具随之退场)。
    let read_config = config.clone();
    let read_paths = paths.clone();
    registry.register(ToolSpec::new(
        "read",
        "Read a UTF-8 text file by 1-based line offset, or list a directory page. Use absolute paths, workspace-relative paths, or ~/ paths. Large files are paged and binary files are refused.",
        json!({"type":"object","properties":{"path":{"type":"string","description": "File or directory path."},"offset":{"type":"integer","description": "Starting line, 1-based."},"limit":{"type":"integer","description": "Maximum lines to read."}},"required":["path"],"additionalProperties":false}),
        move |args| {
            let config = read_config.clone();
            let paths = read_paths.clone();
            async move { read_dispatch(args, &config, &paths) }
        },
    ));
    registry.register(ToolSpec::new(
        "glob",
        "Find files by case-insensitive glob pattern under a directory. Defaults to workspace; use ~ or /home for user files, or / for protected global search.",
        json!({"type":"object","properties":{"path":{"type":"string","description": "Directory to search. Defaults to workspace; use ~ or /home for user files, or / for protected global search."},"pattern":{"type":"string","description": "Case-insensitive glob pattern, for example *ai*test*."},"max_results":{"type":"integer","description": "Maximum results."}},"required":["pattern"],"additionalProperties":false}),
        |args| async move { glob_files(args).await },
    ));
    registry.register(ToolSpec::new(
        "grep",
        "Search file contents using ripgrep under a directory or single file. Defaults to workspace; use ~ or /home for user files, or / for protected global search. No matches are returned as an empty ok result.",
        json!({"type":"object","properties":{"path":{"type":"string","description": "Directory or file to search. Defaults to workspace; use ~ or /home for user files, or / for protected global search."},"pattern":{"type":"string","description": "Regex pattern."},"include":{"type":"string","description": "Optional case-insensitive file glob filter."},"max_results":{"type":"integer","description": "Maximum matches."}},"required":["pattern"],"additionalProperties":false}),
        |args| async move { grep_text(args).await },
    ));
}

/// `read` 的命名空间分发:`artifact:名字` 读当前会话的 Artifact 库
/// (空名字=列清单),`kb:相对路径` 读知识库,其余走文件系统原路。
fn read_dispatch(
    mut args: Value,
    config: &crate::config::AppConfig,
    paths: &crate::paths::MiyuPaths,
) -> Result<String> {
    let path_arg = args
        .get("path")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim()
        .to_string();
    if let Some(name) = path_arg.strip_prefix("artifact:") {
        let session = crate::tools::workspace::try_session()
            .ok_or_else(|| anyhow::anyhow!("artifact: paths require a session turn"))?;
        let name = name.trim();
        // 成员回合的 artifact 库在自己家里(artifacts_root 按 config.member_home_dir()
        // 解析),不能用 admin_owned 的 paths.artifacts_dir()——否则成员读
        // `artifact:x` 会解析到管理员的 home 再被沙盒挡下(09-11 实测)。
        let root = crate::tools::artifact::artifacts_root(config, paths);
        if name.is_empty() {
            return crate::tools::artifact::managed_manifest(&root, &session);
        }
        let resolved = crate::tools::artifact::managed_file_path(&root, &session, name)?;
        args["path"] = Value::String(resolved.to_string_lossy().to_string());
    } else if let Some(rel) = path_arg.strip_prefix("kb:") {
        let kb = crate::tools::knowledge_base::KnowledgeBase::new(config.clone(), paths.clone())?;
        let resolved = kb.safe_file_path(rel.trim())?;
        args["path"] = Value::String(resolved.to_string_lossy().to_string());
    }
    read_file(args)
}

fn clip_output_with_meta(value: &str) -> ClippedOutput {
    let value = value.trim();
    let total = value.chars().count();
    if total <= MAX_COMMAND_OUTPUT_CHARS {
        return ClippedOutput {
            text: value.to_string(),
            truncated: false,
            omitted_chars: 0,
        };
    }
    let omitted = total - MAX_COMMAND_OUTPUT_CHARS;
    let tail = value
        .chars()
        .skip(omitted)
        .collect::<String>()
        .trim_start_matches('\n')
        .to_string();
    ClippedOutput {
        text: format!(
            "...[{} {omitted} {}]\n{tail}",
            "omitted", "chars, showing tail"
        ),
        truncated: true,
        omitted_chars: omitted,
    }
}

fn command_output_limited(output: std::process::Output, max_lines: usize) -> Result<String> {
    let stdout_raw = String::from_utf8_lossy(&output.stdout);
    let mut stdout = stdout_raw
        .lines()
        .take(max_lines)
        .collect::<Vec<_>>()
        .join("\n");
    if stdout_raw.lines().nth(max_lines).is_some() {
        stdout.push_str(&format!(
            "\n[{} {max_lines} {}]",
            "truncated to the first", "results"
        ));
    }
    Ok(command_text(
        output.status,
        stdout.into_bytes(),
        output.stderr,
    ))
}

fn search_output_limited(output: std::process::Output, max_lines: usize) -> Result<String> {
    // rg 的 "无匹配" 是退出码 1 + 空 stdout。那不是失败,别让模型看到
    // `[exit code: 1]` 后误以为搜索坏了。
    if output.status.code() == Some(1) && output.stdout.is_empty() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stderr = stderr.trim();
        return Ok(if stderr.is_empty() {
            "no matches".to_string()
        } else {
            format!("{}\n[stderr]\n{stderr}", "no matches")
        });
    }
    command_output_limited(output, max_lines)
}

fn prepare_search_path(path: &Path) -> Result<PathBuf> {
    let path = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    if path == Path::new("/usr") || path == Path::new("/var") || path == Path::new("/etc") {
        bail!(
            "refusing broad system search path: {}; use / for protected global search or choose a specific subdirectory",
            path.display()
        );
    }
    Ok(path)
}

fn search_exclude_args(search_root: &Path) -> Vec<String> {
    let mut args = vec!["--glob=!**/.git/**".to_string()];
    if search_root == Path::new("/") {
        args.extend(
            [
                "--glob=!dev/**",
                "--glob=!proc/**",
                "--glob=!sys/**",
                "--glob=!run/**",
                "--glob=!tmp/**",
                "--glob=!var/cache/**",
                "--glob=!var/lib/**",
                "--glob=!var/log/**",
                "--glob=!usr/**",
                "--glob=!nix/**",
                "--glob=!snap/**",
                "--glob=!flatpak/**",
            ]
            .into_iter()
            .map(ToString::to_string),
        );
    }
    args
}

fn ensure_not_binary_file(path: &Path) -> Result<()> {
    let mut file = std::fs::File::open(path)?;
    let mut buffer = [0u8; 8192];
    let read = file.read(&mut buffer)?;
    let sample = &buffer[..read];
    if sample.contains(&0) {
        bail!("cannot read binary file: {}", path.display())
    }
    let non_printable = sample
        .iter()
        .filter(|byte| **byte < 9 || (**byte > 13 && **byte < 32))
        .count();
    if !sample.is_empty() && non_printable * 10 > sample.len() * 3 {
        bail!("cannot read binary file: {}", path.display())
    }
    Ok(())
}

fn ensure_editable_file_path(path: &Path) -> Result<()> {
    let canonical = path.canonicalize()?;
    if !canonical.is_file() {
        bail!("not a regular file: {}", path.display())
    }
    Ok(())
}

fn resolve_existing_path_without_following_leaf(path: &Path) -> Result<PathBuf> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let filename = path
        .file_name()
        .ok_or_else(|| anyhow::anyhow!("refusing to trash a root path: {}", path.display()))?;
    let parent = parent.canonicalize()?;
    let resolved = parent.join(filename);
    std::fs::symlink_metadata(&resolved)?;
    Ok(resolved)
}

fn ensure_safe_trash_target(path: &Path) -> Result<()> {
    let cwd = super::workspace::effective_workdir().canonicalize()?;
    let home = directories::BaseDirs::new().map(|dirs| dirs.home_dir().to_path_buf());
    let dangerous = [
        Path::new("/"),
        Path::new("/bin"),
        Path::new("/boot"),
        Path::new("/dev"),
        Path::new("/etc"),
        Path::new("/home"),
        Path::new("/opt"),
        Path::new("/proc"),
        Path::new("/root"),
        Path::new("/run"),
        Path::new("/sbin"),
        Path::new("/sys"),
        Path::new("/tmp"),
        Path::new("/usr"),
        Path::new("/var"),
    ];
    if dangerous.iter().any(|item| path == *item) {
        bail!(
            "refusing to trash dangerous system path: {}",
            path.display()
        )
    }
    if path == cwd {
        bail!(
            "refusing to trash current workspace root: {}",
            path.display()
        )
    }
    if let Some(home) = home {
        if path == home {
            bail!("refusing to trash home directory: {}", path.display())
        }
        let trash_dir = home.join(".local/share/Trash");
        if path == trash_dir || path.starts_with(&trash_dir) {
            bail!(
                "refusing to trash the Trash directory itself: {}",
                path.display()
            )
        }
    }
    Ok(())
}

fn path_kind(metadata: &std::fs::Metadata) -> &'static str {
    let file_type = metadata.file_type();
    if file_type.is_symlink() {
        "symlink"
    } else if file_type.is_dir() {
        "directory"
    } else if file_type.is_file() {
        "file"
    } else {
        "other"
    }
}

fn max_results(args: &Value) -> usize {
    args.get("max_results")
        .and_then(Value::as_u64)
        .unwrap_or(100)
        .clamp(1, 500) as usize
}

fn path_arg(args: &Value, key: &str) -> Result<PathBuf> {
    let value = required(args, key)?;
    Ok(expand_path(&value))
}

fn optional_path(args: &Value) -> Option<PathBuf> {
    args.get("path")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .map(expand_path)
}

fn expand_path(value: &str) -> PathBuf {
    let value = value.trim();
    if let Some(rest) = value.strip_prefix("~/") {
        if let Some(home) = directories::BaseDirs::new().map(|dirs| dirs.home_dir().to_path_buf()) {
            return home.join(rest);
        }
    }
    let path = Path::new(value);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        super::workspace::effective_workdir().join(path)
    }
}

fn required(args: &Value, key: &str) -> Result<String> {
    let value = args
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim();
    if value.is_empty() {
        bail!("{}: {key}", "required argument missing")
    } else {
        Ok(value.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_trash(args: Value) -> Result<String> {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        trash_paths_with(args, &ToolProgress::new(tx), |path| {
            if std::fs::symlink_metadata(path)?.file_type().is_dir() {
                std::fs::remove_dir_all(path)?;
            } else {
                std::fs::remove_file(path)?;
            }
            Ok(())
        })
    }

    #[tokio::test]
    async fn command_execution_streams_stdout_and_stderr() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let output = execute_command("printf 'out'; printf 'err' >&2", 5, ToolProgress::new(tx))
            .await
            .unwrap();
        // dsh 式纯文本:正文是 stdout,有 stderr 才追加 [stderr] 段,
        // 退出码为 0 时一个标记都不打。
        assert_eq!(output, "out\n[stderr]\nerr");

        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        while let Ok(event) = rx.try_recv() {
            if let crate::tools::ToolProgressEvent::CommandOutput { stream, chunk } = event {
                match stream {
                    CommandOutputStream::Stdout => stdout.extend(chunk),
                    CommandOutputStream::Stderr => stderr.extend(chunk),
                }
            }
        }
        assert_eq!(stdout, b"out");
        assert_eq!(stderr, b"err");
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn command_timeout_kills_descendant_processes() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let result = execute_command("sleep 30 & echo $!; wait", 1, ToolProgress::new(tx)).await;
        assert!(result.is_err());

        let mut stdout = Vec::new();
        while let Ok(event) = rx.try_recv() {
            if let crate::tools::ToolProgressEvent::CommandOutput {
                stream: CommandOutputStream::Stdout,
                chunk,
            } = event
            {
                stdout.extend(chunk);
            }
        }
        let pid = String::from_utf8(stdout)
            .unwrap()
            .trim()
            .parse::<i32>()
            .unwrap();
        let mut gone = false;
        for _ in 0..20 {
            if unsafe { libc::kill(pid, 0) } == -1
                && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
            {
                gone = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        assert!(gone, "descendant process {pid} survived command timeout");
    }

    #[test]
    fn edit_file_replaces_lines() {
        let cwd = std::env::current_dir().unwrap();
        let temp = tempfile::tempdir_in(cwd).unwrap();
        let path = temp.path().join("sample.txt");
        std::fs::write(&path, "one\ntwo\nthree\n").unwrap();
        let result = edit_file(
            json!({
                "path": path.display().to_string(),
                "start_line": 2,
                "end_line": 2,
                "replacement": "TWO\nTWO-B"
            }),
            ToolProgress::default(),
        );
        let data: Value = serde_json::from_str(&result.unwrap()).unwrap();
        assert!(data.get("diff").is_none());
        assert_eq!(
            std::fs::read_to_string(path).unwrap(),
            "one\nTWO\nTWO-B\nthree\n"
        );
    }

    #[test]
    fn edit_file_allows_existing_files_outside_workspace() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("sample.txt");
        std::fs::write(&path, "one\ntwo\n").unwrap();
        edit_file(
            json!({
                "path": path.display().to_string(),
                "start_line": 1,
                "end_line": 2,
                "replacement": "table"
            }),
            ToolProgress::default(),
        )
        .unwrap();
        assert_eq!(std::fs::read_to_string(path).unwrap(), "table\n");
    }

    #[test]
    fn read_file_paginates_text() {
        let cwd = std::env::current_dir().unwrap();
        let temp = tempfile::tempdir_in(cwd).unwrap();
        let path = temp.path().join("sample.txt");
        std::fs::write(&path, "one\ntwo\nthree\n").unwrap();
        let result = read_file(json!({
            "path": path.display().to_string(),
            "offset": 2,
            "limit": 1,
        }))
        .unwrap();
        let data: Value = serde_json::from_str(&result).unwrap();
        assert_eq!(data["type"], "text-page");
        assert_eq!(data["content"], "2: two");
        assert_eq!(data["truncated"], true);
        assert_eq!(data["next"], 3);
    }

    #[test]
    fn read_file_rejects_binary() {
        let cwd = std::env::current_dir().unwrap();
        let temp = tempfile::tempdir_in(cwd).unwrap();
        let path = temp.path().join("sample.bin");
        std::fs::write(&path, [0, 1, 2, 3]).unwrap();
        assert!(read_file(json!({"path": path.display().to_string()})).is_err());
    }

    #[tokio::test]
    async fn glob_files_matches_filename_case_insensitively() {
        let cwd = std::env::current_dir().unwrap();
        let temp = tempfile::tempdir_in(cwd).unwrap();
        let path = temp.path().join("ai测试题.txt");
        std::fs::write(&path, "content").unwrap();
        let result = glob_files(json!({
            "path": temp.path().display().to_string(),
            "pattern": "*Ai*测试*",
        }))
        .await
        .unwrap();
        assert!(result.contains("ai测试题.txt"), "{result}");
        assert!(!result.contains("[exit code:"), "{result}");
    }

    #[tokio::test]
    async fn grep_no_matches_is_successful_empty_result() {
        let cwd = std::env::current_dir().unwrap();
        let temp = tempfile::tempdir_in(cwd).unwrap();
        std::fs::write(temp.path().join("sample.txt"), "hello").unwrap();
        let result = grep_text(json!({
            "path": temp.path().display().to_string(),
            "pattern": "definitely-not-present",
        }))
        .await
        .unwrap();
        // rg 的"无匹配"是退出码 1 + 空 stdout,不能渲染成失败。
        // 工具结果是模型可见面,恒为英文,不随系统 locale 变化。
        assert_eq!(result, "no matches");
    }

    #[test]
    fn root_search_uses_protective_excludes() {
        let root = Path::new("/");
        assert!(prepare_search_path(root).is_ok());
        let args = search_exclude_args(root).join(" ");
        assert!(args.contains("--glob=!proc/**"));
        assert!(args.contains("--glob=!usr/**"));
    }

    #[test]
    fn trash_path_rejects_workspace_root() {
        let cwd = std::env::current_dir().unwrap().canonicalize().unwrap();
        assert!(ensure_safe_trash_target(&cwd).is_err());
    }

    #[test]
    fn trash_moves_files_and_directories_in_one_call() {
        let cwd = std::env::current_dir().unwrap();
        let temp = tempfile::tempdir_in(cwd).unwrap();
        let file = temp.path().join("trash-me.txt");
        std::fs::write(&file, "bye").unwrap();
        let dir = temp.path().join("trash-dir");
        std::fs::create_dir(&dir).unwrap();
        std::fs::write(dir.join("child.txt"), "bye").unwrap();

        let result = fake_trash(json!({"paths": [
            file.display().to_string(),
            dir.display().to_string(),
        ]}))
        .unwrap();
        let data: Value = serde_json::from_str(&result).unwrap();
        assert_eq!(data["ok"], true);
        assert_eq!(data["moved"], 2);
        assert_eq!(data["failed"], 0);
        assert_eq!(data["total"], 2);
        assert!(!file.exists());
        assert!(!dir.exists());
        // 提示语整次一份,不再逐条重复——这是返回体积的大头。
        assert!(data["note"].is_string());
        assert_eq!(data["failures"].as_array().unwrap().len(), 0);
    }

    /// 一条失败不该带走整批:因为第 2 条不存在就放弃第 3 条,只会让模型再发一轮。
    #[test]
    fn trash_reports_each_failure_and_keeps_going() {
        let cwd = std::env::current_dir().unwrap();
        let temp = tempfile::tempdir_in(cwd).unwrap();
        let first = temp.path().join("a.txt");
        let last = temp.path().join("b.txt");
        std::fs::write(&first, "a").unwrap();
        std::fs::write(&last, "b").unwrap();
        let missing = temp.path().join("nope.txt");

        let result = fake_trash(json!({"paths": [
            first.display().to_string(),
            missing.display().to_string(),
            last.display().to_string(),
        ]}))
        .unwrap();
        let data: Value = serde_json::from_str(&result).unwrap();
        assert_eq!(data["moved"], 2);
        assert_eq!(data["failed"], 1);
        assert_eq!(data["ok"], true, "还有成功的就不算整体失败");
        assert!(!first.exists());
        assert!(!last.exists(), "失败项之后的路径仍要处理");
        let failures = data["failures"].as_array().unwrap();
        assert_eq!(failures.len(), 1);
        assert!(failures[0]["path"].as_str().unwrap().contains("nope.txt"));
        assert!(!failures[0]["error"].as_str().unwrap().is_empty());
    }

    #[test]
    fn trash_rejects_an_empty_or_missing_path_list() {
        assert!(fake_trash(json!({"paths": []})).is_err());
        assert!(fake_trash(json!({"paths": ["", "   "]})).is_err());
        assert!(fake_trash(json!({"path": "/tmp/x"})).is_err());
    }
}
