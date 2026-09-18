//! Legacy MCP HTTP+SSE transport: GET event stream, endpoint event, POST messages.
use crate::mcp::{rpc_id, McpServerConfig, McpSession};
use anyhow::{Context, Result};
use async_trait::async_trait;
use futures_util::StreamExt;
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicI64, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::sync::{oneshot, Mutex};

type Pending = Arc<Mutex<HashMap<i64, oneshot::Sender<Result<Value, String>>>>>;
struct LegacySession {
    name: String,
    url: String,
    endpoint: String,
    headers: HashMap<String, String>,
    http: reqwest::Client,
    pending: Pending,
    next: AtomicI64,
    capabilities: std::sync::OnceLock<Value>,
    reader: tokio::task::JoinHandle<()>,
}
impl Drop for LegacySession {
    fn drop(&mut self) {
        self.reader.abort();
    }
}

#[derive(Default)]
struct Decoder {
    bytes: Vec<u8>,
    event: String,
    data: Vec<String>,
    skip_lf: bool,
}
impl Decoder {
    fn push(&mut self, chunk: &[u8]) -> Vec<(String, String)> {
        let mut events = Vec::new();
        for &byte in chunk {
            if self.skip_lf {
                self.skip_lf = false;
                if byte == b'\n' {
                    continue;
                }
            }
            if byte != b'\r' && byte != b'\n' {
                self.bytes.push(byte);
                continue;
            }
            self.skip_lf = byte == b'\r';
            let line = String::from_utf8_lossy(&self.bytes).to_string();
            self.bytes.clear();
            if line.is_empty() {
                if !self.data.is_empty() {
                    events.push((self.event.clone(), self.data.join("\n")));
                    self.data.clear();
                }
                self.event.clear();
            } else if line == "data" {
                self.data.push(String::new());
            } else if let Some(data) = line.strip_prefix("data:") {
                self.data
                    .push(data.strip_prefix(' ').unwrap_or(data).to_string());
            } else if let Some(event) = line.strip_prefix("event:") {
                self.event = event.strip_prefix(' ').unwrap_or(event).to_string();
            }
        }
        events
    }
}

#[cfg(test)]
mod tests {
    use super::Decoder;
    #[test]
    fn mixed_line_endings_and_split_unicode() {
        let source =
            "event: endpoint\rdata: /消息\r\revent: message\r\ndata:  one\ndata: two\r\n\r\n"
                .as_bytes();
        for split in 0..=source.len() {
            let mut decoder = Decoder::default();
            let mut events = decoder.push(&source[..split]);
            events.extend(decoder.push(&source[split..]));
            assert_eq!(
                events,
                vec![
                    ("endpoint".into(), "/消息".into()),
                    ("message".into(), " one\ntwo".into())
                ]
            );
        }
    }
}

pub async fn connect(name: &str, config: &McpServerConfig) -> Result<Arc<dyn McpSession>> {
    let url = config.url.as_ref().context("SSE MCP requires url")?;
    let http = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .redirect(reqwest::redirect::Policy::none())
        .build()?;
    let mut request = http.get(url).header("accept", "text/event-stream");
    for (name, value) in &config.headers {
        request = request.header(name, value);
    }
    if let Some(token) = crate::mcp_oauth::access_token(url).await? {
        request = request.bearer_auth(token);
    }
    let response = tokio::time::timeout(Duration::from_secs(30), request.send())
        .await??
        .error_for_status()?;
    let mut stream = response.bytes_stream();
    let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
    let reader_pending = pending.clone();
    let (endpoint_tx, endpoint_rx) = oneshot::channel();
    let reader = tokio::spawn(async move {
        let mut decoder = Decoder::default();
        let mut endpoint_tx = Some(endpoint_tx);
        while let Some(Ok(chunk)) = stream.next().await {
            for (event, data) in decoder.push(&chunk) {
                if event == "endpoint" {
                    if let Some(sender) = endpoint_tx.take() {
                        let _ = sender.send(data);
                    }
                } else if let Ok(value) = serde_json::from_str::<Value>(&data) {
                    if let Some(id) = rpc_id(&value) {
                        if let Some(sender) = reader_pending.lock().await.remove(&id) {
                            let _ = sender.send(
                                if let Some(error) = value.get("error").filter(|e| !e.is_null()) {
                                    Err(error.to_string())
                                } else {
                                    Ok(value["result"].clone())
                                },
                            );
                        }
                    }
                }
            }
        }
        for (_, sender) in reader_pending.lock().await.drain() {
            let _ = sender.send(Err("MCP SSE disconnected".into()));
        }
    });
    let endpoint = match tokio::time::timeout(Duration::from_secs(30), endpoint_rx).await {
        Ok(Ok(endpoint)) => endpoint,
        _ => {
            reader.abort();
            anyhow::bail!("MCP SSE endpoint event missing");
        }
    };
    let base = url::Url::parse(url)?;
    let endpoint = base.join(&endpoint)?;
    if endpoint.origin() != base.origin() {
        reader.abort();
        anyhow::bail!("MCP SSE endpoint must have the same origin");
    }
    let session = Arc::new(LegacySession {
        name: name.into(),
        url: url.clone(),
        endpoint: endpoint.to_string(),
        headers: config.headers.clone(),
        http,
        pending,
        next: AtomicI64::new(1),
        capabilities: std::sync::OnceLock::new(),
        reader,
    });
    let handshake = session.request("initialize", json!({"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"wonderland","version":env!("CARGO_PKG_VERSION")}})).await?;
    let _ = session.capabilities.set(handshake["capabilities"].clone());
    session
        .notify("notifications/initialized", json!({}))
        .await?;
    Ok(session)
}
impl LegacySession {
    async fn post(&self, body: Value) -> Result<()> {
        let mut request = self
            .http
            .post(&self.endpoint)
            .json(&body)
            .timeout(Duration::from_secs(30));
        for (name, value) in &self.headers {
            request = request.header(name, value);
        }
        if let Some(token) = crate::mcp_oauth::access_token(&self.url).await? {
            request = request.bearer_auth(token);
        }
        request.send().await?.error_for_status()?;
        Ok(())
    }
}
#[async_trait]
impl McpSession for LegacySession {
    fn name(&self) -> &str {
        &self.name
    }
    fn capabilities(&self) -> Value {
        self.capabilities.get().cloned().unwrap_or(Value::Null)
    }
    async fn notify(&self, method: &str, params: Value) -> Result<()> {
        self.post(json!({"jsonrpc":"2.0","method":method,"params":params}))
            .await
    }
    async fn request(&self, method: &str, params: Value) -> Result<Value> {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let (sender, receiver) = oneshot::channel();
        self.pending.lock().await.insert(id, sender);
        let result = async {
            self.post(json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}))
                .await?;
            tokio::time::timeout(Duration::from_secs(30), receiver)
                .await
                .context("MCP SSE timed out")?
                .context("MCP SSE closed")?
                .map_err(anyhow::Error::msg)
        }
        .await;
        self.pending.lock().await.remove(&id);
        result
    }
}
