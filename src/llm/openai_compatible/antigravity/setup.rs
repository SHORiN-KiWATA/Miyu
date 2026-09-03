//! agy 侧落盘物:人格代理文件与全局 MCP 桥注册,以及清空会话时的转录删除。
//!
//! 三条放置方案实测过(评审 §1.3):写进会话工作区 `.agents/` 太侵入;Miyu
//! 私有目录 + 双 `--add-dir` 会让原生 run_command 的 cwd 在两目录间随机;
//! 只剩全局目录这条路,副作用是用户的 /agents 面板多一条 `miyu`、交互式 agy
//! 也挂上 miyu 服务器(守卫 env 让它在没有会话时只应答空工具表)。

use super::{AGENT_NAME, MCP_SERVER_NAME, NATIVE_TOOLS};
use crate::llm::openai_compatible::*;
use std::path::Path;

/// `~/.gemini/config`;`MIYU_AGY_CONFIG_DIR` 改道(测试/隔离环境)。
pub(super) fn default_config_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("MIYU_AGY_CONFIG_DIR") {
        return PathBuf::from(dir);
    }
    let home = std::env::var_os("HOME").unwrap_or_default();
    Path::new(&home).join(".gemini").join("config")
}

/// agy 的数据目录(`~/.gemini/antigravity-cli`):会话转录落这里。
fn data_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("MIYU_AGY_DATA_DIR") {
        return Some(PathBuf::from(dir));
    }
    let home = std::env::var_os("HOME")?;
    Some(Path::new(&home).join(".gemini").join("antigravity-cli"))
}

/// 代理文件全文:frontmatter(`tools:` 白名单按原生开关给)+ 提示词正文。
pub(super) fn render_agent_file(prompt: &str, native_on: bool) -> String {
    let mut text = String::new();
    text.push_str("---\n");
    text.push_str(&format!("name: {AGENT_NAME}\n"));
    text.push_str("description: Miyu relay persona (managed by Miyu; rewritten whenever the prompt changes)\n");
    text.push_str("mainAgent: true\n");
    text.push_str("subagent: false\n");
    if native_on {
        text.push_str("tools:\n");
        for tool in NATIVE_TOOLS {
            text.push_str(&format!("  - {tool}\n"));
        }
    } else {
        text.push_str("tools: []\n");
    }
    text.push_str("---\n\n");
    text.push_str(prompt.trim_end());
    text.push('\n');
    text
}

/// 代理文件路径:`<config>/agents/miyu/agent.md`。
pub(super) fn agent_file_path(config_dir: &Path) -> PathBuf {
    config_dir.join("agents").join(AGENT_NAME).join("agent.md")
}

/// 内容不变就不写:agy 每个进程都重读,写盘只在提示词/工具开关变化时发生。
pub(super) fn ensure_agent_file(config_dir: &Path, prompt: &str, native_on: bool) -> Result<()> {
    let path = agent_file_path(config_dir);
    let wanted = render_agent_file(prompt, native_on);
    if std::fs::read_to_string(&path).ok().as_deref() == Some(wanted.as_str()) {
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    write_atomically(&path, &wanted)
}

pub(super) fn mcp_config_path(config_dir: &Path) -> PathBuf {
    config_dir.join("mcp_config.json")
}

/// 桥条目应有的样子。`tools` 按名字逐条 eager(agy 不认通配符);
/// `MIYU_MCP_REQUIRE_SESSION` 是给用户自己交互式开 agy 时的守卫——那时没有
/// `MIYU_SESSION`,mcp-serve 只应答空工具表,不会降级成无作用域直连。
pub(super) fn bridge_entry(eager_tools: &[String]) -> Result<Value> {
    let exe = crate::paths::miyu_executable().context("locating the Miyu executable")?;
    let mut tools = Map::new();
    for name in eager_tools {
        tools.insert(name.clone(), json!({ "eager": true }));
    }
    Ok(json!({
        "command": exe.display().to_string(),
        "args": ["mcp-serve"],
        "env": { "MIYU_MCP_REQUIRE_SESSION": "1" },
        "timeoutSeconds": 1800,
        "tools": tools,
    }))
}

/// 只动 `mcpServers.miyu` 这一个键,用户自己的其它服务器与未知字段原样保留。
pub(super) fn ensure_mcp_entry(config_dir: &Path, eager_tools: &[String]) -> Result<()> {
    let path = mcp_config_path(config_dir);
    let mut root: Value = match std::fs::read_to_string(&path) {
        Ok(text) if !text.trim().is_empty() => {
            serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))?
        }
        _ => json!({}),
    };
    if !root.is_object() {
        bail!("{} is not a JSON object", path.display());
    }
    let wanted = bridge_entry(eager_tools)?;
    let servers = root
        .as_object_mut()
        .expect("checked above")
        .entry("mcpServers")
        .or_insert_with(|| json!({}));
    if !servers.is_object() {
        bail!("{}: mcpServers is not a JSON object", path.display());
    }
    let servers = servers.as_object_mut().expect("checked above");
    if servers.get(MCP_SERVER_NAME) == Some(&wanted) {
        return Ok(());
    }
    servers.insert(MCP_SERVER_NAME.to_string(), wanted);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let text = serde_json::to_string_pretty(&root)? + "\n";
    write_atomically(&path, &text)
}

/// 供应商禁用时的清理:删代理目录,摘掉桥条目(文件里别的东西不动)。
pub(crate) fn remove_relay_files(config_dir: &Path) {
    let agent_dir = config_dir.join("agents").join(AGENT_NAME);
    if agent_dir.exists() {
        if let Err(error) = std::fs::remove_dir_all(&agent_dir) {
            tracing::warn!(%error, path = %agent_dir.display(), "failed to remove the agy persona agent");
        }
    }
    let path = mcp_config_path(config_dir);
    let Ok(text) = std::fs::read_to_string(&path) else {
        return;
    };
    let Ok(mut root) = serde_json::from_str::<Value>(&text) else {
        return;
    };
    let removed = root
        .get_mut("mcpServers")
        .and_then(Value::as_object_mut)
        .map(|servers| servers.remove(MCP_SERVER_NAME).is_some())
        .unwrap_or(false);
    if removed {
        if let Ok(text) = serde_json::to_string_pretty(&root) {
            let _ = write_atomically(&path, &(text + "\n"));
        }
    }
}

/// 清空 Miyu 会话 / 辅助请求用完的联动:尽力删 agy 侧转录。两处:
/// `conversations/<id>.db{,-shm,-wal}` 与 `brain/<id>/`。后者不删的话,旧转录
/// 会躺在磁盘上,模型缺上下文时会拿全开的原生工具把它翻出来(09-03 实测 28
/// 次调用考古出旧暗号)。
pub(in crate::llm::openai_compatible) fn remove_conversation_files(conversation_id: &str) {
    // id 是 UUID:拒绝任何带路径分隔符/点的输入,免得删错地方。
    if conversation_id.is_empty()
        || !conversation_id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-')
    {
        return;
    }
    let Some(data) = data_dir() else {
        return;
    };
    let mut touched = false;
    for suffix in [".db", ".db-shm", ".db-wal"] {
        let file = data
            .join("conversations")
            .join(format!("{conversation_id}{suffix}"));
        if file.exists() {
            touched |= std::fs::remove_file(&file).is_ok();
        }
    }
    let brain = data.join("brain").join(conversation_id);
    if brain.is_dir() {
        touched |= std::fs::remove_dir_all(&brain).is_ok();
    }
    if touched {
        tracing::info!(conversation_id, "removed the agy-side conversation files");
    }
}

fn write_atomically(path: &Path, text: &str) -> Result<()> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, text).with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("renaming into {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_file_lists_native_allowlist_or_empties_it() {
        let on = render_agent_file("persona", true);
        assert!(on.starts_with("---\nname: miyu\n"));
        assert!(on.contains("tools:\n  - run_command\n"));
        assert!(!on.contains("ask_question"), "原生问答不进白名单");
        assert!(on.ends_with("---\n\npersona\n"));
        let off = render_agent_file("persona", false);
        assert!(off.contains("tools: []\n"));
    }

    #[test]
    fn ensure_agent_file_writes_only_on_change() {
        let dir = tempfile::tempdir().unwrap();
        ensure_agent_file(dir.path(), "one", true).unwrap();
        let path = agent_file_path(dir.path());
        let first = std::fs::metadata(&path).unwrap().modified().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        ensure_agent_file(dir.path(), "one", true).unwrap();
        assert_eq!(std::fs::metadata(&path).unwrap().modified().unwrap(), first);
        ensure_agent_file(dir.path(), "two", true).unwrap();
        assert!(std::fs::read_to_string(&path).unwrap().ends_with("two\n"));
    }

    #[test]
    fn mcp_entry_preserves_other_servers_and_unknown_fields() {
        let dir = tempfile::tempdir().unwrap();
        let path = mcp_config_path(dir.path());
        std::fs::write(
            &path,
            r#"{"mcpServers":{"other":{"command":"x","weird":1}},"topLevelUnknown":true}"#,
        )
        .unwrap();
        ensure_mcp_entry(dir.path(), &["use_meme".to_string()]).unwrap();
        let root: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(root["topLevelUnknown"], true);
        assert_eq!(root["mcpServers"]["other"]["weird"], 1);
        let miyu = &root["mcpServers"]["miyu"];
        assert_eq!(miyu["args"], json!(["mcp-serve"]));
        assert_eq!(miyu["env"]["MIYU_MCP_REQUIRE_SESSION"], "1");
        assert_eq!(miyu["timeoutSeconds"], 1800);
        assert_eq!(miyu["tools"]["use_meme"]["eager"], true);
        // 摘掉桥条目后别的服务器还在。
        remove_relay_files(dir.path());
        let root: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert!(root["mcpServers"].get("miyu").is_none());
        assert_eq!(root["mcpServers"]["other"]["command"], "x");
    }

    #[test]
    fn conversation_cleanup_refuses_path_like_ids() {
        // 不崩、不删:id 校验先于任何文件操作。
        remove_conversation_files("../etc");
        remove_conversation_files("");
    }
}
