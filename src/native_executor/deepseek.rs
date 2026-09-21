//! Exact-release ACP adapter for the unmodified official DeepSeek Harness CLI.
//! ACP v0.1.6 emits committed message blocks, not provider token deltas.
use super::{
    cancellation, emit, empty_result, executable_digest, read_frame, NativeControl, NativeEvent,
    NativeRequest, NativeResult, MAX_FRAME_BYTES, MAX_OUTPUT_BYTES,
};
use anyhow::{bail, ensure, Context, Result};
use serde_json::{json, Value};
use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};
use tokio::{
    io::{AsyncWrite, AsyncWriteExt, BufReader},
    sync::{mpsc, watch},
};

const VERSION: &str = "0.1.6-alpha.2";
const PROVIDER: &str = "deepseek-official";
const CLI_HASH: &str = "69c49c871735dc7ee81ec51f266bbec129f075fd5066e046374f4b13ab02a705";
const BASE_HASH: &str = "a395d4e6b1b4de3694d354ab21901c174f59a30557d906f6324a474f486b2cc8";
const ACP_HASH: &str = "3c559e8860348f1a878637a740bb11f80863f8e97b895060badda962a8a127fd";
const MAX_PENDING: usize = 64;
const MAX_INTERACTIONS: usize = 4096;

pub(super) fn validate_request(req: &NativeRequest) -> Result<()> {
    ensure!(
        req.app_id == "deepseek",
        "DeepSeek Harness profile required"
    );
    ensure!(
        req.model.starts_with("deepseek-")
            && req.model.len() <= 160
            && req
                .model
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'.'),
        "DeepSeek requires an explicit deepseek-* model ID"
    );
    ensure!(
        req.config_path.is_none(),
        "managed DeepSeek uses an isolated official ACP profile; custom patches are unsupported"
    );
    if let Some(effort) = &req.reasoning_effort {
        ensure!(
            ["off", "low", "high", "max"].contains(&effort.as_str()),
            "unsupported DeepSeek reasoning effort"
        );
    }
    Ok(())
}

struct Prepared {
    node: PathBuf,
    cli: PathBuf,
    sha256: String,
}

/// Node's entry-point resolver rejects Windows extended-length paths even
/// when CreateProcess and Rust filesystem APIs accept the same path.
fn node_path(path: &Path) -> Result<PathBuf> {
    #[cfg(windows)]
    {
        use std::path::{Component, Prefix};
        let mut parts = path.components();
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
                    bail!("unsupported Windows device path for DeepSeek")
                }
                _ => None,
            };
            if let Some(mut normal) = normal {
                for part in parts {
                    if !matches!(part, Component::RootDir) {
                        normal.push(part.as_os_str());
                    }
                }
                // Changing path syntax must not change the filesystem object.
                ensure!(
                    std::fs::canonicalize(&normal)? == std::fs::canonicalize(path)?,
                    "DeepSeek path normalization changed its target"
                );
                return Ok(normal);
            }
        }
    }
    Ok(path.to_path_buf())
}

fn read_json(path: &Path) -> Result<Value> {
    use std::io::Read;
    let mut data = Vec::new();
    std::fs::File::open(path)?
        .take(256 * 1024 + 1)
        .read_to_end(&mut data)?;
    ensure!(
        data.len() <= 256 * 1024,
        "DeepSeek package metadata exceeds limit"
    );
    serde_json::from_slice(&data).context("invalid DeepSeek package metadata")
}

fn verify_installation(cli: &Path) -> Result<Prepared> {
    ensure!(
        cli.is_absolute(),
        "WONDERLAND_DSH_CLI must be an absolute official npm lib/bin.js path"
    );
    let cli = std::fs::canonicalize(cli).context("DeepSeek CLI entry is missing")?;
    let root = cli
        .parent()
        .and_then(Path::parent)
        .context("invalid DeepSeek npm layout")?;
    ensure!(
        cli == root.join("lib/bin.js"),
        "DeepSeek must use the official lib/bin.js entry"
    );
    let package = read_json(&root.join("package.json"))?;
    ensure!(
        package["name"] == "@deepseek-ai/dsh"
            && package["version"] == VERSION
            && package.pointer("/bin/dsh").and_then(Value::as_str) == Some("lib/bin.js")
            && package.pointer("/repository/url").and_then(Value::as_str)
                == Some("git+https://github.com/deepseek-ai/deepseek-harness.git"),
        "unsupported DeepSeek npm package; managed ACP requires @deepseek-ai/dsh@{VERSION}"
    );
    let scope = root.parent().context("missing DeepSeek npm scope")?;
    ensure!(
        scope.file_name().is_some_and(|name| name == "@deepseek-ai"),
        "unsupported DeepSeek installation layout"
    );
    // npm uses caret dependencies: checking the top-level CLI version alone is
    // insufficient. The managed adapter accepts a flat, consistent release.
    let mut checked = HashSet::new();
    for entry in std::fs::read_dir(scope)? {
        let path = entry?.path();
        let name = path
            .file_name()
            .context("invalid package path")?
            .to_string_lossy();
        if name == "dsh" || name.starts_with("dsh-") {
            let metadata = read_json(&path.join("package.json"))?;
            ensure!(
                metadata["name"] == format!("@deepseek-ai/{name}")
                    && metadata["version"] == VERSION,
                "DeepSeek dependency release drift; reinstall a consistent {VERSION} package set"
            );
            ensure!(
                !path.join("node_modules/@deepseek-ai").exists(),
                "nested DeepSeek package overrides are unsupported"
            );
            checked.insert(name.into_owned());
            ensure!(
                checked.len() <= 1024,
                "DeepSeek package inventory exceeds limit"
            );
        }
    }
    for name in [
        "dsh-base",
        "dsh-acp-app",
        "dsh-acp",
        "dsh-agent",
        "dsh-llm-deepseek",
        "dsh-sandbox-local",
        "dsh-fs-sandbox",
        "dsh-permission-presets",
    ] {
        ensure!(
            checked.contains(name),
            "required DeepSeek profile dependency is missing"
        );
    }
    let sha256 = executable_digest(&cli)?;
    ensure!(sha256.eq_ignore_ascii_case(CLI_HASH)
        && executable_digest(&scope.join("dsh-base/cordis.patch.yml"))?.eq_ignore_ascii_case(BASE_HASH)
        && executable_digest(&scope.join("dsh-acp-app/cordis.patch.yml"))?.eq_ignore_ascii_case(ACP_HASH),
        "DeepSeek CLI/profile content differs from the verified official release; use the manual terminal");
    let node = crate::desktop_bridge::find_executable("node")
        .context("Node.js is required by the official DeepSeek CLI")?;
    Ok(Prepared {
        node,
        cli: node_path(&cli)?,
        sha256,
    })
}

fn prepare() -> Result<Prepared> {
    if let Some(path) = std::env::var_os("WONDERLAND_DSH_CLI") {
        return verify_installation(Path::new(&path));
    }
    let resolved = crate::desktop_bridge::find_executable("dsh")
        .context("install @deepseek-ai/dsh@0.1.6-alpha.2 or set WONDERLAND_DSH_CLI to its official lib/bin.js")?;
    let canonical = std::fs::canonicalize(&resolved)?;
    let mut candidates = Vec::new();
    for entry in [&resolved, &canonical] {
        if let Some(parent) = entry.parent() {
            candidates.push(parent.join("node_modules/@deepseek-ai/dsh/lib/bin.js"));
            candidates.push(parent.join("../lib/node_modules/@deepseek-ai/dsh/lib/bin.js"));
        }
        if entry.ends_with("lib/bin.js") {
            candidates.push(entry.to_path_buf());
        }
    }
    let candidate = candidates.into_iter().find(|path| path.is_file())
        .context("DeepSeek npm metadata is unavailable; standalone binaries and source checkouts require the manual terminal")?;
    verify_installation(&candidate)
}

/// Fresh UUID directory; no existing DSH_HOME, profiles, credentials files,
/// settings or hooks are read. Only the explicit environment API key is passed.
struct ManagedHome {
    root: PathBuf,
    canonical: PathBuf,
}
impl ManagedHome {
    fn create(req: &NativeRequest) -> Result<Self> {
        use std::io::Write;
        let root = std::env::temp_dir().join(format!("wonderland-dsh-{}", uuid::Uuid::new_v4()));
        #[allow(unused_mut)]
        let mut builder = std::fs::DirBuilder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder.create(&root)?;
        let home = Self {
            canonical: std::fs::canonicalize(&root)?,
            root,
        };
        builder.create(home.root.join("home"))?;
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        options
            .open(home.root.join("managed.patch.json"))?
            .write_all(&serde_json::to_vec(&managed_patch(req))?)?;
        Ok(home)
    }
}
impl Drop for ManagedHome {
    fn drop(&mut self) {
        // Never recursively remove a changed root or follow a replaced root
        // link. remove_dir_all itself does not follow links within the tree.
        if std::fs::symlink_metadata(&self.root)
            .is_ok_and(|m| m.is_dir() && !m.file_type().is_symlink())
            && std::fs::canonicalize(&self.root).is_ok_and(|p| p == self.canonical)
        {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }
}

fn managed_patch(req: &NativeRequest) -> Value {
    let mut rows: Vec<Value> = [
        "settings",
        "llm-pi-ai",
        "plugin-manager",
        "tool-plugin-manager",
        "hmr",
        "session-title-llm",
        "subagent",
        "subagent-spawn-in-process",
        "subagent-fork-in-process",
        "tool-subagent",
        "tool-subagent-fork",
        "tool-subagent-control",
        "tool-subagent-list-agents",
        "ptc-runtime",
        "workflow-ptc",
        "tool-workflow",
        "goal-round-driver",
        "tool-goal",
        "tool-ralph",
        "mcp-resources",
        "web",
        "web-search-deepseek",
        "web-fetch-http",
        "tool-web",
        "commands",
        "command-feedback",
        "command-goal",
        "command-compact",
    ]
    .iter()
    .map(|id| json!({"id":id,"disabled":true}))
    .collect();
    let mode = if req.read_only {
        "read-only"
    } else {
        "workspace-write"
    };
    let approval = if req.read_only { "never" } else { "ask" };
    rows.extend([
        json!({"id":"acp","config":{"provider":PROVIDER,"model":req.model}}),
        json!({"id":"agent-default-model","config":{"provider":PROVIDER,"model":req.model}}),
        json!({"id":"llm-deepseek","config":{"baseURL":"https://api.deepseek.com/anthropic","protocol":"messages","apiKeyEnv":"DEEPSEEK_API_KEY"}}),
        json!({"id":"sandbox-policy","config":{"mode":mode,"workspaceRoot":req.cwd}}),
        json!({"id":"approval","config":{"policy":approval}}),
        json!({"id":"permission","config":{"defaultPreset":mode,"presets":{(mode):{"sandbox":mode,"approval":approval}}}}),
        json!({"id":"fs-sandbox","config":{"cwd":req.cwd}}),
    ]);
    Value::Array(rows)
}

fn command(prepared: &Prepared, home: &ManagedHome, credentials: bool) -> tokio::process::Command {
    let mut command = tokio::process::Command::new(&prepared.node);
    command
        .arg(&prepared.cli)
        .current_dir(&home.root)
        .env_clear();
    // A strict allowlist also removes Node preload/proxy/TLS overrides, DSH
    // custom roots and provider endpoint overrides. Never log these values.
    for key in [
        "PATH",
        "SystemRoot",
        "WINDIR",
        "TEMP",
        "TMP",
        "COMSPEC",
        "PATHEXT",
        "USERPROFILE",
        "HOMEDRIVE",
        "HOMEPATH",
        "HOME",
        "LANG",
        "LC_ALL",
    ] {
        if let Some(value) = std::env::var_os(key) {
            command.env(key, value);
        }
    }
    if credentials {
        if let Some(value) = std::env::var_os("DEEPSEEK_API_KEY") {
            command.env("DEEPSEEK_API_KEY", value);
        }
    }
    command
        .env("DSH_HOME", home.root.join("home"))
        .env("DSH_TELEMETRY_DISABLED", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    #[cfg(windows)]
    command.creation_flags(0x0800_0000);
    #[cfg(unix)]
    command.process_group(0);
    command
}

async fn verify_banner(prepared: &Prepared, home: &ManagedHome) -> Result<()> {
    let mut child = command(prepared, home, false).arg("--version").spawn()?;
    let tree = match crate::process_tree::ProcessTree::attach(
        child.id().context("DeepSeek version process has no ID")?,
    ) {
        Ok(tree) => tree,
        Err(error) => {
            let _ = child.start_kill();
            let _ = tokio::time::timeout(Duration::from_secs(2), child.wait()).await;
            return Err(error);
        }
    };
    drop(child.stdin.take());
    let mut reader = BufReader::new(
        child
            .stdout
            .take()
            .context("DeepSeek version stream unavailable")?,
    );
    let mut frame = Vec::new();
    let result = tokio::time::timeout(Duration::from_secs(8), async {
        ensure!(
            read_frame(&mut reader, &mut frame).await?,
            "DeepSeek version banner missing"
        );
        ensure!(
            std::str::from_utf8(&frame)?.trim() == VERSION,
            "DeepSeek CLI version banner mismatch"
        );
        ensure!(
            child.wait().await?.success(),
            "DeepSeek version probe failed"
        );
        Ok::<_, anyhow::Error>(())
    })
    .await
    .context("DeepSeek version probe timed out")
    .and_then(|r| r);
    drop(tree);
    let _ = child.start_kill();
    let _ = tokio::time::timeout(Duration::from_secs(2), child.wait()).await;
    result
}

pub(super) async fn execute_with_control(
    mut req: NativeRequest,
    events: mpsc::Sender<NativeEvent>,
    mut cancel: watch::Receiver<bool>,
    controls: mpsc::Receiver<NativeControl>,
) -> Result<NativeResult> {
    validate_request(&req)?;
    req.cwd = node_path(&req.cwd)?;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(req.max_duration_secs);
    if *cancel.borrow() {
        return Ok(empty_result(&req, "cancelled"));
    }
    let preparation = tokio::task::spawn_blocking(prepare);
    let prepared = tokio::select! {
        result = preparation => result??,
        _ = cancellation(&mut cancel) => return Ok(empty_result(&req, "cancelled")),
        _ = tokio::time::sleep_until(deadline) => return Ok(empty_result(&req, "timed_out")),
        _ = events.closed() => bail!("DeepSeek event consumer disconnected during preparation"),
    };
    let home = ManagedHome::create(&req)?;
    // Dropping a timed-out/cancelled probe kills its entire process tree.
    tokio::select! {
        result = verify_banner(&prepared, &home) => result?,
        _ = cancellation(&mut cancel) => return Ok(empty_result(&req, "cancelled")),
        _ = tokio::time::sleep_until(deadline) => return Ok(empty_result(&req, "timed_out")),
        _ = events.closed() => bail!("DeepSeek event consumer disconnected during version probe"),
    }
    emit(&events, NativeEvent::Identity {
        executable: prepared.cli.to_string_lossy().into_owned(), version: VERSION.into(), sha256: prepared.sha256.clone(),
        verification: "Official npm layout, exact DSH dependency versions, CLI/profile digests and version banner verified; publisher signature and remote model identity are not attested".into(),
    }).await?;
    let mut child = command(&prepared, &home, true)
        .args(["--profile", "acp", "--patch"])
        .arg(home.root.join("managed.patch.json"))
        .spawn()
        .context("could not start official DeepSeek ACP profile")?;
    let tree = match crate::process_tree::ProcessTree::attach(
        child.id().context("DeepSeek process has no ID")?,
    ) {
        Ok(tree) => tree,
        Err(error) => {
            let _ = child.start_kill();
            let _ = tokio::time::timeout(Duration::from_secs(2), child.wait()).await;
            return Err(error);
        }
    };
    let stdin = child.stdin.take().context("DeepSeek stdin unavailable")?;
    let stdout = child.stdout.take().context("DeepSeek stdout unavailable")?;
    let (tx, rx) = mpsc::channel(64);
    let reader = tokio::spawn(async move {
        let mut reader = BufReader::new(stdout);
        loop {
            let mut frame = Vec::new();
            let result = match read_frame(&mut reader, &mut frame).await {
                Ok(true) => {
                    serde_json::from_slice(&frame).context("invalid DeepSeek ACP JSON frame")
                }
                Ok(false) => Err(anyhow::anyhow!(
                    "DeepSeek ACP stream closed before response"
                )),
                Err(error) => Err(error),
            };
            let failed = result.is_err();
            if tx.send(result).await.is_err() || failed {
                break;
            }
        }
    });
    let mut runner = Runner::new(Box::new(stdin), rx, controls, events.clone(), &req);
    let outcome = tokio::select! {
        result = runner.run(&req) => result,
        _ = cancellation(&mut cancel) => Ok("cancelled".into()),
        _ = tokio::time::sleep_until(deadline) => Ok("timed_out".into()),
        _ = events.closed() => Err(anyhow::anyhow!("DeepSeek event consumer disconnected")),
    };
    // Always close, including JSON-RPC errors, route drift and UI disconnect.
    // Cancel is only a notification; close and the process tree are the fences.
    let _ = tokio::time::timeout(Duration::from_millis(600), runner.shutdown()).await;
    drop(runner.stdin);
    let _ = tokio::time::timeout(Duration::from_millis(400), child.wait()).await;
    drop(tree);
    let _ = child.start_kill();
    let _ = tokio::time::timeout(Duration::from_secs(2), child.wait()).await;
    reader.abort();
    let _ = reader.await;
    let status = outcome?;
    runner.result.status = status.clone();
    emit(&events, NativeEvent::Completed { status }).await?;
    Ok(runner.result)
}

struct Pending {
    id: Value,
    tool_id: String,
    allow: String,
    reject: String,
}
struct Runner {
    stdin: Box<dyn AsyncWrite + Unpin + Send>,
    frames: mpsc::Receiver<Result<Value>>,
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

impl Runner {
    fn new(
        stdin: Box<dyn AsyncWrite + Unpin + Send>,
        frames: mpsc::Receiver<Result<Value>>,
        controls: mpsc::Receiver<NativeControl>,
        events: mpsc::Sender<NativeEvent>,
        req: &NativeRequest,
    ) -> Self {
        Self {
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
        let mut bytes = serde_json::to_vec(&value)?;
        ensure!(
            bytes.len() < MAX_FRAME_BYTES,
            "DeepSeek outgoing ACP frame exceeds limit"
        );
        bytes.push(b'\n');
        tokio::time::timeout(Duration::from_secs(5), async {
            self.stdin.write_all(&bytes).await?;
            self.stdin.flush().await
        })
        .await
        .context("DeepSeek ACP stdin stopped draining")??;
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
                    let frame = frame.context("DeepSeek ACP reader disconnected")??;
                    ensure!(frame["jsonrpc"] == "2.0", "DeepSeek ACP envelope mismatch");
                    if frame.get("method").is_some() { self.handle_server(frame).await?; continue; }
                    ensure!(frame.get("id").and_then(Value::as_u64) == Some(id), "unexpected DeepSeek ACP response identity");
                    // Provider error text may echo secret-bearing requests; retain
                    // only the error code, never the arbitrary server message/data.
                    if let Some(error) = frame.get("error") {
                        bail!("DeepSeek ACP {method} failed (code {}); see the official CLI for account diagnostics", error["code"].as_i64().unwrap_or(-32603));
                    }
                    return frame.get("result").cloned().context("DeepSeek ACP result missing");
                },
                control = self.controls.recv(), if self.controls_open => {
                    match control {
                        Some(NativeControl::Permission{request_id, approve}) => self.resolve(&request_id, approve).await?,
                        Some(NativeControl::Answer{..}) => emit(&self.events, NativeEvent::Status{message:"DeepSeek ACP does not expose question answers".into()}).await?,
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

    async fn initialize(&mut self, req: &NativeRequest) -> Result<()> {
        let init = self.rpc("initialize", json!({"protocolVersion":1,"clientInfo":{"name":"wonderland","version":env!("CARGO_PKG_VERSION")},"clientCapabilities":{}})).await?;
        ensure!(
            init["protocolVersion"] == 1
                && init.pointer("/agentInfo/name").and_then(Value::as_str)
                    == Some("deepseek-harness-acp")
                && init.pointer("/agentInfo/version").and_then(Value::as_str) == Some("0.0.1")
                && init
                    .pointer("/agentCapabilities/sessionCapabilities/close")
                    .is_some(),
            "DeepSeek ACP identity/capability handshake mismatch"
        );
        let session = self
            .rpc("session/new", json!({"cwd":req.cwd,"mcpServers":[]}))
            .await?;
        let session_id = session["sessionId"]
            .as_str()
            .context("DeepSeek ACP session ID missing")?;
        ensure!(
            uuid::Uuid::parse_str(session_id).is_ok(),
            "invalid DeepSeek ACP session ID"
        );
        self.result.session_id = Some(session_id.into());
        self.check_options(&session["configOptions"])?;
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
                json!({"sessionId":session_id,"configId":"model","value":model_value(&req.model)}),
            )
            .await?;
        self.check_options(&selected["configOptions"])?;
        if let Some(effort) = req.reasoning_effort.as_ref() {
            self.enforce_effort = true;
            let selected = self
                .rpc(
                    "session/set_config_option",
                    json!({"sessionId":session_id,"configId":"reasoning_effort","value":effort}),
                )
                .await?;
            self.check_options(&selected["configOptions"])?;
        }
        Ok(())
    }

    async fn run(&mut self, req: &NativeRequest) -> Result<String> {
        emit(
            &self.events,
            NativeEvent::Started {
                app_id: req.app_id.clone(),
                model: req.model.clone(),
                protocol: "deepseek-acp".into(),
            },
        )
        .await?;
        self.initialize(req).await?;
        emit(&self.events, NativeEvent::Status {message:"DeepSeek official ACP: committed message blocks; usage reports context occupancy, not billed tokens. The official file sandbox applies; Windows confinement has upstream documented limitations.".into()}).await?;
        let result = self.rpc("session/prompt", json!({"sessionId":self.result.session_id,"prompt":[{"type":"text","text":req.prompt}]})).await?;
        ensure!(
            self.pending.is_empty(),
            "DeepSeek prompt settled with unresolved permissions"
        );
        match result["stopReason"].as_str() {
            Some("end_turn") => {
                ensure!(
                    self.tools.is_empty(),
                    "DeepSeek prompt ended with active tools"
                );
                Ok("completed".into())
            }
            Some("cancelled") => Ok("cancelled".into()),
            Some("max_tokens" | "max_turn_requests") => {
                bail!("DeepSeek stopped at an execution limit; task remains incomplete")
            }
            _ => bail!("DeepSeek ACP returned an unsupported stop reason"),
        }
    }

    fn check_options(&self, options: &Value) -> Result<()> {
        let options = options
            .as_array()
            .context("DeepSeek ACP config options missing")?;
        let models: Vec<_> = options
            .iter()
            .filter(|item| item["id"] == "model")
            .collect();
        ensure!(
            models.len() == 1 && models[0]["currentValue"] == model_value(&self.result.model),
            "DeepSeek provider/model drift detected"
        );
        if self.enforce_effort {
            let efforts: Vec<_> = options
                .iter()
                .filter(|item| item["id"] == "reasoning_effort")
                .collect();
            ensure!(
                efforts.len() == 1 && efforts[0]["currentValue"].as_str() == self.effort.as_deref(),
                "DeepSeek reasoning effort drift detected"
            );
        }
        Ok(())
    }

    fn check_session(&self, params: &Value) -> Result<()> {
        ensure!(
            self.result.session_id.is_some()
                && params["sessionId"].as_str() == self.result.session_id.as_deref(),
            "DeepSeek ACP session mismatch"
        );
        Ok(())
    }

    async fn handle_server(&mut self, frame: Value) -> Result<()> {
        match frame["method"].as_str() {
            Some("session/update") if frame.get("id").is_none() => {
                self.check_session(&frame["params"])?;
                self.update(&frame["params"]["update"]).await
            }
            Some("session/request_permission") if frame.get("id").is_some() => self.permission(frame).await,
            _ if frame.get("id").is_some() => {
                self.write(json!({"jsonrpc":"2.0","id":frame["id"],"error":{"code":-32601,"message":"Client method is unsupported"}})).await
            }
            _ => bail!("unsupported DeepSeek ACP notification"),
        }
    }

    async fn update(&mut self, update: &Value) -> Result<()> {
        self.streamed_bytes = self
            .streamed_bytes
            .checked_add(serde_json::to_vec(update)?.len())
            .context("DeepSeek output length overflow")?;
        ensure!(
            self.streamed_bytes <= MAX_OUTPUT_BYTES,
            "DeepSeek output exceeds limit"
        );
        match update["sessionUpdate"].as_str() {
            Some("config_option_update") => self.check_options(&update["configOptions"]),
            Some("agent_message_chunk" | "agent_thought_chunk") => {
                ensure!(
                    update["content"]["type"] == "text",
                    "unsupported DeepSeek output content"
                );
                let text = update["content"]["text"]
                    .as_str()
                    .context("DeepSeek text chunk missing")?;
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
                    .context("DeepSeek tool identity missing")?;
                ensure!(
                    !id.is_empty() && id.len() <= 256,
                    "invalid DeepSeek tool identity"
                );
                if update["sessionUpdate"] == "tool_call" {
                    ensure!(
                        update["status"] == "in_progress",
                        "DeepSeek tool start has an invalid status"
                    );
                    ensure!(
                        self.tools.len() < MAX_PENDING && !self.tools.contains_key(id),
                        "duplicate or excessive DeepSeek tool calls"
                    );
                    // Keep only bounded attribution, never full tool result bodies.
                    self.tools.insert(
                        id.into(),
                        json!({"title":update["title"],"rawInput":update["rawInput"]}),
                    );
                } else {
                    ensure!(
                        matches!(update["status"].as_str(), Some("completed" | "failed")),
                        "DeepSeek tool result has an invalid status"
                    );
                    ensure!(
                        self.tools.remove(id).is_some(),
                        "unknown or repeated DeepSeek tool result"
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
                    "invalid DeepSeek context usage"
                );
                let data =
                    json!({"kind":"context_occupancy","used":update["used"],"size":update["size"]});
                self.result.usage = Some(data.clone());
                emit(&self.events, NativeEvent::Usage { data }).await
            }
            _ => bail!("unsupported DeepSeek ACP session update"),
        }
    }

    async fn permission(&mut self, frame: Value) -> Result<()> {
        let params = &frame["params"];
        self.check_session(params)?;
        let id = frame["id"].clone();
        ensure!(
            (id.is_string() && id.as_str().is_some_and(|s| !s.is_empty() && s.len() <= 256))
                || id.is_i64()
                || id.is_u64(),
            "invalid DeepSeek permission request ID"
        );
        ensure!(
            self.seen.len() < MAX_INTERACTIONS && self.seen.insert(id.to_string()),
            "duplicate or excessive DeepSeek permission requests"
        );
        ensure!(
            self.pending.len() < MAX_PENDING,
            "too many pending DeepSeek permissions"
        );
        let options = params["options"]
            .as_array()
            .context("DeepSeek permission options missing")?;
        ensure!(options.len() == 2, "DeepSeek permission options changed");
        let allow = options
            .iter()
            .find(|option| option["kind"] == "allow_once" && option["optionId"] == "allow-once");
        let reject = options
            .iter()
            .find(|option| option["kind"] == "reject_once" && option["optionId"] == "reject-once");
        ensure!(
            allow.is_some() && reject.is_some(),
            "DeepSeek permissions must remain one-shot"
        );
        let tool = params["toolCall"]["toolCallId"]
            .as_str()
            .context("DeepSeek permission tool ID missing")?;
        let attribution = self.tools.get(tool).cloned();
        let request_id = format!("deepseek-{}", uuid::Uuid::new_v4());
        self.pending.insert(
            request_id.clone(),
            Pending {
                id,
                tool_id: tool.into(),
                allow: "allow-once".into(),
                reject: "reject-once".into(),
            },
        );
        // Missing attribution, a disconnected UI or read-only mode can never
        // grant authority. A read-only approval button cannot widen the policy.
        if self.read_only || !self.controls_open || attribution.is_none() {
            return self.resolve(&request_id, false).await;
        }
        let data = attribution.unwrap_or(Value::Null);
        let description = format!(
            "DeepSeek requests one-time permission for {}",
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

    async fn resolve(&mut self, request_id: &str, approve: bool) -> Result<()> {
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

    async fn shutdown(&mut self) -> Result<()> {
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
}

fn model_value(model: &str) -> String {
    json!([PROVIDER, model]).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        pin::Pin,
        sync::{Arc, Mutex},
        task::{Context as TaskContext, Poll},
    };
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
        json!([{"id":"model","currentValue":model_value(model)},{"id":"reasoning_effort","currentValue":effort}])
    }
    fn fixture(
        req: &NativeRequest,
    ) -> (
        Runner,
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
            Runner::new(Box::new(written.clone()), rx, control_rx, event_tx, req),
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

    #[test]
    fn rejects_model_routes_and_generic_effort_aliases() {
        let mut req = request(true);
        assert!(validate_request(&req).is_ok());
        for model in [
            "gpt-5",
            "deepseek-v4-pro --help",
            "deepseek-v4-pro\n",
            "deepseek/x",
        ] {
            req.model = model.into();
            assert!(validate_request(&req).is_err());
        }
        req.model = "deepseek-v4-pro".into();
        req.reasoning_effort = Some("medium".into());
        assert!(validate_request(&req).is_err());
    }

    #[test]
    fn managed_profile_has_no_wider_readonly_preset_or_alternate_route() {
        let patch = managed_patch(&request(true));
        let rows = patch.as_array().unwrap();
        let row = |id: &str| rows.iter().find(|row| row["id"] == id).unwrap();
        assert_eq!(
            row("permission")["config"]["presets"],
            json!({"read-only":{"sandbox":"read-only","approval":"never"}})
        );
        assert_eq!(
            row("llm-deepseek")["config"]["baseURL"],
            "https://api.deepseek.com/anthropic"
        );
        for id in [
            "llm-pi-ai",
            "settings",
            "plugin-manager",
            "subagent",
            "tool-subagent-fork",
            "commands",
            "web-search-deepseek",
            "mcp-resources",
        ] {
            assert_eq!(row(id)["disabled"], true);
        }
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
        assert_eq!(frames[2]["params"]["value"], model_value(&req.model));
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

    /// No prompt, no credentials and no paid provider request. Opt in with the
    /// official exact-release CLI path; the test never inherits an API key.
    #[tokio::test]
    #[ignore = "requires WONDERLAND_DSH_CLI and the official exact npm release"]
    async fn official_keyless_initialize_configure_close() {
        let req = request(true);
        let prepared = prepare().unwrap();
        let home = ManagedHome::create(&req).unwrap();
        verify_banner(&prepared, &home).await.unwrap();
        let mut child = command(&prepared, &home, false)
            .args(["--profile", "acp", "--patch"])
            .arg(home.root.join("managed.patch.json"))
            .spawn()
            .unwrap();
        let tree = crate::process_tree::ProcessTree::attach(child.id().unwrap()).unwrap();
        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let (tx, rx) = mpsc::channel(32);
        let reader = tokio::spawn(async move {
            let mut reader = BufReader::new(stdout);
            loop {
                let mut frame = Vec::new();
                let result = match read_frame(&mut reader, &mut frame).await {
                    Ok(true) => serde_json::from_slice(&frame).context("invalid JSON"),
                    _ => break,
                };
                if tx.send(result).await.is_err() {
                    break;
                }
            }
        });
        let (_control, controls) = mpsc::channel(8);
        let (events, _event_rx) = mpsc::channel(8);
        let mut runner = Runner::new(Box::new(stdin), rx, controls, events, &req);
        let result = tokio::time::timeout(Duration::from_secs(30), runner.initialize(&req)).await;
        let _ = tokio::time::timeout(Duration::from_secs(1), runner.shutdown()).await;
        drop(runner);
        let _ = tokio::time::timeout(Duration::from_secs(2), child.wait()).await;
        drop(tree);
        let _ = child.start_kill();
        let _ = tokio::time::timeout(Duration::from_secs(2), child.wait()).await;
        reader.abort();
        result.unwrap().unwrap();
    }
}
