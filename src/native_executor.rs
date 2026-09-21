//! Official harness processes, with structured events and explicit permissions.
//! This adapter owns process lifetime; it never substitutes a direct model API.
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{collections::HashMap, path::PathBuf, process::Stdio, time::Duration};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    sync::{mpsc, watch},
};

mod claude;
mod deepseek;

/// Implemented host controls. These describe our adapter, not account access.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct NativeCapabilities {
    pub managed: bool,
    pub read_only: bool,
    pub permissions: bool,
    pub questions: bool,
    pub reasoning_efforts: Vec<String>,
    pub protocol: Option<String>,
    pub resume: bool,
    pub fork: bool,
}

pub fn capabilities(app_id: &str) -> NativeCapabilities {
    let protocol = match app_id {
        "codex" => Some("codex-app-server"),
        "kimi-cli" => Some("kimi-wire"),
        "claude" => Some("claude-stream-json"),
        "deepseek" => Some("deepseek-acp"),
        _ => None,
    };
    let managed = protocol.is_some();
    NativeCapabilities {
        managed,
        read_only: managed,
        permissions: managed,
        questions: managed && app_id != "deepseek",
        reasoning_efforts: match app_id {
            "codex" => &[
                "none", "minimal", "low", "medium", "high", "xhigh", "max", "ultra",
            ][..],
            "claude" => &["low", "medium", "high", "xhigh", "max"][..],
            "deepseek" => &["off", "low", "high", "max"][..],
            _ => &[],
        }
        .iter()
        .map(|value| (*value).into())
        .collect(),
        protocol: protocol.map(str::to_owned),
        resume: false,
        fork: false,
    }
}

/// Structural validation is also used before storing a draft. Executable,
/// account and protocol identity remain checks performed at execution time.
pub fn validate_binding(
    app_id: &str,
    model: &str,
    effort: Option<&str>,
    read_only: bool,
) -> Result<()> {
    let caps = capabilities(app_id);
    anyhow::ensure!(
        caps.managed,
        "this application has no managed native adapter"
    );
    anyhow::ensure!(
        !read_only || caps.read_only,
        "this adapter does not support read-only tasks"
    );
    anyhow::ensure!(
        !model.is_empty() && model.len() <= 160 && !model.contains(['\0', '\r', '\n', ' ']),
        "invalid exact model ID"
    );
    let matches_provider = match app_id {
        "codex" => {
            model.starts_with("gpt-")
                || ["o1", "o3", "o4"]
                    .iter()
                    .any(|prefix| model == *prefix || model.starts_with(&format!("{prefix}-")))
        }
        "kimi-cli" => model.starts_with("kimi-"),
        "claude" => model.starts_with("claude-"),
        "deepseek" => model.starts_with("deepseek-"),
        _ => false,
    };
    anyhow::ensure!(
        matches_provider,
        "requested model does not belong to this official tool's provider"
    );
    if let Some(effort) = effort {
        anyhow::ensure!(
            caps.reasoning_efforts.iter().any(|item| item == effort),
            "reasoning effort is unsupported by this native adapter"
        );
    }
    Ok(())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NativeRequest {
    pub app_id: String,
    pub model: String,
    pub cwd: PathBuf,
    pub prompt: String,
    pub read_only: bool,
    pub max_duration_secs: u64,
    #[serde(default)]
    pub reasoning_effort: Option<String>,
    /// Optional official Kimi configuration file. Never included in events.
    #[serde(default)]
    pub config_path: Option<PathBuf>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum NativeEvent {
    Identity {
        executable: String,
        version: String,
        sha256: String,
        verification: String,
    },
    Started {
        app_id: String,
        model: String,
        protocol: String,
    },
    SessionStarted {
        session_id: String,
    },
    TextDelta {
        text: String,
    },
    ReasoningDelta {
        text: String,
    },
    ToolActivity {
        data: Value,
    },
    Usage {
        data: Value,
    },
    PermissionRequested {
        request_id: String,
        description: String,
        data: Value,
    },
    PermissionResolved {
        request_id: String,
        approved: bool,
    },
    QuestionRequested {
        request_id: String,
        data: Value,
    },
    Status {
        message: String,
    },
    Completed {
        status: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum NativeControl {
    Permission {
        request_id: String,
        approve: bool,
    },
    /// Kimi: map question IDs to answer strings; Codex: map IDs to {answers: [...]}.
    Answer {
        request_id: String,
        answers: Value,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NativeResult {
    pub app_id: String,
    pub model: String,
    pub session_id: Option<String>,
    pub output: String,
    /// completed, cancelled, or timed_out. Protocol/identity failures return Err.
    pub status: String,
    pub usage: Option<Value>,
}

const MAX_FRAME_BYTES: usize = 2 * 1024 * 1024;
const MAX_OUTPUT_BYTES: usize = 8 * 1024 * 1024;
const SEND_TIMEOUT: Duration = Duration::from_secs(5);

/// True means an adapter is implemented; account, executable and handshake
/// validation still happen for each invocation before any prompt is submitted.
pub fn supports_native(app_id: &str) -> bool {
    capabilities(app_id).managed
}

/// Without a control channel all permission requests are denied. Use
/// execute_with_control when the desktop can collect explicit user decisions.
pub async fn execute(
    req: NativeRequest,
    events: mpsc::Sender<NativeEvent>,
    cancel: watch::Receiver<bool>,
) -> Result<NativeResult> {
    let (tx, rx) = mpsc::channel(1);
    drop(tx);
    execute_with_control(req, events, cancel, rx).await
}

pub async fn execute_with_control(
    req: NativeRequest,
    events: mpsc::Sender<NativeEvent>,
    mut cancel: watch::Receiver<bool>,
    controls: mpsc::Receiver<NativeControl>,
) -> Result<NativeResult> {
    validate_request(&req)?;
    if req.app_id == "claude" {
        return claude::execute(req, events, cancel, controls).await;
    }
    if req.app_id == "deepseek" {
        return deepseek::execute_with_control(req, events, cancel, controls).await;
    }
    let deadline = tokio::time::Instant::now() + Duration::from_secs(req.max_duration_secs);
    if *cancel.borrow() {
        return Ok(empty_result(&req, "cancelled"));
    }
    let prepared_req = req.clone();
    let preparation = tokio::task::spawn_blocking(move || prepare_native(&prepared_req));
    let prepared = tokio::select! {
        result = preparation => result??,
        _ = cancellation(&mut cancel) => return Ok(empty_result(&req, "cancelled")),
        _ = tokio::time::sleep_until(deadline) => return Ok(empty_result(&req, "timed_out")),
        _ = events.closed() => bail!("native event consumer disconnected during preparation"),
    };
    if *cancel.borrow() {
        return Ok(empty_result(&req, "cancelled"));
    }
    emit(&events, NativeEvent::Identity {
        executable:prepared.identity_path.to_string_lossy().into_owned(),
        version:prepared.version.clone(), sha256:prepared.sha256.clone(),
        verification:"Local executable path, version banner and file digest recorded; publisher signature and remote model identity are not attested".into(),
    }).await?;
    let mut command = tokio::process::Command::new(&prepared.spec.executable);
    command
        .args(&prepared.spec.args)
        .current_dir(&prepared.spec.cwd)
        .envs(&prepared.spec.env)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        // Harness stderr may contain provider credentials or unsanitized dumps.
        .stderr(Stdio::null())
        .kill_on_drop(true);
    for name in [
        "OPENAI_BASE_URL",
        "OPENAI_API_BASE",
        "KIMI_BASE_URL",
        "KIMI_MODEL_NAME",
        "KIMI_MODEL_CAPABILITIES",
    ] {
        command.env_remove(name);
    }
    #[cfg(windows)]
    command.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    #[cfg(unix)]
    command.process_group(0);
    let mut child = command
        .spawn()
        .context("could not start official harness")?;
    let process_tree = match crate::process_tree::ProcessTree::attach(
        child.id().context("missing child identity")?,
    ) {
        Ok(tree) => tree,
        Err(error) => {
            let _ = child.kill().await;
            return Err(error.context("cannot contain official harness process tree"));
        }
    };
    let stdin = child.stdin.take().context("harness stdin unavailable")?;
    let stdout = child.stdout.take().context("harness stdout unavailable")?;
    let (frames_tx, frames_rx) = mpsc::channel(64);
    let reader = tokio::spawn(async move {
        let mut reader = BufReader::new(stdout);
        loop {
            let mut frame = Vec::new();
            let result = read_frame(&mut reader, &mut frame).await;
            let message = match result {
                Ok(false) => Err(anyhow::anyhow!(
                    "official harness closed its protocol stream"
                )),
                Ok(true) => {
                    serde_json::from_slice::<Value>(&frame).context("invalid harness JSON frame")
                }
                Err(e) => Err(e),
            };
            let failed = message.is_err();
            if frames_tx.send(message).await.is_err() || failed {
                break;
            }
        }
    });
    let mut runner = Runner {
        stdin: Box::new(stdin),
        frames: frames_rx,
        controls,
        controls_open: true,
        events: events.clone(),
        pending: HashMap::new(),
        next_id: 0,
        prompt_id: None,
        result: empty_result(&req, "completed"),
        protocol: if req.app_id == "codex" {
            Protocol::Codex
        } else {
            Protocol::KimiWire
        },
        read_only: req.read_only,
        turn_id: None,
    };
    runner.result.session_id = prepared.session_id.clone();
    let outcome = tokio::select! {
        result = runner.run(&req, &prepared.resolved_model) => result,
        _ = cancellation(&mut cancel) => Ok("cancelled".to_owned()),
        _ = tokio::time::sleep_until(deadline) => Ok("timed_out".to_owned()),
        _ = events.closed() => Err(anyhow::anyhow!("native event consumer disconnected")),
    };
    // Always attempt cooperative cancellation before bounded force cleanup,
    // including protocol errors and UI disconnection.
    if !matches!(&outcome, Ok(status) if status == "completed") {
        let _ = tokio::time::timeout(Duration::from_millis(500), runner.interrupt()).await;
    }
    drop(runner.stdin);
    let _ = tokio::time::timeout(Duration::from_millis(400), child.wait()).await;
    drop(process_tree);
    let _ = child.start_kill();
    let _ = tokio::time::timeout(Duration::from_secs(2), child.wait()).await;
    reader.abort();
    let _ = reader.await;
    drop(prepared);
    let status = outcome?;
    runner.result.status = status.clone();
    emit(&events, NativeEvent::Completed { status }).await?;
    Ok(runner.result)
}

async fn cancellation(cancel: &mut watch::Receiver<bool>) {
    loop {
        if *cancel.borrow() || cancel.changed().await.is_err() {
            return;
        }
    }
}

fn empty_result(req: &NativeRequest, status: &str) -> NativeResult {
    NativeResult {
        app_id: req.app_id.clone(),
        model: req.model.clone(),
        session_id: None,
        output: String::new(),
        status: status.into(),
        usage: None,
    }
}

fn validate_request(req: &NativeRequest) -> Result<()> {
    validate_binding(
        &req.app_id,
        &req.model,
        req.reasoning_effort.as_deref(),
        req.read_only,
    )?;
    anyhow::ensure!(
        supports_native(&req.app_id),
        "native control is unavailable for {}; use its manual terminal",
        req.app_id
    );
    anyhow::ensure!(
        req.cwd.is_absolute() && req.cwd.is_dir(),
        "native workspace must be an existing absolute directory"
    );
    anyhow::ensure!(
        !req.prompt.trim().is_empty() && req.prompt.len() <= MAX_FRAME_BYTES / 2,
        "prompt is empty or too large"
    );
    anyhow::ensure!(
        (1..=86400).contains(&req.max_duration_secs),
        "duration must be 1-86400 seconds"
    );
    anyhow::ensure!(
        req.app_id == "kimi-cli" || req.config_path.is_none(),
        "this tool uses its official account configuration; arbitrary config injection is unsupported"
    );
    if req.app_id == "claude" {
        claude::validate(&req)?;
    }
    if req.app_id == "deepseek" {
        deepseek::validate_request(&req)?;
    }
    Ok(())
}

async fn emit(events: &mpsc::Sender<NativeEvent>, event: NativeEvent) -> Result<()> {
    tokio::time::timeout(SEND_TIMEOUT, events.send(event))
        .await
        .context("native event consumer stopped draining")?
        .context("native event consumer disconnected")
}

async fn read_frame<R: tokio::io::AsyncBufRead + Unpin>(
    reader: &mut R,
    frame: &mut Vec<u8>,
) -> Result<bool> {
    loop {
        let buf = reader.fill_buf().await?;
        if buf.is_empty() {
            anyhow::ensure!(frame.is_empty(), "incomplete harness JSON frame");
            return Ok(false);
        }
        let end = buf.iter().position(|b| *b == b'\n');
        let take = end.map_or(buf.len(), |i| i + 1);
        anyhow::ensure!(
            frame.len() + take <= MAX_FRAME_BYTES,
            "harness frame exceeds size limit"
        );
        frame.extend_from_slice(&buf[..take]);
        reader.consume(take);
        if end.is_some() {
            return Ok(true);
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum Protocol {
    Codex,
    KimiWire,
}

struct PreparedNative {
    spec: crate::desktop_bridge::LaunchSpec,
    resolved_model: String,
    session_id: Option<String>,
    identity_path: PathBuf,
    version: String,
    sha256: String,
    _temporary: Option<PrivateFiles>,
}

fn prepare_native(req: &NativeRequest) -> Result<PreparedNative> {
    let profile = crate::desktop_bridge::default_cli_profiles()
        .into_iter()
        .find(|profile| profile.id == req.app_id)
        .context("official tool profile is not registered")?;
    let mut profile = profile;
    let mut temporary = None;
    let mut resolved_model = req.model.clone();
    let mut session_id = None;
    match req.app_id.as_str() {
        "codex" => {
            profile.args = vec![
                "app-server".into(),
                "--listen".into(),
                "stdio://".into(),
                "-c".into(),
                "model_provider=\"openai\"".into(),
                "-c".into(),
                "features.multi_agent=false".into(),
                "-c".into(),
                "features.multi_agent_v2=false".into(),
                "-c".into(),
                "features.guardian_approval=false".into(),
                "-c".into(),
                "features.guardianv2=false".into(),
                "-c".into(),
                "features.memories=false".into(),
                "-c".into(),
                "approvals_reviewer=\"user\"".into(),
            ];
        }
        "kimi-cli" => {
            if crate::desktop_bridge::find_executable(&profile.executable).is_none() {
                profile.executable = "kimi".into();
            }
            let share_dir = std::env::var_os("KIMI_SHARE_DIR")
                .filter(|value| !value.is_empty())
                .map(PathBuf::from)
                .or_else(|| {
                    std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" })
                        .map(|home| PathBuf::from(home).join(".kimi"))
                })
                .context("cannot locate official Kimi account directory")?;
            let share_dir = if share_dir.is_absolute() {
                share_dir
            } else {
                req.cwd.join(share_dir)
            };
            // Upstream loads plugin tools after applying the agent allowlist.
            // Such tools may mutate files or route to a different model, so a
            // fixed-model invocation must fail before submitting its prompt.
            reject_kimi_plugins(&share_dir.join("plugins"))?;
            let config_path = req
                .config_path
                .clone()
                .unwrap_or_else(|| share_dir.join("config.toml"));
            let config_path = if config_path.is_absolute() {
                config_path
            } else {
                req.cwd.join(config_path)
            };
            let content = read_config_bounded(&config_path)?;
            let config: Value = if config_path.extension().is_some_and(|ext| ext == "json") {
                serde_json::from_str(&content)
                    .map_err(|_| anyhow::anyhow!("invalid Kimi JSON configuration"))?
            } else {
                toml::from_str(&content)
                    .map_err(|_| anyhow::anyhow!("invalid Kimi TOML configuration"))?
            };
            let (mut config, alias, actual) = restricted_kimi_config(config, &req.model)?;
            config["default_yolo"] = json!(false);
            config["default_plan_mode"] = json!(req.read_only);
            config["hooks"] = json!([]);
            resolved_model = actual;
            let mut files = PrivateFiles::create()?;
            let file = files.write("config.json", &serde_json::to_vec(&config)?)?;
            // Official agent inheritance preserves its prompt and runtime, while
            // disabling nested model routing and mutation tools for Chat.
            let agent = files.write(
                "agent.json",
                &serde_json::to_vec(&kimi_agent_config(req.read_only))?,
            )?;
            let mcp = files.write("mcp.json", br#"{"mcpServers":{}}"#)?;
            let session = uuid::Uuid::new_v4().to_string();
            session_id = Some(session.clone());
            profile.args = vec![
                "--wire".into(),
                "--config-file".into(),
                file.to_string_lossy().into_owned(),
                "--model".into(),
                alias,
                "--agent-file".into(),
                agent.to_string_lossy().into_owned(),
                "--session".into(),
                session,
                "--mcp-config-file".into(),
                mcp.to_string_lossy().into_owned(),
            ];
            temporary = Some(files);
        }
        _ => bail!("native executor has no adapter for {}", req.app_id),
    }
    // Probe before protocol startup; do not interpret an arbitrary same-name
    // program's banner as cryptographic publisher verification.
    let mut probe = profile.clone();
    probe.args.clear();
    let status = crate::desktop_bridge::detect_cli(&probe);
    anyhow::ensure!(status.error.is_none(), "official tool version probe failed");
    let version = status.version.unwrap_or_default();
    let lowercase_version = version.to_ascii_lowercase();
    let identity_matches = if req.app_id == "codex" {
        lowercase_version.contains("codex")
    } else {
        lowercase_version.starts_with("kimi, version ")
            || lowercase_version.starts_with("kimi-cli, version ")
    };
    anyhow::ensure!(
        identity_matches,
        "installed executable identity does not match requested official tool"
    );
    let identity_path = status
        .executable
        .context("version probe did not retain executable identity")?;
    profile.executable = identity_path.to_string_lossy().into_owned();
    let sha256 = executable_digest(&identity_path)?;
    // For the official npm Codex shim, use its Node entry directly so JSON stdin
    // cannot be consumed or recoded by an intermediate shell.
    #[cfg(windows)]
    if req.app_id == "codex" {
        if let Some(path) = crate::desktop_bridge::find_executable(&profile.executable) {
            if path
                .extension()
                .is_some_and(|ext| ext == "cmd" || ext == "ps1")
            {
                let entry = path
                    .parent()
                    .context("invalid Codex installation")?
                    .join("node_modules/@openai/codex/bin/codex.js");
                anyhow::ensure!(
                    entry.is_file(),
                    "Codex shell shim has no official npm entry; select a native installation"
                );
                let package: Value = serde_json::from_slice(&std::fs::read(
                    entry
                        .parent()
                        .and_then(std::path::Path::parent)
                        .context("invalid Codex package path")?
                        .join("package.json"),
                )?)?;
                anyhow::ensure!(
                    package.get("name").and_then(Value::as_str) == Some("@openai/codex"),
                    "Codex npm package identity mismatch"
                );
                profile.executable = "node".into();
                profile.args.insert(0, entry.to_string_lossy().into_owned());
            }
        }
    }
    let spec = crate::desktop_bridge::prepare_native_cli(&profile, &req.cwd, &[])?;
    Ok(PreparedNative {
        spec,
        resolved_model,
        session_id,
        identity_path,
        version,
        sha256,
        _temporary: temporary,
    })
}

fn kimi_agent_config(read_only: bool) -> Value {
    let mut config = json!({"version":1,"agent":{"extend":"default","exclude_tools":[
        "kimi_cli.tools.agent:Agent", "kimi_cli.tools.plan:ExitPlanMode", "kimi_cli.tools.plan.enter:EnterPlanMode"
    ],"subagents":{}}});
    if read_only {
        // An allowlist keeps newly introduced upstream tools out of Chat until
        // their effects have been reviewed. This is a tool boundary, not an OS sandbox.
        config["agent"]["allowed_tools"] = json!([
            "kimi_cli.tools.ask_user:AskUserQuestion",
            "kimi_cli.tools.todo:SetTodoList",
            "kimi_cli.tools.file:ReadFile",
            "kimi_cli.tools.file:ReadMediaFile",
            "kimi_cli.tools.file:Glob",
            "kimi_cli.tools.file:Grep",
            "kimi_cli.tools.web:SearchWeb",
            "kimi_cli.tools.web:FetchURL"
        ]);
    }
    config
}

fn reject_kimi_plugins(directory: &std::path::Path) -> Result<()> {
    let entries = match std::fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(_) => bail!("cannot inspect installed Kimi plugins before native execution"),
    };
    for (index, entry) in entries.enumerate() {
        anyhow::ensure!(
            index < 4096,
            "Kimi plugin directory exceeds inspection limit"
        );
        let entry = entry.context("cannot inspect installed Kimi plugin entry")?;
        anyhow::ensure!(!entry.path().join("plugin.json").is_file(),
            "installed Kimi plugins can bypass fixed-model and Chat tool restrictions; use an official account profile without plugins");
    }
    Ok(())
}

fn restricted_kimi_config(mut config: Value, requested: &str) -> Result<(Value, String, String)> {
    let models = config
        .get("models")
        .and_then(Value::as_object)
        .context("Kimi configuration contains no models")?;
    let (alias, model) = models
        .get_key_value(requested)
        .or_else(|| {
            models
                .iter()
                .find(|(_, v)| v.get("model").and_then(Value::as_str) == Some(requested))
        })
        .context("requested Kimi model is not configured")?;
    let actual = model
        .get("model")
        .and_then(Value::as_str)
        .context("Kimi model identity is missing")?
        .to_owned();
    anyhow::ensure!(
        actual == requested,
        "Kimi alias resolves to a different model; request the exact configured model ID"
    );
    let provider_key = model
        .get("provider")
        .and_then(Value::as_str)
        .context("Kimi provider missing")?
        .to_owned();
    let mut provider = config
        .get("providers")
        .and_then(|v| v.get(&provider_key))
        .cloned()
        .context("Kimi provider configuration missing")?;
    anyhow::ensure!(
        provider.get("type").and_then(Value::as_str) == Some("kimi"),
        "native Kimi requires its official provider"
    );
    let url = url::Url::parse(
        provider
            .get("base_url")
            .and_then(Value::as_str)
            .context("Kimi endpoint missing")?,
    )
    .map_err(|_| anyhow::anyhow!("invalid Kimi endpoint"))?;
    anyhow::ensure!(
        url.scheme() == "https"
            && url.username().is_empty()
            && url.password().is_none()
            && matches!(
                url.host_str(),
                Some("api.kimi.com" | "api.moonshot.cn" | "api.moonshot.ai")
            )
            && url.port_or_known_default() == Some(443),
        "Kimi endpoint is not an allowed official service"
    );
    anyhow::ensure!(
        provider
            .get("env")
            .is_none_or(|v| v.is_null() || v.as_object().is_some_and(|m| m.is_empty())),
        "provider environment overrides require manual review"
    );
    if let Some(object) = provider.as_object_mut() {
        object.remove("env");
    }
    let alias = alias.clone();
    let model = model.clone();
    config["models"] = json!({alias.clone(): model});
    config["providers"] = json!({provider_key: provider});
    config["default_model"] = json!(alias);
    Ok((config, alias, actual))
}

fn read_config_bounded(path: &std::path::Path) -> Result<String> {
    use std::io::Read;
    let file = std::fs::File::open(path)
        .context("cannot read official Kimi configuration; log in with Kimi first")?;
    anyhow::ensure!(
        file.metadata()?.is_file(),
        "Kimi configuration must be a regular file"
    );
    let mut text = String::new();
    file.take((MAX_FRAME_BYTES + 1) as u64)
        .read_to_string(&mut text)
        .map_err(|_| anyhow::anyhow!("could not decode official Kimi configuration"))?;
    anyhow::ensure!(
        text.len() <= MAX_FRAME_BYTES,
        "Kimi configuration exceeds size limit"
    );
    Ok(text)
}

fn executable_digest(path: &std::path::Path) -> Result<String> {
    use sha2::{Digest, Sha256};
    use std::io::Read;
    let mut file = std::fs::File::open(path).context("cannot fingerprint native executable")?;
    anyhow::ensure!(
        file.metadata()?.is_file(),
        "native executable is not a regular file"
    );
    let mut hash = Sha256::new();
    let mut buffer = [0u8; 65536];
    let mut total = 0u64;
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        total += count as u64;
        anyhow::ensure!(
            total <= 1024 * 1024 * 1024,
            "native executable is too large to fingerprint"
        );
        hash.update(&buffer[..count]);
    }
    Ok(format!("{:x}", hash.finalize()))
}

struct PrivateFiles {
    directory: PathBuf,
    files: Vec<PathBuf>,
}
impl PrivateFiles {
    fn create() -> Result<Self> {
        let directory =
            std::env::temp_dir().join(format!("wonderland-native-{}", uuid::Uuid::new_v4()));
        #[allow(unused_mut)]
        let mut builder = std::fs::DirBuilder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder.create(&directory)?;
        Ok(Self {
            directory,
            files: Vec::new(),
        })
    }
    fn write(&mut self, name: &str, content: &[u8]) -> Result<PathBuf> {
        use std::io::Write;
        let path = self.directory.join(name);
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&path)?;
        self.files.push(path.clone());
        file.write_all(content)?;
        Ok(path)
    }
}
impl Drop for PrivateFiles {
    fn drop(&mut self) {
        for file in &self.files {
            let _ = std::fs::remove_file(file);
        }
        let _ = std::fs::remove_dir(&self.directory);
    }
}

struct Runner {
    stdin: Box<dyn tokio::io::AsyncWrite + Send + Unpin>,
    frames: mpsc::Receiver<Result<Value>>,
    controls: mpsc::Receiver<NativeControl>,
    controls_open: bool,
    events: mpsc::Sender<NativeEvent>,
    pending: HashMap<String, PendingInteraction>,
    next_id: u64,
    prompt_id: Option<String>,
    result: NativeResult,
    protocol: Protocol,
    read_only: bool,
    turn_id: Option<String>,
}

impl Runner {
    async fn run(&mut self, req: &NativeRequest, model: &str) -> Result<String> {
        let protocol_name = match self.protocol {
            Protocol::Codex => "codex-app-server",
            Protocol::KimiWire => "kimi-wire",
        };
        emit(
            &self.events,
            NativeEvent::Started {
                app_id: req.app_id.clone(),
                model: model.to_owned(),
                protocol: protocol_name.into(),
            },
        )
        .await?;
        match self.protocol {
            Protocol::Codex => self.start_codex(req, model).await?,
            Protocol::KimiWire => self.start_kimi(req, model).await?,
        }
        Ok("completed".into())
    }

    async fn start_codex(&mut self, req: &NativeRequest, model: &str) -> Result<()> {
        let init = self
            .send(
                "initialize",
                json!({"clientInfo":{"name":"wonderland","version":env!("CARGO_PKG_VERSION")},"capabilities":{}}),
            )
            .await?;
        self.wait_for_response(&init).await?;
        self.write_json(json!({"jsonrpc":"2.0","method":"initialized","params":{}}))
            .await?;
        let id = self
            .send("config/read", json!({"includeLayers":false,"cwd":req.cwd}))
            .await?;
        let config = self.wait_for_response(&id).await?;
        validate_codex_config(
            config
                .get("config")
                .context("Codex did not return effective configuration")?,
        )?;
        if req.read_only {
            validate_codex_chat_config(&config["config"])?;
        }
        let sandbox = if req.read_only {
            "read-only"
        } else {
            "workspace-write"
        };
        let id = self.send("thread/start", json!({"model":model,"modelProvider":"openai","cwd":req.cwd,"approvalPolicy":"on-request","approvalsReviewer":"user","sandbox":sandbox,"ephemeral":true,
            "config":{"features.multi_agent":false,"features.multi_agent_v2":false,"features.memories":false}})).await?;
        let thread = self.wait_for_response(&id).await?;
        anyhow::ensure!(
            thread.get("model").and_then(Value::as_str) == Some(model),
            "Codex selected a different model; refusing automatic substitution"
        );
        anyhow::ensure!(
            thread.get("modelProvider").and_then(Value::as_str) == Some("openai"),
            "Codex selected an unexpected model provider"
        );
        let thread_id =
            text_at(&thread, &["thread", "id"]).context("Codex did not return a thread ID")?;
        self.result.session_id = Some(thread_id.clone());
        emit(
            &self.events,
            NativeEvent::SessionStarted {
                session_id: thread_id.clone(),
            },
        )
        .await?;
        let mut params =
            json!({"threadId":thread_id,"input":[{"type":"text","text":req.prompt}],"model":model});
        if let Some(effort) = &req.reasoning_effort {
            params["effort"] = json!(effort);
        }
        let id = self.send("turn/start", params).await?;
        self.prompt_id = Some(id);
        self.wait_for_turn().await
    }

    async fn start_kimi(&mut self, req: &NativeRequest, model: &str) -> Result<()> {
        let id = self.send("initialize", json!({"protocol_version":"1.10","client":{"name":"wonderland","version":env!("CARGO_PKG_VERSION")},"capabilities":{"supports_question":true,"supports_plan_mode":false}})).await?;
        let initialized = self.wait_for_response(&id).await?;
        anyhow::ensure!(
            initialized.pointer("/server/name").and_then(Value::as_str) == Some("Kimi Code CLI"),
            "Kimi Wire server identity mismatch"
        );
        anyhow::ensure!(
            initialized
                .get("protocol_version")
                .and_then(Value::as_str)
                .is_some_and(|v| v.starts_with("1.")),
            "unsupported Kimi Wire protocol version"
        );
        self.result.model = model.into();
        if let Some(session_id) = &self.result.session_id {
            emit(
                &self.events,
                NativeEvent::SessionStarted {
                    session_id: session_id.clone(),
                },
            )
            .await?;
        }
        let id = self
            .send("prompt", json!({"user_input":req.prompt}))
            .await?;
        self.prompt_id = Some(id);
        self.wait_for_turn().await
    }

    async fn wait_for_turn(&mut self) -> Result<()> {
        loop {
            let Some(item) = self.next_item().await? else {
                bail!("official harness ended before a turn result")
            };
            if let Incoming::Control(control) = item {
                self.handle_control(control).await?;
                continue;
            }
            let value = match item {
                Incoming::Frame(value) => value,
                Incoming::Control(_) => unreachable!(),
            };
            if self.handle_value(value).await? {
                return Ok(());
            }
        }
    }

    async fn wait_for_response(&mut self, id: &str) -> Result<Value> {
        loop {
            let Some(item) = self.next_item().await? else {
                bail!("official harness ended while waiting for response {id}")
            };
            if let Incoming::Control(control) = item {
                self.handle_control(control).await?;
                continue;
            }
            let value = match item {
                Incoming::Frame(value) => value,
                Incoming::Control(_) => unreachable!(),
            };
            if value.get("id").and_then(Value::as_str) == Some(id) && value.get("method").is_none()
            {
                check_rpc_error(&value)?;
                return value
                    .get("result")
                    .cloned()
                    .context("harness response has no result");
            }
            self.handle_value(value).await?;
        }
    }

    async fn next_item(&mut self) -> Result<Option<Incoming>> {
        loop {
            tokio::select! {
                frame = self.frames.recv() => return frame.transpose().map(|value| value.map(Incoming::Frame)),
                control = self.controls.recv(), if self.controls_open => {
                    match control {
                        Some(value) => return Ok(Some(Incoming::Control(value))),
                        None => {
                            self.controls_open = false;
                            let ids: Vec<_> = self.pending.keys().cloned().collect();
                            for id in ids { self.deny_interaction(&id).await?; }
                        }
                    }
                }
            }
        }
    }

    async fn send(&mut self, method: &str, params: Value) -> Result<String> {
        self.next_id += 1;
        let id = self.next_id.to_string();
        self.write_json(json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}))
            .await?;
        Ok(id)
    }

    async fn write_json(&mut self, value: Value) -> Result<()> {
        let frame = serde_json::to_vec(&value)?;
        anyhow::ensure!(
            frame.len() <= MAX_FRAME_BYTES,
            "outgoing harness frame too large"
        );
        self.stdin.write_all(&frame).await?;
        self.stdin.write_all(b"\n").await?;
        self.stdin.flush().await?;
        Ok(())
    }

    async fn interrupt(&mut self) -> Result<()> {
        let method = match self.protocol {
            Protocol::Codex => "turn/interrupt",
            Protocol::KimiWire => "cancel",
        };
        let params = if matches!(self.protocol, Protocol::Codex) {
            json!({"threadId":self.result.session_id,"turnId":self.turn_id})
        } else {
            json!({})
        };
        let _ = self.send(method, params).await?;
        Ok(())
    }

    async fn handle_control(&mut self, control: NativeControl) -> Result<()> {
        match control {
            NativeControl::Permission {
                request_id,
                approve,
            } => {
                let Some(pending) = self.pending.get(&request_id) else {
                    return Ok(());
                };
                anyhow::ensure!(
                    !pending.question,
                    "permission response does not match a permission request"
                );
                let approve = approve && !self.read_only;
                let result = permission_result(self.protocol, &pending.logical_id, approve);
                let pending = self
                    .pending
                    .remove(&request_id)
                    .context("permission disappeared")?;
                self.write_json(json!({"jsonrpc":"2.0","id":pending.wire_id,"result":result}))
                    .await?;
                emit(
                    &self.events,
                    NativeEvent::PermissionResolved {
                        request_id,
                        approved: approve,
                    },
                )
                .await?;
            }
            NativeControl::Answer {
                request_id,
                answers,
            } => {
                let Some(pending) = self.pending.get(&request_id) else {
                    return Ok(());
                };
                anyhow::ensure!(
                    pending.question && answers.is_object(),
                    "invalid question answer"
                );
                anyhow::ensure!(
                    serde_json::to_vec(&answers)?.len() <= 64 * 1024,
                    "question answer is too large"
                );
                let result = if matches!(self.protocol, Protocol::KimiWire) {
                    anyhow::ensure!(
                        answers
                            .as_object()
                            .is_some_and(|m| m.values().all(Value::is_string)),
                        "Kimi question answers must be strings"
                    );
                    json!({"request_id":pending.logical_id,"answers":answers})
                } else {
                    json!({"answers":answers})
                };
                let pending = self
                    .pending
                    .remove(&request_id)
                    .context("question disappeared")?;
                self.write_json(json!({"jsonrpc":"2.0","id":pending.wire_id,"result":result}))
                    .await?;
            }
        }
        Ok(())
    }

    async fn deny_interaction(&mut self, id: &str) -> Result<()> {
        if self.pending.get(id).is_some_and(|p| p.question) {
            self.handle_control(NativeControl::Answer {
                request_id: id.into(),
                answers: json!({}),
            })
            .await?;
        } else {
            self.handle_control(NativeControl::Permission {
                request_id: id.into(),
                approve: false,
            })
            .await?;
        }
        Ok(())
    }

    async fn handle_value(&mut self, value: Value) -> Result<bool> {
        if let Some(method) = value.get("method").and_then(Value::as_str) {
            let params = value.get("params").cloned().unwrap_or(Value::Null);
            if let Some(wire_id) = value.get("id") {
                let wire_type = params.get("type").and_then(Value::as_str).unwrap_or("");
                let permission = matches!(
                    method,
                    "item/commandExecution/requestApproval" | "item/fileChange/requestApproval"
                ) || (method == "request" && wire_type == "ApprovalRequest");
                let question = method == "item/tool/requestUserInput"
                    || (method == "request" && wire_type == "QuestionRequest");
                if !permission && !question {
                    self.write_json(json!({"jsonrpc":"2.0","id":wire_id,"error":{"code":-32601,"message":"This host does not support that request"}})).await?;
                    return Ok(false);
                }
                anyhow::ensure!(
                    self.pending.len() < 64,
                    "too many pending harness interactions"
                );
                let key = wire_id
                    .as_str()
                    .map(str::to_owned)
                    .unwrap_or_else(|| wire_id.to_string());
                anyhow::ensure!(
                    !self.pending.contains_key(&key),
                    "duplicate harness request ID"
                );
                let payload = if matches!(self.protocol, Protocol::KimiWire) {
                    params
                        .get("payload")
                        .cloned()
                        .context("missing Kimi request payload")?
                } else {
                    params
                };
                let logical_id = payload
                    .get("id")
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned)
                    .unwrap_or_else(|| key.clone());
                self.pending.insert(
                    key.clone(),
                    PendingInteraction {
                        wire_id: wire_id.clone(),
                        logical_id,
                        question,
                    },
                );
                if question {
                    emit(
                        &self.events,
                        NativeEvent::QuestionRequested {
                            request_id: key.clone(),
                            data: payload,
                        },
                    )
                    .await?;
                } else {
                    let description = payload
                        .get("description")
                        .or_else(|| payload.get("reason"))
                        .and_then(Value::as_str)
                        .unwrap_or("Official tool requests permission")
                        .to_owned();
                    emit(
                        &self.events,
                        NativeEvent::PermissionRequested {
                            request_id: key.clone(),
                            description,
                            data: payload,
                        },
                    )
                    .await?;
                }
                if self.controls.is_closed() || (!question && self.read_only) {
                    self.deny_interaction(&key).await?;
                }
                return Ok(false);
            }
            match self.protocol {
                Protocol::Codex => {
                    if let Some(thread_id) = params.get("threadId").and_then(Value::as_str) {
                        if self
                            .result
                            .session_id
                            .as_deref()
                            .is_some_and(|id| id != thread_id)
                        {
                            return Ok(false);
                        }
                    }
                    match method {
                        "turn/started" => {
                            self.turn_id = text_at(&params, &["turn", "id"]);
                        }
                        "turn/completed" => {
                            anyhow::ensure!(
                                self.prompt_id.is_some(),
                                "Codex completed a turn before prompt submission"
                            );
                            anyhow::ensure!(params.get("threadId").and_then(Value::as_str).is_some_and(|id| Some(id) == self.result.session_id.as_deref()), "Codex completion omitted the active thread identity");
                            let expected = self
                                .turn_id
                                .as_deref()
                                .context("Codex completed a turn without a matching start")?;
                            anyhow::ensure!(
                                params.pointer("/turn/id").and_then(Value::as_str)
                                    == Some(expected),
                                "Codex completed an unexpected turn"
                            );
                            anyhow::ensure!(
                                params.pointer("/turn/status").and_then(Value::as_str)
                                    == Some("completed"),
                                "Codex turn did not complete successfully"
                            );
                            return Ok(true);
                        }
                        "item/agentMessage/delta" => {
                            self.append_text(
                                params.get("delta").and_then(Value::as_str).unwrap_or(""),
                                false,
                            )
                            .await?
                        }
                        "item/reasoning/textDelta" | "item/reasoning/summaryTextDelta" => {
                            self.append_text(
                                params.get("delta").and_then(Value::as_str).unwrap_or(""),
                                true,
                            )
                            .await?
                        }
                        "thread/tokenUsage/updated" => {
                            let usage = params.get("tokenUsage").cloned().unwrap_or(Value::Null);
                            self.result.usage = Some(usage.clone());
                            emit(&self.events, NativeEvent::Usage { data: usage }).await?;
                        }
                        "item/started" | "item/completed" | "item/commandExecution/outputDelta" => {
                            emit(&self.events, NativeEvent::ToolActivity { data: params }).await?;
                        }
                        "error" => {
                            if params.get("willRetry").and_then(Value::as_bool) != Some(true) {
                                bail!("Codex reported an execution error; see the official tool's diagnostics");
                            }
                        }
                        _ => {}
                    }
                }
                Protocol::KimiWire => {
                    if method != "event" {
                        return Ok(false);
                    }
                    let kind = params.get("type").and_then(Value::as_str).unwrap_or("");
                    let payload = params.get("payload").cloned().unwrap_or(Value::Null);
                    match kind {
                        "ContentPart" => match payload.get("type").and_then(Value::as_str) {
                            Some("text") => {
                                self.append_text(
                                    payload.get("text").and_then(Value::as_str).unwrap_or(""),
                                    false,
                                )
                                .await?
                            }
                            Some("think") => {
                                self.append_text(
                                    payload.get("think").and_then(Value::as_str).unwrap_or(""),
                                    true,
                                )
                                .await?
                            }
                            _ => {}
                        },
                        "TextPart" => {
                            self.append_text(
                                payload.get("text").and_then(Value::as_str).unwrap_or(""),
                                false,
                            )
                            .await?
                        }
                        "ThinkPart" => {
                            self.append_text(
                                payload.get("think").and_then(Value::as_str).unwrap_or(""),
                                true,
                            )
                            .await?
                        }
                        "StatusUpdate" => {
                            if let Some(usage) = payload.get("token_usage").filter(|v| !v.is_null())
                            {
                                self.result.usage = Some(usage.clone());
                                emit(
                                    &self.events,
                                    NativeEvent::Usage {
                                        data: usage.clone(),
                                    },
                                )
                                .await?;
                            }
                        }
                        "ToolCall" | "ToolCallPart" | "ToolResult" | "PlanDisplay" => {
                            emit(&self.events, NativeEvent::ToolActivity { data: params }).await?
                        }
                        "SubagentEvent" => {
                            bail!("Kimi unexpectedly invoked a nested agent in a single-model run")
                        }
                        _ => {}
                    }
                }
            }
            return Ok(false);
        }
        check_rpc_error(&value)?;
        if value.get("id").and_then(Value::as_str) == self.prompt_id.as_deref() {
            let result = value.get("result").context("missing prompt result")?;
            match self.protocol {
                Protocol::KimiWire => {
                    anyhow::ensure!(
                        result.get("status").and_then(Value::as_str) == Some("finished"),
                        "Kimi prompt stopped without successful completion"
                    );
                    return Ok(true);
                }
                Protocol::Codex => {
                    self.turn_id = text_at(result, &["turn", "id"]);
                }
            }
        }
        Ok(false)
    }

    async fn append_text(&mut self, text: &str, reasoning: bool) -> Result<()> {
        if text.is_empty() {
            return Ok(());
        }
        if !reasoning {
            anyhow::ensure!(
                self.result.output.len() + text.len() <= MAX_OUTPUT_BYTES,
                "native output exceeds size limit"
            );
            self.result.output.push_str(text);
        }
        emit(
            &self.events,
            if reasoning {
                NativeEvent::ReasoningDelta { text: text.into() }
            } else {
                NativeEvent::TextDelta { text: text.into() }
            },
        )
        .await
    }
}

enum Incoming {
    Frame(Value),
    Control(NativeControl),
}

struct PendingInteraction {
    wire_id: Value,
    logical_id: String,
    question: bool,
}

fn permission_result(protocol: Protocol, id: &str, approved: bool) -> Value {
    match protocol {
        Protocol::Codex => json!({"decision":if approved {"accept"} else {"decline"}}),
        Protocol::KimiWire => {
            json!({"request_id":id,"response":if approved {"approve"} else {"reject"}})
        }
    }
}

fn validate_codex_config(config: &Value) -> Result<()> {
    anyhow::ensure!(
        config
            .get("model_provider")
            .and_then(Value::as_str)
            .is_none_or(|p| p == "openai"),
        "Codex effective provider is not OpenAI"
    );
    for pointer in ["/model_providers/openai/base_url", "/chatgpt_base_url"] {
        if let Some(endpoint) = config.pointer(pointer).and_then(Value::as_str) {
            let endpoint =
                url::Url::parse(endpoint).map_err(|_| anyhow::anyhow!("invalid Codex endpoint"))?;
            anyhow::ensure!(
                endpoint.scheme() == "https"
                    && endpoint.username().is_empty()
                    && endpoint.password().is_none()
                    && endpoint.port_or_known_default() == Some(443)
                    && matches!(
                        endpoint.host_str(),
                        Some("api.openai.com" | "chatgpt.com" | "chat.openai.com")
                    ),
                "Codex configuration redirects the model to a nonofficial endpoint"
            );
        }
    }
    if let Some(provider) = config.pointer("/model_providers/openai") {
        for key in ["gateway_oauth", "aws", "model_catalog_url", "auth"] {
            anyhow::ensure!(
                provider.get(key).is_none_or(Value::is_null),
                "custom Codex provider authentication requires manual review"
            );
        }
    }
    for feature in [
        "multi_agent",
        "multi_agent_v2",
        "memories",
        "guardian_approval",
        "guardianv2",
    ] {
        anyhow::ensure!(
            config
                .get("features")
                .and_then(|v| v.get(feature))
                .is_none_or(|v| v == &json!(false)),
            "Codex nested-model feature could not be disabled"
        );
    }
    Ok(())
}

fn validate_codex_chat_config(config: &Value) -> Result<()> {
    if let Some(servers) = config.get("mcp_servers").and_then(Value::as_object) {
        anyhow::ensure!(servers.values().all(|server| server.get("enabled").and_then(Value::as_bool) == Some(false)), "Chat cannot enforce read-only access for configured MCP servers; use a profile without MCP");
    }
    for key in ["hooks", "plugins"] {
        anyhow::ensure!(
            config.get(key).is_none_or(|v| v.is_null()
                || v.as_array().is_some_and(Vec::is_empty)
                || v.as_object().is_some_and(serde_json::Map::is_empty)),
            "Chat cannot enforce read-only behavior with configured Codex hooks or plugins"
        );
    }
    anyhow::ensure!(
        config.pointer("/features/apps").and_then(Value::as_bool) != Some(true),
        "Chat cannot enforce read-only access to Codex apps"
    );
    Ok(())
}

fn check_rpc_error(value: &Value) -> Result<()> {
    if let Some(error) = value.get("error") {
        // Provider error messages can contain URLs, headers, or user content.
        bail!(
            "official harness rejected a request (code {}); consult its local diagnostics",
            error.get("code").and_then(Value::as_i64).unwrap_or(-1)
        );
    }
    Ok(())
}

fn text_at(value: &Value, path: &[&str]) -> Option<String> {
    let mut current = value;
    for key in path {
        current = current.get(*key)?;
    }
    current.as_str().map(ToOwned::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(app_id: &str, model: &str) -> NativeRequest {
        NativeRequest {
            app_id: app_id.into(),
            model: model.into(),
            cwd: std::env::current_dir().unwrap(),
            prompt: "task".into(),
            read_only: true,
            max_duration_secs: 30,
            reasoning_effort: None,
            config_path: None,
        }
    }

    #[test]
    fn unsupported_apps_fail_before_process_creation() {
        let result = validate_request(&request("mimo", "mimo-x"));
        assert!(result.is_err());
    }

    #[test]
    fn provider_model_pair_is_hard_filtered() {
        assert!(validate_request(&request("codex", "kimi-k2")).is_err());
        assert!(validate_request(&request("kimi-cli", "gpt-5")).is_err());
    }

    #[tokio::test]
    async fn framing_rejects_incomplete_and_oversized_input() {
        let mut reader = BufReader::new(&b"{\"unfinished\":"[..]);
        assert!(read_frame(&mut reader, &mut Vec::new()).await.is_err());
        let bytes = vec![b'x'; MAX_FRAME_BYTES + 1];
        let mut reader = BufReader::new(&bytes[..]);
        assert!(read_frame(&mut reader, &mut Vec::new()).await.is_err());
        let mut reader = BufReader::new(&b"{\"id\":1}\n{\"id\":2}\n"[..]);
        let mut frame = Vec::new();
        assert!(read_frame(&mut reader, &mut frame).await.unwrap());
        assert_eq!(serde_json::from_slice::<Value>(&frame).unwrap()["id"], 1);
        frame.clear();
        assert!(read_frame(&mut reader, &mut frame).await.unwrap());
        assert_eq!(serde_json::from_slice::<Value>(&frame).unwrap()["id"], 2);
    }

    fn kimi_config() -> Value {
        json!({"models":{"kimi-for-coding":{"model":"kimi-for-coding","provider":"official","max_context_size":262144},
            "other":{"model":"gpt-5","provider":"unrelated","max_context_size":128000}},
            "providers":{"official":{"type":"kimi","base_url":"https://api.kimi.com/coding/v1","api_key":"synthetic"},
            "unrelated":{"type":"openai_responses","base_url":"https://example.invalid/v1","api_key":"not-retained"}}})
    }

    #[test]
    fn kimi_configuration_removes_unselected_providers_and_rejects_substitution() {
        let (config, alias, actual) =
            restricted_kimi_config(kimi_config(), "kimi-for-coding").unwrap();
        assert_eq!(
            (alias.as_str(), actual.as_str()),
            ("kimi-for-coding", "kimi-for-coding")
        );
        assert_eq!(config["models"].as_object().unwrap().len(), 1);
        assert_eq!(config["providers"].as_object().unwrap().len(), 1);
        let mut substituted = kimi_config();
        substituted["models"]["kimi-for-coding"]["model"] = json!("gpt-5");
        assert!(restricted_kimi_config(substituted, "kimi-for-coding").is_err());
        let mut same_vendor = kimi_config();
        same_vendor["models"]["kimi-for-coding"]["model"] = json!("kimi-other");
        assert!(restricted_kimi_config(same_vendor, "kimi-for-coding").is_err());
        let mut redirected = kimi_config();
        redirected["providers"]["official"]["base_url"] = json!("https://example.invalid");
        assert!(restricted_kimi_config(redirected, "kimi-for-coding").is_err());
    }

    #[test]
    fn codex_rejects_redirected_provider_and_internal_model_routing() {
        assert!(validate_codex_config(&json!({"model_provider":"openai"})).is_ok());
        assert!(validate_codex_config(&json!({"model_provider":"openai","model_providers":{"openai":{"base_url":"https://example.invalid/v1"}}})).is_err());
        assert!(validate_codex_config(
            &json!({"model_provider":"openai","features":{"multi_agent":true}})
        )
        .is_err());
    }

    #[test]
    fn codex_chat_rejects_remote_mutation_surfaces() {
        assert!(validate_codex_chat_config(
            &json!({"mcp_servers":{"disabled":{"enabled":false}}, "hooks":[], "plugins":{}})
        )
        .is_ok());
        for config in [
            json!({"mcp_servers":{"remote":{"url":"https://example.invalid/mcp"}}}),
            json!({"hooks":[{"command":"external-tool"}]}),
            json!({"plugins":{"external":{"enabled":true}}}),
            json!({"features":{"apps":true}}),
        ] {
            assert!(validate_codex_chat_config(&config).is_err());
        }
    }

    #[test]
    fn kimi_plugins_cannot_bypass_native_tool_restrictions() {
        let files = PrivateFiles::create().unwrap();
        let plugins = files.directory.join("plugins");
        assert!(reject_kimi_plugins(&plugins).is_ok());
        std::fs::create_dir(&plugins).unwrap();
        assert!(reject_kimi_plugins(&plugins).is_ok());
        let plugin = plugins.join("custom-plugin");
        std::fs::create_dir(&plugin).unwrap();
        let manifest = plugin.join("plugin.json");
        std::fs::write(&manifest, br#"{"name":"custom-plugin","tools":[]}"#).unwrap();
        assert!(reject_kimi_plugins(&plugins).is_err());
        std::fs::remove_file(&manifest).unwrap();
        std::fs::remove_dir(&plugin).unwrap();
        std::fs::remove_dir(&plugins).unwrap();
    }

    #[tokio::test]
    async fn cancellation_before_preparation_does_not_require_an_installed_cli() {
        let (events, _receiver) = mpsc::channel(2);
        let (_cancel, cancelled) = watch::channel(true);
        let mut request = request("kimi-cli", "kimi-for-coding");
        request.config_path = Some(request.cwd.join("nonexistent-test-config.toml"));
        let result = execute(request, events, cancelled).await.unwrap();
        assert_eq!(result.status, "cancelled");
        assert!(result.output.is_empty());
    }

    fn runner(
        protocol: Protocol,
    ) -> (
        Runner,
        tokio::io::DuplexStream,
        mpsc::Sender<Result<Value>>,
        mpsc::Sender<NativeControl>,
        mpsc::Receiver<NativeEvent>,
    ) {
        let (stdin, output) = tokio::io::duplex(65536);
        let (frames_tx, frames) = mpsc::channel(32);
        let (controls_tx, controls) = mpsc::channel(16);
        let (events, events_rx) = mpsc::channel(64);
        (
            Runner {
                stdin: Box::new(stdin),
                frames,
                controls,
                controls_open: true,
                events,
                pending: HashMap::new(),
                next_id: 0,
                prompt_id: None,
                result: empty_result(&request("codex", "gpt-5"), "completed"),
                protocol,
                read_only: false,
                turn_id: None,
            },
            output,
            frames_tx,
            controls_tx,
            events_rx,
        )
    }

    #[tokio::test]
    async fn codex_stream_does_not_treat_item_completion_as_turn_completion() {
        let (mut runner, _output, _frames, _controls, mut events) = runner(Protocol::Codex);
        runner.result.session_id = Some("thread-one".into());
        runner.prompt_id = Some("3".into());
        runner.turn_id = Some("turn-one".into());
        assert!(!runner.handle_value(json!({"method":"item/completed","params":{"threadId":"thread-one","item":{"type":"commandExecution"}}})).await.unwrap());
        assert!(!runner.handle_value(json!({"method":"item/agentMessage/delta","params":{"threadId":"thread-one","delta":"hello"}})).await.unwrap());
        assert_eq!(runner.result.output, "hello");
        assert!(!runner.handle_value(json!({"method":"turn/completed","params":{"threadId":"other","turn":{"status":"completed"}}})).await.unwrap());
        assert!(runner.handle_value(json!({"method":"turn/completed","params":{"threadId":"thread-one","turn":{"id":"wrong","status":"completed"}}})).await.is_err());
        assert!(runner.handle_value(json!({"method":"turn/completed","params":{"turn":{"id":"turn-one","status":"completed"}}})).await.is_err());
        assert!(runner.handle_value(json!({"method":"turn/completed","params":{"threadId":"thread-one","turn":{"id":"turn-one","status":"completed"}}})).await.unwrap());
        assert!(matches!(
            events.recv().await,
            Some(NativeEvent::ToolActivity { .. })
        ));
        assert!(
            matches!(events.recv().await,Some(NativeEvent::TextDelta {text}) if text == "hello")
        );
    }

    #[tokio::test]
    async fn read_only_permission_is_denied_and_numeric_rpc_id_is_preserved() {
        let (mut runner, output, _frames, _controls, _events) = runner(Protocol::Codex);
        runner.read_only = true;
        runner.handle_value(json!({"id":42,"method":"item/fileChange/requestApproval","params":{"reason":"edit file"}})).await.unwrap();
        let mut output = BufReader::new(output);
        let mut bytes = Vec::new();
        assert!(read_frame(&mut output, &mut bytes).await.unwrap());
        let reply: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(
            reply,
            json!({"jsonrpc":"2.0","id":42,"result":{"decision":"decline"}})
        );
        assert!(runner.pending.is_empty());
        runner
            .handle_control(NativeControl::Permission {
                request_id: "42".into(),
                approve: true,
            })
            .await
            .unwrap();
        assert!(runner.pending.is_empty());
    }

    #[tokio::test]
    async fn closed_control_channel_does_not_look_like_protocol_eof() {
        let (mut runner, _output, frames, controls, _events) = runner(Protocol::KimiWire);
        drop(controls);
        frames
            .send(Ok(
                json!({"method":"event","params":{"type":"TextPart","payload":{"text":"hello"}}}),
            ))
            .await
            .unwrap();
        assert!(matches!(
            runner.next_item().await.unwrap(),
            Some(Incoming::Frame(_))
        ));
    }

    #[tokio::test]
    async fn kimi_success_requires_the_matching_prompt_response() {
        let (mut runner, _output, _frames, _controls, _events) = runner(Protocol::KimiWire);
        runner.prompt_id = Some("2".into());
        assert!(!runner
            .handle_value(json!({"method":"event","params":{"type":"TurnEnd","payload":{}}}))
            .await
            .unwrap());
        assert!(!runner
            .handle_value(json!({"id":"unrelated","result":{"status":"finished"}}))
            .await
            .unwrap());
        assert!(runner
            .handle_value(json!({"id":"2","result":{"status":"finished"}}))
            .await
            .unwrap());
        assert!(runner
            .handle_value(json!({"id":"2","result":{"status":"max_steps_reached"}}))
            .await
            .is_err());
    }

    #[tokio::test]
    async fn kimi_wire_content_parts_stream_text_and_keep_reasoning_separate() {
        let (mut runner, _output, _frames, _controls, mut events) = runner(Protocol::KimiWire);
        for payload in [
            json!({"type":"think","think":"checking the test"}),
            json!({"type":"text","text":"All four "}),
            json!({"type":"text","text":"tests passed."}),
            json!({"type":"image_url","image_url":{"url":"hidden"}}),
        ] {
            assert!(!runner
                .handle_value(
                    json!({"method":"event","params":{"type":"ContentPart","payload":payload}})
                )
                .await
                .unwrap());
        }
        assert_eq!(runner.result.output, "All four tests passed.");
        assert!(
            matches!(events.recv().await, Some(NativeEvent::ReasoningDelta { text }) if text == "checking the test")
        );
        assert!(
            matches!(events.recv().await, Some(NativeEvent::TextDelta { text }) if text == "All four ")
        );
        assert!(
            matches!(events.recv().await, Some(NativeEvent::TextDelta { text }) if text == "tests passed.")
        );
        assert!(events.try_recv().is_err());
    }

    #[tokio::test]
    async fn codex_model_substitution_stops_before_prompt_submission() {
        let (mut runner, output, frames, _controls, _events) = runner(Protocol::Codex);
        for value in [
            json!({"id":"1","result":{}}),
            json!({"id":"2","result":{"config":{"model_provider":"openai"}}}),
            json!({"id":"3","result":{"model":"gpt-other","modelProvider":"openai","thread":{"id":"new"}}}),
        ] {
            frames.send(Ok(value)).await.unwrap();
        }
        assert!(runner
            .start_codex(&request("codex", "gpt-5"), "gpt-5")
            .await
            .is_err());
        drop(runner.stdin);
        let mut output = BufReader::new(output);
        let mut text = String::new();
        tokio::io::AsyncReadExt::read_to_string(&mut output, &mut text)
            .await
            .unwrap();
        assert!(!text.contains("turn/start"));
    }
}
