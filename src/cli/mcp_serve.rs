//! `miyu mcp-serve`:把本会话的工具注册表以 MCP stdio server 形态挂给外部
//! agent(claude-code 供应商中转的工具桥,由 claude 作为子进程拉起)。
//!
//! 与 `miyu tool-call` 同源:daemon 存活时经 IPC 以 MIYU_SESSION 的会话身份
//! 解析目录并执行(guard/超时管线齐备);daemon 不在(直连调试形态)则本地
//! 建 registry 兜底。传输是 MCP stdio(JSON-RPC 2.0 行分隔),只实现 tools
//! 能力;工具失败按 MCP 语义回 `isError` 结果而不是 JSON-RPC error,让上游
//! 模型能读到错误文本自行调整。

use crate::cli::*;

pub(in crate::cli) async fn run_mcp_serve(paths: &MiyuPaths) -> Result<()> {
    let session = std::env::var("MIYU_SESSION").ok().filter(|s| !s.is_empty());
    // antigravity 线的全局 MCP 注册对用户自己交互式开的 agy 同样生效——那时
    // 没有会话身份;守卫在场就只应答空工具表,不降级成无作用域直连。
    let require_session = std::env::var("MIYU_MCP_REQUIRE_SESSION")
        .map(|value| value == "1")
        .unwrap_or(false);
    let guarded = require_session && session.is_none();
    // 上游模型方言:antigravity 中转点名 gemini,桥吐的 schema 按它整形。
    let dialect = std::env::var("MIYU_MCP_SCHEMA_DIALECT").unwrap_or_default();
    // 与 claude 原生重复的工具由拉起方经 env 点名剔除(原生优先):目录里
    // 不出现、调用被拒,两边同源。
    let excluded: std::collections::HashSet<String> = std::env::var("MIYU_MCP_EXCLUDE")
        .unwrap_or_default()
        .split(',')
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(str::to_string)
        .collect();
    let origin = std::env::var("MIYU_TURN_ORIGIN")
        .ok()
        .filter(|s| !s.is_empty());
    let depth: u32 = std::env::var("MIYU_BRIDGE_DEPTH")
        .ok()
        .and_then(|raw| raw.parse().ok())
        .unwrap_or(0);
    use tokio::io::AsyncBufReadExt as _;
    let mut lines = tokio::io::BufReader::new(tokio::io::stdin()).lines();
    while let Some(line) = lines.next_line().await? {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(request) = serde_json::from_str::<serde_json::Value>(trimmed) else {
            continue;
        };
        let method = request
            .get("method")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_string();
        // 通知(initialized 等)没有 id,协议规定不应答。
        let Some(id) = request.get("id").cloned().filter(|id| !id.is_null()) else {
            continue;
        };
        let params = request
            .get("params")
            .cloned()
            .unwrap_or(serde_json::Value::Null);
        let response = match method.as_str() {
            "tools/list" if guarded => {
                serde_json::json!({ "jsonrpc": "2.0", "id": id, "result": { "tools": [] } })
            }
            "tools/call" if guarded => serde_json::json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "content": [{
                        "type": "text",
                        "text": "no Miyu session is attached to this MCP server (started outside a Miyu relay turn)"
                    }],
                    "isError": true
                }
            }),
            "initialize" | "ping" | "tools/list" | "tools/call" => {
                match handle_request(
                    paths, &session, &origin, depth, &excluded, &dialect, &method, &params,
                )
                .await
                {
                    Ok(result) => {
                        serde_json::json!({ "jsonrpc": "2.0", "id": id, "result": result })
                    }
                    Err(error) => serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "error": { "code": -32603, "message": format!("{error:#}") }
                    }),
                }
            }
            other => serde_json::json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": { "code": -32601, "message": format!("method not found: {other}") }
            }),
        };
        write_line(&response)?;
    }
    Ok(())
}

fn write_line(value: &serde_json::Value) -> Result<()> {
    use std::io::Write as _;
    let stdout = io::stdout();
    let mut out = stdout.lock();
    let mut serialized = value.to_string();
    serialized.push('\n');
    out.write_all(serialized.as_bytes())?;
    out.flush()?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn handle_request(
    paths: &MiyuPaths,
    session: &Option<String>,
    origin: &Option<String>,
    depth: u32,
    excluded: &std::collections::HashSet<String>,
    dialect: &str,
    method: &str,
    params: &serde_json::Value,
) -> Result<serde_json::Value> {
    match method {
        "initialize" => {
            // 协议版本回显客户端的请求值:本服务只用 tools 基本面,各版本同形。
            let version = params
                .get("protocolVersion")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("2024-11-05");
            Ok(serde_json::json!({
                "protocolVersion": version,
                "capabilities": { "tools": {} },
                "serverInfo": { "name": "miyu", "version": env!("CARGO_PKG_VERSION") },
            }))
        }
        "ping" => Ok(serde_json::json!({})),
        "tools/list" => {
            let mut listing = list_tools(paths, session, dialect).await?;
            if let Some(tools) = listing
                .get_mut("tools")
                .and_then(serde_json::Value::as_array_mut)
            {
                tools.retain(|tool| {
                    tool.get("name")
                        .and_then(serde_json::Value::as_str)
                        .map(|name| !excluded.contains(name))
                        .unwrap_or(true)
                });
            }
            Ok(listing)
        }
        "tools/call" => {
            let name = params
                .get("name")
                .and_then(serde_json::Value::as_str)
                .context("tools/call requires a tool name")?;
            if excluded.contains(name) {
                return Ok(serde_json::json!({
                    "content": [{ "type": "text", "text": format!(
                        "tool {name} is not exposed here; use your own native equivalent instead"
                    ) }],
                    "isError": true,
                }));
            }
            let arguments = params
                .get("arguments")
                .cloned()
                .unwrap_or_else(|| serde_json::json!({}));
            let arguments = serde_json::to_string(&arguments)?;
            // 工具失败回 isError 结果而不是协议错误:模型要能读到错误文本。
            match call_tool(paths, session, origin, depth, name, &arguments).await {
                Ok(output) => Ok(serde_json::json!({
                    "content": [{ "type": "text", "text": output }],
                    "isError": false,
                })),
                Err(error) => Ok(serde_json::json!({
                    "content": [{ "type": "text", "text": format!("{error:#}") }],
                    "isError": true,
                })),
            }
        }
        other => bail!("method not found: {other}"),
    }
}

async fn list_tools(
    paths: &MiyuPaths,
    session: &Option<String>,
    dialect: &str,
) -> Result<serde_json::Value> {
    if ipc::daemon_info(paths).await.is_some() {
        let (_, data) = send_ipc_admin(
            paths,
            IpcCommand::ToolCatalog {
                session: session.clone(),
                name: None,
                full: true,
            },
        )
        .await?;
        let tools = data
            .get("tools")
            .and_then(serde_json::Value::as_array)
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .map(|tool| {
                let name = tool
                    .get("name")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                let description = tool
                    .get("description")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                let schema = tool
                    .get("parameters")
                    .cloned()
                    .filter(|schema| schema.is_object())
                    .unwrap_or_else(|| serde_json::json!({ "type": "object" }));
                let schema = super::mcp_schema::shape_for_dialect(schema, dialect);
                serde_json::json!({
                    "name": name,
                    "description": description,
                    "inputSchema": schema,
                })
            })
            .collect::<Vec<_>>();
        return Ok(serde_json::json!({ "tools": tools }));
    }
    // 直连回退:与 tool-call 的回退同一构建方式(模式取 MIYU_TURN_MODE)。
    let config = AppConfig::load_or_default(paths)?;
    let mode = if std::env::var("MIYU_TURN_MODE").unwrap_or_default() == "dev" {
        AgentMode::Dev
    } else {
        AgentMode::Normal
    };
    let registry = build_tool_registry(&config, paths, mode, false)?;
    let mut names = registry.tool_names();
    names.sort();
    let tools = names
        .iter()
        .filter_map(|name| registry.get(name))
        .map(|spec| {
            serde_json::json!({
                "name": spec.name,
                "description": spec.description,
                "inputSchema": super::mcp_schema::shape_for_dialect(spec.parameters.clone(), dialect),
            })
        })
        .collect::<Vec<_>>();
    Ok(serde_json::json!({ "tools": tools }))
}

async fn call_tool(
    paths: &MiyuPaths,
    session: &Option<String>,
    origin: &Option<String>,
    depth: u32,
    name: &str,
    arguments: &str,
) -> Result<String> {
    if ipc::daemon_info(paths).await.is_some() {
        let (_, data) = send_ipc_admin(
            paths,
            IpcCommand::ToolCall {
                session: session.clone(),
                name: name.to_string(),
                arguments: arguments.to_string(),
                origin: origin.clone(),
                depth,
            },
        )
        .await?;
        return Ok(data
            .get("output")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_string());
    }
    if depth >= crate::tools::workspace::MAX_BRIDGE_DEPTH {
        bail!("tool bridge recursion limit reached (depth {depth})");
    }
    let config = AppConfig::load_or_default(paths)?;
    let mode = if std::env::var("MIYU_TURN_MODE").unwrap_or_default() == "dev" {
        AgentMode::Dev
    } else {
        AgentMode::Normal
    };
    let registry = build_tool_registry(&config, paths, mode, false)?;
    if !registry.contains(name) {
        bail!("{:#}", registry.unknown_tool_error(name));
    }
    let turn_origin: crate::tools::workspace::TurnOrigin = origin
        .as_deref()
        .and_then(|raw| serde_json::from_str(raw).ok())
        .unwrap_or(crate::tools::workspace::TurnOrigin::Human);
    let invoke = crate::tools::workspace::with_turn_origin(
        turn_origin,
        crate::tools::workspace::with_bridge_depth(depth + 1, async {
            registry.call(name, arguments).await
        }),
    );
    match session {
        Some(session) => {
            let session: std::sync::Arc<str> = session.clone().into();
            crate::tools::workspace::with_session(session, invoke).await
        }
        None => invoke.await,
    }
}
