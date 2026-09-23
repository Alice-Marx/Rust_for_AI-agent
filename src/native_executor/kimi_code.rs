//! Kimi Code · Node（Moonshot，`@moonshot-ai/kimi-code`）受管 ACP 适配器。
//!
//! `kimi acp`（`dist/main.mjs acp`，由 `@moonshot-ai/acp-server` 驱动）在
//! stdin/stdout 上讲 Agent Client Protocol。复用 `acp::AcpRunner` 框架；
//! 方言按上游 2.0.2 源码核对的形状严格解码。
//!
//! 身份注意：Node 版 `kimi` 与 Python 版 kimi-cli 的命令同名。本适配器只
//! 认 npm 布局里的 `@moonshot-ai/kimi-code/dist/main.mjs`（经 Node 执行），
//! 绝不解析 PATH 上的裸 `kimi`，两个 Kimi 适配互不冒充。
//!
//! 真实 CLI 握手与真实推理尚未执行（等账号登录后按 H09 步骤 7-8 验证）。

use super::acp::{AcpDialect, AcpRunner};
use crate::native_executor::{emit, NativeControl, NativeEvent, NativeRequest, NativeResult};
use anyhow::{bail, ensure, Context, Result};
use serde_json::{json, Value};
use std::{
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};
use tokio::{io::BufReader, sync::mpsc};

/// 官方 npm 包固定版本；升级必须重新核对 `packages/acp-server` 的协议
/// 形状（config-options.ts / approval.ts / events-map.ts）并更新此常量。
const PACKAGE_VERSION: &str = "2.0.2";

pub(super) fn validate_request(req: &NativeRequest) -> Result<()> {
    ensure!(req.app_id == "kimi-code", "Kimi Code ACP profile required");
    ensure!(
        req.model.starts_with("kimi-")
            && req.model.len() <= 160
            && req
                .model
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'.'),
        "Kimi Code requires an explicit kimi-* model ID",
    );
    ensure!(
        req.config_path.is_none(),
        "managed Kimi Code uses its official account configuration; custom config injection is unsupported"
    );
    // The upstream `thinking` select is model-dependent (off/on or declared
    // effort levels); Wonderland's generic effort values are never mapped onto
    // it until a verified per-model vocabulary exists.
    ensure!(
        req.reasoning_effort.is_none(),
        "Kimi Code exposes a model-dependent thinking option, not a generic reasoning effort; omit it"
    );
    Ok(())
}

struct Prepared {
    node: PathBuf,
    cli: PathBuf,
    sha256: String,
}

fn read_json(path: &Path) -> Result<Value> {
    use std::io::Read;
    let mut data = Vec::new();
    std::fs::File::open(path)?
        .take(256 * 1024 + 1)
        .read_to_end(&mut data)?;
    ensure!(
        data.len() <= 256 * 1024,
        "Kimi Code package metadata exceeds limit"
    );
    serde_json::from_slice(&data).context("invalid Kimi Code package metadata")
}

fn resolve_entry() -> Result<PathBuf> {
    if let Some(value) = std::env::var_os("WONDERLAND_KIMI_CODE_CLI") {
        let path = PathBuf::from(&value);
        ensure!(
            path.is_absolute(),
            "WONDERLAND_KIMI_CODE_CLI must be an absolute official dist/main.mjs path"
        );
        return Ok(path);
    }
    let output = std::process::Command::new("npm")
        .args(["root", "-g"])
        .output()
        .context("npm is required to locate the official Kimi Code installation")?;
    ensure!(
        output.status.success(),
        "npm root -g failed while locating Kimi Code"
    );
    let text = String::from_utf8_lossy(&output.stdout);
    let base = PathBuf::from(text.trim().trim_end_matches(['/', '\\']));
    ensure!(base.is_absolute(), "npm root -g returned a relative path");
    let entry = base
        .join("@moonshot-ai")
        .join("kimi-code")
        .join("dist")
        .join("main.mjs");
    ensure!(
        entry.is_file(),
        "official Kimi Code entry not found; install @moonshot-ai/kimi-code via npm or set WONDERLAND_KIMI_CODE_CLI"
    );
    Ok(entry)
}

fn verify_installation(entry: &Path) -> Result<Prepared> {
    ensure!(
        entry.is_absolute(),
        "Kimi Code entry must be an absolute path"
    );
    let cli = std::fs::canonicalize(entry).context("Kimi Code entry is missing")?;
    let root = cli
        .parent()
        .and_then(Path::parent)
        .context("invalid Kimi Code npm layout")?;
    ensure!(
        cli == root.join("dist").join("main.mjs"),
        "Kimi Code must use the official dist/main.mjs entry"
    );
    let package = read_json(&root.join("package.json"))?;
    ensure!(
        package["name"] == "@moonshot-ai/kimi-code",
        "unexpected package next to the Kimi Code entry: {}",
        package["name"]
    );
    ensure!(
        package["version"] == PACKAGE_VERSION,
        "Kimi Code npm version changed: {} (expected {PACKAGE_VERSION})",
        package["version"]
    );
    let node = which_node()?;
    let sha256 = super::executable_digest(&cli)?;
    Ok(Prepared { node, cli, sha256 })
}

fn which_node() -> Result<PathBuf> {
    let output = std::process::Command::new("node")
        .arg("--version")
        .output()
        .context("Node.js is required to run the official Kimi Code entry")?;
    ensure!(
        output.status.success(),
        "node --version failed while preparing Kimi Code"
    );
    let version = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    ensure!(
        version.starts_with('v') && version.len() <= 32,
        "unexpected node version banner"
    );
    let program = std::env::var_os("WONDERLAND_NODE")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("node"));
    Ok(program)
}

/// Kimi Code ACP dialect, checked against `packages/acp-server` @2.0.2.
pub(super) struct KimiCodeDialect;

impl AcpDialect for KimiCodeDialect {
    fn label(&self) -> &'static str {
        "Kimi Code"
    }
    fn protocol(&self) -> &'static str {
        "kimi-code-acp"
    }
    fn request_prefix(&self) -> &'static str {
        "kimi-code"
    }
    fn status_note(&self) -> String {
        "Kimi Code official ACP (Node): the upstream thinking select is model-dependent and deliberately untouched; usage_update reports context occupancy only (the engine emits no cost). Login state is the official Kimi account; nothing here attests subscription quota.".into()
    }
    fn verify_initialize(&self, init: &Value) -> Result<()> {
        ensure!(
            init["protocolVersion"] == 1
                && init.pointer("/agentInfo/name").and_then(Value::as_str) == Some("Kimi Code CLI")
                && init.pointer("/agentInfo/version").and_then(Value::as_str)
                    == Some(PACKAGE_VERSION)
                && init
                    .pointer("/agentCapabilities/sessionCapabilities/close")
                    .is_some(),
            "Kimi Code ACP identity/capability handshake mismatch"
        );
        Ok(())
    }
    fn model_config_id(&self) -> &'static str {
        "model"
    }
    fn model_value(&self, req: &NativeRequest) -> String {
        // The upstream model option uses the bare catalog model id.
        req.model.clone()
    }
    fn effort_config_id(&self) -> Option<&'static str> {
        None
    }
    fn verify_config_options(
        &self,
        options: &Value,
        model: &str,
        _effort: Option<&str>,
    ) -> Result<()> {
        let options = options
            .as_array()
            .context("Kimi Code ACP config options missing")?;
        let models: Vec<_> = options
            .iter()
            .filter(|item| item["id"] == "model")
            .collect();
        ensure!(
            models.len() == 1 && models[0]["currentValue"].as_str() == Some(model),
            "Kimi Code provider/model drift detected"
        );
        Ok(())
    }
    fn permission_selection(&self, options: &[Value]) -> Result<(String, String)> {
        let allow = options
            .iter()
            .find(|option| option["kind"] == "allow_once" && option["optionId"] == "approve_once");
        let reject = options
            .iter()
            .find(|option| option["kind"] == "reject_once" && option["optionId"] == "reject");
        ensure!(
            allow.is_some() && reject.is_some(),
            "Kimi Code permission options changed; approve_once/reject must remain selectable"
        );
        Ok(("approve_once".into(), "reject".into()))
    }
    fn passthrough_updates(&self) -> &'static [&'static str] {
        &["plan", "available_commands_update"]
    }
    fn stop_status(&self, reason: Option<&str>) -> Result<String> {
        match reason {
            Some("end_turn") => Ok("completed".into()),
            Some("cancelled") => Ok("cancelled".into()),
            _ => bail!("Kimi Code ACP returned an unsupported stop reason"),
        }
    }
}

pub(super) async fn execute_with_control(
    mut req: NativeRequest,
    events: mpsc::Sender<NativeEvent>,
    mut cancel: tokio::sync::watch::Receiver<bool>,
    controls: mpsc::Receiver<NativeControl>,
) -> Result<NativeResult> {
    validate_request(&req)?;
    // Node's entry-point resolver rejects Windows extended-length paths even
    // when CreateProcess and Rust filesystem APIs accept the same path.
    #[cfg(windows)]
    {
        use std::path::{Component, Prefix};
        let mut parts = req.cwd.components();
        if let Some(Component::Prefix(prefix)) = parts.next() {
            let normal = match prefix.kind() {
                Prefix::VerbatimDisk(drive) => Some(PathBuf::from(format!("{}:\\", drive as char))),
                Prefix::VerbatimUNC(server, share) => {
                    let mut root = std::ffi::OsString::from("\\\\");
                    root.push(server);
                    root.push("\\");
                    root.push(share);
                    root.push("\\");
                    Some(PathBuf::from(root))
                }
                Prefix::Verbatim(_) | Prefix::DeviceNS(_) => {
                    bail!("unsupported Windows device path for Kimi Code")
                }
                _ => None,
            };
            if let Some(mut normalized) = normal {
                for part in parts {
                    if !matches!(part, Component::RootDir) {
                        normalized.push(part.as_os_str());
                    }
                }
                ensure!(
                    std::fs::canonicalize(&normalized)? == std::fs::canonicalize(&req.cwd)?,
                    "Kimi Code path normalization changed its target"
                );
                req.cwd = normalized;
            }
        }
    }
    let deadline = tokio::time::Instant::now() + Duration::from_secs(req.max_duration_secs);
    if *cancel.borrow() {
        return Ok(super::empty_result(&req, "cancelled"));
    }
    let preparation = tokio::task::spawn_blocking(|| verify_installation(&resolve_entry()?));
    let prepared = tokio::select! {
        result = preparation => result??,
        _ = super::cancellation(&mut cancel) => return Ok(super::empty_result(&req, "cancelled")),
        _ = tokio::time::sleep_until(deadline) => return Ok(super::empty_result(&req, "timed_out")),
        _ = events.closed() => bail!("Kimi Code event consumer disconnected during preparation"),
    };
    emit(
        &events,
        NativeEvent::Identity {
            executable: prepared.cli.to_string_lossy().into_owned(),
            version: PACKAGE_VERSION.into(),
            sha256: prepared.sha256.clone(),
            verification: "Official npm layout and exact package version verified; entry digest recorded (pinned constant pending real-install verification); no real handshake or inference has run in this environment".into(),
        },
    )
    .await?;
    let mut child = tokio::process::Command::new(&prepared.node)
        .arg(&prepared.cli)
        .arg("acp")
        .current_dir(&req.cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .context("could not start the official Kimi Code ACP server")?;
    let tree = match crate::process_tree::ProcessTree::attach(
        child.id().context("Kimi Code process has no ID")?,
    ) {
        Ok(tree) => tree,
        Err(error) => {
            let _ = child.start_kill();
            let _ = tokio::time::timeout(Duration::from_secs(2), child.wait()).await;
            return Err(error);
        }
    };
    let stdin = child.stdin.take().context("Kimi Code stdin unavailable")?;
    let stdout = child
        .stdout
        .take()
        .context("Kimi Code stdout unavailable")?;
    let (tx, rx) = mpsc::channel(64);
    let reader = tokio::spawn(async move {
        let mut reader = BufReader::new(stdout);
        loop {
            let mut frame = Vec::new();
            let result = match super::read_frame(&mut reader, &mut frame).await {
                Ok(true) => {
                    serde_json::from_slice(&frame).context("invalid Kimi Code ACP JSON frame")
                }
                Ok(false) => Err(anyhow::anyhow!(
                    "Kimi Code ACP stream closed before response"
                )),
                Err(error) => Err(error),
            };
            let failed = result.is_err();
            if tx.send(result).await.is_err() || failed {
                break;
            }
        }
    });
    let mut runner = AcpRunner::new(
        Box::new(KimiCodeDialect),
        Box::new(stdin),
        rx,
        controls,
        events.clone(),
        &req,
    );
    let outcome = tokio::select! {
        result = runner.run(&req) => result,
        _ = super::cancellation(&mut cancel) => Ok("cancelled".into()),
        _ = tokio::time::sleep_until(deadline) => Ok("timed_out".into()),
        _ = events.closed() => Err(anyhow::anyhow!("Kimi Code event consumer disconnected")),
    };
    // Always close, including JSON-RPC errors, route drift and UI disconnect.
    // Cancel is only a notification; close and the process tree are the fences.
    let _ = tokio::time::timeout(Duration::from_millis(600), runner.shutdown()).await;
    let mut native_result = runner.into_result();
    let _ = tokio::time::timeout(Duration::from_millis(400), child.wait()).await;
    drop(tree);
    let _ = child.start_kill();
    let _ = tokio::time::timeout(Duration::from_secs(2), child.wait()).await;
    reader.abort();
    let _ = reader.await;
    let status = outcome?;
    native_result.status = status.clone();
    emit(&events, NativeEvent::Completed { status }).await?;
    Ok(native_result)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(read_only: bool) -> NativeRequest {
        NativeRequest {
            app_id: "kimi-code".into(),
            model: "kimi-k2.7-code".into(),
            cwd: std::env::current_dir().unwrap(),
            prompt: "inspect only".into(),
            read_only,
            max_duration_secs: 30,
            reasoning_effort: None,
            config_path: None,
        }
    }

    #[test]
    fn model_ids_and_generic_efforts_are_rejected() {
        let mut req = request(true);
        assert!(validate_request(&req).is_ok());
        for model in ["gpt-5", "kimi", "kimi-k2.7-code/x", "kimi-k2.7-code "] {
            req.model = model.into();
            assert!(validate_request(&req).is_err(), "{model}");
        }
        let mut with_effort = request(true);
        with_effort.reasoning_effort = Some("high".into());
        assert!(validate_request(&with_effort).is_err());
    }

    #[test]
    fn dialect_pins_upstream_identity_and_one_shot_permissions() {
        let dialect = KimiCodeDialect;
        assert!(dialect
            .verify_initialize(&json!({
                "protocolVersion":1,
                "agentInfo":{"name":"Kimi Code CLI","version":PACKAGE_VERSION},
                "agentCapabilities":{"sessionCapabilities":{"close":{},"resume":{}}}
            }))
            .is_ok());
        for identity in [
            json!({"protocolVersion":1,"agentInfo":{"name":"kimi","version":PACKAGE_VERSION},"agentCapabilities":{"sessionCapabilities":{"close":{}}}}),
            json!({"protocolVersion":1,"agentInfo":{"name":"Kimi Code CLI","version":"2.0.1"},"agentCapabilities":{"sessionCapabilities":{"close":{}}}}),
        ] {
            assert!(dialect.verify_initialize(&identity).is_err());
        }
        let options = vec![
            json!({"kind":"allow_once","optionId":"approve_once","name":"Approve once"}),
            json!({"kind":"allow_always","optionId":"approve_always","name":"Always approve"}),
            json!({"kind":"reject_once","optionId":"reject","name":"Reject"}),
        ];
        assert_eq!(
            dialect.permission_selection(&options).unwrap(),
            ("approve_once".to_owned(), "reject".to_owned())
        );
        assert!(dialect
            .permission_selection(&vec![
                json!({"kind":"allow_always","optionId":"approve_always"}),
                json!({"kind":"reject_once","optionId":"reject"}),
            ])
            .is_err());
        assert_eq!(dialect.model_value(&request(true)), "kimi-k2.7-code");
        assert_eq!(dialect.effort_config_id(), None);
        assert!(dialect.passthrough_updates().contains(&"plan"));
        assert_eq!(dialect.stop_status(Some("end_turn")).unwrap(), "completed");
        assert_eq!(dialect.stop_status(Some("cancelled")).unwrap(), "cancelled");
        assert!(dialect.stop_status(Some("refusal")).is_err());
        let options = json!([
            {"id":"model","currentValue":"kimi-k2.7-code","type":"select"},
            {"id":"thinking","currentValue":"off","type":"select"}
        ]);
        assert!(dialect
            .verify_config_options(&options, "kimi-k2.7-code", None)
            .is_ok());
        assert!(dialect
            .verify_config_options(&options, "kimi-k3", None)
            .is_err());
    }

    #[tokio::test]
    async fn full_kimi_code_session_over_the_generic_runner() {
        use super::super::acp::AcpRunner;
        use std::pin::Pin;
        use std::sync::{Arc, Mutex};
        use tokio::io::AsyncWrite;

        #[derive(Clone, Default)]
        struct Recorded(Arc<Mutex<Vec<u8>>>);
        impl AsyncWrite for Recorded {
            fn poll_write(
                self: Pin<&mut Self>,
                _: &mut std::task::Context<'_>,
                data: &[u8],
            ) -> std::task::Poll<std::io::Result<usize>> {
                self.0.lock().unwrap().extend_from_slice(data);
                std::task::Poll::Ready(Ok(data.len()))
            }
            fn poll_flush(
                self: Pin<&mut Self>,
                _: &mut std::task::Context<'_>,
            ) -> std::task::Poll<std::io::Result<()>> {
                std::task::Poll::Ready(Ok(()))
            }
            fn poll_shutdown(
                self: Pin<&mut Self>,
                _: &mut std::task::Context<'_>,
            ) -> std::task::Poll<std::io::Result<()>> {
                std::task::Poll::Ready(Ok(()))
            }
        }

        let req = request(true);
        let written = Recorded::default();
        let (tx, rx) = mpsc::channel(32);
        let (_control_tx, control_rx) = mpsc::channel(32);
        let (event_tx, mut event_rx) = mpsc::channel(64);
        let mut runner = AcpRunner::new(
            Box::new(KimiCodeDialect),
            Box::new(written.clone()),
            rx,
            control_rx,
            event_tx,
            &req,
        );
        let options = json!([
            {"id":"model","currentValue":"kimi-k2.7-code","type":"select"},
            {"id":"thinking","currentValue":"off","type":"select"}
        ]);
        let session = "1f0e7a61-2b3c-4d5e-8f9a-0b1c2d3e4f5a";
        for value in [
            json!({"jsonrpc":"2.0","id":1,"result":{"protocolVersion":1,"agentInfo":{"name":"Kimi Code CLI","version":PACKAGE_VERSION},"agentCapabilities":{"sessionCapabilities":{"close":{},"resume":{}}}}}),
            json!({"jsonrpc":"2.0","id":2,"result":{"sessionId":session,"configOptions":options}}),
            json!({"jsonrpc":"2.0","id":3,"result":{"configOptions":options}}),
            json!({"jsonrpc":"2.0","method":"session/update","params":{"sessionId":session,"update":{"sessionUpdate":"plan","entries":[{"label":"inspect","status":"completed"}]}}}),
            json!({"jsonrpc":"2.0","method":"session/update","params":{"sessionId":session,"update":{"sessionUpdate":"agent_thought_chunk","content":{"type":"text","text":"thinking..."}}}}),
            json!({"jsonrpc":"2.0","method":"session/update","params":{"sessionId":session,"update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"done"}}}}),
            json!({"jsonrpc":"2.0","method":"session/update","params":{"sessionId":session,"update":{"sessionUpdate":"usage_update","used":800,"size":262144}}}),
            json!({"jsonrpc":"2.0","id":4,"result":{"stopReason":"end_turn"}}),
        ] {
            tx.send(Ok(value)).await.unwrap();
        }
        assert_eq!(runner.run(&req).await.unwrap(), "completed");
        let result = runner.into_result();
        assert_eq!(result.output, "done");
        // kimi-code emits no cost: usage stays context-occupancy only.
        assert_eq!(result.usage.as_ref().unwrap()["kind"], "context_occupancy");
        assert!(result
            .usage
            .as_ref()
            .unwrap()
            .get("reported_cost_usd")
            .is_none());
        let mut saw_plan = false;
        let mut saw_reasoning = false;
        while let Ok(event) = event_rx.try_recv() {
            if let NativeEvent::ToolActivity { data } = &event {
                if data["sessionUpdate"] == "plan" {
                    saw_plan = true;
                }
            }
            if matches!(event, NativeEvent::ReasoningDelta { .. }) {
                saw_reasoning = true;
            }
        }
        assert!(saw_plan, "plan forwards as activity");
        assert!(saw_reasoning, "thought chunks surface as reasoning deltas");
        let frames: Vec<Value> = written
            .0
            .lock()
            .unwrap()
            .split(|b| *b == b'\n')
            .filter(|v| !v.is_empty())
            .map(|v| serde_json::from_slice(v).unwrap())
            .collect();
        assert_eq!(frames[2]["params"]["value"], "kimi-k2.7-code");
        assert_eq!(frames[3]["method"], "session/prompt");
    }
}
