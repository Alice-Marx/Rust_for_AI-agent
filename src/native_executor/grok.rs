//! Grok Build（xAI，官方 `grok` 二进制）受管 ACP 适配器。
//!
//! 固定协议依据：grok-build 源码快照 `SOURCE_REV`
//! `9bb727ccdff0a793ee73bcde4e2e09cbef6b5387`，外层归档提交
//! `4247f661689354b831191f11eeeac8424993fe3d`。发布版 `1.0.38`
//! 通过 `grok agent stdio` 在 stdin/stdout 上提供 ACP。
//!
//! 本适配器只选择已广告的非交互认证方法，固定模型与推理档位并回读，
//! 严格限制一次性权限。真实安装的固定二进制指纹、账号握手和付费推理仍
//! 属于 H01/H09 的外部条件验证，不在离线 fixture 中冒充完成。

use super::acp::{AcpDialect, AcpRunner};
use crate::native_executor::{emit, NativeControl, NativeEvent, NativeRequest, NativeResult};
use anyhow::{bail, ensure, Context, Result};
use serde_json::{json, Value};
use std::{
    collections::HashSet,
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};
use tokio::{io::BufReader, sync::mpsc};

const VERSION: &str = "1.0.38";
const SOURCE_REV: &str = "9bb727ccdff0a793ee73bcde4e2e09cbef6b5387";
const ARCHIVE_REV: &str = "4247f661689354b831191f11eeeac8424993fe3d";

pub(super) fn validate_request(req: &NativeRequest) -> Result<()> {
    ensure!(req.app_id == "grok", "Grok ACP profile required");
    ensure!(
        is_exact_model_id(&req.model),
        "Grok requires an exact grok-* model ID"
    );
    ensure!(
        req.config_path.is_none(),
        "managed Grok uses its official account configuration; custom config injection is unsupported"
    );
    Ok(())
}

fn is_exact_model_id(model: &str) -> bool {
    model.starts_with("grok-")
        && model.len() > "grok-".len()
        && model.len() <= 160
        && model
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_'))
}

struct Prepared {
    cli: PathBuf,
    sha256: String,
}

fn resolve_binary() -> Result<PathBuf> {
    if let Some(value) = std::env::var_os("WONDERLAND_GROK_BIN") {
        let path = PathBuf::from(value);
        ensure!(
            path.is_absolute(),
            "WONDERLAND_GROK_BIN must be an absolute official binary path"
        );
        return Ok(path);
    }
    crate::desktop_bridge::find_executable("grok").context(
        "Grok official binary not found; install Grok Build from xAI or set WONDERLAND_GROK_BIN",
    )
}

fn verify_installation(cli: &Path) -> Result<Prepared> {
    let cli = std::fs::canonicalize(cli).context("Grok binary is missing")?;
    ensure!(cli.is_file(), "Grok binary path is not a file");
    let sha256 = super::executable_digest(&cli)?;
    Ok(Prepared { cli, sha256 })
}

fn verify_banner(cli: &Path) -> Result<()> {
    let output = std::process::Command::new(cli)
        .arg("--version")
        .output()
        .context("could not execute the Grok binary")?;
    ensure!(
        output.status.success(),
        "Grok --version failed with status {}",
        output.status
    );
    let banner = String::from_utf8_lossy(&output.stdout);
    let first = banner.lines().next().unwrap_or_default().trim();
    ensure!(
        first == format!("grok {VERSION}") || first.starts_with(&format!("grok {VERSION} ")),
        "Grok version banner does not match the pinned release {VERSION}"
    );
    Ok(())
}

pub(super) struct GrokDialect {
    api_key_available: bool,
}

impl GrokDialect {
    fn production() -> Self {
        Self {
            api_key_available: std::env::var_os("XAI_API_KEY")
                .is_some_and(|value| !value.is_empty()),
        }
    }

    #[cfg(test)]
    fn fixture(api_key_available: bool) -> Self {
        Self { api_key_available }
    }

    fn config<'a>(options: &'a [Value], id: &str) -> Result<&'a Value> {
        let matching: Vec<_> = options.iter().filter(|item| item["id"] == id).collect();
        ensure!(
            matching.len() == 1,
            "Grok ACP config option {id} is missing or duplicated"
        );
        Ok(matching[0])
    }
}

impl AcpDialect for GrokDialect {
    fn label(&self) -> &'static str {
        "Grok"
    }

    fn protocol(&self) -> &'static str {
        "grok-acp"
    }

    fn request_prefix(&self) -> &'static str {
        "grok"
    }

    fn status_note(&self) -> String {
        format!(
            "Grok Build official ACP {VERSION}; non-interactive authentication and exact model/effort readback are verified per session. Account plan remains unknown unless the protocol reports it."
        )
    }

    fn verify_initialize(&self, init: &Value) -> Result<()> {
        ensure!(
            init["protocolVersion"] == 1
                && init.pointer("/_meta/grokShell").and_then(Value::as_bool) == Some(true)
                && init.pointer("/_meta/agentVersion").and_then(Value::as_str) == Some(VERSION)
                && init
                    .pointer("/agentCapabilities/sessionCapabilities/close")
                    .is_some(),
            "Grok ACP identity/capability handshake mismatch"
        );
        let methods = init["authMethods"]
            .as_array()
            .context("Grok ACP authentication methods missing")?;
        ensure!(
            !methods.is_empty(),
            "Grok ACP advertised no authentication method"
        );
        let mut seen = HashSet::new();
        for method in methods {
            let id = method["id"]
                .as_str()
                .context("Grok ACP authentication method ID missing")?;
            ensure!(
                !id.is_empty() && id.len() <= 64 && seen.insert(id),
                "invalid or duplicate Grok ACP authentication method"
            );
        }
        Ok(())
    }

    fn authenticate(&self, init: &Value) -> Result<Option<Value>> {
        let methods = init["authMethods"]
            .as_array()
            .context("Grok ACP authentication methods missing")?;
        let ids: Vec<_> = methods
            .iter()
            .filter_map(|method| method["id"].as_str())
            .collect();
        let method = if self.api_key_available {
            ensure!(
                ids.first() == Some(&"xai.api_key"),
                "Grok did not advertise xai.api_key first while XAI_API_KEY is present"
            );
            "xai.api_key"
        } else {
            ensure!(
                ids.contains(&"cached_token"),
                "Grok has no advertised non-interactive cached authentication"
            );
            "cached_token"
        };
        Ok(Some(json!({"methodId":method,"_meta":{"headless":true}})))
    }

    fn confirmed_billing_channel(&self, auth: &Value) -> Option<String> {
        (auth["methodId"] == "xai.api_key").then(|| "api".into())
    }

    fn model_config_id(&self) -> &'static str {
        "model"
    }

    fn model_value(&self, req: &NativeRequest) -> String {
        req.model.clone()
    }

    fn effort_config_id(&self) -> Option<&'static str> {
        Some("reasoning_effort")
    }

    fn verify_initial_config_options(
        &self,
        options: &Value,
        _model: &str,
        _effort: Option<&str>,
    ) -> Result<()> {
        let options = options
            .as_array()
            .context("Grok ACP config options missing")?;
        let model = Self::config(options, "model")?["currentValue"]
            .as_str()
            .context("Grok ACP current model missing")?;
        ensure!(
            is_exact_model_id(model),
            "Grok ACP current model is invalid"
        );
        Ok(())
    }

    fn verify_config_options(
        &self,
        options: &Value,
        model: &str,
        effort: Option<&str>,
    ) -> Result<()> {
        let options = options
            .as_array()
            .context("Grok ACP config options missing")?;
        ensure!(
            Self::config(options, "model")?["currentValue"].as_str() == Some(model),
            "Grok model drift detected"
        );
        if let Some(effort) = effort {
            ensure!(
                Self::config(options, "reasoning_effort")?["currentValue"].as_str() == Some(effort),
                "Grok reasoning-effort drift detected"
            );
        }
        Ok(())
    }

    fn confirmed_provider(&self, req: &NativeRequest) -> Option<String> {
        req.model.starts_with("grok-").then(|| "xai".into())
    }

    fn confirmed_reasoning_effort(&self, options: &Value) -> Option<String> {
        let options = options.as_array()?;
        let matching: Vec<_> = options
            .iter()
            .filter(|item| item["id"] == "reasoning_effort")
            .collect();
        (matching.len() == 1)
            .then(|| matching[0]["currentValue"].as_str().map(str::to_owned))
            .flatten()
    }

    fn permission_selection(&self, options: &[Value]) -> Result<(String, String)> {
        let select = |kind: &str| -> Result<String> {
            let matching: Vec<_> = options
                .iter()
                .filter(|option| option["kind"] == kind)
                .collect();
            ensure!(
                matching.len() == 1,
                "Grok permission options must expose exactly one {kind} choice"
            );
            let id = matching[0]["optionId"]
                .as_str()
                .context("Grok permission option ID missing")?;
            ensure!(
                !id.is_empty() && id.len() <= 256,
                "invalid Grok permission option ID"
            );
            Ok(id.into())
        };
        Ok((select("allow_once")?, select("reject_once")?))
    }

    fn passthrough_updates(&self) -> &'static [&'static str] {
        &[
            "available_commands_update",
            "current_mode_update",
            "plan",
            "user_message_chunk",
        ]
    }

    fn extension_notification(&self, method: &str, params: &Value) -> Option<Value> {
        match method {
            "x.ai/session_notification" => Some(params.clone()),
            "_x.ai/session_notification" if params["method"] == "x.ai/session_notification" => {
                Some(params["params"].clone())
            }
            _ => None,
        }
    }

    fn tool_start_status(&self, status: Option<&str>) -> Result<()> {
        ensure!(
            matches!(status, Some("pending" | "in_progress")),
            "invalid Grok tool start status"
        );
        Ok(())
    }

    fn tool_update_terminal(&self, status: Option<&str>) -> Result<bool> {
        match status {
            None | Some("pending" | "in_progress") => Ok(false),
            Some("completed" | "failed") => Ok(true),
            _ => bail!("invalid Grok tool update status"),
        }
    }

    fn verify_prompt_result(&self, result: &Value, req: &NativeRequest) -> Result<()> {
        ensure!(
            result.pointer("/_meta/modelId").and_then(Value::as_str) == Some(req.model.as_str()),
            "Grok prompt result did not confirm the selected model"
        );
        Ok(())
    }

    fn prompt_usage(&self, result: &Value) -> Result<Option<Value>> {
        let Some(usage) = result.pointer("/_meta/usage") else {
            return Ok(None);
        };
        let input = usage["inputTokens"]
            .as_u64()
            .context("Grok prompt usage inputTokens missing")?;
        let output = usage["outputTokens"]
            .as_u64()
            .context("Grok prompt usage outputTokens missing")?;
        let total = usage["totalTokens"]
            .as_u64()
            .context("Grok prompt usage totalTokens missing")?;
        ensure!(
            input.checked_add(output) == Some(total),
            "Grok prompt usage totals are inconsistent"
        );
        let incomplete = usage["usageIsIncomplete"].as_bool().unwrap_or(false);
        let partial = usage["costIsPartial"].as_bool().unwrap_or(false);
        let mut normalized = json!({
            "kind":"provider_reported_prompt_usage",
            "input_tokens":input,
            "output_tokens":output,
            "total_tokens":total,
            "cached_read_tokens":usage["cachedReadTokens"].as_u64().unwrap_or(0),
            "cache_creation_tokens":usage["cacheCreationTokens"].as_u64().unwrap_or(0),
            "reasoning_tokens":usage["reasoningTokens"].as_u64().unwrap_or(0),
            "model_calls":usage["modelCalls"].as_u64().unwrap_or(0),
            "usage_is_incomplete":incomplete,
            "cost_is_partial":partial,
        });
        if let Some(ticks) = usage["costUsdTicks"].as_i64() {
            ensure!(
                ticks >= 0 && !incomplete && !partial,
                "Grok returned cost for incomplete or partial usage"
            );
            normalized["reported_cost_usd"] = json!(ticks as f64 / 10_000_000_000_f64);
            normalized["reported_cost_usd_ticks"] = json!(ticks);
        }
        Ok(Some(normalized))
    }

    fn stop_status(&self, reason: Option<&str>) -> Result<String> {
        match reason {
            Some("end_turn") => Ok("completed".into()),
            Some("cancelled") => Ok("cancelled".into()),
            Some("max_tokens" | "max_turn_requests") => {
                bail!("Grok stopped at an execution limit; task remains incomplete")
            }
            _ => bail!("Grok ACP returned an unsupported stop reason"),
        }
    }
}

pub(super) async fn execute_with_control(
    req: NativeRequest,
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
        _ = events.closed() => bail!("Grok event consumer disconnected during preparation"),
    };
    let banner_cli = prepared.cli.clone();
    {
        let preparation = tokio::task::spawn_blocking(move || verify_banner(&banner_cli));
        tokio::select! {
            result = preparation => result??,
            _ = super::cancellation(&mut cancel) => return Ok(super::empty_result(&req, "cancelled")),
            _ = tokio::time::sleep_until(deadline) => return Ok(super::empty_result(&req, "timed_out")),
            _ = events.closed() => bail!("Grok event consumer disconnected during version probe"),
        }
    }
    emit(
        &events,
        NativeEvent::Identity {
            executable: prepared.cli.to_string_lossy().into_owned(),
            version: VERSION.into(),
            sha256: prepared.sha256.clone(),
            verification: format!("Official Grok version banner verified against {VERSION}; binary digest recorded, fixed published digest pending real-install verification; source snapshot {SOURCE_REV} (archive {ARCHIVE_REV})"),
        },
    )
    .await?;
    let mut child = tokio::process::Command::new(&prepared.cli)
        .args(["agent", "stdio"])
        .current_dir(&req.cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .context("could not start the official Grok ACP server")?;
    let tree = match crate::process_tree::ProcessTree::attach(
        child.id().context("Grok process has no ID")?,
    ) {
        Ok(tree) => tree,
        Err(error) => {
            let _ = child.start_kill();
            let _ = tokio::time::timeout(Duration::from_secs(2), child.wait()).await;
            return Err(error);
        }
    };
    let stdin = child.stdin.take().context("Grok stdin unavailable")?;
    let stdout = child.stdout.take().context("Grok stdout unavailable")?;
    let (tx, rx) = mpsc::channel(64);
    let reader = tokio::spawn(async move {
        let mut reader = BufReader::new(stdout);
        loop {
            let mut frame = Vec::new();
            let result = match super::read_frame(&mut reader, &mut frame).await {
                Ok(true) => serde_json::from_slice(&frame).context("invalid Grok ACP JSON frame"),
                Ok(false) => Err(anyhow::anyhow!("Grok ACP stream closed before response")),
                Err(error) => Err(error),
            };
            let failed = result.is_err();
            if tx.send(result).await.is_err() || failed {
                break;
            }
        }
    });
    let mut runner = AcpRunner::new(
        Box::new(GrokDialect::production()),
        Box::new(stdin),
        rx,
        controls,
        events.clone(),
        &req,
    );
    runner.set_executable_identity(VERSION, &prepared.sha256);
    let outcome = tokio::select! {
        result = runner.run(&req) => result,
        _ = super::cancellation(&mut cancel) => Ok("cancelled".into()),
        _ = tokio::time::sleep_until(deadline) => Ok("timed_out".into()),
        _ = events.closed() => Err(anyhow::anyhow!("Grok event consumer disconnected")),
    };
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
    use std::{
        pin::Pin,
        sync::{Arc, Mutex},
        task::{Context as TaskContext, Poll},
    };
    use tokio::io::AsyncWrite;

    const SESSION: &str = "e9242386-45ef-4a29-a730-02ca1175ed3e";

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
                .split(|byte| *byte == b'\n')
                .filter(|frame| !frame.is_empty())
                .map(|frame| serde_json::from_slice(frame).unwrap())
                .collect()
        }
    }

    fn request(read_only: bool) -> NativeRequest {
        NativeRequest {
            app_id: "grok".into(),
            model: "grok-4.5".into(),
            cwd: std::env::current_dir().unwrap(),
            prompt: "inspect only".into(),
            read_only,
            max_duration_secs: 30,
            reasoning_effort: Some("high".into()),
            config_path: None,
        }
    }

    fn options(model: &str, effort: &str) -> Value {
        json!([
            {"id":"model","currentValue":model,"options":[{"value":"grok-4"},{"value":"grok-4.5"}]},
            {"id":"reasoning_effort","currentValue":effort,"options":[{"value":"low"},{"value":"high"}]}
        ])
    }

    fn initialize(methods: Value) -> Value {
        json!({
            "protocolVersion":1,
            "authMethods":methods,
            "agentCapabilities":{"loadSession":true,"sessionCapabilities":{"close":{},"list":{},"resume":{}}},
            "_meta":{"grokShell":true,"agentVersion":VERSION}
        })
    }

    fn fixture(
        req: &NativeRequest,
        api_key: bool,
    ) -> (
        AcpRunner,
        Recorded,
        mpsc::Sender<Result<Value>>,
        mpsc::Receiver<NativeEvent>,
    ) {
        let written = Recorded::default();
        let (tx, rx) = mpsc::channel(32);
        let (_control_tx, control_rx) = mpsc::channel(8);
        let (event_tx, event_rx) = mpsc::channel(64);
        (
            AcpRunner::new(
                Box::new(GrokDialect::fixture(api_key)),
                Box::new(written.clone()),
                rx,
                control_rx,
                event_tx,
                req,
            ),
            written,
            tx,
            event_rx,
        )
    }

    #[test]
    fn request_and_dialect_negative_cases_fail_closed() {
        let mut req = request(true);
        assert!(validate_request(&req).is_ok());
        for model in ["gpt-5", "GROK-4", "grok-4/high", "grok 4", "grok-"] {
            req.model = model.into();
            assert!(validate_request(&req).is_err(), "{model}");
        }
        let dialect = GrokDialect::fixture(true);
        assert!(dialect
            .verify_initialize(&initialize(json!([{"id":"xai.api_key"}])))
            .is_ok());
        for bad in [
            json!({"protocolVersion":1,"authMethods":[{"id":"xai.api_key"}],"agentCapabilities":{"sessionCapabilities":{"close":{}}},"_meta":{"agentVersion":VERSION}}),
            json!({"protocolVersion":1,"authMethods":[{"id":"xai.api_key"}],"agentCapabilities":{"sessionCapabilities":{"close":{}}},"_meta":{"grokShell":true,"agentVersion":"1.0.39"}}),
            initialize(json!([])),
            initialize(json!([{"id":"xai.api_key"},{"id":"xai.api_key"}])),
        ] {
            assert!(dialect.verify_initialize(&bad).is_err());
        }
    }

    #[test]
    fn authentication_only_selects_advertised_noninteractive_methods() {
        let api = GrokDialect::fixture(true);
        let init = initialize(json!([
            {"id":"xai.api_key"},
            {"id":"cached_token"},
            {"id":"grok.com"}
        ]));
        let auth = api.authenticate(&init).unwrap().unwrap();
        assert_eq!(
            auth,
            json!({"methodId":"xai.api_key","_meta":{"headless":true}})
        );
        assert_eq!(api.confirmed_billing_channel(&auth).as_deref(), Some("api"));

        let cached = GrokDialect::fixture(false);
        let auth = cached.authenticate(&init).unwrap().unwrap();
        assert_eq!(auth["methodId"], "cached_token");
        assert!(cached.confirmed_billing_channel(&auth).is_none());
        assert!(cached
            .authenticate(&initialize(json!([{"id":"grok.com"}])))
            .is_err());
        assert!(api
            .authenticate(&initialize(
                json!([{"id":"cached_token"},{"id":"xai.api_key"}])
            ))
            .is_err());
    }

    #[tokio::test]
    async fn full_grok_acp_lifecycle_is_pinned_and_auditable() {
        let req = request(true);
        let (mut runner, written, tx, mut events) = fixture(&req, true);
        for frame in [
            json!({"jsonrpc":"2.0","id":1,"result":initialize(json!([{"id":"xai.api_key"},{"id":"cached_token"},{"id":"grok.com"}]))}),
            json!({"jsonrpc":"2.0","id":2,"result":{}}),
            json!({"jsonrpc":"2.0","id":3,"result":{"sessionId":SESSION,"configOptions":options("grok-4","low")}}),
            json!({"jsonrpc":"2.0","id":4,"result":{"configOptions":options("grok-4.5","low")}}),
            json!({"jsonrpc":"2.0","id":5,"result":{"configOptions":options("grok-4.5","high")}}),
            json!({"jsonrpc":"2.0","method":"session/update","params":{"sessionId":SESSION,"update":{"sessionUpdate":"tool_call","toolCallId":"tool-1","title":"read file","status":"pending","rawInput":{"path":"README.md"}}}}),
            json!({"jsonrpc":"2.0","id":81,"method":"session/request_permission","params":{"sessionId":SESSION,"toolCall":{"toolCallId":"tool-1"},"options":[{"kind":"allow_once","optionId":"allow-this"},{"kind":"allow_always","optionId":"always"},{"kind":"reject_once","optionId":"reject-this"}]}}),
            json!({"jsonrpc":"2.0","method":"session/update","params":{"sessionId":SESSION,"update":{"sessionUpdate":"tool_call_update","toolCallId":"tool-1","status":"in_progress"}}}),
            json!({"jsonrpc":"2.0","method":"x.ai/session_notification","params":{"sessionId":SESSION,"update":{"sessionUpdate":"model_changed","model_id":"grok-4.5","reasoning_effort":"high"}}}),
            json!({"jsonrpc":"2.0","method":"session/update","params":{"sessionId":SESSION,"update":{"sessionUpdate":"agent_thought_chunk","content":{"type":"text","text":"checking"}}}}),
            json!({"jsonrpc":"2.0","method":"session/update","params":{"sessionId":SESSION,"update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"checked"}}}}),
            json!({"jsonrpc":"2.0","method":"session/update","params":{"sessionId":SESSION,"update":{"sessionUpdate":"tool_call_update","toolCallId":"tool-1","status":"completed","content":[]}}}),
            json!({"jsonrpc":"2.0","id":6,"result":{"stopReason":"end_turn","_meta":{"modelId":"grok-4.5","usage":{"inputTokens":100,"outputTokens":20,"totalTokens":120,"cachedReadTokens":30,"reasoningTokens":5,"modelCalls":1,"costUsdTicks":2000000000}}}}),
        ] {
            tx.send(Ok(frame)).await.unwrap();
        }
        assert_eq!(runner.run(&req).await.unwrap(), "completed");
        let result = runner.into_result();
        assert_eq!(result.output, "checked");
        assert_eq!(
            result.identity.configured_model.as_deref(),
            Some("grok-4.5")
        );
        assert_eq!(result.identity.effective_model.as_deref(), Some("grok-4.5"));
        assert_eq!(result.identity.effective_provider.as_deref(), Some("xai"));
        assert_eq!(
            result.identity.effective_reasoning_effort.as_deref(),
            Some("high")
        );
        assert_eq!(result.identity.billing_channel.as_deref(), Some("api"));
        assert_eq!(result.usage.as_ref().unwrap()["reported_cost_usd"], 0.2);

        let frames = written.frames();
        assert_eq!(frames[0]["method"], "initialize");
        assert_eq!(
            frames[0]["params"]["_meta"]["startupHints"]["nonInteractive"],
            true
        );
        assert_eq!(frames[1]["method"], "authenticate");
        assert_eq!(frames[1]["params"]["methodId"], "xai.api_key");
        assert_eq!(frames[1]["params"]["_meta"]["headless"], true);
        assert_eq!(frames[2]["method"], "session/new");
        assert_eq!(frames[3]["params"]["configId"], "model");
        assert_eq!(frames[4]["params"]["configId"], "reasoning_effort");
        assert_eq!(frames[5]["method"], "session/prompt");
        assert_eq!(frames[6]["id"], 81);
        assert_eq!(frames[6]["result"]["outcome"]["optionId"], "reject-this");

        let mut thought = false;
        let mut extension = false;
        let mut usage = false;
        while let Ok(event) = events.try_recv() {
            thought |= matches!(event, NativeEvent::ReasoningDelta { .. });
            if let NativeEvent::ToolActivity { data } = &event {
                extension |= data["sessionUpdate"] == "model_changed";
            }
            usage |= matches!(event, NativeEvent::Usage { .. });
        }
        assert!(thought && extension && usage);
    }

    #[tokio::test]
    async fn authentication_rejection_is_sanitized_and_stops_before_session() {
        let req = request(true);
        let (mut runner, written, tx, _events) = fixture(&req, true);
        tx.send(Ok(
            json!({"jsonrpc":"2.0","id":1,"result":initialize(json!([{"id":"xai.api_key"}]))}),
        ))
        .await
        .unwrap();
        tx.send(Ok(
            json!({"jsonrpc":"2.0","id":2,"error":{"code":-32000,"message":"secret-value"}}),
        ))
        .await
        .unwrap();
        let error = runner.run(&req).await.unwrap_err().to_string();
        assert!(error.contains("code -32000"));
        assert!(!error.contains("secret-value"));
        assert_eq!(written.frames().len(), 2);
    }

    #[test]
    fn drift_bad_permissions_extensions_and_partial_costs_fail_closed() {
        let dialect = GrokDialect::fixture(false);
        assert!(dialect
            .verify_config_options(&options("grok-4", "high"), "grok-4.5", Some("high"))
            .is_err());
        assert!(dialect
            .permission_selection(&[json!({"kind":"allow_once","optionId":"allow"})])
            .is_err());
        assert!(dialect
            .prompt_usage(&json!({"_meta":{"usage":{"inputTokens":2,"outputTokens":1,"totalTokens":3,"usageIsIncomplete":true,"costUsdTicks":1}}}))
            .is_err());

        let wrapped = dialect
            .extension_notification(
                "_x.ai/session_notification",
                &json!({"method":"x.ai/session_notification","params":{"sessionId":SESSION,"update":{"sessionUpdate":"retry_state","attempt":1}}}),
            )
            .unwrap();
        assert_eq!(wrapped["sessionId"], SESSION);
        assert_eq!(wrapped["update"]["sessionUpdate"], "retry_state");
        assert!(dialect
            .extension_notification("x.ai/unknown", &json!({}))
            .is_none());
    }

    #[test]
    fn cancellation_and_limits_are_not_reported_as_success() {
        let dialect = GrokDialect::fixture(false);
        assert_eq!(dialect.stop_status(Some("cancelled")).unwrap(), "cancelled");
        assert!(dialect.stop_status(Some("max_tokens")).is_err());
        assert!(dialect.stop_status(Some("refusal")).is_err());
    }
}
