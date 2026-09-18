//! MCP（Model Context Protocol）stdio 客户端。
//!
//! Claude Code、Codex、Kimi CLI 都用 MCP 作为「外部工具生态」的统一入口。
//! 本模块让 Wonderland 复用同一批 MCP 服务器：在项目里配置 `mcpServers` 后，
//! 服务器暴露的工具会以 `mcp__<server>__<tool>` 的名字注册进工具表，
//! 权限管线、hooks、会话轨迹对它们与内置工具一视同仁。
//!
//! 配置文件（从低到高优先级）：`<cwd>/.mcp.json`、`<cwd>/.claude/settings.json`、
//! `<cwd>/.wonderland/settings.json`，都读取顶层 `mcpServers` 对象。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{ChildStdin, Command};
use tokio::sync::{oneshot, Mutex};

use crate::tools::{Tool, ToolContext, ToolOutput};

/// 与参考实现一致：MCP 工具在模型侧的名字前缀。
pub const MCP_TOOL_PREFIX: &str = "mcp__";
/// 我们声明的协议版本。
pub const MCP_PROTOCOL_VERSION: &str = "2024-11-05";
/// 工具名在各家 API 上的长度上限（OpenAI 为 64）。
const MAX_TOOL_NAME_LEN: usize = 64;
/// 默认请求超时。
const DEFAULT_TIMEOUT_MS: u64 = 30_000;

/// 一个 MCP 服务器的启动配置，字段名与 Claude Code 的 `mcpServers` 对齐。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct McpServerConfig {
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: HashMap<String, String>,
    /// 服务器进程的工作目录，缺省用请求的 cwd。
    #[serde(default)]
    pub cwd: Option<String>,
    /// 把该服务器的全部工具按只读处理（只读探索类服务器可这样声明）。
    #[serde(default)]
    pub read_only: bool,
    /// 显式关闭某个服务器，便于临时停用而不删配置。
    #[serde(default)]
    pub enabled: Option<bool>,
}

impl McpServerConfig {
    fn is_enabled(&self) -> bool {
        self.enabled.unwrap_or(true) && !self.command.trim().is_empty()
    }
}

/// 从项目目录加载 `mcpServers` 配置，后加载的文件覆盖同名服务器。
pub fn load_server_configs(cwd: &Path) -> Vec<(String, McpServerConfig)> {
    let files = [
        cwd.join(".mcp.json"),
        cwd.join(".claude").join("settings.json"),
        cwd.join(".wonderland").join("settings.json"),
    ];
    let mut merged: Vec<(String, McpServerConfig)> = Vec::new();
    for file in files {
        let Ok(raw) = std::fs::read_to_string(&file) else {
            continue;
        };
        let Ok(value) = serde_json::from_str::<Value>(&raw) else {
            tracing::warn!(file = %file.display(), "mcpServers 配置不是合法 JSON，已忽略");
            continue;
        };
        let Some(servers) = value.get("mcpServers").and_then(Value::as_object) else {
            continue;
        };
        for (name, entry) in servers {
            match serde_json::from_value::<McpServerConfig>(entry.clone()) {
                Ok(config) if config.is_enabled() => {
                    merged.retain(|(existing, _)| existing != name);
                    merged.push((name.clone(), config));
                }
                Ok(_) => {
                    merged.retain(|(existing, _)| existing != name);
                    tracing::info!(server = %name, "mcp server disabled by configuration");
                }
                Err(error) => {
                    tracing::warn!(server = %name, %error, "invalid mcp server config, skipped");
                }
            }
        }
    }
    merged
}

/// 一个 MCP 服务器暴露的工具描述。
#[derive(Debug, Clone, PartialEq)]
pub struct McpToolInfo {
    /// 服务器侧的工具原名。
    pub name: String,
    pub description: String,
    pub input_schema: Value,
    pub read_only: bool,
}

/// 解析 `tools/list` 的结果：`(工具列表, 下一页 cursor)`。纯函数。
pub fn parse_tools_list(result: &Value) -> (Vec<McpToolInfo>, Option<String>) {
    let mut tools = Vec::new();
    if let Some(entries) = result.get("tools").and_then(Value::as_array) {
        for entry in entries {
            let Some(name) = entry
                .get("name")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|name| !name.is_empty())
            else {
                continue;
            };
            let description = entry
                .get("description")
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim()
                .to_string();
            let input_schema = entry
                .get("inputSchema")
                .cloned()
                .filter(|schema| schema.is_object())
                .unwrap_or_else(|| json!({"type": "object", "properties": {}}));
            let read_only = entry
                .get("annotations")
                .and_then(|annotations| annotations.get("readOnlyHint"))
                .and_then(Value::as_bool)
                .unwrap_or(false);
            tools.push(McpToolInfo {
                name: name.to_string(),
                description,
                input_schema,
                read_only,
            });
        }
    }
    let next_cursor = result
        .get("nextCursor")
        .and_then(Value::as_str)
        .filter(|cursor| !cursor.is_empty())
        .map(str::to_string);
    (tools, next_cursor)
}

/// 把 `tools/call` 的结果渲染为文本：`(内容, 是否错误)`。纯函数。
pub fn render_call_result(result: &Value) -> (String, bool) {
    let mut parts = Vec::new();
    if let Some(blocks) = result.get("content").and_then(Value::as_array) {
        for block in blocks {
            match block
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or_default()
            {
                "text" => {
                    if let Some(text) = block.get("text").and_then(Value::as_str) {
                        parts.push(text.to_string());
                    }
                }
                "image" | "audio" => {
                    let mime = block
                        .get("mimeType")
                        .and_then(Value::as_str)
                        .unwrap_or("application/octet-stream");
                    parts.push(format!("[{mime} 内容已省略]"));
                }
                "resource" => {
                    let uri = block
                        .get("resource")
                        .and_then(|resource| resource.get("uri"))
                        .and_then(Value::as_str)
                        .unwrap_or("unknown");
                    parts.push(format!("[resource {uri}]"));
                }
                other => parts.push(format!("[{other}]")),
            }
        }
    }
    if parts.is_empty() {
        if let Some(structured) = result.get("structuredContent") {
            if !structured.is_null() {
                parts.push(structured.to_string());
            }
        }
    }
    if parts.is_empty() {
        parts.push("(MCP 工具没有返回内容)".to_string());
    }
    let is_error = result
        .get("isError")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    (parts.join("\n"), is_error)
}

/// 服务器侧工具名 → 模型可见的工具名。纯函数。
pub fn exposed_tool_name(server: &str, tool: &str) -> String {
    let sanitize = |value: &str| -> String {
        value
            .chars()
            .map(|ch| {
                if ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' {
                    ch
                } else {
                    '_'
                }
            })
            .collect()
    };
    let mut name = format!("{MCP_TOOL_PREFIX}{}__{}", sanitize(server), sanitize(tool));
    if name.len() > MAX_TOOL_NAME_LEN {
        name.truncate(MAX_TOOL_NAME_LEN);
    }
    name
}

/// 从模型可见的工具名解析出 MCP 服务器名；非 MCP 工具返回 `None`。
pub fn tool_server(exposed: &str) -> Option<&str> {
    let rest = exposed.strip_prefix(MCP_TOOL_PREFIX)?;
    let (server, _tool) = rest.split_once("__")?;
    if server.is_empty() {
        return None;
    }
    Some(server)
}

/// 工具来源标签：内置工具为 `builtin`，MCP 工具为服务器名。
pub fn tool_source(exposed: &str) -> String {
    tool_server(exposed).unwrap_or("builtin").to_string()
}

fn request_timeout() -> Duration {
    let millis = std::env::var("AGENT_MCP_TIMEOUT_MS")
        .ok()
        .and_then(|value| value.trim().parse::<u64>().ok())
        .unwrap_or(DEFAULT_TIMEOUT_MS);
    Duration::from_millis(millis.max(1_000))
}

type PendingMap = Arc<Mutex<HashMap<i64, oneshot::Sender<Result<Value, String>>>>>;

/// 一个已握手的 MCP 服务器连接。
pub struct McpClient {
    name: String,
    stdin: Mutex<ChildStdin>,
    pending: PendingMap,
    next_id: AtomicI64,
    timeout: Duration,
}

impl McpClient {
    /// 启动服务器进程并完成 `initialize` 握手。
    pub async fn connect(name: &str, config: &McpServerConfig, cwd: &Path) -> Result<Arc<Self>> {
        let mut command = Command::new(&config.command);
        command.args(&config.args);
        command.envs(&config.env);
        command.kill_on_drop(false);
        command.stdin(Stdio::piped());
        command.stdout(Stdio::piped());
        command.stderr(Stdio::piped());
        let working_dir = config
            .cwd
            .as_deref()
            .map(PathBuf::from)
            .unwrap_or_else(|| cwd.to_path_buf());
        command.current_dir(working_dir);

        let mut child = command
            .spawn()
            .with_context(|| format!("failed to start mcp server `{name}` ({})", config.command))?;
        let stdin = child.stdin.take().context("mcp server stdin unavailable")?;
        let stdout = child
            .stdout
            .take()
            .context("mcp server stdout unavailable")?;
        let stderr = child.stderr.take();

        let pending: PendingMap = Arc::new(Mutex::new(HashMap::new()));
        let reader_pending = pending.clone();
        let server_name = name.to_string();
        tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let line = line.trim();
                if line.is_empty() {
                    continue;
                }
                let Ok(value) = serde_json::from_str::<Value>(line) else {
                    tracing::debug!(server = %server_name, "unparsable mcp message");
                    continue;
                };
                let id = value
                    .get("id")
                    .and_then(|id| {
                        id.as_i64()
                            .or_else(|| id.as_str().and_then(|text| text.parse::<i64>().ok()))
                    })
                    .unwrap_or(i64::MIN);
                if id == i64::MIN {
                    // 通知（如 logging），忽略。
                    continue;
                }
                let sender = reader_pending.lock().await.remove(&id);
                if let Some(sender) = sender {
                    let outcome = match value.get("error") {
                        Some(error) => Err(error.to_string()),
                        None => Ok(value.get("result").cloned().unwrap_or(Value::Null)),
                    };
                    let _ = sender.send(outcome);
                }
            }
            tracing::debug!(server = %server_name, "mcp server stdout closed");
            let _ = child.kill().await;
        });

        if let Some(stderr) = stderr {
            let server_name = name.to_string();
            tokio::spawn(async move {
                let mut lines = BufReader::new(stderr).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    tracing::debug!(server = %server_name, "mcp stderr: {line}");
                }
            });
        }

        let client = Arc::new(Self {
            name: name.to_string(),
            stdin: Mutex::new(stdin),
            pending,
            next_id: AtomicI64::new(1),
            timeout: request_timeout(),
        });

        client
            .request(
                "initialize",
                json!({
                    "protocolVersion": MCP_PROTOCOL_VERSION,
                    "capabilities": {},
                    "clientInfo": {
                        "name": "wonderland",
                        "version": env!("CARGO_PKG_VERSION"),
                    },
                }),
            )
            .await
            .with_context(|| format!("mcp server `{name}` failed to initialize"))?;
        client
            .notify("notifications/initialized", json!({}))
            .await?;
        Ok(client)
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    async fn request(&self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (sender, receiver) = oneshot::channel();
        self.pending.lock().await.insert(id, sender);

        let payload = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        {
            let mut stdin = self.stdin.lock().await;
            stdin
                .write_all(serde_json::to_string(&payload)?.as_bytes())
                .await
                .context("failed to write to mcp server")?;
            stdin.write_all(b"\n").await?;
            stdin.flush().await?;
        }

        match tokio::time::timeout(self.timeout, receiver).await {
            Ok(Ok(Ok(result))) => Ok(result),
            Ok(Ok(Err(message))) => bail!("mcp {method} failed: {message}"),
            Ok(Err(_)) => bail!("mcp server closed the connection during {method}"),
            Err(_) => {
                self.pending.lock().await.remove(&id);
                bail!(
                    "mcp {method} timed out after {} ms",
                    self.timeout.as_millis()
                )
            }
        }
    }

    async fn notify(&self, method: &str, params: Value) -> Result<()> {
        let payload = json!({"jsonrpc": "2.0", "method": method, "params": params});
        let mut stdin = self.stdin.lock().await;
        stdin
            .write_all(serde_json::to_string(&payload)?.as_bytes())
            .await?;
        stdin.write_all(b"\n").await?;
        stdin.flush().await?;
        Ok(())
    }

    /// `tools/list`，自动跟随 `nextCursor` 分页。
    pub async fn list_tools(&self) -> Result<Vec<McpToolInfo>> {
        let mut tools = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let params = match &cursor {
                Some(cursor) => json!({ "cursor": cursor }),
                None => json!({}),
            };
            let result = self.request("tools/list", params).await?;
            let (page, next) = parse_tools_list(&result);
            tools.extend(page);
            match next {
                Some(next) => cursor = Some(next),
                None => break,
            }
        }
        Ok(tools)
    }

    /// `tools/call`。
    pub async fn call_tool(&self, tool: &str, arguments: Value) -> Result<(String, bool)> {
        let result = self
            .request(
                "tools/call",
                json!({ "name": tool, "arguments": arguments }),
            )
            .await?;
        Ok(render_call_result(&result))
    }
}

/// 把 MCP 工具适配成内置工具同款接口。
pub struct McpTool {
    client: Arc<McpClient>,
    exposed_name: String,
    server_tool: String,
    description: String,
    schema: Value,
    read_only: bool,
}

impl McpTool {
    pub fn new(client: Arc<McpClient>, info: &McpToolInfo) -> Self {
        Self {
            exposed_name: exposed_tool_name(client.name(), &info.name),
            server_tool: info.name.clone(),
            description: if info.description.trim().is_empty() {
                format!("MCP 工具 {}（由服务器 {} 提供）", info.name, client.name())
            } else {
                format!(
                    "{} [MCP 服务器 {} 提供]",
                    info.description.trim(),
                    client.name()
                )
            },
            schema: info.input_schema.clone(),
            read_only: info.read_only,
            client,
        }
    }

    pub fn server(&self) -> &str {
        self.client.name()
    }

    pub fn server_tool(&self) -> &str {
        &self.server_tool
    }
}

#[async_trait]
impl Tool for McpTool {
    fn name(&self) -> &str {
        &self.exposed_name
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn input_schema(&self) -> Value {
        self.schema.clone()
    }

    fn is_read_only(&self, _input: &Value) -> bool {
        self.read_only
    }

    async fn call(&self, input: Value, _ctx: &mut ToolContext) -> Result<ToolOutput> {
        let (content, is_error) = self.client.call_tool(&self.server_tool, input).await?;
        Ok(if is_error {
            ToolOutput::err(content)
        } else {
            ToolOutput::ok(content)
        })
    }
}

/// 一次 MCP 加载的结果。
#[derive(Default)]
pub struct McpLoadOutcome {
    pub tools: Vec<Arc<dyn Tool>>,
    /// `(服务器名, 失败原因)`。
    pub errors: Vec<(String, String)>,
    /// `(服务器名, 工具数)`。
    pub servers: Vec<(String, usize)>,
}

/// 启动全部配置的 MCP 服务器并收集工具。单个服务器失败不影响其它服务器。
pub async fn load_tools(cwd: &Path) -> McpLoadOutcome {
    let mut outcome = McpLoadOutcome::default();
    for (name, config) in load_server_configs(cwd) {
        match McpClient::connect(&name, &config, cwd).await {
            Ok(client) => match client.list_tools().await {
                Ok(tools) => {
                    outcome.servers.push((name.clone(), tools.len()));
                    for info in &tools {
                        let mut tool = McpTool::new(client.clone(), info);
                        // 服务器级只读声明：该进程提供的全部工具按只读处理。
                        tool.read_only |= config.read_only;
                        outcome.tools.push(Arc::new(tool));
                    }
                }
                Err(error) => outcome.errors.push((name, error.to_string())),
            },
            Err(error) => outcome.errors.push((name, error.to_string())),
        }
    }
    outcome
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn write_settings(path: &Path, servers: Value) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, json!({ "mcpServers": servers }).to_string()).unwrap();
    }

    #[test]
    fn loads_servers_from_all_three_files_with_override() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        write_settings(
            &root.join(".mcp.json"),
            json!({"filesystem": {"command": "npx", "args": ["-y", "fs"], "env": {"A": "1"}}}),
        );
        write_settings(
            &root.join(".claude").join("settings.json"),
            json!({"github": {"command": "gh-mcp"}}),
        );
        // 高优先级文件覆盖同名服务器。
        write_settings(
            &root.join(".wonderland").join("settings.json"),
            json!({"filesystem": {"command": "python", "args": ["server.py"]}}),
        );

        let configs = load_server_configs(root);
        let names: Vec<&str> = configs.iter().map(|(name, _)| name.as_str()).collect();
        assert!(names.contains(&"github"));
        let filesystem = configs
            .iter()
            .find(|(name, _)| name == "filesystem")
            .map(|(_, config)| config.clone())
            .unwrap();
        assert_eq!(filesystem.command, "python");
        assert_eq!(filesystem.args, vec!["server.py".to_string()]);
        assert!(filesystem.env.is_empty());
    }

    #[test]
    fn disabled_and_invalid_servers_are_skipped() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        write_settings(
            &root.join(".mcp.json"),
            json!({
                "off": {"command": "x", "enabled": false},
                "blank": {"command": "   "},
                "broken": {"args": ["no-command"]},
                "ok": {"command": "run-me"}
            }),
        );
        let configs = load_server_configs(root);
        assert_eq!(configs.len(), 1);
        assert_eq!(configs[0].0, "ok");
    }

    #[test]
    fn missing_or_invalid_files_are_ignored() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        assert!(load_server_configs(root).is_empty());
        fs::write(root.join(".mcp.json"), "{ not json").unwrap();
        assert!(load_server_configs(root).is_empty());
        fs::write(root.join(".mcp.json"), json!({"other": {}}).to_string()).unwrap();
        assert!(load_server_configs(root).is_empty());
    }

    #[test]
    fn tool_names_are_namespaced_and_sanitized() {
        assert_eq!(
            exposed_tool_name("github", "search_repos"),
            "mcp__github__search_repos"
        );
        assert_eq!(
            exposed_tool_name("my server", "read/file"),
            "mcp__my_server__read_file"
        );
        let long = exposed_tool_name("server", &"x".repeat(120));
        assert_eq!(long.len(), MAX_TOOL_NAME_LEN);
        assert!(long.starts_with("mcp__server__"));
    }

    #[test]
    fn tool_server_is_recovered_from_exposed_name() {
        assert_eq!(tool_server("mcp__github__search"), Some("github"));
        assert_eq!(tool_server("mcp__my_server__read_file"), Some("my_server"));
        assert_eq!(tool_server("mcp__broken"), None);
        assert_eq!(tool_server("Bash"), None);
        assert_eq!(tool_source("Bash"), "builtin");
        assert_eq!(tool_source("mcp__github__search"), "github");
    }

    #[test]
    fn parses_tools_list_with_pagination_and_annotations() {
        let result = json!({
            "tools": [
                {
                    "name": "read_file",
                    "description": "read a file",
                    "inputSchema": {"type": "object", "properties": {"path": {"type": "string"}}},
                    "annotations": {"readOnlyHint": true}
                },
                {"name": "bad"},
                {"description": "without name"}
            ],
            "nextCursor": "page-2"
        });
        let (tools, cursor) = parse_tools_list(&result);
        assert_eq!(tools.len(), 2);
        assert_eq!(cursor.as_deref(), Some("page-2"));
        assert_eq!(tools[0].name, "read_file");
        assert!(tools[0].read_only);
        // 缺省 schema 兜底为空对象，缺省描述为空串。
        assert_eq!(tools[1].input_schema["type"], "object");
        assert_eq!(tools[1].description, "");
        assert!(!tools[1].read_only);

        let (empty, none) = parse_tools_list(&json!({"tools": []}));
        assert!(empty.is_empty());
        assert!(none.is_none());
    }

    #[test]
    fn renders_call_results_of_every_block_type() {
        let (text, is_error) = render_call_result(&json!({
            "content": [
                {"type": "text", "text": "line one"},
                {"type": "text", "text": "line two"},
                {"type": "image", "mimeType": "image/png", "data": "..."},
                {"type": "resource", "resource": {"uri": "file:///a.txt"}}
            ]
        }));
        assert!(!is_error);
        assert!(text.contains("line one"));
        assert!(text.contains("image/png"));
        assert!(text.contains("file:///a.txt"));

        let (structured, _) = render_call_result(&json!({"structuredContent": {"count": 2}}));
        assert!(structured.contains("\"count\":2"));

        let (fallback, _) = render_call_result(&json!({"content": []}));
        assert!(fallback.contains("没有返回内容"));

        let (_, error) = render_call_result(&json!({"content": [], "isError": true}));
        assert!(error);
    }

    /// 用 Python 写一个最小 MCP 服务器做端到端握手验证；
    /// 找不到 Python 时跳过（CI 环境可能没有解释器）。
    #[tokio::test]
    async fn handshake_and_tool_call_against_a_stdio_server() {
        let interpreter = ["python", "python3", "py"].into_iter().find(|candidate| {
            std::process::Command::new(candidate)
                .arg("--version")
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .map(|status| status.success())
                .unwrap_or(false)
        });
        let Some(interpreter) = interpreter else {
            eprintln!("python not available; skipping mcp stdio integration test");
            return;
        };

        let script = r#"
import json, sys
for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    message = json.loads(line)
    method = message.get("method")
    identifier = message.get("id")
    if identifier is None:
        continue
    if method == "initialize":
        result = {"protocolVersion": "2024-11-05", "capabilities": {"tools": {}},
                  "serverInfo": {"name": "fake", "version": "0.0.1"}}
    elif method == "tools/list":
        result = {"tools": [
            {"name": "echo", "description": "echo text back",
             "inputSchema": {"type": "object", "properties": {"text": {"type": "string"}},
                             "required": ["text"]},
             "annotations": {"readOnlyHint": True}}]}
    elif method == "tools/call":
        arguments = message.get("params", {}).get("arguments", {})
        result = {"content": [{"type": "text", "text": "echo:" + str(arguments.get("text"))}]}
    else:
        result = None
    if result is None:
        response = {"jsonrpc": "2.0", "id": identifier,
                    "error": {"code": -32601, "message": "method not found"}}
    else:
        response = {"jsonrpc": "2.0", "id": identifier, "result": result}
    sys.stdout.write(json.dumps(response) + "\n")
    sys.stdout.flush()
"#;

        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        write_settings(
            &root.join(".mcp.json"),
            json!({"fake": {"command": interpreter, "args": ["-u", "-c", script]}}),
        );

        let outcome = load_tools(root).await;
        assert!(
            outcome.errors.is_empty(),
            "mcp load errors: {:?}",
            outcome.errors
        );
        assert_eq!(outcome.servers, vec![("fake".to_string(), 1)]);
        assert_eq!(outcome.tools.len(), 1);

        let tool = outcome.tools[0].clone();
        assert_eq!(tool.name(), "mcp__fake__echo");
        assert!(tool.is_read_only(&json!({})));
        assert_eq!(tool.input_schema()["properties"]["text"]["type"], "string");

        let mut context = ToolContext::for_tests(root.to_path_buf());
        let output = tool
            .call(json!({"text": "hello"}), &mut context)
            .await
            .unwrap();
        assert!(!output.is_error);
        assert_eq!(output.content, "echo:hello");

        // 服务器不认识的工具调用必须以错误返回，而不是让 agent loop 挂住。
        let (content, is_error) = outcome.tools[0]
            .as_ref()
            .call(json!({"text": "ignored"}), &mut context)
            .await
            .map(|output| (output.content, output.is_error))
            .unwrap();
        assert!(!is_error);
        assert!(content.starts_with("echo:"));

        // 服务器退出后，后续请求应快速失败而不是永久等待。
        let client = McpClient::connect(
            "fake",
            &McpServerConfig {
                command: interpreter.to_string(),
                args: vec![
                    "-u".to_string(),
                    "-c".to_string(),
                    script.replace("tools/call", "tools/unknown"),
                ],
                env: HashMap::new(),
                cwd: None,
                read_only: false,
                enabled: None,
            },
            root,
        )
        .await
        .unwrap();
        let error = client.call_tool("echo", json!({})).await.unwrap_err();
        assert!(error.to_string().contains("method not found"), "{error}");
    }
}
