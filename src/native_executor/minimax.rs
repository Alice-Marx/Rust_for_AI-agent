//! MiniMax Code（`minimax-code`，bin `mcode`）受管 ACP 适配器。
//!
//! `mcode acp`（`node dist/cli.js acp`）在 stdin/stdout 上讲 Agent Client
//! Protocol。复用 `acp::AcpRunner` 框架；方言按上游 0.5.2 源码核对的形状
//! 严格解码（模型值为 `m:<provider>:<model>[:v<variant>]` wire 格式）。
//!
//! 真实 CLI 握手、`mcode acp login` 登录与真实推理尚未执行（等账号登录后
//! 按 H09 步骤 7-8 验证）。

use super::acp::{AcpDialect, AcpRunner};
use crate::native_executor::{emit, NativeControl, NativeEvent, NativeRequest, NativeResult};
use anyhow::{bail, ensure, Context, Result};
#[cfg(test)]
use serde_json::json;
use serde_json::Value;
use std::{
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};
use tokio::{io::BufReader, sync::mpsc};

/// 官方 npm 包固定版本；升级必须重新核对 `packages/acp-server` 的协议
/// 形状（config-options.ts / approval.ts / events-map.ts）并更新此常量。
const PACKAGE_VERSION: &str = "0.5.2";

pub(super) fn validate_request(req: &NativeRequest) -> Result<()> {
    ensure!(req.app_id == "minimax", "MiniMax ACP profile required");
    ensure!(
        is_model_wire_id(&req.model),
        "MiniMax requires the exact m:<provider>:<model>[:v<variant>] wire ID"
    );
    ensure!(
        req.config_path.is_none(),
        "managed MiniMax uses its official account configuration; custom config injection is unsupported"
    );
    // The upstream `thinkingEffort` config is model-dependent; Wonderland's
    // generic effort values are never mapped onto it.
    ensure!(
        req.reasoning_effort.is_none(),
        "MiniMax exposes a model-dependent thinkingEffort option, not a generic reasoning effort; omit it"
    );
    Ok(())
}

/// `m:<provider>:<model>` or `m:<provider>:<model>:v<variant>` — the exact
/// upstream wire format (control-state.ts parseModelConfigValue).
fn is_model_wire_id(model: &str) -> bool {
    if model.is_empty() || model.len() > 160 {
        return false;
    }
    if model.bytes().any(|b| b.is_ascii_whitespace() || b == 0) {
        return false;
    }
    let parts: Vec<&str> = model.split(':').collect();
    let ok_segment = |part: &str| {
        !part.is_empty()
            && part.len() <= 64
            && part
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_'))
    };
    match parts.as_slice() {
        ["m", provider, model] => ok_segment(provider) && ok_segment(model),
        ["m", provider, model, "v", variant] => {
            ok_segment(provider) && ok_segment(model) && ok_segment(variant)
        }
        _ => false,
    }
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
        "MiniMax package metadata exceeds limit"
    );
    serde_json::from_slice(&data).context("invalid MiniMax package metadata")
}

fn resolve_entry() -> Result<PathBuf> {
    if let Some(value) = std::env::var_os("WONDERLAND_MINIMAX_CLI") {
        let path = PathBuf::from(&value);
        ensure!(
            path.is_absolute(),
            "WONDERLAND_MINIMAX_CLI must be an absolute official dist/cli.js path"
        );
        return Ok(path);
    }
    let output = std::process::Command::new("npm")
        .args(["root", "-g"])
        .output()
        .context("npm is required to locate the official MiniMax installation")?;
    ensure!(
        output.status.success(),
        "npm root -g failed while locating MiniMax"
    );
    let text = String::from_utf8_lossy(&output.stdout);
    let base = PathBuf::from(text.trim().trim_end_matches(['/', '\\']));
    ensure!(base.is_absolute(), "npm root -g returned a relative path");
    let entry = base.join("minimax-code").join("dist").join("cli.js");
    ensure!(
        entry.is_file(),
        "official MiniMax entry not found; install minimax-code via npm or set WONDERLAND_MINIMAX_CLI"
    );
    Ok(entry)
}

fn verify_installation(entry: &Path) -> Result<Prepared> {
    ensure!(
        entry.is_absolute(),
        "MiniMax entry must be an absolute path"
    );
    let cli = std::fs::canonicalize(entry).context("MiniMax entry is missing")?;
    let root = cli
        .parent()
        .and_then(Path::parent)
        .context("invalid MiniMax npm layout")?;
    ensure!(
        cli == root.join("dist").join("cli.js"),
        "MiniMax must use the official dist/cli.js entry"
    );
    let package = read_json(&root.join("package.json"))?;
    ensure!(
        package["name"] == "minimax-code" || package["name"] == "@minimax/code",
        "unexpected package next to the MiniMax entry: {}",
        package["name"]
    );
    ensure!(
        package["version"] == PACKAGE_VERSION,
        "MiniMax npm version changed: {} (expected {PACKAGE_VERSION})",
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
        .context("Node.js is required to run the official MiniMax entry")?;
    ensure!(
        output.status.success(),
        "node --version failed while preparing MiniMax"
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
pub(super) struct MiniMaxDialect;

impl AcpDialect for MiniMaxDialect {
    fn label(&self) -> &'static str {
        "MiniMax"
    }
    fn protocol(&self) -> &'static str {
        "minimax-acp"
    }
    fn request_prefix(&self) -> &'static str {
        "minimax"
    }
    fn status_note(&self) -> String {
        "MiniMax official ACP: model selections use the m:<provider>:<model> wire format; the thinkingEffort option is model-dependent and deliberately untouched. Login state is the official MiniMax account; nothing here attests subscription quota.".into()
    }
    fn verify_initialize(&self, init: &Value) -> Result<()> {
        ensure!(
            init["protocolVersion"] == 1
                && init.pointer("/agentInfo/name").and_then(Value::as_str) == Some("minimax-code")
                && init.pointer("/agentInfo/version").and_then(Value::as_str)
                    == Some(PACKAGE_VERSION)
                && init
                    .pointer("/agentCapabilities/sessionCapabilities/close")
                    .is_some(),
            "MiniMax ACP identity/capability handshake mismatch"
        );
        Ok(())
    }
    fn model_config_id(&self) -> &'static str {
        "model"
    }
    fn model_value(&self, req: &NativeRequest) -> String {
        // The upstream model option consumes the exact wire-format value.
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
            .context("MiniMax ACP config options missing")?;
        let models: Vec<_> = options
            .iter()
            .filter(|item| item["id"] == "model")
            .collect();
        ensure!(
            models.len() == 1 && models[0]["currentValue"].as_str() == Some(model),
            "MiniMax provider/model drift detected"
        );
        Ok(())
    }
    fn confirmed_provider(&self, req: &NativeRequest) -> Option<String> {
        let mut parts = req.model.split(':');
        (parts.next() == Some("m"))
            .then(|| parts.next().map(str::to_owned))
            .flatten()
    }
    fn permission_selection(&self, options: &[Value]) -> Result<(String, String)> {
        let allow = options
            .iter()
            .find(|option| option["kind"] == "allow_once" && option["optionId"] == "allow-once");
        let reject = options
            .iter()
            .find(|option| option["kind"] == "reject_once" && option["optionId"] == "deny");
        ensure!(
            allow.is_some() && reject.is_some(),
            "MiniMax permission options changed; allow-once/deny must remain selectable"
        );
        Ok(("allow-once".into(), "deny".into()))
    }
    fn passthrough_updates(&self) -> &'static [&'static str] {
        &["plan", "available_commands_update"]
    }
    fn stop_status(&self, reason: Option<&str>) -> Result<String> {
        match reason {
            Some("end_turn") => Ok("completed".into()),
            Some("cancelled") => Ok("cancelled".into()),
            _ => bail!("MiniMax ACP returned an unsupported stop reason"),
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
                    bail!("unsupported Windows device path for MiniMax")
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
                    "MiniMax path normalization changed its target"
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
        _ = events.closed() => bail!("MiniMax event consumer disconnected during preparation"),
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
        .context("could not start the official MiniMax ACP server")?;
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
    let stdin = child.stdin.take().context("MiniMax stdin unavailable")?;
    let stdout = child.stdout.take().context("MiniMax stdout unavailable")?;
    let (tx, rx) = mpsc::channel(64);
    let reader = tokio::spawn(async move {
        let mut reader = BufReader::new(stdout);
        loop {
            let mut frame = Vec::new();
            let result = match super::read_frame(&mut reader, &mut frame).await {
                Ok(true) => {
                    serde_json::from_slice(&frame).context("invalid MiniMax ACP JSON frame")
                }
                Ok(false) => Err(anyhow::anyhow!("MiniMax ACP stream closed before response")),
                Err(error) => Err(error),
            };
            let failed = result.is_err();
            if tx.send(result).await.is_err() || failed {
                break;
            }
        }
    });
    let mut runner = AcpRunner::new(
        Box::new(MiniMaxDialect),
        Box::new(stdin),
        rx,
        controls,
        events.clone(),
        &req,
    );
    runner.set_executable_identity(PACKAGE_VERSION, &prepared.sha256);
    let outcome = tokio::select! {
        result = runner.run(&req) => result,
        _ = super::cancellation(&mut cancel) => Ok("cancelled".into()),
        _ = tokio::time::sleep_until(deadline) => Ok("timed_out".into()),
        _ = events.closed() => Err(anyhow::anyhow!("MiniMax event consumer disconnected")),
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
            app_id: "minimax".into(),
            model: "m:minimax:minimax-m3".into(),
            cwd: std::env::current_dir().unwrap(),
            prompt: "inspect only".into(),
            read_only,
            max_duration_secs: 30,
            reasoning_effort: None,
            config_path: None,
        }
    }

    #[test]
    fn model_ids_must_be_the_upstream_wire_format() {
        let mut req = request(true);
        assert!(validate_request(&req).is_ok());
        req.model = "m:minimax:minimax-m3:v:high".into();
        assert!(validate_request(&req).is_ok());
        for model in [
            "minimax-m3",
            "m:minimax",
            "m::minimax-m3",
            "m:minimax:minimax-m3:x",
            "m:minimax:minimax m3",
        ] {
            req.model = model.into();
            assert!(validate_request(&req).is_err(), "{model}");
        }
        let mut with_effort = request(true);
        with_effort.reasoning_effort = Some("high".into());
        assert!(validate_request(&with_effort).is_err());
    }

    #[test]
    fn dialect_pins_upstream_identity_and_one_shot_permissions() {
        let dialect = MiniMaxDialect;
        assert!(dialect
            .verify_initialize(&json!({
                "protocolVersion":1,
                "agentInfo":{"name":"minimax-code","version":PACKAGE_VERSION},
                "agentCapabilities":{"sessionCapabilities":{"close":{}}}
            }))
            .is_ok());
        for identity in [
            json!({"protocolVersion":1,"agentInfo":{"name":"MiniMax","version":PACKAGE_VERSION},"agentCapabilities":{"sessionCapabilities":{"close":{}}}}),
            json!({"protocolVersion":1,"agentInfo":{"name":"minimax-code","version":"0.5.1"},"agentCapabilities":{"sessionCapabilities":{"close":{}}}}),
        ] {
            assert!(dialect.verify_initialize(&identity).is_err());
        }
        let options = vec![
            json!({"kind":"allow_once","optionId":"allow-once","name":"Allow once"}),
            json!({"kind":"allow_always","optionId":"allow-always","name":"Always allow"}),
            json!({"kind":"reject_once","optionId":"deny","name":"Deny"}),
        ];
        assert_eq!(
            dialect.permission_selection(&options).unwrap(),
            ("allow-once".to_owned(), "deny".to_owned())
        );
        assert!(dialect
            .permission_selection(&vec![
                json!({"kind":"allow_always","optionId":"allow-always"}),
                json!({"kind":"reject_once","optionId":"deny"}),
            ])
            .is_err());
        assert_eq!(dialect.model_value(&request(true)), "m:minimax:minimax-m3");
        assert_eq!(
            dialect.confirmed_provider(&request(true)).as_deref(),
            Some("minimax")
        );
        assert_eq!(dialect.effort_config_id(), None);
        assert_eq!(dialect.stop_status(Some("end_turn")).unwrap(), "completed");
        assert_eq!(dialect.stop_status(Some("cancelled")).unwrap(), "cancelled");
        assert!(dialect.stop_status(Some("mystery")).is_err());
        let options = json!([
            {"id":"model","currentValue":"m:minimax:minimax-m3"},
            {"id":"permissionMode","currentValue":"normal"}
        ]);
        assert!(dialect
            .verify_config_options(&options, "m:minimax:minimax-m3", None)
            .is_ok());
        assert!(dialect
            .verify_config_options(&options, "m:minimax:abab6.5s", None)
            .is_err());
    }

    #[tokio::test]
    async fn full_minimax_session_over_the_generic_runner() {
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
            Box::new(MiniMaxDialect),
            Box::new(written.clone()),
            rx,
            control_rx,
            event_tx,
            &req,
        );
        let options = json!([
            {"id":"model","currentValue":"m:minimax:minimax-m3"},
            {"id":"permissionMode","currentValue":"normal"}
        ]);
        let session = "2a1b3c4d-5e6f-4a7b-8c9d-0e1f2a3b4c5d";
        for value in [
            json!({"jsonrpc":"2.0","id":1,"result":{"protocolVersion":1,"agentInfo":{"name":"minimax-code","version":PACKAGE_VERSION},"agentCapabilities":{"sessionCapabilities":{"close":{}}}}}),
            json!({"jsonrpc":"2.0","id":2,"result":{"sessionId":session,"configOptions":options}}),
            json!({"jsonrpc":"2.0","id":3,"result":{"configOptions":options}}),
            json!({"jsonrpc":"2.0","method":"session/update","params":{"sessionId":session,"update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"ok"}}}}),
            json!({"jsonrpc":"2.0","method":"session/update","params":{"sessionId":session,"update":{"sessionUpdate":"tool_call","toolCallId":"call-1","title":"read","status":"in_progress","rawInput":{}}}}),
            json!({"jsonrpc":"2.0","id":7,"method":"session/request_permission","params":{"sessionId":session,"toolCall":{"toolCallId":"call-1"},"options":[{"kind":"allow_once","optionId":"allow-once"},{"kind":"allow_always","optionId":"allow-always"},{"kind":"reject_once","optionId":"deny"}]}}),
            json!({"jsonrpc":"2.0","method":"session/update","params":{"sessionId":session,"update":{"sessionUpdate":"tool_call_update","toolCallId":"call-1","status":"completed","content":[]}}}),
            json!({"jsonrpc":"2.0","id":4,"result":{"stopReason":"end_turn"}}),
        ] {
            tx.send(Ok(value)).await.unwrap();
        }
        assert_eq!(runner.run(&req).await.unwrap(), "completed");
        let result = runner.into_result();
        assert_eq!(result.output, "ok");
        let mut saw_tool = false;
        while let Ok(event) = event_rx.try_recv() {
            if matches!(event, NativeEvent::ToolActivity { .. }) {
                saw_tool = true;
            }
        }
        assert!(saw_tool);
        let frames: Vec<Value> = written
            .0
            .lock()
            .unwrap()
            .split(|b| *b == b'\n')
            .filter(|v| !v.is_empty())
            .map(|v| serde_json::from_slice(v).unwrap())
            .collect();
        assert_eq!(frames[2]["params"]["value"], "m:minimax:minimax-m3");
        let deny = frames
            .iter()
            .find(|frame| frame["id"] == json!(7))
            .expect("permission response recorded");
        assert_eq!(deny["result"]["outcome"]["optionId"], "deny");
    }
}
