//! 工具产物的识别与发布。
//!
//! 判定「这次工具调用产生了值得发给用户的文件」是启发式的，所以**取保守**：
//! `artifact_candidates_only_include_new_files` 那条测试守的就是这个——宁可漏
//! 发也不要把工具读过的无关文件当成产物推给用户。

use crate::agent::*;

pub(crate) fn tool_output_succeeded(output: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(output)
        .ok()
        .and_then(|value| {
            value
                .get("success")
                .and_then(serde_json::Value::as_bool)
                .or_else(|| value.get("ok").and_then(serde_json::Value::as_bool))
        })
        .unwrap_or(true)
}

#[derive(Debug)]
pub(in crate::agent) struct AutoArtifactCandidate {
    pub(in crate::agent) call_id: String,
    pub(in crate::agent) tool_name: String,
    pub(in crate::agent) path: PathBuf,
}

pub(in crate::agent) fn artifact_delivery_requested(messages: &[ChatMessage]) -> bool {
    let text = messages
        .iter()
        .rev()
        .find(|message| message.role == "user")
        .and_then(chat_message_text)
        .unwrap_or_default()
        .to_lowercase();
    let zh_action = ["生成", "创建", "制作", "导出", "保存为", "写一", "写个"]
        .iter()
        .any(|word| text.contains(word));
    let zh_deliverable = [
        "报告",
        "文档",
        "文件",
        "网页",
        "页面",
        "表格",
        "清单",
        "markdown",
        "md",
        "html",
        "json",
        "csv",
        "pdf",
        "代码文件",
        "独立脚本",
        "示例程序",
    ]
    .iter()
    .any(|word| text.contains(word));
    let en_action = ["create", "generate", "write", "make", "export", "save"]
        .iter()
        .any(|word| text.split_whitespace().any(|part| part == *word));
    let en_deliverable = [
        "report",
        "document",
        "file",
        "webpage",
        "page",
        "table",
        "spreadsheet",
        "markdown",
        "html",
        "json",
        "csv",
        "pdf",
        "script",
        "standalone program",
    ]
    .iter()
    .any(|word| text.contains(word));
    (zh_action && zh_deliverable) || (en_action && en_deliverable)
}

pub(in crate::agent) fn chat_message_text(message: &ChatMessage) -> Option<String> {
    match message.content.as_ref()? {
        ChatContent::Text(text) => Some(text.clone()),
        ChatContent::Parts(parts) => Some(
            parts
                .iter()
                .filter_map(|part| match part {
                    ChatContentPart::Text { text } => Some(text.as_str()),
                    ChatContentPart::ImageUrl { .. }
                    | ChatContentPart::VideoUrl { .. }
                    | ChatContentPart::File { .. } => None,
                })
                .collect::<Vec<_>>()
                .join("\n"),
        ),
    }
}

pub(in crate::agent) fn artifact_candidate_paths(tool_name: &str, output: &str) -> Vec<PathBuf> {
    let Ok(payload) = serde_json::from_str::<Value>(output) else {
        return Vec::new();
    };
    let raw_paths = match tool_name {
        "write_file" if payload.get("created").and_then(Value::as_bool) == Some(true) => payload
            .get("path")
            .and_then(Value::as_str)
            .into_iter()
            .collect::<Vec<_>>(),
        // "edit" 是 apply_patch 的统一化新名(08-21);artifact:/kb: 命名空间
        // 的输出路径带前缀,过不了下面的扩展名/存在性检查,天然不会误发布。
        "edit" | "apply_patch" => payload
            .get("files")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter(|file| file.get("operation").and_then(Value::as_str) == Some("add"))
            .filter_map(|file| file.get("path").and_then(Value::as_str))
            .collect::<Vec<_>>(),
        _ => Vec::new(),
    };
    raw_paths
        .into_iter()
        .map(resolve_tool_output_path)
        .filter(|path| artifact_candidate_extension(path))
        .collect()
}

pub(in crate::agent) fn resolve_tool_output_path(path: &str) -> PathBuf {
    if let Some(rest) = path.strip_prefix("~/") {
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home).join(rest);
        }
    }
    let path = PathBuf::from(path);
    if path.is_absolute() {
        path
    } else {
        tools::workspace::effective_workdir().join(path)
    }
}

pub(in crate::agent) fn artifact_candidate_extension(path: &std::path::Path) -> bool {
    let extension = path
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    matches!(
        extension.as_str(),
        "md" | "markdown"
            | "html"
            | "htm"
            | "pdf"
            | "json"
            | "jsonl"
            | "csv"
            | "tsv"
            | "txt"
            | "log"
            | "css"
            | "js"
            | "jsx"
            | "ts"
            | "tsx"
            | "c"
            | "h"
            | "cpp"
            | "hpp"
            | "rs"
            | "py"
            | "sh"
            | "toml"
            | "yaml"
            | "yml"
            | "xml"
            | "sql"
    )
}

pub(in crate::agent) fn publish_auto_artifact_candidates<F>(
    candidates: &[AutoArtifactCandidate],
    on_event: &mut F,
) -> Result<()>
where
    F: FnMut(AgentEvent) -> Result<()>,
{
    let mut published = HashSet::new();
    for candidate in candidates {
        let key = candidate
            .path
            .canonicalize()
            .unwrap_or_else(|_| candidate.path.clone());
        if !published.insert(key) || !candidate.path.is_file() {
            continue;
        }
        on_event(AgentEvent::Artifact {
            call_id: candidate.call_id.clone(),
            name: candidate.tool_name.clone(),
            path: candidate.path.clone(),
            title: String::new(),
        })?;
    }
    Ok(())
}
