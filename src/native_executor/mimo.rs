//! MiMo Code (Xiaomi, `@mimo-ai/cli`) 受管 ACP 适配器。
//!
//! 官方发布是平台二进制（launcher 包 `@mimo-ai/cli` + 平台包
//! `@mimo-ai/mimocode-<platform>-<arch>` 中的 `mimo`/`mimo.exe`）；
//! `mimo acp` 在 stdin/stdout 上讲 Agent Client Protocol。本适配器复用
//! `acp::AcpRunner` 框架，方言按上游 0.1.15 源码核对的形状严格解码。
//!
//! 身份注意：MiMo 的 ACP `initialize` 上报 `agentInfo.name = "OpenCode"`
//!（上游 fork 的遗留身份），握手校验按官方二进制的实际上报锁定，而不是
//! 期望它自称 MiMo。真实 CLI 握手与真实推理尚未执行（H09 步骤 7 待办），
//! 在那之前本适配器报告的能力边界以本文件与文档为准。

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

/// 官方 launcher 包与平台二进制包的固定版本。升级必须重新核对
/// `acp/agent.ts` 的协议形状并更新此常量。
const PACKAGE_VERSION: &str = "0.1.15";

pub(super) fn validate_request(req: &NativeRequest) -> Result<()> {
    ensure!(req.app_id == "mimo", "MiMo ACP profile required");
    ensure!(
        is_exact_model_id(&req.model),
        "MiMo requires an exact providerID/modelID (variant segments allowed in modelID)"
    );
    ensure!(
        req.config_path.is_none(),
        "managed MiMo uses its official account configuration; custom config injection is unsupported"
    );
    ensure!(
        req.reasoning_effort.is_none(),
        "MiMo ACP exposes no reasoning-effort config option; do not send generic effort values"
    );
    Ok(())
}

/// `providerID/modelID…`：provider 段是小写标识符，其余整体是模型 ID，
/// 变体段（例如 `claude-sonnet-4/high`）保留在 modelID 内，由官方侧解析。
fn is_exact_model_id(model: &str) -> bool {
    let Some((provider, rest)) = model.split_once('/') else {
        return false;
    };
    !model.is_empty()
        && model.len() <= 160
        && !model.bytes().any(|b| b.is_ascii_whitespace() || b == 0)
        && !provider.is_empty()
        && provider.len() <= 64
        && provider
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && !rest.is_empty()
        && rest
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'/' | b'_'))
}

struct Prepared {
    cli: PathBuf,
    sha256: String,
}

/// Resolve the official platform binary: WONDERLAND_MIMO_BIN or the official
/// MIMOCODE_BIN_PATH first, then the npm launcher layout.
fn resolve_binary() -> Result<PathBuf> {
    for variable in ["WONDERLAND_MIMO_BIN", "MIMOCODE_BIN_PATH"] {
        if let Some(value) = std::env::var_os(variable) {
            let path = PathBuf::from(&value);
            ensure!(
                path.is_absolute(),
                "{variable} must be an absolute official binary path"
            );
            return Ok(path);
        }
    }
    let root = npm_mimo_root().context(
        "MiMo official binary not found; install @mimo-ai/cli via npm or set WONDERLAND_MIMO_BIN",
    )?;
    for relative in [
        "cli/bin/.mimocode",
        "mimocode-windows-x64/mimo.exe",
        "mimocode-windows-x64-baseline/mimo.exe",
        "mimocode-linux-x64/mimo",
        "mimocode-linux-x64-baseline/mimo",
        "mimocode-linux-arm64/mimo",
        "mimocode-darwin-x64/mimo",
        "mimocode-darwin-arm64/mimo",
    ] {
        let candidate = root.join(relative);
        if candidate.is_file() {
            return Ok(candidate);
        }
    }
    bail!("MiMo platform binary is absent from the official npm layout")
}

fn npm_mimo_root() -> Result<PathBuf> {
    let output = std::process::Command::new("npm")
        .args(["root", "-g"])
        .output()
        .context("npm is required to locate the official MiMo installation")?;
    ensure!(
        output.status.success(),
        "npm root -g failed while locating MiMo"
    );
    let text = String::from_utf8_lossy(&output.stdout);
    let base = PathBuf::from(text.trim().trim_end_matches(['/', '\\']));
    ensure!(base.is_absolute(), "npm root -g returned a relative path");
    Ok(base.join("@mimo-ai"))
}

fn read_json(path: &Path) -> Result<Value> {
    use std::io::Read;
    let mut data = Vec::new();
    std::fs::File::open(path)?
        .take(256 * 1024 + 1)
        .read_to_end(&mut data)?;
    ensure!(
        data.len() <= 256 * 1024,
        "MiMo package metadata exceeds limit"
    );
    serde_json::from_slice(&data).context("invalid MiMo package metadata")
}

fn verify_installation(cli: &Path) -> Result<Prepared> {
    let cli = std::fs::canonicalize(cli).context("MiMo binary is missing")?;
    // The platform package (binary's own directory) or the launcher package
    // (sibling `cli/` directory) both carry the pinned npm version.
    for metadata in [
        cli.parent().map(|parent| parent.join("package.json")),
        cli.parent()
            .and_then(Path::parent)
            .map(|scope| scope.join("cli").join("package.json")),
    ]
    .into_iter()
    .flatten()
    {
        if !metadata.is_file() {
            continue;
        }
        let package = read_json(&metadata)?;
        let name = package["name"].as_str().unwrap_or_default();
        ensure!(
            name == "@mimo-ai/cli" || name.starts_with("@mimo-ai/mimocode-"),
            "unexpected package next to the MiMo binary: {name}"
        );
        ensure!(
            package["version"] == PACKAGE_VERSION,
            "MiMo npm version changed: {} (expected {PACKAGE_VERSION})",
            package["version"]
        );
        break;
    }
    let sha256 = super::executable_digest(&cli)?;
    Ok(Prepared { cli, sha256 })
}

/// The binary banner is the second identity gate; the exact output format is
/// pinned by the official 0.1.15 release and re-verified on upgrade.
fn verify_banner(cli: &Path) -> Result<()> {
    let output = std::process::Command::new(cli)
        .arg("--version")
        .output()
        .context("could not execute the MiMo binary")?;
    ensure!(
        output.status.success(),
        "MiMo --version failed with status {}",
        output.status
    );
    let banner = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    ensure!(
        banner.contains(PACKAGE_VERSION),
        "MiMo banner '{banner}' does not match the pinned release {PACKAGE_VERSION}"
    );
    Ok(())
}

/// MiMo Code ACP dialect, checked against the upstream `acp/agent.ts`.
pub(super) struct MimoDialect;

impl AcpDialect for MimoDialect {
    fn label(&self) -> &'static str {
        "MiMo"
    }
    fn protocol(&self) -> &'static str {
        "mimo-acp"
    }
    fn request_prefix(&self) -> &'static str {
        "mimo"
    }
    fn status_note(&self) -> String {
        "MiMo official ACP: multi-provider harness (managed accounts or BYOK); provider identity, account channel and billed amounts are not attested here. usage_update carries a harness-reported cost field that is not a provider-confirmed invoice.".into()
    }
    fn verify_initialize(&self, init: &Value) -> Result<()> {
        ensure!(
            init["protocolVersion"] == 1
                // The official binary identifies itself with the upstream
                // fork's name; we pin exactly what 0.1.15 reports.
                && init.pointer("/agentInfo/name").and_then(Value::as_str) == Some("OpenCode")
                && init.pointer("/agentInfo/version").and_then(Value::as_str)
                    == Some(PACKAGE_VERSION)
                && init
                    .pointer("/agentCapabilities/sessionCapabilities/close")
                    .is_some(),
            "MiMo ACP identity/capability handshake mismatch"
        );
        Ok(())
    }
    fn model_config_id(&self) -> &'static str {
        "model"
    }
    fn model_value(&self, req: &NativeRequest) -> String {
        req.model.clone()
    }
    fn effort_config_id(&self) -> Option<&'static str> {
        // MiMo exposes `mode`, not a reasoning effort; refusing generic values.
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
            .context("MiMo ACP config options missing")?;
        let models: Vec<_> = options
            .iter()
            .filter(|item| item["id"] == "model")
            .collect();
        ensure!(
            models.len() == 1 && models[0]["currentValue"].as_str() == Some(model),
            "MiMo provider/model drift detected"
        );
        Ok(())
    }
    fn confirmed_provider(&self, req: &NativeRequest) -> Option<String> {
        req.model
            .split_once('/')
            .map(|(provider, _)| provider.to_owned())
    }
    fn permission_selection(&self, options: &[Value]) -> Result<(String, String)> {
        // Exactly the one-shot pair is selected; allow_always exists upstream
        // but managed tasks never widen to session-wide grants.
        let allow = options
            .iter()
            .find(|option| option["kind"] == "allow_once" && option["optionId"] == "once");
        let reject = options
            .iter()
            .find(|option| option["kind"] == "reject_once" && option["optionId"] == "reject");
        ensure!(
            allow.is_some() && reject.is_some(),
            "MiMo permission options changed; one-shot allow/reject must remain selectable"
        );
        Ok(("once".into(), "reject".into()))
    }
    fn passthrough_updates(&self) -> &'static [&'static str] {
        &["available_commands_update"]
    }
    fn stop_status(&self, reason: Option<&str>) -> Result<String> {
        match reason {
            Some("end_turn") => Ok("completed".into()),
            Some("cancelled") => Ok("cancelled".into()),
            Some("max_tokens" | "max_turn_requests") => {
                bail!("MiMo stopped at an execution limit; task remains incomplete")
            }
            _ => bail!("MiMo ACP returned an unsupported stop reason"),
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
    let deadline = tokio::time::Instant::now() + Duration::from_secs(req.max_duration_secs);
    if *cancel.borrow() {
        return Ok(super::empty_result(&req, "cancelled"));
    }
    let preparation = tokio::task::spawn_blocking(|| {
        let cli = resolve_binary()?;
        verify_installation(&cli)
    });
    let prepared = tokio::select! {
        result = preparation => result??,
        _ = super::cancellation(&mut cancel) => return Ok(super::empty_result(&req, "cancelled")),
        _ = tokio::time::sleep_until(deadline) => return Ok(super::empty_result(&req, "timed_out")),
        _ = events.closed() => bail!("MiMo event consumer disconnected during preparation"),
    };
    let banner_cli = prepared.cli.clone();
    {
        let preparation = tokio::task::spawn_blocking(move || verify_banner(&banner_cli));
        tokio::select! {
            result = preparation => result??,
            _ = super::cancellation(&mut cancel) => return Ok(super::empty_result(&req, "cancelled")),
            _ = tokio::time::sleep_until(deadline) => return Ok(super::empty_result(&req, "timed_out")),
            _ = events.closed() => bail!("MiMo event consumer disconnected during version probe"),
        }
    }
    emit(
        &events,
        NativeEvent::Identity {
            executable: prepared.cli.to_string_lossy().into_owned(),
            version: PACKAGE_VERSION.into(),
            sha256: prepared.sha256.clone(),
            verification: "Official npm layout, launcher package pin and version banner verified; the platform binary digest is recorded but not yet pinned to a published constant, and no real handshake or inference has run in this environment".into(),
        },
    )
    .await?;
    let mut child = tokio::process::Command::new(&prepared.cli)
        .arg("acp")
        .current_dir(&req.cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .context("could not start the official MiMo ACP server")?;
    let tree = match crate::process_tree::ProcessTree::attach(
        child.id().context("MiMo process has no ID")?,
    ) {
        Ok(tree) => tree,
        Err(error) => {
            let _ = child.start_kill();
            let _ = tokio::time::timeout(Duration::from_secs(2), child.wait()).await;
            return Err(error);
        }
    };
    let stdin = child.stdin.take().context("MiMo stdin unavailable")?;
    let stdout = child.stdout.take().context("MiMo stdout unavailable")?;
    let (tx, rx) = mpsc::channel(64);
    let reader = tokio::spawn(async move {
        let mut reader = BufReader::new(stdout);
        loop {
            let mut frame = Vec::new();
            let result = match super::read_frame(&mut reader, &mut frame).await {
                Ok(true) => serde_json::from_slice(&frame).context("invalid MiMo ACP JSON frame"),
                Ok(false) => Err(anyhow::anyhow!("MiMo ACP stream closed before response")),
                Err(error) => Err(error),
            };
            let failed = result.is_err();
            if tx.send(result).await.is_err() || failed {
                break;
            }
        }
    });
    let mut runner = AcpRunner::new(
        Box::new(MimoDialect),
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
        _ = events.closed() => Err(anyhow::anyhow!("MiMo event consumer disconnected")),
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

    fn request() -> NativeRequest {
        NativeRequest {
            app_id: "mimo".into(),
            model: "anthropic/claude-sonnet-4.6".into(),
            cwd: std::env::current_dir().unwrap(),
            prompt: "inspect only".into(),
            read_only: true,
            max_duration_secs: 30,
            reasoning_effort: None,
            config_path: None,
        }
    }

    #[test]
    fn model_ids_must_be_exact_provider_slash_model() {
        let mut req = request();
        assert!(validate_request(&req).is_ok());
        for model in [
            "claude-sonnet-4.6",
            "Anthropic/claude",
            "/claude",
            "anthropic/",
            "anthropic/claude sonnet",
            "a",
        ] {
            req.model = model.into();
            assert!(validate_request(&req).is_err(), "{model}");
        }
        // Variant segments stay inside the modelID half.
        req.model = "anthropic/claude-sonnet-4.6/high".into();
        assert!(validate_request(&req).is_ok());
        let mut with_effort = request();
        with_effort.reasoning_effort = Some("high".into());
        assert!(validate_request(&with_effort).is_err());
    }

    #[tokio::test]
    async fn full_mimo_acp_session_over_the_generic_runner() {
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

        let req = request();
        let written = Recorded::default();
        let (tx, rx) = mpsc::channel(32);
        let (control_tx, control_rx) = mpsc::channel(32);
        let (event_tx, mut event_rx) = mpsc::channel(64);
        let mut runner = AcpRunner::new(
            Box::new(MimoDialect),
            Box::new(written.clone()),
            rx,
            control_rx,
            event_tx,
            &req,
        );
        let options = json!([{"id":"model","currentValue":"anthropic/claude-sonnet-4.6"}]);
        for value in [
            json!({"jsonrpc":"2.0","id":1,"result":{"protocolVersion":1,"agentInfo":{"name":"OpenCode","version":PACKAGE_VERSION},"agentCapabilities":{"sessionCapabilities":{"close":{},"fork":{},"resume":{},"list":{}}}}}),
            json!({"jsonrpc":"2.0","id":2,"result":{"sessionId":"5b0e6d5e-1a2b-4c3d-9e8f-0a1b2c3d4e5f","configOptions":options}}),
            json!({"jsonrpc":"2.0","id":3,"result":{"configOptions":options}}),
            // available_commands_update is forwarded as tool activity, never output.
            json!({"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"5b0e6d5e-1a2b-4c3d-9e8f-0a1b2c3d4e5f","update":{"sessionUpdate":"available_commands_update","availableCommands":[{"name":"/init"}]}}}),
            json!({"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"5b0e6d5e-1a2b-4c3d-9e8f-0a1b2c3d4e5f","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"checked"}}}}),
            json!({"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"5b0e6d5e-1a2b-4c3d-9e8f-0a1b2c3d4e5f","update":{"sessionUpdate":"usage_update","used":1200,"size":200000,"cost":{"amount":0.0125,"currency":"USD"}}}}),
            // Tool call then a three-option permission request.
            json!({"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"5b0e6d5e-1a2b-4c3d-9e8f-0a1b2c3d4e5f","update":{"sessionUpdate":"tool_call","toolCallId":"call-1","title":"read file","status":"in_progress","rawInput":{"path":"x"}}}}),
            json!({"jsonrpc":"2.0","id":7,"method":"session/request_permission","params":{"sessionId":"5b0e6d5e-1a2b-4c3d-9e8f-0a1b2c3d4e5f","toolCall":{"toolCallId":"call-1"},"options":[{"kind":"allow_once","optionId":"once","name":"Allow once"},{"kind":"allow_always","optionId":"always","name":"Always allow"},{"kind":"reject_once","optionId":"reject","name":"Reject"}]}}),
            json!({"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"5b0e6d5e-1a2b-4c3d-9e8f-0a1b2c3d4e5f","update":{"sessionUpdate":"tool_call_update","toolCallId":"call-1","status":"completed","content":[]}}}),
            json!({"jsonrpc":"2.0","id":4,"result":{"stopReason":"end_turn"}}),
        ] {
            tx.send(Ok(value)).await.unwrap();
        }
        // The task is read-only, so the permission is auto-denied; run() must
        // still settle because the tool completion clears the stale approval.
        assert_eq!(runner.run(&req).await.unwrap(), "completed");
        let result = runner.into_result();
        assert_eq!(result.output, "checked");
        assert_eq!(result.usage.as_ref().unwrap()["reported_cost_usd"], 0.0125);
        let _ = control_tx;
        let mut saw_permission = false;
        let mut saw_commands = false;
        while let Ok(event) = event_rx.try_recv() {
            if matches!(event, NativeEvent::PermissionRequested { .. }) {
                saw_permission = true;
            }
            if let NativeEvent::ToolActivity { data } = &event {
                if data["sessionUpdate"] == "available_commands_update" {
                    saw_commands = true;
                }
            }
        }
        assert!(!saw_permission, "read-only tasks never surface approvals");
        assert!(
            saw_commands,
            "available_commands_update forwards as activity"
        );
        let frames: Vec<Value> = written
            .0
            .lock()
            .unwrap()
            .split(|b| *b == b'\n')
            .filter(|v| !v.is_empty())
            .map(|v| serde_json::from_slice(v).unwrap())
            .collect();
        // Model option carries the plain providerID/modelID string.
        assert_eq!(frames[2]["params"]["value"], "anthropic/claude-sonnet-4.6");
        // The three-option request is answered with the one-shot deny.
        let deny = frames
            .iter()
            .find(|frame| frame["id"] == json!(7))
            .expect("permission response recorded");
        assert_eq!(deny["result"]["outcome"]["optionId"], "reject");
    }

    #[test]
    fn dialect_pins_upstream_identity_and_one_shot_permissions() {
        let dialect = MimoDialect;
        assert!(dialect
            .verify_initialize(&json!({
                "protocolVersion":1,
                "agentInfo":{"name":"OpenCode","version":PACKAGE_VERSION},
                "agentCapabilities":{"sessionCapabilities":{"close":{},"fork":{},"resume":{}}}
            }))
            .is_ok());
        assert!(dialect
            .verify_initialize(&json!({
                "protocolVersion":1,
                "agentInfo":{"name":"mimo","version":PACKAGE_VERSION},
                "agentCapabilities":{"sessionCapabilities":{"close":{}}}
            }))
            .is_err());
        assert!(dialect
            .verify_initialize(&json!({
                "protocolVersion":1,
                "agentInfo":{"name":"OpenCode","version":"0.1.14"},
                "agentCapabilities":{"sessionCapabilities":{"close":{}}}
            }))
            .is_err());
        let options = vec![
            json!({"kind":"allow_once","optionId":"once"}),
            json!({"kind":"allow_always","optionId":"always"}),
            json!({"kind":"reject_once","optionId":"reject"}),
        ];
        assert_eq!(
            dialect.permission_selection(&options).unwrap(),
            ("once".to_owned(), "reject".to_owned())
        );
        assert!(dialect
            .permission_selection(&vec![
                json!({"kind":"allow_always","optionId":"always"}),
                json!({"kind":"reject_once","optionId":"reject"}),
            ])
            .is_err());
        assert_eq!(
            dialect.model_value(&request()),
            "anthropic/claude-sonnet-4.6"
        );
        assert_eq!(
            dialect.confirmed_provider(&request()).as_deref(),
            Some("anthropic")
        );
        assert_eq!(dialect.effort_config_id(), None);
        assert!(dialect
            .passthrough_updates()
            .contains(&"available_commands_update"));
        assert!(dialect.stop_status(Some("end_turn")).unwrap() == "completed");
        assert!(dialect.stop_status(Some("max_tokens")).is_err());
        assert!(dialect.stop_status(Some("mystery")).is_err());
        // Config drift fails closed.
        let options = json!([{"id":"model","currentValue":"anthropic/claude-sonnet-4.6"}]);
        assert!(dialect
            .verify_config_options(&options, "anthropic/claude-sonnet-4.6", None)
            .is_ok());
        assert!(dialect
            .verify_config_options(&options, "openai/gpt-5.2", None)
            .is_err());
    }
}
