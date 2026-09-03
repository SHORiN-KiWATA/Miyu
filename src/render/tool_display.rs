//! 工具调用在终端里的呈现。
//!
//! 一行摘要要说清「在对什么做什么」，所以 `tool_subject` 按工具类型挑出最有信
//! 息量的那个参数（读文件挑路径、搜索挑关键词）。
//!
//! `redact_sensitive_inline` / `redact_bearer_token` 是必需的：工具参数里可能带
//! token 或密钥，而终端内容会被截图、会进日志。

use crate::render::*;

#[derive(Default)]
pub(crate) struct ToolStats {
    pub(crate) calls: usize,
    pub(crate) ok: usize,
    pub(crate) error: usize,
    pub(crate) subject: Option<String>,
    pub(crate) progress: Option<String>,
    pub(crate) final_progress: Option<String>,
    pub(crate) started_at: Option<std::time::Instant>,
    pub(crate) elapsed: Option<std::time::Duration>,
    /// The subagent handed itself off to the background. Its call returned at
    /// once, so the elapsed timer would only ever read `0s` — and worse, imply
    /// the work finished instantly. The job strip tracks it from here on.
    pub(crate) detached: bool,
    pub(crate) seq: usize,
}

impl ToolStats {
    pub(crate) fn elapsed(&self) -> Option<std::time::Duration> {
        self.elapsed
            .or_else(|| self.started_at.map(|started| started.elapsed()))
    }

    /// Every issued call has completed (ok or err) — nothing running.
    pub(crate) fn settled(&self) -> bool {
        self.calls > 0 && self.ok + self.error >= self.calls
    }
}

#[derive(Clone, Copy)]
pub(crate) enum SummaryStyle {
    Reasoning,
    Tool,
}

/// The still-line equivalent of a spinner style, for terminals that cannot
/// animate — so a phase keeps its identity (thinking vs tool) either way.
pub(crate) fn summary_style_for(style: SpinnerStyle) -> SummaryStyle {
    match style {
        SpinnerStyle::Scanner => SummaryStyle::Reasoning,
        SpinnerStyle::Braille => SummaryStyle::Tool,
    }
}

pub(crate) fn style_summary_text(text: &str, style: SummaryStyle) -> String {
    match style {
        SummaryStyle::Reasoning => format!("\x1b[38;5;10m{text}\x1b[0m"),
        SummaryStyle::Tool => format!("\x1b[2m{text}\x1b[0m"),
    }
}

pub(crate) fn write_activity_summary(
    writer: &mut impl Write,
    text: &str,
    style: SummaryStyle,
) -> Result<()> {
    writeln!(writer, "{}", style_summary_text(text, style))?;
    writeln!(writer)?;
    Ok(())
}

pub(crate) fn tool_status_text(name: &str, stats: &ToolStats, subagent: bool) -> String {
    let calls = stats.calls.max(stats.ok + stats.error).max(1);
    let running = stats.calls.saturating_sub(stats.ok + stats.error);
    let text = if calls == 1 && running > 0 {
        format!("{name}×1 {}", t("running", "运行中"))
    } else if calls == 1 && stats.error > 0 {
        format!("{name}×1 err")
    } else if calls == 1 && stats.ok > 0 {
        format!("{name}×1 ok")
    } else if running > 0 {
        let mut text = format!(
            "{name}×{calls} {}:{} ok:{}",
            t("running", "运行中"),
            running,
            stats.ok,
        );
        if stats.error > 0 {
            text.push_str(&format!(" err:{}", stats.error));
        }
        text
    } else if stats.error > 0 {
        format!("{name}×{calls} ok:{} err:{}", stats.ok, stats.error)
    } else {
        format!("{name}×{calls} ok:{}", stats.ok)
    };
    if subagent && !stats.detached {
        if let Some(elapsed) = stats.elapsed() {
            return format!("{text} · {}", format_elapsed(elapsed));
        }
    }
    text
}

pub(crate) fn tool_result_status(status: &str, elapsed: Option<std::time::Duration>) -> String {
    elapsed.map_or_else(
        || status.to_string(),
        |elapsed| format!("{status} · {}", format_elapsed(elapsed)),
    )
}

pub(crate) fn format_elapsed(elapsed: std::time::Duration) -> String {
    let seconds = elapsed.as_secs();
    if seconds < 60 {
        format!("{seconds}s")
    } else if seconds < 3_600 {
        format!("{}m {:02}s", seconds / 60, seconds % 60)
    } else {
        format!("{}h {:02}m", seconds / 3_600, (seconds % 3_600) / 60)
    }
}

pub(crate) fn format_reasoning_elapsed(elapsed: std::time::Duration) -> String {
    if elapsed < std::time::Duration::from_millis(1) {
        "<1ms".to_string()
    } else if elapsed < std::time::Duration::from_secs(1) {
        format!("{}ms", elapsed.as_millis())
    } else if elapsed < std::time::Duration::from_secs(60) {
        format!("{:.1}s", elapsed.as_secs_f64())
    } else if elapsed < std::time::Duration::from_secs(3_600) {
        format!("{}m {:02}s", elapsed.as_secs() / 60, elapsed.as_secs() % 60)
    } else {
        format!(
            "{}h {:02}m",
            elapsed.as_secs() / 3_600,
            (elapsed.as_secs() % 3_600) / 60
        )
    }
}

/// 输出不该打印的工具:它们自己已经把内容送到终端上了。
///
/// `use_meme:show` 带 action 后缀(见 `agent::reports::tool_event_name`)——
/// `use_meme` 里只有 show 是静默的,search 要照常显示摘要。
pub(crate) fn is_silent_tool(name: &str) -> bool {
    matches!(name, "use_meme:show" | "ask_question")
}

pub(crate) fn is_subagent_tool(name: &str) -> bool {
    let name = tool_event_base_name(name);
    matches!(name, "deep_research" | "task")
}

pub(crate) fn tool_event_base_name(name: &str) -> &str {
    if name.starts_with("divine:") {
        "divine"
    } else if name.starts_with("use_meme:") {
        "use_meme"
    } else if name.starts_with("load_skill:") {
        "load_skill"
    } else if name.starts_with("load_tools:") {
        "load_tools"
    } else if name.starts_with("task:") {
        "task"
    } else {
        name
    }
}

pub(crate) fn inline_tool_subject(name: &str) -> bool {
    // 回收站的 subject 是条数,贴在标题上比单占一行更紧凑,
    // 而成功时整个块本来就只有这一行。
    matches!(tool_event_base_name(name), "load_tools" | "trash_path")
}

pub(crate) fn tool_subject(name: &str, arguments: &str) -> Option<String> {
    let args = serde_json::from_str::<Value>(arguments).ok()?;
    let name = tool_event_base_name(name);
    let value = match name {
        // —— claude 原生工具(中转侧闭环执行,REPL 摘要行同样要有 ↳ 主题) ——
        "Bash" => {
            let command = string_arg(&args, &["command"])?;
            Some(
                if args.get("run_in_background").and_then(Value::as_bool) == Some(true) {
                    format!("[后台] {command}")
                } else {
                    command
                },
            )
        }
        "Read" | "Edit" | "Write" | "NotebookEdit" => {
            string_arg(&args, &["file_path", "path", "notebook_path"])
        }
        "WebFetch" => string_arg(&args, &["url"]).and_then(|url| safe_url_subject(&url)),
        "WebSearch" | "ToolSearch" => string_arg(&args, &["query"]),
        "Task" | "Agent" => string_arg(&args, &["description"]),
        "SlashCommand" => string_arg(&args, &["command"]),
        // —— agy 原生工具(antigravity 中转;入参键已在流层归一成 Miyu 的) ——
        "view_file" | "write_to_file" | "replace_file_content" | "list_dir" => {
            string_arg(&args, &["path"])
        }
        "find_by_name" | "grep_search" => {
            let needle = string_arg(&args, &["pattern", "query"])?;
            Some(match string_arg(&args, &["path"]) {
                Some(path) => format!("{needle} · {path}"),
                None => needle,
            })
        }
        "read_url_content" => string_arg(&args, &["url"]).and_then(|url| safe_url_subject(&url)),
        "search_web" => string_arg(&args, &["query"]),
        "call_mcp_tool" => string_arg(&args, &["ToolName"]),
        "task" => string_arg(&args, &["description"]),
        "web_search"
        | "search_web_images"
        | "use_meme"
        | "search_knowledge_base"
        | "search_evicted_context"
        | "recall_memories"
        | "aur"
        | "online_man"
        | "game_compat"
        | "fcitx5_input_method_wiki_qurey" => string_arg(&args, &["query", "topic"]),
        "archwiki_query" | "query_moegirl" => string_arg(&args, &["title", "query"]),
        "read" | "read_file" => {
            let path = string_arg(&args, &["path"])?;
            Some(match read_page_label(&args) {
                Some(page) => format!("{path} ({page})"),
                None => path,
            })
        }
        // 补丁三件套:从补丁头里抠文件名当副标题,不然工具行只剩一个名字,
        // 用户分不清这回编辑的是什么(08-22 验收反馈)。
        "edit" | "kb" | "artifact" | "apply_patch" | "apply_artifact_patch" => {
            let patch = string_arg(&args, &["patchText", "patch_text"])?;
            let files: Vec<String> = patch
                .lines()
                .filter_map(|line| {
                    line.strip_prefix("*** Add File: ")
                        .or_else(|| line.strip_prefix("*** Update File: "))
                        .or_else(|| line.strip_prefix("*** Delete File: "))
                })
                .map(|name| name.trim().to_string())
                .collect();
            match files.as_slice() {
                [] => None,
                [only] => Some(only.clone()),
                [first, ..] => Some(format!("{first} +{} {}", files.len() - 1, t("more", "项"))),
            }
        }
        "write_file" | "edit_file" | "edit_string" | "manage_script" => {
            string_arg(&args, &["path"])
        }
        "trash_path" => {
            let paths = args.get("paths").and_then(Value::as_array)?;
            match paths.len() {
                0 => None,
                // 只删一个时报路径更有用;成堆删时路径无信息量,报条数。
                1 => paths[0].as_str().map(str::to_string),
                count => Some(format!("{count} {}", t("items", "项"))),
            }
        }
        "run_command" => {
            let command = string_arg(&args, &["command"])?;
            Some(
                if args.get("background").and_then(Value::as_bool) == Some(true) {
                    format!("[后台] {command}")
                } else {
                    command
                },
            )
        }
        "read_knowledge_base_file" | "edit_knowledge_base_file" | "remove_knowledge_base_file" => {
            string_arg(&args, &["file_name"])
        }
        "glob" | "grep" | "Glob" | "Grep" => {
            let pattern = string_arg(&args, &["pattern"]);
            let path = string_arg(&args, &["path"]);
            match (pattern, path) {
                (Some(pattern), Some(path)) if !path.trim().is_empty() => {
                    Some(format!("{pattern} · {path}"))
                }
                (pattern, _) => pattern,
            }
        }
        "web_fetch" => string_arg(&args, &["url"]).and_then(|url| safe_url_subject(&url)),
        "load_skill" => string_arg(&args, &["name"]),
        "manage_skill" => string_arg(&args, &["name", "draft_id"]),
        "load_tools" => args.get("names").and_then(Value::as_array).map(|names| {
            names
                .iter()
                .filter_map(Value::as_str)
                .map(|name| {
                    let display = readable_tool_name(&format!("load_tools:{name}"));
                    display
                        .split_once('：')
                        .or_else(|| display.split_once(": "))
                        .map(|(_, target)| target.to_string())
                        .unwrap_or(display)
                })
                .collect::<Vec<_>>()
                .join(t(", ", "、"))
        }),
        "deep_research" => string_arg(&args, &["topic"]),
        "check_issue" => string_arg(&args, &["target", "area", "issue", "symptom"]),
        "get_weather" => string_arg(&args, &["location"])
            .or_else(|| Some(t("missing location", "缺少地点").to_string())),
        "get_exchange_rate" => {
            let base = string_arg(&args, &["base"])?;
            let target = string_arg(&args, &["target"])?;
            Some(format!(
                "{} → {}",
                base.to_uppercase(),
                target.to_uppercase()
            ))
        }
        "scientific_calculator" => string_arg(&args, &["expression", "operation"]),
        "alarm" => string_arg(&args, &["label", "time", "id"]),
        "archlinux_official_package_query" | "review_aur_package" | "install_aur_package" => {
            string_arg(&args, &["package_name", "package"])
        }
        "vision_analyze" | "print_image" | "manage_meme" => {
            string_arg(&args, &["image"]).map(|image| image_basename(&image))
        }
        "generate_image" => string_arg(&args, &["prompt"]),
        "upload_text_to_knowledge_base" => string_arg(&args, &["file_name", "title"]),
        "register_deep_research_topic_title" => string_arg(&args, &["topic_title"]),
        "register_deep_research_reference" => string_arg(&args, &["title"]),
        "remove_deep_research_reference" => string_arg(&args, &["ref"]),
        _ => None,
    }?;
    safe_inline_subject(&value)
}

/// Page label for a read_file call: `L<start>-<end>` when the range is
/// bounded, `L<start>+` for an open tail. `None` for a plain full read so
/// the common case stays a bare path.
pub(crate) fn read_page_label(args: &Value) -> Option<String> {
    let offset = args.get("offset").and_then(Value::as_u64);
    let limit = args.get("limit").and_then(Value::as_u64);
    let start = offset.unwrap_or(1).max(1);
    match (offset, limit) {
        (None, None) => None,
        (_, Some(limit)) => Some(format!(
            "L{start}-{}",
            start.saturating_add(limit.saturating_sub(1))
        )),
        (Some(_), None) => Some(format!("L{start}+")),
    }
}

pub(crate) fn string_arg(args: &Value, keys: &[&str]) -> Option<String> {
    keys.iter()
        .find_map(|key| args.get(*key).and_then(Value::as_str))
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

pub(crate) fn safe_inline_subject(value: &str) -> Option<String> {
    let value = truncate_inline_input(&sanitize_terminal_text(value), 256);
    let value = clip_progress_line(&value, 256);
    let value = redact_sensitive_inline(&value);
    let value = clip_progress_line(&value, 80);
    (!value.is_empty()).then_some(value)
}

pub(crate) fn truncate_inline_input(value: &str, max_chars: usize) -> String {
    value.chars().take(max_chars).collect()
}

pub(crate) fn redact_sensitive_inline(value: &str) -> String {
    const KEYS: &[&str] = &[
        "secret_access_key",
        "secret-access-key",
        "access_key_id",
        "access-key-id",
        "api_key",
        "api-key",
        "apikey",
        "token",
        "password",
        "passwd",
        "secret",
        "authorization",
        "cookie",
        "credential",
        "private_key",
        "private-key",
    ];
    let mut output = value.to_string();
    for key in KEYS {
        let mut from = 0usize;
        loop {
            let lower = output.to_ascii_lowercase();
            let Some(relative) = lower[from..].find(key) else {
                break;
            };
            let key_start = from + relative;
            let key_end = key_start + key.len();
            let boundary_ok =
                key_start == 0 || !lower.as_bytes()[key_start - 1].is_ascii_alphanumeric();
            let mut separator = key_end;
            if matches!(lower.as_bytes().get(separator), Some(b'\'' | b'"')) {
                separator += 1;
            }
            let mut had_space = false;
            while lower.as_bytes().get(separator) == Some(&b' ') {
                had_space = true;
                separator += 1;
            }
            let flag_prefix = &lower[..key_start];
            let single_dash_flag = flag_prefix.ends_with('-')
                && (key_start == 1 || lower.as_bytes()[key_start - 2].is_ascii_whitespace());
            let flag_space = had_space && (flag_prefix.ends_with("--") || single_dash_flag);
            let space_delimited = had_space
                && (matches!(*key, "authorization" | "password" | "passwd") || flag_space);
            if !boundary_ok
                || (!space_delimited
                    && !matches!(lower.as_bytes().get(separator), Some(b'=' | b':')))
            {
                from = key_end;
                continue;
            }
            let mut value_start = separator + usize::from(!space_delimited);
            while lower.as_bytes().get(value_start) == Some(&b' ') {
                value_start += 1;
            }
            let quote = lower
                .as_bytes()
                .get(value_start)
                .copied()
                .filter(|value| matches!(value, b'\'' | b'"'));
            value_start += usize::from(quote.is_some());
            let value_end = quote
                .and_then(|quote| {
                    lower.as_bytes()[value_start..]
                        .iter()
                        .position(|value| *value == quote)
                        .map(|end| value_start + end)
                })
                .or_else(|| {
                    flag_space.then(|| {
                        lower.as_bytes()[value_start..]
                            .iter()
                            .position(|byte| byte.is_ascii_whitespace())
                            .map(|end| value_start + end)
                            .unwrap_or(output.len())
                    })
                })
                .or_else(|| {
                    lower[value_start..]
                        .find(['&', ',', ';'])
                        .map(|end| value_start + end)
                })
                .unwrap_or(output.len());
            output.replace_range(value_start..value_end, "[redacted]");
            from = value_start + "[redacted]".len();
        }
    }
    redact_bearer_token(output)
}

pub(crate) fn redact_bearer_token(mut output: String) -> String {
    let mut from = 0usize;
    loop {
        let lower = output.to_ascii_lowercase();
        let Some(relative) = lower[from..].find("bearer") else {
            break;
        };
        let start = from + relative;
        let end = start + "bearer".len();
        let boundary_ok = start == 0 || !lower.as_bytes()[start - 1].is_ascii_alphanumeric();
        let mut value_start = end;
        while lower.as_bytes().get(value_start) == Some(&b' ') {
            value_start += 1;
        }
        if !boundary_ok || value_start == end || value_start == output.len() {
            from = end;
            continue;
        }
        let value_end = lower.as_bytes()[value_start..]
            .iter()
            .position(|byte| byte.is_ascii_whitespace() || matches!(*byte, b',' | b';' | b'&'))
            .map(|relative| value_start + relative)
            .unwrap_or(output.len());
        output.replace_range(value_start..value_end, "[redacted]");
        from = value_start + "[redacted]".len();
    }
    output
}

pub(crate) fn safe_url_subject(value: &str) -> Option<String> {
    let mut url = reqwest::Url::parse(value).ok()?;
    let _ = url.set_username("");
    let _ = url.set_password(None);
    url.set_query(None);
    url.set_fragment(None);
    Some(url.to_string())
}

pub(crate) fn image_basename(value: &str) -> String {
    if let Some(url) = safe_url_subject(value) {
        return url;
    }
    std::path::Path::new(value)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(value)
        .to_string()
}

pub(crate) fn readable_tool_name(name: &str) -> String {
    crate::tools::readable_tool_name(name)
}
