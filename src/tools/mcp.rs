use super::{ToolRegistry, ToolSpec};
use crate::config::{AppConfig, McpServerConfig};
use anyhow::{bail, Context, Result};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

const JSONRPC_VERSION: &str = "2.0";

#[derive(Debug, Clone)]
struct McpToolBinding {
    server: McpServerConfig,
    tool_name: String,
}

#[derive(Debug, Deserialize)]
struct JsonRpcResponse {
    #[serde(default)]
    result: Option<Value>,
    #[serde(default)]
    error: Option<JsonRpcError>,
}

#[derive(Debug, Deserialize)]
struct JsonRpcError {
    code: i64,
    message: String,
    #[serde(default)]
    data: Option<Value>,
}

#[derive(Debug, Deserialize)]
struct ToolsListResult {
    #[serde(default)]
    tools: Vec<McpToolInfo>,
}

#[derive(Debug, Clone, Deserialize)]
struct McpToolInfo {
    name: String,
    #[serde(default)]
    description: String,
    #[serde(default, rename = "inputSchema")]
    input_schema: Value,
}

// ── tools/list 缓存 ──
//
// 09-04 issue #36:注册表每次重建(cold_context、TurnResources 的 normal/dev
// 两套、配置保存……)都对每个 MCP server 现 spawn 一次子进程做 tools/list,
// 并且是串行的。一个不可达的 server 就把每次重建拖到 timeout_seconds + 5s,
// 多个不可达的还累加;daemon 启动路径上这条链跑在 bind 之前,CLI 的 8 秒
// 就绪窗口一到就把 daemon 杀了,失败 warn 都来不及打。
//
// 三刀:(1) 列举结果按 server 配置缓存在进程里,成功永久(配置一变键就变),
// 失败带 TTL 免得每次重建都重试一遍死 server;(2) 未命中的 server 并行列举,
// 总耗时取最大而非求和;(3) 列举预算与 `timeout_seconds` 解耦——那个值是
// 给工具调用用的(60s 的调用很正常),tools/list 一个健康 server 一两秒就
// 该答完,上限单独封顶。

/// 失败的列举在这段时间内不再重试:死 server 让每次注册表重建都白等一轮
/// 太浪费,但也不能永久判死——server 修好了得能自动回来。
const FAILED_LISTING_RETRY_AFTER: Duration = Duration::from_secs(60);

/// tools/list 外层预算的封顶(秒),不含 5s 余量。
const LIST_TIMEOUT_CAP_SECS: u64 = 15;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ListingKey {
    id: String,
    command: String,
    args: Vec<String>,
    env: Vec<(String, String)>,
    timeout_seconds: u64,
}

impl ListingKey {
    fn of(server: &McpServerConfig) -> Self {
        let mut env: Vec<(String, String)> = server
            .env
            .iter()
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect();
        env.sort();
        Self {
            id: server.id.clone(),
            command: server.command.clone(),
            args: server.args.clone(),
            env,
            timeout_seconds: server.timeout_seconds,
        }
    }
}

#[derive(Debug, Clone)]
enum CachedListing {
    Tools(Arc<Vec<McpToolInfo>>),
    Failed { at: Instant, error: String },
}

fn listing_cache() -> &'static Mutex<HashMap<ListingKey, CachedListing>> {
    static CACHE: OnceLock<Mutex<HashMap<ListingKey, CachedListing>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// 缓存里可用的条目:成功的永远可用,失败的在 TTL 内可用(返回 Err 让调用
/// 方跳过、不重试),过期的当未命中。
fn cached_listing(key: &ListingKey) -> Option<std::result::Result<Arc<Vec<McpToolInfo>>, String>> {
    let cache = listing_cache().lock().unwrap();
    match cache.get(key)? {
        CachedListing::Tools(tools) => Some(Ok(tools.clone())),
        CachedListing::Failed { at, error } => {
            (at.elapsed() < FAILED_LISTING_RETRY_AFTER).then(|| Err(error.clone()))
        }
    }
}

fn store_listing(key: ListingKey, entry: CachedListing) {
    listing_cache().lock().unwrap().insert(key, entry);
}

/// 把一批 server 的工具列表拿到手:命中缓存的直接用,未命中的并行列举后
/// 回填。返回顺序与入参一致;列举失败的 server 值为 None。
fn resolve_listings(servers: &[&McpServerConfig]) -> Vec<Option<Arc<Vec<McpToolInfo>>>> {
    let mut resolved: Vec<Option<Arc<Vec<McpToolInfo>>>> = vec![None; servers.len()];
    let mut pending = Vec::new();
    for (index, server) in servers.iter().enumerate() {
        let key = ListingKey::of(server);
        match cached_listing(&key) {
            Some(Ok(tools)) => resolved[index] = Some(tools),
            Some(Err(error)) => tracing::debug!(
                server = %server.id,
                error = %error,
                "MCP server listing recently failed; skipping until retry window passes"
            ),
            None => pending.push((index, key)),
        }
    }
    if pending.is_empty() {
        return resolved;
    }
    // 结果走 channel 按完成顺序收:秒退的 server 立刻落日志、进缓存,不用
    // 排在最慢那个后面等。
    let (outcome_tx, outcome_rx) = std::sync::mpsc::channel();
    let expected = pending.len();
    for (index, key) in pending {
        let server = servers[index].clone();
        let outcome_tx = outcome_tx.clone();
        tracing::info!(server = %server.id, "listing MCP server tools");
        std::thread::spawn(move || {
            let outcome = list_server_tools_with_timeout(&server);
            let _ = outcome_tx.send((index, key, outcome));
        });
    }
    drop(outcome_tx);
    for (index, key, outcome) in outcome_rx.iter().take(expected) {
        let server = servers[index];
        match outcome {
            Ok(tools) => {
                tracing::info!(
                    server = %server.id,
                    tools = tools.len(),
                    "MCP server tools listed"
                );
                let tools = Arc::new(tools);
                store_listing(key, CachedListing::Tools(tools.clone()));
                resolved[index] = Some(tools);
            }
            Err(error) => {
                // 失败必须留日志,否则用户无从排查工具为何消失。daemon 默认
                // 只记 error 级(见 logging::selected_level),warn 用户看不到。
                let error = format!("{error:#}");
                tracing::error!(
                    server = %server.id,
                    error = %error,
                    retry_after_secs = FAILED_LISTING_RETRY_AFTER.as_secs(),
                    "MCP server failed to start or list tools; its tools are skipped"
                );
                store_listing(
                    key,
                    CachedListing::Failed {
                        at: Instant::now(),
                        error,
                    },
                );
            }
        }
    }
    resolved
}

pub fn register(registry: &mut ToolRegistry, config: AppConfig) {
    let servers: Vec<&McpServerConfig> = config
        .mcp
        .servers
        .iter()
        .filter(|server| server.enabled && !server.id.trim().is_empty())
        .collect();
    let listings = resolve_listings(&servers);
    for (server, tools) in servers.into_iter().zip(listings) {
        let Some(tools) = tools else {
            continue;
        };
        for tool in tools.iter().cloned() {
            let tool_id = mcp_tool_id(&server.id, &tool.name);
            let display_name = if server.display_name.trim().is_empty() {
                format!("MCP {} / {}", server.id, tool.name)
            } else {
                format!("MCP {} / {}", server.display_name, tool.name)
            };
            let binding = McpToolBinding {
                server: server.clone(),
                tool_name: tool.name.clone(),
            };
            let description = if tool.description.trim().is_empty() {
                format!("Call MCP tool {} from server {}.", tool.name, server.id)
            } else {
                tool.description.clone()
            };
            registry.register(
                ToolSpec::new(
                    tool_id,
                    description,
                    normalize_schema(tool.input_schema),
                    move |args| {
                        let binding = binding.clone();
                        async move { call_mcp_tool_async(binding, args).await }
                    },
                )
                .with_display_name(display_name)
                .with_always_loaded(false),
            );
        }
    }
}

/// 会话内的 `request` 超时只在两次成功读之间生效:server 完全不吐字节时
/// `read_line` 永久阻塞,轮不到超时检查。外层兜底 = 会话超时 + 5s 余量,
/// 超时后 SIGKILL 子进程让阻塞读收到 EOF,工作线程随之回收。
fn kill_mcp_child(pid: u32) {
    #[cfg(unix)]
    unsafe {
        libc::kill(pid as libc::pid_t, libc::SIGKILL);
    }
    #[cfg(not(unix))]
    let _ = pid;
}

fn overall_timeout(server: &McpServerConfig) -> Duration {
    Duration::from_secs(server.timeout_seconds.max(1)) + Duration::from_secs(5)
}

/// tools/list 的外层预算:`timeout_seconds` 是给工具调用的,列举单独封顶。
fn list_timeout(server: &McpServerConfig) -> Duration {
    Duration::from_secs(server.timeout_seconds.clamp(1, LIST_TIMEOUT_CAP_SECS))
        + Duration::from_secs(5)
}

fn list_server_tools(server: &McpServerConfig) -> Result<Vec<McpToolInfo>> {
    list_server_tools_inner(server, None)
}

fn list_server_tools_inner(
    server: &McpServerConfig,
    pid_notify: Option<std::sync::mpsc::Sender<u32>>,
) -> Result<Vec<McpToolInfo>> {
    let mut session = McpSession::start(server)?;
    if let Some(notify) = pid_notify {
        let _ = notify.send(session.child.id());
    }
    session.initialize()?;
    let result = session.request("tools/list", json!({}))?;
    let parsed: ToolsListResult = serde_json::from_value(result)?;
    Ok(parsed.tools)
}

fn list_server_tools_with_timeout(server: &McpServerConfig) -> Result<Vec<McpToolInfo>> {
    let deadline = list_timeout(server);
    let (result_tx, result_rx) = std::sync::mpsc::channel();
    let (pid_tx, pid_rx) = std::sync::mpsc::channel();
    let server_clone = server.clone();
    std::thread::spawn(move || {
        let _ = result_tx.send(list_server_tools_inner(&server_clone, Some(pid_tx)));
    });
    match result_rx.recv_timeout(deadline) {
        Ok(result) => result,
        Err(_) => {
            if let Ok(pid) = pid_rx.try_recv() {
                kill_mcp_child(pid);
            }
            bail!(
                "MCP server {} did not answer tools/list within {}s",
                server.id,
                deadline.as_secs()
            )
        }
    }
}

fn call_mcp_tool(binding: McpToolBinding, args: Value) -> Result<String> {
    call_mcp_tool_inner(binding, args, None)
}

fn call_mcp_tool_inner(
    binding: McpToolBinding,
    args: Value,
    pid_notify: Option<std::sync::mpsc::Sender<u32>>,
) -> Result<String> {
    let mut session = McpSession::start(&binding.server)?;
    if let Some(notify) = pid_notify {
        let _ = notify.send(session.child.id());
    }
    session.initialize()?;
    let result = session.request(
        "tools/call",
        json!({
            "name": binding.tool_name,
            "arguments": args,
        }),
    )?;
    Ok(format_mcp_result(&result))
}

/// 同步 MCP 会话移出 async 线程(spawn_blocking):在 actor 的单线程
/// runtime 上直接跑同步 IO 会冻结全部并发 turn;多次挂起还会耗尽 tokio
/// 阻塞池。外层超时 + kill 保证阻塞线程一定能回收。
async fn call_mcp_tool_async(binding: McpToolBinding, args: Value) -> Result<String> {
    let deadline = overall_timeout(&binding.server);
    let server_id = binding.server.id.clone();
    let (pid_tx, pid_rx) = std::sync::mpsc::channel();
    let task =
        tokio::task::spawn_blocking(move || call_mcp_tool_inner(binding, args, Some(pid_tx)));
    match tokio::time::timeout(deadline, task).await {
        Ok(joined) => joined.context("MCP worker task failed")?,
        Err(_) => {
            if let Ok(pid) = pid_rx.try_recv() {
                kill_mcp_child(pid);
            }
            bail!(
                "MCP server {server_id} did not answer within {}s",
                deadline.as_secs()
            )
        }
    }
}

struct McpSession {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: u64,
    timeout: Duration,
}

impl McpSession {
    fn start(server: &McpServerConfig) -> Result<Self> {
        if server.command.trim().is_empty() {
            bail!("MCP server {} has no command", server.id);
        }
        let mut command = Command::new(&server.command);
        command.args(&server.args);
        for (key, value) in &server.env {
            command.env(key, value);
        }
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let mut child = command
            .spawn()
            .with_context(|| format!("failed to start MCP server {}", server.id))?;
        let stdin = child.stdin.take().context("failed to open MCP stdin")?;
        let stdout = child.stdout.take().context("failed to open MCP stdout")?;
        Ok(Self {
            child,
            stdin,
            stdout: BufReader::new(stdout),
            next_id: 1,
            timeout: Duration::from_secs(server.timeout_seconds.max(1)),
        })
    }

    fn initialize(&mut self) -> Result<()> {
        self.request(
            "initialize",
            json!({
                "protocolVersion": "2025-03-26",
                "capabilities": {},
                "clientInfo": {"name": "miyu", "version": env!("CARGO_PKG_VERSION")},
            }),
        )?;
        self.notify("notifications/initialized", json!({}))?;
        Ok(())
    }

    fn request(&mut self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id;
        self.next_id += 1;
        let request = json!({
            "jsonrpc": JSONRPC_VERSION,
            "id": id,
            "method": method,
            "params": params,
        });
        self.write_message(&request)?;
        let started = Instant::now();
        loop {
            if started.elapsed() > self.timeout {
                bail!("MCP request timed out: {method}");
            }
            let response = self.read_message()?;
            if response.get("id").and_then(Value::as_u64) != Some(id) {
                continue;
            }
            let response: JsonRpcResponse = serde_json::from_value(response)?;
            if let Some(error) = response.error {
                bail!(
                    "MCP error {}: {}{}",
                    error.code,
                    error.message,
                    format_error_data(&error.data)
                );
            }
            return Ok(response.result.unwrap_or(Value::Null));
        }
    }

    fn notify(&mut self, method: &str, params: Value) -> Result<()> {
        self.write_message(&json!({
            "jsonrpc": JSONRPC_VERSION,
            "method": method,
            "params": params,
        }))
    }

    fn write_message(&mut self, value: &Value) -> Result<()> {
        serde_json::to_writer(&mut self.stdin, value)?;
        self.stdin.write_all(b"\n")?;
        self.stdin.flush()?;
        Ok(())
    }

    fn read_message(&mut self) -> Result<Value> {
        let mut line = String::new();
        let bytes = self.stdout.read_line(&mut line)?;
        if bytes == 0 {
            bail!("MCP server closed stdout");
        }
        Ok(serde_json::from_str(line.trim())?)
    }
}

impl Drop for McpSession {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn mcp_tool_id(server_id: &str, tool_name: &str) -> String {
    format!("mcp_{}_{}", sanitize_id(server_id), sanitize_id(tool_name))
}

fn sanitize_id(value: &str) -> String {
    let mut out = value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() {
                ch.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect::<String>();
    while out.contains("__") {
        out = out.replace("__", "_");
    }
    out.trim_matches('_').to_string()
}

fn normalize_schema(schema: Value) -> Value {
    if schema.is_object() {
        schema
    } else {
        json!({"type":"object","properties":{},"additionalProperties":true})
    }
}

fn format_mcp_result(result: &Value) -> String {
    if let Some(content) = result.get("content").and_then(Value::as_array) {
        let parts = content
            .iter()
            .filter_map(format_content_part)
            .collect::<Vec<_>>();
        if !parts.is_empty() {
            return parts.join("\n\n");
        }
    }
    serde_json::to_string_pretty(result).unwrap_or_else(|_| result.to_string())
}

fn format_content_part(value: &Value) -> Option<String> {
    match value.get("type").and_then(Value::as_str) {
        Some("text") => value
            .get("text")
            .and_then(Value::as_str)
            .map(str::to_string),
        Some(kind) => {
            Some(serde_json::to_string_pretty(value).unwrap_or_else(|_| kind.to_string()))
        }
        None => Some(serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string())),
    }
}

fn format_error_data(data: &Option<Value>) -> String {
    data.as_ref()
        .map(|data| format!(": {}", data))
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn sanitizes_mcp_tool_ids() {
        assert_eq!(
            mcp_tool_id("file-system", "read_file"),
            "mcp_file_system_read_file"
        );
    }

    #[test]
    fn formats_text_content_results() {
        let result = json!({"content":[{"type":"text","text":"hello"}]});
        assert_eq!(format_mcp_result(&result), "hello");
    }

    #[test]
    fn lists_and_calls_stdio_mcp_tool() {
        let script = r#"
import json, sys
for line in sys.stdin:
    request = json.loads(line)
    method = request.get('method')
    if 'id' not in request:
        continue
    if method == 'initialize':
        result = {'protocolVersion':'2025-03-26','capabilities':{},'serverInfo':{'name':'mock','version':'1'}}
    elif method == 'tools/list':
        result = {'tools':[{'name':'echo','description':'Echo text','inputSchema':{'type':'object','properties':{'text':{'type':'string'}}}}]}
    elif method == 'tools/call':
        text = request.get('params', {}).get('arguments', {}).get('text', '')
        result = {'content':[{'type':'text','text':'echo: ' + text}]}
    else:
        result = {}
    print(json.dumps({'jsonrpc':'2.0','id':request['id'],'result':result}), flush=True)
"#;
        let server = McpServerConfig {
            id: "mock".to_string(),
            display_name: String::new(),
            command: "python".to_string(),
            args: vec!["-c".to_string(), script.to_string()],
            env: HashMap::new(),
            timeout_seconds: 5,
            enabled: true,
        };

        let tools = list_server_tools(&server).unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "echo");

        let output = call_mcp_tool(
            McpToolBinding {
                server,
                tool_name: "echo".to_string(),
            },
            json!({"text":"hi"}),
        )
        .unwrap();
        assert_eq!(output, "echo: hi");
    }
}

#[cfg(test)]
mod listing_cache_tests {
    use super::*;
    use std::collections::HashMap;

    /// 每次启动往 `marker` 追加一行,便于数子进程被 spawn 了几次。
    /// `delay_secs` > 0 时先睡再应答,用来量并行度。`exit_early` 则记完
    /// 标记直接退出,模拟起不来的 server。
    fn mock_server(
        id: &str,
        marker: &std::path::Path,
        delay_secs: f64,
        exit_early: bool,
    ) -> McpServerConfig {
        let script = format!(
            r#"
import json, sys, time
with open({marker:?}, 'a') as f:
    f.write('start\n')
if {exit_early}:
    sys.exit(0)
time.sleep({delay_secs})
for line in sys.stdin:
    request = json.loads(line)
    method = request.get('method')
    if 'id' not in request:
        continue
    if method == 'initialize':
        result = {{'protocolVersion':'2025-03-26','capabilities':{{}},'serverInfo':{{'name':'mock','version':'1'}}}}
    elif method == 'tools/list':
        result = {{'tools':[{{'name':'echo','description':'Echo text','inputSchema':{{'type':'object'}}}}]}}
    else:
        result = {{}}
    print(json.dumps({{'jsonrpc':'2.0','id':request['id'],'result':result}}), flush=True)
"#,
            marker = marker.to_string_lossy(),
            exit_early = if exit_early { "True" } else { "False" },
        );
        McpServerConfig {
            id: id.to_string(),
            display_name: String::new(),
            command: "python".to_string(),
            args: vec!["-c".to_string(), script],
            env: HashMap::new(),
            timeout_seconds: 5,
            enabled: true,
        }
    }

    fn config_with(servers: Vec<McpServerConfig>) -> AppConfig {
        let mut config = AppConfig::default();
        config.mcp.enabled = true;
        config.mcp.servers = servers;
        config
    }

    fn spawn_count(marker: &std::path::Path) -> usize {
        std::fs::read_to_string(marker)
            .map(|text| text.lines().count())
            .unwrap_or(0)
    }

    #[test]
    fn successful_listing_is_reused_across_registry_rebuilds() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("spawns");
        let config = config_with(vec![mock_server("cache-hit", &marker, 0.0, false)]);

        let mut first = ToolRegistry::new();
        register(&mut first, config.clone());
        let mut second = ToolRegistry::new();
        register(&mut second, config);

        assert!(first.contains("mcp_cache_hit_echo"));
        assert!(second.contains("mcp_cache_hit_echo"));
        assert_eq!(spawn_count(&marker), 1, "second rebuild must hit the cache");
    }

    #[test]
    fn failed_listing_is_not_retried_within_window() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("spawns");
        let config = config_with(vec![mock_server("cache-miss", &marker, 0.0, true)]);

        let mut first = ToolRegistry::new();
        register(&mut first, config.clone());
        let mut second = ToolRegistry::new();
        register(&mut second, config);

        assert!(!first.contains("mcp_cache_miss_echo"));
        assert!(!second.contains("mcp_cache_miss_echo"));
        assert_eq!(
            spawn_count(&marker),
            1,
            "dead server must not be re-spawned per rebuild"
        );
    }

    #[test]
    fn uncached_servers_are_listed_in_parallel() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("spawns");
        let config = config_with(vec![
            mock_server("parallel-a", &marker, 1.0, false),
            mock_server("parallel-b", &marker, 1.0, false),
            mock_server("parallel-c", &marker, 1.0, false),
        ]);

        let started = Instant::now();
        let mut registry = ToolRegistry::new();
        register(&mut registry, config);
        let elapsed = started.elapsed();

        assert!(registry.contains("mcp_parallel_a_echo"));
        assert!(registry.contains("mcp_parallel_c_echo"));
        assert_eq!(spawn_count(&marker), 3);
        assert!(
            elapsed < Duration::from_secs(2),
            "three 1s servers must overlap, took {elapsed:?}"
        );
    }

    #[test]
    fn list_budget_is_capped_independently_of_call_timeout() {
        let mut server = mock_server("budget", std::path::Path::new("/dev/null"), 0.0, false);
        server.timeout_seconds = 600;
        assert_eq!(overall_timeout(&server), Duration::from_secs(605));
        assert_eq!(
            list_timeout(&server),
            Duration::from_secs(LIST_TIMEOUT_CAP_SECS + 5)
        );
        server.timeout_seconds = 3;
        assert_eq!(list_timeout(&server), Duration::from_secs(8));
    }
}
