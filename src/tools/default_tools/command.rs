//! 执行 shell 命令。
//!
//! `CommandProcessGroup` 把子进程放进独立进程组，`Drop` 时整组杀掉——只杀直接
//! 子进程的话，它拉起的孙子进程会活下来占着终端。
//!
//! 输出边读边截（`read_command_output`），不是先收完再截：一个 `yes` 就能把内存
//! 吃光。

use crate::tools::default_tools::*;

pub(in crate::tools) const MAX_COMMAND_OUTPUT_CHARS: usize = 20_000;

pub(in crate::tools) async fn run_command(
    args: Value,
    allowed: bool,
    progress: ToolProgress,
) -> Result<String> {
    if !allowed {
        bail!("{}", "command execution is disabled; set skills.allow_command_execution=true in config.jsonc to enable run_command");
    }
    let command = required(&args, "command")?;
    if args
        .get("background")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        let title = args.get("title").and_then(Value::as_str);
        return crate::tools::jobs::spawn_background(&command, title, &progress).await;
    }
    // 下限 1:timeout_seconds=0 会立即超时,命令根本没机会执行。
    let timeout = args
        .get("timeout_seconds")
        .and_then(Value::as_u64)
        .unwrap_or(30)
        .clamp(1, 120);
    execute_command(&command, timeout, progress).await
}

pub(in crate::tools) async fn execute_command(
    command: &str,
    timeout: u64,
    progress: ToolProgress,
) -> Result<String> {
    let mut command_process = Command::new("sh");
    command_process
        .arg("-lc")
        .arg(command)
        // Explicit cwd: shell commands must run in the turn workspace, not
        // whatever the daemon process cwd happens to be.
        .current_dir(crate::tools::workspace::effective_workdir());
    // 工具桥环境(任务#12):脚本里 `miyu tool-call` 凭这些以本回合的
    // 会话身份/来源打回 daemon 执行结构化工具,内层调用照走 guard 管线。
    if let Some(session) = crate::tools::workspace::try_session() {
        command_process.env("MIYU_SESSION", &*session);
    }
    if let Ok(origin) = serde_json::to_string(&crate::tools::workspace::current_turn_origin()) {
        command_process.env("MIYU_TURN_ORIGIN", origin);
    }
    command_process.env(
        "MIYU_BRIDGE_DEPTH",
        crate::tools::workspace::current_bridge_depth().to_string(),
    );
    command_process
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(unix)]
    command_process.process_group(0);
    // 成员回合:子进程套 Landlock(策略在回合的 task-local 上;管理员没有)。
    crate::tools::sandbox::confine(&mut command_process);
    let mut child = command_process.spawn()?;
    let mut process_group = CommandProcessGroup::new(child.id());
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow::anyhow!("failed to capture command stdout"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| anyhow::anyhow!("failed to capture command stderr"))?;

    let execution = tokio::time::timeout(std::time::Duration::from_secs(timeout), async {
        tokio::join!(
            child.wait(),
            read_command_output(stdout, progress.clone(), |progress, chunk| {
                progress.report_command_output(CommandOutputStream::Stdout, chunk);
            }),
            read_command_output(stderr, progress, |progress, chunk| {
                progress.report_command_output(CommandOutputStream::Stderr, chunk);
            }),
        )
    })
    .await;

    let (status, stdout, stderr) = match execution {
        Ok((status, stdout, stderr)) => {
            process_group.disarm();
            (status?, stdout?, stderr?)
        }
        Err(elapsed) => {
            process_group.terminate();
            let _ = child.start_kill();
            let _ = child.wait().await;
            process_group.disarm();
            return Err(elapsed.into());
        }
    };
    command_output(status, stdout, stderr)
}

pub(in crate::tools) struct CommandProcessGroup {
    #[cfg(unix)]
    pub(in crate::tools) pgid: Option<i32>,
}

impl CommandProcessGroup {
    pub(in crate::tools) fn new(child_id: Option<u32>) -> Self {
        Self {
            #[cfg(unix)]
            pgid: child_id.and_then(|id| i32::try_from(id).ok()),
        }
    }

    pub(in crate::tools) fn terminate(&self) {
        #[cfg(unix)]
        if let Some(pgid) = self.pgid {
            unsafe {
                libc::kill(-pgid, libc::SIGKILL);
            }
        }
    }

    pub(in crate::tools) fn disarm(&mut self) {
        #[cfg(unix)]
        {
            self.pgid = None;
        }
    }
}

impl Drop for CommandProcessGroup {
    fn drop(&mut self) {
        self.terminate();
    }
}

/// Cumulative cap for collected command output. Beyond it the stream is
/// still drained (so the child never blocks on a full pipe) but no longer
/// buffered or forwarded — unbounded collection plus a clone per chunk
/// into the progress channel is a memory hazard on runaway commands.
pub(in crate::tools) const MAX_COMMAND_OUTPUT_BYTES: usize = 8 * 1024 * 1024;

pub(in crate::tools) async fn read_command_output(
    mut reader: impl tokio::io::AsyncRead + Unpin,
    progress: ToolProgress,
    report: impl Fn(&ToolProgress, Vec<u8>),
) -> std::io::Result<Vec<u8>> {
    let mut output = Vec::new();
    let mut truncated = false;
    let mut buffer = [0; 8192];
    loop {
        let read = reader.read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        let remaining = MAX_COMMAND_OUTPUT_BYTES.saturating_sub(output.len());
        if remaining == 0 {
            truncated = true;
            continue;
        }
        let take = read.min(remaining);
        if take < read {
            truncated = true;
        }
        let chunk = buffer[..take].to_vec();
        output.extend_from_slice(&chunk);
        report(&progress, chunk);
    }
    if truncated {
        // 截断标记进入工具返回体（模型上下文），走 agent_text 恒英文。
        output.extend_from_slice("\n[output truncated at the 8MB cap]".as_bytes());
    }
    Ok(output)
}

/// dsh 式纯文本返回体(08-17)。此前每条结果都裹一层 pretty-print JSON,
/// 里面 6 个字段在说"什么都没发生"(`stderr:""`、`*_truncated:false`、
/// `*_omitted_chars:0`、`success:true`)——实测一次 `uname -r; pwd; ls`
/// 407 字符里信封占 217(53%)。
///
/// 新形态:正文就是 stdout;有 stderr 才追加 `[stderr]` 段;完全没输出时
/// 精确输出 `(no output)`;截断和非零退出码各自只在真发生时补一行标记。
/// 退出码是通用 Unix 词汇,不额外解释。
pub(in crate::tools) fn command_text(
    status: std::process::ExitStatus,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
) -> String {
    let stdout = clip_output_with_meta(&String::from_utf8_lossy(&stdout));
    let stderr = clip_output_with_meta(&String::from_utf8_lossy(&stderr));
    let mut body = stdout.text.trim_end().to_string();
    if !stderr.text.trim().is_empty() {
        if !body.is_empty() {
            body.push('\n');
        }
        body.push_str("[stderr]\n");
        body.push_str(stderr.text.trim_end());
    }
    if body.is_empty() {
        body.push_str("(no output)");
    }
    if let Some(code) = status.code() {
        if code != 0 {
            body.push_str(&format!("\n[exit code: {code}]"));
        }
    } else {
        // 没有退出码 = 被信号杀掉;不说一声模型会把它当成功。
        body.push_str("\n[killed by signal]");
    }
    body
}

pub(in crate::tools) fn command_output(
    status: std::process::ExitStatus,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
) -> Result<String> {
    Ok(command_text(status, stdout, stderr))
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub(in crate::tools) struct ClippedOutput {
    pub(in crate::tools) text: String,
    pub(in crate::tools) truncated: bool,
    pub(in crate::tools) omitted_chars: usize,
}
