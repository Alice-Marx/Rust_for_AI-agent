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

use crate::provider::SseBuffer;
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
    /// 传输类型：stdio（缺省）或 http（Streamable HTTP / SSE）。
    #[serde(default, rename = "type")]
    pub transport: Option<String>,
    /// http 传输的端点地址。
    #[serde(default)]
    pub url: Option<String>,
    /// http 传输的附加请求头（例如 Authorization）。
    #[serde(default)]
    pub headers: HashMap<String, String>,
}

impl McpServerConfig {
    fn is_enabled(&self) -> bool {
        if !self.enabled.unwrap_or(true) {
            return false;
        }
        match self.transport_kind() {
            McpTransportKind::Http => self
                .url
                .as_deref()
                .map(str::trim)
                .is_some_and(|url| !url.is_empty()),
            McpTransportKind::Stdio => !self.command.trim().is_empty(),
        }
    }

    pub fn transport_kind(&self) -> McpTransportKind {
        match self
            .transport
            .as_deref()
            .map(str::trim)
            .map(str::to_ascii_lowercase)
            .as_deref()
        {
            Some("http") | Some("sse") | Some("streamable-http") | Some("streamable_http") => {
                McpTransportKind::Http
            }
            _ if self.command.trim().is_empty()
                && self
                    .url
                    .as_deref()
                    .map(str::trim)
                    .is_some_and(|url| !url.is_empty()) =>
            {
                McpTransportKind::Http
            }
            _ => McpTransportKind::Stdio,
        }
    }
}

/// 传输类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum McpTransportKind {
    Stdio,
    Http,
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

/// 会话抽象：stdio 与 HTTP 两种传输共享同一套工具适配。
#[async_trait]
pub trait McpSession: Send + Sync {
    fn name(&self) -> &str;

    /// initialize 返回的能力声明。
    fn capabilities(&self) -> Value;

    async fn request(&self, method: &str, params: Value) -> Result<Value>;

    /// 发送通知（没有 id，不等待响应）。
    async fn notify(&self, method: &str, params: Value) -> Result<()>;

    /// tools/list，自动跟随 nextCursor 分页。
    async fn list_tools(&self) -> Result<Vec<McpToolInfo>> {
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

    /// tools/call。
    async fn call_tool(&self, tool: &str, arguments: Value) -> Result<(String, bool)> {
        let result = self
            .request(
                "tools/call",
                json!({ "name": tool, "arguments": arguments }),
            )
            .await?;
        Ok(render_call_result(&result))
    }
}

/// Streamable HTTP（含 SSE 响应）MCP 客户端。
pub struct McpHttpClient {
    client: reqwest::Client,
    name: String,
    url: String,
    headers: HashMap<String, String>,
    session_id: Mutex<Option<String>>,
    next_id: AtomicI64,
    timeout: Duration,
    capabilities: std::sync::OnceLock<Value>,
}

impl McpHttpClient {
    pub async fn connect(name: &str, config: &McpServerConfig) -> Result<Arc<Self>> {
        let url = config
            .url
            .clone()
            .context("http 传输的 mcp 服务器缺少 url")?;
        let client = Arc::new(Self {
            client: reqwest::Client::new(),
            name: name.to_string(),
            url,
            headers: config.headers.clone(),
            session_id: Mutex::new(None),
            next_id: AtomicI64::new(1),
            timeout: request_timeout(),
            capabilities: std::sync::OnceLock::new(),
        });
        let handshake = client
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
            .with_context(|| format!("mcp http server {name} failed to initialize"))?;
        let _ = client.capabilities.set(
            handshake
                .get("capabilities")
                .cloned()
                .unwrap_or(Value::Null),
        );
        client
            .notify("notifications/initialized", json!({}))
            .await?;
        Ok(client)
    }

    fn build_request(&self, body: Value) -> reqwest::RequestBuilder {
        let mut builder = self
            .client
            .post(&self.url)
            .header("content-type", "application/json")
            .header("accept", "application/json, text/event-stream");
        for (name, value) in &self.headers {
            builder = builder.header(name, value);
        }
        builder.json(&body)
    }

    async fn notify(&self, method: &str, params: Value) -> Result<()> {
        let response = self
            .build_request(json!({"jsonrpc": "2.0", "method": method, "params": params}))
            .send()
            .await
            .context("mcp http notification failed")?;
        self.remember_session(&response).await;
        Ok(())
    }

    async fn remember_session(&self, response: &reqwest::Response) {
        if let Some(session) = response
            .headers()
            .get("mcp-session-id")
            .and_then(|value| value.to_str().ok())
        {
            *self.session_id.lock().await = Some(session.to_string());
        }
    }
}

#[async_trait]
impl McpSession for McpHttpClient {
    fn name(&self) -> &str {
        &self.name
    }

    async fn notify(&self, method: &str, params: Value) -> Result<()> {
        McpHttpClient::notify(self, method, params).await
    }

    fn capabilities(&self) -> Value {
        self.capabilities.get().cloned().unwrap_or(Value::Null)
    }

    async fn request(&self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let mut builder = self.build_request(json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        }));
        if let Some(session) = self.session_id.lock().await.clone() {
            builder = builder.header("mcp-session-id", session);
        }
        let response = tokio::time::timeout(self.timeout, builder.send())
            .await
            .map_err(|_| anyhow::anyhow!("mcp {method} timed out"))?
            .context("mcp http request failed")?;
        self.remember_session(&response).await;
        let status = response.status();
        let content_type = response
            .headers()
            .get("content-type")
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_string();
        let body = response.text().await.unwrap_or_default();
        if !status.is_success() {
            bail!("mcp http {method} returned {status}: {body}");
        }
        let payload = if content_type.contains("text/event-stream") {
            find_sse_response(&body, id)
                .with_context(|| format!("mcp http {method} 的 SSE 响应里没有 id={id} 的结果"))?
        } else {
            serde_json::from_str::<Value>(&body)
                .with_context(|| format!("mcp http {method} 返回了非法 JSON"))?
        };
        if let Some(error) = payload.get("error").filter(|value| !value.is_null()) {
            bail!("mcp {method} failed: {error}");
        }
        Ok(payload.get("result").cloned().unwrap_or(Value::Null))
    }
}

/// 从 SSE 文本里取出 id 匹配的 JSON-RPC 响应。纯函数。
pub fn find_sse_response(body: &str, id: i64) -> Option<Value> {
    let mut decoder = SseBuffer::new();
    for payload in decoder.push(body) {
        let trimmed = payload.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<Value>(trimmed) else {
            continue;
        };
        let matches = value
            .get("id")
            .and_then(|value| {
                value
                    .as_i64()
                    .or_else(|| value.as_str().and_then(|text| text.parse::<i64>().ok()))
            })
            .map(|value| value == id)
            .unwrap_or(false);
        if matches {
            return Some(value);
        }
    }
    None
}

/// 一个已握手的 MCP 服务器连接（stdio 传输）。
pub struct McpClient {
    name: String,
    stdin: Mutex<ChildStdin>,
    pending: PendingMap,
    next_id: AtomicI64,
    timeout: Duration,
    capabilities: std::sync::OnceLock<Value>,
}

impl McpClient {
    /// 启动服务器进程并完成 initialize 握手。
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
            .with_context(|| format!("failed to start mcp server {name} ({})", config.command))?;
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
                    // 通知（例如 logging），忽略。
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
            capabilities: std::sync::OnceLock::new(),
        });

        let handshake = client
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
            .with_context(|| format!("mcp server {name} failed to initialize"))?;
        let _ = client.capabilities.set(
            handshake
                .get("capabilities")
                .cloned()
                .unwrap_or(Value::Null),
        );
        client
            .notify("notifications/initialized", json!({}))
            .await?;
        Ok(client)
    }
}

#[async_trait]
impl McpSession for McpClient {
    fn name(&self) -> &str {
        &self.name
    }

    async fn notify(&self, method: &str, params: Value) -> Result<()> {
        let payload = json!({"jsonrpc": "2.0", "method": method, "params": params});
        let mut stdin = self.stdin.lock().await;
        stdin
            .write_all(serde_json::to_string(&payload)?.as_bytes())
            .await?;
        // 换行符用字节写入，避免转义层级出错。
        stdin.write_all(&[10u8]).await?;
        stdin.flush().await?;
        Ok(())
    }

    fn capabilities(&self) -> Value {
        self.capabilities.get().cloned().unwrap_or(Value::Null)
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
            stdin.write_all(b"").await?;
            stdin.write_all(&[10u8]).await?;
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
}

/// 把 MCP 工具适配成内置工具同款接口。
pub struct McpTool {
    session: Arc<dyn McpSession>,
    exposed_name: String,
    server_tool: String,
    description: String,
    schema: Value,
    read_only: bool,
}

impl McpTool {
    pub fn new(session: Arc<dyn McpSession>, info: &McpToolInfo) -> Self {
        Self {
            exposed_name: exposed_tool_name(session.name(), &info.name),
            server_tool: info.name.clone(),
            description: if info.description.trim().is_empty() {
                format!("MCP 工具 {}（由服务器 {} 提供）", info.name, session.name())
            } else {
                format!(
                    "{} [MCP 服务器 {} 提供]",
                    info.description.trim(),
                    session.name()
                )
            },
            schema: info.input_schema.clone(),
            read_only: info.read_only,
            session,
        }
    }

    pub fn server(&self) -> &str {
        self.session.name()
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
        let (content, is_error) = self.session.call_tool(&self.server_tool, input).await?;
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
        let session: Result<Arc<dyn McpSession>> = match config.transport_kind() {
            McpTransportKind::Http => McpHttpClient::connect(&name, &config)
                .await
                .map(|client| client as Arc<dyn McpSession>),
            McpTransportKind::Stdio => McpClient::connect(&name, &config, cwd)
                .await
                .map(|client| client as Arc<dyn McpSession>),
        };
        let session = match session {
            Ok(session) => session,
            Err(error) => {
                outcome.errors.push((name, error.to_string()));
                continue;
            }
        };
        match session.list_tools().await {
            Ok(tools) => {
                outcome.servers.push((name.clone(), tools.len()));
                for info in &tools {
                    let mut tool = McpTool::new(session.clone(), info);
                    // 服务器级只读声明：该服务器提供的全部工具按只读处理。
                    tool.read_only |= config.read_only;
                    outcome.tools.push(Arc::new(tool));
                }
                for tool in auxiliary_tools(session.clone()) {
                    outcome.tools.push(tool);
                }
            }
            Err(error) => outcome.errors.push((name, error.to_string())),
        }
    }
    outcome
}

/// 当服务器声明 resources / prompts 能力时，补上对应的读取工具。
fn auxiliary_tools(session: Arc<dyn McpSession>) -> Vec<Arc<dyn Tool>> {
    let capabilities = session.capabilities();
    let mut tools: Vec<Arc<dyn Tool>> = Vec::new();
    for (capability, kinds) in [
        (
            "resources",
            vec![McpAuxKind::ListResources, McpAuxKind::ReadResource],
        ),
        (
            "prompts",
            vec![McpAuxKind::ListPrompts, McpAuxKind::GetPrompt],
        ),
    ] {
        if capabilities.get(capability).is_none() {
            continue;
        }
        for kind in kinds {
            tools.push(Arc::new(McpAuxTool::new(session.clone(), kind)));
        }
    }
    tools
}

/// resources / prompts 这类"读取型"MCP 能力的工具包装。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum McpAuxKind {
    ListResources,
    ReadResource,
    ListPrompts,
    GetPrompt,
}

pub struct McpAuxTool {
    session: Arc<dyn McpSession>,
    kind: McpAuxKind,
    exposed_name: String,
}

impl McpAuxTool {
    pub fn new(session: Arc<dyn McpSession>, kind: McpAuxKind) -> Self {
        let exposed_name = exposed_tool_name(session.name(), kind.tool_suffix());
        Self {
            session,
            kind,
            exposed_name,
        }
    }
}

impl McpAuxKind {
    fn tool_suffix(self) -> &'static str {
        match self {
            Self::ListResources => "list_resources",
            Self::ReadResource => "read_resource",
            Self::ListPrompts => "list_prompts",
            Self::GetPrompt => "get_prompt",
        }
    }

    fn description(self) -> &'static str {
        match self {
            Self::ListResources => "列出该 MCP 服务器暴露的资源（URI 与名称）",
            Self::ReadResource => "按 URI 读取该 MCP 服务器的资源内容",
            Self::ListPrompts => "列出该 MCP 服务器提供的提示词模板",
            Self::GetPrompt => "按名称渲染该 MCP 服务器的提示词模板",
        }
    }

    fn method(self) -> &'static str {
        match self {
            Self::ListResources => "resources/list",
            Self::ReadResource => "resources/read",
            Self::ListPrompts => "prompts/list",
            Self::GetPrompt => "prompts/get",
        }
    }
}

#[async_trait]
impl Tool for McpAuxTool {
    fn name(&self) -> &str {
        &self.exposed_name
    }

    fn description(&self) -> &str {
        self.kind.description()
    }

    fn input_schema(&self) -> Value {
        match self.kind {
            McpAuxKind::ReadResource => json!({
                "type": "object",
                "properties": { "uri": { "type": "string" } },
                "required": ["uri"],
            }),
            McpAuxKind::GetPrompt => json!({
                "type": "object",
                "properties": {
                    "name": { "type": "string" },
                    "arguments": { "type": "object" },
                },
                "required": ["name"],
            }),
            _ => json!({ "type": "object", "properties": {} }),
        }
    }

    fn is_read_only(&self, _input: &Value) -> bool {
        true
    }

    async fn call(&self, input: Value, _ctx: &mut ToolContext) -> Result<ToolOutput> {
        let params = match self.kind {
            McpAuxKind::ReadResource => json!({
                "uri": input.get("uri").and_then(|value| value.as_str()).unwrap_or_default(),
            }),
            McpAuxKind::GetPrompt => json!({
                "name": input.get("name").and_then(|value| value.as_str()).unwrap_or_default(),
                "arguments": input.get("arguments").cloned().unwrap_or_else(|| json!({})),
            }),
            _ => json!({}),
        };
        let result = self.session.request(self.kind.method(), params).await?;
        Ok(ToolOutput::ok(render_aux_result(&result)))
    }
}

/// 把 resources / prompts 的结果渲染成可读文本。纯函数。
pub fn render_aux_result(result: &Value) -> String {
    if let Some(resources) = result.get("resources").and_then(Value::as_array) {
        if resources.is_empty() {
            return "（没有可用资源）".to_string();
        }
        return resources
            .iter()
            .map(|resource| {
                format!(
                    "{}  {}",
                    resource
                        .get("uri")
                        .and_then(Value::as_str)
                        .unwrap_or("(no uri)"),
                    resource
                        .get("name")
                        .and_then(Value::as_str)
                        .or_else(|| resource.get("description").and_then(Value::as_str))
                        .unwrap_or("")
                )
                .trim_end()
                .to_string()
            })
            .collect::<Vec<_>>()
            .join("\n");
    }
    if let Some(contents) = result.get("contents").and_then(Value::as_array) {
        return contents
            .iter()
            .filter_map(|entry| entry.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n");
    }
    if let Some(prompts) = result.get("prompts").and_then(Value::as_array) {
        if prompts.is_empty() {
            return "（没有可用提示词模板）".to_string();
        }
        return prompts
            .iter()
            .map(|prompt| {
                format!(
                    "{}  {}",
                    prompt
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or("(no name)"),
                    prompt
                        .get("description")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                )
                .trim_end()
                .to_string()
            })
            .collect::<Vec<_>>()
            .join("\n");
    }
    if let Some(messages) = result.get("messages").and_then(Value::as_array) {
        return messages
            .iter()
            .map(|message| {
                let role = message
                    .get("role")
                    .and_then(Value::as_str)
                    .unwrap_or("user");
                let text = message
                    .get("content")
                    .and_then(|content| content.get("text"))
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                format!("[{role}] {text}")
            })
            .collect::<Vec<_>>()
            .join("\n");
    }
    result.to_string()
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
    fn sse_payloads_are_matched_by_request_id() {
        let sep = char::from_u32(10).unwrap();
        let first = json!({"jsonrpc": "2.0", "id": 1, "result": {"ok": true}}).to_string();
        let second = json!({"jsonrpc": "2.0", "id": 2, "result": {"other": 1}}).to_string();
        let body = format!("event: message{sep}data: {first}{sep}{sep}data: {second}{sep}{sep}");
        let found = find_sse_response(&body, 2).unwrap();
        assert_eq!(found["result"]["other"], 1);
        assert!(find_sse_response(&body, 9).is_none());
        assert!(find_sse_response("", 1).is_none());
    }

    #[test]
    fn auxiliary_results_render_for_every_capability() {
        let resources = json!({"resources": [
            {"uri": "file:///a.txt", "name": "a"},
            {"uri": "file:///b.txt"}
        ]});
        let rendered = render_aux_result(&resources);
        assert!(rendered.contains("file:///a.txt"));
        assert!(rendered.contains("file:///b.txt"));
        assert!(render_aux_result(&json!({"resources": []})).contains("没有可用资源"));

        let contents = json!({"contents": [{"text": "first"}, {"text": "second"}]});
        assert_eq!(
            render_aux_result(&contents),
            "first
second"
        );

        let prompts = json!({"prompts": [{"name": "review", "description": "审查"}]});
        assert!(render_aux_result(&prompts).contains("review"));

        let messages = json!({"messages": [{"role": "user", "content": {"text": "hi"}}]});
        assert!(render_aux_result(&messages).contains("[user] hi"));

        assert!(render_aux_result(&json!({"weird": 1})).contains("weird"));
    }

    #[test]
    fn http_servers_are_detected_from_config() {
        let http = McpServerConfig {
            command: String::new(),
            args: Vec::new(),
            env: HashMap::new(),
            cwd: None,
            read_only: false,
            enabled: None,
            transport: Some("http".to_string()),
            url: Some("http://127.0.0.1:9000/mcp".to_string()),
            headers: HashMap::new(),
        };
        assert_eq!(http.transport_kind(), McpTransportKind::Http);
        assert!(http.is_enabled());

        let implicit = McpServerConfig {
            transport: None,
            ..http.clone()
        };
        assert_eq!(implicit.transport_kind(), McpTransportKind::Http);

        let broken = McpServerConfig {
            url: None,
            ..http.clone()
        };
        assert!(!broken.is_enabled());

        let stdio = McpServerConfig {
            command: "npx".to_string(),
            transport: None,
            url: None,
            ..broken
        };
        assert_eq!(stdio.transport_kind(), McpTransportKind::Stdio);
        assert!(stdio.is_enabled());
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
                transport: None,
                url: None,
                headers: HashMap::new(),
            },
            root,
        )
        .await
        .unwrap();
        let error = client.call_tool("echo", json!({})).await.unwrap_err();
        assert!(error.to_string().contains("method not found"), "{error}");
    }
}
