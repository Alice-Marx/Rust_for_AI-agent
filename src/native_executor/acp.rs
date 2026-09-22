//! 通用 ACP（Agent Client Protocol）会话框架。
//!
//! 从 DeepSeek Harness transport 泛化：JSON-RPC 帧循环、initialize/session/prompt
//! 生命周期、流式更新、一次性权限与取消语义是 ACP 的公共部分，由 `AcpRunner`
//! 统一实现；方言差异（握手身份、模型/档位配置值、权限选项形状、停止原因、
//! 提示文本）由 `AcpDialect` 提供。每个官方工具一个 dialect 实现，禁止在
//! dialect 之间互相 fallback。

use crate::native_executor::{emit, NativeControl, NativeEvent, NativeRequest, NativeResult};
use anyhow::{bail, ensure, Context, Result};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use tokio::io::AsyncWriteExt;
use tokio::sync::mpsc;

use super::{empty_result, MAX_FRAME_BYTES, MAX_OUTPUT_BYTES};

const MAX_PENDING: usize = 64;
const MAX_INTERACTIONS: usize = 4096;

/// Per-tool protocol dialect. Every method must fail closed: an unrecognized
/// handshake, drifted configuration or unsupported stop reason is an error,
/// never a guess.
pub(super) trait AcpDialect: Send {
    /// Human label used in error messages ("DeepSeek", "MiMo").
    fn label(&self) -> &'static str;
    /// Protocol identifier reported in `NativeEvent::Started`.
    fn protocol(&self) -> &'static str;
    /// Prefix for emitted permission request IDs.
    fn request_prefix(&self) -> &'static str;
    /// Status note emitted after initialization.
    fn status_note(&self) -> String;
    /// Verify the `initialize` result: agent identity and capabilities.
    fn verify_initialize(&self, init: &Value) -> Result<()>;
    /// Config option id used to pin the exact model.
    fn model_config_id(&self) -> &'static str;
    /// Wire value for the model config option.
    fn model_value(&self, req: &NativeRequest) -> String;
    /// Config option id used to pin the reasoning effort, when supported.
    fn effort_config_id(&self) -> Option<&'static str>;
    /// Verify reported config options against the pinned request.
    fn verify_config_options(
        &self,
        options: &Value,
        model: &str,
        effort: Option<&str>,
    ) -> Result<()>;
    /// Select the (allow, reject) optionIds from a permission request's
    /// options array; one-shot semantics only.
    fn permission_selection(&self, options: &[Value]) -> Result<(String, String)>;
    /// Session-update kinds forwarded as tool activity without interpretation.
    fn passthrough_updates(&self) -> &'static [&'static str];
    /// Map the prompt stop reason to a terminal status.
    fn stop_status(&self, reason: Option<&str>) -> Result<String>;
}

struct Pending {
    id: Value,
    tool_id: String,
    allow: String,
    reject: String,
}

pub(super) struct AcpRunner {
    dialect: Box<dyn AcpDialect>,
    stdin: Box<dyn tokio::io::AsyncWrite + Unpin + Send>,
    frames: mpsc::Receiver<anyhow::Result<Value>>,
    controls: mpsc::Receiver<NativeControl>,
    controls_open: bool,
    events: mpsc::Sender<NativeEvent>,
    pending: HashMap<String, Pending>,
    seen: HashSet<String>,
    tools: HashMap<String, Value>,
    next_id: u64,
    result: NativeResult,
    read_only: bool,
    effort: Option<String>,
    enforce_effort: bool,
    streamed_bytes: usize,
}

impl AcpRunner {
    pub(super) fn new(
        dialect: Box<dyn AcpDialect>,
        stdin: Box<dyn tokio::io::AsyncWrite + Unpin + Send>,
        frames: mpsc::Receiver<anyhow::Result<Value>>,
        controls: mpsc::Receiver<NativeControl>,
        events: mpsc::Sender<NativeEvent>,
        req: &NativeRequest,
    ) -> Self {
        Self {
            dialect,
            stdin,
            frames,
            controls,
            controls_open: true,
            events,
            pending: HashMap::new(),
            seen: HashSet::new(),
            tools: HashMap::new(),
            next_id: 1,
            result: empty_result(req, "running"),
            read_only: req.read_only,
            effort: req.reasoning_effort.clone(),
            enforce_effort: false,
            streamed_bytes: 0,
        }
    }

    async fn write(&mut self, value: Value) -> Result<()> {
        let label = self.dialect.label();
        let mut bytes = serde_json::to_vec(&value)?;
        ensure!(
            bytes.len() < MAX_FRAME_BYTES,
            "{label} outgoing ACP frame exceeds limit"
        );
        bytes.push(b'\n');
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            self.stdin.write_all(&bytes).await?;
            self.stdin.flush().await
        })
        .await
        .with_context(|| format!("{label} ACP stdin stopped draining"))??;
        Ok(())
    }

    async fn rpc(&mut self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id;
        self.next_id += 1;
        self.write(json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}))
            .await?;
        loop {
            tokio::select! {
                frame = self.frames.recv() => {
                    let label = self.dialect.label();
                    let frame = frame
                        .with_context(|| format!("{label} ACP reader disconnected"))??;
                    ensure!(frame["jsonrpc"] == "2.0", "{label} ACP envelope mismatch");
                    if frame.get("method").is_some() { self.handle_server(frame).await?; continue; }
                    ensure!(frame.get("id").and_then(Value::as_u64) == Some(id), "unexpected {label} ACP response identity");
                    // Provider error text may echo secret-bearing requests; retain
                    // only the error code, never the arbitrary server message/data.
                    if let Some(error) = frame.get("error") {
                        bail!("{label} ACP {method} failed (code {}); see the official CLI for account diagnostics", error["code"].as_i64().unwrap_or(-32603));
                    }
                    return frame.get("result").cloned().with_context(|| format!("{label} ACP result missing"));
                },
                control = self.controls.recv(), if self.controls_open => {
                    let label = self.dialect.label();
                    match control {
                        Some(NativeControl::Permission{request_id, approve}) => self.resolve(&request_id, approve).await?,
                        Some(NativeControl::Answer{..}) => emit(&self.events, NativeEvent::Status{message:format!("{label} ACP does not expose question answers")}).await?,
                        None => {
                            self.controls_open = false;
                            let keys: Vec<String> = self.pending.keys().cloned().collect();
                            for key in keys { self.resolve(&key, false).await?; }
                        }
                    }
                }
            }
        }
    }

    pub(super) async fn initialize(&mut self, req: &NativeRequest) -> Result<()> {
        let init = self
            .rpc(
                "initialize",
                json!({"protocolVersion":1,"clientInfo":{"name":"wonderland","version":env!("CARGO_PKG_VERSION")},"clientCapabilities":{}}),
            )
            .await?;
        self.dialect.verify_initialize(&init)?;
        let session = self
            .rpc("session/new", json!({"cwd":req.cwd,"mcpServers":[]}))
            .await?;
        let session_id = session["sessionId"]
            .as_str()
            .context("ACP session ID missing")?;
        ensure!(
            uuid::Uuid::parse_str(session_id).is_ok(),
            "invalid ACP session ID"
        );
        self.result.session_id = Some(session_id.into());
        self.dialect
            .verify_config_options(&session["configOptions"], &req.model, None)?;
        emit(
            &self.events,
            NativeEvent::SessionStarted {
                session_id: session_id.into(),
            },
        )
        .await?;
        let selected = self
            .rpc(
                "session/set_config_option",
                json!({"sessionId":session_id,"configId":self.dialect.model_config_id(),"value":self.dialect.model_value(req)}),
            )
            .await?;
        self.dialect
            .verify_config_options(&selected["configOptions"], &req.model, None)?;
        if let (Some(config_id), Some(effort)) = (
            self.dialect.effort_config_id(),
            req.reasoning_effort.as_ref(),
        ) {
            self.enforce_effort = true;
            let selected = self
                .rpc(
                    "session/set_config_option",
                    json!({"sessionId":session_id,"configId":config_id,"value":effort}),
                )
                .await?;
            self.dialect.verify_config_options(
                &selected["configOptions"],
                &req.model,
                req.reasoning_effort.as_deref(),
            )?;
        }
        Ok(())
    }

    pub(super) async fn run(&mut self, req: &NativeRequest) -> Result<String> {
        emit(
            &self.events,
            NativeEvent::Started {
                app_id: req.app_id.clone(),
                model: req.model.clone(),
                protocol: self.dialect.protocol().into(),
            },
        )
        .await?;
        self.initialize(req).await?;
        let note = self.dialect.status_note();
        emit(&self.events, NativeEvent::Status { message: note }).await?;
        let result = self
            .rpc(
                "session/prompt",
                json!({"sessionId":self.result.session_id,"prompt":[{"type":"text","text":req.prompt}]}),
            )
            .await?;
        let label = self.dialect.label();
        ensure!(
            self.pending.is_empty(),
            "{label} prompt settled with unresolved permissions"
        );
        let status = self.dialect.stop_status(result["stopReason"].as_str())?;
        if status == "completed" {
            ensure!(
                self.tools.is_empty(),
                "{label} prompt ended with active tools"
            );
        }
        Ok(status)
    }

    fn check_session(&self, params: &Value) -> Result<()> {
        let label = self.dialect.label();
        ensure!(
            self.result.session_id.is_some()
                && params["sessionId"].as_str() == self.result.session_id.as_deref(),
            "{label} ACP session mismatch"
        );
        Ok(())
    }

    async fn handle_server(&mut self, frame: Value) -> Result<()> {
        match frame["method"].as_str() {
            Some("session/update") if frame.get("id").is_none() => {
                self.check_session(&frame["params"])?;
                self.update(&frame["params"]["update"]).await
            }
            Some("session/request_permission") if frame.get("id").is_some() => {
                self.permission(frame).await
            }
            _ if frame.get("id").is_some() => {
                self.write(json!({"jsonrpc":"2.0","id":frame["id"],"error":{"code":-32601,"message":"Client method is unsupported"}})).await
            }
            _ => bail!("unsupported ACP notification"),
        }
    }

    async fn update(&mut self, update: &Value) -> Result<()> {
        let label = self.dialect.label();
        self.streamed_bytes = self
            .streamed_bytes
            .checked_add(serde_json::to_vec(update)?.len())
            .with_context(|| format!("{label} output length overflow"))?;
        ensure!(
            self.streamed_bytes <= MAX_OUTPUT_BYTES,
            "{label} output exceeds limit"
        );
        let kind = update["sessionUpdate"].as_str();
        if let Some(kind) = kind {
            if self.dialect.passthrough_updates().contains(&kind) {
                emit(
                    &self.events,
                    NativeEvent::ToolActivity {
                        data: update.clone(),
                    },
                )
                .await?;
                return Ok(());
            }
        }
        match kind {
            Some("config_option_update") => self.dialect.verify_config_options(
                &update["configOptions"],
                &self.result.model,
                if self.enforce_effort {
                    self.effort.as_deref()
                } else {
                    None
                },
            ),
            Some("agent_message_chunk" | "agent_thought_chunk") => {
                ensure!(
                    update["content"]["type"] == "text",
                    "unsupported {label} output content"
                );
                let text = update["content"]["text"]
                    .as_str()
                    .with_context(|| format!("{label} text chunk missing"))?;
                let event = if update["sessionUpdate"] == "agent_message_chunk" {
                    self.result.output.push_str(text);
                    NativeEvent::TextDelta { text: text.into() }
                } else {
                    NativeEvent::ReasoningDelta { text: text.into() }
                };
                emit(&self.events, event).await
            }
            Some("tool_call" | "tool_call_update") => {
                let id = update["toolCallId"]
                    .as_str()
                    .with_context(|| format!("{label} tool identity missing"))?;
                ensure!(
                    !id.is_empty() && id.len() <= 256,
                    "invalid {label} tool identity"
                );
                if update["sessionUpdate"] == "tool_call" {
                    ensure!(
                        update["status"] == "in_progress",
                        "{label} tool start has an invalid status"
                    );
                    ensure!(
                        self.tools.len() < MAX_PENDING && !self.tools.contains_key(id),
                        "duplicate or excessive {label} tool calls"
                    );
                    // Keep only bounded attribution, never full tool result bodies.
                    self.tools.insert(
                        id.into(),
                        json!({"title":update["title"],"rawInput":update["rawInput"]}),
                    );
                } else {
                    ensure!(
                        matches!(update["status"].as_str(), Some("completed" | "failed")),
                        "{label} tool result has an invalid status"
                    );
                    ensure!(
                        self.tools.remove(id).is_some(),
                        "unknown or repeated {label} tool result"
                    );
                    let stale: Vec<_> = self
                        .pending
                        .iter()
                        .filter(|(_, pending)| pending.tool_id == id)
                        .map(|(key, _)| key.clone())
                        .collect();
                    for key in stale {
                        self.resolve(&key, false).await?;
                    }
                }
                emit(
                    &self.events,
                    NativeEvent::ToolActivity {
                        data: update.clone(),
                    },
                )
                .await
            }
            Some("usage_update") => {
                ensure!(
                    update["used"].as_u64().is_some() && update["size"].as_u64().is_some(),
                    "invalid {label} context usage"
                );
                let mut data =
                    json!({"kind":"context_occupancy","used":update["used"],"size":update["size"]});
                if update["cost"]["amount"]
                    .as_f64()
                    .is_some_and(f64::is_finite)
                    && update["cost"]["currency"].as_str() == Some("USD")
                {
                    // Harness-reported accumulated cost; whether the provider
                    // confirms it is a separate attestation question.
                    data["reported_cost_usd"] = update["cost"]["amount"].clone();
                }
                self.result.usage = Some(data.clone());
                emit(&self.events, NativeEvent::Usage { data }).await
            }
            _ => bail!("unsupported {label} ACP session update"),
        }
    }

    async fn permission(&mut self, frame: Value) -> Result<()> {
        let label = self.dialect.label();
        let prefix = self.dialect.request_prefix();
        let params = &frame["params"];
        self.check_session(params)?;
        let id = frame["id"].clone();
        ensure!(
            (id.is_string() && id.as_str().is_some_and(|s| !s.is_empty() && s.len() <= 256))
                || id.is_i64()
                || id.is_u64(),
            "invalid {label} permission request ID"
        );
        ensure!(
            self.seen.len() < MAX_INTERACTIONS && self.seen.insert(id.to_string()),
            "duplicate or excessive {label} permission requests"
        );
        ensure!(
            self.pending.len() < MAX_PENDING,
            "too many pending {label} permissions"
        );
        let options = params["options"]
            .as_array()
            .with_context(|| format!("{label} permission options missing"))?;
        let (allow, reject) = self.dialect.permission_selection(options)?;
        let tool = params["toolCall"]["toolCallId"]
            .as_str()
            .with_context(|| format!("{label} permission tool ID missing"))?;
        let attribution = self.tools.get(tool).cloned();
        let request_id = format!("{prefix}-{}", uuid::Uuid::new_v4());
        self.pending.insert(
            request_id.clone(),
            Pending {
                id,
                tool_id: tool.into(),
                allow,
                reject,
            },
        );
        // Missing attribution, a disconnected UI or read-only mode can never
        // grant authority. A read-only approval button cannot widen the policy.
        if self.read_only || !self.controls_open || attribution.is_none() {
            return self.resolve(&request_id, false).await;
        }
        let data = attribution.unwrap_or(Value::Null);
        let description = format!(
            "{label} requests one-time permission for {}",
            data["title"].as_str().unwrap_or("a tool")
        );
        emit(
            &self.events,
            NativeEvent::PermissionRequested {
                request_id,
                description,
                data,
            },
        )
        .await
    }

    pub(super) async fn resolve(&mut self, request_id: &str, approve: bool) -> Result<()> {
        let Some(pending) = self.pending.remove(request_id) else {
            return Ok(());
        };
        let approved = approve && !self.read_only && self.tools.contains_key(&pending.tool_id);
        let option = if approved {
            pending.allow
        } else {
            pending.reject
        };
        self.write(json!({"jsonrpc":"2.0","id":pending.id,"result":{"outcome":{"outcome":"selected","optionId":option}}})).await?;
        emit(
            &self.events,
            NativeEvent::PermissionResolved {
                request_id: request_id.into(),
                approved,
            },
        )
        .await
    }

    pub(super) async fn shutdown(&mut self) -> Result<()> {
        // Do not depend on the event consumer during cleanup.
        for (_, pending) in self.pending.drain().collect::<Vec<_>>() {
            self.write(json!({"jsonrpc":"2.0","id":pending.id,"result":{"outcome":{"outcome":"cancelled"}}})).await?;
        }
        if let Some(session) = self.result.session_id.clone() {
            self.write(
                json!({"jsonrpc":"2.0","method":"session/cancel","params":{"sessionId":session}}),
            )
            .await?;
            self.write(json!({"jsonrpc":"2.0","id":self.next_id,"method":"session/close","params":{"sessionId":session}})).await?;
        }
        self.stdin.shutdown().await?;
        Ok(())
    }

    /// Consumes the runner, closing the child's stdin pipe (the boxed writer
    /// is dropped) and returning the accumulated result.
    pub(super) fn into_result(mut self) -> NativeResult {
        self.stdin = Box::new(tokio::io::sink());
        self.result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::native_executor::deepseek::DeepSeekDialect;
    use crate::native_executor::{NativeControl, NativeEvent, NativeRequest};
    use std::{
        pin::Pin,
        sync::{Arc, Mutex},
        task::{Context as TaskContext, Poll},
    };
    use tokio::io::AsyncWrite;
    const SESSION: &str = "bec6941c-a1ce-4cfe-bb28-c1c396f9f1e5";

    #[derive(Clone, Default)]
    struct Recorded(Arc<Mutex<Vec<u8>>>);
    impl AsyncWrite for Recorded {
        fn poll_write(
            self: Pin<&mut Self>,
            _: &mut TaskContext<'_>,
            data: &[u8],
        ) -> Poll<std::io::Result<usize>> {
            self.0.lock().unwrap().extend_from_slice(data);
            Poll::Ready(Ok(data.len()))
        }
        fn poll_flush(self: Pin<&mut Self>, _: &mut TaskContext<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }
        fn poll_shutdown(
            self: Pin<&mut Self>,
            _: &mut TaskContext<'_>,
        ) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }
    impl Recorded {
        fn frames(&self) -> Vec<Value> {
            self.0
                .lock()
                .unwrap()
                .split(|b| *b == b'\n')
                .filter(|v| !v.is_empty())
                .map(|v| serde_json::from_slice(v).unwrap())
                .collect()
        }
    }
    fn request(read_only: bool) -> NativeRequest {
        NativeRequest {
            app_id: "deepseek".into(),
            model: "deepseek-v4-pro".into(),
            cwd: std::env::current_dir().unwrap(),
            prompt: "inspect only".into(),
            read_only,
            max_duration_secs: 30,
            reasoning_effort: Some("low".into()),
            config_path: None,
        }
    }
    fn options(model: &str, effort: &str) -> Value {
        json!([{"id":"model","currentValue":json!(["deepseek-official", model]).to_string()},{"id":"reasoning_effort","currentValue":effort}])
    }
    fn fixture(
        req: &NativeRequest,
    ) -> (
        AcpRunner,
        Recorded,
        mpsc::Sender<Result<Value>>,
        mpsc::Sender<NativeControl>,
        mpsc::Receiver<NativeEvent>,
    ) {
        let written = Recorded::default();
        let (tx, rx) = mpsc::channel(32);
        let (control_tx, control_rx) = mpsc::channel(32);
        let (event_tx, event_rx) = mpsc::channel(32);
        (
            AcpRunner::new(
                Box::new(DeepSeekDialect),
                Box::new(written.clone()),
                rx,
                control_rx,
                event_tx,
                req,
            ),
            written,
            tx,
            control_tx,
            event_rx,
        )
    }
    async fn handshake(tx: &mpsc::Sender<Result<Value>>, req: &NativeRequest) {
        for value in [
            json!({"jsonrpc":"2.0","id":1,"result":{"protocolVersion":1,"agentInfo":{"name":"deepseek-harness-acp","version":"0.0.1"},"agentCapabilities":{"sessionCapabilities":{"close":{}}}}}),
            json!({"jsonrpc":"2.0","id":2,"result":{"sessionId":SESSION,"configOptions":options(&req.model,"high")}}),
            json!({"jsonrpc":"2.0","id":3,"result":{"configOptions":options(&req.model,"high")}}),
            json!({"jsonrpc":"2.0","id":4,"result":{"configOptions":options(&req.model,"low")}}),
        ] {
            tx.send(Ok(value)).await.unwrap();
        }
    }
    fn permission(id: u64) -> Value {
        json!({"jsonrpc":"2.0","id":id,"method":"session/request_permission","params":{"sessionId":SESSION,"toolCall":{"toolCallId":"call-1"},"options":[{"kind":"allow_once","optionId":"allow-once"},{"kind":"reject_once","optionId":"reject-once"}]}})
    }

    #[tokio::test]
    async fn handshake_pins_provider_model_and_effort_before_prompt() {
        let req = request(true);
        let (mut runner, written, tx, _controls, mut events) = fixture(&req);
        handshake(&tx, &req).await;
        tx.send(Ok(json!({"jsonrpc":"2.0","method":"session/update","params":{"sessionId":SESSION,"update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"checked"}}}}))).await.unwrap();
        tx.send(Ok(
            json!({"jsonrpc":"2.0","id":5,"result":{"stopReason":"end_turn"}}),
        ))
        .await
        .unwrap();
        assert_eq!(runner.run(&req).await.unwrap(), "completed");
        assert_eq!(runner.result.output, "checked");
        let frames = written.frames();
        assert_eq!(frames[1]["params"]["mcpServers"], json!([]));
        assert_eq!(
            frames[2]["params"]["value"],
            json!(["deepseek-official", req.model]).to_string()
        );
        assert_eq!(frames[3]["params"]["value"], "low");
        assert_eq!(frames[4]["method"], "session/prompt");
        let mut text = false;
        while let Ok(event) = events.try_recv() {
            if matches!(event, NativeEvent::TextDelta { .. }) {
                text = true;
            }
        }
        assert!(text);
    }

    #[tokio::test]
    async fn config_drift_and_cross_session_updates_fail_closed() {
        let req = request(true);
        let (mut runner, _, _, _, _events) = fixture(&req);
        runner.result.session_id = Some(SESSION.into());
        let wrong_model = json!({"sessionUpdate":"config_option_update","configOptions":options("deepseek-flash","low")});
        assert!(runner.update(&wrong_model).await.is_err());
        assert!(runner.handle_server(json!({"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"another","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"wrong"}}}})).await.is_err());
        assert!(runner.result.output.is_empty());
    }

    #[tokio::test]
    async fn readonly_never_grants_one_shot_and_rejects_replayed_request() {
        let req = request(true);
        let (mut runner, written, _, _, _events) = fixture(&req);
        runner.result.session_id = Some(SESSION.into());
        runner.tools.insert(
            "call-1".into(),
            json!({"title":"pwsh","rawInput":{"command":"write"}}),
        );
        runner.permission(permission(17)).await.unwrap();
        assert_eq!(
            written.frames()[0]["result"]["outcome"]["optionId"],
            "reject-once"
        );
        assert!(runner.permission(permission(17)).await.is_err());
        assert!(runner.pending.is_empty());
    }

    #[tokio::test]
    async fn grants_only_correlated_one_shot_and_ignores_reused_control() {
        let req = request(false);
        let (mut runner, written, _, _, mut events) = fixture(&req);
        runner.result.session_id = Some(SESSION.into());
        runner.tools.insert(
            "call-1".into(),
            json!({"title":"pwsh","rawInput":{"command":"build"}}),
        );
        runner.permission(permission(17)).await.unwrap();
        let NativeEvent::PermissionRequested { request_id, .. } = events.recv().await.unwrap()
        else {
            panic!("missing permission event");
        };
        runner.resolve(&request_id, true).await.unwrap();
        runner.resolve(&request_id, true).await.unwrap();
        assert_eq!(written.frames().len(), 1);
        assert_eq!(
            written.frames()[0]["result"]["outcome"]["optionId"],
            "allow-once"
        );
    }

    #[tokio::test]
    async fn missing_attribution_and_closed_control_channel_deny_permission() {
        let req = request(false);
        let (mut runner, written, _, _, _events) = fixture(&req);
        runner.result.session_id = Some(SESSION.into());
        runner.permission(permission(20)).await.unwrap();
        runner.controls_open = false;
        runner
            .tools
            .insert("call-1".into(), json!({"title":"pwsh"}));
        runner.permission(permission(21)).await.unwrap();
        let frames = written.frames();
        assert_eq!(frames.len(), 2);
        for frame in frames {
            assert_eq!(frame["result"]["outcome"]["optionId"], "reject-once");
        }
    }

    #[tokio::test]
    async fn execution_limit_is_not_reported_as_completion() {
        let req = request(true);
        let (mut runner, _, tx, _controls, _events) = fixture(&req);
        handshake(&tx, &req).await;
        tx.send(Ok(
            json!({"jsonrpc":"2.0","id":5,"result":{"stopReason":"max_tokens"}}),
        ))
        .await
        .unwrap();
        assert!(runner
            .run(&req)
            .await
            .unwrap_err()
            .to_string()
            .contains("incomplete"));
    }

    #[tokio::test]
    async fn cleanup_cancels_permissions_then_closes_session() {
        let req = request(false);
        let (mut runner, written, _, _, _events) = fixture(&req);
        runner.result.session_id = Some(SESSION.into());
        runner.pending.insert(
            "pending".into(),
            Pending {
                id: json!(71),
                tool_id: "call-1".into(),
                allow: "allow-once".into(),
                reject: "reject-once".into(),
            },
        );
        runner.shutdown().await.unwrap();
        let frames = written.frames();
        assert_eq!(frames[0]["result"]["outcome"]["outcome"], "cancelled");
        assert_eq!(frames[1]["method"], "session/cancel");
        assert_eq!(frames[2]["method"], "session/close");
    }

    #[tokio::test]
    async fn tool_completion_revokes_stale_approval_and_rejects_duplicate_terminal() {
        let (mut runner, written, _, _controls, _events) = fixture(&request(false));
        runner.result.session_id = Some(SESSION.into());
        runner.update(&json!({"sessionUpdate":"tool_call","toolCallId":"call-1","status":"in_progress","title":"pwsh"})).await.unwrap();
        runner.permission(permission(33)).await.unwrap();
        let key = runner.pending.keys().next().unwrap().clone();
        let done =
            json!({"sessionUpdate":"tool_call_update","toolCallId":"call-1","status":"completed"});
        runner.update(&done).await.unwrap();
        runner.resolve(&key, true).await.unwrap();
        assert!(runner.pending.is_empty());
        assert!(runner.tools.is_empty());
        assert_eq!(written.frames().len(), 1);
        assert_eq!(
            written.frames()[0]["result"]["outcome"]["optionId"],
            "reject-once"
        );
        assert!(runner.update(&done).await.is_err());
    }

    #[tokio::test]
    async fn active_tools_prevent_successful_turn_completion() {
        let req = request(false);
        let (mut runner, _, tx, _controls, _events) = fixture(&req);
        handshake(&tx, &req).await;
        tx.send(Ok(json!({"jsonrpc":"2.0","method":"session/update","params":{"sessionId":SESSION,"update":{"sessionUpdate":"tool_call","toolCallId":"call-1","status":"in_progress","title":"pwsh"}}}))).await.unwrap();
        tx.send(Ok(
            json!({"jsonrpc":"2.0","id":5,"result":{"stopReason":"end_turn"}}),
        ))
        .await
        .unwrap();
        assert!(runner
            .run(&req)
            .await
            .unwrap_err()
            .to_string()
            .contains("active tools"));
    }

    #[tokio::test]
    async fn context_usage_is_not_reported_as_billable_token_usage() {
        let (mut runner, _, _, _, mut events) = fixture(&request(true));
        runner
            .update(&json!({"sessionUpdate":"usage_update","used":100,"size":128000}))
            .await
            .unwrap();
        assert_eq!(
            runner.result.usage.as_ref().unwrap()["kind"],
            "context_occupancy"
        );
        assert!(matches!(
            events.recv().await,
            Some(NativeEvent::Usage { .. })
        ));
    }

    #[tokio::test]
    async fn tool_output_budget_is_bounded_too() {
        let (mut runner, _, _, _, _events) = fixture(&request(true));
        runner.streamed_bytes = MAX_OUTPUT_BYTES;
        assert!(runner.update(&json!({"sessionUpdate":"tool_call_update","toolCallId":"a","status":"completed","content":[]})).await.is_err());
    }
}
